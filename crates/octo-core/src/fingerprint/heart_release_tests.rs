//! Tests for the heart's release pick: every recorded question rebuilt byte for byte, the readers
//! of the answers, and the recorded answers themselves.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;
use crate::fingerprint::music_brainz_client::artist_lookup_url;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heart_album")
}

fn row_folder(row: &str) -> PathBuf {
    fixtures().join("rows").join(row.replace('#', "_"))
}

/// One line of `rows.tsv`: a real catalog row as a person hearts it.
struct FixtureRow {
    name: String,
    artist: String,
    album: String,
    isrc: String,
    upc: String,
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
                album: field[4].to_string(),
                isrc: field[6].to_string(),
                upc: field[9].to_string(),
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
