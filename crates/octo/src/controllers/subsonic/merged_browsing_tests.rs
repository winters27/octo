//! getAlbum and getArtist merge the outside songs and albums into Navidrome's answer. The merge
//! reads JSON, and an XML client used to get Navidrome's own XML back, without the outside part:
//! half an album. Both formats must now carry the same songs.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use octo_core::settings::AppSettings;
use octo_core::settings::SettingsStore;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_subsonic::xml::XElement;
use parking_lot::Mutex;
use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::test_app::*;
use crate::app::AppState;
use crate::services::cover_art::CoverArtAggregator;
use crate::services::cover_art::animated_art::Clip;
use crate::services::metadata::{DeezerMetadataService, DeezerRateLimitHandler, DeezerRateLimiter};
use crate::services::soulseek::SoulseekMetadataService;
use crate::services::you_tube::YouTubeResolver;

const AUTH: &str = "u=alice&t=good&s=salt&v=1.16.1&c=test";
const OCTO_AUTH: &str = "u=alice&t=good&s=salt&v=1.16.1&c=Octo";

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

fn xml(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/xml")
}

/// Deezer knowing all four tracks of the album Navidrome holds two of.
#[derive(Clone, Default)]
struct FakeDeezer {
    /// Every path the catalog was asked for.
    calls: Arc<Mutex<Vec<String>>>,
}

impl Respond for FakeDeezer {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let query = query_of(request);
        let q = query.get("q").cloned().unwrap_or_default();
        self.calls.lock().push(path.clone());
        // The catalog writes this title with a curly apostrophe; the library's tags do not.
        if path.starts_with("/search/album") && q.contains("Look Back") {
            return json(
                r#"{"data":[{"id":20,"title":"Don’t Look Back","record_type":"album","nb_tracks":10,"artist":{"name":"Test Artist"}},{"id":21,"title":"Look Back Again","record_type":"album","nb_tracks":10,"artist":{"name":"Test Artist"}}]}"#,
            );
        }
        // octo-player#1: a catalog album with the library album's very name, and none of its songs.
        if path.starts_with("/search/album") && q.contains("Nightcore") {
            return json(
                r#"{"data":[{"id":30,"title":"Nightcore","record_type":"album","nb_tracks":3,"artist":{"name":"Nightcore"}}]}"#,
            );
        }
        match path.as_str() {
            "/album/30/tracks" => json(
                r#"{"total":3,"data":[
                  {"title":"Love Tonight (Nightcore Remix)","duration":142,"track_position":1,"disk_number":1,"artist":{"name":"Nightcore"}},
                  {"title":"Angel (Nightcore Remix)","duration":159,"track_position":2,"disk_number":1,"artist":{"name":"Nightcore"}},
                  {"title":"Sad Songs & Depression (Nightcore Remix)","duration":280,"track_position":3,"disk_number":1,"artist":{"name":"Nightcore"}}
                ]}"#,
            ),
            "/album/30" => json(
                r#"{"id":30,"title":"Nightcore","release_date":"2014-01-01","artist":{"name":"Nightcore"}}"#,
            ),
            _ if path.starts_with("/search/album") => json(
                r#"{"data":[{"id":1,"title":"Test Album","record_type":"album","nb_tracks":4,"artist":{"name":"Test Artist"}}]}"#,
            ),
            "/album/1/tracks" => json(
                r#"{"total":4,"data":[
                  {"title":"One","duration":100,"track_position":1,"disk_number":1,"isrc":"GBAAA0000001","artist":{"name":"Test Artist"}},
                  {"title":"Two","duration":200,"track_position":2,"disk_number":1,"isrc":"GBAAA0000002","artist":{"name":"Test Artist"}},
                  {"title":"Three","duration":300,"track_position":3,"disk_number":1,"isrc":"GBAAA0000003","artist":{"name":"Test Artist"}},
                  {"title":"Four","duration":400,"track_position":4,"disk_number":1,"isrc":"GBAAA0000004","artist":{"name":"Test Artist"}}
                ]}"#,
            ),
            "/album/2" => json(
                r#"{"id":2,"title":"Other Album","nb_tracks":9,"release_date":"2005-05-05","artist":{"name":"Test Artist"}}"#,
            ),
            "/album/1" => json(
                r#"{"id":1,"title":"Test Album","release_date":"2001-01-01","artist":{"name":"Test Artist"}}"#,
            ),
            // A bigger act whose name contains this one comes first, and a better known
            // artist of the very same name before the one the library holds.
            _ if path.starts_with("/search/artist") => json(
                r#"{"data":[{"id":8,"name":"Test Artist Orchestra","nb_fan":90000,"picture_xl":"https://cdn/orchestra.jpg"},{"id":9,"name":"Test Artist","nb_fan":5000,"picture_xl":"https://cdn/somebody-else.jpg"},{"id":7,"name":"Test Artist","nb_fan":10,"picture_xl":"https://cdn/test-artist.jpg"}]}"#,
            ),
            _ if path.starts_with("/artist/9/albums") => json(
                r#"{"data":[{"id":90,"title":"Somebody Else's Record","record_type":"album","release_date":"2010-01-01"}]}"#,
            ),
            // The catalog's own shape: no artist and no track counts on this listing. An EP
            // shares the album's title, and would open as the album (or the album as it).
            _ if path.starts_with("/artist/7/albums") => json(
                r#"{"data":[{"id":1,"title":"Test Album","record_type":"album","release_date":"2001-01-01"},{"id":2,"title":"Other Album","record_type":"album","release_date":"2005-05-05"},{"id":3,"title":"A Single","record_type":"single","release_date":"2006-01-01"},{"id":4,"title":"Other Album","record_type":"ep","release_date":"2004-04-04"}]}"#,
            ),
            _ => ResponseTemplate::new(404),
        }
    }
}

