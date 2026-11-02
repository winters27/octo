//! The dashboard sign-in. Everything here runs with it ON (the rest of the suite turns it off),
//! against a fake Navidrome that knows these people: admin and listener sign in with "pw";
//! demoted lost the admin role; gone no longer exists; admin2 is an admin until a test demotes
//! them.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use octo_core::common::Clock;
use octo_core::settings::{AppSettings, RadioSourceSettings, SoulseekSettings, SubsonicSettings};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::test_app::{Reply, admin_write, app, get, send};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::{App, app_routes};
use crate::http::static_files::StaticAssets;
use crate::middleware::admin_sign_in_gate::{APP_CALL_ENDPOINTS, OPEN_ENDPOINTS};
use crate::services::admin::{AdminRecoveryCode, SignInThrottle};
use crate::services::test_support::refused_address;

// ---- the fake Navidrome ------------------------------------------------------------------------

/// The few Navidrome answers the sign-in needs; anything else is a 404. Also octo-sonic's
/// `/health`, under `/sonic`.
#[derive(Default)]
struct FakeNavidrome {
    user_list_calls: AtomicUsize,
    user_list_fails: AtomicBool,
    user_list_as_listener: AtomicBool,
    admin2_demoted: AtomicBool,
    non_admin_sees_self: AtomicBool,
    /// An octo-sonic from before transition profiles, with Sounds alike turned off in .env
    /// (RADIO_SOUNDS_ALIKE=false): it reads nothing.
    sonic_off: AtomicBool,
    /// octo-sonic with Sounds alike turned off in .env: it makes transition profiles only.
    sonic_profiles_only: AtomicBool,
}

impl FakeNavidrome {
    async fn serve(self: &Arc<Self>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let address = listener.local_addr().expect("an address");
        let app = Router::new().fallback(navidrome).with_state(Arc::clone(self));
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{address}")
    }

    fn flag(&self, flag: &AtomicBool) -> bool {
        flag.load(Ordering::SeqCst)
    }
}

fn answer(json: String) -> Response {
    ([("content-type", "application/json")], json).into_response()
}

fn subsonic(inner: &str) -> Response {
    answer(format!(
        r#"{{"subsonic-response":{{{inner},"version":"1.16.1"}}}}"#
    ))
}

fn failed(code: i32) -> Response {
    subsonic(&format!(
        r#""status":"failed","error":{{"code":{code},"message":"no"}}"#
    ))
}

