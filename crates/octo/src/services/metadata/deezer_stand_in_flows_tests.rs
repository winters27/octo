//! The Deezer service and its callers on a world whose Deezer is closed or unreachable: what each
//! public entry asks Apple and Deezer, and what it hands back.

use super::*;
use crate::services::metadata::deezer_metadata_service::TrackAnswer;

/// Deezer closed to this server, knowing no barcode Apple's artwork names: every Apple album stays
/// Apple's.
async fn closed_without_twins(world: &World) {
    Mock::given(path("/infos"))
        .respond_with(ok(INDIA_INFOS))
        .mount(&world.deezer)
        .await;
    Mock::given(path_regex(r"^/search(/album|/track)?$"))
        .respond_with(ok(HIDDEN_SEARCH))
        .mount(&world.deezer)
        .await;
    Mock::given(path_regex(r"^/album/upc:"))
        .respond_with(ok(NO_SUCH))
        .mount(&world.deezer)
        .await;
    world.reach.read_infos().await;
    assert!(matches!(world.reach.reach(), Reach::Closed { .. }));
}

/// Apple's song search for Hans Zimmer's "Stay", the song itself by its id, and the Interstellar
/// soundtrack's tracklist for any other lookup.
async fn apple_songs(world: &World) {
    let songs: Value = serde_json::from_str(&recorded("search-song-hans-zimmer-stay-in.json")).unwrap();
    let stay = songs["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|song| song["trackName"] == "Stay")
        .unwrap()
        .clone();
    Mock::given(path("/search"))
        .and(query_param("entity", "song"))
        .respond_with(ok(songs.to_string()))
        .mount(&world.apple)
        .await;
    Mock::given(path("/lookup"))
        .and(query_param("id", "1533984570"))
        .respond_with(ok(json!({"resultCount": 1, "results": [stay]}).to_string()))
        .with_priority(1)
        .mount(&world.apple)
        .await;
    Mock::given(path("/lookup"))
        .respond_with(ok(recorded("lookup-1533983552-songs-in.json")))
        .mount(&world.apple)
        .await;
}

/// Apple's own collection result for the Interstellar soundtrack.
fn interstellar_collection() -> Value {
    let doc: Value = serde_json::from_str(&recorded("lookup-1533983552-songs-in.json")).unwrap();
    doc["results"][0].clone()
}

async fn deezer_paths(world: &World) -> Vec<String> {
    world
        .deezer
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| request.url.path().to_string())
        .collect()
}

#[tokio::test]
async fn the_taggers_blank_fill_reads_the_song_and_its_album_from_apple() {
    let world = World::new().await;
    closed_without_twins(&world).await;
    apple_songs(&world).await;
    let full = world
        .service()
        .enrich_track_full("Hans Zimmer", "Stay")
        .await
        .expect("Apple has the song");
    let album = interstellar_collection();
    assert_eq!(full.title.as_deref(), Some("Stay"));
    assert_eq!(full.track_number, Some(5));
    assert_eq!(full.disc_number, Some(1));
    assert_eq!(full.duration, Some(412));
    assert_eq!(full.total_tracks, Some(30));
    assert_eq!(full.artist_name.as_deref(), Some("Hans Zimmer"));
    assert_eq!(full.album_artist_name.as_deref(), Some("Hans Zimmer"));
    assert_eq!(full.album_title.as_deref(), album["collectionName"].as_str());
    assert_eq!(full.record_type.as_deref(), Some("album"));
    let year = album["releaseDate"]
        .as_str()
        .and_then(|date| date.get(..4))
        .and_then(|year| year.parse::<i32>().ok());
    assert_eq!(full.year, year);
    assert_eq!(full.genre.as_deref(), album["primaryGenreName"].as_str());
    assert_eq!(full.track_id, Some(code_apple_id(1_533_984_570).to_string()));
    assert_eq!(full.album_id, Some(code_apple_id(INTERSTELLAR).to_string()));
    assert_eq!(full.isrc, None, "Apple names no ISRC");
    assert!(
        deezer_paths(&world)
            .await
            .iter()
            .all(|path| path == "/infos" || path.starts_with("/album/upc:")),
        "Deezer is asked nothing it hides"
    );
}

