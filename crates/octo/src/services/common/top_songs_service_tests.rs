use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use octo_core::settings::{AppSettings, GeneratedPlaylistSettings, LastFmSettings};
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use octo_core::soulseek::soulseek_metadata_service::SoulseekRouting;

use super::*;
use crate::services::charts::ListenerCharts;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::test_support::{Matching, Routes};

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

const DEEZER_CHART: &str = r#"{"data":[
  {"id":11,"title":"Dracula (with JENNIE)","duration":185,"explicit_lyrics":true,"explicit_content_lyrics":1,
   "artist":{"id":27,"name":"Tame Impala"},"album":{"id":302,"title":"Deadbeat","cover_xl":"https://cdn/xl.jpg"}},
  {"id":12,"title":"Choosin' Texas","duration":232,"explicit_lyrics":false,
   "artist":{"id":28,"name":"Ella Langley"},"album":{"id":303,"title":"Choosin' Texas","cover_xl":"https://cdn/ct.jpg"}}
]}"#;

struct Charts {
    service: TopSongsService,
    apple: MockServer,
    _deezer: MockServer,
    deezer_routes: Routes,
    hints: Arc<ArtworkHints>,
    listeners: Arc<ChartCountryStore>,
    settings: Arc<SettingsStore>,
    metadata: Arc<FakeMetadata>,
}

/// Last.fm's names for two countries, in place of the generated table.
const GEO_FIXTURE: &[(&str, &str)] = &[("jp", "Japan"), ("us", "United States")];

impl Charts {
    /// The service reading Last.fm's country names from [`GEO_FIXTURE`].
    fn with_geo_fixture(mut self) -> Charts {
        self.service = self.service.with_geo_names(GEO_FIXTURE);
        self
    }

    /// The catalog knows these songs, so a Last.fm row for one of them is listed.
    fn knows(&self, artist: &str, title: &str) {
        self.metadata.hits.lock().insert(
            (artist.to_string(), title.to_string()),
            Song {
                artist: artist.to_string(),
                title: title.to_string(),
                ..Default::default()
            },
        );
    }

    async fn new(country: &str, deezer_chart: Option<&str>, last_fm: Option<&MockServer>) -> Charts {
        Charts::with_gap(country, deezer_chart, last_fm, Duration::ZERO).await
    }

    /// [`Charts::new`], with `gap` between two reads of Apple.
    async fn with_gap(
        country: &str,
        deezer_chart: Option<&str>,
        last_fm: Option<&MockServer>,
        gap: Duration,
    ) -> Charts {
        let apple = MockServer::builder().start().await;
        let deezer_routes = Routes::new(
            deezer_chart
                .map(|body| vec![("/chart/0/tracks", vec![body])])
                .unwrap_or_default(),
            Matching::IgnoreCase,
        );
        let deezer_server = deezer_routes.serve().await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            generated_playlists: GeneratedPlaylistSettings {
                chart_country: country.to_string(),
                ..Default::default()
            },
            last_fm: LastFmSettings {
                api_key: if last_fm.is_some() {
                    "key".to_string()
                } else {
                    String::new()
                },
                ..Default::default()
            },
            ..Default::default()
        }));
        let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
        let deezer = DeezerMetadataService::with_base_url(http, Arc::clone(&settings), deezer_server.uri());
        let last_fm = last_fm.map(|server| {
            Arc::new(LastFmService::with_base_url(
                Arc::clone(&settings),
                &format!("{}/2.0/", server.uri()),
            ))
        });
        let hints = Arc::new(ArtworkHints::new());
        let listeners = Arc::new(ChartCountryStore::new(None));
        let metadata = Arc::new(FakeMetadata::default());
        let service = TopSongsService::new(
            deezer,
            last_fm,
            metadata.clone(),
            Arc::new(ExternalIdRegistry::in_memory()),
            Arc::new(AppleMusicPages::with_base_url(&apple.uri(), gap).with_genres_base_url(&apple.uri())),
            Arc::clone(&hints),
            Arc::clone(&settings),
        )
        .with_listeners(Arc::clone(&listeners));
        Charts {
            service,
            apple,
            _deezer: deezer_server,
            deezer_routes,
            hints,
            listeners,
            settings,
            metadata,
        }
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

    /// The US overall chart as another country's: the same songs, that country's storefront.
    async fn overall_for(&self, country: &str) {
        let body =
            fixture!("us_all").replace("\"storefront\":\"us\"", &format!("\"storefront\":\"{country}\""));
        self.serve(&format!("/{country}/new/top-charts/songs"), None, &body)
            .await;
        let genre =
            fixture!("us_18").replace("\"storefront\":\"us\"", &format!("\"storefront\":\"{country}\""));
        self.serve(&format!("/{country}/new/top-charts/songs/"), Some("18"), &genre)
            .await;
    }

    async fn asked_for(&self, at: &str) -> usize {
        self.apple
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == at)
            .count()
    }

    async fn serve(&self, at: &str, genre: Option<&str>, body: &str) {
        let mock = Mock::given(method("GET")).and(path(at));
        let mock = match genre {
            Some(genre) => mock.and(query_param("genreId", genre)),
            None => mock.and(query_param_is_missing("genreId")),
        };
        mock.respond_with(ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/html"))
            .mount(&self.apple)
            .await;
    }

    async fn apple_down(&self) {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&self.apple)
            .await;
    }

    async fn apple_asked(&self) -> usize {
        self.apple.received_requests().await.map_or(0, |r| r.len())
    }

    async fn us_pages(&self) {
        self.serve("/us/new/top-charts/songs", None, fixture!("us_all"))
            .await;
        self.serve("/us/new/top-charts/songs/", Some("18"), fixture!("us_18"))
            .await;
        self.serve("/us/new", None, fixture!("us_new")).await;
        self.serve("/us/room/6818396128", None, fixture!("us_room_best_new"))
            .await;
        self.serve("/us/room/6818395867", None, fixture!("us_room_trending"))
            .await;
    }
}

