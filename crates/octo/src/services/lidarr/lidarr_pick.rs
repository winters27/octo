//! When Lidarr is used for a pick, and what a pick does before its search. Lidarr's import picks
//! the album from the files among all of an artist's albums and replaces that album's files, so
//! Lidarr is used only for an artist with no music in the user's Lidarr: no track file, and no
//! audio file lying in the artist's folder for an add's rescan to link. Octo adds what Lidarr
//! lacks unmonitored, waits for the add's refresh and rescan to end, reads the artist again, pins
//! the album to a release that carries the heard recording, waits for the pin's rescan, reads
//! once more, and only then searches. A pick skipped on the way is put back: the pin restored,
//! and what Octo's add created deleted from Lidarr, never from disk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet;
use octo_core::lidarr::LidarrError;
use octo_core::lidarr::lidarr_client::{LidarrArtist, LidarrDeletion, LidarrHistoryEvent};
use octo_core::settings::SettingsStore;
use tracing::{info, warn};

use super::lidarr_heart_acquisition_service::translate_imported_path;
use super::{LidarrClient, LidarrPendingAlbums, PendingAlbum};
use crate::services::subsonic::NavidromeIdentityService;

/// The downloads line of a pick whose album the user's Lidarr monitors: it is looking for it.
pub const ALREADY_LOOKING: &str = "Lidarr is already looking for this album for you";

/// The audio files whose presence in an artist's folder means the artist has music.
const AUDIO_EXTENSIONS: [&str; 14] = [
    "flac", "mp3", "m4a", "aac", "alac", "ogg", "opus", "wav", "aiff", "aif", "ape", "wv", "wma", "dsf",
];

/// An artist as the user's Lidarr holds it now.
#[derive(Debug, Clone, Default)]
pub struct ArtistMusic {
    /// The artist, None when Lidarr does not have it.
    pub artist: Option<LidarrArtist>,
    /// The artist has a track file in Lidarr, or audio lies in its folder.
    pub has_music: bool,
}

/// Reads whether an artist already has music in the user's Lidarr.
#[derive(Clone)]
pub struct LidarrArtistMusic {
    client: Arc<LidarrClient>,
    /// The Lidarr settings and `Library:DownloadPath`, read at use.
    settings: Arc<SettingsStore>,
    nav_identity: NavidromeIdentityService,
}

impl LidarrArtistMusic {
    pub fn new(
        client: Arc<LidarrClient>,
        settings: Arc<SettingsStore>,
        nav_identity: NavidromeIdentityService,
    ) -> Self {
        LidarrArtistMusic {
            client,
            settings,
            nav_identity,
        }
    }

    /// Octo's music root: where Lidarr's root folder lies on Octo's side.
    pub fn octo_root(&self) -> String {
        self.nav_identity.effective_download_path(
            &self
                .settings
                .raw("Library:DownloadPath")
                .unwrap_or_else(|| "/music".to_string()),
        )
    }

    /// Where a path under the root folder Octo selected in Lidarr lies on Octo's side; None when
    /// it cannot be mapped.
    pub fn local(&self, lidarr_path: &str) -> Option<PathBuf> {
        let root = self.settings.current().lidarr.root_folder_path.clone();
        translate_imported_path(lidarr_path, root.as_deref(), &self.octo_root())
            .ok()
            .map(PathBuf::from)
    }

    /// The artist with this MusicBrainz id as the user's Lidarr holds it, and whether it has
    /// music there: a track file of its own, or an audio file lying in its folder, where a rescan
    /// would link it. A missing artist's folder is the one Lidarr would give it under the root
    /// folder Octo selected. A folder Octo cannot map has nothing Octo can list, so only the
    /// track files say.
    pub async fn read(&self, foreign_artist_id: &str) -> Result<ArtistMusic, LidarrError> {
        let artist = self.client.find_artist(foreign_artist_id).await?;
        let has_music = match &artist {
            Some(artist) => {
                self.has_files(artist).await? || self.folder_holds_audio(artist.path.as_deref()).await
            }
            None => {
                let folder = self.client.artist_folder(foreign_artist_id).await?;
                let root = self.settings.current().lidarr.root_folder_path.clone();
                let path = match (root, folder) {
                    (Some(root), Some(folder)) if !dotnet::is_blank(&root) => Some(format!(
                        "{}/{}",
                        root.trim_end_matches('/'),
                        folder.trim_matches('/')
                    )),
                    _ => None,
                };
                self.folder_holds_audio(path.as_deref()).await
            }
        };
        Ok(ArtistMusic { artist, has_music })
    }

