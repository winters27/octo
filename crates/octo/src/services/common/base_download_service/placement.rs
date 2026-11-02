//! Where a download goes: the configured layout, the name it is filed under, a path already
//! taken, a library action's replacement, the files beside it, and the clean-up behind it.

use std::path::{Path, PathBuf};

use octo_core::common::layout::{Field, Template, album_type_word, song_year};
use octo_core::common::path_helper::{PATH_BUDGET, POOL_BUDGET};
use octo_core::common::{
    AlbumHook, Layout, LayoutKind, LayoutValues, PathHelper, SettleHooks, Settled, SongIdentity,
    artist_credit, dotnet,
};
use octo_core::fingerprint::VerificationVerdict;
use octo_core::fingerprint::verification::RecordingProof;
use octo_core::lyrics::lyrics_text::LyricsText;
use octo_core::models::domain::Song;
use octo_core::settings::DownloadSource;
use octo_core::tagging::candidate_sources::is_various_artists;
use octo_media::cover::cover_files;
use octo_media::tags::KeptIdentityTags;
use octo_media::tags::tag_writer_extras;
use tracing::{debug, error, info, warn};

use super::tagging::AlbumSiblingFile;
use super::{BaseDownloadService, FileNotFoundException, RequestedIdentity, file_exists, get_extension};
use crate::services::common::acquisition_tracker::{acquisition_event_kinds, key_of};
use crate::services::common::album_identity::{
    AlbumIdentity, LayoutTags, Shown, Verdict, album_files, artist_key, audio_files_in, disc_folders,
    find_album_folder, read_layout_tags,
};
use crate::services::common::soulseek_hold_store::held_root;
use crate::services::i_download_service::HeldEnd;
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::library::replacement_handoff::ArrivedTags;
use crate::services::library::tidy::layout_pass::tag_values;
use crate::services::library::{ReplacementHandoff, ReplacementRejectedException};
use crate::services::lidarr::{FollowEnd, HELD_FOR_LIDARR, HeldPlacement};
use crate::services::local::LocalSongMapping;
use crate::services::lyrics::lyrics_sidecar_writer::LyricsJob;
use crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME;

/// What a download's path is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutChoice {
    pub folder_artist: String,
    pub file_artist: String,
    pub title: String,
    pub album: String,
    pub track: Option<i32>,
}

/// Where a download was placed; whether its album's folder (or its own, in a layout with none)
/// is new, which is the only place a cover.jpg may go without changing an album that was already
/// there; and the folder the layout gives its album alone, the only kind of folder a cover.jpg
/// may go in at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub path: String,
    pub created_folder: bool,
    pub album_folder: Option<String>,
    /// What the layout's names were filled from, for a file the layout placed.
    pub values: Option<LayoutValues>,
    /// Why the file could not be placed (a folder that could not be read, a move that failed):
    /// the download fails, rather than a file being kept where no scan sees it.
    pub failed: Option<String>,
    /// A file of the album folder the song joined, read while the folder was settled, whose
    /// album-level facts the song takes before its tags are written. `values` already carry
    /// them.
    pub pinned: Option<AlbumSiblingFile>,
}

impl Placement {
    pub fn new(path: impl Into<String>, created_folder: bool) -> Self {
        Self {
            path: path.into(),
            created_folder,
            album_folder: None,
            values: None,
            failed: None,
            pinned: None,
        }
    }

    /// The placement with the folder the layout gives the song's album alone, if any.
    pub fn in_album_folder(mut self, album_folder: Option<String>) -> Self {
        self.album_folder = album_folder;
        self
    }
}

/// Held by every route that settles a name and moves a file into the library, so two of them
/// never make two folders of one name at once. Taken after `download_lock`, a family pool claim
/// and the write gate's share, never before them, and no other lock is awaited under it.
pub(crate) static PLACEMENT: std::sync::LazyLock<PlacementLock> =
    std::sync::LazyLock::new(|| PlacementLock(tokio::sync::Mutex::new(())));

/// The placement lock ([`PLACEMENT`]).
pub(crate) struct PlacementLock(tokio::sync::Mutex<()>);

impl PlacementLock {
    /// The lock alone.
    pub(crate) async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.0.lock().await
    }

    /// The lock when it is free now.
    pub(crate) fn try_lock(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, tokio::sync::TryLockError> {
        self.0.try_lock()
    }

    /// The lock, taken by every route that places a file, with every move into the library a
    /// stop left half done put right first, from its record in `staging`
    /// ([`recover_replacements`]): a music folder can be found only after Octo started.
    pub(crate) async fn placing(&self, staging: &Path) -> tokio::sync::MutexGuard<'_, ()> {
        let placing = self.0.lock().await;
        recover_replacements(staging);
        placing
    }

    /// The name a copy that is to take the name `to` is written under until it is whole, beside
    /// `to`, with a record of it in `staging` while it is under way, so a stop leaves no
    /// unfinished copy [`recover_replacements`] does not remove. `moving` names the file the copy
    /// is made from when that file goes once the copy has its name ([`UnderWay::settle`]). The
    /// record goes when the answer's [`UnderWay`] is dropped. For the family pool's copies.
    ///
    /// `undo`: the move's caller takes an error as nothing moved (a pool file moved, kept aside or
    /// promoted), so a move a stop cut short after its rename is taken back. Any other move's
    /// caller follows the copy whatever the answer (a member's own copy, a song's lyrics), so its
    /// record says from the start that the source goes once the copy has its name.
    pub(crate) fn copy_under_way(
        &self,
        staging: &Path,
        to: &Path,
        moving: Option<&Path>,
        undo: bool,
    ) -> std::io::Result<(PathBuf, UnderWay)> {
        let mut name = to.as_os_str().to_owned();
        name.push(COPY_SUFFIX);
        let staged = PathBuf::from(name);
        let id = uuid::Uuid::new_v4().simple().to_string();
        let record = UnderWay::record(
            staging,
            &id,
            &ReplaceRecord {
                original: to.to_string_lossy().into_owned(),
                staged: staged.to_string_lossy().into_owned(),
                source: moving
                    .map(|from| from.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                size: moving.map_or(0, size_of),
                undo: moving.is_some() && undo,
                moved: moving.is_some() && !undo,
                ..ReplaceRecord::default()
            },
        )?;
        Ok((staged, record))
    }

    /// Makes the disk refuse to remove each of `paths` on this thread, until
    /// [`Self::allow_all`].
    #[cfg(test)]
    pub(crate) fn refuse_removals(&self, paths: &[&Path]) {
        REFUSED_REMOVALS.with(|refused| {
            refused
                .borrow_mut()
                .extend(paths.iter().map(|path| path.to_path_buf()))
        });
    }

    /// Makes the disk refuse to flush the folder of each of `paths` on this thread, until
    /// [`Self::allow_all`].
    #[cfg(test)]
    pub(crate) fn refuse_flushes(&self, paths: &[&Path]) {
        REFUSED_FLUSHES.with(|refused| {
            refused.borrow_mut().extend(
                paths
                    .iter()
                    .filter_map(|path| path.parent().map(Path::to_path_buf)),
            )
        });
    }

    /// Lets the disk remove every file, flush every folder and write every record again on this
    /// thread.
    #[cfg(test)]
    pub(crate) fn allow_all(&self) {
        REFUSED_REMOVALS.with(|refused| refused.borrow_mut().clear());
        REFUSED_FLUSHES.with(|refused| refused.borrow_mut().clear());
        REFUSED_MOVED_MARKS.with(|refused| refused.set(false));
    }

    /// Makes the disk refuse to write a record marked moved on this thread, until
    /// [`Self::allow_all`].
    #[cfg(test)]
    pub(crate) fn refuse_moved_marks(&self) {
        REFUSED_MOVED_MARKS.with(|refused| refused.set(true));
    }

    /// What the moves and recoveries on this thread did since the last call to make a rename
    /// last before a file goes ([`SETTLING`]).
    #[cfg(test)]
    pub(crate) fn take_settling(&self) -> Vec<String> {
        SETTLING.with(|log| log.borrow_mut().drain(..).collect())
    }
}

/// The staging folder of the music folder at `download_path`, where no scan looks: replacements
/// wait there, and a move into the library keeps its record there while it is under way.
fn staging_of(download_path: &str) -> PathBuf {
    Path::new(download_path).join(INCOMING_FOLDER_NAME)
}

/// One download's layout and root, read once, so the album sibling identify looks for and the
/// place the file goes come from the same settings, even when a save lands between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placing {
    pub layout: Layout,
    pub root: String,
    /// The download is a track of an album walk, which settles the album's facts itself: its
    /// placement takes none from the album's folder.
    pub walk: bool,
}

/// Where the layout files a song once the folders already there are looked at.
#[derive(Debug, Clone)]
pub(crate) struct Filed {
    pub(crate) settled: Settled,
    /// The album's folder, when the template gives one.
    pub(crate) album_dir: Option<String>,
    /// Whether the album's folder is already there.
    pub(crate) album_existed: bool,
    /// What the names were filled from, with the kind whose template filed the song.
    pub(crate) values: LayoutValues,
    /// Whether that kind is not the song's own, so its album folder joined on a positive sign.
    pub(crate) other_kind: bool,
    /// The album folder's loose audio of this album, when the song would make the album's first
    /// disc folder beside them.
    pub(crate) loose: Vec<String>,
}

