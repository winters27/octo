//! Apple's catalog: the iTunes Search API and Apple's chart feed, each request paced by
//! [`ApplePace`] and asked in the server's store, good answers kept for a while.
//!
//! The store is chosen once a process, on first need: the Charts country when it is set, else the
//! server's own country as its [`StoreSource`] learns it, else `us`. Every store the server has
//! used is kept in `apple-store.json`, so an album listed in one store still opens after the store
//! changes: an id lookup that finds nothing in the current store asks the others, then `us`. A
//! store Apple does not have (HTTP 400 naming `[country]`) gives way to `us`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use octo_core::common::dotnet;
use octo_core::metadata::apple_as_deezer;
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use super::apple_pace::{AppleClass, ApplePace, Refused, Ticket};
use crate::services::framework::MemoryCache;
use crate::services::framework::http::client_builder;
use crate::services::state_file;

/// Apple's iTunes Search API.
pub const ITUNES_BASE: &str = "https://itunes.apple.com";
/// Apple's chart feed.
pub const FEED_BASE: &str = "https://rss.marketingtools.apple.com";

/// How long one request to Apple may take.
const TIMEOUT: Duration = Duration::from_secs(6);
/// How long an answer is kept.
const ANSWER_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// How long "Apple has nothing for this" is kept.
const NO_DATA_TTL: Duration = Duration::from_secs(5 * 60);
/// How long a chart is kept.
const CHART_TTL: Duration = Duration::from_secs(60 * 60);
/// What the kept answers may weigh, in KiB.
const CACHE_KIB: u64 = 32 * 1024;
/// The store every other store gives way to.
const FALLBACK_STORE: &str = "us";

/// How Apple answered.
#[derive(Debug, Clone, PartialEq)]
pub enum AppleAsked<T> {
    Answer(T),
    /// Apple answered and has nothing for this.
    NoData,
    /// Apple did not answer this time: down, slow, refusing, or the pace's queue was full.
    Busy,
}

/// What a search looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entity {
    Song,
    Album,
    MusicArtist,
}

impl Entity {
    fn word(self) -> &'static str {
        match self {
            Self::Song => "song",
            Self::Album => "album",
            Self::MusicArtist => "musicArtist",
        }
    }
}

/// Where the store comes from.
#[async_trait::async_trait]
pub trait StoreSource: Send + Sync {
    /// The Charts country when it is set and Apple has it: it pins the store.
    fn pinned(&self) -> Option<String>;

    /// The server's own country when the store is first needed and nothing pins it.
    async fn learned(&self) -> Option<String>;
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoresFile {
    #[serde(default)]
    store: Option<String>,
    #[serde(default)]
    used: Vec<String>,
}

#[derive(Debug, Default)]
struct Stores {
    current: Option<String>,
    used: Vec<String>,
}

/// One request's outcome, shared by everyone who asked the same address at once.
#[derive(Debug, Clone)]
enum Flight {
    Answer(Arc<Value>),
    NoData,
    Busy,
    /// The pace refused the request's turn: never handed to a joiner as its answer.
    Refused,
    /// Apple has no such store.
    NoSuchStore,
}

struct Running {
    outcome: Shared<BoxFuture<'static, Flight>>,
    ticket: Arc<Ticket>,
}

/// The songs a lookup lists for each album: Apple's `limit` counts per id.
const SONGS_PER_ALBUM: u32 = 200;

/// Albums one tracklist lookup asks for.
const BATCH_ALBUMS: usize = 50;

/// Tracklist requests gathering for one Apple turn: every request that arrives before the turn
/// comes joins, while the lookup has room for another album.
struct Gathering {
    ids: Arc<Mutex<Vec<u64>>>,
    ticket: Arc<Ticket>,
    outcome: Shared<BoxFuture<'static, Flight>>,
}

/// The catalog client. Cheap to share behind an `Arc`.
pub struct AppleCatalog {
    client: reqwest::Client,
    base: String,
    feed: String,
    pace: Arc<ApplePace>,
    kept: Arc<MemoryCache<Option<Arc<Value>>>>,
    running: Arc<Mutex<HashMap<String, Running>>>,
    gathering: Arc<Mutex<Option<Gathering>>>,
    batch_albums: usize,
    stores: Mutex<Stores>,
    stores_path: Option<PathBuf>,
    source: OnceLock<Arc<dyn StoreSource>>,
    warned: Arc<Mutex<HashSet<&'static str>>>,
}

impl AppleCatalog {
    /// The catalog on Apple's own hosts, its stores kept at `stores_path`.
    pub fn new(pace: Arc<ApplePace>, stores_path: Option<PathBuf>) -> Self {
        Self::with_bases(pace, ITUNES_BASE, FEED_BASE, stores_path)
    }

