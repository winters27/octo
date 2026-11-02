//! The catalog clip runner: which Deezer track, the clip fetch, the heard print and the reading.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use octo_core::fingerprint::heart_gate::{ClipReading, HeardRecord, HeardSource, WaitingFor};
use octo_core::settings::{AppSettings, SettingsStore};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use reqwest::Url;
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::soulseek_hold_store::SoulseekHoldStore;
use crate::services::metadata::deezer_metadata_service::DeezerMetadataService;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::prefetch::heard_samples::tests::{Rig, rig_in};
use crate::services::prefetch::heard_samples::{HeardPrints, ITEM_SECONDS, sample_id_of};
use crate::services::you_tube::YouTubeResolver;

const ISRC: &str = "GBAYE0601477";
const HEARD: &str = "AAAAAAAAAA1";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../octo-core/tests/fixtures/heart_album"
    ))
    .join(name)
}

fn items_in(path: &Path) -> Vec<u32> {
    crate::services::prefetch::heard_samples::tests::items_of(path)
}

struct Setup {
    clip: CatalogClip,
    rig: Rig,
    /// Deezer, the shim and the CDN.
    server: MockServer,
    _dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let rig = rig_in(
        dir.path(),
        &server.uri(),
        Arc::new(SoulseekHoldStore::new(None::<PathBuf>)),
        16,
        64 << 20,
    );
    let deezer = Arc::new(DeezerMetadataService::with_base_url(
        Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
        Arc::new(SettingsStore::for_tests(AppSettings::default())),
        server.uri(),
    ));
    let youtube = Arc::new(YouTubeResolver::with_base_url(Some(&server.uri())));
    let clip = CatalogClip::new(deezer, rig.samples.clone(), youtube);
    let cdn = Url::parse(&server.uri()).unwrap();
    clip.use_clip_hosts(Arc::new(move |url: &Url| {
        url.host_str() == cdn.host_str() && url.port() == cdn.port()
    }));
    Setup {
        clip,
        rig,
        server,
        _dir: dir,
    }
}

fn help() -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("Help!".into()),
        duration: Some(139),
        isrc: Some(ISRC.into()),
        ..Default::default()
    }
}

fn track(id: u64, title: &str, artist: &str, seconds: i32, preview: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!(
            r#"{{"id":{id},"title":"{title}","duration":{seconds},"isrc":"{ISRC}","preview":"{preview}","artist":{{"id":1,"name":"{artist}"}},"album":{{"id":9,"title":"Help!"}}}}"#
        ),
        "application/json",
    )
}

/// Deezer's ISRC lookup answers Help!, whose clip the CDN serves as `clip_file`'s bytes.
async fn help_on_deezer(server: &MockServer, clip_file: &Path) {
    help_on_deezer_at(server, clip_file, 139).await;
}

/// [`help_on_deezer`] with the track `seconds` long.
async fn help_on_deezer_at(server: &MockServer, clip_file: &Path, seconds: i32) {
    let preview = format!("{}/clip/help.mp3?hdnea=exp=1~hmac=x", server.uri());
    Mock::given(path(format!("/track/isrc:{ISRC}")))
        .respond_with(track(
            116,
            "Help! (Remastered 2009)",
            "The Beatles",
            seconds,
            &preview,
        ))
        .mount(server)
        .await;
    Mock::given(path("/clip/help.mp3"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(std::fs::read(clip_file).unwrap(), "audio/mpeg"),
        )
        .mount(server)
        .await;
}

/// The heard video's channel, by a fresh `/facts`.
async fn channel(server: &MockServer, name: &str) {
    Mock::given(path("/facts"))
        .and(query_param("id", HEARD))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"{{"fresh":true,"channel":"{name}"}}"#),
            "application/json",
        ))
        .mount(server)
        .await;
}