/// Navidrome holding two of the album's four tracks.
#[derive(Clone, Default)]
struct FakeNavidrome {
    /// The `f` each getAlbum/getArtist was asked with.
    formats: Arc<Mutex<Vec<String>>>,
}

impl Respond for FakeNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let query = query_of(request);
        let is_json = query.get("f").map(String::as_str) == Some("json");
        if path.ends_with("/rest/getAlbum") || path.ends_with("/rest/getArtist") {
            self.formats
                .lock()
                .push(query.get("f").cloned().unwrap_or_else(|| "xml".into()));
        }
        if path.ends_with("/rest/getAlbum") && query.get("id").map(String::as_str) == Some("al-nc") {
            return json(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","album":{
                  "id":"al-nc","name":"Nightcore","artist":"Nightcore","songCount":3,"duration":518,
                  "song":[
                    {"id":"nc-1","title":"Believer (Rock Version)","album":"Nightcore","artist":"Missigno","track":1,"duration":216},
                    {"id":"nc-2","title":"Mi Mi Mi (Rock Version)","album":"Nightcore","artist":"Missigno","track":2,"duration":168},
                    {"id":"nc-3","title":"MAGIC","album":"Nightcore","artist":"Missigno","track":3,"duration":134}
                  ]}}}"#,
            );
        }
        if path.ends_with("/rest/getAlbum") && query.get("id").map(String::as_str) == Some("al-full") {
            return json(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","album":{
                  "id":"al-full","name":"Test Album","artist":"Test Artist","songCount":4,"duration":1000,
                  "song":[
                    {"id":"f-1","title":"One","album":"Test Album","artist":"Test Artist","track":1,"duration":100,"isrc":["GBAAA0000001"]},
                    {"id":"f-2","title":"Two","album":"Test Album","artist":"Test Artist","track":2,"duration":200,"isrc":["GBAAA0000002"]},
                    {"id":"f-3","title":"Three","album":"Test Album","artist":"Test Artist","track":3,"duration":300,"isrc":["GBAAA0000003"]},
                    {"id":"f-4","title":"Four","album":"Test Album","artist":"Test Artist","track":4,"duration":400,"isrc":["GBAAA0000004"]}
                  ]}}}"#,
            );
        }
        if path.ends_with("/rest/getAlbum") && query.get("id").map(String::as_str) == Some("al-ext") {
            return json(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","album":{
                  "id":"al-ext","name":"Test Album","artist":"Test Artist","songCount":4,"duration":1000}}}"#,
            );
        }
        if path.ends_with("/rest/getAlbum") {
            return if is_json {
                json(
                    r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","album":{
                      "id":"al-1","name":"Test Album","artist":"Test Artist","songCount":2,"duration":400,
                      "genres":[{"name":"Rock"}],
                      "song":[
                        {"id":"s-1","title":"One","album":"Test Album","artist":"Test Artist","track":1,"duration":100,"isrc":["GBAAA0000001"],"replayGain":{"trackGain":-6.5},"path":"Test Artist/Test Album/01 One.flac","size":34684600,"created":"2026-08-14T23:11:28Z","suffix":"flac","bitRate":867},
                        {"id":"s-3","title":"Three","album":"Test Album","artist":"Test Artist","track":3,"duration":300,"isrc":[]}
                      ]}}}"#,
                )
            } else {
                xml(
                    r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><album id="al-1" name="Test Album" artist="Test Artist" songCount="2"><song id="s-1" title="One" track="1"/><song id="s-3" title="Three" track="3"/></album></subsonic-response>"#,
                )
            };
        }
        if path.ends_with("/rest/getArtist") {
            return if is_json {
                json(
                    r#"{"subsonic-response":{"status":"ok","version":"1.16.1","artist":{"id":"ar-1","name":"Test Artist","albumCount":1,"album":[{"id":"al-1","name":"Test Album","artist":"Test Artist","songCount":2,"releaseTypes":["album","compilation"]}]}}}"#,
                )
            } else {
                xml(
                    r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><artist id="ar-1" name="Test Artist" albumCount="1"><album id="al-1" name="Test Album"><releaseTypes>album</releaseTypes><releaseTypes>compilation</releaseTypes></album></artist></subsonic-response>"#,
                )
            };
        }
        if path.ends_with("/rest/ping") {
            return json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#);
        }
        ResponseTemplate::new(404)
    }
}

