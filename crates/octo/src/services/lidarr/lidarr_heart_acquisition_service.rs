//! Hearts fetched through Lidarr. Which song an imported track is (`match_song`) is
//! `octo_core::lidarr::lidarr_heart_acquisition_service`; what follows a search, from the grab to
//! the last download accounted for, is `lidarr_follow.rs`; the album's tracks held until the
//! hearted song passes its gate are `lidarr_hold_folder.rs`; Lidarr's indexers are
//! `lidarr_health.rs`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardRecord, HeardRecording, HeartGate};
use octo_core::fingerprint::heart_release::{HeardIdentity, HeartSong};
use octo_core::lidarr::lidarr_client::LidarrAlbum;
use octo_core::lidarr::lidarr_heart_acquisition_service::{IndexerHealth, match_song};
use octo_core::lidarr::lidarr_track_fetcher::is_lossless;
use octo_core::lidarr::{LidarrAlbumCandidate, LidarrError, LidarrImportedTrack, LidarrReleasePick};
use octo_core::models::domain::{Album, Song};
use octo_core::notifications::{NotificationEvent, NotificationEventType};
use octo_core::settings::{DownloadSource, LidarrCompletionMode, LidarrSettings, SettingsStore};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::lidarr_pending_albums::{FollowEnd, HELD_FOR_LIDARR, SongGate, StopWaiting, UnsettledFollow};
use super::lidarr_pick::{
    ALREADY_LOOKING, LidarrArtistMusic, delete_moved_record, put_back_unsearched,
    put_back_unsearched_after_restart, users_queue_item, wait_for_rescans,
};
use super::{
    LidarrAlbumClaims, LidarrClient, LidarrImport, LidarrImportHandoff, LidarrPendingAlbums, PendingAdd,
    PendingAlbum,
};
use crate::services::common::album_gates::{AlbumGateHold, AlbumGates};
use crate::services::common::song_finder::LIDARR_LEFT_ALONE;
use crate::services::common::track_acquisition_queue::acquisition_key;
use crate::services::common::{AcquisitionHeld, AcquisitionState, AcquisitionTracker, HeartGates};
use crate::services::fingerprint::MusicBrainzClient;
use crate::services::fingerprint::download_verification_service::LabelSource;
use crate::services::fingerprint::download_verification_service::{pick_contexts, song_in};
use crate::services::fingerprint::music_brainz_client::{
    CatalogQuestions, HeartBusy, HeartQuestions, Lane, heart_song,
};
use crate::services::i_download_service::IDownloadService;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::library::{LibraryActionJournal, LibraryOwnership, OwnedCopy, UpgradeAsk, UpgradeQueue};
use crate::services::metadata::DeezerMetadataService;
use crate::services::notifications::NotificationService;
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};
use crate::services::subsonic::NavidromeIdentityService;

/// Hearts through Lidarr.
#[async_trait]
pub trait ILidarrHeartAcquisitionService: Send + Sync {
    /// True once the song is in the library: Lidarr brought it and it passed Octo's checks, or it
    /// was there already. False when Lidarr could not get it, so the heart's next source tries.
    /// Waits for Lidarr's import, up to Lidarr's import timeout; a download still moving then is
    /// followed on, and what it brings later is still Octo's. `Err` when the attempt itself
    /// fails. `origin` says why it runs and `gates` are the gates of the hearts or plays that
    /// asked, each judged on its own.
    async fn try_acquire_track(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool>;

    /// True once every song of the album is in the library; false leaves the rest to
    /// the next source, which skips the songs that did land.
    async fn try_acquire_album(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool>;

    /// Lidarr says it imported files of an album. An import of one of Octo's own downloads (its
    /// download id on a watch list of Octo's, from a download client) has the follow holding that
    /// download read Lidarr at once, or start again when a restart left it waiting, whether or not
    /// Lidarr calls it an upgrade. Any other import is not Octo's and is left alone: a follow on
    /// its artist only reads Lidarr at once, so a file of the user's is seen before Octo's own
    /// import could replace it. Which files are Octo's is decided by the follow, file by file.
    async fn lidarr_imported(&self, _event: LidarrImportEvent) -> LidarrImportOutcome {
        LidarrImportOutcome::NotOurs
    }

    /// Whether Lidarr's last try for the song left Lidarr unable to look for it: every album it
    /// would have searched belongs to an artist that already has music in the user's Lidarr, or
    /// Lidarr has no indexer that can search.
    fn left_alone(&self, _provider: &str, _external_id: &str) -> bool {
        false
    }

    /// What the song's or the hearted album's last Lidarr step, run last in the chain, waits on:
    /// MusicBrainz's question or Deezer for its walk or its album's barcode lookup, or Lidarr's
    /// failing indexers, so the heart holds for it. Taken: a second read answers None.
    fn waits_on(&self, _provider: &str, _external_id: &str) -> Option<LidarrWaits> {
        None
    }

    /// The hearted song's file Lidarr brought that waits in the one hold path for what its gate
    /// needs (AcoustID answering), as any source's held file does: the heart holds with it.
    /// Taken: a second read answers None.
    fn held_file(&self, _provider: &str, _external_id: &str) -> Option<AcquisitionHeld> {
        None
    }

    /// Lidarr's webhook says its health changed (`HealthRestored`) or it grabbed a release
    /// (`Grab`): a heart waiting for Lidarr's indexers looks whether they search again.
    fn lidarr_health_event(&self) {}

    /// A play or a heart of the song: a follow it belongs to reads whether Lidarr's download made
    /// any progress since the last one.
    fn song_event(&self, _provider: &str, _external_id: &str) {}

    /// `user` took their heart on the song or the hearted album back: a follow nobody's heart is
    /// on any more ends.
    fn unhearted(&self, _provider: &str, _external_id: &str, _user: &str) {}

    /// Whether an open follow is on the song or the hearted album.
    fn follows_heart(&self, _provider: &str, _external_id: &str) -> bool {
        false
    }

    /// `user` stopped waiting for Lidarr on a song whose kept copy, kept for them, waits for a
    /// follow: the copy is placed now and they leave the follow's waits for the song. A follow
    /// nobody's heart waits on any more ends and Octo's downloads there are removed; one others
    /// still wait on goes on.
    async fn stop_waiting_for_lidarr(&self, _provider: &str, _external_id: &str, _user: &str) -> StopWaiting {
        StopWaiting::NothingWaits
    }

    /// Whether a kept copy of the song waits for a follow, so its row offers "Stop waiting for
    /// Lidarr".
    fn waits_for_lidarr(&self, _provider: &str, _external_id: &str) -> bool {
        false
    }

    /// The people who may stop waiting for Lidarr on the song: everyone a kept copy of it that
    /// waits for an open follow was kept for, in the order they were listed.
    fn stop_waiters(&self, _provider: &str, _external_id: &str) -> Vec<String> {
        Vec::new()
    }

    /// The follows Octo cannot settle: a download of Octo's left Lidarr's queue while its client
    /// lists nothing else, so Octo cannot prove it gone, and the follow keeps reading Lidarr.
    fn unsettled_follows(&self) -> Vec<UnsettledFollow> {
        Vec::new()
    }

    /// The admin's "Put Lidarr back" on a follow Octo cannot settle (`follow`, the album's foreign
    /// id): the downloads it cannot see count as gone, the follow ends, and its pick is put back
    /// as any pick that ends without a gated import is. False when the follow is not one Octo
    /// cannot settle.
    fn put_lidarr_back(&self, _follow: &str) -> bool {
        false
    }
}

/// What Lidarr's webhook says it imported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LidarrImportEvent {
    /// The album's MusicBrainz release group id.
    pub foreign_album_id: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// The imported files, as Lidarr sees them.
    pub paths: Vec<String>,
    /// The files replaced files Lidarr already had. It never decides whose import it was.
    pub is_upgrade: bool,
    /// The download the files came from (`downloadId`): a folder import may carry one Lidarr
    /// guessed.
    pub download_id: Option<String>,
    /// The download client that delivered them (`downloadClient`); a folder import has none.
    pub download_client: Option<String>,
}

/// What a Lidarr step run last in a heart's chain waits on, so the heart holds for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LidarrWaits {
    /// MusicBrainz or Deezer did not answer for the walk.
    Answer(HeartBusy),
    /// Every pick that searched grabbed nothing, and Lidarr's indexers are failing.
    Indexers,
}

/// The downloads line of a song Lidarr could not search for: no indexer of it searches.
pub const NO_INDEXER_SEARCHES: &str = "Lidarr has no indexer that can search";

/// Told when Lidarr's indexers search again, so every heart waiting for them runs again.
pub type IndexersBack = Arc<dyn Fn() + Send + Sync>;

/// What became of a webhook's import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LidarrImportOutcome {
    /// A running follow reads Lidarr now.
    Woke,
    /// A follow of Octo's that was not running starts again, and reads Lidarr.
    Late,
    /// Not an album Octo follows.
    NotOurs,
}

/// Pauses the heard-master rounds of an acquisition (its key and its gates) while it waits on
/// something outside it, such as a Lidarr follow.
pub type PauseRounds = Arc<dyn Fn(&str, &[HeartGate]) + Send + Sync>;

/// The optional services the heart service works with. The download pipeline, the journal, the
/// ownership check and the upgrade queue sit on top of the acquisition queue that the heart
/// coordinator, and so this service, feeds; each is None where the service is not there. The
/// library-action settings are read from the settings store.
#[derive(Default, Clone)]
pub struct LidarrHeartExtras {
    /// The live progress list. Lidarr reports no bytes, so it only hears accepted from here;
    /// the pipeline moves each song's row on from there.
    pub tracker: Option<Arc<AcquisitionTracker>>,
    pub music_brainz: Option<Arc<MusicBrainzClient>>,
    pub claims: Option<Arc<LidarrAlbumClaims>>,
    /// A fresh one when None.
    pub imports: Option<Arc<LidarrImportHandoff>>,
    pub ids: Option<Arc<ExternalIdRegistry>>,
    pub downloads: Option<Arc<dyn IDownloadService>>,
    pub journal: Option<Arc<LibraryActionJournal>>,
    pub ownership: Option<Arc<LibraryOwnership>>,
    pub upgrades: Option<Arc<UpgradeQueue>>,
    /// The albums asked for and their follows; one in memory when None.
    pub pending: Option<Arc<LidarrPendingAlbums>>,
    /// The catalog side of a song's identity and walk; Deezer when None.
    pub catalog: Option<Arc<dyn CatalogQuestions>>,
    /// Pauses a heart's heard-master rounds while it waits for a follow.
    pub rounds: Option<PauseRounds>,
    /// The gates of hearted albums' chains: an album heart's import is filed track by track by
    /// its own sound against them.
    pub album_gates: Option<Arc<AlbumGates>>,
    /// What files one file of a hearted album's import among its tracks by its own sound.
    pub gates: Option<Arc<dyn HeartGates>>,
}

/// Files the tracks of a hearted album an import held (the album's provider and id, the person,
/// the origin) as held album tracks of that person's heart.
pub type AlbumHolds = Arc<dyn Fn(&str, &str, &str, AcquisitionOrigin) + Send + Sync>;

/// Hearts through Lidarr. Lidarr only fetches whole albums, so a heart becomes an album search;
/// what Lidarr imports of Octo's own downloads is handed, song by song, to the download pipeline a
/// Soulseek file goes through (AcoustID, the spectrum, release matching, tags, placing, history),
/// with Lidarr as the place the file came from (LidarrImportHandoff). A song already in the
/// library or removed with a library action is deleted from what Lidarr brought instead.
///
/// One job per album, alive while its search is followed: a second heart or a play on an album
/// Lidarr is already fetching joins it and starts no search. Each heart waits up to Lidarr's
/// import timeout; the follow goes on until every download of Octo's is accounted for.
#[derive(Clone)]
pub struct LidarrHeartAcquisitionService {
    inner: Arc<Inner>,
}

struct Inner {
    client: Arc<LidarrClient>,
    metadata: Arc<dyn IMusicMetadataService>,
    deezer: Arc<DeezerMetadataService>,
    /// The Lidarr, Subsonic and library action settings and `Library:DownloadPath`, all read at
    /// use.
    settings: Arc<SettingsStore>,
    notifications: Arc<NotificationService>,
    tracker: Option<Arc<AcquisitionTracker>>,
    music_brainz: Option<Arc<MusicBrainzClient>>,
    claims: Option<Arc<LidarrAlbumClaims>>,
    imports: Arc<LidarrImportHandoff>,
    ids: Option<Arc<ExternalIdRegistry>>,
    downloads: Option<Arc<dyn IDownloadService>>,
    journal: Option<Arc<LibraryActionJournal>>,
    ownership: Option<Arc<LibraryOwnership>>,
    upgrades: Option<Arc<UpgradeQueue>>,
    pending: Arc<LidarrPendingAlbums>,
    catalog: Arc<dyn CatalogQuestions>,
    rounds: Option<PauseRounds>,
    album_gates: Option<Arc<AlbumGates>>,
    gates: Option<Arc<dyn HeartGates>>,
    /// Who files the tracks an album heart's import held.
    album_holds: Mutex<Option<AlbumHolds>>,
    /// The replacements' fetches waiting for a song of an album, by foreign album id ignoring
    /// case.
    fetches: Mutex<HashMap<String, Vec<replacement::FetchWaiter>>>,
    /// What the user's Lidarr holds of an artist.
    music: LidarrArtistMusic,
    /// The songs whose last try left Lidarr alone, by provider and id ignoring case.
    left_alone: Mutex<HashSet<String>>,
    /// What each song's last Lidarr step, run last, waited on, by provider and id ignoring case.
    busy: Mutex<HashMap<String, LidarrWaits>>,
    /// Each hearted song's file that waits in the one hold path, by provider and id ignoring case.
    held_files: Mutex<HashMap<String, AcquisitionHeld>>,
    /// Lidarr's indexers, while a heart waits for them.
    indexers: health::IndexerWatch,
    /// Stops every follow this service runs, for tests that start the service again.
    stopping: CancellationToken,
    /// Put backs Lidarr failed, tried again when it next answers and at the poll pace.
    put_backs: PutBacks,
    /// Held while a restart puts back the picks it cut short before their search: a fresh pick
    /// waits until that is done, so it never merges into an entry being put back.
    restart: Arc<tokio::sync::RwLock<()>>,
    /// The service itself, for work it starts on its own.
    me: OnceLock<std::sync::Weak<Inner>>,

