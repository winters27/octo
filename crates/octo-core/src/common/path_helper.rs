//! Building and sanitizing paths for downloaded music files.
//!
//! Paths are built as strings joined with `/`, as on Linux, where Octo runs. Every name is also
//! made safe where the library is read from: Windows and SMB shares (no `\ / : * ? " < > |`, no
//! control characters, no trailing dot or space on a folder, no reserved name such as `CON`),
//! ext4 (255 bytes a name) and Navidrome (no name with one leading dot, which it hides).

use std::path::Path;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use unicode_normalization::UnicodeNormalization;

use crate::common::SongIdentity;
use crate::common::dotnet::{eq_ignore_case, is_blank, starts_with_ignore_case, utf16_len};
use crate::common::layout::{Layout, LayoutValues, RenderedName};
use crate::settings::subsonic::FolderStructure;

/// Helper class for path building and sanitization.
/// Provides utilities for creating safe file and folder paths for downloaded music files.
pub struct PathHelper;

/// The characters no cleaned name holds on Windows or Linux, which the tests check one by one.
/// Control characters never stay either.
#[cfg(test)]
const INVALID_FILE_NAME_CHARS: [char; 10] = ['\0', '/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// The characters Windows refuses in a name, each with the full-width look-alike written instead,
/// so a name reads as it did and two names never merge.
const LOOK_ALIKES: [(char, char); 7] = [
    (':', '\u{FF1A}'),
    ('*', '\u{FF0A}'),
    ('?', '\u{FF1F}'),
    ('"', '\u{FF02}'),
    ('<', '\u{FF1C}'),
    ('>', '\u{FF1E}'),
    ('|', '\u{FF5C}'),
];

/// What a leading dot becomes: Navidrome hides a name that starts with one dot, and Samba every
/// name that starts with a dot.
const LEADING_DOT: char = '\u{FF0E}';

/// Folder names Navidrome never scans, in any capitals.
const UNSCANNED_FOLDERS: [&str; 6] = [
    "lost+found",
    "@eaDir",
    "#snapshot",
    "$RECYCLE.BIN",
    "@Recycle",
    "@Recently-Snapshot",
];

/// Names Windows keeps for devices, with or without an extension, in any capitals.
const RESERVED_NAMES: [&str; 30] = [
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM0",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "COM\u{B9}",
    "COM\u{B2}",
    "COM\u{B3}",
    "LPT0",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "LPT\u{B9}",
    "LPT\u{B2}",
    "LPT\u{B3}",
];

/// The longest name ext4 takes, in bytes, the extension included.
const MAX_NAME_BYTES: usize = 255;

/// The longest path under the music folder, in UTF-16 units, so a Windows client reaching it
/// through a share named up to 29 characters (`\\192.168.100.100\music\` is 24) stays within
/// MAX_PATH.
pub const PATH_BUDGET: usize = 230;

/// [`PATH_BUDGET`] under the family pool, less room for a member's folder (a name of up to 32
/// characters and its `/`), where each member's link to the file sits.
pub const POOL_BUDGET: usize = PATH_BUDGET - 33;

/// A name the path budget shortens is never cut below this, in UTF-16 units.
const SHORTEST_NAME: usize = 40;

static ANNOTATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[\(\[]([^\)\]]*)[\)\]]").expect("the annotation pattern is valid"));

static WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("the whitespace pattern is valid"));

/// Words that only ever describe how a video was uploaded, never which recording it is.
const UPLOAD_NOISE: [&str; 19] = [
    "official",
    "music",
    "video",
    "audio",
    "lyric",
    "lyrics",
    "visualizer",
    "visualiser",
    "hd",
    "hq",
    "4k",
    "8k",
    "1080p",
    "720p",
    "480p",
    "mv",
    "m/v",
    "clip",
    "videoclip",
];

fn is_upload_noise(word: &str) -> bool {
    UPLOAD_NOISE.iter().any(|noise| eq_ignore_case(noise, word))
}

