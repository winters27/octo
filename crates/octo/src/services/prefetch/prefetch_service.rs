//! Getting upcoming songs ahead. Each now-playing (a scrobble with `submission=false`, or a
//! stream from its first byte) works out the songs that play next, and those are fetched as real
//! FLAC copies into the prefetch folder while nothing else is downloading, so they play lossless
//! from their first second. A heart on one later keeps that file instead of downloading again.
//!
//! A download ahead runs only while Octo may download more than one song at a time, and always
//! leaves one of those transfers free, so a heart never waits behind one. With DownloadOnPlay on,
//! the coming songs are queued as the plays they will be instead, one at a time, each only while
//! no real play is waiting.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use octo_core::common::Clock;
use octo_core::settings::{PrefetchSettings, SettingsStore};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::play_source::PlaySources;
use super::preview_copies::PreviewCopies;
use super::queue_window::{SavedQueue, window};
use super::store::{PrefetchStore, free_bytes, prepare_folder};
use crate::services::common::base_download_service::AheadBusy;
use crate::services::common::{DownloadConcurrency, HeartAcquisitionCoordinator, TrackAcquisitionQueue};
use crate::services::family::{FamilyCaller, LibraryWriteGate, audio_gate};
use crate::services::i_download_service::{Ahead, IDownloadService};
use crate::services::library::GeneratedPlaylistService;
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::soulseek_link::SoulseekLinkState;
use crate::services::soulseek::{ExternalIdRegistry, ISoulseekLink, RadioQueueStore};
use crate::services::spotiflac::{SpotiflacClient, mornye_waited};

/// A saved play queue older than this no longer says what plays next.
const SAVED_QUEUE_FOR: Duration = Duration::hours(6);
/// Nothing new is fetched once nobody has played anything for this long (PauseWhenStopped).
const STOPPED_AFTER: Duration = Duration::minutes(10);
/// A listener who played nothing for this long has no songs coming up any more.
const LISTENER_IDLE_AFTER: Duration = Duration::minutes(30);
/// Mixes are kept ready for the listeners who played in this long.
const MIXES_FOR: Duration = Duration::days(7);
/// A song that could not be fetched ahead is not tried again for this long.
const RETRY_AFTER: Duration = Duration::hours(6);
/// A song that could not be fetched because a source could not be reached is tried again after this.
const RETRY_UNREACHABLE_AFTER: Duration = Duration::minutes(10);
/// How often the folder is swept and the mixes kept ready.
const HOURLY: Duration = Duration::hours(1);
/// How often the worker looks again when nothing woke it.
const TICK: StdDuration = StdDuration::from_secs(30);
/// How many listeners' plays are remembered.
const MAX_LISTENERS: usize = 256;
/// How many failures the dashboard shows.
const ERRORS_SHOWN: usize = 5;

/// Why nothing new starts while downloads run one at a time.
pub const ONE_AT_A_TIME: &str = "Waits until Octo can download more than one song at a time";

/// What the service is built over.
pub struct PrefetchParts {
    pub settings: Arc<SettingsStore>,
    pub store: Arc<PrefetchStore>,
    pub downloads: Arc<dyn IDownloadService>,
    pub queue: Arc<TrackAcquisitionQueue>,
    pub coordinator: Arc<HeartAcquisitionCoordinator>,
    pub radio_queues: Arc<RadioQueueStore>,
    pub write_gate: Arc<LibraryWriteGate>,
    pub library: Arc<dyn ILocalLibraryService>,
    /// How many transfers may run at once: a download ahead always leaves one free. Without it,
    /// downloads are one at a time and nothing is fetched ahead.
    pub concurrency: Option<Arc<DownloadConcurrency>>,
    /// Names the songs being fetched, for the dashboard.
    pub names: Option<Arc<ExternalIdRegistry>>,
    /// Whether Soulseek is connected: nothing is fetched while it is not, unless SpotiFLAC is on.
    pub soulseek: Option<Arc<dyn ISoulseekLink>>,
    /// Mornye, whose check can keep it from every extension: nothing is fetched then either,
    /// while Soulseek cannot be used.
    pub spotiflac: Option<Arc<SpotiflacClient>>,
    pub clock: Clock,
}

/// Why a song is being fetched ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reason {
    /// It plays soon.
    Upcoming,
    /// It is kept ready in a mix, after everything that plays soon.
    Mix,
    /// It is in a playlist kept ready, with the mixes kept ready.
    Kept,
}

