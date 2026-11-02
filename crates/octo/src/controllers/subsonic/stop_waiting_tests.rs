//! "Stop waiting for Lidarr" over the Subsonic API (octoAcquisitions 4): getAcquisitions and
//! getAcquisition carry `stopWaiting` only for a caller whose heart kept a copy of the row's song
//! that waits for an open Lidarr follow, and no more once the wait stopped; stopWaitingForLidarr
//! lands that copy and answers a plain ok, refuses anyone else with 50, asks the admin to name the
//! person with 10, and answers 70 once nothing waits.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::{AppSettings, DownloadSource, SettingsStore};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::downloads::{NOTHING_WAITS_FOR_LIDARR, STOP_NAME_THE_PERSON, STOP_NOT_YOURS};
use super::test_app::*;
use crate::app::AppState;
use crate::http::pipeline::App;
use crate::services::common::test_fakes::{FakeMetadata, until};
use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::ReplacementHandoff;
use crate::services::lidarr::{
    HeldPlacement, LidarrClient, LidarrHeartAcquisitionService, LidarrHeartExtras, LidarrPendingAlbums,
    PendingAlbum,
};
use crate::services::metadata::{DeezerMetadataService, DeezerRateLimitHandler, DeezerRateLimiter};
use crate::services::notifications::NotificationService;
use crate::services::subsonic::NavidromeIdentityService;

/// Navidrome as far as these calls need it: a ping that accepts the token "good", and getUser
/// saying only "admin" is an admin.
struct WaitNavidrome;

impl Respond for WaitNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path();
        let query = query_of(request);
        if path.ends_with("/rest/ping") {
            return if query.get("t").map(String::as_str) == Some("good") {
                navidrome_json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#)
            } else {
                navidrome_json(NAVIDROME_WRONG_PASSWORD)
            };
        }
        if path.ends_with("/rest/getUser") {
            let username = query.get("username").cloned().unwrap_or_default();
            return navidrome_json(
                &json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "user": {
                    "username": username, "adminRole": username == "admin",
                }}})
                .to_string(),
            );
        }
        ResponseTemplate::new(404)
    }
}

/// The download pipeline as far as a follow's held files go: each is moved to its target.
#[derive(Default)]
struct Placer {
    placed: Mutex<Vec<String>>,
}