impl PathHelper {
    /// Where a track goes in one of the built-in layouts ([`FolderStructure::Custom`] gives
    /// Organized), with one name for the artist folder and the file's credit alike. A track with
    /// no album takes its own title as the album. Placement builds from the whole
    /// [`LayoutValues`] instead.
    pub fn build_layout_path(
        structure: FolderStructure,
        download_path: &str,
        artist: &str,
        album: &str,
        title: &str,
        track_number: Option<i32>,
        extension: &str,
    ) -> String {
        let values = LayoutValues {
            artist: artist.to_string(),
            track_artist: artist.to_string(),
            album: album.to_string(),
            title: title.to_string(),
            track: track_number,
            ..LayoutValues::default()
        };
        let names = Layout::preset(structure).render_names(&values, extension);
        Self::join_names(download_path, &names, extension)
    }

    /// `root` with the folder names and the file name (and its extension) under it.
    pub fn join_names(root: &str, names: &[String], extension: &str) -> String {
        let Some((file, folders)) = names.split_last() else {
            return root.to_string();
        };
        let folder = folders
            .iter()
            .fold(root.to_string(), |dir, name| combine(&dir, name));
        combine(&folder, &format!("{file}{extension}"))
    }

    /// A title as a file name. Drops a leading "Artist - " and any bracket that is nothing but
    /// upload noise ("(Official Video)", "[HD]"), and keeps every other annotation, because
    /// "(Live)", "[Remix]" and "(feat. X)" each name a different recording.
    ///
    /// Naming used to strip EVERY bracket, so "Song (Live)" and "Song" landed on one path and
    /// the second download deleted the first: the silent collapse of versions a library must
    /// never suffer (#53).
    pub fn file_title(title: &str, artist: &str) -> String {
        let mut t = title.trim().to_string();
        if t.is_empty() {
            return t;
        }
        let a = artist.trim();
        let prefix = format!("{a} - ");
        if !a.is_empty() && starts_with_ignore_case(&t, &prefix) {
            // OrdinalIgnoreCase matches one character for one, so the prefix spans as many
            // characters of the title as it has itself.
            t = t
                .chars()
                .skip(prefix.chars().count())
                .collect::<String>()
                .trim()
                .to_string();
        }

        let kept = ANNOTATION.replace_all(&t, |found: &Captures<'_>| {
            let words: Vec<&str> = found[1]
                .split([' ', '-', '_'])
                .filter(|w| !w.is_empty())
                .collect();
            if !words.is_empty() && words.iter().all(|word| is_upload_noise(word)) {
                String::new()
            } else {
                found[0].to_string()
            }
        });
        let kept = WHITESPACE.replace_all(&kept, " ").trim().to_string();
        // A title that is only noise ("(Official Video)") keeps its original text rather than
        // becoming an empty file name.
        if kept.is_empty() { t } else { kept }
    }

    /// Gets the cache directory path for temporary file storage.
    /// Uses system temp directory combined with octo-cache subfolder.
    /// Respects TMPDIR environment variable on Linux/macOS.
    pub fn get_cache_path() -> String {
        combine(&std::env::temp_dir().to_string_lossy(), "octo-cache")
    }

    /// Builds the output path for a downloaded track in the Organized layout:
    /// `Artist/Album/01 - Title.ext`, the album being the title when it is blank.
    pub fn build_track_path(
        download_path: &str,
        artist: &str,
        album: &str,
        title: &str,
        track_number: Option<i32>,
        extension: &str,
    ) -> String {
        Self::build_layout_path(
            FolderStructure::Organized,
            download_path,
            artist,
            album,
            title,
            track_number,
            extension,
        )
    }

    /// A title or other free text as part of a file name: NFC, every character Windows refuses
    /// replaced (see [`clean_chars`]), at most 100 UTF-16 units, trimmed; "Unknown" when blank.
    pub fn sanitize_file_name(file_name: &str) -> String {
        if is_blank(file_name) {
            return "Unknown".to_string();
        }

        let mut sanitized = clean_chars(file_name);

        if utf16_len(&sanitized) > 100 {
            sanitized = truncate_utf16(&sanitized, 100);
        }

        sanitized.trim().to_string()
    }

    /// An artist or album as a name: as [`Self::sanitize_file_name`], and with no trailing dots
    /// (Windows drops them from a folder name); "Unknown" when nothing is left.
    pub fn sanitize_folder_name(folder_name: &str) -> String {
        if is_blank(folder_name) {
            return "Unknown".to_string();
        }

        let sanitized = clean_chars(folder_name);

        // Windows drops a folder name's trailing dots and spaces.
        let mut sanitized = sanitized.trim().trim_end_matches(['.', ' ']).to_string();

        if utf16_len(&sanitized) > 100 {
            sanitized = truncate_utf16(&sanitized, 100)
                .trim_end_matches(['.', ' '])
                .to_string();
        }

        // Ensure we have a valid name
        if is_blank(&sanitized) {
            return "Unknown".to_string();
        }

        sanitized
    }

    /// A whole name an older rule made, with every character no name may hold replaced as in each
    /// value of a name (see [`clean_chars`]), but never cut to a value's length, for a name kept
    /// as it is that [`Self::safe_name`] then finishes.
    pub fn clean_name(name: &str) -> String {
        clean_chars(name)
    }

    /// Whether Navidrome hides a file or folder of this name: it starts with one dot, not two
    /// (`...And Justice for All` is listed).
    pub fn is_hidden_name(name: &str) -> bool {
        name != "." && name.starts_with('.') && !name.starts_with("..")
    }

    /// A whole folder name, or a file name without its `extension` (`Some(".flac")`), as it goes
    /// on disk, these steps repeated until none changes it: trimmed; a folder (or a file with no
    /// extension) with no trailing dot or space; a leading dot written as its look-alike (Navidrome
    /// hides a name with one, Samba every one); at most 255 bytes with the extension; a Windows
    /// reserved name (`CON`, `Aux.Mix`), or a folder named like one Navidrome never scans
    /// (`lost+found`, `@eaDir`), with `_` after its first part. "Unknown" when nothing is left.
    pub fn safe_name(name: &str, extension: Option<&str>) -> String {
        let bare_end = extension.is_none_or(str::is_empty);
        let room = MAX_NAME_BYTES.saturating_sub(extension.map_or(0, str::len));
        let mut name = name.to_string();
        loop {
            let before = name.clone();
            name = name.trim().to_string();
            if bare_end {
                name = name.trim_end_matches(['.', ' ']).to_string();
            }
            if name.starts_with('.') {
                name = format!("{LEADING_DOT}{}", &name[1..]);
            }
            if name.len() > room {
                let mut cut = room;
                while !name.is_char_boundary(cut) {
                    cut -= 1;
                }
                name.truncate(cut);
            }
            let first = name.split('.').next().unwrap_or("").trim_end().to_string();
            let reserved = RESERVED_NAMES.contains(&first.to_uppercase().as_str());
            let unscanned = extension.is_none()
                && UNSCANNED_FOLDERS
                    .iter()
                    .any(|unscanned| unscanned.eq_ignore_ascii_case(&name));
            if reserved || unscanned {
                name = format!("{first}_{}", &name[first.len()..]);
            }
            if name == before {
                break;
            }
        }
        if name.is_empty() {
            return "Unknown".to_string();
        }
        name
    }

    /// Whether two names are one on Windows and over SMB ([`fold_name`]): `Are You Ready?.mp3` and
    /// `are you ready？.mp3` are. For files the extensions are compared apart.
    pub fn same_name(a: &str, b: &str, is_file: bool) -> bool {
        if !is_file {
            return fold_name(a, None) == fold_name(b, None);
        }
        let (a_stem, a_extension) = split_extension(a);
        let (b_stem, b_extension) = split_extension(b);
        eq_ignore_case(a_extension, b_extension)
            && fold_name(a_stem, Some(a_extension)) == fold_name(b_stem, Some(b_extension))
    }

    /// Where a new file goes under `root`, from its rendered folder names and file name
    /// ([`crate::common::layout::Layout::render`]), once what is already there is looked at:
    ///
    /// - Each folder's real name comes from its parent's listing: the same name, else another
    ///   spelling of it ([`fold_name`]: other capitals, an old name with a character Windows
    ///   refuses, a name cut to a built-in cap or one the rendered name carries in
    ///   [`RenderedName::cut_caps`]), else, at `hooks.album.at` or `hooks.disc.at`, the folder
    ///   that hook says holds the song's album or disc, else the rendered name. A hidden folder is
    ///   never used.
    /// - The file name shrinks until the path under `root` fits `budget` UTF-16 units, through
    ///   `hooks.fit` when given (the template shrinks its values), else by cutting the name; folders
    ///   never do.
    /// - A file already there by the final or the unshrunk name, or another spelling of either, is
    ///   `taken`.
    ///
    /// A listing that cannot be read is tried once more; a second failure is the answer, so no
    /// folder is ever made on a guess.
    pub fn settle(
        root: &str,
        names: &[RenderedName],
        extension: &str,
        budget: usize,
        hooks: SettleHooks<'_>,
    ) -> std::io::Result<Settled> {
        let SettleHooks { album, disc, fit } = hooks;
        let Some((stem, folders)) = names.split_last() else {
            return Ok(Settled {
                path: root.to_string(),
                folder_existed: false,
                taken: None,
                folders: Vec::new(),
                existing: 0,
            });
        };
        let mut settled: Vec<String> = Vec::with_capacity(folders.len());
        let mut dir = root.to_string();
        let mut there = Path::new(root).is_dir();
        let mut existing = 0;
        for (at, folder) in folders.iter().enumerate() {
            let found = if there {
                let at_album = album.as_ref().is_some_and(|hook| hook.at == at);
                let hook = [&album, &disc].into_iter().flatten().find(|hook| hook.at == at);
                match (existing_folder(&dir, folder, !at_album)?, hook) {
                    (Some(name), _) => Some(name),
                    (None, Some(hook)) => (hook.find)(&dir).filter(|name| !Self::is_hidden_name(name)),
                    (None, None) => None,
                }
            } else {
                None
            };
            there = found.is_some();
            if there {
                existing += 1;
            }
            let name = found.unwrap_or_else(|| folder.name.clone());
            dir = combine(&dir, &name);
            settled.push(name);
        }
        let mut file_stem = stem.name.clone();
        match fit {
            Some(fit) => {
                let room = budget.saturating_sub(
                    settled.iter().map(|name| utf16_len(name)).sum::<usize>()
                        + settled.len()
                        + utf16_len(extension),
                );
                if utf16_len(&file_stem) > room {
                    file_stem = fit(room.max(SHORTEST_NAME));
                }
            }
            None => Self::fit_stem(&settled, &mut file_stem, extension, budget),
        }
        let folder = settled
            .iter()
            .fold(root.to_string(), |dir, name| combine(&dir, name));
        let file = format!("{file_stem}{extension}");
        let taken = if there {
            existing_file(&folder, &file, &format!("{}{extension}", stem.uncut))?
        } else {
            None
        };
        Ok(Settled {
            path: combine(&folder, &file),
            folder_existed: there,
            taken,
            folders: settled,
            existing,
        })
    }

    /// Shortens the file name `stem` so the path under the music folder (`folders`, `stem` and
    /// `extension`) fits `budget` UTF-16 units, never below 40 units, cut at a word break when
    /// one is within the last 15 units. Unchanged when it already fits.
    pub fn fit_stem(folders: &[String], stem: &mut String, extension: &str, budget: usize) {
        let length = folders.iter().map(|name| utf16_len(name)).sum::<usize>()
            + folders.len()
            + utf16_len(stem)
            + utf16_len(extension);
        let excess = length.saturating_sub(budget);
        if excess > 0 {
            *stem = shorten(stem, excess, Some(extension));
        }
    }

    /// True when the path starts with a Windows drive letter ("E:\" or "E:/").
    /// On a non-Windows host such a path is not a location: the filesystem
    /// treats it as a literal directory name.
    pub fn looks_like_windows_drive_path(path: Option<&str>) -> bool {
        let Some(path) = path else { return false };
        let mut chars = path.chars();
        matches!(
            (chars.next(), chars.next(), chars.next()),
            (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic()
        )
    }

    /// Where `path` is once the folder `from` has moved to `to`: `to` for `from` itself, the
    /// same place inside `to` for a path inside `from`, and None for any other path.
    pub fn moved_path(path: &str, from: &str, to: &str) -> Option<String> {
        let from = from.trim_end_matches('/');
        let to = to.trim_end_matches('/');
        if from.is_empty() {
            return None;
        }
        if path == from {
            return Some(to.to_string());
        }
        path.strip_prefix(from)
            .filter(|rest| rest.starts_with('/'))
            .map(|rest| format!("{to}{rest}"))
    }

    /// `base_path`, or `Name (1).ext`, `Name (2).ext` and so on, the first that no file or folder
    /// in its folder already has, by that name or by another spelling of it ([`Self::same_name`]):
    /// on Windows and over SMB those are one name.
    pub fn resolve_unique_path(base_path: &str) -> String {
        let directory = directory_name(base_path);
        let taken: Vec<String> = std::fs::read_dir(if directory.is_empty() { "." } else { &directory })
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let free = |name: &str| {
            !taken
                .iter()
                .any(|there| there == name || Self::same_name(there, name, true))
        };
        if free(file_name(base_path)) {
            return base_path.to_string();
        }

        let extension = extension_of(base_path);
        let file_name_without_ext = file_name_without_extension(base_path);

        let mut counter = 1;
        loop {
            let candidate = format!("{file_name_without_ext} ({counter}){extension}");
            counter += 1;
            if free(&candidate) {
                return combine(&directory, &candidate);
            }
        }
    }
}

