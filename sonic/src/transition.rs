//! A song's transition profile as Octo keeps it and the apps read it: the
//! features of its first 45 s and last 60 s, its level there every 100 ms,
//! and the whole song's level and tempo. Made from the same decoded sound
//! as the Sounds alike reading.
//!
//! The JSON, every time in milliseconds of song time and every level in
//! dBFS; a time or tempo the song does not have is left out:
//!
//! ```text
//! { "version": 1, "revision": 2, "durationMs": 241330, "bodyDb": -11.52,
//!   "tempo": TEMPO, "head": SECTION, "tail": SECTION }
//! SECTION: { "startMs", "hopMs", "levels", "bodyDb", "gateDb", "soundStartMs",
//!   "soundEndMs", "outroStartMs", "introEndMs", "boundariesMs": [..], "tempo": TEMPO }
//! TEMPO: { "bpm", "trusted", "beatMs", "firstBeatMs", "downbeatMs", "confidence",
//!   "consistency", "steady" }
//! ```
//!
//! `levels` is base64 of one byte per hop: the level in half-dB steps above
//! -100 dBFS. Both ends' features are measured against the whole song's
//! level. `revision` is the pass of the analysis within its version: a
//! player reads every revision the same way, and Octo reads a song again
//! when its profile's revision is lower than the one this octo-sonic writes.
//!
//! A published tempo (a catalog's BPM or the file's BPM tag) is right less
//! often than the analysis, so it never sets a tempo: it only picks between
//! the octaves the analysis found (half, the same, or double), and only when
//! one of them is within [`PUBLISHED_WITHIN`] of it.

use serde_json::{json, Map, Value};

use crate::automix::envelope::{EnvelopeBuilder, SectionEnvelope, SILENT_DB};
use crate::automix::features::{
    analyze_head, analyze_tail, SectionAnalysis, SectionFeatures, Tempo, HEAD_BODY_PERCENTILE,
};
use crate::automix::profile::{
    level_code, SectionProfile, TransitionProfile, PROFILE_HEAD_MS, PROFILE_REVISION, PROFILE_TAIL_MS,
    PROFILE_VERSION,
};

/// How close a published tempo must be to one of the analysis's octaves, as a share of it,
/// to pick that octave.
pub const PUBLISHED_WITHIN: f64 = 0.02;

/// The profile version this octo-sonic writes.
pub fn version() -> u32 {
    PROFILE_VERSION
}

/// The pass of the analysis, within its version, that this octo-sonic's profiles come from.
pub fn revision() -> u32 {
    PROFILE_REVISION
}

/// The profile of decoded mono sound at `sample_rate`. `published` is the song's published
/// tempo, when it has one.
pub fn analyze(mono: &[f32], sample_rate: u32, published: Option<f64>) -> TransitionProfile {
    let mut builder = EnvelopeBuilder::new(sample_rate, 1, 0);
    builder.push(mono);
    profile_of(&builder.into_envelope(), published)
}

/// The octave of a detected tempo that a published one picks: 0.5, 1 or 2, whichever puts the
/// detected tempo within [`PUBLISHED_WITHIN`] of the published one (the closest, should two).
/// `None` when the published tempo is missing (0) or none of them is that close.
pub fn octave_for(detected: f64, published: f64) -> Option<f64> {
    if !(published.is_finite() && published > 0.0 && detected.is_finite() && detected > 0.0) {
        return None;
    }
    [0.5, 1.0, 2.0]
        .into_iter()
        .map(|factor| (factor, (detected * factor / published - 1.0).abs()))
        .filter(|&(_, off)| off <= PUBLISHED_WITHIN)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(factor, _)| factor)
}

