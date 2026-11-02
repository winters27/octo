//! `family-pool.json`: the one real copy of every song a member downloaded, under
//! `<members>/.pool/`, and which members link to it from their own library. This file is the
//! record of who links what; Octo never counts links by walking folders. It also keeps what
//! Octo has learned about links on this server (Navidrome showed one, or never does), so a
//! restart does not forget it.
//!
//! The file is `{"Links": {...}, "Files": [...]}`; a plain list of files is read too.

use std::path::{Path, PathBuf};

use octo_core::common::dotnet::eq_ignore_case;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::store_file;

/// What Octo has learned about links in members' libraries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LinkState {
    /// Navidrome has shown a linked song in a member's library at least once.
    #[serde(default)]
    pub seen: bool,
    /// How many songs had a link that a finished Navidrome scan did not show, and a copy in its
    /// place that Navidrome then showed. Each song counts once.
    #[serde(default)]
    pub unshown: u32,
    /// The pool files of the songs counted in `unshown` (the most recent [`MISSED_KEPT`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missed: Vec<String>,
    /// Links do not work for Navidrome here: members get real copies from now on.
    #[serde(default)]
    pub copies: bool,
}

/// How many songs [`LinkState::missed`] keeps.
pub const MISSED_KEPT: usize = 64;

/// The file as it is written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PoolDocument {
    #[serde(default)]
    links: LinkState,
    #[serde(default)]
    files: Vec<PoolFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    copies: Vec<CopySource>,
}

/// A pool file that is a copy of a shared library's file, made for members who cannot see that
/// library, so the next member who needs it gets the same copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CopySource {
    /// The pool file.
    pub path: String,
    /// The shared library's file it was copied from.
    pub source: String,
}

/// The file as it is read: the plain list of files it once was, or the document. The list is
/// tried first: a document is never a list, but a list could pass for a document.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredPool {
    Files(Vec<PoolFile>),
    Document(PoolDocument),
}

impl Default for StoredPool {
    fn default() -> Self {
        StoredPool::Document(PoolDocument::default())
    }
}

/// One file in the pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PoolFile {
    /// The file's path inside the container (`/members/.pool/<Artist>/<Album>/<file>`).
    pub path: String,
    /// Bytes.
    #[serde(default)]
    pub size: u64,
    /// The file's format (`flac`, `mp3`, ...).
    #[serde(default)]
    pub format: String,
    /// The outside ids this file is a copy of.
    #[serde(default)]
    pub outside_ids: Vec<String>,
    #[serde(default)]
    pub isrc: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    /// The members whose library links to this file.
    #[serde(default)]
    pub refs: Vec<String>,
    /// For a FLAC file the spectrum check found was made from lossy audio, what it was likely
    /// made from ("about 128 kbps MP3"). None for a real FLAC and for every other format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcoded_from: Option<String>,
}

impl PoolFile {
    /// A FLAC file that was not made from lossy audio: what a FLAC request takes.
    pub fn is_real_flac(&self) -> bool {
        self.format.eq_ignore_ascii_case("flac") && self.transcoded_from.is_none()
    }
}

/// The store behind `family-pool.json`.
pub struct FamilyPoolStore {
    files: Mutex<Vec<PoolFile>>,
    links: Mutex<LinkState>,
    copies: Mutex<Vec<CopySource>>,
    path: Option<PathBuf>,
    save_lock: Mutex<()>,
    load_warning: Option<String>,
}