/// Where [`PathHelper::settle`] puts a new file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// The file's path, with the real names of the folders already there.
    pub path: String,
    /// Whether the file's folder is already there.
    pub folder_existed: bool,
    /// The file already there under this name or another spelling of it.
    pub taken: Option<String>,
    /// The folder names under the root, as they are spelled on disk where they are there.
    pub folders: Vec<String>,
    /// How many of `folders`, from the root down, are already there.
    pub existing: usize,
}

/// The album folder [`PathHelper::settle`] looks for when no folder of the rendered name is
/// there: `find` answers, for the album folder's parent, the name of the folder already holding
/// the song's album, if one does.
pub struct AlbumHook<'a> {
    /// The album folder's place among the rendered folder names.
    pub at: usize,
    pub find: &'a (dyn Fn(&str) -> Option<String> + Send + Sync),
}

/// What [`PathHelper::settle`] asks the caller, each optional.
#[derive(Default)]
pub struct SettleHooks<'a> {
    /// The album folder's search, for its parent.
    pub album: Option<AlbumHook<'a>>,
    /// The disc folder's search, for the album folder: the sub-folder already holding the song's
    /// disc, whatever it is called (`CD2`).
    pub disc: Option<AlbumHook<'a>>,
    /// The file name rendered again to fit a room of this many UTF-16 units without the
    /// extension, shrinking the template's values rather than cutting the name.
    pub fit: Option<&'a (dyn Fn(usize) -> String + Send + Sync)>,
}