/// `user` heard the video, whose stored sample is `print_file`'s print.
fn heard_video(rig: &Rig, print_file: &Path) -> HeardRecord {
    let source = HeardSource::Video {
        id: HEARD.into(),
        size: Some(1000),
        content_type: "audio/mp4".into(),
        itag: None,
    };
    let id = sample_id_of(&source);
    rig.samples.store(
        &id,
        &HeardPrints {
            raw: items_in(print_file),
            compressed: "AQAD".into(),
            seconds: 240,
            whole: true,
            reaches_end: true,
        },
        Some(HEARD),
    );
    HeardRecord {
        heard_at: Utc::now(),
        source,
        sample_id: Some(id),
    }
}

async fn asked(server: &MockServer, prefix: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path().starts_with(prefix))
        .count()
}

#[test]
fn only_https_from_the_catalogs_clip_hosts_is_ever_fetched() {
    let allowed = |url: &str| clip_url_allowed(&Url::parse(url).unwrap());
    assert!(allowed(
        "https://cdnt-preview.dzcdn.net/api/1/1/a/b/c/0/abc.mp3?hdnea=x"
    ));
    assert!(allowed(
        "https://audio-ssl.itunes.apple.com/itunes-assets/AudioPreview125/v4/a.m4a"
    ));
    assert!(
        !allowed("http://cdnt-preview.dzcdn.net/api/1/1/a.mp3"),
        "never over http"
    );
    assert!(
        !allowed("http://audio-ssl.itunes.apple.com/a.m4a"),
        "never over http"
    );
    assert!(!allowed("https://dzcdn.net.evil.com/a.mp3"));
    assert!(!allowed("https://evil-dzcdn.net/a.mp3"));
    assert!(!allowed("https://example.com/a.mp3"));
    assert!(
        !allowed("https://is1-ssl.mzstatic.com/a.m4a"),
        "Apple's other hosts are not clip hosts"
    );
    assert!(!allowed("https://audio-ssl.itunes.apple.com.evil.com/a.m4a"));
}

