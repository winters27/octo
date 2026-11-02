//! The prefetch service over a handler-test state: what a now-playing queues, who it never
//! queues for, what DownloadOnPlay does instead, how many run beside a heart, and the worker's
//! download into the folder.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use octo_core::common::Clock;
use octo_core::family::abilities::{Abilities, Preset};
use octo_core::models::domain::Song;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::{AppSettings, DownloadSource, PrefetchScope, PrefetchSettings};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::app::{AppState, TestServices};
use crate::services::common::DownloadConcurrency;
use crate::services::common::test_fakes::{LoggedInLink, OffLink, OfflineLink, until};
use crate::services::family::FamilyCaller;
use crate::services::family::test_support::member;
use crate::services::i_download_service::{Ahead, AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::ReplacementHandoff;
use crate::services::soulseek::ISoulseekLink;
use crate::services::spotiflac::SpotiflacClient;

/// A download service whose downloads ahead land a small file and its facts in the folder: at
/// once, or once the test opens the gate; or that fail as told.
#[derive(Default)]
struct AheadDownloads {
    asked: Mutex<Vec<String>>,
    gated: Mutex<bool>,
    open: Notify,
    now: AtomicUsize,
    most: AtomicUsize,
    refuse: Mutex<Option<std::io::ErrorKind>>,
    /// Every download fails because Mornye's check is paused.
    mornye_waits: Mutex<bool>,
    /// That pause names no check, as one read from Mornye's words can.
    mornye_names_no_check: Mutex<bool>,
}

#[async_trait]
impl IDownloadService for AheadDownloads {
    async fn download_song(&self, _: &str, _: &str, _: &CancellationToken) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_and_stream(
        &self,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        anyhow::bail!("not set up")
    }

    fn download_remaining_album_tracks_in_background(&self, _: &str, _: &str, _: &str) {}

    async fn execute_acquisition(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: bool,
        _: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        _: bool,
        _: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_album_with_source(
        &self,
        _: &str,
        _: &str,
        _: DownloadSource,
        _: bool,
        _: &CancellationToken,
        _: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn download_ahead(
        &self,
        provider: &str,
        external_id: &str,
        folder: &Path,
        _: &CancellationToken,
    ) -> anyhow::Result<Ahead> {
        self.asked.lock().push(external_id.to_string());
        let refused = *self.refuse.lock();
        if let Some(kind) = refused {
            return Err(std::io::Error::new(kind, "the source did not answer").into());
        }
        if *self.mornye_waits.lock() {
            let pause = octo_core::spotiflac::check::CheckPause {
                until_utc: Utc::now() + Duration::minutes(15),
                labels: if *self.mornye_names_no_check.lock() {
                    Vec::new()
                } else {
                    vec!["Example".into()]
                },
                holding: Vec::new(),
                url: String::new(),
            };
            return Err(crate::services::spotiflac::MornyeWaiting::new(pause, true, Utc::now()).into_error());
        }
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        let gated = *self.gated.lock();
        if gated {
            self.open.notified().await;
        }
        self.now.fetch_sub(1, Ordering::SeqCst);
        assert!(prepare_folder(folder)?);
        let path = folder.join(file_name(provider, external_id, "flac"));
        std::fs::write(&path, b"fLaC")?;
        let song = Song {
            artist: "Artist".into(),
            title: external_id.into(),
            ..Default::default()
        };
        write_sidecar(
            &path,
            &AheadSidecar::of(provider, external_id, &path, &song, None, Utc::now()),
        )?;
        Ok(Ahead::Kept { path })
    }

    fn get_download_status(&self, _: &str) -> Option<DownloadInfo> {
        None
    }

    async fn get_local_path_if_exists(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn get_direct_stream(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        Ok(None)
    }
}

struct World {
    state: AppState,
    service: Arc<PrefetchService>,
    downloads: Arc<AheadDownloads>,
    folder: tempfile::TempDir,
    ids: Vec<String>,
    now: Arc<Mutex<DateTime<Utc>>>,
}

fn world(change: impl FnOnce(&mut AppSettings)) -> World {
    world_with(change, None)
}

fn world_with(change: impl FnOnce(&mut AppSettings), soulseek: Option<Arc<dyn ISoulseekLink>>) -> World {
    world_full(change, soulseek, None)
}

/// The service over the state's own parts, with downloads three at a time, this Soulseek link
/// and this Mornye.
fn world_full(
    change: impl FnOnce(&mut AppSettings),
    soulseek: Option<Arc<dyn ISoulseekLink>>,
    spotiflac: Option<Arc<SpotiflacClient>>,
) -> World {
    let folder = tempfile::tempdir().expect("a temp dir");
    let mut settings = AppSettings {
        prefetch: PrefetchSettings {
            enabled: true,
            scope: PrefetchScope::All,
            at_once: 3,
            min_free_gb: 1,
            folder: folder.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    settings.soulseek.parallel_downloads = 3;
    change(&mut settings);
    let downloads = Arc::new(AheadDownloads::default());
    let state = AppState::for_tests_with(
        settings,
        TestServices {
            download_service: Some(downloads.clone() as Arc<dyn IDownloadService>),
            ..Default::default()
        },
    );
    let ids: Vec<String> = ["One", "Two", "Three", "Four", "Five", "Six", "Seven"]
        .iter()
        .map(|title| {
            state.external_id_registry.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some("Artist".into()),
                title: Some(title.to_string()),
                duration: Some(200),
                ..Default::default()
            })
        })
        .collect();
    let concurrency = Arc::new(DownloadConcurrency::new(state.settings.clone()));
    concurrency.prove();
    let now = Arc::new(Mutex::new(Utc::now()));
    let clock_now = Arc::clone(&now);
    let clock = Clock::new(move || *clock_now.lock());
    // The same folder as the state reads, on the test clock.
    let in_folder = folder.path().to_path_buf();
    let store = Arc::new(PrefetchStore::new(
        Arc::new(move || in_folder.clone()),
        clock.clone(),
    ));
    let service = PrefetchService::new(PrefetchParts {
        settings: state.settings.clone(),
        store,
        downloads: downloads.clone(),
        queue: state.track_acquisition_queue.clone(),
        coordinator: state.heart_acquisition_coordinator.clone(),
        radio_queues: state.radio_queues.clone(),
        write_gate: state.family_write_gate.clone(),
        library: state.local_library.clone(),
        concurrency: Some(concurrency),
        names: Some(state.external_id_registry.clone()),
        soulseek,
        spotiflac,
        clock,
    });
    World {
        state,
        service,
        downloads,
        folder,
        ids,
        now,
    }
}

impl World {
    fn save_queue(&self, user: &str, current: usize) {
        self.service.saved_queue(
            user,
            self.ids.clone(),
            Some(self.ids[current].clone()),
            Some(current),
        );
    }

    fn asked(&self) -> Vec<String> {
        let mut asked = self.downloads.asked.lock().clone();
        asked.sort();
        asked
    }

    fn ids_of(&self, at: &[usize]) -> Vec<String> {
        let mut ids: Vec<String> = at.iter().map(|i| self.ids[*i].clone()).collect();
        ids.sort();
        ids
    }

    fn later(&self, by: Duration) {
        *self.now.lock() += by;
    }

    async fn run_jobs(&self) {
        self.service.step().await;
        until(|| self.service.state_running() == 0).await;
    }
}

#[tokio::test]
async fn a_now_playing_fetches_the_songs_after_the_next_into_the_folder() {
    let w = world(|s| s.prefetch.count = 2);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    // The next song plays as it would have: the app buffers it before a download could finish.
    assert_eq!(w.asked(), w.ids_of(&[2, 3]));
    for id in &w.ids[2..4] {
        let ready = w.service.store().ready("soulseek", id).expect("ready");
        assert!(ready.starts_with(w.folder.path()));
    }
    assert!(w.service.store().ready("soulseek", &w.ids[1]).is_none());
    // Nothing went into the library or its logs.
    assert!(w.state.download_history.get_recent(10).is_empty());
}

#[tokio::test]
async fn songs_that_left_the_window_are_dropped_before_they_start() {
    let w = world(|s| s.prefetch.count = 2);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    // A skip to the fourth song before anything ran: the third and fourth are no longer coming.
    w.service.on_playing("alice", &w.ids[3], None).await;
    w.run_jobs().await;
    assert_eq!(w.asked(), w.ids_of(&[5, 6]));
}

#[tokio::test]
async fn nothing_is_fetched_while_prefetch_is_off() {
    let w = world(|s| s.prefetch.enabled = false);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert!(w.asked().is_empty());
}

#[tokio::test]
async fn with_downloads_one_at_a_time_nothing_is_fetched_ahead() {
    let w = world(|s| s.soulseek.parallel_downloads = 1);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert!(w.asked().is_empty());
    assert_eq!(w.service.status().await.paused.as_deref(), Some(ONE_AT_A_TIME));
}

#[tokio::test]
async fn one_transfer_is_always_left_for_a_heart() {
    let w = world(|s| s.prefetch.count = 4);
    *w.downloads.gated.lock() = true;
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.service.step().await;
    until(|| w.downloads.now.load(Ordering::SeqCst) == 2).await;
    w.service.step().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        w.service.state_running(),
        2,
        "three at once, so two ahead at most"
    );
    // Each one let through makes room for the next, never for a third beside them.
    for _ in 0..8 {
        w.downloads.open.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        w.service.step().await;
        assert!(w.service.state_running() <= 2);
    }
    until(|| w.service.state_running() == 0).await;
    assert_eq!(w.downloads.asked.lock().len(), 4);
    assert_eq!(w.downloads.most.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_member_whose_plays_keep_no_copies_gets_nothing_fetched() {
    let members = tempfile::tempdir().expect("a temp dir");
    let path = members.path().to_string_lossy().into_owned();
    let w = world(move |s| {
        s.family.enabled = true;
        s.family.members_path = path;
    });
    w.state.family_store.set_owner("winters");
    w.state
        .family_store
        .upsert_member(member("sam", Preset::Listener, 4));
    w.state
        .family_store
        .upsert_member(member("alex", Preset::Member, 3));
    let sam = FamilyCaller::member("sam", Preset::Listener, Abilities::preset(Preset::Listener));
    assert!(!w.service.may_keep_copies("sam", Some(&sam)));
    w.save_queue("sam", 0);
    w.service.on_playing("sam", &w.ids[0], Some(&sam)).await;
    w.run_jobs().await;
    assert!(w.asked().is_empty(), "a listener's plays keep no copies");

    // A member who adds songs at once does get the songs coming up ahead.
    let alex = FamilyCaller::member("alex", Preset::Member, Abilities::preset(Preset::Member));
    assert!(w.service.may_keep_copies("alex", Some(&alex)));
    w.save_queue("alex", 0);
    w.service.on_playing("alex", &w.ids[0], Some(&alex)).await;
    w.run_jobs().await;
    assert!(!w.asked().is_empty());
}

/// YouTube first, so a play is queued at once rather than after a look at Soulseek.
fn you_tube_first(s: &mut AppSettings) {
    s.subsonic.heart_download_sources = vec![octo_core::settings::HeartDownloadStep {
        source: octo_core::settings::HeartDownloadSource::YouTube,
        song_enabled: Some(true),
        album_enabled: Some(true),
        ..Default::default()
    }];
}

#[tokio::test]
async fn with_download_on_play_the_songs_coming_up_are_queued_as_plays_one_at_a_time() {
    let w = world(|s| {
        s.subsonic.download_on_play = true;
        s.subsonic.wait_for_lossless_on_play = true;
        you_tube_first(s);
        s.prefetch.count = 2;
        // AcoustID on: a song coming up has no heard audio, so only AcoustID can prove its copy.
        s.soulseek.acoust_id_api_key = "user-key".into();
    });
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.service.step().await;
    let queue = &w.state.track_acquisition_queue;
    assert!(queue.is_in_flight("soulseek", &w.ids[2]));
    assert!(
        !queue.is_in_flight("soulseek", &w.ids[3]),
        "one play waits at most"
    );
    w.service.step().await;
    assert!(
        !queue.is_in_flight("soulseek", &w.ids[3]),
        "never in a real play's place"
    );

    // The worker takes the waiting play: the next song coming up takes the free place.
    let taken = queue
        .dequeue(&CancellationToken::new())
        .await
        .expect("the waiting play");
    assert_eq!(taken.external_id, w.ids[2]);
    w.service.step().await;
    assert!(queue.is_in_flight("soulseek", &w.ids[3]));
    assert!(w.asked().is_empty(), "nothing goes into the prefetch folder");
}

#[tokio::test]
async fn a_radio_list_counts_only_in_order_and_radio_only_keeps_radio_songs() {
    let w = world(|s| s.prefetch.scope = PrefetchScope::RadioOnly);
    w.state.radio_queues.register(w.ids.clone());
    w.service.note_radio(&w.ids[..4]);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.service.on_playing("alice", &w.ids[1], None).await;
    w.run_jobs().await;
    // The fifth and sixth songs are coming up in the list too, but no radio offered them.
    assert_eq!(w.asked(), w.ids_of(&[3]));
}

#[tokio::test]
async fn a_song_already_ready_is_not_fetched_again() {
    let w = world(|s| s.prefetch.count = 1);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    w.service.on_playing("alice", &w.ids[6], None).await;
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert_eq!(w.asked(), w.ids_of(&[2]));
}

#[tokio::test]
async fn a_listener_idle_for_half_an_hour_has_nothing_coming_up_but_what_others_share() {
    let w = world(|s| {
        s.prefetch.count = 1;
        s.soulseek.parallel_downloads = 1;
    });
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.save_queue("bob", 3);
    w.service.on_playing("bob", &w.ids[3], None).await;
    w.later(Duration::minutes(20));
    // Carol has the same song coming up as Alice.
    w.save_queue("carol", 0);
    w.service.on_playing("carol", &w.ids[0], None).await;
    assert_eq!(w.service.queued_ids(), vec![w.ids[2].clone(), w.ids[5].clone()]);
    w.later(Duration::minutes(15));
    w.service.step().await;
    // Bob played 35 minutes ago: his song goes. Alice's stays, since Carol has it coming up too.
    assert_eq!(w.service.queued_ids(), vec![w.ids[2].clone()]);
}

#[tokio::test]
async fn nothing_starts_while_soulseek_is_off_or_cannot_be_reached() {
    for link in [Arc::new(OffLink) as Arc<dyn ISoulseekLink>, Arc::new(OfflineLink)] {
        let w = world_with(|s| s.prefetch.count = 1, Some(link));
        w.save_queue("alice", 0);
        w.service.on_playing("alice", &w.ids[0], None).await;
        w.run_jobs().await;
        assert!(w.asked().is_empty());
        assert_eq!(
            w.service.status().await.paused.as_deref(),
            Some("Waiting for Soulseek to connect")
        );
    }
}

/// A Mornye whose one gateway session is in `state`, and its client over `settings`.
async fn mornye(state: &str) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::builder().start().await;
    let paused_s = if state == "active" { 0.0 } else { 900.0 };
    Mock::given(method("POST"))
        .and(path("/api/get_signed_sessions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"sessions": [
                {"key": "gateway-v2-abc-6836abb49bb0e8c9", "kind": "gateway", "label": "Example",
                 "extensions": ["example-ext"], "state": state, "auth_paused_s": paused_s}
            ]}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get_download_services"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"services": [
                {"id": "ext:example-ext", "label": "Example", "extensions": ["example-ext"]}
            ]}})),
        )
        .mount(&server)
        .await;
    server
}

