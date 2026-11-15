//! `octo-playlists/`: the Navidrome playlists Octo holds because they keep songs found online.
//! One file per playlist (`<id>.json`), the outside entries a replace took out
//! (`replaced-<id>-<revision>.json`), playlists being made (`creating-<n>.json`), and files kept
//! aside for playlists Navidrome stopped knowing (`gone/`).
//!
//! Every save takes a revision from the store's one counter. A playlist's files are written
//! only while its playlist lock is held, in an order a crash can't break: the new record first,
//! carrying the revision its main file gets (`ForRevision`), then the main file, which is the
//! commit point, then the delete of the previous record. Load drops any record whose main file
//! is older than it, so after a crash the disk holds a committed past state.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use octo_core::json::datetime;
use octo_core::soulseek::SoulseekRouting;
use parking_lot::{Mutex, MutexGuard};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::same_take::{TakeFacts, explicit_word};
use crate::services::family::store_file;
use crate::services::library::navidrome_playlist_api::SongFacts;
use crate::services::state_file;

/// The words the store's warnings name it by.
const AREA: &str = "Playlists";

/// The folder files of playlists Navidrome stopped knowing are kept in.
const GONE: &str = "gone";

const RECORD_PREFIX: &str = "replaced-";
const CREATING_PREFIX: &str = "creating-";

/// What a song was when it was added: enough to serve it while it streams.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SongSnapshot {
    pub title: String,
    pub artist: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    pub album: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub isrcs: Vec<String>,
    /// `explicit`, `clean`, or none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_art: Option<String>,
}

impl SongSnapshot {
    /// What [`super::same_take::same_take`] compares.
    pub fn take_facts(&self) -> TakeFacts {
        TakeFacts {
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            seconds: self.seconds,
            isrcs: self.isrcs.clone(),
            explicit_status: self.explicit_status.clone(),
        }
    }
}

impl From<&SongFacts> for SongSnapshot {
    fn from(facts: &SongFacts) -> Self {
        SongSnapshot {
            title: facts.title.clone(),
            artist: facts.artist.clone(),
            album: facts.album.clone(),
            seconds: facts.seconds,
            isrcs: facts.isrcs.clone(),
            explicit_status: explicit_word(facts.explicit_status.as_deref()).map(str::to_string),
            ..Default::default()
        }
    }
}

/// The registry routing a seed is, written in the routing's own shape.
mod shared_routing {
    use std::sync::Arc;

    use octo_core::soulseek::SoulseekRouting;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        value: &Option<Arc<SoulseekRouting>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.as_deref().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Arc<SoulseekRouting>>, D::Error> {
        Ok(Option::<SoulseekRouting>::deserialize(deserializer)?.map(Arc::new))
    }
}

/// One place in a held playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HeldEntry {
    /// The Navidrome song id, while the song is in the library.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_id: Option<String>,
    /// The outside id exactly as the client sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// The acquisition key of the outside song (`acquisition_tracker::key_of`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The registry id the outside song resolved to when it was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_id: Option<String>,
    /// A copy of the registry routing, used only if the registry's own file is lost.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "shared_routing")]
    pub seed: Option<Arc<SoulseekRouting>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song: Option<SongSnapshot>,
    #[serde(with = "datetime::utc")]
    pub added: DateTime<Utc>,
}

impl HeldEntry {
    /// Whether it streams: an outside song with no library copy.
    pub fn streams(&self) -> bool {
        self.outside_id.is_some() && self.library_id.is_none()
    }

    /// Whether the caps count it: it streams and the registry keeps it.
    pub fn counted(&self) -> bool {
        self.streams() && self.seed_id.is_some()
    }

    fn seed_pair(&self) -> Option<(String, Arc<SoulseekRouting>)> {
        Some((self.seed_id.clone()?, self.seed.clone()?))
    }
}

/// The registry ids `entries` keep (streamed entries with a seed), with their seeds, for
/// `ExternalIdRegistry::set_kept`.
pub fn kept_ids(entries: &[HeldEntry]) -> Vec<(String, SoulseekRouting)> {
    entries
        .iter()
        .filter(|entry| entry.streams())
        .filter_map(HeldEntry::seed_pair)
        .map(|(id, seed)| (id, (*seed).clone()))
        .collect()
}

/// Every seed `entries` carry, bound ones too, for the seed index.
pub fn seeds(entries: &[HeldEntry]) -> Vec<(String, Arc<SoulseekRouting>)> {
    entries.iter().filter_map(HeldEntry::seed_pair).collect()
}

/// Which half of a mirror write was running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WritePhase {
    Removing,
    Adding,
}