impl BaseDownloadService {
    /// Fail a download whose file is not on disk. Everything after the transfer carries on past
    /// a missing file (placement keeps the path, tagging logs and moves on), which is how a song
    /// was recorded as downloaded with 0 bytes and never placed (#69). Failing marks the request
    /// Failed, writes no history, and lets the failure notice and any fallback source run.
    pub fn ensure_on_disk(path: Option<&str>) -> Result<(), FileNotFoundException> {
        let path = path.filter(|p| !p.is_empty());
        // A file that cannot be read is no more use than a missing one.
        let length = path
            .filter(|p| file_exists(p))
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len());
        if length > 0 {
            return Ok(());
        }
        Err(FileNotFoundException {
            message: match path {
                None => "The download returned no file".to_string(),
                Some(path) => format!("The download returned {path}, but there is no audio there"),
            },
            path: path.map(str::to_string),
        })
    }

    /// Ensures a directory exists, creating it and all parent directories if necessary.
    pub fn ensure_directory_exists(&self, path: &str) -> std::io::Result<()> {
        if Path::new(path).is_dir() {
            return Ok(());
        }
        match std::fs::create_dir_all(path) {
            Ok(()) => {
                debug!("Created directory: {path}");
                Ok(())
            }
            Err(e) => {
                error!("Failed to create directory: {path}: {e}");
                Err(e)
            }
        }
    }

    /// Move a finished download into the configured layout, named by [`Self::choose_layout`].
    ///
    /// Never overwrites a different file. A path that is already taken is replaced only when it
    /// provably holds this same song (see `is_same_song`); anything else keeps both. The move
    /// used to delete whatever sat at the target, which is how "Song (Live)" replaced "Song".
    /// Returns the landed path when the move fails, so the song is still registered.
    pub async fn place_in_library(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        current_path: &str,
    ) -> Placement {
        self.place_under(song, requested, current_path, None).await
    }

    /// [`Self::place_in_library`] under another root with the same layout: the family pool,
    /// where members' downloads are filed.
    pub async fn place_in_root(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        current_path: &str,
        root: &str,
    ) -> Placement {
        self.place_under(song, requested, current_path, Some(root)).await
    }

    /// Places a download as [`Self::place_in_library`] (or [`Self::place_in_root`], under `root`)
    /// does, unless a follow of Octo's Lidarr search is open for its artist and the place the
    /// layout gives it lies under one of Lidarr's root folders: a rescan there would link it into
    /// the artist while Octo's own import is coming, and that import would then replace it. Such a
    /// file waits in the one hold path, plays from there, and is placed when the follow ends. A
    /// file another source proved by its sound ends the artist's follows instead (Lidarr's own
    /// imports never do), and is placed as soon as Octo's items are out of Lidarr's queue. `users`
    /// are the people it was fetched or kept for.
    pub(super) async fn place_or_hold(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        current_path: &str,
        root: Option<&str>,
        source: Option<DownloadSource>,
        users: &[String],
    ) -> Placement {
        if let Some(held) = self
            .hold_for_lidarr(song, requested, current_path, root, source, users)
            .await
        {
            return held;
        }
        self.place_under(song, requested, current_path, root).await
    }

    /// The file moved into the one hold path for the open follows it waits for, or None when no
    /// follow holds it (or it could not be moved, and is placed as usual).
    async fn hold_for_lidarr(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        current_path: &str,
        root: Option<&str>,
        source: Option<DownloadSource>,
        users: &[String],
    ) -> Option<Placement> {
        let pending = self.services.lidarr_pending.as_ref()?;
        let download_path = self.download_path();
        let layout_root = root.map_or_else(|| download_path.clone(), str::to_string);
        if download_path.is_empty() || layout_root.is_empty() || !file_exists(current_path) {
            return None;
        }
        let target = self.layout_target_in(&layout_root, song, requested, get_extension(current_path));
        let choice = Self::choose_layout(song, requested, self.settings.current().soulseek.name_from_match);
        let artists = vec![
            song.artist.clone(),
            song.primary_artist.clone().unwrap_or_default(),
            choice.folder_artist,
            choice.file_artist,
            requested.artist.clone(),
        ];
        let follows = pending.covering(&artists, &choice.album, &target);
        if follows.is_empty() {
            return None;
        }
        let proof = song
            .verification
            .as_deref()
            .map(|verification| verification.proof.clone())
            .unwrap_or_default();
        let proven = proof == RecordingProof::Proven && source != Some(DownloadSource::Lidarr);
        if proven && !follows.iter().any(|follow| pending.is_running(follow)) {
            // Nothing follows Lidarr right now, so no import of Octo's can race it.
            for follow in &follows {
                pending.request_end(follow, FollowEnd::ProvenElsewhere);
            }
            return None;
        }
        let (provider, external_id) = (
            song.external_provider.as_deref().filter(|p| !dotnet::is_blank(p)),
            song.external_id.as_deref().filter(|i| !dotnet::is_blank(i)),
        );
        let song_key = match (provider, external_id) {
            (Some(provider), Some(id)) => format!("{provider}:{id}"),
            _ => String::new(),
        };
        let name = if song_key.is_empty() {
            uuid::Uuid::new_v4().simple().to_string()
        } else {
            song_key.clone()
        };
        let folder_name: String = format!("lidarr-follow-{name}")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let folder = held_root(Path::new(&download_path)).join(folder_name);
        let _share = match &self.services.write_gate {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        if let Err(e) = std::fs::create_dir_all(&folder) {
            warn!("Could not make the hold folder {}: {e}", folder.display());
            return None;
        }
        let mut held_path = folder.join(get_file_name(&target)).to_string_lossy().into_owned();
        if file_exists(&held_path) {
            held_path = PathHelper::resolve_unique_path(&held_path);
        }
        if let Err(e) = move_off_thread(&self.staging_folder(), current_path, &held_path).await {
            warn!("Could not hold {current_path} back from the library: {e}");
            return None;
        }
        Self::try_remove_empty_parents(get_directory_name(current_path).as_deref(), &download_path);
        pending.hold_placement(HeldPlacement {
            song: song_key,
            held_path: held_path.clone(),
            target,
            kept: matches!(proof, RecordingProof::AsHeard { .. } | RecordingProof::KeptByYou),
            follows: follows.clone(),
            users: users.to_vec(),
            proven_for: Vec::new(),
            // Filed again from these when the follow ends, against the folders there then.
            values: Some(Self::layout_values(song, requested, self.name_from_match())),
            root: layout_root,
        });
        if proven {
            info!("{held_path} is proven: Octo stops following Lidarr for its artist and places it");
            for follow in &follows {
                pending.request_end(follow, FollowEnd::ProvenElsewhere);
            }
        } else {
            info!("Holding {held_path} back from the library while Lidarr's download for its artist runs");
            if let (Some(provider), Some(id)) = (provider, external_id) {
                self.track(|tracker| tracker.wait_aside(provider, id, users, HELD_FOR_LIDARR));
            }
        }
        Some(Placement::new(held_path, false))
    }

    /// Places a file a Lidarr follow held back, now that no follow that may still import holds it
    /// (see [`crate::services::i_download_service::IDownloadService::place_held`]): filed by the
    /// layout against the folders there now (an album folder that appeared meanwhile is joined),
    /// beside a different file already there and never over it, with whatever was written beside
    /// it, the library's record moved along, and a scan asked for. A kept copy whose song's Lidarr
    /// file passed the gate of someone it was kept for never lands beside that file: in the shared
    /// library it gives way to it; in the family pool it lands, recorded and linked, only in the
    /// libraries of the members it is still for, and gives way once there are none. Never while a
    /// library job or a download is placing files: it waits for the download lock first, then the
    /// gate's share, as a download does.
    pub async fn place_held_file(&self, held: &HeldPlacement) -> HeldEnd {
        let Ok(_permit) = self.download_lock.acquire().await else {
            return HeldEnd::Later;
        };
        let share = match &self.services.write_gate {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        if !file_exists(&held.held_path) {
            return HeldEnd::Later;
        }
        let pool = self
            .services
            .family
            .as_ref()
            .filter(|family| family.is_pool_path(&held.target));
        let record = self
            .local_library
            .get_mappings()
            .await
            .into_iter()
            .find(|mapping| mapping.local_path == held.held_path);
        let kept_for = kept_for(held, record.as_ref());
        // Whom it was kept for that the song's proven file is for already: as the follow saved
        // it, and as the library records it (a restart may have come before the follow saved it).
        let mut proven_for: Vec<String> = Vec::new();
        if held.kept {
            let in_library = self.proven_in_library(held, &kept_for).await;
            for user in held.proven_for.iter().chain(in_library.iter()) {
                if one_of(&kept_for, user) && !one_of(&proven_for, user) {
                    proven_for.push(user.clone());
                }
            }
        }
        let still_for: Vec<String> = kept_for
            .iter()
            .filter(|user| !one_of(&proven_for, user))
            .cloned()
            .collect();
        if !proven_for.is_empty() && (pool.is_none() || still_for.is_empty()) {
            return if self.drop_held_locked(held, &proven_for).await {
                info!(
                    "The kept {} gives way to the song's proven Lidarr file instead of being placed: {}",
                    held.song, held.held_path
                );
                HeldEnd::GaveWay
            } else {
                HeldEnd::Later
            };
        }
        let target = {
            let _placing = PLACEMENT.placing(&self.staging_folder()).await;
            if !file_exists(&held.held_path) {
                return HeldEnd::Later;
            }
            let extension = get_extension(&held.held_path);
            let (target, made) = match &held.values {
                Some(values) => {
                    let root = if held.root.trim().is_empty() {
                        self.download_path()
                    } else {
                        held.root.clone()
                    };
                    let filed = match self.file_settled(&self.layout(), &root, values.clone(), extension) {
                        Ok(filed) => filed,
                        Err(e) => {
                            warn!(
                                "Could not read the library to place the held {}: {e}",
                                held.held_path
                            );
                            return HeldEnd::Later;
                        }
                    };
                    let made = match make_folders(&root, &filed.settled) {
                        Ok(made) => made,
                        Err(e) => {
                            warn!("Could not make the folders for the held {}: {e}", held.held_path);
                            return HeldEnd::Later;
                        }
                    };
                    let target = if filed.settled.taken.is_some() {
                        PathHelper::resolve_unique_path(&filed.settled.path)
                    } else {
                        filed.settled.path
                    };
                    (target, made)
                }
                // Held by an Octo that kept no values: the path it worked out then.
                None => {
                    let mut target = held.target.clone();
                    if file_exists(&target) {
                        target = PathHelper::resolve_unique_path(&target);
                    }
                    if let Some(dir) = get_directory_name(&target).filter(|d| !d.is_empty())
                        && let Err(e) = std::fs::create_dir_all(&dir)
                    {
                        warn!("Could not make {dir} for a held file: {e}");
                        return HeldEnd::Later;
                    }
                    (target, Vec::new())
                }
            };
            if let Err(e) = move_off_thread(&self.staging_folder(), &held.held_path, &target).await {
                remove_made(&made);
                warn!("Could not place the held {}: {e}", held.held_path);
                return HeldEnd::Later;
            }
            move_beside(&held.held_path, &target);
            // The hold folder goes with its last file.
            if let Some(folder) = Path::new(&held.held_path).parent() {
                let _ = std::fs::remove_dir(folder);
            }
            target
        };
        if let Err(e) = self
            .local_library
            .update_local_path(&held.held_path, &target)
            .await
        {
            warn!(
                "Could not move the library's record of {} to {target}: {e}",
                held.held_path
            );
        }
        if let Some(family) = pool {
            self.place_for_members(family.as_ref(), held, &target, record, &still_for)
                .await;
        }
        drop(share);
        self.local_library.trigger_library_scan(false).await;
        info!(
            "Placed the held {} where the layout files it: {target}",
            held.song
        );
        HeldEnd::Placed(target)
    }

    /// Those of `people` the library records another, proven file of the held copy's song for:
    /// that file passed their own gate.
    async fn proven_in_library(&self, held: &HeldPlacement, people: &[String]) -> Vec<String> {
        let Some((provider, external_id)) = held.song.split_once(':') else {
            return Vec::new();
        };
        let held_path = get_full_path(&held.held_path);
        let mappings = self.local_library.get_mappings().await;
        people
            .iter()
            .filter(|user| {
                mappings.iter().any(|mapping| {
                    mapping.external_provider == provider
                        && mapping.external_id == external_id
                        && !mapping.kept_unproven()
                        && !dotnet::eq_ignore_case(&get_full_path(&mapping.local_path), &held_path)
                        && mapping.heart.as_ref().is_some_and(|heart| heart.is_for(user))
                })
            })
            .cloned()
            .collect()
    }

    /// A held file placed in the family pool at `target`: recorded there and linked into the
    /// libraries of `members` alone. A kept copy's record names only them, so a member whose gate
    /// passed the song's Lidarr file is never counted as holding this copy too.
    async fn place_for_members(
        &self,
        family: &dyn crate::services::family::FamilyPlacement,
        held: &HeldPlacement,
        target: &str,
        record: Option<crate::services::local::LocalSongMapping>,
        members: &[String],
    ) {
        let (provider, external_id) = held.song.split_once(':').unwrap_or(("", held.song.as_str()));
        let song = Song {
            title: record.as_ref().map(|r| r.title.clone()).unwrap_or_default(),
            artist: record.as_ref().map(|r| r.artist.clone()).unwrap_or_default(),
            album: record.as_ref().map(|r| r.album.clone()).unwrap_or_default(),
            external_provider: Some(provider.to_string()),
            external_id: Some(external_id.to_string()),
            transcoded_from: record.as_ref().and_then(|r| r.transcoded_from.clone()),
            ..Default::default()
        };
        family
            .placed(target, &song, provider, external_id, members, true)
            .await;
        let Some(mut record) = record.filter(|record| {
            record.heart.as_ref().is_some_and(|heart| {
                heart.users.len() != members.len() || heart.users.iter().any(|user| !one_of(members, user))
            })
        }) else {
            return;
        };
        if let Some(heart) = record.heart.as_mut() {
            heart.users.retain(|user| one_of(members, user));
        }
        record.local_path = target.to_string();
        let rewritten = match self.local_library.forget_mapping_keyed(&record.key()).await {
            Ok(_) => {
                self.local_library
                    .restore_mappings(std::slice::from_ref(&record), target)
                    .await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = rewritten {
            warn!("Could not write the library's record of {target} for its members: {e}");
        }
    }

    /// Drops a kept copy a Lidarr follow held back, once Lidarr's file for its song passed the gate
    /// of `users` (see [`crate::services::i_download_service::IDownloadService::drop_held`]): its
    /// file, whatever was written beside it, and the library's record of it go. In the shared
    /// library that is when it was kept for any of them, the library holding one file of the song;
    /// in the family pool, only when it was kept for none but them. Never while a library job or a
    /// download is placing files: it waits for the download lock first, then the gate's share, as
    /// a download does.
    pub async fn drop_held_copy(&self, held: &HeldPlacement, users: &[String]) -> bool {
        let Ok(_permit) = self.download_lock.acquire().await else {
            return false;
        };
        let _share = match &self.services.write_gate {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        self.drop_held_locked(held, users).await
    }

    /// [`Self::drop_held_copy`], with the download lock and the write gate held by the caller.
    async fn drop_held_locked(&self, held: &HeldPlacement, users: &[String]) -> bool {
        let mapping = self
            .local_library
            .get_mappings()
            .await
            .into_iter()
            .find(|mapping| mapping.local_path == held.held_path);
        let kept_for = kept_for(held, mapping.as_ref());
        let in_pool = self
            .services
            .family
            .as_ref()
            .is_some_and(|family| family.is_pool_path(&held.target));
        let gives_way = !kept_for.is_empty()
            && if in_pool {
                kept_for.iter().all(|kept| one_of(users, kept))
            } else {
                kept_for.iter().any(|kept| one_of(users, kept))
            };
        if !gives_way {
            return false;
        }
        if mapping.is_some()
            && let Err(e) = self.local_library.forget_mapping(&held.held_path).await
        {
            warn!("Could not forget the library's record of {}: {e}", held.held_path);
            return false;
        }
        if file_exists(&held.held_path)
            && let Err(e) = std::fs::remove_file(&held.held_path)
        {
            warn!("Could not delete the kept copy {}: {e}", held.held_path);
        }
        if let Some(folder) = Path::new(&held.held_path).parent() {
            let stem = Path::new(&held.held_path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Ok(beside) = std::fs::read_dir(folder) {
                for file in beside.flatten() {
                    let name = file.file_name().to_string_lossy().into_owned();
                    if !stem.is_empty() && name.starts_with(&format!("{stem}.")) {
                        let _ = std::fs::remove_file(file.path());
                    }
                }
            }
            let _ = std::fs::remove_dir(folder);
        }
        info!(
            "Dropped the kept copy of {} for the proven copy: {}",
            held.song, held.held_path
        );
        true
    }

    /// A song fetched ahead, moved to `<folder>/<provider>_<id>.<ext>`, where no library scan
    /// looks. A copy of the song already there (and its facts) is replaced. Refused for a folder
    /// with other files in it that Octo did not make.
    pub(super) fn place_ahead(
        &self,
        current_path: &str,
        folder: &Path,
        provider: &str,
        external_id: &str,
    ) -> std::io::Result<String> {
        if !crate::services::prefetch::prepare_folder(folder)? {
            return Err(std::io::Error::other(format!(
                "{} has other files in it, so songs are not downloaded ahead there",
                folder.display()
            )));
        }
        let name = crate::services::prefetch::file_name(provider, external_id, get_extension(current_path));
        let target = folder.join(name);
        if target.exists() {
            std::fs::remove_file(&target)?;
            let _ = std::fs::remove_file(crate::services::prefetch::sidecar_path(&target));
        }
        let target = target.to_string_lossy().into_owned();
        move_file(&self.staging_folder(), current_path, &target)?;
        Self::try_remove_empty_parents(get_directory_name(current_path).as_deref(), &self.download_path());
        Ok(target)
    }

    async fn place_under(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        current_path: &str,
        root: Option<&str>,
    ) -> Placement {
        // Never while a library job (the tidy, a re-tag) is rewriting or moving folders.
        let share = match &self.services.write_gate {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        let placed = self.try_place(song, requested, current_path, root).await;
        drop(share);
        match placed {
            Ok(placement) => placement,
            Err(e) => {
                warn!("Could not place {current_path} in the configured layout: {e}");
                let mut failed = Placement::new(current_path, false);
                failed.failed = Some(e.to_string());
                failed
            }
        }
    }

    /// The staging folder, where no scan looks: replacements wait there, and a move into the
    /// library keeps its record there while it is under way.
    pub(super) fn staging_folder(&self) -> PathBuf {
        staging_of(&self.download_path())
    }

    /// Files the download under `root` and moves it there, under the placement lock.
    async fn try_place(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        current_path: &str,
        root: Option<&str>,
    ) -> std::io::Result<Placement> {
        let download_path = self.download_path();
        let layout_root = root.map_or_else(|| download_path.clone(), str::to_string);
        if layout_root.is_empty() || !file_exists(current_path) {
            return Ok(Placement::new(current_path, false));
        }

        let structure = self.settings.current().subsonic.folder_structure;
        let extension = get_extension(current_path);
        // Every route that makes names settles and moves under this one lock, so two of them
        // never make two folders of one name at once.
        let staging = self.staging_folder();
        let _placing = PLACEMENT.placing(&staging).await;
        let layout = self.layout_of(requested);
        let values = Self::layout_values(song, requested, self.name_from_match());
        let filed = self.file_settled(&layout, &layout_root, values, extension)?;
        #[cfg(test)]
        self.after_settle(&filed.settled.path).await;
        let (filed, pinned) = self.pin_album_facts(
            song,
            requested,
            (&layout, &layout_root, extension),
            current_path,
            filed,
        )?;
        let mut target = filed.settled.path.clone();

        if dotnet::eq_ignore_case(&get_full_path(&target), &get_full_path(current_path)) {
            return Ok(Placement::new(current_path, false));
        }

        // The folder whose newness decides cover.jpg: the album's folder when the layout gives
        // one (a disc folder below it is part of it), else the file's own.
        let target_dir = get_directory_name(&target).unwrap_or_default();
        let watched = filed.album_dir.clone().unwrap_or_else(|| target_dir.clone());
        let folder_had_audio = !watched.is_empty() && holds_audio(&watched)?;

        // A file already there by this name, or by another spelling of it that Windows and SMB
        // read as the same name (other capitals, an old name with a character Windows refuses).
        let mut replacing = None;
        if let Some(taken) = &filed.settled.taken {
            if dotnet::eq_ignore_case(&get_full_path(taken), &get_full_path(current_path))
                || same_file(taken, current_path)
            {
                return Ok(Placement::new(current_path, false));
            }
            if self.is_same_song(taken, song).await {
                // Replaced under its own name, so every record of that path stays right.
                info!("{taken} already holds this song; replacing it with the new download");
                replacing = Some(taken.clone());
                target = taken.clone();
            } else {
                let unique = PathHelper::resolve_unique_path(&target);
                info!("{taken} already holds a different file; keeping both as {unique}");
                target = unique;
            }
        }

        let made = make_folders(&layout_root, &filed.settled)?;
        let moved = match &replacing {
            Some(taken) => replace_same_song(&staging, taken, current_path),
            None => move_file(&staging, current_path, &target),
        };
        if let Err(e) = moved {
            // Nothing is left behind: the folders this placement made go again while empty.
            remove_made(&made);
            return Err(e);
        }
        info!("Placed download in {structure:?}: {current_path} -> {target}");

        // Clean up any now-empty folder the file came from (a Soulseek peer's own layout, or the
        // YouTube staging folder), never walking above the music root.
        Self::try_remove_empty_parents(get_directory_name(current_path).as_deref(), &download_path);
        let mut placement = Placement::new(target, !folder_had_audio).in_album_folder(filed.album_dir);
        placement.values = Some(filed.values);
        placement.pinned = pinned;
        Ok(placement)
    }

    /// `filed` with the album-level facts the song takes from one file of the album folder it
    /// joins ([`folder_fact_file`], the file identify's album sibling is), and that file read
    /// once. A tidy rename or a family pool promotion can have made that folder since the song
    /// was identified. When the facts change what the names are filled from, the song is settled
    /// again with them, so its file name and any folder below the album's show what its tags
    /// will say; facts that would file it outside that album folder are not taken, and it is
    /// filed by its own. A walk's track keeps the facts the walk settled on.
    fn pin_album_facts(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        (layout, root, extension): (&Layout, &str, &str),
        landed: &str,
        filed: Filed,
    ) -> std::io::Result<(Filed, Option<AlbumSiblingFile>)> {
        let walk = requested.placing.get().is_some_and(|placing| placing.walk);
        let Some(dir) = filed.album_dir.clone().filter(|_| filed.album_existed && !walk) else {
            return Ok((filed, None));
        };
        let Some(file) = folder_fact_file(&dir, &filed.values, landed) else {
            return Ok((filed, None));
        };
        let sibling = AlbumSiblingFile::read(&file);
        let mut pinned = song.clone();
        self.pin_to_folder(&mut pinned, &sibling);
        let mut values = filed.values.clone();
        take_album_facts(&mut values, &pinned);
        if values.disc_count != filed.values.disc_count {
            values.multi_disc = values.multi_disc || values.says_multi_disc();
        }
        if values == filed.values {
            return Ok((filed, Some(sibling)));
        }
        let budget = self.budget_for(root);
        let again = settle_by(
            layout.template(values.kind),
            root,
            values,
            extension,
            budget,
            filed.other_kind,
        )?;
        let again = choose_loose(layout, root, again, extension, budget)?;
        let same_folder = again
            .album_dir
            .as_deref()
            .is_some_and(|again| dotnet::eq_ignore_case(&get_full_path(again), &get_full_path(&dir)));
        if !same_folder {
            info!(
                "The album facts of {file} would file {} outside {dir}, so it keeps its own",
                song.title
            );
            return Ok((filed, None));
        }
        Ok((again, Some(sibling)))
    }

    /// Whether NameFromMatch is on now.
    pub(super) fn name_from_match(&self) -> bool {
        self.settings.current().soulseek.name_from_match
    }

    /// The path budget under `root`: the family pool's leaves room for a member's folder.
    fn budget_for(&self, root: &str) -> usize {
        if self.is_pool_root(root) {
            POOL_BUDGET
        } else {
            PATH_BUDGET
        }
    }

    /// [`file_values`] under `root`, with a multi-disc song whose album folder holds loose audio
    /// of its album and no disc folder landing loose beside them ([`choose_loose`]).
    pub(super) fn file_settled(
        &self,
        layout: &Layout,
        root: &str,
        values: LayoutValues,
        extension: &str,
    ) -> std::io::Result<Filed> {
        let budget = self.budget_for(root);
        let filed = file_values(layout, root, values, extension, budget)?;
        choose_loose(layout, root, filed, extension, budget)
    }

    /// One download's layout and root, read once from the settings.
    pub(super) fn placing_for(&self, root: Option<&str>) -> Placing {
        Placing {
            layout: self.layout(),
            root: root.map_or_else(|| self.download_path(), str::to_string),
            walk: false,
        }
    }

    /// The layout the settings name now. A custom template that does not parse is logged, and
    /// its stand-in used, so a download is always placed.
    pub(super) fn layout(&self) -> Layout {
        let layout = Layout::from_settings(&self.settings.current().subsonic);
        for problem in layout.problems() {
            warn!(
                "The custom file layout's {} is not used ({}); songs are filed as the dashboard's File layout card says",
                problem.setting, problem.message
            );
        }
        layout
    }

    /// What the layout's names are filled from for this song: the names [`Self::choose_layout`]
    /// picks, and the release's facts (year, disc, type, album artist) only when the release is
    /// the album the folder is named after.
    pub fn layout_values(song: &Song, requested: &RequestedIdentity, name_from_match: bool) -> LayoutValues {
        let choice = Self::choose_layout(song, requested, name_from_match);
        let no_album = dotnet::is_blank(&choice.album);
        let facts = !no_album && SongIdentity::key(&song.album) == SongIdentity::key(&choice.album);
        let named = |name: &str| {
            if dotnet::is_blank(name) {
                "Unknown Artist".to_string()
            } else {
                name.to_string()
            }
        };
        // A Various Artists album artist makes a compilation as the flag does; featured or guest
        // artists never do.
        let compilation = song.is_compilation
            || octo_core::tagging::candidate_sources::is_various_artists(song.album_artist.as_deref());
        let (kind, album_type) = if facts {
            (
                LayoutKind::of(song.release_type.as_deref(), compilation, false),
                album_type_word(song.release_type.as_deref(), compilation),
            )
        } else if no_album {
            (LayoutKind::Single, Some("Single".to_string()))
        } else {
            (LayoutKind::Album, None)
        };
        let year = song_year(song).filter(|_| facts);
        // `{Album}` falls back to the title in a folder name; the template does that itself.
        let mut values = LayoutValues {
            artist: named(&choice.folder_artist),
            track_artist: named(&choice.file_artist),
            album_artist: song.album_artist.clone().filter(|_| facts),
            title: PathHelper::file_title(&choice.title, &choice.file_artist),
            album: choice.album,
            track: choice.track,
            disc: song.disc_number.filter(|_| facts),
            disc_count: song.total_discs.filter(|_| facts),
            multi_disc: false,
            year,
            album_type,
            kind,
            release_group: song
                .music_brainz_release_group_id
                .clone()
                .filter(|id| facts && !id.trim().is_empty()),
            tags: None,
        };
        values.multi_disc = values.says_multi_disc();
        values
    }

    /// The download's layout: the one read for it before identify when there is one, else the
    /// settings' now.
    pub(super) fn layout_of(&self, requested: &RequestedIdentity) -> Layout {
        requested
            .placing
            .get()
            .map_or_else(|| self.layout(), |placing| placing.layout.clone())
    }

    /// Where the configured layout files this song under `root` once the folders and files
    /// already there are looked at ([`file_values`]), before the loose-album choice
    /// [`Self::file_settled`] makes.
    fn layout_filed_in(
        &self,
        root: &str,
        song: &Song,
        requested: &RequestedIdentity,
        extension: &str,
    ) -> std::io::Result<Filed> {
        let values = Self::layout_values(song, requested, self.name_from_match());
        file_values(
            &self.layout_of(requested),
            root,
            values,
            extension,
            self.budget_for(root),
        )
    }

    /// Whether `root` is the family pool's, where each member's link adds their folder's name.
    fn is_pool_root(&self, root: &str) -> bool {
        self.services
            .family
            .as_ref()
            .and_then(|family| family.pool_root())
            .is_some_and(|pool| get_full_path(&pool) == get_full_path(root))
    }

    /// Where the configured layout files this song under `root`, before any clash is looked at.
    /// A folder that cannot be read gives the rendered path, which a later settle reads again.
    fn layout_target_in(
        &self,
        root: &str,
        song: &Song,
        requested: &RequestedIdentity,
        extension: &str,
    ) -> String {
        match self.layout_filed_in(root, song, requested, extension) {
            Ok(filed) => filed.settled.path,
            Err(e) => {
                warn!("Could not read the library to place {}: {e}", song.title);
                let values = Self::layout_values(song, requested, self.name_from_match());
                PathHelper::join_names(
                    root,
                    &self.layout_of(requested).render_names(&values, extension),
                    extension,
                )
            }
        }
    }

    /// The album folder the configured layout files the song in, when it gives the album a folder
    /// of its own and that folder is already there (found by name or by the album it holds), for
    /// the album a single may join, under the download's own root (the family pool's for a
    /// member), with the values the song is filed from. None when the layout gives none, or the
    /// library cannot be read.
    pub(super) fn album_folder_target(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        extension: &str,
    ) -> Option<(String, LayoutValues)> {
        let root = requested
            .placing
            .get()
            .map_or_else(|| self.download_path(), |placing| placing.root.clone());
        match self.layout_filed_in(&root, song, requested, extension) {
            Ok(filed) => filed
                .album_dir
                .filter(|_| filed.album_existed)
                .map(|dir| (dir, filed.values)),
            Err(e) => {
                debug!(
                    "could not read the library for {}'s album folder: {e}",
                    song.title
                );
                None
            }
        }
    }

    /// The album folder a replacement at `target` sits in, when the layout gives one and its
    /// album folder is the file's own folder (a disc folder layout keeps the cover it has).
    fn album_folder_of(&self, song: &Song, requested: &RequestedIdentity, target: &str) -> Option<String> {
        let values = Self::layout_values(song, requested, self.name_from_match());
        let layout = self.layout_of(requested);
        let template = layout.template(values.kind);
        let last_folder = template.folder_count().checked_sub(1);
        (template.album_folder_segment().is_some() && template.album_folder_segment() == last_folder)
            .then(|| get_directory_name(target))
            .flatten()
    }

    /// Move a replacement into the incoming dot folder, which Navidrome never scans.
    pub fn stage_replacement(&self, landed_path: &str) -> anyhow::Result<Placement> {
        let download_path = self.download_path();
        let incoming = Path::new(&download_path).join(INCOMING_FOLDER_NAME);
        std::fs::create_dir_all(&incoming)?;
        let staged = incoming
            .join(format!(
                "replacement-{}{}",
                uuid::Uuid::new_v4().simple(),
                get_extension(landed_path)
            ))
            .to_string_lossy()
            .into_owned();
        move_file(&self.staging_folder(), landed_path, &staged)?;
        Self::try_remove_empty_parents(get_directory_name(landed_path).as_deref(), &download_path);
        Ok(Placement::new(staged, false))
    }

    /// Give the staged replacement the original's identity, let the library action judge it,
    /// then move it to the original's folder and name in one rename. Nothing may scan it
    /// before its tags are final: Navidrome would file it as a new song for good. A refused one
    /// is deleted here, where no scan ever saw it.
    pub async fn reveal_replacement(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        staged: &str,
        handoff: &ReplacementHandoff,
    ) -> anyhow::Result<Placement> {
        let revealed = async {
            if let Ok(file) = octo_media::tags::TagFile::open(Path::new(staged)) {
                handoff.set_arrived(ArrivedTags {
                    title: file.title(),
                    album: file.album(),
                });
            }
            KeptIdentityTags::apply(Path::new(staged), &handoff.identity)?;
            if let Some(problem) = (handoff.before_reveal)(staged.to_string()).await {
                return Err(ReplacementRejectedException::new(problem).into());
            }

            // Never while a library job moves folders, from here until the replacement is in its
            // place. Taken only now: the checks above can take long, and a job that moved the
            // folder of an original gone to quarantine holds that folder by its own checks.
            let share = match &self.services.write_gate {
                Some(gate) => Some(gate.write().await),
                None => None,
            };
            let extension = get_extension(staged);
            let staging = self.staging_folder();
            let target = {
                let _placing = PLACEMENT.placing(&staging).await;
                let (target, made) = match handoff.target_for(extension) {
                    // The original's own name, free by every spelling: Navidrome updates the song.
                    Some(target) if PathHelper::resolve_unique_path(&target) == target => {
                        if let Some(dir) = get_directory_name(&target).filter(|d| !d.is_empty()) {
                            std::fs::create_dir_all(dir)?;
                        }
                        (target, Vec::new())
                    }
                    // Else filed by the layout, joining the album's folder by any name, beside
                    // whatever is already there and never over it.
                    _ => {
                        let root = self.download_path();
                        let values = Self::layout_values(song, requested, self.name_from_match());
                        let filed =
                            self.file_settled(&self.layout_of(requested), &root, values, extension)?;
                        let made = make_folders(&root, &filed.settled)?;
                        let target = if filed.settled.taken.is_some() {
                            PathHelper::resolve_unique_path(&filed.settled.path)
                        } else {
                            filed.settled.path
                        };
                        (target, made)
                    }
                };
                if let Err(e) = move_file(&staging, staged, &target) {
                    remove_made(&made);
                    return Err(e.into());
                }
                handoff.set_revealed_path(&target);
                if let Some(on_revealed) = &handoff.on_revealed {
                    on_revealed(&target);
                }
                target
            };
            drop(share);
            info!("Placed the replacement where the original was: {target}");
            let album_dir = self.album_folder_of(song, requested, &target);
            anyhow::Ok(Placement::new(target, false).in_album_folder(album_dir))
        }
        .await;
        if handoff.revealed_path().is_none() && file_exists(staged) {
            // Swept after a day when this fails too.
            let _ = std::fs::remove_file(staged);
        }
        revealed
    }

    /// Whether an occupied path holds this same song: both carry the same MusicBrainz recording,
    /// or, for a file whose sound was not proven to be a recording (an import the shipped checks
    /// kept), it is a file Octo itself downloaded for the same artist and title. A file kept as
    /// heard or kept anyway is never the same song by name, on either side, and a file the sound
    /// proved replaces only that same recording: a second version is placed beside the first.
    /// A file of another disc of the album is another song (disc 2's "Intro" landing loose beside
    /// disc 1's): its disc is its tag's, else the one Octo saved for it, and a missing disc on
    /// either side is disc 1. Anything short of that is somebody's other file and is never
    /// replaced.
    async fn is_same_song(&self, target: &str, song: &Song) -> bool {
        let proof = song
            .verification
            .as_deref()
            .map(|verification| verification.proof.clone())
            .unwrap_or_default();
        if matches!(proof, RecordingProof::AsHeard { .. } | RecordingProof::KeptByYou) {
            return false;
        }
        let full = get_full_path(target);
        let existing = self
            .local_library
            .get_mappings()
            .await
            .into_iter()
            .filter(|mapping| {
                !mapping.local_path.is_empty()
                    && dotnet::eq_ignore_case(&get_full_path(&mapping.local_path), &full)
            })
            .collect::<Vec<_>>();
        if existing.iter().any(|mapping| mapping.kept_unproven()) {
            return false;
        }
        // A missing disc is disc 1 on both sides, as the file names take it. The taken file's
        // disc is its tag's, else the one Octo saved for it.
        let wanted = song.disc_number.filter(|disc| *disc > 0).unwrap_or(1);
        let taken = read_layout_tags(Path::new(target))
            .and_then(|tags| tags.disc)
            .or_else(|| {
                existing
                    .iter()
                    .find_map(|mapping| mapping.layout.as_ref().and_then(|layout| layout.disc))
                    .filter(|disc| *disc > 0)
            })
            .unwrap_or(1);
        if wanted != taken {
            return false;
        }
        let (tagged, _) = tag_writer_extras::read_identity(Path::new(target));
        let existing_id = tagged.filter(|id| !id.is_empty()).or_else(|| {
            existing
                .iter()
                .find_map(|mapping| mapping.music_brainz_recording_id.clone())
                .filter(|id| !id.is_empty())
        });
        let wanted = song
            .music_brainz_recording_id
            .as_deref()
            .filter(|id| !id.is_empty());
        if let (Some(existing), Some(wanted)) = (&existing_id, wanted) {
            return dotnet::eq_ignore_case(existing, wanted);
        }
        if proof == RecordingProof::Proven {
            return false;
        }

        let key = SongIdentity::match_key(&song.artist, &song.title);
        existing
            .iter()
            .any(|mapping| SongIdentity::match_key(&mapping.artist, &mapping.title) == key)
    }

    /// Which name a download is filed under. With NameFromMatch on and a confirmed match, the
    /// recording's, already applied to the Song. Otherwise the request's, except that a list of
    /// credits never names a folder and a request with no album takes the album it was tagged
    /// with, together with that album's track number.
    pub fn choose_layout(song: &Song, requested: &RequestedIdentity, name_from_match: bool) -> LayoutChoice {
        let matched = song
            .verification
            .as_deref()
            .filter(|v| v.verdict == VerificationVerdict::Confirmed)
            .and_then(|v| v.r#match.as_ref());

        if name_from_match && let Some(matched) = matched {
            return LayoutChoice {
                folder_artist: matched
                    .primary_artist()
                    .map(str::to_string)
                    .or_else(|| song.primary_artist.clone())
                    .unwrap_or_else(|| song.artist.clone()),
                file_artist: song.artist.clone(),
                title: song.title.clone(),
                album: song.album.clone(),
                track: song.track,
            };
        }

        let from_request = !requested.album.is_empty();
        LayoutChoice {
            folder_artist: Self::primary_credit(
                &requested.artist,
                &[
                    matched.and_then(|m| m.primary_artist()),
                    song.primary_artist.as_deref(),
                ],
            ),
            file_artist: requested.artist.clone(),
            title: requested.title.clone(),
            album: if from_request {
                requested.album.clone()
            } else {
                song.album.clone()
            },
            track: if from_request { requested.track } else { song.track },
        }
    }

    /// The artist a folder is named after: the rule the library tidy follows too.
    pub fn primary_credit(requested: &str, structured: &[Option<&str>]) -> String {
        artist_credit::primary_credit(requested, structured)
    }

    /// Walk up from `start_dir` removing empty folders, never past `stop_at`.
    pub fn try_remove_empty_parents(start_dir: Option<&str>, stop_at: &str) {
        let Some(start_dir) = start_dir.filter(|d| !d.is_empty()) else {
            return;
        };
        if stop_at.is_empty() {
            return;
        }
        let stop = get_full_path(stop_at).trim_end_matches('/').to_string();
        let mut current = get_full_path(start_dir).trim_end_matches('/').to_string();
        // Walk up while we're inside the download path and the directory is empty.
        while !current.is_empty()
            && dotnet::utf16_len(&current) > dotnet::utf16_len(&stop)
            && dotnet::starts_with_ignore_case(&current, &stop)
            && Path::new(&current).is_dir()
        {
            let empty = match std::fs::read_dir(&current) {
                Ok(mut entries) => entries.next().is_none(),
                Err(_) => break,
            };
            if !empty || std::fs::remove_dir(&current).is_err() {
                break;
            }
            current = get_directory_name(&current).unwrap_or_default();
        }
    }

    /// The cached file for a provider and external id (`{provider}_{externalId}.*` anywhere under
    /// the cache folder), or `None` when there is none.
    pub fn get_cached_file_path(&self, provider: &str, external_id: &str) -> Option<String> {
        let prefix = format!("{provider}_{external_id}.");
        match find_file(Path::new(&self.cache_path), &prefix) {
            Ok(found) => found,
            Err(e) => {
                warn!("Failed to search for cached file: {provider}_{external_id}: {e}");
                None
            }
        }
    }

    /// Files beside the audio file. Best-effort; never fails a download.
    ///
    /// cover.jpg only in a folder the layout gives the album alone (Organized, or a custom layout
    /// with an album folder) and only in a folder this download created. Navidrome ranks cover.*
    /// above embedded art, so in Flat every download shares one folder, in ByArtist one folder
    /// holds all of an artist's albums, and in an album folder that was already there one new
    /// track would change the whole album's cover. The one exception is a cover.jpg Octo wrote
    /// itself, in an album folder: a later track of the same album that found a larger cover
    /// replaces it, so a soft first track does not set the album's cover for good.
    pub fn write_sidecars(&self, song: &Song, placement: &Placement, cover: Option<&[u8]>) {
        let current = self.settings.current();
        if current.metadata.write_cover_file
            && let Some(cover) = cover.filter(|c| !c.is_empty())
            && let Some(dir) = placement.album_folder.as_deref().filter(|d| !d.is_empty())
            && cover_files::should_write(Path::new(dir), cover, placement.created_folder)
        {
            match cover_files::write(Path::new(&dir), cover) {
                Ok(()) => {
                    info!("Wrote cover.jpg beside {}", placement.path);
                    self.log_song_step(
                        song,
                        acquisition_event_kinds::COVER,
                        "Saved cover.jpg in the album folder",
                        None,
                    );
                }
                Err(e) => warn!("Could not write cover.jpg beside {}: {e}", placement.path),
            }
        }

        // Lyrics are fetched in the background: this runs under the download lock, and a lyrics
        // service that is slow or shedding load must not hold up the next download (#52).
        if current.metadata.fetch_lyrics
            && let Some(lyrics) = &self.services.lyrics
        {
            // The log's row hears how the look went, after the song is in.
            let row = match (
                song.external_provider.as_deref().filter(|p| !p.is_empty()),
                song.external_id.as_deref().filter(|i| !i.is_empty()),
            ) {
                (Some(provider), Some(id)) => Some(key_of(provider, id)),
                _ => None,
            };
            let job = LyricsJob::new(
                &placement.path,
                song.primary_artist.clone().unwrap_or_else(|| song.artist.clone()),
                LyricsText::query_title(&song.title, &song.artist),
                Some(song.album.clone()),
                song.duration,
            )
            .with_acquisition(row);
            if lyrics.try_enqueue(job) {
                self.log_song_step(
                    song,
                    acquisition_event_kinds::LYRICS,
                    "Looking for lyrics",
                    Some("In the background, so the next download need not wait"),
                );
            }
        }
    }
}

/// Moves a file to a path nothing lies at, never over a file that lands there meanwhile (an
/// import of Lidarr's runs outside Octo's locks), keeping no record: linked into place and the old
/// name removed, or, on another disk or a file system with no hard links, copied into place by
/// [`copy_new`] and the old file removed. Whatever fails leaves the file where it was and nothing
/// new at `to`. For files kept outside the library; a move into it is [`move_file`].
fn move_unrecorded(from: &str, to: &str) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => std::fs::remove_file(from).inspect_err(|_| {
            let _ = std::fs::remove_file(to);
        }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(already_there(to)),
        Err(_) => {
            copy_new(Path::new(from), Path::new(to))?;
            std::fs::remove_file(from).inspect_err(|_| {
                let _ = std::fs::remove_file(to);
            })
        }
    }
}

/// [`move_file`] on a blocking thread: a move across disks copies the whole file.
async fn move_off_thread(staging: &Path, from: &str, to: &str) -> std::io::Result<()> {
    let (staging, from, to) = (staging.to_path_buf(), from.to_string(), to.to_string());
    tokio::task::spawn_blocking(move || move_file(&staging, &from, &to))
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

fn already_there(to: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("The file '{to}' already exists."),
    )
}

/// Copies a file to a path nothing lies at, never over a file that lands there meanwhile: to a
/// temporary name in the target's folder first, then linked into place, which fails when the path
/// is taken; on a file system with no hard links, the path is claimed by creating it new before
/// the copy is renamed onto it. A failed copy leaves no temporary file and nothing at `to`.
pub(crate) fn copy_new(from: &Path, to: &Path) -> std::io::Result<()> {
    let folder = to.parent().unwrap_or_else(|| Path::new("."));
    let name = to
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = folder.join(format!(".{name}.{}.part", uuid::Uuid::new_v4().simple()));
    if let Err(e) = std::fs::copy(from, &temporary) {
        let _ = std::fs::remove_file(&temporary);
        return Err(e);
    }
    let placed = match std::fs::hard_link(&temporary, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)
            .and_then(|claimed| {
                drop(claimed);
                std::fs::rename(&temporary, to).inspect_err(|_| {
                    let _ = std::fs::remove_file(to);
                })
            }),
    };
    let _ = std::fs::remove_file(&temporary);
    placed
}

impl BaseDownloadService {
    /// Moves a file to a path nothing lies at, never over a file that lands there meanwhile (see
    /// [`move_unrecorded`]), for the services that hold files outside the library.
    pub fn move_new(from: &Path, to: &Path) -> std::io::Result<()> {
        move_unrecorded(&from.to_string_lossy(), &to.to_string_lossy())
    }
}

/// Whom a held file was kept for: as the library records it, else as it was held.
fn kept_for(held: &HeldPlacement, record: Option<&crate::services::local::LocalSongMapping>) -> Vec<String> {
    record
        .and_then(|record| record.heart.as_ref())
        .map(|heart| heart.users.clone())
        .filter(|kept_for| !kept_for.is_empty())
        .unwrap_or_else(|| held.users.clone())
}

/// Whether `user` is one of `people`, ignoring case and spaces around.
fn one_of(people: &[String], user: &str) -> bool {
    people
        .iter()
        .any(|known| dotnet::eq_ignore_case(known.trim(), user.trim()))
}

/// `dir` with `name` under it, joined by one `/`.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Files `values` under `root` by `layout`: by the song's own kind's template, and when that
/// finds no folder of its album already there, by each other kind's template that differs from
/// it, where an album folder counts only on a positive sign ([`AlbumIdentity::positive_match`]),
/// so a single and its album's track share one folder and an album never joins another's folder
/// of the same title. Found there, the song is filed by that kind's whole template, and its
/// values say which kind it was. Nothing is made or moved.
pub(crate) fn file_values(
    layout: &Layout,
    root: &str,
    values: LayoutValues,
    extension: &str,
    budget: usize,
) -> std::io::Result<Filed> {
    let own = layout.template(values.kind);
    let filed = settle_by(own, root, values.clone(), extension, budget, false)?;
    if filed.album_existed {
        return Ok(filed);
    }
    let mut tried: Vec<&Template> = vec![own];
    for kind in [LayoutKind::Album, LayoutKind::Single, LayoutKind::Compilation] {
        let template = layout.template(kind);
        if tried.contains(&template) || !template.has_album_folder() {
            continue;
        }
        tried.push(template);
        let mut theirs = values.clone();
        theirs.kind = kind;
        let found = settle_by(template, root, theirs, extension, budget, true)?;
        if found.album_existed
            && found
                .album_dir
                .as_deref()
                .is_some_and(|dir| joins_positively(dir, &found.values))
        {
            return Ok(found);
        }
    }
    Ok(filed)
}

/// The one choice [`file_values`] leaves: a multi-disc song whose album folder holds loose audio
/// of its album and no disc folder. Placement never moves another song, so the song is settled
/// again without its disc and lands loose beside them, whoever's they are: the album keeps one
/// folder and one shape, and the File layout task offers its disc folders.
pub(crate) fn choose_loose(
    layout: &Layout,
    root: &str,
    filed: Filed,
    extension: &str,
    budget: usize,
) -> std::io::Result<Filed> {
    if filed.loose.is_empty() {
        return Ok(filed);
    }
    let mut values = filed.values.clone();
    values.multi_disc = false;
    let template = layout.template(values.kind);
    settle_by(template, root, values, extension, budget, filed.other_kind)
}

/// The mapping of the file at `path`, by its full path, ignoring case.
pub(crate) fn mapping_at<'a>(mappings: &'a [LocalSongMapping], path: &str) -> Option<&'a LocalSongMapping> {
    let full = get_full_path(path);
    mappings.iter().find(|mapping| {
        !mapping.local_path.is_empty() && dotnet::eq_ignore_case(&get_full_path(&mapping.local_path), &full)
    })
}

/// What a song's names are filled from when only its file can say: its own tags, read as the
/// File layout task reads them, with a Various Artists album artist making it a compilation as
/// placement decides one. None when the tags cannot name the song (no title, or no artist).
pub(crate) fn values_from_tags(tags: &LayoutTags) -> Option<LayoutValues> {
    if tags.title.is_none() || tags.performers.is_empty() {
        return None;
    }
    let mut values = tag_values(tags);
    let has_album = tags
        .album
        .as_deref()
        .is_some_and(|album| !dotnet::is_blank(album));
    if has_album && !tags.compilation && is_various_artists(tags.album_artist.as_deref()) {
        values.kind = LayoutKind::of(tags.release_type.as_deref(), true, false);
        values.album_type = album_type_word(tags.release_type.as_deref(), true);
    }
    Some(values)
}

/// One template's answer for `values`: settled once; again with `{Disc}` shown when the album's
/// folder already holds disc folders, though the song's own release did not say it has several
/// discs; and, for a song that would make the album's first disc folder, the loose tracks of the
/// album it would sit beside. `other_kind` asks a positive sign of any album folder the search
/// finds.
fn settle_by(
    template: &Template,
    root: &str,
    mut values: LayoutValues,
    extension: &str,
    budget: usize,
    other_kind: bool,
) -> std::io::Result<Filed> {
    let mut filed = settle_once(template, root, &values, extension, budget, other_kind)?;
    let album_dir = filed.album_dir.clone().filter(|_| filed.album_existed);
    if let Some(dir) = album_dir.as_deref()
        && template.uses(Field::Disc)
    {
        let discs = disc_folders(Path::new(dir));
        if !values.multi_disc && !discs.is_empty() {
            values.multi_disc = true;
            filed = settle_once(template, root, &values, extension, budget, other_kind)?;
        } else if values.multi_disc && discs.is_empty() && template.disc_folder_segment().is_some() {
            filed.loose = loose_tracks_of(dir, &values);
        }
    }
    filed.values = values;
    Ok(filed)
}

/// [`PathHelper::settle`] with the template's names for `values` and every hook: the album's
/// folder by any name ([`find_album_folder`], with a positive sign asked under another kind's
/// template or where no folder above the album's names an artist), the disc folder holding the
/// song's disc whatever it is called (a missing disc is disc 1), and a file name that shrinks
/// its values to fit.
fn settle_once(
    template: &Template,
    root: &str,
    values: &LayoutValues,
    extension: &str,
    budget: usize,
    other_kind: bool,
) -> std::io::Result<Filed> {
    let names = template.render(values, extension, budget);
    let place_of = |segment: Option<usize>| {
        segment.and_then(|segment| names.iter().position(|name| name.segment == segment))
    };
    let album_at = place_of(template.album_folder_segment());
    let disc_at = place_of(template.disc_folder_segment());
    let song = AlbumIdentity::of_song(values);
    let (year, album_type) = template.album_folder_shows();
    let shown = Shown { year, album_type };
    let positive = (other_kind || !template.artist_above_album_folder()).then_some(values.artist.as_str());
    let find_album = |parent: &str| find_album_folder(Path::new(parent), &song, shown, positive);
    let disc = values.disc.filter(|disc| *disc > 0).unwrap_or(1);
    let find_disc = |album_dir: &str| {
        disc_folders(Path::new(album_dir))
            .into_iter()
            .find(|(_, theirs)| *theirs == disc)
            .map(|(name, _)| name)
    };
    let fit = |room: usize| template.render_file_within(values, extension, room);
    let settled = PathHelper::settle(
        root,
        &names,
        extension,
        budget,
        SettleHooks {
            album: album_at.map(|at| AlbumHook {
                at,
                find: &find_album,
            }),
            disc: disc_at.map(|at| AlbumHook { at, find: &find_disc }),
            fit: Some(&fit),
        },
    )?;
    let album_dir = album_at.map(|at| {
        settled.folders[..=at]
            .iter()
            .fold(root.to_string(), |dir, name| join(&dir, name))
    });
    let album_existed = album_at.is_some_and(|at| settled.existing > at);
    Ok(Filed {
        settled,
        album_dir,
        album_existed,
        values: values.clone(),
        other_kind,
        loose: Vec::new(),
    })
}

/// The identity of the album folder at `dir` with its disc folders, None when it holds no audio,
/// a file cannot be read, or its files are of more than one album.
fn folder_identity(dir: &Path) -> Option<AlbumIdentity> {
    let files = album_files(dir);
    let tags: Vec<LayoutTags> = files.iter().filter_map(|file| read_layout_tags(file)).collect();
    if tags.is_empty() || tags.len() != files.len() {
        return None;
    }
    AlbumIdentity::of_files(&tags)
}

/// Whether the album folder at `dir` holds the song's album on a positive sign: its files are of
/// the song's album, and of the same release group or every one credited to its `{Artist}`.
fn joins_positively(dir: &str, values: &LayoutValues) -> bool {
    let Some(folder) = folder_identity(Path::new(dir)) else {
        return false;
    };
    let song = AlbumIdentity::of_song(values);
    song.verdict(&folder) != Verdict::Different && song.positive_match(&folder, &values.artist)
}

/// The audio files directly in the album folder at `dir` that are not of another album (one whose
/// tags cannot be read counts as the album's).
fn loose_tracks_of(dir: &str, values: &LayoutValues) -> Vec<String> {
    let album = SongIdentity::key(&values.album);
    audio_files_in(Path::new(dir))
        .into_iter()
        .filter(|file| {
            read_layout_tags(file)
                .is_none_or(|tags| SongIdentity::key(tags.album.as_deref().unwrap_or("")) == album)
        })
        .map(|file| file.to_string_lossy().into_owned())
        .collect()
}

/// One audio file of the album folder at `dir`, never the song's own `landed` file, to read the
/// album's facts from: the album's identify sibling and placement's pin both take this one. It
/// carries the year most of the folder's files carry and the folder's own album artist (or
/// none), so the facts it gives keep the song in that folder's album; else the first of the
/// majority year; else the first. None when the folder holds no other file, or holds another
/// album than the song's.
pub(super) fn folder_fact_file(dir: &str, values: &LayoutValues, landed: &str) -> Option<String> {
    let landed = get_full_path(landed);
    let read: Vec<(PathBuf, LayoutTags)> = album_files(Path::new(dir))
        .into_iter()
        .filter(|file| !dotnet::eq_ignore_case(&get_full_path(&file.to_string_lossy()), &landed))
        .filter_map(|file| read_layout_tags(&file).map(|tags| (file, tags)))
        .collect();
    let tags: Vec<LayoutTags> = read.iter().map(|(_, tags)| tags.clone()).collect();
    let folder = AlbumIdentity::of_files(&tags)?;
    if AlbumIdentity::of_song(values).verdict(&folder) == Verdict::Different {
        return None;
    }
    let of_year = |tags: &LayoutTags| folder.year.is_none() || tags.year == folder.year;
    let of_artist = |tags: &LayoutTags| {
        tags.album_artist
            .as_deref()
            .is_none_or(|artist| artist_key(artist) == folder.album_artist)
    };
    read.iter()
        .find(|(_, tags)| of_year(tags) && of_artist(tags))
        .or_else(|| read.iter().find(|(_, tags)| of_year(tags)))
        .or_else(|| read.first())
        .map(|(file, _)| file.to_string_lossy().into_owned())
}

/// The album-level values a song took from its album's folder (its year, disc count, album
/// artist and release group), when the song's album is the one the values name.
fn take_album_facts(values: &mut LayoutValues, song: &Song) {
    if dotnet::is_blank(&values.album) || SongIdentity::key(&song.album) != SongIdentity::key(&values.album) {
        return;
    }
    if let Some(year) = song_year(song) {
        values.year = Some(year);
    }
    if let Some(count) = song.total_discs.filter(|count| *count > 0) {
        values.disc_count = Some(count);
    }
    if let Some(album_artist) = song.album_artist.clone().filter(|name| !dotnet::is_blank(name)) {
        values.album_artist = Some(album_artist);
    }
    if let Some(group) = song
        .music_brainz_release_group_id
        .clone()
        .filter(|id| !dotnet::is_blank(id))
    {
        values.release_group = Some(group);
    }
}

/// Makes the folders of `settled` that are not there yet, from the root down, and gives them
/// back in that order. A folder that cannot be made takes the ones made before it away again.
fn make_folders(root: &str, settled: &Settled) -> std::io::Result<Vec<String>> {
    std::fs::create_dir_all(root)?;
    let mut made = Vec::new();
    let mut dir = root.to_string();
    for name in &settled.folders {
        dir = join(&dir, name);
        if Path::new(&dir).is_dir() {
            continue;
        }
        if let Err(e) = std::fs::create_dir(&dir) {
            remove_made(&made);
            return Err(e);
        }
        made.push(dir.clone());
    }
    Ok(made)
}

/// Removes the folders a placement made, deepest first, each only while it is empty.
fn remove_made(made: &[String]) {
    for dir in made.iter().rev() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Whether `dir` holds an audio file, directly or in one of its folders.
fn holds_audio(dir: &str) -> std::io::Result<bool> {
    if !Path::new(dir).is_dir() {
        return Ok(false);
    }
    Ok(!album_files(Path::new(dir)).is_empty())
}

/// Whether two paths are one file on disk (a link, or one name in other capitals).
fn same_file(a: &str, b: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

/// Moves a file into the library, to a path nothing lies at and never over a file that lands
/// there meanwhile (an import of Lidarr's runs outside Octo's locks): linked into place and the
/// old name removed; or, on another disk or a file system with no hard links, a copy that takes
/// `to`'s name only once it is whole ([`copy_into_place`]), with a record of it in `staging`
/// while it is under way, so a stop leaves nothing [`recover_replacements`] does not put right.
/// Whatever fails leaves the file where it was.
pub(super) fn move_file(staging: &Path, from: &str, to: &str) -> std::io::Result<()> {
    match link(from, to) {
        Ok(()) => std::fs::remove_file(from).inspect_err(|_| {
            let _ = std::fs::remove_file(to);
        }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(already_there(to)),
        Err(_) => {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let staged = beside(to, &format!("{MOVING_PREFIX}{id}"));
            let mut record = UnderWay::record(
                staging,
                &id,
                &ReplaceRecord {
                    original: to.to_string(),
                    staged: staged.to_string_lossy().into_owned(),
                    source: from.to_string(),
                    size: size_of(Path::new(from)),
                    ..ReplaceRecord::default()
                },
            )?;
            copy_into_place(from, &staged, to, &mut record)?;
            if record.settle(Some(Path::new(from)), Path::new(to)).is_err() {
                let _ = record.keep_moved();
            }
            Ok(())
        }
    }
}

/// A hard link, which a test can make answer that the two paths are on different file systems.
fn link(from: &str, to: &str) -> std::io::Result<()> {
    #[cfg(test)]
    if COPY_ACROSS.with(std::cell::Cell::get) {
        return Err(std::io::Error::from(std::io::ErrorKind::CrossesDevices));
    }
    std::fs::hard_link(from, to)
}

#[cfg(test)]
thread_local! {
    /// Every link on this thread answers that the paths are on different file systems.
    pub(crate) static COPY_ACROSS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Run with the hidden copy and the name it is about to take, once the copy is whole.
    #[allow(clippy::type_complexity)]
    pub(crate) static AFTER_COPY: std::cell::RefCell<Option<Box<dyn FnMut(&Path, &str)>>> =
        const { std::cell::RefCell::new(None) };
    /// What the moves and recoveries on this thread did to make a rename last before a file
    /// goes, in order: `flush <folder>` and `remove <file>`.
    pub(crate) static SETTLING: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// How many kept copies the recoveries on this thread have said.
    pub(crate) static KEPT_REPORTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The files whose removal the disk refuses on this thread.
    pub(crate) static REFUSED_REMOVALS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Whether the disk refuses to write a record marked moved on this thread.
    pub(crate) static REFUSED_MOVED_MARKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The folders the disk refuses to flush on this thread.
    pub(crate) static REFUSED_FLUSHES: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Removes the file at `path` (a test can make the disk refuse it).
fn remove_file(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    {
        SETTLING.with(|log| {
            log.borrow_mut()
                .push(format!("remove {}", path.to_string_lossy()))
        });
        if REFUSED_REMOVALS.with(|refused| refused.borrow().iter().any(|refused| refused == path)) {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
    }
    std::fs::remove_file(path)
}

/// Copies `from` onto `to` across file systems: copied to `staged`, a hidden name beside `to`,
/// flushed to the disk and marked whole in its `record`, then renamed onto `to` in one step
/// within its folder, so `to` never holds part of a file. `from` stays: the caller lets it go
/// once the rename lasts ([`UnderWay::settle`]).
fn copy_into_place(from: &str, staged: &Path, to: &str, record: &mut UnderWay) -> std::io::Result<()> {
    let copied = std::fs::copy(from, staged)
        .and_then(|_| std::fs::OpenOptions::new().write(true).open(staged)?.sync_all())
        .and_then(|()| record.mark_whole());
    if let Err(e) = copied {
        let _ = std::fs::remove_file(staged);
        return Err(e);
    }
    #[cfg(test)]
    AFTER_COPY.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(staged, to);
        }
    });
    if let Err(e) = place_new(staged, to) {
        let _ = std::fs::remove_file(staged);
        return Err(e);
    }
    Ok(())
}

/// Gives the whole copy at `staged` the name `to`, never over a file that lands there meanwhile:
/// linked into place and the hidden name removed, or, on a file system with no hard links, the
/// name claimed by creating it empty before the copy is renamed onto it. A stop between the steps
/// leaves the copy under both names, or the empty claim beside it, which a recovery puts right.
fn place_new(staged: &Path, to: &str) -> std::io::Result<()> {
    match std::fs::hard_link(staged, to) {
        Ok(()) => {
            let _ = std::fs::remove_file(staged);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(already_there(to)),
        Err(_) => std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)
            .and_then(|claimed| {
                drop(claimed);
                std::fs::rename(staged, to).inspect_err(|_| {
                    let _ = std::fs::remove_file(to);
                })
            }),
    }
}

/// `name` in the folder of `path`.
fn beside(path: &str, name: &str) -> PathBuf {
    match Path::new(path).parent() {
        Some(folder) => folder.join(name),
        None => PathBuf::from(name),
    }
}

/// The folder under the staging folder that holds the records of moves under way.
const REPLACING_FOLDER: &str = "replacing";

/// How the old file of a replace is named while the new one moves in, and (with `.new`) the new
/// one while it is copied across file systems: dot names, which Navidrome and the tidy never read.
const REPLACING_PREFIX: &str = ".octo-replacing-";

/// How a file copied across file systems is named beside its place until it is whole.
const MOVING_PREFIX: &str = ".octo-moving-";

/// How a family pool copy is named, after the name it takes, until it is whole.
const COPY_SUFFIX: &str = ".octo-copy";

/// What a move into the library was doing, kept in the staging folder until it is done: the
/// file's own path; for a replace, where its old copy waits beside it; a copy that has not taken
/// its name yet; and the file a copy across file systems was made from, with its size, which goes
/// once the copy has its name. `whole` says the copy was flushed to the disk whole, before it
/// took its name. `moved` says the source goes wherever it is once the copy has its name: the
/// move's caller follows the copy whatever the answer, or the move was answered as done while
/// its source could not go yet. `undo` says the move's caller takes an error as nothing moved, so
/// a move a stop cut short after its rename is taken back. `reported` says a recovery has already
/// said it keeps the copy.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct ReplaceRecord {
    original: String,
    #[serde(default)]
    backup: String,
    #[serde(default)]
    staged: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    whole: bool,
    #[serde(default)]
    moved: bool,
    #[serde(default)]
    undo: bool,
    #[serde(default)]
    reported: bool,
}

/// Writes `record` at `path` whole under a side name, flushed, then renamed over it, and the
/// records folder flushed, so a stop never leaves it half written and a power cut never takes
/// back what it says once it is written.
fn write_record(path: &Path, record: &ReplaceRecord) -> std::io::Result<()> {
    let mut side = path.as_os_str().to_owned();
    side.push(".tmp");
    let side = PathBuf::from(side);
    let written = serde_json::to_vec(record)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            let mut file = std::fs::File::create(&side)?;
            std::io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&side, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&side);
    }
    written.and_then(|()| flush_folder_of(path))
}

/// The record at `path`, when it can be read whole.
fn read_record(path: &Path) -> Option<ReplaceRecord> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ReplaceRecord>(&bytes).ok())
}

