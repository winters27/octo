//! The songs asked to be found in higher quality (`<config>/upgrades.json`), the worker that runs
//! them, and `AudioSummary`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;
use octo_core::common::Clock;
use octo_core::common::acquisition_tracker::user_safe;
use octo_core::common::dotnet;
use octo_core::json::datetime;
use octo_core::settings::{DownloadSource, LibraryAction, SettingsStore, SoulseekSettings};
use octo_core::soulseek::soulseek_link::SoulseekLinkState;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::library_action_executor::{LibraryActionCodes, LibraryActionExecutor, LibraryActionRequest};
use super::quality_upgrade_worker::{QualityUpgradeAttempt, QualityUpgradeStore};
use super::{
    LibraryActionOutcome, LibraryActionState, NavidromeSongPathResolver, ResolvedSongFile, UpgradeSources,
};
use crate::services::common::acquisition_tracker::{acquisition_event_kinds, acquisition_kinds};
use crate::services::common::download_picks::{DownloadPicks, PickedCopy};
use crate::services::common::{AcquisitionTracker, DownloadConcurrency};
use crate::services::framework::DotnetDictionary;
use crate::services::soulseek::{ISoulseekLink, SoulseekMetadataService};
use crate::services::state_file;

/// The words a job's state goes by on the wire. A contract with the Octo apps.
pub struct UpgradeStates;

impl UpgradeStates {
    pub const QUEUED: &'static str = "queued";
    pub const WAITING: &'static str = "waiting";
    pub const WORKING: &'static str = "working";
    pub const UPGRADED: &'static str = "upgraded";
    pub const NOT_FOUND: &'static str = "notFound";
    pub const REHEARSED: &'static str = "rehearsed";
    pub const SKIPPED: &'static str = "skipped";
    pub const FAILED: &'static str = "failed";

    /// Still to run or running: not an answer yet.
    pub fn open(state: &str) -> bool {
        matches!(state, Self::QUEUED | Self::WAITING | Self::WORKING)
    }
}

/// One song someone asked to find in higher quality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UpgradeJob {
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub navidrome_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub suffix: Option<String>,

    /// The weekly upgrade's key for this file (path and size), when the page knew it, so a
    /// song with no FLAC anywhere is not tried again by the weekly run for four weeks.
    #[serde(default)]
    pub attempt_key: Option<String>,

    /// Who it acts as: the person who asked in an app, or the admin signed in on the page.
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub requested_by: String,
    #[serde(default = "default_origin", deserialize_with = "state_file::null_as_default")]
    pub origin: String,
    #[serde(default = "default_state", deserialize_with = "state_file::null_as_default")]
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,

    /// provider:externalId of the replacement download, once it is queued.
    #[serde(default)]
    pub acquisition_key: Option<String>,
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub queued_utc: DateTime<Utc>,
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub updated_utc: DateTime<Utc>,

    /// When it started running, for how long it took.
    #[serde(default, with = "datetime::utc_option")]
    pub started_utc: Option<DateTime<Utc>>,

    /// What a finished job found and did, for a person to read.
    #[serde(default)]
    pub result: Option<UpgradeResult>,

    /// The copy someone picked in Find songs, fetched instead of searching; None to search.
    #[serde(default)]
    pub pick: Option<crate::services::common::download_picks::PickedCopy>,
}

fn default_origin() -> String {
    "app".to_string()
}

fn default_state() -> String {
    UpgradeStates::QUEUED.to_string()
}

impl Default for UpgradeJob {
    fn default() -> Self {
        UpgradeJob {
            navidrome_id: String::new(),
            title: None,
            artist: None,
            album: None,
            suffix: None,
            attempt_key: None,
            requested_by: String::new(),
            origin: default_origin(),
            state: default_state(),
            detail: None,
            acquisition_key: None,
            queued_utc: datetime::min_value(),
            updated_utc: datetime::min_value(),
            started_utc: None,
            result: None,
            pick: None,
        }
    }
}

/// What an upgrade did, in words: the file before and after, the checks the new one passed,
/// where the original is kept, and how long it took. Proof that "Upgraded" really means it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UpgradeResult {
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub before_bytes: Option<i64>,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub after_bytes: Option<i64>,
    #[serde(default)]
    pub new_file: Option<String>,
    #[serde(default)]
    pub kept_at: Option<String>,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub checks: Vec<String>,
    #[serde(default)]
    pub seconds: Option<f64>,
}

