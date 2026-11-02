//! The external search: outside songs and albums for a query.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use octo_core::common::dotnet::{self, ordinal_ignore_case_key};
use octo_core::common::{SongIdentity, SupersedableBuildCoordinator, skipped_takes};
use octo_core::last_fm::SimilarTrack;
use octo_core::last_fm::last_fm_search_cleanup::LastFmSearchCleanup;
use octo_core::last_fm::last_fm_search_ranking::LastFmSearchRanking;
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{SettingsStore, SubsonicSettings};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::LastFmService;

/// Rows built per query, regardless of how many the caller wants.
///
/// It has to be a constant rather than the caller's target, or single-flight is
/// unsound: a client asking for 8 rows could win the race and hand 8 rows to a caller
/// that asked for 150. 60 is the number because that is where enrichment stops
/// (BackgroundEnrichLimit), so rows past it would ship as bare placeholders carrying a
/// fallback duration, and because the Navidrome-native search path already caps here.
pub const BUILD_SIZE: i32 = 60;

// track.search returns at most 50 rows, so padding whenever the list was short of
// BuildSize made the second, sequential Last.fm call on every search.
const THIN_SEARCH_THRESHOLD: usize = 20;

/// Deadline for one build. Last.fm has no configured HTTP timeout of its own, so
/// without this a single hung call would pin the query for every joined caller.
const BUILD_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest album ownership waits for a song build's row lookups before reading its own.
const ROWS_WAIT: Duration = Duration::from_secs(10);

/// Deadline for one album build. Deezer's own client timeout already bounds
/// the call inside it; this is the ceiling on the whole build.
const ALBUM_BUILD_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the external (discovery) half of a search, once per query.
///
/// Every caller for the same query joins one execution and receives the same list. That
/// is not an optimization, it is what keeps the search3 fix from re-creating issue #8.
/// Clients routinely fire several search calls for one typed query, and registry ids are
/// deterministic, so those calls resolve to the *same* SoulseekRouting objects and would
/// each run the enrichment pipeline over them concurrently: three writers to a shared
/// int? duration, and three times the Deezer fan-out against a budget that is already the
/// tightest thing in the system.
///
/// The returned list is FROZEN (an `Arc` nobody can write through). Callers slice it and
/// serialize it, concurrently, without copying. Both Subsonic serializers and the native one
/// only read, and the star/download paths rebuild a Song from the registry rather than from a
/// search result. Any future "top up the enrichment because this caller wanted more rows"
/// belongs inside the build, not after it.
pub struct ExternalSearchService {
    // Amperfy (and most Subsonic clients' type-ahead) fires one search3 call per
    // keystroke, uncancelled. Plain SingleFlight only collapses two callers asking for
    // the SAME query; it does nothing for the sequence "cage", "cage t", "cage the",
    // where each keystroke is a distinct key racing the same rate-limited lane as the
    // query the user actually meant. SupersedableBuildCoordinator adds prefix-based
    // cancellation on top of that collapsing.
    song_builds: SupersedableBuildCoordinator<(Arc<Vec<Song>>, SongSource)>,
    album_builds: SupersedableBuildCoordinator<Arc<Vec<Album>>>,
    metadata: Arc<dyn IMusicMetadataService>,
    last_fm: Option<Arc<LastFmService>>,
    /// Optional: how long a build waits for lengths, read at every build.
    settings: Option<Arc<SettingsStore>>,
    /// The last finished builds, by query.
    recent_songs: Arc<RecentResults<Song>>,
    recent_albums: RecentResults<Album>,
    /// Last.fm answers that came too late for their search, for the next one.
    last_fm_answers: Arc<RecentResults<SimilarTrack>>,
    row_signals: RowSignals,
}

impl ExternalSearchService {
    pub const BUILD_SIZE: i32 = BUILD_SIZE;

    pub fn new(
        metadata: Arc<dyn IMusicMetadataService>,
        last_fm: Option<Arc<LastFmService>>,
        settings: Option<Arc<SettingsStore>>,
    ) -> Self {
        ExternalSearchService {
            song_builds: SupersedableBuildCoordinator::new(),
            album_builds: SupersedableBuildCoordinator::new(),
            metadata,
            last_fm,
            settings,
            recent_songs: Arc::new(RecentResults::new(RECENT_RESULTS_TTL, RECENT_RESULTS_MAX)),
            recent_albums: RecentResults::new(RECENT_RESULTS_TTL, RECENT_RESULTS_MAX),
            last_fm_answers: Arc::new(RecentResults::new(RECENT_RESULTS_TTL, RECENT_RESULTS_MAX)),
            row_signals: RowSignals::default(),
        }
    }

