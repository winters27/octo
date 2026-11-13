//! The shared automix vectors (`testdata/automix-vectors.json`, the file
//! the apps test their own copies with): every section is rebuilt from its
//! layers and analyzed, and its envelope and features must match within
//! the file's tolerances, as must the filters and the tempo trust rule. A
//! failure lists every value that is off, not only the first. The planner
//! is not copied here, so the vectors' plans are left to the apps; what a
//! profile takes from the sections (45 s heads measured against the whole
//! song's body, and the revision it says) is checked on its own.

use std::collections::HashMap;

use serde_json::Value;

use super::dsp::Biquad;
use super::features::{analyze_head, analyze_tail, SectionAnalysis, Tempo};
use super::profile::{TransitionProfile, PROFILE_HEAD_MS, PROFILE_REVISION, PROFILE_VERSION};
use super::synthetic::{SongLayer, SyntheticSong};

const VECTORS: &str = include_str!("../../testdata/automix-vectors.json");

// How many samples of each biquad answer the file holds.
const FILTER_SAMPLES: usize = 512;

fn num(o: &Value, key: &str) -> f64 {
    o[key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key} is not a number in {o}"))
}

fn num_or_none(o: &Value, key: &str) -> Option<f64> {
    o.get(key).and_then(Value::as_f64)
}

fn int(o: &Value, key: &str) -> i64 {
    o[key]
        .as_i64()
        .unwrap_or_else(|| panic!("{key} is not a whole number in {o}"))
}

fn text<'a>(o: &'a Value, key: &str) -> &'a str {
    o[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} is not text in {o}"))
}

fn flag(o: &Value, key: &str) -> bool {
    o[key]
        .as_bool()
        .unwrap_or_else(|| panic!("{key} is not true or false in {o}"))
}

fn list<'a>(o: &'a Value, key: &str) -> &'a Vec<Value> {
    o[key].as_array().unwrap_or_else(|| panic!("{key} is not a list"))
}

fn song_of(o: &Value) -> SyntheticSong {
    SyntheticSong {
        rate: int(o, "rate") as u32,
        channels: int(o, "channels") as u32,
        length_ms: int(o, "lengthMs"),
        layers: list(o, "layers")
            .iter()
            .map(|l| SongLayer {
                kind: text(l, "kind").to_string(),
                from_ms: num(l, "fromMs"),
                to_ms: num(l, "toMs"),
                hz: num(l, "hz"),
                db: num(l, "db"),
                end_db: num(l, "endDb"),
                bpm: num_or_none(l, "bpm").unwrap_or(0.0),
                first_ms: num_or_none(l, "firstMs").unwrap_or(0.0),
                every: l.get("every").and_then(Value::as_i64).unwrap_or(1),
                decay_ms: num_or_none(l, "decayMs").unwrap_or(0.0),
            })
            .collect(),
    }
}

#[derive(Default)]
struct Checker {
    failures: Vec<String>,
}

impl Checker {
    fn near(&mut self, what: &str, expected: Option<f64>, actual: Option<f64>, within: f64) {
        match (expected, actual) {
            (Some(e), Some(a)) if (e - a).abs() > within => self
                .failures
                .push(format!("{what}: expected {e}, got {a} (within {within})")),
            (Some(_), Some(_)) | (None, None) => {}
            (e, a) => self.failures.push(format!("{what}: expected {e:?}, got {a:?}")),
        }
    }
}

/// A section analyzed as the vectors say: tagged with `tagBpm`, and measured against
/// `bodyLevelDb`, the whole song's body, when they are set.
fn analyze(song: &SyntheticSong, section: &Value) -> SectionAnalysis {
    let envelope = song.envelope(num(section, "fromMs"), num(section, "toMs"), 4_093);
    let tag = num_or_none(section, "tagBpm");
    let body = num_or_none(section, "bodyLevelDb");
    if text(section, "part") == "tail" {
        analyze_tail(envelope, tag, body)
    } else {
        analyze_head(envelope, tag, body)
    }
}

