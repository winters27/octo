//! A download tagged end to end, from the landed file to the tags on disk and the report in the
//! fetched-songs log. The catalog and the music database are the real clients against one mock
//! server, the loudness meter is a fake, the fingerprint service's answer is a parsed lookup, and
//! the files are real files in a temp folder.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octo_core::fingerprint::acoust_id_client::parse_lookup;
use octo_core::fingerprint::{AcoustIdLookup, VerificationResult, VerificationVerdict};
use octo_core::models::domain::{Album, Song};
use octo_core::models::download::DownloadHistoryEntry;
use octo_core::settings::{
    AppSettings, DownloadSource, FolderStructure, MetadataSettings, SoulseekSettings, SubsonicSettings,
};
use octo_core::tagging::FieldDecision;
use octo_media::audio::{ILoudnessMeter, Loudness, ReplayGainTags};
use octo_media::tags::{KeptIdentityTags, TagField, TagFile, tag_writer_extras};
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::matchers::{any, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::placement_tests::{vorbis, vorbis_all};
use super::test_support::{Harness, build, catalog};
use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::fingerprint::MusicBrainzClient;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME;
use crate::services::test_support::{Routes, flac, mp3};

// ---- the fakes ----------------------------------------------------------------------------

struct FakeMeter {
    result: Option<Loudness>,
    called: AtomicBool,
    file_still_there_when_done: AtomicBool,
}

impl FakeMeter {
    fn new(result: Option<Loudness>) -> Arc<Self> {
        Arc::new(Self {
            result,
            called: AtomicBool::new(false),
            file_still_there_when_done: AtomicBool::new(false),
        })
    }

    fn called(&self) -> bool {
        self.called.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ILoudnessMeter for FakeMeter {
    async fn measure(&self, path: &Path, _: i32, _: &CancellationToken) -> Option<Loudness> {
        self.called.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.file_still_there_when_done
            .store(path.is_file(), Ordering::SeqCst);
        self.result
    }
}

/// The `special` answer: the music database down, the catalog over its quota.
#[derive(Clone)]
struct DownAndThrottled;

impl Respond for DownAndThrottled {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if request.url.path().starts_with("/ws/2/") {
            ResponseTemplate::new(503)
        } else {
            ResponseTemplate::new(200).set_body_string(
                r#"{"error":{"type":"Exception","message":"Quota limit exceeded","code":4}}"#,
            )
        }
    }
}

struct Tagging {
    harness: Harness,
    meter: Arc<FakeMeter>,
    root: tempfile::TempDir,
    _server: MockServer,
}

impl Tagging {
    fn root(&self) -> &Path {
        self.root.path()
    }

    async fn download(&self, id: &str) -> anyhow::Result<String> {
        self.harness
            .service
            .download_song("test", id, &CancellationToken::new())
            .await
    }

    async fn download_in_walk(&self, id: &str, context: &Arc<AlbumTagContext<Loudness>>) -> String {
        self.harness
            .service
            .download_song_internal(
                "test",
                id,
                DownloadOptions {
                    trigger_album_download: false,
                    force_permanent: true,
                    suppress_notify: true,
                    album_context: Some(Arc::clone(context)),
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .await
            .expect("downloaded")
    }

    fn last_entry(&self) -> DownloadHistoryEntry {
        let entries = self.harness.history.get_recent(1);
        assert_eq!(entries.len(), 1);
        entries.into_iter().next().expect("one entry")
    }
}

fn landed_flac(root: &Path, folder: &str, tag: impl FnOnce(&mut TagFile)) -> String {
    let dir = root.join(folder);
    std::fs::create_dir_all(&dir).expect("made");
    let path = dir.join(format!("{}.flac", uuid::Uuid::new_v4().simple()));
    std::fs::write(&path, flac()).expect("written");
    let mut file = TagFile::open(&path).expect("opens");
    file.set_title(Some("Teardrop"));
    file.set_performers(&["Massive Attack".to_string()]);
    tag(&mut file);
    file.save().expect("saved");
    path.to_string_lossy().into_owned()
}

struct Options {
    metadata: MetadataSettings,
    soulseek: SoulseekSettings,
    loudness: Option<Loudness>,
    layout: FolderStructure,
    /// The albums template, for the Custom layout.
    template: Option<&'static str>,
    /// The downloads log the download reports to, when a test reads it.
    tracker: Option<Arc<AcquisitionTracker>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            metadata: MetadataSettings::default(),
            soulseek: SoulseekSettings::default(),
            loudness: Some(Loudness::new(-11.5, 6.3, -0.3)),
            layout: FolderStructure::Flat,
            template: None,
            tracker: None,
        }
    }
}

async fn build_with(songs: Vec<(&str, Song)>, server: MockServer, options: Options) -> Tagging {
    build_with_catalog(catalog(songs), server, options).await
}

async fn build_with_catalog(metadata: Arc<FakeMetadata>, server: MockServer, options: Options) -> Tagging {
    let root = tempfile::tempdir().expect("a temp dir");
    let meter = FakeMeter::new(options.loudness);
    let settings = AppSettings {
        subsonic: SubsonicSettings {
            folder_structure: options.layout,
            folder_template: options
                .template
                .unwrap_or(octo_core::common::layout::ORGANIZED)
                .to_string(),
            download_mode: octo_core::settings::DownloadMode::Track,
            storage_mode: octo_core::settings::StorageMode::Permanent,
            ..Default::default()
        },
        metadata: options.metadata,
        soulseek: options.soulseek,
        ..Default::default()
    };
    // The catalog reads its settings live; it shares the service's store below.
    let store = Arc::new(octo_core::settings::SettingsStore::for_tests(settings.clone()));
    let deezer = Arc::new(DeezerMetadataService::with_base_url(
        Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
        store,
        server.uri(),
    ));
    let music_brainz = Arc::new(MusicBrainzClient::with_base_url(
        Url::parse(&format!("{}/ws/2/", server.uri())).expect("a base address"),
    ));
    let services = DownloadServices {
        deezer: Some(deezer),
        music_brainz: Some(music_brainz),
        loudness_meter: Some(Arc::clone(&meter) as Arc<dyn ILoudnessMeter>),
        tracker: options.tracker,
        ..Default::default()
    };
    let harness = build(root.path(), settings, None, Some(metadata), services);
    Tagging {
        harness,
        meter,
        root,
        _server: server,
    }
}

async fn serve(routes: Vec<(&str, String)>) -> MockServer {
    Routes::new(
        routes
            .iter()
            .map(|(needle, body)| (*needle, vec![body.as_str()]))
            .collect(),
        crate::services::test_support::Matching::IgnoreCase,
    )
    .serve()
    .await
}

// ---- the fixtures -------------------------------------------------------------------------

const TEARDROP_LOOKUP: &str = r#"
{"status": "ok", "results": [{"id": "acoustid-1", "score": 0.97, "recordings": [{
  "id": "rec-teardrop", "title": "Teardrop", "duration": 330.2, "sources": 40, "isrcs": ["GBAAA9800001"],
  "artists": [{"id": "a-ma", "name": "Massive Attack", "joinphrase": " feat. "}, {"id": "a-ef", "name": "Elizabeth Fraser"}],
  "releasegroups": [
    {"id": "g-collected", "title": "Collected", "type": "Album", "secondarytypes": ["Compilation"],
     "artists": [{"id": "a-ma", "name": "Massive Attack"}],
     "releases": [{"id": "r-col", "date": {"year": 2006, "month": 3, "day": 27}, "country": "GB",
       "mediums": [{"position": 1, "track_count": 14, "tracks": [{"id": "t-col", "position": 4}]}]}]},
    {"id": "g-mezzanine", "title": "Mezzanine", "type": "Album",
     "artists": [{"id": "a-ma", "name": "Massive Attack"}],
     "releases": [
       {"id": "r-mezz-2019", "date": {"year": 2019, "month": 8, "day": 23}, "country": "XE",
        "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m19", "position": 3}]}]},
       {"id": "r-mezz", "date": {"year": 1998, "month": 4, "day": 20}, "country": "GB",
        "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m", "position": 3}]}]}]}
  ]}]}]}
"#;

