//! The track acquisition queue: `AcquisitionRequest` and the queue.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use indexmap::IndexMap;
use octo_core::common::dotnet::{self, compare_ordinal_ignore_case, eq_ignore_case};
use octo_core::fingerprint::acoust_id_client::AcoustIdAnswer;
use octo_core::fingerprint::heart_gate::{
    Acceptance, AcquisitionOrigin, HeardRecording, HeartGate, WaitingFor,
};
use octo_core::fingerprint::verification::{SongName, is_same_song, read_answer};
use octo_core::settings::DownloadSource;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::services::common::soulseek_hold_store::HeldLidarrFile;
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
/// files it keeps in the one hold path, those of them Lidarr brought with where their labels are
/// read from, and whose gates hold (empty for everyone who asked). A verify step resolves the
/// request's completion with it, and the heart chain files the hold.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{line}")]
pub struct AcquisitionHeld {
    pub waiting_for: WaitingFor,
    pub line: String,
    pub files: Vec<String>,
    pub lidarr: Vec<HeldLidarrFile>,
    pub users: Vec<String>,
}

/// What one heart's or play's gate said of the file its acquisition kept or held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// The file is what this person heard.
    Accepted,
    /// The gate holds for this.
    Held(WaitingFor),
    /// The file is not provably what they heard: refused, or undecided, and why.
    NotAccepted(String),
    /// AcoustID could not tell which version the file is, and it is not the master they heard: it
    /// was not kept, and the heard audio is what this person keeps if nothing better is proven.
    Undecided,
}

/// The gates' verdicts on the file one acquisition ended with, by person and origin, and for each
/// verdict that accepted it, which acceptance did. Shared by everyone who waits on the
/// acquisition, so each one reads their own gate's verdict.
#[derive(Clone, Default)]
pub struct GateLedger {
    verdicts: Arc<Mutex<Vec<(String, AcquisitionOrigin, GateOutcome)>>>,
    accepted_by: Arc<Mutex<Vec<(String, AcquisitionOrigin, Acceptance)>>>,
}

impl GateLedger {
    /// Records one gate's verdict, a later one for the same person and origin replacing it. A
    /// verdict that does not accept the file forgets how an earlier one accepted it.
    pub fn record(&self, user: &str, origin: AcquisitionOrigin, outcome: GateOutcome) {
        let user = user.trim();
        if outcome != GateOutcome::Accepted {
            self.accepted_by
                .lock()
                .retain(|(who, from, _)| !(*from == origin && eq_ignore_case(who, user)));
        }
        let mut entries = self.verdicts.lock();
        match entries
            .iter_mut()
            .find(|(who, from, _)| *from == origin && eq_ignore_case(who, user))
        {
            Some(entry) => entry.2 = outcome,
            None => entries.push((user.to_string(), origin, outcome)),
        }
    }

    /// Records that `user`'s gate of `origin` accepted the file, and by which acceptance: the
    /// very audio heard, AcoustID's clear reading, or the master heard.
    pub fn record_accepted(&self, user: &str, origin: AcquisitionOrigin, by: Acceptance) {
        self.record(user, origin, GateOutcome::Accepted);
        let user = user.trim();
        let mut accepted = self.accepted_by.lock();
        match accepted
            .iter_mut()
            .find(|(who, from, _)| *from == origin && eq_ignore_case(who, user))
        {
            Some(entry) => entry.2 = by,
            None => accepted.push((user.to_string(), origin, by)),
        }
    }

    /// What `user`'s gate of `origin` said, when it judged the file.
    pub fn outcome_for(&self, user: &str, origin: AcquisitionOrigin) -> Option<GateOutcome> {
        self.verdicts
            .lock()
            .iter()
            .find(|(who, from, _)| *from == origin && eq_ignore_case(who, user.trim()))
            .map(|(_, _, outcome)| outcome.clone())
    }

    /// Which acceptance passed the file for `user`'s gate of `origin`, when it was recorded.
    pub fn acceptance_for(&self, user: &str, origin: AcquisitionOrigin) -> Option<Acceptance> {
        self.accepted_by
            .lock()
            .iter()
            .find(|(who, from, _)| *from == origin && eq_ignore_case(who, user.trim()))
            .map(|(_, _, by)| *by)
    }

