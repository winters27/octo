//! The import page's endpoints. The dashboard's need its sign-in, the apps' check the caller with
//! Navidrome, and both answer in the shapes the page and the apps read.

use axum::body::Body;
use axum::http::{Method, Request};
use http_body_util::BodyExt;
use octo_core::settings::{AppSettings, FamilySettings, SubsonicSettings};
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::test_app::{Reply, app, get, send};
use super::*;
use crate::app::TestServices;
use crate::http::pipeline::App;

/// The fake Navidrome: a ping that takes `t=good`, a login, one library song.
async fn navidrome() -> MockServer {
    let server = MockServer::start().await;
    let json =
        |body: &str| ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json");
    Mock::given(path_regex("/rest/ping(\\.view)?$"))
        .and(query_param("t", "good"))
        .respond_with(json(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(path_regex("/rest/ping(\\.view)?$"))
        .respond_with(json(
            r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(path_regex("/rest/getOpenSubsonicExtensions"))
        .respond_with(json(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","openSubsonic":true,"openSubsonicExtensions":[{"name":"formPost","versions":[1]}]}}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(json(
            r#"{"token":"jwt","isAdmin":true,"username":"admin","subsonicToken":"t","subsonicSalt":"s"}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .respond_with(json(
            r#"[{"id":"u-admin","userName":"admin","name":"Admin","isAdmin":true},{"id":"u-alice","userName":"alice","name":"Alice","isAdmin":false},{"id":"u-bob","userName":"Bob","name":"Bob","isAdmin":false},{"id":"u-alex","userName":"alex","name":"Alex","isAdmin":false}]"#,
        ))
        .mount(&server)
        .await;
    Mock::given(path("/api/song"))
        .respond_with(json(
            r#"[{"id":"nd-1","path":"/m/a.flac","suffix":"flac","bitRate":900,"title":"Teardrop","artist":"Massive Attack","duration":330.2}]"#,
        ))
        .mount(&server)
        .await;
    server
}

struct Rig {
    _navidrome: MockServer,
    state: AppState,
    app: App,
}

async fn rig() -> Rig {
    rig_with(false).await
}

/// [`rig`] with Family on or off.
async fn rig_with(family: bool) -> Rig {
    let server = navidrome().await;
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                admin_username: Some("admin".into()),
                admin_password: Some("synthetic".into()),
                ..Default::default()
            },
            family: FamilySettings {
                enabled: family,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices::default(),
    );
    Rig {
        _navidrome: server,
        app: app(state.clone()),
        state,
    }
}

impl Rig {
    fn sign_in(&self, user: &str) -> String {
        self.state.browse_sessions.create(user)
    }

    async fn upload(&self, token: &str, file_name: &str, text: &str) -> Reply {
        let boundary = "octo-test-boundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: text/csv\r\n\r\n{text}\r\n--{boundary}--\r\n"
        );
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/imports/file")
            .header("X-Octo-Admin", "1")
            .header("X-Octo-Browse-Token", token)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .expect("request");
        let response = self.app.clone().oneshot(request).await.expect("infallible");
        let (parts, body) = response.into_parts();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: body.collect().await.expect("body").to_bytes().to_vec(),
        }
    }
}

#[tokio::test]
async fn every_dashboard_endpoint_needs_the_sign_in() {
    let rig = rig().await;
    let cases = [
        (Method::GET, "/api/admin/imports"),
        (Method::GET, "/api/admin/imports/lists/spotify-liked"),
        (Method::POST, "/api/admin/imports/link"),
        (Method::POST, "/api/admin/imports/lists/x/fetch"),
        (Method::POST, "/api/admin/imports/lists/x/stream"),
        (Method::POST, "/api/admin/imports/trickle/pause"),
        (Method::DELETE, "/api/admin/imports/lists/x"),
    ];
    for (verb, uri) in cases {
        let body = (verb == Method::POST).then_some(r#"{"on":true}"#);
        let reply = send(&rig.app, verb.clone(), uri, &[("X-Octo-Admin", "1")], body).await;
        assert_eq!(
            reply.status,
            StatusCode::UNAUTHORIZED,
            "{verb} {uri}: {}",
            reply.text()
        );
    }
}

#[tokio::test]
async fn the_dashboard_and_subsonic_import_actions_toggle_the_same_streaming_snapshot() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let uploaded = rig
        .upload(
            &token,
            "Snapshot.csv",
            "Track Name,Artist Name(s)\nTeardrop,Massive Attack\nAngel,Massive Attack\n",
        )
        .await
        .json();
    let id = uploaded["listId"].as_str().unwrap();
    let auth = [("X-Octo-Browse-Token", token.as_str()), ("X-Octo-Admin", "1")];
    let enabled = send(
        &rig.app,
        Method::POST,
        &format!("/api/admin/imports/lists/{id}/stream"),
        &auth,
        Some(r#"{"on":true}"#),
    )
    .await;
    assert_eq!(enabled.status, StatusCode::OK, "{}", enabled.text());
    let detail = send(
        &rig.app,
        Method::GET,
        &format!("/api/admin/imports/lists/{id}"),
        &auth,
        None,
    )
    .await
    .json();
    assert_eq!(detail["list"]["streamWithoutDownloading"], true);
    assert!(
        detail["list"]["streamingPlaylistId"]
            .as_str()
            .unwrap()
            .starts_with("oi")
    );
    assert_eq!(detail["list"]["keepPlaylist"], false);
    assert_eq!(detail["list"]["getMissing"], false);
    assert!(rig.state.imports.streaming_lists("bob").is_empty());
    assert_eq!(rig.state.imports.overview("alice").trickle.queued, 0);

    let off = get(
        &rig.app,
        &format!(
            "/rest/importAction.view?u=alice&t=good&s=salt&v=1.16.1&c=test&action=stream&id={id}&on=false"
        ),
    )
    .await
    .json();
    assert_eq!(off["subsonic-response"]["importAction"]["ok"], true);
    assert!(rig.state.imports.streaming_lists("alice").is_empty());
}

#[tokio::test]
async fn the_dashboard_shows_the_signed_in_persons_lists_and_adds_a_file() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let upload = rig
        .upload(
            &token,
            "Gym.csv",
            "Track Name,Artist Name(s)\nTeardrop,Massive Attack\nAngel,Massive Attack\n",
        )
        .await;
    assert_eq!(upload.status, StatusCode::OK, "{}", upload.text());
    let added = upload.json();
    assert_eq!(added["message"], "Added Gym, 2 songs.");
    let list_id = added["listId"].as_str().expect("an id").to_string();

    let auth = [("X-Octo-Browse-Token", token.as_str()), ("X-Octo-Admin", "1")];
    let page = send(&rig.app, Method::GET, "/api/admin/imports", &auth, None)
        .await
        .json();
    assert_eq!(page["user"], "alice");
    let overview = &page["overview"];
    assert!(overview.get("spotify").is_none(), "{overview}");
    assert!(overview.get("reading").is_none(), "{overview}");
    let lists = overview["lists"].as_array().expect("lists");
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0]["have"], 1);
    assert_eq!(lists[0]["missing"], 1);
    assert_eq!(overview["trickle"]["state"], "idle");

    let fetch = send(
        &rig.app,
        Method::POST,
        &format!("/api/admin/imports/lists/{list_id}/fetch"),
        &auth,
        Some(r#"{"on":true}"#),
    )
    .await;
    assert_eq!(fetch.status, StatusCode::OK, "{}", fetch.text());
    let detail = send(
        &rig.app,
        Method::GET,
        &format!("/api/admin/imports/lists/{list_id}"),
        &auth,
        None,
    )
    .await
    .json();
    let states: Vec<&str> = detail["tracks"]
        .as_array()
        .expect("tracks")
        .iter()
        .map(|t| t["state"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(states, ["have", "queued"]);
}

#[tokio::test]
async fn a_write_without_the_admin_header_is_refused() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let reply = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/link",
        &[("X-Octo-Browse-Token", token.as_str())],
        Some(r#"{"url":"https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn there_is_no_spotify_sign_in() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let auth = [("X-Octo-Browse-Token", token.as_str()), ("X-Octo-Admin", "1")];
    for uri in [
        "/api/admin/imports/spotify/connect",
        "/api/admin/imports/spotify/finish",
        "/api/admin/imports/spotify/read",
        "/api/admin/imports/spotify/disconnect",
    ] {
        let reply = send(&rig.app, Method::POST, uri, &auth, Some("{}")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}: {}", reply.text());
    }
    let callback = get(&rig.app, "/imports/spotify/callback?code=abc&state=nope").await;
    assert_ne!(callback.status, StatusCode::OK, "{}", callback.text());
    assert!(
        !callback.text().contains("Spotify sign-in"),
        "{}",
        callback.text()
    );
}

const AUTH: &str = "u=alice&t=good&s=salt&v=1.16.1&c=Octo&f=json";

#[tokio::test]
async fn the_apps_see_the_same_lists_as_whoever_they_signed_in_as() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let upload = rig
        .upload(
            &token,
            "Gym.csv",
            "Track Name,Artist Name(s)\nTeardrop,Massive Attack\n",
        )
        .await;
    assert_eq!(upload.status, StatusCode::OK, "{}", upload.text());

    let all = get(&rig.app, &format!("/rest/getImports.view?{AUTH}"))
        .await
        .json();
    let envelope = &all["subsonic-response"];
    assert_eq!(envelope["status"], "ok");
    let list = &envelope["imports"]["lists"][0];
    assert_eq!(list["name"], "Gym");

    let one = get(
        &rig.app,
        &format!(
            "/rest/getImport?{AUTH}&id={}",
            list["id"].as_str().unwrap_or_default()
        ),
    )
    .await
    .json();
    assert_eq!(one["subsonic-response"]["import"]["tracks"][0]["state"], "have");

    let bob = get(
        &rig.app,
        "/rest/getImports?u=bob&t=good&s=salt&v=1.16.1&c=Octo&f=json",
    )
    .await
    .json();
    assert_eq!(
        bob["subsonic-response"]["imports"]["lists"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn an_app_action_answers_in_words_and_a_wrong_password_is_refused() {
    let rig = rig().await;
    let pause = get(&rig.app, &format!("/rest/importAction?{AUTH}&action=pause"))
        .await
        .json();
    let answer = &pause["subsonic-response"]["importAction"];
    assert_eq!(answer["ok"], true);
    assert!(
        answer["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Paused")
    );

    for action in ["connect", "finish", "read", "disconnect"] {
        let gone = get(
            &rig.app,
            &format!("/rest/importAction?{AUTH}&action={action}&redirect=http://127.0.0.1:5555/callback"),
        )
        .await
        .json();
        let answer = &gone["subsonic-response"]["importAction"];
        assert_eq!(answer["ok"], false, "{action}");
        assert_eq!(answer["message"], format!("Unknown action \"{action}\"."));
    }

    let wrong = get(
        &rig.app,
        "/rest/getImports?u=alice&t=bad&s=salt&v=1.16.1&c=Octo&f=json",
    )
    .await
    .json();
    assert_eq!(wrong["subsonic-response"]["error"]["code"], 40);
}

#[tokio::test]
async fn the_apps_are_told_imports_are_here() {
    let rig = rig().await;
    let doc = get(
        &rig.app,
        "/rest/getOpenSubsonicExtensions.view?f=json&v=1.16.1&c=Octo",
    )
    .await
    .json();
    let names: Vec<&str> = doc["subsonic-response"]["openSubsonicExtensions"]
        .as_array()
        .expect("extensions")
        .iter()
        .map(|e| e["name"].as_str().unwrap_or_default())
        .collect();
    assert!(names.contains(&"octoImports"), "{names:?}");
}

// ---- The binder's recorded answers ------------------------------------------------------------

#[tokio::test]
async fn a_member_of_the_wrong_type_is_refused_with_the_reference_message() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let auth = [("X-Octo-Browse-Token", token.as_str()), ("X-Octo-Admin", "1")];
    let reply = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/lists/x/fetch",
        &auth,
        Some(r#"{"on": "yes"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let errors = &reply.json()["errors"];
    assert_eq!(errors["request"][0], "The request field is required.");
    assert_eq!(
        errors["$.on"][0],
        "The JSON value could not be converted to Octo.Controllers.ImportsController+OnRequest. Path: $.on | LineNumber: 0 | BytePositionInLine: 12."
    );
    let keys: Vec<&str> = errors
        .as_object()
        .expect("errors")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["request", "$.on"]);
}

#[tokio::test]
async fn a_file_sent_as_json_finds_no_action_and_an_empty_one_is_asked_for() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let auth = [("X-Octo-Browse-Token", token.as_str()), ("X-Octo-Admin", "1")];
    let json = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/file",
        &auth,
        Some(r#"{"file":"Gym.csv"}"#),
    )
    .await;
    assert_eq!(json.status, StatusCode::NOT_FOUND);
    let empty = rig.upload(&token, "Empty.csv", "").await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.json()["message"], "Choose a file first.");
}

// ---- octoImports version 2 ------------------------------------------------------------------

impl Rig {
    /// A multipart POST to `uri` with one part named `field`, and text fields after it.
    async fn post_form(
        &self,
        uri: &str,
        field: &str,
        file_name: Option<&str>,
        text: &str,
        extra: &[(&str, &str)],
    ) -> Reply {
        let boundary = "octo-test-boundary";
        let disposition = match file_name {
            Some(name) => format!("form-data; name=\"{field}\"; filename=\"{name}\""),
            None => format!("form-data; name=\"{field}\""),
        };
        let mut body = format!("--{boundary}\r\nContent-Disposition: {disposition}\r\n\r\n{text}\r\n");
        for (name, value) in extra {
            body.push_str(&format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            ));
        }
        body.push_str(&format!("--{boundary}--\r\n"));
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .expect("request");
        let response = self.app.clone().oneshot(request).await.expect("infallible");
        let (parts, body) = response.into_parts();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: body.collect().await.expect("body").to_bytes().to_vec(),
        }
    }
}