/// A section as analyzed on its own, or analyzed again in the octave a published tempo picks.
/// The analysis is handed the octave it found itself, never the published number, so the
/// published tempo only ever moves the beat by a whole octave.
fn in_published_octave(
    plain: SectionAnalysis,
    published: Option<f64>,
    again: impl FnOnce(f64) -> SectionAnalysis,
) -> SectionAnalysis {
    let Some(detected) = plain.features.tempo.map(|t| t.bpm) else {
        return plain;
    };
    match published.and_then(|p| octave_for(detected, p)) {
        Some(factor) if factor != 1.0 => again(detected * factor),
        _ => plain,
    }
}

/// [`TransitionProfile::of_envelope`] with a published tempo choosing the octave of the whole
/// song, of its start and of its end, each on its own.
pub fn profile_of(song: &SectionEnvelope, published: Option<f64>) -> TransitionProfile {
    let duration_ms = song.end_ms();
    let whole = in_published_octave(
        SectionAnalysis::of(song.clone(), None, None, HEAD_BODY_PERCENTILE),
        published,
        |bpm| SectionAnalysis::of(song.clone(), Some(bpm), None, HEAD_BODY_PERCENTILE),
    );
    let body_db = Some(whole.features.body_db).filter(|&b| b > SILENT_DB as f64);
    let head_part = song.slice(0.0, PROFILE_HEAD_MS.min(duration_ms) as f64);
    let head = in_published_octave(analyze_head(head_part.clone(), None, body_db), published, |bpm| {
        analyze_head(head_part, Some(bpm), body_db)
    });
    let tail_from = (duration_ms - PROFILE_TAIL_MS).max(0);
    let tail_part = song.slice(tail_from as f64, duration_ms as f64);
    let tail = in_published_octave(analyze_tail(tail_part.clone(), None, body_db), published, |bpm| {
        analyze_tail(tail_part, Some(bpm), body_db)
    });
    TransitionProfile {
        version: PROFILE_VERSION,
        revision: PROFILE_REVISION,
        duration_ms,
        body_db,
        tempo: whole.features.tempo,
        head: SectionProfile::of(&head),
        tail: SectionProfile::of(&tail),
    }
}

/// The profile as JSON.
pub fn to_json(profile: &TransitionProfile) -> Value {
    let mut out = Map::new();
    out.insert("version".into(), json!(profile.version));
    out.insert("revision".into(), json!(profile.revision));
    out.insert("durationMs".into(), json!(profile.duration_ms));
    if let Some(body) = profile.body_db {
        out.insert("bodyDb".into(), json!(decibels(body)));
    }
    if let Some(tempo) = &profile.tempo {
        out.insert("tempo".into(), tempo_json(tempo));
    }
    out.insert("head".into(), section_json(&profile.head));
    out.insert("tail".into(), section_json(&profile.tail));
    Value::Object(out)
}

fn section_json(section: &SectionProfile) -> Value {
    let codes: Vec<u8> = section.levels.iter().map(|&db| level_code(db)).collect();
    let mut out = Map::new();
    out.insert("startMs".into(), json!(section.start_ms));
    out.insert("hopMs".into(), json!(section.hop_ms));
    out.insert("levels".into(), json!(base64(&codes)));
    features_into(&mut out, &section.features);
    Value::Object(out)
}

fn features_into(out: &mut Map<String, Value>, f: &SectionFeatures) {
    out.insert("bodyDb".into(), json!(decibels(f.body_db)));
    out.insert("gateDb".into(), json!(decibels(f.gate_db)));
    let times = [
        ("soundStartMs", f.sound_start_ms),
        ("soundEndMs", f.sound_end_ms),
        ("outroStartMs", f.outro_start_ms),
        ("introEndMs", f.intro_end_ms),
    ];
    for (name, time) in times {
        if let Some(ms) = time {
            out.insert(name.into(), json!(ms));
        }
    }
    out.insert("boundariesMs".into(), json!(f.boundaries_ms));
    if let Some(tempo) = &f.tempo {
        out.insert("tempo".into(), tempo_json(tempo));
    }
}

