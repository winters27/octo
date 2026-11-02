//! The recommendation builds against a stand-in Last.fm answering by `method`.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use octo_core::last_fm::last_fm_radio_recommendation_service::{RadioRandom, Randomizer, seeded_randomizer};
use octo_core::models::radio::LastFmRadioTrack;
use octo_core::settings::{AppSettings, DiscoveryStationSettings, LastFmSettings, MetadataSettings};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::soulseek::ExternalIdRegistry;

/// The usual Last.fm fake: tags, similar tracks and artists, and top tracks, by `method`.
#[derive(Clone)]
struct RecommendationHandler;

fn query(request: &Request) -> HashMap<String, String> {
    request.url.query_pairs().into_owned().collect()
}

impl Respond for RecommendationHandler {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query = query(request);
        let get = |name: &str| query.get(name).cloned().unwrap_or_default();
        let body = match get("method").as_str() {
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
}

/// A Last.fm fake whose similar tracks are specific to the seed asked about.
#[derive(Clone)]
struct SeedSpecificHandler;

impl Respond for SeedSpecificHandler {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query = query(request);
        let seed = query.get("track").cloned().unwrap_or_else(|| "seed".into());
        let body: Value = match query.get("method").map(String::as_str) {
            Some("track.getsimilar") => json!({"similartracks": {"track": (0..15).map(|index| json!({
                "name": format!("{seed} sim {index}"), "match": 1.0 - f64::from(index) / 100.0,
                "duration": 180000, "artist": {"name": format!("{seed} Neighbour {index}")}
            })).collect::<Vec<_>>()}}),
            Some("artist.gettoptags") => json!({"toptags": {"tag": []}}),
            _ => json!({}),
        };
        ResponseTemplate::new(200).set_body_string(body.to_string())
    }
}

/// An empty answer to everything: a Last.fm with nothing usable to say.
#[derive(Clone)]
struct Nothing;

impl Respond for Nothing {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string("{}")
    }
}

struct Fixture {
    _dir: TempDir,
    _server: MockServer,
    settings: Arc<SettingsStore>,
    state: Arc<LastFmRadioStateStore>,
    service: LastFmRadioRecommendationService,
}

async fn fixture(last_fm: LastFmSettings, handler: impl Respond + 'static) -> Fixture {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(handler).mount(&server).await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm,
        metadata: MetadataSettings {
            language: "en".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let state = Arc::new(LastFmRadioStateStore::new(
        dir.path().join("state.json"),
        settings.clone(),
        Arc::new(ExternalIdRegistry::in_memory()),
    ));
    let client = Arc::new(LastFmService::with_base_url(
        settings.clone(),
        &format!("{}/", server.uri()),
    ));
    let service = LastFmRadioRecommendationService::new(client, state.clone(), settings.clone(), None);
    Fixture {
        _dir: dir,
        _server: server,
        settings,
        state,
        service,
    }
}

fn record(state: &LastFmRadioStateStore, play: LastFmRadioPlay) {
    state.record_play("alice", play);
}

fn station(definition: (&str, &str, &[&str])) -> DiscoveryStationSettings {
    DiscoveryStationSettings {
        id: definition.0.into(),
        name: definition.1.into(),
        tags: definition.2.iter().map(|tag| tag.to_string()).collect(),
        ..Default::default()
    }
}

fn keys(station: &LastFmRadioStation) -> Vec<String> {
    station
        .tracks
        .iter()
        .map(|track| seed_normalizer::track_key(&track.artist, &track.title))
        .collect()
}

async fn build(service: &LastFmRadioRecommendationService) -> Vec<LastFmRadioStation> {
    service
        .build("alice", &CancellationToken::new())
        .await
        .expect("the build succeeds")
}

/// A draw that always returns the same number: order becomes pure weight order.
fn flat_random() -> Randomizer {
    struct Flat;
    impl RadioRandom for Flat {
        fn next_double(&mut self) -> f64 {
            0.5
        }
    }
    Arc::new(|| Box::new(Flat) as Box<dyn RadioRandom>)
}

#[tokio::test]
async fn sparse_history_produces_deterministic_starter_and_local_pinned_fallback() {
    let fixture = fixture(
        LastFmSettings {
            api_key: String::new(),
            minimum_plays: 10,
            radio_track_count: 10,
            discovery_stations: vec![station(("rock", "Rock Discovery", &["rock"]))],
            ..Default::default()
        },
        Nothing,
    )
    .await;
    for index in 0..6 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: format!("Artist {index}"),
                title: format!("Track {index}"),
                genre: Some(if index < 5 { "Rock" } else { "Jazz" }.into()),
                played_at_utc: Utc::now() - TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    // Same seed for both builds: the draw is what varies a refresh, and with it pinned the rest
    // of the pipeline has to be deterministic.
    fixture.service.set_randomizer(seeded_randomizer(1234));
    let first = build(&fixture.service).await;
    let second = build(&fixture.service).await;
    assert!(first.iter().any(|s| s.kind == LastFmRadioStationKind::Starter));
    assert!(first.iter().any(|s| s.kind == LastFmRadioStationKind::Pinned));
    assert!(!first.iter().any(|s| s.kind == LastFmRadioStationKind::YourMix));
    let ids = |stations: &[LastFmRadioStation]| stations.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&first), ids(&second));
    let tracks = |stations: &[LastFmRadioStation]| {
        stations
            .iter()
            .flat_map(|s| s.tracks.iter().map(|t| format!("{}{}", t.artist, t.title)))
            .collect::<Vec<_>>()
    };
    assert_eq!(tracks(&first), tracks(&second));
}

