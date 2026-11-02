//! The trickle worker: fetches an import's missing songs a few an hour.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use octo_core::common::Clock;
use octo_core::common::acquisition_tracker::user_safe;
use octo_core::common::dotnet::{eq_ignore_case, to_lower_invariant};
use octo_core::fingerprint::heart_gate::AcquisitionOrigin;
use octo_core::settings::{HeartDownloadSource, SettingsStore};
use octo_core::soulseek::soulseek_link::SoulseekLinkState;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::TrickleJob;
use super::import_models::import_track_states;
use super::trickle_queue::TrickleQueue;
use crate::services::common::acquisition_tracker::{
    AcquisitionEnd, AcquisitionSnapshot, AcquisitionState, AcquisitionTracker, key_of,
};
use crate::services::common::{HeartAcquisitionCoordinator, TrackAcquisitionQueue};
use crate::services::library::LibraryOwnership;
use crate::services::soulseek::{ExternalIdRegistry, ISoulseekLink, SoulseekMetadataService};
use crate::services::spotiflac::SpotiflacClient;

/// A song the trickle may not fetch for this person now (a family member whose library is full,
/// or who only saves songs), with the words they read. The job is skipped rather than failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TrickleHeld(pub String);

/// A job whose song could not be checked yet (the music database did not answer, or the
/// library could not be read): put back with these words, to be checked again on a later turn.
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub struct TrickleCheckLater {
    pub detail: String,
    /// Whether it gives its turn back, so the next song may start at once: when only songs like
    /// it wait, not when every song would wait the same way.
    pub turn_back: bool,
}

/// What a job says while its song is checked against the music database.
const CHECKING: &str = "Checking against the music database";

/// What a job says while the library cannot be read for its last look.
const READING_LIBRARY: &str = "Waiting to read your library";

/// What a job says while MusicBrainz, which has to check its song, is not answering.
pub(crate) const WAITING_FOR_ANSWER: &str = "Waiting for MusicBrainz to answer";

/// What the trickle is doing, in a word the dashboard and the apps know, and when it starts its
/// next song.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrickleStatus {
    pub state: &'static str,
    pub next_utc: Option<DateTime<Utc>>,
    pub per_hour: i32,
}

/// The trickle's words for [`TrickleStatus::state`]. A contract with the dashboard and the apps.
pub mod trickle_states {
    /// Nothing queued.
    pub const IDLE: &str = "idle";
    /// Working, or waiting for its next turn.
    pub const RUNNING: &str = "running";
    /// The asker paused theirs.
    pub const PAUSED: &str = "paused";
    /// Songs per hour is 0.
    pub const OFF: &str = "off";
    /// Someone's own downloads come first.
    pub const YIELDING: &str = "yielding";
    /// Soulseek is out and nothing else can fetch.
    pub const WAITING_FOR_SOULSEEK: &str = "waitingForSoulseek";
    /// Mornye is the lossless source left, and its check keeps it from every extension.
    pub const WAITING_FOR_MORNYE_CHECK: &str = "waitingForMornyeCheck";
    /// MusicBrainz is not answering and every waiting song has an ISRC it has to check.
    pub const WAITING_FOR_MUSIC_DATABASE: &str = "waitingForMusicDatabase";

    /// The trickle's state in the words a person reads, the dashboard and the apps alike.
    /// Running reads "Fetching" while a song downloads.
    pub fn label(state: &str, downloading: bool) -> &'static str {
        match state {
            IDLE => "Nothing to fetch",
            RUNNING if downloading => "Fetching",
            RUNNING => "Waiting for the next turn",
            PAUSED => "Paused",
            OFF => "Off",
            YIELDING => "Waiting for other downloads",
            WAITING_FOR_SOULSEEK => "Waiting for Soulseek",
            WAITING_FOR_MORNYE_CHECK => "Waiting for Mornye's check",
            WAITING_FOR_MUSIC_DATABASE => "Waiting for MusicBrainz",
            _ => "Working on it",
        }
    }
}

