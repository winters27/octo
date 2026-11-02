//! The pure heart gate over the real cases: every case of `gate-cases-real.tsv` decided as the
//! reference decides it, from the frozen AcoustID answers, each print's R, ISRC and routing, the
//! shipped print comparison for proof 2, and the heard master's verdict from each case's own
//! column (the measure the heard-master check was proven by).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use octo_core::fingerprint::acoust_id_client::{AcoustIdAnswer, AcoustIdOutcome, parse_answer};
use octo_core::fingerprint::heart_gate::{
    Acceptance, AcquisitionOrigin, FileFacts, FileLabels, GateVerdict, HeardAnswerReading, HeardMaster,
    HeardReading, HeardRecord, HeardRecording, HeardSource, HeartGate, Judgement, MusicBrainzFact, Need,
    Refusal, TagClaim, Undecided, WaitingFor, became, read_heard_answer,
};

use super::preview_check;

/// Octo's default match score, 85%.
const MATCH_SCORE: f64 = 0.85;

fn gate_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../octo-core/tests/fixtures/heart_album/gate")
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The tab-separated rows of a fixture table, its comment lines left out.
fn rows(name: &str) -> Vec<Vec<String>> {
    read_text(&gate_dir().join(name))
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.split('\t').map(str::to_string).collect())
        .collect()
}

/// Each print's catalog R: its recording ids, its heard catalog track's ISRC, and its routing's
/// title and artist.
fn catalog_rs() -> HashMap<String, HeardRecording> {
    let mut rs: HashMap<String, HeardRecording> = HashMap::new();
    for row in rows("print-r.tsv") {
        rs.entry(row[0].clone()).or_default().ids = row[2].split(',').map(str::to_string).collect();
    }
    for row in rows("print-isrc.tsv") {
        rs.get_mut(&row[0]).expect("every ISRC row has an R").isrcs = vec![row[1].clone()];
    }
    for row in rows("print-routing.tsv") {
        let r = rs.get_mut(&row[0]).expect("every routing row has an R");
        r.title = row[1].clone();
        r.artist = row[2].clone();
    }
    rs
}

fn frozen(name: &str) -> AcoustIdAnswer {
    let file: serde_json::Value =
        serde_json::from_str(&read_text(&gate_dir().join(format!("answers/{name}.json")))).expect("JSON");
    parse_answer(&file["answer"])
        .expect("the answer reads")
        .expect("the answer's status is ok")
}

/// A raw print's items, as the shipped comparison reads them; None when there is no such print.
fn print_items(name: &str) -> Option<Vec<u32>> {
    let path = gate_dir().join(format!("prints/{name}.json"));
    if !path.exists() {
        return None;
    }
    let doc: serde_json::Value = serde_json::from_str(&read_text(&path)).expect("a print is JSON");
    let items = doc["fingerprint"].as_array().expect("a print lists its items");
    Some(
        items
            .iter()
            .map(|item| item.as_i64().expect("an item is a whole number") as u32)
            .collect(),
    )
}

/// A verdict in the reference's own words.
fn words(verdict: &GateVerdict) -> String {
    match verdict {
        GateVerdict::Accept {
            by: Acceptance::HeardMaster,
            tags: TagClaim::AsHeard,
        } => "accept: the heard master, as heard".into(),
        GateVerdict::Accept {
            by: Acceptance::HeardMaster,
            tags: TagClaim::Recording { .. },
        } => "accept: the heard master".into(),
        GateVerdict::Accept {
            by: Acceptance::HeardAudio,
            ..
        } => "accept A0".into(),
        GateVerdict::Accept {
            by: Acceptance::Sound,
            ..
        } => "accept: sound alone".into(),
        GateVerdict::Accept {
            by: Acceptance::SameTake,
            ..
        } => "accept: the same take".into(),
        GateVerdict::Refuse(Refusal::DiffersFromHeard) => "refuse: proof 2".into(),
        GateVerdict::Refuse(Refusal::Label) => "refuse: a label names another recording".into(),
        GateVerdict::Refuse(Refusal::AnotherRecording) => "refuse: AcoustID names another recording".into(),
        GateVerdict::Refuse(Refusal::NoEntry) => "refuse: AcoustID empty".into(),
        GateVerdict::Refuse(Refusal::Unknown) => "refuse: AcoustID unknown".into(),
        GateVerdict::Refuse(Refusal::Unreadable) => "refuse: AcoustID could not read it".into(),
        GateVerdict::Undecided(Undecided::KeepHeardCopy) => "undecided: keep the heard copy".into(),
        GateVerdict::Undecided(Undecided::NotYetProven) => "undecided: not yet proven".into(),
        GateVerdict::Hold(WaitingFor::AcoustId) => "hold: waiting for AcoustID to read what played".into(),
        GateVerdict::Hold(other) => format!("hold: {}", other.name()),
    }
}

