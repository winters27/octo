//! Tests of the Lidarr client. A mock server answers every request with 200 and a body chosen by
//! method and path, and keeps the requests; the calls the heart's rules make are tested against
//! the fake Lidarr, request by request.

use std::path::Path;
use std::sync::Arc;

use octo_core::lidarr::LidarrAlbumCandidate;
use octo_core::settings::{AppSettings, LidarrSettings, SettingsStore};
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::lidarr::fake_lidarr::{
    ALBUM_ID, ARTIST_ID, CatalogAlbum, CommandMode, FakeLidarr, Grab, GrabFile, LIDARR_DOWNLOADS,
    LIDARR_RECYCLE_BIN, LIDARR_ROOT, RETAGGED,
};
use crate::services::lidarr::lidarr_heart_acquisition_service::translate_imported_path;

/// Answers each request with `respond(method, path)`, always 200.
struct Handler(Box<dyn Fn(&str, &str) -> String + Send + Sync>);

impl Respond for Handler {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string((self.0)(request.method.as_str(), request.url.path()))
    }
}

async fn serve(respond: impl Fn(&str, &str) -> String + Send + Sync + 'static) -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(Handler(Box::new(respond)))
        .mount(&server)
        .await;
    server
}

fn build(server: &MockServer) -> LidarrClient {
    let settings = LidarrSettings {
        base_url: Some(server.uri()),
        api_key: Some("secret".into()),
        root_folder_path: Some("/data/music".into()),
        quality_profile_id: 3,
        metadata_profile_id: 4,
        ..Default::default()
    };
    LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings {
        lidarr: settings,
        ..Default::default()
    })))
}

fn candidate(foreign_id: &str, title: &str, artist: &str, year: Option<i32>) -> LidarrAlbumCandidate {
    let resource = json!({
        "foreignAlbumId": foreign_id,
        "title": title,
        "releaseDate": year.map(|y| format!("{y}-01-01T00:00:00Z")),
        "artist": { "artistName": artist, "foreignArtistId": "artist-mbid" },
    });
    LidarrAlbumCandidate {
        id: 0,
        foreign_album_id: foreign_id.into(),
        title: title.into(),
        artist: artist.into(),
        year,
        resource: resource.as_object().cloned().expect("an object"),
    }
}

fn path_and_query(request: &Request) -> String {
    match request.url.query() {
        Some(query) => format!("{}?{query}", request.url.path()),
        None => request.url.path().to_string(),
    }
}

fn api_key(request: &Request) -> Option<String> {
    request
        .headers
        .get("X-Api-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).expect("a JSON body")
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.expect("recording is on")
}

#[tokio::test]
async fn options_use_api_key_and_map_server_choices() {
    let server = serve(|_, path| {
        match path {
            "/api/v1/rootfolder" => r#"[{"id":1,"path":"/data/music"}]"#,
            "/api/v1/qualityprofile" => r#"[{"id":2,"name":"Lossless"}]"#,
            "/api/v1/metadataprofile" => r#"[{"id":3,"name":"Standard"}]"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    let result = build(&server).get_options().await.expect("options");

    assert_eq!(result.root_folders.len(), 1);
    assert_eq!(result.root_folders[0].path, "/data/music");
    assert_eq!(result.quality_profiles.len(), 1);
    assert_eq!(result.quality_profiles[0].name, "Lossless");
    assert_eq!(result.metadata_profiles.len(), 1);
    assert_eq!(result.metadata_profiles[0].name, "Standard");
    for request in requests(&server).await {
        assert_eq!(api_key(&request).as_deref(), Some("secret"));
    }
}

#[tokio::test]
async fn connection_test_uses_entered_url_and_api_key_without_saving() {
    let server = serve(|_, path| {
        if path == "/api/v1/system/status" {
            "{}"
        } else {
            "[]"
        }
        .to_string()
    })
    .await;
    // The saved settings point elsewhere: the entered address is the one asked.
    let client = LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings::default())));

    client
        .test_connection(&format!("{}/", server.uri()), "entered-key")
        .await
        .expect("the server answers");

    let requests = requests(&server).await;
    // The mock server sees the path and the Host header; together they are the URL asked
    // (the trailing slash entered is trimmed, so no "//").
    let host = server.address().to_string();
    assert!(
        requests
            .iter()
            .any(|r| path_and_query(r) == "/api/v1/system/status"
                && r.headers.get("host").and_then(|h| h.to_str().ok()) == Some(host.as_str()))
    );
    for request in &requests {
        assert_eq!(api_key(request).as_deref(), Some("entered-key"));
    }
    for path in [
        "/api/v1/rootfolder",
        "/api/v1/qualityprofile",
        "/api/v1/metadataprofile",
    ] {
        assert!(requests.iter().any(|r| path_and_query(r) == path), "{path}");
    }
}

// AlbumSelectionRequiresExactIdentityAndUsesYearToDisambiguate is in
// octo_core::lidarr::lidarr_client's tests, with select_best_album.

#[tokio::test]
async fn album_tracks_join_track_files_returned_by_separate_endpoint() {
    let server = serve(|_, path| {
        match path {
            "/api/v1/track" => {
                r#"[{"id":1,"title":"Song","trackNumber":"1","duration":123000,"hasFile":true,"trackFileId":8}]"#
            }
            "/api/v1/trackFile" => r#"[{"id":8,"path":"/data/music/Artist/Album/01.flac","size":456}]"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    let tracks = build(&server).get_album_tracks(42).await.expect("tracks");

    assert_eq!(tracks.len(), 1);
    let track = &tracks[0];
    assert!(track.has_file);
    assert_eq!(track.path.as_deref(), Some("/data/music/Artist/Album/01.flac"));
    assert_eq!(track.size_bytes, 456);
    assert_eq!(track.duration_seconds, Some(123));
    assert_eq!(track.track_file_id, 8);
    let requests = requests(&server).await;
    assert!(
        requests
            .iter()
            .any(|r| path_and_query(r) == "/api/v1/track?albumId=42")
    );
    assert!(
        requests
            .iter()
            .any(|r| path_and_query(r) == "/api/v1/trackFile?albumId=42")
    );
}

#[tokio::test]
async fn import_completion_uses_album_statistics_not_alternate_release_rows() {
    let server = serve(|_, path| {
        match path {
            "/api/v1/album/42" => r#"{"id":42,"statistics":{"trackCount":1,"trackFileCount":1}}"#,
            "/api/v1/track" => {
                r#"[{"id":1,"title":"Song","hasFile":true,"trackFileId":8},{"id":2,"title":"Alternate release bonus","hasFile":false,"trackFileId":0}]"#
            }
            "/api/v1/trackFile" => r#"[{"id":8,"path":"/data/music/Artist/Album/01.flac","size":456}]"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    let state = build(&server).get_album_import_state(42).await.expect("a state");

    assert!(state.is_complete());
    assert_eq!(state.track_count, 1);
    assert_eq!(state.tracks.len(), 2);
}

#[test]
fn imported_paths_translate_relative_to_selected_root_and_reject_escapes() {
    let translated = translate_imported_path(
        "/data/music/Radiohead/In Rainbows/01.flac",
        Some("/data/music"),
        "/music",
    );
    assert_eq!(translated.as_deref(), Ok("/music/Radiohead/In Rainbows/01.flac"));
    assert!(matches!(
        translate_imported_path("/downloads/other.flac", Some("/data/music"), "/music"),
        Err(LidarrError::InvalidOperation(_))
    ));
}

// What the client fails with, by kind.

#[tokio::test]
async fn an_error_status_is_an_http_error_with_the_start_of_the_body() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(401).set_body_string("x".repeat(400)))
        .mount(&server)
        .await;

    let error = build(&server).get_options().await.expect_err("401");
    assert_eq!(
        error,
        LidarrError::Http(format!("Lidarr returned HTTP 401: {}", "x".repeat(300)))
    );
    assert!(!build(&server).is_reachable().await);
}

/// A client of the Lidarr at `base_url`.
fn build_at(base_url: String) -> LidarrClient {
    LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings {
        lidarr: LidarrSettings {
            base_url: Some(base_url),
            api_key: Some("secret".into()),
            ..Default::default()
        },
        ..Default::default()
    })))
}

/// A request that went out with no answer of Lidarr's back: a connection dropped once the request
/// was read, or a proxy's 502, 503 or 504, may have been taken (Lidarr may have queued the
/// search); a connection never made, and Lidarr's own refusal, were not.
#[tokio::test]
async fn a_request_lidarr_may_have_taken_is_told_apart_from_one_it_never_did() {
    // Reads the whole request, then drops the connection without a word.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let dropping = format!("http://{}", listener.local_addr().expect("its address"));
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while let Ok(read) = socket.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            drop(socket);
        }
    });
    let dropped = build_at(dropping).album_search(7).await.expect_err("dropped");
    assert!(matches!(dropped, LidarrError::MaybeSent(_)), "{dropped:?}");

    let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let nobody = format!("http://{}", closed.local_addr().expect("its address"));
    drop(closed);
    let refused = build_at(nobody).album_search(7).await.expect_err("refused");
    assert!(matches!(refused, LidarrError::Http(_)), "{refused:?}");

    for (status, maybe) in [(502, true), (503, true), (504, true), (400, false), (500, false)] {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(status).set_body_string("no"))
            .mount(&server)
            .await;
        let error = build(&server).album_search(7).await.expect_err("failed");
        assert_eq!(
            matches!(error, LidarrError::MaybeSent(_)),
            maybe,
            "{status}: {error:?}"
        );
    }
}

