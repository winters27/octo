//! The heart gate in Octo, end to end over its own fetches and lookups: a never-played heart's
//! stand-in fetched from its pinned video, a short heard print topped up before proof 2 runs, the
//! shim holding a heart, proof 2 refusing what AcoustID or nothing would let through, a label
//! checked through MusicBrainz while AcoustID is busy, the catalog's clip deciding with AcoustID
//! off, and a label question MusicBrainz did not answer asked again until it resumes the hold.

use std::sync::Arc;
use std::time::Duration;

use octo_core::fingerprint::heart_gate::{Acceptance, Refusal};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::services::common::{HeldAcquisition, HeldKind, acquisition_key};
use crate::services::fingerprint::heart_test_support::{
    GateRig, GateSetup, flac_header, gate_fixture, gate_rig, print_items, sound_bytes, sound_flac,
};
use crate::services::prefetch::heard_samples::HeardPrints;
use crate::services::prefetch::heard_samples::tests::items_of;

const VIDEO: &str = "AAAAAAAAAA1";
const BEATLES: &str = "b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d";
const HELP_2009: &str = "4416d38e-0f68-4c75-93b9-c1a793fccb49";
const HELP_2009_TWIN: &str = "be4ce82b-1f7a-4f41-a7c3-0d6b6e1a2a01";
const HELP_2015: &str = "01b6e4de-ff48-4ec0-862e-cd623c924d7b";
const HELP_CODE: &str = "GBAYE0601465";

fn help(duration: Option<i32>, isrc: Option<&str>) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("Help!".into()),
        duration,
        isrc: isrc.map(str::to_string),
        you_tube_id: Some(VIDEO.into()),
        ..Default::default()
    }
}

/// An AcoustID answer naming each (id, title, artist, length, submissions, ISRC).
fn answer(recordings: &[(&str, &str, &str, i32, i32, &str)]) -> String {
    let recordings: Vec<serde_json::Value> = recordings
        .iter()
        .map(|(id, title, artist, seconds, sources, isrc)| {
            serde_json::json!({
                "id": id, "title": title, "duration": seconds, "sources": sources,
                "artists": [{"id": format!("{artist}-id"), "name": artist}],
                "isrcs": [isrc],
            })
        })
        .collect();
    serde_json::json!({"status": "ok", "results": [{"id": "fp", "score": 0.97, "recordings": recordings}]})
        .to_string()
}

/// `brandon` heard `video`, whose stored print is `raw` (`whole` when it covers the song).
fn heard_with(rig: &GateRig, video: &str, raw: Vec<u32>, whole: bool, compressed: &str) -> HeardRecord {
    let source = HeardSource::Video {
        id: video.into(),
        size: Some(4_000_000),
        content_type: "audio/mp4".into(),
        itag: None,
    };
    let sample_id = sample_id_of(&source);
    rig.rig.samples.store(
        &sample_id,
        &HeardPrints {
            raw,
            compressed: compressed.into(),
            seconds: 240,
            whole,
            reaches_end: false,
        },
        Some(video),
    );
    HeardRecord {
        heard_at: Utc::now(),
        source,
        sample_id: Some(sample_id),
    }
}

async fn build(rig: &GateRig, id: &str, heard: Option<HeardRecord>) -> GateBuild {
    rig.service
        .build_heart_gate(
            "soulseek",
            id,
            "brandon",
            AcquisitionOrigin::Heart,
            heard,
            &mut ClipPrints::default(),
        )
        .await
}

fn waits(built: &GateBuild) -> Option<(WaitingFor, String)> {
    match built {
        GateBuild::Hold {
            waiting_for, line, ..
        } => Some((*waiting_for, line.clone())),
        GateBuild::Ready(_) | GateBuild::End { .. } => None,
    }
}

fn gate_of(built: GateBuild) -> HeartGate {
    match built {
        GateBuild::Ready(gate) | GateBuild::Hold { gate, .. } | GateBuild::End { gate, .. } => gate,
    }
}

/// A file in the rig's folder whose sound is named, with `isrc` as its own tag.
fn file(
    rig: &GateRig,
    name: &str,
    print: &str,
    master: &str,
    answer: &str,
    isrc: Option<&str>,
) -> std::path::PathBuf {
    let path = rig.dir.path().join(format!("{name}.flac"));
    sound_flac(&path, 200, &sound_bytes(print, master, answer), isrc);
    path
}