#[async_trait]
impl IDownloadService for Placer {
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
        anyhow::bail!("not set up")
    }

    fn get_download_status(&self, _: &str) -> Option<DownloadInfo> {
        None
    }

    async fn place_held(&self, held: &HeldPlacement) -> Option<String> {
        std::fs::create_dir_all(Path::new(&held.target).parent()?).ok()?;
        std::fs::rename(&held.held_path, &held.target).ok()?;
        self.placed.lock().push(held.target.clone());
        Some(held.target.clone())
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

struct WaitApp {
    state: AppState,
    app: App,
    _navidrome: MockServer,
    files: tempfile::TempDir,
    pending: Arc<LidarrPendingAlbums>,
    placer: Arc<Placer>,
}

impl WaitApp {
    async fn new() -> WaitApp {
        let navidrome = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(WaitNavidrome)
            .mount(&navidrome)
            .await;
        let pending = Arc::new(LidarrPendingAlbums::new(None));
        let placer = Arc::new(Placer::default());
        // Lidarr itself is not set up: a follow asked to end lets go of its held files all the same.
        let lidarr = Arc::new(SettingsStore::for_tests(AppSettings::default()));
        let service = LidarrHeartAcquisitionService::new(
            Arc::new(LidarrClient::new(lidarr.clone())),
            Arc::new(FakeMetadata::default()),
            Arc::new(DeezerMetadataService::with_base_url(
                Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
                lidarr.clone(),
                "http://127.0.0.1:1",
            )),
            lidarr.clone(),
            NavidromeIdentityService::new(lidarr.clone(), reqwest::Client::new()),
            Arc::new(NotificationService::new(Vec::new(), lidarr, None)),
            LidarrHeartExtras {
                pending: Some(pending.clone()),
                downloads: Some(placer.clone()),
                ..Default::default()
            },
        );
        service.set_poll_override(Some(Duration::from_millis(20)));
        let state = state_with(settings(&navidrome.uri()), |inner| {
            inner.lidarr_hearts = Arc::new(service);
        });
        WaitApp {
            app: app(&state),
            state,
            _navidrome: navidrome,
            files: tempfile::tempdir().expect("a temp dir"),
            pending,
            placer,
        }
    }

    /// An open follow of Octo's on `album`, and alice's kept copy of `song` waiting for it, the
    /// song's row listed for alice and bob, who hearted it too but kept nothing.
    fn kept_copy_waits(&self, album: &str, song: &str) -> HeldPlacement {
        self.pending.remember(PendingAlbum {
            foreign_album_id: album.into(),
            artist: "Massive Attack".into(),
            title: "Mezzanine".into(),
            search_command: 3,
            songs: vec![format!("soulseek:{song}")],
            holders: vec!["alice".into()],
            ..Default::default()
        });
        let held_path = self.files.path().join(format!("held-{song}.m4a"));
        std::fs::write(&held_path, b"the version alice heard").expect("written");
        let held = HeldPlacement {
            song: format!("soulseek:{song}"),
            held_path: held_path.to_string_lossy().into_owned(),
            target: self
                .files
                .path()
                .join(format!("Massive Attack/Mezzanine/{song}.m4a"))
                .to_string_lossy()
                .into_owned(),
            kept: true,
            follows: vec![album.into()],
            users: vec!["alice".into()],
        };
        self.pending.hold_placement(held.clone());
        for user in ["alice", "bob"] {
            self.state.acquisition_tracker.begin(
                "soulseek",
                song,
                None,
                Some(user),
                Some("Massive Attack"),
                Some(song),
                Some("Mezzanine"),
            );
        }
        held
    }

    /// `user`'s getAcquisitions rows, by key.
    async fn rows(&self, user: &str) -> serde_json::Map<String, Value> {
        let envelope = get(
            &self.app,
            &format!(
                "/rest/getAcquisitions.view?{}",
                auth(user, "good", "octo-android")
            ),
        )
        .await
        .envelope();
        envelope["acquisitions"]["acquisition"]
            .as_array()
            .expect("a list of acquisitions")
            .iter()
            .map(|row| (row["key"].as_str().expect("a key").to_string(), row.clone()))
            .collect()
    }

    /// stopWaitingForLidarr as `user`, with `query`.
    async fn stop(&self, user: &str, query: &str) -> Value {
        get(
            &self.app,
            &format!(
                "/rest/stopWaitingForLidarr.view?{}&{query}",
                auth(user, "good", "octo-android")
            ),
        )
        .await
        .envelope()
    }
}

fn error_of(envelope: &Value) -> (i64, String) {
    assert_eq!(envelope["status"], "failed", "{envelope}");
    (
        envelope["error"]["code"].as_i64().expect("a code"),
        envelope["error"]["message"]
            .as_str()
            .expect("a message")
            .to_string(),
    )
}

/// The rows carry `stopWaiting` only for someone whose heart kept the copy that waits: bob, whose
/// row it is too, and the admin get none, and a row with no copy waiting carries none.
/// getAcquisition carries the same.
#[tokio::test]
async fn the_rows_carry_stop_waiting_only_for_someone_whose_kept_copy_waits_for_lidarr() {
    let t = WaitApp::new().await;
    t.kept_copy_waits("rg-mezzanine", "teardrop");
    t.state.acquisition_tracker.begin(
        "soulseek",
        "angel",
        None,
        Some("alice"),
        Some("Massive Attack"),
        Some("Angel"),
        None,
    );

    let rows = t.rows("alice").await;
    assert_eq!(rows["soulseek:teardrop"]["stopWaiting"], true);
    assert!(
        rows["soulseek:angel"].get("stopWaiting").is_none(),
        "nothing of it waits for Lidarr"
    );
    assert!(
        t.rows("bob").await["soulseek:teardrop"]
            .get("stopWaiting")
            .is_none(),
        "bob's heart kept no copy of it"
    );
    t.state
        .acquisition_tracker
        .begin("soulseek", "teardrop", None, Some("admin"), None, None, None);
    assert!(
        t.rows("admin").await["soulseek:teardrop"]
            .get("stopWaiting")
            .is_none(),
        "the admin stops it from the dashboard"
    );

    let detail = get(
        &t.app,
        &format!(
            "/rest/getAcquisition.view?{}&key=soulseek:teardrop",
            auth("alice", "good", "octo-android")
        ),
    )
    .await
    .envelope();
    assert_eq!(detail["acquisition"]["stopWaiting"], true);
}

/// stopWaitingForLidarr: a missing key is error 10; bob, who hearted the song but kept nothing, is
/// told nothing waits for him (70), and carol, whose row it is not, gets 50; bob naming alice gets
/// 50; the admin who names nobody gets 10 while a copy waits, and 70 on a row where nothing does.
/// Alice's own call answers a plain ok, the follow nobody else waits on ends and her held copy
/// lands; her row no longer offers it, and asking again answers 70.
#[tokio::test]
async fn stop_waiting_for_lidarr_lands_the_held_copy_and_answers_10_50_and_70() {
    let t = WaitApp::new().await;
    let held = t.kept_copy_waits("rg-mezzanine", "teardrop");

    assert_eq!(
        error_of(&t.stop("alice", "f=json").await),
        (10, "Required parameter is missing: key".to_string())
    );
    assert_eq!(
        error_of(&t.stop("bob", "key=soulseek:teardrop").await),
        (70, NOTHING_WAITS_FOR_LIDARR.to_string()),
        "bob hearted it: nothing waits for him"
    );
    assert_eq!(
        error_of(&t.stop("carol", "key=soulseek:teardrop").await),
        (50, STOP_NOT_YOURS.to_string())
    );
    assert_eq!(
        error_of(&t.stop("bob", "key=soulseek:teardrop&user=alice").await),
        (50, STOP_NOT_YOURS.to_string()),
        "only the admin stops another person's wait"
    );
    assert_eq!(
        error_of(&t.stop("admin", "key=soulseek:teardrop").await),
        (10, STOP_NAME_THE_PERSON.to_string())
    );
    assert_eq!(
        error_of(&t.stop("admin", "key=soulseek:angel").await),
        (70, NOTHING_WAITS_FOR_LIDARR.to_string()),
        "nothing waits there, so there is nobody to name"
    );
    assert!(t.pending.get("rg-mezzanine").expect("listed").following());

    let ok = t.stop("alice", "key=soulseek:teardrop").await;
    assert_eq!(ok["status"], "ok", "{ok}");
    assert!(ok.get("error").is_none());
    assert!(
        !t.pending.get("rg-mezzanine").expect("listed").following(),
        "the follow's wait ended"
    );

    until(|| t.placer.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file(), "the held copy landed");
    assert!(!Path::new(&held.held_path).exists());
    assert!(
        t.rows("alice").await["soulseek:teardrop"]
            .get("stopWaiting")
            .is_none(),
        "dropped once the wait stopped"
    );
    assert_eq!(
        error_of(&t.stop("alice", "key=soulseek:teardrop").await),
        (70, NOTHING_WAITS_FOR_LIDARR.to_string())
    );
    assert_eq!(
        error_of(&t.stop("bob", "key=soulseek:teardrop").await),
        (70, NOTHING_WAITS_FOR_LIDARR.to_string()),
        "nothing waits for anyone now"
    );
}

/// One follow, two people waiting on it for two songs, each with a kept copy: alice's Stop waiting
/// lands her copy alone and ends only her wait, so the follow goes on for bob, his copy still
/// waiting; once bob stops too, nobody waits on the follow and it ends.
#[tokio::test]
async fn one_persons_stop_waiting_ends_only_their_wait_on_a_shared_follow() {
    use crate::services::lidarr::SongGate;
    use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardReading, HeartGate};

    let t = WaitApp::new().await;
    let gate = |song: &str, user: &str| SongGate {
        song: format!("soulseek:{song}"),
        title: song.into(),
        artist: "Massive Attack".into(),
        gate: HeartGate {
            user: user.into(),
            origin: AcquisitionOrigin::Heart,
            r: None,
            heard: None,
            heard_seconds: None,
            reading: HeardReading::NotHeard,
            stand_in: None,
        },
    };
    t.pending.remember(PendingAlbum {
        foreign_album_id: "rg-mezzanine".into(),
        artist: "Massive Attack".into(),
        title: "Mezzanine".into(),
        search_command: 3,
        songs: vec!["soulseek:teardrop".into(), "soulseek:angel".into()],
        holders: vec!["alice".into(), "bob".into()],
        gates: vec![gate("teardrop", "alice"), gate("angel", "bob")],
        ..Default::default()
    });
    let kept = |song: &str, user: &str| {
        let held_path = t.files.path().join(format!("held-{song}.m4a"));
        std::fs::write(&held_path, format!("the version {user} heard")).expect("written");
        let held = HeldPlacement {
            song: format!("soulseek:{song}"),
            held_path: held_path.to_string_lossy().into_owned(),
            target: t
                .files
                .path()
                .join(format!("Massive Attack/Mezzanine/{song}.m4a"))
                .to_string_lossy()
                .into_owned(),
            kept: true,
            follows: vec!["rg-mezzanine".into()],
            users: vec![user.into()],
        };
        t.pending.hold_placement(held.clone());
        t.state.acquisition_tracker.begin(
            "soulseek",
            song,
            None,
            Some(user),
            Some("Massive Attack"),
            Some(song),
            Some("Mezzanine"),
        );
        held
    };
    let alices = kept("teardrop", "alice");
    let bobs = kept("angel", "bob");

    let ok = t.stop("alice", "key=soulseek:teardrop").await;
    assert_eq!(ok["status"], "ok", "{ok}");
    until(|| t.placer.placed.lock().len() == 1).await;
    assert!(Path::new(&alices.target).is_file(), "alice's copy landed");
    let entry = t.pending.get("rg-mezzanine").expect("listed");
    assert!(entry.following(), "bob still waits on the follow");
    assert_eq!(entry.holders, ["bob"]);
    assert!(entry.gates_of("soulseek:teardrop").is_empty());
    assert!(Path::new(&bobs.held_path).is_file(), "bob's copy still waits");
    assert_eq!(t.rows("bob").await["soulseek:angel"]["stopWaiting"], true);

    let ok = t.stop("bob", "key=soulseek:angel").await;
    assert_eq!(ok["status"], "ok", "{ok}");
    until(|| t.placer.placed.lock().len() == 2).await;
    assert!(
        !t.pending.get("rg-mezzanine").expect("listed").following(),
        "nobody waits on it any more"
    );
}

/// The admin stops a holder's wait by naming them; naming someone whose heart kept nothing
/// answers 70.
#[tokio::test]
async fn the_admin_stops_a_holders_wait_by_naming_them() {
    let t = WaitApp::new().await;
    let held = t.kept_copy_waits("rg-mezzanine", "teardrop");

    assert_eq!(
        error_of(&t.stop("admin", "key=soulseek:teardrop&user=bob").await),
        (70, NOTHING_WAITS_FOR_LIDARR.to_string())
    );
    let ok = t.stop("admin", "key=soulseek:teardrop&user=alice").await;
    assert_eq!(ok["status"], "ok", "{ok}");

    until(|| t.placer.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
}
