//! What the apps' Library health changes in a song's file other than removing it (tags written
//! in place, an album joined to another, a cover added where there was none, a lookup of the tags
//! a download would get, and the undo of each), putting a removed song back, and `LibraryRescan`,
//! the one Navidrome scan a little after a run of changes.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::SongIdentity;
use octo_core::common::dotnet::is_blank;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::models::domain::Song;
use octo_core::settings::{LibraryAction, MetadataSettings, SettingsStore};
use octo_core::tagging::{AlbumTagContext, FingerprintVerifier, ReleaseIdentifier, TagPlan};
use octo_media::cover::cover_image;
use octo_media::tags::TagFile;
use octo_media::tags::kept_identity::{KeptIdentity, KeptIdentityTags};
use octo_media::tags::tag_writer_extras::{self as extras, TagFields};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::library_action_journal::LibraryActionEntry;
use super::library_action_journal::{LibraryActionJournal, LibraryActionState};
use super::library_action_quarantine::LibraryActionQuarantine;
use super::library_identity_sweep::stamp_of;
use super::library_tag_edits::{
    self as tag_edits, TagEditEntry, TagEditJournal, TagEditResult, TagValues, song_tag_fields as fields,
    tag_edit_kinds,
};
use super::library_write_gate::LibraryWriteGate;
use super::navidrome_song_path_resolver::{
    NavidromeSongPathResolver, PathSource, ResolvedSongFile, get_full_path, is_inside,
};
use super::upgrade_queue::AudioSummary;
use crate::services::common::base_download_service::BaseDownloadService;
use crate::services::cover_art::{AlbumCoverQuery, FoundCover, IAlbumCoverFinder};
use crate::services::fingerprint::DownloadVerificationService;
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::RejectedPeerRegistry;

/// How one edit from an app went. State is one of the libraryAction states.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryEditOutcome {
    pub state: String,
    pub detail: Option<String>,
    pub before: Option<TagValues>,
    pub after: Option<TagValues>,
}

impl LibraryEditOutcome {
    pub fn new(state: &str, detail: impl Into<String>) -> Self {
        LibraryEditOutcome {
            state: state.to_string(),
            detail: Some(detail.into()),
            before: None,
            after: None,
        }
    }

    fn with(mut self, result: &TagEditResult) -> Self {
        self.before = Some(result.before.clone());
        self.after = Some(result.after.clone());
        self
    }
}

/// What a lookup found for one song: the file's tags now and what the lookup would write.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SongLookup {
    pub state: String,
    pub detail: Option<String>,
    pub current: TagValues,
    pub suggested: TagValues,
    pub confidence: Option<String>,
    pub source: Option<String>,
    pub release: Option<String>,
}

impl SongLookup {
    /// A lookup that found nothing to say, with empty tag sets.
    pub fn said(state: &str, detail: impl Into<String>) -> Self {
        SongLookup {
            state: state.to_string(),
            detail: Some(detail.into()),
            ..SongLookup::default()
        }
    }
}

/// A song taken out of the library that is still in the server's trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedSong {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub username: String,
    pub removed_utc: DateTime<Utc>,
    pub gone_utc: Option<DateTime<Utc>>,
    /// The removal's entry in the library actions history, which Allow downloading again takes.
    pub key: Option<String>,
    /// Whether this removal keeps the song from being downloaded again.
    pub blocks_downloads: bool,
    /// The song's own files (its lyrics) in the trash with it, by name.
    pub sidecars: Vec<String>,
    /// For a file another one replaced, rather than one removed: both files described.
    pub replaced: Option<ReplacedFiles>,
}

/// A replaced file in the trash and the one in the library in its place, each described from
/// its own header ("FLAC 16-bit 44.1 kHz").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplacedFiles {
    pub was: Option<String>,
    pub now: Option<String>,
}

/// What a Put back answers when the file another one replaced was not kept.
pub const NOT_KEPT_TEXT: &str = "Your old copy was not kept on this server, so there is nothing to put back.";

/// The services the edits use, each optional.
#[derive(Default, Clone)]
pub struct LibraryEditServices {
    /// `ILocalLibraryService`: the outside-song links a restore puts back.
    pub library: Option<Arc<dyn ILocalLibraryService>>,
    /// `ReleaseIdentifier`: the lookup.
    pub identifier: Option<Arc<ReleaseIdentifier>>,
    /// `DownloadVerificationService`: the lookup's fingerprint.
    pub verification: Option<Arc<DownloadVerificationService>>,
    /// `IAlbumCoverFinder`: the cover action.
    pub cover_finder: Option<Arc<dyn IAlbumCoverFinder>>,
    /// The music folder's one-writer gate: each change takes a share, so it waits while a job
    /// rewrites the folder.
    pub write_gate: Option<LibraryWriteGate>,
    /// `RejectedPeerRegistry`: the Soulseek file a Wrong song denied, lifted when it is put back.
    pub rejected_peers: Option<Arc<RejectedPeerRegistry>>,
    /// `NoticeQueue`: the library sweep's note of a replacement, reworded when it is put back.
    pub notices: Option<Arc<super::NoticeQueue>>,
}

/// A file this service wrote, by Navidrome id: the path and what the file was right after.
#[derive(Clone)]
struct Written {
    path: String,
    size: u64,
    write_ticks: i64,
    at: Instant,
    file: ResolvedSongFile,
}

