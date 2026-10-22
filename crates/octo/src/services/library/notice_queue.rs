//! What Octo has asked each person about, and what they answered (`<config>/notice-queue.json`,
//! the one state file that writes its enums as names).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::{PathHelper, dotnet};
use octo_core::fingerprint::verification::{
    InconclusiveReason, VerificationResult, threshold_percent, whole_percent,
};
use octo_core::json::datetime;
use octo_core::models::domain::Song;
use octo_core::settings::NoticeKind;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::duplicate_scan_worker::{DuplicateGroup, LibraryTrack};
use crate::services::framework::DotnetDictionary;
use crate::services::state_file;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NoticeState {
    #[default]
    Waiting,
    Queued,
    Kept,
    Acted,
    Dismissed,
    Expired,
}

/// What raised a question. Download is 0 so every entry written before this loads as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NoticeOrigin {
    #[default]
    Download,
    LibrarySweep,
}

/// An enum `JsonStringEnumConverter` wrote by name: the members in declaration order.
pub(crate) trait NamedEnum: Copy + PartialEq + 'static {
    const ALL: &'static [Self];
    fn name(self) -> &'static str;
}

impl NamedEnum for NoticeState {
    const ALL: &'static [Self] = &[
        Self::Waiting,
        Self::Queued,
        Self::Kept,
        Self::Acted,
        Self::Dismissed,
        Self::Expired,
    ];
    fn name(self) -> &'static str {
        match self {
            Self::Waiting => "Waiting",
            Self::Queued => "Queued",
            Self::Kept => "Kept",
            Self::Acted => "Acted",
            Self::Dismissed => "Dismissed",
            Self::Expired => "Expired",
        }
    }
}

impl NamedEnum for NoticeOrigin {
    const ALL: &'static [Self] = &[Self::Download, Self::LibrarySweep];
    fn name(self) -> &'static str {
        match self {
            Self::Download => "Download",
            Self::LibrarySweep => "LibrarySweep",
        }
    }
}

impl NamedEnum for NoticeKind {
    const ALL: &'static [Self] = &[NoticeKind::Review, NoticeKind::Duplicates];
    fn name(self) -> &'static str {
        match self {
            NoticeKind::Review => "Review",
            NoticeKind::Duplicates => "Duplicates",
        }
    }
}

impl NamedEnum for InconclusiveReason {
    const ALL: &'static [Self] = &InconclusiveReason::ALL;
    fn name(self) -> &'static str {
        InconclusiveReason::name(self)
    }
}

/// `JsonStringEnumConverter` with no naming policy: written as the member name; read as a name
/// in any case, or as the member's number.
mod by_name {
    use serde::de::{Error, Visitor};
    use serde::{Deserializer, Serializer};

    use super::NamedEnum;

    pub fn serialize<S: Serializer, T: NamedEnum>(value: &T, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(value.name())
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: NamedEnum>(d: D) -> Result<T, D::Error> {
        struct V<T>(std::marker::PhantomData<T>);
        impl<T: NamedEnum> Visitor<'_> for V<T> {
            type Value = T;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an enum name or number")
            }
            fn visit_str<E: Error>(self, text: &str) -> Result<T, E> {
                let trimmed = text.trim();
                if let Some(found) = T::ALL
                    .iter()
                    .find(|member| octo_core::common::dotnet::eq_ignore_case(member.name(), trimmed))
                {
                    return Ok(*found);
                }
                match trimmed.parse::<i64>() {
                    Ok(number) => self.visit_i64(number),
                    Err(_) => Err(E::custom(format!("The JSON value '{text}' is not a member"))),
                }
            }
            fn visit_i64<E: Error>(self, number: i64) -> Result<T, E> {
                usize::try_from(number)
                    .ok()
                    .and_then(|i| T::ALL.get(i).copied())
                    .ok_or_else(|| E::custom(format!("no member is numbered {number}")))
            }
            fn visit_u64<E: Error>(self, number: u64) -> Result<T, E> {
                self.visit_i64(i64::try_from(number).unwrap_or(i64::MAX))
            }
        }
        d.deserialize_any(V(std::marker::PhantomData))
    }
}

