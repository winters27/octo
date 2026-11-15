//! The versions people chose for songs found online: one record per song, by its artist and
//! title (a clean edit apart), kept in `stream-choices.json` beside the registry's file. Every
//! record written and every record removed takes the next number of one change sequence, a
//! removal leaving a tombstone, so Octo's apps learn which songs changed version since they last
//! asked and drop their saved copies of them. The file carries an epoch: a file found unreadable
//! is kept aside and a new epoch starts, which tells every app to drop every outside copy.
//!
//! The ids each song was played under are kept apart in `stream-plays.json` (at most
//! [`ChoiceCaps::ids`] a song and [`ChoiceCaps::songs`] songs, the least recently played out), so
//! a change names ids the registry no longer holds. The index is `lossy` once it lost an id: a
//! song put out, or a start after a stop that never wrote its clean mark.
//!
//! Lock order: the store's locks are taken last. Nothing else is locked while one is held, and
//! the store never takes a routing's or the registry's lock.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use octo_core::soulseek::{LengthSource, RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::ExternalIdRegistry;
use crate::services::family::store_file;

/// The words the store's messages name it by.
const AREA: &str = "Versions";

/// The version word of a clean edit.
const CLEAN: &str = "clean";

/// How far apart two lengths of one song may be, in seconds.
pub const LENGTH_TOLERANCE: i32 = 8;

/// What a version of a song is on YouTube.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamVersionKind {
    /// The release's own audio (YouTube Music's song row).
    #[default]
    Release,
    /// The official music video.
    MusicVideo,
    /// Any other upload.
    Video,
}

/// The version chosen for one song.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct StreamChoice {
    /// The chosen YouTube video.
    pub video: String,
    pub title: Option<String>,
    pub channel: Option<String>,
    /// The video's own length.
    pub seconds: Option<i32>,
    pub kind: StreamVersionKind,
    /// The song's catalog length when the choice was made, when one was known.
    pub song_seconds: Option<i32>,
    /// The Subsonic user who chose it; empty for Octo's own pick.
    pub by: String,
    pub at: DateTime<Utc>,
    /// Octo's own pick, written when a person's choice was undone.
    pub auto: bool,
    /// The change number this record was written with.
    pub seq: u64,
}

/// The changes after a cursor, by song key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangePage {
    pub epoch: String,
    /// The cursor to ask from next time: the last saved change.
    pub seq: u64,
    /// False when the caller has to drop every outside copy instead: another epoch, a cursor
    /// older than the tombstones kept, or more changes than one answer lists.
    pub complete: bool,
    /// Each change's number and song key, in order.
    pub changes: Vec<(u64, String)>,
}

/// [`ChangePage`] with the ids of each changed song, as the change feed answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeFeed {
    pub epoch: String,
    pub seq: u64,
    pub complete: bool,
    pub changes: Vec<(u64, Vec<String>)>,
}

/// How much the store keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChoiceCaps {
    /// Records; past it Octo's own picks go first, the least recently written first.
    pub records: usize,
    /// Tombstones; the oldest goes first and raises the floor.
    pub tombstones: usize,
    /// Songs in the played index; the least recently played goes first.
    pub songs: usize,
    /// Ids a song keeps in the played index; the least recently played goes first.
    pub ids: usize,
}

impl Default for ChoiceCaps {
    fn default() -> Self {
        ChoiceCaps {
            records: 20_000,
            tombstones: 2_000,
            songs: 50_000,
            ids: 8,
        }
    }
}

/// A removed record: its song key, its change number and the ids it applied to then.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Tombstone {
    key: String,
    seq: u64,
    ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct KeyedChoice {
    key: String,
    choice: StreamChoice,
}

/// `stream-choices.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ChoicesFile {
    epoch: String,
    seq: u64,
    saved_seq: u64,
    floor: u64,
    records: Vec<KeyedChoice>,
    tombstones: Vec<Tombstone>,
}

#[derive(Default)]
struct Choices {
    epoch: String,
    seq: u64,
    /// The highest change number a written file holds: the feed hands out none above it.
    saved_seq: u64,
    /// The highest change number a tombstone put out dropped.
    floor: u64,
    /// By song key, the least recently written first.
    records: IndexMap<String, StreamChoice>,
    tombstones: VecDeque<Tombstone>,
    /// A change not written yet (a save failed); the registry's flush writes it.
    dirty: bool,
}

