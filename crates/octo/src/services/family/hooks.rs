//! The hooks Octo runs the family service with: each trait in [`FamilyHooks`] over the service
//! that does the work. Requests go to the request service, members' folders and storage to
//! their personal libraries, a removed member's files through the family pool, live streams to
//! the stream tracker, the unmarked count to the library index (and "Mark them now" to the
//! explicit marks run), and family playlists through the one set of rules in
//! [`family_playlists`].

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::family::abilities::RequestQuality;
use octo_core::family::request::{FamilyRequest, RequestState};
use octo_core::json::datetime;
use tracing::warn;

use super::caller::FamilyCaller;
use super::family_playlist_store::{FamilyPlaylist, FamilyPlaylistEntry, FamilyPlaylistStore};
use super::family_playlists::{self, PlaylistAddRequests, PlaylistError};
use super::family_service::{
    FamilyError, FamilyPlaylistSummary, FamilyPlaylists, LiveStreams, LoadReport, LoadStream, MemberFiles,
    MyLibraryPage, MySong, NewRequest, PersonalFolders, Playing, QueueLoad, RemoveFromMyLibrary,
    RequestService, StorageUsage, UnratedCount,
};
use super::family_store::FamilyStore;
use super::kid_filter;
use super::library_index::{LibraryIndex, ScopeResolver};
use super::personal_library::{LibraryError, PersonalLibrary};
use super::pool::{FamilyPool, FilesChoice, LinkMode, NotMarked};
use super::request_service::{RequestError, RequestService as Requests, facts_from_path};
use super::stream_tracker::{LiveStreamView, SongName, StreamTracker};
use crate::services::common::AcquisitionTracker;
use crate::services::common::acquisition_tracker::AcquisitionState;
use crate::services::library::explicit_backfill::{
    ExplicitBackfill, ExplicitBackfillMode, ExplicitBackfillRequest,
};
use crate::services::soulseek::ExternalIdRegistry;

/// A request error in the family's words: 10 missing or wrong, 50 not allowed, 70 not found.
pub fn request_failure(e: RequestError) -> FamilyError {
    match e.code {
        10 => FamilyError::Invalid(e.message),
        50 => FamilyError::Refused(e.message),
        70 => FamilyError::NotFound(e.message),
        _ => FamilyError::Unavailable(e.message),
    }
}

fn library_failure(e: LibraryError) -> FamilyError {
    match e.code {
        50 => FamilyError::Refused(e.message),
        70 => FamilyError::NotFound(e.message),
        _ => FamilyError::Unavailable(e.message),
    }
}

// ------------------------------------------------------------------------------------------
// Requests
// ------------------------------------------------------------------------------------------

/// Requests through the request service.
pub struct LiveRequests {
    pub service: Arc<Requests>,
}

#[async_trait]
impl RequestService for LiveRequests {
    async fn create(&self, ask: NewRequest) -> Result<FamilyRequest, FamilyError> {
        // An MP3 asked for a song whose FLAC request failed is that request tried again.
        if ask.quality == RequestQuality::Mp3
            && let Some(failed) = self
                .service
                .requests_of(&ask.username)
                .into_iter()
                .rev()
                .find(|r| {
                    r.target == ask.target
                        && r.state == RequestState::Failed
                        && r.quality == RequestQuality::Flac
                        && ask.kind.is_none_or(|kind| kind == r.kind)
                })
        {
            return self.retry_as_mp3(&failed.id, &ask.username).await;
        }
        self.service
            .create(&ask.username, &ask.target, ask.quality, ask.kind)
            .await
            .map_err(request_failure)
    }

    async fn cancel(&self, id: &str, by: &str) -> Result<FamilyRequest, FamilyError> {
        self.service.cancel(id, by).map_err(request_failure)
    }

    async fn decide(
        &self,
        id: &str,
        approve: bool,
        by: &str,
        note: &str,
    ) -> Result<FamilyRequest, FamilyError> {
        let note = Some(note).filter(|n| !n.trim().is_empty());
        if approve {
            self.service.approve(id, by, note)
        } else {
            self.service.decline(id, by, note)
        }
        .map_err(request_failure)
    }

    async fn retry_as_mp3(&self, id: &str, by: &str) -> Result<FamilyRequest, FamilyError> {
        self.service.retry_as_mp3(id, by).await.map_err(request_failure)
    }
}

/// Family playlist additions that wait for approval become playlist addition requests.
pub struct RequestPlaylistAdditions {
    pub service: Weak<Requests>,
}

