//! Every copy offered for a file kept as heard is held to proof 2 against the kept file itself:
//! its print kept with it, else the file printed, never "no reference", even with the heard-sample
//! store emptied.

use octo_core::fingerprint::heart_gate::{GateVerdict, Refusal};
use octo_core::settings::AppSettings;
use octo_media::audio::AudioFingerprinter;
use url::Url;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::SoulseekHoldStore;
use crate::services::fingerprint::catalog_clip::CatalogClip;
use crate::services::fingerprint::download_verification_service::{HeartCandidate, HeartParts, LabelSource};
use crate::services::fingerprint::heard_master_check::HeardMasterCheck;
use crate::services::fingerprint::{
    AcoustIdRateLimitHandler, AcoustIdRateLimiter, DownloadVerificationService,
};
use crate::services::library::library_action_test_support::RecordingLibrary;
use crate::services::metadata::deezer_metadata_service::DeezerMetadataService;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::prefetch::heard_samples::tests::{rig_in, song_bytes};
use crate::services::soulseek::RejectedPeerRegistry;
use crate::services::you_tube::YouTubeResolver;

const HELP_2009: &str = "4416d38e-0f68-4c75-93b9-c1a793fccb49";

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            dir.join(format!("{program}{}", std::env::consts::EXE_SUFFIX))
                .is_file()
        })
    })
}

/// 40 seconds of `source` (an ffmpeg `lavfi` source) as FLAC, or None where ffmpeg failed.
fn made(path: &Path, source: &str) -> Option<PathBuf> {
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", source])
        .arg(path)
        .status()
        .is_ok_and(|status| status.success());
    made.then(|| path.to_path_buf())
}

/// The kept file's sound: pink noise.
const NOISE: &str = "anoisesrc=color=pink:duration=40:sample_rate=44100:amplitude=0.3";
/// A copy's sound nothing like it: two rising tones.
const SWEEP: &str =
    "aevalsrc=0.4*sin(2*PI*(220+40*t)*t)|0.4*sin(2*PI*(330+25*t)*t):sample_rate=44100:duration=40";

/// The heart gate's service over an AcoustID that reads every file clearly as the 2009 Help!,
/// and the kept-file checks beside it, sharing one emptied heard-sample store.
struct Gate {
    service: Arc<DownloadVerificationService>,
    rechecks: KeptRechecks,
    library: Arc<RecordingLibrary>,
    _servers: [MockServer; 3],
}

async fn gate(dir: &Path) -> Gate {
    let acoust_id = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(
                r#"{{"status":"ok","results":[{{"id":"fp","score":0.98,"recordings":[{{"id":"{HELP_2009}","title":"Help!","duration":40,"sources":60,"artists":[{{"id":"b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d","name":"The Beatles"}}]}}]}}]}}"#
            ),
            "application/json",
        ))
        .mount(&acoust_id)
        .await;
    let (shim, deezer) = (MockServer::start().await, MockServer::start().await);
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(1), Duration::from_millis(1))
            .with_jitter(|| 1.0),
    );
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::with_base_url(
            limiter,
            Url::parse(&format!("{}/", acoust_id.uri())).expect("parses"),
        )),
        settings.clone(),
        Some("octo-key"),
    ));
    let holds = Arc::new(SoulseekHoldStore::new(None::<PathBuf>));
    let rig = rig_in(&dir.join("samples"), &shim.uri(), holds.clone(), 16, 64 << 20);
    let service = Arc::new(DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        client.clone(),
        settings.clone(),
        None,
        None,
    ));
    let clip = CatalogClip::new(
        Arc::new(DeezerMetadataService::with_base_url(
            Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
            settings.clone(),
            deezer.uri(),
        )),
        rig.samples.clone(),
        Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
    );
    service.use_heart(HeartParts {
        registry: rig.registry.clone(),
        samples: rig.samples.clone(),
        master: Arc::new(HeardMasterCheck::new(rig.samples.clone())),
        clip: Arc::new(clip),
        catalog: Arc::new(super::kept_tests::NoCatalog),
        holds,
        rejected: Arc::new(RejectedPeerRegistry::in_memory()),
    });
    let library = Arc::new(RecordingLibrary::default());
    let rechecks = KeptRechecks::new(library.clone(), client, settings, rig.registry.clone());
    rechecks.use_samples(rig.samples.clone());
    assert_eq!(rig.samples.count(), 0, "the heard-sample store is empty");
    Gate {
        service,
        rechecks,
        library,
        _servers: [acoust_id, shim, deezer],
    }
}

