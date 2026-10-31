//! The Status page's reads of what Octo's checks by sound cannot do right now: the downloads
//! card's lines (imported downloads going unchecked, with the switch that turns the checks on,
//! AcoustID off, hearts waiting for AcoustID, a refused submission key), the yt-dlp shim's card
//! for a shim too old to check YouTube, and Lidarr's card for hearts waiting on its indexers and
//! for no indexer that can search.

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::Method;
use chrono::Utc;
use octo_core::fingerprint::acoust_id_client::AcoustIdSubmission;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, WaitingFor};
use octo_core::settings::{
    AppSettings, LidarrSettings, SettingsFileWriter, SettingsStore, SoulseekSettings, SubsonicSettings,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::system::{ACOUST_ID_HOLDS, ACOUST_ID_OFF, OLD_SHIM, SUBMISSION_KEY_REFUSED, VERIFICATION_OFF};
use super::test_app::{admin_write, app, get};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;
use crate::services::common::{HeldAcquisition, HeldKind, Holder};
use crate::services::test_support::refused_address;

/// Settings whose services all refuse at once, with the user's own AcoustID key and verification
/// on: nothing for the downloads card to say. The sign-in is off, as the test state sets it, so a
/// test that puts these back still reads the signed-in status.
fn quiet(closed: &str) -> AppSettings {
    let mut settings = AppSettings {
        subsonic: SubsonicSettings {
            url: Some(closed.to_string()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        soulseek: SoulseekSettings {
            acoust_id_api_key: "user-key".into(),
            acoust_id_timeout_seconds: 2,
            verify_downloads: true,
            ..Default::default()
        },
        ..Default::default()
    };
    settings.radio_sources.sonic_url = closed.to_string();
    settings.admin.sign_in = "off".into();
    settings
}

/// The raw keys that point slskd and the shim at `shim` (a closed port unless a test serves one).
fn raw(closed: &str, shim: &str) -> Vec<(String, String)> {
    vec![
        ("Soulseek:BaseUrl".into(), closed.to_string()),
        ("YouTube:ShimUrl".into(), shim.to_string()),
    ]
}

/// The downloads card's lines in the signed-in status answer, as (id, text) pairs.
async fn lines(app: &App) -> Vec<(String, String)> {
    let body = get(app, "/api/admin/status").await.json();
    body["downloads"]
        .as_array()
        .expect("the downloads card's lines")
        .iter()
        .map(|line| {
            (
                line["id"].as_str().expect("an id").to_string(),
                line["text"].as_str().expect("its words").to_string(),
            )
        })
        .collect()
}

fn line(id: &str, text: &str) -> (String, String) {
    (id.to_string(), text.to_string())
}

/// A hold of the song `id` waiting for `waiting_for`, for alice.
fn held(id: &str, waiting_for: WaitingFor) -> HeldAcquisition {
    HeldAcquisition {
        kind: HeldKind::Track,
        provider: "soulseek".into(),
        external_id: id.into(),
        requested_by: Some("alice".into()),
        held_since_utc: Utc::now(),
        origin: Some(AcquisitionOrigin::Heart),
        waiting_for: Some(waiting_for),
        holders: vec![Holder {
            user: "alice".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Each check that cannot run now has its line, and only while it cannot: imported downloads
/// unchecked (with the setting its switch turns on), AcoustID switched off, a heart waiting for
/// AcoustID, and AcoustID refusing the user's submission key. With nothing wrong the card says
/// nothing.
#[tokio::test]
async fn the_downloads_card_carries_a_line_for_each_check_that_cannot_run_now() {
    let closed = format!("http://{}", refused_address());
    let acoust_id = MockServer::start().await;
    Mock::given(path("/v2/submit"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"status":"error","error":{"code":6,"message":"invalid user API key"}}"#),
        )
        .mount(&acoust_id)
        .await;
    let settings = quiet(&closed);
    let state = AppState::for_tests_with(
        settings.clone(),
        TestServices {
            raw_settings: raw(&closed, &closed),
            acoust_id_base_url: Some(format!("{}/", acoust_id.uri())),
            ..Default::default()
        },
    );
    let app = app(state.clone());
    assert_eq!(lines(&app).await, Vec::<(String, String)>::new());

    let mut off = settings.clone();
    off.soulseek.verify_downloads = false;
    state.settings.set(off);
    let body = get(&app, "/api/admin/status").await.json();
    assert_eq!(
        body["downloads"],
        json!([{
            "id": "verificationOff",
            "text": VERIFICATION_OFF,
            "warning": true,
            "turnOn": "Soulseek.VerifyDownloads",
        }])
    );

    let mut no_acoust_id = settings.clone();
    no_acoust_id.soulseek.acoust_id_api_key = "off".into();
    state.settings.set(no_acoust_id);
    assert_eq!(lines(&app).await, [line("acoustIdOff", ACOUST_ID_OFF)]);
    let body = get(&app, "/api/admin/status").await.json();
    assert_eq!(
        body["downloads"][0]["warning"], false,
        "a choice, so it only informs"
    );

    state.settings.set(settings.clone());
    let record = state.soulseek_holds.hold(held("help", WaitingFor::AcoustId));
    state.soulseek_holds.hold(held("angel", WaitingFor::AcoustIdOff));
    assert_eq!(lines(&app).await, [line("acoustIdHolds", ACOUST_ID_HOLDS)]);
    state.soulseek_holds.release(&record.key());
    assert_eq!(
        lines(&app).await,
        Vec::<(String, String)>::new(),
        "a hold of another kind says nothing here"
    );

    let items = [AcoustIdSubmission {
        fingerprint: "AQAD".into(),
        duration_seconds: 200,
        recording_id: "mbid".into(),
        file_format: None,
    }];
    assert!(!state.acoust_id.submit("user-key", "person", &items, 10).await);
    assert_eq!(
        lines(&app).await,
        [line("submissionKeyRefused", SUBMISSION_KEY_REFUSED)]
    );
}

/// An install whose `.env` still says `SLSKD_VERIFY_DOWNLOADS=false` (an old example it kept)
/// shows the line, and its switch, which saves the setting the Verify downloads card saves, turns
/// the checks on over the `.env`, and the line goes.
#[tokio::test]
async fn an_install_whose_env_says_verification_is_off_shows_the_line_and_the_switch_turns_checks_on() {
    let closed = format!("http://{}", refused_address());
    let folder = tempfile::tempdir().expect("a temp folder");
    let settings_file: PathBuf = folder.path().join("settings.json");
    std::fs::write(&settings_file, "{}").expect("the settings file");
    let env: Vec<(String, String)> = [
        ("Subsonic__Url", closed.as_str()),
        ("Subsonic__AutoDetectDownloadPath", "false"),
        ("Soulseek__BaseUrl", closed.as_str()),
        ("Soulseek__AcoustIdApiKey", "user-key"),
        ("Soulseek__VerifyDownloads", "false"),
        ("YouTube__ShimUrl", closed.as_str()),
        ("RadioSources__SonicUrl", closed.as_str()),
        ("Library__DownloadPath", &folder.path().to_string_lossy()),
        ("Admin__SignIn", "off"),
    ]
    .iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    let store = Arc::new(SettingsStore::from_env(env, Some(settings_file.clone())));
    let mut state = AppState::for_tests(quiet(&closed));
    {
        let inner = Arc::get_mut(&mut state.inner).expect("nothing else holds the test state yet");
        inner.settings = Arc::clone(&store);
        inner.settings_writer = Arc::new(SettingsFileWriter::new(&settings_file));
    }
    let app = app(state.clone());
    assert!(!store.current().soulseek.verify_downloads, "the .env's word");

    let body = get(&app, "/api/admin/status").await.json();
    let shown: &Value = &body["downloads"][0];
    assert_eq!(shown["id"], "verificationOff");
    assert_eq!(shown["text"], VERIFICATION_OFF);
    let setting = shown["turnOn"].as_str().expect("the switch's setting");
    let (section, key) = setting.split_once('.').expect("Section.Key");

    // What the switch sends: the setting, on.
    let saved = admin_write(
        &app,
        Method::POST,
        "/api/admin/settings",
        Some(&json!({ section: { key: true } }).to_string()),
    )
    .await;
    assert_eq!(saved.json()["ok"], true, "{}", saved.text());
    assert!(
        store.current().soulseek.verify_downloads,
        "settings.json outranks the .env"
    );
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_file).expect("the settings file"))
            .expect("the settings file reads");
    assert_eq!(file["Soulseek"]["VerifyDownloads"], true);
    assert!(
        lines(&app).await.iter().all(|(id, _)| id != "verificationOff"),
        "the line goes once checks are on"
    );
}

/// The shim's card tells an install with a shim from before fresh lookups (its answer to
/// `fresh=1` carries no header) to update it, and says nothing of the kind for a current shim.
#[tokio::test]
async fn an_old_shim_is_told_to_update_and_a_current_one_is_not() {
    let closed = format!("http://{}", refused_address());
    for (current, expected) in [(false, OLD_SHIM), (true, "HTTP 200")] {
        let shim = MockServer::start().await;
        Mock::given(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&shim)
            .await;
        let refusal = ResponseTemplate::new(400).set_body_string("missing id");
        Mock::given(path("/facts"))
            .and(query_param("fresh", "1"))
            .respond_with(if current {
                refusal.insert_header("X-Octo-Fresh", "1")
            } else {
                refusal
            })
            .mount(&shim)
            .await;
        let state = AppState::for_tests_with(
            quiet(&closed),
            TestServices {
                raw_settings: raw(&closed, &shim.uri()),
                ..Default::default()
            },
        );
        let body = get(&app(state), "/api/admin/status").await.json();
        let card = &body["services"]["ytDlpShim"];
        assert_eq!(card["detail"], expected, "current: {current}");
        assert_eq!(card["ok"], true, "current: {current}");
        assert_eq!(card["warning"], !current, "current: {current}");
        let asked = shim.received_requests().await.expect("recorded");
        assert!(
            asked
                .iter()
                .any(|request| request.url.path() == "/facts"
                    && request.url.query_pairs().all(|(k, _)| k != "id")),
            "the question names no video, so nothing is resolved"
        );
    }
}

/// Lidarr's card, once Lidarr answers: hearts holding for its indexers say so, its health read
/// now saying no indexer searches says that, and both say both. `indexers` marks the warning as
/// one about the indexers, so setup still counts Lidarr as answering. A healthy Lidarr with
/// nothing waiting is reachable.
#[tokio::test]
async fn lidarrs_card_says_hearts_wait_for_its_indexers_and_when_none_can_search() {
    let closed = format!("http://{}", refused_address());
    let lidarr = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"OK"}"#))
        .mount(&lidarr)
        .await;
    let health = |checks: Value| {
        Mock::given(method("GET"))
            .and(path("/api/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(checks))
    };
    let mut settings = quiet(&closed);
    settings.lidarr = LidarrSettings {
        base_url: Some(lidarr.uri()),
        api_key: Some("lidarr-key".into()),
        ..Default::default()
    };
    let state = AppState::for_tests_with(
        settings,
        TestServices {
            raw_settings: raw(&closed, &closed),
            ..Default::default()
        },
    );
    let app = app(state.clone());
    let card = || async {
        let body = get(&app, "/api/admin/status").await.json();
        body["services"]["lidarr"].clone()
    };

    let healthy = health(json!([])).mount_as_scoped(&lidarr).await;
    let quiet_card = card().await;
    assert_eq!(quiet_card["detail"], "reachable");
    assert_eq!(quiet_card["warning"], false);
    assert!(quiet_card.get("indexers").is_none());

    let waiting = state
        .soulseek_holds
        .hold(held("teardrop", WaitingFor::LidarrIndexers));
    let waits = card().await;
    assert_eq!(waits["detail"], "Waiting for Lidarr's indexers");
    assert_eq!(waits["ok"], true);
    assert_eq!(waits["warning"], true);
    assert_eq!(waits["indexers"], true);
    drop(healthy);

    let _none = health(json!([{
        "source": "IndexerSearchCheck",
        "type": "warning",
        "message": "No indexers available with Automatic Search enabled, Lidarr will not provide any automatic search results",
        "wikiUrl": "https://wiki.servarr.com/lidarr/system#no-indexers-available-with-automatic-search-enabled-lidarr-will-not-provide-any-automatic-search-results",
    }]))
    .mount_as_scoped(&lidarr)
    .await;
    assert_eq!(
        card().await["detail"],
        "Waiting for Lidarr's indexers. Lidarr has no indexer that can search"
    );
    state.soulseek_holds.release(&waiting.key());
    let none = card().await;
    assert_eq!(none["detail"], "Lidarr has no indexer that can search");
    assert_eq!(none["warning"], true);
    assert_eq!(none["indexers"], true);
}
