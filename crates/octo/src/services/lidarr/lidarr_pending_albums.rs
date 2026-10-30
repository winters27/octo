//! `lidarr-pending.json`: the albums Octo asked Lidarr for whose files have not all come in yet.
//! A heart waits for Lidarr's import only so long; when Lidarr imports the album later, or
//! after Octo restarted, its webhook names the album, and an album listed here is still taken
//! through the download pipeline. An album nobody listed here is the Lidarr owner's own and is
//! left alone.

use std::path::PathBuf;

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::dotnet;
use octo_core::lidarr::lidarr_client::LidarrReleaseChoice;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::services::state_file;

/// How long an album stays listed after Octo asked for it.
pub const KEPT_FOR: TimeDelta = TimeDelta::days(14);

/// One album Octo asked Lidarr for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PendingAlbum {
    /// The MusicBrainz release group id Lidarr knows the album by.
    pub foreign_album_id: String,
    /// Lidarr's own id for the album.
    pub lidarr_album_id: i32,
    pub artist: String,
    pub title: String,
    pub year: Option<i32>,
    /// The track files Lidarr had before Octo's search; they are the owner's, never taken.
    pub files_before: Vec<i32>,
    /// The tracks that already had a file before Octo's search. Their files stay the owner's
    /// even when Lidarr later replaces them with a new file id.
    pub tracks_before: Vec<i32>,
    /// Who hearted it.
    pub requested_by: Vec<String>,
    pub asked_at: DateTime<Utc>,
    /// Lidarr's own id for the album's artist; 0 while it is not known.
    pub lidarr_artist_id: i32,
    /// The add Octo was about to send, saved before the request went out; None when Lidarr had
    /// the album already.
    pub add: Option<PendingAdd>,
    /// Octo's add created the album.
    pub album_added: bool,
    /// The artist Octo's add created, by Lidarr's id; 0 when Lidarr had the artist already.
    pub artist_added: i32,
    /// Every album Lidarr listed for an artist Octo's add created, right after the add's refresh.
    pub albums_after_add: Vec<i32>,
    /// The album's release choice before Octo pinned it, put back exactly afterwards.
    pub release_choice: Option<LidarrReleaseChoice>,
    /// The release choice Octo's pin set; a choice that is no longer this one was changed by the
    /// user, and stays as the user set it.
    pub pin: Option<LidarrReleaseChoice>,
    /// The AlbumSearch Octo posted, saved the moment Lidarr answered; 0 before the search.
    pub search_command: i32,
    /// When the search was posted.
    pub searched_at: Option<DateTime<Utc>>,
    /// Every download id of Octo's own grabs for this album.
    pub grab_ids: Vec<String>,
    /// Track file records of Octo's moved imports that Lidarr would not delete yet, asked again
    /// later.
    pub records_to_delete: Vec<i32>,
    /// The tracks whose late imports Octo dealt with: their records may be gone since Octo moved
    /// the files, and they count as in.
    pub tracks_taken: Vec<i32>,
}

impl Default for PendingAlbum {
    fn default() -> Self {
        PendingAlbum {
            foreign_album_id: String::new(),
            lidarr_album_id: 0,
            artist: String::new(),
            title: String::new(),
            year: None,
            files_before: Vec::new(),
            tracks_before: Vec::new(),
            requested_by: Vec::new(),
            asked_at: Utc::now(),
            lidarr_artist_id: 0,
            add: None,
            album_added: false,
            artist_added: 0,
            albums_after_add: Vec::new(),
            release_choice: None,
            pin: None,
            search_command: 0,
            searched_at: None,
            grab_ids: Vec::new(),
            records_to_delete: Vec::new(),
            tracks_taken: Vec::new(),
        }
    }
}

impl PendingAlbum {
    /// Whether Octo posted the album's search.
    pub fn searched(&self) -> bool {
        self.search_command > 0 || self.searched_at.is_some()
    }
}

/// An add Octo was about to send to Lidarr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PendingAdd {
    /// The MusicBrainz artist id the album is added under.
    pub foreign_artist_id: String,
    /// When the add was sent: an artist Lidarr added at or after it is the add's.
    pub posted_at: DateTime<Utc>,
    /// The artists Lidarr had right before the add, by Lidarr's id.
    pub artists_before: Vec<i32>,
}

