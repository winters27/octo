use super::*;

fn t(artist: &str, title: &str, listeners: Option<i64>) -> SimilarTrack {
    SimilarTrack::new(artist, title, 1.0).with_listeners(listeners)
}

fn rank(query: &str, tracks: Vec<SimilarTrack>) -> Vec<(String, String)> {
    LastFmSearchRanking::rank(query, tracks)
        .into_iter()
        .map(|track| (track.artist, track.title))
        .collect()
}

fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(artist, title)| (artist.to_string(), title.to_string()))
        .collect()
}

#[test]
fn adele_puts_the_biggest_hits_first_not_last_fms_fuzzy_order() {
    // Last.fm's own order for "adele", with the listener counts measured on 2026-10-06.
    let rows = rank(
        "adele",
        vec![
            t("Adele", "Skyfall", Some(1_430_000)),
            t("<Unknown>", "Adele - Hello", Some(346)),
            t("Adele", "Chasing Pavements", Some(1_650_000)),
            t("Adele", "Someone Like You", Some(2_200_000)),
            t("Adele", "Rolling in the Deep", Some(2_680_000)),
        ],
    );

    assert_eq!(
        rows,
        pairs(&[
            ("Adele", "Rolling in the Deep"),
            ("Adele", "Someone Like You"),
            ("Adele", "Chasing Pavements"),
            ("Adele", "Skyfall"),
            ("<Unknown>", "Adele - Hello"),
        ])
    );
}

#[test]
fn hello_puts_adele_first() {
    let rows = rank(
        "hello",
        vec![
            t("Lionel Richie", "Hello", Some(900_000)),
            t("Adele", "Hello", Some(1_070_000)),
            t("Martin Solveig", "Hello", Some(200_000)),
        ],
    );

    assert_eq!(rows[0], ("Adele".to_string(), "Hello".to_string()));
}

#[test]
fn same_listeners_keep_last_fms_order_and_rows_without_a_count_go_last() {
    let rows = rank(
        "song",
        vec![
            t("A", "No count", None),
            t("B", "First", Some(100)),
            t("C", "Second", Some(100)),
            t("D", "Bigger", Some(500)),
        ],
    );

    assert_eq!(
        rows,
        pairs(&[
            ("D", "Bigger"),
            ("B", "First"),
            ("C", "Second"),
            ("A", "No count")
        ])
    );
}

#[test]
fn ranking_is_stable_over_many_equal_rows() {
    let input: Vec<SimilarTrack> = (0..40)
        .map(|i| t("Artist", &format!("Song {i}"), Some(10)))
        .collect();
    let titles: Vec<String> = input.iter().map(|track| track.title.clone()).collect();

    let ranked: Vec<String> = LastFmSearchRanking::rank("artist", input)
        .into_iter()
        .map(|track| track.title)
        .collect();

    assert_eq!(ranked, titles);
}

#[test]
fn a_cyrillic_row_is_listed_after_the_latin_rows_for_a_latin_query() {
    let rows = rank(
        "adele hello",
        vec![
            t("Адель", "Привет", Some(3_000_000)),
            t("Adele", "Hello", Some(1_070_000)),
            t("Adele", "Skyfall", Some(20)),
        ],
    );

    assert_eq!(
        rows,
        pairs(&[("Adele", "Hello"), ("Adele", "Skyfall"), ("Адель", "Привет")])
    );
}

#[test]
fn other_scripts_are_listed_after_the_latin_rows_for_a_latin_query() {
    for (artist, title) in [
        ("田中", "こんにちは"), // CJK and kana
        ("العربي", "مرحبا"),    // Arabic
        ("안녕", "하세요"),     // Hangul
        ("שלום", "הי"),         // Hebrew
        ("สวัสดี", "ครับ"),        // Thai
        ("Αλέξης", "Τραγούδι"), // Greek
    ] {
        let rows = rank(
            "hello",
            vec![t(artist, title, Some(5_000_000)), t("Adele", "Hello", Some(10))],
        );

        assert_eq!(rows, pairs(&[("Adele", "Hello"), (artist, title)]), "{artist}");
    }
}

