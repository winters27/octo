//! The download-time cover chain (#51): the right release's cover when a fingerprint named it,
//! the catalog and the aggregator next, and a cover that is not square never passes for one.
//! Also the cover picture and cover.jpg rules and Apple's master lookups the chain stands on.
//! The `AlbumCoverFinder` tests are in `album_cover_finder_tests.rs`.

use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use octo_core::common::Clock;
use octo_core::settings::{AppSettings, MetadataSettings};
use octo_media::cover::cover_files;
use parking_lot::Mutex;
use url::Url;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::cover_art::ICoverArtSource;
use crate::services::http_client_factory;

fn jpeg_with(width: u32, height: u32, center: Option<[u8; 3]>, quality: u8) -> Vec<u8> {
    let mut image = RgbImage::from_pixel(width, height, Rgb([0, 0, 0]));
    if let Some(color) = center {
        let side = width.min(height);
        let (x0, y0) = ((width - side) / 2, (height - side) / 2);
        for y in y0..y0 + side {
            for x in x0..x0 + side {
                image.put_pixel(x, y, Rgb(color));
            }
        }
    }
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(&image)
        .expect("encodes");
    out.into_inner()
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    jpeg_with(width, height, None, 75)
}

fn square() -> Vec<u8> {
    jpeg_with(600, 600, Some([200, 40, 40]), 75)
}

fn sharp() -> Vec<u8> {
    jpeg_with(1200, 1200, Some([40, 40, 200]), 75)
}

fn catalog() -> Vec<u8> {
    jpeg_with(1000, 1000, Some([200, 200, 40]), 75)
}

fn thumbnail() -> Vec<u8> {
    jpeg_with(200, 200, Some([90, 90, 90]), 75)
}

fn video_frame() -> Vec<u8> {
    jpeg_with(1280, 720, Some([40, 200, 40]), 75)
}

// ---- CoverImage ---------------------------------------------------------------------------

#[test]
fn is_usable_sixteen_by_nine_is_not_a_square_cover() {
    assert!(!cover_image::is_usable(Some(&video_frame()), true));
    assert!(cover_image::is_usable(Some(&video_frame()), false));
}

#[test]
fn is_usable_nearly_square_passes() {
    assert!(cover_image::is_usable(Some(&jpeg(600, 590)), true));
}

#[test]
fn is_usable_too_small_or_not_an_image_fails() {
    assert!(!cover_image::is_usable(Some(&jpeg(100, 100)), true));
    assert!(!cover_image::is_usable(Some(b"not an image"), false));
    assert!(!cover_image::is_usable(None, false));
}

/// A YouTube "Topic" frame letterboxes the real cover; its center square is that cover.
#[test]
fn crop_to_square_letterbox_returns_the_center() {
    let cropped = cover_image::crop_to_square(&video_frame()).expect("cropped");

    let image = image::load_from_memory(&cropped).expect("decodes").to_rgb8();
    assert_eq!((image.width(), image.height()), (720, 720));
    let middle = image.get_pixel(360, 360);
    assert!(middle[1] > 150 && middle[0] < 100, "centre pixel was {middle:?}");
}

// ---- DownloadCoverResolver ----------------------------------------------------------------

struct FixedSource {
    bytes: Option<Vec<u8>>,
    calls: AtomicUsize,
}

