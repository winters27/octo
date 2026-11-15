//! "Keep as playlist": an imported list kept as a Navidrome playlist.

use std::sync::Arc;

use async_trait::async_trait;
use octo_core::common::dotnet::eq_ignore_case;
use tracing::{info, warn};

use super::ImportList;
use super::import_models::import_sources;
use crate::services::library::NavidromePlaylistApi;
use crate::services::playlists::held_playlists::HeldSlot;
use crate::services::subsonic::NavidromeIdentityService;

/// How keeping a list's playlist went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSyncResult {
    pub ok: bool,
    pub playlist_id: Option<String>,
    pub note: Option<String>,
    pub deleted: bool,
    pub songs: i32,
}

impl PlaylistSyncResult {
    fn failed(playlist_id: Option<String>, note: Option<String>, songs: i32) -> Self {
        PlaylistSyncResult {
            ok: false,
            playlist_id,
            note,
            deleted: false,
            songs,
        }
    }
}

/// What the playlist keeper asks Navidrome. A trait, so a test can build one with no Navidrome
/// at all.
#[async_trait]
pub trait PlaylistNavidrome: Send + Sync {
    /// A new private playlist, as the admin: its id, or None.
    async fn create(&self, name: &str, comment: Option<&str>) -> Option<String>;
    /// True when the playlist is the user's afterwards.
    async fn give_to(&self, id: &str, user: &str) -> bool;
    /// Whether the playlist is still there: None when Navidrome could not say.
    async fn exists(&self, id: &str) -> Option<bool>;
    /// Its songs in order, as (position, song id), or None when they could not be read.
    async fn read_tracks(&self, id: &str) -> Option<Vec<(String, String)>>;
    async fn remove(&self, id: &str, positions: &[String]) -> bool;
    async fn add(&self, id: &str, songs: &[String]) -> bool;
    /// Deletes a playlist Octo just made and could not fill.
    async fn delete(&self, id: &str) -> bool;
    /// The admin Octo signs in to Navidrome as.
    fn admin_name(&self) -> Option<String>;
}

/// The real one: Navidrome's native API as the admin.
pub struct NavidromePlaylists {
    pub api: Arc<NavidromePlaylistApi>,
    pub identity: NavidromeIdentityService,
}

#[async_trait]
impl PlaylistNavidrome for NavidromePlaylists {
    async fn create(&self, name: &str, comment: Option<&str>) -> Option<String> {
        self.api.create_playlist(name, comment).await.unwrap_or_else(|e| {
            warn!("Could not create playlist '{name}': {e}");
            None
        })
    }

    async fn give_to(&self, id: &str, user: &str) -> bool {
        self.api.give_to(id, user).await
    }

    async fn exists(&self, id: &str) -> Option<bool> {
        self.api.playlist_exists(id).await.unwrap_or(None)
    }

    async fn read_tracks(&self, id: &str) -> Option<Vec<(String, String)>> {
        self.api.read_all_tracks(id).await.ok().flatten().map(|rows| {
            rows.into_iter()
                .map(|row| (row.position, row.media_file_id))
                .collect()
        })
    }

    async fn remove(&self, id: &str, positions: &[String]) -> bool {
        self.api.remove_positions(id, positions).await.unwrap_or(false)
    }

    async fn add(&self, id: &str, songs: &[String]) -> bool {
        self.api.add_tracks(id, songs).await.unwrap_or(false)
    }

    async fn delete(&self, id: &str) -> bool {
        self.api.delete_playlist(id).await.unwrap_or(false)
    }

    fn admin_name(&self) -> Option<String> {
        self.identity.get_scan_auth().map(|(user, _, _)| user)
    }
}

/// Keeps an imported list as a Navidrome playlist of the songs the library has, in the source's
/// order, so it is there in every Subsonic app. Made through Navidrome's own API as the admin and
/// then handed to the person, as Navidrome's playlist editor lets an admin do, so a song that
/// arrives later can be put in its place without anyone's password.
///
/// Only what differs is written: the songs up to the first difference stay, everything after it
/// is replaced. Songs nobody has yet are left out until they arrive. A playlist the person
/// deleted stays deleted.
///
/// A playlist this call made and could not fill is deleted again, so a failure never leaves an
/// empty one behind, and the next try never makes a second one. When even the delete fails, its
/// id is handed back so the next try fills that one.
pub struct ImportPlaylists {
    navidrome: Arc<dyn PlaylistNavidrome>,
    /// The held playlists, once they are built: each sync then holds the playlist's lock, keeps
    /// only songs the owner can read, and writes a held playlist through the held service.
    held: Option<HeldSlot>,
}

impl ImportPlaylists {
    const REMOVE_CHUNK: usize = 100;
    const ADD_CHUNK: usize = 500;

    pub fn new(navidrome: Arc<dyn PlaylistNavidrome>) -> Self {
        ImportPlaylists {
            navidrome,
            held: None,
        }
    }

    /// Syncs through the held playlists in `slot` once they are there.
    pub fn with_held(mut self, slot: HeldSlot) -> Self {
        self.held = Some(slot);
        self
    }

