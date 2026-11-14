use std::time::Duration;

use chrono::TimeZone;
use octo_core::settings::{AppSettings, GeneratedPlaylistSettings, SubsonicSettings};
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::services::charts::apple_music_pages::AppleMusicPages;
use crate::services::charts::{ChartCountryStore, ListenerCharts};
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::cover_art::ArtworkHints;
use crate::services::metadata::deezer_metadata_service::DeezerMetadataService;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/apple-music/",
            $name,
            ".html"
        ))
    };
}

const ALICE: &str = "u=alice&t=good&s=salt&v=1.16.1&c=test";

struct Lists {
    service: ChartPlaylistService,
    top_songs: TopSongsService,
    settings: Arc<SettingsStore>,
    navidrome: MockServer,
    _apple: MockServer,
    _deezer: Option<MockServer>,
    now: Arc<Mutex<DateTime<Utc>>>,
    listeners: Arc<ChartCountryStore>,
    matches: Arc<LibraryMatchCache>,
}

fn settings_for(navidrome: &str, charts: &str, discovery: bool, filter: ExplicitFilter) -> AppSettings {
    AppSettings {
        subsonic: SubsonicSettings {
            url: Some(navidrome.to_string()),
            enable_search_discovery: discovery,
            explicit_filter: filter,
            ..Default::default()
        },
        generated_playlists: GeneratedPlaylistSettings {
            charts: charts.to_string(),
            chart_country: "us".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

impl Lists {
    async fn new(charts: &str) -> Lists {
        Self::with(charts, true, ExplicitFilter::All).await
    }

    async fn with(charts: &str, discovery: bool, filter: ExplicitFilter) -> Lists {
        Self::build(charts, discovery, filter, false).await
    }

    /// Lists that remember what the library lacks between builds.
    async fn remembering(charts: &str) -> Lists {
        Self::build(charts, true, ExplicitFilter::All, true).await
    }

    /// Lists whose Deezer answers its chart with `chart`.
    async fn with_deezer(charts: &str, chart: &str) -> Lists {
        Self::build_with(charts, true, ExplicitFilter::All, false, Some(chart)).await
    }

    async fn build(charts: &str, discovery: bool, filter: ExplicitFilter, remembers: bool) -> Lists {
        Self::build_with(charts, discovery, filter, remembers, None).await
    }

    async fn build_with(
        charts: &str,
        discovery: bool,
        filter: ExplicitFilter,
        remembers: bool,
        deezer_chart: Option<&str>,
    ) -> Lists {
        let deezer_server = match deezer_chart {
            Some(body) => Some(
                crate::services::test_support::Routes::new(
                    vec![("/chart/0/tracks", vec![body])],
                    crate::services::test_support::Matching::IgnoreCase,
                )
                .serve()
                .await,
            ),
            None => None,
        };
        let navidrome = MockServer::builder().start().await;
        let apple = MockServer::start().await;
        for (at, genre, body) in [
            ("/us/new/top-charts/songs", None, fixture!("us_all")),
            ("/us/new/top-charts/songs/", Some("18"), fixture!("us_18")),
            ("/us/new", None, fixture!("us_new")),
            ("/us/room/6818396128", None, fixture!("us_room_best_new")),
            ("/us/room/6818395867", None, fixture!("us_room_trending")),
        ] {
            let mock = Mock::given(method("GET")).and(path(at));
            let mock = match genre {
                Some(genre) => mock.and(query_param("genreId", genre)),
                None => mock.and(query_param_is_missing("genreId")),
            };
            mock.respond_with(ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/html"))
                .mount(&apple)
                .await;
        }
        let settings = Arc::new(SettingsStore::for_tests(settings_for(
            &navidrome.uri(),
            charts,
            discovery,
            filter,
        )));
        let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
        let deezer = DeezerMetadataService::with_base_url(
            http,
            Arc::clone(&settings),
            deezer_server.as_ref().map_or_else(
                || format!("http://{}", crate::services::test_support::refused_address()),
                MockServer::uri,
            ),
        );
        let registry = Arc::new(ExternalIdRegistry::in_memory());
        let listeners = Arc::new(ChartCountryStore::new(None));
        let top_songs = TopSongsService::new(
            deezer,
            None,
            Arc::new(FakeMetadata::default()),
            Arc::clone(&registry),
            Arc::new(AppleMusicPages::with_base_url(&apple.uri(), Duration::ZERO)),
            Arc::new(ArtworkHints::new()),
            Arc::clone(&settings),
        )
        .with_listeners(Arc::clone(&listeners));
        let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap()));
        let clock_now = Arc::clone(&now);
        let service = ChartPlaylistService::new(
            top_songs.clone(),
            Arc::new(
                crate::services::subsonic::subsonic_response_builder::new_subsonic_response_builder(
                    Arc::clone(&registry),
                    &SubsonicSettings::default(),
                ),
            ),
            registry,
            SubsonicProxyService::new(Arc::clone(&settings)),
            Arc::clone(&settings),
            Clock::new(move || *clock_now.lock()),
        );
        let matches = Arc::new(LibraryMatchCache::new());
        let service = if remembers {
            service.with_library_matches(Arc::clone(&matches))
        } else {
            service
        };
        Lists {
            service,
            top_songs,
            settings,
            navidrome,
            _apple: apple,
            _deezer: deezer_server,
            now,
            listeners,
            matches,
        }
    }

    /// Apple's pages for another country: Japan's own overall chart for `jp`, the US pages
    /// under that country's storefront for the rest. `genre_down`: its genre chart fails.
    async fn pages_for(&self, code: &str, genre_down: bool) {
        let mine = |body: &str| body.replace("\"storefront\":\"us\"", &format!("\"storefront\":\"{code}\""));
        let overall = if code == "jp" {
            fixture!("jp_all").to_string()
        } else {
            mine(fixture!("us_all"))
        };
        for (at, body) in [
            (format!("/{code}/new/top-charts/songs"), overall),
            (format!("/{code}/new"), mine(fixture!("us_new"))),
            (
                format!("/{code}/room/6818396128"),
                mine(fixture!("us_room_best_new")),
            ),
            (
                format!("/{code}/room/6818395867"),
                mine(fixture!("us_room_trending")),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(at))
                .and(query_param_is_missing("genreId"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/html"))
                .mount(&self._apple)
                .await;
        }
        let genre = Mock::given(method("GET"))
            .and(path(format!("/{code}/new/top-charts/songs/")))
            .and(query_param("genreId", "18"));
        let answer = if genre_down {
            ResponseTemplate::new(503)
        } else {
            ResponseTemplate::new(200).set_body_raw(mine(fixture!("us_18")).into_bytes(), "text/html")
        };
        genre.respond_with(answer).mount(&self._apple).await;
    }

    fn choose(&self, user: &str, country: &str, pinned: &[&str]) {
        self.listeners.set(
            user,
            ListenerCharts {
                country: country.to_string(),
                pinned: pinned.iter().map(|c| (*c).to_string()).collect(),
                ..Default::default()
            },
        );
    }

    async fn searches(&self) -> usize {
        self.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/rest/search3")
            .count()
    }

    fn advance_minutes(&self, minutes: i64) {
        let mut now = self.now.lock();
        *now += TimeDelta::minutes(minutes);
    }

    /// Navidrome's search3: the songs for a query, or nothing for any other query.
    async fn library(&self, answers: Vec<(&str, Value)>) {
        let answers: HashMap<String, Value> = answers.into_iter().map(|(q, v)| (q.to_string(), v)).collect();
        Mock::given(method("GET"))
            .and(path("/rest/search3"))
            .respond_with(move |request: &Request| {
                let query = request
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "query")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let songs = answers.get(&query).cloned().unwrap_or(json!([]));
                ResponseTemplate::new(200).set_body_json(json!({
                    "subsonic-response": {"status": "ok", "version": "1.16.1", "searchResult3": {"song": songs}}
                }))
            })
            .mount(&self.navidrome)
            .await;
    }

    async fn searches_for(&self, query: &str) -> usize {
        self.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.query_pairs().any(|(k, v)| k == "query" && v == query))
            .count()
    }

    fn advance(&self, hours: i64) {
        let mut now = self.now.lock();
        *now += TimeDelta::hours(hours);
    }
}

fn auth() -> Parameters {
    ALICE
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn every_list_on_is_a_playlist_overall_first() {
    let lists = Lists::new("new,18,trending").await;
    lists.library(Vec::new()).await;
    lists.service.warm("alice", &auth()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;

    let listed = lists.service.list("alice", &auth()).await;

    let names: Vec<&str> = listed.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "Popular right now",
            "Best New Songs",
            "Top Hip-Hop/Rap",
            "Trending Songs"
        ]
    );
    assert!(
        listed
            .iter()
            .all(|p| p.kind == "chart" && p.owner == "alice" && p.id.starts_with("og"))
    );
    assert_eq!(listed[0].pool_size, 5);
}

#[tokio::test]
async fn the_overall_keeps_its_old_id_and_each_listener_has_their_own() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;

    let listed = lists.service.list("alice", &auth()).await;

    assert_eq!(listed[0].id, playlist_id("alice", "chart:popular"));
    assert_ne!(
        playlist_id("alice", "chart:popular"),
        playlist_id("bob", "chart:popular")
    );
    assert_eq!(
        lists.service.which("alice", &listed[0].id).as_deref(),
        Some(OVERALL)
    );
    assert_eq!(lists.service.which("bob", &listed[0].id), None);
}

#[tokio::test]
async fn library_songs_are_the_listeners_own_the_rest_outside() {
    let lists = Lists::new("").await;
    lists
        .library(vec![("Drake Quebec", json!([{"id": "nd-quebec", "artist": "Drake", "title": "Quebec", "starred": "2026-10-01T00:00:00Z"}]))])
        .await;

    let songs = lists.service.songs("alice", OVERALL, &auth()).await;

    assert_eq!(songs.len(), 5);
    assert_eq!(songs[1]["id"], "nd-quebec");
    assert_eq!(songs[1]["starred"], "2026-10-01T00:00:00Z");
    assert_eq!(songs[0]["title"], "Solar Eclipse");
    assert_ne!(songs[0]["id"], "nd-quebec");
    let outside = lists.service.outside_songs("alice", OVERALL);
    assert_eq!(outside.len(), 4);
    assert!(outside.iter().all(|s| !s.is_local));
}

#[tokio::test]
async fn a_song_on_two_lists_is_searched_once() {
    let lists = Lists::new("new").await;
    lists.library(Vec::new()).await;

    lists.service.songs("alice", OVERALL, &auth()).await;
    lists.service.songs("alice", "new", &auth()).await;

    // Solar Eclipse leads both Popular right now and Best New Songs.
    assert_eq!(lists.searches_for("Drake Solar Eclipse").await, 1);
}

#[tokio::test]
async fn a_build_skips_a_song_the_library_lacked_until_the_library_changes() {
    let lists = Lists::remembering("").await;
    lists.library(Vec::new()).await;
    let scan_status = std::sync::Arc::new(Mutex::new("2026-10-01T00:00:00Z".to_string()));
    let answered = std::sync::Arc::clone(&scan_status);
    Mock::given(method("GET"))
        .and(path("/rest/getScanStatus"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200).set_body_json(json!({
                "subsonic-response": {"status": "ok", "scanStatus": {"lastScan": *answered.lock()}}
            }))
        })
        .mount(&lists.navidrome)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/getMusicFolders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "subsonic-response": {"status": "ok", "musicFolders": {"musicFolder": [{"id": 1, "name": "Music"}]}}
        })))
        .mount(&lists.navidrome)
        .await;

    lists.service.songs("alice", OVERALL, &auth()).await;
    let first = lists.searches().await;
    lists.advance(7);
    lists.service.songs("alice", OVERALL, &auth()).await;
    assert_eq!(lists.searches().await, first, "nothing in the library changed");

    *scan_status.lock() = "2026-10-02T00:00:00Z".into();
    lists.advance(7);
    lists.service.songs("alice", OVERALL, &auth()).await;
    assert_eq!(lists.searches().await, first * 2, "a new scan asks again");
}