/// Brandon's kept-as-heard Help! at `path`, heard from a video whose sample the store no longer
/// has, kept with `print`.
fn kept_at(path: &Path, print: Option<String>) -> LocalSongMapping {
    LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: "help".into(),
        local_path: path.to_string_lossy().replace('\\', "/"),
        artist: "The Beatles".into(),
        title: "Help!".into(),
        proof_mark: Some("as heard on YouTube".into()),
        heart: Some(KeptHeart {
            users: vec!["brandon".into()],
            recording: Some(HeardRecording {
                ids: vec![HELP_2009.into()],
                title: "Help!".into(),
                artist: "The Beatles".into(),
                ..Default::default()
            }),
            heard: Some(HeardRecord {
                heard_at: Utc::now(),
                source: HeardSource::Video {
                    id: "u7K72X4eo_s".into(),
                    size: Some(4_000_000),
                    content_type: "audio/mp4".into(),
                    itag: Some(140),
                },
                sample_id: Some("sample-gone".into()),
            }),
            as_heard: true,
            print,
            seconds: Some(40),
        }),
        ..Default::default()
    }
}

async fn judged(gate: &Gate, kept: &Path, candidate: &Path) -> GateVerdict {
    let KeptFor::Gate(heart) = gate
        .rechecks
        .gate_for(&kept.to_string_lossy().replace('\\', "/"), "brandon")
        .await
    else {
        panic!("no gate for the kept file");
    };
    gate.service
        .verify_for_heart(
            candidate,
            &HeartCandidate {
                labels: LabelSource::None,
                identity: None,
                acquisition_key: String::new(),
            },
            &heart,
        )
        .await
        .verdict
}

/// With the heard-sample store emptied, a copy AcoustID reads clearly as R but whose sound differs
/// from the kept file is refused by proof 2 against the kept file's own print, as kept with it;
/// that print reads back to exactly the items `fpcalc` gives the kept file.
#[tokio::test]
async fn a_copy_proof_two_refuses_against_the_kept_file_is_refused_with_the_store_emptied() {
    if !on_path("ffmpeg") || !on_path("fpcalc") {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let (Some(kept), Some(candidate)) = (
        made(&dir.path().join("kept.flac"), NOISE),
        made(&dir.path().join("candidate.flac"), SWEEP),
    ) else {
        panic!("ffmpeg made no audio");
    };
    let gate = gate(dir.path()).await;
    let settings = AppSettings::default().soulseek;
    let (seconds, timeout) = (
        settings.effective_fingerprint_seconds(),
        settings.effective_fingerprint_timeout_seconds(),
    );
    let printer = AudioFingerprinter::new();
    let print = printer
        .fingerprint(&kept, seconds, timeout)
        .await
        .fingerprint
        .expect("fpcalc printed the kept file");
    let raw = printer
        .raw_fingerprint(&kept, seconds, timeout)
        .await
        .expect("fpcalc's raw items")
        .items;
    assert_eq!(compressed_print::decompress(&print), Some(raw));
    gate.library.mappings.lock().push(kept_at(&kept, Some(print)));

    assert_eq!(
        judged(&gate, &kept, &candidate).await,
        GateVerdict::Refuse(Refusal::DiffersFromHeard)
    );
}

/// A kept file with no print kept with it is printed for proof 2 itself: a copy that differs from
/// it is refused against it, with nothing in the heard-sample store.
#[tokio::test]
async fn a_kept_file_with_no_print_is_printed_for_proof_two() {
    if !on_path("ffmpeg") || !on_path("fpcalc") {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let Some(candidate) = made(&dir.path().join("candidate.flac"), SWEEP) else {
        panic!("ffmpeg made no audio");
    };
    // The test printer reads these bytes back as the kept file's items.
    let kept = dir.path().join("kept.m4a");
    std::fs::write(&kept, song_bytes(40.0, 7)).expect("written");
    let gate = gate(dir.path()).await;
    gate.library.mappings.lock().push(kept_at(&kept, None));

    assert_eq!(
        judged(&gate, &kept, &candidate).await,
        GateVerdict::Refuse(Refusal::DiffersFromHeard)
    );
}
