//! The upgrade queue, and the worker that runs it. The Better quality page's endpoint tests
//! belong to the admin controller.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use futures::FutureExt;
use octo_core::common::Clock;
use octo_core::settings::{AppSettings, LibraryAction, SettingsStore, SoulseekSettings};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::services::soulseek::soulseek_link::OFFLINE_TEXT;

fn ask(id: &str) -> UpgradeAsk {
    UpgradeAsk {
        navidrome_id: id.to_string(),
        title: Some(format!("Song {id}")),
        artist: Some("Artist".to_string()),
        ..Default::default()
    }
}

fn asks(ids: &[&str]) -> Vec<UpgradeAsk> {
    ids.iter().map(|id| ask(id)).collect()
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/rust-migration/fixtures/state")
        .join(name)
}

// ---- The queue ---------------------------------------------------------------------------

#[test]
fn a_song_already_queued_is_not_queued_twice() {
    let queue = UpgradeQueue::new();
    queue.add(asks(&["a", "b"]), "alice", "app");
    let (jobs, refused) = queue.add(asks(&["a"]), "bob", "page");
    assert!(refused.is_none());
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].requested_by, "alice");
    assert_eq!(queue.snapshot().len(), 2);
    assert_eq!(queue.open_count(), 2);
}

#[test]
fn only_jobs_that_have_not_started_can_be_canceled() {
    let queue = UpgradeQueue::new();
    queue.add(asks(&["a", "b"]), "alice", "app");
    let running = queue.take_next().expect("a job");
    assert_eq!(queue.cancel(&["a", "b"]), 1);
    let left = queue.snapshot();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].navidrome_id, running.navidrome_id);
}

#[test]
fn a_restart_keeps_the_queue_and_a_running_job_goes_back_in_it() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("upgrades.json");
    let queue = UpgradeQueue::with_path(Some(path.clone()));
    queue.add(asks(&["a", "b"]), "alice", "app");
    assert_eq!(queue.take_next().expect("a job").state, UpgradeStates::WORKING);

    let again = UpgradeQueue::with_path(Some(path));
    assert!(
        again
            .snapshot()
            .iter()
            .all(|job| job.state == UpgradeStates::QUEUED)
    );
    assert_eq!(again.open_count(), 2);
}

#[test]
fn finished_jobs_are_kept_a_week() {
    let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap()));
    let clock_now = now.clone();
    let queue = UpgradeQueue::new().with_clock(Clock::new(move || *clock_now.lock()));
    queue.add(asks(&["a"]), "alice", "app");
    queue.update("a", |job| job.state = UpgradeStates::UPGRADED.to_string());
    *now.lock() += chrono::TimeDelta::days(6);
    assert_eq!(queue.snapshot().len(), 1);
    *now.lock() += chrono::TimeDelta::days(2);
    assert!(queue.snapshot().is_empty());
}

#[test]
fn each_person_sees_their_own_jobs() {
    let queue = UpgradeQueue::new();
    queue.add(asks(&["a"]), "alice", "app");
    queue.add(asks(&["b"]), "bob", "app");
    let mine = queue.snapshot_for(Some("Alice"));
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].navidrome_id, "a");
}

#[test]
fn clear_forgets_only_finished_jobs() {
    let queue = UpgradeQueue::new();
    queue.add(asks(&["a", "b"]), "alice", "app");
    queue.update("a", |job| job.state = UpgradeStates::NOT_FOUND.to_string());
    assert_eq!(queue.clear_finished(), 1);
    let left = queue.snapshot();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].navidrome_id, "b");
}

// ---- The worker --------------------------------------------------------------------------

type Apply = Arc<dyn Fn(LibraryActionRequest) -> BoxFuture<'static, LibraryActionOutcome> + Send + Sync>;

fn seams(width: i32, apply: Apply, offline: Arc<AtomicBool>) -> UpgradeWorkerSeams {
    UpgradeWorkerSeams {
        apply: Arc::new(move |request| {
            let apply = apply.clone();
            async move { Ok(apply(request).await) }.boxed()
        }),
        describe: Arc::new(|_| async { None }.boxed()),
        width: Arc::new(move || width),
        soulseek_offline: Arc::new(move || {
            let offline = offline.load(Ordering::SeqCst);
            async move { offline }.boxed()
        }),
        source_name: Arc::new(|| "Soulseek".to_string()),
    }
}

fn worker(
    queue: &Arc<UpgradeQueue>,
    width: i32,
    apply: Apply,
    offline: Option<Arc<AtomicBool>>,
) -> Arc<UpgradeWorker> {
    Arc::new(UpgradeWorker::with_seams(
        queue.clone(),
        seams(width, apply, offline.unwrap_or_default()),
        None,
        None,
        None,
    ))
}