#[tokio::test]
async fn release_candidates_come_from_apple_under_coded_ids() {
    let world = World::new().await;
    closed_without_twins(&world).await;
    apple_songs(&world).await;
    let candidates = world
        .service()
        .enrich_track_candidates("Hans Zimmer", "Stay", 3)
        .await;
    assert!(!candidates.did_not_answer);
    assert!(
        candidates
            .hits
            .iter()
            .any(|hit| hit.title.as_deref() == Some("Stay"))
    );
    for hit in &candidates.hits {
        let id = hit.track_id.as_deref().and_then(|id| id.parse::<u64>().ok());
        assert!(
            id.is_some_and(|id| id >= APPLE_ID_BASE),
            "an Apple song carries a coded id: {:?}",
            hit.track_id
        );
    }
}

#[tokio::test]
async fn a_song_lookup_miss_asks_apple_at_most_three_times_and_skips_the_album_and_top_steps() {
    let world = World::new().await;
    closed_without_twins(&world).await;
    apple_songs(&world).await;
    let answer = world
        .service()
        .enrich_track_answered("Nobody Anyone Knows", "A Song Nowhere", false, true)
        .await;
    assert_eq!(answer, TrackAnswer::Missing);
    let asked = world.apple.received_requests().await.unwrap_or_default();
    assert!(
        !asked.is_empty() && asked.len() <= 3,
        "{} Apple requests",
        asked.len()
    );
    assert!(
        asked
            .iter()
            .all(|request| request.url.query().is_some_and(|q| q.contains("entity=song"))),
        "only song searches: no album search and no artist's songs"
    );
    assert_eq!(deezer_paths(&world).await, ["/infos"]);
}

#[tokio::test]
async fn go_to_album_resolves_an_apple_album_or_its_twin() {
    let apple_only = World::new().await;
    closed_without_twins(&apple_only).await;
    apple_only.apple_album_search().await;
    let title = interstellar_collection()["collectionName"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        apple_only.service().find_album_id("Hans Zimmer", &title).await,
        Some(code_apple_id(INTERSTELLAR).to_string())
    );

    let twinned = World::new().await;
    twinned.india().await;
    twinned.apple_album_search().await;
    twinned.reach.read_infos().await;
    assert_eq!(
        twinned.service().find_album_id("Hans Zimmer", &title).await,
        Some(INTERSTELLAR_TWIN.to_string())
    );
}

#[tokio::test]
async fn an_artists_top_songs_come_in_apples_order() {
    let world = World::new().await;
    closed_without_twins(&world).await;
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
    // Apple's artist songs, in Apple's order: the artist, then three of the soundtrack's songs.
    let songs: Value = serde_json::from_str(&recorded("search-song-hans-zimmer-stay-in.json")).unwrap();
    let picked: Vec<Value> = songs["results"].as_array().unwrap()[1..4].to_vec();
    let mut results =
        vec![json!({"wrapperType": "artist", "artistId": 454_295_032, "artistName": "Hans Zimmer"})];
    results.extend(picked.iter().cloned());
    Mock::given(path("/lookup"))
        .and(query_param("entity", "song"))
        .respond_with(ok(
            json!({ "resultCount": results.len(), "results": results }).to_string()
        ))
        .mount(&world.apple)
        .await;
    let top = world
        .service()
        .get_artist_top("1935", 3)
        .await
        .expect("the artist's songs");
    let titles: Vec<&str> = top.iter().map(|track| track.title.as_str()).collect();
    let expected: Vec<&str> = picked
        .iter()
        .map(|song| song["trackName"].as_str().unwrap())
        .collect();
    assert_eq!(titles, expected);
}

