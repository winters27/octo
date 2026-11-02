//! Tests of the Apple stand-in at Deezer's door: a mock Deezer answering as it does from India
//! (or as a served country), a mock Apple answering from answers recorded in Apple's Indian store.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use octo_core::metadata::apple_as_deezer::{APPLE_ID_BASE, code_apple_id};
use reqwest::StatusCode;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use octo_core::soulseek::soulseek_metadata_service::SoulseekRouting;

use super::*;
use crate::services::apple::{ApplePace, PaceConfig, StoreSource};
use crate::services::metadata::deezer_reach::{FAILURES_TO_UNREACHABLE, PAGES_PER_REREAD};
use crate::services::metadata::{DeezerRateLimitHandler, DeezerRateLimiter};

const INTERSTELLAR: u64 = 1_533_983_552;
const INTERSTELLAR_TWIN: u64 = 185_320_622;

const INDIA_INFOS: &str = r#"{"country_iso":"IN","country":"India","open":false}"#;
const US_INFOS: &str = r#"{"country_iso":"US","country":"United States of America","open":true}"#;
const HIDDEN_SEARCH: &str = r#"{"data":[],"total":128}"#;
const HIDDEN_LIST: &str = r#"{"data":[],"total":0}"#;
const NO_SUCH: &str = r#"{"error":{"type":"DataException","message":"no data","code":800}}"#;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../octo-core/tests/fixtures/apple")
}

fn recorded(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(name)).expect("the recorded answer reads")
}

fn ok(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.into(), "application/json")
}

struct InStore;

#[async_trait::async_trait]
impl StoreSource for InStore {
    fn pinned(&self) -> Option<String> {
        None
    }

    async fn learned(&self) -> Option<String> {
        Some("in".into())
    }
}

struct World {
    deezer: MockServer,
    apple: MockServer,
    door: Arc<DeezerRateLimitHandler>,
    stand_in: Arc<DeezerStandIn>,
    reach: Arc<DeezerReach>,
    hints: Arc<ArtworkHints>,
    origin: String,
}

impl World {
    async fn new() -> Self {
        Self::on(MockServer::start().await, Duration::from_secs(5)).await
    }

    async fn on(deezer: MockServer, timeout: Duration) -> Self {
        let origin = deezer.uri();
        Self::with_infos_at(deezer, timeout, &origin).await
    }

    /// A world whose `/infos` is read from `infos_origin` (a Deezer that cannot be reached).
    async fn with_infos_at(deezer: MockServer, timeout: Duration, infos_origin: &str) -> Self {
        let apple = MockServer::start().await;
        let pace = Arc::new(ApplePace::new(PaceConfig {
            spacing: Duration::ZERO,
            cover_spacing: Duration::ZERO,
            bucket_capacity: 1000,
            refill_every: Duration::from_millis(1),
            ..PaceConfig::default()
        }));
        let reach = Arc::new(DeezerReach::new(Arc::clone(&pace)));
        let catalog = Arc::new(AppleCatalog::with_bases(pace, &apple.uri(), &apple.uri(), None));
        catalog.set_store_source(Arc::new(InStore));
        let stand_in = Arc::new(DeezerStandIn::new(Arc::clone(&reach), catalog));
        let hints = Arc::new(ArtworkHints::new());
        stand_in.set_hints(Arc::clone(&hints));
        let door = Arc::new(
            DeezerRateLimitHandler::with_api_host(Arc::new(DeezerRateLimiter::new()), "127.0.0.1")
                .with_timeout(timeout),
        );
        stand_in.set_door(Arc::downgrade(&door));
        door.set_stand_in(Arc::clone(&stand_in));
        let origin = deezer.uri();
        reach.set_reader(Arc::new(DoorInfos::new(Arc::downgrade(&door), infos_origin)));
        Self {
            deezer,
            apple,
            door,
            stand_in,
            reach,
            hints,
            origin,
        }
    }

    /// A request to a port on the API host: [`REFUSING_PORT`] fails to connect.
    async fn ask_port(&self, port: u16, path: &str) -> anyhow::Result<HttpAnswer> {
        self.door
            .get_for(&format!("http://127.0.0.1:{port}/{path}"), AskFor::page(), None)
            .await
    }

    /// Deezer unreachable: connecting fails three times in a row.
    async fn unreachable(&self) {
        for _ in 0..FAILURES_TO_UNREACHABLE {
            let _ = self.ask_port(REFUSING_PORT, "album/1").await;
        }
        assert!(matches!(self.reach.reach(), Reach::Unreachable { .. }));
    }

    async fn get(&self, path_and_query: &str) -> (StatusCode, Value) {
        self.get_as(path_and_query, AskFor::page()).await
    }

    async fn get_as(&self, path_and_query: &str, purpose: AskFor) -> (StatusCode, Value) {
        let answer = self
            .door
            .get_for(&format!("{}/{path_and_query}", self.origin), purpose, None)
            .await
            .expect("the door answers");
        let doc = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
        (answer.status, doc)
    }