#[tokio::test]
async fn the_apps_get_the_export_services() {
    let rig = rig().await;
    let answer = get(&rig.app, &format!("/rest/getImportServices.view?{AUTH}"))
        .await
        .json();
    let envelope = &answer["subsonic-response"];
    assert_eq!(envelope["status"], "ok");
    assert_eq!(envelope["type"], "octo");
    let services = envelope["importServices"]["services"]
        .as_array()
        .expect("services");
    assert_eq!(services.len(), 10);
    assert_eq!(services[0]["id"], "spotify");
    assert_eq!(
        services[4]["exportUrl"],
        "https://www.tunemymusic.com/transfer/tidal-to-file"
    );
    assert_eq!(services[4]["tile"], "TIDAL");
    assert!(
        envelope["importServices"].get("spotifyConnect").is_none(),
        "{envelope}"
    );

    let wrong = get(
        &rig.app,
        "/rest/getImportServices?u=alice&t=bad&s=salt&v=1.16.1&c=Octo&f=json",
    )
    .await
    .json();
    assert_eq!(wrong["subsonic-response"]["error"]["code"], 40);
}

#[tokio::test]
async fn an_app_pastes_a_list_and_reads_it_back() {
    let rig = rig().await;
    let pasted = get(
        &rig.app,
        &format!(
            "/rest/importText?{AUTH}&name=Gym&text=Massive%20Attack%20-%20Teardrop%0ANobody%20-%20Nothing"
        ),
    )
    .await
    .json();
    let answer = &pasted["subsonic-response"]["importAction"];
    assert_eq!(answer["ok"], true, "{pasted}");
    assert_eq!(answer["message"], "Added Gym, 2 songs.");
    let lists = get(&rig.app, &format!("/rest/getImports?{AUTH}")).await.json();
    let list = &lists["subsonic-response"]["imports"]["lists"][0];
    assert_eq!(list["name"], "Gym");
    assert_eq!(list["have"], 1);
    assert!(lists["subsonic-response"]["imports"].get("approval").is_none());

    let unnamed = rig
        .post_form(
            &format!("/rest/importText?{AUTH}"),
            "text",
            None,
            "Massive Attack - Teardrop",
            &[],
        )
        .await
        .json();
    assert_eq!(
        unnamed["subsonic-response"]["importAction"]["message"],
        "Added Pasted list, 1 songs."
    );

    let empty = get(&rig.app, &format!("/rest/importText?{AUTH}&text=%23%20nothing"))
        .await
        .json();
    let answer = &empty["subsonic-response"]["importAction"];
    assert_eq!(answer["ok"], false);
    assert_eq!(
        answer["message"],
        "No songs found. Each line should read Artist - Title."
    );

    let missing = get(&rig.app, &format!("/rest/importText?{AUTH}")).await.json();
    assert_eq!(missing["subsonic-response"]["error"]["code"], 10);
}