#[tokio::test]
async fn library_searches_carry_only_the_sign_in() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    let mut asked = auth();
    asked.insert("offset".into(), "500".into());

    lists.service.songs("alice", OVERALL, &asked).await;

    let request = &lists.navidrome.received_requests().await.unwrap()[0];
    let keys: Vec<String> = request.url.query_pairs().map(|(k, _)| k.into_owned()).collect();
    assert!(!keys.contains(&"offset".to_string()));
    assert!(keys.contains(&"t".to_string()) && keys.contains(&"query".to_string()));
}

#[tokio::test]
async fn with_search_discovery_off_only_the_listeners_own_songs() {
    let lists = Lists::with("", false, ExplicitFilter::All).await;
    lists
        .library(vec![(
            "Drake Quebec",
            json!([{"id": "nd-quebec", "artist": "Drake", "title": "Quebec"}]),
        )])
        .await;

    let songs = lists.service.songs("alice", OVERALL, &auth()).await;

    assert_eq!(songs.len(), 1);
    assert_eq!(songs[0]["id"], "nd-quebec");
}

#[tokio::test]
async fn the_explicit_filter_leaves_out_what_it_should() {
    let lists = Lists::with("", true, ExplicitFilter::CleanOnly).await;
    lists.library(Vec::new()).await;

    let songs = lists.service.songs("alice", OVERALL, &auth()).await;

    // Solar Eclipse and Quebec carry Apple's explicit badge.
    let titles: Vec<&str> = songs.iter().filter_map(|s| s["title"].as_str()).collect();
    assert_eq!(titles, vec!["Cold Shoulder", "Choosin' Texas", "BbY WOW"]);
}