impl FixedSource {
    fn new(bytes: Option<Vec<u8>>) -> Arc<Self> {
        Arc::new(Self {
            bytes,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ICoverArtSource for FixedSource {
    fn name(&self) -> &str {
        "fixed"
    }

    async fn try_fetch(&self, _: &SoulseekRouting, _: bool) -> Option<Bytes> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.bytes.clone().map(Bytes::from)
    }
}

type Answer = Arc<dyn Fn(&str, &Request) -> ResponseTemplate + Send + Sync>;

/// One stand-in behind every client: answers by URL and records the calls.
#[derive(Clone)]
struct Fake {
    answer: Answer,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Respond for Fake {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let url = request.url.to_string();
        self.calls.lock().push(url.clone());
        (self.answer)(&url, request)
    }
}

/// A server standing in for every host: the Cover Art Archive at `/coverartarchive/`, Apple at
/// `/itunes.apple.com`, and any image the tests name (`/deezer.example/...`, `/is1.example/...`).
struct Hosts {
    server: MockServer,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Hosts {
    async fn start(answer: impl Fn(&str, &Request) -> ResponseTemplate + Send + Sync + 'static) -> Hosts {
        let server = MockServer::builder().start().await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            answer: Arc::new(answer),
            calls: Arc::clone(&calls),
        };
        Mock::given(any()).respond_with(fake).mount(&server).await;
        Hosts { server, calls }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.server.uri())
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }

    fn clear(&self) {
        self.calls.lock().clear();
    }

    fn itunes(&self, cache: Option<std::path::PathBuf>) -> ITunesCoverArtLookup {
        // Apple's pacing is for the real service; these talk to a fake (a test lookup's own pace
        // never waits).
        ITunesCoverArtLookup::with_base_url(cache, &self.url("itunes.apple.com"), Clock::system())
    }

    fn resolver(
        &self,
        aggregated: Arc<FixedSource>,
        settings: Option<MetadataSettings>,
        itunes: bool,
    ) -> DownloadCoverResolver {
        let archive = CoverArtArchiveLookup::with_base_url(
            Url::parse(&self.url("coverartarchive/")).expect("a base address"),
        );
        let settings = SettingsStore::for_tests(AppSettings {
            metadata: settings.unwrap_or_default(),
            ..Default::default()
        });
        DownloadCoverResolver::new(
            Arc::new(archive),
            Arc::new(CoverArtAggregator::new(vec![
                aggregated as Arc<dyn ICoverArtSource>,
            ])),
            http_client_factory::default_client(),
            Arc::new(settings),
            itunes.then(|| Arc::new(self.itunes(None))),
        )
    }
}

fn picture(bytes: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_bytes(bytes.to_vec())
}

fn not_found() -> ResponseTemplate {
    ResponseTemplate::new(404)
}

fn json(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(body)
}

async fn resolve(
    resolver: &DownloadCoverResolver,
    song: &Song,
    embedded: Option<&[u8]>,
) -> Option<CoverChoice> {
    resolver.resolve(song, embedded, &CancellationToken::new()).await
}

fn song(artist: &str, title: &str) -> Song {
    Song {
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    }
}

fn with_catalog(mut song: Song, hosts: &Hosts) -> Song {
    song.cover_art_url_large = Some(hosts.url("deezer.example/cover.jpg"));
    song
}

fn matched_release(hosts: &Hosts) -> Song {
    with_catalog(
        Song {
            album: "Before the Dawn Heals Us".into(),
            music_brainz_release_id: Some("rel-1".into()),
            music_brainz_album_title: Some("Before the Dawn Heals Us".into()),
            ..song("M83", "Lower Your Eyelids")
        },
        hosts,
    )
}

/// The path of an archive call, read from its URL.
fn archive_path(url: &str) -> Option<String> {
    url.split("/coverartarchive/").nth(1).map(str::to_string)
}

#[tokio::test]
async fn resolve_archive_first_when_the_album_is_the_matched_release() {
    let sharp = sharp();
    let hosts = Hosts::start(move |url, _| {
        if url.contains("coverartarchive") {
            picture(&sharp)
        } else {
            not_found()
        }
    })
    .await;

    let choice = resolve(
        &hosts.resolver(FixedSource::new(None), None, false),
        &matched_release(&hosts),
        None,
    )
    .await
    .expect("a cover");

    assert_eq!(choice.source, "Cover Art Archive");
    assert!(
        hosts
            .calls()
            .iter()
            .any(|u| u.contains("release/rel-1/front-1200"))
    );
    // Asked as well: a larger copy of the same art may come after a sharp one.
    assert!(hosts.calls().iter().any(|u| u.contains("deezer.example")));
}

/// A soft cover from a real library: the archive's 500 px scan beat the catalog's 1000.
#[tokio::test]
async fn resolve_a_small_archive_scan_loses_to_the_catalogs_larger_cover() {
    let (small, catalog) = (jpeg(500, 500), catalog());
    let hosts = Hosts::start(move |url, _| {
        if url.contains("front-1200") {
            not_found()
        } else if url.contains("front-500") {
            picture(&small)
        } else if url.contains("deezer.example") {
            picture(&catalog)
        } else {
            not_found()
        }
    })
    .await;

    let choice = resolve(
        &hosts.resolver(FixedSource::new(None), None, false),
        &matched_release(&hosts),
        None,
    )
    .await
    .expect("a cover");

    assert_eq!(choice.source, "the catalog");
    assert_eq!(cover_image::measure(&choice.bytes), Some((1000, 1000)));
}

#[tokio::test]
async fn resolve_the_archives_smaller_thumbnail_is_used_when_it_has_no_large_one() {
    let square = square();
    let hosts = Hosts::start(move |url, _| {
        if url.contains("front-500") {
            picture(&square)
        } else {
            not_found()
        }
    })
    .await;

    let choice = resolve(
        &hosts.resolver(FixedSource::new(None), None, false),
        &matched_release(&hosts),
        None,
    )
    .await
    .expect("a cover");

    assert_eq!(choice.source, "Cover Art Archive");
    let asked: Vec<String> = hosts.calls().iter().filter_map(|u| archive_path(u)).collect();
    assert_eq!(asked, ["release/rel-1/front-1200", "release/rel-1/front-500"]);
}

#[tokio::test]
async fn resolve_a_small_catalog_cover_keeps_looking_and_the_searchs_larger_one_wins() {
    let square = square();
    let hosts = Hosts::start(move |_, _| picture(&square)).await;
    let search = FixedSource::new(Some(catalog()));

    let choice = resolve(
        &hosts.resolver(search, None, false),
        &with_catalog(song("A", "T"), &hosts),
        Some(&thumbnail()),
    )
    .await
    .expect("a cover");

    assert_eq!(choice.source, "a cover search");
    assert!(!choice.keeps_existing);
}

#[tokio::test]
async fn resolve_a_peers_tiny_thumbnail_never_beats_a_real_cover() {
    let square = square();
    let hosts = Hosts::start(move |_, _| picture(&square)).await;

    let choice = resolve(
        &hosts.resolver(FixedSource::new(None), None, false),
        &with_catalog(song("A", "T"), &hosts),
        Some(&thumbnail()),
    )
    .await
    .expect("a cover");

    assert_eq!(choice.source, "the catalog");
    assert!(!choice.keeps_existing);
}

#[tokio::test]
async fn resolve_the_files_own_art_stays_when_it_is_the_largest() {
    let square = square();
    let hosts = Hosts::start(move |_, _| picture(&square)).await;

    let choice = resolve(
        &hosts.resolver(FixedSource::new(None), None, false),
        &with_catalog(song("A", "T"), &hosts),
        Some(&sharp()),
    )
    .await
    .expect("a cover");

    assert!(choice.keeps_existing);
    assert_eq!(choice.source, "the file itself");
}

#[tokio::test]
async fn resolve_a_sharp_catalog_cover_stops_the_search() {
    let catalog = catalog();
    let hosts = Hosts::start(move |_, _| picture(&catalog)).await;
    let search = FixedSource::new(Some(sharp()));

    let choice = resolve(
        &hosts.resolver(Arc::clone(&search), None, false),
        &with_catalog(song("A", "T"), &hosts),
        None,
    )
    .await
    .expect("a cover");

    assert_eq!(choice.source, "the catalog");
    assert_eq!(search.calls(), 0);
}

/// A download tagged with a compilation's name must not get the original album's cover.
#[tokio::test]
async fn resolve_archive_skipped_when_the_album_is_another_release() {
    let square = square();
    let hosts = Hosts::start(move |_, _| picture(&square)).await;
    let song = with_catalog(
        Song {
            album: "Now 42".into(),
            music_brainz_release_id: Some("rel-1".into()),
            music_brainz_album_title: Some("The Real Album".into()),
            ..song("A", "T")
        },
        &hosts,
    );

    let choice = resolve(&hosts.resolver(FixedSource::new(None), None, false), &song, None)
        .await
        .expect("a cover");

    assert_eq!(choice.source, "the catalog");
    assert!(!hosts.calls().iter().any(|u| u.contains("coverartarchive")));
}

#[tokio::test]
async fn resolve_non_square_catalog_cover_falls_through_to_the_search() {
    let frame = video_frame();
    let hosts = Hosts::start(move |_, _| picture(&frame)).await;
    let search = FixedSource::new(Some(square()));
    let mut song = song("A", "T");
    song.cover_art_url_large = Some(hosts.url("i.ytimg.example/maxres.jpg"));

    let choice = resolve(&hosts.resolver(Arc::clone(&search), None, false), &song, None)
        .await
        .expect("a cover");

    assert_eq!(choice.source, "a cover search");
    assert_eq!(search.calls(), 1);
}

#[tokio::test]
async fn resolve_nothing_found_uses_the_center_of_the_video_frame() {
    let hosts = Hosts::start(|_, _| not_found()).await;

    let choice = resolve(
        &hosts.resolver(FixedSource::new(None), None, false),
        &song("A", "T"),
        Some(&video_frame()),
    )
    .await
    .expect("a cover");

    assert!(!choice.keeps_existing);
    assert!(cover_image::is_usable(Some(&choice.bytes), true));
}

#[tokio::test]
async fn resolve_replace_video_covers_off_keeps_the_frame() {
    let hosts = Hosts::start(|_, _| not_found()).await;
    let settings = MetadataSettings {
        replace_video_covers: false,
        ..Default::default()
    };

    let choice = resolve(
        &hosts.resolver(FixedSource::new(None), Some(settings), false),
        &song("A", "T"),
        Some(&video_frame()),
    )
    .await
    .expect("a cover");

    assert!(choice.keeps_existing);
}

#[tokio::test]
async fn resolve_square_cover_already_on_the_file_is_kept_without_a_rewrite() {
    let hosts = Hosts::start(|_, _| not_found()).await;

    let choice = resolve(
        &hosts.resolver(FixedSource::new(None), None, false),
        &song("A", "T"),
        Some(&square()),
    )
    .await
    .expect("a cover");

    assert!(choice.keeps_existing);
    assert_eq!(choice.source, "the file itself");
}

#[tokio::test]
async fn resolve_nothing_anywhere_is_null() {
    let hosts = Hosts::start(|_, _| not_found()).await;

    assert_eq!(
        resolve(
            &hosts.resolver(FixedSource::new(None), None, false),
            &song("A", "T"),
            None
        )
        .await,
        None
    );
}

// ---- Apple's master -----------------------------------------------------------------------

fn itunes_answer(rows: &[(&str, &str, Option<&str>, &str, String)]) -> String {
    let results: Vec<serde_json::Value> = rows
        .iter()
        .map(|(artist, collection, track, explicitness, art)| {
            serde_json::json!({
                // Apple's lookup answers carry it, and the barcode path keeps only collections.
                "wrapperType": "collection",
                "artistName": artist,
                "collectionName": collection,
                "trackName": track,
                "collectionExplicitness": explicitness,
                "artworkUrl100": art,
            })
        })
        .collect();
    serde_json::json!({"resultCount": rows.len(), "results": results}).to_string()
}

/// The server's own address, for answers that must name it before it starts.
type Base = Arc<Mutex<String>>;

fn art(base: &Base, name: &str) -> String {
    format!("{}/is1.example/Music/{name}/100x100bb.jpg", base.lock())
}

async fn hosts_with_base(
    answer: impl Fn(&str, &Request, &Base) -> ResponseTemplate + Send + Sync + 'static,
) -> Hosts {
    let base: Base = Arc::new(Mutex::new(String::new()));
    let shared = Arc::clone(&base);
    let hosts = Hosts::start(move |url, request| answer(url, request, &shared)).await;
    *base.lock() = hosts.server.uri();
    hosts
}

#[tokio::test]
async fn resolve_apple_master_of_the_same_album_comes_first_at_full_size() {
    let master = jpeg(3000, 3000);
    let hosts = hosts_with_base(move |url, _, base| {
        if url.contains("itunes.apple.com/search") {
            json(itunes_answer(&[
                ("Daft Punk", "Homework", None, "notExplicit", art(base, "wrong")),
                ("Daft Punk", "Discovery", None, "notExplicit", art(base, "right")),
            ]))
        } else if url.contains("/right/5000x5000bb") {
            picture(&master)
        } else {
            not_found()
        }
    })
    .await;
    let song = with_catalog(
        Song {
            album: "Discovery".into(),
            ..song("Daft Punk", "One More Time")
        },
        &hosts,
    );

    let choice = resolve(
        &hosts.resolver(FixedSource::new(Some(catalog())), None, true),
        &song,
        None,
    )
    .await
    .expect("a cover");

    assert_eq!(choice.source, "iTunes");
    assert_eq!(cover_image::measure(&choice.bytes), Some((3000, 3000)));
}

/// A barcode the chooser found asks Apple by barcode, never by search: one lookup, then the
/// master from the match it primed.
#[tokio::test]
async fn resolve_a_barcode_on_the_song_primes_apple_and_no_search_is_made() {
    let master = jpeg(3000, 3000);
    let hosts = hosts_with_base(move |url, _, base| {
        if url.contains("itunes.apple.com/lookup") && url.contains("724384960629") {
            json(itunes_answer(&[(
                "Daft Punk",
                "Discovery",
                None,
                "notExplicit",
                art(base, "disc"),
            )]))
        } else if url.contains("/disc/5000x5000bb") {
            picture(&master)
        } else {
            not_found()
        }
    })
    .await;
    let song = with_catalog(
        Song {
            album: "Discovery".into(),
            barcode: Some("0724384960629".into()),
            ..song("Daft Punk", "One More Time")
        },
        &hosts,
    );

    let choice = resolve(
        &hosts.resolver(FixedSource::new(Some(catalog())), None, true),
        &song,
        None,
    )
    .await
    .expect("a cover");

    assert_eq!(choice.source, "iTunes");
    assert_eq!(cover_image::measure(&choice.bytes), Some((3000, 3000)));
    let calls = hosts.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|u| u.contains("itunes.apple.com/lookup"))
            .count(),
        1
    );
    assert!(!calls.iter().any(|u| u.contains("itunes.apple.com/search")));
}

