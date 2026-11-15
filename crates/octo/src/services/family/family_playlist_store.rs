//! `family-playlists.json`: playlists every member can add to. Each entry keeps a library song
//! id and an outside id where it has them, so a member who cannot see a song in someone's own
//! library still gets its outside version.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use octo_core::json::datetime;
use octo_core::soulseek::SoulseekRouting;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use super::store_file;
use crate::services::playlists::caps::{CapRefusal, PlaylistCaps};
use crate::services::playlists::seed_index::{SeedIndex, family_key};
use crate::services::soulseek::ExternalIdRegistry;

/// One song in a family playlist. Equality leaves out the seed (`seed_id`, `seed`), so a seed
/// refresh never moves a playlist's change time or trips [`FamilyPlaylistStore::remove_indexes_if`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FamilyPlaylistEntry {
    /// The Navidrome song id, when the song is in a library.
    #[serde(default)]
    pub library_id: Option<String>,
    /// Which library the song is in: `Shared`, or `Personal:<username>`.
    #[serde(default)]
    pub scope: String,
    /// The outside id (`ext-...`), when Octo knows one.
    #[serde(default)]
    pub outside_id: Option<String>,
    #[serde(default)]
    pub added_by: String,
    #[serde(with = "datetime::utc")]
    pub added: DateTime<Utc>,
    /// Waiting for an approver (playlists that need approval for additions).
    #[serde(default)]
    pub pending: bool,
    /// The outside version's length in seconds, as the catalog gave it when it was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<i64>,
    /// The outside version's explicit mark (`explicit`, `clean`, or empty for none), as the
    /// catalog gave it when it was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit_status: Option<String>,
    /// The registry id the outside version is kept under, for an outside song the registry knew
    /// when it was added or at start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_id: Option<String>,
    /// A copy of the outside version's routing, used only if the registry's own file is lost.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "seed_copy")]
    pub seed: Option<Arc<SoulseekRouting>>,
}

impl PartialEq for FamilyPlaylistEntry {
    fn eq(&self, other: &Self) -> bool {
        self.library_id == other.library_id
            && self.scope == other.scope
            && self.outside_id == other.outside_id
            && self.added_by == other.added_by
            && self.added == other.added
            && self.pending == other.pending
            && self.duration == other.duration
            && self.explicit_status == other.explicit_status
    }
}

impl Eq for FamilyPlaylistEntry {}

/// A seed copy in the file: the routing written whole, read back behind an `Arc`.
mod seed_copy {
    use std::sync::Arc;

    use octo_core::soulseek::SoulseekRouting;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        seed: &Option<Arc<SoulseekRouting>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        seed.as_deref().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Arc<SoulseekRouting>>, D::Error> {
        Ok(Option::<SoulseekRouting>::deserialize(deserializer)?.map(Arc::new))
    }
}

/// One family playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FamilyPlaylist {
    /// `of` and 20 letters or digits: 22 characters, like Octo's other playlist ids.
    pub id: String,
    pub name: String,
    #[serde(with = "datetime::utc")]
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub approve_additions: bool,
    #[serde(default)]
    pub entries: Vec<FamilyPlaylistEntry>,
    /// When the playlist last changed: a rename, the approval switch, songs added, taken out,
    /// approved or denied, or a library move.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "datetime::utc_option"
    )]
    pub changed: Option<DateTime<Utc>>,
}

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `of` + 20 base62 characters of SHA-256(name + created).
pub fn family_playlist_id(name: &str, created: &DateTime<Utc>) -> String {
    let digest = Sha256::digest(format!("{name}{}", datetime::format_utc(created)).as_bytes());
    let tail: String = digest
        .iter()
        .take(20)
        .map(|b| char::from(BASE62[usize::from(*b) % BASE62.len()]))
        .collect();
    format!("of{tail}")
}

/// Where the family playlists' kept ids, seeds and counts go.
#[derive(Clone)]
pub struct FamilyKept {
    pub registry: Arc<ExternalIdRegistry>,
    pub seed_index: Arc<SeedIndex>,
    pub caps: Arc<PlaylistCaps>,
}