    /// Up to [`BUILD_SIZE`] enriched external songs for this query. Callers take
    /// the prefix they need; the list is shared and must not be mutated. While Skip covers,
    /// karaoke and instrumentals is on, cover, karaoke and instrumental takes are left out; the build itself
    /// keeps them, so switching the setting changes the next answer at once.
    pub async fn get(&self, query: &str) -> Arc<Vec<Song>> {
        let key = recent_key(query);
        let songs = self.songs(query, &key).await;
        // However the songs were had, nothing more of this query's rows will be looked up.
        self.row_signals.done(&key);
        if self.skips_covers_and_instrumentals()
            && songs.iter().any(|song| skipped_takes::hides_song(song, true))
        {
            return Arc::new(skipped_takes::listed_songs(songs.as_ref().clone(), true));
        }
        songs
    }

    /// Skip covers and instrumentals, as it is set now.
    fn skips_covers_and_instrumentals(&self) -> bool {
        self.settings
            .as_ref()
            .is_some_and(|settings| settings.current().soulseek.skip_covers_and_instrumentals)
    }

    /// Resolves once the song build for `query` has looked its rows up on Deezer (at once
    /// when there is nothing to wait for, and after [`ROWS_WAIT`] at the latest). Taken before
    /// [`Self::get`] is called for the query, so the build cannot finish unseen.
    pub fn rows_looked_up(&self, query: &str) -> impl Future<Output = ()> + Send + 'static {
        let mut done = self.row_signals.subscribe(&recent_key(query));
        async move {
            let _ = tokio::time::timeout(ROWS_WAIT, done.wait_for(|done| *done)).await;
        }
    }

    async fn songs(&self, query: &str, key: &str) -> Arc<Vec<Song>> {
        if dotnet::is_blank(query) {
            return Arc::new(Vec::new());
        }
        // Only the key, not the radio switch. Discovery in the search bar is a different
        // feature from radio, and gating it on EnableRadio made turning radio off empty
        // the search results too.
        let Some(last_fm) = self.last_fm.clone().filter(|l| l.has_api_key()) else {
            return Arc::new(Vec::new());
        };

        let key = key.to_string();
        if let Some(cached) = self.recent_songs.get(&key, Instant::now()) {
            // A copy: the cached list is shared and frozen like any build's. Rows whose video
            // was found after the build answered carry its length now.
            let mut songs = cached.as_ref().clone();
            self.metadata.refresh_outside_songs(&mut songs);
            debug!(
                "external search '{query}' answered from the last {} rows",
                songs.len()
            );
            return Arc::new(songs);
        }

        let metadata = self.metadata.clone();
        let subsonic = self
            .settings
            .as_ref()
            .map_or_else(SubsonicSettings::default, |s| s.current().subsonic.clone());
        let wait = subsonic.effective_search_duration_wait();
        let last_fm_wait = subsonic.effective_search_last_fm_wait();
        let query_text = query.to_string();
        let shared = BuildShared {
            key: key.clone(),
            last_fm_answers: Arc::clone(&self.last_fm_answers),
            recent_songs: Arc::clone(&self.recent_songs),
        };
        let rows_done = RowsDone {
            signals: self.row_signals.clone(),
            key: key.clone(),
        };
        let (songs, source) = self
            .song_builds
            .run(
                query.trim(),
                move |token: CancellationToken| async move {
                    tokio::select! {
                        (built, source) = build(query_text, last_fm, metadata, wait, last_fm_wait, shared, rows_done) => Ok((Arc::new(built), source)),
                        () = token.cancelled() => Err(anyhow::anyhow!("The operation was canceled.")),
                    }
                },
                // The waits for Last.fm and for lengths are on top of the deadline, so long
                // ones never time the whole build out.
                BUILD_TIMEOUT + wait + last_fm_wait,
                (Arc::new(Vec::new()), SongSource::Nothing),
                // Discovery is an addition to search, never a precondition for it. The
                // steps inside the build are individually best-effort, but the deadline is
                // not, and a timeout must not take local results down with it.
                |q: &str, e: &anyhow::Error| debug!("external search '{q}' failed: {e}"),
            )
            .await;
        // A failed, timed-out or superseded build answers an empty list, which is not kept. A
        // build from Deezer's rows gives way to Last.fm's answer once that is in.
        if !songs.is_empty() {
            self.recent_songs.put(key.clone(), songs.clone(), Instant::now());
            if source == SongSource::Deezer && self.last_fm_answers.get(&key, Instant::now()).is_some() {
                self.recent_songs.remove(&key);
            }
        }
        songs
    }

    /// Up to `limit` external albums for this query, via the same prefix-supersession as
    /// [`ExternalSearchService::get`]. Needs no Last.fm key: Deezer's album catalog is keyless.
    /// While Skip covers and instrumentals is on, an album named as covers, karaoke or instrumentals is left out.
    pub async fn get_albums(&self, query: &str, limit: i32) -> Arc<Vec<Album>> {
        let albums = self.albums(query, limit).await;
        if self.skips_covers_and_instrumentals()
            && albums.iter().any(|album| skipped_takes::hides_album(album, true))
        {
            return Arc::new(skipped_takes::listed_albums(albums.as_ref().clone(), true));
        }
        albums
    }

    async fn albums(&self, query: &str, limit: i32) -> Arc<Vec<Album>> {
        if dotnet::is_blank(query) || limit <= 0 {
            return Arc::new(Vec::new());
        }
        let key = format!("{}|{limit}", recent_key(query));
        if let Some(cached) = self.recent_albums.get(&key, Instant::now()) {
            return cached;
        }
        let metadata = self.metadata.clone();
        let query_text = query.to_string();
        let albums = self
            .album_builds
            .run(
                query,
                move |token: CancellationToken| async move {
                    tokio::select! {
                        albums = metadata.search_album_rows(&query_text, limit) => Ok(Arc::new(albums)),
                        () = token.cancelled() => Err(anyhow::anyhow!("The operation was canceled.")),
                    }
                },
                ALBUM_BUILD_TIMEOUT,
                Arc::new(Vec::new()),
                |q: &str, e: &anyhow::Error| debug!("external album search '{q}' failed: {e}"),
            )
            .await;
        if !albums.is_empty() {
            self.recent_albums.put(key, albums.clone(), Instant::now());
        }
        albums
    }
}