/// Mornye on at `url`, Soulseek off.
fn mornye_world(url: String) -> World {
    let settings_url = url.clone();
    let probe = AppSettings {
        spotiflac: octo_core::settings::SpotiflacSettings {
            enabled: true,
            url,
            ..Default::default()
        },
        ..Default::default()
    };
    let client = Arc::new(SpotiflacClient::new(Arc::new(
        octo_core::settings::SettingsStore::for_tests(probe),
    )));
    world_full(
        move |s| {
            s.prefetch.count = 1;
            s.spotiflac.enabled = true;
            s.spotiflac.url = settings_url;
        },
        Some(Arc::new(OffLink)),
        Some(client),
    )
}

#[tokio::test]
async fn nothing_starts_while_mornye_is_the_only_source_and_its_check_is_paused() {
    let server = mornye("unverified").await;
    let w = mornye_world(server.uri());
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert!(w.asked().is_empty());
    assert_eq!(
        w.service.status().await.paused.as_deref(),
        Some("Waiting for Mornye's check")
    );
}

#[tokio::test]
async fn songs_are_fetched_while_mornyes_check_passes() {
    let server = mornye("active").await;
    let w = mornye_world(server.uri());
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert_eq!(w.asked(), w.ids_of(&[2]));
    assert_eq!(w.service.status().await.paused, None);
}