    /// By foreign album id, ignoring case.
    jobs: Mutex<HashMap<String, Arc<AlbumJob>>>,
    timing: Mutex<Timing>,
}

/// The put backs owed, by foreign album id, and what wakes the work that tries them again.
#[derive(Default)]
struct PutBacks {
    owed: Mutex<HashSet<String>>,
    /// Lidarr answered a call of Octo's while a put back is owed.
    answered: tokio::sync::Notify,
    /// The work that tries them again runs.
    running: std::sync::atomic::AtomicBool,
}

/// What a pick's album needs for its pin: the release the person listened from when the album is
/// its group, and the recordings whose carriers name the album's releases.
#[derive(Debug, Clone, Default)]
struct PinHint {
    listened_release: Option<String>,
    recordings: Vec<String>,
    lane: Option<Lane>,
}

/// What a pick asks of its album's search beyond the album: what its pin reads, and a release
/// picked in Find songs that the search grabs instead of Lidarr's own search.
#[derive(Default)]
struct Ask {
    hint: Option<PinHint>,
    release: Option<LidarrReleasePick>,
    /// The hearted album the album is searched for, as `provider:id`.
    album_heart: Option<String>,
}

/// How one pick ended for a heart.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PickEnd {
    /// The song is in the library.
    Landed,
    /// Lidarr was not used for the album, with the downloads line that says why when there is
    /// one.
    Skipped(Option<&'static str>),
    /// Lidarr searched, the follow ended, and the song did not land.
    Missed,
    /// Lidarr searched and grabbed nothing of Octo's.
    NoGrab,
    /// The import timeout passed while Lidarr's download still moves: the heart's step ends, and
    /// the follow goes on.
    TimedOut,
    /// The song's file waits in the one hold path for what its gate needs: the heart holds.
    Held,
}

/// How a pick's preparation ended.
enum Prepared {
    /// The search was posted for the album.
    Searched {
        album_id: i32,
    },
    Skipped(Option<&'static str>),
}

/// The service's poll intervals.
#[derive(Clone, Copy)]
struct Timing {
    poll_override: Option<Duration>,
    /// The pace while a download of Octo's is finished and waits for its import, when Octo
    /// watches for a rescan or another import that must not run before it.
    turn_poll: Duration,
}

/// An error a joined heart sees as the creator's job saw it (a faulted task, awaited twice).
type SharedResult = Result<(), Arc<anyhow::Error>>;

/// The keys of the songs in the library: a hearted song's own id, and every song's artist and
/// title, so a heart that joined late still finds its song among the album's other tracks.
/// Shared with the job's handoffs.
type Landed = Arc<Mutex<HashSet<String>>>;

/// How a heart's wait on a job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waited {
    Landed,
    Ended,
    TimedOut,
    /// The song's file waits in the one hold path.
    Held,
}

/// One album Lidarr is fetching for one or more hearts or plays, alive while its search is
/// followed.
struct AlbumJob {
    candidate: LidarrAlbumCandidate,
    album: Album,
    songs: Mutex<JobSongs>,
    /// Who asked, first spelling kept, compared ignoring case.
    requesters: Mutex<Vec<String>>,
    /// Whose heart is on it.
    holders: Mutex<Vec<String>>,
    /// Each person's gate for each song, the first kept per song and person.
    gates: Mutex<Vec<SongGate>>,
    submitted: OnceLock<Shared<BoxFuture<'static, SharedResult>>>,
    landed: Landed,
    /// Moves on whenever a song lands, a song's file is held, or the follow ends.
    progress: watch::Sender<u64>,
    /// The follow ended, or Lidarr was not used for the album: nothing more lands.
    ended: Mutex<bool>,
    /// The follow ended with no grab of Octo's.
    no_grab: Mutex<bool>,
    /// Why a hearted song did not land, by its key, for the progress list.
    missed: Mutex<HashMap<String, String>>,
    /// What the album's pin reads, from the first heart whose walk picked it.
    hint: Mutex<Option<PinHint>>,
    /// Lidarr was not used for the album, and the line that says why.
    skipped: Mutex<Option<Option<&'static str>>>,
    /// How many hearts wait on each song now, by its key: a song nobody waits for is a late
    /// import's.
    waiting: Mutex<HashMap<String, usize>>,
    /// The songs whose file waits in the one hold path, for the hearts waiting on them.
    held: Mutex<HashMap<String, AcquisitionHeld>>,
    /// The people whose own gate passed a hearted song's file, by the song's key.
    accepted: Mutex<HashMap<String, Vec<String>>>,
    /// One look at the album's hold folder at a time.
    settling: tokio::sync::Mutex<()>,
    /// The job's own hold on each hearted album's gate, so a late import is still filed against
    /// it once the album's chain has ended; let go with the job.
    gate_holds: Mutex<Vec<AlbumGateHold>>,
    /// One file of an album heart's import filed at a time.
    filing: tokio::sync::Mutex<()>,
}

/// A heart waiting on a song of a job, counted while it waits.
struct Waiting<'a> {
    job: &'a AlbumJob,
    keys: Vec<String>,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut waiting = self.job.waiting.lock();
        for key in &self.keys {
            if let Some(count) = waiting.get_mut(key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    waiting.remove(key);
                }
            }
        }
    }
}

#[derive(Default)]
struct JobSongs {
    songs: Vec<Song>,
    match_by_number: bool,
    album_heart: bool,
    /// A replacement's fetch asked for the album.
    replacement: bool,
    /// The release picked in Find songs that the album's search grabs instead.
    release: Option<LidarrReleasePick>,
    /// The hearted albums the album is searched for, as `provider:id`.
    album_hearts: Vec<String>,
}

impl AlbumJob {
    fn new(candidate: LidarrAlbumCandidate, album: Album) -> Self {
        AlbumJob {
            candidate,
            album,
            songs: Mutex::new(JobSongs::default()),
            requesters: Mutex::new(Vec::new()),
            holders: Mutex::new(Vec::new()),
            gates: Mutex::new(Vec::new()),
            submitted: OnceLock::new(),
            landed: Arc::default(),
            progress: watch::channel(0).0,
            ended: Mutex::new(false),
            no_grab: Mutex::new(false),
            missed: Mutex::new(HashMap::new()),
            hint: Mutex::new(None),
            skipped: Mutex::new(None),
            waiting: Mutex::new(HashMap::new()),
            held: Mutex::new(HashMap::new()),
            accepted: Mutex::new(HashMap::new()),
            settling: tokio::sync::Mutex::new(()),
            gate_holds: Mutex::new(Vec::new()),
            filing: tokio::sync::Mutex::new(()),
        }
    }

    /// A job over a follow a restart or Lidarr's webhook starts again: nothing to submit.
    fn resumed(candidate: LidarrAlbumCandidate, album: Album, entry: &PendingAlbum) -> Self {
        let job = AlbumJob::new(candidate, album);
        {
            let mut songs = job.songs.lock();
            songs.replacement = entry.replacement;
            songs.album_hearts = entry.album_hearts.clone();
        }
        *job.requesters.lock() = entry.requested_by.clone();
        *job.holders.lock() = entry.holders.clone();
        *job.gates.lock() = entry.gates.clone();
        let ready: BoxFuture<'static, SharedResult> = async { Ok(()) }.boxed();
        let _ = job.submitted.set(ready.shared());
        job
    }

    /// Whether the album's tracks wait in its hold folder until a hearted song passes: a heart's
    /// or a play's gate is on one of its songs. An album heart's tracks are each its own.
    fn gated(&self) -> bool {
        !self.album_heart() && self.gates.lock().iter().any(SongGate::personal)
    }

    /// Whether only replacements asked for the album: no heart's, play's or album heart's file
    /// comes of its import, so a file no fetch waits for is the library's in no way.
    fn replacement_only(&self) -> bool {
        let asked = {
            let songs = self.songs.lock();
            songs.replacement && !songs.album_heart
        };
        asked && !self.gates.lock().iter().any(SongGate::personal)
    }

    /// Whether each file of the job's import is filed among a hearted album's tracks by its own
    /// sound: an album heart's gate is on its tracks.
    fn files_by_sound(&self) -> bool {
        self.gates
            .lock()
            .iter()
            .any(|gate| gate.gate.origin == AcquisitionOrigin::AlbumHeart)
    }

    /// Whether a heart waits on the song now.
    fn waits_for(&self, key: &str) -> bool {
        self.waiting.lock().get(key).is_some_and(|count| *count > 0)
    }

    /// The song's file waits in the one hold path: the hearts waiting on it hold with it.
    fn hold(&self, key: &str, held: AcquisitionHeld) {
        self.held.lock().insert(key.to_string(), held);
        self.progress.send_modify(|n| *n += 1);
    }

    #[allow(clippy::too_many_arguments)] // who asked, why, and what they heard
    fn join(
        &self,
        songs: &[Song],
        match_by_number: bool,
        album_heart: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[SongGate],
    ) {
        {
            let mut known = self.songs.lock();
            for song in songs {
                if !known.songs.iter().any(|k| key_of(k) == key_of(song)) {
                    known.songs.push(song.clone());
                }
            }
            known.match_by_number |= match_by_number;
            known.album_heart |= album_heart;
            known.replacement |= origin == AcquisitionOrigin::Replacement;
        }
        if let Some(name) = requested_by.filter(|r| !dotnet::is_blank(r)) {
            let name = name.trim();
            let mut requesters = self.requesters.lock();
            if !requesters.iter().any(|r| dotnet::eq_ignore_case(r, name)) {
                requesters.push(name.to_string());
            }
            if matches!(origin, AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart) {
                let mut holders = self.holders.lock();
                if !holders.iter().any(|h| dotnet::eq_ignore_case(h, name)) {
                    holders.push(name.to_string());
                }
            }
        }
        let mut known = self.gates.lock();
        for gate in gates {
            if !known.iter().any(|k| k.same_person(gate)) {
                known.push(gate.clone());
            }
        }
    }

    /// What the job adds to its album's saved entry: its songs, people and gates.
    fn saved(&self) -> PendingAlbum {
        let (songs, replacement, album_hearts) = {
            let songs = self.songs.lock();
            (
                songs.songs.iter().filter_map(song_entry_key).collect(),
                songs.replacement,
                songs.album_hearts.clone(),
            )
        };
        PendingAlbum {
            foreign_album_id: self.candidate.foreign_album_id.clone(),
            songs,
            requested_by: self.requesters.lock().clone(),
            holders: self.holders.lock().clone(),
            gates: self.gates.lock().clone(),
            replacement,
            album_hearts,
            ..Default::default()
        }
    }

    /// `user` stopped waiting for Lidarr on `song`: their gates for it are marked stopped (they
    /// still judge Lidarr's file for it), and they stop holding the job unless a heart's gate of
    /// theirs still waits on it, so no later save of the job has them hold the follow again.
    fn stopped_waiting(&self, song: &str, user: &str) {
        let theirs = |name: &str| dotnet::eq_ignore_case(name.trim(), user.trim());
        let waits = {
            let mut gates = self.gates.lock();
            for gate in gates
                .iter_mut()
                .filter(|gate| gate.song == song && theirs(&gate.gate.user) && gate.personal())
            {
                gate.stopped = true;
            }
            gates
                .iter()
                .any(|gate| theirs(&gate.gate.user) && gate.holds_follow())
        };
        if !waits {
            self.holders.lock().retain(|holder| !theirs(holder));
        }
    }

    fn match_by_number(&self) -> bool {
        self.songs.lock().match_by_number
    }

    fn album_heart(&self) -> bool {
        self.songs.lock().album_heart
    }

    fn view(&self) -> Album {
        Album {
            title: self.album.title.clone(),
            artist: self.album.artist.clone(),
            year: self.album.year,
            songs: self.songs.lock().songs.clone(),
            ..Default::default()
        }
    }

    fn asked_by(&self) -> Option<Vec<String>> {
        let mut names = self.requesters.lock().clone();
        if names.is_empty() {
            return None;
        }
        names.sort_by(|a, b| dotnet::compare_ordinal_ignore_case(a, b));
        Some(names)
    }

