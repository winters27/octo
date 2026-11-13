//! Every song's transition profile from octo-sonic: one small file per song under
//! `transition-profiles/`, and an index (`transition-profiles.json`) of which song has one,
//! of which file stamp, analysis version and revision. The index is written at most once a
//! minute, like `sonic-features.json`, and every few songs read again for a newer revision; a
//! profile's own file is written as it comes.
//!
//! Library songs are kept by their Navidrome id, songs from outside the library by their
//! acquisition key (`provider:id`) with the path of their file.
//!
//! A revision is a pass of the analysis within one version: players read every revision of a
//! version alike, so a profile of a lower revision is served until its song is read again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::json::datetime;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::services::state_file::{self, null_as_default};

/// The revision of a profile that does not say, and of an index no pass has completed.
pub const FIRST_REVISION: i32 = 1;

fn first_revision() -> i32 {
    FIRST_REVISION
}

/// One song's profile, as the index knows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TransitionEntry {
    /// What the song's file was when it was read, so a changed file is read again.
    #[serde(deserialize_with = "null_as_default")]
    pub stamp: String,
    /// The analysis version the profile comes from.
    pub version: i32,
    /// The revision of that version the profile comes from.
    #[serde(default = "first_revision")]
    pub revision: i32,
    /// For a song from outside the library: its file.
    pub path: Option<String>,
    /// The last reading of it for its profile alone that failed: a song from outside the
    /// library, or a library song read again for a newer revision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<TransitionFailure>,
}

impl Default for TransitionEntry {
    fn default() -> Self {
        TransitionEntry {
            stamp: String::new(),
            version: 0,
            revision: FIRST_REVISION,
            path: None,
            failed: None,
        }
    }
}

/// A reading of a song for its profile alone that failed: its file then, the revision it was
/// read for, and when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TransitionFailure {
    #[serde(deserialize_with = "null_as_default")]
    pub stamp: String,
    pub revision: i32,
    #[serde(with = "datetime::utc")]
    pub at_utc: DateTime<Utc>,
}

impl Default for TransitionFailure {
    fn default() -> Self {
        TransitionFailure {
            stamp: String::new(),
            revision: 0,
            at_utc: datetime::min_value(),
        }
    }
}

/// The index file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TransitionIndex {
    /// The analysis version octo-sonic writes, as it last said; 0 before it has.
    pub version: i32,
    /// The revision octo-sonic said when the last complete pass over the songs began, so every
    /// song it read is at least at it; [`FIRST_REVISION`] until then.
    #[serde(default = "first_revision")]
    pub revision: i32,
    #[serde(deserialize_with = "null_as_default")]
    pub songs: IndexMap<String, TransitionEntry>,
}

impl Default for TransitionIndex {
    fn default() -> Self {
        TransitionIndex {
            version: 0,
            revision: FIRST_REVISION,
            songs: IndexMap::new(),
        }
    }
}

/// How far a re-reading of profiles has come: of the `total` profiles of the version served
/// that are read again or still can be, `done` are at the revision octo-sonic writes now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevisionProgress {
    pub done: usize,
    pub total: usize,
}

/// What there is for a song.
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionLookup {
    /// The profile, as octo-sonic wrote it.
    Ready(Value),
    /// None yet, or only one from an older analysis.
    NotReady,
}

// The index of a whole library is a few megabytes; once a minute is plenty.
const FLUSH_EVERY: TimeDelta = TimeDelta::minutes(1);

/// Songs read again for a newer revision that may wait in memory before the index is saved,
/// in an index of up to [`SONGS_PER_REREAD`] times as many songs.
const REREADS_PER_SAVE: usize = 5;
/// A save rewrites the whole index, some 100 bytes a song: in a larger index one song read
/// again per this many songs may wait, so a song read again costs at most some 400 KB of
/// writing, and the minute's save bounds the loss in time.
const SONGS_PER_REREAD: usize = 4000;

