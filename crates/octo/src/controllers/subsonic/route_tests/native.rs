//! The catch-all's Navidrome-native answers: native artists and album searches, the external-id
//! safety net, the native song answers and the native searches.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{AppSettings, LastFmSettings, SubsonicSettings};
use octo_core::soulseek::{RoutingKind, SoulseekRouting, StreamFacts};
use parking_lot::Mutex;
use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::{Reply, TestMetadata, app, get, query_value, send};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;

/// The fake servers: Navidrome, and Deezer under `/deezer`.
#[derive(Clone, Default)]
struct FakeServers {
    deezer_calls: Arc<Mutex<Vec<String>>>,
    /// Each catalog answer waits this long.
    hold_catalog: Arc<Mutex<Option<Duration>>>,
}

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "application/json; charset=utf-8")
}

impl Respond for FakeServers {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let q = query_value(request, "q").unwrap_or_default();
        if let Some(path) = path.strip_prefix("/deezer") {
            self.deezer_calls.lock().push(path.to_string());
            let answer = if path.starts_with("/search/album") && q.contains("Look Back") {
                json(
                    r#"{"data":[{"id":20,"title":"Don’t Look Back","record_type":"album","nb_tracks":10,"artist":{"name":"Test Artist"}},{"id":21,"title":"Look Back Again","record_type":"album","nb_tracks":10,"artist":{"name":"Test Artist"}}]}"#,
                )
            } else if path.starts_with("/search/album") {
                json(
                    r#"{"data":[{"id":1,"title":"Test Album","record_type":"album","nb_tracks":4,"artist":{"name":"Test Artist"}}]}"#,
                )
            } else if path == "/album/1/tracks" {
                json(
                    r#"{"total":4,"data":[{"title":"One","duration":100,"track_position":1,"disk_number":1,"isrc":"GBAAA0000001","artist":{"name":"Test Artist"}},{"title":"Two","duration":200,"track_position":2,"disk_number":1,"isrc":"GBAAA0000002","artist":{"name":"Test Artist"}},{"title":"Three","duration":300,"track_position":3,"disk_number":1,"isrc":"GBAAA0000003","artist":{"name":"Test Artist"}},{"title":"Four","duration":400,"track_position":4,"disk_number":1,"isrc":"GBAAA0000004","artist":{"name":"Test Artist"}}]}"#,
                )
            } else if path == "/album/2" {
                json(
                    r#"{"id":2,"title":"Other Album","nb_tracks":9,"release_date":"2005-05-05","artist":{"name":"Test Artist"}}"#,
                )
            } else if path == "/album/1" {
                json(
                    r#"{"id":1,"title":"Test Album","release_date":"2001-01-01","artist":{"name":"Test Artist"}}"#,
                )
            } else if path.starts_with("/search/artist") {
                // A bigger act whose name contains this one comes first, and a better known
                // artist of the very same name before the one the library holds.
                json(
                    r#"{"data":[{"id":8,"name":"Test Artist Orchestra","nb_fan":90000,"picture_xl":"https://cdn/orchestra.jpg"},{"id":9,"name":"Test Artist","nb_fan":5000,"picture_xl":"https://cdn/somebody-else.jpg"},{"id":7,"name":"Test Artist","nb_fan":10,"picture_xl":"https://cdn/test-artist.jpg"}]}"#,
                )
            } else if path.starts_with("/artist/9/albums") {
                json(
                    r#"{"data":[{"id":90,"title":"Somebody Else's Record","record_type":"album","release_date":"2010-01-01"}]}"#,
                )
            } else if path.starts_with("/artist/7/albums") {
                // The catalog's own shape: no artist and no track counts on this listing. An EP
                // shares the album's title, and would open as the album (or the album as it).
                json(
                    r#"{"data":[{"id":1,"title":"Test Album","record_type":"album","release_date":"2001-01-01"},{"id":2,"title":"Other Album","record_type":"album","release_date":"2005-05-05"},{"id":3,"title":"A Single","record_type":"single","release_date":"2006-01-01"},{"id":4,"title":"Other Album","record_type":"ep","release_date":"2004-04-04"}]}"#,
                )
            } else {
                ResponseTemplate::new(404)
            };
            return match *self.hold_catalog.lock() {
                Some(hold) => answer.set_delay(hold),
                None => answer,
            };
        }