fn titles(list: &TopList) -> Vec<&str> {
    list.songs.iter().map(|s| s.song.title.as_str()).collect()
}

#[tokio::test]
async fn the_chart_is_apples_in_apples_order() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("34").await;

    assert_eq!(list.source, "apple");
    assert_eq!(list.chart.as_deref(), Some("34"));
    assert_eq!(list.name.as_deref(), Some("Popular right now"));
    assert_eq!(list.country.as_deref(), Some("us"));
    assert_eq!(
        titles(&list),
        vec![
            "Solar Eclipse",
            "Quebec",
            "Cold Shoulder",
            "Choosin' Texas",
            "BbY WOW"
        ]
    );
    let ranks: Vec<usize> = list.songs.iter().map(|s| s.rank).collect();
    assert_eq!(ranks, vec![1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn a_genre_chart_is_named_top_and_the_genre() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("18").await;

    assert_eq!(list.name.as_deref(), Some("Top Hip-Hop/Rap"));
    assert_eq!(list.songs[0].song.title, "Janice STFU");
}

#[tokio::test]
async fn best_new_songs_comes_from_the_first_room_and_trending_from_the_second() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let best = charts.service.chart("new").await;
    let trending = charts.service.chart("trending").await;

    assert_eq!(best.name.as_deref(), Some("Best New Songs"));
    assert_eq!(best.room_id.as_deref(), Some("6818396128"));
    assert_eq!(best.songs[1].song.title, "Chelsea Boots (Bonus Track)");
    assert_eq!(trending.name.as_deref(), Some("Trending Songs"));
    assert_eq!(trending.songs[0].song.title, "Patient Zero");
}

#[tokio::test]
async fn a_collaboration_is_filed_under_its_main_artist() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("34").await;

    let solar = &list.songs[0].song;
    assert_eq!(solar.artist, "Drake");
    assert_eq!(solar.artists, vec!["Drake", "Don Toliver"]);
    assert_eq!(solar.primary_artist.as_deref(), Some("Drake"));
    assert_eq!(solar.album, "HABIBTI (FOMO)");
    assert!(!solar.is_local);
    assert_eq!(solar.external_id.as_deref(), Some(solar.id.as_str()));
    assert!(
        list.songs[1].song.artists.is_empty(),
        "one artist leaves the list empty"
    );
}

#[tokio::test]
async fn explicit_badge_marks_the_song_explicit() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("34").await;

    assert_eq!(list.songs[0].song.explicit_content_lyrics, Some(1));
    assert_eq!(list.songs[2].song.explicit_content_lyrics, Some(0));
}

#[tokio::test]
async fn chart_songs_leave_artwork_hints() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("34").await;

    let song = &list.songs[0].song;
    let routing = SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some(song.artist.clone()),
        title: Some(song.title.clone()),
        ..Default::default()
    };
    let url = charts.hints.for_routing(&routing).unwrap();
    assert!(url.ends_with("/1200x1200bb.jpg"), "{url}");
    assert_eq!(song.cover_art_url.as_deref(), Some(url.as_str()));
}

#[tokio::test]
async fn a_whole_list_is_kept_and_asked_once() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    charts.service.chart("18").await;
    let asked = charts.apple_asked().await;
    charts.service.chart("18").await;

    assert_eq!(charts.apple_asked().await, asked);
}

#[tokio::test]
async fn apple_down_the_overall_is_deezers() {
    let charts = Charts::new("us", Some(DEEZER_CHART), None).await;
    charts.apple_down().await;

    let list = charts.service.chart("34").await;

    assert_eq!(list.source, "deezer");
    assert_eq!(list.name.as_deref(), Some("Popular right now"));
    assert_eq!(titles(&list), vec!["Dracula (with JENNIE)", "Choosin' Texas"]);
    let fetch = charts
        .service
        .fetches()
        .into_iter()
        .find(|f| f.chart == "34")
        .unwrap();
    assert_eq!(fetch.source, "deezer");
    assert!(
        fetch
            .error
            .unwrap()
            .starts_with("Apple Music could not be reached")
    );
}

#[tokio::test]
async fn apple_and_deezer_down_the_overall_is_last_fms() {
    let last_fm = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("method", "chart.gettoptracks"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!({"tracks": {"track": [{"name": "Dracula", "playcount": "10", "artist": {"name": "Tame Impala"}}]}})
                .to_string(),
        ))
        .mount(&last_fm)
        .await;
    let charts = Charts::new("us", None, Some(&last_fm)).await;
    charts.apple_down().await;

    let list = charts.service.chart("34").await;

    assert_eq!(list.source, "lastfm");
    // The fake catalog had no hit for it, so the row is dropped, not invented.
    assert!(list.songs.is_empty());
    assert_eq!(charts.deezer_routes.calls("/chart/0/tracks"), 0);
}

