//! Mornye's check, as Octo reads it: the answers of Mornye's page about its signed sessions and
//! download services, which extensions a song could use, whether every one of them waits for
//! Mornye's check, the episodes a push is sent for, and the words a failed download is told in.
//! No I/O here; `octo::services::spotiflac::check` does the asking and keeps the state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{ExtensionRow, ServerInfo};

/// A call of Mornye's page (`POST /api/{method}`) answers `{"result": ...}`. `result` has no
/// default, so an answer without it does not parse and reads as "cannot tell".
#[derive(Debug, Clone, Deserialize)]
pub struct RpcAnswer<T> {
    pub result: T,
}

/// `get_signed_sessions`: one row per session file, and `error` when Mornye could not list them
/// (Mornye 1.1 without its utilities extension).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SessionsAnswer {
    pub sessions: Vec<SessionRow>,
    pub error: Option<String>,
}

/// One signed session. `auth_paused_s` is its gateway's pause, the same on every row of that
/// gateway; a row whose own session is `active` or `refresh_due` downloads through it anyway.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SessionRow {
    pub key: String,
    pub kind: String,
    pub label: String,
    pub extensions: Vec<String>,
    pub state: String,
    pub auth_paused_s: f64,
}

/// `get_download_services`: the download services Mornye can offer, each with the extensions
/// behind it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServicesAnswer {
    pub services: Vec<ServiceRow>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServiceRow {
    pub id: String,
    pub label: String,
    pub extensions: Vec<String>,
}

/// `POST /api/v1/library/scan`: how many song files Mornye found in the folder.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ScanAnswer {
    pub scanned: i64,
    pub unreadable: i64,
}

/// What a read of Mornye gave: an answer, or why Octo cannot tell.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading<T> {
    Known(T),
    CannotTell(String),
}

impl<T> Reading<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Reading::Known(value) => Some(value),
            Reading::CannotTell(_) => None,
        }
    }
}

/// The text Mornye 1.1 answers when its utilities extension is missing.
pub const UTILITIES_MISSING: &str = "utilities extension is not installed";

/// Whether Mornye's text says its utilities extension is missing.
pub fn names_missing_utilities(text: &str) -> bool {
    text.to_ascii_lowercase().contains(UTILITIES_MISSING)
}

/// An extension's id by Mornye's own rule: without a leading `ext:`, then without a trailing
/// `-web`, then without a trailing `-py`. Case is kept, as Mornye keeps it.
pub fn extension_id(name: &str) -> String {
    let name = name.trim();
    let name = name.strip_prefix("ext:").unwrap_or(name);
    let name = name.strip_suffix("-web").unwrap_or(name);
    let name = name.strip_suffix("-py").unwrap_or(name);
    name.to_string()
}

/// A session row as Octo keeps it: its extensions as ids, and its pause as a moment, fixed when
/// the row was read, so a later look never stretches it.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckRow {
    pub key: String,
    pub label: String,
    pub extensions: BTreeSet<String>,
    pub state: String,
    /// When the pause ends, for a row whose own session waits for the check; None otherwise.
    pub paused_until: Option<DateTime<Utc>>,
}

impl CheckRow {
    pub fn from_read(row: &SessionRow, read_utc: DateTime<Utc>) -> CheckRow {
        let waits = row.auth_paused_s > 0.0 && waits_for_check(&row.state);
        CheckRow {
            key: row.key.clone(),
            label: plain_dashes(row.label.trim()),
            extensions: row
                .extensions
                .iter()
                .map(|name| extension_id(name))
                .filter(|id| !id.is_empty())
                .collect(),
            state: row.state.clone(),
            paused_until: waits.then(|| read_utc + Duration::seconds(row.auth_paused_s.ceil() as i64)),
        }
    }

    /// Whether this row's pause still holds at `now`.
    pub fn paused_at(&self, now: DateTime<Utc>) -> bool {
        self.paused_until.is_some_and(|until| until > now)
    }

    /// Whether this row keeps extension `id` from Mornye at `now`.
    pub fn covers(&self, id: &str, now: DateTime<Utc>) -> bool {
        self.paused_at(now) && self.extensions.contains(id)
    }

    /// Whether the row's own session works.
    pub fn working(&self) -> bool {
        matches!(self.state.as_str(), "active" | "refresh_due")
    }
}

/// A session that must pass Mornye's check before it downloads.
fn waits_for_check(state: &str) -> bool {
    matches!(state, "unverified" | "expired")
}

