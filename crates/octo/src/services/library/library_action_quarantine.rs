//! The library action trash, and the `<file>.octo-action.json` manifest beside each quarantined
//! file.

use std::io;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use octo_core::common::PathHelper;
use octo_core::json::datetime;
use octo_core::settings::{LibraryAction, SettingsStore};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::ResolvedSongFile;
use super::library_action_journal::action_name;
use super::navidrome_song_path_resolver::{get_full_path, is_inside};
use crate::services::local::LocalSongMapping;
use crate::services::state_file;

/// A move into or out of the trash. A restore answers the outside-song mappings the removal
/// dropped, for the library to have again.
#[derive(Debug, Clone, PartialEq)]
pub struct QuarantineResult {
    pub moved: bool,
    pub quarantine_path: Option<String>,
    pub error: Option<String>,
    pub mappings: Option<Vec<LocalSongMapping>>,
}

impl QuarantineResult {
    fn moved(path: String) -> Self {
        QuarantineResult {
            moved: true,
            quarantine_path: Some(path),
            error: None,
            mappings: None,
        }
    }

    fn refused(error: impl Into<String>) -> Self {
        QuarantineResult {
            moved: false,
            quarantine_path: None,
            error: Some(error.into()),
            mappings: None,
        }
    }
}

/// What one sweep deleted for good: how many files, and the Navidrome ids their notes named.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Swept {
    pub files: i32,
    pub navidrome_ids: Vec<String>,
}

/// What is written next to a quarantined file so a restore works without the journal. Its
/// `Action` is the enum's name. Sidecars are the song's own files that went with it (its
/// lyrics), put back with it; Mappings the outside songs Octo knew the file as, which a removal
/// forgets and a restore puts back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct QuarantineManifest {
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub original_path: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub navidrome_id: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub action: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub username: String,
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub at_utc: DateTime<Utc>,
    /// The song's own files that went to the trash with it (its lyrics), put back with it.
    #[serde(default)]
    pub sidecars: Option<Vec<QuarantinedSidecar>>,
    /// The outside songs Octo knew the file as, which a removal forgets and a restore puts back.
    #[serde(default)]
    pub mappings: Option<Vec<LocalSongMapping>>,
}

/// A file of the song's own that went to the trash with it: where it was, and where it waits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct QuarantinedSidecar {
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub original_path: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub quarantine_path: String,
}

/// Moves a verified file out of the library instead of deleting it.
///
/// Nothing in library actions deletes a file except the retention sweep. These actions delete
/// files Octo did NOT create, on one tap in a music client with no confirmation dialog, and the
/// whole point of the feature is that the user is correcting a mistake, which means they can make
/// one.
///
/// This is where DiscardRejectedDownload's philosophy is reconciled rather than contradicted.
/// That guard refuses to delete anything created before the attempt started, because its input
/// is a GUESS: ResolveLocalPath matches on leaf name and approximate size. Here the user has
/// pointed at a specific track, so a creation-time guard would break the feature's purpose. The
/// invariant underneath still holds: never act on a path you inferred rather than proved. That
/// is discharged by the resolver's byte-size check, which is strictly stronger than
/// DiscardRejectedDownload's own approximate-size matching, plus quarantine instead of delete,
/// which makes getting it wrong recoverable rather than terminal.
pub struct LibraryActionQuarantine {
    /// The library action settings: the folder and the retention are read at use.
    settings: Arc<SettingsStore>,
}

impl LibraryActionQuarantine {
    pub const MANIFEST_SUFFIX: &'static str = ".octo-action.json";

    pub fn new(settings: Arc<SettingsStore>) -> Self {
        LibraryActionQuarantine { settings }
    }

    pub fn root_for(&self, music_root: &str) -> String {
        combine(
            music_root,
            &self
                .settings
                .current()
                .library_actions
                .effective_quarantine_directory(),
        )
    }

