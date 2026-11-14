//! Tests for SongIdentity: query variants, comparisons, ISRCs, the shared cases file, and
//! targeted checks of the places where Rust's own Unicode and regex behavior is overridden.

use super::*;
use serde_json::Value;

fn texts(queries: &[SongQuery]) -> Vec<String> {
    queries.iter().map(SongQuery::text).collect()
}

// ---- Query variants --------------------------------------------------------------------------

#[test]
fn query_variants_stylized_artist_adds_the_spelled_out_query() {
    let queries = SongIdentity::query_variants("$UICIDE", "$uicideboy$");

    assert_eq!(
        texts(&queries),
        ["$uicideboy$ $UICIDE", "suicideboys SUICIDE", "$UICIDE"]
    );
    assert_eq!(queries.last().expect("a query").artist, "");
}

#[test]
fn query_variants_order_is_original_clean_stylized_primary_title_only() {
    let queries =
        SongIdentity::query_variants("Ca$h (feat. Gue$t) [Official Video]", "A$AP Rocky feat. Gue$t");

    assert_eq!(
        queries,
        [
            SongQuery::new("Ca$h (feat. Gue$t) [Official Video]", "A$AP Rocky feat. Gue$t"),
            SongQuery::new("Ca$h", "A$AP Rocky feat. Gue$t"),
            SongQuery::new("Cash", "ASAP Rocky feat. Guest"),
            SongQuery::new("Ca$h", "A$AP Rocky"),
            SongQuery::new("Ca$h", ""),
        ]
    );
}

#[test]
fn query_variants_nothing_to_clean_is_the_original_and_the_title() {
    assert_eq!(
        texts(&SongIdentity::query_variants("Landed", "Drake")),
        ["Drake Landed", "Landed"]
    );
}

#[test]
fn query_variants_keep_the_version_out_of_the_clean_query_but_the_match_still_needs_it() {
    // A looser query never means a looser match: the studio hit the clean query finds is
    // still not the live song asked for.
    let queries = SongIdentity::query_variants("Creep (Live)", "Radiohead");
    assert!(queries.iter().any(|query| query.title == "Creep"));
    assert_eq!(
        SongIdentity::same_text("Creep (Live)", "Radiohead", "Creep", "Radiohead", None).verdict,
        SongVerdict::SameSongDifferentVersion
    );
}

#[test]
fn query_variants_artist_prefixed_title_loses_the_prefix() {
    assert_eq!(
        texts(&SongIdentity::query_variants("Adele - Hello", "Adele")),
        ["Adele Adele - Hello", "Adele Hello", "Hello"]
    );
}

// ---- Lyrics (the SongIdentity half) -----------------------------------------------------------

#[test]
fn lyrics_a_clean_edits_words_fit_the_song() {
    // The KuGou half of this check lives with the lyrics sources. Here: a download still
    // never takes the clean edit for the song asked for.
    for (want, got) in [
        ("Movie Star", "Movie Star (Clean)"),
        ("Movie Star", "Movie Star (Clean Version)"),
        ("Movie Star (Explicit)", "Movie Star (Censored)"),
    ] {
        assert_ne!(
            SongIdentity::same_text(want, "Artist", got, "Artist", None).verdict,
            SongVerdict::Same,
            "{want} / {got}"
        );
    }
}

// ---- The comparison itself --------------------------------------------------------------------

#[test]
fn same_says_why_and_how_sure() {
    let loose = SongIdentity::same_text("$UICIDE", "$uicideboy$", "Suicide", "Suicideboys", None);
    assert_eq!(loose.verdict, SongVerdict::Same);
    assert!(loose.confidence < 1.0);
    assert!(loose.reason.contains("stylized"), "{}", loose.reason);

    let live = SongIdentity::same_text("Creep", "Radiohead", "Creep (Live)", "Radiohead", None);
    assert_eq!(live.verdict, SongVerdict::SameSongDifferentVersion);
    assert!(live.reason.contains("live"), "{}", live.reason);

    let exact = SongIdentity::same(
        &SongRef::new("Creep", "Radiohead").with_seconds(238.0),
        &SongRef::new("Creep", "Radiohead").with_seconds(239.0),
        None,
    );
    assert_eq!(exact.confidence, 1.0);
}

