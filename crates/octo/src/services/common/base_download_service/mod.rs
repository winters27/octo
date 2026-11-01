//! The download pipeline every source shares.
//!
//! The shared machinery is [`BaseDownloadService`], and the source-specific parts (Soulseek's
//! download service, for one) are the [`DownloadBackend`] trait it calls back into. A backend
//! gets the base on every call, so it reaches the shared members (`download_path`, `track`,
//! `notifications`, `services`, ...). `BaseDownloadService` implements [`IDownloadService`].
//!
//! The optional services a backend can reach are the fields of [`DownloadServices`], each `None`
//! when the host has no such service.
//!
//! The pipeline is split by topic: this module holds the download, the album walk and the
//! bookkeeping; `placement` where a file goes; `tagging` what it is and what is written to it.

mod placement;
mod tagging;

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "placement_tests.rs"]
mod placement_tests;

#[cfg(test)]
#[path = "tagging_tests.rs"]
mod tagging_tests;

#[cfg(test)]
#[path = "ownership_tests.rs"]
mod ownership_tests;

#[cfg(test)]
#[path = "parallel_lock_split_tests.rs"]
mod parallel_lock_split_tests;

#[cfg(test)]
#[path = "family_tests.rs"]
mod family_tests;

#[cfg(test)]
#[path = "prefetch_tests.rs"]
mod prefetch_tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt, TryStreamExt};
use octo_core::common::{PathHelper, SongIdentity, dotnet};
use octo_core::fingerprint::VerificationResult;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardSource, HeartGate};
use octo_core::fingerprint::verification::RecordingProof;
use octo_core::models::domain::{Album, Song};
use octo_core::models::download::{AcquisitionHeard, DownloadHistoryEntry, DownloadInfo, DownloadStatus};
use octo_core::notifications::{NotificationEvent, NotificationEventType};
use octo_core::settings::{
    DownloadMode, DownloadSource, LibraryAction, SettingsStore, SoulseekSettings, StorageMode,
    SubsonicSettings,
};
use octo_core::tagging::{AlbumTagContext, ReleaseIdentifier};
use octo_media::audio::{ILoudnessMeter, Loudness};
use octo_media::tags::TagWriterExtras;
use parking_lot::Mutex;
use tokio::sync::{Semaphore, SemaphorePermit, watch};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::services::common::acquisition_tracker::{acquisition_event_kinds, key_of};
use crate::services::common::{
    AcquisitionHeld, AcquisitionPlace, AcquisitionState, AcquisitionTracker, AlbumGates, DownloadArrivals,
    DownloadConcurrency, DownloadPicks, GateLedger, SoundDeny, acquisition_key,
};
use crate::services::cover_art::DownloadCoverResolver;
use crate::services::family::FamilyPlacement;
use crate::services::fingerprint::MusicBrainzClient;
use crate::services::i_download_service::{Ahead, AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::LastFmService;
use crate::services::library::library_write_gate::LibraryWriteGate;
use crate::services::library::{
    LibraryActionJournal, LibraryOwnership, OwnedCopy, OwnedDecision, ReplacementHandoff,
    ReplacementRejectedException, UpgradeAsk, UpgradeQueue, UpgradeSources,
};
use crate::services::local::{DownloadHistoryService, ILocalLibraryService, KeptHeart};
use crate::services::lyrics::LyricsSidecarWriter;
use crate::services::metadata::DeezerMetadataService;
use crate::services::notifications::NotificationService;
use crate::services::prefetch::{AheadSidecar, PrefetchedFiles, write_sidecar};
use crate::services::subsonic::NavidromeIdentityService;

pub use placement::{LayoutChoice, Placement};
pub use tagging::{CatalogBlanks, MeterPreview};

/// The name a download was asked for under, captured before anything corrects it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestedIdentity {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub track: Option<i32>,
    /// For a download asked for with no album: the release group's id and title its ISRC
    /// picked, set once the pick has finished.
    pub album_pick: Arc<OnceLock<(String, String)>>,
}

impl RequestedIdentity {
    pub fn new(artist: &str, title: &str, album: &str, track: Option<i32>) -> Self {
        Self {
            artist: artist.to_string(),
            title: title.to_string(),
            album: album.to_string(),
            track,
            album_pick: Arc::default(),
        }
    }
}

/// A download's file was not on disk (from `ensure_on_disk`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FileNotFoundException {
    pub message: String,
    pub path: Option<String>,
}

/// The caller's token was canceled while the download waited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("The operation was canceled.")]
pub struct OperationCanceled;

/// One transfer a backend runs: the download's arguments, the song it may correct as it goes (the
/// peer, the verdict, the transcode), and the per-song facts kept beside the `Song`.
pub struct TrackDownload {
    /// External track ID.
    pub track_id: String,
    /// Song metadata, which the backend may change; the base carries on with it.
    pub song: Song,
    /// Mute per-track notifications (album walk, cache fills).
    pub suppress_notify: bool,
    pub source_override: Option<DownloadSource>,
    pub upgrade_search: bool,
    /// The library file this download replaces, or `None` for an ordinary download. It travels
    /// with the transfer, so a backend that can use it (Lidarr reads the album from its tags)
    /// sees it without a new parameter.
    pub replacing_path: Option<String>,
    /// The source a backend fetched the song from, when the format alone does not say: a FLAC can
    /// be Lidarr's as well as Soulseek's.
    pub fetched_from: Option<String>,
    /// The backend asked for the song to land without a notice of its own (a Lidarr album's songs
    /// that nobody hearted, and the songs of an album heart, which gets one for the album).
    pub muted: bool,
    /// Only FLAC files are candidates; the base still refuses one made from lossy audio.
    pub flac_only: bool,
    /// A download ahead of a play, which leaves an album walk's queued files to the walk.
    pub ahead: bool,
    /// The files a resumed hold held: checked first, as a fresh download's file is.
    pub held_files: Vec<String>,
    /// Those of `held_files` Lidarr brought, each with where its labels are read from.
    pub held_lidarr: Vec<crate::services::common::soulseek_hold_store::HeldLidarrFile>,
    /// Why the acquisition runs; None for one queued without an origin (the shipped checks).
    pub origin: Option<AcquisitionOrigin>,
    /// The gate of each heart or play that asked: a file is judged against each on its own.
    pub gates: Vec<HeartGate>,
    /// The acquisition's key, which finds each person's open heard-master round.
    pub acquisition_key: String,
    /// Where each gate's verdict on the file is written.
    pub ledger: GateLedger,
    /// Everyone who asked for the song, as the request had them when it started.
    pub requested_by: Vec<String>,
    /// What a replacement refuses by its original's sound.
    pub deny: Option<SoundDeny>,
    /// A person's keep: a file the hold kept lands as it is, or the very audio they heard, and
    /// nothing is searched for.
    pub keep_anyway: bool,
}

/// The source-specific parts of the download pipeline. Every call is handed the base, so a
/// backend reaches the shared members through it.
#[async_trait]
pub trait DownloadBackend: Send + Sync + 'static {
    /// The provider name (e.g., "deezer", "qobuz").
    fn provider_name(&self) -> &str;

    /// Checks if the service is properly configured and functional.
    async fn is_available(&self, base: &BaseDownloadService) -> bool;

    /// A direct stream from the provider CDN (true streaming, no disk). The default is not
    /// supported.
    async fn get_direct_stream(
        &self,
        _base: &BaseDownloadService,
        _external_provider: &str,
        _external_id: &str,
        _range_header: Option<&str>,
        _cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        Ok(None)
    }

    /// Downloads a track and saves it to disk. Backends implement provider-specific logic
    /// (encryption, authentication, etc.). Returns the local file path where the track landed;
    /// the base places, tags and registers it from there.
    async fn download_track(
        &self,
        base: &BaseDownloadService,
        download: &mut TrackDownload,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<String>;

    /// The external album ID from the internal album ID format.
    /// Example: "ext-deezer-album-123456" -> "123456".
    fn extract_external_id_from_album_id(&self, album_id: &str) -> Option<String>;

    /// Re-assert any routing state a long-running batch depends on. An album download can run for
    /// hours while searches keep filling the id registry, so a track's routing can be evicted
    /// before its turn comes. Ids are a pure hash of the routing fields, so re-registering the
    /// SAME fields restores the SAME id and is idempotent.
    fn ensure_routing_registered(&self, _track: &Song) {}

    /// Queue what an album walk can take from one place in one go, before the walk starts, and
    /// say which tracks that covers. Each of those tracks still goes through its own download,
    /// which takes the queued file first. The default takes nothing, so the walk is song by song.
    async fn prepare_album(
        &self,
        _base: &BaseDownloadService,
        _album: &Album,
        _tracks: &[Song],
        _source: Option<DownloadSource>,
        _cancellation_token: &CancellationToken,
    ) -> Vec<String> {
        Vec::new()
    }

    /// Called when the walk ends, however it ends: let go of whatever was prepared and not used.
    async fn finish_album(&self, _base: &BaseDownloadService, _prepared: &[String]) {}

    /// Judges a song fetched ahead before a heart or a play keeps it, as its gates judge a fresh
    /// download: true keeps it (the song carries the verdict it is tagged by); false passes it
    /// over, and the download runs. The default keeps it, as every download always did.
    async fn judge_ahead(
        &self,
        _base: &BaseDownloadService,
        _download: &mut TrackDownload,
        _path: &str,
        _identity: HeardSource,
    ) -> bool {
        true
    }

    /// A song fetched ahead kept for a download nobody gates: each heart or play that joined it is
    /// judged by its own gate, so it counts the copy as its own only when that gate passes it. The
    /// default judges nothing.
    async fn judge_ahead_joiners(&self, _download: &TrackDownload, _path: &str, _identity: HeardSource) {}
}

/// The notice queue's review: ask one person about a download they can settle by listening. False
/// when it was already asked.
pub trait ReviewQueue: Send + Sync {
    fn add_review(&self, owner: &str, local_path: &str, song: &Song, verdict: &VerificationResult) -> bool;
}

/// The two playlist sync calls a download makes.
#[async_trait]
pub trait PlaylistTrackSync: Send + Sync {
    fn get_playlist_id_for_track(&self, song_id: &str) -> Option<String>;

    async fn add_track_to_m3u(
        &self,
        playlist_id: &str,
        song: &Song,
        local_path: &str,
        is_full_playlist_download: bool,
    ) -> anyhow::Result<()>;
}

/// The services every download needs.
pub struct DownloadCore {
    /// The Subsonic, genre, Soulseek, metadata and library action settings, all read at use;
    /// and the raw `Library:DownloadPath`.
    pub settings: Arc<SettingsStore>,
    pub local_library: Arc<dyn ILocalLibraryService>,
    pub metadata: Arc<dyn IMusicMetadataService>,
    pub navidrome_identity: NavidromeIdentityService,
    pub history: Arc<DownloadHistoryService>,
    pub notifications: Arc<NotificationService>,
}