/// The heart chain's lossless step is the Soulseek step with no Lidarr after it: an outage there
/// leaves no source, so a trickle in no hurry waits rather than fall back or hold.
fn soulseek_step_alone(settings: &SettingsStore) -> bool {
    let steps: Vec<HeartDownloadSource> = settings
        .current()
        .subsonic
        .effective_heart_download_sources()
        .into_iter()
        .filter(|step| step.song_enabled == Some(true))
        .map(|step| step.source)
        .collect();
    steps.contains(&HeartDownloadSource::Soulseek) && !steps.contains(&HeartDownloadSource::Lidarr)
}

/// slskd is logged out on a Soulseek step with nothing after it, and Mornye cannot stand in for
/// it (off, not answering, or unable to download). A Mornye that only waits for its check stands
/// in: that wait is [`mornye_keeps_the_step`]. slskd is read first, so Mornye is never asked
/// while slskd can be used.
pub(crate) async fn soulseek_keeps_the_step(
    settings: &SettingsStore,
    soulseek: &dyn ISoulseekLink,
    mornye: Option<&SpotiflacClient>,
) -> bool {
    if !soulseek_step_alone(settings) {
        return false;
    }
    let logged_out = soulseek
        .read(false)
        .await
        .is_some_and(|reading| reading.link == SoulseekLinkState::NotLoggedIn);
    if !logged_out {
        return false;
    }
    match mornye {
        Some(client) => !client.stands_in().await,
        None => true,
    }
}

/// Mornye is the lossless source left on a Soulseek step with nothing after it (slskd off,
/// unreachable or logged out), and its check keeps it from every extension a song could use.
pub(crate) async fn mornye_keeps_the_step(
    settings: &SettingsStore,
    soulseek: &dyn ISoulseekLink,
    mornye: Option<&SpotiflacClient>,
) -> bool {
    let Some(client) = mornye.filter(|client| client.is_on()) else {
        return false;
    };
    if !soulseek_step_alone(settings) {
        return false;
    }
    // slskd still fetches: Mornye's check keeps nothing from the step, and Mornye is not asked.
    let slskd_out = soulseek.is_off()
        || soulseek
            .read(false)
            .await
            .is_none_or(|reading| reading.link == SoulseekLinkState::NotLoggedIn);
    if !slskd_out || !client.stands_in().await {
        return false;
    }
    let Some(view) = client.check_view(None).await else {
        return false;
    };
    view.pause(&client.settings().effective_services(), chrono::Utc::now())
        .is_some()
}

/// A song the library already has: its id there, when Navidrome said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSong(pub Option<String>);

pub type OwnedSeam =
    Arc<dyn Fn(TrickleJob) -> BoxFuture<'static, anyhow::Result<Option<OwnedSong>>> + Send + Sync>;
pub type StartSeam = Arc<dyn Fn(TrickleJob) -> BoxFuture<'static, anyhow::Result<String>> + Send + Sync>;
pub type RowSeam = Arc<dyn Fn(&str) -> Option<AcquisitionSnapshot> + Send + Sync>;
pub type BusySeam = Arc<dyn Fn() -> bool + Send + Sync>;
pub type SoulseekOutSeam = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;
pub type FinishedListener = Arc<dyn Fn(&TrickleJob) + Send + Sync>;
/// The places, in the jobs it is given, of the queued songs that wait for MusicBrainz to answer.
pub type HeldSeam = Arc<dyn Fn(Vec<TrickleJob>) -> BoxFuture<'static, HashSet<usize>> + Send + Sync>;
pub type HeldListener = Arc<dyn Fn(&str) + Send + Sync>;

/// Seams, so the pace and the outcomes can be tested without downloading anything.
#[derive(Clone)]
pub struct TrickleSeams {
    pub owned: OwnedSeam,
    pub start: StartSeam,
    pub row: RowSeam,
    pub people_downloading: BusySeam,
    pub soulseek_out: SoulseekOutSeam,
    /// Mornye is the lossless source left and its check keeps it from every extension.
    pub mornye_out: SoulseekOutSeam,
    /// The places, in the jobs it is given, of the queued songs that wait for MusicBrainz to answer.
    pub held: HeldSeam,
    pub clock: Clock,
}

