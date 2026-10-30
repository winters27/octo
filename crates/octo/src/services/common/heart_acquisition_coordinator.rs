//! The heart acquisition coordinator: which source a heart or a play is handed to.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardRecord, HeartGate, WaitingFor};
use octo_core::settings::{DownloadSource, HeartDownloadSource, SettingsStore};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::TrackAcquisitionQueue;
use super::acquisition_tracker::key_of;
use super::soulseek_hold_store::Holder;
use super::{
    AcquisitionHeld, AcquisitionState, AcquisitionTarget, AcquisitionTracker, HeldAcquisition, HeldKind,
    SoulseekHoldStore, StarOnArrival,
};
use crate::services::family::{FamilyPool, LibraryScope, LibraryWriteGate, WriteDecision};
use crate::services::fingerprint::MusicBrainzClient;
use crate::services::i_download_service::IDownloadService;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::imports::trickle_worker::{TrickleCheckLater, WAITING_FOR_ANSWER};
use crate::services::library::{HeartOwnership, OwnedCopy};
use crate::services::lidarr::ILidarrHeartAcquisitionService;
use crate::services::prefetch::PrefetchedFiles;
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
/// import waiting for MusicBrainz is put back on the trickle instead, its wait being the import
/// matcher's own.
fn files_hold(origin: AcquisitionOrigin, waiting_for: WaitingFor) -> bool {
    match origin {
        AcquisitionOrigin::Play | AcquisitionOrigin::Request => false,
        AcquisitionOrigin::Import => waiting_for != WaitingFor::MusicBrainz,
        _ => true,
    }
}