    /// Whether a gate accepted the file for `user`.
    pub fn accepted(&self, user: &str) -> bool {
        self.verdicts
            .lock()
            .iter()
            .any(|(who, _, outcome)| *outcome == GateOutcome::Accepted && eq_ignore_case(who, user.trim()))
    }

    /// Whether any gate judged the file: none did when the acquisition ran the shipped checks.
    pub fn is_empty(&self) -> bool {
        self.verdicts.lock().is_empty()
    }

    /// Forgets every verdict, for a file that is not kept after all.
    pub fn clear(&self) {
        self.verdicts.lock().clear();
        self.accepted_by.lock().clear();
    }
}

/// A ledger is where verdicts are written, never part of what a place asks for, so any two
/// compare equal.
impl PartialEq for GateLedger {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for GateLedger {}

impl std::fmt::Debug for GateLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.verdicts.lock().iter())
            .entries(self.accepted_by.lock().iter())
            .finish()
    }
}

/// The gates of a request as they stand now, shared with the download that runs it: a heart or a
/// play that joins after the download started is judged by its own gate all the same. A handle is
/// where gates are read from, never part of what a place asks for, so any two compare equal.
#[derive(Clone, Default)]
pub struct SharedGates(Option<Arc<Mutex<Vec<HeartGate>>>>);

impl SharedGates {
    /// Every gate of the request now, or None for a place built without a request.
    pub fn current(&self) -> Option<Vec<HeartGate>> {
        self.0.as_ref().map(|gates| gates.lock().clone())
    }
}

impl PartialEq for SharedGates {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for SharedGates {}

impl std::fmt::Debug for SharedGates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SharedGates").field(&self.current()).finish()
    }
}

/// What a Wrong song or Wrong version replacement refuses by sound, from its original's own
/// AcoustID answer, read once before the download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundDeny {
    /// The original reads clearly as one recording: a file whose dominant recording is it, or one
    /// of its same-note duplicates, is the original again.
    Recordings(HeardRecording),
    /// A Wrong song whose original reads ambiguous: a file whose dominant recording is that song
    /// at all is the wrong song again.
    Song(SongName),
}

/// Why a replacement is refused for being its original's recording again, as the rest of a
/// sentence about the file.
pub const SAME_RECORDING_AGAIN: &str = "is the same recording as the copy it was to replace";
/// Why a replacement is refused for being its original's song again.
pub const SAME_SONG_AGAIN: &str = "is the same song as the copy it was to replace";

impl SoundDeny {
    /// Why a file whose AcoustID answer is `answer` is refused, or None: only by its dominant
    /// recording, among the results that reach `min_score`.
    pub fn refuses(&self, answer: &AcoustIdAnswer, min_score: f64) -> Option<&'static str> {
        let reading = read_answer(answer, &HeardRecording::default(), min_score);
        let dominant = reading.dominant()?;
        match self {
            SoundDeny::Recordings(r) => (r.holds(&dominant.id)
                || r.duplicates
                    .iter()
                    .any(|id| id.eq_ignore_ascii_case(&dominant.id)))
            .then_some(SAME_RECORDING_AGAIN),
            SoundDeny::Song(song) => is_same_song(&dominant.song(), song).then_some(SAME_SONG_AGAIN),
        }
    }
}

/// The key an acquisition of a track runs under: `provider:id`, and `:flac` for a request that
/// wants only a real FLAC copy.
pub fn acquisition_key(provider: &str, external_id: &str, flac_only: bool) -> String {
    if flac_only {
        format!("{provider}:{external_id}:flac")
    } else {
        format!("{provider}:{external_id}")
    }
}

/// A one-shot completion: completed once, by the worker or by a drop, and awaited by everyone
/// who joined the request. Waiters are woken as tasks of their own, so completing this never
/// runs a waiting request's continuation (response headers, body writes, the client's whole
/// download) inline on the worker.
#[derive(Clone)]
pub struct Completion {
    outcome: Arc<watch::Sender<Option<AcquisitionOutcome>>>,
    ledger: GateLedger,
}

impl Default for Completion {
    fn default() -> Self {
        Completion {
            outcome: Arc::new(watch::Sender::new(None)),
            ledger: GateLedger::default(),
        }
    }
}