#[tokio::test]
async fn nothing_is_asked_until_lidarr_is_set_up() {
    let client = LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings::default())));
    assert_eq!(
        client.get_options().await,
        Err(LidarrError::InvalidOperation(
            "Lidarr URL and API key are required.".into()
        ))
    );
    assert!(!client.is_reachable().await);
    assert!(matches!(
        client.test_connection(" ", "key").await,
        Err(LidarrError::InvalidOperation(_))
    ));
}

#[tokio::test]
async fn adding_an_album_needs_the_root_folder_and_both_profiles() {
    let server = serve(|_, _| "[]".to_string()).await;
    let client = LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings {
        lidarr: LidarrSettings {
            base_url: Some(server.uri()),
            api_key: Some("k".into()),
            ..Default::default()
        },
        ..Default::default()
    })));
    assert_eq!(
        client
            .add_album(&candidate("mbid", "Album", "Artist", None), None)
            .await
            .map(|added| added.album_id),
        Err(LidarrError::InvalidOperation(
            "Choose a Lidarr root folder, quality profile, and metadata profile.".into()
        ))
    );
    assert!(client.is_reachable().await);
}

#[tokio::test]
async fn a_lookup_with_no_single_match_is_an_invalid_operation() {
    let server = serve(|_, path| {
        if path == "/api/v1/album/lookup" {
            r#"[{"foreignAlbumId":"a","title":"In Rainbows","releaseDate":"2007-10-10","artist":{"artistName":"Radiohead"}},
                {"foreignAlbumId":"","title":"In Rainbows","artist":{"artistName":"Radiohead"}}]"#
        } else {
            "[]"
        }
        .to_string()
    })
    .await;
    let client = build(&server);

    let found = client
        .resolve_album("Radiohead", "In Rainbows", None)
        .await
        .expect("one album");
    assert_eq!(found.foreign_album_id, "a");
    assert_eq!(found.year, Some(2007));
    assert_eq!(
        client.resolve_album("Radiohead", "Kid A", None).await,
        Err(LidarrError::InvalidOperation(
            "Lidarr could not unambiguously match 'Radiohead - Kid A'.".into()
        ))
    );
    assert_eq!(
        client
            .resolve_album_by_foreign_id("A")
            .await
            .map(|c| c.map(|c| c.title)),
        Ok(Some("In Rainbows".to_string()))
    );
    let lookups: Vec<String> = requests(&server).await.iter().map(path_and_query).collect();
    assert!(lookups.contains(&"/api/v1/album/lookup?term=Radiohead%20In%20Rainbows".to_string()));
    assert!(lookups.contains(&"/api/v1/album/lookup?term=lidarr%3AA".to_string()));
}

// ---- Releases: the Lidarr half of Find songs picks ------------------------------------------

#[tokio::test]
async fn releases_are_listed_with_their_quality_and_why_lidarr_rejects_one() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::path("/api/v1/release"))
        .and(wiremock::matchers::query_param("albumId", "12"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"guid":"a1","indexerId":3,"indexer":"Redacted","title":"Artist - Album (2020) [FLAC]","size":312000000,
                 "quality":{"quality":{"name":"FLAC"}},"seeders":14,"leechers":1,"protocol":"torrent","ageHours":49.0,
                 "rejected":false,"rejections":[]},
                {"guid":"b2","indexerId":4,"indexer":"NZBgeek","title":"Artist - Album MP3 V0","size":90000000.0,
                 "quality":{"quality":{"name":"MP3-VBR-V0"}},"protocol":"usenet","age":12,
                 "rejected":true,"rejections":["Quality is not wanted in profile"]},
                {"guid":"","indexerId":5,"title":"no guid"}]"#,
        ))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    let releases = build(&server).list_releases(12).await.expect("releases");

    assert_eq!(releases.len(), 2);
    let flac = &releases[0];
    assert_eq!(
        (
            flac.guid.as_str(),
            flac.indexer_id,
            flac.indexer.as_deref(),
            flac.quality.as_deref(),
            flac.size,
            flac.seeders,
            flac.age_days,
            flac.rejected
        ),
        (
            "a1",
            3,
            Some("Redacted"),
            Some("FLAC"),
            Some(312000000),
            Some(14),
            Some(2),
            false
        )
    );
    let mp3 = &releases[1];
    assert!(mp3.rejected);
    assert_eq!(mp3.size, Some(90000000));
    assert_eq!(mp3.age_days, Some(12));
    assert_eq!(
        mp3.rejections,
        vec!["Quality is not wanted in profile".to_string()]
    );
}

