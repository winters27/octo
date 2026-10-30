//! The track acquisition queue: `AcquisitionRequest` and the queue.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use indexmap::IndexMap;
use octo_core::common::dotnet::{self, compare_ordinal_ignore_case, eq_ignore_case};
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeartGate, WaitingFor};
use octo_core::settings::DownloadSource;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::services::library::ReplacementHandoff;

/// How an acquisition ended: the path of the file on disk and fully registered, or why not.
/// The error is shared, so every waiter of one request sees the same error.
pub type AcquisitionOutcome = Result<String, Arc<anyhow::Error>>;

/// Why a play's acquisition resolved without a file it may serve, which a waiting play reads from
/// its completion's error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlayOutcome {
    /// The gate refused the download, held it, or could not prove it is the version that played.
    #[error("Octo could not prove this copy is the version that played")]
    NotProven,
}

/// A download that holds instead of landing: what it waits for, the line its row shows, the
/// files it keeps in the one hold path, and whose gates hold (empty for everyone who asked). A
/// verify step resolves the request's completion with it, and the heart chain files the hold.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{line}")]
pub struct AcquisitionHeld {
    pub waiting_for: WaitingFor,
    pub line: String,
    pub files: Vec<String>,
    pub users: Vec<String>,
}

/// A one-shot completion: completed once, by the worker or by a drop, and awaited by everyone
/// who joined the request. Waiters are woken as tasks of their own, so completing this never
/// runs a waiting request's continuation (response headers, body writes, the client's whole
/// download) inline on the worker.
#[derive(Clone)]
pub struct Completion {
    outcome: Arc<watch::Sender<Option<AcquisitionOutcome>>>,
}

impl Default for Completion {
    fn default() -> Self {
        Completion {
            outcome: Arc::new(watch::Sender::new(None)),
        }
    }
}

impl Completion {
    /// Completes with the file's path; false when it had already completed.
    pub fn try_set_result(&self, path: impl Into<String>) -> bool {
        self.try_set(Ok(path.into()))
    }

    /// Completes with an error; false when it had already completed.
    pub fn try_set_error(&self, error: impl Into<Arc<anyhow::Error>>) -> bool {
        self.try_set(Err(error.into()))
    }

    fn try_set(&self, outcome: AcquisitionOutcome) -> bool {
        let mut outcome = Some(outcome);
        self.outcome.send_if_modified(|slot| {
            if slot.is_some() {
                return false;
            }
            *slot = outcome.take();
            true
        })
    }

    /// `Task.IsCompleted`.
    pub fn is_completed(&self) -> bool {
        self.outcome.borrow().is_some()
    }

    /// Waits for the outcome (`await request.Completion.Task`).
    pub async fn wait(&self) -> AcquisitionOutcome {
        let mut receiver = self.outcome.subscribe();
        match receiver.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone().expect("waited for an outcome"),
            // The sender lives in `self`, so it cannot close while this waits.
            Err(_) => Err(Arc::new(anyhow::anyhow!("the acquisition was abandoned"))),
        }
    }
}

/// Whose library an acquisition is for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AcquisitionTarget {
    /// The shared library, as every download has always gone.
    Shared,
    /// A family member's own library: the file lands in the family pool and is linked into
    /// their library.
    Member(String),
}

impl AcquisitionTarget {
    /// The member this is for, if any.
    pub fn member(&self) -> Option<&str> {
        match self {
            AcquisitionTarget::Shared => None,
            AcquisitionTarget::Member(user) => Some(user),
        }
    }
}

/// Where one acquisition's file goes and what it must be, as the worker hands it to the
/// download service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcquisitionPlace {
    /// The members the file is linked for, when it lands in the family pool; None for the
    /// shared library.
    pub pool_members: Option<Vec<String>>,
    /// Only a real FLAC file will do: lossy files and FLAC made from lossy audio are refused.
    pub flac_only: bool,
    /// Why the acquisition runs; None for a place built without one (the shipped checks).
    pub origin: Option<AcquisitionOrigin>,
    /// One gate per heart or play that asked, each judged on its own.
    pub gates: Vec<HeartGate>,
    /// The files a resumed hold held in the one hold path, which the download checks first.
    pub held_files: Vec<String>,
}

