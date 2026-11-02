//! A station refresh that gets no answer for some of a listener's stations keeps those stations
//! as they were and tries them again on their own; a station Last.fm answered with too little
//! goes. Driven through the refresh worker against a stand-in Last.fm.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::Clock;
use octo_core::last_fm::last_fm_radio_recommendation_service::{definition_version, key};
use octo_core::models::radio::{
    LastFmRadioPlay, LastFmRadioStation, LastFmRadioStationKind, LastFmRadioTrack,
};
use octo_core::radio::radio_provider::{LISTEN_BRAINZ, YOU_TUBE_MUSIC};
use octo_core::radio::{RadioAnswer, RadioSeed};
use octo_core::settings::{AppSettings, DiscoveryStationSettings, LastFmSettings, SettingsStore};
use parking_lot::Mutex;
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::services::last_fm::LastFmService;
use crate::services::last_fm::last_fm_radio_recommendation_service::LastFmRadioRecommendationService;
use crate::services::last_fm::last_fm_radio_refresh_queue::{LastFmRadioRefreshJob, LastFmRadioRefreshQueue};
use crate::services::last_fm::last_fm_radio_refresh_worker::LastFmRadioRefreshWorker;
use crate::services::last_fm::last_fm_radio_state_store::{LastFmRadioStateStore, station_id};
use crate::services::radio::{IRadioSource, RadioSourceSet};
use crate::services::soulseek::ExternalIdRegistry;

type Query = HashMap<String, String>;
type Rule = Arc<dyn Fn(&Query) -> Option<ResponseTemplate> + Send + Sync>;

/// Last.fm with good lists for every method, except what `rule` answers.
#[derive(Clone)]
struct LastFm(Rule);

impl Respond for LastFm {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query: Query = request.url.query_pairs().into_owned().collect();
        (self.0)(&query).unwrap_or_else(|| healthy(&query))
    }
}

fn last_fm(rule: impl Fn(&Query) -> Option<ResponseTemplate> + Send + Sync + 'static) -> LastFm {
    LastFm(Arc::new(rule))
}

fn method(query: &Query) -> &str {
    query.get("method").map(String::as_str).unwrap_or_default()
}

fn healthy(query: &Query) -> ResponseTemplate {
    let get = |name: &str| query.get(name).cloned().unwrap_or_default();
    let body = match method(query) {
        "artist.gettoptags" => json!({"toptags": {"tag": [
            {"name": "electronica"}, {"name": "seen live"}, {"name": "rock"}
        ]}}),
        "track.getsimilar" => json!({"similartracks": {"track": (0..15).map(|index| json!({
            "name": format!("similar-{index}"), "match": 1.0 - f64::from(index) / 100.0,
            "duration": 180000, "artist": {"name": format!("Similar Artist {index}")}
        })).collect::<Vec<_>>()}}),
        "artist.getsimilar" => json!({"similarartists": {"artist": (0..6).map(|index| json!({
            "name": format!("Neighbor {index}"), "match": 0.9 - f64::from(index) / 10.0
        })).collect::<Vec<_>>()}}),
        "artist.gettoptracks" => json!({"toptracks": {"track": (0..12).map(|index| json!({
            "name": format!("{}-top-{index}", get("artist"))
        })).collect::<Vec<_>>()}}),
        "tag.gettoptracks" => json!({"tracks": {"track": (0..20).map(|index| json!({
            "name": format!("{}-{index}", get("tag")), "duration": 180000,
            "artist": {"name": format!("{} Artist {index}", get("tag"))}
        })).collect::<Vec<_>>()}}),
        _ => json!({}),
    };
    ResponseTemplate::new(200).set_body_string(body.to_string())
}

fn status(code: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(code).set_body_string(body.to_string())
}

/// Similar songs answer (so Your Radio is made); every other method gets `answer`.
fn only_similar_songs(answer: ResponseTemplate) -> LastFm {
    last_fm(move |query| (method(query) != "track.getsimilar").then(|| answer.clone()))
}

/// Discovery Radio's lookups (the walk's similar artists and the tag charts) get `answer`.
fn discovery_lookups(answer: ResponseTemplate) -> LastFm {
    last_fm(move |query| {
        matches!(method(query), "artist.getsimilar" | "tag.gettoptracks").then(|| answer.clone())
    })
}

