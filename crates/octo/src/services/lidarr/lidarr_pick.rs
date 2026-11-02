//! When Lidarr is used for a pick, and what a pick does before its search. Lidarr's import picks
//! the album from the files among all of an artist's albums and replaces that album's files, so
//! Lidarr is used only for an artist with no music in the user's Lidarr: no track file, and no
//! audio file lying in the artist's folder for an add's rescan to link. Octo adds what Lidarr
//! lacks unmonitored, waits for the add's refresh and rescan to end, reads the artist again, pins
//! the album to a release that carries the heard recording, waits for the pin's rescan, reads
//! once more, and only then searches. A pick skipped on the way is put back: the pin restored,
//! and what Octo's add created deleted from Lidarr, never from disk. A pick whose search ended
//! without a gated import is put back the same way once Octo's own files are deleted (and looked
//! for again after the restore's rescan); one whose import landed only gets its pin restored.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet;
use octo_core::lidarr::LidarrError;
use octo_core::lidarr::lidarr_client::{
    LidarrAlbum, LidarrArtist, LidarrDeletion, LidarrHistoryEvent, LidarrQueueItem, LidarrReleaseChoice,
};
use octo_core::settings::SettingsStore;
use tracing::{info, warn};

use super::lidarr_heart_acquisition_service::{same_path, translate_imported_path};
use super::lidarr_pending_albums::MovedRecord;
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
/// scans skip them, so nothing there can be linked. A folder reached twice (a link back up the
/// tree) is read once.
fn holds_audio(folder: &Path) -> bool {
    holds_audio_once(folder, &mut HashSet::new())
}