/// Another album by the same artist is a wrong tag, not a soft picture.
#[tokio::test]
async fn resolve_apple_is_skipped_when_no_release_has_the_albums_name() {
    let (catalog, sharp) = (catalog(), sharp());
    let hosts = hosts_with_base(move |url, _, base| {
        if url.contains("itunes.apple.com/search") {
            json(itunes_answer(&[(
                "Daft Punk",
                "Homework",
                None,
                "notExplicit",
                art(base, "wrong"),
            )]))
        } else if url.contains("deezer.example") {
            picture(&catalog)
        } else {
            picture(&sharp)
        }
    })
    .await;
    let song = with_catalog(
        Song {
            album: "Discovery".into(),
            ..song("Daft Punk", "One More Time")
        },
        &hosts,
    );

    let choice = resolve(&hosts.resolver(FixedSource::new(None), None, true), &song, None)
        .await
        .expect("a cover");

    assert_eq!(choice.source, "the catalog");
}

#[tokio::test]
async fn resolve_a_single_is_matched_by_its_song_and_apple_single_suffix() {
    let single = jpeg(1400, 1400);
    let hosts = hosts_with_base(move |url, _, base| {
        if url.contains("itunes.apple.com/search") && url.contains("entity=song") {
            json(itunes_answer(&[
                (
                    "Tame Impala",
                    "Currents",
                    Some("Let It Happen"),
                    "notExplicit",
                    art(base, "album"),
                ),
                (
                    "Tame Impala",
                    "Let It Happen - Single",
                    Some("Let It Happen"),
                    "notExplicit",
                    art(base, "single"),
                ),
            ]))
        } else if url.contains("/single/5000x5000bb") {
            picture(&single)
        } else {
            not_found()
        }
    })
    .await;
    let song = Song {
        album: "Let It Happen".into(),
        ..song("Tame Impala", "Let It Happen")
    };

    let choice = resolve(&hosts.resolver(FixedSource::new(None), None, true), &song, None)
        .await
        .expect("a cover");

    assert_eq!(choice.source, "iTunes");
    assert_eq!(cover_image::measure(&choice.bytes), Some((1400, 1400)));
}