    pub async fn sync(&self, list: &ImportList) -> PlaylistSyncResult {
        let mut playlist_id = list.playlist_id.clone();
        let mut note = list.playlist_note.clone();
        let mut made = false;
        if let Some(id) = &playlist_id {
            match self.navidrome.exists(id).await {
                None => return PlaylistSyncResult::failed(playlist_id, note, 0),
                Some(false) => {
                    info!(
                        "Playlist for imported list '{}' ({}) was deleted in Navidrome; not making it again",
                        list.name, list.owner
                    );
                    return PlaylistSyncResult {
                        ok: true,
                        playlist_id: None,
                        note: None,
                        deleted: true,
                        songs: 0,
                    };
                }
                Some(true) => {}
            }
        } else {
            let Some(id) = self.navidrome.create(&list.name, Some(comment(list))).await else {
                return PlaylistSyncResult::failed(None, note, 0);
            };
            made = true;
            let admin = self.navidrome.admin_name();
            if !admin.as_deref().is_some_and(|a| eq_ignore_case(a, &list.owner))
                && !self.navidrome.give_to(&id, &list.owner).await
            {
                note = Some(format!(
                    "Navidrome lists it under {}: it could not be given to {}.",
                    admin.as_deref().unwrap_or("Octo's admin account"),
                    list.owner
                ));
                warn!(
                    "Playlist {id} for {} stays the admin's: Navidrome would not change its owner",
                    list.owner
                );
            } else {
                note = None;
            }
            info!("Made playlist '{}' ({id}) for {}", list.name, list.owner);
            playlist_id = Some(id);
        }
        let playlist_id = playlist_id.unwrap_or_default();

        let mut wanted: Vec<String> = Vec::new();
        for id in list.tracks.iter().filter_map(|t| t.library_id.as_ref()) {
            if !wanted.contains(id) {
                wanted.push(id.clone());
            }
        }
        // The rest of the sync holds the playlist's lock. Only songs the owner can read go in; when
        // that can't be told now, the sync waits for its next turn.
        let held = self.held.as_ref().and_then(|slot| slot.read().clone());
        let guard = match &held {
            Some(held) => match held.lock(&playlist_id).await {
                Ok(guard) => Some(guard),
                Err(_) => return self.give_up(playlist_id, note, made, 0).await,
            },
            None => None,
        };
        if let Some(held) = &held {
            wanted = match held.readable_library_ids(&list.owner, wanted).await {
                Ok(readable) => readable,
                Err(_) => return self.give_up(playlist_id, note, made, 0).await,
            };
        }
        if let (Some(held), Some(guard)) = (&held, &guard)
            && held.is_held(guard)
        {
            return match held.apply_library_list(guard, &wanted).await {
                Ok(()) => PlaylistSyncResult {
                    ok: true,
                    playlist_id: Some(playlist_id),
                    note,
                    deleted: false,
                    songs: wanted.len() as i32,
                },
                Err(_) => PlaylistSyncResult::failed(Some(playlist_id), note, 0),
            };
        }
        let Some(current) = self.navidrome.read_tracks(&playlist_id).await else {
            return self.give_up(playlist_id, note, made, 0).await;
        };

        let mut same = 0;
        while same < current.len() && same < wanted.len() && current[same].1 == wanted[same] {
            same += 1;
        }
        if same == current.len() && same == wanted.len() {
            return PlaylistSyncResult {
                ok: true,
                playlist_id: Some(playlist_id),
                note,
                deleted: false,
                songs: wanted.len() as i32,
            };
        }

        // From the end backwards: removing the last positions leaves the earlier ones where they are.
        let stale: Vec<String> = current[same..]
            .iter()
            .map(|(position, _)| position.clone())
            .collect();
        let mut end = stale.len();
        while end > 0 {
            let start = end.saturating_sub(Self::REMOVE_CHUNK);
            if !self.navidrome.remove(&playlist_id, &stale[start..end]).await {
                return self.give_up(playlist_id, note, made, same as i32).await;
            }
            end = start;
        }
        for chunk in wanted[same..].chunks(Self::ADD_CHUNK) {
            if !self.navidrome.add(&playlist_id, chunk).await {
                return self.give_up(playlist_id, note, made, same as i32).await;
            }
        }
        info!(
            "Playlist '{}' ({playlist_id}) now holds {} songs ({same} kept in place)",
            list.name,
            wanted.len()
        );
        PlaylistSyncResult {
            ok: true,
            playlist_id: Some(playlist_id),
            note,
            deleted: false,
            songs: wanted.len() as i32,
        }
    }

    /// A failure after the playlist was there. One this call made goes again, so nothing empty
    /// is left behind; when Navidrome will not delete it either, its id goes back so the next try
    /// fills it rather than making another.
    async fn give_up(
        &self,
        playlist_id: String,
        note: Option<String>,
        made: bool,
        songs: i32,
    ) -> PlaylistSyncResult {
        if made && self.navidrome.delete(&playlist_id).await {
            info!("Deleted playlist {playlist_id} again: it could not be filled");
            return PlaylistSyncResult::failed(None, None, songs);
        }
        PlaylistSyncResult::failed(Some(playlist_id), note, songs)
    }
}

fn comment(list: &ImportList) -> &'static str {
    match list.source.as_str() {
        import_sources::SPOTIFY_LIKED => {
            "Your liked songs on Spotify, from the ones in your library. Octo keeps it up to date."
        }
        import_sources::FILE => "Imported by Octo from a file.",
        _ => "Imported from Spotify by Octo, from the songs in your library. Octo keeps it up to date.",
    }
}

#[cfg(test)]
#[path = "import_playlists_tests.rs"]
mod tests;
