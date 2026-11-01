//! Mornye's check, as Octo keeps it: one cached read of Mornye's signed sessions and download
//! services, the episodes a push is sent for, and the songs Mornye refused while its check was
//! paused, which "Pass Mornye's check" asks for again. The rules live in
//! `octo_core::spotiflac::check`; this is the asking and the keeping.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use octo_core::spotiflac::check::{
    CheckPause, CheckRow, Episodes, Reading, ServiceRow, names_list, names_missing_utilities, paused_for,
    until_words, usable,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::Connection;
use crate::services::state_file;

/// How long a read of Mornye's check stays fresh, and how long each of its calls may take.
#[derive(Debug, Clone, Copy)]
pub struct CheckTimes {
    pub fresh_for: Duration,
    pub call_bound: Duration,
}

impl Default for CheckTimes {
    fn default() -> Self {
        CheckTimes {
            fresh_for: Duration::from_secs(60),
            call_bound: Duration::from_secs(5),
        }
    }
}

/// One read of Mornye's check, for one connection.
#[derive(Debug, Clone)]
pub struct CheckView {
    pub connection: Connection,
    pub read_started: Instant,
    pub read_utc: DateTime<Utc>,
    pub rows: Reading<Vec<CheckRow>>,
    pub services: Reading<Vec<ServiceRow>>,
    /// A call answered that Mornye's utilities extension is missing.
    pub utilities_missing: bool,
    /// Mornye answered the read (even "no such call"), so it is up.
    pub reached: bool,
}

impl CheckView {
    pub fn new(
        connection: Connection,
        read_started: Instant,
        read_utc: DateTime<Utc>,
        rows: Reading<Vec<CheckRow>>,
        services: Reading<Vec<ServiceRow>>,
    ) -> CheckView {
        let says_missing = |reading: Option<&str>| reading.is_some_and(names_missing_utilities);
        let utilities_missing = says_missing(cannot_tell(&rows)) || says_missing(cannot_tell(&services));
        let reached = utilities_missing || rows.known().is_some() || services.known().is_some();
        CheckView {
            connection,
            read_started,
            read_utc,
            rows,
            services,
            utilities_missing,
            reached,
        }
    }

    /// Whether Mornye can stand in for Soulseek: it answered, has its utilities extension, and
    /// offers a download service a song could use (an older Mornye that lists none is taken at
    /// its word).
    pub fn stands_in(&self, setting: &[String]) -> bool {
        if !self.reached || self.utilities_missing {
            return false;
        }
        match &self.services {
            Reading::Known(_) => !usable(setting, &self.services).is_empty(),
            Reading::CannotTell(_) => true,
        }
    }

    /// Whether Mornye's check keeps every extension a song could use, with the Services setting
    /// `setting`, at `now`.
    pub fn pause(&self, setting: &[String], now: DateTime<Utc>) -> Option<CheckPause> {
        paused_for(&self.rows, &usable(setting, &self.services), now)
    }

    /// The pause that keeps extension `provider` at `now`, from its own covering rows.
    pub fn pause_of(&self, provider: &str, now: DateTime<Utc>) -> Option<CheckPause> {
        let id = octo_core::spotiflac::check::extension_id(provider);
        let rows = self.rows.known()?;
        let covering: Vec<&CheckRow> = rows.iter().filter(|row| row.covers(&id, now)).collect();
        let until = covering.iter().filter_map(|row| row.paused_until).max()?;
        let mut labels: Vec<String> = covering.iter().map(|row| row.label.clone()).collect();
        labels.sort();
        labels.dedup();
        Some(CheckPause {
            until_utc: until,
            labels,
            holding: Vec::new(),
        })
    }
}

/// A wait read from Mornye's own words for extension `provider`, until `until`, beside `view`
/// (None when Octo could not read Mornye's check). It watches the rows holding the extension that
/// are not working in that read, so only a change after the wait (one of them passing) lets it
/// go early; none when every one already works (that read cannot tell a pass, so the wait lasts
/// until its own end). It shares every row holding the extension with other waits. With no row
/// holding it, the extension's id, which a later read's rows can still name.
pub fn words_pause(view: Option<&CheckView>, provider: &str, until: DateTime<Utc>) -> CheckPause {
    let id = octo_core::spotiflac::check::extension_id(provider);
    let holding: Vec<&CheckRow> = view
        .and_then(|view| view.rows.known())
        .map(|rows| rows.iter().filter(|row| row.extensions.contains(&id)).collect())
        .unwrap_or_default();
    if holding.is_empty() {
        return CheckPause {
            until_utc: until,
            labels: vec![id],
            holding: Vec::new(),
        };
    }
    let sorted = |mut labels: Vec<String>| {
        labels.sort();
        labels.dedup();
        labels
    };
    CheckPause {
        until_utc: until,
        labels: sorted(
            holding
                .iter()
                .filter(|row| !row.working())
                .map(|row| row.label.clone())
                .collect(),
        ),
        holding: sorted(holding.iter().map(|row| row.label.clone()).collect()),
    }
}

