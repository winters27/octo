//! Every way a member could get audio past their family rules, or change what they may not,
//! tried through the whole pipeline against a Navidrome stood in for by wiremock. Each test
//! names the path it closes; the last ones show that the owner, and everyone with Family off,
//! get exactly what they had before.

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use http_body_util::BodyExt;
use octo_core::family::abilities::Preset;
use octo_core::family::device::{Device, NetworkPlace};
use octo_core::family::quality::{DeviceQualityMode, QualityPreference, StreamQuality};
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, FamilySettings, SubsonicSettings};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::app::{AppState, TestServices};
use crate::http::pipeline::{App, build_with};
use crate::services::family::device_registry::{Sighting, detected_device_id};
use crate::services::family::member_credentials::tests::member;

/// Alex's one login password.
const APP_SECRET: &str = "app-secret-1";
const HOME: &str = "192.168.1.5";
const AWAY: &str = "203.0.113.9";
const LIBRARY_AUDIO: &str = "library-audio-bytes";

struct Family {
    server: MockServer,
    state: AppState,
    app: App,
    dir: tempfile::TempDir,
}

/// What Alex can do: a preset and edits over it.
struct Alex {
    preset: Preset,
    edits: Value,
}

impl Alex {
    fn listener() -> Self {
        Alex {
            preset: Preset::Listener,
            edits: json!({}),
        }
    }

    fn with(edits: Value) -> Self {
        Alex {
            preset: Preset::Listener,
            edits,
        }
    }
}

/// The id Octo gives Alex's Octo app ([`ALEX`], `c=Octo`).
fn d_app() -> String {
    detected_device_id(
        "alex",
        &Sighting {
            client: "Octo".into(),
            ..Default::default()
        },
    )
}

/// The id Octo gives Alex's Symfonium ([`ALEX_SUB`]).
fn d_sub() -> String {
    detected_device_id(
        "alex",
        &Sighting {
            client: "Symfonium".into(),
            ..Default::default()
        },
    )
}

/// The owner `winters` and Alex (the Octo app [`d_app`] and Symfonium [`d_sub`], both seen
/// before), over a Navidrome that answers every call.
async fn family(enabled: bool, alex: Alex, configure: impl FnOnce(&mut AppSettings)) -> Family {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut settings = AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("pw".into()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        family: FamilySettings {
            enabled,
            members_path: dir.path().join("members").to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    // AcoustID on, by a key of the user's own: a Wait for lossless play waits for its copy.
    settings.soulseek.acoust_id_api_key = "user-key".into();
    configure(&mut settings);
    let state = AppState::for_tests_with(
        settings,
        TestServices {
            config_dir: Some(dir.path().join("config")),
            ..Default::default()
        },
    );
    assert!(state.family_vault.create_key_if_missing().expect("a key"));
    let store = &state.family_store;
    store.set_owner("winters");
    let mut member_alex = member("alex", state.family_vault.seal("hidden-1").expect("sealed"));
    member_alex.account_password =
        crate::services::family::passwords::seal(&state.family_vault, APP_SECRET).expect("sealed");
    member_alex.preset = alex.preset;
    if let Value::Object(edits) = alex.edits {
        member_alex.edits = edits;
    }
    store.upsert_member(member_alex);
    let mut sam = member("sam", state.family_vault.seal("hidden-2").expect("sealed"));
    sam.personal_library_id = Some(7);
    store.upsert_member(sam);
    for (id, name, app) in [
        (d_app(), "Alex's phone", "Octo"),
        (d_sub(), "Pixel 9", "Symfonium"),
    ] {
        store.add_device(Device {
            id,
            username: "alex".into(),
            name: name.into(),
            created: Utc::now(),
            last_seen: None,
            app: app.into(),
            place: NetworkPlace::Home,
            seen_apps: Vec::new(),
            navidrome_jwt: None,
            quality: Default::default(),
        });
    }
    navidrome(&server).await;
    let app = build_with(state.clone(), crate::controllers::subsonic::routes());
    Family {
        server,
        state,
        app,
        dir,
    }
}

/// Navidrome: the admin sign-in, audio for `stream` and `download`, the extension list with
/// `transcoding`, a transcode decision that plays the file as it is, and ok for the rest.
async fn navidrome(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "admin-jwt", "isAdmin": true, "username": "admin",
            "subsonicToken": "tok", "subsonicSalt": "salt"
        })))
        .mount(server)
        .await;
    for endpoint in ["/rest/stream", "/rest/download"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_raw(LIBRARY_AUDIO, "audio/mpeg"))
            .with_priority(1)
            .mount(server)
            .await;
    }
    Mock::given(path("/rest/getOpenSubsonicExtensions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","openSubsonicExtensions":[{"name":"transcoding","versions":[1]},{"name":"formPost","versions":[1]}]}}"#,
            "application/json",
        ))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(path_regex("^/rest/getTranscode"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","transcodeDecision":{"canDirectPlay":true}}}"#,
            "application/json",
        ))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(path_regex("^/(rest|api)/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            "application/json",
        ))
        .with_priority(10)
        .mount(server)
        .await;
}

impl Family {
    /// What Navidrome was asked, for one path.
    async fn asked(&self, at: &str) -> Vec<wiremock::Request> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.url.path().eq_ignore_ascii_case(at))
            .collect()
    }

    /// The query Navidrome got for the one `stream` it was asked for.
    async fn streamed(&self) -> Vec<(String, String)> {
        let asked = self.asked("/rest/stream").await;
        assert_eq!(asked.len(), 1, "one stream relayed");
        pairs(asked[0].url.query().unwrap_or(""))
    }

    /// The library song `lib-9` at `<dir>/music/A/B/song.flac` (or in Sam's own library),
    /// read into the library index.
    async fn index_song(&self, in_sams_library: bool) -> String {
        let (library_id, library_path) = if in_sams_library {
            (7, self.dir.path().join("members").join("sam"))
        } else {
            (1, self.dir.path().join("music"))
        };
        let file = library_path.join("A").join("B").join("song.flac");
        std::fs::create_dir_all(file.parent().expect("a folder")).expect("folders");
        std::fs::write(&file, b"the-original-flac-file").expect("the file");
        Mock::given(method("GET"))
            .and(path("/api/song"))
            .and(query_param("_start", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "id": "lib-9", "albumId": "al-9", "artistId": "ar-9", "libraryId": library_id,
                "libraryPath": library_path.to_string_lossy(), "path": "A/B/song.flac",
                "explicitStatus": "", "size": 22, "title": "Song"
            }])))
            .with_priority(1)
            .mount(&self.server)
            .await;
        assert_eq!(self.state.library_index.refresh().await.expect("read"), 1);
        file.to_string_lossy().into_owned()
    }
}

struct Reply {
    status: StatusCode,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    fn error_code(&self) -> Option<i64> {
        self.json()["subsonic-response"]["error"]["code"].as_i64()
    }
}

fn request(method: Method, uri: &str, headers: &[(&str, &str)], peer: &str) -> Request {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut req = req.body(Body::empty()).expect("request");
    let address: SocketAddr = format!("{peer}:50000").parse().expect("an address");
    req.extensions_mut().insert(ConnectInfo(address));
    req
}