/// The app over the fake Navidrome, its catalog over the fake Deezer.
struct Merged {
    state: AppState,
    deezer: FakeDeezer,
    navidrome: FakeNavidrome,
    _servers: (MockServer, MockServer),
}

impl Merged {
    async fn new() -> Merged {
        Merged::with_settings(|_| {}).await
    }

    /// The same app with `Subsonic:FillMissingAlbumTracks` set to `fill`.
    async fn with_fill(fill: bool) -> Merged {
        Merged::with_settings(|settings| settings.subsonic.fill_missing_album_tracks = fill).await
    }

    async fn with_settings(change: impl FnOnce(&mut AppSettings)) -> Merged {
        let navidrome = FakeNavidrome::default();
        let navidrome_server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(navidrome.clone())
            .mount(&navidrome_server)
            .await;
        let deezer = FakeDeezer::default();
        let deezer_server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(deezer.clone())
            .mount(&deezer_server)
            .await;

        let deezer_base = deezer_server.uri();
        let mut app_settings = settings(&navidrome_server.uri());
        change(&mut app_settings);
        let state = state_with(app_settings, move |inner| {
            let deezer_settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
            let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
            let catalog = Arc::new(DeezerMetadataService::with_base_url(
                http,
                deezer_settings,
                deezer_base,
            ));
            inner.music_metadata = Arc::new(SoulseekMetadataService::new(
                Arc::new(YouTubeResolver::with_base_url(Some("http://127.0.0.1:1"))),
                inner.external_id_registry.clone(),
                catalog,
                Arc::new(CoverArtAggregator::new(Vec::new())),
                None,
            ));
            inner.metadata_service = inner.music_metadata.clone();
        });
        Merged {
            state,
            deezer,
            navidrome,
            _servers: (navidrome_server, deezer_server),
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        get(&app(&self.state), uri).await
    }

    async fn json(&self, uri: &str) -> Value {
        let reply = self.get(uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.text());
        reply.json()
    }

    async fn xml(&self, uri: &str) -> XElement {
        let reply = self.get(uri).await;
        XElement::parse(&reply.text())
            .unwrap_or_else(|e| panic!("{uri} is not XML ({e:?}): {}", reply.text()))
    }
}

fn names(values: &Value, key: &str) -> Vec<String> {
    values
        .as_array()
        .expect("a list")
        .iter()
        .map(|v| v[key].as_str().unwrap_or_default().to_string())
        .collect()
}

fn ids(values: &Value) -> Vec<String> {
    names(values, "id")
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

fn xml_ids(album: &XElement) -> Vec<String> {
    children(album, "song")
        .iter()
        .map(|song| song.attribute("id").unwrap_or_default().to_string())
        .collect()
}

fn children<'a>(element: &'a XElement, name: &str) -> Vec<&'a XElement> {
    element.elements().filter(|e| e.name == name).collect()
}

