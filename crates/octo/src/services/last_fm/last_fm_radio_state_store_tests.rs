//! The Last.fm radio state store, and the fixture through the store.

use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use octo_core::models::radio::{LastFmRadioStateDocument, LastFmRadioUserState};
use octo_core::settings::{AppSettings, LastFmSettings};
use tempfile::TempDir;

use super::*;

pub(crate) fn play(artist: &str, title: &str, at: Option<DateTime<Utc>>) -> LastFmRadioPlay {
    LastFmRadioPlay {
        artist: artist.into(),
        title: title.into(),
        played_at_utc: at.unwrap_or_else(Utc::now),
        ..Default::default()
    }
}

pub(crate) fn settings_store(last_fm: LastFmSettings) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm,
        ..Default::default()
    }))
}

struct StateFixture {
    _dir: TempDir,
    path: PathBuf,
    store: LastFmRadioStateStore,
}

impl StateFixture {
    fn new(json: Option<&str>, settings: Option<LastFmSettings>) -> Self {
        let dir = TempDir::new().expect("a temp dir");
        let path = dir.path().join("state.json");
        if let Some(json) = json {
            std::fs::write(&path, json).expect("writes");
        }
        let store = LastFmRadioStateStore::new(
            path.clone(),
            settings_store(settings.unwrap_or_default()),
            Arc::new(ExternalIdRegistry::in_memory()),
        );
        StateFixture {
            _dir: dir,
            path,
            store,
        }
    }
}

fn document_with(user: LastFmRadioUserState) -> String {
    let mut document = LastFmRadioStateDocument::default();
    document.users.insert("alice".into(), user);
    octo_core::json::to_string(&document)
}

#[test]
fn missing_and_corrupt_files_recover_and_persist_atomically() {
    let fixture = StateFixture::new(Some("not json"), None);
    assert!(fixture.store.known_users().is_empty());
    assert!(fixture.store.record_play("alice", play("A", "One", None)));
    assert!(fixture.path.exists());
    assert!(!state_file::tmp_path(&fixture.path).exists());
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&fixture.path).unwrap()).unwrap();
    assert_eq!(written["Version"], CURRENT_VERSION);
}

