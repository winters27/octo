//! One answer to "is this the same album?", for placement's album search, the album sibling and
//! the Tidy's File layout task: an album's identity read from its files' tags, and compared the
//! same way everywhere.
//!
//! An album folder is the folder and its disc folders one level down. Its identity is a set
//! value: the album key, one album artist for the whole set, the MusicBrainz release group when
//! every file has one, and the year and type most of its files carry. The album key and the album
//! artist never differ between the tracks of one album; a year or a type can (a track the catalog
//! dates by a reissue), so without a release group they only choose between folders the stable
//! parts already match.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use octo_core::common::artist_credit::primary_credit;
use octo_core::common::layout::{LayoutValues, is_audio_name};
use octo_core::common::{PathHelper, SongIdentity, TagSnapshot};
use octo_core::tagging::candidate_sources::is_various_artists;
use octo_media::tags::TagFile;
use octo_media::tags::tag_writer_extras::{self as extras, TagFields};
use parking_lot::Mutex;

/// The one key every Various Artists spelling ("VA", "Various") compares as.
pub const VARIOUS_ARTISTS: &str = "variousartists";

/// What the layout reads from one audio file's tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutTags {
    pub title: Option<String>,
    /// The performers (ARTIST), in order.
    pub performers: Vec<String>,
    /// The ARTISTS tag, one credited artist a value.
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track: Option<i32>,
    pub disc: Option<i32>,
    pub disc_count: Option<i32>,
    pub year: Option<i32>,
    pub release_type: Option<String>,
    pub compilation: bool,
    pub release_group: Option<String>,
}

impl LayoutTags {
    /// The layout's tags of an open file.
    pub fn read(file: &TagFile) -> LayoutTags {
        let text = |value: Option<String>| value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let number = |value: u32| (value > 0).then_some(value as i32);
        let clean = |values: Vec<String>| -> Vec<String> {
            values
                .into_iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect()
        };
        LayoutTags {
            title: text(file.title()),
            performers: clean(file.performers()),
            artists: clean(extras::read_values(file, TagFields::ARTISTS)),
            album: text(file.album()),
            album_artist: text(file.first_album_artist()),
            track: number(file.track()),
            disc: number(file.disc()),
            disc_count: number(file.disc_count()),
            year: number(file.year()),
            release_type: text(extras::read_text(file, TagFields::RELEASE_TYPE)),
            compilation: extras::is_compilation(file),
            release_group: text(file.music_brainz_release_group_id()).map(|id| id.to_lowercase()),
        }
    }

    /// The artist the file's folder is named after: the first of its credit, as placement picks it
    /// (`primary_credit`, with the ARTISTS tag as the structured credit).
    pub fn primary_credit(&self) -> String {
        let joined = self.performers.join("; ");
        primary_credit(&joined, &[self.artists.first().map(String::as_str)])
    }

    /// The artist the file is by, for telling albums apart: its primary credit without a
    /// featured guest (`Kendrick Lamar feat. Rihanna` is Kendrick Lamar's), so an album with
    /// guests on some tracks is still one artist's.
    pub fn lead_credit(&self) -> String {
        without_featured(&self.primary_credit())
    }

    /// These tags as a saved song's values keep them, read the way the File layout task reads a
    /// file's tags back, so a file whose tags are unchanged compares equal field by field.
    pub fn snapshot(&self) -> TagSnapshot {
        let key = |value: Option<&str>| SongIdentity::key(value.unwrap_or(""));
        TagSnapshot {
            title: key(self.title.as_deref()),
            album: key(self.album.as_deref()),
            artist: SongIdentity::key(&self.performers.join("; ")),
            album_artist: key(self.album_artist.as_deref()),
            track: self.track,
            disc: self.disc,
            disc_count: self.disc_count,
            year: self.year,
            release_type: self.release_type.as_deref().unwrap_or("").trim().to_lowercase(),
            compilation: self.compilation,
            release_group: self.release_group.as_deref().unwrap_or("").trim().to_lowercase(),
        }
    }
}

/// What a featured guest adds to a credit: `feat.`, `ft.` or `featuring`, bracketed or not, and
/// everything after it.
static FEATURED: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\s*[\(\[]?\s*\b(feat\.?|ft\.?|featuring)\s.*$")
        .expect("the featured pattern is valid")
});

