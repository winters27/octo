//! Lidarr's imports at the heart gate, through the real download service: every file Lidarr brings
//! for a heart is judged in the check every Lidarr file meets by the person's own gate, the pinned
//! edition's file included, and its acceptance recorded; a refused file goes back to its album's
//! hold folder; its own labels are read only from the download as delivered or, with Lidarr
//! writing no tags, from the file itself; a held file of Lidarr's is judged again by the same
//! check and the labels of its download, which its hold carries with it; a heart that joined an
//! import of Lidarr's is judged by its own gate; and only a Better quality aims Lidarr at what the
//! original is.

use octo_core::fingerprint::heart_gate::Acceptance;

use super::*;
use crate::services::common::HeartGates;
use crate::services::common::soulseek_hold_store::HeldLidarrFile;
use crate::services::fingerprint::download_verification_service::LabelSource;

/// A recording of "Started" MusicBrainz knows as a live take.
const LIVE_TAKE: &str = "5b1c1d1e-2f3a-4b5c-8d6e-7f8091a2b3c4";

/// A Lidarr fetcher that hands over the audio it was given, with no labels (Lidarr wrote its
/// tags), and remembers each request. The original it is asked about reads as "rec-original".
#[derive(Default)]
struct LidarrFetches {
    audio: Mutex<Vec<u8>>,
    requests: Mutex<Vec<LidarrTrackRequest>>,
}

#[async_trait]
impl ILidarrTrackFetcher for LidarrFetches {
    async fn fetch(
        &self,
        request: &LidarrTrackRequest,
        destination_directory: &str,
        _ct: &CancellationToken,
    ) -> Result<crate::services::lidarr::LidarrFetched, octo_core::lidarr::LidarrError> {
        self.requests.lock().push(request.clone());
        std::fs::create_dir_all(destination_directory).expect("made");
        let path = combine(destination_directory, "04 - Started.flac");
        std::fs::write(&path, self.audio.lock().clone()).expect("written");
        Ok(crate::services::lidarr::LidarrFetched {
            path,
            labels: LabelSource::None,
        })
    }

    async fn original_recordings(&self, _request: &LidarrTrackRequest) -> Vec<String> {
        vec!["rec-original".into()]
    }
}

/// A FLAC of "Started" whose sound is named, tagged by Lidarr's own matching as `recording`.
fn tagged_by_lidarr(master: &str, answer: &str, recording: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("tagged.flac");
    std::fs::write(&path, named_flac("started", master, answer, None)).expect("written");
    let mut file = octo_media::tags::TagFile::open(&path).expect("the FLAC opens");
    octo_media::tags::tag_writer_extras::set_recording_id(&mut file, recording);
    file.save().expect("the tag is saved");
    std::fs::read(&path).expect("the tagged file")
}

/// A replacement's place: work nobody waits on, judged by the shipped checks.
fn replacement() -> AcquisitionPlace {
    AcquisitionPlace {
        origin: Some(AcquisitionOrigin::Replacement),
        ..Default::default()
    }
}

/// What an acquisition left waiting in the one hold path: one file Lidarr brought, carrying the
/// labels it was judged by (none: Lidarr wrote its tags).
fn held_lidarr_file_of(waited: &anyhow::Error) -> AcquisitionHeld {
    let held = waited
        .downcast_ref::<AcquisitionHeld>()
        .unwrap_or_else(|| panic!("held: {waited:?}"))
        .clone();
    assert_eq!(held.files.len(), 1, "{held:?}");
    assert!(std::path::Path::new(&held.files[0]).is_file(), "{held:?}");
    assert_eq!(
        held.lidarr,
        [HeldLidarrFile::of(&held.files[0], &LabelSource::None)],
        "{held:?}"
    );
    held
}

/// `place` resumed with what a hold held, as the chain and a replacement hand it back.
fn resumed(held: &AcquisitionHeld, place: AcquisitionPlace) -> AcquisitionPlace {
    AcquisitionPlace {
        held_files: held.files.clone(),
        held_lidarr: held.lidarr.clone(),
        ..place
    }
}

