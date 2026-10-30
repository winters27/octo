//! Tests for reading AcoustID's whole answer against R: every frozen answer reads as the reference
//! reading does, the lone-submission floor, a fingerprint AcoustID has not linked, answers that
//! hold two mixes, the believed different song, the one recording an answer gives and the song
//! name comparisons under them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;
use crate::fingerprint::acoust_id_client::{AnswerRecording, AnswerResult, parse_answer};

/// Octo's default match score, 85%.
const MATCH_SCORE: f64 = 0.85;

const BEATLES: (&str, &str) = ("b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d", "The Beatles");
const OASIS: (&str, &str) = ("39ab1aed-75e0-4140-bd47-540276886b60", "Oasis");
const VALE: (&str, &str) = ("5a1e0000-0000-4000-8000-00000000a1e0", "Ezra Vale");
const VALE_AGAIN: (&str, &str) = ("5a1e0000-0000-4000-8000-00000000a1e0", "E.V.");
const COVER: (&str, &str) = ("c0de0000-0000-4000-8000-00000000c0de", "The Cover Band");

const HELP_2009: &str = "4416d38e-0f68-4c75-93b9-c1a793fccb49";
const HELP_2015: &str = "01b6e4de-ff48-4ec0-862e-cd623c924d7b";
const WALRUS_2009: &str = "3eb7efbb-8717-4034-a698-109c70798524";
const WALRUS_2023: &str = "21ca4704-2263-4e9f-ae4b-412af5d13e1e";

