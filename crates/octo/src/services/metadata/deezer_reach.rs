//! Whether Deezer serves this server: Open, Closed (Deezer does not serve the server's country and
//! hides its catalog searches), or Unreachable (no answer at all), learned from Deezer's own
//! `/infos`, from its catalog search answers and from requests that never got an answer. Never on a
//! timer: every read of `/infos` is started by a request, or by the server starting. A read someone
//! waits on, and the read at start, go on Deezer's interactive lane; a read nothing waits on (a
//! page answered from Apple asking whether Deezer serves this server again) goes on its background
//! lane, on the first such page after the reach changed and then on every
//! [`PAGES_PER_REREAD`]th.
//!
//! A change between Open and Closed or Unreachable (the last *settled* state; Unknown never
//! settles) is a catalog switch: the catalog generation goes up, and every cache key and in-flight
//! join key built after it differs from the ones before, so nothing computed under one catalog is
//! read under the other.

use std::sync::{Arc, OnceLock};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::services::apple::ApplePace;

/// Network failures in a row that make Deezer unreachable.
pub const FAILURES_TO_UNREACHABLE: u32 = 3;

/// Pages answered from Apple between two reads of `/infos` nothing waits on.
pub const PAGES_PER_REREAD: u32 = 20;

/// What Deezer's reach is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    Unknown,
    /// Deezer serves this server.
    Open {
        country_iso: String,
        country: String,
    },
    /// Deezer does not serve this server's country and hides its catalog searches.
    Closed {
        country_iso: String,
        country: String,
    },
    /// Deezer did not answer at all.
    Unreachable {
        error: String,
    },
}

impl Reach {
    /// Whether Apple answers Deezer's catalog searches.
    pub fn stands_in(&self) -> bool {
        matches!(self, Self::Closed { .. } | Self::Unreachable { .. })
    }

    fn settled(&self) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::Open { .. } => Some(false),
            Self::Closed { .. } | Self::Unreachable { .. } => Some(true),
        }
    }
}

/// What happened to one request the door sent to Deezer's API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Deezer answered with this HTTP status.
    Answered(u16),
    /// The request was sent and nothing answered: a connect error, a connect timeout or the
    /// request's own timeout.
    NetworkFailed(String),
    /// The request never left: our own limiter's queue was full, or its budget ran out while it
    /// waited for a permit or an Apple turn. Says nothing about Deezer.
    NotSent,
}

/// What `/infos` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Infos {
    Open {
        country_iso: String,
        country: String,
    },
    Closed {
        country_iso: String,
        country: String,
    },
    /// No answer: the call failed or its answer could not be read.
    Silent,
    /// The read never left: our own limiter refused it, or its budget ran out first.
    NotSent,
}

impl Infos {
    /// `/infos`' answer document.
    pub fn read(doc: &Value) -> Self {
        let open = doc.get("open").and_then(Value::as_bool);
        let country_iso = doc
            .get("country_iso")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let country = doc
            .get("country")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        match open {
            Some(true) => Self::Open { country_iso, country },
            Some(false) => Self::Closed { country_iso, country },
            None => Self::Silent,
        }
    }
}

/// Asks Deezer's `/infos` through the door.
#[async_trait::async_trait]
pub trait InfosReader: Send + Sync {
    /// `/infos`' answer, read on Deezer's background lane or its interactive one.
    async fn read(&self, background: bool) -> Infos;
}

/// The reach as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachStatus {
    pub reach: Reach,
    pub generation: u64,
}

struct State {
    reach: Reach,
    failures: u32,
    /// Bumped by every observation; an `/infos` answer from a read started before the last
    /// applied observation is dropped.
    seen: u64,
    settled: Option<bool>,
    generation: u64,
    /// No read of `/infos` has been sent yet: the next page answered from Apple starts one.
    read_needed: bool,
    /// The reach changed since the last read nothing waits on started.
    changed: bool,
    /// Pages answered from Apple since the last read nothing waits on started.
    pages: u32,
}

