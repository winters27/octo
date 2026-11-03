//! Tests for the pure heart gate over answers built in the recorded shape: the heard answer's
//! branches, R becoming the heard recording, a never-played heart's stand-in, A0's identity, the
//! label rules, the import rule, and the gate's order with the facts it asks for.

use std::path::Path;

use chrono::{TimeZone, Utc};

use super::*;
use crate::fingerprint::acoust_id_client::{AnswerArtist, AnswerResult, parse_answer};

/// Octo's default match score, 85%.
const MATCH_SCORE: f64 = 0.85;

const BEATLES: (&str, &str) = ("b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d", "The Beatles");
const OASIS: (&str, &str) = ("39ab1aed-75e0-4140-bd47-540276886b60", "Oasis");
const HELP_2009: &str = "4416d38e-0f68-4c75-93b9-c1a793fccb49";
const HELP_2015: &str = "01b6e4de-ff48-4ec0-862e-cd623c924d7b";
const ISRC_2009: &str = "GBAYE0601465";
const ISRC_2015: &str = "GBUM71505900";
const WONDERWALL_ISRC: &str = "GBBQY9500021";

fn recording(id: &str, title: &str, artist: (&str, &str), isrcs: &[&str], sources: i64) -> AnswerRecording {
    AnswerRecording {
        id: id.into(),
        title: title.into(),
        artists: vec![AnswerArtist {
            id: Some(artist.0.into()),
            name: artist.1.into(),
        }],
        duration_seconds: None,
        isrcs: isrcs.iter().map(|code| code.to_string()).collect(),
        sources,
    }
}

fn help(id: &str, isrcs: &[&str], sources: i64) -> AnswerRecording {
    recording(id, "Help!", BEATLES, isrcs, sources)
}

fn wonderwall(sources: i64) -> AnswerRecording {
    recording("w1", "Wonderwall", OASIS, &[WONDERWALL_ISRC], sources)
}

fn answer(recordings: Vec<AnswerRecording>) -> AcoustIdAnswer {
    AcoustIdAnswer {
        results: vec![AnswerResult {
            id: None,
            score: 0.97,
            recordings,
        }],
    }
}

fn answered(recordings: Vec<AnswerRecording>) -> AcoustIdOutcome {
    AcoustIdOutcome::Answered(answer(recordings))
}

/// What a 2009-mix file of Help! reads as: the 2009 recording dominant, the 2015 mix beside it
/// with its own ISRC, so AcoustID cannot tell which mix the file is.
fn clustered() -> Vec<AnswerRecording> {
    vec![
        help(HELP_2009, &[ISRC_2009], 700),
        help(HELP_2015, &[ISRC_2015], 300),
    ]
}

/// An answer that reads clear for the 2009 recording.
fn clear() -> Vec<AnswerRecording> {
    vec![help(HELP_2009, &[ISRC_2009], 50)]
}

/// R for a 2009 heart of Help!.
fn help_r() -> HeardRecording {
    HeardRecording {
        ids: vec![HELP_2009.into()],
        isrcs: vec![ISRC_2009.into()],
        title: "Help! (Remastered 2009)".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    }
}

fn heard_video() -> HeardSource {
    HeardSource::Video {
        id: "heard-video".into(),
        size: Some(2_300_000),
        content_type: "audio/mp4".into(),
        itag: Some(140),
    }
}

fn gate(reading: HeardReading) -> HeartGate {
    HeartGate {
        user: "brandon".into(),
        origin: AcquisitionOrigin::Heart,
        r: Some(help_r()),
        heard: Some(HeardRecord {
            heard_at: Utc.with_ymd_and_hms(2026, 10, 28, 12, 0, 0).unwrap(),
            source: heard_video(),
            sample_id: Some("sample-1".into()),
        }),
        heard_seconds: Some(139),
        reading,
        stand_in: None,
    }
}

fn r_stays() -> HeartGate {
    gate(HeardReading::RStays { tag: None })
}

fn never_played() -> HeartGate {
    HeartGate {
        heard: None,
        ..gate(HeardReading::NotHeard)
    }
}

fn facts(answer: AcoustIdOutcome) -> FileFacts {
    FileFacts::new(answer, FileLabels::default())
}

fn with_isrc(answer: AcoustIdOutcome, code: &str) -> FileFacts {
    FileFacts::new(
        answer,
        FileLabels {
            isrcs: vec![code.into()],
            recording_id: None,
        },
    )
}

fn with_recording_id(answer: AcoustIdOutcome, id: &str) -> FileFacts {
    FileFacts::new(
        answer,
        FileLabels {
            isrcs: Vec::new(),
            recording_id: Some(id.into()),
        },
    )
}

fn as_heard(by: Acceptance) -> GateVerdict {
    GateVerdict::Accept {
        by,
        tags: TagClaim::AsHeard,
    }
}

const KEEP: GateVerdict = GateVerdict::Undecided(Undecided::KeepHeardCopy);
const NOT_YET: GateVerdict = GateVerdict::Undecided(Undecided::NotYetProven);

/// Judges a file to its verdict as Octo's runner does, answering each fact the gate asks for: the
/// heard master from `master` (a test fails when the gate asks for it and none is given), a
/// recording asked of MusicBrainz as itself (no merge). Returns the verdict and what was asked.
fn settle(gate: &HeartGate, mut facts: FileFacts, master: Option<HeardMaster>) -> (GateVerdict, Vec<Need>) {
    let mut asked = Vec::new();
    for _ in 0..8 {
        match gate.judge(&facts, MATCH_SCORE) {
            Judgement::Verdict(verdict) => return (verdict, asked),
            Judgement::Needs(need) => {
                asked.push(need.clone());
                match need {
                    Need::HeardMaster => {
                        facts.heard_master = Some(master.expect("the heard master was asked for"));
                    }
                    Need::CurrentRecording(id) => {
                        facts
                            .current_recordings
                            .push((id.clone(), MusicBrainzFact::Known(id)));
                    }
                    Need::IsrcRecordings(code) => panic!("ISRC {code} asked of MusicBrainz"),
                }
            }
        }
    }
    panic!("the gate kept asking: {asked:?}");
}

fn verdict_of(gate: &HeartGate, facts: FileFacts, master: Option<HeardMaster>) -> GateVerdict {
    settle(gate, facts, master).0
}

#[test]
fn the_heard_answer_reads_in_branch_order() {
    let r = help_r();
    let read = |outcome: AcoustIdOutcome| read_heard_answer(&outcome, &r, MATCH_SCORE);
    assert_eq!(read(AcoustIdOutcome::Busy), HeardAnswerReading::Busy);
    assert_eq!(read(AcoustIdOutcome::Off), HeardAnswerReading::NeedsClip);
    assert_eq!(
        read(AcoustIdOutcome::Unreadable { code: Some(3) }),
        HeardAnswerReading::NeedsClip
    );
    // Another song at 90% of the weight wins over R's 2%.
    assert_eq!(
        read(answered(vec![wonderwall(900), help(HELP_2009, &[ISRC_2009], 20)])),
        HeardAnswerReading::AnotherSong
    );
    assert_eq!(
        read(answered(clear())),
        HeardAnswerReading::RStays {
            tag: Some(HELP_2009.into())
        }
    );
    assert_eq!(
        read(answered(clustered())),
        HeardAnswerReading::RStays { tag: None }
    );
    // Another song leads under half the weight while R holds 10%: R stays.
    assert_eq!(
        read(answered(vec![
            wonderwall(45),
            recording("y1", "Yesterday", BEATLES, &[], 45),
            help(HELP_2009, &[ISRC_2009], 10),
        ])),
        HeardAnswerReading::RStays { tag: None }
    );
    // R holds 0.5% and the 2015 mix holds the rest: R becomes it.
    let stood_in = read(answered(vec![
        help(HELP_2015, &[ISRC_2015], 995),
        help(HELP_2009, &[ISRC_2009], 5),
    ]));
    let HeardAnswerReading::Becomes(became_to) = &stood_in else {
        panic!("{stood_in:?}");
    };
    assert_eq!(became_to.id, HELP_2015);
    // The same, but the 2015 mix holds under half: nothing proves what played.
    assert_eq!(
        read(answered(vec![
            help(HELP_2015, &[ISRC_2015], 400),
            wonderwall(350),
            recording("y1", "Yesterday", BEATLES, &[], 245),
            help(HELP_2009, &[ISRC_2009], 5),
        ])),
        HeardAnswerReading::OnlyHeardOrMaster
    );
    assert_eq!(
        read(AcoustIdOutcome::Answered(AcoustIdAnswer::default())),
        HeardAnswerReading::OnlyHeardOrMaster
    );
    let unlinked = AcoustIdAnswer {
        results: vec![AnswerResult {
            id: Some("fp".into()),
            score: 0.95,
            recordings: Vec::new(),
        }],
    };
    assert_eq!(
        read(AcoustIdOutcome::Answered(unlinked)),
        HeardAnswerReading::OnlyHeardOrMaster
    );
}