#[test]
fn plays_are_deduplicated_and_isolated_per_user_even_when_disabled() {
    let fixture = StateFixture::new(
        None,
        Some(LastFmSettings {
            enable_radio: false,
            ..Default::default()
        }),
    );
    assert!(fixture.store.record_play("alice", play("Artist", "Title", None)));
    assert!(!fixture.store.record_play("alice", play("Artist", "Title", None)));
    assert!(fixture.store.record_play("bob", play("Artist", "Title", None)));
    assert_eq!(fixture.store.get_user("alice").plays.len(), 1);
    assert_eq!(fixture.store.get_user("bob").plays.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_do_not_lose_distinct_plays() {
    let fixture = Arc::new(StateFixture::new(None, None));
    let tasks: Vec<_> = (0..30)
        .map(|index| {
            let fixture = fixture.clone();
            tokio::task::spawn_blocking(move || {
                fixture
                    .store
                    .record_play("alice", play("Artist", &format!("Title {index}"), None))
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(fixture.store.get_user("alice").plays.len(), 30);
}

#[test]
fn load_prunes_old_and_over_bound_history() {
    let plays = (0..2100)
        .map(|index| {
            play(
                "A",
                &format!("T{index}"),
                Some(Utc::now() - TimeDelta::minutes(index)),
            )
        })
        .collect();
    let json = document_with(LastFmRadioUserState {
        username: "alice".into(),
        plays,
        ..Default::default()
    });
    let fixture = StateFixture::new(Some(&json), None);
    assert_eq!(fixture.store.get_user("alice").plays.len(), 2000);
}

#[test]
fn unsupported_version_starts_clean_and_retention_drops_expired_plays() {
    let unsupported = StateFixture::new(
        Some(r#"{"Version":99,"Users":{"alice":{"Username":"alice"}}}"#),
        None,
    );
    assert!(unsupported.store.known_users().is_empty());

    let json = document_with(LastFmRadioUserState {
        username: "alice".into(),
        plays: vec![
            play("A", "old", Some(Utc::now() - TimeDelta::days(30))),
            play("A", "new", None),
        ],
        ..Default::default()
    });
    let retained = StateFixture::new(
        Some(&json),
        Some(LastFmSettings {
            history_retention_days: 7,
            ..Default::default()
        }),
    );
    let plays = retained.store.get_user("alice").plays;
    assert_eq!(plays.len(), 1);
    assert_eq!(plays[0].title, "new");
}

#[test]
fn external_routes_are_rehydrated_after_restart() {
    let fixture = StateFixture::new(None, None);
    let station = LastFmRadioStation {
        id: station_id("alice", "mix"),
        name: "Mix".into(),
        owner: "alice".into(),
        tracks: vec![LastFmRadioTrack {
            artist: "A".into(),
            title: "T".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    fixture.store.replace_stations("alice", &[station]);
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let restarted = LastFmRadioStateStore::new(
        fixture.path.clone(),
        settings_store(LastFmSettings::default()),
        registry.clone(),
    );
    let user = restarted.get_user("alice");
    assert_eq!(user.stations.len(), 1);
    assert_eq!(user.stations[0].tracks.len(), 1);
    let track = &user.stations[0].tracks[0];
    let id = track.resolved_id.as_deref().expect("a route id");
    assert!(registry.lookup(id).is_some());
    assert_eq!(track.external_provider.as_deref(), Some("soulseek"));
}

#[test]
fn installing_identical_snapshot_preserves_created_and_changed_metadata() {
    let fixture = StateFixture::new(None, None);
    let mut station = LastFmRadioStation {
        id: station_id("alice", "mix"),
        name: "Mix".into(),
        owner: "alice".into(),
        created_utc: Utc::now() - TimeDelta::days(2),
        changed_utc: Utc::now() - TimeDelta::days(1),
        tracks: vec![LastFmRadioTrack {
            artist: "A".into(),
            title: "T".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    fixture
        .store
        .replace_stations("alice", std::slice::from_ref(&station));
    station.created_utc = Utc::now();
    station.changed_utc = Utc::now();
    fixture
        .store
        .replace_stations("alice", std::slice::from_ref(&station));
    let user = fixture.store.get_user("alice");
    assert_eq!(user.stations.len(), 1);
    let installed = &user.stations[0];
    assert_eq!(
        (station.created_utc - TimeDelta::days(2)).date_naive(),
        installed.created_utc.date_naive()
    );
    assert!(installed.changed_utc < Utc::now() - TimeDelta::hours(23));
}

#[test]
fn reject_track_removes_it_from_every_station_and_persists_cooldown() {
    let fixture = StateFixture::new(None, None);
    let bad = LastFmRadioTrack {
        artist: "Bad Artist".into(),
        title: "Bad Song".into(),
        ..Default::default()
    };
    let other = |artist: &str, title: &str| LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    };
    fixture.store.replace_stations(
        "alice",
        &[
            LastFmRadioStation {
                id: "one".into(),
                tracks: vec![bad.clone(), other("A", "T")],
                ..Default::default()
            },
            LastFmRadioStation {
                id: "two".into(),
                tracks: vec![bad.clone(), other("B", "U")],
                ..Default::default()
            },
        ],
    );

    assert_eq!(fixture.store.reject_track("alice", &bad, None), 2);
    let user = fixture.store.get_user("alice");
    assert!(
        user.stations
            .iter()
            .all(|station| station.tracks.iter().all(|track| track.title != "Bad Song"))
    );
    assert_eq!(user.unavailable_tracks.len(), 1);
    let unavailable = &user.unavailable_tracks[0];
    assert_eq!(
        seed_normalizer::track_key("Bad Artist", "Bad Song"),
        unavailable.key
    );
    assert!(unavailable.retry_after_utc > unavailable.failed_at_utc);
}

/// The store writes the document indented, `Version` then `Users`, and keeps a user's key as
/// it was written; a hand-edited key in another case is still found.
#[test]
fn the_store_keeps_keys_and_finds_them_ignoring_case() {
    let at = Utc::now();
    let json = format!(
        r#"{{"Version":1,"Users":{{"Brandon":{{"Username":"Brandon","LastSeenUtc":"{}"}}}}}}"#,
        octo_core::json::datetime::format_utc(&at)
    );
    let fixture = StateFixture::new(Some(&json), None);
    assert_eq!(fixture.store.get_user(" BRANDON ").username, "Brandon");
    fixture.store.mark_refreshing("brandon");
    let written = std::fs::read_to_string(&fixture.path).unwrap();
    assert!(
        written.starts_with("{\n  \"Version\": 1,\n  \"Users\": {\n    \"Brandon\": {"),
        "{written}"
    );
    assert!(fixture.store.reset("BRANDON"));
    assert!(fixture.store.known_users().is_empty());
}

fn one_station(owner: &str) -> LastFmRadioStation {
    LastFmRadioStation {
        id: station_id(owner, "mix"),
        name: "Mix".into(),
        owner: owner.into(),
        tracks: vec![LastFmRadioTrack {
            artist: "A".into(),
            title: "T".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn a_cleared_listener_keeps_the_entry_but_nothing_learned() {
    let fixture = StateFixture::new(None, None);
    fixture.store.record_play("alice", play("A", "T", None));
    fixture.store.replace_stations("alice", &[one_station("alice")]);
    assert!(!fixture.store.wants_starting_profile("alice"));
    assert!(fixture.store.wants_starting_profile("nobody yet"));

    assert!(fixture.store.reset("ALICE"));
    assert!(!fixture.store.reset("nobody yet"));

    let alice = fixture.store.get_user("alice");
    assert_eq!(alice.username, "alice");
    assert!(alice.plays.is_empty() && alice.stations.is_empty());
    assert!(alice.history_cleared_utc.is_some());
    // No starting profile from stars, and no refresh work until a new play.
    assert!(!fixture.store.wants_starting_profile("alice"));
    assert!(fixture.store.known_users().is_empty());
    // The dashboard still lists the listener, now with nothing held.
    let summaries = fixture.store.get_summaries();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].play_count, 0);

    fixture.store.record_play("alice", play("B", "U", None));
    assert_eq!(fixture.store.known_users(), ["alice"]);

    // The mark survives a restart.
    let restarted = LastFmRadioStateStore::new(
        fixture.path.clone(),
        settings_store(LastFmSettings::default()),
        Arc::new(ExternalIdRegistry::in_memory()),
    );
    assert!(restarted.get_user("alice").history_cleared_utc.is_some());
}

#[test]
fn stations_from_a_refresh_begun_before_a_clear_are_discarded() {
    let fixture = StateFixture::new(None, None);
    fixture.store.record_play("alice", play("A", "T", None));
    fixture.store.mark_refreshing("alice");
    assert!(fixture.store.reset("alice"));

    fixture.store.replace_stations("alice", &[one_station("alice")]);
    assert!(fixture.store.get_user("alice").stations.is_empty());

    fixture.store.mark_refreshing("alice");
    fixture.store.replace_stations("alice", &[one_station("alice")]);
    let alice = fixture.store.get_user("alice");
    assert_eq!(alice.stations.len(), 1);
    assert!(!alice.refreshing);
}

#[test]
fn a_listener_never_cleared_leaves_no_mark_in_the_file() {
    let fixture = StateFixture::new(None, None);
    fixture.store.record_play("alice", play("A", "T", None));
    let written = std::fs::read_to_string(&fixture.path).unwrap();
    assert!(!written.contains("HistoryCleared"), "{written}");
}

/// Two keys that differ only in case fail the load, so it starts clean.
#[test]
fn keys_that_differ_only_in_case_start_clean() {
    let fixture = StateFixture::new(
        Some(r#"{"Version":1,"Users":{"a":{"Username":"a"},"A":{"Username":"A"}}}"#),
        None,
    );
    assert!(fixture.store.known_users().is_empty());
}

/// A failed refresh keeps its first 500 characters, and the summaries come newest first.
#[test]
fn failures_are_bounded_and_summaries_are_newest_first() {
    let fixture = StateFixture::new(None, None);
    fixture.store.mark_refresh_failed("alice", &"x".repeat(600));
    std::thread::sleep(std::time::Duration::from_millis(2));
    fixture.store.mark_refreshing("bob");
    fixture.store.record_play("bob", play("A", "T", None));
    let summaries = fixture.store.get_summaries();
    assert_eq!(summaries[0].username, "bob");
    assert_eq!(summaries[0].play_count, 1);
    assert!(summaries[0].refreshing);
    assert_eq!(
        summaries[1].last_refresh_error.as_deref().map(str::len),
        Some(500)
    );
    assert!(fixture.store.mark_heart("bob", "", "A", "T"));
    assert!(!fixture.store.mark_heart("bob", "", "A", "T"));
    assert!(fixture.store.get_user("bob").plays[0].hearted);
}

/// The fixture through the store: loading and saving it again with nothing pruned (a clock on
/// the fixture's day, a long retention) gives the same file back. The resolved id of the
/// non-local track is the registry's own.
#[test]
fn the_fixture_survives_a_load_and_a_save() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/state/lastfm-radio-state.json"
    ))
    .unwrap();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("lastfm-radio-state.json");
    std::fs::write(&path, &text).unwrap();
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-03T19:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let store = LastFmRadioStateStore::with_clock(
        path.clone(),
        settings_store(LastFmSettings {
            history_retention_days: 365,
            ..Default::default()
        }),
        registry.clone(),
        Clock::fixed(at),
    );
    assert!(!store.mark_heart("brandon", "4Kq3cS0bWq9dHq7y1nZb2e", "", ""));
    assert!(store.mark_heart("brandon", "1aB2cD3eF4gH5iJ6kL7mN8", "", ""));
    let written = std::fs::read_to_string(&path).unwrap();
    let minted = store.get_user("brandon").stations[0].tracks[0]
        .resolved_id
        .clone()
        .unwrap();
    let expected = text.replace("7YzXwVu6TsRq5PoN4mLk3J", &minted).replace(
        "\"Hearted\": false,\n          \"LearnedSignal\": false",
        "\"Hearted\": true,\n          \"LearnedSignal\": false",
    );
    assert_eq!(written, expected);
}

#[test]
fn playlist_serializer_has_json_xml_symmetry_and_read_only_metadata() {
    use octo_core::models::domain::Song;
    use octo_core::settings::SubsonicSettings;

    use crate::services::subsonic::subsonic_response_builder::new_subsonic_response_builder;

    let builder = new_subsonic_response_builder(
        Arc::new(ExternalIdRegistry::in_memory()),
        &SubsonicSettings::default(),
    );
    let station = LastFmRadioStation {
        id: station_id("alice", "mix"),
        key: "mix".into(),
        name: "Your Radio".into(),
        owner: "alice".into(),
        personalized: true,
        created_utc: Utc::now(),
        changed_utc: Utc::now(),
        valid_until_utc: Utc::now() + TimeDelta::hours(12),
        tracks: vec![LastFmRadioTrack {
            artist: "Artist".into(),
            title: "Title".into(),
            duration: Some(200),
            ..Default::default()
        }],
        ..Default::default()
    };
    let song = Song {
        id: "local-1".into(),
        artist: "Artist".into(),
        title: "Title".into(),
        album: "Album".into(),
        duration: Some(200),
        is_local: true,
        ..Default::default()
    };
    let json: serde_json::Value = serde_json::from_str(
        &builder
            .create_radio_playlist_response("json", &station, std::slice::from_ref(&song))
            .text(),
    )
    .unwrap();
    let playlist = &json["subsonic-response"]["playlist"];
    assert_eq!(playlist["readonly"], true);
    assert_eq!(playlist["entry"][0]["id"], "local-1");
    let xml = builder
        .create_radio_playlist_response("xml", &station, std::slice::from_ref(&song))
        .text()
        .into_owned();
    let element = xml.lines().nth(1).unwrap();
    assert!(element.contains(r#"readonly="true""#), "{element}");
    let child = xml.lines().nth(2).unwrap();
    assert!(child.contains(r#"id="local-1""#), "{child}");
}

fn song(artist: &str, title: &str) -> LastFmRadioTrack {
    LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        is_local: true,
        ..Default::default()
    }
}

fn titles(tracks: &[LastFmRadioTrack]) -> Vec<&str> {
    tracks.iter().map(|track| track.title.as_str()).collect()
}

/// A saved station a day old, holding `tracks` and `reserve`.
fn day_old(id: &str, tracks: Vec<LastFmRadioTrack>, reserve: Vec<LastFmRadioTrack>) -> LastFmRadioStation {
    let then = Utc::now() - TimeDelta::days(1);
    LastFmRadioStation {
        id: id.into(),
        key: id.into(),
        name: format!("{id} Radio"),
        owner: "alice".into(),
        created_utc: then,
        changed_utc: then,
        tracks,
        reserve,
        ..Default::default()
    }
}

/// A refresh saves what it built after a song was rated one star or found unplayable: the song
/// stays out of the saved station, from its songs and its reserve.
#[test]
fn a_save_leaves_out_songs_rated_one_star_or_found_unplayable_meanwhile() {
    let fixture = StateFixture::new(None, None);
    assert!(fixture.store.set_radio_ban("alice", "", "A", "Rated", true));
    fixture.store.reject_track("alice", &song("A", "Broken"), None);
    let built = day_old(
        "s1",
        vec![song("A", "Rated"), song("A", "Broken"), song("A", "Kept")],
        vec![song("A", "Rated"), song("A", "Broken"), song("A", "Spare")],
    );
    fixture.store.replace_stations("alice", &[built]);
    let saved = &fixture.store.get_user("alice").stations[0];
    assert_eq!(titles(&saved.tracks), ["Kept"]);
    assert_eq!(titles(&saved.reserve), ["Spare"]);
}

/// A song held out because it would not play comes back once its time is up.
#[test]
fn a_song_whose_hold_has_passed_is_saved() {
    let fixture = StateFixture::new(None, None);
    fixture
        .store
        .reject_track("alice", &song("A", "Back"), Some(Utc::now() - TimeDelta::days(2)));
    fixture
        .store
        .replace_stations("alice", &[day_old("s1", vec![song("A", "Back")], Vec::new())]);
    assert_eq!(
        titles(&fixture.store.get_user("alice").stations[0].tracks),
        ["Back"]
    );
}

/// A station left with no song is not saved.
#[test]
fn a_station_left_with_no_song_is_not_saved() {
    let fixture = StateFixture::new(None, None);
    assert!(fixture.store.set_radio_ban("alice", "", "A", "Only", true));
    fixture.store.replace_stations(
        "alice",
        &[
            day_old("empty", vec![song("A", "Only")], Vec::new()),
            day_old("full", vec![song("A", "Other")], Vec::new()),
        ],
    );
    let keys: Vec<String> = fixture
        .store
        .get_user("alice")
        .stations
        .into_iter()
        .map(|station| station.key)
        .collect();
    assert_eq!(keys, ["full"]);
}

/// A one-star rating or an unplayable song leaves a station's reserve too, and says the station
/// changed; the count of songs taken out is of listed songs only.
#[test]
fn rating_one_star_or_rejecting_clears_the_reserve_and_marks_the_change() {
    let fixture = StateFixture::new(None, None);
    let saved = day_old(
        "s1",
        vec![song("A", "Listed")],
        vec![song("A", "Spare one"), song("A", "Spare two")],
    );
    let then = saved.changed_utc;
    fixture.store.replace_stations("alice", &[saved]);

    assert!(fixture.store.set_radio_ban("alice", "", "A", "Spare one", true));
    let station = fixture.store.get_user("alice").stations[0].clone();
    assert_eq!(titles(&station.reserve), ["Spare two"]);
    assert!(station.changed_utc > then);

    let changed = station.changed_utc;
    assert_eq!(
        fixture.store.reject_track("alice", &song("A", "Spare two"), None),
        0
    );
    let station = fixture.store.get_user("alice").stations[0].clone();
    assert!(station.reserve.is_empty());
    assert!(station.changed_utc >= changed);
    assert_eq!(titles(&station.tracks), ["Listed"]);
}

/// A copy saved again with other songs says it changed now, even when the copy was made
/// earlier; a first save keeps its own time.
#[test]
fn a_changed_copy_saved_again_is_marked_changed_now() {
    let fixture = StateFixture::new(None, None);
    let first = day_old("s1", vec![song("A", "One")], Vec::new());
    let then = first.changed_utc;
    fixture.store.replace_stations("alice", &[first]);
    assert_eq!(fixture.store.get_user("alice").stations[0].changed_utc, then);

    let before = Utc::now();
    let copy = day_old("s1", vec![song("A", "One"), song("A", "Two")], Vec::new());
    fixture.store.replace_stations("alice", &[copy]);
    assert!(fixture.store.get_user("alice").stations[0].changed_utc >= before);
}

/// The reserve is part of what a station holds: other reserve songs are a change.
#[test]
fn a_different_reserve_is_a_changed_station() {
    let fixture = StateFixture::new(None, None);
    let first = day_old("s1", vec![song("A", "One")], vec![song("A", "Spare")]);
    fixture.store.replace_stations("alice", &[first]);
    let before = Utc::now();
    let copy = day_old("s1", vec![song("A", "One")], vec![song("A", "Other spare")]);
    fixture.store.replace_stations("alice", &[copy]);
    assert!(fixture.store.get_user("alice").stations[0].changed_utc >= before);
}

/// A station a one-star rating or an unplayable song leaves with no song goes from the list.
#[test]
fn a_station_emptied_by_a_rating_or_a_song_that_would_not_play_goes() {
    let fixture = StateFixture::new(None, None);
    fixture.store.replace_stations(
        "alice",
        &[
            day_old("one", vec![song("A", "Only")], Vec::new()),
            day_old("two", vec![song("B", "Last")], Vec::new()),
            day_old("three", vec![song("C", "Stays")], Vec::new()),
        ],
    );
    assert!(fixture.store.set_radio_ban("alice", "", "A", "Only", true));
    assert_eq!(fixture.store.reject_track("alice", &song("B", "Last"), None), 1);
    let keys: Vec<String> = fixture
        .store
        .get_user("alice")
        .stations
        .into_iter()
        .map(|station| station.key)
        .collect();
    assert_eq!(keys, ["three"]);
}

#[path = "last_fm_radio_carry_over_tests.rs"]
mod carry_over;