/// What the reference reading gives for each frozen answer against the R and routing of a print,
/// one row per pair: the answer, the print whose R it is read against, the reading, the dominant
/// recording's first 8 characters, R's share of the weight, how many recordings the answer links,
/// their weight together, and whether the dominant recording is R's song, a believed different
/// song and the one recording the answer gives.
const READINGS: &str = "
constructed-another-song-240 help-2009              another   c0ffee00 0.021739 2  920   no  yes no
constructed-another-song-240 help-2015              another   c0ffee00 0.000000 2  920   no  yes no
constructed-another-song-240 help-catalog-1967-1970 another   c0ffee00 0.000000 2  920   no  yes no
constructed-clear-2009-120   help-2009              clear     4416d38e 1.000000 1  50    yes no  yes
constructed-clear-2009-120   help-2015              another   4416d38e 0.000000 1  50    yes no  yes
constructed-clear-2009-120   help-catalog-1967-1970 another   4416d38e 0.000000 1  50    yes no  yes
help-2009-120                help-2009              ambiguous 4416d38e 0.726167 67 14958 yes no  yes
help-2009-120                help-2015              ambiguous 4416d38e 0.028145 67 14958 yes no  yes
help-2009-120                help-catalog-1967-1970 another   4416d38e 0.000535 67 14958 yes no  yes
help-2009-240                help-2009              ambiguous 4416d38e 0.726167 67 14958 yes no  yes
help-2009-240                help-2015              ambiguous 4416d38e 0.028145 67 14958 yes no  yes
help-2009-240                help-catalog-1967-1970 another   4416d38e 0.000535 67 14958 yes no  yes
help-2015-120                help-2009              ambiguous 4416d38e 0.688150 44 11823 yes no  yes
help-2015-120                help-2015              ambiguous 4416d38e 0.033494 44 11823 yes no  yes
help-2015-120                help-catalog-1967-1970 another   4416d38e 0.000507 44 11823 yes no  yes
help-2015-240                help-2009              ambiguous 4416d38e 0.688150 44 11823 yes no  yes
help-2015-240                help-2015              ambiguous 4416d38e 0.033494 44 11823 yes no  yes
help-2015-240                help-catalog-1967-1970 another   4416d38e 0.000507 44 11823 yes no  yes
lovestory-2008-120           lovestory-2008         ambiguous 9086b742 0.983897 3  621   yes no  yes
lovestory-2008-120           lovestory-tv           another   9086b742 0.000000 3  621   yes no  yes
lovestory-2008-120           help-2009              another   9086b742 0.000000 3  621   no  yes no
lovestory-2008-240           lovestory-2008         ambiguous 9086b742 0.983897 3  621   yes no  yes
lovestory-2008-240           lovestory-tv           another   9086b742 0.000000 3  621   yes no  yes
lovestory-tv-120             lovestory-2008         ambiguous 93484aa2 0.012263 7  897   yes no  yes
lovestory-tv-120             lovestory-tv           ambiguous 93484aa2 0.966555 7  897   yes no  yes
lovestory-tv-240             lovestory-2008         ambiguous 93484aa2 0.012263 7  897   yes no  yes
lovestory-tv-240             lovestory-tv           ambiguous 93484aa2 0.966555 7  897   yes no  yes
lovestory-tv-chapter-120     lovestory-2008         ambiguous 93484aa2 0.012263 7  897   yes no  yes
lovestory-tv-chapter-120     lovestory-tv           ambiguous 93484aa2 0.966555 7  897   yes no  yes
lovestory-tv-chapter-240     lovestory-2008         ambiguous 93484aa2 0.012263 7  897   yes no  yes
lovestory-tv-chapter-240     lovestory-tv           ambiguous 93484aa2 0.966555 7  897   yes no  yes
lovestory-tv-more-120        lovestory-2008         ambiguous 93484aa2 0.012263 7  897   yes no  yes
lovestory-tv-more-120        lovestory-tv           ambiguous 93484aa2 0.966555 7  897   yes no  yes
lovestory-tv-more-240        lovestory-2008         ambiguous 93484aa2 0.012263 7  897   yes no  yes
lovestory-tv-more-240        lovestory-tv           ambiguous 93484aa2 0.966555 7  897   yes no  yes
mrsrobinson-live-120         mrsrobinson-live       ambiguous 047e8a87 0.650224 3  892   yes no  yes
mrsrobinson-live-120         mrsrobinson-studio     ambiguous 047e8a87 0.343049 3  892   yes no  yes
mrsrobinson-live-240         mrsrobinson-live       ambiguous 047e8a87 0.650224 3  892   yes no  yes
mrsrobinson-live-240         mrsrobinson-studio     ambiguous 047e8a87 0.343049 3  892   yes no  yes
mrsrobinson-studio-120       mrsrobinson-live       another   df569676 0.000532 17 5638  yes no  yes
mrsrobinson-studio-120       mrsrobinson-studio     ambiguous df569676 0.973395 17 5638  yes no  yes
mrsrobinson-studio-120       yesterday-2009         another   df569676 0.000000 17 5638  no  yes no
mrsrobinson-studio-240       mrsrobinson-live       another   df569676 0.000532 17 5638  yes no  yes
mrsrobinson-studio-240       mrsrobinson-studio     ambiguous df569676 0.973395 17 5638  yes no  yes
walrus-2009-120              walrus-2009            ambiguous 3eb7efbb 0.801838 32 7292  yes no  yes
walrus-2009-120              walrus-2023            another   3eb7efbb 0.000274 32 7292  yes no  yes
walrus-2009-120              lovestory-tv           another   3eb7efbb 0.000000 32 7292  no  yes no
walrus-2009-240              walrus-2009            ambiguous 3eb7efbb 0.801838 32 7292  yes no  yes
walrus-2009-240              walrus-2023            another   3eb7efbb 0.000274 32 7292  yes no  yes
walrus-2023-120              walrus-2009            another   21ca4704 0.000000 2  622   yes no  yes
walrus-2023-120              walrus-2023            ambiguous 21ca4704 0.995177 2  622   yes no  yes
walrus-2023-240              walrus-2009            another   21ca4704 0.000000 2  622   yes no  yes
walrus-2023-240              walrus-2023            ambiguous 21ca4704 0.995177 2  622   yes no  yes
yesterday-2009-120           yesterday-2009         ambiguous c05194a3 0.636972 57 13594 yes no  yes
yesterday-2009-120           yesterday-2015         ambiguous c05194a3 0.026924 57 13594 yes no  yes
yesterday-2009-240           yesterday-2009         ambiguous c05194a3 0.636972 57 13594 yes no  yes
yesterday-2009-240           yesterday-2015         ambiguous c05194a3 0.026924 57 13594 yes no  yes
yesterday-2015-120           yesterday-2009         ambiguous c05194a3 0.598840 52 11726 yes no  yes
yesterday-2015-120           yesterday-2015         ambiguous c05194a3 0.033515 52 11726 yes no  yes
yesterday-2015-240           yesterday-2009         ambiguous c05194a3 0.598840 52 11726 yes no  yes
yesterday-2015-240           yesterday-2015         ambiguous c05194a3 0.033515 52 11726 yes no  yes
";