#[tokio::test]
async fn a_changed_explicit_filter_makes_the_list_again_on_the_next_ask() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    assert_eq!(lists.service.songs("alice", OVERALL, &auth()).await.len(), 5);

    let mut changed = (*lists.settings.current()).clone();
    changed.subsonic.explicit_filter = ExplicitFilter::CleanOnly;
    lists.settings.set(changed);

    let songs = lists.service.songs("alice", OVERALL, &auth()).await;
    let titles: Vec<&str> = songs.iter().filter_map(|s| s["title"].as_str()).collect();
    assert_eq!(titles, vec!["Cold Shoulder", "Choosin' Texas", "BbY WOW"]);
}

#[tokio::test]
async fn a_list_turned_off_is_gone_on_the_next_list() {
    let lists = Lists::new("18").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", "18", &auth()).await;
    assert_eq!(lists.service.list("alice", &auth()).await.len(), 2);

    let mut changed = (*lists.settings.current()).clone();
    changed.generated_playlists.charts = String::new();
    lists.settings.set(changed);

    let listed = lists.service.list("alice", &auth()).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Popular right now");
}

#[tokio::test]
async fn a_genre_the_country_lacks_is_not_listed() {
    let lists = Lists::new("99").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;

    let listed = lists.service.list("alice", &auth()).await;

    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn a_list_is_listed_once_its_chart_is_known_while_discovery_is_on() {
    let lists = Lists::new("").await;
    // Navidrome is slow: the list cannot be made within the first look's wait.
    Mock::given(method("GET"))
        .and(path("/rest/search3"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&lists.navidrome)
        .await;
    lists.top_songs.chart(OVERALL).await;

    let listed = lists.service.list("alice", &auth()).await;

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].pool_size, 5);
}

