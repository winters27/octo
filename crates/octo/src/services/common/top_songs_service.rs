//! Top songs and the charts: an artist's most played songs, for search, and every chart list
//! (Popular right now, Best New Songs, Trending Songs, the genre charts), for the chart
//! playlists, `getTopChart` and the apps' Charts page. Each comes back ranked, as outside songs
//! that play and can be added like any search row; which of them a caller's library already has
//! is settled per request by [`TopSongsService::match_library`], because the library changes and
//! the list does not.
//!
//! Sources, and why in this order:
//!   - The charts are Apple Music's (`AppleMusicPages`): current, per country, per genre, and
//!     each row carries its album, length, explicit badge and artwork, so a row costs no
//!     further lookup. Best New Songs and Trending Songs are the New page's song rooms.
//!   - When Apple's page cannot be read, Popular right now is Deezer's chart, then Last.fm's,
//!     for the server's own country only (neither knows another country's chart). The genre
//!     charts, the rooms and other countries' lists have no stand-in: they keep their last list.
//!   - An artist's ranking is Last.fm's play counts when a key is set, each row dressed by
//!     Deezer's own top list; without a key, Deezer's list is the ranking.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use octo_core::charts::apple_pages::{ChartGenre, RoomKind};
use octo_core::charts::chart_settings::{
    self, ChartCountry, ChartKind, OVERALL, english_menu, genre_name, is_latin_script, kind, max_songs,
    playlist_name, table_genre_name,
};
use octo_core::charts::top_songs::{
    APPLE_SOURCE, CatalogTrack, CountedTrack, DEEZER_SOURCE, MAX_ROWS, PlannedRow, library_hit,
    library_query, library_title_query, merge, one_row_per_library_song,
};
use octo_core::common::dotnet::is_blank;
use octo_core::common::{SongIdentity, skipped_takes};
use octo_core::models::domain::Song;
use octo_core::settings::SettingsStore;
use octo_core::soulseek::soulseek_metadata_service::RoutingKind;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::services::charts::apple_music_pages::{AppleMusicPages, LaneCell, PageError};
use crate::services::charts::{ChartCountryStore, GenreNames};
use crate::services::common::lastfm_country_names::LASTFM_COUNTRY_NAMES;
use crate::services::cover_art::ArtworkHints;
use crate::services::framework::{InFlight, MemoryCache};
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::last_fm_service::LastFmService;
use crate::services::library::MatchSession;
use crate::services::library::for_you_lists;
use crate::services::metadata::deezer_metadata_service::{CHART_LIMIT, DeezerMetadataService};
use crate::services::soulseek::external_id_registry::ExternalIdRegistry;

/// A whole list keeps for this long once every source answered; one built while a source did
/// not answer, or that came out empty, is asked again soon.
const ARTIST_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const CHART_TTL: Duration = Duration::from_secs(60 * 60);
const SHORT_TTL: Duration = Duration::from_secs(10 * 60);
/// The country's genre menu changes rarely.
const MENU_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Ceiling on one build, shared by everyone waiting on it. A chart's page waits its turn at
/// Apple's gate behind the others, so it gets longer than an artist's list.
const ARTIST_BUILD_TIMEOUT: Duration = Duration::from_secs(15);
const CHART_BUILD_TIMEOUT: Duration = Duration::from_secs(45);

/// How many catalog artists of a name are weighed, as an outside artist page does.
const ARTIST_CANDIDATES: i32 = 5;

/// Library searches running at once for one list. Navidrome is local and quick.
pub const LIBRARY_SEARCHES_AT_ONCE: usize = 4;

/// How often a failing Apple list is warned about.
const WARN_EVERY: Duration = Duration::from_secs(60 * 60);

/// How many other countries one listener may have loading at once, besides their own, their
/// pinned ones and the server's. A country counts from when its list starts loading until that
/// load ends.
const OTHER_WORK_AT_ONCE: usize = 2;

/// One ranked song. Plays and listeners are Last.fm's, when it gave them.
#[derive(Debug, Clone, Default)]
pub struct TopSong {
    pub rank: usize,
    pub song: Song,
    pub plays: Option<i64>,
    pub listeners: Option<i64>,
}

/// A ranked list and where its order came from.
#[derive(Debug, Clone, Default)]
pub struct TopList {
    /// The artist, on an artist's list; None on a chart.
    pub artist: Option<String>,
    /// `apple`, `deezer`, `lastfm`, or empty when nothing answered.
    pub source: String,
    /// The chart's id (`34`, `new`, `trending`, a genre id), on a chart.
    pub chart: Option<String>,
    /// The chart's playlist name, on a chart.
    pub name: Option<String>,
    /// The country the chart is for, when known.
    pub country: Option<String>,
    /// This week's room, on Best New Songs and Trending Songs.
    pub room_id: Option<String>,
    pub songs: Vec<TopSong>,
}

/// How a chart's last fetch went, for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartFetch {
    pub chart: String,
    /// The country Apple's page was for.
    pub country: Option<String>,
    /// The country it was read for, None for Apple's own pick.
    pub asked: Option<String>,
    pub source: String,
    pub songs: usize,
    pub fetched_utc: DateTime<Utc>,
    pub error: Option<String>,
    pub room_id: Option<String>,
}

/// Cheap to clone: the clones share one cache.
#[derive(Clone)]
pub struct TopSongsService {
    inner: Arc<Inner>,
}