/// How many songs read again may wait before an index of `songs` songs is saved: as many as a
/// crash would lose, and then read once more.
fn rereads_per_save(songs: usize) -> usize {
    (songs / SONGS_PER_REREAD).max(REREADS_PER_SAVE)
}

struct Inner {
    index: TransitionIndex,
    dirty: bool,
    last_flush: DateTime<Utc>,
    /// Songs read again for a newer revision since the index was last saved.
    rereads: usize,
}

pub struct TransitionStore {
    dir: PathBuf,
    index_path: PathBuf,
    inner: Mutex<Inner>,
    /// Held from the snapshot to the file move, so an older snapshot never lands last.
    file_lock: Mutex<()>,
}

impl TransitionStore {
    /// The store in `dir` (`<dir>/transition-profiles.json` and `<dir>/transition-profiles/`).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let index_path = dir.join("transition-profiles.json");
        let index = load(&index_path);
        TransitionStore {
            dir: dir.join("transition-profiles"),
            index_path,
            inner: Mutex::new(Inner {
                index,
                dirty: false,
                last_flush: datetime::min_value(),
                rereads: 0,
            }),
            file_lock: Mutex::new(()),
        }
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    /// The analysis version octo-sonic writes, when it has said.
    pub fn current_version(&self) -> Option<i32> {
        Some(self.inner.lock().index.version).filter(|&v| v > 0)
    }

    /// What octo-sonic said it writes now; profiles of other versions are no longer served.
    /// Revisions count within a version, so a new version has had no pass at any of them.
    pub fn set_current_version(&self, version: i32) {
        let mut inner = self.inner.lock();
        if inner.index.version != version {
            inner.index.version = version;
            inner.index.revision = FIRST_REVISION;
            inner.dirty = true;
        }
    }

    /// The revision the last complete pass over the songs read them at.
    pub fn revision(&self) -> i32 {
        self.inner.lock().index.revision
    }

    /// A pass over the songs that began at `revision` is complete.
    pub fn set_revision(&self, revision: i32) {
        let mut inner = self.inner.lock();
        if inner.index.revision != revision {
            inner.index.revision = revision;
            inner.dirty = true;
        }
    }

    /// Of the profiles of the version served, how many are at `revision` or above (`done`), out
    /// of those and the ones below it that `still_read` says will be read again (`total`).
    pub fn progress(
        &self,
        revision: i32,
        still_read: impl Fn(&str, &TransitionEntry) -> bool,
    ) -> RevisionProgress {
        let inner = self.inner.lock();
        let version = inner.index.version;
        let (mut done, mut total) = (0, 0);
        for (key, entry) in inner.index.songs.iter().filter(|(_, e)| e.version == version) {
            if entry.revision >= revision {
                done += 1;
                total += 1;
            } else if still_read(key, entry) {
                total += 1;
            }
        }
        RevisionProgress { done, total }
    }

    /// A song read again for a newer revision was kept, its profile or its failure: once a few
    /// wait, the index is saved, so a crash loses no more of them than that.
    pub fn reread_kept(&self) {
        let due = {
            let mut inner = self.inner.lock();
            inner.rereads += 1;
            inner.rereads >= rereads_per_save(inner.index.songs.len())
        };
        if due {
            self.flush();
        }
    }

    /// The songs from outside the library, by key and file, whose profile comes from a revision
    /// below `revision`.
    pub fn outside_below(&self, revision: i32) -> Vec<(String, String)> {
        let inner = self.inner.lock();
        inner
            .index
            .songs
            .iter()
            .filter(|(_, e)| e.revision < revision)
            .filter_map(|(key, e)| Some((key.clone(), e.path.clone()?)))
            .collect()
    }

    /// A reading for its profile alone of a song that has an entry failed: its file was at
    /// `stamp`, it was read for `revision`. The profile it has is kept and served.
    pub fn record_failure(&self, key: &str, stamp: &str, revision: i32, at_utc: DateTime<Utc>) {
        if !self.inner.lock().index.songs.contains_key(key) {
            return;
        }
        self.write(|index| {
            if let Some(entry) = index.songs.get_mut(key) {
                entry.failed = Some(TransitionFailure {
                    stamp: stamp.to_string(),
                    revision,
                    at_utc,
                });
            }
        });
    }