/// The rows of a sessions answer, read at `read_utc`. An answer carrying `error` cannot tell.
pub fn check_rows(answer: &SessionsAnswer, read_utc: DateTime<Utc>) -> Reading<Vec<CheckRow>> {
    match answer.error.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        Some(error) => Reading::CannotTell(plain_dashes(error)),
        None => Reading::Known(
            answer
                .sessions
                .iter()
                .map(|row| CheckRow::from_read(row, read_utc))
                .collect(),
        ),
    }
}

/// The services of an answer. An answer carrying `error` cannot tell.
pub fn service_rows(answer: &ServicesAnswer) -> Reading<Vec<ServiceRow>> {
    match answer.error.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        Some(error) => Reading::CannotTell(plain_dashes(error)),
        None => Reading::Known(answer.services.clone()),
    }
}

/// The extensions Mornye could use for a song: with the Services setting blank, every extension
/// behind a download service; with it set, its entries that Mornye offers (Mornye drops the
/// rest). When the services cannot be read, the setting's entries alone. Empty: Octo cannot
/// tell, and asks Mornye.
pub fn usable(setting: &[String], services: &Reading<Vec<ServiceRow>>) -> BTreeSet<String> {
    let wanted: BTreeSet<String> = setting
        .iter()
        .map(|entry| extension_id(entry))
        .filter(|id| !id.is_empty())
        .collect();
    match services {
        Reading::Known(rows) => {
            let offered: BTreeSet<String> = rows
                .iter()
                .flat_map(|row| row.extensions.iter().map(|name| extension_id(name)))
                .filter(|id| !id.is_empty())
                .collect();
            if wanted.is_empty() {
                offered
            } else {
                wanted.intersection(&offered).cloned().collect()
            }
        }
        Reading::CannotTell(_) => wanted,
    }
}

/// Every usable extension waits for Mornye's check: the first moment one of them is free again,
/// and the checks that keep them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckPause {
    pub until_utc: DateTime<Utc>,
    pub labels: Vec<String>,
}

/// Whether Mornye's check keeps every usable extension at `now`. None when Octo cannot tell, when
/// no extension is usable, or when one of them is free.
pub fn paused_for(
    rows: &Reading<Vec<CheckRow>>,
    usable: &BTreeSet<String>,
    now: DateTime<Utc>,
) -> Option<CheckPause> {
    let Reading::Known(rows) = rows else {
        return None;
    };
    if usable.is_empty() {
        return None;
    }
    let mut until: Option<DateTime<Utc>> = None;
    let mut labels = BTreeSet::new();
    for id in usable {
        let covering: Vec<&CheckRow> = rows.iter().filter(|row| row.covers(id, now)).collect();
        let free_at = covering.iter().filter_map(|row| row.paused_until).max()?;
        until = Some(until.map_or(free_at, |earliest| earliest.min(free_at)));
        labels.extend(covering.iter().map(|row| row.label.clone()));
    }
    Some(CheckPause {
        until_utc: until?,
        labels: labels.into_iter().filter(|label| !label.is_empty()).collect(),
    })
}

/// The latest end among the rows paused at `now`, for a watcher to sleep to.
pub fn latest_pause_end(rows: &Reading<Vec<CheckRow>>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    rows.known()?
        .iter()
        .filter(|row| row.paused_at(now))
        .filter_map(|row| row.paused_until)
        .max()
}

/// One stretch of a gateway's check failing, opened by a read that shows it paused and closed by
/// work: a read showing its session working, or a song Mornye brought through one of its
/// extensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Episode {
    pub label: String,
    pub extensions: BTreeSet<String>,
    #[serde(with = "crate::json::datetime::utc")]
    pub opened_utc: DateTime<Utc>,
    #[serde(with = "crate::json::datetime::utc")]
    pub paused_until_utc: DateTime<Utc>,
}

/// Mornye forgets a gateway's count of failed tries this long after its last pause ended; a pause
/// after that is a new stretch, so it opens a new episode.
pub const MORNYE_FORGETS_AFTER_HOURS: i64 = 24;

/// How much later than the stored end a read's end must be to move it.
const EPISODE_END_SLACK_SECONDS: i64 = 5;

/// The episodes of one Mornye, by session row key, as `mornye-check.json` keeps them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Episodes {
    pub url: String,
    pub episodes: BTreeMap<String, Episode>,
}