/// How a song is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Into the prefetch folder.
    Ahead,
    /// As the play it will be (DownloadOnPlay), into the library.
    AsPlay,
}

#[derive(Debug, Clone)]
struct Job {
    provider: String,
    external_id: String,
    /// The song's id as the player knows it.
    client_id: String,
    /// The listeners it is coming up for; none for a mix kept ready.
    listeners: Vec<String>,
    reason: Reason,
    kind: Kind,
}

impl Job {
    fn key(&self) -> String {
        format!("{}:{}", self.provider, self.external_id)
    }

    fn is_for(&self, user: &str) -> bool {
        self.listeners.iter().any(|l| user_key(l) == user_key(user))
    }
}

/// What the service knows of one listener's playing.
#[derive(Debug, Clone, Default)]
struct Listener {
    saved: Option<SavedQueue>,
    previous: Option<String>,
    playing: Option<String>,
    last_played: Option<DateTime<Utc>>,
    /// The name the listener signed in with.
    name: String,
}

#[derive(Default)]
struct State {
    listeners: HashMap<String, Listener>,
    queued: VecDeque<Job>,
    running: HashMap<String, (Job, CancellationToken)>,
    /// Songs not tried again until then: they failed, or turned out to be the listener's already.
    resting: HashMap<String, DateTime<Utc>>,
    /// For a resting song Mornye's check kept: the address of that Mornye and the checks the
    /// song waits on.
    resting_for_mornye: HashMap<String, (String, Vec<String>)>,
    errors: VecDeque<String>,
    last_played: Option<DateTime<Utc>>,
    last_sweep: Option<DateTime<Utc>>,
    last_mixes: Option<DateTime<Utc>>,
}

/// One song for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    pub artist: Option<String>,
    pub title: Option<String>,
}

/// The dashboard's view of the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrefetchStatus {
    pub enabled: bool,
    pub ready: usize,
    pub used_bytes: u64,
    pub cap_bytes: u64,
    pub free_bytes: Option<u64>,
    pub folder: String,
    pub running: Vec<JobView>,
    pub next: Vec<JobView>,
    pub errors: Vec<String>,
    /// Why nothing new is fetched right now, in words, or None.
    pub paused: Option<String>,
}

/// Getting upcoming songs ahead.
pub struct PrefetchService {
    parts: PrefetchParts,
    state: Mutex<State>,
    sources: PlaySources,
    wake: Notify,
    mixes: OnceLock<Arc<GeneratedPlaylistService>>,
    copies: OnceLock<Arc<PreviewCopies>>,
}

impl PrefetchService {
    pub fn new(parts: PrefetchParts) -> Arc<Self> {
        let sources = PlaySources::new(parts.clock.clone());
        Arc::new(PrefetchService {
            parts,
            state: Mutex::new(State::default()),
            sources,
            wake: Notify::new(),
            mixes: OnceLock::new(),
            copies: OnceLock::new(),
        })
    }

    /// How many songs are being fetched right now.
    #[cfg(test)]
    pub(crate) fn state_running(&self) -> usize {
        self.state.lock().running.len()
    }

    /// The songs waiting to be fetched, by id, in order.
    #[cfg(test)]
    pub(crate) fn queued_ids(&self) -> Vec<String> {
        self.state
            .lock()
            .queued
            .iter()
            .map(|job| job.external_id.clone())
            .collect()
    }

    /// The songs waiting to be fetched, in order, with the listeners each is for and whether it
    /// goes to the prefetch folder.
    #[cfg(test)]
    pub(crate) fn queued_jobs(&self) -> Vec<(String, Vec<String>, bool)> {
        self.state
            .lock()
            .queued
            .iter()
            .map(|job| {
                (
                    job.external_id.clone(),
                    job.listeners.clone(),
                    job.kind == Kind::Ahead,
                )
            })
            .collect()
    }

    /// Gives the service the mixes, for keeping them ready. Once only.
    pub fn use_mixes(&self, mixes: Arc<GeneratedPlaylistService>) {
        let _ = self.mixes.set(mixes);
    }

    /// Gives the service the preview copies, which it sweeps with its own folder. Once only.
    pub fn use_preview_copies(&self, copies: Arc<PreviewCopies>) {
        let _ = self.copies.set(copies);
    }