    /// Files beside a song that belong to it alone, by its name: the lyrics Octo writes (.lrc,
    /// .txt) and the other lyrics files players read. Never a folder's cover, which the album
    /// shares.
    pub const SIDECAR_EXTENSIONS: [&'static str; 7] =
        [".lrc", ".txt", ".ttml", ".elrc", ".srt", ".yaml", ".yml"];

    const AUDIO_EXTENSIONS: [&'static str; 18] = [
        ".flac", ".mp3", ".m4a", ".aac", ".ogg", ".oga", ".opus", ".wav", ".aiff", ".aif", ".ape", ".wv",
        ".alac", ".wma", ".mp4", ".dsf", ".dff", ".mka",
    ];

    /// The song's own sidecars: files with its name and a lyrics extension. None while another
    /// audio file of the same name stays beside it (a second copy in another format), since the
    /// lyrics are that copy's too.
    pub fn sidecars_of(audio_path: &str) -> Vec<String> {
        let folder = directory_name(audio_path);
        if folder.is_empty() || !Path::new(&folder).is_dir() {
            return Vec::new();
        }
        let stem = file_name_without_extension(audio_path);
        let Ok(entries) = std::fs::read_dir(&folder) else {
            return Vec::new();
        };
        let named: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
            .map(|entry| combine(&folder, &entry.file_name().to_string_lossy()))
            .filter(|path| file_name_without_extension(path) == stem)
            .collect();
        if named.iter().any(|path| {
            path != audio_path
                && Self::AUDIO_EXTENSIONS
                    .iter()
                    .any(|audio| octo_core::common::dotnet::eq_ignore_case(audio, &extension_of(path)))
        }) {
            return Vec::new();
        }
        named
            .into_iter()
            .filter(|path| {
                Self::SIDECAR_EXTENSIONS
                    .iter()
                    .any(|lyrics| octo_core::common::dotnet::eq_ignore_case(lyrics, &extension_of(path)))
            })
            .collect()
    }

    /// Move a verified file into quarantine, preserving its layout underneath so a restore is a
    /// straight copy back. The song's sidecars stay where they are.
    pub fn move_file(
        &self,
        file: &ResolvedSongFile,
        music_root: &str,
        action: LibraryAction,
        username: &str,
    ) -> QuarantineResult {
        self.move_file_with_sidecars(file, music_root, action, username, false)
    }

    /// [`Self::move_file`]; with `with_sidecars`, the song's own lyrics files go with it, named
    /// after its place in the trash; one that cannot move stays, and is logged.
    pub fn move_file_with_sidecars(
        &self,
        file: &ResolvedSongFile,
        music_root: &str,
        action: LibraryAction,
        username: &str,
        with_sidecars: bool,
    ) -> QuarantineResult {
        match self.try_move(file, music_root, action, username, with_sidecars) {
            Ok(result) => result,
            Err(e) => {
                warn!("Library action could not quarantine {}: {e}", file.absolute_path);
                QuarantineResult::refused(e.to_string())
            }
        }
    }

    fn try_move(
        &self,
        file: &ResolvedSongFile,
        music_root: &str,
        action: LibraryAction,
        username: &str,
        with_sidecars: bool,
    ) -> io::Result<QuarantineResult> {
        if let Some(refused) = self.outside(&file.absolute_path, music_root) {
            return Ok(QuarantineResult::refused(refused));
        }
        let relative = get_relative_path(&get_full_path(music_root), &get_full_path(&file.absolute_path));
        // Read before the song moves: with it gone, nothing would tell its lyrics from another
        // copy's.
        let sidecars = if with_sidecars {
            Self::sidecars_of(&file.absolute_path)
        } else {
            Vec::new()
        };

        let destination = combine(
            &combine(
                &self.root_for(music_root),
                &Utc::now().format("%Y-%m-%d").to_string(),
            ),
            &relative,
        );
        if let Some(parent) = Path::new(&destination).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let destination = unique(&destination);

        if let Err(moving) = rename_new(&file.absolute_path, &destination) {
            // A refused permission never falls back to the copy.
            if moving.kind() == io::ErrorKind::PermissionDenied {
                return Err(moving);
            }
            // Across devices the rename fails, so copy, confirm the length, then remove. The
            // length check is what stops a truncated copy from turning into a delete.
            copy_new(&file.absolute_path, &destination)?;
            if std::fs::metadata(&destination)?.len() as i64 != file.size_bytes {
                try_delete(&destination);
                return Ok(QuarantineResult::refused(
                    "the copy did not match the original's size",
                ));
            }
            std::fs::remove_file(&file.absolute_path)?;
        }

        let mut moved = Vec::new();
        for sidecar in sidecars {
            let target = combine(
                &directory_name(&destination),
                &format!(
                    "{}{}",
                    file_name_without_extension(&destination),
                    extension_of(&sidecar)
                ),
            );
            if self.move_sidecar(&sidecar, &target, music_root) {
                moved.push(QuarantinedSidecar {
                    original_path: sidecar,
                    quarantine_path: target,
                });
            }
        }

        self.write_manifest(
            &destination,
            &QuarantineManifest {
                original_path: file.absolute_path.clone(),
                navidrome_id: file.navidrome_id.clone(),
                action: action_name(action).to_string(),
                username: username.to_string(),
                at_utc: Utc::now(),
                sidecars: (!moved.is_empty()).then_some(moved),
                // Filled in by `record_mappings` once the library has forgotten them.
                mappings: None,
            },
        );

        info!(
            "Library action {} by {username}: quarantined {} -> {destination}",
            action_name(action),
            file.absolute_path
        );
        Ok(QuarantineResult::moved(destination))
    }

    /// Why a file may not be moved out of the library, or None when it may. The resolver proved
    /// it already; this is the last word before a move, so it does not trust that: the full path
    /// must sit inside the music root (a "..", another drive or a lookalike folder name such as
    /// "/music-old" all fail), must not already be in the trash, and must be a real file rather
    /// than a link that could point anywhere.
    pub fn outside(&self, path: &str, music_root: &str) -> Option<&'static str> {
        if path.is_empty() {
            return Some("the file's path cannot be read");
        }
        let full = get_full_path(path);
        if !is_inside(&full, music_root) {
            return Some("the file is not under the music root");
        }
        if is_inside(&full, &self.root_for(music_root)) {
            return Some("the file is in the trash already");
        }
        if std::fs::symlink_metadata(&full).is_ok_and(|info| info.file_type().is_symlink()) {
            return Some("the file is a link, not a song file");
        }
        None
    }