async fn navidrome(
    State(fake): State<Arc<FakeNavidrome>>,
    uri: Uri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    if path == "/sonic/health" {
        return if fake.flag(&fake.sonic_off) {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [("content-type", "application/json")],
                r#"{"error":"Sounds alike is off (RADIO_SOUNDS_ALIKE=false)","featuresVersion":2,"off":true,"ok":false}"#,
            )
                .into_response()
        } else if fake.flag(&fake.sonic_profiles_only) {
            answer(r#"{"featuresVersion":2,"ok":true,"soundsAlike":false,"transitionVersion":1}"#.into())
        } else {
            answer(r#"{"featuresVersion":2,"ok":true}"#.into())
        };
    }
    let q: Vec<(String, String)> = url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let value = |key: &str| {
        q.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };

    if path == "/auth/login" {
        let login: Value = serde_json::from_slice(&body).unwrap_or_default();
        let user = login["username"].as_str().unwrap_or("").to_string();
        let password = login["password"].as_str().unwrap_or("");
        if password != "pw" || !["admin", "admin2", "listener"].contains(&user.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let is_admin = user == "admin" || (user == "admin2" && !fake.flag(&fake.admin2_demoted));
        return answer(
            json!({
                "id": user, "name": user, "username": user, "isAdmin": is_admin,
                "token": format!("jwt-{user}"), "subsonicSalt": "salt", "subsonicToken": format!("token-{user}"),
            })
            .to_string(),
        );
    }

    if path.starts_with("/rest/ping") {
        let known = value("p") == "pw" && ["admin", "listener"].contains(&value("u").as_str());
        return if known {
            subsonic(r#""status":"ok""#)
        } else {
            failed(40)
        };
    }

    // As real Navidrome does: getUser answers only for the caller's own name, and anyone else,
    // even asked by an admin, is error 50. (server/subsonic/users.go)
    if path.starts_with("/rest/getUser") {
        if !value("username").eq_ignore_ascii_case(&value("u")) {
            return failed(50);
        }
        return match value("u").as_str() {
            "admin" => subsonic(r#""status":"ok","user":{"adminRole":true}"#),
            "listener" => subsonic(r#""status":"ok","user":{"adminRole":false}"#),
            _ => failed(40),
        };
    }

    // The native user list; an admin's token sees everyone. (server/nativeapi, model/user.go)
    if path == "/api/user" {
        fake.user_list_calls.fetch_add(1, Ordering::SeqCst);
        if fake.flag(&fake.user_list_fails) {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let auth = headers
            .get("x-nd-authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // admin2's own token after Navidrome demoted admin2. Real Navidrome 0.64.2 refuses the
        // list to a non-admin outright (seen on LXC 131); non_admin_sees_self models a server
        // that shows a non-admin only themselves instead.
        if auth == "Bearer jwt-admin2" && fake.flag(&fake.admin2_demoted) {
            return if fake.flag(&fake.non_admin_sees_self) {
                answer(json!([{ "userName": "admin2", "isAdmin": false }]).to_string())
            } else {
                (
                    StatusCode::FORBIDDEN,
                    [("content-type", "application/json")],
                    r#"{"error":"Error reading user: Permission denied"}"#,
                )
                    .into_response()
            };
        }
        if auth != "Bearer jwt-admin" && auth != "Bearer jwt-admin2" {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let users = if fake.flag(&fake.user_list_as_listener) {
            json!([{ "userName": "listener", "isAdmin": false }])
        } else {
            json!([
                { "userName": "admin", "isAdmin": true },
                { "userName": "other-admin", "isAdmin": true },
                { "userName": "listener", "isAdmin": false },
                { "userName": "demoted", "isAdmin": false },
                { "userName": "admin2", "isAdmin": !fake.flag(&fake.admin2_demoted) },
            ])
        };
        return answer(users.to_string());
    }

    StatusCode::NOT_FOUND.into_response()
}

// ---- the factory ------------------------------------------------------------------------------

/// Octo with the sign-in on (or the value given), a fake Navidrome, and its state in a temp
/// folder.
struct SignInFactory {
    navidrome: Arc<FakeNavidrome>,
    state: AppState,
    app: App,
    _directory: tempfile::TempDir,
}

impl SignInFactory {
    async fn new() -> Self {
        Self::with("required", false).await
    }

    async fn with(sign_in: &str, admin_login_in_settings: bool) -> Self {
        Self::with_catalog(sign_in, admin_login_in_settings, None).await
    }

    /// `catalog`: a mock Deezer and a mock Apple, with the catalog stand-in wired between them.
    async fn with_catalog(
        sign_in: &str,
        admin_login_in_settings: bool,
        catalog: Option<(String, String)>,
    ) -> Self {
        let directory = tempfile::tempdir().expect("a temp dir");
        let navidrome = Arc::new(FakeNavidrome::default());
        let url = navidrome.serve().await;
        let closed = format!("http://{}", refused_address());
        let settings = AppSettings {
            subsonic: SubsonicSettings {
                url: Some(url.clone()),
                // The admin login a starter stack writes, which Octo can always log in again with.
                admin_username: admin_login_in_settings.then(|| "admin".to_string()),
                admin_password: admin_login_in_settings.then(|| "pw".to_string()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            soulseek: SoulseekSettings {
                base_url: Some(closed.clone()),
                ..Default::default()
            },
            radio_sources: RadioSourceSettings {
                sonic_url: format!("{url}/sonic"),
                ..Default::default()
            },
            ..Default::default()
        };
        let state = AppState::for_tests_with(
            settings,
            TestServices {
                sign_in: Some(sign_in.to_string()),
                config_dir: Some(directory.path().to_path_buf()),
                raw_settings: vec![
                    ("YouTube:ShimUrl".into(), closed.clone()),
                    ("Soulseek:BaseUrl".into(), closed),
                    ("Subsonic:Url".into(), url),
                ],
                deezer_base_url: catalog.as_ref().map(|(deezer, _)| deezer.clone()),
                apple_catalog_base_url: catalog.map(|(_, apple)| apple),
                ..Default::default()
            },
        );
        let app = app(state.clone());
        SignInFactory {
            navidrome,
            state,
            app,
            _directory: directory,
        }
    }

    fn recovery(&self) -> &AdminRecoveryCode {
        &self.state.admin_recovery_code
    }

    async fn sign_in(&self, username: &str, password: &str) -> Reply {
        post_json(
            &self.app,
            "/api/admin/browse/auth",
            &json!({ "username": username, "password": password }),
            &[],
        )
        .await
    }

    async fn get_with_token(&self, path: &str, token: &str) -> Reply {
        send(
            &self.app,
            Method::GET,
            path,
            &[("X-Octo-Browse-Token", token)],
            None,
        )
        .await
    }
}

/// A dashboard write with a JSON body and some headers (a cookie, say).
async fn post_json(app: &App, path: &str, body: &Value, headers: &[(&str, &str)]) -> Reply {
    let mut all = vec![("X-Octo-Admin", "1")];
    all.extend_from_slice(headers);
    send(app, Method::POST, path, &all, Some(&body.to_string())).await
}

/// The `octo_browse` cookie a reply set, as a request's `Cookie` header.
fn cookie_of(reply: &Reply) -> String {
    let set = reply
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("octo_browse="))
        .expect("a sign-in cookie");
    set.split(';').next().expect("a value").to_string()
}

async fn get_cookie(app: &App, path: &str, cookie: &str) -> Reply {
    send(app, Method::GET, path, &[("Cookie", cookie)], None).await
}

// ---- what is open, and everything else is not -------------------------------------------------

#[tokio::test]
async fn only_the_marked_endpoints_answer_without_a_sign_in() {
    let factory = SignInFactory::new().await;
    let templates: Vec<String> = app_routes(&StaticAssets::default())
        .templates()
        .into_iter()
        .filter(|t| t.to_ascii_lowercase().starts_with("/api/admin"))
        .collect();
    assert!(!templates.is_empty());
    // The lists name routes that exist.
    for (_, template) in OPEN_ENDPOINTS {
        assert!(templates.iter().any(|t| t == template), "{template}");
    }
    for (_, template, _) in APP_CALL_ENDPOINTS {
        assert!(templates.iter().any(|t| t == template), "{template}");
    }
    let mut open: Vec<&str> = OPEN_ENDPOINTS.iter().map(|(_, t)| *t).collect();
    open.sort();
    assert_eq!(
        open,
        [
            "/api/admin/auth/recovery",
            "/api/admin/browse/auth",
            "/api/admin/browse/session",
            "/api/admin/browse/signout",
            "/api/admin/status"
        ]
    );

    // Default deny: every method on every admin route, apart from the open ones, asks for a
    // sign-in (a method a route lacks too, which reaches the catch-all).
    let mut checked = 0;
    let parameter = regex::Regex::new(r"\{[^}]+\}").expect("a pattern");
    for template in &templates {
        let path = parameter.replace_all(template, "x").to_string();
        for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
            if OPEN_ENDPOINTS.iter().any(|(m, t)| *m == method && t == template) {
                continue;
            }
            let body = (method != Method::GET).then_some("{}");
            let reply = send(
                &factory.app,
                method.clone(),
                &path,
                &[("X-Octo-Admin", "1")],
                body,
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} answered {} without a sign-in",
                reply.status
            );
            assert_eq!(
                reply.json()["signIn"],
                true,
                "{method} {path} did not ask for a sign-in"
            );
            checked += 1;
        }
    }
    assert!(checked > 200, "only {checked} endpoints were walked");
}

#[tokio::test]
async fn the_upcoming_songs_card_asks_for_a_sign_in() {
    let factory = SignInFactory::new().await;
    assert_eq!(
        get(&factory.app, "/api/admin/prefetch").await.status,
        StatusCode::UNAUTHORIZED
    );
    let clear = send(
        &factory.app,
        Method::POST,
        "/api/admin/prefetch/clear",
        &[("X-Octo-Admin", "1")],
        Some("{}"),
    )
    .await;
    assert_eq!(clear.status, StatusCode::UNAUTHORIZED);
    assert_eq!(clear.json()["signIn"], true);
}

#[tokio::test]
async fn an_unknown_admin_path_and_odd_casing_stay_closed() {
    let factory = SignInFactory::new().await;
    assert_eq!(
        get(&factory.app, "/API/Admin/Settings").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&factory.app, "/api/admin/no-such-thing").await.status,
        StatusCode::UNAUTHORIZED
    );
}

// ---- status -----------------------------------------------------------------------------------

#[tokio::test]
async fn status_tells_anyone_only_whether_each_service_is_up() {
    let factory = SignInFactory::new().await;
    let reply = get(&factory.app, "/api/admin/status").await;
    assert_eq!(reply.status, StatusCode::OK);
    // The shape install.sh greps for.
    assert!(reply.text().contains(r#""navidrome":{"ok":"#), "{}", reply.text());
    let document = reply.json();
    assert!(document.get("octo").is_some());
    for (name, service) in document["services"].as_object().expect("services") {
        let mut keys: Vec<&String> = service.as_object().expect("a service").keys().collect();
        keys.sort();
        assert_eq!(keys, ["configured", "ok"], "{name}");
    }
}

/// A signed-out caller never learns that a Mornye exists, and their status never asks Mornye.
#[tokio::test]
async fn status_never_names_mornye_to_a_signed_out_caller() {
    let factory = SignInFactory::new().await;
    let mornye = wiremock::MockServer::builder().start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(500))
        .expect(0)
        .mount(&mornye)
        .await;
    let mut on = (*factory.state.settings.current()).clone();
    on.spotiflac.enabled = true;
    on.spotiflac.url = mornye.uri();
    factory.state.settings.set(on);

    let document = get(&factory.app, "/api/admin/status").await.json();

    assert!(document["services"].get("mornye").is_none(), "{document}");
    mornye.verify().await;
}

#[tokio::test]
async fn status_gives_a_signed_in_admin_the_details() {
    let factory = SignInFactory::new().await;
    let token = factory.state.browse_sessions.create("admin");
    let services = factory.get_with_token("/api/admin/status", &token).await.json()["services"].clone();
    assert!(services["navidrome"].get("detail").is_some());
}

/// The music catalog's card on a server Deezer does not serve: up for anyone, and the reason for a
/// signed-in admin.
#[tokio::test]
async fn the_music_catalog_says_apple_answers_where_deezer_does_not_serve() {
    let deezer = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/infos"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
            r#"{"country_iso":"IN","country":"India","open":false}"#,
            "application/json",
        ))
        .mount(&deezer)
        .await;
    let apple = wiremock::MockServer::start().await;
    let factory = SignInFactory::with_catalog("required", false, Some((deezer.uri(), apple.uri()))).await;
    factory
        .state
        .deezer_reach
        .as_ref()
        .expect("the stand-in is wired")
        .read_infos()
        .await;

    let anyone = get(&factory.app, "/api/admin/status").await.json();
    assert_eq!(anyone["services"]["catalog"]["ok"], json!(true));
    assert!(
        anyone["services"]["catalog"].get("detail").is_none(),
        "anyone learns only that it is up"
    );

    let token = factory.state.browse_sessions.create("admin");
    let admin = factory.get_with_token("/api/admin/status", &token).await.json();
    let detail = admin["services"]["catalog"]["detail"]
        .as_str()
        .unwrap_or_default();
    assert!(
        detail.contains("Deezer does not serve India") && detail.contains("Apple Music"),
        "{detail}"
    );
}

/// octo-sonic turned off in .env reads as off on the status page, like the switch, not as a
/// service that is down.
#[tokio::test]
async fn octo_sonic_turned_off_in_env_is_off_not_down() {
    for (off, ok, configured, detail) in [
        (true, true, false, "Sounds alike is off in .env"),
        (false, true, true, "reachable"),
    ] {
        let factory = SignInFactory::new().await;
        factory.navidrome.sonic_off.store(off, Ordering::SeqCst);
        let token = factory.state.browse_sessions.create("admin");
        let sonic =
            factory.get_with_token("/api/admin/status", &token).await.json()["services"]["sonic"].clone();
        assert_eq!(sonic["ok"], ok, "{off}");
        assert_eq!(sonic["configured"], configured, "{off}");
        assert_eq!(sonic["detail"], detail, "{off}");
    }
}

/// With Sounds alike off, in .env or on the Radio page, octo-sonic still runs for transition
/// profiles, and the status page says so instead of calling it off.
#[tokio::test]
async fn octo_sonic_with_sounds_alike_off_shows_it_runs_for_transitions() {
    let running = "running for transitions; Sounds alike is off";
    let factory = SignInFactory::new().await;
    factory
        .navidrome
        .sonic_profiles_only
        .store(true, Ordering::SeqCst);
    let token = factory.state.browse_sessions.create("admin");
    let sonic = factory.get_with_token("/api/admin/status", &token).await.json()["services"]["sonic"].clone();
    assert_eq!(
        (sonic["ok"].clone(), sonic["configured"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(sonic["detail"], running);

    let factory = SignInFactory::new().await;
    let mut settings = (*factory.state.settings.current()).clone();
    settings.radio_sources.sounds_alike = false;
    factory.state.settings.set(settings);
    let token = factory.state.browse_sessions.create("admin");
    let sonic = factory.get_with_token("/api/admin/status", &token).await.json()["services"]["sonic"].clone();
    // An octo-sonic that does not say it makes profiles is plainly off.
    assert_eq!(sonic["detail"], "Sounds alike is off");
    assert_eq!(sonic["configured"], false);
    factory
        .navidrome
        .sonic_profiles_only
        .store(true, Ordering::SeqCst);
    let sonic = factory.get_with_token("/api/admin/status", &token).await.json()["services"]["sonic"].clone();
    assert_eq!(sonic["detail"], running);
    assert_eq!(sonic["configured"], true);
}

// ---- signing in with Navidrome ------------------------------------------------------------------

#[tokio::test]
async fn a_navidrome_admin_signs_in_and_the_cookie_opens_the_dashboard() {
    let factory = SignInFactory::new().await;
    let sign_in = factory.sign_in("admin", "pw").await;
    assert_eq!(sign_in.status, StatusCode::OK);
    let set_cookie = sign_in
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(";")
        .to_lowercase();
    for part in ["octo_browse=", "path=/api/admin", "samesite=strict", "httponly"] {
        assert!(set_cookie.contains(part), "{part} in {set_cookie}");
    }
    let cookie = cookie_of(&sign_in);

    assert_eq!(
        get_cookie(&factory.app, "/api/admin/settings", &cookie)
            .await
            .status,
        StatusCode::OK
    );
    let session = get_cookie(&factory.app, "/api/admin/browse/session", &cookie)
        .await
        .json();
    assert_eq!(session["signedIn"], true);
    assert_eq!(session["user"], "admin");
    assert_eq!(session["recovery"], false);
    assert_eq!(session["signInOff"], false);
}

/// Library health answers a signed-in Navidrome admin, and its fixes still go through the library
/// actions gates: off here, so nothing is changed.
#[tokio::test]
async fn library_health_answers_a_signed_in_admin_and_its_fixes_keep_the_library_actions_gates() {
    let factory = SignInFactory::new().await;
    let cookie = cookie_of(&factory.sign_in("admin", "pw").await);

    let report = get_cookie(&factory.app, "/api/admin/health", &cookie).await;
    // The fake Navidrome has no native song list, so the report says it could not read one.
    assert_eq!(report.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        report.json()["error"]
            .as_str()
            .unwrap_or("")
            .contains("could not read your library")
    );
    // Check again asks with ?fresh=1.
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/health?fresh=1", &cookie)
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );

    let apply = post_json(
        &factory.app,
        "/api/admin/health/apply",
        &json!({
            "label": "Fixing copies",
            "check": "duplicates",
            "steps": [{ "action": "remove", "id": "a", "title": "A" }],
        }),
        &[("Cookie", &cookie)],
    )
    .await;
    assert_eq!(apply.status, StatusCode::FORBIDDEN);
    assert_eq!(apply.json()["error"], "Library actions are off.");

    let no_header = send(
        &factory.app,
        Method::POST,
        "/api/admin/health/undo",
        &[("Cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(no_header.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_listener_cannot_sign_in_to_the_dashboard() {
    let factory = SignInFactory::new().await;
    let reply = factory.sign_in("listener", "pw").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap_or("")
            .contains("not a Navidrome admin")
    );
}

#[tokio::test]
async fn ten_wrong_passwords_make_the_address_wait() {
    let factory = SignInFactory::new().await;
    for _ in 0..SignInThrottle::MAX_FAILURES {
        assert_eq!(
            factory.sign_in("admin", "wrong").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    let blocked = factory.sign_in("admin", "pw").await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(blocked.header("retry-after").is_some());
}

// ---- the recovery code ------------------------------------------------------------------------

#[tokio::test]
async fn the_recovery_code_works_once_and_opens_settings_but_not_files() {
    let factory = SignInFactory::new().await;
    let code = factory.recovery().current().expect("a code");

    let first = post_json(
        &factory.app,
        "/api/admin/auth/recovery",
        &json!({ "code": code.to_lowercase().replace('-', " ") }),
        &[],
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    assert_ne!(factory.recovery().current().as_deref(), Some(code.as_str()));
    let cookie = cookie_of(&first);
    assert!(
        first.header("set-cookie").unwrap_or("").contains("max-age=3600"),
        "an hour"
    );

    assert_eq!(
        get_cookie(&factory.app, "/api/admin/settings", &cookie)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/lyrics/library", &cookie)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/browse", &cookie)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/health", &cookie)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_json(
            &factory.app,
            "/api/admin/health/undo",
            &json!({}),
            &[("Cookie", &cookie)]
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/browse/session", &cookie)
            .await
            .json()["recovery"],
        true
    );

    let other = post_json(
        &factory.app,
        "/api/admin/auth/recovery",
        &json!({ "code": code }),
        &[],
    )
    .await;
    assert_eq!(other.status, StatusCode::UNAUTHORIZED);
}

// ---- the Octo app's own sign-in ------------------------------------------------------------------

#[tokio::test]
async fn the_app_signs_its_admin_calls_with_its_subsonic_sign_in() {
    let factory = SignInFactory::new().await;
    let status = |uri: &'static str| {
        let app = factory.app.clone();
        async move { get(&app, uri).await.status }
    };
    assert_eq!(
        status("/api/admin/downloads?u=admin&p=pw&v=1.16.1&c=test").await,
        StatusCode::OK
    );
    assert_eq!(
        status("/api/admin/downloads?u=listener&p=pw&v=1.16.1&c=test").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status("/api/admin/downloads?u=listener&p=bad&v=1.16.1&c=test").await,
        StatusCode::UNAUTHORIZED
    );
    // A fetched song's cover: an admin's call reaches it (no such entry here), a listener's not.
    assert_eq!(
        status("/api/admin/downloads/cover?at=x&u=admin&p=pw&v=1.16.1&c=test").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status("/api/admin/downloads/cover?at=x&u=listener&p=pw&v=1.16.1&c=test").await,
        StatusCode::FORBIDDEN
    );
    // Not an app call, so a Subsonic sign-in opens nothing here.
    assert_eq!(
        status("/api/admin/settings?u=admin&p=pw&v=1.16.1&c=test").await,
        StatusCode::UNAUTHORIZED
    );
}

/// The app's server update panel: a Navidrome admin reads the update card and starts Update now
/// with the app's own sign-in; a listener reaches neither.
#[tokio::test]
async fn an_admin_app_reads_the_update_card_and_asks_for_an_update() {
    let factory = SignInFactory::new().await;
    let card = get(&factory.app, "/api/admin/update?u=admin&p=pw&v=1.16.1&c=test").await;
    assert_eq!(card.status, StatusCode::OK);
    assert!(card.json().get("running").is_some(), "{}", card.text());
    assert!(card.json().get("updateAvailable").is_some(), "{}", card.text());
    assert_eq!(
        get(&factory.app, "/api/admin/update?u=listener&p=pw&v=1.16.1&c=test")
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // Past the gate, the update endpoint itself answers: nothing newer is known here.
    let ask = admin_write(
        &factory.app,
        Method::POST,
        "/api/admin/update?u=admin&p=pw&v=1.16.1&c=test",
        Some(r#"{"tag":"2099.01.01"}"#),
    )
    .await;
    assert_eq!(ask.status, StatusCode::CONFLICT, "{}", ask.text());
    assert_eq!(
        admin_write(
            &factory.app,
            Method::POST,
            "/api/admin/update?u=listener&p=pw&v=1.16.1&c=test",
            Some(r#"{"tag":"2099.01.01"}"#),
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // Asking GitHub again stays the dashboard's.
    assert_eq!(
        admin_write(
            &factory.app,
            Method::POST,
            "/api/admin/update/check?u=admin&p=pw&v=1.16.1&c=test",
            None,
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_listener_sees_and_rebuilds_only_their_own_stations() {
    let factory = SignInFactory::new().await;
    let radio = get(
        &factory.app,
        "/api/admin/lastfm/radio?user=admin&u=listener&p=pw&v=1.16.1&c=test",
    )
    .await
    .json();
    assert_eq!(radio["selectedUser"], "listener");
    for user in radio["users"].as_array().expect("users") {
        assert_eq!(user["username"], "listener");
    }

    let reply = admin_write(
        &factory.app,
        Method::POST,
        "/api/admin/lastfm/radio/refresh?u=listener&p=pw&v=1.16.1&c=test",
        Some(r#"{"user":"admin"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let job = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        factory
            .state
            .last_fm_radio_refresh_queue
            .dequeue(&CancellationToken::new()),
    )
    .await
    .expect("queued in time")
    .expect("a job");
    assert_eq!(job.username, "listener");
}

#[tokio::test]
async fn wrong_app_passwords_count_toward_the_wait() {
    let factory = SignInFactory::new().await;
    for i in 0..SignInThrottle::MAX_FAILURES {
        get(
            &factory.app,
            &format!("/api/admin/downloads?u=admin&p=bad{i}&v=1.16.1&c=test"),
        )
        .await;
    }
    assert_eq!(
        get(&factory.app, "/api/admin/downloads?u=admin&p=pw&v=1.16.1&c=test")
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

// ---- sessions end when Navidrome says so ----------------------------------------------------------

#[tokio::test]
async fn sign_ins_end_when_navidrome_takes_the_admin_role_away() {
    let factory = SignInFactory::new().await;
    // Signing in gives Octo its own admin identity, which the role check asks with.
    let sign_in = factory.sign_in("admin", "pw").await;
    assert_eq!(sign_in.status, StatusCode::OK);
    let admin = cookie_of(&sign_in);

    let sessions = &factory.state.browse_sessions;
    let demoted = sessions.create("demoted");
    let gone = sessions.create("gone");
    assert_eq!(
        factory
            .get_with_token("/api/admin/settings", &demoted)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        factory.get_with_token("/api/admin/settings", &gone).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(sessions.user_of(Some(&demoted)), None);
    assert_eq!(sessions.user_of(Some(&gone)), None);

    // Asked once an hour, not on every call.
    let asked = factory.navidrome.user_list_calls.load(Ordering::SeqCst);
    for _ in 0..2 {
        assert_eq!(
            get_cookie(&factory.app, "/api/admin/settings", &admin)
                .await
                .status,
            StatusCode::OK
        );
    }
    assert_eq!(
        factory.navidrome.user_list_calls.load(Ordering::SeqCst),
        asked + 1
    );
    let later = chrono::Utc::now() + chrono::TimeDelta::minutes(61);
    factory
        .state
        .admin_role_check
        .set_clock(Clock::new(move || later));
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/settings", &admin)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        factory.navidrome.user_list_calls.load(Ordering::SeqCst),
        asked + 2
    );
}

/// Found on a real starter stack: signing in made admin2's own login Octo's admin token, so after
/// the demotion Octo checked admin2 with admin2's token, saw a non-admin's view, and let the
/// session stand. That view is answer enough about admin2 themselves.
#[tokio::test]
async fn a_demoted_admin_is_signed_out_even_when_octos_token_is_their_own() {
    let factory = SignInFactory::new().await;
    let sign_in = factory.sign_in("admin2", "pw").await;
    assert_eq!(sign_in.status, StatusCode::OK);
    let cookie = cookie_of(&sign_in);
    factory.navidrome.admin2_demoted.store(true, Ordering::SeqCst);
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/settings", &cookie)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_demoted_admin_is_signed_out_on_a_server_that_shows_a_non_admin_only_themselves() {
    let factory = SignInFactory::new().await;
    let sign_in = factory.sign_in("admin2", "pw").await;
    assert_eq!(sign_in.status, StatusCode::OK);
    let cookie = cookie_of(&sign_in);
    factory
        .navidrome
        .non_admin_sees_self
        .store(true, Ordering::SeqCst);
    factory.navidrome.admin2_demoted.store(true, Ordering::SeqCst);
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/settings", &cookie)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

/// Octo's token came from admin2, since demoted; checking someone else needs a real admin's
/// view, so Octo logs in again with the admin login in its settings, as a starter stack has.
#[tokio::test]
async fn a_stale_token_is_dropped_and_the_settings_login_asks_again() {
    let factory = SignInFactory::with("required", true).await;
    assert_eq!(factory.sign_in("admin2", "pw").await.status, StatusCode::OK);
    let demoted = factory.state.browse_sessions.create("demoted");
    factory.navidrome.admin2_demoted.store(true, Ordering::SeqCst);
    assert_eq!(
        factory
            .get_with_token("/api/admin/settings", &demoted)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(factory.state.browse_sessions.user_of(Some(&demoted)), None);
}

#[tokio::test]
async fn nobody_is_locked_out_when_navidrome_cannot_say() {
    let factory = SignInFactory::new().await;
    assert_eq!(factory.sign_in("admin", "pw").await.status, StatusCode::OK);

    // The list fails outright.
    factory.navidrome.user_list_fails.store(true, Ordering::SeqCst);
    let first = factory.state.browse_sessions.create("stranger");
    assert_eq!(
        factory.get_with_token("/api/admin/settings", &first).await.status,
        StatusCode::OK
    );

    // The list comes back as a non-admin's view (just one listener): it cannot say anyone is
    // missing.
    factory.navidrome.user_list_fails.store(false, Ordering::SeqCst);
    factory
        .navidrome
        .user_list_as_listener
        .store(true, Ordering::SeqCst);
    let second = factory.state.browse_sessions.create("someone-else");
    assert_eq!(
        factory
            .get_with_token("/api/admin/settings", &second)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        factory.state.browse_sessions.user_of(Some(&second)).as_deref(),
        Some("someone-else")
    );
}

#[tokio::test]
async fn a_browser_that_never_signed_in_is_told_so_without_a_prompt() {
    let factory = SignInFactory::new().await;
    let reply = get(&factory.app, "/api/admin/browse/session").await;
    assert_eq!(reply.status, StatusCode::OK);
    let session = reply.json();
    assert_eq!(session["signedIn"], false);
    assert_eq!(session["signInOff"], false);
}

#[tokio::test]
async fn sign_out_everywhere_ends_every_browser_of_that_person() {
    let factory = SignInFactory::new().await;
    let sign_in = factory.sign_in("admin", "pw").await;
    assert_eq!(sign_in.status, StatusCode::OK);
    let cookie = cookie_of(&sign_in);
    let sessions = &factory.state.browse_sessions;
    let other_browser = sessions.create("admin");
    let someone_else = sessions.create("other-admin");

    let answer = post_json(
        &factory.app,
        "/api/admin/browse/signout?everywhere=true",
        &json!({}),
        &[("Cookie", &cookie)],
    )
    .await
    .json();
    assert_eq!(answer["ended"], 2);
    assert_eq!(sessions.user_of(Some(&other_browser)), None);
    assert_eq!(
        sessions.user_of(Some(&someone_else)).as_deref(),
        Some("other-admin")
    );
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/settings", &cookie)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

/// A sign-out answers with the cookie's deletion alone: the gate's renewal of a signed-in
/// call is left out when the endpoint deletes the cookie, for this browser, everywhere, and the
/// recovery code alike.
#[tokio::test]
async fn a_sign_out_answers_with_the_deletion_alone() {
    let factory = SignInFactory::new().await;
    let code = factory.recovery().current().expect("a code");
    for (path, sign_in) in [
        ("/api/admin/browse/signout", factory.sign_in("admin", "pw").await),
        (
            "/api/admin/browse/signout?everywhere=true",
            factory.sign_in("admin", "pw").await,
        ),
        (
            "/api/admin/browse/signout?everywhere=true",
            post_json(
                &factory.app,
                "/api/admin/auth/recovery",
                &json!({ "code": code }),
                &[],
            )
            .await,
        ),
    ] {
        let cookie = cookie_of(&sign_in);
        let reply = post_json(&factory.app, path, &json!({}), &[("Cookie", &cookie)]).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
        let set: Vec<&str> = reply
            .headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(set.len(), 1, "{path}: {set:?}");
        assert!(set[0].starts_with("octo_browse=;"), "{path}: {set:?}");
    }
}

// ---- headers ----------------------------------------------------------------------------------

#[tokio::test]
async fn the_dashboard_cannot_be_framed_and_admin_answers_are_never_cached() {
    let factory = SignInFactory::new().await;
    let page = get(&factory.app, "/admin/index.html").await;
    assert_eq!(page.header("x-frame-options"), Some("SAMEORIGIN"));
    assert_eq!(
        page.header("content-security-policy"),
        Some("frame-ancestors 'self'")
    );

    let token = factory.state.browse_sessions.create("admin");
    let settings = factory.get_with_token("/api/admin/settings", &token).await;
    assert!(
        settings
            .header("cache-control")
            .unwrap_or("")
            .contains("no-store")
    );
    assert_eq!(settings.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(settings.header("referrer-policy"), Some("same-origin"));
}

// ---- switched off, or switched to something unknown ----------------------------------------------

#[tokio::test]
async fn with_the_sign_in_off_the_dashboard_is_open_but_files_still_ask_for_navidrome() {
    let factory = SignInFactory::with("off", false).await;
    assert_eq!(
        get(&factory.app, "/api/admin/settings").await.status,
        StatusCode::OK
    );
    assert_eq!(
        get(&factory.app, "/api/admin/browse/session").await.json()["signInOff"],
        true
    );
    assert_eq!(
        get(&factory.app, "/api/admin/lyrics/library").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&factory.app, "/api/admin/health").await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn an_unknown_sign_in_value_keeps_the_sign_in_on() {
    let factory = SignInFactory::with("of", false).await;
    assert_eq!(
        get(&factory.app, "/api/admin/settings").await.status,
        StatusCode::UNAUTHORIZED
    );
}

// ---- Cookie renewal, recovery sessions and saved keys ----------------------------------------

/// A signed-in call carries the cookie again, ahead of the endpoint's own (the gate's renewal,
/// then `browse`'s), with the recovery code's hour for a recovery session.
#[tokio::test]
async fn the_gate_renews_the_cookie_on_every_signed_in_call() {
    let factory = SignInFactory::new().await;
    let sign_in = factory.sign_in("admin", "pw").await;
    let cookie = cookie_of(&sign_in);
    let reply = get_cookie(&factory.app, "/api/admin/browse", &cookie).await;
    let renewed: Vec<&str> = reply
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    assert_eq!(renewed.len(), 2, "{renewed:?}");
    assert!(renewed.iter().all(|c| c.contains("max-age=7776000")));

    // A header sign-in sets no cookie.
    let token = factory.state.browse_sessions.create("admin");
    let reply = factory.get_with_token("/api/admin/settings", &token).await;
    assert!(reply.header("set-cookie").is_none());
}

/// The signed-out status is kept ten seconds: a second call inside them probes nothing.
#[tokio::test]
async fn the_signed_out_status_is_kept_ten_seconds() {
    let factory = SignInFactory::new().await;
    let first = get(&factory.app, "/api/admin/status").await.text();
    let second = get(&factory.app, "/api/admin/status").await.text();
    assert_eq!(first, second, "the same answer, time included");
}

/// The gate refuses a recovery-code session on every route that reads or changes library
/// files, the explicit marking included, with the cookie renewed as on any signed-in call.
#[tokio::test]
async fn a_recovery_session_never_reaches_a_library_file_route() {
    let factory = SignInFactory::new().await;
    let code = factory.recovery().current().expect("a code");
    let signed = post_json(
        &factory.app,
        "/api/admin/auth/recovery",
        &json!({ "code": code }),
        &[],
    )
    .await;
    let cookie = cookie_of(&signed);

    for (method, path) in [
        (Method::GET, "/api/admin/explicit/backfill"),
        (Method::POST, "/api/admin/explicit/backfill/cancel"),
        (Method::POST, "/api/admin/explicit/backfill"),
        (Method::GET, "/api/admin/health"),
        (Method::GET, "/api/admin/trash"),
        (Method::GET, "/api/admin/lyrics/choices"),
        (Method::GET, "/api/admin/covers/upgrade"),
    ] {
        let reply = send(
            &factory.app,
            method.clone(),
            path,
            &[("Cookie", &cookie), ("X-Octo-Admin", "1")],
            (method == Method::POST).then_some("{}"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{method} {path}");
        assert_eq!(
            reply.json()["error"],
            "Sign in with your Navidrome admin account first.",
            "{method} {path}"
        );
        assert!(
            reply.header("set-cookie").unwrap_or("").contains("max-age=3600"),
            "{method} {path}"
        );
    }
    let browse = get_cookie(&factory.app, "/api/admin/browse", &cookie).await;
    assert_eq!(browse.json()["error"], "Browse session required.");
    let imports = get_cookie(&factory.app, "/api/admin/imports", &cookie).await;
    assert_eq!(
        imports.json()["error"],
        "Sign in with your Navidrome account first."
    );

    // Settings and the rest of the dashboard still answer it.
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/settings", &cookie)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        get_cookie(&factory.app, "/api/admin/downloads", &cookie)
            .await
            .status,
        StatusCode::OK
    );
    // A Navidrome admin's session is not refused by the gate (the route itself answers).
    let admin = factory.state.browse_sessions.create("admin");
    let reply = factory
        .get_with_token("/api/admin/explicit/backfill", &admin)
        .await;
    assert_ne!(reply.status, StatusCode::UNAUTHORIZED);
}

/// `lidarr/test` sends the saved key only to the saved address.
#[tokio::test]
async fn lidarr_test_never_sends_the_saved_key_to_a_new_address() {
    let factory = SignInFactory::with("off", false).await;
    let reply = admin_write(
        &factory.app,
        Method::POST,
        "/api/admin/lidarr/test",
        Some(r#"{"baseUrl":"http://evil.example:8686","apiKey":"(saved, not shown)"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.text(),
        r#"{"ok":false,"error":"Retype the Lidarr API key for the new address."}"#
    );
}

/// A fix run, or its undo, waits for no job writing the music folder: it is refused, naming the job.
#[tokio::test]
async fn library_health_fixes_are_refused_while_a_job_writes_the_music_folder() {
    let factory = SignInFactory::new().await;
    let cookie = cookie_of(&factory.sign_in("admin", "pw").await);
    let mut settings = (*factory.state.settings.current()).clone();
    settings.library_actions.enabled = true;
    settings.library_actions.dry_run = false;
    settings.library_actions.allowed_users = vec!["admin".into()];
    factory.state.settings.set(settings);
    let tidy = factory
        .state
        .library_write_gate
        .begin_job("Tidy artists")
        .await
        .expect("free");

    let apply = post_json(
        &factory.app,
        "/api/admin/health/apply",
        &json!({
            "label": "Fixing copies",
            "check": "duplicates",
            "steps": [{ "action": "remove", "id": "a", "title": "A" }],
        }),
        &[("Cookie", &cookie)],
    )
    .await;
    assert_eq!(apply.status, StatusCode::CONFLICT);
    assert_eq!(
        apply.json()["error"],
        "Tidy artists is changing the music folder right now. Try again when it is done."
    );
    let undo = post_json(
        &factory.app,
        "/api/admin/health/undo",
        &json!({}),
        &[("Cookie", &cookie)],
    )
    .await;
    assert_eq!(undo.status, StatusCode::CONFLICT);
    assert_eq!(undo.json()["error"], apply.json()["error"]);
    assert!(factory.state.library_health.run().is_none());

    drop(tidy);
    let apply = post_json(
        &factory.app,
        "/api/admin/health/apply",
        &json!({
            "label": "Fixing copies",
            "check": "duplicates",
            "steps": [{ "action": "remove", "id": "a", "title": "A" }],
        }),
        &[("Cookie", &cookie)],
    )
    .await;
    assert_eq!(apply.status, StatusCode::ACCEPTED, "{}", apply.text());
}

// ---- Listening ---------------------------------------------------------------------------------

fn listened(id: &str) -> crate::services::listening::Play {
    crate::services::listening::Play {
        song_id: id.into(),
        cover_id: id.into(),
        artist: "Kavinsky".into(),
        title: format!("Song {id}"),
        album: "OutRun".into(),
        seconds: 240,
        is_local: true,
        played_at: chrono::Utc::now() - chrono::TimeDelta::hours(1),
        ..Default::default()
    }
}

#[tokio::test]
async fn listening_needs_a_sign_in_and_a_listener_sees_only_their_own() {
    let factory = SignInFactory::new().await;
    let log = &factory.state.listening;
    assert!(log.record("admin", listened("a")));
    assert!(log.record("listener", listened("b")));

    assert_eq!(
        get(&factory.app, "/api/admin/listening").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        admin_write(&factory.app, Method::POST, "/api/admin/listening/clear", None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    // The app with a listener's sign-in: their own page, whoever it asks for, and no people.
    let mine = get(
        &factory.app,
        "/api/admin/listening?range=7&user=admin&u=listener&p=pw&v=1.16.1&c=test",
    )
    .await;
    assert_eq!(mine.status, StatusCode::OK, "{}", mine.text());
    let mine = mine.json();
    assert_eq!(mine["user"], "listener");
    assert_eq!(mine["everyone"], false);
    assert!(mine["people"].is_null());
    assert_eq!(mine["rollup"]["plays"], 1);
    assert_eq!(mine["rollup"]["topSongs"][0]["songId"], "b");
    // Clearing is the dashboard's alone.
    let refused = admin_write(
        &factory.app,
        Method::POST,
        "/api/admin/listening/clear?u=listener&p=pw&v=1.16.1&c=test",
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(log.plays_of("listener").len(), 1);

    // The dashboard sees everyone, and one person on asking.
    let cookie = cookie_of(&factory.sign_in("admin", "pw").await);
    let all = get_cookie(&factory.app, "/api/admin/listening?range=30", &cookie)
        .await
        .json();
    assert_eq!(all["everyone"], true);
    assert_eq!(all["range"], 30);
    assert_eq!(all["people"].as_array().expect("people").len(), 2);
    assert_eq!(all["rollup"]["plays"], 2);
    let one = get_cookie(&factory.app, "/api/admin/listening?user=listener", &cookie)
        .await
        .json();
    assert_eq!(one["user"], "listener");
    assert_eq!(one["rollup"]["plays"], 1);

    let cleared = post_json(
        &factory.app,
        "/api/admin/listening/clear?user=listener",
        &json!({}),
        &[("Cookie", &cookie)],
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.text());
    assert_eq!(cleared.json()["cleared"], 1);
    assert!(log.plays_of("listener").is_empty());
    assert_eq!(log.plays_of("admin").len(), 1);
}

#[tokio::test]
async fn listening_is_everyones_for_an_admin_app_and_nobodys_for_the_recovery_code() {
    let factory = SignInFactory::new().await;
    let log = &factory.state.listening;
    assert!(log.record("admin", listened("a")));
    assert!(log.record("listener", listened("b")));

    // A Navidrome admin's own sign-in in the app sees everyone.
    let admin = get(&factory.app, "/api/admin/listening?u=admin&p=pw&v=1.16.1&c=test").await;
    assert_eq!(admin.status, StatusCode::OK, "{}", admin.text());
    assert_eq!(admin.json()["everyone"], true);
    assert_eq!(admin.json()["rollup"]["plays"], 2);

    // The recovery code fixes settings; it never reads anyone's listening, or clears it.
    let code = factory.recovery().current().expect("a code");
    let signed = post_json(
        &factory.app,
        "/api/admin/auth/recovery",
        &json!({ "code": code }),
        &[],
    )
    .await;
    assert_eq!(signed.status, StatusCode::OK);
    let cookie = cookie_of(&signed);
    let refused = get_cookie(&factory.app, "/api/admin/listening", &cookie).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let not_cleared = post_json(
        &factory.app,
        "/api/admin/listening/clear",
        &json!({}),
        &[("Cookie", &cookie)],
    )
    .await;
    assert_eq!(not_cleared.status, StatusCode::FORBIDDEN);
    assert_eq!(log.plays_of("listener").len(), 1);
}

#[tokio::test]
async fn listening_is_hidden_while_the_dashboard_sign_in_is_off() {
    let state = AppState::for_tests(AppSettings::default());
    assert!(state.listening.record("admin", listened("a")));
    let app = app(state.clone());
    let refused = get(&app, "/api/admin/listening").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(refused.text().contains("sign-in is off"), "{}", refused.text());
    let not_cleared = admin_write(&app, Method::POST, "/api/admin/listening/clear", None).await;
    assert_eq!(not_cleared.status, StatusCode::FORBIDDEN);
    assert_eq!(state.listening.plays_of("admin").len(), 1);
}

// ---- Playlists kept ready ----------------------------------------------------------------------

#[tokio::test]
async fn playlists_kept_ready_are_each_persons_and_hidden_while_the_sign_in_is_off() {
    let factory = SignInFactory::new().await;
    let log = &factory.state.listening;
    assert!(log.record("admin", listened("a")));
    assert!(log.record("listener", listened("b")));
    assert_eq!(
        get(&factory.app, "/api/admin/prefetch/keep-ready").await.status,
        StatusCode::UNAUTHORIZED
    );

    // The dashboard: anyone's, with the people to choose from.
    let cookie = cookie_of(&factory.sign_in("admin", "pw").await);
    let theirs = get_cookie(
        &factory.app,
        "/api/admin/prefetch/keep-ready?user=listener",
        &cookie,
    )
    .await;
    assert_eq!(theirs.status, StatusCode::OK, "{}", theirs.text());
    let theirs = theirs.json();
    assert_eq!(theirs["user"], "listener");
    assert_eq!(theirs["mostPlayed"], true);
    assert_eq!(theirs["people"].as_array().expect("people").len(), 2);
    let put = |body: Value| {
        let cookie = cookie.clone();
        let app = factory.app.clone();
        async move {
            send(
                &app,
                Method::PUT,
                "/api/admin/prefetch/keep-ready",
                &[("X-Octo-Admin", "1"), ("Cookie", &cookie)],
                Some(&body.to_string()),
            )
            .await
        }
    };
    let saved = put(json!({ "user": "listener", "lists": [], "mostPlayed": false })).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text());
    assert_eq!(saved.json()["mostPlayed"], false);
    assert_eq!(
        get_cookie(
            &factory.app,
            "/api/admin/prefetch/keep-ready?user=listener",
            &cookie
        )
        .await
        .json()["mostPlayed"],
        false
    );
    let unknown = put(json!({ "user": "listener", "lists": ["oi-nobodys"] })).await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert!(
        unknown.text().contains("not one of listener's playlists"),
        "{}",
        unknown.text()
    );

    // An app's own sign-in does not open it: members choose theirs on the family page.
    let app_call = get(
        &factory.app,
        "/api/admin/prefetch/keep-ready?user=admin&u=listener&p=pw&v=1.16.1&c=test",
    )
    .await;
    assert_eq!(app_call.status, StatusCode::UNAUTHORIZED);

    // With the sign-in off, nobody's.
    let state = AppState::for_tests(AppSettings::default());
    let open = app(state);
    let refused = get(&open, "/api/admin/prefetch/keep-ready").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(refused.text().contains("dashboard sign-in"), "{}", refused.text());
}