#[tokio::test]
async fn learned_profile_applies_decay_aliases_denylist_discovery_merge_and_spacing() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            minimum_plays: 3,
            radio_track_count: 10,
            discovery_percent: 50,
            discovery_stations: vec![station(("fusion", "Fusion", &["rock", "idm"]))],
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    for index in 0..3 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: "Fresh".into(),
                title: format!("Fresh Seed {index}"),
                genre: Some("Electronica".into()),
                hearted: index == 0,
                played_at_utc: Utc::now() - TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    for index in 0..9 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: "Repeated Old".into(),
                title: format!("Old Seed {index}"),
                genre: Some("Rock".into()),
                played_at_utc: Utc::now() - TimeDelta::days(80) + TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    fixture.state.reject_track(
        "alice",
        &LastFmRadioTrack {
            artist: "rock Artist 0".into(),
            title: "rock-0".into(),
            ..Default::default()
        },
        None,
    );

    let stations = build(&fixture.service).await;
    let mixes: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::YourMix)
        .collect();
    assert_eq!(mixes.len(), 1);
    let familiar_keys: Vec<String> = fixture
        .state
        .get_user("alice")
        .plays
        .iter()
        .map(|play| seed_normalizer::track_key(&play.artist, &play.title))
        .collect();
    let familiar = keys(mixes[0])
        .iter()
        .filter(|key| familiar_keys.contains(key))
        .count();
    assert!((1..=5).contains(&familiar), "{familiar} familiar tracks");

    let fresh: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::Artist && s.name == "Fresh Radio")
        .collect();
    assert_eq!(fresh.len(), 1);
    assert!(fresh[0].tracks.len() >= 5);
    assert!(
        stations
            .iter()
            .any(|s| s.kind == LastFmRadioStationKind::Genre
                && s.seeds.iter().any(|seed| seed == "electronic"))
    );
    assert!(
        !stations
            .iter()
            .flat_map(|s| &s.seeds)
            .any(|seed| seed == "seen live")
    );

    let pinned: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::Pinned)
        .collect();
    assert_eq!(pinned.len(), 1);
    assert!(pinned[0].tracks.iter().any(|t| t.title.starts_with("rock-")));
    assert!(pinned[0].tracks.iter().any(|t| t.title.starts_with("idm-")));
    assert!(!pinned[0].tracks.iter().any(|t| t.title == "rock-0"));
    assert_eq!(
        pinned[0].tracks.len() as i32,
        fixture.settings.current().last_fm.effective_radio_track_count()
    );
    for station in &stations {
        let mut distinct = keys(station);
        distinct.sort();
        distinct.dedup();
        assert_eq!(station.tracks.len(), distinct.len(), "{}", station.name);
        assert!(
            !station
                .tracks
                .windows(2)
                .any(|pair| dotnet::eq_ignore_case(&pair[0].artist, &pair[1].artist)),
            "{}",
            station.name
        );
    }
}

#[tokio::test]
async fn refresh_draws_a_different_snapshot_and_rotates_away_from_the_previous_one() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            radio_track_count: 10,
            enable_personalized_stations: false,
            discovery_stations: vec![station(("fusion", "Fusion", &["rock", "idm"]))],
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;

    // Two fresh builds from different draws over the same 40 candidates.
    fixture.service.set_randomizer(seeded_randomizer(1));
    let first = build(&fixture.service).await;
    assert_eq!(first.len(), 1);
    fixture.service.set_randomizer(seeded_randomizer(2));
    let alternative = build(&fixture.service).await;
    assert_eq!(alternative.len(), 1);
    assert_eq!(
        first[0].tracks.len() as i32,
        fixture.settings.current().last_fm.effective_radio_track_count()
    );
    assert_ne!(keys(&first[0]), keys(&alternative[0]));

    // Once the first snapshot is installed, a refresh mostly leaves it behind: ten of forty
    // candidates carry a third of their weight, so the expected carry-over is about one track.
    fixture.state.replace_stations("alice", &first);
    fixture.service.set_randomizer(seeded_randomizer(3));
    let refreshed = build(&fixture.service).await;
    assert_eq!(refreshed.len(), 1);
    let before = keys(&first[0]);
    let carried_over = keys(&refreshed[0])
        .iter()
        .filter(|key| before.contains(key))
        .count();
    assert!(carried_over <= 4, "{carried_over} carried over");
}

#[tokio::test]
async fn shaping_leads_with_the_provider_rank_and_pushes_the_previous_snapshot_behind_new_tracks() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            radio_track_count: 10,
            enable_personalized_stations: false,
            discovery_stations: vec![station(("fusion", "Fusion", &["rock", "idm"]))],
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    fixture.service.set_randomizer(flat_random());
    let rank = |track: &LastFmRadioTrack| -> i32 { track.title.rsplit('-').next().unwrap().parse().unwrap() };

    // Both tags answer rank 0..19 at match 1.0. With the draw flattened, the ten picks are the
    // five best-ranked of each tag, and rank 0 opens the station.
    let first = build(&fixture.service).await;
    assert_eq!(first.len(), 1);
    assert_eq!(rank(&first[0].tracks[0]), 0);
    assert!(first[0].tracks.iter().all(|track| (0..=4).contains(&rank(track))));

    // Installed as the previous snapshot, those ten keep a third of their weight, which is less
    // than any fresh candidate down to rank 19 carries. A refresh therefore continues down the
    // ranking instead of restating the top.
    fixture.state.replace_stations("alice", &first);
    let refreshed = build(&fixture.service).await;
    assert_eq!(refreshed.len(), 1);
    assert!(
        refreshed[0]
            .tracks
            .iter()
            .all(|track| (5..=9).contains(&rank(track)))
    );
}

#[tokio::test]
async fn radio_completions_count_less_than_chosen_plays_for_learning_and_seeding() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            minimum_plays: 5,
            radio_track_count: 10,
            enable_discovery_stations: false,
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    fixture.service.set_randomizer(seeded_randomizer(5));

    // Twelve tracks the radio played to the end weigh 4.8 against a threshold of five: not
    // enough to call the profile learned on the radio's own output.
    for index in 0..12 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: format!("Radio Artist {index}"),
                title: format!("Served {index}"),
                source: "internet-radio".into(),
                played_at_utc: Utc::now() - TimeDelta::minutes(index),
                ..Default::default()
            },
        );
    }
    assert!(
        build(&fixture.service)
            .await
            .iter()
            .any(|s| s.kind == LastFmRadioStationKind::Starter)
    );

    // One play the listener chose tips it, and although it is the oldest play it is the
    // strongest seed, so the mix is seeded from it first.
    record(
        &fixture.state,
        LastFmRadioPlay {
            artist: "Chosen Artist".into(),
            title: "Chosen Track".into(),
            source: "scrobble".into(),
            played_at_utc: Utc::now() - TimeDelta::hours(2),
            ..Default::default()
        },
    );
    let stations = build(&fixture.service).await;
    let mixes: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::YourMix)
        .collect();
    assert_eq!(mixes.len(), 1);
    assert_eq!(mixes[0].seeds[0], "Chosen Artist");
}