#[tokio::test]
async fn the_explicit_word_and_the_version_checks_covers_come_from_apple() {
    let world = World::new().await;
    closed_without_twins(&world).await;
    apple_songs(&world).await;
    world.apple_album_search().await;
    let service = world.service();
    let facts = service
        .search_track_facts(Some("Hans Zimmer"), Some("Stay"))
        .await;
    assert!(!facts.did_not_answer);
    let stay = facts
        .tracks
        .iter()
        .find(|track| track.title == "Stay")
        .expect("Apple's Stay");
    assert_eq!(stay.explicit_content, Some(0), "Apple's notExplicit");
    assert_eq!(stay.duration, Some(412));

    let covers = service.release_covers("interstellar").await;
    assert!(!covers.is_empty());
    assert!(
        covers.iter().all(|(_, _, cover)| cover.contains("mzstatic.com")),
        "Apple's covers: {covers:?}"
    );
}

#[tokio::test]
async fn the_chart_comes_from_apples_most_played_in_its_order_under_apples_ids() {
    let world = World::new().await;
    closed_without_twins(&world).await;
    let feed: Value = serde_json::from_str(&recorded("rss-most-played-in-10.json")).unwrap();
    let items = feed["feed"]["results"].as_array().unwrap().clone();
    Mock::given(path_regex(
        r"^/api/v2/[a-z]{2}/music/most-played/\d+/songs\.json$",
    ))
    .respond_with(ok(feed.to_string()))
    .mount(&world.apple)
    .await;
    let songs: Vec<Value> = items
        .iter()
        .map(|item| {
            let id: u64 = item["id"].as_str().unwrap().parse().unwrap();
            json!({
                "wrapperType": "track", "kind": "song", "trackId": id, "trackName": item["name"],
                "artistName": item["artistName"], "collectionId": id + 1, "collectionName": item["name"],
                "trackTimeMillis": 200_000, "trackNumber": 1, "discNumber": 1,
                "trackExplicitness": "notExplicit",
            })
        })
        .collect();
    Mock::given(path("/lookup"))
        .respond_with(ok(
            json!({"resultCount": songs.len(), "results": songs}).to_string()
        ))
        .mount(&world.apple)
        .await;
    let chart = world.service().get_chart(10).await.expect("Apple's chart");
    let titles: Vec<&str> = chart.iter().map(|track| track.title.as_str()).collect();
    let expected: Vec<&str> = items.iter().map(|item| item["name"].as_str().unwrap()).collect();
    assert_eq!(titles, expected);
    assert!(
        chart
            .iter()
            .all(|track| octo_core::metadata::apple_as_deezer::is_apple_id(&track.catalog_id)),
        "the chart's rows carry Apple's ids, which name it Apple's"
    );
    assert!(
        !deezer_paths(&world)
            .await
            .iter()
            .any(|path| path.starts_with("/chart"))
    );
}

