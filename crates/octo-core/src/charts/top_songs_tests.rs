use serde_json::json;

use super::*;

fn catalog(title: &str, artist: &str, album: &str) -> CatalogTrack {
    CatalogTrack {
        catalog_id: format!("{artist}|{title}"),
        title: title.into(),
        artist: artist.into(),
        artists: Vec::new(),
        artist_id: Some("27".into()),
        album: Some(album.into()),
        album_id: Some("302127".into()),
        cover_url: Some(format!("https://cdn/{album}.jpg")),
        duration: Some(240),
        explicit: Some(false),
        explicit_content: None,
    }
}

fn daft(title: &str) -> CatalogTrack {
    catalog(title, "Daft Punk", "Discovery")
}

fn counted(title: &str, plays: i64) -> CountedTrack {
    CountedTrack {
        artist: "Daft Punk".into(),
        title: title.into(),
        plays: Some(plays),
        listeners: Some(plays / 10),
    }
}

#[test]
fn merge_with_play_counts_follows_last_fm_and_dresses_each_row_from_the_catalog() {
    let catalog = vec![
        catalog(
            "Instant Crush (feat. Julian Casablancas)",
            "Daft Punk",
            "Random Access Memories",
        ),
        daft("One More Time"),
        daft("Harder, Better, Faster, Stronger"),
    ];
    let counts = vec![
        counted("Harder, Better, Faster, Stronger", 2_500_000),
        counted("One More Time", 1_900_000),
        counted("Aerodynamic", 1_500_000),
        counted("Instant Crush", 900_000),
    ];

    let (rows, source) = merge(
        Some(catalog.as_slice()),
        Some(counts.as_slice()),
        true,
        10,
        DEEZER_SOURCE,
    );

    assert_eq!(source, LASTFM_SOURCE);
    let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(
        titles,
        vec![
            "Harder, Better, Faster, Stronger",
            "One More Time",
            "Aerodynamic",
            "Instant Crush"
        ]
    );
    let plays: Vec<i64> = rows.iter().map(|r| r.plays.unwrap()).collect();
    assert_eq!(plays, vec![2_500_000, 1_900_000, 1_500_000, 900_000]);
    // A guest credit is still the song, so the catalog's copy dresses it.
    assert_eq!(
        rows[3].catalog.as_ref().unwrap().album.as_deref(),
        Some("Random Access Memories")
    );
    // One the catalog's top list lacks is left for a lookup.
    assert!(rows[2].catalog.is_none());
}

#[test]
fn merge_without_counts_keeps_the_catalog_order_and_one_row_per_song() {
    let catalog = vec![
        daft("Instant Crush (feat. Julian Casablancas)"),
        catalog(
            "Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)",
            "Daft Punk",
            "Get Lucky",
        ),
        daft("One More Time"),
        catalog(
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
            "Daft Punk",
            "Random Access Memories",
        ),
    ];

    let (rows, source) = merge(Some(catalog.as_slice()), None, true, 10, DEEZER_SOURCE);

    assert_eq!(source, DEEZER_SOURCE);
    assert_eq!(rows.len(), 3);
    assert!(rows[1].title.starts_with("Get Lucky (Radio Edit"));
    assert!(rows.iter().all(|r| r.plays.is_none()));
}

#[test]
fn merge_on_a_chart_keeps_two_artists_songs_of_one_title() {
    let catalog = vec![
        catalog("Hello", "Adele", "25"),
        catalog("Hello", "Lionel Richie", "Can't Slow Down"),
        catalog("Hello", "Adele", "Hello"),
    ];

    let (rows, _) = merge(Some(catalog.as_slice()), None, false, 10, DEEZER_SOURCE);

    let artists: Vec<&str> = rows.iter().map(|r| r.artist.as_str()).collect();
    assert_eq!(artists, vec!["Adele", "Lionel Richie"]);
}

#[test]
fn merge_stops_at_the_limit_and_says_nothing_for_nothing() {
    let catalog: Vec<CatalogTrack> = (1..=30).map(|i| daft(&format!("Song {i}"))).collect();
    assert_eq!(
        merge(Some(catalog.as_slice()), None, true, 5, DEEZER_SOURCE)
            .0
            .len(),
        5
    );
    assert_eq!(merge(None, None, true, 5, DEEZER_SOURCE).1, "");
    // Counts that came back empty leave the catalog's order.
    assert_eq!(
        merge(Some(catalog.as_slice()), Some(&[][..]), true, 5, DEEZER_SOURCE).1,
        DEEZER_SOURCE
    );
}

#[test]
fn merge_says_where_a_catalog_order_came_from() {
    let catalog = vec![daft("One More Time")];
    assert_eq!(
        merge(Some(catalog.as_slice()), None, false, 5, APPLE_SOURCE).1,
        APPLE_SOURCE
    );
}

#[test]
fn library_query_is_the_main_artist_and_the_bare_title() {
    let cases = [
        (
            "Daft Punk",
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
            "Daft Punk Get Lucky",
        ),
        (
            "Daft Punk",
            "Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)",
            "Daft Punk Get Lucky",
        ),
        ("Drake feat. Rihanna", "Too Good", "Drake Too Good"),
        ("Air", "Sexy Boy", "Air Sexy Boy"),
    ];
    for (artist, title, want) in cases {
        assert!(
            library_query(artist, title).eq_ignore_ascii_case(want),
            "{artist} / {title}"
        );
    }
}

#[test]
fn library_title_query_is_the_bare_title_and_never_repeats_the_first_query() {
    assert_eq!(
        library_title_query("Kanye West", "Through the Wire").as_deref(),
        Some("Through the Wire")
    );
    assert_eq!(
        library_title_query("Daft Punk", "Get Lucky (feat. Pharrell Williams)").as_deref(),
        Some("Get Lucky")
    );
    // No artist: the first query already was the title alone.
    assert_eq!(library_title_query("", "Heartless"), None);
    assert_eq!(library_title_query("Kanye West", ""), None);
}