fn candidate(id: &str) -> HeartCandidate {
    HeartCandidate {
        labels: LabelSource::TheFile,
        identity: None,
        acquisition_key: acquisition_key("soulseek", id, false),
    }
}

// ---- A heart never played: the pinned video's stand-in ----------------------------------------

/// A search heart nobody played has no catalog R: Octo fetches the routing's pinned video once,
/// prints it, and the one recording its answer gives that fits the song's length becomes R. The
/// stand-in was not heard: the gate reads nothing heard, and its print is what proof 2 compares
/// with, so a file AcoustID reads clear for R is kept only when it sounds like it.
#[tokio::test]
async fn a_search_heart_never_played_takes_r_from_its_pinned_videos_stand_in() {
    let rig = gate_rig(GateSetup {
        music_brainz: false,
        ..Default::default()
    })
    .await;
    rig.shim_streams(VIDEO, sound_bytes("stand-in", "stand-in-master", "stand-in"))
        .await;
    let clear_2015 = answer(&[(HELP_2015, "Help!", "The Beatles", 139, 40, "GBUM71505900")]);
    rig.answers.answer("stand-in", &clear_2015);
    let id = rig.rig.registry.register(help(Some(139), None));

    let GateBuild::Ready(gate) = build(&rig, &id, None).await else {
        panic!("the stand-in gave R");
    };
    assert_eq!(
        rig.streams_of(VIDEO).await,
        1,
        "the pinned video was fetched once"
    );
    assert_eq!(gate.r.as_ref().expect("R").ids, [HELP_2015]);
    assert_eq!(gate.reading, HeardReading::NotHeard, "a stand-in is never heard");
    assert!(gate.heard.is_none());
    let stand_in = gate.stand_in.clone().expect("the stand-in");
    assert_eq!(stand_in.video_id, VIDEO);
    assert!(stand_in.is_reference);

    rig.answers.answer("file", &clear_2015);
    let like_it = file(&rig, "like", "stand-in", "a", "file", None);
    let check = rig
        .service
        .verify_for_heart(&like_it, &candidate(&id), &gate)
        .await;
    assert!(
        matches!(
            check.verdict,
            GateVerdict::Accept {
                by: Acceptance::Sound,
                ..
            }
        ),
        "{:?}",
        check.verdict
    );
    let unlike = file(&rig, "unlike", "another-take", "b", "file", None);
    let check = rig
        .service
        .verify_for_heart(&unlike, &candidate(&id), &gate)
        .await;
    assert_eq!(check.verdict, GateVerdict::Refuse(Refusal::DiffersFromHeard));
}

/// A search heart whose stand-in names nothing that fits the song's length, or that AcoustID does
/// not know, has no R: it downloads nothing and holds until the person plays it once.
#[tokio::test]
async fn a_search_hearts_stand_in_with_nothing_that_fits_holds_until_a_play() {
    for (body, why) in [
        (
            answer(&[(HELP_2015, "Help!", "The Beatles", 300, 40, "GBUM71505900")]),
            "another length",
        ),
        (r#"{"status":"ok","results":[]}"#.to_string(), "no entry"),
    ] {
        let rig = gate_rig(GateSetup {
            music_brainz: false,
            ..Default::default()
        })
        .await;
        rig.shim_streams(VIDEO, sound_bytes("stand-in", "m", "stand-in"))
            .await;
        rig.answers.answer("stand-in", &body);
        let id = rig.rig.registry.register(help(Some(139), None));
        let built = build(&rig, &id, None).await;
        assert_eq!(
            waits(&built),
            Some((WaitingFor::NotYetProven, NEVER_PLAYED.to_string())),
            "{why}"
        );
        assert_eq!(rig.streams_of(VIDEO).await, 1, "{why}: the stand-in was fetched");
        assert_eq!(rig.answers.asked(), ["stand-in"], "{why}");
    }
}

/// A list heart's catalog R (here the song's code's two recordings) is only narrowed by its
/// stand-in, to the one recording of R its answer gives; a stand-in that names another recording
/// of the song leaves R as it is and is no reference for proof 2. R never becomes what a stand-in
/// played.
#[tokio::test]
async fn a_list_hearts_stand_in_only_narrows_its_catalog_r() {
    let listed: Vec<serde_json::Value> = [HELP_2009, HELP_2009_TWIN]
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id, "title": "Help!", "length": 139_000, "disambiguation": "", "isrcs": [],
                "first-release-date": "1965-07-19",
                "artist-credit": [{"name": "The Beatles", "artist": {"id": BEATLES, "name": "The Beatles"}}],
            })
        })
        .collect();
    let code = serde_json::json!({ "recordings": listed }).to_string();
    for (named, narrowed, reference) in [
        (HELP_2009, vec![HELP_2009], true),
        (HELP_2015, vec![HELP_2009, HELP_2009_TWIN], false),
    ] {
        let rig = gate_rig(GateSetup::default()).await;
        rig.music_brainz().code(HELP_CODE, &code);
        rig.shim_streams(VIDEO, sound_bytes("stand-in", "m", "stand-in"))
            .await;
        rig.answers.answer(
            "stand-in",
            &answer(&[(named, "Help!", "The Beatles", 139, 40, HELP_CODE)]),
        );
        let id = rig.rig.registry.register(help(Some(139), Some(HELP_CODE)));
        let GateBuild::Ready(gate) = build(&rig, &id, None).await else {
            panic!("a list heart with R is ready");
        };
        let r = gate.r.expect("R");
        assert_eq!(r.ids, narrowed, "a stand-in naming {named}");
        assert_eq!(
            gate.stand_in.expect("the stand-in").is_reference,
            reference,
            "{named}"
        );
        assert_eq!(rig.streams_of(VIDEO).await, 1);
    }
}