fn gate() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heart_album/gate")
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The tab-separated rows of a fixture table, its comment lines left out.
fn rows(path: &Path) -> Vec<Vec<String>> {
    read_text(path)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.split('\t').map(str::to_string).collect())
        .collect()
}

/// Each print's R from `print-r.tsv`, with its routing's title and artist from
/// `print-routing.tsv`.
fn print_rs() -> BTreeMap<String, HeardRecording> {
    let mut rs = BTreeMap::new();
    for row in rows(&gate().join("print-r.tsv")) {
        let r = HeardRecording {
            ids: row[2].split(',').map(str::to_string).collect(),
            ..Default::default()
        };
        rs.insert(row[0].clone(), r);
    }
    for row in rows(&gate().join("print-routing.tsv")) {
        let r = rs.get_mut(&row[0]).expect("every routing row has an R");
        r.title = row[1].clone();
        r.artist = row[2].clone();
    }
    rs
}

/// A frozen answer, read whole.
fn frozen(name: &str) -> AcoustIdAnswer {
    let path = gate().join(format!("answers/{name}.json"));
    let file: serde_json::Value = serde_json::from_str(&read_text(&path)).expect("an answer file is JSON");
    parse_answer(&file["answer"])
        .expect("the answer reads")
        .expect("the answer's status is ok")
}

fn reading_named(name: &str) -> Reading {
    match name {
        "clear" => Reading::Clear,
        "ambiguous" => Reading::Ambiguous,
        "another" => Reading::Another,
        "empty" => Reading::Empty,
        "unknown" => Reading::Unknown,
        other => panic!("no reading is named {other:?}"),
    }
}

#[test]
fn every_frozen_answer_reads_as_the_reference_reads_it() {
    let rs = print_rs();
    let mut answers: BTreeMap<&str, AcoustIdAnswer> = BTreeMap::new();
    let mut pairs = 0;
    for line in READINGS.lines().filter(|line| !line.trim().is_empty()) {
        let &[
            name,
            r_name,
            reading,
            dominant,
            share,
            count,
            weight,
            same,
            believed,
            one,
        ] = line.split_whitespace().collect::<Vec<_>>().as_slice()
        else {
            panic!("a row has ten fields: {line}");
        };
        let answer = answers.entry(name).or_insert_with(|| frozen(name));
        let r = &rs[r_name];
        let got = read_answer(answer, r, MATCH_SCORE);
        let pair = format!("{name} against {r_name}");
        assert_eq!(got.reading, reading_named(reading), "{pair}");
        let want_share: f64 = share.parse().expect("a share");
        assert!((got.r_share - want_share).abs() < 5e-7, "{pair}: {}", got.r_share);
        assert_eq!(got.recordings.len().to_string(), count, "{pair}");
        assert_eq!(got.total_weight().to_string(), weight, "{pair}");
        let top = got
            .dominant()
            .unwrap_or_else(|| panic!("{pair}: a dominant recording"));
        assert!(top.id.starts_with(dominant), "{pair}: {}", top.id);
        assert_eq!(same_song(&got, r, top), same == "yes", "{pair}: same song");
        assert_eq!(
            believed_other_song(&got, r, top),
            believed == "yes",
            "{pair}: believed"
        );
        assert_eq!(
            one_recording(&got, r).map(|found| found.id.as_str()),
            (one == "yes").then_some(top.id.as_str()),
            "{pair}: one recording"
        );
        pairs += 1;
    }
    // Every real answer in the folder is replayed; constructed ones are named so.
    let constructed = |name: &str| name.starts_with("constructed-");
    let mut real: Vec<String> = std::fs::read_dir(gate().join("answers"))
        .expect("the answers folder")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".json").map(str::to_string)
        })
        .filter(|name| !constructed(name))
        .collect();
    real.sort();
    let replayed: Vec<&str> = answers
        .keys()
        .copied()
        .filter(|name| !constructed(name))
        .collect();
    assert_eq!(real, replayed);
    assert_eq!((answers.len(), pairs), (26, 61));
}