/// What the apps' Library health changes in a song's file, other than removing it: tags written
/// in place, an album joined to another, a cover added where there was none, a lookup of the
/// tags a download would get, and the undo of each, plus putting a removed song back.
///
/// The same gates as every library action (switched on, the caller on the allowed list, dry run
/// rehearses), and one more the controller checks: the caller is a Navidrome admin. Every file is
/// one the resolver proved, or one this service itself wrote a moment ago (Navidrome reports a
/// file's old size until it scans, and a second edit should not have to wait for that).
pub struct LibraryEditService {
    resolver: Arc<NavidromeSongPathResolver>,
    quarantine: Arc<LibraryActionQuarantine>,
    actions: Arc<LibraryActionJournal>,
    journal: Arc<TagEditJournal>,
    rescan: Arc<LibraryRescan>,
    /// The library action settings, read at use.
    settings: Arc<SettingsStore>,
    services: LibraryEditServices,

    // The files being changed right now, so two edits of one file never interleave.
    busy: Mutex<HashSet<String>>,

    // Files this service wrote, by Navidrome id.
    written: Mutex<HashMap<String, Written>>,

    // Covers found lately, by album, so the songs of one album cost one lookup.
    covers: Mutex<HashMap<String, (Option<FoundCover>, Instant)>>,
}

pub const BUSY_TEXT: &str = "Another change to this song is still running, so this one was skipped.";
pub const CHANGED_SINCE_TEXT: &str =
    "The file has changed since it was checked, so it was left as it is. Check it again first.";
const UNRESOLVED_TEXT: &str = "Could not work out which file this is, so nothing was touched.";
/// What the journal key of a put back's own entry carries.
const PUT_BACK_KEY: &str = "putback:";

impl LibraryEditService {
    const COVER_MEMORY: Duration = Duration::from_secs(15 * 60);
    const WRITTEN_MEMORY: Duration = Duration::from_secs(30 * 60);

    pub fn new(
        resolver: Arc<NavidromeSongPathResolver>,
        quarantine: Arc<LibraryActionQuarantine>,
        actions: Arc<LibraryActionJournal>,
        journal: Arc<TagEditJournal>,
        rescan: Arc<LibraryRescan>,
        settings: Arc<SettingsStore>,
        services: LibraryEditServices,
    ) -> Self {
        LibraryEditService {
            resolver,
            quarantine,
            actions,
            journal,
            rescan,
            settings,
            services,
            busy: Mutex::new(HashSet::new()),
            written: Mutex::new(HashMap::new()),
            covers: Mutex::new(HashMap::new()),
        }
    }

    /// Whether an album cover finder is registered (getLibraryActions offers cover only then).
    pub fn has_cover_finder(&self) -> bool {
        self.services.cover_finder.is_some()
    }

    /// Why this caller may not change library files, or None when they may.
    pub fn refusal(&self, username: Option<&str>, admin: bool) -> Option<String> {
        let settings = self.settings.current().library_actions.clone();
        let username = username.filter(|name| !is_blank(name));
        let Some(username) = username else {
            return Some(
                "Sign in with a username to change songs; an API key alone does not say who is asking."
                    .into(),
            );
        };
        if !settings.enabled {
            return Some("Library actions are off.".into());
        }
        if !settings.is_allowed(Some(username)) {
            return Some(format!("{username} is not on the library actions allowed list."));
        }
        if !admin {
            return Some("Only a server admin can change the files in the library.".into());
        }
        None
    }

    fn dry_run(&self) -> bool {
        self.settings.current().library_actions.dry_run
    }

    /// Writes these tags into the song's file.
    pub async fn retag(
        self: &Arc<Self>,
        id: &str,
        username: &str,
        changes: IndexMap<String, Option<String>>,
    ) -> LibraryEditOutcome {
        self.retag_unchanged(id, username, changes, None).await
    }

    /// Writes these tags into the song's file, unless its size and last write are no longer
    /// `expected_stamp` (as the library tag check keeps them): values read from a file before it
    /// was replaced are never written over the new one. None writes whatever the file is now.
    pub async fn retag_unchanged(
        self: &Arc<Self>,
        id: &str,
        username: &str,
        changes: IndexMap<String, Option<String>>,
        expected_stamp: Option<&str>,
    ) -> LibraryEditOutcome {
        if let Some(invalid) = tag_edits::invalid(&changes) {
            return LibraryEditOutcome::new("failed", invalid);
        }
        let Some(file) = self.resolve(id).await else {
            return unresolved();
        };
        if self.dry_run() {
            return LibraryEditOutcome::new(
                "rehearsed",
                format!(
                    "Dry run: would change {} of {}",
                    field_words(changes.keys().map(String::as_str)),
                    file.absolute_path
                ),
            );
        }
        let username = username.to_string();
        let expected_stamp = expected_stamp.map(str::to_string);
        self.locked(&file, move |this, file| {
            // Checked inside the lock, so no other edit lands between the check and the write.
            if let Some(expected) = expected_stamp
                && stamp_of(&file.absolute_path).is_none_or(|stamp| stamp != expected)
            {
                return LibraryEditOutcome::new("skipped", CHANGED_SINCE_TEXT);
            }
            let result = tag_edits::write(Path::new(&file.absolute_path), &changes);
            let changed_text = format!("Changed the {}.", field_words(changed(&result)));
            this.finish(
                file,
                &username,
                tag_edit_kinds::RETAG,
                result,
                None,
                &changed_text,
                "Nothing to change: the file already says that.",
            )
        })
        .await
    }