// ---- What was heard: the top-up, the shim, proof 2 ---------------------------------------------

/// A heard sample of 64 KB gives a print of a few seconds: the gate tops it up with one fetch of
/// the heard video, reads the whole print's answer, and proof 2 then runs against that whole print,
/// refusing a file AcoustID reads clear but that does not sound like what played.
#[tokio::test]
async fn a_64_kb_heard_print_is_topped_up_and_proof_2_runs_against_the_whole_print() {
    let rig = gate_rig(GateSetup {
        music_brainz: false,
        ..Default::default()
    })
    .await;
    let short = print_items("heard-mix", 4.0);
    let record = heard_with(&rig, VIDEO, short, false, "short");
    rig.shim_streams(VIDEO, sound_bytes("heard-mix", "heard-master", "heard-whole"))
        .await;
    let clear_2015 = answer(&[(HELP_2015, "Help!", "The Beatles", 139, 40, "GBUM71505900")]);
    rig.answers.answer("heard-whole", &clear_2015);
    let id = rig.rig.registry.register(help(Some(139), None));

    let GateBuild::Ready(gate) = build(&rig, &id, Some(record.clone())).await else {
        panic!("the topped-up print was read");
    };
    assert_eq!(rig.streams_of(VIDEO).await, 1, "one fetch tops it up");
    assert_eq!(
        rig.answers.asked(),
        ["heard-whole"],
        "the whole print was looked up"
    );
    let kept = rig
        .rig
        .samples
        .prints(record.sample_id.as_deref().expect("its sample"))
        .expect("kept");
    assert!(kept.whole);
    assert_eq!(kept.raw, print_items("heard-mix", 240.0));
    assert_eq!(gate.r.as_ref().expect("R").ids, [HELP_2015]);

    rig.answers.answer("file", &clear_2015);
    let heard_mix = file(&rig, "heard", "heard-mix", "heard-master", "file", None);
    let check = rig
        .service
        .verify_for_heart(&heard_mix, &candidate(&id), &gate)
        .await;
    assert!(
        matches!(check.verdict, GateVerdict::Accept { .. }),
        "{:?}",
        check.verdict
    );
    let other_mix = file(&rig, "other", "other-mix", "other-master", "file", None);
    let check = rig
        .service
        .verify_for_heart(&other_mix, &candidate(&id), &gate)
        .await;
    assert_eq!(check.verdict, GateVerdict::Refuse(Refusal::DiffersFromHeard));
}