#[tokio::test]
async fn mix_spreads_across_seeds_instead_of_letting_the_first_seed_fill_it() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            minimum_plays: 3,
            radio_track_count: 12,
            discovery_percent: 100,
            enable_discovery_stations: false,
            ..Default::default()
        },
        SeedSpecificHandler,
    )
    .await;
    for index in 0..6 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: format!("Seed Artist {index}"),
                title: format!("Seed {index}"),
                played_at_utc: Utc::now() - TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    fixture.service.set_randomizer(flat_random());

    // Every seed answers fifteen neighbors at the same match scores, and the newest seed
    // carries the most affinity. Without a share per source the flat draw would take the newest
    // seed's list and most of the second's; with it, no seed holds more than a share and a half
    // (three of twelve).
    let stations = build(&fixture.service).await;
    let mixes: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::YourMix)
        .collect();
    assert_eq!(mixes.len(), 1);
    assert_eq!(mixes[0].tracks.len(), 12);
    let mut per_seed: HashMap<String, usize> = HashMap::new();
    for track in &mixes[0].tracks {
        *per_seed
            .entry(track.title.split(" sim ").next().unwrap().to_string())
            .or_insert(0) += 1;
    }
    assert!(
        per_seed.values().all(|count| (1..=3).contains(count)),
        "{per_seed:?}"
    );
    assert!(per_seed.len() >= 4, "only {} seeds represented", per_seed.len());
}

#[tokio::test]
async fn personalized_and_pinned_toggles_are_independent_and_do_not_delete_state() {
    let fixture = fixture(
        LastFmSettings {
            api_key: String::new(),
            enable_personalized_stations: false,
            enable_discovery_stations: true,
            discovery_stations: vec![station(("rock", "Rock", &["rock"]))],
            ..Default::default()
        },
        Nothing,
    )
    .await;
    for index in 0..3 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: format!("A{index}"),
                title: format!("T{index}"),
                genre: Some("rock".into()),
                ..Default::default()
            },
        );
    }
    let pinned_only = build(&fixture.service).await;
    assert!(
        pinned_only
            .iter()
            .all(|s| s.kind == LastFmRadioStationKind::Pinned)
    );

    let mut changed = fixture.settings.current().as_ref().clone();
    changed.last_fm = LastFmSettings {
        api_key: String::new(),
        enable_personalized_stations: true,
        enable_discovery_stations: false,
        discovery_stations: changed.last_fm.discovery_stations.clone(),
        ..Default::default()
    };
    fixture.settings.set(changed);
    let personalized_only = build(&fixture.service).await;
    assert!(!personalized_only.is_empty());
    assert!(
        !personalized_only
            .iter()
            .any(|s| s.kind == LastFmRadioStationKind::Pinned)
    );
    assert_eq!(fixture.state.get_user("alice").plays.len(), 3);
}

/// Enough plays that the build takes the learned branch and builds all four kinds. The key has
/// to be set: without one LastFmService short-circuits every tag lookup, so the discovery and
/// genre stations have nothing to be built from.
fn learned() -> LastFmSettings {
    LastFmSettings {
        api_key: "test-key".into(),
        enable_personalized_stations: true,
        enable_discovery_stations: false,
        minimum_plays: 3,
        ..Default::default()
    }
}

fn record_twelve(state: &LastFmRadioStateStore) {
    for index in 0..12 {
        record(
            state,
            LastFmRadioPlay {
                artist: format!("Artist {}", index % 3),
                title: format!("T{index}"),
                genre: Some("rock".into()),
                ..Default::default()
            },
        );
    }
}

/// Each dynamic station type is configured on its own (issue #39). Before this the four types
/// shared one switch and the counts were fixed in the build, so a listener who wanted Your
/// Mix also got an artist radio per favorite band.
#[tokio::test]
async fn station_type_toggles_build_only_the_enabled_kinds() {
    let fixture = fixture(learned(), RecommendationHandler).await;
    record_twelve(&fixture.state);

    let all = build(&fixture.service).await;
    assert!(all.iter().any(|s| s.kind == LastFmRadioStationKind::Artist));
    assert!(all.iter().any(|s| s.kind == LastFmRadioStationKind::Genre));

    fixture.settings.set(AppSettings {
        last_fm: LastFmSettings {
            enable_your_mix: false,
            artist_station_count: 0,
            genre_station_count: 0,
            ..learned()
        },
        ..fixture.settings.current().as_ref().clone()
    });

    let discovery_only = build(&fixture.service).await;
    assert!(!discovery_only.iter().any(|s| matches!(
        s.kind,
        LastFmRadioStationKind::Artist
            | LastFmRadioStationKind::Genre
            | LastFmRadioStationKind::YourMix
            | LastFmRadioStationKind::Starter
    )));
}

#[tokio::test]
async fn artist_station_count_decides_how_many_artist_radios_are_built() {
    let fixture = fixture(
        LastFmSettings {
            artist_station_count: 1,
            genre_station_count: 0,
            ..learned()
        },
        RecommendationHandler,
    )
    .await;
    record_twelve(&fixture.state);

    let stations = build(&fixture.service).await;
    assert_eq!(
        stations
            .iter()
            .filter(|s| s.kind == LastFmRadioStationKind::Artist)
            .count(),
        1
    );
}

/// Switching radio off builds nothing, and a canceled caller sees the cancellation rather than a
/// partial build.
#[tokio::test]
async fn radio_off_builds_nothing_and_a_canceled_caller_is_told() {
    let fixture = fixture(
        LastFmSettings {
            enable_radio: false,
            ..learned()
        },
        RecommendationHandler,
    )
    .await;
    record_twelve(&fixture.state);
    assert!(build(&fixture.service).await.is_empty());

    fixture.settings.set(AppSettings {
        last_fm: learned(),
        ..Default::default()
    });
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = fixture
        .service
        .build("alice", &cancelled)
        .await
        .expect_err("the caller cancelled");
    assert!(error.is::<OperationCanceled>());
}

// ---- Group L: one-star bans, spacing, the bookkeeping tags and stations from every source ----