    /// The catalog on other hosts: a test's mock servers.
    pub fn with_bases(pace: Arc<ApplePace>, base: &str, feed: &str, stores_path: Option<PathBuf>) -> Self {
        let used = stores_path
            .as_deref()
            .and_then(|path| state_file::read_text(path).ok().flatten())
            .and_then(|text| serde_json::from_str::<StoresFile>(&text).ok())
            .map(|file| file.used)
            .unwrap_or_default();
        Self {
            client: client_builder()
                .timeout(TIMEOUT)
                .build()
                .expect("the Apple catalog client builds"),
            base: base.trim_end_matches('/').to_string(),
            feed: feed.trim_end_matches('/').to_string(),
            pace,
            kept: Arc::new(MemoryCache::new(CACHE_KIB)),
            running: Arc::new(Mutex::new(HashMap::new())),
            gathering: Arc::new(Mutex::new(None)),
            batch_albums: BATCH_ALBUMS,
            stores: Mutex::new(Stores { current: None, used }),
            stores_path,
            source: OnceLock::new(),
            warned: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The same catalog keeping at most `kib` KiB of answers (a test's small cap).
    pub fn with_cache_kib(mut self, kib: u64) -> Self {
        self.kept = Arc::new(MemoryCache::new(kib));
        self
    }

    /// The same catalog asking at most `albums` albums' tracklists in one lookup (a test's cap).
    pub fn with_batch_albums(mut self, albums: usize) -> Self {
        self.batch_albums = albums.max(1);
        self
    }

    /// Where the store comes from; set once, before the first request.
    pub fn set_store_source(&self, source: Arc<dyn StoreSource>) {
        let _ = self.source.set(source);
    }

    /// The store this process asks in.
    pub async fn store(&self) -> String {
        let pinned = self
            .source
            .get()
            .and_then(|source| source.pinned())
            .and_then(normal_store);
        if let Some(pinned) = pinned {
            self.choose(&pinned);
            return pinned;
        }
        if let Some(current) = self.stores.lock().current.clone() {
            return current;
        }
        let learned = match self.source.get() {
            Some(source) => source.learned().await.and_then(normal_store),
            None => None,
        };
        // Another request may have chosen while this one learned: the first choice stands.
        if let Some(current) = self.stores.lock().current.clone() {
            return current;
        }
        let chosen = learned.unwrap_or_else(|| FALLBACK_STORE.to_string());
        self.choose(&chosen);
        chosen
    }

    /// Every store this server has used, the current one first.
    pub fn stores_used(&self) -> Vec<String> {
        let stores = self.stores.lock();
        let mut all: Vec<String> = stores.current.iter().cloned().collect();
        all.extend(
            stores
                .used
                .iter()
                .filter(|s| Some(*s) != stores.current.as_ref())
                .cloned(),
        );
        all
    }

    fn choose(&self, store: &str) {
        let mut stores = self.stores.lock();
        if stores.current.as_deref() == Some(store) {
            return;
        }
        info!("Apple's catalog is asked in the '{store}' store");
        stores.current = Some(store.to_string());
        if !stores.used.iter().any(|s| s == store) {
            stores.used.push(store.to_string());
        }
        let file = StoresFile {
            store: stores.current.clone(),
            used: stores.used.clone(),
        };
        drop(stores);
        if let Some(path) = &self.stores_path
            && let Ok(text) = serde_json::to_string_pretty(&file)
            && let Err(e) = state_file::save_atomic(path, &text)
        {
            warn!("could not keep Apple's stores in {}: {e}", path.display());
        }
    }

    /// Apple's search for `term`: `entity` results, at most `limit` (200 at most).
    pub async fn search(
        &self,
        term: &str,
        entity: Entity,
        limit: u32,
        class: AppleClass,
    ) -> AppleAsked<Arc<Value>> {
        if dotnet::is_blank(term) || limit == 0 {
            return AppleAsked::NoData;
        }
        let address = |store: &str| {
            format!(
                "{}/search?term={}&entity={}&limit={}&country={store}",
                self.base,
                dotnet::escape_data_string(term.trim()),
                entity.word(),
                limit.min(200)
            )
        };
        self.ask_in_store(address, class, "search").await
    }

    /// Apple's lookup of `ids` (one or several), with their `entity` results when given. An id
    /// lookup that finds nothing in the current store asks every other store this server has used,
    /// then `us`, before it is "nothing".
    pub async fn lookup(
        &self,
        ids: &[u64],
        entity: Option<Entity>,
        limit: u32,
        class: AppleClass,
    ) -> AppleAsked<Arc<Value>> {
        if ids.is_empty() {
            return AppleAsked::NoData;
        }
        let joined = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let address = |store: &str| {
            let mut url = format!("{}/lookup?id={joined}", self.base);
            if let Some(entity) = entity {
                url.push_str(&format!("&entity={}", entity.word()));
            }
            url.push_str(&format!("&limit={}&country={store}", limit.clamp(1, 300)));
            url
        };
        let first = self.ask_in_store(&address, class, "lookup").await;
        if !matches!(first, AppleAsked::NoData) {
            return first;
        }
        let current = self.store().await;
        let mut others: Vec<String> = self.stores_used().into_iter().filter(|s| *s != current).collect();
        if current != FALLBACK_STORE && !others.iter().any(|s| s == FALLBACK_STORE) {
            others.push(FALLBACK_STORE.to_string());
        }
        for store in others {
            match self.ask(&address(&store), class, "lookup", ANSWER_TTL).await {
                Flight::Answer(answer) => {
                    debug!("Apple found {joined} in the '{store}' store, not '{current}'");
                    return AppleAsked::Answer(answer);
                }
                Flight::NoData | Flight::NoSuchStore => {}
                Flight::Busy | Flight::Refused => return AppleAsked::Busy,
            }
        }
        AppleAsked::NoData
    }

    /// The song ids of Apple's chart of the most played songs in the store, in order.
    pub async fn chart_ids(&self, limit: u32, class: AppleClass) -> AppleAsked<Vec<u64>> {
        let store = self.store().await;
        let url = format!(
            "{}/api/v2/{store}/music/most-played/{}/songs.json",
            self.feed,
            limit.clamp(1, 100)
        );
        match self.ask(&url, class, "chart", CHART_TTL).await {
            Flight::Answer(feed) => match apple_as_deezer::chart_ids(&feed) {
                apple_as_deezer::AppleRead::Read(ids) if ids.is_empty() => AppleAsked::NoData,
                apple_as_deezer::AppleRead::Read(ids) => AppleAsked::Answer(ids),
                apple_as_deezer::AppleRead::Unreadable => {
                    self.warn_shape("chart");
                    AppleAsked::Busy
                }
            },
            Flight::NoData => AppleAsked::NoData,
            Flight::Busy | Flight::Refused | Flight::NoSuchStore => AppleAsked::Busy,
        }
    }

    async fn ask_in_store(
        &self,
        address: impl Fn(&str) -> String,
        class: AppleClass,
        kind: &'static str,
    ) -> AppleAsked<Arc<Value>> {
        let store = self.store().await;
        let mut flight = self.ask(&address(&store), class, kind, ANSWER_TTL).await;
        if matches!(flight, Flight::NoSuchStore) && store != FALLBACK_STORE {
            warn!("Apple has no '{store}' store; Octo asks the '{FALLBACK_STORE}' store from now on");
            self.choose(FALLBACK_STORE);
            flight = self.ask(&address(FALLBACK_STORE), class, kind, ANSWER_TTL).await;
        }
        match flight {
            Flight::Answer(answer) => AppleAsked::Answer(answer),
            Flight::NoData => AppleAsked::NoData,
            Flight::Busy | Flight::Refused | Flight::NoSuchStore => AppleAsked::Busy,
        }
    }

    /// One address, from what is kept, else joined with a request already running for it (raising
    /// that request to this class), else asked. A refused turn is never a joiner's answer: the
    /// joiner asks once itself.
    async fn ask(&self, url: &str, class: AppleClass, kind: &'static str, ttl: Duration) -> Flight {
        for _ in 0..2 {
            if let Some(kept) = self.kept.get(url) {
                return kept.map_or(Flight::NoData, Flight::Answer);
            }
            let (outcome, joined) = self.join_or_start(url, class, kind, ttl);
            let flight = outcome.await;
            if joined && matches!(flight, Flight::Refused) {
                continue;
            }
            return match flight {
                Flight::Refused => Flight::Busy,
                other => other,
            };
        }
        Flight::Busy
    }

    fn join_or_start(
        &self,
        url: &str,
        class: AppleClass,
        kind: &'static str,
        ttl: Duration,
    ) -> (Shared<BoxFuture<'static, Flight>>, bool) {
        let mut running = self.running.lock();
        if let Some(flight) = running.get(url) {
            flight.ticket.raise(class);
            return (flight.outcome.clone(), true);
        }
        let ticket = Arc::new(self.pace.ticket(class));
        let fetch = self.fetch(url, kind);
        let running_map = Arc::clone(&self.running);
        let address = url.to_string();
        let kept = Arc::clone(&self.kept);
        let turn = Arc::clone(&ticket);
        let task = tokio::spawn(async move {
            let flight = fetch.after_turn(&turn).await;
            match &flight {
                Flight::Answer(answer) => keep(&kept, &address, Some(Arc::clone(answer)), ttl),
                Flight::NoData => keep(&kept, &address, None, NO_DATA_TTL),
                _ => {}
            }
            running_map.lock().remove(&address);
            flight
        });
        let outcome = async move { task.await.unwrap_or(Flight::Busy) }.boxed().shared();
        running.insert(
            url.to_string(),
            Running {
                outcome: outcome.clone(),
                ticket,
            },
        );
        (outcome, false)
    }

    fn fetch(&self, url: &str, kind: &'static str) -> Fetch {
        Fetch {
            client: self.client.clone(),
            pace: Arc::clone(&self.pace),
            warned: Arc::clone(&self.warned),
            url: url.to_string(),
            kind,
        }
    }

    /// The tracklists of several albums in one lookup: a request joins the lookup gathering for
    /// Apple's next turn while it has room for another album ([`BATCH_ALBUMS`]), raising that turn
    /// to its class; otherwise it starts the next gathering. The answer holds every album of the
    /// lookup that Apple has in the store; one it lacks is asked alone ([`Self::lookup`], which
    /// tries the other stores).
    pub async fn tracklists(&self, id: u64, class: AppleClass) -> AppleAsked<Arc<Value>> {
        let store = self.store().await;
        let outcome = {
            let mut gathering = self.gathering.lock();
            let room = self.batch_albums;
            let joinable = gathering.as_ref().is_some_and(|open| {
                let ids = open.ids.lock();
                ids.len() < room || ids.contains(&id)
            });
            match gathering.as_mut() {
                Some(open) if joinable => {
                    let mut ids = open.ids.lock();
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                    drop(ids);
                    open.ticket.raise(class);
                    open.outcome.clone()
                }
                _ => {
                    let ids = Arc::new(Mutex::new(vec![id]));
                    let ticket = Arc::new(self.pace.ticket(class));
                    let (turn, members, gathering_slot) =
                        (Arc::clone(&ticket), Arc::clone(&ids), Arc::clone(&self.gathering));
                    let (client, pace, warned, base) = (
                        self.client.clone(),
                        Arc::clone(&self.pace),
                        Arc::clone(&self.warned),
                        self.base.clone(),
                    );
                    let task = tokio::spawn(async move {
                        if let Err(Refused) = turn.wait().await {
                            close_gathering(&gathering_slot, &turn);
                            return Flight::Refused;
                        }
                        // The turn has come: no one joins from here on.
                        close_gathering(&gathering_slot, &turn);
                        let ids = members.lock().clone();
                        let joined = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
                        let url = format!(
                            "{base}/lookup?id={joined}&entity=song&limit={SONGS_PER_ALBUM}&country={store}"
                        );
                        Fetch {
                            client,
                            pace,
                            warned,
                            url,
                            kind: "lookup",
                        }
                        .send()
                        .await
                    });
                    let outcome = async move { task.await.unwrap_or(Flight::Busy) }.boxed().shared();
                    *gathering = Some(Gathering {
                        ids,
                        ticket,
                        outcome: outcome.clone(),
                    });
                    outcome
                }
            }
        };
        match outcome.await {
            Flight::Answer(answer) => AppleAsked::Answer(answer),
            // Nothing in this store (or no such store): asked alone, through every store used.
            Flight::NoData | Flight::NoSuchStore => self.lookup(&[id], Some(Entity::Song), 300, class).await,
            // Apple did not answer: no one asks again on its own while it is busy.
            Flight::Busy | Flight::Refused => AppleAsked::Busy,
        }
    }

    fn warn_shape(&self, kind: &'static str) {
        warn_once(&self.warned, kind);
    }

    /// Warns once per kind of answer that Apple's answer could not be read.
    pub fn answer_unreadable(&self, kind: &'static str) {
        self.warn_shape(kind);
    }
}

/// Keeps an answer (`None`: Apple has nothing), weighed by its size in KiB.
fn keep(kept: &MemoryCache<Option<Arc<Value>>>, url: &str, answer: Option<Arc<Value>>, ttl: Duration) {
    let weight = answer
        .as_ref()
        .map_or(1, |answer| answer.to_string().len() as u64 / 1024 + 1);
    kept.set(url, answer, weight, ttl);
}

struct Fetch {
    client: reqwest::Client,
    pace: Arc<ApplePace>,
    warned: Arc<Mutex<HashSet<&'static str>>>,
    url: String,
    kind: &'static str,
}

impl Fetch {
    /// Waits for `ticket`'s turn, then sends.
    async fn after_turn(&self, ticket: &Ticket) -> Flight {
        if let Err(Refused) = ticket.wait().await {
            return Flight::Refused;
        }
        self.send().await
    }

    /// Sends now: the turn is already taken.
    async fn send(&self) -> Flight {
        let response = match self.client.get(&self.url).send().await {
            Ok(response) => response,
            Err(e) => {
                debug!("Apple {} {} did not answer: {e}", self.kind, self.url);
                return Flight::Busy;
            }
        };
        let status = response.status();
        if matches!(status, StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS) {
            self.pace.refused_by_apple();
            info!(
                "Apple asked Octo to slow down; one request every {} s at least",
                self.pace.backoff().as_secs_f64()
            );
            return Flight::Busy;
        }
        self.pace.answered_by_apple();
        let Ok(body) = response.bytes().await else {
            return Flight::Busy;
        };
        if status == StatusCode::BAD_REQUEST && String::from_utf8_lossy(&body).contains("[country]") {
            return Flight::NoSuchStore;
        }
        if !status.is_success() {
            debug!(
                "Apple {} {} answered HTTP {}",
                self.kind,
                self.url,
                status.as_u16()
            );
            return Flight::Busy;
        }
        let Ok(doc) = serde_json::from_slice::<Value>(&body) else {
            return Flight::Busy;
        };
        match read_shape(&doc, self.kind) {
            Shape::Items(0) => Flight::NoData,
            Shape::Items(_) => Flight::Answer(Arc::new(doc)),
            Shape::Changed => {
                warn_once(&self.warned, self.kind);
                Flight::Busy
            }
        }
    }
}

/// Ends a gathering when its turn has come (or been refused): the slot is cleared only when it
/// still holds this gathering.
fn close_gathering(slot: &Mutex<Option<Gathering>>, turn: &Arc<Ticket>) {
    let mut slot = slot.lock();
    if slot.as_ref().is_some_and(|open| Arc::ptr_eq(&open.ticket, turn)) {
        *slot = None;
    }
}

fn warn_once(warned: &Mutex<HashSet<&'static str>>, kind: &'static str) {
    if warned.lock().insert(kind) {
        warn!("Apple's {kind} answer has changed shape; Octo reads it as no answer");
    }
}

enum Shape {
    Items(usize),
    Changed,
}

/// How many items an answer holds, or that it no longer reads as Apple's answers did: a search or
/// lookup's `results` (each with a `wrapperType`), a chart's `feed.results`.
fn read_shape(doc: &Value, kind: &str) -> Shape {
    if kind == "chart" {
        return match doc.pointer("/feed/results").and_then(Value::as_array) {
            Some(rows) => Shape::Items(rows.len()),
            None => Shape::Changed,
        };
    }
    let Some(results) = doc.get("results").and_then(Value::as_array) else {
        return Shape::Changed;
    };
    if !results.is_empty()
        && !results
            .iter()
            .any(|r| r.get("wrapperType").and_then(Value::as_str).is_some())
    {
        return Shape::Changed;
    }
    Shape::Items(results.len())
}

/// A two-letter store code in lowercase, or None.
fn normal_store(code: String) -> Option<String> {
    let code = code.trim().to_ascii_lowercase();
    (code.len() == 2 && code.chars().all(|c| c.is_ascii_lowercase())).then_some(code)
}

#[cfg(test)]
#[path = "apple_catalog_tests.rs"]
mod tests;