fn holds_audio_once(folder: &Path, seen: &mut HashSet<PathBuf>) -> bool {
    let real = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    if !seen.insert(real) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(folder) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !entry.file_name().to_string_lossy().starts_with('.') && holds_audio_once(&path, seen) {
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
    for item in client.artist_queue(artist_id).await? {
        if !octos_item(client, pending, &item).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a row of a download dated `date` (Lidarr's clock) is of one of Octo's own grabs.
fn is_octos_at(
    pending: Option<&LidarrPendingAlbums>,
    download_id: Option<&str>,
    date: chrono::DateTime<chrono::Utc>,
) -> bool {
    match (pending, download_id) {
        (Some(pending), Some(id)) => pending.is_octos_grab_at(id, date),
        _ => false,
    }
}

/// Whether a queue item is one of Octo's own grabs: its download's newest grab row (Lidarr's own
/// date, from that download's history, else the item's own time) lies inside the span of a grab
/// of Octo's with that id. A download id is not unique over time: a torrent the user grabs again
/// after Octo was done with it keeps the id and is the user's.
async fn octos_item(
    client: &LidarrClient,
    pending: Option<&LidarrPendingAlbums>,
    item: &LidarrQueueItem,
) -> Result<bool, LidarrError> {
    let (Some(pending), Some(id)) = (pending, item.download_id.as_deref()) else {
        return Ok(false);
    };
    if !pending.knows_grab(id) {
        return Ok(false);
    }
    let grabbed = client
        .download_history(id)
        .await?
        .iter()
        .filter(|row| row.event == LidarrHistoryEvent::Grabbed)
        .map(|row| row.date)
        .max()
        .or(item.added);
    Ok(grabbed.is_some_and(|date| pending.is_octos_grab_at(id, date)))
}

/// What of an artist is the user's: the albums holding a file whose import row is not of one of
/// Octo's own grabs (a row with no client, or a grab id no entry of Octo's lists), or a file the
/// follow judged the user's when it came in, and the albums with a queue item that is not Octo's.
/// A file with no import row that a rescan linked before Octo's search counts for nobody.
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
        users_files: &[i32],
    ) -> Result<UsersOwn, LidarrError> {
        let mut own = UsersOwn::default();
        if artist_id <= 0 {
            return Ok(own);
        }
        let files = client.artist_track_files(artist_id).await?;
        for file in files.iter().filter(|file| users_files.contains(&file.id)) {
            own.file_albums.insert(file.album_id);
        }
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
                let octos = row.download_client().is_some()
                    && is_octos_at(pending, row.download_id.as_deref(), row.date);
                if !octos {
                    own.file_albums.insert(file.album_id);
                }
            }
        }
        for item in client.artist_queue(artist_id).await? {
            if !octos_item(client, pending, &item).await? {
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

/// Puts back a pick that never searched, or whose follow ended with every download of Octo's
/// accounted for and Octo's own files gone, then forgets its entry:
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
    restore_pin(client, entry, poll).await?;
    remove_what_octo_added(client, pending, entry).await
}

/// Puts back a pick whose search ended without a gated import, once every download of Octo's
/// is accounted for and no search of the album runs (the follow waits for both):
/// - Octo's own files still in Lidarr (each by its own import row: one of Octo's downloads, from
///   a download client) are deleted, Octo deleting each file from disk itself so a recycle bin
///   never keeps it, then its record; nothing else of the artist is touched, a folder import of
///   the user's (no client, whatever download id Lidarr guessed) and a file a disk scan adopted
///   (no row) included;
/// - the release choice goes back as [`put_back_unsearched`] puts it back, the rescan it queues
///   waited out;
/// - Octo's own files are looked for again, so one an import landed meanwhile goes too;
/// - then what Octo's add created goes, as [`put_back_unsearched`] removes it, and the entry is
///   forgotten last.
pub async fn put_back_searched(
    client: &LidarrClient,
    pending: Option<&LidarrPendingAlbums>,
    music: &LidarrArtistMusic,
    entry: &PendingAlbum,
    poll: Duration,
) -> Result<(), LidarrError> {
    delete_octos_files(client, music, entry).await?;
    restore_pin(client, entry, poll).await?;
    delete_octos_files(client, music, entry).await?;
    remove_what_octo_added(client, pending, entry).await
}

/// After a gated import landed, once Octo moved its files out: the album's release choice goes
/// back as it was (the rescan it queues waited out), then what Octo's add created goes as
/// [`put_back_unsearched`] removes it (Octo's landed files were moved out and their records
/// deleted, so nothing of them keeps it), and the entry is forgotten.
pub async fn restore_after_landing(
    client: &LidarrClient,
    pending: &LidarrPendingAlbums,
    entry: &PendingAlbum,
    poll: Duration,
) -> Result<(), LidarrError> {
    restore_pin(client, entry, poll).await?;
    remove_what_octo_added(client, Some(pending), entry).await
}

/// The album the entry names, as Lidarr has it now: None when Lidarr no longer has it, or when the
/// album under that id is another (another Lidarr set since, or the album deleted and its id
/// given again).
pub async fn entrys_album(
    client: &LidarrClient,
    entry: &PendingAlbum,
) -> Result<Option<LidarrAlbum>, LidarrError> {
    if entry.lidarr_album_id <= 0 {
        return Ok(None);
    }
    Ok(client
        .album(entry.lidarr_album_id)
        .await?
        .filter(|album| dotnet::eq_ignore_case(&album.foreign_album_id, &entry.foreign_album_id)))
}

/// The MusicBrainz artist id the entry's album is under, as the pick saved it.
fn entrys_foreign_artist(entry: &PendingAlbum) -> Option<&str> {
    Some(entry.foreign_artist_id.as_str())
        .filter(|id| !dotnet::is_blank(id))
        .or_else(|| entry.add.as_ref().map(|add| add.foreign_artist_id.as_str()))
        .filter(|id| !dotnet::is_blank(id))
}

/// The artist under Lidarr's `artist_id`, when it is still the entry's own artist by its
/// MusicBrainz id: nothing of another artist is ever changed.
async fn entrys_artist(
    client: &LidarrClient,
    entry: &PendingAlbum,
    artist_id: i32,
) -> Result<Option<LidarrArtist>, LidarrError> {
    let Some(foreign) = entrys_foreign_artist(entry) else {
        return Ok(None);
    };
    if artist_id <= 0 {
        return Ok(None);
    }
    Ok(client
        .artist(artist_id)
        .await?
        .filter(|artist| dotnet::eq_ignore_case(&artist.foreign_artist_id, foreign)))
}

/// Whether the album's release choice is still Octo's pin: exactly the pin, or the pin with only
/// its monitored release moved to one Lidarr's import of Octo's own download switched the album
/// to. A choice the user changed in any other way is the user's.
pub fn still_octos_pin(current: &LidarrReleaseChoice, pin: &LidarrReleaseChoice, imported: &[i32]) -> bool {
    if current == pin {
        return true;
    }
    if current.any_release_ok != pin.any_release_ok {
        return false;
    }
    let ids = |choice: &LidarrReleaseChoice| -> Vec<i32> {
        let mut ids: Vec<i32> = choice.releases.iter().map(|release| release.id).collect();
        ids.sort_unstable();
        ids
    };
    if ids(current) != ids(pin) {
        return false;
    }
    let monitored = |choice: &LidarrReleaseChoice| -> Vec<i32> {
        choice
            .releases
            .iter()
            .filter(|release| release.monitored)
            .map(|release| release.id)
            .collect()
    };
    let (now, pinned) = (monitored(current), monitored(pin));
    now.len() == 1 && pinned.len() == 1 && imported.contains(&now[0])
}

/// Gives the album back exactly the release choice saved before Octo pinned it, when it is still
/// Octo's pin ([`still_octos_pin`]; a choice the user changed stays as the user set it, checked
/// again in the read right before the change), and waits out the rescan the change queues, so
/// any file it links is seen by what comes after.
async fn restore_pin(client: &LidarrClient, entry: &PendingAlbum, poll: Duration) -> Result<(), LidarrError> {
    let album = entrys_album(client, entry).await?;
    let (Some(album), Some(saved), Some(pin)) = (&album, &entry.release_choice, &entry.pin) else {
        return Ok(());
    };
    let octos = |now: &LidarrAlbum| {
        dotnet::eq_ignore_case(&now.foreign_album_id, &entry.foreign_album_id)
            && still_octos_pin(&now.release_choice(), pin, &entry.imported_releases)
    };
    if !octos(album) {
        return Ok(());
    }
    if client
        .set_release_choice_if(album.id, octos, saved)
        .await?
        .is_some()
    {
        let path = client.artist(album.artist_id).await?.and_then(|a| a.path);
        wait_for_rescans(client, album.id, album.artist_id, path.as_deref(), poll).await?;
    }
    Ok(())
}

/// Deletes every track file of the entry's artist whose own import row is of one of this entry's
/// own downloads, inside that download's own span, and names a download client: from disk by Octo
/// first (when Octo can reach it), then its record, a record Lidarr no longer has counting as
/// deleted. Another open entry's imports on the same artist, and a later grab of the same id, are
/// never this entry's. Nothing is deleted under an artist that is no longer the entry's own.
async fn delete_octos_files(
    client: &LidarrClient,
    music: &LidarrArtistMusic,
    entry: &PendingAlbum,
) -> Result<(), LidarrError> {
    let artist_id = entry.lidarr_artist_id;
    if entrys_artist(client, entry, artist_id).await?.is_none() {
        return Ok(());
    }
    let files = client.artist_track_files(artist_id).await?;
    if files.is_empty() {
        return Ok(());
    }
    let rows = client
        .artist_history(artist_id, Some(LidarrHistoryEvent::TrackFileImported))
        .await?;
    for file in &files {
        let octos = rows.iter().any(|row| {
            row.file_id() == Some(file.id)
                && row.download_client().is_some()
                && row
                    .download_id
                    .as_deref()
                    .is_some_and(|id| entry.owns_grab_at(id, row.date))
        });
        if !octos {
            continue;
        }
        let local = file.path.as_deref().and_then(|path| music.local(path));
        match &local {
            Some(local) if local.is_file() => {
                if let Err(e) = std::fs::remove_file(local) {
                    warn!("Could not delete {}: {e}", local.display());
                    continue;
                }
                info!(
                    "Deleted Octo's own import {} while putting the pick back",
                    local.display()
                );
            }
            Some(_) => {}
            None => info!(
                "Octo cannot reach Lidarr's {}; Lidarr deletes it",
                file.path.as_deref().unwrap_or_default()
            ),
        }
        if client.delete_track_file_record(file.id).await? == LidarrDeletion::Conflict {
            warn!("Lidarr would not delete the record of track file {} now", file.id);
        }
    }
    Ok(())
}

/// Removes what Octo's add created, then forgets the entry. The album and the artist are read
/// first and must still be the entry's own by their MusicBrainz ids: an id Lidarr now gives
/// something else (another Lidarr set since) is never deleted.
async fn remove_what_octo_added(
    client: &LidarrClient,
    pending: Option<&LidarrPendingAlbums>,
    entry: &PendingAlbum,
) -> Result<(), LidarrError> {
    let album = entrys_album(client, entry).await?;
    let artist_id = album
        .as_ref()
        .map(|album| album.artist_id)
        .filter(|id| *id > 0)
        .unwrap_or(entry.lidarr_artist_id);
    let own = UsersOwn::read(client, pending, artist_id, &entry.users_files).await?;
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

/// Whether the artist Octo's add created is still only Octo's, and still the entry's own artist.
async fn artist_is_octos(
    client: &LidarrClient,
    pending: Option<&LidarrPendingAlbums>,
    entry: &PendingAlbum,
    own: &UsersOwn,
) -> Result<bool, LidarrError> {
    let Some(artist) = entrys_artist(client, entry, entry.artist_added).await? else {
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
) -> Vec<String> {
    let mut failed = Vec::new();
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
                        failed.push(entry.foreign_album_id.clone());
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
                        failed.push(entry.foreign_album_id.clone());
                        continue;
                    }
                }
            }
            // What the restart learned of the add is saved, so a put back tried again later
            // knows it too.
            let found = entry.clone();
            pending.update(&entry.foreign_album_id, |saved| {
                saved.album_added |= found.album_added;
                if saved.lidarr_album_id <= 0 {
                    saved.lidarr_album_id = found.lidarr_album_id;
                }
                if saved.lidarr_artist_id <= 0 {
                    saved.lidarr_artist_id = found.lidarr_artist_id;
                }
                if saved.artist_added <= 0 {
                    saved.artist_added = found.artist_added;
                }
            });
        }
        match put_back_unsearched(client, Some(pending), &entry, poll).await {
            Ok(()) => info!(
                "Put back Lidarr's album {} that a restart cut short before its search",
                entry.foreign_album_id
            ),
            Err(e) => {
                warn!(
                    "Could not put back Lidarr's album {} after a restart: {e}",
                    entry.foreign_album_id
                );
                failed.push(entry.foreign_album_id.clone());
            }
        }
    }
    failed
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
                    entry
                        .records_to_delete
                        .retain(|record| record.id != track_file_id);
                });
            }
        }
        Ok(LidarrDeletion::Conflict) => {
            info!("Lidarr keeps the record of track file {track_file_id} for now; asking again later");
            if let Some(pending) = pending {
                let path = local_path.to_string();
                pending.update(foreign_album_id, |entry| {
                    if !entry
                        .records_to_delete
                        .iter()
                        .any(|record| record.id == track_file_id)
                    {
                        entry.records_to_delete.push(MovedRecord {
                            id: track_file_id,
                            path: Some(path),
                        });
                    }
                });
            }
        }
        Err(e) => warn!("Could not delete the record of Lidarr track file {track_file_id}: {e}"),
    }
}

