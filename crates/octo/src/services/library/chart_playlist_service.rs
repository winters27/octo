//! The chart playlists: Popular right now, Best New Songs, Trending Songs and the genre charts,
//! each a read-only playlist for every listener, so every Subsonic app gets them beside the radio
//! stations and mixes. A song the listener's library has is their own copy, as Navidrome
//! describes it to them; the rest are outside songs that play right away and are added with a
//! heart, while search discovery is on and the explicit filter and Skip covers and instrumentals let them.
//!
//! Per listener because the library match is theirs, and made with their own sign-in inside
//! their own request, as the mixes are: Octo keeps no one's Navidrome password. A list holds
//! still for [`REFRESH_HOURS`] so every app shows the same songs, then is made again the next
//! time that listener's app lists playlists. A source or library that does not answer never
//! empties a list that was made. Nothing is written to Navidrome, and nothing to disk.
//!
//! A listener's lists are those of their own chart country, then the same lists for each country
//! they pinned (`Popular right now · Japan`). They are made one after another (six lists searched
//! at once would ask Navidrome two dozen searches at a time), only those that are due, a song on
//! two lists is searched once, a list is listed as soon as its chart is known while search
//! discovery is on (the warm worker fetches the charts at start, so an app's first look after a
//! restart has them), and a list's change time moves only when its songs do.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::charts::apple_pages::ChartGenre;
use octo_core::charts::chart_settings::{self, ChartKind, OVERALL, list_id, list_key, split_list};
use octo_core::charts::top_songs::{APPLE_SOURCE, library_hit, library_query, library_title_query, one_row_per_library_song};
use octo_core::common::{Clock, skipped_takes};
use octo_core::library::generated_playlist_service::{period_index, playlist_id, user_key};
use octo_core::models::domain::Song;
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioTrack};
use octo_core::settings::{ExplicitFilter, SettingsStore};
use octo_core::soulseek::LengthSource;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_subsonic::SubsonicResponseBuilder;
use octo_subsonic::subsonic_request_parser::Parameters;
use parking_lot::Mutex;
use serde_json::Value;
use tracing::{debug, info};

use crate::services::common::{TopSong, TopSongsService, top_songs_service::LIBRARY_SEARCHES_AT_ONCE};
use crate::services::family::{FamilyStore, kid_filter};
use crate::services::framework::InFlight;
use crate::services::library::GeneratedPlaylist;
use crate::services::library::library_match_cache::{LibraryMatchCache, MatchSession};
use crate::services::soulseek::external_id_registry::ExternalIdRegistry;
use crate::services::subsonic::SubsonicProxyService;

/// The kind every chart list carries, as a mix carries "genre" or "decade".
pub const KIND: &str = "chart";

/// Marks the sync catalog's tracks that came from a chart list, whose ids are the list's own
/// rather than found again by search. (Popular right now's word, kept for every list.)
pub const TRACK_SOURCE: &str = "popular";

/// How long one list holds still.
pub const REFRESH_HOURS: i32 = 6;

const FIRST_LIST_WAIT: Duration = Duration::from_millis(1500);
const OPEN_WAIT: Duration = Duration::from_secs(10);
const BUILD_DEADLINE: Duration = Duration::from_secs(60);
const RETRY_AFTER_FAILURE: TimeDelta = TimeDelta::minutes(5);

/// The Subsonic parameters that say who is asking: a password, a token and its salt, or an API
/// key, with the protocol version and the app's name.
const SIGN_IN_KEYS: [&str; 7] = ["u", "p", "t", "s", "apiKey", "v", "c"];

/// A listener's list as made: for which period and settings, whether every library lookup
/// answered, its songs as Subsonic song JSON, and its outside songs.
#[derive(Debug, Clone)]
struct Made {
    period: i64,
    made_utc: DateTime<Utc>,
    changed_utc: DateTime<Utc>,
    rule: String,
    whole: bool,
    /// Where the chart came from (`apple`, `deezer`, `lastfm`); a list from anywhere but Apple
    /// never replaces one that came from Apple.
    source: String,
    name: String,
    hash: u64,
    entries: Vec<Value>,
    outside: Vec<Song>,
}

