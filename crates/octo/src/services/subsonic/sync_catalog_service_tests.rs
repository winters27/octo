//! The sync catalog service's tests: spotting a sync client, the local total from a page, the
//! window, resolving the local total, interleaving, and reading a library artist. The
//! page-writing tests are in `octo_subsonic::sync_catalog_response`.
//!
//! Also the build against a stand-in Navidrome (a library and stations asked of the service
//! directly), the catalog cache and its dates, the walk memo, `slice`, and the catalog's dates
//! reaching a page through `append`.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use octo_core::settings::{AppSettings, SubsonicSettings};
use parking_lot::Mutex;
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::common::test_fakes::FakeMetadata;

// -------------------------------------------------------------------------------
// Who gets it
// -------------------------------------------------------------------------------

#[test]
fn is_sync_client_matches_the_configured_names() {
    let cases: [(Option<&str>, bool); 6] = [
        (Some("Symfonium"), true),
        (Some("symfonium"), true),
        (Some("Symfonium (Android)"), true),
        (Some("Feishin"), false),
        (Some(""), false),
        (None, false),
    ];
    let settings = SubsonicSettings::default();
    for (client, expected) in cases {
        assert_eq!(
            SyncCatalogService::is_sync_client(&settings, client),
            expected,
            "{client:?}"
        );
    }
}

#[test]
fn is_sync_client_is_off_when_the_feature_is_and_takes_a_list() {
    let off = SubsonicSettings {
        enable_sync_catalog: false,
        ..Default::default()
    };
    assert!(!SyncCatalogService::is_sync_client(&off, Some("Symfonium")));
    let listed = SubsonicSettings {
        sync_catalog_clients: " Symfonium , Substreamer ".into(),
        ..Default::default()
    };
    assert!(SyncCatalogService::is_sync_client(&listed, Some("Substreamer")));
    assert!(!SyncCatalogService::is_sync_client(&listed, Some("Feishin")));
}

// -------------------------------------------------------------------------------
// Paging
// -------------------------------------------------------------------------------

#[test]
fn local_total_from_page_is_known_from_a_page_with_library_rows() {
    // (0, 0, 0): empty library, the first page proves it.
    for (offset, returned, expected) in [(0, 0, 0), (2000, 500, 2500), (0, 300, 300)] {
        assert_eq!(
            SyncCatalogService::local_total_from_page(offset, returned),
            Some(expected),
            "{offset}, {returned}"
        );
    }
}

#[test]
fn local_total_from_page_is_unknown_from_an_empty_later_page() {
    assert_eq!(SyncCatalogService::local_total_from_page(3000, 0), None);
}

#[test]
fn window_addresses_the_catalog_after_the_library() {
    let cases = [
        (2000, 1000, 500, 2500, 0, 500),  // the page the library ends on
        (3000, 1000, 0, 2500, 500, 1000), // the page after it
        (2000, 1000, 0, 2000, 0, 1000),   // library a multiple of the page size
    ];
    for (offset, count, returned, total, start, take) in cases {
        assert_eq!(
            SyncCatalogService::window(offset, count, returned, total),
            (start, take),
            "{offset}, {count}, {returned}, {total}"
        );
    }
}

#[tokio::test]
async fn resolve_local_total_finds_the_library_size_without_a_hint() {
    for (total, empty_offset) in [(2500, 3000), (2000, 2000), (0, 1000), (1, 4000)] {
        let mut probes = 0;
        let found = SyncCatalogService::resolve_local_total(empty_offset, None, |index| {
            probes += 1;
            std::future::ready(Some(index < total))
        })
        .await;
        assert_eq!(found, Some(total), "{total}, {empty_offset}");
        assert!(probes <= 13, "{probes} probes ({total}, {empty_offset})");
    }
}

#[tokio::test]
async fn resolve_local_total_trusts_a_remembered_size_only_when_the_probes_confirm_it() {
    let mut probes = 0;
    let found = SyncCatalogService::resolve_local_total(3000, Some(2500), |index| {
        probes += 1;
        std::future::ready(Some(index < 2500))
    })
    .await;
    assert_eq!(found, Some(2500));
    assert_eq!(probes, 2);

    // The library grew since the walk remembered it: bisect instead of trusting it.
    for total in [2600, 1800] {
        let found = SyncCatalogService::resolve_local_total(3000, Some(2500), |index| {
            std::future::ready(Some(index < total))
        })
        .await;
        assert_eq!(found, Some(total));
    }
}