impl PlaylistAddRequests for RequestPlaylistAdditions {
    fn addition_waits(
        &self,
        playlist: &FamilyPlaylist,
        _index: usize,
        entry: &FamilyPlaylistEntry,
        by: &str,
    ) {
        let Some(service) = self.service.upgrade() else {
            return;
        };
        let Some(song) = entry
            .library_id
            .as_deref()
            .or(entry.outside_id.as_deref())
            .filter(|id| !id.trim().is_empty())
        else {
            return;
        };
        if let Err(e) = service.create_playlist_addition(by, &playlist.id, &playlist.name, song) {
            warn!(
                "FAMILY the addition of {song} to {} by {by} could not be asked for: {}",
                playlist.id, e.message
            );
        }
    }
}

// ------------------------------------------------------------------------------------------
// Members' own libraries, folders and files
// ------------------------------------------------------------------------------------------

/// Members' folders as their personal libraries keep them, and the pool's link probe.
pub struct PoolFolders {
    pub personal: Arc<PersonalLibrary>,
}

#[async_trait]
impl PersonalFolders for PoolFolders {
    async fn create(&self, username: &str) -> Result<String, String> {
        let folder = self.personal.folder_of(username);
        self.personal
            .ensure_folder(username)
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|e| format!("Octo could not make {}: {e}", folder.display()))
    }

    async fn remove_if_empty(&self, username: &str) {
        let _ = std::fs::remove_dir(self.personal.folder_of(username));
    }

    /// Also writes the pool's marker file when the pool folder has none yet and plainly is the
    /// pool ([`FamilyPool::keep_marker`]).
    async fn probe_links(&self) -> LinkMode {
        let pool = Arc::clone(self.personal.pool());
        tokio::task::spawn_blocking(move || {
            pool.keep_marker();
            pool.probe_links().links
        })
        .await
        .unwrap_or(LinkMode::Copy)
    }

    /// Writes the marker only where the pool folder plainly is the pool
    /// ([`FamilyPool::mark_when_plainly_ours`]): over records whose files are all missing, Family
    /// stays off and the owner is told the members disk may not be mounted.
    async fn mark_pool(&self) -> Result<(), String> {
        let pool = Arc::clone(self.personal.pool());
        let root = pool.pool_root_path();
        let marked = tokio::task::spawn_blocking(move || pool.mark_when_plainly_ours())
            .await
            .map_err(|e| format!("Octo could not write its marker file in {root} ({e})."))?;
        marked.map_err(|e| match e {
            NotMarked::DiskMissing { recorded } => format!(
                "Octo cannot find any of the {recorded} family downloads it recorded in {root}, so the members disk may not be mounted. Mount it, then turn Family on again."
            ),
            NotMarked::Io(e) => format!("Octo could not write its marker file in {root} ({e})."),
        })
    }

    fn mode(&self) -> Option<LinkMode> {
        Some(self.personal.pool().mode())
    }
}

/// A removed member's files, through the family pool.
pub struct PoolFiles {
    pub pool: Arc<FamilyPool>,
}

#[async_trait]
impl MemberFiles for PoolFiles {
    async fn release(&self, username: &str, choice: FilesChoice) -> Result<(), FamilyError> {
        self.pool
            .release_member(username, choice)
            .await
            .map(|_| ())
            .map_err(|e| FamilyError::Unavailable(format!("Octo could not move their files: {e}")))
    }
}

/// How much each member's own library holds.
pub struct PersonalStorage {
    pub personal: Arc<PersonalLibrary>,
}

impl StorageUsage for PersonalStorage {
    fn used_bytes(&self, username: &str) -> u64 {
        self.personal.storage_used(username)
    }
}

/// A member's own library: its songs as the index knows them, and taking one out.
pub struct PersonalSongs {
    pub personal: Arc<PersonalLibrary>,
}

/// When a file was put where it is (a link's own time, not its target's).
fn placed_at(path: &str) -> Option<String> {
    let modified = std::fs::symlink_metadata(path).ok()?.modified().ok()?;
    Some(datetime::format_utc(&DateTime::<Utc>::from(modified)))
}

#[async_trait]
impl RemoveFromMyLibrary for PersonalSongs {
    fn list(&self, username: &str, offset: usize, size: usize) -> MyLibraryPage {
        let mut songs = self.personal.songs(username);
        songs.sort_by(|a, b| a.path.cmp(&b.path));
        let total = songs.len();
        let songs = songs
            .into_iter()
            .skip(offset)
            .take(size)
            .map(|song| {
                let names = facts_from_path(&song.path);
                MySong {
                    id: song.id.clone(),
                    title: names.title,
                    artist: names.artist,
                    album: names.album,
                    size: song.size,
                    added: placed_at(&song.path),
                }
            })
            .collect();
        MyLibraryPage { songs, total }
    }

