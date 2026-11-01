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

use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    seams_with_mornye(width, apply, offline, Arc::new(|_| async { false }.boxed()))
}

/// The same, with Mornye's check keeping it from every extension while `kept` is set.
fn seams_with_mornye(
    width: i32,
    apply: Apply,
    offline: Arc<AtomicBool>,
    passed: ChecksPassFn,
) -> UpgradeWorkerSeams {
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
        mornye_passed: passed,
        source_name: Arc::new(|| "Soulseek".to_string()),
    }
}

/// An outcome Mornye's check kept, on the pause of check `label` that ends at `until`.
fn kept_on(label: &str, until: chrono::DateTime<chrono::Utc>) -> LibraryActionOutcome {
    LibraryActionOutcome {
        code: Some(LibraryActionCodes::MORNYE_KEPT.to_string()),
        mornye_wait: Some(Box::new(octo_core::spotiflac::check::CheckPause {
            until_utc: until,
            labels: vec![label.to_string()],
            holding: Vec::new(),
        })),
        ..outcome(LibraryActionState::Failed, "Waiting for Mornye's check.")
    }
}

/// A Mornye with two gateways, `A` paused and `B` passing, and the real seam reading it.
async fn mornye_with_a_paused_and_b_passing(paused_a: Arc<AtomicBool>) -> (MockServer, ChecksPassFn) {
    struct Sessions(Arc<AtomicBool>);
    impl wiremock::Respond for Sessions {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            let a = if self.0.load(Ordering::SeqCst) {
                ("unverified", 900.0)
            } else {
                ("active", 0.0)
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"sessions": [
                {"key": "gateway-a-1", "kind": "gateway", "label": "A", "extensions": ["a-ext"], "state": a.0, "auth_paused_s": a.1},
                {"key": "gateway-b-1", "kind": "gateway", "label": "B", "extensions": ["b-ext"], "state": "active", "auth_paused_s": 0.0}
            ]}}))
        }
    }
    let server = MockServer::builder().start().await;
    Mock::given(path("/api/get_signed_sessions"))
        .respond_with(Sessions(paused_a))
        .mount(&server)
        .await;
    Mock::given(path("/api/get_download_services"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"services": [
                {"id": "ext:a-ext", "label": "A", "extensions": ["a-ext"]},
                {"id": "ext:b-ext", "label": "B", "extensions": ["b-ext"]}
            ]}})),
        )
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(octo_core::settings::AppSettings {
        spotiflac: octo_core::settings::SpotiflacSettings {
            enabled: true,
            url: server.uri(),
            ..Default::default()
        },
        ..Default::default()
    }));
    // A read is fresh for no time here, so each look reads Mornye again.
    let check = crate::services::spotiflac::MornyeCheck::new(
        Arc::new(crate::services::spotiflac::MornyeCheckStore::new(None)),
        crate::services::spotiflac::CheckTimes {
            fresh_for: std::time::Duration::ZERO,
            call_bound: std::time::Duration::from_secs(5),
        },
        None,
    );
    let client =
        Arc::new(crate::services::spotiflac::SpotiflacClient::new(Arc::clone(&settings)).with_check(check));
    let sources = Arc::new(UpgradeSources::new(settings, None).with_spotiflac(client));
    (server, mornye_passed_with(Some(sources)))
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