    async fn deezer_asked(&self, wanted: &str) -> usize {
        self.deezer
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == wanted)
            .count()
    }

    async fn apple_asked(&self) -> usize {
        self.apple.received_requests().await.unwrap_or_default().len()
    }

    /// Deezer as from India: catalog searches hidden, `/infos` closed, the Interstellar barcode
    /// known, every other barcode unknown.
    async fn india(&self) {
        Mock::given(path("/infos"))
            .respond_with(ok(INDIA_INFOS))
            .mount(&self.deezer)
            .await;
        Mock::given(path_regex(r"^/search(/album|/track)?$"))
            .respond_with(ok(HIDDEN_SEARCH))
            .mount(&self.deezer)
            .await;
        Mock::given(path("/album/upc:794043201943"))
            .respond_with(ok(recorded("deezer-album-upc-794043201943.json")))
            .mount(&self.deezer)
            .await;
        Mock::given(path_regex(r"^/album/upc:"))
            .respond_with(ok(NO_SUCH))
            .mount(&self.deezer)
            .await;
    }

    async fn apple_album_search(&self) {
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("entity", "album"))
            .respond_with(ok(recorded("search-album-interstellar-in.json")))
            .mount(&self.apple)
            .await;
    }
}

/// Connecting to port 0 fails at once on every system (a closed port takes Windows about two
/// seconds to refuse, longer than a short test budget).
const REFUSING_PORT: u16 = 0;

/// Apple's tracklist lookup answer for `apple_ids`: each album's collection result from a recorded
/// search, with one song of its own.
fn tracklist_answer(search: &str, apple_ids: &[u64]) -> String {
    let doc: Value = serde_json::from_str(&recorded(search)).unwrap();
    let mut results = Vec::new();
    for collection in doc["results"].as_array().unwrap() {
        let Some(id) = collection["collectionId"]
            .as_u64()
            .filter(|id| apple_ids.contains(id))
        else {
            continue;
        };
        results.push(collection.clone());
        results.push(json!({
            "wrapperType": "track",
            "kind": "song",
            "trackId": id + 1,
            "trackName": "Song",
            "collectionId": id,
            "collectionName": collection["collectionName"],
            "artistName": collection["artistName"],
            "trackNumber": 1,
            "discNumber": 1,
            "trackTimeMillis": 200_000,
        }));
    }
    json!({ "resultCount": results.len(), "results": results }).to_string()
}

/// Apple's own album object for an album in the recorded Interstellar search.
fn apple_album(apple: u64) -> Value {
    let doc: Value = serde_json::from_str(&recorded("search-album-interstellar-in.json")).unwrap();
    let AppleRead::Read(albums) = apple_as_deezer::albums(&doc) else {
        panic!("the recorded search reads");
    };
    albums
        .into_iter()
        .find(|album| album["id"] == json!(code_apple_id(apple)))
        .expect("the album is in the recorded search")
}

