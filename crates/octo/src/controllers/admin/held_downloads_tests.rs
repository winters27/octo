//! A held download on the dashboard: the In progress rows say when someone may keep one anyway
//! (with an album's count of held tracks, for "Keep these anyway"), from the holds, never the
//! row's own flag; the holder menu reads who; and the admin keeps for the person they pick, the
//! same action as the apps' keepAcquisitionAnyway with `user`. Every one of these needs a
//! Navidrome admin's own sign-in.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use chrono::Utc;
use octo_core::fingerprint::heart_gate::{
    AcquisitionOrigin, HeardRecord, HeardRecording, HeardSource, HeartGate, WaitingFor,
};
use octo_core::settings::{AppSettings, SoulseekSettings, SubsonicSettings};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use super::downloads::NOTHING_TO_PUT_BACK;
use super::test_app::{Reply, app, send};
use crate::app::{AppState, TestServices};
use crate::controllers::subsonic::downloads::{NAME_THE_PERSON, NOTHING_WAITING};
use crate::http::pipeline::App;
use crate::services::common::album_gates::tracks_wait_line;
use crate::services::common::soulseek_hold_store::HeldAlbumTrack;
use crate::services::common::{HeldAcquisition, HeldKind, Holder};
use crate::services::lidarr::{CANNOT_SEE, ILidarrHeartAcquisitionService, UnsettledFollow};
use crate::services::test_support::refused_address;

/// Lidarr's hearts as far as Put Lidarr back goes: the follows Octo cannot settle wait for the
/// admin to put Lidarr back.
#[derive(Default)]
struct Waits {
    unsettled: Mutex<Vec<UnsettledFollow>>,
    put_back: Mutex<Vec<String>>,
}