#[tokio::test]
async fn with_discovery_off_a_list_waits_until_made() {
    let lists = Lists::with("", false, ExplicitFilter::All).await;
    Mock::given(method("GET"))
        .and(path("/rest/search3"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&lists.navidrome)
        .await;
    lists.top_songs.chart(OVERALL).await;

    assert!(lists.service.list("alice", &auth()).await.is_empty());
}

#[tokio::test]
async fn a_library_that_stops_answering_leaves_the_last_list_alone() {
    let lists = Lists::new("").await;
    lists
        .library(vec![(
            "Drake Quebec",
            json!([{"id": "nd-quebec", "artist": "Drake", "title": "Quebec"}]),
        )])
        .await;
    let first = lists.service.songs("alice", OVERALL, &auth()).await;

    lists.navidrome.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&lists.navidrome)
        .await;
    lists.advance(REFRESH_HOURS.into());
    let second = lists.service.songs("alice", OVERALL, &auth()).await;

    assert_eq!(first, second);
}

#[tokio::test]
async fn the_same_songs_keep_the_change_time() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;
    let first = lists.service.as_sync_stations("alice")[0].changed_utc;

    lists.advance(REFRESH_HOURS.into());
    lists.service.songs("alice", OVERALL, &auth()).await;
    let station = &lists.service.as_sync_stations("alice")[0];

    assert_eq!(station.changed_utc, first);
    assert!(station.created_utc > first);
}

#[tokio::test]
async fn every_list_rides_into_the_sync_catalog_under_its_own_ids() {
    let lists = Lists::new("new").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;

    let stations = lists.service.as_sync_stations("alice");

    assert_eq!(stations.len(), 2);
    assert_eq!(stations[0].id, playlist_id("alice", "chart:popular"));
    assert_eq!(stations[1].name, "Best New Songs");
    let outside = lists.service.outside_songs("alice", OVERALL);
    let track = &stations[0].tracks[0];
    assert_eq!(track.source, TRACK_SOURCE);
    assert_eq!(track.resolved_id.as_deref(), Some(outside[0].id.as_str()));
    assert_eq!(track.artist, "Drake");
}

#[tokio::test]
async fn keep_playable_makes_a_forgotten_outside_song_known_again_under_the_same_id() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    let songs = lists.service.songs("alice", OVERALL, &auth()).await;
    let outside = lists.service.outside_songs("alice", OVERALL);

    let fresh = Arc::new(ExternalIdRegistry::in_memory());
    let forgetful = ChartPlaylistService::new(
        lists.top_songs.clone(),
        Arc::new(
            crate::services::subsonic::subsonic_response_builder::new_subsonic_response_builder(
                Arc::clone(&fresh),
                &SubsonicSettings::default(),
            ),
        ),
        Arc::clone(&fresh),
        SubsonicProxyService::new(Arc::clone(&lists.settings)),
        Arc::clone(&lists.settings),
        Clock::system(),
    );
    forgetful.keep_playable(&outside);

    assert!(fresh.lookup(&outside[0].id).is_some());
    assert_eq!(songs[0]["id"], outside[0].id.as_str());
}

#[test]
fn assemble_keeps_chart_order_the_librarys_copy_first() {
    let row = |title: &str, explicit: Option<i32>| TopSong {
        rank: 1,
        song: Song {
            id: format!("out-{title}"),
            title: title.into(),
            artist: "A".into(),
            explicit_content_lyrics: explicit,
            ..Default::default()
        },
        plays: None,
        listeners: None,
    };
    let rows = vec![
        row("One", Some(1)),
        row("Two", Some(3)),
        row("Three", Some(4)),
        row("Four", None),
    ];
    let library = vec![None, Some(json!({"id": "nd-two"})), None, None];
    let as_json = |s: &Song| json!({"id": s.id});

    let (all, _) = assemble(&rows, &library, true, ExplicitFilter::All, false, 10, as_json);
    let ids: Vec<&str> = all.iter().filter_map(|e| e["id"].as_str()).collect();
    assert_eq!(ids, vec!["out-One", "nd-two", "out-Three", "out-Four"]);

    let (clean, outside) = assemble(
        &rows,
        &library,
        true,
        ExplicitFilter::CleanOnly,
        false,
        10,
        as_json,
    );
    let ids: Vec<&str> = clean.iter().filter_map(|e| e["id"].as_str()).collect();
    assert_eq!(
        ids,
        vec!["nd-two", "out-Four"],
        "4 counts as explicit; library songs are never filtered"
    );
    assert_eq!(outside.len(), 1);

    let (none_outside, _) = assemble(&rows, &library, false, ExplicitFilter::All, false, 10, as_json);
    assert_eq!(none_outside.len(), 1);
}

