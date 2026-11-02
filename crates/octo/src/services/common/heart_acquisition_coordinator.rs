//! The heart acquisition coordinator: which source a heart or a play is handed to.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::fingerprint::heart_gate::{
    Acceptance, AcquisitionOrigin, GateVerdict, HeardReading, HeardRecord, HeardRecording, HeardSource,
    HeartGate, WaitingFor,
};
use octo_core::models::domain::Album;
use octo_core::settings::{DownloadSource, HeartDownloadSource, SettingsStore};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::TrackAcquisitionQueue;
use super::acquisition_tracker::key_of;
use super::album_gates::{
    AlbumGate, AlbumGateHold, AlbumGateTrack, AlbumGates, TrackFiling, tracks_wait_line,
};
use super::soulseek_hold_store::{HeldAlbumTrack, HeldLidarrFile, Holder};
use super::{
    AcquisitionHeld, AcquisitionState, AcquisitionTarget, AcquisitionTracker, HeldAcquisition, HeldFiles,
    HeldKind, SoulseekHoldStore, StarOnArrival,
};
use crate::services::family::{FamilyPool, LibraryScope, LibraryWriteGate, WriteDecision};
use crate::services::fingerprint::MusicBrainzClient;
use crate::services::fingerprint::acoust_id_rate_limiter::{AcoustIdRateLimiter, LookupClass};
use crate::services::fingerprint::download_verification_service::{
    ANOTHER_SONG_PLAYED, LabelSource, NEVER_PLAYED,
};
use crate::services::fingerprint::music_brainz_client::HeartBusy;
use crate::services::i_download_service::IDownloadService;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::imports::trickle_worker::{TrickleCheckLater, WAITING_FOR_ANSWER};
use crate::services::library::{HeartOwnership, OwnedCopy};
use crate::services::lidarr::{ILidarrHeartAcquisitionService, LidarrWaits};
use crate::services::prefetch::{HeardSamples, PrefetchedFiles};
use crate::services::soulseek::soulseek_link::SoulseekLinkState;
use crate::services::soulseek::{ExternalIdRegistry, ISoulseekLink};

/// What one heart's or play's gate came to before its sources run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateStart {
    /// Run the sources, judging every file with this gate.
    Ready(HeartGate),
    /// Hold before any source runs, with the line the downloads row shows. `question` is the
    /// MusicBrainz question that did not answer, when that is what it waits for: its answer
    /// resumes the hold.
    Hold {
        gate: HeartGate,
        waiting_for: WaitingFor,
        line: String,
        question: Option<String>,
    },
    /// The heart ends before any source runs, with the line the downloads row shows: nothing
    /// could ever prove a copy for it.
    End { gate: HeartGate, line: String },
}

/// Why no gate of a chain is ready to run its sources.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GateStop {
    /// Every holder holds; the first wait.
    Held(WaitingFor),
    /// Every holder's heart ended, with this line.
    Ended(String),
}

/// Builds each heart's, play's or import's gate before its sources run, one per person who
/// asked, from what they heard. Without one every acquisition runs with no gate, which keeps
/// the shipped checks.
#[async_trait]
pub trait HeartGates: Send + Sync {
    async fn build(
        &self,
        provider: &str,
        external_id: &str,
        holder: &Holder,
        origin: AcquisitionOrigin,
    ) -> GateStart;

    /// `user` was filed in the hold `record`: what their gate took is kept with it, and what the
    /// hold waits for is asked again until it answers.
    fn on_filed(&self, _record: &HeldAcquisition, _user: &str) {}

    /// The hold `key` runs again once `waiting_for` answers for `user` (who heard `heard`;
    /// `question` the MusicBrainz question that did not answer), whatever the hold itself waits
    /// for: a held album track whose check of a new play met a busy answer keeps that play.
    fn wake_for(
        &self,
        _key: &str,
        _waiting_for: WaitingFor,
        _user: &str,
        _heard: Option<&HeardRecord>,
        _question: Option<String>,
    ) {
    }

    /// Opens one heard-master round per gate for one pass of the chain over the acquisition
    /// `acquisition_key`; the rounds end when what this returns is dropped.
    fn open_rounds(&self, _acquisition_key: &str, _gates: &[HeartGate]) -> Box<dyn Send> {
        Box::new(())
    }

    /// The open rounds of `acquisition_key` give their heard-copy places back before the chain
    /// waits in the queue; the next check makes each copy again.
    fn pause_rounds(&self, _acquisition_key: &str) {}

    /// Whether `gate` proves the person's library copy at `path` is the version they heard: its
    /// own clear reading against R, or the very master that played. Without a way to judge it,
    /// an owned copy never counts.
    async fn owned_passes(&self, _path: &str, _gate: &HeartGate) -> bool {
        false
    }

    /// One holder's gate for a track whose recording is known (a hearted album's track): what
    /// they heard is read against `r`, and no album context is read for it. Without a way of its
    /// own, the gate is built as any other.
    async fn build_for_track(
        &self,
        provider: &str,
        external_id: &str,
        holder: &Holder,
        origin: AcquisitionOrigin,
        r: &HeardRecording,
    ) -> GateStart {
        let _ = r;
        self.build(provider, external_id, holder, origin).await
    }

    /// Judges one held file of a hearted album's track with these gates by the album's rule, its
    /// labels read from `labels`, fetching nothing else: one verdict per gate. `acquisition_key`
    /// finds each person's open heard-master round. Without a way to judge it, no gate judges it
    /// and it stays held.
    async fn judge_held(
        &self,
        _path: &str,
        _labels: LabelSource,
        _acquisition_key: &str,
        _gates: &[HeartGate],
    ) -> Vec<GateVerdict> {
        Vec::new()
    }

    /// Judges a file with each gate as a fresh download's file is judged, its labels read from
    /// `labels`: a file another acquisition landed, which these gates' chain joined and nobody
    /// judged for them, or a held album track's file for those in it by their heart of the song.
    /// One verdict per gate. `acquisition_key` finds each person's open heard-master round.
    /// Without a way to judge it, no gate judges it.
    async fn judge_landed(
        &self,
        _path: &str,
        _labels: LabelSource,
        _acquisition_key: &str,
        _gates: &[HeartGate],
    ) -> Vec<GateVerdict> {
        Vec::new()
    }

    /// Files one file of a hearted album's whole import among the gate's `unfilled` tracks (their
    /// places in its list) by its own sound, its labels read from `labels`: where it is filed and,
    /// for a file that lands, the verdict it is tagged by. Without a way to judge it, nothing
    /// proves it is any of them.
    async fn file_album_file(
        &self,
        _path: &std::path::Path,
        _labels: LabelSource,
        _gate: &AlbumGate,
        _unfilled: &[usize],
        _acquisition_key: &str,
    ) -> (TrackFiling, Option<octo_core::fingerprint::VerificationResult>) {
        (
            TrackFiling::Refuse(octo_core::fingerprint::heart_gate::Refusal::Unknown),
            None,
        )
    }
}

/// The coordinator's optional collaborators, each `None` by default.
#[derive(Default, Clone)]
pub struct HeartCoordinatorExtras {
    /// The chain is the only place that knows which failure is the last, so it is the one that
    /// reports it. Without one, nothing is reported.
    pub tracker: Option<Arc<AcquisitionTracker>>,
    pub id_registry: Option<Arc<ExternalIdRegistry>>,
    /// Without one nothing waits for Soulseek, which is how it always was.
    pub soulseek: Option<Arc<dyn ISoulseekLink>>,
    pub holds: Option<Arc<SoulseekHoldStore>>,
    /// Without it every heart goes down the chain, and the download itself notices a song that
    /// is already there, as before.
    pub owned: Option<Arc<HeartOwnership>>,
    pub stars: Option<Arc<StarOnArrival>>,
    /// The songs fetched ahead: a heart on one is kept at once, without the source chain.
    pub ahead: Option<Arc<dyn PrefetchedFiles>>,
    /// Builds each asker's gate before the sources run. Without it no gate is built and nothing
    /// holds for an answer.
    pub gates: Option<Arc<dyn HeartGates>>,
    /// Asks a hold's MusicBrainz question again until it answers, which resumes the hold.
    pub music_brainz: Option<Arc<MusicBrainzClient>>,
    /// What people heard: a hold keeps their heard copies, and a heart of a song waiting on the
    /// shim starts a round.
    pub heard: Option<Arc<HeardSamples>>,
    /// Whether AcoustID is on. Without it, it counts as on.
    pub acoust_id_on: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Files kept as heard, checked again when their song is hearted and after each heart a gate
    /// proves. Without it nothing is checked again.
    pub kept: Option<Arc<crate::services::library::upgrade_queue::KeptRechecks>>,
    /// The gates of the album chains that run now, which every source's check point files an
    /// album's tracks by. Without it an album heart's tracks keep the checks they had.
    pub album_gates: Option<Arc<AlbumGates>>,
    /// The catalog a hearted album's tracks are listed from; without it, Family's.
    pub metadata: Option<Arc<dyn IMusicMetadataService>>,
    /// The library's own record of each file it placed, which says whose keep a kept file is.
    /// Without it, no keep counts as landed before a restart.
    pub library: Option<Arc<dyn crate::services::local::ILocalLibraryService>>,
    /// Whether Deezer's last call answered: the count of times it answered again after a call it
    /// did not answer, or None while its last call went unanswered. Without it, a hold for Deezer
    /// waits for the next such answer.
    pub deezer_answering: Option<Arc<dyn Fn() -> Option<u64> + Send + Sync>>,
}

/// How one song's heart chain ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackOutcome {
    /// A direct source got it; the file is here (a pool file for a member).
    Landed(String),
    /// The library (in the scope the chain looked in) already had it; it was favorited.
    AlreadyThere,
    /// Lidarr took it on, for the shared library.
    HandedToLidarr,
    /// Every source failed, with the last one's reason.
    Failed(String),
    /// The heart holds for this, its files in the one hold path.
    Held(WaitingFor),
    /// Nothing could be proven, so the heard audio itself was kept; the file is here.
    KeptAsHeard(String),
}

/// Whether a gate or a download holding for `waiting_for` files a hold record for `origin`.
/// Plays never hold, and neither does a family request, whose own state answers the member; an
/// import waiting for MusicBrainz or Deezer is put back on the trickle instead, to be asked again
/// on its next turn.
fn files_hold(origin: AcquisitionOrigin, waiting_for: WaitingFor) -> bool {
    match origin {
        AcquisitionOrigin::Play | AcquisitionOrigin::Request => false,
        AcquisitionOrigin::Import => !trickle_waits(waiting_for),
        _ => true,
    }
}

/// Whether an import waiting for this goes back on the trickle: an answer from MusicBrainz or
/// Deezer, or a sound `fpcalc` could not read, which the trickle's next turn tries again.
fn trickle_waits(waiting_for: WaitingFor) -> bool {
    matches!(
        waiting_for,
        WaitingFor::MusicBrainz | WaitingFor::Deezer | WaitingFor::SoundUnread
    )
}

/// What a chain notes in `filed` for a holder whose gate did not pass the file that landed: they
/// stay where they were.
const STAYS: &str = "";

/// The downloads line of a heart that kept the heard audio, while a source can look for better.
pub const KEPT_AS_HEARD: &str =
    "Kept the version you heard. Octo keeps looking for a better copy it can prove";
/// The downloads line of a heart that kept the heard audio when none of its sources can search.
pub const KEPT_NO_SOURCE: &str =
    "Kept the version you heard. Add Soulseek or Mornye in your sources so Octo can look for a better copy";
/// The downloads line of a heart that kept the heard audio after a copy AcoustID could not tell
/// apart from another version.
pub const KEPT_UNDECIDED: &str =
    "Kept the version you heard: Octo could not prove which version this copy is";
/// The downloads line of a heart whose heard audio is gone with nothing proven.
pub const HEARD_GONE: &str = "Octo could not find a copy it can prove is the version you heard";
/// The downloads line of a heart over a library copy nothing proves, the heard audio kept beside it.
pub const KEPT_BESIDE_OWNED: &str = "You already had a copy; Octo kept the version you heard beside it";
/// The downloads line of a play over a library copy: nothing is downloaded.
pub const PLAY_OVER_OWNED: &str =
    "You already have this song; Octo could not check it is the version you heard";
/// The downloads line of a heart that waited for MusicBrainz, which gave up on its question, and
/// ended with nothing.
pub const MUSIC_BRAINZ_GAVE_UP: &str = "MusicBrainz could not answer for this song";

/// How a heart's chain ended when no source brought a file its gates passed.
struct ChainEnd<'a> {
    holders: &'a [Holder],
    gates: &'a [HeartGate],
    /// A file a gate could not tell apart from another version was passed over.
    undecided: bool,
    /// The library already had a copy nothing proved is the version heard.
    beside_owned: bool,
    /// The sources the chain ran.
    steps: &'a [HeartDownloadSource],
    /// The last source's reason.
    reason: String,
}

/// The gates that keep the heard audio itself: each judges only the very audio its person heard,
/// by its identity, so nothing but that copy can pass, and no other video is tried for it.
fn keep_gates(gates: &[HeartGate]) -> Vec<HeartGate> {
    gates
        .iter()
        .filter(|gate| gate.heard.is_some())
        .map(|gate| HeartGate {
            reading: HeardReading::OnlyHeardOrMaster,
            ..gate.clone()
        })
        .collect()
}

/// What a running heart chain or play holds of what its people heard, for as long as it runs: each
/// heard copy under an id of the run's own (a hold keeps the same copy under the person's id, which
/// no run's end frees) and each heard sample, so no sweep, cap or re-pin takes them meanwhile.
type HeldWhileRunning = crate::services::prefetch::heard_samples::RunHold;

/// The line a held row shows when its hold kept none of its own.
pub(crate) fn waiting_line(waiting_for: WaitingFor) -> &'static str {
    match waiting_for {
        WaitingFor::MusicBrainz => "Waiting for MusicBrainz",
        WaitingFor::LidarrIndexers => "Waiting for Lidarr's indexers",
        WaitingFor::AcoustId => "Waiting for AcoustID",
        WaitingFor::AcoustIdOff => "AcoustID is off, so Octo cannot check what played",
        WaitingFor::Deezer => "Waiting for the music catalog",
        WaitingFor::Shim => "Waiting for YouTube",
        WaitingFor::NotYetProven => "Play it once so Octo can keep the version you hear",
        WaitingFor::SoundUnread => {
            crate::services::fingerprint::download_verification_service::SOUND_NOT_READ
        }
    }
}

/// The key of a track's holds, whatever they wait for.
pub(crate) fn track_base_key(provider: &str, external_id: &str) -> String {
    HeldAcquisition {
        kind: HeldKind::Track,
        provider: provider.to_string(),
        external_id: external_id.to_string(),
        ..Default::default()
    }
    .base_key()
}

/// What the coordinator uses for family members: the write gate (where a member's play goes),
/// the family pool (members' album songs are starred where they land) and the catalog (an
/// album heart for a member is fetched song by song).
#[derive(Clone)]
pub struct FamilyHooks {
    pub gate: Arc<LibraryWriteGate>,
    pub pool: Arc<FamilyPool>,
    pub metadata: Arc<dyn IMusicMetadataService>,
}

/// Routes explicit heart gestures, and plays when DownloadOnPlay or LidarrAlbumOnPlay ask
/// for it.
pub struct HeartAcquisitionCoordinator {
    /// The chain is read at every heart.
    settings: Arc<SettingsStore>,
    direct_queue: Arc<TrackAcquisitionQueue>,
    direct_downloads: Arc<dyn IDownloadService>,
    lidarr: Arc<dyn ILidarrHeartAcquisitionService>,
    extras: HeartCoordinatorExtras,

    // Plays waiting for Soulseek, one per song however often it is played meanwhile.
    held_plays: Mutex<HashSet<String>>,

    // Track ids handed to Lidarr on play, so a replay does not search every indexer again.
    // Cleared when full rather than aged: forgetting costs at most one more search.
    lidarr_plays: Mutex<HashSet<String>>,

    // Set once Family's services exist; without them every heart and play goes to the shared
    // library, as always.
    family: OnceLock<FamilyHooks>,

    // How long a song heart's album waits for the hearted song to show in the library.
    landed_wait: Mutex<LandedSongWait>,

    // The coordinator itself, so an answer that arrives later can resume a hold.
    me: Weak<Self>,

    // The holds whose acquisition is running again now, by key: a second resume waits for it.
    resuming: Mutex<HashSet<String>>,

    // The holds woken again while they ran (a joiner, a new heard source, an answer): each runs
    // once more when its run ends, so no wake is lost.
    again: Mutex<HashSet<String>>,
    // The holds whose MusicBrainz question was given up on, each with its song's provider and id:
    // the run that serves that wake says so on the song's row, by what its last pass came to.
    gave_up: Mutex<std::collections::HashMap<String, (String, String)>>,

    // Told each time a hold's run lets go of it, so a recheck that found one going waits for it.
    run_ended: tokio::sync::Notify,

    // Held album tracks whose recheck waits for Deezer's next answer.
    deezer_waits: Mutex<HashSet<String>>,

    // The hold whose turn it is to run once Deezer answers again: holds waiting for Deezer run one
    // at a time, the one served longest ago first, the next once this one gets past Deezer or
    // Deezer answers another call while this one still fails.
    deezer_turn: Mutex<Option<String>>,

    // For each hold waiting for Deezer, Deezer's answer count when its last turn began: a hold has
    // one turn per count, so one that keeps failing for its own reason never starves the rest.
    deezer_turns: Mutex<HashMap<String, u64>>,

    // How many times Deezer was said to answer again, the count read without a way to ask Deezer.
    deezer_events: std::sync::atomic::AtomicU64,

    // For each hold filed while Deezer was answering, how many times Deezer had answered again
    // then: one that answered since runs it once more, and no more until it answers again.
    deezer_seen: Mutex<HashMap<String, u64>>,
}

/// How long the album walk that follows a song heart waits for the hearted song to show in the
/// library, and how often it looks. The walk skips the songs the library already has, so one
/// that started while the hearted song was still on its way in would fetch that song again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LandedSongWait {
    pub poll: Duration,
    pub max_wait: Duration,
}

impl Default for LandedSongWait {
    fn default() -> Self {
        LandedSongWait {
            poll: Duration::from_secs(2),
            max_wait: Duration::from_secs(3 * 60),
        }
    }
}

impl HeartAcquisitionCoordinator {
    const LIDARR_PLAY_MEMORY: usize = 10_000;