#[tokio::test]
async fn one_star_keeps_a_song_off_every_station_and_any_other_rating_lifts_it() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            minimum_plays: 3,
            radio_track_count: 10,
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    for index in 0..3 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: "Fresh".into(),
                title: format!("Fresh Seed {index}"),
                genre: Some("Electronica".into()),
                played_at_utc: Utc::now() - TimeDelta::days(20) - TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    assert!(
        fixture
            .state
            .set_radio_ban("alice", "id-0", "Similar Artist 0", "similar-0", true)
    );
    assert!(
        fixture
            .state
            .radio_ban_keys("alice")
            .contains(&seed_normalizer::track_key("Similar Artist 0", "similar-0"))
    );
    let stations = build(&fixture.service).await;
    let tracks: Vec<&LastFmRadioTrack> = stations.iter().flat_map(|station| &station.tracks).collect();
    assert!(!tracks.is_empty());
    assert!(
        !tracks
            .iter()
            .any(|track| track.title == "similar-0" && track.artist == "Similar Artist 0")
    );

    assert!(
        fixture
            .state
            .set_radio_ban("alice", "id-0", "Similar Artist 0", "similar-0", false)
    );
    assert!(fixture.state.get_user("alice").radio_bans.is_empty());
    assert!(
        !fixture
            .state
            .set_radio_ban("alice", "id-0", "Similar Artist 0", "similar-0", false)
    );
}

#[tokio::test]
async fn one_star_pulls_the_song_from_the_stations_it_is_on_now() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            ..Default::default()
        },
        Nothing,
    )
    .await;
    let track = |artist: &str, title: &str| LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    };
    fixture.state.replace_stations(
        "alice",
        &[LastFmRadioStation {
            id: "s1".into(),
            key: "your-mix".into(),
            name: "Your Radio".into(),
            owner: "alice".into(),
            tracks: vec![track("A", "Keep"), track("B", "Gone")],
            ..Default::default()
        }],
    );
    assert!(fixture.state.set_radio_ban("alice", "id-b", "B", "Gone", true));
    let user = fixture.state.get_user("alice");
    assert_eq!(user.stations.len(), 1);
    let titles: Vec<&str> = user.stations[0].tracks.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["Keep"]);
}

#[tokio::test]
async fn artist_station_spreads_its_artists_across_the_first_five_songs() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            minimum_plays: 3,
            radio_track_count: 10,
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    for index in 0..3 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: "Fresh".into(),
                title: format!("Fresh Seed {index}"),
                genre: Some("Electronica".into()),
                played_at_utc: Utc::now() - TimeDelta::days(20) - TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    let stations = build(&fixture.service).await;
    let fresh: Vec<&LastFmRadioStation> = stations
        .iter()
        .filter(|station| station.kind == LastFmRadioStationKind::Artist && station.name == "Fresh Radio")
        .collect();
    assert_eq!(fresh.len(), 1);
    let artists: std::collections::HashSet<String> = fresh[0]
        .tracks
        .iter()
        .take(5)
        .map(|track| octo_core::common::dotnet::to_lower_invariant(&track.artist))
        .collect();
    assert_eq!(artists.len(), 5);
}

#[test]
fn bookkeeping_tags_are_not_genres_in_either_spelling() {
    use octo_core::last_fm::last_fm_radio_recommendation_service::canonical_tag;
    for tag in ["favourites", "Favourite Songs", "favorites", "favorite song"] {
        assert_eq!(canonical_tag(tag), "", "{tag}");
    }
}

#[test]
fn a_seeds_lists_from_every_source_are_one_seed() {
    use octo_core::last_fm::last_fm_radio_recommendation_service::seed_group;
    for (source, group) in [
        ("youtube-music:tag:trip hop", "tag:trip hop"),
        ("tag:trip hop", "tag:trip hop"),
        ("lastfm:massive attack - teardrop", "massive attack - teardrop"),
        ("listenbrainz:artist:Portishead", "artist:Portishead"),
        ("history", "history"),
        ("artist:Sunn O))): live", "artist:Sunn O))): live"),
    ] {
        assert_eq!(seed_group(source), group, "{source}");
    }
}

/// A radio source for station tests: always the same answer, or one that never comes.
struct StationSource {
    provider: &'static str,
    r#match: octo_core::radio::RadioMatch,
    count: usize,
    score: f64,
    hangs: bool,
    asked: parking_lot::Mutex<Vec<octo_core::radio::RadioSeed>>,
}