/// What a list's cover is drawn from.
#[derive(Debug, Clone)]
pub struct ChartCover {
    pub name: String,
    /// The chart's id, without the pinned country.
    pub chart: String,
    /// The pinned country, on a pinned country's list.
    pub country: Option<String>,
    pub entries: Vec<Value>,
    pub outside: Vec<Song>,
}

/// Cheap to clone: the clones share one set of lists.
#[derive(Clone)]
pub struct ChartPlaylistService {
    inner: Arc<Inner>,
}

struct Inner {
    top_songs: TopSongsService,
    responses: Arc<SubsonicResponseBuilder>,
    registry: Arc<ExternalIdRegistry>,
    proxy: SubsonicProxyService,
    settings: Arc<SettingsStore>,
    clock: Clock,
    made: Mutex<HashMap<String, HashMap<String, Made>>>,
    /// When a listener's list last could not be made whole, by listener and list, with the rule
    /// that build ran under: a new rule makes the list due at once.
    failed_at: Mutex<HashMap<(String, String), (DateTime<Utc>, String)>>,
    builds: InFlight<bool>,
    /// The family, whose kid profiles get clean lists.
    family: Option<Arc<FamilyStore>>,
    /// What each listener's library already said it lacks, so a build searches it once.
    library_matches: Option<Arc<LibraryMatchCache>>,
}