/// A recording credited to one artist (MusicBrainz id, name), with its ISRC when it has one.
fn recording(
    id: &str,
    title: &str,
    artist: (&str, &str),
    isrc: Option<&str>,
    sources: i64,
) -> AnswerRecording {
    AnswerRecording {
        id: id.into(),
        title: title.into(),
        artists: vec![AnswerArtist {
            id: Some(artist.0.into()),
            name: artist.1.into(),
        }],
        duration_seconds: None,
        isrcs: isrc.into_iter().map(str::to_string).collect(),
        sources,
    }
}

fn result(score: f64, recordings: Vec<AnswerRecording>) -> AnswerResult {
    AnswerResult {
        id: None,
        score,
        recordings,
    }
}

fn answer(results: Vec<AnswerResult>) -> AcoustIdAnswer {
    AcoustIdAnswer { results }
}

/// R as a Help! routing gives it: these recording ids, the routing's title and artist, and no
/// artist ids.
fn help_r(ids: &[&str]) -> HeardRecording {
    HeardRecording {
        ids: ids.iter().map(|id| id.to_string()).collect(),
        title: "Help!".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    }
}

/// R as a "Stronger" routing by Ezra Vale gives it, with the artist's id when `with_id`.
fn stronger_r(with_id: bool) -> HeardRecording {
    HeardRecording {
        ids: vec!["s-heard".into()],
        title: "Stronger".into(),
        artist: VALE.1.into(),
        artist_ids: if with_id { vec![VALE.0.into()] } else { Vec::new() },
        ..Default::default()
    }
}

fn help(id: &str, sources: i64) -> AnswerRecording {
    recording(id, "Help!", BEATLES, None, sources)
}

/// One submission is one person's word: R's recordings must carry at least two together,
/// summed over every recording of R and every qualifying result, a negative count read as none.
#[test]
fn a_lone_submission_never_reads_clear() {
    let r = help_r(&[HELP_2009, "be4ce82b-62e3-47b4-bb20-9eb74c77dd88"]);
    let read = |results| read_answer(&answer(results), &r, MATCH_SCORE);
    let lone = read(vec![result(0.97, vec![help(HELP_2009, 1)])]);
    assert_eq!(lone.reading, Reading::Ambiguous);
    let two = read(vec![result(
        0.97,
        vec![
            help(HELP_2009, 1),
            help("be4ce82b-62e3-47b4-bb20-9eb74c77dd88", 1),
        ],
    )]);
    assert_eq!(two.reading, Reading::Clear);
    let twice = read(vec![
        result(0.97, vec![help(HELP_2009, 1)]),
        result(0.91, vec![help(HELP_2009, 1)]),
    ]);
    assert_eq!(twice.reading, Reading::Clear);
    assert_eq!(twice.recordings.len(), 1);
    assert_eq!(twice.total_weight(), 2);
    let negative = read(vec![result(
        0.97,
        vec![
            help(HELP_2009, 2),
            help("be4ce82b-62e3-47b4-bb20-9eb74c77dd88", -5),
        ],
    )]);
    assert_eq!(negative.reading, Reading::Clear);
    assert_eq!(negative.recordings[1].weight, 0);
    assert_eq!(negative.total_weight(), 2);
}

/// A fingerprint AcoustID holds but has not linked may be the real match, so one scoring above
/// every link leaves nothing to read; one at or under the best link, or under the match score,
/// is passed over.
#[test]
fn an_unlinked_result_above_every_link_reads_unknown() {
    let r = help_r(&[HELP_2009]);
    let read = |results| read_answer(&answer(results), &r, MATCH_SCORE);
    let only = read(vec![result(0.90, Vec::new())]);
    assert_eq!(only.reading, Reading::Unknown);
    let above = read(vec![
        result(0.90, vec![help(HELP_2009, 50)]),
        result(0.98, Vec::new()),
    ]);
    assert_eq!(above.reading, Reading::Unknown);
    assert!(above.recordings.is_empty());
    assert_eq!(above.dominant(), None);
    for passed_over in [0.95, 0.88, 0.80] {
        let reading = read(vec![
            result(0.95, vec![help(HELP_2009, 50)]),
            result(passed_over, Vec::new()),
        ]);
        assert_eq!(
            reading.reading,
            Reading::Clear,
            "an unlinked result at {passed_over}"
        );
    }
}