#[async_trait]
impl ILidarrHeartAcquisitionService for Waits {
    async fn try_acquire_track(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: Option<&str>,
        _: AcquisitionOrigin,
        _: &[HeartGate],
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn try_acquire_album(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: Option<&str>,
        _: AcquisitionOrigin,
        _: &[HeartGate],
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn unsettled_follows(&self) -> Vec<UnsettledFollow> {
        self.unsettled.lock().clone()
    }

    fn put_lidarr_back(&self, follow: &str) -> bool {
        let mut unsettled = self.unsettled.lock();
        let before = unsettled.len();
        unsettled.retain(|known| known.follow != follow);
        if unsettled.len() == before {
            return false;
        }
        self.put_back.lock().push(follow.to_string());
        true
    }
}

struct Dashboard {
    state: AppState,
    app: App,
    waits: Arc<Waits>,
    files: tempfile::TempDir,
}

impl Dashboard {
    fn new() -> Dashboard {
        let files = tempfile::tempdir().expect("a temp dir");
        let closed = format!("http://{}", refused_address());
        let mut settings = AppSettings {
            subsonic: SubsonicSettings {
                url: Some(closed.clone()),
                auto_detect_download_path: false,
                record_requested_by: false,
                ..Default::default()
            },
            soulseek: SoulseekSettings {
                base_url: Some(closed.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        // The status page's probes refuse at once.
        settings.radio_sources.sonic_url = closed.clone();
        let mut state = AppState::for_tests_with(
            settings,
            TestServices {
                sign_in: Some("required".into()),
                config_dir: Some(files.path().to_path_buf()),
                raw_settings: vec![
                    ("Soulseek:BaseUrl".into(), closed.clone()),
                    ("YouTube:ShimUrl".into(), closed),
                ],
                ..Default::default()
            },
        );
        let waits = Arc::new(Waits::default());
        Arc::get_mut(&mut state.inner)
            .expect("nothing else holds a fresh test state")
            .lidarr_hearts = waits.clone();
        Dashboard {
            app: app(state.clone()),
            state,
            waits,
            files,
        }
    }

    fn token(&self, who: &str) -> String {
        match who {
            "recovery" => self.state.browse_sessions.create_recovery(),
            user => self.state.browse_sessions.create(user),
        }
    }

    async fn get(&self, who: &str, path: &str) -> Reply {
        let token = self.token(who);
        send(
            &self.app,
            Method::GET,
            path,
            &[("X-Octo-Browse-Token", &token)],
            None,
        )
        .await
    }

    async fn post(&self, who: Option<&str>, path: &str, body: &Value) -> Reply {
        let token = who.map(|who| self.token(who));
        let mut headers = vec![("X-Octo-Admin", "1")];
        if let Some(token) = token.as_deref() {
            headers.push(("X-Octo-Browse-Token", token));
        }
        send(&self.app, Method::POST, path, &headers, Some(&body.to_string())).await
    }

    /// The In progress rows the dashboard reads, by key.
    async fn rows(&self) -> Map<String, Value> {
        self.get("admin", "/api/admin/acquisitions").await.json()["acquisitions"]
            .as_array()
            .expect("the rows")
            .iter()
            .map(|row| (row["key"].as_str().expect("a key").to_string(), row.clone()))
            .collect()
    }

    async fn holders(&self, key: &str) -> Value {
        self.get("admin", &format!("/api/admin/acquisitions/holders?key={key}"))
            .await
            .json()
    }

    /// Record who asked, switched as the settings page saves it.
    fn record_who_asked(&self, on: bool) {
        let mut settings = (*self.state.settings.current()).clone();
        settings.subsonic.record_requested_by = on;
        self.state.settings.set(settings);
    }
}

/// The labels a holder menu shows, in order.
fn labels(menu: &Value) -> Vec<String> {
    menu.as_array()
        .expect("a menu")
        .iter()
        .map(|person| person["label"].as_str().expect("a label").to_string())
        .collect()
}

/// The token of the `index`th person of a holder menu.
fn token_of(menu: &Value, index: usize) -> String {
    menu[index]["token"].as_str().expect("a token").to_string()
}

impl Dashboard {
    /// The song "help" held while AcoustID is off for alice and bob, who each heard a video of it.
    fn hold_song(&self) -> HeldAcquisition {
        let heard = |video: &str| HeardRecord {
            heard_at: Utc::now(),
            source: HeardSource::Video {
                id: video.into(),
                size: Some(1000),
                content_type: "audio/mp4".into(),
                itag: Some(140),
            },
            sample_id: None,
        };
        let record = self.state.soulseek_holds.hold(HeldAcquisition {
            kind: HeldKind::Track,
            provider: "soulseek".into(),
            external_id: "help".into(),
            requested_by: Some("alice".into()),
            held_since_utc: Utc::now(),
            origin: Some(AcquisitionOrigin::Heart),
            waiting_for: Some(WaitingFor::AcoustIdOff),
            holders: ["alice", "bob"]
                .iter()
                .map(|user| Holder {
                    user: (*user).into(),
                    heard: Some(heard(&format!("{}VIDEO01", user.to_uppercase()))),
                    ..Default::default()
                })
                .collect(),
            line: Some("AcoustID is off, so Octo cannot check what played".into()),
            ..Default::default()
        });
        self.state.heart_acquisition_coordinator.hold_row_for(&record);
        record
    }

    /// The held track `id` of alice's hearted album "help-album", its row and its album's held.
    fn hold_track(&self, id: &str, held: usize) -> HeldAcquisition {
        let file: PathBuf = self.files.path().join(format!("{id}.flac"));
        std::fs::write(&file, b"a held track").expect("written");
        let record = self.state.soulseek_holds.hold(HeldAcquisition {
            kind: HeldKind::Track,
            provider: "soulseek".into(),
            external_id: id.into(),
            requested_by: Some("alice".into()),
            held_since_utc: Utc::now(),
            origin: Some(AcquisitionOrigin::AlbumHeart),
            waiting_for: Some(WaitingFor::NotYetProven),
            holders: vec![Holder {
                user: "alice".into(),
                ..Default::default()
            }],
            held_files: vec![file.to_string_lossy().to_string()],
            album_track: Some(HeldAlbumTrack {
                album_key: "soulseek:help-album".into(),
                song_ids: vec![id.into()],
                artist: "The Beatles".into(),
                title: id.into(),
                duration_seconds: Some(139),
                r: HeardRecording::default(),
            }),
            line: Some(tracks_wait_line(held)),
            ..Default::default()
        });
        self.state.heart_acquisition_coordinator.hold_row_for(&record);
        record
    }
}

/// The holder menu and Keep anyway refuse no sign-in and the recovery code: they name people and
/// land files in the library.
#[tokio::test]
async fn the_holder_menu_and_keep_anyway_need_a_navidrome_admins_own_sign_in() {
    let dash = Dashboard::new();
    let holders = "/api/admin/acquisitions/holders?key=soulseek:help";
    assert_eq!(
        send(&dash.app, Method::GET, holders, &[], None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        dash.get("recovery", holders).await.status,
        StatusCode::UNAUTHORIZED
    );
    for who in [None, Some("recovery")] {
        let body = json!({ "key": "soulseek:help", "user": "alice" });
        assert_eq!(
            dash.post(who, "/api/admin/acquisitions/keep", &body).await.status,
            StatusCode::UNAUTHORIZED,
            "as {who:?}"
        );
    }
    // Nothing on the dashboard stops a wait for Lidarr: that call is gone.
    let body = json!({ "key": "soulseek:help", "user": "alice" });
    assert_eq!(
        dash.post(Some("admin"), "/api/admin/acquisitions/stop-waiting", &body)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// The rows say Keep anyway from the holds: a song held while AcoustID is off, and a hearted
/// album holding two tracks with its count, so its button reads "Keep these anyway"; a row with
/// nothing to keep says nothing. The menu lists who may keep each, and keeping for one person
/// saves their keep with the holds before a plain ok, the other holder still waiting.
#[tokio::test]
async fn the_rows_say_who_may_keep_and_the_dashboard_keeps_for_the_person_it_names() {
    let dash = Dashboard::new();
    let song = dash.hold_song();
    let night = dash.hold_track("night", 2);
    dash.hold_track("day", 2);
    dash.state.acquisition_tracker.begin(
        "soulseek",
        "angel",
        None,
        Some("alice"),
        Some("Massive Attack"),
        Some("Angel"),
        None,
    );

    let rows = dash.rows().await;
    assert_eq!(rows["soulseek:help"]["keepAnyway"], true);
    assert!(rows["soulseek:help"].get("heldTracks").is_none(), "a song row");
    assert_eq!(rows["soulseek:help-album"]["keepAnyway"], true);
    assert_eq!(rows["soulseek:help-album"]["heldTracks"], 2);
    assert!(
        rows["soulseek:angel"].get("keepAnyway").is_none(),
        "nothing of it is held"
    );
    assert!(
        rows.values()
            .all(|row| row.get("keepers").is_none() && row.get("requestedBy").is_none()),
        "the list names nobody while Record who asked is off"
    );
    assert!(
        rows.values().all(|row| row.get("stopWaiting").is_none()),
        "no row offers to stop waiting for Lidarr"
    );

    // Record who asked is off: the menu names nobody, each person a numbered listener with a token.
    let menu = dash.holders("soulseek:help").await;
    assert_eq!(menu["key"], "soulseek:help");
    assert_eq!(labels(&menu["keepers"]), ["Listener 1", "Listener 2"]);
    assert!(menu.get("stopWaiters").is_none(), "keepers is the one menu");
    let text = menu.to_string();
    assert!(!text.contains("alice") && !text.contains("bob"), "{text}");
    assert_ne!(token_of(&menu["keepers"], 0), token_of(&menu["keepers"], 1));
    assert_eq!(
        labels(&dash.holders("soulseek:help-album").await["keepers"]),
        ["Listener 1"]
    );

    let board = &dash;
    let keep = |body: Value| async move {
        board
            .post(Some("admin"), "/api/admin/acquisitions/keep", &body)
            .await
    };
    let unnamed = keep(json!({ "key": "soulseek:help" })).await;
    assert_eq!(unnamed.status, StatusCode::BAD_REQUEST);
    assert_eq!(unnamed.json()["error"], NAME_THE_PERSON);
    let stranger = keep(json!({ "key": "soulseek:help", "user": "carol" })).await;
    assert_eq!(stranger.status, StatusCode::CONFLICT);
    assert_eq!(stranger.json()["error"], NOTHING_WAITING);
    // A token is good for its own row only.
    let elsewhere =
        keep(json!({ "key": "soulseek:help-album", "token": token_of(&menu["keepers"], 1) })).await;
    assert_eq!(elsewhere.status, StatusCode::CONFLICT);
    assert_eq!(elsewhere.json()["error"], NOTHING_WAITING);

    let kept = keep(json!({ "key": "soulseek:help", "token": token_of(&menu["keepers"], 1) })).await;
    assert_eq!(kept.status, StatusCode::OK, "{}", kept.text());
    assert_eq!(kept.json(), json!({ "status": "ok" }));
    let saved = dash
        .state
        .soulseek_holds
        .get(&song.key())
        .expect("held while it is carried out");
    assert_eq!(saved.keep_requested, ["bob"], "saved before the answer");
    assert_eq!(
        labels(&dash.holders("soulseek:help").await["keepers"]),
        ["Listener 1"]
    );
    let again = keep(json!({ "key": "soulseek:help", "user": " bob " })).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "bob asked already");

    // With Record who asked on, the menu names each person.
    dash.record_who_asked(true);
    assert_eq!(labels(&dash.holders("soulseek:help").await["keepers"]), ["alice"]);

    // A held track's download log offers what its holds offer now.
    let log_row = || async {
        dash.get("admin", "/api/admin/downloads/log?key=soulseek:night")
            .await
            .json()["row"]
            .clone()
    };
    assert_eq!(log_row().await["keepAnyway"], true);

    let album = keep(json!({ "key": "soulseek:help-album", "user": "alice" })).await;
    assert_eq!(album.status, StatusCode::OK, "{}", album.text());
    assert!(
        log_row().await.get("keepAnyway").is_none(),
        "the keep is asked, so the log's row offers it no more"
    );
    assert_eq!(
        dash.state
            .soulseek_holds
            .get(&night.key())
            .expect("held")
            .keep_requested,
        ["alice"]
    );
    assert!(
        dash.rows().await["soulseek:help-album"]
            .get("keepAnyway")
            .is_none(),
        "the album's keep is asked, so its row offers it no more"
    );
}

/// A song whose kept copy plays from where it waits for Lidarr's download of its artist is done as
/// far as its own download goes, yet still waits: its row says `waitsAside` in the list and in its
/// log, so In progress keeps showing its line; once the copy no longer waits, the row says it no
/// more, and a row that never waited aside never says it.
#[tokio::test]
async fn a_row_whose_copy_waits_for_lidarr_says_it_waits_aside() {
    use crate::services::lidarr::HELD_FOR_LIDARR;

    let dash = Dashboard::new();
    let tracker = &dash.state.acquisition_tracker;
    tracker.begin(
        "soulseek",
        "angel",
        None,
        Some("alice"),
        Some("Massive Attack"),
        Some("Angel"),
        Some("Mezzanine"),
    );
    // Teardrop's own download is over; its kept copy waits for Lidarr.
    tracker.wait_aside("soulseek", "teardrop", &["alice".to_string()], HELD_FOR_LIDARR);

    let rows = dash.rows().await;
    assert_eq!(rows["soulseek:teardrop"]["state"], "done");
    assert_eq!(rows["soulseek:teardrop"]["waitsAside"], true);
    assert_eq!(rows["soulseek:teardrop"]["note"], HELD_FOR_LIDARR);
    assert!(
        rows["soulseek:angel"].get("waitsAside").is_none(),
        "written only when it waits"
    );
    let log_row = dash
        .get("admin", "/api/admin/downloads/log?key=soulseek:teardrop")
        .await
        .json()["row"]
        .clone();
    assert_eq!(log_row["waitsAside"], true);

    tracker.end_aside("soulseek", "teardrop", None);
    assert!(
        dash.rows().await["soulseek:teardrop"].get("waitsAside").is_none(),
        "the copy no longer waits"
    );
}

/// A follow Octo cannot settle is on Lidarr's card for the admin (never in the signed-out status),
/// with its line and the album "Put Lidarr back" names. Putting it back needs a Navidrome admin's
/// own sign-in and the follow named; a follow Octo can settle by itself is a conflict and nothing
/// changes; the one listed is put back, and the card lists it no more.
#[tokio::test]
async fn lidarrs_card_lists_a_follow_octo_cannot_settle_and_the_admin_puts_lidarr_back() {
    let dash = Dashboard::new();
    *dash.waits.unsettled.lock() = vec![UnsettledFollow {
        follow: "rg-mezzanine".into(),
        artist: "Massive Attack".into(),
        album: "Mezzanine".into(),
    }];

    let status = dash.get("admin", "/api/admin/status").await.json();
    assert_eq!(
        status["services"]["lidarr"]["follows"],
        json!([{
            "follow": "rg-mezzanine",
            "artist": "Massive Attack",
            "album": "Mezzanine",
            "text": CANNOT_SEE,
        }])
    );
    let signed_out = send(&dash.app, Method::GET, "/api/admin/status", &[], None)
        .await
        .json();
    assert!(
        signed_out["services"]["lidarr"].get("follows").is_none(),
        "{signed_out}"
    );

    let path = "/api/admin/lidarr/put-back";
    let follow = json!({ "follow": "rg-mezzanine" });
    for who in [None, Some("recovery")] {
        assert_eq!(
            dash.post(who, path, &follow).await.status,
            StatusCode::UNAUTHORIZED,
            "as {who:?}"
        );
    }
    let unnamed = dash.post(Some("admin"), path, &json!({})).await;
    assert_eq!(unnamed.status, StatusCode::BAD_REQUEST);
    let settled = dash
        .post(Some("admin"), path, &json!({ "follow": "rg-protection" }))
        .await;
    assert_eq!(settled.status, StatusCode::CONFLICT);
    assert_eq!(settled.json()["error"], NOTHING_TO_PUT_BACK);
    assert!(dash.waits.put_back.lock().is_empty(), "nothing is put back");

    let done = dash
        .post(Some("admin"), path, &json!({ "follow": " rg-mezzanine " }))
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text());
    assert_eq!(done.json(), json!({ "status": "ok" }));
    assert_eq!(*dash.waits.put_back.lock(), ["rg-mezzanine"]);
    let status = dash.get("admin", "/api/admin/status").await.json();
    assert!(
        status["services"]["lidarr"].get("follows").is_none(),
        "the card lists it no more"
    );
    let again = dash.post(Some("admin"), path, &follow).await;
    assert_eq!(again.status, StatusCode::CONFLICT);
}
