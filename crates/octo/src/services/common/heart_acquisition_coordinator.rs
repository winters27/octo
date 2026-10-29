//! The heart acquisition coordinator: which source a heart or a play is handed to.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::settings::{DownloadSource, HeartDownloadSource, SettingsStore};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::TrackAcquisitionQueue;
use super::acquisition_tracker::key_of;
use super::{
    AcquisitionState, AcquisitionTarget, AcquisitionTracker, HeldAcquisition, HeldKind, SoulseekHoldStore,
    StarOnArrival,
};
use crate::services::family::{FamilyPool, LibraryScope, LibraryWriteGate, WriteDecision};
use crate::services::i_download_service::IDownloadService;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::{HeartOwnership, OwnedCopy};
use crate::services::lidarr::ILidarrHeartAcquisitionService;
use crate::services::prefetch::PrefetchedFiles;
use crate::services::soulseek::soulseek_link::SoulseekLinkState;
use crate::services::soulseek::{ExternalIdRegistry, ISoulseekLink};

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
        Arc::new(HeartAcquisitionCoordinator {
            settings,
            direct_queue,
            direct_downloads,
            lidarr,
            extras,
            held_plays: Mutex::new(HashSet::new()),
            lidarr_plays: Mutex::new(HashSet::new()),
            family: OnceLock::new(),
            landed_wait: Mutex::new(LandedSongWait::default()),
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
                .acquire_track_for(&provider, &external_id, requested_by.as_deref(), None, &target)
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
        self.queue_play_as(provider, external_id, requested_by, client_id, owner, false);
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
        self.queue_play_as(provider, external_id, requested_by, client_id, owner, true);
    }

    fn queue_play_as(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
        ahead: bool,
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
            if source == DownloadSource::YouTube || !holds {
                self.queue_play_download(
                    provider,
                    external_id,
                    source,
                    requested_by,
                    client_id,
                    owner,
                    &target,
                );
            } else {
                let coordinator = Arc::clone(self);
                let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
                let (client_id, owner) = (client_id.map(str::to_string), owner.map(str::to_string));
                let target = target.clone();
                tokio::spawn(async move {
                    coordinator
                        .hold_then_queue_play(
                            &provider,
                            &external_id,
                            source,
                            requested_by.as_deref(),
                            client_id.as_deref(),
                            owner.as_deref(),
                            &target,
                        )
                        .await
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

    #[allow(clippy::too_many_arguments)] // the play's facts and whose library it is for
    fn queue_play_download(
        &self,
        provider: &str,
        external_id: &str,
        source: DownloadSource,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
        target: &AcquisitionTarget,
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
    #[allow(clippy::too_many_arguments)] // the play's facts and whose library it is for
    async fn hold_then_queue_play(
        &self,
        provider: &str,
        external_id: &str,
        source: DownloadSource,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
        target: &AcquisitionTarget,
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
            .try_acquire_track(provider, external_id, false, requested_by)
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
                .acquire_album_for_member(&provider, &album_id, requested_by.as_deref(), &user, true)
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
                .acquire_track_outcome(song_provider, external_id, requested_by, None, target)
                .await;
            (song_provider, external_id, outcome)
        });
        let mut landed = Vec::new();
        let mut lidarr = false;
        for (song_provider, external_id, outcome) in futures::future::join_all(chains).await {
            match outcome {
                Ok(TrackOutcome::Landed(_)) => landed.push(key_of(song_provider, external_id)),
                Ok(TrackOutcome::HandedToLidarr) => lidarr = true,
                Ok(TrackOutcome::AlreadyThere | TrackOutcome::Failed(_)) => {}
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
                self.acquire_album_for_member(provider, album_external_id, requested_by, user, false)
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
    /// Lidarr, which fills the shared library). With `star`, each song is starred for them once
    /// it shows in their library. Answers where each song that landed is.
    pub async fn acquire_album_for_member(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        user: &str,
        star: bool,
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
                .acquire_track_outcome(track_provider, track_id, requested_by, None, &target)
                .await
            {
                Ok(TrackOutcome::Landed(path)) => {
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
                .acquire_track(
                    &held.provider,
                    &held.external_id,
                    held.requested_by.as_deref(),
                    Some(held.held_since_utc),
                )
                .await;
            coordinator.resumed(&held, chain);
        });
    }

    pub(crate) fn resume_album(self: &Arc<Self>, held: HeldAcquisition) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let chain = coordinator
                .acquire_album(
                    &held.provider,
                    &held.external_id,
                    held.requested_by.as_deref(),
                    Some(held.held_since_utc),
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

    /// The song chain. `Err` for a Lidarr step that failed.
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
        )
        .await
    }

    /// [`Self::acquire_track`] into one library.
    pub(crate) async fn acquire_track_for(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
        target: &AcquisitionTarget,
    ) -> anyhow::Result<()> {
        self.acquire_track_outcome(provider, external_id, requested_by, held_since_utc, target)
            .await
            .map(|_| ())
    }

    /// The song chain into one library, answering how it ended. A member's chain skips Lidarr,
    /// which fills the shared library.
    pub async fn acquire_track_outcome(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
        target: &AcquisitionTarget,
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
        // A song fetched ahead is kept at once: no outage hold, no Lidarr hand-off. The download
        // takes the file from the folder instead of a transfer.
        if self
            .extras
            .ahead
            .as_ref()
            .is_some_and(|ahead| ahead.has(provider, external_id))
        {
            info!("{provider}:{external_id} was downloaded ahead; keeping that copy");
            let completion = self.direct_queue.enqueue_for(
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
                    .try_acquire_track(provider, external_id, is_last, requested_by)
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
                self.hold_for_soulseek(HeldKind::Track, provider, external_id, requested_by, wait_until)
                    .await;
                let completion = self.direct_queue.enqueue_for(
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

    /// The album chain. `Err` for a Lidarr step that failed.
    pub(crate) async fn acquire_album(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
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
                    .try_acquire_album(provider, album_external_id, is_last, requested_by)
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

        let held = self.extras.holds.as_ref().map(|holds| {
            holds.hold(HeldAcquisition {
                kind,
                provider: provider.to_string(),
                external_id: id.to_string(),
                requested_by: requested_by.map(str::to_string),
                held_since_utc: deadline - link.hold_limit(),
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
