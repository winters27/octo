//! The plan for the Tidy's File layout task: songs already in the library moved and renamed into
//! the file layout. Pure: the files the walk read (their tags, Octo's records of them, the
//! listings of the folders), what Lidarr keeps and the layout in, plan out, so every rule is a
//! table test.
//!
//! The task never fights placement or the Artists task:
//!
//! - A file Octo placed is named again from the values placement used, field by field while its
//!   tag still says what was written; one placed by an older Octo is read back from the shape its
//!   path has; anything else from its tags.
//! - A name whose `SongIdentity::key` is the current name's keeps the current spelling, cleaned.
//! - A folder keeps its name when any song of an album in it renders that name at that level;
//!   otherwise that album takes the name most of its songs render.
//! - One album never splits: an album group (its album folder and its disc folders) moves whole
//!   or not at all, and gets one kind and one album folder.
//! - Disc folders are decided once per album group.
//!
//! Folder renames come first, then file moves (a song, then each file beside it that takes its
//! name), then covers copied into a folder an album left its old one for.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use octo_core::common::artist_credit::primary_credit;
use octo_core::common::dotnet::utf16_len;
use octo_core::common::layout::{BY_ARTIST, FLAT, LayoutKind, ORGANIZED, album_type_word, is_audio_name};
use octo_core::common::path_helper::{PATH_BUDGET, fold_name};
use octo_core::common::{Layout, LayoutValues, PathHelper, RenderedName, SongIdentity};
use regex::Regex;
use sha2::{Digest, Sha256};

use super::planner::{TidyCopy, TidyFileMove, TidyKind, TidyMove, TidyPlan, TidyRow, is_excluded, relative};
use super::store::TidyLidarr;
use crate::services::common::album_identity::{
    AlbumIdentity, Candidate, LayoutTags, Shown, VARIOUS_ARTISTS, artist_key, choose,
};

// ---- what the planner reads -------------------------------------------------------------------

/// One audio file as the File layout planner sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutFile {
    /// The absolute path, with "/" separators.
    pub path: String,
    pub size: u64,
    /// Out-of-scope files are never moved, and an album holding one only moves within its folder.
    pub in_scope: bool,
    /// The layout's tags, None when they could not be read.
    pub tags: Option<LayoutTags>,
    /// Octo's record of the file, when Octo placed it.
    pub mapping: Option<LayoutMapping>,
}

/// What Octo recorded when it placed a file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutMapping {
    pub title: String,
    pub album: String,
    pub artist: String,
    /// The values the names were filled from, when the placement kept them.
    pub values: Option<LayoutValues>,
}

/// The folders the walk read, each with its entries, and the folders it left alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutDisk {
    /// Each folder's absolute path and its entries: the name and whether it is a folder.
    pub folders: BTreeMap<String, Vec<(String, bool)>>,
    /// Folders holding an `.ndignore`, left alone with everything under them.
    pub ignored: Vec<String>,
}

/// What Lidarr keeps, in Octo's paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LidarrView {
    /// Files Lidarr tracks.
    pub tracked: HashSet<String>,
    /// Files under a Lidarr root Octo cannot see that may be Lidarr's.
    pub maybe: HashSet<String>,
    /// Lidarr's artist folders, never renamed and never removed when emptied.
    pub artist_folders: Vec<String>,
}

impl LidarrView {
    /// Whether `folder` holds a file Lidarr tracks.
    pub fn keeps_a_song_in(&self, folder: &str) -> bool {
        self.tracked
            .iter()
            .chain(&self.maybe)
            .any(|path| parent_of(path) == folder)
    }
}

/// Whether a song Octo would file at a target waits for an open Lidarr follow: the song's artists,
/// its album and the target.
pub type FollowCheck = Arc<dyn Fn(&[String], &str, &str) -> bool + Send + Sync>;

/// The settings of one File layout plan.
#[derive(Clone)]
pub struct LayoutOptions {
    pub layout: Layout,
    /// The longest path under the root, in UTF-16 units.
    pub budget: usize,
    /// The filesystem treats "a" and "A" as one name.
    pub case_insensitive: bool,
    /// Folders under the root (relative) the person left out.
    pub excluded: Vec<String>,
    pub lidarr: LidarrView,
    pub follow: Option<FollowCheck>,
    /// The family pool: each file moves on its own (with every member's link), and its lyrics
    /// and its folder's covers go with it there, so no folder is renamed and nothing else moves.
    pub pool: bool,
}

impl LayoutOptions {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            budget: PATH_BUDGET,
            case_insensitive: false,
            excluded: Vec::new(),
            lidarr: LidarrView::default(),
            follow: None,
            pool: false,
        }
    }
}

/// The plan, with each Layout row's fingerprint by row id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutPlan {
    pub plan: TidyPlan,
    pub row_hashes: BTreeMap<String, String>,
}

// ---- words ------------------------------------------------------------------------------------

const UNREADABLE: &str = "Its tags could not be read, so Octo cannot tell where it goes.";
const CASE_ONLY: &str = "Only the capitals differ, and this disk cannot rename a folder by capitals alone.";
const OUT_OF_RUN: &str = "Some songs of this album are not in this run, and an album only moves whole.";
const ALBUM_STAYS: &str = "Some songs of this album stay, and an album only moves whole.";
const LIDARR_KEEPS: &str = "Lidarr keeps this song, so Lidarr names it.";
const LIDARR_MAY: &str = "Lidarr may keep this song, so it stays.";
const LIDARR_HERE: &str = "Lidarr keeps a song here now.";
const IGNORED: &str = "Navidrome is told to leave this folder out (.ndignore), so the tidy leaves it too.";
const SCATTERED: &str = "This compilation's songs would each go to their own artist's folder, so it stays together. Set a Compilations layout to file it as one album.";
const FOLLOW: &str = "Octo is waiting on Lidarr for this artist, so this song moves once that is done.";

fn two_songs(a: &str, b: &str, name: &str) -> String {
    format!("{a} and {b} would both be named {name}, so both stay.")
}

fn taken(name: &str) -> String {
    format!("{name} is already there, so this song stays.")
}

fn folder_there(folder: &str) -> String {
    format!("{folder} is already there with other capitals or characters, so these songs stay.")
}

fn sidecar_taken(name: &str) -> String {
    format!("{name} is already there, so this song and its lyrics stay.")
}

fn shared_stem(stem: &str) -> String {
    format!("Two songs share the name {stem}, so their lyrics stay where they are.")
}

// ---- the plan ---------------------------------------------------------------------------------

/// One readable song.
struct Song<'a> {
    file: &'a LayoutFile,
    tags: &'a LayoutTags,
    /// Its folders under the root, as they are now.
    chain: Vec<String>,
    folder: String,
    stem: String,
    /// With its dot, as the file has it; empty when there is none.
    ext: String,
    values: LayoutValues,
    /// The kind it was filed by, while its tags still say what placement wrote.
    saved_kind: Option<LayoutKind>,
    in_scope: bool,
    lidarr: Option<&'static str>,
    group: usize,
    /// The rendered folder names (as decided level by level) and file name.
    folders: Vec<RenderedName>,
    file_name: RenderedName,
    /// Where it goes, before anything holds it.
    wanted: Option<String>,
    /// Why it stays.
    held: Option<String>,
}

struct Group {
    songs: Vec<usize>,
    kind: LayoutKind,
    identity: AlbumIdentity,
    /// The template's album folder, as a place among its segments.
    album_segment: Option<usize>,
    shown: Shown,
}

#[derive(Default)]
struct Out {
    rows: Vec<TidyRow>,
    seen: HashSet<String>,
}

impl Out {
    fn skip(&mut self, id: String, path: &str, evidence: impl Into<String>) {
        if !self.seen.insert(id.clone()) {
            return;
        }
        self.rows.push(TidyRow {
            id,
            kind: TidyKind::Skip,
            path: path.to_string(),
            before: path.to_string(),
            after: String::new(),
            evidence: evidence.into(),
            files: 0,
            opt_in: false,
            ticked: false,
        });
    }
}

