//! Learns, in the background, the true size of the stream an outside song plays, for the rows
//! that went out without one. Searches and plays learn most sizes as they go (the shim names the
//! stream in the same run that picks it); this fills the listed rows nothing else reached, so
//! the next answer that lists them can carry the size.
//!
//! Gentle on YouTube by construction, since every lookup costs one extraction: at most
//! `Subsonic:StreamSizeWarmPerMinute` lookups a minute ([`PER_MINUTE`] by default, 0 turns them
//! off), [`CONCURRENT`] at a time, one queued entry per song, a song tried at most once an
//! hour, and a full queue drops new rows rather than growing. A song with no video pinned yet
//! is searched for only when the answer that listed it had at most [`SEARCH_LISTING_MAX`]
//! outside rows, so a sync catalog or a whole chart never sets off a search per row; a pinned
//! song's facts are still asked for. After [`BREAKER_MISSES`] lookups in a row come back
//! failed or empty, the lookups pause for [`BREAKER_PAUSE`].

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use octo_core::soulseek::{LengthSource, RoutingKind, SongLength, SoulseekRouting, StreamFacts};
use octo_subsonic::subsonic_response_builder::IdRegistry;
use parking_lot::Mutex;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::you_tube_resolver::{YouTubeHit, YouTubeResolver};
use crate::services::soulseek::ExternalIdRegistry;

/// How many songs can wait at once; past this, new rows are dropped.
pub const QUEUE_CAPACITY: usize = 512;
/// Lookups started per minute when the setting does not say otherwise.
pub const PER_MINUTE: usize = 5;
/// Lookups running at once, at most.
pub const CONCURRENT: usize = 2;
/// The most outside rows an answer may list for its unpinned songs to be searched for.
pub const SEARCH_LISTING_MAX: usize = 50;
/// Failed or empty lookups in a row that pause the lookups.
pub const BREAKER_MISSES: u32 = 5;
/// How long the lookups pause once [`BREAKER_MISSES`] came back failed or empty in a row.
pub const BREAKER_PAUSE: Duration = Duration::from_secs(30 * 60);
/// How long a song that was tried is left alone, answered or not.
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// How many tried songs are remembered; the oldest are forgotten first.
const TRIED_MAX: usize = 4096;
/// How long after a play its measured loudness is asked for: the shim measures while the song
/// plays, which takes a few seconds for a whole song.
pub const LOUDNESS_PICKUP_DELAY: Duration = Duration::from_secs(90);

/// Where the warmer asks: the shim, as background work.
#[async_trait]
pub trait StreamFactsSource: Send + Sync {
    /// The stream facts of a video already pinned.
    async fn facts(&self, video_id: &str) -> Option<StreamFacts>;
    /// The video a play would pick for this query and length, with its stream.
    async fn search(&self, query: &str, duration_hint: Option<i32>) -> Option<YouTubeHit>;
}

#[async_trait]
impl StreamFactsSource for YouTubeResolver {
    async fn facts(&self, video_id: &str) -> Option<StreamFacts> {
        YouTubeResolver::facts(self, video_id, true).await
    }

    async fn search(&self, query: &str, duration_hint: Option<i32>) -> Option<YouTubeHit> {
        YouTubeResolver::search(self, query, duration_hint, true).await
    }
}

/// A sliding minute of lookup starts.
#[derive(Debug)]
pub struct RateWindow {
    per_minute: usize,
    starts: VecDeque<Instant>,
}

impl RateWindow {
    pub fn new(per_minute: usize) -> Self {
        RateWindow {
            per_minute: per_minute.max(1),
            starts: VecDeque::new(),
        }
    }

    /// Lookups a minute from now on.
    pub fn set_per_minute(&mut self, per_minute: usize) {
        self.per_minute = per_minute.max(1);
    }

    /// None when a lookup may start at `now` (and counts it); else how long to wait first.
    pub fn wait_before_next(&mut self, now: Instant) -> Option<Duration> {
        const MINUTE: Duration = Duration::from_secs(60);
        while self
            .starts
            .front()
            .is_some_and(|start| now.duration_since(*start) >= MINUTE)
        {
            self.starts.pop_front();
        }
        if self.starts.len() < self.per_minute {
            self.starts.push_back(now);
            return None;
        }
        let oldest = *self.starts.front()?;
        Some(MINUTE.saturating_sub(now.duration_since(oldest)))
    }
}

