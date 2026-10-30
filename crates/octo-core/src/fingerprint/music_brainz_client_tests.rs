//! Tests for the MusicBrainz query builders and the readers of its answers: release dates, the
//! search queries, and the ISRCs of a recording lookup.

use super::*;

// ---- Release dates -----------------------------------------------------------------------

#[test]
fn a_year_only_date_never_beats_a_full_date_of_the_same_year() {
    assert!(is_earlier_date("2009-09-08", "2009"));
    assert!(!is_earlier_date("2009", "2009-09-08"));
    assert!(is_earlier_date("2008", "2009-09-08"));
    assert!(!is_earlier_date("2009-09-08", "2008"));
    assert!(is_earlier_date("2009-09", "2009-10-01"));
    assert!(!is_earlier_date("2009-09-08", "2009-09-08"));
}

// ---- Search queries ----------------------------------------------------------------------
// Titles and names with quotes, colons, slashes or brackets must reach the music database as
// the words they are, not as operators, or the failure reads as "no candidate".

fn query(url: &str) -> String {
    let start = url.find("query=").expect("has a query") + "query=".len();
    let end = url.find("&fmt=json").expect("has fmt");
    dotnet::unescape_data_string(&url[start..end])
}

#[test]
fn build_recording_search_url_escapes_every_operator() {
    let cases: &[(&str, &str, i32, &str)] = &[
        (
            "AC/DC",
            "Thunderstruck",
            292,
            "recording:\"Thunderstruck\" AND artist:\"AC\\/DC\" AND dur:[282000 TO 302000]",
        ),
        (
            "Bizarrap",
            "Bzrp Music Sessions, Vol. 56",
            0,
            "recording:\"Bzrp Music Sessions, Vol. 56\" AND artist:\"Bizarrap\"",
        ),
        (
            "Adele",
            "Hello?",
            295,
            "recording:\"Hello\\?\" AND artist:\"Adele\" AND dur:[285000 TO 305000]",
        ),
        (
            "Shawn Mendes",
            "Señorita: Remix",
            0,
            "recording:\"Señorita\\: Remix\" AND artist:\"Shawn Mendes\"",
        ),
        (
            "\"Weird Al\" Yankovic",
            "Amish Paradise",
            0,
            "recording:\"Amish Paradise\" AND artist:\"\\\"Weird Al\\\" Yankovic\"",
        ),
    ];
    for &(artist, title, seconds, expected) in cases {
        assert_eq!(
            query(&build_recording_search_url(artist, title, seconds)),
            expected,
            "{artist} - {title}"
        );
    }
}

#[test]
fn build_recording_search_url_exact_encoded_url() {
    assert_eq!(
        build_recording_search_url("AC/DC", "Thunderstruck", 292),
        "recording/?query=recording%3A%22Thunderstruck%22%20AND%20artist%3A%22AC%5C%2FDC%22%20AND%20dur%3A%5B282000%20TO%20302000%5D&fmt=json&limit=25"
    );
}

#[test]
fn build_recording_search_url_short_length_never_goes_negative() {
    assert!(query(&build_recording_search_url("A", "B", 5)).ends_with("AND dur:[0 TO 15000]"));
}

#[test]
fn escape_query_cases() {
    let cases = [
        ("plain words", "plain words"),
        (
            "a+b-c&&d||e!f(g)h{i}j[k]l^m\"n~o*p?q:r\\s/t",
            "a\\+b\\-c\\&\\&d\\|\\|e\\!f\\(g\\)h\\{i\\}j\\[k\\]l\\^m\\\"n\\~o\\*p\\?q\\:r\\\\s\\/t",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(escape_query(input), expected, "{input}");
    }
}

// ---- ISRCs of a recording lookup ---------------------------------------------------------

#[test]
fn parse_isrcs_reads_a_recording_lookup() {
    let doc: Value = serde_json::from_str(
        r#"{"id":"mbid","title":"紅蓮華","isrcs":["JPU901901234","jpu901901234","bogus"]}"#,
    )
    .expect("parses");
    assert_eq!(parse_isrcs(&doc).expect("reads"), ["JPU901901234"]);

    let none: Value = serde_json::from_str(r#"{"id":"mbid","title":"紅蓮華"}"#).expect("parses");
    assert!(parse_isrcs(&none).expect("reads").is_empty());
}

// ---- The recording query, and pick --------------------------------------------------------

#[test]
fn the_recording_query_keeps_its_shape() {
    let url = find_recording_url("Massive Attack", "Tear\"drop", 330).expect("asks");
    assert_eq!(
        query(&url),
        "recording:\"Tear\\\"drop\" AND artist:\"Massive Attack\""
    );
    assert!(find_recording_url("A", "B", 0).is_none());
}

#[test]
fn pick_answers_only_when_exactly_one_recording_fits() {
    let doc: Value = serde_json::from_str(
        r#"{"recordings":[
          {"id":"a","score":100,"title":"Teardrop","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"A","score":100,"title":"Teardrop","length":330500,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"live","score":100,"title":"Teardrop","disambiguation":"live","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"low","score":50,"title":"Teardrop","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"video","score":100,"video":true,"title":"Teardrop","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"long","score":100,"title":"Teardrop","length":400000,"artist-credit":[{"name":"Massive Attack"}]}
        ]}"#,
    )
    .expect("parses");
    // "a" and "A" are one id to an OrdinalIgnoreCase set.
    assert_eq!(
        pick(&doc, "Massive Attack", "Teardrop", 330)
            .expect("reads")
            .as_deref(),
        Some("a")
    );

    let two: Value = serde_json::from_str(
        r#"{"recordings":[
          {"id":"a","title":"Teardrop","length":330000,"artist-credit":[{"name":"Massive Attack"}]},
          {"id":"b","title":"Teardrop","length":330027,"artist-credit":[{"name":"Massive Attack"}]}
        ]}"#,
    )
    .expect("parses");
    assert_eq!(
        pick(&two, "Massive Attack", "Teardrop", 330).expect("reads"),
        None
    );
}