    /// The preview copies, once given.
    pub fn preview_copies(&self) -> Option<&Arc<PreviewCopies>> {
        self.copies.get()
    }

    fn settings(&self) -> PrefetchSettings {
        self.parts.settings.current().prefetch.clone()
    }

    fn now(&self) -> DateTime<Utc> {
        self.parts.clock.now()
    }

    /// The folder the songs are kept in.
    pub fn folder(&self) -> PathBuf {
        self.parts.store.folder()
    }

    pub fn store(&self) -> &Arc<PrefetchStore> {
        &self.parts.store
    }

    /// Which source each play started on.
    pub fn sources(&self) -> &PlaySources {
        &self.sources
    }

    /// Whether this listener's plays may keep a copy: anyone with Family off; with it on, not a
    /// member who must ask first (or whose library is full), and not a caller whose audio has a
    /// ceiling, who gets no file above it.
    pub fn may_keep_copies(&self, user: &str, caller: Option<&FamilyCaller>) -> bool {
        if !self.parts.settings.current().family.enabled {
            return true;
        }
        if caller.is_some_and(|caller| audio_gate::ceiling(caller).is_some()) {
            return false;
        }
        let managed = match caller {
            Some(caller) => caller.managed,
            None => self.parts.write_gate.is_member(Some(user)),
        };
        !managed || self.parts.write_gate.play_target(Some(user)).is_some()
    }

    /// Radio offered these songs: with Scope RadioOnly, only such songs are fetched ahead.
    pub fn note_radio(&self, ids: &[String]) {
        self.parts.radio_queues.note_radio(ids.iter().cloned());
    }

    /// A player saved its play queue: the songs in the order they play next.
    pub fn saved_queue(
        &self,
        user: &str,
        ids: Vec<String>,
        current: Option<String>,
        current_index: Option<usize>,
    ) {
        if user.trim().is_empty() {
            return;
        }
        let at = self.now();
        let mut state = self.state.lock();
        let listener = listener_mut(&mut state, user);
        listener.saved = Some(SavedQueue {
            ids,
            current,
            current_index,
            at,
        });
    }

    /// The saved play queue of a listener, for tests and the dashboard.
    pub fn saved_queue_of(&self, user: &str) -> Option<SavedQueue> {
        self.state
            .lock()
            .listeners
            .get(&user_key(user))
            .and_then(|listener| listener.saved.clone())
    }

