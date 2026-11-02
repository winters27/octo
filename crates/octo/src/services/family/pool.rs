//! The family pool: one real file for every song a member downloaded, under
//! `<members>/.pool/<layout>`, and a relative link to it in the library of each member who has
//! it (`<members>/<user>/<layout>`). No Navidrome library points at the pool, so nobody browses
//! it; each member sees the song through their own library only.
//!
//! `family-pool.json` ([`FamilyPoolStore`]) is the record of who links what. When the last
//! member lets a file go it goes to Octo's trash (the library actions trash under the members
//! folder, kept for the same number of days). A better copy of a song replaces the old one for
//! everyone who had it, and a song the shared library wants moves there, leaving no links
//! behind. Stars and family playlist entries follow a song whose id changes with its file.
//!
//! Where links do not work (an rclone mount without `--vfs-links`, some network disks), each
//! member gets a real copy instead and [`FamilyPool::link_status`] says so in plain words.
//! What Octo learns about links (Navidrome showed one, or showed a copy where it never showed
//! the link) is kept in `family-pool.json`, so a restart does not forget it.
//!
//! Every change to the pool (a link, an unlink, a better copy, a move into the shared library,
//! a member let go, a sweep) claims the pool files it works on for its whole run, so two
//! changes to one file never overlap, while changes to different files go on side by side. The
//! pool's lock is held only while a change reads and writes the record and decides what to do,
//! never across a copy, a move or a call to Navidrome (its guard cannot cross an `.await` in a
//! change that builds), and every file operation runs on a blocking thread. Each change runs
//! as a task of its own, so it goes to its end even when whoever asked for it stops waiting:
//! its claims are let go of only once its last file operation is over, and a panic lets go of
//! them too.
//!
//! Octo writes a marker file ([`POOL_MARKER`]) in the pool folder when Family is turned on. A
//! pool folder without it is taken for an empty mount point where the members disk should be:
//! no record is forgotten and no member download is placed there until it is back.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::common::path_helper::{PATH_BUDGET, POOL_BUDGET};
use octo_core::common::{
    AlbumHook, Clock, Layout, LayoutKind, LayoutValues, PathHelper, RenderedName, SettleHooks, SongIdentity,
};
use octo_core::models::domain::Song;
use octo_core::settings::{FolderStructure, LibraryAction, SettingsStore};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::family_playlist_store::FamilyPlaylistStore;
use super::family_pool_store::{FamilyPoolStore, MISSED_KEPT, PoolFile};
use super::family_service::valid_username;
use super::family_store::{FamilyStore, Member, SharedLibraries};
use super::library_index::{IndexedSong, LibraryIndex};
use super::member_credentials::MemberCredentials;
use crate::services::common::acquisition_tracker::key_of;
use crate::services::common::album_identity::{self, AlbumIdentity, LayoutTags, Shown};
use crate::services::common::base_download_service::{
    Filed, PLACEMENT, choose_loose, file_values, mapping_at, values_from_tags,
};
use crate::services::library::library_write_gate::{LibraryWrite, LibraryWriteGate as MusicFolderGate};
use crate::services::library::tidy::layout_pass::{LayoutMapping, kept_values};
use crate::services::library::{LibraryActionQuarantine, PathSource, ResolvedSongFile};
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME;

/// The pool's folder under the members folder. The dot keeps it out of every member's
/// library and out of Navidrome's scans.
pub const POOL_FOLDER: &str = ".pool";

/// Where a removed member's files are kept aside, under the members folder. The dot keeps it
/// out of every library.
pub const KEPT_FOLDER: &str = ".kept";

/// The file Octo writes in the pool folder when Family is turned on. A pool folder without it
/// is not swept and takes no new member downloads.
pub const POOL_MARKER: &str = ".octo-pool";

/// What the marker file says, for anyone who finds it.
const POOL_MARKER_TEXT: &str =
    "Octo keeps the family's downloads in this folder. Leave this file here; without it Octo takes the folder for a disk that is not mounted.
";

/// What a member reads when their download cannot be placed because the pool folder lacks its
/// marker file.
pub const POOL_NOT_READY: &str = "The family library's disk is not ready. Try again later.";

/// Sweeps for pool files deleted by hand run at most this often, and a file goes only when it
/// has been missing at least this long, both measured by a [`SteadyClock`].
pub const SWEEP_SPACING: Duration = Duration::from_secs(10 * 60);

/// Time as it really passes for the pool, from when the pool was made. It only goes forward,
/// whatever the wall clock is set to, so a clock set by hand or by a time server never makes
/// sweeps run closer together or a file look missing for longer than it was.
#[derive(Clone)]
pub struct SteadyClock(Arc<dyn Fn() -> Duration + Send + Sync>);

impl SteadyClock {
    /// The system's steady clock.
    pub fn system() -> Self {
        let start = std::time::Instant::now();
        SteadyClock(Arc::new(move || start.elapsed()))
    }

    /// A clock that reads the given function, for tests that move time along. It must never
    /// go back.
    pub fn new(elapsed: impl Fn() -> Duration + Send + Sync + 'static) -> Self {
        SteadyClock(Arc::new(elapsed))
    }

    /// How long since the clock started.
    pub fn elapsed(&self) -> Duration {
        (self.0)()
    }
}

/// A pool file a sweep found missing: when, by the steady clock and by the wall clock.
#[derive(Debug, Clone, Copy)]
struct Miss {
    since: Duration,
    at: DateTime<Utc>,
}

/// What becomes of the files only a removed member had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesChoice {
    /// Kept aside for the owner, under `<members>/.kept/<user>/`.
    Keep,
    /// Moved into the shared library.
    Promote,
    /// Sent to Octo's trash.
    Delete,
}

impl FilesChoice {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "keep" => Some(FilesChoice::Keep),
            "promote" => Some(FilesChoice::Promote),
            "delete" => Some(FilesChoice::Delete),
            _ => None,
        }
    }
}

/// What became of a removed member's pool files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Released {
    pub kept: usize,
    pub promoted: usize,
    pub trashed: usize,
    /// Files another member still has, left where they are.
    pub shared: usize,
}

/// What the Family page says when the members folder cannot link files.
pub const COPY_MODE_MESSAGE: &str =
    "This disk cannot link files, so a song two members want takes space twice.";

/// How to make links work again, for the same page.
pub const COPY_MODE_FIX: &str = "Start the rclone mount of the members folder with --vfs-links, or keep the members folder on a local disk.";

/// Cover files a member's album folder gets beside its songs.
const COVER_FILES: [&str; 6] = [
    "cover.jpg",
    "cover.png",
    "folder.jpg",
    "folder.png",
    "front.jpg",
    "front.png",
];

/// How a member's library gets a pool song.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkMode {
    /// A relative link to the one pool file.
    Symlink,
    /// A real copy of the file (the disk cannot link).
    Copy,
}

/// The pool's link mode as the Family page shows it (`links`, `message`, `fix`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkStatus {
    pub links: LinkMode,
    /// What the owner reads when links do not work; None when they do.
    pub message: Option<String>,
    /// How to make them work; None when they do.
    pub fix: Option<String>,
}

impl LinkStatus {
    pub fn of(mode: LinkMode) -> Self {
        match mode {
            LinkMode::Symlink => LinkStatus {
                links: mode,
                message: None,
                fix: None,
            },
            LinkMode::Copy => LinkStatus {
                links: mode,
                message: Some(COPY_MODE_MESSAGE.to_string()),
                fix: Some(COPY_MODE_FIX.to_string()),
            },
        }
    }
}

/// Why a pool change could not be made.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("{0} is not a family member")]
    NotMember(String),
    #[error("{0} is not a file in the family pool")]
    NotInPool(String),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// What became of a pool file [`FamilyPool::relayout_held`] was asked to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayoutOutcome {
    /// The file, its lyrics files, its record and every member's link are at the new place.
    Moved,
    /// Nothing moved, for the reason these words give (the tidy's row shows them).
    Held(String),
}

/// What the claim two changes copying one shared file into the pool take turns on starts with,
/// before the shared file's path. No pool path starts with it.
const COPY_OF: &str = "copy of:";

/// What a change answers when it could not run because Octo is stopping.
fn stopping() -> PoolError {
    PoolError::Io(io::Error::other("Octo is stopping"))
}

/// Why the pool folder's marker file was not written.
#[derive(Debug, thiserror::Error)]
pub enum NotMarked {
    /// Octo recorded pool files and none of them is in the pool folder: the members disk may
    /// not be mounted, and the folder may be its empty mount point.
    #[error(
        "none of the {recorded} family downloads Octo recorded is in the pool folder, so the members disk may not be mounted"
    )]
    DiskMissing { recorded: usize },
    /// Writing it failed.
    #[error("{0}")]
    Io(io::Error),
}

impl PoolError {
    /// What a member reads about it: plain words, never a path inside the server.
    pub fn for_member(&self) -> &'static str {
        match self {
            PoolError::NotMember(_) => "Only family members have their own library.",
            PoolError::NotInPool(_) => "That copy is no longer in the family library.",
            PoolError::Io(_) => "Octo could not put the song in your library. Try again later.",
        }
    }
}

/// How many songs had a link a finished scan did not show, and a copy in its place that
/// Navidrome then showed, with no link ever shown, before Octo probes the members folder again
/// to decide whether every member gets copies.
pub const UNSHOWN_LINKS_FOR_COPIES: u32 = 2;

/// How patiently a moved or new file is waited for in the library index.
#[derive(Debug, Clone, Copy)]
pub struct PoolTiming {
    /// Ask again this often (and read the index again when it is older than this).
    pub poll: Duration,
    /// Give up after this long.
    pub max_wait: Duration,
}

impl Default for PoolTiming {
    fn default() -> Self {
        PoolTiming {
            poll: Duration::from_secs(15),
            max_wait: Duration::from_secs(10 * 60),
        }
    }
}

/// Stars as a member sees them, for a song whose id changes when its file moves.
#[async_trait]
pub trait MemberStars: Send + Sync {
    /// Whether `user` has starred library song `song_id`.
    async fn is_starred(&self, user: &str, song_id: &str) -> bool;
    /// Stars library song `song_id` as `user`. True when Navidrome said ok.
    async fn star(&self, user: &str, song_id: &str) -> bool;
}

/// [`MemberStars`] through Navidrome, signed in as the member with a sign-in made at the
/// moment of use from their current hidden password.
pub struct NavidromeMemberStars {
    settings: Arc<SettingsStore>,
    http: reqwest::Client,
    credentials: Arc<MemberCredentials>,
}

impl NavidromeMemberStars {
    pub fn new(
        settings: Arc<SettingsStore>,
        http: reqwest::Client,
        credentials: Arc<MemberCredentials>,
    ) -> Self {
        NavidromeMemberStars {
            settings,
            http,
            credentials,
        }
    }

    async fn call(&self, user: &str, endpoint: &str, song_id: &str) -> Option<serde_json::Value> {
        let base = self.settings.current().subsonic.url.clone().unwrap_or_default();
        if base.trim().is_empty() {
            return None;
        }
        let credential = self.credentials.fresh(user)?;
        let parameters = credential.parameters(&[("f", "json"), ("id", song_id)]);
        let url = format!("{}/rest/{endpoint}", base.trim_end_matches('/'));
        let response = self.http.get(&url).query(&parameters).send().await.ok()?;
        let body: serde_json::Value = response.json().await.ok()?;
        let envelope = body.get("subsonic-response")?.clone();
        (envelope.get("status").and_then(|s| s.as_str()) == Some("ok")).then_some(envelope)
    }
}

#[async_trait]
impl MemberStars for NavidromeMemberStars {
    async fn is_starred(&self, user: &str, song_id: &str) -> bool {
        self.call(user, "getSong", song_id)
            .await
            .is_some_and(|envelope| envelope.pointer("/song/starred").is_some())
    }

    async fn star(&self, user: &str, song_id: &str) -> bool {
        self.call(user, "star", song_id).await.is_some()
    }
}

/// What the download service asks of the family pool while it places a file. With Family off
/// [`FamilyPlacement::pool_root`] is None and the download goes exactly as it always has.
#[async_trait]
pub trait FamilyPlacement: Send + Sync {
    /// Where member downloads are placed (the pool folder); None while Family is off.
    fn pool_root(&self) -> Option<String>;
    /// Whether a member's download may be placed in the pool now: the pool folder holds Octo's
    /// marker file ([`POOL_MARKER`]). Always true while Family is off.
    async fn placement_ready(&self) -> bool;
    /// Whether a path is a file in the pool.
    fn is_pool_path(&self, path: &str) -> bool;
    /// A file in the pool holding this song: by its outside id, then its ISRC, then its artist
    /// and title.
    async fn find_song(&self, provider: &str, external_id: &str, song: &Song) -> Option<PoolFile>;
    /// Whether a pool file is a FLAC the spectrum check found was made from lossy audio.
    fn made_from_lossy(&self, path: &str) -> bool;
    /// The path the first member's link has, for a file about to be linked.
    fn link_path_for(&self, pool_path: &str, user: &str) -> Option<String>;
    /// The owner's copy of a song recorded at `path`: a file in the pool (with Family on or
    /// off) moves into the shared library, still one file, and the answer is where it is now;
    /// any other file is the answer as it is. None when a pool file could not be moved, so the
    /// owner's copy is fetched as usual: the pool is never the owner's copy.
    async fn for_owner(&self, path: &str) -> Option<String>;
    /// Records a file just placed in the pool and links it for `members`. A copy of the same
    /// song already in the pool that this one betters is replaced for everyone, unless the file
    /// is `personal` (judged by its own people's checks): then it replaces that copy only for
    /// those of `members` who have it, and everyone else keeps theirs. Answers the first
    /// member's link.
    async fn placed(
        &self,
        path: &str,
        song: &Song,
        provider: &str,
        external_id: &str,
        members: &[String],
        personal: bool,
    ) -> Option<String>;
    /// Links a finished download for every member who asked, and moves it into the shared
    /// library when someone wants it there. A download that is in a shared library already
    /// stays there, and each member who asked but cannot see that library gets a pool copy of
    /// it (recorded as a copy of `outside_key`). Answers where the file now is.
    async fn settle(
        &self,
        path: &str,
        members: &[String],
        wants_shared: bool,
        outside_key: Option<&str>,
    ) -> String;
}

/// What favoriting a download on arrival asks of the family pool: a member who cannot see the
/// library a download landed in has their own copy, and the favorite goes on that copy.
#[async_trait]
pub trait MemberCopies: Send + Sync {
    /// The link to `who`'s own copy of the song with outside id `key` that landed as library
    /// song `library_id`, when `who` is a member and that library song is not theirs to see:
    /// in another member's library, or in a shared library they cannot see. None for everyone
    /// else, whose favorite goes on `library_id` itself.
    async fn own_copy_of(&self, who: &str, key: &str, library_id: &str) -> Option<String>;
    /// The library song id of `who`'s copy at `link`, once the library index finds it in their
    /// own library. None when it never shows.
    async fn own_song_at(&self, who: &str, link: &str) -> Option<String>;
}

/// A member's star or playlist entry to carry over to the song's new id once the index finds
/// it.
#[derive(Debug, Clone)]
struct Remap {
    user: String,
    old_id: Option<String>,
    starred: bool,
    new_path: String,
    scope: String,
}