/// Another song AcoustID names too faintly to believe (one submission) is never let through as
/// "nothing proves what played", where the heard audio itself could be kept: the catalog's clip
/// decides first. Another recording of R's song as faint still reads as before.
#[test]
fn a_faint_other_song_in_the_heard_answer_sends_it_to_the_clip() {
    let r = help_r();
    let read = |outcome: AcoustIdOutcome| read_heard_answer(&outcome, &r, MATCH_SCORE);
    assert_eq!(read(answered(vec![wonderwall(1)])), HeardAnswerReading::NeedsClip);
    // Another song leads, under half the weight.
    assert_eq!(
        read(answered(vec![
            wonderwall(2),
            recording("y1", "Yesterday", BEATLES, &[], 2),
            recording("j1", "Hey Jude", BEATLES, &[], 1),
        ])),
        HeardAnswerReading::NeedsClip
    );
    // Another mix of R's song leads as faintly: nothing proves what played, as before.
    assert_eq!(
        read(answered(vec![
            help(HELP_2015, &[ISRC_2015], 1),
            wonderwall(1),
            recording("y1", "Yesterday", BEATLES, &[], 1),
        ])),
        HeardAnswerReading::OnlyHeardOrMaster
    );
}

/// An R that names no recording (the catalog gave only the song's names) cannot tell one recording
/// of its song from another: a file AcoustID names as the song reads ambiguous, never another
/// recording, so the heard master still decides it; what played still becomes R.
#[test]
fn with_no_recording_ids_the_song_reads_ambiguous_and_the_master_decides() {
    let named = HeardRecording {
        ids: Vec::new(),
        duplicates: Vec::new(),
        ..help_r()
    };
    let reading = read_answer(&answer(clear()), &named, MATCH_SCORE);
    assert_eq!(reading.reading, Reading::Ambiguous);
    let another_song = read_answer(&answer(vec![wonderwall(40)]), &named, MATCH_SCORE);
    assert_eq!(another_song.reading, Reading::Another);
    let gate = HeartGate {
        r: Some(named.clone()),
        ..r_stays()
    };
    let (verdict, asked) = settle(&gate, facts(answered(clear())), Some(HeardMaster::Same));
    assert_eq!(verdict, as_heard(Acceptance::HeardMaster));
    assert_eq!(asked, [Need::HeardMaster]);
    assert_eq!(
        verdict_of(&gate, facts(answered(clear())), Some(HeardMaster::Differs)),
        KEEP
    );
    let stood_in = read_heard_answer(&answered(clear()), &named, MATCH_SCORE);
    assert!(
        matches!(&stood_in, HeardAnswerReading::Becomes(recording) if recording.id == HELP_2009),
        "{stood_in:?}"
    );
}