    fn missed_for(&self, key: &str) -> Option<String> {
        self.missed.lock().get(key).cloned()
    }

    fn has_landed(&self, song: &Song) -> bool {
        let landed = self.landed.lock();
        landed.contains(&key_of(song)) || landed.contains(&title_key(Some(&song.artist), Some(&song.title)))
    }

    fn is_ended(&self) -> bool {
        *self.ended.lock()
    }

    /// A song landed: the hearts waiting look. `track` is Lidarr's match of the file, which names
    /// a song too; None for a file its own sound filed, where only `song` landed.
    fn add_landed(&self, song: Option<&Song>, track: Option<&LidarrImportedTrack>, album: &Album) {
        {
            let mut landed = self.landed.lock();
            if let Some(song) = song {
                landed.insert(key_of(song));
                landed.insert(title_key(Some(&song.artist), Some(&song.title)));
            }
            if let Some(track) = track {
                landed.insert(title_key(
                    Some(track.artist.as_deref().unwrap_or(&album.artist)),
                    Some(&track.title),
                ));
            }
        }
        self.progress.send_modify(|n| *n += 1);
    }

    /// Nothing more lands for this job.
    fn end(&self) {
        *self.ended.lock() = true;
        self.progress.send_modify(|n| *n += 1);
    }

    /// Whether the song is in for `user`: a song a heart's or a play's gate is on only once that
    /// person's own gate passed its file; any other song once it landed.
    fn landed_for(&self, song: &Song, user: Option<&str>) -> bool {
        let key = key_of(song);
        let gated = user.is_some_and(|user| {
            self.gates.lock().iter().any(|gate| {
                gate.personal()
                    && gate.song == key
                    && dotnet::eq_ignore_case(gate.gate.user.trim(), user.trim())
            })
        });
        if !gated {
            return self.has_landed(song);
        }
        let user = user.unwrap_or_default();
        self.accepted.lock().get(&key).is_some_and(|users| {
            users
                .iter()
                .any(|known| dotnet::eq_ignore_case(known.trim(), user.trim()))
        })
    }

    /// Whether the song's file waits in the one hold path for `user` (anyone, when the hold names
    /// nobody).
    fn held_for(&self, song: &Song, user: Option<&str>) -> bool {
        self.held.lock().get(&key_of(song)).is_some_and(|held| {
            held.users.is_empty()
                || user.is_some_and(|user| {
                    held.users
                        .iter()
                        .any(|known| dotnet::eq_ignore_case(known.trim(), user.trim()))
                })
        })
    }

    /// Waits until every song of `songs` is in for `user`, one's file is held for them, the job
    /// ends, or `deadline` passes. The songs count as waited on meanwhile.
    async fn wait_for(&self, songs: &[Song], deadline: Instant, user: Option<&str>) -> Waited {
        let keys: Vec<String> = songs.iter().map(key_of).collect();
        {
            let mut waiting = self.waiting.lock();
            for key in &keys {
                *waiting.entry(key.clone()).or_insert(0) += 1;
            }
        }
        let _waiting = Waiting { job: self, keys };
        let mut changes = self.progress.subscribe();
        loop {
            if !songs.is_empty() && songs.iter().all(|song| self.landed_for(song, user)) {
                return Waited::Landed;
            }
            if songs.iter().any(|song| self.held_for(song, user)) {
                return Waited::Held;
            }
            if self.is_ended() {
                return Waited::Ended;
            }
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Waited::Ended;
                    }
                }
                () = tokio::time::sleep_until(deadline.into()) => return Waited::TimedOut,
            }
        }
    }
}

fn key_of(song: &Song) -> String {
    match song.external_id.as_deref() {
        Some(id) if !dotnet::is_blank(id) => {
            format!("{}:{id}", song.external_provider.as_deref().unwrap_or(""))
        }
        _ => title_key(Some(&song.artist), Some(&song.title)),
    }
}

/// A song as a follow's entry lists it: `provider:id`, when it has both.
fn song_entry_key(song: &Song) -> Option<String> {
    match (song.external_provider.as_deref(), song.external_id.as_deref()) {
        (Some(provider), Some(id)) if !dotnet::is_blank(provider) && !dotnet::is_blank(id) => {
            Some(format!("{provider}:{id}"))
        }
        _ => None,
    }
}

fn title_key(artist: Option<&str>, title: Option<&str>) -> String {
    format!(
        "t:{}",
        SongIdentity::match_key(artist.unwrap_or(""), title.unwrap_or(""))
    )
}

/// The MusicBrainz id of the artist a lookup row files the album under: the album's own Lidarr
/// artist, which Lidarr's import keeps.
fn artist_of(candidate: &LidarrAlbumCandidate) -> Option<String> {
    let artist = candidate.resource.get("artist")?.as_object()?;
    ["foreignArtistId", "mbId"]
        .iter()
        .find_map(|key| artist.get(*key).and_then(|id| id.as_str()))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// MusicBrainz's id of Various Artists, the artist a compilation is filed under.
const VARIOUS_ARTISTS: &str = "89ad4ac3-39f7-470e-963a-56509c546377";

fn left_alone_key(provider: &str, external_id: &str) -> String {
    dotnet::ordinal_ignore_case_key(&format!("{}:{}", provider.trim(), external_id.trim()))
}

/// The MusicBrainz lane a song's walk asks on: a person waiting on a pick asks in the foreground,
/// imports and replacements in the background, every other origin on the heart's lane.
fn lane_for(origin: AcquisitionOrigin) -> Lane {
    match origin {
        AcquisitionOrigin::SongFinderPick => Lane::Foreground,
        AcquisitionOrigin::Import | AcquisitionOrigin::Replacement => Lane::Background,
        _ => Lane::Heart,
    }
}

/// Whether a walk that could not get an answer waits for it when Lidarr is the chain's last
/// source: a heart holds, and an import goes back on its trickle. Plays never wait, a family
/// request goes on by name, and a pick or a replacement has its own answer for a busy lookup.
fn waits_for_answers(origin: AcquisitionOrigin) -> bool {
    matches!(
        origin,
        AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Import
    )
}

/// Where a song's walk starts.
enum WalkStart {
    Picks(Walk),
    /// No recording to walk from: the album is found by name.
    NoIdentity,
    /// MusicBrainz or Deezer did not answer.
    Busy(HeartBusy),
}

/// The release groups Lidarr is asked for, in the walk's order: the listened-from group first,
/// known without the carriers, which are asked only when the walk needs a second pick. Each group
/// is given once.
struct Walk {
    questions: HeartQuestions,
    song: HeartSong,
    identity: HeardIdentity,
    picks: VecDeque<String>,
    walked: bool,
    seen: HashSet<String>,
    /// The carriers' question that did not answer, which ended the walk early.
    busy: Option<HeartBusy>,
}

impl Walk {
    async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(pick) = self.picks.pop_front() {
                if self.seen.insert(pick.to_ascii_lowercase()) {
                    return Some(pick);
                }
                continue;
            }
            if self.walked {
                return None;
            }
            self.walked = true;
            match self.questions.walk(&self.song, &self.identity).await {
                Ok(picks) => self.picks.extend(picks),
                Err(busy) => {
                    let what = match &busy {
                        HeartBusy::MusicBrainz(question) => format!("MusicBrainz ({question})"),
                        HeartBusy::Deezer => "Deezer".to_string(),
                    };
                    warn!(
                        "{what} did not answer for the walk of '{} - {}'; no further pick",
                        self.song.artist, self.song.title
                    );
                    self.busy = Some(busy);
                    return None;
                }
            }
        }
    }

    /// What a pick's pin reads: the listened-from release on the listened-from group, and the
    /// heard recordings.
    fn hint(&self, pick: &str) -> PinHint {
        let listened = self
            .identity
            .listened_group
            .as_deref()
            .is_some_and(|group| dotnet::eq_ignore_case(group, pick));
        PinHint {
            listened_release: self.identity.listened_release.clone().filter(|_| listened),
            recordings: self.identity.recordings.clone(),
            lane: Some(self.questions.lane()),
        }
    }
}

fn tracked_keys(album: &Album) -> Vec<(String, String)> {
    album
        .songs
        .iter()
        .filter_map(
            |s| match (s.external_provider.as_deref(), s.external_id.as_deref()) {
                (Some(p), Some(id)) if !dotnet::is_blank(p) && !dotnet::is_blank(id) => {
                    Some((p.to_string(), id.to_string()))
                }
                _ => None,
            },
        )
        .collect()
}

/// Why an imported file should not join the library.
struct DropReason {
    reason: String,
    owned: Option<OwnedCopy>,
}

impl LidarrHeartAcquisitionService {
    pub fn new(
        client: Arc<LidarrClient>,
        metadata: Arc<dyn IMusicMetadataService>,
        deezer: Arc<DeezerMetadataService>,
        settings: Arc<SettingsStore>,
        nav_identity: NavidromeIdentityService,
        notifications: Arc<NotificationService>,
        extras: LidarrHeartExtras,
    ) -> Self {
        let catalog = extras
            .catalog
            .unwrap_or_else(|| deezer.clone() as Arc<dyn CatalogQuestions>);
        let pending = extras
            .pending
            .unwrap_or_else(|| Arc::new(LidarrPendingAlbums::new(None)));
        let music = LidarrArtistMusic::new(client.clone(), settings.clone(), nav_identity)
            .with_octos_copies(pending.clone());
        let inner = Arc::new(Inner {
            client,
            metadata,
            deezer,
            settings,
            notifications,
            catalog,
            music,
            left_alone: Mutex::new(HashSet::new()),
            busy: Mutex::new(HashMap::new()),
            stopping: CancellationToken::new(),
            tracker: extras.tracker,
            music_brainz: extras.music_brainz,
            claims: extras.claims,
            imports: extras.imports.unwrap_or_default(),
            ids: extras.ids,
            downloads: extras.downloads,
            journal: extras.journal,
            ownership: extras.ownership,
            upgrades: extras.upgrades,
            pending,
            rounds: extras.rounds,
            album_gates: extras.album_gates,
            gates: extras.gates,
            album_holds: Mutex::new(None),
            fetches: Mutex::new(HashMap::new()),
            held_files: Mutex::new(HashMap::new()),
            indexers: health::IndexerWatch::default(),
            put_backs: PutBacks::default(),
            restart: Arc::new(tokio::sync::RwLock::new(())),
            me: OnceLock::new(),
            jobs: Mutex::new(HashMap::new()),
            timing: Mutex::new(Timing {
                poll_override: None,
                turn_poll: Duration::from_millis(250),
            }),
        });
        let _ = inner.me.set(Arc::downgrade(&inner));
        // A play of a song is one of the events a follow of it reads Lidarr's progress on.
        if let Some(ids) = &inner.ids {
            let weak = Arc::downgrade(&inner);
            ids.on_heard(Arc::new(move |id: &str, _: &str, _: &HeardRecord| {
                if let Some(inner) = weak.upgrade() {
                    inner.song_event(SoulseekMetadataService::PROVIDER_NAME, id);
                }
            }));
        }
        // Lidarr answering any call of Octo's is news of its indexers for a heart waiting on them.
        {
            let weak = Arc::downgrade(&inner);
            inner.client.on_answer(Arc::new(move |path: &str| {
                if let Some(inner) = weak.upgrade() {
                    inner.lidarr_answered(path);
                }
            }));
        }
        LidarrHeartAcquisitionService { inner }
    }

    /// Who runs again every heart that waits for Lidarr's indexers once they search again, and
    /// who says whether any heart still waits for them (their health is read only while one does).
    pub fn on_indexers_back(&self, back: IndexersBack, awaited: Option<IndexersAwaited>) {
        *self.inner.indexers.back.lock() = Some((back, awaited));
    }

    /// Who files the tracks an album heart's import held as held album tracks of the person's
    /// heart, whenever they come, the album's chain running or not.
    pub fn on_album_holds(&self, holds: AlbumHolds) {
        *self.inner.album_holds.lock() = Some(holds);
    }

    /// How often Lidarr is asked what it imported, instead of the timeout's thirtieth.
    pub fn set_poll_override(&self, poll: Option<Duration>) {
        self.inner.timing.lock().poll_override = poll;
    }

    /// What the user's Lidarr holds of an artist, read as this service reads it.
    pub fn artist_music(&self) -> LidarrArtistMusic {
        self.inner.music.clone()
    }

    /// In the background: puts back every pick a restart cut short before its search, and
    /// follows on every search a restart cut short. Does nothing outside a runtime.
    pub fn resume_after_restart(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // Taken now, before any fresh pick can start, and let go once the put back is done.
        let barrier = self.inner.restart.clone().try_write_owned().ok();
        let inner = self.inner.clone();
        runtime.spawn(async move { inner.resume_all(barrier).await });
    }

    /// The same, waited for, for tests.
    #[cfg(test)]
    pub(crate) async fn put_back_after_restart(&self) {
        self.inner.resume_all(None).await;
    }

    /// Stops every follow this service runs, as a restart would, for tests.
    #[cfg(test)]
    pub(crate) fn stop_following(&self) {
        self.inner.stopping.cancel();
    }

