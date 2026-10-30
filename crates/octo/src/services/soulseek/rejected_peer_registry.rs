//! The rejected peer registry, the store behind `rejected-peers.json`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::Clock;
use octo_core::common::dotnet::ordinal_ignore_case_key;
use octo_core::fingerprint::heart_gate::HeardRecording;
use octo_core::json::datetime;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::services::state_file;

/// One remembered rejection: a line of `rejected-peers.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Entry {
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub username: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub filename: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub reason: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub track: String,
    #[serde(with = "datetime::utc")]
    pub rejected_utc: DateTime<Utc>,
    /// For a refusal by one heart's or play's gate, the song and the recording it wanted, which
    /// only heart and play origins read; empty for the shared refusals every download reads.
    /// Written only when set, so the shared list's lines stay as they were.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub heart: String,
}

impl Default for Entry {
    fn default() -> Self {
        Entry {
            username: String::new(),
            filename: String::new(),
            reason: String::new(),
            track: String::new(),
            // The earliest time, for an entry whose line in the file has none.
            rejected_utc: datetime::min_value(),
            heart: String::new(),
        }
    }
}

impl Entry {
    /// The key the list finds this entry by: its peer and file, and for a heart's refusal its
    /// song and recording too.
    fn key(&self) -> String {
        let shared = RejectedPeerRegistry::make_key(Some(&self.username), Some(&self.filename));
        if self.heart.is_empty() {
            shared
        } else {
            format!("heart|{}|{shared}", self.heart)
        }
    }
}

/// The scope of a heart's refusal: the song, and the recording ids the gate wanted, in order of
/// their own so one R always gives one scope.
fn heart_scope(song: &str, r: &HeardRecording) -> String {
    let mut ids: Vec<String> = r.ids.iter().map(|id| id.trim().to_lowercase()).collect();
    ids.sort();
    ids.dedup();
    format!("{}|{}", song.trim(), ids.join(","))
}

/// Remembers the peer-and-file pairs a completed download proved were the wrong recording,
/// so `rank_candidates` never offers them again.
///
/// Before this, a rejection deleted the bytes and threw the fact away. The next star re-ran
/// the same search, ranked the same peer first for the same reasons, and paid for the same
/// wrong file again. That is issue #40's actual complaint: not that Octo picks badly, but
/// that it cannot learn.
///
/// Shaped like [`super::ExternalIdRegistry`]: LRU-bounded, coalesced flush, atomic
/// temp-and-rename, and a file that will not parse is a cold start rather than a failure to
/// boot. The one addition is a TTL, because this list can be WRONG about a file and a
/// permanent deny-list turns one false positive into a track that can never be fetched.
pub struct RejectedPeerRegistry {
    path: Option<PathBuf>,
    ttl_days: Arc<dyn Fn() -> i32 + Send + Sync>,
    clock: Clock,
    lru: Mutex<Lru>,
    dirty: AtomicBool,
    flush_lock: Mutex<()>,
}

/// Case-insensitive on the whole composite: slskd echoes the peer's own path, and the
/// casing differs between a search response and a transfer record. Two distinct files on
/// one peer never differ by case alone, so nothing is over-denied by that.
///
/// Keyed by the case-folded key, the recency list included.
#[derive(Default)]
struct Lru {
    by_key: HashMap<String, (Entry, u64)>,
    order: BTreeMap<u64, String>,
    next: u64,
}

impl Lru {
    fn put(&mut self, folded: String, entry: Entry) {
        let stamp = self.next;
        self.next += 1;
        if let Some((_, old)) = self.by_key.insert(folded.clone(), (entry, stamp)) {
            self.order.remove(&old);
        }
        self.order.insert(stamp, folded);
    }

    fn forget(&mut self, folded: &str) {
        if let Some((_, stamp)) = self.by_key.remove(folded) {
            self.order.remove(&stamp);
        }
    }