    /// `user` started playing `id`. Works out the songs that play next and fetches them ahead:
    /// songs no longer coming up for anyone are dropped from the queue, and new ones join it.
    pub async fn on_playing(&self, user: &str, id: &str, caller: Option<&FamilyCaller>) {
        let settings = self.settings();
        if !settings.enabled || user.trim().is_empty() || id.is_empty() {
            return;
        }
        if !self.may_keep_copies(user, caller) {
            debug!("Not fetching ahead for {user}: their plays keep no copies");
            return;
        }
        let now = self.now();
        let (saved, previous) = {
            let mut state = self.state.lock();
            state.last_played = Some(now);
            let listener = listener_mut(&mut state, user);
            if listener.playing.as_deref() != Some(id) {
                listener.previous = listener.playing.take();
                listener.playing = Some(id.to_string());
            }
            listener.last_played = Some(now);
            (
                listener
                    .saved
                    .clone()
                    .filter(|saved| now - saved.at <= SAVED_QUEUE_FOR),
                listener.previous.clone(),
            )
        };
        let lists = self.parts.radio_queues.snapshot();
        let radio = &self.parts.radio_queues;
        let upcoming = window(
            saved.as_ref(),
            &lists,
            (previous.as_deref(), id),
            settings.effective_count(),
            settings.scope,
            |song| radio.is_radio(song),
        );
        let mut wanted: Vec<(String, String, String)> = Vec::new();
        for song in upcoming {
            let (outside, provider, external_id) = self.parts.library.parse_song_id(&song);
            let (Some(provider), Some(external_id)) = (provider, external_id) else {
                continue;
            };
            if !outside {
                continue;
            }
            if self.wanted(&provider, &external_id).await {
                wanted.push((provider, external_id, song));
            }
        }

        // DownloadOnPlay keeps every played song in the library: the songs coming up are
        // started early as the same downloads their plays would start.
        let kind = if self.parts.settings.current().subsonic.download_on_play {
            Kind::AsPlay
        } else {
            Kind::Ahead
        };
        {
            let mut state = self.state.lock();
            let keys: Vec<String> = wanted.iter().map(|(p, i, _)| format!("{p}:{i}")).collect();
            for job in state.queued.iter_mut() {
                if job.reason == Reason::Upcoming && !keys.contains(&job.key()) {
                    job.listeners.retain(|l| user_key(l) != user_key(user));
                }
            }
            state
                .queued
                .retain(|job| job.reason != Reason::Upcoming || !job.listeners.is_empty());
            for (provider, external_id, client_id) in wanted {
                let key = format!("{provider}:{external_id}");
                if let Some((job, _)) = state.running.get_mut(&key) {
                    if !job.is_for(user) {
                        job.listeners.push(user.to_string());
                    }
                    continue;
                }
                if let Some(at) = state.queued.iter().position(|j| j.key() == key) {
                    if state.queued[at].reason > Reason::Upcoming {
                        // Kept ready, and now coming up: it moves up to the songs that play soon.
                        if let Some(mut job) = state.queued.remove(at) {
                            job.reason = Reason::Upcoming;
                            job.kind = kind;
                            if !job.is_for(user) {
                                job.listeners.push(user.to_string());
                            }
                            let to = state
                                .queued
                                .iter()
                                .position(|j| j.reason > Reason::Upcoming)
                                .unwrap_or(state.queued.len());
                            state.queued.insert(to, job);
                        }
                    } else if let Some(job) = state.queued.get_mut(at)
                        && !job.is_for(user)
                    {
                        job.listeners.push(user.to_string());
                    }
                    continue;
                }
                let job = Job {
                    provider,
                    external_id,
                    client_id,
                    listeners: vec![user.to_string()],
                    reason: Reason::Upcoming,
                    kind,
                };
                // Before the mixes kept ready, after the songs already coming up.
                let at = state
                    .queued
                    .iter()
                    .position(|j| j.reason > Reason::Upcoming)
                    .unwrap_or(state.queued.len());
                state.queued.insert(at, job);
            }
        }
        self.wake.notify_one();
    }

    /// Queues the songs of the playlists kept ready, after everything else: `songs` is each
    /// listener's outside song ids. Songs queued for playlists kept ready before and not asked for
    /// now leave the queue. A listener whose plays keep no copies gets none.
    pub async fn keep_ready(&self, songs: Vec<(String, Vec<String>)>) {
        // Into the prefetch folder, under its space and days, even with DownloadOnPlay on: a song
        // kept ready is not a song played.
        let kind = Kind::Ahead;
        let mut fresh = Vec::new();
        for (user, ids) in songs {
            if !self.may_keep_copies(&user, None) {
                continue;
            }
            for id in ids {
                let (true, Some(provider), Some(external_id)) = self.parts.library.parse_song_id(&id) else {
                    continue;
                };
                if self.wanted(&provider, &external_id).await {
                    fresh.push(Job {
                        provider,
                        external_id,
                        client_id: id,
                        listeners: vec![user.clone()],
                        reason: Reason::Kept,
                        kind,
                    });
                }
            }
        }
        {
            let mut state = self.state.lock();
            state.queued.retain(|job| job.reason != Reason::Kept);
            for job in fresh {
                let key = job.key();
                if !state.running.contains_key(&key) && !state.queued.iter().any(|j| j.key() == key) {
                    state.queued.push_back(job);
                }
            }
        }
        self.wake.notify_one();
    }

    /// Whether an outside song has its FLAC here: fetched ahead, or in the library.
    pub async fn flac_ready(&self, id: &str) -> bool {
        let (true, Some(provider), Some(external_id)) = self.parts.library.parse_song_id(id) else {
            return false;
        };
        self.parts.store.is_ready(&provider, &external_id)
            || self
                .parts
                .library
                .get_local_path_for_external_song(&provider, &external_id)
                .await
                .is_some_and(|path| std::path::Path::new(&path).is_file())
    }

    /// The bytes the songs fetched ahead take.
    pub fn flac_bytes(&self) -> u64 {
        self.parts.store.sizes().1
    }