impl ChartPlaylistService {
    pub fn new(
        top_songs: TopSongsService,
        responses: Arc<SubsonicResponseBuilder>,
        registry: Arc<ExternalIdRegistry>,
        proxy: SubsonicProxyService,
        settings: Arc<SettingsStore>,
        clock: Clock,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                top_songs,
                responses,
                registry,
                proxy,
                settings,
                clock,
                made: Mutex::new(HashMap::new()),
                failed_at: Mutex::new(HashMap::new()),
                builds: InFlight::new(),
                family: None,
                library_matches: None,
            }),
        }
    }

    /// The service with a cache of library matches shared across builds. Call it before the
    /// service is cloned.
    pub fn with_library_matches(mut self, cache: Arc<LibraryMatchCache>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.library_matches = Some(cache);
        }
        self
    }

    /// The service with the family, so a kid profile's lists are clean. Call it before the
    /// service is cloned.
    pub fn with_family(mut self, family: Arc<FamilyStore>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.family = Some(family);
        }
        self
    }

    /// Lets go of everything kept for a listener: their made lists, the times their lists
    /// failed, and what their library was asked.
    pub fn forget_person(&self, username: &str) {
        let user = user_key(username);
        self.inner.made.lock().remove(&user);
        self.inner.failed_at.lock().retain(|(who, _), _| *who != user);
        if let Some(cache) = &self.inner.library_matches {
            cache.forget(username);
        }
    }

    /// The explicit filter for one listener's lists: the server's, or clean only for a kid
    /// profile.
    fn explicit_filter_for(&self, username: &str) -> ExplicitFilter {
        kid_filter::explicit_filter_for(
            &self.inner.settings.current(),
            self.inner.family.as_deref(),
            Some(username),
        )
    }

    /// The charts that are lists, in their order: Popular right now first while it is on, then
    /// the `Charts` setting's. Genres are checked against the country's menu by [`Self::list`].
    pub fn lists_on(&self) -> Vec<String> {
        let settings = &self.inner.settings.current().generated_playlists;
        let mut on = Vec::new();
        if settings.popular_now {
            on.push(OVERALL.to_string());
        }
        on.extend(settings.chart_ids());
        on
    }

    /// Every list id a listener has while on: [`Self::lists_on`], then the same lists for each
    /// country they pinned (`34@jp`). Genres are checked against each country's menu by
    /// [`Self::list`].
    pub fn lists_for(&self, username: &str) -> Vec<String> {
        let on = self.lists_on();
        let top = &self.inner.top_songs;
        let home = top.home_of(username);
        let mut out = on.clone();
        for pin in &top.listeners().get(username).pinned {
            if Some(pin) == home.as_ref() || !top.has_country(pin) {
                continue;
            }
            out.extend(on.iter().map(|chart| list_id(chart, Some(pin))));
        }
        out
    }

    /// The listener's lists that are on and each country has, with their names: their own
    /// country's first, then each pinned country's.
    async fn charts_for(&self, username: &str) -> Vec<(String, String)> {
        let top = &self.inner.top_songs;
        let home = top.country_of(username).await;
        let (_, menu) = top.menu_in(home.clone()).await;
        let mut out = self.named(&menu, None);
        for pin in top.listeners().get(username).pinned {
            if Some(pin.as_str()) == home.as_deref() || !top.has_country(&pin) {
                continue;
            }
            let (_, menu) = top.menu_in(Some(pin.clone())).await;
            out.extend(self.named(&menu, Some(&pin)));
        }
        out
    }

    /// The lists that are on and the menu has, as list ids and names, for the listener's own
    /// country or a pinned one.
    fn named(&self, menu: &[ChartGenre], pinned: Option<&str>) -> Vec<(String, String)> {
        self.lists_on()
            .into_iter()
            .filter_map(|chart| {
                if chart_settings::kind(&chart) == Some(ChartKind::Genre)
                    && !menu.iter().any(|g| g.id == chart)
                {
                    return None;
                }
                let (name, _) = TopSongsService::names(menu, &chart)?;
                let name = match pinned {
                    Some(country) => chart_settings::pinned_name(&name, country),
                    None => name,
                };
                Some((list_id(&chart, pinned), name))
            })
            .collect()
    }

    /// The country a list is read for: its pinned country, else the listener's own.
    async fn country_for(&self, username: &str, list: &str) -> Option<String> {
        match split_list(list).1 {
            Some(country) => Some(country.to_string()),
            None => self.inner.top_songs.country_of(username).await,
        }
    }

    /// The listener's lists, made again first when their period ended or the settings they were
    /// made under changed. A listener's very first lists wait a moment, so they show on their
    /// first look; later ones are made behind the answer.
    pub async fn list(&self, username: &str, auth: &Parameters) -> Vec<GeneratedPlaylist> {
        if username.trim().is_empty() {
            return Vec::new();
        }
        let (any_made, building) = self.ensure(username, auth).await;
        if !any_made && let Some(building) = building {
            let _ = tokio::time::timeout(FIRST_LIST_WAIT, building).await;
        }
        let discovery = self.inner.settings.current().subsonic.enable_search_discovery;
        let mut lists = Vec::new();
        for (list, name) in self.charts_for(username).await {
            let made = self.current(username, &list);
            let chart = split_list(&list).0;
            let pool = match &made {
                Some(made) if !made.entries.is_empty() => made.entries.len(),
                _ if discovery => {
                    let country = self.country_for(username, &list).await;
                    match self.inner.top_songs.peek_chart_in(country, chart).await {
                        Some(source) => source.songs.len().min(chart_settings::max_songs(chart)),
                        None => 0,
                    }
                }
                _ => 0,
            };
            if pool > 0 {
                lists.push(self.playlist(username, &list, made.map_or(name, |m| m.name), pool));
            }
        }
        lists
    }

    /// Which list a playlist id is, for this listener, while that list is on: an app that kept
    /// the id across a restart still opens it.
    pub fn which(&self, username: &str, id: &str) -> Option<String> {
        if username.trim().is_empty() || id.is_empty() {
            return None;
        }
        self.lists_for(username)
            .into_iter()
            .find(|list| playlist_id(username, &list_key(list)) == id)
    }

    /// The listener's playlist id for a list, while the list is made and holds songs.
    pub fn listed_id(&self, username: &str, list: &str) -> Option<String> {
        let made = self.current(username, list)?;
        (!made.entries.is_empty()).then(|| playlist_id(username, &list_key(list)))
    }

    /// The list as a playlist: its period, and `pool` songs.
    pub fn playlist(&self, username: &str, list: &str, name: String, pool: usize) -> GeneratedPlaylist {
        let period = self.current(username, list).map_or_else(
            || period_index(self.inner.clock.now(), REFRESH_HOURS),
            |m| m.period,
        );
        let start = period_start(period);
        GeneratedPlaylist {
            id: playlist_id(username, &list_key(list)),
            key: list_key(list),
            kind: KIND.to_string(),
            label: name.clone(),
            name,
            owner: username.trim().to_string(),
            pool_size: i32::try_from(pool).unwrap_or(i32::MAX),
            period_start_utc: start,
            period_end_utc: start + TimeDelta::hours(i64::from(REFRESH_HOURS)),
        }
    }

    /// The list's playlist as made, for getPlaylist; None while nothing is made.
    pub fn made_playlist(&self, username: &str, chart: &str) -> Option<GeneratedPlaylist> {
        let made = self.current(username, chart)?;
        Some(self.playlist(username, chart, made.name.clone(), made.entries.len()))
    }

    /// Starts making the listener's lists when they are due, without waiting.
    pub async fn warm(&self, username: &str, auth: &Parameters) {
        if !username.trim().is_empty() {
            let _ = self.ensure(username, auth).await;
        }
    }

    /// The list's songs, made again first when due (waiting a few seconds for that, then
    /// answering with the last one rather than keep an app waiting).
    pub async fn songs(&self, username: &str, chart: &str, auth: &Parameters) -> Vec<Value> {
        if username.trim().is_empty() {
            return Vec::new();
        }
        let (_, building) = self.ensure(username, auth).await;
        if let Some(building) = building {
            let made = self
                .current(username, chart)
                .is_some_and(|m| !m.entries.is_empty());
            let wait = if made { OPEN_WAIT } else { BUILD_DEADLINE };
            let _ = tokio::time::timeout(wait, building).await;
        }
        let Some(made) = self.current(username, chart) else {
            return Vec::new();
        };
        self.keep_playable(&made.outside);
        made.entries
    }

    /// What the list's cover is drawn from, as last made. Never makes one.
    pub fn cover(&self, username: &str, id: &str) -> Option<ChartCover> {
        let list = self.which(username, id)?;
        let made = self.current(username, &list)?;
        let (chart, country) = split_list(&list);
        Some(ChartCover {
            name: made.name,
            chart: chart.to_string(),
            country: country.map(str::to_string),
            entries: made.entries,
            outside: made.outside,
        })
    }

    /// The list's outside songs as last made, in order. Never makes one.
    pub fn outside_songs(&self, username: &str, chart: &str) -> Vec<Song> {
        self.current(username, chart)
            .map(|m| m.outside)
            .unwrap_or_default()
    }

    /// Outside songs play by their id, which Octo remembers in memory for a while. A list holds
    /// still for hours, so before it is handed out each of its outside songs is made known again
    /// if it was forgotten: the same song gives the same id.
    pub fn keep_playable(&self, songs: &[Song]) {
        let registry = &self.inner.registry;
        for song in songs {
            if registry.lookup(&song.id).is_some() {
                continue;
            }
            if song
                .artist_id
                .as_deref()
                .is_some_and(|id| registry.lookup(id).is_none())
            {
                registry.register(SoulseekRouting {
                    kind: RoutingKind::Artist,
                    artist: Some(song.artist.clone()),
                    ..Default::default()
                });
            }
            if song
                .album_id
                .as_deref()
                .is_some_and(|id| registry.lookup(id).is_none())
                && !song.album.trim().is_empty()
            {
                registry.register(SoulseekRouting {
                    kind: RoutingKind::Album,
                    artist: Some(song.artist.clone()),
                    album: Some(song.album.clone()),
                    ..Default::default()
                });
            }
            let id = registry.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some(song.artist.clone()),
                title: Some(song.title.clone()),
                album: Some(song.album.clone()).filter(|a| !a.is_empty()),
                duration: song.duration,
                ..Default::default()
            });
            registry.remember_length(&id, song.duration, LengthSource::Deezer);
        }
    }

    /// Each list's outside songs as a station, for the sync catalog of a client that copies the
    /// library to the device: it then holds these songs under the ids the playlist lists, so the
    /// playlist finds them there.
    pub fn as_sync_stations(&self, username: &str) -> Vec<LastFmRadioStation> {
        let on = self.lists_for(username);
        let user = user_key(username);
        let made = self.inner.made.lock().get(&user).cloned().unwrap_or_default();
        on.iter()
            .filter_map(|list| {
                let made = made.get(list).filter(|m| !m.outside.is_empty())?;
                Some(LastFmRadioStation {
                    id: playlist_id(username, &list_key(list)),
                    key: list_key(list),
                    name: made.name.clone(),
                    owner: username.trim().to_string(),
                    created_utc: made.made_utc,
                    changed_utc: made.changed_utc,
                    valid_until_utc: period_start(made.period + 1),
                    tracks: made
                        .outside
                        .iter()
                        .map(|song| LastFmRadioTrack {
                            artist: song.artist.clone(),
                            title: song.title.clone(),
                            album: Some(song.album.clone()),
                            duration: song.duration,
                            source: TRACK_SOURCE.to_string(),
                            resolved_id: Some(song.id.clone()),
                            external_provider: song.external_provider.clone(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                })
            })
            .collect()
    }

    fn current(&self, username: &str, list: &str) -> Option<Made> {
        self.inner
            .made
            .lock()
            .get(&user_key(username))?
            .get(list)
            .cloned()
    }

    /// What decides whether a made list still holds: outside songs shown or not, which of them
    /// the listener's explicit filter and Skip covers and instrumentals let through, and whose
    /// chart it is: the listener's own country for their own lists, so a new own country makes
    /// those again, and the pinned country for a pinned one, which a new own country leaves be.
    fn rule(&self, username: &str, list: &str, home: Option<&str>) -> String {
        let settings = self.inner.settings.current();
        let skip_covers_and_instrumentals = settings.soulseek.skip_covers_and_instrumentals;
        let country = split_list(list).1.or(home).unwrap_or("auto");
        format!(
            "{}|{:?}{}|{country}",
            settings.subsonic.enable_search_discovery,
            self.explicit_filter_for(username),
            if skip_covers_and_instrumentals {
                "|skip-covers-and-instrumentals"
            } else {
                ""
            }
        )
    }

    /// Whether any list is made, and the making of the ones that are due. A list that could not
    /// be made whole a moment ago waits a few minutes, without holding up the others.
    async fn ensure(
        &self,
        username: &str,
        auth: &Parameters,
    ) -> (
        bool,
        Option<futures::future::Shared<futures::future::BoxFuture<'static, bool>>>,
    ) {
        let user = user_key(username);
        let now = self.inner.clock.now();
        let period = period_index(now, REFRESH_HOURS);
        let home = self.inner.top_songs.country_of(username).await;
        // Only the lists each country has: a genre it lacks is never made, and must not make the
        // others due on every request.
        let on = self.charts_for(username).await;
        // Every list that is on, whatever a country's menu says just now: a menu not known for a
        // moment must not let a made list go.
        let kept = self.lists_for(username);
        // A list that could not be made whole waits, but only under the rule it failed under.
        let waiting: Vec<(String, String)> = {
            let mut failed = self.inner.failed_at.lock();
            failed.retain(|(who, list), (at, _)| {
                now - *at < RETRY_AFTER_FAILURE && (*who != user || kept.contains(list))
            });
            failed
                .iter()
                .filter(|((who, _), _)| *who == user)
                .map(|((_, list), (_, rule))| (list.clone(), rule.clone()))
                .collect()
        };
        let (any_made, due) = {
            let mut made = self.inner.made.lock();
            // A list turned off, or a country unpinned, is let go; a list still on is kept even
            // while it is due.
            if let Some(mine) = made.get_mut(&user) {
                mine.retain(|list, _| kept.contains(list));
            }
            let mine = made.get(&user);
            let any_made = mine.is_some_and(|m| {
                on.iter()
                    .any(|(l, _)| m.get(l).is_some_and(|x| !x.entries.is_empty()))
            });
            let due: Vec<(String, String, String)> = on
                .iter()
                .map(|(list, name)| {
                    (
                        list.clone(),
                        name.clone(),
                        self.rule(username, list, home.as_deref()),
                    )
                })
                .filter(|(list, _, rule)| !waiting.iter().any(|(l, r)| l == list && r == rule))
                .filter(|(list, _, rule)| {
                    mine.and_then(|m| m.get(list))
                        .is_none_or(|m| m.period != period || m.rule != *rule || !m.whole)
                })
                .collect();
            (any_made, due)
        };
        if due.is_empty() {
            return (any_made, None);
        }
        let sign_in: Parameters = auth
            .iter()
            .filter(|(k, _)| SIGN_IN_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let this = self.clone();
        let name = username.to_string();
        let building = self
            .inner
            .builds
            .run(&user, async move { this.make(&name, sign_in, period, due).await });
        (any_made, Some(building))
    }

    /// Make these lists of the listener (id, name and the rule each is made under), one after
    /// another: each chart, each song matched against their library by the rule radio uses to
    /// play a library copy, the rest kept as outside songs when they may be shown. A list that
    /// could not be made whole waits a few minutes before it is tried again, on its own.
    async fn make(
        &self,
        username: &str,
        auth: Parameters,
        period: i64,
        lists: Vec<(String, String, String)>,
    ) -> bool {
        let user = user_key(username);
        let settings = self.inner.settings.current();
        let subsonic = settings.subsonic.clone();
        let skip_covers_and_instrumentals = settings.soulseek.skip_covers_and_instrumentals;
        let explicit_filter = self.explicit_filter_for(username);
        let mut searched: HashMap<String, Option<Vec<Value>>> = HashMap::new();
        let session = match &self.inner.library_matches {
            Some(cache) => cache.session(&self.inner.proxy, &auth, username).await,
            None => MatchSession::none(),
        };
        let mut any = false;
        let mut trouble_with: Vec<(String, String)> = Vec::new();
        let mut whole: Vec<String> = Vec::new();
        for (list_id, name, rule) in lists {
            let chart = split_list(&list_id).0.to_string();
            let country = self.country_for(username, &list_id).await;
            let list = self.inner.top_songs.chart_in(country, &chart).await;
            let count = chart_settings::max_songs(&chart);
            // A row more for each outside take Skip covers, karaoke and instrumentals leaves out,
            // so the next rows take their places, as on getTopSongs.
            let rows: Vec<TopSong> =
                TopSongsService::rows_to_match(&list.songs, count, skip_covers_and_instrumentals);
            if rows.is_empty() {
                trouble_with.push((list_id, rule));
                info!("{name} for {username}: the chart did not answer, the last list stays");
                continue;
            }

            // Search what no earlier list of this listener searched, four at a time.
            let queries: Vec<String> = rows
                .iter()
                .map(|r| library_query(&r.song.artist, &r.song.title))
                .collect();
            let missing: Vec<String> = {
                let mut seen = Vec::new();
                for q in &queries {
                    if searched.contains_key(q) || seen.contains(q) {
                        continue;
                    }
                    if session.skips(q) {
                        // The library already said it lacks this song.
                        searched.insert(q.clone(), Some(Vec::new()));
                    } else {
                        seen.push(q.clone());
                    }
                }
                seen
            };
            // Four at a time, as plain futures behind a semaphore (see `match_library`).
            let gate = tokio::sync::Semaphore::new(LIBRARY_SEARCHES_AT_ONCE);
            let asks = missing.into_iter().map(|query| {
                let (proxy, auth, gate) = (self.inner.proxy.clone(), auth.clone(), &gate);
                async move {
                    let _turn = gate.acquire().await.ok();
                    let found = search_library(&proxy, &auth, &query).await;
                    (query, found)
                }
            });
            let answers: Vec<(String, Option<Vec<Value>>)> = futures::future::join_all(asks).await;
            searched.extend(answers);

            // A copy filed under another name of the artist ("Ye" for "Kanye West") is not found
            // by the artist's name asked: where the library answered but has no such song, ask
            // once more with the title alone, as one search per song, four at a time. What it
            // finds is added to that row's hits, so the same rule judges them and the other
            // lists reuse them; a miss stays recorded as one miss below.
            let mut retries: Vec<(String, String)> = Vec::new();
            for (row, query) in rows.iter().zip(&queries) {
                let Some(Some(hits)) = searched.get(query) else {
                    continue;
                };
                if session.skips(query)
                    || library_hit(&row.song.artist, &row.song.title, hits).is_some()
                    || retries.iter().any(|(q, _)| q == query)
                {
                    continue;
                }
                if let Some(by_title) = library_title_query(&row.song.artist, &row.song.title) {
                    retries.push((query.clone(), by_title));
                }
            }
            let to_search: Vec<String> = {
                let mut seen: Vec<String> = Vec::new();
                for (_, by_title) in &retries {
                    if !searched.contains_key(by_title) && !seen.contains(by_title) {
                        seen.push(by_title.clone());
                    }
                }
                seen
            };
            let gate = tokio::sync::Semaphore::new(LIBRARY_SEARCHES_AT_ONCE);
            let asks = to_search.into_iter().map(|query| {
                let (proxy, auth, gate) = (self.inner.proxy.clone(), auth.clone(), &gate);
                async move {
                    let _turn = gate.acquire().await.ok();
                    let found = search_library(&proxy, &auth, &query).await;
                    (query, found)
                }
            });
            searched.extend(futures::future::join_all(asks).await);
            for (query, by_title) in retries {
                match searched.get(&by_title).cloned().flatten() {
                    Some(more) => {
                        if let Some(Some(hits)) = searched.get_mut(&query) {
                            hits.extend(more);
                        }
                    }
                    // The retry was not answered: the row is unanswered, not a recorded miss.
                    None => {
                        searched.insert(query, None);
                    }
                }
            }

            let mut unanswered = 0;
            let mut library: Vec<Option<Value>> = rows
                .iter()
                .zip(&queries)
                .map(|(row, query)| match searched.get(query).cloned().flatten() {
                    Some(hits) => {
                        let hit = library_hit(&row.song.artist, &row.song.title, &hits).cloned();
                        session.record(query, hit.as_ref());
                        hit
                    }
                    None => {
                        unanswered += 1;
                        None
                    }
                })
                .collect();
            one_row_per_library_song(&mut library);

            let before = self.current(username, &list_id);
            // A library that answered nothing at all must not turn a list of the listener's own
            // songs into outside ones.
            if unanswered == rows.len() && before.as_ref().is_some_and(|b| !b.entries.is_empty()) {
                trouble_with.push((list_id, rule));
                continue;
            }
            let from_apple = list.source == APPLE_SOURCE;
            // A stand-in for Apple's chart keeps the Apple list that was made, and asks Apple
            // again soon.
            if !from_apple && before.as_ref().is_some_and(|b| b.source == APPLE_SOURCE) {
                trouble_with.push((list_id, rule));
                continue;
            }
            if unanswered > 0 || !from_apple {
                trouble_with.push((list_id.clone(), rule.clone()));
            } else {
                whole.push(list_id.clone());
            }

            let (entries, outside) = assemble(
                &rows,
                &library,
                subsonic.enable_search_discovery,
                explicit_filter,
                skip_covers_and_instrumentals,
                count,
                |song| Value::Object(self.inner.responses.convert_song_to_json(song)),
            );
            let hash = entries_hash(&entries);
            let now = self.inner.clock.now();
            let changed_utc = before
                .as_ref()
                .filter(|b| b.hash == hash)
                .map_or(now, |b| b.changed_utc);
            info!(
                "{name} for {username}: {} library and {} outside songs from the {} chart",
                entries.len() - outside.len(),
                outside.len(),
                if list.source.is_empty() {
                    "unknown"
                } else {
                    list.source.as_str()
                }
            );
            let made = Made {
                period,
                made_utc: now,
                changed_utc,
                rule,
                whole: unanswered == 0 && from_apple,
                source: list.source.clone(),
                name,
                hash,
                entries,
                outside,
            };
            self.inner
                .made
                .lock()
                .entry(user.clone())
                .or_default()
                .insert(list_id, made);
            any = true;
        }
        let now = self.inner.clock.now();
        let mut failed = self.inner.failed_at.lock();
        for (list, rule) in trouble_with {
            failed.insert((user.clone(), list), (now, rule));
        }
        for list in whole {
            failed.remove(&(user.clone(), list));
        }
        if !any {
            debug!("chart lists for {username}: nothing could be made");
        }
        any
    }
}

/// The list in chart order: the library's song where the row has one, else the outside song when
/// outside songs may be shown, the explicit filter lets it through, and it is no cover, instrumental or
/// karaoke take Skip covers and instrumentals leaves out. The filter is the one stations follow: a list Octo
/// picks follows it, a search does not. At most `count` songs: `rows` may run past that, so a row
/// left out is made up by the next one.
pub fn assemble(
    rows: &[TopSong],
    library: &[Option<Value>],
    outside_shown: bool,
    filter: ExplicitFilter,
    skip_covers_and_instrumentals: bool,
    count: usize,
    as_json: impl Fn(&Song) -> Value,
) -> (Vec<Value>, Vec<Song>) {
    let mut entries = Vec::new();
    let mut outside = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if entries.len() >= count {
            break;
        }
        if let Some(owned) = library.get(i).and_then(Option::as_ref) {
            entries.push(owned.clone());
        } else if outside_shown
            && passes(&row.song, filter)
            && !skipped_takes::hides_song(&row.song, skip_covers_and_instrumentals)
        {
            entries.push(as_json(&row.song));
            outside.push(row.song.clone());
        }
    }
    (entries, outside)
}

/// `ExplicitStatus.ForClients` against the filter: a clean-only list leaves out what is marked
/// explicit (1, or 4 for a partly explicit album) and an outside song whose words are not known
/// because its lookup got no answer; an explicit-only one what is a clean edit (3).
pub fn passes(song: &Song, filter: ExplicitFilter) -> bool {
    let clean = song.explicit_content_lyrics == Some(3);
    match filter {
        ExplicitFilter::CleanOnly => {
            octo_core::models::domain::explicit_status::clean_only_allows(song.explicit_content_lyrics)
        }
        ExplicitFilter::ExplicitOnly => !clean,
        _ => true,
    }
}

fn entries_hash(entries: &[Value]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for entry in entries {
        entry
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .hash(&mut hasher);
    }
    hasher.finish()
}

fn period_start(period: i64) -> DateTime<Utc> {
    DateTime::UNIX_EPOCH + TimeDelta::hours(period * i64::from(REFRESH_HOURS))
}

/// Navidrome's songs for a query, searched as the listener with only their sign-in: Some (maybe
/// empty) when it answered, None when it did not. Library only.
async fn search_library(proxy: &SubsonicProxyService, auth: &Parameters, query: &str) -> Option<Vec<Value>> {
    let mut search = auth.clone();
    search.insert("query".into(), query.to_string());
    search.insert("songCount".into(), "5".into());
    search.insert("albumCount".into(), "0".into());
    search.insert("artistCount".into(), "0".into());
    search.insert("f".into(), "json".into());
    let relay = proxy.relay_safe("rest/search3", &search).await?;
    let doc: Value = serde_json::from_slice(&relay.body).ok()?;
    let response = doc.get("subsonic-response")?;
    if response.get("status").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    Some(
        response
            .pointer("/searchResult3/song")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    )
}

#[cfg(test)]
#[path = "chart_playlist_service_tests.rs"]
mod tests;
