//! The family gate through the whole pipeline, against a Navidrome stood in for by wiremock
//! that signs `alex` in only with the hidden password.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::BodyExt;
use octo_core::family::abilities::{Preset, Role};
use octo_core::family::codes::subsonic_token;
use octo_core::family::quality::{QualityPreference, StreamQuality};
use octo_core::settings::{AppSettings, FamilySettings, SubsonicSettings};
use octo_subsonic::SubsonicCredential;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::app::TestServices;
use crate::http::pipeline::{App, build_with};
use crate::http::routes::RouteSet;
use crate::services::family::family_caller;
use crate::services::family::family_store::SharedLibraries;
use crate::services::family::member_bridge::FailureThrottle;

/// Alex's one login password, whatever app or page signs in with it.
const APP_SECRET: &str = "app-secret-1";
const ACCOUNT_PASSWORD: &str = APP_SECRET;
const HIDDEN: &str = "hidden-1";

/// The id Octo gives an app of Alex's that sends no app name, User-Agent or install id.
fn plain_device() -> String {
    detected_device_id("alex", &Sighting::default())
}

/// A Navidrome sign-in Navidrome would hand out: a JWT shape with `sub` and `exp`.
fn navidrome_jwt(marker: &str) -> String {
    navidrome_jwt_for("alex", marker)
}

fn navidrome_jwt_for(sub: &str, marker: &str) -> String {
    let payload = json!({ "iss": "ND", "sub": sub, "uid": "u-alex", "exp": Utc::now().timestamp() + 3600, "n": marker });
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(payload.to_string()),
        URL_SAFE_NO_PAD.encode(format!("navidrome-signature-{marker}"))
    )
}

/// Matches a request signed in as `alex` with whatever hidden password `accepts` takes.
struct SignedAsAlex {
    accepts: Arc<dyn Fn(&str, &str) -> bool + Send + Sync>,
}

impl wiremock::Match for SignedAsAlex {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let mut pairs: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"))
        {
            pairs.extend(
                url::form_urlencoded::parse(&request.body).map(|(k, v)| (k.into_owned(), v.into_owned())),
            );
        }
        let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let (Some(u), Some(t), Some(s)) = (get("u"), get("t"), get("s")) else {
            return false;
        };
        u == "alex" && get("p").is_none() && (self.accepts)(&t, &s)
    }
}

struct Family {
    server: MockServer,
    state: AppState,
    app: App,
    _dir: tempfile::TempDir,
}

/// What the echo route saw, the parameters a handler reads included.
async fn echo(req: Request) -> Response {
    let caller = family_caller(req.extensions()).cloned();
    let length = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let query = req.uri().query().map(str::to_string);
    let path = req.uri().path().to_string();
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, 1 << 20).await.expect("a body");
    let read_by_gate = GateReading::of(&parts.extensions).is_some();
    let parameters: Vec<Value> = IncomingRequest::from_parts(&parts, body.clone())
        .parameters()
        .iter()
        .map(|(k, v)| json!([k, v]))
        .collect();
    axum::Json(json!({
        "gate": read_by_gate,
        "parameters": parameters,
        "user": caller.as_ref().map(|c| c.user.clone()),
        "managed": caller.as_ref().map(|c| c.managed),
        "role": caller.as_ref().map(|c| format!("{:?}", c.role)),
        "place": caller.as_ref().map(|c| format!("{:?}", c.place)),
        "device": caller.as_ref().and_then(|c| c.device_id.clone()),
        "away": caller.as_ref().map(|c| format!("{:?}", c.quality.away)),
        "deviceQuality": caller.as_ref().map(|c| format!("{:?}", c.device_quality)),
        "length": length,
        "query": query,
        "path": path,
        "body": String::from_utf8_lossy(&body),
    }))
    .into_response()
}

fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("testEcho", get(echo).post(echo))
        .route("/test/echo", get(echo))
}

/// A family of the owner `winters` and the listener `alex` (an Octo app, an app password and an
/// account password), over a Navidrome that takes `alex` only with the hidden password.
async fn family(enabled: bool) -> Family {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                admin_username: Some("admin".into()),
                admin_password: Some("pw".into()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            family: FamilySettings {
                enabled,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            config_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        },
    );
    assert!(state.family_vault.create_key_if_missing().expect("a key"));
    let store = &state.family_store;
    store.set_owner("winters");
    let mut alex = crate::services::family::member_credentials::tests::member(
        "alex",
        state.family_vault.seal(HIDDEN).expect("sealed"),
    );
    alex.display_name = "Alex".into();
    alex.shared_libraries = SharedLibraries::All;
    alex.account_password = passwords::seal(&state.family_vault, ACCOUNT_PASSWORD).expect("sealed");
    store.upsert_member(alex);
    for id in ["d_app", "d_sub"] {
        store.add_device(Device {
            id: id.into(),
            username: "alex".into(),
            name: String::new(),
            created: Utc::now(),
            last_seen: None,
            app: String::new(),
            place: NetworkPlace::Home,
            seen_apps: Vec::new(),
            navidrome_jwt: None,
            quality: Default::default(),
        });
    }
    let app = build_with(state.clone(), routes());
    Family {
        server,
        state,
        app,
        _dir: dir,
    }
}

impl Family {
    /// Navidrome answers `alex` signed in with the password `accepts` takes.
    async fn navidrome_takes(&self, accepts: impl Fn(&str) -> bool + Send + Sync + 'static) {
        Mock::given(SignedAsAlex {
            accepts: Arc::new(move |t, s| {
                // The hidden password is not known here; `accepts` names the candidates.
                ["hidden-1", "hidden-2"]
                    .iter()
                    .any(|candidate| accepts(candidate) && subsonic_token(candidate, s) == t)
                    || accepts(&format!("token:{t}:{s}"))
            }),
        })
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            "application/json",
        ))
        .with_priority(1)
        .mount(&self.server)
        .await;
    }

    /// Everything else signed in to `/rest` gets error 40, as Navidrome answers it.
    async fn navidrome_refuses_the_rest(&self) {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#,
                "application/json",
            ))
            .with_priority(10)
            .mount(&self.server)
            .await;
    }

    async fn rest_requests(&self) -> Vec<wiremock::Request> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.url.path().starts_with("/rest/"))
            .collect()
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

async fn send(app: &App, method: Method, uri: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    send_from(app, method, uri, headers, body, None).await
}