/// Results under the match score are neither read nor weighed.
#[test]
fn results_under_the_match_score_are_never_weighed() {
    let r = help_r(&[HELP_2009]);
    assert_eq!(
        read_answer(&answer(Vec::new()), &r, MATCH_SCORE).reading,
        Reading::Empty
    );
    let under = read_answer(
        &answer(vec![result(0.84, vec![help(HELP_2009, 50)])]),
        &r,
        MATCH_SCORE,
    );
    assert_eq!(under.reading, Reading::Empty);
    assert!(under.recordings.is_empty());
    let mixed = answer(vec![
        result(
            0.97,
            vec![recording(HELP_2009, "Help!", BEATLES, Some("GBAYE0601465"), 50)],
        ),
        result(
            0.80,
            vec![recording(HELP_2015, "Help!", BEATLES, Some("GBUM71505900"), 500)],
        ),
    ]);
    let strict = read_answer(&mixed, &r, MATCH_SCORE);
    assert_eq!(strict.reading, Reading::Clear);
    assert_eq!(strict.total_weight(), 50);
    let loose = read_answer(&mixed, &r, 0.75);
    assert_eq!(loose.reading, Reading::Ambiguous);
    assert_eq!(loose.total_weight(), 550);
}

/// AcoustID clusters close mixes: one answer holding the 2009 and the 2023 Walrus mixes, each
/// with an ISRC of its own, reads clear for neither mix alone. The 2009 codes MusicBrainz also
/// files under the 2023 mix never make it R's.
#[test]
fn an_answer_holding_both_walrus_mixes_is_clear_for_neither() {
    let original = AnswerRecording {
        isrcs: vec!["GBAYE0601638".into(), "GBAYE6700024".into()],
        ..recording(WALRUS_2009, "I Am the Walrus", BEATLES, None, 300)
    };
    let remix = AnswerRecording {
        isrcs: vec![
            "GBAYE0601638".into(),
            "GBAYE6700024".into(),
            "GBUM72306578".into(),
        ],
        ..recording(WALRUS_2023, "I Am the Walrus (2023 mix)", BEATLES, None, 200)
    };
    let r_2009 = HeardRecording {
        ids: vec![WALRUS_2009.into()],
        isrcs: vec!["GBAYE0601638".into()],
        title: "I Am The Walrus (Remastered 2009)".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    };
    let r_2023 = HeardRecording {
        ids: vec![WALRUS_2023.into()],
        isrcs: vec!["GBUM72306578".into()],
        title: "I Am The Walrus (2023 Mix)".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    };
    let both = answer(vec![
        result(0.99, vec![original.clone()]),
        result(0.93, vec![remix.clone()]),
    ]);
    let for_2009 = read_answer(&both, &r_2009, MATCH_SCORE);
    assert_eq!(for_2009.reading, Reading::Ambiguous);
    assert!((for_2009.r_share - 0.6).abs() < 1e-12);
    let for_2023 = read_answer(&both, &r_2023, MATCH_SCORE);
    assert_eq!(for_2023.reading, Reading::Ambiguous);
    assert_eq!(for_2023.dominant().map(|d| d.id.as_str()), Some(WALRUS_2009));
    let remix_alone = answer(vec![result(0.99, vec![remix])]);
    assert_eq!(
        read_answer(&remix_alone, &r_2023, MATCH_SCORE).reading,
        Reading::Clear
    );
    let against_2009 = read_answer(&remix_alone, &r_2009, MATCH_SCORE);
    assert_eq!(against_2009.reading, Reading::Another);
    assert_eq!(against_2009.r_share, 0.0);
}

/// Every result counts, however many: the eleventh result's recording is weighed and can be the
/// dominant one.
#[test]
fn eleven_results_keep_every_recording() {
    let results: Vec<String> = (0..11)
        .map(|n| {
            format!(
                r#"{{"id": "fp-{n}", "score": 0.9{n:02}, "recordings": [{{"id": "rec-{n}", "title": "Help!",
                "artists": [{{"id": "{}", "name": "The Beatles"}}], "sources": {}}}]}}"#,
                BEATLES.0,
                n + 1
            )
        })
        .collect();
    let json = format!(r#"{{"status": "ok", "results": [{}]}}"#, results.join(", "));
    let doc: serde_json::Value = serde_json::from_str(&json).expect("test JSON parses");
    let whole = parse_answer(&doc).expect("the answer reads").expect("ok");
    let reading = read_answer(&whole, &help_r(&["rec-10"]), MATCH_SCORE);
    let ids: Vec<&str> = reading.recordings.iter().map(|r| r.id.as_str()).collect();
    let want: Vec<String> = (0..11).map(|n| format!("rec-{n}")).collect();
    assert_eq!(ids, want);
    assert_eq!(reading.dominant().map(|d| d.id.as_str()), Some("rec-10"));
    assert_eq!(reading.total_weight(), 66);
    assert!((reading.r_share - 11.0 / 66.0).abs() < 1e-12);
    assert_eq!(reading.reading, Reading::Ambiguous);
}