#[tokio::test]
async fn a_job_one_gateways_pause_kept_waits_for_that_pause_not_for_every_extension() {
    // slskd is up the whole time; gateway A's pause kept the song, gateway B passes but had none.
    let paused_a = Arc::new(AtomicBool::new(true));
    let (_mornye, passed) = mornye_with_a_paused_and_b_passing(paused_a.clone()).await;
    let queue = Arc::new(UpgradeQueue::new());
    queue.add(asks(&["a"]), "alice", "app");
    let tries = Arc::new(AtomicUsize::new(0));
    let tried = tries.clone();
    let until = chrono::Utc::now() + chrono::Duration::minutes(15);
    let apply: Apply = Arc::new(move |_| {
        tried.fetch_add(1, Ordering::SeqCst);
        let answer = kept_on("A", until);
        async move { answer }.boxed()
    });
    let worker = Arc::new(UpgradeWorker::with_seams(
        queue.clone(),
        seams_with_mornye(1, apply, Arc::default(), passed),
        None,
        None,
        None,
    ));
    let ct = CancellationToken::new();
    for _ in 0..3 {
        worker.tick(&ct).await;
        worker.drain().await;
    }
    assert_eq!(
        tries.load(Ordering::SeqCst),
        1,
        "never again while A's pause holds"
    );
    let job = queue.snapshot().remove(0);
    assert_eq!(
        (job.state.as_str(), job.wait.as_deref()),
        (UpgradeStates::WAITING, Some(LibraryActionCodes::MORNYE_KEPT))
    );
    assert_eq!(
        job.mornye_hold.as_ref().map(|hold| hold.labels.clone()),
        Some(vec!["A".to_string()])
    );

    // Someone passes A's check: the job goes back at once.
    paused_a.store(false, Ordering::SeqCst);
    worker.tick(&ct).await;
    worker.drain().await;
    assert_eq!(tries.load(Ordering::SeqCst), 2, "once A's check passes");
}

#[tokio::test]
async fn once_a_pause_ends_one_kept_job_tries_and_the_others_follow_its_answer() {
    let queue = Arc::new(UpgradeQueue::new());
    queue.add(asks(&["a", "b", "c"]), "alice", "app");
    let tries = Arc::new(AtomicUsize::new(0));
    let tried = tries.clone();
    let ended = chrono::Utc::now() - chrono::Duration::seconds(1);
    let next = chrono::Utc::now() + chrono::Duration::minutes(45);
    // The first round is kept on a pause that already ended; every later try fails again.
    let apply: Apply = Arc::new(move |_| {
        let n = tried.fetch_add(1, Ordering::SeqCst);
        let answer = kept_on("A", if n < 3 { ended } else { next });
        async move { answer }.boxed()
    });
    let worker = Arc::new(UpgradeWorker::with_seams(
        queue.clone(),
        seams_with_mornye(3, apply, Arc::default(), Arc::new(|_| async { false }.boxed())),
        None,
        None,
        None,
    ));
    let ct = CancellationToken::new();
    worker.tick(&ct).await;
    worker.drain().await;
    assert_eq!(tries.load(Ordering::SeqCst), 3, "the first round of all three");

    for _ in 0..3 {
        worker.tick(&ct).await;
        worker.drain().await;
    }
    assert_eq!(
        tries.load(Ordering::SeqCst),
        4,
        "one scout, whose try failed again"
    );
    assert!(
        queue.snapshot().iter().all(|job| job
            .mornye_hold
            .as_ref()
            .is_some_and(|hold| hold.until_utc == next)),
        "the others now wait for the scout's new pause"
    );
}

#[tokio::test]
async fn a_restart_while_a_scout_is_on_its_way_sends_no_second_one() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("upgrades.json");
    let ended = chrono::Utc::now() - chrono::Duration::seconds(1);
    {
        let before = UpgradeQueue::with_path(Some(path.clone()));
        before.add(asks(&["a", "b"]), "alice", "app");
        // a went back alone when its pause ended and was running at the restart; b still waits.
        before.update("a", |job| {
            job.state = UpgradeStates::WORKING.to_string();
            job.mornye_scout = true;
        });
        before.update("b", |job| {
            job.state = UpgradeStates::WAITING.to_string();
            job.wait = Some(LibraryActionCodes::MORNYE_KEPT.to_string());
            job.mornye_hold = Some(MornyeHold {
                labels: vec!["A".to_string()],
                ..MornyeHold::until(ended)
            });
        });
    }
    let queue = Arc::new(UpgradeQueue::with_path(Some(path)));
    let tries = Arc::new(AtomicUsize::new(0));
    let tried = tries.clone();
    let apply: Apply = Arc::new(move |_| {
        tried.fetch_add(1, Ordering::SeqCst);
        async { outcome(LibraryActionState::Applied, "Upgraded") }.boxed()
    });
    let worker = worker(&queue, 3, apply, None);
    let ct = CancellationToken::new();

    worker.tick(&ct).await;
    worker.drain().await;
    assert_eq!(
        tries.load(Ordering::SeqCst),
        1,
        "only the scout from before the restart"
    );
    let b = queue
        .snapshot()
        .into_iter()
        .find(|job| job.navidrome_id == "b")
        .expect("b");
    assert_eq!(b.state, UpgradeStates::WAITING);

    // The scout's try is known, so b is the next one to go.
    worker.tick(&ct).await;
    worker.drain().await;
    assert_eq!(tries.load(Ordering::SeqCst), 2);
    assert!(queue.snapshot().iter().all(|job| !job.mornye_scout));
}