/// Counts lookups that came back failed or empty in a row, and pauses the lookups for
/// [`BREAKER_PAUSE`] once there are [`BREAKER_MISSES`] of them.
#[derive(Debug, Default)]
pub struct Breaker {
    misses: u32,
    paused_until: Option<Instant>,
}

impl Breaker {
    /// One lookup ended at `now`: `answered` when it brought what it asked for. True when this
    /// miss started a pause.
    pub fn record(&mut self, answered: bool, now: Instant) -> bool {
        if answered {
            self.misses = 0;
            return false;
        }
        self.misses += 1;
        if self.misses < BREAKER_MISSES {
            return false;
        }
        self.misses = 0;
        self.paused_until = Some(now + BREAKER_PAUSE);
        true
    }

    /// How much longer the lookups are paused at `now`, if they are.
    pub fn paused_for(&self, now: Instant) -> Option<Duration> {
        self.paused_until
            .filter(|until| *until > now)
            .map(|until| until - now)
    }
}

/// The outside rows of the answer being built: how many, and which went out without a size.
#[derive(Debug, Default)]
struct Listing {
    rows: usize,
    wanted: Vec<String>,
}

tokio::task_local! {
    static LISTING: RefCell<Listing>;
}

/// Lookups a minute, read at every use so a change applies at once.
pub type PerMinute = Arc<dyn Fn() -> usize + Send + Sync>;

/// The background sizer, and the id registry the response builder reads stream facts through
/// (so every row that goes out without a size asks it to learn one).
pub struct StreamSizeWarmer {
    registry: Arc<ExternalIdRegistry>,
    source: Arc<dyn StreamFactsSource>,
    per_minute: PerMinute,
    sender: mpsc::Sender<String>,
    receiver: Mutex<Option<mpsc::Receiver<String>>>,
    queued: Mutex<HashSet<String>>,
    tried: Mutex<HashMap<String, Instant>>,
    /// Songs played since their loudness was last asked for.
    loudness_wanted: Mutex<HashSet<String>>,
    breaker: Mutex<Breaker>,
}