    /// Put a quarantined file back where it came from. With `music_root`, only into it, whatever
    /// the manifest beside the file says.
    pub fn restore(&self, quarantine_path: &str, music_root: Option<&str>) -> QuarantineResult {
        let attempt = || -> io::Result<QuarantineResult> {
            if !file_exists(quarantine_path) {
                return Ok(QuarantineResult::refused("the quarantined file is gone"));
            }

            let Some(manifest) = self.read_manifest(quarantine_path) else {
                return Ok(QuarantineResult::refused(
                    "no manifest, so the original path is unknown",
                ));
            };
            if let Some(root) = music_root
                && !is_inside(&get_full_path(&manifest.original_path), root)
            {
                return Ok(QuarantineResult::refused(
                    "its old place is not under the music root",
                ));
            }

            if file_exists(&manifest.original_path) {
                return Ok(QuarantineResult::refused(
                    "something already exists at the original path",
                ));
            }
            // On Windows and over SMB a name in other capitals (or with a character Windows
            // refuses written another way) is the same name: putting the song back beside it
            // would make two of one name.
            if twin_on_the_way(&manifest.original_path) {
                return Ok(QuarantineResult::refused(
                    "something of the same name in other capitals is already at the original path",
                ));
            }
            if let Some(parent) = Path::new(&manifest.original_path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }

            rename_new(quarantine_path, &manifest.original_path)?;
            for sidecar in manifest.sidecars.iter().flatten() {
                // Each one meets the same rule as the song: back only into the music folder.
                if let Some(root) = music_root
                    && !is_inside(&get_full_path(&sidecar.original_path), root)
                {
                    continue;
                }
                if !file_exists(&sidecar.quarantine_path) || file_exists(&sidecar.original_path) {
                    continue;
                }
                if let Err(e) = rename_new(&sidecar.quarantine_path, &sidecar.original_path) {
                    warn!("Library action could not put back {}: {e}", sidecar.original_path);
                }
            }
            try_delete(&format!("{quarantine_path}{}", Self::MANIFEST_SUFFIX));

            info!("Library action restored {}", manifest.original_path);
            Ok(QuarantineResult {
                mappings: manifest.mappings,
                ..QuarantineResult::moved(manifest.original_path)
            })
        };
        attempt().unwrap_or_else(|e| {
            warn!("Library action could not restore {quarantine_path}: {e}");
            QuarantineResult::refused(e.to_string())
        })
    }