impl Default for TrickleSeams {
    /// A worker with no services: nothing owned, no downloads set up.
    fn default() -> Self {
        TrickleSeams {
            owned: Arc::new(|_| Box::pin(async { Ok(None) })),
            start: Arc::new(|_| {
                Box::pin(async { Err(anyhow::anyhow!("Downloads are not set up on this server.")) })
            }),
            row: Arc::new(|_| None),
            people_downloading: Arc::new(|| false),
            soulseek_out: Arc::new(|| Box::pin(async { false })),
            mornye_out: Arc::new(|| Box::pin(async { false })),
            held: Arc::new(|_| Box::pin(async { HashSet::new() })),
            clock: Clock::system(),
        }
    }
}

/// Works through the missing songs a few an hour, through the same chain a heart takes: the
/// library check first (a song already there is done, never downloaded twice), then the heart
/// sources in their order, Soulseek's outage hold, Lidarr, parallel downloads and all, so an
/// import is fetched exactly the way a heart would be. It never queues ahead of a person: while
/// anyone's own download runs, it waits, and while Soulseek is out with nothing behind it, it
/// waits for Soulseek rather than taking a YouTube copy.
///
/// One song at a time, started no closer together than the songs-per-hour setting allows. A song
/// handed to Lidarr is followed until Lidarr's import lands, without holding the next one back.
pub struct TrickleWorker {
    queue: Arc<TrickleQueue>,
    settings: Arc<SettingsStore>,
    seams: Mutex<TrickleSeams>,
    running: Mutex<Option<JoinHandle<()>>>,
    /// The song whose chain is running now, set before it starts and cleared once the chain
    /// returns.
    current: Mutex<Option<(String, String)>>,
    waiting_for: Mutex<&'static str>,
    finished: Mutex<Vec<FinishedListener>>,
    /// The queued songs held for MusicBrainz at the latest tick, by owner and key.
    held: Mutex<HashSet<String>>,
    held_listeners: Mutex<Vec<HeldListener>>,
}

/// The services the real trickle fetches with.
pub struct TrickleServices {
    pub owned: Arc<LibraryOwnership>,
    /// The import matcher, for a last look through a song's ISRC before it is fetched.
    pub matcher: Option<Arc<super::ImportMatcher>>,
    pub ids: Arc<ExternalIdRegistry>,
    pub hearts: Arc<HeartAcquisitionCoordinator>,
    pub downloads: Arc<TrackAcquisitionQueue>,
    pub tracker: Arc<AcquisitionTracker>,
    pub soulseek: Arc<dyn ISoulseekLink>,
    /// Mornye, which stands in for Soulseek on the Soulseek step while it can.
    pub spotiflac: Option<Arc<SpotiflacClient>>,
}

impl TrickleWorker {
    pub const TICK_EVERY: Duration = Duration::from_secs(5);
    /// A hand-off nobody reports on is given up after this long.
    pub const GIVE_UP_AFTER: TimeDelta = TimeDelta::hours(12);
    pub const MAX_ATTEMPTS: i32 = 3;

    /// With the given seams, as the tests build it.
    pub fn new(queue: Arc<TrickleQueue>, settings: Arc<SettingsStore>, seams: TrickleSeams) -> Arc<Self> {
        Arc::new(TrickleWorker {
            queue,
            settings,
            seams: Mutex::new(seams),
            running: Mutex::new(None),
            current: Mutex::new(None),
            waiting_for: Mutex::new(trickle_states::IDLE),
            finished: Mutex::new(Vec::new()),
            held: Mutex::new(HashSet::new()),
            held_listeners: Mutex::new(Vec::new()),
        })
    }