#[tokio::test]
async fn resolve_a_compilation_never_asks_apple() {
    let catalog = catalog();
    let hosts = Hosts::start(move |_, _| picture(&catalog)).await;
    let song = with_catalog(
        Song {
            album: "Now 42".into(),
            is_compilation: true,
            ..song("Various Artists", "T")
        },
        &hosts,
    );

    resolve(&hosts.resolver(FixedSource::new(None), None, true), &song, None).await;

    assert!(!hosts.calls().iter().any(|u| u.contains("itunes")));
}

// ---- Embedding and cover.jpg --------------------------------------------------------------

#[test]
fn fit_within_shrinks_a_master_and_leaves_a_smaller_cover_alone() {
    let master = jpeg(3000, 3000);
    assert_eq!(
        cover_image::measure(&cover_image::fit_within(&master, 1500)),
        Some((1500, 1500))
    );
    let catalog = catalog();
    assert_eq!(cover_image::fit_within(&catalog, 1500), catalog);
}

#[test]
fn mark_as_octo_stays_a_readable_jpeg_and_is_recognized() {
    let square = square();
    assert!(!cover_image::is_octo_cover(&square));

    let marked = cover_image::mark_as_octo(&square);

    assert!(cover_image::is_octo_cover(&marked));
    assert_eq!(cover_image::measure(&marked), Some((600, 600)));
    assert_eq!(cover_image::mark_as_octo(&marked), marked);
    let not_jpeg = b"not a jpeg".to_vec();
    assert_eq!(cover_image::mark_as_octo(&not_jpeg), not_jpeg);
}