/// How long a finished build answers the same query again.
pub const RECENT_RESULTS_TTL: Duration = Duration::from_secs(10 * 60);

/// How many queries' builds are kept; past it the oldest go first.
pub const RECENT_RESULTS_MAX: usize = 256;

/// The key a query's build is kept under: trimmed and lowercase, so "Adele " and "adele" share
/// one.
pub fn recent_key(query: &str) -> String {
    query.trim().to_lowercase()
}

/// The builds of the last queries, each kept for [`RECENT_RESULTS_TTL`], at most
/// [`RECENT_RESULTS_MAX`] of them. Typing the same words again, paging a result, or a client
/// asking twice then answers at once instead of asking Last.fm, Deezer and YouTube again.
pub struct RecentResults<T> {
    entries: Mutex<RecentEntries<T>>,
    ttl: Duration,
    max: usize,
}

struct RecentEntries<T> {
    by_key: HashMap<String, (Instant, Arc<Vec<T>>)>,
    /// Keys oldest first, each once.
    order: VecDeque<String>,
}

impl<T> RecentResults<T> {
    pub fn new(ttl: Duration, max: usize) -> Self {
        RecentResults {
            entries: Mutex::new(RecentEntries {
                by_key: HashMap::new(),
                order: VecDeque::new(),
            }),
            ttl,
            max,
        }
    }

    /// The build kept for `key`, unless it is older than the time to live at `now`.
    pub fn get(&self, key: &str, now: Instant) -> Option<Arc<Vec<T>>> {
        let mut entries = self.entries.lock();
        let (kept_at, value) = entries.by_key.get(key)?;
        if now.saturating_duration_since(*kept_at) < self.ttl {
            return Some(value.clone());
        }
        entries.by_key.remove(key);
        entries.order.retain(|k| k != key);
        None
    }