#[test]
fn assemble_leaves_out_outside_instrumentals_while_skip_covers_and_instrumentals_is_on() {
    let row = |title: &str, album: &str| TopSong {
        rank: 1,
        song: Song {
            id: format!("out-{title}"),
            title: title.into(),
            artist: "A".into(),
            album: album.into(),
            ..Default::default()
        },
        plays: None,
        listeners: None,
    };
    let rows = vec![
        row("One", "Album"),
        row("Two (Instrumental)", "Album"),
        row("Three (Karaoke Version)", "Album"),
        row("Four", "Album (Instrumentals)"),
        row("Karaoke Queen", "Album"),
    ];
    let library = vec![None, None, Some(json!({"id": "nd-three"})), None, None];
    let as_json = |s: &Song| json!({"id": s.id});

    let (skipped, outside) = assemble(&rows, &library, true, ExplicitFilter::All, true, 10, as_json);
    let ids: Vec<&str> = skipped.iter().filter_map(|e| e["id"].as_str()).collect();
    assert_eq!(
        ids,
        vec!["out-One", "nd-three", "out-Karaoke Queen"],
        "the library's own copy stays; a name is not a take"
    );
    assert_eq!(outside.len(), 2);

    let (all, _) = assemble(&rows, &library, true, ExplicitFilter::All, false, 10, as_json);
    assert_eq!(all.len(), 5);
}

#[test]
fn a_take_left_out_is_made_up_by_the_next_row_so_the_list_stays_full() {
    let row = |title: &str| TopSong {
        rank: 1,
        song: Song {
            id: format!("out-{title}"),
            title: title.into(),
            artist: "A".into(),
            album: "Album".into(),
            ..Default::default()
        },
        plays: None,
        listeners: None,
    };
    let chart = vec![
        row("One"),
        row("Two (Instrumental)"),
        row("Three"),
        row("Four (Karaoke Version)"),
        row("Five"),
        row("Six"),
    ];
    let as_json = |s: &Song| json!({"id": s.id});

    let rows = TopSongsService::rows_to_match(&chart, 3, true);
    let library = vec![None; rows.len()];
    let (entries, outside) = assemble(&rows, &library, true, ExplicitFilter::All, true, 3, as_json);
    let ids: Vec<&str> = entries.iter().filter_map(|e| e["id"].as_str()).collect();
    assert_eq!(ids, vec!["out-One", "out-Three", "out-Five"]);
    assert_eq!(outside.len(), 3);

    // A take the library has is listed, and the list still stops at its length.
    let mut library = vec![None; rows.len()];
    library[1] = Some(json!({"id": "nd-two"}));
    let (entries, _) = assemble(&rows, &library, true, ExplicitFilter::All, true, 3, as_json);
    let ids: Vec<&str> = entries.iter().filter_map(|e| e["id"].as_str()).collect();
    assert_eq!(ids, vec!["out-One", "nd-two", "out-Three"]);
}

#[tokio::test]
async fn the_warm_worker_fetches_every_list_that_is_on_overall_first() {
    let lists = Lists::new("new,18,99").await;
    let worker = crate::services::charts::ChartsWarmWorker::new(
        lists.top_songs.clone(),
        lists.service.clone(),
        Clock::system(),
    );

    let fetched = worker.warm_once().await;

    assert_eq!(
        fetched, 3,
        "Popular right now, Best New Songs and Top Hip-Hop/Rap; 99 is no genre here"
    );
    assert!(lists.top_songs.peek_chart("new").await.is_some());
    assert!(
        lists
            .navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "no listener, no library search"
    );
}

#[tokio::test]
async fn the_warm_worker_does_nothing_with_every_list_off() {
    let lists = Lists::new("").await;
    let mut off = (*lists.settings.current()).clone();
    off.generated_playlists.popular_now = false;
    lists.settings.set(off);
    let worker = crate::services::charts::ChartsWarmWorker::new(
        lists.top_songs.clone(),
        lists.service.clone(),
        Clock::system(),
    );

    assert_eq!(worker.warm_once().await, 0);
    assert!(
        lists
            ._apple
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

const DEEZER_BODY: &str = r#"{"data":[
  {"id":11,"title":"Dracula","duration":185,"explicit_lyrics":false,
   "artist":{"id":27,"name":"Tame Impala"},"album":{"id":302,"title":"Deadbeat","cover_xl":"https://cdn/xl.jpg"}}
]}"#;

fn an_apple_list(period: i64) -> Made {
    Made {
        period,
        made_utc: Utc.with_ymd_and_hms(2026, 10, 7, 3, 0, 0).unwrap(),
        changed_utc: Utc.with_ymd_and_hms(2026, 10, 7, 3, 0, 0).unwrap(),
        rule: "old".into(),
        whole: true,
        source: "apple".into(),
        name: "Popular right now".into(),
        hash: 1,
        entries: vec![json!({"id": "apple-song"})],
        outside: Vec::new(),
    }
}