/// `name` without a featured guest.
pub fn without_featured(name: &str) -> String {
    let lead = FEATURED.replace(name, "");
    let lead = lead.trim();
    if lead.is_empty() {
        name.trim().to_string()
    } else {
        lead.to_string()
    }
}

impl LayoutTags {
    /// The album artist this file alone says: ALBUMARTIST, else none.
    fn album_artist_key(&self) -> Option<String> {
        self.album_artist.as_deref().map(artist_key)
    }
}

/// An artist name as identities compare it, every Various Artists spelling as one.
pub fn artist_key(name: &str) -> String {
    if is_various_artists(Some(name)) {
        VARIOUS_ARTISTS.to_string()
    } else {
        SongIdentity::key(name)
    }
}

// ---- reading, with a cache ---------------------------------------------------------------------

/// At most this many files' tags are kept, the oldest read going first.
const CACHE_LIMIT: usize = 20_000;

struct Cached {
    size: u64,
    modified: Option<SystemTime>,
    tags: Option<LayoutTags>,
}

#[derive(Default)]
struct TagCache {
    files: HashMap<PathBuf, Cached>,
    order: VecDeque<PathBuf>,
}

static CACHE: LazyLock<Mutex<TagCache>> = LazyLock::new(|| Mutex::new(TagCache::default()));

/// Every tag read in the process, on any thread.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) static TAG_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    /// The tag reads on this thread alone, which tests running beside it never add to.
    pub(crate) static TAG_READS_HERE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The layout's tags of the file at `path`, None when it cannot be read. A file read before with
/// the same size and modified time is answered from the cache; the cache is never held while a
/// file is read.
pub fn read_layout_tags(path: &Path) -> Option<LayoutTags> {
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len();
    let modified = meta.modified().ok();
    {
        let cache = CACHE.lock();
        if let Some(cached) = cache.files.get(path)
            && cached.size == size
            && cached.modified == modified
        {
            return cached.tags.clone();
        }
    }
    #[cfg(test)]
    {
        TAG_READS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        TAG_READS_HERE.with(|reads| reads.set(reads.get() + 1));
    }
    let tags = TagFile::open(path).ok().map(|file| LayoutTags::read(&file));
    let mut cache = CACHE.lock();
    if !cache.files.contains_key(path) {
        cache.order.push_back(path.to_path_buf());
    }
    cache.files.insert(
        path.to_path_buf(),
        Cached {
            size,
            modified,
            tags: tags.clone(),
        },
    );
    while cache.order.len() > CACHE_LIMIT {
        if let Some(oldest) = cache.order.pop_front() {
            cache.files.remove(&oldest);
        }
    }
    tags
}

// ---- folders -------------------------------------------------------------------------------------

/// The visible entries of `dir`, in name order. Names that are not UTF-8 are passed over.
fn entries(dir: &Path) -> Vec<(String, PathBuf, bool)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf, bool)> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if PathHelper::is_hidden_name(&name) {
                return None;
            }
            let path = entry.path();
            let is_dir = path.is_dir();
            Some((name, path, is_dir))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The audio files directly in `dir`, in name order.
pub fn audio_files_in(dir: &Path) -> Vec<PathBuf> {
    entries(dir)
        .into_iter()
        .filter(|(name, _, is_dir)| !is_dir && is_audio_name(name))
        .map(|(_, path, _)| path)
        .collect()
}

/// An album folder's audio files: directly in it, then in each sub-folder one level down.
pub fn album_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = audio_files_in(dir);
    for (_, path, is_dir) in entries(dir) {
        if is_dir {
            files.extend(audio_files_in(&path));
        }
    }
    files
}

/// The sub-folders of `album_dir` that hold audio whose files all carry one disc number, as
/// (name, disc): disc folders, whatever they are called (`Disc 2`, `CD2`). A sub-folder whose
/// files carry no disc number, or several, is not one.
pub fn disc_folders(album_dir: &Path) -> Vec<(String, i32)> {
    let mut found = Vec::new();
    for (name, path, is_dir) in entries(album_dir) {
        if !is_dir {
            continue;
        }
        let files = audio_files_in(&path);
        if files.is_empty() {
            continue;
        }
        let mut disc: Option<i32> = None;
        let mut one = true;
        for file in &files {
            match read_layout_tags(file).and_then(|tags| tags.disc) {
                Some(d) if disc.is_none_or(|seen| seen == d) => disc = Some(d),
                _ => {
                    one = false;
                    break;
                }
            }
        }
        if one && let Some(disc) = disc {
            found.push((name, disc));
        }
    }
    found
}