/// A Mornye client, on at `url`, over a settings store of its own that a test may change.
fn mornye_client(url: &str) -> (Arc<octo_core::settings::SettingsStore>, Arc<SpotiflacClient>) {
    let store = Arc::new(octo_core::settings::SettingsStore::for_tests(AppSettings {
        spotiflac: octo_core::settings::SpotiflacSettings {
            enabled: true,
            url: url.into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let client = Arc::new(SpotiflacClient::new(Arc::clone(&store)));
    (store, client)
}

/// A Mornye that must never be asked.
async fn untouched_mornye() -> wiremock::MockServer {
    let server = wiremock::MockServer::builder().start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_song_mornyes_check_kept_rests_until_mornye_may_try_again() {
    // The wait says Mornye allows its next try in 15 minutes (the stand-in's pause). Mornye
    // stays on and no read shows its check passing.
    let (_, client) = mornye_client("http://127.0.0.1:1");
    let w = world_full(|s| s.prefetch.count = 1, None, Some(client));
    *w.downloads.mornye_waits.lock() = true;
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert_eq!(w.asked(), w.ids_of(&[2]));
    *w.downloads.mornye_waits.lock() = false;
    let again = |w: &World| {
        let (service, ids) = (w.service.clone(), w.ids.clone());
        async move {
            service.on_playing("alice", &ids[1], None).await;
            service.on_playing("alice", &ids[0], None).await;
        }
    };
    w.later(Duration::minutes(11));
    again(&w).await;
    w.run_jobs().await;
    assert_eq!(
        w.downloads.asked.lock().len(),
        1,
        "not asked again before Mornye may try"
    );
    w.later(Duration::minutes(5));
    again(&w).await;
    w.run_jobs().await;
    assert!(
        w.service.store().is_ready("soulseek", &w.ids[2]),
        "tried again once Mornye may"
    );
}

#[tokio::test]
async fn a_pass_ends_the_rest_of_a_song_mornyes_check_kept() {
    use wiremock::matchers::path;
    use wiremock::{Mock, ResponseTemplate};
    struct Sessions(Arc<AtomicBool>);
    impl wiremock::Respond for Sessions {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            let (state, paused) = if self.0.load(Ordering::SeqCst) {
                ("unverified", 900.0)
            } else {
                ("active", 0.0)
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"sessions": [
                {"key": "gateway-v2-abc-1", "kind": "gateway", "label": "Example",
                 "extensions": ["example-ext"], "state": state, "auth_paused_s": paused}
            ]}}))
        }
    }
    let paused = Arc::new(AtomicBool::new(true));
    let server = wiremock::MockServer::builder().start().await;
    Mock::given(path("/api/get_signed_sessions"))
        .respond_with(Sessions(paused.clone()))
        .mount(&server)
        .await;
    Mock::given(path("/api/get_download_services"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"services": [
                {"id": "ext:example-ext", "label": "Example", "extensions": ["example-ext"]}
            ]}})),
        )
        .mount(&server)
        .await;
    let probe = AppSettings {
        spotiflac: octo_core::settings::SpotiflacSettings {
            enabled: true,
            url: server.uri(),
            ..Default::default()
        },
        ..Default::default()
    };
    let check = crate::services::spotiflac::MornyeCheck::new(
        Arc::new(crate::services::spotiflac::MornyeCheckStore::new(None)),
        crate::services::spotiflac::CheckTimes {
            fresh_for: std::time::Duration::ZERO,
            call_bound: std::time::Duration::from_secs(5),
        },
        None,
    );
    let client = Arc::new(
        SpotiflacClient::new(Arc::new(octo_core::settings::SettingsStore::for_tests(probe)))
            .with_check(check),
    );
    let w = world_full(|s| s.prefetch.count = 1, None, Some(client));
    *w.downloads.mornye_waits.lock() = true;
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert_eq!(w.asked(), w.ids_of(&[2]));

    // Someone passes the check: the song goes again at once, before its pause's end.
    *w.downloads.mornye_waits.lock() = false;
    paused.store(false, Ordering::SeqCst);
    w.service.on_playing("alice", &w.ids[1], None).await;
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert!(
        w.service.store().is_ready("soulseek", &w.ids[2]),
        "tried again after the Pass"
    );
}