impl StationSource {
    fn new(provider: &'static str, count: usize) -> Arc<StationSource> {
        Arc::new(StationSource {
            provider,
            r#match: octo_core::radio::RadioMatch::Song,
            count,
            score: 1.0,
            hangs: false,
            asked: parking_lot::Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl crate::services::radio::IRadioSource for StationSource {
    fn provider(&self) -> &'static str {
        self.provider
    }

    fn available(&self) -> bool {
        true
    }

    async fn songs_like(
        &self,
        seed: &octo_core::radio::RadioSeed,
        _count: i32,
        _auth: &indexmap::IndexMap<String, String>,
    ) -> anyhow::Result<octo_core::radio::RadioAnswer> {
        self.asked.lock().push(seed.clone());
        if self.hangs {
            std::future::pending::<()>().await;
        }
        let provider = self.provider;
        Ok(octo_core::radio::RadioAnswer::new(
            provider,
            self.r#match,
            (0..self.count)
                .map(|i| {
                    octo_core::last_fm::SimilarTrack::new(
                        format!("{provider} artist {i}"),
                        format!("{provider} song {i} for {}", seed.title),
                        self.score,
                    )
                    .with_duration(Some(180))
                    .with_provider(Some(provider.to_string()))
                })
                .collect(),
            Vec::new(),
        ))
    }
}

/// A learned listener, and Last.fm (the fake) beside the given sources.
async fn with_sources(extra: Vec<Arc<dyn crate::services::radio::IRadioSource>>) -> Fixture {
    let mut fixture = fixture(learned(), RecommendationHandler).await;
    record_twelve(&fixture.state);
    let last_fm = Arc::new(LastFmService::with_base_url(
        fixture.settings.clone(),
        &format!("{}/", fixture._server.uri()),
    ));
    let mut sources: Vec<Arc<dyn crate::services::radio::IRadioSource>> = vec![Arc::new(
        crate::services::radio::LastFmRadioSource::new(last_fm.clone()),
    )];
    sources.extend(extra);
    let set = Arc::new(crate::services::radio::RadioSourceSet::new(
        sources,
        fixture.settings.clone(),
        None,
    ));
    set.set_source_timeout(std::time::Duration::from_millis(200));
    fixture.service = LastFmRadioRecommendationService::new(
        last_fm,
        fixture.state.clone(),
        fixture.settings.clone(),
        Some(set),
    );
    fixture
}

#[tokio::test]
async fn your_mix_draws_on_every_source_and_remembers_which_suggested_each_song() {
    use octo_core::radio::radio_provider::{HISTORY, LAST_FM, LISTEN_BRAINZ, YOU_TUBE_MUSIC};
    let listen_brainz = Arc::new(StationSource {
        // A count of shared sessions, as ListenBrainz scores: ranked by position, it cannot swamp
        // the rest.
        score: 216.0,
        ..Arc::into_inner(StationSource::new(LISTEN_BRAINZ, 15)).expect("one owner")
    });
    let fixture = with_sources(vec![StationSource::new(YOU_TUBE_MUSIC, 15), listen_brainz]).await;
    let stations = build(&fixture.service).await;
    let mix = stations
        .iter()
        .find(|station| station.kind == LastFmRadioStationKind::YourMix)
        .expect("Your Radio");
    // This fake's Last.fm answers every seed with the same 15 songs, so whether one of them is
    // drawn is chance; the sources that answer each seed differently always are.
    let sources: std::collections::HashSet<&str> =
        mix.tracks.iter().map(|track| track.source.as_str()).collect();
    assert!(sources.contains(YOU_TUBE_MUSIC), "{sources:?}");
    assert!(sources.contains(LISTEN_BRAINZ), "{sources:?}");
    assert!(
        sources
            .iter()
            .all(|source| [LAST_FM, YOU_TUBE_MUSIC, LISTEN_BRAINZ, HISTORY].contains(source))
    );
    for track in mix
        .tracks
        .iter()
        .filter(|track| track.title.starts_with('T') && track.artist.starts_with("Artist "))
    {
        assert_eq!(track.source, HISTORY);
    }
    assert!(
        mix.tracks
            .iter()
            .filter(|track| track.source == LISTEN_BRAINZ)
            .count()
            < mix.tracks.len() / 2,
        "ListenBrainz's large scores do not take over the mix"
    );
}

#[tokio::test]
async fn a_source_that_never_answers_does_not_stop_the_stations() {
    use octo_core::radio::radio_provider::YOU_TUBE_MUSIC;
    let hanging = Arc::new(StationSource {
        hangs: true,
        ..Arc::into_inner(StationSource::new(YOU_TUBE_MUSIC, 10)).expect("one owner")
    });
    let fixture = with_sources(vec![hanging]).await;
    let started = std::time::Instant::now();
    let stations = build(&fixture.service).await;
    assert!(
        stations
            .iter()
            .any(|station| station.kind == LastFmRadioStationKind::YourMix && !station.tracks.is_empty())
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_artist_station_asks_the_other_sources_not_last_fm_again() {
    use octo_core::radio::radio_provider::{LAST_FM, YOU_TUBE_MUSIC};
    let last_fm = StationSource::new(LAST_FM, 5);
    let ytm = StationSource::new(YOU_TUBE_MUSIC, 10);
    let fixture = with_sources(vec![last_fm.clone(), ytm.clone()]).await;
    build(&fixture.service).await;
    // An artist station's seed has no title; Last.fm answered it with its own top tracks.
    assert!(ytm.asked.lock().iter().any(|seed| seed.title.is_empty()));
    assert!(!last_fm.asked.lock().iter().any(|seed| seed.title.is_empty()));
}

/// Last.fm for a listener of "Artist 0", "Artist 1" and "Artist 2": each of them points to new
/// names and to one another, and the new names point one step further. `walk` false answers no
/// similar artists at all.
#[derive(Clone)]
struct FrontierHandler {
    walk: bool,
}

impl Respond for FrontierHandler {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query = query(request);
        let get = |name: &str| query.get(name).cloned().unwrap_or_default();
        let artist = get("artist");
        let similar = |names: &[(&str, f64)]| {
            json!({"similarartists": {"artist": names.iter().map(|(name, m)| json!({
                "name": name, "match": m
            })).collect::<Vec<_>>()}})
        };
        let body = match get("method").as_str() {
            "artist.gettoptags" => json!({"toptags": {"tag": [{"name": "rock"}]}}),
            "artist.getsimilar" if !self.walk => json!({}),
            "artist.getsimilar" => match artist.as_str() {
                "Artist 0" => similar(&[("Artist 1", 1.0), ("Shared Find", 0.6), ("Solo Find", 0.5)]),
                "Artist 1" => similar(&[("Artist 2", 1.0), ("Shared Find", 0.6)]),
                "Artist 2" => similar(&[("Artist 0", 1.0), ("Other Find", 0.3)]),
                _ => similar(&[("Far Find", 0.8)]),
            },
            "artist.gettoptracks" => json!({"toptracks": {"track": (0..3).map(|index| json!({
                "name": format!("{artist} hit {index}")
            })).collect::<Vec<_>>()}}),
            "tag.gettoptracks" => json!({"tracks": {"track": (0..20).map(|index| json!({
                "name": format!("{}-{index}", get("tag")), "duration": 180000,
                "artist": {"name": if index < 6 { format!("Artist {}", index % 3) } else { format!("Chart Artist {index}") }}
            })).collect::<Vec<_>>()}}),
            _ => json!({}),
        };
        ResponseTemplate::new(200).set_body_string(body.to_string())
    }
}

fn discovery_of(stations: &[LastFmRadioStation]) -> Option<&LastFmRadioStation> {
    stations
        .iter()
        .find(|s| s.kind == LastFmRadioStationKind::Discovery)
}

/// Discovery Radio comes from the similar-artist walk: only artists the listener has never
/// played, the one two favorites point to among them, the step past them too, and no more than
/// two songs of any one artist.
#[tokio::test]
async fn discovery_mix_draws_artists_the_listener_does_not_know_from_the_similar_artist_walk() {
    let fixture = fixture(
        LastFmSettings {
            radio_track_count: 10,
            ..learned()
        },
        FrontierHandler { walk: true },
    )
    .await;
    record_twelve(&fixture.state);

    let stations = build(&fixture.service).await;
    let discovery = discovery_of(&stations).expect("a Discovery Radio");
    let artists: Vec<&str> = discovery
        .tracks
        .iter()
        .chain(&discovery.reserve)
        .map(|t| t.artist.as_str())
        .collect();
    assert!(!artists.is_empty());
    assert!(artists.iter().all(|a| !a.starts_with("Artist ")), "{artists:?}");
    for find in ["Shared Find", "Solo Find", "Other Find", "Far Find"] {
        assert!(artists.contains(&find), "{find} missing from {artists:?}");
    }
    for artist in &artists {
        assert!(
            artists.iter().filter(|a| *a == artist).count() <= 2,
            "{artist} more than twice in {artists:?}"
        );
    }
}

/// When the walk finds fewer than five songs, the listener's tag charts make Discovery Radio as
/// before.
#[tokio::test]
async fn discovery_mix_falls_back_to_the_tag_charts_when_the_walk_finds_too_little() {
    let fixture = fixture(
        LastFmSettings {
            radio_track_count: 10,
            ..learned()
        },
        FrontierHandler { walk: false },
    )
    .await;
    record_twelve(&fixture.state);

    let stations = build(&fixture.service).await;
    let discovery = discovery_of(&stations).expect("a Discovery Radio");
    assert!(
        discovery.tracks.iter().all(|t| t.title.starts_with("rock-")),
        "{:?}",
        discovery.tracks
    );
    assert_eq!(discovery.seeds, ["rock"]);
}

/// A genre station leads with the genre's names the listener does not know: its own artists
/// keep a third of their weight, so with a flat draw the chart's other artists come first.
#[tokio::test]
async fn a_genre_station_leads_with_artists_the_listener_does_not_know() {
    let fixture = fixture(
        LastFmSettings {
            radio_track_count: 10,
            ..learned()
        },
        FrontierHandler { walk: true },
    )
    .await;
    record_twelve(&fixture.state);
    fixture.service.set_randomizer(flat_random());

    let stations = build(&fixture.service).await;
    let genre = stations
        .iter()
        .find(|s| s.kind == LastFmRadioStationKind::Genre)
        .expect("a genre station");
    let first: Vec<&str> = genre.tracks.iter().take(5).map(|t| t.artist.as_str()).collect();
    assert!(first.iter().all(|a| a.starts_with("Chart Artist")), "{first:?}");
}

/// The walk's similar-artist calls never answer in time; everything else answers at once.
#[derive(Clone)]
struct StuckWalk;

impl Respond for StuckWalk {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query = query(request);
        let walking = query.get("method").map(String::as_str) == Some("artist.getsimilar")
            && query.get("limit").map(String::as_str) == Some(&FRONTIER_NEIGHBOURS.to_string());
        if walking {
            return ResponseTemplate::new(200)
                .set_body_string("{}")
                .set_delay(std::time::Duration::from_secs(30));
        }
        FrontierHandler { walk: true }.respond(request)
    }
}

/// A slow similar-artist walk costs Discovery Radio its own few seconds, never the artist and
/// genre stations, which are built first.
#[tokio::test]
async fn a_slow_discovery_walk_never_starves_the_artist_and_genre_stations() {
    let fixture = fixture(
        LastFmSettings {
            radio_track_count: 10,
            ..learned()
        },
        StuckWalk,
    )
    .await;
    record_twelve(&fixture.state);
    let started = std::time::Instant::now();

    let stations = build(&fixture.service).await;

    assert!(
        started.elapsed() < DISCOVERY_WALK_BUDGET + std::time::Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert!(stations.iter().any(|s| s.kind == LastFmRadioStationKind::Artist));
    assert!(stations.iter().any(|s| s.kind == LastFmRadioStationKind::Genre));
    // The walk found nothing in its time, so the tag charts make the mix.
    assert!(discovery_of(&stations).is_some());
    assert_eq!(DISCOVERY_WALK_BUDGET, std::time::Duration::from_secs(8));
}

fn radio_track(artist: &str, title: &str) -> LastFmRadioTrack {
    LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    }
}

fn titles(tracks: &[LastFmRadioTrack]) -> Vec<&str> {
    tracks.iter().map(|track| track.title.as_str()).collect()
}

/// A rock station of two tracks and one in reserve, all from the tag chart the fake answers.
fn install_rock(state: &LastFmRadioStateStore) -> LastFmRadioStation {
    let station = LastFmRadioStation {
        id: octo_core::last_fm::last_fm_radio_state_store::station_id("alice", "genre-rock"),
        key: "genre-rock".into(),
        name: "Rock Radio".into(),
        owner: "alice".into(),
        kind: LastFmRadioStationKind::Genre,
        personalized: true,
        seeds: vec!["rock".into()],
        tracks: vec![
            radio_track("rock Artist 0", "rock-0"),
            radio_track("rock Artist 1", "rock-1"),
        ],
        reserve: vec![radio_track("rock Artist 2", "rock-2")],
        ..Default::default()
    };
    state.replace_stations("alice", std::slice::from_ref(&station));
    station
}

/// A station is not a fixed list: its own tracks and then its reserve come first, without the
/// ones left out; after them come new songs of its kind, none of them left out, and the next
/// page holds none of the page before.
#[tokio::test]
async fn a_station_goes_on_past_its_snapshot_with_new_songs_of_its_kind() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            radio_track_count: 10,
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    let station = install_rock(&fixture.state);
    let token = CancellationToken::new();
    let next = |exclude: Vec<LastFmRadioTrack>, count: usize| {
        let (service, id, token) = (&fixture.service, station.id.clone(), token.clone());
        async move {
            service
                .next_for_station("alice", &id, &exclude, count, &token)
                .await
        }
    };

    assert_eq!(titles(&next(Vec::new(), 2).await), ["rock-0", "rock-1"]);
    assert_eq!(
        titles(&next(vec![radio_track("ROCK ARTIST 0", "Rock-0")], 2).await),
        ["rock-1", "rock-2"]
    );

    let mut after: Vec<LastFmRadioTrack> = station.tracks.iter().chain(&station.reserve).cloned().collect();
    let page = next(after.clone(), 4).await;
    assert_eq!(page.len(), 4, "{:?}", titles(&page));
    for track in &page {
        assert!(track.title.starts_with("rock-"), "{}", track.title);
        assert!(!["rock-0", "rock-1", "rock-2"].contains(&track.title.as_str()));
    }
    after.extend(page.iter().cloned());
    let second = next(after.clone(), 4).await;
    assert_eq!(second.len(), 4, "{:?}", titles(&second));
    for track in &second {
        assert!(
            !after.iter().any(|earlier| earlier.title == track.title),
            "{}",
            track.title
        );
    }

    // Another listener's station, or none at all, gives nothing.
    assert!(
        fixture
            .service
            .next_for_station("bob", &station.id, &[], 5, &token)
            .await
            .is_empty()
    );
    assert!(
        fixture
            .service
            .next_for_station("alice", "or-nothing", &[], 5, &token)
            .await
            .is_empty()
    );
}

/// What a station served is remembered by id for its listener alone.
#[tokio::test]
async fn served_songs_are_remembered_by_id_per_listener() {
    let fixture = fixture(LastFmSettings::default(), Nothing).await;
    let song = Song {
        id: "local-1".into(),
        artist: "Artist".into(),
        title: "Title".into(),
        album: "Album".into(),
        is_local: true,
        ..Default::default()
    };
    fixture.service.remember_served("Alice", [&song]);
    let track = fixture
        .service
        .served_track("alice", "local-1")
        .expect("remembered");
    assert_eq!((track.artist.as_str(), track.title.as_str()), ("Artist", "Title"));
    assert_eq!(track.album.as_deref(), Some("Album"));
    assert!(fixture.service.served_track("bob", "local-1").is_none());
}

/// A song the station served is not served again by a later ask that does not leave it out
/// itself, for a day; then it may come back.
#[tokio::test]
async fn a_served_song_stays_out_of_the_station_for_a_day() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            radio_track_count: 10,
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    let station = install_rock(&fixture.state);
    let now = Arc::new(parking_lot::Mutex::new(Utc::now()));
    let clock_now = Arc::clone(&now);
    fixture
        .service
        .set_clock(octo_core::common::Clock::new(move || *clock_now.lock()));
    let token = CancellationToken::new();

    fixture
        .service
        .record_station_served("alice", &station.id, [radio_track("rock Artist 0", "rock-0")]);
    let next = fixture
        .service
        .next_for_station("alice", &station.id, &[], 2, &token)
        .await;
    assert_eq!(titles(&next), ["rock-1", "rock-2"]);
    // Another listener's station history is their own.
    assert!(fixture.service.station_served("bob", &station.id).is_empty());

    *now.lock() += STATION_MEMORY + TimeDelta::hours(1);
    assert!(fixture.service.station_served("alice", &station.id).is_empty());
    let next = fixture
        .service
        .next_for_station("alice", &station.id, &[], 2, &token)
        .await;
    assert_eq!(titles(&next), ["rock-0", "rock-1"]);
}

/// A station remembers at most a thousand served songs: the oldest is forgotten first.
#[tokio::test]
async fn a_station_remembers_its_newest_thousand_songs() {
    let fixture = fixture(LastFmSettings::default(), Nothing).await;
    let station = install_rock(&fixture.state);
    fixture.service.record_station_served(
        "alice",
        &station.id,
        (0..=STATION_MEMORY_SONGS)
            .map(|index| radio_track(&format!("Artist {index}"), &format!("Song {index}"))),
    );
    let served = fixture.service.station_served("alice", &station.id);
    assert_eq!(served.len(), STATION_MEMORY_SONGS);
    assert_eq!(served[0].title, "Song 1");
    assert_eq!(
        served[STATION_MEMORY_SONGS - 1].title,
        format!("Song {STATION_MEMORY_SONGS}")
    );
    // Served again, a song moves to the newest end instead of being kept twice.
    fixture
        .service
        .record_station_served("alice", &station.id, [radio_track("Artist 1", "Song 1")]);
    let served = fixture.service.station_served("alice", &station.id);
    assert_eq!(served.len(), STATION_MEMORY_SONGS);
    assert_eq!(served[0].title, "Song 2");
    assert_eq!(served[STATION_MEMORY_SONGS - 1].title, "Song 1");
}

/// With nothing new to be found anywhere, the songs served longest ago come round again rather
/// than none at all, never one the caller left out.
#[tokio::test]
async fn with_nothing_new_the_oldest_served_songs_come_round_again() {
    let fixture = fixture(LastFmSettings::default(), Nothing).await;
    let station = install_rock(&fixture.state);
    let all: Vec<LastFmRadioTrack> = station.tracks.iter().chain(&station.reserve).cloned().collect();
    fixture
        .service
        .record_station_served("alice", &station.id, all.iter().rev().cloned());
    let next = fixture
        .service
        .next_for_station(
            "alice",
            &station.id,
            &[radio_track("rock Artist 1", "rock-1")],
            5,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(titles(&next), ["rock-2", "rock-0"]);
}

/// The listener's own station and the discovery one are named Radio, and keep the ids they had
/// when they were named Mix: an id comes from the station's key, never its name.
#[tokio::test]
async fn your_radio_and_discovery_radio_keep_their_ids() {
    let fixture = fixture(
        LastFmSettings {
            radio_track_count: 10,
            ..learned()
        },
        FrontierHandler { walk: true },
    )
    .await;
    record_twelve(&fixture.state);

    let stations = build(&fixture.service).await;
    let mine = stations
        .iter()
        .find(|s| s.kind == LastFmRadioStationKind::YourMix)
        .expect("the listener's own station");
    assert_eq!(mine.name, "Your Radio");
    assert_eq!(mine.key, "your-mix");
    assert_eq!(
        mine.id,
        octo_core::last_fm::last_fm_radio_state_store::station_id("alice", "your-mix")
    );
    let discovery = discovery_of(&stations).expect("a Discovery Radio");
    assert_eq!(discovery.name, "Discovery Radio");
    assert_eq!(discovery.key, "discovery");
    assert_eq!(
        discovery.id,
        octo_core::last_fm::last_fm_radio_state_store::station_id("alice", "discovery")
    );
}

/// [`RecommendationHandler`] answering only after `delay`, counting what it is asked.
#[derive(Clone)]
struct SlowCounted {
    delay: std::time::Duration,
    asked: Arc<std::sync::atomic::AtomicUsize>,
}

impl Respond for SlowCounted {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        RecommendationHandler.respond(request).set_delay(self.delay)
    }
}

/// The rock station with everything it holds left out, so an ask needs a walk on, over a
/// Last.fm that answers after `delay`; and the count of Last.fm asks.
async fn slow_rock(
    delay: std::time::Duration,
) -> (
    Fixture,
    LastFmRadioStation,
    Vec<LastFmRadioTrack>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            radio_track_count: 10,
            ..Default::default()
        },
        SlowCounted {
            delay,
            asked: Arc::clone(&asked),
        },
    )
    .await;
    let station = install_rock(&fixture.state);
    let held: Vec<LastFmRadioTrack> = station.tracks.iter().chain(&station.reserve).cloned().collect();
    (fixture, station, held, asked)
}

