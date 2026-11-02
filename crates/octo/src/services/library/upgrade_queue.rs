//! The songs asked to be found in higher quality (`<config>/upgrades.json`), the worker that runs
//! them, and `AudioSummary`.

use std::collections::HashMap;
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
use crate::services::fingerprint::acoust_id_rate_limiter::LookupClass;
use crate::services::fingerprint::download_verification_service::{
    ANOTHER_SONG_PLAYED, became_with_duplicates,
};
use crate::services::fingerprint::music_brainz_client::{CatalogQuestions, HeartQuestions, Lane};
use crate::services::fingerprint::{AcoustIdClient, MusicBrainzClient};
use crate::services::framework::DotnetDictionary;
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::local::{ILocalLibraryService, KeptHeart, LocalSongMapping};
use crate::services::prefetch::HeardSamples;
use crate::services::prefetch::heard_samples::{FpcalcClass, HeardPrints, sample_id_of};
use crate::services::soulseek::{ExternalIdRegistry, ISoulseekLink, SoulseekMetadataService};
use crate::services::state_file;
use octo_core::fingerprint::acoust_id_client::AcoustIdOutcome;
use octo_core::fingerprint::compressed_print;
use octo_core::fingerprint::heart_gate::{
    AcquisitionOrigin, HeardAnswerReading, HeardReading, HeardRecord, HeardRecording, HeardSource, HeartGate,
    read_heard_answer,
};
use octo_core::fingerprint::verification::{Reading, SongName, read_answer};

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

    /// What a waiting job waits for, as its outcome's code: `mornyeKept` goes back in the queue
    /// by its own Mornye pause ([`Self::mornye_hold`]), any other wait once a source can search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<String>,

    /// For a job Mornye's check kept: when that pause ends and the checks it named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mornye_hold: Option<MornyeHold>,

    /// The job that went back alone when its Mornye pause ended: while it is queued or running,
    /// the others held on Mornye's check wait for what its try shows. Kept on disk, so a restart
    /// does not send a second one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mornye_scout: bool,
}

/// The Mornye pause a job waits on: it goes back once the pause ends (one job at a time, the
/// others following once that one's try is known), once a read shows its checks passing, or all
/// at once when the Mornye that paused is switched off or moved to another address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MornyeHold {
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub until_utc: DateTime<Utc>,
    /// The checks whose passing lets the job go early.
    #[serde(default)]
    pub labels: Vec<String>,
    /// The checks it shares with other held jobs; empty means `labels` (see
    /// `CheckPause::holding`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holding: Vec<String>,
    /// How many times another job's try on a shared check has reached this hold, so a pass
    /// read before such a try never lets the job go.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub moved: u32,
    /// The address of the Mornye whose check paused; empty when Octo does not know it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

impl MornyeHold {
    pub fn of(pause: &octo_core::spotiflac::check::CheckPause) -> MornyeHold {
        MornyeHold {
            until_utc: pause.until_utc,
            labels: pause.labels.clone(),
            holding: pause.holding.clone(),
            moved: 0,
            url: pause.url.clone(),
        }
    }

    /// A hold Octo knows only the end of.
    pub fn until(until_utc: DateTime<Utc>) -> MornyeHold {
        MornyeHold {
            until_utc,
            labels: Vec::new(),
            holding: Vec::new(),
            moved: 0,
            url: String::new(),
        }
    }

    fn shared(&self) -> &[String] {
        if self.holding.is_empty() {
            &self.labels
        } else {
            &self.holding
        }
    }

    /// Whether the two wait on a check in common. A hold that names no check at all may wait on
    /// any of them.
    fn shares_a_check(&self, other: &MornyeHold) -> bool {
        let (mine, theirs) = (self.shared(), other.shared());
        mine.is_empty() || theirs.is_empty() || mine.iter().any(|label| theirs.contains(label))
    }
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
            wait: None,
            mornye_hold: None,
            mornye_scout: false,
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

    /// Waiting jobs back in the queue, once Soulseek is back. Jobs Mornye's check kept wait for
    /// their own pause ([`Self::release_mornye`]).
    pub fn requeue(&self) -> usize {
        self.requeue_where(|job| !Self::kept_by_mornye(job))
    }