/// A write of the library part to Navidrome that Octo started, with its progress saved before
/// each call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PendingWrite {
    pub from: Vec<String>,
    pub target: Vec<String>,
    pub prefix: usize,
    pub phase: WritePhase,
    pub done: usize,
}

/// A playlist Octo holds beside Navidrome's copy of its library songs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HeldPlaylist {
    /// The Navidrome playlist id.
    pub id: String,
    /// The owner's Navidrome username.
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub entries: Vec<HeldEntry>,
    /// The library ids Navidrome held, in order, at the last sync.
    #[serde(default)]
    pub mirror: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingWrite>,
    /// Moves only when the served list changes.
    #[serde(with = "datetime::utc")]
    pub changed: DateTime<Utc>,
    /// One admin read said 404.
    #[serde(default)]
    pub gone_seen: bool,
    #[serde(default)]
    pub revision: u64,
}

impl HeldPlaylist {
    /// Whether an entry streams.
    pub fn streams(&self) -> bool {
        self.entries.iter().any(HeldEntry::streams)
    }

    /// Whether Octo still holds it: an entry streams or a write is pending.
    pub fn holds(&self) -> bool {
        self.streams() || self.pending.is_some()
    }

    /// How many of its entries the caps count.
    pub fn counted(&self) -> usize {
        self.entries.iter().filter(|entry| entry.counted()).count()
    }

    /// The library part, in order.
    pub fn library_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|entry| entry.library_id.clone())
            .collect()
    }
}

/// The outside entries a replace took out of its playlist, kept so a split replace can take
/// them back. Never holds the playlist.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ReplacedRecord {
    pub playlist: String,
    #[serde(default)]
    pub by_user: String,
    #[serde(default)]
    pub by_client: String,
    #[serde(default)]
    pub entries: Vec<HeldEntry>,
    #[serde(default)]
    pub revision: u64,
    /// The revision the playlist's main file got in the save that wrote this record.
    #[serde(default)]
    pub for_revision: u64,
    #[serde(default)]
    pub gone_seen: bool,
}

/// A playlist being made with an outside song, written before Navidrome is asked to make it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Creating {
    pub owner: String,
    pub name: String,
    #[serde(default)]
    pub entries: Vec<HeldEntry>,
    #[serde(with = "datetime::utc")]
    pub started: DateTime<Utc>,
    /// The owner's playlists of the same name before the relay.
    #[serde(default)]
    pub before: Vec<String>,
}

/// One playlist's held state and record, as one operation reads and leaves them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistState {
    /// None when Octo does not hold the playlist.
    pub held: Option<HeldPlaylist>,
    pub replaced: Option<ReplacedRecord>,
}

/// A save that did not finish.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SaveError {
    /// Nothing was committed: the caller restores its copy ([`HeldStore::restore`]) before it
    /// answers anyone.
    #[error("{0}")]
    BeforeCommit(String),
    /// The change is committed; a file left behind is written by the next save or sync of the
    /// playlist ([`HeldStore::sync_files`]).
    #[error("{0}")]
    AfterCommit(String),
}

thread_local! {
    static HELD_STORE_LOCKS: Cell<u32> = const { Cell::new(0) };
}

/// Whether this thread holds a held store's mutex now. No registry call and no Navidrome call
/// is made while it does.
pub fn store_mutex_held() -> bool {
    HELD_STORE_LOCKS.with(|held| held.get() > 0)
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileOp {
    Write,
    Remove,
}

/// Called with every file the store writes or deletes; true makes that call fail.
#[cfg(test)]
pub(crate) type WriteHook = Arc<dyn Fn(&Path, FileOp) -> bool + Send + Sync>;

#[derive(Default)]
struct Inner {
    playlists: HashMap<String, HeldPlaylist>,
    replaced: HashMap<String, ReplacedRecord>,
    creating: BTreeMap<u64, Creating>,
    next_revision: u64,
    next_creating: u64,
    /// Playlists whose files may not match memory: a failure after a commit point, or a cleanup
    /// that did not run.
    needs_save: HashSet<String>,
}

impl Inner {
    fn take_revision(&mut self) -> u64 {
        let revision = self.next_revision.max(1);
        self.next_revision = revision + 1;
        revision
    }
}

/// The store's mutex, held: marks this thread for [`store_mutex_held`].
struct Locked<'a>(MutexGuard<'a, Inner>);

impl Deref for Locked<'_> {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut Inner {
        &mut self.0
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        HELD_STORE_LOCKS.with(|held| held.set(held.get().saturating_sub(1)));
    }
}