    /// Makes the song part of the album the lead song is on.
    pub async fn join_album(self: &Arc<Self>, id: &str, lead_id: &str, username: &str) -> LibraryEditOutcome {
        if id == lead_id {
            return LibraryEditOutcome::new("skipped", "The song is already on that album.");
        }
        let file = self.resolve(id).await;
        let lead = self.resolve(lead_id).await;
        let (Some(file), Some(lead)) = (file, lead) else {
            return unresolved();
        };
        if self.dry_run() {
            return LibraryEditOutcome::new(
                "rehearsed",
                format!(
                    "Dry run: would put {} on the album of {}",
                    file.absolute_path, lead.absolute_path
                ),
            );
        }
        let username = username.to_string();
        self.locked(&file, move |this, file| {
            let album_before =
                KeptIdentityTags::read(Path::new(&file.absolute_path), file.album_artist.as_deref());
            let result = tag_edits::join_album(
                Path::new(&file.absolute_path),
                Path::new(&lead.absolute_path),
                lead.album_artist.as_deref(),
                file.album_artist.as_deref(),
            );
            this.finish(
                file,
                &username,
                tag_edit_kinds::JOIN_ALBUM,
                result,
                album_before,
                &format!("Moved onto {}.", lead.album),
                "It was on that album already.",
            )
        })
        .await
    }

    /// Finds a cover for the song's album and puts it inside the file, when the file has none.
    pub async fn add_cover(self: &Arc<Self>, id: &str, username: &str, preview: bool) -> LibraryEditOutcome {
        let Some(file) = self.resolve(id).await else {
            return unresolved();
        };
        if tag_edits::has_picture(Path::new(&file.absolute_path)) {
            return LibraryEditOutcome::new(
                "skipped",
                "The song has a picture of its own already, so it was left alone.",
            );
        }
        let Some(found) = self.find_cover(&file).await else {
            return LibraryEditOutcome::new("failed", "No cover for this album could be found.");
        };
        let words = format!("a {} pixel cover from {}", found.side, found.source);
        if preview {
            return LibraryEditOutcome::new("found", format!("Found {words}."));
        }
        if self.dry_run() {
            return LibraryEditOutcome::new(
                "rehearsed",
                format!("Dry run: would put {words} inside {}", file.absolute_path),
            );
        }
        let cover = cover_image::fit_within(&found.bytes, MetadataSettings::EMBEDDED_COVER_SIDE as u32);
        let username = username.to_string();
        self.locked(&file, move |this, file| {
            let result = tag_edits::add_cover(Path::new(&file.absolute_path), &cover);
            this.finish(
                file,
                &username,
                tag_edit_kinds::COVER,
                result,
                None,
                &format!("Added {words}."),
                "The song has a picture already.",
            )
        })
        .await
    }

    /// Puts back the song's last edit made here, when the file has not changed since.
    pub async fn undo(self: &Arc<Self>, id: &str, username: &str) -> LibraryEditOutcome {
        let Some(entry) = self.journal.last(id) else {
            return LibraryEditOutcome::new("skipped", "There is no change to this song to undo.");
        };
        let file = self.resolve(id).await;
        let path = file
            .as_ref()
            .map_or_else(|| entry.path.clone(), |file| file.absolute_path.clone());
        let metadata = std::fs::metadata(&path).ok().filter(|m| m.is_file());
        let Some(metadata) = metadata.filter(|_| {
            same_path(&path, &entry.path) && is_inside(&get_full_path(&path), &self.resolver.music_root())
        }) else {
            return unresolved();
        };
        if metadata.len() as i64 != entry.size_after || last_write_ticks(&path) != entry.write_ticks_after {
            return LibraryEditOutcome::new("failed", "The file has changed since, so it was left as it is.");
        }
        if self.dry_run() {
            return LibraryEditOutcome::new(
                "rehearsed",
                format!("Dry run: would undo the last change to {path}"),
            );
        }
        let target = file.unwrap_or_else(|| ResolvedSongFile {
            navidrome_id: id.to_string(),
            absolute_path: path.clone(),
            size_bytes: metadata.len() as i64,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            suffix: String::new(),
            duration_seconds: None,
            source: PathSource::LocalMappings,
            album_artist: None,
        });
        let username = username.to_string();
        self.locked(&target, move |this, target| {
            let at = Path::new(&path);
            let result = match entry.kind.as_str() {
                tag_edit_kinds::COVER => tag_edits::remove_cover(at),
                tag_edit_kinds::ADVISORY => tag_edits::set_advisory(
                    at,
                    entry
                        .before
                        .get(tag_edit_kinds::ADVISORY)
                        .cloned()
                        .flatten()
                        .and_then(|advisory| advisory.trim().parse::<i32>().ok()),
                ),
                tag_edit_kinds::JOIN_ALBUM if entry.album_before.is_some() => tag_edits::restore_album(
                    at,
                    entry.album_before.as_ref().expect("checked just above"),
                    entry.before.get(fields::YEAR).cloned().flatten().as_deref(),
                ),
                _ => {
                    let back: IndexMap<String, Option<String>> = entry
                        .before
                        .iter()
                        .filter(|(key, value)| entry.after.get(*key).cloned().flatten() != **value)
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    tag_edits::write(at, &back)
                }
            };
            if let Some(error) = &result.error {
                return LibraryEditOutcome::new("failed", format!("Could not undo it: {error}."));
            }
            this.journal.mark_undone(&entry.id, true);
            this.remember(target);
            this.rescan.soon();
            info!("{username} undid the {} of {path} from an app", entry.kind);
            LibraryEditOutcome::new("applied", "Put back as it was.").with(&result)
        })
        .await
    }