#[tokio::test]
async fn an_app_uploads_a_file_as_a_multipart_form() {
    let rig = rig().await;
    let uri = format!("/rest/importFile.view?{AUTH}");
    let csv = rig
        .post_form(
            &uri,
            "file",
            Some("Gym.csv"),
            "Track Name,Artist Name(s)\nTeardrop,Massive Attack\nAngel,Massive Attack\n",
            &[],
        )
        .await;
    assert_eq!(csv.status, StatusCode::OK);
    let reply = csv.json();
    let answer = &reply["subsonic-response"]["importAction"];
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(answer["message"], "Added Gym, 2 songs.");
    assert_eq!(answer["count"], 2);

    // A part with no file name takes the name parameter.
    let txt = rig
        .post_form(
            &uri,
            "file",
            None,
            "Teardrop - Massive Attack\n",
            &[("name", "Mine.txt")],
        )
        .await
        .json();
    assert_eq!(
        txt["subsonic-response"]["importAction"]["message"],
        "Added Mine, 1 songs."
    );

    let unreadable = rig
        .post_form(&uri, "file", Some("x.txt"), "\n\n", &[])
        .await
        .json();
    let answer = &unreadable["subsonic-response"]["importAction"];
    assert_eq!(answer["ok"], false);
    assert!(
        answer["message"]
            .as_str()
            .unwrap_or_default()
            .contains("No songs found. Each line should read Artist - Title."),
        "{answer}"
    );

    let no_file = rig.post_form(&uri, "other", Some("x.csv"), "a", &[]).await.json();
    assert_eq!(
        no_file["subsonic-response"]["importAction"]["message"],
        "Choose a file first."
    );

    let wrong = rig
        .post_form(
            "/rest/importFile?u=alice&t=bad&s=salt&v=1.16.1&c=Octo&f=json",
            "file",
            Some("Other.csv"),
            "Track Name,Artist Name(s)\nTeardrop,Massive Attack\n",
            &[],
        )
        .await
        .json();
    assert_eq!(wrong["subsonic-response"]["error"]["code"], 40);
    // Nothing was added for a refused caller.
    assert_eq!(rig.state.imports.overview("alice").lists.len(), 2);
}