const MEZZANINE_DETAILS: &str = r#"
{"id": "r-mezz", "title": "Mezzanine", "status": "Official", "date": "1998-04-20", "country": "GB", "barcode": "724384559922",
 "label-info": [{"catalog-number": "CDV 2851", "label": {"id": "l-virgin", "name": "Virgin"}}],
 "release-group": {"id": "g-mezzanine", "title": "Mezzanine", "primary-type": "Album", "secondary-types": [], "first-release-date": "1998-04-20"},
 "artist-credit": [{"name": "Massive Attack", "artist": {"id": "a-ma", "name": "Massive Attack"}}],
 "media": [{"position": 1, "track-count": 11, "tracks": [
   {"id": "t-m", "position": 3, "number": "3", "title": "Teardrop", "recording": {"id": "rec-teardrop", "title": "Teardrop", "isrcs": ["GBAAA9800001"]}}]}],
 "genres": [{"name": "trip hop", "count": 9}, {"name": "electronic", "count": 3}, {"name": "downtempo", "count": 1}]}
"#;

fn mezzanine_routes(catalog_album: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "/album/1",
            r#"{"id":1,"record_type":"album","upc":"724384559922","label":"Virgin","release_date":"1998-04-20","nb_tracks":11,"artist":{"name":"Massive Attack"},"genres":{"data":[{"name":"Electro"}]}}"#
                .to_string(),
        ),
        (
            "/track/11",
            r#"{"id":11,"track_position":3,"disk_number":1,"isrc":"GBAAA9800001","gain":-9.8,"contributors":[{"name":"Massive Attack","role":"Main"},{"name":"Elizabeth Fraser","role":"Featured"}]}"#
                .to_string(),
        ),
        (
            "/search?q=",
            r#"{"data":[{"id":11,"title":"Teardrop","duration":330,"isrc":"GBAAA9800001","album":{"id":1,"title":"CATALOG_ALBUM","cover_xl":"https://cdn.example/mezz.jpg"},"artist":{"name":"Massive Attack"}}]}"#
                .replace("CATALOG_ALBUM", catalog_album),
        ),
        ("release/r-mezz?", MEZZANINE_DETAILS.to_string()),
    ]
}

fn lookup(json: &str) -> AcoustIdLookup {
    parse_lookup(&serde_json::from_str(json).expect("json")).expect("a lookup")
}

fn confirmed_teardrop() -> VerificationResult {
    confirmed_by(TEARDROP_LOOKUP)
}

/// A check that confirmed the first recording of this fingerprint answer.
fn confirmed_by(json: &str) -> VerificationResult {
    let lookup = lookup(json);
    let recording = lookup.results[0].recordings[0].clone();
    VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        score: 0.97,
        recording_id: Some(recording.recording_id.clone()),
        matched_title: Some(recording.title.clone()),
        matched_artist: Some(recording.artist_credit()),
        matched_album: recording.album_title.clone(),
        matched_year: recording.year,
        r#match: Some(recording),
        lookup: Some(lookup),
        acoust_id: Some("acoustid-1".into()),
        ..Default::default()
    }
}

fn confirm(song: &mut Song, verdict: VerificationResult) {
    verdict.apply_tags_to(song);
    song.verification = Some(Box::new(verdict));
}

fn teardrop() -> Song {
    Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        ..Default::default()
    }
}

// ---- A: the FLAC ends with the full tag set -----------------------------------------------

#[tokio::test]
async fn lone_star_flac_ends_with_the_full_release_set_replay_gain_and_a_report() {
    let tagging = build_with(
        vec![("1", teardrop())],
        serve(mezzanine_routes("Mezzanine")).await,
        Options::default(),
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        // `file.Tag.Comment`: on a FLAC, the Vorbis COMMENT field.
        const COMMENT: TagField = TagField {
            id3_frame: None,
            id3_description: Some("COMMENT"),
            vorbis: "COMMENT",
            mp4: "COMMENT",
        };
        landed_flac(&root, "peer share", |file| {
            tag_writer_extras::set_text(file, COMMENT, Some("a peer's comment"));
        })
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Mezzanine"));
    assert_eq!(vorbis(&path, "DATE").as_deref(), Some("1998"));
    assert_eq!(vorbis(&path, "ORIGINALDATE").as_deref(), Some("1998-04-20"));
    assert_eq!(vorbis(&path, "LABEL").as_deref(), Some("Virgin"));
    assert_eq!(vorbis(&path, "CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(vorbis(&path, "BARCODE").as_deref(), Some("724384559922"));
    assert_eq!(vorbis(&path, "ISRC").as_deref(), Some("GBAAA9800001"));
    assert_eq!(vorbis(&path, "RELEASETYPE").as_deref(), Some("album"));
    assert_eq!(vorbis(&path, "RELEASESTATUS").as_deref(), Some("official"));
    assert_eq!(vorbis(&path, "RELEASECOUNTRY").as_deref(), Some("GB"));
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_TRACKID").as_deref(),
        Some("rec-teardrop")
    );
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_RELEASETRACKID").as_deref(),
        Some("t-m")
    );
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_RELEASEGROUPID").as_deref(),
        Some("g-mezzanine")
    );
    assert_eq!(vorbis_all(&path, "MUSICBRAINZ_ARTISTID"), ["a-ma", "a-ef"]);
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_ALBUMARTISTID").as_deref(),
        Some("a-ma")
    );
    assert_eq!(vorbis(&path, "ACOUSTID_ID").as_deref(), Some("acoustid-1"));
    assert_eq!(
        vorbis(&path, "REPLAYGAIN_TRACK_GAIN").as_deref(),
        Some("-6.50 dB")
    );
    assert_eq!(
        vorbis(&path, "REPLAYGAIN_TRACK_PEAK").as_deref(),
        Some("0.966051")
    );
    assert_eq!(
        vorbis(&path, "TRACKNUMBER").and_then(|n| n.parse::<i32>().ok()),
        Some(3)
    );
    // The code has its own field now; the peer's comment is left exactly as it was.
    assert_eq!(vorbis(&path, "COMMENT").as_deref(), Some("a peer's comment"));

    let report = tagging.last_entry().tagging.expect("a report");
    assert_eq!(report.confidence, "Strong");
    assert_eq!(report.release_id.as_deref(), Some("r-mezz"));
    assert!(report.details_prefetch_hit);
    assert_eq!(report.fields["album"].source.as_deref(), Some("Fingerprint"));
    assert_eq!(report.integrated_lufs, Some(-11.5));
    assert!(report.stage_seconds.contains_key("loudness"));
    assert!(report.stage_seconds.contains_key("total"));
    // The measurement finished before the file moved: nothing reads a file while it moves.
    assert!(tagging.meter.called());
    assert!(tagging.meter.file_still_there_when_done.load(Ordering::SeqCst));
}

// ---- an upload's tags are not evidence ----------------------------------------------------

