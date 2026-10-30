//! Where a download goes: the configured layout, the name it is filed under, a path already
//! taken, a library action's replacement, the files beside it, and the clean-up behind it.

use std::path::Path;

use octo_core::common::{PathHelper, SongIdentity, artist_credit, dotnet};
use octo_core::fingerprint::VerificationVerdict;
use octo_core::fingerprint::verification::RecordingProof;
use octo_core::lyrics::lyrics_text::LyricsText;
use octo_core::models::domain::Song;
use octo_core::settings::{DownloadSource, FolderStructure};
use octo_media::cover::cover_files;
use octo_media::tags::KeptIdentityTags;
use octo_media::tags::tag_writer_extras;
use tracing::{debug, error, info, warn};

use super::{BaseDownloadService, FileNotFoundException, RequestedIdentity, file_exists, get_extension};
use crate::services::common::acquisition_tracker::{acquisition_event_kinds, key_of};
use crate::services::common::soulseek_hold_store::held_root;
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::library::replacement_handoff::ArrivedTags;
use crate::services::library::{ReplacementHandoff, ReplacementRejectedException};
use crate::services::lidarr::{FollowEnd, HELD_FOR_LIDARR, HeldPlacement};
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

/// Where a download was placed, and whether its folder is new, which is the only place a
/// cover.jpg may go without changing an album that was already there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub path: String,
    pub created_folder: bool,
}

impl Placement {
    pub fn new(path: impl Into<String>, created_folder: bool) -> Self {
        Self {
            path: path.into(),
            created_folder,
        }
    }
}

const AUDIO_EXTENSIONS: [&str; 14] = [
    ".flac", ".mp3", ".m4a", ".aac", ".alac", ".ogg", ".opus", ".wav", ".aiff", ".aif", ".ape", ".wv",
    ".wma", ".dsf",
];