async fn send(f: &Family, method: Method, uri: &str, headers: &[(&str, &str)], peer: &str) -> Reply {
    let res = f
        .app
        .clone()
        .oneshot(request(method, uri, headers, peer))
        .await
        .expect("infallible");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    Reply {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

async fn get(f: &Family, uri: &str, peer: &str) -> Reply {
    send(f, Method::GET, uri, &[], peer).await
}

fn pairs(query: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

const ALEX: &str = "u=alex&p=app-secret-1&v=1.16.1&c=Octo&f=json";
const ALEX_SUB: &str = "u=alex&p=app-secret-1&v=1.16.1&c=Symfonium&f=json";
const OWNER: &str = "u=winters&t=abc&s=def&v=1.16.1&c=Feishin&f=json";

/// A Navidrome browser token for `sub`, as the web player would carry it.
fn navidrome_jwt(sub: &str) -> String {
    let payload = json!({ "iss": "ND", "sub": sub, "exp": Utc::now().timestamp() + 3600 });
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(payload.to_string()),
        URL_SAFE_NO_PAD.encode("navidrome-signature")
    )
}

fn capped() -> Alex {
    Alex::with(json!({ "streamCap": 192 }))
}

/// The browser token a member's Navidrome web player holds: one Octo signed for Alex's web
/// device `d_web`, which keeps the real Navidrome token.
fn octo_web_token(f: &Family) -> String {
    use crate::services::family::web_session::{WebClaims, mint};
    f.state.family_store.add_device(Device {
        id: "d_web".into(),
        username: "alex".into(),
        name: "Navidrome in a browser".into(),
        created: Utc::now(),
        last_seen: None,
        app: String::new(),
        place: NetworkPlace::Home,
        seen_apps: Vec::new(),
        navidrome_jwt: f.state.family_vault.seal(&navidrome_jwt("alex")),
        quality: Default::default(),
    });
    let key = f.state.family_vault.jwt_key().expect("a key");
    let now = Utc::now().timestamp();
    mint(
        &key,
        &WebClaims {
            iss: "ND".into(),
            sub: "alex".into(),
            uid: "u-alex".into(),
            adm: false,
            iat: now,
            exp: now + 3600,
            did: "d_web".into(),
        },
    )
}

// ---------------------------------------------------------------------------------------
// The paths
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn transcode_decision_and_stream_are_refused_with_a_ceiling() {
    let f = family(true, capped(), |_| {}).await;
    for endpoint in ["getTranscodeDecision", "getTranscodeStream"] {
        let r = get(
            &f,
            &format!("/rest/{endpoint}?{ALEX}&mediaId=lib-1&mediaType=song"),
            HOME,
        )
        .await;
        assert_eq!(r.error_code(), Some(0), "{endpoint}: {}", r.body);
    }
    assert!(f.asked("/rest/getTranscodeDecision.view").await.is_empty());
    assert!(f.asked("/rest/getTranscodeStream").await.is_empty());
    // The extension list does not offer it either, so a client goes to `stream`.
    let r = get(&f, &format!("/rest/getOpenSubsonicExtensions?{ALEX}"), HOME).await;
    assert!(!r.body.contains("\"transcoding\""), "{}", r.body);
    assert!(r.body.contains("formPost"), "{}", r.body);
}

#[tokio::test]
async fn transcode_decisions_stay_for_a_member_without_a_ceiling() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let r = get(
        &f,
        &format!("/rest/getTranscodeDecision?{ALEX}&mediaId=lib-1"),
        HOME,
    )
    .await;
    assert_eq!(
        r.json()["subsonic-response"]["transcodeDecision"]["canDirectPlay"],
        true
    );
    let r = get(&f, &format!("/rest/getOpenSubsonicExtensions?{ALEX}"), HOME).await;
    assert!(r.body.contains("\"transcoding\""), "{}", r.body);
}

#[tokio::test]
async fn local_file_shortcut_relays_the_library_copy_with_the_ceiling() {
    let f = family(true, capped(), |s| s.subsonic.wait_for_lossless_on_play = true).await;
    let file = f.index_song(false).await;
    let song = Song {
        title: "Song".into(),
        artist: "Artist".into(),
        external_provider: Some("deezer".into()),
        external_id: Some("1".into()),
        ..Default::default()
    };
    f.state
        .local_library
        .register_downloaded_song(&song, &file)
        .await
        .expect("mapped");
    let r = get(
        &f,
        &format!("/rest/stream?{ALEX}&id=ext-deezer-song-1&format=raw"),
        HOME,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, LIBRARY_AUDIO, "the library copy, never the file");
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "id"), Some("lib-9"));
    assert_eq!(value(&sent, "maxBitRate"), Some("192"));
    assert_eq!(value(&sent, "format"), Some("opus"));
}

/// `who`'s own Wait for lossless play of the song, landed already with `file`: the acquisition their
/// play joins. A play's download is its own person's, so no one else's play joins it.
fn landed_play_of(f: &Family, who: &str, file: &str) {
    let landed = f.state.track_acquisition_queue.enqueue_as(
        Some(octo_core::fingerprint::heart_gate::AcquisitionOrigin::Play),
        Vec::new(),
        &crate::services::common::AcquisitionTarget::Shared,
        false,
        "deezer",
        "1",
        false,
        false,
        true,
        None,
        true,
        Some(who),
        false,
        None,
    );
    assert!(landed.try_set_result(file));
}

#[tokio::test]
async fn serve_acquired_relays_the_library_copy_with_the_ceiling() {
    // Only a member who adds songs at once has a play keep a copy, so only their play joins
    // the acquisition.
    let direct = Alex::with(json!({ "addToLibrary": "Direct", "streamCap": 192 }));
    let f = family(true, direct, |s| s.subsonic.wait_for_lossless_on_play = true).await;
    let file = f.index_song(false).await;
    landed_play_of(&f, "alex", &file);
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=ext-deezer-song-1"), HOME).await;
    assert_eq!(r.body, LIBRARY_AUDIO, "the library copy, never the file");
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "id"), Some("lib-9"));
    assert_eq!(value(&sent, "maxBitRate"), Some("192"));
}

#[tokio::test]
async fn a_capped_listener_plays_from_the_source_and_never_gets_the_file() {
    let f = family(true, capped(), |s| s.subsonic.wait_for_lossless_on_play = true).await;
    let file = f.index_song(false).await;
    let landed = f
        .state
        .track_acquisition_queue
        .enqueue("deezer", "1", false, false, true, None, true, None, false, None);
    assert!(landed.try_set_result(file));
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=ext-deezer-song-1"), HOME).await;
    assert!(!r.body.contains("the-original-flac-file"), "{}", r.body);
}

#[tokio::test]
async fn a_file_in_another_members_library_is_never_served() {
    // The owner has no ceiling, so only the library a file is in keeps it back.
    let f = family(true, Alex::listener(), |s| {
        s.subsonic.wait_for_lossless_on_play = true
    })
    .await;
    let file = f.index_song(true).await;
    let song = Song {
        external_provider: Some("deezer".into()),
        external_id: Some("1".into()),
        ..Default::default()
    };
    f.state
        .local_library
        .register_downloaded_song(&song, &file)
        .await
        .expect("mapped");
    // Each player's own play of the song has landed the file already.
    for who in ["winters", "alex"] {
        landed_play_of(&f, who, &file);
    }
    for who in [OWNER, ALEX] {
        let r = get(&f, &format!("/rest/stream?{who}&id=ext-deezer-song-1"), HOME).await;
        assert!(!r.body.contains("the-original-flac-file"), "{who}: {}", r.body);
    }
}

#[tokio::test]
async fn download_is_refused_when_downloads_are_off() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let r = get(&f, &format!("/rest/download?{ALEX}&id=lib-1"), HOME).await;
    assert_eq!(r.error_code(), Some(50));
    assert!(r.body.contains("Downloads are off"), "{}", r.body);
    assert!(f.asked("/rest/download").await.is_empty());
}

#[tokio::test]
async fn an_allowed_download_takes_the_ceiling() {
    let f = family(
        true,
        Alex::with(json!({ "downloadFiles": true, "streamCap": 192 })),
        |_| {},
    )
    .await;
    let r = get(&f, &format!("/rest/download?{ALEX}&id=lib-1"), HOME).await;
    assert_eq!(r.body, LIBRARY_AUDIO);
    let asked = f.asked("/rest/download").await;
    let sent = pairs(asked[0].url.query().unwrap_or(""));
    assert_eq!(value(&sent, "format"), Some("opus"));
    assert_eq!(value(&sent, "bitrate"), Some("192"));
}

