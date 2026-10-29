//! Tests for the heart's release pick: every recorded question rebuilt byte for byte, the readers
//! of the answers, the recorded answers themselves, the five frozen runs of real rows replayed
//! through every question, and each rule on constructed rows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde_json::{Value, json};

use super::*;
use crate::fingerprint::music_brainz_client::{artist_country, artist_lookup_url};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heart_album")
}

fn row_folder(row: &str) -> PathBuf {
    fixtures().join("rows").join(row.replace('#', "_"))
}

/// One line of `rows.tsv`: a real catalog row as a person hearts it, and the release group each
/// of the five runs picks first ("none" for no pick), in the order of [`Run::ALL`].
struct FixtureRow {
    name: String,
    artist: String,
    title: String,
    album: String,
    album_id: String,
    isrc: String,
    seconds: u32,
    upc: String,
    total_tracks: Option<u32>,
    first_picks: [String; 5],
}

fn fixture_rows() -> Vec<FixtureRow> {
    let text = std::fs::read_to_string(fixtures().join("rows.tsv")).expect("rows.tsv reads");
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let field: Vec<&str> = line.split('\t').collect();
            FixtureRow {
                name: field[0].to_string(),
                artist: field[2].to_string(),
                title: field[3].to_string(),
                album: field[4].to_string(),
                album_id: field[5].to_string(),
                isrc: field[6].to_string(),
                seconds: field[7].parse().unwrap_or(0),
                upc: field[9].to_string(),
                total_tracks: field[10].parse().ok(),
                first_picks: [field[12], field[13], field[14], field[15], field[17]].map(str::to_string),
            }
        })
        .collect()
}

/// The question a MusicBrainz answer file's name stands for, built the way the heart builds it.
fn musicbrainz_question(name: &str, row: Option<&FixtureRow>) -> Option<String> {
    if name == "isrc-lookup" {
        return isrc_lookup_url(&row?.isrc);
    }
    if name == "barcode" {
        return barcode_search_url(&row?.upc);
    }
    if let Some(id) = name.strip_prefix("barcode-release-") {
        return release_lookup_url(id);
    }
    if let Some(id) = name.strip_prefix("artist-") {
        return artist_lookup_url(id);
    }
    // carriers-<id>-p<page> for pages of 100, carriers-<id>-o<offset>-l<limit> otherwise.
    let rest = name.strip_prefix("carriers-")?;
    let (id, paging) = (rest.get(..36)?, rest.get(36..)?);
    if let Some(page) = paging.strip_prefix("-p") {
        return carriers_url(id, page.parse::<u32>().ok()? * CARRIERS_PAGE, CARRIERS_PAGE);
    }
    let (offset, limit) = paging.strip_prefix("-o")?.split_once("-l")?;
    carriers_url(id, offset.parse().ok()?, limit.parse().ok()?)
}

fn deezer_question(name: &str, rows: &[FixtureRow]) -> Option<String> {
    if let Some(id) = name.strip_prefix("deezer-album-tracks/") {
        return Some(deezer_album_tracks_url(id));
    }
    if let Some(id) = name.strip_prefix("deezer-album/") {
        return Some(deezer_album_url(id));
    }
    let row = rows
        .iter()
        .find(|row| Some(row.name.as_str()) == name.strip_prefix("albumsearch "))?;
    deezer_album_search_url(&row.artist, &row.album)
}

#[test]
fn every_recorded_question_is_built_byte_for_byte() {
    let rows = fixture_rows();
    assert_eq!(rows.len(), 92);
    let mut checked = 0;
    for row in &rows {
        let answers = RecordedAnswers::open(&row_folder(&row.name)).expect("the row's urls.tsv reads");
        for line in answers.questions() {
            assert_eq!(
                musicbrainz_question(&line.name, Some(row)).as_deref(),
                Some(line.question.as_str()),
                "{} {}",
                row.name,
                line.name
            );
            checked += 1;
        }
    }
    for folder in ["c4-timeout-retry", "c4-probe"] {
        let answers = RecordedAnswers::open(&fixtures().join(folder)).expect("the folder's urls.tsv reads");
        for line in answers.questions() {
            assert_eq!(
                musicbrainz_question(&line.name, None).as_deref(),
                Some(line.question.as_str()),
                "{folder} {}",
                line.name
            );
            checked += 1;
        }
    }
    let deezer = RecordedAnswers::open(&fixtures().join("deezer")).expect("the Deezer urls.tsv reads");
    for line in deezer.questions() {
        assert_eq!(
            deezer_question(&line.name, &rows).as_deref(),
            Some(line.question.as_str()),
            "deezer {}",
            line.name
        );
        checked += 1;
    }
    assert_eq!(checked, 766);
}