    /// Puts a removed song back where it was, from the server's trash, or the file another one
    /// replaced back in that one's place.
    pub async fn restore(&self, id: &str, username: &str) -> LibraryEditOutcome {
        let entry = self.actions.recent(usize::MAX).into_iter().find(|e| {
            e.navidrome_id == id
                && !e.dry_run
                && e.state == LibraryActionState::Applied
                && ((e.action == LibraryAction::Delete && e.quarantine_path.is_some()) || is_replacement(e))
        });
        let Some(entry) = entry else {
            return LibraryEditOutcome::new("skipped", "This song is not in the server's trash.");
        };
        if entry.action != LibraryAction::Delete {
            return self.put_back_replaced(&entry, username).await;
        }
        let trashed = entry.quarantine_path.clone().unwrap_or_default();
        if !file_exists(&trashed) {
            return LibraryEditOutcome::new("failed", "The file is no longer in the server's trash.");
        }
        if self.dry_run() {
            return LibraryEditOutcome::new(
                "rehearsed",
                format!(
                    "Dry run: would put back {}",
                    entry.source_path.as_deref().unwrap_or("")
                ),
            );
        }
        let restored = self
            .quarantine
            .restore(&trashed, Some(&self.resolver.music_root()));
        if !restored.moved {
            return LibraryEditOutcome::new(
                "failed",
                format!(
                    "Could not put it back: {}.",
                    restored.error.as_deref().unwrap_or("")
                ),
            );
        }
        let put_back = restored.quarantine_path.clone().unwrap_or_default();
        // The outside songs Octo knew this file as point at it again, as before it was removed.
        if let Some(mappings) = restored.mappings.as_ref().filter(|mappings| !mappings.is_empty())
            && let Some(library) = &self.services.library
            && let Err(e) = library.restore_mappings(mappings, &put_back).await
        {
            warn!("The outside songs of {put_back} could not be linked to it again: {e}");
        }
        self.actions.complete(
            &entry.key,
            LibraryActionState::Restored,
            Some(&format!("Put back by {username}.")),
            None,
            None,
            None,
        );
        self.actions.flush();
        self.rescan.soon();
        info!("{username} put back {put_back} from the trash");
        LibraryEditOutcome::new("applied", "Put back in your library.")
    }