impl Completion {
    /// What each gate said of the file the acquisition ended with.
    pub fn ledger(&self) -> &GateLedger {
        &self.ledger
    }

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
    /// The request's gates as they stand when a file is judged, joiners' included.
    pub live_gates: SharedGates,
    /// The files a resumed hold held in the one hold path, which the download checks first.
    pub held_files: Vec<String>,
    /// Those of `held_files` Lidarr brought, each with where its labels are read from.
    pub held_lidarr: Vec<HeldLidarrFile>,
    /// Where each gate's verdict on the file is written, for everyone who waits on it.
    pub ledger: GateLedger,
    /// What a replacement refuses by its original's sound.
    pub deny: Option<SoundDeny>,
    /// The person keeps it anyway: the download lands a file its hold kept, as it is, or the very
    /// audio they heard, marked "kept by you, version not proven", and searches for nothing.
    pub keep_anyway: bool,
    /// Whose keep it is: a file a keep lands counts as kept only for them.
    pub keep_for: Vec<String>,
    /// Files of the song that other people's requests landed while this one waited behind them,
    /// which its own gates judge before anything is fetched.
    pub landed: Vec<String>,
    /// Other people of the hold judge the same held files on requests of their own: a held file
    /// this download's gates refuse stays for them.
    pub held_for_others: bool,
}

/// What a resumed hold brings to its download: the files it held, which the download checks
/// before any source, the labels of those Lidarr brought, and whether other people of the hold
/// judge the same files on requests of their own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeldFiles {
    pub files: Vec<String>,
    pub lidarr: Vec<HeldLidarrFile>,
    pub shared: bool,
}

impl HeldFiles {
    /// These held files, the labels of those Lidarr brought, judged by one person's request alone.
    pub fn of(files: Vec<String>, lidarr: Vec<HeldLidarrFile>) -> Self {
        HeldFiles {
            files,
            lidarr,
            shared: false,
        }
    }
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
    /// included, shared with the download that runs the request.
    gates: Arc<Mutex<Vec<HeartGate>>>,
    /// The files a resumed hold held, which the download checks before any source.
    held_files: Mutex<Vec<String>>,
    /// Those of the held files Lidarr brought, with where their labels are read from.
    held_lidarr: Mutex<Vec<HeldLidarrFile>>,
    /// Hearts and plays that joined with a gate of their own: their library, their name and their
    /// claim on the outcome are recorded only once their own gate passes the landed file.
    joiners: Mutex<Vec<GatedJoiner>>,
    /// What a replacement refuses by its original's sound.
    pub deny: Option<SoundDeny>,
    /// A person's keep. Only another keep of the same person shares its download.
    keep_anyway: bool,
    /// Whose keep it is: the people of its gates, a file it lands counted as kept for them alone.
    keep_for: Mutex<Vec<String>>,
    /// Files of the song the requests it waited behind landed, which its own gates judge first.
    landed: Mutex<Vec<String>>,
    /// Other people of its hold judge the same held files on requests of their own.
    held_for_others: AtomicBool,

    pub completion: Completion,
}