fn cannot_tell<T>(reading: &Reading<T>) -> Option<&str> {
    match reading {
        Reading::CannotTell(why) => Some(why),
        Reading::Known(_) => None,
    }
}

/// A song Mornye refused while its check was paused: what "Pass Mornye's check" asks Mornye for,
/// so Mornye opens its check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CheckSong {
    pub link: String,
    pub artist: String,
    pub title: String,
    #[serde(with = "octo_core::json::datetime::utc")]
    pub utc: DateTime<Utc>,
}

/// The most songs kept for "Pass Mornye's check".
pub const CHECK_SONGS_KEPT: usize = 5;

/// What `mornye-check.json` holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct CheckFile {
    #[serde(flatten)]
    pub episodes: Episodes,
    pub check_songs: Vec<CheckSong>,
    /// The song job a Pass run asked Mornye for, until the run has cancelled it. A run Octo
    /// stopped following (a restart) leaves it here, and the next look at Mornye cancels it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass_job: Option<PassJob>,
}

/// A Pass run's song job at Mornye: the address it was asked at, its id and its folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PassJob {
    pub url: String,
    pub id: String,
    pub dir: String,
}

/// `mornye-check.json`: Mornye's episodes and check songs. A missing, damaged or unwritable file
/// reads as empty and never stops a download.
pub struct MornyeCheckStore {
    path: Option<PathBuf>,
    state: Mutex<CheckFile>,
    /// The number of the latest changed state, counted under `state`'s lock.
    made: AtomicU64,
    /// The number of the newest state a write was tried for: one writer at a time, never older.
    written: Mutex<u64>,
}

impl MornyeCheckStore {
    pub fn new(path: Option<PathBuf>) -> MornyeCheckStore {
        let state = path
            .as_deref()
            .and_then(|path| state_file::read_text(path).ok().flatten())
            .and_then(|text| serde_json::from_str::<CheckFile>(&text).ok())
            .unwrap_or_default();
        MornyeCheckStore {
            path,
            state: Mutex::new(state),
            made: AtomicU64::new(0),
            written: Mutex::new(0),
        }
    }

    pub fn snapshot(&self) -> CheckFile {
        self.state.lock().clone()
    }

    /// Changes the file's state and, when `change` says it changed, writes it after the state's
    /// lock is let go, so a download never waits on the disk.
    fn update<R>(&self, change: impl FnOnce(&mut CheckFile) -> (R, bool)) -> R {
        let (result, text) = {
            let mut state = self.state.lock();
            let (result, changed) = change(&mut state);
            let text = (changed && self.path.is_some()).then(|| {
                (
                    self.made.fetch_add(1, Ordering::SeqCst) + 1,
                    serde_json::to_string_pretty(&*state),
                )
            });
            (result, text)
        };
        if let (Some(path), Some((made, text))) = (&self.path, text) {
            self.write(path, made, text);
        }
        result
    }

    /// Writes state number `made`, unless a newer one was already tried.
    fn write(&self, path: &Path, made: u64, text: serde_json::Result<String>) {
        let mut written = self.written.lock();
        if made <= *written {
            return;
        }
        *written = made;
        match text {
            Ok(text) => {
                if let Err(e) = state_file::save_atomic(path, &text) {
                    debug!("Could not write {}: {e}", path.display());
                }
            }
            Err(e) => debug!("Could not write Mornye's check: {e}"),
        }
    }

    /// Takes in a read of Mornye at `url`: the labels of the episodes it opened.
    pub fn note(&self, url: &str, rows: &[CheckRow], now: DateTime<Utc>) -> Vec<String> {
        self.update(|state| {
            // A first read keeps the songs; only songs refused at another address go.
            let url_changed = !state.episodes.url.is_empty() && state.episodes.url != url;
            let (opened, changed) = state.episodes.note(url, rows, now);
            if url_changed {
                state.check_songs.clear();
            }
            (opened, changed)
        })
    }

    /// Mornye brought a song through `provider`, at `link`: its episodes are over, and the song
    /// is no longer one to ask for again.
    pub fn brought(&self, provider: Option<&str>, link: &str) {
        self.update(|state| {
            let mut changed = provider
                .map(octo_core::spotiflac::check::extension_id)
                .is_some_and(|id| state.episodes.brought_through(&id));
            let before = state.check_songs.len();
            state.check_songs.retain(|song| song.link != link);
            changed |= state.check_songs.len() != before;
            ((), changed)
        })
    }

    /// Keeps a song Mornye refused while its check was paused, newest first.
    pub fn remember(&self, song: CheckSong) {
        self.update(|state| {
            if state
                .check_songs
                .first()
                .is_some_and(|first| first.link == song.link)
            {
                return ((), false);
            }
            state.check_songs.retain(|kept| kept.link != song.link);
            state.check_songs.insert(0, song);
            state.check_songs.truncate(CHECK_SONGS_KEPT);
            ((), true)
        })
    }