/// A song to look for, with what the asker already knows about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpgradeAsk {
    pub navidrome_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub suffix: Option<String>,
    pub attempt_key: Option<String>,
    /// The copy someone picked in Find songs, fetched instead of searching.
    pub pick: Option<PickedCopy>,
}

impl UpgradeAsk {
    /// An ask for one Navidrome id, everything else unknown.
    pub fn new(navidrome_id: impl Into<String>) -> Self {
        UpgradeAsk {
            navidrome_id: navidrome_id.into(),
            ..Default::default()
        }
    }
}

/// The songs waiting to be found in higher quality, from the apps and the Better quality page, on
/// disk so a restart keeps them. One job per song: asking again for a song already queued or
/// running changes nothing. A job left running by a restart is queued again, and the action
/// journal reconciles any replacement it had started. Finished jobs stay a week so the apps can
/// read how they went.
pub struct UpgradeQueue {
    path: Option<PathBuf>,
    jobs: Mutex<DotnetDictionary<UpgradeJob>>,
    clock: Clock,
}

impl Default for UpgradeQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl UpgradeQueue {
    /// Jobs still to run at once, across everyone. Past it a request is refused with a reason.
    pub const MAX_OPEN_JOBS: usize = 5000;
    pub const KEEP_FINISHED: TimeDelta = TimeDelta::days(7);

    /// A queue in memory only.
    pub fn new() -> Self {
        Self::with_path(None)
    }

