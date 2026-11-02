//! The discovery catalog a syncing client is handed after the last library row ([`SyncCatalog`]),
//! and the service that builds, caches and pages it ([`SyncCatalogService`]). The page writer
//! that puts the rows on the wire is `octo_subsonic::sync_catalog_response`, whose `append` takes
//! [`SyncCatalog::added`].

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use futures::future::{BoxFuture, Shared};
use futures::{FutureExt, StreamExt};
use indexmap::IndexMap;
use octo_core::common::dotnet::{is_blank, is_null_or_white_space, ordinal_ignore_case_key};
use octo_core::common::{Clock, SongIdentity, skipped_takes};
use octo_core::json::element::{ElementResult, get_string, try_get_property};
use octo_core::last_fm::last_fm_radio_track_resolver::is_same_recording;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioTrack};
use octo_core::settings::{ExplicitFilter, SettingsStore, SubsonicSettings};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::services::family::{LibraryScope, ScopeResolver};
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::subsonic::SubsonicProxyService;

/// Which kind of row a sync walk pages through (one kind per walk).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncCatalogKind {
    Artist,
    Album,
    Song,
}

impl SyncCatalogKind {
    /// The kind's name, as the walk key spells it.
    fn name(self) -> &'static str {
        match self {
            SyncCatalogKind::Artist => "Artist",
            SyncCatalogKind::Album => "Album",
            SyncCatalogKind::Song => "Song",
        }
    }
}

/// One user's discovery catalog: the placeholder songs a syncing client is handed after the
/// last library row, and the albums and artists those songs need to exist on the device.
/// Immutable, so a walk can hold on to the one it started with while a rebuild swaps in.
#[derive(Debug)]
pub struct SyncCatalog {
    songs: Vec<Song>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    added: HashMap<String, DateTime<Utc>>,
    fingerprint: String,
    built_utc: DateTime<Utc>,
    /// Index into `songs` per id; the first song of an id wins (`TryAdd`).
    songs_by_id: HashMap<String, usize>,
}

/// The empty catalog, one shared instance.
static EMPTY: LazyLock<Arc<SyncCatalog>> = LazyLock::new(|| {
    Arc::new(SyncCatalog::new(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        String::new(),
        min_value(),
    ))
});

/// The earliest time: midnight, 1 January of the year 1.
fn min_value() -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .expect("the year 1 is a valid date")
        .and_utc()
}

impl SyncCatalog {
    pub fn new(
        songs: Vec<Song>,
        albums: Vec<Album>,
        artists: Vec<Artist>,
        added: HashMap<String, DateTime<Utc>>,
        fingerprint: String,
        built_utc: DateTime<Utc>,
    ) -> SyncCatalog {
        let mut songs_by_id = HashMap::with_capacity(songs.len());
        for (index, song) in songs.iter().enumerate() {
            songs_by_id.entry(song.id.clone()).or_insert(index);
        }
        SyncCatalog {
            songs,
            albums,
            artists,
            added,
            fingerprint,
            built_utc,
            songs_by_id,
        }
    }

    /// The empty catalog.
    pub fn empty() -> Arc<SyncCatalog> {
        EMPTY.clone()
    }

    pub fn songs(&self) -> &[Song] {
        &self.songs
    }

    pub fn albums(&self) -> &[Album] {
        &self.albums
    }

    pub fn artists(&self) -> &[Artist] {
        &self.artists
    }