    /// The people a running job answers to, for tests that join one.
    #[cfg(test)]
    pub(crate) fn askers_of(&self, foreign_album_id: &str) -> Option<Vec<String>> {
        let job = self
            .inner
            .jobs
            .lock()
            .get(&dotnet::ordinal_ignore_case_key(foreign_album_id))
            .cloned()?;
        job.asked_by()
    }
}

#[async_trait]
impl ILidarrHeartAcquisitionService for LidarrHeartAcquisitionService {
    async fn lidarr_imported(&self, event: LidarrImportEvent) -> LidarrImportOutcome {
        self.inner.clone().lidarr_imported(event)
    }

    async fn try_acquire_track(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        let work =
            self.inner
                .acquire_track(provider, external_id, notify_failure, requested_by, origin, gates);
        Ok(self
            .inner
            .try_acquire(work, "track", provider, external_id, notify_failure)
            .await)
    }

    fn left_alone(&self, provider: &str, external_id: &str) -> bool {
        self.inner
            .left_alone
            .lock()
            .contains(&left_alone_key(provider, external_id))
    }

    fn waits_on(&self, provider: &str, external_id: &str) -> Option<LidarrWaits> {
        self.inner
            .busy
            .lock()
            .remove(&left_alone_key(provider, external_id))
    }

    fn held_file(&self, provider: &str, external_id: &str) -> Option<AcquisitionHeld> {
        self.inner
            .held_files
            .lock()
            .remove(&left_alone_key(provider, external_id))
    }

    fn lidarr_health_event(&self) {
        self.inner.indexers.nudge();
    }

    fn song_event(&self, provider: &str, external_id: &str) {
        self.inner.song_event(provider, external_id);
    }

    fn unhearted(&self, provider: &str, external_id: &str, user: &str) {
        let ending = self
            .inner
            .pending
            .unhearted(&format!("{provider}:{external_id}"), user);
        for follow in ending {
            info!("Everyone took their heart on {follow} back; Octo stops following Lidarr there");
            self.inner.ensure_following(&follow);
        }
    }

    fn follows_heart(&self, provider: &str, external_id: &str) -> bool {
        self.inner
            .pending
            .follows_heart(&format!("{provider}:{external_id}"))
    }

    async fn stop_waiting_for_lidarr(&self, provider: &str, external_id: &str, user: &str) -> StopWaiting {
        self.inner.stop_waiting(provider, external_id, user).await
    }

    fn waits_for_lidarr(&self, provider: &str, external_id: &str) -> bool {
        self.inner
            .pending
            .kept_copy_waits(&format!("{provider}:{external_id}"))
    }

    fn stop_waiters(&self, provider: &str, external_id: &str) -> Vec<String> {
        let mut waiters: Vec<String> = Vec::new();
        for held in self
            .inner
            .pending
            .kept_copies_waiting(&format!("{provider}:{external_id}"))
        {
            for user in held.users.iter().map(|user| user.trim()) {
                if !user.is_empty() && !waiters.iter().any(|known| dotnet::eq_ignore_case(known, user)) {
                    waiters.push(user.to_string());
                }
            }
        }
        waiters
    }

    fn unsettled_follows(&self) -> Vec<UnsettledFollow> {
        self.inner.pending.unsettled()
    }

    fn put_lidarr_back(&self, follow: &str) -> bool {
        if !self.inner.pending.let_go_unseen(follow) {
            return false;
        }
        info!("The admin put Lidarr back for {follow}: the downloads Octo cannot see count as gone");
        self.inner.ensure_following(follow);
        true
    }

    async fn try_acquire_album(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        let work =
            self.inner
                .acquire_album(provider, external_id, notify_failure, requested_by, origin, gates);
        Ok(self
            .inner
            .try_acquire(work, "album", provider, external_id, notify_failure)
            .await)
    }
}

impl Inner {
    /// A song through Lidarr: the albums the walk picks for what the person heard, each in turn
    /// until the song lands (at most four), or, with no identity to walk from, the album found by
    /// the song's names. A pick Lidarr does not know, or whose artist already has music in the
    /// user's Lidarr, is passed over for the next; a pick whose download still moves when the
    /// import timeout passes ends the step, and so does one whose file of the song waits in the
    /// one hold path. With Lidarr last in the chain, a walk MusicBrainz or Deezer did not answer
    /// for leaves what it waits on for the heart to hold on; so do Lidarr's failing indexers when
    /// every pick that searched grabbed nothing.
    async fn acquire_track(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        let song = self
            .metadata
            .get_song(provider, external_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("The starred external track is no longer available."))?;
        let song_key = song_entry_key(&song).unwrap_or_else(|| format!("{provider}:{external_id}"));
        let song_gates: Vec<SongGate> = gates
            .iter()
            .map(|gate| SongGate {
                song: song_key.clone(),
                title: song.title.clone(),
                artist: song.artist.clone(),
                gate: gate.clone(),
                stopped: false,
            })
            .collect();

        let mut ends: Vec<PickEnd> = Vec::new();
        let mut last: Option<(Arc<AlbumJob>, Song)> = None;
        let mut busy: Option<LidarrWaits> = None;
        match self.walk(external_id, origin, gates, requested_by).await {
            WalkStart::Picks(mut walk) => {
                while let Some(pick) = walk.next().await {
                    let Some(candidate) = self.client.resolve_album_by_foreign_id(&pick).await? else {
                        info!("Lidarr does not know release group {pick}; the walk goes on");
                        continue;
                    };
                    let mut song = song.clone();
                    song.album = candidate.title.clone();
                    let album = Album {
                        title: candidate.title.clone(),
                        artist: candidate.artist.clone(),
                        year: candidate.year,
                        songs: vec![song.clone()],
                        ..Default::default()
                    };
                    let ask = Ask {
                        hint: Some(walk.hint(&pick)),
                        ..Default::default()
                    };
                    let job = self
                        .queue_resolved_album(
                            &album,
                            requested_by,
                            Some(candidate),
                            false,
                            false,
                            ask,
                            origin,
                            &song_gates,
                        )
                        .await?;
                    let end = self.pick_end(&job, &song, provider, external_id, gates).await;
                    if end == PickEnd::Landed {
                        self.set_left_alone(provider, external_id, false);
                        return Ok(true);
                    }
                    self.note_skip(provider, external_id, &end);
                    let stop = matches!(end, PickEnd::TimedOut | PickEnd::Held);
                    ends.push(end);
                    last = Some((job, song));
                    if stop {
                        break;
                    }
                }
                busy = walk.busy.take().map(LidarrWaits::Answer);
            }
            WalkStart::Busy(waits) => {
                info!("MusicBrainz did not answer for {provider}:{external_id}; Lidarr is not asked");
                busy = Some(LidarrWaits::Answer(waits));
            }
            WalkStart::NoIdentity => {
                let (album, song) = self.album_by_name(song).await?;
                let job = self
                    .queue_resolved_album(
                        &album,
                        requested_by,
                        None,
                        false,
                        false,
                        Ask::default(),
                        origin,
                        &song_gates,
                    )
                    .await?;
                let end = self.pick_end(&job, &song, provider, external_id, gates).await;
                if end == PickEnd::Landed {
                    self.set_left_alone(provider, external_id, false);
                    return Ok(true);
                }
                self.note_skip(provider, external_id, &end);
                ends.push(end);
                last = Some((job, song));
            }
        }
        // The song's file waits in the one hold path: the heart holds with it, whatever comes after
        // Lidarr in its chain.
        if ends.contains(&PickEnd::Held)
            && let Some((job, song)) = &last
            && let Some(held) = job.held.lock().get(&key_of(song)).cloned()
        {
            info!("Lidarr's file of {provider}:{external_id} waits: {}", held.line);
            self.set_left_alone(provider, external_id, false);
            self.held_files
                .lock()
                .insert(left_alone_key(provider, external_id), held);
            return Ok(false);
        }
        let last_waits = notify_failure && waits_for_answers(origin) && !ends.contains(&PickEnd::TimedOut);
        // Every pick that searched grabbed nothing, with Lidarr last: its indexers may be why.
        if busy.is_none()
            && last_waits
            && ends.contains(&PickEnd::NoGrab)
            && ends
                .iter()
                .all(|end| matches!(end, PickEnd::NoGrab | PickEnd::Skipped(_)))
        {
            match self.indexers_after_misses().await {
                IndexerHealth::Failing => busy = Some(LidarrWaits::Indexers),
                IndexerHealth::NoneCanSearch => {
                    info!("Lidarr has no indexer that can search for {provider}:{external_id}");
                    self.note_skip(
                        provider,
                        external_id,
                        &PickEnd::Skipped(Some(NO_INDEXER_SEARCHES)),
                    );
                    self.set_left_alone(provider, external_id, true);
                    if let Some(tracker) = &self.tracker {
                        tracker.fail(provider, external_id, Some(NO_INDEXER_SEARCHES));
                    }
                    return Ok(false);
                }
                IndexerHealth::Healthy => {}
            }
        }
        if let Some(waits) = busy
            && last_waits
        {
            match &waits {
                LidarrWaits::Indexers => info!(
                    "Lidarr is the last source for {provider}:{external_id} and its indexers are failing; the heart waits for them"
                ),
                LidarrWaits::Answer(_) => info!(
                    "Lidarr is the last source for {provider}:{external_id} and its walk got no answer; the heart waits for one"
                ),
            }
            if waits == LidarrWaits::Indexers {
                self.indexers_failing();
            }
            self.set_left_alone(provider, external_id, false);
            self.busy
                .lock()
                .insert(left_alone_key(provider, external_id), waits);
            return Ok(false);
        }
        let left_alone = !ends.is_empty()
            && ends
                .iter()
                .all(|end| *end == PickEnd::Skipped(Some(LIDARR_LEFT_ALONE)));
        self.set_left_alone(provider, external_id, left_alone);
        if notify_failure && let Some(tracker) = &self.tracker {
            let reason = match ends.last() {
                Some(PickEnd::Skipped(Some(line))) => Some(line.to_string()),
                _ => last.and_then(|(job, song)| job.missed_for(&key_of(&song))),
            }
            .unwrap_or_else(|| "Lidarr did not bring this song.".to_string());
            tracker.fail(provider, external_id, Some(&reason));
        }
        Ok(false)
    }

    /// The album the song's names find: the album Deezer files it under, else its own.
    async fn album_by_name(&self, mut song: Song) -> anyhow::Result<(Album, Song)> {
        let enriched = self
            .deezer
            .enrich_track(&song.artist, &song.title, true, false)
            .await;
        let mut album_title = enriched.as_ref().and_then(|e| e.album_title.clone());
        if dotnet::is_null_or_white_space(album_title.as_deref()) {
            album_title = Some(song.album.clone());
        }
        let album_title = album_title.unwrap_or_default();
        if dotnet::is_blank(&album_title) {
            anyhow::bail!(
                "Could not resolve an album for '{} - {}'.",
                song.artist,
                song.title
            );
        }
        song.album = album_title.clone();
        if song.cover_art_url.is_none() {
            song.cover_art_url = enriched.as_ref().and_then(|e| e.album_cover_url.clone());
        }
        if song.year.is_none() {
            song.year = enriched.as_ref().and_then(|e| e.year);
        }
        let album = Album {
            title: album_title,
            artist: enriched
                .as_ref()
                .and_then(|e| e.artist_name.clone())
                .unwrap_or_else(|| song.artist.clone()),
            year: enriched.as_ref().and_then(|e| e.year),
            cover_art_url: enriched.as_ref().and_then(|e| e.album_cover_url.clone()),
            songs: vec![song.clone()],
            ..Default::default()
        };
        Ok((album, song))
    }

    /// How one pick's job ended for the song: it landed, the follow ended without it, Lidarr was
    /// not used, or the import timeout passed with the follow still open. The heart's heard-master
    /// rounds wait paused meanwhile.
    async fn pick_end(
        &self,
        job: &AlbumJob,
        song: &Song,
        provider: &str,
        external_id: &str,
        gates: &[HeartGate],
    ) -> PickEnd {
        if let Some(pause) = &self.rounds {
            pause(&acquisition_key(provider, external_id, false), gates);
        }
        let user = gates.first().map(|gate| gate.user.as_str());
        match job
            .wait_for(std::slice::from_ref(song), self.import_deadline(), user)
            .await
        {
            Waited::Landed => PickEnd::Landed,
            Waited::Held => PickEnd::Held,
            Waited::Ended => match *job.skipped.lock() {
                Some(line) => PickEnd::Skipped(line),
                None if *job.no_grab.lock() => PickEnd::NoGrab,
                None => PickEnd::Missed,
            },
            Waited::TimedOut => {
                self.timed_out(job, std::slice::from_ref(song));
                PickEnd::TimedOut
            }
        }
    }

    /// When a heart waiting from now stops waiting: Lidarr's import timeout.
    fn import_deadline(&self) -> Instant {
        let seconds = self.settings.current().lidarr.import_timeout_seconds.max(1);
        Instant::now() + Duration::from_secs(seconds as u64)
    }