    /// A queue kept in `path`, read now. None, or a blank path, keeps it in memory. A file that
    /// cannot be read is logged and the queue starts empty; it is overwritten on the next change.
    pub fn with_path(path: Option<PathBuf>) -> Self {
        let path = path.filter(|p| !dotnet::is_blank(&p.to_string_lossy()));
        let mut jobs = DotnetDictionary::new();
        if let Some(path) = &path
            && let Err(e) = Self::load(path, &mut jobs)
        {
            warn!("the upgrade queue could not be read: {e}");
        }
        UpgradeQueue {
            path,
            jobs: Mutex::new(jobs),
            clock: Clock::system(),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The `Clock` seam the tests set.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn load(path: &Path, jobs: &mut DotnetDictionary<UpgradeJob>) -> anyhow::Result<()> {
        let Some(text) = state_file::read_text(path)? else {
            return Ok(());
        };
        for mut job in serde_json::from_str::<Option<Vec<UpgradeJob>>>(&text)?.unwrap_or_default() {
            if job.state == UpgradeStates::WORKING {
                job.state = UpgradeStates::QUEUED.to_string();
            }
            jobs.set(job.navidrome_id.clone(), job);
        }
        Ok(())
    }

    /// Queues one job per song not already queued or running. Answers each song's job as it now
    /// stands, and a reason for the songs left out because the queue is full.
    pub fn add(
        &self,
        asks: Vec<UpgradeAsk>,
        requester: &str,
        origin: &str,
    ) -> (Vec<UpgradeJob>, Option<String>) {
        let mut jobs = self.jobs.lock();
        let now = self.clock.now();
        let mut open = jobs
            .values()
            .filter(|job| UpgradeStates::open(&job.state))
            .count();
        let mut answer = Vec::new();
        let mut refused = 0;
        let mut seen: Vec<String> = Vec::new();
        for ask in asks {
            // One ask per Navidrome id: the first ask for an id wins.
            if seen.contains(&ask.navidrome_id) {
                continue;
            }
            seen.push(ask.navidrome_id.clone());
            if dotnet::is_blank(&ask.navidrome_id) {
                continue;
            }
            if let Some(existing) = jobs.get(&ask.navidrome_id)
                && UpgradeStates::open(&existing.state)
            {
                answer.push(existing.clone());
                continue;
            }
            if open >= Self::MAX_OPEN_JOBS {
                refused += 1;
                continue;
            }
            let job = UpgradeJob {
                navidrome_id: ask.navidrome_id,
                title: ask.title,
                artist: ask.artist,
                album: ask.album,
                suffix: ask.suffix,
                attempt_key: ask.attempt_key,
                pick: ask.pick,
                requested_by: requester.to_string(),
                origin: origin.to_string(),
                state: UpgradeStates::QUEUED.to_string(),
                queued_utc: now,
                updated_utc: now,
                ..Default::default()
            };
            jobs.set(job.navidrome_id.clone(), job.clone());
            open += 1;
            answer.push(job);
        }
        self.save(&jobs);
        let refused = (refused > 0).then(|| {
            format!(
                "{refused} {} left out: {} are already waiting.",
                if refused == 1 { "song was" } else { "songs were" },
                Self::MAX_OPEN_JOBS
            )
        });
        (answer, refused)
    }

    /// Puts another picked copy on a job that has not started, for the person who asked for it.
    /// Answers the job as it now stands, or None when it has started or is someone else's: the
    /// copy it fetches is settled then.
    pub fn replace_pick(&self, navidrome_id: &str, requester: &str, pick: PickedCopy) -> Option<UpgradeJob> {
        let mut jobs = self.jobs.lock();
        let now = self.clock.now();
        let job = jobs.get_mut(navidrome_id).filter(|job| {
            matches!(job.state.as_str(), UpgradeStates::QUEUED | UpgradeStates::WAITING)
                && dotnet::eq_ignore_case(&job.requested_by, requester.trim())
        })?;
        job.pick = Some(pick);
        job.updated_utc = now;
        let copy = job.clone();
        self.save(&jobs);
        Some(copy)
    }

    /// Every job, as copies, oldest first (`Snapshot()`).
    pub fn snapshot(&self) -> Vec<UpgradeJob> {
        self.snapshot_for(None)
    }

    /// Every job, or only one person's, as copies, oldest first (`Snapshot(requester)`).
    pub fn snapshot_for(&self, requester: Option<&str>) -> Vec<UpgradeJob> {
        let mut jobs = self.jobs.lock();
        self.prune(&mut jobs);
        let mut mine: Vec<UpgradeJob> = jobs
            .values()
            .filter(|job| requester.is_none_or(|r| dotnet::eq_ignore_case(&job.requested_by, r)))
            .cloned()
            .collect();
        mine.sort_by_key(|job| job.queued_utc);
        mine
    }

    pub fn open_count(&self) -> usize {
        self.jobs
            .lock()
            .values()
            .filter(|job| UpgradeStates::open(&job.state))
            .count()
    }

    /// Takes back jobs that have not started. A running job runs to its end.
    pub fn cancel<S: AsRef<str>>(&self, ids: &[S]) -> usize {
        let mut jobs = self.jobs.lock();
        let mut removed = 0;
        for id in ids {
            let id = id.as_ref();
            let cancellable = jobs.get(id).is_some_and(|job| {
                matches!(job.state.as_str(), UpgradeStates::QUEUED | UpgradeStates::WAITING)
            });
            if cancellable && jobs.remove(id).is_some() {
                removed += 1;
            }
        }
        if removed > 0 {
            self.save(&jobs);
        }
        removed
    }

    /// Forgets every finished job.
    pub fn clear_finished(&self) -> usize {
        let mut jobs = self.jobs.lock();
        let finished: Vec<String> = jobs
            .values()
            .filter(|job| !UpgradeStates::open(&job.state))
            .map(|job| job.navidrome_id.clone())
            .collect();
        for id in &finished {
            jobs.remove(id);
        }
        if !finished.is_empty() {
            self.save(&jobs);
        }
        finished.len()
    }

    /// The oldest queued job, now working, or None.
    pub fn take_next(&self) -> Option<UpgradeJob> {
        let mut jobs = self.jobs.lock();
        let next = jobs
            .values()
            .filter(|job| job.state == UpgradeStates::QUEUED)
            // The earliest queued, the first of equals.
            .fold(None::<&UpgradeJob>, |best, job| match best {
                Some(best) if best.queued_utc <= job.queued_utc => Some(best),
                _ => Some(job),
            })?
            .navidrome_id
            .clone();
        let started = self.clock.now();
        let updated = self.clock.now();
        let job = jobs.get_mut(&next).expect("the job was just found");
        job.state = UpgradeStates::WORKING.to_string();
        job.detail = None;
        job.started_utc = Some(started);
        job.result = None;
        job.updated_utc = updated;
        let copy = job.clone();
        self.save(&jobs);
        Some(copy)
    }

    /// Waiting jobs back in the queue, once Soulseek is back.
    pub fn requeue(&self) -> usize {
        let mut jobs = self.jobs.lock();
        let mut count = 0;
        for job in jobs.values_mut() {
            if job.state == UpgradeStates::WAITING {
                job.state = UpgradeStates::QUEUED.to_string();
                job.updated_utc = self.clock.now();
                count += 1;
            }
        }
        if count > 0 {
            self.save(&jobs);
        }
        count
    }

    pub fn any_waiting(&self) -> bool {
        self.jobs
            .lock()
            .values()
            .any(|job| job.state == UpgradeStates::WAITING)
    }

    pub fn update(&self, navidrome_id: &str, change: impl FnOnce(&mut UpgradeJob)) {
        let mut jobs = self.jobs.lock();
        let Some(job) = jobs.get_mut(navidrome_id) else {
            return;
        };
        change(job);
        job.updated_utc = self.clock.now();
        self.save(&jobs);
    }

    // Called with the lock held.
    fn prune(&self, jobs: &mut DotnetDictionary<UpgradeJob>) {
        let cutoff = self.clock.now() - Self::KEEP_FINISHED;
        let stale: Vec<String> = jobs
            .values()
            .filter(|job| !UpgradeStates::open(&job.state) && job.updated_utc < cutoff)
            .map(|job| job.navidrome_id.clone())
            .collect();
        for id in stale {
            jobs.remove(&id);
        }
    }

    // Called with the lock held.
    fn save(&self, jobs: &DotnetDictionary<UpgradeJob>) {
        let Some(path) = &self.path else {
            return;
        };
        let json = octo_core::json::to_string(&jobs.values().collect::<Vec<_>>());
        if let Err(e) = state_file::save_atomic(path, &json) {
            warn!("the upgrade queue could not be written: {e}");
        }
    }
}

/// Runs Better quality on one request (`LibraryActionExecutor::apply`).
pub type ApplyFn = Arc<
    dyn Fn(LibraryActionRequest) -> BoxFuture<'static, anyhow::Result<LibraryActionOutcome>> + Send + Sync,
>;

/// Describes a song from Navidrome (`NavidromeSongPathResolver::resolve`).
pub type DescribeFn = Arc<dyn Fn(String) -> BoxFuture<'static, Option<ResolvedSongFile>> + Send + Sync>;

/// Whether every source is out because Soulseek is.
pub type OfflineFn = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;

/// `LibraryActionExecutor::apply` as a seam.
pub(crate) fn apply_with(executor: Arc<LibraryActionExecutor>) -> ApplyFn {
    Arc::new(move |request| {
        let executor = executor.clone();
        async move { executor.apply(request).await }.boxed()
    })
}

/// The default `soulseek_offline` seam: waiting only when every source is out. With Lidarr set
/// up, a Soulseek outage leaves Lidarr.
pub(crate) fn soulseek_offline_with(
    sources: Option<Arc<UpgradeSources>>,
    soulseek: Option<Arc<dyn ISoulseekLink>>,
) -> OfflineFn {
    Arc::new(move || {
        let sources = sources.clone();
        let soulseek = soulseek.clone();
        async move {
            if let Some(sources) = sources {
                return sources.waiting_for_soulseek().await;
            }
            match soulseek {
                Some(link) => link
                    .read(false)
                    .await
                    .is_some_and(|reading| reading.link == SoulseekLinkState::NotLoggedIn),
                None => false,
            }
        }
        .boxed()
    })
}

/// The worker's seams, which tests replace, the same way the weekly upgrade exposes them.
#[derive(Clone)]
pub struct UpgradeWorkerSeams {
    pub apply: ApplyFn,
    pub describe: DescribeFn,
    pub width: Arc<dyn Fn() -> i32 + Send + Sync>,
    pub soulseek_offline: OfflineFn,
    pub source_name: Arc<dyn Fn() -> String + Send + Sync>,
}

/// What the worker is built from besides the queue, the optional services as `Option`.
pub struct UpgradeWorkerParts {
    pub executor: Arc<LibraryActionExecutor>,
    pub resolver: Arc<NavidromeSongPathResolver>,
    pub concurrency: Option<Arc<DownloadConcurrency>>,
    pub soulseek: Option<Arc<dyn ISoulseekLink>>,
    pub tracker: Option<Arc<AcquisitionTracker>>,
    pub attempts: Option<Arc<QualityUpgradeStore>>,
    pub settings: Option<Arc<SettingsStore>>,
    pub sources: Option<Arc<UpgradeSources>>,
    pub picks: Option<Arc<DownloadPicks>>,
}

/// Runs the upgrade queue: Better quality on each song, up to as many at once as downloads may
/// run, with every safety the action has (the allowlist, dry run, the quarantine, the original
/// back when the new file is not really lossless, and the same Navidrome id after). While
/// Soulseek is out a job waits rather than failing, and goes again once slskd is logged in.
pub struct UpgradeWorker {
    queue: Arc<UpgradeQueue>,
    tracker: Option<Arc<AcquisitionTracker>>,
    attempts: Option<Arc<QualityUpgradeStore>>,
    /// The Soulseek settings, read at each report.
    settings: Option<Arc<SettingsStore>>,
    seams: UpgradeWorkerSeams,
    running: Mutex<Vec<JoinHandle<()>>>,
    /// Copies picked in Find songs, handed to the replacement's download.
    picks: Option<Arc<DownloadPicks>>,
}

impl UpgradeWorker {
    pub const TICK_EVERY: Duration = Duration::from_secs(2);

    pub fn new(queue: Arc<UpgradeQueue>, parts: UpgradeWorkerParts) -> Self {
        let resolver = parts.resolver;
        let concurrency = parts.concurrency;
        let name_sources = parts.sources.clone();
        let seams = UpgradeWorkerSeams {
            apply: apply_with(parts.executor),
            describe: Arc::new(move |id| {
                let resolver = resolver.clone();
                async move { resolver.resolve(&id).await }.boxed()
            }),
            width: Arc::new(move || concurrency.as_ref().map_or(1, |c| c.current()).max(1)),
            soulseek_offline: soulseek_offline_with(parts.sources, parts.soulseek),
            source_name: Arc::new(move || {
                name_sources
                    .as_ref()
                    .map_or_else(|| "Soulseek".to_string(), |s| s.name())
            }),
        };
        Self::with_seams(queue, seams, parts.tracker, parts.attempts, parts.settings).with_picks(parts.picks)
    }

    /// The picks a job's chosen copy is pinned in for its download.
    pub fn with_picks(mut self, picks: Option<Arc<DownloadPicks>>) -> Self {
        self.picks = picks;
        self
    }

    /// A worker over the given seams (tests).
    pub fn with_seams(
        queue: Arc<UpgradeQueue>,
        seams: UpgradeWorkerSeams,
        tracker: Option<Arc<AcquisitionTracker>>,
        attempts: Option<Arc<QualityUpgradeStore>>,
        settings: Option<Arc<SettingsStore>>,
    ) -> Self {
        UpgradeWorker {
            queue,
            tracker,
            attempts,
            settings,
            seams,
            running: Mutex::new(Vec::new()),
            picks: None,
        }
    }

    /// Jobs this worker is running now.
    pub fn running(&self) -> usize {
        self.running
            .lock()
            .iter()
            .filter(|task| !task.is_finished())
            .count()
    }

    /// One pass: waiting jobs back in the queue when Soulseek is back, then queued jobs started
    /// while there is room. Answers how many it started.
    pub async fn tick(self: &Arc<Self>, ct: &CancellationToken) -> usize {
        if self.queue.any_waiting() && !(self.seams.soulseek_offline)().await {
            self.queue.requeue();
        }
        let mut started = 0;
        self.running.lock().retain(|task| !task.is_finished());
        while (self.running() as i64) < i64::from((self.seams.width)()) {
            let Some(job) = self.queue.take_next() else {
                break;
            };
            let worker = self.clone();
            let ct = ct.clone();
            let run = tokio::spawn(async move {
                tokio::select! {
                    _ = worker.run_job(job) => {}
                    // Stopped mid-job: the job stays working, and the next start queues it again.
                    _ = ct.cancelled() => {}
                }
            });
            self.running.lock().push(run);
            started += 1;
        }
        started
    }

    /// Waits for every job started so far. Only tests need it.
    pub async fn drain(&self) {
        let running: Vec<JoinHandle<()>> = std::mem::take(&mut *self.running.lock());
        for task in running {
            let _ = task.await;
        }
    }

    async fn run_job(self: Arc<Self>, mut job: UpgradeJob) {
        if let Err(e) = self.try_run_job(&mut job).await {
            warn!("Higher quality for {} failed: {e}", job.navidrome_id);
            self.queue.update(&job.navidrome_id, |j| {
                j.state = UpgradeStates::FAILED.to_string();
                j.detail = Some(e.to_string());
            });
        }
    }

    async fn try_run_job(&self, job: &mut UpgradeJob) -> anyhow::Result<()> {
        if job.title.is_none()
            && let Some(song) = (self.seams.describe)(job.navidrome_id.clone()).await
        {
            self.queue.update(&job.navidrome_id, |j| {
                j.title = Some(song.title.clone());
                j.artist = Some(song.artist.clone());
                j.album = Some(song.album.clone());
                j.suffix = Some(song.suffix.clone());
            });
            job.title = Some(song.title);
            job.artist = Some(song.artist);
            job.album = Some(song.album);
        }

        let queue = self.queue.clone();
        let tracker = self.tracker.clone();
        let picks = self.picks.clone();
        let followed = job.clone();
        // One pick for the whole job, so the one pinned is the one let go below.
        let pick = job.pick.clone().map(Arc::new);
        let acquired: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let mut request =
            LibraryActionRequest::new(LibraryAction::BetterQuality, &job.navidrome_id, &job.requested_by);
        let (pinned, heard) = (pick.clone(), acquired.clone());
        request.on_replacement_queued = Some(Arc::new(move |provider: &str, external_id: &str| {
            *heard.lock() = Some(external_id.to_string());
            queue.update(&followed.navidrome_id, |j| {
                j.acquisition_key = Some(format!("{provider}:{external_id}"));
            });
            // A copy picked in Find songs is fetched as it is, with no search.
            if let (Some(pick), Some(picks)) = (&pinned, &picks) {
                picks.pin(external_id, pick.clone());
            }
            // The tracker follows only rows that were opened, so the replacement gets one. It
            // carries the library song's id, so the apps draw that song's cover on it.
            if let Some(tracker) = &tracker {
                tracker.begin_kind(
                    provider,
                    external_id,
                    Some(&followed.navidrome_id),
                    Some(&followed.requested_by),
                    followed.artist.as_deref(),
                    followed.title.as_deref(),
                    followed.album.as_deref(),
                    acquisition_kinds::UPGRADE,
                );
                let note = match &pinned {
                    Some(chosen) => format!("Getting {}", chosen.describe()),
                    None => format!(
                        "Your copy is {}; looking for a lossless one",
                        followed
                            .suffix
                            .as_deref()
                            .map_or_else(|| "lossy".to_string(), dotnet::to_upper_invariant)
                    ),
                };
                tracker.log(
                    provider,
                    external_id,
                    acquisition_event_kinds::NOTE,
                    &note,
                    Some("Your copy stays until the new one passes every check"),
                );
            }
        }));
        request.only_source = pick.as_ref().map(|only| {
            if only.is_soulseek() {
                DownloadSource::Soulseek
            } else {
                DownloadSource::Lidarr
            }
        });
        request.anyway = pick.as_ref().and_then(|only| only.anyway.clone());
        let outcome = (self.seams.apply)(request).await;
        // A pick the replacement never used (it joined another download, or found the song
        // needed nothing) is let go with the job, not left for a later download.
        let acquired = acquired.lock().clone();
        if let (Some(acquired), Some(unused), Some(picks)) = (&acquired, &pick, &self.picks) {
            picks.forget(acquired, unused);
        }
        let outcome = outcome?;

        let state = Self::state_for(&outcome);
        let result = (state == UpgradeStates::UPGRADED).then(|| self.report(&outcome, job));
        let detail = match state {
            UpgradeStates::WAITING => Some("Waiting for Soulseek".to_string()),
            UpgradeStates::UPGRADED if result.as_ref().is_some_and(|r| r.after.is_some()) => {
                let result = result.as_ref().expect("checked above");
                Some(format!(
                    "Now {}{}, was {}{}.",
                    result.after.as_deref().unwrap_or(""),
                    size(result.after_bytes),
                    result
                        .before
                        .clone()
                        .or_else(|| job.suffix.as_deref().map(dotnet::to_upper_invariant))
                        .unwrap_or_default(),
                    size(result.before_bytes)
                ))
            }
            // A copy someone picked that did not go in: what stopped it, in plain words.
            UpgradeStates::NOT_FOUND | UpgradeStates::SKIPPED | UpgradeStates::FAILED
                if job.pick.is_some() =>
            {
                Some(Self::pick_not_taken(outcome.detail.as_deref()))
            }
            UpgradeStates::NOT_FOUND => Some(format!(
                "No lossless copy of this song on {} right now. Your copy is unchanged.",
                (self.seams.source_name)()
            )),
            _ => outcome.detail.clone(),
        };
        // The verdict comes after the download's own row has ended, so it is a line of its own.
        if let (Some(acquired), Some(tracker)) = (&acquired, &self.tracker) {
            let text = match state {
                UpgradeStates::UPGRADED => "Took the place of your old copy",
                _ if job.pick.is_some()
                    && state != UpgradeStates::UPGRADED
                    && state != UpgradeStates::WAITING =>
                {
                    "The copy you picked did not go in; your copy was kept"
                }
                UpgradeStates::NOT_FOUND => "No lossless copy found; your copy is unchanged",
                UpgradeStates::WAITING => "Waiting for Soulseek",
                UpgradeStates::REHEARSED => "Only rehearsed; nothing changed",
                _ => "Your copy is unchanged",
            };
            let verdict = match result
                .as_ref()
                .and_then(|r| r.after.as_deref().map(|now| (r, now)))
            {
                Some((result, now)) if state == UpgradeStates::UPGRADED => Some(format!(
                    "Now {now}{}, was {}{}",
                    size(result.after_bytes),
                    result
                        .before
                        .clone()
                        .or_else(|| job.suffix.as_deref().map(dotnet::to_upper_invariant))
                        .unwrap_or_default(),
                    size(result.before_bytes)
                )),
                _ => user_safe(outcome.detail.as_deref()),
            };
            tracker.log(
                SoulseekMetadataService::PROVIDER_NAME,
                acquired,
                if state == UpgradeStates::UPGRADED {
                    acquisition_event_kinds::DONE
                } else {
                    acquisition_event_kinds::NOTE
                },
                text,
                verdict.as_deref(),
            );
        }
        self.queue.update(&job.navidrome_id, |j| {
            j.state = state.to_string();
            j.result = result;
            j.detail = detail;
        });
        if state == UpgradeStates::NOT_FOUND
            && let (Some(key), Some(attempts)) = (&job.attempt_key, &self.attempts)
        {
            let attempt = QualityUpgradeAttempt {
                at_utc: Utc::now(),
                outcome: outcome.state.name().to_string(),
                detail: outcome.detail.clone(),
            };
            attempts.update(|s| s.attempts.set(key.clone(), attempt));
        }
        info!(
            "Higher quality for {} ('{} - {}') by {}: {state} - {}",
            job.navidrome_id,
            job.artist.as_deref().unwrap_or(""),
            job.title.as_deref().unwrap_or(""),
            job.requested_by,
            outcome.detail.as_deref().unwrap_or("")
        );
        Ok(())
    }

    /// The proof for a replacement that went in: both files described from their own headers,
    /// the checks the new one passed to get there, and where the original waits. Never fails: a
    /// file that cannot be read is simply not described.
    pub fn report(&self, outcome: &LibraryActionOutcome, job: &UpgradeJob) -> UpgradeResult {
        let settings = self
            .settings
            .as_ref()
            .map_or_else(SoulseekSettings::default, |s| s.current().soulseek.clone());
        let mut result = UpgradeResult {
            new_file: outcome.new_path.as_deref().map(file_name),
            kept_at: outcome.quarantine_path.as_deref().map(kept_folder),
            seconds: job.started_utc.map(|started| {
                let elapsed = (Utc::now() - started).num_microseconds().unwrap_or(i64::MAX) as f64 / 1e6;
                dotnet::round(elapsed, 0)
            }),
            ..Default::default()
        };
        (result.after, result.after_bytes) = AudioSummary::describe(outcome.new_path.as_deref());
        (result.before, result.before_bytes) = AudioSummary::describe(outcome.quarantine_path.as_deref());
        // Every replacement is held to these before it may take the original's place. One the
        // person said to take whatever its quality is held to the song they picked it as.
        let anyway = job.pick.as_ref().is_some_and(|pick| pick.anyway.is_some());
        result.checks.push(
            if anyway {
                "the length of the song you picked"
            } else {
                "the same length"
            }
            .to_string(),
        );
        if settings.verify_downloads && settings.acoust_id_key_for_heart().is_some() {
            result.checks.push("AcoustID: the same recording".to_string());
        }
        if anyway {
            result
                .checks
                .push("no quality check: you said to replace yours anyway".to_string());
        } else if settings.detect_transcodes {
            result
                .checks
                .push("the spectrum: really lossless, not a converted MP3".to_string());
        }
        result
    }

    /// Why a copy someone picked did not take their song's place, ending with what happened to
    /// theirs: "The copy you picked did not replace yours: it was the wrong recording. Your copy
    /// was kept."
    pub fn pick_not_taken(detail: Option<&str>) -> String {
        let why = user_safe(detail).unwrap_or_default();
        let why = why.split(", so nothing changed").next().unwrap_or("").trim();
        let why = why.trim_end_matches('.');
        let why = why
            .strip_prefix("The replacement ")
            .map_or_else(|| why.to_string(), |rest| format!("it {rest}"));
        if why.is_empty() {
            "The copy you picked did not replace yours. Your copy was kept.".to_string()
        } else {
            let mut chars = why.chars();
            let first = chars
                .next()
                .map(|c| c.to_lowercase().collect::<String>())
                .unwrap_or_default();
            let why = if why.starts_with("Octo") || why.starts_with("Soulseek") || why.starts_with("Lidarr") {
                why.clone()
            } else {
                format!("{first}{}", chars.as_str())
            };
            format!("The copy you picked did not replace yours: {why}. Your copy was kept.")
        }
    }

    /// How an action's outcome reads as a job state. On the reason code, never the words.
    pub fn state_for(outcome: &LibraryActionOutcome) -> &'static str {
        match (outcome.state, outcome.code.as_deref()) {
            (LibraryActionState::Applied, _) => UpgradeStates::UPGRADED,
            (LibraryActionState::Rehearsed, _) => UpgradeStates::REHEARSED,
            (_, Some(LibraryActionCodes::SOULSEEK_OFFLINE)) => UpgradeStates::WAITING,
            (_, Some(LibraryActionCodes::NO_REPLACEMENT)) => UpgradeStates::NOT_FOUND,
            (LibraryActionState::Skipped, _) => UpgradeStates::SKIPPED,
            _ => UpgradeStates::FAILED,
        }
    }

    /// The worker loop, until the host stops.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        while !stopping.is_cancelled() {
            // Per-tick catch: a tick that panics is logged and the loop goes on.
            let tick = std::panic::AssertUnwindSafe(self.tick(&stopping))
                .catch_unwind()
                .await;
            if tick.is_err() {
                error!("Upgrade queue tick failed");
            }
            tokio::select! {
                _ = tokio::time::sleep(Self::TICK_EVERY) => {}
                _ = stopping.cancelled() => break,
            }
        }
        Ok(())
    }
}