/// A member's star on a song, read before its file moves.
struct StarBefore {
    user: String,
    old_id: Option<String>,
    starred: bool,
}

/// What a finished Navidrome scan showed of a file in a member's library.
#[derive(Debug, Clone)]
pub enum ScanCheck {
    /// The index finds it.
    Shown(IndexedSong),
    /// Navidrome says its scan is finished, and the index read after that still does not find it.
    Missed,
    /// Navidrome is still scanning, or cannot say.
    Unsure,
}

/// The lock a change holds while it reads and writes the pool's records. Its guard
/// ([`RecordsHeld`]) may not move to another thread, so an `async` change that held it across
/// an `.await` would make a future that may not either; every change runs as a task of its own
/// ([`FamilyPool::whole`]), which needs a future that may, so such a change does not build.
#[derive(Default)]
struct RecordsLock(Mutex<()>);

/// A change's hold on the pool's records, let go of when dropped.
struct RecordsHeld<'a> {
    _held: parking_lot::MutexGuard<'a, ()>,
    /// Makes the hold one that may not move to another thread, whatever the mutex allows.
    _here: PhantomData<*const ()>,
}

impl RecordsLock {
    fn lock(&self) -> RecordsHeld<'_> {
        RecordsHeld {
            _held: self.0.lock(),
            _here: PhantomData,
        }
    }
}

/// Pool files that changes are working on now, each held by one change at a time.
#[derive(Default)]
struct Claims {
    held: Mutex<HashSet<String>>,
    freed: tokio::sync::Notify,
}

/// A change's hold on pool files: while it lasts, no other change touches them. Let go when
/// dropped, however the change ends.
struct Claim<'a> {
    claims: &'a Claims,
    paths: Vec<String>,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        {
            let mut held = self.claims.held.lock();
            for path in &self.paths {
                held.remove(path);
            }
        }
        self.claims.freed.notify_waiters();
    }
}

/// A change's hold while it makes names ([`FamilyPool::naming`]): the placement lock, then the
/// gate's share, let go of in that order.
struct Naming {
    _placing: tokio::sync::MutexGuard<'static, ()>,
    _share: Option<LibraryWrite>,
}

impl Claims {
    /// Holds `paths` at once when none of them is held, without waiting; None when one is.
    fn try_claim(&self, paths: &[&str]) -> Option<Claim<'_>> {
        let mut paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        paths.sort();
        paths.dedup();
        let mut held = self.held.lock();
        if paths.iter().any(|path| held.contains(path)) {
            return None;
        }
        held.extend(paths.iter().cloned());
        Some(Claim { claims: self, paths })
    }

    /// Waits until none of `paths` is held, then holds them all at once (so two changes that
    /// each want two files never wait on each other).
    async fn claim(&self, paths: &[&str]) -> Claim<'_> {
        let mut paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        paths.sort();
        paths.dedup();
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            {
                let mut held = self.held.lock();
                if paths.iter().all(|path| !held.contains(path)) {
                    held.extend(paths.iter().cloned());
                    return Claim { claims: self, paths };
                }
            }
            freed.await;
        }
    }
}

/// The members folder and the pool folder as one change reads them, for the file work it does
/// on a blocking thread, and the music folder's staging folder, which keeps a record of each copy
/// while it is under way.
#[derive(Debug, Clone)]
struct Places {
    members: String,
    pool: String,
    staging: PathBuf,
}

impl Places {
    /// A pool file's path inside the pool.
    fn relative(&self, path: &str) -> Option<String> {
        relative_under(&self.pool, path)
    }

    /// A member's own folder.
    fn member_root(&self, name: &str) -> String {
        format!("{}/{}", self.members, folder_of(name))
    }

    /// Where `name`'s library has the pool file at `relative`.
    fn under(&self, name: &str, relative: &str) -> String {
        format!("{}/{}/{relative}", self.members, folder_of(name))
    }
}

/// The folder a member's library lives in. Adding a member makes it from the lower-case
/// username, while a member who joined with an existing Navidrome account keeps that account's
/// spelling ("Jamie"). Every path to a member's folder goes through here. A name with no capital
/// letter, which is every member Octo created itself, is borrowed as it is.
fn folder_of(name: &str) -> Cow<'_, str> {
    let name = name.trim();
    if name.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(name)
    }
}

/// Whether `name`'s folder is one plain folder name (see [`valid_username`]).
fn has_valid_folder(name: &str) -> bool {
    valid_username(&folder_of(name))
}

/// The family pool. See the module comment.
pub struct FamilyPool {
    me: Weak<FamilyPool>,
    settings: Arc<SettingsStore>,
    store: Arc<FamilyPoolStore>,
    family: Arc<FamilyStore>,
    index: Arc<LibraryIndex>,
    library: Arc<dyn ILocalLibraryService>,
    playlists: Arc<FamilyPlaylistStore>,
    stars: Arc<dyn MemberStars>,
    quarantine: Arc<LibraryActionQuarantine>,
    /// The shared library's folder, read at use (it follows Navidrome's detected folder).
    shared_root: Box<dyn Fn() -> String + Send + Sync>,
    mode: RwLock<LinkMode>,
    /// Held while a change reads and writes the records and decides what to do; never across a
    /// copy, a move, a call to Navidrome or any other `.await`.
    records: RecordsLock,
    /// The pool files changes are working on.
    claims: Claims,
    /// The music folder's one-writer gate, which a change that makes names takes a share of.
    write_gate: OnceLock<MusicFolderGate>,
    /// When the last sweep ran, by the steady clock.
    last_sweep: Mutex<Option<Duration>>,
    /// Pool files sweeps found missing, with when they were first found missing; a file goes
    /// only when a sweep at least [`SWEEP_SPACING`] later finds it missing still.
    misses: Mutex<HashMap<String, Miss>>,
    /// The wall clock, for what the log says.
    clock: RwLock<Clock>,
    /// The steady clock sweeps are spaced by.
    steady: RwLock<SteadyClock>,
    timing: PoolTiming,
    /// How long each copy waits before it starts (tests make copies slow with it).
    #[cfg(test)]
    copy_pause: Mutex<Duration>,
    /// How many sweeps got past the spacing (tests count them).
    #[cfg(test)]
    sweeps_run: std::sync::atomic::AtomicUsize,
}

/// The services a [`FamilyPool`] works with.
pub struct PoolParts {
    pub settings: Arc<SettingsStore>,
    pub store: Arc<FamilyPoolStore>,
    pub family: Arc<FamilyStore>,
    pub index: Arc<LibraryIndex>,
    pub library: Arc<dyn ILocalLibraryService>,
    pub playlists: Arc<FamilyPlaylistStore>,
    pub stars: Arc<dyn MemberStars>,
    pub shared_root: Box<dyn Fn() -> String + Send + Sync>,
}

impl FamilyPool {
    pub fn new(parts: PoolParts) -> Arc<Self> {
        Self::with_timing(parts, PoolTiming::default())
    }