impl Episodes {
    /// Takes in a read of Mornye at `url`. Answers the labels of the episodes it opened, which a
    /// push names, and whether anything changed. A read of another Mornye starts over.
    pub fn note(&mut self, url: &str, rows: &[CheckRow], now: DateTime<Utc>) -> (Vec<String>, bool) {
        let mut changed = false;
        if self.url != url {
            changed = true;
            self.url = url.to_string();
            self.episodes.clear();
        }
        let mut opened = BTreeSet::new();
        let present: BTreeSet<&str> = rows.iter().map(|row| row.key.as_str()).collect();
        let before = self.episodes.len();
        self.episodes.retain(|key, _| present.contains(key.as_str()));
        changed |= self.episodes.len() != before;
        for row in rows {
            if row.paused_at(now) {
                let end = row.paused_until.unwrap_or(now);
                let forgot = |episode: &Episode| {
                    now > episode.paused_until_utc + Duration::hours(MORNYE_FORGETS_AFTER_HOURS)
                };
                match self.episodes.get_mut(&row.key) {
                    Some(episode) if !forgot(episode) => {
                        // A later read of the same pause lands within a second of the stored end;
                        // only a longer pause moves it.
                        if end > episode.paused_until_utc + Duration::seconds(EPISODE_END_SLACK_SECONDS) {
                            episode.paused_until_utc = end;
                            changed = true;
                        }
                    }
                    _ => {
                        self.episodes.insert(
                            row.key.clone(),
                            Episode {
                                label: row.label.clone(),
                                extensions: row.extensions.clone(),
                                opened_utc: now,
                                paused_until_utc: end,
                            },
                        );
                        opened.insert(row.label.clone());
                        changed = true;
                    }
                }
            } else if row.working() && self.episodes.remove(&row.key).is_some() {
                changed = true;
            }
        }
        (
            opened.into_iter().filter(|label| !label.is_empty()).collect(),
            changed,
        )
    }

    /// Mornye brought a song through extension `id`: every episode that lists it is over.
    pub fn brought_through(&mut self, id: &str) -> bool {
        let before = self.episodes.len();
        self.episodes
            .retain(|_, episode| !episode.extensions.contains(id));
        self.episodes.len() != before
    }

    /// The earliest opening among the episodes that list one of `labels`: since when the check
    /// has not passed.
    pub fn since(&self, labels: &[String]) -> Option<DateTime<Utc>> {
        self.episodes
            .values()
            .filter(|episode| labels.contains(&episode.label))
            .map(|episode| episode.opened_utc)
            .min()
    }
}

/// A list of names for a sentence: "a", "a and b", "a, b and c".
pub fn names_list(names: &[String]) -> String {
    match names {
        [] => "its extensions".to_string(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A moment for a sentence, rounded up to the minute: "14:05 UTC" on `now`'s day (UTC), else
/// "Oct 31, 14:05 UTC".
pub fn until_words(until: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let rounded = if until.second() > 0 || until.nanosecond() > 0 {
        until
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(until)
            + Duration::minutes(1)
    } else {
        until
    };
    if rounded.date_naive() == now.date_naive() {
        rounded.format("%H:%M UTC").to_string()
    } else if rounded.year() == now.year() {
        rounded.format("%b %-d, %H:%M UTC").to_string()
    } else {
        rounded.format("%b %-d %Y, %H:%M UTC").to_string()
    }
}

static EM_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\x{2014}\s*").unwrap());
static EN_DASH_RANGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d)\s*\x{2013}\s*(\d)").unwrap());

/// Mornye's own text, as Octo shows it: an em dash becomes a comma, an en dash between numbers
/// becomes "to", any other en dash a hyphen.
pub fn plain_dashes(text: &str) -> String {
    let text = EM_DASH.replace_all(text, ", ");
    let text = EN_DASH_RANGE.replace_all(&text, "$1 to $2");
    text.replace('\u{2013}', "-")
}

static NEXT_TRY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)verification paused.*next try in (?:(\d+) h )?(\d+) min").unwrap());

/// When Mornye's own words about a paused check let it try again ("verification paused after a
/// failed attempt, next try in 1 h 05 min"), counted from `now`; None for other words.
pub fn next_try_from_words(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let found = NEXT_TRY.captures(text)?;
    let hours: i64 = found.get(1).map_or(Ok(0), |h| h.as_str().parse()).ok()?;
    let minutes: i64 = found.get(2)?.as_str().parse().ok()?;
    Some(now + Duration::minutes(hours * 60 + minutes))
}

/// The words of the gate: the sentence a row shows, then what the log adds.
pub fn gate_words(pause: &CheckPause, now: DateTime<Utc>) -> (String, String) {
    (
        format!(
            "Mornye could not pass its check for {}, so Octo did not ask it for this song.",
            names_list(&pause.labels)
        ),
        format!(
            "It allows its next try at {}; the next song after that lets it try.",
            until_words(pause.until_utc, now)
        ),
    )
}

/// What Octo knows about a Mornye job that failed.
#[derive(Debug, Clone, Copy, Default)]
pub struct FailureFacts<'a> {
    /// The job's own reason, as Mornye gave it.
    pub reason: &'a str,
    /// The extension the job failed in; None for none or `none`.
    pub provider: Option<&'a str>,
    /// That extension's health row after the failure.
    pub health: Option<&'a ExtensionRow>,
    /// Whether the health row's last error is this job's own.
    pub own_job: bool,
    /// The job's folder on Mornye's side.
    pub their_dir: &'a str,
    /// Whether a read of Mornye said its utilities extension is missing.
    pub utilities_missing: bool,
}

