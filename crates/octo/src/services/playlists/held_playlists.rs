//! Playlists that hold songs found online.
//!
//! A Navidrome playlist that gets a streamed song keeps its id: Octo holds the whole list beside
//! it (`octo-playlists/<id>.json`) and keeps Navidrome's copy of the library songs in the same
//! order. When nothing streams and no write is pending, Octo forgets the playlist and Navidrome
//! alone holds it, under the same id.
//!
//! - One writer at a time per playlist: [`HeldPlaylists::lock`] is a tokio mutex per id in a map
//!   of weak handles (dead ones swept each time a lock is made), taken within
//!   [`HeldTiming::lock_wait`]. Plain playlists take the same lock for every Octo change that
//!   reads before it writes, so a conversion and a plain change never interleave.
//! - A route's locked work runs in a spawned task the handler awaits, so a dropped request never
//!   stops a change halfway; the deadline applies inside the task, so a task that missed it never
//!   runs its change later.
//! - Lock order: playlist lock, then the store's mutex. The store's mutex guards copies and swaps
//!   only: no await, and no registry, seed index, caps or Navidrome call while it is held (the
//!   accessors of those debug-assert it).
//! - Saves: an operation writes its new `Replaced` record first (carrying the main file's new
//!   revision), then the main file, the commit point, then the delete of a record it cleared. A
//!   failure before the commit point puts memory, the kept ids and the counters back before
//!   anyone is answered; a failure after it is written by the next save or the flush pass.
//! - A mirror write saves `Pending` before its first Navidrome call and its progress before each
//!   call, writes once, then merges with what Navidrome holds: what another writer did wins.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use futures::StreamExt;
use futures::future::BoxFuture;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::common::{Clock, SongIdentity};
use octo_core::models::domain::Song;
use parking_lot::{Mutex, RwLock};
use similar::{Algorithm, DiffOp, capture_diff_slices};
use tokio::sync::{Mutex as TokioMutex, Notify, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::binder::{self, Binder};
use super::caps::{CapRefusal, PlaylistCaps};
use super::entries::{self, BuildContext, Built};
use super::held_store::{
    Creating, HeldEntry, HeldPlaylist, HeldStore, PendingWrite, PlaylistState, ReplacedRecord, SaveError,
    WritePhase, kept_ids, seeds, store_mutex_held,
};
use super::merge::{self, RawRow};
use super::same_take::{phone_match, same_take};
use super::seed_index::{SeedIndex, held_key, replaced_key};
use crate::services::common::acquisition_tracker::{AcquisitionEnd, AcquisitionTracker};
use crate::services::family::family_playlist_store::FamilyPlaylistStore;
use crate::services::family::library_index::ScopeResolver;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::navidrome_playlist_api::{PlaylistInfo, SongFacts, TrackFacts};
use crate::services::library::{NavidromePlaylistApi, NavidromeSongPathResolver};
use crate::services::local::{DownloadHistoryService, ILocalLibraryService, ParsedExternalId};
use crate::services::soulseek::ExternalIdRegistry;

pub const NOT_OWNER: &str = "Only the playlist's owner can change it.";
pub const MADE_BY_RULES: &str = "This playlist is made by Navidrome's rules, so songs can't be added to it.";
pub const UPKEEP_REFUSED: &str = "Songs found online can't go in Octo's library upkeep playlists.";
pub const NOT_A_SONG: &str = "Only songs can go in a playlist.";
pub const BUSY: &str = "This playlist is busy. Try again in a moment.";
pub const SAVE_FAILED: &str = "Octo couldn't save the playlist, so nothing was changed.";
pub const CHANGED: &str = "The playlist changed. Read it again.";
pub const PERSON_CAP: &str = "Your playlists hold as many songs found online as Octo keeps.";
pub const SERVER_CAP: &str = "This server holds as many songs found online in playlists as Octo keeps.";
pub const ONE_BY_ONE: &str = "Only songs found online can be added one by one.";
pub const NOT_FOUND: &str = "Playlist not found";

/// How many tracks one Navidrome remove call takes, from the end.
const REMOVE_CHUNK: usize = 100;
/// How many tracks one Navidrome add call takes.
const ADD_CHUNK: usize = 500;
/// How many songs are asked about at once.
const AT_ONCE: usize = 8;

/// Why a held-playlist operation was refused. Subsonic routes answer [`HeldError::code`] and
/// [`HeldError::message`] (with `octoConflict: true` when [`HeldError::conflict`]); native routes
/// answer [`HeldError::http_status`] with `{"error": message}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldError {
    /// The caller is not the playlist's owner.
    NotOwner,
    /// A smart or synced playlist.
    MadeByRules,
    /// An outside song sent to one of the caller's library upkeep playlists.
    Upkeep,
    /// An album, an artist or anything else that is not a song.
    NotSong,
    /// The lock's deadline passed, or Navidrome could not tell ("not now", or no answer).
    Busy,
    /// A save before the commit point failed; nothing was changed.
    SaveFailed,
    /// `octoIfStamp` is not the playlist's current `changed`.
    Changed,
    /// The person's cap of songs found online.
    PersonCap,
    /// The server's cap of songs found online.
    ServerCap,
    /// Navidrome answers that there is no such playlist.
    NotFound,
}

impl HeldError {
    pub fn code(self) -> i32 {
        match self {
            HeldError::NotOwner | HeldError::MadeByRules => 50,
            HeldError::NotSong => 10,
            HeldError::NotFound => 70,
            _ => 0,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            HeldError::NotOwner => NOT_OWNER,
            HeldError::MadeByRules => MADE_BY_RULES,
            HeldError::Upkeep => UPKEEP_REFUSED,
            HeldError::NotSong => NOT_A_SONG,
            HeldError::Busy => BUSY,
            HeldError::SaveFailed => SAVE_FAILED,
            HeldError::Changed => CHANGED,
            HeldError::PersonCap => PERSON_CAP,
            HeldError::ServerCap => SERVER_CAP,
            HeldError::NotFound => NOT_FOUND,
        }
    }

    /// Whether the answer carries `octoConflict: true`.
    pub fn conflict(self) -> bool {
        self == HeldError::Changed
    }

    /// The status a native route answers with.
    pub fn http_status(self) -> u16 {
        match self {
            HeldError::Busy => 503,
            HeldError::NotOwner => 403,
            HeldError::NotFound => 404,
            HeldError::SaveFailed => 500,
            _ => 400,
        }
    }

    fn from_cap(refusal: CapRefusal) -> Self {
        match refusal {
            CapRefusal::Person => HeldError::PersonCap,
            CapRefusal::Server => HeldError::ServerCap,
        }
    }
}

/// Who is asking: the Navidrome username Navidrome's answer to the caller named, and the
/// request's own `u` and `c` (a `Replaced` record remembers those two).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeldCaller {
    pub username: String,
    pub user: String,
    pub client: String,
}

impl HeldCaller {
    fn made(&self, record: &ReplacedRecord) -> bool {
        record.by_user == self.user && record.by_client == self.client
    }
}

/// What a relayed Navidrome call answered, to pass back to the client unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAnswer {
    pub ok: bool,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

/// A relay of the caller's own request to Navidrome, run under the playlist's lock: `None` when
/// Navidrome did not answer.
pub type Relay = BoxFuture<'static, Option<RelayAnswer>>;

/// The list a caller is served, worked out by the route under the playlist's lock.
#[async_trait]
pub trait ServedView: Send + Sync {
    /// For `entries` as they stand, the index of each entry the caller's answer lists, in order
    /// (after the answer rules, so a kid's explicit songs are not there). `None` when it could not
    /// be worked out (Navidrome did not answer).
    async fn served(&self, entries: &[HeldEntry]) -> Option<Vec<usize>>;

    /// A plain playlist's `changed` as the caller's relayed `getPlaylist` says it, for the stamp
    /// check of a replace: `None` when Navidrome did not answer, `Some(None)` when it named none.
    async fn changed(&self) -> Option<Option<DateTime<Utc>>> {
        Some(None)
    }
}

/// One playlist in an admin listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPlaylist {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub created: Option<DateTime<Utc>>,
}

/// What the held service asks Navidrome, as the admin.
#[async_trait]
pub trait HeldNavidrome: Send + Sync {
    /// `Ok(None)` when Navidrome answers there is no such playlist.
    async fn playlist_info(&self, id: &str) -> Result<Option<PlaylistInfo>, String>;
    /// Every track in order, missing files included; `Ok(None)` when they could not be read.
    async fn read_track_facts(&self, id: &str) -> Result<Option<Vec<TrackFacts>>, String>;
    /// `Ok(None)` when Navidrome answers it has no such song.
    async fn song_facts(&self, id: &str) -> Result<Option<SongFacts>, String>;
    async fn remove_positions(&self, id: &str, positions: &[String]) -> Result<bool, String>;
    async fn add_tracks(&self, id: &str, songs: &[String]) -> Result<bool, String>;
    /// Every playlist, or `None` when the list could not be read (which also proves the token).
    async fn list_playlists(&self) -> Option<Vec<ListedPlaylist>>;
}

/// Navidrome's native API as the admin.
pub struct AdminNavidrome {
    pub api: Arc<NavidromePlaylistApi>,
}

#[async_trait]
impl HeldNavidrome for AdminNavidrome {
    async fn playlist_info(&self, id: &str) -> Result<Option<PlaylistInfo>, String> {
        self.api.playlist_info(id).await
    }

    async fn read_track_facts(&self, id: &str) -> Result<Option<Vec<TrackFacts>>, String> {
        self.api.read_track_facts(id).await
    }

    async fn song_facts(&self, id: &str) -> Result<Option<SongFacts>, String> {
        self.api.song_facts(id).await
    }

    async fn remove_positions(&self, id: &str, positions: &[String]) -> Result<bool, String> {
        self.api.remove_positions(id, positions).await
    }

    async fn add_tracks(&self, id: &str, songs: &[String]) -> Result<bool, String> {
        self.api.add_tracks(id, songs).await
    }

    async fn list_playlists(&self) -> Option<Vec<ListedPlaylist>> {
        self.api.list_playlist_kinds().await.map(|kinds| {
            kinds
                .into_iter()
                .map(|kind| ListedPlaylist {
                    id: kind.id,
                    name: kind.name,
                    owner: kind.owner,
                    created: None,
                })
                .collect()
        })
    }
}

/// Whether a playlist's owner can read a library song.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readable {
    Yes,
    No,
    /// The library index has not been read yet: asked again later.
    NotNow,
}

/// The readable-library rule: with no personal libraries everyone reads everything; otherwise a
/// song in a member's own library is readable only by that member, a song in any other library
/// by everyone, and a song whose library is not known yet only once the index is ready (an
/// unknown library counts as shared).
pub fn readable_by(
    owner: &str,
    personal: &[(i64, String)],
    library: Option<i64>,
    index_ready: bool,
) -> Readable {
    if personal.is_empty() {
        return Readable::Yes;
    }
    let Some(library) = library else {
        return if index_ready {
            Readable::Yes
        } else {
            Readable::NotNow
        };
    };
    match personal.iter().find(|(id, _)| *id == library) {
        None => Readable::Yes,
        Some((_, member)) if eq_ignore_case(member, owner) => Readable::Yes,
        Some(_) => Readable::No,
    }
}

/// What the held service reads from the library and the catalog.
#[async_trait]
pub trait HeldLibrary: Send + Sync {
    /// The file of the owner's own version of an outside song: a version kept for them, else one
    /// kept for nobody in particular; never another person's.
    async fn own_version_path(&self, owner: &str, provider: &str, external_id: &str) -> Option<String>;
    /// The Navidrome id of the file at `path`.
    async fn library_id_at(&self, artist: &str, title: &str, path: &str) -> Option<String>;
    /// The outside song (provider, external id) a library song was downloaded from.
    async fn downloaded_from(&self, library_id: &str) -> Option<(String, String)>;
    /// Whether `owner` can read library song `library_id`, whose library is `library` when the
    /// caller knows it.
    fn readable(&self, owner: &str, library_id: &str, library: Option<i64>) -> Readable;
    /// The catalog's song.
    async fn catalog_song(&self, provider: &str, external_id: &str) -> Option<Song>;
    fn parse_external_id(&self, id: &str) -> ParsedExternalId;
}