/// The size of the file at `path`, 0 when it cannot be read.
fn size_of(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |meta| meta.len())
}

/// Flushes the folder holding `path` to its disk, so a rename into it lasts a power cut. A disk
/// that cannot flush a folder (and Windows, where a folder cannot be opened that way; the server
/// runs on Linux) counts as done.
fn flush_folder_of(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    SETTLING.with(|log| {
        let folder = path.parent().map(|folder| folder.to_string_lossy().into_owned());
        log.borrow_mut()
            .push(format!("flush {}", folder.unwrap_or_default()));
    });
    #[cfg(test)]
    if REFUSED_FLUSHES.with(|refused| {
        refused
            .borrow()
            .iter()
            .any(|refused| path.parent() == Some(refused.as_path()))
    }) {
        return Err(std::io::Error::other("the disk refused to flush the folder"));
    }
    #[cfg(unix)]
    if let Some(folder) = path.parent().filter(|folder| !folder.as_os_str().is_empty()) {
        match std::fs::File::open(folder).and_then(|folder| folder.sync_all()) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
                ) => {}
            Err(e) => return Err(e),
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The records of the moves under way in this process, which a recovery leaves alone.
static UNDER_WAY: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// One move's record in the staging folder while the move is under way: written when it starts,
/// and removed when the move is done, unless [`UnderWay::keep`] leaves it for a recovery. The
/// records folder itself stays, so a record is never written into a folder going away.
pub(crate) struct UnderWay {
    id: String,
    records: PathBuf,
    record: ReplaceRecord,
    kept: bool,
}

impl UnderWay {
    /// Writes `record` under `id` in `staging`'s records folder ([`write_record`]), before
    /// anything moves.
    fn record(staging: &Path, id: &str, record: &ReplaceRecord) -> std::io::Result<UnderWay> {
        UNDER_WAY.lock().insert(id.to_string());
        let under_way = UnderWay {
            id: id.to_string(),
            records: staging.join(REPLACING_FOLDER),
            record: record.clone(),
            kept: false,
        };
        std::fs::create_dir_all(&under_way.records)?;
        // The records folder, and the staging folder holding it, last a power cut before any
        // record in it is counted on.
        flush_folder_of(&under_way.records)?;
        flush_folder_of(staging)?;
        write_record(&under_way.path(), record)?;
        Ok(under_way)
    }

    fn path(&self) -> PathBuf {
        self.records.join(format!("{}.json", self.id))
    }

    /// Writes the record again with `change` made ([`write_record`]).
    fn mark(&mut self, change: impl FnOnce(&mut ReplaceRecord)) -> std::io::Result<()> {
        #[cfg(test)]
        let was_moved = self.record.moved;
        change(&mut self.record);
        #[cfg(test)]
        if !was_moved && self.record.moved && REFUSED_MOVED_MARKS.with(std::cell::Cell::get) {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        write_record(&self.path(), &self.record)
    }

    /// Marks the copy whole on the disk, once it is flushed and before it takes its name: a
    /// recovery gives a copy whose source is gone its name only then.
    pub(crate) fn mark_whole(&mut self) -> std::io::Result<()> {
        self.mark(|record| record.whole = true)
    }

    /// Makes the rename onto `to` last a power cut (its folder flushed), then removes `from`, the
    /// file the copy was made from, when given (one already gone counts as removed): never the
    /// other way round, so a power cut never leaves the song with no whole copy. An error when
    /// either could not be done; `from` then stays.
    pub(crate) fn settle(&self, from: Option<&Path>, to: &Path) -> std::io::Result<()> {
        if let Err(e) = flush_folder_of(to) {
            warn!("Could not flush the folder of {}: {e}", to.display());
            return Err(e);
        }
        let Some(from) = from else {
            return Ok(());
        };
        match remove_file(from) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                warn!(
                    "{} is in place, but its copy at {} could not be removed: {e}",
                    to.display(),
                    from.display()
                );
                Err(e)
            }
        }
    }

    /// [`Self::settle`] for a family pool move, whose caller takes an error as nothing moved.
    /// When the settle fails while `from` is still whole, the copy at `to` is removed (that
    /// folder flushed, else the record is kept so the next recovery takes the copy back again)
    /// and the error answered: the song stays where it was, alone. When the copy cannot be
    /// removed, the copy is the song's: the move is done, and its record is kept for the next
    /// recovery to remove `from` ([`Self::keep_moved`]). A record that cannot be marked so is kept
    /// as it is and the error answered, so the next recovery takes the copy back instead
    /// ([`recover_replacements`]).
    pub(crate) fn settle_or_undo(self, from: &Path, to: &Path) -> std::io::Result<()> {
        let Err(e) = self.settle(Some(from), to) else {
            return Ok(());
        };
        if from.is_file() && size_of(from) == size_of(to) {
            match remove_file(to) {
                Ok(()) => {
                    warn!(
                        "{} stays where it was: its copy at {} was taken back",
                        from.display(),
                        to.display()
                    );
                    if let Err(flush) = flush_folder_of(to) {
                        warn!("Could not flush the folder of {}: {flush}", to.display());
                        self.keep();
                    }
                    return Err(e);
                }
                Err(undo) => warn!(
                    "Could not take back the copy at {}: {undo}; it is the song's now, and {} goes once it can",
                    to.display(),
                    from.display()
                ),
            }
        }
        if self.keep_moved() { Ok(()) } else { Err(e) }
    }

    /// Leaves the record for the next recovery to finish.
    pub(crate) fn keep(mut self) {
        self.kept = true;
    }

    /// Leaves the record for the next recovery, marked moved: the move was answered as done, so
    /// the file the copy was made from goes wherever it is, once the name holds as many bytes.
    /// True when the record on the disk says moved, which it does once written even when the
    /// folder flush after it failed; false when it is kept as it was.
    pub(crate) fn keep_moved(mut self) -> bool {
        self.kept = true;
        match self.mark(|record| record.moved = true) {
            Ok(()) => true,
            Err(e) => {
                warn!("Could not mark {} as moved: {e}", self.path().display());
                read_record(&self.path()).is_some_and(|record| record.moved)
            }
        }
    }
}