/// One question Octo is asking one person about one track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NoticeEntry {
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub key: String,
    #[serde(default, with = "by_name")]
    pub kind: NoticeKind,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub username: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub local_path: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub artist: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub title: String,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub navidrome_id: Option<String>,
    #[serde(default)]
    pub group_key: Option<String>,
    #[serde(default)]
    pub order: i32,
    #[serde(default, with = "by_name")]
    pub state: NoticeState,

    /// What the dashboard shows as the reason Octo is asking.
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub reason: String,

    /// Why verification could not decide, which is what makes an answer submittable.
    #[serde(default, with = "by_name")]
    pub cause: InconclusiveReason,

    /// A library sweep question is never sent to AcoustID: keeping a file says it is fine to
    /// keep, not that its tags name the right recording.
    #[serde(default, with = "by_name")]
    pub origin: NoticeOrigin,

    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub duration_seconds: i32,
    #[serde(default)]
    pub candidate_recording_id: Option<String>,
    #[serde(default)]
    pub file_format: Option<String>,
    #[serde(default)]
    pub submitted: bool,
    #[serde(default)]
    pub lookup_attempts: i32,
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub next_lookup_utc: DateTime<Utc>,
    /// The current time when the entry is made, and when a file leaves it out.
    #[serde(default = "Utc::now", with = "datetime::utc")]
    pub created_utc: DateTime<Utc>,
    #[serde(default, with = "datetime::utc_option")]
    pub queued_utc: Option<DateTime<Utc>>,
    #[serde(default, with = "datetime::utc_option")]
    pub resolved_utc: Option<DateTime<Utc>>,

    /// How sure AcoustID was, as a whole percent rounded down: of the recording it named, else
    /// of its best result. None when it gave no score. Left out of the file when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acoust_id_score: Option<i32>,
    /// The score AcoustID's answer needed to count, as a whole percent, beside a score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<i32>,
    /// What AcoustID matched, "Artist - Title", when it named a recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    /// The best recording AcoustID heard, "Artist - Title", when it named none: what came
    /// closest without counting as a match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closest: Option<String>,
}

impl Default for NoticeEntry {
    fn default() -> Self {
        NoticeEntry {
            key: String::new(),
            kind: NoticeKind::Review,
            username: String::new(),
            local_path: String::new(),
            artist: String::new(),
            title: String::new(),
            album: None,
            navidrome_id: None,
            group_key: None,
            order: 0,
            state: NoticeState::Waiting,
            reason: String::new(),
            cause: InconclusiveReason::None,
            origin: NoticeOrigin::Download,
            fingerprint: None,
            duration_seconds: 0,
            candidate_recording_id: None,
            file_format: None,
            submitted: false,
            lookup_attempts: 0,
            next_lookup_utc: datetime::min_value(),
            created_utc: Utc::now(),
            queued_utc: None,
            resolved_utc: None,
            acoust_id_score: None,
            threshold: None,
            matched: None,
            closest: None,
        }
    }
}

impl NoticeEntry {
    /// `[JsonIgnore] IsOpen`.
    pub fn is_open(&self) -> bool {
        matches!(self.state, NoticeState::Waiting | NoticeState::Queued)
    }
}

/// The same user, ignoring case and the asker's surrounding spaces.
fn same_user(entry_user: &str, username: &str) -> bool {
    dotnet::eq_ignore_case(entry_user, username.trim())
}

/// Where a kind sorts: Review first.
fn kind_order(kind: NoticeKind) -> u8 {
    match kind {
        NoticeKind::Review => 0,
        NoticeKind::Duplicates => 1,
    }
}

/// What Octo has asked each person about, and what they answered (#47, #53).
///
/// Resolved entries are kept, bounded, so a rescan never asks again about something a person
/// already settled: "a track resolved once should not come back". A fingerprint is dropped the
/// moment its entry resolves any way but Keep, and once it has been sent or refused, so the
/// file stays small.
///
/// [`NoticeQueue::run_flusher`] flushes every 5 s (a worker, which flushes once more on
/// shutdown), and `Drop` flushes what is left.
pub struct NoticeQueue {
    path: Option<PathBuf>,
    entries: Mutex<DotnetDictionary<NoticeEntry>>,
    flush_lock: Mutex<()>,
    dirty: AtomicBool,
}