    /// Swaps the file a replacement took the place of back in: the file in the library now goes
    /// to the trash (so it can be swapped in again the same way), the old one back in the
    /// library, carrying the identity the song has now so Navidrome keeps it as the same song,
    /// with its plays, favorites, ratings and playlist places. When the old file lands at
    /// another path (another kind of file), its own names go back once Navidrome shows the
    /// song there; at the same path they never left it.
    async fn put_back_replaced(&self, entry: &LibraryActionEntry, username: &str) -> LibraryEditOutcome {
        let old = entry.quarantine_path.clone().unwrap_or_default();
        let now = entry.revealed_path.clone().unwrap_or_default();
        if !file_exists(&old) {
            return if self.settings.current().library_actions.keep_replaced_originals {
                LibraryEditOutcome::new("failed", "The old file is no longer in the server's trash.")
            } else {
                LibraryEditOutcome::new("skipped", NOT_KEPT_TEXT)
            };
        }
        if !file_exists(&now) {
            return LibraryEditOutcome::new(
                "failed",
                "The file in your library now is missing, so nothing was swapped.",
            );
        }
        if self.dry_run() {
            return LibraryEditOutcome::new(
                "rehearsed",
                format!("Dry run: would put back {old} in place of {now}"),
            );
        }
        let music_root = self.resolver.music_root();
        let current = KeptIdentityTags::read(Path::new(&now), None);
        let own = KeptIdentityTags::read(Path::new(&old), None);
        let renamed = matches!((&current, &own), (Some(current), Some(own))
            if KeptIdentityTags::pid_inputs(current) != KeptIdentityTags::pid_inputs(own));
        if renamed
            && let Some(current) = &current
            && let Err(e) = KeptIdentityTags::apply(Path::new(&old), current)
        {
            return LibraryEditOutcome::new("failed", format!("Could not get the old file ready ({e})."));
        }
        let size = std::fs::metadata(&now).map(|m| m.len() as i64).unwrap_or(0);
        let out = self.quarantine.move_file(
            &ResolvedSongFile {
                navidrome_id: entry.navidrome_id.clone(),
                absolute_path: now.clone(),
                size_bytes: size,
                title: entry.title.clone(),
                artist: entry.artist.clone(),
                album: entry.album.clone(),
                suffix: String::new(),
                duration_seconds: None,
                source: entry.resolution.unwrap_or(PathSource::NativeApi),
                album_artist: None,
            },
            &music_root,
            LibraryAction::BetterQuality,
            username,
        );
        if !out.moved {
            if renamed && let Some(own) = &own {
                let _ = KeptIdentityTags::apply(Path::new(&old), own);
            }
            return LibraryEditOutcome::new(
                "failed",
                format!(
                    "Could not move the file in your library now aside: {}.",
                    out.error.as_deref().unwrap_or("")
                ),
            );
        }
        let newer = out.quarantine_path.clone().unwrap_or_default();
        let back = self.quarantine.restore(&old, Some(&music_root));
        if !back.moved {
            // The library is never left without the song: the newer file goes back.
            let undone = self.quarantine.restore(&newer, Some(&music_root));
            if renamed && let Some(own) = &own {
                let _ = KeptIdentityTags::apply(Path::new(&old), own);
            }
            return LibraryEditOutcome::new(
                "failed",
                format!(
                    "Could not put the old file back: {}.{}",
                    back.error.as_deref().unwrap_or(""),
                    if undone.moved {
                        " The song is as it was."
                    } else {
                        ""
                    }
                ),
            );
        }
        let restored = back.quarantine_path.clone().unwrap_or_default();
        // The outside songs follow their files: the newer one's go to the trash with it, so
        // swapping it in again gives them back, and the old file's come back to it, a file kept
        // as heard still kept as heard.
        if let Some(library) = &self.services.library {
            match library.forget_mapping(&now).await {
                Ok(newer_songs) => self.quarantine.record_mappings(&newer, &newer_songs),
                Err(e) => warn!("The outside songs of {now} could not be moved aside with it: {e}"),
            }
            if let Some(mappings) = back.mappings.as_ref().filter(|mappings| !mappings.is_empty())
                && let Err(e) = library.restore_mappings(mappings, &restored).await
            {
                warn!("The outside songs of {restored} could not be linked to it again: {e}");
            }
        }
        self.actions.complete(
            &entry.key,
            LibraryActionState::Restored,
            Some(&format!("Put back by {username}.")),
            None,
            None,
            None,
        );
        self.actions.record(LibraryActionEntry {
            key: LibraryActionJournal::make_key(
                LibraryAction::BetterQuality,
                &entry.navidrome_id,
                &format!("{PUT_BACK_KEY}{}", dotnet_ticks(Utc::now())),
            ),
            source_path: Some(now.clone()),
            quarantine_path: Some(newer.clone()),
            revealed_path: Some(restored.clone()),
            state: LibraryActionState::Applied,
            detail: Some(format!(
                "Put back by {username}: the old copy took the newer one's place."
            )),
            username: username.to_string(),
            at_utc: Utc::now(),
            history_kept: None,
            // The denial is lifted below, once; swapping back again denies nothing.
            denied_source: None,
            ..entry.clone()
        });
        self.actions.flush();
        self.undo_replacement_marks(entry);
        self.rescan.soon();
        info!("{username} put back {restored} in place of {now}, which went to {newer}");
        if renamed && let Some(own) = own {
            if restored == now {
                // The same path keeps the same song, so its own names go back at once.
                if let Err(e) = KeptIdentityTags::apply(Path::new(&restored), &own) {
                    warn!("The old names of {restored} could not be written back: {e}");
                }
                self.rescan.soon();
            } else {
                let (resolver, rescan, id) = (
                    self.resolver.clone(),
                    self.rescan.clone(),
                    entry.navidrome_id.clone(),
                );
                tokio::spawn(async move {
                    for _ in 0..120 {
                        if resolver
                            .resolve(&id)
                            .await
                            .is_some_and(|song| song.absolute_path == restored)
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    if let Err(e) = KeptIdentityTags::apply(Path::new(&restored), &own) {
                        warn!("The old names of {restored} could not be written back: {e}");
                    }
                    rescan.soon();
                });
            }
        }
        LibraryEditOutcome::new(
            "applied",
            "Put back your old copy. The one that replaced it is in Recently removed.",
        )
    }

    /// What a Wrong song or Wrong version left besides the swap, taken back once its original is
    /// in place again: the Soulseek file it denied as the wrong song (that one denial and no
    /// other), and the library sweep's note, which then says the original was put back. A swap
    /// back of a put back lifts no denial; it brings the replacement in again, so the note reads
    /// as replaced again, and whichever way the files last went, the note says so.
    fn undo_replacement_marks(&self, entry: &LibraryActionEntry) {
        if !matches!(
            entry.action,
            LibraryAction::WrongSong | LibraryAction::WrongVersion
        ) {
            return;
        }
        let swapped_back = entry.key.contains(PUT_BACK_KEY);
        if !swapped_back
            && let (Some(denied), Some(rejected)) = (&entry.denied_source, &self.services.rejected_peers)
        {
            rejected.allow(&denied.username, &denied.filename);
        }
        if let Some(notices) = &self.services.notices {
            let reworded = if swapped_back {
                notices.mark_replaced_again(&entry.navidrome_id) || notices.mark_put_back(&entry.navidrome_id)
            } else {
                notices.mark_put_back(&entry.navidrome_id)
            };
            if reworded {
                notices.flush();
            }
        }
    }

    /// The songs removed from the apps or the playlists that are still in the trash, newest first.
    pub fn trash(&self) -> Vec<TrashedSong> {
        let days = self
            .settings
            .current()
            .library_actions
            .effective_quarantine_retention_days();
        let mut seen: HashSet<String> = HashSet::new();
        self.actions
            .recent(usize::MAX)
            .into_iter()
            .filter(|e| {
                (e.action == LibraryAction::Delete || is_replacement(e))
                    && !e.dry_run
                    && e.state == LibraryActionState::Applied
                    && e.quarantine_path.as_deref().is_some_and(file_exists)
            })
            .filter(|e| seen.insert(e.navidrome_id.clone()))
            .take(500)
            .map(|e| {
                let trashed = e.quarantine_path.clone().unwrap_or_default();
                TrashedSong {
                    id: e.navidrome_id.clone(),
                    title: e.title.clone(),
                    artist: e.artist.clone(),
                    album: e.album.clone(),
                    username: e.username.clone(),
                    removed_utc: e.at_utc,
                    // The sweep goes by the day folder, so the file goes at the start of that day
                    // plus the days kept.
                    gone_utc: (days > 0).then(|| {
                        e.at_utc
                            .date_naive()
                            .and_hms_opt(0, 0, 0)
                            .expect("midnight exists")
                            .and_utc()
                            + TimeDelta::days(i64::from(days) + 1)
                    }),
                    key: Some(e.key.clone()),
                    blocks_downloads: e.blocks_downloads(),
                    sidecars: self.quarantine.sidecar_names(&trashed),
                    replaced: is_replacement(&e).then(|| ReplacedFiles {
                        was: AudioSummary::describe(Some(&trashed)).0,
                        now: AudioSummary::describe(e.revealed_path.as_deref()).0,
                    }),
                }
            })
            .collect()
    }

    /// The tags a download of this song would get, found the way a download finds them (its
    /// fingerprint when that is set up, then the catalogs), beside what the file says now.
    /// Writes nothing.
    pub async fn lookup(&self, id: &str) -> SongLookup {
        let Some(file) = self.resolve(id).await else {
            return SongLookup::said("unresolved", UNRESOLVED_TEXT);
        };
        let Some(identifier) = &self.services.identifier else {
            return SongLookup::said("failed", "This server cannot look songs up.");
        };

        let current = match tag_edits::read(Path::new(&file.absolute_path)) {
            Ok(current) => current,
            Err(e) => {
                return SongLookup::said("failed", format!("Could not read the song's tags ({e})."));
            }
        };

        let mut song = Song {
            artist: current
                .get(fields::ARTIST)
                .cloned()
                .flatten()
                .unwrap_or_else(|| file.artist.clone()),
            title: current
                .get(fields::TITLE)
                .cloned()
                .flatten()
                .unwrap_or_else(|| file.title.clone()),
            album: String::new(),
            duration: file.duration_seconds,
            ..Song::default()
        };
        if let Some(verification) = &self.services.verification
            && verification.is_fingerprinting_enabled()
        {
            let verdict = FingerprintVerifier::verify(
                verification.as_ref(),
                &file.absolute_path,
                &song.artist,
                &song.title,
            )
            .await;
            song.verification = Some(Box::new(verdict));
        }
        let request = ReleaseIdentifier::request_for(&song, &song.artist, &song.title, None, None);
        // Never canceled: the handler's future is dropped when the client goes, which stops
        // the lookup.
        let token = CancellationToken::new();
        let plan = match identifier
            .identify::<()>(
                &song,
                request,
                &file.absolute_path,
                true,
                None::<&AlbumTagContext>,
                &token,
            )
            .await
        {
            Ok(plan) => plan,
            Err(_) => {
                info!("Lookup of {} failed: cancelled", file.absolute_path);
                return SongLookup {
                    state: "failed".into(),
                    detail: Some("The lookup did not work (The operation was canceled.).".into()),
                    current,
                    ..SongLookup::default()
                };
            }
        };
        let mut plan = plan;
        plan.apply_to(&mut song);
        BaseDownloadService::fill_blanks_from_catalog(&mut song, plan.catalog_best.as_ref());
        let chosen = plan.chosen.as_ref().map(|chosen| &chosen.candidate);
        SongLookup {
            state: "found".into(),
            detail: None,
            current,
            suggested: suggested(&song),
            confidence: Some(plan.confidence.name().to_string()),
            source: chosen.map(|chosen| chosen.source.name().to_string()).or_else(|| {
                plan.catalog_best.as_ref().map(|best| {
                    // An id above Deezer's range is Apple's: Apple's catalog answered.
                    let apple = best
                        .track_id
                        .as_deref()
                        .is_some_and(octo_core::metadata::apple_as_deezer::is_apple_id);
                    if apple { "Apple Music" } else { "Deezer" }.to_string()
                })
            }),
            release: chosen.map(TagPlan::describe),
        }
    }

    async fn find_cover(&self, file: &ResolvedSongFile) -> Option<FoundCover> {
        let finder = self.services.cover_finder.as_ref()?;
        let tags = tag_edits::read(Path::new(&file.absolute_path)).ok()?;
        let tag = |name: &str| tags.get(name).cloned().flatten();
        let artist = tag(fields::ALBUM_ARTIST)
            .or_else(|| file.album_artist.clone())
            .or_else(|| tag(fields::ARTIST))
            .unwrap_or_else(|| file.artist.clone());
        let album = tag(fields::ALBUM).unwrap_or_else(|| file.album.clone());
        let key = SongIdentity::match_key(&artist, &album);
        if let Some((cover, at)) = self.covers.lock().get(&key)
            && at.elapsed() < Self::COVER_MEMORY
        {
            return cover.clone();
        }

        // The names alone still find most covers when the file cannot be read again.
        let (release_id, barcode) = match TagFile::open(&file.absolute_path) {
            Ok(tag_file) => (
                extras::read_text(&tag_file, TagFields::ALBUM_ID),
                extras::read_text(&tag_file, TagFields::BARCODE),
            ),
            Err(_) => (None, None),
        };

        let query = AlbumCoverQuery {
            artist: artist.clone(),
            album: Some(album.clone()),
            title: Some(tag(fields::TITLE).unwrap_or_else(|| file.title.clone())),
            music_brainz_release_id: release_id,
            music_brainz_release_group_id: None,
            barcode,
        };
        let found = finder.find(&query).await;
        let mut covers = self.covers.lock();
        covers.retain(|_, (_, at)| at.elapsed() < Self::COVER_MEMORY);
        covers.insert(key, (found.clone(), Instant::now()));
        found
    }

    /// The verified file, or the one this service wrote lately when Navidrome still reports its
    /// size from before the write.
    async fn resolve(&self, id: &str) -> Option<ResolvedSongFile> {
        if let Some(resolved) = self.resolver.resolve(id).await {
            return Some(resolved);
        }
        let written = self.written.lock().get(id).cloned()?;
        if written.at.elapsed() > Self::WRITTEN_MEMORY {
            return None;
        }
        let metadata = std::fs::metadata(&written.path).ok().filter(|m| m.is_file())?;
        if metadata.len() != written.size || last_write_ticks(&written.path) != written.write_ticks {
            return None;
        }
        if !is_inside(&get_full_path(&written.path), &self.resolver.music_root()) {
            return None;
        }
        Some(ResolvedSongFile {
            size_bytes: metadata.len() as i64,
            ..written.file
        })
    }

    fn remember(&self, file: &ResolvedSongFile) {
        if let Some(metadata) = std::fs::metadata(&file.absolute_path)
            .ok()
            .filter(|m| m.is_file())
        {
            self.written.lock().insert(
                file.navidrome_id.clone(),
                Written {
                    path: file.absolute_path.clone(),
                    size: metadata.len(),
                    write_ticks: last_write_ticks(&file.absolute_path),
                    at: Instant::now(),
                    file: file.clone(),
                },
            );
        }
    }

    /// One change to one file at a time; the file work runs off the async threads.
    async fn locked<F>(self: &Arc<Self>, file: &ResolvedSongFile, work: F) -> LibraryEditOutcome
    where
        F: FnOnce(&LibraryEditService, &ResolvedSongFile) -> LibraryEditOutcome + Send + 'static,
    {
        let key = get_full_path(&file.absolute_path);
        if !self.busy.lock().insert(key.clone()) {
            return LibraryEditOutcome::new("skipped", BUSY_TEXT);
        }
        let share = match &self.services.write_gate {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        let this = Arc::clone(self);
        let file = file.clone();
        let done = tokio::task::spawn_blocking(move || work(&this, &file)).await;
        drop(share);
        self.busy.lock().remove(&key);
        done.unwrap_or_else(|e| LibraryEditOutcome::new("failed", format!("Could not change the song: {e}.")))
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        file: &ResolvedSongFile,
        username: &str,
        kind: &str,
        result: TagEditResult,
        album_before: Option<KeptIdentity>,
        changed_text: &str,
        same_text: &str,
    ) -> LibraryEditOutcome {
        if let Some(error) = &result.error {
            return LibraryEditOutcome::new("failed", format!("Could not change the song: {error}."));
        }
        if !result.changed {
            return LibraryEditOutcome::new("skipped", same_text).with(&result);
        }
        let size = std::fs::metadata(&file.absolute_path).map_or(0, |m| m.len() as i64);
        let kept = self.journal.record(
            TagEditEntry {
                id: uuid::Uuid::new_v4().simple().to_string(),
                navidrome_id: file.navidrome_id.clone(),
                path: file.absolute_path.clone(),
                kind: kind.to_string(),
                username: username.to_string(),
                before: result.before.clone(),
                after: result.after.clone(),
                size_after: size,
                write_ticks_after: last_write_ticks(&file.absolute_path),
                at_utc: Utc::now(),
                album_before,
                undone: false,
                run_id: None,
            },
            true,
        );
        if !kept {
            warn!(
                "The {kind} of {} was written but could not be kept for an undo",
                file.absolute_path
            );
        }
        self.remember(file);
        self.rescan.soon();
        info!(
            "{username} made a {kind} of {} from an app: {changed_text}",
            file.absolute_path
        );
        LibraryEditOutcome::new("applied", changed_text).with(&result)
    }
}

/// The lookup's suggestion in the apps' names, keeping only what it found.
pub fn suggested(song: &Song) -> TagValues {
    fn text(value: Option<&str>) -> Option<String> {
        value
            .filter(|value| !is_blank(value))
            .map(|value| value.trim().to_string())
    }
    fn number(value: Option<i32>) -> Option<String> {
        value.filter(|value| *value > 0).map(|value| value.to_string())
    }
    let found: [(&str, Option<String>); 9] = [
        (fields::TITLE, text(Some(&song.title))),
        (fields::ARTIST, text(Some(&song.artist))),
        (fields::ALBUM, text(Some(&song.album))),
        (fields::ALBUM_ARTIST, text(song.album_artist.as_deref())),
        (fields::YEAR, number(song.year)),
        (fields::GENRE, text(song.genre.as_deref())),
        (fields::TRACK, number(song.track)),
        (fields::DISC, number(song.disc_number)),
        (
            fields::ISRC,
            song.isrc.as_deref().and_then(SongIdentity::normalize_isrc),
        ),
    ];
    found
        .into_iter()
        .filter(|(_, value)| value.is_some())
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

fn unresolved() -> LibraryEditOutcome {
    LibraryEditOutcome::new("unresolved", UNRESOLVED_TEXT)
}

/// The fields an edit changed, by name.
fn changed(result: &TagEditResult) -> impl Iterator<Item = &str> {
    result
        .after
        .iter()
        .filter(|(key, value)| result.before.get(*key).cloned().flatten() != **value)
        .map(|(key, _)| key.as_str())
}

/// Field names in a sentence: "tags", "genre", "genre and track number", "a, b and c".
fn field_words<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut words: Vec<&str> = Vec::new();
    for name in names {
        let word = tag_edits::words(fields::canonical(name).unwrap_or(name));
        if !words.contains(&word) {
            words.push(word);
        }
    }
    match words.len() {
        0 => "tags".to_string(),
        1 => words[0].to_string(),
        n => format!("{} and {}", words[..n - 1].join(", "), words[n - 1]),
    }
}

fn same_path(a: &str, b: &str) -> bool {
    get_full_path(a) == get_full_path(b)
}

fn file_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// The file's last write time in 100 ns ticks since 0001-01-01, or the ticks of 1601-01-01 for a
/// missing file.
fn last_write_ticks(path: &str) -> i64 {
    const MISSING: i64 = 504_911_232_000_000_000;
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|modified| dotnet_ticks(DateTime::<Utc>::from(modified)))
        .unwrap_or(MISSING)
}

/// One Navidrome scan a little after a run of changes, rather than one per change. A normal
/// scan, never a full one: over the cloud mount a full scan reads every file again (over fifteen
/// minutes for 2,500 songs), and a normal one finds a changed or missing file by its folder.
pub struct LibraryRescan {
    library: Option<Arc<dyn ILocalLibraryService>>,
    delay: Mutex<Duration>,
    pending: Arc<AtomicBool>,
    // While held (a run of fixes from the dashboard), a change only marks a scan as wanted; the
    // last release asks for that one scan.
    holds: AtomicI32,
    held_asks: AtomicI32,
    /// Counted for tests.
    asked: AtomicUsize,
    /// Counted for tests: scans actually asked for, held asks folded into one.
    scheduled: AtomicUsize,
}

impl LibraryRescan {
    /// Over the library service that asks Navidrome to scan; None asks nobody (the tests').
    pub fn new(library: Option<Arc<dyn ILocalLibraryService>>) -> Self {
        LibraryRescan {
            library,
            delay: Mutex::new(Duration::from_secs(8)),
            pending: Arc::new(AtomicBool::new(false)),
            holds: AtomicI32::new(0),
            held_asks: AtomicI32::new(0),
            asked: AtomicUsize::new(0),
            scheduled: AtomicUsize::new(0),
        }
    }