#[tokio::test]
async fn a_picked_release_is_grabbed_with_no_album_search() {
    let server = serve(|method, path| {
        match (method, path) {
            ("POST", "/api/v1/release") => r#"{"guid":"a1"}"#,
            ("POST", "/api/v1/command") => r#"{"id":9}"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;
    let pick = LidarrReleasePick {
        guid: "a1".into(),
        indexer_id: 3,
        title: Some("Artist - Album (2020) [FLAC]".into()),
        size: None,
    };

    let guid = build(&server).grab_picked(12, &pick).await.expect("grabbed");

    assert_eq!(guid, "a1");
    let requests = requests(&server).await;
    assert!(!requests.iter().any(|r| r.url.path() == "/api/v1/command"));
    let grabs: Vec<&Request> = requests
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/api/v1/release")
        .collect();
    assert_eq!(grabs.len(), 1);
    let grab = body(grabs[0]);
    assert_eq!(grab["guid"], "a1");
    assert_eq!(grab["indexerId"], 3);
}

/// A forgotten guid is found again by indexer, title (any case, trimmed) and size, and a size
/// that differs is another release.
#[tokio::test]
async fn a_forgotten_pick_is_grabbed_again_under_its_new_guid() {
    let server = MockServer::builder().start().await;
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/api/v1/release"))
        .and(wiremock::matchers::body_partial_json(json!({ "guid": "stale" })))
        .respond_with(ResponseTemplate::new(404).set_body_string(
            r#"{"message":"Couldn't find requested release in cache, cache timeout probably expired."}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/api/v1/release"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/api/v1/release"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"guid":"bigger","indexerId":3,"title":"Air - Moon Safari [FLAC]","size":999},
                {"guid":"fresh","indexerId":3,"title":" air - moon safari [flac] ","size":300}]"#,
        ))
        .mount(&server)
        .await;
    let client = build(&server);
    let pick = |size| LidarrReleasePick {
        guid: "stale".into(),
        indexer_id: 3,
        title: Some("Air - Moon Safari [FLAC]".into()),
        size,
    };

    // The guid the grab went out under is the new one.
    assert_eq!(
        client.grab_picked(7, &pick(Some(300))).await,
        Ok("fresh".to_string())
    );

    let grabbed: Vec<Value> = requests(&server)
        .await
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(body)
        .collect();
    assert_eq!(grabbed.len(), 2);
    assert_eq!(grabbed[1]["guid"], "fresh");
    // Without a size of its own the pick takes the first of that title.
    client.grab_picked(7, &pick(None)).await.expect("found again");
    let last = requests(&server)
        .await
        .into_iter()
        .rev()
        .find(|r| r.method.as_str() == "POST");
    assert_eq!(body(&last.expect("a grab"))["guid"], "bigger");
    // A pick with no title cannot be found again.
    let untitled = LidarrReleasePick {
        title: None,
        ..pick(Some(300))
    };
    assert_eq!(
        client.grab_picked(7, &untitled).await,
        Err(LidarrError::InvalidOperation(PICK_GONE_TEXT.into()))
    );
}

// ---- Waiting for an album's track list before AlbumSearch -----------------------------------

/// A Lidarr whose album 12 (artist 7) has no tracks until a refresh finishes. Each look at the
/// command list moves a running refresh one step on; `refresh_steps` looks finish a refresh
/// that was already running (`usize::MAX`: never), one look a RefreshAlbum asked for.
#[derive(Clone)]
struct RefreshingLidarr {
    state: Arc<parking_lot::Mutex<RefreshState>>,
}

struct RefreshState {
    has_tracks: bool,
    /// The refresh Lidarr is running: (name, body, looks left until it finishes).
    running: Option<(String, Value, usize)>,
    /// Whether a finished refresh brings the tracks.
    refresh_brings_tracks: bool,
    /// What happened, in order: "tracks", "commands", "RefreshAlbum", "AlbumSearch".
    log: Vec<String>,
}

impl Respond for RefreshingLidarr {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut state = self.state.lock();
        let ok = |body: Value| ResponseTemplate::new(200).set_body_string(body.to_string());
        match (request.method.as_str(), request.url.path()) {
            ("GET", "/api/v1/album") => ok(json!([
                { "id": 12, "foreignAlbumId": "mbid", "monitored": true, "artistId": 7,
                  "artist": { "id": 7, "monitored": true } }
            ])),
            ("GET", "/api/v1/track") => {
                state.log.push("tracks".into());
                ok(if state.has_tracks {
                    json!([{ "id": 1, "title": "Song" }])
                } else {
                    json!([])
                })
            }
            ("GET", "/api/v1/command") => {
                state.log.push("commands".into());
                let mut listed = Vec::new();
                if let Some((name, body, left)) = state.running.take() {
                    if left == 0 {
                        if state.refresh_brings_tracks {
                            state.has_tracks = true;
                        }
                    } else {
                        listed.push(json!({ "id": 3, "name": name, "status": "started", "body": body }));
                        let left = if left == usize::MAX { left } else { left - 1 };
                        state.running = Some((name, body, left));
                    }
                }
                listed.push(json!({ "id": 2, "name": "RssSync", "status": "started", "body": {} }));
                ok(Value::Array(listed))
            }
            ("POST", "/api/v1/command") => {
                let body: Value = serde_json::from_slice(&request.body).expect("a JSON body");
                let name = body["name"].as_str().unwrap_or_default().to_string();
                state.log.push(name.clone());
                if name == "RefreshAlbum" {
                    state.running = Some((name, body, 1));
                }
                ok(json!({ "id": 9 }))
            }
            _ => ok(json!([])),
        }
    }
}

async fn refreshing_lidarr(
    has_tracks: bool,
    running: Option<(&str, Value)>,
    refresh_steps: usize,
    refresh_brings_tracks: bool,
) -> (RefreshingLidarr, MockServer) {
    let fake = RefreshingLidarr {
        state: Arc::new(parking_lot::Mutex::new(RefreshState {
            has_tracks,
            running: running.map(|(name, body)| (name.to_string(), body, refresh_steps)),
            refresh_brings_tracks,
            log: Vec::new(),
        })),
    };
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(fake.clone()).mount(&server).await;
    (fake, server)
}

fn build_waiting(server: &MockServer, import_timeout_seconds: i32) -> LidarrClient {
    let settings = LidarrSettings {
        base_url: Some(server.uri()),
        api_key: Some("secret".into()),
        root_folder_path: Some("/data/music".into()),
        quality_profile_id: 3,
        metadata_profile_id: 4,
        import_timeout_seconds,
        ..Default::default()
    };
    let client = LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings {
        lidarr: settings,
        ..Default::default()
    })));
    client.set_metadata_poll(std::time::Duration::from_millis(5));
    client
}

fn log_of(fake: &RefreshingLidarr) -> Vec<String> {
    fake.state.lock().log.clone()
}

#[tokio::test]
async fn an_album_that_has_its_tracks_needs_no_wait() {
    let (fake, server) = refreshing_lidarr(true, None, 0, false).await;

    assert!(
        build_waiting(&server, 60)
            .wait_for_tracks(12, 7)
            .await
            .expect("asked")
    );

    assert_eq!(log_of(&fake), ["tracks"]);
}

#[tokio::test]
async fn the_wait_for_tracks_waits_for_the_artist_refresh_already_running() {
    let (fake, server) = refreshing_lidarr(
        false,
        Some(("RefreshArtist", json!({ "artistIds": [7] }))),
        2,
        true,
    )
    .await;

    assert!(
        build_waiting(&server, 60)
            .wait_for_tracks(12, 7)
            .await
            .expect("asked")
    );

    let log = log_of(&fake);
    assert_eq!(log.last().map(String::as_str), Some("tracks"));
    assert!(!log.iter().any(|step| step == "RefreshAlbum"), "{log:?}");
    assert!(fake.state.lock().has_tracks);
}

#[tokio::test]
async fn an_album_with_no_tracks_and_no_refresh_gets_one_refresh_album() {
    let (fake, server) = refreshing_lidarr(false, None, 1, true).await;

    assert!(
        build_waiting(&server, 60)
            .wait_for_tracks(12, 7)
            .await
            .expect("asked")
    );

    let requests = requests(&server).await;
    let refreshes: Vec<Value> = requests
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/api/v1/command")
        .map(body)
        .filter(|b| b["name"] == "RefreshAlbum")
        .collect();
    assert_eq!(refreshes.len(), 1);
    assert_eq!(refreshes[0]["albumId"], 12);
    assert!(fake.state.lock().has_tracks);
}

#[tokio::test]
async fn a_refresh_that_brings_no_tracks_is_asked_once_and_says_so() {
    let (fake, server) = refreshing_lidarr(false, None, 1, false).await;

    assert!(
        !build_waiting(&server, 60)
            .wait_for_tracks(12, 7)
            .await
            .expect("asked")
    );

    let log = log_of(&fake);
    assert_eq!(
        log.iter().filter(|step| *step == "RefreshAlbum").count(),
        1,
        "{log:?}"
    );
}

#[tokio::test]
async fn a_refresh_that_never_ends_is_waited_for_only_until_the_import_timeout() {
    let (fake, server) = refreshing_lidarr(
        false,
        Some(("RefreshAlbum", json!({ "albumId": 12 }))),
        usize::MAX,
        false,
    )
    .await;

    let started = std::time::Instant::now();
    assert!(
        !build_waiting(&server, 1)
            .wait_for_tracks(12, 7)
            .await
            .expect("asked")
    );

    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let log = log_of(&fake);
    assert!(!log.iter().any(|step| step == "RefreshAlbum"), "{log:?}");
}

#[test]
fn the_wait_for_a_track_list_stops_at_five_minutes_whatever_the_import_timeout() {
    let wait = |import_timeout_seconds: i32| {
        track_list_wait(&LidarrSettings {
            import_timeout_seconds,
            ..Default::default()
        })
    };
    assert_eq!(wait(3600), std::time::Duration::from_secs(5 * 60));
    assert_eq!(wait(120), std::time::Duration::from_secs(120));
    assert_eq!(wait(0), std::time::Duration::from_secs(1));
}

// ---- The calls the heart's rules make, against the fake Lidarr ------------------------------

/// The fake Lidarr over a fresh octo root.
async fn fake_lidarr() -> (tempfile::TempDir, FakeLidarr, MockServer) {
    let root = tempfile::tempdir().expect("a temp dir");
    let (fake, server) = FakeLidarr::start(root.path()).await;
    (root, fake, server)
}

