//! The iTunes lookup. The state file is checked against its fixture
//! (`docs/rust-migration/fixtures/state/itunes-masters.json`), and the master search against a
//! mock Apple.

use chrono::TimeZone;
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/rust-migration/fixtures/state/itunes-masters.json")
}

fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 3, h, m, s)
        .single()
        .expect("a valid time")
}

/// Read → write through the file's serde shape gives the fixture back byte for byte.
#[test]
fn itunes_masters_fixture_round_trips_byte_for_byte() {
    let original = std::fs::read(fixture_path()).expect("the fixture is there");
    let rows: Vec<CachedMaster> = serde_json::from_slice(&original).expect("the fixture reads");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].url, None);
    assert_eq!(octo_core::json::to_string(&rows).as_bytes(), &original[..]);
}

/// The store loads the fixture (on a clock inside both TTLs) and writes it back unchanged.
#[test]
fn the_store_loads_and_writes_the_fixture_unchanged() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("itunes-masters.json");
    std::fs::copy(fixture_path(), &file).expect("copied");
    let original = std::fs::read(&file).expect("read");

    let lookup =
        ITunesCoverArtLookup::with_base_url(Some(file.clone()), "http://unused", Clock::fixed(at(9, 0, 0)));
    write_rows(&file, &lookup.cached_rows()).expect("written");

    assert_eq!(std::fs::read(&file).expect("read"), original);
    assert!(!dir.path().join("itunes-masters.json.tmp").exists());
}

/// On load, a miss older than a day is dropped and a hit is kept for thirty.
#[test]
fn loading_drops_what_has_expired() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("itunes-masters.json");
    std::fs::copy(fixture_path(), &file).expect("copied");

    let next_day = Clock::fixed(at(9, 0, 0) + chrono::Duration::days(1));
    let lookup = ITunesCoverArtLookup::with_base_url(Some(file.clone()), "http://unused", next_day);
    let rows = lookup.cached_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.as_deref(), Some("motleycrue|drfeelgood|album"));

    let much_later = Clock::fixed(at(9, 0, 0) + chrono::Duration::days(31));
    let lookup = ITunesCoverArtLookup::with_base_url(Some(file), "http://unused", much_later);
    assert!(lookup.cached_rows().is_empty());
}

#[test]
fn an_unreadable_file_is_an_empty_cache() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("itunes-masters.json");
    std::fs::write(&file, "not json").expect("written");
    let lookup = ITunesCoverArtLookup::with_base_url(Some(file), "http://unused", Clock::fixed(at(9, 0, 0)));
    assert!(lookup.cached_rows().is_empty());
}