#[tokio::test]
async fn staged_upload_ignores_its_own_tags_as_evidence() {
    let routes = vec![
        (
            "/album/2",
            r#"{"id":2,"record_type":"single","release_date":"2021-05-01","nb_tracks":1,"artist":{"name":"Artist"}}"#
                .to_string(),
        ),
        ("/track/22", r#"{"id":22,"track_position":1,"disk_number":1}"#.to_string()),
        (
            "/search?q=",
            r#"{"data":[{"id":22,"title":"Song","duration":200,"album":{"id":2,"title":"Song"},"artist":{"name":"Artist"}}]}"#
                .to_string(),
        ),
    ];
    let song = Song {
        artist: "Artist".into(),
        title: "Song".into(),
        ..Default::default()
    };
    let tagging = build_with(vec![("1", song)], serve(routes).await, Options::default()).await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |_, _| {
        let dir = root.join(INCOMING_FOLDER_NAME);
        std::fs::create_dir_all(&dir).expect("made");
        let path = dir.join("upload.mp3");
        std::fs::write(&path, mp3()).expect("written");
        let mut file = TagFile::open(&path).expect("opens");
        file.set_title(Some("Some Channel - Song"));
        file.set_album(Some("Some Channel"));
        file.set_performers(&["Some Channel".to_string()]);
        file.save().expect("saved");
        path.to_string_lossy().into_owned()
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(
        TagFile::open(&path).expect("opens").album().as_deref(),
        Some("Song")
    );
    let report = tagging.last_entry().tagging.expect("a report");
    assert!(!report.candidates.iter().any(|c| c.source == "FileTags"));
    assert_eq!(report.confidence, "Medium");
}

// ---- C: the request's album is never overwritten ------------------------------------------

#[tokio::test]
async fn album_walk_track_keeps_the_requested_album_and_track() {
    let song = Song {
        album: "Collected".into(),
        track: Some(4),
        total_tracks: Some(14),
        ..teardrop()
    };
    let tagging = build_with(
        vec![("1", song)],
        serve(mezzanine_routes("Mezzanine")).await,
        Options::default(),
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |_| {})
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Collected"));
    assert_eq!(
        vorbis(&path, "TRACKNUMBER").and_then(|n| n.parse::<i32>().ok()),
        Some(4)
    );
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_TRACKID").as_deref(),
        Some("rec-teardrop")
    );
    // The chooser confirmed the compilation, which is the release it was asked about.
    assert_eq!(
        tagging
            .last_entry()
            .tagging
            .expect("a report")
            .release_id
            .as_deref(),
        Some("r-col")
    );
}

// ---- G: a catalog-only Medium fills blanks only -------------------------------------------

#[tokio::test]
async fn catalog_only_medium_fills_blanks_only_no_release_facts_written() {
    let routes = vec![
        (
            "/album/2",
            r#"{"id":2,"record_type":"single","release_date":"2021-05-01","nb_tracks":1,"label":"A Label","upc":"111111111111","artist":{"name":"Artist"}}"#
                .to_string(),
        ),
        (
            "/track/22",
            r#"{"id":22,"track_position":1,"disk_number":1,"isrc":"USAAA2100001"}"#.to_string(),
        ),
        (
            "/search?q=",
            r#"{"data":[{"id":22,"title":"Song","duration":200,"album":{"id":2,"title":"Song"},"artist":{"name":"Artist"}}]}"#
                .to_string(),
        ),
    ];
    let song = Song {
        artist: "Artist".into(),
        title: "Song".into(),
        ..Default::default()
    };
    let tagging = build_with(vec![("1", song)], serve(routes).await, Options::default()).await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |_, _| {
        landed_flac(&root, "peer share", |file| {
            file.set_title(Some("Song"));
            file.set_performers(&["Artist".to_string()]);
            file.set_album(Some("Peer Album"));
        })
    });

    let path = tagging.download("1").await.expect("downloaded");

    let report = tagging.last_entry().tagging.expect("a report");
    assert_eq!(report.confidence, "Medium");
    assert_eq!(report.source.as_deref(), Some("Catalog"));
    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Song"));
    assert_eq!(vorbis(&path, "DATE").as_deref(), Some("2021"));
    assert_eq!(vorbis(&path, "ISRC").as_deref(), Some("USAAA2100001"));
    // Only the chooser writes a release's kind and status, and it did not get to.
    assert_eq!(vorbis(&path, "RELEASETYPE"), None);
    assert_eq!(vorbis(&path, "RELEASESTATUS"), None);
    assert_eq!(vorbis(&path, "CATALOGNUMBER"), None);
    assert!(!report.fields.contains_key("album"));
}

// ---- H: the database down and the catalog throttled ---------------------------------------

#[tokio::test]
async fn database_down_and_catalog_throttled_keeps_the_fingerprints_release_and_says_so() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(DownAndThrottled)
        .mount(&server)
        .await;
    let tagging = build_with(vec![("1", teardrop())], server, Options::default()).await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |_| {})
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Mezzanine"));
    assert_eq!(vorbis(&path, "DATE").as_deref(), Some("1998"));
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_TRACKID").as_deref(),
        Some("rec-teardrop")
    );
    assert_eq!(vorbis(&path, "LABEL"), None);
    assert_eq!(vorbis(&path, "CATALOGNUMBER"), None);
    let report = tagging.last_entry().tagging.expect("a report");
    assert_eq!(report.confidence, "Strong");
    assert!(report.notes.iter().any(|n| n.contains("catalog did not answer")));
    assert!(report.notes.iter().any(|n| n.contains("release lookup")));
    assert!(!report.candidates.iter().any(|c| c.source == "Catalog"));
}

// ---- I: a rip from a compilation ----------------------------------------------------------

const NOW_LOOKUP: &str = r#"
{"status": "ok", "results": [{"id": "acoustid-1", "score": 0.95, "recordings": [{
  "id": "rec-teardrop", "title": "Teardrop", "duration": 330, "sources": 40,
  "artists": [{"id": "a-ma", "name": "Massive Attack"}],
  "releasegroups": [
    {"id": "g-now42", "title": "Now That's What I Call Music! 42", "type": "Album", "secondarytypes": ["Compilation"],
     "artists": [{"id": "va", "name": "Various Artists"}],
     "releases": [{"id": "r-now", "date": {"year": 1999, "month": 4, "day": 12}, "country": "GB",
       "mediums": [{"position": 1, "track_count": 21, "tracks": [{"id": "t-now", "position": 9}]}]}]},
    {"id": "g-mezzanine", "title": "Mezzanine", "type": "Album",
     "artists": [{"id": "a-ma", "name": "Massive Attack"}],
     "releases": [{"id": "r-mezz", "date": {"year": 1998, "month": 4, "day": 20}, "country": "GB",
       "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m", "position": 3}]}]}]}
  ]}]}]}
"#;

async fn compilation_rip(prefer_original_album: bool) -> Tagging {
    compilation_rip_tagged(prefer_original_album, None).await
}

/// `CompilationRip(preferOriginalAlbum, peer)`: `peer` adds what the peer wrote to the file.
async fn compilation_rip_tagged(prefer_original_album: bool, peer: Option<fn(&mut TagFile)>) -> Tagging {
    let routes = vec![
        ("release/r-mezz?", MEZZANINE_DETAILS.to_string()),
        ("/search?q=", r#"{"data":[]}"#.to_string()),
    ];
    let options = Options {
        metadata: MetadataSettings {
            prefer_original_album,
            ..Default::default()
        },
        ..Default::default()
    };
    let tagging = build_with(vec![("1", teardrop())], serve(routes).await, options).await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        let lookup = lookup(NOW_LOOKUP);
        let recording = lookup.results[0].recordings[0].clone();
        let verdict = VerificationResult {
            verdict: VerificationVerdict::Confirmed,
            score: 0.95,
            recording_id: Some(recording.recording_id.clone()),
            matched_album: recording.album_title.clone(),
            r#match: Some(recording),
            lookup: Some(lookup),
            acoust_id: Some("acoustid-1".into()),
            ..Default::default()
        };
        confirm(song, verdict);
        landed_flac(&root, "peer share", |file| {
            file.set_album(Some("Now That's What I Call Music! 42"));
            file.set_album_artists(&["Various Artists".to_string()]);
            tag_writer_extras::set_compilation(file, true);
            if let Some(peer) = peer {
                peer(file);
            }
        })
    });
    tagging
}

/// A Vorbis field set directly, through the writer's own field setter.
fn set_vorbis(file: &mut TagFile, name: &'static str, value: &str) {
    let field = TagField {
        id3_frame: None,
        id3_description: Some(name),
        vorbis: name,
        mp4: name,
    };
    tag_writer_extras::set_text(file, field, Some(value));
}

/// What a peer who tagged the compilation wrote about it: every one of these names the
/// compilation, not the studio album the song is filed under.
fn tagged_for_the_compilation(file: &mut TagFile) {
    set_vorbis(
        file,
        "MUSICBRAINZ_ALBUMID",
        "0b1c2d3e-4f50-4617-8899-aabbccddeeff",
    );
    set_vorbis(file, "MUSICBRAINZ_RELEASETRACKID", "t-now");
    set_vorbis(file, "MUSICBRAINZ_RELEASEGROUPID", "g-now42");
    set_vorbis(file, "RELEASEDATE", "1999-04-12");
    set_vorbis(file, "UPC", "0724352168227");
    set_vorbis(file, "ORGANIZATION", "EMI");
    set_vorbis(file, "TRACKNUMBER", "9");
    set_vorbis(file, "TRACKTOTAL", "21");
    set_vorbis(file, "DISCNUMBER", "2");
    set_vorbis(file, "TOTALDISCS", "2");
    set_vorbis(file, "REPLAYGAIN_ALBUM_GAIN", "-9.40 dB");
}

// ---- a download keeps only its own album's facts -----------------------------------------