    /// Whether Lidarr's database holds a track file of the artist. The artist's statistic is
    /// only a quick first read: above 0 it proves files, at 0 the track file list has the last
    /// word.
    pub async fn has_files(&self, artist: &LidarrArtist) -> Result<bool, LidarrError> {
        if artist.track_file_count.unwrap_or(0) > 0 {
            return Ok(true);
        }
        Ok(!self.client.artist_track_files(artist.id).await?.is_empty())
    }

    async fn folder_holds_audio(&self, lidarr_path: Option<&str>) -> bool {
        let Some(local) = lidarr_path
            .filter(|path| !dotnet::is_blank(path))
            .and_then(|path| self.local(path))
        else {
            return false;
        };
        tokio::task::spawn_blocking(move || holds_audio(&local))
            .await
            .unwrap_or(false)
    }
}

/// Whether an audio file lies anywhere under a folder. Dot folders are skipped, as Lidarr's
/// scans skip them, so nothing there can be linked.
fn holds_audio(folder: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !entry.file_name().to_string_lossy().starts_with('.') && holds_audio(&path) {
                return true;
            }
        } else if path.extension().is_some_and(|extension| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|audio| extension.eq_ignore_ascii_case(audio))
        }) {
            return true;
        }
    }
    false
}

/// Waits, on Lidarr's own command states and never a timer, until no queued or running refresh
/// names the album or its artist, and then until no queued or running rescan covers the artist:
/// a refresh queues its rescan, and only once that rescan ended are the artist's files read
/// again.
pub async fn wait_for_rescans(
    client: &LidarrClient,
    album_id: i32,
    artist_id: i32,
    artist_path: Option<&str>,
    poll: Duration,
) -> Result<(), LidarrError> {
    loop {
        let mut refreshing = false;
        for command in client.commands().await? {
            if command.is_active() && command.refreshes(album_id, artist_id)? {
                refreshing = true;
                break;
            }
        }
        if !refreshing {
            break;
        }
        tokio::time::sleep(poll).await;
    }
    loop {
        let rescanning = client
            .commands()
            .await?
            .iter()
            .any(|command| command.is_active() && command.rescans_artist(artist_id, artist_path));
        if !rescanning {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
}

/// Whether the artist has a queue item that is not one of Octo's own grabs: a download of the
/// user's, or a release Lidarr holds back for them. Such a grab will bring files any import of
/// Octo's could then replace.
pub async fn users_queue_item(
    client: &LidarrClient,
    pending: Option<&LidarrPendingAlbums>,
    artist_id: i32,
) -> Result<bool, LidarrError> {
    if artist_id <= 0 {
        return Ok(false);
    }
    Ok(client
        .artist_queue(artist_id)
        .await?
        .iter()
        .any(|item| !is_octos(pending, item.download_id.as_deref())))
}

fn is_octos(pending: Option<&LidarrPendingAlbums>, download_id: Option<&str>) -> bool {
    match (pending, download_id) {
        (Some(pending), Some(id)) => pending.is_octos_grab(id),
        _ => false,
    }
}

/// What of an artist is the user's: the albums holding a file whose import row is not of one of
/// Octo's own grabs (a row with no client, or a grab id no entry of Octo's lists), and the albums
/// with a queue item that is not Octo's. A file with no import row (one a rescan linked) counts
/// for nobody.
#[derive(Debug, Default)]
struct UsersOwn {
    file_albums: HashSet<i32>,
    queue_albums: HashSet<i32>,
}

impl UsersOwn {
    async fn read(
        client: &LidarrClient,
        pending: Option<&LidarrPendingAlbums>,
        artist_id: i32,
    ) -> Result<UsersOwn, LidarrError> {
        let mut own = UsersOwn::default();
        if artist_id <= 0 {
            return Ok(own);
        }
        let files = client.artist_track_files(artist_id).await?;
        if !files.is_empty() {
            for row in client
                .artist_history(artist_id, Some(LidarrHistoryEvent::TrackFileImported))
                .await?
            {
                let Some(file) = row
                    .file_id()
                    .and_then(|id| files.iter().find(|file| file.id == id))
                else {
                    continue;
                };
                let octos = row.download_client().is_some() && is_octos(pending, row.download_id.as_deref());
                if !octos {
                    own.file_albums.insert(file.album_id);
                }
            }
        }
        for item in client.artist_queue(artist_id).await? {
            if !is_octos(pending, item.download_id.as_deref()) {
                own.queue_albums.insert(item.album_id.unwrap_or(0));
            }
        }
        Ok(own)
    }

    fn keeps(&self, album_id: i32) -> bool {
        self.file_albums.contains(&album_id) || self.queue_albums.contains(&album_id)
    }

    fn any(&self) -> bool {
        !self.file_albums.is_empty() || !self.queue_albums.is_empty()
    }
}

/// Puts back a pick that never searched, then forgets its entry:
/// - the album's release choice goes back to exactly what was saved, when it is still exactly
///   Octo's pin (a choice the user changed stays as the user set it), and the rescan the restore
///   queues is waited out;
/// - an album Octo's add created is deleted from Lidarr with its files left on disk, unless it
///   holds a file the user imported or a queue item of the user's;
/// - an artist Octo's add created is deleted the same way when it is still unmonitored and
///   monitors no new release, none of its albums is monitored or holds anything of the user's,
///   and no other entry of Octo's uses it.
///
/// Files a rescan linked, with no import row, never keep what Octo created: deleting it leaves
/// them where they lie.
pub async fn put_back_unsearched(
    client: &LidarrClient,
    pending: Option<&LidarrPendingAlbums>,
    entry: &PendingAlbum,
    poll: Duration,
) -> Result<(), LidarrError> {
    let album = match entry.lidarr_album_id {
        id if id > 0 => client.album(id).await?,
        _ => None,
    };
    if let (Some(album), Some(saved), Some(pin)) = (&album, &entry.release_choice, &entry.pin)
        && album.release_choice() == *pin
    {
        client.set_release_choice(album.id, saved).await?;
        let path = client.artist(album.artist_id).await?.and_then(|a| a.path);
        wait_for_rescans(client, album.id, album.artist_id, path.as_deref(), poll).await?;
    }
    let artist_id = album
        .as_ref()
        .map(|album| album.artist_id)
        .filter(|id| *id > 0)
        .unwrap_or(entry.lidarr_artist_id);
    let own = UsersOwn::read(client, pending, artist_id).await?;
    if entry.album_added
        && let Some(album) = &album
    {
        if own.keeps(album.id) {
            info!(
                "Lidarr's album {} holds something of the user's; it stays",
                album.id
            );
        } else if client.delete_album(album.id).await? == LidarrDeletion::Conflict {
            warn!("Lidarr would not delete album {} now", album.id);
        }
    }
    if entry.artist_added > 0 && artist_is_octos(client, pending, entry, &own).await? {
        client.delete_artist(entry.artist_added).await?;
    }
    if let Some(pending) = pending {
        pending.forget(&entry.foreign_album_id);
    }
    Ok(())
}

/// Whether the artist Octo's add created is still only Octo's.
async fn artist_is_octos(
    client: &LidarrClient,
    pending: Option<&LidarrPendingAlbums>,
    entry: &PendingAlbum,
    own: &UsersOwn,
) -> Result<bool, LidarrError> {
    let Some(artist) = client.artist(entry.artist_added).await? else {
        return Ok(false);
    };
    let untouched = !artist.monitored
        && artist
            .monitor_new_items
            .as_deref()
            .is_some_and(|items| dotnet::eq_ignore_case(items, "none"));
    if !untouched || own.any() {
        return Ok(false);
    }
    if client
        .artist_albums(artist.id)
        .await?
        .iter()
        .any(|album| album.monitored)
    {
        return Ok(false);
    }
    Ok(!pending.is_some_and(|pending| pending.uses_artist(artist.id, &entry.foreign_album_id)))
}

/// After a restart: every entry whose search Octo never posted is put back. An add sent right
/// before the restart, with nothing saved of what it created, is read from Lidarr: the album, which
/// was not in Lidarr at the read right before the add, is the add's when Lidarr has it now; the
/// artist is the add's when Lidarr has it now, it was not among the artists read right before the
/// add, and Lidarr added it at or after the add was sent.
pub async fn put_back_unsearched_after_restart(
    client: &LidarrClient,
    pending: &LidarrPendingAlbums,
    poll: Duration,
) {
    for mut entry in pending.entries().into_iter().filter(|entry| !entry.searched()) {
        if let Some(add) = entry.add.clone() {
            if !entry.album_added {
                match client.album_by_foreign_id(&entry.foreign_album_id).await {
                    Ok(Some(album)) => {
                        entry.album_added = true;
                        entry.lidarr_album_id = album.id;
                        entry.lidarr_artist_id = album.artist_id;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(
                            "Could not read Lidarr's album {} after a restart: {e}",
                            entry.foreign_album_id
                        );
                        continue;
                    }
                }
            }
            if entry.artist_added <= 0 {
                match client.find_artist(&add.foreign_artist_id).await {
                    Ok(Some(artist))
                        if !add.artists_before.contains(&artist.id)
                            && artist.added.is_none_or(|added| added >= add.posted_at) =>
                    {
                        entry.artist_added = artist.id;
                        entry.lidarr_artist_id = artist.id;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(
                            "Could not read Lidarr's artist {} after a restart: {e}",
                            add.foreign_artist_id
                        );
                        continue;
                    }
                }
            }
        }
        match put_back_unsearched(client, Some(pending), &entry, poll).await {
            Ok(()) => info!(
                "Put back Lidarr's album {} that a restart cut short before its search",
                entry.foreign_album_id
            ),
            Err(e) => warn!(
                "Could not put back Lidarr's album {} after a restart: {e}",
                entry.foreign_album_id
            ),
        }
    }
}

/// Deletes the Lidarr record of a track file Octo moved into its own layout, so the artist reads
/// as having no files and stays usable. Only when no file lies at the recorded path any more: a
/// record whose file is still there is left, since deleting it would delete the file. A record
/// Lidarr keeps for now (its root folder missing or empty) is saved on the entry and asked again
/// later; one Lidarr no longer has counts as deleted.
pub async fn delete_moved_record(
    client: &LidarrClient,
    pending: Option<&LidarrPendingAlbums>,
    foreign_album_id: &str,
    track_file_id: i32,
    local_path: &str,
) {
    if track_file_id <= 0 || Path::new(local_path).exists() {
        return;
    }
    match client.delete_track_file_record(track_file_id).await {
        Ok(LidarrDeletion::Deleted | LidarrDeletion::NotFound) => {
            if let Some(pending) = pending {
                pending.update(foreign_album_id, |entry| {
                    entry.records_to_delete.retain(|id| *id != track_file_id);
                });
            }
        }
        Ok(LidarrDeletion::Conflict) => {
            info!("Lidarr keeps the record of track file {track_file_id} for now; asking again later");
            if let Some(pending) = pending {
                pending.update(foreign_album_id, |entry| {
                    if !entry.records_to_delete.contains(&track_file_id) {
                        entry.records_to_delete.push(track_file_id);
                    }
                });
            }
        }
        Err(e) => warn!("Could not delete the record of Lidarr track file {track_file_id}: {e}"),
    }
}

/// Asks Lidarr again to delete the records it kept, for the entry of this album.
pub async fn retry_moved_records(
    client: &LidarrClient,
    pending: &LidarrPendingAlbums,
    foreign_album_id: &str,
) {
    let Some(entry) = pending.get(foreign_album_id) else {
        return;
    };
    for track_file_id in entry.records_to_delete {
        match client.delete_track_file_record(track_file_id).await {
            Ok(LidarrDeletion::Deleted | LidarrDeletion::NotFound) => {
                pending.update(foreign_album_id, |entry| {
                    entry.records_to_delete.retain(|id| *id != track_file_id);
                });
            }
            Ok(LidarrDeletion::Conflict) => {}
            Err(e) => warn!("Could not delete the record of Lidarr track file {track_file_id}: {e}"),
        }
    }
}

#[cfg(test)]
#[path = "lidarr_pick_tests.rs"]
mod tests;