#[tokio::test]
async fn the_songs_own_clip_on_the_artists_channel_reads_this_song_and_its_audio_is_gone() {
    let s = setup().await;
    help_on_deezer(&s.server, &fixture("clip/help-2009-clip.json")).await;
    channel(&s.server, "The Beatles - Topic").await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    let mut prints = ClipPrints::default();
    assert_eq!(
        s.clip.read(&help(), &heard, &mut prints).await,
        ClipReading::ThisSong
    );
    assert_eq!(prints.clip, Some(items_in(&fixture("clip/help-2009-clip.json"))));
    assert_eq!(prints.whole_heard, None, "the stored print covers the song");
    assert_eq!(
        s.rig.samples.scratch_use().0,
        0,
        "the clip's audio went once printed"
    );
    assert_eq!(
        std::fs::read_dir(s.rig.samples.scratch_folder()).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn the_channel_guard_reads_unclear_for_another_channel() {
    let s = setup().await;
    help_on_deezer(&s.server, &fixture("clip/help-2009-clip.json")).await;
    channel(&s.server, "Karaoke Hits").await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    assert_eq!(
        s.clip.read(&help(), &heard, &mut ClipPrints::default()).await,
        ClipReading::Unclear
    );
}

#[tokio::test]
async fn another_song_played_for_this_one_reads_another_song_or_take() {
    let s = setup().await;
    help_on_deezer(&s.server, &fixture("clip/help-2009-clip.json")).await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/mrsrobinson-studio-240.json"));
    assert_eq!(
        s.clip.read(&help(), &heard, &mut ClipPrints::default()).await,
        ClipReading::AnotherSongOrTake
    );
}

#[tokio::test]
async fn a_300_second_song_whose_clip_sits_at_250_seconds_takes_a_whole_print_and_reads_this_song() {
    let s = setup().await;
    let dir = tempfile::tempdir().unwrap();
    // The heard song, 300 s, kept as the person's file; its stored sample stops at 240 s.
    let whole = dir.path().join("song.raw");
    std::fs::write(
        &whole,
        crate::services::prefetch::heard_samples::tests::song_bytes(300.0, 11),
    )
    .unwrap();
    let items = items_in(&whole);
    let source = HeardSource::Library {
        path: whole.to_string_lossy().into_owned(),
    };
    let capped = (240.0 / ITEM_SECONDS) as usize;
    s.rig.samples.store(
        &sample_id_of(&source),
        &HeardPrints {
            raw: items[..capped].to_vec(),
            compressed: "AQAD".into(),
            seconds: 300,
            whole: true,
            reaches_end: false,
        },
        None,
    );
    // The catalog's clip: 30 s from 250 s in.
    let from = (250.0 / ITEM_SECONDS) as usize;
    let clip_items = &items[from..from + (30.0 / ITEM_SECONDS) as usize];
    let clip_file = dir.path().join("clip.raw");
    std::fs::write(
        &clip_file,
        clip_items
            .iter()
            .flat_map(|i| i.to_le_bytes())
            .collect::<Vec<u8>>(),
    )
    .unwrap();
    help_on_deezer_at(&s.server, &clip_file, 300).await;
    let routing = SoulseekRouting {
        duration: Some(300),
        ..help()
    };
    let heard = HeardRecord {
        heard_at: Utc::now(),
        source,
        sample_id: None,
    };
    let mut prints = ClipPrints::default();
    assert_eq!(
        s.clip.read(&routing, &heard, &mut prints).await,
        ClipReading::ThisSong
    );
    assert_eq!(prints.whole_heard.as_deref(), Some(items.as_slice()));
}

#[tokio::test]
async fn a_deezer_429_then_a_200_gives_a_reading_never_no_clip() {
    let s = setup().await;
    Mock::given(path(format!("/track/isrc:{ISRC}")))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&s.server)
        .await;
    help_on_deezer(&s.server, &fixture("clip/help-2009-clip.json")).await;
    channel(&s.server, "The Beatles").await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    let mut prints = ClipPrints::default();
    assert_eq!(
        s.clip.read(&help(), &heard, &mut prints).await,
        ClipReading::Busy(WaitingFor::Deezer)
    );
    assert_eq!(
        s.clip.read(&help(), &heard, &mut prints).await,
        ClipReading::ThisSong
    );
}

#[tokio::test]
async fn an_isrc_whose_deezer_track_is_another_song_is_skipped_for_the_title_searchs_track() {
    let s = setup().await;
    let preview = |name: &str| format!("{}/clip/{name}.mp3", s.server.uri());
    Mock::given(path(format!("/track/isrc:{ISRC}")))
        .respond_with(track(7, "Yesterday", "The Beatles", 125, &preview("yesterday")))
        .mount(&s.server)
        .await;
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"data":[{"id":5,"title":"Help!","duration":139,"artist":{"id":1,"name":"The Beatles"},"album":{"id":9,"title":"Help!"}}]}"#,
            "application/json",
        ))
        .mount(&s.server)
        .await;
    Mock::given(path("/track/5"))
        .respond_with(track(5, "Help!", "The Beatles", 139, &preview("help")))
        .mount(&s.server)
        .await;
    Mock::given(path("/clip/help.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            std::fs::read(fixture("clip/help-2009-clip.json")).unwrap(),
            "audio/mpeg",
        ))
        .mount(&s.server)
        .await;
    channel(&s.server, "The Beatles - Topic").await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    assert_eq!(
        s.clip.read(&help(), &heard, &mut ClipPrints::default()).await,
        ClipReading::ThisSong
    );
    assert_eq!(
        asked(&s.server, "/clip/yesterday").await,
        0,
        "another song's clip is never fetched"
    );
    assert_eq!(asked(&s.server, "/clip/help").await, 1);
}