/// A shim that cannot fetch the heard video now holds the heart for the shim, before anything is
/// looked up.
#[tokio::test]
async fn a_transient_shim_failure_holds_the_heart_for_the_shim() {
    let rig = gate_rig(GateSetup {
        music_brainz: false,
        ..Default::default()
    })
    .await;
    Mock::given(path("/stream"))
        .respond_with(
            ResponseTemplate::new(503).set_body_raw(r#"{"error":"busy","fresh":true}"#, "application/json"),
        )
        .mount(&rig.shim)
        .await;
    let record = heard_with(&rig, VIDEO, print_items("heard", 4.0), false, "short");
    let id = rig.rig.registry.register(help(Some(139), None));
    let built = build(&rig, &id, Some(record)).await;
    assert_eq!(
        waits(&built),
        Some((
            WaitingFor::Shim,
            crate::services::common::heart_acquisition_coordinator::waiting_line(WaitingFor::Shim)
                .to_string()
        ))
    );
    assert!(rig.answers.asked().is_empty(), "nothing was looked up");
}

/// A gate whose R stays as the catalog's, for `brandon`, who heard `record`.
fn heard_gate(r: HeardRecording, record: HeardRecord, reading: HeardReading) -> HeartGate {
    HeartGate {
        user: "brandon".into(),
        origin: AcquisitionOrigin::Heart,
        r: Some(r),
        heard: Some(record),
        heard_seconds: None,
        reading,
        stand_in: None,
    }
}

fn r_of(id: &str, isrc: &str, title: &str, artist: &str) -> HeardRecording {
    HeardRecording {
        ids: vec![id.into()],
        duplicates: vec![id.into()],
        isrcs: vec![isrc.into()],
        title: title.into(),
        artist: artist.into(),
        ..Default::default()
    }
}

fn frozen_print(name: &str) -> Vec<u32> {
    items_of(&gate_fixture(&format!("prints/{name}.json")))
}

/// Taylor's Version for a 2008 heart is refused by proof 2 even when AcoustID wrongly reads it
/// clear for the 2008 recording.
#[tokio::test]
async fn taylors_version_is_refused_by_proof_2_even_when_acoust_id_names_2008() {
    let rig = gate_rig(GateSetup::default()).await;
    let r = r_of(
        "9086b742-358b-4f73-9a14-84cb1a9ce4ce",
        "USCJY0803275",
        "Love Story",
        "Taylor Swift",
    );
    let record = heard_with(&rig, VIDEO, frozen_print("lovestory-2008-240"), true, "heard");
    rig.shim_streams(VIDEO, sound_bytes("-", "lovestory-2008", "-"))
        .await;
    let gate = heard_gate(r, record, HeardReading::RStays { tag: None });
    rig.printer
        .register("taylors-version", frozen_print("lovestory-tv-120"));
    rig.answers.answer(
        "wrongly-2008",
        &answer(&[(
            "9086b742-358b-4f73-9a14-84cb1a9ce4ce",
            "Love Story",
            "Taylor Swift",
            235,
            60,
            "USCJY0803275",
        )]),
    );
    let tv = file(
        &rig,
        "tv",
        "taylors-version",
        "lovestory-tv",
        "wrongly-2008",
        None,
    );
    let check = rig.service.verify_for_heart(&tv, &candidate("x"), &gate).await;
    assert_eq!(check.verdict, GateVerdict::Refuse(Refusal::DiffersFromHeard));
    assert_eq!(rig.answers.asked(), ["wrongly-2008"]);
}

/// The Central Park take for a studio heart is refused by proof 2 with AcoustID off: no lookup is
/// sent, and the take is never kept.
#[tokio::test]
async fn the_central_park_take_is_refused_by_proof_2_with_acoust_id_off() {
    let rig = gate_rig(GateSetup {
        acoust_id_off: true,
        ..Default::default()
    })
    .await;
    let r = r_of(
        "df569676-9c08-4bec-8137-daa122c04ca9",
        "USSM16800249",
        "Mrs. Robinson",
        "Simon & Garfunkel",
    );
    let record = heard_with(&rig, VIDEO, frozen_print("mrsrobinson-studio-240"), true, "heard");
    rig.shim_streams(VIDEO, sound_bytes("-", "studio", "-")).await;
    let gate = heard_gate(r, record, HeardReading::Clip(ClipReading::ThisSong));
    rig.printer
        .register("central-park", frozen_print("mrsrobinson-live-120"));
    let live = file(&rig, "live", "central-park", "live", "central-park", None);
    let check = rig.service.verify_for_heart(&live, &candidate("x"), &gate).await;
    assert_eq!(check.verdict, GateVerdict::Refuse(Refusal::DiffersFromHeard));
    assert!(rig.answers.asked().is_empty(), "AcoustID is off");
}

// ---- A label checked by MusicBrainz -----------------------------------------------------------

/// MusicBrainz's code lookup listing one recording of `title` by `artist`.
fn code_of(id: &str, title: &str, artist: &str, artist_id: &str) -> String {
    serde_json::json!({"recordings": [{
        "id": id, "title": title, "length": 139_000, "disambiguation": "", "isrcs": [],
        "artist-credit": [{"name": artist, "artist": {"id": artist_id, "name": artist}}],
    }]})
    .to_string()
}

/// Help! 2009, heard by `brandon`: the heard answer was read, R stays, and the heard master is what
/// the shim sends again.
fn help_heard(rig: &GateRig) -> HeartGate {
    let record = heard_with(rig, VIDEO, print_items("help-2009", 240.0), true, "heard");
    heard_gate(
        r_of(HELP_2009, HELP_CODE, "Help!", "The Beatles"),
        record,
        HeardReading::RStays { tag: None },
    )
}

/// Prints nothing, as an fpcalc that is missing or runs out of time.
struct NoPrints;

#[async_trait::async_trait]
impl crate::services::prefetch::heard_samples::Printer for NoPrints {
    async fn raw(&self, _: &std::path::Path, _: i32, _: i32) -> Option<octo_media::audio::RawFingerprint> {
        None
    }

    async fn compressed(&self, _: &std::path::Path, _: i32, _: i32) -> Option<(String, i32)> {
        None
    }
}

/// A file fpcalc could not print here is never refused for it: AcoustID was never asked, so the
/// file is judged as with AcoustID off. Another master is not kept and is no refusal; the master
/// that played is kept as heard.
#[tokio::test]
async fn a_file_fpcalc_could_not_print_is_judged_without_acoust_id() {
    let rig = gate_rig(GateSetup::default()).await;
    rig.shim_streams(VIDEO, sound_bytes("-", "help-2009-master", "-"))
        .await;
    let gate = help_heard(&rig);
    rig.rig.samples.use_printer(Arc::new(NoPrints));
    let other = file(&rig, "other", "help-2009", "another-master", "help", None);
    let check = rig.service.verify_for_heart(&other, &candidate("x"), &gate).await;
    assert_eq!(
        check.verdict,
        GateVerdict::Undecided(octo_core::fingerprint::heart_gate::Undecided::KeepHeardCopy)
    );
    let master = file(&rig, "master", "help-2009", "help-2009-master", "help", None);
    let check = rig
        .service
        .verify_for_heart(&master, &candidate("x"), &gate)
        .await;
    assert!(
        matches!(
            check.verdict,
            GateVerdict::Accept {
                by: Acceptance::HeardMaster,
                ..
            }
        ),
        "{:?}",
        check.verdict
    );
    assert!(rig.answers.asked().is_empty(), "nothing was sent to AcoustID");
}

/// Octo's own file check reads a file's length and its names for the same take: another master
/// of the take heard, which AcoustID cannot tell from the other mix, is kept as the same take
/// when it is as long as the song; named live, or 30 s off the song's length, the heard copy is
/// kept instead.
#[tokio::test]
async fn another_master_as_long_as_the_song_is_kept_as_the_same_take() {
    let rig = gate_rig(GateSetup::default()).await;
    rig.shim_streams(VIDEO, sound_bytes("-", "help-2009-master", "-"))
        .await;
    rig.answers.answer(
        "clustered",
        &answer(&[
            (HELP_2009, "Help!", "The Beatles", 139, 700, HELP_CODE),
            (HELP_2015, "Help!", "The Beatles", 139, 300, "GBUM71505900"),
        ]),
    );
    let lasting = |seconds: i32| HeartGate {
        heard_seconds: Some(seconds),
        ..help_heard(&rig)
    };
    let keep = GateVerdict::Undecided(octo_core::fingerprint::heart_gate::Undecided::KeepHeardCopy);
    // Audio after the header, so the file's container gives its 200 s.
    let lasting_file = |name: &str| {
        let path = rig.dir.path().join(format!("{name}.flac"));
        let mut bytes = flac_header(200);
        bytes.extend_from_slice(&sound_bytes("help-2009", "another-master", "clustered"));
        bytes.extend(std::iter::repeat_n(0u8, 256 * 1024));
        std::fs::write(&path, bytes).expect("the file is written");
        path
    };

    let other = lasting_file("Help!");
    let check = rig
        .service
        .verify_for_heart(&other, &candidate("help"), &lasting(200))
        .await;
    assert_eq!(
        check.verdict,
        GateVerdict::Accept {
            by: Acceptance::SameTake,
            tags: TagClaim::AsHeard,
        }
    );
    let check = rig
        .service
        .verify_for_heart(&other, &candidate("help"), &lasting(170))
        .await;
    assert_eq!(check.verdict, keep);
    let live = lasting_file("Help! (Live)");
    let check = rig
        .service
        .verify_for_heart(&live, &candidate("help"), &lasting(200))
        .await;
    assert_eq!(check.verdict, keep);
}

/// With AcoustID busy for the file itself, a file that is the very master that played (A2) lands
/// when its own ISRC is one MusicBrainz lists on another release of the same song, and is refused
/// when its ISRC names another song.
#[tokio::test]
async fn with_acoust_id_busy_an_a2_passing_file_from_another_release_lands_and_one_naming_another_song_is_refused()
 {
    let rig = gate_rig(GateSetup::default()).await;
    rig.shim_streams(VIDEO, sound_bytes("-", "help-2009-master", "-"))
        .await;
    rig.answers.status("busy", 503);
    rig.music_brainz().code(
        "GBXYZ1500001",
        &code_of(
            "6c3e0c2f-5b3a-4b8f-9a55-7f1e2c3d4a01",
            "Help!",
            "The Beatles",
            BEATLES,
        ),
    );
    rig.music_brainz().code(
        "GBXYZ9500002",
        &code_of(
            "7d4f1d3a-6c4b-4c9a-8b66-8a2f3d4e5b02",
            "Wonderwall",
            "Oasis",
            "39ab1aed-75e0-4140-bd47-540276886b60",
        ),
    );
    let gate = help_heard(&rig);

    let reissue = file(
        &rig,
        "reissue",
        "help-2009",
        "help-2009-master",
        "busy",
        Some("GBXYZ1500001"),
    );
    let check = rig
        .service
        .verify_for_heart(&reissue, &candidate("help"), &gate)
        .await;
    assert!(
        matches!(
            check.verdict,
            GateVerdict::Accept {
                by: Acceptance::HeardMaster,
                ..
            }
        ),
        "{:?}",
        check.verdict
    );

    let another = file(
        &rig,
        "another",
        "help-2009",
        "help-2009-master",
        "busy",
        Some("GBXYZ9500002"),
    );
    let check = rig
        .service
        .verify_for_heart(&another, &candidate("help"), &gate)
        .await;
    assert_eq!(check.verdict, GateVerdict::Refuse(Refusal::Label));
    let asked = rig.music_brainz().asked();
    assert!(
        asked.iter().any(|p| p.ends_with("/isrc/GBXYZ1500001")),
        "{asked:?}"
    );
    assert!(
        asked.iter().any(|p| p.ends_with("/isrc/GBXYZ9500002")),
        "{asked:?}"
    );
}

/// A file's label question MusicBrainz does not answer holds the file for MusicBrainz; filed, the
/// hold asks that question again until it answers, and the answer resumes the hold.
#[tokio::test]
async fn a_label_question_musicbrainz_does_not_answer_is_asked_again_and_resumes_its_hold() {
    let rig = gate_rig(GateSetup::default()).await;
    rig.shim_streams(VIDEO, sound_bytes("-", "help-2009-master", "-"))
        .await;
    rig.answers.status("busy", 503);
    rig.music_brainz().code_status("GBXYZ1500001", 503);
    let resumed: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = resumed.clone();
    rig.service
        .on_resume(Arc::new(move |key: &str| seen.lock().push(key.to_string())));
    let id = rig.rig.registry.register(help(Some(139), Some(HELP_CODE)));
    let gate = help_heard(&rig);
    let reissue = file(
        &rig,
        "reissue",
        "help-2009",
        "help-2009-master",
        "busy",
        Some("GBXYZ1500001"),
    );
    let check = rig
        .service
        .verify_for_heart(&reissue, &candidate(&id), &gate)
        .await;
    assert_eq!(check.verdict, GateVerdict::Hold(WaitingFor::MusicBrainz));

    let record = rig.holds.hold(HeldAcquisition {
        kind: HeldKind::Track,
        provider: "soulseek".into(),
        external_id: id.clone(),
        requested_by: Some("brandon".into()),
        held_since_utc: Utc::now(),
        origin: Some(AcquisitionOrigin::Heart),
        waiting_for: Some(WaitingFor::MusicBrainz),
        holders: vec![Holder {
            user: "brandon".into(),
            heard: gate.heard.clone(),
            ..Default::default()
        }],
        ..Default::default()
    });
    let gates: &dyn HeartGates = rig.service.as_ref();
    gates.on_filed(&record, "brandon");
    rig.music_brainz().code(
        "GBXYZ1500001",
        &code_of(
            "6c3e0c2f-5b3a-4b8f-9a55-7f1e2c3d4a01",
            "Help!",
            "The Beatles",
            BEATLES,
        ),
    );
    let key = record.key();
    for _ in 0..400 {
        if resumed.lock().contains(&key) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(*resumed.lock(), [key], "the answer resumed the hold");
}

// ---- AcoustID off: the catalog's clip -----------------------------------------------------------

/// Deezer's track for `isrc`, `seconds` long, whose clip the CDN serves as a frozen clip print.
async fn on_deezer(rig: &GateRig, isrc: &str, title: &str, seconds: i32, clip: &str) {
    let preview = format!("{}/clip/{clip}.mp3?hdnea=exp=1~hmac=x", rig.shim.uri());
    Mock::given(path(format!("/track/isrc:{isrc}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(
                r#"{{"id":116,"title":"{title}","duration":{seconds},"isrc":"{isrc}","preview":"{preview}","artist":{{"id":1,"name":"The Beatles"}},"album":{{"id":9,"title":"x"}}}}"#
            ),
            "application/json",
        ))
        .mount(&rig.shim)
        .await;
    let clip_print =
        std::fs::read(gate_fixture("../clip").join(format!("{clip}-clip.json"))).expect("the clip print");
    Mock::given(path(format!("/clip/{clip}.mp3")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(clip_print, "audio/mpeg"))
        .mount(&rig.shim)
        .await;
}

/// The heard video's channel, by a fresh `/facts`.
async fn channel(rig: &GateRig, name: &str) {
    Mock::given(path("/facts"))
        .and(query_param("id", VIDEO))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"{{"fresh":true,"channel":"{name}"}}"#),
            "application/json",
        ))
        .mount(&rig.shim)
        .await;
}

const WALRUS_CODE: &str = "GBAYE0601638";

/// Walrus 2009's row, whose stream played the 2023 mix: its heard print and its audio again.
async fn walrus_heard(rig: &GateRig) -> (String, HeardRecord) {
    let id = rig.rig.registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("I Am The Walrus (Remastered 2009)".into()),
        duration: Some(276),
        isrc: Some(WALRUS_CODE.into()),
        you_tube_id: Some(VIDEO.into()),
        ..Default::default()
    });
    on_deezer(
        rig,
        WALRUS_CODE,
        "I Am The Walrus (Remastered 2009)",
        276,
        "walrus-2009",
    )
    .await;
    rig.printer
        .register("walrus-2023-heard", frozen_print("walrus-2023-240"));
    rig.shim_streams(VIDEO, sound_bytes("walrus-2023-heard", "walrus-2023-master", "-"))
        .await;
    let record = heard_with(rig, VIDEO, frozen_print("walrus-2023-240"), true, "heard");
    (id, record)
}

