//! `stream` with songs downloaded ahead: Octo's apps get the ready FLAC for a play that starts on
//! it, other apps only with Wait for lossless on play, a play keeps the source it started on, and
//! a caller whose audio has a ceiling gets the preview.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use http_body_util::BodyExt;
use octo_core::family::abilities::{Abilities, Preset};
use octo_core::family::quality::StreamQuality;
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, PrefetchSettings};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer};

use super::media::stream;
use super::test_app::*;
use crate::app::AppState;
use crate::services::family::FamilyCaller;
use crate::services::prefetch::{AheadSidecar, file_name, write_sidecar};

/// The FLAC downloaded ahead, as its bytes.
const FLAC: &[u8] = b"fLaC-the-whole-song";
/// The preview the outside source streams.
const PREVIEW: &[u8] = &[1, 2, 3];

struct World {
    state: AppState,
    _navidrome: MockServer,
    folder: tempfile::TempDir,
    streams: Arc<FakeStreams>,
}

async fn world(change: impl FnOnce(&mut AppSettings)) -> World {
    let navidrome = MockServer::start().await;
    Mock::given(any())
        .respond_with(navidrome_json(NAVIDROME_OK))
        .mount(&navidrome)
        .await;
    let folder = tempfile::tempdir().expect("a temp dir");
    let mut settings = settings(&navidrome.uri());
    settings.prefetch = PrefetchSettings {
        enabled: true,
        folder: folder.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    settings.soulseek.base_url = Some(format!(
        "http://{}",
        crate::services::test_support::refused_address()
    ));
    change(&mut settings);
    let streams = Arc::new(FakeStreams::streaming("soulseek", "song1", PREVIEW, "audio/mp4"));
    let library = Arc::new(FakeLibrary::with_external("outside-1", "soulseek", "song1"));
    let state = state_with(settings, |inner| {
        inner.download_service = streams.clone();
        inner.local_library = library.clone();
    });
    World {
        state,
        _navidrome: navidrome,
        folder,
        streams,
    }
}

impl World {
    /// The song is ready in the folder now.
    fn land(&self) -> PathBuf {
        let path = self.folder.path().join(file_name("soulseek", "song1", "flac"));
        std::fs::write(&path, FLAC).expect("the file");
        let song = Song {
            artist: "Artist".into(),
            title: "Song".into(),
            ..Default::default()
        };
        write_sidecar(
            &path,
            &AheadSidecar::of("soulseek", "song1", &path, &song, None, chrono::Utc::now()),
        )
        .expect("the facts");
        self.state.prefetch.store().adopt(&path);
        path
    }

    async fn play(&self, client: &str, range: Option<&str>) -> Reply {
        let app = app(&self.state);
        let uri = format!("/rest/stream?id=outside-1&{}", auth("alice", "good", client));
        let headers: Vec<(&str, &str)> = range.map(|r| ("Range", r)).into_iter().collect();
        send(&app, Method::GET, &uri, &headers, Body::empty()).await
    }
}

fn is_flac(reply: &Reply) -> bool {
    FLAC.ends_with(&reply.body)
        && !reply.body.is_empty()
        && reply.header("content-type") == Some("audio/flac")
}

fn is_preview(reply: &Reply) -> bool {
    reply.body.as_ref() == PREVIEW
}

/// The world's song filed in the registry under its outside id, with a person's choice of video.
fn chosen(w: &World, video: &str) {
    let registry = &w.state.external_id_registry;
    registry.restore_as(
        "song1",
        octo_core::soulseek::SoulseekRouting {
            kind: octo_core::soulseek::RoutingKind::Song,
            artist: Some("Artist".into()),
            title: Some("Song".into()),
            you_tube_id: Some("oldvideo-1".into()),
            ..Default::default()
        },
    );
    registry
        .choose_stream_video(
            "song1",
            crate::services::soulseek::StreamChoice {
                video: video.into(),
                by: "alice".into(),
                at: chrono::Utc::now(),
                ..Default::default()
            },
        )
        .expect("stored");
}

/// The song ready in the folder, its identity checked against `judged`.
fn land_judged(w: &World, judged: &str) {
    let path = w.folder.path().join(file_name("soulseek", "song1", "flac"));
    std::fs::write(&path, FLAC).expect("the file");
    let song = Song {
        artist: "Artist".into(),
        title: "Song".into(),
        ..Default::default()
    };
    write_sidecar(
        &path,
        &AheadSidecar {
            judged_video: Some(judged.to_string()),
            ..AheadSidecar::of("soulseek", "song1", &path, &song, None, chrono::Utc::now())
        },
    )
    .expect("the facts");
    w.state.prefetch.store().adopt(&path);
}

#[tokio::test]
async fn a_copy_checked_against_another_version_than_the_chosen_one_is_not_served() {
    let w = world(|_| {}).await;
    chosen(&w, "newvideo-1");
    w.land();
    let reply = w.play("Octo", None).await;
    assert!(is_preview(&reply), "{:?}", reply.body);

    let w = world(|_| {}).await;
    chosen(&w, "newvideo-1");
    land_judged(&w, "oldvideo-1");
    assert!(is_preview(&w.play("Octo", None).await));

    let w = world(|_| {}).await;
    chosen(&w, "newvideo-1");
    land_judged(&w, "newvideo-1");
    assert!(
        is_flac(&w.play("Octo", None).await),
        "a copy of the chosen version is served"
    );
}

#[tokio::test]
async fn an_octo_app_gets_the_flac_of_a_song_downloaded_ahead() {
    let w = world(|_| {}).await;
    w.land();
    let reply = w.play("Octo", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(is_flac(&reply), "{:?}", reply.body);
    assert_eq!(reply.body.as_ref(), FLAC);
    assert!(
        w.streams.stream_calls().is_empty(),
        "the preview was never asked for"
    );
}

#[tokio::test]
async fn another_app_gets_the_preview_unless_wait_for_lossless_is_on() {
    let w = world(|_| {}).await;
    w.land();
    let reply = w.play("Symfonium", None).await;
    assert!(is_preview(&reply), "{:?}", reply.body);

    let w = world(|s| s.subsonic.wait_for_lossless_on_play = true).await;
    w.land();
    let reply = w.play("Symfonium", None).await;
    assert_eq!(reply.body.as_ref(), FLAC);
}

#[tokio::test]
async fn prefetch_off_leaves_an_octo_app_on_the_preview() {
    let w = world(|s| s.prefetch.enabled = false).await;
    w.land();
    let reply = w.play("Octo", None).await;
    assert!(is_preview(&reply), "{:?}", reply.body);
}

#[tokio::test]
async fn a_seek_after_the_file_lands_stays_on_the_preview() {
    let w = world(|_| {}).await;
    let started = w.play("Octo", None).await;
    assert!(is_preview(&started));
    w.land();
    let seek = w.play("Octo", Some("bytes=2-")).await;
    assert!(is_preview(&seek), "{:?}", seek.body);
    assert_eq!(
        w.streams
            .stream_calls()
            .last()
            .and_then(|c| c.2.clone())
            .as_deref(),
        Some("bytes=2-")
    );
}

#[tokio::test]
async fn a_later_range_with_no_play_started_on_the_flac_gets_the_preview() {
    let w = world(|_| {}).await;
    w.land();
    let reply = w.play("Octo", Some("bytes=500000-")).await;
    assert!(is_preview(&reply), "{:?}", reply.body);
}

#[tokio::test]
async fn a_play_that_started_on_the_flac_keeps_it_for_its_later_ranges() {
    let w = world(|_| {}).await;
    w.land();
    let first = w.play("Octo", None).await;
    assert!(is_flac(&first));
    let later = w.play("Octo", Some("bytes=5-")).await;
    assert_eq!(later.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(later.body.as_ref(), &FLAC[5..]);
    assert!(w.streams.stream_calls().is_empty());
}

#[tokio::test]
async fn a_caller_whose_audio_has_a_ceiling_gets_the_preview() {
    let members = tempfile::tempdir().expect("a temp dir");
    let path = members.path().to_string_lossy().into_owned();
    let w = world(move |s| {
        s.family.enabled = true;
        s.family.members_path = path;
    })
    .await;
    w.land();
    let mut caller = FamilyCaller::member("alex", Preset::Member, Abilities::preset(Preset::Member));
    caller.quality.home = StreamQuality::Standard;
    caller.quality.away = StreamQuality::Standard;
    let mut request = Request::builder()
        .uri(format!(
            "/rest/stream?id=outside-1&{}",
            auth("alex", "good", "Octo")
        ))
        .body(Body::empty())
        .expect("a request");
    request.extensions_mut().insert(caller);
    let response = stream(State(w.state.clone()), request).await.expect("answered");
    let body = response.into_body().collect().await.expect("read").to_bytes();
    assert_eq!(body.as_ref(), PREVIEW);
    assert!(Path::new(&w.folder.path().join(file_name("soulseek", "song1", "flac"))).is_file());
}

#[tokio::test]
async fn with_wait_for_lossless_a_seek_after_the_file_lands_stays_on_the_preview() {
    let w = world(|s| s.subsonic.wait_for_lossless_on_play = true).await;
    let started = w.play("Octo", None).await;
    assert!(is_preview(&started), "{:?}", started.body);
    w.land();
    let seek = w.play("Octo", Some("bytes=2-")).await;
    assert!(is_preview(&seek), "{:?}", seek.body);
}

#[tokio::test]
async fn with_wait_for_lossless_a_later_range_with_no_play_started_on_the_flac_gets_the_preview() {
    let w = world(|s| s.subsonic.wait_for_lossless_on_play = true).await;
    w.land();
    let reply = w.play("Octo", Some("bytes=500000-")).await;
    assert!(is_preview(&reply), "{:?}", reply.body);
}

#[tokio::test]
async fn a_play_whose_file_is_gone_is_asked_to_start_again() {
    let w = world(|_| {}).await;
    let path = w.land();
    assert!(is_flac(&w.play("Octo", None).await));
    std::fs::remove_file(&path).expect("swept");
    let later = w.play("Octo", Some("bytes=5-")).await;
    assert_eq!(later.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!is_preview(&later), "never the preview's bytes after FLAC ones");
}

#[tokio::test]
async fn two_devices_of_one_listener_have_their_own_plays() {
    let w = world(|_| {}).await;
    w.land();
    let app = app(&w.state);
    let uri = format!("/rest/stream?id=outside-1&{}", auth("alice", "good", "Octo"));
    let started = send(&app, Method::GET, &uri, &[], Body::empty()).await;
    assert!(is_flac(&started));
    // The same listener's other app has started nothing: its later range is the preview's.
    let other = format!(
        "/rest/stream?id=outside-1&{}",
        auth("alice", "good", "OctoDesktop")
    );
    let reply = send(&app, Method::GET, &other, &[("Range", "bytes=5-")], Body::empty()).await;
    assert!(is_preview(&reply), "{:?}", reply.body);
}

/// A transfer that never runs: a song kept from the folder needs none.
#[derive(Default)]
struct NoTransfer {
    transfers: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl crate::services::common::DownloadBackend for NoTransfer {
    fn provider_name(&self) -> &str {
        "soulseek"
    }

    async fn is_available(&self, _: &crate::services::common::BaseDownloadService) -> bool {
        true
    }

    async fn download_track(
        &self,
        _: &crate::services::common::BaseDownloadService,
        _: &mut crate::services::common::TrackDownload,
        _: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<String> {
        self.transfers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("no transfer in this test")
    }

    fn extract_external_id_from_album_id(&self, _: &str) -> Option<String> {
        None
    }
}

/// The download pipeline over the state's songs downloaded ahead, filing into its music folder.
fn pipeline(
    w: &World,
) -> (
    Arc<crate::services::common::BaseDownloadService>,
    Arc<NoTransfer>,
    PathBuf,
) {
    use crate::services::common::{BaseDownloadService, DownloadCore, DownloadServices};
    let settings = w.state.settings.clone();
    let music = PathBuf::from(settings.raw("Library:DownloadPath").expect("a music folder"));
    let metadata = crate::services::common::test_fakes::FakeMetadata::default();
    metadata.songs.lock().insert(
        ("soulseek".into(), "song1".into()),
        Song {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            external_provider: Some("soulseek".into()),
            external_id: Some("song1".into()),
            ..Default::default()
        },
    );
    let backend = Arc::new(NoTransfer::default());
    let service = BaseDownloadService::new(
        DownloadCore {
            settings: settings.clone(),
            local_library: Arc::new(crate::services::local::test_support::FakeLocalLibrary::default()),
            metadata: Arc::new(metadata),
            navidrome_identity: crate::services::subsonic::NavidromeIdentityService::new(
                settings.clone(),
                reqwest::Client::new(),
            ),
            history: w.state.download_history.clone(),
            notifications: w.state.notifications.clone(),
        },
        DownloadServices {
            prefetched: Some(
                w.state.prefetch.store().clone() as Arc<dyn crate::services::prefetch::PrefetchedFiles>
            ),
            ..Default::default()
        },
        backend.clone(),
    );
    (service, backend, music)
}

#[tokio::test]
async fn a_heart_during_a_play_on_the_flac_keeps_that_play_s_later_ranges_on_it() {
    let w = world(|_| {}).await;
    w.land();
    assert!(is_flac(&w.play("Octo", None).await));
    let (pipeline, backend, music) = pipeline(&w);
    let kept = crate::services::i_download_service::IDownloadService::execute_acquisition(
        pipeline.as_ref(),
        "soulseek",
        "song1",
        false,
        true,
        None,
        &tokio_util::sync::CancellationToken::new(),
        Some(vec!["alice".into()]),
        false,
        None,
    )
    .await
    .expect("kept");
    assert!(Path::new(&kept).starts_with(&music), "{kept}");
    assert_eq!(backend.transfers.load(std::sync::atomic::Ordering::SeqCst), 0);
    let later = w.play("Octo", Some("bytes=5-")).await;
    assert_eq!(later.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(later.body.as_ref(), &FLAC[5..]);
}

#[tokio::test]
async fn with_wait_for_lossless_a_ready_song_served_is_kept_in_the_library_by_its_play() {
    let w = world(|s| s.subsonic.wait_for_lossless_on_play = true).await;
    w.land();
    assert!(is_flac(&w.play("Octo", None).await));
    // The play queued its own lossless copy, as a play with Wait for lossless on does.
    let queue = &w.state.track_acquisition_queue;
    assert!(queue.is_in_flight("soulseek", "song1"));
    let request = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        queue.dequeue(&tokio_util::sync::CancellationToken::new()),
    )
    .await
    .expect("in time")
    .expect("the play's acquisition");
    let (pipeline, backend, music) = pipeline(&w);
    let kept = crate::services::i_download_service::IDownloadService::execute_acquisition(
        pipeline.as_ref(),
        &request.provider,
        &request.external_id,
        false,
        true,
        None,
        &tokio_util::sync::CancellationToken::new(),
        None,
        false,
        None,
    )
    .await
    .expect("kept");
    assert!(Path::new(&kept).starts_with(&music), "{kept}");
    assert!(Path::new(&kept).is_file());
    assert_eq!(backend.transfers.load(std::sync::atomic::Ordering::SeqCst), 0);
    // And the play goes on with the file it started on.
    let later = w.play("Octo", Some("bytes=5-")).await;
    assert_eq!(later.body.as_ref(), &FLAC[5..]);
}