#[test]
fn same_artist_name_is_the_whole_name_never_a_part() {
    for (a, b, same) in [
        ("Beyoncé", "Beyonce", true),
        ("The Weeknd", "Weeknd", true),
        ("$uicideboy$", "Suicideboys", true),
        ("Kanye West", "Ye", true),
        ("Bob Marley & The Wailers", "Bob Marley", false),
        ("Drake feat. Rihanna", "Rihanna", false),
    ] {
        assert_eq!(SongIdentity::same_artist_name(a, b), same, "{a} / {b}");
    }
}

#[test]
fn title_key_ignores_guests_but_keeps_the_version() {
    assert_eq!(
        SongIdentity::title_key("Too Good"),
        SongIdentity::title_key("Too Good (feat. Rihanna)")
    );
    assert_ne!(
        SongIdentity::title_key("Too Good"),
        SongIdentity::title_key("Too Good (Live)")
    );
}

#[test]
fn match_key_one_song_one_key_one_version_one_key() {
    assert_eq!(
        SongIdentity::match_key("Drake feat. Rihanna", "Too Good"),
        SongIdentity::match_key("Drake", "Too Good (feat. Rihanna)")
    );
    assert_eq!(
        SongIdentity::match_key("Kanye West", "Stronger"),
        SongIdentity::match_key("Ye (侃爷)", "Stronger (Explicit)")
    );
    assert_ne!(
        SongIdentity::match_key("Radiohead", "Creep"),
        SongIdentity::match_key("Radiohead", "Creep (Live)")
    );
}

/// An outside song already in the library is known as owned however the outside source writes
/// it. The first of each pair is spelled as library tags carry it (MusicBrainz spellings kept by
/// Lidarr); the second as Last.fm or Deezer commonly write the same song.
#[test]
fn match_key_the_same_song_gets_the_same_key_however_it_is_written() {
    for (owned_artist, owned_title, artist, title) in [
        ("Damso", "Θ. Macarena", "Damso", "Θ. Macarena"),
        ("Damso", "Θ. Macarena", "DAMSO", "Θ. MACARENA"),
        ("Damso", "Ε. Signaler", "Damso", "Ε. Signaler"),
        ("Damso", "Qui m\u{2019}a demandé", "Damso", "Qui m'a demandé"),
        ("Damso", "T\u{2019}es mon DEL", "Damso", "T'es mon DEL"),
        ("Damso", "Frère", "Damso", "Frere"),
        (
            "SZA feat. James Fauntleroy",
            "Wavy (Interlude)",
            "SZA",
            "Wavy (Interlude)",
        ),
        ("SZA feat. James Fauntleroy", "Wavy (Interlude)", "SZA", "Wavy"),
        (
            "Dr. Dre • Xzibit • Tray-Dee",
            "Lolo (Intro)",
            "Dr. Dre",
            "Lolo (Intro)",
        ),
        (
            "Dr. Dre • Eddie Griffin",
            "Ed-Ucation (Skit)",
            "Dr. Dre",
            "Ed\u{2010}ucation (Skit)",
        ),
        (
            "Kendrick Lamar",
            "Savior (Interlude)",
            "Kendrick Lamar",
            "Savior (Interlude)",
        ),
        (
            "Queen",
            "Bohemian Rhapsody",
            "Queen",
            "Bohemian Rhapsody - Remastered 2011",
        ),
        ("Adele", "Hello", "Adele", "Hello (feat. Someone)"),
    ] {
        assert_eq!(
            SongIdentity::match_key(owned_artist, owned_title),
            SongIdentity::match_key(artist, title),
            "{owned_artist} - {owned_title} / {artist} - {title}"
        );
    }
}

// ---- ISRCs ------------------------------------------------------------------------------------

#[test]
fn normalize_isrc_one_spelling_or_absent() {
    let cases: &[(Option<&str>, Option<&str>)] = &[
        (Some("USRC17607839"), Some("USRC17607839")),
        (Some("us-rc1-76-07839"), Some("USRC17607839")),
        (Some(" US RC1 76 07839 "), Some("USRC17607839")),
        (Some("US.RC1.76.07839"), Some("USRC17607839")),
        (Some("ＵＳＲＣ１７６０７８３９"), Some("USRC17607839")),
        (Some("GBAHT1600302"), Some("GBAHT1600302")),
        (Some("USRC1760783"), None),
        (Some("USRC176078390"), None),
        (Some("1SRC17607839"), None),
        (Some("USRC1760783X"), None),
        (Some("US_RC17607839"), None),
        (Some("ISRC: USRC17607839"), None),
        (Some(""), None),
        (None, None),
    ];
    for &(value, expected) in cases {
        assert_eq!(
            SongIdentity::normalize_isrc(value.unwrap_or("")).as_deref(),
            expected,
            "{value:?}"
        );
    }
}