#[tokio::test]
async fn a_stand_in_never_replaces_an_apple_list() {
    let lists = Lists::with_deezer("", DEEZER_BODY).await;
    lists.library(Vec::new()).await;
    // Apple has no pages for this country, so Deezer's chart stands in for it.
    let mut moved = (*lists.settings.current()).clone();
    moved.generated_playlists.chart_country = "gb".into();
    lists.settings.set(moved);
    let last_period = period_index(*lists.now.lock(), REFRESH_HOURS) - 1;
    lists
        .service
        .inner
        .made
        .lock()
        .entry("alice".into())
        .or_default()
        .insert(OVERALL.into(), an_apple_list(last_period));

    let kept = lists.service.songs("alice", OVERALL, &auth()).await;

    let made = lists.service.current("alice", OVERALL).unwrap();
    assert_eq!(kept, vec![json!({"id": "apple-song"})]);
    assert_eq!(made.source, "apple", "the stand-in did not take its place");
    assert!(
        lists
            .service
            .inner
            .failed_at
            .lock()
            .contains_key(&("alice".to_string(), OVERALL.to_string())),
        "Apple is asked again soon"
    );

    // With nothing made, the stand-in is the list, and it is not whole.
    let first = lists.service.songs("bob", OVERALL, &auth()).await;
    let made = lists.service.current("bob", OVERALL).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(made.source, "deezer");
    assert!(!made.whole, "the next build tries Apple again");
}

const DEEZER_MARKED_BODY: &str = r#"{"data":[
  {"id":21,"title":"Pale Signal","duration":201,"explicit_content_lyrics":1,
   "artist":{"id":41,"name":"Lumen Drift"},"album":{"id":401,"title":"Pale Signal","cover_xl":"https://cdn/a.jpg"}},
  {"id":22,"title":"Paper Lanterns","duration":190,"explicit_content_lyrics":3,
   "artist":{"id":42,"name":"Quiet Harbor"},"album":{"id":402,"title":"Paper Lanterns","cover_xl":"https://cdn/b.jpg"}}
]}"#;

fn set_filter(lists: &Lists, filter: ExplicitFilter) {
    let mut changed = (*lists.settings.current()).clone();
    changed.subsonic.explicit_filter = filter;
    lists.settings.set(changed);
}

fn titles(songs: &[Value]) -> Vec<String> {
    songs
        .iter()
        .filter_map(|s| s["title"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_changed_explicit_filter_makes_a_stand_in_list_again_at_once() {
    let lists = Lists::with_deezer("", DEEZER_MARKED_BODY).await;
    lists.library(Vec::new()).await;
    // Apple has no pages for this country, so Deezer's chart stands in, and the list is not
    // whole: it waits a few minutes before it is tried again under the same settings.
    let mut moved = (*lists.settings.current()).clone();
    moved.generated_playlists.chart_country = "gb".into();
    lists.settings.set(moved);

    let all = lists.service.songs("alice", OVERALL, &auth()).await;
    assert_eq!(titles(&all), vec!["Pale Signal", "Paper Lanterns"]);

    set_filter(&lists, ExplicitFilter::CleanOnly);
    let clean = lists.service.songs("alice", OVERALL, &auth()).await;
    assert_eq!(titles(&clean), vec!["Paper Lanterns"]);

    set_filter(&lists, ExplicitFilter::ExplicitOnly);
    let explicit = lists.service.songs("alice", OVERALL, &auth()).await;
    assert_eq!(titles(&explicit), vec!["Pale Signal"]);

    set_filter(&lists, ExplicitFilter::All);
    let again = lists.service.songs("alice", OVERALL, &auth()).await;
    assert_eq!(titles(&again), vec!["Pale Signal", "Paper Lanterns"]);

    // Nothing changed since: the list waits out its retry and is not made again.
    let searched = lists.searches().await;
    lists.service.songs("alice", OVERALL, &auth()).await;
    assert_eq!(lists.searches().await, searched);
}

#[tokio::test]
async fn an_unchanged_whole_list_is_not_made_again() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;
    let searched = lists.searches().await;

    lists.service.songs("alice", OVERALL, &auth()).await;
    lists.advance_minutes(6);
    lists.service.songs("alice", OVERALL, &auth()).await;

    assert_eq!(lists.searches().await, searched);
}

/// A warm worker that also checks Navidrome's people, which answers `/api/user` with `users`.
async fn worker_with_people(
    lists: &Lists,
    users: ResponseTemplate,
) -> crate::services::charts::ChartsWarmWorker {
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "jwt-1", "isAdmin": true, "username": "admin",
            "subsonicToken": "tok", "subsonicSalt": "salt"
        })))
        .mount(&lists.navidrome)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .respond_with(users)
        .mount(&lists.navidrome)
        .await;
    let (admin, _, _) = crate::services::family::navidrome_admin::tests::admin_for(&lists.navidrome.uri());
    crate::services::charts::ChartsWarmWorker::new(
        lists.top_songs.clone(),
        lists.service.clone(),
        Clock::system(),
    )
    .with_people(admin)
}

fn only_the_admin() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!([
        {"id": "u1", "userName": "admin", "isAdmin": true}
    ]))
}

#[tokio::test]
async fn an_unreadable_user_list_drops_nothing() {
    let lists = Lists::new("new").await;
    lists.choose("bob", "br", &[]);
    let worker = worker_with_people(&lists, ResponseTemplate::new(503)).await;

    worker.warm_once().await;

    assert_eq!(lists.listeners.get("bob").country, "br");
    assert!(lists.listeners.is_known("someone-else"), "nothing was verified");
}

#[tokio::test]
async fn an_ok_answer_that_is_no_list_drops_nothing() {
    let lists = Lists::new("new").await;
    lists.choose("bob", "br", &[]);
    let worker = worker_with_people(&lists, ResponseTemplate::new(200).set_body_json(json!([]))).await;

    worker.warm_once().await;

    assert_eq!(lists.listeners.get("bob").country, "br");
}