/// A radio source other than Last.fm that fails, or answers with nothing.
struct Source {
    provider: &'static str,
    fails: bool,
}

#[async_trait]
impl IRadioSource for Source {
    fn provider(&self) -> &'static str {
        self.provider
    }

    fn available(&self) -> bool {
        true
    }

    async fn songs_like(
        &self,
        _seed: &RadioSeed,
        _count: i32,
        _auth: &IndexMap<String, String>,
    ) -> anyhow::Result<RadioAnswer> {
        if self.fails {
            anyhow::bail!("down");
        }
        Ok(RadioAnswer::nothing(self.provider))
    }

    async fn tag(&self, _tag: &str, _count: i32) -> anyhow::Result<RadioAnswer> {
        if self.fails {
            anyhow::bail!("down");
        }
        Ok(RadioAnswer::nothing(self.provider))
    }
}

struct Fixture {
    _dir: TempDir,
    server: MockServer,
    path: PathBuf,
    settings: Arc<SettingsStore>,
    registry: Arc<ExternalIdRegistry>,
    now: Arc<Mutex<DateTime<Utc>>>,
    base_url: String,
    state: Arc<LastFmRadioStateStore>,
    queue: Arc<LastFmRadioRefreshQueue>,
    worker: Arc<LastFmRadioRefreshWorker>,
}

/// The store, queue and worker on the file at `path`, asking Last.fm at `base_url`.
fn parts(
    path: &Path,
    settings: &Arc<SettingsStore>,
    registry: &Arc<ExternalIdRegistry>,
    now: &Arc<Mutex<DateTime<Utc>>>,
    base_url: &str,
    sources: Vec<Arc<dyn IRadioSource>>,
) -> (
    Arc<LastFmRadioStateStore>,
    Arc<LastFmRadioRefreshQueue>,
    Arc<LastFmRadioRefreshWorker>,
) {
    let read = now.clone();
    let clock = Clock::new(move || *read.lock());
    let state = Arc::new(LastFmRadioStateStore::with_clock(
        path.to_path_buf(),
        settings.clone(),
        registry.clone(),
        clock.clone(),
    ));
    let client = Arc::new(LastFmService::with_base_url(settings.clone(), base_url));
    let sources =
        (!sources.is_empty()).then(|| Arc::new(RadioSourceSet::new(sources, settings.clone(), None)));
    let recommendations = Arc::new(LastFmRadioRecommendationService::new(
        client,
        state.clone(),
        settings.clone(),
        sources,
    ));
    let queue = Arc::new(LastFmRadioRefreshQueue::new());
    let worker = Arc::new(
        LastFmRadioRefreshWorker::new(
            queue.clone(),
            recommendations,
            state.clone(),
            settings.clone(),
            None,
        )
        .with_clock(clock),
    );
    (state, queue, worker)
}

impl Fixture {
    async fn new(settings: LastFmSettings, answers: LastFm) -> Self {
        Self::build(settings, answers, Vec::new(), None).await
    }

    async fn build(
        settings: LastFmSettings,
        answers: LastFm,
        sources: Vec<Arc<dyn IRadioSource>>,
        base_url: Option<String>,
    ) -> Self {
        let dir = TempDir::new().unwrap();
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(answers).mount(&server).await;
        let base_url = base_url.unwrap_or_else(|| format!("{}/", server.uri()));
        let path = dir.path().join("state.json");
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            last_fm: settings,
            ..Default::default()
        }));
        let registry = Arc::new(ExternalIdRegistry::in_memory());
        let now = Arc::new(Mutex::new(Utc::now()));
        let (state, queue, worker) = parts(&path, &settings, &registry, &now, &base_url, sources);
        Fixture {
            _dir: dir,
            server,
            path,
            settings,
            registry,
            now,
            base_url,
            state,
            queue,
            worker,
        }
    }

    /// Last.fm answers `answers` from now on.
    async fn answer(&self, answers: LastFm) {
        self.server.reset().await;
        Mock::given(any()).respond_with(answers).mount(&self.server).await;
    }

    fn later(&self, minutes: i64) {
        *self.now.lock() += TimeDelta::minutes(minutes);
    }

    async fn refresh(&self) -> anyhow::Result<bool> {
        self.worker.process(&LastFmRadioRefreshJob::new("alice")).await
    }

    /// The next queued job, run.
    async fn run_queued(&self) -> anyhow::Result<bool> {
        let job = tokio::time::timeout(
            Duration::from_secs(1),
            self.queue.dequeue(&CancellationToken::new()),
        )
        .await
        .expect("a job within a second")
        .expect("a job");
        self.worker.process(&job).await
    }

    fn keys(&self) -> Vec<String> {
        self.state
            .get_user("alice")
            .stations
            .into_iter()
            .map(|station| station.key)
            .collect()
    }

    fn station(&self, key: &str) -> LastFmRadioStation {
        self.state
            .get_user("alice")
            .stations
            .into_iter()
            .find(|station| station.key == key)
            .unwrap_or_else(|| panic!("{key} is saved"))
    }
}