#[test]
fn shares_isrc_needs_a_valid_code_on_both_sides() {
    let none: [&str; 0] = [];
    assert!(SongIdentity::shares_isrc(["USRC17607839"], ["us-rc1-76-07839"]));
    assert!(SongIdentity::shares_isrc(
        ["GBAHT1600302", "USRC17607839"],
        ["USRC17607839"]
    ));
    assert!(!SongIdentity::shares_isrc(["USRC17607839"], ["GBAHT1600302"]));
    assert!(!SongIdentity::shares_isrc(["USRC17607839"], none));
    assert!(!SongIdentity::shares_isrc(none, ["USRC17607839"]));
    assert!(!SongIdentity::shares_isrc(["junk"], ["junk"]));
}

#[test]
fn same_one_isrc_is_the_same_recording_at_full_confidence() {
    let found = SongIdentity::same(
        &SongRef::new("紅蓮華", "LiSA").with_isrcs(["JPU901901234"]),
        &SongRef::new("Gurenge", "LiSA").with_isrcs(["JP-U90-19-01234"]),
        None,
    );

    assert_eq!(found.verdict, SongVerdict::Same);
    assert_eq!(found.confidence, 1.0);
    assert_eq!(found.reason, "same ISRC");
}

#[test]
fn same_different_isrcs_fall_back_to_the_text_unchanged() {
    let with_codes = SongIdentity::same(
        &SongRef::new("Song (Remastered 2011)", "Artist")
            .with_seconds(200.0)
            .with_isrcs(["GBAAA0100001"]),
        &SongRef::new("Song", "Artist")
            .with_seconds(201.0)
            .with_isrcs(["GBAAA1100002"]),
        None,
    );
    let without = SongIdentity::same(
        &SongRef::new("Song (Remastered 2011)", "Artist").with_seconds(200.0),
        &SongRef::new("Song", "Artist").with_seconds(201.0),
        None,
    );

    assert_eq!(without, with_codes);
}

#[test]
fn known_name_an_alias_leads_to_the_name_the_artist_is_known_by() {
    for (alias, known) in [
        ("Ye", "Kanye West"),
        ("Kanye", "Kanye West"),
        ("ye", "Kanye West"),
        ("Tupac Shakur", "2Pac"),
        ("Puff Daddy", "Diddy"),
        ("Snoop Lion", "Snoop Dogg"),
        ("Yasiin Bey", "Mos Def"),
        ("Biggie Smalls", "The Notorious B.I.G."),
        ("Donald Glover", "Childish Gambino"),
        ("The Artist Formerly Known as Prince", "Prince"),
    ] {
        assert_eq!(SongIdentity::known_name(alias).as_deref(), Some(known), "{alias}");
        // The name found is the same artist by the alias table's own rule.
        assert!(SongIdentity::same_artist_name(alias, known), "{alias}");
    }
}

#[test]
fn known_name_none_for_a_name_that_is_not_an_alias() {
    // "" twice: an empty name and a missing one read the same.
    for artist in ["Kanye West", "2Pac", "Diddy", "Radiohead", "", ""] {
        assert_eq!(SongIdentity::known_name(artist), None, "{artist}");
    }
}

#[test]
fn has_other_names_for_both_sides_of_an_alias_and_only_the_main_credit() {
    for artist in [
        "Kanye West",
        "Ye",
        "Kanye West feat. Jay-Z",
        "2Pac",
        "Makaveli",
        "Diddy",
        "The Notorious B.I.G.",
    ] {
        assert!(SongIdentity::has_other_names(artist), "{artist}");
    }
    for artist in ["Radiohead", "Jay-Z feat. Kanye West", "Air", ""] {
        assert!(!SongIdentity::has_other_names(artist), "{artist}");
    }
}

// ---- The shared cases file ---------------------------------------------------------------------

fn cases() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/song-identity-cases.json");
    let text = std::fs::read_to_string(path).expect("docs/song-identity-cases.json is in the repo");
    serde_json::from_str(&text).expect("the cases file is JSON")
}

fn strings(array: &Value) -> Vec<String> {
    array
        .as_array()
        .expect("an array")
        .iter()
        .map(|value| value.as_str().expect("a string").to_string())
        .collect()
}

