//! The admin API's contract with the dashboard and with the Raw config editor, plus the
//! recorded shapes and refusals of the other settings, browse, Last.fm and status routes.
//!
//! Two of these guard against a drift that has bitten four times: GET settings pre-fills the
//! forms and GET raw-config is what the Raw editor writes back WHOLESALE, so a setting missing
//! from either is a setting the next save silently blanks or deletes. Checking every settings
//! property (here: every field the settings structs serialize) makes the next one fail here.

use axum::http::{Method, StatusCode};
use octo_core::settings::{AppSettings, SubsonicSettings};
use serde_json::Value;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::plumbing::SECRET_PLACEHOLDER;
use super::test_app::{Reply, admin_write, app, get, send};
use crate::app::AppState;
use crate::http::pipeline::App;

/// The test app's settings: Navidrome on a closed port, a synthetic admin password.
fn factory_settings() -> AppSettings {
    AppSettings {
        subsonic: SubsonicSettings {
            url: Some("http://127.0.0.1:1".into()),
            admin_username: Some("admin".into()),
            admin_password: Some("synthetic-admin-password".into()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn factory() -> (AppState, App) {
    factory_at("http://127.0.0.1:1")
}

/// [`factory`] with Navidrome, slskd and the shim at `url`.
fn factory_at(url: &str) -> (AppState, App) {
    let mut settings = factory_settings();
    settings.subsonic.url = Some(url.into());
    let state = AppState::for_tests(settings);
    state.settings.set_raw("Soulseek:BaseUrl", Some(url));
    state.settings.set_raw("YouTube:ShimUrl", Some(url));
    (state.clone(), app(state))
}

/// The settings sections and the property names each class carries, from the settings structs
/// themselves (their serde names are the property names settings.json uses).
fn settable_properties() -> Vec<(String, Vec<String>)> {
    let value = serde_json::to_value(AppSettings::default()).expect("settings serialise");
    value
        .as_object()
        .expect("an object")
        .iter()
        .map(|(section, fields)| {
            (
                section.clone(),
                fields
                    .as_object()
                    .expect("a section object")
                    .keys()
                    .cloned()
                    .collect(),
            )
        })
        .collect()
}

/// The sections left out: who may use the dashboard is not a dashboard setting (it is set in .env
/// and needs a restart), and neither GET lists it.
const NOT_LISTED: [&str; 1] = ["Admin"];

fn missing_from(document: &Value) -> Vec<String> {
    let mut missing = Vec::new();
    for (section, names) in settable_properties() {
        if NOT_LISTED.contains(&section.as_str()) {
            continue;
        }
        // Family is listed only once it is in use (effective_document_keeps_family).
        if section == "Family" && document.get("Family").is_none() {
            continue;
        }
        // So is SpotiFLAC (effective_document_keeps_spotiflac).
        if section == "Spotiflac" && document.get("Spotiflac").is_none() {
            continue;
        }
        // And getting upcoming songs ahead (effective_document_keeps_prefetch).
        if section == "Prefetch" && document.get("Prefetch").is_none() {
            continue;
        }
        // And Listening, once changed from its defaults or saved.
        if section == "Listening" && document.get("Listening").is_none() {
            continue;
        }
        for name in names {
            // The family notices are listed with the Family section.
            if name.starts_with("NotifyFamily") && document.get("Family").is_none() {
                continue;
            }
            if document.get(&section).and_then(|s| s.get(&name)).is_none() {
                missing.push(format!("{section}.{name}"));
            }
        }
    }
    missing
}

#[tokio::test]
async fn get_settings_exposes_every_settings_property() {
    let (_, app) = factory();
    let document = get(&app, "/api/admin/settings").await.json();
    assert_eq!(missing_from(&document), Vec::<String>::new());
}

#[tokio::test]
async fn get_raw_config_exposes_every_settings_property() {
    let (_, app) = factory();
    let reply = get(&app, "/api/admin/raw-config").await;
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert!(reply.text().starts_with("{\n  \"Subsonic\": {\n    \"Url\": "));
    assert_eq!(missing_from(&reply.json()), Vec::<String>::new());
}

/// With Family at its defaults neither document lists it; once it is on (or saved in
/// settings.json) both list every Family property, so a save never drops it.
#[tokio::test]
async fn effective_document_keeps_family() {
    let (_, off) = factory();
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&off, path).await.json();
        assert!(document.get("Family").is_none(), "{path}");
    }

    let mut settings = factory_settings();
    settings.family.enabled = true;
    let on = app(AppState::for_tests(settings));
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&on, path).await.json();
        assert_eq!(document["Family"]["Enabled"], Value::Bool(true), "{path}");
        assert_eq!(document["Family"]["MembersPath"], "Auto", "{path}");
        assert_eq!(missing_from(&document), Vec::<String>::new(), "{path}");
    }

    let (state, saved) = factory();
    let mut file = octo_core::settings::JsonObject::new();
    file.insert(
        "Family".into(),
        super::plumbing::n_obj([("Enabled", octo_core::json::dom::Node::Bool(false))]),
    );
    state.settings_writer.replace(&file).expect("writes");
    let document = get(&saved, "/api/admin/raw-config").await.json();
    assert_eq!(document["Family"]["Enabled"], Value::Bool(false));
    assert_eq!(missing_from(&document), Vec::<String>::new());
}

