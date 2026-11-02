//! Tests of Apple's catalog client against mock Apple hosts, on real time with millisecond paces.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::apple::apple_pace::PaceConfig;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../octo-core/tests/fixtures/apple")
}

fn recorded(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(name)).expect("the recorded answer reads")
}

fn instant_pace() -> Arc<ApplePace> {
    Arc::new(ApplePace::new(PaceConfig {
        spacing: Duration::ZERO,
        cover_spacing: Duration::ZERO,
        bucket_capacity: 1000,
        refill_every: Duration::from_millis(1),
        backoff_max: Duration::from_millis(1),
        answers_to_halve: 1,
        share_turns: 4,
        row_cap: 8,
        background_cap: 32,
    }))
}

struct Learned(Option<&'static str>, Option<&'static str>);

#[async_trait::async_trait]
impl StoreSource for Learned {
    fn pinned(&self) -> Option<String> {
        self.0.map(str::to_string)
    }

    async fn learned(&self) -> Option<String> {
        self.1.map(str::to_string)
    }
}

async fn catalog(server: &MockServer, store: Option<&'static str>, file: Option<PathBuf>) -> AppleCatalog {
    let catalog = AppleCatalog::with_bases(instant_pace(), &server.uri(), &server.uri(), file);
    catalog.set_store_source(Arc::new(Learned(None, store)));
    catalog
}

async fn calls(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap_or_default()
}

fn ok(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/javascript; charset=utf-8")
}

#[tokio::test]
async fn a_search_asks_with_its_term_entity_limit_and_the_learned_store() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ok(recorded("search-album-interstellar-in.json")))
        .mount(&server)
        .await;
    let apple = catalog(&server, Some("IN"), None).await;
    let AppleAsked::Answer(answer) = apple
        .search("interstellar", Entity::Album, 20, AppleClass::Page)
        .await
    else {
        panic!("an answer");
    };
    assert!(answer["resultCount"].as_u64().unwrap() > 0);
    let asked = &calls(&server).await[0];
    let query: std::collections::HashMap<String, String> = asked.url.query_pairs().into_owned().collect();
    assert_eq!(query["term"], "interstellar");
    assert_eq!(query["entity"], "album");
    assert_eq!(query["limit"], "20");
    assert_eq!(query["country"], "in");
}

#[tokio::test]
async fn the_charts_country_pins_the_store_and_nothing_known_is_us() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(r#"{"resultCount":0,"results":[]}"#.to_string()))
        .mount(&server)
        .await;
    let pinned = AppleCatalog::with_bases(instant_pace(), &server.uri(), &server.uri(), None);
    pinned.set_store_source(Arc::new(Learned(Some("GB"), Some("in"))));
    assert_eq!(pinned.store().await, "gb");
    let unknown = AppleCatalog::with_bases(instant_pace(), &server.uri(), &server.uri(), None);
    unknown.set_store_source(Arc::new(Learned(None, None)));
    assert_eq!(unknown.store().await, "us");
}