    fn trim(&mut self) {
        while self.by_key.len() > RejectedPeerRegistry::MAX_ENTRIES {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.by_key.remove(&oldest);
        }
    }
}

impl RejectedPeerRegistry {
    pub const MAX_ENTRIES: usize = 10_000;

    /// Fallback when no settings are supplied, which is only the case in tests. The real value
    /// is Soulseek:RejectedPeerTtlDays, because how long a denial stands is a judgment about
    /// how much a user trusts the verdict, not an invariant.
    pub const DEFAULT_TTL_DAYS: i32 = 30;

    /// A search registers many candidates, so flushing per write would turn one
    /// download into a burst of file writes. Same reasoning as the id registry.
    pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

    /// In memory only, with the default TTL.
    pub fn in_memory() -> Self {
        Self::new(None::<PathBuf>, None)
    }

    /// A registry kept at `path` (loaded now; in memory when absent or blank). `ttl_days` is
    /// read at every use, so a settings change applies without a restart; None means
    /// [`Self::DEFAULT_TTL_DAYS`].
    pub fn new(path: Option<impl AsRef<Path>>, ttl_days: Option<Arc<dyn Fn() -> i32 + Send + Sync>>) -> Self {
        Self::with_clock(path, ttl_days, Clock::system())
    }

    /// [`Self::new`] reading the time from `clock`.
    pub fn with_clock(
        path: Option<impl AsRef<Path>>,
        ttl_days: Option<Arc<dyn Fn() -> i32 + Send + Sync>>,
        clock: Clock,
    ) -> Self {
        let path = path
            .map(|p| p.as_ref().to_path_buf())
            .filter(|p| !p.as_os_str().to_string_lossy().trim().is_empty());
        let registry = RejectedPeerRegistry {
            path,
            ttl_days: ttl_days.unwrap_or_else(|| Arc::new(|| Self::DEFAULT_TTL_DAYS)),
            clock,
            lru: Mutex::new(Lru::default()),
            dirty: AtomicBool::new(false),
            flush_lock: Mutex::new(()),
        };
        if registry.path.is_some() {
            registry.load();
        }
        registry
    }

    /// Identity of a candidate: the same pair `SoulseekFileHit` carries and the same pair
    /// `enqueue_download` and `wait_for_completion` already use. Deliberately NOT the
    /// username alone, which would blacklist a whole well-stocked library over one bad rip.
    pub fn make_key(username: Option<&str>, filename: Option<&str>) -> String {
        format!("{}|{}", username.unwrap_or(""), filename.unwrap_or(""))
    }

    /// Pure so the lapse rule can be tested without waiting a month. 0 days never
    /// lapses, which is a real choice for anyone who would rather clear the list by hand.
    pub fn is_expired(rejected_utc: DateTime<Utc>, now_utc: DateTime<Utc>, ttl_days: i32) -> bool {
        ttl_days > 0 && now_utc - rejected_utc >= TimeDelta::days(i64::from(ttl_days))
    }

    pub fn count(&self) -> usize {
        self.lru.lock().by_key.len()
    }

    pub fn deny(&self, username: Option<&str>, filename: Option<&str>, reason: &str, track: &str) {
        self.remember(username, filename, reason, track, String::new());
    }

    /// A refusal by one heart's or play's gate: this peer's file is not the recording `r` for the
    /// song `song`, which says nothing about whether it is the song at all. Only heart and play
    /// origins read it ([`Self::is_denied_for_heart`]), so the same file still serves imports and
    /// other people's hearts. A video is remembered under the peer name "youtube".
    pub fn deny_for_heart(
        &self,
        song: &str,
        r: &HeardRecording,
        username: Option<&str>,
        filename: Option<&str>,
        reason: &str,
    ) {
        self.remember(username, filename, reason, song, heart_scope(song, r));
    }

