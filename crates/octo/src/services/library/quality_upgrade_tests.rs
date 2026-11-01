//! The weekly upgrade trickles lossy songs through Better quality while nothing else is
//! downloading (#70), and the download pipeline hands the upgrade search on.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::TimeZone;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::{AppSettings, DownloadSource, LibraryActionDefinition};
use parking_lot::Mutex;

use super::*;
use crate::services::common::{AcquisitionActivity, AcquisitionWorker, TrackAcquisitionQueue};
use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::ReplacementHandoff;
use crate::services::notifications::NotificationService;
use crate::services::soulseek::ExternalIdRegistry;

/// The download service of the pipeline tests: one acquisition answered when its arguments are
/// the upgrade's, and transfers reported running or not.
#[derive(Default)]
struct UpgradeDownloads {
    active: AtomicBool,
    calls: Mutex<Vec<(String, String, bool, bool, Option<DownloadSource>, bool)>>,
}

#[async_trait]
impl IDownloadService for UpgradeDownloads {
    async fn download_song(&self, _: &str, _: &str, _: &CancellationToken) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_and_stream(
        &self,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        anyhow::bail!("not set up")
    }

    fn download_remaining_album_tracks_in_background(&self, _: &str, _: &str, _: &str) {}

    async fn execute_acquisition(
        &self,
        provider: &str,
        external_id: &str,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        upgrade_search: bool,
        _: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        self.calls.lock().push((
            provider.to_string(),
            external_id.to_string(),
            trigger_album_download,
            force_permanent,
            source_override,
            upgrade_search,
        ));
        // Only this exact call is answered.
        if (
            provider,
            external_id,
            trigger_album_download,
            force_permanent,
            source_override,
            upgrade_search,
        ) == (
            "soulseek",
            "id-1",
            false,
            true,
            Some(DownloadSource::Soulseek),
            true,
        ) {
            Ok("/music/a.flac".to_string())
        } else {
            anyhow::bail!("not set up")
        }
    }