#[test]
fn cover_file_is_written_into_a_new_folder_but_not_an_old_one_without_a_cover() {
    let dir = tempfile::tempdir().expect("a temp dir");
    assert!(cover_files::should_write(dir.path(), &square(), true));
    assert!(!cover_files::should_write(dir.path(), &square(), false));
}

#[test]
fn cover_file_octos_own_gives_way_to_a_larger_one_only() {
    let dir = tempfile::tempdir().expect("a temp dir");
    cover_files::write(dir.path(), &square()).expect("written");
    let written = std::fs::read(dir.path().join("cover.jpg")).expect("there");
    assert!(cover_image::is_octo_cover(&written));

    assert!(!cover_files::should_write(dir.path(), &jpeg(500, 500), false));
    assert!(cover_files::should_write(dir.path(), &catalog(), false));

    cover_files::write(dir.path(), &catalog()).expect("written");
    let written = std::fs::read(dir.path().join("cover.jpg")).expect("there");
    assert_eq!(cover_image::measure(&written), Some((1000, 1000)));
    assert_eq!(std::fs::read_dir(dir.path()).expect("lists").count(), 1);
}

#[test]
fn cover_file_the_owners_cover_is_never_replaced() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::write(dir.path().join("cover.jpg"), thumbnail()).expect("written");
    assert!(!cover_files::should_write(dir.path(), &sharp(), true));

    std::fs::remove_file(dir.path().join("cover.jpg")).expect("removed");
    std::fs::write(dir.path().join("folder.jpg"), thumbnail()).expect("written");
    assert!(!cover_files::should_write(dir.path(), &sharp(), true));
}

/// Apple answers about 20 searches a minute: a thousand albums were an hour. One lookup by
/// barcode answers 40, and an album matched there is never searched.
#[tokio::test]
async fn a_barcode_lookup_matches_many_albums_at_once_and_they_are_not_searched_after() {
    let master = jpeg(3000, 3000);
    let hosts = hosts_with_base(move |url, _, base| {
        if url.contains("itunes.apple.com/lookup") {
            json(itunes_answer(&[
                ("Daft Punk", "Discovery", None, "notExplicit", art(base, "disc")),
                ("Tame Impala", "Currents", None, "notExplicit", art(base, "curr")),
                (
                    "Someone Else",
                    "Some Stray Single - Single",
                    None,
                    "notExplicit",
                    art(base, "stray"),
                ),
            ]))
        } else if url.contains("/disc/5000x5000bb") {
            picture(&master)
        } else {
            not_found()
        }
    })
    .await;
    let itunes = hosts.itunes(None);

    let albums = [
        ("Daft Punk", "Discovery", "724384960650"),
        ("Tame Impala", "Currents", "602547306807"),
        ("Lorde", "Melodrama", "602557000000"),
    ]
    .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()));
    let matched = itunes.prime_by_barcode(&albums, None).await;
    let cover = itunes
        .try_fetch_album_master(Some("Daft Punk"), Some("Discovery"), Some("One More Time"))
        .await
        .expect("the master");

    assert_eq!(matched, 2);
    let calls = hosts.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|u| u.contains("itunes.apple.com/lookup")
                && u.contains("724384960650,602547306807,602557000000"))
            .count(),
        1
    );
    assert!(!calls.iter().any(|u| u.contains("itunes.apple.com/search")));
    assert_eq!(cover_image::measure(&cover), Some((3000, 3000)));
}

fn pattern(side: u32, flipped: bool, quality: u8) -> Vec<u8> {
    let mut image = RgbImage::new(side, side);
    for y in 0..side {
        for x in 0..side {
            let v = if flipped {
                (255 * (side - 1 - x) / side) as u8
            } else {
                (255 * (((x / (side / 8)) + (y / (side / 8))) % 2)) as u8
            };
            image.put_pixel(x, y, Rgb([v, v, v]));
        }
    }
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(&image)
        .expect("encodes");
    out.into_inner()
}

#[test]
fn the_same_artwork_looks_alike_at_any_size_and_another_does_not() {
    let sharp = cover_image::looks_hash(Some(&pattern(3000, false, 90))).expect("a hash");
    let soft = cover_image::looks_hash(Some(&pattern(200, false, 40))).expect("a hash");
    let other = cover_image::looks_hash(Some(&pattern(3000, true, 90))).expect("a hash");

    assert!(cover_image::look_alike(sharp, soft));
    assert!(!cover_image::look_alike(sharp, other));
    assert_eq!(cover_image::looks_hash(Some(b"not an image")), None);
}