    /// Whether a heart's or play's gate refused this peer's file for the song and this R.
    pub fn is_denied_for_heart(
        &self,
        song: &str,
        r: &HeardRecording,
        username: Option<&str>,
        filename: Option<&str>,
    ) -> bool {
        let (Some(username), Some(filename)) = (
            username.filter(|u| !u.is_empty()),
            filename.filter(|f| !f.is_empty()),
        ) else {
            return false;
        };
        let entry = Entry {
            username: username.to_string(),
            filename: filename.to_string(),
            heart: heart_scope(song, r),
            ..Default::default()
        };
        self.is_denied_key(&ordinal_ignore_case_key(&entry.key()))
    }

    fn remember(
        &self,
        username: Option<&str>,
        filename: Option<&str>,
        reason: &str,
        track: &str,
        heart: String,
    ) {
        let (Some(username), Some(filename)) = (
            username.filter(|u| !u.is_empty()),
            filename.filter(|f| !f.is_empty()),
        ) else {
            return;
        };
        let entry = Entry {
            username: username.to_string(),
            filename: filename.to_string(),
            reason: reason.to_string(),
            track: track.to_string(),
            rejected_utc: self.clock.now(),
            heart,
        };
        let for_heart = !entry.heart.is_empty();
        {
            let mut lru = self.lru.lock();
            lru.put(ordinal_ignore_case_key(&entry.key()), entry);
            lru.trim();
        }
        self.dirty.store(true, Ordering::SeqCst);
        if for_heart {
            info!("Remembering {username} -> {filename} as refused for one heart's version of {track} ({reason})");
        } else {
            info!("Remembering rejected candidate {username} -> {filename} ({reason})");
        }
    }

    /// Expiry is enforced here rather than by a sweep timer: a container that runs for
    /// months would otherwise honor a lapsed denial forever, which is the exact trap the
    /// TTL exists to avoid. Called from inside the candidate ranking, so it is a dictionary
    /// hit and nothing more.
    pub fn is_denied(&self, username: Option<&str>, filename: Option<&str>) -> bool {
        let (Some(username), Some(filename)) = (
            username.filter(|u| !u.is_empty()),
            filename.filter(|f| !f.is_empty()),
        ) else {
            return false;
        };
        let folded = ordinal_ignore_case_key(&Self::make_key(Some(username), Some(filename)));
        self.is_denied_key(&folded)
    }

    fn is_denied_key(&self, folded: &str) -> bool {
        let mut lru = self.lru.lock();
        let Some((entry, _)) = lru.by_key.get(folded) else {
            return false;
        };
        if !Self::is_expired(entry.rejected_utc, self.clock.now(), (self.ttl_days)()) {
            return true;
        }
        lru.forget(folded);
        drop(lru);
        self.dirty.store(true, Ordering::SeqCst);
        false
    }

    /// Lifts the denial of `username`'s `filename`, and only that one: the user's other files
    /// stay denied. Written at once, as [`Self::clear`] is. True when there was one to lift.
    pub fn allow(&self, username: &str, filename: &str) -> bool {
        if username.is_empty() || filename.is_empty() {
            return false;
        }
        let folded = ordinal_ignore_case_key(&Self::make_key(Some(username), Some(filename)));
        let lifted = {
            let mut lru = self.lru.lock();
            let present = lru.by_key.contains_key(&folded);
            lru.forget(&folded);
            present
        };
        if lifted {
            self.dirty.store(true, Ordering::SeqCst);
            self.flush();
            info!("No longer rejecting {username} -> {filename}");
        }
        lifted
    }

    /// The recovery lever for a wrong denial. Flushes synchronously rather than waiting for
    /// the timer: a user who clears the list and immediately restarts the container must not
    /// get every entry back, which is what "cleared" would otherwise mean for ten seconds.
    pub fn clear(&self) -> usize {
        let removed = {
            let mut lru = self.lru.lock();
            let removed = lru.by_key.len();
            lru.by_key.clear();
            lru.order.clear();
            removed
        };
        self.dirty.store(true, Ordering::SeqCst);
        self.flush();
        removed
    }