/// `text` in NFC with every character no name may hold replaced: `/`, `\` and control characters
/// by `_` (as `/` always was, so `AC_DC` folders keep their name), and the seven Windows refuses
/// by their full-width look-alikes.
pub(crate) fn clean_chars(text: &str) -> String {
    text.nfc()
        .map(|c| {
            if let Some((_, alike)) = LOOK_ALIKES.iter().find(|(bad, _)| *bad == c) {
                *alike
            } else if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// A name as Windows and SMB compare it, once cleaned as Octo cleans a name: NFC, then each
/// character in its simple upper case (one character for one, as NTFS and Samba compare names),
/// so `ſ` is `S` and `ı` is `I`.
pub fn fold_name(name: &str, extension: Option<&str>) -> String {
    PathHelper::safe_name(&clean_chars(name), extension)
        .chars()
        .map(|c| {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(one), None) => one,
                _ => c,
            }
        })
        .collect()
}

/// A file name's stem and its extension with the dot (empty when there is none).
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(at) if at > 0 && at + 1 < name.len() => (&name[..at], &name[at..]),
        _ => (name, ""),
    }
}

/// `name` cut to `keep` UTF-16 units, at a word break when one is within the last 15 units and
/// leaves at least [`SHORTEST_NAME`], then made safe again. A folder when `extension` is None.
pub(crate) fn cut_name(name: &str, keep: usize, extension: Option<&str>) -> String {
    if utf16_len(name) <= keep {
        return name.to_string();
    }
    let cut = truncate_utf16(name, keep);
    let at_word = cut.rfind(' ').filter(|at| {
        let kept = utf16_len(&cut[..*at]);
        kept + 15 >= keep && kept >= SHORTEST_NAME
    });
    let cut = match at_word {
        Some(at) => &cut[..at],
        None => cut.as_str(),
    };
    PathHelper::safe_name(cut, extension)
}