/// The store behind `octo-playlists/`.
pub struct HeldStore {
    dir: Option<PathBuf>,
    inner: Mutex<Inner>,
    load_warnings: Vec<String>,
    #[cfg(test)]
    write_hook: Mutex<Option<WriteHook>>,
}

impl HeldStore {
    pub fn in_memory() -> Self {
        Self::new(None::<PathBuf>)
    }

    /// The store kept in the folder `dir` (read now), or in memory only when it is absent or
    /// blank.
    pub fn new(dir: Option<impl AsRef<Path>>) -> Self {
        let dir = store_file::clean_path(dir);
        let (inner, load_warnings) = match &dir {
            Some(dir) => load(dir),
            None => (Inner::default(), Vec::new()),
        };
        HeldStore {
            dir,
            inner: Mutex::new(inner),
            load_warnings,
            #[cfg(test)]
            write_hook: Mutex::new(None),
        }
    }

    /// Files that could not be read and were kept aside, or could not be cleaned up.
    pub fn load_warnings(&self) -> &[String] {
        &self.load_warnings
    }

    fn lock(&self) -> Locked<'_> {
        let guard = self.inner.lock();
        HELD_STORE_LOCKS.with(|held| held.set(held.get() + 1));
        Locked(guard)
    }

    pub fn get(&self, id: &str) -> Option<HeldPlaylist> {
        self.lock().playlists.get(id).cloned()
    }

    pub fn is_held(&self, id: &str) -> bool {
        self.lock().playlists.contains_key(id)
    }

    /// Whether no playlist is held.
    pub fn is_empty(&self) -> bool {
        self.lock().playlists.is_empty()
    }

    /// Every held playlist's id, sorted; no playlist is copied.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().playlists.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Whether the playlist has a `Replaced` record.
    pub fn has_record(&self, id: &str) -> bool {
        self.lock().replaced.contains_key(id)
    }

    /// Every held playlist, by id.
    pub fn list(&self) -> Vec<HeldPlaylist> {
        let mut all: Vec<HeldPlaylist> = self.lock().playlists.values().cloned().collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    }

    pub fn replaced(&self, id: &str) -> Option<ReplacedRecord> {
        self.lock().replaced.get(id).cloned()
    }

    /// Every record, by playlist.
    pub fn records(&self) -> Vec<ReplacedRecord> {
        let mut all: Vec<ReplacedRecord> = self.lock().replaced.values().cloned().collect();
        all.sort_by(|a, b| a.playlist.cmp(&b.playlist));
        all
    }

    /// A revision from the store's one counter, above every revision on disk.
    pub fn take_revision(&self) -> u64 {
        self.lock().take_revision()
    }

    pub fn state(&self, id: &str) -> PlaylistState {
        let inner = self.lock();
        PlaylistState {
            held: inner.playlists.get(id).cloned(),
            replaced: inner.replaced.get(id).cloned(),
        }
    }

    /// Puts `next` in memory for `id` under the store mutex, taking a revision for it: the held
    /// state gets it, and so does a record that differs from the one held now (with
    /// `ForRevision` set to it). Hands back the state it replaced (for [`Self::restore`]) and the
    /// state stored (for [`Self::save`]). Call while holding the playlist's lock.
    pub fn swap(&self, id: &str, next: PlaylistState) -> (PlaylistState, PlaylistState) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let before = PlaylistState {
            held: inner.playlists.get(id).cloned(),
            replaced: inner.replaced.get(id).cloned(),
        };
        let revision = inner.take_revision();
        let mut stored = next;
        if let Some(playlist) = &mut stored.held {
            playlist.id = id.to_string();
            playlist.revision = revision;
        }
        if stored.replaced != before.replaced
            && let Some(record) = &mut stored.replaced
        {
            record.playlist = id.to_string();
            record.revision = revision;
            record.for_revision = revision;
        }
        put(inner, id, &stored);
        (before, stored)
    }

    /// Puts `before` back in memory after a save failed before its commit point.
    pub fn restore(&self, id: &str, before: PlaylistState) {
        let mut guard = self.lock();
        put(&mut guard, id, &before);
    }

    /// Writes `id`'s files to go from `before` to `stored` (from [`Self::swap`]): a new record
    /// first, then the main file (written, or deleted on a release), the commit point, then the
    /// previous record's delete. Call while holding the playlist's lock.
    pub fn save(&self, id: &str, before: &PlaylistState, stored: &PlaylistState) -> Result<(), SaveError> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let record_changed = before.replaced != stored.replaced;
        let new_record = stored.replaced.as_ref().filter(|_| record_changed);
        if let Some(record) = new_record {
            self.write(&record_path(dir, record), record)
                .map_err(SaveError::BeforeCommit)?;
        }
        let main = main_path(dir, id);
        let committed = match (&before.held, &stored.held) {
            (_, Some(playlist)) if before.held.as_ref() != Some(playlist) => self.write(&main, playlist),
            (Some(_), None) => self.remove(&main),
            _ => Ok(()),
        };
        if let Err(e) = committed {
            if let Some(record) = new_record
                && self.remove(&record_path(dir, record)).is_err()
            {
                // Load drops it (its main file is older); a sync of the files drops it sooner.
                self.lock().needs_save.insert(id.to_string());
            }
            return Err(SaveError::BeforeCommit(e));
        }
        if record_changed
            && let Some(previous) = &before.replaced
            && let Err(e) = self.remove(&record_path(dir, previous))
        {
            self.lock().needs_save.insert(id.to_string());
            return Err(SaveError::AfterCommit(e));
        }
        Ok(())
    }

    /// A save of `playlist` alone (its record unchanged), such as a mirror write's progress:
    /// memory is restored when it fails before its commit point. Hands back what was stored.
    /// Call while holding the playlist's lock.
    pub fn save_progress(&self, playlist: HeldPlaylist) -> Result<HeldPlaylist, SaveError> {
        let id = playlist.id.clone();
        let replaced = self.replaced(&id);
        let (before, stored) = self.swap(
            &id,
            PlaylistState {
                held: Some(playlist),
                replaced,
            },
        );
        match self.save(&id, &before, &stored) {
            Ok(()) => Ok(stored.held.unwrap_or_default()),
            Err(SaveError::BeforeCommit(e)) => {
                self.restore(&id, before);
                Err(SaveError::BeforeCommit(e))
            }
            Err(e) => Err(e),
        }
    }

    /// Playlists whose files may not match memory, for a flush pass.
    pub fn needs_save(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().needs_save.iter().cloned().collect();
        ids.sort();
        ids
    }

    /// Makes `id`'s files match memory: its record, then its main file (or its delete), then the
    /// delete of every other record file of it. Call while holding the playlist's lock.
    pub fn sync_files(&self, id: &str) -> Result<(), SaveError> {
        let Some(dir) = &self.dir else {
            self.lock().needs_save.remove(id);
            return Ok(());
        };
        let state = self.state(id);
        let keep = state.replaced.as_ref().map(|record| record_path(dir, record));
        if let (Some(record), Some(path)) = (&state.replaced, &keep) {
            self.write(path, record).map_err(SaveError::BeforeCommit)?;
        }
        let main = main_path(dir, id);
        let written = match &state.held {
            Some(playlist) => self.write(&main, playlist),
            None => self.remove(&main),
        };
        written.map_err(SaveError::BeforeCommit)?;
        for path in record_files(dir, id) {
            if keep.as_ref() != Some(&path) {
                self.remove(&path).map_err(SaveError::AfterCommit)?;
            }
        }
        self.lock().needs_save.remove(id);
        Ok(())
    }

    /// Every intent still on disk, the oldest started first.
    pub fn intents(&self) -> Vec<(u64, Creating)> {
        let mut all: Vec<(u64, Creating)> = self
            .lock()
            .creating
            .iter()
            .map(|(n, intent)| (*n, intent.clone()))
            .collect();
        all.sort_by(|a, b| a.1.started.cmp(&b.1.started).then(a.0.cmp(&b.0)));
        all
    }

    /// Writes an intent and hands back its number; memory changes only when the file was written.
    pub fn add_intent(&self, intent: Creating) -> anyhow::Result<u64> {
        let n = {
            let mut inner = self.lock();
            let n = inner.next_creating;
            inner.next_creating += 1;
            n
        };
        if let Some(dir) = &self.dir {
            self.write(&creating_path(dir, n), &intent)
                .map_err(anyhow::Error::msg)?;
        }
        self.lock().creating.insert(n, intent);
        Ok(n)
    }

    pub fn drop_intent(&self, n: u64) -> anyhow::Result<()> {
        if let Some(dir) = &self.dir {
            self.remove(&creating_path(dir, n)).map_err(anyhow::Error::msg)?;
        }
        self.lock().creating.remove(&n);
        Ok(())
    }

    /// Moves `id`'s files (its main file and its records) into `gone/` and drops it from memory,
    /// handing back what it was. Call while holding the playlist's lock.
    pub fn move_to_gone(&self, id: &str) -> anyhow::Result<PlaylistState> {
        let state = self.state(id);
        if let Some(dir) = &self.dir {
            let gone = dir.join(GONE);
            std::fs::create_dir_all(&gone)?;
            let main = main_path(dir, id);
            let mut files = record_files(dir, id);
            if main.exists() {
                files.push(main);
            }
            for path in files {
                std::fs::rename(&path, gone.join(file_name(&path)))?;
            }
        }
        let mut inner = self.lock();
        inner.playlists.remove(id);
        inner.replaced.remove(id);
        inner.needs_save.remove(id);
        Ok(state)
    }

    /// The playlists with files kept aside in `gone/`.
    pub fn gone_ids(&self) -> Vec<String> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        let mut ids: Vec<String> = json_files(&dir.join(GONE))
            .iter()
            .filter_map(|path| gone_id(&file_name(path)))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// What `gone/` keeps for `id`: its held state and its newest record.
    pub fn read_gone(&self, id: &str) -> Option<PlaylistState> {
        let gone = self.dir.as_ref()?.join(GONE);
        let held = read_quiet::<HeldPlaylist>(&main_path(&gone, id));
        let replaced = record_files(&gone, id)
            .iter()
            .filter_map(|path| read_quiet::<ReplacedRecord>(path))
            .max_by_key(|record| record.revision);
        (held.is_some() || replaced.is_some()).then_some(PlaylistState { held, replaced })
    }

    /// Deletes what `gone/` keeps for `id`, once it was moved back and saved.
    pub fn forget_gone(&self, id: &str) -> anyhow::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let gone = dir.join(GONE);
        let mut files = record_files(&gone, id);
        files.push(main_path(&gone, id));
        for path in files {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn write<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), String> {
        #[cfg(test)]
        self.refused(path, FileOp::Write)?;
        let text = octo_core::json::to_string(value);
        store_file::blocking(|| state_file::save_atomic(path, &text))
            .map_err(|e| format!("{} could not be written: {e}", file_name(path)))
    }

    fn remove(&self, path: &Path) -> Result<(), String> {
        #[cfg(test)]
        self.refused(path, FileOp::Remove)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{} could not be deleted: {e}", file_name(path))),
        }
    }

    #[cfg(test)]
    fn refused(&self, path: &Path, op: FileOp) -> Result<(), String> {
        let hook = self.write_hook.lock().clone();
        match hook {
            Some(hook) if hook(path, op) => Err(format!("{} refused by a test hook", file_name(path))),
            _ => Ok(()),
        }
    }

    /// Makes every file write or delete the hook says true for fail; None stops it.
    #[cfg(test)]
    pub(crate) fn fail_writes_when(&self, hook: Option<WriteHook>) {
        *self.write_hook.lock() = hook;
    }

    /// Runs `work` while this thread holds the store's mutex, for the test that no registry or
    /// Navidrome call is made under it.
    #[cfg(test)]
    pub(crate) fn while_locked<R>(&self, work: impl FnOnce() -> R) -> R {
        let _locked = self.lock();
        work()
    }
}