/// The real library seam.
pub struct AppHeldLibrary {
    pub local: Arc<dyn ILocalLibraryService>,
    pub resolver: Arc<NavidromeSongPathResolver>,
    pub scope: ScopeResolver,
    pub metadata: Arc<dyn IMusicMetadataService>,
    pub history: Arc<DownloadHistoryService>,
}

#[async_trait]
impl HeldLibrary for AppHeldLibrary {
    async fn own_version_path(&self, owner: &str, provider: &str, external_id: &str) -> Option<String> {
        self.local
            .get_local_path_for_external_song_as(Some(owner), provider, external_id)
            .await
    }

    async fn library_id_at(&self, artist: &str, title: &str, path: &str) -> Option<String> {
        self.resolver.find_id_by_path(artist, title, path).await
    }

    async fn downloaded_from(&self, library_id: &str) -> Option<(String, String)> {
        let path = self.scope.index().song(library_id).map(|song| song.path);
        let mapped = self.local.get_mappings().await.into_iter().find(|mapping| {
            mapping.local_subsonic_id.as_deref() == Some(library_id)
                || path.as_deref().is_some_and(|path| mapping.local_path == path)
        });
        if let Some(mapping) = mapped
            && !mapping.external_provider.is_empty()
            && !mapping.external_id.is_empty()
        {
            return Some((mapping.external_provider, mapping.external_id));
        }
        let path = path?;
        self.history
            .get_recent(i32::MAX)
            .into_iter()
            .find(|entry| entry.path == path)
            .and_then(|entry| entry.key)
            .and_then(|key| {
                let (provider, external_id) = key.split_once(':')?;
                (!provider.is_empty() && !external_id.is_empty())
                    .then(|| (provider.to_string(), external_id.to_string()))
            })
    }

    fn readable(&self, owner: &str, library_id: &str, library: Option<i64>) -> Readable {
        let personal: Vec<(i64, String)> = self
            .scope
            .personal_library_ids()
            .into_iter()
            .filter_map(|id| self.scope.personal_library_owner(id).map(|member| (id, member)))
            .collect();
        let library = library.or_else(|| self.scope.index().library_of(library_id));
        readable_by(owner, &personal, library, self.scope.index().is_ready())
    }

    async fn catalog_song(&self, provider: &str, external_id: &str) -> Option<Song> {
        self.metadata.get_song(provider, external_id).await
    }

    fn parse_external_id(&self, id: &str) -> ParsedExternalId {
        self.local.parse_external_id(id)
    }
}

/// The held service's timers, each settable so tests run in seconds.
#[derive(Debug, Clone)]
pub struct HeldTiming {
    /// How long taking a playlist's lock may wait.
    pub lock_wait: Duration,
}

impl Default for HeldTiming {
    fn default() -> Self {
        HeldTiming {
            lock_wait: Duration::from_secs(20),
        }
    }
}

/// What the held service is built from.
pub struct HeldParts {
    pub store: HeldStore,
    pub navidrome: Arc<dyn HeldNavidrome>,
    pub library: Arc<dyn HeldLibrary>,
    pub registry: Arc<ExternalIdRegistry>,
    pub seed_index: Arc<SeedIndex>,
    pub caps: Arc<PlaylistCaps>,
    /// The family playlists, whose seed copies the start sweep refreshes.
    pub family: Option<Arc<FamilyPlaylistStore>>,
    pub timing: HeldTiming,
    pub clock: Clock,
}

/// The held playlists, for the work that is built before them (the import keeper).
pub type HeldSlot = Arc<RwLock<Option<Arc<HeldPlaylists>>>>;

/// A playlist's lock, held.
pub struct PlaylistGuard {
    id: String,
    _guard: OwnedMutexGuard<()>,
}

impl PlaylistGuard {
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// A held playlist as it is stored now.
#[derive(Debug, Clone)]
pub struct HeldView {
    pub id: String,
    pub owner: String,
    pub entries: Vec<HeldEntry>,
    pub changed: DateTime<Utc>,
}

impl HeldView {
    /// The `changed` a row and a stamp check use: the later of Navidrome's and the held one.
    pub fn served_changed(&self, navidrome: Option<DateTime<Utc>>) -> DateTime<Utc> {
        navidrome.map_or(self.changed, |changed| changed.max(self.changed))
    }
}

fn view_from(playlist: HeldPlaylist) -> HeldView {
    HeldView {
        id: playlist.id,
        owner: playlist.owner,
        entries: playlist.entries,
        changed: playlist.changed,
    }
}

/// What a library entry of a served list is served as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// The caller's own row of the library song, by its place in the caller's answer.
    Library { entry: usize, row: usize },
    /// The entry's outside song: its snapshot (or the catalog when it has none), by `id`.
    Outside { entry: usize, id: String },
}

/// The served walk: a library entry takes the next unused row of the
/// caller's answer with its id, else its outside fallback, else it is left out; a streamed entry
/// is served by its outside id.
pub fn walk(entries: &[HeldEntry], caller_rows: &[String]) -> Vec<Pick> {
    let mut used = vec![false; caller_rows.len()];
    let mut picks = Vec::new();
    for (at, entry) in entries.iter().enumerate() {
        if let Some(library) = entry.library_id.as_deref() {
            let row = (0..caller_rows.len()).find(|row| !used[*row] && caller_rows[*row] == library);
            if let Some(row) = row {
                used[row] = true;
                picks.push(Pick::Library { entry: at, row });
                continue;
            }
        }
        if let Some(outside) = entry.outside_id.clone() {
            picks.push(Pick::Outside {
                entry: at,
                id: outside,
            });
        }
    }
    picks
}

/// A streamed entry's snapshot as a song, by the entry's outside id, for the serve routes.
pub fn snapshot_song(entry: &HeldEntry) -> Option<Song> {
    let outside = entry.outside_id.clone()?;
    let snapshot = entry.song.as_ref()?;
    Some(Song {
        id: outside,
        title: snapshot.title.clone(),
        artist: snapshot.artist.clone(),
        artist_id: snapshot.artist_id.clone(),
        album: snapshot.album.clone(),
        album_id: snapshot.album_id.clone(),
        duration: snapshot.seconds,
        track: snapshot.track,
        disc_number: snapshot.disc,
        year: snapshot.year,
        genre: snapshot.genre.clone(),
        isrc: snapshot.isrcs.first().cloned(),
        isrcs: snapshot.isrcs.clone(),
        external_provider: entry.provider.clone(),
        external_id: entry.external_id.clone(),
        explicit_content_lyrics: match snapshot.explicit_status.as_deref() {
            Some("explicit") => Some(1),
            Some("clean") => Some(3),
            _ => None,
        },
        ..Song::default()
    })
}

/// What an update did.
#[derive(Debug)]
pub enum UpdateOutcome {
    /// The playlist is held (or was, and was released by this change: `view` is `None`).
    Held {
        view: Option<HeldView>,
        skipped: Vec<String>,
    },
    /// A plain playlist: the caller's request was relayed under the lock.
    Plain(Option<RelayAnswer>),
    /// Navidrome refused the name, comment or public change; nothing else was changed.
    Relayed(RelayAnswer),
}

/// What a replace did.
#[derive(Debug)]
pub enum ReplaceOutcome {
    Held {
        view: Option<HeldView>,
        skipped: Vec<String>,
    },
    Plain(Option<RelayAnswer>),
}

/// What a create did.
#[derive(Debug)]
pub enum CreateOutcome {
    Held {
        view: Option<HeldView>,
        skipped: Vec<String>,
    },
    /// Navidrome refused or did not answer the create: its answer, passed back.
    Relayed(Option<RelayAnswer>),
}

/// `updatePlaylist` (and native adds and removals) on a playlist, held or not.
pub struct UpdateRequest {
    pub id: String,
    pub caller: HeldCaller,
    /// The owner Navidrome's answer to the caller named (the route checked it is the caller).
    pub owner: String,
    /// `songIndexToRemove`: places over the caller's served list.
    pub remove_indexes: Vec<usize>,
    /// `octoSongToRemove=<id>@<n>`: the n-th (1-based) served entry answering to the id.
    pub remove_songs: Vec<(String, usize)>,
    /// `songIdToAdd`, built by [`HeldPlaylists::build_entries`] with [`HeldPlaylists::record_ids`].
    pub adds: Vec<Built>,
    /// The name, comment and public change relayed as the caller without song parameters.
    pub fields: Option<Relay>,
    /// The caller's request relayed as it is (without `octoSongToRemove`), for a playlist that
    /// stays plain.
    pub plain: Relay,
    pub view: Arc<dyn ServedView>,
}

/// `createPlaylist playlistId=`: a replace.
pub struct ReplaceRequest {
    pub id: String,
    pub caller: HeldCaller,
    pub owner: String,
    /// One per `songId`, in order, built with [`HeldPlaylists::takeover_ids`].
    pub songs: Vec<Built>,
    /// `octoOwnIds=1`: the client sends the server's own ids, so nothing is kept in a `Replaced`
    /// record and nothing is paired.
    pub own_ids: bool,
    /// Pairing for 1.12.0 phones: `c=Octo` without `octoOwnIds=1`.
    pub pairing: bool,
    /// `octoIfStamp`, parsed.
    pub if_stamp: Option<DateTime<Utc>>,
    pub view: Arc<dyn ServedView>,
    pub plain: Relay,
}

/// `createPlaylist` without `playlistId`, holding an outside song.
pub struct CreateRequest {
    pub owner: String,
    pub name: String,
    /// Every entry in order (library and outside).
    pub entries: Vec<HeldEntry>,
    pub skipped: Vec<String>,
    /// The owner's playlist ids of this name, read before the relay.
    pub before: Vec<String>,
    /// The create relayed as the caller with library ids only: the new playlist's id, or the
    /// answer to pass back.
    pub relay: BoxFuture<'static, Result<String, Option<RelayAnswer>>>,
}

/// What a gone check found for the owner's placements: Navidrome-shaped playlist ids confirmed
/// gone by two 404s, whose placements the folder store drops.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoneReport {
    pub confirmed: Vec<String>,
}

/// What an operation does to its playlist's `Replaced` record.
enum RecordChange {
    Keep,
    Set(ReplacedRecord),
    Clear,
}

/// What reading a playlist's tracks gave.
enum RawRead {
    Rows(Vec<TrackFacts>),
    Gone,
    Unreachable,
}

/// What a locked sync read.
pub(crate) struct Synced {
    raw: Vec<TrackFacts>,
    info: Option<PlaylistInfo>,
}

fn assert_store_free() {
    debug_assert!(
        !store_mutex_held(),
        "a registry, seed index, caps or Navidrome call while the held store's mutex is held"
    );
}

/// Points a test parks a task at.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Park {
    /// A read, after its raw read without the lock.
    AfterPreRead,
    /// A locked operation, right after its sync.
    AfterSync,
    /// A mirror write, before each Navidrome call.
    BeforeCall,
}

/// A parked point: told when a task reaches it, and the signal it waits for.
#[cfg(test)]
type ParkSlot = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
#[derive(Default)]
struct Hooks {
    parks: Mutex<HashMap<Park, ParkSlot>>,
    fail_writes: std::sync::atomic::AtomicBool,
}

/// The one service every held-playlist writer goes through.
pub struct HeldPlaylists {
    store: HeldStore,
    navidrome: Arc<dyn HeldNavidrome>,
    library: Arc<dyn HeldLibrary>,
    registry: Arc<ExternalIdRegistry>,
    seed_index: Arc<SeedIndex>,
    caps: Arc<PlaylistCaps>,
    family: Option<Arc<FamilyPlaylistStore>>,
    timing: HeldTiming,
    clock: Clock,
    /// Ids whose held file is kept aside in `gone/`.
    aside: Mutex<HashSet<String>>,
    /// Ids whose files a save past the commit point could not write, for the flush pass.
    unsaved: Mutex<HashSet<String>>,
    /// `<owner>|<id>` of placements one gone check found missing.
    placements_seen: Mutex<HashSet<String>>,
    locks: Mutex<HashMap<String, Weak<TokioMutex<()>>>>,
    /// Creation intents whose task is still running.
    live_creates: Mutex<HashSet<u64>>,
    /// Owners with a gone check in flight.
    gone_running: Mutex<HashSet<String>>,
    flush_wanted: Notify,
    binder: Binder,
    #[cfg(test)]
    hooks: Hooks,
}

fn owner_key(owner: &str) -> String {
    owner.trim().to_lowercase()
}

/// Whether an entry streams: an outside song with no library song.
pub fn streams(entry: &HeldEntry) -> bool {
    entry.library_id.is_none() && entry.outside_id.is_some()
}