fn child<'a>(element: &'a XElement, name: &str) -> &'a XElement {
    element
        .elements()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no <{name}> in <{}>", element.name))
}

#[tokio::test]
async fn get_album_xml_carries_the_same_merged_songs_as_json() {
    let fixture = Merged::new().await;

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-1"))
        .await;
    let json_songs = names(&json["subsonic-response"]["album"]["song"], "title");

    let response = fixture.get(&format!("/rest/getAlbum.view?{AUTH}&id=al-1")).await;
    assert_eq!(response.media_type().as_deref(), Some("application/xml"));
    let xml = XElement::parse(&response.text()).expect("XML");
    let album = child(&xml, "album");
    let xml_songs: Vec<String> = children(album, "song")
        .iter()
        .map(|s| s.attribute("title").unwrap_or_default().to_string())
        .collect();

    assert_eq!(json_songs, ["One", "Two", "Three", "Four"]);
    assert_eq!(json_songs, xml_songs);
    assert_eq!(album.attribute("songCount"), Some("4"));
    assert_eq!(xml.attribute("status"), Some("ok"));
    // Navidrome was asked for JSON both times: the merge reads JSON.
    assert!(fixture.navidrome.formats.lock().iter().all(|f| f == "json"));
}

#[tokio::test]
async fn get_album_leaves_an_album_alone_when_the_catalog_album_only_shares_its_name() {
    let fixture = Merged::new().await;

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-nc"))
        .await;
    let album = &json["subsonic-response"]["album"];

    assert_eq!(
        names(&album["song"], "title"),
        ["Believer (Rock Version)", "Mi Mi Mi (Rock Version)", "MAGIC"]
    );
    assert_eq!(album["songCount"], 3);
    // The catalog was asked, and its album turned down for having none of these songs.
    assert!(
        fixture
            .deezer
            .calls
            .lock()
            .iter()
            .any(|c| c == "/album/30/tracks")
    );
}