#[tokio::test]
async fn create_share_is_refused_when_shares_are_off() {
    let f = family(true, Alex::listener(), |_| {}).await;
    for endpoint in ["createShare", "updateShare"] {
        let r = get(&f, &format!("/rest/{endpoint}?{ALEX}&id=lib-1"), HOME).await;
        assert_eq!(r.error_code(), Some(50), "{endpoint}");
        assert!(f.asked(&format!("/rest/{endpoint}")).await.is_empty());
    }
    // A share write through Navidrome's own API is refused too.
    let jwt = octo_web_token(&f);
    let r = send(
        &f,
        Method::POST,
        "/api/share",
        &[("X-Nd-Authorization", &format!("Bearer {jwt}"))],
        HOME,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn native_player_write_is_refused() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let jwt = format!("Bearer {}", octo_web_token(&f));
    let r = send(
        &f,
        Method::PUT,
        "/api/player/p1",
        &[("X-Nd-Authorization", &jwt)],
        HOME,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(
        &f,
        Method::PUT,
        "/api/user/u-alex",
        &[("X-Nd-Authorization", &jwt)],
        HOME,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("Change this in Octo"), "{}", r.body);
    assert!(f.asked("/api/player/p1").await.is_empty());
    assert!(f.asked("/api/user/u-alex").await.is_empty());
    // Reading is fine.
    let r = send(
        &f,
        Method::GET,
        "/api/player",
        &[("X-Nd-Authorization", &jwt)],
        HOME,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn unknown_rest_endpoint_is_refused_for_strict_members() {
    let f = family(true, capped(), |_| {}).await;
    let r = get(&f, &format!("/rest/getSomethingNew?{ALEX}"), HOME).await;
    assert_eq!(r.error_code(), Some(0));
    assert!(f.asked("/rest/getSomethingNew").await.is_empty());
    // Changing the account's password in Navidrome would break the family's sign-in.
    let r = get(
        &f,
        &format!("/rest/changePassword?{ALEX}&username=alex&password=x"),
        HOME,
    )
    .await;
    assert_eq!(r.error_code(), Some(50));
    // A kid is strict too; a member with no limit is not.
    let kid = family(
        true,
        Alex {
            preset: Preset::Kid,
            edits: json!({}),
        },
        |_| {},
    )
    .await;
    let r = get(&kid, &format!("/rest/getSomethingNew?{ALEX}"), HOME).await;
    assert_eq!(r.error_code(), Some(0));
    let open = family(true, Alex::listener(), |_| {}).await;
    let r = get(&open, &format!("/rest/getSomethingNew?{ALEX}"), HOME).await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok");
    assert_eq!(open.asked("/rest/getSomethingNew").await.len(), 1);
}

#[tokio::test]
async fn quickconnect_authorize_is_refused() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let jwt = format!("Bearer {}", octo_web_token(&f));
    let r = send(
        &f,
        Method::POST,
        "/api/quickconnect/authorize?code=123456",
        &[("X-Nd-Authorization", &jwt)],
        HOME,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(f.asked("/api/quickconnect/authorize").await.is_empty());
}

#[tokio::test]
async fn radio_away_off_refuses_the_stream() {
    let f = family(true, Alex::with(json!({ "away": false })), |_| {}).await;
    let token = f
        .state
        .last_fm_radio_stream_sessions
        .issue("alex", "station-1", [("u", "alex")], None);
    let r = get(&f, &format!("/radio/stream/{token}"), AWAY).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("away from home"), "{}", r.body);
    // At home the stream goes on to the radio (which has no such station here).
    let r = get(&f, &format!("/radio/stream/{token}"), HOME).await;
    assert_ne!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn away_off_refuses_a_stream() {
    let f = family(true, Alex::with(json!({ "away": false })), |_| {}).await;
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=lib-1"), AWAY).await;
    assert_eq!(r.error_code(), Some(50));
    assert!(f.asked("/rest/stream").await.is_empty());
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=lib-1"), HOME).await;
    assert_eq!(r.body, LIBRARY_AUDIO);
}

#[tokio::test]
async fn kid_explicit_stream_is_refused() {
    let f = family(
        true,
        Alex {
            preset: Preset::Kid,
            edits: json!({}),
        },
        |_| {},
    )
    .await;
    Mock::given(path("/rest/getSong"))
        .and(query_param("id", "lib-x"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "subsonic-response": {"status": "ok", "song": {"id": "lib-x", "explicitStatus": "explicit", "bitRate": 320}}
        })))
        .with_priority(1)
        .mount(&f.server)
        .await;
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=lib-x"), HOME).await;
    assert_eq!(r.error_code(), Some(50));
    assert!(r.body.contains("Not available on this profile"), "{}", r.body);
    assert!(f.asked("/rest/stream").await.is_empty());
    // Asked once, then known for an hour.
    let _ = get(&f, &format!("/rest/stream?{ALEX}&id=lib-x"), HOME).await;
    assert_eq!(f.asked("/rest/getSong").await.len(), 1);
    // A song with no mark plays.
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=lib-1"), HOME).await;
    assert_eq!(r.body, LIBRARY_AUDIO);
}

#[tokio::test]
async fn owner_is_unaffected() {
    let f = family(true, capped(), |_| {}).await;
    let r = get(&f, &format!("/rest/stream?{OWNER}&id=lib-1&format=raw"), AWAY).await;
    assert_eq!(r.body, LIBRARY_AUDIO);
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "format"), Some("raw"));
    assert_eq!(value(&sent, "maxBitRate"), None);
    let r = get(
        &f,
        &format!("/rest/getTranscodeDecision?{OWNER}&mediaId=lib-1"),
        HOME,
    )
    .await;
    assert_eq!(
        r.json()["subsonic-response"]["transcodeDecision"]["canDirectPlay"],
        true
    );
    let r = get(&f, &format!("/rest/download?{OWNER}&id=lib-1"), HOME).await;
    assert_eq!(r.body, LIBRARY_AUDIO);
    let r = get(&f, &format!("/rest/getSomethingNew?{OWNER}"), HOME).await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok");
    let jwt = format!("Bearer {}", navidrome_jwt("winters"));
    let r = send(
        &f,
        Method::PUT,
        "/api/player/p1",
        &[("X-Nd-Authorization", &jwt)],
        HOME,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = get(&f, &format!("/rest/getOpenSubsonicExtensions?{OWNER}"), HOME).await;
    assert!(r.body.contains("\"transcoding\""), "{}", r.body);
}

#[tokio::test]
async fn family_off_is_unaffected() {
    let f = family(false, capped(), |_| {}).await;
    f.state.family_store.set_preference(
        "winters",
        QualityPreference {
            home: StreamQuality::DataSaver,
            away: StreamQuality::DataSaver,
        },
    );
    let query = format!("{ALEX}&id=lib-1&format=raw");
    let r = get(&f, &format!("/rest/stream?{query}"), AWAY).await;
    assert_eq!(r.body, LIBRARY_AUDIO);
    let asked = f.asked("/rest/stream").await;
    let sent = pairs(asked[0].url.query().unwrap_or(""));
    assert_eq!(value(&sent, "format"), Some("raw"));
    assert_eq!(value(&sent, "maxBitRate"), None);
    let r = get(
        &f,
        &format!("/rest/getTranscodeDecision?{ALEX}&mediaId=lib-1"),
        HOME,
    )
    .await;
    assert_eq!(
        r.json()["subsonic-response"]["transcodeDecision"]["canDirectPlay"],
        true
    );
    let r = get(&f, &format!("/rest/getSomethingNew?{ALEX}"), HOME).await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok");
    assert!(f.state.family_audio.tracker().is_empty());
}

// ---------------------------------------------------------------------------------------
// Quality
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn account_away_preference_caps_raw() {
    let f = family(true, Alex::listener(), |_| {}).await;
    f.state.family_store.set_preference(
        "alex",
        QualityPreference {
            home: StreamQuality::Original,
            away: StreamQuality::Standard,
        },
    );
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=lib-1&format=raw"), AWAY).await;
    assert_eq!(r.body, LIBRARY_AUDIO);
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "format"), Some("opus"));
    assert_eq!(value(&sent, "maxBitRate"), Some("160"));
    assert_eq!(
        sent.iter().filter(|(k, _)| k == "format").count(),
        1,
        "raw is gone: {sent:?}"
    );
}