        // Navidrome's native API, for a library artist only.
        if path == "/api/artist/ar-1" {
            return json(r#"{"id":"ar-1","name":"Test Artist","albumCount":1,"songCount":2,"size":1}"#);
        }
        if path == "/api/album" && query_value(request, "name").as_deref() == Some("Look Back") {
            return json(
                r#"[{"id":"al-9","name":"Don't Look Back","albumArtist":"Test Artist","libraryId":1}]"#,
            );
        }
        if path == "/api/album" && query_value(request, "artist_id").as_deref() == Some("ar-1") {
            return json(r#"[{"id":"al-1","name":"Test Album","albumArtistId":"ar-1"}]"#)
                .insert_header("X-Total-Count", "1");
        }
        if path.ends_with("/rest/ping") {
            return json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#);
        }
        ResponseTemplate::new(404)
    }
}

struct Fixture {
    _server: MockServer,
    servers: FakeServers,
    state: AppState,
    app: App,
}

/// The app with the production metadata service over a fake Deezer.
async fn fixture() -> Fixture {
    let servers = FakeServers::default();
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(servers.clone())
        .mount(&server)
        .await;
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            deezer_base_url: Some(format!("{}/deezer", server.uri())),
            ..Default::default()
        },
    );
    Fixture {
        _server: server,
        servers,
        app: app(state.clone()),
        state,
    }
}

fn register_outside_artist(state: &AppState) -> String {
    state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some("Test Artist".into()),
        // Tapped in search: this artist, not the better known one of the name.
        external_artist_id: Some("7".into()),
        ..Default::default()
    })
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|album| album["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn native_artist_an_outside_artist_opens_with_their_albums() {
    // Feishin in Navidrome mode opens an artist page with /api/artist/{id} and asks for
    // the albums with /api/album?artist_id=. Relayed, an outside id reached Navidrome,
    // which has no such artist, and the page never loaded.
    let fixture = fixture().await;
    let id = register_outside_artist(&fixture.state);

    let detail = get(&fixture.app, &format!("/api/artist/{id}")).await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
    let artist = detail.json();
    assert_eq!(artist["id"], id.as_str());
    assert_eq!(artist["name"], "Test Artist");
    assert_eq!(artist["albumCount"], 3);
    assert_eq!(artist["stats"]["albumartist"]["albumCount"], 3);
    assert_eq!(artist["size"], 0);
    assert_eq!(artist["largeImageUrl"], "https://cdn/test-artist.jpg");

    // Feishin's own request for the page: the whole discography, _end=-1.
    let list = get(
        &fixture.app,
        &format!("/api/album?_end=-1&_order=DESC&_sort=max_year&_start=0&artist_id={id}&missing=false"),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.header("X-Total-Count"), Some("3"));
    let albums = list.json();
    // The albums, then the single, as getArtist lists them.
    assert_eq!(names(&albums), ["Other Album", "Test Album", "A Single"]);
    for album in albums.as_array().unwrap() {
        assert_eq!(album["albumArtistId"], id.as_str());
        assert_eq!(album["albumArtist"], "Test Artist");
    }
    assert_eq!(albums[0]["songCount"], 9);

    // Each album opens natively too.
    let opened = get(
        &fixture.app,
        &format!("/api/album/{}", albums[0]["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
}

/// An outside album opened through Navidrome's own API (Feishin, Navidrome's web UI) records, for
/// each of its songs, the reader's context under `navidrome-native` with the catalog's album, the
/// song's place on it and its code.
#[tokio::test]
async fn opening_an_outside_album_natively_records_each_songs_context_for_its_reader() {
    use octo_core::fingerprint::heart_gate::NATIVE_CLIENT;

    let fixture = fixture().await;
    let album_id = fixture.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some("Test Artist".into()),
        album: Some("Test Album".into()),
        external_album_id: Some("1".into()),
        ..Default::default()
    });

    let opened = get(&fixture.app, &format!("/api/album/{album_id}?u=brandon")).await;
    assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
    let listed = get(&fixture.app, &format!("/api/song?album_id={album_id}&u=anna")).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);

    let rows = listed.json();
    let songs = rows.as_array().expect("a list");
    assert_eq!(songs.len(), 4);
    for (place, song) in songs.iter().enumerate() {
        let id = song["id"].as_str().expect("an id");
        let contexts = fixture.state.external_id_registry.contexts(id);
        let read: Vec<(&str, Option<&str>, &str)> = contexts
            .iter()
            .map(|c| (c.user.as_str(), c.client.as_deref(), c.catalog_album_id.as_str()))
            .collect();
        assert_eq!(
            read,
            [
                ("brandon", Some(NATIVE_CLIENT), "1"),
                ("anna", Some(NATIVE_CLIENT), "1")
            ],
            "{id}"
        );
        assert_eq!(contexts[0].track, Some(place as i32 + 1));
        assert_eq!(contexts[0].total, Some(4));
        assert_eq!(
            contexts[0].isrc.as_deref(),
            Some(format!("GBAAA000000{}", place + 1).as_str())
        );
    }
}