/// A slow Last.fm never holds an ask past its wait: the walk goes on in the background and its
/// songs are there for the next ask at once. Two asks at the same time walk once.
#[tokio::test]
async fn a_slow_walk_never_holds_an_ask_past_its_wait_and_two_asks_walk_once() {
    let delay = std::time::Duration::from_secs(2);
    let wait = std::time::Duration::from_millis(300);
    let (fixture, station, held, asked) = slow_rock(delay).await;
    let token = CancellationToken::new();
    let ask = |wait| {
        fixture
            .service
            .next_for_station_within("alice", &station.id, &held, 4, false, wait, &token)
    };

    let started = std::time::Instant::now();
    let (first, second) = tokio::join!(ask(wait), ask(wait));
    assert!(started.elapsed() < delay, "{:?}", started.elapsed());
    assert!(first.is_empty() && second.is_empty(), "{first:?} {second:?}");
    fixture.service.settled().await;
    let two_asks = asked.load(std::sync::atomic::Ordering::SeqCst);
    assert!(two_asks > 0);

    let started = std::time::Instant::now();
    let next = ask(std::time::Duration::ZERO).await;
    assert!(started.elapsed() < delay, "{:?}", started.elapsed());
    assert_eq!(next.len(), 4, "{:?}", titles(&next));
    for track in &next {
        assert!(!["rock-0", "rock-1", "rock-2"].contains(&track.title.as_str()));
    }

    // One ask alone asks Last.fm just as often.
    let (alone, station, held, asked) = slow_rock(delay).await;
    let _ = alone
        .service
        .next_for_station_within("alice", &station.id, &held, 4, false, wait, &token)
        .await;
    alone.service.settled().await;
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), two_asks);
}