    /// How long a change waits before its scan (eight seconds; the tests shorten it).
    pub fn set_delay(&self, delay: Duration) {
        *self.delay.lock() = delay;
    }

    /// How many changes asked for a scan.
    pub fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }

    /// How many scans were scheduled, held asks folded into one.
    pub fn scheduled(&self) -> usize {
        self.scheduled.load(Ordering::SeqCst)
    }

    /// Holds scans back until the returned handle is dropped, then asks for one if any change
    /// came in meanwhile: a run of a hundred fixes is one scan at its end.
    pub fn hold(self: &Arc<Self>) -> RescanHold {
        self.holds.fetch_add(1, Ordering::SeqCst);
        RescanHold {
            rescan: Arc::clone(self),
            done: AtomicBool::new(false),
        }
    }

    pub fn soon(&self) {
        self.asked.fetch_add(1, Ordering::SeqCst);
        if self.holds.load(Ordering::SeqCst) > 0 {
            self.held_asks.store(1, Ordering::SeqCst);
            // The hold may have ended in between; then nobody else will ask.
            if self.holds.load(Ordering::SeqCst) > 0 || self.held_asks.swap(0, Ordering::SeqCst) == 0 {
                return;
            }
        }
        self.schedule();
    }

    fn schedule(&self) {
        self.scheduled.fetch_add(1, Ordering::SeqCst);
        let Some(library) = self.library.clone() else {
            return;
        };
        if self.pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.pending.store(false, Ordering::SeqCst);
            return;
        };
        let delay = *self.delay.lock();
        let pending = Arc::clone(&self.pending);
        runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            pending.store(false, Ordering::SeqCst);
            if !library.trigger_library_scan(true).await {
                debug!("Scan after a library change failed");
            }
        });
    }
}