    /// The same pool, waiting for the index on another schedule (tests).
    pub fn with_timing(parts: PoolParts, timing: PoolTiming) -> Arc<Self> {
        let quarantine = Arc::new(LibraryActionQuarantine::new(Arc::clone(&parts.settings)));
        let mode = if parts.store.link_state().copies {
            LinkMode::Copy
        } else {
            LinkMode::Symlink
        };
        Arc::new_cyclic(|me| FamilyPool {
            me: me.clone(),
            settings: parts.settings,
            store: parts.store,
            family: parts.family,
            index: parts.index,
            library: parts.library,
            playlists: parts.playlists,
            stars: parts.stars,
            quarantine,
            shared_root: parts.shared_root,
            mode: RwLock::new(mode),
            records: RecordsLock::default(),
            claims: Claims::default(),
            write_gate: OnceLock::new(),
            last_sweep: Mutex::new(None),
            misses: Mutex::new(HashMap::new()),
            clock: RwLock::new(Clock::system()),
            steady: RwLock::new(SteadyClock::system()),
            timing,
            #[cfg(test)]
            copy_pause: Mutex::new(Duration::ZERO),
            #[cfg(test)]
            sweeps_run: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn arc(&self) -> Option<Arc<FamilyPool>> {
        self.me.upgrade()
    }

    /// The music folder's one-writer gate, shared with placements and the library jobs: a
    /// promotion, a shared file's pool copy and a relayout each take a share of it. Only the first
    /// call counts.
    pub fn set_write_gate(&self, gate: MusicFolderGate) {
        let _ = self.write_gate.set(gate);
    }

    /// What a change that makes names in the shared library or the pool holds while it settles
    /// and moves, taken after its claims: the gate's share (no library job moves folders
    /// meanwhile), then the placement lock (no other route makes a folder of the same name at
    /// once), with every move into the library a stop left half done put right first. Nothing
    /// awaits another lock while it is held.
    async fn naming(&self) -> Naming {
        let share = match self.write_gate.get() {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        let placing = PLACEMENT.placing(&self.staging()).await;
        Naming {
            _placing: placing,
            _share: share,
        }
    }

    /// Reads the wall clock time from `clock` from now on (tests move it along).
    pub fn set_clock(&self, clock: Clock) {
        *self.clock.write() = clock;
    }

    /// Spaces sweeps by `steady` from now on (tests move it along).
    pub fn set_steady_clock(&self, steady: SteadyClock) {
        *self.steady.write() = steady;
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.read().now()
    }

    fn steady_now(&self) -> Duration {
        self.steady.read().elapsed()
    }

    /// Runs `change` as a task of its own and waits for it, so the change goes to its end,
    /// holding its claims until its last file operation is over, even when whoever asked for it
    /// stops waiting (a dropped web request, an aborted download). A panic in the change is
    /// the caller's panic again, its claims let go of on the way. `stopped` is the answer when
    /// the change cannot run because Octo is stopping.
    async fn whole<R, F, Fut>(&self, stopped: R, change: F) -> R
    where
        F: FnOnce(Arc<FamilyPool>) -> Fut,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let Some(pool) = self.arc() else {
            return stopped;
        };
        match tokio::spawn(change(pool)).await {
            Ok(done) => done,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => stopped,
        }
    }

    /// Makes every copy wait `pause` before it starts.
    #[cfg(test)]
    pub(crate) fn slow_copies(&self, pause: Duration) {
        *self.copy_pause.lock() = pause;
    }

    #[cfg(test)]
    fn copy_pause(&self) -> Duration {
        *self.copy_pause.lock()
    }

    #[cfg(not(test))]
    fn copy_pause(&self) -> Duration {
        Duration::ZERO
    }

    /// Whether Family is on.
    pub fn active(&self) -> bool {
        self.settings.current().family.enabled
    }

    pub fn store(&self) -> &Arc<FamilyPoolStore> {
        &self.store
    }

    pub fn index(&self) -> &Arc<LibraryIndex> {
        &self.index
    }

    pub fn timing(&self) -> PoolTiming {
        self.timing
    }

    /// The members folder, without a trailing slash.
    pub fn members_root(&self) -> String {
        super::members_place::resolve_in(&self.settings.current(), &(self.shared_root)())
    }

    /// The pool folder, `<members>/.pool`.
    pub fn pool_root_path(&self) -> String {
        format!("{}/{POOL_FOLDER}", self.members_root())
    }

    fn places(&self) -> Places {
        let members = self.members_root();
        let pool = format!("{members}/{POOL_FOLDER}");
        Places {
            members,
            pool,
            staging: self.staging(),
        }
    }

    /// The music folder's staging folder, which holds the records of moves and copies under way.
    /// The shared folder is the music folder.
    fn staging(&self) -> PathBuf {
        Path::new(&(self.shared_root)()).join(INCOMING_FOLDER_NAME)
    }

    /// A member's name as Octo keeps it (anyone else's as given).
    fn member_name(&self, user: &str) -> String {
        self.family
            .member(user)
            .map(|m| m.username)
            .unwrap_or_else(|| user.trim().to_string())
    }

    /// A member's own folder, `<members>/<user>`, named as [`folder_of`] says.
    pub fn member_root(&self, user: &str) -> String {
        format!("{}/{}", self.members_root(), folder_of(&self.member_name(user)))
    }

    /// A pool file's path inside the pool (`Artist/Album/01 Song.flac`).
    pub fn relative_in_pool(&self, path: &str) -> Option<String> {
        relative_under(&self.pool_root_path(), path)
    }

    /// Where `user`'s link to a pool file is.
    pub fn link_path(&self, pool_path: &str, user: &str) -> Option<String> {
        let relative = self.relative_in_pool(pool_path)?;
        Some(format!("{}/{relative}", self.member_root(user)))
    }

    /// The pool file a member's link (or copy) stands for.
    pub fn pool_path_of_link(&self, user: &str, link: &str) -> Option<String> {
        let root = self.member_root(user);
        let relative = link.strip_prefix(&root)?.strip_prefix('/')?;
        (!relative.is_empty()).then(|| format!("{}/{relative}", self.pool_root_path()))
    }

    /// How members' libraries get pool songs now.
    pub fn mode(&self) -> LinkMode {
        *self.mode.read()
    }

    /// The link mode with the words the Family page shows for it.
    pub fn link_status(&self) -> LinkStatus {
        LinkStatus::of(self.mode())
    }

    /// Tries a link in the members folder: makes a file, links it, reads through the link and
    /// removes both. Links when that works, copies when it does not, and copies whenever
    /// Octo has learned that Navidrome does not show links here.
    pub fn probe_links(&self) -> LinkStatus {
        let mode = if !self.store.link_state().copies && probe(Path::new(&self.members_root())) {
            LinkMode::Symlink
        } else {
            LinkMode::Copy
        };
        *self.mode.write() = mode;
        info!("FAMILY members folder links: {mode:?}");
        self.link_status()
    }

    /// Whether the pool folder holds Octo's marker file ([`POOL_MARKER`]): it is the disk Octo
    /// placed members' downloads on, and not an empty mount point where that disk should be.
    pub fn pool_ready(&self) -> bool {
        Path::new(&self.pool_root_path()).join(POOL_MARKER).is_file()
    }

    /// Writes the marker file in the pool folder, making the folder first. Done when Family is
    /// turned on.
    pub fn mark_pool_root(&self) -> io::Result<()> {
        let root = PathBuf::from(self.pool_root_path());
        make_dirs(&root)?;
        let marker = root.join(POOL_MARKER);
        if marker.is_file() {
            return Ok(());
        }
        std::fs::write(&marker, POOL_MARKER_TEXT)
    }

    /// Writes the marker file when the pool folder plainly is the pool: nothing is recorded yet
    /// (so nothing could be forgotten by mistake), or a recorded file is there. A folder with
    /// records but none of their files is left without one: it is most likely an empty mount
    /// point, and members' downloads wait until the disk is back. Done when Family is turned
    /// on, and when Octo starts with Family on. Answers why the marker is not there now.
    pub fn mark_when_plainly_ours(&self) -> Result<(), NotMarked> {
        if self.pool_ready() {
            return Ok(());
        }
        let recorded = self.store.list();
        let ours = recorded.is_empty() || recorded.iter().any(|file| Path::new(&file.path).is_file());
        if !ours {
            return Err(NotMarked::DiskMissing {
                recorded: recorded.len(),
            });
        }
        self.mark_pool_root().map_err(NotMarked::Io)
    }

    /// [`Self::mark_when_plainly_ours`], saying in the log why the marker is not there. Answers
    /// whether it is there now.
    pub fn keep_marker(&self) -> bool {
        match self.mark_when_plainly_ours() {
            Ok(()) => true,
            Err(NotMarked::DiskMissing { recorded }) => {
                warn!(
                    "FAMILY {} has no {POOL_MARKER} file and none of the {recorded} pool files Octo recorded; its disk may not be mounted, so no member download is placed there until it is",
                    self.pool_root_path()
                );
                false
            }
            Err(NotMarked::Io(e)) => {
                warn!(
                    "FAMILY could not write {POOL_MARKER} in {}: {e}",
                    self.pool_root_path()
                );
                false
            }
        }
    }

    /// Whether Navidrome has shown a linked song in a member's library yet.
    pub fn links_seen(&self) -> bool {
        self.mode() == LinkMode::Copy || self.store.link_state().seen
    }

    /// Navidrome showed a linked song in a member's library. Kept, so links are never given up
    /// on after that.
    pub fn confirm_links_seen(&self) {
        self.store.set_link_state(|links| links.seen = true);
    }

    /// Navidrome did not show `user`'s link at `link` in time. Only that one link becomes a
    /// real copy (written beside it first, then put in its place), and their library is scanned
    /// for it. Nothing changes once a link has ever been shown, or for anything but a link.
    /// Answers whether the link became a copy.
    pub async fn copy_unshown_link(&self, user: &str, link: &str) -> bool {
        let (user, link) = (user.to_string(), link.to_string());
        self.whole(false, move |pool| async move {
            pool.copy_unshown_link_now(&user, &link).await
        })
        .await
    }

    /// [`Self::copy_unshown_link`] on the caller's task.
    async fn copy_unshown_link_now(&self, user: &str, link: &str) -> bool {
        if self.links_seen() {
            return false;
        }
        let pool_path = self
            .pool_path_of_link(user, link)
            .unwrap_or_else(|| link.to_string());
        let _claim = self.claims.claim(&[&pool_path]).await;
        let (path, pause, staging) = (PathBuf::from(link), self.copy_pause(), self.staging());
        let copied = on_disk(move || -> io::Result<Option<u64>> {
            if !is_symlink(&path) {
                return Ok(None);
            }
            wait_before_copy(pause);
            copy_staged(&staging, &path, &path)?;
            Ok(Some(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)))
        })
        .await
        .and_then(|copied| copied);
        let size = match copied {
            Ok(Some(size)) => size,
            Ok(None) => return false,
            Err(e) => {
                warn!("FAMILY could not make a copy of {link} for {user}: {e}");
                return false;
            }
        };
        if let Some(library_id) = self.family.member(user).and_then(|m| m.personal_library_id) {
            self.index.note_placed(library_id, link, size);
            self.library.trigger_library_scan_of(library_id).await;
        }
        info!("FAMILY Navidrome did not show {user}'s link {link}; it is a copy now");
        true
    }

    /// What a finished scan shows of `path` (in `library` when given): Navidrome is asked
    /// whether it is still scanning, and once it says it is not, the index is read again.
    pub async fn after_finished_scan(&self, path: &str, library: Option<i64>) -> ScanCheck {
        match self.library.get_scan_status().await {
            Some(status) if !status.scanning => {}
            _ => return ScanCheck::Unsure,
        }
        if let Err(e) = self.index.refresh().await {
            warn!("FAMILY could not read the library index: {e}");
            return ScanCheck::Unsure;
        }
        match self.shown_at(path, library).await {
            Some(song) => ScanCheck::Shown(song),
            None => ScanCheck::Missed,
        }
    }

    /// A finished scan did not show the link to the pool file `song`, and Navidrome then showed
    /// the copy that took the link's place. Each song counts once. Once
    /// [`UNSHOWN_LINKS_FOR_COPIES`] songs have, with no link ever shown, the members folder is
    /// probed for links again, and only when that probe fails does every member get copies from
    /// now on ([`Self::fall_back_to_copies`]). Answers whether that happened now.
    pub async fn copy_shown_where_link_was_not(&self, song: &str) -> bool {
        let song = song.to_string();
        self.whole(false, move |pool| async move {
            pool.copy_shown_where_link_was_not_now(&song).await
        })
        .await
    }

    /// [`Self::copy_shown_where_link_was_not`] on the caller's task.
    async fn copy_shown_where_link_was_not_now(&self, song: &str) -> bool {
        if self.links_seen() {
            return false;
        }
        let counted = {
            let _records = self.records.lock();
            let mut counted = false;
            self.store.set_link_state(|links| {
                if links.missed.iter().any(|missed| missed == song) {
                    return;
                }
                links.missed.push(song.to_string());
                if links.missed.len() > MISSED_KEPT {
                    links.missed.remove(0);
                }
                links.unshown = links.unshown.saturating_add(1);
                counted = true;
            });
            counted
        };
        let unshown = self.store.link_state().unshown;
        if !counted || unshown < UNSHOWN_LINKS_FOR_COPIES {
            return false;
        }
        let root = PathBuf::from(self.members_root());
        if on_disk(move || probe(&root)).await.unwrap_or(true) {
            warn!(
                "FAMILY Navidrome showed copies of {unshown} songs where it did not show their links, but links work in the members folder; members keep getting links"
            );
            return false;
        }
        self.fall_back_to_copies_now().await;
        true
    }

    /// Links do not work here: every member gets real copies from now on (kept across
    /// restarts), and every link made so far becomes a copy, written beside the link before it
    /// takes the link's place. Answers how many links were replaced.
    pub async fn fall_back_to_copies(&self) -> usize {
        self.whole(0, |pool| async move { pool.fall_back_to_copies_now().await })
            .await
    }

    /// [`Self::fall_back_to_copies`] on the caller's task.
    async fn fall_back_to_copies_now(&self) -> usize {
        {
            let _records = self.records.lock();
            *self.mode.write() = LinkMode::Copy;
            self.store.set_link_state(|links| links.copies = true);
        }
        warn!("FAMILY links do not work for Navidrome here; members get copies from now on");
        let mut replaced = 0;
        for listed in self.store.list() {
            let _claim = self.claims.claim(&[&listed.path]).await;
            let Some(file) = self.store.get(&listed.path) else {
                continue;
            };
            let links: Vec<PathBuf> = file
                .refs
                .iter()
                .filter_map(|user| self.link_path(&file.path, user))
                .map(PathBuf::from)
                .collect();
            let (pause, staging) = (self.copy_pause(), self.staging());
            replaced += on_disk(move || {
                links
                    .iter()
                    .filter(|link| is_symlink(link))
                    .filter(|link| {
                        wait_before_copy(pause);
                        match copy_staged(&staging, link, link) {
                            Ok(()) => true,
                            Err(e) => {
                                warn!("FAMILY could not make a copy of {}: {e}", link.display());
                                false
                            }
                        }
                    })
                    .count()
            })
            .await
            .unwrap_or(0);
        }
        replaced
    }

    /// A pool file holding this song: its outside id, then its ISRC, then its artist and title
    /// (one version: a live take is its own song). Only files still on disk count.
    pub async fn find(
        &self,
        outside_key: Option<&str>,
        isrc: Option<&str>,
        artist: &str,
        title: &str,
    ) -> Option<PoolFile> {
        let mut candidates = Vec::new();
        candidates.extend(outside_key.and_then(|key| self.store.find_by_outside_id(key)));
        candidates.extend(isrc.and_then(|isrc| self.store.find_by_isrc(isrc)));
        let wanted = SongIdentity::match_key(artist, title);
        if !wanted.is_empty() {
            candidates.extend(
                self.store
                    .list()
                    .into_iter()
                    .filter(|file| SongIdentity::match_key(&file.artist, &file.title) == wanted),
            );
        }
        if candidates.is_empty() {
            return None;
        }
        on_disk(move || {
            candidates
                .into_iter()
                .find(|file| Path::new(&file.path).is_file())
        })
        .await
        .ok()
        .flatten()
    }

    /// Records a file in the pool (or updates its record).
    pub fn record(&self, file: PoolFile) {
        let _records = self.records.lock();
        self.store.upsert(file);
    }

    /// The member named `user`, when their name is one a member's own folder can have. A name
    /// that starts with a dot, or is not one plain folder name, would reach the pool or the
    /// members folder itself, so it never gets a library here.
    fn member_named(&self, user: &str) -> Result<Member, PoolError> {
        self.family
            .member(user)
            .filter(|member| has_valid_folder(&member.username))
            .ok_or_else(|| PoolError::NotMember(user.to_string()))
    }

    /// Puts a pool file in `user`'s library: a relative link (or a copy in copy mode), plus the
    /// song's lyrics files and the folder's cover. Records the member's reference, tells the
    /// index the file is there, and starts a scan of their library. Doing it twice changes
    /// nothing. Answers the link's path.
    pub async fn link(&self, pool_path: &str, user: &str) -> Result<String, PoolError> {
        let (pool_path, user) = (pool_path.to_string(), user.to_string());
        self.whole(Err(stopping()), move |pool| async move {
            let _claim = pool.claims.claim(&[&pool_path]).await;
            pool.link_claimed(&pool_path, &user).await
        })
        .await
    }

    /// [`Self::link`] for a change that holds the file's claim already.
    async fn link_claimed(&self, pool_path: &str, user: &str) -> Result<String, PoolError> {
        let member = self.member_named(user)?;
        let places = self.places();
        let relative = places
            .relative(pool_path)
            .ok_or_else(|| PoolError::NotInPool(pool_path.to_string()))?;
        let link = places.under(&member.username, &relative);
        let mode = self.mode();
        let size = {
            let (name, path, link) = (member.username.clone(), pool_path.to_string(), link.clone());
            on_disk(move || -> Result<u64, PoolError> {
                if !Path::new(&path).is_file() {
                    return Err(PoolError::NotInPool(path));
                }
                make_link(&places.staging, mode, &path, &link, &relative)?;
                for companion in companions(&path) {
                    let Some(companion_relative) = places.relative(&companion) else {
                        continue;
                    };
                    let companion_link = places.under(&name, &companion_relative);
                    if let Err(e) = make_link(
                        &places.staging,
                        mode,
                        &companion,
                        &companion_link,
                        &companion_relative,
                    ) {
                        warn!("FAMILY could not give {name} {companion}: {e}");
                    }
                }
                Ok(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0))
            })
            .await??
        };
        {
            let _records = self.records.lock();
            if self.store.get(pool_path).is_none() {
                self.store.upsert(unrecorded_file(pool_path, size, Vec::new()));
            }
            self.store.add_ref(pool_path, &member.username);
        }
        // Removing a member takes them out of the family, then lets go of every pool file still
        // recorded for them. A member taken out while this link was being made is seen here, and
        // gets nothing: either way no link is left for someone who is gone.
        if self.family.member(&member.username).is_none() {
            self.remove_link_claimed(pool_path, &member.username).await;
            let left = {
                let _records = self.records.lock();
                self.store.remove_ref(pool_path, &member.username)
            };
            if left.is_none_or(|left| left == 0) {
                self.gc_claimed(pool_path).await;
            }
            return Err(PoolError::NotMember(member.username));
        }
        if let Some(library_id) = member.personal_library_id {
            self.index.note_placed(library_id, &link, size);
            self.library.trigger_library_scan_of(library_id).await;
        }
        Ok(link)
    }

    /// Takes a pool file out of `user`'s library and drops their reference; the file goes to
    /// the trash once nobody has it.
    pub async fn unlink(&self, pool_path: &str, user: &str) -> Result<(), PoolError> {
        let member = self.member_named(user)?;
        let pool_path = pool_path.to_string();
        self.whole(Err(stopping()), move |pool| async move {
            pool.unlink_now(&pool_path, &member).await;
            Ok(())
        })
        .await
    }

    /// [`Self::unlink`] for a member, on the caller's task.
    async fn unlink_now(&self, pool_path: &str, member: &Member) {
        let _claim = self.claims.claim(&[pool_path]).await;
        self.remove_link_claimed(pool_path, &member.username).await;
        let left = {
            let _records = self.records.lock();
            self.store.remove_ref(pool_path, &member.username)
        };
        if left.is_none_or(|left| left == 0) {
            self.gc_claimed(pool_path).await;
        }
        if let Some(library_id) = member.personal_library_id {
            self.library.trigger_library_scan_of(library_id).await;
        }
    }

    /// Lets go of a download that landed for `user` after they stopped wanting it (their
    /// request was closed while it was on its way, as they left the family, or it could not be
    /// put in their library): their link and their reference go, and the file goes to the
    /// trash when nobody else has it. Whether or not they are still a member.
    pub async fn let_go(&self, pool_path: &str, user: &str) {
        if self.relative_in_pool(pool_path).is_none() {
            return;
        }
        let (pool_path, name) = (pool_path.to_string(), self.member_name(user));
        self.whole(
            (),
            move |pool| async move { pool.let_go_now(&pool_path, &name).await },
        )
        .await;
    }

    /// [`Self::let_go`] for `name`, on the caller's task.
    async fn let_go_now(&self, pool_path: &str, name: &str) {
        let name = name.to_string();
        let _claim = self.claims.claim(&[pool_path]).await;
        if has_valid_folder(&name) {
            self.remove_link_claimed(pool_path, &name).await;
        }
        let left = {
            let _records = self.records.lock();
            self.store.remove_ref(pool_path, &name)
        };
        if left.is_none_or(|left| left == 0) {
            self.gc_claimed(pool_path).await;
        }
        info!("FAMILY let go of {pool_path} for {name}, who no longer wants it");
    }

    /// Removes `name`'s link to a pool file (with its companions' links and any folders left
    /// empty), on a blocking thread.
    async fn remove_link_claimed(&self, pool_path: &str, name: &str) {
        let (places, name, path) = (self.places(), name.to_string(), pool_path.to_string());
        let removed = on_disk(move || {
            let companions = companions(&path);
            remove_member_link(&places, &name, &path, &companions);
        })
        .await;
        if let Err(e) = removed {
            warn!("FAMILY could not remove a link to {pool_path}: {e}");
        }
    }

    /// Forgets pool files that are no longer on disk (someone deleted one by hand): each
    /// member's link to it goes, and their library is scanned. Sweeps run at most every
    /// [`SWEEP_SPACING`], and a file goes only when a sweep at least that long after the one
    /// that first missed it misses it still. Nothing goes while the pool folder lacks Octo's
    /// marker file (a members disk that is not mounted looks like every file gone, and a
    /// download onto its empty mount point makes the folder look alive). Time is read from the
    /// steady clock, so a wall clock set back or forward changes nothing. Answers how many
    /// files went.
    pub async fn sweep_dangling(&self) -> usize {
        self.whole(0, |pool| async move { pool.sweep_dangling_now().await })
            .await
    }

    /// [`Self::sweep_dangling`] on the caller's task.
    async fn sweep_dangling_now(&self) -> usize {
        let (now, wall) = (self.steady_now(), self.now());
        {
            let mut last = self.last_sweep.lock();
            if last.is_some_and(|at| now.saturating_sub(at) < SWEEP_SPACING) {
                return 0;
            }
            *last = Some(now);
        }
        #[cfg(test)]
        self.sweeps_run.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let recorded = self.store.list();
        if recorded.is_empty() {
            self.misses.lock().clear();
            return 0;
        }
        let root = self.pool_root_path();
        let paths: Vec<String> = recorded.into_iter().map(|file| file.path).collect();
        let checked = {
            let root = root.clone();
            on_disk(move || {
                Path::new(&root).join(POOL_MARKER).is_file().then(|| {
                    paths
                        .into_iter()
                        .filter(|path| !Path::new(path).exists())
                        .collect::<HashSet<String>>()
                })
            })
            .await
        };
        let Ok(Some(missing)) = checked else {
            warn!(
                "FAMILY the pool folder {root} has no {POOL_MARKER} file (its disk may not be mounted); its records are kept as they are"
            );
            self.misses.lock().clear();
            return 0;
        };
        let gone: Vec<(String, Miss)> = {
            let mut misses = self.misses.lock();
            misses.retain(|path, _| missing.contains(path));
            let mut gone = Vec::new();
            for path in missing {
                match misses.get(&path) {
                    Some(first) if now.saturating_sub(first.since) >= SWEEP_SPACING => {
                        gone.push((path, *first));
                    }
                    Some(_) => {}
                    None => {
                        misses.insert(path, Miss { since: now, at: wall });
                    }
                }
            }
            gone
        };
        let mut count = 0;
        for (path, first) in gone {
            let _claim = self.claims.claim(&[&path]).await;
            self.misses.lock().remove(&path);
            let Some(file) = self.store.get(&path) else {
                continue;
            };
            let still_missing = {
                let path = path.clone();
                on_disk(move || !Path::new(&path).exists()).await.unwrap_or(false)
            };
            if !still_missing {
                continue;
            }
            for user in &file.refs {
                let (places, name, pool_path) = (self.places(), self.member_name(user), path.clone());
                let _ = on_disk(move || remove_member_link(&places, &name, &pool_path, &[])).await;
                if let Some(library_id) = self.family.member(user).and_then(|m| m.personal_library_id) {
                    self.library.trigger_library_scan_of(library_id).await;
                }
            }
            {
                let _records = self.records.lock();
                self.store.remove(&path);
            }
            if let Err(e) = self.library.forget_mapping(&path).await {
                warn!("FAMILY could not forget {path}'s mapping: {e}");
            }
            warn!(
                "FAMILY {path} is gone from the pool (missing since {}); removed the links of {}",
                first.at.to_rfc3339(),
                file.refs.join(", ")
            );
            count += 1;
        }
        count
    }

    /// Sends a pool file nobody links any more to the trash (kept as long as library actions
    /// keep removed songs), with its lyrics files. A file a member's library still links on
    /// disk is never trashed, whatever the record says: the record learns of them again.
    /// True when it went.
    pub async fn gc(&self, pool_path: &str) -> bool {
        let pool_path = pool_path.to_string();
        self.whole(false, move |pool| async move {
            let _claim = pool.claims.claim(&[&pool_path]).await;
            pool.gc_claimed(&pool_path).await
        })
        .await
    }

    /// [`Self::gc`] for a change that holds the file's claim already.
    async fn gc_claimed(&self, pool_path: &str) -> bool {
        if self
            .store
            .get(pool_path)
            .is_some_and(|file| !file.refs.is_empty())
        {
            return false;
        }
        let names: Vec<String> = self
            .family
            .members()
            .into_iter()
            .map(|member| member.username)
            .filter(|name| has_valid_folder(name))
            .collect();
        let (places, path, quarantine) = (self.places(), pool_path.to_string(), Arc::clone(&self.quarantine));
        let outcome = on_disk(move || trash_unless_linked(&places, &names, &path, &quarantine)).await;
        match outcome {
            Ok(Trashing::Held { holders, size }) => {
                {
                    let _records = self.records.lock();
                    self.restore_refs(pool_path, &holders, size);
                }
                warn!(
                    "FAMILY {pool_path} had no record of who has it, but {} still do; it stays",
                    holders.join(", ")
                );
                false
            }
            Ok(Trashing::Gone) => {
                {
                    let _records = self.records.lock();
                    self.store.remove(pool_path);
                }
                if let Err(e) = self.library.forget_mapping(pool_path).await {
                    warn!("FAMILY could not forget {pool_path}'s mapping: {e}");
                }
                true
            }
            Ok(Trashing::Stayed) => false,
            Err(e) => {
                warn!("FAMILY could not tidy {pool_path} away: {e}");
                false
            }
        }
    }

    /// Records `holders` as linking a pool file, recording the file first when it is not.
    fn restore_refs(&self, pool_path: &str, holders: &[String], size: u64) {
        if self.store.get(pool_path).is_none() {
            self.store
                .upsert(unrecorded_file(pool_path, size, holders.to_vec()));
            return;
        }
        for holder in holders {
            self.store.add_ref(pool_path, holder);
        }
    }

    /// Reads, from Navidrome, whether each of `users` has starred the song their own copy at
    /// `pool_path` is in their library.
    async fn stars_before(&self, pool_path: &str, users: &[String]) -> Vec<StarBefore> {
        let mut read = Vec::new();
        for user in users {
            let Some(link) = self.link_path(pool_path, user) else {
                continue;
            };
            let old_id = self.index.find_by_path(&link).map(|s| s.id);
            let starred = match &old_id {
                Some(id) => self.stars.is_starred(user, id).await,
                None => false,
            };
            read.push(StarBefore {
                user: user.clone(),
                old_id,
                starred,
            });
        }
        read
    }

    /// A better copy of a song replaces `old_path` for everyone who has it: each member's link
    /// moves to `new_path`, the old file goes to the trash, and their stars and family playlist
    /// entries follow the song once the index finds it again. Answers the members moved.
    pub async fn repoint(&self, old_path: &str, new_path: &str) -> Vec<String> {
        let (old_path, new_path) = (old_path.to_string(), new_path.to_string());
        self.whole(Vec::new(), move |pool| async move {
            let _claim = pool.claims.claim(&[&old_path, &new_path]).await;
            pool.repoint_claimed(&old_path, &new_path, None).await
        })
        .await
    }

    /// [`Self::repoint`] for a change that holds both files' claims already. With `only`, just
    /// those of them who have the old copy move, and the old copy stays as it is while anyone
    /// else still has it.
    async fn repoint_claimed(&self, old_path: &str, new_path: &str, only: Option<&[String]>) -> Vec<String> {
        if old_path == new_path {
            return Vec::new();
        }
        let Some(old) = self.store.get(old_path) else {
            return Vec::new();
        };
        let moving: Vec<String> = old
            .refs
            .iter()
            .filter(|user| only.is_none_or(|only| only.iter().any(|member| eq_ignore_case(member, user))))
            .cloned()
            .collect();
        let mut remaps = Vec::new();
        let mut moved = Vec::new();
        for before in self.stars_before(old_path, &moving).await {
            let Some(new_link) = self.link_path(new_path, &before.user) else {
                continue;
            };
            match self.link_claimed(new_path, &before.user).await {
                Ok(_) => {
                    let name = self.member_name(&before.user);
                    self.remove_link_claimed(old_path, &name).await;
                    {
                        let _records = self.records.lock();
                        self.store.remove_ref(old_path, &before.user);
                    }
                    moved.push(before.user.clone());
                    remaps.push(Remap {
                        scope: format!("Personal:{}", before.user),
                        user: before.user,
                        old_id: before.old_id,
                        starred: before.starred,
                        new_path: new_link,
                    });
                }
                Err(e) => warn!(
                    "FAMILY could not move {} to the better copy {new_path}: {e}",
                    before.user
                ),
            }
        }
        if only.is_some() && self.store.get(old_path).is_some_and(|file| !file.refs.is_empty()) {
            info!(
                "FAMILY {} moved from {old_path} to the better copy {new_path}; everyone else keeps {old_path}",
                moved.join(", ")
            );
            self.carry_over(remaps);
            return moved;
        }
        // Whoever bettered the copy is recorded too, so the store follows the outside ids.
        {
            let _records = self.records.lock();
            if let Some(mut better) = self.store.get(new_path) {
                for id in old.outside_ids {
                    if !better.outside_ids.contains(&id) {
                        better.outside_ids.push(id);
                    }
                }
                if better.isrc.is_none() {
                    better.isrc = old.isrc;
                }
                self.store.upsert(better);
            }
        }
        self.gc_claimed(old_path).await;
        info!(
            "FAMILY {} moved from {old_path} to the better copy {new_path}",
            moved.join(", ")
        );
        self.carry_over(remaps);
        moved
    }

    /// The shared library wants a pool song: the file moves into the shared library's layout,
    /// every member's link goes (they see it there now), and their stars and family playlist
    /// entries follow the song. Still one file. A member who cannot see that shared library
    /// keeps their link, and the pool keeps the file for them: then the shared library gets a
    /// copy. Answers its new path.
    pub async fn promote(&self, pool_path: &str) -> Result<String, PoolError> {
        let pool_path = pool_path.to_string();
        self.whole(Err(stopping()), move |pool| async move {
            let _claim = pool.claims.claim(&[&pool_path]).await;
            pool.promote_claimed(&pool_path, None).await
        })
        .await
    }

    /// A song the owner already has a copy of at `path`, as the owner may have it: a pool file
    /// (with Family on or off) moves into the shared library like [`Self::promote`], still one
    /// file, and the answer is where it is now; any other file is the answer as it is. None
    /// when a pool file could not be moved: the pool is never the owner's copy.
    pub async fn for_owner(&self, path: &str) -> Option<String> {
        if self.relative_in_pool(path).is_none() {
            return Some(path.to_string());
        }
        let path = path.to_string();
        self.whole(None, move |pool| async move {
            let _claim = pool.claims.claim(&[&path]).await;
            match pool.promote_claimed(&path, None).await {
                Ok(moved) => Some(moved),
                Err(e) => {
                    warn!("FAMILY could not move {path} into the shared library for the owner: {e}");
                    None
                }
            }
        })
        .await
    }

    /// [`Self::promote`] for a change that holds the file's claim already. `leaving` (a member
    /// being let go) never keeps the file in the pool, whatever they can see.
    async fn promote_claimed(&self, pool_path: &str, leaving: Option<&str>) -> Result<String, PoolError> {
        let places = self.places();
        let relative = places
            .relative(pool_path)
            .ok_or_else(|| PoolError::NotInPool(pool_path.to_string()))?;
        let there = {
            let path = pool_path.to_string();
            on_disk(move || Path::new(&path).is_file()).await?
        };
        if !there {
            return Err(PoolError::NotInPool(pool_path.to_string()));
        }
        let shared_root = (self.shared_root)().trim_end_matches('/').to_string();
        let refs = self.store.get(pool_path).map(|f| f.refs).unwrap_or_default();
        let shared_library = self.index.library_of_folder(&shared_root);
        let (seers, keepers): (Vec<String>, Vec<String>) = refs.into_iter().partition(|user| {
            leaving.is_some_and(|leaving| eq_ignore_case(leaving, user))
                || self
                    .family
                    .member(user)
                    .is_none_or(|member| sees_shared_library(&member, shared_library))
        });
        let stars = self.stars_before(pool_path, &seers).await;

        let stays_pooled = !keepers.is_empty();
        let seer_names: Vec<String> = seers.iter().map(|user| self.member_name(user)).collect();
        let (path, pause) = (pool_path.to_string(), self.copy_pause());
        let layout = Layout::from_settings(&self.settings.current().subsonic);
        let naming = self.naming().await;
        // Filed under the shared folder from its own tags as a new song is: the folders already
        // there by those names or other spellings of them, the folder already holding its album,
        // and its disc folders.
        let target = self
            .file_by_tags(pool_path, &shared_root, &relative, &layout, PATH_BUDGET)
            .await?;
        let moved = {
            let target = target.clone();
            on_disk(move || -> io::Result<()> {
                let companions = companions(&path);
                wait_before_copy(pause);
                if stays_pooled {
                    copy_staged(&places.staging, Path::new(&path), Path::new(&target))?;
                } else {
                    move_pool_file(&places.staging, Path::new(&path), Path::new(&target))?;
                }
                carry_companions(&places.staging, &companions, Path::new(&target), stays_pooled);
                for name in &seer_names {
                    remove_member_link(&places, name, &path, &companions);
                }
                if !stays_pooled && let Some(folder) = Path::new(&path).parent() {
                    remove_lonely_covers(folder);
                    prune_empty(folder, Path::new(&places.pool));
                }
                Ok(())
            })
            .await
        };
        drop(naming);
        moved??;
        {
            let _records = self.records.lock();
            if stays_pooled {
                for user in &seers {
                    self.store.remove_ref(pool_path, user);
                }
            } else {
                self.store.remove(pool_path);
            }
        }
        match self.library.forget_mapping(pool_path).await {
            Ok(mappings) => {
                if let Err(e) = self.library.restore_mappings(&mappings, &target).await {
                    warn!("FAMILY could not point {target}'s mappings at it: {e}");
                }
            }
            Err(e) => warn!("FAMILY could not forget {pool_path}'s mapping: {e}"),
        }
        self.library.trigger_library_scan(true).await;
        for user in &seers {
            if let Some(library_id) = self.family.member(user).and_then(|m| m.personal_library_id) {
                self.library.trigger_library_scan_of(library_id).await;
            }
        }
        if stays_pooled {
            info!(
                "FAMILY {pool_path} was copied into the shared library as {target}; it stays in the pool for {}, who cannot see that library",
                keepers.join(", ")
            );
        } else {
            info!("FAMILY {pool_path} moved into the shared library as {target}");
        }
        let remaps = stars
            .into_iter()
            .map(|before| Remap {
                user: before.user,
                old_id: before.old_id,
                starred: before.starred,
                new_path: target.clone(),
                scope: "Shared".to_string(),
            })
            .collect();
        self.carry_over(remaps);
        Ok(target)
    }

    /// Whether `path` is a file of the shared library: neither in the pool nor in a member's
    /// own folder.
    pub fn is_shared_file(&self, path: &str) -> bool {
        let members = format!("{}/", self.members_root());
        !path.trim().is_empty() && !path.starts_with(&members)
    }

    /// The shared library a shared file is in: the index's answer for the file, else the
    /// library of the nearest folder above it that the index holds songs in, up to the shared
    /// folder. None when the index cannot tell yet.
    pub fn shared_library_of(&self, path: &str) -> Option<i64> {
        if let Some(library) = self.index.find_by_path(path).and_then(|song| song.library_id) {
            return Some(library);
        }
        let shared_root = (self.shared_root)();
        let shared_root = shared_root.trim_end_matches('/');
        if shared_root.is_empty() {
            return None;
        }
        let inside = format!("{shared_root}/");
        let mut folder = Path::new(path).parent();
        while let Some(dir) = folder {
            let text = dir.to_string_lossy();
            if text != shared_root && !text.starts_with(&inside) {
                return None;
            }
            if let Some(library) = self.index.library_of_folder(&text) {
                return Some(library);
            }
            if text == shared_root {
                return None;
            }
            folder = dir.parent();
        }
        None
    }

    /// Whether `user` sees the shared library a shared file is in. Anyone who is not a member
    /// (the owner) sees every library.
    pub fn sees_shared_file(&self, user: &str, path: &str) -> bool {
        self.family
            .member(user)
            .is_none_or(|member| sees_shared_library(&member, self.shared_library_of(path)))
    }

    /// A shared file's path inside the pool, before it is settled there: its path under the
    /// shared folder, or its last three parts (`Artist/Album/file`) when it is somewhere else.
    fn pool_relative_of_shared(&self, shared_path: &str) -> Option<String> {
        let shared_root = (self.shared_root)();
        let shared_root = shared_root.trim_end_matches('/');
        let under_root = (!shared_root.is_empty())
            .then(|| shared_path.strip_prefix(shared_root)?.strip_prefix('/'))
            .flatten()
            .map(str::to_string);
        let relative = under_root.unwrap_or_else(|| {
            let mut parts: Vec<&str> = shared_path.rsplit('/').take(3).collect();
            parts.reverse();
            parts.join("/")
        });
        let plain = !relative.is_empty()
            && relative
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..");
        plain.then_some(relative)
    }

    /// Gives `user`, who cannot see the shared library a shared file is in, a copy they can
    /// play through the pool: the file, with its lyrics and its folder's cover, is copied once
    /// into the pool at its place in the shared layout (each name cleaned and settled there as a
    /// new song's is), recorded as a copy of that file and of `song`, and linked into their
    /// library like any pool song. Another member who needs it gets a link to that same pool
    /// file, found by its record (the copy of this file, or of this outside song), never by where
    /// a copy would go. Two changes for one shared file take turns. Answers their link.
    pub async fn copy_shared_for(
        &self,
        shared_path: &str,
        user: &str,
        outside_key: Option<&str>,
        song: Option<&Song>,
    ) -> Result<String, PoolError> {
        let member = self.member_named(user)?;
        let relative = self
            .pool_relative_of_shared(shared_path)
            .ok_or_else(|| PoolError::NotInPool(shared_path.to_string()))?;
        let (shared_path, outside_key, song) = (
            shared_path.to_string(),
            outside_key.map(str::to_string),
            song.cloned(),
        );
        self.whole(Err(stopping()), move |pool| async move {
            let source = format!("{COPY_OF}{shared_path}");
            let _source = pool.claims.claim(&[&source]).await;
            pool.copy_shared_claimed(
                &shared_path,
                &member,
                &relative,
                outside_key.as_deref(),
                song.as_ref(),
            )
            .await
        })
        .await
    }

    /// The pool's copy of a shared file, found by its record: the copy made of this file, else a
    /// pool file of this outside song. Only a file still on disk counts.
    async fn recorded_copy(&self, shared_path: &str, outside_key: Option<&str>) -> Option<String> {
        let mut candidates: Vec<String> = Vec::new();
        candidates.extend(self.store.find_by_source(shared_path).map(|file| file.path));
        candidates.extend(
            outside_key
                .and_then(|key| self.store.find_by_outside_id(key))
                .map(|file| file.path),
        );
        if candidates.is_empty() {
            return None;
        }
        on_disk(move || candidates.into_iter().find(|path| Path::new(path).is_file()))
            .await
            .ok()
            .flatten()
    }

    /// [`Self::copy_shared_for`] for a change that holds the claim on copies of `shared_path`.
    /// `relative` is the copy's path in the pool before it is settled.
    async fn copy_shared_claimed(
        &self,
        shared_path: &str,
        member: &Member,
        relative: &str,
        outside_key: Option<&str>,
        song: Option<&Song>,
    ) -> Result<String, PoolError> {
        if let Some(existing) = self.recorded_copy(shared_path, outside_key).await {
            let _claim = self.claims.claim(&[&existing]).await;
            // Looked at again now that this change holds it: another change may have moved it.
            if self.store.get(&existing).is_some() {
                let size = {
                    let path = existing.clone();
                    on_disk(move || std::fs::metadata(&path).map(|m| m.len()).ok()).await?
                };
                if let Some(size) = size {
                    return self
                        .give_copy(shared_path, member, &existing, size, outside_key, song)
                        .await;
                }
            }
        }
        let from = PathBuf::from(shared_path);
        let layout = Layout::from_settings(&self.settings.current().subsonic);
        let pool_root = self.pool_root_path();
        loop {
            let naming = self.naming().await;
            // Filed in the pool from the file's own tags as a member's download is, with the
            // album's folder by any name and its disc folders.
            let target = self
                .file_by_tags(shared_path, &pool_root, relative, &layout, POOL_BUDGET)
                .await?;
            let Some(_claim) = self.claims.try_claim(&[&target]) else {
                // Another change holds that name: once it is done, the name is settled again.
                drop(naming);
                drop(self.claims.claim(&[&target]).await);
                continue;
            };
            let (from, to, pause, staging) = (
                from.clone(),
                PathBuf::from(&target),
                self.copy_pause(),
                self.staging(),
            );
            let size = on_disk(move || -> io::Result<u64> {
                wait_before_copy(pause);
                copy_with_companions(&staging, &from, &to)?;
                Ok(std::fs::metadata(&to).map(|m| m.len()).unwrap_or(0))
            })
            .await;
            drop(naming);
            let size = size??;
            info!(
                "FAMILY {shared_path} was copied into the pool as {target} for {}, who cannot see its shared library",
                member.username
            );
            return self
                .give_copy(shared_path, member, &target, size, outside_key, song)
                .await;
        }
    }

    /// Where the file at `path`, now at `relative`, goes under `root`: filed by the layout as a
    /// download is (the album's folder by any name, its disc folders, and a new disc landing
    /// loose beside an album's loose files). The names come from what the File layout task names
    /// the file by: for a file Octo placed, the values its mapping saved, each field kept only
    /// while its tag still says what was written; else the file's own tags. When neither can
    /// name the song, its path's own names are cleaned and settled. A name already taken gives
    /// the next free one. For a change that holds the naming locks.
    async fn file_by_tags(
        &self,
        path: &str,
        root: &str,
        relative: &str,
        layout: &Layout,
        budget: usize,
    ) -> io::Result<String> {
        let name = relative.rsplit('/').next().unwrap_or(relative);
        let extension = match name.rfind('.') {
            Some(dot) if dot > 0 => name[dot..].to_string(),
            _ => String::new(),
        };
        let mappings = self.library.get_mappings().await;
        let placed = mapping_at(&mappings, path).map(|mapping| LayoutMapping {
            title: mapping.title.clone(),
            album: mapping.album.clone(),
            artist: mapping.artist.clone(),
            values: mapping.layout.clone(),
        });
        let first = {
            let (path, root, relative, layout, extension) = (
                path.to_string(),
                root.to_string(),
                relative.to_string(),
                layout.clone(),
                extension.clone(),
            );
            on_disk(move || -> io::Result<Filing> {
                if !Path::new(&path).is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{path} is not there"),
                    ));
                }
                let tags = album_identity::read_layout_tags(Path::new(&path));
                let values = tags.as_ref().and_then(|tags| {
                    placed
                        .as_ref()
                        .and_then(|placed| kept_values(placed, tags))
                        .or_else(|| values_from_tags(tags))
                });
                Ok(match values {
                    Some(values) => Filing::Filed(file_values(&layout, &root, values, &extension, budget)?),
                    None => Filing::Path(settle_relative(&root, &relative, tags.as_ref(), &layout, budget)?),
                })
            })
            .await??
        };
        let filed = match first {
            Filing::Filed(filed) => filed,
            Filing::Path(target) => return Ok(target),
        };
        let (root, layout) = (root.to_string(), layout.clone());
        let filed = on_disk(move || choose_loose(&layout, &root, filed, &extension, budget)).await??;
        Ok(match filed.settled.taken {
            Some(_) => PathHelper::resolve_unique_path(&filed.settled.path),
            None => filed.settled.path,
        })
    }

    /// Records the pool file at `target` as the copy of `shared_path` and of `song`, and links it
    /// into `member`'s library, for a change that holds its claim. A copy nobody could be given
    /// is not kept.
    async fn give_copy(
        &self,
        shared_path: &str,
        member: &Member,
        target: &str,
        size: u64,
        outside_key: Option<&str>,
        song: Option<&Song>,
    ) -> Result<String, PoolError> {
        {
            let _records = self.records.lock();
            self.record_shared_copy(target, size, outside_key, song);
            self.store.set_source(target, shared_path);
        }
        let linked = self.link_claimed(target, &member.username).await;
        if linked.is_err() && self.store.get(target).is_some_and(|file| file.refs.is_empty()) {
            self.gc_claimed(target).await;
        }
        linked
    }

    /// Records a pool copy of a shared file as a copy of `song`. A record it already has gains
    /// the outside id, and whatever titles and ISRC it was missing.
    fn record_shared_copy(&self, pool_path: &str, size: u64, outside_key: Option<&str>, song: Option<&Song>) {
        if let Some(mut file) = self.store.get(pool_path) {
            let mut changed = false;
            if let Some(key) = outside_key
                && !file.outside_ids.iter().any(|id| id == key)
            {
                file.outside_ids.push(key.to_string());
                changed = true;
            }
            if let Some(song) = song {
                for (kept, known) in [
                    (&mut file.title, &song.title),
                    (&mut file.artist, &song.artist),
                    (&mut file.album, &song.album),
                ] {
                    if kept.is_empty() && !known.is_empty() {
                        kept.clone_from(known);
                        changed = true;
                    }
                }
                if file.isrc.is_none()
                    && let Some(isrc) = song.isrc.clone().filter(|isrc| !isrc.trim().is_empty())
                {
                    file.isrc = Some(isrc);
                    changed = true;
                }
            }
            if changed {
                self.store.upsert(file);
            }
            return;
        }
        let format = extension_of(pool_path);
        self.store.upsert(PoolFile {
            path: pool_path.to_string(),
            size,
            transcoded_from: song
                .and_then(|s| s.transcoded_from.clone())
                .filter(|from| format == "flac" && !from.trim().is_empty()),
            format,
            outside_ids: outside_key.map(str::to_string).into_iter().collect(),
            isrc: song
                .and_then(|s| s.isrc.clone())
                .filter(|isrc| !isrc.trim().is_empty()),
            title: song.map(|s| s.title.clone()).unwrap_or_default(),
            artist: song.map(|s| s.artist.clone()).unwrap_or_default(),
            album: song.map(|s| s.album.clone()).unwrap_or_default(),
            refs: Vec::new(),
        });
    }

    /// Holds the pool files at `paths` for the library tidy's job, which holds the music folder's
    /// gate whole while it moves them: until the hold is dropped no other change touches them.
    /// It never waits: None when another change is working on one of them, which may itself be
    /// waiting for the gate, so the job lets the gate go a moment and asks again.
    pub fn try_hold(&self, paths: &[&str]) -> Option<PoolFilesHeld> {
        let pool = self.arc()?;
        let mut paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        paths.sort();
        paths.dedup();
        {
            let mut held = self.claims.held.lock();
            if paths.iter().any(|path| held.contains(path)) {
                return None;
            }
            held.extend(paths.iter().cloned());
        }
        Some(PoolFilesHeld(Arc::new(HeldFiles { pool, paths })))
    }

    /// Moves a pool file from `old` to `new` (both in the pool, both in `held`) for the library
    /// tidy's File layout task, while its job holds the music folder's gate whole: no download,
    /// lyrics write or other placement runs until the job lets the gate go, so none of them sees
    /// an album half moved. It takes no share of the gate (the job's hold covers it), only the
    /// placement lock. Before anything moves, the new place is looked at in the pool and in the
    /// library of every member who has the song: anything there by that name or another spelling
    /// of it, or by another spelling of a folder on the way, holds the move, with the words its
    /// row shows. Then the file and its own lyrics files move, its record follows, and each
    /// member's link is made again at the new place, pointing at it (a member's real copy is
    /// renamed instead). The covers of the folder it leaves are copied into a new folder that has
    /// none. Each move is told to `remap`, so a lyrics job waiting for the gate writes where the
    /// song is now. Stars are not carried over: Navidrome finds a moved file again by its tags.
    /// `relayout_held(new, old)` puts it back. Answers [`RelayoutOutcome::Moved`] also when it
    /// was already done.
    pub async fn relayout_held(
        &self,
        held: &PoolFilesHeld,
        old: &str,
        new: &str,
        remap: Option<crate::services::lyrics::LyricsPathRemap>,
    ) -> anyhow::Result<RelayoutOutcome> {
        if !held.covers(old) || !held.covers(new) {
            anyhow::bail!("{old} and {new} are not both held for the move");
        }
        let (old, new, held) = (old.to_string(), new.to_string(), held.clone());
        self.whole(Err(anyhow::anyhow!("Octo is stopping")), move |pool| async move {
            let _placing = PLACEMENT.lock().await;
            let moved = pool.relayout_claimed(&old, &new, remap.as_ref()).await;
            drop(held);
            moved
        })
        .await
    }

    /// [`Self::relayout_held`] for a test, holding both paths first.
    #[cfg(test)]
    pub(crate) async fn relayout(&self, old: &str, new: &str) -> anyhow::Result<RelayoutOutcome> {
        let held = self.try_hold(&[old, new]).expect("no other change holds them");
        self.relayout_held(&held, old, new, None).await
    }

    /// [`Self::relayout_held`] for a change that holds both paths and the placement lock, under
    /// the job's hold on the gate.
    async fn relayout_claimed(
        &self,
        old: &str,
        new: &str,
        remap: Option<&crate::services::lyrics::LyricsPathRemap>,
    ) -> anyhow::Result<RelayoutOutcome> {
        let places = self.places();
        let (Some(old_relative), Some(new_relative)) = (places.relative(old), places.relative(new)) else {
            anyhow::bail!("{old} and {new} are not both files in the family pool");
        };
        if old_relative == new_relative {
            return Ok(RelayoutOutcome::Moved);
        }
        let holders: Vec<String> = self
            .store
            .get(old)
            .or_else(|| self.store.get(new))
            .map(|file| file.refs)
            .unwrap_or_default()
            .iter()
            .filter_map(|user| self.family.member(user).map(|member| member.username))
            .filter(|name| has_valid_folder(name))
            .collect();
        let move_ = PoolMove {
            places,
            old: old.to_string(),
            new: new.to_string(),
            old_relative,
            new_relative,
        };
        let held = {
            let (move_, holders) = (move_.clone(), holders.clone());
            on_disk(move || move_.held_by(&holders)).await??
        };
        if let Some(words) = held {
            info!("FAMILY {old} stays where it is: {words}");
            return Ok(RelayoutOutcome::Held(words));
        }
        let sidecars = {
            let move_ = move_.clone();
            on_disk(move || move_.move_in_pool()).await??
        };
        // Told before anything is let go of, so no lyrics job takes the gate between the move
        // and the record of it.
        let mut moved = vec![(old.to_string(), new.to_string())];
        moved.extend(holders.iter().map(|name| {
            (
                move_.places.under(name, &move_.old_relative),
                move_.places.under(name, &move_.new_relative),
            )
        }));
        if let Some(remap) = remap {
            remap.record(&moved);
        }
        {
            let _records = self.records.lock();
            self.store.move_file(old, new);
        }
        let mode = self.mode();
        on_disk(move || {
            for name in &holders {
                move_.relink(mode, name, &sidecars);
            }
        })
        .await?;
        info!("FAMILY {old} moved to {new} for the file layout");
        Ok(RelayoutOutcome::Moved)
    }

    /// Lets go of everything a member who is leaving the family has: each of their links goes,
    /// and each pool file only they had follows `choice` (kept aside under
    /// `<members>/.kept/<user>/`, moved into the shared library, or sent to the trash). Files
    /// another member still has stay where they are. A file that cannot be kept aside or moved
    /// stays exactly as it was, linked and recorded for the member, and the first such failure
    /// is the answer once every other file is done, so nothing is left behind without a record.
    /// Each file is claimed in turn, so changes to other files go on meanwhile. Answers what
    /// became of the files.
    pub async fn release_member(&self, user: &str, choice: FilesChoice) -> Result<Released, PoolError> {
        let member = self.member_named(user)?;
        self.whole(Err(stopping()), move |pool| async move {
            pool.release_member_now(member.username, choice).await
        })
        .await
    }

    /// [`Self::release_member`] for the member named `name`, on the caller's task.
    async fn release_member_now(&self, name: String, choice: FilesChoice) -> Result<Released, PoolError> {
        let mut released = Released::default();
        let mut failure: Option<PoolError> = None;
        for listed in self.store.refs_of(&name) {
            let _claim = self.claims.claim(&[&listed.path]).await;
            // The record as it is now that this change holds the file.
            let Some(file) = self
                .store
                .get(&listed.path)
                .filter(|file| file.refs.iter().any(|r| eq_ignore_case(r, &name)))
            else {
                continue;
            };
            let only_theirs = file.refs.iter().all(|r| eq_ignore_case(r, &name));
            if !only_theirs {
                self.remove_link_claimed(&file.path, &name).await;
                {
                    let _records = self.records.lock();
                    self.store.remove_ref(&file.path, &name);
                }
                released.shared += 1;
                continue;
            }
            let there = {
                let path = file.path.clone();
                on_disk(move || Path::new(&path).is_file()).await.unwrap_or(false)
            };
            if !there {
                self.remove_link_claimed(&file.path, &name).await;
                {
                    let _records = self.records.lock();
                    self.store.remove(&file.path);
                }
                continue;
            }
            let outcome = match choice {
                FilesChoice::Keep => self
                    .keep_aside_claimed(&file.path, &name)
                    .await
                    .map(|()| released.kept += 1),
                FilesChoice::Promote => self
                    .promote_claimed(&file.path, Some(&name))
                    .await
                    .map(|_| released.promoted += 1),
                FilesChoice::Delete => {
                    self.remove_link_claimed(&file.path, &name).await;
                    {
                        let _records = self.records.lock();
                        self.store.remove_ref(&file.path, &name);
                    }
                    if self.gc_claimed(&file.path).await {
                        released.trashed += 1;
                    } else {
                        let path = file.path.clone();
                        if on_disk(move || Path::new(&path).exists()).await.unwrap_or(true) {
                            released.shared += 1;
                        }
                    }
                    Ok(())
                }
            };
            if let Err(e) = outcome {
                warn!("FAMILY {}'s file {} stays as it was: {e}", name, file.path);
                failure.get_or_insert(e);
            }
        }
        if let Some(e) = failure {
            return Err(e);
        }
        // Whatever is left of their folder is empty folders and covers.
        let root = PathBuf::from(self.member_root(&name));
        let _ = on_disk(move || prune_folder_tree(&root)).await;
        info!(
            "FAMILY released {name}'s files ({choice:?}): {} kept, {} promoted, {} trashed, {} still with others",
            released.kept, released.promoted, released.trashed, released.shared
        );
        Ok(released)
    }

    /// Moves a pool file only `name` had to `<members>/.kept/<name>/<layout>`, with its lyrics
    /// and cover, out of every library, and removes their link to it. Nothing changes when the
    /// song itself cannot be moved. For a change that holds the file's claim already.
    async fn keep_aside_claimed(&self, pool_path: &str, name: &str) -> Result<(), PoolError> {
        let places = self.places();
        let relative = places
            .relative(pool_path)
            .ok_or_else(|| PoolError::NotInPool(pool_path.to_string()))?;
        let target = format!("{}/{KEPT_FOLDER}/{name}/{relative}", places.members);
        let (path, name) = (pool_path.to_string(), name.to_string());
        on_disk(move || -> io::Result<()> {
            let target = if Path::new(&target).exists() {
                PathHelper::resolve_unique_path(&target)
            } else {
                target
            };
            let companions = companions(&path);
            move_pool_file(&places.staging, Path::new(&path), Path::new(&target))?;
            carry_companions(&places.staging, &companions, Path::new(&target), false);
            remove_member_link(&places, &name, &path, &companions);
            if let Some(folder) = Path::new(&path).parent() {
                remove_lonely_covers(folder);
                prune_empty(folder, Path::new(&places.pool));
            }
            Ok(())
        })
        .await??;
        {
            let _records = self.records.lock();
            self.store.remove(pool_path);
        }
        if let Err(e) = self.library.forget_mapping(pool_path).await {
            warn!("FAMILY could not forget {pool_path}'s mapping: {e}");
        }
        Ok(())
    }

    /// The index's song at `path` (in `library` when given). A member's link the index finds
    /// tells Octo that Navidrome shows links.
    async fn shown_at(&self, path: &str, library: Option<i64>) -> Option<IndexedSong> {
        let song = self
            .index
            .find_by_path(path)
            .filter(|song| library.is_none() || song.library_id == library)?;
        if library.is_some() {
            let path = path.to_string();
            if on_disk(move || is_symlink(Path::new(&path)))
                .await
                .unwrap_or(false)
            {
                self.confirm_links_seen();
            }
        }
        Some(song)
    }

    /// Waits until the library index finds a file at `path` (in `library_id` when given),
    /// reading the index again when it is older than the poll interval. None after
    /// [`PoolTiming::max_wait`].
    pub async fn wait_for_path(&self, path: &str, library_id: Option<i64>) -> Option<IndexedSong> {
        let started = tokio::time::Instant::now();
        loop {
            if let Some(song) = self.shown_at(path, library_id).await {
                return Some(song);
            }
            if started.elapsed() >= self.timing.max_wait {
                return None;
            }
            let poll = TimeDelta::from_std(self.timing.poll).unwrap_or(TimeDelta::seconds(15));
            let stale = self.index.refreshed_at().is_none_or(|at| at < Utc::now() - poll);
            if stale && self.index.refresh().await.is_ok() {
                continue;
            }
            tokio::time::sleep(self.timing.poll).await;
        }
    }

    /// Stars `user`'s copy at `path` once the index finds it, without holding anything up.
    pub fn star_when_indexed(&self, user: &str, path: &str) {
        let Some(pool) = self.arc() else { return };
        let (user, path) = (user.to_string(), path.to_string());
        tokio::spawn(async move {
            // A link is looked for in their own library; a shared song anywhere.
            let library = pool
                .pool_path_of_link(&user, &path)
                .and_then(|_| pool.family.member(&user))
                .and_then(|m| m.personal_library_id);
            if let Some(song) = pool.wait_for_path(&path, library).await {
                pool.stars.star(&user, &song.id).await;
            }
        });
    }

    /// Carries stars and family playlist entries over to the new ids, once the index finds
    /// the files where they are now.
    fn carry_over(&self, remaps: Vec<Remap>) {
        if remaps.is_empty() {
            return;
        }
        let Some(pool) = self.arc() else { return };
        tokio::spawn(async move { pool.finish_remaps(remaps).await });
    }

    async fn finish_remaps(&self, remaps: Vec<Remap>) {
        let paths: Vec<&str> = remaps.iter().map(|r| r.new_path.as_str()).collect();
        let mut done: HashSet<&str> = HashSet::new();
        for path in paths {
            if !done.insert(path) {
                continue;
            }
            let Some(song) = self.wait_for_path(path, None).await else {
                warn!("FAMILY {path} did not show up in the library; stars on its old id stay there");
                continue;
            };
            for remap in remaps.iter().filter(|r| r.new_path == path) {
                if remap.starred && !self.stars.star(&remap.user, &song.id).await {
                    warn!("FAMILY could not star {path} again for {}", remap.user);
                }
                if let Some(old) = &remap.old_id {
                    self.playlists.remap_library_id(old, &song.id, &remap.scope);
                }
            }
        }
    }
}