    /// Forgets what is kept for `key`.
    pub fn remove(&self, key: &str) {
        let mut entries = self.entries.lock();
        if entries.by_key.remove(key).is_some() {
            entries.order.retain(|k| k != key);
        }
    }

    /// Keeps `value` for `key` from `now`, replacing an older build of it; past the limit the
    /// oldest query goes.
    pub fn put(&self, key: String, value: Arc<Vec<T>>, now: Instant) {
        let mut entries = self.entries.lock();
        if entries.by_key.insert(key.clone(), (now, value)).is_some() {
            entries.order.retain(|k| *k != key);
        }
        entries.order.push_back(key);
        while entries.order.len() > self.max {
            if let Some(oldest) = entries.order.pop_front() {
                entries.by_key.remove(&oldest);
            }
        }
    }
}

/// Where a build's song rows came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongSource {
    /// Nothing was built (a failed or superseded build).
    Nothing,
    /// Last.fm's track.search, answered in time.
    LastFm,
    /// Last.fm's answer to an earlier search of the same words, which came too late for it.
    LastFmKept,
    /// Deezer's own track search: Last.fm did not answer in time.
    Deezer,
    /// Last.fm, waited for after all: it did not answer in time and Deezer had nothing.
    LastFmLate,
}

impl SongSource {
    fn describe(self) -> &'static str {
        match self {
            SongSource::Nothing => "nothing",
            SongSource::LastFm => "Last.fm",
            SongSource::LastFmKept => "Last.fm (kept from an earlier search)",
            SongSource::Deezer => "Deezer's track search (Last.fm did not answer in time)",
            SongSource::LastFmLate => "Last.fm (late; Deezer's track search had nothing)",
        }
    }
}

/// What a build shares with the service: Last.fm answers kept for the next search, the
/// finished builds, and who waits for its row lookups.
struct BuildShared {
    key: String,
    last_fm_answers: Arc<RecentResults<SimilarTrack>>,
    recent_songs: Arc<RecentResults<Song>>,
}