/// With AcoustID off, a 2009 Walrus row whose stream played the 2023 mix on "The Beatles - Topic"
/// reads "this song" by the catalog's clip, and the master that played is kept by A2; on another
/// channel the same stream is unclear, and the heart holds for AcoustID being off.
#[tokio::test]
async fn with_acoust_id_off_walrus_2023_on_the_topic_channel_is_this_song_and_its_master_is_kept() {
    let rig = gate_rig(GateSetup {
        acoust_id_off: true,
        music_brainz: false,
        ..Default::default()
    })
    .await;
    let (id, record) = walrus_heard(&rig).await;
    channel(&rig, "The Beatles - Topic").await;
    let GateBuild::Ready(gate) = build(&rig, &id, Some(record)).await else {
        panic!("the clip read this song");
    };
    assert_eq!(gate.reading, HeardReading::Clip(ClipReading::ThisSong));
    rig.printer
        .register("walrus-2023-file", frozen_print("walrus-2023-120"));
    let master = file(
        &rig,
        "master",
        "walrus-2023-file",
        "walrus-2023-master",
        "x",
        None,
    );
    let check = rig
        .service
        .verify_for_heart(&master, &candidate(&id), &gate)
        .await;
    assert!(
        matches!(
            check.verdict,
            GateVerdict::Accept {
                by: Acceptance::HeardMaster,
                ..
            }
        ),
        "{:?}",
        check.verdict
    );
    assert!(rig.answers.asked().is_empty(), "AcoustID is off");
}