#[test]
fn a_barcode_is_asked_in_each_form_musicbrainz_files_it_under() {
    assert_eq!(
        barcode_search_url("602547673503").as_deref(),
        Some("release/?query=barcode%3A%280602547673503%20OR%20602547673503%29&fmt=json&limit=25")
    );
    assert_eq!(
        barcode_search_url("0602547673503").as_deref(),
        Some("release/?query=barcode%3A%280602547673503%20OR%20602547673503%29&fmt=json&limit=25")
    );
    assert_eq!(
        barcode_search_url("5099902987620").as_deref(),
        Some("release/?query=barcode%3A%285099902987620%29&fmt=json&limit=25")
    );
    assert_eq!(barcode_search_url(" "), None);
    assert_eq!(
        isrc_lookup_url("gb-um7-15-05917").as_deref(),
        Some("isrc/GBUM71505917?inc=artist-credits&fmt=json")
    );
    assert_eq!(isrc_lookup_url("not a code"), None);
    assert_eq!(release_lookup_url("not-an-mbid"), None);
    assert_eq!(deezer_album_search_url("The Beatles", " "), None);
}

#[test]
fn the_code_lookup_reads_without_dj_mix_segments() {
    let answer = json!({"recordings": [
        {"id": "a", "title": "Song", "length": 200000, "disambiguation": "  Original  Stereo Mix ",
         "isrcs": ["gb-aaa-09-00001", "bad"], "first-release-date": "2009-09-09",
         "artist-credit": [{"name": "The Band", "artist": {"id": "band", "name": "Band, The"}}]},
        {"id": "b", "title": "Song", "length": 0,
         "disambiguation": "part of \u{201C}Mix\u{201D} DJ\u{2010}mix"},
        {"id": "", "title": "Song"},
        {"id": "c", "title": "Song"}
    ]});
    let recordings = read_code_recordings(&answer);
    assert_eq!(
        recordings,
        [
            CodeRecording {
                id: "a".into(),
                title: "Song".into(),
                length_ms: Some(200_000),
                note: "original stereo mix".into(),
                isrcs: vec!["GBAAA0900001".into()],
                first_release_date: "2009-09-09".into(),
                credits: vec![CreditedArtist {
                    name: "The Band".into(),
                    artist_id: "band".into(),
                    artist_name: "Band, The".into(),
                }],
            },
            CodeRecording {
                id: "c".into(),
                title: "Song".into(),
                ..CodeRecording::default()
            },
        ]
    );
}

#[test]
fn a_release_track_takes_its_recordings_length_else_its_own() {
    let answer = json!({"id": "release", "title": "Album", "date": "2009",
    "release-group": {"id": "group", "title": "Album", "primary-type": "Album", "secondary-types": ["Live"]},
    "media": [{"position": 2, "tracks": [
        {"position": 1, "title": "One", "length": 100000, "recording": {"id": "r1", "length": 101000}},
        {"position": 2, "title": "Two", "length": 120000, "recording": {"id": "r2", "length": null}}
    ]}]});
    let release = read_barcode_release(&answer).expect("a release");
    assert_eq!(release.group.secondary_types, ["Live"]);
    assert_eq!(release.media[0].position, Some(2));
    let lengths: Vec<Option<u32>> = release.media[0]
        .tracks
        .iter()
        .map(|track| track.length_ms)
        .collect();
    assert_eq!(lengths, [Some(101_000), Some(120_000)]);
    assert_eq!(read_barcode_release(&json!({"error": "Not Found"})), None);
}

#[test]
fn carrier_pages_go_on_to_the_release_count() {
    let answer = json!({"release-count": 3, "releases": [
        {"id": "r1", "title": "Album", "date": "1990", "country": "GB",
         "release-group": {"id": "g1", "title": "Album", "primary-type": "Album"},
         "artist-credit": [{"name": "The Band", "artist": {"id": "band", "name": "The Band"}}]},
        {"id": "r2", "title": "No group"}
    ]});
    let page = read_carrier_page(&answer);
    assert_eq!(page.release_count, 3);
    assert_eq!(page.listed, 2);
    assert_eq!(page.releases.len(), 1);
    assert_eq!(page.releases[0].lead_artist_id, "band");
    assert_eq!(next_carrier_offset(0, &page), Some(2));
    assert_eq!(next_carrier_offset(1, &page), None);
    assert_eq!(next_carrier_offset(0, &CarrierPage::default()), None);
}