struct Inner {
    deezer: DeezerMetadataService,
    last_fm: Option<Arc<LastFmService>>,
    metadata: Arc<dyn IMusicMetadataService>,
    registry: Arc<ExternalIdRegistry>,
    apple: Arc<AppleMusicPages>,
    hints: Arc<ArtworkHints>,
    settings: Arc<SettingsStore>,
    cache: MemoryCache<Arc<TopList>>,
    /// Charts read for a country that is not the server's, kept apart so that browsing many
    /// countries never pushes out the server's charts or the artists' lists.
    others: MemoryCache<Arc<TopList>>,
    menus: MemoryCache<Arc<Vec<ChartGenre>>>,
    /// The countries (`auto` for the server's own) whose [`TopSongsService::learn_in`] started
    /// by a request is going.
    learning: Mutex<HashSet<String>>,
    /// English names of genres the tables lack, learned from Apple.
    genre_names: Arc<GenreNames>,
    /// The genre ids whose English name is being read.
    naming: Mutex<HashSet<String>>,
    builds: InFlight<(Arc<TopList>, bool)>,
    /// The lane each running build reads in, by build key, so a request that joins a background
    /// build can turn it urgent.
    urgent: Mutex<HashMap<String, LaneCell>>,
    fetches: Mutex<HashMap<String, ChartFetch>>,
    warned: Mutex<HashMap<String, Instant>>,
    /// Each listener's own and pinned chart countries.
    listeners: Arc<ChartCountryStore>,
    /// Per listener, how many other countries are loading for them now.
    others_working: Arc<Mutex<HashMap<String, usize>>>,
    /// Last.fm's name for each country it has a chart of, by code.
    geo_names: &'static [(&'static str, &'static str)],
}

/// One other country's load, counted against its listener until it is dropped.
pub struct OtherGuard {
    working: Arc<Mutex<HashMap<String, usize>>>,
    who: String,
}

impl Drop for OtherGuard {
    fn drop(&mut self) {
        let mut working = self.working.lock();
        if let Some(count) = working.get_mut(&self.who) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                working.remove(&self.who);
            }
        }
    }
}

/// What a listener's request for another country's list costs them. It moves into the work it
/// starts and lasts as long as that work does.
pub enum OtherWork {
    /// Nothing: their own, pinned or the server's country, a list already held, or a load
    /// already running that this request only joins.
    Free,
    /// One of their [`OTHER_WORK_AT_ONCE`] other countries loading.
    Counted(OtherGuard),
}

/// What a chart build came back with: the list, whether every source answered, and why not.
struct Built {
    list: TopList,
    whole: bool,
    error: Option<String>,
}