    async fn remove(&self, username: &str, song_id: &str) -> Result<(), FamilyError> {
        self.personal
            .remove_from_my_library(username, song_id)
            .await
            .map_err(library_failure)
    }
}

// ------------------------------------------------------------------------------------------
// Live streams
// ------------------------------------------------------------------------------------------

/// A song's title and artist as Octo holds them in memory, without asking anyone: the library
/// index (the file's place in the library), the outside id registry, or a download of it.
pub fn known_names(
    index: &LibraryIndex,
    registry: &ExternalIdRegistry,
    acquisitions: &AcquisitionTracker,
    song_id: &str,
) -> Option<SongName> {
    if let Some(song) = index.song(song_id) {
        let names = facts_from_path(&song.path);
        return Some(SongName {
            title: names.title,
            artist: names.artist,
        });
    }
    if let Some(routing) = registry.lookup(song_id) {
        let routing = routing.snapshot();
        if let Some(title) = routing.title.filter(|t| !t.trim().is_empty()) {
            return Some(SongName {
                title,
                artist: routing.artist.unwrap_or_default(),
            });
        }
    }
    let fetched = acquisitions.all().into_iter().find(|a| {
        !a.external_id.is_empty()
            && (song_id.ends_with(&format!("-{}", a.external_id)) || song_id == a.external_id)
    })?;
    let title = fetched.title.filter(|t| !t.trim().is_empty())?;
    Some(SongName {
        title,
        artist: fetched.artist.unwrap_or_default(),
    })
}

/// What is playing, from the stream tracker. Song names come from what Octo holds in memory
/// ([`known_names`]), else from the lookup that began when an outside song's stream started.
pub struct TrackerStreams {
    pub tracker: Arc<StreamTracker>,
    pub index: Arc<LibraryIndex>,
    pub registry: Arc<ExternalIdRegistry>,
    pub acquisitions: Arc<AcquisitionTracker>,
}

impl TrackerStreams {
    /// A stream's title and artist; the song id stands in while neither is known.
    fn names(&self, stream: &LiveStreamView) -> (String, String) {
        known_names(&self.index, &self.registry, &self.acquisitions, &stream.song_id)
            .or_else(|| stream.name.clone())
            .map_or_else(
                || (stream.song_id.clone(), String::new()),
                |name| (name.title, name.artist),
            )
    }

    fn playing_now(&self) -> Vec<LiveStreamView> {
        self.tracker
            .live()
            .into_iter()
            .filter(|stream| stream.purpose.is_playing())
            .collect()
    }
}

impl LiveStreams for TrackerStreams {
    fn playing(&self, device_id: &str) -> Option<Playing> {
        let stream = self
            .playing_now()
            .into_iter()
            .rev()
            .find(|stream| stream.device_id.as_deref() == Some(device_id))?;
        let (title, artist) = self.names(&stream);
        Some(Playing {
            song_id: stream.song_id,
            title,
            artist,
        })
    }

    fn user_playing(&self, username: &str) -> bool {
        self.playing_now()
            .iter()
            .any(|stream| eq_ignore_case(&stream.username, username))
    }

    fn count(&self) -> usize {
        self.tracker.len()
    }

    fn load(&self) -> LoadReport {
        let streams: Vec<LoadStream> = self
            .tracker
            .live()
            .into_iter()
            .map(|stream| LoadStream {
                song_title: self.names(&stream).0,
                username: stream.username,
                device: stream.device_name,
                kbps: stream.kbps,
                place: stream.place,
            })
            .collect();
        let upload_kbps = streams.iter().map(|s| s.kbps).sum();
        let mut queue: Vec<QueueLoad> = Vec::new();
        for fetching in self.acquisitions.all() {
            if matches!(fetching.state, AcquisitionState::Done | AcquisitionState::Failed) {
                continue;
            }
            for user in &fetching.requested_by {
                match queue.iter_mut().find(|q| eq_ignore_case(&q.username, user)) {
                    Some(row) => row.waiting += 1,
                    None => queue.push(QueueLoad {
                        username: user.clone(),
                        waiting: 1,
                    }),
                }
            }
        }
        LoadReport {
            streams,
            queue,
            upload_kbps,
            ..LoadReport::default()
        }
    }
}

// ------------------------------------------------------------------------------------------
// Explicit marks
// ------------------------------------------------------------------------------------------

/// Unmarked shared songs counted from the library index; marking them starts the explicit
/// marks run over the shared library.
pub struct IndexUnrated {
    pub index: Arc<LibraryIndex>,
    pub scope: ScopeResolver,
    pub backfill: Arc<ExplicitBackfill>,
}