/// Each request the fake answered, as `METHOD path?query`.
fn asked(fake: &FakeLidarr) -> Vec<String> {
    fake.requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

#[tokio::test]
async fn what_lidarr_holds_is_read_with_these_requests() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    fake.import(1, "Angel", "FLAC");
    fake.set_stale_statistics(5, 0);
    fake.catalog_artist("ot", "Other Artist", 6);
    let elsewhere = tempfile::tempdir().expect("a temp dir");
    fake.add_root_folder("/data/other", elsewhere.path());
    let client = build(&server);

    let artists = client.artists().await.expect("artists");
    assert_eq!(artists.len(), 1);
    assert_eq!(
        (artists[0].id, artists[0].path.as_deref(), artists[0].monitored),
        (5, Some("/data/music/Massive Attack"), true)
    );
    // The statistic is stale; the track file list says what the artist has.
    assert_eq!(artists[0].track_file_count, Some(0));
    assert_eq!(
        client.find_artist("MA").await.expect("read").map(|a| a.id),
        Some(5)
    );
    assert_eq!(
        client.artist(5).await.expect("read").map(|a| a.name),
        Some("Massive Attack".to_string())
    );
    assert_eq!(client.artist(99).await, Ok(None));
    let album = client.album(7).await.expect("read").expect("held");
    assert_eq!(
        (album.monitored, album.any_release_ok, album.artist_id),
        (false, true, 5)
    );
    assert_eq!(album.release_choice().releases.len(), 1);
    assert!(
        album
            .release("mezzanine-uk")
            .is_some_and(|r| r.id == 70 && r.monitored)
    );
    assert_eq!(client.album(99).await, Ok(None));
    assert_eq!(
        client
            .album_by_foreign_id(ALBUM_ID)
            .await
            .expect("read")
            .map(|a| a.id),
        Some(7)
    );
    assert_eq!(client.album_by_foreign_id("missing").await, Ok(None));
    assert_eq!(client.artist_albums(5).await.expect("read").len(), 1);
    let files = client.artist_track_files(5).await.expect("read");
    assert_eq!(
        files.iter().map(|f| (f.id, f.album_id)).collect::<Vec<_>>(),
        [(100, 7)]
    );
    assert_eq!(client.album_track_files(7).await.expect("read").len(), 1);
    assert_eq!(
        client
            .root_folders()
            .await
            .expect("read")
            .iter()
            .map(|r| r.path.clone())
            .collect::<Vec<_>>(),
        [LIDARR_ROOT, "/data/other"]
    );
    assert_eq!(
        client.artist_folder(ARTIST_ID).await,
        Ok(Some("Massive Attack".to_string()))
    );
    assert_eq!(
        client.artist_folder("ot").await,
        Ok(Some("Other Artist".to_string()))
    );
    assert_eq!(client.artist_folder("nobody").await, Ok(None));

    assert_eq!(
        asked(&fake),
        [
            "GET /api/v1/artist",
            "GET /api/v1/artist",
            "GET /api/v1/artist/5",
            "GET /api/v1/artist/99",
            "GET /api/v1/album/7",
            "GET /api/v1/album/99",
            "GET /api/v1/album?foreignAlbumId=rg-mezzanine",
            "GET /api/v1/album?foreignAlbumId=missing",
            "GET /api/v1/album?artistId=5",
            "GET /api/v1/trackfile?artistId=5",
            "GET /api/v1/trackfile?albumId=7",
            "GET /api/v1/rootfolder",
            "GET /api/v1/artist/lookup?term=lidarr%3Ama",
            "GET /api/v1/artist/lookup?term=lidarr%3Aot",
            "GET /api/v1/artist/lookup?term=lidarr%3Anobody",
        ]
    );
}

#[tokio::test]
async fn lidarrs_settings_status_and_health_are_read_with_these_requests() {
    let (_root, fake, server) = fake_lidarr().await;
    let client = build(&server);

    assert_eq!(client.completed_download_handling().await, Ok(true));
    assert_eq!(client.writes_audio_tags().await, Ok(false));
    assert_eq!(client.recycle_bin().await, Ok(None));
    let started = client.start_time().await.expect("read").expect("a start time");
    assert!(started < Utc::now());
    assert_eq!(client.health().await, Ok(Vec::new()));

    fake.set_completed_download_handling(false);
    fake.set_write_audio_tags("newFiles");
    fake.set_recycle_bin(true);
    fake.set_health(json!([{
        "source": "IndexerStatusCheck", "type": "error",
        "message": "Indexers unavailable due to failures: Fake indexer",
        "wikiUrl": "https://wiki.servarr.com/lidarr/system#indexers-are-unavailable-due-to-failures",
    }]));
    assert_eq!(client.completed_download_handling().await, Ok(false));
    assert_eq!(client.writes_audio_tags().await, Ok(true));
    assert_eq!(
        client.recycle_bin().await,
        Ok(Some(LIDARR_RECYCLE_BIN.to_string()))
    );
    let health = client.health().await.expect("read");
    assert_eq!(
        (health[0].source.as_str(), health[0].kind.as_str()),
        ("IndexerStatusCheck", "error")
    );

    assert_eq!(
        asked(&fake),
        [
            "GET /api/v1/config/downloadclient",
            "GET /api/v1/config/metadataprovider",
            "GET /api/v1/config/mediamanagement",
            "GET /api/v1/system/status",
            "GET /api/v1/health",
            "GET /api/v1/config/downloadclient",
            "GET /api/v1/config/metadataprovider",
            "GET /api/v1/config/mediamanagement",
            "GET /api/v1/health",
        ]
    );
}

#[tokio::test]
async fn an_album_is_added_unmonitored_under_a_new_artist_that_monitors_nothing() {
    let (_root, fake, server) = fake_lidarr().await;
    let client = build(&server);
    let candidate = client
        .resolve_album_by_foreign_id(ALBUM_ID)
        .await
        .expect("looked up")
        .expect("known");

    let added = client.add_album(&candidate, None).await.expect("added");

    assert_eq!(
        added,
        LidarrAdded {
            album_id: 7,
            artist_id: 5
        }
    );
    // The add alone: no search, and nothing else changed.
    let changes = fake.changes();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(
        (changes[0].method.as_str(), changes[0].path.as_str()),
        ("POST", "/api/v1/album")
    );
    let body = changes[0].body.clone().expect("a body");
    assert_eq!(body["monitored"], false);
    assert_eq!(body["addOptions"], json!({ "searchForNewAlbum": false }));
    assert!(body.get("id").is_none());
    let artist = &body["artist"];
    assert!(artist.get("id").is_none());
    assert_eq!(artist["monitored"], false);
    assert_eq!(artist["monitorNewItems"], "none");
    assert_eq!(
        artist["addOptions"],
        json!({ "monitor": "none", "searchForMissingAlbums": false })
    );
    assert_eq!(artist["rootFolderPath"], "/data/music");
    assert_eq!(
        (
            artist["qualityProfileId"].clone(),
            artist["metadataProfileId"].clone()
        ),
        (json!(3), json!(4))
    );
    // Lidarr refreshed the new artist, then rescanned its folder.
    let commands = client.commands().await.expect("listed");
    assert_eq!(
        commands
            .iter()
            .map(|c| (c.name.as_str(), c.status.as_str()))
            .collect::<Vec<_>>(),
        [("RefreshArtist", "completed"), ("RescanFolders", "completed")]
    );
    assert_eq!(commands[1].body["folders"], json!(["/data/music/Massive Attack"]));
    assert!(commands[1].rescans_artist(5, Some("/data/music/Massive Attack")));
    assert!(!client.album(7).await.expect("read").expect("held").monitored);
    let artist = client.artist(5).await.expect("read").expect("held");
    assert_eq!(
        (artist.monitored, artist.monitor_new_items.as_deref()),
        (false, Some("none"))
    );
    assert_eq!(client.wait_for_tracks(7, 5).await, Ok(true));
}