impl TopSongsService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        deezer: DeezerMetadataService,
        last_fm: Option<Arc<LastFmService>>,
        metadata: Arc<dyn IMusicMetadataService>,
        registry: Arc<ExternalIdRegistry>,
        apple: Arc<AppleMusicPages>,
        hints: Arc<ArtworkHints>,
        settings: Arc<SettingsStore>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                deezer,
                last_fm,
                metadata,
                registry,
                apple,
                hints,
                settings,
                cache: MemoryCache::new(512),
                others: MemoryCache::new(256),
                menus: MemoryCache::new(chart_settings::APPLE_COUNTRIES.len() as u64 + 16),
                learning: Mutex::new(HashSet::new()),
                genre_names: Arc::new(GenreNames::new(None)),
                naming: Mutex::new(HashSet::new()),
                builds: InFlight::new(),
                urgent: Mutex::new(HashMap::new()),
                fetches: Mutex::new(HashMap::new()),
                warned: Mutex::new(HashMap::new()),
                listeners: Arc::new(ChartCountryStore::new(None)),
                others_working: Arc::new(Mutex::new(HashMap::new())),
                geo_names: LASTFM_COUNTRY_NAMES,
            }),
        }
    }

    /// The service with its own table of Last.fm's country names. Call it before the service is
    /// cloned.
    #[cfg(test)]
    pub(crate) fn with_geo_names(mut self, table: &'static [(&'static str, &'static str)]) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.geo_names = table;
        }
        self
    }

    /// The service with each listener's chart countries. Call it before the service is cloned.
    pub fn with_listeners(mut self, store: Arc<ChartCountryStore>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.listeners = store;
        }
        self
    }

    /// The service keeping the genre names it learns in `store`. Call it before the service is
    /// cloned.
    pub fn with_genre_names(mut self, store: Arc<GenreNames>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.genre_names = store;
        }
        self
    }

    /// Each listener's own and pinned chart countries.
    pub fn listeners(&self) -> &Arc<ChartCountryStore> {
        &self.inner.listeners
    }

    /// The pages this reads Apple Music through, for the dashboard's country list.
    pub fn apple(&self) -> &Arc<AppleMusicPages> {
        &self.inner.apple
    }

    fn has_last_fm_key(&self) -> bool {
        self.inner.last_fm.as_ref().is_some_and(|l| l.has_api_key())
    }

    // ---- artists ---------------------------------------------------------------------------

    /// An artist's most played songs. `artist_id` may be an outside artist's id from search,
    /// which already names the catalog artist; a library id, or none, finds the artist by name.
    pub async fn for_artist(&self, name: &str, artist_id: Option<&str>) -> Arc<TopList> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Arc::default();
        }
        let known = artist_id
            .filter(|id| !is_blank(id))
            .and_then(|id| self.inner.registry.lookup(id))
            .map(|routing| routing.snapshot())
            .filter(|r| {
                r.kind == RoutingKind::Artist
                    && SongIdentity::same_artist_name(r.artist.as_deref().unwrap_or(""), &name)
            })
            .and_then(|r| r.external_artist_id)
            .filter(|id| !id.is_empty());
        let key = match &known {
            Some(id) => format!("artist|id|{id}"),
            None => format!("artist|name|{}", SongIdentity::key(&name)),
        };
        let this = self.clone();
        self.cached(
            Kept::Server,
            key,
            ARTIST_TTL,
            ARTIST_BUILD_TIMEOUT,
            &LaneCell::behind(),
            OtherWork::Free,
            async move {
                let (list, whole) = this.build_artist(&name, known).await;
                (list, whole)
            },
        )
        .await
    }

    async fn build_artist(&self, name: &str, known: Option<String>) -> (TopList, bool) {
        let last_fm = self.inner.last_fm.clone().filter(|l| l.has_api_key());
        let name_owned = name.to_string();
        let counts_task = async move {
            match last_fm {
                Some(l) => l.get_artist_chart(&name_owned, MAX_ROWS as i32).await,
                None => None,
            }
        };
        let catalog_task = async {
            let id = match known {
                Some(id) => Some(id),
                None => self.find_catalog_artist(name).await,
            };
            match id {
                None => Some(Arc::new(Vec::new())),
                Some(id) => self.inner.deezer.get_artist_top(&id, CHART_LIMIT).await,
            }
        };
        let (catalog, counts) = tokio::join!(catalog_task, counts_task);
        let (rows, source) = merge(
            catalog.as_deref().map(Vec::as_slice),
            counts.as_deref(),
            true,
            MAX_ROWS,
            DEEZER_SOURCE,
        );
        // Whole: every source that could answer did. A catalog miss on the name is an answer.
        let whole = catalog.is_some() && (counts.is_some() || !self.has_last_fm_key());
        let shown = rows
            .iter()
            .find_map(|row| row.catalog.as_ref().map(|c| c.artist.clone()))
            .unwrap_or_else(|| name.to_string());
        let songs = self.build_songs(&rows).await;
        info!("Top songs for '{name}': {} from {source}", songs.len());
        let artist = if SongIdentity::same_artist_name(&shown, name) {
            shown
        } else {
            name.to_string()
        };
        (
            TopList {
                artist: Some(artist),
                source: source.to_string(),
                songs,
                ..Default::default()
            },
            whole,
        )
    }

    /// The catalog artist of this exact name that more people follow, as an outside artist's
    /// page settles it when it holds no id.
    async fn find_catalog_artist(&self, name: &str) -> Option<String> {
        let hits = self.inner.deezer.search_artists(name, ARTIST_CANDIDATES).await;
        hits.into_iter()
            .filter(|hit| SongIdentity::same_artist_name(&hit.name, name))
            .max_by_key(|hit| hit.fans)
            .map(|hit| hit.deezer_id)
    }

    // ---- charts ----------------------------------------------------------------------------

    /// The country the charts are read for: the setting, or the one Apple was last seen to put
    /// the server in. Never waits on Apple: while Automatic has not learned the country yet this
    /// is None (Apple's own redirect picks it) and the learning starts behind the answer.
    pub async fn country(&self) -> Option<String> {
        self.server_country()
    }

    /// The country Apple's chart pages were last seen to put the server in, without asking.
    pub fn known_country(&self) -> Option<String> {
        self.inner.apple.known_country()
    }

    fn server_country(&self) -> Option<String> {
        match self.inner.settings.current().generated_playlists.chart_country() {
            ChartCountry::Fixed(code) => Some(code),
            ChartCountry::Auto => {
                let known = self.inner.apple.known_country();
                if known.is_none() {
                    self.learn_behind(None);
                }
                known
            }
        }
    }

    /// A listener's chart country: their own choice while Apple Music has it, else the server's
    /// (see [`Self::country`]).
    pub async fn country_of(&self, user: &str) -> Option<String> {
        self.home_of(user)
    }

    /// [`Self::country_of`], for code that cannot wait.
    pub fn home_of(&self, user: &str) -> Option<String> {
        let own = self.inner.listeners.get(user).country;
        if own.is_empty() || !self.has_country(&own) {
            self.server_country()
        } else {
            Some(own)
        }
    }

    /// Whether Apple Music has this country (its country switcher's list).
    pub fn has_country(&self, code: &str) -> bool {
        self.inner.apple.has_country(code)
    }

    /// The key of the load of a country's chart, for [`Self::begin_other_work`].
    pub fn chart_work(country: &str, chart: &str) -> String {
        chart_key(Some(country), chart)
    }

    /// The key of the load of a country's genre menu, for [`Self::begin_other_work`].
    pub fn menu_work(country: &str) -> String {
        format!("menu|{country}")
    }

    /// Starts counting a load of `country`'s list (`key`, from [`Self::chart_work`] or
    /// [`Self::menu_work`]) for `who`. Their own country, their pinned ones, the server's, a list
    /// already held and a load already running are free. Any other load counts until the work
    /// it moves into ends, and at most [`OTHER_WORK_AT_ONCE`] count at once, so no one listener
    /// fills Apple's queue. None when `who` has that many loading already.
    pub fn begin_other_work(&self, who: &str, country: &str, key: &str) -> Option<OtherWork> {
        let who = who.trim().to_lowercase();
        let country = country.trim().to_ascii_lowercase();
        let mine = self.inner.listeners.get(&who);
        if mine.country == country
            || mine.pinned.contains(&country)
            || self.server_country().as_deref() == Some(country.as_str())
            || self.held_or_running(key, &country)
        {
            return Some(OtherWork::Free);
        }
        let mut working = self.inner.others_working.lock();
        let count = working.get(&who).copied().unwrap_or(0);
        if count >= OTHER_WORK_AT_ONCE {
            drop(working);
            info!("Other countries still loading for {who}; refused {country}");
            return None;
        }
        working.insert(who.clone(), count + 1);
        Some(OtherWork::Counted(OtherGuard {
            working: Arc::clone(&self.inner.others_working),
            who,
        }))
    }

    /// Whether the list `key` is held, or its load is running.
    fn held_or_running(&self, key: &str, country: &str) -> bool {
        if key.starts_with("menu|") {
            return self.inner.menus.get(country).is_some() || self.inner.learning.lock().contains(country);
        }
        self.inner.cache.get(key).is_some()
            || self.inner.others.get(key).is_some()
            || self.running(key)
            || self.running(&format!("other|{key}"))
    }

    /// Whether a build runs under this build key.
    fn running(&self, building: &str) -> bool {
        self.inner.urgent.lock().contains_key(building)
    }

    /// The server's genre menu (see [`Self::menu_in`]).
    pub async fn menu(&self) -> (Option<String>, Arc<Vec<ChartGenre>>) {
        self.menu_in(self.country().await).await
    }

    /// A country's genre menu, "All Genres" first, as Apple lists it; Octo's English list
    /// while Apple's is not known yet (learned behind the answer, never waited on). None is the
    /// country Apple's own redirect picks.
    pub async fn menu_in(&self, country: Option<String>) -> (Option<String>, Arc<Vec<ChartGenre>>) {
        self.menu_in_counted(country, OtherWork::Free).await
    }

    /// [`Self::menu_in`], the learning it starts counted as `work` until it ends. Dropped at once
    /// when nothing is started.
    pub async fn menu_in_counted(
        &self,
        country: Option<String>,
        work: OtherWork,
    ) -> (Option<String>, Arc<Vec<ChartGenre>>) {
        let Some(code) = country.clone() else {
            return (None, Arc::new(english_menu()));
        };
        if let Some(menu) = self.inner.menus.get(&code) {
            return (country, Arc::new(self.named(&menu)));
        }
        self.learn_behind_counted(Some(code), work);
        (country, Arc::new(english_menu()))
    }

    /// A menu with every genre in English: a table's name, else a learned one, else Apple's own
    /// when it is in Latin script. A genre with no such name is left out until one is known.
    fn named(&self, menu: &[ChartGenre]) -> Vec<ChartGenre> {
        menu.iter()
            .filter_map(|g| {
                let name = if table_genre_name(&g.id).is_some() {
                    g.name.clone()
                } else if let Some(learned) = self.inner.genre_names.get(&g.id) {
                    learned
                } else if is_latin_script(&g.name) {
                    g.name.clone()
                } else {
                    return None;
                };
                Some(ChartGenre {
                    id: g.id.clone(),
                    name,
                })
            })
            .collect()
    }

    /// Starts reading the English name of each genre of `menu` that has none, in the background.
    fn name_missing(&self, menu: &[ChartGenre]) {
        let missing = self.unnamed(menu);
        if missing.is_empty() {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move { this.learn_names(missing).await });
    }

    /// The ids of `menu` that have no English name yet.
    fn unnamed(&self, menu: &[ChartGenre]) -> Vec<String> {
        menu.iter()
            .filter(|g| {
                table_genre_name(&g.id).is_none()
                    && self.inner.genre_names.get(&g.id).is_none()
                    && !is_latin_script(&g.name)
            })
            .map(|g| g.id.clone())
            .collect()
    }

    /// Reads and saves the English name of each genre, at most one read per id at a time. An id
    /// Apple does not answer for is asked again the next time its country's menu is learned.
    async fn learn_names(&self, ids: Vec<String>) {
        for id in ids {
            if !self.inner.naming.lock().insert(id.clone()) {
                continue;
            }
            let found = self.inner.apple.genre_english_name(&id).await;
            self.inner.naming.lock().remove(&id);
            if let Some(name) = found {
                self.inner.genre_names.set(&id, &name);
                info!("Learned the English name of Apple genre {id}: {name}");
            }
        }
    }

    /// [`Self::learn_in`] for the server's own country.
    pub async fn learn(&self) -> (Option<String>, Arc<Vec<ChartGenre>>) {
        self.learn_in(None).await
    }

    /// Learns a country's genre menu from Apple, waiting for it: for the warm worker and tests,
    /// never a request. None is the server's own country, itself learned first when Automatic.
    /// What cannot be learned stays unknown, so the English menu and Apple's redirect go on
    /// serving.
    pub async fn learn_in(&self, country: Option<String>) -> (Option<String>, Arc<Vec<ChartGenre>>) {
        self.learn_on(country, &LaneCell::behind()).await
    }

    /// [`Self::learn_in`], its Apple reads made in `lane`.
    pub async fn learn_on(
        &self,
        country: Option<String>,
        lane: &LaneCell,
    ) -> (Option<String>, Arc<Vec<ChartGenre>>) {
        let country = match country {
            Some(code) => Some(code),
            None => match self.inner.settings.current().generated_playlists.chart_country() {
                ChartCountry::Fixed(code) => Some(code),
                ChartCountry::Auto => self.inner.apple.detected_country_on(lane).await,
            },
        };
        let Some(code) = country.clone() else {
            return (None, Arc::new(english_menu()));
        };
        if let Some(menu) = self.inner.menus.get(&code) {
            self.name_missing(&menu);
            return (country, Arc::new(self.named(&menu)));
        }
        match self.inner.apple.chart_on(Some(&code), OVERALL, lane).await {
            Ok(page) if !page.genres.is_empty() => {
                let menu = Arc::new(page.genres.clone());
                self.inner.menus.set(code, menu.clone(), 1, MENU_TTL);
                self.name_missing(&menu);
                (country, Arc::new(self.named(&menu)))
            }
            _ => (country, Arc::new(english_menu())),
        }
    }

    /// [`Self::learn_in`] in the background, one at a time per country; Apple's own memo of what
    /// failed keeps a server that cannot reach it from asking again for a while.
    fn learn_behind(&self, country: Option<String>) {
        self.learn_behind_counted(country, OtherWork::Free);
    }

    /// [`Self::learn_behind`], with `work` held until the learning ends. Dropped at once when a
    /// learning of that country is already going.
    fn learn_behind_counted(&self, country: Option<String>, work: OtherWork) {
        let key = country.clone().unwrap_or_else(|| "auto".into());
        if !self.inner.learning.lock().insert(key.clone()) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let _work = work;
            // Lets the country go even when the learning stops part way, so it can start again.
            let _done = Learned(this.clone(), key);
            this.learn_in(country).await;
        });
    }

    /// A chart's name as a playlist and a chip: `Popular right now`, `Best New Songs`,
    /// `Trending Songs` or `Top {genre}`, with the genre's own name for the chip.
    pub fn names(menu: &[ChartGenre], id: &str) -> Option<(String, String)> {
        let label = match kind(id)? {
            ChartKind::Overall => "Top songs".to_string(),
            ChartKind::NewSongs => chart_settings::NEW_SONGS_NAME.to_string(),
            ChartKind::Trending => chart_settings::TRENDING_NAME.to_string(),
            ChartKind::Genre => genre_name(menu, id)?,
        };
        Some((playlist_name(id, &label), label))
    }

    /// The list a chart already holds for the server's country, without building it.
    pub async fn peek_chart(&self, id: &str) -> Option<Arc<TopList>> {
        self.peek_chart_in(self.country().await, id).await
    }

    /// The list a chart already holds for a country, without building it.
    pub async fn peek_chart_in(&self, country: Option<String>, id: &str) -> Option<Arc<TopList>> {
        let key = chart_key(country.as_deref(), id);
        self.inner.cache.get(&key).or_else(|| self.inner.others.get(&key))
    }

    /// A chart for the server's country (see [`Self::chart_in`]).
    pub async fn chart(&self, id: &str) -> Arc<TopList> {
        self.chart_in(self.country().await, id).await
    }

    /// [`Self::chart_on`] in the background lane.
    pub async fn chart_in(&self, country: Option<String>, id: &str) -> Arc<TopList> {
        self.chart_on(country, id, &LaneCell::behind()).await
    }

    /// A chart for a country: `34` (Popular right now), `new`, `trending` or a genre id. Empty
    /// for an id that is no chart, or a genre the country does not have, without asking anyone.
    /// Every listener of one country shares one list. Its Apple reads go in `lane`; a request
    /// that joins a build already running in the background turns that build urgent.
    pub async fn chart_on(&self, country: Option<String>, id: &str, lane: &LaneCell) -> Arc<TopList> {
        self.chart_in_counted(country, id, lane, OtherWork::Free).await
    }

    /// [`Self::chart_on`], the build it starts counted as `work` until it ends or times out,
    /// whether or not the request that asked is still waiting. Dropped at once when nothing is
    /// started.
    pub async fn chart_in_counted(
        &self,
        country: Option<String>,
        id: &str,
        lane: &LaneCell,
        work: OtherWork,
    ) -> Arc<TopList> {
        let Some(chart_kind) = kind(id) else {
            return Arc::default();
        };
        // The stand-ins for Popular right now know only the server's own region.
        let backup = country == self.country().await;
        let (country, menu) = self.menu_in(country).await;
        if chart_kind == ChartKind::Genre && !menu.iter().any(|g| g.id == id) {
            return Arc::default();
        }
        let Some((name, _)) = Self::names(&menu, id) else {
            return Arc::default();
        };
        let key = chart_key(country.as_deref(), id);
        let this = self.clone();
        let id_owned = id.to_string();
        let build_lane = lane.clone();
        // Another country's list is kept apart from the server's, so a failed read of it never
        // stands in for the server's own (which has stand-ins) under the same key.
        let place = if backup { Kept::Server } else { Kept::Other };
        self.cached(
            place,
            key,
            CHART_TTL,
            CHART_BUILD_TIMEOUT,
            lane,
            work,
            async move {
                let started = Instant::now();
                let built = this
                    .build_chart(&id_owned, chart_kind, country.clone(), backup, &build_lane)
                    .await;
                let Built {
                    mut list,
                    whole,
                    error,
                } = built;
                list.chart = Some(id_owned.clone());
                list.name = Some(name);
                list.country = list.country.or(country.clone());
                this.record(&id_owned, country.as_deref(), &list, error.as_deref());
                info!(
                    "Chart {id_owned} for {}: {} songs from {} in {} ms",
                    list.country.as_deref().unwrap_or("the server's country"),
                    list.songs.len(),
                    if list.source.is_empty() {
                        "nowhere"
                    } else {
                        list.source.as_str()
                    },
                    started.elapsed().as_millis()
                );
                (list, whole)
            },
        )
        .await
    }

    async fn build_chart(
        &self,
        id: &str,
        chart_kind: ChartKind,
        country: Option<String>,
        backup: bool,
        lane: &LaneCell,
    ) -> Built {
        let max = max_songs(id);
        let apple = match chart_kind {
            ChartKind::Overall | ChartKind::Genre => self
                .inner
                .apple
                .chart_on(country.as_deref(), id, lane)
                .await
                .map(|page| (None, page)),
            ChartKind::NewSongs | ChartKind::Trending => match country.as_deref() {
                None => Err(PageError::Unavailable("the country is not known".into())),
                Some(code) => {
                    let room = if chart_kind == ChartKind::NewSongs {
                        RoomKind::BestNewSongs
                    } else {
                        RoomKind::TrendingSongs
                    };
                    self.inner
                        .apple
                        .room_on(code, room, lane)
                        .await
                        .map(|(room_id, page)| (Some(room_id), page))
                }
            },
        };
        match apple {
            Ok((room_id, page)) => {
                if chart_kind == ChartKind::Overall
                    && !page.genres.is_empty()
                    && octo_core::charts::apple_pages::is_country_code(&page.country)
                {
                    self.inner
                        .menus
                        .set(page.country.clone(), Arc::new(page.genres.clone()), 1, MENU_TTL);
                    self.name_missing(&page.genres);
                }
                let catalog: Vec<CatalogTrack> = page.songs.iter().map(CatalogTrack::from_apple).collect();
                let (rows, source) = merge(Some(catalog.as_slice()), None, false, max, APPLE_SOURCE);
                let songs = self.build_songs(&rows).await;
                let whole = !songs.is_empty();
                Built {
                    list: TopList {
                        source: source.to_string(),
                        country: Some(page.country.clone()).filter(|c| !c.is_empty()),
                        room_id,
                        songs,
                        ..Default::default()
                    },
                    whole,
                    error: None,
                }
            }
            Err(error) => {
                let why = error.describe();
                self.warn_once(id, country.as_deref(), &why);
                if chart_kind != ChartKind::Overall {
                    return Built {
                        list: TopList::default(),
                        whole: false,
                        error: Some(why),
                    };
                }
                self.backup_overall(country.as_deref(), backup, why).await
            }
        }
    }

    /// Popular right now when Apple's chart cannot be read, for `country` (None is wherever
    /// Apple puts the server). `server_own`: it is the server's own country. In order:
    /// - Deezer's chart, which follows the server's address, so only for the country Apple puts
    ///   the server in (or the server's own while that is not learned yet);
    /// - Last.fm's chart of that country, when Last.fm has one;
    /// - Last.fm's global chart, for the server's own country only.
    async fn backup_overall(&self, country: Option<&str>, server_own: bool, apple_error: String) -> Built {
        let max = max_songs(OVERALL);
        let by_address = match self.inner.apple.known_country() {
            Some(known) => country.is_none_or(|c| c == known),
            None => server_own,
        };
        if by_address && let Some(catalog) = self.inner.deezer.get_chart(CHART_LIMIT).await {
            // Apple's catalog answers the chart on a server Deezer does not serve; its rows carry
            // Apple's ids.
            let named = if catalog
                .first()
                .is_some_and(|row| octo_core::metadata::apple_as_deezer::is_apple_id(&row.catalog_id))
            {
                octo_core::charts::top_songs::APPLE_SOURCE
            } else {
                DEEZER_SOURCE
            };
            let (rows, source) = merge(Some(catalog.as_slice()), None, false, max, named);
            let songs = self.build_songs(&rows).await;
            return Built {
                list: TopList {
                    source: source.to_string(),
                    songs,
                    ..Default::default()
                },
                whole: true,
                error: Some(apple_error),
            };
        }
        let last_fm = self.inner.last_fm.as_ref().filter(|l| l.has_api_key());
        if let (Some(last_fm), Some(code)) = (last_fm, country)
            && let Some(name) = self
                .inner
                .geo_names
                .iter()
                .find(|(listed, _)| listed.eq_ignore_ascii_case(code))
                .map(|(_, name)| *name)
            && let Some(counts) = last_fm.get_country_chart(name, max as i32).await
        {
            let (rows, source) = merge(None, Some(counts.as_slice()), false, max, DEEZER_SOURCE);
            let songs = self.build_songs(&rows).await;
            if !songs.is_empty() {
                return Built {
                    list: TopList {
                        source: source.to_string(),
                        songs,
                        ..Default::default()
                    },
                    whole: false,
                    error: Some(apple_error),
                };
            }
        }
        if !server_own {
            return Built {
                list: TopList::default(),
                whole: false,
                error: Some(apple_error),
            };
        }
        let counts: Option<Vec<CountedTrack>> = match last_fm {
            Some(l) => l.get_chart(max as i32).await,
            None => None,
        };
        let (rows, source) = merge(None, counts.as_deref(), false, max, DEEZER_SOURCE);
        let songs = self.build_songs(&rows).await;
        Built {
            list: TopList {
                source: source.to_string(),
                songs,
                ..Default::default()
            },
            whole: false,
            error: Some(apple_error),
        }
    }

    /// Every chart's last fetch, for the dashboard.
    pub fn fetches(&self) -> Vec<ChartFetch> {
        self.inner.fetches.lock().values().cloned().collect()
    }

    fn record(&self, id: &str, asked: Option<&str>, list: &TopList, error: Option<&str>) {
        let fetch = ChartFetch {
            chart: id.to_string(),
            country: list.country.clone(),
            asked: asked.map(str::to_string),
            source: list.source.clone(),
            songs: list.songs.len(),
            fetched_utc: Utc::now(),
            error: error.map(str::to_string),
            room_id: list.room_id.clone(),
        };
        self.inner.fetches.lock().insert(chart_key(asked, id), fetch);
    }

    fn warn_once(&self, id: &str, country: Option<&str>, why: &str) {
        let now = Instant::now();
        let key = chart_key(country, id);
        let mut warned = self.inner.warned.lock();
        let due = warned
            .get(&key)
            .is_none_or(|at| now.duration_since(*at) >= WARN_EVERY);
        let chart = match country {
            Some(code) => format!("{id} for {code}"),
            None => id.to_string(),
        };
        if due {
            warned.insert(key, now);
            warn!("Chart {chart}: {why}");
        } else {
            debug!("Chart {chart}: {why}");
        }
    }

    // ---- shared ----------------------------------------------------------------------------

    async fn cached<F>(
        &self,
        place: Kept,
        key: String,
        ttl: Duration,
        timeout: Duration,
        lane: &LaneCell,
        work: OtherWork,
        build: F,
    ) -> Arc<TopList>
    where
        F: Future<Output = (TopList, bool)> + Send + 'static,
    {
        if let Some(cached) = self.kept(place).get(&key) {
            return cached;
        }
        let this = self.clone();
        let cache_key = key.clone();
        let building = match place {
            Kept::Server => key.clone(),
            Kept::Other => format!("other|{key}"),
        };
        let (service, build_key) = (self.clone(), building.clone());
        // The lane is recorded under the same lock that starts the build, so a build that ends
        // at once cannot leave its lane behind.
        let (running, started) = {
            let mut urgent = self.inner.urgent.lock();
            let (running, joined) = self.inner.builds.run_joined(&building, async move {
                let _forget = Forget(service, build_key);
                // Counted until the build ends or times out, not until the request goes away.
                let _work = work;
                let (list, whole) = match tokio::time::timeout(timeout, build).await {
                    Ok(built) => built,
                    Err(_) => {
                        // Top songs are an addition, never a reason for anything to fail.
                        debug!("top songs '{cache_key}' timed out");
                        return (Arc::new(TopList::default()), false);
                    }
                };
                let list = Arc::new(list);
                let keep = if whole && !list.songs.is_empty() {
                    ttl
                } else {
                    SHORT_TTL
                };
                this.kept(place).set(cache_key, list.clone(), 1, keep);
                (list, whole)
            });
            if joined {
                (running, urgent.get(&building).cloned())
            } else {
                urgent.insert(building, lane.clone());
                (running, None)
            }
        };
        // A request waiting on a background build makes it interactive.
        if lane.is_now()
            && let Some(started) = started
            && started.make_now()
        {
            self.inner.apple.gate().wake_behind();
        }
        let (list, _) = running.await;
        list
    }

    fn kept(&self, place: Kept) -> &MemoryCache<Arc<TopList>> {
        match place {
            Kept::Server => &self.inner.cache,
            Kept::Other => &self.inner.others,
        }
    }

    /// Outside songs for the rows, registered so they play, open their album and are added like
    /// any other outside song. A row a catalog dressed carries its album, cover and length; the
    /// rest are looked up the way search looks its rows up.
    async fn build_songs(&self, rows: &[PlannedRow]) -> Vec<TopSong> {
        let mut found: Vec<Option<(Song, &PlannedRow)>> = Vec::with_capacity(rows.len());
        let mut bare_at: Vec<usize> = Vec::new();
        for row in rows {
            match &row.catalog {
                Some(track) => found.push(Some((
                    catalog_song(&self.inner.registry, &self.inner.hints, track),
                    row,
                ))),
                None => {
                    let hits = self
                        .inner
                        .metadata
                        .search_songs_by_artist_title(&row.artist, &row.title, 1, None)
                        .await;
                    match hits.into_iter().next() {
                        Some(song) => {
                            bare_at.push(found.len());
                            found.push(Some((song, row)));
                        }
                        None => found.push(None),
                    }
                }
            }
        }
        if !bare_at.is_empty() {
            let mut bare: Vec<Song> = bare_at
                .iter()
                .filter_map(|at| found[*at].as_ref().map(|(song, _)| song.clone()))
                .collect();
            self.inner.metadata.enrich_external_songs(&mut bare).await;
            for (at, song) in bare_at.iter().zip(bare) {
                if let Some((slot, _)) = found[*at].as_mut() {
                    *slot = song;
                }
            }
        }
        let songs: Vec<TopSong> = found
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, (song, row))| TopSong {
                rank: i + 1,
                song,
                plays: row.plays,
                listeners: row.listeners,
            })
            .collect();

        // A list is opened to be played: warm the covers it shows and the first few streams, as
        // search does for its own first screen. Never awaited.
        let all: Vec<Song> = songs.iter().map(|top| top.song.clone()).collect();
        let metadata = Arc::clone(&self.inner.metadata);
        tokio::spawn(async move {
            metadata.prewarm_cover_art(&all, 20).await;
            metadata.prewarm_you_tube_ids(&all, 5).await;
        });
        songs
    }

    /// The library song each row already is, or None, in row order. `search_songs` searches the
    /// caller's library and answers its song list (Navidrome's searchResult3 songs), or None when
    /// it could not. A row is the library's only when it is the same recording, by the rule radio
    /// uses to play a library copy in place of an outside one.
    pub async fn match_library<F, Fut>(rows: &[TopSong], search_songs: F) -> Vec<Option<Value>>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Option<Vec<Value>>>,
    {
        Self::match_library_with(rows, search_songs, &MatchSession::none()).await
    }

    /// [`Self::match_library`] under a session: a row the library already answered it lacks is
    /// not searched, and every answer is recorded. A row it matched is searched each time, so
    /// the song comes back as the library has it now.
    pub async fn match_library_with<F, Fut>(
        rows: &[TopSong],
        search_songs: F,
        session: &MatchSession<'_>,
    ) -> Vec<Option<Value>>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Option<Vec<Value>>>,
    {
        // Plain futures behind a semaphore, not a stream combinator: a combinator's closures
        // make a handler's future fail rustc's Send check ("not general enough").
        let gate = tokio::sync::Semaphore::new(LIBRARY_SEARCHES_AT_ONCE);
        let search_songs = &search_songs;
        let searches = rows.iter().map(|row| {
            let (artist, title) = (row.song.artist.clone(), row.song.title.clone());
            let query = library_query(&artist, &title);
            let search = (!session.skips(&query)).then(|| search_songs(query.clone()));
            let gate = &gate;
            async move {
                let search = search?;
                let _turn = gate.acquire().await.ok()?;
                let hits = search.await?;
                let mut hit = library_hit(&artist, &title, &hits).cloned();
                if hit.is_none()
                    && let Some(by_title) = library_title_query(&artist, &title)
                {
                    // The file may be credited to another name of the artist ("Ye" for "Kanye
                    // West"); the title alone finds it, and the same rule judges the hits. Held
                    // inside this turn, so searches stay four at a time; the one miss recorded
                    // below covers both, so the retry is not repeated.
                    let by_title_hits = search_songs(by_title).await?;
                    hit = library_hit(&artist, &title, &by_title_hits).cloned();
                }
                session.record(&query, hit.as_ref());
                hit
            }
        });
        let mut found: Vec<Option<Value>> = futures::future::join_all(searches).await;
        one_row_per_library_song(&mut found);
        found
    }

    /// The rows to match against the library for a list of `count` while Skip covers and instrumentals is
    /// `skip`: every row up to the `count`th one it lets through. The rows it would hide are
    /// matched too, since a song the library has is listed whatever it is.
    pub fn rows_to_match(rows: &[TopSong], count: usize, skip: bool) -> Vec<TopSong> {
        let mut let_through = 0;
        let mut taken = Vec::new();
        for row in rows {
            if let_through >= count {
                break;
            }
            if !skipped_takes::hides_song(&row.song, skip) {
                let_through += 1;
            }
            taken.push(row.clone());
        }
        taken
    }

    /// The rows a list shows, each with its library match, at most `count`: an outside
    /// cover, karaoke or instrumental take is left out while Skip covers and instrumentals is `skip`, unless the
    /// library has it. Rows keep their chart rank.
    pub fn shown_rows(
        rows: Vec<TopSong>,
        library: Vec<Option<Value>>,
        count: usize,
        skip: bool,
    ) -> (Vec<TopSong>, Vec<Option<Value>>) {
        let mut library = library.into_iter();
        rows.into_iter()
            .map(|row| (row, library.next().flatten()))
            .filter(|(row, owned)| owned.is_some() || !skipped_takes::hides_song(&row.song, skip))
            .take(count)
            .unzip()
    }
}