    /// Whether a song is worth fetching ahead: not ready, not being downloaded, not resting
    /// after a failure, and not already in the library under its outside id.
    async fn wanted(&self, provider: &str, external_id: &str) -> bool {
        let key = format!("{provider}:{external_id}");
        let now = self.now();
        let kept = self.state.lock().resting_for_mornye.get(&key).cloned();
        // A song Mornye's check kept rests until that pause ends, until its checks pass, or until
        // the Mornye that kept it is switched off or moved.
        let passed = match kept {
            Some((url, labels)) => match self.parts.spotiflac.as_ref() {
                Some(client) => client.wait_over(&url, &labels).await,
                None => true,
            },
            None => false,
        };
        {
            let mut state = self.state.lock();
            if let Some(until) = state.resting.get(&key) {
                if *until > now && !passed {
                    return false;
                }
                state.resting.remove(&key);
                state.resting_for_mornye.remove(&key);
            }
        }
        if self.parts.store.is_ready(provider, external_id)
            || self.parts.queue.is_in_flight(provider, external_id)
        {
            return false;
        }
        self.parts
            .library
            .get_local_path_for_external_song(provider, external_id)
            .await
            .is_none_or(|path| !std::path::Path::new(&path).is_file())
    }

    /// The dashboard's view.
    pub async fn status(&self) -> PrefetchStatus {
        let settings = self.settings();
        let (ready, used_bytes) = self.parts.store.sizes();
        let folder = self.folder();
        let paused = self.paused(&settings).await;
        let (running, next, errors) = {
            let state = self.state.lock();
            (
                state.running.values().map(|(job, _)| self.view(job)).collect(),
                state.queued.iter().take(5).map(|job| self.view(job)).collect(),
                state.errors.iter().cloned().collect(),
            )
        };
        PrefetchStatus {
            enabled: settings.enabled,
            ready,
            used_bytes,
            cap_bytes: settings.effective_cache_bytes(),
            free_bytes: free_bytes(&folder),
            folder: folder.to_string_lossy().into_owned(),
            running,
            next,
            errors,
            paused,
        }
    }

    fn view(&self, job: &Job) -> JobView {
        let routing = self
            .parts
            .names
            .as_ref()
            .and_then(|names| names.lookup(&job.external_id))
            .map(|shared| shared.snapshot());
        JobView {
            artist: routing.as_ref().and_then(|r| r.artist.clone()),
            title: routing.as_ref().and_then(|r| r.title.clone()),
        }
    }

    /// Deletes every song downloaded ahead (but one playing now) and drops the queue. Answers how
    /// many songs went.
    pub fn clear(&self) -> usize {
        self.state.lock().queued.clear();
        self.parts.store.clear()
    }

    /// How many downloads ahead may run now: never more than one fewer than the transfers Octo
    /// may run at once, so one is always free for a heart.
    fn room(&self, settings: &PrefetchSettings) -> usize {
        let width = self
            .parts
            .concurrency
            .as_ref()
            .map_or(1, |c| c.current().max(1) as usize);
        settings.effective_at_once().min(width - 1)
    }

    /// Why no new song may start now, in words, or None when one may.
    async fn paused(&self, settings: &PrefetchSettings) -> Option<String> {
        if !settings.enabled {
            return Some("Off".to_string());
        }
        if self.stopped(settings) {
            return Some("Waiting for someone to play something".to_string());
        }
        if self.parts.settings.current().subsonic.download_on_play {
            return None;
        }
        if self.room(settings) == 0 {
            return Some(ONE_AT_A_TIME.to_string());
        }
        if !self.parts.queue.is_idle() {
            return Some("Hearts and plays download first".to_string());
        }
        if !prepare_folder(&self.folder()).unwrap_or(false) {
            return Some("The folder has other files in it; choose an empty one".to_string());
        }
        if free_bytes(&self.folder()).is_some_and(|free| free < settings.effective_min_free_bytes()) {
            return Some("The disk is nearly full".to_string());
        }
        if self.soulseek_down().await {
            return Some(self.step_out_words().to_string());
        }
        if self.mornye_kept().await {
            return Some("Waiting for Mornye's check".to_string());
        }
        None
    }