/// A replacement Lidarr brought a copy for, held for AcoustID's bulk turn, is judged again with
/// the labels of its download once the turn comes: the recording tag Lidarr wrote from its own
/// matching (a live take, which the shipped checks would refuse as another version) is no label,
/// so the copy lands by its sound, and Lidarr is not asked for the song again.
#[tokio::test]
async fn a_lidarr_replacement_held_for_its_bulk_turn_is_judged_again_with_its_downloads_labels() {
    use crate::services::fingerprint::acoust_id_rate_limiter::{BusyKind, CallEnd, KeyLane, LookupClass};

    let music_brainz = MockServer::builder().start().await;
    Mock::given(wiremock::matchers::path(format!("/ws/2/recording/{LIVE_TAKE}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            json!({"id": LIVE_TAKE, "title": "Started (Live)", "releases": []}).to_string(),
            "application/json",
        ))
        .mount(&music_brainz)
        .await;
    let base = url::Url::parse(&format!("{}/ws/2/", music_brainz.uri())).expect("parses");
    let quick = Duration::from_millis(5);
    let client = Arc::new(
        crate::services::fingerprint::MusicBrainzClient::with_base_url(base).with_pace(
            quick,
            Duration::from_millis(500),
            quick,
        ),
    );
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(20), Duration::from_millis(10))
            .with_jitter(|| 1.0),
    );
    let fetches = Arc::new(LidarrFetches::default());
    *fetches.audio.lock() = tagged_by_lidarr("lidarr-master", "nothing", LIVE_TAKE);
    let root = Root::new("octo-gate-lidarr-replace-held-");
    let g = gated_with(
        &root,
        DownloadSource::Soulseek,
        Vec::new(),
        Options {
            limiter: Some(limiter.clone()),
            verify: true,
            lidarr_fetcher: Some(fetches.clone()),
            music_brainz: Some(client),
            ..Default::default()
        },
    )
    .await;
    // AcoustID failed bulk calls in a row: the next bulk turn is not free at once.
    for _ in 0..5 {
        limiter
            .permit(KeyLane::BuiltIn, LookupClass::Bulk, true)
            .await
            .expect("a turn")
            .end(CallEnd::Busy(BusyKind::Outage));
    }

    let waited = acquire_as(&g, DownloadSource::Lidarr, replacement())
        .await
        .expect_err("the copy waits for its bulk turn");
    let held = held_lidarr_file_of(&waited);

    tokio::time::sleep(limiter.gap(KeyLane::BuiltIn, LookupClass::Bulk)).await;
    let landed = acquire_as(&g, DownloadSource::Lidarr, resumed(&held, replacement()))
        .await
        .expect("the copy lands: Lidarr's own tag is no label");

    assert!(std::path::Path::new(&landed).is_file(), "{landed}");
    assert!(
        !std::path::Path::new(&held.files[0]).exists(),
        "taken from the hold path"
    );
    assert_eq!(fetches.requests.lock().len(), 1, "Lidarr is not asked again");
}

/// A held replacement copy of Lidarr's that its own check refuses when judged again ends the
/// download from Lidarr, as a refusal in time does: Lidarr is not searched again for it.
#[tokio::test]
async fn a_held_lidarr_replacement_refused_when_judged_again_does_not_search_lidarr_again() {
    let fetches = Arc::new(LidarrFetches::default());
    let root = Root::new("octo-gate-lidarr-replace-refused-");
    let g = gated_with(
        &root,
        DownloadSource::Soulseek,
        Vec::new(),
        Options {
            key: "key",
            verify: true,
            lidarr_fetcher: Some(fetches.clone()),
            ..Default::default()
        },
    )
    .await;
    g.answers.answer(
        "another",
        &json!({"status": "ok", "results": [{"id": "fp", "score": 0.97, "recordings": [{
            "id": "rec-other", "title": "Another Song", "artists": [{"name": "Someone Else"}],
            "duration": 180, "sources": 40,
        }]}]})
        .to_string(),
    );
    let folder = format!("{}/.octo-incoming/held/Track_soulseek_x", root.path());
    std::fs::create_dir_all(&folder).expect("made");
    let held = format!("{folder}/held-04 - Started.flac");
    std::fs::write(&held, named_flac("other", "other-master", "another", None)).expect("written");

    let refused = acquire_as(
        &g,
        DownloadSource::Lidarr,
        AcquisitionPlace {
            held_files: vec![held.clone()],
            held_lidarr: vec![HeldLidarrFile::of(&held, &LabelSource::None)],
            ..replacement()
        },
    )
    .await
    .expect_err("another song is refused");

    assert!(refused.to_string().contains("AcoustID identified"), "{refused}");
    assert!(!std::path::Path::new(&held).exists(), "thrown away");
    assert!(fetches.requests.lock().is_empty(), "Lidarr is not searched again");
}