#[test]
fn catalog_tracks_read_the_explicit_value_as_the_routing_stores_it() {
    let answer = json!({"data": [
        {"id": 1, "title": "A", "isrc": "GBAAA0900001", "duration": 200, "track_position": 1,
         "disk_number": 1, "explicit_content_lyrics": 4},
        {"id": 2, "title": "B", "duration": 200, "track_position": 2, "disk_number": 1,
         "explicit_content_lyrics": 2, "explicit_lyrics": true},
        {"id": 3, "title": "C", "duration": 200, "track_position": 3, "disk_number": 1,
         "explicit_content_lyrics": 6},
        {"id": 4, "title": "D", "duration": 200, "track_position": 4, "disk_number": 1,
         "explicit_content_lyrics": 3}
    ]});
    let tracks = read_catalog_tracks(&answer);
    let explicit: Vec<Option<i32>> = tracks.iter().map(|track| track.explicit).collect();
    assert_eq!(explicit, [Some(1), Some(1), None, Some(3)]);
    assert_eq!(tracks[0].id, "1");
    assert_eq!(tracks[0].isrc, "GBAAA0900001");
    assert_eq!(tracks[1].isrc, "");
    let album = read_catalog_album(
        &json!({"id": 12047956, "upc": "602547673503", "nb_tracks": 27, "record_type": "album"}),
    )
    .expect("an album");
    assert_eq!(
        (album.id.as_str(), album.upc.as_str(), album.track_count),
        ("12047956", "602547673503", Some(27))
    );
    let hits =
        read_album_search(&json!({"data": [{"id": 7, "title": "1", "artist": {"name": "The Beatles"}}]}));
    assert_eq!(hits[0].artist, "The Beatles");
}

#[test]
fn recorded_answers_serve_a_404_and_nothing_unlisted() {
    let answers =
        RecordedAnswers::open(&row_folder("loggins-footloose#2")).expect("the row's urls.tsv reads");
    let code = isrc_lookup_url("GBPS81500657").expect("a code");
    assert_eq!(answers.get(&code), Some(RecordedAnswer::NotFound));
    assert_eq!(answers.get("isrc/GBPS81500658?inc=artist-credits&fmt=json"), None);
    let unasked: Vec<&str> = answers.unasked().iter().map(|line| line.name.as_str()).collect();
    assert_eq!(unasked, ["barcode"]);
    let barcode = barcode_search_url("889176539091").expect("a barcode");
    assert!(matches!(
        answers.answer(&barcode),
        RecordedAnswer::Found(Value::Object(_))
    ));
    assert!(answers.unasked().is_empty());
}

#[test]
#[should_panic(expected = "recorded no answer")]
fn an_unlisted_question_fails_the_test() {
    let answers =
        RecordedAnswers::open(&row_folder("loggins-footloose#2")).expect("the row's urls.tsv reads");
    answers.answer("artist/00000000-0000-0000-0000-000000000000?fmt=json");
}

// ---- the five runs, replayed ----------------------------------------------------------------

/// The five ways a heart can know its song: what the routing carries in each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// ISRC, catalog album and position.
    Full,
    /// Catalog album and position.
    NoIsrc,
    /// ISRC only.
    NoAlbum,
    /// ISRC, album title and position: the catalog album found from its title.
    TitleOnly,
    /// Album title and position.
    TitleOnlyNoIsrc,
}

impl Run {
    const ALL: [Run; 5] = [
        Run::Full,
        Run::NoIsrc,
        Run::NoAlbum,
        Run::TitleOnly,
        Run::TitleOnlyNoIsrc,
    ];

    fn name(self) -> &'static str {
        match self {
            Run::Full => "full",
            Run::NoIsrc => "no-isrc",
            Run::NoAlbum => "no-album",
            Run::TitleOnly => "title-only",
            Run::TitleOnlyNoIsrc => "title-only-no-isrc",
        }
    }

    fn column(self) -> usize {
        Run::ALL.iter().position(|run| *run == self).unwrap_or(0)
    }

    fn with_isrc(self) -> bool {
        !matches!(self, Run::NoIsrc | Run::TitleOnlyNoIsrc)
    }

    fn by_title(self) -> bool {
        matches!(self, Run::TitleOnly | Run::TitleOnlyNoIsrc)
    }
}

/// One row heard one way, replayed from the recorded answers.
struct Replayed {
    row: String,
    run: Run,
    /// The catalog album the album title found, in the runs that look for it.
    found_album: Option<String>,
    identity: HeardIdentity,
    walk: Vec<String>,
}

/// Every row in every run, and the recorded questions none of them asked.
struct Replay {
    runs: Vec<Replayed>,
    unasked: Vec<String>,
}