#[tokio::test]
async fn compilation_rip_filed_under_the_studio_album_keeps_none_of_the_compilations_facts() {
    let tagging = compilation_rip_tagged(true, Some(tagged_for_the_compilation)).await;

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Mezzanine"));
    let number = |field: &str| vorbis(&path, field).and_then(|text| text.parse::<i32>().ok());
    assert_eq!(
        (
            number("TRACKNUMBER"),
            number("TRACKTOTAL"),
            number("DISCNUMBER"),
            number("DISCTOTAL")
        ),
        (Some(3), Some(11), Some(1), Some(1))
    );
    assert_eq!(vorbis(&path, "BARCODE").as_deref(), Some("724384559922"));
    assert_eq!(vorbis_all(&path, "LABEL"), ["Virgin"]);
    for field in [
        "MUSICBRAINZ_ALBUMID",
        "RELEASEDATE",
        "UPC",
        "ORGANIZATION",
        "TOTALDISCS",
        "REPLAYGAIN_ALBUM_GAIN",
        "COMPILATION",
    ] {
        assert_eq!(vorbis(&path, field), None, "{field} should have gone");
    }
    assert_ne!(
        vorbis(&path, "MUSICBRAINZ_RELEASETRACKID").as_deref(),
        Some("t-now")
    );
    assert_ne!(
        vorbis(&path, "MUSICBRAINZ_RELEASEGROUPID").as_deref(),
        Some("g-now42")
    );
    let report = tagging.last_entry().tagging.expect("a report");
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("Now That's What I Call Music! 42") && note.contains("UPC")),
        "{:?}",
        report.notes
    );
}

#[tokio::test]
async fn compilation_kept_loses_only_the_grouping_values() {
    let tagging = compilation_rip_tagged(false, Some(tagged_for_the_compilation)).await;

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(
        vorbis(&path, "ALBUM").as_deref(),
        Some("Now That's What I Call Music! 42")
    );
    assert_eq!(vorbis(&path, "MUSICBRAINZ_ALBUMID"), None);
    assert_eq!(vorbis(&path, "RELEASEDATE"), None);
    // Still the compilation, so what the peer wrote about it stays.
    assert_eq!(vorbis(&path, "ORGANIZATION").as_deref(), Some("EMI"));
    assert_eq!(vorbis(&path, "COMPILATION").as_deref(), Some("1"));
    assert_eq!(
        vorbis(&path, "REPLAYGAIN_ALBUM_GAIN").as_deref(),
        Some("-9.40 dB")
    );
}

#[tokio::test]
async fn same_album_from_a_peer_ends_with_one_label_one_barcode_and_no_grouping_values() {
    let tagging = build_with(
        vec![("1", teardrop())],
        serve(mezzanine_routes("Mezzanine")).await,
        Options::default(),
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |file| {
            file.set_album(Some("Mezzanine"));
            set_vorbis(
                file,
                "MUSICBRAINZ_ALBUMID",
                "0b1c2d3e-4f50-4617-8899-aabbccddeeff",
            );
            set_vorbis(file, "RELEASEDATE", "2019-08-23");
            set_vorbis(file, "ORGANIZATION", "Circa");
            set_vorbis(file, "UPC", "724384559922");
            set_vorbis(file, "MUSICBRAINZ_ALBUMTYPE", "album");
        })
    });

    let path = tagging.download("1").await.expect("downloaded");

    let view = KeptIdentityTags::navidrome_view(&TagFile::open(&path).expect("opens"));
    assert_eq!(view["label"], ["Virgin"]);
    assert!(!view.contains_key("organization"));
    assert_eq!(view["barcode"], ["724384559922"]);
    assert!(!view.contains_key("upc"));
    assert!(!view.contains_key("musicbrainz_albumtype"));
    let identity = KeptIdentityTags::read(Path::new(&path), None).expect("reads");
    assert_eq!(identity.album_id, None);
    assert_eq!(identity.release_date, None);
}

#[tokio::test]
async fn joining_an_album_folder_carries_the_albums_own_grouping_values() {
    const THEIRS: &str = "99999999-8888-4777-8666-555555555555";
    let tagging = build_with(
        vec![("1", teardrop())],
        serve(mezzanine_routes("Collected")).await,
        Options {
            layout: FolderStructure::Organized,
            metadata: MetadataSettings {
                tag_rehearsal: true,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;
    let target = octo_core::common::PathHelper::build_layout_path(
        FolderStructure::Organized,
        &tagging.root().to_string_lossy(),
        "Massive Attack",
        "Collected",
        "Teardrop",
        Some(4),
        ".flac",
    );
    let folder = Path::new(&target).parent().expect("a folder").to_path_buf();
    std::fs::create_dir_all(&folder).expect("made");
    let sibling = folder.join("01 - Unfinished Sympathy.flac");
    std::fs::write(&sibling, flac()).expect("written");
    {
        let mut file = TagFile::open(&sibling).expect("opens");
        file.set_title(Some("Unfinished Sympathy"));
        file.set_album(Some("Collected"));
        file.set_album_artists(&["Massive Attack".to_string()]);
        set_vorbis(&mut file, "MUSICBRAINZ_ALBUMID", THEIRS);
        set_vorbis(&mut file, "RELEASEDATE", "2006-03-27");
        file.save().expect("saved");
    }
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        // A peer's own release id for another pressing of the same compilation.
        landed_flac(&root, "peer share", |file| {
            set_vorbis(
                file,
                "MUSICBRAINZ_ALBUMID",
                "0b1c2d3e-4f50-4617-8899-aabbccddeeff",
            );
        })
    });

    // A rehearsal files it under the catalog's album, the one the folder holds.
    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(Path::new(&path).parent(), Some(folder.as_path()));
    let identity = KeptIdentityTags::read(Path::new(&path), None).expect("reads");
    assert_eq!(
        (identity.album_id.as_deref(), identity.release_date.as_deref()),
        (Some(THEIRS), Some("2006-03-27"))
    );
}

/// An album sibling with its grouping values, at `relative` under the music folder, of disc
/// `disc` of `discs` when given.
fn sibling_in(root: &Path, relative: &str, album: &str, discs: Option<(u32, u32)>) -> String {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a folder")).expect("made");
    std::fs::write(&path, flac()).expect("written");
    let mut file = TagFile::open(&path).expect("opens");
    file.set_title(Some("Unfinished Sympathy"));
    file.set_album(Some(album));
    file.set_performers(&["Massive Attack".to_string()]);
    file.set_album_artists(&["Massive Attack".to_string()]);
    if let Some((disc, count)) = discs {
        file.set_disc(disc);
        file.set_disc_count(count);
    }
    set_vorbis(
        &mut file,
        "MUSICBRAINZ_ALBUMID",
        "99999999-8888-4777-8666-555555555555",
    );
    set_vorbis(&mut file, "RELEASEDATE", "2006-03-27");
    file.save().expect("saved");
    path.to_string_lossy().into_owned()
}

/// A rehearsal download of Teardrop, which files it under the catalog's album `album`, in
/// `layout` (with `template` for Custom).
async fn sibling_rig(album: &str, layout: FolderStructure, template: Option<&'static str>) -> Tagging {
    let tagging = build_with(
        vec![("1", teardrop())],
        serve(mezzanine_routes(album)).await,
        Options {
            layout,
            template,
            metadata: MetadataSettings {
                tag_rehearsal: true,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |_| {})
    });
    tagging
}

fn grouping_of(path: &str) -> (Option<String>, Option<String>) {
    let identity = KeptIdentityTags::read(Path::new(path), None).expect("reads");
    (identity.album_id, identity.release_date)
}

fn their_grouping() -> (Option<String>, Option<String>) {
    (
        Some("99999999-8888-4777-8666-555555555555".to_string()),
        Some("2006-03-27".to_string()),
    )
}

/// The album's folder was named by an older Octo, with a character Windows refuses: identify
/// still finds the sibling there, and the new track joins it.
#[cfg(unix)]
#[tokio::test]
async fn sibling_found_in_old_named_album_folder() {
    let tagging = sibling_rig("Collected?", FolderStructure::Organized, None).await;
    sibling_in(
        tagging.root(),
        "Massive Attack/Collected?/01 - Unfinished Sympathy.flac",
        "Collected?",
        None,
    );

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(
        Path::new(&path).parent(),
        Some(tagging.root().join("Massive Attack/Collected?").as_path())
    );
    assert_eq!(grouping_of(&path), their_grouping());
}

#[tokio::test]
async fn custom_album_folder_joins_its_sibling() {
    let tagging = sibling_rig(
        "Collected",
        FolderStructure::Custom,
        Some("{Album Artist} - {Album}/{Track:00} - {Title}"),
    )
    .await;
    sibling_in(
        tagging.root(),
        "Massive Attack - Collected/01 - Unfinished Sympathy.flac",
        "Collected",
        None,
    );

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(
        Path::new(&path).parent(),
        Some(tagging.root().join("Massive Attack - Collected").as_path())
    );
    assert_eq!(grouping_of(&path), their_grouping());
}

/// A disc 1 track whose release did not say how many discs there are joins an album already
/// filed by disc in disc 1's folder, and takes the album's disc count from its sibling.
#[tokio::test]
async fn disc_folder_is_consistent_when_the_count_is_unknown() {
    let tagging = sibling_rig(
        "Collected",
        FolderStructure::Custom,
        Some("{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}"),
    )
    .await;
    sibling_in(
        tagging.root(),
        "Massive Attack/Collected/Disc 1/01 - Unfinished Sympathy.flac",
        "Collected",
        Some((1, 2)),
    );
    sibling_in(
        tagging.root(),
        "Massive Attack/Collected/Disc 2/01 - Safe From Harm.flac",
        "Collected",
        Some((2, 2)),
    );

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(
        Path::new(&path).parent(),
        Some(tagging.root().join("Massive Attack/Collected/Disc 1").as_path())
    );
    assert_eq!(vorbis(&path, "DISCTOTAL").as_deref(), Some("2"));
}

/// "Money Trees" twice in the catalog: the explicit copy first, then the clean edit, each with
/// its own code.
fn money_trees_routes() -> Vec<(&'static str, String)> {
    vec![
        (
            "/album/7",
            r#"{"id":7,"record_type":"album","release_date":"2012-10-22","nb_tracks":12,"artist":{"name":"Kendrick Lamar"}}"#.to_string(),
        ),
        (
            "/album/8",
            r#"{"id":8,"record_type":"album","release_date":"2012-10-22","nb_tracks":12,"artist":{"name":"Kendrick Lamar"}}"#.to_string(),
        ),
        (
            "/track/71",
            r#"{"id":71,"track_position":5,"disk_number":1,"isrc":"USUM71210782","explicit_lyrics":true,"explicit_content_lyrics":1}"#.to_string(),
        ),
        (
            "/track/81",
            r#"{"id":81,"track_position":5,"disk_number":1,"isrc":"USUM71210787","explicit_lyrics":false,"explicit_content_lyrics":3}"#.to_string(),
        ),
        (
            "/search?q=",
            r#"{"data":[
              {"id":71,"title":"Money Trees","duration":386,"explicit_lyrics":true,"album":{"id":7,"title":"good kid, m.A.A.d city"},"artist":{"name":"Kendrick Lamar"}},
              {"id":81,"title":"Money Trees","duration":386,"explicit_lyrics":false,"album":{"id":8,"title":"good kid, m.A.A.d city"},"artist":{"name":"Kendrick Lamar"}}]}"#
                .to_string(),
        ),
    ]
}