#[tokio::test]
async fn with_acoust_id_off_walrus_2023_on_another_channel_holds() {
    let rig = gate_rig(GateSetup {
        acoust_id_off: true,
        music_brainz: false,
        ..Default::default()
    })
    .await;
    let (id, record) = walrus_heard(&rig).await;
    channel(&rig, "Some Uploader").await;
    let built = build(&rig, &id, Some(record)).await;
    assert_eq!(
        waits(&built),
        Some((
            WaitingFor::AcoustIdOff,
            crate::services::common::heart_acquisition_coordinator::waiting_line(WaitingFor::AcoustIdOff)
                .to_string()
        ))
    );
    assert_eq!(gate_of(built).reading, HeardReading::Clip(ClipReading::Unclear));
}

/// With AcoustID off, a catalog clip fpcalc cannot print says nothing of what played: the heart
/// holds under the wait whose wake-ups run the check again (Octo's next start, the song's next
/// play), saying Octo could not read the sound, never that it waits for a Deezer that answered.
#[tokio::test]
async fn with_acoust_id_off_a_clip_fpcalc_cannot_print_holds_saying_the_sound_was_not_read() {
    let rig = gate_rig(GateSetup {
        acoust_id_off: true,
        music_brainz: false,
        ..Default::default()
    })
    .await;
    let id = rig.rig.registry.register(help(Some(139), Some(HELP_CODE)));
    let preview = format!("{}/clip/unprintable.mp3?hdnea=exp=1~hmac=x", rig.shim.uri());
    Mock::given(path(format!("/track/isrc:{HELP_CODE}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(
                r#"{{"id":116,"title":"Help! (Remastered 2009)","duration":139,"isrc":"{HELP_CODE}","preview":"{preview}","artist":{{"id":1,"name":"The Beatles"}},"album":{{"id":9,"title":"x"}}}}"#
            ),
            "application/json",
        ))
        .mount(&rig.shim)
        .await;
    Mock::given(path("/clip/unprintable.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"ab".to_vec(), "audio/mpeg"))
        .mount(&rig.shim)
        .await;
    let record = heard_with(&rig, VIDEO, frozen_print("help-2009-240"), true, "heard");
    let built = build(&rig, &id, Some(record)).await;
    assert_eq!(
        waits(&built),
        Some((ClipReading::UNPRINTED_WAITS_FOR, SOUND_NOT_READ.to_string()))
    );
    assert_eq!(gate_of(built).reading, HeardReading::Clip(ClipReading::Unprinted));
}

/// With AcoustID off, a Mrs. Robinson stream for a Help! row reads "another song or another take":
/// that video is refused for this heart only (a live take would read the same), never for every
/// user, and the heart holds with its line.
#[tokio::test]
async fn with_acoust_id_off_a_mrs_robinson_stream_for_help_is_refused_for_this_heart_only() {
    let rig = gate_rig(GateSetup {
        acoust_id_off: true,
        music_brainz: false,
        ..Default::default()
    })
    .await;
    let id = rig.rig.registry.register(help(Some(139), Some(HELP_CODE)));
    on_deezer(&rig, HELP_CODE, "Help! (Remastered 2009)", 139, "help-2009").await;
    channel(&rig, "Simon & Garfunkel - Topic").await;
    let record = heard_with(&rig, VIDEO, frozen_print("mrsrobinson-studio-240"), true, "heard");
    let built = build(&rig, &id, Some(record)).await;
    assert_eq!(
        waits(&built),
        Some((WaitingFor::AcoustIdOff, ANOTHER_SONG_OR_TAKE.to_string()))
    );
    let gate = gate_of(built);
    let r = gate.r.clone().expect("R");
    assert!(
        rig.rejected
            .is_denied_for_heart(&id, &r, Some(YOUTUBE_PEER), Some(VIDEO)),
        "refused for this heart"
    );
    let routing = rig
        .rig
        .registry
        .resolve_song(&id, false)
        .expect("registered")
        .snapshot();
    assert!(!routing.refuses_video(VIDEO), "never for every user");
}