#[tokio::test]
async fn native_artist_the_page_and_its_albums_at_once_ask_the_catalog_once() {
    // Feishin asks for the artist and the album list together. Each walked the catalog,
    // album counts and all, so one page cost twice its calls. (Each catalog answer here is late
    // enough that the two requests meet.)
    let fixture = fixture().await;
    let id = register_outside_artist(&fixture.state);
    *fixture.servers.hold_catalog.lock() = Some(Duration::from_millis(300));

    let artist_uri = format!("/api/artist/{id}");
    let albums_uri =
        format!("/api/album?_end=-1&_order=DESC&_sort=max_year&_start=0&artist_id={id}&missing=false");
    let (artist, albums) = tokio::join!(get(&fixture.app, &artist_uri), get(&fixture.app, &albums_uri));
    assert_eq!(artist.status, StatusCode::OK);
    assert_eq!(albums.status, StatusCode::OK);

    let calls = fixture.servers.deezer_calls.lock().clone();
    // The search that names the artist, the listing, and each album's own record, once.
    assert_eq!(
        calls.iter().filter(|p| p.starts_with("/search/artist")).count(),
        1,
        "{calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|p| *p == "/artist/7/albums").count(),
        1,
        "{calls:?}"
    );
    let mut records: Vec<&String> = calls.iter().filter(|p| p.starts_with("/album/")).collect();
    records.sort();
    assert_eq!(records, ["/album/1", "/album/2", "/album/3"], "{calls:?}");
    assert_eq!(calls.len(), 5, "{calls:?}");
}

#[tokio::test]
async fn native_artist_albums_honor_the_page_asked() {
    let fixture = fixture().await;
    let id = register_outside_artist(&fixture.state);

    let response = get(
        &fixture.app,
        &format!("/api/album?_start=1&_end=2&artist_id={id}"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(names(&response.json()), ["Test Album"]);
    assert_eq!(response.header("X-Total-Count"), Some("3"));
}

#[tokio::test]
async fn native_artist_albums_say_what_kind_of_release_they_are() {
    // Navidrome's native album keeps its release types among its tags, lowercase, and
    // Feishin groups an artist's page by them.
    let fixture = fixture().await;
    let id = register_outside_artist(&fixture.state);

    let list = get(
        &fixture.app,
        &format!("/api/album?_start=0&_end=-1&artist_id={id}"),
    )
    .await
    .json();
    let types = |name: &str| -> Vec<String> {
        let album = list
            .as_array()
            .unwrap()
            .iter()
            .find(|album| album["name"] == name)
            .unwrap();
        album["tags"]["releasetype"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(types("Other Album"), ["album"]);
    assert_eq!(types("A Single"), ["single"]);
}

#[tokio::test]
async fn native_artist_a_library_artist_still_comes_from_navidrome() {
    let fixture = fixture().await;
    let detail = get(&fixture.app, "/api/artist/ar-1").await.json();
    assert_eq!(detail["size"], 1);

    let list = get(&fixture.app, "/api/album?_start=0&_end=-1&artist_id=ar-1")
        .await
        .json();
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["al-1"]);
}

#[tokio::test]
async fn native_album_search_an_owned_album_is_not_added_again_over_an_apostrophe() {
    // The library has "Don't Look Back"; the catalog calls it "Don’t Look Back". The
    // native search matched the two by exact text, so the album showed up twice.
    let fixture = fixture().await;
    let response = get(&fixture.app, "/api/album?_start=0&_end=20&name=Look%20Back").await;
    assert_eq!(response.status, StatusCode::OK);
    let list = response.json();
    assert_eq!(names(&list), ["Don't Look Back", "Look Back Again"]);
    assert_eq!(list[0]["id"], "al-9");
    assert_eq!(response.header("X-Total-Count"), Some("2"));
}

// ---------------------------------------------------------------------------------------
// The safety net and the native song answers.
// ---------------------------------------------------------------------------------------

async fn plain_fixture(metadata: TestMetadata) -> (MockServer, AppState, App) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(json(r#"[{"id":"lib-1","title":"Library Song"}]"#).insert_header("X-Total-Count", "1"))
        .mount(&server)
        .await;
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                ..Default::default()
            },
            last_fm: LastFmSettings::default(),
            ..Default::default()
        },
        TestServices {
            metadata: Some(Arc::new(metadata)),
            ..Default::default()
        },
    );
    let app = app(state.clone());
    (server, state, app)
}

/// Step 3: any id-shaped parameter holding an external id gets the empty ok of the endpoint's
/// element, in XML an empty element and in JSON none.
#[tokio::test]
async fn an_external_id_on_an_unhandled_endpoint_gets_the_empty_ok() {
    let (_server, state, app) = plain_fixture(TestMetadata::default()).await;
    let id = state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("A".into()),
        title: Some("B".into()),
        ..Default::default()
    });
    let xml: Reply = get(&app, &format!("/rest/getTopSongs.view?u=a&p=b&albumId={id}")).await;
    assert_eq!(xml.status, StatusCode::OK);
    assert_eq!(xml.header("content-type"), Some("application/xml"));
    assert!(xml.body.contains("<topSongs />"), "{}", xml.body);
    let json = get(&app, &format!("/rest/unstar?u=a&p=b&f=json&id={id}")).await;
    assert_eq!(
        json.body,
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#
    );
    // A DELETE on a Subsonic route reaches the catch-all too.
    let delete = send(
        &app,
        Method::DELETE,
        &format!("/rest/getAlbum?id={id}"),
        &[],
        Body::empty(),
    )
    .await;
    assert!(delete.body.contains("<album />"), "{}", delete.body);
}