    /// The only code in this feature that really deletes, and it goes on age rather than on
    /// which action produced the file. Retention 0 means never sweep. Answers how many files went.
    pub fn sweep(&self, music_root: &str) -> i32 {
        self.sweep_gone(music_root).files
    }

    /// [`Self::sweep`], also answering the Navidrome ids the deleted files' notes named: songs
    /// that can no longer be put back, which Navidrome still keeps as missing.
    pub fn sweep_gone(&self, music_root: &str) -> Swept {
        let days = self
            .settings
            .current()
            .library_actions
            .effective_quarantine_retention_days();
        if days <= 0 {
            return Swept::default();
        }

        let root = self.root_for(music_root);
        if !Path::new(&root).is_dir() {
            return Swept::default();
        }

        let cutoff = Utc::now() - TimeDelta::days(i64::from(days));
        let mut swept = Swept::default();
        let mut attempt = || -> io::Result<()> {
            for dated in std::fs::read_dir(&root)? {
                let dated = dated?;
                if !dated.file_type()?.is_dir() {
                    continue;
                }
                let name = dated.file_name().to_string_lossy().into_owned();
                let Some(day) = parse_day(&name) else {
                    continue;
                };
                if day >= cutoff {
                    continue;
                }

                let count = count_files(&dated.path())?;
                let mut ids = Vec::new();
                noted_ids(&dated.path(), &mut ids);
                std::fs::remove_dir_all(dated.path())?;
                swept.files += count;
                for id in ids {
                    if !swept.navidrome_ids.contains(&id) {
                        swept.navidrome_ids.push(id);
                    }
                }
                info!("Library action quarantine swept {name} ({count} file(s))");
            }
            Ok(())
        };
        if let Err(e) = attempt() {
            warn!("Library action quarantine sweep failed: {e}");
        }
        swept
    }

    /// One sidecar into the trash beside its song. False, and left where it is, when it is
    /// outside the music folder, a link, or anything is in the way.
    fn move_sidecar(&self, from: &str, to: &str, music_root: &str) -> bool {
        if let Some(refused) = self.outside(from, music_root) {
            info!("Library action left {from} where it is: {refused}");
            return false;
        }
        if file_exists(to) {
            return false;
        }
        let attempt = || -> io::Result<bool> {
            if let Err(moving) = rename_new(from, to) {
                if moving.kind() == io::ErrorKind::PermissionDenied {
                    return Err(moving);
                }
                let length = std::fs::metadata(from)?.len();
                copy_new(from, to)?;
                if std::fs::metadata(to)?.len() != length {
                    try_delete(to);
                    return Ok(false);
                }
                std::fs::remove_file(from)?;
            }
            Ok(true)
        };
        attempt().unwrap_or_else(|e| {
            warn!("Library action could not move {from} with its song: {e}");
            false
        })
    }

    /// Writes into a trashed song's note which outside songs Octo knew it as, so Put back can
    /// tell the library again.
    pub fn record_mappings(&self, quarantine_path: &str, mappings: &[LocalSongMapping]) {
        if mappings.is_empty() {
            return;
        }
        let Some(manifest) = self.read_manifest(quarantine_path) else {
            return;
        };
        self.write_manifest(
            quarantine_path,
            &QuarantineManifest {
                mappings: Some(mappings.to_vec()),
                ..manifest
            },
        );
    }

    /// The names of the song's own files (its lyrics) waiting in the trash with it, which a
    /// restore puts back too. Empty when there are none, or the manifest cannot be read.
    pub fn sidecar_names(&self, quarantine_path: &str) -> Vec<String> {
        self.read_manifest(quarantine_path)
            .and_then(|manifest| manifest.sidecars)
            .unwrap_or_default()
            .into_iter()
            .filter(|sidecar| file_exists(&sidecar.quarantine_path))
            .map(|sidecar| file_name(&sidecar.original_path))
            .collect()
    }