#[test]
fn rows_in_another_script_are_ranked_by_listeners_among_themselves() {
    let rows = rank(
        "yorushika",
        vec![
            t("ヨルシカ", "だから僕は音楽を辞めた", Some(200_000)),
            t("ヨルシカ", "ただ君に晴れ", Some(500_000)),
            t("Yorushika", "Say It", Some(1_000)),
        ],
    );

    assert_eq!(
        rows,
        pairs(&[
            ("Yorushika", "Say It"),
            ("ヨルシカ", "ただ君に晴れ"),
            ("ヨルシカ", "だから僕は音楽を辞めた"),
        ])
    );
}

#[test]
fn a_latin_title_with_a_greek_letter_is_kept() {
    // Damso's Ipseite is titled with Greek letters in front of French words.
    let rows = rank(
        "damso",
        vec![
            t("Damso", "Θ. Macarena", Some(90_000)),
            t("Damso", "Ε. Signaler", Some(80_000)),
            t("Damso", "Γ. Mosaïque solitaire", Some(70_000)),
        ],
    );

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].1, "Θ. Macarena");
}

#[test]
fn a_cyrillic_query_keeps_cyrillic_rows_in_listener_order() {
    let rows = rank(
        "привет",
        vec![
            t("Adele", "Hello", Some(1_070_000)),
            t("Адель", "Привет", Some(3_000_000)),
        ],
    );

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "Привет");
}

#[test]
fn versions_of_one_song_are_one_row_the_most_listened() {
    for variant in [
        "Hello - Live",
        "Hello (Live at the Royal Albert Hall)",
        "Hello (2011 Remaster)",
        "Hello - Remastered 2011",
        "Hello [Mono Version]",
        "Hello (Radio Edit)",
        "Hello (Stereo)",
        "Hello (Demo)",
        "Hello (Acoustic)",
        "Hello (Live) (Remastered)",
    ] {
        let rows = rank(
            "hello",
            vec![
                t("Adele", variant, Some(50_000)),
                t("Adele", "Hello", Some(1_000_000)),
            ],
        );
        assert_eq!(rows, pairs(&[("Adele", "Hello")]), "{variant}");

        // The variant wins when it is the one people listen to.
        let reversed = rank(
            "hello",
            vec![
                t("Adele", variant, Some(3_000_000)),
                t("Adele", "Hello", Some(1_000_000)),
            ],
        );
        assert_eq!(reversed, pairs(&[("Adele", variant)]), "{variant}");
    }
}

#[test]
fn remixes_and_features_are_other_songs_and_stay() {
    for other in [
        "Hello (Skrillex Remix)",
        "Hello - Skrillex Remix",
        "Hello (Extended Mix)",
        "Hello (feat. Someone)",
        "Hello (Live Remix)",
        "Hello (Sped Up)",
        "Hello (Instrumental)",
    ] {
        let rows = rank(
            "hello",
            vec![
                t("Adele", "Hello", Some(1_000_000)),
                t("Adele", other, Some(50_000)),
            ],
        );

        assert_eq!(rows.len(), 2, "{other}");
    }
}

#[test]
fn the_same_title_by_two_artists_is_two_rows() {
    let rows = rank(
        "hello",
        vec![
            t("Adele", "Hello", Some(1_070_000)),
            t("Adele", "Hello (Live)", Some(90_000)),
            t("Lionel Richie", "Hello (Live)", Some(80_000)),
        ],
    );

    assert_eq!(
        rows,
        pairs(&[("Adele", "Hello"), ("Lionel Richie", "Hello (Live)")])
    );
}

#[test]
fn empty_stays_empty() {
    assert!(LastFmSearchRanking::rank("x", Vec::new()).is_empty());
}