#[tokio::test]
async fn switching_mornye_off_ends_the_rest_of_a_song_its_check_kept() {
    let (store, client) = mornye_client("http://127.0.0.1:1");
    let w = world_full(|s| s.prefetch.count = 1, None, Some(client));
    *w.downloads.mornye_waits.lock() = true;
    // A pause that names no check: no read could end it, only Mornye switched off.
    *w.downloads.mornye_names_no_check.lock() = true;
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert_eq!(w.asked(), w.ids_of(&[2]));

    // Mornye is switched off a minute later, long before its pause ends: the song goes again.
    *w.downloads.mornye_waits.lock() = false;
    let mut off = (*store.current()).clone();
    off.spotiflac.enabled = false;
    store.set(off);
    w.later(Duration::minutes(1));
    w.service.on_playing("alice", &w.ids[1], None).await;
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert!(
        w.service.store().is_ready("soulseek", &w.ids[2]),
        "tried again once Mornye was off"
    );
}

#[tokio::test]
async fn with_mornye_off_prefetch_waits_for_soulseek_as_before_and_never_asks_mornye() {
    let server = untouched_mornye().await;
    let (store, client) = mornye_client(&server.uri());
    let mut off = (*store.current()).clone();
    off.spotiflac.enabled = false;
    store.set(off);
    let w = world_full(
        |s| s.prefetch.count = 1,
        Some(Arc::new(OfflineLink)),
        Some(client),
    );
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;

    assert!(w.asked().is_empty());
    assert_eq!(
        w.service.status().await.paused.as_deref(),
        Some("Waiting for Soulseek to connect")
    );
    server.verify().await;
}