// ---- Where an artist is from ----------------------------------------------------------------

fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("test JSON parses")
}

#[test]
fn an_artist_lookup_reads_the_country_then_the_area_and_never_a_region() {
    assert_eq!(
        artist_country(&json(
            r#"{"id":"a","name":"Rosalía","country":"ES","area":{"iso-3166-1-codes":["ES"]}}"#
        )),
        "ES"
    );
    assert_eq!(
        artist_country(&json(
            r#"{"id":"a","name":"X","country":null,"area":{"iso-3166-1-codes":["jp"]}}"#
        )),
        "JP"
    );
    assert_eq!(
        artist_country(&json(
            r#"{"id":"a","name":"X","country":"XW","area":{"name":"Worldwide"}}"#
        )),
        ""
    );
    assert_eq!(artist_country(&json(r#"{"id":"a","name":"X"}"#)), "");
    assert_eq!(
        artist_lookup_url(" 5e5c5a1c-b4fc-4ba2-9d1e-5f0e1bd1b9a2 ").as_deref(),
        Some("artist/5e5c5a1c-b4fc-4ba2-9d1e-5f0e1bd1b9a2?fmt=json")
    );
    assert_eq!(artist_lookup_url("  "), None);
}

#[test]
fn a_name_search_takes_only_the_same_name_scoring_90_or_more() {
    let answer = json(
        r#"{"artists":[
            {"id":"m","name":"The Marías","score":100,"country":"US"},
            {"id":"r","name":"Rosalía","score":95,"country":"ES"}
        ]}"#,
    );
    // An exact 95 is taken; the fuzzy 100 is another artist.
    assert_eq!(searched_artist_country(&answer, "ROSALIA"), "ES");
    assert_eq!(searched_artist_country(&answer, "Marias"), "");
    // The same name below 90 is not taken either.
    let low = json(r#"{"artists":[{"id":"r","name":"Rosalía","score":89,"country":"ES"}]}"#);
    assert_eq!(searched_artist_country(&low, "Rosalía"), "");
    // An alias with the same name counts as the artist.
    let alias = json(
        r#"{"artists":[{"id":"k","name":"Kanye West","score":92,"country":"US",
            "aliases":[{"name":"Ye"},{"name":"Yeezy"}]}]}"#,
    );
    assert_eq!(searched_artist_country(&alias, "Ye"), "US");
    assert_eq!(searched_artist_country(&json(r#"{"artists":[]}"#), "Ye"), "");
    assert_eq!(
        artist_search_url("AC/DC").as_deref(),
        Some("artist/?query=artist%3A%22AC%5C%2FDC%22%20OR%20alias%3A%22AC%5C%2FDC%22&limit=3&fmt=json")
    );
    assert_eq!(artist_search_url("  "), None);
}

#[test]
fn a_name_search_takes_an_artist_found_only_by_an_alias_of_the_same_name() {
    let answer = json(
        r#"{"artists":[
            {"id":"p","name":"Puff Daddy & The Family","score":91,"country":"GB"},
            {"id":"s","name":"Sean Combs","score":90,"country":"US",
             "aliases":[{"name":"Puff Daddy"},{"name":"P. Diddy"}]}
        ]}"#,
    );
    assert_eq!(searched_artist_country(&answer, "puff daddy"), "US");
}

