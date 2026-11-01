//! The Test's words for each thing that can be missing, and the client's calls, against a
//! stand-in SpotiFLAC.

use std::sync::Arc;
use std::time::Duration;

use octo_core::settings::{AppSettings, SettingsStore, SpotiflacSettings};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn client(settings: SpotiflacSettings) -> SpotiflacClient {
    SpotiflacClient::with_poll_interval(
        Arc::new(SettingsStore::for_tests(AppSettings {
            spotiflac: settings,
            ..Default::default()
        })),
        Duration::from_millis(5),
    )
}

fn connection(url: &str, token: &str) -> Connection {
    Connection {
        url: url.to_string(),
        token: token.to_string(),
    }
}

async fn spotiflac(info: serde_json::Value, extensions: serde_json::Value) -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(info))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/extensions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(extensions))
        .mount(&server)
        .await;
    server
}

fn info(authenticated: bool, multiuser: bool) -> serde_json::Value {
    json!({"name": "SpotiFLAC", "version": "5.0.5", "api_version": "v1", "multiuser": multiuser, "authenticated": authenticated})
}

fn two_extensions() -> serde_json::Value {
    json!({"providers": [{"provider": "one", "installed": true}, {"provider": "two", "installed": true}], "totals": {}})
}

fn texts(report: &TestReport, state: &str) -> Vec<String> {
    report
        .checks
        .iter()
        .filter(|check| check.state == state)
        .map(|check| check.text.clone())
        .collect()
}

#[tokio::test]
async fn a_test_with_no_address_says_to_type_one() {
    let music = tempfile::tempdir().expect("a folder");
    let report = client(SpotiflacSettings::default())
        .test(&connection("", ""), false, None, &music.path().to_string_lossy())
        .await;
    assert!(!report.ok);
    assert_eq!(
        report.message,
        "No address yet. Type the address of your Mornye server, for example http://spotiflac:8000."
    );
}

#[tokio::test]
async fn a_test_that_reaches_nothing_says_where_it_looked() {
    let music = tempfile::tempdir().expect("a folder");
    let report = client(SpotiflacSettings::default())
        .test(
            &connection("http://127.0.0.1:1", ""),
            true,
            None,
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(!report.ok);
    assert!(
        report
            .message
            .starts_with("Octo could not reach Mornye at http://127.0.0.1:1."),
        "{}",
        report.message
    );
}

#[tokio::test]
async fn a_token_that_cannot_travel_in_a_cookie_is_refused_before_any_call() {
    let server = MockServer::builder().start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let music = tempfile::tempdir().expect("a folder");
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "bad token"),
            true,
            None,
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(!report.ok);
    assert!(
        report.message.starts_with("The web token travels in a cookie"),
        "{}",
        report.message
    );
    server.verify().await;
}

#[tokio::test]
async fn a_wrong_token_is_named() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Missing or invalid ?token="))
        .mount(&server)
        .await;
    let music = tempfile::tempdir().expect("a folder");
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "wrong"),
            true,
            None,
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(!report.ok);
    assert!(
        report.message.contains("refused the web token"),
        "{}",
        report.message
    );
}

#[tokio::test]
async fn an_address_that_is_not_spotiflac_is_named() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>Navidrome</html>"))
        .mount(&server)
        .await;
    let music = tempfile::tempdir().expect("a folder");
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), ""),
            true,
            None,
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(!report.ok);
    assert!(
        report.message.contains("answers, but not as Mornye's web API"),
        "{}",
        report.message
    );
}

#[tokio::test]
async fn no_extensions_says_what_to_bring() {
    let server = spotiflac(info(false, false), json!({"providers": [], "totals": {}})).await;
    let music = tempfile::tempdir().expect("a folder");
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), ""),
            true,
            None,
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(!report.ok);
    assert!(
        report.message.starts_with("Mornye has no extensions installed"),
        "{}",
        report.message
    );
    assert!(
        report.message.contains("SPOTIFLAC_REGISTRIES"),
        "{}",
        report.message
    );
    assert_eq!(report.extensions, Some(0));
    assert!(
        texts(&report, "warn")
            .iter()
            .any(|text| text.contains("no web token")),
        "{:?}",
        report.checks
    );
}