    /// Follow a folder the library tidy moved: each note in the trash whose song (or one of its
    /// own files) came from under `from` names its place under `to`, so Put back returns it
    /// there. Answers how many notes followed.
    pub fn move_paths(&self, music_root: &str, from: &str, to: &str) -> usize {
        self.move_paths_batch(music_root, &[(from.to_string(), to.to_string())])
    }

    /// [`Self::move_paths`] for each `(from, to)` in order, so `a` to `b` then `b` to `c` ends at
    /// `c`, with the trash walked once and each note written once. Answers how many notes
    /// followed, each move counted on its own.
    pub fn move_paths_batch(&self, music_root: &str, moves: &[(String, String)]) -> usize {
        if moves.is_empty() {
            return 0;
        }
        let mut folders = vec![std::path::PathBuf::from(self.root_for(music_root))];
        let mut moved = 0;
        while let Some(folder) = folders.pop() {
            let Ok(entries) = std::fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    folders.push(path);
                    continue;
                }
                let name = path.to_string_lossy().into_owned();
                let Some(quarantine_path) = name.strip_suffix(Self::MANIFEST_SUFFIX) else {
                    continue;
                };
                let Some(mut manifest) = self.read_manifest(quarantine_path) else {
                    continue;
                };
                let mut written = false;
                for (from, to) in moves {
                    let moved_path = |path: &str| octo_core::common::PathHelper::moved_path(path, from, to);
                    let mut changed = false;
                    if let Some(now) = moved_path(&manifest.original_path) {
                        manifest.original_path = now;
                        changed = true;
                    }
                    for sidecar in manifest.sidecars.iter_mut().flatten() {
                        if let Some(now) = moved_path(&sidecar.original_path) {
                            sidecar.original_path = now;
                            changed = true;
                        }
                    }
                    if changed {
                        moved += 1;
                        written = true;
                    }
                }
                if written {
                    self.write_manifest(quarantine_path, &manifest);
                }
            }
        }
        moved
    }

    /// Written straight over the path, not through a temporary file.
    fn write_manifest(&self, quarantine_path: &str, manifest: &QuarantineManifest) {
        let path = format!("{quarantine_path}{}", Self::MANIFEST_SUFFIX);
        if let Err(e) = std::fs::write(&path, octo_core::json::to_string(manifest)) {
            // The journal still records this, so a missing manifest costs the standalone
            // restore rather than the recovery entirely.
            warn!("Library action could not write a quarantine manifest: {e}");
        }
    }

    pub fn read_manifest(&self, quarantine_path: &str) -> Option<QuarantineManifest> {
        let path = format!("{quarantine_path}{}", Self::MANIFEST_SUFFIX);
        let text = state_file::read_text(Path::new(&path)).ok()??;
        serde_json::from_str(&text).ok()
    }
}

/// The directory: everything before the last separator, or empty.
fn directory_name(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(at) => path[..at].to_string(),
        None => String::new(),
    }
}

/// Everything after the last separator.
fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// The file name up to its last dot.
fn file_name_without_extension(path: &str) -> String {
    let name = file_name(path);
    match name.rfind('.') {
        Some(dot) => name[..dot].to_string(),
        None => name,
    }
}

/// The extension: the name's last dot and what follows, or empty when there is no dot or
/// nothing follows it.
fn extension_of(path: &str) -> String {
    let name = file_name(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot..].to_string(),
        _ => String::new(),
    }
}

/// A rename that never overwrites: refused when something is already at `to`.
fn rename_new(from: &str, to: &str) -> io::Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("Cannot create a file when that file already exists: '{to}'"),
        ));
    }
    std::fs::rename(from, to)
}

/// A copy that never overwrites: refused when something is already at `to`.
fn copy_new(from: &str, to: &str) -> io::Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("The file '{to}' already exists."),
        ));
    }
    std::fs::copy(from, to).map(|_| ())
}