fn outcome(state: LibraryActionState, detail: &str) -> LibraryActionOutcome {
    LibraryActionOutcome::new(state, Some(detail.to_string()))
}

#[tokio::test]
async fn no_more_run_at_once_than_downloads_may() {
    let queue = Arc::new(UpgradeQueue::new());
    queue.add((0..6).map(|i| ask(&format!("s{i}"))).collect(), "alice", "app");
    let (release, released) = tokio::sync::watch::channel(false);
    let inside = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    let (counted, peak) = (inside.clone(), most.clone());
    let apply: Apply = Arc::new(move |_| {
        let (inside, most, mut released) = (counted.clone(), peak.clone(), released.clone());
        async move {
            let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            let _ = released.wait_for(|go| *go).await;
            inside.fetch_sub(1, Ordering::SeqCst);
            outcome(LibraryActionState::Applied, "Replaced with x.flac.")
        }
        .boxed()
    });
    let worker = worker(&queue, 3, apply, None);
    let ct = CancellationToken::new();

    assert_eq!(worker.tick(&ct).await, 3);
    assert_eq!(worker.tick(&ct).await, 0);
    for _ in 0..1000 {
        if inside.load(Ordering::SeqCst) >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    release.send(true).expect("released");
    worker.drain().await;
    assert_eq!(worker.tick(&ct).await, 3);
    worker.drain().await;

    assert_eq!(most.load(Ordering::SeqCst), 3);
    assert!(
        queue
            .snapshot()
            .iter()
            .all(|job| job.state == UpgradeStates::UPGRADED)
    );
}

#[tokio::test]
async fn during_an_outage_a_job_waits_then_runs_once_soulseek_is_back() {
    let queue = Arc::new(UpgradeQueue::new());
    queue.add(asks(&["a"]), "alice", "app");
    let offline = Arc::new(AtomicBool::new(true));
    let tries = Arc::new(AtomicUsize::new(0));
    let (out, tried) = (offline.clone(), tries.clone());
    let apply: Apply = Arc::new(move |_| {
        tried.fetch_add(1, Ordering::SeqCst);
        let answer = if out.load(Ordering::SeqCst) {
            LibraryActionOutcome {
                code: Some(LibraryActionCodes::SOULSEEK_OFFLINE.to_string()),
                ..outcome(LibraryActionState::Failed, OFFLINE_TEXT)
            }
        } else {
            outcome(LibraryActionState::Applied, "Replaced.")
        };
        async move { answer }.boxed()
    });
    let worker = worker(&queue, 1, apply, Some(offline.clone()));
    let ct = CancellationToken::new();

    worker.tick(&ct).await;
    worker.drain().await;
    let job = queue.snapshot().remove(0);
    assert_eq!(job.state, UpgradeStates::WAITING);
    assert_eq!(job.detail.as_deref(), Some("Waiting for Soulseek"));

    assert_eq!(worker.tick(&ct).await, 0, "still out: not tried again");
    offline.store(false, Ordering::SeqCst);
    assert_eq!(worker.tick(&ct).await, 1);
    worker.drain().await;
    assert_eq!(queue.snapshot()[0].state, UpgradeStates::UPGRADED);
    assert_eq!(tries.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_job_waiting_for_mornyes_check_says_so() {
    let queue = Arc::new(UpgradeQueue::new());
    queue.add(asks(&["a"]), "alice", "app");
    let words = "Waiting for Mornye's check, which it could not pass for Example; it allows its next try at 14:05 UTC.";
    let apply: Apply = Arc::new(move |_| {
        let answer = LibraryActionOutcome {
            code: Some(LibraryActionCodes::MORNYE_CHECK.to_string()),
            ..outcome(LibraryActionState::Failed, words)
        };
        async move { answer }.boxed()
    });
    let worker = worker(&queue, 1, apply, None);
    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;
    let job = queue.snapshot().remove(0);
    assert_eq!(job.state, UpgradeStates::WAITING);
    assert_eq!(job.detail.as_deref(), Some(words));
}

#[test]
fn outcomes_map_on_the_code_not_the_words() {
    let cases = [
        (LibraryActionState::Applied, None, UpgradeStates::UPGRADED),
        (LibraryActionState::Rehearsed, None, UpgradeStates::REHEARSED),
        (
            LibraryActionState::Failed,
            Some(LibraryActionCodes::SOULSEEK_OFFLINE),
            UpgradeStates::WAITING,
        ),
        (
            LibraryActionState::Failed,
            Some(LibraryActionCodes::MORNYE_CHECK),
            UpgradeStates::WAITING,
        ),
        (
            LibraryActionState::Failed,
            Some(LibraryActionCodes::NO_REPLACEMENT),
            UpgradeStates::NOT_FOUND,
        ),
        (LibraryActionState::Skipped, None, UpgradeStates::SKIPPED),
        (LibraryActionState::Unresolved, None, UpgradeStates::FAILED),
        (LibraryActionState::Failed, None, UpgradeStates::FAILED),
    ];
    for (state, code, expected) in cases {
        let outcome = LibraryActionOutcome {
            code: code.map(str::to_string),
            ..outcome(state, "No Soulseek FLAC found, whatever")
        };
        assert_eq!(UpgradeWorker::state_for(&outcome), expected, "{state:?} {code:?}");
    }
}

#[tokio::test]
async fn the_replacements_download_is_written_on_the_job() {
    let queue = Arc::new(UpgradeQueue::new());
    queue.add(asks(&["a"]), "alice", "app");
    let apply: Apply = Arc::new(|request| {
        if let Some(queued) = &request.on_replacement_queued {
            queued("soulseek", "ext-1");
        }
        assert_eq!(request.action, LibraryAction::BetterQuality);
        assert_eq!(request.username, "alice");
        assert!(request.on_replacement_queued.is_some());
        async { outcome(LibraryActionState::Applied, "ok") }.boxed()
    });
    let worker = worker(&queue, 1, apply, None);
    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;
    assert_eq!(
        queue.snapshot()[0].acquisition_key.as_deref(),
        Some("soulseek:ext-1")
    );
}

#[tokio::test]
async fn a_song_with_no_copy_anywhere_is_not_retried_by_the_weekly_run() {
    let store = Arc::new(QualityUpgradeStore::new());
    let queue = Arc::new(UpgradeQueue::new());
    queue.add(
        vec![UpgradeAsk {
            navidrome_id: "a".into(),
            attempt_key: Some("Artist/a.mp3|100".into()),
            ..Default::default()
        }],
        "alice",
        "page",
    );
    let apply: Apply = Arc::new(|_| {
        async {
            LibraryActionOutcome {
                code: Some(LibraryActionCodes::NO_REPLACEMENT.to_string()),
                ..outcome(LibraryActionState::Failed, "nothing")
            }
        }
        .boxed()
    });
    let worker = Arc::new(UpgradeWorker::with_seams(
        queue.clone(),
        seams(1, apply, Arc::default()),
        None,
        Some(store.clone()),
        None,
    ));
    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;
    assert!(store.snapshot().attempts.contains_key("Artist/a.mp3|100"));
    assert_eq!(
        queue.snapshot()[0].detail.as_deref(),
        Some("No lossless copy of this song on Soulseek right now. Your copy is unchanged.")
    );
}

/// `AudioFixtures.Mp3`: twenty silent MPEG-1 Layer III frames, 128 kbps, 44.1 kHz.
fn mp3() -> Vec<u8> {
    let mut bytes = vec![0u8; 417 * 20];
    for frame in 0..20 {
        bytes[frame * 417..frame * 417 + 4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    }
    bytes
}

/// A FLAC file of only a STREAMINFO block describing `seconds` of 16-bit stereo, no frames.
fn flac_of(seconds: u64) -> Vec<u8> {
    let mut bytes = b"fLaC".to_vec();
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let packed: u64 = (44100u64 << 44) | (1u64 << 41) | (15u64 << 36) | (44100u64 * seconds);
    bytes.extend_from_slice(&packed.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes
}

fn soulseek(settings: SoulseekSettings) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: settings,
        ..Default::default()
    }))
}

fn idle_seams() -> UpgradeWorkerSeams {
    seams(
        1,
        Arc::new(|_| async { outcome(LibraryActionState::Failed, "unused") }.boxed()),
        Arc::default(),
    )
}

#[test]
fn an_upgrade_says_what_changed_and_what_it_passed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let new_file = dir.path().join("Song.flac");
    std::fs::write(&new_file, flac_of(200)).expect("written");
    let kept = dir.path().join(".octo-trash").join("2026-10-03").join("Song.mp3");
    std::fs::create_dir_all(kept.parent().expect("a folder")).expect("created");
    std::fs::write(&kept, mp3()).expect("written");
    let settings = soulseek(SoulseekSettings {
        verify_downloads: true,
        acoust_id_api_key: "key".into(),
        detect_transcodes: true,
        ..Default::default()
    });
    let worker = UpgradeWorker::with_seams(
        Arc::new(UpgradeQueue::new()),
        idle_seams(),
        None,
        None,
        Some(settings),
    );

    let result = worker.report(
        &LibraryActionOutcome {
            new_path: Some(new_file.to_string_lossy().into_owned()),
            quarantine_path: Some(kept.to_string_lossy().into_owned()),
            ..outcome(LibraryActionState::Applied, "Replaced.")
        },
        &UpgradeJob {
            started_utc: Some(Utc::now() - chrono::TimeDelta::seconds(68)),
            ..Default::default()
        },
    );

    assert_eq!(result.after.as_deref(), Some("FLAC 16-bit 44.1 kHz"));
    assert!(
        result.before.as_deref().is_some_and(|b| b.starts_with("MP3")),
        "{:?}",
        result.before
    );
    assert_eq!(result.new_file.as_deref(), Some("Song.flac"));
    assert_eq!(result.kept_at.as_deref(), Some(".octo-trash/2026-10-03"));
    assert_eq!(
        result.checks,
        [
            "the same length",
            "AcoustID: the same recording",
            "the spectrum: really lossless, not a converted MP3"
        ]
    );
    let seconds = result.seconds.expect("timed");
    assert!((67.0..=70.0).contains(&seconds), "{seconds}");
}