    /// The import timeout passed for these songs while Lidarr's download still moves: each is
    /// missed with the timeout's line, and the follow goes on.
    fn timed_out(&self, job: &AlbumJob, songs: &[Song]) {
        let settings = self.settings.current().lidarr.clone();
        let detail = format!(
            "Lidarr import timed out after {} minute(s)",
            settings.import_timeout_seconds.max(1) / 60
        );
        {
            let mut missed = job.missed.lock();
            for song in songs {
                missed.entry(key_of(song)).or_insert_with(|| format!("{detail}."));
            }
        }
        warn!(
            "{detail} for '{} - {}'; Octo keeps following Lidarr's download",
            job.album.artist, job.album.title
        );
        if settings.completion_mode == LidarrCompletionMode::Imported {
            self.notifications.notify(NotificationEvent {
                artist: Some(job.album.artist.clone()),
                title: Some(job.album.title.clone()),
                album: Some(job.album.title.clone()),
                source: Some("Lidarr".into()),
                cover_art_url: job.album.cover_art_url.clone(),
                detail: Some(detail),
                ..NotificationEvent::new(NotificationEventType::DownloadFailed)
            });
        }
    }

    /// The downloads entry says why Lidarr was not used for a pick.
    fn note_skip(&self, provider: &str, external_id: &str, end: &PickEnd) {
        if let (PickEnd::Skipped(Some(line)), Some(tracker)) = (end, &self.tracker) {
            tracker.stage(
                provider,
                external_id,
                AcquisitionState::Searching,
                Some("Lidarr"),
                Some(line),
            );
        }
    }

    fn set_left_alone(&self, provider: &str, external_id: &str, left_alone: bool) {
        let key = left_alone_key(provider, external_id);
        let mut songs = self.left_alone.lock();
        if left_alone {
            songs.insert(key);
        } else {
            songs.remove(&key);
        }
    }

    /// The walk for the song as the person heard it: its routing and the album context the
    /// person listed it from (the one whose recordings are the gate's R, when the gate knows R),
    /// then the identity those give. When the gate's R is none of the identity's recordings,
    /// what played became another recording, and the walk follows it. No routing, no MusicBrainz,
    /// or no recording to walk from: the album is found by name. A family request MusicBrainz does
    /// not answer for goes on by name too; any other origin leaves what did not answer.
    async fn walk(
        &self,
        external_id: &str,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
        requested_by: Option<&str>,
    ) -> WalkStart {
        let (Some(music_brainz), Some(ids)) = (self.music_brainz.clone(), self.ids.clone()) else {
            return WalkStart::NoIdentity;
        };
        let personal = matches!(
            origin,
            AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Play
        );
        let Some(routing) = ids
            .resolve_song(external_id, personal)
            .map(|shared| shared.snapshot())
        else {
            return WalkStart::NoIdentity;
        };
        let busy = |waits: HeartBusy| {
            if origin == AcquisitionOrigin::Request {
                WalkStart::NoIdentity
            } else {
                WalkStart::Busy(waits)
            }
        };
        let song = heart_song(&routing);
        let questions = HeartQuestions::new(music_brainz, self.catalog.clone(), lane_for(origin));
        let r: Option<HeardRecording> = gates
            .iter()
            .find_map(|gate| gate.r.clone())
            .filter(|r| !r.ids.is_empty());
        let user = gates
            .first()
            .map(|gate| gate.user.clone())
            .or_else(|| requested_by.map(str::to_string))
            .unwrap_or_default();
        let candidates = pick_contexts(&ids.contexts(external_id), &user);
        let mut chosen: Option<(HeartSong, HeardIdentity)> = None;
        if candidates.is_empty() {
            match questions.identity(&song, None).await {
                Ok(identity) => chosen = Some((song.clone(), identity)),
                Err(waits) => return busy(waits),
            }
        }
        for context in &candidates {
            let listed = song_in(&song, context);
            let identity = match questions.identity(&listed, Some(&context.catalog_album_id)).await {
                Ok(identity) => identity,
                Err(waits) => return busy(waits),
            };
            let fits = r
                .as_ref()
                .is_some_and(|r| identity.recordings.iter().any(|id| r.holds(id)));
            if chosen.is_none() || fits {
                chosen = Some((listed, identity));
            }
            if fits {
                break;
            }
        }
        let Some((song, mut identity)) = chosen else {
            return WalkStart::NoIdentity;
        };
        if let Some(r) = &r
            && !identity.recordings.iter().any(|id| r.holds(id))
        {
            identity.recordings = r.ids.clone();
            identity.recordings_uncapped = r.ids.clone();
            identity.heard_track = None;
            identity.listened_release = None;
            identity.listened_group = None;
        }
        if identity.recordings.is_empty() {
            return WalkStart::NoIdentity;
        }
        let picks: VecDeque<String> = identity.listened_group.iter().cloned().collect();
        WalkStart::Picks(Walk {
            questions,
            song,
            identity,
            picks,
            walked: false,
            seen: HashSet::new(),
            busy: None,
        })
    }

    async fn acquire_album(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        let album = self
            .metadata
            .get_album(provider, external_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("The starred external album is no longer available."))?;
        // No walk runs on this path, so the track list is announced here instead.
        if let Some(tracker) = &self.tracker {
            let listed: Vec<_> = album
                .songs
                .iter()
                .filter_map(|s| {
                    s.external_id.as_deref().filter(|id| !id.is_empty()).map(|id| {
                        (
                            id.to_string(),
                            Some(s.artist.clone()),
                            Some(s.title.clone()),
                            Some(album.title.clone()),
                        )
                    })
                })
                .collect();
            tracker.announce(provider, Some(external_id), None, &listed);
        }
        // The album the heart names, by its catalog album's barcode; by name only with neither a
        // catalog id nor a catalog album its title finds.
        let (resolved, hint) = match self.album_find(external_id, &album, origin).await {
            AlbumFind::Group { group, release } => {
                match self.client.resolve_album_by_foreign_id(&group).await? {
                    Some(candidate) => {
                        info!(
                            "Album heart {provider}:{external_id} is release group {group}, pinned to its barcode release {release}"
                        );
                        let hint = PinHint {
                            listened_release: Some(release),
                            recordings: Vec::new(),
                            lane: Some(lane_for(origin)),
                        };
                        (Some(candidate), Some(hint))
                    }
                    None => {
                        info!(
                            "Lidarr does not know release group {group} of album heart {provider}:{external_id}"
                        );
                        self.album_missed(&album, notify_failure, "Lidarr did not bring this song.");
                        return Ok(false);
                    }
                }
            }
            AlbumFind::ByName => (None, None),
            // With Lidarr last, the album heart holds for the answer, as a song heart's walk does:
            // nothing is asked of Lidarr and no row fails. With a source after it, the step ends.
            AlbumFind::Busy(busy) if notify_failure && waits_for_answers(origin) => {
                info!(
                    "Lidarr is the last source for album heart {provider}:{external_id} and {busy:?} did not answer for its album; the heart waits for an answer"
                );
                self.busy
                    .lock()
                    .insert(left_alone_key(provider, external_id), LidarrWaits::Answer(busy));
                return Ok(false);
            }
            AlbumFind::Busy(busy) => {
                warn!(
                    "{busy:?} did not answer for album heart {provider}:{external_id}; Lidarr is not asked"
                );
                self.album_missed(&album, notify_failure, "Lidarr did not bring this song.");
                return Ok(false);
            }
        };
        // Each track of an album heart has its own gate, the track's own recording, from the
        // album's gate; without one, the gates asked with are on every song.
        let user = requested_by.map(str::trim).filter(|user| !user.is_empty());
        let album_gate = match (user, &self.album_gates) {
            (Some(user), Some(store)) if self.gates.is_some() => store.gate(provider, external_id, user),
            _ => None,
        };
        let song_gates: Vec<SongGate> = match &album_gate {
            Some(gate) => album_song_gates(provider, &album, gate),
            None => album
                .songs
                .iter()
                .filter_map(|song| song_entry_key(song).map(|key| (key, song)))
                .flat_map(|(key, song)| {
                    gates.iter().map(move |gate| SongGate {
                        song: key.clone(),
                        title: song.title.clone(),
                        artist: song.artist.clone(),
                        gate: gate.clone(),
                        stopped: false,
                    })
                })
                .collect(),
        };
        let ask = Ask {
            hint,
            album_heart: album_gate.is_some().then(|| format!("{provider}:{external_id}")),
            ..Default::default()
        };
        let job = self
            .queue_resolved_album(
                &album,
                requested_by,
                resolved,
                true,
                true,
                ask,
                origin,
                &song_gates,
            )
            .await?;
        // The job's own hold on the album's gate outlives the album's chain while its follow runs.
        if let (Some(gate), Some(store)) = (album_gate, &self.album_gates) {
            job.gate_holds
                .lock()
                .push(store.register(provider, external_id, gate, Vec::new()));
        }
        if job.wait_for(&album.songs, self.import_deadline(), None).await == Waited::TimedOut {
            let missing: Vec<Song> = album
                .songs
                .iter()
                .filter(|song| !job.has_landed(song))
                .cloned()
                .collect();
            self.timed_out(&job, &missing);
        }
        let missing: Vec<&Song> = album.songs.iter().filter(|song| !job.has_landed(song)).collect();
        if notify_failure && let Some(tracker) = &self.tracker {
            for song in &missing {
                let (Some(p), Some(id)) = (song.external_provider.as_deref(), song.external_id.as_deref())
                else {
                    continue;
                };
                if p.is_empty() || id.is_empty() {
                    continue;
                }
                let reason = job
                    .missed_for(&key_of(song))
                    .unwrap_or_else(|| "Lidarr finished the album without this song.".to_string());
                tracker.fail(p, id, Some(&reason));
            }
        }
        Ok(missing.is_empty())
    }