/// One case's gate: R1(g) on the heard print's 240 s answer against the catalog R (the answer
/// read as busy for a `<print>!busy` heard print), and R becoming the heard recording when the
/// stream stood in with another version.
fn gate_for(catalog: &HeardRecording, heard: &str) -> HeartGate {
    let mut gate = HeartGate {
        user: "brandon".into(),
        origin: AcquisitionOrigin::Heart,
        r: Some(catalog.clone()),
        heard: None,
        heard_seconds: None,
        reading: HeardReading::NotHeard,
        stand_in: None,
    };
    if heard == "-" {
        return gate;
    }
    let (print, busy) = match heard.strip_suffix("!busy") {
        Some(print) => (print, true),
        None => (heard, false),
    };
    gate.heard = Some(HeardRecord {
        heard_at: Utc.with_ymd_and_hms(2026, 10, 28, 12, 0, 0).unwrap(),
        source: HeardSource::Video {
            id: format!("{print}-video"),
            size: Some(4_000_000),
            content_type: "audio/mp4".into(),
            itag: Some(140),
        },
        sample_id: Some(format!("{print}-240")),
    });
    let outcome = if busy {
        AcoustIdOutcome::Busy
    } else {
        AcoustIdOutcome::Answered(frozen(&format!("{print}-240")))
    };
    gate.reading = match read_heard_answer(&outcome, catalog, MATCH_SCORE) {
        HeardAnswerReading::Busy => HeardReading::Busy,
        HeardAnswerReading::NeedsClip => panic!("AcoustID answered for {heard}"),
        HeardAnswerReading::AnotherSong => HeardReading::AnotherSong,
        HeardAnswerReading::RStays { tag } => HeardReading::RStays { tag },
        HeardAnswerReading::Becomes(dominant) => {
            let AcoustIdOutcome::Answered(answer) = &outcome else {
                unreachable!("R becomes a recording only from an answer");
            };
            let (r, tag) = became(answer, &dominant, &[], catalog, MATCH_SCORE);
            gate.r = Some(r);
            HeardReading::RBecame { tag }
        }
        HeardAnswerReading::OnlyHeardOrMaster => HeardReading::OnlyHeardOrMaster,
    };
    gate
}

#[test]
fn every_real_case_is_decided_as_the_reference_decides_it() {
    let rs = catalog_rs();
    let mut decided = 0;
    let mut misses = Vec::new();
    for row in rows("gate-cases-real.tsv") {
        let [
            case,
            file,
            heard,
            r_from,
            label,
            identical,
            heard_master,
            expected,
        ] = row.as_slice()
        else {
            panic!("a case has eight fields: {row:?}");
        };
        let catalog = rs
            .get(r_from)
            .unwrap_or_else(|| panic!("{case}: no R for {r_from}"));
        let gate = gate_for(catalog, heard);
        let labels = FileLabels {
            isrcs: if label == "none" {
                Vec::new()
            } else {
                vec![label.clone()]
            },
            recording_id: None,
        };
        let mut facts = FileFacts::new(AcoustIdOutcome::Answered(frozen(&format!("{file}-120"))), labels);
        facts.identical = identical == "yes";
        if heard != "-" {
            let heard_print = heard.trim_end_matches("!busy");
            let file_items = print_items(&format!("{file}-120"));
            let heard_items = print_items(&format!("{heard_print}-240"));
            if let (Some(file_items), Some(heard_items)) = (file_items, heard_items) {
                facts.heard_distance = preview_check::compare(&file_items, &heard_items).map(|c| c.bit_error);
            }
        }
        let mut verdict = None;
        for _ in 0..8 {
            match gate.judge(&facts, MATCH_SCORE) {
                Judgement::Verdict(found) => {
                    verdict = Some(found);
                    break;
                }
                Judgement::Needs(Need::HeardMaster) => {
                    facts.heard_master = Some(match heard_master.as_str() {
                        "yes" => HeardMaster::Same,
                        "no" => HeardMaster::Differs,
                        other => panic!("{case}: the heard master was asked for with {other:?}"),
                    });
                }
                Judgement::Needs(Need::CurrentRecording(id)) => {
                    facts
                        .current_recordings
                        .push((id.clone(), MusicBrainzFact::Known(id)));
                }
                Judgement::Needs(Need::IsrcRecordings(code)) => {
                    panic!("{case}: ISRC {code} asked of MusicBrainz with an answer to read")
                }
            }
        }
        let got = words(&verdict.unwrap_or_else(|| panic!("{case}: the gate kept asking")));
        if &got != expected {
            misses.push(format!("{case}: {got} (expected {expected})"));
        }
        decided += 1;
    }
    assert!(
        misses.is_empty(),
        "{} of {decided} missed:\n{}",
        misses.len(),
        misses.join("\n")
    );
    assert_eq!(decided, 31);
}

