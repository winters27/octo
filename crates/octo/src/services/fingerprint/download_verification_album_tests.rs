//! A whole album's files filed among its catalog tracks by their own sound: positions never decide,
//! a label is judged only against the track a file is filed against, and a bonus track the catalog
//! album lacks is another recording than every track.

use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, Refusal};

use super::*;
use crate::services::common::album_gates::{AlbumGate, AlbumGateTrack, TrackFiling};
use crate::services::fingerprint::heart_test_support::{GateSetup, gate_rig, sound_bytes, sound_flac};

fn track(id: &str, title: &str, seconds: i32, isrc: &str) -> AlbumGateTrack {
    AlbumGateTrack {
        catalog_track_id: id.into(),
        title: title.into(),
        artist: "The Beatles".into(),
        duration_seconds: Some(seconds),
        r: HeardRecording {
            ids: vec![format!("rec-{id}")],
            duplicates: vec![format!("rec-{id}")],
            isrcs: vec![isrc.into()],
            title: title.into(),
            artist: "The Beatles".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The catalog album: Help!, The Night Before, Yesterday.
fn help_album() -> AlbumGate {
    AlbumGate {
        user: "brandon".into(),
        origin: AcquisitionOrigin::AlbumHeart,
        tracks: vec![
            track("help", "Help!", 139, "GBAYE0601465"),
            track("night", "The Night Before", 154, "GBAYE0601466"),
            track("yesterday", "Yesterday", 125, "GBAYE0601477"),
        ],
    }
}

/// AcoustID's clear answer naming one recording of the Beatles with its own ISRC.
fn clear(id: &str, title: &str, seconds: i32, isrc: &str) -> String {
    format!(
        r#"{{"status":"ok","results":[{{"id":"fp","score":0.97,"recordings":[{{"id":"{id}","title":"{title}","duration":{seconds},"sources":40,"isrcs":["{isrc}"],"artists":[{{"name":"The Beatles"}}]}}]}}]}}"#
    )
}

/// A deluxe edition's import: its files at other positions than the catalog album's, each tagged
/// with its own track's ISRC, and a bonus track the catalog album lacks. Each file is filed as the
/// catalog track its own sound is, none is refused for its position or its label, and the bonus is
/// another recording than every track.
#[tokio::test]
async fn a_deluxe_imports_files_are_filed_by_their_sound_whatever_their_positions() {
    let rig = gate_rig(GateSetup::default()).await;
    rig.answers.answer(
        "yesterday",
        &clear("rec-yesterday", "Yesterday", 125, "GBAYE0601477"),
    );
    rig.answers
        .answer("help", &clear("rec-help", "Help!", 139, "GBAYE0601465"));
    rig.answers.answer(
        "night",
        &clear("rec-night", "The Night Before", 154, "GBAYE0601466"),
    );
    rig.answers.answer(
        "bonus",
        &clear("rec-ticket", "Ticket to Ride", 190, "GBAYE0601468"),
    );
    let import = |position: u32, sound: &str, seconds: u64, isrc: Option<&str>| {
        let path = rig.dir.path().join(format!("{position:02} - track.flac"));
        sound_flac(&path, seconds, &sound_bytes(sound, sound, sound), isrc);
        path
    };
    // The deluxe edition's order: Yesterday first, the bonus second, Help! third, The Night Before
    // last.
    let files = [
        (import(1, "yesterday", 125, Some("GBAYE0601477")), Some(2)),
        (import(2, "bonus", 190, None), None),
        (import(3, "help", 139, Some("GBAYE0601465")), Some(0)),
        (import(4, "night", 154, Some("GBAYE0601466")), Some(1)),
    ];
    let gate = help_album();
    let mut unfilled: Vec<usize> = (0..gate.tracks.len()).collect();

    for (path, filed_as) in files {
        let (filing, verification) = rig
            .service
            .file_album_file(
                &path,
                LabelSource::TheFile,
                &gate,
                &unfilled,
                "soulseek:help-album",
            )
            .await;
        match filed_as {
            Some(track) => {
                assert!(
                    matches!(filing, TrackFiling::Land { track: landed, .. } if landed == track),
                    "{} filed as {track}: {filing:?}",
                    path.display()
                );
                let verification = verification.expect("the verdict it is tagged by");
                assert_eq!(
                    verification.recording_id.as_deref(),
                    Some(gate.tracks[track].r.ids[0].as_str())
                );
                unfilled.retain(|index| *index != track);
            }
            None => assert_eq!(filing, TrackFiling::Refuse(Refusal::AnotherRecording)),
        }
    }
    assert!(unfilled.is_empty(), "every catalog track is filled");
}

/// A file whose sound is one track's, tagged with another track's ISRC: its label names another
/// recording than the track it is filed against, and it is refused.
#[tokio::test]
async fn a_label_naming_another_track_than_the_files_own_sound_refuses_it() {
    let rig = gate_rig(GateSetup::default()).await;
    rig.answers
        .answer("help", &clear("rec-help", "Help!", 139, "GBAYE0601465"));
    let path = rig.dir.path().join("mislabelled.flac");
    sound_flac(
        &path,
        139,
        &sound_bytes("help", "help", "help"),
        Some("GBAYE0601477"),
    );
    let gate = help_album();

    let (filing, _) = rig
        .service
        .file_album_file(
            &path,
            LabelSource::TheFile,
            &gate,
            &[0, 1, 2],
            "soulseek:help-album",
        )
        .await;

    assert_eq!(filing, TrackFiling::Refuse(Refusal::Label));
    assert_eq!(
        rig.answers.asked(),
        ["help"],
        "printed and looked up once for every track"
    );
}