/// With SpotiFLAC at its defaults neither document lists it; once it is on (or saved in
/// settings.json) both list every SpotiFLAC property, the token as the placeholder.
#[tokio::test]
async fn effective_document_keeps_spotiflac() {
    let (_, off) = factory();
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&off, path).await.json();
        assert!(document.get("Spotiflac").is_none(), "{path}");
    }

    let mut settings = factory_settings();
    settings.spotiflac.enabled = true;
    settings.spotiflac.url = "http://spotiflac:8000".into();
    settings.spotiflac.token = "s3cret".into();
    let on = app(AppState::for_tests(settings));
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&on, path).await.json();
        assert_eq!(document["Spotiflac"]["Enabled"], Value::Bool(true), "{path}");
        assert_eq!(document["Spotiflac"]["Placement"], "AfterSoulseek", "{path}");
        assert_eq!(
            document["Spotiflac"]["Token"],
            super::plumbing::SECRET_PLACEHOLDER,
            "{path}"
        );
        assert_eq!(missing_from(&document), Vec::<String>::new(), "{path}");
    }

    let (state, saved) = factory();
    let mut file = octo_core::settings::JsonObject::new();
    file.insert(
        "Spotiflac".into(),
        super::plumbing::n_obj([("Enabled", octo_core::json::dom::Node::Bool(false))]),
    );
    state.settings_writer.replace(&file).expect("writes");
    let document = get(&saved, "/api/admin/raw-config").await.json();
    assert_eq!(document["Spotiflac"]["Enabled"], Value::Bool(false));
    assert_eq!(missing_from(&document), Vec::<String>::new());
}

/// With Prefetch at its defaults neither document lists it, so the settings answers stay as they
/// were; once it is on (or saved in settings.json) both list every Prefetch property.
#[tokio::test]
async fn effective_document_keeps_prefetch() {
    let (_, off) = factory();
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&off, path).await.json();
        assert!(document.get("Prefetch").is_none(), "{path}");
    }

    let mut settings = factory_settings();
    settings.prefetch.enabled = true;
    let on = app(AppState::for_tests(settings));
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&on, path).await.json();
        assert_eq!(document["Prefetch"]["Enabled"], Value::Bool(true), "{path}");
        assert_eq!(document["Prefetch"]["Scope"], "RadioOnly", "{path}");
        assert_eq!(document["Prefetch"]["Count"], 3, "{path}");
        assert_eq!(
            document["Prefetch"]["PauseWhenStopped"],
            Value::Bool(true),
            "{path}"
        );
        assert_eq!(missing_from(&document), Vec::<String>::new(), "{path}");
    }

    let (state, saved) = factory();
    let mut file = octo_core::settings::JsonObject::new();
    file.insert(
        "Prefetch".into(),
        super::plumbing::n_obj([("Enabled", octo_core::json::dom::Node::Bool(false))]),
    );
    state.settings_writer.replace(&file).expect("writes");
    let document = get(&saved, "/api/admin/raw-config").await.json();
    assert_eq!(document["Prefetch"]["Enabled"], Value::Bool(false));
    assert_eq!(missing_from(&document), Vec::<String>::new());
}

/// The factory configures a synthetic admin password. Neither GET may return it; both return
/// the placeholder the save path swaps back.
#[tokio::test]
async fn admin_password_is_never_returned() {
    for url in ["/api/admin/settings", "/api/admin/raw-config"] {
        let (_, app) = factory();
        let body = get(&app, url).await;
        assert!(!body.text().contains("synthetic-admin-password"), "{url}");
        assert_eq!(
            body.json()["Subsonic"]["AdminPassword"],
            Value::String(SECRET_PLACEHOLDER.into()),
            "{url}"
        );
    }
}

/// If the factory's upstream override ever stops applying, a test would talk to a real
/// server. Fail loudly instead.
#[tokio::test]
async fn test_host_points_at_a_closed_port_not_a_real_server() {
    let (_, app) = factory();
    let document = get(&app, "/api/admin/settings").await.json();
    assert_eq!(document["Subsonic"]["Url"], "http://127.0.0.1:1");
}

#[tokio::test]
async fn settings_list_how_many_stream_sizes_are_learned_a_minute() {
    let (_, app) = factory();
    let document = get(&app, "/api/admin/settings").await.json();
    assert_eq!(document["Subsonic"]["StreamSizeWarmPerMinute"], 5);
}

#[tokio::test]
async fn settings_list_the_apps_given_a_size_on_every_song() {
    let (_, app) = factory();
    let document = get(&app, "/api/admin/settings").await.json();
    assert_eq!(document["Subsonic"]["SizeFlagClients"], "Amperfy");
}