/// Every real case through Octo's own file check, `verify_for_heart`: the file printed in a heart's
/// `fpcalc` place, its frozen answer asked of AcoustID on the heart's class, its ISRC read from its
/// own tags, proof 2 against the heard print the samples keep, the heard master from a round over
/// the heard video fetched again, and MusicBrainz asked for the current id of a merged one. Every
/// case is decided as the reference decides it, and no file's code is asked of MusicBrainz while
/// its answer can be read.
#[tokio::test]
async fn every_real_case_is_decided_so_by_octos_own_file_check() {
    use crate::services::fingerprint::download_verification_service::{HeartCandidate, LabelSource};
    use crate::services::fingerprint::heart_test_support::{
        GateSetup, frozen_body, gate_rig, sound_bytes, sound_flac,
    };
    use crate::services::prefetch::heard_samples::HeardPrints;

    let rs = catalog_rs();
    let rig = gate_rig(GateSetup::default()).await;
    for entry in std::fs::read_dir(gate_dir().join("answers")).expect("the answers") {
        let name = entry
            .expect("an answer")
            .path()
            .file_stem()
            .expect("a name")
            .to_string_lossy()
            .to_string();
        rig.answers.answer(&name, &frozen_body(&name));
    }
    let mut decided = 0;
    let mut misses = Vec::new();
    for (n, row) in rows("gate-cases-real.tsv").iter().enumerate() {
        let [
            case,
            file,
            heard,
            r_from,
            label,
            identical,
            heard_master,
            expected,
        ] = row.as_slice()
        else {
            panic!("a case has eight fields: {row:?}");
        };
        let catalog = rs
            .get(r_from)
            .unwrap_or_else(|| panic!("{case}: no R for {r_from}"));
        let mut gate = gate_for(catalog, heard);
        let heard_print = heard.trim_end_matches("!busy");
        // What was heard: its own video, its whole print kept, and its audio the shim sends again.
        if let Some(record) = gate.heard.as_mut() {
            let video = format!("case-{n}-video");
            record.source = HeardSource::Video {
                id: video.clone(),
                size: Some(4_000_000),
                content_type: "audio/mp4".into(),
                itag: Some(140),
            };
            let sample = format!("case-{n}-heard");
            record.sample_id = Some(sample.clone());
            if let Some(items) = print_items(&format!("{heard_print}-240")) {
                rig.rig.samples.store(
                    &sample,
                    &HeardPrints {
                        raw: items,
                        compressed: format!("{heard_print}-240"),
                        seconds: 240,
                        whole: true,
                        reaches_end: false,
                    },
                    Some(&video),
                );
            }
            rig.shim_streams(&video, sound_bytes("-", &format!("{heard_print}-master"), "-"))
                .await;
        }
        let print = format!("{file}-120");
        rig.printer.register(
            &print,
            print_items(&print).unwrap_or_else(|| panic!("{case}: no print {print}")),
        );
        let master = match heard_master.as_str() {
            "yes" => format!("{heard_print}-master"),
            _ => format!("{file}-another-master-{n}"),
        };
        let path = rig.dir.path().join(format!("case-{n}.flac"));
        sound_flac(
            &path,
            200,
            &sound_bytes(&print, &master, &print),
            (label != "none").then_some(label.as_str()),
        );
        let candidate = HeartCandidate {
            labels: LabelSource::TheFile,
            identity: (identical == "yes")
                .then(|| gate.heard.as_ref().map(|heard| heard.source.clone()))
                .flatten(),
            acquisition_key: format!("soulseek:case-{n}"),
        };
        let check = rig.service.verify_for_heart(&path, &candidate, &gate).await;
        let got = words(&check.verdict);
        if &got != expected {
            misses.push(format!("{case}: {got} (expected {expected})"));
        }
        decided += 1;
    }
    assert!(
        misses.is_empty(),
        "{} of {decided} missed:\n{}",
        misses.len(),
        misses.join("\n")
    );
    assert_eq!(decided, 31);
    let codes: Vec<String> = rig
        .music_brainz()
        .asked()
        .into_iter()
        .filter(|path| path.starts_with("/ws/2/isrc/"))
        .collect();
    assert!(codes.is_empty(), "codes asked with answers to read: {codes:?}");
    assert!(
        rig.answers.asked().iter().all(|name| name.ends_with("-120")),
        "only the files were looked up: {:?}",
        rig.answers.asked()
    );
}