#[test]
fn without_acoust_id_the_report_does_not_claim_it() {
    let settings = soulseek(SoulseekSettings {
        verify_downloads: false,
        detect_transcodes: true,
        ..Default::default()
    });
    let worker = UpgradeWorker::with_seams(
        Arc::new(UpgradeQueue::new()),
        idle_seams(),
        None,
        None,
        Some(settings),
    );
    let result = worker.report(
        &outcome(LibraryActionState::Applied, "ok"),
        &UpgradeJob::default(),
    );
    assert!(!result.checks.iter().any(|check| check.contains("AcoustID")));
}

// ---- The state file, the limit and the words ---------------------------------------------

/// The state file reads and writes back byte for byte.
#[test]
fn the_upgrades_fixture_round_trips_byte_for_byte() {
    let original = std::fs::read(fixture("upgrades.json")).expect("the fixture");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("upgrades.json");
    std::fs::write(&path, &original).expect("copied");

    // The fixture was written on 2026-10-03; read it on that day, so its finished jobs are
    // still inside the keep window whatever day the test runs.
    let written = Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap();
    let queue = UpgradeQueue::with_path(Some(path.clone())).with_clock(Clock::fixed(written));
    // Add always writes, even when nothing was asked.
    queue.add(Vec::new(), "nobody", "app");
    assert_eq!(
        String::from_utf8(std::fs::read(&path).expect("written")).expect("text"),
        String::from_utf8(original).expect("text")
    );
    let jobs = queue.snapshot();
    assert_eq!(jobs[0].result.as_ref().and_then(|r| r.seconds), Some(269.0));
}