#[tokio::test]
async fn resolve_local_total_gives_up_when_a_probe_fails() {
    let found = SyncCatalogService::resolve_local_total(3000, None, |_| std::future::ready(None)).await;
    assert_eq!(found, None);
}

fn station(tracks: &[(&str, &str)]) -> LastFmRadioStation {
    LastFmRadioStation {
        tracks: tracks
            .iter()
            .map(|(artist, title)| LastFmRadioTrack {
                artist: artist.to_string(),
                title: title.to_string(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn names(tracks: &[LastFmRadioTrack]) -> Vec<String> {
    tracks
        .iter()
        .map(|track| format!("{}|{}", track.artist, track.title))
        .collect()
}

#[test]
fn interleave_takes_from_every_station_in_turn_without_repeats() {
    let stations = [
        station(&[("A", "1"), ("A", "2"), ("A", "3")]),
        station(&[("B", "1"), ("a", "1"), ("B", "2")]),
    ];

    let all = SyncCatalogService::interleave(&stations, 100);
    assert_eq!(names(&all), ["A|1", "B|1", "A|2", "A|3", "B|2"]);

    let capped = SyncCatalogService::interleave(&stations, 3);
    assert_eq!(names(&capped), ["A|1", "B|1", "A|2"]);
}

// -------------------------------------------------------------------------------
// What the library already holds
// -------------------------------------------------------------------------------

#[test]
fn parse_library_artist_matches_the_artist_exactly_and_collects_its_albums_and_songs() {
    let body = br#"
        {"subsonic-response":{"status":"ok","searchResult3":{
          "artist":[{"id":"ar-airbourne","name":"Airbourne"},{"id":"ar-air","name":"Air"}],
          "album":[{"id":"al-moon","name":"Moon Safari","artist":"Air","artistId":"ar-air"},
                   {"id":"al-rock","name":"Runnin' Wild","artist":"Airbourne","artistId":"ar-airbourne"}],
          "song":[{"id":"s1","artist":"Air","title":"La Femme d'Argent"}]}}}
        "#;

    let air = SyncCatalogService::parse_library_artist(body, "AIR")
        .unwrap()
        .unwrap();
    assert_eq!(air.artist_id.as_deref(), Some("ar-air"));
    assert_eq!(air.album_ids.len(), 1);
    assert_eq!(air.album_ids.values().next().map(String::as_str), Some("al-moon"));
    assert!(air.owns("Air", "La Femme d'Argent"));
    assert!(!air.owns("Air", "Sexy Boy"));

    let stranger = SyncCatalogService::parse_library_artist(body, "Aire")
        .unwrap()
        .unwrap();
    assert_eq!(stranger.artist_id, None);
    assert!(stranger.album_ids.is_empty());
}

#[test]
fn parse_library_artist_is_null_for_a_failed_response() {
    let body = br#"{"subsonic-response":{"status":"failed","error":{"code":40}}}"#;
    assert_eq!(
        SyncCatalogService::parse_library_artist(body, "Air").unwrap(),
        None
    );
}

/// Where the body does not parse, the parser errs, and the lookup reads that as failed.
#[test]
fn parse_library_artist_errs_on_an_unreadable_body() {
    assert!(SyncCatalogService::parse_library_artist(b"not json", "Air").is_err());
    assert!(
        SyncCatalogService::parse_library_artist(br#"{"subsonic-response":{"status":3}}"#, "Air").is_err()
    );
    let no_result =
        SyncCatalogService::parse_library_artist(br#"{"subsonic-response":{"status":"ok"}}"#, "Air")
            .unwrap()
            .unwrap();
    assert_eq!(no_result.artist_id, None);
    assert!(no_result.songs.is_empty());
}

// -------------------------------------------------------------------------------
// Building
// -------------------------------------------------------------------------------

/// `SyncUpstreamHandler`'s search3 by artist name: "Owned Artist" is in the library with one
/// album and one song; every other artist is not.
#[derive(Clone, Default)]
struct Navidrome {
    queries: Arc<Mutex<Vec<String>>>,
}

impl Respond for Navidrome {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let term = query.get("query").cloned().unwrap_or_default();
        self.queries.lock().push(term.clone());
        let result = if term == "Owned Artist" {
            json!({
                "artist": [{"id": "ar-owned", "name": "Owned Artist"}],
                "album": [{"id": "al-owned", "name": "Owned Album", "artist": "Owned Artist", "artistId": "ar-owned"}],
                "song": [{"id": "lib-owned-hit", "title": "Owned Hit", "artist": "Owned Artist"}],
            })
        } else {
            json!({})
        };
        let body =
            json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "searchResult3": result}});
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "application/json")
            .set_body_string(body.to_string())
    }
}

/// A Your Radio station: twelve "Stranger" songs, an owned hit, a missing one on an owned album,
/// and a single with no album.
fn your_mix(changed_hour: u32) -> LastFmRadioStation {
    let track = |artist: &str, title: &str, album: Option<&str>, duration: i32| LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        album: album.map(str::to_string),
        duration: Some(duration),
        ..Default::default()
    };
    let mut tracks: Vec<LastFmRadioTrack> = (1..=12)
        .map(|index| {
            let album = if index <= 6 {
                "First Record"
            } else {
                "Second Record"
            };
            track("Stranger", &format!("Stranger Song {index}"), Some(album), 200)
        })
        .collect();
    tracks.insert(0, track("Owned Artist", "Owned Hit", None, 180));
    tracks.insert(2, track("Owned Artist", "Missing Hit", Some("Owned Album"), 190));
    tracks.push(track("Stranger", "Loner", None, 150));
    LastFmRadioStation {
        id: "or-your-mix".into(),
        key: "your-mix".into(),
        changed_utc: Utc.with_ymd_and_hms(2026, 9, 1, changed_hour, 0, 0).unwrap(),
        tracks,
        ..Default::default()
    }
}

/// The station tracks the library does not own.
fn expected_catalog_titles() -> Vec<String> {
    let mut titles = vec!["Missing Hit".to_string()];
    titles.extend((1..=12).map(|index| format!("Stranger Song {index}")));
    titles.push("Loner".into());
    titles
}

/// A metadata service with a placeholder per artist and title.
fn metadata(stations: &[LastFmRadioStation]) -> Arc<FakeMetadata> {
    let metadata = FakeMetadata::default();
    for track in stations.iter().flat_map(|station| &station.tracks) {
        metadata.hits.lock().insert(
            (track.artist.clone(), track.title.clone()),
            Song {
                id: format!("ph-{}-{}", track.artist, track.title).replace(' ', "-"),
                artist: track.artist.clone(),
                title: track.title.clone(),
                album: String::new(),
                duration: Some(track.duration.unwrap_or(180)),
                is_local: false,
                external_provider: Some("soulseek".into()),
                ..Default::default()
            },
        );
    }
    Arc::new(metadata)
}

struct Fixture {
    _server: MockServer,
    navidrome: Navidrome,
    registry: Arc<ExternalIdRegistry>,
    now: Arc<Mutex<DateTime<Utc>>>,
    service: Arc<SyncCatalogService>,
}

async fn fixture(stations: &[LastFmRadioStation]) -> Fixture {
    let navidrome = Navidrome::default();
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(navidrome.clone())
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            ..Default::default()
        },
        ..Default::default()
    }));
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap()));
    let clock_now = now.clone();
    let service = Arc::new(SyncCatalogService::new(
        SubsonicProxyService::new(settings.clone()),
        metadata(stations),
        registry.clone(),
        settings,
        Clock::new(move || *clock_now.lock()),
    ));
    Fixture {
        _server: server,
        navidrome,
        registry,
        now,
        service,
    }
}

