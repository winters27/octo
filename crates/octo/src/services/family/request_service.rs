//! Members' requests for a copy: made, decided, fetched and finished.
//!
//! A new request for a song is met without a download whenever it can be, in this order:
//!
//! 1. The song is in the shared library: done at once ("Already in the family library").
//! 2. Another member has it in the family pool: a link is added to the member's library and
//!    it is done at once ("Added from the family library"). Not for a kid, or anyone whose
//!    `InstantFromFamily` is off, or whose library is full: their request waits like any other.
//! 3. Someone is downloading it right now: the member joins that download and gets a link when
//!    it lands (same rules as 2).
//! 4. Otherwise it waits for an approver, or is approved at once when the member may (see
//!    [`policy::request_decision`]).
//!
//! An approved song request downloads into the family pool, linked for the member. It is done
//! only once the library index finds the song in the member's own library, which can take a
//! scan or two; Octo looks for up to ten minutes. A song that lands in the shared library
//! instead (someone wanted it there) is done ("Already in the family library") once the index
//! finds it, for a member who sees that library; one who does not gets a copy in the pool,
//! linked and done like their own download.
//! A FLAC request takes FLAC files only, and a
//! FLAC made from lossy audio does not count; without a real one it fails with "No real FLAC
//! copy found", and the member can ask again for an MP3. A better copy of a shared song goes
//! through the upgrade queue as the owner; an import list's missing songs go to the imports
//! trickle as the member's.
//!
//! Requests met without a download never count toward the weekly limit. Every change is
//! raised on [`FamilyEvents`] so pushes and the apps can tell people.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::family::abilities::{AddToLibrary, Preset, RequestQuality};
use octo_core::family::policy::{self, RequestDecision, storage_full};
use octo_core::family::request::{FamilyRequest, RequestKind, RequestOutcome, RequestState};
use octo_core::fingerprint::heart_gate::AcquisitionOrigin;
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{DownloadSource, SettingsStore};
use octo_core::soulseek::soulseek_metadata_service::RoutingKind;
use tokio::sync::broadcast;
use tracing::{info, warn};

use super::family_playlist_store::FamilyPlaylistStore;
use super::family_store::{FamilyStore, Member};
use super::personal_library::PersonalLibrary;
use super::pool::{FamilyPool, LinkMode, MemberStars, ScanCheck, sees_shared_library};
use super::request_store::RequestStore;
use crate::services::common::acquisition_tracker::key_of;
use crate::services::common::base_download_service::NO_REAL_FLAC;
use crate::services::common::heart_acquisition_coordinator::TrackOutcome;
use crate::services::common::{
    AcquisitionTarget, AcquisitionTracker, HeartAcquisitionCoordinator, TrackAcquisitionQueue,
};
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::imports::ImportService;
use crate::services::library::{LibraryOwnership, OwnedCopy, UpgradeAsk, UpgradeQueue};
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};

/// One change to a request, for whoever tells people about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyEvent {
    /// A request waits for an approver.
    RequestCreated(FamilyRequest),
    /// An approver approved or declined it.
    RequestDecided(FamilyRequest),
    /// The song is in the member's library.
    RequestDone(FamilyRequest),
    /// It could not be met, with the reason in `failure`.
    RequestFailed(FamilyRequest),
}

/// The request events, for every listener (pushes, the apps' notices).
pub struct FamilyEvents {
    sender: broadcast::Sender<FamilyEvent>,
}

impl Default for FamilyEvents {
    fn default() -> Self {
        FamilyEvents {
            sender: broadcast::channel(256).0,
        }
    }
}

impl FamilyEvents {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<FamilyEvent> {
        self.sender.subscribe()
    }

    pub fn send(&self, event: FamilyEvent) {
        // Nobody listening is fine: the request itself is the record.
        let _ = self.sender.send(event);
    }

    /// How many listeners hear the events now.
    pub fn listeners(&self) -> usize {
        self.sender.receiver_count()
    }
}

/// Why a request could not be made or changed, with the Subsonic error code the app gets (10
/// missing, 50 not allowed, 70 not found) and the words the member reads.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct RequestError {
    pub code: i32,
    pub message: String,
}

impl RequestError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        RequestError {
            code,
            message: message.into(),
        }
    }
}

/// What a member reads when their account cannot ask for copies.
pub const NO_REQUESTS_MESSAGE: &str = "Asking for copies is off for this account.";

/// What a member reads when the song never showed up in their library.
pub const NOT_SHOWN_MESSAGE: &str = "The song did not show up in your library.";

/// An outside song or album an id names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideTarget {
    pub kind: RequestKind,
    pub provider: String,
    pub external_id: String,
}

/// Titles for a request Octo cannot look up itself (an import list, a playlist addition).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestFacts {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover_art: String,
}

/// What requests look up.
#[async_trait]
pub trait RequestCatalog: Send + Sync {
    /// The outside song or album an id names.
    fn outside(&self, id: &str) -> Option<OutsideTarget>;
    async fn song(&self, provider: &str, external_id: &str) -> Option<Song>;
    async fn album(&self, provider: &str, external_id: &str) -> Option<Album>;
    /// The shared library's copy of a song.
    async fn shared_copy(&self, song: &Song) -> Option<OwnedCopy>;
}

/// How a song request's download ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// The file is here (a pool file, or the shared library's when someone wanted it shared).
    Landed(String),
    /// The member's library or the shared one had it already.
    AlreadyThere,
    /// It could not be got, with the reason.
    Failed(String),
    /// Mornye's check kept it from Mornye, with the words: a FLAC request keeps them rather than
    /// reading as "no real FLAC".
    MornyeCheck(String),
}

/// How a request's failed download reads: Mornye's check, or the reason as given.
fn fetched_failure(error: &anyhow::Error) -> Fetched {
    match crate::services::spotiflac::mornye_waited(error) {
        Some(wait) => Fetched::MornyeCheck(wait.shown.clone()),
        None => Fetched::Failed(error.to_string()),
    }
}

/// What requests start.
#[async_trait]
pub trait RequestFetcher: Send + Sync {
    /// Whether this song is downloading for someone right now.
    fn in_flight(&self, provider: &str, external_id: &str) -> bool;
    /// One song into `user`'s own library at this quality.
    async fn song(
        &self,
        user: &str,
        provider: &str,
        external_id: &str,
        quality: RequestQuality,
        song: Option<&Song>,
    ) -> Fetched;
    /// Every song of an album into `user`'s own library; where each that landed is.
    async fn album(&self, user: &str, provider: &str, external_id: &str) -> Vec<String>;
    /// Queues a better copy of shared song `navidrome_id` as the owner. False when it could not
    /// be queued.
    fn upgrade(&self, owner: &str, navidrome_id: &str, facts: &RequestFacts) -> bool;
    /// How the better copy of `navidrome_id` ended, once it has: Ok, or the reason.
    fn upgrade_result(&self, navidrome_id: &str) -> Option<Result<(), String>>;
    /// Hands an import list's missing songs to the trickle for `user`. The words to show.
    async fn import(&self, user: &str, list_id: &str) -> Result<String, String>;
}