#[tokio::test]
async fn settings_list_how_long_search_waits_for_lengths() {
    let (_, app) = factory();
    let document = get(&app, "/api/admin/settings").await.json();
    assert_eq!(document["Subsonic"]["SearchDurationWaitMs"], 1000);
    assert_eq!(document["Subsonic"]["SearchLastFmWaitMs"], 1000);
}

#[tokio::test]
async fn settings_list_the_sweep_replacing_off_with_ten_a_day() {
    let (_, app) = factory();
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&app, path).await.json();
        let actions = &document["LibraryActions"];
        assert_eq!(actions["ReviewSweepAutoReplace"], false, "{path}");
        assert_eq!(actions["ReviewSweepAutoReplacePerDay"], 10, "{path}");
        assert_eq!(actions["ReviewSweepAutoReplacePreview"], false, "{path}");
    }
}

#[tokio::test]
async fn settings_list_the_year_and_artist_mixes_off_by_default() {
    let (_, app) = factory();
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&app, path).await.json();
        let mixes = &document["GeneratedPlaylists"];
        assert_eq!(mixes["Years"], false, "{path}");
        assert_eq!(mixes["MaxYearPlaylists"], 10, "{path}");
        assert_eq!(mixes["Artists"], false, "{path}");
        assert_eq!(mixes["ArtistCount"], 5, "{path}");
    }
}