#[tokio::test]
async fn a_genre_chart_never_falls_back() {
    let charts = Charts::new("us", Some(DEEZER_CHART), None).await;
    charts
        .serve("/us/new/top-charts/songs", None, fixture!("us_all"))
        .await;
    Mock::given(method("GET"))
        .and(path("/us/new/top-charts/songs/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&charts.apple)
        .await;

    let list = charts.service.chart("18").await;

    assert!(list.songs.is_empty());
    assert_eq!(list.source, "");
    assert_eq!(charts.deezer_routes.calls("/chart/0/tracks"), 0);
}

#[tokio::test]
async fn an_unknown_genre_is_empty_without_asking_apple() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;
    charts.service.menu().await;
    let asked = charts.apple_asked().await;

    assert!(charts.service.chart("99999").await.songs.is_empty());
    assert!(charts.service.chart("pop").await.songs.is_empty());

    assert_eq!(charts.apple_asked().await, asked);
}

#[tokio::test]
async fn auto_country_uses_the_detected_one() {
    let charts = Charts::new("auto", None, None).await;
    charts
        .serve("/new/top-charts/songs", None, fixture!("us_all"))
        .await;
    charts
        .serve("/us/new/top-charts/songs/", Some("18"), fixture!("us_18"))
        .await;

    // Learned by the warm worker, not by a request.
    charts.service.learn().await;
    assert_eq!(charts.service.country().await.as_deref(), Some("us"));
    let list = charts.service.chart("18").await;

    assert_eq!(list.country.as_deref(), Some("us"));
    assert_eq!(list.songs[0].song.title, "Janice STFU");
}

#[tokio::test]
async fn a_request_never_waits_to_learn_the_country() {
    let charts = Charts::new("auto", None, None).await;
    Mock::given(method("GET"))
        .and(path("/new/top-charts/songs"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture!("us_all").as_bytes().to_vec(), "text/html")
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&charts.apple)
        .await;

    let started = std::time::Instant::now();
    let (country, menu) = charts.service.menu().await;

    // Apple's own redirect and Octo's English genres serve until the country is learned behind.
    assert_eq!(country, None);
    assert!(menu.iter().any(|g| g.id == "18"));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the request waited on Apple"
    );
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(charts.service.country().await.as_deref(), Some("us"));
}

#[tokio::test]
async fn a_fixed_country_is_asked_for() {
    let charts = Charts::new("GB", None, None).await;
    charts
        .serve("/gb/new/top-charts/songs", None, fixture!("us_all"))
        .await;
    charts
        .serve("/gb/new/top-charts/songs/", Some("18"), fixture!("gb_18"))
        .await;

    let list = charts.service.chart("18").await;

    assert_eq!(list.songs[0].song.title, "Raindance");
    let asked = charts.apple.received_requests().await.unwrap();
    assert!(asked.iter().all(|r| r.url.path().starts_with("/gb/")));
}

#[tokio::test]
async fn a_listener_country_reads_that_country() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;
    charts.overall_for("br").await;
    charts.choose("anna", "br", &[]);

    let country = charts.service.country_of("anna").await;
    let list = charts.service.chart_in(country, "34").await;

    assert_eq!(list.country.as_deref(), Some("br"));
    assert_eq!(list.songs[0].song.title, "Solar Eclipse");
    assert!(charts.asked_for("/br/new/top-charts/songs").await >= 1);
    assert_eq!(charts.service.country_of("bob").await.as_deref(), Some("us"));
}

#[tokio::test]
async fn a_saved_country_apple_lacks_follows_the_server() {
    let charts = Charts::new("us", None, None).await;
    charts.choose("anna", "zz", &[]);

    assert_eq!(charts.service.country_of("anna").await.as_deref(), Some("us"));
}

#[tokio::test]
async fn two_listeners_on_one_country_share_one_fetch() {
    let charts = Charts::new("us", None, None).await;
    charts.overall_for("br").await;
    charts.choose("anna", "br", &[]);
    charts.choose("bob", "br", &["jp"]);
    charts.service.learn_in(Some("br".into())).await;

    let (anna, bob) = (
        charts.service.country_of("anna").await,
        charts.service.country_of("bob").await,
    );
    let (a, b) = tokio::join!(
        charts.service.chart_in(anna, "18"),
        charts.service.chart_in(bob, "18")
    );

    assert_eq!(a.songs[0].song.title, "Janice STFU");
    assert_eq!(a.songs.len(), b.songs.len());
    assert_eq!(charts.asked_for("/br/new/top-charts/songs/").await, 1);
}

#[tokio::test]
async fn no_deezer_stand_in_for_another_country() {
    let charts = Charts::new("us", Some(DEEZER_CHART), None).await;
    charts.apple_down().await;

    let japan = charts.service.chart_in(Some("jp".into()), "34").await;

    assert!(japan.songs.is_empty());
    assert_eq!(japan.source, "");
    assert_eq!(charts.deezer_routes.calls("/chart/0/tracks"), 0);
    let fetch = charts
        .service
        .fetches()
        .into_iter()
        .find(|f| f.asked.as_deref() == Some("jp"))
        .unwrap();
    assert!(fetch.error.is_some());

    let home = charts.service.chart("34").await;
    assert_eq!(home.source, "deezer");
    assert_eq!(charts.service.fetches().len(), 2, "one fetch per country");
}

fn last_fm_tracks(artist: &str, title: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(
        json!({"tracks": {"track": [{"name": title, "playcount": "10", "artist": {"name": artist}}]}})
            .to_string(),
    )
}

async fn last_fm_answers(
    server: &MockServer,
    method_name: &str,
    country: Option<&str>,
    answer: ResponseTemplate,
) {
    let mock = Mock::given(method("GET")).and(query_param("method", method_name));
    let mock = match country {
        Some(country) => mock.and(query_param("country", country)),
        None => mock,
    };
    mock.respond_with(answer).mount(server).await;
}