fn money_trees() -> Song {
    Song {
        artist: "Kendrick Lamar".into(),
        title: "Money Trees".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_explicit_copy_that_landed_is_tagged_explicit_and_the_clean_one_clean() {
    async fn landing(isrc: &'static str) -> (String, octo_core::tagging::TagReport) {
        let tagging = build_with(
            vec![("1", money_trees())],
            serve(money_trees_routes()).await,
            Options::default(),
        )
        .await;
        let root = tagging.root().to_path_buf();
        tagging.harness.backend.land(move |_, _| {
            landed_flac(&root, "peer share", |file| {
                file.set_title(Some("Money Trees"));
                file.set_performers(&["Kendrick Lamar".to_string()]);
                tag_writer_extras::set_text(file, octo_media::tags::TagFields::ISRC, Some(isrc));
            })
        });
        let path = tagging.download("1").await.expect("downloaded");
        let report = tagging.last_entry().tagging.expect("a report");
        let advisory = vorbis(&path, "ITUNESADVISORY");
        (advisory.unwrap_or_default(), report)
    }

    let (explicit_copy, report) = landing("USUM71210782").await;
    assert_eq!(explicit_copy, "1");
    assert_eq!(
        report.fields.get("advisory"),
        Some(&FieldDecision::new(Some("explicit"), Some("Catalog")))
    );

    // The catalog's first hit is the explicit one; the file's own code says it is the edit.
    let (clean_copy, _) = landing("USUM71210787").await;
    assert_eq!(clean_copy, "2");
}

/// Try it on a song shows the advisory and what a download would take off, and writes nothing.
#[tokio::test]
async fn try_it_on_a_song_shows_the_advisory_and_what_a_download_would_take_off_and_writes_nothing() {
    let tagging = build_with(vec![], serve(money_trees_routes()).await, Options::default()).await;
    let path = landed_flac(tagging.root(), "library", |file| {
        file.set_title(Some("Money Trees"));
        file.set_performers(&["Kendrick Lamar".to_string()]);
        file.set_album(Some("Kendrick Lamar Mixtape Vol. 2"));
        set_vorbis(file, "ISRC", "USUM71210787");
        set_vorbis(
            file,
            "MUSICBRAINZ_ALBUMID",
            "0b1c2d3e-4f50-4617-8899-aabbccddeeff",
        );
        set_vorbis(file, "CATALOGNUMBER", "MIX-2");
    });
    let before = std::fs::read(&path).expect("read");
    let service = &tagging.harness.service;
    let preview = octo_core::tagging::TagPreview::new(
        Arc::clone(&service.settings),
        Arc::clone(service.identifier()),
        Arc::new(octo_media::tags::TagWriterExtras),
        None,
        None,
        Arc::new(CatalogBlanks),
    );

    let report = preview
        .preview(Some(&path), None, None, None, &CancellationToken::new())
        .await
        .expect("a report");

    assert_eq!(
        report.fields.get("advisory"),
        Some(&FieldDecision::new(Some("clean edit"), Some("Catalog")))
    );
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.starts_with("a download would take off")
                && note.contains("MUSICBRAINZ_ALBUMID")
                && note.contains("CATALOGNUMBER")),
        "{:?}",
        report.notes
    );
    assert_eq!(std::fs::read(&path).expect("read"), before);
}

#[tokio::test]
async fn compilation_rip_prefer_original_album_on_is_filed_under_the_studio_album() {
    let tagging = compilation_rip(true).await;

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Mezzanine"));
    assert_eq!(vorbis(&path, "CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(vorbis(&path, "COMPILATION"), None);
    assert_eq!(
        tagging.last_entry().tagging.expect("a report").confidence,
        "Medium"
    );
}

#[tokio::test]
async fn compilation_rip_prefer_original_album_off_keeps_the_compilation() {
    let tagging = compilation_rip(false).await;

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(
        vorbis(&path, "ALBUM").as_deref(),
        Some("Now That's What I Call Music! 42")
    );
    assert_eq!(vorbis(&path, "COMPILATION").as_deref(), Some("1"));
    assert_eq!(
        tagging
            .last_entry()
            .tagging
            .expect("a report")
            .release_id
            .as_deref(),
        Some("r-now")
    );
}

// ---- rehearsal ----------------------------------------------------------------------------

#[tokio::test]
async fn rehearsal_writes_todays_set_plus_the_additive_facts_and_reports_the_plan() {
    let options = Options {
        metadata: MetadataSettings {
            tag_rehearsal: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let tagging = build_with(
        vec![("1", teardrop())],
        serve(mezzanine_routes("Collected")).await,
        options,
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |_| {})
    });

    let path = tagging.download("1").await.expect("downloaded");

    // What the old rules wrote: the catalog's own album for a song that arrived without one.
    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Collected"));
    assert_eq!(vorbis(&path, "CATALOGNUMBER"), None);
    assert_eq!(vorbis(&path, "MUSICBRAINZ_RELEASETRACKID"), None);
    assert_eq!(vorbis(&path, "RELEASESTATUS"), None);
    // The additive facts that do not depend on the match.
    assert_eq!(
        vorbis(&path, "REPLAYGAIN_TRACK_GAIN").as_deref(),
        Some("-6.50 dB")
    );
    assert_eq!(vorbis(&path, "ISRC").as_deref(), Some("GBAAA9800001"));
    assert_eq!(vorbis(&path, "ACOUSTID_ID").as_deref(), Some("acoustid-1"));
    let report = tagging.last_entry().tagging.expect("a report");
    assert!(report.rehearsed);
    assert_eq!(report.confidence, "Strong");
    assert_eq!(report.release_title.as_deref(), Some("Mezzanine"));
}

// ---- loudness that could not be measured --------------------------------------------------

#[tokio::test]
async fn measurement_that_gives_nothing_leaves_no_replay_gain_and_the_report_says_so() {
    let options = Options {
        loudness: Some(Loudness::new(f64::NEG_INFINITY, 0.0, f64::NEG_INFINITY)),
        ..Default::default()
    };
    let tagging = build_with(
        vec![("1", teardrop())],
        serve(mezzanine_routes("Mezzanine")).await,
        options,
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |_| {})
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "REPLAYGAIN_TRACK_GAIN"), None);
    assert!(
        tagging
            .last_entry()
            .tagging
            .expect("a report")
            .notes
            .iter()
            .any(|n| n.contains("loudness could not be measured"))
    );
}