#[async_trait]
impl FamilyPlacement for FamilyPool {
    fn pool_root(&self) -> Option<String> {
        self.active().then(|| self.pool_root_path())
    }

    async fn placement_ready(&self) -> bool {
        if !self.active() {
            return true;
        }
        let marker = Path::new(&self.pool_root_path()).join(POOL_MARKER);
        let ready = on_disk(move || marker.is_file()).await.unwrap_or(false);
        if !ready {
            warn!(
                "FAMILY the pool folder {} has no {POOL_MARKER} file (its disk may not be mounted); a member's download waits until it does",
                self.pool_root_path()
            );
        }
        ready
    }

    fn is_pool_path(&self, path: &str) -> bool {
        self.active() && self.relative_in_pool(path).is_some()
    }

    async fn find_song(&self, provider: &str, external_id: &str, song: &Song) -> Option<PoolFile> {
        if !self.active() {
            return None;
        }
        let key = key_of(provider, external_id);
        self.find(Some(&key), song.isrc.as_deref(), &song.artist, &song.title)
            .await
    }

    fn made_from_lossy(&self, path: &str) -> bool {
        self.store
            .get(path)
            .is_some_and(|file| file.transcoded_from.is_some())
    }

    fn link_path_for(&self, pool_path: &str, user: &str) -> Option<String> {
        self.link_path(pool_path, user)
    }