#[async_trait]
impl UnratedCount for IndexUnrated {
    fn unrated(&self) -> Option<u64> {
        if !self.scope.ready() {
            return None;
        }
        Some(kid_filter::unrated_count(&self.index, &self.scope) as u64)
    }

    async fn mark_now(&self, by: &str) -> Result<String, FamilyError> {
        if self.backfill.is_busy() {
            return Ok("Octo is already looking songs up for explicit marks.".into());
        }
        if !self
            .backfill
            .try_enqueue(ExplicitBackfillRequest::new(ExplicitBackfillMode::Preview, by))
        {
            return Err(FamilyError::Unavailable(
                "Octo could not start looking the songs up. Try again in a minute.".into(),
            ));
        }
        Ok("Octo is looking the songs up now. When it is done, open Tags and genres in the dashboard and write the marks it found.".into())
    }
}

// ------------------------------------------------------------------------------------------
// Family playlists
// ------------------------------------------------------------------------------------------

/// Family playlists through the rules every other family playlist path uses.
pub struct RulePlaylists {
    pub store: Arc<FamilyPlaylistStore>,
    pub family: Arc<FamilyStore>,
}

/// The person `user` is, as the family playlist rules see them.
pub fn caller_for(family: &FamilyStore, user: &str) -> FamilyCaller {
    match family.member(user) {
        Some(member) if !member.suspended => {
            FamilyCaller::member(&member.username, member.preset, member.abilities())
        }
        Some(member) => FamilyCaller::unmanaged(&member.username, false),
        None => FamilyCaller::unmanaged(user, family.is_owner(user)),
    }
}

fn playlist_failure(e: PlaylistError) -> FamilyError {
    match e {
        PlaylistError::NotFound => FamilyError::NotFound("There is no such family playlist.".into()),
        PlaylistError::NotAllowed(message) => FamilyError::Refused(message.into()),
        PlaylistError::BlankName => FamilyError::Invalid("Give the playlist a name.".into()),
        PlaylistError::Changed => FamilyError::Conflict(PlaylistError::Changed.message().into()),
        PlaylistError::Refused(refused) => FamilyError::Conflict(refused.message().into()),
    }
}

impl RulePlaylists {
    fn summary(playlist: &FamilyPlaylist, viewer: &FamilyCaller) -> FamilyPlaylistSummary {
        FamilyPlaylistSummary {
            id: playlist.id.clone(),
            name: playlist.name.clone(),
            song_count: family_playlists::visible_count(playlist, viewer),
            approve_additions: playlist.approve_additions,
            created_by: playlist.created_by.clone(),
            created: datetime::format_utc(&playlist.created),
        }
    }
}

#[async_trait]
impl FamilyPlaylists for RulePlaylists {
    fn list(&self, viewer: &str) -> Vec<FamilyPlaylistSummary> {
        let caller = caller_for(&self.family, viewer);
        family_playlists::visible_playlists(&self.store, &caller)
            .iter()
            .map(|playlist| Self::summary(playlist, &caller))
            .collect()
    }

    fn get(&self, id: &str, viewer: &str) -> Option<FamilyPlaylistSummary> {
        let caller = caller_for(&self.family, viewer);
        family_playlists::find(&self.store, &caller, id).map(|playlist| Self::summary(&playlist, &caller))
    }

    async fn create(
        &self,
        name: &str,
        by: &str,
        approve_additions: bool,
    ) -> Result<FamilyPlaylistSummary, FamilyError> {
        let caller = caller_for(&self.family, by);
        family_playlists::create(&self.store, &caller, name, approve_additions, Utc::now())
            .map(|playlist| Self::summary(&playlist, &caller))
            .map_err(playlist_failure)
    }

    async fn update(
        &self,
        id: &str,
        by: &str,
        name: Option<&str>,
        approve_additions: Option<bool>,
    ) -> Result<FamilyPlaylistSummary, FamilyError> {
        let caller = caller_for(&self.family, by);
        let mut playlist = family_playlists::find(&self.store, &caller, id)
            .ok_or_else(|| playlist_failure(PlaylistError::NotFound))?;
        if let Some(name) = name {
            playlist = family_playlists::rename(&self.store, &caller, id, name).map_err(playlist_failure)?;
        }
        if let Some(approve) = approve_additions {
            playlist =
                family_playlists::set_approve(&self.store, &caller, id, approve).map_err(playlist_failure)?;
        }
        Ok(Self::summary(&playlist, &caller))
    }

    async fn delete(&self, id: &str, by: &str) -> Result<(), FamilyError> {
        let caller = caller_for(&self.family, by);
        family_playlists::delete(&self.store, &caller, id).map_err(playlist_failure)
    }
}