/// Why songs could not be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyAddRefusal {
    PersonCap,
    ServerCap,
    SaveFailed,
}

impl FamilyAddRefusal {
    pub fn message(self) -> &'static str {
        match self {
            FamilyAddRefusal::PersonCap => "Your playlists hold as many songs found online as Octo keeps.",
            FamilyAddRefusal::ServerCap => {
                "This server holds as many songs found online in playlists as Octo keeps."
            }
            FamilyAddRefusal::SaveFailed => "Octo couldn't save the playlist, so nothing was changed.",
        }
    }
}

/// Whether an entry counts against its adder's cap (and is kept in the registry): an outside song
/// with no library id, kept under a registry id.
fn counted(entry: &FamilyPlaylistEntry) -> bool {
    entry.library_id.as_deref().is_none_or(|id| id.trim().is_empty()) && entry.seed_id.is_some()
}

/// Per adder (lowercased), how the counted entries change from `before` to `after`.
fn count_changes(before: &[FamilyPlaylistEntry], after: &[FamilyPlaylistEntry]) -> Vec<(String, i64)> {
    let mut changes: Vec<(String, i64)> = Vec::new();
    for (entries, sign) in [(before, -1i64), (after, 1i64)] {
        for entry in entries.iter().filter(|entry| counted(entry)) {
            let owner = entry.added_by.trim().to_lowercase();
            match changes.iter_mut().find(|(o, _)| *o == owner) {
                Some((_, n)) => *n += sign,
                None => changes.push((owner, sign)),
            }
        }
    }
    changes.retain(|(_, n)| *n != 0);
    changes
}

/// A seed copy for an outside entry the registry knows, read with `peek`.
fn fill_seed(registry: &ExternalIdRegistry, entry: &mut FamilyPlaylistEntry) {
    if entry.seed_id.is_some() {
        return;
    }
    let Some(outside) = entry.outside_id.clone().filter(|id| !id.trim().is_empty()) else {
        return;
    };
    if let Some(routing) = registry.peek(&outside) {
        entry.seed = Some(Arc::new(routing.snapshot()));
        entry.seed_id = Some(outside);
    }
}

/// The store behind `family-playlists.json`.
///
/// Every change takes a revision under the store's mutex and, once the mutex is released, tells
/// the registry (`set_kept`) and the playlists' seed index what the playlist holds; count changes
/// go to the caps outside the mutex too. An add is refused past a cap before anything changes, and
/// one whose save fails is taken back out of memory with its counts.
pub struct FamilyPlaylistStore {
    playlists: Mutex<Vec<FamilyPlaylist>>,
    path: Option<PathBuf>,
    save_lock: Mutex<()>,
    load_warning: Option<String>,
    revision: AtomicU64,
    kept: RwLock<Option<FamilyKept>>,
}