static REPLAY: LazyLock<Replay> = LazyLock::new(|| {
    let rows = fixture_rows();
    let positions_text = std::fs::read_to_string(fixtures().join("deezer/positions-v3.json"))
        .expect("positions-v3.json reads");
    let positions: Value = serde_json::from_str(&positions_text).expect("positions-v3.json parses");
    let deezer = RecordedAnswers::open(&fixtures().join("deezer")).expect("the Deezer urls.tsv reads");
    let mut runs = Vec::new();
    let mut unasked = Vec::new();
    for row in &rows {
        let answers = RecordedAnswers::open(&row_folder(&row.name)).expect("the row's urls.tsv reads");
        for run in Run::ALL {
            runs.push(replay(row, &positions[row.name.as_str()], run, &answers, &deezer));
        }
        unasked.extend(
            answers
                .unasked()
                .iter()
                .map(|line| format!("{} {}", row.name, line.question)),
        );
    }
    unasked.extend(
        deezer
            .unasked()
            .iter()
            .map(|line| format!("deezer {}", line.question)),
    );
    Replay { runs, unasked }
});

fn found(answer: RecordedAnswer) -> Option<Value> {
    match answer {
        RecordedAnswer::Found(doc) => Some(doc),
        RecordedAnswer::NotFound => None,
    }
}

/// One row heard one way, through every question the heart asks, answered from the recordings.
fn replay(
    row: &FixtureRow,
    position: &Value,
    run: Run,
    answers: &RecordedAnswers,
    deezer: &RecordedAnswers,
) -> Replayed {
    let listing = found(deezer.answer(&deezer_album_tracks_url(&row.album_id)))
        .map(|doc| read_catalog_tracks(&doc))
        .unwrap_or_default();
    let track_id = position["track_id"].to_string();
    let heard = listing.iter().find(|track| track.id == track_id);
    let place = |field: &str| {
        position[field]
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
    };
    let song = HeartSong {
        artist: row.artist.clone(),
        title: row.title.clone(),
        album: if run == Run::NoAlbum {
            String::new()
        } else {
            row.album.clone()
        },
        disc: place("disc"),
        position: place("position"),
        total_tracks: row.total_tracks,
        isrc: if run.with_isrc() {
            row.isrc.clone()
        } else {
            String::new()
        },
        seconds: row.seconds,
        explicit: heard.and_then(|track| track.explicit),
    };

    let catalog_album = if run.by_title() {
        let hits = deezer_album_search_url(&song.artist, &song.album)
            .and_then(|question| found(deezer.answer(&question)))
            .map(|doc| read_album_search(&doc))
            .unwrap_or_default();
        catalog_album_candidates(&song, &hits)
            .into_iter()
            .find(|hit| {
                let album =
                    found(deezer.answer(&deezer_album_url(&hit.id))).and_then(|doc| read_catalog_album(&doc));
                let tracks = found(deezer.answer(&deezer_album_tracks_url(&hit.id)))
                    .map(|doc| read_catalog_tracks(&doc))
                    .unwrap_or_default();
                catalog_album_fits(&song, album.as_ref(), &tracks)
            })
            .map(|hit| hit.id.clone())
    } else if run == Run::NoAlbum {
        None
    } else {
        Some(row.album_id.clone())
    };
    // The recorded barcode answers belong to the row's own catalog album.
    let upc = catalog_album
        .as_ref()
        .filter(|id| **id == row.album_id)
        .and_then(|id| found(deezer.answer(&deezer_album_url(id))))
        .and_then(|doc| read_catalog_album(&doc))
        .map(|album| album.upc);

    let code_recordings = isrc_lookup_url(&song.isrc)
        .and_then(|question| found(answers.answer(&question)))
        .map(|doc| read_code_recordings(&doc))
        .unwrap_or_default();
    let mut releases = Vec::new();
    if let Some(upc) = &upc
        && let Some(question) = barcode_search_url(upc)
        && let Some(doc) = found(answers.answer(&question))
    {
        for id in releases_to_look_up(&read_barcode_hits(&doc), upc) {
            let looked_up = release_lookup_url(&id)
                .and_then(|question| found(answers.answer(&question)))
                .and_then(|doc| read_barcode_release(&doc));
            releases.extend(looked_up.filter(|release| release.id == id));
        }
    }
    let identity = identify(
        &song,
        &code_recordings,
        listened_release(&song, &releases, &code_recordings),
    );

    let home = artist_lookup_url(&identity.artist_id)
        .and_then(|question| found(answers.answer(&question)))
        .map(|doc| artist_country(&doc))
        .unwrap_or_default();
    let carriers: Vec<Carriers> = identity
        .recordings
        .iter()
        .map(|id| {
            let mut releases = Vec::new();
            let mut offset = 0;
            loop {
                let question = carriers_url(id, offset, CARRIERS_PAGE).expect("a MusicBrainz recording id");
                let page = found(answers.answer(&question))
                    .map(|doc| read_carrier_page(&doc))
                    .unwrap_or_default();
                let next = next_carrier_offset(offset, &page);
                releases.extend(page.releases);
                match next {
                    Some(next) => offset = next,
                    None => break,
                }
            }
            Carriers {
                recording_id: id.clone(),
                releases,
            }
        })
        .collect();
    let picks = walk(&song, &identity, &home, &carriers);
    Replayed {
        row: row.name.clone(),
        run,
        found_album: if run.by_title() { catalog_album } else { None },
        identity,
        walk: picks,
    }
}