/// Plans the File layout task for `files` under `root`.
pub fn plan(root: &str, files: &[LayoutFile], disk: &LayoutDisk, options: &LayoutOptions) -> LayoutPlan {
    let root = root.trim_end_matches('/').to_string();
    let mut out = Out::default();
    let shape = preset_shape(&options.layout);

    for folder in &disk.ignored {
        let rel = rel_of(&root, folder);
        out.skip(format!("ignored|{rel}"), &rel, IGNORED);
    }

    let mut songs: Vec<Song> = Vec::new();
    for file in files {
        let Some(rel) = relative(&root, &file.path) else {
            continue;
        };
        if disk.ignored.iter().any(|folder| is_under(&file.path, folder)) {
            continue;
        }
        let in_scope = file.in_scope && !is_excluded(&root, &file.path, &options.excluded);
        let Some(tags) = file.tags.as_ref() else {
            if in_scope {
                let shown = rel.join("/");
                out.skip(format!("unreadable|{shown}"), &shown, UNREADABLE);
            }
            continue;
        };
        let name = rel.last().cloned().unwrap_or_default();
        let (stem, ext) = split_name(&name);
        let chain = rel[..rel.len() - 1].to_vec();
        let (values, saved_kind) = values_of(file, tags, &chain, &stem, shape);
        let lidarr = if options.lidarr.tracked.contains(&file.path) {
            Some(LIDARR_KEEPS)
        } else if options.lidarr.maybe.contains(&file.path) {
            Some(LIDARR_MAY)
        } else {
            None
        };
        songs.push(Song {
            file,
            tags,
            chain,
            folder: parent_of(&file.path),
            stem,
            ext,
            values,
            saved_kind,
            in_scope,
            lidarr,
            group: 0,
            folders: Vec::new(),
            file_name: RenderedName::default(),
            wanted: None,
            held: None,
        });
    }

    let mut groups = group_songs(&root, &mut songs);
    for group in &mut groups {
        let template = options.layout.template(group.kind);
        group.album_segment = template.album_folder_segment();
        let (year, album_type) = template.album_folder_shows();
        group.shown = Shown { year, album_type };
    }
    render(&mut songs, &groups, options);
    decide_names(&root, &mut songs, &groups, &mut out);
    resolve(&root, &mut songs, &groups, disk, options, &mut out);
    operations(&root, files, &mut songs, &groups, disk, options, &mut out)
}

// ---- values -----------------------------------------------------------------------------------

/// A song's values from its tags alone.
pub fn tag_values(tags: &LayoutTags) -> LayoutValues {
    let named = |name: String| {
        if name.trim().is_empty() {
            "Unknown Artist".to_string()
        } else {
            name
        }
    };
    let track_artist = tags.performers.join("; ");
    let album = tags.album.clone().unwrap_or_default();
    let no_album = album.trim().is_empty();
    let album_type = if no_album {
        Some("Single".to_string())
    } else {
        album_type_word(tags.release_type.as_deref(), tags.compilation)
    };
    let mut values = LayoutValues {
        artist: named(tags.primary_credit()),
        track_artist: named(track_artist.clone()),
        album_artist: tags.album_artist.clone(),
        title: PathHelper::file_title(tags.title.as_deref().unwrap_or(""), &track_artist),
        album,
        track: tags.track,
        disc: tags.disc,
        disc_count: tags.disc_count,
        multi_disc: false,
        year: tags.year,
        album_type,
        kind: LayoutKind::of(tags.release_type.as_deref(), tags.compilation, no_album),
        release_group: tags.release_group.clone(),
        tags: None,
    };
    values.multi_disc = values.says_multi_disc();
    values
}

/// The values a song is named from, and the kind its placement filed it by while that still holds.
fn values_of(
    file: &LayoutFile,
    tags: &LayoutTags,
    chain: &[String],
    stem: &str,
    shape: Option<Shape>,
) -> (LayoutValues, Option<LayoutKind>) {
    let from_tags = tag_values(tags);
    let Some(mapping) = &file.mapping else {
        return (from_tags, None);
    };
    if let Some(saved) = &mapping.values {
        return saved_values(saved, mapping, tags, &from_tags);
    }
    match read_back(mapping, tags, chain, stem, shape, &from_tags) {
        Some(values) => (values, None),
        None => (from_tags, None),
    }
}

/// The values a file Octo placed is named from, as this task names it: the values `mapping`
/// saved, each field kept only while the file's tag still says what was written. None when the
/// mapping saved none.
pub(crate) fn kept_values(mapping: &LayoutMapping, tags: &LayoutTags) -> Option<LayoutValues> {
    let saved = mapping.values.as_ref()?;
    Some(saved_values(saved, mapping, tags, &tag_values(tags)).0)
}

/// The saved values, each field kept only while the file's tag still says what was written; a
/// field whose tag changed since comes from the tags.
fn saved_values(
    saved: &LayoutValues,
    mapping: &LayoutMapping,
    tags: &LayoutTags,
    from_tags: &LayoutValues,
) -> (LayoutValues, Option<LayoutKind>) {
    let key = |value: Option<&str>| SongIdentity::key(value.unwrap_or(""));
    let mut values = saved.clone();
    let snapshot = saved.tags.clone();
    let (title, album, artist) = match &snapshot {
        Some(snapshot) => (
            snapshot.title.clone(),
            snapshot.album.clone(),
            snapshot.artist.clone(),
        ),
        None => (
            SongIdentity::key(&mapping.title),
            SongIdentity::key(&mapping.album),
            SongIdentity::key(&mapping.artist),
        ),
    };
    if key(tags.title.as_deref()) != title {
        values.title = from_tags.title.clone();
    }
    if key(tags.album.as_deref()) != album {
        values.album = from_tags.album.clone();
    }
    if SongIdentity::key(&tags.performers.join("; ")) != artist {
        values.artist = from_tags.artist.clone();
        values.track_artist = from_tags.track_artist.clone();
    }
    let mut kind = Some(saved.kind);
    if let Some(snapshot) = &snapshot {
        if key(tags.album_artist.as_deref()) != snapshot.album_artist {
            values.album_artist = from_tags.album_artist.clone();
        }
        if tags.track != snapshot.track {
            values.track = from_tags.track;
        }
        if tags.disc != snapshot.disc {
            values.disc = from_tags.disc;
        }
        if tags.disc_count != snapshot.disc_count {
            values.disc_count = from_tags.disc_count;
        }
        if tags.year != snapshot.year {
            values.year = from_tags.year;
        }
        let release_type = tags.release_type.as_deref().unwrap_or("").trim().to_lowercase();
        if release_type != snapshot.release_type || tags.compilation != snapshot.compilation {
            values.album_type = from_tags.album_type.clone();
            values.kind = from_tags.kind;
            kind = None;
        }
        let group = tags.release_group.as_deref().unwrap_or("").trim().to_lowercase();
        if group != snapshot.release_group {
            values.release_group = from_tags.release_group.clone();
        }
    }
    values.multi_disc = values.says_multi_disc();
    (values, kind)
}

/// Which built-in layout's shape a path may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Flat,
    ByArtist,
    Organized,
}

/// The built-in layout every template of `layout` is, if they are all one.
fn preset_shape(layout: &Layout) -> Option<Shape> {
    let texts = layout.texts();
    [
        (FLAT, Shape::Flat),
        (BY_ARTIST, Shape::ByArtist),
        (ORGANIZED, Shape::Organized),
    ]
    .into_iter()
    .find(|(text, _)| texts.iter().all(|t| t == text))
    .map(|(_, shape)| shape)
}

static TRACK_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(-?\d+) - (.+)$").expect("the track prefix pattern is valid"));

static COPY_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*) \((\d+)\)$").expect("the copy number pattern is valid"));