    /// When each song and album id first entered this user's catalog. Rows go out
    /// with this as their `created` date rather than "now", or every sync would present
    /// the whole catalog as the newest additions to the library and bury real downloads.
    /// This is what `octo_subsonic::sync_catalog_response::append` takes as `added`.
    pub fn added(&self) -> &HashMap<String, DateTime<Utc>> {
        &self.added
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn built_utc(&self) -> DateTime<Utc> {
        self.built_utc
    }

    pub fn try_get_song(&self, id: &str) -> Option<&Song> {
        self.songs_by_id.get(id).map(|&index| &self.songs[index])
    }

    pub fn count(&self, kind: SyncCatalogKind) -> usize {
        match kind {
            SyncCatalogKind::Artist => self.artists.len(),
            SyncCatalogKind::Album => self.albums.len(),
            SyncCatalogKind::Song => self.songs.len(),
        }
    }
}

/// How long a built catalog is reused while its stations are unchanged. Long
/// enough that a sync does not rebuild per page, short enough that a track hearted and
/// downloaded drops out of the catalog on a later sync the same day.
pub(crate) const CATALOG_TTL: Duration = Duration::from_secs(60 * 60);

/// How long a walk's pinned catalog and library size are trusted. A sync of a
/// large library over a slow link is minutes, not hours.
pub(crate) const WALK_TTL: Duration = Duration::from_secs(20 * 60);

const LOOKUP_CONCURRENCY: usize = 4;

/// `now - at < ttl` (a clock that went back reads as fresh).
fn younger_than(now: DateTime<Utc>, at: DateTime<Utc>, ttl: Duration) -> bool {
    now.signed_duration_since(at) < TimeDelta::from_std(ttl).expect("the TTLs fit a TimeDelta")
}

struct WalkMemo {
    local_total: i32,
    catalog: Arc<SyncCatalog>,
    at_utc: DateTime<Utc>,
}

/// A build in flight: its fingerprint and the shared build. `done` says whether the build has
/// finished, set by the build itself (a `Shared` future only knows it has finished once someone
/// polled it).
#[derive(Clone)]
struct RunningBuild {
    fingerprint: String,
    task: Shared<BoxFuture<'static, Arc<SyncCatalog>>>,
    done: Arc<AtomicBool>,
}

/// Discovery for clients that never send a search to the server.
///
/// Symfonium copies the whole library to the device by paging search3 with an empty query
/// (`query=""&songCount=1000&songOffset=N`, one kind per walk) and searches that
/// copy offline, so a typed query never reaches Octo and search-time discovery has nothing
/// to add to. The walk itself does reach Octo. This extends it past the last library row
/// with the tracks of the user's radio stations that the library does not own, as ordinary
/// placeholder songs. On the device they are then searchable, browsable and playable like
/// anything else, the station playlists resolve against rows the device already has, and a
/// heart downloads the track the same way it does from any other client.
///
/// The walk is paged by offset with no total, so the catalog is addressed as rows that sit
/// after the library: [`SyncCatalogService::window`] turns a page request into a slice of it. A
/// walk pins the catalog it started on, so a rebuild finishing half way through does not shift
/// rows under it.
pub struct SyncCatalogService {
    /// The background proxy: a build's relays carry no client request.
    proxy: SubsonicProxyService,
    metadata: Arc<dyn IMusicMetadataService>,
    registry: Arc<ExternalIdRegistry>,
    settings: Arc<SettingsStore>,
    clock: Clock,

    /// Per user, keyed by the user name under `OrdinalIgnoreCase`.
    catalogs: Mutex<HashMap<String, Arc<SyncCatalog>>>,
    builds: Mutex<HashMap<String, RunningBuild>>,
    walks: Mutex<HashMap<String, WalkMemo>>,

    /// Which library each song is in, while Family is on: a song only in a family member's own
    /// library is not owned by anyone else, so their catalog keeps its outside version.
    scope: Mutex<Option<ScopeResolver>>,
}

/// What the library holds for one artist: its id, its albums by name, and its
/// recordings. `None` from the parser when the lookup failed, which leaves that artist's tracks
/// out rather than risk handing the device a copy of a song it already has.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LibraryArtist {
    pub artist_id: Option<String>,
    pub album_ids: HashMap<String, String>,
    pub songs: Vec<(String, String)>,
}

impl LibraryArtist {
    pub fn owns(&self, artist: &str, title: &str) -> bool {
        self.songs
            .iter()
            .any(|(song_artist, song_title)| is_same_recording(artist, title, song_artist, song_title))
    }
}

impl SyncCatalogService {
    pub fn new(
        proxy: SubsonicProxyService,
        metadata: Arc<dyn IMusicMetadataService>,
        registry: Arc<ExternalIdRegistry>,
        settings: Arc<SettingsStore>,
        clock: Clock,
    ) -> SyncCatalogService {
        SyncCatalogService {
            proxy,
            metadata,
            registry,
            settings,
            clock,
            catalogs: Mutex::new(HashMap::new()),
            builds: Mutex::new(HashMap::new()),
            walks: Mutex::new(HashMap::new()),
            scope: Mutex::new(None),
        }
    }

    /// Keeps what each person owns to their view of the library while Family is on: the
    /// shared library, and a member's own songs for that member only.
    pub fn use_library_scope(&self, scope: ScopeResolver) {
        *self.scope.lock() = Some(scope);
    }