    async fn for_owner(&self, path: &str) -> Option<String> {
        FamilyPool::for_owner(self, path).await
    }

    async fn placed(
        &self,
        path: &str,
        song: &Song,
        provider: &str,
        external_id: &str,
        members: &[String],
        personal: bool,
    ) -> Option<String> {
        let (path, song, provider, external_id, members) = (
            path.to_string(),
            song.clone(),
            provider.to_string(),
            external_id.to_string(),
            members.to_vec(),
        );
        self.whole(None, move |pool| async move {
            pool.placed_now(&path, &song, &provider, &external_id, &members, personal)
                .await
        })
        .await
    }

    async fn settle(
        &self,
        path: &str,
        members: &[String],
        wants_shared: bool,
        outside_key: Option<&str>,
    ) -> String {
        // With Family off nothing is in the pool and nobody gets a copy: the file stays put.
        if !self.active() {
            return path.to_string();
        }
        let (owned, members, outside_key) = (
            path.to_string(),
            members.to_vec(),
            outside_key.map(str::to_string),
        );
        self.whole(path.to_string(), move |pool| async move {
            pool.settle_now(&owned, &members, wants_shared, outside_key.as_deref())
                .await
        })
        .await
    }
}

impl FamilyPool {
    /// [`FamilyPlacement::placed`] on the caller's task.
    async fn placed_now(
        &self,
        path: &str,
        song: &Song,
        provider: &str,
        external_id: &str,
        members: &[String],
        personal: bool,
    ) -> Option<String> {
        // Each new pool file is a moment to let go of any that went missing by hand.
        self.sweep_dangling_now().await;
        let key = key_of(provider, external_id);
        let older = self
            .find(Some(&key), song.isrc.as_deref(), &song.artist, &song.title)
            .await
            .filter(|file| file.path != path);
        let older_path = older.as_ref().map(|file| file.path.clone());
        let mut claimed = vec![path];
        claimed.extend(older_path.as_deref());
        let _claim = self.claims.claim(&claimed).await;
        let size = {
            let path = path.to_string();
            on_disk(move || std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0))
                .await
                .unwrap_or(0)
        };
        let format = extension_of(path);
        let placed = PoolFile {
            path: path.to_string(),
            size,
            transcoded_from: song
                .transcoded_from
                .clone()
                .filter(|from| format == "flac" && !from.trim().is_empty()),
            format,
            outside_ids: vec![key],
            isrc: song.isrc.clone().filter(|i| !i.trim().is_empty()),
            title: song.title.clone(),
            artist: song.artist.clone(),
            album: song.album.clone(),
            refs: Vec::new(),
        };
        let bettered = older.filter(|older| better(&placed, older));
        self.record(placed);
        let mut first = None;
        for member in members {
            match self.link_claimed(path, member).await {
                Ok(link) => {
                    first.get_or_insert(link);
                }
                Err(e) => warn!("FAMILY could not give {member} {path}: {e}"),
            }
        }
        if let Some(older) = bettered {
            // A personal file is proven only for its own people: anyone else keeps their copy.
            let only = personal.then_some(members);
            self.repoint_claimed(&older.path, path, only).await;
        }
        first
    }

    /// [`FamilyPlacement::settle`] on the caller's task.
    async fn settle_now(
        &self,
        path: &str,
        members: &[String],
        wants_shared: bool,
        outside_key: Option<&str>,
    ) -> String {
        if !self.is_pool_path(path) {
            if self.active() && !members.is_empty() && self.is_shared_file(path) {
                for member in members {
                    if self.sees_shared_file(member, path) {
                        continue;
                    }
                    if let Err(e) = self.copy_shared_for(path, member, outside_key, None).await {
                        warn!("FAMILY could not give {member} a copy of {path}: {e}");
                    }
                }
            }
            return path.to_string();
        }
        let _claim = self.claims.claim(&[path]).await;
        // Members who joined after the file was placed are linked first, so a move keeps it in
        // the pool for any of them who cannot see the shared library.
        for member in members {
            if let Err(e) = self.link_claimed(path, member).await {
                warn!("FAMILY could not give {member} {path}: {e}");
            }
        }
        if !wants_shared {
            return path.to_string();
        }
        match self.promote_claimed(path, None).await {
            Ok(moved) => moved,
            Err(e) => {
                warn!("FAMILY could not move {path} into the shared library: {e}");
                path.to_string()
            }
        }
    }
}