fn put(inner: &mut Inner, id: &str, state: &PlaylistState) {
    match &state.held {
        Some(playlist) => {
            inner.playlists.insert(id.to_string(), playlist.clone());
        }
        None => {
            inner.playlists.remove(id);
        }
    }
    match &state.replaced {
        Some(record) => {
            inner.replaced.insert(id.to_string(), record.clone());
        }
        None => {
            inner.replaced.remove(id);
        }
    }
}

fn load(dir: &Path) -> (Inner, Vec<String>) {
    let mut inner = Inner::default();
    let mut warnings = Vec::new();
    let mut highest = 0u64;
    let mut records: Vec<(PathBuf, ReplacedRecord)> = Vec::new();
    for path in json_files(dir) {
        let name = file_name(&path);
        if name.starts_with(RECORD_PREFIX) {
            if let Some(record) = read_one::<ReplacedRecord>(&path, &mut warnings) {
                highest = highest.max(record.revision).max(record.for_revision);
                records.push((path, record));
            }
        } else if let Some(n) = creating_number(&name) {
            if let Some(intent) = read_one::<Creating>(&path, &mut warnings) {
                inner.next_creating = inner.next_creating.max(n + 1);
                inner.creating.insert(n, intent);
            }
        } else if let Some(playlist) =
            read_one::<HeldPlaylist>(&path, &mut warnings).filter(|playlist| !playlist.id.is_empty())
        {
            highest = highest.max(playlist.revision);
            inner.playlists.insert(playlist.id.clone(), playlist);
        }
    }
    // Kept-aside files count for the revisions too, so a moved-back file is never ahead of a
    // later save.
    for path in json_files(&dir.join(GONE)) {
        if file_name(&path).starts_with(RECORD_PREFIX) {
            if let Some(record) = read_quiet::<ReplacedRecord>(&path) {
                highest = highest.max(record.revision).max(record.for_revision);
            }
        } else if let Some(playlist) = read_quiet::<HeldPlaylist>(&path) {
            highest = highest.max(playlist.revision);
        }
    }
    // Newest first: a playlist keeps its newest committed record. Every other one is deleted: a
    // previous record whose delete a crash cut short, or a record whose main file was never
    // written (its main file is older than the record says).
    records.sort_by_key(|(_, record)| std::cmp::Reverse(record.revision));
    for (path, record) in records {
        let committed = inner
            .playlists
            .get(&record.playlist)
            .is_none_or(|playlist| playlist.revision >= record.for_revision);
        if committed && !inner.replaced.contains_key(&record.playlist) {
            inner.replaced.insert(record.playlist.clone(), record);
        } else if let Err(e) = std::fs::remove_file(&path) {
            warnings.push(format!("{} could not be deleted: {e}", file_name(&path)));
        }
    }
    inner.next_revision = highest + 1;
    (inner, warnings)
}