/// Whether an entry counts against its owner's cap (and is kept in the registry): it streams and
/// has a registry id.
fn counts(entry: &HeldEntry) -> bool {
    streams(entry) && entry.seed_id.is_some()
}

fn counted(entries: &[HeldEntry]) -> usize {
    entries.iter().filter(|entry| counts(entry)).count()
}

fn library_ids(entries: &[HeldEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| entry.library_id.clone())
        .collect()
}

fn ids_of(raw: &[TrackFacts]) -> Vec<String> {
    raw.iter().map(|row| row.media_file_id.clone()).collect()
}

fn common_prefix(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Whether an entry answers to a song id: its library id or its outside id.
fn answers(entry: &HeldEntry, id: &str) -> bool {
    entry.library_id.as_deref() == Some(id) || entry.outside_id.as_deref() == Some(id)
}

/// The id a caller sees an entry by.
fn served_id(entry: &HeldEntry) -> &str {
    entry
        .library_id
        .as_deref()
        .or(entry.outside_id.as_deref())
        .unwrap_or_default()
}

/// What the served list is made of: the ids, their order and the snapshot fields served.
fn served_signature(
    entries: &[HeldEntry],
) -> Vec<(
    Option<&str>,
    Option<&str>,
    Option<&super::held_store::SongSnapshot>,
)> {
    entries
        .iter()
        .map(|entry| {
            (
                entry.library_id.as_deref(),
                entry.outside_id.as_deref(),
                entry.song.as_ref(),
            )
        })
        .collect()
}

fn raw_row(row: &TrackFacts) -> RawRow {
    RawRow {
        position: row.position.clone(),
        id: row.media_file_id.clone(),
        title: row.title.clone(),
        artist: row.artist.clone(),
        seconds: row.seconds,
        missing: row.missing,
    }
}

/// An instant as Navidrome and the apps write it.
pub fn parse_instant(text: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text?.trim())
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Whether two instants are one at millisecond precision (the `octoIfStamp` compare).
pub fn same_instant(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a.timestamp_millis() == b.timestamp_millis()
}

/// A Navidrome-shaped playlist id: 22 base62 characters starting 0 to 7.
pub fn is_navidrome_shaped(id: &str) -> bool {
    id.len() == 22
        && id.bytes().all(|b| b.is_ascii_alphanumeric())
        && id.as_bytes().first().is_some_and(|b| (b'0'..=b'7').contains(b))
}

/// Per owner, how the counted entries change from `before` to `after`.
fn count_deltas(before: Option<&HeldPlaylist>, after: Option<&HeldPlaylist>) -> Vec<(String, i64)> {
    let mut deltas: Vec<(String, i64)> = Vec::new();
    let mut add = |owner: &str, n: i64| {
        let key = owner_key(owner);
        match deltas.iter_mut().find(|(o, _)| *o == key) {
            Some((_, delta)) => *delta += n,
            None => deltas.push((key, n)),
        }
    };
    if let Some(before) = before {
        add(&before.owner, -(counted(&before.entries) as i64));
    }
    if let Some(after) = after {
        add(&after.owner, counted(&after.entries) as i64);
    }
    deltas.retain(|(_, n)| *n != 0);
    deltas
}

/// `a - b`, per owner.
fn minus(a: &[(String, i64)], b: &[(String, i64)]) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = a.to_vec();
    for (owner, n) in b {
        match out.iter_mut().find(|(o, _)| o == owner) {
            Some((_, delta)) => *delta -= n,
            None => out.push((owner.clone(), -n)),
        }
    }
    out.retain(|(_, n)| *n != 0);
    out
}

fn negate(deltas: &[(String, i64)]) -> Vec<(String, i64)> {
    deltas.iter().map(|(owner, n)| (owner.clone(), -n)).collect()
}

/// Removes the entries at these indexes (sorted or not, repeats ignored).
fn remove_at(entries: &mut Vec<HeldEntry>, places: &[usize]) {
    let gone: HashSet<usize> = places.iter().copied().collect();
    let mut at = 0;
    entries.retain(|_| {
        let keep = !gone.contains(&at);
        at += 1;
        keep
    });
}

/// The entry indexes `octoSongToRemove` pairs name: the n-th (1-based) served entry answering to
/// each id.
fn places_of_songs(entries: &[HeldEntry], served: &[usize], songs: &[(String, usize)]) -> Vec<usize> {
    songs
        .iter()
        .filter_map(|(id, n)| {
            served
                .iter()
                .copied()
                .filter(|at| entries.get(*at).is_some_and(|entry| answers(entry, id)))
                .nth(n.checked_sub(1)?)
        })
        .collect()
}

/// The changed stretches between the equal runs of a Myers diff of `old` and `new`.
fn gaps(old: &[String], new: &[String]) -> Vec<(Range<usize>, Range<usize>)> {
    let mut gaps = Vec::new();
    let (mut old_at, mut new_at) = (0, 0);
    for op in capture_diff_slices(Algorithm::Myers, old, new) {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            if old_index > old_at || new_index > new_at {
                gaps.push((old_at..old_index, new_at..new_index));
            }
            old_at = old_index + len;
            new_at = new_index + len;
        }
    }
    if old_at < old.len() || new_at < new.len() {
        gaps.push((old_at..old.len(), new_at..new.len()));
    }
    gaps
}

/// What a replace leaves.
struct Replacement {
    entries: Vec<HeldEntry>,
    removed: Vec<HeldEntry>,
    skipped: Vec<String>,
}

/// A replace: each sent id takes the first unused served entry answering to it, else
/// the `Replaced` record's entry for it, else the entry built for it. With `pairing`, between the
/// takeover anchors an unused streamed entry pairs with an unused sent library id in the same gap
/// that passes the 1.12.0 phones' match: bound when `same_take` also says so, else the phone's own
/// match (the library id is dropped and the streamed entry keeps its place). Entries the caller is
/// not served stay after the entry they followed; any other unused served entry is removed.
fn replace_entries(
    entries: &[HeldEntry],
    served: &[usize],
    songs: Vec<Built>,
    pairing: bool,
    mut record: Option<&mut ReplacedRecord>,
) -> Replacement {
    enum Slot {
        Taken(usize),
        Moved(usize, HeldEntry),
        New(HeldEntry),
        Empty,
    }
    let served: Vec<usize> = served.iter().copied().filter(|at| *at < entries.len()).collect();
    let mut used = vec![false; entries.len()];
    let sent: Vec<String> = songs.iter().map(|song| song.id().to_string()).collect();
    let mut skipped = Vec::new();
    let mut slots: Vec<Slot> = Vec::with_capacity(songs.len());
    for song in songs {
        let id = song.id().to_string();
        if let Some(at) = served
            .iter()
            .copied()
            .find(|at| !used[*at] && answers(&entries[*at], &id))
        {
            used[at] = true;
            slots.push(Slot::Taken(at));
            continue;
        }
        let back = record.as_deref_mut().and_then(|record| {
            let at = record.entries.iter().position(|entry| answers(entry, &id))?;
            Some(record.entries.remove(at))
        });
        match (back, song) {
            (Some(entry), _) | (None, Built::Entry { entry, .. }) => slots.push(Slot::New(entry)),
            (None, _) => {
                skipped.push(id);
                slots.push(Slot::Empty);
            }
        }
    }
    if pairing {
        let served_ids: Vec<String> = served
            .iter()
            .map(|at| served_id(&entries[*at]).to_string())
            .collect();
        for (old, new) in gaps(&served_ids, &sent) {
            let streamed: Vec<usize> = served[old]
                .iter()
                .copied()
                .filter(|at| !used[*at] && streams(&entries[*at]) && entries[*at].song.is_some())
                .collect();
            let library: Vec<usize> = new
                .filter(|slot| matches!(&slots[*slot], Slot::New(entry) if entry.library_id.is_some()))
                .collect();
            let mut next = 0;
            for at in streamed {
                let Some(snapshot) = entries[at].song.as_ref() else {
                    continue;
                };
                let facts = entries::take_of_snapshot(snapshot);
                let found = (next..library.len()).find(|k| {
                    matches!(&slots[library[*k]], Slot::New(candidate)
                        if candidate.song.as_ref().is_some_and(|song| phone_match(&facts, &entries::take_of_snapshot(song))))
                });
                let Some(k) = found else {
                    continue;
                };
                next = k + 1;
                let slot = library[k];
                let (library_id, bound) = match &slots[slot] {
                    Slot::New(candidate) => (
                        candidate.library_id.clone(),
                        candidate
                            .song
                            .as_ref()
                            .is_some_and(|song| same_take(&facts, &entries::take_of_snapshot(song))),
                    ),
                    _ => continue,
                };
                let mut entry = entries[at].clone();
                if bound {
                    entry.library_id = library_id;
                }
                used[at] = true;
                slots[slot] = Slot::Moved(at, entry);
            }
        }
    }
    let mut out: Vec<(Option<usize>, HeldEntry)> = slots
        .into_iter()
        .filter_map(|slot| match slot {
            Slot::Taken(at) => Some((Some(at), entries[at].clone())),
            Slot::Moved(at, entry) => Some((Some(at), entry)),
            Slot::New(entry) => Some((None, entry)),
            Slot::Empty => None,
        })
        .collect();
    let shown: HashSet<usize> = served.iter().copied().collect();
    let mut at_start = 0;
    for hidden in (0..entries.len()).filter(|at| !shown.contains(at)) {
        let after = out
            .iter()
            .enumerate()
            .filter_map(|(place, (origin, _))| {
                origin
                    .filter(|origin| *origin < hidden)
                    .map(|origin| (origin, place))
            })
            .max_by_key(|(origin, _)| *origin)
            .map(|(_, place)| place);
        let place = match after {
            Some(place) => place + 1,
            None => {
                at_start += 1;
                at_start - 1
            }
        };
        out.insert(place, (Some(hidden), entries[hidden].clone()));
    }
    let removed = served
        .iter()
        .copied()
        .filter(|at| !used[*at])
        .map(|at| entries[at].clone())
        .collect();
    Replacement {
        entries: out.into_iter().map(|(_, entry)| entry).collect(),
        removed,
        skipped,
    }
}

/// An add: each id an entry of the playlist's `Replaced` record answers to takes that entry back
/// (seed, snapshot and binding); the rest are the entries built for them. Answers the entries, the
/// ids skipped, and whether anything was taken back.
fn take_back(
    adds: Vec<Built>,
    mut record: Option<&mut ReplacedRecord>,
) -> (Vec<HeldEntry>, Vec<String>, bool) {
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let mut took = false;
    for add in adds {
        let id = add.id().to_string();
        let back = record.as_deref_mut().and_then(|record| {
            let at = record.entries.iter().position(|entry| answers(entry, &id))?;
            Some(record.entries.remove(at))
        });
        match (back, add) {
            (Some(entry), _) => {
                entries.push(entry);
                took = true;
            }
            (None, Built::Entry { entry, .. }) => entries.push(entry),
            (None, _) => skipped.push(id),
        }
    }
    (entries, skipped, took)
}

/// What a caller's change does to the record: the same caller's add-only update keeps it (less
/// what it took back); anything else clears it.
fn record_rule(
    before: Option<&ReplacedRecord>,
    after: Option<ReplacedRecord>,
    caller: &HeldCaller,
    add_only: bool,
) -> RecordChange {
    let Some(before) = before else {
        return RecordChange::Keep;
    };
    if !(add_only && caller.made(before)) {
        return RecordChange::Clear;
    }
    match after {
        Some(after) if after.entries.is_empty() => RecordChange::Clear,
        Some(after) if after.entries.len() != before.entries.len() => RecordChange::Set(after),
        _ => RecordChange::Keep,
    }
}

/// The list Navidrome holds right before and right after the call a pending write has on record,
/// and the write as it stands after that call; `None` when no call is left.
fn call_states(write: &PendingWrite) -> Option<(Vec<String>, Vec<String>, PendingWrite)> {
    let stale = write.from.len().saturating_sub(write.prefix);
    let removing = matches!(write.phase, WritePhase::Removing);
    if removing && write.done < stale {
        let end = stale - write.done;
        let start = end.saturating_sub(REMOVE_CHUNK);
        let mut after = write.clone();
        after.done += end - start;
        return Some((
            write.from[..write.prefix + end].to_vec(),
            write.from[..write.prefix + start].to_vec(),
            after,
        ));
    }
    let added = if removing { 0 } else { write.done };
    let from = write.prefix + added;
    if from >= write.target.len() {
        return None;
    }
    let to = (from + ADD_CHUNK).min(write.target.len());
    let mut after = write.clone();
    after.phase = WritePhase::Adding;
    after.done = to - write.prefix;
    Some((write.target[..from].to_vec(), write.target[..to].to_vec(), after))
}