/// A file that cannot be read starts an empty queue (it is not set aside) and is overwritten on
/// the next change.
#[test]
fn an_unreadable_file_starts_an_empty_queue() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("upgrades.json");
    std::fs::write(&path, "{ not json").expect("written");
    let queue = UpgradeQueue::with_path(Some(path.clone()));
    assert!(queue.snapshot().is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).expect("listed").count(), 1);
    queue.add(asks(&["a"]), "alice", "app");
    assert!(
        std::fs::read_to_string(&path)
            .expect("read")
            .starts_with("[{\"NavidromeId\":\"a\"")
    );
}

#[test]
fn a_full_queue_refuses_with_a_reason() {
    let queue = UpgradeQueue::new();
    let many: Vec<UpgradeAsk> = (0..UpgradeQueue::MAX_OPEN_JOBS)
        .map(|i| ask(&i.to_string()))
        .collect();
    queue.add(many, "alice", "app");
    let (jobs, refused) = queue.add(asks(&["x", "y", "0"]), "alice", "app");
    assert_eq!(jobs.len(), 1, "the queued one answers as it stands");
    assert_eq!(
        refused.as_deref(),
        Some("2 songs were left out: 5000 are already waiting.")
    );
}

#[test]
fn kept_folders_and_sizes_keep_their_text() {
    assert_eq!(
        kept_folder("/music/.octo-trash/2026-10-03/a/b.mp3"),
        ".octo-trash/2026-10-03/a"
    );
    assert_eq!(kept_folder("/music/Kept/b.mp3"), "/music/Kept");
    assert_eq!(size(Some(31_457_280)), ", 30.0 MB");
    assert_eq!(size(Some(8_650_752)), ", 8.3 MB");
    assert_eq!(size(Some(0)), "");
    assert_eq!(size(None), "");
}