fn learned() -> LastFmSettings {
    LastFmSettings {
        api_key: "key".into(),
        minimum_plays: 3,
        ..Default::default()
    }
}

/// Artist A listened to most lately, then B, then C; every play is rock.
fn record_listening(state: &LastFmRadioStateStore) {
    for (artist, hours) in [
        ("Artist A", 1),
        ("Artist A", 2),
        ("Artist A", 3),
        ("Artist B", 48),
        ("Artist B", 49),
        ("Artist B", 50),
        ("Artist C", 120),
        ("Artist C", 121),
    ] {
        assert!(state.record_play(
            "alice",
            LastFmRadioPlay {
                artist: artist.into(),
                title: format!("{artist} song {hours}"),
                genre: Some("rock".into()),
                played_at_utc: Utc::now() - TimeDelta::hours(hours),
                ..Default::default()
            },
        ));
    }
}

fn artist_station(name: &str) -> String {
    format!("artist-{}", key(name))
}

fn genre_station(tag: &str) -> String {
    format!("genre-{}", key(tag))
}

/// A station saved by an earlier refresh: twelve songs by `by`.
fn saved(key: &str, name: &str, kind: LastFmRadioStationKind, by: &str) -> LastFmRadioStation {
    let then = Utc::now() - TimeDelta::hours(13);
    LastFmRadioStation {
        id: station_id("alice", key),
        key: key.into(),
        name: name.into(),
        owner: "alice".into(),
        kind,
        personalized: kind != LastFmRadioStationKind::Pinned,
        definition_version: 1,
        created_utc: then,
        changed_utc: then,
        valid_until_utc: then + TimeDelta::hours(12),
        tracks: (0..12)
            .map(|index| LastFmRadioTrack {
                artist: by.into(),
                title: format!("{by} {index}"),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn your_radio() -> LastFmRadioStation {
    saved(
        "your-mix",
        "Your Radio",
        LastFmRadioStationKind::YourMix,
        "Old Mix",
    )
}

fn discovery() -> LastFmRadioStation {
    saved(
        "discovery",
        "Discovery Radio",
        LastFmRadioStationKind::Discovery,
        "Old Discovery",
    )
}

/// Every kind of station the listener has before the refresh.
fn everything_saved() -> Vec<LastFmRadioStation> {
    vec![
        your_radio(),
        discovery(),
        saved(
            &artist_station("Artist A"),
            "Artist A Radio",
            LastFmRadioStationKind::Artist,
            "Old A",
        ),
        saved(
            &artist_station("Artist C"),
            "Artist C Radio",
            LastFmRadioStationKind::Artist,
            "Old C",
        ),
        saved(
            &genre_station("rock"),
            "Rock Radio",
            LastFmRadioStationKind::Genre,
            "Old Rock",
        ),
        saved(
            &genre_station("jazz"),
            "Jazz Radio",
            LastFmRadioStationKind::Genre,
            "Old Jazz",
        ),
    ]
}

fn all_by(station: &LastFmRadioStation, artist: &str) -> bool {
    !station.tracks.is_empty() && station.tracks.iter().all(|track| track.artist == artist)
}

fn none_by(station: &LastFmRadioStation, artist: &str) -> bool {
    !station.tracks.is_empty() && station.tracks.iter().all(|track| track.artist != artist)
}

/// The listener of the first case, after a refresh where only similar songs answered.
async fn carried_after_an_outage() -> Fixture {
    let fixture = Fixture::new(learned(), only_similar_songs(status(500, ""))).await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &everything_saved());
    assert!(fixture.refresh().await.unwrap());
    fixture
}

#[tokio::test]
async fn a_refresh_last_fm_does_not_answer_keeps_the_stations_it_could_not_make() {
    let fixture = carried_after_an_outage().await;

    assert_eq!(
        fixture.keys(),
        [
            "your-mix".to_string(),
            "discovery".to_string(),
            artist_station("Artist A"),
            genre_station("rock"),
            genre_station("jazz"),
        ],
        "Artist C left the top artists, so its radio goes"
    );
    assert!(
        none_by(&fixture.station("your-mix"), "Old Mix"),
        "Your Radio was answered, so it is new"
    );
    let discovery = fixture.station("discovery");
    assert!(all_by(&discovery, "Old Discovery"));
    assert_eq!(discovery.tracks.len(), 12);
    assert!(discovery.valid_until_utc > Utc::now());
    assert!(all_by(&fixture.station(&artist_station("Artist A")), "Old A"));
    assert!(all_by(&fixture.station(&genre_station("rock")), "Old Rock"));
    assert!(fixture.state.get_user("alice").last_refresh_error.is_none());
}

#[tokio::test]
async fn a_switched_off_station_is_never_kept() {
    let fixture = Fixture::new(
        LastFmSettings {
            enable_discovery_mix: false,
            ..learned()
        },
        only_similar_songs(status(500, "")),
    )
    .await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &everything_saved());
    assert!(fixture.refresh().await.unwrap());
    assert!(!fixture.keys().contains(&"discovery".to_string()));
}

#[tokio::test]
async fn stations_last_fm_answered_with_too_little_go() {
    let fixture = Fixture::new(learned(), only_similar_songs(status(200, "{}"))).await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &everything_saved());
    assert!(fixture.refresh().await.unwrap());
    assert_eq!(fixture.keys(), ["your-mix"]);
    fixture.later(16);
    assert_eq!(
        fixture.worker.queue_due(),
        0,
        "nothing was kept, so nothing is tried again"
    );
}

#[tokio::test]
async fn with_no_key_no_last_fm_station_is_kept() {
    let fixture = Fixture::new(
        LastFmSettings {
            api_key: String::new(),
            ..learned()
        },
        last_fm(|_| Some(status(500, ""))),
    )
    .await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &everything_saved());
    assert!(fixture.refresh().await.unwrap());
    assert_eq!(fixture.keys(), ["your-mix"]);
    assert!(none_by(&fixture.station("your-mix"), "Old Mix"));
}