#[tokio::test]
async fn the_dashboard_pastes_a_list_and_lists_the_export_services() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let auth = [("X-Octo-Browse-Token", token.as_str()), ("X-Octo-Admin", "1")];
    let pasted = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/text",
        &auth,
        Some(r#"{"name":"Road trip","text":"Massive Attack - Teardrop\nNobody - Nothing"}"#),
    )
    .await;
    assert_eq!(pasted.status, StatusCode::OK, "{}", pasted.text());
    assert_eq!(pasted.json()["message"], "Added Road trip, 2 songs.");

    let empty = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/text",
        &auth,
        Some(r#"{"text":"  "}"#),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.json()["message"], "Paste a list first.");

    let services = send(&rig.app, Method::GET, "/api/admin/imports/services", &auth, None).await;
    assert_eq!(services.status, StatusCode::OK);
    let services = services.json();
    assert_eq!(services["services"].as_array().expect("services").len(), 10);
    assert!(services.get("spotifyConnect").is_none(), "{services}");

    for (verb, uri) in [
        (Method::GET, "/api/admin/imports/services"),
        (Method::POST, "/api/admin/imports/text"),
    ] {
        let body = (verb == Method::POST).then_some(r#"{"text":"A - B"}"#);
        let reply = send(&rig.app, verb.clone(), uri, &[("X-Octo-Admin", "1")], body).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{verb} {uri}");
    }
}

#[tokio::test]
async fn the_apps_are_told_imports_are_at_versions_one_and_two() {
    let rig = rig().await;
    let doc = get(
        &rig.app,
        "/rest/getOpenSubsonicExtensions.view?f=json&v=1.16.1&c=Octo",
    )
    .await
    .json();
    let imports = doc["subsonic-response"]["openSubsonicExtensions"]
        .as_array()
        .expect("extensions")
        .iter()
        .find(|e| e["name"] == "octoImports")
        .cloned()
        .expect("octoImports");
    assert_eq!(imports["versions"], serde_json::json!([1, 2]));
}

// ---- Importing for someone else (for=<Navidrome username>) ------------------------------------

/// Two songs one a line: the first in the library, the second not.
const TWO_SONGS: &str = r#"{"name":"Gym","text":"Massive Attack - Teardrop\nNobody - Nothing"}"#;

impl Rig {
    fn auth(token: &str) -> [(&'static str, &str); 2] {
        [("X-Octo-Browse-Token", token), ("X-Octo-Admin", "1")]
    }

    /// The names of the lists `user` has.
    fn list_names(&self, user: &str) -> Vec<String> {
        self.state
            .imports
            .overview(user)
            .lists
            .iter()
            .map(|list| list.name.clone())
            .collect()
    }
}

#[tokio::test]
async fn without_for_the_import_is_the_signed_in_persons_own() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let auth = Rig::auth(&token);
    let pasted = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/text",
        &auth,
        Some(TWO_SONGS),
    )
    .await;
    assert_eq!(pasted.status, StatusCode::OK, "{}", pasted.text());
    assert_eq!(rig.list_names("alice"), ["Gym"]);
    assert!(rig.list_names("Bob").is_empty());
    let page = send(&rig.app, Method::GET, "/api/admin/imports", &auth, None)
        .await
        .json();
    assert_eq!(page["user"], "alice");
    assert_eq!(page["overview"]["lists"][0]["name"], "Gym");
    // Naming yourself, in any case, is the same as naming nobody.
    let own = send(&rig.app, Method::GET, "/api/admin/imports?for=ALICE", &auth, None)
        .await
        .json();
    assert_eq!(own["user"], "alice");
    assert_eq!(own["overview"]["lists"][0]["name"], "Gym");
}