#[tokio::test]
async fn while_slskd_is_logged_in_prefetch_never_asks_mornye() {
    let server = untouched_mornye().await;
    let (_, client) = mornye_client(&server.uri());
    let w = world_full(
        |s| s.prefetch.count = 1,
        Some(Arc::new(LoggedInLink)),
        Some(client),
    );
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;

    assert_eq!(w.asked(), w.ids_of(&[2]));
    assert_eq!(w.service.status().await.paused, None);
    server.verify().await;
}

#[tokio::test]
async fn with_no_slskd_a_mornye_that_does_not_answer_is_what_prefetch_waits_for() {
    let w = mornye_world("http://127.0.0.1:1".to_string());
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;

    assert!(w.asked().is_empty());
    assert_eq!(
        w.service.status().await.paused.as_deref(),
        Some("Waiting for Mornye")
    );
}

#[tokio::test]
async fn a_source_that_could_not_be_reached_is_tried_again_soon() {
    let w = world(|s| s.prefetch.count = 1);
    *w.downloads.refuse.lock() = Some(std::io::ErrorKind::ConnectionRefused);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert_eq!(w.asked(), w.ids_of(&[2]));
    assert_eq!(w.service.status().await.errors.len(), 1);
    *w.downloads.refuse.lock() = None;
    w.later(Duration::minutes(11));
    w.service.on_playing("alice", &w.ids[1], None).await;
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert!(w.service.store().is_ready("soulseek", &w.ids[2]), "tried again");
}

