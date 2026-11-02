//! The Subsonic handlers on a server Deezer does not serve: the catalog stand-in wired, a mock
//! Deezer answering as Deezer answers India, and a mock Apple answering from recorded answers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;
use octo_core::settings::AppSettings;
use serde_json::{Value, json};
use wiremock::matchers::{any, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::test_app::*;
use crate::app::{AppState, TestServices};

const AUTH: &str = "u=alice&t=good&s=salt&v=1.16.1&c=test";
const SOUNDTRACK: &str = "Interstellar (Original Motion Picture Soundtrack) [Expanded Edition]";
const TWIN: &str = "185320622";
const STAY_ISRC: &str = "USNLR1400777";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../octo-core/tests/fixtures/apple")
}

fn recorded(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(name)).expect("the recorded answer reads")
}

fn ok(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.into(), "application/json")
}

/// The library: the soundtrack holding one song, "Stay", as the reporter's library had it (382 s,
/// no ISRC: Octo's copy of "S.T.A.Y."), or with Deezer's ISRC of "Stay" (412 s).
#[derive(Clone)]
struct Library;

impl Respond for Library {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let query = query_of(request);
        let album = |id: &str, song: Value| {
            ok(json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "type": "navidrome", "album": {
                "id": id, "name": SOUNDTRACK, "artist": "Hans Zimmer", "songCount": 1, "song": [song]}}})
            .to_string())
        };
        if path.ends_with("/rest/getAlbum") {
            match query.get("id").map(String::as_str) {
                Some("al-stay") => {
                    return album(
                        "al-stay",
                        json!({"id": "st-1", "title": "Stay", "album": SOUNDTRACK, "albumId": "al-stay",
                               "artist": "Hans Zimmer", "track": 1, "duration": 382, "isrc": []}),
                    );
                }
                Some("al-isrc") => {
                    return album(
                        "al-isrc",
                        json!({"id": "st-2", "title": "Stay", "album": SOUNDTRACK, "albumId": "al-isrc",
                               "artist": "Hans Zimmer", "track": 5, "duration": 412, "isrc": [STAY_ISRC]}),
                    );
                }
                _ => {}
            }
        }
        if path.ends_with("/rest/search3") {
            return ok(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","searchResult3":{}}}"#,
            );
        }
        if path.ends_with("/rest/ping") {
            return ok(NAVIDROME_OK);
        }
        ResponseTemplate::new(404)
    }
}