#[tokio::test]
async fn an_album_is_added_under_an_artist_lidarr_has_which_stays_as_it_was() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_artist(ARTIST_ID, false);
    let client = build(&server);
    let candidate = client
        .resolve_album_by_foreign_id(ALBUM_ID)
        .await
        .expect("looked up")
        .expect("known");
    let artist = client.find_artist(ARTIST_ID).await.expect("read").expect("held");

    let added = client.add_album(&candidate, Some(&artist)).await.expect("added");

    assert_eq!(added.album_id, 7);
    let changes = fake.changes();
    assert_eq!(changes.len(), 1, "{changes:?}");
    let body = changes[0].body.clone().expect("a body");
    assert_eq!(body["artistId"], 5);
    assert_eq!(body["artist"], Value::Object(artist.resource.clone()));
    assert_eq!(body["monitored"], false);
    assert!(!client.artist(5).await.expect("read").expect("held").monitored);
    let commands = client.commands().await.expect("listed");
    assert_eq!(
        commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["RefreshAlbum", "RescanFolders"]
    );
    assert_eq!(commands[0].refreshes(7, 5), Ok(true));
}

#[tokio::test]
async fn the_adds_rescan_links_files_lying_in_the_artist_folder_with_no_history_row() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.set_write_audio_tags("allFiles");
    let lying = fake.place_file(
        "/data/music/Massive Attack/Mezzanine/03 - Teardrop.flac",
        b"the user's own",
    );
    // Lidarr's scans skip dot folders, so a file Octo holds there is never linked.
    let held = fake.place_file(
        "/data/music/.octo-incoming/held/1/01 - Angel.flac",
        b"held by Octo",
    );
    let client = build(&server);
    let candidate = client
        .resolve_album_by_foreign_id(ALBUM_ID)
        .await
        .expect("looked up")
        .expect("known");

    client.add_album(&candidate, None).await.expect("added");

    let files = client.artist_track_files(5).await.expect("read");
    assert_eq!(
        files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        [Some(
            "/data/music/Massive Attack/Mezzanine/03 - Teardrop.flac".to_string()
        )]
    );
    assert_eq!(client.artist_history(5, None).await, Ok(Vec::new()));
    // Lidarr wrote its own tags into the file it linked, and left the held one alone.
    assert_eq!(
        std::fs::read(&lying).expect("still there"),
        [b"the user's own".as_slice(), RETAGGED].concat()
    );
    assert_eq!(std::fs::read(&held).expect("still there"), b"held by Octo");
    // Each track carries the recording Lidarr matched it to.
    let tracks = client.get_album_tracks(7).await.expect("read");
    assert_eq!(
        tracks
            .iter()
            .map(|t| (t.title.as_str(), t.has_file, t.foreign_recording_id.as_deref()))
            .collect::<Vec<_>>(),
        [("Angel", false, Some("rec-1")), ("Teardrop", true, Some("rec-3"))]
    );
}

#[tokio::test]
async fn waiting_for_a_track_list_that_never_comes_says_so() {
    let (_root, fake, server) = fake_lidarr().await;
    let mut empty = CatalogAlbum::new("rg-empty", "Empty", ARTIST_ID, 8, &[(20, "Gone", 1)]);
    empty.tracks_appear = false;
    empty.in_profile = false;
    fake.catalog_album(empty);
    fake.user_artist(ARTIST_ID, true);
    let client = build_waiting(&server, 1);
    let candidate = client
        .resolve_album_by_foreign_id("rg-empty")
        .await
        .expect("looked up")
        .expect("known");
    let artist = client.find_artist(ARTIST_ID).await.expect("read").expect("held");
    client.add_album(&candidate, Some(&artist)).await.expect("added");

    let started = std::time::Instant::now();
    assert_eq!(client.wait_for_tracks(8, 5).await, Ok(false));

    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    // One RefreshAlbum of its own, which brought nothing.
    let refreshes = fake
        .changes()
        .into_iter()
        .filter(|r| r.body.as_ref().is_some_and(|b| b["name"] == "RefreshAlbum"))
        .count();
    assert_eq!(refreshes, 1);
}

#[tokio::test]
async fn a_release_pin_is_sent_and_put_back_exactly_and_lidarr_rescans_after_each() {
    let (_root, fake, server) = fake_lidarr().await;
    let mut two = CatalogAlbum::new("rg-two", "Two Releases", ARTIST_ID, 9, &[(30, "One", 1)]);
    two.releases = vec![(90, "two-us".into()), (91, "two-uk".into())];
    fake.catalog_album(two);
    fake.user_album("rg-two", false);
    // A file the pin unlinks and its rescan links again.
    fake.import(30, "One", "FLAC");
    let client = build(&server);
    let album = client.album(9).await.expect("read").expect("held");
    let saved = album.release_choice();
    let uk = album.release("TWO-UK").expect("listed").id;
    let pin = album.pinned_to(uk).expect("a pin");

    let pinned = client.set_release_choice(9, &pin).await.expect("pinned");

    assert_eq!(pinned.release_choice(), pin);
    let put = fake.changes().last().cloned().expect("the change");
    assert_eq!(
        (put.method.as_str(), put.path.as_str()),
        ("PUT", "/api/v1/album/9")
    );
    let body = put.body.expect("a body");
    assert_eq!(body["anyReleaseOk"], false);
    assert_eq!(body["monitored"], false);
    assert_eq!(
        body["releases"]
            .as_array()
            .expect("the releases")
            .iter()
            .map(|r| (r["id"].clone(), r["monitored"].clone()))
            .collect::<Vec<_>>(),
        [(json!(90), json!(false)), (json!(91), json!(true))]
    );
    let rescans = |commands: &[LidarrCommand]| {
        commands
            .iter()
            .filter(|c| c.rescans_artist(5, Some("/data/music/Massive Attack")))
            .count()
    };
    assert_eq!(rescans(&client.commands().await.expect("listed")), 1);
    assert_eq!(client.album_track_files(9).await.expect("read").len(), 1);

    let restored = client.set_release_choice(9, &saved).await.expect("restored");

    assert_eq!(restored.release_choice(), saved);
    assert_eq!(rescans(&client.commands().await.expect("listed")), 2);
    // The album's own monitored flag never moved.
    assert!(fake.monitor_changes().is_empty());
}

#[tokio::test]
async fn an_album_search_sends_only_the_search_and_its_grab_is_read_from_history() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    fake.plan_grab(
        7,
        Grab::of("DL-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let client = build(&server);
    assert_eq!(
        client.album_history_count(7, LidarrHistoryEvent::Grabbed).await,
        Ok(0)
    );

    let command = client.album_search(7).await.expect("searched");

    assert_eq!(command.name, "AlbumSearch");
    let changes = fake.changes();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(
        changes[0].body,
        Some(json!({ "name": "AlbumSearch", "albumIds": [7] }))
    );
    let read = client.command(command.id).await.expect("read").expect("listed");
    let (started, ended) = (read.started.expect("started"), read.ended.expect("ended"));
    assert_eq!(
        client.album_history_count(7, LidarrHistoryEvent::Grabbed).await,
        Ok(1)
    );
    let grabs = client
        .album_history(7, LidarrHistoryEvent::Grabbed, Some(started))
        .await
        .expect("read");
    assert_eq!(grabs.len(), 1);
    assert!(grabs[0].date >= started && grabs[0].date <= ended);
    assert_eq!(
        (
            grabs[0].download_id.as_deref(),
            grabs[0].release_source(),
            grabs[0].download_client()
        ),
        (Some("DL-1"), Some("UserInvokedSearch"), Some("QBittorrent"))
    );
    assert_eq!(
        client
            .album_history(
                7,
                LidarrHistoryEvent::Grabbed,
                Some(ended + chrono::TimeDelta::seconds(1))
            )
            .await,
        Ok(Vec::new())
    );
    assert!(!client.album(7).await.expect("read").expect("held").monitored);
    let asked = asked(&fake);
    for path in [
        "GET /api/v1/history?albumId=7&eventType=1&pageSize=1",
        "POST /api/v1/command",
        "GET /api/v1/command/1",
        "GET /api/v1/history?albumId=7&eventType=1&page=1&pageSize=100&sortKey=date&sortDirection=descending",
    ] {
        assert!(asked.iter().any(|a| a == path), "{path} in {asked:?}");
    }
}

#[tokio::test]
async fn commands_are_listed_and_read_by_id_as_they_move() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.set_command_mode(CommandMode::Held);
    let client = build(&server);

    let health = client.check_health().await.expect("asked");

    assert_eq!(
        (health.name.as_str(), health.status.as_str()),
        ("CheckHealth", "queued")
    );
    assert_eq!(fake.changes()[0].body, Some(json!({ "name": "CheckHealth" })));
    fake.queue_command("RescanFolders", json!({ "folders": null }), "scheduled");
    let scan = fake.queue_command("DownloadedAlbumsScan", json!({}), "manual");
    fake.step();
    // One disk command at a time: the scan waits for the rescan.
    let listed = client.commands().await.expect("listed");
    assert_eq!(
        listed.iter().map(|c| c.status.as_str()).collect::<Vec<_>>(),
        ["started", "started", "queued"]
    );
    // A RescanFolders with no folders covers every root folder.
    assert!(listed[1].rescans_artist(5, None));
    fake.run_commands();
    let ended = client.command(health.id).await.expect("read").expect("kept");
    assert_eq!(ended.status, "completed");
    assert!(ended.started.is_some() && ended.ended > ended.started);
    fake.forget_command(scan);
    assert_eq!(client.command(scan).await, Ok(None));
    assert_eq!(
        asked(&fake),
        [
            "POST /api/v1/command",
            "GET /api/v1/command",
            "GET /api/v1/command/1",
            "GET /api/v1/command/3",
        ]
    );
}