#[tokio::test]
async fn a_multi_user_spotiflac_is_refused() {
    let server = spotiflac(info(true, true), two_extensions()).await;
    let music = tempfile::tempdir().expect("a folder");
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "t"),
            true,
            None,
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(!report.ok);
    assert!(report.message.contains("--web-multiuser"), "{}", report.message);
}

#[tokio::test]
async fn a_mornye_answers_by_its_own_name() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/info"))
        .and(header("cookie", "mornye_web_token=s3cret; spotiflac_web_token=s3cret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "Mornye", "version": "1.0.3", "api_version": "v1", "multiuser": false, "authenticated": true
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/extensions"))
        .and(header(
            "cookie",
            "mornye_web_token=s3cret; spotiflac_web_token=s3cret",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(two_extensions()))
        .mount(&server)
        .await;
    let music = tempfile::tempdir().expect("a folder");
    let root = music.path().to_string_lossy().into_owned();
    let report = client(SpotiflacSettings::default())
        .test(&connection(&server.uri(), "s3cret"), true, None, &root)
        .await;
    assert!(report.ok, "{:?}", report.checks);
    assert_eq!(report.version.as_deref(), Some("1.0.3"));
    assert!(
        texts(&report, "ok")
            .iter()
            .any(|text| text.starts_with("Mornye 1.0.3 answers at"))
    );
}

/// A stand-in Mornye's library scan: `/downloads` on its side is `music` on Octo's, and it counts
/// the song files it finds in the one folder asked about, as Mornye does.
struct LooksInOctosFolder {
    music: std::path::PathBuf,
}

impl wiremock::Respond for LooksInOctosFolder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let asked: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        assert_eq!(asked["recursive"], json!(false), "one folder, not its subfolders");
        let seen = asked["path"]
            .as_str()
            .and_then(|theirs| theirs.strip_prefix("/downloads/"))
            .and_then(|rest| std::fs::read_dir(self.music.join(rest)).ok())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "flac"))
                    .count()
            })
            .unwrap_or(0);
        ResponseTemplate::new(200).set_body_json(json!({
            "target": "HI_RES_LOSSLESS", "scanned": seen, "unreadable": seen, "already_ok": 0, "upgradable": 0, "candidates": []
        }))
    }
}

fn mornye_info(name: &str, version: &str) -> serde_json::Value {
    json!({"name": name, "version": version, "api_version": "v1", "multiuser": false, "authenticated": true})
}

fn mornye_extensions() -> serde_json::Value {
    json!({"providers": [
        {"provider": "ext:example-ext", "installed": true, "attempts": 0, "last_outcome": "", "last_error": ""},
        {"provider": "ext:example-ext-two", "installed": true, "attempts": 0, "last_outcome": "", "last_error": ""}
    ]})
}

fn one_service() -> serde_json::Value {
    json!({"result": {"services": [{"id": "ext:example-ext", "label": "Example", "extensions": ["example-ext"]}]}})
}

fn no_sessions() -> serde_json::Value {
    json!({"result": {"sessions": []}})
}

/// A Mornye with these answers; `scan` answers its library scan.
async fn mornye(
    info: serde_json::Value,
    extensions: serde_json::Value,
    services: serde_json::Value,
    sessions: serde_json::Value,
    scan: impl wiremock::Respond + 'static,
) -> MockServer {
    let server = spotiflac(info, extensions).await;
    Mock::given(method("POST"))
        .and(path("/api/get_download_services"))
        .respond_with(ResponseTemplate::new(200).set_body_json(services))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get_signed_sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sessions))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/library/scan"))
        .respond_with(scan)
        .mount(&server)
        .await;
    server
}