#[async_trait]
impl MemberCopies for FamilyPool {
    async fn own_copy_of(&self, who: &str, key: &str, library_id: &str) -> Option<String> {
        if !self.active() {
            return None;
        }
        let member = self.member_named(who).ok()?;
        let mut library = self.index.library_of(library_id);
        if library.is_none() {
            // A song that only just landed may be newer than the index.
            if let Err(e) = self.index.refresh().await {
                warn!("FAMILY could not read the library index: {e}");
            }
            library = self.index.library_of(library_id);
        }
        if library.is_some() && library == member.personal_library_id {
            return None;
        }
        // Their own pool file of this song, wherever the download itself landed.
        if let Some(file) = self
            .store
            .find_by_outside_id(key)
            .filter(|file| file.refs.iter().any(|r| eq_ignore_case(r, &member.username)))
        {
            return self.link_path(&file.path, &member.username);
        }
        // A shared song they cannot see: their copy, found by its record, else where the pool
        // would put it now.
        let song = self.index.song(library_id)?;
        if !self.is_shared_file(&song.path) || sees_shared_library(&member, library) {
            return None;
        }
        if let Some(copy) = self.store.find_by_source(&song.path) {
            return self.link_path(&copy.path, &member.username);
        }
        let relative = self.pool_relative_of_shared(&song.path)?;
        let (pool_root, layout, shared) = (
            self.pool_root_path(),
            Layout::from_settings(&self.settings.current().subsonic),
            song.path.clone(),
        );
        let place = on_disk(move || {
            let tags = album_identity::read_layout_tags(Path::new(&shared));
            settle_relative(&pool_root, &relative, tags.as_ref(), &layout, POOL_BUDGET).ok()
        })
        .await
        .ok()
        .flatten()?;
        self.link_path(&place, &member.username)
    }