#[tokio::test]
async fn the_queue_is_read_by_album_and_artist_and_octos_items_are_removed() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    fake.plan_grab(
        7,
        Grab::of("DL-1", &[(1, "Angel", "FLAC")]).by_client("Sabnzbd", "SAB Music", "usenet", false),
    );
    let client = build(&server);
    client.album_search(7).await.expect("searched");
    let pending = fake.pending_release(7, "delay");

    let queue = client.album_queue(7).await.expect("read");

    assert_eq!(queue.len(), 2);
    assert_eq!(
        (
            queue[0].download_id.as_deref(),
            queue[0].download_client.as_deref(),
            queue[0].tracked_download_state.as_deref()
        ),
        (Some("DL-1"), Some("SAB Music"), Some("downloading"))
    );
    assert_eq!(
        (
            queue[1].id,
            queue[1].download_id.clone(),
            queue[1].status.as_deref()
        ),
        (pending, None, Some("delay"))
    );
    assert_eq!(client.artist_queue(5).await.expect("read").len(), 2);
    assert_eq!(client.artist_queue(6).await, Ok(Vec::new()));
    assert_eq!(
        client.remove_queue_item(queue[0].id, false).await,
        Ok(LidarrDeletion::Deleted)
    );
    assert_eq!(
        client.remove_queue_item(queue[0].id, false).await,
        Ok(LidarrDeletion::NotFound)
    );
    fake.drop_pending(pending);
    assert_eq!(client.album_queue(7).await, Ok(Vec::new()));
    // A plain removal writes no row that ends the download.
    assert!(
        client
            .download_history("DL-1")
            .await
            .expect("read")
            .iter()
            .all(|r| !r.event.ends_download())
    );
    let asked = asked(&fake);
    for path in [
        "GET /api/v1/queue/details?albumIds=7",
        "GET /api/v1/queue/details?artistId=5",
        "GET /api/v1/queue/details?artistId=6",
        "DELETE /api/v1/queue/1?removeFromClient=true&blocklist=false&skipRedownload=true",
    ] {
        assert!(asked.iter().any(|a| a == path), "{path} in {asked:?}");
    }
}

#[tokio::test]
async fn a_finished_download_is_imported_and_followed_through_its_history() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    fake.plan_grab(
        7,
        Grab::of("DL-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let client = build(&server);
    client.album_search(7).await.expect("searched");
    fake.complete("DL-1");
    // Until Lidarr's next refresh its queue shows the download as it was.
    let state_now = |queue: Vec<LidarrQueueItem>| queue[0].tracked_download_state.clone();
    assert_eq!(
        state_now(client.album_queue(7).await.expect("read")).as_deref(),
        Some("downloading")
    );

    fake.refresh();

    use LidarrHistoryEvent::{DownloadImported, Grabbed, TrackFileImported};
    let rows = client.download_history("DL-1").await.expect("read");
    // Newest first: the download's own row, one per file, then the grab.
    assert_eq!(
        rows.iter().map(|r| r.event).collect::<Vec<_>>(),
        [DownloadImported, TrackFileImported, TrackFileImported, Grabbed]
    );
    let files = client.album_track_files(7).await.expect("read");
    assert_eq!(files.len(), 2);
    for row in &rows[1..3] {
        assert!(files.iter().any(|f| Some(f.id) == row.file_id()));
        assert_eq!(row.download_client(), Some("QBittorrent"));
        // A seeding torrent keeps its own copy at the dropped path.
        let dropped = row.dropped_path().expect("a dropped path");
        assert!(dropped.starts_with(LIDARR_DOWNLOADS));
        assert!(fake.local(dropped).is_file());
    }
    assert_eq!(
        client
            .artist_history(5, Some(TrackFileImported))
            .await
            .expect("read")
            .len(),
        2
    );
    assert_eq!(client.artist_history(5, None).await.expect("read").len(), 4);
    // The queue still says the import is pending until the next refresh drops the item.
    assert_eq!(
        state_now(client.album_queue(7).await.expect("read")).as_deref(),
        Some("importPending")
    );
    fake.refresh();
    assert_eq!(client.album_queue(7).await, Ok(Vec::new()));
    let asked = asked(&fake);
    for path in [
        "GET /api/v1/history?downloadId=DL-1&page=1&pageSize=100&sortKey=date&sortDirection=descending",
        "GET /api/v1/history/artist?artistId=5&eventType=3",
        "GET /api/v1/history/artist?artistId=5",
    ] {
        assert!(asked.iter().any(|a| a == path), "{path} in {asked:?}");
    }
}

#[tokio::test]
async fn history_is_read_page_by_page_and_back_to_a_time() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    for n in 0..120 {
        fake.user_grab(7, Grab::of(&format!("DL-{n}"), &[]), "Rss");
    }
    let client = build(&server);

    let rows = client
        .album_history(7, LidarrHistoryEvent::Grabbed, None)
        .await
        .expect("read");

    assert_eq!(rows.len(), 120);
    assert!(rows.windows(2).all(|pair| pair[0].date >= pair[1].date));
    let since = rows[9].date;
    assert_eq!(
        client
            .album_history(7, LidarrHistoryEvent::Grabbed, Some(since))
            .await
            .expect("read")
            .len(),
        10
    );
    let pages: Vec<String> = asked(&fake)
        .into_iter()
        .filter(|a| a.starts_with("GET /api/v1/history?"))
        .collect();
    assert_eq!(
        pages,
        [
            "GET /api/v1/history?albumId=7&eventType=1&page=1&pageSize=100&sortKey=date&sortDirection=descending",
            "GET /api/v1/history?albumId=7&eventType=1&page=2&pageSize=100&sortKey=date&sortDirection=descending",
            "GET /api/v1/history?albumId=7&eventType=1&page=1&pageSize=100&sortKey=date&sortDirection=descending",
        ]
    );
}