#[tokio::test]
async fn get_album_marks_the_songs_the_library_does_not_hold() {
    let fixture = Merged::new().await;

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-1"))
        .await;
    let songs: HashMap<String, Value> = json["subsonic-response"]["album"]["song"]
        .as_array()
        .expect("songs")
        .iter()
        .map(|s| (s["title"].as_str().unwrap_or_default().to_string(), s.clone()))
        .collect();

    // The library's songs go out as Navidrome described them.
    let one = &songs["One"];
    assert_eq!(one["isExternal"], false);
    assert_eq!(one["id"], "s-1");
    assert_eq!(one["path"], "Test Artist/Test Album/01 One.flac");
    assert_eq!(one["size"], 34684600);
    assert_eq!(one["created"], "2026-08-14T23:11:28Z");
    assert_eq!(one["bitRate"], 867);
    assert_eq!(songs["Three"]["isExternal"], false);

    // The outside ones say so, and claim no file.
    for title in ["Two", "Four"] {
        let song = &songs[title];
        assert_eq!(song["isExternal"], true, "{title}");
        for key in [
            "path",
            "size",
            "created",
            "bitDepth",
            "samplingRate",
            "channelCount",
        ] {
            assert!(song.get(key).is_none(), "{title} has {key}");
        }
        assert_eq!(song["suffix"], "m4a", "{title}");
        // Filed under the album the page was opened as, like the library's songs.
        assert_eq!(song["albumId"], "al-1", "{title}");
        assert_eq!(song["parent"], "al-1", "{title}");
    }

    // An app that hides songs without a size (Amperfy) is given one for each outside song,
    // and the library's sizes are left as they are.
    let amperfy = fixture
        .xml(&format!("/rest/getAlbum.view?{AUTH}&id=al-1&c=Amperfy"))
        .await;
    for song in children(child(&amperfy, "album"), "song") {
        match song.attribute("title") {
            Some("One") => assert_eq!(song.attribute("size"), Some("34684600")),
            Some("Two" | "Four") => {
                assert_eq!(song.attribute("size"), Some("1"), "{song:?}");
                assert_eq!(song.attribute("albumId"), Some("al-1"), "{song:?}");
            }
            _ => {}
        }
    }

    // XML says the same.
    let xml = fixture.xml(&format!("/rest/getAlbum.view?{AUTH}&id=al-1")).await;
    let rows: HashMap<String, &XElement> = children(child(&xml, "album"), "song")
        .into_iter()
        .map(|s| (s.attribute("title").unwrap_or_default().to_string(), s))
        .collect();
    assert_eq!(rows["One"].attribute("isExternal"), Some("false"));
    assert_eq!(
        rows["One"].attribute("path"),
        Some("Test Artist/Test Album/01 One.flac")
    );
    assert_eq!(rows["Two"].attribute("isExternal"), Some("true"));
    assert_eq!(rows["Two"].attribute("path"), None);
    assert_eq!(rows["Two"].attribute("created"), None);

    // The outside song opens by its id, marked the same way, without asking Navidrome.
    let two_id = songs["Two"]["id"].as_str().expect("an id").to_string();
    let opened = fixture
        .json(&format!("/rest/getSong.view?{AUTH}&f=json&id={two_id}"))
        .await;
    let song = &opened["subsonic-response"]["song"];
    assert_eq!(song["title"], "Two");
    assert_eq!(song["isExternal"], true);
    assert!(song.get("path").is_none());
}

#[tokio::test]
async fn get_album_xml_writes_lists_and_objects_the_open_subsonic_way() {
    let fixture = Merged::new().await;

    let xml = fixture.xml(&format!("/rest/getAlbum.view?{AUTH}&id=al-1")).await;
    let album = child(&xml, "album");
    let songs = children(album, "song");
    let one = songs
        .iter()
        .find(|s| s.attribute("title") == Some("One"))
        .expect("One");
    let two = songs
        .iter()
        .find(|s| s.attribute("title") == Some("Two"))
        .expect("Two");

    // A list of plain values: one text element each. An object: one child element.
    let isrcs =
        |song: &XElement| -> Vec<String> { children(song, "isrc").iter().map(|e| e.value()).collect() };
    assert_eq!(isrcs(one), ["GBAAA0000001"]);
    assert_eq!(child(one, "replayGain").attribute("trackGain"), Some("-6.5"));
    // An outside song carries Deezer's code the same way.
    assert_eq!(isrcs(two), ["GBAAA0000002"]);
    // A list of objects: one element each, named for the list.
    assert_eq!(child(album, "genres").attribute("name"), Some("Rock"));
    // Plain values are attributes, never child elements.
    assert_eq!(one.attribute("track"), Some("1"));
    assert!(children(one, "track").is_empty());
}