#[tokio::test]
async fn a_probe_learns_the_masters_size_and_takes_apples_small_copy_without_the_master() {
    let ranged = Arc::new(Mutex::new(false));
    let seen = Arc::clone(&ranged);
    let (master, tile) = (jpeg(3000, 3000), jpeg(320, 320));
    let hosts = hosts_with_base(move |url, request, base| {
        if url.contains("itunes.apple.com/search") {
            json(itunes_answer(&[(
                "Daft Punk",
                "Discovery",
                None,
                "notExplicit",
                art(base, "disc"),
            )]))
        } else if url.contains("/5000x5000bb") {
            if request.headers.contains_key("range") {
                *seen.lock() = true;
            }
            picture(&master)
        } else if url.contains("/320x320bb") {
            picture(&tile)
        } else {
            not_found()
        }
    })
    .await;

    let (side, thumb) = hosts
        .itunes(None)
        .try_probe_album_master(Some("Daft Punk"), Some("Discovery"), Some("One More Time"))
        .await
        .expect("a probe");

    assert_eq!(side, 3000);
    assert_eq!(cover_image::measure(&thumb), Some((320, 320)));
    assert!(*ranged.lock());
}

#[tokio::test]
async fn apple_matches_are_remembered_on_disk_across_a_restart() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let cache = dir.path().join("itunes.json");
    let master = jpeg(3000, 3000);
    let hosts = hosts_with_base(move |url, _, base| {
        if url.contains("itunes.apple.com/search") {
            json(itunes_answer(&[(
                "Daft Punk",
                "Discovery",
                None,
                "notExplicit",
                art(base, "disc"),
            )]))
        } else if url.contains("/5000x5000bb") {
            picture(&master)
        } else {
            not_found()
        }
    })
    .await;

    {
        let first = hosts.itunes(Some(cache.clone()));
        assert!(
            first
                .try_fetch_album_master(Some("Daft Punk"), Some("Discovery"), None)
                .await
                .is_some()
        );
        // The last flush wrote the matches.
        first.flush_cache();
    }
    hosts.clear();

    let second = hosts.itunes(Some(cache));
    assert!(
        second
            .try_fetch_album_master(Some("Daft Punk"), Some("Discovery"), None)
            .await
            .is_some()
    );
    assert!(
        !hosts
            .calls()
            .iter()
            .any(|u| u.contains("itunes.apple.com/search"))
    );
}

// ---- the cover keep, in the resolver ------------------------------------------------------
//
// The song's own cover is judged against the release's art, kept when it is the same art and
// nothing much larger turned up, and the timeline is told what was kept or replaced and what
// else was looked at.

mod keep {
    use super::*;
    use crate::services::cover_art::cover_keep::tests::{art, blown_up_art};

    fn catalog_art() -> Vec<u8> {
        art(1, 1000)
    }

    fn apple(base: &Base, album: &str, song: bool) -> String {
        let track = song.then_some("Be Her");
        let results = serde_json::json!([{
            "wrapperType": if song { "track" } else { "collection" },
            "artistName": "Ella Langley",
            "collectionName": album,
            "trackName": track,
            "collectionExplicitness": "notExplicit",
            "artworkUrl100": format!("{}/is1.example/Music/m/100x100bb.jpg", base.lock()),
        }]);
        serde_json::json!({"resultCount": 1, "results": results}).to_string()
    }