impl HeldPlaylists {
    /// Starts the service over a loaded held store, telling the registry and the seed index the
    /// kept ids and seeds of every held playlist and `Replaced` record (each under its stored
    /// revision). Called once at start, before the family seeds and `finish_start`.
    pub fn start(parts: HeldParts) -> Arc<HeldPlaylists> {
        for warning in parts.store.load_warnings() {
            warn!("PLAYLISTS {warning}");
        }
        let aside: HashSet<String> = parts.store.gone_ids().into_iter().collect();
        let held = Arc::new(HeldPlaylists {
            store: parts.store,
            navidrome: parts.navidrome,
            library: parts.library,
            registry: parts.registry,
            seed_index: parts.seed_index,
            caps: parts.caps,
            family: parts.family,
            timing: parts.timing,
            clock: parts.clock,
            aside: Mutex::new(aside),
            unsaved: Mutex::new(HashSet::new()),
            placements_seen: Mutex::new(HashSet::new()),
            locks: Mutex::new(HashMap::new()),
            live_creates: Mutex::new(HashSet::new()),
            gone_running: Mutex::new(HashSet::new()),
            flush_wanted: Notify::new(),
            binder: Binder::default(),
            #[cfg(test)]
            hooks: Hooks::default(),
        });
        for playlist in held.store.list() {
            held.publish(&held_key(&playlist.id), playlist.revision, &playlist.entries);
        }
        for record in held.store.records() {
            held.publish(&replaced_key(&record.playlist), record.revision, &record.entries);
        }
        held
    }