    /// Over the real heart chain, following the tracker's rows as they end.
    pub fn wired(
        queue: Arc<TrickleQueue>,
        settings: Arc<SettingsStore>,
        services: TrickleServices,
    ) -> Arc<Self> {
        let TrickleServices {
            owned,
            matcher,
            ids,
            hearts,
            downloads,
            tracker,
            soulseek,
            spotiflac,
        } = services;
        let mornye_settings = settings.clone();
        let mornye_soulseek = soulseek.clone();
        let mornye_client = spotiflac.clone();
        let start_queue = queue.clone();
        let look_queue = queue.clone();
        let start_tracker = tracker.clone();
        let row_tracker = tracker.clone();
        let out_settings = settings.clone();
        let held_matcher = matcher.clone();
        let seams = TrickleSeams {
            owned: Arc::new(move |job| {
                let (owned, matcher, queue) = (owned.clone(), matcher.clone(), look_queue.clone());
                Box::pin(async move {
                    let by_name = owned
                        .find(
                            Some(&job.artist),
                            Some(&job.title),
                            job.seconds,
                            job.album.as_deref(),
                        )
                        .await
                        .map(|copy| OwnedSong(copy.navidrome_id));
                    if by_name.is_none()
                        && matcher.as_ref().is_some_and(|matcher| {
                            matcher.has_pending_isrcs(std::slice::from_ref(&job.as_track()))
                        })
                    {
                        queue.say_while_running(&job.owner, &job.key, CHECKING);
                    }
                    last_look(by_name, matcher.as_deref(), &job).await
                })
            }),
            start: Arc::new(move |job| {
                let (ids, hearts, queue, tracker) = (
                    ids.clone(),
                    hearts.clone(),
                    start_queue.clone(),
                    start_tracker.clone(),
                );
                Box::pin(async move {
                    // A family member's songs go to their own library; one whose library is full
                    // (or who only saves songs) gets nothing until that changes.
                    let target = hearts.import_target(&job.owner).map_err(TrickleHeld)?;
                    let external_id = ids.register(SoulseekRouting {
                        kind: RoutingKind::Song,
                        artist: Some(job.artist.clone()),
                        title: Some(job.title.clone()),
                        album: job.album.clone(),
                        duration: job.seconds,
                        isrc: job.isrc.clone(),
                        ..Default::default()
                    });
                    let provider = SoulseekMetadataService::PROVIDER_NAME;
                    let key = key_of(provider, &external_id);
                    queue.update(&job.owner, &job.key, |j| j.acquisition_key = Some(key.clone()));
                    // The tracker follows only rows that were opened, so the download gets one,
                    // as a heart's does.
                    tracker.begin(
                        provider,
                        &external_id,
                        None,
                        Some(&job.owner),
                        Some(&job.artist),
                        Some(&job.title),
                        job.album.as_deref(),
                    );
                    // An import whose identity MusicBrainz could not answer comes back as
                    // TrickleCheckLater, and is put back for its next turn.
                    hearts
                        .acquire_track_for(
                            provider,
                            &external_id,
                            Some(&job.owner),
                            None,
                            &target,
                            AcquisitionOrigin::Import,
                        )
                        .await?;
                    Ok(key)
                })
            }),
            row: Arc::new(move |key| {
                row_tracker
                    .all()
                    .into_iter()
                    .find(|row| key_of(&row.provider, &row.external_id) == key)
            }),
            people_downloading: Arc::new(move || !downloads.is_idle()),
            soulseek_out: Arc::new(move || {
                let (settings, soulseek, mornye) =
                    (out_settings.clone(), soulseek.clone(), spotiflac.clone());
                Box::pin(async move {
                    soulseek_keeps_the_step(&settings, soulseek.as_ref(), mornye.as_deref()).await
                })
            }),
            mornye_out: Arc::new(move || {
                let (settings, soulseek, mornye) = (
                    mornye_settings.clone(),
                    mornye_soulseek.clone(),
                    mornye_client.clone(),
                );
                Box::pin(async move {
                    mornye_keeps_the_step(&settings, soulseek.as_ref(), mornye.as_deref()).await
                })
            }),
            held: held_by(held_matcher),
            clock: Clock::system(),
        };
        let worker = Self::new(queue, settings, seams);
        let weak = Arc::downgrade(&worker);
        tracker.subscribe_ended(Arc::new(move |end: &AcquisitionEnd| {
            if let Some(worker) = weak.upgrade() {
                worker.on_ended(end);
            }
        }));
        worker
    }