impl Drop for UnderWay {
    fn drop(&mut self) {
        if !self.kept {
            let _ = std::fs::remove_file(self.path());
        }
        UNDER_WAY.lock().remove(&self.id);
    }
}

/// Puts the file at `from` where `taken` is, never losing the song: the old file is renamed
/// aside in its own folder, with a record of it in `staging`, and goes only once the new file is
/// in, renamed onto the name in one step (a copy across file systems takes it only once whole).
/// A move that fails puts the old file back. Octo stopped between the steps is put right by
/// [`recover_replacements`].
fn replace_same_song(staging: &Path, taken: &str, from: &str) -> std::io::Result<()> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let backup = beside(taken, &format!("{REPLACING_PREFIX}{id}"));
    let staged = beside(taken, &format!("{REPLACING_PREFIX}{id}.new"));
    let mut record = UnderWay::record(
        staging,
        &id,
        &ReplaceRecord {
            original: taken.to_string(),
            backup: backup.to_string_lossy().into_owned(),
            staged: staged.to_string_lossy().into_owned(),
            source: from.to_string(),
            size: size_of(Path::new(from)),
            ..ReplaceRecord::default()
        },
    )?;
    std::fs::rename(taken, &backup)?;
    let mut copied = false;
    // Linked in, never over a file that lands at the name meanwhile; across file systems (or with
    // no hard links), copied in whole.
    let moved = match link(from, taken) {
        Ok(()) => std::fs::remove_file(from).inspect_err(|_| {
            let _ = std::fs::remove_file(taken);
        }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(already_there(taken)),
        Err(_) => {
            copied = true;
            copy_into_place(from, &staged, taken, &mut record)
        }
    };
    if let Err(e) = moved {
        if let Err(back) = std::fs::rename(&backup, taken) {
            error!(
                "Could not put {taken} back from {} after the new copy failed to move in: {back}; it goes back before the next file is placed",
                backup.display()
            );
            record.keep();
        }
        return Err(e);
    }
    // The new copy's name lasts a power cut before its source or the old copy goes; what cannot
    // go yet keeps its record, so the next recovery finishes it.
    let source = copied.then(|| Path::new(from));
    if record.settle(source, Path::new(taken)).is_err() || std::fs::remove_file(&backup).is_err() {
        if copied {
            let _ = record.keep_moved();
        } else {
            record.keep();
        }
    }
    Ok(())
}