#[tokio::test]
async fn kept_stations_are_tried_again_and_replaced_once_last_fm_answers() {
    let fixture = carried_after_an_outage().await;
    let mix = fixture.station("your-mix");
    let succeeded = fixture.state.get_user("alice").last_refresh_success_utc;
    fixture.answer(last_fm(|_| None)).await;
    fixture.later(14);
    assert_eq!(fixture.worker.queue_due(), 0, "not before a quarter of an hour");
    fixture.later(2);
    assert_eq!(fixture.worker.queue_due(), 1);
    assert!(fixture.run_queued().await.unwrap());

    // Jazz was only standing in for a genre the ranking could not name; answered, it goes.
    assert_eq!(
        fixture.keys(),
        [
            "your-mix".to_string(),
            "discovery".to_string(),
            artist_station("Artist A"),
            genre_station("rock"),
        ]
    );
    let untouched = fixture.station("your-mix");
    let titles = |station: &LastFmRadioStation| -> Vec<String> {
        station.tracks.iter().map(|track| track.title.clone()).collect()
    };
    assert_eq!(
        titles(&untouched),
        titles(&mix),
        "a station made before is not drawn again"
    );
    assert_eq!(untouched.changed_utc, mix.changed_utc);
    assert_eq!(
        fixture.state.get_user("alice").last_refresh_success_utc,
        succeeded
    );
    let discovery = fixture.station("discovery");
    assert!(none_by(&discovery, "Old Discovery"));
    for track in discovery.tracks.iter().filter(|track| !track.is_local) {
        let id = track.resolved_id.as_deref().expect("a route id");
        assert!(fixture.registry.lookup(id).is_some(), "{} plays", track.title);
    }
    assert!(none_by(&fixture.station(&artist_station("Artist A")), "Old A"));
    assert!(none_by(&fixture.station(&genre_station("rock")), "Old Rock"));
    fixture.later(16);
    assert_eq!(fixture.worker.queue_due(), 0, "nothing is kept any more");
}