    /// `match_by_number`: whether an imported file may be matched to a song by its track
    /// number. Not for a track heart: its one song carries the number from whatever release it
    /// was found on, often the single, where it is track 1, which on the album is another song.
    /// An album whose search is followed already, by a job here or by a follow a restart left,
    /// is joined: nothing is searched again, and the joiner's people and gates are merged into
    /// its saved entry.
    #[allow(clippy::too_many_arguments)] // the album, who asked, why, and what they heard
    async fn queue_resolved_album(
        self: &Arc<Self>,
        album: &Album,
        requested_by: Option<&str>,
        resolved: Option<LidarrAlbumCandidate>,
        match_by_number: bool,
        album_heart: bool,
        ask: Ask,
        origin: AcquisitionOrigin,
        gates: &[SongGate],
    ) -> anyhow::Result<Arc<AlbumJob>> {
        if dotnet::is_blank(&album.artist) || dotnet::is_blank(&album.title) {
            anyhow::bail!("Lidarr requires an album artist and title.");
        }

        let candidate = match resolved {
            Some(candidate) => candidate,
            None => {
                self.client
                    .resolve_album(&album.artist, &album.title, album.year)
                    .await?
            }
        };
        let key = dotnet::ordinal_ignore_case_key(&candidate.foreign_album_id);
        let existing = self.jobs.lock().get(&key).cloned();
        let existing = match existing {
            Some(job) => Some(job),
            None => self
                .pending
                .get(&candidate.foreign_album_id)
                .filter(PendingAlbum::searched)
                .map(|entry| self.resume_job(&entry)),
        };
        let (job, created) = match existing {
            Some(job) => (job, false),
            None => {
                let mut jobs = self.jobs.lock();
                match jobs.get(&key) {
                    Some(job) => (job.clone(), false),
                    None => {
                        let job = Arc::new(AlbumJob::new(candidate.clone(), album.clone()));
                        jobs.insert(key.clone(), job.clone());
                        (job, true)
                    }
                }
            }
        };
        job.join(
            &album.songs,
            match_by_number,
            album_heart,
            requested_by,
            origin,
            gates,
        );
        if let Some(hint) = ask.hint {
            job.hint.lock().get_or_insert(hint);
        }
        {
            let mut songs = job.songs.lock();
            // One album, one search: only the job's first asker picks the release it grabs.
            if created && let Some(release) = ask.release {
                songs.release = Some(release);
            }
            if let Some(heart) = ask.album_heart
                && !songs.album_hearts.contains(&heart)
            {
                songs.album_hearts.push(heart);
            }
        }
        if created {
            // Spawned: the submission runs whether or not anyone is still waiting.
            let handle = tokio::spawn(self.clone().submit(job.clone()));
            let submitted: BoxFuture<'static, SharedResult> = async move {
                match handle.await {
                    Ok(result) => result.map_err(Arc::new),
                    Err(e) => Err(Arc::new(anyhow::anyhow!("{e}"))),
                }
            }
            .boxed();
            let _ = job.submitted.set(submitted.shared());
        } else {
            let saved = job.saved();
            self.pending
                .update(&candidate.foreign_album_id, |entry| entry.merge(saved));
            if let Some(tracker) = &self.tracker {
                // Joined a search already running: its songs are followed from here on.
                for (provider, id) in tracked_keys(album) {
                    tracker.transfer(&provider, &id, None, None, None, Some("Lidarr"));
                }
            }
            // A track the album holds already may be the joiner's song.
            if job.gated() {
                tokio::spawn(
                    self.clone()
                        .settle(job.clone(), candidate.foreign_album_id.clone(), false),
                );
            }
        }
        let submitted = job
            .submitted
            .get()
            .cloned()
            .expect("set when the job was created");
        if let Err(e) = submitted.await {
            let mut jobs = self.jobs.lock();
            if jobs.get(&key).is_some_and(|current| Arc::ptr_eq(current, &job)) {
                jobs.remove(&key);
            }
            return Err(anyhow::anyhow!("{e}"));
        }
        Ok(job)
    }

    /// The job of a follow that runs here, else a job over the saved entry whose follow starts
    /// again now.
    fn resume_job(self: &Arc<Self>, entry: &PendingAlbum) -> Arc<AlbumJob> {
        let key = dotnet::ordinal_ignore_case_key(&entry.foreign_album_id);
        let job = {
            let mut jobs = self.jobs.lock();
            if let Some(job) = jobs.get(&key) {
                return job.clone();
            }
            let candidate = LidarrAlbumCandidate {
                id: entry.lidarr_album_id,
                foreign_album_id: entry.foreign_album_id.clone(),
                title: entry.title.clone(),
                artist: entry.artist.clone(),
                year: entry.year,
                resource: Default::default(),
            };
            let album = Album {
                title: entry.title.clone(),
                artist: entry.artist.clone(),
                year: entry.year,
                ..Default::default()
            };
            let job = Arc::new(AlbumJob::resumed(candidate, album, entry));
            jobs.insert(key, job.clone());
            job
        };
        self.hold_album_gates(&job, &entry.album_hearts, &entry.gates);
        if let Some(claims) = &self.claims
            && !job.replacement_only()
        {
            claims.heart_started(&entry.foreign_album_id);
        }
        tokio::spawn(self.clone().follow(job.clone(), entry.lidarr_album_id));
        job
    }

    /// Starts the follow of a searched entry again when none runs, so it acts on what was asked
    /// of it.
    fn ensure_following(self: &Arc<Self>, foreign_album_id: &str) {
        if self.pending.is_running(foreign_album_id) {
            return;
        }
        if let Some(entry) = self.pending.get(foreign_album_id).filter(PendingAlbum::searched) {
            self.resume_job(&entry);
        }
    }

    /// A put back Lidarr failed: it is tried again when Lidarr next answers a call of Octo's, and
    /// at the poll pace, until it is done.
    fn owe_put_back(&self, foreign_album_id: &str) {
        self.put_backs.owed.lock().insert(foreign_album_id.to_string());
        if self
            .put_backs
            .running
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let me = self.me.get().and_then(std::sync::Weak::upgrade);
        match (me, tokio::runtime::Handle::try_current()) {
            (Some(me), Ok(runtime)) => {
                runtime.spawn(me.put_backs_again());
            }
            _ => self
                .put_backs
                .running
                .store(false, std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// Tries every owed put back again, on Lidarr's next answer or at the poll pace, until none
    /// is owed. A pick that never searched is put back here; a searched one through its follow,
    /// whose end puts it back.
    async fn put_backs_again(self: Arc<Self>) {
        loop {
            let poll = self.poll(&self.settings.current().lidarr);
            tokio::select! {
                () = tokio::time::sleep(poll) => {}
                () = self.put_backs.answered.notified() => {}
                () = self.stopping.cancelled() => {
                    self.put_backs.running.store(false, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
            let owed: Vec<String> = self.put_backs.owed.lock().drain().collect();
            let settings = self.settings.current().lidarr.clone();
            for key in owed {
                let Some(entry) = self.pending.get(&key) else {
                    continue;
                };
                if follow::lidarr_moved(&entry, &settings) == follow::LidarrMoved::Swapped {
                    info!(
                        "Octo let go of Lidarr's album {key}: the Lidarr it was asked of is no longer Octo's"
                    );
                    self.pending.forget(&key);
                    continue;
                }
                if !entry.searched() {
                    match put_back_unsearched(&self.client, Some(self.pending.as_ref()), &entry, poll).await {
                        Ok(()) => info!("Put back Lidarr's album {key} on a later try"),
                        Err(e) => {
                            warn!("Could not put back Lidarr's album {key} yet: {e}");
                            self.put_backs.owed.lock().insert(key);
                        }
                    }
                } else if self.pending.is_running(&key) {
                    // Its follow is still winding up; looked at again next time.
                    self.put_backs.owed.lock().insert(key);
                } else {
                    self.ensure_following(&key);
                }
            }
            // Lidarr answering the tries just made is no news.
            let _ = self.put_backs.answered.notified().now_or_never();
            if self.put_backs.owed.lock().is_empty() {
                self.put_backs
                    .running
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                // One owed while this stopped starts it again.
                if self.put_backs.owed.lock().is_empty()
                    || self
                        .put_backs
                        .running
                        .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    return;
                }
            }
        }
    }

    /// After a restart: every pick cut short before its search is put back (a fresh pick waits
    /// for that behind `barrier`; one Lidarr fails is tried again later), a hold folder no entry
    /// names goes, a held file that waits for no follow is placed, each copy held for an open
    /// follow has its row back, and every search is followed on, the tracks its album holds
    /// settled again.
    async fn resume_all(self: &Arc<Self>, barrier: Option<tokio::sync::OwnedRwLockWriteGuard<()>>) {
        let poll = self.poll(&self.settings.current().lidarr);
        let failed = put_back_unsearched_after_restart(&self.client, &self.pending, poll).await;
        drop(barrier);
        for key in failed {
            self.owe_put_back(&key);
        }
        self.sweep_hold_folders();
        for held in self.pending.release_unfollowed() {
            self.place_held(held, FollowEnd::Done).await;
        }
        if let Some(tracker) = &self.tracker {
            for held in self.pending.held() {
                let open = held
                    .follows
                    .iter()
                    .any(|follow| self.pending.get(follow).is_some_and(|entry| entry.following()));
                if open && let Some((provider, id)) = held.song.split_once(':') {
                    tracker.wait_aside(provider, id, &held.users, HELD_FOR_LIDARR);
                }
            }
        }
        for entry in self.pending.entries() {
            if entry.searched() && !self.pending.is_running(&entry.foreign_album_id) {
                let job = self.resume_job(&entry);
                if !entry.held_tracks.is_empty() {
                    tokio::spawn(self.clone().settle(job, entry.foreign_album_id.clone(), false));
                }
            }
        }
    }

    async fn submit(self: Arc<Self>, job: Arc<AlbumJob>) -> anyhow::Result<()> {
        // A restart's put back of what it cut short ends first.
        drop(self.restart.read().await);
        let snapshot = self.settings.current().lidarr.clone();
        let key = job.candidate.foreign_album_id.clone();
        // An upgrade borrowing this album takes the files its search brings in; the heart waits
        // until it lets go rather than lose its songs to it. A replacement's own pick is that
        // upgrade's, and claims nothing of a heart's.
        if let Some(claims) = &self.claims
            && !job.replacement_only()
        {
            if claims.upgrade_busy(&key) {
                info!("An upgrade borrows Lidarr's album {key}; the heart waits until it lets go");
            }
            claims.upgrades_let_go(&key).await;
            claims.heart_started(&key);
        }
        let album_id = match self.prepare(&job, &snapshot).await {
            Ok(Prepared::Searched { album_id }) => album_id,
            Ok(Prepared::Skipped(line)) => {
                self.end_skipped(&job, line);
                if let Some(claims) = &self.claims {
                    claims.heart_ended(&key);
                }
                return Ok(());
            }
            Err(e) => {
                if let Some(claims) = &self.claims {
                    claims.heart_ended(&key);
                }
                return Err(e.into());
            }
        };

        let album = &job.album;
        info!(
            "Lidarr accepted AlbumSearch for '{} - {}' ({key}, local id {album_id})",
            album.artist, album.title
        );
        self.notifications.notify(NotificationEvent {
            artist: Some(album.artist.clone()),
            title: Some(album.title.clone()),
            album: Some(album.title.clone()),
            source: Some("Lidarr".into()),
            cover_art_url: album.cover_art_url.clone(),
            detail: Some("Album search accepted".into()),
            ..NotificationEvent::new(NotificationEventType::DownloadStarted)
        });
        // Accepted. Lidarr says nothing about bytes, so this is a download with no figure on it.
        if let Some(tracker) = &self.tracker {
            for (provider, id) in tracked_keys(&job.view()) {
                tracker.transfer(&provider, &id, None, None, None, Some("Lidarr"));
            }
        }
        tokio::spawn(self.clone().follow(job.clone(), album_id));
        Ok(())
    }

    /// The poll pace of Lidarr's imports and commands: the timeout's thirtieth, 1 to 10 s.
    fn poll(&self, settings: &LidarrSettings) -> Duration {
        self.timing.lock().poll_override.unwrap_or_else(|| {
            Duration::from_secs((settings.import_timeout_seconds / 30).clamp(1, 10) as u64)
        })
    }

    /// The pace while a download of Octo's waits for its import: under a second.
    fn turn_pace(&self, settings: &LidarrSettings) -> Duration {
        let turn = self.timing.lock().turn_poll;
        self.poll(settings).min(turn)
    }

    /// A job whose album Lidarr was not used for ends at once with nothing landed, each song
    /// missed with the line that says why.
    fn end_skipped(&self, job: &Arc<AlbumJob>, line: Option<&'static str>) {
        *job.skipped.lock() = Some(line);
        {
            let mut missed = job.missed.lock();
            for song in &job.view().songs {
                missed.insert(
                    key_of(song),
                    line.unwrap_or("Lidarr did not bring this song.").to_string(),
                );
            }
        }
        let job_key = dotnet::ordinal_ignore_case_key(&job.candidate.foreign_album_id);
        {
            let mut jobs = self.jobs.lock();
            if jobs
                .get(&job_key)
                .is_some_and(|current| Arc::ptr_eq(current, job))
            {
                jobs.remove(&job_key);
            }
        }
        job.end();
    }

    /// Everything a pick does up to its search, each read on work and never on a timer:
    /// - "When Lidarr is used": the album's Lidarr artist with music in the user's Lidarr (a track
    ///   file, or audio in its folder) leaves Lidarr alone, with nothing sent;
    /// - a monitored album is the user's, and Lidarr already looks for it; a queue item of the
    ///   user's on the artist is a grab whose files Octo's import could replace;
    /// - the entry is saved before anything is sent, with the add about to go out, the people who
    ///   asked and each one's gate;
    /// - an album Lidarr lacks is added unmonitored (and its artist, monitoring nothing);
    /// - its track list, then the add's refresh and rescan, are waited for, and the artist is read
    ///   again: anything the rescan linked means the artist has music;
    /// - the album is pinned to a release that carries what was heard, the pin's rescan waited
    ///   for, and the artist read once more;
    /// - only then the search, whose command id is saved at once.
    ///
    /// A pick skipped after anything was sent is put back.
    async fn prepare(&self, job: &Arc<AlbumJob>, settings: &LidarrSettings) -> Result<Prepared, LidarrError> {
        let candidate = &job.candidate;
        let key = candidate.foreign_album_id.clone();
        let Some(foreign_artist_id) = artist_of(candidate) else {
            warn!("Lidarr's lookup of {key} names no artist; Lidarr is not asked for it");
            return Ok(Prepared::Skipped(None));
        };
        let music = self.music.read(&foreign_artist_id).await?;
        if music.has_music {
            info!(
                "Lidarr's artist of {key} already has music; Lidarr is left alone for '{} - {}'",
                job.album.artist, job.album.title
            );
            return Ok(Prepared::Skipped(Some(LIDARR_LEFT_ALONE)));
        }
        let existing = self.client.album_by_foreign_id(&key).await?;
        if existing.as_ref().is_some_and(|album| album.monitored) {
            return Ok(Prepared::Skipped(Some(ALREADY_LOOKING)));
        }
        if let Some(artist) = &music.artist
            && users_queue_item(&self.client, Some(self.pending.as_ref()), artist.id).await?
        {
            info!("Lidarr is fetching something of the artist of {key} for the user; Octo stays out");
            return Ok(Prepared::Skipped(None));
        }

        let mut entry = PendingAlbum {
            lidarr_album_id: existing.as_ref().map_or(0, |album| album.id),
            lidarr_artist_id: music.artist.as_ref().map_or(0, |artist| artist.id),
            artist: job.album.artist.clone(),
            title: job.album.title.clone(),
            year: job.album.year,
            asked_at: Utc::now(),
            various_artists: dotnet::eq_ignore_case(&foreign_artist_id, VARIOUS_ARTISTS),
            foreign_artist_id: foreign_artist_id.clone(),
            lidarr_url: lidarr_identity(settings),
            ..job.saved()
        };
        if existing.is_none() {
            let artists_before = self.client.artists().await?.iter().map(|a| a.id).collect();
            entry.add = Some(PendingAdd {
                foreign_artist_id: foreign_artist_id.clone(),
                posted_at: self.client.now().await?,
                artists_before,
            });
        }
        self.save(&entry);

        let prepared = self
            .prepare_saved(job, settings, &mut entry, existing, &music)
            .await;
        let skipped = match &prepared {
            Ok(Prepared::Searched { .. }) => return prepared,
            Ok(Prepared::Skipped(_)) => true,
            Err(_) => false,
        };
        let saved = self.pending.get(&key).unwrap_or(entry);
        // The search or the grab may have reached Lidarr with no answer back in time: what it
        // started is followed, never put back as unsearched under it. A refusal Lidarr answered
        // started nothing.
        let unanswered = matches!(prepared, Err(LidarrError::Canceled(_)));
        if unanswered && saved.search_pending.is_some() && saved.lidarr_album_id > 0 {
            warn!("Lidarr did not answer Octo's search of {key}; Octo follows whatever it started");
            return Ok(Prepared::Searched {
                album_id: saved.lidarr_album_id,
            });
        }
        // Skipped, or Lidarr failed partway: put back what was sent. A put-back that fails is
        // tried again when Lidarr next answers and at the poll pace.
        let poll = self.poll(settings);
        match put_back_unsearched(&self.client, Some(self.pending.as_ref()), &saved, poll).await {
            Ok(()) if skipped => info!("Put back Lidarr's album {key}: nothing was searched"),
            Ok(()) => {}
            Err(e) => {
                warn!("Could not put back Lidarr's album {key}: {e}");
                self.owe_put_back(&key);
            }
        }
        prepared
    }

    async fn prepare_saved(
        &self,
        job: &Arc<AlbumJob>,
        settings: &LidarrSettings,
        entry: &mut PendingAlbum,
        existing: Option<LidarrAlbum>,
        music: &super::ArtistMusic,
    ) -> Result<Prepared, LidarrError> {
        let poll = self.poll(settings);
        let (album_id, artist_id) = match &existing {
            Some(album) => (album.id, album.artist_id),
            None => {
                let added = self
                    .client
                    .add_album(&job.candidate, music.artist.as_ref())
                    .await?;
                entry.lidarr_album_id = added.album_id;
                entry.lidarr_artist_id = added.artist_id;
                entry.album_added = true;
                if music.artist.is_none() {
                    entry.artist_added = added.artist_id;
                }
                self.save(entry);
                (added.album_id, added.artist_id)
            }
        };
        if !self.client.wait_for_tracks(album_id, artist_id).await? {
            warn!("Lidarr lists no tracks for album {album_id}; it is not searched");
            return Ok(Prepared::Skipped(None));
        }
        let artist_path = self.client.artist(artist_id).await?.and_then(|a| a.path);
        wait_for_rescans(&self.client, album_id, artist_id, artist_path.as_deref(), poll).await?;
        if entry.artist_added > 0 {
            entry.albums_after_add = self
                .client
                .artist_albums(artist_id)
                .await?
                .iter()
                .map(|album| album.id)
                .collect();
            self.save(entry);
        }
        let album = match self.read_again(album_id, artist_id).await? {
            Ok(album) => album,
            Err(line) => return Ok(Prepared::Skipped(line)),
        };

        if let Some(pin) = self.pin_for(job, &album).await {
            let before = album.release_choice();
            entry.release_choice = Some(before.clone());
            entry.pin = Some(pin.clone());
            self.save(entry);
            // Pinned only while the choice is still the one just read: a change the user made
            // meanwhile is never written over, and the pick steps out.
            let pinned = self
                .client
                .set_release_choice_if(album_id, |now| now.release_choice() == before, &pin)
                .await?;
            if pinned.is_none() {
                info!("Lidarr's album {album_id} changed while Octo pinned it; it is not searched");
                self.pending
                    .update(&entry.foreign_album_id, |saved| saved.pin = None);
                return Ok(Prepared::Skipped(None));
            }
            wait_for_rescans(&self.client, album_id, artist_id, artist_path.as_deref(), poll).await?;
            if let Err(line) = self.read_again(album_id, artist_id).await? {
                return Ok(Prepared::Skipped(line));
            }
        }

        // Lidarr's time is saved before the search or the grab goes out, so a restart before its
        // answer is saved still follows what it started. A release picked in Find songs is grabbed
        // as it is: its grab is Octo's by its guid, saved before each grab is sent.
        let pending_at = self.client.now().await?;
        entry.search_pending = Some(pending_at);
        let key = entry.foreign_album_id.clone();
        let release = job.songs.lock().release.clone();
        match release {
            Some(pick) => {
                entry.picked_guid = Some(pick.guid.clone());
                self.save(entry);
                let guid = self
                    .client
                    .grab_picked_saving(album_id, &pick, |guid| {
                        self.pending
                            .update(&key, |saved| saved.picked_guid = Some(guid.to_string()));
                    })
                    .await?;
                entry.picked_guid = Some(guid.clone());
                entry.searched_at = Some(pending_at);
                self.pending.update(&key, |saved| {
                    saved.picked_guid = Some(guid);
                    saved.searched_at = Some(pending_at);
                });
            }
            None => {
                self.save(entry);
                let command = self.client.album_search(album_id).await?;
                entry.search_command = command.id;
                entry.searched_at = Some(command.queued.unwrap_or(pending_at));
                let searched_at = entry.searched_at;
                self.pending.update(&key, |saved| {
                    saved.search_command = command.id;
                    saved.searched_at = searched_at;
                });
            }
        }
        Ok(Prepared::Searched { album_id })
    }

    /// The artist and the album read again right before the pin and the search: any track
    /// file of the artist (one a rescan linked included) leaves Lidarr alone; a monitored album is
    /// the user's; a queue item of the user's keeps Octo out. The album as it stands otherwise.
    async fn read_again(
        &self,
        album_id: i32,
        artist_id: i32,
    ) -> Result<Result<LidarrAlbum, Option<&'static str>>, LidarrError> {
        if !self.client.artist_track_files(artist_id).await?.is_empty() {
            return Ok(Err(Some(LIDARR_LEFT_ALONE)));
        }
        let Some(album) = self.client.album(album_id).await? else {
            return Ok(Err(None));
        };
        if album.monitored {
            return Ok(Err(Some(ALREADY_LOOKING)));
        }
        if users_queue_item(&self.client, Some(self.pending.as_ref()), artist_id).await? {
            return Ok(Err(None));
        }
        Ok(Ok(album))
    }

    /// The release choice that pins the album to one release carrying what was heard: on the
    /// listened-from group the release the person listened from, when Lidarr lists it; otherwise
    /// the first carrier of a heard recording in this group that Lidarr lists. None when Lidarr
    /// lists none (the album is searched unpinned), and when MusicBrainz does not answer.
    async fn pin_for(
        &self,
        job: &AlbumJob,
        album: &LidarrAlbum,
    ) -> Option<octo_core::lidarr::lidarr_client::LidarrReleaseChoice> {
        let hint = job.hint.lock().clone()?;
        if let Some(listened) = &hint.listened_release
            && let Some(release) = album.release(listened)
        {
            return album.pinned_to(release.id);
        }
        let music_brainz = self.music_brainz.clone()?;
        let questions = HeartQuestions::new(
            music_brainz,
            self.catalog.clone(),
            hint.lane.unwrap_or(Lane::Heart),
        );
        for recording in &hint.recordings {
            let carriers = questions.carriers(recording).await.ok()?;
            for carrier in carriers
                .releases
                .iter()
                .filter(|carrier| dotnet::eq_ignore_case(&carrier.group.id, &album.foreign_album_id))
            {
                if let Some(release) = album.release(&carrier.id) {
                    return album.pinned_to(release.id);
                }
            }
        }
        None
    }

    /// Saves a pick's entry, merged into what is listed for the album.
    fn save(&self, entry: &PendingAlbum) {
        self.pending.remember(entry.clone());
    }

    /// One file of Octo's own that Lidarr imported for a job: dropped when the library must not
    /// take it, else handed to the pipeline, whose work is returned. `hearted` false never hands
    /// it as a song a heart's or a play's gate is on (its candidates were judged by that gate
    /// already): it then goes as one of the album's tracks under an id of its own.
    async fn take_import(
        self: &Arc<Self>,
        job: &Arc<AlbumJob>,
        track: &LidarrImportedTrack,
        imported_path: String,
        labels: LabelSource,
        hearted: bool,
    ) -> Option<JoinHandle<()>> {
        let album = job.view();
        let song = match_song(&album, track, job.match_by_number())
            .filter(|song| {
                hearted || {
                    let key = song_entry_key(song);
                    !job.gates
                        .lock()
                        .iter()
                        .any(|gate| gate.personal() && Some(&gate.song) == key.as_ref())
                }
            })
            .cloned();
        self.take_import_as(job, track, imported_path, labels, song, None)
            .await
    }

    /// [`Self::take_import`] as `song`, a hearted album's track its own sound already proved it
    /// to be when `filed` carries that verdict.
    async fn take_import_as(
        self: &Arc<Self>,
        job: &Arc<AlbumJob>,
        track: &LidarrImportedTrack,
        imported_path: String,
        labels: LabelSource,
        song: Option<Song>,
        filed: album::Filed,
    ) -> Option<JoinHandle<()>> {
        let album = job.view();
        if let Some(drop) = self
            .should_drop(&album, track, song.as_ref(), &imported_path, job)
            .await
        {
            self.drop_import(
                &job.candidate.foreign_album_id,
                track,
                &imported_path,
                &drop.reason,
            )
            .await;
            // Already in the library: in, as far as the heart is concerned, and its row
            // closes on the library's own copy.
            if let Some(owned) = drop.owned {
                job.add_landed(song.as_ref(), filed.is_none().then_some(track), &album);
                if let (Some(song), Some(tracker)) = (&song, &self.tracker)
                    && let (Some(provider), Some(id)) =
                        (song.external_provider.as_deref(), song.external_id.as_deref())
                    && !provider.is_empty()
                    && !id.is_empty()
                {
                    tracker.imported(
                        provider,
                        id,
                        Some(&song.artist),
                        Some(&song.title),
                        Some(&owned.absolute_path),
                    );
                }
            } else if let Some(song) = &song {
                job.missed
                    .lock()
                    .insert(key_of(song), format!("Not taken: {}.", drop.reason));
            }
            return None;
        }
        Some(tokio::spawn(self.clone().hand_to_pipeline(
            job.clone(),
            track.clone(),
            song,
            album,
            imported_path,
            labels,
            filed,
        )))
    }

    /// Lidarr's webhook named an album it imported files of. Whose import it was is decided first,
    /// by the download it came from, before anything Lidarr says of it (an upgrade included): an
    /// import of one of Octo's own downloads, delivered by a download client, has every follow
    /// watching that download read Lidarr now, and one a restart left waiting starts again once
    /// no upgrade borrows its album. Any other import (a folder import, even with a download id
    /// Lidarr guessed, has no client) is not Octo's: a running follow on its album or artist only
    /// reads Lidarr at once, so a file of the user's is seen before Octo's own import.
    fn lidarr_imported(self: Arc<Self>, event: LidarrImportEvent) -> LidarrImportOutcome {
        let key = event.foreign_album_id.trim().to_string();
        let artist = event.artist.as_deref().map(str::trim).unwrap_or("");
        let download = event
            .download_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .filter(|_| {
                event
                    .download_client
                    .as_deref()
                    .is_some_and(|client| !dotnet::is_blank(client))
            });
        let follows: Vec<PendingAlbum> = match download {
            Some(download) => self
                .pending
                .entries()
                .into_iter()
                .filter(PendingAlbum::searched)
                .filter(|entry| entry.owns_grab(download))
                .collect(),
            None => Vec::new(),
        };
        if follows.is_empty() {
            for entry in self.pending.entries().into_iter().filter(PendingAlbum::following) {
                let on_it = (!key.is_empty() && dotnet::eq_ignore_case(&entry.foreign_album_id, &key))
                    || (!artist.is_empty() && dotnet::eq_ignore_case(entry.artist.trim(), artist));
                if on_it {
                    self.pending.wake(&entry.foreign_album_id);
                }
            }
            return LidarrImportOutcome::NotOurs;
        }
        let mut late = false;
        for entry in follows {
            let album = entry.foreign_album_id.clone();
            if self.pending.is_running(&album) {
                self.pending.wake(&album);
                continue;
            }
            late = true;
            // An upgrade borrowing the album deletes what its own search brings in; the follow
            // starts again once it lets go.
            if self
                .claims
                .as_ref()
                .is_some_and(|claims| claims.upgrade_busy(&album))
            {
                tokio::spawn(self.clone().after_claims(album));
            } else {
                self.resume_job(&entry);
            }
        }
        if late {
            LidarrImportOutcome::Late
        } else {
            LidarrImportOutcome::Woke
        }
    }

    /// Starts the follow of an album again once no upgrade borrows it: the upgrade letting go
    /// ends the wait, never a clock.
    async fn after_claims(self: Arc<Self>, foreign_album_id: String) {
        if let Some(claims) = &self.claims {
            claims.upgrades_let_go(&foreign_album_id).await;
        }
        self.ensure_following(&foreign_album_id);
    }

    /// One imported file through the download pipeline, as the song it is: a hearted song by its
    /// own id, so its progress row moves on, and any other track of the album by an id minted for
    /// it. Whatever the pipeline did not take is deleted through Lidarr, so nothing is left behind
    /// in the library unchecked. `filed`: what the file's own sound proved, so it lands as that.
    #[allow(clippy::too_many_arguments)] // the file, the song it is, and what proved it
    async fn hand_to_pipeline(
        self: Arc<Self>,
        job: Arc<AlbumJob>,
        track: LidarrImportedTrack,
        song: Option<Song>,
        album: Album,
        imported_path: String,
        labels: LabelSource,
        filed: album::Filed,
    ) {
        let provider = SoulseekMetadataService::PROVIDER_NAME;
        // A file its own sound filed is only the song it proved; Lidarr's match names nothing.
        let by_sound = filed.is_some();
        let mut id = song.as_ref().and_then(|s| {
            let p = s.external_provider.as_deref()?;
            let e = s.external_id.as_deref().filter(|e| !e.is_empty())?;
            dotnet::eq_ignore_case(p, provider).then(|| e.to_string())
        });
        if id.is_none()
            && let Some(ids) = &self.ids
        {
            id = Some(ids.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some(track.artist.clone().unwrap_or_else(|| album.artist.clone())),
                title: if dotnet::is_blank(&track.title) {
                    song.as_ref().map(|s| s.title.clone())
                } else {
                    Some(track.title.clone())
                },
                album: Some(album.title.clone()),
                duration: track.duration_seconds,
                track: track.track_number,
                ..Default::default()
            }));
        }
        let (Some(id), Some(downloads)) = (id, self.downloads.clone()) else {
            warn!(
                "Lidarr brought '{} - {}', but there is no download pipeline to take it",
                track.artist.as_deref().unwrap_or(""),
                track.title
            );
            return;
        };

        // A song nobody hearted lands quietly; an album heart gets one notice for the album.
        self.imports.offer_import(
            &id,
            LidarrImport {
                path: imported_path.clone(),
                quiet: song.is_none() || job.album_heart(),
                labels,
                refused_to: None,
                filed,
            },
        );
        // The song's own gates go with it (an import's, a request's), so the pipeline's check
        // applies their rule and judges each joiner by their own; any other track keeps the
        // shipped checks.
        let gates: Vec<HeartGate> = match song.as_ref().and_then(song_entry_key) {
            Some(key) => job
                .gates
                .lock()
                .iter()
                .filter(|gate| gate.song == key)
                .map(|gate| gate.gate.clone())
                .collect(),
            None => Vec::new(),
        };
        let result = match gates.first().map(|gate| gate.origin) {
            Some(origin) => {
                downloads
                    .execute_acquisition_to(
                        provider,
                        &id,
                        false,
                        true,
                        Some(DownloadSource::Lidarr),
                        &CancellationToken::new(),
                        job.asked_by(),
                        false,
                        None,
                        crate::services::common::AcquisitionPlace {
                            origin: Some(origin),
                            gates,
                            ..Default::default()
                        },
                    )
                    .await
            }
            None => {
                downloads
                    .execute_acquisition(
                        provider,
                        &id,
                        false,
                        true,
                        Some(DownloadSource::Lidarr),
                        &CancellationToken::new(),
                        job.asked_by(),
                        false,
                        None,
                    )
                    .await
            }
        };
        match result {
            Ok(_) => {
                if self.imports.withdraw(&id) {
                    // The pipeline found the song in the library and never took this file.
                    self.drop_import(
                        &job.candidate.foreign_album_id,
                        &track,
                        &imported_path,
                        "Octo already had this song",
                    )
                    .await;
                } else {
                    // The pipeline moved the file into Octo's own layout: Lidarr's record of it
                    // goes too, so the artist reads as having no files and Lidarr stays usable.
                    delete_moved_record(
                        &self.client,
                        Some(self.pending.as_ref()),
                        &job.candidate.foreign_album_id,
                        track.track_file_id,
                        &imported_path,
                    )
                    .await;
                }
                job.add_landed(song.as_ref(), (!by_sound).then_some(&track), &album);
            }
            Err(e) => {
                self.imports.withdraw(&id);
                info!(
                    "Lidarr's '{} - {}' was not taken: {e}",
                    track.artist.as_deref().unwrap_or(""),
                    track.title
                );
                if let Some(song) = &song {
                    job.missed.lock().insert(key_of(song), e.to_string());
                }
                if Path::new(&imported_path).is_file() {
                    self.drop_import(
                        &job.candidate.foreign_album_id,
                        &track,
                        &imported_path,
                        &format!("Octo did not take it ({e})"),
                    )
                    .await;
                }
            }
        }
    }

    /// Why an imported file should not join the library, or None when it should. A song removed
    /// with a library action stays removed, and a song already in the library is not added twice,
    /// the same rules as a Soulseek download. An owned lossy copy is queued for Better quality
    /// instead, which swaps it in place and keeps its plays.
    async fn should_drop(
        &self,
        album: &Album,
        track: &LidarrImportedTrack,
        song: Option<&Song>,
        imported_path: &str,
        job: &AlbumJob,
    ) -> Option<DropReason> {
        let artist = match song {
            Some(song) => song.artist.clone(),
            None => track.artist.clone().unwrap_or_else(|| album.artist.clone()),
        };
        let title = if dotnet::is_blank(&track.title) {
            song.map(|s| s.title.clone())
        } else {
            Some(track.title.clone())
        };
        let title = title.filter(|t| !dotnet::is_blank(t))?;
        if dotnet::is_blank(&artist) {
            return None;
        }

        if let Some(journal) = &self.journal
            && journal.is_never_requested(Some(&artist), Some(&title))
        {
            return Some(DropReason {
                reason: "it was removed with a library action".into(),
                owned: None,
            });
        }

        if !self.settings.current().subsonic.skip_owned_songs {
            return None;
        }
        let ownership = self.ownership.as_ref()?;
        let owned = ownership
            .find(
                Some(&artist),
                Some(&title),
                track.duration_seconds.or_else(|| song.and_then(|s| s.duration)),
                Some(&album.title),
            )
            .await?;
        // The import itself, once Navidrome has scanned it, is not a second copy.
        if same_path(&owned.absolute_path, imported_path) {
            return None;
        }
        let suffix = dotnet::to_upper_invariant(&owned.suffix);
        if !owned.lossless
            && is_lossless(track)
            && self.queue_upgrade(&owned, &artist, &title, &album.title, job)
        {
            return Some(DropReason {
                reason: format!("you have it as {suffix}, which is queued for a higher quality copy"),
                owned: Some(owned),
            });
        }
        Some(DropReason {
            reason: format!("it is already in your library ({suffix})"),
            owned: Some(owned),
        })
    }

    /// Queue Better quality for an owned lossy copy, when every gate of the action is open
    /// for the person who hearted the album.
    fn queue_upgrade(
        &self,
        owned: &OwnedCopy,
        artist: &str,
        title: &str,
        album: &str,
        job: &AlbumJob,
    ) -> bool {
        let Some(navidrome_id) = &owned.navidrome_id else {
            return false;
        };
        let Some(askers) = job.asked_by().filter(|a| !a.is_empty()) else {
            return false;
        };
        let Some(queue) = &self.upgrades else {
            return false;
        };
        if !LibraryOwnership::upgrade_allowed(&self.settings.current().library_actions, Some(&askers[0])) {
            return false;
        }
        queue.add(
            vec![UpgradeAsk {
                navidrome_id: navidrome_id.clone(),
                title: Some(title.to_string()),
                artist: Some(artist.to_string()),
                album: Some(album.to_string()),
                suffix: Some(owned.suffix.clone()),
                attempt_key: None,
                pick: None,
            }],
            &askers[0],
            "heart",
        );
        true
    }

    /// Takes an import of Octo's own back out. Octo deletes the file from disk itself, so
    /// Lidarr's recycle bin never keeps it, then deletes Lidarr's record of it, so Lidarr's own
    /// records stay true; a file Octo cannot reach is deleted through Lidarr. Only a file the
    /// follow judged Octo's, by its own import row, ever comes here.
    async fn drop_import(&self, album: &str, track: &LidarrImportedTrack, imported_path: &str, reason: &str) {
        let artist = track.artist.as_deref().unwrap_or("");
        let on_disk = Path::new(imported_path).is_file();
        if on_disk && let Err(e) = std::fs::remove_file(imported_path) {
            warn!("Could not delete {imported_path} ({reason}): {e}");
        }
        if !Path::new(imported_path).exists() {
            delete_moved_record(
                &self.client,
                Some(self.pending.as_ref()),
                album,
                track.track_file_id,
                imported_path,
            )
            .await;
        } else if track.track_file_id > 0
            && let Err(e) = self.client.delete_track_file(track.track_file_id).await
        {
            warn!(
                "Could not delete Lidarr's '{artist} - {}' ({reason}): {e}",
                track.title
            );
        }
        info!(
            "Lidarr brought '{artist} - {}', but {reason}; deleted it ({imported_path})",
            track.title
        );
    }

    /// `user` stopped waiting for Lidarr on the song: their kept copy is placed now, and they leave
    /// the waits of every follow it waited for. A follow anyone else's heart still waits on goes on;
    /// one nobody's is on any more ends, its follow (running or started again for it) removing
    /// Octo's downloads.
    async fn stop_waiting(self: &Arc<Self>, provider: &str, external_id: &str, user: &str) -> StopWaiting {
        let song = format!("{provider}:{external_id}");
        let (copies, ending) = self.pending.stop_waiting(&song, user);
        if copies.is_empty() {
            return StopWaiting::NothingWaits;
        }
        for follow in copies.iter().flat_map(|held| held.follows.iter()) {
            let job = self
                .jobs
                .lock()
                .get(&dotnet::ordinal_ignore_case_key(follow))
                .cloned();
            if let Some(job) = job {
                job.stopped_waiting(&song, user);
            }
        }
        for held in copies {
            info!("{user} stopped waiting for Lidarr: the kept copy of {song} is placed");
            self.place_held(held, FollowEnd::Stopped).await;
        }
        for follow in ending {
            info!("Nobody waits for Lidarr on {follow} any more; Octo stops following it");
            self.ensure_following(&follow);
        }
        StopWaiting::Stopped
    }

    async fn try_acquire(
        &self,
        work: impl Future<Output = anyhow::Result<bool>>,
        kind: &str,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
    ) -> bool {
        match work.await {
            Ok(result) => result,
            Err(e) => {
                error!("Lidarr {kind} heart failed for {external_id}: {e:#}");
                // notifyFailure is true only for the last source in the chain, which is also the
                // only failure the progress list may show.
                if notify_failure {
                    let message = e.to_string();
                    if let Some(tracker) = &self.tracker {
                        if kind == "album" {
                            tracker.fail_album(provider, external_id, Some(&message));
                        } else {
                            tracker.fail(provider, external_id, Some(&message));
                        }
                    }
                    self.notifications.notify(NotificationEvent {
                        source: Some("Lidarr".into()),
                        detail: Some(message),
                        ..NotificationEvent::new(NotificationEventType::DownloadFailed)
                    });
                }
                false
            }
        }
    }
}

/// The Lidarr Octo is set to use, by its address, or None when none is set.
fn lidarr_identity(settings: &LidarrSettings) -> Option<String> {
    let url = settings.base_url.as_deref().map(str::trim).unwrap_or_default();
    if url.is_empty() || dotnet::is_null_or_white_space(settings.api_key.as_deref()) {
        return None;
    }
    Some(dotnet::to_lower_invariant(url.trim_end_matches('/')))
}

/// `SamePath`: the two full paths are one (ordinal, as on Linux).
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    get_full_path(a) == get_full_path(b)
}