    /// Changes the seams (tests use it).
    pub fn configure(&self, change: impl FnOnce(&mut TrickleSeams)) {
        change(&mut self.seams.lock());
    }

    fn seams(&self) -> TrickleSeams {
        self.seams.lock().clone()
    }

    fn now(&self) -> DateTime<Utc> {
        self.seams.lock().clock.now()
    }

    /// A song finished: fetched (with its library id when Navidrome showed it) or not.
    pub fn on_finished(&self, listener: FinishedListener) {
        self.finished.lock().push(listener);
    }

    /// Someone has songs held for MusicBrainz: told their name on each tick that holds one.
    pub fn on_held(&self, listener: HeldListener) {
        self.held_listeners.lock().push(listener);
    }

    /// What the trickle is doing for this person.
    pub fn status_for(&self, owner: &str) -> TrickleStatus {
        let per_hour = self.settings.current().imports.effective_songs_per_hour();
        let jobs = self.queue.snapshot(Some(owner));
        let open = jobs.iter().any(|job| import_track_states::open(&job.state));
        if !open {
            return TrickleStatus {
                state: trickle_states::IDLE,
                next_utc: None,
                per_hour,
            };
        }
        if self.queue.is_paused(owner) {
            return TrickleStatus {
                state: trickle_states::PAUSED,
                next_utc: None,
                per_hour,
            };
        }
        if per_hour == 0 {
            return TrickleStatus {
                state: trickle_states::OFF,
                next_utc: None,
                per_hour,
            };
        }
        if jobs.iter().any(|job| job.state == import_track_states::QUEUED) {
            // Every waiting song held: nothing goes when someone's downloads end or Soulseek is
            // back, so that is what it waits for.
            let all_held = {
                let held = self.held.lock();
                !jobs
                    .iter()
                    .any(|job| job.state == import_track_states::DOWNLOADING)
                    && jobs
                        .iter()
                        .filter(|job| job.state == import_track_states::QUEUED)
                        .all(|job| held.contains(&held_id(&job.owner, &job.key)))
            };
            if all_held {
                return TrickleStatus {
                    state: trickle_states::WAITING_FOR_MUSIC_DATABASE,
                    next_utc: None,
                    per_hour,
                };
            }
            let waiting = *self.waiting_for.lock();
            if matches!(
                waiting,
                trickle_states::YIELDING
                    | trickle_states::WAITING_FOR_SOULSEEK
                    | trickle_states::WAITING_FOR_MORNYE_CHECK
            ) {
                return TrickleStatus {
                    state: waiting,
                    next_utc: None,
                    per_hour,
                };
            }
        }
        TrickleStatus {
            state: trickle_states::RUNNING,
            next_utc: self.next_start(per_hour),
            per_hour,
        }
    }

    fn next_start(&self, per_hour: i32) -> Option<DateTime<Utc>> {
        if per_hour <= 0 {
            return None;
        }
        let now = self.now();
        let next = match self.queue.last_start_utc() {
            None => now,
            Some(last) => last + TimeDelta::microseconds((3600.0 / f64::from(per_hour) * 1_000_000.0) as i64),
        };
        Some(if next < now { now } else { next })
    }