    pub fn entry(&self, key: &str) -> Option<TransitionEntry> {
        self.inner.lock().index.songs.get(key).cloned()
    }

    /// How many songs have a profile of the current version.
    pub fn ready_count(&self) -> usize {
        let inner = self.inner.lock();
        let version = inner.index.version;
        inner
            .index
            .songs
            .values()
            .filter(|e| e.version == version)
            .count()
    }

    /// Keeps a song's profile, of the version and revision it says (a profile that does not say
    /// its revision is of the first). False (with a warning) when its file could not be written.
    pub fn put(&self, key: &str, stamp: &str, path: Option<&str>, profile: &Value) -> bool {
        let number = |name: &str| {
            profile
                .get(name)
                .and_then(Value::as_i64)
                .and_then(|v| i32::try_from(v).ok())
        };
        let version = number("version").unwrap_or(0);
        let revision = number("revision").filter(|&r| r > 0).unwrap_or(FIRST_REVISION);
        let file = self.file_of(key);
        if let Err(e) = state_file::save_atomic(&file, &profile.to_string()) {
            warn!("Could not save {}: {e}", file.display());
            return false;
        }
        self.write(|index| {
            index.songs.insert(
                key.to_string(),
                TransitionEntry {
                    stamp: stamp.to_string(),
                    version,
                    revision,
                    path: path.map(str::to_string),
                    failed: None,
                },
            );
        });
        true
    }

    /// The song's profile when there is one of the analysis version octo-sonic writes now.
    pub fn lookup(&self, key: &str) -> TransitionLookup {
        let current = {
            let inner = self.inner.lock();
            match inner.index.songs.get(key) {
                Some(entry) if inner.index.version == 0 || entry.version == inner.index.version => {
                    entry.version
                }
                _ => return TransitionLookup::NotReady,
            }
        };
        let read = state_file::read_text(&self.file_of(key))
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        match read {
            Some(profile @ Value::Object(_))
                if profile.get("version").and_then(Value::as_i64) == Some(i64::from(current)) =>
            {
                TransitionLookup::Ready(profile)
            }
            _ => TransitionLookup::NotReady,
        }
    }

    /// Forgets the library songs not in `present`, and the outside songs whose file is gone.
    pub fn keep_only(&self, present: &HashSet<String>) {
        let mut gone = Vec::new();
        self.write(|index| {
            index.songs.retain(|key, entry| {
                let keep = match &entry.path {
                    Some(path) => Path::new(path).is_file(),
                    None => present.contains(key),
                };
                if !keep {
                    gone.push(key.clone());
                }
                keep
            });
        });
        for key in gone {
            let _ = std::fs::remove_file(self.file_of(&key));
        }
    }

    /// Forgets every profile.
    pub fn clear(&self) {
        let keys: Vec<String> = self.inner.lock().index.songs.keys().cloned().collect();
        self.write(|index| index.songs.clear());
        for key in keys {
            let _ = std::fs::remove_file(self.file_of(&key));
        }
        self.flush();
    }

    fn write(&self, write: impl FnOnce(&mut TransitionIndex)) {
        let due = {
            let mut inner = self.inner.lock();
            write(&mut inner.index);
            inner.dirty = true;
            Utc::now() - inner.last_flush >= FLUSH_EVERY
        };
        if due {
            self.flush();
        }
    }

    pub fn flush(&self) {
        let _file = self.file_lock.lock();
        let snapshot = {
            let mut inner = self.inner.lock();
            if !inner.dirty {
                return;
            }
            inner.dirty = false;
            inner.last_flush = Utc::now();
            inner.rereads = 0;
            inner.index.clone()
        };
        if let Err(e) = state_file::save_atomic(&self.index_path, &octo_core::json::to_string(&snapshot)) {
            warn!("Could not save {}: {e}", self.index_path.display());
            self.inner.lock().dirty = true;
        }
    }