/// Whether `staging` holds records of moves not yet put right.
pub(super) fn replacements_pending(staging: &Path) -> bool {
    std::fs::read_dir(staging.join(REPLACING_FOLDER)).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().extension().and_then(|extension| extension.to_str()) == Some("json"))
    })
}

/// Puts right every move into the library (and every family pool copy) a stop left half done,
/// from its record in `staging` (a move under way in this process is left alone):
///
/// - A copy that never took its name, while the file it was made from is still there, is
///   removed: an original path holds the new copy only once that rename is done.
/// - A copy that never took its name, whose source is gone, is the only copy left (a power cut
///   kept the source's removal and lost the rename, or a sweep took the source): it takes its
///   name when its record says it was flushed whole and it holds the source's size, and is never
///   removed.
/// - A family pool move whose caller takes an error as nothing moved, whose copy has its name,
///   never answered as done, whose source is still whole, is taken back: the pool's records
///   still name the source, so the copy goes (that folder flushed) and the source never does.
/// - Once any other copy has its name, that folder is flushed, then the file it was made from
///   goes, when the name holds as many bytes and it is in the staging folder, or anywhere once
///   the move was answered as done with its copy flushed whole.
/// - For a replace, an original path that holds a file has the new copy, and its old copy goes
///   (that folder flushed first); one that holds none gets its old copy back.
/// - A record cut short was written before any file moved, so it goes alone.
///
/// Run under the placement lock (and the gate's share, where there is one).
pub(super) fn recover_replacements(staging: &Path) {
    let records = staging.join(REPLACING_FOLDER);
    let Ok(entries) = std::fs::read_dir(&records) else {
        return;
    };
    let ours = |path: &Path| {
        path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.starts_with(REPLACING_PREFIX)
                || name.starts_with(MOVING_PREFIX)
                || name.ends_with(COPY_SUFFIX)
        })
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let id = name.split('.').next().unwrap_or_default();
        if UNDER_WAY.lock().contains(id) {
            continue;
        }
        if name.ends_with(".json.tmp") {
            // A record a stop cut short while it was written again: the record stands whole.
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        if let Some(record) = read_record(&path) {
            let staged = Path::new(&record.staged);
            let original = Path::new(&record.original);
            let mut unfinished = !record.staged.is_empty() && staged.exists();
            let source_gone = !record.source.is_empty() && !Path::new(&record.source).exists();
            // The copy took its name by a link and a stop came before its hidden name went: one
            // file under two names, so the hidden one goes. A name claimed empty for the copy (on
            // a file system with no hard links) takes the copy, once it was flushed whole.
            if unfinished && ours(staged) && same_file(&record.staged, &record.original) {
                let _ = std::fs::remove_file(staged);
                unfinished = false;
            } else if unfinished
                && ours(staged)
                && record.whole
                && original.is_file()
                && size_of(original) == 0
                && (record.size == 0 || size_of(staged) == record.size)
            {
                if let Err(e) = std::fs::rename(staged, original).and_then(|()| flush_folder_of(original)) {
                    warn!(
                        "Could not give {} its name {}: {e}",
                        record.staged, record.original
                    );
                    continue;
                }
                unfinished = false;
            }
            if unfinished && ours(staged) && source_gone {
                if !name_the_only_copy(&path, &record, staged, original) {
                    continue;
                }
                unfinished = false;
            } else if unfinished && ours(staged) {
                if let Err(e) = std::fs::remove_file(staged) {
                    warn!(
                        "Could not remove {}, a copy a stop left unfinished: {e}",
                        record.staged
                    );
                    continue;
                }
                info!(
                    "Removed an unfinished copy of {} that a stop left",
                    record.original
                );
            }
            let backup = Path::new(&record.backup);
            // The copy took its name: nothing is left unfinished, the name holds a file, and a
            // replace's old copy waits aside (the name is only filled again by that rename).
            let renamed = !unfinished
                && original.is_file()
                && (record.backup.is_empty() || (ours(backup) && backup.exists()));
            if renamed && pool_move_cut_short(&record, original) {
                if !take_back_copy(original) {
                    continue;
                }
            } else if renamed && !remove_copied_source(staging, &record, original) {
                continue;
            }
            if !record.backup.is_empty() && ours(backup) && backup.exists() {
                if original.exists() {
                    if let Err(e) = flush_folder_of(original) {
                        warn!("Could not flush the folder of {}: {e}", record.original);
                        continue;
                    }
                    if let Err(e) = std::fs::remove_file(backup) {
                        warn!(
                            "Could not remove {}, the old copy of {}: {e}",
                            record.backup, record.original
                        );
                        continue;
                    }
                    info!(
                        "Removed the old copy of {} that a stop left beside it",
                        record.original
                    );
                } else {
                    if let Err(e) = std::fs::rename(backup, &record.original) {
                        warn!(
                            "Could not put {} back from {}: {e}",
                            record.original, record.backup
                        );
                        continue;
                    }
                    info!(
                        "Put {} back: Octo stopped before its new copy moved in",
                        record.original
                    );
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Renames `staged`, a copy whose source is gone, onto its name `original`, then flushes that
/// folder: the copy is the only one left. Only when its record says it was flushed whole, it
/// holds the size the record gave its source (or the record gave none) and the name is free;
/// else nothing moves and nothing is removed, and the kept copy is said once
/// ([`report_kept_copy`]). False when the record (at `path`) is to stay.
fn name_the_only_copy(path: &Path, record: &ReplaceRecord, staged: &Path, original: &Path) -> bool {
    if !record.whole {
        report_kept_copy(path, record, "Octo stopped before it was flushed whole");
        return false;
    }
    if record.size > 0 && size_of(staged) != record.size {
        let why = format!(
            "it holds {} of the {} bytes its original had",
            size_of(staged),
            record.size
        );
        report_kept_copy(path, record, &why);
        return false;
    }
    if original.exists() {
        report_kept_copy(path, record, &format!("{} is taken", record.original));
        return false;
    }
    if let Err(e) = std::fs::rename(staged, original).and_then(|()| flush_folder_of(original)) {
        warn!(
            "Could not give {} its name {}: {e}",
            record.staged, record.original
        );
        return false;
    }
    info!(
        "{} took its name: a stop left its copy whole and its source gone",
        record.original
    );
    true
}

/// Says once that a recovery keeps `record`'s unfinished copy where it is, `why`: its original is
/// gone, so nothing was replaced. The record (at `path`) is marked as said ([`write_record`]), so
/// later recoveries pass it quietly.
fn report_kept_copy(path: &Path, record: &ReplaceRecord, why: &str) {
    if record.reported {
        return;
    }
    let name = Path::new(&record.original)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| record.original.clone());
    warn!(
        "Octo kept an unfinished copy of {name} at {}, meant for {}: its original is gone, so nothing was replaced ({why})",
        record.staged, record.original
    );
    #[cfg(test)]
    KEPT_REPORTS.with(|reports| reports.set(reports.get() + 1));
    let marked = ReplaceRecord {
        reported: true,
        ..record.clone()
    };
    if let Err(e) = write_record(path, &marked) {
        warn!("Could not mark {} as said: {e}", path.display());
    }
}

/// Whether `record` is a family pool move a stop cut short after its copy took the name
/// `original` and before its source went: one whose caller takes an error as nothing moved
/// (`undo`), never answered as done (so the pool's records still name the source), with the
/// source still there, as many bytes as the copy.
fn pool_move_cut_short(record: &ReplaceRecord, original: &Path) -> bool {
    let source = Path::new(&record.source);
    record.undo
        && !record.moved
        && !record.source.is_empty()
        && source.is_file()
        && size_of(source) == size_of(original)
}

/// Removes `copy`, the copy of a family pool move a stop cut short ([`pool_move_cut_short`]),
/// then flushes its folder, so the song is only where the pool's records say. False when either
/// could not be done, so the record stays for the next recovery.
fn take_back_copy(copy: &Path) -> bool {
    if let Err(e) = remove_file(copy) {
        warn!(
            "Could not take back {}, the copy of a move a stop cut short: {e}",
            copy.display()
        );
        return false;
    }
    if let Err(e) = flush_folder_of(copy) {
        warn!("Could not flush the folder of {}: {e}", copy.display());
        return false;
    }
    info!(
        "Took back {}: Octo stopped before its move was done, so the song stays where it was",
        copy.display()
    );
    true
}

/// Removes the file `record`'s copy across file systems was made from, now that the copy holds
/// the name `original`: only when `original` holds as many bytes and the source lies in the
/// staging folder, or anywhere once the move was answered as done (`moved`) with its copy
/// flushed whole (`whole`, so the name holds that copy), and only once that name's folder is
/// flushed, so a song is never left with no whole copy. False when it should go and could not,
/// so its record stays for the next recovery.
fn remove_copied_source(staging: &Path, record: &ReplaceRecord, original: &Path) -> bool {
    let source = record.source.as_str();
    let from = Path::new(source);
    let anywhere = record.moved && record.whole;
    if source.is_empty() || !(anywhere || from.starts_with(staging)) || !from.is_file() {
        return true;
    }
    if size_of(from) != size_of(original) {
        warn!(
            "{source} stays: {} does not hold as many bytes",
            original.display()
        );
        return true;
    }
    if let Err(e) = flush_folder_of(original) {
        warn!("Could not flush the folder of {}: {e}", original.display());
        return false;
    }
    match remove_file(from) {
        Ok(()) => {
            info!(
                "Removed {source}: it was left after its copy took the name {}",
                original.display()
            );
            true
        }
        Err(e) => {
            warn!(
                "Could not remove {source}, already copied to {}: {e}",
                original.display()
            );
            false
        }
    }
}

/// Moves the files written beside `from` (its name with another extension, such as its lyrics)
/// to beside `to`, named after it. Best effort; one already there stays. The files it moved, by
/// their new paths.
fn move_beside(from: &str, to: &str) -> Vec<String> {
    let mut moved = Vec::new();
    let (from, to) = (Path::new(from), Path::new(to));
    let (Some(from_dir), Some(to_dir)) = (from.parent(), to.parent()) else {
        return moved;
    };
    let (Some(from_stem), Some(to_stem)) = (from.file_stem(), to.file_stem()) else {
        return moved;
    };
    let Ok(entries) = std::fs::read_dir(from_dir) else {
        return moved;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_stem() != Some(from_stem) {
            continue;
        }
        let Some(extension) = path.extension() else {
            continue;
        };
        let beside = to_dir.join(format!(
            "{}.{}",
            to_stem.to_string_lossy(),
            extension.to_string_lossy()
        ));
        if move_unrecorded(&path.to_string_lossy(), &beside.to_string_lossy()).is_ok() {
            moved.push(beside.to_string_lossy().into_owned());
        }
    }
    moved
}

/// The folder of a path: everything before the last separator, `"/"` for a file in the root,
/// empty for a bare name, and `None` for the root itself or an empty path.
pub(super) fn get_directory_name(path: &str) -> Option<String> {
    if path.is_empty() || path == "/" {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(path[..at].to_string()),
        None => Some(String::new()),
    }
}

/// The file name of a path: what follows the last `/`, or the whole path when it has none.
pub(super) fn get_file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The first file under `dir`, at any depth, whose name starts with `prefix`.
fn find_file(dir: &Path, prefix: &str) -> std::io::Result<Option<String>> {
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                folders.push(entry.path());
            } else if entry.file_name().to_string_lossy().starts_with(prefix) {
                return Ok(Some(entry.path().to_string_lossy().into_owned()));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
#[path = "placement_follow_tests.rs"]
mod follow_tests;