    /// Since when the check of any of `labels` has not passed.
    pub fn since(&self, labels: &[String]) -> Option<DateTime<Utc>> {
        self.state.lock().episodes.since(labels)
    }

    /// The song job a Pass run asked Mornye for and has not cancelled yet, if any.
    pub fn pass_job(&self) -> Option<PassJob> {
        self.state.lock().pass_job.clone()
    }

    /// Keeps the song job a Pass run just asked Mornye for.
    pub fn keep_pass_job(&self, job: PassJob) {
        self.update(|state| {
            let changed = state.pass_job.as_ref() != Some(&job);
            state.pass_job = Some(job);
            ((), changed)
        })
    }

    /// Forgets `job` once it is cancelled, unless another has been kept since.
    pub fn forget_pass_job(&self, job: &PassJob) {
        self.update(|state| {
            let same = state.pass_job.as_ref() == Some(job);
            if same {
                state.pass_job = None;
            }
            ((), same)
        })
    }
}

/// Mornye's check for one client: the stored file, the cached read and the push.
pub struct MornyeCheck {
    pub store: Arc<MornyeCheckStore>,
    pub times: CheckTimes,
    view: Mutex<Option<Arc<CheckView>>>,
    refresh: tokio::sync::Mutex<()>,
    push: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl MornyeCheck {
    pub fn new(
        store: Arc<MornyeCheckStore>,
        times: CheckTimes,
        push: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> MornyeCheck {
        MornyeCheck {
            store,
            times,
            view: Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
            push,
        }
    }

    /// One that keeps nothing on disk and pushes nothing.
    pub fn in_memory() -> MornyeCheck {
        MornyeCheck::new(Arc::new(MornyeCheckStore::new(None)), CheckTimes::default(), None)
    }

    /// The cached view, when it is for `connection` and fits: started after `after`, or else
    /// still fresh.
    pub(super) fn cached(&self, connection: &Connection, after: Option<Instant>) -> Option<Arc<CheckView>> {
        let view = self.view.lock().clone()?;
        let fits = view.connection == *connection
            && match after {
                Some(after) => view.read_started >= after,
                None => view.read_started.elapsed() < self.times.fresh_for,
            };
        fits.then_some(view)
    }

    pub(super) fn refresh_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.refresh
    }

    /// Keeps `view` unless a read that started later is already kept. Answers whether it kept it.
    pub(super) fn keep(&self, view: &Arc<CheckView>) -> bool {
        let mut slot = self.view.lock();
        let newer = slot
            .as_ref()
            .is_some_and(|kept| kept.connection == view.connection && kept.read_started > view.read_started);
        if newer {
            return false;
        }
        *slot = Some(Arc::clone(view));
        true
    }

    /// Takes in a kept read: episodes, and one push when it opened any.
    pub(super) fn note(&self, view: &CheckView, words: impl FnOnce(&[String], &CheckView) -> String) {
        let Reading::Known(rows) = &view.rows else {
            return;
        };
        let opened = self.store.note(&view.connection.url, rows, view.read_utc);
        if opened.is_empty() {
            return;
        }
        if let Some(push) = &self.push {
            push(words(&opened, view));
        }
    }
}

/// The words of the push: which checks, what happens meanwhile, and where to pass it.
/// What Octo does for a song while Mornye's check is paused, which the push says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meanwhile {
    pub soulseek_on: bool,
    /// The download source falls back to an MP3 from YouTube.
    pub then_mp3: bool,
}

impl Meanwhile {
    pub fn of(all: &octo_core::settings::AppSettings) -> Meanwhile {
        Meanwhile {
            soulseek_on: all.soulseek.is_on(),
            then_mp3: all.subsonic.download_source
                == octo_core::settings::DownloadSource::SoulseekThenYouTube,
        }
    }
}

pub fn push_words(
    labels: &[String],
    until: Option<DateTime<Utc>>,
    meanwhile: Meanwhile,
    public_url: &str,
    now: DateTime<Utc>,
) -> String {
    let meanwhile = match meanwhile {
        Meanwhile {
            soulseek_on: true,
            then_mp3: true,
        } => "so Octo asks Soulseek, then YouTube, in the meantime",
        Meanwhile {
            soulseek_on: true,
            then_mp3: false,
        } => "so Octo asks Soulseek in the meantime",
        Meanwhile {
            soulseek_on: false,
            then_mp3: true,
        } => "so Octo gets MP3s from YouTube in the meantime",
        Meanwhile {
            soulseek_on: false,
            then_mp3: false,
        } => "so songs only Mornye can bring fail until it passes",
    };
    let next = until
        .map(|until| format!(" Mornye allows its next try at {}.", until_words(until, now)))
        .unwrap_or_default();
    let public_url = public_url.trim().trim_end_matches('/');
    let pass = if public_url.is_empty() {
        "Pass it on the Mornye page of Octo's dashboard.".to_string()
    } else {
        format!("Pass it on the dashboard: {public_url}/admin/#spotiflac")
    };
    format!(
        "Mornye could not pass the check for {}, {meanwhile}.{next} {pass}",
        names_list(labels)
    )
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;