#[tokio::test]
async fn get_album_fill_defaults_on_and_merges_the_catalog_tracks() {
    let fixture = Merged::new().await;
    assert!(
        fixture
            .state
            .settings
            .current()
            .subsonic
            .fill_missing_album_tracks
    );

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-1"))
        .await;
    let album = &json["subsonic-response"]["album"];

    assert_eq!(names(&album["song"], "title"), ["One", "Two", "Three", "Four"]);
    assert_eq!(album["songCount"], 4);
    assert!(album.get("ownedCount").is_none());
    assert!(
        fixture
            .deezer
            .calls
            .lock()
            .iter()
            .any(|call| call == "/album/1/tracks"),
        "the catalog was not asked: {:?}",
        fixture.deezer.calls.lock()
    );
}

#[tokio::test]
async fn get_album_fill_off_keeps_a_complete_local_album() {
    let fixture = Merged::with_fill(false).await;

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-full"))
        .await;
    let album = &json["subsonic-response"]["album"];

    assert_eq!(ids(&album["song"]), ["f-1", "f-2", "f-3", "f-4"]);
    assert_eq!(album["songCount"], 4);
    assert!(album.get("ownedCount").is_none());
    assert!(
        fixture.deezer.calls.lock().is_empty(),
        "the catalog was asked: {:?}",
        fixture.deezer.calls.lock()
    );
}

#[tokio::test]
async fn get_album_fill_off_keeps_a_partial_local_album() {
    let fixture = Merged::with_fill(false).await;

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-1"))
        .await;
    let album = &json["subsonic-response"]["album"];

    assert_eq!(ids(&album["song"]), ["s-1", "s-3"]);
    assert_eq!(names(&album["song"], "title"), ["One", "Three"]);
    assert_eq!(album["songCount"], 2);
    assert!(album.get("ownedCount").is_none());
    assert!(
        fixture.deezer.calls.lock().is_empty(),
        "the catalog was asked: {:?}",
        fixture.deezer.calls.lock()
    );
}

#[tokio::test]
async fn get_album_fill_off_xml_matches_json() {
    let fixture = Merged::with_fill(false).await;

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-1"))
        .await;
    let json_ids = sorted(ids(&json["subsonic-response"]["album"]["song"]));

    let response = fixture.get(&format!("/rest/getAlbum.view?{AUTH}&id=al-1")).await;
    assert_eq!(response.media_type().as_deref(), Some("application/xml"));
    let xml = XElement::parse(&response.text()).expect("XML");
    let album = child(&xml, "album");
    let xml_song_ids = sorted(xml_ids(album));

    assert_eq!(json_ids, ["s-1", "s-3"]);
    assert_eq!(xml_song_ids, json_ids);
    assert_eq!(album.attribute("songCount"), Some("2"));
    assert!(
        fixture.deezer.calls.lock().is_empty(),
        "the catalog was asked: {:?}",
        fixture.deezer.calls.lock()
    );
}

#[tokio::test]
async fn get_album_fill_off_still_returns_an_external_only_album() {
    let fixture = Merged::with_fill(false).await;
    let id = fixture.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some("Test Artist".into()),
        album: Some("Test Album".into()),
        external_album_id: Some("1".into()),
        ..Default::default()
    });

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id={id}"))
        .await;
    let album = &json["subsonic-response"]["album"];

    assert_eq!(album["name"], "Test Album");
    assert_eq!(names(&album["song"], "title"), ["One", "Two", "Three", "Four"]);
    assert!(
        fixture
            .deezer
            .calls
            .lock()
            .iter()
            .any(|call| call == "/album/1/tracks"),
        "the catalog was not asked: {:?}",
        fixture.deezer.calls.lock()
    );
}