#[tokio::test]
async fn a_request_held_across_a_catalog_switch_keeps_its_answer_to_itself() {
    let world = World::new().await;
    closed_without_twins(&world).await;
    Mock::given(path("/search"))
        .and(query_param("entity", "album"))
        .respond_with(ok(recorded("search-album-interstellar-in.json")).set_delay(Duration::from_millis(800)))
        .mount(&world.apple)
        .await;
    let service = Arc::new(world.service());
    let held = tokio::spawn({
        let service = Arc::clone(&service);
        async move { service.search_albums("interstellar", 20, true).await }
    });
    for _ in 0..400 {
        if world.apple_asked().await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(world.apple_asked().await, 1, "the search is held at Apple");

    // Deezer serves this server now: a catalog switch.
    world.deezer.reset().await;
    Mock::given(path("/infos"))
        .respond_with(ok(US_INFOS))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/search/album"))
        .respond_with(ok(r#"{"data":[{"id":5,"title":"Interstellar","artist":{"id":1,"name":"Hans Zimmer"},"nb_tracks":30,"record_type":"album"}],"total":1}"#))
        .mount(&world.deezer)
        .await;
    let before = world.reach.generation();
    world.reach.read_infos().await;
    assert_eq!(world.reach.generation(), before + 1);

    let after = service.search_albums("interstellar", 20, true).await;
    assert_eq!(
        after.iter().map(|hit| hit.deezer_id.as_str()).collect::<Vec<_>>(),
        ["5"],
        "a new caller asks Deezer rather than joining the held request"
    );
    let old = held.await.unwrap();
    assert!(
        old.iter()
            .any(|hit| hit.deezer_id == code_apple_id(INTERSTELLAR).to_string()),
        "the held request still gets Apple's answer"
    );
    let again = service.search_albums("interstellar", 20, true).await;
    assert_eq!(
        again.iter().map(|hit| hit.deezer_id.as_str()).collect::<Vec<_>>(),
        ["5"],
        "what the held request kept is never read under the new catalog"
    );
    assert_eq!(world.apple_asked().await, 1);
}

/// One page request against a world whose door gives 400 ms, and how long it took.
async fn page_within(world: &World, path_and_query: &str) -> (Option<StatusCode>, Duration) {
    let started = std::time::Instant::now();
    let answer = world
        .door
        .get_for(
            &format!("{}/{path_and_query}", world.origin),
            AskFor::page(),
            None,
        )
        .await;
    (answer.ok().map(|answer| answer.status), started.elapsed())
}

#[tokio::test]
async fn a_hung_infos_read_ends_at_the_pages_budget_and_is_never_nothing() {
    let world = World::with_timeouts(Duration::from_millis(400), Duration::from_secs(6)).await;
    Mock::given(path_regex(r"^/search(/album|/track)?$"))
        .respond_with(ok(HIDDEN_SEARCH))
        .mount(&world.deezer)
        .await;
    Mock::given(path("/infos"))
        .respond_with(ok(INDIA_INFOS).set_delay(Duration::from_secs(30)))
        .mount(&world.deezer)
        .await;
    let (status, took) = page_within(&world, "search/album?q=interstellar&limit=20").await;
    assert_ne!(
        status,
        Some(StatusCode::OK),
        "a hidden answer is never handed out as nothing"
    );
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[tokio::test]
async fn a_hung_deezer_step_inside_the_stand_in_ends_at_the_pages_budget() {
    let world = World::with_timeouts(Duration::from_millis(400), Duration::from_secs(6)).await;
    closed_without_twins(&world).await;
    Mock::given(path("/artist/1935"))
        .respond_with(ok(recorded("deezer-artist-1935.json")).set_delay(Duration::from_secs(30)))
        .mount(&world.deezer)
        .await;
    let (status, took) = page_within(&world, "artist/1935/albums?limit=100").await;
    assert_ne!(status, Some(StatusCode::OK));
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[tokio::test]
async fn work_no_one_waits_on_ends_when_apples_own_call_does() {
    let world = World::with_timeouts(Duration::from_secs(5), Duration::from_millis(300)).await;
    closed_without_twins(&world).await;
    Mock::given(path("/search"))
        .respond_with(ok(recorded("search-album-interstellar-in.json")).set_delay(Duration::from_secs(30)))
        .mount(&world.apple)
        .await;
    let started = std::time::Instant::now();
    let answer = world
        .door
        .get_after_permit_for(
            &format!("{}/search/album?q=interstellar&limit=20", world.origin),
            AskFor::background(),
            None,
        )
        .await;
    assert!(
        answer.map_or(true, |answer| answer.status != StatusCode::OK),
        "an unanswered Apple call is never nothing"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_catalog_switch_clears_what_the_stand_in_learned() {
    let world = World::new().await;
    world.stand_in.follow_switches();
    interstellar_twin_known(&world).await;
    let generation = world.reach.generation();
    assert!(world.stand_in.twins.get(generation, INTERSTELLAR).is_some());
    world.deezer.reset().await;
    Mock::given(path("/infos"))
        .respond_with(ok(US_INFOS))
        .mount(&world.deezer)
        .await;
    world.reach.read_infos().await;
    assert_eq!(world.reach.generation(), generation + 1);
    for _ in 0..400 {
        if world.stand_in.twins.get(generation, INTERSTELLAR).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        world.stand_in.twins.get(generation, INTERSTELLAR).is_none(),
        "the twin map is cleared at the switch"
    );
}