/// [`LibraryRescan::hold`]'s handle: dropping it lets the held scan go.
pub struct RescanHold {
    rescan: Arc<LibraryRescan>,
    done: AtomicBool,
}

impl Drop for RescanHold {
    fn drop(&mut self) {
        if self.done.swap(true, Ordering::SeqCst) {
            return;
        }
        let rescan = &self.rescan;
        if rescan.holds.fetch_sub(1, Ordering::SeqCst) == 1 && rescan.held_asks.swap(0, Ordering::SeqCst) == 1
        {
            rescan.schedule();
        }
    }
}

/// A Better quality replacement that went in, whose current file is still where it went: what
/// Put back swaps.
/// A Better quality, Wrong song or Wrong version that put another file in the song's place,
/// which is still there.
fn is_replacement(entry: &LibraryActionEntry) -> bool {
    matches!(
        entry.action,
        LibraryAction::BetterQuality | LibraryAction::WrongSong | LibraryAction::WrongVersion
    ) && entry.revealed_path.as_deref().is_some_and(file_exists)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_keeps_only_what_the_lookup_found_in_the_apps_names() {
        let found = suggested(&Song {
            title: "Teardrop".into(),
            artist: "Massive Attack".into(),
            album: "Mezzanine".into(),
            album_artist: Some("Massive Attack".into()),
            year: Some(1998),
            genre: Some("Trip Hop".into()),
            track: Some(3),
            disc_number: Some(1),
            isrc: Some("gb-aaa-98-00003".into()),
            ..Song::default()
        });
        assert_eq!(found.get("year"), Some(&Some("1998".to_string())));
        assert_eq!(found.get("track"), Some(&Some("3".to_string())));
        assert_eq!(found.get("isrc"), Some(&Some("GBAAA9800003".to_string())));
        assert_eq!(found.get("genre"), Some(&Some("Trip Hop".to_string())));

        let thin = suggested(&Song {
            title: "Teardrop".into(),
            artist: "Massive Attack".into(),
            ..Song::default()
        });
        let mut keys: Vec<&str> = thin.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["artist", "title"]);
    }

    #[test]
    fn field_words_read_as_a_sentence() {
        assert_eq!(field_words([].into_iter()), "tags");
        assert_eq!(field_words(["genre"].into_iter()), "genre");
        assert_eq!(
            field_words(["GENRE", "track"].into_iter()),
            "genre and track number"
        );
        assert_eq!(
            field_words(["albumartist", "disc", "isrc", "disc"].into_iter()),
            "album artist, disc number and ISRC"
        );
    }

    #[tokio::test]
    async fn a_hold_folds_every_change_into_one_scan_at_its_end() {
        let rescan = Arc::new(LibraryRescan::new(None));
        {
            let _hold = rescan.hold();
            let _inner = rescan.hold();
            rescan.soon();
            rescan.soon();
            assert_eq!(rescan.scheduled(), 0);
        }
        assert_eq!((rescan.asked(), rescan.scheduled()), (2, 1));

        // Nothing changed while held: nothing to scan.
        drop(rescan.hold());
        assert_eq!(rescan.scheduled(), 1);
        rescan.soon();
        assert_eq!(rescan.scheduled(), 2);
    }
}