// ---- two tracks of one walk share the album-level fields ----------------------------------

#[tokio::test]
async fn two_tracks_of_one_walk_share_the_release_facts() {
    let songs = vec![
        (
            "1",
            Song {
                album: "Mezzanine".into(),
                track: Some(3),
                ..teardrop()
            },
        ),
        (
            "2",
            Song {
                title: "Angel".into(),
                album: "Mezzanine".into(),
                track: Some(1),
                ..teardrop()
            },
        ),
    ];
    let options = Options {
        layout: FolderStructure::Organized,
        ..Default::default()
    };
    let tagging = build_with(songs, serve(mezzanine_routes("Mezzanine")).await, options).await;
    let context = Arc::new(AlbumTagContext::<Loudness>::new(
        Some("1"),
        "Mezzanine",
        Some("Massive Attack"),
    ));
    let root = tagging.root().to_path_buf();
    let second = Arc::new(AtomicBool::new(false));
    tagging.harness.backend.land(move |song, _| {
        let mut verdict = confirmed_teardrop();
        if second.load(Ordering::SeqCst) {
            // The second track's fingerprint names only the 2019 pressing of the same album.
            let lookup = lookup(
                &TEARDROP_LOOKUP
                    .replace("\"r-mezz\"", "\"r-gone\"")
                    .replace("1998", "2019"),
            );
            let mut matched = lookup.results[0].recordings[0].clone();
            matched.recording_id = "rec-angel".into();
            verdict = VerificationResult {
                lookup: Some(lookup),
                r#match: Some(matched),
                recording_id: Some("rec-angel".into()),
                ..verdict
            };
        }
        confirm(song, verdict);
        let title = song.title.clone();
        let path = landed_flac(&root, "peer share", |file| file.set_title(Some(&title)));
        second.store(true, Ordering::SeqCst);
        path
    });

    let first = tagging.download_in_walk("1", &context).await;
    let next = tagging.download_in_walk("2", &context).await;

    assert!(context.captured());
    assert_eq!(vorbis(&first, "CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(vorbis(&next, "CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(vorbis(&first, "DATE"), vorbis(&next, "DATE"));
    assert_eq!(vorbis(&first, "LABEL"), vorbis(&next, "LABEL"));
    assert_eq!(context.loudness().len(), 2);
    assert_eq!(Path::new(&first).parent(), Path::new(&next).parent());
}

#[tokio::test]
async fn write_album_gain_writes_the_album_values_into_every_measured_file() {
    let root = tempfile::tempdir().expect("a temp dir");
    let harness = build(
        root.path(),
        AppSettings::default(),
        None,
        None,
        DownloadServices::default(),
    );
    let context = AlbumTagContext::<Loudness>::new(Some("1"), "Mezzanine", Some("Massive Attack"));
    let a = landed_flac(root.path(), "album", |_| {});
    let b = landed_flac(root.path(), "album", |_| {});
    let (la, lb) = (Loudness::new(-10.0, 0.0, -1.0), Loudness::new(-14.0, 0.0, -0.5));
    context.set_loudness(&a, Some(la));
    context.set_loudness(&b, Some(lb));

    harness.service.write_album_gain(&context, "Mezzanine");

    let expected = ReplayGainTags::for_album(&[Some(la), Some(lb)]).expect("an album gain");
    assert_eq!(vorbis(&a, "REPLAYGAIN_ALBUM_GAIN"), Some(expected.gain_text()));
    assert_eq!(vorbis(&b, "REPLAYGAIN_ALBUM_GAIN"), Some(expected.gain_text()));
    assert_eq!(vorbis(&b, "REPLAYGAIN_ALBUM_PEAK"), Some(expected.peak_text()));
}

// ---- #69: a download with nothing on disk fails -------------------------------------------

#[tokio::test]
async fn a_landed_path_with_no_audio_fails_and_is_never_recorded() {
    for empty_file in [false, true] {
        let tagging = build_with(vec![("1", teardrop())], serve(vec![]).await, Options::default()).await;
        let path = tagging.root().join("slskd/incomplete/peer/Teardrop.flac");
        if empty_file {
            std::fs::create_dir_all(path.parent().expect("a folder")).expect("made");
            std::fs::write(&path, []).expect("written");
        }
        let landed = path.to_string_lossy().into_owned();
        tagging.harness.backend.land(move |_, _| landed.clone());

        let error = tagging.download("1").await.expect_err("nothing on disk");

        assert!(
            error.downcast_ref::<FileNotFoundException>().is_some(),
            "{empty_file}: {error}"
        );
        assert!(tagging.harness.history.get_recent(50).is_empty(), "{empty_file}");
        assert!(!tagging.meter.called(), "{empty_file}");
    }
}

// ---- an album heart files every track under the album asked for ----------------------------

const FUTURE_NOSTALGIA: &str = "Future Nostalgia";
const MOONLIGHT: &str = "Future Nostalgia (The Moonlight Edition)";

/// The hearted album's tracks in its own order: id, title, recording, and the track's place on
/// the larger edition that holds the same recordings.
const NOSTALGIA_TRACKS: [(&str, &str, &str, i32); 3] = [
    ("1", "Levitating (feat. DaBaby)", "rec-levitating", 18),
    ("2", "Future Nostalgia", "rec-title", 1),
    ("3", "Don't Start Now", "rec-dsn", 2),
];

/// Every album-level tag, which must read the same on every track of one album.
const ALBUM_TAGS: [&str; 18] = [
    "ALBUM",
    "ALBUMARTIST",
    "DATE",
    "TRACKTOTAL",
    "DISCTOTAL",
    "LABEL",
    "CATALOGNUMBER",
    "BARCODE",
    "RELEASETYPE",
    "RELEASESTATUS",
    "RELEASECOUNTRY",
    "ORIGINALDATE",
    "ORIGINALYEAR",
    "MUSICBRAINZ_RELEASEGROUPID",
    "MUSICBRAINZ_ALBUMARTISTID",
    "MUSICBRAINZ_ALBUMID",
    "RELEASEDATE",
    "COMPILATION",
];

/// The fingerprint service's answer for one recording: the larger edition first, with the
/// recording at its place there, then the album itself when the recording is on it.
fn nostalgia_lookup(recording: &str, title: &str, album_place: Option<i32>, edition_place: i32) -> String {
    let group = |id: &str, title: &str, release: &str, year: i32, tracks: i32, place: i32| {
        format!(
            r#"{{"id": "{id}", "title": "{title}", "type": "Album",
             "artists": [{{"id": "a-dua", "name": "Dua Lipa"}}],
             "releases": [{{"id": "{release}", "date": {{"year": {year}, "month": 3, "day": 27}}, "country": "XW",
               "mediums": [{{"position": 1, "track_count": {tracks}, "tracks": [{{"id": "t-{release}-{place}", "position": {place}}}]}}]}}]}}"#
        )
    };
    let mut groups = vec![group("g-moon", MOONLIGHT, "r-moon", 2021, 19, edition_place)];
    if let Some(place) = album_place {
        groups.push(group("g-fn", FUTURE_NOSTALGIA, "r-fn", 2020, 3, place));
    }
    format!(
        r#"{{"status": "ok", "results": [{{"id": "acoustid-{recording}", "score": 0.97, "recordings": [{{
          "id": "{recording}", "title": "{title}", "duration": 200.0, "sources": 40,
          "artists": [{{"id": "a-dua", "name": "Dua Lipa"}}],
          "releasegroups": [{}]}}]}}]}}"#,
        groups.join(",")
    )
}