    /// A library lookup's answer as `user` may see it: with Family on, songs from another
    /// member's own library go, and so do albums and artists only those songs had. Unchanged
    /// with Family off or for anyone Octo cannot name.
    pub(crate) fn scoped_answer(&self, user: Option<&str>, body: &[u8]) -> Option<Vec<u8>> {
        let resolver = self.scope.lock().clone().filter(ScopeResolver::active)?.view();
        let user = user.filter(|u| !u.trim().is_empty())?;
        let scope = LibraryScope::Viewer(user.to_string());
        let mut document: Value = serde_json::from_slice(body).ok()?;
        let result = document.pointer_mut("/subsonic-response/searchResult3")?;
        let songs = result.get_mut("song")?.as_array_mut()?;
        let (mut kept_albums, mut kept_artists) = (HashSet::new(), HashSet::new());
        let (mut gone_albums, mut gone_artists) = (HashSet::new(), HashSet::new());
        songs.retain(|song| {
            let id = song.get("id").and_then(Value::as_str);
            let keep = resolver.allows(&scope, id, None);
            let (albums, artists) = if keep {
                (&mut kept_albums, &mut kept_artists)
            } else {
                (&mut gone_albums, &mut gone_artists)
            };
            if let Some(album) = song.get("albumId").and_then(Value::as_str) {
                albums.insert(album.to_string());
            }
            if let Some(artist) = song.get("artistId").and_then(Value::as_str) {
                artists.insert(artist.to_string());
            }
            keep
        });
        let only_gone = |gone: &HashSet<String>, kept: &HashSet<String>, row: &Value| {
            row.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| gone.contains(id) && !kept.contains(id))
        };
        if let Some(albums) = result.get_mut("album").and_then(Value::as_array_mut) {
            albums.retain(|album| !only_gone(&gone_albums, &kept_albums, album));
        }
        if let Some(artists) = result.get_mut("artist").and_then(Value::as_array_mut) {
            artists.retain(|artist| !only_gone(&gone_artists, &kept_artists, artist));
        }
        serde_json::to_vec(&document).ok()
    }

    /// Whether a request comes from a client that syncs the library rather than searching
    /// the server. Decided by the `c` parameter because nothing about the request tells
    /// a sync walk apart from an ordinary "all songs" listing, and handing catalog rows to a
    /// client that lists songs online would put suggestions into its library views.
    pub fn is_sync_client(settings: &SubsonicSettings, client: Option<&str>) -> bool {
        let Some(client) = client.filter(|c| !is_blank(c)) else {
            return false;
        };
        if !settings.enable_sync_catalog {
            return false;
        }
        // A substring match ignoring case: "Symfonium (Android)" is Symfonium.
        let client = ordinal_ignore_case_key(client);
        settings
            .sync_catalog_clients
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .any(|name| client.contains(&ordinal_ignore_case_key(name)))
    }

    // ---------------------------------------------------------------------------------
    // Paging. A walk asks for `count` rows at `offset` and gets `local_returned` from the
    // library; the virtual list is the library followed by the catalog.
    // ---------------------------------------------------------------------------------

    /// The library's size when this page alone proves it: a short page that still
    /// had library rows on it ends exactly where the library does, and so does an empty
    /// first page. An empty later page only says the library ended at or before it.
    pub fn local_total_from_page(offset: i32, local_returned: i32) -> Option<i32> {
        (local_returned > 0 || offset == 0).then(|| offset.wrapping_add(local_returned))
    }

    /// The catalog slice that fills the rest of a page, given the library size.
    pub fn window(offset: i32, count: i32, local_returned: i32, local_total: i32) -> (i32, i32) {
        (
            0.max(offset.wrapping_add(local_returned).wrapping_sub(local_total)),
            0.max(count.wrapping_sub(local_returned)),
        )
    }

    /// The library's size for an empty page past its end, which the page cannot say. The
    /// size remembered from earlier in the walk is used when two probes confirm it (the row
    /// before it exists, the row at it does not); otherwise it is found by bisection, since
    /// clients that fetch pages in parallel can ask for one past the end before the page
    /// that would have told us. `None` when a probe fails, so nothing is appended on a guess.
    pub async fn resolve_local_total<F, Fut>(
        empty_offset: i32,
        remembered: Option<i32>,
        mut exists: F,
    ) -> Option<i32>
    where
        F: FnMut(i32) -> Fut,
        Fut: Future<Output = Option<bool>>,
    {
        if let Some(candidate) = remembered
            && candidate >= 0
            && candidate <= empty_offset
        {
            let before = if candidate == 0 {
                Some(true)
            } else {
                exists(candidate - 1).await
            };
            let at = if candidate == empty_offset {
                Some(false)
            } else {
                exists(candidate).await
            };
            let (Some(before), Some(at)) = (before, at) else {
                return None;
            };
            if before && !at {
                return Some(candidate);
            }
        }

        let (mut low, mut high) = (0, empty_offset);
        while low < high {
            let middle = low + (high - low) / 2;
            if exists(middle).await? {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Some(low)
    }

    pub fn remembered_local_total(&self, username: &str, kind: SyncCatalogKind) -> Option<i32> {
        let now = self.clock.now();
        self.walks
            .lock()
            .get(&walk_key(username, kind))
            .filter(|memo| fresh(memo, now))
            .map(|memo| memo.local_total)
    }

    /// The catalog this walk started on, for its later pages.
    pub fn pinned_catalog(&self, username: &str, kind: SyncCatalogKind) -> Option<Arc<SyncCatalog>> {
        let now = self.clock.now();
        self.walks
            .lock()
            .get(&walk_key(username, kind))
            .filter(|memo| fresh(memo, now))
            .map(|memo| memo.catalog.clone())
    }

    pub fn remember(
        &self,
        username: &str,
        kind: SyncCatalogKind,
        local_total: i32,
        catalog: Arc<SyncCatalog>,
    ) {
        let memo = WalkMemo {
            local_total,
            catalog,
            at_utc: self.clock.now(),
        };
        self.walks.lock().insert(walk_key(username, kind), memo);
    }

    pub fn slice(
        catalog: &SyncCatalog,
        kind: SyncCatalogKind,
        start: i32,
        take: i32,
    ) -> (Vec<Song>, Vec<Album>, Vec<Artist>) {
        fn rows<T: Clone>(rows: &[T], start: i32, take: i32) -> Vec<T> {
            // `Skip` of a negative count skips nothing.
            let start = usize::try_from(start).unwrap_or(0);
            let take = usize::try_from(take).unwrap_or(0);
            if start >= rows.len() || take == 0 {
                return Vec::new();
            }
            rows[start..].iter().take(take).cloned().collect()
        }
        match kind {
            SyncCatalogKind::Artist => (Vec::new(), Vec::new(), rows(&catalog.artists, start, take)),
            SyncCatalogKind::Album => (Vec::new(), rows(&catalog.albums, start, take), Vec::new()),
            SyncCatalogKind::Song => (rows(&catalog.songs, start, take), Vec::new(), Vec::new()),
        }
    }

    // ---------------------------------------------------------------------------------
    // Building.
    // ---------------------------------------------------------------------------------

    /// The user's catalog as last built, if any. Lets other responses describe a
    /// catalog song exactly as the sync did.
    pub fn try_get_song(&self, username: &str, id: &str) -> Option<Song> {
        let catalog = self
            .catalogs
            .lock()
            .get(&ordinal_ignore_case_key(username))
            .cloned()?;
        catalog.try_get_song(id).cloned()
    }

    /// Starts a build if the cached catalog is stale, without waiting for it.
    pub fn warm(
        self: &Arc<Self>,
        username: &str,
        stations: &[LastFmRadioStation],
        authenticated_parameters: &IndexMap<String, String>,
    ) {
        // The build runs on its own task; dropping the answer does not stop it.
        drop(self.get(username, stations, authenticated_parameters));
    }

    /// The user's catalog for these stations: the cached one while the stations are unchanged and
    /// it is younger than [`CATALOG_TTL`], else a build. One build per user at a time, shared by
    /// every caller, and never canceled by the request that started it, because the next page of
    /// the same walk is about to want it.
    ///
    /// The decision and the start of a build happen before this returns; only waiting for the
    /// catalog is left to the returned future.
    pub fn get(
        self: &Arc<Self>,
        username: &str,
        stations: &[LastFmRadioStation],
        authenticated_parameters: &IndexMap<String, String>,
    ) -> BoxFuture<'static, Arc<SyncCatalog>> {
        if username.is_empty() || stations.is_empty() {
            return futures::future::ready(SyncCatalog::empty()).boxed();
        }
        let settings = self.settings.current();
        let fingerprint = Self::fingerprint(
            stations,
            &settings.subsonic,
            settings.soulseek.skip_covers_and_instrumentals,
        );
        let key = ordinal_ignore_case_key(username);
        let cached = self.catalogs.lock().get(&key).cloned();
        if let Some(cached) = cached
            && cached.fingerprint == fingerprint
            && younger_than(self.clock.now(), cached.built_utc, CATALOG_TTL)
        {
            return futures::future::ready(cached).boxed();
        }

        // One lock over the check and the start: two callers must not both start a build.
        let mut builds = self.builds.lock();
        if let Some(running) = builds.get(&key)
            && running.fingerprint == fingerprint
            && !running.done.load(Ordering::Acquire)
        {
            return running.task.clone().boxed();
        }
        let running = self.start_build(
            username,
            stations.to_vec(),
            authenticated_parameters.clone(),
            fingerprint,
        );
        builds.insert(key, running.clone());
        running.task.boxed()
    }

    fn start_build(
        self: &Arc<Self>,
        username: &str,
        stations: Vec<LastFmRadioStation>,
        auth: IndexMap<String, String>,
        fingerprint: String,
    ) -> RunningBuild {
        // Detached from the request that asked: the build outlives it.
        let done = Arc::new(AtomicBool::new(false));
        let service = self.clone();
        let user = username.to_string();
        let finished = done.clone();
        let build_fingerprint = fingerprint.clone();
        let handle = tokio::spawn(async move {
            let key = ordinal_ignore_case_key(&user);
            let previous = service.catalogs.lock().get(&key).cloned();
            let outcome =
                AssertUnwindSafe(service.build(&user, &stations, &auth, &build_fingerprint, previous))
                    .catch_unwind()
                    .await;
            let catalog = match outcome {
                Ok(built) => {
                    let built = Arc::new(built);
                    service.catalogs.lock().insert(key, built.clone());
                    info!(
                        "Sync catalog for {user}: {} songs, {} albums, {} artists from {} stations",
                        built.songs.len(),
                        built.albums.len(),
                        built.artists.len(),
                        stations.len()
                    );
                    built
                }
                Err(_) => {
                    warn!("Sync catalog build failed for {user}");
                    service
                        .catalogs
                        .lock()
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(SyncCatalog::empty)
                }
            };
            finished.store(true, Ordering::Release);
            catalog
        });
        // Only a runtime shutting down drops the build's task; the waiters then get nothing.
        let task = async move { handle.await.unwrap_or_else(|_| SyncCatalog::empty()) }
            .boxed()
            .shared();
        RunningBuild {
            fingerprint,
            task,
            done,
        }
    }

    /// What decides whether a built catalog still holds: each station's last change, the cap, the
    /// explicit filter, and Skip covers and instrumentals (named only while it is on).
    pub(crate) fn fingerprint(
        stations: &[LastFmRadioStation],
        settings: &SubsonicSettings,
        skip_covers_and_instrumentals: bool,
    ) -> String {
        let stations = stations
            .iter()
            .map(|station| format!("{}:{}", station.id, dotnet_ticks(station.changed_utc)))
            .collect::<Vec<_>>()
            .join("|");
        format!(
            "{stations}#{}#{}{}",
            settings.effective_sync_catalog_max_songs(),
            explicit_filter_name(settings.explicit_filter),
            if skip_covers_and_instrumentals {
                "#SkipCoversAndInstrumentals"
            } else {
                ""
            }
        )
    }

    /// Station tracks in round-robin order, one from each station in turn, de-duplicated. A
    /// cap then trims every station a little rather than dropping the last stations whole.
    pub(crate) fn interleave(stations: &[LastFmRadioStation], limit: i32) -> Vec<LastFmRadioTrack> {
        let limit = usize::try_from(limit).unwrap_or(0);
        let mut output = Vec::new();
        let mut seen = HashSet::new();
        let depth = stations
            .iter()
            .map(|station| station.tracks.len())
            .max()
            .unwrap_or(0);
        let mut index = 0;
        while index < depth && output.len() < limit {
            for station in stations {
                if index >= station.tracks.len() || output.len() >= limit {
                    continue;
                }
                let track = &station.tracks[index];
                if is_blank(&track.artist) || is_blank(&track.title) {
                    continue;
                }
                if seen.insert(SongIdentity::match_key(&track.artist, &track.title)) {
                    output.push(track.clone());
                }
            }
            index += 1;
        }
        output
    }

    async fn build(
        &self,
        username: &str,
        stations: &[LastFmRadioStation],
        auth: &IndexMap<String, String>,
        fingerprint: &str,
        previous: Option<Arc<SyncCatalog>>,
    ) -> SyncCatalog {
        let current = self.settings.current();
        let settings = current.subsonic.clone();
        let skip_covers_and_instrumentals = current.soulseek.skip_covers_and_instrumentals;
        let tracks = Self::interleave(stations, settings.effective_sync_catalog_max_songs());
        if tracks.is_empty() {
            return SyncCatalog::new(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                HashMap::new(),
                fingerprint.to_string(),
                self.clock.now(),
            );
        }

        // One library lookup per artist answers everything the catalog needs to know: which
        // of the artist's tracks are owned (they are already on the device), and the ids to
        // file the rest under so an artist or album the user owns does not appear twice.
        let mut seen_artists = HashSet::new();
        let lookups: Vec<_> = tracks
            .iter()
            .filter(|track| seen_artists.insert(SongIdentity::key(&track.artist)))
            .map(|track| self.lookup_keyed(username, auth, track.artist.clone()))
            .collect();
        let library: HashMap<String, Option<LibraryArtist>> = futures::stream::iter(lookups)
            .buffer_unordered(LOOKUP_CONCURRENCY)
            .collect()
            .await;

        let mut songs: Vec<Song> = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut local_ids = HashSet::new();
        for track in &tracks {
            let Some(Some(owned)) = library.get(&SongIdentity::key(&track.artist)) else {
                continue;
            };
            if owned.owns(&track.artist, &track.title) {
                continue;
            }

            // A chart list's song is known by the id the list gives it; a station's is found by
            // the same call, with the same duration, that the station playlist makes, so the
            // placeholder id here is the id the playlist lists.
            let listed = (track.source == crate::services::library::chart_playlist_service::TRACK_SOURCE)
                .then(|| track.resolved_id.as_deref().filter(|id| !id.is_empty()))
                .flatten();
            let hit = match listed {
                Some(id) => {
                    self.metadata
                        .get_song(
                            crate::services::soulseek::soulseek_metadata_service::SoulseekMetadataService::PROVIDER_NAME,
                            id,
                        )
                        .await
                }
                None => self
                    .metadata
                    .search_songs_by_artist_title(&track.artist, &track.title, 1, track.duration)
                    .await
                    .into_iter()
                    .next(),
            };
            let Some(mut hit) = hit else {
                continue;
            };
            if hit.id.is_empty() || !seen_ids.insert(hit.id.clone()) {
                continue;
            }
            if settings.explicit_filter == ExplicitFilter::CleanOnly
                && !octo_core::models::domain::explicit_status::clean_only_allows(hit.explicit_content_lyrics)
            {
                continue;
            }
            if settings.explicit_filter == ExplicitFilter::ExplicitOnly
                && hit.explicit_content_lyrics == Some(3)
            {
                continue;
            }
            if skipped_takes::hides_song(&hit, skip_covers_and_instrumentals) {
                continue;
            }

            // Same fallback the response builder applies, made here so the album rows and the
            // songs agree on it: a single is its own album.
            let album = if !is_blank(&hit.album) {
                hit.album.clone()
            } else if !is_null_or_white_space(track.album.as_deref()) {
                track.album.clone().unwrap_or_default()
            } else {
                hit.title.clone()
            };
            hit.album = album.clone();
            if hit.genre.is_none() {
                hit.genre = track.genre.clone();
            }
            if hit.year.is_none() {
                hit.year = track.year;
            }
            if hit.duration.is_none() {
                hit.duration = track.duration;
            }

            match owned.artist_id.as_deref() {
                Some(artist_id) if !artist_id.is_empty() => {
                    hit.artist_id = Some(artist_id.to_string());
                    local_ids.insert(artist_id.to_string());
                }
                _ => {
                    hit.artist_id = Some(self.registry.register(SoulseekRouting {
                        kind: RoutingKind::Artist,
                        artist: Some(hit.artist.clone()),
                        ..Default::default()
                    }));
                }
            }

            let library_album = owned
                .artist_id
                .as_ref()
                .and_then(|_| owned.album_ids.get(&SongIdentity::key(&album)));
            match library_album {
                Some(album_id) => {
                    hit.album_id = Some(album_id.clone());
                    local_ids.insert(album_id.clone());
                }
                None => {
                    hit.album_id = Some(self.registry.register(SoulseekRouting {
                        kind: RoutingKind::Album,
                        artist: Some(hit.artist.clone()),
                        album: Some(album),
                        ..Default::default()
                    }));
                }
            }

            songs.push(hit);
        }

        let albums: Vec<Album> = group_by_id(
            songs
                .iter()
                .filter(|song| !local_ids.contains(id_of(&song.album_id))),
            |song| id_of(&song.album_id),
        )
        .into_iter()
        .map(|(id, group)| {
            let first = group[0];
            Album {
                id: id.to_string(),
                title: first.album.clone(),
                artist: first.artist.clone(),
                artist_id: first.artist_id.clone(),
                song_count: Some(group.len() as i32),
                year: group.iter().find_map(|song| song.year),
                genre: group
                    .iter()
                    .map(|song| song.genre.as_deref())
                    .find(|genre| !is_null_or_white_space(*genre))
                    .flatten()
                    .map(str::to_string),
                is_local: false,
                external_provider: first.external_provider.clone(),
                ..Default::default()
            }
        })
        .collect();

        let catalog_artists: Vec<Artist> = group_by_id(
            songs
                .iter()
                .filter(|song| !local_ids.contains(id_of(&song.artist_id))),
            |song| id_of(&song.artist_id),
        )
        .into_iter()
        .map(|(id, group)| {
            let album_count = group
                .iter()
                .map(|song| song.album_id.as_deref())
                .collect::<HashSet<_>>()
                .len();
            Artist {
                id: id.to_string(),
                name: group[0].artist.clone(),
                album_count: Some(album_count as i32),
                is_local: false,
                external_provider: group[0].external_provider.clone(),
                ..Default::default()
            }
        })
        .collect();

        let now = self.clock.now();
        let mut added = HashMap::new();
        let ids = songs
            .iter()
            .map(|song| &song.id)
            .chain(albums.iter().map(|album| &album.id))
            .chain(catalog_artists.iter().map(|artist| &artist.id));
        for id in ids {
            let first = previous
                .as_ref()
                .and_then(|previous| previous.added.get(id).copied())
                .unwrap_or(now);
            added.insert(id.clone(), first);
        }

        SyncCatalog::new(
            songs,
            albums,
            catalog_artists,
            added,
            fingerprint.to_string(),
            now,
        )
    }

    /// One artist's lookup, under the key the build reads it by.
    async fn lookup_keyed(
        &self,
        username: &str,
        auth: &IndexMap<String, String>,
        artist: String,
    ) -> (String, Option<LibraryArtist>) {
        (
            SongIdentity::key(&artist),
            self.lookup_artist(username, auth, &artist).await,
        )
    }

    /// One artist's library lookup, scoped to the listener the catalog is built for. The name
    /// is the listener the request resolved, which an API-key app has no `u` parameter to say.
    async fn lookup_artist(
        &self,
        username: &str,
        auth: &IndexMap<String, String>,
        artist: &str,
    ) -> Option<LibraryArtist> {
        let mut parameters = auth.clone();
        parameters.insert("query".into(), artist.to_string());
        parameters.insert("artistCount".into(), "10".into());
        parameters.insert("artistOffset".into(), "0".into());
        parameters.insert("albumCount".into(), "200".into());
        parameters.insert("albumOffset".into(), "0".into());
        parameters.insert("songCount".into(), "500".into());
        parameters.insert("songOffset".into(), "0".into());
        parameters.insert("f".into(), "json".into());
        let result = self.proxy.relay_safe("rest/search3", &parameters).await?;
        if result.body.is_empty() {
            return None;
        }
        let scoped = self.scoped_answer(Some(username), &result.body);
        let body = scoped.as_deref().unwrap_or(&result.body);
        match Self::parse_library_artist(body, artist) {
            Ok(owned) => owned,
            Err(error) => {
                debug!("sync catalog library lookup failed for {artist}: {error}");
                None
            }
        }
    }

    /// Reads a Navidrome search3 answer for an artist name. Only an exact (normalized)
    /// name counts as the same artist: a search for "Air" also returns "Airbourne".
    ///
    /// An error for a body that is not JSON, or a value of the wrong kind; the lookup reads it
    /// as a failed lookup.
    pub(crate) fn parse_library_artist(body: &[u8], artist: &str) -> anyhow::Result<Option<LibraryArtist>> {
        let document: Value = serde_json::from_slice(body)?;
        let Some(response) = try_get_property(&document, "subsonic-response")? else {
            return Ok(None);
        };
        if let Some(status) = try_get_property(response, "status")?
            && get_string(status)? != Some("ok")
        {
            return Ok(None);
        }
        let want = SongIdentity::key(artist);
        let Some(result) = try_get_property(response, "searchResult3")? else {
            return Ok(Some(LibraryArtist {
                artist_id: None,
                album_ids: HashMap::new(),
                songs: Vec::new(),
            }));
        };

        let mut artist_id = None;
        for row in rows(result, "artist")? {
            if SongIdentity::key(&text(row, "name")?) != want {
                continue;
            }
            let id = text(row, "id")?;
            if !id.is_empty() {
                artist_id = Some(id);
                break;
            }
        }

        let mut albums = HashMap::new();
        if let Some(artist_id) = &artist_id {
            for row in rows(result, "album")? {
                let by_artist =
                    text(row, "artistId")? == *artist_id || SongIdentity::key(&text(row, "artist")?) == want;
                let name = SongIdentity::key(&text(row, "name")?);
                let id = text(row, "id")?;
                if by_artist && !name.is_empty() && !id.is_empty() {
                    albums.entry(name).or_insert(id);
                }
            }
        }

        let songs = rows(result, "song")?
            .iter()
            .map(|row| Ok((text(row, "artist")?, text(row, "title")?)))
            .collect::<ElementResult<Vec<_>>>()?;
        Ok(Some(LibraryArtist {
            artist_id,
            album_ids: albums,
            songs,
        }))
    }
}