/// Fans out to Last.fm, then fills in the metadata a client needs to render and play
/// the rows. Order:
///   1. track.search hits for the query as typed, most listened first
///   2. canonical artist's top tracks (in case (1) was thin: common for
///      single-word artist queries)
///
/// When Last.fm has not answered within `last_fm_wait`, the rows are Deezer's own track search
/// for the query instead, in Deezer's order, and Last.fm's answer is kept for the next search
/// of the same words.
///
/// Deduped by `SongIdentity::match_key`, so the same track cannot appear twice however its
/// artist and title are written.
#[allow(clippy::too_many_arguments)]
async fn build(
    query: String,
    last_fm: Arc<LastFmService>,
    metadata: Arc<dyn IMusicMetadataService>,
    duration_wait: Duration,
    last_fm_wait: Duration,
    shared: BuildShared,
    rows_done: RowsDone,
) -> (Vec<Song>, SongSource) {
    let started = Instant::now();
    // While another catalog answers the catalog's searches, one song search for the query (the
    // one `song_rows` asks when Last.fm is late, so the two share it) fills the rows' album and
    // explicit word before each row looks itself up.
    let catalog_rows = metadata.catalog_searches_stand_in().then(|| {
        let (metadata, query) = (Arc::clone(&metadata), query.clone());
        tokio::spawn(async move { metadata.search_catalog_tracks(&query, BUILD_SIZE).await })
    });
    let (collected, source) = song_rows(&query, &last_fm, &metadata, last_fm_wait, &shared).await;
    let searched = started.elapsed();

    let mut songs = Vec::with_capacity(collected.len());
    for (artist, title) in &collected {
        if let Some(hit) = metadata
            .search_songs_by_artist_title(artist, title, 1, None)
            .await
            .into_iter()
            .next()
        {
            songs.push(hit);
        }
    }
    let placed = started.elapsed();
    info!("External search '{query}' -> {} placeholder songs", songs.len());

    // Album/art/year from Deezer (fast), then the ACCURATE duration for the top of the
    // list from the real YouTube video (so the scrub bar matches the audio and the
    // client advances correctly), waited for at most `duration_wait`. Rows not resolved by
    // then go out with Deezer's length and are resolved in the background. Bounded + cached.
    // The video lookups start now, so they run while Deezer answers; the pick still waits
    // for Deezer's length.
    metadata.prefetch_top_videos(&songs);
    if let Some(catalog_rows) = catalog_rows {
        let _ = catalog_rows.await;
    }
    metadata.enrich_search_rows(&mut songs).await;
    let enriched = started.elapsed();
    // Album ownership reads tracklists from the same Deezer budget; they start now.
    rows_done.mark();
    let before: Vec<Option<i32>> = songs.iter().map(|song| song.duration).collect();
    let durations = if duration_wait.is_zero() {
        None
    } else {
        Some(
            metadata
                .resolve_top_durations_within(&mut songs, duration_wait)
                .await,
        )
    };
    let lengthened = started.elapsed();
    info!(
        "External search '{query}' built in {} ms from {}: songs {} ms, rows {} ms, Deezer {} ms, YouTube lengths {} ms ({} rows changed{})",
        lengthened.as_millis(),
        source.describe(),
        searched.as_millis(),
        (placed - searched).as_millis(),
        (enriched - placed).as_millis(),
        (lengthened - enriched).as_millis(),
        songs
            .iter()
            .zip(&before)
            .filter(|(song, before)| song.duration != **before)
            .count(),
        match &durations {
            Some(Some(_)) => ", the rest in the background",
            Some(None) => "",
            None => ", all in the background",
        }
    );

    // Fire-and-forget: pre-resolve YouTube videoIds for the top hits so the first
    // /rest/stream click doesn't pay the cold yt-dlp double-call cost (ytsearch1: + -g,
    // 6-16s combined). Arpeggi cancels at ~10s and falls back to a local song; without
    // this, external playback is unreachable from that client. 12 is about what fits on
    // the first page of search results.
    //
    // It runs LAST on purpose. It used to run before enrichment, where it wrote a videoId chosen
    // with no duration hint while the top durations pass was choosing a different one using the
    // Deezer duration: so for the top rows the two raced and the loser could leave a song
    // advertising the length of a video that would not be the one played.
    //
    // The background work gets its own copy of the list: the songs this build returns stay
    // frozen, and in background mode the durations pass writes only the routing. The prewarm
    // waits for the rows the durations pass is still resolving.
    let background = songs.clone();
    let prewarm = metadata.clone();
    match durations {
        None => {
            tokio::spawn(resolve_durations_then_prewarm(prewarm, background));
        }
        Some(still_running) => {
            tokio::spawn(async move {
                if let Some(pass) = still_running {
                    let _ = pass.await;
                }
                prewarm.prewarm_you_tube_ids(&background, 12).await;
            });
        }
    }

    // Same reasoning, for cover art: a client renders the first screen of results a
    // moment after this returns, and without a prewarm each row's getCoverArt call
    // pays for the Deezer/iTunes/Last.fm chain itself, one row at a time. 24 covers
    // more than a screenful so scrolling a little still finds a warm cache.
    let covers = songs.clone();
    tokio::spawn(async move { metadata.prewarm_cover_art(&covers, 24).await });

    (songs, source)
}

/// The artist and title of each row, deduped, at most [`BUILD_SIZE`]: Last.fm's, or Deezer's
/// when Last.fm is late (see [`build`]).
async fn song_rows(
    query: &str,
    last_fm: &Arc<LastFmService>,
    metadata: &Arc<dyn IMusicMetadataService>,
    last_fm_wait: Duration,
    shared: &BuildShared,
) -> (Vec<(String, String)>, SongSource) {
    if let Some(kept) = shared.last_fm_answers.get(&shared.key, Instant::now()) {
        return (last_fm_rows(query, &kept, last_fm).await, SongSource::LastFmKept);
    }
    let asking = Arc::clone(last_fm);
    let asked = query.to_string();
    let mut answer = tokio::spawn(async move { asking.search_tracks(&asked, 50.min(BUILD_SIZE * 2)).await });
    let in_time = if last_fm_wait.is_zero() {
        Some((&mut answer).await)
    } else {
        tokio::time::timeout(last_fm_wait, &mut answer).await.ok()
    };
    if let Some(tracks) = in_time {
        return (
            last_fm_rows(query, &tracks.unwrap_or_default(), last_fm).await,
            SongSource::LastFm,
        );
    }

    match metadata.search_catalog_tracks(query, BUILD_SIZE).await {
        Some(rows) if !rows.is_empty() => {
            // Last.fm's answer, when it comes, is the next search's, and the build kept from
            // this one gives way to it.
            let key = shared.key.clone();
            let answers = Arc::clone(&shared.last_fm_answers);
            let recent = Arc::clone(&shared.recent_songs);
            tokio::spawn(async move {
                if let Ok(tracks) = answer.await
                    && !tracks.is_empty()
                {
                    answers.put(key.clone(), Arc::new(tracks), Instant::now());
                    recent.remove(&key);
                }
            });
            (deduped(rows.into_iter()), SongSource::Deezer)
        }
        _ => (
            last_fm_rows(query, &answer.await.unwrap_or_default(), last_fm).await,
            SongSource::LastFmLate,
        ),
    }
}