    async fn own_song_at(&self, who: &str, link: &str) -> Option<String> {
        let library = self.family.member(who)?.personal_library_id?;
        self.wait_for_path(link, Some(library)).await.map(|song| song.id)
    }
}

/// Whether `new` is a better copy than `old`: a real FLAC for a lossy file, or for a FLAC made
/// from lossy audio.
fn better(new: &PoolFile, old: &PoolFile) -> bool {
    new.is_real_flac() && !old.is_real_flac()
}

/// Whether `member` sees the shared library `library`. One Octo cannot name is seen only by a
/// member who sees every shared library.
pub fn sees_shared_library(member: &Member, library: Option<i64>) -> bool {
    match &member.shared_libraries {
        SharedLibraries::All => true,
        SharedLibraries::Ids(ids) => library.is_some_and(|id| ids.contains(&id)),
    }
}

/// The record of a pool file Octo knows nothing more about than where it is.
fn unrecorded_file(pool_path: &str, size: u64, refs: Vec<String>) -> PoolFile {
    PoolFile {
        path: pool_path.to_string(),
        size,
        format: extension_of(pool_path),
        outside_ids: Vec::new(),
        isrc: None,
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        refs,
        transcoded_from: None,
    }
}

/// Runs file work on a blocking thread, so a slow disk never holds up the async workers.
async fn on_disk<R, F>(work: F) -> io::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Waits `pause` before a copy starts (none outside tests).
fn wait_before_copy(pause: Duration) {
    if !pause.is_zero() {
        std::thread::sleep(pause);
    }
}

/// `path`'s place under `root`, when it is a file under it and never climbs out of it.
fn relative_under(root: &str, path: &str) -> Option<String> {
    let rest = path.strip_prefix(root)?.strip_prefix('/')?;
    (!rest.is_empty() && !rest.split('/').any(|part| part == "..")).then(|| rest.to_string())
}

/// Whether `path` is a link (not following it).
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// What became of a pool file on its way to the trash.
enum Trashing {
    /// Members' libraries still have it on disk, so it stays: these members, and its size.
    Held { holders: Vec<String>, size: u64 },
    /// It is in the trash, or was gone already.
    Gone,
    /// It could not be moved to the trash.
    Stayed,
}

/// Sends a pool file to the trash unless one of `names` (members) still has something at its
/// place in their library, then tidies its folder away when nothing else is in it.
fn trash_unless_linked(
    places: &Places,
    names: &[String],
    pool_path: &str,
    quarantine: &LibraryActionQuarantine,
) -> Trashing {
    let holders: Vec<String> = match places.relative(pool_path) {
        Some(relative) => names
            .iter()
            .filter(|name| std::fs::symlink_metadata(places.under(name, &relative)).is_ok())
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    if !holders.is_empty() {
        let size = std::fs::metadata(pool_path).map(|m| m.len()).unwrap_or(0);
        return Trashing::Held { holders, size };
    }
    if Path::new(pool_path).exists() {
        let size = std::fs::metadata(pool_path).map(|m| m.len() as i64).unwrap_or(0);
        let file = ResolvedSongFile {
            navidrome_id: String::new(),
            absolute_path: pool_path.to_string(),
            size_bytes: size,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            suffix: extension_of(pool_path),
            duration_seconds: None,
            source: PathSource::LocalMappings,
            album_artist: None,
        };
        let moved =
            quarantine.move_file_with_sidecars(&file, &places.members, LibraryAction::Delete, "family", true);
        if !moved.moved {
            warn!(
                "FAMILY could not move {pool_path} to the trash: {}",
                moved.error.unwrap_or_default()
            );
            return Trashing::Stayed;
        }
        info!("FAMILY nobody has {pool_path} any more; it went to the trash");
    }
    if let Some(folder) = Path::new(pool_path).parent() {
        remove_lonely_covers(folder);
        prune_empty(folder, Path::new(&places.pool));
    }
    quarantine.sweep(&places.members);
    Trashing::Gone
}

/// Makes `link` stand for `pool_path`: a relative link in link mode, or a copy (in copy mode,
/// and when the disk refuses the link). Nothing changes when something is there already. A
/// folder another change tidied away in the meantime is made again.
fn make_link(staging: &Path, mode: LinkMode, pool_path: &str, link: &str, relative: &str) -> io::Result<()> {
    let link_path = Path::new(link);
    for _ in 0..3 {
        if std::fs::symlink_metadata(link_path).is_ok() {
            return Ok(());
        }
        if let Some(parent) = link_path.parent() {
            make_dirs(parent)?;
        }
        let made = match mode {
            LinkMode::Symlink => match symlink(&relative_target(relative), link_path) {
                Ok(()) => Ok(()),
                Err(e) if matches!(e.kind(), io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound) => {
                    Err(e)
                }
                Err(e) => {
                    warn!("FAMILY could not link {link}: {e}; copying it instead");
                    copy_staged(staging, Path::new(pool_path), link_path)
                }
            },
            LinkMode::Copy => copy_staged(staging, Path::new(pool_path), link_path),
        };
        match made {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound && Path::new(pool_path).exists() => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(format!("the folder of {link} kept going away")))
}

/// Removes `name`'s link to a pool file, their links to its `companions` (read before the pool
/// file moved; never the folder's cover, which the album shares), and their album folder's cover
/// link and folders once nothing else is in them.
fn remove_member_link(places: &Places, name: &str, pool_path: &str, companions: &[String]) {
    let Some(relative) = places.relative(pool_path) else {
        return;
    };
    let link = places.under(name, &relative);
    let link_path = Path::new(&link);
    if std::fs::symlink_metadata(link_path).is_ok()
        && let Err(e) = std::fs::remove_file(link_path)
    {
        warn!("FAMILY could not remove {link}: {e}");
    }
    for companion in companions {
        if COVER_FILES
            .iter()
            .any(|cover| companion.to_lowercase().ends_with(&format!("/{cover}")))
        {
            continue;
        }
        if let Some(companion_relative) = places.relative(companion) {
            let _ = std::fs::remove_file(places.under(name, &companion_relative));
        }
    }
    if let Some(folder) = link_path.parent() {
        remove_lonely_covers(folder);
        prune_empty(folder, Path::new(&places.member_root(name)));
    }
}

/// Takes a song's `companions` along to `target`'s folder where nothing is there yet: covers
/// are always copied (the album folder keeps its own), lyrics files copied when `copy` and
/// moved otherwise.
fn carry_companions(staging: &Path, companions: &[String], target: &Path, copy: bool) {
    for companion in companions {
        let Some((destination, is_cover)) = companion_place(companion, target) else {
            continue;
        };
        if destination.exists() {
            continue;
        }
        let result = if is_cover || copy {
            std::fs::copy(companion, &destination).map(|_| ())
        } else {
            move_file(staging, Path::new(companion), &destination)
        };
        if let Err(e) = result {
            warn!("FAMILY could not move {companion} with its song: {e}");
        }
    }
}

/// Copies `from` to `to` (through a link at `from`, when it is one), written beside `to` first
/// and then put in its place, so a half-written file never stands there; a record of it in
/// `staging` while it is under way lets a recovery remove what a stop left. With `from` and `to`
/// the same link, the link becomes a real copy of what it points to.
fn copy_staged(staging: &Path, from: &Path, to: &Path) -> io::Result<()> {
    stage_copy(staging, from, to, Staged::Copy)
}

/// What a staged copy is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Staged {
    /// A copy: the file it is made from stays.
    Copy,
    /// A move whose caller follows the copy whatever the answer (a member's own copy, a song's
    /// lyrics): the file it is made from goes once the copy has its name, and one that cannot go
    /// yet goes with the next recovery, the move answered as done.
    Move,
    /// A move whose caller takes an error as nothing moved (a pool file moved, kept aside or
    /// promoted): one whose source cannot go, or whose name cannot be made to last, is taken back
    /// and answered as an error, and one a stop cut short is taken back by the next recovery.
    MoveOrNothing,
}

/// [`copy_staged`], and for a move, `from` goes once the copy's name lasts a power cut (its
/// folder flushed), as `staged_for` says ([`Staged`]).
fn stage_copy(staging: &Path, from: &Path, to: &Path, staged_for: Staged) -> io::Result<()> {
    let moving = staged_for != Staged::Copy;
    let (staged, mut record) = PLACEMENT.copy_under_way(
        staging,
        to,
        moving.then_some(from),
        staged_for == Staged::MoveOrNothing,
    )?;
    // Whole on the disk, and its record says so, before it takes the name.
    let whole = copy_file(from, &staged)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&staged)
                .and_then(|file| file.sync_all())
        })
        .and_then(|()| record.mark_whole());
    if let Err(e) = whole {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    #[cfg(test)]
    STAGED_COPIES.lock().push((
        to.to_path_buf(),
        to.exists(),
        staged.is_file(),
        marked_whole(&staging.join("replacing")),
    ));
    if let Err(e) = std::fs::rename(&staged, to) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    match staged_for {
        Staged::Copy => {
            let _ = record.settle(None, to);
        }
        Staged::Move => {
            if record.settle(Some(from), to).is_err() {
                let _ = record.keep_moved();
            }
        }
        Staged::MoveOrNothing => return record.settle_or_undo(from, to),
    }
    Ok(())
}

/// Whether the records folder `records` holds a record that marks its copy whole.
#[cfg(test)]
fn marked_whole(records: &Path) -> bool {
    std::fs::read_dir(records).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            std::fs::read(entry.path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|record| record["whole"] == serde_json::json!(true))
        })
    })
}

/// Copies a song to `to` ([`copy_staged`]), then copies its lyrics files and its folder's cover
/// beside it where nothing is there yet.
fn copy_with_companions(staging: &Path, from: &Path, to: &Path) -> io::Result<()> {
    copy_staged(staging, from, to)?;
    for companion in companions(&from.to_string_lossy()) {
        let Some((destination, _)) = companion_place(&companion, to) else {
            continue;
        };
        if destination.exists() {
            continue;
        }
        if let Err(e) = std::fs::copy(&companion, &destination) {
            warn!("FAMILY could not copy {companion} with its song: {e}");
        }
    }
    Ok(())
}

/// The lower-case extension without its dot.
fn extension_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// The link text that reaches `<members>/.pool/<relative>` from `<members>/<user>/<relative>`.
pub fn relative_target(relative: &str) -> String {
    let depth = relative.split('/').filter(|part| !part.is_empty()).count();
    format!("{}{POOL_FOLDER}/{relative}", "../".repeat(depth))
}

/// Files that go with a song: its own lyrics files and its folder's cover.
fn companions(song_path: &str) -> Vec<String> {
    let mut found = LibraryActionQuarantine::sidecars_of(song_path);
    if let Some(folder) = Path::new(song_path).parent()
        && let Ok(entries) = std::fs::read_dir(folder)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if COVER_FILES.iter().any(|cover| eq_ignore_case(cover, &name)) {
                found.push(entry.path().to_string_lossy().into_owned());
            }
        }
    }
    found
}

/// Where a song's `companion` goes beside the song at `target`, and whether it is a cover: a
/// cover keeps its name, and a lyrics file takes the song's name with its own extension, so the
/// two still pair when the song's name changed on the way.
fn companion_place(companion: &str, target: &Path) -> Option<(PathBuf, bool)> {
    let name = Path::new(companion).file_name()?;
    if COVER_FILES
        .iter()
        .any(|cover| eq_ignore_case(cover, &name.to_string_lossy()))
    {
        return Some((target.with_file_name(name), true));
    }
    let extension = Path::new(companion).extension()?;
    Some((target.with_extension(extension), false))
}

/// The cover files directly in `folder`.
fn covers_in(folder: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            COVER_FILES.iter().any(|cover| eq_ignore_case(cover, &name))
        })
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

/// `relative`, a song's path, with the extension of `sidecar` in place of its own.
fn with_extension_of(relative: &str, sidecar: &str) -> String {
    let name_start = relative.rfind('/').map_or(0, |slash| slash + 1);
    let stem = match relative.rfind('.') {
        Some(dot) if dot > name_start => &relative[..dot],
        _ => relative,
    };
    let extension = Path::new(sidecar)
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{stem}.{extension}")
}

/// The first entry in the way of a file at `root/relative`, which is at `root/own` now: a file
/// where a folder must be, another spelling of a folder that would have to be made (other
/// capitals, a character Windows refuses), or anything by the file's name or another spelling of
/// it. The entries on `own`'s own way are never in the way. None when the way is clear.
fn in_the_way(root: &str, relative: &str, own: &str) -> Option<String> {
    let parts: Vec<&str> = relative.split('/').collect();
    let own_parts: Vec<&str> = own.split('/').collect();
    let mut dir = PathBuf::from(root);
    for (at, part) in parts.iter().enumerate() {
        let last = at + 1 == parts.len();
        let own_here = (own_parts.len() > at && own_parts[..at] == parts[..at]).then(|| own_parts[at]);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return None;
        };
        let names: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                (Some(name.as_str()) != own_here).then(|| {
                    let is_dir = entry.path().is_dir();
                    (name, is_dir)
                })
            })
            .collect();
        if last {
            return names
                .into_iter()
                .find(|(name, _)| name == part || PathHelper::same_name(name, part, true))
                .map(|(name, _)| name);
        }
        if own_here == Some(*part) || names.iter().any(|(name, is_dir)| name == part && *is_dir) {
            dir.push(part);
            continue;
        }
        // The folder is made new, so nothing below it can be in the way.
        return names
            .into_iter()
            .find(|(name, _)| name == part || PathHelper::same_name(name, part, false))
            .map(|(name, _)| name);
    }
    None
}

/// Puts a member's `old_link` at `new_link` for the pool file now at `target` (`relative` in
/// the pool): a link is made again pointing at it, since its text named the old place; a real
/// copy is renamed; a missing one is made.
fn move_link(
    staging: &Path,
    mode: LinkMode,
    (old_link, new_link): (&str, &str),
    target: &str,
    relative: &str,
) -> io::Result<()> {
    match std::fs::symlink_metadata(old_link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if eq_ignore_case(old_link, new_link) {
                // One entry on a disk that ignores capitals: made again in its own place.
                std::fs::remove_file(old_link)?;
                make_link(staging, LinkMode::Symlink, target, new_link, relative)
            } else {
                make_link(staging, LinkMode::Symlink, target, new_link, relative)?;
                std::fs::remove_file(old_link)
            }
        }
        Ok(_) => move_file(staging, Path::new(old_link), Path::new(new_link)),
        Err(_) => make_link(staging, mode, target, new_link, relative),
    }
}

/// The library tidy's hold on pool files ([`FamilyPool::try_hold`]), let go of when the last
/// clone is dropped. Clones share it.
#[derive(Clone)]
pub struct PoolFilesHeld(Arc<HeldFiles>);

impl PoolFilesHeld {
    fn covers(&self, path: &str) -> bool {
        self.0.paths.iter().any(|held| held == path)
    }
}

struct HeldFiles {
    pool: Arc<FamilyPool>,
    paths: Vec<String>,
}

impl Drop for HeldFiles {
    fn drop(&mut self) {
        {
            let mut held = self.pool.claims.held.lock();
            for path in &self.paths {
                held.remove(path);
            }
        }
        self.pool.claims.freed.notify_waiters();
    }
}