/// One queued request to fetch a permanent copy of a track.
pub struct AcquisitionRequest {
    pub provider: String,
    pub external_id: String,

    /// Carried per request rather than inferred from whoever won a race:
    /// heart routing decides song-versus-album scope, while playback never expands to an album.
    pub trigger_album_download: bool,
    pub force_permanent: bool,
    pub is_star: bool,
    pub source_override: Option<DownloadSource>,
    pub notify_on_failure: bool,

    /// A library action's replacement: staged, given the original's identity and only
    /// then moved in. Not joined: a request already in flight runs without it.
    pub replacement: Option<Arc<ReplacementHandoff>>,

    // Set when a heart joins a request a play started. From then on the heart owns the
    // outcome: a failure is reported the way a star's is, and the play leaves the row alone.
    heart_notifies: AtomicBool,
    heart_joined: AtomicBool,

    /// Search the slow, wide way. Settable after queueing, like RequestedBy, so a Better
    /// quality that joins a request already in flight still widens its search.
    upgrade_search: AtomicBool,

    /// Case-insensitive, first spelling kept (`ConcurrentDictionary` with `OrdinalIgnoreCase`).
    requested_by: Mutex<Vec<String>>,

    /// Someone wants this file in the shared library.
    shared_wanted: AtomicBool,
    /// Members who want this file in their own library, first spelling kept.
    pool_members: Mutex<Vec<String>>,
    /// Only a real FLAC file will do.
    pub flac_only: bool,

    /// Why the acquisition runs: set by [`Self::with_origin`] and read into its place. None keeps
    /// the shipped checks.
    pub origin: Option<AcquisitionOrigin>,
    /// The gates of the hearts and plays that asked, one per person and origin, joiners'
    /// included.
    gates: Mutex<Vec<HeartGate>>,
    /// The files a resumed hold held, which the download checks before any source.
    held_files: Mutex<Vec<String>>,

    pub completion: Completion,
}