/// A read of `/infos` in flight, joined by the reads asked for while it runs.
struct Reading {
    id: u64,
    background: bool,
    read: Shared<BoxFuture<'static, Infos>>,
}

/// The reach. One per server.
pub struct DeezerReach {
    state: Mutex<State>,
    switches: watch::Sender<u64>,
    reader: OnceLock<Arc<dyn InfosReader>>,
    reading: Mutex<Option<Reading>>,
    reads: std::sync::atomic::AtomicU64,
    pace: Arc<ApplePace>,
}

impl DeezerReach {
    pub fn new(pace: Arc<ApplePace>) -> Self {
        let (switches, _) = watch::channel(0);
        Self {
            state: Mutex::new(State {
                reach: Reach::Unknown,
                failures: 0,
                seen: 0,
                settled: None,
                generation: 0,
                read_needed: true,
                changed: false,
                pages: 0,
            }),
            switches,
            reader: OnceLock::new(),
            reading: Mutex::new(None),
            reads: std::sync::atomic::AtomicU64::new(0),
            pace,
        }
    }

    /// How `/infos` is read; set once, before the first request.
    pub fn set_reader(&self, reader: Arc<dyn InfosReader>) {
        let _ = self.reader.set(reader);
    }

    pub fn reach(&self) -> Reach {
        self.state.lock().reach.clone()
    }

    /// Whether Apple answers Deezer's catalog searches now.
    pub fn stands_in(&self) -> bool {
        self.state.lock().reach.stands_in()
    }

    /// The catalog generation: read when a request starts, it keys what the request keeps.
    pub fn generation(&self) -> u64 {
        self.state.lock().generation
    }

    /// Each catalog switch's new generation.
    pub fn switches(&self) -> watch::Receiver<u64> {
        self.switches.subscribe()
    }

    pub fn status(&self) -> ReachStatus {
        let state = self.state.lock();
        ReachStatus {
            reach: state.reach.clone(),
            generation: state.generation,
        }
    }

    /// What happened to a request the door sent to Deezer's API.
    pub fn observe(self: &Arc<Self>, outcome: &Outcome) {
        let start_read = {
            let mut state = self.state.lock();
            match outcome {
                Outcome::NotSent => false,
                Outcome::Answered(_) => {
                    state.failures = 0;
                    if matches!(state.reach, Reach::Unreachable { .. }) {
                        state.seen += 1;
                        state.changed = true;
                        self.set(&mut state, Reach::Unknown);
                        true
                    } else {
                        false
                    }
                }
                Outcome::NetworkFailed(error) => {
                    state.failures += 1;
                    if state.failures >= FAILURES_TO_UNREACHABLE
                        && !matches!(state.reach, Reach::Unreachable { .. })
                    {
                        state.seen += 1;
                        state.changed = true;
                        self.set(&mut state, Reach::Unreachable { error: error.clone() });
                    }
                    false
                }
            }
        };
        if start_read {
            self.start_read();
        }
    }

    /// A catalog search answered at least one row: Deezer serves this server.
    pub fn observe_rows(&self) {
        let mut state = self.state.lock();
        if matches!(state.reach, Reach::Open { .. }) {
            return;
        }
        state.seen += 1;
        state.changed = true;
        let (country_iso, country) = match &state.reach {
            Reach::Closed { country_iso, country } => (country_iso.clone(), country.clone()),
            _ => (String::new(), String::new()),
        };
        self.set(&mut state, Reach::Open { country_iso, country });
    }

    /// A page someone opened was answered from Apple because of the reach: Deezer is asked again
    /// whether it serves this server (a VPN, a returning network), on the first such page after the
    /// reach changed or while no read has been sent yet, and then on every
    /// [`PAGES_PER_REREAD`]th.
    pub fn page_answered(self: &Arc<Self>) {
        let due = {
            let mut state = self.state.lock();
            state.pages += 1;
            let due = state.read_needed || state.changed || state.pages >= PAGES_PER_REREAD;
            if due {
                state.changed = false;
                state.pages = 0;
            }
            due
        };
        if due {
            self.start_read();
        }
    }