#[tokio::test]
async fn a_matching_track_with_no_clip_is_passed_over_for_the_next_match() {
    let s = setup().await;
    let preview = format!("{}/clip/help.mp3", s.server.uri());
    // Where Deezer does not serve the server's country, a song's own record names no clip.
    Mock::given(path(format!("/track/isrc:{ISRC}")))
        .respond_with(track(116, "Help! (Remastered 2009)", "The Beatles", 139, ""))
        .mount(&s.server)
        .await;
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"data":[{"id":5,"title":"Help!","duration":139,"artist":{"id":1,"name":"The Beatles"},"album":{"id":9,"title":"Help!"}}]}"#,
            "application/json",
        ))
        .mount(&s.server)
        .await;
    Mock::given(path("/track/5"))
        .respond_with(track(5, "Help!", "The Beatles", 139, &preview))
        .mount(&s.server)
        .await;
    Mock::given(path("/clip/help.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            std::fs::read(fixture("clip/help-2009-clip.json")).unwrap(),
            "audio/mpeg",
        ))
        .mount(&s.server)
        .await;
    channel(&s.server, "The Beatles - Topic").await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    assert_eq!(
        s.clip.read(&help(), &heard, &mut ClipPrints::default()).await,
        ClipReading::ThisSong
    );
    assert_eq!(asked(&s.server, "/track/5").await, 1, "the next match was asked");
}

/// A song with no ISRC on a server Deezer does not serve: the title search is answered from
/// Apple's catalog, and Apple's clip of the song is the one read.
#[tokio::test]
async fn where_deezer_hides_its_catalog_apples_clip_is_read() {
    use crate::services::apple::{AppleCatalog, ApplePace, PaceConfig};
    use crate::services::metadata::{DeezerReach, DeezerStandIn, DoorInfos};

    let s = setup().await;
    let apple_server = MockServer::start().await;
    let pace = Arc::new(ApplePace::new(PaceConfig::unpaced()));
    let reach = Arc::new(DeezerReach::new(Arc::clone(&pace)));
    let apple = Arc::new(AppleCatalog::with_bases(
        pace,
        &apple_server.uri(),
        &apple_server.uri(),
        None,
    ));
    let stand_in = Arc::new(DeezerStandIn::new(Arc::clone(&reach), apple));
    let door = Arc::new(DeezerRateLimitHandler::with_api_host(
        Arc::new(DeezerRateLimiter::new()),
        "127.0.0.1",
    ));
    stand_in.set_door(Arc::downgrade(&door));
    door.set_stand_in(Arc::clone(&stand_in));
    reach.set_reader(Arc::new(DoorInfos::new(Arc::downgrade(&door), &s.server.uri())));
    let deezer = Arc::new(DeezerMetadataService::with_base_url(
        Arc::clone(&door),
        Arc::new(SettingsStore::for_tests(AppSettings::default())),
        s.server.uri(),
    ));
    let clip = CatalogClip::new(
        deezer,
        s.rig.samples.clone(),
        Arc::new(YouTubeResolver::with_base_url(Some(&s.server.uri()))),
    );
    let cdn = Url::parse(&s.server.uri()).unwrap();
    clip.use_clip_hosts(Arc::new(move |url: &Url| {
        url.host_str() == cdn.host_str() && url.port() == cdn.port()
    }));

    Mock::given(path("/infos"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"country_iso":"IN","country":"India","open":false}"#,
            "application/json",
        ))
        .mount(&s.server)
        .await;
    reach.read_infos().await;
    let song = format!(
        r#"{{"wrapperType":"track","kind":"song","trackId":1440833098,"trackName":"Help!","artistName":"The Beatles","artistId":136975,"collectionId":1441164426,"collectionName":"Help!","trackTimeMillis":139000,"trackNumber":1,"discNumber":1,"trackExplicitness":"notExplicit","previewUrl":"{}/clip/apple-help.mp3","artworkUrl100":"https://is1-ssl.mzstatic.com/image/thumb/a/100x100bb.jpg"}}"#,
        s.server.uri()
    );
    let answer = format!(r#"{{"resultCount":1,"results":[{song}]}}"#);
    for apple_path in ["/search", "/lookup"] {
        Mock::given(path(apple_path))
            .respond_with(ResponseTemplate::new(200).set_body_raw(answer.clone(), "text/javascript"))
            .mount(&apple_server)
            .await;
    }
    Mock::given(path("/clip/apple-help.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            std::fs::read(fixture("clip/help-2009-clip.json")).unwrap(),
            "audio/mpeg",
        ))
        .mount(&s.server)
        .await;
    channel(&s.server, "The Beatles - Topic").await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    let routing = SoulseekRouting { isrc: None, ..help() };
    assert_eq!(
        clip.read(&routing, &heard, &mut ClipPrints::default()).await,
        ClipReading::ThisSong
    );
    assert_eq!(asked(&s.server, "/clip/apple-help.mp3").await, 1, "Apple's clip");
}