/// How patiently requests wait.
#[derive(Debug, Clone, Copy)]
pub struct RequestTiming {
    /// How often a better copy is asked after.
    pub poll: Duration,
    /// How long a better copy is waited for.
    pub upgrade_wait: Duration,
}

impl Default for RequestTiming {
    fn default() -> Self {
        RequestTiming {
            poll: Duration::from_secs(30),
            upgrade_wait: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// The services a [`RequestService`] works with.
pub struct RequestParts {
    pub settings: Arc<SettingsStore>,
    pub family: Arc<FamilyStore>,
    pub requests: Arc<RequestStore>,
    pub personal: Arc<PersonalLibrary>,
    pub catalog: Arc<dyn RequestCatalog>,
    pub fetcher: Arc<dyn RequestFetcher>,
    pub stars: Arc<dyn MemberStars>,
    pub events: Arc<FamilyEvents>,
    /// The family playlists whose waiting additions are requests too.
    pub playlists: Arc<FamilyPlaylistStore>,
}

/// Members' requests. See the module comment.
pub struct RequestService {
    me: Weak<RequestService>,
    settings: Arc<SettingsStore>,
    family: Arc<FamilyStore>,
    requests: Arc<RequestStore>,
    personal: Arc<PersonalLibrary>,
    catalog: Arc<dyn RequestCatalog>,
    fetcher: Arc<dyn RequestFetcher>,
    stars: Arc<dyn MemberStars>,
    events: Arc<FamilyEvents>,
    playlists: Arc<FamilyPlaylistStore>,
    timing: RequestTiming,
    /// Approved requests left unfinished by a restart have been started again.
    resumed: AtomicBool,
}

/// The target of a playlist addition request: `<playlist id>:<song id>`.
pub fn playlist_addition_target(playlist_id: &str, song_id: &str) -> String {
    format!("{playlist_id}:{song_id}")
}

/// A playlist addition request's playlist and song.
fn playlist_addition_parts(target: &str) -> Option<(&str, &str)> {
    target
        .split_once(':')
        .filter(|(playlist, song)| !playlist.is_empty() && !song.is_empty())
}

/// A request's titles from a file's place in the `<Artist>/<Album>/<file>` layout.
pub fn facts_from_path(path: &str) -> RequestFacts {
    let parts: Vec<&str> = path.rsplit('/').collect();
    let file = parts.first().copied().unwrap_or("");
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    // "01 Title" and "01 - Title" read as the title.
    let title = stem
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start_matches([' ', '-', '.'])
        .trim();
    RequestFacts {
        title: if title.is_empty() { stem } else { title }.to_string(),
        artist: parts.get(2).copied().unwrap_or("").to_string(),
        album: parts.get(1).copied().unwrap_or("").to_string(),
        cover_art: String::new(),
    }
}

impl RequestService {
    pub fn new(parts: RequestParts) -> Arc<Self> {
        Self::with_timing(parts, RequestTiming::default())
    }

    pub fn with_timing(parts: RequestParts, timing: RequestTiming) -> Arc<Self> {
        Arc::new_cyclic(|me| RequestService {
            me: me.clone(),
            settings: parts.settings,
            family: parts.family,
            requests: parts.requests,
            personal: parts.personal,
            catalog: parts.catalog,
            fetcher: parts.fetcher,
            stars: parts.stars,
            events: parts.events,
            playlists: parts.playlists,
            timing,
            resumed: AtomicBool::new(false),
        })
    }

    pub fn events(&self) -> &Arc<FamilyEvents> {
        &self.events
    }

    pub fn store(&self) -> &Arc<RequestStore> {
        &self.requests
    }

    fn pool(&self) -> &Arc<FamilyPool> {
        self.personal.pool()
    }

    pub fn get(&self, id: &str) -> Option<FamilyRequest> {
        self.requests.get(id)
    }

    /// One member's requests, oldest first.
    pub fn requests_of(&self, user: &str) -> Vec<FamilyRequest> {
        self.requests.for_user(user)
    }

    /// Every request waiting for an approver, oldest first.
    pub fn pending(&self) -> Vec<FamilyRequest> {
        self.requests.pending()
    }

    /// The member's weekly slots in use now.
    pub fn quota_used(&self, user: &str) -> u32 {
        self.requests.quota_used(user, Utc::now())
    }

    fn member(&self, user: &str) -> Result<Member, RequestError> {
        if !self.settings.current().family.enabled {
            return Err(RequestError::new(50, "Family is off on this server."));
        }
        let member = self
            .family
            .member(user.trim())
            .ok_or_else(|| RequestError::new(50, "Only family members ask for copies."))?;
        if member.suspended {
            return Err(RequestError::new(
                50,
                "This account is paused by the family owner.",
            ));
        }
        Ok(member)
    }

    /// Whether `user` may take a song another member already has, without asking.
    fn instant_allowed(&self, member: &Member) -> bool {
        let abilities = member.abilities();
        abilities.instant_from_family
            && member.preset != Preset::Kid
            && storage_full(&abilities, self.personal.storage_used(&member.username)).is_none()
    }

    /// A request for `target` (an outside song or album id, or a library song id for a better
    /// copy) at `quality`. The kind is read from the id unless given.
    pub async fn create(
        &self,
        user: &str,
        target: &str,
        quality: RequestQuality,
        kind: Option<RequestKind>,
    ) -> Result<FamilyRequest, RequestError> {
        self.create_with(user, kind, target, quality, None).await
    }

    /// [`Self::create`] with titles for a request Octo cannot look up (an import list).
    pub async fn create_with(
        &self,
        user: &str,
        kind: Option<RequestKind>,
        target: &str,
        quality: RequestQuality,
        facts: Option<RequestFacts>,
    ) -> Result<FamilyRequest, RequestError> {
        self.create_inner(user, kind, target, quality, facts, false).await
    }

    /// [`Self::create_with`]. `reuse_slot` is a failed approved request asked again: while its
    /// weekly slot is still free the new request takes that slot and that approval, and is
    /// not asked about again.
    async fn create_inner(
        &self,
        user: &str,
        kind: Option<RequestKind>,
        target: &str,
        quality: RequestQuality,
        facts: Option<RequestFacts>,
        reuse_slot: bool,
    ) -> Result<FamilyRequest, RequestError> {
        let target = target.trim();
        if target.is_empty() {
            return Err(RequestError::new(10, "Required parameter is missing: id"));
        }
        let member = self.member(user)?;
        let abilities = member.abilities();
        if abilities.add_to_library == AddToLibrary::SaveOnly {
            return Err(RequestError::new(50, NO_REQUESTS_MESSAGE));
        }
        if quality.rank() > abilities.request_quality.rank() {
            return Err(RequestError::new(
                50,
                format!(
                    "This account can ask for {} copies at most.",
                    abilities.request_quality.label()
                ),
            ));
        }
        let outside = self.catalog.outside(target);
        let kind = match (kind, &outside) {
            (Some(kind), _) => kind,
            (None, Some(outside)) => outside.kind,
            (None, None) if self.pool().index().song(target).is_some() => RequestKind::Upgrade,
            (None, None) => return Err(RequestError::new(70, "That song was not found.")),
        };
        // Asking again for something still waiting answers the request already made.
        if let Some(existing) = self
            .requests
            .for_user(&member.username)
            .into_iter()
            .find(|r| same_live_ask(r, &member.username, kind, target, quality))
        {
            return Ok(existing);
        }
        match kind {
            RequestKind::Song => {
                let outside = outside
                    .filter(|o| o.kind == RequestKind::Song)
                    .ok_or_else(|| RequestError::new(70, "That song was not found."))?;
                self.create_song(&member, target, &outside, quality, facts, reuse_slot)
                    .await
            }
            RequestKind::Album => {
                let outside = outside
                    .filter(|o| o.kind == RequestKind::Album)
                    .ok_or_else(|| RequestError::new(70, "That album was not found."))?;
                self.create_album(&member, target, &outside, quality, facts, reuse_slot)
                    .await
            }
            RequestKind::Upgrade => self.create_upgrade(&member, target, quality, facts, reuse_slot),
            RequestKind::Import => self.decide_new(
                &member,
                self.new_request(&member, kind, target, quality, facts.unwrap_or_default()),
                reuse_slot,
            ),
            RequestKind::PlaylistAdd => Err(RequestError::new(
                50,
                "Add the song to the family playlist; the approvers see it there.",
            )),
        }
    }

    /// A request for an addition to a family playlist that waits for approval: `song_id` was
    /// added to `playlist_id` by `user`. Approving it puts the song in the playlist; declining
    /// or canceling it takes the song out. The request exists at once; its titles are looked
    /// up next, and the approvers hear of it then.
    pub fn create_playlist_addition(
        &self,
        user: &str,
        playlist_id: &str,
        playlist_name: &str,
        song_id: &str,
    ) -> Result<FamilyRequest, RequestError> {
        let member = self.member(user)?;
        let target = playlist_addition_target(playlist_id, song_id);
        let mut request = self.new_request(
            &member,
            RequestKind::PlaylistAdd,
            &target,
            member.abilities().request_quality,
            RequestFacts::default(),
        );
        request.note = format!("For the family playlist {playlist_name}");
        request.cover_art = song_id.to_string();
        let (made, new) = self.requests.create_unless(request, Utc::now(), |r| {
            r.kind == RequestKind::PlaylistAdd
                && r.target == target
                && r.state == RequestState::Pending
                && eq_ignore_case(&r.username, &member.username)
        });
        if !new {
            return Ok(made);
        }
        info!(
            "FAMILY request {} {} PlaylistAdd {song_id} to {playlist_id} -> Pending",
            made.id, made.username
        );
        match self.me.upgrade() {
            Some(me) if tokio::runtime::Handle::try_current().is_ok() => {
                let (id, song) = (made.id.clone(), song_id.to_string());
                tokio::spawn(async move { me.titled(&id, &song).await });
            }
            _ => self.events.send(FamilyEvent::RequestCreated(made.clone())),
        }
        Ok(made)
    }

    /// Fills in a playlist addition's titles from the song, then tells the approvers.
    async fn titled(&self, id: &str, song_id: &str) {
        let facts = match self.catalog.outside(song_id) {
            Some(outside) => song_facts(
                self.catalog
                    .song(&outside.provider, &outside.external_id)
                    .await
                    .as_ref(),
                song_id,
            ),
            None => self
                .pool()
                .index()
                .song(song_id)
                .map(|song| facts_from_path(&song.path))
                .unwrap_or_default(),
        };
        let updated = self.requests.update(id, |r| {
            r.title = facts.title.clone();
            r.artist = facts.artist.clone();
            r.album = facts.album.clone();
        });
        if let Some(request) = updated.filter(|r| r.state == RequestState::Pending) {
            self.events.send(FamilyEvent::RequestCreated(request));
        }
    }

    /// The waiting entry a playlist addition request stands for: its playlist and its place.
    fn playlist_entry(&self, request: &FamilyRequest) -> Option<(String, usize)> {
        let (playlist_id, song_id) = playlist_addition_parts(&request.target)?;
        let playlist = self.playlists.get(playlist_id)?;
        let index = playlist.entries.iter().position(|entry| {
            entry.pending
                && eq_ignore_case(&entry.added_by, &request.username)
                && (entry.library_id.as_deref() == Some(song_id)
                    || entry.outside_id.as_deref() == Some(song_id))
        })?;
        Some((playlist.id, index))
    }

    /// Takes a declined or canceled playlist addition out of its playlist.
    fn drop_playlist_entry(&self, request: &FamilyRequest) {
        if request.kind != RequestKind::PlaylistAdd {
            return;
        }
        if let Some((playlist, index)) = self.playlist_entry(request) {
            self.playlists.remove_indexes(&playlist, &[index]);
            info!(
                "FAMILY request {} took its song out of family playlist {playlist}",
                request.id
            );
        }
    }

    /// One request for an import list's missing songs (`list_id`), titled from the list.
    pub fn create_import(
        &self,
        user: &str,
        list_id: &str,
        facts: RequestFacts,
    ) -> Result<FamilyRequest, RequestError> {
        let member = self.member(user)?;
        if member.abilities().add_to_library == AddToLibrary::SaveOnly {
            return Err(RequestError::new(50, NO_REQUESTS_MESSAGE));
        }
        if let Some(existing) = self.requests.for_user(&member.username).into_iter().find(|r| {
            r.kind == RequestKind::Import
                && r.target == list_id
                && matches!(r.state, RequestState::Pending | RequestState::Approved)
        }) {
            return Ok(existing);
        }
        // The list fetches at whatever quality the trickle finds, so it asks at the member's most.
        let quality = member.abilities().request_quality;
        let request = self.new_request(&member, RequestKind::Import, list_id, quality, facts);
        self.decide_new(&member, request, false)
    }

    /// Keeps a request that waits for an approver or is approved, unless the member's same ask
    /// is already waiting or approved: then that one is the answer, and false says so.
    fn keep_live(&self, request: FamilyRequest) -> (FamilyRequest, bool) {
        let (user, kind, target, quality) = (
            request.username.clone(),
            request.kind,
            request.target.clone(),
            request.quality,
        );
        self.requests.create_unless(request, Utc::now(), |r| {
            same_live_ask(r, &user, kind, &target, quality)
        })
    }

    fn new_request(
        &self,
        member: &Member,
        kind: RequestKind,
        target: &str,
        quality: RequestQuality,
        facts: RequestFacts,
    ) -> FamilyRequest {
        FamilyRequest {
            id: String::new(),
            username: member.username.clone(),
            kind,
            target: target.to_string(),
            title: facts.title,
            artist: facts.artist,
            album: facts.album,
            cover_art: facts.cover_art,
            quality,
            state: RequestState::Pending,
            created: Utc::now(),
            decided_by: None,
            decided: None,
            note: String::new(),
            library_song_id: None,
            failure: None,
            outcome: None,
        }
    }

    async fn create_song(
        &self,
        member: &Member,
        target: &str,
        outside: &OutsideTarget,
        quality: RequestQuality,
        facts: Option<RequestFacts>,
        reuse_slot: bool,
    ) -> Result<FamilyRequest, RequestError> {
        let song = self.catalog.song(&outside.provider, &outside.external_id).await;
        let facts = facts.unwrap_or_else(|| song_facts(song.as_ref(), target));
        let mut request = self.new_request(member, RequestKind::Song, target, quality, facts.clone());

        // 1. Already shared, in a shared library the member sees.
        if let Some(song) = &song
            && let Some(copy) = self.catalog.shared_copy(song).await
            && self.sees_copy(member, copy.navidrome_id.as_deref())
        {
            if quality != RequestQuality::Flac || copy.lossless {
                request.state = RequestState::Done;
                request.outcome = Some(RequestOutcome::AlreadyShared);
                request.library_song_id = copy.navidrome_id.clone();
                let made = self.requests.create(request, Utc::now());
                if let Some(id) = &copy.navidrome_id {
                    self.stars.star(&member.username, id).await;
                }
                info!(
                    "FAMILY request {} {} {}: already shared",
                    made.id, made.username, made.title
                );
                self.events.send(FamilyEvent::RequestDone(made.clone()));
                return Ok(made);
            }
            // A FLAC asked for a song the shared library has only lossy: a better copy of it.
            if let Some(id) = &copy.navidrome_id {
                return self.create_upgrade(member, id, quality, Some(facts), reuse_slot);
            }
        }

        // 2. Another member has it.
        if let Some(file) = self.pooled(outside, song.as_ref(), quality).await
            && (file.refs.iter().any(|r| eq_ignore_case(r, &member.username)) || self.instant_allowed(member))
        {
            return match self.pool().link(&file.path, &member.username).await {
                Ok(_) => {
                    request.state = RequestState::Done;
                    request.outcome = Some(RequestOutcome::AddedFromFamily);
                    let made = self.requests.create(request, Utc::now());
                    info!(
                        "FAMILY request {} {}: added from the family library",
                        made.id, made.username
                    );
                    self.events.send(FamilyEvent::RequestDone(made.clone()));
                    Ok(made)
                }
                Err(e) => {
                    warn!("FAMILY {} could not be given {}: {e}", member.username, file.path);
                    Err(RequestError::new(0, e.for_member()))
                }
            };
        }

        // 3. Downloading for someone now.
        if quality != RequestQuality::Flac
            && self.fetcher.in_flight(&outside.provider, &outside.external_id)
            && self.instant_allowed(member)
        {
            request.state = RequestState::Approved;
            let (made, new) = self.keep_live(request);
            if new {
                info!(
                    "FAMILY request {} {}: joins a download in flight",
                    made.id, made.username
                );
                self.start(made.clone(), true);
            }
            return Ok(made);
        }

        // 4. Ask.
        self.decide_new(member, request, reuse_slot)
    }

    /// Whether `member` sees the shared library a shared song is in (by its library song id).
    fn sees_copy(&self, member: &Member, navidrome_id: Option<&str>) -> bool {
        let library = navidrome_id.and_then(|id| self.pool().index().library_of(id));
        sees_shared_library(member, library)
    }

    /// A pool file holding the song at a quality that will do: for a FLAC request, only a real
    /// FLAC (never one made from lossy audio).
    async fn pooled(
        &self,
        outside: &OutsideTarget,
        song: Option<&Song>,
        quality: RequestQuality,
    ) -> Option<super::family_pool_store::PoolFile> {
        let key = key_of(&outside.provider, &outside.external_id);
        let (isrc, artist, title) = song
            .map(|s| (s.isrc.as_deref(), s.artist.as_str(), s.title.as_str()))
            .unwrap_or((None, "", ""));
        self.pool()
            .find(Some(&key), isrc, artist, title)
            .await
            .filter(|file| quality != RequestQuality::Flac || file.is_real_flac())
    }

    async fn create_album(
        &self,
        member: &Member,
        target: &str,
        outside: &OutsideTarget,
        quality: RequestQuality,
        facts: Option<RequestFacts>,
        reuse_slot: bool,
    ) -> Result<FamilyRequest, RequestError> {
        let album = self.catalog.album(&outside.provider, &outside.external_id).await;
        let facts = facts.unwrap_or_else(|| RequestFacts {
            title: album.as_ref().map(|a| a.title.clone()).unwrap_or_default(),
            artist: album.as_ref().map(|a| a.artist.clone()).unwrap_or_default(),
            album: album.as_ref().map(|a| a.title.clone()).unwrap_or_default(),
            cover_art: target.to_string(),
        });
        let mut request = self.new_request(member, RequestKind::Album, target, quality, facts);
        // Every song already shared, in shared libraries the member sees: nothing to fetch.
        if let Some(album) = &album
            && !album.songs.is_empty()
        {
            let mut all_shared = true;
            for song in &album.songs {
                let seen = self
                    .catalog
                    .shared_copy(song)
                    .await
                    .is_some_and(|copy| self.sees_copy(member, copy.navidrome_id.as_deref()));
                if !seen {
                    all_shared = false;
                    break;
                }
            }
            if all_shared {
                request.state = RequestState::Done;
                request.outcome = Some(RequestOutcome::AlreadyShared);
                let made = self.requests.create(request, Utc::now());
                self.events.send(FamilyEvent::RequestDone(made.clone()));
                return Ok(made);
            }
        }
        self.decide_new(member, request, reuse_slot)
    }

    fn create_upgrade(
        &self,
        member: &Member,
        navidrome_id: &str,
        quality: RequestQuality,
        facts: Option<RequestFacts>,
        reuse_slot: bool,
    ) -> Result<FamilyRequest, RequestError> {
        let index = self.pool().index();
        if let Some(library) = index.library_of(navidrome_id)
            && self
                .family
                .members()
                .iter()
                .any(|m| m.personal_library_id == Some(library))
        {
            return Err(RequestError::new(
                50,
                "Ask for a better copy of a shared song only.",
            ));
        }
        let request = self.new_request(
            member,
            RequestKind::Upgrade,
            navidrome_id,
            quality,
            facts.unwrap_or_default(),
        );
        self.decide_new(member, request, reuse_slot)
    }

    /// Pending, approved at once, or refused, by the member's abilities, their weekly limit
    /// and their storage. With `reuse_slot` (a failed approved request asked again) it is
    /// approved at once while the member has a weekly slot free, as the failed one held.
    /// The member's same ask already waiting or approved is the answer instead of a new one.
    /// The weekly limit is read and the request kept in one step of the request store, so asks
    /// for different songs at the same moment never pass the limit together.
    fn decide_new(
        &self,
        member: &Member,
        request: FamilyRequest,
        reuse_slot: bool,
    ) -> Result<FamilyRequest, RequestError> {
        let abilities = member.abilities();
        let storage = self.personal.storage_used(&member.username);
        let (user, kind, target, quality) = (
            request.username.clone(),
            request.kind,
            request.target.clone(),
            request.quality,
        );
        let (made, new) = self.requests.create_deciding(
            request,
            Utc::now(),
            |r| same_live_ask(r, &user, kind, &target, quality),
            |used, request| {
                let slot_free = abilities.weekly_request_limit == 0 || used < abilities.weekly_request_limit;
                // A playlist addition always waits for whoever decides on that playlist.
                let decision = if request.kind == RequestKind::PlaylistAdd {
                    RequestDecision::Pending
                } else if reuse_slot && slot_free {
                    RequestDecision::AutoApprove
                } else {
                    policy::request_decision(&abilities, request.quality, used, storage)
                };
                match decision {
                    RequestDecision::Refuse(message) => return Err(RequestError::new(50, message)),
                    RequestDecision::AutoApprove => request.state = RequestState::Approved,
                    RequestDecision::Pending => {
                        request.state = RequestState::Pending;
                        if let Some(full) = storage_full(&abilities, storage) {
                            request.note = full;
                        }
                    }
                }
                Ok(())
            },
        )?;
        if !new {
            return Ok(made);
        }
        if made.state == RequestState::Approved {
            info!(
                "FAMILY request {} {} {:?} \"{}\" {:?} -> AutoApproved",
                made.id, made.username, made.kind, made.title, made.quality
            );
            self.events.send(FamilyEvent::RequestDecided(made.clone()));
            self.start(made.clone(), false);
        } else {
            info!(
                "FAMILY request {} {} {:?} \"{}\" {:?} -> Pending",
                made.id, made.username, made.kind, made.title, made.quality
            );
            self.events.send(FamilyEvent::RequestCreated(made.clone()));
        }
        Ok(made)
    }

    /// Whether `by` may decide requests: the owner, or a member with `ApproveRequests`.
    fn may_decide(&self, by: &str) -> bool {
        self.family.is_owner(by)
            || self
                .family
                .member(by)
                .is_some_and(|m| !m.suspended && m.abilities().approve_requests)
    }

    /// Approves a waiting request; it is fetched now. An approver may approve past the member's
    /// storage limit or weekly limit. Checked and changed in one step: of two approvers at the
    /// same moment one approves, and the other is told it was already decided.
    pub fn approve(&self, id: &str, by: &str, note: Option<&str>) -> Result<FamilyRequest, RequestError> {
        if !self.may_decide(by) {
            return Err(RequestError::new(50, "Only approvers decide requests."));
        }
        let decided = self
            .requests
            .update_if(
                id,
                |r| r.state == RequestState::Pending,
                |r| {
                    r.state = RequestState::Approved;
                    r.decided_by = Some(by.to_string());
                    r.decided = Some(Utc::now());
                    if let Some(note) = note.filter(|n| !n.trim().is_empty()) {
                        r.note = note.trim().to_string();
                    }
                },
            )
            .map_err(already_decided)?;
        info!("FAMILY request {id} approved by {by}");
        self.events.send(FamilyEvent::RequestDecided(decided.clone()));
        self.start(decided.clone(), false);
        Ok(decided)
    }

    /// Declines a waiting request, with an optional note for the member. It does not count
    /// toward their weekly limit. Checked and changed in one step, as [`Self::approve`].
    pub fn decline(&self, id: &str, by: &str, note: Option<&str>) -> Result<FamilyRequest, RequestError> {
        if !self.may_decide(by) {
            return Err(RequestError::new(50, "Only approvers decide requests."));
        }
        let decided = self
            .requests
            .update_if(
                id,
                |r| r.state == RequestState::Pending,
                |r| {
                    r.state = RequestState::Declined;
                    r.decided_by = Some(by.to_string());
                    r.decided = Some(Utc::now());
                    r.note = note.map(str::trim).unwrap_or_default().to_string();
                },
            )
            .map_err(already_decided)?;
        info!("FAMILY request {id} declined by {by}");
        self.drop_playlist_entry(&decided);
        self.events.send(FamilyEvent::RequestDecided(decided.clone()));
        Ok(decided)
    }

    /// Takes back one of the member's own waiting requests, checked and changed in one step.
    pub fn cancel(&self, id: &str, by_member: &str) -> Result<FamilyRequest, RequestError> {
        let canceled = self
            .requests
            .update_if(
                id,
                |r| eq_ignore_case(&r.username, by_member) && r.state == RequestState::Pending,
                |r| r.state = RequestState::Cancelled,
            )
            .map_err(|refused| match refused {
                Some(request) if eq_ignore_case(&request.username, by_member) => {
                    RequestError::new(50, "Only a waiting request can be taken back.")
                }
                _ => RequestError::new(70, "There is no such request."),
            })?;
        self.drop_playlist_entry(&canceled);
        Ok(canceled)
    }

    /// Closes the requests of a member who has left the family (called once they are out of
    /// it): each one still waiting or approved is canceled (so nobody can approve it any more,
    /// and a restart never starts it again), and its playlist addition, if it is one, leaves
    /// the playlist. A download already on its way for one of them is let go of when it lands
    /// ([`FamilyPool::let_go`]), and so is every pool file still recorded for them now: one
    /// that landed after their files were let go of and before they were out of the family.
    /// Nothing is left in the pool for nobody. Answers how many requests were canceled.
    pub async fn close_member(&self, user: &str) -> usize {
        let canceled = self.requests.cancel_live_of(user);
        for request in &canceled {
            self.drop_playlist_entry(request);
        }
        if !canceled.is_empty() {
            info!(
                "FAMILY {} waiting or approved request(s) of {user} canceled as they left the family",
                canceled.len()
            );
        }
        let pool = self.pool();
        for file in pool.store().refs_of(user) {
            pool.let_go(&file.path, user).await;
        }
        canceled.len()
    }

    /// Whether a request is still approved and running: false once it was canceled (its member
    /// left the family) or finished.
    fn still_approved(&self, request: &FamilyRequest) -> bool {
        self.requests
            .get(&request.id)
            .is_some_and(|r| r.state == RequestState::Approved)
    }

    /// Asks again for an MP3 of a song whose FLAC request failed. The new request takes the
    /// failed one's weekly slot and approval while that slot is still free.
    pub async fn retry_as_mp3(&self, id: &str, by_member: &str) -> Result<FamilyRequest, RequestError> {
        let request = self
            .requests
            .get(id)
            .filter(|r| eq_ignore_case(&r.username, by_member))
            .ok_or_else(|| RequestError::new(70, "There is no such request."))?;
        if request.state != RequestState::Failed || request.quality != RequestQuality::Flac {
            return Err(RequestError::new(
                50,
                "Only a FLAC request that failed can be tried as MP3.",
            ));
        }
        let facts = RequestFacts {
            title: request.title.clone(),
            artist: request.artist.clone(),
            album: request.album.clone(),
            cover_art: request.cover_art.clone(),
        };
        self.create_inner(
            by_member,
            Some(request.kind),
            &request.target,
            RequestQuality::Mp3,
            Some(facts),
            true,
        )
        .await
    }

    /// Starts again every approved request a restart left unfinished: Octo keeps no running
    /// work across a restart, and an approved request would otherwise stay approved for good
    /// and stand in the way of asking again. Runs once per start of Octo. Answers how many.
    pub fn resume_approved(&self) -> usize {
        if self.resumed.swap(true, Ordering::SeqCst) {
            return 0;
        }
        let approved: Vec<FamilyRequest> = self
            .requests
            .all()
            .into_iter()
            .filter(|r| r.state == RequestState::Approved)
            .collect();
        for request in &approved {
            info!(
                "FAMILY request {} {} {:?} \"{}\" resumes after a restart",
                request.id, request.username, request.kind, request.title
            );
            self.start(request.clone(), false);
        }
        approved.len()
    }

    /// Fetches an approved request on a task of its own.
    fn start(&self, request: FamilyRequest, joined: bool) {
        let Some(me) = self.me.upgrade() else { return };
        tokio::spawn(async move { me.run(request, joined).await });
    }

    async fn run(&self, request: FamilyRequest, joined: bool) {
        match request.kind {
            RequestKind::Song => self.run_song(&request, joined).await,
            RequestKind::Album => self.run_album(&request).await,
            RequestKind::Upgrade => self.run_upgrade(&request).await,
            RequestKind::Import => match self.fetcher.import(&request.username, &request.target).await {
                Ok(_) => self.done(&request, Some(RequestOutcome::Downloaded), None),
                Err(reason) => self.failed(&request, &reason),
            },
            RequestKind::PlaylistAdd => match self.playlist_entry(&request) {
                Some((playlist, index)) => {
                    self.playlists.approve_entry(&playlist, index);
                    info!(
                        "FAMILY request {} put its song in family playlist {playlist}",
                        request.id
                    );
                    self.done(&request, None, None);
                }
                None => self.failed(&request, "The song is no longer waiting in that playlist."),
            },
        }
    }

    async fn run_song(&self, request: &FamilyRequest, joined: bool) {
        let Some(outside) = self.catalog.outside(&request.target) else {
            self.failed(request, "That song was not found.");
            return;
        };
        let user = request.username.clone();
        let song = self.catalog.song(&outside.provider, &outside.external_id).await;
        // Someone may have got it since it was asked for.
        if let Some(file) = self.pooled(&outside, song.as_ref(), request.quality).await
            && self.still_approved(request)
        {
            match self.pool().link(&file.path, &user).await {
                Ok(link) => {
                    self.wait_done(request, &link, RequestOutcome::AddedFromFamily)
                        .await;
                    return;
                }
                Err(e) => warn!("FAMILY request {} could not link {}: {e}", request.id, file.path),
            }
        }
        let fetched = self
            .fetcher
            .song(
                &user,
                &outside.provider,
                &outside.external_id,
                request.quality,
                song.as_ref(),
            )
            .await;
        let outcome = if joined {
            RequestOutcome::AddedFromFamily
        } else {
            RequestOutcome::Downloaded
        };
        if let Fetched::Landed(path) = &fetched
            && !self.still_approved(request)
        {
            self.pool().let_go(path, &user).await;
            return;
        }
        match fetched {
            Fetched::Landed(path) => match self.pool().link_path(&path, &user) {
                Some(_) => match self.pool().link(&path, &user).await {
                    Ok(link) => self.wait_done(request, &link, outcome).await,
                    Err(e) => {
                        warn!("FAMILY request {} could not link {path}: {e}", request.id);
                        // Not theirs after all (they left the family as it landed, or it could
                        // not be linked): it is not kept for nobody.
                        self.pool().let_go(&path, &user).await;
                        self.failed(request, e.for_member());
                    }
                },
                // Someone wanted it shared, so it is in the shared library.
                None => {
                    let key = key_of(&outside.provider, &outside.external_id);
                    match self
                        .outside_pool_place(&user, &path, Some(&key), song.as_ref())
                        .await
                    {
                        Ok((shown, true)) => {
                            self.wait_done(request, &shown, RequestOutcome::AlreadyShared)
                                .await
                        }
                        Ok((shown, false)) => self.wait_done(request, &shown, outcome).await,
                        Err(e) => {
                            warn!(
                                "FAMILY request {} could not get a copy of {path}: {e}",
                                request.id
                            );
                            self.failed(request, e.for_member());
                        }
                    }
                }
            },
            Fetched::AlreadyThere => self.done(request, Some(RequestOutcome::AlreadyShared), None),
            Fetched::Failed(reason) => {
                let reason = if request.quality == RequestQuality::Flac {
                    NO_REAL_FLAC.to_string()
                } else {
                    // The member reads this, on a phone: the first sentence, no server paths.
                    octo_core::common::acquisition_tracker::user_safe(Some(&reason)).unwrap_or(reason)
                };
                self.failed(request, &reason);
            }
            Fetched::MornyeCheck(words) => self.failed(request, &words),
        }
    }

    async fn run_album(&self, request: &FamilyRequest) {
        let Some(outside) = self.catalog.outside(&request.target) else {
            self.failed(request, "That album was not found.");
            return;
        };
        let landed = self
            .fetcher
            .album(&request.username, &outside.provider, &outside.external_id)
            .await;
        if !self.still_approved(request) {
            for path in &landed {
                self.pool().let_go(path, &request.username).await;
            }
            return;
        }
        let mut shown = Vec::new();
        let mut failure = None;
        for path in &landed {
            if let Some(link) = self.pool().link_path(path, &request.username) {
                // A song placed without their link (they left the family as it landed) is not
                // kept for nobody.
                let theirs = self
                    .pool()
                    .store()
                    .get(path)
                    .is_some_and(|file| file.refs.iter().any(|r| eq_ignore_case(r, &request.username)));
                if theirs {
                    shown.push(link);
                } else {
                    self.pool().let_go(path, &request.username).await;
                }
                continue;
            }
            match self.outside_pool_place(&request.username, path, None, None).await {
                Ok((place, _)) => shown.push(place),
                Err(e) => {
                    warn!(
                        "FAMILY request {} could not get a copy of {path}: {e}",
                        request.id
                    );
                    failure.get_or_insert(e);
                }
            }
        }
        let Some(first) = shown.first() else {
            match failure {
                Some(e) => self.failed(request, e.for_member()),
                None => self.failed(request, "None of the album's songs could be found."),
            }
            return;
        };
        self.wait_done(request, first, RequestOutcome::Downloaded).await;
    }

    /// Where a song that landed outside the pool shows for `user`, and whether it is the shared
    /// library's own file. A shared file is theirs as it is when they see its shared library.
    /// When they cannot see it they get a pool copy of it ([`FamilyPool::copy_shared_for`]), and
    /// the answer is their link to it. A file in their own folder stays where it is.
    async fn outside_pool_place(
        &self,
        user: &str,
        path: &str,
        outside_key: Option<&str>,
        song: Option<&Song>,
    ) -> Result<(String, bool), super::pool::PoolError> {
        let pool = self.pool();
        if !pool.is_shared_file(path) {
            return Ok((path.to_string(), false));
        }
        if pool.sees_shared_file(user, path) {
            return Ok((path.to_string(), true));
        }
        let link = pool.copy_shared_for(path, user, outside_key, song).await?;
        info!("FAMILY {user} cannot see the shared library {path} is in; they get a copy");
        Ok((link, false))
    }

    async fn run_upgrade(&self, request: &FamilyRequest) {
        let facts = RequestFacts {
            title: request.title.clone(),
            artist: request.artist.clone(),
            album: request.album.clone(),
            cover_art: request.cover_art.clone(),
        };
        let owner = self.family.owner();
        if !self.fetcher.upgrade(&owner, &request.target, &facts) {
            self.failed(
                request,
                "The better copy could not be asked for now. Try again later.",
            );
            return;
        }
        let started = tokio::time::Instant::now();
        loop {
            if let Some(result) = self.fetcher.upgrade_result(&request.target) {
                match result {
                    Ok(()) => self.done(request, Some(RequestOutcome::Downloaded), None),
                    Err(reason) => self.failed(request, &reason),
                }
                return;
            }
            if started.elapsed() >= self.timing.upgrade_wait {
                self.failed(request, "No better copy came in time.");
                return;
            }
            tokio::time::sleep(self.timing.poll).await;
        }
    }

    /// Done once the library index finds `path` in the member's own library (anywhere, for a
    /// shared path). A link Navidrome does not show in time, before any link was ever shown,
    /// becomes a real copy for this member alone and Octo looks again: a slow scan never
    /// changes how anyone else gets songs. The song counts toward giving up on links only when
    /// Navidrome said its scan was finished and the index read after that still did not show
    /// the link, and Navidrome then showed the copy; each song counts once
    /// ([`FamilyPool::copy_shown_where_link_was_not`]).
    async fn wait_done(&self, request: &FamilyRequest, path: &str, outcome: RequestOutcome) {
        let pool = self.pool();
        let library = pool
            .pool_path_of_link(&request.username, path)
            .and_then(|_| self.personal.library_id(&request.username));
        if let Some(song) = pool.wait_for_path(path, library).await {
            self.stars.star(&request.username, &song.id).await;
            self.done(request, Some(outcome), Some(song.id));
            return;
        }
        if pool.mode() == LinkMode::Symlink {
            let missed = match pool.after_finished_scan(path, library).await {
                ScanCheck::Shown(song) => {
                    self.stars.star(&request.username, &song.id).await;
                    self.done(request, Some(outcome), Some(song.id));
                    return;
                }
                ScanCheck::Missed => true,
                ScanCheck::Unsure => false,
            };
            if pool.copy_unshown_link(&request.username, path).await
                && let Some(song) = pool.wait_for_path(path, library).await
            {
                if missed {
                    let pool_file = pool
                        .pool_path_of_link(&request.username, path)
                        .unwrap_or_else(|| path.to_string());
                    pool.copy_shown_where_link_was_not(&pool_file).await;
                }
                self.stars.star(&request.username, &song.id).await;
                self.done(request, Some(outcome), Some(song.id));
                return;
            }
        }
        self.failed(request, NOT_SHOWN_MESSAGE);
    }

    fn done(
        &self,
        request: &FamilyRequest,
        outcome: Option<RequestOutcome>,
        library_song_id: Option<String>,
    ) {
        // A request canceled meanwhile (its member left the family) stays canceled.
        let Ok(done) = self.requests.update_if(
            &request.id,
            |r| r.state == RequestState::Approved,
            |r| {
                r.state = RequestState::Done;
                r.outcome = outcome;
                if library_song_id.is_some() {
                    r.library_song_id = library_song_id.clone();
                }
            },
        ) else {
            return;
        };
        info!("FAMILY request {} {} done ({outcome:?})", done.id, done.username);
        self.events.send(FamilyEvent::RequestDone(done));
    }

    fn failed(&self, request: &FamilyRequest, reason: &str) {
        let Ok(failed) = self.requests.update_if(
            &request.id,
            |r| r.state == RequestState::Approved,
            |r| {
                r.state = RequestState::Failed;
                r.failure = Some(reason.to_string());
            },
        ) else {
            return;
        };
        warn!(
            "FAMILY request {} {} failed: {reason}",
            failed.id, failed.username
        );
        self.events.send(FamilyEvent::RequestFailed(failed));
    }
}

/// What an approver reads when a request could not be decided: there is none, or it was
/// decided already.
fn already_decided(refused: Option<Box<FamilyRequest>>) -> RequestError {
    match refused {
        Some(_) => RequestError::new(50, "This request was already decided."),
        None => RequestError::new(70, "There is no such request."),
    }
}

/// Whether `request` is `user`'s same ask (kind, target and quality; any quality for an import
/// list) and still waiting or approved.
fn same_live_ask(
    request: &FamilyRequest,
    user: &str,
    kind: RequestKind,
    target: &str,
    quality: RequestQuality,
) -> bool {
    eq_ignore_case(&request.username, user)
        && request.kind == kind
        && request.target == target
        && (request.quality == quality || kind == RequestKind::Import)
        && matches!(request.state, RequestState::Pending | RequestState::Approved)
}

/// A request's titles from the song it asks for.
fn song_facts(song: Option<&Song>, target: &str) -> RequestFacts {
    RequestFacts {
        title: song.map(|s| s.title.clone()).unwrap_or_default(),
        artist: song.map(|s| s.artist.clone()).unwrap_or_default(),
        album: song.map(|s| s.album.clone()).unwrap_or_default(),
        cover_art: target.to_string(),
    }
}

/// [`RequestCatalog`] over the catalog, the shared library's ownership check and Octo's ids.
pub struct LiveRequestCatalog {
    pub metadata: Arc<dyn IMusicMetadataService>,
    pub ownership: Arc<LibraryOwnership>,
    pub library: Arc<dyn ILocalLibraryService>,
    pub registry: Arc<ExternalIdRegistry>,
}

#[async_trait]
impl RequestCatalog for LiveRequestCatalog {
    fn outside(&self, id: &str) -> Option<OutsideTarget> {
        let (is_external, provider, external_id) = self.library.parse_song_id(id);
        let album = self
            .registry
            .lookup(id)
            .is_some_and(|routing| routing.snapshot().kind == RoutingKind::Album);
        if album {
            return Some(OutsideTarget {
                kind: RequestKind::Album,
                provider: provider.unwrap_or_else(|| SoulseekMetadataService::PROVIDER_NAME.to_string()),
                external_id: id.to_string(),
            });
        }
        is_external.then(|| OutsideTarget {
            kind: RequestKind::Song,
            provider: provider.unwrap_or_default(),
            external_id: external_id.unwrap_or_default(),
        })
    }

    async fn song(&self, provider: &str, external_id: &str) -> Option<Song> {
        self.metadata.get_song(provider, external_id).await
    }

    async fn album(&self, provider: &str, external_id: &str) -> Option<Album> {
        self.metadata.get_album(provider, external_id).await
    }

    async fn shared_copy(&self, song: &Song) -> Option<OwnedCopy> {
        self.ownership
            .find_song_in(&super::LibraryScope::Shared, song)
            .await
    }
}

/// [`RequestFetcher`] over the acquisition queue, the heart chain, the upgrade queue and the
/// imports.
pub struct LiveRequestFetcher {
    pub queue: Arc<TrackAcquisitionQueue>,
    pub coordinator: Arc<HeartAcquisitionCoordinator>,
    pub tracker: Arc<AcquisitionTracker>,
    pub upgrades: Arc<UpgradeQueue>,
    imports: OnceLock<Weak<ImportService>>,
}

impl LiveRequestFetcher {
    pub fn new(
        queue: Arc<TrackAcquisitionQueue>,
        coordinator: Arc<HeartAcquisitionCoordinator>,
        tracker: Arc<AcquisitionTracker>,
        upgrades: Arc<UpgradeQueue>,
    ) -> Self {
        LiveRequestFetcher {
            queue,
            coordinator,
            tracker,
            upgrades,
            imports: OnceLock::new(),
        }
    }

    /// The imports an approved import request hands its list to.
    pub fn use_imports(&self, imports: &Arc<ImportService>) {
        let _ = self.imports.set(Arc::downgrade(imports));
    }
}

#[async_trait]
impl RequestFetcher for LiveRequestFetcher {
    fn in_flight(&self, provider: &str, external_id: &str) -> bool {
        self.queue.is_in_flight(provider, external_id)
    }

    async fn song(
        &self,
        user: &str,
        provider: &str,
        external_id: &str,
        quality: RequestQuality,
        song: Option<&Song>,
    ) -> Fetched {
        self.tracker.begin(
            provider,
            external_id,
            None,
            Some(user),
            song.map(|s| s.artist.as_str()),
            song.map(|s| s.title.as_str()),
            song.map(|s| s.album.as_str()),
        );
        let target = AcquisitionTarget::Member(user.to_string());
        let source = match quality {
            RequestQuality::Best => {
                return match self
                    .coordinator
                    .acquire_track_outcome(
                        provider,
                        external_id,
                        Some(user),
                        None,
                        &target,
                        AcquisitionOrigin::Request,
                    )
                    .await
                {
                    Ok(TrackOutcome::Landed(path) | TrackOutcome::KeptAsHeard(path)) => Fetched::Landed(path),
                    Ok(TrackOutcome::AlreadyThere) => Fetched::AlreadyThere,
                    Ok(TrackOutcome::HandedToLidarr) => {
                        Fetched::Failed("Lidarr took the song for the shared library.".into())
                    }
                    Ok(TrackOutcome::Failed(reason)) => Fetched::Failed(reason),
                    // A request never holds: what it would wait for is why it could not be got.
                    Ok(TrackOutcome::Held(waiting)) => Fetched::Failed(
                        crate::services::common::heart_acquisition_coordinator::waiting_line(waiting)
                            .to_string(),
                    ),
                    Err(e) => fetched_failure(&e),
                };
            }
            RequestQuality::Flac => DownloadSource::Soulseek,
            RequestQuality::Mp3 => DownloadSource::YouTube,
        };
        // The request's own gate, so the reading's refusals apply at every quality.
        let gates = self.coordinator.request_gates(provider, external_id, user).await;
        let completion = self.queue.enqueue_as(
            Some(AcquisitionOrigin::Request),
            gates,
            &target,
            quality == RequestQuality::Flac,
            provider,
            external_id,
            true,
            false,
            true,
            Some(source),
            true,
            Some(user),
            false,
            None,
        );
        match completion.wait().await {
            Ok(path) => Fetched::Landed(path),
            Err(e) => {
                let reason = e.to_string();
                self.tracker.fail(provider, external_id, Some(&reason));
                fetched_failure(&e)
            }
        }
    }

    async fn album(&self, user: &str, provider: &str, external_id: &str) -> Vec<String> {
        self.tracker.begin_album(provider, external_id, Some(user));
        self.coordinator
            .acquire_album_for_member(
                provider,
                external_id,
                Some(user),
                user,
                false,
                AcquisitionOrigin::Request,
            )
            .await
    }

    fn upgrade(&self, owner: &str, navidrome_id: &str, facts: &RequestFacts) -> bool {
        let (jobs, _) = self.upgrades.add(
            vec![UpgradeAsk {
                navidrome_id: navidrome_id.to_string(),
                title: Some(facts.title.clone()).filter(|t| !t.is_empty()),
                artist: Some(facts.artist.clone()).filter(|a| !a.is_empty()),
                album: Some(facts.album.clone()).filter(|a| !a.is_empty()),
                suffix: None,
                attempt_key: None,
                pick: None,
            }],
            owner,
            "request",
        );
        !jobs.is_empty()
    }

    fn upgrade_result(&self, navidrome_id: &str) -> Option<Result<(), String>> {
        use crate::services::library::upgrade_queue::UpgradeStates;
        let job = self
            .upgrades
            .snapshot()
            .into_iter()
            .find(|job| job.navidrome_id == navidrome_id)?;
        if UpgradeStates::open(&job.state) {
            return None;
        }
        Some(if job.state == UpgradeStates::UPGRADED {
            Ok(())
        } else {
            Err(job
                .detail
                .unwrap_or_else(|| "No better copy was found.".to_string()))
        })
    }

    async fn import(&self, user: &str, list_id: &str) -> Result<String, String> {
        let Some(imports) = self.imports.get().and_then(Weak::upgrade) else {
            return Err("Imports are not running.".to_string());
        };
        let result = imports.approve_family_list(user, list_id);
        if result.ok {
            Ok(result.message)
        } else {
            Err(result.message)
        }
    }
}

#[cfg(test)]
#[path = "request_service_tests.rs"]
mod tests;