#[test]
fn element_names_come_from_the_endpoint() {
    use crate::controllers::subsonic::native::element_for;
    for (endpoint, element) in [
        ("rest/getTopSongs.view", "topSongs"),
        ("rest/unstar", "unstar"),
        ("rest/GETSTARRED", "sTARRED"),
        ("rest/get", "get"),
        ("rest/", "response"),
        ("api/song/x", "x"),
    ] {
        assert_eq!(element_for(endpoint), element, "{endpoint}");
    }
}

/// Step 4: an outside song's native detail, in Navidrome's song shape.
#[tokio::test]
async fn a_native_song_detail_for_an_outside_song_is_answered_by_octo() {
    let metadata = TestMetadata::default();
    let id = "ext-song-1".to_string();
    metadata.songs.lock().insert(
        id.clone(),
        Song {
            id: id.clone(),
            title: "Glass/Harbor".into(),
            artist: "Zephyr Echo".into(),
            album: String::new(),
            duration: Some(241),
            year: Some(2020),
            ..Default::default()
        },
    );
    let tidal = SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Zephyr Echo".into()),
        title: Some("Tidal".into()),
        ..Default::default()
    };
    let known = crate::services::soulseek::ExternalIdRegistry::make_short_id(&tidal);
    metadata.songs.lock().insert(
        known.clone(),
        Song {
            id: known.clone(),
            title: "Tidal".into(),
            artist: "Zephyr Echo".into(),
            duration: Some(241),
            ..Default::default()
        },
    );
    let (_server, state, app) = plain_fixture(metadata).await;
    let reply = get(&app, "/api/song/ext-soulseek-song-ext-song-1").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert!(
        reply.header("content-length").is_none(),
        "written to the body, so chunked"
    );
    let object = reply.json();
    assert_eq!(object["path"], "Zephyr Echo/Unknown/Glass_Harbor.m4a");
    assert_eq!(object["albumId"], "ext-song-1-al");
    // No estimate: the size is the stream's true one, 0 until it is learned.
    assert_eq!(object["size"], 0);
    assert_eq!(object["bitRate"], 128);
    assert_eq!(object["year"], 2020);
    assert!(object.get("genre").is_none());
    assert_eq!(object["createdAt"], "2020-01-01T00:00:00Z");

    // Once the stream is known, its true size and its format.
    assert_eq!(state.external_id_registry.register(tidal), known);
    state.external_id_registry.set_you_tube_id(&known, "vid-a");
    state.external_id_registry.remember_stream(
        &known,
        "vid-a",
        &StreamFacts {
            size: Some(7_012_345),
            itag: Some("251".into()),
            ext: Some("webm".into()),
            bit_rate: Some(160),
            ..Default::default()
        },
    );
    let object = get(&app, &format!("/api/song/ext-soulseek-song-{known}"))
        .await
        .json();
    assert_eq!(object["size"], 7_012_345);
    assert_eq!(object["suffix"], "webm");
    assert_eq!(object["bitRate"], 160);
}