async fn send_from(
    app: &App,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
    peer: Option<&str>,
) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut req = req.body(Body::from(body.to_string())).expect("request");
    if let Some(peer) = peer {
        let address: SocketAddr = format!("{peer}:50000").parse().expect("an address");
        req.extensions_mut().insert(ConnectInfo(address));
    }
    let res = app.clone().oneshot(req).await.expect("infallible");
    let (parts, body) = res.into_parts();
    let body = body.collect().await.expect("body").to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn pairs(query: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The signed-in pairs prove the hidden password.
fn signed_with(pairs: &[(String, String)], hidden: &str) -> bool {
    match (value(pairs, "u"), value(pairs, "t"), value(pairs, "s")) {
        (Some("alex"), Some(t), Some(s)) => subsonic_token(hidden, s) == t && s.len() == 12,
        _ => false,
    }
}

async fn ok_relay(f: &Family) {
    Mock::given(method("GET"))
        .and(path("/rest/getThing"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .with_priority(20)
        .mount(&f.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rest/getThing"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .with_priority(20)
        .mount(&f.server)
        .await;
}

#[tokio::test]
async fn family_off_changes_nothing() {
    let f = family(false).await;
    ok_relay(&f).await;
    let query = "u=alex&p=app-secret-1&id=A&id=B&c=My%20App&v=1.16.1";
    let r = send(&f.app, Method::GET, &format!("/rest/getThing?{query}"), &[], "").await;
    assert_eq!(r.status, StatusCode::OK);
    let body = "u=alex&p=app-secret-1&id=1&id=2";
    let r = send(
        &f.app,
        Method::POST,
        "/rest/getThing?f=json",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        body,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let sent = f.rest_requests().await;
    assert_eq!(sent.len(), 2);
    // Exactly what the client sent, the member's own secret included: with Family off Octo
    // does not know there is a family.
    assert_eq!(sent[0].url.query(), Some(query));
    assert_eq!(sent[1].body, body.as_bytes());
    assert!(String::from_utf8_lossy(&sent[1].body).contains("p=app-secret-1"));
    // And nobody was named on the request.
    let r = send(&f.app, Method::GET, &format!("/rest/testEcho?{query}"), &[], "").await;
    assert_eq!(r.json()["user"], Value::Null);
    assert_eq!(r.json()["query"], json!(query));
    assert!(f.state.family_store.detected().is_empty());
}

#[tokio::test]
async fn member_token_is_rewritten() {
    let f = family(true).await;
    f.navidrome_takes(|hidden| hidden == HIDDEN).await;
    f.navidrome_refuses_the_rest().await;
    let t = subsonic_token(APP_SECRET, "c19b2d");
    let r = send(
        &f.app,
        Method::GET,
        &format!("/rest/getThing?u=alex&t={t}&s=c19b2d&v=1.16.1&c=Octo&f=json&id=A&id=B"),
        &[("User-Agent", "Octo/1.6 (Android)")],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
    let sent = f.rest_requests().await;
    assert_eq!(sent.len(), 1);
    let query = sent[0].url.query().expect("a query");
    // Everything else as the client wrote it, in its order, then the hidden sign-in.
    assert!(
        query.starts_with("v=1.16.1&c=Octo&f=json&id=A&id=B&u=alex&t="),
        "{query}"
    );
    assert!(signed_with(&pairs(query), HIDDEN), "{query}");
    assert!(!query.contains(&t));
    // The password works plain too, and as `enc:` hex.
    for p in [APP_SECRET.to_string(), format!("enc:{}", hex::encode(APP_SECRET))] {
        let r = send(
            &f.app,
            Method::GET,
            &format!("/rest/getThing?u=ALEX&p={p}&f=json"),
            &[],
            "",
        )
        .await;
        assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{p}: {}", r.body);
    }
    // The device was recorded, with its app.
    let device = f
        .state
        .family_store
        .devices_of("alex")
        .into_iter()
        .find(|d| d.seen_apps.iter().any(|a| a == "Octo 1.6 (Android)"))
        .expect("recorded");
    assert!(
        device.seen_apps.iter().any(|a| a == "Octo 1.6 (Android)"),
        "{:?}",
        device.seen_apps
    );
    assert!(device.last_seen.is_some());
}

#[tokio::test]
async fn member_form_body_is_rewritten_and_length_fixed() {
    let f = family(true).await;
    let body = "u=alex&p=app-secret-1&id=1&id=2";
    let r = send(
        &f.app,
        Method::POST,
        "/rest/testEcho?f=json",
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    )
    .await;
    let seen = r.json();
    assert_eq!(seen["user"], "alex");
    assert_eq!(seen["managed"], true);
    assert_eq!(seen["device"], plain_device());
    let sent_body = seen["body"].as_str().expect("a body");
    assert!(sent_body.starts_with("id=1&id=2&u=alex&t="), "{sent_body}");
    assert!(signed_with(&pairs(sent_body), HIDDEN));
    assert!(!sent_body.contains("app-secret"));
    assert_eq!(seen["length"], json!(sent_body.len().to_string()));
    // The query had no sign-in and keeps none.
    assert_eq!(seen["query"], "f=json");
    // Relayed, Navidrome gets the same body.
    f.navidrome_takes(|hidden| hidden == HIDDEN).await;
    let r = send(
        &f.app,
        Method::POST,
        "/rest/getThing?f=json",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        body,
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
    let sent = f.rest_requests().await;
    let upstream = String::from_utf8_lossy(&sent[0].body).into_owned();
    assert!(upstream.starts_with("id=1&id=2&u=alex&t="), "{upstream}");
    assert_eq!(
        sent[0]
            .headers
            .get("content-length")
            .and_then(|v| v.to_str().ok()),
        Some(upstream.len().to_string().as_str())
    );
}

#[tokio::test]
async fn wrong_secret_gets_error_40_without_reaching_navidrome() {
    let f = family(true).await;
    ok_relay(&f).await;
    let r = send(
        &f.app,
        Method::GET,
        "/rest/getThing?u=alex&p=wrong&f=json",
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["error"]["code"], 40);
    let r = send(&f.app, Method::GET, "/rest/getThing?u=alex&p=wrong", &[], "").await;
    assert!(r.body.contains(r#"code="40""#), "{}", r.body);
    assert_eq!(r.headers["content-type"], "application/xml");
    let r = send(
        &f.app,
        Method::GET,
        "/rest/getThing?u=alex&t=00&s=11&f=jsonp&callback=cb",
        &[],
        "",
    )
    .await;
    assert!(r.body.starts_with("cb({\"subsonic-response\""), "{}", r.body);
    assert_eq!(r.headers["content-type"], "application/javascript");
    // Another member's name with a sign-in that is not theirs, two names at once.
    let r = send(
        &f.app,
        Method::GET,
        "/rest/getThing?u=alex&u=winters&p=app-secret-1&f=json",
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["error"]["code"], 40);
    assert!(f.rest_requests().await.is_empty());
    // A paused member is told so, even with the right secret.
    let mut alex = f.state.family_store.member("alex").expect("there");
    alex.suspended = true;
    f.state.family_store.upsert_member(alex);
    let r = send(
        &f.app,
        Method::GET,
        "/rest/getThing?u=alex&p=app-secret-1&f=json",
        &[],
        "",
    )
    .await;
    assert_eq!(
        r.json()["subsonic-response"]["error"]["message"],
        "This account is paused by the family owner."
    );
    assert!(f.rest_requests().await.is_empty());
}

#[tokio::test]
async fn owner_passes_untouched() {
    let f = family(true).await;
    ok_relay(&f).await;
    let query = "u=winters&t=abc&s=def&id=A&id=B&c=Feishin";
    send(&f.app, Method::GET, &format!("/rest/getThing?{query}"), &[], "").await;
    let relayed: Vec<_> = f
        .rest_requests()
        .await
        .into_iter()
        .filter(|r| r.url.path() == "/rest/getThing")
        .collect();
    assert_eq!(relayed[0].url.query(), Some(query));
    // The owner is named, unmanaged, with a detected device.
    let r = send(
        &f.app,
        Method::GET,
        &format!("/rest/testEcho?{query}"),
        &[("X-Octo-Device-Id", "install-1")],
        "",
    )
    .await;
    let seen = r.json();
    assert_eq!(seen["user"], "winters");
    assert_eq!(seen["managed"], false);
    assert_eq!(seen["role"], "Owner");
    assert_eq!(seen["query"], json!(query));
    assert!(seen["device"].as_str().expect("a device").starts_with("x_"));
    // Someone outside the family is named too.
    let r = send(&f.app, Method::GET, "/rest/testEcho?u=bob&p=x", &[], "").await;
    assert_eq!(r.json()["role"], "Unmanaged");
}

#[tokio::test]
async fn failures_are_throttled() {
    let f = family(true).await;
    f.navidrome_takes(|hidden| hidden == HIDDEN).await;
    for _ in 0..member_bridge::MAX_FAILURES {
        let r = send_from(
            &f.app,
            Method::GET,
            "/rest/getThing?u=alex&p=nope&f=json",
            &[],
            "",
            Some("203.0.113.9"),
        )
        .await;
        assert_eq!(r.json()["subsonic-response"]["error"]["code"], 40);
    }
    // The right secret from the same address is now refused without a check.
    let right = "/rest/getThing?u=alex&p=app-secret-1&f=json";
    let r = send_from(&f.app, Method::GET, right, &[], "", Some("203.0.113.9")).await;
    assert_eq!(r.json()["subsonic-response"]["error"]["code"], 40);
    assert!(f.rest_requests().await.is_empty());
    // Another address is not held up.
    let r = send_from(&f.app, Method::GET, right, &[], "", Some("203.0.113.10")).await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
}

#[tokio::test]
async fn api_key_member_is_identified() {
    let f = family(true).await;
    Mock::given(method("GET"))
        .and(path("/rest/tokenInfo"))
        .and(query_param("apiKey", "alex-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","tokenInfo":{"username":"alex"}}}"#,
            "application/json",
        ))
        .mount(&f.server)
        .await;
    f.navidrome_takes(|hidden| hidden == HIDDEN).await;
    let r = send(
        &f.app,
        Method::GET,
        "/rest/getThing?apiKey=alex-key&f=json&id=7",
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
    let sent: Vec<_> = f
        .rest_requests()
        .await
        .into_iter()
        .filter(|r| r.url.path() == "/rest/getThing")
        .collect();
    let query = sent[0].url.query().expect("a query");
    assert!(!query.contains("apiKey"), "{query}");
    assert!(query.starts_with("f=json&id=7&u=alex"), "{query}");
    let r = send(&f.app, Method::GET, "/rest/testEcho?apiKey=alex-key", &[], "").await;
    assert_eq!(r.json()["user"], "alex");
    assert_eq!(r.json()["managed"], true);
}

/// A web device for alex holding the real token `real`, and an Octo token for it.
fn web_device_with(f: &Family, real: &str) -> String {
    f.state.family_store.add_device(Device {
        id: "d_web".into(),
        username: "alex".into(),
        name: "Navidrome in a browser".into(),
        created: Utc::now(),
        last_seen: None,
        app: String::new(),
        place: NetworkPlace::Home,
        seen_apps: Vec::new(),
        navidrome_jwt: f.state.family_vault.seal(real),
        quality: Default::default(),
    });
    let member = f.state.family_store.member("alex").expect("there");
    mint_for(&f.state, &member, "d_web", real, None).expect("a token")
}

#[tokio::test]
async fn jwt_query_member_is_identified() {
    let f = family(true).await;
    f.navidrome_takes(|hidden| hidden == HIDDEN).await;
    let octo = web_device_with(&f, &navidrome_jwt("real"));
    let r = send(
        &f.app,
        Method::GET,
        &format!("/rest/getThing?jwt={octo}&f=json"),
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
    let query = f.rest_requests().await[0]
        .url
        .query()
        .expect("a query")
        .to_string();
    assert!(!query.contains("jwt"), "{query}");
    assert!(signed_with(&pairs(&query), HIDDEN));
    let r = send(
        &f.app,
        Method::GET,
        &format!("/rest/testEcho?jwt={octo}"),
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["device"], "d_web");
    // A token Octo did not sign names nobody as a member.
    let forged = navidrome_jwt("forged");
    let r = send(
        &f.app,
        Method::GET,
        &format!("/rest/testEcho?u=alex&jwt={forged}&f=json"),
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["error"]["code"], 40);
}

async fn login_mock(f: &Family, hidden: &str, real: &str) {
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .and(body_partial_json(
            json!({ "username": "alex", "password": hidden }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "u-alex", "name": "Alex", "username": "alex", "isAdmin": false,
            "token": real, "subsonicSalt": "ndsalt", "subsonicToken": "ndtoken"
        })))
        .mount(&f.server)
        .await;
}

async fn web_login(f: &Family, password: &str) -> Reply {
    send(
        &f.app,
        Method::POST,
        "/auth/login",
        &[("Content-Type", "application/json")],
        &json!({ "username": "alex", "password": password }).to_string(),
    )
    .await
}

#[tokio::test]
async fn member_web_sign_in_returns_device_token() {
    let f = family(true).await;
    let real = navidrome_jwt("real");
    login_mock(&f, HIDDEN, &real).await;
    f.navidrome_takes(|hidden| hidden == HIDDEN).await;
    let r = web_login(&f, ACCOUNT_PASSWORD).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let answer = r.json();
    let token = answer["token"].as_str().expect("a token");
    let key = f.state.family_vault.jwt_key().expect("a key");
    let claims = web_session::verify(&key, token, Utc::now().timestamp()).expect("Octo's");
    assert_eq!(
        (claims.sub.as_str(), claims.uid.as_str(), claims.adm),
        ("alex", "u-alex", false)
    );
    assert_eq!(Some(claims.exp), web_session::unverified_expiry(&real));
    // The Subsonic pair is the account password's, and signs in through the gate.
    let device = f.state.family_store.device(&claims.did).expect("a web device");
    assert!(device.navidrome_jwt.is_some());
    let salt = answer["subsonicSalt"].as_str().expect("a salt");
    assert_eq!(
        answer["subsonicToken"],
        json!(subsonic_token(ACCOUNT_PASSWORD, salt))
    );
    let r = send(
        &f.app,
        Method::GET,
        &format!(
            "/rest/getThing?u=alex&t={}&s={salt}&f=json",
            answer["subsonicToken"].as_str().expect("t")
        ),
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
    // Signing in again reuses the web device.
    let again = web_login(&f, ACCOUNT_PASSWORD).await;
    assert_eq!(again.status, StatusCode::OK);
    let webs: Vec<_> = f
        .state
        .family_store
        .devices_of("alex")
        .into_iter()
        .filter(|d| d.navidrome_jwt.is_some())
        .collect();
    assert_eq!(webs.len(), 1);
    // A wrong password is a 401, and Navidrome is never asked.
    let wrong = web_login(&f, "not it").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    let logins = f
        .server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path() == "/auth/login")
        .count();
    assert_eq!(logins, 2);
}

/// Every header value and the body of a reply, as text.
fn everything(reply: &Reply) -> String {
    let mut text = reply.body.clone();
    for (name, value) in &reply.headers {
        text.push_str(&format!(
            "\n{name}: {}",
            String::from_utf8_lossy(value.as_bytes())
        ));
    }
    text
}

/// Signs alex in on the web player and makes two native calls: the first answers with a
/// refreshed Navidrome token, the second must carry it. Answers every reply.
async fn web_session_round(f: &Family, real: &str, refreshed: &str) -> Vec<Reply> {
    login_mock(f, HIDDEN, real).await;
    Mock::given(method("GET"))
        .and(path("/api/queue"))
        .and(header("X-Nd-Authorization", format!("Bearer {real}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Nd-Authorization", refreshed)
                .set_body_json(json!({ "items": [], "token": "none" })),
        )
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/queue"))
        .and(header(
            "X-Nd-Authorization",
            format!("Bearer {refreshed}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .expect(1)
        .mount(&f.server)
        .await;
    let login = web_login(f, ACCOUNT_PASSWORD).await;
    let token = login.json()["token"].as_str().expect("a token").to_string();
    let first = send(
        &f.app,
        Method::GET,
        "/api/queue",
        &[("X-Nd-Authorization", &format!("Bearer {token}"))],
        "",
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let next = first
        .headers
        .get("x-nd-authorization")
        .and_then(|v| v.to_str().ok())
        .expect("a fresh token")
        .to_string();
    let second = send(
        &f.app,
        Method::GET,
        "/api/queue",
        &[("X-Nd-Authorization", &format!("Bearer {next}"))],
        "",
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    vec![login, first, second]
}

#[tokio::test]
async fn real_navidrome_jwt_never_reaches_a_member() {
    let f = family(true).await;
    let (real, refreshed) = (navidrome_jwt("real"), navidrome_jwt("refreshed"));
    let replies = web_session_round(&f, &real, &refreshed).await;
    for reply in &replies {
        let text = everything(reply);
        assert!(!text.contains(&real), "{text}");
        assert!(!text.contains(&refreshed), "{text}");
        // Nor the claims or signature of either (the header is the same for every token).
        for token in [&real, &refreshed] {
            for part in token.split('.').skip(1) {
                assert!(!text.contains(part), "{text}");
            }
        }
    }
}

#[tokio::test]
async fn refreshed_token_is_swapped() {
    let f = family(true).await;
    let (real, refreshed) = (navidrome_jwt("real"), navidrome_jwt("refreshed"));
    let replies = web_session_round(&f, &real, &refreshed).await;
    let key = f.state.family_vault.jwt_key().expect("a key");
    let fresh = replies[1].headers["x-nd-authorization"].to_str().expect("text");
    assert!(web_session::verify(&key, fresh, Utc::now().timestamp()).is_some());
    // The refreshed one is what the device keeps now (the mocks' expectations check the
    // second call carried it).
    let device = f
        .state
        .family_store
        .devices_of("alex")
        .into_iter()
        .find(|d| d.navidrome_jwt.is_some())
        .expect("a web device");
    let kept = f
        .state
        .family_vault
        .open_sealed(device.navidrome_jwt.as_deref().expect("kept"))
        .expect("opens");
    assert_eq!(kept, refreshed);
    f.server.verify().await;
}

#[tokio::test]
async fn spoofed_forwarded_for_stays_away() {
    let f = family(true).await;
    let at = |peer: &'static str, forwarded: &'static str| {
        let app = f.app.clone();
        async move {
            send_from(
                &app,
                Method::GET,
                "/rest/testEcho?u=alex&p=app-secret-1",
                &[
                    ("X-Forwarded-For", forwarded),
                    ("Cf-Connecting-Ip", "192.168.1.5"),
                ],
                "",
                Some(peer),
            )
            .await
            .json()["place"]
                .clone()
        }
    };
    // Through the local proxy: the client's own home address on the left does not count.
    assert_eq!(at("127.0.0.1", "192.168.1.5, 203.0.113.9").await, "Away");
    // Straight in from outside: headers are not read at all.
    assert_eq!(at("203.0.113.9", "192.168.1.5").await, "Away");
    // At home, through the local proxy.
    assert_eq!(at("127.0.0.1", "192.168.1.20").await, "Home");
}

#[tokio::test]
async fn tunnel_address_counts_as_away() {
    let f = family(true).await;
    let r = send_from(
        &f.app,
        Method::GET,
        "/rest/testEcho?u=winters&t=a&s=b",
        &[("X-Forwarded-For", "203.0.113.9")],
        "",
        Some("127.0.0.1"),
    )
    .await;
    assert_eq!(r.json()["place"], "Away");
}

#[tokio::test]
async fn family_gate_sees_canonical_paths() {
    let f = family(true).await;
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let saw = seen.clone();
    f.state.family_answer_rules.register(
        "testEcho",
        Arc::new(move |context: &AnswerContext<'_>, body: Bytes| {
            saw.lock().push(format!("{} {}", context.endpoint, context.path));
            body
        }),
    );
    let r = send(
        &f.app,
        Method::GET,
        "/REST/TestEcho.VIEW?u=alex&p=app-secret-1",
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["user"], "alex");
    assert_eq!(r.json()["path"], "/rest/testEcho.view");
    assert_eq!(*seen.lock(), ["testEcho /rest/testEcho.view"]);
}

#[tokio::test]
async fn answer_rules_see_plain_bodies() {
    let f = family(true).await;
    f.navidrome_takes(|hidden| hidden == HIDDEN).await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let saw = seen.clone();
    f.state.family_answer_rules.register(
        "getThing",
        Arc::new(move |context: &AnswerContext<'_>, body: Bytes| {
            let mut answer: Value = serde_json::from_slice(&body).expect("plain JSON");
            saw.lock().push(answer.clone());
            answer["subsonic-response"]["seenBy"] = json!(context.caller.user);
            Bytes::from(answer.to_string())
        }),
    );
    let r = send(
        &f.app,
        Method::GET,
        "/rest/getThing?u=alex&p=app-secret-1&f=json",
        &[("Accept-Encoding", "gzip, br")],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["seenBy"], "alex", "{}", r.body);
    assert_eq!(seen.lock()[0]["subsonic-response"]["status"], "ok");
    assert!(r.headers.get("content-encoding").is_none());
    // Navidrome was not asked for a compressed answer.
    let sent = f.rest_requests().await;
    assert!(sent[0].headers.get("accept-encoding").is_none());
}

#[tokio::test]
async fn utf8_device_name_is_decoded() {
    let f = family(true).await;
    let r = send(
        &f.app,
        Method::GET,
        "/rest/testEcho?u=alex&p=app-secret-1",
        &[("X-Octo-Device-Name", "Sam%E2%80%99s%20Caf%C3%A9")],
        "",
    )
    .await;
    assert_eq!(r.json()["user"], "alex");
    assert_eq!(
        f.state
            .family_store
            .device(&plain_device())
            .expect("recorded")
            .name,
        "Sam\u{2019}s Caf\u{e9}"
    );
    // A name far too long, or not even UTF-8, is cut or patched, never refused. The owner's
    // device is noted once Navidrome takes the sign-in.
    Mock::given(path("/rest/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            "application/json",
        ))
        .mount(&f.server)
        .await;
    let long = "%C3%A9".repeat(200);
    let r = send(
        &f.app,
        Method::GET,
        "/rest/testEcho?u=winters&t=a&s=b",
        &[
            ("X-Octo-Device-Name", long.as_str()),
            ("X-Octo-Device-Id", "install-9"),
        ],
        "",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let detected = f.state.family_store.detected();
    assert_eq!(detected[0].name.chars().count(), 64);
}

/// Octo's admin sign-in and the user record a repair reads and writes.
async fn admin_mocks(f: &Family) {
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .and(body_partial_json(json!({ "username": "admin" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "admin-jwt", "isAdmin": true, "username": "admin",
            "subsonicToken": "tok", "subsonicSalt": "salt"
        })))
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user/u-alex"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "u-alex", "userName": "alex", "name": "Alex", "isAdmin": false
        })))
        .mount(&f.server)
        .await;
}

fn puts(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.method.as_str() == "PUT" && r.url.path() == "/api/user/u-alex")
        .count()
}

#[tokio::test]
async fn repair_runs_once_then_refuses() {
    let f = family(true).await;
    admin_mocks(&f).await;
    Mock::given(method("PUT"))
        .and(path("/api/user/u-alex"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "u-alex" })))
        .mount(&f.server)
        .await;
    f.navidrome_refuses_the_rest().await;
    let r = send(
        &f.app,
        Method::GET,
        "/rest/getThing?u=alex&p=app-secret-1&f=json",
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["error"]["code"], 40);
    let all = f.server.received_requests().await.unwrap_or_default();
    assert_eq!(puts(&all), 1);
    // Tried with the old password, then once with the new one.
    assert_eq!(f.rest_requests().await.len(), 2);
    let hidden = f
        .state
        .family_vault
        .open_sealed(
            &f.state
                .family_store
                .member("alex")
                .expect("there")
                .navidrome_password,
        )
        .expect("opens");
    assert_ne!(hidden, HIDDEN);
    // Right after, another refusal does not set a password again.
    let r = send(
        &f.app,
        Method::GET,
        "/rest/getThing?u=alex&p=app-secret-1&f=json",
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["error"]["code"], 40);
    assert_eq!(puts(&f.server.received_requests().await.unwrap_or_default()), 1);
}

#[tokio::test]
async fn a_sign_in_while_a_member_leaves_keeping_their_account_repairs_nothing() {
    let f = family(true).await;
    admin_mocks(&f).await;
    Mock::given(method("PUT"))
        .and(path("/api/user/u-alex"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "u-alex" })))
        .mount(&f.server)
        .await;
    f.navidrome_refuses_the_rest().await;
    // The removal holds the repairs while it sets the password they chose.
    let hold = f.state.family_bridge.hold_repairs("Alex").await;
    let app = f.app.clone();
    let mut signing_in = tokio::spawn(async move {
        send(
            &app,
            Method::GET,
            "/rest/getThing?u=alex&p=app-secret-1&f=json",
            &[],
            "",
        )
        .await
    });
    // Navidrome refused the sign-in, and its repair waits on the hold.
    for _ in 0..200 {
        if !f.rest_requests().await.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(f.rest_requests().await.len(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut signing_in)
            .await
            .is_err(),
        "the repair waits while the removal holds it"
    );
    f.state.family_store.remove_member("alex");
    drop(hold);

    let r = signing_in.await.expect("answered");
    assert_eq!(r.json()["subsonic-response"]["error"]["code"], 40);
    assert_eq!(
        puts(&f.server.received_requests().await.unwrap_or_default()),
        0,
        "no hidden password is set over the one they chose"
    );
}

#[tokio::test]
async fn repair_sets_a_new_password_and_retries() {
    let f = family(true).await;
    admin_mocks(&f).await;
    // Navidrome takes whatever password the PUT set, and nothing before it.
    let set = Arc::new(Mutex::new(None::<String>));
    let setting = set.clone();
    Mock::given(method("PUT"))
        .and(path("/api/user/u-alex"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("json");
            *setting.lock() = body["password"].as_str().map(str::to_string);
            ResponseTemplate::new(200).set_body_json(json!({ "id": "u-alex" }))
        })
        .mount(&f.server)
        .await;
    let current = set.clone();
    Mock::given(SignedAsAlex {
        accepts: Arc::new(move |t, s| {
            current
                .lock()
                .as_deref()
                .is_some_and(|hidden| subsonic_token(hidden, s) == t)
        }),
    })
    .respond_with(ResponseTemplate::new(200).set_body_raw(
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
        "application/json",
    ))
    .with_priority(1)
    .mount(&f.server)
    .await;
    f.navidrome_refuses_the_rest().await;
    let r = send(
        &f.app,
        Method::GET,
        "/rest/getThing?u=alex&p=app-secret-1&f=json",
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
    let kept = f
        .state
        .family_vault
        .open_sealed(
            &f.state
                .family_store
                .member("alex")
                .expect("there")
                .navidrome_password,
        )
        .expect("opens");
    assert_eq!(Some(kept), set.lock().clone());
}

#[tokio::test]
async fn star_on_arrival_survives_repair() {
    let f = family(true).await;
    let calls = Arc::new(Mutex::new(
        Vec::<(String, indexmap::IndexMap<String, String>)>::new(),
    ));
    let calling = calls.clone();
    f.state.star_on_arrival.configure(move |seams| {
        seams.call = Some(Arc::new(move |endpoint, parameters| {
            calling.lock().push((endpoint, parameters));
            Box::pin(async { Ok(br#"{"subsonic-response":{"status":"ok","song":{"id":"nd-1"}}}"#.to_vec()) })
        }));
    });
    // A member's heart arrives signed with the hidden password of the moment.
    let salt = "0123456789ab";
    let held = SubsonicCredential::from(
        [
            ("u".to_string(), "alex".to_string()),
            ("t".to_string(), subsonic_token(HIDDEN, salt)),
            ("s".to_string(), salt.to_string()),
        ]
        .iter()
        .map(|(k, v)| (k, v)),
    )
    .expect("a sign-in");
    f.state
        .star_on_arrival
        .hold_song("deezer", "123", &held, Some("alex"));
    // A repair sets a new hidden password while the download runs.
    let mut alex = f.state.family_store.member("alex").expect("there");
    alex.navidrome_password = f.state.family_vault.seal("hidden-2").expect("sealed");
    f.state.family_store.upsert_member(alex);
    assert!(
        f.state
            .star_on_arrival
            .favorite_owned("deezer", "123", Some("nd-1"), "A", "T", "/x")
    );
    for _ in 0..100 {
        if calls.lock().iter().any(|(endpoint, _)| endpoint == "rest/star") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let calls = calls.lock();
    assert!(
        calls.iter().any(|(endpoint, _)| endpoint == "rest/star"),
        "{calls:?}"
    );
    for (endpoint, parameters) in calls.iter() {
        assert_eq!(parameters["u"], "alex", "{endpoint}");
        assert_eq!(
            parameters["t"],
            subsonic_token("hidden-2", &parameters["s"]),
            "{endpoint}"
        );
    }
}

#[tokio::test]
async fn caller_carries_the_account_quality() {
    let f = family(true).await;
    f.state.family_store.set_preference(
        "winters",
        QualityPreference {
            home: StreamQuality::Original,
            away: StreamQuality::DataSaver,
        },
    );
    let r = send(&f.app, Method::GET, "/rest/testEcho?u=winters&t=a&s=b", &[], "").await;
    assert_eq!(r.json()["away"], "DataSaver");
    assert_eq!(r.json()["deviceQuality"], "Account");
    // Alex's app is recorded at its first sign-in; its own choice then rides on each call.
    send(
        &f.app,
        Method::GET,
        "/rest/testEcho?u=alex&p=app-secret-1",
        &[],
        "",
    )
    .await;
    assert!(f.state.family_store.set_device_quality(
        &plain_device(),
        octo_core::family::quality::DeviceQualityMode::App
    ));
    let r = send(
        &f.app,
        Method::GET,
        "/rest/testEcho?u=alex&p=app-secret-1",
        &[],
        "",
    )
    .await;
    assert_eq!(r.json()["deviceQuality"], "App");
    assert_eq!(r.json()["role"], format!("{:?}", Role::from(Preset::Listener)));
}

#[tokio::test]
async fn native_paths_skip_the_family_pages_and_name_the_owner() {
    let f = family(true).await;
    // The family page's own API is never touched by the gate.
    let r = send(&f.app, Method::GET, "/test/echo?u=alex&p=app-secret-1", &[], "").await;
    assert_eq!(r.json()["user"], Value::Null);
    assert!(skipped("/api/family/me"));
    assert!(skipped("/family/join"));
    // Pairing is read by the gate like any `/rest` call, though nobody is named on it.
    assert!(!skipped("/rest/octoFamilyPair.view"));
    assert!(!skipped("/radio/stream/abc"));
    // A Navidrome token of the owner names the owner and goes on untouched.
    Mock::given(method("GET"))
        .and(path("/api/queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .mount(&f.server)
        .await;
    let owner = navidrome_jwt_for("winters", "owner");
    let r = send(
        &f.app,
        Method::GET,
        "/api/queue",
        &[("X-Nd-Authorization", &format!("Bearer {owner}"))],
        "",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let sent: Vec<_> = f
        .server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path() == "/api/queue")
        .collect();
    assert_eq!(
        sent[0]
            .headers
            .get("x-nd-authorization")
            .and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {owner}").as_str())
    );
}

#[tokio::test]
async fn wrong_account_password_is_401() {
    let f = family(true).await;
    login_mock(&f, HIDDEN, &navidrome_jwt("real")).await;
    let r = web_login(&f, "not the password").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error"], "Invalid username or password");
    let asked = f.server.received_requests().await.unwrap_or_default();
    assert!(asked.iter().all(|r| r.url.path() != "/auth/login"));
    // Someone outside the family goes on to Navidrome as before.
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .and(body_partial_json(json!({ "username": "bob" })))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({ "error": "Invalid username or password" })),
        )
        .mount(&f.server)
        .await;
    let r = send(
        &f.app,
        Method::POST,
        "/auth/login",
        &[("Content-Type", "application/json")],
        r#"{"username":"bob","password":"x"}"#,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let sent = f.server.received_requests().await.unwrap_or_default();
    assert_eq!(
        sent.last().expect("relayed").body,
        br#"{"username":"bob","password":"x"}"#
    );
}

#[tokio::test]
async fn navidrome_web_device_is_reused() {
    let f = family(true).await;
    login_mock(&f, HIDDEN, &navidrome_jwt("real")).await;
    let first = web_login(&f, ACCOUNT_PASSWORD).await.json();
    let second = web_login(&f, APP_SECRET).await.json();
    let key = f.state.family_vault.jwt_key().expect("a key");
    let now = Utc::now().timestamp();
    let did = |answer: &Value| {
        web_session::verify(&key, answer["token"].as_str().expect("a token"), now)
            .expect("signed by Octo")
            .did
    };
    assert_eq!(did(&first), did(&second));
    let webs = f
        .state
        .family_store
        .devices_of("alex")
        .into_iter()
        .filter(|d| d.navidrome_jwt.is_some())
        .count();
    assert_eq!(webs, 1);
    let device = f.state.family_store.device(&did(&first)).expect("there");
    assert_eq!(device.name, "Navidrome in a browser");
}

// ------------------------------------------------------------------------------------------
// Answers in a coding
// ------------------------------------------------------------------------------------------

fn encoded(coding: &str, plain: &[u8]) -> Vec<u8> {
    use std::io::Write;
    match coding {
        "gzip" => {
            let mut out = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            out.write_all(plain).expect("written");
            out.finish().expect("finished")
        }
        "deflate" => {
            let mut out = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            out.write_all(plain).expect("written");
            out.finish().expect("finished")
        }
        "br" => {
            let mut out = Vec::new();
            {
                let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
                writer.write_all(plain).expect("written");
            }
            out
        }
        other => panic!("no encoder for {other}"),
    }
}

fn now_playing_answer(body: Vec<u8>, coding: Option<&str>) -> Response {
    let mut response = Response::new(Body::from(body));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(coding) = coding {
        response.headers_mut().insert(
            header::CONTENT_ENCODING,
            HeaderValue::from_str(coding).expect("a value"),
        );
    }
    response
}

#[tokio::test]
async fn an_encoded_answer_is_opened_before_the_answer_rules_read_it() {
    let f = family(true).await;
    let alex = FamilyCaller::member(
        "alex",
        Preset::Listener,
        octo_core::family::abilities::Abilities::preset(Preset::Listener),
    );
    let plain = json!({"subsonic-response": {"status": "ok", "nowPlaying": {"entry": [
        {"username": "alex", "id": "1"}, {"username": "sam", "id": "2"}
    ]}}})
    .to_string();
    let mut parameters = Parameters::new();
    parameters.insert("f".into(), "json".into());
    for coding in ["gzip", "deflate", "br"] {
        let answer = now_playing_answer(encoded(coding, plain.as_bytes()), Some(coding));
        let out = apply_answer_rules(
            &f.state,
            answer,
            &alex,
            &Method::GET,
            "getNowPlaying",
            "/rest/getNowPlaying",
            &parameters,
        )
        .await;
        assert_eq!(out.status(), StatusCode::OK, "{coding}");
        assert!(out.headers().get(header::CONTENT_ENCODING).is_none(), "{coding}");
        let body = out.into_body().collect().await.expect("a body").to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("\"alex\""), "{coding}: {text}");
        assert!(
            !text.contains("\"sam\""),
            "{coding}: the rule did not run: {text}"
        );
    }
    // A coding Octo cannot open never goes out unread.
    let answer = now_playing_answer(plain.clone().into_bytes(), Some("zstd"));
    let out = apply_answer_rules(
        &f.state,
        answer,
        &alex,
        &Method::GET,
        "getNowPlaying",
        "/rest/getNowPlaying",
        &parameters,
    )
    .await;
    assert_eq!(out.status(), StatusCode::BAD_GATEWAY);
    // Nor does a broken one.
    let answer = now_playing_answer(b"not gzip at all".to_vec(), Some("gzip"));
    let out = apply_answer_rules(
        &f.state,
        answer,
        &alex,
        &Method::GET,
        "getNowPlaying",
        "/rest/getNowPlaying",
        &parameters,
    )
    .await;
    assert_eq!(out.status(), StatusCode::BAD_GATEWAY);
    // "identity" is no coding at all.
    let answer = now_playing_answer(plain.into_bytes(), Some("identity"));
    let out = apply_answer_rules(
        &f.state,
        answer,
        &alex,
        &Method::GET,
        "getNowPlaying",
        "/rest/getNowPlaying",
        &parameters,
    )
    .await;
    let body = out.into_body().collect().await.expect("a body").to_bytes();
    assert!(!String::from_utf8_lossy(&body).contains("\"sam\""));
}

#[test]
fn an_answer_that_opens_too_large_is_not_opened() {
    // A small gzip body that would open past the limit.
    let huge = vec![b' '; ANSWER_LIMIT + 10];
    let packed = encoded("gzip", &huge);
    assert!(packed.len() < 1024 * 1024);
    assert!(decoded("gzip", &packed).is_none());
    assert_eq!(
        decoded("gzip", &encoded("gzip", b"small")).as_deref(),
        Some(&b"small"[..])
    );
}

#[test]
fn answer_outcomes_are_read_in_both_formats() {
    let refused = br#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"Wrong"}}}"#;
    assert_eq!(outcome_of(refused), Outcome::Refused);
    let xml = br#"<subsonic-response status="failed"><error code="40" message="Wrong"/></subsonic-response>"#;
    assert_eq!(outcome_of(xml), Outcome::Refused);
    let other = br#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"Not found"}}}"#;
    assert_eq!(outcome_of(other), Outcome::Failed);
    let code_400 = br#"{"subsonic-response":{"status":"failed","error":{"code":400}}}"#;
    assert_eq!(outcome_of(code_400), Outcome::Failed);
    assert_eq!(
        outcome_of(br#"{"subsonic-response":{"status":"ok"}}"#),
        Outcome::Fine
    );
}

#[tokio::test]
async fn web_player_sign_ins_sent_all_at_once_count_before_they_are_checked() {
    let f = family(true).await;
    let guesses: Vec<String> = (0..(member_bridge::MAX_FAILURES + 5))
        .map(|n| format!("wrong-{n}"))
        .collect();
    let replies = futures::future::join_all(guesses.iter().map(|guess| web_login(&f, guess))).await;
    assert!(replies.iter().all(|r| r.status == StatusCode::UNAUTHORIZED));
    let key = FailureThrottle::key(&IpAddr::V4(Ipv4Addr::UNSPECIFIED), "alex");
    assert_eq!(
        f.state.family_bridge.throttle.tries(&key),
        member_bridge::MAX_FAILURES,
        "only the tries that were let through were checked and counted"
    );
    // The right password waits too, until the tries age out.
    let r = web_login(&f, ACCOUNT_PASSWORD).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_right_web_player_sign_in_is_not_held_against_the_member() {
    let f = family(true).await;
    login_mock(&f, HIDDEN, &navidrome_jwt("real")).await;
    let r = web_login(&f, ACCOUNT_PASSWORD).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let key = FailureThrottle::key(&IpAddr::V4(Ipv4Addr::UNSPECIFIED), "alex");
    assert_eq!(f.state.family_bridge.throttle.tries(&key), 0);
}

// ---------------------------------------------------------------------------------------
// One reading of every request
// ---------------------------------------------------------------------------------------

/// The same call with each kind of body: the query alone, a url-encoded form, JSON, multipart
/// with a boundary, and multipart with none.
fn body_kinds() -> Vec<(&'static str, Method, String, Option<&'static str>, &'static str)> {
    vec![
        (
            "query",
            Method::GET,
            "u=winters&p=pw&id=A&id=B&v=1.16.1&c=App&f=json".into(),
            None,
            "",
        ),
        (
            "form",
            Method::POST,
            "f=json&id=Q".into(),
            Some("application/x-www-form-urlencoded"),
            "u=winters&p=pw&id=1&id=2&format=raw",
        ),
        (
            "json",
            Method::POST,
            "u=winters&p=pw&f=json".into(),
            Some("application/json"),
            r#"{"id":"9","format":"raw","n":[1, 2]}"#,
        ),
        (
            "multipart",
            Method::POST,
            "u=winters&p=pw&f=json".into(),
            Some("multipart/form-data; boundary=XyZ"),
            "--XyZ\r\nContent-Disposition: form-data; name=\"id\"\r\n\r\n5\r\n--XyZ--\r\n",
        ),
        (
            "boundaryless",
            Method::POST,
            "u=winters&f=json".into(),
            Some("multipart/form-data"),
            "u=guest&p=guest-pw&id=7",
        ),
    ]
}

/// What a handler read and what Navidrome was sent, for each kind of body, with Family off.
/// These are the values Octo gave before the family gate read requests at all.
#[tokio::test]
async fn family_off_reads_and_relays_every_kind_of_body_as_before() {
    let f = family(false).await;
    ok_relay(&f).await;
    let mut seen = Vec::new();
    for (kind, method, query, content_type, body) in body_kinds() {
        let headers: Vec<(&str, &str)> = content_type.iter().map(|ct| ("Content-Type", *ct)).collect();
        let echo = send(
            &f.app,
            method.clone(),
            &format!("/rest/testEcho?{query}"),
            &headers,
            body,
        )
        .await
        .json();
        assert_eq!(
            echo["gate"], false,
            "{kind}: the gate never reads with Family off"
        );
        assert_eq!(echo["user"], Value::Null, "{kind}");
        assert_eq!(echo["query"], json!(query), "{kind}");
        assert_eq!(echo["body"], json!(body), "{kind}");
        send(&f.app, method, &format!("/rest/getThing?{query}"), &headers, body).await;
        seen.push((kind, echo["parameters"].clone()));
    }
    let expected = [
        (
            "query",
            json!([
                ["u", "winters"],
                ["p", "pw"],
                ["id", "A,B"],
                ["v", "1.16.1"],
                ["c", "App"],
                ["f", "json"]
            ]),
        ),
        (
            "form",
            json!([
                ["f", "json"],
                ["id", "1,2"],
                ["u", "winters"],
                ["p", "pw"],
                ["format", "raw"]
            ]),
        ),
        (
            "json",
            json!([
                ["u", "winters"],
                ["p", "pw"],
                ["f", "json"],
                ["id", "9"],
                ["format", "raw"],
                ["n", "[1, 2]"]
            ]),
        ),
        (
            "multipart",
            json!([["u", "winters"], ["p", "pw"], ["f", "json"], ["id", "5"]]),
        ),
        (
            "boundaryless",
            json!([["u", "guest"], ["f", "json"], ["p", "guest-pw"], ["id", "7"]]),
        ),
    ];
    for ((kind, parameters), (want_kind, want)) in seen.iter().zip(expected.iter()) {
        assert_eq!(kind, want_kind);
        assert_eq!(parameters, want, "{kind}");
    }
    let relayed: Vec<(String, Option<String>, String)> = f
        .rest_requests()
        .await
        .into_iter()
        .filter(|r| r.url.path() == "/rest/getThing")
        .map(|r| {
            (
                r.method.to_string(),
                r.url.query().map(str::to_string),
                String::from_utf8_lossy(&r.body).into_owned(),
            )
        })
        .collect();
    let expected: Vec<(String, Option<String>, String)> = vec![
        (
            "GET".into(),
            Some("u=winters&p=pw&id=A&id=B&v=1.16.1&c=App&f=json".into()),
            String::new(),
        ),
        (
            "POST".into(),
            Some("f=json".into()),
            "u=winters&p=pw&id=1&id=2&format=raw".into(),
        ),
        (
            "POST".into(),
            Some("u=winters&p=pw&f=json&id=9&format=raw&n=%5B1%2C%202%5D".into()),
            r#"{"id":"9","format":"raw","n":[1, 2]}"#.into(),
        ),
        (
            "POST".into(),
            Some("u=winters&p=pw&f=json&id=5".into()),
            "--XyZ\r\nContent-Disposition: form-data; name=\"id\"\r\n\r\n5\r\n--XyZ--\r\n".into(),
        ),
        (
            "POST".into(),
            Some("u=guest&f=json&p=guest-pw&id=7".into()),
            "u=guest&p=guest-pw&id=7".into(),
        ),
    ];
    assert_eq!(relayed, expected);
}

/// With Family on, a handler reads the parameters the gate read: for a member the hidden
/// sign-in in place of the device secret, whatever kind of body carried the call.
#[tokio::test]
async fn handlers_read_the_parameters_the_gate_read() {
    let f = family(true).await;
    for (content_type, body) in [
        ("application/x-www-form-urlencoded", "id=1&id=2"),
        ("application/json", r#"{"id":"9"}"#),
        (
            "multipart/form-data; boundary=XyZ",
            "--XyZ\r\nContent-Disposition: form-data; name=\"id\"\r\n\r\n5\r\n--XyZ--\r\n",
        ),
    ] {
        let r = send(
            &f.app,
            Method::POST,
            &format!("/rest/testEcho?u=alex&p={APP_SECRET}&f=json"),
            &[("Content-Type", content_type)],
            body,
        )
        .await;
        let seen = r.json();
        assert_eq!(seen["gate"], true, "{content_type}");
        assert_eq!(seen["user"], "alex", "{content_type}");
        let parameters: Vec<(String, String)> = seen["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .map(|pair| {
                (
                    pair[0].as_str().unwrap_or("").to_string(),
                    pair[1].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        assert!(signed_with(&parameters, HIDDEN), "{content_type}: {parameters:?}");
        assert!(
            value(&parameters, "p").is_none(),
            "{content_type}: {parameters:?}"
        );
        assert!(
            value(&parameters, "id").is_some(),
            "{content_type}: {parameters:?}"
        );
    }
    // A body the gate cannot read never reaches a handler.
    let r = send(
        &f.app,
        Method::POST,
        "/rest/testEcho?u=winters&f=json",
        &[("Content-Type", "multipart/form-data")],
        "u=guest&p=guest-pw",
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["error"]["code"], 0, "{}", r.body);
}

#[test]
fn the_gate_reads_a_path_as_an_upstream_server_may() {
    for (sent, read) in [
        ("/rest/getNowPlaying", "/rest/getNowPlaying"),
        ("/rest/getNowPlaying/", "/rest/getNowPlaying/"),
        ("/rest/%67etNowPlaying", "/rest/getNowPlaying"),
        ("/rest/%2567etNowPlaying", "/rest/getNowPlaying"),
        ("/rest/x/../getNowPlaying", "/rest/getNowPlaying"),
        ("/rest/x/%2E%2E/getNowPlaying", "/rest/getNowPlaying"),
        ("/rest/./getNowPlaying", "/rest/getNowPlaying"),
        ("//rest//getNowPlaying", "/rest/getNowPlaying"),
        ("/rest%2FgetNowPlaying", "/rest/getNowPlaying"),
        ("/rest%5CgetNowPlaying", "/rest/getNowPlaying"),
        ("/rest/getNow%09Playing", "/rest/getNowPlaying"),
        ("/rest/getNowPlaying%3Fu=alex%26x=", "/rest/getNowPlaying"),
        ("/rest/getNowPlaying%23x", "/rest/getNowPlaying"),
        ("/api/../rest/ping", "/rest/ping"),
        ("/../../rest/ping", "/rest/ping"),
        ("/", "/"),
        ("/assets/a%20b.js", "/assets/a b.js"),
    ] {
        assert_eq!(gate_path(sent), read, "{sent}");
    }
}

#[test]
fn an_import_file_may_carry_a_whole_export_and_everything_else_keeps_the_usual_limit() {
    assert!(body_limit("/rest/importFile.view") > 64 * 1024 * 1024);
    assert_eq!(body_limit("/REST/importfile"), IMPORT_FILE_BODY);
    assert_eq!(body_limit("/rest/importText"), MAX_BODY);
    assert_eq!(body_limit("/rest/stream"), MAX_BODY);
}

/// A family member looks up a place like any listener: the gate signs them in with their hidden
/// password, and getPlace answers.
#[tokio::test]
async fn a_member_looks_up_a_place() {
    let f = family(true).await;
    f.navidrome_takes(|hidden| hidden == HIDDEN).await;
    f.navidrome_refuses_the_rest().await;
    crate::controllers::subsonic::places_tests::write_fixture(f._dir.path());
    let app = build_with(f.state.clone(), crate::controllers::subsonic::places::routes());
    let t = subsonic_token(APP_SECRET, "c19b2d");
    let r = send(
        &app,
        Method::GET,
        &format!("/rest/getPlace?u=alex&t={t}&s=c19b2d&v=1.16.1&c=Octo&f=json&code=90210&country=US"),
        &[],
        "",
    )
    .await;
    assert_eq!(
        r.json()["subsonic-response"]["place"]["name"],
        "Beverly Hills",
        "{}",
        r.body
    );
    let wrong = send(
        &app,
        Method::GET,
        "/rest/getPlace?u=alex&p=not-it&v=1.16.1&c=Octo&f=json&code=90210&country=US",
        &[],
        "",
    )
    .await;
    assert_eq!(
        wrong.json()["subsonic-response"]["error"]["code"],
        40,
        "{}",
        wrong.body
    );
}