/// The optional services. `None` is a service the host did not register, which the base copes
/// without.
#[derive(Clone, Default)]
pub struct DownloadServices {
    /// How many transfers may run at once, and the gate they share. `None` is one at a time.
    pub concurrency: Option<Arc<DownloadConcurrency>>,
    /// The live progress list.
    pub tracker: Option<Arc<AcquisitionTracker>>,
    pub ownership: Option<Arc<LibraryOwnership>>,
    pub upgrade_queue: Option<Arc<UpgradeQueue>>,
    pub upgrade_sources: Option<Arc<UpgradeSources>>,
    pub action_journal: Option<Arc<LibraryActionJournal>>,
    /// The notice queue, which asks a person to review a download.
    pub review_queue: Option<Arc<dyn ReviewQueue>>,
    /// The playlist sync.
    pub playlist_sync: Option<Arc<dyn PlaylistTrackSync>>,
    pub deezer: Option<Arc<DeezerMetadataService>>,
    pub music_brainz: Option<Arc<MusicBrainzClient>>,
    /// Built on the spot from the catalog and the music database above when `None`.
    pub release_identifier: Option<Arc<ReleaseIdentifier>>,
    pub loudness_meter: Option<Arc<dyn ILoudnessMeter>>,
    pub cover_resolver: Option<Arc<DownloadCoverResolver>>,
    pub lyrics: Option<Arc<LyricsSidecarWriter>>,
    pub last_fm: Option<Arc<LastFmService>>,
    /// Copies picked in Find songs, taken instead of a search.
    pub picks: Option<Arc<DownloadPicks>>,
    /// The music folder's one-writer gate: a placement waits while a library job holds it.
    pub write_gate: Option<LibraryWriteGate>,
    /// Octo's follows of its Lidarr searches: a file placed under Lidarr's root folders for a
    /// followed artist waits in the hold path until the follow ends.
    pub lidarr_pending: Option<Arc<crate::services::lidarr::LidarrPendingAlbums>>,
    /// The family pool, where members' downloads land. `None`, or Family off, places every
    /// download in the shared library as always.
    pub family: Option<Arc<dyn FamilyPlacement>>,
    /// Told of every song whose file lands in the music folder.
    pub arrivals: Option<Arc<DownloadArrivals>>,
    /// The songs fetched ahead of their play, which a heart takes instead of downloading again.
    pub prefetched: Option<Arc<dyn PrefetchedFiles>>,
    /// The gates of the album chains that run now: a walk of a hearted album files each track by
    /// its own sound against its own recording.
    pub album_gates: Option<Arc<AlbumGates>>,
}

/// Everything a single download is asked with beyond its provider and id: the optional
/// parameters of a download.
#[derive(Clone, Default)]
pub struct DownloadOptions {
    /// Start the album walk once this track lands, in Album mode.
    pub trigger_album_download: bool,
    /// Treat this as a permanent download regardless of Cache storage mode. Cache mode
    /// otherwise skips library registration, the fetched-songs log and the rescan, which is
    /// wrong for a deliberate "keep this" gesture like hearting an album.
    pub force_permanent: bool,
    /// Mute per-track notifications. Set by the album walk, which fires one summary at the end
    /// instead of a ping per track.
    pub suppress_notify: bool,
    pub source_override: Option<DownloadSource>,
    /// The users who asked for this track; `None` or empty when nobody in particular did.
    pub requested_by: Option<Vec<String>>,
    /// The album walk this track belongs to.
    pub album_context: Option<Arc<AlbumTagContext<Loudness>>>,
    /// Search Soulseek the slow, wide way (Better quality, weekly upgrade).
    pub upgrade_search: bool,
    /// A library action's replacement, revealed in the original's place.
    pub replacement: Option<Arc<ReplacementHandoff>>,
    /// Where the file goes (the family pool for members) and whether only a real FLAC will do.
    pub place: AcquisitionPlace,
    /// Fetch the song ahead of its play into `ahead_folder`, outside the library: checked and
    /// tagged, and nothing else (no registration, fetched-songs log, notice or scan).
    pub prefetch: bool,
    pub ahead_folder: Option<PathBuf>,
}

/// A song being fetched ahead is not waited for by another download ahead of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("This song is already being downloaded")]
pub struct AheadBusy;

/// Why a download that had to be a real FLAC copy failed: no FLAC came, or the one that came
/// was made from lossy audio.
pub const NO_REAL_FLAC: &str = "No real FLAC copy found";

/// What a landed file was kept for: the people whose own gate passed it, the recording their gate
/// judged it against and what they heard, and for a file kept as heard its own print. None when
/// no gate judged it (the shipped checks kept it, as before).
fn kept_heart(place: &AcquisitionPlace, song: &Song) -> Option<KeptHeart> {
    let accepted: Vec<&HeartGate> = place
        .gates
        .iter()
        .filter(|gate| place.ledger.accepted(&gate.user))
        .collect();
    let first = accepted.first()?;
    let verification = song.verification.as_deref();
    // A file kept anyway is the heard audio itself when its person's gate heard something: the
    // keep lands nothing else for them.
    let as_heard = verification.is_some_and(|verification| match verification.proof {
        RecordingProof::AsHeard { .. } => true,
        RecordingProof::KeptByYou => first.heard.is_some(),
        _ => false,
    });
    let mut users: Vec<String> = Vec::new();
    for gate in &accepted {
        let user = gate.user.trim();
        if !user.is_empty() && !users.iter().any(|known| dotnet::eq_ignore_case(known, user)) {
            users.push(user.to_string());
        }
    }
    Some(KeptHeart {
        users,
        recording: first.r.clone(),
        heard: first.heard.clone(),
        as_heard,
        print: verification
            .and_then(|verification| verification.fingerprint.clone())
            .filter(|print| !print.is_empty()),
        seconds: verification
            .map(|verification| verification.duration_seconds)
            .filter(|seconds| *seconds > 0),
    })
}

/// Whether a gate of this place heard a copy fetched ahead, which a person's keep may take as the
/// very audio they heard.
fn heard_an_ahead_copy(place: &AcquisitionPlace) -> bool {
    place.gates.iter().any(|gate| {
        matches!(
            gate.heard.as_ref().map(|heard| &heard.source),
            Some(HeardSource::Copy { .. })
        )
    })
}

/// A file the person kept anyway. What its own check proved by sound stays proven; anything less
/// is marked "kept by you, version not proven", claims no recording, no ISRC and no match, and is
/// named as the request names it.
fn kept_by_you(song: &mut Song) {
    let mut verification = song.verification.take().map(|kept| *kept).unwrap_or_default();
    if verification.proof != RecordingProof::Proven {
        verification.proof = RecordingProof::KeptByYou;
        verification.verdict = octo_core::fingerprint::VerificationVerdict::Inconclusive;
        verification.recording_id = None;
        verification.candidate_recording_id = None;
        verification.r#match = None;
        verification.tags_authoritative = false;
        song.isrc = None;
        song.music_brainz_recording_id = None;
    }
    song.verification = Some(Box::new(verification));
}

/// Whether a path names a FLAC file.
fn is_flac_path(path: &str) -> bool {
    dotnet::eq_ignore_case(get_extension(path), ".flac")
}

/// A walk in [`BaseDownloadService::album_walks`]: the source and the people it walks for, and
/// its answer once it ends.
struct RunningWalk {
    source: Option<DownloadSource>,
    requested_by: Option<Vec<String>>,
    /// The track the walk leaves out, or empty for none.
    exclude: String,
    answer: watch::Receiver<Option<bool>>,
}

/// An album's place in [`BaseDownloadService::album_walks`], given up when its walk ends, however
/// it ends.
struct AlbumWalkEntry<'a> {
    walks: &'a Mutex<HashMap<String, RunningWalk>>,
    album_external_id: &'a str,
}

impl Drop for AlbumWalkEntry<'_> {
    fn drop(&mut self) {
        self.walks.lock().remove(self.album_external_id);
    }
}

/// Waits for a walk to end and gives its answer, `None` when it ended without one (it panicked or
/// its caller left). Its answer's sender goes only after its place is given up, so the album is
/// free by the time this returns.
async fn walk_ended(running: &mut watch::Receiver<Option<bool>>) -> Option<bool> {
    while running.changed().await.is_ok() {}
    *running.borrow()
}

/// Abstract base class for download services, as a struct: common download logic, tracking,
/// and metadata writing. The backend implements provider-specific download and authentication.
pub struct BaseDownloadService {
    me: Weak<BaseDownloadService>,
    backend: Arc<dyn DownloadBackend>,
    settings: Arc<SettingsStore>,
    local_library: Arc<dyn ILocalLibraryService>,
    metadata: Arc<dyn IMusicMetadataService>,
    navidrome_identity: NavidromeIdentityService,
    history: Arc<DownloadHistoryService>,
    notifications: Arc<NotificationService>,
    services: DownloadServices,

    /// The configured Library:DownloadPath. With auto-detect on this is only a fallback used
    /// until Navidrome's real music folder is detected. Read once, at construction.
    configured_download_path: String,
    cache_path: String,

    /// Shared because the album walk reads this WITHOUT holding the download lock while a
    /// request can be writing to it under the lock.
    active_downloads: Mutex<HashMap<String, DownloadInfo>>,
    /// The albums being fetched song by song outside a walk, newest last: their tracks are
    /// filed and tagged as one walk's would be.
    held_albums: Mutex<Vec<Arc<AlbumTagContext<Loudness>>>>,
    /// The albums being walked, by external id, each with its walk's source, people and answer. A
    /// walk asked for while one of the same album runs waits for that one: two walks side by side
    /// would each queue a batch for the same songs, and the second would orphan the first.
    album_walks: Mutex<HashMap<String, RunningWalk>>,
    /// One permit: one download at a time inside the lock.
    download_lock: Semaphore,

    identifier: OnceLock<Arc<ReleaseIdentifier>>,

    /// The cover lines written to the downloads log, for tests.
    #[cfg(test)]
    pub(crate) cover_steps: Mutex<Vec<(String, Option<String>)>>,
}

impl BaseDownloadService {
    /// The finalize phase runs under the download lock, so past this it is logged with its stage
    /// timings. Nothing is cut short beyond the per-stage caps.
    const FINALIZE_BUDGET: Duration = Duration::from_secs(20);

    pub fn new(
        core: DownloadCore,
        services: DownloadServices,
        backend: Arc<dyn DownloadBackend>,
    ) -> Arc<Self> {
        let configured_download_path = core
            .settings
            .raw("Library:DownloadPath")
            .unwrap_or_else(|| "./downloads".to_string());
        let cache_path = PathHelper::get_cache_path();

        // A drive-letter path inside a Linux container is a config mistake the filesystem
        // hides: create_dir_all below happily makes a literal directory named "E:\Media\Music"
        // and downloads vanish into it.
        if PathHelper::looks_like_windows_drive_path(Some(&configured_download_path)) {
            warn!(
                "Library:DownloadPath is the Windows path '{configured_download_path}' but this host is not Windows. \
                 It will be treated as a literal directory name. Use the container path instead \
                 (normally /music) and move the library via the DOWNLOAD_PATH bind mount."
            );
        }

        let service = Arc::new_cyclic(|me| BaseDownloadService {
            me: me.clone(),
            backend,
            settings: core.settings,
            local_library: core.local_library,
            metadata: core.metadata,
            navidrome_identity: core.navidrome_identity,
            history: core.history,
            notifications: core.notifications,
            services,
            configured_download_path,
            cache_path,
            active_downloads: Mutex::new(HashMap::new()),
            held_albums: Mutex::new(Vec::new()),
            album_walks: Mutex::new(HashMap::new()),
            download_lock: Semaphore::new(1),
            identifier: OnceLock::new(),
            #[cfg(test)]
            cover_steps: Mutex::new(Vec::new()),
        });

        for dir in [service.download_path(), service.cache_path.clone()] {
            if !dir.is_empty()
                && !std::path::Path::new(&dir).is_dir()
                && let Err(e) = std::fs::create_dir_all(&dir)
            {
                // Not fatal: a host without its music folder still starts, and the first
                // download says why it cannot place anything.
                error!("Failed to create directory: {dir}: {e}");
            }
        }
        service
    }

    // ---- what a backend reaches ----------------------------------------------------------

    /// This service as an `Arc`, for work that outlives the call (the album walk, the rescan).
    pub fn arc(&self) -> Arc<BaseDownloadService> {
        self.me
            .upgrade()
            .expect("the service is alive while it is called")
    }

    pub fn backend(&self) -> &Arc<dyn DownloadBackend> {
        &self.backend
    }