    /// `/infos`, read now on Deezer's interactive lane or joined with an interactive read already
    /// running, and applied unless an observation came after the read started.
    pub async fn read_infos(self: &Arc<Self>) -> Infos {
        self.read(false).await
    }

    /// The read at the server's start, on Deezer's interactive lane; nothing waits for it.
    pub fn read_at_start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.read(false).await;
        });
    }

    /// Starts a read of `/infos` on Deezer's background lane that nothing waits for (joined with
    /// any read already running).
    pub fn start_read(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.read(true).await;
        });
    }

    /// A read joins one in flight on the same lane or a better one: an interactive read never
    /// waits on a background one, which Deezer's limiter may refuse.
    async fn read(self: &Arc<Self>, background: bool) -> Infos {
        let read = {
            let mut reading = self.reading.lock();
            match reading.as_ref() {
                Some(running) if background || !running.background => running.read.clone(),
                _ => {
                    let Some(reader) = self.reader.get().cloned() else {
                        return Infos::Silent;
                    };
                    let started_at = self.state.lock().seen;
                    let id = self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let this = Arc::clone(self);
                    let read = async move {
                        let infos = reader.read(background).await;
                        this.apply_infos(&infos, started_at);
                        let mut reading = this.reading.lock();
                        if reading.as_ref().is_some_and(|running| running.id == id) {
                            *reading = None;
                        }
                        infos
                    }
                    .boxed()
                    .shared();
                    *reading = Some(Reading {
                        id,
                        background,
                        read: read.clone(),
                    });
                    read
                }
            }
        };
        read.await
    }

    fn apply_infos(&self, infos: &Infos, started_at: u64) {
        let mut state = self.state.lock();
        if !matches!(infos, Infos::NotSent) {
            state.read_needed = false;
        }
        if state.seen > started_at {
            return;
        }
        state.seen += 1;
        match infos {
            Infos::Open { country_iso, country } => self.set(
                &mut state,
                Reach::Open {
                    country_iso: country_iso.clone(),
                    country: country.clone(),
                },
            ),
            Infos::Closed { country_iso, country } => self.set(
                &mut state,
                Reach::Closed {
                    country_iso: country_iso.clone(),
                    country: country.clone(),
                },
            ),
            Infos::Silent | Infos::NotSent => {}
        }
    }

    fn set(&self, state: &mut State, reach: Reach) {
        if state.reach == reach {
            return;
        }
        match &reach {
            Reach::Unknown => info!("Deezer's reach is being checked again"),
            Reach::Open { country, .. } => info!(
                "Deezer answers this server's catalog searches{}",
                if country.is_empty() {
                    String::new()
                } else {
                    format!(" ({country})")
                }
            ),
            Reach::Closed { country, .. } => warn!(
                "Deezer does not serve {}; Octo answers its catalog searches from Apple Music's catalog",
                if country.is_empty() {
                    "this server's country"
                } else {
                    country
                }
            ),
            Reach::Unreachable { error } => warn!(
                "Octo cannot reach Deezer ({error}); it answers Deezer's catalog searches from Apple Music's catalog"
            ),
        }
        state.reach = reach;
        self.pace.set_strict(state.reach.stands_in());
        if let Some(settled) = state.reach.settled()
            && state.settled != Some(settled)
        {
            let was = state.settled.replace(settled);
            if was.is_some() {
                state.generation += 1;
                info!("Octo's catalog switched; catalog generation {}", state.generation);
                let _ = self.switches.send(state.generation);
            }
        }
    }
}

#[cfg(test)]
#[path = "deezer_reach_tests.rs"]
mod tests;