#[test]
fn a_title_search_hit_by_another_artist_is_not_the_row() {
    let hits = [
        json!({"id": "a", "artist": "Some Band", "title": "Through the Wire"}),
        json!({"id": "b", "artist": "Ye", "title": "Through the Wire"}),
    ];
    let hit = library_hit("Kanye West", "Through the Wire", &hits).unwrap();
    assert_eq!(hit["id"], "b");
    assert!(library_hit("Kanye West", "Through the Wire", &hits[..1]).is_none());
}

#[test]
fn match_library_finds_the_same_recording_only_and_gives_a_library_song_to_one_row() {
    let rows = [
        ("Air", "Sexy Boy"),
        (
            "Daft Punk",
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
        ),
        ("Daft Punk", "Get Lucky (Radio Edit)"),
        ("Daft Punk", "Around the World"),
    ];
    let answer = |query: &str| -> Option<Vec<Value>> {
        match query {
            "Air Sexy Boy" => Some(vec![
                json!({"id": "nd-airbourne", "artist": "Airbourne", "title": "Sexy Boy"}),
            ]),
            "Daft Punk Get Lucky" => Some(vec![
                json!({"id": "nd-lucky", "artist": "Daft Punk", "title": "Get Lucky"}),
            ]),
            "Daft Punk Around the World" => None,
            _ => Some(Vec::new()),
        }
    };
    let mut found: Vec<Option<Value>> = rows
        .iter()
        .map(|(artist, title)| {
            let hits = answer(&library_query(artist, title))?;
            library_hit(artist, title, &hits).cloned()
        })
        .collect();
    one_row_per_library_song(&mut found);

    assert!(found[0].is_none());
    assert_eq!(found[1].as_ref().unwrap()["id"], "nd-lucky");
    // The radio edit is the same song, but the library copy already stands for row 2.
    assert!(found[2].is_none());
    assert!(found[3].is_none());
}

#[test]
fn a_hit_without_an_id_never_counts() {
    let hits = vec![json!({"artist": "Air", "title": "Sexy Boy"})];
    assert!(library_hit("Air", "Sexy Boy", &hits).is_none());
}

#[test]
fn explicit_from_catalog_reads_deezers_two_fields_as_one() {
    let cases = [
        ((Some(1), None), Some(1)),
        ((Some(4), Some(false)), Some(1)),
        ((Some(3), Some(true)), Some(3)),
        ((Some(0), Some(true)), Some(0)),
        ((Some(2), Some(true)), Some(1)),
        ((Some(6), Some(false)), None),
        ((None, None), None),
        ((None, Some(true)), Some(1)),
    ];
    for ((content, flag), want) in cases {
        assert_eq!(explicit_from_catalog(content, flag), want, "{content:?} {flag:?}");
    }
}

fn apple(title: &str, artists: &[&str], explicit: bool) -> AppleSong {
    AppleSong {
        rank: 1,
        title: title.into(),
        artists: artists.iter().map(|a| (*a).to_string()).collect(),
        album: Some("HABIBTI (FOMO)".into()),
        song_id: Some("6818209989".into()),
        album_id: Some("6818209777".into()),
        duration_secs: Some(218),
        explicit,
        artwork: Some("https://is1-ssl.mzstatic.com/x.jpg/{w}x{h}bb.{f}".into()),
    }
}

#[test]
fn a_collaboration_is_filed_under_its_main_artist() {
    let track = CatalogTrack::from_apple(&apple("Solar Eclipse", &["Drake", "Don Toliver"], true));
    assert_eq!(track.artist, "Drake");
    assert_eq!(track.artists, vec!["Drake", "Don Toliver"]);
}

#[test]
fn a_single_artist_leaves_artists_empty() {
    let track = CatalogTrack::from_apple(&apple("Quebec", &["Drake"], true));
    assert_eq!(track.artist, "Drake");
    assert!(track.artists.is_empty());
}

#[test]
fn an_apple_row_carries_its_album_cover_length_and_badge() {
    let explicit = CatalogTrack::from_apple(&apple("Solar Eclipse", &["Drake"], true));
    assert_eq!(explicit.catalog_id, "6818209989");
    assert_eq!(explicit.album.as_deref(), Some("HABIBTI (FOMO)"));
    assert_eq!(
        (explicit.artist_id, explicit.album_id),
        (None, None),
        "Apple's ids are not Deezer's"
    );
    assert_eq!(
        explicit.cover_url.as_deref(),
        Some("https://is1-ssl.mzstatic.com/x.jpg/1200x1200bb.jpg")
    );
    assert_eq!(explicit.duration, Some(218));
    assert_eq!(explicit.explicit_content, Some(1));
    let plain = CatalogTrack::from_apple(&apple("Cold Shoulder", &["Drake"], false));
    assert_eq!(
        plain.explicit_content,
        Some(0),
        "no badge is none, never a clean edit"
    );
}

#[test]
fn apple_rows_merge_by_artist_and_title() {
    let rows = vec![
        CatalogTrack::from_apple(&apple("Hello", &["Adele"], false)),
        CatalogTrack::from_apple(&apple("Hello", &["Lionel Richie"], false)),
        CatalogTrack::from_apple(&apple("Hello (Radio Edit)", &["Adele"], false)),
    ];
    let (planned, source) = merge(Some(rows.as_slice()), None, false, 50, APPLE_SOURCE);
    assert_eq!(source, APPLE_SOURCE);
    assert_eq!(planned.len(), 2);
}