    /// Mornye is the lossless source left (Soulseek off, unreachable or logged out) and its
    /// check keeps it from every extension a song could use.
    async fn mornye_kept(&self) -> bool {
        let Some(client) = self.parts.spotiflac.as_ref().filter(|client| client.is_on()) else {
            return false;
        };
        if !self.soulseek_unusable().await {
            return false;
        }
        let Some(view) = client.check_view(None).await else {
            return false;
        };
        view.stands_in(&client.settings().effective_services())
            && view
                .pause(&client.settings().effective_services(), chrono::Utc::now())
                .is_some()
    }

    /// Nobody played anything lately, and nothing new starts then (PauseWhenStopped).
    fn stopped(&self, settings: &PrefetchSettings) -> bool {
        settings.pause_when_stopped
            && self
                .state
                .lock()
                .last_played
                .is_none_or(|at| self.now() - at > STOPPED_AFTER)
    }

    /// Soulseek is the only lossless source and cannot be used: it is off, slskd cannot be
    /// reached, or slskd is not logged in, and Mornye cannot stand in for it (off, not
    /// answering, or unable to download). slskd is read first, so Mornye is never asked while
    /// slskd can be used.
    async fn soulseek_down(&self) -> bool {
        if !self.soulseek_unusable().await {
            return false;
        }
        match &self.parts.spotiflac {
            Some(client) => !client.stands_in().await,
            None => !self.parts.settings.current().spotiflac.is_on(),
        }
    }