/// The values an older Octo named a file from, read back from the preset shape its path has: the
/// old names, cleaned again. Under the preset of that shape they render the path back; under any
/// other layout each one counts only where it agrees with the record or the tags. None when the
/// path has no preset's shape.
fn read_back(
    mapping: &LayoutMapping,
    tags: &LayoutTags,
    chain: &[String],
    stem: &str,
    shape: Option<Shape>,
    from_tags: &LayoutValues,
) -> Option<LayoutValues> {
    let tag_artist = tags.performers.join("; ");
    let credits = [
        SongIdentity::key(&mapping.artist),
        SongIdentity::key(&tag_artist),
        SongIdentity::key(&primary_credit(&mapping.artist, &[])),
        SongIdentity::key(&tags.primary_credit()),
    ];
    let is_credit = |name: &str| {
        let key = SongIdentity::key(name);
        !key.is_empty() && credits.contains(&key)
    };
    let (path_shape, artist, album, track, title) = match chain {
        [] => {
            let splits: Vec<usize> = stem.match_indices(" - ").map(|(at, _)| at).collect();
            let at = splits
                .iter()
                .copied()
                .find(|at| is_credit(&stem[..*at]))
                .or_else(|| splits.first().copied())?;
            (
                Shape::Flat,
                stem[..at].to_string(),
                None,
                None,
                stem[at + 3..].to_string(),
            )
        }
        [artist] => (Shape::ByArtist, artist.clone(), None, None, stem.to_string()),
        [artist, album] => match TRACK_PREFIX.captures(stem) {
            Some(found) => (
                Shape::Organized,
                artist.clone(),
                Some(album.clone()),
                found[1].parse::<i32>().ok(),
                found[2].to_string(),
            ),
            None => (
                Shape::Organized,
                artist.clone(),
                Some(album.clone()),
                None,
                stem.to_string(),
            ),
        },
        _ => return None,
    };
    let mut values = from_tags.clone();
    let key = |value: Option<&str>| SongIdentity::key(value.unwrap_or(""));
    if shape == Some(path_shape) {
        match path_shape {
            Shape::Flat => values.track_artist = artist,
            _ => values.artist = artist,
        }
        if let Some(album) = album {
            values.album = album;
        }
        values.track = track;
        values.title = title;
    } else {
        let title_key = SongIdentity::key(&title);
        let record_key = SongIdentity::key(&mapping.title);
        if !title_key.is_empty()
            && !record_key.is_empty()
            && (title_key.starts_with(&record_key) || record_key.starts_with(&title_key))
        {
            values.title = title;
        }
        if is_credit(&artist) {
            match path_shape {
                Shape::Flat => {
                    values.artist = primary_credit(&artist, &[tags.artists.first().map(String::as_str)]);
                    values.track_artist = artist;
                }
                _ => values.artist = artist,
            }
        }
        if let Some(album) = album {
            let album_key = SongIdentity::key(&album);
            if album_key == SongIdentity::key(&mapping.album) || album_key == key(tags.album.as_deref()) {
                values.album = album;
            }
        }
        if let Some(track) = track
            && tags.track.is_none_or(|tag| tag == track)
        {
            values.track = Some(track);
        }
    }
    // A tag edited since the file was placed wins over the old name.
    if key(tags.title.as_deref()) != SongIdentity::key(&mapping.title) {
        values.title = from_tags.title.clone();
    }
    if key(tags.album.as_deref()) != SongIdentity::key(&mapping.album) {
        values.album = from_tags.album.clone();
    }
    if SongIdentity::key(&tag_artist) != SongIdentity::key(&mapping.artist) {
        values.artist = from_tags.artist.clone();
        values.track_artist = from_tags.track_artist.clone();
    }
    Some(values)
}

// ---- album groups -----------------------------------------------------------------------------

/// Groups the songs into albums, three steps so a group never depends on itself: within each album
/// folder (with its disc folders), by the album key and the set's album artist, and the release
/// group where every file has one; then one kind per group. Each song learns its group.
fn group_songs(root: &str, songs: &mut [Song]) -> Vec<Group> {
    let containers = containers(root, songs);
    let mut by_container: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (at, song) in songs.iter().enumerate() {
        let container = containers
            .get(&song.folder)
            .cloned()
            .unwrap_or_else(|| song.folder.clone());
        by_container.entry(container).or_default().push(at);
    }
    let mut groups: Vec<Group> = Vec::new();
    for (container, members) in by_container {
        let album_folder = container != root;
        let mut by_album: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for at in members {
            let key = SongIdentity::key(songs[at].tags.album.as_deref().unwrap_or(""));
            if key.is_empty() {
                groups.push(new_group(vec![at], songs));
            } else {
                by_album.entry(key).or_default().push(at);
            }
        }
        for (_, members) in by_album {
            // The set's album artist: ALBUMARTIST where a file has one; for the rest, the credit
            // they all share, else Various Artists. Loose files in the music folder are no set.
            let mut by_artist: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            let mut untagged: Vec<usize> = Vec::new();
            for at in members {
                match songs[at].tags.album_artist.as_deref() {
                    Some(artist) => by_artist.entry(artist_key(artist)).or_default().push(at),
                    None => untagged.push(at),
                }
            }
            if !untagged.is_empty() {
                let credits: BTreeSet<String> = untagged
                    .iter()
                    .map(|at| artist_key(&songs[*at].tags.lead_credit()))
                    .collect();
                if album_folder || credits.len() == 1 {
                    let key = if credits.len() == 1 {
                        credits.into_iter().next().unwrap_or_default()
                    } else {
                        VARIOUS_ARTISTS.to_string()
                    };
                    by_artist.entry(key).or_default().extend(untagged);
                } else {
                    for at in untagged {
                        let key = artist_key(&songs[at].tags.lead_credit());
                        by_artist.entry(key).or_default().push(at);
                    }
                }
            }
            for (_, members) in by_artist {
                let ids: Vec<Option<&str>> = members
                    .iter()
                    .map(|at| songs[*at].tags.release_group.as_deref())
                    .collect();
                if ids.iter().all(Option::is_some) {
                    let mut by_id: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                    for (at, id) in members.iter().zip(ids) {
                        by_id
                            .entry(id.unwrap_or_default().to_lowercase())
                            .or_default()
                            .push(*at);
                    }
                    for (_, members) in by_id {
                        groups.push(new_group(members, songs));
                    }
                } else {
                    groups.push(new_group(members, songs));
                }
            }
        }
    }
    for (at, group) in groups.iter().enumerate() {
        for song in &group.songs {
            songs[*song].group = at;
        }
    }
    groups
}

/// A group of `members`, with its identity and its one kind: the saved kind most of its files
/// carry; else Compilation by a file's compilation flag or a Various Artists album artist (never by
/// a guest on some tracks); else a single or an album by the release type most files carry.
fn new_group(members: Vec<usize>, songs: &mut [Song]) -> Group {
    let tags: Vec<LayoutTags> = members.iter().map(|at| songs[*at].tags.clone()).collect();
    let identity =
        AlbumIdentity::of_files(&tags).unwrap_or_else(|| AlbumIdentity::of_song(&songs[members[0]].values));
    let saved: Vec<LayoutKind> = members.iter().filter_map(|at| songs[*at].saved_kind).collect();
    let kind = if let Some(kind) = most_common(saved.iter().copied()) {
        kind
    } else if tags.iter().any(|tag| tag.compilation) || identity.album_artist == VARIOUS_ARTISTS {
        LayoutKind::Compilation
    } else {
        let release_type = most_common(tags.iter().filter_map(|tag| tag.release_type.clone()));
        let blank = tags
            .iter()
            .all(|tag| tag.album.as_deref().is_none_or(|album| album.trim().is_empty()));
        LayoutKind::of(release_type.as_deref(), false, blank)
    };
    // One album artist for the set: songs whose tags name none take the set's.
    if identity.album_artist == VARIOUS_ARTISTS || identity.primary_credits.len() == 1 {
        let name = if identity.album_artist == VARIOUS_ARTISTS {
            "Various Artists".to_string()
        } else {
            songs[members[0]].tags.lead_credit()
        };
        for at in &members {
            let song = &mut songs[*at];
            if song.values.album_artist.is_none()
                && song.tags.album_artist.is_none()
                && song.saved_kind.is_none()
            {
                song.values.album_artist = Some(name.clone());
            }
        }
    }
    // Disc folders are decided once per group: one track of several discs puts every track in
    // its disc folder.
    let multi = members.iter().any(|at| songs[*at].values.says_multi_disc());
    for at in &members {
        songs[*at].values.multi_disc = multi;
        songs[*at].values.kind = kind;
    }
    Group {
        songs: members,
        kind,
        identity,
        album_segment: None,
        shown: Shown::default(),
    }
}