    /// One pass: settle hand-offs, then start the next song when its turn has come. True when one
    /// started.
    pub async fn tick(self: &Arc<Self>) -> bool {
        self.settle_hand_offs();
        let seams = self.seams();
        // A song running when the tick began keeps the turn, even if it ends while the hold is
        // worked out: a song it put back is not taken again in the same tick.
        let was_running = self
            .running
            .lock()
            .as_ref()
            .is_some_and(|task| !task.is_finished());
        let (held, seen) = self.hold_for_music_database(&seams).await;
        if was_running
            || self
                .running
                .lock()
                .as_ref()
                .is_some_and(|task| !task.is_finished())
        {
            return false;
        }
        let per_hour = self.settings.current().imports.effective_songs_per_hour();
        if per_hour == 0 {
            return false;
        }
        if self.next_start(per_hour).is_some_and(|next| next > self.now()) {
            return false;
        }
        if (seams.people_downloading)() {
            *self.waiting_for.lock() = trickle_states::YIELDING;
            return false;
        }
        if (seams.soulseek_out)().await {
            *self.waiting_for.lock() = trickle_states::WAITING_FOR_SOULSEEK;
            return false;
        }
        if (seams.mornye_out)().await {
            *self.waiting_for.lock() = trickle_states::WAITING_FOR_MORNYE_CHECK;
            return false;
        }
        *self.waiting_for.lock() = trickle_states::RUNNING;
        // A song queued since the hold was worked out is asked about when it comes up; one that
        // waits too goes back with its turn, and the next is taken.
        let mut passed: HashSet<String> = HashSet::new();
        let job = loop {
            let Some(job) = self.queue.take_next_where(|job| {
                let id = held_id(&job.owner, &job.key);
                !held.contains(&id) && !passed.contains(&id)
            }) else {
                *self.waiting_for.lock() = trickle_states::IDLE;
                return false;
            };
            let id = held_id(&job.owner, &job.key);
            if seen.contains(&id) || (seams.held)(vec![job.clone()]).await.is_empty() {
                break job;
            }
            self.queue
                .put_back(&job.owner, &job.key, WAITING_FOR_ANSWER, true);
            passed.insert(id);
        };
        *self.current.lock() = Some((job.owner.clone(), job.key.clone()));
        let worker = self.clone();
        *self.running.lock() = Some(tokio::spawn(async move { worker.run(job).await }));
        true
    }

    /// The queued songs that wait for MusicBrainz to answer, and every queued song seen, by owner
    /// and key, as the seam names them by place. Each says so, a
    /// song no longer held loses the words, and everyone with one held is told, so their ISRCs
    /// are asked about. A held song is not taken, so nothing here asks MusicBrainz; its next
    /// answer to anyone ends the hold.
    async fn hold_for_music_database(&self, seams: &TrickleSeams) -> (HashSet<String>, HashSet<String>) {
        let queued: Vec<TrickleJob> = self
            .queue
            .snapshot(None)
            .into_iter()
            .filter(|job| job.state == import_track_states::QUEUED)
            .collect();
        let places = if queued.is_empty() {
            HashSet::new()
        } else {
            (seams.held)(queued.clone()).await
        };
        let mut held = HashSet::new();
        let seen: HashSet<String> = queued.iter().map(|job| held_id(&job.owner, &job.key)).collect();
        let mut owners: Vec<String> = Vec::new();
        let mut words: Vec<(String, String, Option<String>)> = Vec::new();
        for (at, job) in queued.iter().enumerate() {
            let says = job.detail.as_deref() == Some(WAITING_FOR_ANSWER);
            if places.contains(&at) {
                held.insert(held_id(&job.owner, &job.key));
                if !owners.iter().any(|owner| eq_ignore_case(owner, &job.owner)) {
                    owners.push(job.owner.clone());
                }
                if !says {
                    words.push((
                        job.owner.clone(),
                        job.key.clone(),
                        Some(WAITING_FOR_ANSWER.to_string()),
                    ));
                }
            } else if says {
                words.push((job.owner.clone(), job.key.clone(), None));
            }
        }
        self.queue.set_queued_details(&words);
        *self.held.lock() = held.clone();
        let listeners = self.held_listeners.lock().clone();
        for owner in &owners {
            for listener in &listeners {
                listener(owner);
            }
        }
        (held, seen)
    }

    /// Waits for the song started last. Only tests need it.
    pub async fn drain(&self) {
        let running = self.running.lock().take();
        if let Some(task) = running {
            let _ = task.await;
        }
    }