#[test]
fn a_finished_run_moves_the_holds_it_shares_a_check_with_and_leaves_the_rest() {
    let queue = UpgradeQueue::new();
    queue.add(asks(&["a", "b", "c"]), "alice", "app");
    let early = Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap();
    let later = Utc.with_ymd_and_hms(2026, 11, 1, 13, 0, 0).unwrap();
    for (id, label) in [("b", "A"), ("c", "B")] {
        queue.update(id, |job| {
            job.state = UpgradeStates::WAITING.to_string();
            job.wait = Some(LibraryActionCodes::MORNYE_KEPT.to_string());
            job.mornye_hold = Some(MornyeHold {
                labels: vec![label.to_string()],
                ..MornyeHold::until(early)
            });
        });
    }
    queue.update("a", |job| job.mornye_scout = true);

    queue.finish_run("a", |job| {
        job.state = UpgradeStates::WAITING.to_string();
        job.wait = Some(LibraryActionCodes::MORNYE_KEPT.to_string());
        job.mornye_hold = Some(MornyeHold {
            labels: vec!["A".to_string()],
            ..MornyeHold::until(later)
        });
    });

    let jobs = queue.snapshot();
    let hold = |id: &str| {
        jobs.iter()
            .find(|job| job.navidrome_id == id)
            .and_then(|job| job.mornye_hold.as_ref())
            .map(|hold| hold.until_utc)
    };
    assert!(jobs.iter().all(|job| !job.mornye_scout), "a's try is known");
    assert_eq!(hold("b"), Some(later), "b shares check A");
    assert_eq!(hold("c"), Some(early), "c waits on another check");
}

/// Holds job `id` on Mornye's check `label` until `until`.
fn hold_on(queue: &UpgradeQueue, id: &str, label: &str, until: chrono::DateTime<Utc>) {
    queue.update(id, |job| {
        job.state = UpgradeStates::WAITING.to_string();
        job.wait = Some(LibraryActionCodes::MORNYE_KEPT.to_string());
        job.mornye_hold = Some(MornyeHold {
            labels: vec![label.to_string()],
            ..MornyeHold::until(until)
        });
    });
}

#[test]
fn a_wait_read_from_mornyes_words_moves_the_holds_on_its_extensions_checks_only() {
    let queue = UpgradeQueue::new();
    queue.add(asks(&["a", "b", "c"]), "alice", "app");
    let early = Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap();
    let later = Utc.with_ymd_and_hms(2026, 11, 1, 13, 0, 0).unwrap();
    hold_on(&queue, "b", "Qobuz", early);
    hold_on(&queue, "c", "Tidal", early);

    // Mornye's words kept a's song on the Qobuz extension while its gateway read as working:
    // nothing to watch, but it shares Qobuz's check.
    queue.finish_run("a", |job| {
        job.state = UpgradeStates::WAITING.to_string();
        job.wait = Some(LibraryActionCodes::MORNYE_KEPT.to_string());
        job.mornye_hold = Some(MornyeHold {
            holding: vec!["Qobuz".to_string()],
            ..MornyeHold::until(later)
        });
    });

    let jobs = queue.snapshot();
    let hold = |id: &str| {
        jobs.iter()
            .find(|job| job.navidrome_id == id)
            .and_then(|job| job.mornye_hold.as_ref())
            .map(|hold| hold.until_utc)
    };
    assert_eq!(hold("b"), Some(later), "b waits on the same check");
    assert_eq!(hold("c"), Some(early), "Tidal's own pause is left alone");
}