fn replayed(row: &str, run: Run) -> &'static Replayed {
    REPLAY
        .runs
        .iter()
        .find(|replayed| replayed.row == row && replayed.run == run)
        .expect("the row was replayed")
}

#[test]
fn the_five_runs_pick_first_what_was_frozen() {
    let rows: HashMap<String, FixtureRow> = fixture_rows()
        .into_iter()
        .map(|row| (row.name.clone(), row))
        .collect();
    let misses: Vec<String> = REPLAY
        .runs
        .iter()
        .filter_map(|replayed| {
            let wanted = &rows[&replayed.row].first_picks[replayed.run.column()];
            let first = replayed.walk.first().map_or("none", String::as_str);
            (first != wanted).then(|| {
                format!(
                    "{} {}: {first}, frozen {wanted}",
                    replayed.row,
                    replayed.run.name()
                )
            })
        })
        .collect();
    assert_eq!(REPLAY.runs.len(), 460);
    assert!(
        misses.is_empty(),
        "{} of 460 first picks differ: {misses:#?}",
        misses.len()
    );
}

#[test]
fn the_five_runs_walk_what_was_frozen() {
    let text = std::fs::read_to_string(fixtures().join("walks.tsv")).expect("walks.tsv reads");
    let frozen: HashMap<(String, String), String> = text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut field = line.split('\t');
            Some((
                (field.next()?.to_string(), field.next()?.to_string()),
                field.next()?.to_string(),
            ))
        })
        .collect();
    assert_eq!(frozen.len(), 460);
    let misses: Vec<String> = REPLAY
        .runs
        .iter()
        .filter_map(|replayed| {
            let walked = if replayed.walk.is_empty() {
                "-".to_string()
            } else {
                replayed.walk.join(",")
            };
            let wanted = &frozen[&(replayed.row.clone(), replayed.run.name().to_string())];
            (walked != *wanted).then(|| {
                format!(
                    "{} {}: {walked}, frozen {wanted}",
                    replayed.row,
                    replayed.run.name()
                )
            })
        })
        .collect();
    assert!(
        misses.is_empty(),
        "{} of 460 walks differ: {misses:#?}",
        misses.len()
    );
}

#[test]
fn the_album_title_finds_the_catalog_album_and_never_a_wrong_one() {
    let rows: HashMap<String, FixtureRow> = fixture_rows()
        .into_iter()
        .map(|row| (row.name.clone(), row))
        .collect();
    for (run, wanted) in [(Run::TitleOnly, 92), (Run::TitleOnlyNoIsrc, 82)] {
        let mut found = 0;
        for replayed in REPLAY.runs.iter().filter(|replayed| replayed.run == run) {
            if let Some(album) = &replayed.found_album {
                assert_eq!(
                    *album,
                    rows[&replayed.row].album_id,
                    "{} {}",
                    replayed.row,
                    run.name()
                );
                found += 1;
            }
        }
        assert_eq!(found, wanted, "{}", run.name());
    }
}

#[test]
fn the_runs_ask_every_recorded_question_and_nothing_else() {
    assert!(REPLAY.unasked.is_empty(), "never asked: {:#?}", REPLAY.unasked);
}

#[test]
fn codes_that_name_two_mixes_give_the_heard_one_or_nothing() {
    let walrus = ["3eb7efbb-8717-4034-a698-109c70798524"];
    let help = ["01b6e4de-ff48-4ec0-862e-cd623c924d7b"];
    let original = ["659b4269-fe81-40e4-86e9-12879c09c9e6"];
    // The 2023 Walrus mix was first released after its code's year.
    assert_eq!(
        replayed("beatles-i-am-the-walrus#0", Run::Full)
            .identity
            .recordings,
        walrus
    );
    assert_eq!(
        replayed("beatles-i-am-the-walrus#0", Run::NoAlbum)
            .identity
            .recordings,
        walrus
    );
    // Help!'s 2015 code also names the 1983 recording: the heard track, else the code's year.
    assert_eq!(replayed("beatles-help#0", Run::Full).identity.recordings, help);
    assert_eq!(replayed("beatles-help#0", Run::NoAlbum).identity.recordings, help);
    // Can't Buy Me Love's code names two mixes from before its year: only the heard track tells.
    assert_eq!(
        replayed("beatles-cant-buy-me-love#0", Run::Full)
            .identity
            .recordings,
        original
    );
    assert!(
        replayed("beatles-cant-buy-me-love#0", Run::NoAlbum)
            .identity
            .recordings
            .is_empty()
    );
}