#[tokio::test]
async fn home_original_preference_keeps_raw() {
    let f = family(true, Alex::listener(), |_| {}).await;
    f.state.family_store.set_preference(
        "alex",
        QualityPreference {
            home: StreamQuality::Original,
            away: StreamQuality::Standard,
        },
    );
    let _ = get(&f, &format!("/rest/stream?{ALEX}&id=lib-1&format=raw"), HOME).await;
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "format"), Some("raw"));
    assert_eq!(value(&sent, "maxBitRate"), None);
}

#[tokio::test]
async fn app_mode_ignores_account_preference() {
    let f = family(true, Alex::listener(), |_| {}).await;
    f.state.family_store.set_preference(
        "alex",
        QualityPreference {
            home: StreamQuality::DataSaver,
            away: StreamQuality::DataSaver,
        },
    );
    assert!(
        f.state
            .family_store
            .set_device_quality(&d_app(), DeviceQualityMode::App)
    );
    let _ = get(&f, &format!("/rest/stream?{ALEX}&id=lib-1&format=raw"), AWAY).await;
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "format"), Some("raw"));
    assert_eq!(value(&sent, "maxBitRate"), None);
}

#[tokio::test]
async fn family_limit_wins_over_original_preference() {
    let f = family(true, capped(), |_| {}).await;
    let _ = get(
        &f,
        &format!("/rest/stream?{ALEX}&id=lib-1&format=raw&maxBitRate=320"),
        HOME,
    )
    .await;
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "format"), Some("opus"));
    assert_eq!(value(&sent, "maxBitRate"), Some("192"));
}

#[tokio::test]
async fn client_lower_request_wins() {
    let f = family(true, capped(), |_| {}).await;
    let _ = get(
        &f,
        &format!("/rest/stream?{ALEX}&id=lib-1&format=mp3&maxBitRate=96"),
        HOME,
    )
    .await;
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "format"), Some("mp3"));
    assert_eq!(value(&sent, "maxBitRate"), Some("96"));
}

#[tokio::test]
async fn owner_preference_applies() {
    let f = family(true, Alex::listener(), |_| {}).await;
    f.state.family_store.set_preference(
        "winters",
        QualityPreference {
            home: StreamQuality::High,
            away: StreamQuality::Original,
        },
    );
    let _ = get(&f, &format!("/rest/stream?{OWNER}&id=lib-1"), HOME).await;
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "format"), Some("opus"));
    assert_eq!(value(&sent, "maxBitRate"), Some("256"));
    // The owner's own ceiling also keeps transcode decisions from them.
    let r = get(
        &f,
        &format!("/rest/getTranscodeDecision?{OWNER}&mediaId=lib-1"),
        HOME,
    )
    .await;
    assert_eq!(r.error_code(), Some(0));
}

#[tokio::test]
async fn family_off_never_rewrites_quality() {
    let f = family(false, Alex::listener(), |_| {}).await;
    f.state.family_store.set_preference(
        "winters",
        QualityPreference {
            home: StreamQuality::DataSaver,
            away: StreamQuality::DataSaver,
        },
    );
    let query = format!("{OWNER}&id=lib-1&format=raw");
    let _ = get(&f, &format!("/rest/stream?{query}"), HOME).await;
    let asked = f.asked("/rest/stream").await;
    assert_eq!(asked[0].url.query(), Some(query.as_str()));
}

// ---------------------------------------------------------------------------------------
// Devices at once
// ---------------------------------------------------------------------------------------

/// A stream that stays open (its body unread past the first chunk) while the test runs.
async fn open_stream(f: &Family, query: &str, headers: &[(&str, &str)]) -> Body {
    let res = f
        .app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/rest/stream?{query}"),
            headers,
            HOME,
        ))
        .await
        .expect("infallible");
    let mut body = res.into_body();
    let first = body.frame().await.expect("a chunk").expect("ok");
    assert_eq!(first.into_data().expect("data"), LIBRARY_AUDIO.as_bytes());
    body
}

#[tokio::test]
async fn a_stream_through_the_gate_is_tracked() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let body = open_stream(&f, &format!("{ALEX}&id=lib-1"), &[]).await;
    let live = f.state.family_audio.tracker().live();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].username, "alex");
    assert_eq!(live[0].device_id.as_deref(), Some(d_app().as_str()));
    assert_eq!(live[0].song_id, "lib-1");
    assert_eq!(live[0].bytes, LIBRARY_AUDIO.len() as u64);
    drop(body);
    assert!(f.state.family_audio.tracker().is_empty());
}

#[tokio::test]
async fn the_newest_device_wins_and_the_other_is_told() {
    let f = family(true, Alex::with(json!({ "devicesAtOnce": 1 })), |_| {}).await;
    let mut phone = open_stream(&f, &format!("{ALEX}&id=lib-1"), &[]).await;
    let _pixel = open_stream(
        &f,
        &format!("{ALEX_SUB}&id=lib-2"),
        &[("X-Octo-Device-Name", "Pixel 9")],
    )
    .await;
    // The phone's stream was ended.
    let ended = tokio::time::timeout(Duration::from_secs(2), phone.frame())
        .await
        .expect("ends at once");
    assert!(
        matches!(ended, Some(Err(_))),
        "the phone's body ends with an error"
    );
    // And while the Pixel plays, the phone is told where the music is now.
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=lib-1"), HOME).await;
    assert_eq!(r.error_code(), Some(50));
    assert!(r.body.contains("Playing on Pixel 9 now"), "{}", r.body);
}

#[tokio::test]
async fn offline_purpose_never_counts() {
    let f = family(true, Alex::with(json!({ "devicesAtOnce": 1 })), |_| {}).await;
    let _pixel = open_stream(&f, &format!("{ALEX_SUB}&id=lib-1"), &[]).await;
    let _copy = open_stream(&f, &format!("{ALEX}&id=lib-2"), &[("X-Octo-Purpose", "offline")]).await;
    // The Pixel keeps playing next to the Octo app's offline copy.
    let live = f.state.family_audio.tracker().live();
    let devices: Vec<Option<&str>> = live.iter().map(|s| s.device_id.as_deref()).collect();
    assert_eq!(devices, [Some(d_sub().as_str()), Some(d_app().as_str())]);
    assert_eq!(
        f.state
            .family_audio
            .tracker()
            .bumped_by("alex", Some(d_sub().as_str())),
        None
    );
    // Without offline copies the copy is refused.
    let f = family(true, Alex::with(json!({ "offlineCopies": false })), |_| {}).await;
    let r = send(
        &f,
        Method::GET,
        &format!("/rest/stream?{ALEX}&id=lib-1"),
        &[("X-Octo-Purpose", "offline")],
        HOME,
    )
    .await;
    assert_eq!(r.error_code(), Some(50));
}

#[tokio::test]
async fn radio_streams_count_on_the_device_that_opened_them() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let token = "r".repeat(48);
    let answer =
        format!(r#"{{"internetRadioStation":[{{"streamUrl":"http://octo/radio/stream/{token}"}}]}}"#);
    let mut caller = crate::services::family::FamilyCaller::member(
        "alex",
        Preset::Listener,
        octo_core::family::abilities::Abilities::preset(Preset::Listener),
    );
    caller.device_id = Some(d_sub());
    f.state
        .family_audio
        .note_radio_sessions(answer.as_bytes(), &caller);
    assert_eq!(
        f.state.family_audio.radio_opener(&token).as_deref(),
        Some(d_sub().as_str())
    );
    assert!(f.state.family_audio.radio_refusal("alex", &token).is_none());
}