/// A zero wait never waits at all: it answers with what the station holds and starts the walk.
#[tokio::test]
async fn a_zero_wait_answers_with_what_the_station_holds() {
    let delay = std::time::Duration::from_secs(2);
    let (fixture, station, _, asked) = slow_rock(delay).await;
    let started = std::time::Instant::now();
    let picks = fixture
        .service
        .next_for_station_within(
            "alice",
            &station.id,
            &[],
            10,
            true,
            std::time::Duration::ZERO,
            &CancellationToken::new(),
        )
        .await;
    assert!(started.elapsed() < delay, "{:?}", started.elapsed());
    assert_eq!(titles(&picks), ["rock-0", "rock-1", "rock-2"]);
    fixture.service.settled().await;
    assert!(asked.load(std::sync::atomic::Ordering::SeqCst) > 0);
}

/// A station track that went out as a song of another id stays out by that id as well as by its
/// own artist and title.
#[tokio::test]
async fn a_song_served_as_another_id_stays_out_by_that_id() {
    let fixture = fixture(LastFmSettings::default(), Nothing).await;
    let station = install_rock(&fixture.state);
    let id = fixture
        .state
        .find_station("alice", &station.id)
        .and_then(|station| station.tracks[0].resolved_id.clone())
        .expect("the station's first track has an id");
    // Spelled otherwise, served as the station track's own id.
    fixture.service.record_station_served_as(
        "alice",
        &station.id,
        [(radio_track("Rock Artist Zero", "Rock Zero"), Some(id))],
    );
    let next = fixture
        .service
        .next_for_station("alice", &station.id, &[], 2, &CancellationToken::new())
        .await;
    assert_eq!(titles(&next), ["rock-1", "rock-2"]);
}