    /// The waiting jobs `ready` lets go back in the queue, each by what it waits for.
    pub fn requeue_where(&self, ready: impl Fn(&UpgradeJob) -> bool) -> usize {
        let mut jobs = self.jobs.lock();
        let mut count = 0;
        for job in jobs.values_mut() {
            if job.state == UpgradeStates::WAITING && ready(job) {
                job.state = UpgradeStates::QUEUED.to_string();
                job.wait = None;
                job.mornye_hold = None;
                job.mornye_scout = false;
                job.updated_utc = self.clock.now();
                count += 1;
            }
        }
        if count > 0 {
            self.save(&jobs);
        }
        count
    }

    fn kept_by_mornye(job: &UpgradeJob) -> bool {
        job.state == UpgradeStates::WAITING && job.wait.as_deref() == Some(LibraryActionCodes::MORNYE_KEPT)
    }

    /// Sends back the jobs Mornye's check kept that may try again, from the queue as it is now:
    /// each in `passing` (a read showed the checks of that hold passing) whose hold is still the
    /// one the read judged, and, unless the job sent back alone is still queued or running, the
    /// oldest whose pause has ended by `now`, marked as that one (the scout). One lock: a scout
    /// that finished while the checks were read, and moved the others' pause, is seen here.
    /// Answers how many went back.
    pub fn release_mornye(&self, passing: &HashMap<String, MornyeHold>, now: DateTime<Utc>) -> usize {
        let mut jobs = self.jobs.lock();
        let passes = |job: &UpgradeJob| {
            passing
                .get(&job.navidrome_id)
                .is_some_and(|judged| job.mornye_hold.as_ref() == Some(judged))
        };
        let scout = if Self::scout_on_its_way(&jobs) {
            None
        } else {
            jobs.values()
                .filter(|job| Self::kept_by_mornye(job) && !passes(job))
                .filter(|job| {
                    job.mornye_hold
                        .as_ref()
                        .map_or(job.updated_utc, |hold| hold.until_utc)
                        <= now
                })
                .min_by(|a, b| {
                    a.queued_utc
                        .cmp(&b.queued_utc)
                        .then_with(|| a.navidrome_id.cmp(&b.navidrome_id))
                })
                .map(|job| job.navidrome_id.clone())
        };
        let stamp = self.clock.now();
        let mut count = 0;
        for job in jobs.values_mut() {
            let is_scout = scout.as_deref() == Some(job.navidrome_id.as_str());
            if Self::kept_by_mornye(job) && (is_scout || passes(job)) {
                job.state = UpgradeStates::QUEUED.to_string();
                job.wait = None;
                job.mornye_hold = None;
                job.mornye_scout = is_scout;
                job.updated_utc = stamp;
                count += 1;
            }
        }
        if count > 0 {
            self.save(&jobs);
        }
        count
    }

    /// The waiting jobs Mornye's check kept, oldest first, with their holds.
    pub fn mornye_holds(&self) -> Vec<(String, MornyeHold)> {
        let jobs = self.jobs.lock();
        let mut held: Vec<(DateTime<Utc>, String, MornyeHold)> = jobs
            .values()
            .filter(|job| job.state == UpgradeStates::WAITING)
            .filter(|job| job.wait.as_deref() == Some(LibraryActionCodes::MORNYE_KEPT))
            .map(|job| {
                let hold = job
                    .mornye_hold
                    .clone()
                    .unwrap_or_else(|| MornyeHold::until(job.updated_utc));
                (job.queued_utc, job.navidrome_id.clone(), hold)
            })
            .collect();
        held.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        held.into_iter().map(|(_, id, hold)| (id, hold)).collect()
    }

    /// Whether the job sent back alone when its Mornye pause ended is still queued or running.
    pub fn mornye_scout_on_its_way(&self) -> bool {
        Self::scout_on_its_way(&self.jobs.lock())
    }

    fn scout_on_its_way(jobs: &DotnetDictionary<UpgradeJob>) -> bool {
        jobs.values().any(|job| {
            job.mornye_scout && matches!(job.state.as_str(), UpgradeStates::QUEUED | UpgradeStates::WORKING)
        })
    }