impl Choices {
    fn file(&self) -> ChoicesFile {
        ChoicesFile {
            epoch: self.epoch.clone(),
            seq: self.seq,
            saved_seq: self.seq,
            floor: self.floor,
            records: self
                .records
                .iter()
                .map(|(key, choice)| KeyedChoice {
                    key: key.clone(),
                    choice: choice.clone(),
                })
                .collect(),
            tombstones: self.tombstones.iter().cloned().collect(),
        }
    }

    fn tombstone(&mut self, key: &str, ids: Vec<String>, cap: usize) {
        self.seq += 1;
        self.tombstones.push_back(Tombstone {
            key: key.to_string(),
            seq: self.seq,
            ids,
        });
        while self.tombstones.len() > cap {
            if let Some(dropped) = self.tombstones.pop_front() {
                self.floor = self.floor.max(dropped.seq);
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PlayedId {
    id: String,
    /// The routing's catalog length when it played.
    seconds: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PlayedSong {
    key: String,
    ids: Vec<PlayedId>,
}

/// `stream-plays.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PlaysFile {
    lossy: bool,
    /// Written true by the last write before a clean stop, false at every start.
    clean: bool,
    songs: Vec<PlayedSong>,
}

#[derive(Default)]
struct Plays {
    /// By song key, the least recently played first; each song's ids the newest last.
    songs: IndexMap<String, Vec<PlayedId>>,
    lossy: bool,
    clean: bool,
    dirty: bool,
}

impl Plays {
    fn file(&self) -> PlaysFile {
        PlaysFile {
            lossy: self.lossy,
            clean: self.clean,
            songs: self
                .songs
                .iter()
                .map(|(key, ids)| PlayedSong {
                    key: key.clone(),
                    ids: ids.clone(),
                })
                .collect(),
        }
    }
}

/// The chosen versions and the played index.
pub struct StreamChoices {
    choices_path: Option<PathBuf>,
    plays_path: Option<PathBuf>,
    caps: Mutex<ChoiceCaps>,
    state: Mutex<Choices>,
    plays: Mutex<Plays>,
    /// Held by a write of `stream-choices.json`, so writes go out in order.
    choices_saving: Mutex<()>,
    /// Held by a write of `stream-plays.json`.
    plays_saving: Mutex<()>,
}

impl StreamChoices {
    /// The store beside the registry's file at `registry_path`, loaded now; in memory only with
    /// no path.
    pub fn open(registry_path: Option<&Path>) -> Self {
        Self::open_with(registry_path, ChoiceCaps::default())
    }

    /// [`Self::open`] keeping at most `caps`.
    pub fn open_with(registry_path: Option<&Path>, caps: ChoiceCaps) -> Self {
        let store = StreamChoices {
            choices_path: registry_path.map(|path| path.with_file_name("stream-choices.json")),
            plays_path: registry_path.map(|path| path.with_file_name("stream-plays.json")),
            caps: Mutex::new(caps),
            state: Mutex::new(Choices::default()),
            plays: Mutex::new(Plays::default()),
            choices_saving: Mutex::new(()),
            plays_saving: Mutex::new(()),
        };
        store.load();
        store
    }

    /// Reads both files. A missing or unreadable choices file (kept aside by
    /// [`store_file::load_in`]) starts a new epoch, written at once. The played index is marked
    /// not cleanly stopped and written at once, so a crash from here on reads as one.
    fn load(&self) {
        let (file, _) = store_file::load_in::<ChoicesFile>(self.choices_path.as_deref(), AREA);
        let fresh = file.epoch.trim().is_empty();
        {
            let mut c = self.state.lock();
            if fresh {
                c.epoch = uuid::Uuid::new_v4().simple().to_string();
            } else {
                c.epoch = file.epoch;
                c.seq = file.seq;
                c.floor = file.floor;
                for KeyedChoice { key, choice } in file.records {
                    if key.is_empty() || choice.video.is_empty() {
                        continue;
                    }
                    c.seq = c.seq.max(choice.seq);
                    c.records.insert(key, choice);
                }
                c.tombstones = file
                    .tombstones
                    .into_iter()
                    .filter(|tombstone| !tombstone.key.is_empty())
                    .collect();
                let newest = c.tombstones.iter().map(|t| t.seq).max().unwrap_or(0);
                c.seq = c.seq.max(newest);
                c.saved_seq = c.seq;
            }
        }
        if fresh {
            self.save_choices();
        }

        let (plays, warning) = store_file::load_in::<Option<PlaysFile>>(self.plays_path.as_deref(), AREA);
        {
            let mut p = self.plays.lock();
            match plays {
                Some(file) => {
                    p.lossy = file.lossy || !file.clean;
                    for PlayedSong { key, ids } in file.songs {
                        if !key.is_empty() {
                            p.songs.insert(key, ids);
                        }
                    }
                }
                // No index yet is a first start; an unreadable one lost every id it had.
                None => p.lossy = warning.is_some(),
            }
            p.clean = false;
            p.dirty = true;
        }
        self.save_plays();
    }

    /// The key a song's record is filed under: its artist and title as
    /// [`ExternalIdRegistry::song_key`] reads them, with `\nclean` for a clean edit. None for
    /// anything but a song, or a song without both names.
    pub fn key_of(routing: &SoulseekRouting) -> Option<String> {
        if routing.kind != RoutingKind::Song {
            return None;
        }
        let key = ExternalIdRegistry::song_key(
            routing.artist.as_deref().unwrap_or(""),
            routing.title.as_deref().unwrap_or(""),
        )?;
        Some(if routing.version.as_deref() == Some(CLEAN) {
            format!("{key}\n{CLEAN}")
        } else {
            key
        })
    }

    /// The song's length as a catalog says it: its shown length when Last.fm or Deezer gave it.
    /// Never `duration` (search rows and lookups write a video's length there), nor a length a
    /// video or a file gave.
    pub fn catalog_seconds(routing: &SoulseekRouting) -> Option<i32> {
        matches!(
            routing.shown_duration_source,
            LengthSource::LastFm | LengthSource::Deezer
        )
        .then_some(routing.shown_duration)
        .flatten()
        .filter(|seconds| *seconds > 0)
    }

    /// Whether a choice fits a routing whose catalog length is `catalog`: with the song's length
    /// on the record, the routing's within [`LENGTH_TOLERANCE`] of it; without, within it of the
    /// chosen video's own length. A routing with no catalog length always fits.
    pub fn fits(choice: &StreamChoice, catalog: Option<i32>) -> bool {
        let Some(catalog) = catalog else {
            return true;
        };
        match choice.song_seconds.or(choice.seconds) {
            Some(anchor) => (catalog - anchor).abs() <= LENGTH_TOLERANCE,
            None => true,
        }
    }

    /// The one length every known length agrees on within [`LENGTH_TOLERANCE`] (the shortest of
    /// them), or None when none is known or two disagree.
    pub fn agreed_seconds(lengths: impl IntoIterator<Item = Option<i32>>) -> Option<i32> {
        let known: Vec<i32> = lengths.into_iter().flatten().filter(|s| *s > 0).collect();
        let (low, high) = (known.iter().min()?, known.iter().max()?);
        (high - low <= LENGTH_TOLERANCE).then_some(*low)
    }

    /// The record of the song under `key`.
    pub fn record_for_key(&self, key: &str) -> Option<StreamChoice> {
        self.state.lock().records.get(key).cloned()
    }

    /// The store's epoch.
    pub fn epoch(&self) -> String {
        self.state.lock().epoch.clone()
    }

    /// Stores `choice` as the record of the song under `key`, with the next change number, and
    /// writes the file at once (a failed write is retried by [`Self::flush`]; the record applies
    /// from memory meanwhile). Answers the record put out to make room, with its key.
    pub(crate) fn put(&self, key: &str, choice: StreamChoice) -> Option<(String, StreamChoice)> {
        let caps = *self.caps.lock();
        let evicted = {
            let mut c = self.state.lock();
            c.seq += 1;
            let choice = StreamChoice { seq: c.seq, ..choice };
            c.records.shift_remove(key);
            c.records.insert(key.to_string(), choice);
            if c.records.len() > caps.records {
                // Octo's own picks first (their routings take that very video at once), then the
                // least recently written person's choice. Never the record just written.
                let older = c.records.len() - 1;
                let at = c
                    .records
                    .values()
                    .take(older)
                    .position(|record| record.auto)
                    .unwrap_or(0);
                c.records.shift_remove_index(at)
            } else {
                None
            }
        };
        self.save_choices();
        evicted
    }

    /// Removes the record of the song under `key` when it chose `video` (any video with None),
    /// leaving a tombstone with `ids`, and writes the file at once. Answers the record removed.
    pub(crate) fn remove(&self, key: &str, video: Option<&str>, ids: Vec<String>) -> Option<StreamChoice> {
        let caps = *self.caps.lock();
        let removed = {
            let mut c = self.state.lock();
            let matches = c
                .records
                .get(key)
                .is_some_and(|record| video.is_none_or(|video| record.video == video));
            if !matches {
                return None;
            }
            let removed = c.records.shift_remove(key);
            c.tombstone(key, ids, caps.tombstones);
            removed
        };
        self.save_choices();
        removed
    }

    /// Leaves a tombstone for the song under `key` with no record removed (its routings let go of
    /// a chosen video no record named any more), and writes the file at once.
    pub(crate) fn note_removed(&self, key: &str, ids: Vec<String>) {
        let caps = *self.caps.lock();
        self.state.lock().tombstone(key, ids, caps.tombstones);
        self.save_choices();
    }

    /// The keys a song's records may be filed under by its names: its clean edit's and its own.
    pub(crate) fn keys_for_names(artist: &str, title: &str) -> Vec<String> {
        ExternalIdRegistry::song_key(artist, title)
            .map(|key| vec![format!("{key}\n{CLEAN}"), key])
            .unwrap_or_default()
    }

    /// The changes after `since` (everything kept with no `since`), at most `limit`, never past
    /// the last saved change. Incomplete for another epoch, a cursor below the floor, or more
    /// than `limit` changes.
    pub fn changes_since(&self, epoch: Option<&str>, since: Option<u64>, limit: usize) -> ChangePage {
        let c = self.state.lock();
        let after = since.unwrap_or(0);
        let mut changes: Vec<(u64, String)> = c
            .records
            .iter()
            .map(|(key, record)| (record.seq, key.clone()))
            .chain(c.tombstones.iter().map(|t| (t.seq, t.key.clone())))
            .filter(|(seq, _)| *seq > after && *seq <= c.saved_seq)
            .collect();
        changes.sort_unstable();
        let same_epoch = epoch.is_none_or(|asked| asked == c.epoch);
        let complete = same_epoch && after >= c.floor && changes.len() <= limit;
        changes.truncate(limit);
        ChangePage {
            epoch: c.epoch.clone(),
            seq: c.saved_seq,
            complete,
            changes,
        }
    }

    /// The ids the removed records of the song under `key` applied to.
    pub(crate) fn removed_ids(&self, key: &str) -> Vec<String> {
        let c = self.state.lock();
        let mut ids: Vec<String> = Vec::new();
        for tombstone in c.tombstones.iter().filter(|t| t.key == key) {
            for id in &tombstone.ids {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
        }
        ids
    }

    /// Notes that `id`, a routing of the song under `key` whose catalog length is `seconds`,
    /// started a play. The index is marked for writing only when its ids changed.
    pub(crate) fn note_played(&self, key: &str, id: &str, seconds: Option<i32>) {
        if key.is_empty() || id.is_empty() {
            return;
        }
        let caps = *self.caps.lock();
        let mut guard = self.plays.lock();
        let plays = &mut *guard;
        let mut changed = false;
        let at = match plays.songs.get_index_of(key) {
            Some(at) => {
                let last = plays.songs.len() - 1;
                plays.songs.move_index(at, last);
                last
            }
            None => {
                plays.songs.insert(key.to_string(), Vec::new());
                changed = true;
                plays.songs.len() - 1
            }
        };
        let ids = &mut plays.songs[at];
        match ids.iter().position(|known| known.id == id) {
            Some(found) => {
                let mut known = ids.remove(found);
                if known.seconds != seconds {
                    known.seconds = seconds;
                    changed = true;
                }
                ids.push(known);
            }
            None => {
                ids.push(PlayedId {
                    id: id.to_string(),
                    seconds,
                });
                let excess = ids.len().saturating_sub(caps.ids);
                ids.drain(..excess);
                changed = true;
            }
        }
        while plays.songs.len() > caps.songs {
            plays.songs.shift_remove_index(0);
            plays.lossy = true;
            changed = true;
        }
        if changed {
            plays.dirty = true;
        }
    }

    /// The ids the song under `key` played under, the least recently played first, each with
    /// its catalog length then.
    pub(crate) fn played_ids(&self, key: &str) -> Vec<(String, Option<i32>)> {
        self.plays
            .lock()
            .songs
            .get(key)
            .map(|ids| {
                ids.iter()
                    .map(|known| (known.id.clone(), known.seconds))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the played index holds the song under `key`.
    pub(crate) fn knows_played(&self, key: &str) -> bool {
        self.plays.lock().songs.contains_key(key)
    }

    /// Whether the played index lost ids.
    pub fn lossy(&self) -> bool {
        self.plays.lock().lossy
    }

    /// Writes what is not written yet: the records after a failed write, the played index after
    /// a change. Called by the registry's flush.
    pub(crate) fn flush(&self) {
        if self.state.lock().dirty {
            self.save_choices();
        }
        if self.plays.lock().dirty {
            self.save_plays();
        }
    }

    /// Writes the played index with its clean mark: the server is stopping.
    pub(crate) fn stop_cleanly(&self) {
        {
            let mut p = self.plays.lock();
            p.clean = true;
            p.dirty = true;
        }
        self.save_plays();
    }

    /// Changes how much the store keeps.
    #[cfg(test)]
    pub(crate) fn set_caps(&self, caps: ChoiceCaps) {
        *self.caps.lock() = caps;
    }

    fn save_choices(&self) {
        let Some(path) = self.choices_path.as_deref() else {
            let mut c = self.state.lock();
            c.saved_seq = c.seq;
            c.dirty = false;
            return;
        };
        let mut written = None;
        let result = store_file::try_save_with(Some(path), &self.choices_saving, AREA, || {
            let mut c = self.state.lock();
            c.dirty = false;
            written = Some(c.seq);
            c.file()
        });
        let mut c = self.state.lock();
        match (result, written) {
            (Ok(()), Some(seq)) => c.saved_seq = c.saved_seq.max(seq),
            _ => c.dirty = true,
        }
    }

    fn save_plays(&self) {
        let Some(path) = self.plays_path.as_deref() else {
            self.plays.lock().dirty = false;
            return;
        };
        let result = store_file::try_save_with(Some(path), &self.plays_saving, AREA, || {
            let mut p = self.plays.lock();
            p.dirty = false;
            p.file()
        });
        if result.is_err() {
            self.plays.lock().dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(artist: &str, title: &str) -> SoulseekRouting {
        SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some(artist.into()),
            title: Some(title.into()),
            ..Default::default()
        }
    }

    fn chosen(video: &str, auto: bool) -> StreamChoice {
        StreamChoice {
            video: video.into(),
            seconds: Some(215),
            by: if auto { String::new() } else { "ede".into() },
            at: Utc::now(),
            auto,
            ..Default::default()
        }
    }

    fn registry_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("external-ids.json")
    }

    #[test]
    fn the_key_sets_a_clean_edit_apart_and_needs_both_names() {
        let plain = StreamChoices::key_of(&song("Drake", "Started From the Bottom")).expect("a key");
        let clean = StreamChoices::key_of(&SoulseekRouting {
            version: Some("clean".into()),
            ..song("Drake", "Started From the Bottom")
        })
        .expect("a key");
        assert_eq!(clean, format!("{plain}\nclean"));
        assert_eq!(StreamChoices::key_of(&song("", "Started")), None);
        let album = SoulseekRouting {
            kind: RoutingKind::Album,
            ..song("Drake", "Started")
        };
        assert_eq!(StreamChoices::key_of(&album), None);
    }

    #[test]
    fn the_catalog_length_is_last_fm_or_deezer_and_never_a_videos() {
        let with = |seconds: i32, source: LengthSource| SoulseekRouting {
            duration: Some(301),
            shown_duration: Some(seconds),
            shown_duration_source: source,
            ..song("A", "B")
        };
        assert_eq!(
            StreamChoices::catalog_seconds(&with(215, LengthSource::Deezer)),
            Some(215)
        );
        assert_eq!(
            StreamChoices::catalog_seconds(&with(214, LengthSource::LastFm)),
            Some(214)
        );
        assert_eq!(
            StreamChoices::catalog_seconds(&with(301, LengthSource::Video)),
            None
        );
        assert_eq!(
            StreamChoices::catalog_seconds(&with(216, LengthSource::File)),
            None
        );
        assert_eq!(
            StreamChoices::catalog_seconds(&song("A", "B")),
            None,
            "duration alone never counts"
        );
    }

    #[test]
    fn a_choice_fits_by_the_songs_length_else_by_the_videos() {
        let mut choice = chosen("v2", false);
        choice.song_seconds = Some(215);
        assert!(StreamChoices::fits(&choice, Some(222)));
        assert!(
            !StreamChoices::fits(&choice, Some(245)),
            "30 s longer is another recording"
        );
        assert!(
            StreamChoices::fits(&choice, None),
            "no catalog length always fits"
        );
        choice.song_seconds = None;
        choice.seconds = Some(240);
        assert!(
            StreamChoices::fits(&choice, Some(236)),
            "anchored on the video's length"
        );
        assert!(!StreamChoices::fits(&choice, Some(215)));
        assert_eq!(
            StreamChoices::agreed_seconds([Some(215), None, Some(220)]),
            Some(215)
        );
        assert_eq!(StreamChoices::agreed_seconds([Some(215), Some(240)]), None);
        assert_eq!(StreamChoices::agreed_seconds([None, None]), None);
    }

    #[test]
    fn records_and_tombstones_are_saved_and_read_back_under_the_same_epoch() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = registry_path(&dir);
        let store = StreamChoices::open(Some(&path));
        let epoch = store.epoch();
        assert!(
            dir.path().join("stream-choices.json").is_file(),
            "a new epoch is written at once"
        );
        assert!(store.put("a\nsong", chosen("v2", false)).is_none());
        assert!(store.put("b\nsong", chosen("v3", false)).is_none());
        assert!(
            store.remove("b\nsong", Some("v9"), vec![]).is_none(),
            "another video stays"
        );
        assert!(store.remove("b\nsong", Some("v3"), vec!["id-b".into()]).is_some());
        drop(store);

        let again = StreamChoices::open(Some(&path));
        assert_eq!(again.epoch(), epoch);
        assert_eq!(
            again.record_for_key("a\nsong").map(|r| (r.video, r.seq)),
            Some(("v2".into(), 1))
        );
        assert_eq!(again.record_for_key("b\nsong"), None);
        assert_eq!(again.removed_ids("b\nsong"), ["id-b"]);
        let page = again.changes_since(Some(&epoch), Some(0), 500);
        assert_eq!(
            page.changes,
            [(1, "a\nsong".to_string()), (3, "b\nsong".to_string())]
        );
        assert_eq!((page.seq, page.complete), (3, true));
        let later = again.changes_since(Some(&epoch), Some(1), 500);
        assert_eq!(later.changes, [(3, "b\nsong".to_string())]);
    }

    #[test]
    fn the_feed_never_hands_out_a_change_whose_write_failed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = StreamChoices::open(Some(&registry_path(&dir)));
        let file = dir.path().join("stream-choices.json");
        store.put("a\nsong", chosen("v2", false));
        // A folder where the file goes: the next write fails.
        std::fs::remove_file(&file).expect("removed");
        std::fs::create_dir(&file).expect("a folder in its place");
        store.put("b\nsong", chosen("v3", false));
        assert_eq!(
            store.record_for_key("b\nsong").map(|r| r.video),
            Some("v3".into()),
            "applies from memory"
        );
        let page = store.changes_since(None, Some(0), 500);
        assert_eq!(page.seq, 1);
        assert_eq!(page.changes, [(1, "a\nsong".to_string())]);

        std::fs::remove_dir(&file).expect("the folder goes");
        store.flush();
        let page = store.changes_since(None, Some(1), 500);
        assert_eq!((page.seq, page.changes.len()), (2, 1));
    }

    #[test]
    fn a_full_store_puts_out_octos_own_pick_first_then_the_oldest_choice() {
        let store = StreamChoices::open_with(
            None,
            ChoiceCaps {
                records: 2,
                ..ChoiceCaps::default()
            },
        );
        store.put("a", chosen("va", false));
        store.put("b", chosen("vb", true));
        let (key, record) = store.put("c", chosen("vc", false)).expect("one goes");
        assert_eq!((key.as_str(), record.auto), ("b", true));
        let (key, _) = store.put("d", chosen("vd", false)).expect("one goes");
        assert_eq!(key, "a");
        assert!(store.record_for_key("c").is_some() && store.record_for_key("d").is_some());
    }

    #[test]
    fn tombstones_past_the_cap_raise_the_floor_and_an_older_cursor_is_incomplete() {
        let store = StreamChoices::open_with(
            None,
            ChoiceCaps {
                tombstones: 2,
                ..ChoiceCaps::default()
            },
        );
        for key in ["a", "b", "c"] {
            store.put(key, chosen("v", false));
        }
        for key in ["a", "b", "c"] {
            store.remove(key, None, vec![]);
        }
        // Six changes; the first tombstone (seq 4) went.
        let epoch = store.epoch();
        let old = store.changes_since(Some(&epoch), Some(3), 500);
        assert!(!old.complete);
        let fresh = store.changes_since(Some(&epoch), Some(4), 500);
        assert!(fresh.complete);
        assert_eq!(fresh.changes, [(5, "b".to_string()), (6, "c".to_string())]);
        assert!(!store.changes_since(Some("another"), Some(4), 500).complete);
        let capped = store.changes_since(Some(&epoch), Some(4), 1);
        assert!(!capped.complete);
        assert_eq!(capped.changes.len(), 1);
        assert!(
            !store.changes_since(None, None, 500).complete,
            "no cursor below a raised floor"
        );
    }

    #[test]
    fn an_unreadable_file_is_kept_aside_and_a_new_epoch_starts() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = registry_path(&dir);
        let epoch = StreamChoices::open(Some(&path)).epoch();
        std::fs::write(dir.path().join("stream-choices.json"), "{ torn").expect("a torn file");
        let store = StreamChoices::open(Some(&path));
        assert_ne!(store.epoch(), epoch);
        let kept_aside = std::fs::read_dir(dir.path())
            .expect("listed")
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("stream-choices.json.broken-")
            });
        assert!(kept_aside);
        assert!(!store.changes_since(Some(&epoch), Some(0), 500).complete);
    }

    #[test]
    fn the_index_keeps_eight_ids_a_song_and_is_lossy_once_a_song_goes() {
        let store = StreamChoices::open_with(
            None,
            ChoiceCaps {
                songs: 2,
                ..ChoiceCaps::default()
            },
        );
        for n in 0..10 {
            store.note_played("a", &format!("id{n}"), Some(215));
        }
        let ids: Vec<String> = store.played_ids("a").into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids.len(), 8);
        assert_eq!(ids.first().map(String::as_str), Some("id2"));
        assert!(!store.lossy());
        store.plays.lock().dirty = false;
        store.note_played("a", "id9", Some(215));
        assert!(
            !store.plays.lock().dirty,
            "the same id again changes nothing written"
        );
        store.note_played("b", "x", None);
        store.note_played("c", "y", None);
        assert!(store.lossy(), "the least recently played song went");
        assert!(!store.knows_played("a"));
    }

    #[test]
    fn a_start_after_a_stop_without_its_clean_mark_is_lossy() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = registry_path(&dir);
        let store = StreamChoices::open(Some(&path));
        store.note_played("a", "id1", None);
        store.flush();
        store.stop_cleanly();
        drop(store);
        let clean = StreamChoices::open(Some(&path));
        assert!(!clean.lossy());
        assert_eq!(clean.played_ids("a"), [("id1".to_string(), None::<i32>)]);
        // Stopped with no clean mark written (a crash).
        drop(clean);
        assert!(StreamChoices::open(Some(&path)).lossy());
    }
}