#[tokio::test]
async fn a_list_without_the_admin_drops_nothing() {
    let lists = Lists::new("new").await;
    lists.choose("bob", "br", &[]);
    let others = ResponseTemplate::new(200).set_body_json(json!([{"id": "u2", "userName": "carl"}]));
    let worker = worker_with_people(&lists, others).await;

    worker.warm_once().await;

    assert_eq!(lists.listeners.get("bob").country, "br");
}

#[tokio::test]
async fn pruning_runs_with_every_list_off() {
    let lists = Lists::new("").await;
    let mut off = (*lists.settings.current()).clone();
    off.generated_playlists.popular_now = false;
    lists.settings.set(off);
    lists.choose("bob", "br", &[]);
    lists.choose("admin", "jp", &[]);
    let worker = worker_with_people(&lists, only_the_admin()).await;

    assert_eq!(worker.warm_once().await, 0);

    assert_eq!(lists.listeners.get("bob"), ListenerCharts::default());
    assert_eq!(lists.listeners.get("admin").country, "jp");
    assert_eq!(lists.listeners.get("admin").navidrome_id, "u1");
}

#[tokio::test]
async fn a_dropped_person_is_forgotten_by_playlists_and_the_match_cache() {
    let lists = Lists::remembering("new").await;
    lists.library(Vec::new()).await;
    Mock::given(method("GET"))
        .and(path("/rest/getScanStatus"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "subsonic-response": {"status": "ok", "scanStatus": {"lastScan": "2026-10-01T00:00:00Z"}}
        })))
        .mount(&lists.navidrome)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/getMusicFolders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "subsonic-response": {"status": "ok", "musicFolders": {"musicFolder": [{"id": 1, "name": "Music"}]}}
        })))
        .mount(&lists.navidrome)
        .await;
    lists.choose("alice", "", &["jp"]);
    lists.service.songs("alice", OVERALL, &auth()).await;
    assert!(lists.service.current("alice", OVERALL).is_some());
    assert!(lists.matches.kept_for("alice") > 0);
    let worker = worker_with_people(&lists, only_the_admin()).await;

    worker.warm_once().await;

    assert_eq!(lists.listeners.get("alice"), ListenerCharts::default());
    assert!(lists.service.current("alice", OVERALL).is_none());
    assert_eq!(lists.matches.kept_for("alice"), 0);
}

#[tokio::test]
async fn a_pinned_country_adds_every_list_that_is_on() {
    let lists = Lists::new("new").await;
    lists.pages_for("jp", false).await;
    lists.library(Vec::new()).await;
    lists.choose("alice", "", &["jp"]);
    lists.service.songs("alice", OVERALL, &auth()).await;

    let listed = lists.service.list("alice", &auth()).await;

    let names: Vec<&str> = listed.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "Popular right now",
            "Best New Songs",
            "Popular right now · Japan",
            "Best New Songs · Japan"
        ]
    );
    assert_eq!(lists.service.as_sync_stations("alice").len(), 4);
}

#[tokio::test]
async fn pinned_ids_differ_and_home_ids_stay() {
    let lists = Lists::new("").await;
    lists.pages_for("jp", false).await;
    lists.library(Vec::new()).await;
    lists.choose("alice", "", &["jp"]);
    lists.service.songs("alice", OVERALL, &auth()).await;

    let listed = lists.service.list("alice", &auth()).await;

    assert_eq!(listed[0].id, playlist_id("alice", "chart:popular"));
    assert_eq!(listed[0].key, "chart:popular");
    assert_eq!(listed[1].id, playlist_id("alice", "chart:popular@jp"));
    assert_eq!(listed[1].key, "chart:popular@jp");
    assert_ne!(listed[0].id, listed[1].id);
}

#[tokio::test]
async fn which_finds_a_pinned_list() {
    let lists = Lists::new("").await;
    lists.pages_for("jp", false).await;
    lists.library(Vec::new()).await;
    lists.choose("alice", "", &["jp"]);
    let id = playlist_id("alice", "chart:popular@jp");
    lists.service.songs("alice", "34@jp", &auth()).await;

    assert_eq!(lists.service.which("alice", &id).as_deref(), Some("34@jp"));
    assert_eq!(lists.service.which("bob", &id), None);
    let cover = lists.service.cover("alice", &id).unwrap();
    assert_eq!(cover.chart, "34");
    assert_eq!(cover.country.as_deref(), Some("jp"));
    assert_eq!(cover.name, "Popular right now · Japan");
}

#[tokio::test]
async fn a_pin_equal_to_the_home_country_is_skipped() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    lists.choose("alice", "", &["us"]);
    lists.service.songs("alice", OVERALL, &auth()).await;

    let listed = lists.service.list("alice", &auth()).await;

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Popular right now");
    assert_eq!(lists.service.lists_for("alice"), vec![OVERALL.to_string()]);
}