    async fn run(&self, job: TrickleJob) {
        let seams = self.seams();
        let outcome: anyhow::Result<()> = async {
            if let Some(owned) = (seams.owned)(job.clone()).await? {
                self.queue.end_check(&job.owner, &job.key);
                self.finish(
                    &job,
                    import_track_states::DONE,
                    Some("Already in your library"),
                    owned.0,
                );
                return Ok(());
            }
            // Taken back or skipped while it was checked: nothing starts.
            if !self.queue.begin_attempt(&job.owner, &job.key) {
                return Ok(());
            }
            let key = (seams.start)(job.clone()).await?;
            // The chain is over: a song in the library, a failure, or a hand-off to Lidarr still
            // running. The tracker's row says which; a hand-off is settled when it ends.
            *self.current.lock() = None;
            self.settle(&job.owner, &job.key, (seams.row)(&key));
            Ok(())
        }
        .await;
        if let Err(e) = outcome {
            if let Some(later) = e.downcast_ref::<TrickleCheckLater>() {
                self.queue
                    .put_back(&job.owner, &job.key, &later.detail, later.turn_back);
                *self.current.lock() = None;
                return;
            }
            if let Some(held) = e.downcast_ref::<TrickleHeld>() {
                info!(
                    "Trickle for '{} - {}' ({}) skipped: {held}",
                    job.artist, job.title, job.owner
                );
                self.finish(&job, import_track_states::SKIPPED, Some(&held.0), None);
                *self.current.lock() = None;
                return;
            }
            warn!(
                "Trickle for '{} - {}' ({}) failed: {e}",
                job.artist, job.title, job.owner
            );
            let detail = user_safe(Some(&e.to_string())).unwrap_or_else(|| "Could not get it.".to_string());
            self.finish(&job, import_track_states::NOT_FOUND, Some(&detail), None);
        }
        *self.current.lock() = None;
    }

    fn is_current(&self, job: &TrickleJob) -> bool {
        self.current
            .lock()
            .as_ref()
            .is_some_and(|(owner, key)| *key == job.key && eq_ignore_case(owner, &job.owner))
    }

    fn on_ended(&self, end: &AcquisitionEnd) {
        // The song whose chain is still running is settled when the chain returns: a source in
        // the middle of the chain failing is not the end of it.
        let Some(job) = self.queue.by_acquisition(&end.key) else {
            return;
        };
        if self.is_current(&job) {
            return;
        }
        let row = (self.seams().row)(&end.key);
        self.settle(&job.owner, &job.key, row);
        debug!("Trickle settled {} on its download's end", job.key);
    }

    /// Songs whose chain returned with a download still running elsewhere (Lidarr), and ones
    /// nothing reports on.
    fn settle_hand_offs(&self) {
        let row = self.seams().row;
        for job in self
            .queue
            .snapshot(None)
            .into_iter()
            .filter(|job| job.state == import_track_states::DOWNLOADING && !self.is_current(job))
        {
            let found = job.acquisition_key.as_deref().and_then(|key| row(key));
            self.settle(&job.owner, &job.key, found);
        }
    }

    fn settle(&self, owner: &str, track_key: &str, row: Option<AcquisitionSnapshot>) {
        let Some(job) = self
            .queue
            .get(owner, track_key)
            .filter(|job| job.state == import_track_states::DOWNLOADING)
        else {
            return;
        };
        let age = job
            .started_utc
            .map_or(TimeDelta::zero(), |started| self.now() - started);
        match row.as_ref().map(|r| r.state) {
            Some(AcquisitionState::Done) => {
                let row = row.as_ref().expect("a row");
                let detail = match &row.source {
                    Some(source) => format!("Fetched from {source}"),
                    None => "Fetched".to_string(),
                };
                self.finish(
                    &job,
                    import_track_states::DONE,
                    Some(&detail),
                    row.library_id.clone(),
                );
            }
            Some(AcquisitionState::Failed) => {
                let row = row.as_ref().expect("a row");
                let detail = row
                    .error
                    .clone()
                    .unwrap_or_else(|| "No download source could get this song.".to_string());
                self.finish(&job, import_track_states::NOT_FOUND, Some(&detail), None);
            }
            None => {
                // No row: it aged out of the tracker, or never opened. The library check at the
                // start of its next turn says whether it arrived; three times without one is
                // enough.
                if job.attempts >= Self::MAX_ATTEMPTS {
                    self.finish(
                        &job,
                        import_track_states::NOT_FOUND,
                        Some("Octo lost track of this download three times."),
                        None,
                    );
                } else {
                    self.queue.update(owner, track_key, |j| {
                        j.state = import_track_states::QUEUED.to_string();
                        j.detail = Some("Checking again".to_string());
                    });
                }
            }
            Some(_) => {
                if age > Self::GIVE_UP_AFTER {
                    self.finish(
                        &job,
                        import_track_states::NOT_FOUND,
                        Some("Still not in the library after 12 hours."),
                        None,
                    );
                }
            }
        }
    }