/// Which cache a list is kept in.
#[derive(Debug, Clone, Copy)]
enum Kept {
    /// The server's charts and the artists' lists.
    Server,
    /// Charts of other countries.
    Other,
}

/// Forgets a build's lane when dropped, with the build.
struct Forget(TopSongsService, String);

impl Drop for Forget {
    fn drop(&mut self) {
        self.0.inner.urgent.lock().remove(&self.1);
    }
}

/// Lets a country's learning go when dropped.
struct Learned(TopSongsService, String);

impl Drop for Learned {
    fn drop(&mut self) {
        self.0.inner.learning.lock().remove(&self.1);
    }
}

#[cfg(test)]
impl TopSongsService {
    /// Sets a country's genre menu, as Apple's page would.
    pub(crate) fn set_menu(&self, country: &str, menu: Vec<ChartGenre>) {
        self.inner
            .menus
            .set(country.to_string(), Arc::new(menu), 1, MENU_TTL);
    }
}

fn chart_key(country: Option<&str>, id: &str) -> String {
    format!("chart|{}|{id}", country.unwrap_or("auto"))
}

/// A catalog track as an outside song: registered through New Releases' shared
/// [`for_you_lists::catalog_song`] (artist, album and song, so it plays, opens its album and is
/// added like any other), then given what a chart row carries besides: every credited artist,
/// the explicit mark, and its artwork remembered for its cover.
pub fn catalog_song(registry: &ExternalIdRegistry, hints: &ArtworkHints, track: &CatalogTrack) -> Song {
    let album = track.album.as_deref().filter(|a| !is_blank(a));
    let mut song = for_you_lists::catalog_song(
        registry,
        &for_you_lists::CatalogTrackSong {
            title: &track.title,
            artist: &track.artist,
            artist_id: track.artist_id.as_deref(),
            album,
            album_id: track.album_id.as_deref(),
            cover_url: track.cover_url.as_deref(),
            duration: track.duration,
        },
    );
    if let Some(url) = track.cover_url.as_deref() {
        hints.remember(RoutingKind::Song, &track.artist, &track.title, url);
        if let Some(album) = album {
            hints.remember(RoutingKind::Album, &track.artist, album, url);
        }
    }
    song.explicit_content_lyrics = track
        .explicit_content
        .or(track.explicit.map(|e| if e { 1 } else { 0 }));
    song.artists = track.artists.clone();
    song.primary_artist = (!track.artists.is_empty()).then(|| track.artist.clone());
    song
}

#[cfg(test)]
#[path = "top_songs_service_tests.rs"]
mod tests;