    /// What prefetch waits for while the Soulseek step is out: Mornye, when it is that step's only
    /// source (slskd is not in use, by the rule Better quality and imports use), else Soulseek.
    fn step_out_words(&self) -> &'static str {
        let mornye_only = self.parts.spotiflac.as_ref().is_some_and(|client| client.is_on())
            && !crate::services::library::UpgradeSources::slskd_in_use_with(
                &self.parts.settings.current().soulseek,
                self.parts.soulseek.as_deref(),
            );
        if mornye_only {
            "Waiting for Mornye"
        } else {
            "Waiting for Soulseek to connect"
        }
    }

    /// Soulseek cannot be used: it is off, slskd cannot be reached, or slskd is not logged in.
    async fn soulseek_unusable(&self) -> bool {
        let Some(link) = &self.parts.soulseek else {
            return false;
        };
        if link.is_off() {
            return true;
        }
        match link.read(false).await {
            None => true,
            Some(reading) => reading.link == SoulseekLinkState::NotLoggedIn,
        }
    }

    /// The worker: starts songs while there is room, sweeps the folder every hour and keeps the
    /// mixes ready.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        while !stopping.is_cancelled() {
            self.step().await;
            tokio::select! {
                _ = stopping.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(TICK) => {}
            }
        }
        for (_, (_, token)) in self.state.lock().running.drain() {
            token.cancel();
        }
        Ok(())
    }

    /// One look: the hourly work when it is due, then as many new songs as there is room for.
    pub async fn step(self: &Arc<Self>) {
        let settings = self.settings();
        let now = self.now();
        let hourly = |last: Option<DateTime<Utc>>| last.is_none_or(|at| now - at >= HOURLY);
        // Songs left from before are swept with Prefetch off too.
        if hourly(self.state.lock().last_sweep) {
            self.state.lock().last_sweep = Some(now);
            self.parts
                .store
                .sweep(settings.effective_keep_days(), settings.effective_cache_bytes());
            if let Some(copies) = self.copies.get().cloned() {
                let swept = Arc::clone(&copies);
                let _ = tokio::task::spawn_blocking(move || swept.sweep()).await;
                // Held copies count toward the cap too: past it on their own, the oldest whose
                // video still plays is let go.
                copies.release_held_over_cap().await;
            }
        }
        if !settings.enabled {
            let mut state = self.state.lock();
            state.queued.clear();
            for (_, token) in state.running.values() {
                token.cancel();
            }
            return;
        }
        self.forget_idle_listeners(now);
        if settings.keep_mixes_ready && hourly(self.state.lock().last_mixes) {
            self.state.lock().last_mixes = Some(now);
            self.keep_mixes_ready(&settings).await;
        }
        self.queue_next_play(&settings);
        while self.start_next(&settings).await {}
    }

    /// A listener who played nothing for half an hour has nothing coming up: their songs leave
    /// the queue, unless another listener has them coming up too.
    fn forget_idle_listeners(&self, now: DateTime<Utc>) {
        let mut state = self.state.lock();
        let idle: Vec<String> = state
            .listeners
            .values()
            .filter(|l| l.last_played.is_none_or(|at| now - at > LISTENER_IDLE_AFTER))
            .map(|l| user_key(&l.name))
            .collect();
        if idle.is_empty() {
            return;
        }
        for job in state.queued.iter_mut() {
            // A song kept ready keeps the listener it is for, whose library it goes to.
            if job.reason == Reason::Upcoming {
                job.listeners.retain(|l| !idle.contains(&user_key(l)));
            }
        }
        state
            .queued
            .retain(|job| job.reason != Reason::Upcoming || !job.listeners.is_empty());
    }

    /// With DownloadOnPlay on: the next song coming up is queued as the play it will be, once no
    /// play is waiting, so it never takes a real play's place. One at a time, as plays are.
    fn queue_next_play(&self, settings: &PrefetchSettings) {
        if self.stopped(settings) || self.parts.queue.waiting_plays() > 0 {
            return;
        }
        let job = {
            let mut state = self.state.lock();
            let Some(at) = state.queued.iter().position(|j| j.kind == Kind::AsPlay) else {
                return;
            };
            state.queued.remove(at)
        };
        let Some(job) = job else { return };
        if self.parts.queue.is_in_flight(&job.provider, &job.external_id) {
            return;
        }
        let owner = job.listeners.first().cloned();
        let record = self.parts.settings.current().subsonic.record_requested_by;
        let requester = owner.as_deref().filter(|_| record);
        self.parts.coordinator.queue_play_ahead(
            &job.provider,
            &job.external_id,
            requester,
            Some(&job.client_id),
            owner.as_deref(),
        );
    }

    /// Starts the next queued song when there is room for it. False when none started.
    async fn start_next(self: &Arc<Self>, settings: &PrefetchSettings) -> bool {
        let has_job = self.state.lock().queued.iter().any(|j| j.kind == Kind::Ahead);
        if !has_job || self.state.lock().running.len() >= self.room(settings) {
            return false;
        }
        if self.paused(settings).await.is_some() {
            return false;
        }
        // The space also holds the preview copies kept for playlists kept ready.
        let kept = self.copies.get().map_or(0, |copies| copies.kept_bytes());
        let cap = settings.effective_cache_bytes().saturating_sub(kept);
        if self.parts.store.over(cap) {
            self.parts.store.sweep(settings.effective_keep_days(), cap);
            if self.parts.store.over(cap) {
                return false;
            }
        }
        let job = {
            let mut state = self.state.lock();
            let Some(at) = state.queued.iter().position(|j| j.kind == Kind::Ahead) else {
                return false;
            };
            let Some(job) = state.queued.remove(at) else {
                return false;
            };
            let token = CancellationToken::new();
            state.running.insert(job.key(), (job.clone(), token.clone()));
            (job, token)
        };
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let (job, token) = job;
            service.fetch(&job, &token).await;
            service.state.lock().running.remove(&job.key());
            service.wake.notify_one();
        });
        true
    }

    /// One song, fetched ahead.
    async fn fetch(&self, job: &Job, token: &CancellationToken) {
        let folder = self.folder();
        let key = job.key();
        let outcome = self
            .parts
            .downloads
            .download_ahead(&job.provider, &job.external_id, &folder, token)
            .await;
        let now = self.now();
        match outcome {
            Ok(Ahead::Kept { path }) => self.parts.store.adopt(&path),
            Ok(Ahead::AlreadyYours { .. }) => {
                self.state.lock().resting.insert(key, now + RETRY_AFTER);
            }
            // Stopped because getting songs ahead was turned off: tried again once it is on.
            Err(_) if token.is_cancelled() => {}
            // A heart or a play is downloading it already, into the library.
            Err(e) if e.downcast_ref::<AheadBusy>().is_some() => {
                self.state.lock().resting.insert(key, now + RETRY_AFTER);
            }
            Err(e) => {
                let words = format!("{e:#}");
                info!("Could not download {key} ahead: {words}");
                // A source that could not be reached says nothing about the song. Mornye's check
                // keeping it says only that Mornye was not asked: the song rests until Mornye may
                // try its check again, so the other sources are not asked again in the meantime.
                let rest = match mornye_waited(&e) {
                    Some(wait) => (wait.pause.until_utc - now).max(RETRY_UNREACHABLE_AFTER),
                    None if unreachable(&e) || self.soulseek_down().await => RETRY_UNREACHABLE_AFTER,
                    None => RETRY_AFTER,
                };
                let kept = mornye_waited(&e).map(|wait| (wait.pause.url.clone(), wait.pause.labels.clone()));
                let named = self.view(job);
                let mut state = self.state.lock();
                state.resting.insert(key.clone(), now + rest);
                // A Pass of those checks, or that Mornye switched off or moved, ends the rest
                // before its time.
                match kept {
                    Some(kept) => {
                        state.resting_for_mornye.insert(key, kept);
                    }
                    None => {
                        state.resting_for_mornye.remove(&key);
                    }
                }
                let line = match (named.artist, named.title) {
                    (Some(artist), Some(title)) => format!("{artist} - {title}: {words}"),
                    _ => words,
                };
                state.errors.push_front(line);
                state.errors.truncate(ERRORS_SHOWN);
            }
        }
    }

    /// Queues the first and a few random outside songs of every mix of the listeners who played
    /// in the last week, after everything that plays soon.
    async fn keep_mixes_ready(&self, settings: &PrefetchSettings) {
        let Some(mixes) = self.mixes.get() else {
            return;
        };
        let now = self.now();
        let users: Vec<String> = self
            .state
            .lock()
            .listeners
            .values()
            .filter(|l| l.last_played.is_some_and(|at| now - at <= MIXES_FOR))
            .map(|l| l.name.clone())
            .collect();
        let count = settings.effective_keep_ready_count();
        let kind = if self.parts.settings.current().subsonic.download_on_play {
            Kind::AsPlay
        } else {
            Kind::Ahead
        };
        let mut wanted: Vec<(String, String, String, String)> = Vec::new();
        for user in users {
            if !self.may_keep_copies(&user, None) {
                continue;
            }
            for mix in mixes.current_lists(&user) {
                let Some(songs) = mixes.drawn(&user, &mix) else {
                    continue;
                };
                let outside: Vec<(String, String, String)> = songs
                    .iter()
                    .filter_map(|song| song.get("id").and_then(|id| id.as_str()))
                    .filter_map(|id| match self.parts.library.parse_song_id(id) {
                        (true, Some(provider), Some(external_id)) => {
                            Some((provider, external_id, id.to_string()))
                        }
                        _ => None,
                    })
                    .collect();
                let mut picked: Vec<(String, String, String)> = outside.iter().take(count).cloned().collect();
                let mut rest: Vec<(String, String, String)> = outside.iter().skip(count).cloned().collect();
                for i in (1..rest.len()).rev() {
                    rest.swap(i, rand::random_range(0..=i));
                }
                picked.extend(rest.into_iter().take(count));
                wanted.extend(
                    picked
                        .into_iter()
                        .map(|(provider, id, client)| (provider, id, client, user.clone())),
                );
            }
        }
        let mut fresh = Vec::new();
        for (provider, external_id, client_id, user) in wanted {
            if self.wanted(&provider, &external_id).await {
                fresh.push(Job {
                    provider,
                    external_id,
                    client_id,
                    listeners: vec![user],
                    reason: Reason::Mix,
                    kind,
                });
            }
        }
        let mut state = self.state.lock();
        state.queued.retain(|job| job.reason != Reason::Mix);
        for job in fresh {
            let key = job.key();
            if !state.running.contains_key(&key) && !state.queued.iter().any(|j| j.key() == key) {
                state.queued.push_back(job);
            }
        }
    }
}

/// Whether a failure is a source that could not be reached at all (refused, timed out).
fn unreachable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_connect() || e.is_timeout())
            || cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::TimedOut
                )
            })
    })
}

fn user_key(user: &str) -> String {
    user.trim().to_lowercase()
}

fn listener_mut<'a>(state: &'a mut State, user: &str) -> &'a mut Listener {
    let key = user_key(user);
    if !state.listeners.contains_key(&key) && state.listeners.len() >= MAX_LISTENERS {
        // The listener who played longest ago is forgotten.
        if let Some(oldest) = state
            .listeners
            .iter()
            .min_by_key(|(_, l)| l.last_played)
            .map(|(k, _)| k.clone())
        {
            state.listeners.remove(&oldest);
        }
    }
    let listener = state.listeners.entry(key).or_default();
    if listener.name.is_empty() {
        listener.name = user.trim().to_string();
    }
    listener
}