#[tokio::test]
async fn kept_stations_stay_kept_while_last_fm_still_does_not_answer() {
    let fixture = carried_after_an_outage().await;
    let before = fixture.keys();
    fixture.later(16);
    assert_eq!(fixture.worker.queue_due(), 1);
    assert!(fixture.run_queued().await.unwrap());
    assert_eq!(fixture.keys(), before);
    assert!(all_by(&fixture.station("discovery"), "Old Discovery"));
    fixture.later(16);
    assert_eq!(
        fixture.worker.queue_due(),
        1,
        "tried again while it is still kept"
    );
}

#[tokio::test]
async fn a_rate_limit_in_a_success_is_never_kept_as_an_answer() {
    let fixture = Fixture::new(learned(), only_similar_songs(status(200, r#"{"error":29}"#))).await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &everything_saved());
    assert!(fixture.refresh().await.unwrap());
    fixture.later(16);
    assert_eq!(fixture.worker.queue_due(), 1);
    assert!(fixture.run_queued().await.unwrap());
    assert!(
        all_by(&fixture.station("discovery"), "Old Discovery"),
        "the first answer was not kept to be read again as an empty one"
    );
}

#[tokio::test]
async fn a_refresh_that_could_make_nothing_keeps_everything() {
    let fixture = Fixture::new(learned(), last_fm(|_| Some(status(500, "")))).await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &everything_saved());
    let error = fixture.refresh().await.expect_err("nothing was made");
    assert_eq!(
        error.to_string(),
        "Provider returned no usable replacement stations"
    );
    assert_eq!(fixture.keys().len(), 6);
    assert!(all_by(&fixture.station("discovery"), "Old Discovery"));
}

#[tokio::test]
async fn a_listener_asked_for_nothing_keeps_no_station() {
    let fixture = Fixture::new(
        LastFmSettings {
            enable_your_mix: false,
            ..learned()
        },
        last_fm(|_| None),
    )
    .await;
    assert!(fixture.state.record_play(
        "alice",
        LastFmRadioPlay {
            artist: "Artist A".into(),
            title: "Only".into(),
            ..Default::default()
        },
    ));
    fixture.state.replace_stations("alice", &[discovery()]);
    assert!(
        fixture.refresh().await.unwrap(),
        "radio has not learned enough for any station"
    );
    assert!(fixture.keys().is_empty());
    assert!(fixture.state.get_user("alice").last_refresh_error.is_none());
}

#[tokio::test]
async fn a_history_reset_during_a_retry_keeps_nothing() {
    let fixture = carried_after_an_outage().await;
    fixture
        .answer(last_fm(|query| {
            Some(healthy(query).set_delay(Duration::from_millis(300)))
        }))
        .await;
    fixture.later(16);
    assert_eq!(fixture.worker.queue_due(), 1);
    let job = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.queue.dequeue(&CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    let worker = fixture.worker.clone();
    let retry = tokio::spawn(async move { worker.process(&job).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(fixture.state.reset("alice"));
    let _ = retry.await.unwrap();
    assert!(fixture.keys().is_empty());
    fixture.later(16);
    assert_eq!(fixture.worker.queue_due(), 0);
}

#[tokio::test]
async fn switching_every_kind_off_stops_the_retries() {
    let fixture = carried_after_an_outage().await;
    fixture.settings.set(AppSettings {
        last_fm: LastFmSettings {
            enable_personalized_stations: false,
            enable_discovery_stations: false,
            ..learned()
        },
        ..Default::default()
    });
    assert!(!fixture.refresh().await.unwrap());
    fixture.settings.set(AppSettings {
        last_fm: learned(),
        ..Default::default()
    });
    fixture.later(16);
    assert_eq!(fixture.worker.queue_due(), 0);
}

/// The listener has Discovery Radio only; `answers` answer its lookups.
async fn discovery_after(answers: LastFm, base_url: Option<String>) -> Fixture {
    let fixture = Fixture::build(learned(), answers, Vec::new(), base_url).await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &[discovery()]);
    assert!(fixture.refresh().await.unwrap());
    fixture
}

fn kept_discovery(fixture: &Fixture) -> bool {
    fixture
        .state
        .get_user("alice")
        .stations
        .iter()
        .any(|station| station.key == "discovery" && all_by(station, "Old Discovery"))
}

#[tokio::test]
async fn a_rate_limit_keeps_discovery_radio() {
    let fixture = discovery_after(discovery_lookups(status(429, "")), None).await;
    assert!(kept_discovery(&fixture));
}

#[tokio::test]
async fn no_connection_keeps_discovery_radio() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}/", closed.local_addr().unwrap());
    drop(closed);
    let fixture = discovery_after(last_fm(|_| None), Some(base_url)).await;
    assert!(kept_discovery(&fixture));
}

#[tokio::test]
async fn a_page_that_is_not_last_fm_keeps_discovery_radio() {
    for answer in [
        status(200, "<html>busy</html>"),
        status(403, "<html>blocked</html>"),
        status(408, ""),
    ] {
        let fixture = discovery_after(discovery_lookups(answer), None).await;
        assert!(kept_discovery(&fixture));
    }
}

#[tokio::test]
async fn last_fm_saying_try_again_keeps_discovery_radio() {
    for answer in [
        status(400, r#"{"error":16}"#),
        status(200, r#"{"error":29}"#),
        status(200, r#"{"error":8}"#),
    ] {
        let fixture = discovery_after(discovery_lookups(answer), None).await;
        assert!(kept_discovery(&fixture));
    }
}

#[tokio::test]
async fn last_fm_answering_with_an_error_of_its_own_drops_discovery_radio() {
    for answer in [
        status(
            400,
            r#"{"error":6,"message":"The artist you supplied could not be found"}"#,
        ),
        status(403, r#"{"error":10,"message":"Invalid API key"}"#),
    ] {
        let fixture = discovery_after(discovery_lookups(answer), None).await;
        assert!(!fixture.keys().contains(&"discovery".to_string()));
    }
}

#[tokio::test]
async fn a_walk_cut_off_by_its_time_keeps_discovery_radio() {
    // The walk's similar artists never come in time; the tag charts answer with nothing.
    let fixture = discovery_after(
        last_fm(|query| match method(query) {
            "artist.getsimilar" if query.get("limit").map(String::as_str) == Some("30") => {
                Some(status(200, "{}").set_delay(Duration::from_secs(30)))
            }
            "tag.gettoptracks" => Some(status(200, "{}")),
            _ => None,
        }),
        None,
    )
    .await;
    assert!(kept_discovery(&fixture));
}

#[tokio::test]
async fn answers_kept_from_before_are_answers() {
    let fixture = Fixture::new(learned(), last_fm(|_| None)).await;
    record_listening(&fixture.state);
    assert!(fixture.refresh().await.unwrap());
    let made = fixture.keys();
    fixture.answer(last_fm(|_| Some(status(500, "")))).await;
    assert!(fixture.refresh().await.unwrap());
    assert_eq!(fixture.keys(), made);
    fixture.later(16);
    assert_eq!(
        fixture.worker.queue_due(),
        0,
        "every station was made from kept answers"
    );
}

#[tokio::test]
async fn your_radio_is_kept_only_when_no_source_answered() {
    let failing: Arc<dyn IRadioSource> = Arc::new(Source {
        provider: LISTEN_BRAINZ,
        fails: true,
    });
    // Artist A's own lists answer with nothing.
    let thin_artist = || {
        last_fm(|query| {
            (matches!(method(query), "artist.gettoptracks")
                || (method(query) == "artist.getsimilar"
                    && query.get("limit").map(String::as_str) == Some("15")))
            .then(|| status(200, "{}"))
        })
    };
    let fixture = Fixture::build(learned(), thin_artist(), vec![failing.clone()], None).await;
    record_listening(&fixture.state);
    fixture.state.replace_stations(
        "alice",
        &[
            your_radio(),
            saved(
                &artist_station("Artist A"),
                "Artist A Radio",
                LastFmRadioStationKind::Artist,
                "Old A",
            ),
        ],
    );
    assert!(fixture.refresh().await.unwrap());
    assert!(
        all_by(&fixture.station("your-mix"), "Old Mix"),
        "no source answered, so it stays"
    );
    assert!(
        !fixture.keys().contains(&artist_station("Artist A")),
        "Last.fm answered for Artist A with too little; another source failing changes nothing"
    );

    let answering: Arc<dyn IRadioSource> = Arc::new(Source {
        provider: YOU_TUBE_MUSIC,
        fails: false,
    });
    let fixture = Fixture::build(learned(), last_fm(|_| None), vec![failing, answering], None).await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &[your_radio()]);
    assert!(fixture.refresh().await.unwrap());
    assert!(
        none_by(&fixture.station("your-mix"), "Old Mix"),
        "a source answered with nothing: the station is the listener's own plays"
    );
}

fn rock_definition(name: &str) -> DiscoveryStationSettings {
    DiscoveryStationSettings {
        id: "rock".into(),
        name: name.into(),
        tags: vec!["rock".into()],
        ..Default::default()
    }
}

fn pop_definition() -> DiscoveryStationSettings {
    DiscoveryStationSettings {
        id: "pop".into(),
        name: "Pop".into(),
        tags: vec!["pop".into()],
        ..Default::default()
    }
}

/// Only pinned stations, with the rock tag chart failing.
async fn pinned_after(name: &str) -> Fixture {
    let fixture = Fixture::new(
        LastFmSettings {
            api_key: "key".into(),
            enable_personalized_stations: false,
            discovery_stations: vec![rock_definition(name), pop_definition()],
            ..Default::default()
        },
        last_fm(|query| {
            (method(query) == "tag.gettoptracks" && query.get("tag").map(String::as_str) == Some("rock"))
                .then(|| status(500, ""))
        }),
    )
    .await;
    record_listening(&fixture.state);
    let mut rock = saved(
        "pinned-rock",
        "Rock",
        LastFmRadioStationKind::Pinned,
        "Old Pinned",
    );
    rock.definition_version = definition_version(&rock_definition("Rock"));
    fixture.state.replace_stations("alice", &[rock]);
    assert!(fixture.refresh().await.unwrap());
    fixture
}

#[tokio::test]
async fn a_pinned_station_whose_chart_did_not_answer_is_kept_unless_it_was_edited() {
    let fixture = pinned_after("Rock").await;
    assert!(all_by(&fixture.station("pinned-rock"), "Old Pinned"));

    let fixture = pinned_after("Rock & Roll").await;
    let edited = fixture.station("pinned-rock");
    assert_eq!(edited.name, "Rock & Roll");
    assert!(
        none_by(&edited, "Old Pinned"),
        "an edited station is made from the listener's plays"
    );
}

#[tokio::test]
async fn an_answered_genre_ranking_keeps_only_the_genres_it_names() {
    let fixture = Fixture::new(
        learned(),
        last_fm(|query| {
            (method(query) == "tag.gettoptracks" && query.get("tag").map(String::as_str) == Some("rock"))
                .then(|| status(500, ""))
        }),
    )
    .await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &everything_saved());
    assert!(fixture.refresh().await.unwrap());
    assert!(all_by(&fixture.station(&genre_station("rock")), "Old Rock"));
    assert!(!fixture.keys().contains(&genre_station("jazz")));
}

#[tokio::test]
async fn a_pinned_refresh_leaves_other_kept_stations_kept() {
    let fixture = Fixture::new(
        LastFmSettings {
            discovery_stations: vec![rock_definition("Rock")],
            ..learned()
        },
        only_similar_songs(status(500, "")),
    )
    .await;
    record_listening(&fixture.state);
    fixture.state.replace_stations("alice", &everything_saved());
    assert!(fixture.refresh().await.unwrap());
    fixture.answer(last_fm(|_| None)).await;
    assert!(fixture.queue.enqueue("alice", Some("rock")));
    assert!(fixture.run_queued().await.unwrap());
    assert!(all_by(&fixture.station("discovery"), "Old Discovery"));
    fixture.later(16);
    assert_eq!(
        fixture.worker.queue_due(),
        1,
        "Discovery Radio is still tried again"
    );
}

#[tokio::test]
async fn kept_stations_are_tried_again_after_a_restart() {
    let fixture = carried_after_an_outage().await;
    let (_, queue, worker) = parts(
        &fixture.path,
        &fixture.settings,
        &fixture.registry,
        &fixture.now,
        &fixture.base_url,
        Vec::new(),
    );
    fixture.later(16);
    assert_eq!(worker.queue_due(), 1);
    let job = tokio::time::timeout(Duration::from_secs(1), queue.dequeue(&CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(worker.process(&job).await.unwrap());
}