/// One pool file's move for the File layout task, as its file work on a blocking thread reads
/// it. The relative paths are inside the pool, and the same inside each member's library.
#[derive(Debug, Clone)]
struct PoolMove {
    places: Places,
    old: String,
    new: String,
    old_relative: String,
    new_relative: String,
}

impl PoolMove {
    /// The file is at the new place and not at the old.
    fn done(&self) -> bool {
        !Path::new(&self.old).is_file() && Path::new(&self.new).is_file()
    }

    /// The song's own lyrics files that go with it, each as (where it is, where it goes), inside
    /// the pool. Once the move is done, the ones beside the new place.
    fn sidecars(&self) -> Vec<(String, String)> {
        let (from, to, done) = if self.done() {
            (&self.new, &self.old_relative, true)
        } else {
            (&self.old, &self.new_relative, false)
        };
        LibraryActionQuarantine::sidecars_of(from)
            .into_iter()
            .filter_map(|sidecar| {
                let here = self.places.relative(&sidecar)?;
                let there = with_extension_of(to, &sidecar);
                Some(if done { (there, here) } else { (here, there) })
            })
            .collect()
    }

    /// Why the file may not move now, in the words its row shows; None when the way is clear in
    /// the pool and in each of `holders`' libraries. A move already done is never held.
    fn held_by(&self, holders: &[String]) -> io::Result<Option<String>> {
        let done = self.done();
        if !done && !Path::new(&self.old).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not there", self.old),
            ));
        }
        let mut moves = vec![(self.old_relative.clone(), self.new_relative.clone())];
        moves.extend(self.sidecars());
        if !done {
            for (at, (from, to)) in moves.iter().enumerate() {
                if let Some(name) = in_the_way(&self.places.pool, to, from) {
                    return Ok(Some(if at == 0 {
                        format!("{name} is already there, so this song stays.")
                    } else {
                        format!("{name} is already there, so this song and its lyrics stay.")
                    }));
                }
            }
        }
        for holder in holders {
            let root = self.places.member_root(holder);
            for (from, to) in &moves {
                let (old_link, new_link) = (self.places.under(holder, from), self.places.under(holder, to));
                let moved = std::fs::symlink_metadata(&old_link).is_err()
                    && std::fs::symlink_metadata(&new_link).is_ok();
                if !moved && let Some(name) = in_the_way(&root, to, from) {
                    return Ok(Some(format!(
                        "{holder}'s library already has {name}, so this song stays."
                    )));
                }
            }
        }
        Ok(None)
    }

    /// Moves the pool file and its lyrics files, then copies the covers of the folder it leaves
    /// into a new folder that has none, and tidies that folder away once nothing else is in it.
    /// Answers the lyrics files now at the new place, as (old, new) inside the pool.
    fn move_in_pool(&self) -> io::Result<Vec<(String, String)>> {
        let sidecars = self.sidecars();
        if self.done() {
            return Ok(sidecars);
        }
        let leaving = Path::new(&self.old).parent().map(covers_in).unwrap_or_default();
        move_pool_file(&self.places.staging, Path::new(&self.old), Path::new(&self.new))?;
        let mut moved = Vec::new();
        for (from, to) in sidecars {
            let (here, there) = (
                format!("{}/{from}", self.places.pool),
                format!("{}/{to}", self.places.pool),
            );
            if Path::new(&there).exists() {
                continue;
            }
            match move_file(&self.places.staging, Path::new(&here), Path::new(&there)) {
                Ok(()) => moved.push((from, to)),
                Err(e) => warn!("FAMILY could not move {here} with its song: {e}"),
            }
        }
        if let Some(folder) = Path::new(&self.new).parent()
            && covers_in(folder).is_empty()
        {
            for cover in &leaving {
                let Some(name) = Path::new(cover).file_name() else {
                    continue;
                };
                if let Err(e) = std::fs::copy(cover, folder.join(name)) {
                    warn!("FAMILY could not copy {cover} with its album: {e}");
                }
            }
        }
        if let Some(folder) = Path::new(&self.old).parent() {
            remove_lonely_covers(folder);
            prune_empty(folder, Path::new(&self.places.pool));
        }
        Ok(moved)
    }

    /// Makes `name`'s link at the new place, pointing at the moved file, with links to its moved
    /// lyrics files (`sidecars`, inside the pool) and its new folder's covers, then tidies away
    /// the folder the old link leaves once nothing else is in it.
    fn relink(&self, mode: LinkMode, name: &str, sidecars: &[(String, String)]) {
        let mut moves = vec![(self.old_relative.clone(), self.new_relative.clone())];
        moves.extend(sidecars.iter().cloned());
        for (from, to) in &moves {
            let (old_link, new_link) = (self.places.under(name, from), self.places.under(name, to));
            let target = format!("{}/{to}", self.places.pool);
            let links = (old_link.as_str(), new_link.as_str());
            if let Err(e) = move_link(&self.places.staging, mode, links, &target, to) {
                warn!("FAMILY could not move {name}'s {old_link} to {new_link}: {e}");
            }
        }
        if let Some(folder) = Path::new(&self.new).parent() {
            for cover in covers_in(folder) {
                let Some(relative) = self.places.relative(&cover) else {
                    continue;
                };
                let link = self.places.under(name, &relative);
                if let Err(e) = make_link(&self.places.staging, mode, &cover, &link, &relative) {
                    warn!("FAMILY could not give {name} {cover}: {e}");
                }
            }
        }
        let old_link = self.places.under(name, &self.old_relative);
        if let Some(folder) = Path::new(&old_link).parent() {
            remove_lonely_covers(folder);
            prune_empty(folder, Path::new(&self.places.member_root(name)));
        }
    }
}

/// What a song's own tags say its kind is, as placement decides it: a compilation by its flag,
/// its type or a Various Artists album artist; a single by its type or having no album.
fn kind_of(tags: &LayoutTags) -> LayoutKind {
    let various = tags
        .album_artist
        .as_deref()
        .is_some_and(|artist| album_identity::artist_key(artist) == album_identity::VARIOUS_ARTISTS);
    LayoutKind::of(
        tags.release_type.as_deref(),
        tags.compilation || various,
        tags.album.as_deref().is_none_or(|album| album.trim().is_empty()),
    )
}

/// How a path a layout made reads: where its album folder is, which folders an artist alone
/// names, and what the album folder's name shows.
struct Shape {
    album_at: Option<usize>,
    artist_level: Vec<bool>,
    shown: Shown,
}

/// The shape of a path `folders` deep: the template `layout` files a song of `kind` by when it
/// is that deep, else the built-in layout of that depth. None when neither is.
fn shape_of(layout: &Layout, kind: LayoutKind, folders: usize, extension: &str) -> Option<Shape> {
    let presets = [
        FolderStructure::Organized,
        FolderStructure::ByArtist,
        FolderStructure::Flat,
    ]
    .map(Layout::preset);
    let template = std::iter::once(layout.template(kind))
        .chain(presets.iter().map(|preset| preset.template(LayoutKind::Album)))
        .find(|template| template.folder_count() == folders)?;
    // Every name filled, so every folder of the template renders and says what names it.
    let probe = LayoutValues {
        artist: "a".into(),
        track_artist: "a".into(),
        album_artist: Some("a".into()),
        album: "b".into(),
        title: "c".into(),
        track: Some(1),
        disc: Some(1),
        disc_count: Some(2),
        multi_disc: true,
        year: Some(2000),
        album_type: Some("Album".into()),
        ..LayoutValues::default()
    };
    let mut artist_level = vec![false; folders];
    for name in template.render(&probe, extension, PATH_BUDGET) {
        if let Some(level) = artist_level.get_mut(name.segment) {
            *level = name.artist_level;
        }
    }
    let (year, album_type) = template.album_folder_shows();
    Some(Shape {
        album_at: template.album_folder_segment(),
        artist_level,
        shown: Shown { year, album_type },
    })
}

/// Where a file's own tags file it, before the choice a loose album leaves; or, for a file whose
/// tags cannot name the song, the path its own names settle to.
enum Filing {
    Filed(Filed),
    Path(String),
}

/// Where a song now at `relative` (a path the pool's layout made, or an older rule's) goes
/// under `root`: each name cleaned as the layout cleans one, then settled as a new song is, so it
/// joins the folder already there by that name or another spelling of it, and the folder that
/// already holds its album or its disc, whatever that is called. `tags` are the song's own. A
/// file already there by the same name or another spelling of it stays, and this one takes the
/// next free name.
fn settle_relative(
    root: &str,
    relative: &str,
    tags: Option<&LayoutTags>,
    layout: &Layout,
    budget: usize,
) -> io::Result<String> {
    let parts: Vec<&str> = relative.split('/').filter(|part| !part.is_empty()).collect();
    let Some((file, folders)) = parts.split_last() else {
        return Err(io::Error::other(format!("{relative} has no file name")));
    };
    let (stem, extension) = match file.rfind('.') {
        Some(dot) if dot > 0 => (&file[..dot], &file[dot..]),
        _ => (*file, ""),
    };
    let shape = tags.and_then(|tags| shape_of(layout, kind_of(tags), folders.len(), extension));
    let artist_level = |at: usize| {
        shape
            .as_ref()
            .and_then(|shape| shape.artist_level.get(at).copied())
            .unwrap_or(false)
    };
    let mut names: Vec<RenderedName> = folders
        .iter()
        .enumerate()
        .map(|(at, folder)| {
            let name = PathHelper::safe_name(&PathHelper::sanitize_folder_name(folder), None);
            RenderedName {
                uncut: name.clone(),
                name,
                segment: at,
                artist_level: artist_level(at),
                cap: budget,
                cut_caps: layout.cut_caps().to_vec(),
            }
        })
        .collect();
    let stem = PathHelper::safe_name(&PathHelper::clean_name(stem), Some(extension));
    names.push(RenderedName {
        uncut: stem.clone(),
        name: stem,
        segment: folders.len(),
        artist_level: false,
        cap: budget,
        cut_caps: layout.cut_caps().to_vec(),
    });

    let identity = tags.and_then(|tags| AlbumIdentity::of_files(std::slice::from_ref(tags)));
    let album_at = shape.as_ref().and_then(|shape| shape.album_at);
    let shown = shape.as_ref().map(|shape| shape.shown).unwrap_or_default();
    // A folder of another name under a parent no artist names is the album's only on a positive
    // sign.
    let positive = album_at
        .filter(|at| *at == 0 || !artist_level(at - 1))
        .and(tags)
        .map(LayoutTags::primary_credit);
    let find_album = |parent: &str| -> Option<String> {
        album_identity::find_album_folder(Path::new(parent), identity.as_ref()?, shown, positive.as_deref())
    };
    let disc = tags.and_then(|tags| tags.disc);
    let find_disc = |album_dir: &str| -> Option<String> {
        let disc = disc?;
        album_identity::disc_folders(Path::new(album_dir))
            .into_iter()
            .find(|(_, found)| *found == disc)
            .map(|(name, _)| name)
    };
    let hooks = SettleHooks {
        album: album_at.filter(|_| identity.is_some()).map(|at| AlbumHook {
            at,
            find: &find_album,
        }),
        disc: album_at
            .map(|at| at + 1)
            .filter(|at| *at < folders.len() && disc.is_some())
            .map(|at| AlbumHook { at, find: &find_disc }),
        fit: None,
    };
    let settled = PathHelper::settle(root, &names, extension, budget, hooks)?;
    Ok(match settled.taken {
        Some(_) => PathHelper::resolve_unique_path(&settled.path),
        None => settled.path,
    })
}

/// Removes cover files (or links) from a folder that holds nothing else.
fn remove_lonely_covers(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    let names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    let only_covers = names.iter().all(|path| {
        path.file_name().is_some_and(|name| {
            COVER_FILES
                .iter()
                .any(|cover| eq_ignore_case(cover, &name.to_string_lossy()))
        })
    });
    if only_covers {
        for path in names {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Removes empty folders from `folder` up to (not including) `stop`.
fn prune_empty(folder: &Path, stop: &Path) {
    let mut current = Some(folder.to_path_buf());
    while let Some(dir) = current {
        if dir == stop || !dir.starts_with(stop) {
            return;
        }
        if std::fs::remove_dir(&dir).is_err() {
            return;
        }
        current = dir.parent().map(Path::to_path_buf);
    }
}

/// Removes every empty folder under `root` (and `root` itself when it ends up empty), and covers
/// left alone in a folder. Files are never touched otherwise.
fn prune_folder_tree(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            prune_folder_tree(&path);
        }
    }
    remove_lonely_covers(root);
    let _ = std::fs::remove_dir(root);
}

/// Creates a folder and its parents, readable and writable by the group (Navidrome often runs
/// as another user in the same group).
fn make_dirs(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o775);
    }
    builder.create(path)
}

/// Moves a file, copying it across disks, for a caller that follows it whatever the answer (a
/// member's own copy, a song's lyrics): [`Staged::Move`].
fn move_file(staging: &Path, from: &Path, to: &Path) -> io::Result<()> {
    move_with(staging, from, to, Staged::Move, |from, to| {
        std::fs::rename(from, to)
    })
}

/// Moves a pool file, copying it across disks, for a caller that takes an error as nothing
/// moved (a pool file moved, kept aside or promoted): [`Staged::MoveOrNothing`].
fn move_pool_file(staging: &Path, from: &Path, to: &Path) -> io::Result<()> {
    move_with(staging, from, to, Staged::MoveOrNothing, |from, to| {
        std::fs::rename(from, to)
    })
}

/// A move by `rename`: a rename, else a copy that takes `to`'s name only once it is whole
/// ([`stage_copy`] as `staged_for` says, with its record in `staging`), so a stop never leaves
/// part of a file under the song's name; `from` goes last. For [`Staged::MoveOrNothing`], an
/// error means the file is still at `from` alone.
fn move_with(
    staging: &Path,
    from: &Path,
    to: &Path,
    staged_for: Staged,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        make_dirs(parent)?;
    }
    if rename(from, to).is_ok() {
        return Ok(());
    }
    stage_copy(staging, from, to, staged_for)
}

/// Each staged copy as it was about to take its name: the name, whether a file already stood
/// there, whether the copy stood beside it, and whether a record in the staging folder marked a
/// copy under way whole.
#[cfg(test)]
pub(crate) static STAGED_COPIES: std::sync::LazyLock<Mutex<Vec<(PathBuf, bool, bool, bool)>>> =
    std::sync::LazyLock::new(Mutex::default);

/// Copies a file (through a link at `from`, when it is one), checking the copy's size.
fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        make_dirs(parent)?;
    }
    let length = std::fs::metadata(from)?.len();
    std::fs::copy(from, to)?;
    if std::fs::metadata(to)?.len() != length {
        let _ = std::fs::remove_file(to);
        return Err(io::Error::other("the copy did not match the original's size"));
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &str, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &str, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "links need a unix host",
    ))
}

/// Makes a file in `root`, links to it, reads through the link, and removes both.
fn probe(root: &Path) -> bool {
    if make_dirs(root).is_err() {
        return false;
    }
    let name = format!(".octo-link-probe-{}", hex::encode(rand::random::<[u8; 6]>()));
    let file = root.join(&name);
    let link = root.join(format!("{name}.link"));
    let works = std::fs::write(&file, b"octo").is_ok()
        && symlink(&name, &link).is_ok()
        && std::fs::read(&link).is_ok_and(|read| read == b"octo");
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&file);
    works
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pool_property_tests.rs"]
mod property_tests;