/// A heart or play that joined a request in flight with its own gate, waiting for that gate's
/// verdict on the landed file.
#[derive(Debug, Clone)]
struct GatedJoiner {
    users: Vec<String>,
    requested_by: Option<String>,
    target: AcquisitionTarget,
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
            gates: Arc::new(Mutex::new(Vec::new())),
            held_files: Mutex::new(Vec::new()),
            held_lidarr: Mutex::new(Vec::new()),
            joiners: Mutex::new(Vec::new()),
            deny: None,
            keep_anyway: false,
            keep_for: Mutex::new(Vec::new()),
            landed: Mutex::new(Vec::new()),
            held_for_others: AtomicBool::new(false),
            completion: Completion::default(),
        }
    }

    /// The people this request's sound checks are for, in lower case: nobody for a request with no
    /// gate (the shipped checks, which every asker shares), else the person of each of its gates,
    /// and the asker of a play whose gate is built inside its own acquisition.
    fn checked_for(&self) -> Vec<String> {
        let mut people: Vec<String> = self
            .gates
            .lock()
            .iter()
            .map(|gate| dotnet::to_lower_invariant(gate.user.trim()))
            .collect();
        if people.is_empty() && self.origin == Some(AcquisitionOrigin::Play) {
            people.push(
                self.first_requester()
                    .map(|user| dotnet::to_lower_invariant(user.trim()))
                    .unwrap_or_default(),
            );
        }
        people.sort();
        people.dedup();
        people
    }

    /// Whether `other` may share this request's download: a download that carries a sound check
    /// belongs to one person, so either neither carries one or both carry them for that same one
    /// person; and a person's keep shares only with another keep.
    fn shares_with(&self, other: &AcquisitionRequest) -> bool {
        if self.keep_anyway != other.keep_anyway {
            return false;
        }
        let mine = self.checked_for();
        mine.len() <= 1 && mine == other.checked_for()
    }

    /// Adds the people of a keep's gates to whose keep it is.
    fn add_keepers(&self, gates: &[HeartGate]) {
        let mut keepers = self.keep_for.lock();
        for gate in gates {
            let user = gate.user.trim();
            if !keepers.iter().any(|known| eq_ignore_case(known, user)) {
                keepers.push(user.to_string());
            }
        }
    }

    /// Files of the song the requests this one waited behind landed: its own gates judge them
    /// before anything is fetched.
    pub fn landed(&self) -> Vec<String> {
        self.landed.lock().clone()
    }

    /// A heart or play joining with its own gates: its library and its name wait for its own
    /// gate's verdict on the landed file. A heart joining a play claims the outcome at once: from
    /// now on the play leaves the row alone, whether the download lands or fails, and the heart's
    /// own chain goes on from it.
    fn join_gated(
        &self,
        gates: &[HeartGate],
        requested_by: Option<&str>,
        target: &AcquisitionTarget,
        heart: Option<bool>,
    ) {
        for gate in gates {
            self.add_gate(gate.clone());
        }
        if let Some(notify) = heart {
            self.join_heart(notify);
        }
        self.joiners.lock().push(GatedJoiner {
            users: gates.iter().map(|gate| gate.user.trim().to_string()).collect(),
            requested_by: requested_by.map(str::to_string),
            target: target.clone(),
        });
    }

    /// The file landed: every gated joiner whose own gate passed it is recorded now, with its
    /// library and its name; one with no such verdict (its gate did not pass the file, or never
    /// judged it) gets neither. A joiner that brought no gate is recorded as before.
    pub(crate) fn settle_joiners(&self) {
        let joiners = std::mem::take(&mut *self.joiners.lock());
        let ledger = self.completion.ledger();
        for joiner in joiners {
            if !joiner.users.is_empty() && !joiner.users.iter().any(|user| ledger.accepted(user)) {
                debug!(
                    "{} joined {} and its own gate did not pass the file; no link and no attribution",
                    joiner.users.join(", "),
                    self.key()
                );
                continue;
            }
            self.add_requester(joiner.requested_by.as_deref());
            self.add_target(&joiner.target);
        }
    }

    /// Adds files a resumed hold held, which the download checks before any source, with the
    /// labels of those Lidarr brought.
    pub fn add_held_files(&self, files: &[String], lidarr: &[HeldLidarrFile]) {
        let mut held = self.held_files.lock();
        for file in files {
            if !held.contains(file) {
                held.push(file.clone());
            }
        }
        let mut labeled = self.held_lidarr.lock();
        for file in lidarr {
            if files.contains(&file.path) && !labeled.iter().any(|known| known.path == file.path) {
                labeled.push(file.clone());
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
            live_gates: SharedGates(Some(Arc::clone(&self.gates))),
            held_files: self.held_files.lock().clone(),
            held_lidarr: self.held_lidarr.lock().clone(),
            ledger: self.completion.ledger().clone(),
            deny: self.deny.clone(),
            keep_anyway: self.keep_anyway,
            keep_for: self.keep_for.lock().clone(),
            landed: self.landed(),
            held_for_others: self.held_for_others.load(Ordering::SeqCst),
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
        acquisition_key(&self.provider, &self.external_id, self.flac_only)
    }
}

/// Runs when a play is skipped because another is still waiting: a test seam.
#[cfg(test)]
pub(crate) type OnSkippedPlay = Arc<dyn Fn() + Send + Sync>;

/// Whether stars take turns per person now.
pub type FairTurns = Arc<dyn Fn() -> bool + Send + Sync>;

/// Holds a wake until AcoustID's lane gives a turn again: the AcoustID client's `hold_lookup`.
pub type TurnWaiter = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

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

/// The requests of one song now: the one queued or running, the requests that may not share its
/// download waiting behind it, each its own, in the order they came, and the files the song's
/// requests landed meanwhile.
struct SongRequests {
    running: Arc<AcquisitionRequest>,
    behind: VecDeque<Arc<AcquisitionRequest>>,
    landed: Vec<String>,
}

impl SongRequests {
    fn new(running: Arc<AcquisitionRequest>) -> Self {
        SongRequests {
            running,
            behind: VecDeque::new(),
            landed: Vec::new(),
        }
    }

    /// The request of the song `request` may share: the one running, else one waiting behind it.
    fn sharing(&self, request: &AcquisitionRequest) -> Option<Arc<AcquisitionRequest>> {
        std::iter::once(&self.running)
            .chain(self.behind.iter())
            .find(|known| known.shares_with(request))
            .cloned()
    }
}

/// Where a new request of a song went.
enum Claimed {
    /// Nothing of the song was queued: it is the song's request now.
    New,
    /// It shares this request of the song.
    Joins(Arc<AcquisitionRequest>),
    /// It waits behind the song's running request as its own.
    Behind,
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
    in_flight: Mutex<HashMap<String, SongRequests>>,

    // Requests in the plays channel that the worker has not taken yet.
    waiting_plays: AtomicI32,

    // Stars queued and not yet handed out, in the channel or lined up per person: the bound
    // counts both, so lining stars up never lets more than STAR_CAPACITY wait.
    waiting_stars: AtomicUsize,

    /// Whether stars take turns per person (while Family is on); otherwise first in, first out.
    fair_turns: FairTurns,

    /// Wakes work whose file waits for AcoustID's bulk turn, once the lane gives turns again.
    turns: Mutex<Option<TurnWaiter>>,

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
            turns: Mutex::new(None),
            #[cfg(test)]
            on_skipped_play: Mutex::new(None),
        }
    }

    /// Wakes work whose file waits for AcoustID's bulk turn through `turns`.
    pub fn use_turns(&self, turns: TurnWaiter) {
        *self.turns.lock() = Some(turns);
    }

    /// Waits until AcoustID's lane gives turns again; false at once when nothing can say so.
    pub async fn wait_for_turn(&self) -> bool {
        let Some(turns) = self.turns.lock().clone() else {
            return false;
        };
        let (woken, wait) = tokio::sync::oneshot::channel::<()>();
        turns(Box::new(move || {
            let _ = woken.send(());
        }));
        wait.await.is_ok()
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

    /// The request queued or running under `key`.
    #[cfg(test)]
    pub(crate) fn running(&self, key: &str) -> Option<Arc<AcquisitionRequest>> {
        self.in_flight.lock().get(key).map(|song| song.running.clone())
    }

    /// How many requests of the song wait behind its running one.
    #[cfg(test)]
    pub(crate) fn waiting_behind(&self, provider: &str, external_id: &str) -> usize {
        self.in_flight
            .lock()
            .get(&acquisition_key(provider, external_id, false))
            .map_or(0, |song| song.behind.len())
    }

    /// Claims the song for `request`, joins a request of it `request` may share, or puts it behind
    /// the song's running request as its own.
    fn claim(&self, request: &Arc<AcquisitionRequest>) -> Claimed {
        let mut in_flight = self.in_flight.lock();
        let Some(song) = in_flight.get_mut(&request.key()) else {
            in_flight.insert(request.key(), SongRequests::new(request.clone()));
            return Claimed::New;
        };
        match song.sharing(request) {
            Some(known) => Claimed::Joins(known),
            None => {
                song.behind.push_back(request.clone());
                Claimed::Behind
            }
        }
    }

    /// Hands a request that holds its song's claim to the worker's lane. False when the lane is
    /// full: the request is dropped, saying so, its claim let go and its completion failed.
    fn send(&self, request: &Arc<AcquisitionRequest>) -> bool {
        let is_star = request.is_star;
        if is_star {
            self.waiting_stars.fetch_add(1, Ordering::SeqCst);
        } else {
            self.waiting_plays.fetch_add(1, Ordering::SeqCst);
        }
        let channel = if is_star { &self.stars } else { &self.plays };
        let room = !is_star || self.waiting_stars.load(Ordering::SeqCst) <= Self::STAR_CAPACITY;
        if room && channel.try_send(request.clone()).is_ok() {
            return true;
        }
        if is_star {
            self.waiting_stars.fetch_sub(1, Ordering::SeqCst);
        } else {
            self.waiting_plays.fetch_sub(1, Ordering::SeqCst);
        }
        // Full. Say so out loud: silently shedding work is how "why didn't that download?" becomes
        // unanswerable.
        warn!(
            "Acquisition queue full ({}); dropped {}:{}",
            if is_star { "star" } else { "play" },
            request.provider,
            request.external_id
        );
        self.release(request);
        request.completion.try_set_error(queue_full());
        false
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
            HeldFiles::default(),
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

    /// [`Self::enqueue_as`] for a resumed hold: `held` is what it held, which the download checks
    /// before any source.
    #[allow(clippy::too_many_arguments)] // enqueue_as's arguments and the held files
    pub fn enqueue_holding(
        &self,
        origin: Option<AcquisitionOrigin>,
        gates: Vec<HeartGate>,
        held: HeldFiles,
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
        self.enqueue_denying(
            None,
            origin,
            gates,
            held,
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

    /// [`Self::enqueue_holding`] for a replacement that refuses files by its original's sound. A
    /// request already in flight is joined as it is, without `deny`, as a replacement always was.
    #[allow(clippy::too_many_arguments)] // enqueue_holding's arguments and what is refused
    pub fn enqueue_denying(
        &self,
        deny: Option<SoundDeny>,
        origin: Option<AcquisitionOrigin>,
        gates: Vec<HeartGate>,
        held: HeldFiles,
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
        request.deny = deny;
        self.queue_request(request, gates, held, target, requested_by, upgrade_search, false)
            .0
    }

    /// A person's keep of what a hold kept for them: the download lands one of `held_files` as it
    /// is, or else the very audio a gate's person heard, and searches for nothing; it counts as
    /// kept only for the people of `gates`. It shares a download only with another keep of the same
    /// person: every other request of the song runs on its own, before or after it. Answers the
    /// completion, and whether it joined such a keep in flight.
    #[allow(clippy::too_many_arguments)] // why it runs, whose it is, what it keeps and where from
    pub fn enqueue_keeping(
        &self,
        origin: AcquisitionOrigin,
        gates: Vec<HeartGate>,
        held_files: Vec<String>,
        target: &AcquisitionTarget,
        provider: &str,
        external_id: &str,
        source_override: Option<DownloadSource>,
        requested_by: Option<&str>,
    ) -> (Completion, bool) {
        let request = AcquisitionRequest::new(
            provider,
            external_id,
            true,
            false,
            true,
            source_override,
            true,
            None,
        )
        .with_origin(origin);
        // A keep lands a held file as it is: nothing reads its labels.
        self.queue_request(
            request,
            gates,
            HeldFiles::of(held_files, Vec::new()),
            target,
            requested_by,
            false,
            true,
        )
    }

    /// Queues `request` with its askers, joins a request of its song it may share, or waits behind
    /// the song's running request as its own. `held`: what a hold held. True with the completion
    /// when it joined.
    #[allow(clippy::too_many_arguments)] // the request and what each asker brings to it
    fn queue_request(
        &self,
        mut request: AcquisitionRequest,
        gates: Vec<HeartGate>,
        held: HeldFiles,
        target: &AcquisitionTarget,
        requested_by: Option<&str>,
        upgrade_search: bool,
        keep: bool,
    ) -> (Completion, bool) {
        let (provider, external_id) = (request.provider.clone(), request.external_id.clone());
        let (is_star, notify_on_failure) = (request.is_star, request.notify_on_failure);
        request.keep_anyway = keep;
        let request = Arc::new(request);
        request.add_requester(requested_by);
        request.add_target(target);
        for gate in &gates {
            request.add_gate(gate.clone());
        }
        if keep {
            request.add_keepers(&gates);
        }
        request.add_held_files(&held.files, &held.lidarr);
        if held.shared {
            request.held_for_others.store(true, Ordering::SeqCst);
        }
        if upgrade_search {
            request.ask_for_upgrade_search();
        }

        match self.claim(&request) {
            Claimed::Joins(existing) => {
                // Already queued or running for the same people. Join it rather than fetching the
                // same file twice, and record this caller on the request that is actually going to
                // run, so the file is attributed to everyone who asked and not just to whoever was
                // first. A heart for a track a play already asked for. The source cannot change any
                // more, but the failure is now someone's explicit ask, and the heart chain still
                // owns its fallback.
                let heart = (is_star && !existing.is_star).then_some(notify_on_failure);
                if gates.is_empty() {
                    existing.add_requester(requested_by);
                    existing.add_target(target);
                    if let Some(notify) = heart {
                        existing.join_heart(notify);
                    }
                } else {
                    // A heart or play with its own gate counts the file as its own only once that
                    // gate passes it.
                    existing.join_gated(&gates, requested_by, target, heart);
                }
                if keep {
                    existing.add_keepers(&gates);
                }
                existing.add_held_files(&held.files, &held.lidarr);
                if held.shared {
                    existing.held_for_others.store(true, Ordering::SeqCst);
                }
                if upgrade_search {
                    existing.ask_for_upgrade_search();
                }
                (existing.completion.clone(), true)
            }
            Claimed::Behind => {
                info!(
                    "{provider}:{external_id} is being fetched for someone else; this {} waits behind it as its own",
                    if is_star { "star" } else { "play" }
                );
                (request.completion.clone(), false)
            }
            Claimed::New => {
                if self.send(&request) {
                    info!(
                        "Queued {} acquisition for {provider}:{external_id}",
                        if is_star { "star" } else { "play" }
                    );
                }
                (request.completion.clone(), false)
            }
        }
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
        // Already wanted: ride along, whatever is waiting, on a request this play may share; else
        // wait behind the song's running request as its own.
        let known = {
            let mut in_flight = self.in_flight.lock();
            in_flight.get_mut(&request.key()).map(|song| match song.sharing(&request) {
                Some(known) => Claimed::Joins(known),
                None => {
                    song.behind.push_back(request.clone());
                    Claimed::Behind
                }
            })
        };
        match known {
            Some(Claimed::Joins(known)) => {
                join_play(&known, &gates, requested_by, target);
                return Some(known);
            }
            Some(_) => {
                if let Some(on_queued) = on_queued {
                    on_queued();
                }
                info!("{provider}:{external_id} is being fetched for someone else; this play waits behind it");
                return Some(request);
            }
            None => {}
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
        match self.claim(&request) {
            Claimed::Joins(existing) => {
                self.waiting_plays.fetch_sub(1, Ordering::SeqCst);
                join_play(&existing, &gates, requested_by, target);
                return Some(existing);
            }
            Claimed::Behind => {
                self.waiting_plays.fetch_sub(1, Ordering::SeqCst);
                if let Some(on_queued) = on_queued {
                    on_queued();
                }
                return Some(request);
            }
            Claimed::New => {}
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
        self.release_landed(request, None);
    }

    /// [`Self::release`] for a request whose file landed at `landed`: the next request waiting
    /// behind it, someone else's, runs now and judges that file with its own gates first.
    pub(crate) fn release_landed(&self, request: &Arc<AcquisitionRequest>, landed: Option<&str>) {
        let next = {
            let mut in_flight = self.in_flight.lock();
            let key = request.key();
            let Some(song) = in_flight.get_mut(&key) else {
                return;
            };
            if !Arc::ptr_eq(&song.running, request) {
                // One that never ran lets go of its place behind.
                song.behind.retain(|waiting| !Arc::ptr_eq(waiting, request));
                return;
            }
            if let Some(path) = landed.filter(|path| !path.trim().is_empty())
                && !song.landed.iter().any(|known| known == path)
            {
                song.landed.push(path.to_string());
            }
            match song.behind.pop_front() {
                Some(next) => {
                    next.landed.lock().extend(song.landed.iter().cloned());
                    song.running = next.clone();
                    Some(next)
                }
                None => {
                    in_flight.remove(&key);
                    None
                }
            }
        };
        if let Some(next) = next
            && self.send(&next)
        {
            info!(
                "{}:{} runs again for the next person who asked for it",
                next.provider, next.external_id
            );
        }
    }
}

fn queue_full() -> anyhow::Error {
    anyhow::anyhow!("Acquisition queue is full; try again shortly.")
}

/// A play riding along on a request in flight: with a gate of its own, its library and name wait
/// for that gate's verdict; without one, they are recorded at once.
fn join_play(
    request: &AcquisitionRequest,
    gates: &[HeartGate],
    requested_by: Option<&str>,
    target: &AcquisitionTarget,
) {
    if gates.is_empty() {
        request.add_requester(requested_by);
        request.add_target(target);
    } else {
        request.join_gated(gates, requested_by, target, None);
    }
}

#[cfg(test)]
#[path = "track_acquisition_queue_tests.rs"]
mod tests;