fn text_of(value: &Value) -> &str {
    // GetString() of a JSON null is null, which every SongIdentity call reads as empty.
    value.as_str().unwrap_or("")
}

fn song_ref(side: &Value) -> SongRef {
    let mut song = SongRef::new(text_of(&side["title"]), text_of(&side["artist"]));
    song.seconds = side.get("seconds").and_then(Value::as_f64);
    // One ISRC as a string, or several as a list. Absent in every case written before it.
    song.isrcs = match side.get("isrc") {
        Some(Value::Array(_)) => strings(&side["isrc"]),
        Some(isrc) => vec![isrc.as_str().expect("an ISRC string").to_string()],
        None => Vec::new(),
    };
    song
}

fn options_of(item: &Value) -> SongMatchOptions {
    let mut result = SongMatchOptions::default();
    let Some(options) = item.get("options") else {
        return result;
    };
    if let Some(tolerance) = options.get("lengthToleranceSeconds") {
        result.length_tolerance_seconds = tolerance.as_i64().map(|t| t as i32);
    }
    if let Some(extras) = options.get("extrasMustAgree") {
        result.extras_must_agree = extras.as_bool().expect("a bool");
    }
    if let Some(neutral) = options.get("alsoNeutral") {
        result.also_neutral = strings(neutral);
    }
    result
}

#[test]
fn the_file_has_enough_cases() {
    let cases = cases();
    assert!(cases["compare"].as_array().expect("compare").len() >= 120);
    assert!(cases["parse"].as_array().expect("parse").len() >= 30);
}