#[tokio::test]
async fn with_completed_download_handling_off_octo_imports_its_own_download_by_hand() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.set_completed_download_handling(false);
    fake.user_album(ALBUM_ID, false);
    let mut grab = Grab::of("DL-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    grab.files[1].rejection = Some("Not an upgrade for existing track file(s)".into());
    fake.plan_grab(7, grab);
    let client = build(&server);
    client.album_search(7).await.expect("searched");
    fake.complete("DL-1");
    fake.refresh();
    // Lidarr no longer tracks a finished download, and never imports it itself.
    assert_eq!(client.album_queue(7).await, Ok(Vec::new()));
    assert_eq!(client.album_track_files(7).await, Ok(Vec::new()));

    let offered = client.manual_import_candidates("DL-1").await.expect("read");
    assert_eq!(offered.len(), 2);
    assert_eq!(
        offered[1].rejections,
        ["Not an upgrade for existing track file(s)"]
    );
    let take: Vec<LidarrManualImportItem> = offered
        .into_iter()
        .filter(|item| item.rejections.is_empty() && item.album_id == 7)
        .collect();
    let command = client.manual_import(&take).await.expect("asked");

    assert_eq!(
        fake.changes().last().and_then(|r| r.body.clone()),
        Some(json!({
            "name": "ManualImport",
            "files": [{
                "path": "/data/downloads/DL-1/01 - Angel.flac", "artistId": 5, "albumId": 7,
                "albumReleaseId": 70, "trackIds": [1], "quality": { "quality": { "name": "FLAC" } },
                "indexerFlags": 0, "downloadId": "DL-1", "disableReleaseSwitching": false,
            }],
            "importMode": "auto",
            "replaceExistingFiles": false,
        }))
    );
    assert_eq!(
        client.command(command.id).await.expect("read").map(|c| c.status),
        Some("completed".to_string())
    );
    let rows = client.download_history("DL-1").await.expect("read");
    // Imported as that tracked download, so its row names the client; only part of the
    // download came in, so no row ends it.
    assert_eq!(
        rows.iter().map(|r| r.event).collect::<Vec<_>>(),
        [LidarrHistoryEvent::TrackFileImported, LidarrHistoryEvent::Grabbed]
    );
    assert_eq!(rows[0].download_client(), Some("QBittorrent"));
    assert!(
        asked(&fake)
            .iter()
            .any(|a| a == "GET /api/v1/manualimport?downloadId=DL-1&replaceExistingFiles=false")
    );
}

#[tokio::test]
async fn deleting_a_moved_track_file_removes_only_its_record() {
    let (root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    let angel = fake.import(1, "Angel", "FLAC");
    let teardrop = fake.import(3, "Teardrop", "FLAC");
    // Octo moved Angel into its hold folder.
    let held = root.path().join(".octo-incoming/held/1/01 - Angel.flac");
    std::fs::create_dir_all(held.parent().expect("a parent")).expect("the hold folder");
    std::fs::rename(&angel, &held).expect("moved");
    let client = build(&server);

    assert_eq!(
        client.delete_track_file_record(100).await,
        Ok(LidarrDeletion::Deleted)
    );
    assert!(held.is_file());
    assert_eq!(
        client.delete_track_file_record(100).await,
        Ok(LidarrDeletion::NotFound)
    );
    // With a recycle bin, a file still at its path is moved there, not removed.
    fake.set_recycle_bin(true);
    assert_eq!(
        client.delete_track_file_record(101).await,
        Ok(LidarrDeletion::Deleted)
    );
    assert!(!Path::new(&teardrop).exists());
    assert!(
        fake.local(LIDARR_RECYCLE_BIN)
            .join("03 - Teardrop.flac")
            .is_file()
    );
    let deleted = client
        .artist_history(5, Some(LidarrHistoryEvent::TrackFileDeleted))
        .await
        .expect("read");
    assert_eq!(
        deleted.iter().map(|r| r.reason()).collect::<Vec<_>>(),
        [Some("Manual"), Some("Manual")]
    );
    // With the artist's root folder empty, Lidarr keeps the record.
    fake.import(1, "Angel", "FLAC");
    std::fs::remove_dir_all(root.path().join("Massive Attack")).expect("removed");
    std::fs::remove_dir_all(root.path().join(".octo-incoming")).expect("removed");
    assert_eq!(
        client.delete_track_file_record(102).await,
        Ok(LidarrDeletion::Conflict)
    );
    assert_eq!(client.album_track_files(7).await.expect("read").len(), 1);
    assert!(asked(&fake).iter().any(|a| a == "DELETE /api/v1/trackfile/100"));
}

#[tokio::test]
async fn deleting_an_album_or_its_artist_leaves_the_disk_alone() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    let angel = fake.import(1, "Angel", "FLAC");
    let client = build(&server);

    assert_eq!(client.delete_album(7).await, Ok(LidarrDeletion::Deleted));
    assert!(Path::new(&angel).is_file());
    assert_eq!(client.album(7).await, Ok(None));
    assert_eq!(client.delete_album(7).await, Ok(LidarrDeletion::NotFound));
    assert_eq!(client.delete_artist(5).await, Ok(LidarrDeletion::Deleted));
    assert!(Path::new(&angel).is_file());
    assert_eq!(client.artists().await, Ok(Vec::new()));
    let deletes: Vec<String> = asked(&fake)
        .into_iter()
        .filter(|a| a.starts_with("DELETE"))
        .collect();
    assert_eq!(
        deletes,
        [
            "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false",
            "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false",
            "DELETE /api/v1/artist/5?deleteFiles=false&addImportListExclusion=false",
        ]
    );

    // The fake deletes the artist's folder when asked to, so a test of a wrong request fails.
    fake.user_album(ALBUM_ID, false);
    let teardrop = fake.import(3, "Teardrop", "FLAC");
    reqwest::Client::new()
        .delete(format!("{}/api/v1/artist/5?deleteFiles=true", server.uri()))
        .send()
        .await
        .expect("answered");
    assert!(!Path::new(&teardrop).exists());
}

#[tokio::test]
async fn an_import_replaces_every_file_the_album_had_and_writes_its_rows_after_its_files() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    // The user's own Angel: an import onto the album replaces it.
    let users = fake.import(1, "Angel", "MP3-320");
    fake.plan_grab(7, Grab::of("DL-1", &[(3, "Teardrop", "FLAC")]));
    let client = build(&server);
    client.album_search(7).await.expect("searched");
    fake.complete("DL-1");
    fake.set_rows_one_per_step(true);
    fake.set_command_mode(CommandMode::PerPoll);
    fake.refresh();
    let active = async || -> Vec<String> {
        client
            .commands()
            .await
            .expect("listed")
            .into_iter()
            .filter(LidarrCommand::is_active)
            .map(|c| c.name)
            .collect()
    };

    // Each read of the commands moves Lidarr one step: the refresh runs, then the import
    // starts, which moves its file in and the user's out before writing any row.
    assert_eq!(active().await, ["RefreshMonitoredDownloads"]);
    assert_eq!(active().await, ["ProcessMonitoredDownloads"]);
    assert_eq!(
        client
            .album_track_files(7)
            .await
            .expect("read")
            .iter()
            .map(|f| f.id)
            .collect::<Vec<_>>(),
        [101]
    );
    assert!(!Path::new(&users).exists());
    let events = async || -> Vec<LidarrHistoryEvent> {
        client
            .download_history("DL-1")
            .await
            .expect("read")
            .iter()
            .map(|r| r.event)
            .collect()
    };
    assert_eq!(events().await, [LidarrHistoryEvent::Grabbed]);
    assert_eq!(active().await, ["ProcessMonitoredDownloads"]);
    assert_eq!(
        events().await,
        [LidarrHistoryEvent::TrackFileImported, LidarrHistoryEvent::Grabbed]
    );
    assert_eq!(active().await, ["ProcessMonitoredDownloads"]);
    assert_eq!(events().await[0], LidarrHistoryEvent::DownloadImported);
    assert!(active().await.is_empty());
    let replaced = client
        .artist_history(5, Some(LidarrHistoryEvent::TrackFileDeleted))
        .await
        .expect("read");
    assert_eq!(
        replaced
            .iter()
            .map(|r| (r.reason(), r.source_title.as_str()))
            .collect::<Vec<_>>(),
        [(
            Some("Upgrade"),
            "/data/music/Massive Attack/Mezzanine/01 - Angel.mp3"
        )]
    );
}

#[tokio::test]
async fn the_queue_is_lidarrs_last_refresh_and_empty_after_a_restart() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    fake.plan_grab(7, Grab::of("DL-1", &[(1, "Angel", "FLAC")]));
    let client = build(&server);
    let search = client.album_search(7).await.expect("searched");
    let first_start = client.start_time().await.expect("read");
    let left = |queue: Vec<LidarrQueueItem>| queue.first().and_then(|item| item.size_left_bytes);

    fake.progress("DL-1", 1000);
    assert_eq!(
        left(client.album_queue(7).await.expect("read")),
        Some(300_000_000)
    );
    fake.refresh();
    assert_eq!(left(client.album_queue(7).await.expect("read")), Some(1000));

    fake.restart();

    assert_eq!(client.album_queue(7).await, Ok(Vec::new()));
    assert!(client.start_time().await.expect("read") > first_start);
    // The command list starts again; an older command is still read by its id.
    assert_eq!(client.commands().await, Ok(Vec::new()));
    assert_eq!(
        client.command(search.id).await.expect("read").map(|c| c.status),
        Some("completed".to_string())
    );
    fake.refresh();
    assert_eq!(client.album_queue(7).await.expect("read").len(), 1);
}