/// The walk key: the user and the kind, compared ignoring case.
fn walk_key(username: &str, kind: SyncCatalogKind) -> String {
    ordinal_ignore_case_key(&format!("{username}\n{}", kind.name()))
}

fn fresh(memo: &WalkMemo, now: DateTime<Utc>) -> bool {
    younger_than(now, memo.at_utc, WALK_TTL)
}

/// The filter's name, as the fingerprint spells it.
fn explicit_filter_name(filter: ExplicitFilter) -> &'static str {
    match filter {
        ExplicitFilter::All => "All",
        ExplicitFilter::ExplicitOnly => "ExplicitOnly",
        ExplicitFilter::CleanOnly => "CleanOnly",
    }
}

/// A catalog song's album or artist id, which the build always sets.
fn id_of(id: &Option<String>) -> &str {
    id.as_deref().unwrap_or_default()
}

/// `GroupBy` by an ordinal key: the groups in the order their first member came.
fn group_by_id<'a>(
    songs: impl Iterator<Item = &'a Song>,
    key: impl Fn(&'a Song) -> &'a str,
) -> Vec<(&'a str, Vec<&'a Song>)> {
    let mut groups: Vec<(&str, Vec<&Song>)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for song in songs {
        let id = key(song);
        match index.get(id) {
            Some(&at) => groups[at].1.push(song),
            None => {
                index.insert(id, groups.len());
                groups.push((id, vec![song]));
            }
        }
    }
    groups
}