// ---------------------------------------------------------------------------------------
// Tokens Octo did not sign
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_navidrome_token_naming_a_member_is_refused_on_rest() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let jwt = navidrome_jwt("alex");
    for endpoint in ["getFamily", "stream", "getAlbumList2"] {
        let r = get(
            &f,
            &format!("/rest/{endpoint}?jwt={jwt}&v=1.16.1&c=x&f=json&id=lib-1"),
            HOME,
        )
        .await;
        assert_eq!(r.error_code(), Some(40), "{endpoint}: {}", r.body);
        assert!(
            f.asked(&format!("/rest/{endpoint}")).await.is_empty(),
            "{endpoint}"
        );
    }
}

#[tokio::test]
async fn a_navidrome_token_naming_a_member_is_refused_on_the_native_api() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let jwt = navidrome_jwt("alex");
    let bearer = format!("Bearer {jwt}");
    for headers in [
        vec![("X-Nd-Authorization", bearer.as_str())],
        vec![("Authorization", bearer.as_str())],
    ] {
        let r = send(&f, Method::GET, "/api/song", &headers, HOME).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{headers:?}: {}", r.body);
    }
    let r = get(&f, &format!("/api/song?jwt={jwt}"), HOME).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{}", r.body);
    assert!(f.asked("/api/song").await.is_empty());
    // The member's own web token still works.
    let own = format!("Bearer {}", octo_web_token(&f));
    let r = send(
        &f,
        Method::GET,
        "/api/song",
        &[("X-Nd-Authorization", &own)],
        HOME,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

// ---------------------------------------------------------------------------------------
// Parameters wherever a request puts them
// ---------------------------------------------------------------------------------------

async fn send_body(f: &Family, uri: &str, content_type: &str, body: &str) -> Reply {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", content_type)
        .body(Body::from(body.to_string()))
        .expect("request");
    let address: SocketAddr = format!("{HOME}:50000").parse().expect("an address");
    req.extensions_mut().insert(ConnectInfo(address));
    let res = f.app.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    Reply {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

const BOUNDARY: &str = "multipart/form-data; boundary=XyZ";

fn multipart(name: &str, value: &str) -> String {
    format!("--XyZ\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n--XyZ--\r\n")
}

/// What Navidrome was sent for each `stream`: its query and its body.
async fn relayed_streams(f: &Family) -> Vec<(Vec<(String, String)>, String)> {
    f.asked("/rest/stream")
        .await
        .iter()
        .map(|r| {
            (
                pairs(r.url.query().unwrap_or("")),
                String::from_utf8_lossy(&r.body).into_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_capped_member_cannot_ask_for_the_original_in_a_json_body() {
    let f = family(true, capped(), |_| {}).await;
    let _ = send_body(
        &f,
        &format!("/rest/stream?{ALEX}&id=lib-1"),
        "application/json",
        r#"{"format":"raw","maxBitRate":"0"}"#,
    )
    .await;
    let relayed = relayed_streams(&f).await;
    assert_eq!(relayed.len(), 1, "{relayed:?}");
    let (sent, body) = &relayed[0];
    assert_eq!(value(sent, "format"), Some("opus"), "{relayed:?}");
    assert_eq!(value(sent, "maxBitRate"), Some("192"), "{relayed:?}");
    assert!(!body.contains("raw"), "{body}");
}

#[tokio::test]
async fn a_capped_member_cannot_ask_for_the_original_in_a_form_body_in_any_case() {
    let f = family(true, capped(), |_| {}).await;
    let _ = send_body(
        &f,
        &format!("/rest/stream?{ALEX}&id=lib-1"),
        "application/x-www-form-urlencoded",
        "Format=raw&MAXBITRATE=0",
    )
    .await;
    let relayed = relayed_streams(&f).await;
    assert_eq!(relayed.len(), 1, "{relayed:?}");
    let (sent, body) = &relayed[0];
    assert_eq!(value(sent, "format"), Some("opus"), "{relayed:?}");
    assert_eq!(value(sent, "maxBitRate"), Some("192"), "{relayed:?}");
    assert!(!body.to_ascii_lowercase().contains("raw"), "{body}");
}

#[tokio::test]
async fn a_capped_member_gets_the_family_limit_whatever_the_body_type() {
    // In the address.
    let f = family(true, capped(), |_| {}).await;
    let _ = get(
        &f,
        &format!("/rest/stream?{ALEX}&id=lib-1&format=raw&maxBitRate=0"),
        HOME,
    )
    .await;
    let sent = f.streamed().await;
    assert_eq!(value(&sent, "format"), Some("opus"));
    assert_eq!(value(&sent, "maxBitRate"), Some("192"));
    // In a JSON body whose type has a charset, and in a form body next to the song.
    for (content_type, body) in [
        (
            "application/json; charset=utf-8",
            r#"{"FORMAT":"raw","maxbitrate":0}"#,
        ),
        (
            "application/x-www-form-urlencoded",
            "id=lib-1&format=raw&maxBitRate=0",
        ),
    ] {
        let f = family(true, capped(), |_| {}).await;
        let _ = send_body(&f, &format!("/rest/stream?{ALEX}&id=lib-1"), content_type, body).await;
        let relayed = relayed_streams(&f).await;
        assert_eq!(relayed.len(), 1, "{content_type}: {relayed:?}");
        assert_eq!(value(&relayed[0].0, "format"), Some("opus"), "{content_type}");
        assert_eq!(value(&relayed[0].0, "maxBitRate"), Some("192"), "{content_type}");
    }
    // In a multipart form, which Octo cannot rewrite: refused, nothing relayed.
    let f = family(true, capped(), |_| {}).await;
    let r = send_body(
        &f,
        &format!("/rest/stream?{ALEX}&id=lib-1"),
        BOUNDARY,
        &multipart("format", "raw"),
    )
    .await;
    assert_eq!(r.error_code(), Some(0), "{}", r.body);
    assert!(f.asked("/rest/stream").await.is_empty());
}

async fn kid() -> Family {
    let f = family(
        true,
        Alex {
            preset: Preset::Kid,
            edits: json!({}),
        },
        |_| {},
    )
    .await;
    Mock::given(path("/rest/getSong"))
        .and(query_param("id", "lib-x"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "subsonic-response": {"status": "ok", "song": {"id": "lib-x", "explicitStatus": "explicit", "bitRate": 320}}
        })))
        .with_priority(1)
        .mount(&f.server)
        .await;
    f
}

#[tokio::test]
async fn kid_transcode_stream_reads_only_the_media_id() {
    let f = kid().await;
    let r = get(
        &f,
        &format!("/rest/getTranscodeStream?{ALEX}&mediaId=lib-x&mediaType=song&transcodeParams=tp&id=lib-1"),
        HOME,
    )
    .await;
    assert_eq!(r.error_code(), Some(50), "{}", r.body);
    assert!(f.asked("/rest/getTranscodeStream").await.is_empty());
}

#[tokio::test]
async fn kid_transcode_stream_with_the_media_id_twice_is_refused() {
    let f = kid().await;
    let r = get(
        &f,
        &format!(
            "/rest/getTranscodeStream?{ALEX}&mediaId=lib-x&mediaId=lib-x&mediaType=song&transcodeParams=tp"
        ),
        HOME,
    )
    .await;
    assert_eq!(r.error_code(), Some(50), "{}", r.body);
    assert!(f.asked("/rest/getTranscodeStream").await.is_empty());
}

#[tokio::test]
async fn kid_stream_with_the_id_repeated_in_any_case_is_refused() {
    let f = kid().await;
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=lib-x&ID=lib-x"), HOME).await;
    assert_eq!(r.error_code(), Some(50), "{}", r.body);
    assert!(f.asked("/rest/stream").await.is_empty());
    // Two different songs at once are refused, whichever one is clean.
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=lib-1&id=lib-x"), HOME).await;
    assert_eq!(r.error_code(), Some(0), "{}", r.body);
    assert!(f.asked("/rest/stream").await.is_empty());
}

#[tokio::test]
async fn kid_rules_hold_for_a_song_named_in_any_body() {
    let f = kid().await;
    let stream = format!("/rest/stream?{ALEX}");
    let clean_in_query = format!("/rest/stream?{ALEX}&id=lib-1");
    for (uri, content_type, body, code) in [
        (
            stream.as_str(),
            "application/x-www-form-urlencoded",
            "id=lib-x".to_string(),
            50,
        ),
        (
            stream.as_str(),
            "application/x-www-form-urlencoded",
            "iD=lib-x".to_string(),
            50,
        ),
        (
            stream.as_str(),
            "application/json",
            r#"{"id":"lib-x"}"#.to_string(),
            50,
        ),
        (
            stream.as_str(),
            "application/json",
            r#"{"Id":"lib-x"}"#.to_string(),
            50,
        ),
        (stream.as_str(), BOUNDARY, multipart("id", "lib-x"), 0),
        (
            clean_in_query.as_str(),
            "application/x-www-form-urlencoded",
            "id=lib-x".to_string(),
            0,
        ),
        (
            clean_in_query.as_str(),
            "application/json",
            r#"{"id":"lib-x"}"#.to_string(),
            0,
        ),
    ] {
        let r = send_body(&f, uri, content_type, &body).await;
        assert_eq!(
            r.error_code(),
            Some(code),
            "{uri} {content_type} {body}: {}",
            r.body
        );
        assert!(f.asked("/rest/stream").await.is_empty(), "{content_type} {body}");
    }
    // A clean song named in a form body plays.
    let _ = send_body(&f, &stream, "application/x-www-form-urlencoded", "id=lib-1").await;
    assert_eq!(f.asked("/rest/stream").await.len(), 1);
}

// ---------------------------------------------------------------------------------------
// Offline copies
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn offline_purpose_counts_only_from_a_paired_octo_app() {
    let f = family(true, Alex::with(json!({ "devicesAtOnce": 1 })), |_| {}).await;
    let _phone = open_stream(&f, &format!("{ALEX}&id=lib-1"), &[]).await;
    // Another app saying "offline" is playing like any other, so the newest device wins.
    let _pixel = open_stream(
        &f,
        &format!("{ALEX_SUB}&id=lib-2"),
        &[("X-Octo-Purpose", "offline")],
    )
    .await;
    assert_eq!(
        f.state
            .family_audio
            .tracker()
            .bumped_by("alex", Some(d_app().as_str()))
            .as_deref(),
        Some("Pixel 9")
    );
    let live = f.state.family_audio.tracker().live();
    let pixel = live
        .iter()
        .find(|s| s.device_id.as_deref() == Some(d_sub().as_str()))
        .expect("the Pixel plays");
    assert_eq!(
        pixel.purpose,
        crate::services::family::stream_tracker::StreamPurpose::Play
    );
    // And the offline switch does not apply to it, since it is not an offline copy.
    let f = family(true, Alex::with(json!({ "offlineCopies": false })), |_| {}).await;
    let r = send(
        &f,
        Method::GET,
        &format!("/rest/stream?{ALEX_SUB}&id=lib-1"),
        &[("X-Octo-Purpose", "offline")],
        HOME,
    )
    .await;
    assert_eq!(r.body, LIBRARY_AUDIO);
}

// ---------------------------------------------------------------------------------------
// A reverse proxy Octo does not trust
// ---------------------------------------------------------------------------------------

const BRIDGE: &str = "172.18.0.5";

#[tokio::test]
async fn an_untrusted_proxy_at_home_makes_everyone_away() {
    let f = family(true, Alex::with(json!({ "away": false })), |_| {}).await;
    let through = |client: &'static str| vec![("X-Forwarded-For", client)];
    let r = send(
        &f,
        Method::GET,
        &format!("/rest/stream?{ALEX}&id=lib-1"),
        &through("192.168.1.9"),
        BRIDGE,
    )
    .await;
    assert_eq!(r.error_code(), Some(50), "{}", r.body);
    assert!(r.body.contains("away from home"), "{}", r.body);
    // The same address with no proxy header is a person at home.
    let r = get(&f, &format!("/rest/stream?{ALEX}&id=lib-1"), BRIDGE).await;
    assert_eq!(r.body, LIBRARY_AUDIO);
    // Once the proxy is trusted, the person it names is home again.
    let f = family(true, Alex::with(json!({ "away": false })), |s| {
        s.family.trusted_proxies = format!("127.0.0.0/8, ::1/128, {BRIDGE}");
    })
    .await;
    let r = send(
        &f,
        Method::GET,
        &format!("/rest/stream?{ALEX}&id=lib-1"),
        &through("192.168.1.9"),
        BRIDGE,
    )
    .await;
    assert_eq!(r.body, LIBRARY_AUDIO);
}

#[tokio::test]
async fn wrong_tries_behind_an_untrusted_proxy_count_per_client() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let wrong = "u=alex&p=wrong-secret&v=1.16.1&c=x&f=json";
    for _ in 0..crate::services::family::member_bridge::MAX_FAILURES {
        let r = send(
            &f,
            Method::GET,
            &format!("/rest/ping?{wrong}"),
            &[("X-Forwarded-For", "198.51.100.7")],
            BRIDGE,
        )
        .await;
        assert_eq!(r.error_code(), Some(40));
    }
    // That client now waits, even with the right secret.
    let r = send(
        &f,
        Method::GET,
        &format!("/rest/ping?{ALEX}"),
        &[("X-Forwarded-For", "198.51.100.7")],
        BRIDGE,
    )
    .await;
    assert_eq!(r.error_code(), Some(40), "{}", r.body);
    // Another client behind the same proxy does not.
    let r = send(
        &f,
        Method::GET,
        &format!("/rest/ping?{ALEX}"),
        &[("X-Forwarded-For", "198.51.100.8")],
        BRIDGE,
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
}

// ---------------------------------------------------------------------------------------
// One reading of every request
// ---------------------------------------------------------------------------------------

/// A POST whose body is raw bytes, from the home address.
async fn send_bytes(f: &Family, uri: &str, content_type: &str, body: Vec<u8>) -> Reply {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", content_type)
        .body(Body::from(body))
        .expect("request");
    let address: SocketAddr = format!("{HOME}:50000").parse().expect("an address");
    req.extensions_mut().insert(ConnectInfo(address));
    let res = f.app.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    Reply {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// Navidrome takes only the guest's sign-in.
async fn only_guest_signs_in(f: &Family) {
    Mock::given(path("/rest/ping"))
        .and(query_param("u", "guest"))
        .and(query_param("p", "guest-pw"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            "application/json",
        ))
        .with_priority(1)
        .mount(&f.server)
        .await;
    Mock::given(path("/rest/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#,
            "application/json",
        ))
        .with_priority(2)
        .mount(&f.server)
        .await;
}

/// The gate's refusal of a body it cannot read.
fn assert_unreadable(r: &Reply) {
    assert_eq!(r.error_code(), Some(0), "{}", r.body);
    assert!(
        r.json()["subsonic-response"]["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("could not read")),
        "{}",
        r.body
    );
}

/// A multipart form with no boundary cannot be read; the handlers once fell back to reading it
/// as a query, so a guest's sign-in in it went to Navidrome while the gate saw the owner named
/// in the address.
#[tokio::test]
async fn an_owner_named_in_the_address_with_a_guest_sign_in_in_a_boundaryless_form_is_refused() {
    let f = family(true, Alex::listener(), |_| {}).await;
    only_guest_signs_in(&f).await;
    let devices_before = f.state.family_store.devices_of("alex").len();
    let r = send_bytes(
        &f,
        "/rest/addFamilyDevice?u=winters&v=1.16.1&c=x&f=json&kind=SubsonicApp&name=x&username=alex",
        "multipart/form-data",
        b"u=guest&p=guest-pw".to_vec(),
    )
    .await;
    assert!(
        !r.body.contains("appPassword"),
        "an app password for alex went out: {}",
        r.body
    );
    assert_eq!(f.state.family_store.devices_of("alex").len(), devices_before);
    assert_unreadable(&r);
    assert!(f.asked("/rest/ping").await.is_empty(), "nothing was checked");
}

/// The same with a boundary longer than the form reader takes (over 70 characters).
#[tokio::test]
async fn an_owner_named_in_the_address_with_a_guest_sign_in_behind_a_long_boundary_is_refused() {
    let f = family(true, Alex::listener(), |_| {}).await;
    only_guest_signs_in(&f).await;
    let boundary = "b".repeat(80);
    let r = send_bytes(
        &f,
        "/rest/getFamilyDevices?u=winters&v=1.16.1&c=x&f=json&all=true",
        &format!("multipart/form-data; boundary={boundary}"),
        b"u=guest&p=guest-pw".to_vec(),
    )
    .await;
    assert!(
        !r.body.contains(&d_app()),
        "every member's devices went out: {}",
        r.body
    );
    assert_unreadable(&r);
}

/// The owner named in the address and a guest in a readable form body name two people, so
/// nobody is checked and the family call is refused.
#[tokio::test]
async fn an_owner_in_the_address_and_a_guest_in_a_form_body_is_refused() {
    let f = family(true, Alex::listener(), |_| {}).await;
    only_guest_signs_in(&f).await;
    let r = send_bytes(
        &f,
        "/rest/getFamilyDevices?u=winters&v=1.16.1&c=x&f=json&all=true",
        "application/x-www-form-urlencoded",
        b"u=guest&p=guest-pw".to_vec(),
    )
    .await;
    assert!(!r.body.contains(&d_app()), "{}", r.body);
    assert_eq!(r.error_code(), Some(40), "{}", r.body);
}

/// The owner's sign-in split between the address and a form body is checked whole, as it was
/// read: Navidrome is asked with exactly that `u` and `p`.
#[tokio::test]
async fn the_owner_is_checked_with_exactly_the_sign_in_the_gate_read() {
    let f = family(true, Alex::listener(), |_| {}).await;
    Mock::given(path("/rest/ping"))
        .and(query_param("u", "winters"))
        .and(query_param("p", "owner-pw"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            "application/json",
        ))
        .with_priority(1)
        .mount(&f.server)
        .await;
    Mock::given(path("/rest/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#,
            "application/json",
        ))
        .with_priority(2)
        .mount(&f.server)
        .await;
    let r = send_bytes(
        &f,
        "/rest/getFamilyDevices?u=winters&v=1.16.1&c=x&f=json&all=true",
        "application/x-www-form-urlencoded",
        b"p=owner-pw".to_vec(),
    )
    .await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
    let pinged = f.asked("/rest/ping").await;
    assert_eq!(pinged.len(), 1, "one check, kept for the next call");
    let sent = pairs(pinged[0].url.query().unwrap_or(""));
    assert_eq!(value(&sent, "u"), Some("winters"));
    assert_eq!(value(&sent, "p"), Some("owner-pw"));
    assert!(
        value(&sent, "all").is_none(),
        "only the sign-in is sent: {sent:?}"
    );
}

/// A family playlist change with a body the gate cannot read never reaches the playlist code.
#[tokio::test]
async fn a_playlist_change_with_a_body_the_gate_cannot_read_is_refused() {
    let f = family(true, Alex::listener(), |_| {}).await;
    only_guest_signs_in(&f).await;
    let r = send_bytes(
        &f,
        "/rest/updatePlaylist?u=winters&v=1.16.1&c=x&f=json&playlistId=fp_1&name=x",
        "multipart/form-data",
        b"u=guest&p=guest-pw".to_vec(),
    )
    .await;
    assert_unreadable(&r);
}

/// A JSON body that is not one object cannot be read either.
#[tokio::test]
async fn a_json_body_that_is_not_an_object_is_refused() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let r = send_bytes(
        &f,
        &format!("/rest/getFamily?{OWNER}"),
        "application/json",
        b"[\"u\",\"guest\"]".to_vec(),
    )
    .await;
    assert_unreadable(&r);
    // An empty one is nothing to read, and goes on.
    let r = send_bytes(&f, &format!("/rest/ping?{OWNER}"), "application/json", Vec::new()).await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
}

/// `download` goes through the catch-all. A JSON body with a byte order mark, or bytes that are
/// not UTF-8, is read as the parameters read it, so the family limit holds wherever it is read.
#[tokio::test]
async fn a_capped_download_with_a_json_body_read_loosely_never_asks_for_the_original() {
    for body in [
        "\u{feff}{\"format\":\"raw\",\"bitrate\":\"0\"}"
            .as_bytes()
            .to_vec(),
        b"{\"format\":\"raw\",\"bitrate\":\"0\",\"x\":\"\xff\"}".to_vec(),
    ] {
        let f = family(
            true,
            Alex::with(json!({ "downloadFiles": true, "streamCap": 192 })),
            |_| {},
        )
        .await;
        let _ = send_bytes(
            &f,
            &format!("/rest/download?{ALEX}&id=lib-1"),
            "application/json",
            body.clone(),
        )
        .await;
        let asked = f.asked("/rest/download").await;
        assert_eq!(asked.len(), 1, "one download relayed");
        let sent = pairs(asked[0].url.query().unwrap_or(""));
        let formats: Vec<&str> = sent
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("format"))
            .map(|(_, v)| v.as_str())
            .collect();
        assert!(
            !formats.contains(&"raw"),
            "Navidrome was asked for the original: {sent:?} (body {:?})",
            String::from_utf8_lossy(&body)
        );
        assert!(
            !String::from_utf8_lossy(&asked[0].body).contains("raw"),
            "the body still asks for the original: {:?}",
            String::from_utf8_lossy(&asked[0].body)
        );
    }
}

// ---------------------------------------------------------------------------------------
// Throttles across addresses
// ---------------------------------------------------------------------------------------

/// A phone on the home network, talking to Octo directly, names a new X-Forwarded-For hop on
/// every try: the tries still count against the address that connected and against the
/// account, so a member secret cannot be guessed without end.
#[tokio::test]
async fn a_home_device_naming_new_forwarded_hops_is_still_throttled() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let wrong = "u=alex&p=wrong-secret&v=1.16.1&c=x&f=json";
    for n in 0..120 {
        let hop = format!("203.0.{}.{}", n / 200, n % 200 + 1);
        let r = send(
            &f,
            Method::GET,
            &format!("/rest/ping?{wrong}"),
            &[("X-Forwarded-For", hop.as_str())],
            HOME,
        )
        .await;
        assert_eq!(r.error_code(), Some(40));
    }
    let r = send(
        &f,
        Method::GET,
        &format!("/rest/ping?{ALEX}"),
        &[("X-Forwarded-For", "198.51.100.200")],
        HOME,
    )
    .await;
    assert_eq!(
        r.error_code(),
        Some(40),
        "120 wrong tries from one device, and a sign-in still goes through: {}",
        r.body
    );
}

/// Wrong tries for one account from many addresses count together.
#[tokio::test]
async fn wrong_tries_for_one_account_from_many_addresses_count_together() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let wrong = "u=alex&p=wrong-secret&v=1.16.1&c=x&f=json";
    for n in 0..crate::services::family::member_bridge::ACCOUNT_FAILURES {
        let from = format!("203.0.{}.{}", n / 200, n % 200 + 1);
        let r = get(&f, &format!("/rest/ping?{wrong}"), &from).await;
        assert_eq!(r.error_code(), Some(40));
    }
    let r = get(&f, &format!("/rest/ping?{ALEX}"), "198.51.100.9").await;
    assert_eq!(r.error_code(), Some(40), "{}", r.body);
    // Anyone else is not held back.
    let r = get(&f, &format!("/rest/ping?{OWNER}"), "198.51.100.9").await;
    assert_eq!(r.json()["subsonic-response"]["status"], "ok", "{}", r.body);
}

/// The web player's sign-in counts against the address that connected however many hops it
/// names.
#[tokio::test]
async fn web_player_sign_ins_naming_new_hops_are_still_throttled() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let sign_in = |password: &str, hop: String| {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/auth/login")
            .header("content-type", "application/json")
            .header("X-Forwarded-For", hop)
            .body(Body::from(
                json!({ "username": "alex", "password": password }).to_string(),
            ))
            .expect("request");
        let address: SocketAddr = format!("{HOME}:50000").parse().expect("an address");
        req.extensions_mut().insert(ConnectInfo(address));
        f.app.clone().oneshot(req)
    };
    for n in 0..crate::services::family::member_bridge::PEER_FAILURES {
        let res = sign_in("wrong-guess", format!("203.0.{}.{}", n / 200, n % 200 + 1))
            .await
            .expect("infallible");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
    // The right secret now waits too.
    let res = sign_in(APP_SECRET, "198.51.100.77".into())
        .await
        .expect("infallible");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// The web player's sign-in takes a small body only.
#[tokio::test]
async fn a_huge_web_player_sign_in_is_refused_unread() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let password = "x".repeat(64 * 1024);
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "username": "alex", "password": password }).to_string(),
        ))
        .expect("request");
    let address: SocketAddr = format!("{HOME}:50000").parse().expect("an address");
    req.extensions_mut().insert(ConnectInfo(address));
    let res = f.app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// ---------------------------------------------------------------------------------------
// Clean-only downloads and device notes
// ---------------------------------------------------------------------------------------

/// A clean-only member allowed to download asks `download` for an album id: Navidrome would
/// send a zip of every track, explicit ones too, so only a song the index knows goes through.
#[tokio::test]
async fn a_clean_only_download_of_an_album_id_is_refused() {
    let f = family(
        true,
        Alex {
            preset: Preset::Kid,
            edits: json!({ "downloadFiles": true }),
        },
        |_| {},
    )
    .await;
    let r = get(&f, &format!("/rest/download?{ALEX}&id=al-explicit-album"), HOME).await;
    assert!(
        f.asked("/rest/download").await.is_empty(),
        "the album went to Navidrome for a zip: {}",
        r.body
    );
    assert_eq!(r.error_code(), Some(50), "{}", r.body);
    // A song the index does not know is refused the same way.
    let r = get(&f, &format!("/rest/download?{ALEX}&id=lib-unknown"), HOME).await;
    assert_eq!(r.error_code(), Some(50), "{}", r.body);
    assert!(f.asked("/rest/download").await.is_empty());
    // A song the index knows, with no mark, downloads.
    f.index_song(false).await;
    let r = get(&f, &format!("/rest/download?{ALEX}&id=lib-9"), HOME).await;
    assert_eq!(f.asked("/rest/download").await.len(), 1, "{}", r.body);
}

/// A wrong owner password beside an outside song id: the catch-all answers an outside id with
/// a plain ok and never asks Navidrome, so a device is noted only once Navidrome itself takes
/// the sign-in.
#[tokio::test]
async fn a_wrong_owner_password_beside_an_outside_id_notes_no_device() {
    let f = family(true, Alex::listener(), |_| {}).await;
    only_guest_signs_in(&f).await;
    let before = f.state.family_store.detected().len();
    for n in 0..5 {
        let device = format!("spoofed-{n}");
        let r = send(
            &f,
            Method::GET,
            "/rest/getSomethingNew?u=winters&p=wrong&v=1.16.1&c=Spoof&f=json&id=ext-deezer-song-1",
            &[("X-Octo-Device-Id", device.as_str())],
            HOME,
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    }
    assert_eq!(
        f.state.family_store.detected().len(),
        before,
        "devices noted for a sign-in nobody checked: {:?}",
        f.state.family_store.detected()
    );
}

/// The owner's right sign-in still notes the device it came from, once Navidrome takes it.
#[tokio::test]
async fn a_right_owner_sign_in_notes_its_device_once_navidrome_takes_it() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let r = send(
        &f,
        Method::GET,
        &format!("/rest/getSomethingNew?{OWNER}&id=ext-deezer-song-1"),
        &[("X-Octo-Device-Id", "owner-phone")],
        HOME,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let detected = f.state.family_store.detected();
    assert_eq!(detected.len(), 1, "{detected:?}");
    assert_eq!(detected[0].username, "winters");
    assert_eq!(f.asked("/rest/ping").await.len(), 1, "checked once");
    // The same device again within a minute is not checked again only to be noted.
    let _ = send(
        &f,
        Method::GET,
        &format!("/rest/getSomethingNew?{OWNER}&id=ext-deezer-song-1"),
        &[("X-Octo-Device-Id", "owner-phone")],
        HOME,
    )
    .await;
    assert_eq!(f.asked("/rest/ping").await.len(), 1);
}

// ---- Imports need the import ability ---------------------------------------------------------

/// A multipart `importFile` with one small list, sent as `who`.
async fn import_file(f: &Family, who: &str) -> Reply {
    let boundary = "octo-test-boundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"Mine.txt\"\r\n\r\nMassive Attack - Teardrop\r\n--{boundary}--\r\n"
    );
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(format!("/rest/importFile.view?{who}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .expect("request");
    let address: SocketAddr = format!("{HOME}:50000").parse().expect("an address");
    req.extensions_mut().insert(ConnectInfo(address));
    let res = f.app.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    Reply {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

#[tokio::test]
async fn imports_are_refused_when_the_import_ability_is_off() {
    let f = family(true, Alex::with(json!({ "importPlaylists": false })), |_| {}).await;
    for call in ["getImportServices", "importText", "importAction"] {
        let r = get(
            &f,
            &format!("/rest/{call}?{ALEX}&action=pause&text=A%20-%20B"),
            HOME,
        )
        .await;
        assert_eq!(r.error_code(), Some(50), "{call}: {}", r.body);
        assert!(
            r.body.contains("Importing playlists is off for this account."),
            "{call}: {}",
            r.body
        );
    }
    let r = import_file(&f, ALEX).await;
    assert_eq!(r.error_code(), Some(50), "{}", r.body);
    assert!(f.state.imports.overview("alex").lists.is_empty());
    // Reading the lists already there stays open.
    let lists = get(&f, &format!("/rest/getImports?{ALEX}"), HOME).await;
    assert_eq!(
        lists.json()["subsonic-response"]["status"],
        "ok",
        "{}",
        lists.body
    );
    // The owner is never asked about abilities.
    let owner = get(&f, &format!("/rest/getImportServices?{OWNER}"), HOME).await;
    assert_eq!(
        owner.json()["subsonic-response"]["status"],
        "ok",
        "{}",
        owner.body
    );
}

#[tokio::test]
async fn a_member_with_the_import_ability_imports_from_a_file_and_pasted_text() {
    let f = family(true, Alex::listener(), |_| {}).await;
    let services = get(&f, &format!("/rest/getImportServices?{ALEX}"), HOME).await;
    let answer = services.json();
    assert_eq!(
        answer["subsonic-response"]["importServices"]["services"]
            .as_array()
            .map(Vec::len),
        Some(10),
        "{}",
        services.body
    );
    let file = import_file(&f, ALEX).await;
    assert_eq!(file.status, StatusCode::OK);
    assert_eq!(
        file.json()["subsonic-response"]["importAction"]["ok"],
        true,
        "{}",
        file.body
    );
    let text = get(
        &f,
        &format!("/rest/importText?{ALEX}&name=Pasted&text=Air%20-%20La%20Femme%20d%27Argent"),
        HOME,
    )
    .await;
    assert_eq!(
        text.json()["subsonic-response"]["importAction"]["ok"],
        true,
        "{}",
        text.body
    );
    let names: Vec<String> = f
        .state
        .imports
        .overview("alex")
        .lists
        .iter()
        .map(|l| l.name.clone())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    // A Listener asks before anything is fetched, and is told who says yes.
    let lists = get(&f, &format!("/rest/getImports?{ALEX}"), HOME).await;
    assert_eq!(
        lists.json()["subsonic-response"]["imports"]["approval"],
        json!({"needed": true, "by": "winters"}),
        "{}",
        lists.body
    );
}