// ---- the rules, on constructed rows ---------------------------------------------------------

const BAND: &str = "11111111-1111-1111-1111-111111111111";
const OTHER: &str = "22222222-2222-2222-2222-222222222222";

fn credit(name: &str, id: &str) -> CreditedArtist {
    CreditedArtist {
        name: name.to_string(),
        artist_id: id.to_string(),
        artist_name: name.to_string(),
    }
}

/// "Song" by The Band, 200 s, track 2 of 10 on disc 1, under a 2009 code.
fn song() -> HeartSong {
    HeartSong {
        artist: "The Band".to_string(),
        title: "Song".to_string(),
        album: "Album".to_string(),
        disc: Some(1),
        position: Some(2),
        total_tracks: Some(10),
        isrc: "GBAAA0900001".to_string(),
        seconds: 200,
        explicit: None,
    }
}

fn coded(id: &str, note: &str, first_released: &str, by: CreditedArtist) -> CodeRecording {
    CodeRecording {
        id: id.to_string(),
        title: "Song".to_string(),
        length_ms: Some(200_400),
        note: note.to_string(),
        isrcs: vec!["GBAAA0900001".to_string()],
        first_release_date: first_released.to_string(),
        credits: vec![by],
    }
}

/// A ten-track release by `credits` whose track 2 is `recording_id`, titled `title`.
fn listened(recording_id: &str, title: &str, length_ms: u32, credits: &[CreditedArtist]) -> BarcodeRelease {
    let tracks = (1..=10)
        .map(|position| ReleaseTrack {
            position: Some(position),
            title: if position == 2 {
                title.to_string()
            } else {
                format!("Other {position}")
            },
            length_ms: Some(if position == 2 { length_ms } else { 180_000 }),
            recording_id: if position == 2 {
                recording_id.to_string()
            } else {
                format!("other-{position}")
            },
            credits: credits.to_vec(),
        })
        .collect();
    BarcodeRelease {
        id: "release-listened".to_string(),
        title: "Album".to_string(),
        date: "2009-09-09".to_string(),
        credits: credits.to_vec(),
        group: GroupFacts {
            id: "group-listened".to_string(),
            title: "Album".to_string(),
            primary_type: "Album".to_string(),
            secondary_types: Vec::new(),
        },
        media: vec![ReleaseMedium {
            position: Some(1),
            tracks,
        }],
    }
}

#[test]
fn a_duplicate_on_the_listened_release_joins_the_heard_recordings_first() {
    let band = credit("The Band", BAND);
    let codes = [coded("rec-coded", "", "2009", band.clone())];
    let release = listened("rec-duplicate", "Song (2009 Remaster)", 201_000, &[band]);
    let identity = identify(&song(), &codes, Some(&release));
    assert_eq!(identity.recordings, ["rec-duplicate", "rec-coded"]);
    assert_eq!(identity.heard_track.as_deref(), Some("rec-duplicate"));
    assert_eq!(identity.listened_release.as_deref(), Some("release-listened"));
    assert_eq!(identity.listened_group.as_deref(), Some("group-listened"));
    assert_eq!(identity.artist_id, BAND);
    assert_eq!(identity.artist_ids, [BAND]);
}

#[test]
fn another_acts_recording_stays_out() {
    let codes = [
        coded("rec-band", "", "2009", credit("The Band", BAND)),
        coded("rec-cover", "", "2009", credit("Somebody Else", OTHER)),
        coded("rec-same-artist", "", "2009", credit("Band, The", BAND)),
        coded(
            "rec-compilation",
            "",
            "2009",
            credit("Various Artists", VARIOUS_ARTISTS_ID),
        ),
    ];
    // The listened release credits the band and Various Artists; its track 2 is another song.
    let release = listened(
        "rec-elsewhere",
        "Elsewhere",
        999_000,
        &[
            credit("The Band", BAND),
            credit("Various Artists", VARIOUS_ARTISTS_ID),
        ],
    );
    let identity = identify(&song(), &codes, Some(&release));
    assert_eq!(identity.heard_track, None);
    assert_eq!(identity.recordings, ["rec-band", "rec-same-artist"]);
    assert_eq!(identity.artist_id, BAND);
}