#[tokio::test]
async fn get_album_fill_off_still_consults_the_catalog_for_an_album_with_no_local_songs() {
    let fixture = Merged::with_fill(false).await;

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id=al-ext"))
        .await;
    let album = &json["subsonic-response"]["album"];

    assert!(album.get("song").is_none());
    assert!(
        fixture
            .deezer
            .calls
            .lock()
            .iter()
            .any(|call| call.starts_with("/search/album")),
        "the catalog was not asked: {:?}",
        fixture.deezer.calls.lock()
    );
}

#[tokio::test]
async fn get_album_fill_off_keeps_the_animated_art() {
    let fixture = Merged::with_fill(false).await;
    fixture.state.animated_art.keep(
        "Test Artist",
        "Test Album",
        Some(Clip {
            video: "https://example.invalid/animated/square.m3u8".into(),
            poster: Some("https://example.invalid/animated/square.jpg".into()),
            color: Some("0a0b0c".into()),
            width: 1080,
            height: 1080,
        }),
        None,
    );

    let json = fixture
        .json(&format!("/rest/getAlbum.view?{OCTO_AUTH}&f=json&id=al-1"))
        .await;
    let album = &json["subsonic-response"]["album"];

    assert_eq!(ids(&album["song"]), ["s-1", "s-3"]);
    assert_eq!(
        album["animatedArt"]["square"]["video"],
        "https://example.invalid/animated/square.m3u8"
    );
    assert_eq!(album["animatedArt"]["square"]["color"], "0a0b0c");
    assert!(album["animatedArt"].get("tall").is_none());
    assert!(
        fixture.deezer.calls.lock().is_empty(),
        "the catalog was asked: {:?}",
        fixture.deezer.calls.lock()
    );
}

#[tokio::test]
async fn get_artist_a_library_artist_gains_the_albums_the_library_lacks() {
    let fixture = Merged::new().await;

    let json = fixture
        .json(&format!("/rest/getArtist.view?{AUTH}&f=json&id=ar-1"))
        .await;
    let artist = &json["subsonic-response"]["artist"];
    let albums = artist["album"].as_array().expect("albums").clone();

    // The owned album once, then the outside ones, the album before the single.
    assert_eq!(
        names(&artist["album"], "name"),
        ["Test Album", "Other Album", "A Single"]
    );
    assert_eq!(artist["albumCount"], 3);
    let outside = &albums[1];
    assert_eq!(outside["artist"], "Test Artist");
    // It links back to the library artist, not to an outside copy of them.
    assert_eq!(outside["artistId"], "ar-1");
    assert_eq!(outside["year"], 2005);
    // The listing has no track count; the album's own record fills it in.
    assert_eq!(outside["songCount"], 9);

    // And the outside album opens, as the album rather than the EP of its name.
    let outside_id = outside["id"].as_str().expect("an id").to_string();
    let opened = fixture
        .json(&format!("/rest/getAlbum.view?{AUTH}&f=json&id={outside_id}"))
        .await;
    assert_eq!(opened["subsonic-response"]["album"]["name"], "Other Album");
    let routing = fixture
        .state
        .external_id_registry
        .lookup(&outside_id)
        .expect("registered")
        .snapshot();
    assert_eq!(routing.external_album_id.as_deref(), Some("2"));
}

#[tokio::test]
async fn get_artist_an_outside_artists_page_lists_their_albums() {
    let fixture = Merged::new().await;
    let id = fixture.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some("Test Artist".into()),
        // Tapped in search: this artist, not the better known one of the name.
        external_artist_id: Some("7".into()),
        ..Default::default()
    });

    let json = fixture
        .json(&format!("/rest/getArtist.view?{AUTH}&f=json&id={id}"))
        .await;
    let artist = &json["subsonic-response"]["artist"];

    assert_eq!(
        names(&artist["album"], "name"),
        ["Other Album", "Test Album", "A Single"]
    );
    for album in artist["album"].as_array().expect("albums") {
        assert_eq!(album["artistId"], id.as_str());
    }
}