#[test]
fn a_name_search_refuses_another_artist_whose_alias_is_only_close() {
    let answer = json(
        r#"{"artists":[{"id":"m","name":"The Marías","score":100,"country":"US",
            "aliases":[{"name":"Marías, The"},{"name":"The Marias Band"}]}]}"#,
    );
    assert_eq!(searched_artist_country(&answer, "Marias"), "");
}

// ---- artists by name, and recording credits ------------------------------------------------

const ARTISTS: &str = r#"{"artists":[
  {"id":"a1","name":"Earth, Wind & Fire","score":100},
  {"id":"a2","name":"Earth, Wind & Fire Experience","score":88},
  {"id":"a1","name":"Earth, Wind & Fire","score":100},
  {"id":"a3","name":"Ye","aliases":[{"name":"Kanye West"},{"name":"Yeezy"}]},
  {"id":"a4","name":"Beyoncé"}
]}"#;

fn artists_named(name: &str) -> Vec<String> {
    let doc: Value = serde_json::from_str(ARTISTS).expect("test JSON parses");
    parse_artists_named(&doc, name)
        .into_iter()
        .map(|a| a.id)
        .collect()
}

#[test]
fn artists_named_keeps_only_the_exact_name_once() {
    assert_eq!(artists_named("earth, wind & fire"), ["a1"]);
    assert_eq!(artists_named("Earth, Wind and Fire"), ["a1"]);
    assert!(artists_named("Earth").is_empty());
}

#[test]
fn artists_named_finds_an_alias_and_an_unaccented_spelling() {
    assert_eq!(artists_named("Kanye West"), ["a3"]);
    assert_eq!(artists_named("Beyonce"), ["a4"]);
    let none: Value = serde_json::from_str(r#"{"error":"nope"}"#).expect("parses");
    assert!(parse_artists_named(&none, "X").is_empty());
}

#[test]
fn the_artist_search_asks_by_name_and_alias() {
    let url = artists_named_url("Bizarrap");
    assert!(url.starts_with("artist/?query="), "{url}");
    assert!(
        url.contains("artist%3A%22Bizarrap%22%20OR%20alias%3A%22Bizarrap%22"),
        "{url}"
    );
    assert!(artists_named_url("A \"B\"").contains("%5C%22B%5C%22"));
}

#[test]
fn recording_credits_are_one_search_for_many_ids() {
    let ids = vec![
        "11111111-2222-3333-4444-555555555555".to_string(),
        "not an id) OR artist:(x".to_string(),
        "AAAAAAAA-2222-3333-4444-555555555555".to_string(),
    ];
    let url = recording_credits_url(&ids).expect("a url");
    assert!(
        url.contains(
            "rid%3A%2811111111-2222-3333-4444-555555555555%20OR%20aaaaaaaa-2222-3333-4444-555555555555%29"
        ),
        "{url}"
    );
    assert!(url.ends_with("&fmt=json&limit=100"));
    assert!(recording_credits_url(&["nope".to_string()]).is_none());
}

#[test]
fn a_recording_credit_without_a_last_join_phrase_reads_as_empty() {
    let doc: Value = serde_json::from_str(
        r#"{"recordings":[{"id":"R1","artist-credit":[
            {"name":"Bizarrap","joinphrase":" & ","artist":{"id":"b"}},
            {"name":"Rauw Alejandro","artist":{"id":"r"}}]},
           {"id":"r2"}]}"#,
    )
    .expect("parses");
    let credits = parse_recording_credits(&doc);
    assert_eq!(
        credits["r1"],
        [
            CreditPart {
                name: "Bizarrap".into(),
                join_phrase: " & ".into()
            },
            CreditPart {
                name: "Rauw Alejandro".into(),
                join_phrase: String::new()
            },
        ]
    );
    assert!(credits["r2"].is_empty());
}

/// Two artists of the same name both scoring 90 or more: which one is meant cannot be told, so
/// no country is taken.
#[test]
fn a_name_search_takes_no_country_when_two_artists_share_the_name() {
    let answer = json(
        r#"{"artists":[
            {"id":"a","name":"Nirvana","score":100,"country":"US"},
            {"id":"b","name":"Nirvana","score":98,"country":"GB"}
        ]}"#,
    );
    assert_eq!(searched_artist_country(&answer, "Nirvana"), "");
    // The same artist listed twice is still one artist.
    let twice = json(
        r#"{"artists":[
            {"id":"a","name":"Nirvana","score":100,"country":"US"},
            {"id":"a","name":"Nirvana","score":100,"country":"US"},
            {"id":"c","name":"Nirvana","score":80,"country":"GB"}
        ]}"#,
    );
    assert_eq!(searched_artist_country(&twice, "Nirvana"), "US");
}