/// Where Lidarr's imported file is in Octo's library: its path under Lidarr's root folder, taken
/// onto Octo's root. Throws for a path outside that root, and for one that would land outside
/// Octo's.
pub fn translate_imported_path(
    lidarr_path: &str,
    lidarr_root: Option<&str>,
    octo_root: &str,
) -> Result<String, LidarrError> {
    let Some(lidarr_root) = lidarr_root.filter(|r| !dotnet::is_blank(r)) else {
        return Err(LidarrError::InvalidOperation(
            "Lidarr root folder is not configured.".into(),
        ));
    };
    let root = get_full_path(lidarr_root);
    let source = get_full_path(lidarr_path);
    let relative = get_relative_path(&root, &source);
    if relative == ".." || relative.starts_with("../") {
        return Err(LidarrError::InvalidOperation(format!(
            "Lidarr imported a path outside its configured root: {lidarr_path}"
        )));
    }
    let target_root = get_full_path(octo_root);
    let target = get_full_path(&path_combine(&target_root, &relative));
    if get_relative_path(&target_root, &target).starts_with("..") {
        return Err(LidarrError::InvalidOperation(
            "Translated Lidarr path escaped Octo's library root.".into(),
        ));
    }
    Ok(target)
}

/// Joins `b` onto `a` with one `/`; a `b` that starts with `/` replaces `a`.
fn path_combine(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `path` relative to `relative_to`, for two full paths on Unix (case-sensitive): "."
/// for the same path, the rest of `path` under `relative_to`, otherwise a climb out with `..`.
fn get_relative_path(relative_to: &str, path: &str) -> String {
    let from: Vec<&str> = relative_to.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    if common == from.len() && common == to.len() {
        return ".".to_string();
    }
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    let mut relative = parts.join("/");
    // A trailing separator on the path survives.
    if path.ends_with('/') && !relative.is_empty() {
        relative.push('/');
    }
    relative
}

#[path = "lidarr_follow.rs"]
mod follow;

#[path = "lidarr_hold_folder.rs"]
mod hold;

#[path = "lidarr_health.rs"]
mod health;

#[path = "lidarr_album_filing.rs"]
mod album;

#[path = "lidarr_replacement.rs"]
mod replacement;

use album::{AlbumFind, album_song_gates};
pub use health::IndexersAwaited;

#[cfg(test)]
#[path = "lidarr_heart_protection_tests.rs"]
mod tests;
