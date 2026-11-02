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
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::common::{Clock, PathHelper, SongIdentity};
use octo_core::models::domain::Song;
use octo_core::settings::{LibraryAction, SettingsStore};
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
use crate::services::library::{LibraryActionQuarantine, PathSource, ResolvedSongFile};
use crate::services::local::ILocalLibraryService;

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

impl Claims {
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
/// on a blocking thread.
#[derive(Debug, Clone)]
struct Places {
    members: String,
    pool: String,
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
        Places { members, pool }
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
        let (path, pause) = (PathBuf::from(link), self.copy_pause());
        let copied = on_disk(move || -> io::Result<Option<u64>> {
            if !is_symlink(&path) {
                return Ok(None);
            }
            wait_before_copy(pause);
            copy_staged(&path, &path)?;
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
            let pause = self.copy_pause();
            replaced += on_disk(move || {
                links
                    .iter()
                    .filter(|link| is_symlink(link))
                    .filter(|link| {
                        wait_before_copy(pause);
                        match copy_staged(link, link) {
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
                make_link(mode, &path, &link, &relative)?;
                for companion in companions(&path) {
                    let Some(companion_relative) = places.relative(&companion) else {
                        continue;
                    };
                    let companion_link = places.under(&name, &companion_relative);
                    if let Err(e) = make_link(mode, &companion, &companion_link, &companion_relative) {
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
        let target = on_disk(move || -> io::Result<String> {
            let mut target = format!("{shared_root}/{relative}");
            if Path::new(&target).exists() {
                target = PathHelper::resolve_unique_path(&target);
            }
            let companions = companions(&path);
            wait_before_copy(pause);
            if stays_pooled {
                copy_staged(Path::new(&path), Path::new(&target))?;
            } else {
                move_file(Path::new(&path), Path::new(&target))?;
            }
            carry_companions(&companions, Path::new(&target), stays_pooled);
            for name in &seer_names {
                remove_member_link(&places, name, &path, &companions);
            }
            if !stays_pooled && let Some(folder) = Path::new(&path).parent() {
                remove_lonely_covers(folder);
                prune_empty(folder, Path::new(&places.pool));
            }
            Ok(target)
        })
        .await??;
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

    /// A shared file's place in the pool: its path under the shared folder, or its last three
    /// parts (`Artist/Album/file`) when it is somewhere else.
    fn pool_place_of_shared(&self, shared_path: &str) -> Option<String> {
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
        plain.then(|| format!("{}/{relative}", self.pool_root_path()))
    }

    /// Gives `user`, who cannot see the shared library a shared file is in, a copy they can
    /// play through the pool: the file, with its lyrics and its folder's cover, is copied once
    /// into the pool at its place in the shared layout, recorded as a copy of `song`, and
    /// linked into their library like any pool song. Another member who needs it gets a link
    /// to that same pool file. The copy is written on a blocking thread while only that pool
    /// file is claimed, so other changes to the pool go on beside it. Answers their link.
    pub async fn copy_shared_for(
        &self,
        shared_path: &str,
        user: &str,
        outside_key: Option<&str>,
        song: Option<&Song>,
    ) -> Result<String, PoolError> {
        let member = self.member_named(user)?;
        let target = self
            .pool_place_of_shared(shared_path)
            .ok_or_else(|| PoolError::NotInPool(shared_path.to_string()))?;
        let (shared_path, outside_key, song) = (
            shared_path.to_string(),
            outside_key.map(str::to_string),
            song.cloned(),
        );
        self.whole(Err(stopping()), move |pool| async move {
            let _claim = pool.claims.claim(&[&target]).await;
            let linked = pool
                .copy_shared_claimed(
                    &shared_path,
                    &member,
                    &target,
                    outside_key.as_deref(),
                    song.as_ref(),
                )
                .await;
            // A copy nobody could be given is not kept.
            if linked.is_err() && pool.store.get(&target).is_some_and(|file| file.refs.is_empty()) {
                pool.gc_claimed(&target).await;
            }
            linked
        })
        .await
    }

    /// [`Self::copy_shared_for`] for a change that holds the claim on `target`, the copy's
    /// place in the pool.
    async fn copy_shared_claimed(
        &self,
        shared_path: &str,
        member: &Member,
        target: &str,
        outside_key: Option<&str>,
        song: Option<&Song>,
    ) -> Result<String, PoolError> {
        let (from, to, pause) = (
            PathBuf::from(shared_path),
            PathBuf::from(target),
            self.copy_pause(),
        );
        let (copied, size) = on_disk(move || -> io::Result<(bool, u64)> {
            if !from.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} is not there", from.display()),
                ));
            }
            let copied = !to.is_file();
            if copied {
                wait_before_copy(pause);
                copy_with_companions(&from, &to)?;
            }
            Ok((copied, std::fs::metadata(&to).map(|m| m.len()).unwrap_or(0)))
        })
        .await??;
        if copied {
            info!(
                "FAMILY {shared_path} was copied into the pool as {target} for {}, who cannot see its shared library",
                member.username
            );
        }
        {
            let _records = self.records.lock();
            self.record_shared_copy(target, size, outside_key, song);
        }
        self.link_claimed(target, &member.username).await
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
            move_file(Path::new(&path), Path::new(&target))?;
            carry_companions(&companions, Path::new(&target), false);
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
        // A shared song they cannot see: their copy goes where the pool keeps it.
        let song = self.index.song(library_id)?;
        if !self.is_shared_file(&song.path) || sees_shared_library(&member, library) {
            return None;
        }
        let place = self.pool_place_of_shared(&song.path)?;
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
fn make_link(mode: LinkMode, pool_path: &str, link: &str, relative: &str) -> io::Result<()> {
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
                    copy_staged(Path::new(pool_path), link_path)
                }
            },
            LinkMode::Copy => copy_staged(Path::new(pool_path), link_path),
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
fn carry_companions(companions: &[String], target: &Path, copy: bool) {
    for companion in companions {
        let Some(name) = Path::new(companion).file_name() else {
            continue;
        };
        let destination = target.with_file_name(name);
        if destination.exists() {
            continue;
        }
        let is_cover = COVER_FILES
            .iter()
            .any(|cover| eq_ignore_case(cover, &name.to_string_lossy()));
        let result = if is_cover || copy {
            std::fs::copy(companion, &destination).map(|_| ())
        } else {
            move_file(Path::new(companion), &destination)
        };
        if let Err(e) = result {
            warn!("FAMILY could not move {companion} with its song: {e}");
        }
    }
}

/// Copies `from` to `to` (through a link at `from`, when it is one), written beside `to` first
/// and then put in its place, so a half-written file never stands there. With `from` and `to`
/// the same link, the link becomes a real copy of what it points to.
fn copy_staged(from: &Path, to: &Path) -> io::Result<()> {
    let mut name = to.as_os_str().to_owned();
    name.push(".octo-copy");
    let staged = PathBuf::from(name);
    copy_file(from, &staged)?;
    if let Err(e) = std::fs::rename(&staged, to) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    Ok(())
}

/// Copies a song to `to` ([`copy_staged`]), then copies its lyrics files and its folder's cover
/// beside it where nothing is there yet.
fn copy_with_companions(from: &Path, to: &Path) -> io::Result<()> {
    copy_staged(from, to)?;
    for companion in companions(&from.to_string_lossy()) {
        let Some(file_name) = Path::new(&companion).file_name() else {
            continue;
        };
        let destination = to.with_file_name(file_name);
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

/// Moves a file, copying it across disks.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        make_dirs(parent)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_file(from, to)?;
    std::fs::remove_file(from)
}

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