    fn load(&self) {
        let Some(path) = &self.path else { return };
        let result = (|| -> anyhow::Result<()> {
            let Some(text) = state_file::read_text(path)? else {
                return Ok(());
            };
            let entries: Option<Vec<Entry>> = serde_json::from_str(&text)?;
            let Some(entries) = entries else { return Ok(()) };

            let now = self.clock.now();
            // Stored most-recently-used first, so replaying in order rebuilds the same
            // eviction order. A restart is also when lapsed entries get collected.
            let mut lru = self.lru.lock();
            let mut stamp = entries.len() as u64;
            for entry in entries {
                if entry.username.is_empty() || entry.filename.is_empty() {
                    continue;
                }
                if Self::is_expired(entry.rejected_utc, now, (self.ttl_days)()) {
                    continue;
                }
                let folded = ordinal_ignore_case_key(&entry.key());
                match lru.by_key.get_mut(&folded) {
                    Some((existing, _)) => *existing = entry,
                    None => {
                        lru.by_key.insert(folded.clone(), (entry, stamp));
                        lru.order.insert(stamp, folded);
                        stamp -= 1;
                    }
                }
            }
            lru.next = lru.order.keys().next_back().map_or(0, |s| s + 1);
            lru.trim();
            let count = lru.by_key.len();
            drop(lru);
            if count > 0 {
                info!("rejected peer registry restored {count} entries");
            }
            Ok(())
        })();
        if let Err(e) = result {
            // A registry that will not load is a cold start, not a failure to boot.
            warn!("rejected peer registry could not be read: {e}");
        }
    }

    /// Writes the list when anything changed since the last write.
    pub fn flush(&self) {
        let Some(path) = &self.path else { return };
        let _flushing = self.flush_lock.lock();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let entries: Vec<Entry> = {
            let lru = self.lru.lock();
            lru.order
                .values()
                .rev()
                .filter_map(|k| lru.by_key.get(k).map(|(e, _)| e.clone()))
                .collect()
        };
        if let Err(e) = state_file::save_atomic(path, &octo_core::json::to_string(&entries)) {
            // Best-effort: losing a flush costs the denials since the last one, which is the
            // behavior we had before this existed. It must never take a download down.
            self.dirty.store(true, Ordering::SeqCst);
            warn!("rejected peer registry could not be written: {e}");
        }
    }

    /// The flush timer, as a worker: every [`Self::FLUSH_INTERVAL`], and once more on shutdown.
    pub async fn run_flusher(
        self: Arc<Self>,
        token: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let registry = self.clone();
        state_file::flush_every(Self::FLUSH_INTERVAL, token, Arc::new(move || registry.flush())).await
    }
}

impl Drop for RejectedPeerRegistry {
    /// The last flush.
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FILE1: &str = r"MyMusic\Mark Morrison\Return of the Mack\05 Return of the Mack.flac";
    const FILE2: &str = r"MyMusic\Mark Morrison\Return of the Mack\06 Horny.flac";