#[tokio::test]
async fn settings_report_restart_pending_as_a_list() {
    let (_, app) = factory();
    let reply = get(&app, "/api/admin/settings").await;
    assert_eq!(
        reply.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    let meta = &reply.json()["_meta"];
    assert!(meta["RestartPending"].is_array());
    assert!(meta["ConfigFileValid"].is_boolean());
    assert_eq!(meta["SecretPlaceholder"], SECRET_PLACEHOLDER);
    assert_eq!(meta["Version"], octo_core::VERSION);
    assert_eq!(meta["ConfigFileExists"], false);
}

/// A page on another origin cannot add this header without a preflight Octo refuses, so a
/// write without it is either a cross-site request or a script that did not opt in.
#[tokio::test]
async fn admin_write_without_the_header_is_refused() {
    let (_, app) = factory();
    let reply = send(
        &app,
        Method::POST,
        "/api/admin/soulseek/rejected-peers/clear",
        &[],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

/// Any admin read shows the same.
#[tokio::test]
async fn admin_read_from_another_origin_carries_no_cors_headers() {
    let (_, app) = factory();
    let reply = send(
        &app,
        Method::GET,
        "/api/admin/settings",
        &[("Origin", "http://evil.example")],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!reply.has_cors());
}

#[tokio::test]
async fn admin_preflight_is_not_approved() {
    let (_, app) = factory();
    let reply = send(
        &app,
        Method::OPTIONS,
        "/api/admin/settings",
        &[
            ("Origin", "http://evil.example"),
            ("Access-Control-Request-Method", "POST"),
            ("Access-Control-Request-Headers", "X-Octo-Admin"),
        ],
        None,
    )
    .await;
    assert!(reply.header("access-control-allow-origin").is_none());
    assert!(reply.header("access-control-allow-headers").is_none());
}

/// Subsonic web players on another origin depend on CORS; the guard must not reach past
/// /api/admin.
#[tokio::test]
async fn subsonic_route_from_another_origin_keeps_cors() {
    let (_, app) = factory();
    let reply = send(
        &app,
        Method::GET,
        "/rest/ping.view?u=a&p=b&v=1.16.1&c=test&f=json",
        &[("Origin", "http://player.example")],
        None,
    )
    .await;
    assert!(reply.header("access-control-allow-origin").is_some());
}

// ---- the tag preview cannot be pointed at any file ------------------------------------------

/// With a session, a path that walks out of the music folder is refused before anything is
/// read; the tool cannot be used to read arbitrary files.
#[tokio::test]
async fn tag_preview_path_outside_the_music_folder_is_refused() {
    for path in ["../../etc/passwd", "C:\\Windows\\win.ini", "/etc/passwd"] {
        let (state, app) = factory();
        let token = state.browse_sessions.create("admin");
        let body = serde_json::json!({ "path": path }).to_string();
        let reply = send(
            &app,
            Method::POST,
            "/api/admin/tags/preview",
            &[("X-Octo-Admin", "1"), ("X-Octo-Browse-Token", &token)],
            Some(&body),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path}");
        assert!(reply.text().contains("inside the music folder"), "{path}");
    }
}

#[tokio::test]
async fn tag_preview_neither_a_path_nor_a_name_is_refused() {
    let (state, app) = factory();
    let token = state.browse_sessions.create("admin");
    let reply = send(
        &app,
        Method::POST,
        "/api/admin/tags/preview",
        &[("X-Octo-Admin", "1"), ("X-Octo-Browse-Token", &token)],
        Some(r#"{"artist":"Only an artist"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.text(),
        r#"{"error":"Give a path inside the music folder, or an artist and a title."}"#
    );
}

// ---- The recorded answers of the other routes ------------------------------------------------

#[tokio::test]
async fn tag_preview_without_a_session_is_401_and_an_empty_body_is_a_validation_problem() {
    let (_, app) = factory();
    let reply = admin_write(
        &app,
        Method::POST,
        "/api/admin/tags/preview",
        Some(r#"{"artist":"a","title":"b"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        reply.text(),
        r#"{"error":"Sign in with your Navidrome admin account first."}"#
    );

    let reply = admin_write(&app, Method::POST, "/api/admin/browse/auth", Some("")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.header("content-type"),
        Some("application/problem+json; charset=utf-8")
    );
    let body = reply.json();
    assert_eq!(body["errors"][""][0], "A non-empty request body is required.");
    assert_eq!(body["errors"]["req"][0], "The req field is required.");

    // No JSON content type at all: the input formatter is not chosen.
    let reply = send(
        &app,
        Method::POST,
        "/api/admin/lastfm/radio/refresh",
        &[("X-Octo-Admin", "1")],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

async fn navidrome_login() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .and(body_json(
            serde_json::json!({ "username": "admin", "password": "pw" }),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "isAdmin": true, "token": "x" })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .and(body_json(
            serde_json::json!({ "username": "listener", "password": "pw" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "isAdmin": false })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({ "error": "no" })))
        .mount(&server)
        .await;
    server
}

fn cookie_of(reply: &Reply) -> String {
    let set_cookie = reply.header("set-cookie").expect("a cookie");
    set_cookie
        .split(';')
        .next()
        .and_then(|pair| pair.strip_prefix("octo_browse="))
        .expect("the browse cookie")
        .to_string()
}

#[tokio::test]
async fn browse_auth_signs_an_admin_in_with_a_cookie_and_refuses_everyone_else() {
    let navidrome = navidrome_login().await;
    let mut settings = factory_settings();
    settings.subsonic.url = Some(format!("{}/", navidrome.uri()));
    let app = app(AppState::for_tests(settings));

    for (body, error) in [
        (
            r#"{"username":"","password":""}"#,
            "Username and password are required.",
        ),
        (
            r#"{"username":"admin","password":"nope"}"#,
            "Navidrome rejected those credentials.",
        ),
        (
            r#"{"username":"listener","password":"pw"}"#,
            "That account is not a Navidrome admin.",
        ),
    ] {
        let reply = admin_write(&app, Method::POST, "/api/admin/browse/auth", Some(body)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(reply.json()["error"], error, "{body}");
        assert!(reply.header("set-cookie").is_none());
    }

    // Binding ignores case: the corpus signs in with "Username"/"Password".
    let reply = admin_write(
        &app,
        Method::POST,
        "/api/admin/browse/auth",
        Some(r#"{"Username":"admin","Password":"pw"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), r#"{"ok":true,"user":"admin"}"#);
    let token = cookie_of(&reply);
    assert_eq!(
        reply.header("set-cookie"),
        Some(
            format!("octo_browse={token}; max-age=7776000; path=/api/admin; samesite=strict; httponly")
                .as_str()
        )
    );

    // The cookie signs browse/session in and is renewed; the header alone does not.
    let cookie = format!("octo_browse={token}");
    let reply = send(
        &app,
        Method::GET,
        "/api/admin/browse/session",
        &[("Cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(
        reply.text(),
        r#"{"signedIn":true,"user":"admin","recovery":false,"signInOff":true}"#
    );
    assert_eq!(cookie_of(&reply), token);
    let reply = send(
        &app,
        Method::GET,
        "/api/admin/browse/session",
        &[("X-Octo-Browse-Token", &token)],
        None,
    )
    .await;
    assert_eq!(
        reply.text(),
        r#"{"signedIn":false,"user":null,"recovery":false,"signInOff":true}"#
    );
    assert!(reply.header("set-cookie").is_none());

    // A stale cookie hides a valid header.
    let reply = send(
        &app,
        Method::GET,
        "/api/admin/browse?path=/",
        &[("Cookie", "octo_browse=stale"), ("X-Octo-Browse-Token", &token)],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.text(), r#"{"error":"Browse session required."}"#);
    let reply = send(
        &app,
        Method::GET,
        "/api/admin/browse?path=/",
        &[("X-Octo-Browse-Token", &token)],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let listing = reply.json();
    assert_eq!(listing["path"], "/");
    assert_eq!(listing["containerised"], false);
    assert!(listing["entries"].is_array());

    // Sign-out forgets the session and deletes the cookie.
    let reply = send(
        &app,
        Method::POST,
        "/api/admin/browse/signout",
        &[("X-Octo-Admin", "1"), ("Cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(reply.text(), r#"{"ok":true,"ended":0}"#);
    assert_eq!(
        reply.header("set-cookie"),
        Some("octo_browse=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/api/admin")
    );
    let reply = send(
        &app,
        Method::GET,
        "/api/admin/browse/session",
        &[("Cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(
        reply.text(),
        r#"{"signedIn":false,"user":null,"recovery":false,"signInOff":true}"#
    );
}

#[tokio::test]
async fn browse_auth_without_a_navidrome_url_is_503_and_an_unreachable_one_502() {
    let mut settings = factory_settings();
    settings.subsonic.url = None;
    let app_without = app(AppState::for_tests(settings));
    let reply = admin_write(
        &app_without,
        Method::POST,
        "/api/admin/browse/auth",
        Some(r#"{"username":"a","password":"b"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.text(),
        r#"{"error":"Navidrome URL is not configured yet. Use the recovery code instead."}"#
    );

    let (_, app) = factory();
    let reply = admin_write(
        &app,
        Method::POST,
        "/api/admin/browse/auth",
        Some(r#"{"username":"a","password":"b"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        reply.text(),
        r#"{"error":"Could not reach Navidrome to verify credentials. If it is down, use the recovery code."}"#
    );
}

#[tokio::test]
async fn settings_post_refusals_read_as_recorded() {
    let (_, app) = factory();
    let cases = [
        ("", r#"{"error":"empty body"}"#),
        (
            "{not json",
            r#"{"error":"invalid JSON: 'n' is an invalid start of a property name. Expected a '\"'. LineNumber: 0 | BytePositionInLine: 1."}"#,
        ),
        ("[]", r#"{"error":"invalid JSON: expected object"}"#),
        (
            r#"{"Subsonic":{"AdminPassword":"(saved, not shown)x"}}"#,
            r#"{"error":"Retype the whole admin password; it was added to the hidden placeholder."}"#,
        ),
        (
            r#"{"Subsonic":{"AdminPassword":"(saved, not shown)","AdminUsername":"someone-else"}}"#,
            r#"{"error":"Retype the admin password for the new username."}"#,
        ),
        (
            r#"{"LibraryActions":{"Enabled":true,"AllowedUsers":[]}}"#,
            r#"{"error":"Library actions need at least one allowed user. An empty list means nobody."}"#,
        ),
        (
            r#"{"Genre":{"Mappings":[{"Pattern":"rock"},{"Pattern":"ROCK"}]}}"#,
            r#"{"error":"Duplicate genre rule pattern 'ROCK': only the first could ever fire"}"#,
        ),
        (
            r#"{"LastFm":{"DiscoveryStations":[{"Id":"a","Name":"A","Tags":[]}]}}"#,
            r#"{"error":"A must contain between one and five non-empty tags"}"#,
        ),
        (
            r#"{"LastFm":{"ApiSecret":"(saved, not shown)typed"}}"#,
            r#"{"error":"Retype the whole Last.fm shared secret; it was added to the hidden placeholder."}"#,
        ),
    ];
    for (body, expected) in cases {
        let reply = admin_write(&app, Method::POST, "/api/admin/settings", Some(body)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.text(), expected, "{body}");
    }

    // A value of the wrong kind is refused before validation, with the generic 400 answer.
    let reply = admin_write(
        &app,
        Method::POST,
        "/api/admin/settings",
        Some(r#"{"LibraryActions":{"Enabled":"yes"}}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json()["subsonic-response"]["error"]["message"],
        "Operation not valid"
    );
}

/// The editor sends every action back. Wrong lyrics and Wrong cover are saved only when someone
/// changed them, so an untouched settings file still reads in a build that does not know them.
#[tokio::test]
async fn settings_save_keeps_untouched_lyrics_and_cover_actions_out_of_the_file() {
    let (state, app) = factory();
    let body = r#"{"LibraryActions":{"Actions":[
        {"Action":"Delete","Name":"Delete","Enabled":true,"Rating":1},
        {"Action":"WrongLyrics","Name":"Wrong lyrics","Enabled":false,"Rating":0},
        {"Action":"WrongCover","Name":" Wrong cover ","Enabled":false,"Rating":null}
    ]}}"#;
    let reply = admin_write(&app, Method::POST, "/api/admin/settings", Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let saved = std::fs::read_to_string(state.settings_writer.file_path()).expect("the file was written");
    assert!(saved.contains("\"Delete\""), "{saved}");
    assert!(
        !saved.contains("WrongLyrics") && !saved.contains("WrongCover"),
        "{saved}"
    );

    // Changed from the defaults, they are saved.
    let body = r#"{"LibraryActions":{"Actions":[
        {"Action":"WrongLyrics","Name":"Wrong lyrics","Enabled":true,"Rating":0},
        {"Action":"WrongCover","Name":"Bad art","Enabled":false,"Rating":0}
    ]}}"#;
    let reply = admin_write(&app, Method::POST, "/api/admin/settings", Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let saved = std::fs::read_to_string(state.settings_writer.file_path()).expect("the file was written");
    assert!(
        saved.contains("WrongLyrics") && saved.contains("Bad art"),
        "{saved}"
    );
}

#[tokio::test]
async fn settings_post_merges_the_patch_and_echoes_it_with_secrets_masked() {
    let (state, app) = factory();
    let reply = admin_write(
        &app,
        Method::POST,
        "/api/admin/settings",
        Some(r#"{"Server": {"PublicUrl": "http://parity.example"}, "Subsonic": {"AdminPassword": "(saved, not shown)"}, "_meta": {"x": 1}}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.text(),
        r#"{"ok":true,"persisted":{"Server":{"PublicUrl":"http://parity.example"},"Subsonic":{}}}"#
    );
    let saved = std::fs::read_to_string(state.settings_writer.file_path()).expect("the file was written");
    assert_eq!(
        saved,
        "{\n  \"Server\": {\n    \"PublicUrl\": \"http://parity.example\"\n  },\n  \"Subsonic\": {}\n}"
    );

    let reply = admin_write(
        &app,
        Method::POST,
        "/api/admin/settings",
        Some(r#"{"Subsonic": {"AdminPassword": "new-one"}}"#),
    )
    .await;
    assert_eq!(
        reply.json()["persisted"]["Subsonic"]["AdminPassword"],
        SECRET_PLACEHOLDER
    );
    assert!(
        std::fs::read_to_string(state.settings_writer.file_path())
            .unwrap()
            .contains("new-one")
    );

    // A corrupt file is refused with a 409, and the raw-config PUT is the way out.
    std::fs::write(state.settings_writer.file_path(), "{ broken").unwrap();
    let reply = admin_write(
        &app,
        Method::POST,
        "/api/admin/settings",
        Some(r#"{"Server":{}}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.json()["error"].as_str().unwrap().starts_with(
        "settings.json is not valid JSON, so Octo will not write over it. Fix it in Raw config, or on disk at "
    ));
    assert_eq!(
        get(&app, "/api/admin/settings").await.json()["_meta"]["ConfigFileValid"],
        false
    );
    let reply = admin_write(
        &app,
        Method::PUT,
        "/api/admin/raw-config",
        Some(r#"{"Server":{"PublicUrl":""}}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), r#"{"ok":true,"bytes":41}"#);
}

#[tokio::test]
async fn raw_config_put_refusals_read_as_recorded() {
    let (_, app) = factory();
    for (body, expected) in [
        ("", r#"{"error":"empty body"}"#),
        ("[]", r#"{"error":"invalid JSON: top level must be an object"}"#),
        (
            r#"{"Subsonic":{"AdminPassword":"(saved, not shown)x"}}"#,
            r#"{"error":"Retype the whole admin password; it was added to the hidden placeholder."}"#,
        ),
    ] {
        let reply = admin_write(&app, Method::PUT, "/api/admin/raw-config", Some(body)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.text(), expected, "{body}");
    }
}

#[tokio::test]
async fn config_sources_masks_secrets_and_lists_lists_as_empty() {
    let (state, app) = factory();
    state
        .settings
        .set_raw("Soulseek:Password", Some("a-password-longer-than-sixteen"));
    state.settings.set_raw("Lidarr:ApiKey", Some("abc"));
    let body = get(&app, "/api/admin/config-sources").await.json();
    let rows = body["keys"].as_array().expect("rows");
    let row = |key: &str| rows.iter().find(|r| r["Key"] == key).cloned().expect(key);
    // One mask for every secret, so the page never tells how long one is.
    assert_eq!(row("Soulseek:Password")["Value"], "\u{2022}".repeat(8));
    assert_eq!(row("Soulseek:Password")["IsSecret"], true);
    assert_eq!(row("Lidarr:ApiKey")["Value"], "\u{2022}".repeat(8));
    assert_eq!(row("Soulseek:BaseUrl")["Value"], "http://127.0.0.1:1");
    assert_eq!(row("Soulseek:BaseUrl")["IsSecret"], false);
    assert_eq!(row("Genre:Mappings")["Value"], "");
    assert_eq!(row("ListenBrainz:Token")["IsSecret"], true);
    assert_eq!(row("Lidarr:WebhookSecret")["IsSecret"], true);
    // Every settings key, GeneratedPlaylists:Charts and :ChartCountry included, Liked Songs'
    // three: GeneratedPlaylists:LikedSongs, :LikedSongsName and :LikedSongsMax, the
    // country mixes' four: :CountryMixes, :Countries, :MaxCountryPlaylists and
    // :CountryLookupsPerRefresh, Lidarr:WebhookSecret, Soulseek:Enabled,
    // RadioSources:ListenBrainzRadioMode, Subsonic:StreamSizeWarmPerMinute,
    // Subsonic:SearchDurationWaitMs, Subsonic:SearchLastFmWaitMs, Subsonic:CoverCacheMb,
    // Subsonic:SizeFlagClients, Soulseek:SkipCoversAndInstrumentals and
    // LibraryActions:TagCheckPerHour (in their sections), the year and artist mixes' four:
    // GeneratedPlaylists:Years, :MaxYearPlaylists, :Artists and :ArtistCount, the sweep's
    // replacing two: LibraryActions:ReviewSweepAutoReplace and :ReviewSweepAutoReplacePerDay,
    // then LibraryActions:KeepRatedCopies, Subsonic:DownloadAlbumOnSongStar,
    // Metadata:AnimatedArt and Subsonic:CoverBadge, then the sweep's preview replacing,
    // LibraryActions:ReviewSweepAutoReplacePreview, listed last.
    assert_eq!(rows.len(), 205);
    assert!(!rows.iter().any(|r| {
        r["Key"]
            .as_str()
            .is_some_and(|k| k.starts_with("Imports:Spotify"))
    }));
    assert_eq!(rows[194]["Key"], "GeneratedPlaylists:Years");
    assert_eq!(rows[197]["Key"], "GeneratedPlaylists:ArtistCount");
    assert_eq!(rows[198]["Key"], "LibraryActions:ReviewSweepAutoReplace");
    assert_eq!(rows[199]["Key"], "LibraryActions:ReviewSweepAutoReplacePerDay");
    assert_eq!(rows[200]["Key"], "LibraryActions:KeepRatedCopies");
    assert_eq!(rows[201]["Key"], "Subsonic:DownloadAlbumOnSongStar");
    assert_eq!(rows[202]["Key"], "Metadata:AnimatedArt");
    assert_eq!(rows[203]["Key"], "Subsonic:CoverBadge");
    assert_eq!(rows[204]["Key"], "LibraryActions:ReviewSweepAutoReplacePreview");
    assert_eq!(
        row("LibraryActions:ReviewSweepAutoReplacePreview")["IsSecret"],
        false
    );
    assert_eq!(row("Subsonic:CoverBadge")["IsSecret"], false);
    assert_eq!(row("Subsonic:DownloadAlbumOnSongStar")["IsSecret"], false);
    assert_eq!(
        row("LibraryActions:ReviewSweepAutoReplacePerDay")["IsSecret"],
        false
    );
    assert_eq!(row("GeneratedPlaylists:ArtistCount")["IsSecret"], false);
    assert_eq!(row("LibraryActions:TagCheckPerHour")["IsSecret"], false);
    assert_eq!(row("Soulseek:SkipCoversAndInstrumentals")["IsSecret"], false);
    assert_eq!(row("LibraryActions:KeepRatedCopies")["IsSecret"], false);
    assert_eq!(row("Subsonic:SizeFlagClients")["IsSecret"], false);
    assert_eq!(row("Subsonic:StreamSizeWarmPerMinute")["IsSecret"], false);
    assert_eq!(row("Subsonic:CoverCacheMb")["IsSecret"], false);
    assert_eq!(row("Subsonic:SearchDurationWaitMs")["IsSecret"], false);
    assert_eq!(row("Subsonic:SearchLastFmWaitMs")["IsSecret"], false);
    assert_eq!(row("GeneratedPlaylists:CountryMixes")["IsSecret"], false);
    assert_eq!(row("GeneratedPlaylists:LikedSongsName")["IsSecret"], false);
    assert_eq!(row("RadioSources:ListenBrainzRadioMode")["IsSecret"], false);
    assert!(body["configFile"].as_str().unwrap().ends_with("settings.json"));
}

#[tokio::test]
async fn last_fm_and_listen_brainz_reads_have_the_recorded_shapes() {
    // The recorded stack's radio switches (compose turns them off).
    let mut settings = factory_settings();
    settings.last_fm.enable_radio = false;
    settings.last_fm.enable_personalized_stations = false;
    settings.last_fm.enable_discovery_stations = false;
    settings.last_fm.expose_radio_as_playlists = false;
    settings.last_fm.expose_radio_as_streams = false;
    let app = app(AppState::for_tests(settings));
    let radio = get(&app, "/api/admin/lastfm/radio").await;
    assert_eq!(
        radio.text(),
        r#"{"enabled":false,"hasApiKey":false,"personalizedEnabled":false,"discoveryEnabled":false,"playlistsEnabled":false,"streamsEnabled":false,"streamBitrateKbps":192,"icyMetadataEnabled":true,"minimumPlays":10,"selectedUser":null,"users":[],"learning":null,"stations":[]}"#
    );
    let radio = get(&app, "/api/admin/lastfm/radio?user=admin").await.json();
    assert_eq!(radio["selectedUser"], "admin");
    assert_eq!(
        radio["learning"],
        serde_json::json!({"plays":0,"needed":10,"source":"waiting for completed scrobbles","refreshing":false,"lastRefreshAttemptUtc":null,"lastRefreshSuccessUtc":null,"lastRefreshError":null})
    );

    let scrobble = get(&app, "/api/admin/lastfm/scrobble").await;
    assert_eq!(
        scrobble.text(),
        r#"{"available":true,"hasApiKey":false,"hasApiSecret":false,"enabled":true,"libraryPlays":true,"users":[]}"#
    );

    let empty = get(&app, "/api/admin/listenbrainz/validate").await;
    assert_eq!(
        empty.text(),
        r#"{"configured":false,"valid":false,"detail":"No token configured."}"#
    );
}

#[tokio::test]
async fn radio_refresh_and_reset_refuse_a_missing_user() {
    let (_, app) = factory();
    let reply = admin_write(&app, Method::POST, "/api/admin/lastfm/radio/refresh", Some("{}")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.text(), r#"{"error":"A known Navidrome user is required"}"#);

    let reply = admin_write(
        &app,
        Method::POST,
        "/api/admin/lastfm/radio/refresh",
        Some(r#"{"user":"alice"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.text(), r#"{"ok":true,"queued":true}"#);

    for uri in [
        "/api/admin/lastfm/radio/history",
        "/api/admin/lastfm/radio/history?user=",
    ] {
        let reply = admin_write(&app, Method::DELETE, uri, None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(
            reply.json()["errors"]["user"][0],
            "The user field is required.",
            "{uri}"
        );
    }
    let reply = admin_write(
        &app,
        Method::DELETE,
        "/api/admin/lastfm/radio/history?user=%20",
        None,
    )
    .await;
    assert_eq!(reply.text(), r#"{"error":"A known Navidrome user is required"}"#);
    let reply = admin_write(
        &app,
        Method::DELETE,
        "/api/admin/lastfm/radio/history?user=admin",
        None,
    )
    .await;
    assert_eq!(
        reply.text(),
        r#"{"ok":false,"user":"admin","removedPlays":0,"removedStations":0,"message":"Radio history and generated snapshots were removed. Downloaded music was untouched."}"#
    );
}

#[tokio::test]
async fn the_side_effect_posts_answer_as_recorded() {
    let (_, app) = factory();
    let reply = admin_write(&app, Method::POST, "/api/admin/clear-metadata-cache", None).await;
    assert_eq!(reply.text(), r#"{"cleared":true}"#);
    let reply = admin_write(
        &app,
        Method::POST,
        "/api/admin/soulseek/rejected-peers/clear",
        None,
    )
    .await;
    assert_eq!(reply.text(), r#"{"cleared":0}"#);
    let reply = admin_write(&app, Method::POST, "/api/admin/test-notification", None).await;
    assert_eq!(
        reply.text(),
        r#"{"results":[{"sink":"ntfy","configured":false,"ok":false,"detail":"not configured"},{"sink":"discord","configured":false,"ok":false,"detail":"not configured"}]}"#
    );
    assert_eq!(
        get(&app, "/api/admin/downloads").await.text(),
        r#"{"downloads":[]}"#
    );
    assert_eq!(
        get(&app, "/api/admin/acquisitions").await.text(),
        r#"{"acquisitions":[]}"#
    );

    let reply = admin_write(&app, Method::POST, "/api/admin/lidarr/test", Some("{}")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.text(),
        r#"{"ok":false,"error":"Lidarr URL and API key are required."}"#
    );

    // The answer comes before the shutdown; the test's runtime ends long before it fires.
    let reply = admin_write(&app, Method::POST, "/api/admin/restart", None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.text(), r#"{"ok":true,"message":"restarting"}"#);
}

#[tokio::test]
async fn status_says_a_switched_off_soulseek_is_off_and_optional() {
    let mut settings = factory_settings();
    settings.soulseek.enabled = false;
    let app = app(AppState::for_tests(settings));

    let body = get(&app, "/api/admin/status").await.json();

    assert_eq!(body["services"]["slskd"]["detail"], "Off. Optional.");
    assert_eq!(body["services"]["slskd"]["ok"], true);
    assert_eq!(body["services"]["slskd"]["configured"], false);
    assert_eq!(body["services"]["slskd"]["warning"], false);
}

#[tokio::test]
async fn status_reports_each_service() {
    // A port that refuses at once: the status probes wait out a timeout on one that does not.
    let address = crate::services::test_support::refused_address();
    let (_, app) = factory_at(&format!("http://{address}"));
    let body = get(&app, "/api/admin/status").await.json();
    assert_eq!(
        body["octo"],
        serde_json::json!({"ok":true,"detail":"Octo is responding","warning":false,"configured":true})
    );
    assert_eq!(body["services"]["navidrome"]["ok"], false);
    assert_eq!(
        body["services"]["navidrome"]["detail"],
        format!("Could not connect to {address}: the connection was refused.")
    );
    assert_eq!(body["services"]["lidarr"]["detail"], "Not set up. Optional.");
    assert_eq!(body["services"]["lidarr"]["configured"], false);
    assert_eq!(
        body["services"]["ytDlpShim"]["detail"],
        format!("Could not connect to {address}: the connection was refused.")
    );
    assert_eq!(body["services"]["lastfm"]["configured"], false);
    let time = body["time"].as_str().expect("a time");
    assert!(time.ends_with("+00:00") && time.len() == 33, "{time}");
}

/// With Listening at its defaults neither document lists it, so their answers stay as they
/// were; once it is changed both list it.
#[tokio::test]
async fn effective_document_keeps_listening() {
    let (_, defaults) = factory();
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&defaults, path).await.json();
        assert!(document.get("Listening").is_none(), "{path}");
    }

    let mut settings = factory_settings();
    settings.listening.enabled = false;
    settings.listening.keep_days = 400;
    let off = app(AppState::for_tests(settings));
    for path in ["/api/admin/settings", "/api/admin/raw-config"] {
        let document = get(&off, path).await.json();
        assert_eq!(document["Listening"]["Enabled"], Value::Bool(false), "{path}");
        assert_eq!(document["Listening"]["KeepDays"], 365, "{path}");
        assert_eq!(missing_from(&document), Vec::<String>::new(), "{path}");
    }
}