    /// Whatever changed since the last write goes down on the way out.
    pub async fn run_flusher(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        stopping.cancelled().await;
        self.flush();
        Ok(())
    }

    // A song's file, named for its key so any id is a safe name, in one of 256 folders.
    fn file_of(&self, key: &str) -> PathBuf {
        let name = hex::encode(Sha256::digest(key.as_bytes()));
        self.dir.join(&name[..2]).join(format!("{}.json", &name[2..32]))
    }
}

/// The index as the store starts from it; a file that cannot be read starts over, with a
/// warning.
fn load(path: &Path) -> TransitionIndex {
    let read = || -> anyhow::Result<TransitionIndex> {
        let Some(text) = state_file::read_text(path)? else {
            return Ok(TransitionIndex::default());
        };
        Ok(serde_json::from_str::<Option<TransitionIndex>>(&text)?.unwrap_or_default())
    };
    read().unwrap_or_else(|e| {
        warn!("{} could not be read; starting over: {e}", path.display());
        TransitionIndex::default()
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn profile(version: i32) -> Value {
        json!({ "version": version, "durationMs": 200_000, "head": {}, "tail": {} })
    }

    #[test]
    fn a_kept_profile_is_served_until_the_analysis_moves_on() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TransitionStore::new(dir.path());
        assert_eq!(store.lookup("nd-1"), TransitionLookup::NotReady);
        store.set_current_version(1);
        assert!(store.put("nd-1", "10:20", None, &profile(1)));
        assert_eq!(store.lookup("nd-1"), TransitionLookup::Ready(profile(1)));
        assert_eq!(store.ready_count(), 1);
        // octo-sonic now writes version 2: the old profile is stale until read again.
        store.set_current_version(2);
        assert_eq!(store.lookup("nd-1"), TransitionLookup::NotReady);
        assert_eq!(store.ready_count(), 0);
        store.put("nd-1", "10:20", None, &profile(2));
        assert_eq!(store.lookup("nd-1"), TransitionLookup::Ready(profile(2)));
    }

    #[test]
    fn what_was_kept_is_there_after_a_restart_and_a_broken_index_starts_over() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let first = TransitionStore::new(dir.path());
        first.set_current_version(1);
        first.put("nd-1", "10:20", None, &profile(1));
        first.put("youtube:abc", "5:6", Some("/music/a.mp3"), &profile(1));
        first.flush();
        let again = TransitionStore::new(dir.path());
        assert_eq!(again.current_version(), Some(1));
        assert_eq!(again.entry("nd-1").map(|e| e.stamp), Some("10:20".to_string()));
        assert_eq!(
            again.entry("youtube:abc").and_then(|e| e.path),
            Some("/music/a.mp3".to_string())
        );
        assert_eq!(again.lookup("nd-1"), TransitionLookup::Ready(profile(1)));

        std::fs::write(again.index_path(), "{ not json").expect("writes");
        let broken = TransitionStore::new(dir.path());
        assert_eq!(broken.lookup("nd-1"), TransitionLookup::NotReady);
        assert_eq!(broken.current_version(), None);
    }

    #[test]
    fn a_missing_or_damaged_profile_file_is_not_ready() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TransitionStore::new(dir.path());
        store.set_current_version(1);
        store.put("nd-1", "1:1", None, &profile(1));
        std::fs::write(store.file_of("nd-1"), "{ half").expect("writes");
        assert_eq!(store.lookup("nd-1"), TransitionLookup::NotReady);
        std::fs::remove_file(store.file_of("nd-1")).expect("removes");
        assert_eq!(store.lookup("nd-1"), TransitionLookup::NotReady);
    }

    #[test]
    fn gone_songs_are_forgotten_with_their_files() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TransitionStore::new(dir.path());
        store.set_current_version(1);
        let landed = dir.path().join("landed.mp3");
        std::fs::write(&landed, b"x").expect("writes");
        let landed = landed.to_string_lossy().into_owned();
        store.put("nd-keep", "1:1", None, &profile(1));
        store.put("nd-gone", "1:1", None, &profile(1));
        store.put("youtube:here", "1:1", Some(&landed), &profile(1));
        store.put("youtube:gone", "1:1", Some("/nowhere/x.mp3"), &profile(1));
        let gone_file = store.file_of("nd-gone");
        store.keep_only(&HashSet::from(["nd-keep".to_string()]));
        assert!(store.entry("nd-keep").is_some() && store.entry("youtube:here").is_some());
        assert!(store.entry("nd-gone").is_none() && store.entry("youtube:gone").is_none());
        assert!(!gone_file.exists());
        store.clear();
        assert_eq!(store.lookup("nd-keep"), TransitionLookup::NotReady);
    }

    fn profile_at(version: i32, revision: i32) -> Value {
        json!({ "version": version, "revision": revision, "durationMs": 200_000, "head": {}, "tail": {} })
    }

    #[test]
    fn a_new_index_and_one_from_before_revisions_start_at_the_first() {
        assert_eq!(TransitionIndex::default().revision, FIRST_REVISION);
        assert_eq!(TransitionEntry::default().revision, FIRST_REVISION);
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(TransitionStore::new(dir.path()).revision(), 1);
        let index = dir.path().join("transition-profiles.json");
        std::fs::write(
            &index,
            r#"{"Version":1,"Songs":{"nd-1":{"Stamp":"1:1","Version":1,"Path":null}}}"#,
        )
        .expect("writes");
        let old = TransitionStore::new(dir.path());
        assert_eq!(old.revision(), 1);
        assert_eq!(old.entry("nd-1").map(|e| e.revision), Some(1));
        std::fs::write(&index, "null").expect("writes");
        assert_eq!(TransitionStore::new(dir.path()).revision(), 1);
    }

    #[test]
    fn a_profiles_revision_is_kept_and_a_lower_one_is_served_until_read_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TransitionStore::new(dir.path());
        store.set_current_version(1);
        store.put("nd-old", "1:1", None, &profile(1));
        store.put("nd-new", "1:1", None, &profile_at(1, 2));
        assert_eq!(store.entry("nd-old").map(|e| e.revision), Some(1));
        assert_eq!(store.entry("nd-new").map(|e| e.revision), Some(2));
        // The version is what decides serving: revision 1 is read the same way.
        assert_eq!(store.lookup("nd-old"), TransitionLookup::Ready(profile(1)));
        let all = |_: &str, _: &TransitionEntry| true;
        assert_eq!(store.progress(2, all), RevisionProgress { done: 1, total: 2 });
        assert_eq!(store.progress(1, all), RevisionProgress { done: 2, total: 2 });

        store.set_revision(2);
        store.flush();
        let again = TransitionStore::new(dir.path());
        assert_eq!(again.revision(), 2);
        assert_eq!(again.entry("nd-new").map(|e| e.revision), Some(2));
        // A new version has had no pass at any revision.
        again.set_current_version(2);
        assert_eq!(again.revision(), 1);
        assert_eq!(again.progress(2, all), RevisionProgress { done: 0, total: 0 });
    }

    #[test]
    fn the_progress_counts_a_lower_profile_only_while_it_will_be_read_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TransitionStore::new(dir.path());
        store.set_current_version(1);
        store.put("nd-done", "1:1", None, &profile_at(1, 2));
        store.put("nd-left", "1:1", None, &profile(1));
        store.put("nd-gone", "1:1", None, &profile(1));
        store.put("youtube:gone", "1:1", Some("/nowhere/x.mp3"), &profile(1));
        let left = |key: &str, _: &TransitionEntry| key == "nd-left";
        assert_eq!(store.progress(2, left), RevisionProgress { done: 1, total: 2 });
        let asked = std::cell::RefCell::new(Vec::new());
        store.progress(2, |key, entry| {
            asked.borrow_mut().push((key.to_string(), entry.revision));
            true
        });
        // Asked only of the profiles below the revision.
        let mut asked = asked.into_inner();
        asked.sort();
        assert_eq!(
            asked,
            [
                ("nd-gone".to_string(), 1),
                ("nd-left".to_string(), 1),
                ("youtube:gone".to_string(), 1)
            ]
        );
    }

    #[test]
    fn songs_read_again_are_saved_every_few() {
        assert_eq!(rereads_per_save(0), 5);
        assert_eq!(rereads_per_save(20_000), 5);
        assert_eq!(rereads_per_save(40_000), 10);
        assert_eq!(rereads_per_save(200_000), 50);

        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TransitionStore::new(dir.path());
        store.set_current_version(1);
        let ids: Vec<String> = (0..6).map(|i| format!("nd-{i}")).collect();
        for id in &ids {
            store.put(id, "1:1", None, &profile(1));
        }
        store.flush();
        let saved = |revision: i32| {
            let on_disk = TransitionStore::new(dir.path());
            ids.iter()
                .filter(|id| on_disk.entry(id).is_some_and(|e| e.revision == revision))
                .count()
        };
        for id in &ids[..4] {
            store.put(id, "1:1", None, &profile_at(1, 2));
            store.reread_kept();
        }
        // Within the minute, four wait in memory.
        assert_eq!(saved(2), 0);
        store.put(&ids[4], "1:1", None, &profile_at(1, 2));
        store.reread_kept();
        assert_eq!(saved(2), 5);
        // The count starts over at each save.
        store.put(&ids[5], "1:1", None, &profile_at(1, 2));
        store.reread_kept();
        assert_eq!(saved(2), 5);
    }

    #[test]
    fn outside_songs_below_a_revision_are_listed_with_their_files() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TransitionStore::new(dir.path());
        store.set_current_version(1);
        store.put("nd-1", "1:1", None, &profile(1));
        store.put("youtube:old", "1:1", Some("/music/old.mp3"), &profile(1));
        store.put("youtube:new", "1:1", Some("/music/new.mp3"), &profile_at(1, 2));
        assert_eq!(
            store.outside_below(2),
            [("youtube:old".to_string(), "/music/old.mp3".to_string())]
        );
        assert!(store.outside_below(1).is_empty());
    }

    #[test]
    fn a_failed_outside_reading_is_kept_on_its_entry_until_it_is_read() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TransitionStore::new(dir.path());
        store.set_current_version(1);
        let landed = dir.path().join("landed.mp3");
        std::fs::write(&landed, b"x").expect("writes");
        let landed = landed.to_string_lossy().into_owned();
        store.put("youtube:a", "1:1", Some(&landed), &profile(1));
        let at = Utc::now();
        store.record_failure("youtube:a", "1:1", 2, at);
        // Nothing to record it on.
        store.record_failure("youtube:none", "1:1", 2, at);
        assert!(store.entry("youtube:none").is_none());
        let failed = TransitionFailure {
            stamp: "1:1".into(),
            revision: 2,
            at_utc: at,
        };
        assert_eq!(
            store.entry("youtube:a").and_then(|e| e.failed),
            Some(failed.clone())
        );
        // The profile it has is still served, and a pass's end keeps the record.
        assert_eq!(store.lookup("youtube:a"), TransitionLookup::Ready(profile(1)));
        store.keep_only(&HashSet::new());
        store.flush();
        let again = TransitionStore::new(dir.path());
        let kept = again
            .entry("youtube:a")
            .and_then(|e| e.failed)
            .expect("still failed");
        assert_eq!((kept.stamp, kept.revision), (failed.stamp, failed.revision));
        assert!((kept.at_utc - at).abs() < TimeDelta::seconds(1));
        again.put("youtube:a", "1:1", Some(&landed), &profile_at(1, 2));
        assert_eq!(again.entry("youtube:a").and_then(|e| e.failed), None);
    }

    #[test]
    fn any_id_makes_a_safe_file_name() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TransitionStore::new(dir.path());
        let file = store.file_of("../../etc/passwd");
        assert!(file.starts_with(dir.path().join("transition-profiles")));
        assert_eq!(file.file_name().map(|n| n.len()), Some(35));
    }
}