    pub fn provider_name(&self) -> &str {
        self.backend.provider_name()
    }

    /// Effective download destination. Resolves fresh each access so that once Navidrome's
    /// music folder is detected, downloads follow it without a restart.
    pub fn download_path(&self) -> String {
        self.navidrome_identity
            .effective_download_path(&self.configured_download_path)
    }

    pub fn cache_path(&self) -> &str {
        &self.cache_path
    }

    pub fn settings(&self) -> &Arc<SettingsStore> {
        &self.settings
    }

    /// `SubsonicSettings`: read through the monitor, never captured, so the admin UI's download
    /// source, storage mode and folder structure reach the download path without a restart.
    pub fn subsonic_settings(&self) -> SubsonicSettings {
        self.settings.current().subsonic.clone()
    }

    /// `CurrentSoulseekSettings`: the Soulseek settings as they are now, for switches a backend
    /// reads live.
    pub fn current_soulseek_settings(&self) -> SoulseekSettings {
        self.settings.current().soulseek.clone()
    }

    pub fn local_library(&self) -> &Arc<dyn ILocalLibraryService> {
        &self.local_library
    }

    pub fn metadata_service(&self) -> &Arc<dyn IMusicMetadataService> {
        &self.metadata
    }

    pub fn notifications(&self) -> &Arc<NotificationService> {
        &self.notifications
    }

    pub fn services(&self) -> &DownloadServices {
        &self.services
    }

    /// `Concurrency`: how many transfers may run at once, and the gate they share.
    pub fn concurrency(&self) -> Option<&Arc<DownloadConcurrency>> {
        self.services.concurrency.as_ref()
    }

    /// `DownloadLock`: placing, tagging and registering touch the library and the mapping file,
    /// and those stay one at a time.
    pub fn download_lock(&self) -> &Semaphore {
        &self.download_lock
    }

    /// Tell the live progress list where a download has got to. It only watches, and a
    /// bookkeeping step never fails or slows a download.
    pub fn track(&self, step: impl FnOnce(&Arc<AcquisitionTracker>)) {
        if let Some(tracker) = &self.services.tracker {
            step(tracker);
        }
    }

    /// One line in the downloads log of this source's row for a song. Watching only, like
    /// [`Self::track`].
    pub fn log_step(&self, external_id: &str, kind: &str, text: &str, detail: Option<&str>) {
        self.log_step_heard(external_id, kind, text, detail, Vec::new());
    }

    /// The same with the other recordings AcoustID heard in the file.
    pub fn log_step_heard(
        &self,
        external_id: &str,
        kind: &str,
        text: &str,
        detail: Option<&str>,
        heard: Vec<AcquisitionHeard>,
    ) {
        let provider = self.provider_name().to_string();
        self.track(|t| t.log_heard(&provider, external_id, kind, text, detail, heard));
    }

    /// The same for a song that carries its own outside id.
    pub(crate) fn log_song_step(&self, song: &Song, kind: &str, text: &str, detail: Option<&str>) {
        self.log_song_step_heard(song, kind, text, detail, Vec::new());
    }

    /// The same for a song that carries its own outside id, with the other recordings AcoustID
    /// heard in the file.
    pub(crate) fn log_song_step_heard(
        &self,
        song: &Song,
        kind: &str,
        text: &str,
        detail: Option<&str>,
        heard: Vec<AcquisitionHeard>,
    ) {
        if let (Some(provider), Some(id)) = (
            song.external_provider.as_deref().filter(|p| !p.is_empty()),
            song.external_id.as_deref().filter(|i| !i.is_empty()),
        ) {
            self.track(|t| t.log_heard(provider, id, kind, text, detail, heard));
        }
    }

    /// The download record of a song id, if one is active.
    pub fn active_download(&self, song_id: &str) -> Option<DownloadInfo> {
        self.active_downloads.lock().get(song_id).cloned()
    }

    // ---- the download ---------------------------------------------------------------------

