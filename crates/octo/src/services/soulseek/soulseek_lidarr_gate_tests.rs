//! Lidarr's imports at the heart gate, through the real download service: every file Lidarr brings
//! for a heart is judged in the check every Lidarr file meets by the person's own gate, the pinned
//! edition's file included, and its acceptance recorded; a refused file goes back to its album's
//! hold folder; its own labels are read only from the download as delivered or, with Lidarr
//! writing no tags, from the file itself; and a heart that joined an import of Lidarr's is judged
//! by its own gate.

use octo_core::fingerprint::heart_gate::Acceptance;

use super::*;
use crate::services::fingerprint::download_verification_service::LabelSource;

/// A gated walk whose download takes Lidarr's imports from `imports`.
async fn lidarr_gated(root: &Root, imports: &Arc<LidarrImportHandoff>) -> Gated {
    gated_with(
        root,
        DownloadSource::Soulseek,
        Vec::new(),
        Options {
            key: "key",
            lidarr_imports: Some(imports.clone()),
            ..Default::default()
        },
    )
    .await
}

/// Lidarr's import of "Started" with this sound, waiting in its album's hold folder.
fn held_import(root: &Root, bytes: &[u8]) -> String {
    let folder = format!("{}/.octo-incoming/lidarr-held/rg-nwts", root.path());
    std::fs::create_dir_all(&folder).expect("made");
    let path = format!("{folder}/04 - Started.flac");
    std::fs::write(&path, bytes).expect("written");
    path
}

/// Offers the held import as the song, its labels read from `labels`, given back to where it
/// waits when refused.
fn offer(imports: &LidarrImportHandoff, g: &Gated, held: &str, labels: LabelSource) {
    imports.offer_import(
        &g.started(),
        LidarrImport {
            path: held.to_string(),
            quiet: false,
            labels,
            refused_to: Some(held.to_string()),
            filed: None,
        },
    );
}

/// AcoustID's answer naming a live take of "Started": the same song, another recording.
fn live_take() -> String {
    json!({"status": "ok", "results": [{"id": "fp", "score": 0.97, "recordings": [{
        "id": "rec-started-live", "title": "Started (Live)", "artists": [{"name": "Drake"}],
        "duration": 180, "sources": 40,
    }]}]})
    .to_string()
}