    fn temp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("octo-denylist.json");
        (dir, path)
    }

    #[test]
    fn is_denied_after_deny_blocks_that_exact_peer_and_file() {
        let registry = RejectedPeerRegistry::in_memory();
        registry.deny(
            Some("peer1"),
            Some(FILE1),
            "is a karaoke version",
            "Mark Morrison - Return of the Mack",
        );
        assert!(registry.is_denied(Some("peer1"), Some(FILE1)));
        assert_eq!(registry.count(), 1);
    }

    /// Denying a peer wholesale would blacklist an entire well-stocked library over one bad
    /// rip, which costs far more than the one file it saves.
    #[test]
    fn is_denied_same_peer_different_file_is_still_allowed() {
        let registry = RejectedPeerRegistry::in_memory();
        registry.deny(Some("peer1"), Some(FILE1), "wrong recording", "A - B");
        assert!(!registry.is_denied(Some("peer1"), Some(FILE2)));
        assert!(!registry.is_denied(Some("peer2"), Some(FILE1)));
    }

    /// slskd echoes the peer's own path, and the casing differs between a search response and
    /// a transfer record. Two distinct files on one peer never differ by case alone.
    #[test]
    fn is_denied_casing_varies_between_search_and_transfer_still_matches() {
        let registry = RejectedPeerRegistry::in_memory();
        registry.deny(Some("Peer1"), Some(FILE1), "wrong recording", "A - B");
        assert!(registry.is_denied(Some("peer1"), Some(&FILE1.to_uppercase())));
    }

    /// A deny-list with no expiry turns one false positive into a track that can never be
    /// fetched again, with nothing in the UI saying why.
    #[test]
    fn is_expired_lapses_at_the_configured_age() {
        let now = Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap();
        for (age, ttl, expired) in [
            (31, 30, true),
            (30, 30, true),
            (29, 30, false),
            (1, 30, false),
            (8, 7, true),
            (6, 7, false),
        ] {
            assert_eq!(
                RejectedPeerRegistry::is_expired(now - TimeDelta::days(age), now, ttl),
                expired,
                "age {age}, ttl {ttl}"
            );
        }
    }

    /// 0 is a real choice, not a disabled feature: it suits anyone who would rather clear the
    /// list by hand than have denials lapse on their own.
    #[test]
    fn is_expired_zero_days_never_lapses() {
        let now = Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap();
        assert!(!RejectedPeerRegistry::is_expired(
            now - TimeDelta::days(5 * 365),
            now,
            0
        ));
    }

    #[test]
    fn deny_survives_a_restart() {
        let (_dir, path) = temp_path();
        {
            let first = RejectedPeerRegistry::new(Some(&path), None);
            first.deny(Some("peer1"), Some(FILE1), "is a karaoke version", "A - B");
        }
        let second = RejectedPeerRegistry::new(Some(&path), None);
        assert!(second.is_denied(Some("peer1"), Some(FILE1)));
    }

    /// Clearing flushes synchronously. A user who clears the list and immediately restarts the
    /// container must not get every entry back, which is what "cleared" would otherwise mean
    /// for the ten seconds until the coalesced flush fires.
    #[test]
    fn clear_takes_effect_before_the_next_flush_tick() {
        let (_dir, path) = temp_path();
        let first = RejectedPeerRegistry::new(Some(&path), None);
        first.deny(Some("peer1"), Some(FILE1), "wrong recording", "A - B");
        first.deny(Some("peer2"), Some(FILE2), "wrong recording", "A - B");
        assert_eq!(first.clear(), 2);
        // Not dropped: the file must already say so.
        let second = RejectedPeerRegistry::new(Some(&path), None);
        assert_eq!(second.count(), 0);
        drop(first);
    }

    /// A registry that will not load is a cold start, not a failure to boot.
    #[test]
    fn load_unreadable_file_starts_empty_rather_than_throwing() {
        let (_dir, path) = temp_path();
        std::fs::write(&path, "{ this is not the shape we wrote }").expect("writes");
        let registry = RejectedPeerRegistry::new(Some(&path), None);
        assert_eq!(registry.count(), 0);
        assert!(!registry.is_denied(Some("peer1"), Some(FILE1)));
    }

    fn r(ids: &[&str]) -> HeardRecording {
        HeardRecording {
            ids: ids.iter().map(|id| id.to_string()).collect(),
            ..Default::default()
        }
    }

    /// A heart's refusal is its own: it never bars the peer's file for an import (the shared
    /// list), nor for a heart of the same song that wants another recording; the shared list
    /// never reads as a heart's refusal either. Both survive a restart, and the shared list's
    /// lines carry no heart field.
    #[test]
    fn a_hearts_refusal_bars_only_that_song_and_recording() {
        let (_dir, path) = temp_path();
        {
            let first = RejectedPeerRegistry::new(Some(&path), None);
            first.deny_for_heart("song-1", &r(&["rec-2009"]), Some("peer1"), Some(FILE1), "another mix");
            first.deny(Some("peer2"), Some(FILE2), "is a karaoke version", "A - B");
            assert!(first.is_denied_for_heart("song-1", &r(&["REC-2009"]), Some("peer1"), Some(FILE1)));
            assert!(!first.is_denied(Some("peer1"), Some(FILE1)), "imports still take it");
            assert!(!first.is_denied_for_heart("song-1", &r(&["rec-2015"]), Some("peer1"), Some(FILE1)));
            assert!(!first.is_denied_for_heart("song-2", &r(&["rec-2009"]), Some("peer1"), Some(FILE1)));
            assert!(!first.is_denied_for_heart("song-1", &r(&["rec-2009"]), Some("peer2"), Some(FILE2)));
        }
        let text = std::fs::read_to_string(&path).expect("written");
        assert_eq!(text.matches("\"Heart\"").count(), 1, "{text}");
        let second = RejectedPeerRegistry::new(Some(&path), None);
        assert!(second.is_denied_for_heart("song-1", &r(&["rec-2009"]), Some("peer1"), Some(FILE1)));
        assert!(!second.is_denied(Some("peer1"), Some(FILE1)));
        assert!(second.is_denied(Some("peer2"), Some(FILE2)));
    }

    #[test]
    fn deny_blank_peer_or_file_is_ignored() {
        let registry = RejectedPeerRegistry::in_memory();
        registry.deny(Some(""), Some(FILE1), "r", "t");
        registry.deny(Some("peer1"), Some(""), "r", "t");
        registry.deny(None, Some(FILE1), "r", "t");
        assert_eq!(registry.count(), 0);
        assert!(!registry.is_denied(Some(""), Some(FILE1)));
    }

    // ---- The state file and lapsed denials ----------------------------------------------

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/state/rejected-peers.json"
    );

    #[test]
    fn the_fixture_round_trips_byte_for_byte() {
        let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
        let entries: Vec<Entry> = serde_json::from_str(&text).expect("the fixture reads");
        assert_eq!(entries.len(), 2);
        assert_eq!(octo_core::json::to_string(&entries), text.trim_end_matches('\n'));
    }

    #[test]
    fn a_load_keeps_live_entries_in_order_and_drops_lapsed_ones() {
        let (_dir, path) = temp_path();
        let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
        std::fs::write(&path, &text).expect("writes");

        // Both live: written back unchanged.
        let at = Utc.with_ymd_and_hms(2026, 10, 4, 0, 0, 0).unwrap();
        {
            let registry = RejectedPeerRegistry::with_clock(Some(&path), None, Clock::fixed(at));
            assert_eq!(registry.count(), 2);
            registry.dirty.store(true, Ordering::SeqCst);
        }
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads"),
            text.trim_end_matches('\n')
        );

        // A 5-day TTL, read live: the 2026-09-28 entry has lapsed at load.
        let ttl: Arc<dyn Fn() -> i32 + Send + Sync> = Arc::new(|| 5);
        let registry = RejectedPeerRegistry::with_clock(Some(&path), Some(ttl), Clock::fixed(at));
        assert_eq!(registry.count(), 1);
        assert!(registry.is_denied(
            Some("VINYL_RIPS_4U"),
            Some("@@abcde\\Music\\Björk\\Homogenic\\05 - Jóga.flac")
        ));
    }

    #[test]
    fn a_lapsed_denial_is_forgotten_when_asked_about() {
        let start = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        let now = Arc::new(Mutex::new(start));
        let clock = {
            let now = now.clone();
            Clock::new(move || *now.lock())
        };
        let registry = RejectedPeerRegistry::with_clock(None::<PathBuf>, None, clock);
        registry.deny(Some("peer1"), Some(FILE1), "r", "t");
        *now.lock() = start + TimeDelta::days(29);
        assert!(registry.is_denied(Some("peer1"), Some(FILE1)));
        *now.lock() = start + TimeDelta::days(30);
        assert!(!registry.is_denied(Some("peer1"), Some(FILE1)));
        assert_eq!(registry.count(), 0);
    }
}