/// A heart's Lidarr file that waits for AcoustID is judged again once AcoustID answers by the
/// check every Lidarr file meets, with its download's labels: an ISRC Lidarr wrote from its own
/// matching, naming another recording, does not refuse it, and it is kept by its sound for the
/// person. Octo's own key's lane starts at test gaps, and the heart's pace the busy answer backed
/// off is waited out before the file is judged again, so the second lookup is AcoustID's to
/// answer however fast the test runs.
#[tokio::test]
async fn a_hearts_held_lidarr_file_is_judged_again_with_its_downloads_labels() {
    use crate::services::fingerprint::acoust_id_rate_limiter::{KeyLane, LookupClass};

    let root = Root::new("octo-gate-lidarr-held-");
    let imports = Arc::new(LidarrImportHandoff::new());
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(20), Duration::from_millis(10))
            .with_jitter(|| 1.0),
    );
    let g = gated_with(
        &root,
        DownloadSource::Soulseek,
        Vec::new(),
        Options {
            limiter: Some(limiter.clone()),
            lidarr_imports: Some(imports.clone()),
            ..Default::default()
        },
    )
    .await;
    let heard = g.heard_video(4_000_000, None, "started", "heard-master").await;
    let gate = gate_of("brandon", Some(heard), HeardReading::RStays { tag: None });
    g.answers.status("clear", 503);
    let held = held_import(
        &root,
        &named_flac("started", "pinned-master", "clear", Some("USXX19900001")),
    );
    offer(&imports, &g, &held, LabelSource::None);

    let waited = acquire_as(
        &g,
        DownloadSource::Lidarr,
        heart_place(vec![gate.clone()], &GateLedger::default()),
    )
    .await
    .expect_err("it waits for AcoustID");
    let waiting = held_lidarr_file_of(&waited);
    assert!(
        limiter.backing_off(KeyLane::BuiltIn, LookupClass::Heart),
        "the busy answer backed the heart's pace off"
    );

    g.answers.answer("clear", &started_clear());
    tokio::time::sleep(limiter.gap(KeyLane::BuiltIn, LookupClass::Heart)).await;
    let ledger = GateLedger::default();
    let kept = acquire_as(
        &g,
        DownloadSource::Soulseek,
        resumed(&waiting, heart_place(vec![gate], &ledger)),
    )
    .await
    .expect("kept by its sound: Lidarr's own tag is no label");

    assert!(std::path::Path::new(&kept).is_file(), "{kept}");
    assert!(
        !std::path::Path::new(&waiting.files[0]).exists(),
        "taken from the hold path"
    );
    assert_eq!(
        ledger.acceptance_for("brandon", AcquisitionOrigin::Heart),
        Some(Acceptance::Sound)
    );
}

/// A hearted album's held track Lidarr brought is checked again by the labels of its download:
/// the ISRC Lidarr wrote, naming another recording, refuses it only when it is the download's own.
#[tokio::test]
async fn a_held_album_track_lidarr_brought_is_checked_again_with_its_downloads_labels() {
    let root = Root::new("octo-gate-lidarr-album-held-");
    let imports = Arc::new(LidarrImportHandoff::new());
    let g = lidarr_gated(&root, &imports).await;
    g.answers.answer("clear", &started_clear());
    let gate = HeartGate {
        origin: AcquisitionOrigin::AlbumHeart,
        ..started_gate()
    };
    let delivered = format!("{}/downloads/04 - Started.flac", root.path());
    std::fs::create_dir_all(format!("{}/downloads", root.path())).expect("made");
    std::fs::write(
        &delivered,
        named_flac("started", "pinned-master", "clear", Some("USXX19900001")),
    )
    .expect("written");
    for (labels, accepted) in [
        (LabelSource::None, true),
        (LabelSource::Delivered(PathBuf::from(&delivered)), false),
    ] {
        let held = held_import(
            &root,
            &named_flac("started", "pinned-master", "clear", Some("USXX19900001")),
        );
        // The labels the held track's hold carries for it.
        let carried = HeldLidarrFile::of(&held, &labels).labels();

        let verdicts = g
            .verification
            .judge_held(
                &held,
                carried,
                &acquisition_key("soulseek", &g.started(), false),
                std::slice::from_ref(&gate),
            )
            .await;

        assert_eq!(verdicts.len(), 1, "{labels:?}");
        assert_eq!(
            matches!(
                verdicts[0],
                octo_core::fingerprint::heart_gate::GateVerdict::Accept { .. }
            ),
            accepted,
            "{labels:?}: {verdicts:?}"
        );
    }
}