/// Everything after the last separator.
fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// The directory: everything before the last separator (and any separators right before it), the
/// root kept; None for the root itself.
fn directory_name(path: &str) -> Option<String> {
    if path.is_empty() || path == "/" {
        return None;
    }
    match path.rfind('/') {
        None => Some(String::new()),
        Some(last) => {
            let head = path[..last].trim_end_matches('/');
            Some(if head.is_empty() && path.starts_with('/') {
                "/".to_string()
            } else {
                head.to_string()
            })
        }
    }
}

// ".octo-trash/2026-10-03", the part of the quarantine path a person can find again.
fn kept_folder(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|p| !p.is_empty()).collect();
    match parts
        .iter()
        .position(|p| p.starts_with('.') && dotnet::to_lower_invariant(p).contains("trash"))
    {
        Some(trash) if trash < parts.len() => parts[trash..parts.len() - 1].join("/"),
        _ => directory_name(path).unwrap_or_else(|| path.to_string()),
    }
}

fn size(bytes: Option<i64>) -> String {
    match bytes {
        Some(bytes) if bytes > 0 => format!(", {} MB", fixed_one(bytes as f64 / 1_048_576.0)),
        _ => String::new(),
    }
}

/// The custom format `"0.0"`: always one decimal.
fn fixed_one(value: f64) -> String {
    let text = dotnet::format_optional_decimals(value, 1);
    if text.contains('.') {
        text
    } else {
        format!("{text}.0")
    }
}

