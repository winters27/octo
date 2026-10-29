//! Tests for the clip check: 13 saved catalog clip prints against 12 saved heard prints read as
//! `tests/fixtures/heart_album/clip/expected.tsv` says, the rules for a usable clip, the
//! readings' lines and the channel name test.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heart_album")
}

/// The items of a saved `fpcalc -raw -json` print.
fn print_items(path: &Path) -> Vec<u32> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let doc: serde_json::Value = serde_json::from_str(&text).expect("a print is JSON");
    doc["fingerprint"]
        .as_array()
        .expect("a print has items")
        .iter()
        .map(|item| (item.as_i64().expect("an item is a whole number") & 0xFFFF_FFFF) as u32)
        .collect()
}

#[test]
fn edge_and_length_are_whole_items() {
    assert_eq!(EDGE_ITEMS, (1.0 / ITEM_SECONDS).round() as usize);
    assert_eq!(MIN_ITEMS, (8.0 / ITEM_SECONDS).round() as usize);
}

#[test]
fn every_saved_clip_reads_every_heard_print_as_expected() {
    let root = fixtures();
    let text =
        std::fs::read_to_string(root.join("clip/expected.tsv")).expect("the expected readings are readable");
    let mut clips: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    let mut heard: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    let mut pairs = 0;
    for line in text.lines() {
        // Rows: clip, heard print, same song or not, bit error, place in seconds, reading.
        let &[clip_name, heard_name, _, bit_error, place, reading] =
            line.split('\t').collect::<Vec<_>>().as_slice()
        else {
            continue;
        };
        if clip_name == "clip" {
            continue;
        }
        let clip = clips
            .entry(clip_name)
            .or_insert_with(|| print_items(&root.join(format!("clip/{clip_name}-clip.json"))));
        let heard_print = heard
            .entry(heard_name)
            .or_insert_with(|| print_items(&root.join(format!("gate/prints/{heard_name}-240.json"))));
        let got = check(clip, heard_print);
        let want = match reading {
            "same" => ClipReading::ThisSong,
            "other" => ClipReading::OtherSongOrTake,
            "unclear" => ClipReading::Unclear,
            other => panic!("no reading is named {other:?}"),
        };
        let pair = format!("{clip_name} against {heard_name}");
        assert_eq!(got.reading, want, "{pair}");
        if bit_error == "none" {
            assert_eq!(got.score, None, "{pair}");
        } else {
            let score = got.score.unwrap_or_else(|| panic!("{pair}: a score"));
            let want_error: f64 = bit_error.parse().expect("a bit error");
            assert!(
                (score.bit_error - want_error).abs() <= 0.0005 + 1e-9,
                "{pair}: {}",
                score.bit_error
            );
            // The place column is the kept clip's first item, in seconds, plus the dropped edge.
            let got_place = (score.heard_item + EDGE_ITEMS) as f64 * ITEM_SECONDS;
            let want_place: f64 = place.parse().expect("a place");
            assert!(
                (got_place - want_place).abs() <= 0.05 + 1e-9,
                "{pair}: at {got_place:.2} s"
            );
        }
        pairs += 1;
    }
    assert_eq!((clips.len(), heard.len(), pairs), (13, 12, 156));
}

/// `n` items that change at every step, every bit of them as likely set as not (xorshift).
fn varied(seed: u32, n: usize) -> Vec<u32> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        })
        .collect()
}

#[test]
fn a_clip_cut_from_the_heard_print_is_this_song_at_its_place() {
    let heard = varied(1, 600);
    let got = check(&heard[100..321], &heard);
    assert_eq!(got.reading, ClipReading::ThisSong);
    let score = got.score.expect("a usable clip");
    assert_eq!(score.bit_error, 0.0);
    assert_eq!(score.heard_item, 100 + EDGE_ITEMS);
    assert!((score.clip_start_seconds() - 100.0 * ITEM_SECONDS).abs() < 1e-9);
}

#[test]
fn the_earliest_of_two_equal_places_wins() {
    let once = varied(2, 300);
    let twice = [once.clone(), once.clone()].concat();
    let score = slide(&once[..221], &twice).expect("a usable clip");
    assert_eq!(score.heard_item, EDGE_ITEMS);
}

#[test]
fn an_unrelated_print_is_another_song() {
    let got = check(&varied(3, 221), &varied(4, 2000));
    assert_eq!(got.reading, ClipReading::OtherSongOrTake);
    assert!(got.score.expect("a usable clip").bit_error > 0.45);
}

#[test]
fn a_short_steady_or_too_long_clip_is_unclear() {
    let heard = varied(5, 600);
    // 65 items kept once a second at each end is dropped is enough; 64 is not.
    assert!(slide(&heard[..2 * EDGE_ITEMS + MIN_ITEMS], &heard).is_some());
    assert_eq!(
        check(&heard[..2 * EDGE_ITEMS + MIN_ITEMS - 1], &heard).reading,
        ClipReading::Unclear
    );
    assert_eq!(slide(&heard[..2 * EDGE_ITEMS + MIN_ITEMS - 1], &heard), None);
    assert_eq!(slide(&[], &heard), None);
    // Under 70% of the kept items changing: one number held over most of the clip.
    let mut steady = heard[..221].to_vec();
    steady[EDGE_ITEMS..150].fill(7);
    assert_eq!(slide(&steady, &heard), None);
    // A clip longer than the heard print.
    assert_eq!(check(&heard[..221], &heard[..200]).score, None);
}

#[test]
fn the_lines_between_readings() {
    let at = |bit_error| ClipScore {
        bit_error,
        heard_item: 0,
    };
    assert_eq!(ClipReading::of(Some(&at(MATCH_AT_MOST))), ClipReading::ThisSong);
    assert_eq!(ClipReading::of(Some(&at(0.2501))), ClipReading::Unclear);
    assert_eq!(ClipReading::of(Some(&at(0.3799))), ClipReading::Unclear);
    assert_eq!(
        ClipReading::of(Some(&at(MISMATCH_FROM))),
        ClipReading::OtherSongOrTake
    );
    assert_eq!(ClipReading::of(None), ClipReading::Unclear);
}

#[test]
fn a_channel_names_the_artist_only_by_the_artist_s_own_name() {
    for (channel, artist, own) in [
        ("The Beatles - Topic", "The Beatles", true),
        ("The Beatles", "The Beatles", true),
        ("Beatles", "The Beatles", true),
        (" The Weeknd - Topic ", "The Weeknd", true),
        ("Bob Marley & The Wailers - Topic", "Bob Marley", false),
        ("Sing King", "The Beatles", false),
        ("Parlophone Records", "The Beatles", false),
    ] {
        assert_eq!(channel_names_artist(channel, artist), own, "{channel} / {artist}");
    }
}