#[tokio::test]
async fn get_artist_xml_carries_the_same_albums_as_json() {
    let fixture = Merged::new().await;

    let json = fixture
        .json(&format!("/rest/getArtist.view?{AUTH}&f=json&id=ar-1"))
        .await;
    let json_albums = names(&json["subsonic-response"]["artist"]["album"], "name");

    let response = fixture.get(&format!("/rest/getArtist.view?{AUTH}&id=ar-1")).await;
    assert_eq!(response.media_type().as_deref(), Some("application/xml"));
    let xml = XElement::parse(&response.text()).expect("XML");
    let artist = child(&xml, "artist");
    let xml_albums: Vec<String> = children(artist, "album")
        .iter()
        .map(|a| a.attribute("name").unwrap_or_default().to_string())
        .collect();

    assert!(!json_albums.is_empty());
    assert_eq!(json_albums, xml_albums);
    assert_eq!(artist.attribute("name"), Some("Test Artist"));
}

#[tokio::test]
async fn get_artist_every_album_says_what_kind_of_release_it_is() {
    let fixture = Merged::new().await;

    let json = fixture
        .json(&format!("/rest/getArtist.view?{AUTH}&f=json&id=ar-1"))
        .await;
    let types: HashMap<String, Vec<String>> = json["subsonic-response"]["artist"]["album"]
        .as_array()
        .expect("albums")
        .iter()
        .map(|a| {
            (
                a["name"].as_str().unwrap_or_default().to_string(),
                a["releaseTypes"]
                    .as_array()
                    .expect("release types")
                    .iter()
                    .map(|t| t.as_str().unwrap_or_default().to_string())
                    .collect(),
            )
        })
        .collect();

    // The library's album keeps what Navidrome said, word for word.
    assert_eq!(types["Test Album"], ["album", "compilation"]);
    // The outside ones say what the catalog calls them, in the same lowercase words, so
    // one artist's list never mixes "album" and "Album".
    assert_eq!(types["Other Album"], ["album"]);
    assert_eq!(types["A Single"], ["single"]);
    assert!(types.values().flatten().all(|t| *t == t.to_lowercase()));

    let xml = fixture.xml(&format!("/rest/getArtist.view?{AUTH}&id=ar-1")).await;
    let xml_types: HashMap<String, Vec<String>> = children(child(&xml, "artist"), "album")
        .iter()
        .map(|a| {
            (
                a.attribute("name").unwrap_or_default().to_string(),
                children(a, "releaseTypes").iter().map(|e| e.value()).collect(),
            )
        })
        .collect();
    assert_eq!(types, xml_types);
}

/// The answers getSong, getArtist and getAlbum give without the catalog.
#[tokio::test]
async fn missing_and_unknown_ids_answer_without_the_catalog() {
    let fixture = Merged::new().await;

    for endpoint in ["getSong", "getArtist", "getAlbum"] {
        let missing = fixture.json(&format!("/rest/{endpoint}?{AUTH}&f=json")).await;
        assert_eq!(missing["subsonic-response"]["error"]["code"], 10, "{endpoint}");
        assert_eq!(
            missing["subsonic-response"]["error"]["message"], "Missing id parameter",
            "{endpoint}"
        );
    }
    let song = fixture
        .json(&format!("/rest/getSong?{AUTH}&f=json&id=ext-deezer-song-1"))
        .await;
    assert_eq!(song["subsonic-response"]["error"]["message"], "Song not found");
    let playlist = fixture
        .json(&format!("/rest/getAlbum?{AUTH}&f=json&id=pl-deezer-777"))
        .await;
    assert_eq!(
        playlist["subsonic-response"]["error"]["message"],
        "Playlist not found"
    );
    // A local getSong goes to Navidrome, and Navidrome's failure escapes to the global handler.
    let relayed = fixture.get(&format!("/rest/getSong?{AUTH}&f=json&id=nope")).await;
    assert_eq!(relayed.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        relayed.envelope()["error"]["message"],
        "External service unavailable"
    );
}