impl AcquisitionRequest {
    #[allow(clippy::too_many_arguments)] // one argument per field of the request
    pub fn new(
        provider: impl Into<String>,
        external_id: impl Into<String>,
        is_star: bool,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        notify_on_failure: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> Self {
        AcquisitionRequest {
            provider: provider.into(),
            external_id: external_id.into(),
            trigger_album_download,
            force_permanent,
            is_star,
            source_override,
            notify_on_failure,
            replacement,
            heart_notifies: AtomicBool::new(false),
            heart_joined: AtomicBool::new(false),
            upgrade_search: AtomicBool::new(false),
            requested_by: Mutex::new(Vec::new()),
            shared_wanted: AtomicBool::new(false),
            pool_members: Mutex::new(Vec::new()),
            flac_only: false,
            origin: None,
            gates: Mutex::new(Vec::new()),
            held_files: Mutex::new(Vec::new()),
            completion: Completion::default(),
        }
    }

    /// Adds files a resumed hold held, which the download checks before any source.
    pub fn add_held_files(&self, files: &[String]) {
        let mut held = self.held_files.lock();
        for file in files {
            if !held.contains(file) {
                held.push(file.clone());
            }
        }
    }

    /// The same request, wanting only a real FLAC file.
    pub fn with_flac_only(mut self, flac_only: bool) -> Self {
        self.flac_only = flac_only;
        self
    }

    /// The same request, run for `origin`.
    pub fn with_origin(mut self, origin: AcquisitionOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    /// Adds the gate of one heart or play that asked: one per person and origin, a later one
    /// replacing the earlier. Safe to call after the request is queued, so a joiner is gated on
    /// its own.
    pub fn add_gate(&self, gate: HeartGate) {
        let mut gates = self.gates.lock();
        match gates
            .iter_mut()
            .find(|known| known.origin == gate.origin && eq_ignore_case(known.user.trim(), gate.user.trim()))
        {
            Some(known) => *known = gate,
            None => gates.push(gate),
        }
    }

    /// Every gate of the request, the first asker's first.
    pub fn gates(&self) -> Vec<HeartGate> {
        self.gates.lock().clone()
    }

    /// Records whose library this is for. Safe to call after the request is queued: a member
    /// who joins gets a link when the file lands, and anyone who wants it shared makes it so.
    pub fn add_target(&self, target: &AcquisitionTarget) {
        match target {
            AcquisitionTarget::Shared => self.shared_wanted.store(true, Ordering::SeqCst),
            AcquisitionTarget::Member(user) => {
                let user = user.trim();
                if user.is_empty() {
                    return;
                }
                let mut members = self.pool_members.lock();
                if !members.iter().any(|name| eq_ignore_case(name, user)) {
                    members.push(user.to_string());
                }
            }
        }
    }

    /// Whether someone wants this file in the shared library.
    pub fn wants_shared(&self) -> bool {
        self.shared_wanted.load(Ordering::SeqCst)
    }

    /// Every member who wants this file in their own library.
    pub fn pool_members(&self) -> Vec<String> {
        self.pool_members.lock().clone()
    }

    /// Where the file goes: the family pool for these members when only members asked for it,
    /// else None (the shared library, as always).
    pub fn pool_destination(&self) -> Option<Vec<String>> {
        let members = self.pool_members();
        (!members.is_empty() && !self.wants_shared()).then_some(members)
    }

    /// Where this request's file goes and what it must be, as the worker reads it now.
    pub fn place(&self) -> AcquisitionPlace {
        AcquisitionPlace {
            pool_members: self.pool_destination(),
            flac_only: self.flac_only,
            origin: self.origin,
            gates: self.gates(),
            held_files: self.held_files.lock().clone(),
        }
    }

    /// The first person who asked, which is whose turn this request takes in the star queue.
    pub fn first_requester(&self) -> Option<String> {
        self.requested_by.lock().first().cloned()
    }

    pub fn heart_joined(&self) -> bool {
        self.heart_joined.load(Ordering::SeqCst)
    }

    pub fn notifies_on_failure(&self) -> bool {
        (self.is_star && self.notify_on_failure) || self.heart_notifies.load(Ordering::SeqCst)
    }

    pub(crate) fn join_heart(&self, notify_on_failure: bool) {
        if notify_on_failure {
            self.heart_notifies.store(true, Ordering::SeqCst);
        }
        self.heart_joined.store(true, Ordering::SeqCst);
    }

    pub fn upgrade_search(&self) -> bool {
        self.upgrade_search.load(Ordering::SeqCst)
    }

    pub(crate) fn ask_for_upgrade_search(&self) {
        self.upgrade_search.store(true, Ordering::SeqCst);
    }

    /// Every user who asked for this track, not only the one who asked first.
    ///
    /// A star for a track already in flight joins this request rather than queueing a second
    /// transfer, so whoever wins that race is an accident of timing. Attributing the file to
    /// them alone would drop everyone else who asked for the same thing.
    pub fn requested_by(&self) -> Vec<String> {
        let mut names = self.requested_by.lock().clone();
        names.sort_by(|a, b| compare_ordinal_ignore_case(a, b));
        names
    }

    /// Record one more asker. Safe to call after the request is already queued.
    pub(crate) fn add_requester(&self, username: Option<&str>) {
        let Some(username) = username.filter(|u| !dotnet::is_blank(u)) else {
            return;
        };
        let username = username.trim();
        let mut names = self.requested_by.lock();
        if !names.iter().any(|name| eq_ignore_case(name, username)) {
            names.push(username.to_string());
        }
    }

    /// `provider:id`, whoever it is for: a second member joins the download in flight. A
    /// request for a real FLAC copy is its own job.
    pub fn key(&self) -> String {
        if self.flac_only {
            format!("{}:{}:flac", self.provider, self.external_id)
        } else {
            format!("{}:{}", self.provider, self.external_id)
        }
    }
}

/// Runs when a play is skipped because another is still waiting: a test seam.
#[cfg(test)]
pub(crate) type OnSkippedPlay = Arc<dyn Fn() + Send + Sync>;

/// Whether stars take turns per person now.
pub type FairTurns = Arc<dyn Fn() -> bool + Send + Sync>;

struct Receivers {
    stars: mpsc::Receiver<Arc<AcquisitionRequest>>,
    plays: mpsc::Receiver<Arc<AcquisitionRequest>>,
    /// Stars taken off the channel and not yet handed out, by whose turn they take (the first
    /// requester, ignoring case; nobody is ""), in the order each person's turn comes round.
    /// Each star carries the order it arrived in.
    waiting_stars: IndexMap<String, VecDeque<(u64, Arc<AcquisitionRequest>)>>,
    /// The arrival number the next star lined up gets.
    arrivals: u64,
}

impl Receivers {
    /// Takes every star waiting in the channel into the per-person lines.
    fn take_waiting_stars(&mut self) {
        while let Ok(star) = self.stars.try_recv() {
            self.line_up(star);
        }
    }

    fn line_up(&mut self, star: Arc<AcquisitionRequest>) {
        let turn = star
            .first_requester()
            .map(|name| dotnet::to_lower_invariant(name.trim()))
            .unwrap_or_default();
        let arrival = self.arrivals;
        self.arrivals += 1;
        self.waiting_stars
            .entry(turn)
            .or_default()
            .push_back((arrival, star));
    }

    /// The next star. Taking turns, one person at a time: the person at the front gives up one
    /// request and goes to the back of the line while they still have more. Otherwise the
    /// star that arrived first, whoever asked.
    fn next_star(&mut self, fair: bool) -> Option<Arc<AcquisitionRequest>> {
        if fair {
            let (turn, mut line) = self.waiting_stars.shift_remove_index(0)?;
            let star = line.pop_front();
            if !line.is_empty() {
                self.waiting_stars.insert(turn, line);
            }
            return star.map(|(_, star)| star);
        }
        let first = self
            .waiting_stars
            .values()
            .enumerate()
            .filter_map(|(at, line)| line.front().map(|(arrival, _)| (*arrival, at)))
            .min()?
            .1;
        let line = self.waiting_stars.get_index_mut(first)?.1;
        let star = line.pop_front();
        if line.is_empty() {
            self.waiting_stars.shift_remove_index(first);
        }
        star.map(|(_, star)| star)
    }
}

/// Serializes permanent-copy downloads onto a single background worker.
///
/// The point is that a download must never be tied to the HTTP request that asked for it.
/// Playing a track used to run the whole Soulseek transfer inside the stream request, so a
/// client giving up canceled the transfer mid-flight while slskd finished the file anyway,
/// and Octo kept no record of it (issue #9).
///
/// Stars get their own channel and are never dropped: a star is explicit user intent,
/// whereas a play is a hint that can be shed under load.
pub struct TrackAcquisitionQueue {
    plays: mpsc::Sender<Arc<AcquisitionRequest>>,
    stars: mpsc::Sender<Arc<AcquisitionRequest>>,
    /// The single reader's end of both channels.
    receivers: tokio::sync::Mutex<Receivers>,

    // Producer-side, so a duplicate never consumes a queue slot at all. Consumer-side dedup
    // would be too late: Feishin re-requests /rest/stream on seek, so duplicates are routine.
    in_flight: Mutex<HashMap<String, Arc<AcquisitionRequest>>>,

    // Requests in the plays channel that the worker has not taken yet.
    waiting_plays: AtomicI32,

    // Stars queued and not yet handed out, in the channel or lined up per person: the bound
    // counts both, so lining stars up never lets more than STAR_CAPACITY wait.
    waiting_stars: AtomicUsize,

    /// Whether stars take turns per person (while Family is on); otherwise first in, first out.
    fair_turns: FairTurns,

    #[cfg(test)]
    on_skipped_play: Mutex<Option<OnSkippedPlay>>,
}

impl Default for TrackAcquisitionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl TrackAcquisitionQueue {
    // A play storm (skipping through a queue) must not be able to enqueue unbounded work,
    // but a star must always land, so the two channels are bounded differently.
    const PLAY_CAPACITY: usize = 32;
    const STAR_CAPACITY: usize = 512;

    pub fn new() -> Self {
        let (plays, plays_rx) = mpsc::channel(Self::PLAY_CAPACITY);
        let (stars, stars_rx) = mpsc::channel(Self::STAR_CAPACITY);
        TrackAcquisitionQueue {
            plays,
            stars,
            receivers: tokio::sync::Mutex::new(Receivers {
                stars: stars_rx,
                plays: plays_rx,
                waiting_stars: IndexMap::new(),
                arrivals: 0,
            }),
            in_flight: Mutex::new(HashMap::new()),
            waiting_plays: AtomicI32::new(0),
            waiting_stars: AtomicUsize::new(0),
            fair_turns: Arc::new(|| false),
            #[cfg(test)]
            on_skipped_play: Mutex::new(None),
        }
    }

    /// Stars take turns per person while `fair` says so (Family on); otherwise they keep the
    /// order they arrived in, as they always have.
    pub fn with_fair_turns(mut self, fair: FairTurns) -> Self {
        self.fair_turns = fair;
        self
    }

    /// Plays in the plays lane that the worker has not taken yet.
    pub fn waiting_plays(&self) -> i32 {
        self.waiting_plays.load(Ordering::SeqCst)
    }

    /// Nothing queued or running. The weekly upgrade waits for this so it never queues ahead of a person.
    pub fn is_idle(&self) -> bool {
        self.in_flight.lock().is_empty()
    }

    /// Whether this song is queued or downloading right now, for anyone.
    pub fn is_in_flight(&self, provider: &str, external_id: &str) -> bool {
        self.in_flight
            .lock()
            .contains_key(&format!("{provider}:{external_id}"))
    }

    #[cfg(test)]
    pub(crate) fn set_on_skipped_play(&self, hook: OnSkippedPlay) {
        *self.on_skipped_play.lock() = Some(hook);
    }

    /// Queue an acquisition, or join the one already running for this track. The returned
    /// completion settles when the file is on disk and fully registered; callers that do not
    /// care may drop it.
    #[allow(clippy::too_many_arguments)] // one argument per field of the request
    pub fn enqueue(
        &self,
        provider: &str,
        external_id: &str,
        is_star: bool,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        notify_on_failure: bool,
        requested_by: Option<&str>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> Completion {
        self.enqueue_for(
            &AcquisitionTarget::Shared,
            false,
            provider,
            external_id,
            is_star,
            trigger_album_download,
            force_permanent,
            source_override,
            notify_on_failure,
            requested_by,
            upgrade_search,
            replacement,
        )
    }

    /// [`Self::enqueue`] for one library: the shared one, or a member's own (the file lands in
    /// the family pool and is linked for them). A member asking for a song already in flight
    /// joins it. `flac_only` asks for a real FLAC copy and nothing else.
    #[allow(clippy::too_many_arguments)] // enqueue's arguments and where the file goes
    pub fn enqueue_for(
        &self,
        target: &AcquisitionTarget,
        flac_only: bool,
        provider: &str,
        external_id: &str,
        is_star: bool,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        notify_on_failure: bool,
        requested_by: Option<&str>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> Completion {
        self.enqueue_as(
            None,
            Vec::new(),
            target,
            flac_only,
            provider,
            external_id,
            is_star,
            trigger_album_download,
            force_permanent,
            source_override,
            notify_on_failure,
            requested_by,
            upgrade_search,
            replacement,
        )
    }

    /// [`Self::enqueue_for`] run for `origin`, with the gates of the hearts or plays that asked.
    /// Joining a request already in flight adds these gates to it, so each joiner is judged on
    /// its own.
    #[allow(clippy::too_many_arguments)] // enqueue_for's arguments, why it runs and its gates
    pub fn enqueue_as(
        &self,
        origin: Option<AcquisitionOrigin>,
        gates: Vec<HeartGate>,
        target: &AcquisitionTarget,
        flac_only: bool,
        provider: &str,
        external_id: &str,
        is_star: bool,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        notify_on_failure: bool,
        requested_by: Option<&str>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> Completion {
        self.enqueue_holding(
            origin,
            gates,
            Vec::new(),
            target,
            flac_only,
            provider,
            external_id,
            is_star,
            trigger_album_download,
            force_permanent,
            source_override,
            notify_on_failure,
            requested_by,
            upgrade_search,
            replacement,
        )
    }

    /// [`Self::enqueue_as`] for a resumed hold: `held_files` are the files it held, which the
    /// download checks before any source.
    #[allow(clippy::too_many_arguments)] // enqueue_as's arguments and the held files
    pub fn enqueue_holding(
        &self,
        origin: Option<AcquisitionOrigin>,
        gates: Vec<HeartGate>,
        held_files: Vec<String>,
        target: &AcquisitionTarget,
        flac_only: bool,
        provider: &str,
        external_id: &str,
        is_star: bool,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        notify_on_failure: bool,
        requested_by: Option<&str>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> Completion {
        let mut request = AcquisitionRequest::new(
            provider,
            external_id,
            is_star,
            trigger_album_download,
            force_permanent,
            source_override,
            notify_on_failure,
            replacement,
        )
        .with_flac_only(flac_only);
        if let Some(origin) = origin {
            request = request.with_origin(origin);
        }
        let request = Arc::new(request);
        request.add_requester(requested_by);
        request.add_target(target);
        for gate in &gates {
            request.add_gate(gate.clone());
        }
        request.add_held_files(&held_files);
        if upgrade_search {
            request.ask_for_upgrade_search();
        }

        let existing = {
            let mut in_flight = self.in_flight.lock();
            match in_flight.get(&request.key()) {
                Some(existing) => Some(existing.clone()),
                None => {
                    in_flight.insert(request.key(), request.clone());
                    None
                }
            }
        };
        if let Some(existing) = existing {
            // Already queued or running. Join it rather than fetching the same file twice,
            // and record this caller on the request that is actually going to run, so the
            // file is attributed to everyone who asked and not just to whoever was first.
            existing.add_requester(requested_by);
            existing.add_target(target);
            for gate in gates {
                existing.add_gate(gate);
            }
            existing.add_held_files(&held_files);
            if upgrade_search {
                existing.ask_for_upgrade_search();
            }
            // A heart for a track a play already asked for. The source cannot change any more, but
            // the failure is now someone's explicit ask, and the heart chain still owns its fallback.
            if is_star && !existing.is_star {
                existing.join_heart(notify_on_failure);
            }
            return existing.completion.clone();
        }

        if is_star {
            self.waiting_stars.fetch_add(1, Ordering::SeqCst);
        } else {
            self.waiting_plays.fetch_add(1, Ordering::SeqCst);
        }
        let channel = if is_star { &self.stars } else { &self.plays };
        let room = !is_star || self.waiting_stars.load(Ordering::SeqCst) <= Self::STAR_CAPACITY;
        if !room || channel.try_send(request.clone()).is_err() {
            if is_star {
                self.waiting_stars.fetch_sub(1, Ordering::SeqCst);
            } else {
                self.waiting_plays.fetch_sub(1, Ordering::SeqCst);
            }
            // Full. Say so out loud: silently shedding work is how "why didn't that
            // download?" becomes unanswerable.
            warn!(
                "Acquisition queue full ({}); dropped {provider}:{external_id}",
                if is_star { "star" } else { "play" }
            );
            self.release(&request);
            request.completion.try_set_error(queue_full());
            return request.completion.clone();
        }

        info!(
            "Queued {} acquisition for {provider}:{external_id}",
            if is_star { "star" } else { "play" }
        );
        request.completion.clone()
    }

    /// Queue a copy of a played track unless another played track is still waiting. Radio
    /// asks for every track it plays, and a backlog of those is work nobody wants by the
    /// time it runs. A dropped play leaves no claim behind, so the next play asks again.
    /// Returns `None` when dropped, otherwise the request carrying this play (new, or one
    /// already queued or running). `on_queued` runs only for a new request, before the worker
    /// can see it.
    #[cfg(test)]
    pub(crate) fn try_enqueue_play(
        &self,
        provider: &str,
        external_id: &str,
        source_override: DownloadSource,
        requested_by: Option<&str>,
        on_queued: Option<&dyn Fn()>,
    ) -> Option<Arc<AcquisitionRequest>> {
        self.try_enqueue_play_for(
            &AcquisitionTarget::Shared,
            provider,
            external_id,
            source_override,
            requested_by,
            on_queued,
            Vec::new(),
        )
    }

    /// [`Self::try_enqueue_play`] for one library: the shared one, or a member's own, with the
    /// play's gate (added before the worker can see a new request, and to one it joins).
    #[allow(clippy::too_many_arguments)] // the play's facts, whose library it is for and its gate
    pub(crate) fn try_enqueue_play_for(
        &self,
        target: &AcquisitionTarget,
        provider: &str,
        external_id: &str,
        source_override: DownloadSource,
        requested_by: Option<&str>,
        on_queued: Option<&dyn Fn()>,
        gates: Vec<HeartGate>,
    ) -> Option<Arc<AcquisitionRequest>> {
        let request = Arc::new(
            AcquisitionRequest::new(
                provider,
                external_id,
                false,
                false,
                true,
                Some(source_override),
                false,
                None,
            )
            .with_origin(AcquisitionOrigin::Play),
        );
        request.add_requester(requested_by);
        request.add_target(target);
        for gate in &gates {
            request.add_gate(gate.clone());
        }
        // Already wanted: ride along, whatever is waiting.
        let known = self.in_flight.lock().get(&request.key()).cloned();
        if let Some(known) = known {
            known.add_requester(requested_by);
            known.add_target(target);
            for gate in gates {
                known.add_gate(gate);
            }
            return Some(known);
        }
        // The waiting slot is taken BEFORE the key is claimed. Claiming first and letting go on a
        // full slot would leave a heart that joined in between waiting on a request nobody runs:
        // Release does not complete it.
        if self
            .waiting_plays
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            debug!(
                "Skipped play acquisition for {provider}:{external_id}: another played track is still waiting"
            );
            #[cfg(test)]
            {
                let hook = self.on_skipped_play.lock().clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
            return None;
        }
        let existing = {
            let mut in_flight = self.in_flight.lock();
            match in_flight.get(&request.key()) {
                Some(existing) => Some(existing.clone()),
                None => {
                    in_flight.insert(request.key(), request.clone());
                    None
                }
            }
        };
        if let Some(existing) = existing {
            self.waiting_plays.fetch_sub(1, Ordering::SeqCst);
            existing.add_requester(requested_by);
            existing.add_target(target);
            for gate in gates {
                existing.add_gate(gate);
            }
            return Some(existing);
        }
        if let Some(on_queued) = on_queued {
            on_queued();
        }
        if self.plays.try_send(request.clone()).is_err() {
            self.waiting_plays.fetch_sub(1, Ordering::SeqCst);
            warn!("Acquisition queue full (play); dropped {provider}:{external_id}");
            self.release(&request);
            request.completion.try_set_error(queue_full());
            return Some(request);
        }
        info!("Queued play acquisition for {provider}:{external_id}");
        Some(request)
    }

    /// Take the next request, preferring stars. Stars are handed out one person at a time, so
    /// one person asking for a hundred songs never holds back someone else's one; one
    /// person's own stars keep their order. Plays stay first in, first out. `None` once
    /// `cancellation_token` is canceled.
    pub(crate) async fn dequeue(
        &self,
        cancellation_token: &CancellationToken,
    ) -> Option<Arc<AcquisitionRequest>> {
        let mut receivers = self.receivers.lock().await;
        while !cancellation_token.is_cancelled() {
            receivers.take_waiting_stars();
            if let Some(star) = receivers.next_star((self.fair_turns)()) {
                self.waiting_stars.fetch_sub(1, Ordering::SeqCst);
                return Some(star);
            }
            if let Ok(play) = receivers.plays.try_recv() {
                self.waiting_plays.fetch_sub(1, Ordering::SeqCst);
                return Some(play);
            }
            let Receivers { stars, plays, .. } = &mut *receivers;
            let star = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return None,
                star = stars.recv() => star,
                play = plays.recv() => {
                    if let Some(play) = play {
                        self.waiting_plays.fetch_sub(1, Ordering::SeqCst);
                        return Some(play);
                    }
                    None
                }
            };
            // Lined up with any others that came at the same time, and handed out on the
            // next turn of the loop.
            if let Some(star) = star {
                receivers.line_up(star);
            }
        }
        None
    }

    /// Give up the dedup claim. MUST run on every terminal outcome, including a drop and a
    /// failure, not just on success: a leaked claim makes every future request for that
    /// track join a job that will never run.
    pub(crate) fn release(&self, request: &Arc<AcquisitionRequest>) {
        let mut in_flight = self.in_flight.lock();
        if in_flight
            .get(&request.key())
            .is_some_and(|claimed| Arc::ptr_eq(claimed, request))
        {
            in_flight.remove(&request.key());
        }
    }
}

fn queue_full() -> anyhow::Error {
    anyhow::anyhow!("Acquisition queue is full; try again shortly.")
}

#[cfg(test)]
#[path = "track_acquisition_queue_tests.rs"]
mod tests;