/// The Test of a Mornye whose library scan is `scan`, everything else ready.
async fn test_with_scan(scan: impl wiremock::Respond + 'static) -> (TestReport, tempfile::TempDir) {
    let music = tempfile::tempdir().expect("a folder");
    let server = mornye(
        mornye_info("Mornye", "1.0.5"),
        mornye_extensions(),
        one_service(),
        no_sessions(),
        scan,
    )
    .await;
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "s3cret"),
            true,
            Some("/downloads"),
            &music.path().to_string_lossy(),
        )
        .await;
    (report, music)
}

#[tokio::test]
async fn a_ready_mornye_passes_and_sees_octos_folder() {
    let music = tempfile::tempdir().expect("a folder");
    let root = music.path().to_string_lossy().into_owned();
    let server = mornye(
        mornye_info("Mornye", "1.0.5"),
        mornye_extensions(),
        one_service(),
        no_sessions(),
        LooksInOctosFolder {
            music: music.path().to_path_buf(),
        },
    )
    .await;
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "s3cret"),
            true,
            Some("/downloads"),
            &root,
        )
        .await;
    assert!(report.ok, "{:?}", report.checks);
    assert_eq!(report.message, "Mornye is ready.", "{:?}", report.checks);
    assert_eq!(report.version.as_deref(), Some("1.0.5"));
    assert_eq!(report.extensions, Some(2));
    assert!(
        texts(&report, "ok").contains(&format!(
            "Mornye sees Octo's folder: /downloads/.octo-incoming/spotiflac in Mornye is {root}/.octo-incoming/spotiflac in Octo."
        )),
        "{:?}",
        report.checks
    );
    let left: Vec<_> = std::fs::read_dir(music.path().join(SPOTIFLAC_FOLDER))
        .unwrap()
        .collect();
    assert!(left.is_empty(), "the Test's own folder is removed");
}

#[tokio::test]
async fn a_mornye_that_sees_no_song_in_octos_folder_fails_and_says_where_to_mount() {
    // A Mornye whose /downloads is another disk sees an empty folder.
    let (report, _music) =
        test_with_scan(ResponseTemplate::new(200).set_body_json(json!({"scanned": 0, "unreadable": 0})))
            .await;
    assert!(!report.ok);
    assert!(
        report
            .message
            .starts_with("Mornye does not see Octo's folder at /downloads/.octo-incoming/spotiflac."),
        "{}",
        report.message
    );
}

#[tokio::test]
async fn a_folder_outside_mornyes_download_folder_is_a_warning_naming_both_sides() {
    let outside = json!({"error": "That path is outside this instance's download folder.", "detail": "Scans are confined to the folder this account downloads into."});
    let (report, _music) = test_with_scan(ResponseTemplate::new(400).set_body_json(outside)).await;
    assert!(report.ok, "{:?}", report.checks);
    assert_eq!(report.message, "Mornye is ready, with a warning below.");
    assert!(
        texts(&report, "warn")[0].starts_with(
            "Octo cannot check the folder from here, because it is outside Mornye's own download folder."
        ),
        "{:?}",
        report.checks
    );
}

#[tokio::test]
async fn a_scan_that_is_missing_or_broken_is_a_plain_warning() {
    for answer in [ResponseTemplate::new(404), ResponseTemplate::new(500)] {
        let (report, _music) = test_with_scan(answer).await;
        assert!(report.ok, "{:?}", report.checks);
        let warned = texts(&report, "warn");
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0].starts_with("Octo cannot check the folder from here. Make sure"),
            "{warned:?}"
        );
    }
}