// ---- identity ------------------------------------------------------------------------------------

/// An album's identity: of one song about to be filed, or of a set of files already together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumIdentity {
    /// The album's `SongIdentity::key`.
    pub album: String,
    /// One album artist for the whole set ([`artist_key`]).
    pub album_artist: String,
    /// The MusicBrainz release group, lowercase, when every file has one and they agree.
    pub release_group: Option<String>,
    /// The year and the release type word (lowercase) most of the files carry.
    pub year: Option<i32>,
    pub album_type: Option<String>,
    /// Each file's primary credit key, once each.
    pub primary_credits: Vec<String>,
}

/// Which of the year and the type the album folder's name shows, so only those tell two folders
/// apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shown {
    pub year: bool,
    pub album_type: bool,
}

/// How a song's album stands to a folder's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The same release group: the same album, whatever the year.
    Same,
    /// The stable parts match, but there is no release group on one side, so a year or type
    /// only chooses between several such folders.
    Weak,
    /// Another album.
    Different,
}

impl AlbumIdentity {
    /// The identity of the files of one album folder, or None when they do not share one album
    /// key (a folder of mixed albums is never an album's).
    pub fn of_files(files: &[LayoutTags]) -> Option<AlbumIdentity> {
        let first = files.first()?;
        let album = SongIdentity::key(first.album.as_deref().unwrap_or(""));
        if album.is_empty()
            || files
                .iter()
                .any(|file| SongIdentity::key(file.album.as_deref().unwrap_or("")) != album)
        {
            return None;
        }
        let mut credits: Vec<String> = Vec::new();
        for file in files {
            let credit = artist_key(&file.lead_credit());
            if !credits.contains(&credit) {
                credits.push(credit);
            }
        }
        let tagged: Vec<String> = files.iter().filter_map(LayoutTags::album_artist_key).collect();
        let album_artist = if let Some(most) = most_common(tagged.iter().cloned()) {
            most
        } else if credits.len() == 1 {
            credits[0].clone()
        } else {
            VARIOUS_ARTISTS.to_string()
        };
        let groups: Vec<&str> = files
            .iter()
            .filter_map(|file| file.release_group.as_deref())
            .collect();
        let release_group = (groups.len() == files.len() && groups.iter().all(|group| *group == groups[0]))
            .then(|| groups[0].to_string());
        Some(AlbumIdentity {
            album,
            album_artist,
            release_group,
            year: most_common(files.iter().filter_map(|file| file.year)),
            album_type: most_common(
                files
                    .iter()
                    .filter_map(|file| file.release_type.as_deref().map(type_word)),
            ),
            primary_credits: credits,
        })
    }

    /// The identity of a song about to be filed, from the values its names are filled from.
    pub fn of_song(values: &LayoutValues) -> AlbumIdentity {
        let album_artist = values
            .album_artist
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&values.artist);
        AlbumIdentity {
            album: SongIdentity::key(&values.album),
            album_artist: artist_key(&without_featured(album_artist)),
            release_group: values
                .release_group
                .as_deref()
                .map(|id| id.trim().to_lowercase())
                .filter(|id| !id.is_empty()),
            year: values.year,
            album_type: values.album_type.as_deref().map(type_word),
            primary_credits: vec![artist_key(&without_featured(&values.artist))],
        }
    }

    /// The album key and the album artist agree: the parts that never differ between the tracks
    /// of one album.
    pub fn stable_match(&self, other: &AlbumIdentity) -> bool {
        !self.album.is_empty() && self.album == other.album && self.album_artist == other.album_artist
    }

    /// How this song's album stands to `folder`'s.
    pub fn verdict(&self, folder: &AlbumIdentity) -> Verdict {
        if !self.stable_match(folder) {
            return Verdict::Different;
        }
        match (&self.release_group, &folder.release_group) {
            (Some(a), Some(b)) if a == b => Verdict::Same,
            (Some(_), Some(_)) => Verdict::Different,
            _ => Verdict::Weak,
        }
    }

    /// Whether the shown year and type agree.
    pub fn shown_match(&self, folder: &AlbumIdentity, shown: Shown) -> bool {
        (!shown.year || self.year == folder.year)
            && (!shown.album_type || self.album_type == folder.album_type)
    }

    /// A folder found under another kind's template, or by another name under a parent with no
    /// artist in its name, is the album's only on a positive sign: the same release group, or
    /// every file credited to the song's `{Artist}`.
    pub fn positive_match(&self, folder: &AlbumIdentity, artist: &str) -> bool {
        let artist = artist_key(&without_featured(artist));
        (self.release_group.is_some() && self.release_group == folder.release_group)
            || (!folder.primary_credits.is_empty()
                && folder.primary_credits.iter().all(|credit| *credit == artist))
    }
}