/// Clear needs R dominant and no other version of the song that AcoustID could be hearing
/// instead: none with an ISRC of its own at any share, and those without one under 1% together.
/// Another song's recordings say nothing about the version.
#[test]
fn clear_needs_no_other_version_of_the_song() {
    let r = help_r(&[HELP_2009]);
    let read = |others: Vec<AnswerRecording>| {
        let mut recordings = vec![recording(
            HELP_2009,
            "Help!",
            BEATLES,
            Some("GBAYE0601465"),
            9_990,
        )];
        recordings.extend(others);
        read_answer(&answer(vec![result(0.97, recordings)]), &r, MATCH_SCORE).reading
    };
    assert_eq!(read(Vec::new()), Reading::Clear);
    let tagged = recording(HELP_2015, "Help! (2015 Mix)", BEATLES, Some("GBUM71505900"), 1);
    assert_eq!(read(vec![tagged]), Reading::Ambiguous);
    assert_eq!(read(vec![help("u1", 50), help("u2", 49)]), Reading::Clear);
    assert_eq!(read(vec![help("u1", 60), help("u2", 60)]), Reading::Ambiguous);
    let other_song = recording("w1", "Wonderwall", OASIS, Some("GBBQY9500021"), 5);
    assert_eq!(read(vec![other_song]), Reading::Clear);
}

/// Another recording: the dominant one is another song, or the song in another version while R
/// holds under 1%; at 1% it is ambiguous. An R with no song name reads every answer as another.
#[test]
fn another_when_the_dominant_is_another_song_or_r_holds_a_trace() {
    let r = help_r(&[HELP_2009]);
    let read = |recordings| read_answer(&answer(vec![result(0.97, recordings)]), &r, MATCH_SCORE);
    let other = read(vec![
        recording("w1", "Wonderwall", OASIS, None, 900),
        help(HELP_2009, 20),
    ]);
    assert_eq!(other.reading, Reading::Another);
    assert!((other.r_share - 20.0 / 920.0).abs() < 1e-12);
    assert_eq!(
        read(vec![help(HELP_2015, 995), help(HELP_2009, 5)]).reading,
        Reading::Another
    );
    assert_eq!(
        read(vec![help(HELP_2015, 990), help(HELP_2009, 10)]).reading,
        Reading::Ambiguous
    );
    let nameless = HeardRecording {
        ids: vec![HELP_2009.into()],
        ..Default::default()
    };
    let unnamed = read_answer(
        &answer(vec![result(0.97, vec![help(HELP_2015, 50)])]),
        &nameless,
        MATCH_SCORE,
    );
    assert_eq!(unnamed.reading, Reading::Another);
}

/// Each recording is listed once, where the answer first lists it, with its submissions summed;
/// on a tie the first listed is the dominant one.
#[test]
fn recordings_keep_the_order_the_answer_first_lists_them() {
    let r = help_r(&[HELP_2009]);
    let heard = answer(vec![
        result(
            0.97,
            vec![
                recording(HELP_2015, "Help!", BEATLES, Some("GBUM71505900"), 40),
                help(HELP_2009, 30),
            ],
        ),
        result(0.92, vec![help(HELP_2009, 10)]),
    ]);
    let reading = read_answer(&heard, &r, MATCH_SCORE);
    let weighed: Vec<(&str, i64)> = reading
        .recordings
        .iter()
        .map(|recording| (recording.id.as_str(), recording.weight))
        .collect();
    assert_eq!(weighed, [(HELP_2015, 40), (HELP_2009, 40)]);
    assert_eq!(reading.dominant().map(|d| d.id.as_str()), Some(HELP_2015));
    assert_eq!(reading.reading, Reading::Ambiguous);
    assert!((reading.r_share - 0.5).abs() < 1e-12);
}