#[tokio::test]
async fn a_ready_mornye_with_the_switch_off_says_so() {
    let music = tempfile::tempdir().expect("a folder");
    let server = mornye(
        mornye_info("Mornye", "1.0.5"),
        mornye_extensions(),
        one_service(),
        no_sessions(),
        LooksInOctosFolder {
            music: music.path().to_path_buf(),
        },
    )
    .await;
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "t"),
            false,
            Some("/downloads"),
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(report.ok);
    assert_eq!(
        report.message, "Mornye is ready, with a warning below.",
        "{:?}",
        report.checks
    );
    assert!(
        texts(&report, "warn")[0].contains("The switch is off"),
        "{:?}",
        report.checks
    );
}

#[tokio::test]
async fn several_warnings_say_warnings() {
    let server = spotiflac(info(true, false), two_extensions()).await;
    let music = tempfile::tempdir().expect("a folder");
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "t"),
            false,
            None,
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(report.ok);
    assert_eq!(
        report.message, "Mornye is ready, with warnings below.",
        "{:?}",
        report.checks
    );
}

#[tokio::test]
async fn an_old_name_or_version_is_told_to_update() {
    for (name, version, said) in [
        (
            "SpotiFLAC",
            "5.0.5",
            "This is SpotiFLAC 5.0.5, from before its rename to Mornye. Run Mornye 1.0.4 or later.",
        ),
        (
            "Mornye",
            "1.0.3",
            "This Mornye is 1.0.3; versions before 1.0.4 cannot pass the check. Update it.",
        ),
    ] {
        let music = tempfile::tempdir().expect("a folder");
        let server = mornye(
            mornye_info(name, version),
            mornye_extensions(),
            one_service(),
            no_sessions(),
            LooksInOctosFolder {
                music: music.path().to_path_buf(),
            },
        )
        .await;
        let report = client(SpotiflacSettings::default())
            .test(
                &connection(&server.uri(), "t"),
                true,
                Some("/downloads"),
                &music.path().to_string_lossy(),
            )
            .await;
        assert_eq!(
            texts(&report, "warn"),
            vec![said.to_string()],
            "{:?}",
            report.checks
        );
    }
}

#[tokio::test]
async fn a_mornye_without_its_utilities_says_that_alone() {
    // Mornye 1.1.0 with a registry that has no utilities extension: no services, and the check
    // read names what is missing.
    let music = tempfile::tempdir().expect("a folder");
    let server = mornye(
        mornye_info("Mornye", "1.1.0"),
        mornye_extensions(),
        json!({"result": {"services": []}}),
        json!({"result": {"sessions": [], "error": "The optional utilities extension is not installed; 'mornye.core.signed_session_status' requires 'utilities.sflx' to be available."}}),
        LooksInOctosFolder {
            music: music.path().to_path_buf(),
        },
    )
    .await;
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "t"),
            true,
            Some("/downloads"),
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(!report.ok);
    assert!(
        report.message.starts_with("Mornye needs its utilities extension"),
        "{}",
        report.message
    );
    assert_eq!(texts(&report, "fail").len(), 1, "{:?}", report.checks);
    assert!(texts(&report, "warn").is_empty(), "{:?}", report.checks);
}

#[tokio::test]
async fn extensions_that_cannot_download_fail_the_test() {
    let music = tempfile::tempdir().expect("a folder");
    let server = mornye(
        mornye_info("Mornye", "1.0.5"),
        mornye_extensions(),
        json!({"result": {"services": []}}),
        no_sessions(),
        LooksInOctosFolder {
            music: music.path().to_path_buf(),
        },
    )
    .await;
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "t"),
            true,
            Some("/downloads"),
            &music.path().to_string_lossy(),
        )
        .await;
    assert!(!report.ok);
    assert_eq!(report.message, "Mornye has extensions but none that downloads.");
}