#[test]
fn compare_case() {
    let cases = cases();
    let mut failures = Vec::new();
    for item in cases["compare"].as_array().expect("compare") {
        let note = text_of(&item["note"]);
        let a = song_ref(&item["a"]);
        let b = song_ref(&item["b"]);
        let options = options_of(item);
        let expect = match text_of(&item["expect"]) {
            "same" => SongVerdict::Same,
            "version" => SongVerdict::SameSongDifferentVersion,
            "different" => SongVerdict::Different,
            other => panic!("unknown expectation '{other}'"),
        };

        let forward = SongIdentity::same(&a, &b, Some(&options));
        let backward = SongIdentity::same(&b, &a, Some(&options));
        if forward.verdict != expect {
            failures.push((
                note.to_string(),
                format!(
                    "{note} expected {expect:?}, got {:?} ({})",
                    forward.verdict, forward.reason
                ),
            ));
        }
        // A comparison reads the same from either side.
        if backward.verdict != expect {
            failures.push((
                note.to_string(),
                format!(
                    "{note} (reversed) expected {expect:?}, got {:?} ({})",
                    backward.verdict, backward.reason
                ),
            ));
        }
        assert!(
            (0.0..=1.0).contains(&forward.confidence),
            "{note}: confidence {}",
            forward.confidence
        );
        assert!(!is_blank(&forward.reason), "{note}: no reason");
    }
    let failures: Vec<String> = failures.into_iter().map(|(_, line)| line).collect();
    assert!(
        failures.is_empty(),
        "{} failing:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn parse_case() {
    let cases = cases();
    let mut failures = Vec::new();
    for item in cases["parse"].as_array().expect("parse") {
        let note = text_of(&item["note"]);
        let input = &item["input"];
        let title = text_of(&input["title"]);
        // null and "" differ for parse_title's artist.
        let artist = input["artist"].as_str();

        let parsed = SongIdentity::parse_title(title, artist);
        if text_of(&item["expectTitleKey"]) != parsed.key {
            failures.push((note.to_string(), format!("{note}: title key '{}'", parsed.key)));
        }
        if text_of(&item["expectLooseKey"]) != parsed.loose_key {
            failures.push((
                note.to_string(),
                format!("{note}: loose key '{}'", parsed.loose_key),
            ));
        }
        if strings(&item["expectVersions"]) != parsed.versions {
            failures.push((
                note.to_string(),
                format!("{note}: versions {:?}", parsed.versions),
            ));
        }

        let credit_text = match artist {
            Some(artist) if !is_blank(artist) => artist,
            _ => parsed.artist_from_title.as_deref().unwrap_or(""),
        };
        let credit = SongIdentity::parse_artists(credit_text);
        let artists = distinct(
            credit
                .names
                .iter()
                .chain(&credit.featured)
                .chain(&parsed.featured)
                .map(|name| SongIdentity::key(name)),
        );
        if strings(&item["expectArtists"]) != artists {
            failures.push((
                note.to_string(),
                format!("{note}: artists [{}]", artists.join(", ")),
            ));
        }
    }
    let failures: Vec<String> = failures.into_iter().map(|(_, line)| line).collect();
    assert!(
        failures.is_empty(),
        "{} failing:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn query_case() {
    let cases = cases();
    let mut failures = Vec::new();
    for item in cases["queries"].as_array().expect("queries") {
        let note = text_of(&item["note"]);
        let input = &item["input"];
        let queries = texts(&SongIdentity::query_variants(
            text_of(&input["title"]),
            text_of(&input["artist"]),
        ));
        if strings(&item["expect"]) != queries {
            failures.push(format!("{note}: [{}]", queries.join(" | ")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failing:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ---- Lookup cases: docs/song-identity-lookup-cases.json ------------------------------------

fn lookup_cases() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/song-identity-lookup-cases.json"
    );
    let text = std::fs::read_to_string(path).expect("docs/song-identity-lookup-cases.json is in the repo");
    serde_json::from_str(&text).expect("the lookup cases file is JSON")
}

const LOOKUPS: [(&str, Lookup); 2] = [("owned", Lookup::Owned), ("pick", Lookup::Pick)];

fn verdict_name(verdict: SongVerdict) -> &'static str {
    match verdict {
        SongVerdict::Same => "same",
        SongVerdict::SameSongDifferentVersion => "version",
        SongVerdict::Different => "different",
    }
}

/// The verdict both ways round, or a line saying they disagree.
fn both_ways(a: &SongRef, b: &SongRef, options: &SongMatchOptions) -> Result<SongMatch, String> {
    let forward = SongIdentity::same(a, b, Some(options));
    let backward = SongIdentity::same(b, a, Some(options));
    if forward.verdict != backward.verdict {
        return Err(format!(
            "{} one way ({}), {} the other ({})",
            verdict_name(forward.verdict),
            forward.reason,
            verdict_name(backward.verdict),
            backward.reason
        ));
    }
    assert!(
        (0.0..=1.0).contains(&forward.confidence),
        "confidence {}",
        forward.confidence
    );
    assert!(!is_blank(&forward.reason), "no reason");
    Ok(forward)
}

/// The lookup file's options: a length tolerance of 8 s unless the case says otherwise (null
/// compares no lengths), and whatever else the contract cases' options allow.
fn lookup_options_of(item: &Value, lookup: Lookup) -> SongMatchOptions {
    let mut options = options_of(item);
    if item
        .get("options")
        .and_then(|options| options.get("lengthToleranceSeconds"))
        .is_none()
    {
        options.length_tolerance_seconds = Some(8);
    }
    options.lookup = lookup;
    options
}

#[test]
fn lookup_cases_pass() {
    let cases = lookup_cases();
    let items = cases["cases"].as_array().expect("cases");
    assert!(items.len() >= 60, "{} cases", items.len());
    let mut failures = Vec::new();
    for item in items {
        let note = text_of(&item["note"]);
        let a = song_ref(&item["a"]);
        let b = song_ref(&item["b"]);
        for (mode, lookup) in LOOKUPS {
            let expect = item[mode]
                .as_str()
                .unwrap_or_else(|| panic!("{note}: no '{mode}' verdict"));
            match both_ways(&a, &b, &lookup_options_of(item, lookup)) {
                Ok(found) if verdict_name(found.verdict) == expect => {}
                Ok(found) => failures.push(format!(
                    "{note} [{mode}] expected {expect}, got {} ({})",
                    verdict_name(found.verdict),
                    found.reason
                )),
                Err(line) => failures.push(format!("{note} [{mode}] {line}")),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} failing:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn lookup_romanized_keys_are_as_recorded() {
    let cases = lookup_cases();
    let mut failures = Vec::new();
    for item in cases["romanized"].as_array().expect("romanized") {
        let text = text_of(&item["text"]);
        let key = crate::common::romanize::romanized_key(text);
        if key != text_of(&item["key"]) {
            failures.push(format!("{text}: '{key}'"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every contract case again with each lookup on. A verdict that moves must be listed, with what
/// it was and why, and a listed move must still happen.
#[test]
fn lookup_contract_cases_move_only_where_listed() {
    let contract = cases();
    let lookup = lookup_cases();
    let listed: Vec<&Value> = lookup["contractChanges"]
        .as_array()
        .expect("contractChanges")
        .iter()
        .collect();
    let mut seen = vec![false; listed.len()];
    let mut failures = Vec::new();
    for (index, item) in contract["compare"]
        .as_array()
        .expect("compare")
        .iter()
        .enumerate()
    {
        let note = text_of(&item["note"]);
        let a = song_ref(&item["a"]);
        let b = song_ref(&item["b"]);
        let was = text_of(&item["expect"]);
        for (mode, lookup) in LOOKUPS {
            let mut options = options_of(item);
            options.lookup = lookup;
            let got = match both_ways(&a, &b, &options) {
                Ok(found) => verdict_name(found.verdict),
                Err(line) => {
                    failures.push(format!("#{index} {note} [{mode}] {line}"));
                    continue;
                }
            };
            let entry = listed.iter().position(|change| {
                change["index"].as_u64() == Some(index as u64) && text_of(&change["mode"]) == mode
            });
            match entry {
                Some(at) => {
                    seen[at] = true;
                    let change = listed[at];
                    if text_of(&change["note"]) != note || text_of(&change["was"]) != was {
                        failures.push(format!("#{index} [{mode}] listed with another note or 'was'"));
                    }
                    if text_of(&change["expect"]) != got {
                        failures.push(format!(
                            "#{index} {note} [{mode}] listed as {}, got {got}",
                            text_of(&change["expect"])
                        ));
                    }
                    if is_blank(text_of(&change["reason"])) {
                        failures.push(format!("#{index} [{mode}] listed without a reason"));
                    }
                }
                None if got != was => {
                    failures.push(format!(
                        "#{index} {note} [{mode}] moved from {was} to {got}, not listed"
                    ));
                }
                None => {}
            }
        }
    }
    for (at, change) in listed.iter().enumerate() {
        if !seen[at] {
            failures.push(format!("listed change {change} matches no contract case"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failing:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn score_ranks_the_closer_length_and_title_higher() {
    let want = SongRef::new("Bohemian Rhapsody", "Queen").with_seconds(354.0);
    let exact = SongRef::new("Bohemian Rhapsody", "Queen").with_seconds(354.0);
    let off_by_four = SongRef::new("Bohemian Rhapsody", "Queen").with_seconds(358.0);
    let typo = SongRef::new("Bohemian Rapsody", "Queen").with_seconds(354.0);
    let unknown = SongRef::new("Bohemian Rhapsody", "Queen");

    let score = |b: &SongRef| SongIdentity::score(&want, b);
    assert!((score(&exact) - 1.0).abs() < 1e-9, "{}", score(&exact));
    assert!(score(&exact) > score(&off_by_four));
    assert!(score(&exact) > score(&typo));
    assert!((score(&unknown) - 0.9).abs() < 1e-9, "{}", score(&unknown));
}

#[test]
fn score_reads_a_title_across_scripts() {
    let want = SongRef::new("Kamin", "EMIN").with_seconds(200.0);
    let cyrillic = SongRef::new("Камин", "EMIN").with_seconds(200.0);
    assert!(SongIdentity::score(&want, &cyrillic) > 0.99);
}

#[test]
fn pick_best_takes_the_best_score_and_refuses_a_near_tie_between_two_songs() {
    let want = SongRef::new("Bohemian Rhapsody", "Queen").with_seconds(354.0);
    let pick = SongIdentity::pick(5);

    // The closer length wins over the search's order.
    let candidates = [
        SongRef::new("Bohemian Rhapsody", "Queen").with_seconds(358.0),
        SongRef::new("Bohemian Rhapsody", "Queen").with_seconds(354.0),
    ];
    assert_eq!(SongIdentity::pick_best(&want, &candidates, &pick), Some(1));

    // Two copies of one song at one length: the first.
    let copies = [
        SongRef::new("Bohemian Rhapsody", "Queen").with_seconds(354.0),
        SongRef::new("Bohemian Rhapsody (Remastered 2011)", "Queen").with_seconds(354.0),
    ];
    assert_eq!(SongIdentity::pick_best(&want, &copies, &pick), Some(0));

    // Two different titles, each a typo away and as close as the other: no pick.
    let want_typo = SongRef::new("Bohemian Rhapsodx", "Queen").with_seconds(354.0);
    let rivals = [
        SongRef::new("Bohemian Rhapsody", "Queen").with_seconds(354.0),
        SongRef::new("Bohemian Rhapsodz", "Queen").with_seconds(354.0),
    ];
    assert_eq!(SongIdentity::pick_best(&want_typo, &rivals, &pick), None);

    // Nothing accepted: nothing picked.
    let others = [SongRef::new("Killer Queen", "Queen").with_seconds(180.0)];
    assert_eq!(SongIdentity::pick_best(&want, &others, &pick), None);
}

#[test]
fn lookup_off_is_the_default_and_changes_nothing() {
    assert_eq!(SongMatchOptions::default().lookup, Lookup::Off);
    let a = SongRef::new("Kamin", "EMIN").with_seconds(200.0);
    let b = SongRef::new("Камин", "EMIN").with_seconds(200.0);
    assert_eq!(SongIdentity::same(&a, &b, None).verdict, SongVerdict::Different);
    assert_eq!(
        SongIdentity::same(&a, &b, Some(&SongIdentity::owned(8))).verdict,
        SongVerdict::Same
    );
}

// ---- Folding, keying and the overridden Unicode and regex rules ------------------------------

#[test]
fn fold_makes_lookalikes_plain() {
    assert_eq!(SongIdentity::fold("  ＄uicide（Live）  "), "$uicide(Live)");
    assert_eq!(
        SongIdentity::fold("Don’t Stop — Now_Please"),
        "Don't Stop - Now Please"
    );
    assert_eq!(SongIdentity::fold("Rock´n´Roll"), "Rock'n'Roll");
    // A Cyrillic "о" and "е" inside a Latin word are read as Latin...
    assert_eq!(SongIdentity::fold("Hоmеsick"), "Homesick");
    // ...and a word wholly in Cyrillic is left alone.
    assert_eq!(SongIdentity::fold("Кино"), "Кино");
    assert_eq!(SongIdentity::fold("   "), "");
}

#[test]
fn key_lowercases_one_character_at_a_time() {
    // No final sigma: every Σ lowercases to σ.
    assert_eq!(SongIdentity::key("ΟΔΟΣ"), "οδοσ");
    // Lowercasing leaves İ alone (it would only become an ASCII i); its dot is then stripped as
    // an accent, leaving a capital I in the key.
    assert_eq!(SongIdentity::key("İstanbul"), "Istanbul");
    assert_eq!(SongIdentity::key("Straße & Œuvre"), "strasseandoeuvre");
    assert_eq!(SongIdentity::key("Hoppípolla"), SongIdentity::key("Hoppipolla"));
    // A kana's voicing mark is a different letter and stays.
    assert_ne!(SongIdentity::key("ガ"), SongIdentity::key("カ"));
    // Symbols are the name when there is nothing else.
    assert_eq!(SongIdentity::key("!!!"), "!!!");
    assert_eq!(SongIdentity::key("🔥 🔥"), "🔥🔥");
}

#[test]
fn track_numbers_go_only_with_a_letter_after_them() {
    assert_eq!(SongIdentity::parse_title("01 - Creep", None).core, "Creep");
    assert_eq!(SongIdentity::parse_title("1-01 Creep", None).core, "Creep");
    assert_eq!(SongIdentity::parse_title("2) Creep", None).core, "Creep");
    assert_eq!(SongIdentity::parse_title("99 Problems", None).core, "99 Problems");
    assert_eq!(
        SongIdentity::parse_title("1-800-273-8255", None).key,
        "18002738255"
    );
}

#[test]
fn an_x_is_a_separator_only_when_no_guest_word_follows() {
    let names = |credit: &str| SongIdentity::parse_artists(credit).names;
    assert_eq!(names("A x B"), ["A", "B"]);
    assert_eq!(names("A X B"), ["A", "B"]);
    // The lookahead refuses the x, and the search goes on to the next separator.
    assert_eq!(names("A x feat. B"), ["A x", "B"]);
    assert_eq!(names("A x & B"), ["A x", "B"]);
    assert_eq!(names("Chloe x Halle"), ["Chloe x Halle"]);
}

#[test]
fn channel_names_are_not_the_artist() {
    assert_eq!(
        SongIdentity::parse_artists("Rick Astley - Topic").display,
        "Rick Astley"
    );
    assert_eq!(
        SongIdentity::parse_artists("RickAstleyVEVO").display,
        "RickAstley"
    );
    assert_eq!(
        SongIdentity::parse_artists("Rick Astley VEVO").display,
        "Rick Astley"
    );
    // Ignoring case, the lookbehind \p{Ll} takes any cased letter, but not a CJK one.
    assert_eq!(SongIdentity::parse_artists("RICKVEVO").display, "RICK");
    assert_eq!(SongIdentity::parse_artists("侃爷VEVO").display, "侃爷VEVO");
    assert_eq!(SongIdentity::parse_artists("ǅVEVO").display, "Dž");
}

#[test]
fn word_boundaries_and_classes_test_one_utf16_unit() {
    // A Devanagari vowel sign (a spacing mark) is no word character here, so "live" before
    // it is still the word live; to Rust's own \b it would not be.
    assert_eq!(
        SongIdentity::parse_title("Song (live\u{093E})", None).versions,
        ["live"]
    );
    assert!(crate::common::live_version::mentions(Some("Live\u{093E}")));
    // \p{L} matches no supplementary-plane letter, so one splits a word: the Cyrillic "с" after
    // it is a word of its own and stays Cyrillic.
    assert_eq!(SongIdentity::fold("a𠀀с"), "a𠀀с");
    assert_eq!(SongIdentity::fold("aс"), "ac");
    // Nor is a supplementary-plane digit a \d.
    assert_eq!(
        SongIdentity::parse_title("Song (𑁧)", None).extras,
        [SongIdentity::key("𑁧")]
    );
}

#[test]
fn part_numbers_are_spelled_one_way() {
    assert_eq!(
        SongIdentity::parse_title("Song Part II", None).key,
        SongIdentity::parse_title("Song Pt. 2", None).key
    );
    assert_eq!(SongIdentity::parse_title("Song (Vol. 3)", None).key, "songvol3");
}

#[test]
fn lengths_are_written_with_at_most_one_decimal() {
    let found = SongIdentity::same(
        &SongRef::new("Song", "Artist").with_seconds(200.0),
        &SongRef::new("Song", "Artist").with_seconds(204.25),
        None,
    );
    assert_eq!(found.verdict, SongVerdict::SameSongDifferentVersion);
    assert_eq!(found.reason, "lengths differ by 4.3 s");
    let whole = SongIdentity::same(
        &SongRef::new("Song", "Artist").with_seconds(200.0),
        &SongRef::new("Song", "Artist").with_seconds(210.0),
        None,
    );
    assert_eq!(whole.reason, "lengths differ by 10 s");
}

#[test]
fn length_fits_when_unknown_or_close() {
    assert!(SongIdentity::length_fits(None, Some(200.0), 3));
    assert!(SongIdentity::length_fits(Some(200), None, 3));
    assert!(SongIdentity::length_fits(Some(0), Some(10.0), 3));
    assert!(SongIdentity::length_fits(Some(200), Some(203.0), 3));
    assert!(!SongIdentity::length_fits(Some(200), Some(203.5), 3));
}

#[test]
fn strip_features_keeps_everything_else() {
    assert_eq!(
        SongIdentity::strip_features("Too Good (feat. Rihanna) (Live)"),
        "Too Good (Live)"
    );
    assert_eq!(SongIdentity::strip_features("Too Good feat. Rihanna"), "Too Good");
    assert_eq!(SongIdentity::strip_features("feat. Rihanna"), "feat. Rihanna");
}

#[test]
fn added_versions_are_one_directional() {
    let added = SongIdentity::added_versions("Song", "Song (Live)", None);
    assert_eq!(added.into_iter().collect::<Vec<_>>(), ["live"]);
    assert!(SongIdentity::added_versions("Song (Live)", "Song", None).is_empty());
    assert!(SongIdentity::added_versions("Song", "Song (Remastered 2011)", None).is_empty());
}

#[test]
fn primary_artist_is_the_first_name_as_written() {
    assert_eq!(SongIdentity::primary_artist("Beyoncé feat. Jay-Z"), "Beyoncé");
    assert_eq!(
        SongIdentity::primary_artist("Tyler, The Creator"),
        "Tyler, The Creator"
    );
    assert_eq!(
        SongIdentity::primary_artist("Bob Marley & The Wailers"),
        "Bob Marley & The Wailers"
    );
}

#[test]
fn listed_credits_stand_in_for_splitting() {
    assert!(SongIdentity::artists_agree_with_credits(
        "Lil Peep",
        "Lil Peep feat. Someone",
        &["Lil Peep"]
    ));
    let none: [&str; 0] = [];
    assert_eq!(
        SongIdentity::compare_artists_with_credits("Radiohead", "Radiohead", &none),
        ArtistAgreement::Agree
    );
    assert_eq!(
        SongIdentity::compare_artists("", "Radiohead"),
        ArtistAgreement::Unknown
    );
}