/// The master is the one whose artist and album are this release, the explicit one before
/// the clean one, fetched at the master size; the match is remembered, written on flush, and
/// read back by the next process.
#[tokio::test]
async fn a_master_is_matched_strictly_remembered_and_kept_on_disk() {
    let server = MockServer::builder().start().await;
    let art = |name: &str| format!("{}/image/{name}/100x100bb.jpg", server.uri());
    let results = format!(
        r#"{{"results":[
            {{"artistName":"Karaoke Stars","collectionName":"Dr. Feelgood","artworkUrl100":"{}"}},
            {{"artistName":"Mötley Crüe","collectionName":"Dr. Feelgood","collectionExplicitness":"cleaned","artworkUrl100":"{}"}},
            {{"artistName":"Mötley Crüe","collectionName":"Dr. Feelgood","collectionExplicitness":"explicit","artworkUrl100":"{}"}}
        ]}}"#,
        art("karaoke"),
        art("clean"),
        art("explicit")
    );
    Mock::given(path("/search"))
        .and(query_param("entity", "album"))
        .respond_with(ResponseTemplate::new(200).set_body_string(results))
        .mount(&server)
        .await;
    Mock::given(path("/image/explicit/5000x5000bb.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"master".to_vec()))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("config").join("itunes-masters.json");
    let clock = Clock::fixed(at(10, 0, 0));
    let lookup = ITunesCoverArtLookup::with_base_url(Some(file.clone()), &server.uri(), clock.clone());

    let master = lookup
        .try_fetch_album_master(
            Some("Mötley Crüe"),
            Some("Dr. Feelgood"),
            Some("Kickstart My Heart"),
        )
        .await;
    assert_eq!(master.as_deref(), Some(&b"master"[..]));
    lookup
        .try_fetch_album_master(
            Some("Mötley Crüe"),
            Some("Dr. Feelgood"),
            Some("Kickstart My Heart"),
        )
        .await;
    let searches =
        |requests: &[wiremock::Request]| requests.iter().filter(|r| r.url.path() == "/search").count();
    assert_eq!(searches(&server.received_requests().await.unwrap_or_default()), 1);

    lookup.flush_cache();
    let rows: Vec<CachedMaster> =
        serde_json::from_str(&std::fs::read_to_string(&file).expect("written")).expect("reads");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].key.as_deref(),
        Some(format!("{}|album", SongIdentity::match_key("Mötley Crüe", "Dr. Feelgood")).as_str())
    );
    assert_eq!(rows[0].url.as_deref(), Some(art("explicit").as_str()));
    assert_eq!(rows[0].at, at(10, 0, 0));

    // The next process asks Apple nothing for it.
    let again = ITunesCoverArtLookup::with_base_url(Some(file), &server.uri(), clock);
    assert_eq!(
        again
            .try_fetch_album_master(Some("Mötley Crüe"), Some("Dr. Feelgood"), None)
            .await
            .as_deref(),
        Some(&b"master"[..])
    );
    assert_eq!(searches(&server.received_requests().await.unwrap_or_default()), 1);
}

/// Master searches take Cover turns on the lookup's pace; chain searches wait for none while Deezer
/// answers Octo's searches, and take turns while Apple answers them.
#[tokio::test]
async fn master_searches_are_paced_and_chain_searches_only_while_apple_stands_in() {
    let server = MockServer::start().await;
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"results":[]}"#))
        .mount(&server)
        .await;
    let spaced = Arc::new(ApplePace::new(PaceConfig {
        cover_spacing: Duration::from_millis(400),
        ..PaceConfig::unpaced()
    }));
    let masters = ITunesCoverArtLookup::with_base_url(None, &server.uri(), Clock::fixed(at(10, 0, 0)))
        .with_pace(Arc::clone(&spaced));
    let started = Instant::now();
    masters
        .try_fetch_album_master(Some("Air"), Some("Moon Safari"), None)
        .await;
    masters
        .try_fetch_album_master(Some("Air"), Some("Talkie Walkie"), None)
        .await;
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "two master searches are a Cover turn apart"
    );

    // The next Cover turn is half a minute away.
    let slow = Arc::new(ApplePace::new(PaceConfig {
        cover_spacing: Duration::from_secs(30),
        ..PaceConfig::unpaced()
    }));
    let _ = slow.turn(AppleClass::Cover).await;
    let chain = ITunesCoverArtLookup::with_base_url(None, &server.uri(), Clock::fixed(at(10, 0, 0)))
        .with_pace(Arc::clone(&slow));
    let song = |title: &str| SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Kino".into()),
        title: Some(title.into()),
        ..Default::default()
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        chain.try_fetch(&song("Gruppa krovi"), false).await;
        chain.try_fetch(&song("Kukushka"), false).await;
    })
    .await
    .expect("chain searches wait for no turn while Deezer answers");
    slow.set_strict(true);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(500),
            chain.try_fetch(&song("Zvezda"), false)
        )
        .await
        .is_err(),
        "while Apple answers Octo's searches, a chain search waits its turn"
    );
}