fn frozen(name: &str) -> AcoustIdAnswer {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/heart_album/gate/answers/{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let file: serde_json::Value = serde_json::from_str(&text).expect("an answer file is JSON");
    parse_answer(&file["answer"])
        .expect("the answer reads")
        .expect("the answer's status is ok")
}

/// The 2015 Help! stream for a catalog row whose recording AcoustID barely names: R becomes the
/// dominant 2009 recording, but its reading against that recording is not clear, so no tag and
/// an R marked as built on an unclear reading.
#[test]
fn r_becomes_the_dominant_recording_with_no_tag_on_an_unclear_reading() {
    let catalog = HeardRecording {
        ids: vec!["b7ecb9fc-94e5-430d-83b1-d8987a1ad75b".into()],
        isrcs: vec!["GBUM72306558".into()],
        title: "Help!".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    };
    let heard = frozen("help-2015-240");
    let reading = read_heard_answer(&AcoustIdOutcome::Answered(heard.clone()), &catalog, MATCH_SCORE);
    let HeardAnswerReading::Becomes(dominant) = &reading else {
        panic!("{reading:?}");
    };
    assert_eq!(dominant.id, HELP_2009);
    let (r, tag) = became(&heard, dominant, &[], &catalog, MATCH_SCORE);
    assert_eq!(tag, None);
    assert!(r.from_unclear);
    assert_eq!(r.ids, [HELP_2009]);
    assert_eq!(r.isrcs, dominant.isrcs);
    assert_eq!((r.title.as_str(), r.artist.as_str()), ("Help!", "The Beatles"));
    assert_eq!(r.artist_ids, [BEATLES.0]);
}

/// R becomes D and its duplicates, D first and at most six ids, every duplicate kept; D's id is
/// the tag only on a clear reading against that R with D's own two submissions.
#[test]
fn r_becomes_d_and_its_duplicates_and_tags_d_only_on_a_clear_reading() {
    let catalog = help_r();
    let heard = answer(vec![help(HELP_2015, &[ISRC_2015], 50)]);
    let dominant = read_answer(&heard, &catalog, MATCH_SCORE)
        .dominant()
        .cloned()
        .expect("a dominant recording");
    let duplicates: Vec<String> = ["d1", HELP_2015, "d2", "d3", "d1", "d4", "d5", "d6"]
        .iter()
        .map(|id| id.to_string())
        .collect();
    let (r, tag) = became(&heard, &dominant, &duplicates, &catalog, MATCH_SCORE);
    assert_eq!(tag.as_deref(), Some(HELP_2015));
    assert!(!r.from_unclear);
    assert_eq!(r.ids, [HELP_2015, "d1", "d2", "d3", "d4", "d5"]);
    assert_eq!(r.duplicates, [HELP_2015, "d1", "d2", "d3", "d4", "d5", "d6"]);
    assert_eq!(r.isrcs, [ISRC_2015]);
    assert_eq!(r.title, catalog.title);

    let lone = answer(vec![help(HELP_2015, &[ISRC_2015], 1)]);
    let dominant = read_answer(&lone, &catalog, MATCH_SCORE)
        .dominant()
        .cloned()
        .expect("a dominant recording");
    let (r, tag) = became(&lone, &dominant, &[], &catalog, MATCH_SCORE);
    assert_eq!(tag, None);
    assert!(r.from_unclear);
}

#[test]
fn a_stand_in_narrows_a_catalog_r_only_to_its_own_recordings() {
    let r = help_r();
    let read = |recordings| read_stand_in(&answer(recordings), &r, Some(139), MATCH_SCORE);
    let inside = read(vec![
        help(HELP_2009, &[ISRC_2009], 60),
        help(HELP_2015, &[ISRC_2015], 40),
    ]);
    assert_eq!(
        inside.recording.map(|found| found.id),
        Some(HELP_2009.to_string())
    );
    assert!(inside.is_reference);
    let outside = read(vec![
        help(HELP_2015, &[ISRC_2015], 60),
        help(HELP_2009, &[ISRC_2009], 40),
    ]);
    assert_eq!(outside.recording, None);
    assert!(outside.is_reference);
    let another = read(vec![wonderwall(900), help(HELP_2009, &[ISRC_2009], 5)]);
    assert_eq!(another.recording, None);
    assert!(!another.is_reference);
}

#[test]
fn a_stand_in_gives_a_search_heart_its_r_when_the_length_fits() {
    let r = HeardRecording {
        title: "Help!".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    };
    let timed = AnswerRecording {
        duration_seconds: Some(140.5),
        ..help(HELP_2015, &[ISRC_2015], 60)
    };
    let stand_in = answer(vec![timed, help(HELP_2009, &[ISRC_2009], 40)]);
    let fits = read_stand_in(&stand_in, &r, Some(139), MATCH_SCORE);
    assert_eq!(fits.recording.map(|found| found.id), Some(HELP_2015.to_string()));
    assert!(fits.is_reference);
    let too_long = read_stand_in(&stand_in, &r, Some(200), MATCH_SCORE);
    assert_eq!(too_long.recording, None);
    assert!(!too_long.is_reference);
    assert!(read_stand_in(&stand_in, &r, None, MATCH_SCORE).is_reference);
}

#[test]
fn the_heard_video_is_its_own_stream_at_its_size_type_and_format() {
    let heard = heard_video();
    let video = |size: Option<u64>, content_type: &str, itag: Option<u32>| HeardSource::Video {
        id: "heard-video".into(),
        size,
        content_type: content_type.into(),
        itag,
    };
    assert!(is_heard_audio(
        &heard,
        &video(Some(2_300_000), "audio/mp4", Some(140))
    ));
    assert!(is_heard_audio(&heard, &video(Some(2_300_000), "audio/mp4", None)));
    assert!(!is_heard_audio(
        &heard,
        &video(Some(2_300_000), "audio/mp4", Some(251))
    ));
    assert!(!is_heard_audio(
        &heard,
        &video(Some(2_300_001), "audio/mp4", Some(140))
    ));
    assert!(!is_heard_audio(
        &heard,
        &video(Some(2_300_000), "audio/webm", Some(140))
    ));
    assert!(!is_heard_audio(&heard, &video(None, "audio/mp4", Some(140))));
    let unknown_size = video(None, "audio/mp4", None);
    assert!(!is_heard_audio(&unknown_size, &unknown_size));
    let other = HeardSource::Video {
        id: "another-video".into(),
        size: Some(2_300_000),
        content_type: "audio/mp4".into(),
        itag: Some(140),
    };
    assert!(!is_heard_audio(&heard, &other));
}

#[test]
fn a_refetched_copy_under_the_heard_key_is_another_copy() {
    let at = |minute| Some(Utc.with_ymd_and_hms(2026, 10, 28, 12, minute, 0).unwrap());
    let copy = |ready_at, source: &str| HeardSource::Copy {
        key: "soulseek_help".into(),
        ready_at,
        source_file: Some(source.into()),
    };
    let heard = copy(at(5), "Help!.flac");
    assert!(is_heard_audio(&heard, &copy(at(5), "Help!.flac")));
    assert!(!is_heard_audio(&heard, &copy(at(9), "Help!.flac")));
    assert!(!is_heard_audio(&heard, &copy(at(5), "Help! (2015).flac")));
    let undated = copy(None, "Help!.flac");
    assert!(!is_heard_audio(&undated, &undated));
    let library = HeardSource::Library {
        path: "/music/Help!.flac".into(),
    };
    let acquired = HeardSource::Acquired {
        path: "/music/Help!.flac".into(),
    };
    assert!(is_heard_audio(&library, &acquired));
    assert!(!is_heard_audio(
        &library,
        &HeardSource::Library {
            path: "/music/Yesterday.flac".into()
        }
    ));
    assert!(!is_heard_audio(&library, &heard));
}

/// The import rule refuses on the reading or a label naming another recording and otherwise lets
/// the shipped verdict stand, proven only by a clear reading.
#[test]
fn the_import_rule_only_adds_refusals() {
    let r = help_r();
    let judge = |recordings, labels: &[&str]| {
        let reading = read_answer(&answer(recordings), &r, MATCH_SCORE);
        let facts = FileFacts::new(
            AcoustIdOutcome::Busy,
            FileLabels {
                isrcs: labels.iter().map(|code| code.to_string()).collect(),
                recording_id: None,
            },
        );
        import_judgement(&reading, &facts, &r)
    };
    assert_eq!(
        judge(vec![wonderwall(900), help(HELP_2009, &[], 5)], &[]),
        ImportJudgement::Refuse
    );
    assert_eq!(judge(clustered(), &[ISRC_2015]), ImportJudgement::Refuse);
    assert_eq!(judge(clustered(), &[]), ImportJudgement::Stands { proven: false });
    assert_eq!(
        judge(clustered(), &[ISRC_2009]),
        ImportJudgement::Stands { proven: false }
    );
    assert_eq!(judge(clear(), &[]), ImportJudgement::Stands { proven: true });
    let unresolved = FileFacts::new(
        AcoustIdOutcome::Busy,
        FileLabels {
            isrcs: Vec::new(),
            recording_id: Some("not-asked-yet".into()),
        },
    );
    let reading = read_answer(&answer(clustered()), &r, MATCH_SCORE);
    assert_eq!(
        import_judgement(&reading, &unresolved, &r),
        ImportJudgement::Stands { proven: false }
    );
}

#[test]
fn labels_name_r_another_mix_or_another_song() {
    let r = help_r();
    let reading = read_answer(
        &answer(vec![
            help(HELP_2009, &[ISRC_2009, "SHARED00001"], 700),
            help(HELP_2015, &[ISRC_2015, "SHARED00001"], 290),
            wonderwall(10),
        ]),
        &r,
        MATCH_SCORE,
    );
    let check = |codes: &[&str]| {
        let facts = FileFacts::new(
            AcoustIdOutcome::Busy,
            FileLabels {
                isrcs: codes.iter().map(|code| code.to_string()).collect(),
                recording_id: None,
            },
        );
        check_labels(Some(&reading), &facts, &r)
    };
    assert_eq!(check(&[ISRC_2009]), LabelCheck::Clean);
    assert_eq!(check(&["SHARED00001"]), LabelCheck::Clean);
    assert_eq!(
        check(&[ISRC_2015]),
        LabelCheck::NamesAnother { other_song: false }
    );
    assert_eq!(
        check(&["NOBODY00001"]),
        LabelCheck::NamesAnother { other_song: false }
    );
    assert_eq!(
        check(&[WONDERWALL_ISRC]),
        LabelCheck::NamesAnother { other_song: true }
    );
    // Another song wins over every other finding, and an unasked fact over another mix.
    let mut facts = FileFacts::new(
        AcoustIdOutcome::Busy,
        FileLabels {
            isrcs: vec![ISRC_2015.into(), WONDERWALL_ISRC.into()],
            recording_id: Some("unknown-id".into()),
        },
    );
    assert_eq!(
        check_labels(Some(&reading), &facts, &r),
        LabelCheck::NamesAnother { other_song: true }
    );
    facts.labels.isrcs = vec![ISRC_2015.into()];
    assert_eq!(
        check_labels(Some(&reading), &facts, &r),
        LabelCheck::Needs(Need::CurrentRecording("unknown-id".into()))
    );
    facts
        .current_recordings
        .push(("unknown-id".into(), MusicBrainzFact::Busy));
    assert_eq!(check_labels(Some(&reading), &facts, &r), LabelCheck::Busy);
}

/// With no key the file has no reading, so only the very audio that played or the heard master
/// can be kept; a heart never played keeps nothing.
#[test]
fn with_no_key_only_the_heard_audio_or_master_is_kept() {
    let gate = r_stays();
    let identical = FileFacts {
        identical: true,
        ..facts(AcoustIdOutcome::Off)
    };
    assert_eq!(
        verdict_of(&gate, identical, None),
        as_heard(Acceptance::HeardAudio)
    );
    let (verdict, asked) = settle(&gate, facts(AcoustIdOutcome::Off), Some(HeardMaster::Same));
    assert_eq!(verdict, as_heard(Acceptance::HeardMaster));
    assert_eq!(asked, [Need::HeardMaster]);
    assert_eq!(
        verdict_of(&gate, facts(AcoustIdOutcome::Off), Some(HeardMaster::Differs)),
        KEEP
    );
    let (verdict, asked) = settle(&never_played(), facts(AcoustIdOutcome::Off), None);
    assert_eq!(verdict, NOT_YET);
    assert!(asked.is_empty());
}

/// With AcoustID off, a clip that reads "this song" lets A0 and A2 run; any other clip reading
/// holds whatever the file is.
#[test]
fn with_acoustid_off_the_clip_decides_whether_a0_and_a2_run() {
    let this_song = gate(HeardReading::Clip(ClipReading::ThisSong));
    let identical = FileFacts {
        identical: true,
        ..facts(AcoustIdOutcome::Off)
    };
    assert_eq!(
        verdict_of(&this_song, identical.clone(), None),
        as_heard(Acceptance::HeardAudio)
    );
    assert_eq!(
        verdict_of(&this_song, facts(AcoustIdOutcome::Off), Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
    assert_eq!(
        verdict_of(
            &this_song,
            facts(AcoustIdOutcome::Off),
            Some(HeardMaster::Differs)
        ),
        KEEP
    );
    for clip in [
        ClipReading::AnotherSongOrTake,
        ClipReading::Unclear,
        ClipReading::NoClip,
    ] {
        let (verdict, asked) = settle(&gate(HeardReading::Clip(clip)), identical.clone(), None);
        assert_eq!(verdict, GateVerdict::Hold(WaitingFor::AcoustIdOff), "{clip:?}");
        assert!(asked.is_empty());
    }
    for wait in [WaitingFor::Deezer, WaitingFor::Shim] {
        let busy = gate(HeardReading::Clip(ClipReading::Busy(wait)));
        assert_eq!(
            verdict_of(&busy, identical.clone(), None),
            GateVerdict::Hold(wait)
        );
    }
    // A clip check fpcalc could not print for knows nothing of what played: the file waits with
    // the heart, never kept and never refused.
    let unprinted = gate(HeardReading::Clip(ClipReading::Unprinted));
    let (verdict, asked) = settle(&unprinted, identical.clone(), None);
    assert_eq!(verdict, GateVerdict::Hold(ClipReading::UNPRINTED_WAITS_FOR));
    assert!(asked.is_empty());
}

/// Once the heard answer is read, a file whose own lookup is busy is kept when it is the heard
/// master; otherwise it waits for AcoustID.
#[test]
fn a2_accepts_while_the_files_own_lookup_is_busy() {
    let gate = r_stays();
    assert_eq!(
        verdict_of(&gate, facts(AcoustIdOutcome::Busy), Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
    assert_eq!(
        verdict_of(&gate, facts(AcoustIdOutcome::Busy), Some(HeardMaster::Differs)),
        GateVerdict::Hold(WaitingFor::AcoustId)
    );
    assert_eq!(
        verdict_of(&never_played(), facts(AcoustIdOutcome::Busy), None),
        GateVerdict::Hold(WaitingFor::AcoustId)
    );
}

/// While the heard lookup is busy nothing it guards may be kept: the file holds whether it is
/// the heard master or not, and the heard master is never asked for. Proof 2 still refuses.
#[test]
fn a_busy_heard_lookup_holds_the_file_whatever_a2_says() {
    let gate = gate(HeardReading::Busy);
    for identical in [false, true] {
        let file = FileFacts {
            identical,
            heard_distance: Some(0.04),
            ..facts(answered(clustered()))
        };
        let (verdict, asked) = settle(&gate, file, None);
        assert_eq!(verdict, GateVerdict::Hold(WaitingFor::AcoustId));
        assert!(asked.is_empty());
    }
    let far = FileFacts {
        heard_distance: Some(0.40),
        ..facts(answered(clustered()))
    };
    assert_eq!(
        verdict_of(&gate, far, None),
        GateVerdict::Refuse(Refusal::DiffersFromHeard)
    );
}

#[test]
fn no_entry_and_an_unlinked_fingerprint_refuse_unless_a2_accepts() {
    let gate = r_stays();
    let empty = || facts(AcoustIdOutcome::Answered(AcoustIdAnswer::default()));
    assert_eq!(
        verdict_of(&gate, empty(), Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::NoEntry)
    );
    assert_eq!(
        verdict_of(&gate, empty(), Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
    // A 2015-mix file's own fingerprint at 0.98, unlinked, beside a 0.90 link to the 2009
    // recording: never accepted as 2009 by sound.
    let above = || {
        facts(AcoustIdOutcome::Answered(AcoustIdAnswer {
            results: vec![
                AnswerResult {
                    id: Some("linked".into()),
                    score: 0.90,
                    recordings: clear(),
                },
                AnswerResult {
                    id: Some("unlinked".into()),
                    score: 0.98,
                    recordings: Vec::new(),
                },
            ],
        }))
    };
    assert_eq!(
        verdict_of(&gate, above(), Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::Unknown)
    );
    assert_eq!(
        verdict_of(&gate, above(), Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
    let (verdict, asked) = settle(&never_played(), above(), None);
    assert_eq!(verdict, GateVerdict::Refuse(Refusal::Unknown));
    assert!(asked.is_empty());
}

/// After an A2 pass the heard audio wins over a label naming another mix of the song; only a
/// label naming another song refuses, and then the heard master is never asked for.
#[test]
fn a_label_after_an_a2_pass_refuses_only_another_song() {
    let gate = r_stays();
    let mut recordings = clustered();
    recordings.push(wonderwall(5));
    let file = |code: &str| with_isrc(answered(recordings.clone()), code);
    assert_eq!(
        verdict_of(&gate, file(ISRC_2015), Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
    assert_eq!(
        verdict_of(&gate, file(ISRC_2015), Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::Label)
    );
    let (verdict, asked) = settle(&gate, file(WONDERWALL_ISRC), None);
    assert_eq!(verdict, GateVerdict::Refuse(Refusal::Label));
    assert!(asked.is_empty());
    assert_eq!(
        verdict_of(&gate, file(ISRC_2009), Some(HeardMaster::Differs)),
        KEEP
    );
}

/// A `MUSICBRAINZ_TRACKID` outside R is asked of MusicBrainz first: a merged-away id of R's
/// recording is R's, a duplicate of R's needs no question, and any other recording names another.
#[test]
fn a_merged_recording_id_label_is_held_to_r_after_its_lookup() {
    let gate = r_stays();
    let file = |id: &str| with_recording_id(answered(clustered()), id);
    let mut merged = file("merged-away");
    assert_eq!(
        gate.judge(&merged, MATCH_SCORE),
        Judgement::Needs(Need::CurrentRecording("merged-away".into()))
    );
    merged
        .current_recordings
        .push(("MERGED-AWAY".into(), MusicBrainzFact::Known(HELP_2009.into())));
    assert_eq!(
        verdict_of(&gate, merged.clone(), Some(HeardMaster::Differs)),
        KEEP
    );
    let mut other = file("another-mix");
    other
        .current_recordings
        .push(("another-mix".into(), MusicBrainzFact::Known("another-mix".into())));
    assert_eq!(
        verdict_of(&gate, other.clone(), Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::Label)
    );
    assert_eq!(
        verdict_of(&gate, other, Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
    let mut unknown = file("no-such-recording");
    unknown
        .current_recordings
        .push(("no-such-recording".into(), MusicBrainzFact::NotKnown));
    assert_eq!(
        verdict_of(&gate, unknown, Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::Label)
    );
    let mut busy = file("merged-away");
    busy.current_recordings
        .push(("merged-away".into(), MusicBrainzFact::Busy));
    assert_eq!(
        gate.judge(&busy, MATCH_SCORE),
        Judgement::Verdict(GateVerdict::Hold(WaitingFor::MusicBrainz))
    );
    let duplicate = HeartGate {
        r: Some(HeardRecording {
            duplicates: vec![HELP_2009.into(), "beyond-the-cap".into()],
            ..help_r()
        }),
        ..r_stays()
    };
    let (verdict, asked) = settle(&duplicate, file("beyond-the-cap"), Some(HeardMaster::Differs));
    assert_eq!(verdict, KEEP);
    assert_eq!(asked, [Need::HeardMaster]);
}

/// With no answer to read, an ISRC label other than R's is asked of MusicBrainz: a lossless copy
/// of the heard master from another release, with that release's own code, still lands; a code
/// naming another song refuses; a busy MusicBrainz holds.
#[test]
fn an_isrc_label_with_no_answer_is_asked_of_musicbrainz() {
    let gate = r_stays();
    let code = "GBUM72306558";
    let file = || with_isrc(AcoustIdOutcome::Busy, code);
    assert_eq!(
        gate.judge(&file(), MATCH_SCORE),
        Judgement::Needs(Need::IsrcRecordings(code.into()))
    );
    let listed = |recordings: Vec<AnswerRecording>| {
        let mut facts = file();
        facts
            .isrc_recordings
            .push((code.into(), MusicBrainzFact::Known(recordings)));
        facts
    };
    let other_release = recording("compilation-help", "Help! (2023 Mix)", BEATLES, &[code], 0);
    let (verdict, asked) = settle(
        &gate,
        listed(vec![other_release.clone()]),
        Some(HeardMaster::Same),
    );
    assert_eq!(verdict, as_heard(Acceptance::HeardMaster));
    assert_eq!(asked, [Need::HeardMaster]);
    assert_eq!(
        verdict_of(&gate, listed(vec![other_release]), Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::Label)
    );
    let another_song = recording("w1", "Wonderwall", OASIS, &[code], 0);
    let (verdict, asked) = settle(&gate, listed(vec![another_song]), None);
    assert_eq!(verdict, GateVerdict::Refuse(Refusal::Label));
    assert!(asked.is_empty());
    let r_own = help(HELP_2009, &[code], 0);
    assert_eq!(
        verdict_of(&gate, listed(vec![r_own]), Some(HeardMaster::Differs)),
        GateVerdict::Hold(WaitingFor::AcoustId)
    );
    let mut busy = file();
    busy.isrc_recordings.push((code.into(), MusicBrainzFact::Busy));
    assert_eq!(
        gate.judge(&busy, MATCH_SCORE),
        Judgement::Verdict(GateVerdict::Hold(WaitingFor::MusicBrainz))
    );
}

/// A file kept as the heard audio or its master claims only the codes the heard answer proved are
/// its recording's own: a code another recording of the answer carries too is never claimed (one
/// code is filed under more than one mix), and with none left it claims no ISRC at all, never R's.
#[test]
fn a_file_kept_as_heard_claims_only_the_codes_its_sound_proved() {
    let shared = "GBXYZ0000001";
    let heard = answer(vec![
        help(HELP_2009, &[ISRC_2009, shared], 50),
        recording("y1", "Yesterday", BEATLES, &[shared], 1),
    ]);
    assert_eq!(heard_codes(&heard, HELP_2009), [ISRC_2009]);
    let only_shared = answer(vec![
        help(HELP_2009, &[shared], 50),
        recording("y1", "Yesterday", BEATLES, &[shared], 1),
    ]);
    assert!(heard_codes(&only_shared, HELP_2009).is_empty());
    let (became_r, tag) = became(&heard, &weighed(&heard, HELP_2009), &[], &help_r(), MATCH_SCORE);
    assert_eq!(tag.as_deref(), Some(HELP_2009));
    assert_eq!(became_r.tag_isrcs, [ISRC_2009]);

    let tagged = HeartGate {
        r: Some(HeardRecording {
            tag_isrcs: Vec::new(),
            ..help_r()
        }),
        ..gate(HeardReading::RStays {
            tag: Some(HELP_2009.into()),
        })
    };
    assert_eq!(
        verdict_of(&tagged, facts(answered(clustered())), Some(HeardMaster::Same)),
        GateVerdict::Accept {
            by: Acceptance::HeardMaster,
            tags: TagClaim::Recording {
                id: HELP_2009.into(),
                isrcs: Vec::new(),
            },
        },
        "R's own ISRC is never claimed for what the sound did not prove"
    );
}

/// The recording of `answer` with `id`, weighed as the answer weighs it.
fn weighed(answer: &AcoustIdAnswer, id: &str) -> WeighedRecording {
    read_answer(answer, &HeardRecording::default(), MATCH_SCORE)
        .recordings
        .into_iter()
        .find(|recording| recording.id == id)
        .expect("the recording is in the answer")
}

/// A label whose question MusicBrainz gave up on (it failed while MusicBrainz answered others)
/// says nothing of the file: a recording id or a code it never answered for refuses no clear file,
/// where a label MusicBrainz knows nothing by still names another recording.
#[test]
fn a_label_musicbrainz_gave_up_on_refuses_nothing() {
    let gate = r_stays();
    let mut by_id = with_recording_id(answered(clear()), "never-answered");
    by_id
        .current_recordings
        .push(("never-answered".into(), MusicBrainzFact::GaveUp));
    let accepted = GateVerdict::Accept {
        by: Acceptance::Sound,
        tags: TagClaim::Recording {
            id: HELP_2009.into(),
            isrcs: vec![ISRC_2009.into()],
        },
    };
    assert_eq!(verdict_of(&gate, by_id, Some(HeardMaster::Differs)), accepted);
    let code = "GBUM72306558";
    let mut by_code = with_isrc(AcoustIdOutcome::Busy, code);
    by_code
        .isrc_recordings
        .push((code.into(), MusicBrainzFact::GaveUp));
    assert_eq!(
        verdict_of(&gate, by_code, Some(HeardMaster::Differs)),
        GateVerdict::Hold(WaitingFor::AcoustId),
        "the file waits for its own answer, never refused by a code nobody answered for"
    );
    let mut known_nothing = with_isrc(AcoustIdOutcome::Busy, code);
    known_nothing
        .isrc_recordings
        .push((code.into(), MusicBrainzFact::NotKnown));
    assert_eq!(
        verdict_of(&gate, known_nothing, Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::Label)
    );
    let mut refused_by_id = with_recording_id(answered(clear()), "no-such-recording");
    refused_by_id
        .current_recordings
        .push(("no-such-recording".into(), MusicBrainzFact::NotKnown));
    assert_eq!(
        verdict_of(&gate, refused_by_id, Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::Label)
    );
}

/// An R built from a reading that was not clear may be another mix than the one heard, so a file
/// reading clear for it is never accepted by sound alone; the same reading against a catalog R is.
#[test]
fn an_r_from_an_unclear_reading_never_accepts_on_a1() {
    let unclear = HeartGate {
        r: Some(HeardRecording {
            from_unclear: true,
            ..help_r()
        }),
        ..gate(HeardReading::RBecame { tag: None })
    };
    assert_eq!(
        verdict_of(&unclear, facts(answered(clear())), Some(HeardMaster::Differs)),
        KEEP
    );
    assert_eq!(
        verdict_of(&unclear, facts(answered(clear())), Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
    assert_eq!(
        verdict_of(&r_stays(), facts(answered(clear())), Some(HeardMaster::Differs)),
        GateVerdict::Accept {
            by: Acceptance::Sound,
            tags: TagClaim::Recording {
                id: HELP_2009.into(),
                isrcs: vec![ISRC_2009.into()],
            },
        }
    );
}

/// A never-played heart is judged by A1 alone: a clear file is accepted with its recording tags
/// and the heard master is never asked for; an ambiguous one holds "not yet proven", even one that
/// claims to be the heard audio.
#[test]
fn a_never_played_heart_is_judged_by_a1_alone() {
    let gate = never_played();
    let (verdict, asked) = settle(&gate, facts(answered(clear())), None);
    assert_eq!(
        verdict,
        GateVerdict::Accept {
            by: Acceptance::Sound,
            tags: TagClaim::Recording {
                id: HELP_2009.into(),
                isrcs: vec![ISRC_2009.into()],
            },
        }
    );
    assert!(asked.is_empty());
    let identical = FileFacts {
        identical: true,
        ..facts(answered(clustered()))
    };
    assert_eq!(verdict_of(&gate, identical, None), NOT_YET);
    assert_eq!(
        verdict_of(&gate, with_isrc(answered(clustered()), ISRC_2015), None),
        GateVerdict::Refuse(Refusal::Label)
    );
}

/// AcoustID never follows MusicBrainz's merges, so a file whose answer names only a merged-away
/// id of R's recording is asked about first, and then passes A1.
#[test]
fn a_merged_away_id_in_the_answer_passes_a1() {
    let gate = never_played();
    let file = || facts(answered(vec![help("merged-away", &[ISRC_2009], 50)]));
    assert_eq!(
        gate.judge(&file(), MATCH_SCORE),
        Judgement::Needs(Need::CurrentRecording("merged-away".into()))
    );
    let mut merged = file();
    merged
        .current_recordings
        .push(("merged-away".into(), MusicBrainzFact::Known(HELP_2009.into())));
    assert_eq!(
        gate.judge(&merged, MATCH_SCORE),
        Judgement::Verdict(GateVerdict::Accept {
            by: Acceptance::Sound,
            tags: TagClaim::Recording {
                id: HELP_2009.into(),
                isrcs: vec![ISRC_2009.into()],
            },
        })
    );
    assert_eq!(
        verdict_of(&gate, file(), None),
        GateVerdict::Refuse(Refusal::AnotherRecording)
    );
    let mut busy = file();
    busy.current_recordings
        .push(("merged-away".into(), MusicBrainzFact::Busy));
    assert_eq!(
        gate.judge(&busy, MATCH_SCORE),
        Judgement::Verdict(GateVerdict::Hold(WaitingFor::MusicBrainz))
    );
}

/// A file whose answer names only another recording of R's song, which MusicBrainz gave up on
/// saying what it is now, is ambiguous, never refused: that recording may still be R's.
#[test]
fn a_recording_of_rs_song_musicbrainz_gave_up_on_reads_ambiguous() {
    let file = || facts(answered(vec![help("never-answered", &[ISRC_2009], 50)]));
    let mut gave_up = file();
    gave_up
        .current_recordings
        .push(("never-answered".into(), MusicBrainzFact::GaveUp));
    assert_eq!(
        never_played().judge(&gave_up, MATCH_SCORE),
        Judgement::Verdict(GateVerdict::Undecided(Undecided::NotYetProven))
    );
    assert_eq!(
        r_stays().judge(&gave_up, MATCH_SCORE),
        Judgement::Needs(Need::HeardMaster),
        "something heard may still pass it as the heard master"
    );
}

/// Proof 2 refuses a file over the line against the heard audio whatever else it is; it never
/// applies when the stream stood in with another song, and for a heart never played only against
/// a stand-in that is its reference.
#[test]
fn proof_two_refuses_only_against_a_reference_that_is_the_song() {
    let at = |distance: f64| FileFacts {
        identical: true,
        heard_distance: Some(distance),
        ..facts(answered(clear()))
    };
    let differs = GateVerdict::Refuse(Refusal::DiffersFromHeard);
    assert_eq!(verdict_of(&r_stays(), at(0.26), None), differs);
    assert_eq!(
        verdict_of(&r_stays(), at(0.25), None),
        as_heard(Acceptance::HeardAudio)
    );
    let another = gate(HeardReading::AnotherSong);
    let (verdict, asked) = settle(&another, at(0.48), None);
    assert_eq!(
        verdict,
        GateVerdict::Accept {
            by: Acceptance::Sound,
            tags: TagClaim::Recording {
                id: HELP_2009.into(),
                isrcs: vec![ISRC_2009.into()],
            },
        }
    );
    assert!(asked.is_empty());
    let stood_in = |is_reference| HeartGate {
        stand_in: Some(StandIn {
            video_id: "pinned".into(),
            sample_id: None,
            is_reference,
        }),
        ..never_played()
    };
    assert_eq!(verdict_of(&stood_in(true), at(0.30), None), differs);
    assert_ne!(verdict_of(&stood_in(false), at(0.30), None), differs);
    assert_eq!(
        verdict_of(
            &gate(HeardReading::Clip(ClipReading::AnotherSongOrTake)),
            at(0.48),
            None
        ),
        GateVerdict::Hold(WaitingFor::AcoustIdOff)
    );
}

/// The heard master is asked for only when it could still accept: never for a file whose reading
/// names another recording. Its busy shim holds, and a heard video that is gone leaves A1.
#[test]
fn the_heard_master_is_asked_only_when_it_could_accept() {
    let gate = r_stays();
    let other = facts(answered(vec![wonderwall(900), help(HELP_2009, &[ISRC_2009], 5)]));
    let (verdict, asked) = settle(&gate, other, None);
    assert_eq!(verdict, GateVerdict::Refuse(Refusal::AnotherRecording));
    assert!(asked.is_empty());
    assert_eq!(
        verdict_of(&gate, facts(answered(clustered())), Some(HeardMaster::Busy)),
        GateVerdict::Hold(WaitingFor::Shim)
    );
    assert_eq!(
        verdict_of(&gate, facts(answered(clustered())), Some(HeardMaster::HeardGone)),
        KEEP
    );
    assert_eq!(
        verdict_of(&gate, facts(answered(clustered())), Some(HeardMaster::Undecided)),
        KEEP
    );
}

/// When nothing proves which recording played, a clear reading accepts nothing: only the heard
/// master or the heard audio.
#[test]
fn only_a0_or_a2_when_nothing_proves_what_played() {
    let gate = gate(HeardReading::OnlyHeardOrMaster);
    assert_eq!(
        verdict_of(&gate, facts(answered(clear())), Some(HeardMaster::Differs)),
        KEEP
    );
    assert_eq!(
        verdict_of(&gate, facts(answered(clear())), Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
    assert_eq!(
        verdict_of(&gate, facts(AcoustIdOutcome::Busy), Some(HeardMaster::Differs)),
        KEEP
    );
}

/// The heard audio and the heard master carry the heard answer's clear recording as their tag,
/// with the codes the heard answer proved are its own.
#[test]
fn a0_and_a2_carry_the_heard_answers_clear_recording() {
    let tagged = HeartGate {
        r: Some(HeardRecording {
            tag_isrcs: vec![ISRC_2009.into()],
            ..help_r()
        }),
        ..gate(HeardReading::RStays {
            tag: Some(HELP_2009.into()),
        })
    };
    let recording_tags = TagClaim::Recording {
        id: HELP_2009.into(),
        isrcs: vec![ISRC_2009.into()],
    };
    let identical = FileFacts {
        identical: true,
        ..facts(AcoustIdOutcome::Busy)
    };
    assert_eq!(
        verdict_of(&tagged, identical, None),
        GateVerdict::Accept {
            by: Acceptance::HeardAudio,
            tags: recording_tags.clone(),
        }
    );
    assert_eq!(
        verdict_of(&tagged, facts(answered(clustered())), Some(HeardMaster::Same)),
        GateVerdict::Accept {
            by: Acceptance::HeardMaster,
            tags: recording_tags,
        }
    );
}

/// A file AcoustID cannot read by its sound is refused, unless it is the heard master.
#[test]
fn an_unreadable_file_is_refused_unless_it_is_the_heard_master() {
    let gate = r_stays();
    let unreadable = || facts(AcoustIdOutcome::Unreadable { code: Some(3) });
    assert_eq!(
        verdict_of(&gate, unreadable(), Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::Unreadable)
    );
    assert_eq!(
        verdict_of(&gate, unreadable(), Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
}

/// An owned copy counts only when the gate accepts it: the owned 2015 mix for a 2009 heart (proof
/// 2 at 0.103, AcoustID clustering the mixes) is never the heart's file, while the library file
/// that played is.
#[test]
fn an_owned_copy_of_another_mix_never_counts() {
    let owned = HeartGate {
        heard: Some(HeardRecord {
            source: HeardSource::Library {
                path: "/music/Help! (2009).flac".into(),
            },
            ..gate(HeardReading::RStays { tag: None }).heard.unwrap()
        }),
        ..r_stays()
    };
    let other_mix = FileFacts {
        heard_distance: Some(0.103),
        ..facts(answered(vec![
            help(HELP_2009, &[ISRC_2009], 690),
            help(HELP_2015, &[ISRC_2015], 310),
        ]))
    };
    assert_eq!(verdict_of(&owned, other_mix, Some(HeardMaster::Differs)), KEEP);
    let the_heard_file = FileFacts {
        identical: is_heard_audio(
            &owned.heard.as_ref().unwrap().source,
            &HeardSource::Library {
                path: "/music/Help! (2009).flac".into(),
            },
        ),
        ..facts(answered(clustered()))
    };
    assert_eq!(
        verdict_of(&owned, the_heard_file, None),
        as_heard(Acceptance::HeardAudio)
    );
}

// ---- The same take ----------------------------------------------------------------------------

/// A file as long as the song in the gate (139 s).
const SONG_SECONDS: f64 = 139.4;

/// Another master of the take a 2009 heart heard: AcoustID clusters the mixes, its print 0.15
/// from the heard print, the song's length, and names that add no version.
fn another_master() -> FileFacts {
    FileFacts {
        heard_distance: Some(0.15),
        seconds: Some(SONG_SECONDS),
        names: FileNames {
            title: Some("Help! (Remastered 2015)".into()),
            album: Some("1 (Remastered)".into()),
            file: Some("07 - The Beatles - Help!".into()),
        },
        ..facts(answered(clustered()))
    }
}

fn same_take() -> GateVerdict {
    as_heard(Acceptance::SameTake)
}

fn named(title: &str, album: &str) -> FileFacts {
    FileFacts {
        names: FileNames {
            title: Some(title.into()),
            album: Some(album.into()),
            file: None,
        },
        ..another_master()
    }
}

/// AcoustID cannot tell the masters apart and the file is not the master heard: another master of
/// the heard take, as long as the song, with no version named and a song not marked explicit, is
/// the same take, and only once the heard master was asked.
#[test]
fn another_master_of_the_heard_take_is_the_same_take() {
    let (verdict, asked) = settle(&r_stays(), another_master(), Some(HeardMaster::Differs));
    assert_eq!(verdict, same_take());
    assert_eq!(asked, [Need::HeardMaster]);
    // Tagged as the heard master is: the heard answer's clear recording, else as heard.
    let tagged = HeartGate {
        r: Some(HeardRecording {
            tag_isrcs: vec![ISRC_2009.into()],
            ..help_r()
        }),
        ..gate(HeardReading::RStays {
            tag: Some(HELP_2009.into()),
        })
    };
    assert_eq!(
        verdict_of(&tagged, another_master(), Some(HeardMaster::Differs)),
        GateVerdict::Accept {
            by: Acceptance::SameTake,
            tags: TagClaim::Recording {
                id: HELP_2009.into(),
                isrcs: vec![ISRC_2009.into()],
            },
        }
    );
    // Nothing proving which recording played, and a song marked neither explicit nor clean.
    assert_eq!(
        verdict_of(
            &gate(HeardReading::OnlyHeardOrMaster),
            another_master(),
            Some(HeardMaster::Differs)
        ),
        same_take()
    );
    let neither = HeartGate {
        r: Some(HeardRecording {
            explicit: Some(explicit_status::NOT_EXPLICIT),
            ..help_r()
        }),
        ..r_stays()
    };
    assert_eq!(
        verdict_of(&neither, another_master(), Some(HeardMaster::Differs)),
        same_take()
    );
    // A heard title that names a version lets a file name it too.
    let live = HeartGate {
        r: Some(HeardRecording {
            title: "Help! (Live)".into(),
            ..help_r()
        }),
        ..r_stays()
    };
    assert_eq!(
        verdict_of(
            &live,
            named("Help! (Live at the Hollywood Bowl)", "Live at the Hollywood Bowl"),
            Some(HeardMaster::Differs)
        ),
        same_take()
    );
}

/// A file AcoustID reads as another recording of the heart's song, or whose own label names one,
/// at 0.15 from the heard print, is the same take; no entry and an unlinked fingerprint too.
#[test]
fn another_recording_of_the_same_song_is_the_same_take() {
    let gate = r_stays();
    let another = FileFacts {
        answer: answered(vec![help(HELP_2015, &[ISRC_2015], 500)]),
        ..another_master()
    };
    let (verdict, asked) = settle(&gate, another, None);
    assert_eq!(verdict, same_take());
    assert_eq!(asked, [Need::CurrentRecording(HELP_2015.into())]);
    let labeled = FileFacts {
        labels: FileLabels {
            isrcs: vec![ISRC_2015.into()],
            recording_id: None,
        },
        ..another_master()
    };
    assert_eq!(
        verdict_of(&gate, labeled, Some(HeardMaster::Differs)),
        same_take()
    );
    let empty = FileFacts {
        answer: AcoustIdOutcome::Answered(AcoustIdAnswer::default()),
        ..another_master()
    };
    assert_eq!(verdict_of(&gate, empty, Some(HeardMaster::Differs)), same_take());
}

/// Over the match line a re-recording (0.27) and a live take (0.41) are refused as before; between
/// it and the same-take line the heard copy is kept.
#[test]
fn a_rerecording_or_a_live_take_is_not_the_same_take() {
    let gate = r_stays();
    assert_eq!(
        verdict_of(&gate, another_master(), Some(HeardMaster::Differs)),
        same_take()
    );
    let at = |distance: f64| FileFacts {
        heard_distance: Some(distance),
        ..another_master()
    };
    for distance in [0.27, 0.41] {
        assert_eq!(
            verdict_of(&gate, at(distance), Some(HeardMaster::Differs)),
            GateVerdict::Refuse(Refusal::DiffersFromHeard),
            "{distance}"
        );
    }
    assert_eq!(
        verdict_of(&gate, at(SAME_TAKE_AT_MOST), Some(HeardMaster::Differs)),
        same_take()
    );
    assert_eq!(verdict_of(&gate, at(0.23), Some(HeardMaster::Differs)), KEEP);
    let no_distance = FileFacts {
        heard_distance: None,
        ..another_master()
    };
    assert_eq!(verdict_of(&gate, no_distance, Some(HeardMaster::Differs)), KEEP);
}

/// A file 30 s off the song's length is an edit, and a length either side does not know proves
/// nothing: the heard copy is kept.
#[test]
fn an_edit_is_not_the_same_take() {
    let gate = r_stays();
    assert_eq!(
        verdict_of(&gate, another_master(), Some(HeardMaster::Differs)),
        same_take()
    );
    let long = |seconds: Option<f64>| FileFacts {
        seconds,
        ..another_master()
    };
    assert_eq!(
        verdict_of(&gate, long(Some(SONG_SECONDS + 30.0)), Some(HeardMaster::Differs)),
        KEEP
    );
    assert_eq!(
        verdict_of(&gate, long(Some(SONG_SECONDS - 30.0)), Some(HeardMaster::Differs)),
        KEEP
    );
    assert_eq!(
        verdict_of(&gate, long(Some(142.5)), Some(HeardMaster::Differs)),
        KEEP
    );
    assert_eq!(
        verdict_of(&gate, long(Some(141.9)), Some(HeardMaster::Differs)),
        same_take()
    );
    assert_eq!(verdict_of(&gate, long(None), Some(HeardMaster::Differs)), KEEP);
    let unknown_song = HeartGate {
        heard_seconds: None,
        ..r_stays()
    };
    assert_eq!(
        verdict_of(&unknown_song, another_master(), Some(HeardMaster::Differs)),
        KEEP
    );
}

/// A song marked explicit, or the clean edit, keeps the exact rule: the two sound alike. A file
/// whose own names say explicit is not the same take of a song not marked so either.
#[test]
fn an_explicit_song_keeps_the_exact_rule() {
    let marked = |explicit: i32| HeartGate {
        r: Some(HeardRecording {
            explicit: Some(explicit),
            ..help_r()
        }),
        ..r_stays()
    };
    assert_eq!(
        verdict_of(&r_stays(), another_master(), Some(HeardMaster::Differs)),
        same_take()
    );
    for explicit in [explicit_status::EXPLICIT, explicit_status::CLEAN] {
        assert_eq!(
            verdict_of(&marked(explicit), another_master(), Some(HeardMaster::Differs)),
            KEEP,
            "{explicit}"
        );
    }
    assert_eq!(
        verdict_of(
            &r_stays(),
            named("Help! (Explicit)", "Help!"),
            Some(HeardMaster::Differs)
        ),
        KEEP
    );
}

/// A file whose title, album or file name names a version the heard title does not is not the
/// same take; a remaster, mono or stereo word names none.
#[test]
fn a_file_naming_another_version_is_not_the_same_take() {
    let gate = r_stays();
    for (title, album) in [
        ("Help! (Live)", "Help!"),
        ("Help! (Instrumental)", "Help!"),
        ("Help! - Acoustic Version", "Help!"),
        ("Help! (Demo)", "Help!"),
        ("Help! (Karaoke Version)", "Help!"),
        ("Help! (Sped Up)", "Help!"),
        ("Help! (Radio Edit)", "Help!"),
        ("Help! (Extended)", "Help!"),
        ("Help! (Remix)", "Help!"),
        ("Help!", "Live at the BBC"),
    ] {
        assert_eq!(
            verdict_of(&gate, named(title, album), Some(HeardMaster::Differs)),
            KEEP,
            "{title} on {album}"
        );
    }
    let file_named = FileFacts {
        names: FileNames {
            title: None,
            album: None,
            file: Some("07 - The Beatles - Help! (Live)".into()),
        },
        ..another_master()
    };
    assert_eq!(verdict_of(&gate, file_named, Some(HeardMaster::Differs)), KEEP);
    for (title, album) in [
        ("Help! - Remastered 2009", "Help! (Remastered)"),
        ("Help! (Mono)", "Help! (Mono)"),
        ("Help! (2015 Stereo Mix)", "1"),
    ] {
        assert_eq!(
            verdict_of(&gate, named(title, album), Some(HeardMaster::Differs)),
            same_take(),
            "{title} on {album}"
        );
    }
}

/// A label naming another song refuses as before, and so does a reading that does: the same take
/// never keeps another song.
#[test]
fn another_song_is_never_the_same_take() {
    let gate = r_stays();
    assert_eq!(
        verdict_of(&gate, another_master(), Some(HeardMaster::Differs)),
        same_take()
    );
    let mut recordings = clustered();
    recordings.push(wonderwall(5));
    let labeled = FileFacts {
        labels: FileLabels {
            isrcs: vec![WONDERWALL_ISRC.into()],
            recording_id: None,
        },
        ..FileFacts {
            answer: answered(recordings),
            ..another_master()
        }
    };
    let (verdict, asked) = settle(&gate, labeled, None);
    assert_eq!(verdict, GateVerdict::Refuse(Refusal::Label));
    assert!(asked.is_empty());
    let read_as_another_song = FileFacts {
        answer: answered(vec![wonderwall(900), help(HELP_2009, &[ISRC_2009], 5)]),
        ..another_master()
    };
    assert_eq!(
        verdict_of(&gate, read_as_another_song, None),
        GateVerdict::Refuse(Refusal::AnotherRecording)
    );
}

/// A0, A2 and A1-clear accept as before, ahead of the same take; with AcoustID off a never-played
/// heart and a heard one are judged as before; and with no copy that passes the heard copy is
/// kept.
#[test]
fn the_exact_acceptances_and_acoustid_off_are_unchanged() {
    let heard = r_stays();
    let identical = FileFacts {
        identical: true,
        ..another_master()
    };
    assert_eq!(
        verdict_of(&heard, identical, None),
        as_heard(Acceptance::HeardAudio)
    );
    assert_eq!(
        verdict_of(&heard, another_master(), Some(HeardMaster::Same)),
        as_heard(Acceptance::HeardMaster)
    );
    let clear_file = FileFacts {
        answer: answered(clear()),
        ..another_master()
    };
    assert_eq!(
        verdict_of(&heard, clear_file, Some(HeardMaster::Differs)),
        GateVerdict::Accept {
            by: Acceptance::Sound,
            tags: TagClaim::Recording {
                id: HELP_2009.into(),
                isrcs: vec![ISRC_2009.into()],
            },
        }
    );
    let off = || FileFacts {
        answer: AcoustIdOutcome::Off,
        ..another_master()
    };
    let (verdict, asked) = settle(&never_played(), off(), None);
    assert_eq!(verdict, NOT_YET);
    assert!(asked.is_empty());
    let stood_in = HeartGate {
        stand_in: Some(StandIn {
            video_id: "pinned".into(),
            sample_id: None,
            is_reference: true,
        }),
        ..never_played()
    };
    assert_eq!(verdict_of(&stood_in, off(), None), NOT_YET);
    assert_eq!(verdict_of(&heard, off(), Some(HeardMaster::Differs)), KEEP);
    assert_eq!(
        verdict_of(
            &gate(HeardReading::Clip(ClipReading::ThisSong)),
            off(),
            Some(HeardMaster::Differs)
        ),
        KEEP
    );
    let busy = FileFacts {
        answer: AcoustIdOutcome::Busy,
        ..another_master()
    };
    assert_eq!(
        verdict_of(&heard, busy, Some(HeardMaster::Differs)),
        GateVerdict::Hold(WaitingFor::AcoustId)
    );
    let unreadable = FileFacts {
        answer: AcoustIdOutcome::Unreadable { code: Some(3) },
        ..another_master()
    };
    assert_eq!(
        verdict_of(&heard, unreadable, Some(HeardMaster::Differs)),
        GateVerdict::Refuse(Refusal::Unreadable)
    );
}

// ---- The same take of a heart never played ----------------------------------------------------

fn stood_in(is_reference: bool) -> HeartGate {
    HeartGate {
        stand_in: Some(StandIn {
            video_id: "pinned".into(),
            sample_id: Some("pinned-sample".into()),
            is_reference,
        }),
        ..never_played()
    }
}

/// Another master of the song for a heart never played, its print `distance` from the stand-in's,
/// as long as the catalog's track, with no version named.
fn near_the_stand_in(distance: f64) -> FileFacts {
    FileFacts {
        heard_distance: Some(distance),
        ..another_master()
    }
}

/// A heart never played whose pinned video is the reference: a file within the same-take line of
/// the stand-in's print, AcoustID unable to tell the masters apart, is the same take; over the
/// match line (0.27) proof 2 refuses it as before, and between the two lines, or with no
/// distance, it is still not yet proven. The stand-in decides alone.
#[test]
fn a_never_played_hearts_file_near_its_reference_stand_in_is_the_same_take() {
    let (verdict, asked) = settle(&stood_in(true), near_the_stand_in(0.15), None);
    assert_eq!(verdict, same_take());
    assert!(asked.is_empty(), "nothing heard, so no heard master: {asked:?}");
    assert_eq!(
        verdict_of(&stood_in(true), near_the_stand_in(0.27), None),
        GateVerdict::Refuse(Refusal::DiffersFromHeard)
    );
    assert_eq!(
        verdict_of(&stood_in(true), near_the_stand_in(0.23), None),
        NOT_YET
    );
    let no_distance = FileFacts {
        heard_distance: None,
        ..another_master()
    };
    assert_eq!(verdict_of(&stood_in(true), no_distance, None), NOT_YET);
    // A stand-in that is not the reference leaves it to the file's own reading.
    assert_eq!(
        verdict_of(&stood_in(false), near_the_stand_in(0.30), None),
        same_take()
    );
}

/// A heart never played with no reference stand-in: a file AcoustID reads ambiguous with one of
/// R's recordings among it, or clear against an R built on a reading that was not clear, is the
/// same take when it is as long as the catalog's track; an album heart's track too. A play with
/// nothing heard is left as it was.
#[test]
fn a_never_played_hearts_ambiguous_reading_holding_r_is_the_same_take() {
    let listed = || FileFacts {
        heard_distance: None,
        ..another_master()
    };
    let (verdict, asked) = settle(&never_played(), listed(), None);
    assert_eq!(verdict, same_take());
    assert!(asked.is_empty());
    let unclear = HeartGate {
        r: Some(HeardRecording {
            from_unclear: true,
            ..help_r()
        }),
        ..never_played()
    };
    let clear_file = FileFacts {
        answer: answered(clear()),
        ..listed()
    };
    assert_eq!(verdict_of(&unclear, clear_file, None), same_take());
    let album = HeartGate {
        origin: AcquisitionOrigin::AlbumHeart,
        ..never_played()
    };
    assert_eq!(verdict_of(&album, listed(), None), same_take());
    let play = HeartGate {
        origin: AcquisitionOrigin::Play,
        ..never_played()
    };
    assert_eq!(verdict_of(&play, listed(), None), NOT_YET);
}

/// A heart never played keeps "not yet proven" for a reading that holds none of R's recordings, a
/// song marked explicit, a version word or a length 30 s off; another song is refused as before,
/// and with AcoustID off nothing changes.
#[test]
fn a_never_played_heart_is_not_yet_proven_without_the_same_take() {
    let listed = || FileFacts {
        heard_distance: None,
        ..another_master()
    };
    assert_eq!(verdict_of(&never_played(), listed(), None), same_take());
    let nothing_named = HeartGate {
        r: Some(HeardRecording {
            ids: Vec::new(),
            duplicates: Vec::new(),
            ..help_r()
        }),
        ..never_played()
    };
    let r = nothing_named.r.clone().unwrap();
    assert_eq!(
        read_answer(&answer(clustered()), &r, MATCH_SCORE).reading,
        Reading::Ambiguous
    );
    assert_eq!(verdict_of(&nothing_named, listed(), None), NOT_YET);
    let explicit = HeartGate {
        r: Some(HeardRecording {
            explicit: Some(explicit_status::EXPLICIT),
            ..help_r()
        }),
        ..never_played()
    };
    assert_eq!(verdict_of(&explicit, listed(), None), NOT_YET);
    let live = FileFacts {
        heard_distance: None,
        ..named("Help! (Live)", "Help!")
    };
    assert_eq!(verdict_of(&never_played(), live, None), NOT_YET);
    let long = FileFacts {
        seconds: Some(SONG_SECONDS + 30.0),
        ..listed()
    };
    assert_eq!(verdict_of(&never_played(), long, None), NOT_YET);
    let another_song = FileFacts {
        answer: answered(vec![wonderwall(900), help(HELP_2009, &[ISRC_2009], 5)]),
        ..listed()
    };
    assert_eq!(
        verdict_of(&never_played(), another_song, None),
        GateVerdict::Refuse(Refusal::AnotherRecording)
    );
    let off = FileFacts {
        answer: AcoustIdOutcome::Off,
        ..listed()
    };
    assert_eq!(verdict_of(&never_played(), off.clone(), None), NOT_YET);
    assert_eq!(verdict_of(&stood_in(true), off, None), NOT_YET);
}

/// The never-played fallback keeps the lone-submission rule: R's recording among the file's
/// candidates backed by two submissions is the same take, by one it is still not yet proven.
#[test]
fn a_never_played_hearts_fallback_needs_two_submissions_for_r() {
    let at = |sources: i64| FileFacts {
        heard_distance: None,
        answer: answered(vec![
            help(HELP_2009, &[ISRC_2009], sources),
            help(HELP_2015, &[ISRC_2015], 1),
        ]),
        ..another_master()
    };
    assert_eq!(verdict_of(&never_played(), at(2), None), same_take());
    assert_eq!(verdict_of(&never_played(), at(1), None), NOT_YET);
    let lone = FileFacts {
        heard_distance: None,
        answer: answered(vec![help(HELP_2009, &[ISRC_2009], 1)]),
        ..another_master()
    };
    assert_eq!(verdict_of(&never_played(), lone, None), NOT_YET);
}

/// R carries the song's explicit mark, and keeps it when it becomes the recording that played.
#[test]
fn r_keeps_the_songs_explicit_mark_when_it_becomes_what_played() {
    let catalog = HeardRecording {
        explicit: Some(explicit_status::EXPLICIT),
        ..help_r()
    };
    let answer = answer(clustered());
    let dominant = weighed(&answer, HELP_2009);
    let (r, _) = became(&answer, &dominant, &[], &catalog, MATCH_SCORE);
    assert_eq!(r.explicit, Some(explicit_status::EXPLICIT));
}