/// Rows from Last.fm's track.search answer: names put right, most listened first, padded with
/// the artist's top tracks when thin.
async fn last_fm_rows(
    query: &str,
    answer: &[SimilarTrack],
    last_fm: &LastFmService,
) -> Vec<(String, String)> {
    // Last.fm's search carries rows from mislabelled scrobbles; put their names right first,
    // or they reach the results, the player and each listener's Last.fm as they are. Then
    // most listened first: track.search orders by fuzzy relevance, which puts a side project's
    // song ahead of the hit everybody means. Repeated versions of a song are one row, and rows
    // in another script than the query go last (see LastFmSearchRanking).
    let tracks = LastFmSearchRanking::rank(query, LastFmSearchCleanup::clean(answer));
    let mut collected = deduped(tracks.iter().map(|t| (t.artist.clone(), t.title.clone())));

    if collected.len() < THIN_SEARCH_THRESHOLD {
        // Use the first track-search hit's artist as the canonical anchor
        // for top-tracks padding. Falls back to the raw query string when
        // track.search came back empty.
        let anchor = tracks.first().map_or(query, |t| t.artist.as_str());
        let top_tracks = last_fm.search_artist_top_tracks(anchor, BUILD_SIZE * 2).await;
        collected = deduped(
            collected
                .into_iter()
                .chain(top_tracks.iter().map(|t| (t.artist.clone(), t.title.clone()))),
        );
    }
    collected
}

/// Artist and title pairs, each song once by `SongIdentity::match_key`, at most [`BUILD_SIZE`].
fn deduped(rows: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    // HashSet<string>(OrdinalIgnoreCase) over the match keys.
    let mut seen = HashSet::new();
    let mut collected = Vec::new();
    for (artist, title) in rows {
        if collected.len() >= BUILD_SIZE as usize {
            break;
        }
        let key = SongIdentity::match_key(&artist, &title);
        if seen.insert(ordinal_ignore_case_key(&key)) {
            collected.push((artist, title));
        }
    }
    collected
}

/// Who waits for each query's row lookups: album ownership reads its tracklists from the same
/// Deezer budget, so it starts once the song build's rows are looked up.
#[derive(Clone, Default)]
pub struct RowSignals(Arc<Mutex<HashMap<String, watch::Sender<bool>>>>);

impl RowSignals {
    fn subscribe(&self, key: &str) -> watch::Receiver<bool> {
        self.0
            .lock()
            .entry(key.to_string())
            .or_insert_with(|| watch::channel(false).0)
            .subscribe()
    }

    fn done(&self, key: &str) {
        if let Some(sender) = self.0.lock().remove(key) {
            sender.send_replace(true);
        }
    }
}

/// Says a build's rows are looked up, when told or, at the latest, when the build ends.
struct RowsDone {
    signals: RowSignals,
    key: String,
}

impl RowsDone {
    fn mark(&self) {
        self.signals.done(&self.key);
    }
}

impl Drop for RowsDone {
    fn drop(&mut self) {
        self.mark();
    }
}

/// The durations pass search did not wait for, then the videoId prewarm, in that order
/// for the reason given in `build`. It runs in background mode, so the songs this
/// build returned stay frozen.
async fn resolve_durations_then_prewarm(metadata: Arc<dyn IMusicMetadataService>, mut songs: Vec<Song>) {
    metadata.resolve_top_durations(&mut songs, true).await;
    metadata.prewarm_you_tube_ids(&songs, 12).await;
}

#[cfg(test)]
#[path = "external_search_service_tests.rs"]
mod tests;