/// Asks Lidarr again to delete the records it kept, for the entry of this album. Each record is
/// read first: one Lidarr no longer has is done; one that now names another file (another Lidarr,
/// or the id given again) is left alone and let go; one whose file lies at its path again is left
/// for now, since deleting it would delete that file.
pub async fn retry_moved_records(
    client: &LidarrClient,
    pending: &LidarrPendingAlbums,
    music: &LidarrArtistMusic,
    foreign_album_id: &str,
) {
    let Some(entry) = pending.get(foreign_album_id) else {
        return;
    };
    let forget = |id: i32| {
        pending.update(foreign_album_id, |entry| {
            entry.records_to_delete.retain(|record| record.id != id);
        });
    };
    for record in entry.records_to_delete {
        let now = match client.track_file(record.id).await {
            Ok(Some(now)) => now,
            Ok(None) => {
                forget(record.id);
                continue;
            }
            Err(e) => {
                warn!("Could not read Lidarr's track file {}: {e}", record.id);
                continue;
            }
        };
        let local = now.path.as_deref().and_then(|path| music.local(path));
        let names_it = match (&record.path, &local) {
            (Some(saved), Some(local)) => same_path(saved, &local.to_string_lossy()),
            (None, Some(_)) => true,
            _ => false,
        };
        if !names_it {
            info!(
                "Lidarr's track file {} now names another file; it is left alone",
                record.id
            );
            forget(record.id);
            continue;
        }
        if local.as_ref().is_some_and(|local| local.exists()) {
            continue;
        }
        match client.delete_track_file_record(record.id).await {
            Ok(LidarrDeletion::Deleted | LidarrDeletion::NotFound) => forget(record.id),
            Ok(LidarrDeletion::Conflict) => {}
            Err(e) => warn!(
                "Could not delete the record of Lidarr track file {}: {e}",
                record.id
            ),
        }
    }
}

#[cfg(test)]
#[path = "lidarr_pick_tests.rs"]
mod tests;