/// A source whose learned store the test sets as it goes.
struct LearnsLater(Mutex<Option<&'static str>>);

#[async_trait::async_trait]
impl StoreSource for LearnsLater {
    fn pinned(&self) -> Option<String> {
        None
    }

    async fn learned(&self) -> Option<String> {
        self.0.lock().map(str::to_string)
    }
}

#[tokio::test]
async fn nothing_learned_asks_the_last_store_for_now_and_learns_again_later() {
    let folder = tempfile::tempdir().unwrap();
    let file = folder.path().join("apple-store.json");
    std::fs::write(&file, r#"{"store":"in","used":["in"]}"#).unwrap();
    let source = Arc::new(LearnsLater(Mutex::new(None)));
    let apple = AppleCatalog::with_bases(
        instant_pace(),
        "http://127.0.0.1:0",
        "http://127.0.0.1:0",
        Some(file),
    );
    apple.set_store_source(Arc::clone(&source) as Arc<dyn StoreSource>);
    assert_eq!(apple.store().await, "in", "the store chosen last time");
    *source.0.lock() = Some("fr");
    assert_eq!(apple.store().await, "fr", "learned once known");
    *source.0.lock() = Some("de");
    assert_eq!(apple.store().await, "fr", "then chosen for the process");

    let fresh = AppleCatalog::with_bases(instant_pace(), "http://127.0.0.1:0", "http://127.0.0.1:0", None);
    let later = Arc::new(LearnsLater(Mutex::new(None)));
    fresh.set_store_source(Arc::clone(&later) as Arc<dyn StoreSource>);
    assert_eq!(fresh.store().await, "us");
    *later.0.lock() = Some("in");
    assert_eq!(fresh.store().await, "in", "us was never chosen");
}

#[tokio::test]
async fn a_refused_pinned_store_is_asked_once_in_the_process() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("country", "zz"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_raw(recorded("search-bad-country-zz-400.json"), "text/javascript"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("country", "us"))
        .respond_with(ok(recorded("search-album-interstellar-in.json")))
        .mount(&server)
        .await;
    let apple = AppleCatalog::with_bases(instant_pace(), &server.uri(), &server.uri(), None);
    apple.set_store_source(Arc::new(Learned(Some("zz"), None)));
    for term in ["interstellar", "dune"] {
        assert!(matches!(
            apple.search(term, Entity::Album, 5, AppleClass::Page).await,
            AppleAsked::Answer(_)
        ));
    }
    let asked_zz = calls(&server)
        .await
        .iter()
        .filter(|call| call.url.query().is_some_and(|q| q.contains("country=zz")))
        .count();
    assert_eq!(asked_zz, 1);
}

#[tokio::test]
async fn a_store_apple_does_not_have_gives_way_to_us_and_is_not_kept() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("country", "zz"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_raw(recorded("search-bad-country-zz-400.json"), "text/javascript"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("country", "us"))
        .respond_with(ok(recorded("search-album-interstellar-in.json")))
        .mount(&server)
        .await;
    let folder = tempfile::tempdir().unwrap();
    let file = folder.path().join("apple-store.json");
    let apple = catalog(&server, Some("zz"), Some(file.clone())).await;
    assert!(matches!(
        apple
            .search("interstellar", Entity::Album, 5, AppleClass::Page)
            .await,
        AppleAsked::Answer(_)
    ));
    assert_eq!(apple.store().await, "us");
    let kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(kept["store"], serde_json::Value::Null);
    assert_eq!(kept["used"], json!([]));
}

#[tokio::test]
async fn another_bad_request_is_busy_and_keeps_the_store() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400).set_body_raw(
            r#"{"errorMessage":"Invalid value(s) for key(s): [limit]"}"#,
            "text/javascript",
        ))
        .mount(&server)
        .await;
    let apple = catalog(&server, Some("in"), None).await;
    assert_eq!(
        apple.search("x", Entity::Song, 5, AppleClass::Page).await,
        AppleAsked::Busy
    );
    assert_eq!(apple.store().await, "in");
}