#[tokio::test]
async fn for_another_user_makes_the_list_its_playlist_and_its_downloads_theirs() {
    let rig = rig().await;
    let answer = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("POST"))
        .and(path("/api/playlist"))
        .respond_with(answer(serde_json::json!({ "id": "pl-1" })))
        .expect(1)
        .mount(&rig._navidrome)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/playlist/pl-1"))
        .respond_with(answer(
            serde_json::json!({ "id": "pl-1", "name": "Gym", "ownerId": "u-admin" }),
        ))
        .mount(&rig._navidrome)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/playlist/pl-1"))
        .and(body_string_contains(r#""ownerId":"u-bob""#))
        .respond_with(answer(serde_json::json!({ "id": "pl-1" })))
        .expect(1)
        .mount(&rig._navidrome)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/playlist/pl-1/tracks"))
        .respond_with(answer(serde_json::json!([])))
        .mount(&rig._navidrome)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/playlist/pl-1/tracks"))
        .and(body_string_contains("nd-1"))
        .respond_with(answer(serde_json::json!({ "added": 1 })))
        .expect(1)
        .mount(&rig._navidrome)
        .await;

    let token = rig.sign_in("alice");
    let auth = Rig::auth(&token);
    // The name is matched ignoring case and kept the way Navidrome spells it.
    let pasted = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/text?for=bob",
        &auth,
        Some(TWO_SONGS),
    )
    .await;
    assert_eq!(pasted.status, StatusCode::OK, "{}", pasted.text());
    let list_id = pasted.json()["listId"].as_str().expect("an id").to_string();
    assert_eq!(rig.list_names("Bob"), ["Gym"]);
    assert!(rig.list_names("alice").is_empty());

    let playlist = send(
        &rig.app,
        Method::POST,
        &format!("/api/admin/imports/lists/{list_id}/playlist?for=bob"),
        &auth,
        Some(r#"{"on":true}"#),
    )
    .await;
    assert_eq!(playlist.status, StatusCode::OK, "{}", playlist.text());
    let kept = rig.state.imports.overview("Bob");
    assert_eq!(kept.lists[0].playlist_id.as_deref(), Some("pl-1"));
    assert_eq!(kept.lists[0].playlist_note, None, "the playlist is Bob's");

    let fetch = send(
        &rig.app,
        Method::POST,
        &format!("/api/admin/imports/lists/{list_id}/fetch?for=bob"),
        &auth,
        Some(r#"{"on":true}"#),
    )
    .await;
    assert_eq!(fetch.status, StatusCode::OK, "{}", fetch.text());
    assert_eq!(rig.state.imports.overview("Bob").trickle.queued, 1);
    assert_eq!(rig.state.imports.overview("alice").trickle.queued, 0);
    // Bob is no Family member, so his songs go to the shared library, as his own import's do.
    assert_eq!(
        rig.state.heart_acquisition_coordinator.import_target("Bob"),
        Ok(crate::services::common::track_acquisition_queue::AcquisitionTarget::Shared)
    );

    let status = send(&rig.app, Method::GET, "/api/admin/imports?for=bob", &auth, None)
        .await
        .json();
    assert_eq!(status["user"], "Bob");
    assert_eq!(status["overview"]["trickle"]["queued"], 1);
}

#[tokio::test]
async fn a_name_navidrome_does_not_know_is_refused() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let auth = Rig::auth(&token);
    let cases = [
        (Method::GET, "/api/admin/imports?for=nobody", None),
        (Method::GET, "/api/admin/imports/services?for=nobody", None),
        (Method::GET, "/api/admin/imports/lists/x?for=nobody", None),
        (
            Method::POST,
            "/api/admin/imports/text?for=nobody",
            Some(TWO_SONGS),
        ),
        (
            Method::POST,
            "/api/admin/imports/trickle/pause?for=nobody",
            Some(r#"{"paused":true}"#),
        ),
        (Method::DELETE, "/api/admin/imports/lists/x?for=nobody", None),
    ];
    for (verb, uri, body) in cases {
        let reply = send(&rig.app, verb.clone(), uri, &auth, body).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{verb} {uri}: {}",
            reply.text()
        );
        assert_eq!(
            reply.json()["error"],
            "There is no user named nobody on this server.",
            "{verb} {uri}"
        );
    }
    assert!(rig.list_names("alice").is_empty());
    assert!(rig.list_names("nobody").is_empty());
    assert_eq!(rig.state.imports.overview("alice").trickle.state, "idle");
}

#[tokio::test]
async fn a_file_uploaded_for_someone_else_is_theirs() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let boundary = "octo-test-boundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"Gym.csv\"\r\nContent-Type: text/csv\r\n\r\nTrack Name,Artist Name(s)\nTeardrop,Massive Attack\n\r\n--{boundary}--\r\n"
    );
    let send_for = |name: &str| {
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/imports/file?for={name}"))
            .header("X-Octo-Admin", "1")
            .header("X-Octo-Browse-Token", token.as_str())
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body.clone()))
            .expect("request")
    };
    let unknown = rig
        .app
        .clone()
        .oneshot(send_for("nobody"))
        .await
        .expect("infallible");
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    assert!(rig.list_names("alice").is_empty());
    let known = rig
        .app
        .clone()
        .oneshot(send_for("Bob"))
        .await
        .expect("infallible");
    assert_eq!(known.status(), StatusCode::OK);
    assert_eq!(rig.list_names("Bob"), ["Gym"]);
    assert!(rig.list_names("alice").is_empty());
}