#[test]
fn another_title_by_another_artist_is_a_believed_different_song() {
    let r = help_r(&[HELP_2009]);
    let read = |recordings| read_answer(&answer(vec![result(0.97, recordings)]), &r, MATCH_SCORE);
    let wonderwall = |sources| recording("w1", "Wonderwall", OASIS, Some("GBBQY9500021"), sources);
    let stood_in = read(vec![wonderwall(900), help(HELP_2009, 20)]);
    assert!(believed_other_song(&stood_in, &r, &stood_in.recordings[0]));
    assert!(!believed_other_song(&stood_in, &r, &stood_in.recordings[1]));
    let under_half = read(vec![wonderwall(40), help(HELP_2009, 60)]);
    assert!(!believed_other_song(&under_half, &r, &under_half.recordings[0]));
    let lone = read(vec![wonderwall(1)]);
    assert!(!believed_other_song(&lone, &r, &lone.recordings[0]));
}

/// One artist spelled two ways, one MusicBrainz id: the same song, never another, whether R's
/// names come from its recordings in the answer or from the routing.
#[test]
fn the_same_title_under_a_second_spelling_of_one_artist_id_is_not_another_song() {
    let r = stronger_r(false);
    let reading = read_answer(
        &answer(vec![result(
            0.97,
            vec![
                recording("s-other", "Stronger", VALE_AGAIN, Some("USUM70741299"), 95),
                recording("s-heard", "Stronger", VALE, None, 5),
            ],
        )]),
        &r,
        MATCH_SCORE,
    );
    let top = &reading.recordings[0];
    assert!(same_song(&reading, &r, top));
    assert!(!believed_other_song(&reading, &r, top));
    let r = stronger_r(true);
    let alone = read_answer(
        &answer(vec![result(
            0.97,
            vec![recording("s-other", "Stronger", VALE_AGAIN, None, 95)],
        )]),
        &r,
        MATCH_SCORE,
    );
    assert!(same_song(&alone, &r, &alone.recordings[0]));
    assert!(!believed_other_song(&alone, &r, &alone.recordings[0]));
}

/// The same title by another artist id is another song; told apart by names alone (R carries no
/// artist id), the same title is never believed another song.
#[test]
fn the_same_title_by_another_artist_id_is_another_song() {
    let r = stronger_r(false);
    let reading = read_answer(
        &answer(vec![result(
            0.97,
            vec![
                recording("s-cover", "Stronger", COVER, None, 95),
                recording("s-heard", "Stronger", VALE, None, 5),
            ],
        )]),
        &r,
        MATCH_SCORE,
    );
    let top = &reading.recordings[0];
    assert!(!same_song(&reading, &r, top));
    assert!(believed_other_song(&reading, &r, top));
    assert_eq!(reading.reading, Reading::Another);
    let alone = read_answer(
        &answer(vec![result(
            0.97,
            vec![recording("s-cover", "Stronger", COVER, None, 95)],
        )]),
        &r,
        MATCH_SCORE,
    );
    assert!(!same_song(&alone, &r, &alone.recordings[0]));
    assert!(!believed_other_song(&alone, &r, &alone.recordings[0]));
}

/// The one recording an answer gives is its dominant recording at half the weight or more, when
/// it is R's song.
#[test]
fn one_recording_is_the_dominant_one_at_half_the_weight_or_more() {
    let r = help_r(&[HELP_2009]);
    let read = |recordings| read_answer(&answer(vec![result(0.97, recordings)]), &r, MATCH_SCORE);
    let half = read(vec![help(HELP_2015, 50), help("u1", 50)]);
    assert_eq!(one_recording(&half, &r).map(|d| d.id.as_str()), Some(HELP_2015));
    let under = read(vec![help(HELP_2015, 40), help("u1", 30), help("u2", 30)]);
    assert_eq!(one_recording(&under, &r), None);
    let other = read(vec![
        recording("w1", "Wonderwall", OASIS, None, 90),
        help(HELP_2009, 10),
    ]);
    assert_eq!(one_recording(&other, &r), None);
    assert_eq!(
        one_recording(&read_answer(&answer(Vec::new()), &r, MATCH_SCORE), &r),
        None
    );
}

