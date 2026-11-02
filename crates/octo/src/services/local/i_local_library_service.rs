//! The local library service trait.

use async_trait::async_trait;
use octo_core::models::domain::Song;
use octo_core::models::subsonic::ScanStatus;

use super::LocalSongMapping;
use super::local_library_service::KeptHeart;
use super::scan_wait::ScanHold;
use crate::services::subsonic::navidrome_version::ServerInfo;

/// What [`ILocalLibraryService::parse_song_id`] answers: whether the id is external, its
/// provider and its external id.
pub type ParsedSongId = (bool, Option<String>, Option<String>);

/// What [`ILocalLibraryService::parse_external_id`] answers: whether the id is external, its
/// provider, its type ("song", "album" or "artist") and its external id.
pub type ParsedExternalId = (bool, Option<String>, Option<String>, Option<String>);

/// Interface for local music library management
#[async_trait]
pub trait ILocalLibraryService: Send + Sync {
    /// Checks if an external song already exists locally
    async fn get_local_path_for_external_song(
        &self,
        external_provider: &str,
        external_id: &str,
    ) -> Option<String>;

    /// The local file of an outside song for one person: a version kept for them (their own gate
    /// passed it, or it is the very audio they heard), else one kept for nobody in particular.
    /// None when only other people's versions are here. With no person, the most recently landed
    /// version. (By default, the song's one file.)
    async fn get_local_path_for_external_song_as(
        &self,
        user: Option<&str>,
        external_provider: &str,
        external_id: &str,
    ) -> Option<String> {
        let _ = user;
        self.get_local_path_for_external_song(external_provider, external_id)
            .await
    }

    /// Registers a downloaded song in the local library. `Err` when the mappings file could not
    /// be written.
    async fn register_downloaded_song(&self, song: &Song, local_path: &str) -> anyhow::Result<()>;

    /// [`Self::register_downloaded_song`] for the hearts and plays whose own gate passed the file,
    /// with what they heard, so each person finds their own version of the song. (By default, the
    /// plain registration.)
    async fn register_downloaded_song_as(
        &self,
        song: &Song,
        local_path: &str,
        heart: Option<KeptHeart>,
    ) -> anyhow::Result<()> {
        let _ = heart;
        self.register_downloaded_song(song, local_path).await
    }

    /// Gets the mapping between external ID and local ID
    async fn get_local_id_for_external_song(
        &self,
        external_provider: &str,
        external_id: &str,
    ) -> Option<String>;

    /// Parses a song ID to determine if it is external or local
    fn parse_song_id(&self, song_id: &str) -> ParsedSongId;

    /// Parses an external ID to extract the provider, type and ID
    /// Format: ext-{provider}-{type}-{id} (e.g., ext-deezer-artist-259, ext-deezer-album-96126, ext-deezer-song-12345)
    /// Also supports legacy format: ext-{provider}-{id} (assumes song type)
    fn parse_external_id(&self, id: &str) -> ParsedExternalId;

    /// Every download Octo has a record of. The genre backfill uses this to scope a run to
    /// files Octo itself created, which is the only scope where rewriting a tag is rewriting
    /// our own output rather than someone's hand-curated rip.
    async fn get_mappings(&self) -> Vec<LocalSongMapping>;

    /// The mapping whose tags match, or `None` when there is no match OR more than one.
    /// Ambiguity is a failure, not a coin flip: this feeds a delete, and picking arbitrarily
    /// between two candidates is how you delete the wrong one.
    async fn find_mapping_by_tags(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) -> Option<LocalSongMapping>;

    /// Drop the mapping for a path Octo no longer owns, so a re-acquire is not short-circuited by
    /// a file that has just been moved out of the library. Answers the mappings it dropped, so a
    /// song put back can have them again. `Err` when the mappings file could not be written.
    async fn forget_mapping(&self, local_path: &str) -> anyhow::Result<Vec<LocalSongMapping>>;

    /// Drops the one mapping saved under `key` (a [`LocalSongMapping::key`]), leaving every other
    /// mapping at its path alone. Whether there was one. (A no-op by default, for the test fakes;
    /// the real service writes it.)
    async fn forget_mapping_keyed(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Puts back mappings a removal dropped, pointing at the song's path again. A song mapped
    /// anew since keeps its newer mapping. (A no-op by default, for the test fakes; the real
    /// service writes them.)
    async fn restore_mappings(
        &self,
        _mappings: &[LocalSongMapping],
        _local_path: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Triggers a Subsonic library scan. `force` bypasses the debounce. Needed when a caller
    /// must guarantee the scan actually runs, e.g. after each track of an album download so the
    /// album fills in progressively and the final tracks are never left stranded by a
    /// swallowed trigger.
    async fn trigger_library_scan(&self, force: bool) -> bool;

    /// Starts a scan of one Navidrome library. A call is never dropped: one made while that
    /// library's last scan is still recent is folded into one more scan, which starts once the
    /// wait is over. (By default, a forced scan of everything.)
    async fn trigger_library_scan_of(&self, _library_id: i64) -> bool {
        self.trigger_library_scan(true).await
    }

    /// Gets the current scan status
    async fn get_scan_status(&self) -> Option<ScanStatus>;

    /// Point every mapping at or inside `old_path` at where it is now, after the library tidy
    /// moved it. Answers how many mappings followed.
    async fn update_local_path(&self, old_path: &str, new_path: &str) -> anyhow::Result<usize> {
        let _ = (old_path, new_path);
        Ok(0)
    }

    /// [`Self::update_local_path`] for each `(old, new)` in order, so `a` to `b` then `b` to `c`
    /// ends at `c`. Answers how many mappings followed, each move counted on its own. By default
    /// one move at a time; a service that saves its mappings saves them once for the batch.
    async fn update_local_paths(&self, moves: &[(String, String)]) -> anyhow::Result<usize> {
        let mut moved = 0;
        for (old_path, new_path) in moves {
            moved += self.update_local_path(old_path, new_path).await?;
        }
        Ok(moved)
    }

    /// Starts a scan now, past any hold on Octo's own triggers: the scan the holder itself needs.
    async fn start_scan(&self) -> bool {
        self.trigger_library_scan(true).await
    }

    /// The hold on Octo's own scan triggers, when this library has one.
    fn scan_hold(&self) -> Option<ScanHold> {
        None
    }

    /// Which server answers a ping: its type and version. None when nothing answered.
    async fn get_server_info(&self) -> Option<ServerInfo> {
        None
    }
}
