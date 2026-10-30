//! The album gates' store and the album's filing rule, alone.

use octo_core::fingerprint::verification::{AnswerReading, Reading};

use super::*;

fn track(id: &str, title: &str, seconds: i32, recording: &str) -> AlbumGateTrack {
    AlbumGateTrack {
        catalog_track_id: id.into(),
        title: title.into(),
        artist: "The Beatles".into(),
        duration_seconds: Some(seconds),
        r: HeardRecording {
            ids: vec![recording.into()],
            title: title.into(),
            artist: "The Beatles".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn help_album(user: &str) -> AlbumGate {
    AlbumGate {
        user: user.into(),
        origin: AcquisitionOrigin::AlbumHeart,
        tracks: vec![
            track("help", "Help!", 139, "rec-help"),
            track("night", "The Night Before", 154, "rec-night"),
            track("yesterday", "Yesterday", 125, "rec-yesterday"),
        ],
    }
}

fn reading(reading: Reading) -> Option<AnswerReading> {
    Some(AnswerReading {
        reading,
        recordings: Vec::new(),
        r_share: 0.0,
    })
}

fn check(track: usize, verdict: GateVerdict, read: Option<Reading>, fits: bool) -> TrackCheck {
    TrackCheck {
        track,
        verdict,
        reading: read.and_then(reading),
        no_recording: false,
        fits,
    }
}

fn clear() -> GateVerdict {
    GateVerdict::Accept {
        by: Acceptance::Sound,
        tags: TagClaim::Recording {
            id: "rec".into(),
            isrcs: Vec::new(),
        },
    }
}

fn undecided() -> GateVerdict {
    GateVerdict::Undecided(octo_core::fingerprint::heart_gate::Undecided::NotYetProven)
}

/// Two chains of one album for one person: one ends, and the other's next track is still gated.
#[test]
fn two_chains_of_one_album_keep_its_gate_until_the_last_ends() {
    let gates = Arc::new(AlbumGates::new());
    let first = gates.register("soulseek", "help-album", help_album("brandon"), Vec::new());
    let second = gates.register("soulseek", "help-album", help_album("Brandon"), Vec::new());
    drop(first);
    let next = gates.track_gates("soulseek", "help-album", &["brandon".into()], "night");
    assert_eq!(next.len(), 1, "the second chain's next track is still gated");
    assert_eq!(
        next[0].r.as_ref().map(|r| r.ids.clone()),
        Some(vec!["rec-night".to_string()])
    );
    assert_eq!(next[0].origin, AcquisitionOrigin::AlbumHeart);
    assert_eq!(next[0].reading, HeardReading::NotHeard);
    drop(second);
    assert!(gates.gate("soulseek", "help-album", "brandon").is_none());
    assert!(
        gates
            .track_gates("soulseek", "help-album", &["brandon".into()], "night")
            .is_empty()
    );
}

/// Each person's gate is their own: another person's album heart neither gates nor ends theirs.
#[test]
fn two_peoples_gates_of_one_album_are_their_own() {
    let gates = Arc::new(AlbumGates::new());
    let brandon = gates.register("soulseek", "help-album", help_album("brandon"), Vec::new());
    let anna = gates.register("soulseek", "help-album", help_album("anna"), Vec::new());
    let users = vec!["brandon".to_string(), "anna".to_string()];
    let both = gates.track_gates("soulseek", "help-album", &users, "help");
    let whose: Vec<&str> = both.iter().map(|gate| gate.user.as_str()).collect();
    assert_eq!(whose, ["brandon", "anna"]);
    drop(anna);
    let left = gates.track_gates("soulseek", "help-album", &users, "help");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].user, "brandon");
    let (album, _, found) = gates
        .for_track("soulseek", "yesterday", "BRANDON")
        .expect("listed");
    assert_eq!(album, "help-album");
    assert_eq!(found.title, "Yesterday");
    drop(brandon);
}

/// A track a walk held is taken once by its chain, and no later walk of the album fetches it; a
/// track held before the chain began is left alone too.
#[test]
fn held_tracks_are_taken_once_and_left_alone_by_later_walks() {
    let gates = Arc::new(AlbumGates::new());
    let _hold = gates.register(
        "soulseek",
        "help-album",
        help_album("brandon"),
        vec!["yesterday".to_string()],
    );
    let users = vec!["brandon".to_string()];
    let held = AcquisitionHeld {
        waiting_for: WaitingFor::NotYetProven,
        line: tracks_wait_line(1),
        files: vec!["/held/help.flac".into()],
        users: Vec::new(),
    };
    gates.held("soulseek", "help-album", &users, "help", held.clone());
    assert!(gates.settled("soulseek", "help-album", &users, "help"));
    assert!(gates.settled("soulseek", "help-album", &users, "yesterday"));
    assert!(!gates.settled("soulseek", "help-album", &users, "night"));
    assert_eq!(
        gates.take_held("soulseek", "help-album", "brandon"),
        [("help".to_string(), held)]
    );
    assert!(gates.take_held("soulseek", "help-album", "brandon").is_empty());
    assert!(
        !gates.settled("soulseek", "other-album", &users, "help"),
        "no gate, nothing settled"
    );
}

#[test]
fn the_album_line_says_one_track_or_many() {
    assert_eq!(
        tracks_wait_line(1),
        "1 track waits: play it once, or keep it anyway"
    );
    assert_eq!(
        tracks_wait_line(3),
        "3 tracks wait: play them once, or keep them anyway"
    );
}

#[test]
fn a_file_fits_a_track_by_its_core_title_and_length() {
    let help = track("help", "Help!", 139, "rec-help");
    assert!(fits(&help, Some("Help! (Remastered 2009)"), Some(141)));
    assert!(fits(&help, Some("help"), Some(146)), "within 7 s");
    assert!(!fits(&help, Some("Help!"), Some(150)), "11 s off");
    assert!(!fits(&help, Some("Yesterday"), Some(139)));
    assert!(!fits(&help, None, Some(139)), "no title tag fits nothing");
    assert!(!fits(&help, Some("Help!"), None), "no length fits nothing");
    let unknown_length = AlbumGateTrack {
        duration_seconds: None,
        ..help
    };
    assert!(fits(&unknown_length, Some("Help!"), Some(500)));
}

/// A clear reading against a track's recording lands the file as that track, whatever position
/// it came at; a rightly tagged track's label refuses it for every other track, never for its
/// own.
#[test]
fn a_file_is_filed_by_its_sound_and_its_label_judged_against_its_own_track_only() {
    let tagged_three = [
        check(
            0,
            GateVerdict::Refuse(Refusal::Label),
            Some(Reading::Another),
            false,
        ),
        check(
            1,
            GateVerdict::Refuse(Refusal::Label),
            Some(Reading::Another),
            false,
        ),
        check(2, clear(), Some(Reading::Clear), true),
    ];
    assert!(matches!(
        file_among(&tagged_three),
        TrackFiling::Land { track: 2, .. }
    ));
    // Two tracks sharing a recording fill in catalog order.
    let shared = [
        check(0, clear(), Some(Reading::Clear), true),
        check(1, clear(), Some(Reading::Clear), true),
    ];
    assert!(matches!(file_among(&shared), TrackFiling::Land { track: 0, .. }));
}

#[test]
fn another_recording_than_every_track_is_refused() {
    let another = [
        check(
            0,
            GateVerdict::Refuse(Refusal::AnotherRecording),
            Some(Reading::Another),
            true,
        ),
        check(
            1,
            GateVerdict::Refuse(Refusal::AnotherRecording),
            Some(Reading::Another),
            false,
        ),
    ];
    assert_eq!(
        file_among(&another),
        TrackFiling::Refuse(Refusal::AnotherRecording)
    );
}

/// An ambiguous file is held tied to its track, whatever its title tag says; an empty or unknown
/// one is held only when its own title tag and length fit a track, else refused.
#[test]
fn ambiguous_and_fitting_unknown_files_hold_and_others_are_refused() {
    let ambiguous = check(1, undecided(), Some(Reading::Ambiguous), false);
    assert_eq!(
        file_among(&[
            check(
                0,
                GateVerdict::Refuse(Refusal::AnotherRecording),
                Some(Reading::Another),
                false
            ),
            ambiguous
        ]),
        TrackFiling::Hold {
            track: 1,
            waiting_for: WaitingFor::NotYetProven
        }
    );
    for (refusal, read) in [
        (Refusal::NoEntry, Reading::Empty),
        (Refusal::Unknown, Reading::Unknown),
    ] {
        assert_eq!(
            album_verdict(check(0, GateVerdict::Refuse(refusal), Some(read), true)),
            GateVerdict::Hold(WaitingFor::NotYetProven),
            "{read:?} fitting the track"
        );
        assert_eq!(
            album_verdict(check(0, GateVerdict::Refuse(refusal), Some(read), false)),
            GateVerdict::Refuse(refusal),
            "{read:?} fitting nothing"
        );
    }
}

/// With AcoustID off nothing reads the file: it is unknown, held when it fits its track.
#[test]
fn with_acoust_id_off_every_track_file_is_unknown() {
    assert_eq!(
        album_verdict(check(0, undecided(), None, true)),
        GateVerdict::Hold(WaitingFor::NotYetProven)
    );
    assert_eq!(
        album_verdict(check(0, undecided(), None, false)),
        GateVerdict::Refuse(Refusal::Unknown)
    );
}

/// A label refuses a file it could be held for only when it names another recording than every
/// track the file fits.
#[test]
fn a_label_refuses_a_held_candidate_only_against_every_track_it_fits() {
    let both = [
        check(
            0,
            GateVerdict::Refuse(Refusal::Label),
            Some(Reading::Ambiguous),
            true,
        ),
        check(1, undecided(), Some(Reading::Ambiguous), true),
    ];
    assert_eq!(
        file_among(&both),
        TrackFiling::Hold {
            track: 1,
            waiting_for: WaitingFor::NotYetProven
        }
    );
    let only = [check(
        0,
        GateVerdict::Refuse(Refusal::Label),
        Some(Reading::Ambiguous),
        true,
    )];
    assert_eq!(file_among(&only), TrackFiling::Refuse(Refusal::Label));
}

/// A held track whose file a person's play shows is not what played (proof 2), or a file AcoustID
/// cannot read, is refused; a gate holding for an answer holds the file for it.
#[test]
fn proof_two_and_unreadable_refuse_and_a_busy_answer_holds() {
    assert_eq!(
        album_verdict(check(
            0,
            GateVerdict::Refuse(Refusal::DiffersFromHeard),
            Some(Reading::Ambiguous),
            true
        )),
        GateVerdict::Refuse(Refusal::DiffersFromHeard)
    );
    assert_eq!(
        album_verdict(check(0, GateVerdict::Refuse(Refusal::Unreadable), None, true)),
        GateVerdict::Refuse(Refusal::Unreadable)
    );
    assert_eq!(
        album_verdict(check(0, GateVerdict::Hold(WaitingFor::AcoustId), None, false)),
        GateVerdict::Hold(WaitingFor::AcoustId)
    );
    // A heard master that differs leaves an ambiguous file held: an A2 fail is undecided.
    assert_eq!(
        album_verdict(check(
            0,
            GateVerdict::Undecided(octo_core::fingerprint::heart_gate::Undecided::KeepHeardCopy),
            Some(Reading::Ambiguous),
            false
        )),
        GateVerdict::Hold(WaitingFor::NotYetProven)
    );
    // Only AcoustID's "another recording" refuses it then, even when nothing proves which
    // recording the play was.
    assert_eq!(
        album_verdict(check(
            0,
            GateVerdict::Undecided(octo_core::fingerprint::heart_gate::Undecided::KeepHeardCopy),
            Some(Reading::Another),
            true
        )),
        GateVerdict::Refuse(Refusal::AnotherRecording)
    );
}

/// A track with no recording to prove (the album known only by its name) holds a fitting file
/// whatever its reading, and is never proven by a reading against nothing.
#[test]
fn a_track_with_no_recording_holds_a_fitting_file_whatever_it_reads() {
    let no_recording = |verdict: GateVerdict, read: Reading, fits: bool| TrackCheck {
        no_recording: true,
        ..check(0, verdict, Some(read), fits)
    };
    assert_eq!(
        album_verdict(no_recording(clear(), Reading::Clear, true)),
        GateVerdict::Hold(WaitingFor::NotYetProven)
    );
    assert_eq!(
        album_verdict(no_recording(
            GateVerdict::Refuse(Refusal::AnotherRecording),
            Reading::Another,
            true
        )),
        GateVerdict::Hold(WaitingFor::NotYetProven)
    );
    assert!(matches!(
        album_verdict(no_recording(
            GateVerdict::Refuse(Refusal::AnotherRecording),
            Reading::Another,
            false
        )),
        GateVerdict::Refuse(_)
    ));
}