#[tokio::test]
async fn a_client_that_fails_a_refresh_drops_all_its_items_from_it() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    fake.user_grab(7, Grab::of("DL-1", &[]), "Rss");
    fake.user_grab(
        7,
        Grab::of("DL-2", &[]).by_client("Transmission", "Transmission", "torrent", true),
        "Rss",
    );
    let client = build(&server);
    let listed = async || -> Vec<Option<String>> {
        client
            .artist_queue(5)
            .await
            .expect("read")
            .into_iter()
            .map(|item| item.download_id)
            .collect()
    };

    fake.client_fails_next_refresh("qBittorrent");
    fake.refresh();
    assert_eq!(listed().await, [Some("DL-2".to_string())]);

    fake.refresh();
    assert_eq!(
        listed().await,
        [Some("DL-1".to_string()), Some("DL-2".to_string())]
    );
}

#[tokio::test]
async fn downloads_show_their_clients_warnings_failures_and_removals() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    fake.plan_grab(7, Grab::of("DL-1", &[(1, "Angel", "FLAC")]));
    fake.plan_grab(7, Grab::of("DL-2", &[(1, "Angel", "FLAC")]));
    let client = build(&server);
    client.album_search(7).await.expect("searched");
    let item = |queue: &[LidarrQueueItem], id: &str| {
        queue
            .iter()
            .find(|item| item.download_id.as_deref() == Some(id))
            .cloned()
    };

    fake.stall("DL-1");
    fake.refresh();
    let queue = client.album_queue(7).await.expect("read");
    assert_eq!(
        item(&queue, "DL-1").and_then(|i| i.tracked_download_status),
        Some("warning".to_string())
    );

    // A failed grab of a search: its row, and Lidarr's own new search, which grabs again.
    fake.fail("DL-1");
    let grabs = client
        .album_history(7, LidarrHistoryEvent::Grabbed, None)
        .await
        .expect("read");
    assert_eq!(
        grabs
            .iter()
            .map(|r| (r.download_id.as_deref(), r.release_source()))
            .collect::<Vec<_>>(),
        [
            (Some("DL-2"), Some("Search")),
            (Some("DL-1"), Some("UserInvokedSearch"))
        ]
    );
    assert!(
        client
            .download_history("DL-1")
            .await
            .expect("read")
            .iter()
            .any(|r| r.event == LidarrHistoryEvent::DownloadFailed)
    );
    assert!(client.commands().await.expect("listed").iter().any(|c| {
        c.name == "AlbumSearch" && c.trigger.as_deref() == Some("unspecified") && c.album_ids() == [7]
    }));

    // The user's own grab, finished but blocked from importing.
    fake.user_grab(
        7,
        Grab::of("DL-3", &[(3, "Teardrop", "FLAC")]),
        "InteractiveSearch",
    );
    fake.complete("DL-3");
    fake.block_import("DL-3", "importBlocked");
    fake.refresh();
    let queue = client.album_queue(7).await.expect("read");
    let blocked = item(&queue, "DL-3").expect("listed");
    assert_eq!(blocked.tracked_download_state.as_deref(), Some("importBlocked"));
    assert_eq!(client.album_track_files(7).await, Ok(Vec::new()));

    // Removed in the client: no row, and gone from the next refresh.
    fake.remove_in_client("DL-2");
    fake.refresh();
    assert!(item(&client.album_queue(7).await.expect("read"), "DL-2").is_none());
    assert!(
        client
            .download_history("DL-2")
            .await
            .expect("read")
            .iter()
            .all(|r| r.event == LidarrHistoryEvent::Grabbed)
    );

    // Removing with the blocklist marks the download failed.
    assert_eq!(
        client.remove_queue_item(blocked.id, true).await,
        Ok(LidarrDeletion::Deleted)
    );
    assert!(
        client
            .download_history("DL-3")
            .await
            .expect("read")
            .iter()
            .any(|r| r.event == LidarrHistoryEvent::DownloadFailed)
    );
    let removal = format!(
        "DELETE /api/v1/queue/{}?removeFromClient=true&blocklist=true&skipRedownload=true",
        blocked.id
    );
    assert!(asked(&fake).contains(&removal));
}

#[tokio::test]
async fn a_folder_import_names_no_client_even_with_a_guessed_download_id() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    let teardrop = GrabFile {
        album_id: 7,
        track_id: 3,
        title: "Teardrop".into(),
        quality: "FLAC".into(),
        rejection: None,
    };

    fake.user_folder_import(&[teardrop], Some("DL-OCTO"));

    let client = build(&server);
    let rows = client.download_history("DL-OCTO").await.expect("read");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].event,
            rows[0].download_client(),
            rows[0].file_id().is_some()
        ),
        (LidarrHistoryEvent::TrackFileImported, None, true)
    );
    let commands = client.commands().await.expect("listed");
    assert_eq!(
        commands
            .iter()
            .map(|c| (c.name.as_str(), c.trigger.as_deref()))
            .collect::<Vec<_>>(),
        [("ManualImport", Some("manual"))]
    );
}

#[tokio::test]
async fn a_picked_release_is_grabbed_within_the_request_and_its_row_carries_its_guid() {
    let (_root, fake, server) = fake_lidarr().await;
    fake.user_album(ALBUM_ID, false);
    fake.offer("picked", 3, "Massive Attack - Mezzanine [FLAC]", 400_000_000);
    fake.plan_grab(7, Grab::of("DL-9", &[(3, "Teardrop", "FLAC")]));
    let client = build(&server);
    let pick = LidarrReleasePick {
        guid: "picked".into(),
        indexer_id: 3,
        title: None,
        size: None,
    };

    assert_eq!(client.grab_picked(7, &pick).await, Ok("picked".to_string()));

    assert_eq!(fake.grabs(), ["picked"]);
    let rows = client
        .album_history(7, LidarrHistoryEvent::Grabbed, None)
        .await
        .expect("read");
    assert_eq!(
        (
            rows[0].guid(),
            rows[0].release_source(),
            rows[0].download_id.as_deref()
        ),
        (Some("picked"), Some("InteractiveSearch"), Some("DL-9"))
    );
    // No command: the grab is the request itself.
    assert!(!fake.changes().iter().any(|r| r.path == "/api/v1/command"));
}

#[tokio::test]
async fn lidarrs_import_can_file_a_grab_under_another_album_and_monitor_its_release() {
    let (_root, fake, server) = fake_lidarr().await;
    let mut other = CatalogAlbum::new("rg-other", "Other", ARTIST_ID, 9, &[(40, "Angel", 1)]);
    other.releases = vec![(90, "other-us".into()), (91, "other-uk".into())];
    fake.catalog_album(other);
    fake.user_album(ALBUM_ID, false);
    fake.user_album("rg-other", false);
    let mut grab = Grab::of("DL-1", &[(40, "Angel", "FLAC")]).into_album(9);
    grab.release_id = Some(91);
    fake.plan_grab(7, grab);
    let client = build(&server);
    client.album_search(7).await.expect("searched");
    fake.complete("DL-1");

    fake.refresh();

    let imported: Vec<i32> = client
        .download_history("DL-1")
        .await
        .expect("read")
        .iter()
        .filter(|r| r.event == LidarrHistoryEvent::TrackFileImported)
        .map(|r| r.album_id)
        .collect();
    assert_eq!(imported, [9]);
    assert_eq!(client.album_track_files(7).await, Ok(Vec::new()));
    assert_eq!(client.album_track_files(9).await.expect("read").len(), 1);
    // The import's SetMonitored: only the release it matched is monitored now.
    let choice = client
        .album(9)
        .await
        .expect("read")
        .expect("held")
        .release_choice();
    assert_eq!(
        choice
            .releases
            .iter()
            .map(|r| (r.id, r.monitored))
            .collect::<Vec<_>>(),
        [(90, false), (91, true)]
    );
}