pub(super) fn is_audio_file(path: &str) -> bool {
    let extension = get_extension(path);
    AUDIO_EXTENSIONS
        .iter()
        .any(|known| dotnet::eq_ignore_case(known, extension))
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
        if let Err(e) = move_file(current_path, &held_path) {
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

    /// Places a file a Lidarr follow held back, now that no follow holds it: moved where the
    /// layout gave it (beside a different file already there, never over it), with whatever was
    /// written beside it, the library's record moved along, and a scan asked for. Never while a
    /// library job or a download is placing files. None when it could not be placed.
    pub async fn place_held_file(&self, held: &HeldPlacement) -> Option<String> {
        let _share = match &self.services.write_gate {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        let _permit = self.download_lock.acquire().await.ok()?;
        if !file_exists(&held.held_path) {
            return None;
        }
        let mut target = held.target.clone();
        if file_exists(&target) {
            target = PathHelper::resolve_unique_path(&target);
        }
        if let Some(dir) = get_directory_name(&target).filter(|d| !d.is_empty())
            && let Err(e) = std::fs::create_dir_all(&dir)
        {
            warn!("Could not make {dir} for a held file: {e}");
            return None;
        }
        if let Err(e) = move_file(&held.held_path, &target) {
            warn!("Could not place the held {}: {e}", held.held_path);
            return None;
        }
        move_beside(&held.held_path, &target);
        // The hold folder goes with its last file.
        if let Some(folder) = Path::new(&held.held_path).parent() {
            let _ = std::fs::remove_dir(folder);
        }
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
        self.local_library.trigger_library_scan(false).await;
        info!(
            "Placed the held {} where the layout files it: {target}",
            held.song
        );
        Some(target)
    }

    /// Drops a kept copy a Lidarr follow held back, once a proven copy of its song passed the gate
    /// of every person the copy was kept for (`users`): its file, whatever was written beside it,
    /// and the library's record of it go, so those people end with the proven copy alone. A copy
    /// the library records for anyone else, or does not record at all, stays. Never while a
    /// library job or a download is placing files.
    pub async fn drop_held_copy(&self, held: &HeldPlacement, users: &[String]) -> bool {
        let _share = match &self.services.write_gate {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        let Ok(_permit) = self.download_lock.acquire().await else {
            return false;
        };
        let mapping = self
            .local_library
            .get_mappings()
            .await
            .into_iter()
            .find(|mapping| mapping.local_path == held.held_path);
        let only_theirs = mapping
            .as_ref()
            .and_then(|mapping| mapping.heart.as_ref())
            .is_some_and(|heart| {
                !heart.users.is_empty()
                    && heart.users.iter().all(|kept_for| {
                        users
                            .iter()
                            .any(|user| dotnet::eq_ignore_case(user.trim(), kept_for.trim()))
                    })
            });
        if !only_theirs {
            return false;
        }
        if let Err(e) = self.local_library.forget_mapping(&held.held_path).await {
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
        move_file(current_path, &target)?;
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
        let _share = match &self.services.write_gate {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        match self.try_place(song, requested, current_path, root).await {
            Ok(placement) => placement,
            Err(e) => {
                warn!(
                    "Could not place {current_path} in the configured layout; leaving it where it landed: {e}"
                );
                Placement::new(current_path, false)
            }
        }
    }

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
        let mut target = self.layout_target_in(&layout_root, song, requested, get_extension(current_path));

        if dotnet::eq_ignore_case(&get_full_path(&target), &get_full_path(current_path)) {
            return Ok(Placement::new(current_path, false));
        }

        let target_dir = get_directory_name(&target).unwrap_or_default();
        let folder_had_audio = !target_dir.is_empty()
            && Path::new(&target_dir).is_dir()
            && std::fs::read_dir(&target_dir)?
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .any(|e| is_audio_file(&e.file_name().to_string_lossy()));

        if file_exists(&target) {
            if self.is_same_song(&target, song).await {
                info!("{target} already holds this song; replacing it with the new download");
                std::fs::remove_file(&target)?;
            } else {
                let unique = PathHelper::resolve_unique_path(&target);
                info!("{target} already holds a different file; keeping both as {unique}");
                target = unique;
            }
        }

        if !target_dir.is_empty() {
            std::fs::create_dir_all(&target_dir)?;
        }
        move_file(current_path, &target)?;
        info!("Placed download in {structure:?}: {current_path} -> {target}");

        // Clean up any now-empty folder the file came from (a Soulseek peer's own layout, or the
        // YouTube staging folder), never walking above the music root.
        Self::try_remove_empty_parents(get_directory_name(current_path).as_deref(), &download_path);
        Ok(Placement::new(target, !folder_had_audio))
    }

    /// Where the configured layout files this song, before any clash is looked at.
    fn layout_target(&self, song: &Song, requested: &RequestedIdentity, extension: &str) -> String {
        self.layout_target_in(&self.download_path(), song, requested, extension)
    }

    /// [`Self::layout_target`] under `root`.
    fn layout_target_in(
        &self,
        root: &str,
        song: &Song,
        requested: &RequestedIdentity,
        extension: &str,
    ) -> String {
        let current = self.settings.current();
        let choice = Self::choose_layout(song, requested, current.soulseek.name_from_match);
        let structure = current.subsonic.folder_structure;
        // Flat has no folder to scatter, so its file name keeps the whole credit (#49).
        let artist = if structure == FolderStructure::Flat {
            &choice.file_artist
        } else {
            &choice.folder_artist
        };
        PathHelper::build_layout_path(
            structure,
            root,
            if dotnet::is_blank(artist) {
                "Unknown Artist"
            } else {
                artist
            },
            &choice.album,
            &PathHelper::file_title(&choice.title, &choice.file_artist),
            choice.track,
            extension,
        )
    }

    /// The path the configured layout gives the song in an Organized library, for the album
    /// folder a single may join.
    pub(super) fn organized_target(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        extension: &str,
    ) -> String {
        let choice = Self::choose_layout(song, requested, self.settings.current().soulseek.name_from_match);
        PathHelper::build_layout_path(
            FolderStructure::Organized,
            &self.download_path(),
            if dotnet::is_blank(&choice.folder_artist) {
                "Unknown Artist"
            } else {
                &choice.folder_artist
            },
            &choice.album,
            &PathHelper::file_title(&choice.title, &choice.file_artist),
            choice.track,
            extension,
        )
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
        move_file(landed_path, &staged)?;
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

            let extension = get_extension(staged);
            let target = handoff.target_for(extension).unwrap_or_else(|| {
                PathHelper::resolve_unique_path(&self.layout_target(song, requested, extension))
            });
            if let Some(dir) = get_directory_name(&target).filter(|d| !d.is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            move_file(staged, &target)?;
            handoff.set_revealed_path(&target);
            if let Some(on_revealed) = &handoff.on_revealed {
                on_revealed(&target);
            }
            info!("Placed the replacement where the original was: {target}");
            anyhow::Ok(Placement::new(target, false))
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
    /// Anything short of that is somebody's other file and is never replaced.
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
    /// cover.jpg only in the Organized layout and only in a folder this download created.
    /// Navidrome ranks cover.* above embedded art, so in Flat every download shares one folder,
    /// in ByArtist one folder holds all of an artist's albums, and in an album folder that was
    /// already there one new track would change the whole album's cover. The one exception is a
    /// cover.jpg Octo wrote itself, in the Organized layout: a later track of the same album that
    /// found a larger cover replaces it, so a soft first track does not set the album's cover for
    /// good.
    pub fn write_sidecars(&self, song: &Song, placement: &Placement, cover: Option<&[u8]>) {
        let current = self.settings.current();
        if current.metadata.write_cover_file
            && let Some(cover) = cover.filter(|c| !c.is_empty())
            && current.subsonic.folder_structure == FolderStructure::Organized
            && let Some(dir) = get_directory_name(&placement.path).filter(|d| !d.is_empty())
            && cover_files::should_write(Path::new(&dir), cover, placement.created_folder)
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

/// Moves a file: a rename, or a copy and a delete across file systems. Never over an existing
/// file.
pub(super) fn move_file(from: &str, to: &str) -> std::io::Result<()> {
    if Path::new(to).exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("The file '{to}' already exists."),
        ));
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

/// Moves the files written beside `from` (its name with another extension, such as its lyrics)
/// to beside `to`, named after it. Best effort; one already there stays.
fn move_beside(from: &str, to: &str) {
    let (from, to) = (Path::new(from), Path::new(to));
    let (Some(from_dir), Some(to_dir)) = (from.parent(), to.parent()) else {
        return;
    };
    let (Some(from_stem), Some(to_stem)) = (from.file_stem(), to.file_stem()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(from_dir) else {
        return;
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
        if !beside.exists() {
            let _ = std::fs::rename(&path, &beside);
        }
    }
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