#[tokio::test]
async fn an_id_lookup_empty_in_this_store_asks_the_used_stores_then_us() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/lookup"))
        .and(query_param("country", "us"))
        .respond_with(ok(recorded("lookup-1533983552-songs-in.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/lookup"))
        .respond_with(ok(r#"{"resultCount":0,"results":[]}"#.to_string()))
        .mount(&server)
        .await;
    let folder = tempfile::tempdir().unwrap();
    let file = folder.path().join("apple-store.json");
    std::fs::write(&file, r#"{"store":"gb","used":["gb"]}"#).unwrap();
    let apple = catalog(&server, Some("in"), Some(file)).await;
    let answer = apple
        .lookup(&[1_533_983_552], Some(Entity::Song), 300, AppleClass::Page)
        .await;
    assert!(matches!(answer, AppleAsked::Answer(_)));
    let stores: Vec<String> = calls(&server)
        .await
        .iter()
        .map(|request| {
            request
                .url
                .query_pairs()
                .find(|(k, _)| k == "country")
                .unwrap()
                .1
                .into_owned()
        })
        .collect();
    assert_eq!(stores, ["in", "gb", "us"]);
}

#[tokio::test]
async fn a_lookup_nobody_has_is_no_data() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(r#"{"resultCount":0,"results":[]}"#.to_string()))
        .mount(&server)
        .await;
    let apple = catalog(&server, Some("in"), None).await;
    assert_eq!(
        apple.lookup(&[7], None, 1, AppleClass::Page).await,
        AppleAsked::NoData
    );
}

#[tokio::test]
async fn answers_are_kept_and_a_busy_one_is_not() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("term", "kept"))
        .respond_with(ok(recorded("search-album-interstellar-in.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("term", "down"))
        .respond_with(ResponseTemplate::new(503))
        .expect(2)
        .mount(&server)
        .await;
    let apple = catalog(&server, Some("in"), None).await;
    for _ in 0..2 {
        assert!(matches!(
            apple.search("kept", Entity::Album, 5, AppleClass::Page).await,
            AppleAsked::Answer(_)
        ));
        assert_eq!(
            apple.search("down", Entity::Album, 5, AppleClass::Page).await,
            AppleAsked::Busy
        );
    }
}

#[tokio::test]
async fn an_answer_past_the_cache_cap_is_not_kept() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(recorded("lookup-1533983552-songs-in.json")))
        .expect(2)
        .mount(&server)
        .await;
    let apple = catalog(&server, Some("in"), None).await.with_cache_kib(4);
    for _ in 0..2 {
        assert!(matches!(
            apple.search("big", Entity::Song, 50, AppleClass::Page).await,
            AppleAsked::Answer(_)
        ));
    }
}

#[tokio::test]
async fn two_callers_at_once_make_one_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(recorded("search-album-interstellar-in.json")).set_delay(Duration::from_millis(200)))
        .expect(1)
        .mount(&server)
        .await;
    let apple = Arc::new(catalog(&server, Some("in"), None).await);
    let (a, b) = tokio::join!(
        apple.search("same", Entity::Album, 5, AppleClass::Background),
        apple.search("same", Entity::Album, 5, AppleClass::Page),
    );
    assert!(matches!(a, AppleAsked::Answer(_)) && matches!(b, AppleAsked::Answer(_)));
}