impl StreamSizeWarmer {
    pub fn new(registry: Arc<ExternalIdRegistry>, source: Arc<dyn StreamFactsSource>) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        StreamSizeWarmer {
            registry,
            source,
            per_minute: Arc::new(|| PER_MINUTE),
            sender,
            receiver: Mutex::new(Some(receiver)),
            queued: Mutex::new(HashSet::new()),
            tried: Mutex::new(HashMap::new()),
            loudness_wanted: Mutex::new(HashSet::new()),
            breaker: Mutex::new(Breaker::default()),
        }
    }

    /// Reads the lookups a minute from `per_minute` (0 turns the lookups off).
    pub fn with_per_minute(mut self, per_minute: PerMinute) -> Self {
        self.per_minute = per_minute;
        self
    }

    /// Lookups a minute now; 0 when they are off.
    pub fn per_minute(&self) -> usize {
        (self.per_minute)()
    }

    /// Queues `song_id` when it is an outside song with no known size that was not tried in
    /// the last hour and is not already waiting. A song with no video pinned is queued only
    /// when `search` allows the search that would pin one. True when it was queued.
    pub fn want(&self, song_id: &str, search: bool) -> bool {
        if song_id.is_empty() || self.per_minute() == 0 || !self.needs_size(song_id, search) {
            return false;
        }
        if self
            .tried
            .lock()
            .get(song_id)
            .is_some_and(|at| at.elapsed() < RETRY_AFTER)
        {
            return false;
        }
        if !self.queued.lock().insert(song_id.to_string()) {
            return false;
        }
        if self.sender.try_send(song_id.to_string()).is_err() {
            // Full: dropped, and free to be listed again later.
            self.queued.lock().remove(song_id);
            return false;
        }
        true
    }

    fn needs_size(&self, song_id: &str, search: bool) -> bool {
        // A song waiting for its next play to take a chosen video has no size to learn yet: its
        // pin is the video it leaves.
        if self.registry.awaits_switch(song_id) {
            return false;
        }
        self.registry.lookup(song_id).is_some_and(|routing| {
            let r = routing.lock();
            r.kind == RoutingKind::Song
                && r.stream_size.is_none()
                && (r.has_you_tube() || (search && r.has_artist_title()))
        })
    }

    /// Runs `answer`, the building of one answer, noting the outside rows it lists; then queues
    /// the ones that went out without a size. Their searches only when the answer listed at
    /// most [`SEARCH_LISTING_MAX`] outside rows.
    pub async fn listing<F: Future>(&self, answer: F) -> F::Output {
        let (output, listing) = LISTING
            .scope(RefCell::new(Listing::default()), async {
                let output = answer.await;
                let listing = LISTING.with(|listing| std::mem::take(&mut *listing.borrow_mut()));
                (output, listing)
            })
            .await;
        let search = listing.rows <= SEARCH_LISTING_MAX;
        for song_id in &listing.wanted {
            self.want(song_id, search);
        }
        output
    }

    /// Learns the size of one song: the pinned video's facts, or, with no video pinned yet, the
    /// same search a play would make, pinned so the size is the size of what plays. None when
    /// nothing was asked; else whether the lookup brought an answer.
    pub async fn warm_one(&self, song_id: &str) -> Option<bool> {
        if self.registry.awaits_switch(song_id) {
            return None;
        }
        let routing = self.registry.lookup(song_id)?;
        let r: SoulseekRouting = routing.snapshot();
        let loudness = self.loudness_wanted.lock().remove(song_id) && r.track_gain.is_none();
        if r.kind != RoutingKind::Song || (r.stream_size.is_some() && !loudness) {
            return None;
        }
        if let Some(video) = r.you_tube_id.clone().filter(|v| !v.is_empty()) {
            let facts = self.source.facts(&video).await;
            if let Some(facts) = &facts {
                self.registry.remember_stream(song_id, &video, facts);
            }
            return Some(facts.is_some());
        }
        if !r.has_artist_title() {
            return None;
        }
        let query = format!(
            "{} {}",
            r.artist.as_deref().unwrap_or(""),
            r.title.as_deref().unwrap_or("")
        );
        let Some(hit) = self
            .source
            .search(&query, r.duration)
            .await
            .filter(|h| !h.video_id.is_empty())
        else {
            return Some(false);
        };
        // A video the song refuses is neither pinned nor measured.
        if r.refuses_video(&hit.video_id) {
            return Some(false);
        }
        // A play that pinned a video meanwhile keeps it.
        if !self.registry.pin_video(song_id, &hit.video_id, false) {
            return Some(true);
        }
        self.registry.remember_pick(song_id, &hit.video_id, hit.pick);
        self.registry.remember_stream(song_id, &hit.video_id, &hit.stream);
        if let (Some(d), Some(routing)) = (hit.duration, self.registry.lookup(song_id)) {
            let replaced = {
                let mut r = routing.lock();
                let replace = SongLength::stream_length_may_replace(r.duration, d);
                if replace {
                    r.duration = Some(d);
                }
                replace
            };
            if replaced {
                self.registry.mark_changed(song_id);
            }
        }
        self.registry
            .remember_length(song_id, hit.duration, LengthSource::Video);
        Some(true)
    }

    /// A play of `song_id` started: the shim measures its loudness while it plays, so ask for
    /// it a little later, unless it is already known.
    pub fn after_play(self: &Arc<Self>, song_id: &str) {
        let known = self
            .registry
            .stream_facts(song_id)
            .is_some_and(|facts| facts.track_gain.is_some());
        if known || song_id.is_empty() {
            return;
        }
        let warmer = self.clone();
        let song_id = song_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(LOUDNESS_PICKUP_DELAY).await;
            warmer.want_loudness(&song_id);
        });
    }

    /// Queues a played song for its measured loudness: one /facts call for its pinned video,
    /// within the same limits as the size lookups. True when it was queued.
    pub fn want_loudness(&self, song_id: &str) -> bool {
        if self.per_minute() == 0 {
            return false;
        }
        let pinned = self.registry.lookup(song_id).is_some_and(|routing| {
            let r = routing.lock();
            r.kind == RoutingKind::Song && r.has_you_tube() && r.track_gain.is_none()
        });
        if !pinned {
            return false;
        }
        self.loudness_wanted.lock().insert(song_id.to_string());
        if !self.queued.lock().insert(song_id.to_string()) {
            // Already waiting: that lookup brings the loudness too.
            return true;
        }
        if self.sender.try_send(song_id.to_string()).is_err() {
            self.queued.lock().remove(song_id);
            self.loudness_wanted.lock().remove(song_id);
            return false;
        }
        true
    }

    /// A lookup of `song_id` ended; `answered` as [`Self::warm_one`] returned it.
    fn done(&self, song_id: &str, answered: Option<bool>) {
        self.queued.lock().remove(song_id);
        // Played while its lookup ran: that lookup read the wish before it, so ask again.
        if self.loudness_wanted.lock().contains(song_id) {
            self.queued.lock().insert(song_id.to_string());
            if self.sender.try_send(song_id.to_string()).is_err() {
                self.queued.lock().remove(song_id);
                self.loudness_wanted.lock().remove(song_id);
            }
        }
        {
            let mut tried = self.tried.lock();
            tried.insert(song_id.to_string(), Instant::now());
            if tried.len() > TRIED_MAX {
                tried.retain(|_, at| at.elapsed() < RETRY_AFTER);
            }
        }
        if let Some(answered) = answered
            && self.breaker.lock().record(answered, Instant::now())
        {
            warn!(
                "Stream size lookups pause for {} minutes: the last {BREAKER_MISSES} came back empty",
                BREAKER_PAUSE.as_secs() / 60
            );
        }
    }

    /// How much longer the lookups are paused at `now`, if they are.
    pub fn paused_for(&self, now: Instant) -> Option<Duration> {
        self.breaker.lock().paused_for(now)
    }

    /// The worker: takes queued songs at most the setting's lookups a minute, [`CONCURRENT`] at
    /// a time, waiting out a pause, until shutdown.
    pub async fn run(self: Arc<Self>, token: CancellationToken) -> anyhow::Result<()> {
        let Some(mut receiver) = self.receiver.lock().take() else {
            return Ok(());
        };
        let mut window = RateWindow::new(PER_MINUTE);
        let running = Arc::new(Semaphore::new(CONCURRENT));
        loop {
            let song_id = tokio::select! {
                () = token.cancelled() => return Ok(()),
                next = receiver.recv() => match next {
                    Some(song_id) => song_id,
                    None => return Ok(()),
                },
            };
            while let Some(wait) = self.paused_for(Instant::now()) {
                tokio::select! {
                    () = token.cancelled() => return Ok(()),
                    () = tokio::time::sleep(wait) => {}
                }
            }
            let per_minute = self.per_minute();
            if per_minute == 0 {
                // Turned off while it waited: dropped, free to be listed again.
                self.queued.lock().remove(&song_id);
                self.loudness_wanted.lock().remove(&song_id);
                continue;
            }
            window.set_per_minute(per_minute);
            while let Some(wait) = window.wait_before_next(Instant::now()) {
                tokio::select! {
                    () = token.cancelled() => return Ok(()),
                    () = tokio::time::sleep(wait) => {}
                }
            }
            let permit = tokio::select! {
                () = token.cancelled() => return Ok(()),
                permit = running.clone().acquire_owned() => permit?,
            };
            let warmer = self.clone();
            tokio::spawn(async move {
                let answered = warmer.warm_one(&song_id).await;
                warmer.done(&song_id, answered);
                debug!("stream size warm done for {song_id}");
                drop(permit);
            });
        }
    }
}