/// Typed native clients read size and bitRate as numbers, so an unknown one is 0, never missing.
#[tokio::test]
async fn a_native_song_with_an_unknown_size_or_bit_rate_says_zero() {
    let metadata = TestMetadata::default();
    let song = SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Zephyr Echo".into()),
        title: Some("Undertow".into()),
        ..Default::default()
    };
    let id = crate::services::soulseek::ExternalIdRegistry::make_short_id(&song);
    metadata.songs.lock().insert(
        id.clone(),
        Song {
            id: id.clone(),
            title: "Undertow".into(),
            artist: "Zephyr Echo".into(),
            duration: Some(200),
            ..Default::default()
        },
    );
    let (_server, state, app) = plain_fixture(metadata).await;
    assert_eq!(state.external_id_registry.register(song), id);
    state.external_id_registry.set_you_tube_id(&id, "vid-b");
    // Opus in webm, its bit rate not measured, its size not known yet.
    state.external_id_registry.remember_stream(
        &id,
        "vid-b",
        &StreamFacts {
            itag: Some("251".into()),
            ext: Some("webm".into()),
            ..Default::default()
        },
    );

    let object = get(&app, &format!("/api/song/ext-soulseek-song-{id}"))
        .await
        .json();

    assert_eq!(object["suffix"], "webm");
    assert_eq!(object["size"], 0);
    assert_eq!(object["bitRate"], 0);
}

/// Step 5: a first-page native song search gains discovery rows, and X-Total-Count says so.
#[tokio::test]
async fn a_native_song_search_without_discovery_is_relayed_untouched() {
    let (_server, _state, app) = plain_fixture(TestMetadata::default()).await;
    // No Last.fm key: discovery has nothing, so the page is Navidrome's own.
    let reply = get(&app, "/api/song?title=glass&_start=0&_end=50").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("X-Total-Count"), Some("1"));
    assert_eq!(reply.json().as_array().unwrap().len(), 1);
}

#[test]
fn native_album_objects_keep_their_wire_shape() {
    let album = Album {
        id: "al".into(),
        title: "T".into(),
        artist: "A".into(),
        songs: vec![
            Song {
                duration: Some(100),
                ..Default::default()
            },
            Song {
                duration: Some(50),
                ..Default::default()
            },
        ],
        release_types: vec!["EP".into()],
        ..Default::default()
    };
    let text = octo_core::json::to_string(&crate::controllers::subsonic::native::native_album_object(
        &album, 3,
    ));
    assert_eq!(
        text,
        r#"{"id":"al","libraryId":3,"name":"T","albumArtist":"A","albumArtistId":"al-ar","maxYear":0,"minYear":0,"compilation":false,"missing":false,"songCount":2,"duration":150,"size":0,"playCount":0,"createdAt":"2020-01-01T00:00:00Z","updatedAt":"2020-01-01T00:00:00Z","tags":{"releasetype":["ep"]}}"#
    );
}