    /// Per owner (lowercased), the counted entries of every held playlist, for the caps' count
    /// at start.
    pub fn counted_by_owner(&self) -> HashMap<String, usize> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for playlist in self.store.list() {
            *counts.entry(owner_key(&playlist.owner)).or_default() += counted(&playlist.entries);
        }
        counts
    }

    /// Binding listens to the tracker's ended rows from now on.
    pub fn listen(self: &Arc<Self>, tracker: &AcquisitionTracker) {
        let weak = Arc::downgrade(self);
        tracker.subscribe_ended(Arc::new(move |end| {
            if let Some(held) = weak.upgrade() {
                held.binder.ended(&held, end);
            }
        }));
    }

    pub(crate) fn binder(&self) -> &Binder {
        &self.binder
    }

    /// Resolves once every acquisition end told so far has been bound.
    pub async fn binds_settled(&self) {
        self.binder.settled().await;
    }

    // ---- the calls the store's mutex is never held across ---------------------------------------

    fn reg(&self) -> &ExternalIdRegistry {
        assert_store_free();
        &self.registry
    }

    fn index(&self) -> &SeedIndex {
        assert_store_free();
        &self.seed_index
    }

    fn caps(&self) -> &PlaylistCaps {
        assert_store_free();
        &self.caps
    }

    /// Tests: the caps' bounds from now on, the counts kept (the family store shares the caps).
    #[cfg(test)]
    pub fn set_caps_for_tests(&self, person: usize, server: usize) {
        self.caps.set_limits(person, server);
    }

    fn nav(&self) -> &dyn HeldNavidrome {
        assert_store_free();
        self.navidrome.as_ref()
    }

    /// Whether the test hook fails every held-store write now.
    fn writes_fail(&self) -> bool {
        #[cfg(test)]
        if self.hooks.fail_writes.load(std::sync::atomic::Ordering::SeqCst) {
            return true;
        }
        false
    }

    /// Writes an operation's files in the store's commit order.
    fn save_state(&self, id: &str, before: &PlaylistState, stored: &PlaylistState) -> Result<(), SaveError> {
        if self.writes_fail() {
            return Err(SaveError::BeforeCommit("writes fail in this test".to_string()));
        }
        self.store.save(id, before, stored)
    }

    // ---- the lock ---------------------------------------------------------------------------------

    /// Takes a playlist's lock within [`HeldTiming::lock_wait`]: [`HeldError::Busy`] when the
    /// deadline passes. Dead handles in the map are swept each time a lock is made.
    pub async fn lock(&self, id: &str) -> Result<PlaylistGuard, HeldError> {
        let mutex = {
            let mut locks = self.locks.lock();
            locks.retain(|_, weak| weak.strong_count() > 0);
            match locks.get(id).and_then(Weak::upgrade) {
                Some(mutex) => mutex,
                None => {
                    let mutex = Arc::new(TokioMutex::new(()));
                    locks.insert(id.to_string(), Arc::downgrade(&mutex));
                    mutex
                }
            }
        };
        match tokio::time::timeout(self.timing.lock_wait, mutex.lock_owned()).await {
            Ok(guard) => Ok(PlaylistGuard {
                id: id.to_string(),
                _guard: guard,
            }),
            Err(_) => Err(HeldError::Busy),
        }
    }

    /// Runs `work` under the playlist's lock in a spawned task the caller awaits, so a dropped
    /// request never stops it halfway. The deadline is applied inside the task.
    async fn spawn_locked<T, F, Fut>(self: &Arc<Self>, id: &str, work: F) -> Result<T, HeldError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<HeldPlaylists>, PlaylistGuard) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, HeldError>> + Send + 'static,
    {
        let this = Arc::clone(self);
        let id = id.to_string();
        let task = tokio::spawn(async move {
            let guard = this.lock(&id).await?;
            work(this, guard).await
        });
        task.await.unwrap_or(Err(HeldError::Busy))
    }

    /// Whether the guarded playlist is held now.
    pub fn is_held(&self, guard: &PlaylistGuard) -> bool {
        self.is_held_id(guard.id())
    }

    fn is_held_id(&self, id: &str) -> bool {
        self.store.is_held(id)
    }

    /// The held playlist as stored now; `None` when it is not held.
    pub fn view_of(&self, id: &str) -> Option<HeldView> {
        self.store.get(id).map(view_from)
    }

    /// Every held playlist of an owner, as stored now (Keep ready, the sync catalog).
    pub fn views_of_owner(&self, owner: &str) -> Vec<HeldView> {
        self.store
            .list()
            .into_iter()
            .filter(|playlist| eq_ignore_case(&playlist.owner, owner))
            .map(view_from)
            .collect()
    }

    /// Every held playlist id.
    pub fn held_ids(&self) -> Vec<String> {
        self.store.ids()
    }

    /// Whether `id` is held now: one lookup, no playlist copied.
    pub fn contains(&self, id: &str) -> bool {
        self.store.is_held(id)
    }

    /// Whether no playlist is held now.
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    /// Whether Octo has a part in a change to `id` that it would otherwise relay as it came: the
    /// playlist is held, kept aside by the gone check (a locked route moves it back), or keeps a
    /// `Replaced` record (an add may take an entry back, and any other change clears it).
    pub fn has_part(&self, id: &str) -> bool {
        self.store.is_held(id) || self.store.has_record(id) || self.aside.lock().contains(id)
    }

    /// The ids the playlist's `Replaced` record answers to, read without the lock: an update's
    /// adds of these are taken back under the lock, not built.
    pub fn record_ids(&self, id: &str) -> HashSet<String> {
        let mut ids = HashSet::new();
        if let Some(record) = self.store.replaced(id) {
            for entry in record.entries {
                ids.extend(entry.library_id);
                ids.extend(entry.outside_id);
            }
        }
        ids
    }

    /// The ids the playlist's entries and its `Replaced` record answer to, read without the
    /// lock: a replace takes these over under the lock and builds only the rest.
    pub fn takeover_ids(&self, id: &str) -> HashSet<String> {
        let mut ids = self.record_ids(id);
        if let Some(playlist) = self.store.get(id) {
            for entry in playlist.entries {
                ids.extend(entry.library_id);
                ids.extend(entry.outside_id);
            }
        }
        ids
    }

    /// Builds the entries for the song ids a client sends, outside the lock.
    pub async fn build_entries(
        &self,
        owner: &str,
        ids: &[String],
        takeovers: &HashSet<String>,
    ) -> Result<Vec<Built>, HeldError> {
        let context = BuildContext {
            registry: self.reg(),
            library: self.library.as_ref(),
            navidrome: self.nav(),
            now: self.clock.now(),
        };
        entries::build(&context, owner, ids, takeovers).await
    }

    /// The library ids among `ids` the owner can read, for the keeper's wanted list: a definite
    /// "no" leaves an id out; "not now" or "could not tell" is [`HeldError::Busy`].
    pub async fn readable_library_ids(
        &self,
        owner: &str,
        ids: Vec<String>,
    ) -> Result<Vec<String>, HeldError> {
        let checked: Vec<(String, Result<bool, HeldError>)> = futures::stream::iter(ids)
            .map(|id| async move {
                let readable = match self.nav().song_facts(&id).await {
                    Err(_) => Err(HeldError::Busy),
                    Ok(None) => Ok(false),
                    Ok(Some(facts)) => match self.library.readable(owner, &id, facts.library_id) {
                        Readable::Yes => Ok(true),
                        Readable::No => Ok(false),
                        Readable::NotNow => Err(HeldError::Busy),
                    },
                };
                (id, readable)
            })
            .buffered(AT_ONCE)
            .collect()
            .await;
        let mut kept = Vec::new();
        for (id, readable) in checked {
            if readable? {
                kept.push(id);
            }
        }
        Ok(kept)
    }

    // ---- kept ids, counts and saves -------------------------------------------------------------

    /// Tells the registry and the seed index what a key holds now: kept, the registry ids of its
    /// streamed entries; indexed, the seeds of every entry with one.
    fn publish(&self, key: &str, revision: u64, entries: &[HeldEntry]) {
        self.reg().set_kept(key, revision, kept_ids(entries));
        self.index().set(key, revision, seeds(entries));
    }

    /// Publishes what changed from `from` to `to` under a fresh revision: the main key always (a
    /// release empties it), the record key when the record moved.
    fn publish_state(&self, id: &str, from: &PlaylistState, to: &PlaylistState) {
        let revision = self.store.take_revision();
        self.publish(
            &held_key(id),
            revision,
            to.held
                .as_ref()
                .map_or(&[][..], |playlist| playlist.entries.as_slice()),
        );
        if from.replaced != to.replaced {
            self.publish(
                &replaced_key(id),
                revision,
                to.replaced
                    .as_ref()
                    .map_or(&[][..], |record| record.entries.as_slice()),
            );
        }
    }

    /// Applies count changes that never refuse (merges, move-backs, removals, rollbacks).
    fn force(&self, deltas: &[(String, i64)]) {
        let deltas: Vec<(String, i64)> = deltas.iter().filter(|(_, n)| *n != 0).cloned().collect();
        if !deltas.is_empty() {
            self.caps().force(&deltas);
        }
    }

    /// Applies an add's rise, refused past a cap.
    fn rise(
        &self,
        owner: &str,
        entries: &[HeldEntry],
        before: &[HeldEntry],
    ) -> Result<Vec<(String, i64)>, HeldError> {
        let rise = counted(entries) as i64 - counted(before) as i64;
        if rise <= 0 {
            return Ok(Vec::new());
        }
        let deltas = vec![(owner_key(owner), rise)];
        self.caps().apply(&deltas).map_err(HeldError::from_cap)?;
        Ok(deltas)
    }

    fn before(&self, id: &str) -> PlaylistState {
        self.store.state(id)
    }

    fn mark_unsaved(&self, id: &str, error: &str) {
        warn!("PLAYLISTS {id} could not be written ({error}); the next save or flush writes it");
        self.unsaved.lock().insert(id.to_string());
        self.flush_wanted.notify_one();
    }

    /// Commits an operation: memory (with fresh revisions, under the store's mutex), then the kept
    /// ids and the counts, then the files in the store's commit order (the record first, then the
    /// main file, the commit point, then the previous record's delete). A failure before the
    /// commit point puts memory, kept ids and counters back (with `preapplied`, which the caller
    /// applied) and answers [`HeldError::SaveFailed`]; one after it is written by the flush pass.
    fn commit(
        &self,
        guard: &PlaylistGuard,
        before: &PlaylistState,
        after: Option<HeldPlaylist>,
        record: RecordChange,
        preapplied: &[(String, i64)],
    ) -> Result<Option<HeldPlaylist>, HeldError> {
        let id = guard.id();
        let replaced = match record {
            RecordChange::Keep => before.replaced.clone(),
            RecordChange::Set(record) => Some(record),
            RecordChange::Clear => None,
        };
        let (previous, stored) = self.store.swap(
            id,
            PlaylistState {
                held: after,
                replaced,
            },
        );
        self.publish_state(id, &previous, &stored);
        let deltas = count_deltas(previous.held.as_ref(), stored.held.as_ref());
        self.force(&minus(&deltas, preapplied));
        match self.save_state(id, &previous, &stored) {
            Ok(()) => {
                self.unsaved.lock().remove(id);
                Ok(stored.held)
            }
            Err(SaveError::BeforeCommit(error)) => {
                warn!("PLAYLISTS {id} could not be saved, so nothing was changed: {error}");
                self.store.restore(id, previous.clone());
                self.publish_state(id, &stored, &previous);
                self.force(&negate(&deltas));
                Err(HeldError::SaveFailed)
            }
            Err(SaveError::AfterCommit(error)) => {
                self.mark_unsaved(id, &error);
                Ok(stored.held)
            }
        }
    }

    /// Saves a mirror write's progress before its next Navidrome call; the store keeps memory as
    /// it was when the write fails.
    fn save_progress(&self, playlist: &HeldPlaylist) -> Result<HeldPlaylist, HeldError> {
        if self.writes_fail() {
            return Err(HeldError::SaveFailed);
        }
        match self.store.save_progress(playlist.clone()) {
            Ok(saved) => Ok(saved),
            Err(SaveError::BeforeCommit(error)) => {
                warn!(
                    "PLAYLISTS {} could not save the write's progress: {error}",
                    playlist.id
                );
                Err(HeldError::SaveFailed)
            }
            Err(SaveError::AfterCommit(error)) => {
                self.mark_unsaved(&playlist.id, &error);
                Ok(self.store.get(&playlist.id).unwrap_or_else(|| playlist.clone()))
            }
        }
    }

    /// Saves a settled write (past the commit point): kept ids and counts follow its entries; a
    /// failed write is written by the flush pass.
    fn save_settled(&self, guard: &PlaylistGuard, next: HeldPlaylist) {
        let id = guard.id();
        let replaced = self.store.replaced(id);
        let (previous, stored) = self.store.swap(
            id,
            PlaylistState {
                held: Some(next),
                replaced,
            },
        );
        let entries_moved = previous.held.as_ref().map(|playlist| &playlist.entries)
            != stored.held.as_ref().map(|playlist| &playlist.entries);
        if entries_moved {
            self.publish_state(id, &previous, &stored);
            self.force(&count_deltas(previous.held.as_ref(), stored.held.as_ref()));
        }
        if let Err(SaveError::BeforeCommit(error) | SaveError::AfterCommit(error)) =
            self.save_state(id, &previous, &stored)
        {
            self.mark_unsaved(id, &error);
        }
    }

    /// The flush pass: under each playlist's lock, the files of every playlist a write past its
    /// commit point could not write are made to match memory.
    pub async fn flush(&self) {
        let mut ids: HashSet<String> = self.store.needs_save().into_iter().collect();
        ids.extend(self.unsaved.lock().drain());
        for id in ids {
            let Ok(_guard) = self.lock(&id).await else {
                self.unsaved.lock().insert(id);
                continue;
            };
            if self.writes_fail() {
                self.unsaved.lock().insert(id);
                continue;
            }
            if let Err(error) = self.store.sync_files(&id) {
                warn!("PLAYLISTS {id} still could not be written: {error}");
                self.unsaved.lock().insert(id);
            }
        }
    }

    /// The changed stamp: it moves only when the served list changes, to the later of now,
    /// Navidrome's changed plus 1 ms and the previous stamp plus 1 ms.
    fn restamp(
        &self,
        before: Option<&HeldPlaylist>,
        after: &mut HeldPlaylist,
        navidrome: Option<DateTime<Utc>>,
    ) {
        let moved =
            before.is_none_or(|before| served_signature(&before.entries) != served_signature(&after.entries));
        if !moved {
            return;
        }
        let mut changed = self.clock.now();
        if let Some(navidrome) = navidrome {
            changed = changed.max(navidrome + TimeDelta::milliseconds(1));
        }
        if let Some(before) = before {
            changed = changed.max(before.changed + TimeDelta::milliseconds(1));
        }
        after.changed = changed;
    }

    /// Releases the playlist when nothing streams and nothing is pending: its file
    /// goes and its kept key empties. A `Replaced` record never holds a playlist.
    fn release_if_done(&self, guard: &PlaylistGuard) {
        let id = guard.id();
        let done = self
            .store
            .get(id)
            .is_some_and(|playlist| playlist.pending.is_none() && !playlist.entries.iter().any(streams));
        if !done {
            return;
        }
        let before = self.before(id);
        if self.commit(guard, &before, None, RecordChange::Keep, &[]).is_ok() {
            info!("PLAYLISTS {id}: nothing streams any more, so Navidrome alone holds it");
        }
    }

    // ---- reading Navidrome ----------------------------------------------------------------------

    async fn read_raw(&self, id: &str) -> RawRead {
        match self.nav().read_track_facts(id).await {
            Ok(Some(rows)) => RawRead::Rows(rows),
            _ => match self.nav().playlist_info(id).await {
                Ok(None) => RawRead::Gone,
                _ => RawRead::Unreachable,
            },
        }
    }

    /// Whether each library song is still there: `Some(false)` when Navidrome has no such song or
    /// its file is missing, `None` when it could not tell.
    async fn presence(&self, ids: Vec<String>) -> HashMap<String, Option<bool>> {
        futures::stream::iter(ids)
            .map(|id| async move {
                let present = match self.nav().song_facts(&id).await {
                    Ok(Some(facts)) => Some(!facts.missing),
                    Ok(None) => Some(false),
                    Err(_) => None,
                };
                (id, present)
            })
            .buffered(AT_ONCE)
            .collect()
            .await
    }

    /// The merge of what Navidrome holds into the entries; [`HeldError::Busy`] when
    /// a presence it needs could not be told, and then nothing changes.
    async fn merge_raw(
        &self,
        entries: &[HeldEntry],
        base: &[String],
        raw: &[TrackFacts],
    ) -> Result<Vec<HeldEntry>, HeldError> {
        let rows: Vec<RawRow> = raw.iter().map(raw_row).collect();
        let needed = merge::ids_needing_presence(entries, base, &rows);
        let present = self.presence(needed).await;
        merge::merge(entries, base, &rows, &present).map_err(|_| HeldError::Busy)
    }

    /// Refreshes each entry's seed copy from the registry's routing (read with `peek`, so nothing
    /// is restored or made recent); an id the registry does not have keeps its copy.
    fn refresh_seeds(&self, entries: &mut [HeldEntry]) {
        for entry in entries.iter_mut() {
            let Some(seed_id) = entry.seed_id.clone() else {
                continue;
            };
            let Some(routing) = self.reg().peek(&seed_id) else {
                continue;
            };
            let now = routing.snapshot();
            if entry.seed.as_deref() != Some(&now) {
                entry.seed = Some(Arc::new(now));
            }
        }
    }

    // ---- sync and the mirror write --------------------------------------------------------------

    /// A locked sync: reads Navidrome's list again after the lock (a list read before
    /// it only decided whether to lock), moves a kept-aside file back, settles a pending write,
    /// merges a differing list, refreshes the owner and the seed copies, binds, writes the mirror
    /// and checks the release.
    pub(crate) async fn sync_locked(
        &self,
        guard: &PlaylistGuard,
        events: &[AcquisitionEnd],
    ) -> Result<Synced, HeldError> {
        let id = guard.id().to_string();
        let mut raw = match self.read_raw(&id).await {
            RawRead::Rows(raw) => raw,
            RawRead::Gone => return Err(HeldError::NotFound),
            RawRead::Unreachable => return Err(HeldError::Busy),
        };
        let aside = self.aside.lock().contains(&id);
        if aside {
            self.move_back(guard, &raw).await?;
        }
        let found = self.store.get(&id);
        let Some(mut playlist) = found else {
            return Ok(Synced { raw, info: None });
        };
        if playlist.pending.is_some() {
            raw = self.settle_pending(&mut playlist, raw).await?;
        } else if ids_of(&raw) != playlist.mirror {
            playlist.entries = self.merge_raw(&playlist.entries, &playlist.mirror, &raw).await?;
            playlist.mirror = ids_of(&raw);
        }
        let info = match self.nav().playlist_info(&id).await {
            Ok(Some(info)) => Some(info),
            Ok(None) => return Err(HeldError::NotFound),
            Err(_) => None,
        };
        if let Some(info) = &info
            && !info.owner.trim().is_empty()
        {
            playlist.owner = info.owner.clone();
        }
        self.refresh_seeds(&mut playlist.entries);
        binder::bind_entries(
            self.library.as_ref(),
            self.nav(),
            &playlist.owner,
            &mut playlist.entries,
            events,
        )
        .await;
        let navidrome_changed = info
            .as_ref()
            .and_then(|info| parse_instant(info.changed.as_deref()));
        let before = self.before(&id);
        raw = self
            .apply(
                guard,
                &before,
                playlist,
                RecordChange::Keep,
                &[],
                raw,
                navidrome_changed,
            )
            .await?;
        Ok(Synced { raw, info })
    }

    /// Commits an operation's new state and writes the mirror when the library part changed:
    /// `Pending` is part of the commit, so nothing reaches Navidrome unless it is
    /// saved. Then the release check. Answers Navidrome's list as it ends.
    #[allow(clippy::too_many_arguments)] // the operation's state, its record and its counts
    async fn apply(
        &self,
        guard: &PlaylistGuard,
        before: &PlaylistState,
        mut after: HeldPlaylist,
        record: RecordChange,
        preapplied: &[(String, i64)],
        raw: Vec<TrackFacts>,
        navidrome_changed: Option<DateTime<Utc>>,
    ) -> Result<Vec<TrackFacts>, HeldError> {
        let target = library_ids(&after.entries);
        if after.pending.is_none() && target != after.mirror {
            let prefix = common_prefix(&after.mirror, &target);
            after.pending = Some(PendingWrite {
                from: after.mirror.clone(),
                target,
                prefix,
                phase: WritePhase::Removing,
                done: 0,
            });
        }
        self.restamp(before.held.as_ref(), &mut after, navidrome_changed);
        let unchanged = matches!(record, RecordChange::Keep) && before.held.as_ref() == Some(&after);
        let committed = if unchanged {
            after
        } else {
            match self.commit(guard, before, Some(after.clone()), record, preapplied)? {
                Some(committed) => committed,
                None => after,
            }
        };
        let raw = if committed.pending.is_some() {
            self.write_mirror(guard, committed, raw).await
        } else {
            raw
        };
        self.release_if_done(guard);
        Ok(raw)
    }

    /// Runs a committed write's Navidrome calls and settles it. A call or a progress save that
    /// fails leaves `Pending` as last saved, for the next sync to resume.
    async fn write_mirror(
        &self,
        guard: &PlaylistGuard,
        mut playlist: HeldPlaylist,
        raw: Vec<TrackFacts>,
    ) -> Vec<TrackFacts> {
        let finished = match self.run_calls(&mut playlist).await {
            Ok(()) => self.finish_write(&mut playlist).await,
            Err(error) => Err(error),
        };
        match finished {
            Ok(after) => {
                self.save_settled(guard, playlist);
                after
            }
            Err(error) => {
                warn!(
                    "PLAYLISTS {}: the write to Navidrome stopped ({error:?}); the next sync finishes it",
                    guard.id()
                );
                raw
            }
        }
    }

    /// The write's remaining calls: removals after the common prefix, from the end, 100 a call,
    /// then the target's rest, 500 a call. `Phase` and `Done` are saved before each call.
    async fn run_calls(&self, playlist: &mut HeldPlaylist) -> Result<(), HeldError> {
        while let Some(write) = playlist.pending.clone() {
            let Some((before, after, advanced)) = call_states(&write) else {
                return Ok(());
            };
            #[cfg(test)]
            self.park(Park::BeforeCall).await;
            let done = if after.len() < before.len() {
                let positions: Vec<String> =
                    (after.len() + 1..=before.len()).map(|p| p.to_string()).collect();
                matches!(
                    self.nav().remove_positions(&playlist.id, &positions).await,
                    Ok(true)
                )
            } else {
                matches!(
                    self.nav().add_tracks(&playlist.id, &after[before.len()..]).await,
                    Ok(true)
                )
            };
            if !done {
                return Err(HeldError::Busy);
            }
            playlist.pending = Some(advanced);
            *playlist = self.save_progress(playlist)?;
        }
        Ok(())
    }

    /// Reads Navidrome's list after a write: equal to the target, the mirror is the target;
    /// otherwise (another writer, or ids Navidrome dropped) the list is merged with the target as
    /// the base, so what the other writer did wins and Navidrome's refusals are reflected. One
    /// write, never a retry.
    async fn finish_write(&self, playlist: &mut HeldPlaylist) -> Result<Vec<TrackFacts>, HeldError> {
        let Some(write) = playlist.pending.clone() else {
            return Err(HeldError::Busy);
        };
        let raw = match self.read_raw(&playlist.id).await {
            RawRead::Rows(raw) => raw,
            RawRead::Gone => return Err(HeldError::NotFound),
            RawRead::Unreachable => return Err(HeldError::Busy),
        };
        let ids = ids_of(&raw);
        if ids != write.target {
            playlist.entries = self.merge_raw(&playlist.entries, &write.target, &raw).await?;
        }
        playlist.mirror = ids;
        playlist.pending = None;
        Ok(raw)
    }

    /// A sync that finds `Pending`: Navidrome equal to the target settles it;
    /// equal to the state right before or right after the call on record resumes from that call;
    /// anything else (someone else changed it too) is merged with the target as the base.
    async fn settle_pending(
        &self,
        playlist: &mut HeldPlaylist,
        raw: Vec<TrackFacts>,
    ) -> Result<Vec<TrackFacts>, HeldError> {
        let Some(write) = playlist.pending.clone() else {
            return Ok(raw);
        };
        let ids = ids_of(&raw);
        if ids == write.target {
            playlist.mirror = ids;
            playlist.pending = None;
            return Ok(raw);
        }
        match call_states(&write) {
            Some((before, _, _)) if ids == before => {}
            Some((_, after, advanced)) if ids == after => {
                playlist.pending = Some(advanced);
                *playlist = self.save_progress(playlist)?;
            }
            _ => {
                playlist.entries = self.merge_raw(&playlist.entries, &write.target, &raw).await?;
                playlist.mirror = ids;
                playlist.pending = None;
                return Ok(raw);
            }
        }
        self.run_calls(playlist).await?;
        self.finish_write(playlist).await
    }

    /// Moves a kept-aside file back, never overwriting a file already there:
    /// only its streamed entries that are not present are added; its record comes back when the
    /// playlist has none. Merges and move-backs are counted, never refused.
    async fn move_back(&self, guard: &PlaylistGuard, raw: &[TrackFacts]) -> Result<(), HeldError> {
        let id = guard.id().to_string();
        let Some(kept) = self.store.read_gone(&id) else {
            self.aside.lock().remove(&id);
            return Ok(());
        };
        let before = self.before(&id);
        let record = match (&before.replaced, kept.replaced) {
            (None, Some(mut record)) => {
                record.gone_seen = false;
                RecordChange::Set(record)
            }
            _ => RecordChange::Keep,
        };
        let after = match kept.held {
            Some(aside) => {
                let now = self.clock.now();
                let base = before.held.clone().unwrap_or_else(|| HeldPlaylist {
                    id: id.clone(),
                    owner: aside.owner.clone(),
                    entries: raw.iter().map(|row| entries::library_entry(row, now)).collect(),
                    mirror: ids_of(raw),
                    pending: None,
                    changed: aside.changed,
                    gone_seen: false,
                    revision: 0,
                });
                let mut after = base.clone();
                after.entries = merge::move_back_entries(&base.entries, &aside.entries);
                after.gone_seen = false;
                self.restamp(Some(&base), &mut after, None);
                Some(after)
            }
            None => before.held.clone(),
        };
        self.commit(guard, &before, after, record, &[])?;
        if let Err(error) = self.store.forget_gone(&id) {
            warn!("PLAYLISTS {id}'s kept-aside files could not be deleted after the move-back: {error}");
        }
        self.aside.lock().remove(&id);
        info!("PLAYLISTS {id} answers again, so its kept-aside entries are back");
        Ok(())
    }

    fn converted(
        &self,
        id: &str,
        owner: &str,
        raw: &[TrackFacts],
        changed: Option<DateTime<Utc>>,
    ) -> HeldPlaylist {
        let now = self.clock.now();
        HeldPlaylist {
            id: id.to_string(),
            owner: owner.to_string(),
            entries: raw.iter().map(|row| entries::library_entry(row, now)).collect(),
            mirror: ids_of(raw),
            pending: None,
            changed: changed.unwrap_or(now),
            gone_seen: false,
            revision: 0,
        }
    }

    /// `octoSongToRemove` on a plain playlist: each `id@n` is the n-th row of that id among the
    /// admin list's present rows (missing files are rows too, so the positions are Navidrome's
    /// own), removed in one native call.
    async fn remove_songs_plain(
        &self,
        id: &str,
        raw: &[TrackFacts],
        songs: &[(String, usize)],
    ) -> Result<usize, HeldError> {
        let mut positions: Vec<String> = Vec::new();
        for (song, n) in songs {
            let Some(at) = n.checked_sub(1) else {
                continue;
            };
            if let Some(row) = raw
                .iter()
                .filter(|row| !row.missing && row.media_file_id == *song)
                .nth(at)
                && !positions.contains(&row.position)
            {
                positions.push(row.position.clone());
            }
        }
        if positions.is_empty() {
            return Ok(0);
        }
        if !matches!(self.nav().remove_positions(id, &positions).await, Ok(true)) {
            return Err(HeldError::Busy);
        }
        Ok(positions.len())
    }

    // ---- the routes' operations -----------------------------------------------------------------

    /// A read: `Ok(None)` when the playlist is not held.
    /// Navidrome's list read without the lock serves the stored entries when it equals the mirror
    /// and nothing is pending; otherwise a locked sync runs, and a lock that misses its deadline
    /// serves the stored entries without a sync. A kept-aside file whose id the caller's relay just
    /// proved alive is moved back.
    pub async fn serve(self: &Arc<Self>, id: &str) -> Result<Option<HeldView>, HeldError> {
        let stored = self.store.get(id);
        let held = stored.is_some();
        let aside = self.aside.lock().contains(id);
        let pending = stored.as_ref().is_some_and(|playlist| playlist.pending.is_some());
        let mirror = stored.map(|playlist| playlist.mirror);
        if !held && !aside {
            return Ok(None);
        }
        if held && !pending {
            match self.read_raw(id).await {
                RawRead::Rows(raw) if Some(ids_of(&raw)) == mirror => return Ok(self.view_of(id)),
                RawRead::Rows(_) => {}
                RawRead::Gone | RawRead::Unreachable => return Ok(self.view_of(id)),
            }
        }
        #[cfg(test)]
        self.park(Park::AfterPreRead).await;
        let synced = self
            .spawn_locked(id, |this, guard| async move {
                this.sync_locked(&guard, &[]).await.map(|_| ())
            })
            .await;
        match synced {
            Ok(()) | Err(HeldError::Busy) => Ok(self.view_of(id)),
            Err(error) => Err(error),
        }
    }

    /// `updatePlaylist`, classified under the lock after the sync: held when its
    /// file exists then; converted when an add streams or takes a `Replaced` entry back; else the
    /// caller's request is relayed while the lock is held (with `octoSongToRemove` taken natively
    /// first).
    pub async fn update(self: &Arc<Self>, request: UpdateRequest) -> Result<UpdateOutcome, HeldError> {
        let id = request.id.clone();
        self.spawn_locked(&id, move |this, guard| async move {
            this.update_locked(guard, request).await
        })
        .await
    }

    async fn update_locked(
        &self,
        guard: PlaylistGuard,
        request: UpdateRequest,
    ) -> Result<UpdateOutcome, HeldError> {
        let synced = self.sync_locked(&guard, &[]).await?;
        #[cfg(test)]
        self.park(Park::AfterSync).await;
        let id = guard.id().to_string();
        let before = self.before(&id);
        let add_only =
            request.remove_indexes.is_empty() && request.remove_songs.is_empty() && request.fields.is_none();
        let mut record = before.replaced.clone();
        let (adds, skipped, took_back) = take_back(request.adds, record.as_mut());
        let converts = took_back || adds.iter().any(streams);
        if before.held.is_none() && !converts {
            if !request.remove_songs.is_empty() {
                self.remove_songs_plain(&id, &synced.raw, &request.remove_songs)
                    .await?;
            }
            let answer = request.plain.await;
            let change = record_rule(before.replaced.as_ref(), record, &request.caller, add_only);
            if !matches!(change, RecordChange::Keep) {
                self.commit(&guard, &before, None, change, &[])?;
            }
            return Ok(UpdateOutcome::Plain(answer));
        }
        let navidrome_changed = synced
            .info
            .as_ref()
            .and_then(|info| parse_instant(info.changed.as_deref()));
        let mut playlist = match before.held.clone() {
            Some(playlist) => playlist,
            None => self.converted(&id, &request.owner, &synced.raw, navidrome_changed),
        };
        if !request.remove_indexes.is_empty() || !request.remove_songs.is_empty() {
            let Some(served) = request.view.served(&playlist.entries).await else {
                return Err(HeldError::Busy);
            };
            let mut gone: Vec<usize> = request
                .remove_indexes
                .iter()
                .filter_map(|at| served.get(*at).copied())
                .collect();
            gone.extend(places_of_songs(&playlist.entries, &served, &request.remove_songs));
            remove_at(&mut playlist.entries, &gone);
        }
        let mut next = playlist.entries.clone();
        next.extend(adds);
        let preapplied = self.rise(&playlist.owner, &next, &playlist.entries)?;
        if let Some(fields) = request.fields {
            match fields.await {
                Some(answer) if !answer.ok => {
                    self.force(&negate(&preapplied));
                    return Ok(UpdateOutcome::Relayed(answer));
                }
                Some(_) => {}
                None => {
                    self.force(&negate(&preapplied));
                    return Err(HeldError::Busy);
                }
            }
        }
        playlist.entries = next;
        let change = record_rule(before.replaced.as_ref(), record, &request.caller, add_only);
        self.apply(
            &guard,
            &before,
            playlist,
            change,
            &preapplied,
            synced.raw,
            navidrome_changed,
        )
        .await?;
        Ok(UpdateOutcome::Held {
            view: self.view_of(&id),
            skipped,
        })
    }

    /// `createPlaylist playlistId=`: the stamp check, then the takeover, the 1.12.0
    /// pairing and the `Replaced` record, under the lock after the sync.
    pub async fn replace(self: &Arc<Self>, request: ReplaceRequest) -> Result<ReplaceOutcome, HeldError> {
        let id = request.id.clone();
        self.spawn_locked(&id, move |this, guard| async move {
            this.replace_locked(guard, request).await
        })
        .await
    }

    async fn replace_locked(
        &self,
        guard: PlaylistGuard,
        request: ReplaceRequest,
    ) -> Result<ReplaceOutcome, HeldError> {
        let synced = self.sync_locked(&guard, &[]).await?;
        #[cfg(test)]
        self.park(Park::AfterSync).await;
        let id = guard.id().to_string();
        let before = self.before(&id);
        let navidrome_changed = synced
            .info
            .as_ref()
            .and_then(|info| parse_instant(info.changed.as_deref()));
        if let Some(stamp) = request.if_stamp {
            let current = match &before.held {
                Some(playlist) => {
                    Some(navidrome_changed.map_or(playlist.changed, |changed| changed.max(playlist.changed)))
                }
                None => request.view.changed().await.ok_or(HeldError::Busy)?,
            };
            if current.is_none_or(|current| !same_instant(current, stamp)) {
                return Err(HeldError::Changed);
            }
        }
        let current_ids: HashSet<&str> = match &before.held {
            Some(playlist) => playlist
                .entries
                .iter()
                .flat_map(|entry| [entry.library_id.as_deref(), entry.outside_id.as_deref()])
                .flatten()
                .collect(),
            None => synced.raw.iter().map(|row| row.media_file_id.as_str()).collect(),
        };
        let takes_back = |id: &str| {
            !current_ids.contains(id)
                && before
                    .replaced
                    .as_ref()
                    .is_some_and(|record| record.entries.iter().any(|entry| answers(entry, id)))
        };
        let converts = before.held.is_some()
            || request.songs.iter().any(|song| match song {
                Built::Entry { entry, .. } => streams(entry) || takes_back(song.id()),
                _ => takes_back(song.id()),
            });
        if !converts {
            let answer = request.plain.await;
            if before.replaced.is_some() {
                self.commit(&guard, &before, None, RecordChange::Clear, &[])?;
            }
            return Ok(ReplaceOutcome::Plain(answer));
        }
        let mut playlist = match before.held.clone() {
            Some(playlist) => playlist,
            None => self.converted(&id, &request.owner, &synced.raw, navidrome_changed),
        };
        let Some(served) = request.view.served(&playlist.entries).await else {
            return Err(HeldError::Busy);
        };
        let mut record = before.replaced.clone();
        let replacement = replace_entries(
            &playlist.entries,
            &served,
            request.songs,
            request.pairing,
            record.as_mut(),
        );
        let preapplied = self.rise(&playlist.owner, &replacement.entries, &playlist.entries)?;
        let removed: Vec<HeldEntry> = replacement
            .removed
            .into_iter()
            .filter(|entry| entry.outside_id.is_some())
            .collect();
        let change = if !request.own_ids && !removed.is_empty() {
            RecordChange::Set(ReplacedRecord {
                playlist: id.clone(),
                by_user: request.caller.user.clone(),
                by_client: request.caller.client.clone(),
                entries: removed,
                revision: 0,
                for_revision: 0,
                gone_seen: false,
            })
        } else if before.replaced.is_some() {
            RecordChange::Clear
        } else {
            RecordChange::Keep
        };
        playlist.entries = replacement.entries;
        self.apply(
            &guard,
            &before,
            playlist,
            change,
            &preapplied,
            synced.raw,
            navidrome_changed,
        )
        .await?;
        Ok(ReplaceOutcome::Held {
            view: self.view_of(&id),
            skipped: replacement.skipped,
        })
    }

    /// `createPlaylist` without `playlistId`, holding an outside song: the intent is
    /// saved, the create relayed as the caller with library ids only, the new playlist held and
    /// the intent deleted, all in one spawned task. A hold that fails leaves the intent for the
    /// attach pass.
    pub async fn create_with_outside(
        self: &Arc<Self>,
        request: CreateRequest,
    ) -> Result<CreateOutcome, HeldError> {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.create_task(request).await })
            .await
            .unwrap_or(Err(HeldError::Busy))
    }

    async fn create_task(&self, request: CreateRequest) -> Result<CreateOutcome, HeldError> {
        let preapplied = self.rise(&request.owner, &request.entries, &[])?;
        let intent = Creating {
            owner: request.owner.clone(),
            name: request.name.clone(),
            entries: request.entries.clone(),
            started: self.clock.now(),
            before: request.before.clone(),
        };
        // The intent is saved and marked live under one lock, so an attach pass sees it live or not
        // at all.
        let saved = {
            let mut live = self.live_creates.lock();
            let saved = if self.writes_fail() {
                Err(anyhow::anyhow!("writes fail in this test"))
            } else {
                self.store.add_intent(intent)
            };
            if let Ok(n) = &saved {
                live.insert(*n);
            }
            saved
        };
        let n = match saved {
            Ok(n) => n,
            Err(error) => {
                warn!(
                    "PLAYLISTS the new playlist {} could not be saved: {error}",
                    request.name
                );
                self.force(&negate(&preapplied));
                return Err(HeldError::SaveFailed);
            }
        };
        let made = request.relay.await;
        let outcome = match made {
            Err(answer) => {
                self.force(&negate(&preapplied));
                self.forget_intent(n);
                Ok(CreateOutcome::Relayed(answer))
            }
            Ok(new_id) => match self
                .hold_new(&new_id, &request.owner, request.entries, &preapplied)
                .await
            {
                Ok(view) => {
                    self.forget_intent(n);
                    Ok(CreateOutcome::Held {
                        view,
                        skipped: request.skipped,
                    })
                }
                Err(error) => Err(error),
            },
        };
        self.live_creates.lock().remove(&n);
        outcome
    }

    fn forget_intent(&self, n: u64) {
        if let Err(error) = self.store.drop_intent(n) {
            warn!("PLAYLISTS a playlist being made could not be forgotten: {error}");
        }
    }

    /// Holds a playlist Navidrome just made: its entries in order, the mirror what Navidrome
    /// holds (merged when that is not the library part). `preapplied` (the caller's rise) is
    /// reversed on any failure.
    async fn hold_new(
        &self,
        id: &str,
        owner: &str,
        entries: Vec<HeldEntry>,
        preapplied: &[(String, i64)],
    ) -> Result<Option<HeldView>, HeldError> {
        let guard = match self.lock(id).await {
            Ok(guard) => guard,
            Err(error) => {
                self.force(&negate(preapplied));
                return Err(error);
            }
        };
        let RawRead::Rows(raw) = self.read_raw(id).await else {
            self.force(&negate(preapplied));
            return Err(HeldError::Busy);
        };
        let library = library_ids(&entries);
        let mut playlist = HeldPlaylist {
            id: id.to_string(),
            owner: owner.to_string(),
            entries,
            mirror: library.clone(),
            pending: None,
            changed: self.clock.now(),
            gone_seen: false,
            revision: 0,
        };
        if ids_of(&raw) != library {
            match self.merge_raw(&playlist.entries, &library, &raw).await {
                Ok(merged) => playlist.entries = merged,
                Err(error) => {
                    self.force(&negate(preapplied));
                    return Err(error);
                }
            }
        }
        playlist.mirror = ids_of(&raw);
        let before = self.before(id);
        self.apply(
            &guard,
            &before,
            playlist,
            RecordChange::Keep,
            preapplied,
            raw,
            None,
        )
        .await?;
        Ok(self.view_of(id))
    }

    /// The attach pass: intents whose task is not running, oldest first, each take
    /// the owner's oldest playlist of that name that is not in the intent's `before` set, not
    /// held and not claimed earlier in the pass, whose tracks equal the intent's library part. An
    /// intent with no candidate is deleted; one whose hold failed waits for the next pass.
    pub async fn attach_pass(&self, owner: Option<&str>, listing: Vec<ListedPlaylist>) {
        let intents: Vec<(u64, Creating)> = {
            let live = self.live_creates.lock();
            self.store
                .intents()
                .into_iter()
                .filter(|(n, intent)| {
                    !live.contains(n) && owner.is_none_or(|owner| eq_ignore_case(owner, &intent.owner))
                })
                .collect()
        };
        let mut claimed: HashSet<String> = HashSet::new();
        for (n, intent) in intents {
            let mut candidates: Vec<&ListedPlaylist> = listing
                .iter()
                .filter(|listed| {
                    eq_ignore_case(&listed.owner, &intent.owner)
                        && listed.name.trim() == intent.name.trim()
                        && !intent.before.contains(&listed.id)
                        && !claimed.contains(&listed.id)
                        && !self.is_held_id(&listed.id)
                })
                .collect();
            candidates.sort_by_key(|listed| (listed.created.is_none(), listed.created));
            let library = library_ids(&intent.entries);
            let mut failed = false;
            for candidate in candidates {
                let Ok(Some(rows)) = self.nav().read_track_facts(&candidate.id).await else {
                    continue;
                };
                if ids_of(&rows) != library {
                    continue;
                }
                claimed.insert(candidate.id.clone());
                failed = self
                    .hold_new(&candidate.id, &intent.owner, intent.entries.clone(), &[])
                    .await
                    .is_err();
                break;
            }
            if !failed {
                self.forget_intent(n);
            }
        }
    }

    /// `deletePlaylist`, under the lock: the relay, then on ok the held file, its
    /// record and a kept-aside file go. The route removes the caller's placement after.
    pub async fn delete(self: &Arc<Self>, id: &str, relay: Relay) -> Result<Option<RelayAnswer>, HeldError> {
        self.spawn_locked(id, move |this, guard| async move {
            let answer = relay.await;
            if answer.as_ref().is_some_and(|answer| answer.ok) {
                let id = guard.id().to_string();
                let before = this.before(&id);
                if before.held.is_some() || before.replaced.is_some() {
                    let record = if before.replaced.is_some() {
                        RecordChange::Clear
                    } else {
                        RecordChange::Keep
                    };
                    this.commit(&guard, &before, None, record, &[])?;
                }
                let aside = this.aside.lock().remove(&id);
                if aside && let Err(error) = this.store.forget_gone(&id) {
                    warn!("PLAYLISTS {id}'s kept-aside files could not be deleted: {error}");
                }
            }
            Ok(answer)
        })
        .await
    }

    /// `octoSongToRemove` on a playlist that is not held, under the lock after the
    /// owner check. `Ok(None)` when the playlist is held (the route updates it instead).
    pub async fn remove_by_song_plain(
        self: &Arc<Self>,
        id: &str,
        songs: Vec<(String, usize)>,
    ) -> Result<Option<usize>, HeldError> {
        self.spawn_locked(id, move |this, guard| async move {
            let synced = this.sync_locked(&guard, &[]).await?;
            if this.is_held(&guard) {
                return Ok(None);
            }
            this.remove_songs_plain(guard.id(), &synced.raw, &songs)
                .await
                .map(Some)
        })
        .await
    }

    /// `DELETE api/playlist/{id}/tracks?id=<place>...` on a held playlist: `places` are 1-based
    /// over the caller's served list. `Ok(None)` when the playlist is not held (the route relays).
    pub async fn native_remove(
        self: &Arc<Self>,
        id: &str,
        caller: HeldCaller,
        places: Vec<usize>,
        view: Arc<dyn ServedView>,
    ) -> Result<Option<HeldView>, HeldError> {
        self.spawn_locked(id, move |this, guard| async move {
            let synced = this.sync_locked(&guard, &[]).await?;
            let id = guard.id().to_string();
            let before = this.before(&id);
            let Some(mut playlist) = before.held.clone() else {
                return Ok(None);
            };
            let served = view.served(&playlist.entries).await.ok_or(HeldError::Busy)?;
            let gone: Vec<usize> = places
                .iter()
                .filter_map(|place| place.checked_sub(1).and_then(|at| served.get(at).copied()))
                .collect();
            remove_at(&mut playlist.entries, &gone);
            let change = record_rule(before.replaced.as_ref(), before.replaced.clone(), &caller, false);
            let changed = synced
                .info
                .as_ref()
                .and_then(|info| parse_instant(info.changed.as_deref()));
            this.apply(&guard, &before, playlist, change, &[], synced.raw, changed)
                .await?;
            Ok(this.view_of(&id))
        })
        .await
    }

    /// `PUT api/playlist/{id}/tracks/{place}` with `insert_before` on a held playlist: the entry at
    /// 1-based served `place` moves so it sits at served `insert_before`. `Ok(None)` when the
    /// playlist is not held.
    pub async fn native_move(
        self: &Arc<Self>,
        id: &str,
        caller: HeldCaller,
        place: usize,
        insert_before: usize,
        view: Arc<dyn ServedView>,
    ) -> Result<Option<HeldView>, HeldError> {
        self.spawn_locked(id, move |this, guard| async move {
            let synced = this.sync_locked(&guard, &[]).await?;
            let id = guard.id().to_string();
            let before = this.before(&id);
            let Some(mut playlist) = before.held.clone() else {
                return Ok(None);
            };
            let served = view.served(&playlist.entries).await.ok_or(HeldError::Busy)?;
            let Some(from_at) = place.checked_sub(1) else {
                return Ok(this.view_of(&id));
            };
            let Some(from) = served.get(from_at).copied() else {
                return Ok(this.view_of(&id));
            };
            let anchor = insert_before
                .checked_sub(1)
                .and_then(|at| served.get(at).copied().map(|entry| (at, entry)));
            let entry = playlist.entries.remove(from);
            let at = match anchor {
                Some((served_at, anchor)) if served_at > from_at => {
                    let anchor = if anchor > from { anchor - 1 } else { anchor };
                    anchor + 1
                }
                Some((_, anchor)) => {
                    if anchor > from {
                        anchor - 1
                    } else {
                        anchor
                    }
                }
                None => playlist.entries.len(),
            };
            playlist.entries.insert(at.min(playlist.entries.len()), entry);
            let change = record_rule(before.replaced.as_ref(), before.replaced.clone(), &caller, false);
            let changed = synced
                .info
                .as_ref()
                .and_then(|info| parse_instant(info.changed.as_deref()));
            this.apply(&guard, &before, playlist, change, &[], synced.raw, changed)
                .await?;
            Ok(this.view_of(&id))
        })
        .await
    }

    // ---- other writers ---------------------------------------------------------------------------

    /// Library health's keep-marks swap on a held playlist, under its lock: every library entry of
    /// `from` (or only those at the Navidrome positions in `only`) becomes `to` in its place,
    /// keeping its outside fallback; one mirror write. Answers the positions moved.
    pub async fn swap_library(
        &self,
        guard: &PlaylistGuard,
        from: &str,
        to: &str,
        only: Option<&[usize]>,
    ) -> Result<Vec<usize>, HeldError> {
        let synced = self.sync_locked(guard, &[]).await?;
        let before = self.before(guard.id());
        let Some(mut playlist) = before.held.clone() else {
            return Ok(Vec::new());
        };
        let mut moved = Vec::new();
        let mut position = 0;
        for entry in playlist.entries.iter_mut() {
            if entry.library_id.is_none() {
                continue;
            }
            position += 1;
            if entry.library_id.as_deref() == Some(from) && only.is_none_or(|only| only.contains(&position)) {
                entry.library_id = Some(to.to_string());
                moved.push(position);
            }
        }
        if moved.is_empty() {
            return Ok(moved);
        }
        let changed = synced
            .info
            .as_ref()
            .and_then(|info| parse_instant(info.changed.as_deref()));
        self.apply(
            guard,
            &before,
            playlist,
            RecordChange::Keep,
            &[],
            synced.raw,
            changed,
        )
        .await?;
        Ok(moved)
    }

    /// The import keeper's list for a held kept playlist, under its lock: merged with `wanted` as
    /// Navidrome's list (streamed entries keep their places by their neighbors), then one mirror
    /// write.
    pub async fn apply_library_list(
        &self,
        guard: &PlaylistGuard,
        wanted: &[String],
    ) -> Result<(), HeldError> {
        let synced = self.sync_locked(guard, &[]).await?;
        let before = self.before(guard.id());
        let Some(mut playlist) = before.held.clone() else {
            return Ok(());
        };
        let rows: Vec<RawRow> = wanted
            .iter()
            .enumerate()
            .map(|(at, id)| RawRow {
                position: (at + 1).to_string(),
                id: id.clone(),
                title: String::new(),
                artist: String::new(),
                seconds: None,
                missing: false,
            })
            .collect();
        let needed = merge::ids_needing_presence(&playlist.entries, &playlist.mirror, &rows);
        let present = self.presence(needed).await;
        playlist.entries = merge::merge(&playlist.entries, &playlist.mirror, &rows, &present)
            .map_err(|_| HeldError::Busy)?;
        let changed = synced
            .info
            .as_ref()
            .and_then(|info| parse_instant(info.changed.as_deref()));
        self.apply(
            guard,
            &before,
            playlist,
            RecordChange::Keep,
            &[],
            synced.raw,
            changed,
        )
        .await?;
        Ok(())
    }

    // ---- binding --------------------------------------------------------------------------------

    /// The held playlists an ended acquisition row may bind in: a streamed entry with its key, or
    /// (rule two) one whose song shares the row's match key.
    pub(crate) fn playlists_for_end(&self, end: &AcquisitionEnd) -> Vec<String> {
        let by_song = match (end.artist.as_deref(), end.title.as_deref()) {
            (Some(artist), Some(title)) => Some(SongIdentity::match_key(artist, title)),
            _ => None,
        };
        self.store
            .list()
            .into_iter()
            .filter(|playlist| {
                playlist.entries.iter().any(|entry| {
                    streams(entry)
                        && (entry.key.as_deref() == Some(end.key.as_str())
                            || by_song.as_ref().is_some_and(|key| {
                                entry.song.as_ref().is_some_and(|song| {
                                    SongIdentity::match_key(&song.artist, &song.title) == *key
                                })
                            }))
                })
            })
            .map(|playlist| playlist.id)
            .collect()
    }

    /// One locked operation binding a playlist's entries for these ended rows (and the sync's own
    /// bind check), with one mirror write.
    pub(crate) async fn bind_now(self: &Arc<Self>, id: &str, events: &[AcquisitionEnd]) {
        let events = events.to_vec();
        let bound = self
            .spawn_locked(id, move |this, guard| async move {
                this.sync_locked(&guard, &events).await.map(|_| ())
            })
            .await;
        if let Err(error) = bound {
            info!("PLAYLISTS {id}: binding waits for the next sync ({error:?})");
        }
    }

    /// The start sweep: a locked sync of every held playlist (seed copies refreshed with `peek`,
    /// the bind check), then the family seed copies refreshed. Resolves when done.
    pub async fn start_sweep(self: &Arc<Self>) {
        for id in self.held_ids() {
            let swept = self
                .spawn_locked(&id, |this, guard| async move {
                    this.sync_locked(&guard, &[]).await.map(|_| ())
                })
                .await;
            if let Err(error) = swept {
                info!("PLAYLISTS {id} was not swept at start ({error:?})");
            }
        }
        if let Some(family) = &self.family {
            family.refresh_seeds();
            family.publish_kept();
        }
    }

    /// The worker: the start sweep and the attach pass, then the store's flush pass each time a
    /// write past the commit point failed, and once more on shutdown.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        self.start_sweep().await;
        if let Some(listing) = self.nav().list_playlists().await {
            self.attach_pass(None, listing).await;
        }
        loop {
            tokio::select! {
                () = stopping.cancelled() => break,
                () = self.flush_wanted.notified() => {}
            }
            self.flush().await;
        }
        self.flush().await;
        Ok(())
    }

    // ---- gone playlists --------------------------------------------------------------------------

    /// Starts a gone check for `owner` after their `getPlaylists` (`listed`: the ids the answer
    /// listed; `placed`: the playlist ids the owner has placed in folders). One check runs per
    /// owner; `None` when one is already in flight.
    pub fn check_gone(
        self: &Arc<Self>,
        owner: &str,
        listed: Vec<String>,
        placed: Vec<String>,
    ) -> Option<tokio::task::JoinHandle<GoneReport>> {
        let owner = owner_key(owner);
        if !self.gone_running.lock().insert(owner.clone()) {
            return None;
        }
        let this = Arc::clone(self);
        Some(tokio::spawn(async move {
            struct Running(Arc<HeldPlaylists>, String);
            impl Drop for Running {
                fn drop(&mut self) {
                    self.0.gone_running.lock().remove(&self.1);
                }
            }
            let _running = Running(Arc::clone(&this), owner.clone());
            this.gone_pass(&owner, listed, placed).await
        }))
    }

    async fn gone_pass(&self, owner: &str, listed: Vec<String>, placed: Vec<String>) -> GoneReport {
        let listed: HashSet<String> = listed.into_iter().collect();
        if self.nav().list_playlists().await.is_none() {
            return GoneReport::default();
        }
        let held = self.store.list();
        let mut missing = Vec::new();
        let mut seen_again = Vec::new();
        for playlist in held.iter().filter(|playlist| owner_key(&playlist.owner) == owner) {
            if !listed.contains(&playlist.id) {
                missing.push(playlist.id.clone());
            } else if playlist.gone_seen {
                seen_again.push(playlist.id.clone());
            }
        }
        for record in self.store.records() {
            if owner_key(&record.by_user) == owner
                && !held.iter().any(|playlist| playlist.id == record.playlist)
                && !listed.contains(&record.playlist)
            {
                missing.push(record.playlist);
            }
        }
        let aside: Vec<String> = self
            .aside
            .lock()
            .iter()
            .filter(|id| listed.contains(*id))
            .cloned()
            .collect();
        for id in missing {
            let Ok(guard) = self.lock(&id).await else {
                continue;
            };
            match self.nav().playlist_info(&id).await {
                Ok(None) => self.gone_once_more(&guard),
                Ok(Some(_)) => self.found_again(&guard),
                Err(_) => {}
            }
        }
        for id in seen_again {
            if let Ok(guard) = self.lock(&id).await {
                self.found_again(&guard);
            }
        }
        for id in aside {
            let Ok(guard) = self.lock(&id).await else {
                continue;
            };
            if let RawRead::Rows(raw) = self.read_raw(&id).await
                && let Err(error) = self.move_back(&guard, &raw).await
            {
                info!("PLAYLISTS {id} could not be moved back yet ({error:?})");
            }
        }
        let mut report = GoneReport::default();
        for id in placed {
            let seen_key = format!("{owner}|{id}");
            if !is_navidrome_shaped(&id) || listed.contains(&id) {
                self.placements_seen.lock().remove(&seen_key);
                continue;
            }
            match self.nav().playlist_info(&id).await {
                Ok(None) => {
                    let mut seen = self.placements_seen.lock();
                    if seen.remove(&seen_key) {
                        report.confirmed.push(id);
                    } else {
                        seen.insert(seen_key);
                    }
                }
                Ok(Some(_)) => {
                    self.placements_seen.lock().remove(&seen_key);
                }
                Err(_) => {}
            }
        }
        report
    }

    /// A 404 for a held playlist or record: the first sets `GoneSeen`; a second moves its files to
    /// `gone/` (kept aside, never deleted), releasing its keys and its counts.
    fn gone_once_more(&self, guard: &PlaylistGuard) {
        let id = guard.id();
        let before = self.before(id);
        let seen = before.held.as_ref().map_or_else(
            || before.replaced.as_ref().is_some_and(|record| record.gone_seen),
            |playlist| playlist.gone_seen,
        );
        if !seen {
            let after = before.held.clone().map(|mut playlist| {
                playlist.gone_seen = true;
                playlist
            });
            let record = match (&before.held, &before.replaced) {
                (None, Some(record)) => {
                    let mut record = record.clone();
                    record.gone_seen = true;
                    RecordChange::Set(record)
                }
                _ => RecordChange::Keep,
            };
            let _ = self.commit(guard, &before, after, record, &[]);
            return;
        }
        let moved = match self.store.move_to_gone(id) {
            Ok(moved) => moved,
            Err(error) => {
                warn!("PLAYLISTS {id} could not be kept aside: {error}");
                return;
            }
        };
        self.aside.lock().insert(id.to_string());
        self.unsaved.lock().remove(id);
        let revision = self.store.take_revision();
        self.publish(&held_key(id), revision, &[]);
        self.publish(&replaced_key(id), revision, &[]);
        self.force(&count_deltas(moved.held.as_ref(), None));
        info!("PLAYLISTS {id} is gone from Navidrome, so its file is kept aside");
    }

    fn found_again(&self, guard: &PlaylistGuard) {
        let before = self.before(guard.id());
        if let Some(playlist) = before.held.clone()
            && playlist.gone_seen
        {
            let mut after = playlist;
            after.gone_seen = false;
            let _ = self.commit(guard, &before, Some(after), RecordChange::Keep, &[]);
        } else if before.held.is_none()
            && let Some(record) = before.replaced.clone()
            && record.gone_seen
        {
            let mut record = record;
            record.gone_seen = false;
            let _ = self.commit(guard, &before, None, RecordChange::Set(record), &[]);
        }
    }
}

#[cfg(test)]
impl HeldPlaylists {
    /// Parks the next task that reaches `at`: the first channel resolves once it is there, and it
    /// goes on when the second is sent to (or dropped).
    pub(crate) fn park_at(
        &self,
        at: Park,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel();
        self.hooks.parks.lock().insert(at, (reached_tx, go_rx));
        (reached_rx, go_tx)
    }

    async fn park(&self, at: Park) {
        let parked = self.hooks.parks.lock().remove(&at);
        if let Some((reached, go)) = parked {
            let _ = reached.send(());
            let _ = go.await;
        }
    }

    /// Every write to the held store fails while set.
    pub(crate) fn fail_writes(&self, fail: bool) {
        self.hooks
            .fail_writes
            .store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn stored(&self, id: &str) -> Option<HeldPlaylist> {
        self.store.get(id)
    }

    pub(crate) fn is_aside(&self, id: &str) -> bool {
        self.aside.lock().contains(id)
    }
}

#[cfg(test)]
#[path = "held_playlists_tests.rs"]
mod tests;