impl FamilyPoolStore {
    pub fn in_memory() -> Self {
        Self::new(None::<PathBuf>)
    }

    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = store_file::clean_path(path);
        let (stored, load_warning) = store_file::load::<StoredPool>(path.as_deref());
        let document = match stored {
            StoredPool::Document(document) => document,
            StoredPool::Files(files) => PoolDocument {
                links: LinkState::default(),
                files,
                copies: Vec::new(),
            },
        };
        FamilyPoolStore {
            files: Mutex::new(document.files),
            links: Mutex::new(document.links),
            copies: Mutex::new(document.copies),
            path,
            save_lock: Mutex::new(()),
            load_warning,
        }
    }

    pub fn load_warning(&self) -> Option<&str> {
        self.load_warning.as_deref()
    }

    /// What Octo has learned about links.
    pub fn link_state(&self) -> LinkState {
        self.links.lock().clone()
    }

    /// Changes what Octo has learned about links, writing the file when it changed.
    pub fn set_link_state(&self, change: impl FnOnce(&mut LinkState)) {
        let changed = {
            let mut links = self.links.lock();
            let before = links.clone();
            change(&mut links);
            before != *links
        };
        if changed {
            self.save();
        }
    }

    pub fn get(&self, path: &str) -> Option<PoolFile> {
        self.files.lock().iter().find(|f| f.path == path).cloned()
    }

    pub fn list(&self) -> Vec<PoolFile> {
        self.files.lock().clone()
    }

    /// The pool file that is a copy of this outside id.
    pub fn find_by_outside_id(&self, outside_id: &str) -> Option<PoolFile> {
        self.files
            .lock()
            .iter()
            .find(|f| f.outside_ids.iter().any(|id| id == outside_id))
            .cloned()
    }

    /// The pool file with this ISRC (ignoring case).
    pub fn find_by_isrc(&self, isrc: &str) -> Option<PoolFile> {
        let isrc = isrc.trim();
        if isrc.is_empty() {
            return None;
        }
        self.files
            .lock()
            .iter()
            .find(|f| f.isrc.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(isrc)))
            .cloned()
    }

    /// Adds a file, or replaces the record with the same path (keeping its refs when the new
    /// record has none).
    pub fn upsert(&self, mut file: PoolFile) {
        {
            let mut files = self.files.lock();
            match files.iter_mut().find(|f| f.path == file.path) {
                Some(existing) => {
                    if file.refs.is_empty() {
                        file.refs = std::mem::take(&mut existing.refs);
                    }
                    *existing = file;
                }
                None => files.push(file),
            }
        }
        self.save();
    }

    /// Records that `user` links this file. False when there is no such file or they already
    /// did.
    pub fn add_ref(&self, path: &str, user: &str) -> bool {
        let added = {
            let mut files = self.files.lock();
            match files.iter_mut().find(|f| f.path == path) {
                Some(file) if !file.refs.iter().any(|r| eq_ignore_case(r, user)) => {
                    file.refs.push(user.to_string());
                    true
                }
                _ => false,
            }
        };
        if added {
            self.save();
        }
        added
    }

    /// Records that `user` no longer links this file and answers how many members still do;
    /// None when there is no such file. A file nobody links stays recorded until
    /// [`FamilyPoolStore::remove`] (the caller trashes it first).
    pub fn remove_ref(&self, path: &str, user: &str) -> Option<usize> {
        let left = {
            let mut files = self.files.lock();
            let file = files.iter_mut().find(|f| f.path == path)?;
            file.refs.retain(|r| !eq_ignore_case(r, user));
            file.refs.len()
        };
        self.save();
        Some(left)
    }

    /// Forgets a file, and what it was copied from. False when it was not recorded.
    pub fn remove(&self, path: &str) -> bool {
        let removed = {
            let mut files = self.files.lock();
            let before = files.len();
            files.retain(|f| f.path != path);
            before != files.len()
        };
        let uncopied = {
            let mut copies = self.copies.lock();
            let before = copies.len();
            copies.retain(|copy| copy.path != path);
            before != copies.len()
        };
        if removed || uncopied {
            self.save();
        }
        removed
    }

    /// Records that the pool file at `path` is a copy of the shared file `source`.
    pub fn set_source(&self, path: &str, source: &str) {
        {
            let mut copies = self.copies.lock();
            if copies
                .iter()
                .any(|copy| copy.path == path && copy.source == source)
            {
                return;
            }
            copies.retain(|copy| copy.path != path);
            copies.push(CopySource {
                path: path.to_string(),
                source: source.to_string(),
            });
        }
        self.save();
    }

    /// The recorded pool file that is a copy of the shared file `source`.
    pub fn find_by_source(&self, source: &str) -> Option<PoolFile> {
        let path = self
            .copies
            .lock()
            .iter()
            .find(|copy| copy.source == source)
            .map(|copy| copy.path.clone())?;
        self.get(&path)
    }

    /// Follows shared files the library tidy moved: each copy names where its source is now, for
    /// each `(from, to)` in order, saved once. Answers how many followed, each move counted on
    /// its own.
    pub fn move_paths_batch(&self, moves: &[(String, String)]) -> usize {
        let mut moved = 0;
        {
            let mut copies = self.copies.lock();
            for (from, to) in moves {
                for copy in copies.iter_mut() {
                    if let Some(now) = octo_core::common::PathHelper::moved_path(&copy.source, from, to) {
                        copy.source = now;
                        moved += 1;
                    }
                }
            }
        }
        if moved > 0 {
            self.save();
        }
        moved
    }

    /// Moves a file's record from `from` to `to`, its links and ids with it, in place of any
    /// record already at `to`. False when `from` is not recorded.
    pub fn move_file(&self, from: &str, to: &str) -> bool {
        if from == to {
            return self.get(from).is_some();
        }
        let moved = {
            let mut files = self.files.lock();
            if !files.iter().any(|f| f.path == from) {
                false
            } else {
                files.retain(|f| f.path != to);
                if let Some(file) = files.iter_mut().find(|f| f.path == from) {
                    file.path = to.to_string();
                }
                true
            }
        };
        if moved {
            {
                let mut copies = self.copies.lock();
                copies.retain(|copy| copy.path != to);
                for copy in copies.iter_mut().filter(|copy| copy.path == from) {
                    copy.path = to.to_string();
                }
            }
            self.save();
        }
        moved
    }

    /// The files `user` links.
    pub fn refs_of(&self, user: &str) -> Vec<PoolFile> {
        self.files
            .lock()
            .iter()
            .filter(|f| f.refs.iter().any(|r| eq_ignore_case(r, user)))
            .cloned()
            .collect()
    }

    /// The bytes of every file `user` links, each at full size.
    pub fn bytes_linked_by(&self, user: &str) -> u64 {
        self.refs_of(user).iter().map(|f| f.size).sum()
    }

    fn save(&self) {
        store_file::save_with(self.path.as_deref(), &self.save_lock, || PoolDocument {
            links: self.links.lock().clone(),
            files: self.files.lock().clone(),
            copies: self.copies.lock().clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, refs: &[&str]) -> PoolFile {
        PoolFile {
            path: path.to_string(),
            size: 30_000_000,
            format: "flac".into(),
            outside_ids: vec!["ext-deezer-song-1".into()],
            isrc: Some("USUM71703861".into()),
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
            transcoded_from: None,
        }
    }

    const POOL: &str = "/members/.pool/Artist/Album/01 Song.flac";

    #[test]
    fn lookups_by_outside_id_and_isrc() {
        let store = FamilyPoolStore::in_memory();
        store.upsert(file(POOL, &["sam"]));
        assert_eq!(
            store.find_by_outside_id("ext-deezer-song-1").expect("found").path,
            POOL
        );
        assert!(store.find_by_outside_id("ext-deezer-song-2").is_none());
        assert_eq!(store.find_by_isrc(" usum71703861 ").expect("found").path, POOL);
        assert!(store.find_by_isrc("").is_none());
    }

    #[test]
    fn refs_count_each_member_once() {
        let store = FamilyPoolStore::in_memory();
        store.upsert(file(POOL, &["sam"]));
        assert!(store.add_ref(POOL, "alex"));
        assert!(!store.add_ref(POOL, "ALEX"));
        assert!(!store.add_ref("/members/.pool/none.flac", "alex"));
        assert_eq!(store.bytes_linked_by("alex"), 30_000_000);
        assert_eq!(store.remove_ref(POOL, "sam"), Some(1));
        assert_eq!(store.remove_ref(POOL, "alex"), Some(0));
        assert_eq!(store.remove_ref("/nowhere", "alex"), None);
        assert!(store.get(POOL).is_some(), "kept until the caller removes it");
        assert!(store.remove(POOL));
        assert!(!store.remove(POOL));
    }

    #[test]
    fn upsert_keeps_refs_and_survives_a_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-pool.json");
        let store = FamilyPoolStore::new(Some(&path));
        store.upsert(file(POOL, &["sam", "alex"]));
        let mut better = file(POOL, &[]);
        better.size = 40_000_000;
        store.upsert(better);
        let reloaded = FamilyPoolStore::new(Some(&path));
        let got = reloaded.get(POOL).expect("there");
        assert_eq!(got.size, 40_000_000);
        assert_eq!(got.refs, ["sam", "alex"]);
        assert_eq!(reloaded.refs_of("sam").len(), 1);
        assert!(reloaded.refs_of("kim").is_empty());
    }

    /// A record moves with its links and what it is a copy of; the copy's source follows the
    /// shared file it was copied from; both survive a restart, and a removed file forgets it.
    #[test]
    fn a_moved_record_keeps_its_links_and_its_source() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-pool.json");
        let store = FamilyPoolStore::new(Some(&path));
        let moved = "/members/.pool/Artist/Album (2004)/01 - Song.flac";
        store.upsert(file(POOL, &["sam", "alex"]));
        store.set_source(POOL, "/music/Artist/Album/01 Song.flac");

        assert!(store.move_file(POOL, moved));
        assert!(!store.move_file(POOL, moved), "nothing is recorded there now");
        assert!(store.get(POOL).is_none());
        assert_eq!(store.get(moved).expect("moved").refs, ["sam", "alex"]);
        assert_eq!(
            store.move_paths_batch(&[
                ("/music/Artist/Album".to_string(), "/music/Artist/B".to_string()),
                ("/music/Artist/B".to_string(), "/music/Artist/C".to_string()),
            ]),
            2
        );
        assert!(store.find_by_source("/music/Artist/Album/01 Song.flac").is_none());

        let reloaded = FamilyPoolStore::new(Some(&path));
        assert_eq!(
            reloaded
                .find_by_source("/music/Artist/C/01 Song.flac")
                .expect("the copy")
                .path,
            moved
        );
        assert!(reloaded.remove(moved));
        assert!(reloaded.find_by_source("/music/Artist/C/01 Song.flac").is_none());
    }

    #[test]
    fn what_was_learned_about_links_survives_a_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-pool.json");
        let store = FamilyPoolStore::new(Some(&path));
        assert_eq!(store.link_state(), LinkState::default());
        store.set_link_state(|links| links.seen = true);
        let mut fake = file(POOL, &["sam"]);
        fake.transcoded_from = Some("about 128 kbps MP3".into());
        store.upsert(fake);
        let reloaded = FamilyPoolStore::new(Some(&path));
        assert!(reloaded.link_state().seen);
        assert!(!reloaded.link_state().copies);
        assert!(!reloaded.get(POOL).expect("there").is_real_flac());
        assert!(file(POOL, &[]).is_real_flac());
    }

    #[test]
    fn a_plain_list_of_files_is_still_read() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-pool.json");
        std::fs::write(
            &path,
            serde_json::to_string(&vec![file(POOL, &["sam"])]).expect("json"),
        )
        .expect("writes");
        let store = FamilyPoolStore::new(Some(&path));
        assert!(store.load_warning().is_none());
        assert_eq!(store.get(POOL).expect("read").refs, ["sam"]);
    }

    #[test]
    fn a_torn_pool_file_is_kept_aside_with_a_warning() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-pool.json");
        let store = FamilyPoolStore::new(Some(&path));
        store.upsert(file(POOL, &["sam", "alex"]));
        let whole = std::fs::read(&path).expect("written");
        std::fs::write(&path, &whole[..whole.len() - 7]).expect("torn by a crash");
        let reloaded = FamilyPoolStore::new(Some(&path));
        assert!(reloaded.load_warning().is_some());
        assert!(reloaded.list().is_empty());
        std::fs::write(&path, b"").expect("emptied by a crash");
        assert!(FamilyPoolStore::new(Some(&path)).load_warning().is_some());
    }

    #[test]
    fn a_save_torn_by_a_crash_leaves_the_last_whole_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-pool.json");
        let store = FamilyPoolStore::new(Some(&path));
        store.upsert(file(POOL, &["sam"]));
        // A crash in the middle of the next save tears its temporary file, never this one.
        let whole = std::fs::read(&path).expect("written");
        let temporary = crate::services::state_file::tmp_path(&path);
        std::fs::write(&temporary, &whole[..whole.len() / 2]).expect("a torn temporary file");
        let reloaded = FamilyPoolStore::new(Some(&path));
        assert!(reloaded.load_warning().is_none(), "{:?}", reloaded.load_warning());
        assert_eq!(reloaded.get(POOL).expect("the last whole file").refs, ["sam"]);
        // The next save writes over the torn temporary file and leaves none behind.
        assert!(reloaded.add_ref(POOL, "alex"));
        assert!(!temporary.exists());
        assert_eq!(
            FamilyPoolStore::new(Some(&path)).get(POOL).expect("saved").refs,
            ["sam", "alex"]
        );
    }

    #[test]
    fn an_empty_or_blank_pool_file_is_broken_not_an_empty_pool() {
        for blank in ["", " \n\t"] {
            let dir = tempfile::tempdir().expect("a temp dir");
            let path = dir.path().join("family-pool.json");
            std::fs::write(&path, blank).expect("left blank by a crash");
            let store = FamilyPoolStore::new(Some(&path));
            assert!(
                store
                    .load_warning()
                    .is_some_and(|w| w.contains("the file is empty")),
                "{blank:?}: {:?}",
                store.load_warning()
            );
            assert!(store.list().is_empty());
            assert!(
                !path.exists(),
                "{blank:?}: kept aside, never written over as an empty pool"
            );
            let kept = std::fs::read_dir(dir.path())
                .expect("lists")
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("family-pool.json.broken-")
                })
                .count();
            assert_eq!(kept, 1, "{blank:?}");
        }
    }

    #[test]
    fn saves_from_many_threads_leave_the_newest_records_on_disk() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-pool.json");
        let store = std::sync::Arc::new(FamilyPoolStore::new(Some(&path)));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let writers: Vec<_> = (0..8)
            .map(|n| {
                let (store, barrier) = (store.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    for m in 0..15 {
                        let path = format!("/members/.pool/A/{n}/{m}.flac");
                        store.upsert(file(&path, &["sam"]));
                        store.add_ref(&path, "alex");
                        if m % 3 == 0 {
                            store.remove_ref(&path, "sam");
                        }
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("joins");
        }
        let mut in_memory = store.list();
        let mut on_disk = FamilyPoolStore::new(Some(&path)).list();
        in_memory.sort_by(|a, b| a.path.cmp(&b.path));
        on_disk.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(in_memory.len(), 120);
        assert_eq!(on_disk, in_memory, "the last save wrote the newest records");
    }
}