/// Each song folder's album folder: a folder of one disc whose album has its other discs, or loose
/// tracks, in the folder above belongs to that folder; any other folder is its own.
fn containers(root: &str, songs: &[Song]) -> HashMap<String, String> {
    struct Facts {
        disc: Option<i32>,
        keys: HashSet<String>,
    }
    let mut facts: BTreeMap<&str, Facts> = BTreeMap::new();
    for song in songs {
        let key = SongIdentity::key(song.tags.album.as_deref().unwrap_or(""));
        let entry = facts.entry(song.folder.as_str()).or_insert_with(|| Facts {
            disc: song.tags.disc,
            keys: HashSet::new(),
        });
        if entry.disc != song.tags.disc {
            entry.disc = None;
        }
        if !key.is_empty() {
            entry.keys.insert(key);
        }
    }
    let mut children: HashMap<String, Vec<&str>> = HashMap::new();
    for folder in facts.keys() {
        children.entry(parent_of(folder)).or_default().push(folder);
    }
    let mut found = HashMap::new();
    for (folder, own) in &facts {
        let parent = parent_of(folder);
        if *folder == root || parent == root || !is_under(&parent, root) {
            continue;
        }
        let Some(disc) = own.disc else {
            continue;
        };
        let shares = |other: &Facts| other.keys.iter().any(|key| own.keys.contains(key));
        let loose = facts.get(parent.as_str()).is_some_and(shares);
        let sibling = children.get(&parent).is_some_and(|siblings| {
            siblings.iter().any(|sibling| {
                *sibling != *folder
                    && facts
                        .get(sibling)
                        .is_some_and(|other| other.disc.is_some_and(|d| d != disc) && shares(other))
            })
        });
        if loose || sibling {
            found.insert(folder.to_string(), parent);
        }
    }
    found
}

// ---- names ------------------------------------------------------------------------------------

/// Each song's names as its group's template renders them.
fn render(songs: &mut [Song], groups: &[Group], options: &LayoutOptions) {
    for song in songs.iter_mut() {
        let template = options.layout.template(groups[song.group].kind);
        let mut names = template.render(&song.values, &song.ext, options.budget);
        let file = names.pop().unwrap_or_default();
        song.folders = names;
        song.file_name = file;
    }
}