impl Default for NoticeQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl NoticeQueue {
    pub const MAX_ENTRIES: usize = 5000;
    const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

    /// A person who removes a track from Review and then drops it into Delete answered with
    /// Delete, even though the sweep saw the removal first.
    const ACTED_AFTER_DISMISS_WINDOW: TimeDelta = TimeDelta::minutes(10);

    /// A queue in memory only (`new NoticeQueue()`).
    pub fn new() -> Self {
        Self::with_path(None)
    }

    /// A queue kept in `path`, read now. A file that cannot be read is set aside as
    /// `<file>.corrupt-<ticks>` and the queue starts empty.
    pub fn with_path(path: Option<PathBuf>) -> Self {
        let path = path.filter(|p| !dotnet::is_blank(&p.to_string_lossy()));
        let queue = NoticeQueue {
            path,
            entries: Mutex::new(DotnetDictionary::new()),
            flush_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
        };
        if let Some(path) = queue.path.clone() {
            queue.load(&path);
        }
        queue
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Follow a folder the library tidy moved: an open question would otherwise look for its
    /// file where it used to be, and a settled one would no longer stop Octo asking again.
    /// Answers how many entries followed.
    pub fn move_paths(&self, from: &str, to: &str) -> usize {
        let mut moved = 0;
        {
            let mut entries = self.entries.lock();
            let keys: Vec<String> = entries.keys().cloned().collect();
            for key in keys {
                let Some(entry) = entries.get(&key) else {
                    continue;
                };
                let Some(now) = PathHelper::moved_path(&entry.local_path, from, to) else {
                    continue;
                };
                let mut entry = entry.clone();
                let new_key = if entry.kind == NoticeKind::Review {
                    Self::review_key(&entry.username, &now)
                } else {
                    entry.key.clone()
                };
                if new_key != key && entries.contains_key(&new_key) {
                    continue;
                }
                entries.remove(&key);
                entry.key = new_key.clone();
                entry.local_path = now;
                entries.set(new_key, entry);
                moved += 1;
            }
        }
        if moved > 0 {
            self.dirty.store(true, Ordering::SeqCst);
        }
        moved
    }

    pub fn review_key(username: &str, local_path: &str) -> String {
        format!(
            "review|{}|{local_path}",
            dotnet::to_lower_invariant(username.trim())
        )
    }

    /// Ask `username` about a download verification could not settle. A file already asked
    /// about is never asked about again, in any state, which is what stops a dismissal from
    /// being undone by the next download of the same file.
    pub fn add_review(
        &self,
        username: &str,
        local_path: &str,
        song: &Song,
        verdict: &VerificationResult,
    ) -> bool {
        self.add_review_from(username, local_path, song, verdict, NoticeOrigin::Download)
    }

    /// `AddReview(..., origin)`.
    pub fn add_review_from(
        &self,
        username: &str,
        local_path: &str,
        song: &Song,
        verdict: &VerificationResult,
        origin: NoticeOrigin,
    ) -> bool {
        let key = Self::review_key(username, local_path);
        {
            let mut entries = self.entries.lock();
            if entries.contains_key(&key) {
                return false;
            }
            let reason = match verdict.reason {
                InconclusiveReason::NoEntry => "AcoustID has never heard this recording".to_string(),
                InconclusiveReason::BelowThreshold => "AcoustID was not sure what this is".to_string(),
                InconclusiveReason::SourceDisagreed => {
                    format!("AcoustID thinks this is {}", verdict.describe())
                }
                InconclusiveReason::SoundsLikeAnother => {
                    format!("Sounds like {}, not what its tags say", verdict.describe())
                }
                InconclusiveReason::LengthOff => format!(
                    "It runs {}, but the recording it matched runs {}",
                    clock(verdict.duration_seconds),
                    clock(
                        verdict
                            .r#match
                            .as_ref()
                            .and_then(|m| m.duration_seconds)
                            .unwrap_or(0)
                    )
                ),
                InconclusiveReason::DiffersFromYouTube => format!(
                    "AcoustID has never heard this recording, and it sounds nothing like its YouTube preview ({})",
                    verdict.evidence.as_deref().unwrap_or("not the same song")
                ),
                _ => "Octo could not check this download".to_string(),
            };
            let sweep = origin == NoticeOrigin::LibrarySweep;
            let score = verdict.shown_score().map(whole_percent);
            entries.set(
                key.clone(),
                NoticeEntry {
                    key,
                    kind: NoticeKind::Review,
                    username: username.trim().to_string(),
                    local_path: local_path.to_string(),
                    artist: song.artist.clone(),
                    title: song.title.clone(),
                    album: Some(song.album.clone()),
                    state: NoticeState::Waiting,
                    reason,
                    cause: verdict.reason,
                    origin,
                    // Dropped for a library question, so a Keep on one can never be submitted.
                    fingerprint: if sweep { None } else { verdict.fingerprint.clone() },
                    duration_seconds: verdict.duration_seconds,
                    candidate_recording_id: if sweep {
                        None
                    } else {
                        verdict.candidate_recording_id.clone()
                    },
                    file_format: Some(dotnet::to_lower_invariant(&extension(local_path))),
                    next_lookup_utc: Utc::now(),
                    acoust_id_score: score,
                    threshold: score
                        .and(Some(verdict.threshold))
                        .filter(|threshold| *threshold > 0.0)
                        .map(threshold_percent),
                    matched: verdict.matched_text(),
                    closest: verdict.closest_text(),
                    ..Default::default()
                },
            );
            Self::trim(&mut entries);
        }
        self.mark_dirty();
        true
    }

    /// What a replaced file's note says: "Octo replaced this: AcoustID heard Artist - Title, 97%
    /// sure (needs 85%)".
    pub fn replaced_reason(verdict: &VerificationResult, needs: i32) -> String {
        format!(
            "Octo replaced this: AcoustID heard {}, {}% sure (needs {needs}%)",
            verdict
                .matched_text()
                .unwrap_or_else(|| "another song".to_string()),
            whole_percent(verdict.score)
        )
    }

    /// Leave `username` a note that the library sweep replaced the file at `local_path` through
    /// Wrong song: a Review entry already Acted on, so it is never put in a playlist or asked
    /// again, with the cause and AcoustID's numbers a question would carry. False when a Review
    /// entry for this person and file is there already.
    pub fn add_replaced(
        &self,
        username: &str,
        local_path: &str,
        song: &Song,
        verdict: &VerificationResult,
        navidrome_id: &str,
        needs: i32,
    ) -> bool {
        self.add_replaced_because(
            username,
            local_path,
            song,
            verdict,
            navidrome_id,
            Self::replaced_reason(verdict, needs),
            Some(needs),
        )
    }

    /// [`Self::add_replaced`] with the note's own words. `threshold` is the score a heard
    /// recording needed, None where no score decided it.
    #[allow(clippy::too_many_arguments)]
    pub fn add_replaced_because(
        &self,
        username: &str,
        local_path: &str,
        song: &Song,
        verdict: &VerificationResult,
        navidrome_id: &str,
        reason: String,
        threshold: Option<i32>,
    ) -> bool {
        let key = Self::review_key(username, local_path);
        {
            let mut entries = self.entries.lock();
            if entries.contains_key(&key) {
                return false;
            }
            let now = Utc::now();
            entries.set(
                key.clone(),
                NoticeEntry {
                    key,
                    kind: NoticeKind::Review,
                    username: username.trim().to_string(),
                    local_path: local_path.to_string(),
                    artist: song.artist.clone(),
                    title: song.title.clone(),
                    album: Some(song.album.clone()),
                    navidrome_id: Some(navidrome_id.to_string()),
                    state: NoticeState::Acted,
                    reason,
                    cause: verdict.reason,
                    origin: NoticeOrigin::LibrarySweep,
                    duration_seconds: verdict.duration_seconds,
                    file_format: Some(dotnet::to_lower_invariant(&extension(local_path))),
                    next_lookup_utc: now,
                    created_utc: now,
                    resolved_utc: Some(now),
                    acoust_id_score: threshold.map(|_| whole_percent(verdict.score)),
                    threshold,
                    matched: verdict.matched_text(),
                    ..Default::default()
                },
            );
            Self::trim(&mut entries);
        }
        self.mark_dirty();
        true
    }

    /// Every file a Review question was ever about, open or answered, for anyone.
    pub fn reviewed_paths(&self) -> HashSet<String> {
        self.entries
            .lock()
            .values()
            .filter(|entry| entry.kind == NoticeKind::Review)
            .map(|entry| entry.local_path.clone())
            .collect()
    }

    pub fn open_count(&self, origin: NoticeOrigin) -> usize {
        self.entries
            .lock()
            .values()
            .filter(|entry| entry.is_open() && entry.origin == origin)
            .count()
    }

    pub fn for_user(&self, username: &str, kind: NoticeKind) -> Vec<NoticeEntry> {
        self.entries
            .lock()
            .values()
            .filter(|entry| entry.kind == kind && same_user(&entry.username, username))
            .cloned()
            .collect()
    }

    /// Review entries still waiting for Navidrome to scan the file, whose next lookup is due.
    pub fn due_for_lookup(&self, now_utc: DateTime<Utc>, limit: usize) -> Vec<NoticeEntry> {
        let mut due: Vec<NoticeEntry> = self
            .entries
            .lock()
            .values()
            .filter(|entry| {
                entry.kind == NoticeKind::Review
                    && entry.state == NoticeState::Waiting
                    && entry.navidrome_id.is_none()
                    && entry.next_lookup_utc <= now_utc
            })
            .cloned()
            .collect();
        due.sort_by_key(|entry| entry.created_utc);
        due.truncate(limit);
        due
    }

    pub fn is_queued(&self, username: &str, navidrome_id: &str) -> bool {
        self.entries.lock().values().any(|entry| {
            entry.state == NoticeState::Queued
                && entry.navidrome_id.as_deref() == Some(navidrome_id)
                && same_user(&entry.username, username)
        })
    }

    pub fn set_navidrome_id(&self, key: &str, navidrome_id: &str) {
        self.update(key, |entry| entry.navidrome_id = Some(navidrome_id.to_string()));
    }

    /// Not scanned yet: look again after 1, 2, 4, 8, then 16 minutes.
    pub fn defer_lookup(&self, key: &str, now_utc: DateTime<Utc>) {
        self.update(key, |entry| {
            let minutes = 1i64 << entry.lookup_attempts.clamp(0, 4);
            entry.lookup_attempts += 1;
            entry.next_lookup_utc = now_utc + TimeDelta::minutes(minutes);
        });
    }

    pub fn resolve(&self, key: &str, state: NoticeState) {
        self.update(key, |entry| {
            entry.state = state;
            entry.resolved_utc = Some(Utc::now());
            if state != NoticeState::Kept {
                entry.fingerprint = None;
            }
        });
    }

    pub fn mark_queued<S: AsRef<str>>(&self, keys: &[S]) {
        for key in keys {
            self.update(key.as_ref(), |entry| {
                entry.state = NoticeState::Queued;
                entry.queued_utc = Some(Utc::now());
            });
        }
    }

    /// A person kept this track, which answers every question Octo had open for them about it.
    /// For Review the question was about the file, so it is answered for everyone asked. For
    /// Duplicates, keeping one copy says the copies are on purpose, so that person's whole group
    /// is settled. A rating or the Keep playlist cannot say which playlist it came from, so a
    /// track in both is answered in both. Returns the entry Octo had open for this person,
    /// Review first, or None when Octo never asked them about this track.
    pub fn mark_kept(&self, username: &str, navidrome_id: &str) -> Option<NoticeEntry> {
        let mine = {
            let mut entries = self.entries.lock();
            let mut mine: Vec<NoticeEntry> = entries
                .values()
                .filter(|entry| {
                    entry.is_open()
                        && entry.navidrome_id.as_deref() == Some(navidrome_id)
                        && same_user(&entry.username, username)
                })
                .cloned()
                .collect();
            if mine.is_empty() {
                return None;
            }
            mine.sort_by_key(|entry| kind_order(entry.kind));

            let now = Utc::now();
            for asked in &mine {
                let open: Vec<NoticeEntry> = entries
                    .values()
                    .filter(|entry| entry.is_open() && entry.kind == asked.kind)
                    .cloned()
                    .collect();
                for entry in open {
                    let answered = if asked.kind == NoticeKind::Review {
                        entry.navidrome_id.as_deref() == Some(navidrome_id)
                            || entry.local_path == asked.local_path
                    } else {
                        entry.group_key == asked.group_key
                            && dotnet::eq_ignore_case(&entry.username, &asked.username)
                    };
                    if !answered {
                        continue;
                    }
                    let mut changed = entry;
                    changed.state = if asked.kind == NoticeKind::Review {
                        NoticeState::Kept
                    } else {
                        NoticeState::Dismissed
                    };
                    changed.resolved_utc = Some(now);
                    // One answer is one submission. A download nobody requested is asked of
                    // every allowed user, and each of their entries carries the same fingerprint.
                    if changed.key != asked.key {
                        changed.fingerprint = None;
                    }
                    entries.set(changed.key.clone(), changed);
                }
            }
            mine
        };
        self.mark_dirty();
        mine.into_iter().next()
    }

    /// An action ran on this track, so every open question about it is answered, for everyone:
    /// the file it was about has gone or changed. The rest of a duplicate group it belonged to
    /// is no longer the group Octo asked about, so those questions expire, and the next scan
    /// asks again if copies remain.
    pub fn mark_acted(&self, navidrome_id: &str) {
        let now = Utc::now();
        {
            let mut entries = self.entries.lock();
            let mut groups: HashSet<String> = HashSet::new();
            for entry in entries.values_mut() {
                if entry.navidrome_id.as_deref() != Some(navidrome_id) {
                    continue;
                }
                let recently_dismissed = entry.state == NoticeState::Dismissed
                    && entry
                        .resolved_utc
                        .is_some_and(|at| now - at < Self::ACTED_AFTER_DISMISS_WINDOW);
                if !entry.is_open() && !recently_dismissed {
                    continue;
                }
                entry.state = NoticeState::Acted;
                entry.resolved_utc = Some(now);
                entry.fingerprint = None;
                if let Some(group) = &entry.group_key {
                    groups.insert(group.clone());
                }
            }
            for entry in entries.values_mut() {
                if entry.is_open() && entry.group_key.as_ref().is_some_and(|g| groups.contains(g)) {
                    entry.state = NoticeState::Expired;
                    entry.resolved_utc = Some(now);
                }
            }
        }
        self.mark_dirty();
    }

    pub fn duplicate_key(username: &str, group_key: &str, track_id: &str) -> String {
        format!(
            "dup|{}|{group_key}|{track_id}",
            dotnet::to_lower_invariant(username.trim())
        )
    }

    /// Bring the Duplicates questions in line with a library walk (#53): one entry per allowed
    /// user per copy, the copy worth keeping first. Only a scan settles a duplicate by itself: a
    /// group no longer found means a copy went, so its open questions expire. A group someone
    /// settled stays settled for them, and a group that expired and is found again is asked
    /// again. A walk that did not finish adds what it found and expires nothing. Returns how
    /// many questions it opened.
    pub fn sync_duplicates<S: AsRef<str>>(
        &self,
        groups: &[DuplicateGroup],
        users: &[S],
        complete: bool,
    ) -> usize {
        let now = Utc::now();
        // Each user once, ignoring case: the first spelling of each user.
        let mut owners: Vec<String> = Vec::new();
        for user in users {
            let user = user.as_ref().trim();
            if !user.is_empty() && !owners.iter().any(|o| dotnet::eq_ignore_case(o, user)) {
                owners.push(user.to_string());
            }
        }
        let live: HashSet<&str> = groups.iter().map(|group| group.key.as_str()).collect();
        let mut opened = 0;
        {
            let mut entries = self.entries.lock();
            if complete {
                for entry in entries.values_mut() {
                    if entry.kind == NoticeKind::Duplicates
                        && entry.is_open()
                        && !live.contains(entry.group_key.as_deref().unwrap_or(""))
                    {
                        entry.state = NoticeState::Expired;
                        entry.resolved_utc = Some(now);
                    }
                }
            }

            let settled: HashSet<String> = entries
                .values()
                .filter(|entry| {
                    entry.kind == NoticeKind::Duplicates
                        && entry.group_key.is_some()
                        && matches!(
                            entry.state,
                            NoticeState::Dismissed | NoticeState::Kept | NoticeState::Acted
                        )
                })
                .map(|entry| {
                    format!(
                        "{}|{}",
                        dotnet::to_lower_invariant(&entry.username),
                        entry.group_key.as_deref().unwrap_or("")
                    )
                })
                .collect();

            for group in groups {
                for user in &owners {
                    if settled.contains(&format!("{}|{}", dotnet::to_lower_invariant(user), group.key)) {
                        continue;
                    }
                    for (i, track) in group.tracks.iter().enumerate() {
                        let key = Self::duplicate_key(user, &group.key, &track.id);
                        let reason = if i == 0 {
                            format!("The best of {} copies: {}", group.tracks.len(), describe(track))
                        } else {
                            format!(
                                "{}, also in the library as {}",
                                describe(track),
                                describe(&group.tracks[0])
                            )
                        };
                        if let Some(existing) = entries.get_mut(&key) {
                            if existing.state != NoticeState::Expired {
                                continue;
                            }
                            existing.state = NoticeState::Waiting;
                            existing.order = i as i32;
                            existing.reason = reason;
                            existing.created_utc = now;
                            existing.queued_utc = None;
                            existing.resolved_utc = None;
                            opened += 1;
                            continue;
                        }
                        entries.set(
                            key.clone(),
                            NoticeEntry {
                                key,
                                kind: NoticeKind::Duplicates,
                                username: user.clone(),
                                artist: track.artist.clone(),
                                title: track.title.clone(),
                                album: Some(track.album.clone()),
                                navidrome_id: Some(track.id.clone()),
                                group_key: Some(group.key.clone()),
                                order: i as i32,
                                state: NoticeState::Waiting,
                                reason,
                                created_utc: now,
                                next_lookup_utc: now,
                                ..Default::default()
                            },
                        );
                        opened += 1;
                    }
                }
            }
            Self::trim(&mut entries);
        }
        self.mark_dirty();
        opened
    }

    /// Kept entries with a fingerprint that has not been sent or refused yet.
    pub fn awaiting_submission(&self) -> Vec<NoticeEntry> {
        self.entries
            .lock()
            .values()
            .filter(|entry| {
                entry.state == NoticeState::Kept && !entry.submitted && entry.fingerprint.is_some()
            })
            .cloned()
            .collect()
    }

    /// Sent, or deliberately not sent: either way the fingerprint is not needed again.
    pub fn mark_submitted<S: AsRef<str>>(&self, keys: &[S], sent: bool) {
        for key in keys {
            self.update(key.as_ref(), |entry| {
                entry.submitted = sent;
                entry.fingerprint = None;
            });
        }
    }

    /// The newest first, at most `limit`.
    pub fn recent(&self, limit: usize) -> Vec<NoticeEntry> {
        let mut entries: Vec<NoticeEntry> = self.entries.lock().values().cloned().collect();
        // A stable sort: equal times keep their order.
        entries.sort_by(|a, b| {
            let at = |e: &NoticeEntry| e.resolved_utc.or(e.queued_utc).unwrap_or(e.created_utc);
            at(b).cmp(&at(a))
        });
        entries.truncate(limit);
        entries
    }

    fn update(&self, key: &str, change: impl FnOnce(&mut NoticeEntry)) {
        {
            let mut entries = self.entries.lock();
            let Some(entry) = entries.get_mut(key) else {
                return;
            };
            change(entry);
        }
        self.mark_dirty();
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Oldest resolved entries go first. An open question is never forgotten.
    fn trim(entries: &mut DotnetDictionary<NoticeEntry>) {
        if entries.len() <= Self::MAX_ENTRIES {
            return;
        }
        let mut resolved: Vec<(DateTime<Utc>, String)> = entries
            .values()
            .filter(|entry| !entry.is_open())
            .map(|entry| (entry.resolved_utc.unwrap_or(entry.created_utc), entry.key.clone()))
            .collect();
        // A stable sort: equal times go in dictionary order.
        resolved.sort_by_key(|(at, _)| *at);
        let excess = entries.len() - Self::MAX_ENTRIES;
        for (_, key) in resolved.into_iter().take(excess) {
            entries.remove(&key);
        }
    }

    fn load(&self, path: &Path) {
        let attempt: anyhow::Result<()> = (|| {
            if !path.exists() {
                return Ok(());
            }
            let text = state_file::read_all_text(path)?;
            let Some(loaded) = serde_json::from_str::<Option<Vec<NoticeEntry>>>(&text)? else {
                return Ok(());
            };
            let mut entries = self.entries.lock();
            for entry in loaded {
                if !entry.key.is_empty() {
                    entries.set(entry.key.clone(), entry);
                }
            }
            Self::trim(&mut entries);
            Ok(())
        })();
        if let Err(e) = attempt {
            // Kept aside rather than overwritten: it holds what people already answered.
            warn!("notice queue could not be read ({e}); starting empty");
            set_aside(path);
        }
    }

    /// Writes the queue if it changed. False when the write failed; the queue stays dirty.
    pub fn flush(&self) -> bool {
        let Some(path) = self.path.as_deref() else {
            return true;
        };
        let _flushing = self.flush_lock.lock();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return true;
        }
        let json = {
            let entries = self.entries.lock();
            octo_core::json::to_string(&entries.values().collect::<Vec<_>>())
        };
        match state_file::save_atomic(path, &json) {
            Ok(()) => true,
            Err(e) => {
                self.dirty.store(true, Ordering::SeqCst);
                warn!("notice queue could not be written: {e}");
                false
            }
        }
    }

    /// The 5-second flush timer, and the last flush on shutdown, as a worker.
    pub async fn run_flusher(self: Arc<Self>, token: CancellationToken) -> anyhow::Result<()> {
        let queue = self.clone();
        state_file::flush_every(
            Self::FLUSH_INTERVAL,
            token,
            Arc::new(move || {
                queue.flush();
            }),
        )
        .await
    }
}

impl Drop for NoticeQueue {
    /// The last flush.
    fn drop(&mut self) {
        self.flush();
    }
}

/// Renames the file to `<path>.corrupt-<ticks>` (now, in 100 ns ticks), best effort.
pub(crate) fn set_aside(path: &Path) {
    let ticks = octo_core::library::generated_playlist_service::dotnet_ticks(Utc::now());
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".corrupt-{ticks}"));
    let _ = std::fs::rename(path, PathBuf::from(aside));
}