fn auth() -> IndexMap<String, String> {
    [("u", "alice"), ("t", "token"), ("s", "salt"), ("c", "Symfonium")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A playlist's song found online, as the held routes hand it to the catalog.
fn playlist_song(n: usize, title: &str) -> Song {
    Song {
        id: format!("ext-deezer-song-{n}"),
        title: title.into(),
        artist: "Owned Artist".into(),
        artist_id: Some("ext-deezer-artist-1".into()),
        album: "Elsewhere".into(),
        album_id: Some("ext-deezer-album-1".into()),
        duration: Some(200),
        is_local: false,
        ..Default::default()
    }
}

#[tokio::test]
async fn playlist_songs_make_a_catalog_with_no_stations_unmatched_against_the_library_and_uncapped() {
    let fixture = fixture(&[]).await;
    // More than the cap's top, all by an artist the library holds.
    let songs: Vec<Song> = (0..5_001).map(|n| playlist_song(n, "Missing Hit")).collect();
    let held = HeldCatalog {
        songs: songs.clone(),
        fingerprint: "1:a".into(),
    };
    let catalog = fixture.service.get_with("alice", &[], &held, &auth()).await;
    assert_eq!(catalog.songs().len(), 5_001);
    assert!(catalog.try_get_song("ext-deezer-song-0").is_some());
    assert!(fixture.navidrome.queries.lock().is_empty(), "no library lookups");
    assert_eq!(catalog.albums().len(), 1);

    // The same songs and fingerprint: the cached catalog. A new generation builds again.
    let again = fixture.service.get_with("alice", &[], &held, &auth()).await;
    assert!(Arc::ptr_eq(&catalog, &again));
    let fewer = HeldCatalog {
        songs: songs[..1].to_vec(),
        fingerprint: "2:b".into(),
    };
    let rebuilt = fixture.service.get_with("alice", &[], &fewer, &auth()).await;
    assert_eq!(rebuilt.songs().len(), 1);

    // No stations and no playlist songs: the empty catalog, as before.
    let empty = fixture
        .service
        .get_with("alice", &[], &HeldCatalog::default(), &auth())
        .await;
    assert!(empty.songs().is_empty());
}

#[tokio::test]
async fn build_leaves_out_what_the_library_owns_and_files_under_the_librarys_artist_and_album() {
    let stations = [your_mix(2)];
    let fixture = fixture(&stations).await;

    let catalog = fixture.service.get("alice", &stations, &auth()).await;

    // One library lookup per artist.
    let mut queries = fixture.navidrome.queries.lock().clone();
    queries.sort();
    assert_eq!(queries, ["Owned Artist", "Stranger"]);

    let mut titles: Vec<String> = catalog.songs().iter().map(|song| song.title.clone()).collect();
    titles.sort();
    let mut expected = expected_catalog_titles();
    expected.sort();
    assert_eq!(titles, expected);

    let missing = catalog
        .songs()
        .iter()
        .find(|song| song.title == "Missing Hit")
        .unwrap();
    assert_eq!(missing.artist_id.as_deref(), Some("ar-owned"));
    assert_eq!(missing.album_id.as_deref(), Some("al-owned"));
    assert_eq!(missing.album, "Owned Album");
    assert_eq!(missing.duration, Some(190));

    // A single is its own album; the others are filed under minted album and artist ids.
    let loner = catalog.songs().iter().find(|song| song.title == "Loner").unwrap();
    assert_eq!(loner.album, "Loner");
    let stranger_id = loner.artist_id.clone().unwrap();
    assert_ne!(stranger_id, "ar-owned");
    let routing = fixture.registry.lookup(&stranger_id).unwrap().snapshot();
    assert_eq!(routing.kind, RoutingKind::Artist);
    assert_eq!(routing.artist.as_deref(), Some("Stranger"));

    // Albums and artists only for what the library does not have, matching the songs.
    let album_ids: Vec<&str> = catalog.albums().iter().map(|album| album.id.as_str()).collect();
    assert!(!album_ids.contains(&"al-owned"));
    let mut song_albums: Vec<&str> = catalog
        .songs()
        .iter()
        .filter_map(|song| song.album_id.as_deref())
        .filter(|id| *id != "al-owned")
        .collect();
    song_albums.dedup();
    assert_eq!(album_ids, song_albums);
    let first_record = catalog
        .albums()
        .iter()
        .find(|album| album.title == "First Record")
        .unwrap();
    assert_eq!(first_record.song_count, Some(6));
    assert_eq!(first_record.artist_id.as_deref(), Some(stranger_id.as_str()));
    assert!(!first_record.is_local);
    assert_eq!(first_record.external_provider.as_deref(), Some("soulseek"));

    let artists: Vec<&str> = catalog
        .artists()
        .iter()
        .map(|artist| artist.name.as_str())
        .collect();
    assert_eq!(artists, ["Stranger"]);
    assert_eq!(catalog.artists()[0].album_count, Some(3));
    assert_eq!(catalog.count(SyncCatalogKind::Artist), 1);
    assert_eq!(catalog.count(SyncCatalogKind::Album), 3);
    assert_eq!(catalog.count(SyncCatalogKind::Song), 14);

    // Every row is dated when it joined; the station playlist can describe it as the sync did.
    let now = *fixture.now.lock();
    assert!(catalog.added().values().all(|added| *added == now));
    assert_eq!(catalog.added().len(), 14 + 3 + 1);
    let described = fixture.service.try_get_song("ALICE", &missing.id).unwrap();
    assert_eq!(described.album_id.as_deref(), Some("al-owned"));
    assert!(fixture.service.try_get_song("bob", &missing.id).is_none());
}

#[tokio::test]
async fn get_reuses_a_fresh_catalog_and_keeps_each_rows_first_date_across_rebuilds() {
    let stations = [your_mix(2)];
    let fixture = fixture(&stations).await;
    let first = fixture.service.get("alice", &stations, &auth()).await;
    let built_at = first.built_utc();

    // Fresh and unchanged: the same catalog, no lookups.
    let lookups = fixture.navidrome.queries.lock().len();
    let again = fixture.service.get("Alice", &stations, &auth()).await;
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(fixture.navidrome.queries.lock().len(), lookups);

    // The stations changed: a rebuild, with each row keeping the date it first joined.
    *fixture.now.lock() += TimeDelta::minutes(5);
    let changed = [your_mix(3)];
    let rebuilt = fixture.service.get("alice", &changed, &auth()).await;
    assert!(!Arc::ptr_eq(&first, &rebuilt));
    assert_ne!(rebuilt.fingerprint(), first.fingerprint());
    assert_eq!(rebuilt.built_utc(), built_at + TimeDelta::minutes(5));
    assert!(rebuilt.added().values().all(|added| *added == built_at));

    // An hour on, the same stations are built again.
    *fixture.now.lock() += TimeDelta::hours(1);
    let stale = fixture.service.get("alice", &changed, &auth()).await;
    assert!(!Arc::ptr_eq(&rebuilt, &stale));

    // Nobody asking, or no stations: the empty catalog, and nothing built.
    assert_eq!(fixture.service.get("", &changed, &auth()).await.songs().len(), 0);
    assert!(Arc::ptr_eq(
        &fixture.service.get("alice", &[], &auth()).await,
        &SyncCatalog::empty()
    ));
}

#[tokio::test]
async fn concurrent_callers_share_one_build() {
    let stations = [your_mix(2)];
    let fixture = fixture(&stations).await;
    let a = fixture.service.get("alice", &stations, &auth());
    let b = fixture.service.get("ALICE", &stations, &auth());
    // Warm on a running build starts nothing either.
    fixture.service.warm("alice", &stations, &auth());
    let (a, b) = futures::join!(a, b);
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(fixture.navidrome.queries.lock().len(), 2);
}

#[tokio::test]
async fn a_failed_library_lookup_leaves_the_artists_tracks_out() {
    // No Navidrome configured: every lookup fails, so nothing is offered on a guess.
    let stations = [your_mix(2)];
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let service = Arc::new(SyncCatalogService::new(
        SubsonicProxyService::new(settings.clone()),
        metadata(&stations),
        Arc::new(ExternalIdRegistry::in_memory()),
        settings,
        Clock::system(),
    ));
    let catalog = service.get("alice", &stations, &auth()).await;
    assert!(catalog.songs().is_empty());
    assert!(catalog.albums().is_empty());
    assert!(catalog.artists().is_empty());
}

#[tokio::test]
async fn a_walk_pins_its_catalog_and_library_size_for_twenty_minutes() {
    let fixture = fixture(&[]).await;
    let service = &fixture.service;
    assert_eq!(
        service.remembered_local_total("alice", SyncCatalogKind::Song),
        None
    );

    let catalog = Arc::new(SyncCatalog::new(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        "fp".into(),
        *fixture.now.lock(),
    ));
    service.remember("alice", SyncCatalogKind::Song, 2500, catalog.clone());
    assert_eq!(
        service.remembered_local_total("ALICE", SyncCatalogKind::Song),
        Some(2500)
    );
    assert!(Arc::ptr_eq(
        &service.pinned_catalog("alice", SyncCatalogKind::Song).unwrap(),
        &catalog
    ));
    // Each kind is its own walk.
    assert_eq!(
        service.remembered_local_total("alice", SyncCatalogKind::Album),
        None
    );

    *fixture.now.lock() += TimeDelta::minutes(19);
    assert_eq!(
        service.remembered_local_total("alice", SyncCatalogKind::Song),
        Some(2500)
    );
    *fixture.now.lock() += TimeDelta::minutes(1);
    assert_eq!(
        service.remembered_local_total("alice", SyncCatalogKind::Song),
        None
    );
    assert!(service.pinned_catalog("alice", SyncCatalogKind::Song).is_none());
}

fn small_catalog(added: DateTime<Utc>) -> SyncCatalog {
    let songs = (1..=3)
        .map(|index| Song {
            id: format!("cat-song-{index}"),
            title: format!("New {index}"),
            artist: "Stranger".into(),
            album: "New One".into(),
            album_id: Some("cat-album".into()),
            artist_id: Some("cat-artist".into()),
            duration: Some(200),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    let albums = vec![Album {
        id: "cat-album".into(),
        title: "New One".into(),
        artist: "Stranger".into(),
        artist_id: Some("cat-artist".into()),
        song_count: Some(3),
        ..Default::default()
    }];
    let artists = vec![Artist {
        id: "cat-artist".into(),
        name: "Stranger".into(),
        album_count: Some(1),
        ..Default::default()
    }];
    let added = songs
        .iter()
        .map(|song| song.id.clone())
        .chain(["cat-album".to_string(), "cat-artist".to_string()])
        .map(|id| (id, added))
        .collect();
    SyncCatalog::new(songs, albums, artists, added, "fp".into(), added_at())
}

fn added_at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap()
}

#[test]
fn slice_takes_the_window_of_one_kind() {
    let catalog = small_catalog(added_at());
    let ids = |songs: &[Song]| songs.iter().map(|song| song.id.clone()).collect::<Vec<_>>();

    let (songs, albums, artists) = SyncCatalogService::slice(&catalog, SyncCatalogKind::Song, 1, 5);
    assert_eq!(ids(&songs), ["cat-song-2", "cat-song-3"]);
    assert!(albums.is_empty() && artists.is_empty());

    let (songs, albums, artists) = SyncCatalogService::slice(&catalog, SyncCatalogKind::Album, 0, 10);
    assert!(songs.is_empty() && artists.is_empty());
    assert_eq!(albums.len(), 1);

    let (_, _, artists) = SyncCatalogService::slice(&catalog, SyncCatalogKind::Artist, 1, 10);
    assert!(artists.is_empty());
    let (songs, _, _) = SyncCatalogService::slice(&catalog, SyncCatalogKind::Song, 0, 0);
    assert!(songs.is_empty());
    assert_eq!(catalog.try_get_song("cat-song-3").unwrap().title, "New 3");
    assert!(catalog.try_get_song("missing").is_none());
}

/// The catalog's `added` is what `octo_subsonic`'s page writer dates the rows with.
#[test]
fn catalog_rows_reach_the_page_with_the_date_they_joined() {
    let catalog = small_catalog(added_at());
    let builder = crate::services::subsonic::new_subsonic_response_builder(
        Arc::new(ExternalIdRegistry::in_memory()),
        &SubsonicSettings::default(),
    );
    let (songs, albums, artists) = SyncCatalogService::slice(&catalog, SyncCatalogKind::Song, 0, 1);
    let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{"song":[{"id":"lib-1"}]}}}"#;

    let output = octo_subsonic::sync_catalog_response::append(
        body,
        Some("application/json"),
        "searchResult3",
        &builder,
        catalog.added(),
        &artists,
        &albums,
        &songs,
    )
    .unwrap();

    let page: Value = serde_json::from_slice(&output).unwrap();
    let rows = &page["subsonic-response"]["searchResult3"]["song"];
    assert_eq!(rows[1]["id"], "cat-song-1");
    assert_eq!(rows[1]["created"], "2026-09-01T12:00:00.000Z");
}

#[test]
fn the_fingerprint_names_each_station_change_the_cap_and_the_filter() {
    let stations = [your_mix(2)];
    let settings = SubsonicSettings {
        sync_catalog_max_songs: 10,
        explicit_filter: ExplicitFilter::CleanOnly,
        ..Default::default()
    };
    // 2026-09-01T02:00:00Z in ticks (100 ns units since 0001-01-01).
    assert_eq!(
        SyncCatalogService::fingerprint(&stations, &settings, false),
        "or-your-mix:639238248000000000#50#CleanOnly"
    );
    assert_eq!(
        SyncCatalogService::fingerprint(&stations, &settings, true),
        "or-your-mix:639238248000000000#50#CleanOnly#SkipCoversAndInstrumentals"
    );
}