    /// Internal method for downloading a song with control over album download triggering.
    pub async fn download_song_internal(
        &self,
        external_provider: &str,
        external_id: &str,
        mut options: DownloadOptions,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<String> {
        if external_provider != self.provider_name() {
            anyhow::bail!("Provider '{external_provider}' is not supported");
        }

        let song_id = format!("ext-{external_provider}-{external_id}");
        let is_cache = !options.force_permanent
            && !options.prefetch
            && self.settings.current().subsonic.storage_mode == StorageMode::Cache;
        // A track of an album being fetched song by song is one of that album's, as in a walk.
        if options.album_context.is_none()
            && !is_cache
            && !options.prefetch
            && options.replacement.is_none()
            && !options.upgrade_search
        {
            options.album_context = self.held_album(external_id);
        }
        // Cache-mode fills are background plumbing, not a user gesture: they skip the
        // fetched-songs log, so they skip notifications for the same reason. So do songs fetched
        // ahead.
        let silence = options.suppress_notify || is_cache || options.prefetch;

        // Acquire the lock BEFORE checking existence to prevent races with concurrent requests.
        // The in-progress branch lets go early so it can wait without holding it; dropping the
        // permit is the release, so it can never be released twice.
        let mut permit = Some(self.lock(cancellation_token).await?);

        // A download ahead has no row of its own; its lines go only to a heart's row that runs.
        let _hushed = options
            .prefetch
            .then(|| Hushed::new(self.services.tracker.as_ref(), external_provider, external_id));
        let outcome = self
            .download_locked(
                external_provider,
                external_id,
                &song_id,
                &options,
                is_cache,
                silence,
                &mut permit,
                cancellation_token,
            )
            .await;
        match outcome {
            Ok(path) => Ok(path),
            // Another download of the song is running: it is left alone.
            Err(e) if e.downcast_ref::<AheadBusy>().is_some() => Err(e),
            Err(e) => {
                // A download ahead leaves no record behind, so a heart waiting on it goes on to
                // get the song itself.
                if options.prefetch {
                    self.active_downloads.lock().remove(&song_id);
                    info!("Downloading {song_id} ahead failed: {e:#}");
                    return Err(e);
                }
                if let Some(info) = self.active_downloads.lock().get_mut(&song_id) {
                    info.status = DownloadStatus::Failed;
                    info.error_message = Some(e.to_string());
                }
                if e.downcast_ref::<ReplacementRejectedException>().is_some() {
                    info!("Replacement download {song_id} refused: {e}");
                } else {
                    error!("Download failed for {song_id}: {e:#}");
                }
                Err(e)
            }
        }
    }

    async fn lock(&self, cancellation_token: &CancellationToken) -> anyhow::Result<SemaphorePermit<'_>> {
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => Err(OperationCanceled.into()),
            permit = self.download_lock.acquire() => Ok(permit.expect("the download lock is never closed")),
        }
    }

    /// The song's file already here for this download. A heart's counts only a file that is
    /// theirs: one their own gate passed before, or the very library file one of them heard,
    /// which is already theirs and is never downloaded again. Another person's version, or a file
    /// nobody proved, is never theirs: their gate judges what is fetched instead. Every other
    /// download, a play's among them (a play never downloads over a copy that is here), takes the
    /// song's file as before.
    async fn already_downloaded(
        &self,
        external_provider: &str,
        external_id: &str,
        place: &AcquisitionPlace,
    ) -> Option<String> {
        let personal = matches!(
            place.origin,
            Some(AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart)
        ) && !place.gates.is_empty();
        if !personal {
            return self
                .local_library
                .get_local_path_for_external_song(external_provider, external_id)
                .await;
        }
        let heard_file =
            place
                .gates
                .iter()
                .find_map(|gate| match gate.heard.as_ref().map(|heard| &heard.source) {
                    Some(HeardSource::Library { path } | HeardSource::Acquired { path })
                        if file_exists(path) =>
                    {
                        Some(path.clone())
                    }
                    _ => None,
                });
        if heard_file.is_some() {
            return heard_file;
        }
        self.local_library
            .get_mappings()
            .await
            .into_iter()
            .filter(|mapping| {
                mapping.external_provider == external_provider
                    && mapping.external_id == external_id
                    && mapping
                        .heart
                        .as_ref()
                        .is_some_and(|heart| place.gates.iter().any(|gate| heart.is_for(&gate.user)))
            })
            .map(|mapping| mapping.local_path)
            .find(|path| file_exists(path))
    }

    #[allow(clippy::too_many_arguments)] // the locked half of one download, with all its inputs
    async fn download_locked<'a>(
        &'a self,
        external_provider: &str,
        external_id: &str,
        song_id: &str,
        options: &DownloadOptions,
        is_cache: bool,
        silence: bool,
        permit: &mut Option<SemaphorePermit<'a>>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<String> {
        // The family pool, while Family is on. A member's download lands there and is linked
        // into their library; with no members asking, the file goes to the shared library.
        let family = self.services.family.as_ref().filter(|f| f.pool_root().is_some());
        let pool_members = options.place.pool_members.clone().filter(|_| family.is_some());
        let flac_only = options.place.flac_only;
        // A member's download waits while the pool folder is not the disk Octo placed members'
        // downloads on (its disk is not mounted).
        if let Some(family) = family
            && pool_members.is_some()
            && !family.placement_ready().await
        {
            anyhow::bail!("{}", crate::services::family::pool::POOL_NOT_READY);
        }

        // Check if already downloaded (skipped for cache mode, which checks the cache folder).
        // A replacement is always a fresh file: an existing one is what is being replaced.
        if !is_cache && options.replacement.is_none() {
            let mut existing = self
                .already_downloaded(external_provider, external_id, &options.place)
                .await
                .filter(|path| file_exists(path))
                .filter(|path| {
                    !flac_only || (is_flac_path(path) && !family.is_some_and(|f| f.made_from_lossy(path)))
                });
            // The owner's copy is never the pool's: a member's copy (with Family on or off) moves
            // into the shared library, still one file, or the owner's copy is fetched as usual.
            // A song fetched ahead moves nothing: any copy on disk means it is there.
            if let Some(path) = existing.take() {
                existing = match (&self.services.family, &pool_members) {
                    (Some(placement), None) if !options.prefetch => placement.for_owner(&path).await,
                    _ => Some(path),
                };
            }
            if let Some(existing) = existing {
                info!("Song already downloaded: {existing}");
                // Navidrome shows a member's song at their link, never in the pool.
                let shown = family
                    .zip(pool_members.as_ref().and_then(|members| members.first()))
                    .and_then(|(family, first)| family.link_path_for(&existing, first))
                    .unwrap_or_else(|| existing.clone());
                self.track(|t| {
                    t.already_in_library(external_provider, external_id, None, None, Some(&shown))
                });
                return Ok(existing);
            }
        } else if is_cache
            && let Some(cached) = self
                .get_cached_file_path(external_provider, external_id)
                .filter(|path| file_exists(path))
        {
            info!("Song found in cache: {cached}");
            // Update file access time for cache cleanup logic.
            touch_access_time(&cached);
            self.track(|t| t.complete(external_provider, external_id, None));
            return Ok(cached);
        }

        // Check if download in progress.
        if self.is_in_progress(song_id) {
            if options.prefetch {
                return Err(AheadBusy.into());
            }
            info!("Download already in progress for {song_id}, waiting...");
            // Let go of the lock while waiting.
            *permit = None;
            while self.is_in_progress(song_id) {
                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => return Err(OperationCanceled.into()),
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
            }
            let active = self.active_download(song_id);
            if let Some(DownloadInfo {
                status: DownloadStatus::Completed,
                local_path: Some(path),
                ..
            }) = &active
            {
                return Ok(path.clone());
            }
            // A download ahead ended, leaving no record: its file (if it got one) is in the
            // folder, so this download starts over, and takes it from there.
            if active.is_none() {
                *permit = Some(self.lock(cancellation_token).await?);
                return Box::pin(self.download_locked(
                    external_provider,
                    external_id,
                    song_id,
                    options,
                    is_cache,
                    silence,
                    permit,
                    cancellation_token,
                ))
                .await;
            }
            anyhow::bail!(
                "{}",
                active
                    .and_then(|a| a.error_message)
                    .unwrap_or_else(|| "Download failed".to_string())
            );
        }

        // The worker has it now. Looking the song up is the first part of the search.
        self.track(|t| {
            t.stage(
                external_provider,
                external_id,
                AcquisitionState::Searching,
                None,
                None,
            )
        });

        // In Album mode, fetch the full album first to ensure AlbumArtist is correctly set.
        let mut song: Option<Song> = None;
        if self.settings.current().subsonic.download_mode == DownloadMode::Album
            && let Some(temp) = self.metadata.get_song(external_provider, external_id).await
            && let Some(album_id) = temp.album_id.as_deref().filter(|id| !id.is_empty())
            && let Some(album_external_id) = self
                .backend
                .extract_external_id_from_album_id(album_id)
                .filter(|id| !id.is_empty())
            && let Some(album) = self
                .metadata
                .get_album(external_provider, &album_external_id)
                .await
        {
            // Find the track in the album.
            song = album
                .songs
                .into_iter()
                .find(|s| s.external_id.as_deref() == Some(external_id));
        }

        // Fallback to individual song fetch if not in Album mode or album fetch failed.
        if song.is_none() {
            song = self.metadata.get_song(external_provider, external_id).await;
        }
        let Some(mut song) = song else {
            anyhow::bail!("Song not found");
        };
        // A track of an album being fetched whole is filed by that album: the song an id resolves
        // to names whichever album last listed its recording, and a larger edition holding the
        // same recording lists it at another place.
        if let Some(context) = &options.album_context {
            let listed_as = song.album.clone();
            if context.place(external_id, &mut song)
                && !dotnet::is_blank(&listed_as)
                && SongIdentity::key(&listed_as) != SongIdentity::key(&song.album)
            {
                info!(
                    "'{} - {}' is filed under '{}', the album being fetched, not '{listed_as}'",
                    song.artist, song.title, song.album
                );
            }
        }
        self.track(|t| {
            t.describe(
                external_provider,
                external_id,
                Some(&song.artist),
                Some(&song.title),
                Some(&song.album),
            )
        });

        // Never a second copy of a song already in the library; a lossy one is queued for a
        // higher quality copy instead. Not for a replacement or a Better quality search, which
        // exist to download a song that is already there.
        // A song fetched ahead is never one the library has in any quality: it plays from there.
        if !is_cache
            && options.replacement.is_none()
            && !options.upgrade_search
            && (self.settings.current().subsonic.skip_owned_songs || options.prefetch)
            && let Some(ownership) = &self.services.ownership
            && let Some(owned) = ownership
                .find(
                    Some(&song.artist),
                    Some(&song.title),
                    song.duration,
                    Some(&song.album),
                )
                .await
                .filter(|owned| !flac_only || owned.lossless || options.prefetch)
        {
            let decision = LibraryOwnership::decide(
                Some(&owned),
                self.source_can_be_lossless(options.source_override),
                self.upgrade_allowed(options.requested_by.as_deref()),
            );
            if decision == OwnedDecision::KeepAndUpgrade && !options.prefetch {
                self.queue_upgrade_for(&owned, &song, options.requested_by.as_deref().unwrap_or_default());
            }
            info!(
                "'{} - {}' is already in your library ({}) at {}; not downloading another copy{}",
                song.artist,
                song.title,
                owned.suffix,
                owned.absolute_path,
                if decision == OwnedDecision::KeepAndUpgrade {
                    ", looking for a higher quality one instead"
                } else {
                    ""
                }
            );
            self.track(|t| {
                t.log(
                    external_provider,
                    external_id,
                    acquisition_event_kinds::LIBRARY,
                    &format!(
                        "Already in your library as {}, so no second copy",
                        dotnet::to_upper_invariant(&owned.suffix)
                    ),
                    (decision == OwnedDecision::KeepAndUpgrade)
                        .then_some("A higher quality copy is being looked for instead"),
                )
            });
            self.track(|t| {
                t.already_in_library(
                    external_provider,
                    external_id,
                    Some(&song.artist),
                    Some(&song.title),
                    Some(&owned.absolute_path),
                )
            });
            return Ok(owned.absolute_path);
        }

        // A song a family member already has on disk is never downloaded again: another member
        // gets a link to the same file, and the shared library takes the file over.
        if !is_cache
            && options.replacement.is_none()
            && !options.upgrade_search
            && let Some(family) = family
            && let Some(found) = family
                .find_song(external_provider, external_id, &song)
                .await
                .filter(|found| !flac_only || found.is_real_flac())
            && let Some(path) = match &pool_members {
                Some(_) => Some(found.path.clone()),
                None if options.prefetch => Some(found.path.clone()),
                None => family.for_owner(&found.path).await,
            }
        {
            info!(
                "'{} - {}' is already on disk for the family at {path}; not downloading another copy",
                song.artist, song.title
            );
            self.track(|t| {
                t.log(
                    external_provider,
                    external_id,
                    acquisition_event_kinds::LIBRARY,
                    "Already on disk for the family, so no second copy",
                    None,
                )
            });
            let shown = pool_members
                .as_ref()
                .and_then(|members| members.first())
                .and_then(|first| family.link_path_for(&path, first))
                .unwrap_or_else(|| path.clone());
            self.track(|t| {
                t.already_in_library(
                    external_provider,
                    external_id,
                    Some(&song.artist),
                    Some(&song.title),
                    Some(&shown),
                )
            });
            return Ok(path);
        }

        self.active_downloads.lock().insert(
            song_id.to_string(),
            DownloadInfo {
                song_id: song_id.to_string(),
                external_id: external_id.to_string(),
                external_provider: external_provider.to_string(),
                status: DownloadStatus::InProgress,
                started_at: Utc::now(),
                ahead: options.prefetch,
                ..Default::default()
            },
        );

        // The TRANSFER is cancellable. Everything below it is the FINALIZE phase and
        // deliberately is not: once bytes exist on disk, tagging, registration and the rescan
        // must all run or the file becomes an orphan that Octo has no record of. A client giving
        // up on a slow download used to abort exactly here, which is why a completed download
        // could never be played.
        // A user who asked for this track to be removed meant it.
        if let Some(journal) = &self.services.action_journal
            && journal.is_never_requested(Some(&song.artist), Some(&song.title))
        {
            info!(
                "Skipping '{} - {}': it was removed with Delete from disk, so it is not downloaded again. \
                 Put it back from the trash, or choose Allow downloading again beside it in the Library \
                 actions history on the dashboard.",
                song.artist, song.title
            );
            anyhow::bail!(
                "'{} - {}' was removed with Delete from disk, so it is not downloaded again; put it back \
                 from the trash, or allow downloading it again in the dashboard's Library actions history",
                song.artist,
                song.title
            );
        }

        // Snapshot before anything can correct it: with NameFromMatch off this is still what
        // names the file, which is how every existing library was built.
        let requested = RequestedIdentity::new(&song.artist, &song.title, &song.album, song.track);
        self.start_album_pick(&song, &requested);

        // A song fetched ahead earlier is taken from its folder instead of downloaded again, with
        // what its download learned, and finalized exactly as a fresh download. A person's keep
        // takes it only when it is the very copy they heard.
        let promoted = if options.prefetch
            || options.replacement.is_some()
            || options.upgrade_search
            || (options.place.keep_anyway && !heard_an_ahead_copy(&options.place))
        {
            None
        } else {
            self.promote_ahead(external_provider, external_id, &mut song)
        };

        let new_download = |song: Song| TrackDownload {
            track_id: external_id.to_string(),
            song,
            suppress_notify: silence,
            source_override: options.source_override,
            upgrade_search: options.upgrade_search,
            replacing_path: options.replacement.as_ref().map(|r| r.original_path.clone()),
            fetched_from: None,
            muted: false,
            flac_only,
            ahead: options.prefetch,
            held_files: options.place.held_files.clone(),
            held_lidarr: options.place.held_lidarr.clone(),
            origin: options.place.origin,
            gates: options.place.gates.clone(),
            acquisition_key: acquisition_key(external_provider, external_id, flac_only),
            ledger: options.place.ledger.clone(),
            requested_by: options.requested_by.clone().unwrap_or_default(),
            deny: options.place.deny.clone(),
            keep_anyway: options.place.keep_anyway,
        };
        // A heart's or a play's gates judge a song fetched ahead before it is kept; one they pass
        // over is gone, and the download runs.
        let gated = matches!(
            options.place.origin,
            Some(AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Play)
        ) && !options.place.gates.is_empty();
        let promoted = match promoted {
            Some((landed, fetched_from, identity)) if gated => {
                let mut judged = new_download(song.clone());
                if self
                    .backend
                    .judge_ahead(self, &mut judged, &landed, identity)
                    .await
                {
                    song = judged.song;
                    Some((landed, fetched_from))
                } else {
                    let _ = std::fs::remove_file(&landed);
                    None
                }
            }
            Some((landed, fetched_from, identity)) => {
                self.backend
                    .judge_ahead_joiners(&new_download(song.clone()), &landed, identity)
                    .await;
                Some((landed, fetched_from))
            }
            None => None,
        };

        let (transferred, fetched_from, muted) = match promoted {
            Some((landed, fetched_from)) => (Ok(landed), fetched_from, false),
            None => {
                // Parallel only once slskd has proven it files each download in its own folder;
                // until then the lock is held through the transfer exactly as before. The
                // in-progress marker above keeps a second request for this song waiting either
                // way, and the limiter counts every transfer, album walks and hearts outside the
                // queue included.
                let mut slot = None;
                if let Some(concurrency) = &self.services.concurrency
                    && concurrency.current() > 1
                {
                    *permit = None;
                    slot = concurrency
                        .transfers()
                        .enter(&CancellationToken::new())
                        .await
                        .ok();
                }
                let mut download = new_download(song);
                let transferred = self
                    .backend
                    .download_track(self, &mut download, cancellation_token)
                    .await;
                drop(slot);
                let TrackDownload {
                    song: downloaded,
                    fetched_from,
                    muted,
                    ..
                } = download;
                song = downloaded;
                (transferred, fetched_from, muted)
            }
        };
        // The transfer's check may have named another album holding the same recording; the
        // track stays at its place on the album being fetched.
        if let Some(context) = &options.album_context {
            context.place(external_id, &mut song);
        }
        let mut landed_path = transferred?;
        Self::ensure_on_disk(Some(&landed_path))?;
        if options.place.keep_anyway {
            kept_by_you(&mut song);
        }
        // Placing, tagging and registering touch the library and the mapping file, and those stay
        // one at a time.
        if permit.is_none() {
            *permit = Some(
                self.download_lock
                    .acquire()
                    .await
                    .expect("the download lock is never closed"),
            );
        }
        // A file named for another format than it holds is renamed before anything reads it by
        // its name: the tagger chooses how to write by the extension.
        let extension_fix = self.fix_landed_extension(&landed_path, external_provider, external_id);
        if let Some(fix) = &extension_fix {
            landed_path = fix.path.clone();
        }
        // Asked for a real FLAC copy: anything else, or a FLAC the spectrum check found was made
        // from lossy audio, is thrown away rather than kept.
        if flac_only && (!is_flac_path(&landed_path) || song.transcoded_from.is_some()) {
            warn!(
                "'{} - {}' had to be a real FLAC copy, and {landed_path} is not one; discarding it",
                song.artist, song.title
            );
            let _ = std::fs::remove_file(&landed_path);
            anyhow::bail!(NO_REAL_FLAC);
        }
        song.local_path = Some(landed_path.clone());
        self.track(|t| {
            t.stage(
                external_provider,
                external_id,
                AcquisitionState::Importing,
                None,
                None,
            )
        });
        let finalize = std::time::Instant::now();

        // The loudness is measured while the file is identified: ffmpeg works the disk and the
        // lookups work the network, so the two overlap. Both finish before the file is placed,
        // since nothing may read a file while it moves.
        let loudness = self.start_loudness(&landed_path);

        // Identify before the file is placed: the album the chooser settles on names the folder
        // (#50) and its main artist names the artist folder (#49). Reads only; nothing is written
        // to the file until it sits where it will stay.
        self.identify(
            &mut song,
            &requested,
            &landed_path,
            options.album_context.as_deref(),
        )
        .await;
        if let (Some(fix), Some(plan)) = (&extension_fix, song.tag_plan.as_mut()) {
            plan.notes.push(format!(
                "renamed from {} to {}: the file is {}",
                fix.from, fix.to, fix.format
            ));
        }
        self.apply_loudness(&mut song, loudness, &landed_path).await;
        let identified = [
            (!dotnet::is_blank(&song.album)).then(|| match song.year.filter(|y| *y > 0) {
                Some(year) => format!("on {} ({year})", song.album),
                None => format!("on {}", song.album),
            }),
            song.tag_plan
                .as_ref()
                .map(|plan| format!("{} match", dotnet::to_lower_invariant(plan.confidence.name()))),
            song.verification
                .as_ref()
                .filter(|v| v.verdict == octo_core::fingerprint::VerificationVerdict::Confirmed)
                .map(|_| "AcoustID agrees".to_string()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
        self.track(|t| {
            t.log(
                external_provider,
                external_id,
                acquisition_event_kinds::TAGS,
                &format!("Identified as {} - {}", song.artist, song.title),
                Some(&identified),
            )
        });

        // Placed from the Song, so the path and the tags come from one decision (#48). The file
        // used to be moved inside the backend's download, before any of this was known. A
        // library action's replacement is staged where no scan looks, and moved in only once it
        // carries the original's identity and has passed.
        // A member's download is filed in the family pool by the same layout.
        let pool_root = pool_members
            .as_ref()
            .and(family)
            .and_then(|family| family.pool_root());
        let ahead_folder = options.ahead_folder.as_deref().filter(|_| options.prefetch);
        // The people a file held for a Lidarr follow waits for: those whose own gate it went
        // through, else whoever asked.
        let asked: Vec<&str> = if options.place.gates.is_empty() {
            options
                .requested_by
                .iter()
                .flatten()
                .map(|user| user.trim())
                .collect()
        } else {
            options.place.gates.iter().map(|gate| gate.user.trim()).collect()
        };
        let mut waiting_for_it: Vec<String> = Vec::new();
        for user in asked {
            if !user.is_empty()
                && !waiting_for_it
                    .iter()
                    .any(|known| dotnet::eq_ignore_case(known, user))
            {
                waiting_for_it.push(user.to_string());
            }
        }
        let mut placement = match (&options.replacement, &pool_root, ahead_folder) {
            (_, _, Some(folder)) => Placement::new(
                self.place_ahead(&landed_path, folder, external_provider, external_id)?,
                false,
            ),
            (None, None, None) => {
                self.place_or_hold(
                    &song,
                    &requested,
                    &landed_path,
                    None,
                    options.source_override,
                    &waiting_for_it,
                )
                .await
            }
            (None, Some(root), None) => {
                self.place_or_hold(
                    &song,
                    &requested,
                    &landed_path,
                    Some(root),
                    options.source_override,
                    &waiting_for_it,
                )
                .await
            }
            (Some(_), _, None) => self.stage_replacement(&landed_path)?,
        };
        let mut local_path = placement.path.clone();
        song.local_path = Some(local_path.clone());
        // Again after placement: identification takes seconds, and placement hands back the old
        // path when the file is missing rather than failing.
        Self::ensure_on_disk(Some(&local_path))?;
        if let (Some(context), Some(plan)) = (&options.album_context, &song.tag_plan) {
            context.set_loudness(
                &local_path,
                plan.integrated_lufs
                    .map(|lufs| Loudness::new(lufs, 0.0, plan.true_peak_dbfs.unwrap_or(0.0))),
            );
        }

        // Rich tags and real album art, written where the file will stay. Downloads otherwise
        // arrive bare (YouTube: artist/title and a video thumbnail; Soulseek: whatever the peer
        // tagged), so this is what makes every fetched song a properly-tagged library citizen.
        let writing = std::time::Instant::now();
        let replacing = options.replacement.as_ref().map(|r| r.original_path.clone());
        let cover = self
            .write_metadata_replacing(&local_path, &mut song, replacing.as_deref())
            .await;
        if ahead_folder.is_some() {
            return Ok(self.keep_ahead(
                external_provider,
                external_id,
                song_id,
                &local_path,
                song,
                fetched_from.as_deref(),
            ));
        }
        if let Some(replacement) = &options.replacement {
            placement = self
                .reveal_replacement(&song, &requested, &local_path, replacement)
                .await?;
            local_path = placement.path.clone();
            song.local_path = Some(local_path.clone());
            self.track(|t| {
                t.log(
                    external_provider,
                    external_id,
                    acquisition_event_kinds::LIBRARY,
                    "Swapped in where your old copy was",
                    Some("It keeps the old copy's place in Navidrome, so plays and favorites stay"),
                )
            });
        }
        if let Some(context) = &options.album_context {
            context.set_placed(&local_path, &song);
        }
        if !is_cache {
            self.write_sidecars(&song, &placement, cover.as_deref());
        }
        if let Some(plan) = song.tag_plan.as_deref_mut() {
            plan.stage_seconds
                .insert("write".to_string(), writing.elapsed().as_secs_f64());
            plan.stage_seconds
                .insert("total".to_string(), finalize.elapsed().as_secs_f64());
        }
        if let Some(plan) = song.tag_plan.as_deref() {
            info!("{}", plan.describe_song(&song));
            if finalize.elapsed() > Self::FINALIZE_BUDGET {
                let stages: Vec<String> = plan
                    .stage_seconds
                    .iter()
                    .map(|(stage, seconds)| {
                        format!("{stage} {}s", dotnet::format_optional_decimals(*seconds, 1))
                    })
                    .collect();
                warn!(
                    "finalizing '{} - {}' took {:.1}s; stages: {}",
                    song.artist,
                    song.title,
                    finalize.elapsed().as_secs_f64(),
                    stages.join(", ")
                );
            }
        }

        if let Some(seconds) = octo_media::tags::TagFile::open(&local_path)
            .ok()
            .map(|file| file.duration_seconds())
            .filter(|&seconds| seconds > 0)
        {
            self.metadata.remember_file_length(external_id, seconds);
        }
        if !is_cache && let Some(arrivals) = &self.services.arrivals {
            arrivals.arrived(&key_of(external_provider, external_id), &local_path);
        }

        if let Some(info) = self.active_downloads.lock().get_mut(song_id) {
            info.status = DownloadStatus::Completed;
            info.local_path = Some(local_path.clone());
            info.completed_at = Some(Utc::now());
        }

        // Check if this track belongs to a playlist and update M3U.
        if let Some(playlists) = &self.services.playlist_sync
            && let Some(playlist_id) = playlists.get_playlist_id_for_track(song_id)
        {
            info!("Track {song_id} belongs to playlist {playlist_id}, adding to M3U");
            if let Err(e) = playlists
                .add_track_to_m3u(&playlist_id, &song, &local_path, false)
                .await
            {
                warn!("Failed to update playlist M3U for track {song_id}: {e}");
            }
        }

        // Only register and scan if NOT in cache mode.
        if !is_cache {
            self.local_library
                .register_downloaded_song_as(&song, &local_path, kept_heart(&options.place, &song))
                .await?;
            // A pool file is recorded and linked for every member who asked (their libraries
            // are scanned one by one); Navidrome shows it at the first member's link.
            let pooled = match (family, &pool_members, &pool_root) {
                (Some(family), Some(members), Some(_)) => Some(
                    family
                        .placed(&local_path, &song, external_provider, external_id, members)
                        .await
                        .unwrap_or_else(|| local_path.clone()),
                ),
                _ => None,
            };
            self.record_history(
                &song,
                &local_path,
                silence || muted,
                fetched_from.as_deref(),
                options.requested_by.as_deref(),
                Some(&key_of(external_provider, external_id)),
            )
            .await;
            if pooled.is_none() {
                self.ask_for_review(&song, &local_path, options.requested_by.as_deref());
            }
            let shown = pooled.clone().unwrap_or_else(|| local_path.clone());
            self.track(|t| {
                t.imported(
                    external_provider,
                    external_id,
                    Some(&song.artist),
                    Some(&song.title),
                    Some(&shown),
                )
            });

            // Trigger a Subsonic library rescan (with debounce). The pool is no library: its
            // members' libraries were scanned when the file was linked.
            if pooled.is_none() {
                let library = Arc::clone(&self.local_library);
                tokio::spawn(async move {
                    library.trigger_library_scan(false).await;
                });
            }

            // If download mode is Album and triggering is enabled, start background download of
            // remaining tracks.
            if pooled.is_none()
                && options.trigger_album_download
                && self.settings.current().subsonic.download_mode == DownloadMode::Album
                && let Some(album_id) = song.album_id.as_deref().filter(|id| !id.is_empty())
                && let Some(album_external_id) = self
                    .backend
                    .extract_external_id_from_album_id(album_id)
                    .filter(|id| !id.is_empty())
            {
                info!("Download mode is Album, triggering background download for album {album_external_id}");
                let me = self.arc();
                let (exclude, source, requested_by) = (
                    external_id.to_string(),
                    options.source_override,
                    // The album walk is still this user's star, so every track it pulls in is
                    // attributed to them too.
                    options.requested_by.clone(),
                );
                tokio::spawn(async move {
                    let token = CancellationToken::new();
                    let walk = me.download_remaining_album_tracks(
                        &album_external_id,
                        &exclude,
                        source,
                        false,
                        &token,
                        requested_by,
                    );
                    if let Err(e) = walk.await {
                        error!(
                            "Failed to download remaining album tracks for album {album_external_id}: {e:#}"
                        );
                    }
                });
            }
        } else {
            info!("Cache mode: skipping library registration and scan");
            self.track(|t| t.complete(external_provider, external_id, None));
        }

        info!("Download completed: {local_path}");
        Ok(local_path)
    }

    /// Takes a song fetched ahead into a staging folder of its own under the incoming folder (a
    /// copy, when the song is playing now), and gives the song back what its download learned.
    /// The staged path and the source that brought it, or None when there is no such song (or
    /// it could not be taken, and the song is downloaded as usual).
    fn promote_ahead(
        &self,
        external_provider: &str,
        external_id: &str,
        song: &mut Song,
    ) -> Option<(String, Option<String>, HeardSource)> {
        let prefetched = self.services.prefetched.as_ref()?;
        if !prefetched.has(external_provider, external_id) {
            return None;
        }
        let staging = Path::new(&self.download_path())
            .join(crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME)
            .join("prefetch")
            .join(uuid::Uuid::new_v4().simple().to_string());
        if let Err(e) = std::fs::create_dir_all(&staging) {
            warn!(
                "Could not make {}: {e}; downloading the song again",
                staging.display()
            );
            return None;
        }
        let name = crate::services::prefetch::file_name(external_provider, external_id, "flac");
        let Some(taken) = prefetched.take(external_provider, external_id, &staging.join(name)) else {
            let _ = std::fs::remove_dir(&staging);
            return None;
        };
        song.transcoded_from = taken.sidecar.transcoded_from.clone();
        song.source_peer = taken.sidecar.source_peer.clone().or(song.source_peer.take());
        song.source_file = taken.sidecar.source_file.clone().or(song.source_file.take());
        song.verification = taken.verification();
        info!(
            "'{} - {}' was downloaded ahead earlier; keeping that copy",
            song.artist, song.title
        );
        self.track(|t| {
            t.log(
                external_provider,
                external_id,
                acquisition_event_kinds::LIBRARY,
                "Downloaded ahead earlier, so no second download",
                None,
            )
        });
        // The copy's own identity, as a heard copy of it is recorded: the same entry under the same
        // key, made ready from the same file.
        let identity = HeardSource::Copy {
            key: format!("{external_provider}:{external_id}"),
            ready_at: taken.sidecar.ready_at,
            source_file: taken.sidecar.source_file.clone(),
        };
        Some((
            taken.path.to_string_lossy().into_owned(),
            taken.sidecar.fetched_from.clone(),
            identity,
        ))
    }

    /// The end of a download ahead: its facts beside the file in the folder, the folder's index
    /// told, and no record left behind, so a heart waiting on it takes the file from there.
    fn keep_ahead(
        &self,
        external_provider: &str,
        external_id: &str,
        song_id: &str,
        local_path: &str,
        song: Song,
        fetched_from: Option<&str>,
    ) -> String {
        let path = Path::new(local_path);
        if let Some(seconds) = octo_media::tags::TagFile::open(local_path)
            .ok()
            .map(|file| file.duration_seconds())
            .filter(|&seconds| seconds > 0)
        {
            self.metadata.remember_file_length(external_id, seconds);
        }
        let sidecar = AheadSidecar::of(
            external_provider,
            external_id,
            path,
            &song,
            fetched_from,
            Utc::now(),
        );
        if let Err(e) = write_sidecar(path, &sidecar) {
            warn!("Could not write what was learned beside {local_path}: {e}");
        }
        if let Some(prefetched) = &self.services.prefetched {
            prefetched.kept(external_provider, external_id, path, song.verification.clone());
        }
        self.active_downloads.lock().remove(song_id);
        info!(
            "Downloaded ahead: {} - {} ({})",
            song.artist, song.title, sidecar.format
        );
        local_path.to_string()
    }

    fn is_in_progress(&self, song_id: &str) -> bool {
        self.active_downloads
            .lock()
            .get(song_id)
            .is_some_and(|info| info.status == DownloadStatus::InProgress)
    }

    // ---- the album walk -------------------------------------------------------------------

    /// The newest album being fetched song by song that lists this track.
    fn held_album(&self, external_id: &str) -> Option<Arc<AlbumTagContext<Loudness>>> {
        self.held_albums
            .lock()
            .iter()
            .rev()
            .find(|held| held.lists(external_id))
            .cloned()
    }

    /// Holds an album's listing while its tracks are fetched song by song, each as one of a walk.
    pub fn hold_album(&self, album: &Album) {
        let context = AlbumTagContext::<Loudness>::new(
            album.external_id.as_deref().or(Some(album.id.as_str())),
            &album.title,
            Some(&album.artist),
        )
        .with_listing(album);
        self.held_albums.lock().push(Arc::new(context));
    }

    /// Ends the hold of [`Self::hold_album`]: every track fetched under it gets the album-level
    /// tags its tracks settled on.
    pub async fn finish_held_album(&self, album: &Album) {
        let id = album.external_id.as_deref().unwrap_or(album.id.as_str());
        let context = {
            let mut held = self.held_albums.lock();
            let at = held.iter().rposition(|context| {
                context.catalog_album_id() == Some(id) && context.album_title() == album.title
            });
            at.map(|at| held.remove(at))
        };
        if let Some(context) = context
            && self.settle_album_tags(&context, &album.title) > 0
        {
            self.local_library.trigger_library_scan(true).await;
        }
    }

    /// Every track of an album but `exclude_track_external_id`, one download each. True when
    /// none failed. Boxed: a download can start a walk, and a walk runs downloads.
    pub fn download_remaining_album_tracks<'a>(
        &'a self,
        album_external_id: &'a str,
        exclude_track_external_id: &'a str,
        source_override: Option<DownloadSource>,
        suppress_summary: bool,
        cancellation_token: &'a CancellationToken,
        requested_by: Option<Vec<String>>,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
        async move {
            self.walk_album(
                album_external_id,
                exclude_track_external_id,
                source_override,
                suppress_summary,
                cancellation_token,
                requested_by,
            )
            .await
        }
        .boxed()
    }

    /// One walk of an album at a time. A request that comes while one runs for the same source and
    /// the same people, and that walks every track that walk walks, answers what that walk answers,
    /// and starts no walk of its own. Any other
    /// request waits for that walk to end and then walks the album itself: its own source may find
    /// what the running one could not, and its own people decide what it may upgrade.
    async fn walk_album(
        &self,
        album_external_id: &str,
        exclude_track_external_id: &str,
        source_override: Option<DownloadSource>,
        suppress_summary: bool,
        cancellation_token: &CancellationToken,
        requested_by: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        let answer = loop {
            let running = {
                let mut walks = self.album_walks.lock();
                match walks.get(album_external_id) {
                    Some(running) => Err((
                        running.source == source_override
                            && running.requested_by == requested_by
                            && (running.exclude.is_empty() || running.exclude == exclude_track_external_id),
                        running.answer.clone(),
                    )),
                    None => {
                        let (answer, running) = watch::channel(None);
                        walks.insert(
                            album_external_id.to_string(),
                            RunningWalk {
                                source: source_override,
                                requested_by: requested_by.clone(),
                                exclude: exclude_track_external_id.to_string(),
                                answer: running,
                            },
                        );
                        Ok(answer)
                    }
                }
            };
            let (shared, mut running) = match running {
                Ok(answer) => break answer,
                Err(waiting) => waiting,
            };
            if shared {
                info!("Album {album_external_id} is being walked already; this request waits for that walk");
            } else {
                info!(
                    "Album {album_external_id} is being walked for another source or person; this request walks it once that walk ends"
                );
            }
            let ended = tokio::select! {
                _ = cancellation_token.cancelled() => return Ok(false),
                ended = walk_ended(&mut running) => ended,
            };
            if shared {
                // A walk that ended without an answer answers false.
                return Ok(ended.unwrap_or(false));
            }
        };
        // Made after `answer`, so it drops first: the album is free before the walk's waiters wake.
        let _walking = AlbumWalkEntry {
            walks: &self.album_walks,
            album_external_id,
        };
        let walked = self
            .walk_album_once(
                album_external_id,
                exclude_track_external_id,
                source_override,
                suppress_summary,
                cancellation_token,
                requested_by,
            )
            .await;
        answer.send_replace(Some(walked.as_ref().is_ok_and(|all| *all)));
        walked
    }

    async fn walk_album_once(
        &self,
        album_external_id: &str,
        exclude_track_external_id: &str,
        source_override: Option<DownloadSource>,
        suppress_summary: bool,
        cancellation_token: &CancellationToken,
        requested_by: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        let provider = self.provider_name().to_string();
        info!(
            "Starting background download for album {album_external_id} (excluding track {exclude_track_external_id})"
        );

        let album = self.metadata.get_album(&provider, album_external_id).await;
        if let Some(refusal) = Self::album_walk_refusal(album.as_ref()) {
            warn!("Album {album_external_id}: {refusal}");
            // Only a heart on the album itself reports this. A walk started by a track star has
            // already reported that track's own outcome, and a mid-chain source stays quiet so
            // the next source can try.
            if !suppress_summary && exclude_track_external_id.is_empty() {
                self.notifications.notify(NotificationEvent {
                    artist: album.as_ref().map(|a| a.artist.clone()),
                    title: Some(album.as_ref().map_or("album".to_string(), |a| a.title.clone())),
                    cover_art_url: album.as_ref().and_then(|a| a.cover_art_url.clone()),
                    detail: Some(refusal),
                    ..NotificationEvent::new(NotificationEventType::DownloadFailed)
                });
            }
            return Ok(false);
        }
        let album = album.expect("a walkable album is there");

        let mut tracks_to_download: Vec<Song> = album
            .songs
            .iter()
            .filter(|s| {
                s.external_id
                    .as_deref()
                    .is_some_and(|id| id != exclude_track_external_id && !id.is_empty())
            })
            .cloned()
            .collect();

        // Tracks a hearted album already holds wait where they are: no walk fetches them again.
        let askers = requested_by.clone().unwrap_or_default();
        if let Some(gates) = &self.services.album_gates {
            tracks_to_download
                .retain(|track| !gates.settled(&provider, album_external_id, &askers, external_id_of(track)));
        }

        info!(
            "Found {} additional tracks to download for album '{}'",
            tracks_to_download.len(),
            album.title
        );

        // The whole list is known now, so a hearted album shows every track it will fetch.
        let announced: Vec<(String, Option<String>, Option<String>, Option<String>)> = tracks_to_download
            .iter()
            .map(|s| {
                (
                    external_id_of(s).to_string(),
                    Some(s.artist.clone()),
                    Some(s.title.clone()),
                    Some(album.title.clone()),
                )
            })
            .collect();
        self.track(|t| {
            t.announce(
                &provider,
                Some(album_external_id),
                Some(exclude_track_external_id),
                &announced,
            )
        });

        // Per-track notifications are muted below; these feed one summary instead.
        let counters = WalkCounters::default();
        let (mut kept, mut upgrading) = (0, 0);

        // Songs already in the library stay as they are, and a lossy one is queued for a higher
        // quality copy instead of downloaded again. Only what is missing is looked for, by
        // folder or song by song.
        if self.settings.current().subsonic.skip_owned_songs
            && let Some(ownership) = &self.services.ownership
        {
            let album_title = album.title.as_str();
            // Owned songs into the closures: a closure over borrowed items does not satisfy the
            // `Send` the boxed walk asks for.
            let found: Vec<(String, Option<OwnedCopy>)> = futures::stream::iter(tracks_to_download.clone())
                .map(|track: Song| async move {
                    let album_name = if track.album.is_empty() {
                        album_title
                    } else {
                        &track.album
                    };
                    let copy = ownership
                        .find(
                            Some(&track.artist),
                            Some(&track.title),
                            track.duration,
                            Some(album_name),
                        )
                        .await;
                    Ok::<_, std::convert::Infallible>((external_id_of(&track).to_string(), copy))
                })
                .buffer_unordered(4)
                .try_collect()
                .await
                .unwrap_or_default();
            let owned: HashMap<String, OwnedCopy> = found
                .into_iter()
                .filter_map(|(id, copy)| copy.map(|copy| (id, copy)))
                .collect();
            let can_be_lossless = self.source_can_be_lossless(source_override);
            let may_upgrade = self.upgrade_allowed(requested_by.as_deref());
            for track in tracks_to_download.iter() {
                let Some(copy) = owned.get(external_id_of(track)) else {
                    continue;
                };
                if LibraryOwnership::decide(Some(copy), can_be_lossless, may_upgrade)
                    == OwnedDecision::KeepAndUpgrade
                {
                    self.queue_upgrade_for(copy, track, requested_by.as_deref().unwrap_or_default());
                    upgrading += 1;
                } else {
                    kept += 1;
                }
                self.track(|t| {
                    t.already_in_library(
                        &provider,
                        external_id_of(track),
                        Some(&track.artist),
                        Some(&track.title),
                        Some(&copy.absolute_path),
                    )
                });
            }
            tracks_to_download.retain(|t| !owned.contains_key(external_id_of(t)));
            if !owned.is_empty() {
                info!(
                    "Album '{}': {kept} songs already yours, {upgrading} queued for a higher quality copy, {} to download",
                    album.title,
                    tracks_to_download.len()
                );
            }
        }

        // Every track of the walk is filed at its place on this album and shares the release the
        // first one settled on, and the walk measures each track so the album gain can be
        // written once it ends.
        let album_context = Arc::new(
            AlbumTagContext::<Loudness>::new(Some(album_external_id), &album.title, Some(&album.artist))
                .with_listing(&album),
        );
        let walk = Walk {
            album_title: &album.title,
            context: &album_context,
            counters: &counters,
            source_override,
            suppress_summary,
            requested_by: &requested_by,
            cancellation_token,
            album_external_id,
        };

        // Tracks whose file came from one peer's folder of the album, already queued in one
        // batch, go one at a time in album order: that peer sends them back to back, so waiting
        // on several at once would only sit in its queue and run out each wait's quiet window.
        // The rest are searched song by song, side by side when downloads may run in parallel;
        // the transfer gate keeps the total to the setting. With nothing prepared and one at a
        // time, this is the walk as it always was.
        let prepared = self
            .backend
            .prepare_album(
                self,
                &album,
                &tracks_to_download,
                source_override,
                cancellation_token,
            )
            .await;
        let prepared_set: HashSet<&str> = prepared.iter().map(String::as_str).collect();
        let (from_folder, by_search): (Vec<&Song>, Vec<&Song>) = tracks_to_download
            .iter()
            .partition(|t| prepared_set.contains(external_id_of(t)));
        let folder_lane = async {
            for track in &from_folder {
                self.walk_track(track, &walk).await;
            }
        };
        let width = self.concurrency().map_or(1, |c| c.current()).max(1) as usize;
        // The futures are made first and then run side by side: a closure over borrowed songs in
        // the stream does not satisfy the `Send` the boxed walk asks for.
        let searches: Vec<_> = by_search
            .iter()
            .map(|track| self.walk_track(track, &walk))
            .collect();
        let search_lane = futures::stream::iter(searches)
            .buffer_unordered(width)
            .collect::<Vec<()>>();
        futures::join!(folder_lane, search_lane);
        self.backend.finish_album(self, &prepared).await;

        info!("Completed background download for album '{}'", album.title);

        if self.settle_album_tags(&album_context, &album.title) > 0 {
            self.local_library.trigger_library_scan(true).await;
        }
        if self.settings.current().metadata.replay_gain {
            self.write_album_gain(&album_context, &album.title);
        }

        let failed = counters.failed.load(Ordering::SeqCst);
        let summary = Self::build_album_summary(
            &album,
            counters.succeeded.load(Ordering::SeqCst),
            counters.lossless.load(Ordering::SeqCst),
            failed,
            kept,
            upgrading,
        );
        // Hide an intermediate failure while another source remains, but still report success
        // when an earlier priority step completes the album acquisition.
        if (!suppress_summary || failed == 0)
            && let Some(summary) = summary
        {
            self.notifications.notify(summary);
        }
        Ok(failed == 0)
    }

    /// One track of the walk. The counters are shared by both lanes, so they move atomically.
    async fn walk_track(&self, track: &Song, walk: &Walk<'_>) {
        let provider = self.provider_name();
        let track_id = external_id_of(track);
        // Each person whose heart of the album is gated judges the track's file against the
        // track's own recording, by the album's rule.
        let askers = walk.requested_by.clone().unwrap_or_default();
        let gates = self
            .services
            .album_gates
            .as_ref()
            .map(|gates| gates.track_gates(provider, walk.album_external_id, &askers, track_id))
            .unwrap_or_default();
        let attempt: anyhow::Result<()> = async {
            self.backend.ensure_routing_registered(track);

            if let Some(existing) = self
                .local_library
                .get_local_path_for_external_song(provider, track_id)
                .await
                .filter(|path| file_exists(path))
            {
                debug!("Track {track_id} already downloaded, skipping");
                self.track(|t| {
                    t.already_in_library(
                        provider,
                        track_id,
                        Some(&track.artist),
                        Some(&track.title),
                        Some(&existing),
                    )
                });
                return Ok(());
            }

            // Check if download is already in progress or recently completed.
            let song_id = format!("ext-{provider}-{track_id}");
            if let Some(active) = self.active_download(&song_id) {
                // A download ahead lands its file outside the library: the download below waits
                // for it and keeps its file.
                if active.status == DownloadStatus::InProgress && !active.ahead {
                    debug!("Track {track_id} download already in progress, skipping");
                    return Ok(());
                }
                if active.status == DownloadStatus::Completed {
                    debug!("Track {track_id} already downloaded in this session, skipping");
                    self.track(|t| {
                        t.already_in_library(
                            provider,
                            track_id,
                            Some(&track.artist),
                            Some(&track.title),
                            active.local_path.as_deref(),
                        )
                    });
                    return Ok(());
                }
            }

            info!(
                "Downloading track '{}' from album '{}'",
                track.title, walk.album_title
            );
            let path = self
                .download_song_internal(
                    provider,
                    track_id,
                    DownloadOptions {
                        trigger_album_download: false,
                        force_permanent: true,
                        suppress_notify: true,
                        source_override: walk.source_override,
                        requested_by: walk.requested_by.clone(),
                        album_context: Some(Arc::clone(walk.context)),
                        place: AcquisitionPlace {
                            origin: gates.first().map(|gate| gate.origin),
                            gates: gates.clone(),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    walk.cancellation_token,
                )
                .await?;
            walk.counters.succeeded.fetch_add(1, Ordering::SeqCst);
            if dotnet::to_lower_invariant(&path).ends_with(".flac") {
                walk.counters.lossless.fetch_add(1, Ordering::SeqCst);
            }

            // Force a rescan per track so the album fills in progressively in the client instead
            // of appearing all at once at the end. The per-download scan is debounced, which
            // during a batch swallows most triggers and can strand the final tracks entirely.
            self.local_library.trigger_library_scan(true).await;
            Ok(())
        }
        .await;
        if let Err(e) = attempt {
            // Held in the one hold path for the album's hearts: their chain files it, and no later
            // walk of the album fetches it again.
            if let Some(held) = e.downcast_ref::<AcquisitionHeld>()
                && let Some(album_gates) = &self.services.album_gates
            {
                info!(
                    "Track {track_id} '{}' waits in the hold path: {}",
                    track.title, held.line
                );
                album_gates.held(provider, walk.album_external_id, &askers, track_id, held.clone());
                return;
            }
            warn!("Failed to download track {track_id} '{}': {e:#}", track.title);
            walk.counters.failed.fetch_add(1, Ordering::SeqCst);
            // Same rule as the summary: a source with another after it stays quiet, and the next
            // walk picks the track up again.
            if !walk.suppress_summary {
                let message = e.to_string();
                self.track(|t| t.fail(provider, track_id, Some(&message)));
            }
        }
    }

    /// Why an album walk cannot start, or `None` when it can. A walk with nothing to walk used
    /// to return success, so a hearted album whose track list never loaded (the metadata
    /// provider was down or rate-limited) did nothing, said nothing, and stopped the source chain
    /// there.
    pub fn album_walk_refusal(album: Option<&Album>) -> Option<String> {
        match album {
            None => Some(
                "Octo could no longer look this album up, so nothing was downloaded. Heart it again from a fresh search."
                    .to_string(),
            ),
            Some(album)
                if !album
                    .songs
                    .iter()
                    .any(|song| song.external_id.as_deref().is_some_and(|id| !id.is_empty())) =>
            {
                Some(format!(
                    "No track list came back for \"{}\", so nothing was downloaded. The metadata provider may be down; try again later.",
                    album.title
                ))
            }
            Some(_) => None,
        }
    }

    /// `None` when the walk did no work: a re-star whose tracks are all already present must not
    /// ping the phone. Counts cover the walked tracks only; the track whose star triggered the
    /// walk got its own DownloadCompleted.
    pub fn build_album_summary(
        album: &Album,
        succeeded: i32,
        lossless: i32,
        failed: i32,
        kept: i32,
        upgrading: i32,
    ) -> Option<NotificationEvent> {
        if succeeded + failed + kept + upgrading == 0 {
            return None;
        }
        Some(NotificationEvent {
            artist: Some(album.artist.clone()),
            title: Some(album.title.clone()),
            cover_art_url: album.cover_art_url.clone(),
            track_count: Some(succeeded),
            lossless_count: Some(lossless),
            failed_count: Some(failed),
            kept_count: Some(kept),
            upgrading_count: Some(upgrading),
            ..NotificationEvent::new(NotificationEventType::AlbumCompleted)
        })
    }

    /// Whether a lossless copy can be looked for: an owned lossy copy is then queued for Better
    /// quality rather than kept as it is. That runs through the upgrade sources (Soulseek,
    /// Lidarr), whichever source this download uses. Without them, as before: this download's
    /// own source.
    fn source_can_be_lossless(&self, source_override: Option<DownloadSource>) -> bool {
        match &self.services.upgrade_sources {
            Some(sources) => sources.ready(),
            None => matches!(
                source_override.unwrap_or(self.settings.current().subsonic.download_source),
                DownloadSource::Soulseek | DownloadSource::SoulseekThenYouTube
            ),
        }
    }

    /// Whether Better quality may run for the person who asked: every gate of the action.
    fn upgrade_allowed(&self, requested_by: Option<&[String]>) -> bool {
        let Some(asker) = requested_by.and_then(|askers| askers.first()) else {
            return false;
        };
        if self.services.upgrade_queue.is_none() {
            return false;
        }
        let actions = self.settings.current().library_actions.clone();
        actions.enabled
            && !actions.dry_run
            && actions.is_allowed(Some(asker))
            && actions
                .effective_actions()
                .iter()
                .any(|a| a.action == LibraryAction::BetterQuality && a.enabled)
    }

    fn queue_upgrade_for(&self, owned: &OwnedCopy, song: &Song, requested_by: &[String]) {
        if let Some(queue) = &self.services.upgrade_queue {
            queue.add(
                vec![UpgradeAsk {
                    navidrome_id: owned.navidrome_id.clone().unwrap_or_default(),
                    title: Some(song.title.clone()),
                    artist: Some(song.artist.clone()),
                    album: Some(song.album.clone()),
                    suffix: Some(owned.suffix.clone()),
                    attempt_key: None,
                    pick: None,
                }],
                requested_by.first().map(String::as_str).unwrap_or(""),
                "heart",
            );
        }
    }

    // ---- bookkeeping ----------------------------------------------------------------------

    /// Record a completed download in the fetched-songs log. Best-effort: format and source are
    /// derived from the file extension (flac -> Soulseek/lossless, otherwise -> YouTube/lossy),
    /// which matches Octo's two download sources, unless the backend named the source.
    async fn record_history(
        &self,
        song: &Song,
        local_path: &str,
        suppress_notify: bool,
        fetched_from: Option<&str>,
        requested_by: Option<&[String]>,
        key: Option<&str>,
    ) {
        let mut cover = song
            .cover_art_url_large
            .clone()
            .or_else(|| song.cover_art_url.clone());
        let mut album = (!song.album.is_empty()).then(|| song.album.clone());

        // The star path rebuilds the song from its id, so it has no artwork or album. Pull the
        // cover and album straight from Deezer for the log entry (cached, so this is cheap).
        // Best-effort: never fails a download.
        if (cover.as_deref().is_none_or(str::is_empty) || album.is_none())
            && let Some(deezer) = &self.services.deezer
            && let Some(meta) = deezer.enrich_track(&song.artist, &song.title, false, false).await
        {
            cover = cover.or(meta.album_cover_url);
            album = album.or(meta.album_title);
        }

        let ext = dotnet::to_upper_invariant(get_extension(local_path).trim_start_matches('.'));
        let format = if ext.is_empty() {
            "?".to_string()
        } else {
            ext.clone()
        };
        let source = fetched_from
            .map(str::to_string)
            .unwrap_or_else(|| if ext == "FLAC" { "Soulseek" } else { "YouTube" }.to_string());
        let size = std::fs::metadata(local_path).map(|m| m.len() as i64).unwrap_or(0);
        let requested_by: Option<Vec<String>> =
            requested_by.filter(|r| !r.is_empty()).map(<[String]>::to_vec);
        self.history.record(DownloadHistoryEntry {
            artist: song.artist.clone(),
            title: song.title.clone(),
            album: Some(album.clone().unwrap_or_default()),
            path: local_path.to_string(),
            format: format.clone(),
            source: source.clone(),
            cover_art_url: cover.clone(),
            size_bytes: size,
            transcoded_from: song.transcoded_from.clone(),
            tagging: song.tag_plan.as_ref().map(|plan| plan.to_report()),
            downloaded_at: round_trip(&Utc::now()),
            requested_by: requested_by.clone(),
            // The live row's key, which its saved log is found by on the dashboard. The log
            // itself is saved when the row ends (DownloadLogKeeper).
            key: key.map(str::to_string),
            log: None,
        });

        // Same chokepoint as the fetched-songs log, reusing what it just assembled (the
        // Deezer-enriched cover and album included): anything worth logging is worth telling the
        // user about, with the same data.
        if !suppress_notify {
            self.notifications.notify(NotificationEvent {
                artist: Some(song.artist.clone()),
                title: Some(song.title.clone()),
                album,
                format: Some(format),
                source: Some(source),
                cover_art_url: cover,
                size_bytes: Some(size),
                // Identification and tagging ran before this, so these are the values the file
                // itself was tagged with.
                duration_seconds: song.duration,
                year: song.year,
                requested_by,
                ..NotificationEvent::new(NotificationEventType::DownloadCompleted)
            });
        }
    }

    /// A download a person can settle by listening goes to their Review playlist (#47): the ones
    /// who asked for it, or, when nobody on the allowlist did, whoever keeps the library.
    fn ask_for_review(&self, song: &Song, local_path: &str, requested_by: Option<&[String]>) {
        let Some(verdict) = song.verification.as_deref().filter(|v| v.needs_review()) else {
            return;
        };
        let actions = self.settings.current().library_actions.clone();
        if !(actions.enabled && actions.review_enabled) {
            return;
        }
        let Some(notices) = &self.services.review_queue else {
            return;
        };

        let mut owners: Vec<String> = requested_by
            .unwrap_or_default()
            .iter()
            .filter(|user| actions.is_allowed(Some(user)))
            .cloned()
            .collect();
        if owners.is_empty() {
            owners = actions
                .allowed_users
                .iter()
                .filter(|user| !dotnet::is_blank(user))
                .cloned()
                .collect();
        }
        for owner in owners {
            if notices.add_review(&owner, local_path, song, verdict) {
                info!(
                    "Asking {owner} about '{} - {}': {}",
                    song.artist, song.title, verdict.reason
                );
            }
        }
    }

    /// The release identifier: the host's, or one built on the spot from the catalog and the
    /// music database this service has, so a host that registered none still identifies.
    fn identifier(&self) -> &Arc<ReleaseIdentifier> {
        self.identifier.get_or_init(|| {
            self.services.release_identifier.clone().unwrap_or_else(|| {
                Arc::new(ReleaseIdentifier::new(
                    Arc::clone(&self.settings),
                    Arc::new(TagWriterExtras),
                    self.services
                        .music_brainz
                        .clone()
                        .map(|c| c as Arc<dyn octo_core::tagging::ReleaseLookup>),
                    self.services
                        .deezer
                        .clone()
                        .map(|c| c as Arc<dyn octo_core::tagging::CatalogLookup>),
                ))
            })
        })
    }
}

/// A row hushed for a download ahead, for as long as it runs (to its end, or until its caller
/// gives up on it).
struct Hushed {
    tracker: Option<Arc<AcquisitionTracker>>,
    provider: String,
    external_id: String,
}

impl Hushed {
    fn new(tracker: Option<&Arc<AcquisitionTracker>>, provider: &str, external_id: &str) -> Self {
        if let Some(tracker) = tracker {
            tracker.hush(provider, external_id);
        }
        Hushed {
            tracker: tracker.cloned(),
            provider: provider.to_string(),
            external_id: external_id.to_string(),
        }
    }
}

impl Drop for Hushed {
    fn drop(&mut self) {
        if let Some(tracker) = &self.tracker {
            tracker.unhush(&self.provider, &self.external_id);
        }
    }
}

/// The shared state of one album walk.
struct Walk<'a> {
    album_title: &'a str,
    context: &'a Arc<AlbumTagContext<Loudness>>,
    counters: &'a WalkCounters,
    source_override: Option<DownloadSource>,
    suppress_summary: bool,
    requested_by: &'a Option<Vec<String>>,
    cancellation_token: &'a CancellationToken,
    /// The album's id, which its hearts' gates are kept under.
    album_external_id: &'a str,
}