#[tokio::test]
async fn a_removed_pin_stops_being_listed() {
    let lists = Lists::new("").await;
    lists.pages_for("jp", false).await;
    lists.library(Vec::new()).await;
    lists.choose("alice", "", &["jp"]);
    lists.service.songs("alice", OVERALL, &auth()).await;
    assert_eq!(lists.service.list("alice", &auth()).await.len(), 2);

    lists.choose("alice", "", &[]);

    let listed = lists.service.list("alice", &auth()).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Popular right now");
    assert_eq!(lists.service.as_sync_stations("alice").len(), 1);
    assert!(lists.service.outside_songs("alice", "34@jp").is_empty(), "let go");
}

#[tokio::test]
async fn a_listener_country_change_remakes_the_lists() {
    let lists = Lists::new("").await;
    lists.pages_for("br", false).await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;
    assert_eq!(
        lists.searches_for("Drake Solar Eclipse").await,
        1,
        "not due again"
    );

    lists.choose("alice", "br", &[]);
    lists.service.songs("alice", OVERALL, &auth()).await;

    assert_eq!(lists.searches_for("Drake Solar Eclipse").await, 2);
    assert_eq!(
        lists.service.list("alice", &auth()).await[0].name,
        "Popular right now"
    );
}

#[tokio::test]
async fn a_failing_list_does_not_remake_the_others() {
    let lists = Lists::new("18").await;
    lists.pages_for("jp", true).await;
    lists.library(Vec::new()).await;
    lists.choose("alice", "", &["jp"]);
    lists.service.songs("alice", OVERALL, &auth()).await;
    assert!(lists.service.outside_songs("alice", "18@jp").is_empty());
    assert!(!lists.service.outside_songs("alice", "34@jp").is_empty());
    let searched = lists.searches().await;

    // Within the wait, nothing is due; after it, only the list that failed is tried again.
    lists.service.songs("alice", "18@jp", &auth()).await;
    lists.advance_minutes(6);
    lists.service.songs("alice", "18@jp", &auth()).await;

    assert_eq!(lists.searches().await, searched);
    assert_eq!(lists.searches_for("Drake Solar Eclipse").await, 1);
}

#[tokio::test]
async fn a_new_pin_is_made_without_remaking_home_lists() {
    let lists = Lists::new("").await;
    lists.pages_for("jp", false).await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;
    let before = lists.searches().await;

    lists.choose("alice", "", &["jp"]);
    let japan = lists.service.songs("alice", "34@jp", &auth()).await;

    assert!(!japan.is_empty());
    assert!(lists.searches().await > before);
    assert_eq!(
        lists.searches_for("Drake Solar Eclipse").await,
        1,
        "home lists stay"
    );
}

#[tokio::test]
async fn countries_listeners_chose_are_warmed_too() {
    let lists = Lists::new("").await;
    lists.pages_for("jp", false).await;
    lists.choose("alice", "", &["jp"]);
    let worker = crate::services::charts::ChartsWarmWorker::new(
        lists.top_songs.clone(),
        lists.service.clone(),
        Clock::system(),
    );

    assert_eq!(worker.warm_once().await, 2, "Popular right now here and in Japan");
    assert!(
        lists
            .top_songs
            .peek_chart_in(Some("jp".into()), OVERALL)
            .await
            .is_some()
    );
}

#[tokio::test]
async fn a_menu_not_known_for_a_moment_keeps_the_made_lists() {
    let lists = Lists::new("18").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;
    assert!(!lists.service.outside_songs("alice", "18").is_empty());
    let searched = lists.searches().await;

    // The country's menu comes back without the genre for a while, then with it again.
    let without: Vec<_> = octo_core::charts::chart_settings::english_menu()
        .into_iter()
        .filter(|g| g.id != "18")
        .collect();
    lists.top_songs.set_menu("us", without);
    let listed = lists.service.list("alice", &auth()).await;
    assert_eq!(listed.len(), 1, "not listed while the menu lacks it");
    assert!(!lists.service.outside_songs("alice", "18").is_empty(), "but kept");
    lists
        .top_songs
        .set_menu("us", octo_core::charts::chart_settings::english_menu());

    let listed = lists.service.list("alice", &auth()).await;
    assert_eq!(listed.len(), 2);
    assert_eq!(
        lists.searches().await,
        searched,
        "listed again without being made again"
    );
}

#[tokio::test]
async fn a_new_own_country_leaves_pinned_lists_be() {
    let lists = Lists::new("").await;
    lists.pages_for("jp", false).await;
    lists.pages_for("br", false).await;
    lists.library(Vec::new()).await;
    lists.choose("alice", "", &["jp"]);
    lists.service.songs("alice", OVERALL, &auth()).await;
    lists.service.songs("alice", "34@jp", &auth()).await;
    let japan = lists.service.outside_songs("alice", "34@jp");
    assert!(!japan.is_empty());
    let searched = lists.searches().await;

    lists.choose("alice", "br", &["jp"]);
    lists.service.songs("alice", OVERALL, &auth()).await;

    assert_eq!(
        lists.searches().await - searched,
        10,
        "only the five songs of her own list are searched again, each by name and, the library \
         having none, once more by title"
    );
    let ids = |songs: Vec<Song>| songs.into_iter().map(|s| s.id).collect::<Vec<_>>();
    assert_eq!(ids(lists.service.outside_songs("alice", "34@jp")), ids(japan));
}