/// The edition Lidarr was pinned to is only a fetch hint: its file still has to pass the heart's
/// gate by its own sound. A live take is refused (not the master heard, and AcoustID names another
/// recording) and goes back to the album's hold folder; a copy AcoustID reads clearly as what was
/// heard is kept, and its acceptance is recorded for the person.
#[tokio::test]
async fn the_pinned_editions_lidarr_file_still_has_to_pass_a1() {
    let root = Root::new("octo-gate-lidarr-pin-");
    let imports = Arc::new(LidarrImportHandoff::new());
    let g = lidarr_gated(&root, &imports).await;
    g.answers.answer("live", &live_take());
    g.answers.answer("clear", &started_clear());
    let heard = g.heard_video(4_000_000, None, "started", "heard-master").await;
    let gate = gate_of("brandon", Some(heard), HeardReading::RStays { tag: None });

    let held = held_import(&root, &named_flac("started", "pinned-master", "live", None));
    offer(&imports, &g, &held, LabelSource::TheFile);
    let ledger = GateLedger::default();
    let refused = acquire_as(
        &g,
        DownloadSource::Lidarr,
        heart_place(vec![gate.clone()], &ledger),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    let why = format!("{refused:?}");
    assert!(
        why.contains("is not kept: AcoustID names another recording"),
        "{why}"
    );
    assert!(
        std::path::Path::new(&held).is_file(),
        "back in its album's hold folder: {refused:?}, files now {:?}",
        files_under(&root.path())
    );
    assert_ne!(
        ledger.outcome_for("brandon", AcquisitionOrigin::Heart),
        Some(GateOutcome::Accepted)
    );

    std::fs::write(&held, named_flac("started", "pinned-master", "clear", None)).expect("written");
    offer(&imports, &g, &held, LabelSource::TheFile);
    let ledger = GateLedger::default();
    let kept = acquire_as(&g, DownloadSource::Lidarr, heart_place(vec![gate], &ledger))
        .await
        .expect("kept by its own sound");
    assert!(std::path::Path::new(&kept).is_file(), "{kept}");
    assert!(
        !std::path::Path::new(&held).exists(),
        "taken from the hold folder"
    );
    assert_eq!(
        ledger.acceptance_for("brandon", AcquisitionOrigin::Heart),
        Some(Acceptance::Sound)
    );
}

/// A Lidarr file's own ISRC tag naming another recording refuses it only when the tag is the
/// download's own: read from the file when Lidarr writes no tags, or from the download as
/// delivered. With Lidarr writing tags, the file's tags are Lidarr's own matching, never a label,
/// and the file is kept by its sound.
#[tokio::test]
async fn a_lidarr_files_own_tags_count_only_from_the_delivered_file_or_with_lidarr_writing_no_tags() {
    let tagged = || named_flac("started", "pinned-master", "clear", Some("USXX19900001"));
    for (case, kept) in [("the file", false), ("no labels", true), ("delivered", false)] {
        let root = Root::new("octo-gate-lidarr-labels-");
        let imports = Arc::new(LidarrImportHandoff::new());
        let g = lidarr_gated(&root, &imports).await;
        g.answers.answer("clear", &started_clear());
        let heard = g.heard_video(4_000_000, None, "started", "heard-master").await;
        let gate = gate_of("brandon", Some(heard), HeardReading::RStays { tag: None });
        let labels = match case {
            "the file" => LabelSource::TheFile,
            "no labels" => LabelSource::None,
            _ => {
                let delivered = format!("{}/downloads/04 - Started.flac", root.path());
                std::fs::create_dir_all(format!("{}/downloads", root.path())).expect("made");
                std::fs::write(&delivered, tagged()).expect("written");
                LabelSource::Delivered(PathBuf::from(delivered))
            }
        };
        // What Lidarr imported carries its tag only where the download's own file did.
        let imported = if case == "delivered" {
            named_flac("started", "pinned-master", "clear", None)
        } else {
            tagged()
        };
        let held = held_import(&root, &imported);
        offer(&imports, &g, &held, labels);

        let got = acquire_as(
            &g,
            DownloadSource::Lidarr,
            heart_place(vec![gate], &GateLedger::default()),
        )
        .await;

        assert_eq!(got.is_ok(), kept, "{case}: {got:?}");
    }
}

/// A heart that joined an import Lidarr brought is judged by its own gate: the import lands by its
/// own checks, and the heart counts the file as its own only when its own gate passes it.
#[tokio::test]
async fn a_heart_joining_an_import_lidarr_brought_is_judged_by_its_own_gate() {
    for (master, accepted) in [("another-master", false), ("heard-master", true)] {
        let root = Root::new("octo-gate-lidarr-join-");
        let imports = Arc::new(LidarrImportHandoff::new());
        let g = lidarr_gated(&root, &imports).await;
        let heard = g.heard_video(4_000_000, None, "started", "heard-master").await;
        let joiner = gate_of("anna", Some(heard), HeardReading::RStays { tag: None });
        let held = held_import(&root, &named_flac("started", master, "nothing", None));
        offer(&imports, &g, &held, LabelSource::TheFile);
        let ledger = GateLedger::default();
        let place = AcquisitionPlace {
            origin: Some(AcquisitionOrigin::Import),
            gates: vec![import_gate(&["rec-started"]), joiner],
            ledger: ledger.clone(),
            ..Default::default()
        };

        let landed = acquire_as(&g, DownloadSource::Lidarr, place)
            .await
            .expect("the import lands by its own checks");

        assert!(std::path::Path::new(&landed).is_file(), "{landed}");
        assert_eq!(
            ledger.outcome_for("brandon", AcquisitionOrigin::Import),
            Some(GateOutcome::Accepted),
            "{master}"
        );
        let heart = ledger.outcome_for("anna", AcquisitionOrigin::Heart);
        if accepted {
            assert_eq!(heart, Some(GateOutcome::Accepted), "{master}");
            assert_eq!(
                ledger.acceptance_for("anna", AcquisitionOrigin::Heart),
                Some(Acceptance::HeardMaster),
                "{master}"
            );
        } else {
            assert!(
                matches!(heart, Some(GateOutcome::NotAccepted(_))),
                "{master}: {heart:?}"
            );
        }
    }
}