#[tokio::test]
async fn a_song_that_failed_rests_for_hours() {
    let w = world(|s| s.prefetch.count = 1);
    *w.downloads.refuse.lock() = Some(std::io::ErrorKind::InvalidData);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    *w.downloads.refuse.lock() = None;
    w.later(Duration::minutes(11));
    w.service.on_playing("alice", &w.ids[1], None).await;
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert!(!w.service.store().is_ready("soulseek", &w.ids[2]));
    assert_eq!(w.downloads.asked.lock().len(), 1);
}

#[tokio::test]
async fn the_folder_is_swept_with_prefetch_off_too() {
    let w = world(|s| s.prefetch.count = 1);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    assert!(w.service.store().is_ready("soulseek", &w.ids[2]));
    let mut off = w.state.settings.current().as_ref().clone();
    off.prefetch.enabled = false;
    w.state.settings.set(off);
    w.later(Duration::days(31));
    w.service.step().await;
    assert!(!w.service.store().is_ready("soulseek", &w.ids[2]));
}

#[tokio::test]
async fn the_status_reads_the_folder_and_clear_empties_it() {
    let w = world(|s| s.prefetch.count = 1);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.run_jobs().await;
    let status = w.service.status().await;
    assert!(status.enabled);
    assert_eq!(status.ready, 1);
    assert_eq!(status.used_bytes, 4);
    assert_eq!(status.cap_bytes, 20 * 1024 * 1024 * 1024);
    // Played just now, so it stays.
    w.service.store().touch("soulseek", &w.ids[2]);
    assert_eq!(w.service.clear(), 0);
}