/// A file described from its own header: "FLAC 16-bit 44.1 kHz" or "MP3 320 kbps".
pub struct AudioSummary;

impl AudioSummary {
    const LOSSLESS: [&'static str; 7] = ["flac", "wav", "aiff", "aif", "alac", "ape", "wv"];

    pub fn describe(path: Option<&str>) -> (Option<String>, Option<i64>) {
        let Some(path) = path.filter(|p| !p.is_empty()) else {
            return (None, None);
        };
        let Ok(metadata) = std::fs::metadata(path) else {
            return (None, None);
        };
        if !metadata.is_file() {
            return (None, None);
        }
        let bytes = metadata.len() as i64;
        let format = dotnet::to_upper_invariant(&extension(path));
        if let Ok(tagged) = lofty::read_from_path(path) {
            use lofty::file::AudioFile;
            let properties = tagged.properties();
            let bits = properties.bit_depth().map_or(0, i32::from);
            let rate = properties.sample_rate().unwrap_or(0);
            if Self::LOSSLESS.iter().any(|l| dotnet::eq_ignore_case(l, &format)) && bits > 0 && rate > 0 {
                let khz = dotnet::format_optional_decimals(f64::from(rate) / 1000.0, 1);
                return (Some(format!("{format} {bits}-bit {khz} kHz")), Some(bytes));
            }
            if let Some(bitrate) = properties.audio_bitrate().filter(|b| *b > 0) {
                return (Some(format!("{format} {bitrate} kbps")), Some(bytes));
            }
        }
        // Unreadable: the format from the name is still worth saying.
        (Some(format), Some(bytes))
    }
}

/// The extension without its dot, or empty.
fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot + 1..].trim_start_matches('.').to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
#[path = "upgrade_queue_tests.rs"]
mod tests;
