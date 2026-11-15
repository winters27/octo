//! `playlist-folders.json`: each person's playlist folders and where their playlists are filed.
//! Folder ids are made by the apps (`pf` and 20 letters or digits), so an edit made offline
//! needs no id handed back. Every call is idempotent, and a deleted folder leaves a tombstone so
//! a stale edit never brings it back: a save of it is refused, and anything filed under it goes
//! to its nearest live folder.
//!
//! Never written while a playlist lock is held.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use octo_core::json::datetime;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::services::family::store_file;

/// The words the store's warnings name it by.
const AREA: &str = "Playlist folders";

/// How deep folders go; a top-level folder is at depth 1.
pub const MAX_DEPTH: usize = 4;
/// How many folders a person keeps.
pub const MAX_FOLDERS: usize = 500;
/// How many playlists a person files.
pub const MAX_PLACES: usize = 10_000;
/// The longest folder name, in characters.
pub const MAX_NAME: usize = 100;

/// One folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaylistFolder {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(with = "datetime::utc")]
    pub created: DateTime<Utc>,
}

/// One person's folders as the folder endpoints answer them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderState {
    /// When they last changed (the Unix epoch when they never did).
    pub changed: DateTime<Utc>,
    pub folders: Vec<PlaylistFolder>,
    /// (playlist id, folder id), by playlist id.
    pub places: Vec<(String, String)>,
}

/// Why a folder call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderError {
    Name,
    Id,
    Depth,
    Cycle,
    TooManyFolders,
    TooManyPlaces,
    OwnList,
    NotFound,
}

impl FolderError {
    /// The Subsonic error code.
    pub fn code(self) -> i32 {
        match self {
            FolderError::NotFound => 70,
            _ => 10,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            FolderError::Name => "A folder needs a name of 1 to 100 characters.",
            FolderError::Id => "That folder id isn't one Octo can use.",
            FolderError::Depth => "Folders can go 4 deep.",
            FolderError::Cycle => "A folder can't go inside itself.",
            FolderError::TooManyFolders => "You have as many folders as Octo keeps.",
            FolderError::TooManyPlaces => "You have filed as many playlists as Octo keeps.",
            FolderError::OwnList => "Octo's own lists stay in From Octo.",
            FolderError::NotFound => "There is no such folder.",
        }
    }
}

/// A deleted folder and where it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Tombstone {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct OwnerFolders {
    folders: Vec<PlaylistFolder>,
    /// Playlist id → folder id.
    places: BTreeMap<String, String>,
    tombstones: Vec<Tombstone>,
    #[serde(skip_serializing_if = "Option::is_none", with = "datetime::utc_option")]
    changed: Option<DateTime<Utc>>,
}

/// Where something filed under a folder id goes.
enum Target {
    Live(String),
    Top,
    Missing,
}

impl OwnerFolders {
    fn live(&self, id: &str) -> Option<&PlaylistFolder> {
        self.folders.iter().find(|folder| folder.id == id)
    }

    fn tombstone(&self, id: &str) -> Option<&Tombstone> {
        self.tombstones.iter().find(|stone| stone.id == id)
    }

    /// The folder itself while it lives; for a deleted one, its nearest live folder above (or the
    /// top); nothing for an id never seen.
    fn target(&self, id: &str) -> Target {
        if self.live(id).is_some() {
            return Target::Live(id.to_string());
        }
        let Some(mut stone) = self.tombstone(id) else {
            return Target::Missing;
        };
        for _ in 0..=self.tombstones.len() {
            let Some(parent) = stone.parent.as_deref() else {
                return Target::Top;
            };
            if self.live(parent).is_some() {
                return Target::Live(parent.to_string());
            }
            match self.tombstone(parent) {
                Some(next) => stone = next,
                None => return Target::Top,
            }
        }
        Target::Top
    }

    fn parent_of(&self, id: &str) -> Option<&str> {
        self.live(id).and_then(|folder| folder.parent.as_deref())
    }