/// A free name for `path`: itself, then ` (2)` to ` (999)` before the extension, then a GUID.
fn unique(path: &str) -> String {
    if !file_exists(path) {
        return path.to_string();
    }
    let path_ref = Path::new(path);
    let directory = path_ref
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = path_ref
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Stem and extension: split at the name's last dot.
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => (name[..dot].to_string(), name[dot..].to_string()),
        Some(dot) => (name[..dot].to_string(), String::new()),
        None => (name.clone(), String::new()),
    };
    for suffix in 2..1000 {
        let candidate = combine(&directory, &format!("{stem} ({suffix}){extension}"));
        if !file_exists(&candidate) {
            return candidate;
        }
    }
    combine(
        &directory,
        &format!("{stem} ({}){extension}", uuid::Uuid::new_v4().simple()),
    )
}

/// A dated trash folder's name, exactly `yyyy-MM-dd`, as midnight UTC.
fn parse_day(name: &str) -> Option<DateTime<Utc>> {
    let bytes = name.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
    if !shaped {
        return None;
    }
    NaiveDate::parse_from_str(name, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
}

/// The files under a dated folder, not counting the manifests.
fn count_files(dir: &Path) -> io::Result<i32> {
    let mut count = 0;
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let kind = item.file_type()?;
        if kind.is_dir() {
            count += count_files(&item.path())?;
        } else if !item
            .file_name()
            .to_string_lossy()
            .ends_with(LibraryActionQuarantine::MANIFEST_SUFFIX)
        {
            count += 1;
        }
    }
    Ok(count)
}

/// The Navidrome ids named by the notes under `dir`, each once. A note that cannot be read names
/// nothing.
fn noted_ids(dir: &Path, ids: &mut Vec<String>) {
    let Ok(items) = std::fs::read_dir(dir) else {
        return;
    };
    for item in items.flatten() {
        let path = item.path();
        if item.file_type().is_ok_and(|kind| kind.is_dir()) {
            noted_ids(&path, ids);
            continue;
        }
        if !item
            .file_name()
            .to_string_lossy()
            .ends_with(LibraryActionQuarantine::MANIFEST_SUFFIX)
        {
            continue;
        }
        let Some(manifest) = state_file::read_text(&path)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<QuarantineManifest>(&text).ok())
        else {
            continue;
        };
        let id = manifest.navidrome_id.trim();
        if !id.is_empty() && !ids.iter().any(|known| known == id) {
            ids.push(id.to_string());
        }
    }
}

/// Joins two paths: `b` when it is rooted, otherwise joined with one separator.
pub(crate) fn combine(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// The relative path between two full paths (case-sensitive): "." for the same path, the rest of
/// `path` under `relative_to`, otherwise a climb out with `..`.
pub(crate) fn get_relative_path(relative_to: &str, path: &str) -> String {
    let from: Vec<&str> = relative_to.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    if common == from.len() && common == to.len() {
        return ".".to_string();
    }
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    let mut relative = parts.join("/");
    if path.ends_with('/') && !relative.is_empty() {
        relative.push('/');
    }
    relative
}

/// Whether putting a file back at `path` would sit beside another spelling of a name on its way,
/// as Windows and SMB compare names: the file's own name in its folder, or the first folder
/// still to be made for it in the deepest folder that is there.
fn twin_on_the_way(path: &str) -> bool {
    let target = Path::new(path);
    let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let mut first_missing = name;
    let mut is_file = true;
    let mut dir = target.parent();
    while let Some(folder) = dir.filter(|folder| !folder.as_os_str().is_empty() && !folder.is_dir()) {
        let Some(folder_name) = folder.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        first_missing = folder_name;
        is_file = false;
        dir = folder.parent();
    }
    let Some(there) = dir.filter(|dir| !dir.as_os_str().is_empty()) else {
        return false;
    };
    std::fs::read_dir(there).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry.file_name().to_str().is_some_and(|entry| {
                entry != first_missing && PathHelper::same_name(entry, first_missing, is_file)
            })
        })
    })
}

fn file_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn try_delete(path: &str) {
    if file_exists(path) {
        // Best effort.
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
#[path = "library_action_quarantine_tests.rs"]
mod tests;