/// `Text`: the property's string value, or "" when it is missing or not a string.
fn text(element: &Value, name: &str) -> ElementResult<String> {
    Ok(match try_get_property(element, name)? {
        Some(Value::String(value)) => value.clone(),
        _ => String::new(),
    })
}

/// `Rows`: the property's array, or nothing when it is missing or not an array.
fn rows<'a>(parent: &'a Value, name: &str) -> ElementResult<&'a [Value]> {
    Ok(match try_get_property(parent, name)? {
        Some(Value::Array(rows)) => rows,
        _ => &[],
    })
}

#[cfg(test)]
#[path = "sync_catalog_service_tests.rs"]
mod tests;

#[cfg(test)]
mod family_scope_tests {
    use serde_json::json;

    use super::*;
    use crate::services::common::test_fakes::FakeMetadata;
    use crate::services::family::library_index::scope_for_tests;
    use crate::services::family::test_support::{World, navidrome_with_songs, song_row};

    fn service(scope: ScopeResolver) -> SyncCatalogService {
        let settings = Arc::new(SettingsStore::for_tests(Default::default()));
        let service = SyncCatalogService::new(
            SubsonicProxyService::new(settings.clone()),
            Arc::new(FakeMetadata::default()),
            Arc::new(ExternalIdRegistry::new(None::<&str>)),
            settings,
            Clock::system(),
        );
        service.use_library_scope(scope);
        service
    }