#[test]
fn an_original_and_a_remix_under_one_code_give_no_identity_unless_heard() {
    let band = credit("The Band", BAND);
    let codes = [
        coded("rec-original", "", "2009", band.clone()),
        coded("rec-remix", "remix", "2009-06-01", band.clone()),
    ];
    let identity = identify(&song(), &codes, None);
    assert!(identity.recordings.is_empty());
    assert_eq!(identity.listened_group, None);
    assert!(walk(&song(), &identity, "GB", &[]).is_empty());

    let release = listened("rec-remix", "Song (Remix)", 200_000, &[band]);
    assert_eq!(
        identify(&song(), &codes, Some(&release)).recordings,
        ["rec-remix"]
    );
}

#[test]
fn recordings_first_released_after_the_codes_year_drop() {
    let band = credit("The Band", BAND);
    let codes = [
        coded("rec-2009", "", "2009-09-09", band.clone()),
        coded("rec-2015", "", "2015", band.clone()),
    ];
    assert_eq!(identify(&song(), &codes, None).recordings, ["rec-2009"]);
    // When every one is later, every one stays.
    let later = [
        coded("rec-2015", "", "2015", band.clone()),
        coded("rec-2016", "", "2016", band),
    ];
    assert_eq!(
        identify(&song(), &later, None).recordings,
        ["rec-2015", "rec-2016"]
    );
}

#[test]
fn a_recording_that_does_not_fit_the_length_stays_out() {
    let band = credit("The Band", BAND);
    let mut long = coded("rec-long", "", "2009", band.clone());
    long.length_ms = Some(240_000);
    let codes = [coded("rec-fits", "", "2009", band), long];
    assert_eq!(identify(&song(), &codes, None).recordings, ["rec-fits"]);
    // With no length to compare, every credited recording stays.
    let unknown = HeartSong { seconds: 0, ..song() };
    assert_eq!(
        identify(&unknown, &codes, None).recordings,
        ["rec-fits", "rec-long"]
    );
}

#[test]
fn the_cap_keeps_the_heard_track() {
    let band = credit("The Band", BAND);
    let codes: Vec<CodeRecording> = (1..=7)
        .map(|n| coded(&format!("rec-{n}"), "", "2009", band.clone()))
        .collect();
    let release = listened("rec-heard", "Song", 200_000, &[band]);
    let identity = identify(&song(), &codes, Some(&release));
    assert_eq!(
        identity.recordings,
        ["rec-heard", "rec-1", "rec-2", "rec-3", "rec-4", "rec-5"]
    );
    assert_eq!(identity.recordings_uncapped.len(), 8);
}

/// One release of a group, issued in `country` on `date`, led by `lead`.
fn carried(
    group: &str,
    kind: (&str, &[&str]),
    title: &str,
    date: &str,
    country: &str,
    lead: &str,
) -> CarrierRelease {
    CarrierRelease {
        id: format!("{group}-{date}-{country}"),
        title: title.to_string(),
        date: date.to_string(),
        country: country.to_string(),
        group: GroupFacts {
            id: group.to_string(),
            title: title.to_string(),
            primary_type: kind.0.to_string(),
            secondary_types: kind.1.iter().map(|secondary| secondary.to_string()).collect(),
        },
        lead_artist_id: lead.to_string(),
    }
}

fn carrying(releases: Vec<CarrierRelease>) -> Vec<Carriers> {
    vec![Carriers {
        recording_id: "rec-a".to_string(),
        releases,
    }]
}

fn heard(listened_group: Option<&str>) -> HeardIdentity {
    HeardIdentity {
        recordings: vec!["rec-a".to_string()],
        recordings_uncapped: vec!["rec-a".to_string()],
        listened_group: listened_group.map(str::to_string),
        artist_id: BAND.to_string(),
        ..HeardIdentity::default()
    }
}

const ALBUM: (&str, &[&str]) = ("Album", &[]);
const SINGLE: (&str, &[&str]) = ("Single", &[]);

#[test]
fn groups_go_in_the_order_of_their_kinds() {
    let releases = vec![
        carried("k8-others", ALBUM, "Hits", "1990", "GB", OTHER),
        carried(
            "k7-other-own",
            ("Album", &["Remix"]),
            "Remixed",
            "1990",
            "GB",
            BAND,
        ),
        carried(
            "k6-compilation",
            ("Album", &["Compilation"]),
            "Best Of",
            "1990",
            "GB",
            BAND,
        ),
        carried("k5-other-single", SINGLE, "Another One", "1990", "GB", BAND),
        carried("k4-single-elsewhere", SINGLE, "Song", "1990", "US", BAND),
        carried("k3-album-elsewhere", ALBUM, "Album", "1990", "US", BAND),
        carried("k2-home-single", ("EP", &[]), "Song EP", "1990", "GB", BAND),
        carried("k1-home-album", ("Album", &["Live"]), "Live", "1990", "GB", BAND),
    ];
    let mut wanted: Vec<&str> = releases.iter().map(|release| release.group.id.as_str()).collect();
    wanted.reverse();
    let unnamed = HeartSong {
        album: String::new(),
        ..song()
    };
    assert_eq!(
        order(&unnamed, &heard(None), "GB", &carrying(releases.clone())),
        wanted
    );
}