/// `name` cut down by `excess` UTF-16 units, to no fewer than [`SHORTEST_NAME`] ([`cut_name`]).
fn shorten(name: &str, excess: usize, extension: Option<&str>) -> String {
    let length = utf16_len(name);
    if length <= SHORTEST_NAME {
        return name.to_string();
    }
    cut_name(name, length.saturating_sub(excess).max(SHORTEST_NAME), extension)
}

/// The names in `dir`, as UTF-8 (a name that is not is passed over), read twice before an error
/// is given.
fn listing(dir: &str) -> std::io::Result<Vec<(String, bool)>> {
    let read = || -> std::io::Result<Vec<(String, bool)>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            names.push((name, entry.path().is_dir()));
        }
        Ok(names)
    };
    read().or_else(|_| read())
}

/// How many audio files sit in `dir` and every folder below it, hidden ones left out, so an
/// artist's folder (its songs all in album folders) counts its songs too.
fn audio_files_in(dir: &str) -> usize {
    let mut count = 0;
    let mut folders = vec![std::path::PathBuf::from(dir)];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if PathHelper::is_hidden_name(&name) {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                folders.push(entry.path());
            } else if crate::common::layout::is_audio_name(&name) {
                count += 1;
            }
        }
    }
    count
}

/// The caps every template with the least file name gives (40 units and its extension): one to
/// four folders, in the music folder's budget and the family pool's. The built-in layouts are
/// among them (ByArtist's 183 and 150, Organized's 91 and 74).
pub(crate) fn built_in_caps() -> impl Iterator<Item = usize> {
    [PATH_BUDGET, POOL_BUDGET].into_iter().flat_map(|budget| {
        (1..=4usize).map(move |folders| (budget.saturating_sub(46 + folders) / folders).max(30))
    })
}