/// Deezer, MusicBrainz, AcoustID and the shim for the Help! context: Deezer and MusicBrainz answer
/// from what they answered when it was recorded (Deezer under `/deezer`, MusicBrainz under
/// `/ws/2`); anything else, AcoustID and the shim included, is refused and kept, so a test sees
/// every question that left Octo.
#[derive(Clone)]
struct RecordedHosts {
    deezer: Arc<octo_core::fingerprint::heart_release::RecordedAnswers>,
    music_brainz: Arc<octo_core::fingerprint::heart_release::RecordedAnswers>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl RecordedHosts {
    fn open() -> RecordedHosts {
        let folder = |name: &str| {
            Arc::new(
                octo_core::fingerprint::heart_release::RecordedAnswers::open(
                    &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../octo-core/tests/fixtures/heart_album")
                        .join(name),
                )
                .expect("the folder's urls.tsv reads"),
            )
        };
        RecordedHosts {
            deezer: folder("deezer"),
            music_brainz: folder("rows/beatles-yesterday_a0"),
            asked: Arc::default(),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }
}

impl Respond for RecordedHosts {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        use octo_core::fingerprint::heart_release::RecordedAnswer;

        let path = request.url.path();
        let question = |prefix: &str| {
            let path = path.strip_prefix(prefix)?;
            Some(match request.url.query() {
                Some(query) => format!("{path}?{query}"),
                None => path.to_string(),
            })
        };
        self.asked.lock().push(match request.url.query() {
            Some(query) => format!("{path}?{query}"),
            None => path.to_string(),
        });
        let recorded = match (question("/deezer/"), question("/ws/2/")) {
            (Some(asked), _) => self.deezer.get(&asked),
            (_, Some(asked)) => self.music_brainz.get(&asked),
            _ => None,
        };
        match recorded {
            Some(RecordedAnswer::Found(doc)) => json(&doc.to_string()),
            Some(RecordedAnswer::NotFound) => ResponseTemplate::new(404),
            None if path.ends_with("/rest/ping") => {
                json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#)
            }
            None => ResponseTemplate::new(404),
        }
    }
}

/// Opening Help! through Navidrome's own API and hearting Yesterday reads the Help! context: the
/// heart gate the app wires (with fake hosts) asks the catalog for that very album and its barcode
/// release, never searches for an album by its title, and takes R from it: the 2009 remaster's
/// recording, the 1987 remix.
#[tokio::test]
async fn opening_help_natively_and_hearting_yesterday_reads_the_help_context() {
    use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, NATIVE_CLIENT};

    use crate::services::common::{GateStart, HeartGates, Holder};

    let hosts = RecordedHosts::open();
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(hosts.clone())
        .mount(&server)
        .await;
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            deezer_base_url: Some(format!("{}/deezer", server.uri())),
            music_brainz_base_url: Some(format!("{}/ws/2/", server.uri())),
            acoust_id_base_url: Some(format!("{}/acoustid/", server.uri())),
            raw_settings: vec![("YouTube:ShimUrl".into(), format!("{}/shim", server.uri()))],
            heart_gate: true,
            ..Default::default()
        },
    );
    let app = app(state.clone());
    assert!(
        state.download_verification.heart_ready(),
        "the app wired the gate"
    );
    let album = state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some("The Beatles".into()),
        album: Some("Help! (Remastered 2009)".into()),
        external_album_id: Some("12047944".into()),
        ..Default::default()
    });

    let opened = get(&app, &format!("/api/album/{album}?u=brandon")).await;
    assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
    let listed = get(&app, &format!("/api/song?album_id={album}&u=brandon")).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let rows = listed.json();
    let yesterday = rows
        .as_array()
        .expect("a list")
        .iter()
        .find(|song| {
            song["title"]
                .as_str()
                .is_some_and(|title| title.starts_with("Yesterday"))
        })
        .and_then(|song| song["id"].as_str())
        .expect("Yesterday is on Help!")
        .to_string();
    let contexts = state.external_id_registry.contexts(&yesterday);
    assert_eq!(contexts.len(), 1, "{contexts:?}");
    assert_eq!(contexts[0].client.as_deref(), Some(NATIVE_CLIENT));
    assert_eq!(contexts[0].catalog_album_id, "12047944");

    let before = hosts.asked().len();
    let (_, provider, external_id) = state.local_library.parse_song_id(&yesterday);
    let gates: &dyn HeartGates = state.download_verification.as_ref();
    let built = gates
        .build(
            &provider.expect("an outside song"),
            &external_id.expect("an outside song"),
            &Holder {
                user: "brandon".into(),
                ..Default::default()
            },
            AcquisitionOrigin::Heart,
        )
        .await;
    let GateStart::Ready(gate) = built else {
        panic!("the heart held: {built:?}");
    };
    assert_eq!(
        gate.r.expect("R").ids,
        ["c05194a3-f6f0-4f52-b78a-13fb5580bc0f"],
        "the 1987 remix the 2009 Help! carries"
    );
    let asked: Vec<String> = hosts.asked().split_off(before);
    assert!(
        asked.iter().any(|q| q.starts_with("/deezer/album/12047944")),
        "the context's album: {asked:?}"
    );
    assert!(
        asked.iter().any(|q| q.contains("barcode")),
        "its barcode release: {asked:?}"
    );
    assert!(
        !asked.iter().any(|q| q.contains("search/album")),
        "no album found by its title: {asked:?}"
    );
    assert!(
        asked
            .iter()
            .all(|q| q.starts_with("/deezer/") || q.starts_with("/ws/2/") || q.starts_with("/shim/")),
        "nothing else was asked: {asked:?}"
    );
}