    pub fn new(
        settings: Arc<SettingsStore>,
        direct_queue: Arc<TrackAcquisitionQueue>,
        direct_downloads: Arc<dyn IDownloadService>,
        lidarr: Arc<dyn ILidarrHeartAcquisitionService>,
        extras: HeartCoordinatorExtras,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| HeartAcquisitionCoordinator {
            settings,
            direct_queue,
            direct_downloads,
            lidarr,
            extras,
            held_plays: Mutex::new(HashSet::new()),
            lidarr_plays: Mutex::new(HashSet::new()),
            family: OnceLock::new(),
            landed_wait: Mutex::new(LandedSongWait::default()),
            me: me.clone(),
            resuming: Mutex::new(HashSet::new()),
            again: Mutex::new(HashSet::new()),
            run_ended: tokio::sync::Notify::new(),
            gave_up: Mutex::new(std::collections::HashMap::new()),
            deezer_waits: Mutex::new(HashSet::new()),
            deezer_turn: Mutex::new(None),
            deezer_turns: Mutex::new(HashMap::new()),
            deezer_events: std::sync::atomic::AtomicU64::new(0),
            deezer_seen: Mutex::new(HashMap::new()),
        })
    }

    /// Changes how long a song heart's album waits for the hearted song.
    #[cfg(test)]
    pub fn set_landed_song_wait(&self, wait: LandedSongWait) {
        *self.landed_wait.lock() = wait;
    }

    /// Gives the coordinator Family's services. Once only; a second call changes nothing.
    pub fn use_family(&self, hooks: FamilyHooks) {
        let _ = self.family.set(hooks);
    }

    /// Where an import list's song goes for `user`, or the words that say why it may not be
    /// fetched now. A member who must ask first has songs queued only for a list an approver
    /// said yes to, so theirs go to their own library too.
    pub fn import_target(&self, user: &str) -> Result<AcquisitionTarget, String> {
        let Some(family) = self.family.get() else {
            return Ok(AcquisitionTarget::Shared);
        };
        match family.gate.decide(Some(user)) {
            WriteDecision::Shared => Ok(AcquisitionTarget::Shared),
            WriteDecision::Member(user) => Ok(AcquisitionTarget::Member(user)),
            WriteDecision::Request { user, full: None } => Ok(AcquisitionTarget::Member(user)),
            WriteDecision::Request { full: Some(full), .. } => Err(full),
            WriteDecision::Refuse { message, .. } => Err(message),
        }
    }

    fn tracker(&self) -> Option<&Arc<AcquisitionTracker>> {
        self.extras.tracker.as_ref()
    }

    pub fn queue_track(self: &Arc<Self>, provider: &str, external_id: &str, requested_by: Option<&str>) {
        self.queue_track_for(provider, external_id, requested_by, AcquisitionTarget::Shared);
    }

    /// [`Self::queue_track`] into one library: the shared one, or a family member's own.
    pub fn queue_track_for(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        target: AcquisitionTarget,
    ) {
        let coordinator = Arc::clone(self);
        let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
        // Fire and forget: a chain that stops on an error is left as it stands.
        tokio::spawn(async move {
            let _ = coordinator
                .acquire_track_for(
                    &provider,
                    &external_id,
                    requested_by.as_deref(),
                    None,
                    &target,
                    AcquisitionOrigin::Heart,
                )
                .await;
        });
    }

    /// A client started an external track from its first byte.
    pub fn queue_play(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
    ) {
        self.queue_play_as(provider, external_id, requested_by, client_id, owner, false, None);
    }

    /// [`Self::queue_play`] carrying what this play itself played, which its gate is built from.
    pub fn queue_play_heard(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
        heard: Option<HeardRecord>,
    ) {
        self.queue_play_as(
            provider,
            external_id,
            requested_by,
            client_id,
            owner,
            false,
            heard,
        );
    }

    /// A song coming up next, queued with DownloadOnPlay on as the play it will be, only
    /// earlier. Unlike a play under way, it is queued with Wait for lossless on play on too,
    /// since that waits only for the song playing.
    pub fn queue_play_ahead(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
    ) {
        self.queue_play_as(provider, external_id, requested_by, client_id, owner, true, None);
    }

    #[allow(clippy::too_many_arguments)] // the play's facts and what it played
    fn queue_play_as(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
        ahead: bool,
        heard: Option<HeardRecord>,
    ) {
        let settings = self.settings.current().subsonic.clone();
        // A family member's play keeps a copy only when they may add songs at once, and then in
        // their own library; `owner` is who signed in. Everyone else plays as always.
        let target = match self.family.get() {
            Some(family) => match family.gate.play_target(owner) {
                Some(target) => target,
                None => return,
            },
            None => AcquisitionTarget::Shared,
        };
        // With AcoustID off only the heard audio can prove a play's download, so a play with
        // none (nothing playable, or a song coming up next) fetches nothing from any source.
        if heard.is_none() && !self.acoust_id_is_on() {
            debug!(
                "AcoustID is off and {provider}:{external_id} has no heard audio; the play fetches nothing"
            );
            return;
        }
        // WaitForLosslessOnPlay acquires the track itself, from its own source.
        let download = settings.download_on_play && (ahead || !settings.wait_for_lossless_on_play);
        let source = self.play_source().filter(|_| download);
        // Lidarr adds whole albums to the shared library, never to a member's own, once per song.
        let lidarr_key = format!("{provider}:{external_id}");
        let lidarr = settings.lidarr_album_on_play && target == AcquisitionTarget::Shared && {
            let mut plays = self.lidarr_plays.lock();
            if plays.len() >= Self::LIDARR_PLAY_MEMORY {
                plays.clear();
            }
            plays.insert(lidarr_key.clone())
        };
        let holds = self
            .extras
            .soulseek
            .as_ref()
            .is_some_and(|link| link.hold_limit() > TimeDelta::zero());
        let waits = source.is_some_and(|source| source != DownloadSource::YouTube) && holds;
        if self.extras.gates.is_none() && !waits {
            if let Some(source) = source {
                self.queue_play_download(
                    provider,
                    external_id,
                    source,
                    requested_by,
                    client_id,
                    owner,
                    &target,
                    Vec::new(),
                    None,
                );
            }
            if lidarr {
                let coordinator = Arc::clone(self);
                let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
                tokio::spawn(async move {
                    coordinator
                        .hand_play_to_lidarr(
                            &lidarr_key,
                            &provider,
                            &external_id,
                            requested_by.as_deref(),
                            &[],
                        )
                        .await
                });
            }
            return;
        }
        if source.is_none() && !lidarr {
            return;
        }
        // The play's gate is built from what it played before anything is fetched for it.
        let coordinator = Arc::clone(self);
        let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
        let (client_id, owner) = (client_id.map(str::to_string), owner.map(str::to_string));
        tokio::spawn(async move {
            // What the play heard is kept from before its gate is built until its download, and
            // any keep of what it played, ends.
            let player = Holder {
                user: requested_by
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string(),
                heard: heard.clone(),
                ..Default::default()
            };
            let held = coordinator.hold_while_running(&provider, &external_id, &[player], true);
            // A play never downloads over a copy already in the library: its row says so.
            if coordinator
                .play_over_owned(
                    &provider,
                    &external_id,
                    client_id.as_deref(),
                    owner.as_deref(),
                    &target,
                )
                .await
            {
                coordinator.lidarr_plays.lock().remove(&lidarr_key);
                return;
            }
            let gates = match coordinator
                .play_gates(&provider, &external_id, requested_by.as_deref(), heard)
                .await
            {
                Ok(gates) => gates,
                Err(held) => {
                    let (gate, waiting) = *held;
                    // A play never holds: what its gate would wait for, it does without.
                    info!(
                        "The gate of the play of {provider}:{external_id} would wait; the play fetches nothing"
                    );
                    coordinator.lidarr_plays.lock().remove(&lidarr_key);
                    // With DownloadOnPlay, a play whose heard lookup AcoustID could not answer
                    // keeps what it played, and nothing else.
                    if source.is_some() && waiting == WaitingFor::AcoustId {
                        coordinator
                            .keep_play_heard(&provider, &external_id, requested_by.as_deref(), &target, gate)
                            .await;
                    }
                    return;
                }
            };
            if lidarr {
                // Never behind a Soulseek outage's wait.
                let lidarr = Arc::clone(&coordinator);
                let (provider, external_id, requested_by) =
                    (provider.clone(), external_id.clone(), requested_by.clone());
                let play_gates = gates.clone();
                tokio::spawn(async move {
                    lidarr
                        .hand_play_to_lidarr(
                            &lidarr_key,
                            &provider,
                            &external_id,
                            requested_by.as_deref(),
                            &play_gates,
                        )
                        .await
                });
            }
            if let Some(source) = source {
                if waits {
                    coordinator
                        .hold_then_queue_play(
                            &provider,
                            &external_id,
                            source,
                            requested_by.as_deref(),
                            client_id.as_deref(),
                            owner.as_deref(),
                            &target,
                            gates,
                            held,
                        )
                        .await
                } else {
                    coordinator.queue_play_download(
                        &provider,
                        &external_id,
                        source,
                        requested_by.as_deref(),
                        client_id.as_deref(),
                        owner.as_deref(),
                        &target,
                        gates,
                        held,
                    );
                }
            }
        });
    }

    /// Whether AcoustID is on; without a way to ask, it counts as on.
    fn acoust_id_is_on(&self) -> bool {
        self.extras.acoust_id_on.as_ref().is_none_or(|on| on())
    }

    /// The gate of `user`'s family request for a song fetched at one quality, built as every
    /// request's is: R from the catalog, so the reading's refusals apply to its file. A request
    /// never holds: what its gate would wait for, it goes on without. Empty when no gates are
    /// built.
    pub async fn request_gates(&self, provider: &str, external_id: &str, user: &str) -> Vec<HeartGate> {
        let Some(builder) = self.extras.gates.clone() else {
            return Vec::new();
        };
        let holder = self.holder_for(external_id, Some(user));
        match builder
            .build(provider, external_id, &holder, AcquisitionOrigin::Request)
            .await
        {
            GateStart::Ready(gate) | GateStart::Hold { gate, .. } | GateStart::End { gate, .. } => {
                vec![gate]
            }
        }
    }

    /// A play's gate, built from what this play itself played (never another play's), when
    /// gates are built at all. Err when the gate would hold, with that gate and what it would
    /// wait for: a play never holds, so it fetches nothing then.
    async fn play_gates(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        heard: Option<HeardRecord>,
    ) -> Result<Vec<HeartGate>, Box<(HeartGate, WaitingFor)>> {
        let Some(builder) = self.extras.gates.clone() else {
            return Ok(Vec::new());
        };
        let holder = Holder {
            user: requested_by.map(str::trim).unwrap_or_default().to_string(),
            heard,
            ..Default::default()
        };
        match builder
            .build(provider, external_id, &holder, AcquisitionOrigin::Play)
            .await
        {
            GateStart::Ready(gate) => Ok(vec![gate]),
            GateStart::Hold {
                gate, waiting_for, ..
            } => Err(Box::new((gate, waiting_for))),
            // A play is never ended this way; with nothing to prove, it fetches nothing.
            GateStart::End { gate, .. } => Err(Box::new((gate, WaitingFor::NotYetProven))),
        }
    }

    /// Whether the library already has a copy of the played song (in the scope of whose library
    /// the play is for): then the play downloads nothing, and its row closes on that copy with the
    /// line that says nothing checked it is the version heard.
    async fn play_over_owned(
        &self,
        provider: &str,
        external_id: &str,
        client_id: Option<&str>,
        owner: Option<&str>,
        target: &AcquisitionTarget,
    ) -> bool {
        let scope = match target {
            AcquisitionTarget::Shared => LibraryScope::Shared,
            AcquisitionTarget::Member(user) => LibraryScope::Viewer(user.clone()),
        };
        let Some((song, copy)) = self.owned_copy(provider, external_id, &scope).await else {
            return false;
        };
        info!(
            "The play of {provider}:{external_id} downloads nothing: the library has {}",
            copy.absolute_path
        );
        // A held song's row keeps the line it waits with: the play leaves it alone.
        if let Some(tracker) = self
            .tracker()
            .filter(|tracker| !tracker.is_held(provider, external_id))
        {
            tracker.begin(
                provider,
                external_id,
                client_id,
                owner,
                Some(&song.artist),
                Some(&song.title),
                Some(&song.album),
            );
            tracker.done_line(provider, external_id, PLAY_OVER_OWNED);
            self.settle(provider, external_id, &copy);
        }
        true
    }

    /// A DownloadOnPlay play nothing could prove keeps the very audio it played, kept as heard,
    /// when it heard a video that may be the song: never a stream that stood in with another song,
    /// and never anything else fetched for it.
    async fn keep_play_heard(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        target: &AcquisitionTarget,
        gate: HeartGate,
    ) {
        let heard_video = gate
            .heard
            .as_ref()
            .is_some_and(|heard| heard.source.video_id().is_some());
        if !heard_video || matches!(gate.reading, HeardReading::NotHeard | HeardReading::AnotherSong) {
            return;
        }
        match self
            .keep_heard(
                provider,
                external_id,
                requested_by,
                AcquisitionOrigin::Play,
                target,
                &[gate],
            )
            .await
        {
            Ok((path, _)) => info!("The play of {provider}:{external_id} kept what it played: {path}"),
            Err(e) => debug!("The play of {provider}:{external_id} could not keep what it played: {e}"),
        }
    }

    /// Queues a play's download, judged by its gates. `held` is what the play heard, kept since
    /// before its gate was built; without it, what its gates heard is kept from here.
    #[allow(clippy::too_many_arguments)] // the play's facts, whose library it is for and its gate
    fn queue_play_download(
        &self,
        provider: &str,
        external_id: &str,
        source: DownloadSource,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
        target: &AcquisitionTarget,
        gates: Vec<HeartGate>,
        held: Option<HeldWhileRunning>,
    ) {
        let on_queued = || {
            // A held song's row keeps the line it waits with: a play never takes it over.
            if self
                .tracker()
                .is_some_and(|tracker| tracker.is_held(provider, external_id))
            {
                return;
            }
            // Same row a heart gets, opened before the worker can report its first stage.
            let routing = self
                .extras
                .id_registry
                .as_ref()
                .and_then(|r| r.lookup(external_id))
                .map(|shared| shared.snapshot());
            if let Some(tracker) = self.tracker() {
                tracker.begin(
                    provider,
                    external_id,
                    client_id,
                    owner,
                    routing.as_ref().and_then(|r| r.artist.as_deref()),
                    routing.as_ref().and_then(|r| r.title.as_deref()),
                    routing.as_ref().and_then(|r| r.album.as_deref()),
                );
                tracker.stage(
                    provider,
                    external_id,
                    AcquisitionState::Queued,
                    Some(if source == DownloadSource::YouTube {
                        "YouTube"
                    } else {
                        "Soulseek"
                    }),
                    None,
                );
            }
        };
        let own_gates = gates.clone();
        let Some(request) = self.direct_queue.try_enqueue_play_for(
            target,
            provider,
            external_id,
            source,
            requested_by,
            Some(&on_queued),
            gates,
        ) else {
            return;
        };
        // What the play heard is kept until its download, and any keep of what it played, ends.
        let held = held.or_else(|| {
            let heard_holders: Vec<Holder> = own_gates
                .iter()
                .map(|gate| Holder {
                    user: gate.user.clone(),
                    heard: gate.heard.clone(),
                    ..Default::default()
                })
                .collect();
            self.hold_while_running(provider, external_id, &heard_holders, true)
        });
        let tracker = self.tracker().cloned();
        let coordinator = self.me.clone();
        let (provider, external_id) = (provider.to_string(), external_id.to_string());
        let (requested_by, target) = (requested_by.map(str::to_string), target.clone());
        tokio::spawn(async move {
            let _held = held;
            let Err(reason) = request.completion.wait().await else {
                return;
            };
            debug!("Play download failed for {provider}:{external_id}: {reason}");
            // Nothing its gate could pass: the play keeps what it played instead.
            let mine = |gate: &&HeartGate| {
                !request.completion.ledger().accepted(&gate.user)
                    && reason.downcast_ref::<AcquisitionHeld>().is_none()
            };
            if let (Some(coordinator), Some(gate)) =
                (coordinator.upgrade(), own_gates.iter().find(mine).cloned())
            {
                coordinator
                    .keep_play_heard(&provider, &external_id, requested_by.as_deref(), &target, gate)
                    .await;
            }
            // A heart that joined owns the row, and may still be trying its next source; a held
            // song's row keeps the line it waits with.
            if !request.is_star
                && !request.heart_joined()
                && let Some(tracker) = tracker
                && !tracker.is_held(&provider, &external_id)
            {
                tracker.fail(&provider, &external_id, Some(&reason.to_string()));
            }
        });
    }

    /// A play during a Soulseek outage waits for it like a heart, but only in memory: a play is a
    /// hint, and one lost to a restart costs a replay.
    #[allow(clippy::too_many_arguments)] // the play's facts, whose library it is for and its gate
    async fn hold_then_queue_play(
        &self,
        provider: &str,
        external_id: &str,
        source: DownloadSource,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
        target: &AcquisitionTarget,
        gates: Vec<HeartGate>,
        held: Option<HeldWhileRunning>,
    ) {
        let key = format!("{provider}:{external_id}");
        let Some(link) = self.extras.soulseek.clone() else {
            return;
        };
        if link.read(false).await.map(|r| r.link) == Some(SoulseekLinkState::NotLoggedIn) {
            if !self.held_plays.lock().insert(key.clone()) {
                return;
            }
            link.wait_for_login(link.utc_now() + link.hold_limit()).await;
            self.held_plays.lock().remove(&key);
        }
        self.queue_play_download(
            provider,
            external_id,
            source,
            requested_by,
            client_id,
            owner,
            target,
            gates,
            held,
        );
    }

    /// A play's album through Lidarr, the played song's file judged by the play's own gate.
    async fn hand_play_to_lidarr(
        &self,
        key: &str,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        gates: &[HeartGate],
    ) {
        match self
            .lidarr
            .try_acquire_track(
                provider,
                external_id,
                false,
                requested_by,
                AcquisitionOrigin::Play,
                gates,
            )
            .await
        {
            Ok(true) => return,
            Ok(false) => debug!("Lidarr took no album for played track {provider}:{external_id}"),
            Err(e) => debug!("Lidarr hand-off failed for played track {provider}:{external_id}: {e}"),
        }
        // Not handed off, so the next play of this track tries again.
        self.lidarr_plays.lock().remove(key);
    }

    pub fn queue_album(
        self: &Arc<Self>,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
    ) {
        let coordinator = Arc::clone(self);
        let (provider, album_id, requested_by) = owned3(provider, album_external_id, requested_by);
        tokio::spawn(async move {
            let _ = coordinator
                .acquire_album(&provider, &album_id, requested_by.as_deref(), None)
                .await;
        });
    }

    /// [`Self::queue_album`] into one library. A member's album comes song by song into the
    /// family pool, each song starred for them where it lands.
    pub fn queue_album_for(
        self: &Arc<Self>,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        target: AcquisitionTarget,
    ) {
        let AcquisitionTarget::Member(user) = target else {
            self.queue_album(provider, album_external_id, requested_by);
            return;
        };
        let coordinator = Arc::clone(self);
        let (provider, album_id, requested_by) = owned3(provider, album_external_id, requested_by);
        tokio::spawn(async move {
            coordinator
                .acquire_album_for_member(
                    &provider,
                    &album_id,
                    requested_by.as_deref(),
                    &user,
                    true,
                    AcquisitionOrigin::AlbumHeart,
                )
                .await;
        });
    }

    /// Hearted songs whose album follows them (DownloadAlbumOnSongStar). Each song goes down the
    /// song chain first, then the album goes down the album chain, as an album heart would,
    /// once the songs that landed show in the library (waiting at most [`LandedSongWait`]), so
    /// the walk skips them rather than fetching them again. When Lidarr took a song on it
    /// brings the whole album, so the walk is left out. A member's album songs are not starred
    /// for them: only the songs they hearted are. `owner` is who the album's progress list
    /// belongs to.
    pub fn queue_tracks_then_album_for(
        self: &Arc<Self>,
        songs: &[(String, String)],
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        owner: Option<&str>,
        target: AcquisitionTarget,
    ) {
        let coordinator = Arc::clone(self);
        let songs = songs.to_vec();
        let (provider, album_id, requested_by) = owned3(provider, album_external_id, requested_by);
        let owner = owner.map(str::to_string);
        tokio::spawn(async move {
            coordinator
                .acquire_tracks_then_album(
                    &songs,
                    &provider,
                    &album_id,
                    requested_by.as_deref(),
                    owner.as_deref(),
                    &target,
                )
                .await;
        });
    }

    /// [`Self::queue_tracks_then_album_for`], run to its end.
    pub async fn acquire_tracks_then_album(
        &self,
        songs: &[(String, String)],
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        owner: Option<&str>,
        target: &AcquisitionTarget,
    ) {
        let chains = songs.iter().map(|(song_provider, external_id)| async move {
            let outcome = self
                .acquire_track_outcome(
                    song_provider,
                    external_id,
                    requested_by,
                    None,
                    target,
                    AcquisitionOrigin::Heart,
                )
                .await;
            (song_provider, external_id, outcome)
        });
        let mut landed = Vec::new();
        let mut lidarr = false;
        for (song_provider, external_id, outcome) in futures::future::join_all(chains).await {
            match outcome {
                Ok(TrackOutcome::Landed(_) | TrackOutcome::KeptAsHeard(_)) => {
                    landed.push(key_of(song_provider, external_id))
                }
                Ok(TrackOutcome::HandedToLidarr) => lidarr = true,
                Ok(TrackOutcome::AlreadyThere | TrackOutcome::Failed(_) | TrackOutcome::Held(_)) => {}
                Err(e) => warn!("Hearted song {song_provider}:{external_id} failed: {e}"),
            }
        }
        if lidarr {
            info!(
                "Lidarr took on a hearted song, and with it its album; album {album_external_id} is not walked as well"
            );
            return;
        }
        self.wait_for_landed(&landed, album_external_id).await;

        info!("Hearted song's album {album_external_id} goes down the album-heart chain");
        if let Some(tracker) = self.tracker() {
            tracker.begin_album(provider, album_external_id, owner);
        }
        match target {
            AcquisitionTarget::Shared => {
                if let Err(e) = self
                    .acquire_album(provider, album_external_id, requested_by, None)
                    .await
                {
                    warn!("Album {provider}:{album_external_id} of a hearted song failed: {e}");
                }
            }
            AcquisitionTarget::Member(user) => {
                self.acquire_album_for_member(
                    provider,
                    album_external_id,
                    requested_by,
                    user,
                    false,
                    AcquisitionOrigin::AlbumHeart,
                )
                .await;
            }
        }
    }

    /// Waits until none of these rows (songs that landed) is still on its way into the library,
    /// at most [`LandedSongWait::max_wait`]. The library answers held from before they landed
    /// are forgotten, so the album walk asks the library again.
    async fn wait_for_landed(&self, rows: &[String], album_external_id: &str) {
        if rows.is_empty() {
            return;
        }
        if let Some(tracker) = self.tracker() {
            let wait = *self.landed_wait.lock();
            let started = tokio::time::Instant::now();
            while rows.iter().any(|key| tracker.is_running(key)) {
                if started.elapsed() >= wait.max_wait {
                    info!(
                        "The hearted song is not in the library after {} s; album {album_external_id} goes on without waiting longer",
                        wait.max_wait.as_secs()
                    );
                    break;
                }
                tokio::time::sleep(wait.poll).await;
            }
        }
        if let Some(owned) = &self.extras.owned {
            owned.forget();
        }
    }

    /// Every song of an outside album into `user`'s own library, one heart chain each (no
    /// Lidarr, which fills the shared library), each run for `origin`. With `star`, each song is
    /// starred for them once it shows in their library. Answers where each song that landed is.
    pub async fn acquire_album_for_member(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        user: &str,
        star: bool,
        origin: AcquisitionOrigin,
    ) -> Vec<String> {
        let Some(family) = self.family.get().cloned() else {
            return Vec::new();
        };
        let Some(album) = family.metadata.get_album(provider, album_external_id).await else {
            if let Some(tracker) = self.tracker() {
                tracker.fail_album(
                    provider,
                    album_external_id,
                    Some("Octo could no longer look this album up, so nothing was downloaded."),
                );
            }
            return Vec::new();
        };
        let tracks: Vec<_> = album
            .songs
            .iter()
            .filter(|song| song.external_id.as_deref().is_some_and(|id| !id.is_empty()))
            .collect();
        if let Some(tracker) = self.tracker() {
            let listed: Vec<(String, Option<String>, Option<String>, Option<String>)> = tracks
                .iter()
                .map(|song| {
                    (
                        song.external_id.clone().unwrap_or_default(),
                        Some(song.artist.clone()),
                        Some(song.title.clone()),
                        Some(album.title.clone()),
                    )
                })
                .collect();
            tracker.announce(provider, Some(album_external_id), None, &listed);
        }
        info!(
            "Album '{}' for {user}: {} song(s) into their own library",
            album.title,
            tracks.len()
        );
        let target = AcquisitionTarget::Member(user.to_string());
        let mut landed = Vec::new();
        // Each song's chain carries the album's gate: its track's own recording, filed by sound.
        let _gate = match self
            .album_gate_up(provider, album_external_id, &album, user, origin)
            .await
        {
            Ok(gate) => gate,
            Err(()) => return Vec::new(),
        };
        // Each song is filed at its place on this album, with the album tags its songs settle.
        self.direct_downloads.hold_album_listing(provider, &album);
        for song in tracks {
            let track_provider = song.external_provider.as_deref().unwrap_or(provider);
            let track_id = song.external_id.as_deref().unwrap_or_default();
            match self
                .acquire_track_outcome(track_provider, track_id, requested_by, None, &target, origin)
                .await
            {
                Ok(TrackOutcome::Landed(path) | TrackOutcome::KeptAsHeard(path)) => {
                    if star {
                        let shown = family.pool.link_path(&path, user).unwrap_or_else(|| path.clone());
                        family.pool.star_when_indexed(user, &shown);
                    }
                    landed.push(path);
                }
                Ok(_) => {}
                Err(e) => warn!("Album song {track_provider}:{track_id} for {user} failed: {e}"),
            }
        }
        self.direct_downloads.finish_album_listing(provider, &album).await;
        self.album_row(provider, album_external_id);
        landed
    }

    /// The catalog's listing of an outside album.
    async fn album_listing(&self, provider: &str, album_external_id: &str) -> Option<Album> {
        let metadata = self
            .extras
            .metadata
            .clone()
            .or_else(|| self.family.get().map(|family| family.metadata.clone()))?;
        metadata.get_album(provider, album_external_id).await
    }

    /// Builds and registers `user`'s gate of a hearted album before any of its tracks is fetched,
    /// answering the chain's hold on it (None when no gates are built at all). Err when the album
    /// heart holds instead, for the catalog or MusicBrainz, its row saying so.
    async fn album_gate_up(
        &self,
        provider: &str,
        album_external_id: &str,
        album: &Album,
        user: &str,
        origin: AcquisitionOrigin,
    ) -> Result<Option<AlbumGateHold>, ()> {
        let (Some(builder), Some(store)) = (self.extras.gates.clone(), self.extras.album_gates.clone())
        else {
            return Ok(None);
        };
        if user.trim().is_empty() {
            return Ok(None);
        }
        let mut tracks = Vec::new();
        for song in album
            .songs
            .iter()
            .filter(|song| song.external_id.as_deref().is_some_and(|id| !id.is_empty()))
        {
            let track_provider = song.external_provider.as_deref().unwrap_or(provider);
            let track_id = song.external_id.clone().unwrap_or_default();
            let holder = Holder {
                user: user.trim().to_string(),
                ..Default::default()
            };
            let named = || HeardRecording {
                isrcs: song
                    .isrc
                    .as_deref()
                    .and_then(octo_core::common::SongIdentity::normalize_isrc)
                    .into_iter()
                    .collect(),
                title: song.title.clone(),
                artist: song.artist.clone(),
                ..Default::default()
            };
            let r = match builder.build(track_provider, &track_id, &holder, origin).await {
                // An album track's gate never ends: it goes on with what is known of it.
                GateStart::Ready(gate) | GateStart::End { gate, .. } => gate.r.unwrap_or_else(named),
                // A family request never holds: the track goes on with what is known of it.
                GateStart::Hold { gate, .. } if origin == AcquisitionOrigin::Request => {
                    gate.r.unwrap_or_else(named)
                }
                GateStart::Hold {
                    waiting_for,
                    question,
                    ..
                } => {
                    self.hold_album(
                        provider,
                        album_external_id,
                        Some(user),
                        origin,
                        waiting_for,
                        question,
                    );
                    return Err(());
                }
            };
            tracks.push(AlbumGateTrack {
                catalog_track_id: track_id,
                disc: song.disc_number,
                position: song.track,
                title: song.title.clone(),
                artist: song.artist.clone(),
                duration_seconds: song.duration,
                r,
            });
        }
        let gate = AlbumGate {
            user: user.trim().to_string(),
            origin,
            tracks,
        };
        // Tracks already held for this album wait where they are: no walk fetches them again, and
        // this person joins each of them by their album heart, so its proof or a keep lands it for
        // them too. A song's own hold of one of its tracks that holds files is one of them: it
        // becomes this album's held track. One holding no file is left to the walk, whose hold of
        // the track joins it.
        let album_key = key_of(provider, album_external_id);
        let track_of = |record: &HeldAcquisition| {
            gate.tracks
                .iter()
                .find(|track| track.catalog_track_id == record.external_id)
                .map(|track| HeldAlbumTrack {
                    album_key: album_key.clone(),
                    song_ids: vec![track.catalog_track_id.clone()],
                    artist: track.artist.clone(),
                    title: track.title.clone(),
                    duration_seconds: track.duration_seconds,
                    r: track.r.clone(),
                })
        };
        let held: Vec<(HeldAcquisition, Option<HeldAlbumTrack>)> = self
            .extras
            .holds
            .as_ref()
            .map(|holds| {
                holds
                    .snapshot()
                    .into_iter()
                    .filter_map(|record| {
                        let track = match &record.album_track {
                            Some(track) if track.album_key == album_key => None,
                            Some(_) => return None,
                            None => {
                                // A heart's own hold of the song only: an import's is judged as
                                // one, never as an album's track.
                                let song_hold = record.kind == HeldKind::Track
                                    && record.waiting_for.is_some()
                                    && record.origin() == AcquisitionOrigin::Heart
                                    && eq_ignore_case(&record.provider, provider)
                                    && record
                                        .held_files
                                        .iter()
                                        .any(|file| std::path::Path::new(file).is_file());
                                match track_of(&record) {
                                    Some(track) if song_hold => Some(track),
                                    _ => return None,
                                }
                            }
                        };
                        Some((record, track))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(holds) = self.extras.holds.as_ref() {
            let joining: Vec<&(HeldAcquisition, Option<HeldAlbumTrack>)> = held
                .iter()
                .filter(|(record, _)| {
                    !record.holders.iter().any(|holder| {
                        eq_ignore_case(holder.user.trim(), user.trim())
                            && holder
                                .routes_in(record)
                                .contains(&super::soulseek_hold_store::HoldRoute::Album)
                    })
                })
                .collect();
            for (record, track) in &joining {
                holds.hold(HeldAcquisition {
                    kind: record.kind,
                    provider: record.provider.clone(),
                    external_id: record.external_id.clone(),
                    requested_by: record.requested_by.clone(),
                    held_since_utc: record.held_since_utc,
                    origin: record.origin,
                    waiting_for: record.waiting_for,
                    holders: vec![Holder {
                        user: user.trim().to_string(),
                        routes: vec![super::soulseek_hold_store::HoldRoute::Album],
                        ..Default::default()
                    }],
                    album_track: track.clone(),
                    ..Default::default()
                });
                info!(
                    "{user} joins held {}, a track of their hearted album",
                    record.key()
                );
            }
            if !joining.is_empty() {
                self.album_row(provider, album_external_id);
            }
        }
        let settled: Vec<String> = held.into_iter().map(|(record, _)| record.external_id).collect();
        Ok(Some(store.register(provider, album_external_id, gate, settled)))
    }

    /// A hearted album whose tracks' recordings the catalog or MusicBrainz could not answer for,
    /// or whose album a Lidarr step run last could not look up, holds as one, its row saying what
    /// it waits for, and runs again once that answers.
    fn hold_album(
        &self,
        provider: &str,
        album_external_id: &str,
        user: Option<&str>,
        origin: AcquisitionOrigin,
        waiting_for: WaitingFor,
        question: Option<String>,
    ) {
        let Some(holds) = self.extras.holds.as_ref() else {
            return;
        };
        let user = user.map(str::trim).filter(|user| !user.is_empty());
        let line = waiting_line(waiting_for);
        let kept = holds.hold(HeldAcquisition {
            kind: HeldKind::Album,
            provider: provider.to_string(),
            external_id: album_external_id.to_string(),
            requested_by: user.map(str::to_string),
            held_since_utc: Utc::now(),
            origin: Some(origin),
            waiting_for: Some(waiting_for),
            holders: user
                .map(|user| Holder {
                    user: user.to_string(),
                    ..Default::default()
                })
                .into_iter()
                .collect(),
            line: Some(line.to_string()),
            ..Default::default()
        });
        if let Some(tracker) = self.tracker() {
            let owners: Vec<String> = kept.holders.iter().map(|holder| holder.user.clone()).collect();
            tracker.hold_album_row(provider, album_external_id, &owners, line, false);
        }
        info!("Holding album {}: {line}", kept.key());
        if waiting_for == WaitingFor::MusicBrainz
            && let Some(question) = question
        {
            self.re_ask_for(&kept.key(), &question);
        }
        if waiting_for == WaitingFor::Deezer {
            self.deezer_filed(&kept.key());
        }
    }

    /// The held album track `track_id` would be for `user`: the album gate their chain runs
    /// with, else a hold of that track already kept for an album.
    fn album_track_of(&self, provider: &str, track_id: &str, user: &str) -> Option<HeldAlbumTrack> {
        if let Some((album_id, _, track)) = self
            .extras
            .album_gates
            .as_ref()
            .and_then(|gates| gates.for_track(provider, track_id, user))
        {
            return Some(HeldAlbumTrack {
                album_key: key_of(provider, &album_id),
                song_ids: vec![track.catalog_track_id.clone()],
                artist: track.artist,
                title: track.title,
                duration_seconds: track.duration_seconds,
                r: track.r,
            });
        }
        self.extras
            .holds
            .as_ref()?
            .records_for(&track_base_key(provider, track_id))
            .into_iter()
            .find_map(|record| record.album_track)
    }

    /// The album row of a hearted album says how many of its tracks wait, owned by everyone who
    /// holds one, named from the album's listing, and offers to keep them anyway; once none waits,
    /// it lets the row go.
    pub(crate) fn album_row(&self, provider: &str, album_external_id: &str) {
        let (Some(tracker), Some(holds)) = (self.tracker(), self.extras.holds.as_ref()) else {
            return;
        };
        let album_key = key_of(provider, album_external_id);
        let held: Vec<HeldAcquisition> = holds
            .snapshot()
            .into_iter()
            .filter(|record| {
                record
                    .album_track
                    .as_ref()
                    .is_some_and(|track| track.album_key == album_key)
            })
            .collect();
        if held.is_empty() {
            if tracker.is_held(provider, album_external_id) {
                tracker.release_row(provider, album_external_id, holds);
                tracker.complete(provider, album_external_id, None);
            }
            return;
        }
        let mut owners: Vec<String> = Vec::new();
        for holder in held.iter().flat_map(|record| &record.holders) {
            if !owners
                .iter()
                .any(|known| eq_ignore_case(known, holder.user.trim()))
            {
                owners.push(holder.user.trim().to_string());
            }
        }
        let keep_anyway = held.iter().any(HeldAcquisition::keeps_anyway);
        tracker.hold_album_row(
            provider,
            album_external_id,
            &owners,
            &tracks_wait_line(held.len()),
            keep_anyway,
        );
        if let Some(album) = self
            .extras
            .id_registry
            .as_ref()
            .and_then(|ids| ids.lookup(album_external_id))
            .map(|shared| shared.snapshot())
        {
            let name = album.album.as_deref().or(album.title.as_deref());
            tracker.describe(provider, album_external_id, album.artist.as_deref(), name, name);
        }
    }

    /// The tracks a walk (or Lidarr's import) of a hearted album held, filed as held album tracks
    /// of `user`'s heart, each with its files, and the album's row.
    pub(crate) fn file_album_holds(
        &self,
        provider: &str,
        album_external_id: &str,
        user: &str,
        origin: AcquisitionOrigin,
    ) {
        let Some(store) = self.extras.album_gates.as_ref() else {
            return;
        };
        for (track_id, held) in store.take_held(provider, album_external_id, user) {
            let holder = self.holder_for(&track_id, Some(user));
            self.file_hold(
                provider,
                &track_id,
                Some(user),
                origin,
                held.waiting_for,
                vec![holder],
                &held.line,
                Some(&held),
                None,
                &mut Vec::new(),
            );
        }
        self.album_row(provider, album_external_id);
    }

    /// `user` took their heart of an album back: they leave every held track of it their album
    /// heart keeps them in (one their heart of the song also keeps them in stays theirs), and every
    /// song's own hold that heart joined, and a hold nobody is left in ends, its files deleted. A
    /// held track that only song hearts keep now is the song's own hold again, off the album's
    /// row. How many holds they left.
    pub fn leave_album_holds(&self, provider: &str, album_external_id: &str, user: &str) -> usize {
        use super::soulseek_hold_store::HoldRoute;
        let Some(holds) = self.extras.holds.as_ref() else {
            return 0;
        };
        let album_key = key_of(provider, album_external_id);
        let mut left = 0;
        for record in holds.snapshot().into_iter().filter(|record| {
            record
                .album_track
                .as_ref()
                .is_some_and(|track| track.album_key == album_key)
        }) {
            if !self.leave_by_route(&record, user, HoldRoute::Album) {
                continue;
            }
            left += 1;
            if let Some(still) = holds.get(&record.key())
                && !still
                    .holders
                    .iter()
                    .any(|holder| holder.routes_in(&still).contains(&HoldRoute::Album))
            {
                holds.update(&record.key(), as_song_hold);
            }
            self.let_row_go(&record.provider, &record.external_id, user);
        }
        // A song's own hold they joined by this album heart while it held no file: the album it
        // names is this one.
        for record in holds.snapshot().into_iter().filter(|record| {
            record.album_track.is_none()
                && record.holders.iter().any(|holder| {
                    eq_ignore_case(holder.user.trim(), user.trim())
                        && by_album(holder, record)
                        && holder
                            .context
                            .as_ref()
                            .is_some_and(|context| key_of(provider, &context.catalog_album_id) == album_key)
                })
        }) {
            if self.leave_by_route(&record, user, HoldRoute::Album) {
                left += 1;
                self.let_row_go(&record.provider, &record.external_id, user);
            }
        }
        // The album heart itself, held for an answer before any track was fetched.
        let album = HeldAcquisition {
            kind: HeldKind::Album,
            provider: provider.to_string(),
            external_id: album_external_id.to_string(),
            ..Default::default()
        };
        for record in holds.records_for(&album.base_key()).into_iter().filter(|record| {
            record
                .holders
                .iter()
                .any(|holder| eq_ignore_case(holder.user.trim(), user.trim()))
        }) {
            left += 1;
            holds.release_holder(&record.key(), user);
            if let Some(tracker) = self.tracker() {
                tracker.release_row(provider, album_external_id, holds);
            }
        }
        if left > 0 {
            self.album_row(provider, album_external_id);
        }
        left
    }

    /// A heart that was waiting for Soulseek when Octo restarted. Its saved start time keeps the
    /// original deadline. Every person waiting in it runs their own chain again, and each leaves
    /// the hold once their chain has finished either way.
    pub(crate) fn resume_track(self: &Arc<Self>, held: HeldAcquisition) {
        for user in Self::outage_people(&held) {
            let coordinator = Arc::clone(self);
            let held = held.clone();
            tokio::spawn(async move {
                let chain = coordinator
                    .acquire_track_for(
                        &held.provider,
                        &held.external_id,
                        user.as_deref(),
                        Some(held.held_since_utc),
                        &AcquisitionTarget::Shared,
                        held.origin(),
                    )
                    .await;
                coordinator.resumed(&held, user.as_deref(), chain);
            });
        }
    }

    pub(crate) fn resume_album(self: &Arc<Self>, held: HeldAcquisition) {
        for user in Self::outage_people(&held) {
            let coordinator = Arc::clone(self);
            let held = held.clone();
            tokio::spawn(async move {
                let chain = coordinator
                    .acquire_album_as(
                        &held.provider,
                        &held.external_id,
                        user.as_deref(),
                        Some(held.held_since_utc),
                        held.origin(),
                    )
                    .await;
                coordinator.resumed(&held, user.as_deref(), chain);
            });
        }
    }

    /// Whose chains a Soulseek outage's hold runs again: each person in it, or its one requester
    /// when it lists nobody.
    fn outage_people(held: &HeldAcquisition) -> Vec<Option<String>> {
        if held.holders.is_empty() {
            return vec![held.requested_by.clone()];
        }
        held.holders
            .iter()
            .map(|holder| Some(holder.user.clone()))
            .collect()
    }

    /// The end of one person's resumed chain: a chain that failed is logged, and they leave the
    /// hold either way; it ends once nobody is left.
    fn resumed(&self, held: &HeldAcquisition, user: Option<&str>, chain: anyhow::Result<()>) {
        if let Err(e) = chain {
            warn!("Resumed {} failed: {e}", held.key());
        }
        if let Some(holds) = &self.extras.holds {
            self.leave_outage_hold(holds, &held.key(), user);
        }
    }

    /// Runs the held acquisition `key` again now: each holder's own chain, with their own gate, into
    /// their own library, every one of them judging the files the hold kept. A holder whose gate
    /// holds again stays in a hold of that wait; everyone else leaves the record with their chain's
    /// end, and the record ends when nobody is left. A wake that comes while it runs (a joiner, a new
    /// heard source, an answer) runs it once more when the run ends. Nothing for a hold that waits
    /// out a Soulseek outage (its own chain is waiting), or for no such hold.
    pub(crate) fn resume_held(self: &Arc<Self>, key: &str) {
        if !self.claim_run(key) {
            return;
        }
        info!("Resuming held {key}");
        let coordinator = Arc::clone(self);
        let key = key.to_string();
        tokio::spawn(async move { coordinator.run_claimed(&key).await });
    }

    /// [`Self::resume_held`] run to its end here; when it already runs, that run runs once more
    /// and this waits until it has.
    pub(crate) async fn recheck_held(&self, key: &str) {
        if !self.claim_run(key) {
            self.run_over(key).await;
            return;
        }
        info!("Checking held {key} again");
        self.run_claimed(key).await;
    }

    /// Waits until the held acquisition `key` neither runs nor has a run owed to it.
    async fn run_over(&self, key: &str) {
        loop {
            let mut ended = std::pin::pin!(self.run_ended.notified());
            ended.as_mut().enable();
            let going = {
                let again = self.again.lock();
                again.contains(key) || self.resuming.lock().contains(key)
            };
            if !going {
                return;
            }
            ended.await;
        }
    }

    /// Claims the held acquisition `key` for a run: false for no such hold waiting for anything,
    /// and false when it runs already, which then runs once more when it ends.
    fn claim_run(&self, key: &str) -> bool {
        let waits = self
            .extras
            .holds
            .as_ref()
            .and_then(|holds| holds.get(key))
            .is_some_and(|record| record.waiting_for.is_some());
        if !waits {
            // Nothing waits under the key: no wake of it is left to answer, no turn at Deezer is
            // left to it, and no give-up line is owed to it.
            if self.again.lock().remove(key) {
                self.run_ended.notify_waiters();
            }
            self.deezer_turn_ended(key);
            self.gave_up.lock().remove(key);
            return false;
        }
        let mut again = self.again.lock();
        if self.resuming.lock().insert(key.to_string()) {
            return true;
        }
        again.insert(key.to_string());
        false
    }

    /// The run of the held acquisition `key` lets go of it.
    fn release_run(&self, key: &str) {
        self.resuming.lock().remove(key);
        self.run_ended.notify_waiters();
    }

    /// Runs the claimed hold `key`, its saved keeps after each run, and again while wakes came
    /// during a run; the claim ends with the last run.
    async fn run_claimed(&self, key: &str) {
        loop {
            let record = self
                .extras
                .holds
                .as_ref()
                .and_then(|holds| holds.get(key))
                .filter(|record| record.waiting_for.is_some());
            let Some(record) = record else {
                self.again.lock().remove(key);
                self.release_run(key);
                self.deezer_turn_ended(key);
                self.tell_gave_up(key);
                return;
            };
            // This run serves every wake that came before it.
            self.again.lock().remove(key);
            self.checked_again(&record);
            self.run_held(&record).await;
            self.release_run(key);
            self.keeps_after(key, &record.keep_requested).await;
            if !self.run_again(key) {
                return;
            }
            debug!("Held {key} was woken while it ran; it runs again");
        }
    }

    /// Whether the claimed run of the hold `key` that just ended runs once more: a wake came during
    /// it, and the run takes the hold again. With no wake, its last pass is over: the turn at Deezer
    /// and the give-up line are settled now. When another run took the hold meanwhile, the wake is
    /// left for that run, and so are the turn and the line: that run's last pass settles them.
    fn run_again(&self, key: &str) -> bool {
        let next = {
            let mut again = self.again.lock();
            if !again.remove(key) {
                Some(false)
            } else if self.resuming.lock().insert(key.to_string()) {
                Some(true)
            } else {
                again.insert(key.to_string());
                None
            }
        };
        match next {
            Some(again) => {
                if !again {
                    self.deezer_turn_ended(key);
                    self.tell_gave_up(key);
                }
                again
            }
            None => false,
        }
    }

    /// Octo's start: each hold a person waits on ("not yet proven" hearts and held album tracks)
    /// is checked again once, oldest first, one at a time, every AcoustID question it asks taking
    /// the lane's lowest place, behind everything else.
    pub(crate) async fn recheck_at_start(&self, keys: Vec<String>) {
        for key in keys {
            AcoustIdRateLimiter::at_least(LookupClass::Bulk, self.recheck_held(&key)).await;
        }
    }

    /// The holds a heart's proof may check again: every hold a person waits on ("not yet
    /// proven" hearts and held album tracks), each with since when it waits.
    pub fn rechecked_holds(&self) -> Vec<(String, DateTime<Utc>)> {
        self.extras
            .holds
            .as_ref()
            .map(|holds| {
                holds
                    .snapshot()
                    .into_iter()
                    .filter(waits_for_a_person)
                    .map(|record| (record.key(), record.held_since_utc))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A hold a person waits on was checked again now: it goes to the back of the order a heart's
    /// proof checks them in.
    fn checked_again(&self, record: &HeldAcquisition) {
        if waits_for_a_person(record)
            && let Some(kept) = &self.extras.kept
        {
            kept.checked_hold(&record.key());
        }
    }

    /// Runs again now every held acquisition that waits for `waiting_for`: AcoustID turning off
    /// sends every hold waiting for it to the catalog's clip, and a key working again sends every
    /// hold the clip check left back to the heard answer. Deezer answering again runs the holds
    /// waiting for it one at a time, the one served longest ago first, each once the one before
    /// got past Deezer.
    pub fn resume_waiting(self: &Arc<Self>, waiting_for: WaitingFor) {
        if waiting_for == WaitingFor::Deezer {
            self.deezer_events
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.wake_next_deezer();
            return;
        }
        let Some(holds) = self.extras.holds.as_ref() else {
            return;
        };
        for record in holds
            .snapshot()
            .into_iter()
            .filter(|record| record.waiting_for == Some(waiting_for))
        {
            self.resume_held(&record.key());
        }
    }

    /// Deezer's answer count now: how many times it answered again after a call it did not answer,
    /// or, without a way to ask, how many times it was said to. None while its last call went
    /// unanswered.
    fn deezer_count(&self) -> Option<u64> {
        match &self.extras.deezer_answering {
            Some(answering) => answering(),
            None => Some(self.deezer_events.load(std::sync::atomic::Ordering::SeqCst)),
        }
    }

    /// Runs the next hold that waits for Deezer (one filed under that wait, or a held album track
    /// whose check met a busy Deezer), when none has the turn: of those that have not had a turn at
    /// Deezer's answer count now, the one served longest ago (one never served first), oldest first
    /// among equals. It is that hold's turn until its run ends. Nothing while Deezer's last call
    /// went unanswered.
    fn wake_next_deezer(&self) {
        let Some(holds) = self.extras.holds.as_ref() else {
            return;
        };
        let next = {
            let mut turn = self.deezer_turn.lock();
            if turn.is_some() {
                return;
            }
            let Some(count) = self.deezer_count() else {
                return;
            };
            let waits = self.deezer_waits.lock().clone();
            let waiting: Vec<String> = holds
                .snapshot()
                .into_iter()
                .filter(|record| {
                    record.waiting_for == Some(WaitingFor::Deezer) || waits.contains(&record.key())
                })
                .map(|record| record.key())
                .collect();
            let mut turns = self.deezer_turns.lock();
            turns.retain(|key, _| waiting.contains(key));
            let next = waiting
                .into_iter()
                .filter(|key| turns.get(key) != Some(&count))
                .min_by_key(|key| turns.get(key).map_or(0, |served| served + 1));
            if let Some(key) = &next {
                turns.insert(key.clone(), count);
                *turn = Some(key.clone());
            }
            next
        };
        let Some(key) = next else {
            return;
        };
        self.deezer_waits.lock().remove(&key);
        info!("Deezer answers; {key}, the hold waiting for it served longest ago, runs");
        if let Some(me) = self.me.upgrade() {
            me.resume_held(&key);
        }
    }

    /// The run of the hold `key` ended. When it was the hold whose turn it was and it got past
    /// Deezer, the next hold waiting for Deezer runs. When it still waits for Deezer: if Deezer
    /// answered since its turn began, its failure is its own, and the next hold runs; else Deezer
    /// did not answer it, and every hold waits for Deezer's next answer.
    fn deezer_turn_ended(&self, key: &str) {
        {
            let mut turn = self.deezer_turn.lock();
            if turn.as_deref() != Some(key) {
                return;
            }
            *turn = None;
        }
        let still = self.deezer_waits.lock().contains(key)
            || self
                .extras
                .holds
                .as_ref()
                .and_then(|holds| holds.get(key))
                .is_some_and(|record| record.waiting_for == Some(WaitingFor::Deezer));
        if still {
            let began = self.deezer_turns.lock().get(key).copied();
            if self.deezer_count().is_none_or(|now| Some(now) == began) {
                debug!("Held {key} still waits for Deezer; every hold waits for its next answer");
                return;
            }
            debug!(
                "Held {key} still waits for Deezer, which answered since its turn began; the next hold runs"
            );
        } else {
            self.deezer_turns.lock().remove(key);
            self.deezer_seen.lock().remove(key);
        }
        self.wake_next_deezer();
    }

    /// The hold `key` was filed waiting for Deezer. When Deezer's last call answered, the answer
    /// that wakes it may have come before it was filed: it runs once more now, and not again until
    /// Deezer answers again after a call it did not answer.
    fn deezer_filed(&self, key: &str) {
        let Some(answered) = self
            .extras
            .deezer_answering
            .as_ref()
            .and_then(|answering| answering())
        else {
            return;
        };
        {
            let mut seen = self.deezer_seen.lock();
            if let Some(holds) = self.extras.holds.as_ref() {
                seen.retain(|known, _| holds.get(known).is_some());
            }
            if seen.insert(key.to_string(), answered) == Some(answered) {
                return;
            }
        }
        info!("Deezer answered since {key} met it busy; it runs once more");
        if let Some(me) = self.me.upgrade() {
            me.resume_held(key);
        }
    }

    /// Someone streamed a song a hold waits on. When `user` holds it, every hold of it that waits
    /// on what they heard (a proof, the heard lookup, the catalog's clip, the sound `fpcalc` could
    /// not read, the shim, and with AcoustID on a clip check that could not tell) takes this newer
    /// heard source and runs again with it. Anyone else's play only checks again, once, a
    /// hold a person waits on ("not yet proven"), taking nothing it heard: each person's hold
    /// keeps their own heard source. A hearted album's held track is found by the played song's
    /// id among its track's ids, else (a play reached from search or radio) by its artist, core
    /// title and length, when exactly one of the person's held tracks fits.
    pub fn heard_again(self: &Arc<Self>, external_id: &str, user: &str, record: &HeardRecord) {
        let Some(holds) = self.extras.holds.as_ref() else {
            return;
        };
        let routing_of = |id: &str| {
            self.extras
                .id_registry
                .as_ref()
                .and_then(|ids| ids.resolve_song(id, false))
                .map(|shared| shared.snapshot())
                .map(|routing| (routing.artist, routing.title, routing.duration))
        };
        let streamed = routing_of(external_id);
        let held_now = holds.snapshot();
        let holds_it = |held: &HeldAcquisition| {
            held.holders
                .iter()
                .any(|holder| eq_ignore_case(holder.user.trim(), user.trim()))
        };
        let by_id = |held: &HeldAcquisition| {
            held.external_id == external_id
                || held
                    .album_track
                    .as_ref()
                    .is_some_and(|track| track.song_ids.iter().any(|id| id == external_id))
        };
        let by_names = match &streamed {
            Some((artist, title, length))
                if !held_now
                    .iter()
                    .any(|held| held.album_track.is_some() && by_id(held)) =>
            {
                let fitting: Vec<String> = held_now
                    .iter()
                    .filter(|held| holds_it(held))
                    .filter(|held| {
                        held.album_track.as_ref().is_some_and(|track| {
                            plays_as(track, artist.as_deref(), title.as_deref(), *length)
                        })
                    })
                    .map(HeldAcquisition::key)
                    .collect();
                match fitting.as_slice() {
                    [only] => Some(only.clone()),
                    _ => None,
                }
            }
            _ => None,
        };
        for held in held_now.iter().cloned() {
            if held.kind != HeldKind::Track {
                continue;
            }
            let same_song = by_id(&held)
                || by_names.as_deref() == Some(held.key().as_str())
                || (held.album_track.is_none()
                    && streamed.is_some()
                    && routing_of(&held.external_id) == streamed);
            if !same_song {
                continue;
            }
            if !holds_it(&held) {
                if held.waiting_for == Some(WaitingFor::NotYetProven) {
                    info!(
                        "{user} streamed held {}; it is checked again with what its holders heard",
                        held.key()
                    );
                    self.resume_held(&held.key());
                }
                continue;
            }
            // A hold the clip check left unsure while AcoustID is on is read again from a new play
            // too: a working AcoustID reads the new heard audio.
            let waits_on_heard = match held.waiting_for {
                Some(
                    WaitingFor::NotYetProven
                    | WaitingFor::AcoustId
                    | WaitingFor::Deezer
                    | WaitingFor::Shim
                    | WaitingFor::SoundUnread,
                ) => true,
                Some(WaitingFor::AcoustIdOff) => self.acoust_id_is_on(),
                _ => false,
            };
            if !waits_on_heard {
                continue;
            }
            let newer = held
                .holders
                .iter()
                .find(|holder| eq_ignore_case(holder.user.trim(), user.trim()))
                .is_some_and(|holder| {
                    holder
                        .heard
                        .as_ref()
                        .is_none_or(|known| known.heard_at < record.heard_at)
                });
            if !newer {
                continue;
            }
            let key = held.key();
            holds.update(&key, |kept| {
                if let Some(holder) = kept
                    .holders
                    .iter_mut()
                    .find(|holder| eq_ignore_case(holder.user.trim(), user.trim()))
                {
                    holder.heard = Some(record.clone());
                    holder.whole_heard_print = None;
                    holder.shim_tries = Default::default();
                }
            });
            info!("{user} streamed held {key} again; it runs again with what they heard now");
            self.resume_held(&key);
        }
    }

    async fn run_held(&self, record: &HeldAcquisition) {
        // The keeps its people asked for are carried out first; whoever else waits runs as before.
        let Some(record) = self.keeps_first(record).await else {
            return;
        };
        // A held album track with no file left that a song heart holds goes on as the song's hold.
        let record = match self.spent_track_as_song_hold(&record) {
            Some(song) => song,
            None => record,
        };
        let record = &record;
        let Some(holds) = self.extras.holds.clone() else {
            return;
        };
        let key = record.key();
        let origin = record.origin();
        let requested_by = record
            .requested_by
            .clone()
            .or_else(|| record.holders.first().map(|holder| holder.user.clone()))
            .filter(|user| !user.trim().is_empty());
        let target = requested_by
            .as_deref()
            .and_then(|user| self.import_target(user).ok())
            .unwrap_or(AcquisitionTarget::Shared);
        // A hearted album's held track is checked again by its own held files, nothing fetched. One
        // held before any file was fetched runs its chain, each holder by the heart that keeps them.
        if record.album_track.is_some() && record.kind == HeldKind::Track && !record.held_files.is_empty() {
            self.recheck_album_track(record).await;
            return;
        }
        // The row leaves the line it waited with now: the whole chain runs again.
        if let Some(tracker) = self.tracker() {
            tracker.resume_row(&record.provider, &record.external_id);
        }
        // A hearted album held before any track was fetched runs as a fresh album chain, a member's
        // song by song; one that must wait again is held anew.
        if record.kind == HeldKind::Album && record.album_track.is_none() {
            holds.release(&key);
            match &target {
                AcquisitionTarget::Member(user) => {
                    self.acquire_album_for_member(
                        &record.provider,
                        &record.external_id,
                        requested_by.as_deref(),
                        user,
                        false,
                        origin,
                    )
                    .await;
                }
                AcquisitionTarget::Shared => {
                    if let Err(e) = self
                        .acquire_album_as(
                            &record.provider,
                            &record.external_id,
                            requested_by.as_deref(),
                            None,
                            origin,
                        )
                        .await
                    {
                        warn!("Resumed {key} failed: {e}");
                    }
                }
            }
            if let Some(tracker) = self.tracker() {
                tracker.release_row(&record.provider, &record.external_id, &holds);
            }
            return;
        }
        let mut filed: Vec<(String, String)> = Vec::new();
        // Whose chain ended held, "" for a hold that lists nobody.
        let mut held_again: Vec<String> = Vec::new();
        match record.kind {
            HeldKind::Track if record.holders.len() > 1 => {
                // A download that carries a sound check belongs to one person: each holder runs
                // their own chain, with their own gate, into their own library, every one of them
                // judging the files the hold kept, which stay held for the others until it ends.
                let held = HeldFiles {
                    files: record.held_files.clone(),
                    lidarr: record.lidarr_files.clone(),
                    shared: true,
                };
                let chains = record.holders.iter().map(|holder| {
                    let held = held.clone();
                    async move {
                        let user = holder.user.trim().to_string();
                        let target = self.import_target(&user).unwrap_or(AcquisitionTarget::Shared);
                        let mut filed = Vec::new();
                        let ended = self
                            .track_chain(
                                &record.provider,
                                &record.external_id,
                                Some(&user),
                                None,
                                &target,
                                route_origin(holder, record),
                                vec![holder.clone()],
                                held,
                                &mut filed,
                            )
                            .await;
                        (user, ended, filed)
                    }
                });
                for (user, ended, mut theirs) in futures::future::join_all(chains).await {
                    match ended {
                        Ok(TrackOutcome::Held(_)) => held_again.push(user),
                        Err(e) => warn!("Resumed {key} failed for {user}: {e}"),
                        Ok(_) => {}
                    }
                    filed.append(&mut theirs);
                }
            }
            HeldKind::Track => {
                let origin = record
                    .holders
                    .first()
                    .map_or(origin, |holder| route_origin(holder, record));
                let ended = self
                    .track_chain(
                        &record.provider,
                        &record.external_id,
                        requested_by.as_deref(),
                        None,
                        &target,
                        origin,
                        record.holders.clone(),
                        HeldFiles::of(record.held_files.clone(), record.lidarr_files.clone()),
                        &mut filed,
                    )
                    .await;
                match ended {
                    Ok(TrackOutcome::Held(_)) => {
                        held_again.extend(record.holders.iter().map(|holder| holder.user.trim().to_string()));
                        held_again.push(String::new());
                    }
                    Err(e) => warn!("Resumed {key} failed: {e}"),
                    Ok(_) => {}
                }
            }
            HeldKind::Album => {
                let ended = self
                    .acquire_album_as(
                        &record.provider,
                        &record.external_id,
                        requested_by.as_deref(),
                        None,
                        origin,
                    )
                    .await;
                if let Err(e) = ended {
                    warn!("Resumed {key} failed: {e}");
                }
            }
        }
        // A holder filed again under this very hold stays; one filed under another wait has moved
        // there; one their chain held without filing anywhere (an origin that never files that
        // wait) stays where it was. Everyone else's acquisition ended, and they leave it.
        let filed_under = |user: &str| {
            filed
                .iter()
                .filter(|(who, _)| eq_ignore_case(who, user.trim()))
                .map(|(_, filed_key)| filed_key.as_str())
                .collect::<Vec<&str>>()
        };
        let stays = |user: &str| {
            let keys = filed_under(user);
            let held_again = held_again.iter().any(|who| eq_ignore_case(who, user.trim()));
            keys.contains(&key.as_str()) || keys.contains(&STAYS) || (held_again && keys.is_empty())
        };
        if record.holders.is_empty() {
            if !stays("") {
                holds.release(&key);
            }
        } else {
            for holder in record.holders.iter().filter(|holder| !stays(&holder.user)) {
                holds.release_holder(&key, &holder.user);
                self.let_go_of_copies(&record.provider, &record.external_id, holder);
            }
        }
        match holds.get(&key) {
            None => {
                if let Some(tracker) = self.tracker() {
                    tracker.release_row(&record.provider, &record.external_id, &holds);
                }
            }
            // Someone still waits here (a file landed only for the others): the row says so, held
            // again with this hold's line, even when the landing closed it as done.
            Some(still) => self.hold_row_for(&still),
        }
        // A hearted album's track that ran its chain: the album's row counts what still waits.
        if let Some((provider, album_id)) = record
            .album_track
            .as_ref()
            .and_then(|track| track.album_key.split_once(':'))
        {
            self.album_row(provider, album_id);
        }
    }

    /// A hearted album's held track whose held files are all gone, that someone is in by their
    /// heart of the song: the album's hearts leave it (a held track with no file ends for them),
    /// and it goes on as the song's own hold for the rest, its album row counted again. None when
    /// it is no album track, held no file, holds one still, or nobody is in it by the song.
    fn spent_track_as_song_hold(&self, record: &HeldAcquisition) -> Option<HeldAcquisition> {
        use super::soulseek_hold_store::HoldRoute;
        let holds = self.extras.holds.as_ref()?;
        let track = record.album_track.clone()?;
        let by_song = |holder: &Holder| by_song(holder, record);
        if record.kind != HeldKind::Track
            || record.held_files.is_empty()
            || record
                .held_files
                .iter()
                .any(|file| std::path::Path::new(file).is_file())
            || !record.holders.iter().any(by_song)
        {
            return None;
        }
        let leaving: Vec<Holder> = record
            .holders
            .iter()
            .filter(|holder| !by_song(holder))
            .cloned()
            .collect();
        let song = holds.update(&record.key(), |kept| {
            let own = HoldRoute::of(kept);
            kept.holders.retain(|holder| {
                holder.routes.contains(&HoldRoute::Song)
                    || (holder.routes.is_empty() && own == HoldRoute::Song)
            });
            as_song_hold(kept);
        })?;
        for holder in &leaving {
            self.let_go_of_copies(&record.provider, &record.external_id, holder);
        }
        info!(
            "Held {} has no file left for its album; it goes on as the song's own hold",
            record.key()
        );
        if let Some((provider, album_id)) = track.album_key.split_once(':') {
            self.album_row(provider, album_id);
        }
        Some(song)
    }

    /// Checks a hearted album's held track again by its own held files alone, fetching nothing:
    /// each holder's gate, built from what they heard of it (never from the held file itself) as
    /// the heart that keeps them in it is: an album heart's judges each file by the album's rule, a
    /// song heart's by the song's own. A file a gate accepts lands, through the queue, for those
    /// whose gate accepts it; a file every gate refuses (another recording, or not what a person
    /// played) is deleted; anything else stays held as it is. Once a file an album heart accepts
    /// lands, or no file is left, the album's hearts are done with the track, and anyone still in
    /// it by their heart of the song goes on with the song's own hold.
    async fn recheck_album_track(&self, record: &HeldAcquisition) {
        let (Some(holds), Some(track)) = (self.extras.holds.clone(), record.album_track.clone()) else {
            return;
        };
        let key = record.key();
        let mut files: Vec<String> = record
            .held_files
            .iter()
            .filter(|file| std::path::Path::new(file).is_file())
            .cloned()
            .collect();
        if !files.is_empty()
            && let Some(builder) = self.extras.gates.clone()
        {
            // Each ready gate, and whether it judges by the album's rule.
            let mut gates: Vec<(HeartGate, bool)> = Vec::new();
            for holder in &record.holders {
                let album = by_album(holder, record);
                let mut holder = holder.clone();
                let heard_held = holder.heard.as_ref().is_some_and(|heard| match &heard.source {
                    HeardSource::Library { path } | HeardSource::Acquired { path } => {
                        record.held_files.contains(path)
                    }
                    _ => false,
                });
                if heard_held {
                    holder.heard = None;
                }
                let start = if album {
                    builder
                        .build_for_track(
                            &record.provider,
                            &record.external_id,
                            &holder,
                            AcquisitionOrigin::AlbumHeart,
                            &track.r,
                        )
                        .await
                } else {
                    builder
                        .build(
                            &record.provider,
                            &record.external_id,
                            &holder,
                            AcquisitionOrigin::Heart,
                        )
                        .await
                };
                match start {
                    GateStart::Ready(gate) => gates.push((gate, album)),
                    // What they heard cannot be read yet: their held track waits as it is, and is
                    // checked again with what they heard once that answers.
                    GateStart::Hold {
                        waiting_for,
                        question,
                        ..
                    } => {
                        debug!(
                            "{} of held {key} waits for {} before it is checked again",
                            holder.user,
                            waiting_for.name()
                        );
                        if waiting_for == WaitingFor::Deezer {
                            self.deezer_waits.lock().insert(key.clone());
                            self.deezer_filed(&key);
                        } else {
                            builder.wake_for(
                                &key,
                                waiting_for,
                                &holder.user,
                                holder.heard.as_ref(),
                                question,
                            );
                        }
                    }
                    GateStart::End { .. } => {}
                }
            }
            if gates.is_empty() {
                return;
            }
            let album_gates: Vec<HeartGate> = gates
                .iter()
                .filter(|(_, album)| *album)
                .map(|(gate, _)| gate.clone())
                .collect();
            let song_gates: Vec<HeartGate> = gates
                .iter()
                .filter(|(_, album)| !*album)
                .map(|(gate, _)| gate.clone())
                .collect();
            let every: Vec<HeartGate> = gates.into_iter().map(|(gate, _)| gate).collect();
            // One heard-master round per person for this check: the copy of what they heard that
            // judges the held file here also serves its landing.
            let acquisition = super::acquisition_key(&record.provider, &record.external_id, false);
            let _rounds = builder.open_rounds(&acquisition, &every);
            for file in files.clone() {
                // A file Lidarr brought is judged by the labels of its download.
                let labels = record
                    .lidarr_files
                    .iter()
                    .find(|brought| brought.path == file)
                    .map_or(LabelSource::TheFile, HeldLidarrFile::labels);
                let mut verdicts: Vec<(HeartGate, GateVerdict)> = Vec::new();
                if !album_gates.is_empty() {
                    let judged = builder
                        .judge_held(&file, labels.clone(), &acquisition, &album_gates)
                        .await;
                    verdicts.extend(album_gates.iter().cloned().zip(judged));
                }
                if !song_gates.is_empty() {
                    let judged = builder
                        .judge_landed(&file, labels, &acquisition, &song_gates)
                        .await;
                    verdicts.extend(song_gates.iter().cloned().zip(judged));
                }
                if verdicts
                    .iter()
                    .any(|(_, verdict)| matches!(verdict, GateVerdict::Accept { .. }))
                {
                    // The landing waits in the queue, which never keeps a heard-copy place.
                    builder.pause_rounds(&acquisition);
                    self.land_album_track(record, &verdicts, &file).await;
                    return;
                }
                if !verdicts.is_empty()
                    && verdicts
                        .iter()
                        .all(|(_, verdict)| matches!(verdict, GateVerdict::Refuse(_)))
                {
                    info!("The held file {file} of {key} is not its track's recording; deleted");
                    let _ = std::fs::remove_file(&file);
                    files.retain(|kept| kept != &file);
                    holds.update(&key, |kept| kept.held_files.retain(|held| held != &file));
                }
            }
        }
        if files.is_empty() {
            info!("Held {key} has no file left; the album's hearts are done with it");
            self.album_track_over(record, &[], true);
        }
    }

    /// A held album track's file that the gates of `verdicts` accept lands for the people whose
    /// gate accepts it: through the queue for the first of them, by their own gate (its download
    /// checks the held file first and fetches nothing else), into their own library, and the
    /// others' libraries then take the same file. The album's hearts are done with the track once a
    /// file one of them accepts lands; a song heart that accepted it has it; everyone else in it
    /// waits on as before.
    async fn land_album_track(
        &self,
        record: &HeldAcquisition,
        verdicts: &[(HeartGate, GateVerdict)],
        file: &str,
    ) {
        let accepting: Vec<&HeartGate> = verdicts
            .iter()
            .filter(|(_, verdict)| matches!(verdict, GateVerdict::Accept { .. }))
            .map(|(gate, _)| gate)
            .collect();
        let Some(first) = accepting.first() else {
            return;
        };
        let target_of = |user: &str| self.import_target(user).unwrap_or(AcquisitionTarget::Shared);
        let completion = self.direct_queue.enqueue_holding(
            // A held album track's landing: nothing but the held file is fetched for it.
            Some(AcquisitionOrigin::AlbumHeart),
            vec![(*first).clone()],
            HeldFiles::of(
                vec![file.to_string()],
                record
                    .lidarr_files
                    .iter()
                    .filter(|brought| brought.path == file)
                    .cloned()
                    .collect(),
            ),
            &target_of(&first.user),
            false,
            &record.provider,
            &record.external_id,
            true,
            false,
            true,
            Some(DownloadSource::Soulseek),
            true,
            Some(first.user.trim()),
            false,
            None,
        );
        match completion.wait().await {
            Ok(path) => {
                info!("Held {} is proven; it lands at {path}", record.key());
                for gate in accepting.iter().skip(1) {
                    self.credit_landed(
                        &record.provider,
                        &record.external_id,
                        &path,
                        &target_of(&gate.user),
                    )
                    .await;
                }
                let proofs = super::GateLedger::default();
                for (gate, verdict) in verdicts {
                    crate::services::soulseek::soulseek_download_service::record_joiner(
                        &proofs, gate, verdict,
                    );
                }
                self.after_proofs(AcquisitionOrigin::AlbumHeart, &record.holders, &proofs);
                self.after_proofs(AcquisitionOrigin::Heart, &record.holders, &proofs);
                let landed_for: Vec<String> = accepting
                    .iter()
                    .map(|gate| gate.user.trim().to_string())
                    .collect();
                let album_done = accepting
                    .iter()
                    .any(|gate| gate.origin == AcquisitionOrigin::AlbumHeart);
                self.album_track_over(record, &landed_for, album_done);
            }
            Err(failure) if failure.downcast_ref::<AcquisitionHeld>().is_some() => {
                debug!("Held {} waits again: {failure}", record.key());
            }
            Err(failure) => {
                info!("Held {} failed its checks again: {failure}", record.key());
                if !std::path::Path::new(file).is_file() {
                    let left: Vec<String> = record
                        .held_files
                        .iter()
                        .filter(|held| *held != file && std::path::Path::new(held).is_file())
                        .cloned()
                        .collect();
                    if left.is_empty() {
                        self.album_track_over(record, &[], true);
                    }
                }
            }
        }
    }

    /// A held album track's hold ends for the people of `landed_for`, whose file landed, and, when
    /// the album is done with the track (`album_done`: a file one of its hearts accepted landed, or
    /// no file is left), for everyone in it by their heart of the album alone. Anyone left in it by
    /// their heart of the song alone goes on with the song's own hold, run again; while an album
    /// heart is still in it, it stays the album's held track, without the files that are gone, and
    /// is checked again. The rows say so.
    pub(crate) fn album_track_over(&self, record: &HeldAcquisition, landed_for: &[String], album_done: bool) {
        let Some(holds) = self.extras.holds.clone() else {
            return;
        };
        let key = record.key();
        let current = holds.get(&key).unwrap_or_else(|| record.clone());
        let leaves = |holder: &Holder| {
            landed_for
                .iter()
                .any(|user| eq_ignore_case(user.trim(), holder.user.trim()))
                || (album_done && !by_song(holder, &current))
        };
        let leaving: Vec<Holder> = current
            .holders
            .iter()
            .filter(|holder| leaves(holder))
            .cloned()
            .collect();
        let staying: Vec<Holder> = current
            .holders
            .iter()
            .filter(|holder| !leaves(holder))
            .cloned()
            .collect();
        let album_left = staying.iter().any(|holder| by_album(holder, &current)) && !album_done;
        if staying.is_empty() {
            self.end_album_track(record);
        } else {
            let song_only = !album_left;
            holds.update(&key, |kept| {
                kept.holders.retain(|holder| !leaves(holder));
                kept.held_files
                    .retain(|file| std::path::Path::new(file).is_file());
                if song_only {
                    as_song_hold(kept);
                }
            });
            if song_only {
                info!("Held {key} is done for its album; it goes on as the song's own hold");
            }
            if let Some((provider, album_id)) = record
                .album_track
                .as_ref()
                .and_then(|track| track.album_key.split_once(':'))
            {
                self.album_row(provider, album_id);
            }
            if let Some(still) = holds.get(&key) {
                self.hold_row_for(&still);
            }
            if let Some(me) = self.me.upgrade() {
                me.resume_held(&key);
            }
        }
        for holder in &leaving {
            self.let_go_of_copies(&record.provider, &record.external_id, holder);
        }
    }

    /// A file that landed on another request of the song counts as `target`'s too: it is linked
    /// into their library as the worker links a member who asked (a member's own library, or the
    /// shared library, which takes the file in when it wants it). Answers where the file is now.
    async fn credit_landed(
        &self,
        provider: &str,
        external_id: &str,
        path: &str,
        target: &AcquisitionTarget,
    ) -> String {
        use crate::services::family::FamilyPlacement;
        let Some(family) = self.family.get() else {
            return path.to_string();
        };
        let members: Vec<String> = target.member().map(str::to_string).into_iter().collect();
        family
            .pool
            .settle(
                path,
                &members,
                *target == AcquisitionTarget::Shared,
                Some(&key_of(provider, external_id)),
            )
            .await
    }

    /// A held album track ends: its record goes, its row and its album's row with it.
    fn end_album_track(&self, record: &HeldAcquisition) {
        let Some(holds) = self.extras.holds.as_ref() else {
            return;
        };
        holds.release(&record.key());
        if let Some(tracker) = self.tracker() {
            tracker.release_row(&record.provider, &record.external_id, holds);
        }
        if let Some((provider, album_id)) = record
            .album_track
            .as_ref()
            .and_then(|track| track.album_key.split_once(':'))
        {
            self.album_row(provider, album_id);
        }
    }

    /// `user` took their heart of the song back: they leave every hold of the song it keeps them
    /// in, whatever it waits for (a hearted album's held track keeps them while their heart of the
    /// album does), and each hold nobody is left in ends, its files with it. Once nothing of the
    /// song is held or fetched, their row lets them go. How many holds they left.
    pub fn leave_holds(&self, provider: &str, external_id: &str, user: &str) -> usize {
        self.lidarr.unhearted(provider, external_id, user);
        let Some(holds) = self.extras.holds.as_ref() else {
            return 0;
        };
        let mut left = 0;
        for record in holds.records_for(&track_base_key(provider, external_id)) {
            if self.leave_by_route(&record, user, super::soulseek_hold_store::HoldRoute::Song) {
                left += 1;
            }
        }
        if left > 0 {
            self.let_row_go(provider, external_id, user);
        }
        left
    }

    /// `user` leaves `record` by the heart of `route` they took back: off it when no other heart of
    /// theirs keeps them in it, else only that route goes. False when it does not hold them by it.
    fn leave_by_route(
        &self,
        record: &HeldAcquisition,
        user: &str,
        route: super::soulseek_hold_store::HoldRoute,
    ) -> bool {
        let Some(holds) = self.extras.holds.as_ref() else {
            return false;
        };
        let Some(holder) = record
            .holders
            .iter()
            .find(|holder| eq_ignore_case(holder.user.trim(), user.trim()))
            .cloned()
        else {
            return false;
        };
        let routes = holder.routes_in(record);
        if !routes.contains(&route) {
            return false;
        }
        if routes.iter().any(|other| *other != route) {
            info!(
                "{user} took back one heart on held {}; another of theirs keeps them in it",
                record.key()
            );
            holds.update(&record.key(), |kept| {
                if let Some(entry) = kept
                    .holders
                    .iter_mut()
                    .find(|entry| eq_ignore_case(entry.user.trim(), user.trim()))
                {
                    entry.routes = routes.iter().copied().filter(|other| *other != route).collect();
                }
            });
            return true;
        }
        if holds.release_holder(&record.key(), user).is_none() {
            info!(
                "{user} took back the last heart on held {}; it ends",
                record.key()
            );
        }
        self.let_go_of_copies(&record.provider, &record.external_id, &holder);
        true
    }

    /// `user` took a heart back: once no hold of the song is left and nothing fetches it, its row
    /// stops showing a search and lets them go, and goes once nobody is left on it.
    fn let_row_go(&self, provider: &str, external_id: &str, user: &str) {
        let (Some(tracker), Some(holds)) = (self.tracker(), self.extras.holds.as_ref()) else {
            return;
        };
        tracker.release_row(provider, external_id, holds);
        if holds
            .records_for(&track_base_key(provider, external_id))
            .is_empty()
            && !self.direct_queue.is_in_flight(provider, external_id)
        {
            tracker.leave_row(provider, external_id, user);
        }
    }

    /// Octo's start: every file in a hold's folder of the one hold path that no hold names goes, a
    /// hold that ended before its files did, or a crash between.
    pub(crate) fn sweep_held_files(&self) {
        let (Some(holds), Some(root)) = (self.extras.holds.as_ref(), self.direct_downloads.held_root())
        else {
            return;
        };
        let swept = holds.sweep_unnamed(&root);
        if swept > 0 {
            info!("Removed {swept} held files no hold names");
        }
    }

    /// Holds what `holders` heard of the song while a heart chain (`play` false) or a play runs,
    /// each copy under an id of this run's own: two chains of one person and song each hold theirs.
    /// None without heard samples, or when nobody heard anything.
    fn hold_while_running(
        &self,
        provider: &str,
        external_id: &str,
        holders: &[Holder],
        play: bool,
    ) -> Option<HeldWhileRunning> {
        let heard = self.extras.heard.clone()?;
        let heard_by: Vec<(&str, &HeardRecord)> = holders
            .iter()
            .filter_map(|holder| holder.heard.as_ref().map(|record| (holder.user.as_str(), record)))
            .collect();
        heard.hold_for_run(&track_base_key(provider, external_id), &heard_by, play)
    }

    /// A holder of a held song keeps the copy of what they heard under their own holder id, so
    /// nobody else's release frees it.
    fn hold_copies(&self, record: &HeldAcquisition) {
        let Some(heard) = &self.extras.heard else {
            return;
        };
        for holder in &record.holders {
            if let Some(source) = holder.heard.as_ref().map(|h| &h.source) {
                heard.hold_heard(source, &record.copy_holder_id(&holder.user));
            }
        }
    }

    /// `holder` left a hold of the song: once no hold of it lists them, their copy is let go.
    fn let_go_of_copies(&self, provider: &str, external_id: &str, holder: &Holder) {
        let (Some(heard), Some(holds)) = (&self.extras.heard, &self.extras.holds) else {
            return;
        };
        let Some(source) = holder.heard.as_ref().map(|h| &h.source) else {
            return;
        };
        let records = holds.records_for(&track_base_key(provider, external_id));
        let still = records.iter().any(|record| {
            record
                .holders
                .iter()
                .any(|other| eq_ignore_case(other.user.trim(), holder.user.trim()))
        });
        if !still {
            let id = HeldAcquisition {
                kind: HeldKind::Track,
                provider: provider.to_string(),
                external_id: external_id.to_string(),
                ..Default::default()
            }
            .copy_holder_id(&holder.user);
            heard.release_heard(source, &id);
        }
    }

    /// Opens again the downloads row of a held song, as a restart left none: named from the
    /// registry, owned by its holders, held, with its saved line.
    pub(crate) fn hold_row_for(&self, record: &HeldAcquisition) {
        let (Some(tracker), Some(waiting)) = (self.tracker(), record.waiting_for) else {
            return;
        };
        if record.kind != HeldKind::Track {
            // A hearted album held before any of its tracks was fetched: its own row.
            let owners: Vec<String> = record.holders.iter().map(|holder| holder.user.clone()).collect();
            let line = record
                .line
                .clone()
                .unwrap_or_else(|| waiting_line(waiting).to_string());
            tracker.hold_album_row(&record.provider, &record.external_id, &owners, &line, false);
            return;
        }
        let routing = self
            .extras
            .id_registry
            .as_ref()
            .and_then(|ids| ids.resolve_song(&record.external_id, false))
            .map(|shared| shared.snapshot());
        tracker.begin(
            &record.provider,
            &record.external_id,
            None,
            record.requested_by.as_deref(),
            routing.as_ref().and_then(|r| r.artist.as_deref()),
            routing.as_ref().and_then(|r| r.title.as_deref()),
            routing.as_ref().and_then(|r| r.album.as_deref()),
        );
        let owners: Vec<String> = record.holders.iter().map(|holder| holder.user.clone()).collect();
        let line = record
            .line
            .clone()
            .unwrap_or_else(|| waiting_line(waiting).to_string());
        tracker.hold_row(
            &record.provider,
            &record.external_id,
            &owners,
            &line,
            record.keeps_anyway(),
        );
        if let Some((provider, album_id)) = record
            .album_track
            .as_ref()
            .and_then(|track| track.album_key.split_once(':'))
        {
            self.album_row(provider, album_id);
        }
    }

    /// The song chain of a heart, into the shared library. `Err` for a Lidarr step that failed.
    #[cfg(test)]
    pub(crate) async fn acquire_track(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        self.acquire_track_for(
            provider,
            external_id,
            requested_by,
            held_since_utc,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
    }

    /// [`Self::acquire_track`] into one library, run for `origin`. An import whose identity
    /// MusicBrainz could not answer comes back as [`TrickleCheckLater`], so the trickle puts it
    /// back and starts it again on its next turn.
    pub(crate) async fn acquire_track_for(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
        target: &AcquisitionTarget,
        origin: AcquisitionOrigin,
    ) -> anyhow::Result<()> {
        let outcome = self
            .acquire_track_outcome(
                provider,
                external_id,
                requested_by,
                held_since_utc,
                target,
                origin,
            )
            .await?;
        if origin == AcquisitionOrigin::Import
            && let TrackOutcome::Held(waiting) = outcome
            && trickle_waits(waiting)
        {
            return Err(TrickleCheckLater {
                detail: match waiting {
                    WaitingFor::MusicBrainz => WAITING_FOR_ANSWER.to_string(),
                    other => waiting_line(other).to_string(),
                },
                turn_back: true,
            }
            .into());
        }
        Ok(())
    }

    /// The song chain into one library, run for `origin`, answering how it ended. A member's
    /// chain skips Lidarr, which fills the shared library. A heart of a song already held for an
    /// answer, a proof or the person joins that hold, which is asked again at once.
    pub async fn acquire_track_outcome(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
        target: &AcquisitionTarget,
        origin: AcquisitionOrigin,
    ) -> anyhow::Result<TrackOutcome> {
        // A heart of a song is one of the events its kept-as-heard files are checked again on, and
        // a Lidarr follow of it reads whether its download moved.
        if matches!(origin, AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart) {
            self.lidarr.song_event(provider, external_id);
        }
        if matches!(origin, AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart)
            && let Some(kept) = self.extras.kept.clone()
        {
            let (provider, external_id) = (provider.to_string(), external_id.to_string());
            tokio::spawn(async move { kept.song_event(&provider, &external_id).await });
        }
        if let Some(waiting) = self.join_hold(provider, external_id, requested_by, origin) {
            return Ok(TrackOutcome::Held(waiting));
        }
        let holders = vec![self.holder_for(external_id, requested_by)];
        self.track_chain(
            provider,
            external_id,
            requested_by,
            held_since_utc,
            target,
            origin,
            holders,
            HeldFiles::default(),
            &mut Vec::new(),
        )
        .await
    }

    /// Each heart AcoustID's clear reading accepted the landed file for: the kept file checked
    /// longest ago is checked again next, once per such heart, so checks follow real proofs. A file
    /// kept as the very audio heard, or as the master heard, starts none.
    fn after_proofs(&self, origin: AcquisitionOrigin, holders: &[Holder], ledger: &super::GateLedger) {
        if !matches!(origin, AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart) {
            return;
        }
        let Some(kept) = self.extras.kept.clone() else {
            return;
        };
        let proofs = holders
            .iter()
            .filter(|holder| ledger.acceptance_for(&holder.user, origin) == Some(Acceptance::Sound))
            .count();
        if proofs == 0 {
            return;
        }
        tokio::spawn(async move {
            for _ in 0..proofs {
                kept.after_a_proof().await;
            }
        });
    }

    /// Holds `users`' heart of a song "not yet proven" with `line`, joining a hold already there,
    /// its row held with that line: the copy kept for them turned out to be another song, so only
    /// a new play of the song can prove one. Each holder carries what they last heard of it.
    pub fn hold_not_yet_proven(&self, provider: &str, external_id: &str, users: &[String], line: &str) {
        let holders: Vec<Holder> = users
            .iter()
            .map(|user| user.trim())
            .filter(|user| !user.is_empty())
            .map(|user| self.holder_for(external_id, Some(user)))
            .collect();
        let Some(first) = holders.first().map(|holder| holder.user.clone()) else {
            return;
        };
        if let Some(tracker) = self.tracker()
            && !tracker.is_held(provider, external_id)
        {
            let routing = self
                .extras
                .id_registry
                .as_ref()
                .and_then(|ids| ids.resolve_song(external_id, false))
                .map(|shared| shared.snapshot());
            tracker.begin(
                provider,
                external_id,
                None,
                Some(&first),
                routing.as_ref().and_then(|r| r.artist.as_deref()),
                routing.as_ref().and_then(|r| r.title.as_deref()),
                routing.as_ref().and_then(|r| r.album.as_deref()),
            );
        }
        self.file_hold(
            provider,
            external_id,
            Some(&first),
            AcquisitionOrigin::Heart,
            WaitingFor::NotYetProven,
            holders,
            line,
            None,
            None,
            &mut Vec::new(),
        );
    }

    /// One person's place in a heart's gate: who they are and what they heard of the song.
    fn holder_for(&self, external_id: &str, requested_by: Option<&str>) -> Holder {
        let user = requested_by.map(str::trim).unwrap_or_default().to_string();
        let heard = self
            .extras
            .id_registry
            .as_ref()
            .filter(|_| !user.is_empty())
            .and_then(|ids| ids.heard_by(external_id, &user));
        Holder {
            user,
            heard,
            ..Default::default()
        }
    }

    /// A heart of a song already held for something other than a Soulseek outage: the person
    /// joins the hold (with what they heard) and it is asked again at once. The wait it joined,
    /// or None when nothing of the song is held so.
    fn join_hold(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
    ) -> Option<WaitingFor> {
        if !matches!(origin, AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart) {
            return None;
        }
        let holds = self.extras.holds.as_ref()?;
        // Only a heart's hold: a record runs for one origin, and an import's is judged as one.
        let record = holds
            .records_for(&track_base_key(provider, external_id))
            .into_iter()
            .find(|record| {
                record.waiting_for.is_some()
                    && matches!(
                        record.origin(),
                        AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart
                    )
            })?;
        let waiting = record.waiting_for?;
        let mut holder = self.holder_for(external_id, requested_by);
        let key = record.key();
        if !holder.user.is_empty() {
            // Only the person joins, by the heart they joined with: the hold as it is now keeps
            // everything else it has. An album heart joining the song's own hold that holds a file
            // brings the album's track, so taking either heart back leaves the other; one holding
            // no file stays the song's hold, the person in it by their album heart with the album
            // named, so its walk's track is fetched for them when it runs.
            let album_heart = origin == AcquisitionOrigin::AlbumHeart;
            holder.routes = vec![if album_heart {
                super::soulseek_hold_store::HoldRoute::Album
            } else {
                super::soulseek_hold_store::HoldRoute::Song
            }];
            let holds_a_file = record
                .held_files
                .iter()
                .any(|file| std::path::Path::new(file).is_file());
            let theirs = album_heart
                .then(|| self.album_track_of(provider, external_id, &holder.user))
                .flatten();
            let album_track = match &record.album_track {
                Some(track) if album_heart => Some(track.clone()),
                Some(_) => None,
                None if holds_a_file => theirs.clone(),
                None => None,
            };
            if record.album_track.is_none()
                && album_track.is_none()
                && let Some((_, album_id)) = theirs.as_ref().and_then(|track| track.album_key.split_once(':'))
            {
                holder.context = Some(octo_core::fingerprint::heart_gate::AlbumContext {
                    user: holder.user.clone(),
                    catalog_album_id: album_id.to_string(),
                    listed_at: Utc::now(),
                    ..Default::default()
                });
            }
            let joined = holds.hold(HeldAcquisition {
                kind: record.kind,
                provider: record.provider.clone(),
                external_id: record.external_id.clone(),
                requested_by: record.requested_by.clone(),
                held_since_utc: record.held_since_utc,
                origin: record.origin,
                waiting_for: record.waiting_for,
                holders: vec![holder],
                album_track,
                ..Default::default()
            });
            self.hold_copies(&joined);
        }
        info!(
            "{provider}:{external_id} is held ({}); the heart joins it and asks again",
            waiting.name()
        );
        // A heart of a song that waits on the shim is one of the shim's round events.
        if waiting == WaitingFor::Shim
            && let Some(heard) = &self.extras.heard
        {
            heard.shim_event();
            return Some(waiting);
        }
        if let Some(me) = self.me.upgrade() {
            me.resume_held(&key);
        }
        Some(waiting)
    }

    /// Each holder's gate, built before any source runs. A holder whose gate holds is filed under
    /// that wait (`filed` says where); one whose heart ends leaves. Err when nobody's gate is
    /// ready: the first wait, else the line the hearts ended with.
    async fn gate_up(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        holders: &[Holder],
        filed: &mut Vec<(String, String)>,
    ) -> Result<Vec<HeartGate>, GateStop> {
        let Some(builder) = self.extras.gates.clone() else {
            return Ok(Vec::new());
        };
        let mut gates = Vec::new();
        let mut held = None;
        let mut ended = None;
        for holder in holders {
            // A hearted album's or a family album request's track carries the album's gate: its
            // track's own recording, what the holder heard of it read against that.
            let album = matches!(origin, AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Request)
                .then(|| {
                    self.extras
                        .album_gates
                        .as_ref()
                        .and_then(|gates| gates.for_track(provider, external_id, &holder.user))
                })
                .flatten();
            let start = match &album {
                Some((_, gate, track)) if holder.heard.is_none() => GateStart::Ready(gate.gate_for(track)),
                Some((_, _, track)) => {
                    builder
                        .build_for_track(provider, external_id, holder, origin, &track.r)
                        .await
                }
                None => builder.build(provider, external_id, holder, origin).await,
            };
            match start {
                GateStart::Ready(gate) => gates.push(gate),
                GateStart::Hold {
                    waiting_for,
                    line,
                    question,
                    ..
                } => {
                    held.get_or_insert(waiting_for);
                    self.file_hold(
                        provider,
                        external_id,
                        requested_by,
                        origin,
                        waiting_for,
                        vec![holder.clone()],
                        &line,
                        None,
                        question,
                        filed,
                    );
                }
                GateStart::End { line, .. } => {
                    info!(
                        "{provider}:{external_id} can never be proven for {}: {line}",
                        holder.user
                    );
                    ended.get_or_insert(line);
                }
            }
        }
        if !gates.is_empty() {
            return Ok(gates);
        }
        match (held, ended) {
            (Some(waiting), _) => Err(GateStop::Held(waiting)),
            (None, Some(line)) => Err(GateStop::Ended(line)),
            (None, None) => Ok(gates),
        }
    }

    /// Files `holders` in a hold of this song waiting for `waiting_for`, joining one already
    /// there, with the files a download held (`held`), the labels of those Lidarr brought saved
    /// beside them in the same write; holds its downloads row with `line`; and, for a MusicBrainz
    /// question, asks it again until it answers, which resumes the hold. Nothing for an origin
    /// that never holds so.
    #[allow(clippy::too_many_arguments)] // the song, who holds, what for, and where it went
    fn file_hold(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        waiting_for: WaitingFor,
        holders: Vec<Holder>,
        line: &str,
        held: Option<&AcquisitionHeld>,
        question: Option<String>,
        filed: &mut Vec<(String, String)>,
    ) {
        if !files_hold(origin, waiting_for) {
            return;
        }
        let Some(holds) = self.extras.holds.as_ref() else {
            return;
        };
        let requested_by = requested_by
            .map(str::to_string)
            .or_else(|| holders.first().map(|holder| holder.user.clone()))
            .filter(|user| !user.trim().is_empty());
        let users: Vec<String> = holders
            .iter()
            .map(|holder| holder.user.trim().to_string())
            .collect();
        // A hearted album's track is held as that album's, under the album's row.
        let album_track = if origin == AcquisitionOrigin::AlbumHeart {
            users
                .iter()
                .find_map(|user| self.album_track_of(provider, external_id, user))
        } else {
            None
        };
        let album_line;
        let line = match &album_track {
            Some(_) if waiting_for == WaitingFor::NotYetProven => {
                album_line = tracks_wait_line(1);
                album_line.as_str()
            }
            _ => line,
        };
        let album_key = album_track.as_ref().map(|track| track.album_key.clone());
        let mut record = HeldAcquisition {
            kind: HeldKind::Track,
            provider: provider.to_string(),
            external_id: external_id.to_string(),
            requested_by,
            held_since_utc: Utc::now(),
            origin: Some(origin),
            waiting_for: Some(waiting_for),
            holders,
            album_track,
            // The files, their labels and the line are written with the hold, in one write.
            held_files: held.map(|held| held.files.clone()).unwrap_or_default(),
            lidarr_files: held.map(|held| held.lidarr.clone()).unwrap_or_default(),
            line: Some(line.to_string()).filter(|line| !line.trim().is_empty()),
            ..Default::default()
        };
        // A song heart filed in a hearted album's held track keeps its person there by the song,
        // beside any album heart of theirs.
        if record.album_track.is_none()
            && holds
                .get(&record.key())
                .is_some_and(|existing| existing.album_track.is_some())
        {
            for holder in &mut record.holders {
                holder.routes = vec![super::soulseek_hold_store::HoldRoute::Song];
            }
        }
        let kept = holds.hold(record);
        let key = kept.key();
        self.hold_copies(&kept);
        if let Some(gates) = &self.extras.gates {
            for user in &users {
                gates.on_filed(&kept, user);
            }
        }
        filed.extend(users.into_iter().map(|user| (user, key.clone())));
        if let Some(tracker) = self.tracker() {
            let owners: Vec<String> = kept.holders.iter().map(|holder| holder.user.clone()).collect();
            tracker.hold_row(provider, external_id, &owners, line, kept.keeps_anyway());
        }
        if let Some((album_provider, album_id)) = album_key.as_deref().and_then(|key| key.split_once(':')) {
            self.album_row(album_provider, album_id);
        }
        info!("Holding {key}: {line}");
        if waiting_for == WaitingFor::MusicBrainz
            && let Some(question) = question
        {
            self.re_ask_for(&key, &question);
        }
        if waiting_for == WaitingFor::Deezer {
            self.deezer_filed(&key);
        }
    }

    /// Asks the MusicBrainz question a hold waits on again until it answers (or MusicBrainz
    /// gives up on it), then resumes the hold.
    fn re_ask_for(&self, key: &str, question: &str) {
        let Some(music_brainz) = self.extras.music_brainz.clone() else {
            return;
        };
        let coordinator = self.me.clone();
        let key = key.to_string();
        music_brainz.re_ask(
            question,
            Box::new(move |ended| {
                debug!("MusicBrainz {ended:?} the question {key} waited on");
                if let Some(coordinator) = coordinator.upgrade() {
                    if ended == crate::services::fingerprint::music_brainz_client::ReAsked::GaveUp {
                        coordinator.resume_after_gave_up(&key);
                    } else {
                        coordinator.resume_held(&key);
                    }
                }
            }),
        );
    }

    /// MusicBrainz gave up on the question a hold waited on: the hold runs again (or once more,
    /// when it runs now), and the last pass of that run says on the song's row that MusicBrainz
    /// could not answer for it when the heart ended with nothing.
    fn resume_after_gave_up(self: &Arc<Self>, key: &str) {
        let Some(record) = self.extras.holds.as_ref().and_then(|holds| holds.get(key)) else {
            return;
        };
        self.gave_up
            .lock()
            .insert(key.to_string(), (record.provider, record.external_id));
        self.resume_held(key);
    }

    /// The claimed run of the hold `key` ended its last pass: when MusicBrainz gave up on the
    /// question it waited on, its song's row says so if the heart ended with nothing (the hold is
    /// over and nothing of the song is held any more; a row whose file landed keeps its line).
    fn tell_gave_up(&self, key: &str) {
        let Some((provider, external_id)) = self.gave_up.lock().remove(key) else {
            return;
        };
        let holds = self.extras.holds.as_ref();
        let waits = holds
            .and_then(|holds| holds.get(key))
            .is_some_and(|record| record.waiting_for.is_some());
        let song = track_base_key(&provider, &external_id);
        let held = holds.is_some_and(|holds| !holds.records_for(&song).is_empty());
        if !waits
            && !held
            && let Some(tracker) = self.tracker()
        {
            tracker.ended_with_nothing(&provider, &external_id, MUSIC_BRAINZ_GAVE_UP);
        }
    }

    /// The download held instead of landing: the holders it holds for are filed under its wait
    /// with its files.
    fn hold_downloaded(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        holders: &[Holder],
        held: &AcquisitionHeld,
        filed: &mut Vec<(String, String)>,
    ) -> TrackOutcome {
        let holding: Vec<Holder> = holders
            .iter()
            .filter(|holder| {
                held.users.is_empty()
                    || held
                        .users
                        .iter()
                        .any(|user| eq_ignore_case(user.trim(), holder.user.trim()))
            })
            .cloned()
            .collect();
        self.file_hold(
            provider,
            external_id,
            requested_by,
            origin,
            held.waiting_for,
            holding,
            &held.line,
            Some(held),
            None,
            filed,
        );
        TrackOutcome::Held(held.waiting_for)
    }

    /// A file landed: what it is for each holder, by their own gate's verdict. A holder whose gate
    /// passed it has it; one whose gate holds is filed under that wait; one whose gate did not pass
    /// it, or never judged it (a download already running that they joined), never counts it as
    /// theirs, and stays where they were when this runs a hold again. Holders with no gate at all
    /// (`gated` false: the shipped checks ran for them) have it, as before.
    #[allow(clippy::too_many_arguments)] // the song, who held it, the verdicts and where they went
    fn landed_for(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        (holders, gated): (&[Holder], bool),
        ledger: &super::GateLedger,
        path: String,
        filed: &mut Vec<(String, String)>,
    ) -> TrackOutcome {
        if holders.is_empty() || (!gated && ledger.is_empty()) {
            return TrackOutcome::Landed(path);
        }
        self.after_proofs(origin, holders, ledger);
        let mut landed = false;
        let mut held = None;
        let mut why = None;
        for holder in holders {
            match ledger.outcome_for(&holder.user, origin) {
                Some(super::GateOutcome::Accepted) => landed = true,
                Some(super::GateOutcome::Held(waiting)) => {
                    held.get_or_insert(waiting);
                    self.file_hold(
                        provider,
                        external_id,
                        requested_by,
                        origin,
                        waiting,
                        vec![holder.clone()],
                        waiting_line(waiting),
                        None,
                        None,
                        filed,
                    );
                }
                outcome => {
                    let reason = match outcome {
                        Some(super::GateOutcome::NotAccepted(reason)) => reason,
                        _ => "Their own check never judged the copy that landed".to_string(),
                    };
                    info!(
                        "{provider}:{external_id} landed, and is not what {} heard: {reason}",
                        holder.user
                    );
                    filed.push((holder.user.trim().to_string(), STAYS.to_string()));
                    why.get_or_insert(reason);
                }
            }
        }
        match (landed, held, why) {
            (true, _, _) => TrackOutcome::Landed(path),
            (false, Some(waiting), _) => TrackOutcome::Held(waiting),
            (false, None, why) => TrackOutcome::Failed(why.unwrap_or_default()),
        }
    }

    /// A file another acquisition landed, which these holders joined and their own gates never
    /// judged: each such holder's gate judges it now, as a joiner's gate judges a landed file, and
    /// what they say is the chain's outcome when someone here has it or holds for it. A file one of
    /// them passes is linked into their library (`target`), as the worker links a joiner whose gate
    /// passed it. None when nobody was left unjudged, nothing could judge it, or it is nobody's
    /// here.
    #[allow(clippy::too_many_arguments)] // landed_for's facts and the gates that judge the file
    async fn judged_as_joiners(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        (origin, target): (AcquisitionOrigin, &AcquisitionTarget),
        (holders, gates): (&[Holder], &[HeartGate]),
        ledger: &super::GateLedger,
        path: &str,
        filed: &mut Vec<(String, String)>,
    ) -> Option<TrackOutcome> {
        let builder = self.extras.gates.clone()?;
        let unjudged: Vec<HeartGate> = gates
            .iter()
            .filter(|gate| {
                holders
                    .iter()
                    .any(|holder| eq_ignore_case(holder.user.trim(), gate.user.trim()))
                    && ledger.outcome_for(&gate.user, origin).is_none()
            })
            .cloned()
            .collect();
        if unjudged.is_empty() {
            return None;
        }
        let acquisition = super::acquisition_key(provider, external_id, false);
        let verdicts = builder
            .judge_landed(path, LabelSource::TheFile, &acquisition, &unjudged)
            .await;
        if verdicts.len() != unjudged.len() {
            return None;
        }
        for (gate, verdict) in unjudged.iter().zip(&verdicts) {
            info!(
                "{provider}:{external_id} landed for someone else; {}'s own gate says {verdict:?}",
                gate.user
            );
            crate::services::soulseek::soulseek_download_service::record_joiner(ledger, gate, verdict);
        }
        let mut path = path.to_string();
        if verdicts
            .iter()
            .any(|verdict| matches!(verdict, GateVerdict::Accept { .. }))
        {
            path = self.credit_landed(provider, external_id, &path, target).await;
        }
        match self.landed_for(
            provider,
            external_id,
            requested_by,
            origin,
            (holders, true),
            ledger,
            path,
            filed,
        ) {
            TrackOutcome::Failed(_) => None,
            outcome => Some(outcome),
        }
    }

    /// The song chain for these holders: their gates first, then the sources in order, every
    /// file judged with every ready gate. `held` is what a resumed hold held, with the labels of
    /// the files Lidarr brought: the first direct source checks them before it fetches anything.
    /// `filed` says which holders were filed under which hold.
    #[allow(clippy::too_many_arguments)] // the chain's facts, who holds it and where they went
    async fn track_chain(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
        target: &AcquisitionTarget,
        origin: AcquisitionOrigin,
        holders: Vec<Holder>,
        mut held: HeldFiles,
        filed: &mut Vec<(String, String)>,
    ) -> anyhow::Result<TrackOutcome> {
        // What each person heard is kept for as long as the chain runs, from before their gate is
        // built: no sweep takes the copy or the sample its checks compare with.
        let _heard = self.hold_while_running(provider, external_id, &holders, false);
        let scope = match target {
            AcquisitionTarget::Shared => LibraryScope::Shared,
            AcquisitionTarget::Member(user) => LibraryScope::Viewer(user.clone()),
        };
        // A heart's owned copy counts only when its own gate proves it is the version heard, so
        // its gate is built first; every other acquisition counts it as before.
        let judged = self.extras.gates.is_some()
            && matches!(origin, AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart);
        if !judged
            && self
                .already_yours(provider, external_id, requested_by, &scope)
                .await
        {
            return Ok(TrackOutcome::AlreadyThere);
        }
        let gates = match self
            .gate_up(provider, external_id, requested_by, origin, &holders, filed)
            .await
        {
            Ok(gates) => gates,
            Err(GateStop::Held(waiting)) => return Ok(TrackOutcome::Held(waiting)),
            Err(GateStop::Ended(line)) => {
                if let Some(tracker) = self.tracker() {
                    tracker.fail(provider, external_id, Some(&line));
                }
                return Ok(TrackOutcome::Failed(line));
            }
        };
        // A heart's or a play's holders count a file only by their own gate's verdict; every other
        // origin's file, kept by the shipped checks, is everyone's who asked.
        let gated = !gates.is_empty()
            && matches!(
                origin,
                AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Play
            );
        let mut beside_owned = false;
        if judged && let Some((song, copy)) = self.owned_copy(provider, external_id, &scope).await {
            let mut proven = false;
            if let Some(builder) = &self.extras.gates {
                for gate in &gates {
                    if builder.owned_passes(&copy.absolute_path, gate).await {
                        proven = true;
                        break;
                    }
                }
            }
            if proven {
                self.keep_owned(provider, external_id, requested_by, &song, &copy);
                return Ok(TrackOutcome::AlreadyThere);
            }
            // Nobody can prove the owned copy is the version heard: it is left as it is, never
            // counted as the heart's, and the chain runs.
            info!(
                "{provider}:{external_id} has a copy in the library ({}) nothing proves is the version heard; it is left alone",
                copy.absolute_path
            );
            beside_owned = true;
        }
        // Holders whose gate holds are filed already; the chain runs for the others.
        let holders: Vec<Holder> = if gates.is_empty() {
            holders
        } else {
            holders
                .into_iter()
                .filter(|holder| {
                    gates
                        .iter()
                        .any(|gate| eq_ignore_case(gate.user.trim(), holder.user.trim()))
                })
                .collect()
        };
        // One heard-master round per person for this pass of the chain: one copy of what they
        // heard serves every candidate the pass judges.
        let _rounds = self.extras.gates.as_ref().map(|builder| {
            builder.open_rounds(&super::acquisition_key(provider, external_id, false), &gates)
        });
        // A song fetched ahead is kept at once: no outage hold, no Lidarr hand-off. The download
        // takes the file from the folder instead of a transfer. With gates, only when someone
        // heard that very copy: any other copy is judged when a source of the chain reaches it.
        let ahead_copy = super::acquisition_key(provider, external_id, false);
        let heard_ahead = gates.is_empty() || gates.iter().any(|gate| {
            matches!(
                gate.heard.as_ref().map(|heard| &heard.source),
                Some(octo_core::fingerprint::heart_gate::HeardSource::Copy { key, .. }) if *key == ahead_copy
            )
        });
        if heard_ahead
            && self
                .extras
                .ahead
                .as_ref()
                .is_some_and(|ahead| ahead.has(provider, external_id))
        {
            info!("{provider}:{external_id} was downloaded ahead; keeping that copy");
            let completion = self.direct_queue.enqueue_holding(
                Some(origin),
                gates.clone(),
                std::mem::take(&mut held),
                target,
                false,
                provider,
                external_id,
                true,
                false,
                true,
                Some(DownloadSource::Soulseek),
                true,
                requested_by,
                false,
                None,
            );
            return Ok(match completion.wait().await {
                Ok(path) => self.landed_for(
                    provider,
                    external_id,
                    requested_by,
                    origin,
                    (&holders, gated),
                    completion.ledger(),
                    path,
                    filed,
                ),
                Err(failure) => {
                    if let Some(held) = failure.downcast_ref::<AcquisitionHeld>() {
                        return Ok(self.hold_downloaded(
                            provider,
                            external_id,
                            requested_by,
                            origin,
                            &holders,
                            held,
                            filed,
                        ));
                    }
                    if let Some(tracker) = self.tracker() {
                        tracker.fail(provider, external_id, Some(&failure.to_string()));
                    }
                    TrackOutcome::Failed(failure.to_string())
                }
            });
        }
        let steps: Vec<HeartDownloadSource> = self
            .enabled_steps(false)
            .into_iter()
            .filter(|step| *target == AcquisitionTarget::Shared || *step != HeartDownloadSource::Lidarr)
            .collect();
        // A file one of these gates could not tell apart from another version was passed over.
        let mut undecided = false;
        for (index, &step) in steps.iter().enumerate() {
            let is_last = index == steps.len() - 1;
            // One entry for the whole chain. The first direct source is still waiting for the
            // worker; every later source, and Lidarr, starts over by looking.
            if let Some(tracker) = self.tracker() {
                let note = (index > 0).then(|| {
                    format!(
                        "{} couldn't get it, trying {}",
                        source_name(steps[index - 1]),
                        source_name(step)
                    )
                });
                tracker.stage(
                    provider,
                    external_id,
                    if index == 0 && step != HeartDownloadSource::Lidarr {
                        AcquisitionState::Queued
                    } else {
                        AcquisitionState::Searching
                    },
                    Some(source_name(step)),
                    note.as_deref(),
                );
            }
            if step == HeartDownloadSource::Lidarr {
                // A Lidarr that fails ends the chain with its error, as its exception did.
                if self
                    .lidarr
                    .try_acquire_track(provider, external_id, is_last, requested_by, origin, &gates)
                    .await?
                {
                    return Ok(TrackOutcome::HandedToLidarr);
                }
                // Lidarr's file of the song waits in the one hold path for what its gate needs:
                // the chain stops here and the hold resumes it, as for any source's held file.
                if let Some(held) = self.lidarr.held_file(provider, external_id) {
                    return Ok(self.hold_downloaded(
                        provider,
                        external_id,
                        requested_by,
                        origin,
                        &holders,
                        &held,
                        filed,
                    ));
                }
                // Lidarr last, and its walk got no answer or its indexers are failing: a heart
                // holds until that changes, an import goes back on its trickle.
                if is_last && let Some(busy) = self.lidarr.waits_on(provider, external_id) {
                    let (waiting_for, question) = match busy {
                        LidarrWaits::Answer(HeartBusy::MusicBrainz(question)) => {
                            (WaitingFor::MusicBrainz, Some(question))
                        }
                        LidarrWaits::Answer(HeartBusy::Deezer) => (WaitingFor::Deezer, None),
                        LidarrWaits::Indexers => (WaitingFor::LidarrIndexers, None),
                    };
                    if files_hold(origin, waiting_for) {
                        self.file_hold(
                            provider,
                            external_id,
                            requested_by,
                            origin,
                            waiting_for,
                            holders.clone(),
                            waiting_line(waiting_for),
                            None,
                            question,
                            filed,
                        );
                        return Ok(TrackOutcome::Held(waiting_for));
                    }
                    if origin == AcquisitionOrigin::Import && trickle_waits(waiting_for) {
                        return Ok(TrackOutcome::Held(waiting_for));
                    }
                }
                continue;
            }

            // Soulseek waits out an outage, before the step and again when slskd lost its login
            // partway through it. Every other source, and Soulseek once the wait is over, goes on
            // exactly as before.
            let wait_until = self.soulseek_deadline(&steps, index, held_since_utc);
            // A file another acquisition landed, which joined this one, that none of these
            // holders' own gates passed: the step runs once more as their own.
            let mut ran_as_joiner = false;
            let failure = loop {
                self.hold_for_soulseek(
                    HeldKind::Track,
                    provider,
                    external_id,
                    requested_by,
                    wait_until,
                    origin,
                )
                .await;
                let completion = self.direct_queue.enqueue_holding(
                    Some(origin),
                    gates.clone(),
                    std::mem::take(&mut held),
                    target,
                    false,
                    provider,
                    external_id,
                    true,
                    false,
                    true,
                    Some(to_direct_source(step)),
                    is_last,
                    // Passed on every step, not only the first. A track that fails its way
                    // down the source chain is still the same person's star.
                    requested_by,
                    false,
                    None,
                );
                match completion.wait().await {
                    Ok(path) => {
                        let noted = filed.len();
                        match self.landed_for(
                            provider,
                            external_id,
                            requested_by,
                            origin,
                            (&holders, gated),
                            completion.ledger(),
                            path.clone(),
                            filed,
                        ) {
                            // Nobody here has it: their own gates go on down their own chain.
                            TrackOutcome::Failed(why) => {
                                filed.truncate(noted);
                                // A file another acquisition landed that nobody here judged is
                                // judged by their own gates first: one that passes it is theirs,
                                // and nothing is fetched again.
                                if let Some(outcome) = self
                                    .judged_as_joiners(
                                        provider,
                                        external_id,
                                        requested_by,
                                        (origin, target),
                                        (&holders, &gates),
                                        completion.ledger(),
                                        &path,
                                        filed,
                                    )
                                    .await
                                {
                                    return Ok(outcome);
                                }
                                filed.truncate(noted);
                                if !ran_as_joiner {
                                    ran_as_joiner = true;
                                    info!(
                                        "{provider}:{external_id} landed for someone else; its own heart goes on with {}",
                                        source_name(step)
                                    );
                                    continue;
                                }
                                break Arc::new(anyhow::anyhow!(why));
                            }
                            outcome => return Ok(outcome),
                        }
                    }
                    Err(failure) => {
                        undecided |= holders.iter().any(|holder| {
                            completion.ledger().outcome_for(&holder.user, origin)
                                == Some(super::GateOutcome::Undecided)
                        });
                        // Held in the one hold path: the chain stops here and the hold resumes it.
                        if let Some(held) = failure.downcast_ref::<AcquisitionHeld>() {
                            return Ok(self.hold_downloaded(
                                provider,
                                external_id,
                                requested_by,
                                origin,
                                &holders,
                                held,
                                filed,
                            ));
                        }
                        if !self.soulseek_dropped(wait_until).await {
                            break failure;
                        }
                        info!(
                            "Soulseek lost its connection while getting {provider}:{external_id}; waiting for it rather than moving on"
                        );
                    }
                }
            };

            // A muted mid-chain failure must still leave a trace, or a track that
            // silently fell through every source is undiagnosable from the logs.
            warn!(
                "Heart source {} failed for track {provider}:{external_id}: {failure}",
                source_enum_name(step)
            );
            if is_last {
                let ended = ChainEnd {
                    holders: &holders,
                    gates: &gates,
                    undecided,
                    beside_owned,
                    steps: &steps,
                    reason: failure.to_string(),
                };
                return Ok(self
                    .nothing_proven(provider, external_id, requested_by, origin, target, ended, filed)
                    .await);
            }
            // The next enabled source owns the fallback.
        }
        // Only reached when the last source was Lidarr and it said no. It records its own
        // reason first; this is the fallback when it could not.
        const NO_SOURCE: &str = "No download source could get this song.";
        let ended = ChainEnd {
            holders: &holders,
            gates: &gates,
            undecided,
            beside_owned,
            steps: &steps,
            reason: NO_SOURCE.to_string(),
        };
        Ok(self
            .nothing_proven(provider, external_id, requested_by, origin, target, ended, filed)
            .await)
    }

    /// Every source of a heart's chain ran and none brought a file its gates passed. A heart that
    /// was never played holds until a play records what the person hears, and so does one whose
    /// stream stood in with another song. One that heard the song keeps the heard audio itself:
    /// the whole copy of the very stream that played, kept as heard, with the line that says so
    /// and on the path that keeps looking for a copy it can prove. Only when even the heard audio
    /// cannot be had does it end, with the plain line. Every other origin fails as before.
    #[allow(clippy::too_many_arguments)] // the song, who asked, why and how the chain ended
    async fn nothing_proven(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        target: &AcquisitionTarget,
        ended: ChainEnd<'_>,
        filed: &mut Vec<(String, String)>,
    ) -> TrackOutcome {
        let fail = |line: &str| {
            if let Some(tracker) = self.tracker() {
                tracker.fail(provider, external_id, Some(line));
            }
            TrackOutcome::Failed(line.to_string())
        };
        let hearts = matches!(origin, AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart);
        // A hearted album's track no source brought a file of is not held: nothing waits for it.
        let album_track = origin == AcquisitionOrigin::AlbumHeart
            && ended
                .gates
                .iter()
                .any(|gate| self.album_track_of(provider, external_id, &gate.user).is_some());
        if !hearts || ended.gates.is_empty() || album_track {
            return fail(&ended.reason);
        }
        let holder_of = |gate: &HeartGate| {
            ended
                .holders
                .iter()
                .find(|holder| eq_ignore_case(holder.user.trim(), gate.user.trim()))
                .cloned()
                .unwrap_or_else(|| Holder {
                    user: gate.user.clone(),
                    heard: gate.heard.clone(),
                    ..Default::default()
                })
        };
        let mut held = None;
        let mut keeping: Vec<HeartGate> = Vec::new();
        for gate in ended.gates {
            let line = match gate.reading {
                HeardReading::NotHeard => NEVER_PLAYED,
                HeardReading::AnotherSong => ANOTHER_SONG_PLAYED,
                _ => {
                    if gate.heard.is_some() {
                        keeping.push(gate.clone());
                    }
                    continue;
                }
            };
            held.get_or_insert(WaitingFor::NotYetProven);
            self.file_hold(
                provider,
                external_id,
                requested_by,
                origin,
                WaitingFor::NotYetProven,
                vec![holder_of(gate)],
                line,
                None,
                None,
                filed,
            );
        }
        if !keeping.is_empty() {
            let line = if ended.beside_owned {
                KEPT_BESIDE_OWNED
            } else if ended.undecided {
                KEPT_UNDECIDED
            } else if ended.steps.iter().any(|step| match step {
                HeartDownloadSource::Soulseek => true,
                // Lidarr searches only for an artist with no music in it.
                HeartDownloadSource::Lidarr => !self.lidarr.left_alone(provider, external_id),
                HeartDownloadSource::YouTube => false,
            }) {
                KEPT_AS_HEARD
            } else {
                KEPT_NO_SOURCE
            };
            if let Some(tracker) = self.tracker() {
                tracker.done_line(provider, external_id, line);
            }
            info!("Nothing proven for {provider}:{external_id}; keeping the version that was heard");
            let keepers: Vec<Holder> = keeping.iter().map(holder_of).collect();
            match self
                .keep_heard(provider, external_id, requested_by, origin, target, &keeping)
                .await
            {
                Ok((path, ledger)) => {
                    match self.landed_for(
                        provider,
                        external_id,
                        requested_by,
                        origin,
                        (&keepers, true),
                        &ledger,
                        path,
                        filed,
                    ) {
                        TrackOutcome::Landed(path) => return TrackOutcome::KeptAsHeard(path),
                        TrackOutcome::Held(waiting) => {
                            held.get_or_insert(waiting);
                        }
                        _ => {}
                    }
                }
                Err(failure) => {
                    if let Some(waits) = failure.downcast_ref::<AcquisitionHeld>() {
                        return self.hold_downloaded(
                            provider,
                            external_id,
                            requested_by,
                            origin,
                            &keepers,
                            waits,
                            filed,
                        );
                    }
                    info!("The heard audio of {provider}:{external_id} cannot be had: {failure}");
                }
            }
        }
        match held {
            Some(waiting) => TrackOutcome::Held(waiting),
            None if keeping.is_empty() => fail(&ended.reason),
            None => fail(HEARD_GONE),
        }
    }

    /// Keeps the very audio these gates' people heard, by its identity: the whole copy of the heard
    /// video's own stream, at the heard format and size, kept as heard. Nothing else is fetched
    /// for it. The path it landed at and its gates' verdicts, or why it could not be had.
    async fn keep_heard(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        target: &AcquisitionTarget,
        gates: &[HeartGate],
    ) -> Result<(String, super::GateLedger), Arc<anyhow::Error>> {
        let completion = self.direct_queue.enqueue_holding(
            Some(origin),
            keep_gates(gates),
            HeldFiles::default(),
            target,
            false,
            provider,
            external_id,
            origin != AcquisitionOrigin::Play,
            false,
            true,
            Some(DownloadSource::YouTube),
            origin != AcquisitionOrigin::Play,
            requested_by,
            false,
            None,
        );
        let path = completion.wait().await?;
        Ok((path, completion.ledger().clone()))
    }

    /// The album chain of an album heart. `Err` for a Lidarr step that failed.
    pub(crate) async fn acquire_album(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        self.acquire_album_as(
            provider,
            album_external_id,
            requested_by,
            held_since_utc,
            AcquisitionOrigin::AlbumHeart,
        )
        .await
    }

    /// [`Self::acquire_album`] run for `origin`.
    pub(crate) async fn acquire_album_as(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
        origin: AcquisitionOrigin,
    ) -> anyhow::Result<()> {
        if self
            .album_already_yours(provider, album_external_id, requested_by)
            .await
        {
            return Ok(());
        }
        // The album's gate is built before any source runs and held for the whole chain, so every
        // source's check point files each track by its own sound.
        let user = requested_by.map(str::trim).filter(|user| !user.is_empty());
        let listing = match user {
            Some(_) if self.extras.gates.is_some() && self.extras.album_gates.is_some() => {
                self.album_listing(provider, album_external_id).await
            }
            _ => None,
        };
        let _gate = match (user, listing) {
            (Some(user), Some(album)) => match self
                .album_gate_up(provider, album_external_id, &album, user, origin)
                .await
            {
                Ok(gate) => gate,
                Err(()) => return Ok(()),
            },
            _ => None,
        };
        let steps = self.enabled_steps(true);
        for (index, &step) in steps.iter().enumerate() {
            let is_last = index == steps.len() - 1;
            if step == HeartDownloadSource::Lidarr {
                let landed = self
                    .lidarr
                    .try_acquire_album(provider, album_external_id, is_last, requested_by, origin, &[])
                    .await;
                // The tracks Lidarr's import held wait as held album tracks; the next walk leaves
                // them.
                if let Some(user) = user {
                    self.file_album_holds(provider, album_external_id, user, origin);
                }
                if landed? {
                    return Ok(());
                }
                // Lidarr last, and the album's lookup got no answer: the album heart holds until
                // it does, saved and asked again, as a song heart's walk does.
                if is_last
                    && let Some(LidarrWaits::Answer(busy)) = self.lidarr.waits_on(provider, album_external_id)
                {
                    let (waiting_for, question) = match busy {
                        HeartBusy::MusicBrainz(question) => (WaitingFor::MusicBrainz, Some(question)),
                        HeartBusy::Deezer => (WaitingFor::Deezer, None),
                    };
                    if files_hold(origin, waiting_for) {
                        self.hold_album(provider, album_external_id, user, origin, waiting_for, question);
                        return Ok(());
                    }
                }
                continue;
            }

            let wait_until = self.soulseek_deadline(&steps, index, held_since_utc);
            let failure = loop {
                self.hold_for_soulseek(
                    HeldKind::Album,
                    provider,
                    album_external_id,
                    requested_by,
                    wait_until,
                    origin,
                )
                .await;
                let mut failure = None;
                let walked = self
                    .direct_downloads
                    .download_album_with_source(
                        provider,
                        album_external_id,
                        to_direct_source(step),
                        !is_last,
                        &CancellationToken::new(),
                        requested_by.map(|r| vec![r.to_string()]),
                    )
                    .await;
                // The tracks the walk held wait as held album tracks; the next walk leaves them.
                if let Some(user) = user {
                    self.file_album_holds(provider, album_external_id, user, origin);
                }
                match walked {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(e) => failure = Some(e),
                }
                // A walk that came up short because slskd dropped partway: the tracks it got stay,
                // and the next walk skips them.
                if !self.soulseek_dropped(wait_until).await {
                    break failure;
                }
                info!(
                    "Soulseek lost its connection during album {provider}:{album_external_id}; waiting for it rather than moving on"
                );
            };

            if let Some(failure) = failure {
                warn!(
                    "Heart source {} failed for album {provider}:{album_external_id}: {failure}",
                    source_enum_name(step)
                );
                if is_last {
                    if let Some(tracker) = self.tracker() {
                        tracker.fail_album(provider, album_external_id, Some(&failure.to_string()));
                    }
                    return Ok(());
                }
            }
            // Continue down the configured priority list.
        }
        // Every source has had its go. Tracks the last walk already settled keep what it said;
        // this only closes the ones nothing finished.
        if let Some(tracker) = self.tracker() {
            tracker.fail_album(
                provider,
                album_external_id,
                Some("No download source could get this track."),
            );
        }
        Ok(())
    }

    fn enabled_steps(&self, album_heart: bool) -> Vec<HeartDownloadSource> {
        // SpotiFLAC rides on the Soulseek step, so with SpotiFLAC on the step stays.
        let soulseek_off = self.extras.soulseek.as_ref().is_some_and(|link| link.is_off())
            && !self.settings.current().spotiflac.is_on();
        self.settings
            .current()
            .subsonic
            .effective_heart_download_sources()
            .into_iter()
            .filter(|step| {
                if album_heart {
                    step.album_enabled == Some(true)
                } else {
                    step.song_enabled == Some(true)
                }
            })
            .filter(|step| !(soulseek_off && step.source == HeartDownloadSource::Soulseek))
            .map(|step| step.source)
            .collect()
    }

    fn play_source(&self) -> Option<DownloadSource> {
        let sources: Vec<HeartDownloadSource> = self
            .enabled_steps(false)
            .into_iter()
            .filter(|s| *s != HeartDownloadSource::Lidarr)
            .collect();
        let first = *sources.first()?;
        Some(if first == HeartDownloadSource::YouTube {
            DownloadSource::YouTube
        } else if sources.contains(&HeartDownloadSource::YouTube) {
            DownloadSource::SoulseekThenYouTube
        } else {
            DownloadSource::Soulseek
        })
    }

    /// A heart on a song already in the library is a favorite, not a download. Asked first, so
    /// it never waits behind other downloads or a Soulseek outage, and never sends Lidarr for a
    /// whole album. The row closes on the library's own song, which favorites it for whoever
    /// hearted it (StarOnArrival); an owned lossy copy is also queued for Better quality.
    ///
    /// `scope` is whose library counts: the shared one for the owner, and the shared one plus
    /// their own for a member. Another member's own songs never count.
    async fn already_yours(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        scope: &LibraryScope,
    ) -> bool {
        let Some((song, copy)) = self.owned_copy(provider, external_id, scope).await else {
            return false;
        };
        self.keep_owned(provider, external_id, requested_by, &song, &copy);
        true
    }

    /// The library's copy of the song in `scope`, when the owned check is on and finds one.
    async fn owned_copy(
        &self,
        provider: &str,
        external_id: &str,
        scope: &LibraryScope,
    ) -> Option<(octo_core::models::domain::Song, OwnedCopy)> {
        self.extras
            .owned
            .as_ref()?
            .find_song_in(scope, provider, external_id)
            .await
    }

    /// The heart is the owned copy's: it is favorited, its row closed on it, and a lossy copy
    /// queued for Better quality.
    fn keep_owned(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        song: &octo_core::models::domain::Song,
        copy: &OwnedCopy,
    ) {
        let Some(owned) = &self.extras.owned else {
            return;
        };
        let upgrading = owned.queue_upgrade_if_wanted(copy, song, requested_by);
        // Before the row closes, so the close never reads as a download that landed.
        if let Some(stars) = &self.extras.stars {
            stars.favorite_owned(
                provider,
                external_id,
                copy.navidrome_id.as_deref(),
                &song.artist,
                &song.title,
                &copy.absolute_path,
            );
        }
        info!(
            "Hearted '{} - {}' is already in the library ({}); favoriting it instead of downloading{}",
            song.artist,
            song.title,
            copy.suffix,
            if upgrading {
                ", and looking for a higher quality copy"
            } else {
                ""
            }
        );
        self.settle(provider, external_id, copy);
    }

    /// An album heart where every song is already in the library: each is favorited
    /// through its row, and so the album too. One missing song and the album goes down the chain,
    /// where the songs already there are skipped as before.
    async fn album_already_yours(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
    ) -> bool {
        let Some(owned) = &self.extras.owned else {
            return false;
        };
        let Some((album, songs)) = owned.find_whole_album(provider, album_external_id).await else {
            return false;
        };
        let tracked: Vec<&(octo_core::models::domain::Song, OwnedCopy)> = songs
            .iter()
            .filter(|(song, _)| song.external_id.as_deref().is_some_and(|id| !id.is_empty()))
            .collect();
        if let Some(any_song) = songs.iter().find_map(|(_, copy)| copy.navidrome_id.as_deref())
            && let Some(stars) = &self.extras.stars
        {
            stars.favorite_owned_album(provider, album_external_id, any_song);
        }
        if let Some(tracker) = self.tracker() {
            let listed: Vec<(String, Option<String>, Option<String>, Option<String>)> = tracked
                .iter()
                .map(|(song, _)| {
                    (
                        song.external_id.clone().unwrap_or_default(),
                        Some(song.artist.clone()),
                        Some(song.title.clone()),
                        Some(album.title.clone()),
                    )
                })
                .collect();
            tracker.announce(provider, Some(album_external_id), None, &listed);
        }
        let mut upgrading = 0;
        for (song, copy) in &tracked {
            if owned.queue_upgrade_if_wanted(copy, song, requested_by) {
                upgrading += 1;
            }
            self.settle(
                song.external_provider.as_deref().unwrap_or(provider),
                song.external_id.as_deref().unwrap_or_default(),
                copy,
            );
        }
        info!(
            "Hearted album '{} - {}' is already in the library ({} songs); favoriting it instead of downloading{}",
            album.artist,
            album.title,
            songs.len(),
            if upgrading > 0 {
                format!(", {upgrading} queued for a higher quality copy")
            } else {
                String::new()
            }
        );
        true
    }

    /// Close a row on the library's copy: by its Navidrome id when known, which is
    /// immediate, or by its path, which the tracker watches until Navidrome shows it.
    fn settle(&self, provider: &str, external_id: &str, copy: &OwnedCopy) {
        let Some(tracker) = self.tracker() else { return };
        match &copy.navidrome_id {
            Some(library_id) => tracker.complete(provider, external_id, Some(library_id)),
            None => tracker.imported(provider, external_id, None, None, Some(&copy.absolute_path)),
        }
    }

    /// When a Soulseek step stops waiting for slskd, or `None` when it never waits: another
    /// source, no link, the wait switched off, or Lidarr later in the chain. The wait exists so
    /// an outage does not turn a lossless heart into a YouTube MP3; Lidarr can bring a lossless
    /// copy now, and so can SpotiFLAC when it is on.
    fn soulseek_deadline(
        &self,
        steps: &[HeartDownloadSource],
        index: usize,
        held_since_utc: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let link = self.extras.soulseek.as_ref()?;
        (steps[index] == HeartDownloadSource::Soulseek
            && link.hold_limit() > TimeDelta::zero()
            && !steps[index + 1..].contains(&HeartDownloadSource::Lidarr)
            && !self.settings.current().spotiflac.is_on())
        .then(|| held_since_utc.unwrap_or_else(|| link.utc_now()) + link.hold_limit())
    }

    /// Waits while slskd says it is not logged in, up to `wait_until`. On disk for the length
    /// of the wait, so a restart picks the heart up again.
    async fn hold_for_soulseek(
        &self,
        kind: HeldKind,
        provider: &str,
        id: &str,
        requested_by: Option<&str>,
        wait_until: Option<DateTime<Utc>>,
        origin: AcquisitionOrigin,
    ) {
        let (Some(deadline), Some(link)) = (wait_until, self.extras.soulseek.as_ref()) else {
            return;
        };
        let Some(reading) = link
            .read(false)
            .await
            .filter(|r| r.link == SoulseekLinkState::NotLoggedIn)
        else {
            return;
        };

        // A heart of its kind is written as before: no origin on the file.
        let heart_of_kind = match kind {
            HeldKind::Track => AcquisitionOrigin::Heart,
            HeldKind::Album => AcquisitionOrigin::AlbumHeart,
        };
        let held = self.extras.holds.as_ref().map(|holds| {
            holds.hold(HeldAcquisition {
                kind,
                provider: provider.to_string(),
                external_id: id.to_string(),
                requested_by: requested_by.map(str::to_string),
                held_since_utc: deadline - link.hold_limit(),
                origin: (origin != heart_of_kind).then_some(origin),
                ..Default::default()
            })
        });
        if kind == HeldKind::Track
            && let Some(tracker) = self.tracker()
        {
            tracker.stage(
                provider,
                id,
                AcquisitionState::Queued,
                Some("Soulseek"),
                Some(&format!(
                    "Waiting for Soulseek to come back, until {} UTC",
                    deadline.format("%H:%M")
                )),
            );
        }
        info!(
            "Soulseek is not connected (slskd says {}); holding {} {provider}:{id} until {} UTC",
            reading.state.as_deref().unwrap_or("not logged in"),
            kind.name(),
            deadline.format("%H:%M")
        );
        let back = link.wait_for_login(deadline).await;
        if back {
            info!("Soulseek is back; going on with {} {provider}:{id}", kind.name());
        } else {
            warn!(
                "Soulseek still not connected after the wait; {} {provider}:{id} goes on to the next source",
                kind.name()
            );
        }
        if kind == HeldKind::Track
            && let Some(tracker) = self.tracker()
        {
            tracker.stage(
                provider,
                id,
                AcquisitionState::Queued,
                Some("Soulseek"),
                Some(if back {
                    "Soulseek is back"
                } else {
                    "Soulseek did not come back in time"
                }),
            );
        }
        // Only this chain's own place in the wait ends: another person's heart waiting out the
        // same outage stays in it.
        if let (Some(held), Some(holds)) = (held, &self.extras.holds) {
            self.leave_outage_hold(holds, &held.key(), requested_by);
        }
    }

    /// One chain's place in a Soulseek outage's hold ends: its person leaves it, letting go of the
    /// heard copy it kept for them once no hold of the song lists them, and the hold ends once
    /// nobody is left (one with nobody listed ends with its chain).
    fn leave_outage_hold(&self, holds: &SoulseekHoldStore, key: &str, user: Option<&str>) {
        match user.map(str::trim).filter(|user| !user.is_empty()) {
            Some(user) => {
                let left = holds.get(key).and_then(|record| {
                    record
                        .holders
                        .iter()
                        .find(|holder| eq_ignore_case(holder.user.trim(), user))
                        .cloned()
                        .map(|holder| (record.provider.clone(), record.external_id.clone(), holder))
                });
                holds.release_holder(key, user);
                if let Some((provider, external_id, holder)) = left {
                    self.let_go_of_copies(&provider, &external_id, &holder);
                }
            }
            None => {
                if holds.get(key).is_some_and(|record| record.holders.is_empty()) {
                    holds.release(key);
                }
            }
        }
    }

    /// True when the step just failed because slskd lost its login and there is still time
    /// to wait. A fresh read: the cached one may be from before the drop.
    async fn soulseek_dropped(&self, wait_until: Option<DateTime<Utc>>) -> bool {
        let (Some(deadline), Some(link)) = (wait_until, self.extras.soulseek.as_ref()) else {
            return false;
        };
        link.utc_now() < deadline
            && link.read(true).await.map(|r| r.link) == Some(SoulseekLinkState::NotLoggedIn)
    }
}

/// What a person's chain in a hold runs for, by the heart that keeps them in it: the hold's own
/// origin when they are in it by its own route, a song heart's when their heart of the song keeps
/// them in another's hold, and an album heart's when only their heart of the album does.
fn route_origin(holder: &Holder, record: &HeldAcquisition) -> AcquisitionOrigin {
    use super::soulseek_hold_store::HoldRoute;
    let routes = holder.routes_in(record);
    if routes.contains(&HoldRoute::of(record)) {
        record.origin()
    } else if routes.contains(&HoldRoute::Song) {
        AcquisitionOrigin::Heart
    } else {
        AcquisitionOrigin::AlbumHeart
    }
}

/// Whether `holder` is in the held album track `record` by their heart of the album.
fn by_album(holder: &Holder, record: &HeldAcquisition) -> bool {
    holder
        .routes_in(record)
        .contains(&super::soulseek_hold_store::HoldRoute::Album)
}

/// Whether `holder` is in `record` by their heart of the song.
fn by_song(holder: &Holder, record: &HeldAcquisition) -> bool {
    holder
        .routes_in(record)
        .contains(&super::soulseek_hold_store::HoldRoute::Song)
}

/// A held album track becomes the song's own hold, for the people in it by their heart of the
/// song: no album track, a song heart's, nobody listing a route of their own, and the album's
/// line, when it shows one, the wait's own.
fn as_song_hold(record: &mut HeldAcquisition) {
    record.album_track = None;
    record.origin = Some(AcquisitionOrigin::Heart);
    for holder in &mut record.holders {
        holder.routes.clear();
    }
    if record.line.as_deref() == Some(tracks_wait_line(1).as_str()) {
        record.line = record
            .waiting_for
            .map(|waiting| waiting_line(waiting).to_string());
    }
}

/// Whether a hold waits for nothing but a person: a heart held "not yet proven", or a hearted
/// album's held track.
pub(crate) fn waits_for_a_person(record: &HeldAcquisition) -> bool {
    record.kind == HeldKind::Track
        && record.waiting_for == Some(WaitingFor::NotYetProven)
        && matches!(
            record.origin(),
            AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart
        )
}

/// Whether a played song is a held album track by its names: the same artist, the same core title
/// and a length within max(7 s, 4%) of the track's. Without both lengths, never.
fn plays_as(track: &HeldAlbumTrack, artist: Option<&str>, title: Option<&str>, length: Option<i32>) -> bool {
    use octo_core::fingerprint::verification::{core_title, length_fits};
    let (Some(artist), Some(title), Some(length), Some(want)) = (
        artist,
        title,
        length.filter(|seconds| *seconds > 0),
        track.duration_seconds.filter(|seconds| *seconds > 0),
    ) else {
        return false;
    };
    let core = core_title(title);
    !core.is_empty()
        && core == core_title(&track.title)
        && octo_core::common::SongIdentity::same_artist_name(artist, &track.artist)
        && length_fits(f64::from(length), f64::from(want))
}

fn owned3(a: &str, b: &str, c: Option<&str>) -> (String, String, Option<String>) {
    (a.to_string(), b.to_string(), c.map(str::to_string))
}

fn source_name(source: HeartDownloadSource) -> &'static str {
    match source {
        HeartDownloadSource::Lidarr => "Lidarr",
        HeartDownloadSource::YouTube => "YouTube",
        _ => "Soulseek",
    }
}

/// The enum member's name, as `{Source}` interpolated it.
fn source_enum_name(source: HeartDownloadSource) -> &'static str {
    source_name(source)
}

fn to_direct_source(source: HeartDownloadSource) -> DownloadSource {
    match source {
        HeartDownloadSource::YouTube => DownloadSource::YouTube,
        _ => DownloadSource::Soulseek,
    }
}

#[path = "heart_keep_anyway.rs"]
pub mod keep_anyway;

pub use keep_anyway::{KEEP_WAITS_FOR_YOUTUBE, KeepAnyway, KeepMarks};

#[cfg(test)]
#[path = "heart_acquisition_coordinator_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "heart_acquisition_family_tests.rs"]
mod family_tests;

#[cfg(test)]
#[path = "heart_acquisition_hold_tests.rs"]
mod hold_tests;

#[cfg(test)]
#[path = "heart_acquisition_album_tests.rs"]
mod album_tests;