    /// An admin's lookup answer: one shared song, one in Sam's own library on an album and by
    /// an artist nobody else has, and one of Alex's on the shared album.
    fn answer() -> Vec<u8> {
        serde_json::to_vec(&json!({"subsonic-response": {"status": "ok", "searchResult3": {
            "artist": [{"id": "ar-shared", "name": "Portishead"}, {"id": "ar-sam", "name": "Sam's band"}],
            "album": [{"id": "al-shared", "name": "Dummy"}, {"id": "al-sam", "name": "Sam's album"}],
            "song": [
                {"id": "shared-1", "albumId": "al-shared", "artistId": "ar-shared"},
                {"id": "sam-1", "albumId": "al-sam", "artistId": "ar-sam"},
                {"id": "alex-1", "albumId": "al-shared", "artistId": "ar-shared"}
            ]
        }}}))
        .expect("JSON")
    }

    fn ids(body: &[u8], kind: &str) -> Vec<String> {
        let document: Value = serde_json::from_slice(body).expect("JSON");
        document["subsonic-response"]["searchResult3"][kind]
            .as_array()
            .expect("a list")
            .iter()
            .map(|row| row["id"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[tokio::test]
    async fn another_members_songs_are_not_owned() {
        let world = World::new(None);
        let members = world.members();
        let navidrome = navidrome_with_songs(json!([
            song_row("shared-1", 1, "/music", "a.flac", 1),
            song_row("sam-1", 4, &format!("{members}/sam"), "b.flac", 1),
            song_row("alex-1", 3, &format!("{members}/alex"), "c.flac", 1),
        ]))
        .await;
        world.point_at(&navidrome.uri());
        world.index.refresh().await.expect("read");
        let service = service(ScopeResolver::new(
            world.settings.clone(),
            world.family.clone(),
            world.index.clone(),
        ));

        let owner = service.scoped_answer(Some("winters"), &answer()).expect("scoped");
        assert_eq!(ids(&owner, "song"), ["shared-1"]);
        assert_eq!(
            ids(&owner, "album"),
            ["al-shared"],
            "Sam's album goes with Sam's song"
        );
        assert_eq!(ids(&owner, "artist"), ["ar-shared"]);

        let alex = service.scoped_answer(Some("alex"), &answer()).expect("scoped");
        assert_eq!(ids(&alex, "song"), ["shared-1", "alex-1"]);
        let sam = service.scoped_answer(Some("sam"), &answer()).expect("scoped");
        assert_eq!(ids(&sam, "song"), ["shared-1", "sam-1"]);
        assert_eq!(ids(&sam, "album"), ["al-shared", "al-sam"]);
    }

    /// The lookup answer as a Navidrome would send it, with each song's artist and title.
    fn lookup_answer() -> String {
        json!({"subsonic-response": {"status": "ok", "searchResult3": {
            "artist": [{"id": "ar-shared", "name": "Portishead"}],
            "album": [{"id": "al-shared", "name": "Dummy", "artistId": "ar-shared"}],
            "song": [
                {"id": "shared-1", "albumId": "al-shared", "artistId": "ar-shared",
                 "artist": "Portishead", "title": "Glory Box"},
                {"id": "sam-1", "albumId": "al-shared", "artistId": "ar-shared",
                 "artist": "Portishead", "title": "Sam Only"}
            ]
        }}})
        .to_string()
    }

    #[tokio::test]
    async fn an_api_key_lookup_is_scoped_to_its_listener() {
        let world = World::new(None);
        let members = world.members();
        let family = navidrome_with_songs(json!([
            song_row("shared-1", 1, "/music", "a.flac", 1),
            song_row("sam-1", 4, &format!("{members}/sam"), "b.flac", 1),
        ]))
        .await;
        world.point_at(&family.uri());
        world.index.refresh().await.expect("read");
        let scope = ScopeResolver::new(world.settings.clone(), world.family.clone(), world.index.clone());

        let search = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/json")
                    .set_body_string(lookup_answer()),
            )
            .mount(&search)
            .await;
        let settings = Arc::new(SettingsStore::for_tests(octo_core::settings::AppSettings {
            subsonic: octo_core::settings::SubsonicSettings {
                url: Some(search.uri()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let service = SyncCatalogService::new(
            SubsonicProxyService::new(settings.clone()),
            Arc::new(FakeMetadata::default()),
            Arc::new(ExternalIdRegistry::new(None::<&str>)),
            settings,
            Clock::system(),
        );
        service.use_library_scope(scope);

        // An API-key app sends no `u`.
        let api_key: IndexMap<String, String> = [("apiKey", "key"), ("c", "Symfonium")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let owner = service
            .lookup_artist("winters", &api_key, "Portishead")
            .await
            .expect("an answer");
        assert!(owner.owns("Portishead", "Glory Box"));
        assert!(
            !owner.owns("Portishead", "Sam Only"),
            "another member's song is not the owner's"
        );
        let sam = service
            .lookup_artist("sam", &api_key, "Portishead")
            .await
            .expect("an answer");
        assert!(sam.owns("Portishead", "Sam Only"));
    }

    #[test]
    fn family_off_leaves_the_answer_alone() {
        let off = service(scope_for_tests(false, &[("alex", 3), ("sam", 4)]));
        assert!(off.scoped_answer(Some("winters"), &answer()).is_none());
        let on = service(scope_for_tests(true, &[("alex", 3), ("sam", 4)]));
        assert!(on.scoped_answer(None, &answer()).is_none(), "nobody named");
    }
}
