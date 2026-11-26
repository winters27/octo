use super::*;

fn rows(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(artist, title)| (artist.to_string(), title.to_string()))
        .collect()
}

fn of(query: &str, artist: &str, title: &str) -> SearchMatch {
    SearchMatch::of(&SearchMatch::words(query), artist, title)
}

#[test]
fn a_title_and_artist_typed_together_name_the_song_by_that_artist() {
    let kali = of(
        "after the storm kali uchis",
        "Kali Uchis",
        "After The Storm (feat. Tyler, The Creator & Bootsy Collins)",
    );
    let mumford = of("after the storm kali uchis", "Mumford & Sons", "After the Storm");

    assert_eq!(kali.named, 5);
    assert_eq!(mumford.named, 3);
    assert!(kali > mumford);
}

#[test]
fn the_whole_title_named_beats_a_longer_title_that_holds_it() {
    let stay = of("stay", "Rihanna", "Stay");
    let stay_with_me = of("stay", "Sam Smith", "Stay With Me");

    assert!(stay.title_named);
    assert!(!stay_with_me.title_named);
    assert!(stay > stay_with_me);
}

#[test]
fn credits_and_version_markers_do_not_keep_a_title_from_being_named() {
    assert!(of("hello", "Adele", "Hello (feat. Someone)").title_named);
    assert!(of("hello", "Adele", "Hello - Live").title_named);
    assert!(!of("hello", "Adele", "Hello (Skrillex Remix)").title_named);
}

#[test]
fn words_drop_case_accents_and_apostrophes() {
    assert_eq!(of("beyonce halo", "Beyoncé", "Halo").named, 2);
    assert_eq!(
        of(
            "love story taylors version",
            "Taylor Swift",
            "Love Story (Taylor’s Version)"
        )
        .named,
        4
    );
    assert_eq!(of("ROLLING in the deep", "Adele", "Rolling in the Deep").named, 4);
}

#[test]
fn a_query_without_words_matches_nothing() {
    assert_eq!(of("", "Adele", "Hello"), SearchMatch::default());
    assert_eq!(of("!!", "Adele", "Hello"), SearchMatch::default());
}

#[test]
fn order_rows_puts_the_named_artist_first_and_keeps_the_catalog_order_otherwise() {
    let ordered = SearchMatch::order_rows(
        "after the storm kali uchis",
        rows(&[
            ("Mumford & Sons", "After the Storm"),
            ("Bethel Music", "After the Storm"),
            (
                "Kali Uchis",
                "After The Storm (feat. Tyler, The Creator & Bootsy Collins)",
            ),
            ("Aaron Smith", "After the Storm"),
        ]),
    );

    assert_eq!(
        ordered,
        rows(&[
            (
                "Kali Uchis",
                "After The Storm (feat. Tyler, The Creator & Bootsy Collins)"
            ),
            ("Mumford & Sons", "After the Storm"),
            ("Bethel Music", "After the Storm"),
            ("Aaron Smith", "After the Storm"),
        ])
    );
}

#[test]
fn order_rows_is_stable_when_every_row_matches_alike() {
    let input = rows(&[("A", "One"), ("B", "Two"), ("C", "Three")]);

    assert_eq!(SearchMatch::order_rows("zzz", input.clone()), input);
}