#[derive(Default)]
struct WalkCounters {
    succeeded: AtomicI32,
    lossless: AtomicI32,
    failed: AtomicI32,
}

fn external_id_of(song: &Song) -> &str {
    song.external_id.as_deref().unwrap_or("")
}

/// Whether `path` names an existing file, not a directory.
pub(crate) fn file_exists(path: &str) -> bool {
    !path.is_empty() && std::path::Path::new(path).is_file()
}

/// A UTC instant in round-trip ISO 8601 form: seven fractional digits and a `Z`.
fn round_trip(value: &DateTime<Utc>) -> String {
    format!(
        "{}.{:07}Z",
        value.format("%Y-%m-%dT%H:%M:%S"),
        value.timestamp_subsec_nanos() / 100
    )
}

/// Sets the file's last access time to now, best-effort.
fn touch_access_time(path: &str) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let times = std::fs::FileTimes::new().set_accessed(std::time::SystemTime::now());
        let _ = file.set_times(times);
    }
}

/// The extension of the file name: its last dot and what follows, or empty when the name has
/// no dot or ends with one.
pub(crate) fn get_extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => &name[dot..],
        _ => "",
    }
}

#[async_trait]
impl IDownloadService for BaseDownloadService {
    async fn download_song(
        &self,
        external_provider: &str,
        external_id: &str,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<String> {
        self.download_song_internal(
            external_provider,
            external_id,
            DownloadOptions {
                trigger_album_download: true,
                ..Default::default()
            },
            cancellation_token,
        )
        .await
    }

    async fn download_and_stream(
        &self,
        external_provider: &str,
        external_id: &str,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        let local_path = self
            .download_song_internal(
                external_provider,
                external_id,
                DownloadOptions {
                    trigger_album_download: true,
                    ..Default::default()
                },
                cancellation_token,
            )
            .await?;
        let file = tokio::fs::File::open(&local_path).await?;
        Ok(Box::pin(ReaderStream::new(file)))
    }

    fn download_remaining_album_tracks_in_background(
        &self,
        external_provider: &str,
        album_external_id: &str,
        exclude_track_external_id: &str,
    ) {
        if external_provider != self.provider_name() {
            warn!("Provider '{external_provider}' is not supported for album download");
            return;
        }
        let me = self.arc();
        let (album, exclude) = (
            album_external_id.to_string(),
            exclude_track_external_id.to_string(),
        );
        tokio::spawn(async move {
            let token = CancellationToken::new();
            let walk = me.download_remaining_album_tracks(&album, &exclude, None, false, &token, None);
            if let Err(e) = walk.await {
                error!("Failed to download remaining album tracks for album {album}: {e:#}");
            }
        });
    }

    async fn execute_acquisition(
        &self,
        external_provider: &str,
        external_id: &str,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        cancellation_token: &CancellationToken,
        requested_by: Option<Vec<String>>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        self.download_song_internal(
            external_provider,
            external_id,
            DownloadOptions {
                trigger_album_download,
                force_permanent,
                source_override,
                requested_by,
                upgrade_search,
                replacement,
                ..Default::default()
            },
            cancellation_token,
        )
        .await
    }

    async fn execute_acquisition_to(
        &self,
        external_provider: &str,
        external_id: &str,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        cancellation_token: &CancellationToken,
        requested_by: Option<Vec<String>>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
        place: AcquisitionPlace,
    ) -> anyhow::Result<String> {
        self.download_song_internal(
            external_provider,
            external_id,
            DownloadOptions {
                trigger_album_download,
                force_permanent,
                source_override,
                requested_by,
                upgrade_search,
                replacement,
                place,
                ..Default::default()
            },
            cancellation_token,
        )
        .await
    }

    fn hold_album_listing(&self, external_provider: &str, album: &Album) {
        if external_provider == self.provider_name() {
            self.hold_album(album);
        }
    }

    async fn finish_album_listing(&self, external_provider: &str, album: &Album) {
        if external_provider == self.provider_name() {
            self.finish_held_album(album).await;
        }
    }

    async fn download_album_with_source(
        &self,
        external_provider: &str,
        album_external_id: &str,
        source: DownloadSource,
        suppress_summary: bool,
        cancellation_token: &CancellationToken,
        requested_by: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        if external_provider != self.provider_name() {
            return Ok(false);
        }
        self.download_remaining_album_tracks(
            album_external_id,
            "",
            Some(source),
            suppress_summary,
            cancellation_token,
            requested_by,
        )
        .await
    }

    async fn download_ahead(
        &self,
        external_provider: &str,
        external_id: &str,
        folder: &Path,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Ahead> {
        let path = self
            .download_song_internal(
                external_provider,
                external_id,
                DownloadOptions {
                    suppress_notify: true,
                    // The lossless step alone: SpotiFLAC rides on it, and YouTube and Lidarr are
                    // never asked.
                    source_override: Some(DownloadSource::Soulseek),
                    place: AcquisitionPlace {
                        pool_members: None,
                        flac_only: true,
                        ..Default::default()
                    },
                    prefetch: true,
                    ahead_folder: Some(folder.to_path_buf()),
                    ..Default::default()
                },
                cancellation_token,
            )
            .await?;
        let path = PathBuf::from(path);
        Ok(if path.starts_with(folder) {
            Ahead::Kept { path }
        } else {
            Ahead::AlreadyYours { path }
        })
    }

    fn get_download_status(&self, song_id: &str) -> Option<DownloadInfo> {
        self.active_download(song_id)
    }

    fn has_active_downloads(&self) -> bool {
        self.active_downloads
            .lock()
            .values()
            .any(|info| info.status == DownloadStatus::InProgress)
    }

    async fn place_held(&self, held: &crate::services::lidarr::HeldPlacement) -> Option<String> {
        self.place_held_file(held).await
    }

    async fn drop_held(&self, held: &crate::services::lidarr::HeldPlacement, users: &[String]) -> bool {
        self.drop_held_copy(held, users).await
    }

    async fn get_local_path_if_exists(&self, external_provider: &str, external_id: &str) -> Option<String> {
        if external_provider != self.provider_name() {
            return None;
        }

        // Check local library.
        if let Some(local) = self
            .local_library
            .get_local_path_for_external_song(external_provider, external_id)
            .await
            .filter(|path| file_exists(path))
        {
            return Some(local);
        }

        // Check cache directory.
        self.get_cached_file_path(external_provider, external_id)
            .filter(|path| file_exists(path))
    }

    async fn is_available(&self) -> bool {
        self.backend.is_available(self).await
    }

    async fn get_direct_stream(
        &self,
        external_provider: &str,
        external_id: &str,
        range_header: Option<&str>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        self.backend
            .get_direct_stream(
                self,
                external_provider,
                external_id,
                range_header,
                cancellation_token,
            )
            .await
    }
}