#[test]
fn one_ended_job_goes_back_as_the_scout_and_passing_ones_go_back_unmarked() {
    let queue = UpgradeQueue::new();
    queue.add(asks(&["a", "b", "c"]), "alice", "app");
    let now = Utc::now();
    hold_on(&queue, "a", "A", now - chrono::Duration::seconds(1));
    hold_on(&queue, "b", "A", now - chrono::Duration::seconds(1));
    hold_on(&queue, "c", "B", now + chrono::Duration::minutes(30));
    let judged = queue
        .snapshot()
        .into_iter()
        .find(|job| job.navidrome_id == "c")
        .and_then(|job| job.mornye_hold)
        .expect("c's hold");
    let passing: HashMap<String, MornyeHold> = [("c".to_string(), judged)].into();

    assert_eq!(queue.release_mornye(&passing, now), 2);
    let jobs = queue.snapshot();
    let job = |id: &str| {
        jobs.iter()
            .find(|job| job.navidrome_id == id)
            .cloned()
            .expect("a job")
    };
    assert_eq!(
        (job("a").state.as_str(), job("a").mornye_scout),
        (UpgradeStates::QUEUED, true)
    );
    assert_eq!(
        (job("c").state.as_str(), job("c").mornye_scout),
        (UpgradeStates::QUEUED, false)
    );
    assert_eq!(job("b").state, UpgradeStates::WAITING);

    // While a is on its way, b's ended pause sends nothing more.
    assert_eq!(queue.release_mornye(&HashMap::new(), now), 0);
    assert!(queue.mornye_scout_on_its_way());
}

#[tokio::test]
async fn a_pass_read_before_a_hold_moved_does_not_let_that_job_go() {
    let queue = Arc::new(UpgradeQueue::new());
    queue.add(asks(&["a", "b"]), "alice", "app");
    let next = Utc::now() + chrono::Duration::minutes(45);
    queue.update("a", |job| {
        job.state = UpgradeStates::WORKING.to_string();
        job.mornye_scout = true;
    });
    hold_on(&queue, "b", "A", Utc::now() + chrono::Duration::minutes(5));
    // The read says A passes, and while it is read a's try comes back held on A's new pause.
    let finishing = queue.clone();
    let passed: ChecksPassFn = Arc::new(move |_| {
        finishing.finish_run("a", |job| {
            job.state = UpgradeStates::WAITING.to_string();
            job.wait = Some(LibraryActionCodes::MORNYE_KEPT.to_string());
            job.mornye_hold = Some(MornyeHold {
                labels: vec!["A".to_string()],
                ..MornyeHold::until(next)
            });
        });
        async { true }.boxed()
    });
    let tries = Arc::new(AtomicUsize::new(0));
    let tried = tries.clone();
    let apply: Apply = Arc::new(move |_| {
        tried.fetch_add(1, Ordering::SeqCst);
        async { outcome(LibraryActionState::Applied, "Upgraded") }.boxed()
    });
    let worker = Arc::new(UpgradeWorker::with_seams(
        queue.clone(),
        seams_with_mornye(2, apply, Arc::default(), passed),
        None,
        None,
        None,
    ));

    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;

    assert_eq!(tries.load(Ordering::SeqCst), 0, "b's hold changed after the read");
    let b = queue
        .snapshot()
        .into_iter()
        .find(|job| job.navidrome_id == "b")
        .expect("b");
    assert_eq!(b.state, UpgradeStates::WAITING);
}