    async fn download_album_with_source(
        &self,
        _: &str,
        _: &str,
        _: DownloadSource,
        _: bool,
        _: &CancellationToken,
        _: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn get_download_status(&self, _: &str) -> Option<DownloadInfo> {
        None
    }

    fn has_active_downloads(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    async fn get_local_path_if_exists(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn get_direct_stream(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        Ok(None)
    }
}

#[tokio::test]
async fn the_worker_hands_the_source_and_upgrade_search_to_the_download() {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let downloads = Arc::new(UpgradeDownloads::default());
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let worker = Arc::new(AcquisitionWorker::new(
        queue.clone(),
        downloads.clone(),
        Arc::new(ExternalIdRegistry::new(None::<PathBuf>)),
        Arc::new(NotificationService::new(Vec::new(), settings, None)),
        None,
    ));
    let stopping = CancellationToken::new();
    let running = tokio::spawn(worker.run(stopping.clone()));
    let completion = queue.enqueue(
        "soulseek",
        "id-1",
        true,
        false,
        true,
        Some(DownloadSource::Soulseek),
        true,
        None,
        true,
        None,
    );
    let path = completion.wait().await;
    stopping.cancel();
    let _ = running.await;
    assert_eq!(
        path.ok().as_deref(),
        Some("/music/a.flac"),
        "{:?}",
        downloads.calls.lock()
    );
}

fn row(id: &str, path: &str) -> LibrarySongRow {
    row_of(id, path, "mp3", 5_000_000)
}

fn row_of(id: &str, path: &str, suffix: &str, size: i64) -> LibrarySongRow {
    LibrarySongRow {
        id: id.to_string(),
        path: path.to_string(),
        library_path: Some("/music".to_string()),
        size,
        suffix: suffix.to_string(),
        bit_rate: 320,
        title: "T".to_string(),
        artist: "A".to_string(),
        duration: Some(200),
        album: None,
        ..Default::default()
    }
}

fn attempt(at: DateTime<Utc>) -> QualityUpgradeAttempt {
    QualityUpgradeAttempt {
        at_utc: at,
        outcome: "Failed".to_string(),
        detail: None,
    }
}

#[test]
fn never_tried_first_then_oldest_and_lossless_is_ignored() {
    let now = Utc.with_ymd_and_hms(2026, 10, 2, 0, 0, 0).unwrap();
    let mut state = QualityUpgradeState::default();
    let key = |id: &str, path: &str| QualityUpgradeWorker::key_of(&row(id, path));
    state
        .attempts
        .set(key("b", "B/b.mp3"), attempt(now - TimeDelta::days(60)));
    state
        .attempts
        .set(key("c", "C/c.mp3"), attempt(now - TimeDelta::days(40)));
    state
        .attempts
        .set(key("e", "E/e.mp3"), attempt(now - TimeDelta::days(2)));
    let songs = [
        row_of("f", "F/f.flac", "flac", 5_000_000),
        row("c", "C/c.mp3"),
        row("b", "B/b.mp3"),
        row("e", "E/e.mp3"),
        row("a", "A/a.mp3"),
    ];
    assert_eq!(
        QualityUpgradeWorker::pick(&songs, &state, now, false).map(|s| s.id.as_str()),
        Some("a")
    );
    state.attempts.set(key("a", "A/a.mp3"), attempt(now));
    assert_eq!(
        QualityUpgradeWorker::pick(&songs, &state, now, false).map(|s| s.id.as_str()),
        Some("b")
    );
}

#[test]
fn the_key_survives_a_navidrome_id_change() {
    assert_eq!(
        QualityUpgradeWorker::key_of(&row("old-id", "A/x.mp3")),
        QualityUpgradeWorker::key_of(&row("new-id", "A/x.mp3"))
    );
    assert_eq!(
        QualityUpgradeWorker::key_of(&row("1", "A/x.mp3")),
        QualityUpgradeWorker::key_of(&row("2", "/music/A/x.mp3"))
    );
    let now = Utc::now();
    let mut state = QualityUpgradeState::default();
    state.attempts.set(
        QualityUpgradeWorker::key_of(&row("old-id", "A/x.mp3")),
        attempt(now - TimeDelta::days(30)),
    );
    let songs = [row("new-id", "A/x.mp3"), row("fresh", "Z/z.mp3")];
    assert_eq!(
        QualityUpgradeWorker::pick(&songs, &state, now, false).map(|s| s.id.as_str()),
        Some("fresh")
    );
}

#[derive(Default)]
struct Calls {
    list: AtomicUsize,
    apply: AtomicUsize,
}

fn worker(
    per_week: i32,
    idle: bool,
    store: Option<Arc<QualityUpgradeStore>>,
) -> (Arc<QualityUpgradeWorker>, Arc<Calls>) {
    let settings = LibraryActionSettings {
        enabled: true,
        allowed_users: vec!["alice".into()],
        actions: vec![LibraryActionDefinition {
            action: LibraryAction::BetterQuality,
            enabled: true,
            ..Default::default()
        }],
        dry_run: false,
        upgrade_per_week: per_week,
        ..Default::default()
    };
    let calls = Arc::new(Calls::default());
    let (listed, applied) = (calls.clone(), calls.clone());
    let seams = QualityUpgradeSeams {
        list_songs: Arc::new(move || {
            listed.list.fetch_add(1, Ordering::SeqCst);
            async { (vec![row("a", "A/a.mp3")], true) }.boxed()
        }),
        apply: Arc::new(move |_| {
            applied.apply.fetch_add(1, Ordering::SeqCst);
            async {
                Ok(LibraryActionOutcome::new(
                    LibraryActionState::Failed,
                    Some("nothing better".into()),
                ))
            }
            .boxed()
        }),
        has_admin_identity: Arc::new(|| true),
        acquisitions_idle: Arc::new(move || idle),
        ..QualityUpgradeSeams::idle()
    };
    let store = store.unwrap_or_else(|| Arc::new(QualityUpgradeStore::new()));
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        library_actions: settings,
        ..Default::default()
    }));
    (
        Arc::new(QualityUpgradeWorker::with_seams(store, settings, None, seams)),
        calls,
    )
}