/// A folder that may hold the song's album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub identity: AlbumIdentity,
    pub audio_files: usize,
}

/// Which of `candidates` holds the song's album: one of the same release group; else, when
/// exactly one matches on the stable parts, that one; else the one whose shown year and type are
/// the song's. Ties go to the most audio files, then name order.
pub fn choose<'a>(song: &AlbumIdentity, candidates: &'a [Candidate], shown: Shown) -> Option<&'a Candidate> {
    let best = |list: Vec<&'a Candidate>| {
        list.into_iter().max_by(|a, b| {
            a.audio_files
                .cmp(&b.audio_files)
                .then_with(|| b.name.cmp(&a.name))
        })
    };
    let same: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| song.verdict(&candidate.identity) == Verdict::Same)
        .collect();
    if !same.is_empty() {
        return best(same);
    }
    let weak: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| song.verdict(&candidate.identity) == Verdict::Weak)
        .collect();
    match weak.len() {
        0 => None,
        1 => weak.into_iter().next(),
        _ => best(
            weak.into_iter()
                .filter(|candidate| song.shown_match(&candidate.identity, shown))
                .collect(),
        ),
    }
}

/// The folder in `parent` that holds the song's album, whatever it is called. Each folder's first
/// audio file is read first and compared on the parts one file can show (the album key, and its
/// ALBUMARTIST when it has one); only a folder that passes has every file read. `positive` names
/// the song's `{Artist}` when a positive sign is needed ([`AlbumIdentity::positive_match`]).
pub fn find_album_folder(
    parent: &Path,
    song: &AlbumIdentity,
    shown: Shown,
    positive: Option<&str>,
) -> Option<String> {
    if song.album.is_empty() {
        return None;
    }
    let mut candidates = Vec::new();
    for (name, path, is_dir) in entries(parent) {
        if !is_dir {
            continue;
        }
        let files = album_files(&path);
        let Some(first) = files.first().and_then(|file| read_layout_tags(file)) else {
            continue;
        };
        if SongIdentity::key(first.album.as_deref().unwrap_or("")) != song.album {
            continue;
        }
        if let Some(theirs) = first.album_artist_key()
            && theirs != song.album_artist
        {
            continue;
        }
        let tags: Vec<LayoutTags> = files.iter().filter_map(|file| read_layout_tags(file)).collect();
        if tags.len() != files.len() {
            continue;
        }
        let Some(identity) = AlbumIdentity::of_files(&tags) else {
            continue;
        };
        if let Some(artist) = positive
            && !song.positive_match(&identity, artist)
        {
            continue;
        }
        candidates.push(Candidate {
            name,
            identity,
            audio_files: files.len(),
        });
    }
    choose(song, &candidates, shown).map(|candidate| candidate.name.clone())
}

/// A release type as one lowercase word (`album`, `single`, `compilation`), so `Album` and
/// `album; compilation` compare by their first type.
fn type_word(release_type: &str) -> String {
    release_type
        .split([';', '/', ','])
        .map(str::trim)
        .find(|part| !part.is_empty())
        .unwrap_or("")
        .to_lowercase()
}

/// The value most often seen, the first seen among equals.
fn most_common<T: Clone + PartialEq>(values: impl Iterator<Item = T>) -> Option<T> {
    let mut counts: Vec<(T, usize)> = Vec::new();
    for value in values {
        match counts.iter_mut().find(|(seen, _)| *seen == value) {
            Some((_, count)) => *count += 1,
            None => counts.push((value, 1)),
        }
    }
    let top = counts.iter().map(|(_, count)| *count).max()?;
    counts
        .into_iter()
        .find(|(_, count)| *count == top)
        .map(|(value, _)| value)
}

#[cfg(test)]
#[path = "album_identity_tests.rs"]
mod tests;