    fn finish(&self, job: &TrickleJob, state: &str, detail: Option<&str>, library_id: Option<String>) {
        self.queue.update(&job.owner, &job.key, |j| {
            j.state = state.to_string();
            j.detail = detail.map(str::to_string);
            j.library_id = library_id;
        });
        info!(
            "Trickle: '{} - {}' for {}: {state} ({})",
            job.artist,
            job.title,
            job.owner,
            detail.unwrap_or("")
        );
        if let Some(finished) = self.queue.get(&job.owner, &job.key) {
            let listeners = self.finished.lock().clone();
            for listener in listeners {
                let told = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(&finished)));
                if told.is_err() {
                    warn!("A trickle listener failed");
                }
            }
        }
    }

    /// The hosted loop: a tick every five seconds until Octo stops.
    pub async fn run_loop(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        while !stopping.is_cancelled() {
            // Per-tick catch: a panic in one tick must not stop the trickle.
            let worker = self.clone();
            let tick = tokio::spawn(async move { worker.tick().await });
            if let Err(e) = tick.await {
                error!("Trickle tick failed: {e}");
            }
            tokio::select! {
                () = stopping.cancelled() => break,
                () = tokio::time::sleep(Self::TICK_EVERY) => {}
            }
        }
        Ok(())
    }
}

/// The library's copy of a job's song: the name check's, else the one the recordings its ISRC
/// names lead to. While the music database could not be asked, or the library could not be read,
/// the job is put back to be checked again on its next turn.
async fn last_look(
    by_name: Option<OwnedSong>,
    matcher: Option<&super::ImportMatcher>,
    job: &TrickleJob,
) -> anyhow::Result<Option<OwnedSong>> {
    if by_name.is_some() {
        return Ok(by_name);
    }
    let Some(matcher) = matcher else {
        return Ok(None);
    };
    let track = job.as_track();
    match matcher.owned_by_isrc(&job.owner, &track).await {
        super::import_matcher::IsrcOwnership::Owned(id) => Ok(Some(OwnedSong(Some(id)))),
        super::import_matcher::IsrcOwnership::Missing => Ok(None),
        super::import_matcher::IsrcOwnership::Unanswered => Err(TrickleCheckLater {
            detail: WAITING_FOR_ANSWER.to_string(),
            turn_back: true,
        }
        .into()),
        super::import_matcher::IsrcOwnership::LibraryUnread => Err(TrickleCheckLater {
            detail: READING_LIBRARY.to_string(),
            turn_back: false,
        }
        .into()),
    }
}

/// A job's name among the held songs: its owner, ignoring case, and its key.
fn held_id(owner: &str, key: &str) -> String {
    format!("{}\n{key}", to_lower_invariant(owner))
}

/// The held seam over the import matcher: the jobs whose ISRC MusicBrainz still has to answer
/// while it is out. None without a matcher.
pub fn held_by(matcher: Option<Arc<super::ImportMatcher>>) -> HeldSeam {
    Arc::new(move |jobs| {
        let matcher = matcher.clone();
        Box::pin(async move {
            let Some(matcher) = matcher else {
                return HashSet::new();
            };
            let tracks: Vec<super::ImportTrack> = jobs.iter().map(TrickleJob::as_track).collect();
            matcher.waiting_on_music_database(&tracks)
        })
    })
}

#[cfg(test)]
#[path = "trickle_worker_tests.rs"]
mod tests;