/// The line a held row shows when its hold kept none of its own.
pub(crate) fn waiting_line(waiting_for: WaitingFor) -> &'static str {
    match waiting_for {
        WaitingFor::MusicBrainz => "Waiting for MusicBrainz",
        WaitingFor::LidarrIndexers => "Waiting for Lidarr's indexers",
        WaitingFor::AcoustId => "Waiting for AcoustID",
        WaitingFor::AcoustIdOff => "AcoustID is off, so Octo cannot check what played",
        WaitingFor::Deezer => "Waiting for Deezer",
        WaitingFor::Shim => "Waiting for YouTube",
        WaitingFor::NotYetProven => "Play it once so Octo can keep the version you hear",
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
        // WaitForLosslessOnPlay acquires the track itself, from its own source.
        if settings.download_on_play
            && (ahead || !settings.wait_for_lossless_on_play)
            && let Some(source) = self.play_source()
        {
            let holds = self
                .extras
                .soulseek
                .as_ref()
                .is_some_and(|link| link.hold_limit() > TimeDelta::zero());
            let waits = source != DownloadSource::YouTube && holds;
            if !waits && self.extras.gates.is_none() {
                self.queue_play_download(
                    provider,
                    external_id,
                    source,
                    requested_by,
                    client_id,
                    owner,
                    &target,
                    Vec::new(),
                );
            } else {
                // The play's gate is built from what it played before its download is queued.
                let coordinator = Arc::clone(self);
                let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
                let (client_id, owner) = (client_id.map(str::to_string), owner.map(str::to_string));
                let target = target.clone();
                tokio::spawn(async move {
                    let gates = coordinator
                        .play_gates(&provider, &external_id, requested_by.as_deref(), heard)
                        .await;
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
                        );
                    }
                });
            }
        }
        // Lidarr adds whole albums to the shared library, never to a member's own.
        if !settings.lidarr_album_on_play || target != AcquisitionTarget::Shared {
            return;
        }
        let key = format!("{provider}:{external_id}");
        let added = {
            let mut plays = self.lidarr_plays.lock();
            if plays.len() >= Self::LIDARR_PLAY_MEMORY {
                plays.clear();
            }
            plays.insert(key.clone())
        };
        if added {
            let coordinator = Arc::clone(self);
            let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
            tokio::spawn(async move {
                coordinator
                    .hand_play_to_lidarr(&key, &provider, &external_id, requested_by.as_deref())
                    .await
            });
        }
    }

    /// A play's gate, built from what it played, when gates are built at all. A play never
    /// holds, so a gate that would hold is still the play's gate.
    async fn play_gates(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        heard: Option<HeardRecord>,
    ) -> Vec<HeartGate> {
        let Some(builder) = self.extras.gates.clone() else {
            return Vec::new();
        };
        let mut holder = self.holder_for(external_id, requested_by);
        if heard.is_some() {
            holder.heard = heard;
        }
        match builder
            .build(provider, external_id, &holder, AcquisitionOrigin::Play)
            .await
        {
            GateStart::Ready(gate) | GateStart::Hold { gate, .. } => vec![gate],
        }
    }

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
    ) {
        let on_queued = || {
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
        let tracker = self.tracker().cloned();
        let (provider, external_id) = (provider.to_string(), external_id.to_string());
        tokio::spawn(async move {
            let Err(reason) = request.completion.wait().await else {
                return;
            };
            debug!("Play download failed for {provider}:{external_id}: {reason}");
            // A heart that joined owns the row, and may still be trying its next source.
            if !request.is_star
                && !request.heart_joined()
                && let Some(tracker) = tracker
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
        );
    }

    async fn hand_play_to_lidarr(
        &self,
        key: &str,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
    ) {
        match self
            .lidarr
            .try_acquire_track(
                provider,
                external_id,
                false,
                requested_by,
                AcquisitionOrigin::Play,
                &[],
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
        landed
    }

    /// A heart that was waiting for Soulseek when Octo restarted. Its saved start time
    /// keeps the original deadline; the entry goes once the chain has finished either way.
    pub(crate) fn resume_track(self: &Arc<Self>, held: HeldAcquisition) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let chain = coordinator
                .acquire_track_for(
                    &held.provider,
                    &held.external_id,
                    held.requested_by.as_deref(),
                    Some(held.held_since_utc),
                    &AcquisitionTarget::Shared,
                    held.origin(),
                )
                .await;
            coordinator.resumed(&held, chain);
        });
    }

    pub(crate) fn resume_album(self: &Arc<Self>, held: HeldAcquisition) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let chain = coordinator
                .acquire_album_as(
                    &held.provider,
                    &held.external_id,
                    held.requested_by.as_deref(),
                    Some(held.held_since_utc),
                    held.origin(),
                )
                .await;
            coordinator.resumed(&held, chain);
        });
    }

    /// The end of a resumed chain: a chain that failed is logged, and the entry goes either way.
    fn resumed(&self, held: &HeldAcquisition, chain: anyhow::Result<()>) {
        if let Err(e) = chain {
            warn!("Resumed {} failed: {e}", held.key());
        }
        if let Some(holds) = &self.extras.holds {
            holds.release(&held.key());
        }
    }

    /// Runs the held acquisition `key` again now, once: as one acquisition, its first holder
    /// starting it and every other holder joining with their own gate. A holder whose gate holds
    /// again stays in a hold of that wait; everyone else leaves the record with the chain's end,
    /// and the record ends when nobody is left. Nothing for a hold that waits out a Soulseek
    /// outage (its own chain is waiting), for one already running again, or for no such hold.
    pub(crate) fn resume_held(self: &Arc<Self>, key: &str) {
        let Some(record) = self.extras.holds.as_ref().and_then(|holds| holds.get(key)) else {
            return;
        };
        if record.waiting_for.is_none() || !self.resuming.lock().insert(key.to_string()) {
            return;
        }
        info!("Resuming held {key}");
        let coordinator = Arc::clone(self);
        let key = key.to_string();
        tokio::spawn(async move {
            coordinator.run_held(&record).await;
            coordinator.resuming.lock().remove(&key);
        });
    }

    async fn run_held(&self, record: &HeldAcquisition) {
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
        let mut filed: Vec<(String, String)> = Vec::new();
        let ended = match record.kind {
            HeldKind::Track => {
                self.track_chain(
                    &record.provider,
                    &record.external_id,
                    requested_by.as_deref(),
                    None,
                    &target,
                    origin,
                    record.holders.clone(),
                    &mut filed,
                )
                .await
            }
            HeldKind::Album => self
                .acquire_album_as(
                    &record.provider,
                    &record.external_id,
                    requested_by.as_deref(),
                    None,
                    origin,
                )
                .await
                .map(|()| TrackOutcome::AlreadyThere),
        };
        let held_again = matches!(ended, Ok(TrackOutcome::Held(_)));
        if let Err(e) = &ended {
            warn!("Resumed {key} failed: {e}");
        }
        // A holder filed again under this very hold stays; one filed under another wait has moved
        // there; one the chain held without filing anywhere (an origin that never files that
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
            keys.contains(&key.as_str()) || (held_again && keys.is_empty())
        };
        if record.holders.is_empty() {
            if !stays("") {
                holds.release(&key);
            }
        } else {
            for holder in record.holders.iter().filter(|holder| !stays(&holder.user)) {
                holds.release_holder(&key, &holder.user);
            }
        }
        if holds.get(&key).is_none()
            && let Some(tracker) = self.tracker()
        {
            tracker.release_row(&record.provider, &record.external_id, &holds);
        }
    }

    /// `user` took their heart back: they leave every hold of the song, whatever it waits for, and
    /// each hold nobody is left in ends, its row with the last. How many holds they left.
    pub fn leave_holds(&self, provider: &str, external_id: &str, user: &str) -> usize {
        let Some(holds) = self.extras.holds.as_ref() else {
            return 0;
        };
        let mut left = 0;
        for record in holds.records_for(&track_base_key(provider, external_id)) {
            if !record
                .holders
                .iter()
                .any(|holder| eq_ignore_case(holder.user.trim(), user.trim()))
            {
                continue;
            }
            left += 1;
            if holds.release_holder(&record.key(), user).is_none() {
                info!(
                    "{user} took back the last heart on held {}; it ends",
                    record.key()
                );
            }
        }
        if left > 0
            && let Some(tracker) = self.tracker()
        {
            tracker.release_row(provider, external_id, holds);
        }
        left
    }

    /// Opens again the downloads row of a held song, as a restart left none: named from the
    /// registry, owned by its holders, held, with its saved line.
    pub(crate) fn hold_row_for(&self, record: &HeldAcquisition) {
        let (Some(tracker), Some(waiting)) = (self.tracker(), record.waiting_for) else {
            return;
        };
        if record.kind != HeldKind::Track {
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
        if origin == AcquisitionOrigin::Import && outcome == TrackOutcome::Held(WaitingFor::MusicBrainz) {
            return Err(TrickleCheckLater {
                detail: WAITING_FOR_ANSWER.to_string(),
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
            &mut Vec::new(),
        )
        .await
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
        let holder = self.holder_for(external_id, requested_by);
        let key = record.key();
        if !holder.user.is_empty() {
            holds.hold(HeldAcquisition {
                holders: vec![holder],
                ..record
            });
        }
        info!(
            "{provider}:{external_id} is held ({}); the heart joins it and asks again",
            waiting.name()
        );
        if let Some(me) = self.me.upgrade() {
            me.resume_held(&key);
        }
        Some(waiting)
    }

    /// Each holder's gate, built before any source runs. A holder whose gate holds is filed under
    /// that wait (`filed` says where); Err with the wait when nobody's gate is ready.
    async fn gate_up(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        holders: &[Holder],
        filed: &mut Vec<(String, String)>,
    ) -> Result<Vec<HeartGate>, WaitingFor> {
        let Some(builder) = self.extras.gates.clone() else {
            return Ok(Vec::new());
        };
        let mut gates = Vec::new();
        let mut held = None;
        for holder in holders {
            match builder.build(provider, external_id, holder, origin).await {
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
                        Vec::new(),
                        question,
                        filed,
                    );
                }
            }
        }
        match held {
            Some(waiting) if gates.is_empty() => Err(waiting),
            _ => Ok(gates),
        }
    }

    /// Files `holders` in a hold of this song waiting for `waiting_for`, joining one already
    /// there, with the files it keeps; holds its downloads row with `line`; and, for a
    /// MusicBrainz question, asks it again until it answers, which resumes the hold. Nothing for
    /// an origin that never holds so.
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
        files: Vec<String>,
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
        let kept = holds.hold(HeldAcquisition {
            kind: HeldKind::Track,
            provider: provider.to_string(),
            external_id: external_id.to_string(),
            requested_by,
            held_since_utc: Utc::now(),
            origin: Some(origin),
            waiting_for: Some(waiting_for),
            holders,
            line: Some(line.to_string()).filter(|line| !line.trim().is_empty()),
            ..Default::default()
        });
        let key = kept.key();
        let kept = holds
            .update(&key, |record| {
                for file in files {
                    if !record.held_files.contains(&file) {
                        record.held_files.push(file);
                    }
                }
                if !line.trim().is_empty() {
                    record.line = Some(line.to_string());
                }
            })
            .unwrap_or(kept);
        filed.extend(users.into_iter().map(|user| (user, key.clone())));
        if let Some(tracker) = self.tracker() {
            let owners: Vec<String> = kept.holders.iter().map(|holder| holder.user.clone()).collect();
            tracker.hold_row(provider, external_id, &owners, line, kept.keeps_anyway());
        }
        info!("Holding {key}: {line}");
        if waiting_for == WaitingFor::MusicBrainz
            && let Some(question) = question
        {
            self.re_ask_for(&key, &question);
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
                    coordinator.resume_held(&key);
                }
            }),
        );
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
            held.files.clone(),
            None,
            filed,
        );
        TrackOutcome::Held(held.waiting_for)
    }

    /// The song chain for these holders: their gates first, then the sources in order, every
    /// file judged with every ready gate. `filed` says which holders were filed under which hold.
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
        filed: &mut Vec<(String, String)>,
    ) -> anyhow::Result<TrackOutcome> {
        let scope = match target {
            AcquisitionTarget::Shared => LibraryScope::Shared,
            AcquisitionTarget::Member(user) => LibraryScope::Viewer(user.clone()),
        };
        if self
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
            Err(waiting) => return Ok(TrackOutcome::Held(waiting)),
        };
        // A song fetched ahead is kept at once: no outage hold, no Lidarr hand-off. The download
        // takes the file from the folder instead of a transfer.
        if self
            .extras
            .ahead
            .as_ref()
            .is_some_and(|ahead| ahead.has(provider, external_id))
        {
            info!("{provider}:{external_id} was downloaded ahead; keeping that copy");
            let completion = self.direct_queue.enqueue_as(
                Some(origin),
                gates.clone(),
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
                Ok(path) => TrackOutcome::Landed(path),
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
                continue;
            }

            // Soulseek waits out an outage, before the step and again when slskd lost its login
            // partway through it. Every other source, and Soulseek once the wait is over, goes on
            // exactly as before.
            let wait_until = self.soulseek_deadline(&steps, index, held_since_utc);
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
                let completion = self.direct_queue.enqueue_as(
                    Some(origin),
                    gates.clone(),
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
                    Ok(path) => return Ok(TrackOutcome::Landed(path)),
                    Err(failure) => {
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
                if let Some(tracker) = self.tracker() {
                    tracker.fail(provider, external_id, Some(&failure.to_string()));
                }
                return Ok(TrackOutcome::Failed(failure.to_string()));
            }
            // The next enabled source owns the fallback.
        }
        // Only reached when the last source was Lidarr and it said no. It records its own
        // reason first; this is the fallback when it could not.
        const NO_SOURCE: &str = "No download source could get this song.";
        if let Some(tracker) = self.tracker() {
            tracker.fail(provider, external_id, Some(NO_SOURCE));
        }
        Ok(TrackOutcome::Failed(NO_SOURCE.to_string()))
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
        let steps = self.enabled_steps(true);
        for (index, &step) in steps.iter().enumerate() {
            let is_last = index == steps.len() - 1;
            if step == HeartDownloadSource::Lidarr {
                if self
                    .lidarr
                    .try_acquire_album(provider, album_external_id, is_last, requested_by, origin, &[])
                    .await?
                {
                    return Ok(());
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
                match self
                    .direct_downloads
                    .download_album_with_source(
                        provider,
                        album_external_id,
                        to_direct_source(step),
                        !is_last,
                        &CancellationToken::new(),
                        requested_by.map(|r| vec![r.to_string()]),
                    )
                    .await
                {
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
        let Some(owned) = &self.extras.owned else {
            return false;
        };
        let Some((song, copy)) = owned.find_song_in(scope, provider, external_id).await else {
            return false;
        };
        let upgrading = owned.queue_upgrade_if_wanted(&copy, &song, requested_by);
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
        self.settle(provider, external_id, &copy);
        true
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
        if let (Some(held), Some(holds)) = (held, &self.extras.holds) {
            holds.release(&held.key());
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

#[cfg(test)]
#[path = "heart_acquisition_coordinator_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "heart_acquisition_family_tests.rs"]
mod family_tests;

#[cfg(test)]
#[path = "heart_acquisition_hold_tests.rs"]
mod hold_tests;