fn nostalgia_details(release: &str, title: &str, group: &str, facts: [&str; 4], tracks: i32) -> String {
    let [date, label, catalog, barcode] = facts;
    format!(
        r#"{{"id": "{release}", "title": "{title}", "status": "Official", "date": "{date}", "country": "XW", "barcode": "{barcode}",
         "label-info": [{{"catalog-number": "{catalog}", "label": {{"id": "l-{release}", "name": "{label}"}}}}],
         "release-group": {{"id": "{group}", "title": "{title}", "primary-type": "Album", "secondary-types": [], "first-release-date": "{date}"}},
         "artist-credit": [{{"name": "Dua Lipa", "artist": {{"id": "a-dua", "name": "Dua Lipa"}}}}],
         "media": [{{"position": 1, "track-count": {tracks}, "tracks": []}}]}}"#
    )
}

fn nostalgia_routes() -> Vec<(&'static str, String)> {
    vec![
        (
            "release/r-fn?",
            nostalgia_details(
                "r-fn",
                FUTURE_NOSTALGIA,
                "g-fn",
                ["2020-03-27", "Warner FN", "FN-13", "190295269161"],
                3,
            ),
        ),
        (
            "release/r-moon?",
            nostalgia_details(
                "r-moon",
                MOONLIGHT,
                "g-moon",
                ["2021-02-11", "Warner Moon", "MOON-19", "190296521234"],
                19,
            ),
        ),
        ("/search?q=", r#"{"data":[]}"#.to_string()),
    ]
}

/// The hearted album as the catalog lists it.
fn nostalgia_album() -> Album {
    Album {
        id: "fn".into(),
        title: FUTURE_NOSTALGIA.into(),
        artist: "Dua Lipa".into(),
        year: Some(2020),
        songs: NOSTALGIA_TRACKS
            .iter()
            .zip(1..)
            .map(|((id, title, _, _), track)| Song {
                id: (*id).into(),
                title: (*title).into(),
                artist: "Dua Lipa".into(),
                album: FUTURE_NOSTALGIA.into(),
                track: Some(track),
                disc_number: Some(1),
                external_provider: Some("test".into()),
                external_id: Some((*id).into()),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// The hearted album fetched whole, its track ids resolving to songs the larger edition listed
/// last (its album, its places, its track count): by a walk, or song by song under a hold on
/// its listing, as a member's album is. `on_album` says which recordings the fingerprint
/// service also finds on the album itself, and at which place.
async fn fetch_nostalgia(on_album: fn(&str) -> Option<i32>, by_walk: bool) -> (Tagging, Vec<String>) {
    let routed: Vec<(&str, Song)> = NOSTALGIA_TRACKS
        .iter()
        .map(|(id, title, _, place)| {
            (
                *id,
                Song {
                    title: (*title).into(),
                    artist: "Dua Lipa".into(),
                    album: MOONLIGHT.into(),
                    track: Some(*place),
                    disc_number: Some(1),
                    total_tracks: Some(19),
                    external_provider: Some("test".into()),
                    external_id: Some((*id).into()),
                    ..Default::default()
                },
            )
        })
        .collect();
    let metadata = catalog(routed);
    metadata
        .albums
        .lock()
        .insert(("test".into(), "fn".into()), nostalgia_album());
    let options = Options {
        layout: FolderStructure::Organized,
        ..Default::default()
    };
    let tagging = build_with_catalog(metadata, serve(nostalgia_routes()).await, options).await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        let (_, title, recording, place) = NOSTALGIA_TRACKS
            .iter()
            .find(|track| track.1 == song.title)
            .copied()
            .expect("a listed track");
        confirm(
            song,
            confirmed_by(&nostalgia_lookup(recording, title, on_album(recording), place)),
        );
        landed_flac(&root, "peer share", |file| {
            file.set_title(Some(title));
            file.set_performers(&["Dua Lipa".to_string()]);
        })
    });

    let service = &tagging.harness.service;
    if by_walk {
        let walked = service
            .download_album_with_source(
                "test",
                "fn",
                DownloadSource::Soulseek,
                false,
                &CancellationToken::new(),
                None,
            )
            .await
            .expect("walked");
        assert!(walked);
    } else {
        let album = nostalgia_album();
        service.hold_album_listing("test", &album);
        for (id, ..) in NOSTALGIA_TRACKS {
            tagging.download(id).await.expect("downloaded");
        }
        service.finish_album_listing("test", &album).await;
        // The hold is over: the next download of a listed song is its own again.
        assert!(service.held_album("1").is_none());
    }
    let mut files: Vec<String> = files_below(&tagging.root().join("Dua Lipa"))
        .into_iter()
        .filter(|file| file.ends_with(".flac"))
        .collect();
    files.sort();
    (tagging, files)
}

/// Every track sits in the album's one folder at its own place on the album, and reads the same
/// on every album-level tag.
fn assert_one_album(files: &[String]) {
    assert_eq!(files.len(), NOSTALGIA_TRACKS.len(), "{files:?}");
    for file in files {
        assert_eq!(
            Path::new(file).parent().and_then(|dir| dir.file_name()),
            Some(std::ffi::OsStr::new(FUTURE_NOSTALGIA)),
            "{file}"
        );
        let title = vorbis(file, "TITLE").expect("a title");
        let place = NOSTALGIA_TRACKS
            .iter()
            .position(|track| track.1 == title)
            .expect("a listed title")
            + 1;
        assert_eq!(vorbis(file, "TRACKNUMBER"), Some(place.to_string()), "{file}");
        assert!(
            vorbis_all(file, "MUSICBRAINZ_RELEASETRACKID")
                .iter()
                .all(|id| !id.starts_with("t-r-moon")),
            "{file}"
        );
    }
    for name in ALBUM_TAGS {
        let first = vorbis_all(&files[0], name);
        for file in &files[1..] {
            assert_eq!(vorbis_all(file, name), first, "{name} on {file}");
        }
    }
    assert_eq!(vorbis(&files[0], "ALBUM").as_deref(), Some(FUTURE_NOSTALGIA));
    assert_eq!(vorbis(&files[0], "ALBUMARTIST").as_deref(), Some("Dua Lipa"));
    assert_eq!(vorbis(&files[0], "DATE").as_deref(), Some("2020"));
    assert_eq!(vorbis(&files[0], "TRACKTOTAL").as_deref(), Some("3"));
    assert_ne!(vorbis(&files[0], "LABEL").as_deref(), Some("Warner Moon"));
    assert_ne!(
        vorbis(&files[0], "MUSICBRAINZ_RELEASEGROUPID").as_deref(),
        Some("g-moon")
    );
}

/// The second and third recordings are on the album itself, at its places 2 and 3; the first
/// is found on the larger edition alone.
fn last_two_on_the_album(recording: &str) -> Option<i32> {
    ["rec-title", "rec-dsn"]
        .iter()
        .position(|on| *on == recording)
        .map(|at| at as i32 + 2)
}

#[tokio::test]
async fn an_album_heart_files_every_track_under_the_album_asked_for_with_one_set_of_album_tags() {
    // By a walk and song by song alike. The first track is tagged before the album's own
    // release is known, and gets its facts once the album is in.
    for by_walk in [true, false] {
        let (_tagging, files) = fetch_nostalgia(last_two_on_the_album, by_walk).await;

        assert_one_album(&files);
        assert_eq!(
            vorbis(&files[0], "LABEL").as_deref(),
            Some("Warner FN"),
            "{by_walk}"
        );
        assert_eq!(vorbis(&files[0], "CATALOGNUMBER").as_deref(), Some("FN-13"));
        assert_eq!(
            vorbis(&files[0], "MUSICBRAINZ_RELEASEGROUPID").as_deref(),
            Some("g-fn")
        );
        assert_eq!(
            vorbis(&files[0], "MUSICBRAINZ_ALBUMARTISTID").as_deref(),
            Some("a-dua")
        );
    }
}

#[tokio::test]
async fn an_album_heart_no_track_of_which_matches_the_album_still_makes_one_album() {
    let (_tagging, files) = fetch_nostalgia(|_| None, true).await;

    assert_one_album(&files);
    assert_eq!(vorbis(&files[0], "CATALOGNUMBER"), None);
    assert_eq!(vorbis(&files[0], "MUSICBRAINZ_RELEASEGROUPID"), None);
}

// ---- a file named for another format --------------------------------------------------------

/// Every file under a folder, at any depth.
fn files_below(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_below(&path));
        } else {
            found.push(path.to_string_lossy().into_owned());
        }
    }
    found
}