/// One file read with the family's rules: an unreadable one is kept aside with a warning.
fn read_one<T: DeserializeOwned>(path: &Path, warnings: &mut Vec<String>) -> Option<T> {
    let (value, warning) = store_file::load_in::<Option<T>>(Some(path), AREA);
    warnings.extend(warning);
    value
}

/// One file read as it is, nothing kept aside.
fn read_quiet<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let text = state_file::read_text(path).ok().flatten()?;
    serde_json::from_str(&text).ok()
}

/// The `.json` files directly in `dir`, by name.
fn json_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && file_name(path).ends_with(".json"))
        .collect();
    files.sort();
    files
}

fn main_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn record_path(dir: &Path, record: &ReplacedRecord) -> PathBuf {
    dir.join(format!(
        "{RECORD_PREFIX}{}-{}.json",
        record.playlist, record.revision
    ))
}

fn creating_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{CREATING_PREFIX}{n}.json"))
}

fn creating_number(name: &str) -> Option<u64> {
    name.strip_prefix(CREATING_PREFIX)?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

/// Every record file of `id` in `dir`.
fn record_files(dir: &Path, id: &str) -> Vec<PathBuf> {
    let prefix = format!("{RECORD_PREFIX}{id}-");
    json_files(dir)
        .into_iter()
        .filter(|path| file_name(path).starts_with(&prefix))
        .collect()
}

/// The playlist a file in `gone/` belongs to.
fn gone_id(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".json")?;
    match stem.strip_prefix(RECORD_PREFIX) {
        Some(rest) => rest.rsplit_once('-').map(|(id, _)| id.to_string()),
        None => Some(stem.to_string()),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    const ID: &str = "0AbCdEfGhIjKlMnOpQrStU";

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 14, 10, minute, 0)
            .single()
            .expect("a time")
    }

    fn streamed(outside: &str) -> HeldEntry {
        HeldEntry {
            library_id: None,
            outside_id: Some(outside.into()),
            provider: Some("deezer".into()),
            external_id: Some("1".into()),
            key: None,
            seed_id: None,
            seed: None,
            song: Some(SongSnapshot {
                title: "Song".into(),
                artist: "Artist".into(),
                seconds: Some(200),
                ..Default::default()
            }),
            added: at(0),
        }
    }

    fn library(id: &str) -> HeldEntry {
        HeldEntry {
            library_id: Some(id.into()),
            outside_id: None,
            provider: None,
            external_id: None,
            key: None,
            seed_id: None,
            seed: None,
            song: None,
            added: at(0),
        }
    }

    fn playlist(entries: Vec<HeldEntry>) -> HeldPlaylist {
        HeldPlaylist {
            id: ID.into(),
            owner: "alice".into(),
            mirror: entries.iter().filter_map(|e| e.library_id.clone()).collect(),
            entries,
            pending: None,
            changed: at(1),
            gone_seen: false,
            revision: 0,
        }
    }

    fn record(outside: &str) -> ReplacedRecord {
        ReplacedRecord {
            playlist: ID.into(),
            by_user: "alice".into(),
            by_client: "Symfonium".into(),
            entries: vec![streamed(outside)],
            revision: 0,
            for_revision: 0,
            gone_seen: false,
        }
    }

    fn held(playlist: HeldPlaylist, record: Option<ReplacedRecord>) -> PlaylistState {
        PlaylistState {
            held: Some(playlist),
            replaced: record,
        }
    }

    fn commit(store: &HeldStore, next: PlaylistState) -> PlaylistState {
        let (before, stored) = store.swap(ID, next);
        store.save(ID, &before, &stored).expect("saved");
        stored
    }

    fn names(dir: &Path) -> Vec<String> {
        json_files(dir).iter().map(|path| file_name(path)).collect()
    }

    fn main_name() -> String {
        format!("{ID}.json")
    }

    #[test]
    fn a_held_playlist_round_trips_with_its_seed_and_revision() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = HeldStore::new(Some(dir.path()));
        let mut entry = streamed("ext-deezer-song-1");
        entry.seed_id = Some("Seed0123456789012345ab".into());
        entry.seed = Some(Arc::new(SoulseekRouting {
            artist: Some("Artist".into()),
            title: Some("Song".into()),
            you_tube_id: Some("vid".into()),
            ..Default::default()
        }));
        let stored = commit(&store, held(playlist(vec![library("lib1"), entry]), None));

        let reopened = HeldStore::new(Some(dir.path()));
        assert_eq!(reopened.get(ID), stored.held);
        let revision = stored.held.as_ref().expect("held").revision;
        assert!(reopened.take_revision() > revision);
        let entries = reopened.get(ID).expect("held").entries;
        assert_eq!(kept_ids(&entries).len(), 1);
        assert_eq!(seeds(&entries).len(), 1);
        assert!(reopened.load_warnings().is_empty());
    }

    #[test]
    fn a_crash_between_the_record_and_the_main_file_loads_without_the_new_record() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = HeldStore::new(Some(dir.path()));
        let first = commit(
            &store,
            held(
                playlist(vec![library("lib1"), streamed("ext-deezer-song-1")]),
                Some(record("ext-deezer-song-9")),
            ),
        );
        // The next replace writes its record; the main file write fails, and so does the cleanup,
        // as a crash would leave it.
        store.fail_writes_when(Some(Arc::new(|path: &Path, op: FileOp| {
            let name = file_name(path);
            (op == FileOp::Write && name == main_name())
                || (op == FileOp::Remove && name.starts_with(RECORD_PREFIX))
        })));
        let (before, stored) = store.swap(
            ID,
            held(playlist(vec![library("lib1")]), Some(record("ext-deezer-song-1"))),
        );
        assert!(matches!(
            store.save(ID, &before, &stored),
            Err(SaveError::BeforeCommit(_))
        ));
        // The previous record is deleted only after the main file is written.
        let previous = record_path(dir.path(), first.replaced.as_ref().expect("a record"));
        let new_record = record_path(dir.path(), stored.replaced.as_ref().expect("a record"));
        assert!(previous.exists());
        assert!(new_record.exists());
        drop(store);

        let reopened = HeldStore::new(Some(dir.path()));
        assert_eq!(reopened.get(ID), first.held);
        assert_eq!(reopened.replaced(ID), first.replaced);
        assert!(
            !new_record.exists(),
            "load deletes the record its main file never got"
        );
    }

    #[test]
    fn a_committed_save_deletes_the_previous_record_after_the_main_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = HeldStore::new(Some(dir.path()));
        let first = commit(
            &store,
            held(
                playlist(vec![streamed("ext-deezer-song-1")]),
                Some(record("ext-deezer-song-9")),
            ),
        );
        let log: Arc<Mutex<Vec<(String, FileOp)>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let log = log.clone();
            store.fail_writes_when(Some(Arc::new(move |path: &Path, op: FileOp| {
                log.lock().push((file_name(path), op));
                false
            })));
        }
        let second = commit(
            &store,
            held(
                playlist(vec![streamed("ext-deezer-song-2")]),
                Some(record("ext-deezer-song-1")),
            ),
        );
        let first_name = file_name(&record_path(
            dir.path(),
            first.replaced.as_ref().expect("a record"),
        ));
        let second_name = file_name(&record_path(
            dir.path(),
            second.replaced.as_ref().expect("a record"),
        ));
        assert_eq!(
            log.lock().clone(),
            vec![
                (second_name.clone(), FileOp::Write),
                (main_name(), FileOp::Write),
                (first_name, FileOp::Remove),
            ]
        );
        let mut expected = vec![main_name(), second_name];
        expected.sort();
        assert_eq!(names(dir.path()), expected);
    }

    #[test]
    fn a_failure_after_the_commit_point_is_written_by_the_next_sync() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = HeldStore::new(Some(dir.path()));
        commit(
            &store,
            held(
                playlist(vec![streamed("ext-deezer-song-1")]),
                Some(record("ext-deezer-song-9")),
            ),
        );
        store.fail_writes_when(Some(Arc::new(|path: &Path, op: FileOp| {
            op == FileOp::Remove && file_name(path).starts_with(RECORD_PREFIX)
        })));
        let (before, stored) = store.swap(
            ID,
            held(
                playlist(vec![streamed("ext-deezer-song-2")]),
                Some(record("ext-deezer-song-1")),
            ),
        );
        assert!(matches!(
            store.save(ID, &before, &stored),
            Err(SaveError::AfterCommit(_))
        ));
        assert_eq!(store.needs_save(), vec![ID.to_string()]);
        assert_eq!(store.state(ID), stored, "a committed change is never rolled back");

        store.fail_writes_when(None);
        store.sync_files(ID).expect("synced");
        let mut expected = vec![
            main_name(),
            file_name(&record_path(
                dir.path(),
                stored.replaced.as_ref().expect("a record"),
            )),
        ];
        expected.sort();
        assert_eq!(names(dir.path()), expected);
        assert!(store.needs_save().is_empty());
    }

    #[test]
    fn a_failed_save_restores_memory_and_a_release_keeps_its_record() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = HeldStore::new(Some(dir.path()));
        let first = commit(
            &store,
            held(
                playlist(vec![streamed("ext-deezer-song-1")]),
                Some(record("ext-deezer-song-9")),
            ),
        );
        store.fail_writes_when(Some(Arc::new(|path: &Path, op: FileOp| {
            op == FileOp::Write && file_name(path) == main_name()
        })));
        let (before, stored) = store.swap(ID, held(playlist(vec![library("lib2")]), first.replaced.clone()));
        assert!(store.save(ID, &before, &stored).is_err());
        store.restore(ID, before.clone());
        assert_eq!(store.state(ID), before);
        assert!(store.save_progress(playlist(vec![library("lib3")])).is_err());
        assert_eq!(store.state(ID), before);
        store.fail_writes_when(None);

        // A release deletes the main file and keeps the record, which reloads.
        let released = commit(
            &store,
            PlaylistState {
                held: None,
                replaced: first.replaced.clone(),
            },
        );
        assert!(!main_path(dir.path(), ID).exists());
        let reopened = HeldStore::new(Some(dir.path()));
        assert!(!reopened.is_held(ID));
        assert_eq!(reopened.replaced(ID), released.replaced);
    }

    #[test]
    fn intents_survive_a_restart_oldest_started_first() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = HeldStore::new(Some(dir.path()));
        let intent = |minute: u32| Creating {
            owner: "alice".into(),
            name: "Gym".into(),
            entries: vec![streamed("ext-deezer-song-1")],
            started: at(minute),
            before: vec!["0ZzZzZzZzZzZzZzZzZzZzZ".into()],
        };
        let later = store.add_intent(intent(5)).expect("saved");
        let earlier = store.add_intent(intent(3)).expect("saved");

        let reopened = HeldStore::new(Some(dir.path()));
        let numbers: Vec<u64> = reopened.intents().into_iter().map(|(n, _)| n).collect();
        assert_eq!(numbers, vec![earlier, later]);
        reopened.drop_intent(earlier).expect("dropped");
        let again = HeldStore::new(Some(dir.path()));
        assert_eq!(again.intents().len(), 1);
        assert!(again.add_intent(intent(9)).expect("saved") > later);
    }

    #[test]
    fn a_gone_playlist_is_kept_aside_read_back_and_forgotten() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = HeldStore::new(Some(dir.path()));
        let first = commit(
            &store,
            held(
                playlist(vec![streamed("ext-deezer-song-1")]),
                Some(record("ext-deezer-song-9")),
            ),
        );
        let state = store.move_to_gone(ID).expect("moved");
        assert_eq!(state, first);
        assert!(!store.is_held(ID));
        assert!(store.replaced(ID).is_none());
        assert!(names(dir.path()).is_empty());
        assert_eq!(store.gone_ids(), vec![ID.to_string()]);
        assert_eq!(store.read_gone(ID), Some(first.clone()));

        // Revisions after a restart stay above the kept-aside files.
        let reopened = HeldStore::new(Some(dir.path()));
        assert!(!reopened.is_held(ID));
        assert!(reopened.take_revision() > first.held.as_ref().expect("held").revision);
        reopened.forget_gone(ID).expect("forgotten");
        assert!(reopened.gone_ids().is_empty());
        assert!(reopened.read_gone(ID).is_none());
    }

    #[test]
    fn the_store_mutex_is_marked_while_held() {
        let store = HeldStore::in_memory();
        assert!(!store_mutex_held());
        {
            let _held = store.lock();
            assert!(store_mutex_held());
        }
        assert!(!store_mutex_held());
    }
}