/// A length as `h:mm:ss` from an hour up, otherwise `m:ss`, negative taken as zero. The hours
/// are those within the day, so a day or more drops its days.
fn clock(seconds: i32) -> String {
    let total = i64::from(seconds.max(0));
    let (hours, minutes, secs) = ((total / 3600) % 24, (total / 60) % 60, total % 60);
    if seconds >= 3600 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

fn describe(track: &LibraryTrack) -> String {
    let suffix = dotnet::to_upper_invariant(&track.suffix);
    let mut text = if track.bit_rate > 0 {
        format!("{suffix}, {} kbps", track.bit_rate)
    } else {
        suffix
    };
    if let Some(source) = &track.transcoded_from {
        text.push_str(&format!(", likely transcoded from {source}"));
    }
    text
}

/// The extension without its dot, or empty.
fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot + 1..].trim_start_matches('.').to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
#[path = "notice_queue_tests.rs"]
mod tests;

/// What a download asks of the notice queue (`BaseDownloadService` resolved `NoticeQueue`): a
/// file kept although verification doubted it becomes a question for its owner.
impl crate::services::common::base_download_service::ReviewQueue for NoticeQueue {
    fn add_review(&self, owner: &str, local_path: &str, song: &Song, verdict: &VerificationResult) -> bool {
        NoticeQueue::add_review(self, owner, local_path, song, verdict)
    }
}