fn check_section(c: &mut Checker, tol: &Value, section: &Value, analysis: &SectionAnalysis) {
    let t = |name: &str| num(tol, name);
    let mut what = match num_or_none(section, "tagBpm") {
        None => format!("{} {}", text(section, "song"), text(section, "part")),
        Some(tag) => format!("{} {} tagged {tag}", text(section, "song"), text(section, "part")),
    };
    if let Some(body) = num_or_none(section, "bodyLevelDb") {
        what.push_str(&format!(" against a body of {body} dB"));
    }
    let envelope = &analysis.envelope;
    if envelope.size() as i64 != int(section, "hops") || envelope.start_ms != int(section, "startMs") {
        c.failures.push(format!(
            "{what}: {} hops from {}",
            envelope.size(),
            envelope.start_ms
        ));
    }
    for probe in list(section, "probes") {
        let i = int(probe, "index") as usize;
        let at = |values: &Vec<f32>| values.get(i).map(|&v| f64::from(v));
        c.near(
            &format!("{what} hop {i} db"),
            Some(num(probe, "db")),
            at(&envelope.db),
            t("db"),
        );
        c.near(
            &format!("{what} hop {i} lowDb"),
            Some(num(probe, "lowDb")),
            at(&envelope.low_db),
            t("db"),
        );
        c.near(
            &format!("{what} hop {i} onset"),
            Some(num(probe, "onset")),
            at(&envelope.onset),
            t("onset"),
        );
        c.near(
            &format!("{what} hop {i} lowOnset"),
            Some(num(probe, "lowOnset")),
            at(&envelope.low_onset),
            t("onset"),
        );
    }
    let e = &section["features"];
    let f = &analysis.features;
    let ms = |v: Option<i64>| v.map(|v| v as f64);
    c.near(
        &format!("{what} bodyDb"),
        Some(num(e, "bodyDb")),
        Some(f.body_db),
        t("db"),
    );
    c.near(
        &format!("{what} gateDb"),
        Some(num(e, "gateDb")),
        Some(f.gate_db),
        t("db"),
    );
    for (key, actual) in [
        ("soundStartMs", f.sound_start_ms),
        ("soundEndMs", f.sound_end_ms),
        ("outroStartMs", f.outro_start_ms),
        ("introEndMs", f.intro_end_ms),
    ] {
        c.near(&format!("{what} {key}"), num_or_none(e, key), ms(actual), t("ms"));
    }
    let boundaries: Vec<f64> = list(e, "boundariesMs").iter().filter_map(Value::as_f64).collect();
    if boundaries.len() == f.boundaries_ms.len() {
        for (b, a) in boundaries.iter().zip(&f.boundaries_ms) {
            c.near(&format!("{what} boundary"), Some(*b), Some(*a as f64), t("ms"));
        }
    } else {
        c.failures.push(format!(
            "{what} boundaries: expected {boundaries:?}, got {:?}",
            f.boundaries_ms
        ));
    }
    match (&e["tempo"], &f.tempo) {
        (Value::Null, None) => {}
        (Value::Null, Some(a)) => c.failures.push(format!("{what}: expected no tempo, got {a:?}")),
        (_, None) => c.failures.push(format!("{what}: expected a tempo, got none")),
        (e, Some(a)) => {
            c.near(&format!("{what} bpm"), Some(num(e, "bpm")), Some(a.bpm), t("bpm"));
            let confidence = num(e, "confidence");
            c.near(
                &format!("{what} confidence"),
                Some(confidence),
                Some(a.confidence),
                confidence * t("confidenceShare"),
            );
            c.near(
                &format!("{what} consistency"),
                Some(num(e, "consistency")),
                Some(a.consistency),
                0.02,
            );
            if flag(e, "steady") != a.steady || flag(e, "confident") != a.confident() {
                c.failures.push(format!(
                    "{what}: steady {}, confident {}",
                    a.steady,
                    a.confident()
                ));
            }
            c.near(
                &format!("{what} beatMs"),
                Some(num(e, "beatMs")),
                Some(a.beat_ms),
                t("ms") / 10.0,
            );
            c.near(
                &format!("{what} firstBeatMs"),
                Some(num(e, "firstBeatMs")),
                Some(a.first_beat_ms),
                t("ms"),
            );
            c.near(
                &format!("{what} downbeatMs"),
                Some(num(e, "downbeatMs")),
                Some(a.downbeat_ms),
                t("ms"),
            );
        }
    }
}