impl Default for PendingAdd {
    fn default() -> Self {
        PendingAdd {
            foreign_artist_id: String::new(),
            posted_at: Utc::now(),
            artists_before: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PendingFile {
    albums: Vec<PendingAlbum>,
}

/// The listed albums, by foreign album id ignoring case, saved on every change.
pub struct LidarrPendingAlbums {
    path: Option<PathBuf>,
    albums: Mutex<IndexMap<String, PendingAlbum>>,
}

impl LidarrPendingAlbums {
    /// The list over its file; None keeps it in memory only.
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut albums = IndexMap::new();
        if let Some(path) = &path {
            match state_file::read_text(path) {
                Ok(Some(text)) => match serde_json::from_str::<PendingFile>(&text) {
                    Ok(file) => {
                        for album in file.albums {
                            if !album.foreign_album_id.trim().is_empty() {
                                albums
                                    .insert(dotnet::ordinal_ignore_case_key(&album.foreign_album_id), album);
                            }
                        }
                    }
                    Err(e) => warn!(
                        "Could not read {}: {e}; starting with no pending Lidarr albums",
                        path.display()
                    ),
                },
                Ok(None) => {}
                Err(e) => warn!(
                    "Could not read {}: {e}; starting with no pending Lidarr albums",
                    path.display()
                ),
            }
        }
        LidarrPendingAlbums {
            path,
            albums: Mutex::new(albums),
        }
    }

    /// Lists an album, replacing what was listed for it.
    pub fn remember(&self, album: PendingAlbum) {
        let mut albums = self.albums.lock();
        albums.insert(dotnet::ordinal_ignore_case_key(&album.foreign_album_id), album);
        Self::drop_old(&mut albums, Utc::now());
        self.save(&albums);
    }

    /// The listed album, when it was asked for within [`KEPT_FOR`].
    pub fn get(&self, foreign_album_id: &str) -> Option<PendingAlbum> {
        let albums = self.albums.lock();
        albums
            .get(&dotnet::ordinal_ignore_case_key(foreign_album_id))
            .filter(|album| Utc::now() - album.asked_at <= KEPT_FOR)
            .cloned()
    }

    /// Changes the listed album in place and saves; false when it is not listed.
    pub fn update(&self, foreign_album_id: &str, change: impl FnOnce(&mut PendingAlbum)) -> bool {
        let mut albums = self.albums.lock();
        let Some(album) = albums.get_mut(&dotnet::ordinal_ignore_case_key(foreign_album_id)) else {
            return false;
        };
        change(album);
        self.save(&albums);
        true
    }

    /// Every listed album, however long ago it was asked for.
    pub fn entries(&self) -> Vec<PendingAlbum> {
        self.albums.lock().values().cloned().collect()
    }

    /// Whether a download id is one of Octo's own grabs, on any listed album.
    pub fn is_octos_grab(&self, download_id: &str) -> bool {
        self.albums.lock().values().any(|album| {
            album
                .grab_ids
                .iter()
                .any(|id| dotnet::eq_ignore_case(id, download_id))
        })
    }

    /// Whether any listed album other than `except` is under this Lidarr artist.
    pub fn uses_artist(&self, artist_id: i32, except: &str) -> bool {
        artist_id > 0
            && self.albums.lock().values().any(|album| {
                album.lidarr_artist_id == artist_id
                    && !dotnet::eq_ignore_case(&album.foreign_album_id, except)
            })
    }

    /// Takes an album off the list.
    pub fn forget(&self, foreign_album_id: &str) {
        let mut albums = self.albums.lock();
        if albums
            .shift_remove(&dotnet::ordinal_ignore_case_key(foreign_album_id))
            .is_some()
        {
            self.save(&albums);
        }
    }

    fn drop_old(albums: &mut IndexMap<String, PendingAlbum>, now: DateTime<Utc>) {
        albums.retain(|_, album| now - album.asked_at <= KEPT_FOR);
    }

    fn save(&self, albums: &IndexMap<String, PendingAlbum>) {
        let Some(path) = &self.path else {
            return;
        };
        let file = PendingFile {
            albums: albums.values().cloned().collect(),
        };
        let written = serde_json::to_string_pretty(&file)
            .map_err(std::io::Error::other)
            .and_then(|text| state_file::save_atomic(path, &text));
        if let Err(e) = written {
            warn!("Could not save {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str, asked_at: DateTime<Utc>) -> PendingAlbum {
        PendingAlbum {
            foreign_album_id: id.into(),
            lidarr_album_id: 7,
            artist: "Massive Attack".into(),
            title: "Mezzanine".into(),
            files_before: vec![3],
            requested_by: vec!["alice".into()],
            asked_at,
            ..Default::default()
        }
    }

    #[test]
    fn a_pending_album_survives_a_restart_and_is_found_ignoring_case() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        LidarrPendingAlbums::new(Some(path.clone())).remember(album("rg-ABC", Utc::now()));

        let reloaded = LidarrPendingAlbums::new(Some(path.clone()));
        let found = reloaded.get("rg-abc").expect("listed");
        assert_eq!(found.lidarr_album_id, 7);
        assert_eq!(found.files_before, [3]);
        assert_eq!(found.requested_by, ["alice"]);

        reloaded.forget("RG-abc");
        assert!(LidarrPendingAlbums::new(Some(path)).get("rg-abc").is_none());
    }

    #[test]
    fn an_album_asked_for_too_long_ago_is_no_longer_pending() {
        let pending = LidarrPendingAlbums::new(None);
        pending.remember(album("old", Utc::now() - KEPT_FOR - TimeDelta::hours(1)));
        pending.remember(album("new", Utc::now()));

        assert!(pending.get("old").is_none());
        assert!(pending.get("new").is_some());
    }

    /// Everything a pick saves before and around its add, its pin and its search survives a
    /// restart, and a file written before those were saved still reads.
    #[test]
    fn a_picks_add_pin_and_search_survive_a_restart() {
        use octo_core::lidarr::lidarr_client::LidarrReleaseMonitored;

        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        let choice = |monitored: bool| LidarrReleaseChoice {
            any_release_ok: monitored,
            releases: vec![LidarrReleaseMonitored { id: 70, monitored }],
        };
        let posted_at = Utc::now() - TimeDelta::minutes(1);
        let pending = LidarrPendingAlbums::new(Some(path.clone()));
        pending.remember(PendingAlbum {
            add: Some(PendingAdd {
                foreign_artist_id: "ma".into(),
                posted_at,
                artists_before: vec![2, 3],
            }),
            ..album("rg-abc", Utc::now())
        });
        assert!(pending.update("RG-ABC", |entry| {
            entry.album_added = true;
            entry.artist_added = 5;
            entry.lidarr_artist_id = 5;
            entry.release_choice = Some(choice(true));
            entry.pin = Some(choice(false));
            entry.search_command = 12;
            entry.grab_ids.push("DL-1".into());
        }));
        assert!(!pending.update("missing", |_| {}));

        let reloaded = LidarrPendingAlbums::new(Some(path.clone()));
        let found = reloaded.get("rg-abc").expect("listed");
        assert_eq!(
            found.add.as_ref().map(|add| add.artists_before.clone()),
            Some(vec![2, 3])
        );
        assert_eq!(found.add.as_ref().map(|add| add.posted_at), Some(posted_at));
        assert!(found.album_added && found.searched());
        assert_eq!((found.artist_added, found.search_command), (5, 12));
        assert_eq!(found.release_choice, Some(choice(true)));
        assert_eq!(found.pin, Some(choice(false)));
        assert!(reloaded.is_octos_grab("dl-1"));
        assert!(!reloaded.is_octos_grab("DL-2"));
        assert!(reloaded.uses_artist(5, "other"));
        assert!(!reloaded.uses_artist(5, "RG-ABC"));
        assert_eq!(reloaded.entries().len(), 1);

        std::fs::write(
            &path,
            r#"{"Albums":[{"ForeignAlbumId":"rg-old","LidarrAlbumId":7,"FilesBefore":[3]}]}"#,
        )
        .expect("written");
        let old = LidarrPendingAlbums::new(Some(path)).entries();
        assert_eq!(old.len(), 1);
        assert_eq!((old[0].add.clone(), old[0].searched()), (None, false));
    }

    #[test]
    fn an_unreadable_file_starts_empty() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        std::fs::write(&path, "{ not json").expect("written");

        assert!(LidarrPendingAlbums::new(Some(path)).get("anything").is_none());
    }
}