    /// The catalog answers with `catalog`, the archive with `archive`, Apple with `master` for
    /// the album asked (or, when `apple_album` is another album, lists the song only there).
    async fn sources(
        catalog: Option<Vec<u8>>,
        master: Option<Vec<u8>>,
        archive: Option<Vec<u8>>,
        apple_album: &'static str,
    ) -> Hosts {
        hosts_with_base(move |url, _, base| {
            if url.contains("deezer.example") {
                catalog.as_deref().map_or_else(not_found, picture)
            } else if url.contains("coverartarchive") {
                archive.as_deref().map_or_else(not_found, picture)
            } else if url.contains("itunes.apple.com/search") {
                match &master {
                    None => json(r#"{"resultCount":0,"results":[]}"#.to_string()),
                    Some(_) => json(apple(base, apple_album, url.contains("entity=song"))),
                }
            } else if url.contains("is1.example") {
                master.as_deref().map_or_else(not_found, picture)
            } else {
                not_found()
            }
        })
        .await
    }

    fn resolver(hosts: &Hosts, search: Option<Vec<u8>>) -> DownloadCoverResolver {
        hosts.resolver(FixedSource::new(search), None, true)
    }

    fn dandelion(hosts: &Hosts, catalog: bool) -> Song {
        let song = Song {
            album: "Dandelion".into(),
            ..song("Ella Langley", "Choosin' Texas")
        };
        if catalog { with_catalog(song, hosts) } else { song }
    }

    /// The live case (canary, "Be Her"): Apple lists the song only on the album "Dandelion", the
    /// archive has the single at 1200 px and stopped the chain, and the file's own 1400 px cover
    /// of the same art was replaced. Now it stays, and the timeline names everything that was
    /// looked at.
    fn be_her(hosts: &Hosts) -> Song {
        with_catalog(
            Song {
                album: "Be Her".into(),
                music_brainz_release_id: Some("rel-be-her".into()),
                music_brainz_album_title: Some("Be Her".into()),
                ..song("Ella Langley", "Be Her")
            },
            hosts,
        )
    }

    async fn choose(resolver: &DownloadCoverResolver, song: &Song, on_hand: CoversOnHand<'_>) -> CoverChoice {
        resolver
            .choose(song, &on_hand, &CancellationToken::new())
            .await
            .expect("a cover")
    }

    fn note(choice: &CoverChoice) -> &str {
        choice.note.as_deref().unwrap_or_default()
    }

    #[tokio::test]
    async fn choose_be_her_its_own1400_px_cover_stays_and_the_timeline_says_what_else_was_seen() {
        let hosts = sources(
            Some(catalog_art()),
            Some(art(9, 3000)),
            Some(art(1, 1200)),
            "Dandelion",
        )
        .await;
        let own = art(1, 1400);

        let choice = choose(
            &resolver(&hosts, None),
            &be_her(&hosts),
            CoversOnHand::new(Some(&own)),
        )
        .await;

        assert!(choice.keeps_existing);
        assert_eq!(choice.headline.as_deref(), Some("Kept the cover it came with"));
        let pattern = regex::Regex::new(
            r"^1400 x 1400 px, \d+ KB, the same art\. Also looked at: iTunes had none, Cover Art Archive 1200 px, the catalog 1000 px\.$",
        )
        .expect("a pattern");
        assert!(pattern.is_match(note(&choice)), "{}", note(&choice));
        assert!(hosts.calls().iter().any(|u| u.contains("coverartarchive")));
    }

    /// A sharper picture of another album never stands in for the release's art.
    #[tokio::test]
    async fn choose_be_her_a_small_own_cover_goes_to_the_largest_copy_of_the_same_art_never_to_another_picture()
     {
        let hosts = sources(
            Some(catalog_art()),
            Some(art(9, 3000)),
            Some(art(1, 1200)),
            "Be Her",
        )
        .await;
        let own = art(1, 300);

        let choice = choose(
            &resolver(&hosts, None),
            &be_her(&hosts),
            CoversOnHand::new(Some(&own)),
        )
        .await;

        assert_eq!(choice.source, "Cover Art Archive");
        assert_eq!(
            choice.headline.as_deref(),
            Some("Replaced its 300 px cover with a 1200 x 1200 px one from Cover Art Archive")
        );
        assert!(
            note(&choice).contains("iTunes 3000 px of a different picture"),
            "{}",
            note(&choice)
        );
    }

    #[tokio::test]
    async fn choose_the_same_art_at1400_is_kept_over_smaller_copies() {
        let hosts = sources(Some(catalog_art()), Some(art(1, 1200)), None, "Dandelion").await;
        let own = art(1, 1400);

        let choice = choose(
            &resolver(&hosts, None),
            &dandelion(&hosts, true),
            CoversOnHand::new(Some(&own)),
        )
        .await;

        assert!(choice.keeps_existing);
        assert!(note(&choice).starts_with("1400 x 1400 px, "), "{}", note(&choice));
        assert!(
            note(&choice).contains("the same art. Also looked at: iTunes 1200 px, the catalog 1000 px."),
            "{}",
            note(&choice)
        );
    }

    /// A much larger copy of the same art is still worth having: Apple's master over a 1400 px
    /// scan.
    #[tokio::test]
    async fn choose_a_much_larger_copy_of_the_same_art_replaces_its_own() {
        let hosts = sources(Some(catalog_art()), Some(art(1, 3000)), None, "Dandelion").await;
        let own = art(1, 1400);

        let choice = choose(
            &resolver(&hosts, None),
            &dandelion(&hosts, true),
            CoversOnHand::new(Some(&own)),
        )
        .await;

        assert_eq!(choice.source, "iTunes");
        assert_eq!(
            choice.headline.as_deref(),
            Some("Replaced its 1400 px cover with a 3000 x 3000 px one from iTunes")
        );
        assert!(
            note(&choice).starts_with("iTunes has the same art much larger."),
            "{}",
            note(&choice)
        );
    }

    #[tokio::test]
    async fn choose_a_small_cover_is_replaced_by_apple_and_says_so() {
        let hosts = sources(Some(catalog_art()), Some(art(1, 3000)), None, "Dandelion").await;
        let own = art(1, 300);

        let choice = choose(
            &resolver(&hosts, None),
            &dandelion(&hosts, true),
            CoversOnHand::new(Some(&own)),
        )
        .await;

        assert!(!choice.keeps_existing);
        assert_eq!(
            choice.headline.as_deref(),
            Some("Replaced its 300 px cover with a 3000 x 3000 px one from iTunes")
        );
    }

    #[tokio::test]
    async fn choose_a_somewhat_smaller_cover_of_the_same_art_stays_when_nothing_is_much_larger() {
        let hosts = sources(Some(catalog_art()), None, None, "Dandelion").await;
        let own = art(1, 900);

        let choice = choose(
            &resolver(&hosts, None),
            &dandelion(&hosts, true),
            CoversOnHand::new(Some(&own)),
        )
        .await;

        assert!(choice.keeps_existing);
        assert!(
            note(&choice).contains(
                "the same art; nothing found was much larger. Also looked at: iTunes had none, the catalog 1000 px."
            ),
            "{}",
            note(&choice)
        );
    }

    #[tokio::test]
    async fn choose_a_different_picture_is_replaced_even_when_larger() {
        let hosts = sources(Some(catalog_art()), None, None, "Dandelion").await;
        let own = art(2, 1400);

        let choice = choose(
            &resolver(&hosts, None),
            &dandelion(&hosts, true),
            CoversOnHand::new(Some(&own)),
        )
        .await;

        assert!(!choice.keeps_existing);
        assert_eq!(choice.source, "the catalog");
        assert_eq!(
            choice.headline.as_deref(),
            Some("Replaced its 1400 px cover with a 1000 x 1000 px one from the catalog")
        );
        assert!(
            note(&choice).starts_with("It was a different picture from the album's cover."),
            "{}",
            note(&choice)
        );
    }

    #[tokio::test]
    async fn choose_a_blown_up_cover_is_replaced() {
        let hosts = sources(Some(catalog_art()), Some(art(1, 3000)), None, "Dandelion").await;
        let own = blown_up_art(1, 200, 1400);

        let choice = choose(
            &resolver(&hosts, None),
            &dandelion(&hosts, true),
            CoversOnHand::new(Some(&own)),
        )
        .await;

        assert_eq!(choice.source, "iTunes");
        assert!(
            note(&choice).starts_with("It was a smaller picture blown up."),
            "{}",
            note(&choice)
        );
    }

    /// With no strict source, a cover search by name is weak evidence: it may add a larger copy
    /// of the same art, never swap the picture.
    #[tokio::test]
    async fn choose_without_a_strict_source_a_cover_search_may_not_swap_the_picture() {
        let hosts = sources(None, None, None, "Dandelion").await;
        let song = dandelion(&hosts, false);
        let (own_900, own_1000, own_600) = (art(1, 900), art(1, 1000), art(1, 600));

        let other = choose(
            &resolver(&hosts, Some(art(3, 1200))),
            &song,
            CoversOnHand::new(Some(&own_900)),
        )
        .await;
        let larger = choose(
            &resolver(&hosts, Some(art(1, 1200))),
            &song,
            CoversOnHand::new(Some(&own_1000)),
        )
        .await;

        assert!(other.keeps_existing);
        assert!(
            note(&other).contains("a cover search 1200 px of a different picture"),
            "{}",
            note(&other)
        );
        assert!(larger.keeps_existing);
        let much_larger = choose(
            &resolver(&hosts, Some(art(1, 1200))),
            &song,
            CoversOnHand::new(Some(&own_600)),
        )
        .await;
        assert_eq!(much_larger.source, "a cover search");
    }

    #[tokio::test]
    async fn choose_nothing_found_anywhere_keeps_what_it_has() {
        let hosts = sources(None, None, None, "Dandelion").await;
        let own = art(2, 700);

        let choice = choose(
            &resolver(&hosts, None),
            &dandelion(&hosts, false),
            CoversOnHand::new(Some(&own)),
        )
        .await;

        assert!(choice.keeps_existing);
        assert!(
            note(&choice).contains("nothing else was found"),
            "{}",
            note(&choice)
        );
    }

    /// The tagging round's rule stands: filed under another album, its own art never outranks
    /// the chain, and stays only when the chain finds nothing.
    #[tokio::test]
    async fn choose_art_of_the_album_it_was_filed_away_from_loses_to_the_chain_but_stays_as_a_last_resort() {
        let own = art(1, 1400);
        let on_hand = CoversOnHand {
            arrived: Some(&own),
            arrived_from_other_album: true,
            library_copy: None,
        };
        let with = sources(Some(catalog_art()), None, None, "Dandelion").await;
        let without = sources(None, None, None, "Dandelion").await;

        let replaced = choose(&resolver(&with, None), &dandelion(&with, true), on_hand).await;
        let alone = choose(&resolver(&without, None), &dandelion(&without, false), on_hand).await;

        assert_eq!(replaced.source, "the catalog");
        assert!(
            note(&replaced).contains("filed under before"),
            "{}",
            note(&replaced)
        );
        assert!(alone.keeps_existing);
    }

    /// Better quality and the other replacements: the old copy's cover is what the album shows,
    /// so a good one goes onto the new file instead of the peer's.
    #[tokio::test]
    async fn choose_a_replacement_keeps_the_old_copys_good_cover() {
        let hosts = sources(Some(catalog_art()), Some(art(1, 3000)), None, "Dandelion").await;
        let (own, old_cover) = (art(1, 1400), art(4, 1500));
        let on_hand = CoversOnHand {
            arrived: Some(&own),
            arrived_from_other_album: false,
            library_copy: Some(&old_cover),
        };

        let choice = choose(&resolver(&hosts, None), &dandelion(&hosts, true), on_hand).await;

        assert!(!choice.keeps_existing);
        assert_eq!(choice.bytes, old_cover);
        assert_eq!(choice.headline.as_deref(), Some("Kept your old copy's cover"));
        assert!(!hosts.calls().iter().any(|u| u.contains("itunes")));
    }

    #[tokio::test]
    async fn choose_a_replacement_whose_old_cover_is_small_falls_back_to_the_new_files_good_one() {
        let hosts = sources(Some(catalog_art()), None, None, "Dandelion").await;
        let (own, old_cover) = (art(1, 1400), art(1, 250));
        let on_hand = CoversOnHand {
            arrived: Some(&own),
            arrived_from_other_album: false,
            library_copy: Some(&old_cover),
        };

        let choice = choose(&resolver(&hosts, None), &dandelion(&hosts, true), on_hand).await;

        assert!(choice.keeps_existing);
        assert_eq!(choice.headline.as_deref(), Some("Kept the cover it came with"));
    }

    #[tokio::test]
    async fn choose_no_cover_of_its_own_says_so() {
        let hosts = sources(Some(catalog_art()), Some(art(1, 3000)), None, "Dandelion").await;

        let choice = choose(
            &resolver(&hosts, None),
            &dandelion(&hosts, true),
            CoversOnHand::new(None),
        )
        .await;

        assert_eq!(choice.source, "iTunes");
        assert_eq!(
            choice.headline.as_deref(),
            Some("Cover from iTunes, 3000 x 3000 px")
        );
        assert_eq!(
            note(&choice),
            "It came with no cover of its own. Also looked at: iTunes 3000 px, the catalog 1000 px."
        );
    }
}