fn ids(doc: &Value) -> Vec<u64> {
    doc["data"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|row| row["id"].as_u64()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_hidden_album_search_is_answered_from_apple_once_infos_says_closed() {
    let world = World::new().await;
    world.india().await;
    world.apple_album_search().await;
    let (status, doc) = world.get("search/album?q=interstellar&limit=20").await;
    assert_eq!(status, StatusCode::OK);
    assert!(matches!(world.reach.reach(), Reach::Closed { .. }));
    let found = ids(&doc);
    assert!(
        found.contains(&185_320_622),
        "the soundtrack is Deezer's twin: {found:?}"
    );
    assert!(
        found.iter().any(|id| *id > APPLE_ID_BASE),
        "other albums are Apple's: {found:?}"
    );
    let twin = doc["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == 185_320_622)
        .unwrap();
    assert!(twin.get("tracks").is_none(), "a list carries no tracklist");
}

#[tokio::test]
async fn a_hidden_signature_on_a_served_server_is_a_real_miss() {
    let world = World::new().await;
    Mock::given(path("/infos"))
        .respond_with(ok(US_INFOS))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/search/album"))
        .respond_with(ok(HIDDEN_SEARCH))
        .mount(&world.deezer)
        .await;
    let (status, doc) = world.get("search/album?q=nothing&limit=20").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["total"], 128);
    assert_eq!(doc["data"], json!([]));
    assert!(matches!(world.reach.reach(), Reach::Open { .. }));
    assert_eq!(world.apple_asked().await, 0);
}

#[tokio::test]
async fn a_deep_page_is_never_a_hidden_signature() {
    let world = World::new().await;
    Mock::given(path("/search/album"))
        .respond_with(ok(HIDDEN_SEARCH))
        .mount(&world.deezer)
        .await;
    let (_, doc) = world.get("search/album?q=x&limit=20&index=200").await;
    assert_eq!(doc["total"], 128);
    assert_eq!(world.deezer_asked("/infos").await, 0);
    assert_eq!(world.reach.reach(), Reach::Unknown);
}

#[tokio::test]
async fn while_closed_searches_never_ask_deezer_and_a_served_server_comes_back() {
    let world = World::new().await;
    world.india().await;
    world.apple_album_search().await;
    world.get("search/album?q=interstellar&limit=20").await;
    assert_eq!(world.deezer_asked("/search/album").await, 1);
    world.get("search/album?q=interstellar&limit=20").await;
    assert_eq!(
        world.deezer_asked("/search/album").await,
        1,
        "Closed: Deezer is not asked"
    );
    // Now Deezer serves this server; pages answered from Apple ask /infos again at the
    // twentieth.
    world.deezer.reset().await;
    Mock::given(path("/infos"))
        .respond_with(ok(US_INFOS))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/search/album"))
        .respond_with(ok(r#"{"data":[{"id":5,"title":"A","artist":{"name":"B"},"nb_tracks":9,"record_type":"album"}],"total":1}"#))
        .mount(&world.deezer)
        .await;
    let mut pages = 1;
    while world.reach.stands_in() && pages < PAGES_PER_REREAD + 2 {
        world.get("search/album?q=again&limit=20").await;
        pages += 1;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }
    for _ in 0..100 {
        if !world.reach.stands_in() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(matches!(world.reach.reach(), Reach::Open { .. }));
    assert_eq!(
        world.deezer_asked("/infos").await,
        1,
        "one read, at the twentieth page"
    );
    let (_, doc) = world.get("search/album?q=again&limit=20").await;
    assert_eq!(ids(&doc), [5], "Deezer answers again");
}

#[tokio::test]
async fn artist_search_rows_while_closed_leave_the_reach_closed() {
    let world = World::new().await;
    world.india().await;
    world.reach.read_infos().await;
    Mock::given(path("/search/artist"))
        .respond_with(ok(recorded("deezer-search-artist-hans-zimmer.json")))
        .mount(&world.deezer)
        .await;
    let (_, doc) = world.get("search/artist?q=hans%20zimmer&limit=10").await;
    assert_eq!(ids(&doc)[1], 1935);
    assert!(matches!(world.reach.reach(), Reach::Closed { .. }));
}

#[tokio::test]
async fn an_apple_album_opens_from_its_seed_and_tracklists_waiting_together_share_one_lookup() {
    let world = World::new().await;
    world.india().await;
    world.reach.read_infos().await;
    Mock::given(path("/search"))
        .and(query_param("entity", "album"))
        .respond_with(ok(recorded("search-album-anuv-jain-in.json")))
        .mount(&world.apple)
        .await;
    let (_, doc) = world.get("search/album?q=anuv%20jain&limit=20").await;
    let first = doc["data"][0]["id"].as_u64().unwrap();
    let second = doc["data"][1]["id"].as_u64().unwrap();
    Mock::given(path("/lookup"))
        .respond_with(ok(tracklist_answer(
            "search-album-anuv-jain-in.json",
            &[first - APPLE_ID_BASE, second - APPLE_ID_BASE],
        )))
        .expect(1)
        .mount(&world.apple)
        .await;
    let before = world.apple_asked().await;
    let (status, album) = world.get(&format!("album/{first}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(album["id"], json!(first));
    assert_eq!(
        world.apple_asked().await,
        before,
        "the album object came from the search's seed"
    );
    let (first_tracks, second_tracks) = (
        format!("album/{first}/tracks?limit=300"),
        format!("album/{second}/tracks?limit=300"),
    );
    let (a, b) = tokio::join!(world.get(&first_tracks), world.get(&second_tracks));
    assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
    assert_eq!(
        world.deezer_asked(&format!("/album/{first}")).await,
        0,
        "an Apple id never reaches Deezer"
    );
}

#[tokio::test]
async fn a_barcode_twin_of_another_track_count_keeps_apples_row() {
    let world = World::new().await;
    let mut twin: Value = serde_json::from_str(&recorded("deezer-album-upc-794043201943.json")).unwrap();
    twin["nb_tracks"] = json!(29);
    Mock::given(path("/infos"))
        .respond_with(ok(INDIA_INFOS))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/album/upc:794043201943"))
        .respond_with(ok(twin.to_string()))
        .mount(&world.deezer)
        .await;
    Mock::given(path_regex(r"^/album/upc:"))
        .respond_with(ok(NO_SUCH))
        .mount(&world.deezer)
        .await;
    world.apple_album_search().await;
    world.reach.read_infos().await;
    let (_, doc) = world.get("search/album?q=interstellar&limit=20").await;
    let found = ids(&doc);
    assert!(!found.contains(&185_320_622));
    assert!(found.contains(&code_apple_id(1_533_983_552)));
}

#[tokio::test]
async fn an_unanswered_twin_lookup_keeps_apples_row_marks_the_answer_partial_and_is_asked_again() {
    let world = World::new().await;
    Mock::given(path("/infos"))
        .respond_with(ok(INDIA_INFOS))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/album/upc:794043201943"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&world.deezer)
        .await;
    Mock::given(path("/album/upc:794043201943"))
        .respond_with(ok(recorded("deezer-album-upc-794043201943.json")))
        .mount(&world.deezer)
        .await;
    Mock::given(path_regex(r"^/album/upc:"))
        .respond_with(ok(NO_SUCH))
        .mount(&world.deezer)
        .await;
    world.apple_album_search().await;
    world.reach.read_infos().await;
    let (_, first) = world.get("search/album?q=interstellar&limit=20").await;
    assert_eq!(first["octo_partial"], true);
    assert!(ids(&first).contains(&code_apple_id(1_533_983_552)));
    let (_, second) = world.get("search/album?q=interstellar&limit=20").await;
    assert!(ids(&second).contains(&185_320_622), "asked again, and found");
    let asked = world.deezer_asked("/album/upc:794043201943").await;
    assert_eq!(asked, 2);
}

#[tokio::test]
async fn at_most_eight_twin_lookups_per_answer() {
    let world = World::new().await;
    world.india().await;
    world.apple_album_search().await;
    world.reach.read_infos().await;
    world.get("search/album?q=interstellar&limit=20").await;
    let lookups = world
        .deezer
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path().starts_with("/album/upc:"))
        .map(|request| request.url.path().to_string())
        .collect::<std::collections::HashSet<_>>()
        .len();
    assert!(lookups <= 8 * 2, "{lookups} barcodes asked");
}

#[tokio::test]
async fn one_album_one_id_once_its_twin_is_known() {
    let world = World::new().await;
    world.india().await;
    world.apple_album_search().await;
    world.reach.read_infos().await;
    Mock::given(path("/artist/1935"))
        .respond_with(ok(recorded("deezer-artist-1935.json")))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/search"))
        .and(query_param("entity", "musicArtist"))
        .respond_with(ok(recorded("search-artist-hans-zimmer-in.json")))
        .mount(&world.apple)
        .await;
    Mock::given(path("/lookup"))
        .and(query_param("entity", "album"))
        .respond_with(ok(recorded("lookup-454295032-albums-in.json")))
        .mount(&world.apple)
        .await;
    world.get("search/album?q=interstellar&limit=20").await;
    let (_, page) = world.get("artist/1935/albums?limit=200").await;
    let listed = ids(&page);
    assert!(
        listed.contains(&185_320_622),
        "the artist page carries the known twin"
    );
    assert!(!listed.contains(&code_apple_id(1_533_983_552)));
    // The Apple id still opens, as the twin.
    Mock::given(path("/album/185320622/tracks"))
        .respond_with(ok(recorded("deezer-album-185320622-tracks.json")))
        .mount(&world.deezer)
        .await;
    let (_, tracks) = world
        .get(&format!(
            "album/{}/tracks?limit=300",
            code_apple_id(1_533_983_552)
        ))
        .await;
    assert_eq!(tracks["data"][4]["title"], "Stay");
    assert!(
        tracks["data"][4]["isrc"].as_str().is_some(),
        "Deezer's tracks, with ISRCs"
    );
}

#[tokio::test]
async fn a_deezer_artists_page_is_apples_artist_of_that_name_paged_from_one_answer() {
    let world = World::new().await;
    world.india().await;
    world.reach.read_infos().await;
    Mock::given(path("/artist/1935"))
        .respond_with(ok(recorded("deezer-artist-1935.json")))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/search"))
        .and(query_param("entity", "musicArtist"))
        .respond_with(ok(recorded("search-artist-hans-zimmer-in.json")))
        .mount(&world.apple)
        .await;
    Mock::given(path("/lookup"))
        .and(query_param("entity", "album"))
        .respond_with(ok(recorded("lookup-454295032-albums-in.json")))
        .expect(1)
        .mount(&world.apple)
        .await;
    let (_, first) = world.get("artist/1935/albums?limit=100").await;
    assert_eq!(first["total"], 191);
    assert_eq!(ids(&first).len(), 100);
    let (_, second) = world.get("artist/1935/albums?limit=100&index=100").await;
    assert_eq!(ids(&second).len(), 91);
    assert_eq!(
        world.deezer_asked("/artist/1935/albums").await,
        0,
        "Closed: Deezer is not asked"
    );
}

#[tokio::test]
async fn an_apple_artist_takes_its_picture_from_the_deezer_artist_of_that_name_and_album_count() {
    let world = World::new().await;
    world.india().await;
    world.reach.read_infos().await;
    Mock::given(path("/lookup"))
        .and(query_param("entity", "album"))
        .respond_with(ok(recorded("lookup-454295032-albums-in.json")))
        .mount(&world.apple)
        .await;
    Mock::given(path("/lookup"))
        .respond_with(ok(recorded("search-artist-hans-zimmer-in.json")))
        .mount(&world.apple)
        .await;
    Mock::given(path("/search/artist"))
        .respond_with(ok(recorded("deezer-search-artist-hans-zimmer.json")))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/artist/1935/related"))
        .respond_with(ok(
            r#"{"data":[{"id":84873,"name":"The City Of Prague Philharmonic Orchestra"}],"total":1}"#,
        ))
        .mount(&world.deezer)
        .await;
    let apple = code_apple_id(454_295_032);
    let (_, artist) = world.get(&format!("artist/{apple}")).await;
    assert_eq!(artist["name"], "Hans Zimmer");
    assert!(artist["picture_xl"].as_str().unwrap_or("").contains("dzcdn"));
    assert_eq!(
        artist["nb_fan"].as_i64().unwrap() > 1_000_000,
        true,
        "the followed one, by album count"
    );
    let (_, related) = world.get(&format!("artist/{apple}/related?limit=5")).await;
    assert_eq!(ids(&related), [84873]);
}

#[tokio::test]
async fn an_empty_list_while_unknown_asks_infos_before_it_is_believed() {
    let world = World::new().await;
    Mock::given(path("/artist/1935/top"))
        .respond_with(ok(HIDDEN_LIST))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/infos"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&world.deezer)
        .await;
    let (status, _) = world.get("artist/1935/top?limit=50").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no answer from /infos: never \"nothing\""
    );
    world.deezer.reset().await;
    world.india().await;
    Mock::given(path("/artist/1935/top"))
        .respond_with(ok(HIDDEN_LIST))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/artist/1935"))
        .respond_with(ok(recorded("deezer-artist-1935.json")))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/search"))
        .and(query_param("entity", "musicArtist"))
        .respond_with(ok(recorded("search-artist-hans-zimmer-in.json")))
        .mount(&world.apple)
        .await;
    Mock::given(path("/lookup"))
        .and(query_param("entity", "song"))
        .respond_with(ok(recorded("lookup-1533983552-songs-in.json")))
        .mount(&world.apple)
        .await;
    let (status, doc) = world.get("artist/1935/top?limit=50").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!ids(&doc).is_empty(), "Apple's songs of the artist");
    assert!(matches!(world.reach.reach(), Reach::Closed { .. }));
}

#[tokio::test]
async fn an_unreachable_deezer_is_answered_by_apple_and_its_ids_are_refused_at_once() {
    let gone = format!("http://127.0.0.1:{REFUSING_PORT}");
    // Its /infos cannot be reached either.
    let world = World::with_infos_at(MockServer::start().await, Duration::from_secs(5), &gone).await;
    for _ in 0..3 {
        let answer = world
            .door
            .get_for(&format!("{gone}/album/302127"), AskFor::page(), None)
            .await;
        assert!(answer.is_err());
    }
    assert!(matches!(world.reach.reach(), Reach::Unreachable { .. }));
    world.apple_album_search().await;
    let answer = world
        .door
        .get_for(
            &format!("{gone}/search/album?q=interstellar&limit=20"),
            AskFor::page(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(answer.status, StatusCode::OK);
    for refused in ["album/302127", "track/isrc:USNLR1400773", "artist/1935/albums"] {
        let answer = world
            .door
            .get_for(&format!("{gone}/{refused}"), AskFor::page(), None)
            .await
            .unwrap();
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE, "{refused}");
    }
}

#[tokio::test]
async fn another_hosts_answer_says_nothing_about_deezer() {
    let world = World::new().await;
    let image = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok("{}"))
        .mount(&image)
        .await;
    let other = format!("http://localhost:{}/images/cover.jpg", image.address().port());
    world.door.get_for(&other, AskFor::page(), None).await.unwrap();
    assert_eq!(world.reach.reach(), Reach::Unknown);
}

#[tokio::test]
async fn an_apple_answer_past_the_budget_is_a_timeout() {
    let world = World::on(MockServer::start().await, Duration::from_millis(300)).await;
    world.india().await;
    world.reach.read_infos().await;
    Mock::given(path("/search"))
        .respond_with(ok(recorded("search-album-interstellar-in.json")).set_delay(Duration::from_secs(3)))
        .mount(&world.apple)
        .await;
    let started = std::time::Instant::now();
    let answer = world
        .door
        .get_for(
            &format!("{}/search/album?q=slow&limit=5", world.origin),
            AskFor::page(),
            None,
        )
        .await;
    assert!(answer.is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn send_refuses_the_api_host() {
    let world = World::new().await;
    let request = reqwest::Request::new(
        reqwest::Method::GET,
        format!("{}/infos", world.origin).parse().unwrap(),
    );
    assert!(world.door.send(request, false).await.is_err());
}

#[tokio::test]
async fn on_a_served_server_the_stand_in_never_asks_apple() {
    let world = World::new().await;
    Mock::given(path("/infos"))
        .respond_with(ok(US_INFOS))
        .mount(&world.deezer)
        .await;
    Mock::given(path_regex(r"^/search(/album|/track)?$"))
        .respond_with(ok(
            r#"{"data":[{"id":5,"title":"A","artist":{"id":7,"name":"B"}}],"total":1}"#,
        ))
        .mount(&world.deezer)
        .await;
    Mock::given(path_regex(r"^/(album|artist|chart|track)/"))
        .respond_with(ok(r#"{"data":[],"total":0}"#))
        .mount(&world.deezer)
        .await;
    world.reach.read_infos().await;
    for asked in [
        "search?q=a&limit=5",
        "search/album?q=a&limit=5",
        "search/track?q=a&limit=5",
        "album/302127",
        "album/302127/tracks?limit=300",
        "artist/7/albums?limit=100",
        "artist/7/top?limit=50",
        "chart/0/tracks?limit=50",
        "track/1137370832",
    ] {
        world.get(asked).await;
    }
    assert_eq!(world.apple_asked().await, 0);
    let deezer = world.deezer.received_requests().await.unwrap().len();
    assert_eq!(deezer, 9 + 1, "each request once, and the one /infos read");
}

#[test]
fn routes_follow_the_reach() {
    let pace = Arc::new(ApplePace::new(PaceConfig::default()));
    let reach = Arc::new(DeezerReach::new(Arc::clone(&pace)));
    let catalog = Arc::new(AppleCatalog::new(pace, None));
    let stand_in = DeezerStandIn::new(reach, catalog);
    let route = |path: &str| stand_in.route(&format!("https://api.deezer.com/{path}").parse().unwrap());
    assert!(matches!(
        route("search/album?q=x"),
        Route::Check(Question::Albums { .. })
    ));
    assert!(matches!(
        route("search?q=x&index=5"),
        Route::Check(Question::Tracks { index: 5, .. })
    ));
    assert_eq!(route("search/artist?q=x"), Route::Deezer);
    assert_eq!(route("album/302127"), Route::Deezer);
    assert_eq!(
        route("track/-3135556"),
        Route::Deezer,
        "Deezer's own negative ids stay Deezer's"
    );
    assert_eq!(route("album/upc:794043201943"), Route::Deezer);
    assert!(matches!(
        route(&format!("album/{}", code_apple_id(1_533_983_552))),
        Route::Apple(Question::Album { apple: 1_533_983_552 })
    ));
    assert!(matches!(
        route("artist/1935/albums?limit=100"),
        Route::Check(Question::ArtistAlbums { .. })
    ));
    assert!(matches!(
        route("chart/0/tracks?limit=50"),
        Route::Check(Question::Chart { limit: 50 })
    ));
    assert_eq!(route("infos"), Route::Deezer);
}

#[test]
fn only_a_request_given_its_time_and_never_answered_says_deezer_is_unreachable() {
    use crate::services::metadata::deezer_rate_limit_handler::network_failed;
    let full = DeezerRateLimitHandler::CONNECT_TIMEOUT;
    let short = Duration::from_millis(100);
    // (connect error, timed out, headers seen, time given) -> counts
    for (connect, timed_out, headers, given, counts) in [
        (true, false, false, full, true),
        (false, true, false, full, true),
        (true, true, false, short, false),
        (false, true, false, short, false),
        (false, true, true, full, false),
        (true, false, true, full, false),
        (false, false, false, full, false),
    ] {
        assert_eq!(
            network_failed(connect, timed_out, headers, given),
            counts,
            "{connect} {timed_out} {headers} {given:?}"
        );
    }
}

#[tokio::test]
async fn connect_failures_count_while_budget_cuts_and_stalled_bodies_do_not() {
    use std::io::Write;
    let world = World::on(MockServer::start().await, Duration::from_millis(400)).await;
    let closed = REFUSING_PORT;
    // A listener that never answers: each request is cut by its 400 ms budget, short of the
    // connect timeout.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let silent_port = silent.local_addr().unwrap().port();
    for _ in 0..FAILURES_TO_UNREACHABLE {
        assert!(world.ask_port(silent_port, "album/1").await.is_err());
    }
    assert_eq!(
        world.reach.reach(),
        Reach::Unknown,
        "a budget cut says nothing about Deezer"
    );
    // One connect failure, then headers whose body stops short: Deezer answered, the count starts
    // again. The headers are waited for on a door with time to spare, so a busy machine cannot
    // turn them into a budget cut.
    let _ = world.ask_port(closed, "album/1").await;
    let stalling = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stalling_port = stalling.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut socket, _)) = stalling.accept() {
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{");
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let patient = DeezerRateLimitHandler::with_api_host(Arc::new(DeezerRateLimiter::new()), "127.0.0.1")
        .with_timeout(Duration::from_secs(5));
    patient.set_stand_in(Arc::clone(&world.stand_in));
    assert!(
        patient
            .get_for(
                &format!("http://127.0.0.1:{stalling_port}/album/2"),
                AskFor::page(),
                None
            )
            .await
            .is_err(),
        "the body never came"
    );
    for _ in 0..FAILURES_TO_UNREACHABLE - 1 {
        let _ = world.ask_port(closed, "album/1").await;
    }
    assert_eq!(
        world.reach.reach(),
        Reach::Unknown,
        "the stalled answer reset the count"
    );
    let _ = world.ask_port(closed, "album/1").await;
    assert!(
        matches!(world.reach.reach(), Reach::Unreachable { .. }),
        "connect failures in a row count"
    );
    drop(silent);
}

/// The Interstellar search answered while Closed, its twin found.
async fn interstellar_twin_known(world: &World) {
    world.india().await;
    world.apple_album_search().await;
    world.reach.read_infos().await;
    let (_, doc) = world.get("search/album?q=interstellar&limit=20").await;
    assert!(ids(&doc).contains(&INTERSTELLAR_TWIN));
}

#[tokio::test]
async fn a_twin_row_and_the_album_it_opens_keep_apples_title_and_artist() {
    let world = World::new().await;
    interstellar_twin_known(&world).await;
    let apple = apple_album(INTERSTELLAR);
    let (_, doc) = world.get("search/album?q=interstellar&limit=20").await;
    let row = doc["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!(INTERSTELLAR_TWIN))
        .unwrap();
    assert_eq!(row["title"], apple["title"]);
    assert_eq!(row["artist"]["name"], apple["artist"]["name"]);
    assert_eq!(row["upc"], "794043201943", "the barcode is Deezer's");
    // The Apple id opens as the twin: Deezer's album under Apple's id and names.
    Mock::given(path(format!("/album/{INTERSTELLAR_TWIN}")))
        .respond_with(ok(recorded("deezer-album-upc-794043201943.json")))
        .mount(&world.deezer)
        .await;
    let (status, album) = world.get(&format!("album/{}", code_apple_id(INTERSTELLAR))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(album["id"], json!(code_apple_id(INTERSTELLAR)));
    assert_eq!(album["title"], apple["title"]);
    assert_eq!(album["artist"]["name"], apple["artist"]["name"]);
    assert_eq!(album["upc"], "794043201943");
    assert!(album.get("octo_partial").is_none());
}

#[tokio::test]
async fn an_apple_album_with_no_twin_verdict_looks_its_twin_up_first() {
    let world = World::new().await;
    Mock::given(path("/infos"))
        .respond_with(ok(INDIA_INFOS))
        .mount(&world.deezer)
        .await;
    // The search's lookup goes unanswered: the album is seeded with no verdict.
    Mock::given(path("/album/upc:794043201943"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&world.deezer)
        .await;
    Mock::given(path("/album/upc:794043201943"))
        .respond_with(ok(recorded("deezer-album-upc-794043201943.json")))
        .mount(&world.deezer)
        .await;
    Mock::given(path_regex(r"^/album/upc:"))
        .respond_with(ok(NO_SUCH))
        .mount(&world.deezer)
        .await;
    Mock::given(path(format!("/album/{INTERSTELLAR_TWIN}")))
        .respond_with(ok(recorded("deezer-album-upc-794043201943.json")))
        .mount(&world.deezer)
        .await;
    world.apple_album_search().await;
    world.reach.read_infos().await;
    let (_, search) = world.get("search/album?q=interstellar&limit=20").await;
    assert_eq!(search["octo_partial"], true);
    let (_, album) = world.get(&format!("album/{}", code_apple_id(INTERSTELLAR))).await;
    assert_eq!(
        album["upc"], "794043201943",
        "answered from the twin it looked up"
    );
    assert!(album.get("octo_partial").is_none());
    assert_eq!(world.deezer_asked("/album/upc:794043201943").await, 2);
}

#[tokio::test]
async fn an_apple_album_whose_twin_lookup_goes_unanswered_is_apples_and_partial() {
    let world = World::new().await;
    Mock::given(path("/infos"))
        .respond_with(ok(INDIA_INFOS))
        .mount(&world.deezer)
        .await;
    Mock::given(path_regex(r"^/album/upc:"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&world.deezer)
        .await;
    world.apple_album_search().await;
    world.reach.read_infos().await;
    world.get("search/album?q=interstellar&limit=20").await;
    let (status, album) = world.get(&format!("album/{}", code_apple_id(INTERSTELLAR))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(album["id"], json!(code_apple_id(INTERSTELLAR)));
    assert_eq!(
        album["octo_partial"], true,
        "kept only briefly, so the twin is asked again soon"
    );
}

#[tokio::test]
async fn a_slow_twin_lookup_never_holds_the_page_and_still_fills_the_map() {
    let world = World::new().await;
    Mock::given(path("/infos"))
        .respond_with(ok(INDIA_INFOS))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/album/upc:794043201943"))
        .respond_with(ok(recorded("deezer-album-upc-794043201943.json")).set_delay(Duration::from_secs(3)))
        .mount(&world.deezer)
        .await;
    Mock::given(path_regex(r"^/album/upc:"))
        .respond_with(ok(NO_SUCH))
        .mount(&world.deezer)
        .await;
    world.apple_album_search().await;
    world.reach.read_infos().await;
    let started = std::time::Instant::now();
    let (status, first) = world.get("search/album?q=interstellar&limit=20").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        started.elapsed() < Duration::from_millis(2_800),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(first["octo_partial"], true);
    assert!(ids(&first).contains(&code_apple_id(INTERSTELLAR)));
    // The lookup finishes on its own and records the twin.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let (_, second) = world.get("search/album?q=interstellar&limit=20").await;
    assert!(ids(&second).contains(&INTERSTELLAR_TWIN));
    assert_eq!(world.deezer_asked("/album/upc:794043201943").await, 1);
}

#[tokio::test]
async fn while_unreachable_a_twin_album_is_answered_from_apple_under_deezers_id() {
    let world = World::new().await;
    interstellar_twin_known(&world).await;
    Mock::given(path("/lookup"))
        .and(query_param("entity", "song"))
        .respond_with(ok(recorded("lookup-1533983552-songs-in.json")))
        .mount(&world.apple)
        .await;
    world.unreachable().await;
    // Asked as rows, so no page asks Deezer's /infos again (this mock Deezer still answers it).
    let (status, album) = world
        .get_as(&format!("album/{INTERSTELLAR_TWIN}"), AskFor::row())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(album["id"], json!(INTERSTELLAR_TWIN));
    assert_eq!(album["title"], apple_album(INTERSTELLAR)["title"]);
    assert_eq!(album["octo_partial"], true);
    let (status, tracks) = world
        .get_as(
            &format!("album/{INTERSTELLAR_TWIN}/tracks?limit=300"),
            AskFor::row(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!ids(&tracks).is_empty());
    assert_eq!(
        tracks["data"][0]["album"]["id"],
        json!(INTERSTELLAR_TWIN),
        "its songs name the twin"
    );
    let (status, _) = world.get_as("album/302127", AskFor::row()).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a Deezer album with no twin waits for Deezer"
    );
    assert_eq!(
        world.deezer_asked(&format!("/album/{INTERSTELLAR_TWIN}")).await,
        0
    );
}

fn service(world: &World) -> crate::services::metadata::DeezerMetadataService {
    crate::services::metadata::DeezerMetadataService::with_base_url(
        Arc::clone(&world.door),
        Arc::new(octo_core::settings::SettingsStore::for_tests(
            octo_core::settings::AppSettings::default(),
        )),
        &world.origin,
    )
}

async fn closed_with_song_search(world: &World) {
    world.india().await;
    world.reach.read_infos().await;
    Mock::given(path("/search"))
        .and(query_param("entity", "song"))
        .respond_with(ok(recorded("search-song-hans-zimmer-stay-in.json")))
        .mount(&world.apple)
        .await;
}

#[tokio::test]
async fn a_song_lookup_miss_while_apple_stands_in_asks_only_its_searches() {
    let world = World::new().await;
    closed_with_song_search(&world).await;
    let deezer = service(&world);
    let before = world.deezer.received_requests().await.unwrap().len();
    let found = deezer.enrich_track("Nobody", "Not A Song", false, false).await;
    assert!(found.is_none());
    assert!(
        world.apple_asked().await <= 3,
        "{} Apple requests",
        world.apple_asked().await
    );
    let deezer_after = world.deezer.received_requests().await.unwrap().len();
    assert_eq!(deezer_after, before, "no album or top-songs step asks Deezer");
}

#[tokio::test]
async fn the_querys_own_song_search_fills_the_rows_it_lists() {
    let world = World::new().await;
    closed_with_song_search(&world).await;
    let deezer = service(&world);
    let rows = deezer
        .search_tracks("hans zimmer stay", 60)
        .await
        .expect("Apple answers");
    assert!(
        rows.iter()
            .any(|(artist, title)| artist == "Hans Zimmer" && title == "Stay")
    );
    let before = world.apple_asked().await;
    let meta = deezer
        .cached_track("Hans Zimmer", "Stay")
        .expect("the row is known without a lookup");
    assert!(
        meta.album_title
            .as_deref()
            .unwrap_or("")
            .starts_with("Interstellar")
    );
    assert!(
        meta.explicit_content.is_some(),
        "the explicit word came with the search"
    );
    assert_eq!(world.apple_asked().await, before);
}

#[tokio::test]
async fn a_lookup_apple_does_not_answer_is_no_answer_and_kept_nowhere() {
    let world = World::new().await;
    world.india().await;
    world.reach.read_infos().await;
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&world.apple)
        .await;
    let deezer = service(&world);
    let answer = deezer.enrich_track_quick_answered("Hans Zimmer", "Stay").await;
    assert_eq!(
        answer,
        crate::services::metadata::deezer_metadata_service::TrackAnswer::NoAnswer
    );
    assert!(deezer.cached_track("Hans Zimmer", "Stay").is_none());
}

#[tokio::test]
async fn after_a_catalog_switch_nothing_kept_under_the_old_catalog_is_read() {
    let world = World::new().await;
    world.india().await;
    world.apple_album_search().await;
    world.reach.read_infos().await;
    let deezer = service(&world);
    let apple_hits = deezer.search_albums("interstellar", 20, false).await;
    assert!(
        apple_hits
            .iter()
            .any(|hit| hit.deezer_id == code_apple_id(INTERSTELLAR).to_string()
                || hit.deezer_id == INTERSTELLAR_TWIN.to_string())
    );
    // Deezer serves this server now: rows from a catalog search switch the catalog.
    let generation = world.reach.generation();
    world.reach.observe_rows();
    assert_eq!(world.reach.generation(), generation + 1);
    world.deezer.reset().await;
    Mock::given(path("/search/album"))
        .respond_with(ok(r#"{"data":[{"id":5,"title":"Interstellar","artist":{"name":"Hans Zimmer"},"nb_tracks":16,"record_type":"album"}],"total":1}"#))
        .mount(&world.deezer)
        .await;
    let deezer_hits = deezer.search_albums("interstellar", 20, false).await;
    assert_eq!(
        deezer_hits
            .iter()
            .map(|hit| hit.deezer_id.as_str())
            .collect::<Vec<_>>(),
        ["5"],
        "asked again, not read from the Apple answer"
    );
}

#[tokio::test]
async fn songs_and_albums_handed_out_leave_their_apple_covers_as_hints() {
    let world = World::new().await;
    world.india().await;
    world.reach.read_infos().await;
    Mock::given(path("/search"))
        .and(query_param("entity", "song"))
        .respond_with(ok(recorded("search-song-hans-zimmer-stay-in.json")))
        .mount(&world.apple)
        .await;
    let (_, doc) = world.get("search?q=hans%20zimmer%20stay&limit=10").await;
    let first = &doc["data"][0];
    let routing = |kind: RoutingKind, album: Option<&Value>| SoulseekRouting {
        kind,
        artist: first["artist"]["name"].as_str().map(str::to_string),
        title: first["title"].as_str().map(str::to_string),
        album: album.and_then(Value::as_str).map(str::to_string),
        ..Default::default()
    };
    let cover = first["album"]["cover_xl"].as_str().unwrap();
    assert!(cover.contains("mzstatic.com"), "{cover}");
    assert_eq!(
        world
            .hints
            .for_routing(&routing(RoutingKind::Song, None))
            .as_deref(),
        Some(cover)
    );
    assert_eq!(
        world
            .hints
            .for_routing(&routing(RoutingKind::Album, Some(&first["album"]["title"])))
            .as_deref(),
        Some(cover)
    );
}