#[test]
fn letters_decide_the_script_not_digits_or_marks() {
    assert!(!LastFmSearchRanking::is_mostly_non_latin(""));
    assert!(!LastFmSearchRanking::is_mostly_non_latin("1999 !!"));
    assert!(!LastFmSearchRanking::is_mostly_non_latin("Beyoncé Ｈｅｌｌｏ"));
    assert!(LastFmSearchRanking::is_mostly_non_latin("Привет 2024"));
}

#[test]
fn a_query_naming_a_version_keeps_the_version_asked_for() {
    let rows = rank(
        "love story taylor's version",
        vec![
            t("Taylor Swift", "Love Story", Some(2_000_000)),
            t("Taylor Swift", "Love Story (Taylor's Version)", Some(800_000)),
            t("Taylor Swift", "Love Story - Live", Some(10_000)),
        ],
    );

    // The take the query names goes first: it is the only row that holds every word.
    assert_eq!(
        rows,
        pairs(&[
            ("Taylor Swift", "Love Story (Taylor's Version)"),
            ("Taylor Swift", "Love Story"),
            ("Taylor Swift", "Love Story - Live"),
        ])
    );
}

#[test]
fn a_version_whose_marker_shares_a_word_with_the_query_keeps_its_row() {
    let rows = rank(
        "love story taylor's",
        vec![
            t("Taylor Swift", "Love Story", Some(2_000_000)),
            t("Taylor Swift", "Love Story (Taylor's Version)", Some(800_000)),
            t("Taylor Swift", "Love Story - Live", Some(10_000)),
        ],
    );

    // The marker the query names stays, and first, since it holds the query's last word; one
    // the query does not name is still folded away.
    assert_eq!(
        rows,
        pairs(&[
            ("Taylor Swift", "Love Story (Taylor's Version)"),
            ("Taylor Swift", "Love Story"),
        ])
    );
}

#[test]
fn a_title_and_artist_typed_together_put_that_artists_song_first() {
    // Every "After the Storm" track.search finds for the query, the better-known ones first.
    // Ordered by listeners alone, Kali Uchis's song was under all of them.
    let rows = rank(
        "after the storm kali uchis",
        vec![
            t("Mumford & Sons", "After the Storm", Some(900_000)),
            t("Bethel Music", "After the Storm", Some(300_000)),
            t("Aaron Smith", "After The Storm", Some(120_000)),
            t(
                "Kali Uchis",
                "After The Storm (feat. Tyler, The Creator & Bootsy Collins)",
                Some(80_000),
            ),
            t("Kali Uchis", "Telepatía", Some(1_500_000)),
        ],
    );

    assert_eq!(
        rows,
        pairs(&[
            (
                "Kali Uchis",
                "After The Storm (feat. Tyler, The Creator & Bootsy Collins)"
            ),
            ("Mumford & Sons", "After the Storm"),
            ("Bethel Music", "After the Storm"),
            ("Aaron Smith", "After The Storm"),
            ("Kali Uchis", "Telepatía"),
        ])
    );
}

#[test]
fn a_whole_title_named_goes_before_longer_titles_that_hold_it() {
    let rows = rank(
        "stay",
        vec![
            t("Sam Smith", "Stay With Me", Some(2_500_000)),
            t("The Kid LAROI", "Stay", Some(1_800_000)),
            t("Rihanna", "Stay", Some(1_600_000)),
        ],
    );

    assert_eq!(
        rows,
        pairs(&[
            ("The Kid LAROI", "Stay"),
            ("Rihanna", "Stay"),
            ("Sam Smith", "Stay With Me"),
        ])
    );
}

#[test]
fn a_misspelt_word_leaves_the_rest_of_the_query_to_decide() {
    // No row holds "deap"; the rows that hold the other words still go first.
    let rows = rank(
        "rolling in the deap adele",
        vec![
            t("Adele", "Skyfall", Some(1_430_000)),
            t("Aretha Franklin", "Rolling in the Deep", Some(200_000)),
            t("Adele", "Rolling in the Deep", Some(2_680_000)),
        ],
    );

    assert_eq!(rows[0], ("Adele".to_string(), "Rolling in the Deep".to_string()));
}