#[tokio::test]
async fn a_clip_redirected_off_the_cdn_is_never_fetched() {
    let s = setup().await;
    let elsewhere = MockServer::start().await;
    let preview = format!("{}/clip/help.mp3", s.server.uri());
    Mock::given(path(format!("/track/isrc:{ISRC}")))
        .respond_with(track(116, "Help!", "The Beatles", 139, &preview))
        .mount(&s.server)
        .await;
    Mock::given(path("/clip/help.mp3"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", format!("{}/clip.mp3", elsewhere.uri())),
        )
        .mount(&s.server)
        .await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    assert_eq!(
        s.clip.read(&help(), &heard, &mut ClipPrints::default()).await,
        ClipReading::NoClip
    );
    assert!(elsewhere.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn a_clip_over_2_mb_is_no_clip_and_leaves_nothing() {
    let s = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.raw");
    std::fs::write(&big, vec![1u8; (MAX_CLIP_BYTES + 1) as usize]).unwrap();
    help_on_deezer(&s.server, &big).await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    assert_eq!(
        s.clip.read(&help(), &heard, &mut ClipPrints::default()).await,
        ClipReading::NoClip
    );
    assert_eq!(
        std::fs::read_dir(s.rig.samples.scratch_folder()).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn a_held_records_saved_prints_are_reused() {
    let s = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("song.raw");
    std::fs::write(&file, b"x").unwrap();
    let heard = HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Library {
            path: file.to_string_lossy().into_owned(),
        },
        sample_id: None,
    };
    // A check before the hold took both prints, and the hold saved them.
    let holds = SoulseekHoldStore::new(None::<PathBuf>);
    let record = holds.hold(crate::services::common::soulseek_hold_store::HeldAcquisition {
        provider: "soulseek".into(),
        external_id: "help".into(),
        requested_by: Some("anna".into()),
        held_since_utc: Utc::now(),
        waiting_for: Some(WaitingFor::AcoustIdOff),
        ..Default::default()
    });
    ClipPrints {
        clip: Some(items_in(&fixture("clip/help-2009-clip.json"))),
        whole_heard: Some(items_in(&fixture("gate/prints/help-2009-240.json"))),
    }
    .save_to(&holds, &record.key(), "anna");
    let saved = holds.get(&record.key()).unwrap();
    assert!(saved.clip_print.is_some());
    assert!(saved.holders[0].whole_heard_print.is_some());

    // The hold resumes: the saved prints are read back, and nothing is fetched twice.
    let mut prints = ClipPrints::of_hold(&saved, "Anna");
    assert_eq!(
        s.clip.read(&help(), &heard, &mut prints).await,
        ClipReading::ThisSong
    );
    assert!(
        s.server.received_requests().await.unwrap_or_default().is_empty(),
        "nothing fetched twice"
    );
}

/// A shim that does not answer the channel question as asked (one older than Octo, which answers
/// from its cache) holds for the shim and its round, never Keep anyway; only a video that is gone
/// reads unclear.
#[tokio::test]
async fn an_old_shim_holds_for_the_shim_and_a_gone_video_reads_unclear() {
    let s = setup().await;
    help_on_deezer(&s.server, &fixture("clip/help-2009-clip.json")).await;
    Mock::given(path("/facts"))
        .and(query_param("id", HEARD))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"channel":"The Beatles - Topic"}"#, "application/json"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&s.server)
        .await;
    Mock::given(path("/facts"))
        .and(query_param("id", HEARD))
        .respond_with(ResponseTemplate::new(410).set_body_raw(
            r#"{"error":"video_unavailable","fresh":true}"#,
            "application/json",
        ))
        .mount(&s.server)
        .await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    let mut prints = ClipPrints::default();
    assert_eq!(
        s.clip.read(&help(), &heard, &mut prints).await,
        ClipReading::Busy(WaitingFor::Shim)
    );
    assert_eq!(
        s.clip.read(&help(), &heard, &mut prints).await,
        ClipReading::Unclear
    );
}

/// A clip fpcalc could not print says nothing of the song: it reads as unprinted, so the heart
/// holds and runs the check again later, never reading "no clip" (which holds with Keep anyway for
/// good) nor waiting for a Deezer that answered.
#[tokio::test]
async fn a_clip_fpcalc_could_not_print_holds_rather_than_reading_no_clip() {
    let s = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let unprintable = dir.path().join("clip.raw");
    std::fs::write(&unprintable, b"ab").unwrap();
    help_on_deezer(&s.server, &unprintable).await;
    let heard = heard_video(&s.rig, &fixture("gate/prints/help-2009-240.json"));
    let mut prints = ClipPrints::default();
    assert_eq!(
        s.clip.read(&help(), &heard, &mut prints).await,
        ClipReading::Unprinted
    );
    assert_eq!(prints.clip, None, "nothing is kept of a clip never printed");
}

/// `brandon` heard the copy of Help! fetched ahead, made from `video` when it came from YouTube;
/// its stored sample is the 2009 print.
fn heard_copy(s: &Setup, video: Option<&str>) -> HeardRecord {
    use crate::services::prefetch::{AheadSidecar, file_name, prepare_folder, write_sidecar};
    let folder = s._dir.path().join("prefetch");
    prepare_folder(&folder).unwrap();
    let file = folder.join(file_name("soulseek", "help", "m4a"));
    std::fs::write(&file, b"x").unwrap();
    let ready_at = Utc::now();
    write_sidecar(
        &file,
        &AheadSidecar {
            provider: "soulseek".into(),
            external_id: "help".into(),
            ready_at: Some(ready_at),
            video_id: video.map(str::to_string),
            ..Default::default()
        },
    )
    .unwrap();
    let source = HeardSource::Copy {
        key: "soulseek:help".into(),
        ready_at: Some(ready_at),
        source_file: None,
    };
    let id = sample_id_of(&source);
    s.rig.samples.store(
        &id,
        &HeardPrints {
            raw: items_in(&fixture("gate/prints/help-2009-240.json")),
            compressed: "AQAD".into(),
            seconds: 240,
            whole: true,
            reaches_end: true,
        },
        None,
    );
    HeardRecord {
        heard_at: Utc::now(),
        source,
        sample_id: Some(id),
    }
}

/// A copy fetched ahead from a peer has no channel to vouch for it (a peer's upload can be a cover
/// that sounds like the song): it reads unclear. One made from a YouTube video is read by that
/// video's channel, as the video itself would be.
#[tokio::test]
async fn a_copy_fetched_ahead_passes_only_by_the_channel_of_its_own_video() {
    let peer = setup().await;
    help_on_deezer(&peer.server, &fixture("clip/help-2009-clip.json")).await;
    channel(&peer.server, "The Beatles - Topic").await;
    let from_a_peer = heard_copy(&peer, None);
    assert_eq!(
        peer.clip
            .read(&help(), &from_a_peer, &mut ClipPrints::default())
            .await,
        ClipReading::Unclear
    );

    let youtube = setup().await;
    help_on_deezer(&youtube.server, &fixture("clip/help-2009-clip.json")).await;
    channel(&youtube.server, "The Beatles - Topic").await;
    let from_youtube = heard_copy(&youtube, Some(HEARD));
    assert_eq!(
        youtube
            .clip
            .read(&help(), &from_youtube, &mut ClipPrints::default())
            .await,
        ClipReading::ThisSong
    );
    assert_eq!(
        asked(&youtube.server, "/facts").await,
        1,
        "its video's channel was read"
    );
}