/// The folder names, level by level from the top:
///
/// - a name whose key is the current folder's keeps the current spelling, cleaned;
/// - a compilation the template would scatter over several folders stays as it is;
/// - within one album group, a folder keeps its name when any of the group's songs in it renders
///   that name, else takes the name most of them render (songs of other albums in the folder
///   decide for themselves, so a folder of mixed artists is never pulled into one);
/// - every song of an album group shares the folders down to the album folder (the most
///   rendered).
fn decide_names(root: &str, songs: &mut [Song], groups: &[Group], out: &mut Out) {
    for song in songs.iter_mut() {
        for (level, name) in song.folders.iter_mut().enumerate() {
            if let Some(current) = song.chain.get(level)
                && same_key(&name.uncut, current)
            {
                let kept = cleaned_folder(current, name.cap);
                name.uncut = PathHelper::safe_name(&clean_text(current), None);
                name.name = kept;
            }
        }
    }
    for group in groups {
        let Some(album) = group
            .album_segment
            .filter(|_| group.kind == LayoutKind::Compilation)
        else {
            continue;
        };
        let chains: BTreeSet<Vec<String>> = group
            .songs
            .iter()
            .map(|at| {
                songs[*at]
                    .folders
                    .iter()
                    .filter(|name| name.segment <= album)
                    .map(|name| name.name.clone())
                    .collect()
            })
            .collect();
        if chains.len() > 1 {
            let folder = &songs[group.songs[0]].folder;
            let rel = rel_of(root, folder);
            out.skip(format!("scattered|{rel}"), &rel, SCATTERED);
            for at in &group.songs {
                songs[*at].held.get_or_insert_with(|| SCATTERED.to_string());
            }
        }
    }
    let depth = songs.iter().map(|song| song.folders.len()).max().unwrap_or(0);
    for level in 0..depth {
        let mut by_folder: BTreeMap<(usize, String), Vec<usize>> = BTreeMap::new();
        for (at, song) in songs.iter().enumerate() {
            if level < song.folders.len() && level < song.chain.len() {
                let path = join_all(root, &song.chain[..=level]);
                by_folder.entry((song.group, path)).or_default().push(at);
            }
        }
        for ((_, path), members) in by_folder {
            let current = name_of(&path);
            let keeps = members.iter().any(|at| {
                let name = &songs[*at].folders[level];
                name.name == cleaned_folder(&current, name.cap)
            });
            let chosen = if keeps {
                None
            } else {
                most_common_name(members.iter().map(|at| songs[*at].folders[level].name.clone()))
            };
            for at in members {
                let name = &mut songs[at].folders[level];
                match &chosen {
                    None => {
                        name.name = cleaned_folder(&current, name.cap);
                        name.uncut = PathHelper::safe_name(&clean_text(&current), None);
                    }
                    Some(chosen) => {
                        if name.name != *chosen {
                            name.name = chosen.clone();
                            name.uncut = chosen.clone();
                        }
                    }
                }
            }
        }
    }
    // One album, one album folder.
    for group in groups {
        let Some(album) = group.album_segment else {
            continue;
        };
        let shared = |song: &Song| -> Vec<String> {
            song.folders
                .iter()
                .filter(|name| name.segment <= album)
                .map(|name| name.name.clone())
                .collect()
        };
        let chains: Vec<Vec<String>> = group.songs.iter().map(|at| shared(&songs[*at])).collect();
        if chains.windows(2).all(|pair| pair[0] == pair[1]) {
            continue;
        }
        let Some(winner) = most_common_chain(chains.iter().cloned()) else {
            continue;
        };
        let model = group
            .songs
            .iter()
            .find(|at| shared(&songs[**at]) == winner)
            .map(|at| {
                songs[*at]
                    .folders
                    .iter()
                    .filter(|name| name.segment <= album)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for at in &group.songs {
            let song = &mut songs[*at];
            let below: Vec<RenderedName> = song
                .folders
                .iter()
                .filter(|name| name.segment > album)
                .cloned()
                .collect();
            song.folders = model.iter().cloned().chain(below).collect();
        }
    }
}

/// The target of every song not already held: its folders found on disk (the same name; else, at
/// the album folder, the folder of another group holding the same album), the file name, and the
/// path fitted to the budget. A folder of another spelling already there holds the song.
fn resolve(
    root: &str,
    songs: &mut [Song],
    groups: &[Group],
    disk: &LayoutDisk,
    options: &LayoutOptions,
    out: &mut Out,
) {
    for at in 0..songs.len() {
        if songs[at].held.is_some() {
            continue;
        }
        if let Err(folder) = place(root, &mut songs[at], disk) {
            let rel = rel_of(root, &folder);
            let shown = rel_of(root, &songs[at].file.path);
            out.skip(format!("folder-there|{shown}"), &shown, folder_there(&rel));
            songs[at].held = Some(folder_there(&rel));
        }
    }
    // An album whose folder would be new joins the folder another group of the same album goes
    // to under the same parent, as placement's album search does.
    let mut album_dirs: Vec<(usize, String, String)> = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        let Some(album) = group.album_segment else {
            continue;
        };
        let Some(song) = group
            .songs
            .iter()
            .map(|at| &songs[*at])
            .find(|song| song.held.is_none())
        else {
            continue;
        };
        let Some(level) = song.folders.iter().position(|name| name.segment == album) else {
            continue;
        };
        let dir = join_all(
            root,
            &song.folders[..=level]
                .iter()
                .map(|n| n.name.clone())
                .collect::<Vec<_>>(),
        );
        album_dirs.push((index, parent_of(&dir), name_of(&dir)));
    }
    let mut under_parent: HashMap<&str, Vec<usize>> = HashMap::new();
    for (at, (_, parent, _)) in album_dirs.iter().enumerate() {
        under_parent.entry(parent.as_str()).or_default().push(at);
    }
    for (index, parent, name) in &album_dirs {
        let dir = format!("{parent}/{name}");
        let current = songs[groups[*index].songs[0]].folder.clone();
        if disk.folders.contains_key(&dir) || dir == current {
            continue;
        }
        let candidates: Vec<Candidate> = under_parent
            .get(parent.as_str())
            .into_iter()
            .flatten()
            .map(|at| &album_dirs[*at])
            .filter(|(other, _, _)| other != index)
            .map(|(other, _, other_name)| Candidate {
                name: other_name.clone(),
                identity: groups[*other].identity.clone(),
                audio_files: groups[*other].songs.len(),
            })
            .collect();
        let group = &groups[*index];
        let Some(chosen) = choose(&group.identity, &candidates, group.shown) else {
            continue;
        };
        if chosen.name == *name {
            continue;
        }
        let Some(album) = group.album_segment else {
            continue;
        };
        for at in &group.songs {
            let song = &mut songs[*at];
            if song.held.is_some() {
                continue;
            }
            if let Some(level) = song.folders.iter().position(|n| n.segment == album) {
                song.folders[level].name = chosen.name.clone();
                song.folders[level].uncut = chosen.name.clone();
            }
            if let Err(folder) = place(root, song, disk) {
                let rel = rel_of(root, &folder);
                let shown = rel_of(root, &song.file.path);
                out.skip(format!("folder-there|{shown}"), &shown, folder_there(&rel));
                song.held = Some(folder_there(&rel));
            }
        }
    }
    // The file name: the current spelling when only that differs, a `Name (n)` kept beside another
    // song, and fitted to the budget.
    for song in songs.iter_mut() {
        if song.held.is_some() {
            continue;
        }
        let Some(dir) = song.wanted.clone() else {
            continue;
        };
        let template = options.layout.template(groups[song.group].kind);
        let mut stem = song.file_name.name.clone();
        if same_key(&stem, &song.stem) {
            stem = PathHelper::safe_name(&clean_text(&song.stem), Some(&song.ext));
        } else if dir == song.folder
            && let Some(found) = COPY_NUMBER.captures(&song.stem)
            && same_key(&found[1], &stem)
        {
            stem = song.stem.clone();
        }
        let folders: Vec<String> = relative(root, &format!("{dir}/x"))
            .map(|mut segments| {
                segments.pop();
                segments
            })
            .unwrap_or_default();
        let used =
            folders.iter().map(|name| utf16_len(name)).sum::<usize>() + folders.len() + utf16_len(&song.ext);
        if used + utf16_len(&stem) > options.budget {
            let room = options.budget.saturating_sub(used).max(40);
            stem = template.render_file_within(&song.values, &song.ext, room);
        }
        song.wanted = Some(format!("{dir}/{stem}{}", song.ext));
    }
}

/// The song's target folder on disk, kept in `wanted`: each folder the same name as one there,
/// or the song's own folder renamed (another spelling of it), or new. Err with the folder of
/// another spelling that is already there.
fn place(root: &str, song: &mut Song, disk: &LayoutDisk) -> Result<(), String> {
    let mut dir = root.to_string();
    let mut there = true;
    for (level, name) in song.folders.iter().enumerate() {
        let path = format!("{dir}/{}", name.name);
        if there {
            let listing = disk.folders.get(&dir).map(Vec::as_slice).unwrap_or(&[]);
            if listing
                .iter()
                .any(|(entry, is_dir)| *is_dir && *entry == name.name)
            {
                dir = path;
                continue;
            }
            let own = song
                .chain
                .get(level)
                .filter(|_| song.chain.len() > level && join_all(root, &song.chain[..level]) == dir);
            let other = listing.iter().find(|(entry, is_dir)| {
                *is_dir
                    && Some(entry) != own
                    && (fold_name(entry, None) == fold_name(&name.name, None)
                        || fold_name(entry, None) == fold_name(&name.uncut, None)
                        || (name.artist_level && {
                            let key = SongIdentity::key(entry);
                            key.chars().count() >= 3 && key == SongIdentity::key(&name.uncut)
                        }))
            });
            if let Some((entry, _)) = other {
                return Err(format!("{dir}/{entry}"));
            }
            there = false;
        }
        dir = path;
    }
    song.wanted = Some(dir);
    Ok(())
}

// ---- operations -------------------------------------------------------------------------------

/// One move as planned, before rows are made.
struct Planned {
    movement: TidyFileMove,
    /// The song it belongs to.
    song: usize,
    sidecar: bool,
}

#[allow(clippy::too_many_lines)]
fn operations(
    root: &str,
    files: &[LayoutFile],
    songs: &mut [Song],
    groups: &[Group],
    disk: &LayoutDisk,
    options: &LayoutOptions,
    out: &mut Out,
) -> LayoutPlan {
    // Every audio file the walk read, by folder: a folder renames whole only when each moves.
    let mut audio_in: HashMap<String, usize> = HashMap::new();
    let mut with_audio: HashSet<String> = HashSet::new();
    for file in files {
        let folder = parent_of(&file.path);
        *audio_in.entry(folder.clone()).or_default() += 1;
        let mut up = parent_of(&folder);
        while is_under(&up, root) && up != root {
            with_audio.insert(up.clone());
            up = parent_of(&up);
        }
    }

    // Reasons a song stays that hold its album too.
    let lidarr_dirs: HashSet<String> = options
        .lidarr
        .tracked
        .iter()
        .chain(&options.lidarr.maybe)
        .map(|path| parent_of(path))
        .collect();
    for song in songs.iter_mut() {
        if song.held.is_some() {
            continue;
        }
        if let Some(why) = song.lidarr {
            song.held = Some(why.to_string());
        } else if !song.in_scope {
            song.held = Some(String::new());
        } else if let Some(target) = &song.wanted
            && parent_of(target) != song.folder
            && lidarr_dirs.contains(&parent_of(target))
        {
            song.held = Some(LIDARR_HERE.to_string());
        } else if let (Some(follow), Some(target)) = (&options.follow, &song.wanted)
            && *target != song.file.path
        {
            let artists = [
                song.values.artist.clone(),
                song.values.track_artist.clone(),
                song.values.album_artist.clone().unwrap_or_default(),
            ];
            if follow(&artists, &song.values.album, target) {
                song.held = Some(FOLLOW.to_string());
            }
        }
    }

    loop {
        hold_whole_albums(root, songs, groups, out);
        let (renames, moves) = build_moves(root, songs, disk, options, &audio_in, &with_audio, out);
        // A song held by a collision may hold its album, and its folder no longer moves whole.
        if !hold_collisions(root, songs, disk, &renames, &moves, out) {
            let mut plan = finish(root, songs, disk, renames, moves);
            if options.pool {
                plan.plan.copies.clear();
            }
            plan.plan.rows.append(&mut out.rows);
            return plan;
        }
    }
}

/// Reasons a song stays that are its own, shown on its own row.
const OWN_REASONS: [&str; 4] = [LIDARR_KEEPS, LIDARR_MAY, LIDARR_HERE, FOLLOW];

/// A group whose folder changes moves whole: any song of it that stays holds every song.
fn hold_whole_albums(root: &str, songs: &mut [Song], groups: &[Group], out: &mut Out) {
    for group in groups {
        let moves_folder = group.songs.iter().any(|at| {
            let song = &songs[*at];
            song.wanted
                .as_deref()
                .is_some_and(|target| parent_of(target) != song.folder)
        });
        let staying: Vec<usize> = group
            .songs
            .iter()
            .copied()
            .filter(|at| songs[*at].held.is_some())
            .collect();
        let moving: Vec<usize> = group
            .songs
            .iter()
            .copied()
            .filter(|at| songs[*at].held.is_none())
            .collect();
        for at in &staying {
            let song = &songs[*at];
            if let Some(why) = song.held.as_deref().filter(|why| OWN_REASONS.contains(why))
                && song.in_scope
            {
                let shown = rel_of(root, &song.file.path);
                out.skip(format!("stays|{shown}"), &shown, why.to_string());
            }
        }
        if !moves_folder || staying.is_empty() || moving.is_empty() {
            continue;
        }
        let out_of_run = staying.iter().any(|at| !songs[*at].in_scope);
        let lidarr = staying.iter().find_map(|at| songs[*at].lidarr);
        let why = if let Some(words) = lidarr {
            words.to_string()
        } else if out_of_run {
            OUT_OF_RUN.to_string()
        } else {
            ALBUM_STAYS.to_string()
        };
        let folder = rel_of(root, &songs[group.songs[0]].folder);
        out.skip(format!("album|{folder}"), &folder, why.clone());
        for at in moving {
            songs[at].held = Some(why.clone());
        }
    }
}

/// What every song's target asks for, folder by folder: a whole-folder rename where every audio
/// file of the folder goes to one new folder, else file moves; each with the files beside it that
/// take its name.
fn build_moves(
    root: &str,
    songs: &mut [Song],
    disk: &LayoutDisk,
    options: &LayoutOptions,
    audio_in: &HashMap<String, usize>,
    with_audio: &HashSet<String>,
    out: &mut Out,
) -> (Vec<TidyMove>, Vec<Planned>) {
    let mut by_folder: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (at, song) in songs.iter().enumerate() {
        by_folder.entry(song.folder.clone()).or_default().push(at);
    }
    let target_of = |song: &Song| song.wanted.clone().filter(|_| song.held.is_none());
    let mut renames: Vec<TidyMove> = Vec::new();
    let mut moves: Vec<Planned> = Vec::new();
    for (folder, members) in &by_folder {
        let moving = members
            .iter()
            .any(|at| target_of(&songs[*at]).is_some_and(|target| target != songs[*at].file.path));
        if !moving {
            continue;
        }
        let row = format!("layout|{}", rel_of(root, folder));
        let targets: Vec<Option<String>> = members.iter().map(|at| target_of(&songs[*at])).collect();
        let dirs: BTreeSet<String> = targets.iter().flatten().map(|target| parent_of(target)).collect();
        let every =
            targets.iter().all(Option::is_some) && audio_in.get(folder).copied() == Some(members.len());
        let to = dirs.iter().next().cloned().unwrap_or_default();
        let renames_whole = every
            && !options.pool
            && dirs.len() == 1
            && to != *folder
            && *folder != root
            && !with_audio.contains(folder)
            && !options.lidarr.artist_folders.iter().any(|kept| kept == folder)
            && !is_under(&to, folder)
            && !folder_exists(&to, folder, disk, &renames);
        // In the pool the files beside a song go with it on their own.
        let listing = match disk.folders.get(folder) {
            Some(listing) if !options.pool => listing.as_slice(),
            _ => &[],
        };
        if renames_whole {
            if options.case_insensitive
                && parent_of(&to) == parent_of(folder)
                && name_of(&to).to_lowercase() == name_of(folder).to_lowercase()
            {
                let rel = rel_of(root, folder);
                out.skip(format!("case|{rel}"), &rel, CASE_ONLY);
                for at in members {
                    songs[*at].held = Some(CASE_ONLY.to_string());
                }
                continue;
            }
            renames.push(TidyMove {
                from: folder.clone(),
                to: to.clone(),
                row: row.clone(),
                files: members.iter().map(|at| songs[*at].file.path.clone()).collect(),
            });
            for at in members {
                let Some(target) = songs[*at].wanted.clone() else {
                    continue;
                };
                let inside = format!("{to}/{}{}", songs[*at].stem, songs[*at].ext);
                if inside != target {
                    let after = Some(folder.clone());
                    push_song(
                        &mut moves,
                        *at,
                        songs,
                        (&inside, &target),
                        &row,
                        after,
                        (listing, &to),
                        out,
                        root,
                    );
                }
            }
            continue;
        }
        for (at, target) in members.iter().zip(targets) {
            let Some(target) = target.filter(|target| *target != songs[*at].file.path) else {
                continue;
            };
            let from = songs[*at].file.path.clone();
            push_song(
                &mut moves,
                *at,
                songs,
                (&from, &target),
                &row,
                None,
                (listing, folder),
                out,
                root,
            );
        }
    }
    // A move into a folder another folder is renamed to waits for that rename.
    let renamed: HashMap<&str, &str> = renames
        .iter()
        .map(|rename| (rename.to.as_str(), rename.from.as_str()))
        .collect();
    for planned in &mut moves {
        if planned.movement.after.is_none()
            && let Some(from) = renamed.get(parent_of(&planned.movement.to).as_str())
        {
            planned.movement.after = Some(from.to_string());
        }
    }
    (renames, moves)
}

/// A song's move and, right after it, each file beside it that takes its name.
/// `(from, to)` is the song's move; `(listing, from_dir)` the listing of the folder it was in and
/// where its files are once any rename of that folder is done.
#[allow(clippy::too_many_arguments)]
fn push_song(
    moves: &mut Vec<Planned>,
    at: usize,
    songs: &[Song],
    (from, to): (&str, &str),
    row: &str,
    after: Option<String>,
    (listing, from_dir): (&[(String, bool)], &str),
    out: &mut Out,
    root: &str,
) {
    let song = &songs[at];
    moves.push(Planned {
        movement: TidyFileMove {
            from: from.to_string(),
            to: to.to_string(),
            row: row.to_string(),
            song: None,
            after: after.clone(),
        },
        song: at,
        sidecar: false,
    });
    let stems: Vec<String> = listing
        .iter()
        .filter(|(name, is_dir)| !is_dir && is_audio_name(name))
        .map(|(name, _)| split_name(name).0)
        .collect();
    let shared = stems.iter().filter(|stem| **stem == song.stem).count() > 1;
    let (to_dir, to_name) = (parent_of(to), name_of(to));
    let (to_stem, _) = split_name(&to_name);
    for (name, is_dir) in listing {
        if *is_dir || is_audio_name(name) || !name.starts_with(&format!("{}.", song.stem)) {
            continue;
        }
        // A file named for a longer song name in the folder is that song's.
        if stems
            .iter()
            .any(|stem| stem.len() > song.stem.len() && name.starts_with(&format!("{stem}.")))
        {
            continue;
        }
        if shared {
            let rel = rel_of(root, &format!("{}/{}", song.folder, song.stem));
            out.skip(format!("shared|{rel}"), &rel, shared_stem(&song.stem));
            continue;
        }
        let suffix = &name[song.stem.len()..];
        moves.push(Planned {
            movement: TidyFileMove {
                from: format!("{from_dir}/{name}"),
                to: format!("{to_dir}/{to_stem}{suffix}"),
                row: row.to_string(),
                song: Some(from.to_string()),
                after: after.clone(),
            },
            song: at,
            sidecar: true,
        });
    }
}

/// Whether `to` is already there, by its name or another spelling, other than `from` itself, once
/// the renames planned so far are done.
fn folder_exists(to: &str, from: &str, disk: &LayoutDisk, renames: &[TidyMove]) -> bool {
    if renames.iter().any(|rename| rename.to == to) {
        return true;
    }
    let parent = parent_of(to);
    let folded = fold_name(&name_of(to), None);
    disk.folders.get(&parent).is_some_and(|listing| {
        listing.iter().any(|(name, _)| {
            let path = format!("{parent}/{name}");
            path != from
                && !renames.iter().any(|rename| rename.from == path)
                && fold_name(name, None) == folded
        })
    })
}

/// Holds songs whose target another move also wants, or a file there that does not move takes,
/// and songs whose lyrics' target is taken. True when it held any.
fn hold_collisions(
    root: &str,
    songs: &mut [Song],
    disk: &LayoutDisk,
    renames: &[TidyMove],
    moves: &[Planned],
    out: &mut Out,
) -> bool {
    let leaving: HashSet<&str> = moves
        .iter()
        .map(|planned| planned.movement.from.as_str())
        .chain(
            renames
                .iter()
                .flat_map(|rename| rename.files.iter().map(String::as_str)),
        )
        .collect();
    let mut by_target: BTreeMap<String, Vec<&Planned>> = BTreeMap::new();
    for planned in moves {
        by_target
            .entry(fold_path(&planned.movement.to))
            .or_default()
            .push(planned);
    }
    let mut held: Vec<(usize, String, String)> = Vec::new();
    for planned in by_target.values() {
        if planned.len() > 1 {
            let names: Vec<String> = planned
                .iter()
                .map(|p| name_of(&songs[p.song].file.path))
                .collect();
            let target = name_of(&planned[0].movement.to);
            for p in planned {
                held.push((p.song, "twins".into(), two_songs(&names[0], &names[1], &target)));
            }
        }
    }
    let renamed: HashMap<&str, &str> = renames
        .iter()
        .map(|rename| (rename.to.as_str(), rename.from.as_str()))
        .collect();
    // Each target folder's entries once the renames are done, by their folded names.
    let mut folded: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    for planned in moves {
        let target_dir = parent_of(&planned.movement.to);
        let name = name_of(&planned.movement.to);
        let entries = folded.entry(target_dir.clone()).or_insert_with(|| {
            let source = renamed
                .get(target_dir.as_str())
                .map_or(target_dir.as_str(), |from| *from);
            let mut by_fold: HashMap<String, Vec<String>> = HashMap::new();
            for (entry, _) in disk.folders.get(source).map(Vec::as_slice).unwrap_or(&[]) {
                by_fold.entry(fold_file(entry)).or_default().push(entry.clone());
            }
            by_fold
        });
        let blocker = entries.get(&fold_file(&name)).and_then(|alike| {
            alike.iter().find(|entry| {
                let path = format!("{target_dir}/{entry}");
                path != planned.movement.from && !leaving.contains(path.as_str())
            })
        });
        if let Some(entry) = blocker {
            let why = if planned.sidecar {
                sidecar_taken(entry)
            } else {
                taken(entry)
            };
            held.push((planned.song, "taken".into(), why));
        }
    }
    let mut any = false;
    for (at, kind, why) in held {
        if songs[at].held.is_some() {
            continue;
        }
        let shown = rel_of(root, &songs[at].file.path);
        out.skip(format!("{kind}|{shown}"), &shown, why.clone());
        songs[at].held = Some(why);
        any = true;
    }
    any
}

/// The plan's moves and rows: one Layout row per folder songs leave or are renamed in, each with
/// the fingerprint an apply compares.
/// The moves in an order that frees each name before another move takes it (a song renumbered
/// onto the name the next one leaves), each song still followed by the files beside it. Moves in
/// a ring keep their order; apply holds what it cannot do.
fn freeing_names_first(moves: Vec<Planned>) -> Vec<Planned> {
    let mut units: Vec<Vec<Planned>> = Vec::new();
    for planned in moves {
        match units.last_mut() {
            Some(unit) if planned.sidecar => unit.push(planned),
            _ => units.push(vec![planned]),
        }
    }
    let source_of: HashMap<String, usize> = units
        .iter()
        .enumerate()
        .flat_map(|(at, unit)| unit.iter().map(move |p| (p.movement.from.clone(), at)))
        .collect();
    // Each unit waits for the units whose files sit where it goes.
    let mut waits = vec![0usize; units.len()];
    let mut frees: Vec<Vec<usize>> = vec![Vec::new(); units.len()];
    for (at, unit) in units.iter().enumerate() {
        let blockers: BTreeSet<usize> = unit
            .iter()
            .filter_map(|p| source_of.get(&p.movement.to).copied())
            .filter(|blocker| *blocker != at)
            .collect();
        waits[at] = blockers.len();
        for blocker in blockers {
            frees[blocker].push(at);
        }
    }
    if waits.iter().all(|count| *count == 0) {
        return units.into_iter().flatten().collect();
    }
    let mut ready: BTreeSet<usize> = (0..units.len()).filter(|at| waits[*at] == 0).collect();
    let mut order: Vec<usize> = Vec::with_capacity(units.len());
    while let Some(at) = ready.pop_first() {
        order.push(at);
        for next in &frees[at] {
            waits[*next] -= 1;
            if waits[*next] == 0 {
                ready.insert(*next);
            }
        }
    }
    let placed: HashSet<usize> = order.iter().copied().collect();
    order.extend((0..units.len()).filter(|at| !placed.contains(at)));
    let mut slots: Vec<Option<Vec<Planned>>> = units.into_iter().map(Some).collect();
    order
        .into_iter()
        .flat_map(|at| slots[at].take().unwrap_or_default())
        .collect()
}

fn finish(
    root: &str,
    songs: &[Song],
    disk: &LayoutDisk,
    renames: Vec<TidyMove>,
    moves: Vec<Planned>,
) -> LayoutPlan {
    let moves = freeing_names_first(moves);
    let mut copies: Vec<TidyCopy> = Vec::new();
    // Covers go with an album that leaves its folder by file moves: copied, never moved, into a
    // folder that has none.
    let mut covered: HashSet<String> = HashSet::new();
    let mut by_folder: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (at, song) in songs.iter().enumerate() {
        by_folder.entry(song.folder.clone()).or_default().push(at);
    }
    for (folder, members) in &by_folder {
        if renames.iter().any(|rename| rename.from == *folder) {
            continue;
        }
        let leaves = members.iter().all(|at| {
            songs[*at]
                .wanted
                .as_deref()
                .is_some_and(|target| songs[*at].held.is_none() && parent_of(target) != *folder)
        });
        if !leaves {
            continue;
        }
        let listing = disk.folders.get(folder).map(Vec::as_slice).unwrap_or(&[]);
        let covers: Vec<&String> = listing
            .iter()
            .filter(|(name, is_dir)| !is_dir && is_cover(name))
            .map(|(name, _)| name)
            .collect();
        if covers.is_empty() {
            continue;
        }
        let row = format!("layout|{}", rel_of(root, folder));
        let dirs: BTreeSet<String> = members
            .iter()
            .filter_map(|at| songs[*at].wanted.as_deref().map(parent_of))
            .collect();
        for dir in dirs {
            let has = disk
                .folders
                .get(&dir)
                .is_some_and(|listing| listing.iter().any(|(name, is_dir)| !is_dir && is_cover(name)));
            if has || !covered.insert(dir.clone()) {
                continue;
            }
            for cover in &covers {
                copies.push(TidyCopy {
                    from: format!("{folder}/{cover}"),
                    to: format!("{dir}/{cover}"),
                    row: row.clone(),
                });
            }
        }
    }

    let mut rows: BTreeMap<String, TidyRow> = BTreeMap::new();
    let mut lines: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let final_of: HashMap<&str, &str> = moves
        .iter()
        .filter(|planned| !planned.sidecar)
        .map(|planned| {
            (
                songs[planned.song].file.path.as_str(),
                planned.movement.to.as_str(),
            )
        })
        .collect();
    for rename in &renames {
        let rel = rel_of(root, &rename.from);
        let example = rename
            .files
            .first()
            .map(|file| {
                final_of
                    .get(file.as_str())
                    .map_or_else(|| format!("{}/{}", rename.to, name_of(file)), |to| to.to_string())
            })
            .unwrap_or_default();
        rows.insert(
            rename.row.clone(),
            layout_row(
                &rename.row,
                &rel,
                &rel_of(root, &rename.to),
                rename.files.len(),
                &rel_of(root, &example),
            ),
        );
        lines.entry(rename.row.clone()).or_default().push(format!(
            "r|{}|{}|{}",
            rename.from,
            rename.to,
            rename.files.join("\u{1f}")
        ));
    }
    for planned in &moves {
        let row = &planned.movement.row;
        lines
            .entry(row.clone())
            .or_default()
            .push(format!("f|{}|{}", planned.movement.from, planned.movement.to));
        if planned.sidecar {
            continue;
        }
        let folder = &songs[planned.song].folder;
        let entry = rows.entry(row.clone()).or_insert_with(|| {
            layout_row(
                row,
                &rel_of(root, folder),
                &rel_of(root, &parent_of(&planned.movement.to)),
                0,
                &rel_of(root, &planned.movement.to),
            )
        });
        if !renames.iter().any(|rename| rename.row == *row) {
            entry.files += 1;
        }
    }
    for copy in &copies {
        lines
            .entry(copy.row.clone())
            .or_default()
            .push(format!("c|{}|{}", copy.from, copy.to));
    }
    let mut row_hashes = BTreeMap::new();
    for row in rows.values_mut() {
        let songs = if row.files == 1 {
            "1 song moves".to_string()
        } else {
            format!("{} songs move", row.files)
        };
        let example = row
            .evidence
            .strip_prefix(&format!("{}/", row.after))
            .unwrap_or(&row.evidence)
            .to_string();
        row.evidence = format!("{songs} into the layout, for example {example}");
        let mut text = lines.remove(&row.id).unwrap_or_default();
        text.sort();
        row_hashes.insert(
            row.id.clone(),
            hex::encode_upper(Sha256::digest(text.join("\n").as_bytes()))[..16].to_string(),
        );
    }
    LayoutPlan {
        plan: TidyPlan {
            rows: rows.into_values().collect(),
            moves: renames,
            file_moves: moves.into_iter().map(|planned| planned.movement).collect(),
            copies,
            ..TidyPlan::default()
        },
        row_hashes,
    }
}

/// What a pool row's path starts with: Leave out takes `pool:<folder>`.
pub const POOL_PREFIX: &str = "pool:";

/// A plan of the family pool as the library's plan carries it: its rows named `pool|...` (its
/// left-alone rows `pool-...`), their paths `pool:<folder>`, and its file moves as pool moves.
pub fn as_pool_rows(mut planned: LayoutPlan) -> LayoutPlan {
    let rename = |id: &str| match id.strip_prefix("layout|") {
        Some(rest) => format!("pool|{rest}"),
        None => format!("pool-{id}"),
    };
    for row in &mut planned.plan.rows {
        row.id = rename(&row.id);
        row.path = format!("{POOL_PREFIX}{}", row.path);
    }
    let mut moves = std::mem::take(&mut planned.plan.file_moves);
    for movement in &mut moves {
        movement.row = rename(&movement.row);
    }
    planned.plan.pool_moves = moves;
    planned.plan.moves.clear();
    planned.plan.copies.clear();
    planned.row_hashes = planned
        .row_hashes
        .into_iter()
        .map(|(id, hash)| (rename(&id), hash))
        .collect();
    planned
}

/// The excluded folders of the pool, from the excluded list (`pool:<folder>`).
pub fn pool_excluded(excluded: &[String]) -> Vec<String> {
    excluded
        .iter()
        .filter_map(|folder| folder.trim().strip_prefix(POOL_PREFIX))
        .map(str::to_string)
        .collect()
}

fn layout_row(id: &str, before: &str, after: &str, files: usize, example: &str) -> TidyRow {
    TidyRow {
        id: id.to_string(),
        kind: TidyKind::Layout,
        path: before.to_string(),
        before: before.to_string(),
        after: after.to_string(),
        evidence: example.to_string(),
        files,
        opt_in: false,
        ticked: true,
    }
}

// ---- Lidarr -----------------------------------------------------------------------------------

/// What Lidarr keeps, in Octo's paths. Files under the Lidarr root that is Octo's music folder
/// (the one Octo's settings name, or Lidarr's only root) are Lidarr's. A file under a root Octo
/// cannot see is matched by its last three path parts (artist folder, album folder, file name)
/// against the files the walk read: a match of the same size, or one Octo has no record of, may be
/// Lidarr's and stays; a file Octo placed whose size differs is Octo's. No match is outside Octo's
/// view.
pub fn lidarr_view(
    root: &str,
    octo_root: Option<&str>,
    answer: &TidyLidarr,
    files: &[LayoutFile],
) -> LidarrView {
    let mut by_name: HashMap<&str, Vec<&LayoutFile>> = HashMap::new();
    for file in files {
        by_name
            .entry(file.path.rsplit('/').next().unwrap_or(""))
            .or_default()
            .push(file);
    }
    let mut view = LidarrView::default();
    for (_, path) in &answer.artists {
        if let Some(folder) = lidarr_path_in_octo(root, octo_root, &answer.roots, path) {
            view.artist_folders.push(folder);
        }
    }
    for (path, size) in &answer.files {
        if let Some(octo) = lidarr_path_in_octo(root, octo_root, &answer.roots, path) {
            view.tracked.insert(octo);
            continue;
        }
        let tail: Vec<&str> = path.rsplit('/').take(3).collect();
        for file in by_name.get(tail[0]).into_iter().flatten() {
            let theirs: Vec<&str> = file.path.rsplit('/').take(tail.len()).collect();
            if theirs == tail && (file.size == *size || file.mapping.is_none()) {
                view.maybe.insert(file.path.clone());
            }
        }
    }
    view
}

/// Where a path of Lidarr's is in Octo's view: its place under the Lidarr root that is Octo's
/// music folder (the one Octo's settings name, else Lidarr's only root), taken onto `root`. None
/// for a path under any other root.
pub fn lidarr_path_in_octo(
    root: &str,
    octo_root: Option<&str>,
    roots: &[String],
    path: &str,
) -> Option<String> {
    let root = root.trim_end_matches('/');
    let lidarr = octo_root
        .map(str::to_string)
        .filter(|path| !path.trim().is_empty())
        .or_else(|| (roots.len() == 1).then(|| roots[0].clone()))?;
    let lidarr = lidarr.trim_end_matches('/');
    let rest = path.trim_end_matches('/').strip_prefix(lidarr)?;
    if rest.is_empty() {
        return Some(root.to_string());
    }
    rest.strip_prefix('/').map(|rest| format!("{root}/{rest}"))
}

// ---- helpers ----------------------------------------------------------------------------------

/// `text` cleaned as every rendered value is (the characters Windows refuses as their look-alikes),
/// never cut.
fn clean_text(text: &str) -> String {
    PathHelper::clean_name(text)
}

/// A current folder name kept as it is spelled, cleaned and cut to `cap` UTF-16 units at a word
/// break within the last 15 (never below 40), as a rendered folder name is.
fn cleaned_folder(current: &str, cap: usize) -> String {
    let name = PathHelper::safe_name(&clean_text(current), None);
    if utf16_len(&name) <= cap || cap == 0 {
        return name;
    }
    let mut kept = 0;
    let cut: String = name
        .chars()
        .take_while(|c| {
            kept += c.len_utf16();
            kept <= cap
        })
        .collect();
    let at_word = cut.rfind(' ').filter(|at| {
        let kept = utf16_len(&cut[..*at]);
        kept + 15 >= cap && kept >= 40
    });
    let cut = match at_word {
        Some(at) => &cut[..at],
        None => cut.as_str(),
    };
    PathHelper::safe_name(cut, None)
}

/// Whether two names are one name by `SongIdentity::key` (never two empty keys).
fn same_key(a: &str, b: &str) -> bool {
    let key = SongIdentity::key(a);
    !key.is_empty() && key == SongIdentity::key(b)
}

/// A path's folders and file name folded as Windows and SMB compare names.
fn fold_path(path: &str) -> String {
    let (dir, name) = (parent_of(path), name_of(path));
    let folders: Vec<String> = dir.split('/').map(|part| fold_name(part, None)).collect();
    format!("{}/{}", folders.join("/"), fold_file(&name))
}

/// A file name folded as Windows and SMB compare it, the extension apart
/// ([`PathHelper::same_name`]).
fn fold_file(name: &str) -> String {
    let (stem, ext) = split_name(name);
    format!("{}{}", fold_name(&stem, Some(&ext)), ext.to_uppercase())
}

/// A cover or folder image beside an album: `cover.*` or `folder.*`, in any capitals.
fn is_cover(name: &str) -> bool {
    let (stem, ext) = split_name(name);
    !ext.is_empty() && !is_audio_name(name) && matches!(stem.to_lowercase().as_str(), "cover" | "folder")
}

/// A file name's stem and its extension with the dot (empty when it has none).
fn split_name(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(at) if at > 0 => (name[..at].to_string(), name[at..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(at) => path[..at].to_string(),
        None => String::new(),
    }
}

fn name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Whether `path` is `folder` or inside it.
fn is_under(path: &str, folder: &str) -> bool {
    let folder = folder.trim_end_matches('/');
    path == folder || path.starts_with(&format!("{folder}/"))
}

fn rel_of(root: &str, path: &str) -> String {
    relative(root, path)
        .map(|segments| segments.join("/"))
        .unwrap_or_default()
}

fn join_all(root: &str, names: &[String]) -> String {
    names
        .iter()
        .fold(root.to_string(), |dir, name| format!("{dir}/{name}"))
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

/// The name most often seen, ties to the first in ordinal order.
fn most_common_name(values: impl Iterator<Item = String>) -> Option<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    let top = counts.values().copied().max()?;
    counts
        .into_iter()
        .find(|(_, count)| *count == top)
        .map(|(name, _)| name)
}

fn most_common_chain(values: impl Iterator<Item = Vec<String>>) -> Option<Vec<String>> {
    let mut counts: BTreeMap<Vec<String>, usize> = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    let top = counts.values().copied().max()?;
    counts
        .into_iter()
        .find(|(_, count)| *count == top)
        .map(|(chain, _)| chain)
}

#[cfg(test)]
#[path = "layout_pass_tests.rs"]
mod tests;