    /// Ends a job's run: `change` sets what it found, and its scout mark goes (its try is known).
    /// When it waits on a Mornye pause again, the other jobs held on a pause that shares a check
    /// wait at least until that pause's new end, and a pass read before this try no longer lets
    /// them go. One lock for all of it, so no tick sees the job's new hold before the others have
    /// moved.
    pub fn finish_run(&self, navidrome_id: &str, change: impl FnOnce(&mut UpgradeJob)) {
        let mut jobs = self.jobs.lock();
        let Some(job) = jobs.get_mut(navidrome_id) else {
            return;
        };
        change(job);
        job.mornye_scout = false;
        job.updated_utc = self.clock.now();
        let hold = job
            .mornye_hold
            .clone()
            .filter(|_| job.state == UpgradeStates::WAITING);
        if let Some(hold) = &hold {
            for other in jobs.values_mut() {
                if other.navidrome_id != navidrome_id
                    && other.state == UpgradeStates::WAITING
                    && let Some(held) = other.mornye_hold.as_mut()
                    && held.shares_a_check(hold)
                {
                    // A try on this check just failed: a pass read before it says nothing now.
                    held.moved = held.moved.saturating_add(1);
                    held.until_utc = held.until_utc.max(hold.until_utc);
                }
            }
        }
        self.save(&jobs);
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
    /// Whether a Mornye hold may end before its pause does (a Pass, Mornye's own try passing,
    /// or the Mornye that paused switched off or moved).
    pub mornye_hold_over: HoldOverFn,
    pub source_name: Arc<dyn Fn() -> String + Send + Sync>,
}

/// Whether a Mornye hold may end before its pause does.
pub type HoldOverFn = Arc<dyn Fn(MornyeHold) -> BoxFuture<'static, bool> + Send + Sync>;

/// The default `mornye_hold_over` seam: the Mornye the hold was made against is off or has
/// another address now, or a read of Mornye shows every check the hold named working.
pub(crate) fn mornye_hold_over_with(sources: Option<Arc<UpgradeSources>>) -> HoldOverFn {
    Arc::new(move |hold| {
        let sources = sources.clone();
        async move {
            match sources {
                Some(sources) => sources.mornye_wait_over(&hold.url, &hold.labels).await,
                None => true,
            }
        }
        .boxed()
    })
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
            soulseek_offline: soulseek_offline_with(parts.sources.clone(), parts.soulseek),
            mornye_hold_over: mornye_hold_over_with(parts.sources),
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

    /// One pass: waiting jobs back in the queue once a source can search, queued jobs started
    /// while there is room, then the jobs Mornye's check kept back once Mornye may try again (and
    /// started too). Mornye is read only after the start, so a slow Mornye never holds up a song
    /// that is ready to go. Answers how many it started.
    pub async fn tick(self: &Arc<Self>, ct: &CancellationToken) -> usize {
        if self.queue.any_waiting() && !(self.seams.soulseek_offline)().await {
            self.queue.requeue();
        }
        let mut started = self.start_queued(ct);
        if self.queue.any_waiting() {
            let passing = self.mornye_passing().await;
            if self.queue.release_mornye(&passing, Utc::now()) > 0 {
                started += self.start_queued(ct);
            }
        }
        started
    }

    /// Starts queued jobs while there is room. Answers how many it started.
    fn start_queued(self: &Arc<Self>, ct: &CancellationToken) -> usize {
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

    /// The jobs Mornye's check kept that may go back now, all at once: a read shows their checks
    /// passing, or the Mornye that paused is switched off or moved. Each comes with the hold that
    /// was judged. Which of the others goes back alone once its pause ends is decided by the queue
    /// as it stands after this.
    async fn mornye_passing(&self) -> HashMap<String, MornyeHold> {
        let holds = self.queue.mornye_holds();
        let mut released = HashMap::new();
        if holds.is_empty() {
            return released;
        }
        // One answer per Mornye and set of checks.
        let mut judged: Vec<((String, Vec<String>), bool)> = Vec::new();
        for (id, hold) in &holds {
            let key = (hold.url.clone(), hold.labels.clone());
            let known = judged
                .iter()
                .find(|(seen, _)| *seen == key)
                .map(|(_, over)| *over);
            let over = match known {
                Some(over) => over,
                None => {
                    let over = (self.seams.mornye_hold_over)(hold.clone()).await;
                    judged.push((key, over));
                    over
                }
            };
            if over {
                released.insert(id.clone(), hold.clone());
            }
        }
        released
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
        // A copy picked for a song the person said is the wrong version replaces it as Wrong
        // version does; every other job is Better quality.
        let wrong_version = pick.as_ref().is_some_and(|pick| pick.wrong_version);
        let mut request = LibraryActionRequest::new(
            if wrong_version {
                LibraryAction::WrongVersion
            } else {
                LibraryAction::BetterQuality
            },
            &job.navidrome_id,
            &job.requested_by,
        );
        request.picked = wrong_version;
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
        let mornye_check = LibraryActionCodes::waits_on_mornye(outcome.code.as_deref());
        let detail = match state {
            UpgradeStates::WAITING if mornye_check => outcome.detail.clone(),
            UpgradeStates::WAITING if outcome.code.as_deref() == Some(LibraryActionCodes::MORNYE_OUT) => {
                Some("Waiting for Mornye, which is not answering".to_string())
            }
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
                UpgradeStates::WAITING if mornye_check => "Waiting for Mornye's check",
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
        let hold = outcome
            .mornye_wait
            .as_deref()
            .filter(|_| state == UpgradeStates::WAITING)
            .map(MornyeHold::of);
        // A scout's try is known: it failed again (the others now wait for its new pause), or it
        // ended any other way (the others go back once a read shows the check passing, or their
        // pause ends with the next scout).
        self.queue.finish_run(&job.navidrome_id, |j| {
            j.state = state.to_string();
            j.result = result;
            j.detail = detail;
            j.wait = outcome.code.clone().filter(|_| state == UpgradeStates::WAITING);
            j.mornye_hold = hold;
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
        if detail == Some(LibraryActionExecutor::SAME_AS_REJECTED) {
            return LibraryActionExecutor::SAME_AS_REJECTED.to_string();
        }
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
            (
                _,
                Some(
                    LibraryActionCodes::SOULSEEK_OFFLINE
                    | LibraryActionCodes::MORNYE_OUT
                    | LibraryActionCodes::MORNYE_CHECK
                    | LibraryActionCodes::MORNYE_KEPT,
                ),
            ) => UpgradeStates::WAITING,
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

/// What reading a kept file's AcoustID answer again came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reread {
    /// AcoustID did not answer: nothing changes until the next event.
    Busy,
    /// R stays as the heart had it: a copy is accepted by its own clear reading against R, or as
    /// the very master the kept file is.
    Stays { tag: Option<String> },
    /// What played was this recording: R becomes it.
    Became { r: HeardRecording, tag: Option<String> },
    /// Nothing proves which recording the kept file is: only the very master it is can replace it.
    OnlyHeardOrMaster,
    /// The kept file is another song than the one hearted: nothing is tried for it, its video is
    /// refused for the song, and the heart holds for a play.
    AnotherSong,
    /// Nothing to read it by: no print was kept with it, or AcoustID is off.
    Unread,
}

impl Reread {
    /// Whether a search for a better copy can succeed against this: only when a copy can be
    /// accepted by more than the kept file's own master, which the heart's chain already looked
    /// for when it kept it.
    pub fn can_succeed(&self) -> bool {
        matches!(self, Reread::Stays { .. } | Reread::Became { .. })
    }
}

/// Starts the search for a better copy of one kept file.
pub type KeptUpgrade = Arc<dyn Fn(LocalSongMapping) -> BoxFuture<'static, ()> + Send + Sync>;

/// Holds a song's heart for these people "not yet proven", with this line: the provider, the
/// song's id, the people and the line.
pub type HoldHeart = Arc<dyn Fn(&str, &str, &[String], &str) + Send + Sync>;

/// The holds a person waits on ("not yet proven" hearts and hearted albums' held tracks), each by
/// its key with since when it waits.
pub type WaitingHolds = Arc<dyn Fn() -> Vec<(String, DateTime<Utc>)> + Send + Sync>;

/// Checks one hold again by its key.
pub type RecheckHold = Arc<dyn Fn(&str) + Send + Sync>;

/// One item a heart's proof may check again.
enum Recheckable {
    Kept(Box<LocalSongMapping>),
    Held(String),
}

/// Files kept as heard or kept anyway, checked again on the events tied to work: a play or a heart
/// of the song, a heart AcoustID's clear reading accepts, and the person's own Retry. Each check sends one
/// lookup of the print kept with the file (no `fpcalc`), reads it as what was heard is read, and
/// only when a better copy could then be proven starts the search for one, whatever the weekly
/// upgrade's settings and however recently it last tried. A check that finds nothing new fetches
/// nothing. Each proof checks the one item of every kind (kept files, hearts held "not yet proven"
/// and hearted albums' held tracks) checked longest ago, one pool for all three.
pub struct KeptRechecks {
    library: Arc<dyn ILocalLibraryService>,
    acoust_id: Arc<AcoustIdClient>,
    settings: Arc<SettingsStore>,
    registry: Arc<ExternalIdRegistry>,
    upgrade: std::sync::OnceLock<KeptUpgrade>,
    /// How a heart whose kept file is another song is held for a play.
    hold: std::sync::OnceLock<HoldHeart>,
    /// What MusicBrainz is asked when R becomes the recording that played: its same-note
    /// duplicates.
    music_brainz: std::sync::OnceLock<(Arc<MusicBrainzClient>, Arc<dyn CatalogQuestions>)>,
    /// Where a kept file's own print waits as the sample its replacements are compared with.
    samples: std::sync::OnceLock<Arc<HeardSamples>>,
    /// The holds a person waits on, and how one is checked again.
    holds: std::sync::OnceLock<(WaitingHolds, RecheckHold)>,
    /// When each item was last checked, by its kind and key, in the order of checks.
    checked: Mutex<std::collections::HashMap<String, u64>>,
    turn: std::sync::atomic::AtomicU64,
    /// What the last check of each kept file read, taken by the search it started, so the
    /// search sends no second lookup.
    read: Mutex<std::collections::HashMap<String, Reread>>,
}

impl KeptRechecks {
    pub fn new(
        library: Arc<dyn ILocalLibraryService>,
        acoust_id: Arc<AcoustIdClient>,
        settings: Arc<SettingsStore>,
        registry: Arc<ExternalIdRegistry>,
    ) -> Self {
        KeptRechecks {
            library,
            acoust_id,
            settings,
            registry,
            upgrade: std::sync::OnceLock::new(),
            hold: std::sync::OnceLock::new(),
            music_brainz: std::sync::OnceLock::new(),
            samples: std::sync::OnceLock::new(),
            holds: std::sync::OnceLock::new(),
            checked: Mutex::new(std::collections::HashMap::new()),
            turn: std::sync::atomic::AtomicU64::new(0),
            read: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// How a search for a better copy is started. Once only.
    pub fn use_upgrade(&self, upgrade: KeptUpgrade) {
        let _ = self.upgrade.set(upgrade);
    }

    /// How a heart is held "not yet proven" when its kept file turns out to be another song.
    /// Once only.
    pub fn use_hold(&self, hold: HoldHeart) {
        let _ = self.hold.set(hold);
    }

    /// MusicBrainz, for the recordings it files under a recording's own codes with its note.
    /// Once only.
    pub fn use_music_brainz(&self, client: Arc<MusicBrainzClient>, catalog: Arc<dyn CatalogQuestions>) {
        let _ = self.music_brainz.set((client, catalog));
    }

    /// Where a kept file's own print is stored as the sample proof 2 compares its replacements
    /// with, and where files are printed. Once only.
    pub fn use_samples(&self, samples: Arc<HeardSamples>) {
        let _ = self.samples.set(samples);
    }

    /// MusicBrainz's questions at the background lane, when it is there.
    fn questions(&self) -> Option<HeartQuestions> {
        self.music_brainz
            .get()
            .map(|(client, catalog)| HeartQuestions::new(client.clone(), catalog.clone(), Lane::Background))
    }

    /// The kept files of one outside song.
    async fn kept_of(&self, provider: &str, external_id: &str) -> Vec<LocalSongMapping> {
        self.library
            .get_mappings()
            .await
            .into_iter()
            .filter(|mapping| {
                mapping.external_provider == provider
                    && mapping.external_id == external_id
                    && rechecked(mapping)
                    && Path::new(&mapping.local_path).is_file()
            })
            .collect()
    }

    /// A play or a heart of the song: each of its kept files is checked again, once.
    pub async fn song_event(&self, provider: &str, external_id: &str) {
        for mapping in self.kept_of(provider, external_id).await {
            self.recheck(mapping).await;
        }
    }

    /// The holds a person waits on, and how one is checked again: they join kept files in the
    /// order a heart's proof checks items in. Once only.
    pub fn use_holds(&self, waiting: WaitingHolds, recheck: RecheckHold) {
        let _ = self.holds.set((waiting, recheck));
    }

    /// The hold `key` was checked again now, by whatever event: it goes to the back of the order.
    pub fn checked_hold(&self, key: &str) {
        self.mark(&held_item(key));
    }

    fn mark(&self, item: &str) {
        let turn = self.turn.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        self.checked.lock().insert(item.to_string(), turn);
    }

    /// A heart AcoustID's clear reading accepted: the one item checked longest ago (never before
    /// counts as longest; ties go to the one kept or held longest), of every kind at once (a file
    /// kept as heard, a heart held "not yet proven", a hearted album's held track), is checked
    /// next, so checks follow real proofs and never loop on their own answers.
    pub async fn after_a_proof(&self) {
        let checked = self.checked.lock().clone();
        let mut items: Vec<(u64, DateTime<Utc>, Recheckable)> = self
            .library
            .get_mappings()
            .await
            .into_iter()
            .filter(|mapping| rechecked(mapping) && Path::new(&mapping.local_path).is_file())
            .map(|mapping| {
                (
                    checked.get(&kept_item(&mapping)).copied().unwrap_or(0),
                    mapping.downloaded_at,
                    Recheckable::Kept(Box::new(mapping)),
                )
            })
            .collect();
        if let Some((waiting, _)) = self.holds.get() {
            items.extend(waiting().into_iter().map(|(key, since)| {
                (
                    checked.get(&held_item(&key)).copied().unwrap_or(0),
                    since,
                    Recheckable::Held(key),
                )
            }));
        }
        // A stable sort: items of one turn and one time keep their order.
        items.sort_by_key(|(turn, since, _)| (*turn, *since));
        match items.into_iter().next().map(|(_, _, item)| item) {
            Some(Recheckable::Kept(mapping)) => {
                self.recheck(*mapping).await;
            }
            Some(Recheckable::Held(key)) => {
                info!("A proof checks the held {key} again");
                self.checked_hold(&key);
                if let Some((_, recheck)) = self.holds.get() {
                    recheck(&key);
                }
            }
            None => {}
        }
    }

    /// Checks one kept file again: one lookup, read as what was heard is read, and the search for
    /// a better copy only when one could then be proven.
    pub async fn recheck(&self, mapping: LocalSongMapping) -> Reread {
        let Some(heart) = mapping.heart.clone() else {
            return Reread::Unread;
        };
        self.mark(&kept_item(&mapping));
        let read = self.reread(&heart).await;
        info!("Checked the kept-as-heard {} again: {read:?}", mapping.local_path);
        if read == Reread::AnotherSong {
            self.another_song(&mapping, &heart);
            return read;
        }
        if self.worth_a_search(&mapping, &heart, &read)
            && let Some(upgrade) = self.upgrade.get().cloned()
        {
            self.read.lock().insert(mapping.key(), read.clone());
            upgrade(mapping).await;
        }
        read
    }

    /// Whether a search for a better copy of a kept file could prove one: by its sound as it now
    /// reads, or, for a file kept anyway that nobody heard, against what its person has heard of the
    /// song since, whose master a copy may then be.
    fn worth_a_search(&self, mapping: &LocalSongMapping, heart: &KeptHeart, read: &Reread) -> bool {
        read.can_succeed()
            || (!heart.as_heard
                && matches!(read, Reread::OnlyHeardOrMaster | Reread::Unread)
                && heart
                    .users
                    .first()
                    .is_some_and(|user| self.registry.heard_by(&mapping.external_id, user).is_some()))
    }

    /// The kept file is another song than the one hearted: its video is refused for the song, as
    /// a heart's gate refuses what played when it was another song, and the heart holds "not yet
    /// proven" until a play of the song proves a copy. The kept file stays where it is.
    fn another_song(&self, mapping: &LocalSongMapping, heart: &KeptHeart) {
        if let Some(video) = heart.heard.as_ref().and_then(|heard| heard.source.video_id()) {
            let routing = self
                .registry
                .resolve_song(&mapping.external_id, false)
                .map(|shared| shared.snapshot());
            let artist = routing
                .as_ref()
                .and_then(|routing| routing.artist.clone())
                .unwrap_or_else(|| mapping.artist.clone());
            let title = routing
                .as_ref()
                .and_then(|routing| routing.title.clone())
                .unwrap_or_else(|| mapping.title.clone());
            info!(
                "The kept {} is another song than {artist} - {title}; its video {video} is refused for the song",
                mapping.local_path
            );
            self.registry.refuse_video(&mapping.external_id, video);
            self.registry.refuse_song_video(&artist, &title, video);
        }
        if let Some(hold) = self.hold.get() {
            hold(
                &mapping.external_provider,
                &mapping.external_id,
                &heart.users,
                ANOTHER_SONG_PLAYED,
            );
        }
    }

    /// The kept file's AcoustID answer read again, from the print kept with it, as the heart's
    /// gate reads what was heard against R. When the recording that played becomes R, R is it and
    /// its same-note duplicates, asked of MusicBrainz as the gate asks them; MusicBrainz not
    /// answering changes nothing until the next event.
    pub async fn reread(&self, heart: &KeptHeart) -> Reread {
        let (Some(print), Some(seconds)) = (heart.print.as_deref(), heart.seconds) else {
            return Reread::Unread;
        };
        let r = heart.recording.clone().unwrap_or_default();
        let min = self.settings.current().soulseek.effective_min_score_fraction();
        let outcome = self
            .acoust_id
            .lookup_answer(LookupClass::Bulk, print, seconds)
            .await;
        match read_heard_answer(&outcome, &r, min) {
            HeardAnswerReading::Busy => Reread::Busy,
            HeardAnswerReading::NeedsClip => Reread::Unread,
            HeardAnswerReading::AnotherSong => Reread::AnotherSong,
            HeardAnswerReading::RStays { tag } => Reread::Stays { tag },
            HeardAnswerReading::Becomes(dominant) => match &outcome {
                AcoustIdOutcome::Answered(answer) => {
                    let questions = self.questions();
                    match became_with_duplicates(questions.as_ref(), answer, &dominant, &r, min).await {
                        Ok((r, tag)) => Reread::Became { r, tag },
                        Err(_) => Reread::Busy,
                    }
                }
                _ => Reread::OnlyHeardOrMaster,
            },
            HeardAnswerReading::OnlyHeardOrMaster => Reread::OnlyHeardOrMaster,
        }
    }

    /// The kept file's own print stored as its sample, so proof 2 compares each replacement with
    /// the kept file itself: the print kept with it, read back into its items. A file whose print
    /// was not kept, or whose sample is stored already, is left to proof 2, which prints the file.
    fn keep_reference(&self, heart: &KeptHeart, sample_id: &str) {
        let (Some(samples), Some(print)) = (self.samples.get(), heart.print.as_deref()) else {
            return;
        };
        if samples
            .prints(sample_id)
            .is_some_and(|prints| !prints.raw.is_empty())
        {
            return;
        }
        let Some(raw) = compressed_print::decompress(print).filter(|raw| !raw.is_empty()) else {
            return;
        };
        let mut prints = HeardPrints {
            raw,
            compressed: print.to_string(),
            seconds: 0,
            whole: false,
            reaches_end: false,
        };
        prints.seconds = prints.covered_seconds().round() as i32;
        samples.store(sample_id, &prints, None);
    }

    /// The gate a replacement of a kept file is judged by, so nothing short of a proof replaces
    /// it: A1 against R as it now reads, or A2 against the kept file, which is the heard audio,
    /// with proof 2 against the kept file too. For a file kept anyway (not heard), the heard audio
    /// is the hearting person's latest play of the song, and with none only A1 accepts. A kept
    /// file that is another song refuses its video and holds the heart, as a check of it does.
    pub async fn gate_for(&self, original_path: &str, requester: &str) -> KeptFor {
        let wanted = get_full_path(original_path);
        let Some(mapping) = self.library.get_mappings().await.into_iter().find(|mapping| {
            (mapping.heart.is_some() || mapping.kept_unproven())
                && dotnet::eq_ignore_case(&get_full_path(&mapping.local_path), &wanted)
        }) else {
            return KeptFor::NotKept;
        };
        let heart = mapping.heart.clone().unwrap_or_default();
        let user = heart
            .users
            .first()
            .cloned()
            .unwrap_or_else(|| requester.to_string());
        let mut gate = HeartGate {
            user: user.clone(),
            origin: AcquisitionOrigin::Heart,
            r: heart.recording.clone(),
            heard: None,
            heard_seconds: heart.seconds,
            reading: HeardReading::RStays { tag: None },
            stand_in: None,
        };
        if heart.as_heard {
            let known = self.read.lock().remove(&mapping.key());
            let read = match known {
                Some(read) => read,
                None => self.reread(&heart).await,
            };
            gate.reading = match read {
                Reread::Stays { tag } => HeardReading::RStays { tag },
                Reread::Became { r, tag } => {
                    gate.r = Some(r);
                    HeardReading::RBecame { tag }
                }
                Reread::OnlyHeardOrMaster | Reread::Unread => HeardReading::OnlyHeardOrMaster,
                Reread::AnotherSong => {
                    self.another_song(&mapping, &heart);
                    return KeptFor::Nothing(ANOTHER_SONG_PLAYED);
                }
                Reread::Busy => return KeptFor::Nothing(KEPT_BUSY),
            };
            let source = HeardSource::Library {
                path: mapping.local_path.clone(),
            };
            let sample_id = sample_id_of(&source);
            self.keep_reference(&heart, &sample_id);
            gate.heard = Some(HeardRecord {
                heard_at: mapping.downloaded_at,
                source,
                sample_id: Some(sample_id),
            });
        } else {
            gate.heard = self.registry.heard_by(&mapping.external_id, &user);
        }
        KeptFor::Gate(Box::new(gate))
    }
}

/// What a library file's own AcoustID answer says of it, read once before a Wrong song or a
/// Wrong version replacement downloads anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginalSound {
    /// Nothing of its sound is known: AcoustID is off, or the file could not be printed or read.
    Unread,
    /// AcoustID or MusicBrainz did not answer.
    Busy,
    /// No result reaches the match score: AcoustID does not know it.
    Empty,
    /// No result at the match score links a recording, or an unlinked fingerprint scores above
    /// every linked one.
    Unknown,
    /// One recording, clearly: it and the recordings MusicBrainz files under its codes with its
    /// note.
    Clear(HeardRecording),
    /// The same song, but AcoustID cannot tell which version: the song its dominant recording is.
    Ambiguous(SongName),
}

impl KeptRechecks {
    /// What `path`'s own sound says it is: one lookup of its print (of the length downloads are
    /// printed at, in the `fpcalc` queue's ordinary place) on `class`'s lane, sent with the file's
    /// length. Its dominant recording read clear (with that recording's same-note duplicates, as R
    /// becomes what played) is Clear; any other dominant recording is that recording's song.
    pub async fn original_sound(
        &self,
        path: &str,
        seconds: Option<i32>,
        class: LookupClass,
    ) -> OriginalSound {
        let Some(samples) = self.samples.get() else {
            return OriginalSound::Unread;
        };
        let settings = self.settings.current().soulseek.clone();
        let Some((print, decoded)) = samples
            .compressed_print(
                Path::new(path),
                settings.effective_fingerprint_seconds(),
                FpcalcClass::Other,
            )
            .await
        else {
            return OriginalSound::Unread;
        };
        let Some(length) = seconds.filter(|s| *s > 0).or(Some(decoded).filter(|s| *s > 0)) else {
            return OriginalSound::Unread;
        };
        let min = settings.effective_min_score_fraction();
        let answer = match self.acoust_id.lookup_answer(class, &print, length).await {
            AcoustIdOutcome::Answered(answer) => answer,
            AcoustIdOutcome::Busy => return OriginalSound::Busy,
            AcoustIdOutcome::Off | AcoustIdOutcome::Unreadable { .. } => return OriginalSound::Unread,
        };
        let reading = read_answer(&answer, &HeardRecording::default(), min);
        if reading.reading == Reading::Empty {
            return OriginalSound::Empty;
        }
        let Some(dominant) = reading.dominant().cloned() else {
            return OriginalSound::Unknown;
        };
        let named = HeardRecording {
            title: dominant.title.clone(),
            artist: dominant
                .artists
                .first()
                .map(|artist| artist.name.clone())
                .unwrap_or_default(),
            ..Default::default()
        };
        let questions = self.questions();
        match became_with_duplicates(questions.as_ref(), &answer, &dominant, &named, min).await {
            Ok((r, Some(_))) => OriginalSound::Clear(r),
            Ok((_, None)) => OriginalSound::Ambiguous(dominant.song()),
            Err(_) => OriginalSound::Busy,
        }
    }
}

/// Whether a library file is checked again on the events tied to work: one a heart kept with no
/// proven recording, kept as heard or kept anyway.
fn rechecked(mapping: &LocalSongMapping) -> bool {
    mapping.heart.is_some() && mapping.kept_unproven()
}

/// A kept file's place in the order checks follow proofs in.
fn kept_item(mapping: &LocalSongMapping) -> String {
    format!("kept|{}", mapping.key())
}

/// A hold's place in the order checks follow proofs in.
fn held_item(key: &str) -> String {
    format!("held|{key}")
}

/// Why nothing is tried for a kept file while AcoustID does not answer.
pub const KEPT_BUSY: &str = "AcoustID is busy, try again";

/// What a replacement of a library file is judged by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeptFor {
    /// Nobody's heart kept the file: the replacement keeps its own checks.
    NotKept,
    /// A heart kept the file: only a copy this gate passes replaces it.
    Gate(Box<HeartGate>),
    /// A heart kept the file and nothing can be proven against it now, with why.
    Nothing(&'static str),
}

#[cfg(test)]
#[path = "upgrade_queue_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "kept_rechecks_tests.rs"]
mod kept_tests;

#[cfg(test)]
#[path = "kept_proof_two_tests.rs"]
mod kept_proof_two_tests;

#[cfg(test)]
#[path = "kept_pool_tests.rs"]
mod kept_pool_tests;

#[cfg(test)]
#[path = "kept_anyway_rechecks_tests.rs"]
mod kept_anyway_rechecks_tests;