#[tokio::test]
async fn a_song_kept_ready_that_comes_up_moves_ahead_of_the_kept_ones() {
    let w = world(|s| {
        s.prefetch.count = 1;
        s.soulseek.parallel_downloads = 1;
    });
    w.service
        .keep_ready(vec![("alice".into(), vec![w.ids[5].clone(), w.ids[4].clone()])])
        .await;
    assert_eq!(w.service.queued_ids(), vec![w.ids[5].clone(), w.ids[4].clone()]);
    // Alice plays the third song: the fifth is now coming up.
    w.save_queue("alice", 2);
    w.service.on_playing("alice", &w.ids[2], None).await;
    assert_eq!(w.service.queued_ids(), vec![w.ids[4].clone(), w.ids[5].clone()]);
}

#[tokio::test]
async fn a_song_kept_ready_keeps_its_listener_when_they_go_idle() {
    let w = world(|s| s.soulseek.parallel_downloads = 1);
    w.save_queue("alice", 0);
    w.service.on_playing("alice", &w.ids[0], None).await;
    w.service
        .keep_ready(vec![("alice".into(), vec![w.ids[6].clone()])])
        .await;
    w.later(Duration::minutes(40));
    w.service.step().await;
    let kept = w
        .service
        .queued_jobs()
        .into_iter()
        .find(|(id, _, _)| *id == w.ids[6])
        .expect("still kept");
    assert_eq!(kept.1, ["alice"], "whose library it goes to");
}

#[tokio::test]
async fn songs_kept_ready_go_to_the_prefetch_folder_with_download_on_play_too() {
    let w = world(|s| {
        s.subsonic.download_on_play = true;
        s.soulseek.parallel_downloads = 1;
    });
    w.service
        .keep_ready(vec![("alice".into(), vec![w.ids[6].clone()])])
        .await;
    let jobs = w.service.queued_jobs();
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].2, "into the prefetch folder, not the library");
}