fn answer(mut filter: Biquad, input: impl Fn(usize) -> f64) -> Vec<f64> {
    (0..FILTER_SAMPLES).map(|n| filter.process(input(n))).collect()
}

fn check_biquad(c: &mut Checker, tol: &Value, f: &Value) {
    let kind = text(f, "type");
    let rate = int(f, "rate") as u32;
    let hz = num(f, "hz");
    let what = format!("{kind} at {hz} Hz, {rate} Hz");
    let within = num(tol, "sample");
    let low = kind == "lowPass";
    let q = num(f, "q");
    let shaped = if low {
        Biquad::low_pass_q(rate, hz, q)
    } else {
        Biquad::high_pass_q(rate, hz, q)
    };
    let expected = list(f, "coefficients");
    for (i, actual) in [shaped.b0, shaped.b1, shaped.b2, shaped.a1, shaped.a2]
        .into_iter()
        .enumerate()
    {
        c.near(
            &format!("{what} coefficient {i}"),
            expected[i].as_f64(),
            Some(actual),
            within,
        );
    }
    let plain = || {
        if low {
            Biquad::low_pass(rate, hz)
        } else {
            Biquad::high_pass(rate, hz)
        }
    };
    let impulse = answer(plain(), |n| if n == 0 { 1.0 } else { 0.0 });
    let sine = answer(plain(), |n| {
        0.5 * (2.0 * std::f64::consts::PI * 1_000.0 * n as f64 / f64::from(rate)).sin()
    });
    for (name, got) in [("impulse", impulse), ("sine", sine)] {
        for (i, x) in list(f, name).iter().enumerate() {
            c.near(
                &format!("{what} {name} {i}"),
                x.as_f64(),
                got.get(i).copied(),
                within,
            );
        }
    }
}

#[test]
fn the_copy_matches_the_shared_vectors() {
    let vectors: Value = serde_json::from_str(VECTORS).expect("the vectors are JSON");
    assert_eq!(
        vectors["version"].as_i64(),
        Some(1),
        "a vectors version this test does not know"
    );
    let tol = &vectors["tolerances"];
    let songs: HashMap<String, SyntheticSong> = vectors["songs"]
        .as_object()
        .expect("songs")
        .iter()
        .map(|(name, song)| (name.clone(), song_of(song)))
        .collect();
    let sections = list(&vectors, "sections");
    let analyses: Vec<SectionAnalysis> = std::thread::scope(|scope| {
        let jobs: Vec<_> = sections
            .iter()
            .map(|section| {
                let songs = &songs;
                scope.spawn(move || analyze(&songs[text(section, "song")], section))
            })
            .collect();
        jobs.into_iter().map(|j| j.join().expect("an analysis")).collect()
    });
    let mut c = Checker::default();
    for (section, analysis) in sections.iter().zip(&analyses) {
        check_section(&mut c, tol, section, analysis);
    }
    for f in list(&vectors, "biquads") {
        check_biquad(&mut c, tol, f);
    }
    for v in list(&vectors, "tempoTrust") {
        let tempo = Tempo {
            bpm: 120.0,
            confidence: num(v, "confidence"),
            consistency: 1.0,
            steady: flag(v, "steady"),
            beat_ms: 500.0,
            first_beat_ms: 0.0,
            downbeat_ms: 0.0,
        };
        if tempo.confident() != flag(v, "confident") {
            c.failures
                .push(format!("tempo trust {v}: confident {}", tempo.confident()));
        }
    }
    assert!(sections.len() >= 10, "the vectors lost their sections");
    assert!(
        c.failures.is_empty(),
        "{} off:\n{}",
        c.failures.len(),
        c.failures.join("\n")
    );
}