#[tokio::test]
async fn the_downloads_log_says_what_came_off_the_peers_tags_and_whether_the_song_is_explicit() {
    let song = Song {
        external_provider: Some("test".into()),
        external_id: Some("1".into()),
        ..money_trees()
    };
    let tracker = Arc::new(AcquisitionTracker::new(None, octo_core::common::Clock::system()));
    let tagging = build_with(
        vec![("1", song)],
        serve(money_trees_routes()).await,
        Options {
            tracker: Some(tracker.clone()),
            ..Options::default()
        },
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |_, _| {
        landed_flac(&root, "peer share", |file| {
            file.set_title(Some("Money Trees"));
            file.set_performers(&["Kendrick Lamar".to_string()]);
            tag_writer_extras::set_text(file, octo_media::tags::TagFields::ISRC, Some("USUM71210782"));
            set_vorbis(
                file,
                "MUSICBRAINZ_ALBUMID",
                "0b1c2d3e-4f50-4617-8899-aabbccddeeff",
            );
        })
    });
    tracker.begin("test", "1", None, Some("alice"), None, None, None);

    tagging.download("1").await.expect("downloaded");

    let log = tracker
        .detail("test:1", Some("alice"))
        .and_then(|row| row.events)
        .expect("the log");
    assert!(
        log.iter().any(|e| e.kind == "tags"
            && e.text == "Marked explicit"
            && e.detail.as_deref() == Some("The catalog's match for this exact version says so")),
        "{log:?}"
    );
    assert!(
        log.iter().any(|e| e.kind == "tags"
            && e.text == "Took off the peer's own album tags"
            && e.detail
                .as_deref()
                .is_some_and(|d| d.to_uppercase().contains("MUSICBRAINZ_ALBUMID"))),
        "{log:?}"
    );
}

#[tokio::test]
async fn a_flac_named_mp3_is_renamed_before_tagging_and_the_log_and_report_say_so() {
    let song = Song {
        external_provider: Some("test".into()),
        external_id: Some("1".into()),
        ..teardrop()
    };
    let tracker = Arc::new(AcquisitionTracker::new(None, octo_core::common::Clock::system()));
    let tagging = build_with(
        vec![("1", song)],
        serve(mezzanine_routes("Mezzanine")).await,
        Options {
            tracker: Some(tracker.clone()),
            ..Options::default()
        },
    )
    .await;
    tracker.begin("test", "1", None, Some("alice"), None, None, None);
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        let flac = landed_flac(&root, "peer share", |_| {});
        let misnamed = format!("{}.mp3", flac.strip_suffix(".flac").expect("a FLAC"));
        std::fs::rename(&flac, &misnamed).expect("renamed");
        misnamed
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert!(path.ends_with(".flac"), "{path}");
    // Written as a FLAC: the tagger read it by its new name.
    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Mezzanine"));
    assert!(
        !files_below(tagging.root())
            .iter()
            .any(|file| file.ends_with(".mp3")),
        "no .mp3 left"
    );
    let report = tagging.last_entry().tagging.expect("a report");
    assert!(
        report
            .notes
            .contains(&"renamed from .mp3 to .flac: the file is a FLAC".to_string()),
        "{:?}",
        report.notes
    );
    let log = tracker
        .detail("test:1", Some("alice"))
        .and_then(|row| row.events)
        .expect("the log");
    assert!(
        log.iter().any(|e| e.kind == "check"
            && e.text == "Renamed to .flac: the file is a FLAC"
            && e.detail.as_deref() == Some("It arrived named .mp3")),
        "{log:?}"
    );
}

// ---- a download with no album and the no-album pick ---------------------------------------

const YESTERDAY_LOOKUP: &str = r#"
{"status": "ok", "results": [{"id": "acoustid-1", "score": 0.97, "recordings": [{
  "id": "rec-yesterday", "title": "Yesterday", "duration": 125.0, "sources": 40, "isrcs": ["GBAYE0601477"],
  "artists": [{"id": "a-beatles", "name": "The Beatles"}],
  "releasegroups": [
    {"id": "g-yesterday-and-today", "title": "Yesterday and Today", "type": "Album",
     "artists": [{"id": "a-beatles", "name": "The Beatles"}],
     "releases": [{"id": "r-yt", "date": {"year": 1966, "month": 6, "day": 20}, "country": "US",
       "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-yt", "position": 5}]}]}]},
    {"id": "g-help", "title": "Help!", "type": "Album", "secondarytypes": ["Soundtrack"],
     "artists": [{"id": "a-beatles", "name": "The Beatles"}],
     "releases": [{"id": "r-help", "date": {"year": 1965, "month": 8, "day": 6}, "country": "GB",
       "mediums": [{"position": 1, "track_count": 14, "tracks": [{"id": "t-help", "position": 13}]}]}]}
  ]}]}]}
"#;

/// Yesterday asked for with no album, its ISRC known and its sound confirmed.
fn yesterday() -> Song {
    let mut song = Song {
        artist: "The Beatles".into(),
        title: "Yesterday".into(),
        isrc: Some("GBAYE0601477".into()),
        duration: Some(125),
        ..Default::default()
    };
    confirm(&mut song, confirmed_by(YESTERDAY_LOOKUP));
    song.album = String::new();
    song
}

fn landed_yesterday(root: &Path) -> String {
    landed_flac(root, "peer share", |file| {
        file.set_title(Some("Yesterday"));
        file.set_performers(&["The Beatles".to_string()]);
    })
}

#[tokio::test]
async fn a_download_with_no_album_is_filed_under_the_group_its_finished_pick_chose() {
    let routes = vec![("/search?q=", r#"{"data":[]}"#.to_string())];
    let tagging = build_with(vec![], serve(routes).await, Options::default()).await;
    let path = landed_yesterday(tagging.root());

    for (pick, group, album) in [
        (None, "g-yesterday-and-today", "Yesterday and Today"),
        (Some("g-help"), "g-help", "Help!"),
    ] {
        let mut song = yesterday();
        let requested = RequestedIdentity::new(&song.artist, &song.title, "", None);
        if let Some(pick) = pick {
            requested
                .album_pick
                .set((pick.to_string(), "Help!".to_string()))
                .expect("not picked yet");
        }

        tagging
            .harness
            .service
            .identify(&mut song, &requested, &path, None)
            .await;

        let plan = song.tag_plan.as_deref().expect("a plan");
        let asked = &plan.evidence.as_ref().expect("the evidence").request;
        assert_eq!(asked.album_group_id.as_deref(), pick, "{pick:?}");
        let chosen = plan.chosen.as_ref().expect("a choice");
        assert_eq!(
            chosen.candidate.release_group_id.as_deref(),
            Some(group),
            "{pick:?}"
        );
        assert_eq!(song.album, album, "{pick:?}");
    }
}

#[tokio::test]
async fn a_busy_music_brainz_never_holds_up_tagging_and_its_pick_is_left_out() {
    let server = MockServer::builder().start().await;
    Mock::given(path_regex("^/ws/2/isrc/"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    Mock::given(path_regex("^/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .mount(&server)
        .await;
    let tagging = build_with(vec![], server, Options::default()).await;
    let path = landed_yesterday(tagging.root());
    let mut song = yesterday();
    let requested = RequestedIdentity::new(&song.artist, &song.title, "", None);

    tagging.harness.service.start_album_pick(&song, &requested);
    for _ in 0..400 {
        let sent = tagging._server.received_requests().await.unwrap_or_default();
        if sent
            .iter()
            .any(|request| request.url.path().starts_with("/ws/2/isrc/"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let sent = tagging._server.received_requests().await.unwrap_or_default();
    assert!(
        sent.iter()
            .any(|request| request.url.path() == "/ws/2/isrc/GBAYE0601477"),
        "the pick asked the code lookup"
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        tagging
            .harness
            .service
            .identify(&mut song, &requested, &path, None),
    )
    .await
    .expect("tagged without waiting for the pick");

    assert!(requested.album_pick.get().is_none());
    assert_eq!(song.album, "Yesterday and Today");
}