async fn last_fm_asked(server: &MockServer, method_name: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            r.url
                .query_pairs()
                .any(|(k, v)| k == "method" && v == method_name)
        })
        .map(|r| r.url.query().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn an_other_country_with_apple_down_and_nothing_made_uses_last_fm_geo() {
    let last_fm = MockServer::start().await;
    last_fm_answers(
        &last_fm,
        "geo.gettoptracks",
        Some("Japan"),
        last_fm_tracks("Tame Impala", "Dracula"),
    )
    .await;
    let charts = Charts::new("us", Some(DEEZER_CHART), Some(&last_fm))
        .await
        .with_geo_fixture();
    charts.knows("Tame Impala", "Dracula");
    charts.apple_down().await;

    let japan = charts.service.chart_in(Some("jp".into()), "34").await;

    assert_eq!(japan.source, "lastfm");
    assert_eq!(titles(&japan), vec!["Dracula"]);
    assert_eq!(
        charts.deezer_routes.calls("/chart/0/tracks"),
        0,
        "Deezer follows the server's address, not Japan"
    );
    assert!(
        last_fm_asked(&last_fm, "chart.gettoptracks").await.is_empty(),
        "the global chart is for the server's own country"
    );
    let fetch = charts
        .service
        .fetches()
        .into_iter()
        .find(|f| f.asked.as_deref() == Some("jp"))
        .unwrap();
    assert!(fetch.error.is_some(), "Apple's failure is still shown");
}

#[tokio::test]
async fn the_server_country_tries_deezer_then_geo_then_global() {
    // Deezer first.
    let last_fm = MockServer::start().await;
    last_fm_answers(
        &last_fm,
        "geo.gettoptracks",
        Some("United States"),
        last_fm_tracks("Geo Artist", "Geo Song"),
    )
    .await;
    last_fm_answers(
        &last_fm,
        "chart.gettoptracks",
        None,
        last_fm_tracks("Global Artist", "Global Song"),
    )
    .await;
    let charts = Charts::new("us", Some(DEEZER_CHART), Some(&last_fm))
        .await
        .with_geo_fixture();
    charts.knows("Geo Artist", "Geo Song");
    charts.knows("Global Artist", "Global Song");
    charts.apple_down().await;
    assert_eq!(charts.service.chart("34").await.source, "deezer");
    assert!(last_fm_asked(&last_fm, "geo.gettoptracks").await.is_empty());

    // No Deezer chart: the country's own chart.
    let charts = Charts::new("us", None, Some(&last_fm)).await.with_geo_fixture();
    charts.knows("Geo Artist", "Geo Song");
    charts.knows("Global Artist", "Global Song");
    charts.apple_down().await;
    let list = charts.service.chart("34").await;
    assert_eq!(list.source, "lastfm");
    assert_eq!(titles(&list), vec!["Geo Song"]);
    assert!(last_fm_asked(&last_fm, "chart.gettoptracks").await.is_empty());

    // Last.fm has no such country (error 6): the global chart.
    let no_country = MockServer::start().await;
    last_fm_answers(
        &no_country,
        "geo.gettoptracks",
        None,
        ResponseTemplate::new(200).set_body_string(r#"{"error":6,"message":"country param invalid"}"#),
    )
    .await;
    last_fm_answers(
        &no_country,
        "chart.gettoptracks",
        None,
        last_fm_tracks("Global Artist", "Global Song"),
    )
    .await;
    let charts = Charts::new("us", None, Some(&no_country))
        .await
        .with_geo_fixture();
    charts.knows("Global Artist", "Global Song");
    charts.apple_down().await;
    let list = charts.service.chart("34").await;
    assert_eq!(list.source, "lastfm");
    assert_eq!(titles(&list), vec!["Global Song"]);
    assert_eq!(last_fm_asked(&no_country, "geo.gettoptracks").await.len(), 1);
    assert_eq!(last_fm_asked(&no_country, "chart.gettoptracks").await.len(), 1);
}

#[tokio::test]
async fn geo_asks_the_table_name() {
    let last_fm = MockServer::start().await;
    last_fm_answers(
        &last_fm,
        "geo.gettoptracks",
        None,
        last_fm_tracks("Tame Impala", "Dracula"),
    )
    .await;
    let charts = Charts::new("gb", None, Some(&last_fm)).await.with_geo_fixture();
    charts.knows("Tame Impala", "Dracula");
    charts.apple_down().await;

    charts.service.chart_in(Some("jp".into()), "34").await;
    charts.service.chart_in(Some("fr".into()), "34").await;

    let asked = last_fm_asked(&last_fm, "geo.gettoptracks").await;
    assert_eq!(asked.len(), 1, "France is not in the table: {asked:?}");
    assert!(asked[0].contains("country=Japan"), "{}", asked[0]);
}

#[tokio::test]
async fn a_fixed_country_unlike_the_ip_skips_deezer() {
    let last_fm = MockServer::start().await;
    last_fm_answers(
        &last_fm,
        "geo.gettoptracks",
        Some("United States"),
        last_fm_tracks("Geo Artist", "Geo Song"),
    )
    .await;
    let charts = Charts::new("us", Some(DEEZER_CHART), Some(&last_fm))
        .await
        .with_geo_fixture();
    charts.knows("Geo Artist", "Geo Song");
    // Apple puts this server in Great Britain.
    let gb = fixture!("us_all").replace("\"storefront\":\"us\"", "\"storefront\":\"gb\"");
    charts.serve("/new/top-charts/songs", None, &gb).await;
    assert_eq!(
        charts
            .service
            .apple()
            .detected_country_on(&crate::services::charts::apple_music_pages::LaneCell::behind())
            .await
            .as_deref(),
        Some("gb")
    );

    let list = charts.service.chart("34").await;

    assert_eq!(charts.deezer_routes.calls("/chart/0/tracks"), 0);
    assert_eq!(list.source, "lastfm");
    assert_eq!(titles(&list), vec!["Geo Song"]);
}

#[tokio::test]
async fn menus_are_learned_per_country() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;
    charts
        .serve("/jp/new/top-charts/songs", None, fixture!("jp_all"))
        .await;

    // Both start learning behind the answer; one country's learning does not hold up the other's.
    charts.service.menu_in(Some("jp".into())).await;
    charts.service.menu_in(Some("us".into())).await;
    let menus = &charts.service.inner.menus;
    for _ in 0..100 {
        if menus.get("jp").is_some() && menus.get("us").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (_, japan) = charts.service.menu_in(Some("jp".into())).await;
    let (_, home) = charts.service.menu().await;
    assert!(japan.iter().any(|g| g.id == "27"), "J-Pop is Japan's");
    assert!(!home.iter().any(|g| g.id == "27"));
    assert!(home.iter().any(|g| g.id == "18"));
}

/// How many other countries `who` has loading now.
fn counted(charts: &Charts, who: &str) -> usize {
    charts
        .service
        .inner
        .others_working
        .lock()
        .get(who)
        .copied()
        .unwrap_or(0)
}

/// Starts counting `country`'s chart `34` for `who`, expecting it to count.
fn count_chart(charts: &Charts, who: &str, country: &str) -> OtherWork {
    let work = charts
        .service
        .begin_other_work(who, country, &TopSongsService::chart_work(country, "34"))
        .unwrap_or_else(|| panic!("{country} is refused for {who}"));
    assert!(matches!(work, OtherWork::Counted(_)), "{country} counts");
    work
}

#[tokio::test]
async fn a_third_other_country_waits_for_the_first_two() {
    let charts = Charts::new("us", None, None).await;
    let first = count_chart(&charts, "anna", "jp");
    let _second = count_chart(&charts, "anna", "br");

    assert!(
        charts
            .service
            .begin_other_work("anna", "de", &TopSongsService::chart_work("de", "34"))
            .is_none()
    );
    assert!(
        charts
            .service
            .begin_other_work("bob", "de", &TopSongsService::chart_work("de", "34"))
            .is_some(),
        "the limit is per listener"
    );

    drop(first);

    assert_eq!(counted(&charts, "anna"), 1);
    count_chart(&charts, "anna", "de");
}

#[tokio::test]
async fn a_dropped_request_keeps_its_place_until_the_build_ends() {
    let charts = Charts::new("us", None, None).await;
    let release = Arc::new(tokio::sync::Notify::new());
    let key = TopSongsService::chart_work("jp", "34");
    let hold = |service: TopSongsService, work: OtherWork| {
        let release = Arc::clone(&release);
        let key = key.clone();
        async move {
            service
                .cached(
                    Kept::Other,
                    key,
                    CHART_TTL,
                    CHART_BUILD_TIMEOUT,
                    &LaneCell::behind(),
                    work,
                    async move {
                        release.notified().await;
                        (TopList::default(), true)
                    },
                )
                .await
        }
    };

    // The request that started the build goes away while the build is held open.
    let work = count_chart(&charts, "anna", "jp");
    let request = tokio::spawn(hold(charts.service.clone(), work));
    tokio::task::yield_now().await;
    request.abort();
    let _ = request.await;
    let _second = count_chart(&charts, "anna", "br");

    assert!(
        charts
            .service
            .begin_other_work("anna", "de", &TopSongsService::chart_work("de", "34"))
            .is_none(),
        "the dropped request's build still counts"
    );

    // Another request joins the build; when it ends the place is free.
    let joined = tokio::spawn(hold(charts.service.clone(), OtherWork::Free));
    tokio::task::yield_now().await;
    release.notify_one();
    joined.await.unwrap();

    assert_eq!(counted(&charts, "anna"), 1);
    count_chart(&charts, "anna", "de");
}

#[tokio::test]
async fn get_charts_counts_the_menu_it_learns() {
    let charts = Charts::new("us", None, None).await;
    charts
        .serve("/jp/new/top-charts/songs", None, fixture!("jp_all"))
        .await;
    let work = charts
        .service
        .begin_other_work("anna", "jp", &TopSongsService::menu_work("jp"))
        .expect("allowed");
    assert!(matches!(work, OtherWork::Counted(_)));
    assert_eq!(counted(&charts, "anna"), 1);

    charts.service.menu_in_counted(Some("jp".into()), work).await;

    // The learning is still going, or just ended: the count ends with it, not before.
    let freed = {
        let mut freed = false;
        for _ in 0..200 {
            if counted(&charts, "anna") == 0 {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        freed
    };
    assert!(freed, "the learning ended and released its place");
    assert!(charts.service.inner.menus.get("jp").is_some());

    // A menu already held starts nothing, so a counted request drops its place at once.
    let work = charts
        .service
        .begin_other_work("anna", "de", &TopSongsService::menu_work("de"))
        .expect("allowed");
    charts.service.set_menu("de", english_menu());
    charts.service.menu_in_counted(Some("de".into()), work).await;
    assert_eq!(counted(&charts, "anna"), 0);
}

#[tokio::test]
async fn a_joined_build_is_free() {
    let charts = Charts::new("us", None, None).await;
    let release = Arc::new(tokio::sync::Notify::new());
    let key = TopSongsService::chart_work("jp", "34");
    let work = count_chart(&charts, "anna", "jp");
    let building = {
        let (service, release, key) = (charts.service.clone(), Arc::clone(&release), key.clone());
        tokio::spawn(async move {
            service
                .cached(
                    Kept::Other,
                    key,
                    CHART_TTL,
                    CHART_BUILD_TIMEOUT,
                    &LaneCell::behind(),
                    work,
                    async move {
                        release.notified().await;
                        (TopList::default(), true)
                    },
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    let _second = count_chart(&charts, "anna", "br");

    // A third country is refused for anna, but a build already running for jp is free for anyone.
    assert!(
        charts
            .service
            .begin_other_work("anna", "de", &TopSongsService::chart_work("de", "34"))
            .is_none()
    );
    assert!(matches!(
        charts.service.begin_other_work("anna", "jp", &key),
        Some(OtherWork::Free)
    ));
    assert!(matches!(
        charts.service.begin_other_work("bob", "jp", &key),
        Some(OtherWork::Free)
    ));

    release.notify_one();
    building.await.unwrap();
}

#[tokio::test]
async fn held_own_pinned_and_server_countries_are_never_counted() {
    let charts = Charts::new("us", None, None).await;
    charts.choose("anna", "br", &["kr"]);
    let _first = count_chart(&charts, "anna", "jp");
    let _second = count_chart(&charts, "anna", "de");
    // A chart already held.
    charts
        .service
        .cached(
            Kept::Other,
            TopSongsService::chart_work("fr", "34"),
            CHART_TTL,
            CHART_BUILD_TIMEOUT,
            &LaneCell::behind(),
            OtherWork::Free,
            async { (TopList::default(), true) },
        )
        .await;

    for code in ["br", "kr", "us", "fr"] {
        assert!(
            matches!(
                charts
                    .service
                    .begin_other_work("ANNA", code, &TopSongsService::chart_work(code, "34")),
                Some(OtherWork::Free)
            ),
            "{code}"
        );
    }
    assert_eq!(counted(&charts, "anna"), 2);
}

#[tokio::test]
async fn names_give_the_playlist_and_the_chip() {
    let menu = english_menu();
    assert_eq!(
        TopSongsService::names(&menu, "34"),
        Some(("Popular right now".into(), "Top songs".into()))
    );
    assert_eq!(
        TopSongsService::names(&menu, "new"),
        Some(("Best New Songs".into(), "Best New Songs".into()))
    );
    assert_eq!(
        TopSongsService::names(&menu, "18"),
        Some(("Top Hip-Hop/Rap".into(), "Hip-Hop/Rap".into()))
    );
    assert_eq!(TopSongsService::names(&menu, "99999"), None);
}

#[tokio::test]
async fn an_unknown_non_latin_genre_is_hidden_until_named() {
    let charts = Charts::new("us", None, None).await;
    let genre = |id: &str, name: &str| ChartGenre {
        id: id.into(),
        name: name.into(),
    };
    charts.service.set_menu(
        "us",
        vec![
            genre("34", "All Genres"),
            genre("99998", "Música Mexicana"),
            genre("99999", "ロック"),
        ],
    );
    Mock::given(method("GET"))
        .and(path("/WebObjects/MZStoreServices.woa/ws/genres"))
        .and(query_param("id", "99999"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"99999":{"name":"Rock Test","id":"99999"}}"#),
        )
        .mount(&charts.apple)
        .await;

    let (_, menu) = charts.service.menu_in(Some("us".into())).await;
    let ids: Vec<&str> = menu.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, vec!["34", "99998"], "the foreign-script genre is left out");
    assert!(
        charts
            .service
            .chart_in(Some("us".into()), "99999")
            .await
            .songs
            .is_empty()
    );

    charts.service.learn_names(vec!["99999".into()]).await;

    let (_, menu) = charts.service.menu_in(Some("us".into())).await;
    assert_eq!(
        genre_name(&menu, "99999").as_deref(),
        Some("Rock Test"),
        "{:?}",
        menu
    );
    assert_eq!(
        TopSongsService::names(&menu, "99999"),
        Some(("Top Rock Test".into(), "Rock Test".into()))
    );
}

fn top(rank: usize, artist: &str, title: &str) -> TopSong {
    TopSong {
        rank,
        song: Song {
            id: format!("out-{rank}"),
            artist: artist.into(),
            title: title.into(),
            ..Default::default()
        },
        plays: None,
        listeners: None,
    }
}

#[tokio::test]
async fn match_library_finds_the_same_recording_only_and_gives_a_library_song_to_one_row() {
    let rows = vec![
        top(1, "Air", "Sexy Boy"),
        top(
            2,
            "Daft Punk",
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
        ),
        top(3, "Daft Punk", "Get Lucky (Radio Edit)"),
        top(4, "Daft Punk", "Around the World"),
    ];
    let asked = AtomicUsize::new(0);
    let queries = std::sync::Mutex::new(Vec::<String>::new());
    let found = TopSongsService::match_library(&rows, |query| {
        asked.fetch_add(1, Ordering::SeqCst);
        queries.lock().unwrap().push(query.clone());
        async move {
            match query.as_str() {
                "Air Sexy Boy" => Some(vec![
                    json!({"id": "nd-airbourne", "artist": "Airbourne", "title": "Sexy Boy"}),
                ]),
                "Daft Punk Get Lucky" => Some(vec![
                    json!({"id": "nd-lucky", "artist": "Daft Punk", "title": "Get Lucky"}),
                ]),
                "Daft Punk Around the World" => None,
                _ => Some(Vec::new()),
            }
        }
    })
    .await;

    assert!(found[0].is_none());
    assert_eq!(found[1].as_ref().unwrap()["id"], "nd-lucky");
    assert!(found[2].is_none());
    assert!(found[3].is_none());
    // Row 1 found nothing that is its recording, so it asked once more by title alone. Rows 2 and
    // 3 matched the same library song (row 3 loses it to the higher ranked row 2 afterwards), so
    // neither retried; row 4's library did not answer, which is not asked again.
    assert_eq!(asked.load(Ordering::SeqCst), 5, "{:?}", queries.lock().unwrap());
}

/// Navidrome's full-text search as it behaves: every word asked must be in the song's own indexed
/// text (its title and the artist credit the file carries, nothing else).
fn fake_navidrome(songs: &'static [(&'static str, &'static str, &'static str)], query: &str) -> Vec<Value> {
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_string).collect();
    songs
        .iter()
        .filter(|(_, artist, title)| {
            let indexed = format!("{artist} {title}").to_lowercase();
            let have: Vec<&str> = indexed.split_whitespace().collect();
            words.iter().all(|w| have.contains(&w.as_str()))
        })
        .take(5)
        .map(|(id, artist, title)| json!({"id": id, "artist": artist, "title": title}))
        .collect()
}

static FILED_UNDER_YE: &[(&str, &str, &str)] = &[
    ("nd-wire", "Ye", "Through the Wire"),
    ("nd-wire-cover", "Some Band", "Through the Wire"),
    ("nd-jesus", "Ye", "Jesus Walks"),
];

#[tokio::test]
async fn match_library_finds_a_song_filed_under_the_artists_other_name() {
    let rows = vec![
        top(1, "Kanye West", "Through the Wire"),
        top(2, "Kanye West", "Jesus Walks (Explicit)"),
    ];
    let asked = std::sync::Mutex::new(Vec::<String>::new());
    let found = TopSongsService::match_library(&rows, |query| {
        asked.lock().unwrap().push(query.clone());
        let hits = fake_navidrome(FILED_UNDER_YE, &query);
        async move { Some(hits) }
    })
    .await;
    // The first query ("Kanye West Through the Wire") finds nothing in this index.
    assert!(fake_navidrome(FILED_UNDER_YE, "Kanye West Through the Wire").is_empty());
    assert_eq!(found[0].as_ref().unwrap()["id"], "nd-wire");
    assert_eq!(found[1].as_ref().unwrap()["id"], "nd-jesus");
    assert_eq!(asked.lock().unwrap().len(), 4, "one retry per row, no more");
}

#[tokio::test]
async fn match_library_title_retry_never_matches_another_artists_song() {
    // The library only has another artist's song of that title: the retry finds it, the rule
    // refuses it.
    static OTHER_ARTIST: &[(&str, &str, &str)] = &[("nd-cover", "Some Band", "Through the Wire")];
    let rows = vec![top(1, "Kanye West", "Through the Wire")];
    let found = TopSongsService::match_library(&rows, |query| {
        let hits = fake_navidrome(OTHER_ARTIST, &query);
        async move { Some(hits) }
    })
    .await;
    assert!(found[0].is_none());

    // The right artist but another version of the song is no match either.
    static LIVE: &[(&str, &str, &str)] = &[("nd-live", "Ye", "Through the Wire (Live)")];
    let found = TopSongsService::match_library(&rows, |query| {
        let hits = fake_navidrome(LIVE, &query);
        async move { Some(hits) }
    })
    .await;
    assert!(found[0].is_none());
}

#[tokio::test]
async fn match_library_title_retry_is_cached_as_one_miss_and_a_failed_retry_is_not() {
    let rows = vec![top(1, "Kanye West", "Heartless")];
    let cache = crate::services::library::LibraryMatchCache::default();
    let session = cache.session_for_tests("alice", "k");
    let asked = AtomicUsize::new(0);
    for _ in 0..3 {
        let found = TopSongsService::match_library_with(
            &rows,
            |_| {
                asked.fetch_add(1, Ordering::SeqCst);
                async { Some(Vec::new()) }
            },
            &session,
        )
        .await;
        assert!(found[0].is_none());
    }
    assert_eq!(asked.load(Ordering::SeqCst), 2, "searched once by name and once by title, then skipped");

    // A retry that gets no answer is not remembered as a miss.
    let cache = crate::services::library::LibraryMatchCache::default();
    let session = cache.session_for_tests("alice", "k");
    let asked = AtomicUsize::new(0);
    TopSongsService::match_library_with(
        &rows,
        |query| {
            asked.fetch_add(1, Ordering::SeqCst);
            async move { (query == "Kanye West Heartless").then(Vec::new) }
        },
        &session,
    )
    .await;
    assert!(!session.skips("Kanye West Heartless"));
}

#[test]
fn skip_covers_and_instrumentals_leaves_outside_takes_off_a_list_and_the_next_rows_fill_it() {
    let rows = vec![
        top(1, "Adele", "Hello"),
        top(2, "Adele", "Rolling in the Deep (Instrumental)"),
        top(3, "Adele", "Skyfall (Karaoke Version)"),
        top(4, "Catfish and the Bottlemen", "Karaoke Queen"),
        top(5, "Adele", "Someone Like You"),
    ];
    let ranks = |rows: &[TopSong]| rows.iter().map(|row| row.rank).collect::<Vec<_>>();

    // Off: the first three rows, as always.
    let off = TopSongsService::rows_to_match(&rows, 3, false);
    assert_eq!(ranks(&off), [1, 2, 3]);
    let (shown, _) = TopSongsService::shown_rows(off, vec![None; 3], 3, false);
    assert_eq!(ranks(&shown), [1, 2, 3]);

    // On: the two takes are matched too, and left out unless the library has them.
    let on = TopSongsService::rows_to_match(&rows, 3, true);
    assert_eq!(ranks(&on), [1, 2, 3, 4, 5]);
    let (shown, _) = TopSongsService::shown_rows(on.clone(), vec![None; 5], 3, true);
    assert_eq!(ranks(&shown), [1, 4, 5], "a name is not a take; ranks are kept");
    let owned = vec![None, None, Some(json!({"id": "nd-skyfall"})), None, None];
    let (shown, library) = TopSongsService::shown_rows(on, owned, 3, true);
    assert_eq!(ranks(&shown), [1, 3, 4], "the library's own copy is listed");
    assert_eq!(library[1].as_ref().unwrap()["id"], "nd-skyfall");
}

#[tokio::test]
async fn match_library_never_searches_more_than_four_at_once() {
    let rows: Vec<TopSong> = (1..=20).map(|i| top(i, "Artist", &format!("Song {i}"))).collect();
    let running = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    TopSongsService::match_library(&rows, |_| {
        let (running, most) = (Arc::clone(&running), Arc::clone(&most));
        async move {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            running.fetch_sub(1, Ordering::SeqCst);
            Some(Vec::new())
        }
    })
    .await;
    assert!(most.load(Ordering::SeqCst) <= LIBRARY_SEARCHES_AT_ONCE);
}

#[tokio::test]
async fn another_countrys_failed_chart_never_stands_in_for_the_servers() {
    let charts = Charts::new("gb", Some(DEEZER_CHART), None).await;
    charts.apple_down().await;
    let us = charts.service.chart_in(Some("us".into()), "34").await;
    assert!(us.songs.is_empty(), "no stand-in for another country");

    let mut moved = (*charts.settings.current()).clone();
    moved.generated_playlists.chart_country = "us".into();
    charts.settings.set(moved);
    let home = charts.service.chart("34").await;

    assert_eq!(home.source, "deezer", "the server's own chart has its stand-in");
    assert_eq!(titles(&home), vec!["Dracula (with JENNIE)", "Choosin' Texas"]);
}

#[tokio::test]
async fn another_countrys_chart_is_found_without_building_it_again() {
    let charts = Charts::new("us", None, None).await;
    charts.overall_for("br").await;
    charts.service.chart_in(Some("br".into()), "34").await;
    let asked = charts.apple_asked().await;

    let again = charts.service.chart_in(Some("br".into()), "34").await;

    assert_eq!(again.country.as_deref(), Some("br"));
    assert_eq!(charts.apple_asked().await, asked);
    assert!(
        charts
            .service
            .peek_chart_in(Some("br".into()), "34")
            .await
            .is_some()
    );
    assert!(
        charts.service.peek_chart("34").await.is_none(),
        "not the server's"
    );
}

#[tokio::test(start_paused = true)]
async fn a_joined_background_build_turns_urgent() {
    let charts = Charts::with_gap("us", None, None, Duration::from_secs(1)).await;
    let apple = Arc::clone(charts.service.apple());
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let settle = || tokio::time::sleep(Duration::from_millis(1));

    // Older background reads hold the gate's queue.
    for i in 1..=3 {
        let (apple, log) = (Arc::clone(&apple), Arc::clone(&log));
        tokio::spawn(async move {
            apple.gate().wait_in(&LaneCell::behind(), || false).await;
            log.lock().push(format!("old{i}"));
        });
        settle().await;
    }

    // A background build of one list waits behind them.
    let background = LaneCell::behind();
    let build = {
        let (service, apple, log, lane) = (
            charts.service.clone(),
            Arc::clone(&apple),
            Arc::clone(&log),
            background.clone(),
        );
        tokio::spawn(async move {
            let reads_in = lane.clone();
            service
                .cached(
                    Kept::Server,
                    "chart|us|34".into(),
                    CHART_TTL,
                    CHART_BUILD_TIMEOUT,
                    &lane,
                    OtherWork::Free,
                    async move {
                        apple.gate().wait_in(&reads_in, || false).await;
                        log.lock().push("build".into());
                        (TopList::default(), true)
                    },
                )
                .await
        })
    };
    settle().await;
    assert!(!background.is_now());

    // An interactive request for the same list joins it.
    let started_again = Arc::new(AtomicUsize::new(0));
    let asked = {
        let (service, counted) = (charts.service.clone(), Arc::clone(&started_again));
        tokio::spawn(async move {
            service
                .cached(
                    Kept::Server,
                    "chart|us|34".into(),
                    CHART_TTL,
                    CHART_BUILD_TIMEOUT,
                    &LaneCell::now(),
                    OtherWork::Free,
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        (TopList::default(), true)
                    },
                )
                .await
        })
    };
    settle().await;
    assert!(background.is_now(), "the joined build reads interactively");

    tokio::time::sleep(Duration::from_secs(10)).await;

    assert_eq!(log.lock().clone(), ["old1", "old2", "build", "old3"]);
    assert_eq!(
        started_again.load(Ordering::SeqCst),
        0,
        "it joined, never started"
    );
    assert!(build.await.is_ok());
    assert!(asked.await.is_ok());
    assert!(charts.service.inner.urgent.lock().is_empty());
}