/// A refused master search waits the lookup's retry wait, then asks once more.
#[tokio::test]
async fn a_refused_master_search_waits_the_retry_wait_then_asks_again() {
    let server = MockServer::start().await;
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"results":[]}"#))
        .mount(&server)
        .await;
    let pace = Arc::new(ApplePace::new(PaceConfig {
        cover_spacing: Duration::from_millis(10),
        ..PaceConfig::unpaced()
    }));
    let lookup = ITunesCoverArtLookup::with_base_url(None, &server.uri(), Clock::fixed(at(10, 0, 0)))
        .with_pace(Arc::clone(&pace))
        .with_retry_wait(Duration::from_millis(100));
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(10),
        lookup.try_fetch_album_master(Some("Air"), Some("Moon Safari"), None),
    )
    .await
    .expect("the retry wait is the lookup's, not a minute");
    assert!(started.elapsed() >= Duration::from_millis(100));
    let searches = server.received_requests().await.unwrap_or_default().len();
    assert!(searches >= 2, "asked again after the refusal");
    assert!(pace.backoff() > Duration::ZERO, "the refusal slowed the pace");
}

/// One barcode lookup matches albums back by name and keeps them for both ways of asking.
#[tokio::test]
async fn priming_by_barcode_matches_albums_back_by_name() {
    let server = MockServer::builder().start().await;
    let results = format!(
        r#"{{"results":[
            {{"wrapperType":"collection","artistName":"Air","collectionName":"Moon Safari","artworkUrl100":"{0}/a/100x100bb.jpg"}},
            {{"wrapperType":"collection","artistName":"Air","collectionName":"Talkie Walkie - EP","artworkUrl100":"{0}/b/100x100bb.jpg"}},
            {{"wrapperType":"track","artistName":"Air","collectionName":"Moon Safari","artworkUrl100":"{0}/c/100x100bb.jpg"}}
        ]}}"#,
        server.uri()
    );
    Mock::given(path("/lookup"))
        .and(query_param(
            "upc",
            "0724384497859,724384497859,0724359706221,724359706221",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(results))
        .mount(&server)
        .await;
    let lookup = ITunesCoverArtLookup::with_base_url(None, &server.uri(), Clock::fixed(at(10, 0, 0)));
    let albums = vec![
        (
            "Air".to_string(),
            "Moon Safari".to_string(),
            "0724384497859".to_string(),
        ),
        (
            "Air".to_string(),
            "Talkie Walkie".to_string(),
            "0724359706221".to_string(),
        ),
        (
            "Air".to_string(),
            "Premiers Symptômes".to_string(),
            " ".to_string(),
        ),
    ];
    let seen = Mutex::new(Vec::new());
    let report = |done: usize| seen.lock().push(done);

    let matched = lookup.prime_by_barcode(&albums, Some(&report)).await;

    assert_eq!(matched, 2);
    assert_eq!(*seen.lock(), vec![2]);
    assert_eq!(lookup.cached_rows().len(), 4);
}

/// A search ranks its hits by how well the artist is named: the same name in another script
/// beats a longer name that only starts with the same letters.
#[tokio::test]
async fn a_search_takes_the_hit_that_names_the_artist() {
    let server = MockServer::start().await;
    let results = format!(
        r#"{{"results":[
            {{"artistName":"Kinoman","trackName":"Gruppa krovi","artworkUrl100":"{0}/image/kinoman/100x100bb.jpg"}},
            {{"artistName":"Кино","trackName":"Группа крови","artworkUrl100":"{0}/image/kino/100x100bb.jpg"}}
        ]}}"#,
        server.uri()
    );
    Mock::given(path("/search"))
        .and(query_param("entity", "song"))
        .respond_with(ResponseTemplate::new(200).set_body_string(results))
        .mount(&server)
        .await;
    Mock::given(path("/image/kino/1200x1200bb.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"kino".to_vec()))
        .mount(&server)
        .await;
    Mock::given(path("/image/kinoman/1200x1200bb.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"kinoman".to_vec()))
        .mount(&server)
        .await;
    let lookup = ITunesCoverArtLookup::with_base_url(None, &server.uri(), Clock::fixed(at(10, 0, 0)));

    let cover = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some("Kino".into()),
                title: Some("Gruppa krovi".into()),
                ..Default::default()
            },
            false,
        )
        .await;

    assert_eq!(cover.as_deref(), Some(&b"kino"[..]));
}