/// Every cap the folder `wanted` may have been cut to on disk: the built-in ones
/// ([`built_in_caps`]) and the ones it carries ([`RenderedName::cut_caps`]).
fn possible_caps(wanted: &RenderedName) -> Vec<usize> {
    let mut caps: Vec<usize> = built_in_caps().chain(wanted.cut_caps.iter().copied()).collect();
    caps.sort_unstable();
    caps.dedup();
    caps
}

/// What a folder already on disk may be called to be the rendered folder `wanted`, each folded
/// ([`fold_name`]): its name as rendered or before the cap cut it, and, when an older cap's cut
/// is allowed, exactly what each other cap makes of the uncut name.
struct Spellings<'a> {
    wanted: &'a RenderedName,
    names: Vec<String>,
}

impl<'a> Spellings<'a> {
    fn of(wanted: &'a RenderedName, prefix: bool) -> Spellings<'a> {
        let mut names = vec![fold_name(&wanted.name, None), fold_name(&wanted.uncut, None)];
        if prefix {
            let length = utf16_len(&wanted.uncut);
            names.extend(
                possible_caps(wanted)
                    .into_iter()
                    .filter(|cap| length > *cap)
                    .map(|cap| fold_name(&cut_name(&wanted.uncut, cap, None), None)),
            );
        }
        Spellings { wanted, names }
    }

    /// Whether the folder named `there` is the rendered folder: one of its spellings once
    /// folded, or, at a folder named by an artist alone, another spelling of the name
    /// (`SongIdentity::key`).
    fn matches(&self, there: &str) -> bool {
        let folded = fold_name(there, None);
        if self.names.contains(&folded) {
            return true;
        }
        // A key of fewer than three characters is too short to say two names are one artist's:
        // `Mo` and `MØ` are two.
        self.wanted.artist_level && {
            let key = SongIdentity::key(there);
            key.chars().count() >= 3 && key == SongIdentity::key(&self.wanted.uncut)
        }
    }
}

/// The folder in `dir` that is `wanted`, as it is spelled there: the exact name first, else the
/// one of another spelling with the most audio files, then the first in ordinal order. A hidden
/// one is never given. `prefix` allows an older cap's cut ([`Spellings`]).
fn existing_folder(dir: &str, wanted: &RenderedName, prefix: bool) -> std::io::Result<Option<String>> {
    let folders: Vec<String> = listing(dir)?
        .into_iter()
        .filter(|(name, is_dir)| *is_dir && !PathHelper::is_hidden_name(name))
        .map(|(name, _)| name)
        .collect();
    if let Some(exact) = folders.iter().find(|name| **name == wanted.name) {
        return Ok(Some(exact.clone()));
    }
    let spellings = Spellings::of(wanted, prefix);
    let mut found: Vec<(usize, String)> = folders
        .into_iter()
        .filter(|name| spellings.matches(name))
        .map(|name| (audio_files_in(&combine(dir, &name)), name))
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(found.into_iter().next().map(|(_, name)| name))
}

/// The path of the entry in `folder` that is the file `file` (or its unshrunk form `uncut`), by
/// that name or another spelling of it. A hidden one is never given: placing over it would keep
/// the song hidden.
fn existing_file(folder: &str, file: &str, uncut: &str) -> std::io::Result<Option<String>> {
    let entries = listing(folder)?;
    if entries.iter().any(|(name, _)| name == file) {
        return Ok(Some(combine(folder, file)));
    }
    let mut found: Vec<String> = entries
        .into_iter()
        .map(|(name, _)| name)
        .filter(|there| {
            !PathHelper::is_hidden_name(there)
                && (PathHelper::same_name(there, file, true) || PathHelper::same_name(there, uncut, true))
        })
        .collect();
    found.sort();
    Ok(found.into_iter().next().map(|there| combine(folder, &there)))
}

/// Joins two paths: `b` when it is rooted or `a` is empty, else the two joined by one `/`.
fn combine(a: &str, b: &str) -> String {
    if b.is_empty() {
        return a.to_string();
    }
    if a.is_empty() || b.starts_with('/') {
        return b.to_string();
    }
    if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `s[..units]` in UTF-16 code units. A character split in two by the cut is left out whole,
/// since a Rust string cannot hold a lone surrogate.
fn truncate_utf16(s: &str, units: usize) -> String {
    let mut taken = 0;
    s.chars()
        .take_while(|c| {
            taken += c.len_utf16();
            taken <= units
        })
        .collect()
}

/// The file name part of a path: everything after the last `/`.
fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The directory of a path that names a file: everything before the last `/` ("/" itself for a
/// file in the root), or empty when there is none.
fn directory_name(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(at) => path[..at].trim_end_matches('/').to_string(),
        None => String::new(),
    }
}

/// The extension: from the last `.` of the file name, or empty when there is none or it
/// is the last character.
fn extension_of(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(at) if at + 1 < name.len() => &name[at..],
        _ => "",
    }
}

/// The file name up to its last `.`.
fn file_name_without_extension(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(at) => &name[..at],
        None => name,
    }
}

#[cfg(test)]
#[path = "path_helper_tests.rs"]
mod tests;