#[tokio::test]
async fn a_joiner_raises_the_waiting_request_to_its_class() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(r#"{"resultCount":1,"results":[{"wrapperType":"collection","collectionId":1,"collectionName":"A"}]}"#.to_string()))
        .mount(&server)
        .await;
    let pace = Arc::new(ApplePace::new(PaceConfig {
        spacing: Duration::from_millis(300),
        ..PaceConfig::default()
    }));
    pace.turn(AppleClass::Page).await.unwrap();
    let apple = Arc::new(AppleCatalog::with_bases(
        Arc::clone(&pace),
        &server.uri(),
        &server.uri(),
        None,
    ));
    apple.set_store_source(Arc::new(Learned(None, Some("in"))));
    // A background request waits behind the spacing; a row request queues after it.
    let background = tokio::spawn({
        let apple = Arc::clone(&apple);
        async move {
            apple
                .search("joined", Entity::Album, 5, AppleClass::Background)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let row = tokio::spawn({
        let pace = Arc::clone(&pace);
        async move {
            pace.turn(AppleClass::Row).await.unwrap();
            std::time::Instant::now()
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    // A page joins the background request: it now goes before the row.
    let page = apple.search("joined", Entity::Album, 5, AppleClass::Page).await;
    let joined_at = std::time::Instant::now();
    assert!(matches!(page, AppleAsked::Answer(_)));
    assert!(matches!(background.await.unwrap(), AppleAsked::Answer(_)));
    let row_at = row.await.unwrap();
    assert!(joined_at < row_at, "the joined request went before the row");
}

#[tokio::test]
async fn a_refused_leader_does_not_refuse_its_joiner() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(r#"{"resultCount":1,"results":[{"wrapperType":"collection","collectionId":1,"collectionName":"A"}]}"#.to_string()))
        .mount(&server)
        .await;
    let pace = Arc::new(ApplePace::new(PaceConfig {
        spacing: Duration::from_millis(200),
        row_cap: 1,
        ..PaceConfig::default()
    }));
    pace.turn(AppleClass::Page).await.unwrap();
    let apple = Arc::new(AppleCatalog::with_bases(
        Arc::clone(&pace),
        &server.uri(),
        &server.uri(),
        None,
    ));
    apple.set_store_source(Arc::new(Learned(None, Some("in"))));
    let leader = tokio::spawn({
        let apple = Arc::clone(&apple);
        async move { apple.search("refused", Entity::Album, 5, AppleClass::Row).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let joiner = tokio::spawn({
        let apple = Arc::clone(&apple);
        async move { apple.search("refused", Entity::Album, 5, AppleClass::Page).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    // A newer row request fills the row queue: the oldest row waiter, the leader, is refused.
    let newer = tokio::spawn({
        let apple = Arc::clone(&apple);
        async move { apple.search("newer", Entity::Album, 5, AppleClass::Row).await }
    });
    assert_eq!(leader.await.unwrap(), AppleAsked::Busy);
    assert!(
        matches!(joiner.await.unwrap(), AppleAsked::Answer(_)),
        "the joiner asked again itself"
    );
    assert!(matches!(newer.await.unwrap(), AppleAsked::Answer(_)));
}

#[tokio::test]
async fn a_refused_tracklist_lookup_does_not_refuse_the_page_that_joined_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(recorded("lookup-1533983552-songs-in.json")))
        .mount(&server)
        .await;
    let pace = Arc::new(ApplePace::new(PaceConfig {
        spacing: Duration::from_secs(1),
        background_cap: 1,
        ..PaceConfig::default()
    }));
    pace.turn(AppleClass::Page).await.unwrap();
    let apple = Arc::new(AppleCatalog::with_bases(
        Arc::clone(&pace),
        &server.uri(),
        &server.uri(),
        None,
    ));
    apple.set_store_source(Arc::new(Learned(None, Some("in"))));
    let settle = || async {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    };
    let gathered = tokio::spawn({
        let apple = Arc::clone(&apple);
        async move { apple.tracklists(1_533_983_552, AppleClass::Background).await }
    });
    settle().await;
    let page = tokio::spawn({
        let apple = Arc::clone(&apple);
        async move { apple.tracklists(1_710_154_596, AppleClass::Page).await }
    });
    settle().await;
    // A newer background request fills the background queue: the lookup's own turn, taken as a
    // background request, is refused.
    let _newer = tokio::spawn({
        let apple = Arc::clone(&apple);
        async move {
            apple
                .search("newer", Entity::Album, 5, AppleClass::Background)
                .await
        }
    });
    assert_eq!(gathered.await.unwrap(), AppleAsked::Busy);
    assert!(
        matches!(page.await.unwrap(), AppleAsked::Answer(_)),
        "the page asked alone"
    );
}

#[tokio::test]
async fn the_chart_feed_gives_its_song_ids_in_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/in/music/most-played/10/songs.json"))
        .respond_with(ok(recorded("rss-most-played-in-10.json")))
        .mount(&server)
        .await;
    let apple = catalog(&server, Some("in"), None).await;
    let AppleAsked::Answer(ids) = apple.chart_ids(10, AppleClass::Page).await else {
        panic!("the chart's ids");
    };
    assert_eq!(ids.len(), 10);
    assert_eq!(ids[0], 6_814_002_647);
}

#[tokio::test(flavor = "current_thread")]
async fn an_answer_whose_shape_changed_is_busy_warned_and_not_kept() {
    let (logs, _guard) = crate::services::test_support::LogCapture::start();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(
            r#"{"resultCount":1,"results":[{"kind":"album","id":5}]}"#.to_string()
        ))
        .expect(2)
        .mount(&server)
        .await;
    let apple = catalog(&server, Some("in"), None).await;
    for _ in 0..2 {
        assert_eq!(
            apple.search("changed", Entity::Album, 5, AppleClass::Page).await,
            AppleAsked::Busy
        );
    }
    assert!(
        logs.at(tracing::Level::WARN)
            .iter()
            .any(|line| line.contains("answer has changed shape")),
        "{:?}",
        logs.at(tracing::Level::WARN)
    );
}

#[tokio::test]
async fn apple_refusing_slows_the_pace_and_is_busy() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let pace = Arc::new(ApplePace::new(PaceConfig {
        spacing: Duration::ZERO,
        cover_spacing: Duration::from_millis(5),
        ..PaceConfig::default()
    }));
    let apple = AppleCatalog::with_bases(Arc::clone(&pace), &server.uri(), &server.uri(), None);
    apple.set_store_source(Arc::new(Learned(None, Some("in"))));
    assert_eq!(
        apple.search("x", Entity::Song, 5, AppleClass::Page).await,
        AppleAsked::Busy
    );
    assert_eq!(pace.backoff(), Duration::from_millis(10));
}

#[tokio::test]
async fn tracklists_waiting_together_go_out_as_one_lookup() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/lookup"))
        .respond_with(ok(recorded("lookup-1533983552-songs-in.json")))
        .expect(1)
        .mount(&server)
        .await;
    let pace = Arc::new(ApplePace::new(PaceConfig {
        spacing: Duration::from_millis(150),
        ..PaceConfig::default()
    }));
    pace.turn(AppleClass::Page).await.unwrap();
    let apple = Arc::new(AppleCatalog::with_bases(
        Arc::clone(&pace),
        &server.uri(),
        &server.uri(),
        None,
    ));
    apple.set_store_source(Arc::new(Learned(None, Some("in"))));
    let (first, second) = tokio::join!(
        apple.tracklists(1_533_983_552, AppleClass::Background),
        apple.tracklists(1_710_154_596, AppleClass::Page),
    );
    assert!(matches!(first, AppleAsked::Answer(_)) && matches!(second, AppleAsked::Answer(_)));
    let asked = &calls(&server).await[0];
    let ids = asked
        .url
        .query_pairs()
        .find(|(k, _)| k == "id")
        .unwrap()
        .1
        .into_owned();
    assert_eq!(ids, "1533983552,1710154596");
}

#[tokio::test]
async fn a_tracklist_past_the_lookups_room_starts_the_next_lookup() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/lookup"))
        .respond_with(ok(recorded("lookup-1533983552-songs-in.json")))
        .expect(2)
        .mount(&server)
        .await;
    let pace = Arc::new(ApplePace::new(PaceConfig {
        spacing: Duration::from_millis(150),
        ..PaceConfig::default()
    }));
    pace.turn(AppleClass::Page).await.unwrap();
    let apple = Arc::new(
        AppleCatalog::with_bases(Arc::clone(&pace), &server.uri(), &server.uri(), None).with_batch_albums(1),
    );
    apple.set_store_source(Arc::new(Learned(None, Some("in"))));
    let (first, second) = tokio::join!(
        apple.tracklists(1, AppleClass::Page),
        apple.tracklists(2, AppleClass::Page),
    );
    assert!(matches!(first, AppleAsked::Answer(_)) && matches!(second, AppleAsked::Answer(_)));
}