#[tokio::test]
async fn the_lists_and_their_songs_follow_for() {
    let rig = rig().await;
    let token = rig.sign_in("alice");
    let auth = Rig::auth(&token);
    let mine = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/text",
        &auth,
        Some(r#"{"name":"Mine","text":"Massive Attack - Teardrop"}"#),
    )
    .await
    .json();
    let theirs = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/text?for=Bob",
        &auth,
        Some(r#"{"name":"Theirs","text":"Massive Attack - Teardrop"}"#),
    )
    .await
    .json();
    let names = |page: &serde_json::Value| -> Vec<String> {
        page["overview"]["lists"]
            .as_array()
            .expect("lists")
            .iter()
            .map(|list| list["name"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let own = send(&rig.app, Method::GET, "/api/admin/imports", &auth, None)
        .await
        .json();
    assert_eq!(names(&own), ["Mine"]);
    let bob = send(&rig.app, Method::GET, "/api/admin/imports?for=bob", &auth, None)
        .await
        .json();
    assert_eq!(names(&bob), ["Theirs"]);

    let theirs_id = theirs["listId"].as_str().expect("an id");
    let mine_id = mine["listId"].as_str().expect("an id");
    let as_alice = send(
        &rig.app,
        Method::GET,
        &format!("/api/admin/imports/lists/{theirs_id}"),
        &auth,
        None,
    )
    .await;
    assert_eq!(as_alice.status, StatusCode::NOT_FOUND);
    let as_bob = send(
        &rig.app,
        Method::GET,
        &format!("/api/admin/imports/lists/{theirs_id}?for=bob"),
        &auth,
        None,
    )
    .await;
    assert_eq!(as_bob.status, StatusCode::OK);
    assert_eq!(as_bob.json()["list"]["name"], "Theirs");
    let mine_as_bob = send(
        &rig.app,
        Method::GET,
        &format!("/api/admin/imports/lists/{mine_id}?for=bob"),
        &auth,
        None,
    )
    .await;
    assert_eq!(mine_as_bob.status, StatusCode::NOT_FOUND);

    // Removing for Bob removes Bob's list and leaves Alice's.
    let removed = send(
        &rig.app,
        Method::DELETE,
        &format!("/api/admin/imports/lists/{theirs_id}?for=bob"),
        &auth,
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.text());
    assert!(rig.list_names("Bob").is_empty());
    assert_eq!(rig.list_names("alice"), ["Mine"]);
}

/// A Family member who adds songs at once.
fn family_member(name: &str) -> crate::services::family::Member {
    crate::services::family::Member {
        username: name.into(),
        display_name: name.into(),
        preset: octo_core::family::abilities::Preset::Member,
        edits: serde_json::Map::new(),
        navidrome_user_id: format!("u-{name}"),
        navidrome_password: String::new(),
        personal_library_id: Some(7),
        shared_libraries: crate::services::family::SharedLibraries::All,
        account_password: String::new(),
        created: chrono::Utc::now(),
        suspended: false,
        ntfy_topic: String::new(),
    }
}

#[tokio::test]
async fn a_family_members_import_goes_to_their_own_library() {
    let rig = rig_with(true).await;
    rig.state.family_store.upsert_member(family_member("alex"));
    let token = rig.sign_in("admin");
    let auth = Rig::auth(&token);
    let pasted = send(
        &rig.app,
        Method::POST,
        "/api/admin/imports/text?for=Alex",
        &auth,
        Some(TWO_SONGS),
    )
    .await;
    assert_eq!(pasted.status, StatusCode::OK, "{}", pasted.text());
    let list_id = pasted.json()["listId"].as_str().expect("an id").to_string();
    assert_eq!(rig.list_names("alex"), ["Gym"]);
    let fetch = send(
        &rig.app,
        Method::POST,
        &format!("/api/admin/imports/lists/{list_id}/fetch?for=Alex"),
        &auth,
        Some(r#"{"on":true}"#),
    )
    .await;
    assert_eq!(fetch.status, StatusCode::OK, "{}", fetch.text());
    assert_eq!(rig.state.imports.overview("alex").trickle.queued, 1);
    assert_eq!(rig.state.imports.overview("admin").trickle.queued, 0);
    // The trickle fetches each job into its owner's import target: Alex's own library.
    assert_eq!(
        rig.state.heart_acquisition_coordinator.import_target("alex"),
        Ok(crate::services::common::track_acquisition_queue::AcquisitionTarget::Member("alex".into()))
    );
}

#[tokio::test]
async fn the_page_lists_who_it_can_import_for() {
    let rig = rig_with(true).await;
    rig.state.family_store.upsert_member(family_member("alex"));
    let token = rig.sign_in("alice");
    let auth = Rig::auth(&token);
    let people = send(&rig.app, Method::GET, "/api/admin/imports/people", &auth, None).await;
    assert_eq!(people.status, StatusCode::OK, "{}", people.text());
    let people = people.json();
    assert_eq!(people["signedIn"], "alice");
    assert_eq!(people["problem"], serde_json::Value::Null);
    let rows: Vec<(String, bool, bool)> = people["users"]
        .as_array()
        .expect("users")
        .iter()
        .map(|user| {
            (
                user["username"].as_str().unwrap_or_default().to_string(),
                user["member"].as_bool().unwrap_or_default(),
                user["you"].as_bool().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("alice".to_string(), false, true),
            ("admin".to_string(), false, false),
            ("alex".to_string(), true, false),
            ("Bob".to_string(), false, false),
        ]
    );
    assert_eq!(people["users"][0]["name"], "Alice");

    let signed_out = send(
        &rig.app,
        Method::GET,
        "/api/admin/imports/people",
        &[("X-Octo-Admin", "1")],
        None,
    )
    .await;
    assert_eq!(signed_out.status, StatusCode::UNAUTHORIZED);
}

/// While an import's songs are checked with Octo's own AcoustID key (the person has no key of
/// their own), the overview carries the import card's line: a free key of their own checks them
/// faster, and where to get one. With a key of their own it carries none.
#[tokio::test]
async fn the_import_card_says_a_key_of_ones_own_checks_faster_while_octos_own_checks_the_songs() {
    use std::sync::Arc;

    use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};

    let server = navidrome().await;
    let mut state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices::default(),
    );
    let settings = state.settings.clone();
    Arc::get_mut(&mut state.inner)
        .expect("nothing else holds a fresh test state")
        .acoust_id = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::new(
            Arc::new(AcoustIdRateLimiter::new()),
        )),
        settings,
        Some("octo-key"),
    ));
    let app = app(state.clone());
    let token = state.browse_sessions.create("alice");
    let auth = [("X-Octo-Browse-Token", token.as_str()), ("X-Octo-Admin", "1")];

    let page = send(&app, Method::GET, "/api/admin/imports", &auth, None)
        .await
        .json();
    assert_eq!(
        page["overview"]["acoustIdKey"],
        serde_json::json!({ "text": built_in_key_pace(), "link": ACOUST_ID_NEW_KEY })
    );
    assert!(built_in_key_pace().contains("one every 20 seconds"));

    let mut own = (*state.settings.current()).clone();
    own.soulseek.acoust_id_api_key = "user-key".into();
    state.settings.set(own);
    let page = send(&app, Method::GET, "/api/admin/imports", &auth, None)
        .await
        .json();
    assert!(page["overview"].get("acoustIdKey").is_none(), "{page}");
}