impl FamilyPlaylistStore {
    pub fn in_memory() -> Self {
        Self::new(None::<PathBuf>)
    }

    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = store_file::clean_path(path);
        let (playlists, load_warning) = store_file::load::<Vec<FamilyPlaylist>>(path.as_deref());
        FamilyPlaylistStore {
            playlists: Mutex::new(playlists),
            path,
            save_lock: Mutex::new(()),
            load_warning,
            revision: AtomicU64::new(1),
            kept: RwLock::new(None),
        }
    }

    /// Kept ids, seeds and counts go to `kept` from now on.
    pub fn attach_kept(&self, kept: FamilyKept) {
        *self.kept.write() = Some(kept);
    }

    pub fn load_warning(&self) -> Option<&str> {
        self.load_warning.as_deref()
    }

    /// A new, empty playlist.
    pub fn create(&self, name: &str, by: &str, now: DateTime<Utc>) -> FamilyPlaylist {
        let playlist = {
            let mut playlists = self.playlists.lock();
            let mut id = family_playlist_id(name, &now);
            let mut n = 0u32;
            while playlists.iter().any(|p| p.id == id) {
                n += 1;
                id = family_playlist_id(&format!("{name}#{n}"), &now);
            }
            let playlist = FamilyPlaylist {
                id,
                name: name.trim().to_string(),
                created: now,
                created_by: by.to_string(),
                approve_additions: false,
                entries: Vec::new(),
                changed: None,
            };
            playlists.push(playlist.clone());
            playlist
        };
        self.save();
        playlist
    }

    pub fn get(&self, id: &str) -> Option<FamilyPlaylist> {
        self.playlists.lock().iter().find(|p| p.id == id).cloned()
    }

    pub fn list(&self) -> Vec<FamilyPlaylist> {
        self.playlists.lock().clone()
    }

    pub fn rename(&self, id: &str, name: &str) -> bool {
        self.change(id, |p| p.name = name.trim().to_string())
    }

    pub fn set_approve(&self, id: &str, approve: bool) -> bool {
        self.change(id, |p| p.approve_additions = approve)
    }

    /// Adds entries at the end. False when there is no such playlist or the add was refused.
    pub fn add_entries(&self, id: &str, entries: Vec<FamilyPlaylistEntry>) -> bool {
        self.try_add_entries(id, entries).unwrap_or(false)
    }

    /// Adds entries at the end, each outside song with its seed when the registry knows it. A
    /// rise past a cap is refused before anything changes; a save that fails takes the entries out
    /// of memory again and reverses their counts. `Ok(false)` when there is no such playlist.
    pub fn try_add_entries(
        &self,
        id: &str,
        mut entries: Vec<FamilyPlaylistEntry>,
    ) -> Result<bool, FamilyAddRefusal> {
        let kept = self.kept.read().clone();
        if let Some(kept) = &kept {
            for entry in entries.iter_mut() {
                fill_seed(&kept.registry, entry);
            }
        }
        let rise = count_changes(&[], &entries);
        if let Some(kept) = &kept
            && !rise.is_empty()
        {
            kept.caps.apply(&rise).map_err(|refusal| match refusal {
                CapRefusal::Person => FamilyAddRefusal::PersonCap,
                CapRefusal::Server => FamilyAddRefusal::ServerCap,
            })?;
        }
        let added = {
            let mut playlists = self.playlists.lock();
            playlists.iter_mut().find(|p| p.id == id).map(|playlist| {
                let first = playlist.entries.len();
                playlist.entries.extend(entries.iter().cloned());
                playlist.changed = Some(Utc::now());
                let revision = self.revision.fetch_add(1, Ordering::SeqCst);
                (first, revision, playlist.clone())
            })
        };
        let Some((first, revision, playlist)) = added else {
            self.lower(
                &rise
                    .iter()
                    .map(|(owner, n)| (owner.clone(), -n))
                    .collect::<Vec<_>>(),
            );
            return Ok(false);
        };
        self.publish(id, revision, Some(&playlist));
        if let Err(error) = self.try_save() {
            warn!(
                "FAMILY family-playlists.json could not be written, so the songs added to {id} were taken out again: {error}"
            );
            let restored = {
                let mut playlists = self.playlists.lock();
                playlists.iter_mut().find(|p| p.id == id).map(|playlist| {
                    let end = first + entries.len();
                    if playlist.entries.len() >= end && playlist.entries[first..end] == entries[..] {
                        playlist.entries.drain(first..end);
                    }
                    (self.revision.fetch_add(1, Ordering::SeqCst), playlist.clone())
                })
            };
            if let Some((revision, playlist)) = restored {
                self.publish(id, revision, Some(&playlist));
            }
            self.lower(
                &rise
                    .iter()
                    .map(|(owner, n)| (owner.clone(), -n))
                    .collect::<Vec<_>>(),
            );
            return Err(FamilyAddRefusal::SaveFailed);
        }
        Ok(true)
    }

    /// Removes the entries at these positions (positions past the end are ignored).
    pub fn remove_indexes(&self, id: &str, indexes: &[usize]) -> bool {
        self.change(id, |p| {
            let mut at = 0usize;
            p.entries.retain(|_| {
                let keep = !indexes.contains(&at);
                at += 1;
                keep
            });
        })
    }

    /// Removes the entries at these positions, but only while the playlist's entries are still
    /// `expected`. False when the playlist changed or is gone; then nothing is removed.
    pub fn remove_indexes_if(&self, id: &str, expected: &[FamilyPlaylistEntry], indexes: &[usize]) -> bool {
        let mut removed = false;
        self.change(id, |p| {
            if p.entries == expected {
                let mut at = 0usize;
                p.entries.retain(|_| {
                    let keep = !indexes.contains(&at);
                    at += 1;
                    keep
                });
                removed = true;
            }
        });
        removed
    }

    /// Lets a waiting entry in. False when there is no such playlist or entry.
    pub fn approve_entry(&self, id: &str, index: usize) -> bool {
        let mut found = false;
        let changed = self.change(id, |p| {
            if let Some(entry) = p.entries.get_mut(index) {
                entry.pending = false;
                found = true;
            }
        });
        changed && found
    }

    pub fn delete(&self, id: &str) -> bool {
        let removed = {
            let mut playlists = self.playlists.lock();
            playlists
                .iter()
                .position(|p| p.id == id)
                .map(|at| (self.revision.fetch_add(1, Ordering::SeqCst), playlists.remove(at)))
        };
        let Some((revision, playlist)) = removed else {
            return false;
        };
        self.publish(id, revision, None);
        self.lower(&count_changes(&playlist.entries, &[]));
        self.save();
        true
    }

    /// Points every entry of every playlist that names library song `old` at `new` instead, in
    /// `scope` (`Shared` or `Personal:<user>`), for a song whose file moved and whose id
    /// changed with it. Answers how many entries changed.
    pub fn remap_library_id(&self, old: &str, new: &str, scope: &str) -> usize {
        let mut remapped: Vec<(u64, FamilyPlaylist)> = Vec::new();
        let changed = {
            let mut playlists = self.playlists.lock();
            let mut changed = 0;
            for playlist in playlists.iter_mut() {
                let mut moved = false;
                for entry in playlist.entries.iter_mut() {
                    if entry.library_id.as_deref() == Some(old) {
                        entry.library_id = Some(new.to_string());
                        entry.scope = scope.to_string();
                        changed += 1;
                        moved = true;
                    }
                }
                if moved {
                    playlist.changed = Some(Utc::now());
                    remapped.push((self.revision.fetch_add(1, Ordering::SeqCst), playlist.clone()));
                }
            }
            changed
        };
        for (revision, playlist) in &remapped {
            self.publish(&playlist.id, *revision, Some(playlist));
        }
        if changed > 0 {
            self.save();
        }
        changed
    }

    fn change(&self, id: &str, change: impl FnOnce(&mut FamilyPlaylist)) -> bool {
        let found = {
            let mut playlists = self.playlists.lock();
            playlists.iter_mut().find(|p| p.id == id).map(|playlist| {
                let before = playlist.clone();
                change(playlist);
                if *playlist != before {
                    playlist.changed = Some(Utc::now());
                }
                let revision = self.revision.fetch_add(1, Ordering::SeqCst);
                (
                    revision,
                    count_changes(&before.entries, &playlist.entries),
                    playlist.clone(),
                )
            })
        };
        let Some((revision, changes, playlist)) = found else {
            return false;
        };
        self.publish(id, revision, Some(&playlist));
        self.lower(&changes);
        self.save();
        true
    }

    fn save(&self) {
        store_file::save_with(self.path.as_deref(), &self.save_lock, || {
            self.playlists.lock().clone()
        });
    }

    fn try_save(&self) -> Result<(), String> {
        store_file::try_save_with(self.path.as_deref(), &self.save_lock, "Family", || {
            self.playlists.lock().clone()
        })
        .map_err(|error| error.to_string())
    }

    /// Tells the registry and the seed index what a playlist holds now (`None`: it is gone):
    /// kept, the registry ids of its outside entries with no library id; indexed, every seed.
    fn publish(&self, id: &str, revision: u64, playlist: Option<&FamilyPlaylist>) {
        let Some(kept) = self.kept.read().clone() else {
            return;
        };
        let entries = playlist.map_or(&[][..], |playlist| playlist.entries.as_slice());
        let ids: Vec<(String, SoulseekRouting)> = entries
            .iter()
            .filter(|entry| counted(entry))
            .filter_map(|entry| Some((entry.seed_id.clone()?, entry.seed.as_deref()?.clone())))
            .collect();
        let seeds: Vec<(String, Arc<SoulseekRouting>)> = entries
            .iter()
            .filter_map(|entry| Some((entry.seed_id.clone()?, Arc::clone(entry.seed.as_ref()?))))
            .collect();
        let key = family_key(id);
        kept.registry.set_kept(&key, revision, ids);
        kept.seed_index.set(&key, revision, seeds);
    }

    /// Lowers the caps' counts (only lowering changes are applied; they never refuse).
    fn lower(&self, changes: &[(String, i64)]) {
        let Some(kept) = self.kept.read().clone() else {
            return;
        };
        let lowered: Vec<(String, i64)> = changes.iter().filter(|(_, n)| *n < 0).cloned().collect();
        if !lowered.is_empty() {
            let _ = kept.caps.apply(&lowered);
        }
    }

    /// Gives every outside entry the registry still knows its seed (read with `peek`, so nothing
    /// is restored or made recent), in memory only; the next save writes them. Neither the change
    /// time nor equality moves.
    pub fn backfill_seeds(&self) {
        let Some(kept) = self.kept.read().clone() else {
            return;
        };
        let wanted: Vec<String> = {
            let playlists = self.playlists.lock();
            let mut wanted: Vec<String> = playlists
                .iter()
                .flat_map(|playlist| playlist.entries.iter())
                .filter(|entry| entry.seed_id.is_none())
                .filter_map(|entry| entry.outside_id.clone())
                .filter(|id| !id.trim().is_empty())
                .collect();
            wanted.sort();
            wanted.dedup();
            wanted
        };
        let found: HashMap<String, Arc<SoulseekRouting>> = wanted
            .into_iter()
            .filter_map(|id| {
                kept.registry
                    .peek(&id)
                    .map(|routing| (id, Arc::new(routing.snapshot())))
            })
            .collect();
        if found.is_empty() {
            return;
        }
        let mut playlists = self.playlists.lock();
        for entry in playlists
            .iter_mut()
            .flat_map(|playlist| playlist.entries.iter_mut())
        {
            if entry.seed_id.is_none()
                && let Some(outside) = entry.outside_id.clone()
                && let Some(seed) = found.get(&outside)
            {
                entry.seed_id = Some(outside);
                entry.seed = Some(Arc::clone(seed));
            }
        }
    }

    /// Refreshes every seed copy from the registry's routing (read with `peek`), in memory only;
    /// an id the registry does not have keeps its copy. Neither the change time nor equality moves.
    pub fn refresh_seeds(&self) {
        let Some(kept) = self.kept.read().clone() else {
            return;
        };
        let ids: Vec<String> = {
            let playlists = self.playlists.lock();
            let mut ids: Vec<String> = playlists
                .iter()
                .flat_map(|playlist| playlist.entries.iter())
                .filter_map(|entry| entry.seed_id.clone())
                .collect();
            ids.sort();
            ids.dedup();
            ids
        };
        let found: HashMap<String, SoulseekRouting> = ids
            .into_iter()
            .filter_map(|id| kept.registry.peek(&id).map(|routing| (id, routing.snapshot())))
            .collect();
        let mut playlists = self.playlists.lock();
        for entry in playlists
            .iter_mut()
            .flat_map(|playlist| playlist.entries.iter_mut())
        {
            if let Some(seed_id) = &entry.seed_id
                && let Some(now) = found.get(seed_id)
                && entry.seed.as_deref() != Some(now)
            {
                entry.seed = Some(Arc::new(now.clone()));
            }
        }
    }

    /// Tells the registry and the seed index what every playlist holds (at start, after the
    /// backfill, and after a refresh).
    pub fn publish_kept(&self) {
        let all: Vec<(u64, FamilyPlaylist)> = {
            let playlists = self.playlists.lock();
            playlists
                .iter()
                .map(|playlist| (self.revision.fetch_add(1, Ordering::SeqCst), playlist.clone()))
                .collect()
        };
        for (revision, playlist) in &all {
            self.publish(&playlist.id, *revision, Some(playlist));
        }
    }

    /// Per adder (lowercased), the counted entries, for the caps' count at start.
    pub fn counted_by_owner(&self) -> HashMap<String, usize> {
        let playlists = self.playlists.lock();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for entry in playlists
            .iter()
            .flat_map(|playlist| playlist.entries.iter())
            .filter(|entry| counted(entry))
        {
            *counts.entry(entry.added_by.trim().to_lowercase()).or_default() += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 18, 0, 0).unwrap()
    }

    fn entry(id: &str, by: &str) -> FamilyPlaylistEntry {
        FamilyPlaylistEntry {
            library_id: None,
            scope: String::new(),
            outside_id: Some(id.to_string()),
            added_by: by.to_string(),
            added: now(),
            pending: false,
            duration: None,
            explicit_status: None,
            seed_id: None,
            seed: None,
        }
    }

    fn song(title: &str) -> SoulseekRouting {
        SoulseekRouting {
            kind: octo_core::soulseek::RoutingKind::Song,
            title: Some(title.to_string()),
            artist: Some("Artist".to_string()),
            album: Some("Album".to_string()),
            duration: Some(200),
            you_tube_id: Some(format!("v-{title}")),
            ..Default::default()
        }
    }

    /// A store whose kept ids go to a fresh registry and seed index, with these caps.
    fn kept_store(
        path: Option<&Path>,
        person: usize,
        server: usize,
    ) -> (FamilyPlaylistStore, Arc<ExternalIdRegistry>) {
        let store = FamilyPlaylistStore::new(path);
        let registry = Arc::new(ExternalIdRegistry::in_memory());
        store.attach_kept(FamilyKept {
            registry: Arc::clone(&registry),
            seed_index: Arc::new(SeedIndex::new()),
            caps: Arc::new(PlaylistCaps::with_limits(person, server)),
        });
        (store, registry)
    }

    #[test]
    fn of_ids_are_octo_playlist_ids() {
        let id = family_playlist_id("Road trip", &now());
        assert_eq!(id.len(), 22, "{id}");
        assert!(id.starts_with("of"));
        assert!(id[2..].bytes().all(|b| b.is_ascii_alphanumeric()), "{id}");
        assert_eq!(
            id,
            family_playlist_id("Road trip", &now()),
            "the same name and time"
        );
        assert_ne!(id, family_playlist_id("Road trip 2", &now()));
        // Two made at the same instant with one name still get two ids.
        let store = FamilyPlaylistStore::in_memory();
        let a = store.create("Road trip", "winters", now());
        let b = store.create("Road trip", "alex", now());
        assert_ne!(a.id, b.id);
        assert_eq!(b.id.len(), 22);
    }

    #[test]
    fn create_get_rename_and_approval() {
        let store = FamilyPlaylistStore::in_memory();
        let made = store.create(" Road trip ", "winters", now());
        assert_eq!(made.name, "Road trip");
        assert_eq!(store.get(&made.id), Some(made.clone()));
        assert!(store.rename(&made.id, "Summer"));
        assert!(store.set_approve(&made.id, true));
        let got = store.get(&made.id).expect("there");
        assert_eq!(got.name, "Summer");
        assert!(got.approve_additions);
        assert!(!store.rename("of_missing", "x"));
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn entries_add_remove_and_approve() {
        let store = FamilyPlaylistStore::in_memory();
        let made = store.create("Road trip", "winters", now());
        let mut waiting = entry("e2", "alex");
        waiting.pending = true;
        assert!(store.add_entries(&made.id, vec![entry("e1", "alex"), waiting, entry("e3", "sam")]));
        assert!(store.approve_entry(&made.id, 1));
        assert!(!store.approve_entry(&made.id, 9));
        assert!(!store.get(&made.id).expect("there").entries[1].pending);
        assert!(store.remove_indexes(&made.id, &[0, 2, 7]));
        let left = store.get(&made.id).expect("there").entries;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].outside_id.as_deref(), Some("e2"));
    }

    #[test]
    fn changes_move_the_change_time_and_refused_ones_do_not() {
        let store = FamilyPlaylistStore::in_memory();
        let made = store.create("Road trip", "winters", now());
        assert_eq!(made.changed, None);
        store.add_entries(&made.id, vec![entry("e1", "alex")]);
        let added = store.get(&made.id).expect("there");
        assert!(added.changed.is_some(), "an add moves it");
        assert!(store.rename(&made.id, "Road trip"));
        assert_eq!(
            store.get(&made.id).expect("there").changed,
            added.changed,
            "a rename to the same name moves nothing"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(store.rename(&made.id, "Summer"));
        let renamed = store.get(&made.id).expect("there");
        assert!(renamed.changed > added.changed, "a rename moves it");
        let stale = vec![entry("other", "alex")];
        assert!(!store.remove_indexes_if(&made.id, &stale, &[0]));
        assert_eq!(
            store.get(&made.id).expect("there").changed,
            renamed.changed,
            "a refused removal moves nothing"
        );
    }

    #[test]
    fn a_file_written_before_the_change_time_loads() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-playlists.json");
        std::fs::write(
            &path,
            r#"[{"Id":"of1","Name":"Old","Created":"2026-10-20T18:00:00Z","CreatedBy":"winters","ApproveAdditions":false,"Entries":[]}]"#,
        )
        .expect("written");
        let store = FamilyPlaylistStore::new(Some(&path));
        assert_eq!(store.load_warning(), None);
        let got = store.get("of1").expect("loaded");
        assert_eq!(got.name, "Old");
        assert_eq!(got.changed, None);
    }

    #[test]
    fn delete_and_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-playlists.json");
        let store = FamilyPlaylistStore::new(Some(&path));
        let keep = store.create("Keep", "winters", now());
        let gone = store.create("Gone", "winters", now());
        store.add_entries(&keep.id, vec![entry("e1", "alex")]);
        assert!(store.delete(&gone.id));
        assert!(!store.delete(&gone.id));
        let reloaded = FamilyPlaylistStore::new(Some(&path));
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(
            reloaded.get(&keep.id).expect("there").entries,
            vec![entry("e1", "alex")]
        );
    }

    #[test]
    fn t35_a_family_outside_song_survives_ten_thousand_newer_ones() {
        let (store, registry) = kept_store(None, 15_000, 30_000);
        let id = registry.register(song("Road"));
        let list = store.create("Road trip", "winters", now());
        assert!(store.add_entries(&list.id, vec![entry(&id, "alex")]));
        let added = store.get(&list.id).expect("there").entries[0].clone();
        assert_eq!(
            added.seed_id.as_deref(),
            Some(id.as_str()),
            "the add keeps its seed"
        );
        assert!(added.seed.is_some());
        for n in 0..10_050 {
            registry.register(song(&format!("Filler {n}")));
        }
        assert!(
            registry.lookup(&id).is_some(),
            "a family playlist's outside song is kept past the cache"
        );
    }

    #[test]
    fn t35_the_backfill_gives_old_entries_their_seeds_and_the_next_save_writes_them() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-playlists.json");
        let registry_of_ids = ExternalIdRegistry::in_memory();
        let id = registry_of_ids.register(song("Road"));
        std::fs::write(
            &path,
            format!(
                r#"[{{"Id":"of1","Name":"Old","Created":"2026-10-20T18:00:00Z","CreatedBy":"winters","ApproveAdditions":false,"Entries":[{{"Scope":"","OutsideId":"{id}","AddedBy":"alex","Added":"2026-10-20T18:00:00Z","Pending":false}}]}}]"#
            ),
        )
        .expect("written");
        let (store, registry) = kept_store(Some(&path), 15_000, 30_000);
        assert_eq!(registry.register(song("Road")), id);
        let changed = store.get("of1").expect("loaded").changed;
        store.backfill_seeds();
        let entry = store.get("of1").expect("loaded").entries[0].clone();
        assert_eq!(
            entry.seed_id.as_deref(),
            Some(id.as_str()),
            "the backfill sets the seed in memory"
        );
        assert_eq!(
            store.get("of1").expect("loaded").changed,
            changed,
            "the backfill moves no change time"
        );
        assert!(
            !std::fs::read_to_string(&path).expect("reads").contains("SeedId"),
            "the backfill itself writes nothing"
        );
        assert!(store.rename("of1", "Older"));
        assert!(
            std::fs::read_to_string(&path).expect("reads").contains("SeedId"),
            "the next save writes the seed"
        );
    }

    #[test]
    fn t51_a_seed_refresh_moves_no_change_time_and_refuses_no_removal() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-playlists.json");
        let (store, registry) = kept_store(Some(&path), 15_000, 30_000);
        let id = registry.register(song("Road"));
        let list = store.create("Road trip", "winters", now());
        assert!(store.add_entries(
            &list.id,
            vec![entry(&id, "alex"), entry("ext-deezer-song-2", "alex")]
        ));
        let read = store.get(&list.id).expect("there");
        assert!(
            registry.pin_video(&id, "v-other", true),
            "the song's video changes in place"
        );
        store.refresh_seeds();
        let refreshed = store.get(&list.id).expect("there");
        assert_ne!(
            refreshed.entries[0].seed, read.entries[0].seed,
            "the copy follows the routing"
        );
        assert_eq!(refreshed.changed, read.changed, "a refresh moves no change time");
        assert!(
            store.remove_indexes_if(&list.id, &read.entries, &[1]),
            "a removal worked out before the refresh still goes through"
        );
        assert_eq!(store.get(&list.id).expect("there").entries.len(), 1);
    }

    #[test]
    fn t49_a_family_add_whose_save_fails_puts_the_songs_and_the_counts_back() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-playlists.json");
        let (store, registry) = kept_store(Some(&path), 1, 10);
        let first = registry.register(song("One"));
        let second = registry.register(song("Two"));
        let list = store.create("Road trip", "winters", now());
        // The file becomes a folder with something in it, so the next write fails.
        std::fs::remove_file(&path).expect("removed");
        std::fs::create_dir(&path).expect("a folder");
        std::fs::write(path.join("in-the-way"), "x").expect("written");
        assert_eq!(
            store.try_add_entries(&list.id, vec![entry(&first, "alex")]),
            Err(FamilyAddRefusal::SaveFailed)
        );
        assert!(
            store.get(&list.id).expect("there").entries.is_empty(),
            "memory is put back"
        );
        std::fs::remove_dir_all(&path).expect("cleared");
        assert_eq!(
            store.try_add_entries(&list.id, vec![entry(&first, "alex")]),
            Ok(true),
            "the failed add's count was reversed, so one song fits a cap of one"
        );
        assert_eq!(
            store.try_add_entries(&list.id, vec![entry(&second, "alex")]),
            Err(FamilyAddRefusal::PersonCap),
            "the next add past the cap is refused, by its adder"
        );
        assert_eq!(
            store.try_add_entries(&list.id, vec![entry(&second, "sam")]),
            Ok(true),
            "another adder has their own count"
        );
    }
}