    /// How deep a live folder sits: 1 at the top.
    fn depth(&self, id: &str) -> usize {
        let mut depth = 1;
        let mut at = self.parent_of(id);
        while let Some(parent) = at {
            depth += 1;
            if depth > self.folders.len() + 1 {
                break;
            }
            at = self.parent_of(parent);
        }
        depth
    }

    /// Whether `ancestor` is `id` or a folder above it.
    fn is_within(&self, id: &str, ancestor: &str) -> bool {
        let mut at = Some(id);
        let mut steps = 0;
        while let Some(current) = at {
            if current == ancestor {
                return true;
            }
            steps += 1;
            if steps > self.folders.len() + 1 {
                return false;
            }
            at = self.parent_of(current);
        }
        false
    }

    /// The levels a folder and the folders inside it take: 1 for one with none inside.
    fn height(&self, id: &str, budget: usize) -> usize {
        if budget == 0 {
            return 1;
        }
        1 + self
            .folders
            .iter()
            .filter(|folder| folder.parent.as_deref() == Some(id))
            .map(|folder| self.height(&folder.id, budget - 1))
            .max()
            .unwrap_or(0)
    }

    fn state(&self) -> FolderState {
        FolderState {
            changed: self.changed.unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
            folders: self.folders.clone(),
            places: self
                .places
                .iter()
                .map(|(playlist, folder)| (playlist.clone(), folder.clone()))
                .collect(),
        }
    }
}

/// The store behind `playlist-folders.json`.
pub struct PlaylistFolderStore {
    owners: Mutex<BTreeMap<String, OwnerFolders>>,
    path: Option<PathBuf>,
    save_lock: Mutex<()>,
    load_warning: Option<String>,
}