// The beat grid in full: the planner builds bars from it over many beats.
fn tempo_json(t: &Tempo) -> Value {
    json!({
        "bpm": t.bpm,
        "trusted": t.confident(),
        "beatMs": t.beat_ms,
        "firstBeatMs": t.first_beat_ms,
        "downbeatMs": t.downbeat_ms,
        "confidence": t.confidence,
        "consistency": t.consistency,
        "steady": t.steady,
    })
}

// A level to a thousandth of a dB.
fn decibels(db: f64) -> f64 {
    (db * 1000.0).round() / 1000.0
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automix::profile::level_of_code;
    use crate::automix::synthetic::{SongLayer, SyntheticSong};

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/transition-profile.json");

    fn layer(kind: &str, from_ms: f64, to_ms: f64, hz: f64, db: f64) -> SongLayer {
        SongLayer {
            kind: kind.into(),
            from_ms,
            to_ms,
            hz,
            db,
            end_db: db,
            bpm: 120.0,
            first_ms: 4_000.0,
            every: 1,
            decay_ms: 10.0,
        }
    }

    /// 75 s at 120 BPM: a quiet 4 s intro, a kick on every bar and a click
    /// on every beat, then a 15 s fade from 60 s.
    fn fixture_song() -> SyntheticSong {
        let mut pad = layer("tone", 0.0, 4_000.0, 300.0, -40.0);
        pad.end_db = -40.0;
        let mut body = layer("tone", 4_000.0, 60_000.0, 300.0, -22.0);
        body.end_db = -22.0;
        let mut fade = layer("tone", 60_000.0, 75_000.0, 300.0, -22.0);
        fade.end_db = -70.0;
        let kick = SongLayer {
            every: 4,
            decay_ms: 80.0,
            ..layer("pulse", 4_000.0, 60_000.0, 60.0, -6.0)
        };
        let click = layer("pulse", 4_000.0, 60_000.0, 2_000.0, -16.0);
        SyntheticSong {
            rate: 22_050,
            channels: 1,
            length_ms: 75_000,
            layers: vec![pad, body, fade, kick, click],
        }
    }

    fn fixture_profile() -> Value {
        let song = fixture_song();
        to_json(&analyze(&song.render_mono(0.0, song.length_ms as f64), song.rate, None))
    }

    fn decode64(text: &str) -> Vec<u8> {
        let value = |c: u8| match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            _ => 63,
        };
        let mut out = Vec::new();
        for chunk in text.as_bytes().chunks(4) {
            let n = chunk
                .iter()
                .take_while(|&&c| c != b'=')
                .enumerate()
                .fold(0u32, |n, (i, &c)| n | u32::from(value(c)) << (18 - 6 * i));
            let kept = chunk.iter().filter(|&&c| c != b'=').count() - 1;
            out.extend((0..kept).map(|i| (n >> (16 - 8 * i)) as u8));
        }
        out
    }

    // Equal but for the last bits of floating point, which differ between
    // machines: numbers within a hundredth, levels within one step.
    fn alike(path: &str, want: &Value, got: &Value, off: &mut Vec<String>) {
        match (want, got) {
            (Value::Object(w), Value::Object(g)) => {
                let (mut wk, mut gk): (Vec<_>, Vec<_>) = (w.keys().collect(), g.keys().collect());
                wk.sort();
                gk.sort();
                if wk != gk {
                    off.push(format!("{path}: keys {gk:?}, expected {wk:?}"));
                    return;
                }
                for (key, w) in w {
                    alike(&format!("{path}.{key}"), w, &g[key], off);
                }
            }
            (Value::Array(w), Value::Array(g)) if w.len() == g.len() => {
                for (i, (w, g)) in w.iter().zip(g).enumerate() {
                    alike(&format!("{path}[{i}]"), w, g, off);
                }
            }
            (Value::Number(w), Value::Number(g)) => {
                let (w, g) = (w.as_f64().unwrap_or(f64::NAN), g.as_f64().unwrap_or(f64::NAN));
                if (w - g).abs() > 0.01 {
                    off.push(format!("{path}: {g}, expected {w}"));
                }
            }
            (Value::String(w), Value::String(g)) if path.ends_with(".levels") => {
                let (w, g) = (decode64(w), decode64(g));
                if w.len() != g.len() || w.iter().zip(&g).any(|(a, b)| a.abs_diff(*b) > 1) {
                    off.push(format!("{path}: levels differ"));
                }
            }
            (w, g) if w == g => {}
            (w, g) => off.push(format!("{path}: {g}, expected {w}")),
        }
    }

    /// octo-sonic's own profile of the fixture song
    /// (`testdata/transition-profile.json`), the form the apps read their
    /// profile parsing against. `OCTO_WRITE_FIXTURE=1 cargo test` writes it
    /// again after a change to the analysis or the form.
    #[test]
    fn the_profile_is_the_one_the_apps_read() {
        let got = fixture_profile();
        if std::env::var("OCTO_WRITE_FIXTURE").is_ok_and(|v| v == "1") {
            let text = serde_json::to_string_pretty(&got).expect("JSON") + "\n";
            std::fs::write(FIXTURE, text).expect("the fixture is written");
        }
        let want: Value =
            serde_json::from_str(&std::fs::read_to_string(FIXTURE).expect("the fixture")).expect("JSON");
        let mut off = Vec::new();
        alike("profile", &want, &got, &mut off);
        assert!(off.is_empty(), "{}", off.join("\n"));
    }

    #[test]
    fn the_fixture_song_has_what_a_blend_needs() {
        let p = fixture_profile();
        // The second pass of version 1: players read it as they read the first.
        assert_eq!((&p["version"], &p["revision"]), (&json!(1), &json!(2)));
        assert_eq!((version(), revision()), (1, 2));
        assert_eq!(p["durationMs"], json!(75_000));
        assert_eq!(
            (p["head"]["startMs"].as_i64(), p["head"]["hopMs"].as_i64()),
            (Some(0), Some(100))
        );
        assert_eq!(p["tail"]["startMs"], json!(15_000));
        let intro = p["head"]["introEndMs"].as_i64().expect("an intro end");
        assert!((3_400..=4_300).contains(&intro), "{intro}");
        let outro = p["tail"]["outroStartMs"].as_i64().expect("an outro");
        assert!((58_000..=66_000).contains(&outro), "{outro}");
        let tempo = &p["tempo"];
        assert!((tempo["bpm"].as_f64().unwrap() - 120.0).abs() < 0.5, "{tempo}");
        assert_eq!(decode64(p["tail"]["levels"].as_str().unwrap()).len(), 600);
        // The first 45 s.
        assert_eq!(decode64(p["head"]["levels"].as_str().unwrap()).len(), 450);
        assert!(p["tail"]["boundariesMs"].is_array());
        // Small enough to keep for every song and ask for at every song change.
        let size = serde_json::to_string(&p).unwrap().len();
        assert!(size < 2_500, "{size} bytes");
    }

    /// 180 s: two soft tones for the first `quiet_ms`, then a loud body.
    fn quiet_start_song(quiet_ms: f64) -> SyntheticSong {
        SyntheticSong {
            rate: 22_050,
            channels: 1,
            length_ms: 180_000,
            layers: vec![
                layer("tone", 0.0, quiet_ms, 300.0, -38.0),
                layer("tone", 0.0, quiet_ms, 100.0, -41.0),
                layer("tone", quiet_ms, 180_000.0, 300.0, -14.0),
            ],
        }
    }

    #[test]
    fn the_start_is_measured_against_the_whole_song() {
        let intro_end = |quiet_ms| {
            let song = quiet_start_song(quiet_ms);
            analyze(&song.render_mono(0.0, song.length_ms as f64), song.rate, None).head.features.intro_end_ms
        };
        // On its own the start is mostly the quiet part, which would be its body.
        let long = intro_end(35_000.0).expect("an intro end");
        assert!((34_000..=35_500).contains(&long), "{long}");
        // Quiet past the 45 s the profile covers: the intro lasts at least that long.
        assert_eq!(intro_end(60_000.0), Some(45_000));
    }

    #[test]
    fn a_silent_song_leaves_out_what_it_does_not_have() {
        let p = to_json(&analyze(&vec![0.0; 22_050 * 40], 22_050, Some(120.0)));
        assert!(p.get("bodyDb").is_none() && p.get("tempo").is_none());
        assert!(p["tail"].get("soundStartMs").is_none() && p["tail"].get("tempo").is_none());
        assert!(decode64(p["tail"]["levels"].as_str().unwrap())
            .iter()
            .all(|&c| c == 0));
    }

    #[test]
    fn base64_is_the_standard_one() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0, 255, 171, 7]), "AP+rBw==");
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode64(&base64(&all)), all);
        assert_eq!(level_of_code(level_code(-14.26)), -14.5);
    }

    fn fixture_envelope() -> SectionEnvelope {
        let song = fixture_song();
        let mut builder = EnvelopeBuilder::new(song.rate, 1, 0);
        builder.push(&song.render_mono(0.0, song.length_ms as f64));
        builder.into_envelope()
    }

    fn bpm_of(tempo: Option<&Tempo>) -> f64 {
        tempo.expect("a tempo").bpm
    }

    #[test]
    fn a_published_tempo_picks_one_of_the_octaves_found() {
        assert_eq!(octave_for(120.0, 60.0), Some(0.5));
        assert_eq!(octave_for(120.0, 61.0), Some(0.5));
        assert_eq!(octave_for(120.0, 118.0), Some(1.0));
        assert_eq!(octave_for(120.0, 243.0), Some(2.0));
        assert_eq!(octave_for(87.0, 174.0), Some(2.0));
    }

    #[test]
    fn a_published_tempo_near_no_octave_or_missing_picks_nothing() {
        // 70 is nearer the half octave (60) than 120, yet 17% from it.
        assert_eq!(octave_for(120.0, 70.0), None);
        assert_eq!(octave_for(120.0, 125.0), None);
        assert_eq!(octave_for(120.0, 90.0), None);
        assert_eq!(octave_for(120.0, 0.0), None);
        assert_eq!(octave_for(120.0, -60.0), None);
        assert_eq!(octave_for(120.0, f64::NAN), None);
        assert_eq!(octave_for(0.0, 120.0), None);
    }

    #[test]
    fn without_a_published_tempo_the_profile_is_the_engines_own() {
        let song = fixture_envelope();
        assert_eq!(profile_of(&song, None), TransitionProfile::of_envelope(&song));
    }

    #[test]
    fn a_published_tempo_moves_every_part_to_the_octave_it_picks() {
        let song = fixture_envelope();
        let plain = profile_of(&song, None);
        assert!((bpm_of(plain.tempo.as_ref()) - 120.0).abs() < 0.5);
        for (published, want) in [(60.0, 60.0), (59.2, 60.0), (241.0, 240.0)] {
            let picked = profile_of(&song, Some(published));
            for tempo in [
                picked.tempo.as_ref(),
                picked.head.features.tempo.as_ref(),
                picked.tail.features.tempo.as_ref(),
            ] {
                let bpm = bpm_of(tempo);
                assert!((bpm - want).abs() < 0.5, "{published} gave {bpm}, expected {want}");
            }
            // Only the beat moves; the levels and where the sound starts and ends do not.
            assert_eq!(picked.head.levels, plain.head.levels);
            assert_eq!(picked.tail.features.outro_start_ms, plain.tail.features.outro_start_ms);
        }
    }

    #[test]
    fn a_published_tempo_that_matches_no_octave_or_is_missing_changes_nothing() {
        let song = fixture_envelope();
        let plain = profile_of(&song, None);
        for published in [70.0, 100.0, 125.0, 180.0, 0.0, -1.0] {
            assert_eq!(profile_of(&song, Some(published)), plain, "{published}");
        }
        // The octave the analysis found is the analysis as it was.
        assert_eq!(profile_of(&song, Some(119.0)), plain);
    }
}