#[tokio::test]
async fn zero_is_off() {
    assert!(QualityUpgradeWorker::interval(0).is_none());
    let (w, calls) = worker(0, true, None);
    assert_eq!(w.tick().await, Tick::Off);
    assert_eq!(calls.list.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn it_waits_while_a_heart_is_queued_or_running() {
    let (w, calls) = worker(7, false, None);
    assert_eq!(w.tick().await, Tick::Busy);
    assert_eq!(calls.apply.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn during_a_soulseek_outage_no_song_is_tried_and_the_run_is_not_spent() {
    let store = Arc::new(QualityUpgradeStore::new());
    let (w, calls) = worker(7, true, Some(store.clone()));
    w.configure(|s| s.soulseek_offline = Arc::new(|| async { true }.boxed()));

    assert_eq!(w.tick().await, Tick::Offline);
    assert_eq!(calls.apply.load(Ordering::SeqCst), 0);
    assert!(store.snapshot().last_run_utc.is_none());

    // Back online, the same tick runs as normal.
    w.configure(|s| s.soulseek_offline = Arc::new(|| async { false }.boxed()));
    assert_eq!(w.tick().await, Tick::Ran);
    assert_eq!(calls.apply.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_song_mornyes_check_kept_is_not_put_off_for_four_weeks() {
    let store = Arc::new(QualityUpgradeStore::new());
    let (w, calls) = worker(7, true, Some(store.clone()));
    let applied = calls.clone();
    w.configure(|s| {
        s.apply = Arc::new(move |_| {
            applied.apply.fetch_add(1, Ordering::SeqCst);
            async {
                Ok(LibraryActionOutcome {
                    code: Some(LibraryActionCodes::MORNYE_KEPT.to_string()),
                    ..LibraryActionOutcome::new(
                        LibraryActionState::Failed,
                        Some("Waiting for Mornye's check.".into()),
                    )
                })
            }
            .boxed()
        })
    });

    assert_eq!(w.tick().await, Tick::Offline);
    assert_eq!(calls.apply.load(Ordering::SeqCst), 1);
    let state = store.snapshot();
    assert!(state.attempts.is_empty(), "the song is not stamped");
    assert_eq!(state.last_outcome.as_deref(), Some("Waiting for Mornye's check"));
}

#[tokio::test]
async fn a_kept_song_is_passed_over_so_the_next_run_tries_the_next_song() {
    let store = Arc::new(QualityUpgradeStore::new());
    let (w, _) = worker(7, true, Some(store.clone()));
    let now = Arc::new(Mutex::new(chrono::Utc::now()));
    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let (clock_now, seen) = (now.clone(), asked.clone());
    let until = chrono::Utc::now() + chrono::TimeDelta::days(30);
    w.configure(|s| {
        s.clock = Clock::new(move || *clock_now.lock());
        s.list_songs = Arc::new(|| async { (vec![row("a", "A/a.mp3"), row("b", "B/b.mp3")], true) }.boxed());
        s.apply = Arc::new(move |request| {
            seen.lock().push(request.navidrome_id.clone());
            let kept = request.navidrome_id == "a";
            async move {
                Ok(if kept {
                    LibraryActionOutcome {
                        code: Some(LibraryActionCodes::MORNYE_KEPT.to_string()),
                        mornye_wait: Some(Box::new(octo_core::spotiflac::check::CheckPause {
                            until_utc: until,
                            labels: vec!["Example".into()],
                            holding: Vec::new(),
                        })),
                        ..LibraryActionOutcome::new(
                            LibraryActionState::Failed,
                            Some("Waiting for Mornye's check.".into()),
                        )
                    }
                } else {
                    LibraryActionOutcome::new(LibraryActionState::Failed, Some("nothing better".into()))
                })
            }
            .boxed()
        });
    });

    assert_eq!(w.tick().await, Tick::Offline);
    *now.lock() += chrono::TimeDelta::days(2);
    assert_eq!(w.tick().await, Tick::Ran);

    assert_eq!(
        *asked.lock(),
        vec!["a".to_string(), "b".to_string()],
        "the kept song does not block the run"
    );
    assert!(
        store
            .snapshot()
            .mornye_holds
            .contains_key(&QualityUpgradeWorker::key_of(&row("a", "A/a.mp3")))
    );

    // Once a's pause has ended, the next run picks a again.
    *now.lock() += chrono::TimeDelta::days(31);
    w.tick().await;
    assert_eq!(
        *asked.lock(),
        vec!["a".to_string(), "b".to_string(), "a".to_string()]
    );
}

#[test]
fn the_weekly_runs_holds_survive_a_restart() {
    let mut state = QualityUpgradeState::default();
    let hold = MornyeHold {
        until_utc: chrono::Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        labels: vec!["Example".into()],
        holding: vec!["Example".into(), "Example two".into()],
    };
    state.mornye_holds.set("A/a.mp3".to_string(), hold.clone());
    let text = serde_json::to_string(&state).expect("writes");
    let back: QualityUpgradeState = serde_json::from_str(&text).expect("reads");
    assert_eq!(back.mornye_holds.get("A/a.mp3"), Some(&hold));
}

#[tokio::test]
async fn when_navidrome_cannot_list_the_library_the_weeks_run_is_not_spent() {
    let store = Arc::new(QualityUpgradeStore::new());
    let (w, calls) = worker(7, true, Some(store.clone()));
    let answering = Arc::new(AtomicBool::new(false));
    let (listed, up) = (calls.clone(), answering.clone());
    w.configure(|s| {
        s.list_songs = Arc::new(move || {
            listed.list.fetch_add(1, Ordering::SeqCst);
            let up = up.load(Ordering::SeqCst);
            async move {
                if up {
                    (vec![row("a", "A/a.mp3")], true)
                } else {
                    (Vec::new(), false)
                }
            }
            .boxed()
        })
    });

    assert_eq!(w.tick().await, Tick::Unreachable);
    assert!(store.snapshot().last_run_utc.is_none());
    assert!(store.snapshot().last_outcome.is_none());

    answering.store(true, Ordering::SeqCst);
    assert_eq!(w.tick().await, Tick::Ran);
    assert_eq!(calls.apply.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn state_survives_a_restart_and_the_next_run_waits_its_turn() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("quality-upgrade.json");
    let (first, _) = worker(
        7,
        true,
        Some(Arc::new(QualityUpgradeStore::with_path(Some(path.clone())))),
    );
    assert_eq!(first.tick().await, Tick::Ran);
    let reopened = Arc::new(QualityUpgradeStore::with_path(Some(path)));
    assert_eq!(reopened.snapshot().attempts.len(), 1);
    let (second, calls) = worker(7, true, Some(reopened));
    assert_eq!(second.tick().await, Tick::NotDue);
    assert_eq!(calls.apply.load(Ordering::SeqCst), 0);
}

#[test]
fn a_queued_star_counts_as_a_download_in_flight() {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let activity = AcquisitionActivity::new(queue.clone());
    assert!(!activity.is_busy());
    let _ = queue.enqueue("deezer", "1", true, false, false, None, true, None, false, None);
    assert!(activity.is_busy());
}

#[test]
fn a_running_transfer_counts_as_a_download_in_flight() {
    let downloads: Arc<dyn IDownloadService> = Arc::new(UpgradeDownloads {
        active: AtomicBool::new(true),
        ..Default::default()
    });
    let activity = AcquisitionActivity::new(Arc::new(TrackAcquisitionQueue::new()));
    activity.set_downloads(&downloads);
    assert!(activity.is_busy());
}

// ---- The state file and the status -------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/rust-migration/fixtures/state")
        .join(name)
}

/// The state file reads and writes back byte for byte, its keys in their order.
#[test]
fn the_quality_upgrade_fixture_round_trips_byte_for_byte() {
    let original = std::fs::read_to_string(fixture("quality-upgrade.json")).expect("the fixture");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("quality-upgrade.json");
    std::fs::write(&path, &original).expect("copied");

    let store = QualityUpgradeStore::with_path(Some(path.clone()));
    assert_eq!(store.snapshot().attempts.len(), 2);
    store.update(|_| {});
    assert_eq!(std::fs::read_to_string(&path).expect("written"), original);
}

#[test]
fn an_unreadable_file_starts_empty() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("quality-upgrade.json");
    std::fs::write(&path, "[1,").expect("written");
    let store = QualityUpgradeStore::with_path(Some(path));
    assert!(store.snapshot().attempts.is_empty());
    assert!(store.snapshot().last_run_utc.is_none());
}

#[test]
fn why_off_names_the_first_missing_piece() {
    let on = LibraryActionSettings {
        enabled: true,
        allowed_users: vec![" ".into(), " alice ".into()],
        actions: vec![LibraryActionDefinition {
            action: LibraryAction::BetterQuality,
            enabled: true,
            ..Default::default()
        }],
        upgrade_per_week: 3,
        ..Default::default()
    };
    assert_eq!(QualityUpgradeWorker::why_off(&on, true, true), None);
    assert_eq!(QualityUpgradeWorker::acting_user(&on).as_deref(), Some("alice"));
    assert_eq!(
        QualityUpgradeWorker::why_off(&on, true, false),
        Some("Better quality has no source set up: it needs slskd or Lidarr.")
    );
    assert_eq!(
        QualityUpgradeWorker::why_off(&on, false, true),
        Some("Octo needs a Navidrome admin credential to read the library.")
    );
    let nobody = LibraryActionSettings {
        allowed_users: Vec::new(),
        ..on.clone()
    };
    assert_eq!(
        QualityUpgradeWorker::why_off(&nobody, true, true),
        Some("Nobody is on the allowed list.")
    );
    assert_eq!(
        QualityUpgradeWorker::interval(7),
        Some(TimeDelta::days(1)),
        "a week over seven runs"
    );
    assert_eq!(
        QualityUpgradeWorker::interval(3),
        Some(TimeDelta::nanoseconds(201_600_000_000_000)),
        "ticks divided as integers"
    );
}

#[tokio::test]
async fn status_says_when_the_next_run_is_due() {
    let (w, _) = worker(7, true, None);
    let at = Utc.with_ymd_and_hms(2026, 10, 3, 0, 0, 0).unwrap();
    w.configure(|s| s.clock = Clock::fixed(at));
    let before = w.status();
    assert_eq!(before.next_due_utc, Some(at));
    assert_eq!(before.per_week, 7);
    assert_eq!(w.tick().await, Tick::Ran);
    let after = w.status();
    assert_eq!(after.next_due_utc, Some(at + TimeDelta::days(1)));
    assert_eq!(after.last_outcome.as_deref(), Some("Failed"));
    assert_eq!(after.tried, 1);
}

/// While members have libraries of their own (Family on, or turned off since), the weekly
/// upgrade never walks a member's own library.
#[tokio::test]
async fn quality_upgrades_keep_to_the_shared_library() {
    let worker_with = |family_on: bool, members: &[(&str, i64)]| {
        let mut shared = row("a", "Artist/a.mp3");
        shared.library_path = Some("/music".into());
        let mut alexs = row("b", "Artist/b.mp3");
        alexs.library_path = Some("/members/alex".into());
        let seams = QualityUpgradeSeams {
            list_songs: Arc::new(move || {
                let rows = vec![shared.clone(), alexs.clone()];
                async move { (rows, true) }.boxed()
            }),
            ..QualityUpgradeSeams::idle()
        };
        QualityUpgradeWorker::with_seams(
            Arc::new(QualityUpgradeStore::new()),
            Arc::new(SettingsStore::for_tests(AppSettings::default())),
            None,
            seams,
        )
        .with_library_scope(crate::services::family::library_index::scope_for_tests(
            family_on, members,
        ))
    };
    for family_on in [true, false] {
        let (songs, complete) = worker_with(family_on, &[("alex", 3)]).list_songs().await;
        assert_eq!(songs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["a"]);
        assert!(complete);
    }
    let (songs, _) = worker_with(false, &[]).list_songs().await;
    assert_eq!(songs.len(), 2);
}