/// Deezer as from India: `/infos` closed, catalog searches hidden, the soundtrack's barcode, ISRC
/// and album known, every other barcode unknown.
async fn deezer_india(deezer: &MockServer, barcode_delay: Duration) {
    Mock::given(path("/infos"))
        .respond_with(ok(r#"{"country_iso":"IN","country":"India","open":false}"#))
        .mount(deezer)
        .await;
    Mock::given(path_regex(r"^/search(/album|/track)?$"))
        .respond_with(ok(r#"{"data":[],"total":128}"#))
        .mount(deezer)
        .await;
    Mock::given(path("/album/upc:794043201943"))
        .respond_with(ok(recorded("deezer-album-upc-794043201943.json")).set_delay(barcode_delay))
        .mount(deezer)
        .await;
    Mock::given(path_regex(r"^/album/upc:"))
        .respond_with(ok(
            r#"{"error":{"type":"DataException","message":"no data","code":800}}"#,
        ))
        .mount(deezer)
        .await;
    the_twin_on(deezer).await;
    // Deezer still answers its artist search and an artist's own record here.
    Mock::given(path("/search/artist"))
        .respond_with(ok(recorded("deezer-search-artist-hans-zimmer.json")))
        .mount(deezer)
        .await;
    Mock::given(path("/artist/1935"))
        .respond_with(ok(recorded("deezer-artist-1935.json")))
        .mount(deezer)
        .await;
    Mock::given(path(format!("/track/isrc:{STAY_ISRC}")))
        .respond_with(ok(json!({
            "id": 1_137_370_872u64, "title": "Stay", "duration": 412, "isrc": STAY_ISRC,
            "artist": {"id": 1935, "name": "Hans Zimmer"},
            "album": {"id": 185_320_622u64, "title": "Interstellar (Original Motion Picture Soundtrack) (Expanded Edition)"}
        })
        .to_string()))
        .mount(deezer)
        .await;
}

/// Deezer's own soundtrack album and its tracklist.
async fn the_twin_on(deezer: &MockServer) {
    Mock::given(path(format!("/album/{TWIN}")))
        .respond_with(ok(recorded("deezer-album-upc-794043201943.json")))
        .mount(deezer)
        .await;
    Mock::given(path(format!("/album/{TWIN}/tracks")))
        .respond_with(ok(recorded("deezer-album-185320622-tracks.json")))
        .mount(deezer)
        .await;
}

/// Apple's album and song searches for "interstellar", the soundtrack's tracklist, and a
/// tracklist of its own for Harkirat Sangha's "Interstellar" (an album only Apple lists).
async fn apple_answers(apple: &MockServer) {
    Mock::given(path("/search"))
        .and(query_param("entity", "album"))
        .respond_with(ok(recorded("search-album-interstellar-in.json")))
        .mount(apple)
        .await;
    Mock::given(path("/search"))
        .and(query_param("entity", "song"))
        .respond_with(ok(recorded("search-song-hans-zimmer-stay-in.json")))
        .mount(apple)
        .await;
    Mock::given(path("/lookup"))
        .and(query_param("id", "1846285369"))
        .respond_with(ok(apple_only_tracklist()))
        .with_priority(1)
        .mount(apple)
        .await;
    Mock::given(path("/search"))
        .and(query_param("entity", "musicArtist"))
        .respond_with(ok(recorded("search-artist-hans-zimmer-in.json")))
        .mount(apple)
        .await;
    Mock::given(path("/lookup"))
        .and(query_param("entity", "album"))
        .respond_with(ok(recorded("lookup-454295032-albums-in.json")))
        .with_priority(2)
        .mount(apple)
        .await;
    Mock::given(path("/lookup"))
        .respond_with(ok(recorded("lookup-1533983552-songs-in.json")))
        .mount(apple)
        .await;
}

/// The album only Apple lists, with its 16 songs.
fn apple_only_tracklist() -> String {
    let search: Value = serde_json::from_str(&recorded("search-album-interstellar-in.json")).unwrap();
    let album = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|album| album["collectionId"] == 1_846_285_369u64)
        .unwrap()
        .clone();
    let mut results = vec![album.clone()];
    for n in 1..=16u64 {
        results.push(json!({
            "wrapperType": "track", "kind": "song", "trackId": 1_846_285_369u64 + n,
            "trackName": format!("Song {n}"), "collectionId": 1_846_285_369u64,
            "collectionName": album["collectionName"], "artistName": album["artistName"],
            "artistId": album["artistId"], "trackNumber": n, "discNumber": 1,
            "trackTimeMillis": 180_000 + n * 1_000, "trackExplicitness": "notExplicit",
        }));
    }
    json!({"resultCount": results.len(), "results": results}).to_string()
}

struct Closed {
    state: AppState,
    deezer: MockServer,
    apple: MockServer,
    _library: MockServer,
}

impl Closed {
    async fn new(barcode_delay: Duration) -> Closed {
        let library = MockServer::start().await;
        Mock::given(any()).respond_with(Library).mount(&library).await;
        let deezer = MockServer::start().await;
        deezer_india(&deezer, barcode_delay).await;
        let apple = MockServer::start().await;
        apple_answers(&apple).await;
        let state = state_over(&library, &deezer, Some(&apple));
        state
            .deezer_reach
            .as_ref()
            .expect("the stand-in is wired")
            .read_infos()
            .await;
        Closed {
            state,
            deezer,
            apple,
            _library: library,
        }
    }

    async fn json(&self, uri: &str) -> Value {
        let reply = get(&app(&self.state), uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.text());
        reply.json()
    }

    async fn search_albums(&self) -> Vec<Value> {
        self.search_albums_for("interstellar").await
    }

    /// search3 as the Android app asks it.
    async fn search_albums_for(&self, query: &str) -> Vec<Value> {
        self.json(&format!(
            "/rest/search3.view?{AUTH}&f=json&query={query}&artistCount=20&albumCount=20&songCount=20"
        ))
        .await["subsonic-response"]["searchResult3"]["album"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    async fn album(&self, id: &str) -> Vec<Value> {
        self.json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id={id}"))
            .await["subsonic-response"]["album"]["song"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The catalog album an id the apps were handed opens.
    fn opens(&self, id: &str) -> Option<String> {
        self.state
            .external_id_registry
            .lookup(id)?
            .snapshot()
            .external_album_id
    }
}

/// The app over the library, Deezer and (when given) Apple, the stand-in wired between them.
fn state_over(library: &MockServer, deezer: &MockServer, apple: Option<&MockServer>) -> AppState {
    let refused = "http://127.0.0.1:0".to_string();
    AppState::for_tests_with(
        settings(&library.uri()),
        TestServices {
            deezer_base_url: Some(deezer.uri()),
            apple_catalog_base_url: apple.map(MockServer::uri),
            raw_settings: vec![
                ("YouTube:ShimUrl".into(), refused.clone()),
                ("Soulseek:BaseUrl".into(), refused),
            ],
            ..Default::default()
        },
    )
}

fn soundtrack_row(albums: &[Value]) -> &Value {
    albums
        .iter()
        .find(|album| album["name"] == SOUNDTRACK && album["artist"] == "Hans Zimmer")
        .unwrap_or_else(|| panic!("the soundtrack is listed: {albums:?}"))
}

fn titles(songs: &[Value]) -> Vec<String> {
    songs
        .iter()
        .map(|song| song["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn search_lists_the_soundtrack_opened_by_its_deezer_twin() {
    let closed = Closed::new(Duration::ZERO).await;
    let albums = closed.search_albums().await;
    let row = soundtrack_row(&albums);
    let id = row["id"].as_str().expect("an id");
    assert_eq!(closed.opens(id).as_deref(), Some(TWIN));
    let songs = closed.album(id).await;
    assert_eq!(songs.len(), 30);
    assert!(
        closed
            .deezer
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|request| request.url.path() == format!("/album/{TWIN}/tracks")),
        "the soundtrack opens as Deezer's album"
    );
}

#[tokio::test]
async fn a_held_song_fills_its_album_to_thirty_songs_and_keeps_the_other_stay() {
    let closed = Closed::new(Duration::ZERO).await;
    let songs = closed.album("al-stay").await;
    assert_eq!(songs.len(), 30, "{:?}", titles(&songs));
    assert!(
        songs.iter().any(|song| song["id"] == "st-1"),
        "the library's own song"
    );
    assert!(
        songs
            .iter()
            .any(|song| song["title"] == "Stay" && song["duration"] == 412 && song["id"] != "st-1"),
        "Deezer's 412 s Stay is another recording than the library's 382 s file"
    );
    assert!(
        !songs.iter().any(|song| song["title"] == "S.T.A.Y."),
        "the library's file is S.T.A.Y.: that row is owned"
    );
}

#[tokio::test]
async fn a_held_song_with_an_isrc_fills_from_its_deezer_album_with_isrcs() {
    let closed = Closed::new(Duration::ZERO).await;
    let songs = closed.album("al-isrc").await;
    assert_eq!(songs.len(), 30, "{:?}", titles(&songs));
    assert!(
        songs
            .iter()
            .filter(|song| song["id"] != "st-2")
            .all(|song| song["title"] != "Stay"),
        "the library's Stay owns Deezer's Stay by its ISRC"
    );
    let asked: Vec<String> = closed
        .deezer
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| request.url.path().to_string())
        .collect();
    assert!(asked.contains(&format!("/track/isrc:{STAY_ISRC}")), "{asked:?}");
    assert_eq!(
        closed.apple.received_requests().await.unwrap_or_default().len(),
        0,
        "the ISRC's album held the song: no name search"
    );
}

#[tokio::test]
async fn an_album_only_apple_lists_opens_with_every_song_and_its_routing() {
    let closed = Closed::new(Duration::ZERO).await;
    let albums = closed.search_albums().await;
    let row = albums
        .iter()
        .find(|album| album["name"] == "Interstellar" && album["artist"] == "Harkirat Sangha")
        .unwrap_or_else(|| panic!("Apple's album is listed: {albums:?}"));
    let id = row["id"].as_str().expect("an id");
    let songs = closed.album(id).await;
    assert_eq!(songs.len(), 16, "{:?}", titles(&songs));
    for song in &songs {
        let id = song["id"].as_str().expect("a song id");
        let routing = closed
            .state
            .external_id_registry
            .lookup(id)
            .unwrap_or_else(|| panic!("{id} routes"))
            .snapshot();
        assert_eq!(routing.artist.as_deref(), Some("Harkirat Sangha"));
        assert!(
            routing
                .title
                .as_deref()
                .is_some_and(|title| title.starts_with("Song "))
        );
        assert!(routing.duration.is_some_and(|seconds| seconds > 180));
    }
    assert!(
        closed
            .deezer
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .all(|request| !request.url.path().contains("1846285369")),
        "an Apple id never reaches Deezer"
    );
}

#[tokio::test]
async fn an_outside_artists_page_lists_apples_albums() {
    let closed = Closed::new(Duration::ZERO).await;
    let artists = closed
        .json(&format!(
            "/rest/search3.view?{AUTH}&f=json&query=hans%20zimmer&artistCount=20&albumCount=0&songCount=0"
        ))
        .await["subsonic-response"]["searchResult3"]["artist"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let zimmer = artists
        .iter()
        .find(|artist| artist["name"] == "Hans Zimmer")
        .unwrap_or_else(|| panic!("Hans Zimmer is listed: {artists:?}"));
    let id = zimmer["id"].as_str().expect("an id");
    let page = closed
        .json(&format!("/rest/getArtist.view?{AUTH}&f=json&id={id}"))
        .await;
    let albums = page["subsonic-response"]["artist"]["album"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        albums
            .iter()
            .any(|album| album["name"] == "Inception (Music from the Motion Picture)"),
        "Apple's albums of the artist: {:?}",
        titles_of(&albums)
    );
}

fn titles_of(albums: &[Value]) -> Vec<String> {
    albums
        .iter()
        .map(|album| album["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn the_soundtrack_keeps_one_id_before_and_after_its_twin_is_known() {
    // Deezer's barcode answer outlasts the search's wait for twins: the first search lists Apple's
    // album, a later one Deezer's twin.
    let closed = Closed::new(Duration::from_millis(2_500)).await;
    let first = closed.search_albums().await;
    let before = soundtrack_row(&first)["id"].as_str().unwrap().to_string();
    assert_ne!(
        closed.opens(&before).as_deref(),
        Some(TWIN),
        "listed before its twin was known"
    );
    let mut after = String::new();
    // Each search in other words, so none is answered from the first one's kept answer.
    for query in [
        "interstellar%20zimmer",
        "zimmer%20interstellar",
        "interstellar%20hans",
        "hans%20zimmer%20interstellar",
    ] {
        let again = closed.search_albums_for(query).await;
        after = soundtrack_row(&again)["id"].as_str().unwrap().to_string();
        if closed.opens(&after).as_deref() == Some(TWIN) {
            break;
        }
    }
    assert_eq!(
        closed.opens(&after).as_deref(),
        Some(TWIN),
        "the twin became known"
    );
    assert_eq!(before, after, "one album, one id");
}

#[tokio::test]
async fn an_album_page_never_waits_on_a_silent_infos_while_deezer_answers() {
    let library = MockServer::start().await;
    Mock::given(any()).respond_with(Library).mount(&library).await;
    let deezer = MockServer::start().await;
    Mock::given(path("/infos"))
        .respond_with(ok("{}").set_delay(Duration::from_secs(30)))
        .mount(&deezer)
        .await;
    Mock::given(path_regex(r"^/search"))
        .respond_with(ok(r#"{"data":[],"total":0}"#))
        .mount(&deezer)
        .await;
    let apple = MockServer::start().await;
    let state = state_over(&library, &deezer, Some(&apple));
    let started = std::time::Instant::now();
    let reply = get(
        &app(&state),
        &format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-stay"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    let asked: Vec<String> = deezer
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| request.url.path().to_string())
        .collect();
    assert!(!asked.iter().any(|path| path == "/infos"), "{asked:?}");
    assert_eq!(apple.received_requests().await.unwrap_or_default().len(), 0);
}

/// An outside song whose explicit word got no answer: a Clean only server never lists it, any
/// other lists it with no mark.
#[tokio::test]
async fn clean_only_never_lists_an_outside_song_whose_words_got_no_answer() {
    async fn search_songs(filter: octo_core::settings::ExplicitFilter) -> Vec<Value> {
        let library = MockServer::start().await;
        Mock::given(any()).respond_with(Library).mount(&library).await;
        // Last.fm names the song in time; the catalog answers none of its lookups.
        let last_fm = MockServer::start().await;
        Mock::given(any())
            .respond_with(ok(
                r#"{"results":{"trackmatches":{"track":[{"name":"Stay","artist":"Hans Zimmer","listeners":"250000"}]}}}"#,
            ))
            .mount(&last_fm)
            .await;
        let deezer = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(&deezer)
            .await;
        let mut settings = settings(&library.uri());
        settings.subsonic.explicit_filter = filter;
        settings.last_fm.api_key = "a-test-key".into();
        let refused = "http://127.0.0.1:0".to_string();
        let state = AppState::for_tests_with(
            settings,
            TestServices {
                deezer_base_url: Some(deezer.uri()),
                last_fm_base_url: Some(last_fm.uri()),
                raw_settings: vec![
                    ("YouTube:ShimUrl".into(), refused.clone()),
                    ("Soulseek:BaseUrl".into(), refused),
                ],
                ..Default::default()
            },
        );
        let reply = get(
            &app(&state),
            &format!(
                "/rest/search3.view?{AUTH}&f=json&query=interstellar&artistCount=0&albumCount=0&songCount=20"
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        reply.json()["subsonic-response"]["searchResult3"]["song"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
    let all = search_songs(octo_core::settings::ExplicitFilter::All).await;
    let stay = all
        .iter()
        .find(|song| song["title"] == "Stay")
        .unwrap_or_else(|| panic!("the outside song is listed: {all:?}"));
    assert!(
        stay.get("explicitStatus").is_none_or(|status| status == ""),
        "no word is shown for words not known: {stay}"
    );
    let clean = search_songs(octo_core::settings::ExplicitFilter::CleanOnly).await;
    assert!(
        !clean.iter().any(|song| song["title"] == "Stay"),
        "Clean only never lists it: {clean:?}"
    );
}

#[test]
fn only_the_servers_own_build_wires_the_stand_in() {
    let state = AppState::for_tests(AppSettings::default());
    assert!(
        state.deezer_reach.is_none(),
        "a test build wires none unless asked"
    );
    let source = include_str!("../../app.rs");
    let wired = source
        .find("CatalogStandIn::wire(&clients, &config_dir)")
        .expect("the server's build wires the stand-in");
    let first_worker = source[wired..]
        .find(".register_workers(&workers")
        .map(|at| at + wired)
        .expect("workers are registered");
    let before_wiring = &source[..wired];
    let build = before_wiring.rfind("fn build(").expect("the build");
    assert!(
        !source[build..wired].contains(".register_workers(&workers"),
        "the stand-in is wired before any worker"
    );
    assert!(first_worker > wired);
    assert_eq!(
        source.matches("CatalogStandIn::wire(&clients").count(),
        1,
        "wired in one place"
    );
}

/// Deezer serving this server: every request a flow makes, with the stand-in wired and without.
#[tokio::test]
async fn on_a_server_deezer_serves_the_wired_stand_in_changes_no_request() {
    async fn requests(wired: bool) -> (Vec<String>, usize) {
        let library = MockServer::start().await;
        Mock::given(any()).respond_with(Library).mount(&library).await;
        let deezer = MockServer::start().await;
        Mock::given(path("/infos"))
            .respond_with(ok(
                r#"{"country_iso":"US","country":"United States of America","open":true}"#,
            ))
            .mount(&deezer)
            .await;
        Mock::given(path("/search/album"))
            .respond_with(ok(json!({"data": [{
                "id": 185_320_622u64, "title": "Interstellar (Original Motion Picture Soundtrack) (Expanded Edition)",
                "artist": {"id": 1935, "name": "Hans Zimmer"}, "nb_tracks": 30, "record_type": "album"}], "total": 1})
            .to_string()))
            .mount(&deezer)
            .await;
        Mock::given(path_regex(r"^/search"))
            .respond_with(ok(r#"{"data":[],"total":0}"#))
            .mount(&deezer)
            .await;
        the_twin_on(&deezer).await;
        let apple = MockServer::start().await;
        let state = state_over(&library, &deezer, wired.then_some(&apple));
        let app = app(&state);
        for uri in [
            format!(
                "/rest/search3.view?{AUTH}&f=json&query=interstellar&artistCount=0&albumCount=20&songCount=0"
            ),
            format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-stay"),
        ] {
            assert_eq!(get(&app, &uri).await.status, StatusCode::OK);
        }
        let mut paths: Vec<String> = deezer
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| format!("{}?{}", request.url.path(), request.url.query().unwrap_or("")))
            .collect();
        paths.sort();
        (paths, apple.received_requests().await.unwrap_or_default().len())
    }
    let (unwired, _) = requests(false).await;
    let (wired, asked_apple) = requests(true).await;
    assert!(!unwired.is_empty());
    assert_eq!(wired, unwired, "the same Deezer requests");
    assert_eq!(asked_apple, 0, "Apple is never asked for a server Deezer serves");
}