/// What a profile takes from the analysis: the first 45 s of a song, measured against the
/// whole song's body (a quiet start the head alone would call its body is an intro), a start
/// that never comes near the body ending its intro at the head's end, and the revision the
/// profile says it comes from.
#[test]
fn profiles_measure_45_s_heads_against_the_songs_body_and_say_their_revision() {
    assert_eq!((PROFILE_HEAD_MS, super::HEAD_SECS), (45_000, 45.0));
    assert_eq!((PROFILE_VERSION, PROFILE_REVISION), (1, 2));
    let vectors: Value = serde_json::from_str(VECTORS).expect("the vectors are JSON");
    let within = num(&vectors["tolerances"], "ms");
    let songs = &vectors["songs"];
    let heads: Vec<&Value> = list(&vectors, "sections")
        .iter()
        .filter(|s| text(s, "part") == "head")
        .collect();
    for head in &heads {
        let length = int(&songs[text(head, "song")], "lengthMs");
        assert_eq!(
            (num(head, "fromMs"), num(head, "toMs")),
            (0.0, length.min(PROFILE_HEAD_MS) as f64),
            "the head of {}",
            text(head, "song")
        );
    }

    let mut c = Checker::default();
    let ms = |a: SectionAnalysis| a.features.intro_end_ms.map(|v| v as f64);
    let (mut against_body, mut to_the_end) = (Vec::new(), 0);
    for head in heads.iter().filter(|s| num_or_none(s, "bodyLevelDb").is_some()) {
        let name = text(head, "song");
        let alone = heads
            .iter()
            .find(|s| {
                text(s, "song") == name
                    && num_or_none(s, "bodyLevelDb").is_none()
                    && num_or_none(s, "tagBpm").is_none()
            })
            .unwrap_or_else(|| panic!("{name} has no head measured on its own"));
        let song = song_of(&songs[name]);
        let want = num_or_none(&head["features"], "introEndMs");
        let want_alone = num_or_none(&alone["features"], "introEndMs");
        c.near(
            &format!("{name} head against the body: introEndMs"),
            want,
            ms(analyze(&song, head)),
            within,
        );
        c.near(
            &format!("{name} head alone: introEndMs"),
            want_alone,
            ms(analyze(&song, alone)),
            within,
        );
        if want.zip(want_alone).is_none_or(|(w, a)| (w - a).abs() > within) {
            against_body.push((name, *head, want));
        }
        if want == Some(num(head, "toMs") - num(head, "fromMs")) {
            to_the_end += 1;
        }
    }
    assert!(!against_body.is_empty(), "no head where the song's body changes its intro");
    assert!(to_the_end > 0, "no head whose intro lasts to its end");

    // A whole song's profile measures its head the same way, and says its revision.
    let (name, head, want) = against_body[0];
    let song = song_of(&songs[name]);
    let profile = TransitionProfile::of_envelope(&song.envelope(0.0, song.length_ms as f64, 4_093));
    assert_eq!((profile.version, profile.revision), (1, 2));
    c.near(
        &format!("{name} profile bodyDb"),
        num_or_none(head, "bodyLevelDb"),
        profile.body_db,
        num(&vectors["tolerances"], "db"),
    );
    c.near(
        &format!("{name} profile head introEndMs"),
        want,
        profile.head.features.intro_end_ms.map(|v| v as f64),
        within,
    );
    assert!(c.failures.is_empty(), "{}", c.failures.join("\n"));
}