impl PlaylistFolderStore {
    pub fn in_memory() -> Self {
        Self::new(None::<PathBuf>)
    }

    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = store_file::clean_path(path);
        let (owners, load_warning) =
            store_file::load_in::<BTreeMap<String, OwnerFolders>>(path.as_deref(), AREA);
        PlaylistFolderStore {
            owners: Mutex::new(owners),
            path,
            save_lock: Mutex::new(()),
            load_warning,
        }
    }

    pub fn load_warning(&self) -> Option<&str> {
        self.load_warning.as_deref()
    }

    /// `owner`'s folders and places.
    pub fn state(&self, owner: &str) -> FolderState {
        self.owners
            .lock()
            .get(&owner_key(owner))
            .map(OwnerFolders::state)
            .unwrap_or_else(|| OwnerFolders::default().state())
    }

    /// Makes or changes the folder `id`: its name (trimmed, 1 to 100 characters) and its parent
    /// (none for the top). A deleted id is refused; a deleted parent puts it under the parent's
    /// nearest live folder, and an unknown one at the top.
    pub fn save_folder(
        &self,
        owner: &str,
        id: &str,
        name: &str,
        parent: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FolderState, FolderError> {
        if !is_folder_id(id) {
            return Err(FolderError::Id);
        }
        let name = name.trim();
        let length = name.chars().count();
        if length == 0 || length > MAX_NAME {
            return Err(FolderError::Name);
        }
        let parent = parent.map(str::trim).filter(|p| !p.is_empty());
        if parent.is_some_and(|p| !is_folder_id(p)) {
            return Err(FolderError::Id);
        }
        self.change(owner, |folders| {
            if folders.tombstone(id).is_some() {
                return Err(FolderError::NotFound);
            }
            let parent = match parent.map(|p| folders.target(p)) {
                Some(Target::Live(p)) => Some(p),
                None | Some(Target::Top | Target::Missing) => None,
            };
            if let Some(p) = &parent
                && folders.is_within(p, id)
            {
                return Err(FolderError::Cycle);
            }
            let existing = folders.folders.iter().position(|folder| folder.id == id);
            let height = if existing.is_some() {
                folders.height(id, MAX_FOLDERS)
            } else {
                1
            };
            let depth = parent.as_deref().map_or(1, |p| folders.depth(p) + 1);
            if depth + height - 1 > MAX_DEPTH {
                return Err(FolderError::Depth);
            }
            match existing {
                Some(at) => {
                    let folder = &mut folders.folders[at];
                    if folder.name == name && folder.parent == parent {
                        return Ok(false);
                    }
                    folder.name = name.to_string();
                    folder.parent = parent;
                }
                None => {
                    if folders.folders.len() >= MAX_FOLDERS {
                        return Err(FolderError::TooManyFolders);
                    }
                    folders.folders.push(PlaylistFolder {
                        id: id.to_string(),
                        name: name.to_string(),
                        parent,
                        created: now,
                    });
                }
            }
            folders.changed = Some(now);
            Ok(true)
        })
    }

    /// Deletes the folder `id`: its folders and playlists move to its parent (or the top), and
    /// it is tombstoned. Deleting a deleted folder changes nothing; an id never seen is refused.
    pub fn delete_folder(
        &self,
        owner: &str,
        id: &str,
        now: DateTime<Utc>,
    ) -> Result<FolderState, FolderError> {
        if !is_folder_id(id) {
            return Err(FolderError::Id);
        }
        self.change(owner, |folders| {
            if folders.tombstone(id).is_some() {
                return Ok(false);
            }
            let Some(at) = folders.folders.iter().position(|folder| folder.id == id) else {
                return Err(FolderError::NotFound);
            };
            let parent = folders.folders.remove(at).parent;
            for folder in &mut folders.folders {
                if folder.parent.as_deref() == Some(id) {
                    folder.parent = parent.clone();
                }
            }
            match &parent {
                Some(up) => {
                    for folder in folders.places.values_mut() {
                        if folder.as_str() == id {
                            *folder = up.clone();
                        }
                    }
                }
                None => folders.places.retain(|_, folder| folder.as_str() != id),
            }
            folders.tombstones.push(Tombstone {
                id: id.to_string(),
                parent,
            });
            folders.changed = Some(now);
            Ok(true)
        })
    }

    /// Files `playlist_ids` in the folder `folder`, or at the top when it is none. A deleted folder
    /// files them in its nearest live folder; an id never seen is refused, and so are Octo's own
    /// generated lists.
    pub fn set_places(
        &self,
        owner: &str,
        playlist_ids: &[String],
        folder: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<FolderState, FolderError> {
        let ids: Vec<&str> = playlist_ids
            .iter()
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .collect();
        if ids.iter().any(|id| is_generated(id)) {
            return Err(FolderError::OwnList);
        }
        let folder = folder.map(str::trim).filter(|f| !f.is_empty());
        if folder.is_some_and(|f| !is_folder_id(f)) {
            return Err(FolderError::Id);
        }
        self.change(owner, |folders| {
            let target = match folder.map(|f| folders.target(f)) {
                Some(Target::Live(f)) => Some(f),
                None | Some(Target::Top) => None,
                Some(Target::Missing) => return Err(FolderError::NotFound),
            };
            if target.is_some() {
                let added: HashSet<&str> = ids
                    .iter()
                    .copied()
                    .filter(|id| !folders.places.contains_key(*id))
                    .collect();
                if folders.places.len() + added.len() > MAX_PLACES {
                    return Err(FolderError::TooManyPlaces);
                }
            }
            let mut changed = false;
            for id in &ids {
                match &target {
                    Some(f) => {
                        if folders.places.get(*id) != Some(f) {
                            folders.places.insert((*id).to_string(), f.clone());
                            changed = true;
                        }
                    }
                    None => changed |= folders.places.remove(*id).is_some(),
                }
            }
            if changed {
                folders.changed = Some(now);
            }
            Ok(changed)
        })
    }

    /// Takes `playlist_ids` out of `owner`'s folders (a deleted or gone playlist). True when
    /// anything changed.
    pub fn remove_places(&self, owner: &str, playlist_ids: &[String], now: DateTime<Utc>) -> bool {
        let changed = {
            let mut owners = self.owners.lock();
            let Some(folders) = owners.get_mut(&owner_key(owner)) else {
                return false;
            };
            let mut changed = false;
            for id in playlist_ids {
                changed |= folders.places.remove(id.as_str()).is_some();
            }
            if changed {
                folders.changed = Some(now);
            }
            changed
        };
        if changed {
            self.save();
        }
        changed
    }

    /// Runs `change` on `owner`'s folders under the store's mutex (it answers whether anything
    /// changed, after checking everything before it changes anything) and saves after letting
    /// the mutex go.
    fn change(
        &self,
        owner: &str,
        change: impl FnOnce(&mut OwnerFolders) -> Result<bool, FolderError>,
    ) -> Result<FolderState, FolderError> {
        let (state, changed) = {
            let mut owners = self.owners.lock();
            let folders = owners.entry(owner_key(owner)).or_default();
            let changed = change(folders)?;
            (folders.state(), changed)
        };
        if changed {
            self.save();
        }
        Ok(state)
    }

    fn save(&self) {
        store_file::save_with_in(self.path.as_deref(), &self.save_lock, AREA, || {
            self.owners.lock().clone()
        });
    }
}

/// `pf` and 20 letters or digits.
pub fn is_folder_id(id: &str) -> bool {
    id.len() == 22 && id.starts_with("pf") && id.bytes().skip(2).all(|b| b.is_ascii_alphanumeric())
}

/// Octo's generated lists (stations, mixes, Liked Songs), which stay in From Octo.
pub fn is_generated(id: &str) -> bool {
    id.len() == 22 && (id.starts_with("or") || id.starts_with("og"))
}

fn owner_key(owner: &str) -> String {
    owner.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn fid(n: u32) -> String {
        format!("pf{n:020}")
    }

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 14, 11, minute, 0)
            .single()
            .expect("a time")
    }

    fn parent_of(state: &FolderState, id: &str) -> Option<String> {
        state
            .folders
            .iter()
            .find(|folder| folder.id == id)
            .expect("a folder")
            .parent
            .clone()
    }

    fn place_of(state: &FolderState, playlist: &str) -> Option<String> {
        state
            .places
            .iter()
            .find(|(p, _)| p == playlist)
            .map(|(_, folder)| folder.clone())
    }

    #[test]
    fn folders_are_made_renamed_and_moved_per_person() {
        let store = PlaylistFolderStore::in_memory();
        store
            .save_folder("alice", &fid(1), " Road trips ", None, at(0))
            .expect("made");
        let state = store
            .save_folder("alice", &fid(2), "Summer", Some(&fid(1)), at(1))
            .expect("made");
        assert_eq!(parent_of(&state, &fid(2)), Some(fid(1)));
        assert_eq!(state.folders[0].name, "Road trips");
        let state = store
            .save_folder("ALICE", &fid(2), "Summer 2026", None, at(2))
            .expect("moved");
        assert_eq!(parent_of(&state, &fid(2)), None);
        assert_eq!(state.folders[1].name, "Summer 2026");
        assert_eq!(state.changed, at(2));
        // The same call again changes nothing.
        let again = store
            .save_folder("alice", &fid(2), "Summer 2026", None, at(9))
            .expect("same");
        assert_eq!(again.changed, at(2));
        assert!(store.state("bob").folders.is_empty());
        assert_eq!(store.state("bob").changed, DateTime::<Utc>::UNIX_EPOCH);
    }

    #[test]
    fn folders_go_four_deep() {
        let store = PlaylistFolderStore::in_memory();
        store
            .save_folder("alice", &fid(1), "One", None, at(0))
            .expect("made");
        for n in 2..=4 {
            store
                .save_folder("alice", &fid(n), "Deeper", Some(&fid(n - 1)), at(0))
                .expect("made");
        }
        assert_eq!(
            store.save_folder("alice", &fid(5), "Too deep", Some(&fid(4)), at(0)),
            Err(FolderError::Depth)
        );
        store
            .save_folder("alice", &fid(10), "Outer", None, at(0))
            .expect("made");
        store
            .save_folder("alice", &fid(11), "Inner", Some(&fid(10)), at(0))
            .expect("made");
        assert_eq!(
            store.save_folder("alice", &fid(10), "Outer", Some(&fid(3)), at(0)),
            Err(FolderError::Depth),
            "its own folder would sit at depth 5"
        );
        store
            .save_folder("alice", &fid(10), "Outer", Some(&fid(2)), at(0))
            .expect("4 deep with its folder");
    }

    #[test]
    fn a_folder_cannot_go_inside_itself() {
        let store = PlaylistFolderStore::in_memory();
        store
            .save_folder("alice", &fid(1), "Outer", None, at(0))
            .expect("made");
        store
            .save_folder("alice", &fid(2), "Inner", Some(&fid(1)), at(0))
            .expect("made");
        assert_eq!(
            store.save_folder("alice", &fid(1), "Outer", Some(&fid(2)), at(0)),
            Err(FolderError::Cycle)
        );
        assert_eq!(
            store.save_folder("alice", &fid(1), "Outer", Some(&fid(1)), at(0)),
            Err(FolderError::Cycle)
        );
    }

    #[test]
    fn deleting_a_folder_moves_its_contents_up_and_a_stale_save_cannot_bring_it_back() {
        let store = PlaylistFolderStore::in_memory();
        store
            .save_folder("alice", &fid(1), "One", None, at(0))
            .expect("made");
        store
            .save_folder("alice", &fid(2), "Two", Some(&fid(1)), at(0))
            .expect("made");
        store
            .save_folder("alice", &fid(3), "Three", Some(&fid(2)), at(0))
            .expect("made");
        store
            .set_places("alice", &["pl1".to_string()], Some(&fid(2)), at(0))
            .expect("filed");
        store
            .set_places("alice", &["pl2".to_string()], Some(&fid(1)), at(0))
            .expect("filed");

        let state = store.delete_folder("alice", &fid(2), at(1)).expect("deleted");
        assert_eq!(parent_of(&state, &fid(3)), Some(fid(1)));
        assert_eq!(place_of(&state, "pl1"), Some(fid(1)));
        assert_eq!(
            store
                .delete_folder("alice", &fid(2), at(2))
                .expect("again")
                .changed,
            at(1)
        );
        assert_eq!(
            store.save_folder("alice", &fid(2), "Two", None, at(3)),
            Err(FolderError::NotFound)
        );

        let state = store.delete_folder("alice", &fid(1), at(4)).expect("deleted");
        assert_eq!(parent_of(&state, &fid(3)), None);
        assert_eq!(place_of(&state, "pl1"), None);
        assert_eq!(place_of(&state, "pl2"), None);
        assert_eq!(
            store.delete_folder("alice", &fid(99), at(5)),
            Err(FolderError::NotFound)
        );
    }

    #[test]
    fn a_deleted_or_unknown_folder_files_under_its_nearest_live_folder() {
        let store = PlaylistFolderStore::in_memory();
        store
            .save_folder("alice", &fid(1), "One", None, at(0))
            .expect("made");
        store
            .save_folder("alice", &fid(2), "Two", Some(&fid(1)), at(0))
            .expect("made");
        store.delete_folder("alice", &fid(2), at(1)).expect("deleted");

        let state = store
            .save_folder("alice", &fid(5), "Five", Some(&fid(2)), at(2))
            .expect("made");
        assert_eq!(parent_of(&state, &fid(5)), Some(fid(1)));
        let state = store
            .set_places("alice", &["pl1".to_string()], Some(&fid(2)), at(2))
            .expect("filed");
        assert_eq!(place_of(&state, "pl1"), Some(fid(1)));

        store.delete_folder("alice", &fid(1), at(3)).expect("deleted");
        let state = store
            .save_folder("alice", &fid(6), "Six", Some(&fid(2)), at(4))
            .expect("made");
        assert_eq!(parent_of(&state, &fid(6)), None, "no live folder above: the top");
        let state = store
            .save_folder("alice", &fid(7), "Seven", Some(&fid(42)), at(4))
            .expect("made");
        assert_eq!(parent_of(&state, &fid(7)), None, "an unknown parent: the top");
        let state = store
            .set_places("alice", &["pl3".to_string()], Some(&fid(2)), at(5))
            .expect("filed");
        assert_eq!(place_of(&state, "pl3"), None);
        assert_eq!(
            store.set_places("alice", &["pl3".to_string()], Some(&fid(42)), at(5)),
            Err(FolderError::NotFound)
        );
    }

    #[test]
    fn limits_ids_names_and_generated_lists() {
        let store = PlaylistFolderStore::in_memory();
        for n in 0..MAX_FOLDERS as u32 {
            store
                .save_folder("alice", &fid(n), "F", None, at(0))
                .expect("made");
        }
        assert_eq!(
            store.save_folder("alice", &fid(9_999), "F", None, at(0)),
            Err(FolderError::TooManyFolders)
        );
        assert_eq!(
            store.save_folder("bob", "pf123", "F", None, at(0)),
            Err(FolderError::Id)
        );
        assert_eq!(
            store.save_folder("bob", &format!("xx{:020}", 1), "F", None, at(0)),
            Err(FolderError::Id)
        );
        assert_eq!(
            store.save_folder("bob", &fid(1), "   ", None, at(0)),
            Err(FolderError::Name)
        );
        assert_eq!(
            store.save_folder("bob", &fid(1), &"n".repeat(101), None, at(0)),
            Err(FolderError::Name)
        );
        assert!(
            store
                .save_folder("bob", &fid(1), &"n".repeat(100), None, at(0))
                .is_ok()
        );
        assert_eq!(
            store.save_folder("bob", &fid(2), "F", Some("not-an-id"), at(0)),
            Err(FolderError::Id)
        );

        for generated in ["or1234567890123456789A", "og1234567890123456789A"] {
            assert_eq!(
                store.set_places("bob", &[generated.to_string()], Some(&fid(1)), at(0)),
                Err(FolderError::OwnList)
            );
        }
        let ours = [
            "of1234567890123456789A".to_string(),
            "oi1234567890123456789A".to_string(),
        ];
        assert!(store.set_places("bob", &ours, Some(&fid(1)), at(0)).is_ok());

        let many: Vec<String> = (0..MAX_PLACES - 1).map(|n| format!("{n:022}")).collect();
        assert!(
            store.set_places("bob", &many, Some(&fid(1)), at(0)).is_err(),
            "10,001 in all"
        );
        let fewer: Vec<String> = (0..MAX_PLACES - 2).map(|n| format!("{n:022}")).collect();
        assert!(
            store.set_places("bob", &fewer, Some(&fid(1)), at(0)).is_ok(),
            "10,000 in all"
        );

        assert_eq!(FolderError::NotFound.code(), 70);
        assert_eq!(FolderError::Depth.code(), 10);
        assert_eq!(
            FolderError::OwnList.message(),
            "Octo's own lists stay in From Octo."
        );
    }

    #[test]
    fn folders_and_places_survive_a_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("playlist-folders.json");
        let state = {
            let store = PlaylistFolderStore::new(Some(&path));
            store
                .save_folder("alice", &fid(1), "Road trips", None, at(0))
                .expect("made");
            store
                .set_places("alice", &["pl1".to_string()], Some(&fid(1)), at(1))
                .expect("filed")
        };
        let reopened = PlaylistFolderStore::new(Some(&path));
        assert_eq!(reopened.state("Alice"), state);
        assert!(reopened.load_warning().is_none());
        assert!(reopened.remove_places("alice", &["pl1".to_string()], at(2)));
        assert!(!reopened.remove_places("alice", &["pl1".to_string()], at(3)));
        assert!(!reopened.remove_places("nobody", &["pl1".to_string()], at(3)));
        assert_eq!(
            PlaylistFolderStore::new(Some(&path)).state("alice").places,
            Vec::new()
        );
    }
}