#[tokio::test]
async fn a_scout_that_finishes_while_the_checks_are_read_sends_no_second_one_on_the_old_end() {
    let queue = Arc::new(UpgradeQueue::new());
    queue.add(asks(&["a", "b"]), "alice", "app");
    let ended = Utc::now() - chrono::Duration::seconds(1);
    let next = Utc::now() + chrono::Duration::minutes(45);
    // a is the scout, running; b waits on the same check's pause, which has ended.
    queue.update("a", |job| {
        job.state = UpgradeStates::WORKING.to_string();
        job.mornye_scout = true;
    });
    hold_on(&queue, "b", "A", ended);
    // While b's checks are read, a's try comes back held again on a later pause.
    let finishing = queue.clone();
    let passed: ChecksPassFn = Arc::new(move |_| {
        finishing.finish_run("a", |job| {
            job.state = UpgradeStates::WAITING.to_string();
            job.wait = Some(LibraryActionCodes::MORNYE_KEPT.to_string());
            job.mornye_hold = Some(MornyeHold {
                labels: vec!["A".to_string()],
                ..MornyeHold::until(next)
            });
        });
        async { false }.boxed()
    });
    let tries = Arc::new(AtomicUsize::new(0));
    let tried = tries.clone();
    let apply: Apply = Arc::new(move |_| {
        tried.fetch_add(1, Ordering::SeqCst);
        async { outcome(LibraryActionState::Applied, "Upgraded") }.boxed()
    });
    let worker = Arc::new(UpgradeWorker::with_seams(
        queue.clone(),
        seams_with_mornye(2, apply, Arc::default(), passed),
        None,
        None,
        None,
    ));

    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;

    assert_eq!(tries.load(Ordering::SeqCst), 0, "b waits for a's new pause");
    let b = queue
        .snapshot()
        .into_iter()
        .find(|job| job.navidrome_id == "b")
        .expect("b");
    assert_eq!(b.state, UpgradeStates::WAITING);
    assert_eq!(b.mornye_hold.map(|hold| hold.until_utc), Some(next));
}

#[test]
fn soulseek_coming_back_leaves_the_jobs_mornyes_check_kept() {
    let queue = UpgradeQueue::new();
    queue.add(asks(&["a", "b"]), "alice", "app");
    queue.update("a", |job| {
        job.state = UpgradeStates::WAITING.to_string();
        job.wait = Some(LibraryActionCodes::SOULSEEK_OFFLINE.to_string());
    });
    hold_on(&queue, "b", "A", Utc::now() - chrono::Duration::seconds(1));
    assert_eq!(queue.requeue(), 1);
    let b = queue
        .snapshot()
        .into_iter()
        .find(|job| job.navidrome_id == "b")
        .expect("b");
    assert_eq!(b.state, UpgradeStates::WAITING);
}

#[test]
fn a_kept_jobs_hold_and_scout_mark_survive_a_restart() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("upgrades.json");
    let until = Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap();
    let hold = MornyeHold {
        labels: vec!["A".to_string()],
        holding: vec!["A".to_string(), "A two".to_string()],
        ..MornyeHold::until(until)
    };
    {
        let before = UpgradeQueue::with_path(Some(path.clone()));
        before.add(asks(&["a"]), "alice", "app");
        before.update("a", |job| {
            job.state = UpgradeStates::WAITING.to_string();
            job.wait = Some(LibraryActionCodes::MORNYE_KEPT.to_string());
            job.mornye_hold = Some(hold.clone());
            job.mornye_scout = true;
        });
    }
    let job = UpgradeQueue::with_path(Some(path)).snapshot().remove(0);
    assert_eq!(job.mornye_hold, Some(hold));
    assert_eq!(job.wait.as_deref(), Some(LibraryActionCodes::MORNYE_KEPT));
    assert!(job.mornye_scout);
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
            Some(LibraryActionCodes::MORNYE_KEPT),
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
