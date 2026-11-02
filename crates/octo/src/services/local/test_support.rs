//! A hand-written stand-in `ILocalLibraryService` for the tests of the services that take one.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use octo_core::models::domain::Song;
use octo_core::models::subsonic::ScanStatus;
use parking_lot::Mutex;

use super::{ILocalLibraryService, LocalSongMapping, ParsedExternalId, ParsedSongId};

/// Knows nothing unless told: `find_mapping_by_tags` answers the mappings set up for exactly
/// those tags (artist, title and album, a missing album matching only a missing one), scans
/// are counted and succeed, and every id is local.
#[derive(Default)]
pub(crate) struct FakeLocalLibrary {
    pub by_tags: Mutex<Vec<(String, String, Option<String>, LocalSongMapping)>>,
    pub scans: AtomicUsize,
    /// Every move of the records at one path to another, in order.
    pub moved: Mutex<Vec<(String, String)>>,
    /// The library file of an outside song, by its id.
    pub files: Mutex<Vec<(String, String)>>,
    /// Every path a download was registered at, in order.
    pub registered: Mutex<Vec<String>>,
    /// Every path dropped mappings were put back at, in order.
    pub restored: Mutex<Vec<String>>,
}

impl FakeLocalLibrary {
    pub fn with_mapping(artist: &str, title: &str, album: Option<&str>, mapping: LocalSongMapping) -> Self {
        let fake = FakeLocalLibrary::default();
        fake.by_tags
            .lock()
            .push((artist.into(), title.into(), album.map(str::to_string), mapping));
        fake
    }

    pub fn scans(&self) -> usize {
        self.scans.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ILocalLibraryService for FakeLocalLibrary {
    async fn get_local_path_for_external_song(&self, _: &str, external_id: &str) -> Option<String> {
        self.files
            .lock()
            .iter()
            .find(|(id, _)| id == external_id)
            .map(|(_, path)| path.clone())
    }

    async fn register_downloaded_song(&self, _: &Song, local_path: &str) -> anyhow::Result<()> {
        self.registered.lock().push(local_path.to_string());
        Ok(())
    }

    async fn get_local_id_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    fn parse_song_id(&self, _: &str) -> ParsedSongId {
        (false, None, None)
    }

    fn parse_external_id(&self, _: &str) -> ParsedExternalId {
        (false, None, None, None)
    }

    async fn get_mappings(&self) -> Vec<LocalSongMapping> {
        self.by_tags.lock().iter().map(|(.., m)| m.clone()).collect()
    }

    async fn find_mapping_by_tags(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) -> Option<LocalSongMapping> {
        self.by_tags
            .lock()
            .iter()
            .find(|(a, t, al, _)| {
                Some(a.as_str()) == artist && Some(t.as_str()) == title && al.as_deref() == album
            })
            .map(|(.., m)| m.clone())
    }

    async fn forget_mapping(
        &self,
        local_path: &str,
    ) -> anyhow::Result<Vec<crate::services::local::LocalSongMapping>> {
        let mut by_tags = self.by_tags.lock();
        let forgotten = by_tags
            .iter()
            .filter(|(.., mapping)| mapping.local_path == local_path)
            .map(|(.., mapping)| mapping.clone())
            .collect();
        by_tags.retain(|(.., mapping)| mapping.local_path != local_path);
        Ok(forgotten)
    }

    async fn forget_mapping_keyed(&self, key: &str) -> anyhow::Result<bool> {
        let mut by_tags = self.by_tags.lock();
        let before = by_tags.len();
        by_tags.retain(|(.., mapping)| mapping.key() != key);
        Ok(by_tags.len() != before)
    }

    async fn restore_mappings(&self, _: &[LocalSongMapping], local_path: &str) -> anyhow::Result<()> {
        self.restored.lock().push(local_path.to_string());
        Ok(())
    }

    async fn trigger_library_scan(&self, _force: bool) -> bool {
        self.scans.fetch_add(1, Ordering::SeqCst);
        true
    }

    async fn update_local_path(&self, old_path: &str, new_path: &str) -> anyhow::Result<usize> {
        self.moved
            .lock()
            .push((old_path.to_string(), new_path.to_string()));
        Ok(1)
    }

    async fn get_scan_status(&self) -> Option<ScanStatus> {
        None
    }
}