/// The words of a failed Mornye job: the sentence a row shows, and what the log adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureWords {
    pub shown: String,
    pub detail: Option<String>,
}

/// The extension a job item names, or None for `none` and blank.
pub fn named_provider(provider: &str) -> Option<&str> {
    let provider = provider.trim();
    (!provider.is_empty() && !provider.eq_ignore_ascii_case("none")).then_some(provider)
}

/// Why a Mornye job failed, in words. The first rule that fits wins.
pub fn failure_words(facts: &FailureFacts<'_>) -> FailureWords {
    let last_error = facts
        .health
        .filter(|row| row.last_outcome.eq_ignore_ascii_case("failure"))
        .map(|row| plain_dashes(row.last_error.trim()))
        .filter(|error| !error.is_empty());
    if facts.utilities_missing
        || names_missing_utilities(facts.reason)
        || last_error.as_deref().is_some_and(names_missing_utilities)
    {
        return FailureWords {
            shown: "Mornye needs its utilities extension, which your registry does not list.".into(),
            detail: Some(
                "Mornye 1.1 moved its search and its check into an extension named utilities. Use a registry \
                 that lists it, or set SPOTIFLAC_IMAGE back to Mornye 1.0.5."
                    .into(),
            ),
        };
    }
    let their_dir = facts.their_dir.trim().trim_end_matches('/');
    if facts.own_job
        && let Some(error) = &last_error
        && !their_dir.is_empty()
        && error.contains(their_dir)
        && is_write_refusal(error)
    {
        return FailureWords {
            shown: "Mornye could not write the song into Octo's music folder.".into(),
            detail: Some(format!("{error}. The Test on the Mornye page checks the folder.")),
        };
    }
    if let (Some(provider), Some(error)) = (facts.provider, &last_error) {
        let provider = plain_dashes(provider);
        return FailureWords {
            shown: if facts.own_job {
                format!("Mornye's {provider} extension could not get the song: {error}")
            } else {
                format!("Mornye's {provider} extension failed; its last try said: {error}")
            },
            detail: None,
        };
    }
    let reason = plain_dashes(facts.reason.trim());
    let provider = facts.provider.map(plain_dashes);
    let shown = match (reason.as_str(), provider) {
        ("source_not_supported", _) => {
            "Mornye's extensions cannot download this link from the catalog.".to_string()
        }
        ("provider_returned_no_result", Some(provider)) => {
            format!("Mornye's {provider} extension returned nothing for the song.")
        }
        ("provider_returned_no_result", None) => {
            "Mornye's extension returned nothing for the song.".to_string()
        }
        ("download_failed", Some(provider)) => {
            format!("Mornye's {provider} extension failed and gave no reason.")
        }
        ("download_failed" | "", None) => "Mornye failed and gave no reason.".to_string(),
        ("", Some(provider)) => format!("Mornye's {provider} extension failed and gave no reason."),
        (reason, _) => reason.to_string(),
    };
    FailureWords { shown, detail: None }
}

/// Whether an error says the operating system refused a write, in Python's words or Node's.
fn is_write_refusal(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "permission denied",
        "read-only file system",
        "no such file or directory",
    ]
    .iter()
    .any(|words| error.contains(words))
}

impl ServerInfo {
    /// Whether this is a Mornye older than 1.0.4, whose check cannot pass (pydoll 3).
    pub fn cannot_pass_check(&self) -> bool {
        if !self.name.eq_ignore_ascii_case("Mornye") {
            return false;
        }
        let parts: Vec<u32> = self
            .version
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map_while(|part| part.parse().ok())
            .collect();
        match parts.as_slice() {
            [major, minor, patch, ..] => (*major, *minor, *patch) < (1, 0, 4),
            [major, minor] => (*major, *minor) < (1, 0),
            _ => false,
        }
    }
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;