impl IdRegistry for StreamSizeWarmer {
    fn register(&self, routing: SoulseekRouting) -> String {
        self.registry.register(routing)
    }

    /// Only while the song's video is the one it is pinned to: a song waiting for its switch
    /// lists no size of the video it leaves.
    fn stream_facts(&self, song_id: &str) -> Option<StreamFacts> {
        self.registry.song_stream_facts(song_id)
    }

    /// Within an answer, noted for when it is done; outside one (nothing says how long the
    /// list is), only a pinned song is queued.
    fn size_wanted(&self, song_id: &str) {
        let noted = LISTING.try_with(|listing| listing.borrow_mut().wanted.push(song_id.to_string()));
        if noted.is_err() {
            self.want(song_id, false);
        }
    }

    fn outside_listed(&self, _song_id: &str) {
        let _ = LISTING.try_with(|listing| listing.borrow_mut().rows += 1);
    }
}

/// Middleware: each request's answer is one listing (see [`StreamSizeWarmer::listing`]).
pub async fn listing_scope(
    State(warmer): State<Arc<StreamSizeWarmer>>,
    request: Request,
    next: Next,
) -> Response {
    warmer.listing(next.run(request)).await
}

#[cfg(test)]
#[path = "stream_size_warmer_tests.rs"]
mod tests;