#[tokio::test]
async fn an_installed_extensions_last_failure_is_warned_and_a_removed_ones_is_not() {
    let music = tempfile::tempdir().expect("a folder");
    let extensions = json!({"providers": [
        {"provider": "example-ext", "installed": false, "attempts": 4, "failures": 3, "last_outcome": "failure",
         "last_error": "EACCES: permission denied \u{2014} open x.flac"},
        {"provider": "ext:example-ext", "installed": true, "attempts": 0, "last_outcome": "", "last_error": ""},
        {"provider": "gone-ext", "installed": false, "attempts": 2, "failures": 2, "last_outcome": "failure", "last_error": "old trouble"}
    ]});
    let server = mornye(
        mornye_info("Mornye", "1.0.5"),
        extensions,
        one_service(),
        no_sessions(),
        LooksInOctosFolder {
            music: music.path().to_path_buf(),
        },
    )
    .await;
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "t"),
            true,
            Some("/downloads"),
            &music.path().to_string_lossy(),
        )
        .await;
    assert_eq!(
        texts(&report, "warn"),
        vec!["example-ext failed its last try: EACCES: permission denied, open x.flac".to_string()],
        "{:?}",
        report.checks
    );
}

#[tokio::test]
async fn a_paused_check_is_warned_with_its_next_try() {
    let music = tempfile::tempdir().expect("a folder");
    let sessions = json!({"result": {"sessions": [
        {"key": "gateway-v2-abc-6836abb49bb0e8c9", "kind": "gateway", "label": "Example", "extensions": ["example-ext"],
         "state": "unverified", "auth_paused_s": 900.0}
    ]}});
    let server = mornye(
        mornye_info("Mornye", "1.0.5"),
        mornye_extensions(),
        one_service(),
        sessions,
        LooksInOctosFolder {
            music: music.path().to_path_buf(),
        },
    )
    .await;
    let report = client(SpotiflacSettings::default())
        .test(
            &connection(&server.uri(), "t"),
            true,
            Some("/downloads"),
            &music.path().to_string_lossy(),
        )
        .await;
    let warned = texts(&report, "warn");
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0].starts_with("Mornye's check for Example is paused; it allows its next try at "),
        "{warned:?}"
    );
}

#[tokio::test]
async fn a_download_is_asked_with_the_quality_the_extensions_and_the_folder() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads"))
        .and(wiremock::matchers::body_json(json!({
            "url": "https://open.spotify.com/track/abc",
            "quality": "HI_RES_LOSSLESS",
            "services": ["ext:one", "ext:two"],
            "output_dir": "/downloads/.octo-incoming/spotiflac/j1",
        })))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_json(json!({"id": "job-1", "status": "queued", "created_at": 1.0})),
        )
        .mount(&server)
        .await;
    let settings = SpotiflacSettings {
        enabled: true,
        url: server.uri(),
        quality: "hi_res_lossless".into(),
        services: "ext:one, ext:two".into(),
        ..Default::default()
    };
    let job = client(settings.clone())
        .queue(
            &connection(&server.uri(), ""),
            "https://open.spotify.com/track/abc",
            &settings,
            "/downloads/.octo-incoming/spotiflac/j1",
        )
        .await
        .expect("queued");
    assert_eq!(job.id, "job-1");
}

#[tokio::test]
async fn spotiflacs_own_error_words_come_through() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads"))
        .respond_with(ResponseTemplate::new(429).set_body_json(
            json!({"error": "Too many downloads already queued.", "detail": "50 queued, limit 50."}),
        ))
        .mount(&server)
        .await;
    let settings = SpotiflacSettings::default();
    let error = client(settings.clone())
        .queue(&connection(&server.uri(), ""), "https://x/y", &settings, "/d")
        .await
        .expect_err("refused");
    assert_eq!(
        error.to_string(),
        "Mornye said: Too many downloads already queued. 50 queued, limit 50."
    );
}

#[tokio::test]
async fn songs_asked_for_at_once_keep_to_the_setting() {
    let client = Arc::new(client(SpotiflacSettings {
        at_once: 1,
        ..Default::default()
    }));
    let first = client.slot().await;
    let waiting = {
        let client = client.clone();
        tokio::spawn(async move {
            let _second = client.slot().await;
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "the second song waits for the first");
    drop(first);
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the second song starts once the first is done")
        .expect("no panic");
}