#[test]
fn home_is_the_artists_country_worldwide_or_europe_in_the_first_year() {
    let releases = vec![
        carried("g-europe", ALBUM, "Europe", "1990", "XE", BAND),
        carried("g-later-at-home", ALBUM, "Later", "1990", "US", BAND),
        carried("g-later-at-home", ALBUM, "Later", "1995", "GB", BAND),
        carried("g-worldwide", ALBUM, "World", "1991", "XW", BAND),
    ];
    let unnamed = HeartSong {
        album: String::new(),
        ..song()
    };
    let carriers = carrying(releases);
    // A British artist: Europe and worldwide are home; a British release years later is not.
    assert_eq!(
        order(&unnamed, &heard(None), "GB", &carriers),
        ["g-europe", "g-worldwide", "g-later-at-home"]
    );
    // An American artist: the American and worldwide releases are home, Europe is not.
    assert_eq!(
        order(&unnamed, &heard(None), "US", &carriers),
        ["g-later-at-home", "g-worldwide", "g-europe"]
    );
}

#[test]
fn within_a_kind_the_codes_year_then_the_earliest_date_then_the_id_decide() {
    let releases = vec![
        carried("d-only-1965", ALBUM, "Old", "1965-08-06", "US", BAND),
        carried("a-2010", ALBUM, "A", "2010-01-01", "US", BAND),
        carried("e-2010", ALBUM, "E", "2010-01-01", "US", BAND),
        carried("b-2009", ALBUM, "B", "2009", "US", BAND),
        carried("c-2009-09", ALBUM, "C", "2009-09-09", "US", BAND),
        carried("f-1965-and-2009", ALBUM, "F", "1965-08-06", "US", BAND),
        carried("f-1965-and-2009", ALBUM, "F", "2009-09-09", "US", BAND),
    ];
    let unnamed = HeartSong {
        album: String::new(),
        ..song()
    };
    assert_eq!(
        order(&unnamed, &heard(None), "GB", &carrying(releases)),
        [
            "f-1965-and-2009",
            "c-2009-09",
            "b-2009",
            "a-2010",
            "e-2010",
            "d-only-1965"
        ]
    );
}

#[test]
fn the_walk_takes_other_artists_only_when_the_artist_leads_nothing_else() {
    let own_and_others = vec![
        carried("own-1", ALBUM, "One", "1991", "US", BAND),
        carried("own-2", ALBUM, "Two", "1992", "US", BAND),
        carried("own-3", ALBUM, "Three", "1993", "US", BAND),
        carried("own-4", ALBUM, "Four", "1994", "US", BAND),
        carried("other-1", ALBUM, "Hits", "1990", "US", OTHER),
    ];
    assert_eq!(
        walk(&song(), &heard(Some("listened")), "GB", &carrying(own_and_others)),
        ["listened", "own-1", "own-2", "own-3"]
    );
    let others_only = vec![
        carried("listened", ALBUM, "Album", "1990", "GB", BAND),
        carried("other-1", ALBUM, "Hits", "1991", "US", OTHER),
        carried("other-2", ALBUM, "Hits 2", "1992", "US", OTHER),
        carried("other-3", ALBUM, "Hits 3", "1993", "US", OTHER),
        carried("other-4", ALBUM, "Hits 4", "1994", "US", OTHER),
    ];
    let carriers = carrying(others_only);
    assert_eq!(
        walk(&song(), &heard(Some("listened")), "GB", &carriers),
        ["listened", "other-1", "other-2", "other-3"]
    );
    // With no listened release, the carrier group titled like the album is the listened one.
    assert_eq!(
        walk(&song(), &heard(None), "GB", &carriers),
        ["listened", "other-1", "other-2", "other-3"]
    );
    let unnamed = HeartSong {
        album: String::new(),
        ..song()
    };
    assert_eq!(walk(&unnamed, &heard(None), "GB", &carriers), ["listened"]);
}

#[test]
fn without_a_listened_release_the_earliest_group_titled_like_the_album_is_first() {
    let releases = vec![
        carried("deluxe", ALBUM, "Album (Deluxe Edition)", "1990", "US", BAND),
        carried("original", ALBUM, "Album", "1990-05-01", "US", BAND),
        carried("other-title", ALBUM, "Different", "1980", "US", BAND),
    ];
    assert_eq!(
        order(&song(), &heard(None), "GB", &carrying(releases)),
        ["original", "other-title", "deluxe"]
    );
}