/// A held file of Lidarr's that waits again when judged again keeps its labels on the hold it
/// leaves, and so does every held file that waits on with it.
#[tokio::test]
async fn held_lidarr_files_that_wait_again_keep_their_labels() {
    let root = Root::new("octo-gate-lidarr-held-again-");
    let imports = Arc::new(LidarrImportHandoff::new());
    let g = lidarr_gated(&root, &imports).await;
    let heard = g.heard_video(4_000_000, None, "started", "heard-master").await;
    let gate = gate_of("brandon", Some(heard), HeardReading::RStays { tag: None });
    g.answers.status("clear", 503);
    let folder = format!("{}/.octo-incoming/held/Track_soulseek_x", root.path());
    std::fs::create_dir_all(&folder).expect("made");
    let files: Vec<String> = ["a", "b"]
        .iter()
        .map(|name| {
            let file = format!("{folder}/{name}-04 - Started.flac");
            std::fs::write(&file, named_flac("started", "pinned-master", "clear", None)).expect("written");
            file
        })
        .collect();
    let lidarr = vec![
        HeldLidarrFile::of(&files[0], &LabelSource::None),
        HeldLidarrFile::of(
            &files[1],
            &LabelSource::Delivered(PathBuf::from("/downloads/complete/04 - Started.flac")),
        ),
    ];

    let waited = acquire_as(
        &g,
        DownloadSource::Soulseek,
        AcquisitionPlace {
            held_files: files.clone(),
            held_lidarr: lidarr.clone(),
            ..heart_place(vec![gate], &GateLedger::default())
        },
    )
    .await
    .expect_err("AcoustID does not answer: it waits again");

    let held = waited
        .downcast_ref::<AcquisitionHeld>()
        .unwrap_or_else(|| panic!("held: {waited:?}"));
    assert_eq!(held.files, files);
    assert_eq!(held.lidarr, lidarr);
}

/// Only a Better quality aims Lidarr at what the original is (taken anyway too): a Wrong song or
/// a Wrong version asks Lidarr for the song with no recording to pin it to, since its original is
/// the recording the person rejected.
#[tokio::test]
async fn only_a_better_quality_asks_lidarr_for_what_the_original_is() {
    for (upgrade_search, recordings) in [(true, vec!["rec-original".to_string()]), (false, Vec::new())] {
        let fetches = Arc::new(LidarrFetches::default());
        *fetches.audio.lock() = named_flac("started", "lidarr-master", "nothing", None);
        let root = Root::new("octo-gate-lidarr-original-");
        let g = gated_with(
            &root,
            DownloadSource::Soulseek,
            Vec::new(),
            Options {
                lidarr_fetcher: Some(fetches.clone()),
                ..Default::default()
            },
        )
        .await;

        let _ = tokio::time::timeout(
            Duration::from_secs(60),
            g.walk.service.execute_acquisition_to(
                "soulseek",
                &g.started(),
                false,
                true,
                Some(DownloadSource::Lidarr),
                &CancellationToken::new(),
                None,
                upgrade_search,
                None,
                replacement(),
            ),
        )
        .await
        .expect("within a minute");

        let asked = fetches.requests.lock().clone();
        assert_eq!(asked.len(), 1, "{upgrade_search}");
        assert_eq!(asked[0].recordings, recordings, "{upgrade_search}");
    }
}

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