/// Served again, an id counts as served now, and each listener keeps their own newest ids: one
/// listener's many songs never push out another's.
#[tokio::test]
async fn served_ids_renew_when_served_again_and_are_kept_per_listener() {
    let fixture = fixture(LastFmSettings::default(), Nothing).await;
    let song = |id: String| Song {
        id: id.clone(),
        artist: format!("Artist {id}"),
        title: format!("Title {id}"),
        ..Default::default()
    };
    fixture.service.remember_served("bob", [&song("bob-1".into())]);
    let first = song("id-0".into());
    fixture.service.remember_served("alice", [&first]);
    let many: Vec<Song> = (1..SERVED_KEPT)
        .map(|index| song(format!("id-{index}")))
        .collect();
    fixture.service.remember_served("alice", &many);
    fixture.service.remember_served("alice", [&first]);
    fixture
        .service
        .remember_served("alice", [&song(format!("id-{SERVED_KEPT}"))]);

    assert!(fixture.service.served_track("alice", "id-0").is_some());
    assert!(fixture.service.served_track("alice", "id-1").is_none());
    assert!(
        fixture
            .service
            .served_track("alice", &format!("id-{SERVED_KEPT}"))
            .is_some()
    );
    assert!(fixture.service.served_track("bob", "bob-1").is_some());
}

/// Only similar songs answer; every other method fails.
#[derive(Clone)]
struct OnlySimilarSongs;

impl Respond for OnlySimilarSongs {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if query(request).get("method").map(String::as_str) == Some("track.getsimilar") {
            RecommendationHandler.respond(request)
        } else {
            ResponseTemplate::new(500)
        }
    }
}

/// A station carried over keeps the songs it walked on to, but not one rated one star since.
#[tokio::test]
async fn a_carried_station_drops_a_walked_on_song_rated_one_star() {
    let fixture = fixture(learned(), OnlySimilarSongs).await;
    record_twelve(&fixture.state);
    let old: Vec<LastFmRadioTrack> = (0..12)
        .map(|index| LastFmRadioTrack {
            artist: "Old".into(),
            title: format!("Old {index}"),
            ..Default::default()
        })
        .collect();
    let station = create(
        "alice",
        "discovery",
        "Discovery Radio",
        LastFmRadioStationKind::Discovery,
        true,
        &["rock"],
        old,
        1,
        Utc::now() - TimeDelta::hours(13),
    );
    fixture
        .state
        .replace_stations("alice", std::slice::from_ref(&station));
    let saved = fixture.state.get_user("alice").stations[0].clone();
    let key = continuation_key("alice", &saved);
    let walked = |title: &str| LastFmRadioTrack {
        artist: "Walked".into(),
        title: title.into(),
        ..Default::default()
    };
    fixture
        .service
        .continuations
        .lock()
        .insert(key.clone(), vec![walked("Rated"), walked("Kept")]);
    assert!(fixture.state.set_radio_ban("alice", "", "Walked", "Rated", true));

    let stations = build(&fixture.service).await;

    assert!(
        stations.iter().any(|station| station.key == "discovery"),
        "Discovery Radio got no answer, so it is carried over"
    );
    let left: Vec<String> = fixture.service.continuations.lock()[&key]
        .iter()
        .map(|track| track.title.clone())
        .collect();
    assert_eq!(left, ["Kept"]);
}
