//! Which chart songs a listener's library has, remembered between chart builds and requests.
//! A song the library answered with no match is not searched for again until the library
//! changes; a song it matched is searched again each time, so its stars, rating and play count
//! are current. The library's state is its last scan time plus the folders the listener can see.

use std::collections::{HashMap, VecDeque};

use parking_lot::Mutex;
use serde_json::Value;

use octo_subsonic::subsonic_request_parser::Parameters;

use crate::services::subsonic::SubsonicProxyService;

/// The most decisions kept for one listener.
const MAX_DECIDED: usize = 8192;

/// The sign-in a library read carries, and nothing else of the caller's request.
const SIGN_IN_KEYS: [&str; 8] = ["u", "p", "t", "s", "apiKey", "jwt", "v", "c"];

/// Each listener's decisions about the songs charts asked their library for.
pub struct LibraryMatchCache {
    by_user: Mutex<HashMap<String, UserMatches>>,
    cap: usize,
}

struct UserMatches {
    /// The library state these decisions were made under.
    key: String,
    decided: Decided,
}

/// Query to the library song id it matched, or none; the oldest entry goes first past the cap.
struct Decided {
    answers: HashMap<String, Option<String>>,
    order: VecDeque<String>,
}

impl Decided {
    fn new() -> Self {
        Decided {
            answers: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn put(&mut self, query: &str, answer: Option<String>, cap: usize) {
        if self.answers.insert(query.to_string(), answer).is_none() {
            self.order.push_back(query.to_string());
        }
        while self.answers.len() > cap {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.answers.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

impl Default for LibraryMatchCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LibraryMatchCache {
    pub fn new() -> Self {
        Self::with_cap(MAX_DECIDED)
    }

    /// A cache keeping at most `cap` decisions per listener.
    pub fn with_cap(cap: usize) -> Self {
        LibraryMatchCache {
            by_user: Mutex::new(HashMap::new()),
            cap,
        }
    }

    /// The listener's library state read now, as the listener, and the decisions that go with
    /// it. A listener with no name, or a library state that could not be read, gets a session
    /// that remembers nothing.
    pub async fn session(
        &self,
        proxy: &SubsonicProxyService,
        parameters: &Parameters,
        username: &str,
    ) -> MatchSession<'_> {
        let user = username.trim().to_lowercase();
        if user.is_empty() {
            return MatchSession::none();
        }
        match library_key(proxy, parameters).await {
            Some(key) => MatchSession {
                kept: Some(Kept {
                    cache: self,
                    user,
                    key,
                }),
            },
            None => MatchSession::none(),
        }
    }

    /// A session for a listener under a given library state, without reading the library.
    #[cfg(test)]
    pub fn session_for_tests(&self, username: &str, key: &str) -> MatchSession<'_> {
        MatchSession {
            kept: Some(Kept {
                cache: self,
                user: username.to_string(),
                key: key.to_string(),
            }),
        }
    }

    /// The listener's decisions under `key`, those made under another state dropped first.
    fn with_user<R>(&self, user: &str, key: &str, f: impl FnOnce(&mut Decided, usize) -> R) -> R {
        let mut by_user = self.by_user.lock();
        let mine = by_user.entry(user.to_string()).or_insert_with(|| UserMatches {
            key: key.to_string(),
            decided: Decided::new(),
        });
        if mine.key != key {
            mine.key = key.to_string();
            mine.decided = Decided::new();
        }
        f(&mut mine.decided, self.cap)
    }

    /// Drops everything remembered for a listener.
    pub fn forget(&self, username: &str) {
        self.by_user.lock().remove(&username.trim().to_lowercase());
    }

    /// How many decisions are kept for a listener.
    pub fn kept_for(&self, username: &str) -> usize {
        self.by_user
            .lock()
            .get(&username.trim().to_lowercase())
            .map_or(0, |mine| mine.decided.answers.len())
    }
}

/// One build's or request's view of a listener's decisions.
pub struct MatchSession<'a> {
    kept: Option<Kept<'a>>,
}

struct Kept<'a> {
    cache: &'a LibraryMatchCache,
    user: String,
    key: String,
}

impl MatchSession<'_> {
    /// A session that remembers nothing: every query is searched.
    pub fn none() -> MatchSession<'static> {
        MatchSession { kept: None }
    }

    /// Whether the library already answered that it has no such song.
    pub fn skips(&self, query: &str) -> bool {
        self.kept.as_ref().is_some_and(|kept| {
            kept.cache.with_user(&kept.user, &kept.key, |decided, _| {
                matches!(decided.answers.get(query), Some(None))
            })
        })
    }

    /// What the library answered for a query: the song it matched, or none.
    pub fn record(&self, query: &str, matched: Option<&Value>) {
        let Some(kept) = &self.kept else {
            return;
        };
        let id = matched
            .and_then(|song| song.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        // A match without an id cannot be told from no match, so it is not remembered at all.
        if matched.is_some() && id.is_none() {
            return;
        }
        kept.cache.with_user(&kept.user, &kept.key, |decided, cap| {
            decided.put(query, id, cap);
        });
    }
}

/// The library's state as this caller sees it: its last scan and the folders the caller can
/// reach. None when either could not be read.
async fn library_key(proxy: &SubsonicProxyService, parameters: &Parameters) -> Option<String> {
    let mut sign_in: Parameters = parameters
        .iter()
        .filter(|(key, _)| SIGN_IN_KEYS.iter().any(|name| name.eq_ignore_ascii_case(key)))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    sign_in.insert("f".into(), "json".into());
    let (scan, folders) = tokio::join!(
        read(proxy, "rest/getScanStatus", &sign_in),
        read(proxy, "rest/getMusicFolders", &sign_in)
    );
    let last_scan = scan?
        .pointer("/scanStatus/lastScan")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let folders = folders?;
    let mut ids: Vec<String> = folders
        .pointer("/musicFolders/musicFolder")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|folder| match folder.get("id")? {
                    Value::String(id) => Some(id.clone()),
                    Value::Number(id) => Some(id.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    Some(format!("{last_scan}|{}", ids.join(",")))
}

/// The `subsonic-response` of one read, when Navidrome answered ok.
async fn read(proxy: &SubsonicProxyService, endpoint: &str, parameters: &Parameters) -> Option<Value> {
    let relay = proxy.relay_safe(endpoint, parameters).await?;
    let doc: Value = serde_json::from_slice(&relay.body).ok()?;
    let response = doc.get("subsonic-response")?;
    (response.get("status").and_then(Value::as_str) == Some("ok")).then(|| response.clone())
}

#[cfg(test)]
#[path = "library_match_cache_tests.rs"]
mod tests;