/// R's song comes from R's recordings in the answer (the first one's title, every one's artists
/// and ids), else from the routing, else there is none.
#[test]
fn r_song_comes_from_r_recordings_in_the_answer_else_the_routing() {
    let r = HeardRecording {
        ids: vec![HELP_2009.into(), HELP_2015.into()],
        title: "Help! (Remastered 2009)".into(),
        artist: "Beatles".into(),
        artist_ids: vec!["routing-id".into()],
        ..Default::default()
    };
    let lennon = AnswerRecording {
        artists: vec![AnswerArtist {
            id: Some("lennon-id".into()),
            name: "John Lennon".into(),
        }],
        ..help(HELP_2009, 2)
    };
    let wonderwall = || recording("w1", "Wonderwall", OASIS, None, 5);
    let reading = read_answer(
        &answer(vec![result(
            0.97,
            vec![
                wonderwall(),
                recording(HELP_2015, "Help! (2015 Mix)", BEATLES, None, 3),
                lennon,
            ],
        )]),
        &r,
        MATCH_SCORE,
    );
    let song = r_song(&reading, &r).expect("R's recordings name the song");
    assert_eq!(song.title, "Help! (2015 Mix)");
    assert_eq!(song.artists, ["The Beatles", "John Lennon"]);
    assert_eq!(song.artist_ids, [BEATLES.0, "lennon-id"]);
    let elsewhere = read_answer(&answer(vec![result(0.97, vec![wonderwall()])]), &r, MATCH_SCORE);
    assert_eq!(
        r_song(&elsewhere, &r),
        Some(SongName {
            title: "Help! (Remastered 2009)".into(),
            artists: vec!["Beatles".into()],
            artist_ids: vec!["routing-id".into()],
        })
    );
    let nameless = HeardRecording {
        ids: vec![HELP_2009.into()],
        ..Default::default()
    };
    assert_eq!(r_song(&elsewhere, &nameless), None);
}

#[test]
fn a_core_title_drops_bracketed_and_dash_tails() {
    assert_eq!(core_title("Help! (Remastered 2009)"), "help");
    assert_eq!(core_title("I Am The Walrus - Remastered 2009"), "iamthewalrus");
    assert_eq!(core_title("Mrs. Robinson [Live]"), "mrsrobinson");
    assert_eq!(
        core_title("Love Story (Taylor's Version)"),
        core_title("Love Story")
    );
    assert_eq!(core_title("Déjà Vu"), core_title("Deja Vu"));
    assert_eq!(core_title("Me & You"), core_title("Me and You"));
    assert_eq!(core_title("Re-Make"), "remake");
    assert_eq!(core_title("(Untitled)"), "untitled");
    assert_ne!(core_title("Yesterday"), core_title("Yesterday Once More"));
}

#[test]
fn lengths_fit_within_seven_seconds_or_four_percent() {
    assert!(length_fits(107.0, 100.0));
    assert!(length_fits(93.0, 100.0));
    assert!(!length_fits(107.5, 100.0));
    assert!(length_fits(312.0, 300.0));
    assert!(length_fits(288.0, 300.0));
    assert!(!length_fits(312.5, 300.0));
}

#[test]
fn two_names_are_one_song_by_core_title_and_a_shared_artist() {
    let name = |title: &str, artists: &[&str], ids: &[&str]| SongName {
        title: title.into(),
        artists: artists.iter().map(|a| a.to_string()).collect(),
        artist_ids: ids.iter().map(|id| id.to_string()).collect(),
    };
    let vale = name("Stronger", &["Ezra Vale"], &["v1"]);
    assert!(is_same_song(
        &vale,
        &name("Stronger (Remastered)", &["E.V."], &["v1"])
    ));
    assert!(!is_same_song(&vale, &name("Stronger", &["Ezra Vale"], &["c1"])));
    let help = name("Help!", &["The Beatles"], &[]);
    assert!(is_same_song(
        &help,
        &name("Help! (2015 Mix)", &["Beatles"], &["b1"])
    ));
    assert!(!is_same_song(&help, &name("Help!", &["The Cover Band"], &[])));
    assert!(!is_same_song(&help, &name("Yesterday", &["The Beatles"], &[])));
    let untitled = name("", &["The Beatles"], &[]);
    assert!(!is_same_song(&untitled, &untitled));
}
