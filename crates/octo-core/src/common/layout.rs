//! File layouts: the templates that name a song's folders and file. The three built-in layouts
//! (Flat, ByArtist, Organized) are templates too, so every path Octo makes follows one set of
//! rules.
//!
//! A template is text with names in braces, `{Artist}/{Album}/{Track:00} - {Title}`. `/` starts a
//! folder (`\` is read as `/`). Names ignore case, spaces, `_` and `.`, and Lidarr's names are
//! read as their twins (`{Artist Name}`, `{Album Title}`, `{Track Title}`, `{track:00}`), with
//! Lidarr's text inside the braces (`{ (Release Year)}`) kept only when the name has a value. Only
//! Track and Disc take a number format: `:00` pads to two digits.
//!
//! A name with no value leaves no stray text. The rules read the template, never the values, left
//! to right: a folder whose names are all empty is left out; an empty name inside brackets whose
//! names are all empty takes the brackets, what they hold and the spaces before them; an empty
//! name at the start of a name, or right after an opening bracket, takes the separator after it;
//! any other empty name takes the separator before it.

use std::collections::BTreeSet;

use unicode_normalization::UnicodeNormalization;

use crate::common::dotnet::utf16_len;
use crate::common::path_helper::{PATH_BUDGET, POOL_BUDGET, PathHelper, built_in_caps, cut_name};

/// The shortest a folder name's room may be: below it a template is refused.
const MIN_FOLDER_CAP: usize = 30;

/// The fixed text of one folder or file name: everything but the names' values.
fn fixed_len(parts: &[Part]) -> usize {
    parts
        .iter()
        .map(|part| match part {
            Part::Text(text) => utf16_len(text),
            Part::Name { before, after, .. } => utf16_len(before) + utf16_len(after),
        })
        .sum()
}
use crate::settings::subsonic::{FolderStructure, SubsonicSettings};

/// Flat: `Artist - Title.flac`, every file in the music folder. The whole credit names the file,
/// since there is no folder to scatter (#49).
pub const FLAT: &str = "{Track Artist} - {Title}";
/// ByArtist: `Artist/Title.flac`, one folder per artist and no album folder.
pub const BY_ARTIST: &str = "{Artist}/{Title}";
/// Organized: `Artist/Album/01 - Title.flac`.
pub const ORGANIZED: &str = "{Artist}/{Album}/{Track:00} - {Title}";

/// The longest template the dashboard takes.
pub const MAX_TEMPLATE_CHARS: usize = 300;

/// The most folders a template may have: with folder names cut to at least 30 units and a file
/// name of at least 40, four is what fits the family pool's budget.
pub const MAX_FOLDERS: usize = 4;

/// The names a template may use, as the dashboard lists them.
pub const NAMES: &str =
    "{Artist}, {Album Artist}, {Track Artist}, {Album}, {Title}, {Track}, {Disc}, {Year}, {Album Type}";

/// Audio extensions: what a template may not end with (Octo adds each file's own), and what counts
/// as a song in a folder.
pub const AUDIO_EXTENSIONS: [&str; 14] = [
    ".flac", ".mp3", ".m4a", ".aac", ".alac", ".ogg", ".opus", ".wav", ".aiff", ".aif", ".ape", ".wv",
    ".wma", ".dsf",
];

/// Whether a file name ends in an audio extension, in any capitals.
pub fn is_audio_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    AUDIO_EXTENSIONS
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// What a name in braces stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The artist the folder is named after: the first credit when a source says which it is.
    Artist,
    /// The song's whole artist credit.
    TrackArtist,
    /// The album's artist, else the artist.
    AlbumArtist,
    /// The album; in a folder name the song's own title when it has none.
    Album,
    Title,
    Track,
    /// The disc, on a release with more than one.
    Disc,
    Year,
    /// Album, Single, EP, Compilation, Broadcast or Other.
    AlbumType,
}

impl Field {
    /// The field a name in braces means, or None for a name Octo does not know.
    fn named(name: &str) -> Option<Field> {
        let key: String = name
            .chars()
            .filter(|c| !matches!(c, ' ' | '_' | '.'))
            .flat_map(char::to_lowercase)
            .collect();
        Some(match key.as_str() {
            "artist" => Field::Artist,
            "trackartist" | "trackartistname" | "artists" => Field::TrackArtist,
            "albumartist" | "albumartistname" | "artistname" => Field::AlbumArtist,
            "album" | "albumtitle" => Field::Album,
            "title" | "tracktitle" => Field::Title,
            "track" | "tracknumber" => Field::Track,
            "disc" | "discnumber" | "medium" => Field::Disc,
            "year" | "releaseyear" => Field::Year,
            "albumtype" | "releasetype" => Field::AlbumType,
            _ => return None,
        })
    }

    fn takes_digits(self) -> bool {
        matches!(self, Field::Track | Field::Disc)
    }

    fn is_artist(self) -> bool {
        matches!(self, Field::Artist | Field::AlbumArtist)
    }

    /// A value that can differ between the tracks of one album.
    /// A release fact: one album's tracks can still disagree on it, so it may name the album's
    /// own folder (which keeps two releases apart) but nothing above it.
    fn varies_by_release(self) -> bool {
        matches!(self, Field::Year | Field::AlbumType)
    }

    /// A name whose value may be cut to fit a long path: text, never a number.
    fn shrinks(self) -> bool {
        !matches!(self, Field::Track | Field::Disc | Field::Year)
    }

    /// Different on every track of an album: only in the file name or a folder below the album's.
    fn varies_by_track(self) -> bool {
        matches!(
            self,
            Field::Disc | Field::Track | Field::Title | Field::TrackArtist
        )
    }
}

/// One piece of a folder or file name in a template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    /// A name, with the text Lidarr keeps inside its braces before and after it.
    Name {
        field: Field,
        digits: usize,
        before: String,
        after: String,
    },
}

/// Why a template cannot be used, in the dashboard's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError(pub String);

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A parsed template: its folder names, then its file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    text: String,
    segments: Vec<Vec<Part>>,
    /// The caps a folder this template names may have been cut to: its layout's
    /// ([`Layout::cut_caps`]), empty for a template parsed alone. Each rendered folder name
    /// carries them to [`PathHelper::settle`].
    cut_caps: Vec<usize>,
}

/// A rendered folder or file name: as it goes on disk (cut to the template's folder cap), and as
/// it was before the cut, which an older folder of the same name may still carry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenderedName {
    pub name: String,
    pub uncut: String,
    /// The template folder it came from (the file name is the last).
    pub segment: usize,
    /// A folder named by an artist alone, where another spelling of the artist's name (accents,
    /// punctuation) is the same folder.
    pub artist_level: bool,
    /// The cap the folder name was cut to (see [`Template::folder_cap`]).
    pub cap: usize,
    /// The other caps an older folder of this name may have been cut to, beside the built-in
    /// layouts' ([`Layout::cut_caps`]): [`PathHelper::settle`] knows a folder cut to one of them
    /// as this one. Empty knows the built-in caps alone.
    pub cut_caps: Vec<usize>,
}

/// Which template a song is filed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LayoutKind {
    #[default]
    Album,
    Single,
    Compilation,
}

impl LayoutKind {
    /// The kind of a song's release: a compilation by its flag or type, a single by its type or
    /// by having no album at all, anything else an album (an EP included).
    pub fn of(release_type: Option<&str>, compilation: bool, album_blank: bool) -> LayoutKind {
        let types: Vec<String> = release_type
            .unwrap_or("")
            .split(';')
            .map(|part| part.trim().to_lowercase())
            .filter(|part| !part.is_empty())
            .collect();
        if compilation || types.iter().any(|kind| kind == "compilation") {
            LayoutKind::Compilation
        } else if album_blank || types.first().is_some_and(|kind| kind == "single") {
            LayoutKind::Single
        } else {
            LayoutKind::Album
        }
    }
}

/// The word `{Album Type}` gives a release: Compilation for one, else its primary type, else None.
pub fn album_type_word(release_type: Option<&str>, compilation: bool) -> Option<String> {
    let types: Vec<String> = release_type
        .unwrap_or("")
        .split(';')
        .map(|part| part.trim().to_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    if compilation || types.iter().any(|kind| kind == "compilation") {
        return Some("Compilation".to_string());
    }
    Some(
        match types.first()?.as_str() {
            "album" => "Album",
            "single" => "Single",
            "ep" => "EP",
            "broadcast" => "Broadcast",
            _ => "Other",
        }
        .to_string(),
    )
}

/// What a song's names are filled from. Kept with each Octo download, so the File layout task
/// files it again from what placement used.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LayoutValues {
    pub artist: String,
    pub track_artist: String,
    pub album_artist: Option<String>,
    pub album: String,
    pub title: String,
    pub track: Option<i32>,
    /// The disc number as the release said, and how many discs it has.
    pub disc: Option<i32>,
    pub disc_count: Option<i32>,
    /// Whether `{Disc}` shows: decided by the caller from the album as a whole (a missing disc
    /// number then shows as 1).
    pub multi_disc: bool,
    pub year: Option<i32>,
    pub album_type: Option<String>,
    /// The kind whose template the song was filed by: its own, or the kind whose folder already
    /// held its album.
    pub kind: LayoutKind,
    /// The MusicBrainz release group, when the release is the album the folder is named after.
    pub release_group: Option<String>,
    /// The tags as they were written, so a later edit of any of them is seen and wins.
    pub tags: Option<TagSnapshot>,
}

/// The tags that feed a song's layout, as written when it was placed: names by
/// `SongIdentity::key`, numbers as they are. A field whose tag now says something else is taken
/// from the tag, not from the saved values.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TagSnapshot {
    pub title: String,
    pub album: String,
    pub artist: String,
    pub album_artist: String,
    pub track: Option<i32>,
    pub disc: Option<i32>,
    pub disc_count: Option<i32>,
    pub year: Option<i32>,
    pub release_type: String,
    pub compilation: bool,
    pub release_group: String,
}

impl TagSnapshot {
    /// The layout's tags as the tag writer writes them from `song`.
    pub fn of_song(song: &crate::models::domain::song::Song) -> TagSnapshot {
        use crate::common::SongIdentity;
        let text = |value: Option<&str>| SongIdentity::key(value.unwrap_or(""));
        TagSnapshot {
            title: SongIdentity::key(&song.title),
            album: SongIdentity::key(&song.album),
            artist: SongIdentity::key(&song.artist),
            album_artist: text(song.album_artist.as_deref()),
            track: song.track.filter(|n| *n > 0),
            disc: song.disc_number.filter(|n| *n > 0),
            disc_count: song.total_discs.filter(|n| *n > 0),
            year: song_year(song),
            release_type: song.release_type.as_deref().unwrap_or("").trim().to_lowercase(),
            compilation: song.is_compilation,
            release_group: song
                .music_brainz_release_group_id
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_lowercase(),
        }
    }
}

/// A song's year: its own, else the year of its release date.
pub fn song_year(song: &crate::models::domain::song::Song) -> Option<i32> {
    song.year.filter(|year| *year > 0).or_else(|| {
        song.release_date
            .as_deref()
            .and_then(|date| date.get(..4))
            .and_then(|year| year.parse::<i32>().ok())
            .filter(|year| *year > 0)
    })
}

impl LayoutValues {
    /// Whether the song's own release says it is one of several discs.
    pub fn says_multi_disc(&self) -> bool {
        self.disc.is_some_and(|disc| disc > 1) || self.disc_count.is_some_and(|count| count > 1)
    }
}

const SEPARATORS: [char; 8] = [' ', '-', '_', '.', ',', '~', '\u{2013}', '\u{2014}'];

/// Opening brackets and the closing bracket each one pairs with.
const BRACKETS: [(char, char); 2] = [('(', ')'), ('[', ']')];

/// What Lidarr allows inside braces before a name, and after it.
const BEFORE_NAME: [char; 6] = ['-', ' ', '.', '_', '[', '('];
const AFTER_NAME: [char; 6] = ['-', ' ', '.', '_', ']', ')'];

/// Lidarr names Octo has no value for, and what to use instead.
const LIDARR_ONLY: [(&str, &str); 3] = [
    ("mediumformat", "Use Disc {Disc} for a disc folder."),
    ("mediumname", "Use Disc {Disc} for a disc folder."),
    (
        "qualityfull",
        "A replacement keeps the original's name, so the quality would be wrong.",
    ),
];

impl Template {
    /// The template in `text`, or why it cannot be used.
    pub fn parse(text: &str) -> Result<Template, TemplateError> {
        let text: String = text.trim().nfc().collect();
        if text.is_empty() {
            return Err(TemplateError(
                "Type a layout, for example {Artist}/{Album}/{Track:00} - {Title}.".to_string(),
            ));
        }
        if text.chars().count() > MAX_TEMPLATE_CHARS {
            return Err(TemplateError(format!(
                "Keep the layout under {MAX_TEMPLATE_CHARS} characters."
            )));
        }
        let text = text.replace('\\', "/");
        if text.starts_with('/') {
            return Err(TemplateError(
                "Start with a name, not a /: the layout is inside the music folder.".to_string(),
            ));
        }
        if text.ends_with('/') {
            return Err(TemplateError("End with the file name, not a /.".to_string()));
        }
        if text.contains("//") {
            return Err(TemplateError(
                "Two / in a row make a folder with no name. Remove one.".to_string(),
            ));
        }
        let lower = text.to_lowercase();
        if let Some(extension) = AUDIO_EXTENSIONS
            .iter()
            .find(|extension| lower.ends_with(**extension))
        {
            return Err(TemplateError(format!(
                "Leave out {extension}: Octo adds each file's own extension."
            )));
        }
        let mut segments = Vec::new();
        let raws: Vec<&str> = text.split('/').collect();
        if raws.len() > MAX_FOLDERS + 1 {
            return Err(TemplateError(format!(
                "Use at most {MAX_FOLDERS} folders: deeper paths do not fit Windows' length."
            )));
        }
        for (at, raw) in raws.iter().enumerate() {
            if matches!(raw.trim(), "." | "..") {
                return Err(TemplateError("A folder can't be named . or ..".to_string()));
            }
            let parts = parse_segment(raw)?;
            check_typed_text(&parts, at + 1 == raws.len())?;
            segments.push(parts);
        }
        let template = Template {
            text,
            segments,
            cut_caps: Vec::new(),
        };
        let file = template
            .segments
            .last()
            .expect("split gives at least one segment");
        if !file.iter().any(|part| part.field() == Some(Field::Title)) {
            return Err(TemplateError(
                "Put {Title} in the file name, or the songs of one album would share one name.".to_string(),
            ));
        }
        if let Some(album) = template.album_folder_segment() {
            let fields = |segments: &[Vec<Part>]| -> Vec<Field> {
                segments.iter().flatten().filter_map(Part::field).collect()
            };
            if let Some(field) = fields(&template.segments[..=album])
                .into_iter()
                .find(|field| field.varies_by_track())
            {
                return Err(TemplateError(if field == Field::Disc {
                    "{Disc} changes track by track, so it can't be in the album folder's name or above it. Put the disc in its own folder below the album: {Album}/Disc {Disc}.".to_string()
                } else {
                    format!(
                        "{} changes track by track, so it can only be in the file name or a folder below the album.",
                        field.braces()
                    )
                }));
            }
            if let Some(field) = fields(&template.segments[..album])
                .into_iter()
                .find(|field| field.varies_by_release())
            {
                return Err(TemplateError(format!(
                    "{} can differ track by track, so it can't be above the album folder.",
                    field.braces()
                )));
            }
        }
        // Every path the template allows must fit, in the family pool's smaller budget too.
        let cap = template.folder_cap(POOL_BUDGET);
        let folders = &template.segments[..template.segments.len() - 1];
        if !folders.is_empty() && (cap < MIN_FOLDER_CAP || folders.iter().any(|parts| fixed_len(parts) > cap))
        {
            return Err(TemplateError(
                "This layout leaves too little room for names on Windows. Use fewer folders or less fixed text."
                    .to_string(),
            ));
        }
        Ok(template)
    }

    /// The template as it was typed, trimmed, with `/` between folders.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// How many folders the template has.
    pub fn folder_count(&self) -> usize {
        self.segments.len() - 1
    }

    /// The longest a folder name may be, in UTF-16 units, so the path fits `budget` with the file
    /// name's least room ([`Self::file_floor`]): the same for every song this template files, so
    /// one album's tracks always render one folder name. Organized: 91, and 74 in the family pool.
    pub fn folder_cap(&self, budget: usize) -> usize {
        let folders = self.folder_count().max(1);
        budget.saturating_sub(self.file_floor() + folders) / folders
    }

    /// The caps this template cuts folder names to, in the music folder's budget and the family
    /// pool's; none without a folder.
    pub fn own_caps(&self) -> Vec<usize> {
        if self.folder_count() == 0 {
            return Vec::new();
        }
        vec![self.folder_cap(PATH_BUDGET), self.folder_cap(POOL_BUDGET)]
    }

    /// The song's folder names and its file name without the extension, each made safe for disk
    /// and each folder name cut to [`Self::folder_cap`]. A folder whose names are all empty is
    /// left out. Each name carries the caps an older folder of it may have been cut to
    /// ([`RenderedName::cut_caps`]).
    pub fn render(&self, values: &LayoutValues, extension: &str, budget: usize) -> Vec<RenderedName> {
        let last = self.segments.len() - 1;
        let cap = self.folder_cap(budget);
        let mut names = Vec::with_capacity(self.segments.len());
        for (at, parts) in self.segments.iter().enumerate() {
            let is_file = at == last;
            let Some(rendered) = render_segment(parts, values, is_file) else {
                continue;
            };
            let uncut = PathHelper::safe_name(&rendered, is_file.then_some(extension));
            let name = if is_file {
                uncut.clone()
            } else {
                cut_name(&uncut, cap, None)
            };
            let names_in_it: Vec<Field> = parts.iter().filter_map(Part::field).collect();
            let artist_level = !is_file
                && !names_in_it.is_empty()
                && names_in_it
                    .iter()
                    .all(|field| field.is_artist() || *field == Field::TrackArtist);
            names.push(RenderedName {
                name,
                uncut,
                segment: at,
                artist_level,
                cap,
                cut_caps: self.cut_caps.clone(),
            });
        }
        names
    }

    /// The album folder: the first folder name holding `{Album}` at or below the first folder name
    /// holding `{Artist}` or `{Album Artist}`. Without the artist, two artists' albums of one name
    /// would share a folder and its cover. A later folder holding `{Album}` again sits inside it.
    pub fn album_folder_segment(&self) -> Option<usize> {
        let folders = &self.segments[..self.segments.len() - 1];
        let artist = folders
            .iter()
            .position(|parts| parts.iter().filter_map(Part::field).any(Field::is_artist))?;
        (artist..folders.len()).find(|&at| folders[at].iter().any(|part| part.field() == Some(Field::Album)))
    }

    /// The song's file name, without the extension, rendered to fit `room` UTF-16 units: `{Title}`
    /// shrinks first, then the other names from the right, each cut at a word break and never
    /// below 20 units; the fixed text, `{Track}`, `{Disc}` and `{Year}` never shrink.
    pub fn render_file_within(&self, values: &LayoutValues, extension: &str, room: usize) -> String {
        let file = self.segments.last().expect("a template has a file name");
        let render = |values: &LayoutValues| {
            render_segment(file, values, true)
                .map(|stem| PathHelper::safe_name(&stem, Some(extension)))
                .unwrap_or_default()
        };
        let mut current = values.clone();
        let mut name = render(&current);
        let mut order = vec![Field::Title];
        for field in file.iter().rev().filter_map(Part::field) {
            if field.shrinks() && !order.contains(&field) {
                order.push(field);
            }
        }
        for field in order {
            let excess = utf16_len(&name).saturating_sub(room);
            if excess == 0 {
                break;
            }
            let value = text_of(&current, field);
            // The value as it renders: never past the cap every value is cut to.
            let length = utf16_len(&value).min(VALUE_CAP);
            if length <= SHRINK_FLOOR {
                continue;
            }
            let cap = length.saturating_sub(excess).max(SHRINK_FLOOR);
            set_text(&mut current, field, cut_name(&value, cap, None));
            name = render(&current);
        }
        name
    }

    /// Whether the album folder's name shows the year and the release type, the only names that
    /// may tell two folders of one album title apart. Both false with no album folder.
    pub fn album_folder_shows(&self) -> (bool, bool) {
        let Some(at) = self.album_folder_segment() else {
            return (false, false);
        };
        let fields: Vec<Field> = self.segments[at].iter().filter_map(Part::field).collect();
        (fields.contains(&Field::Year), fields.contains(&Field::AlbumType))
    }

    /// The least room the file name needs: its fixed text, 20 units for each name in it (4 for a
    /// track, disc or year), never under 40, plus 6 for the extension.
    fn file_floor(&self) -> usize {
        let file = self.segments.last().expect("a template has a file name");
        let needed: usize = file
            .iter()
            .map(|part| match part {
                Part::Text(text) => utf16_len(text),
                Part::Name {
                    field, before, after, ..
                } => utf16_len(before) + utf16_len(after) + if field.shrinks() { 20 } else { 4 },
            })
            .sum();
        needed.max(40) + 6
    }

    /// Whether the template files an album in a folder of its own.
    pub fn has_album_folder(&self) -> bool {
        self.album_folder_segment().is_some()
    }

    /// Whether any folder or the file name holds `field`.
    pub fn uses(&self, field: Field) -> bool {
        self.segments
            .iter()
            .flatten()
            .any(|part| part.field() == Some(field))
    }

    /// The disc folder: the folder right below the album folder, when its name holds `{Disc}`.
    pub fn disc_folder_segment(&self) -> Option<usize> {
        let below = self.album_folder_segment()? + 1;
        (below + 1 < self.segments.len()
            && self.segments[below]
                .iter()
                .any(|part| part.field() == Some(Field::Disc)))
        .then_some(below)
    }

    /// Whether a folder above the album folder names an artist, so the album folder sits in an
    /// artist's own folder. Without one (`{Album Artist} - {Album}`), the folders beside it are
    /// every artist's.
    pub fn artist_above_album_folder(&self) -> bool {
        self.album_folder_segment().is_some_and(|album| {
            self.segments[..album]
                .iter()
                .any(|parts| parts.iter().filter_map(Part::field).any(Field::is_artist))
        })
    }

    /// Whether the template's top folder is the artist alone, or it has no folder: what the
    /// library tidy's artist folders assume.
    pub fn top_folder_is_artist_or_none(&self) -> bool {
        if self.segments.len() == 1 {
            return true;
        }
        matches!(
            self.segments[0].as_slice(),
            [Part::Name {
                field: Field::Artist,
                before,
                after,
                ..
            }] if before.is_empty() && after.is_empty()
        )
    }
}

impl Part {
    fn field(&self) -> Option<Field> {
        match self {
            Part::Name { field, .. } => Some(*field),
            Part::Text(_) => None,
        }
    }
}

impl Field {
    /// The name as the dashboard writes it.
    fn braces(self) -> &'static str {
        match self {
            Field::Artist => "{Artist}",
            Field::TrackArtist => "{Track Artist}",
            Field::AlbumArtist => "{Album Artist}",
            Field::Album => "{Album}",
            Field::Title => "{Title}",
            Field::Track => "{Track}",
            Field::Disc => "{Disc}",
            Field::Year => "{Year}",
            Field::AlbumType => "{Album Type}",
        }
    }
}

/// The typed text of a folder name with no names, or the text a folder or file name starts or
/// ends with, must already be a name Windows, Samba and Navidrome take: what would be changed
/// silently is refused with words instead. The text inside a name's braces counts as typed
/// text too: `{.Artist}` starts a name with a dot as `.{Artist}` does.
fn check_typed_text(parts: &[Part], is_file: bool) -> Result<(), TemplateError> {
    let first_name = parts.iter().position(|part| part.field().is_some());
    let last_name = parts.iter().rposition(|part| part.field().is_some());
    // The typed text before the first name's value: the text before its braces, then the text
    // inside them before the name.
    let mut start = String::new();
    for part in &parts[..first_name.map_or(parts.len(), |at| at + 1)] {
        match part {
            Part::Text(text) => start.push_str(text),
            Part::Name { before, .. } => start.push_str(before),
        }
    }
    // The typed text after the last name's value: the text inside its braces after the name,
    // then the text after its braces.
    let mut end = String::new();
    for part in &parts[last_name.unwrap_or(0)..] {
        match part {
            Part::Text(text) => end.push_str(text),
            Part::Name { after, .. } => end.push_str(after),
        }
    }
    if start.trim_start().starts_with('.') {
        return Err(TemplateError(
            "A name that starts with a dot is hidden on network shares and in Navidrome. Start it another way."
                .to_string(),
        ));
    }
    if !is_file && (end.ends_with('.') || end.ends_with(' ')) {
        return Err(TemplateError(
            "Windows drops a dot or a space at the end of a folder name. End it another way.".to_string(),
        ));
    }
    if !is_file && let [Part::Text(only)] = parts {
        let typed = only.trim();
        if PathHelper::safe_name(typed, None) != typed {
            return Err(TemplateError(format!(
                "Windows or Navidrome keeps the name {typed} for itself. Use another folder name."
            )));
        }
    }
    Ok(())
}

fn parse_segment(raw: &str) -> Result<Vec<Part>, TemplateError> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                let mut inside = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    match c {
                        '}' => {
                            closed = true;
                            break;
                        }
                        '{' => {
                            return Err(TemplateError(
                                "A { is still open where the next { starts. Close it with }.".to_string(),
                            ));
                        }
                        _ => inside.push(c),
                    }
                }
                if !closed {
                    return Err(TemplateError("A { has no closing }.".to_string()));
                }
                parts.push(parse_name(&inside)?);
            }
            '}' => return Err(TemplateError("A } has no { before it.".to_string())),
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => {
                return Err(TemplateError(format!(
                    "Windows can't have {c} in a name. Leave it out, or use - instead."
                )));
            }
            c if c.is_control() => {
                return Err(TemplateError(
                    "The layout has an invisible control character. Type it again.".to_string(),
                ));
            }
            _ => text.push(c),
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(parts)
}

fn parse_name(inside: &str) -> Result<Part, TemplateError> {
    let rest = inside.trim_start_matches(&BEFORE_NAME[..]);
    let before = &inside[..inside.len() - rest.len()];
    let body = rest.trim_end_matches(&AFTER_NAME[..]);
    let after = &rest[body.len()..];
    let (name, format) = match body.split_once(':') {
        Some((name, format)) => (name, Some(format.trim())),
        None => (body, None),
    };
    let key: String = name
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '.'))
        .flat_map(char::to_lowercase)
        .collect();
    if let Some((_, instead)) = LIDARR_ONLY.iter().find(|(lidarr, _)| *lidarr == key) {
        return Err(TemplateError(format!(
            "Octo doesn't know {{{}}}. {instead}",
            name.trim()
        )));
    }
    let Some(field) = Field::named(name) else {
        return Err(TemplateError(format!(
            "{{{}}} isn't a name Octo knows. Use one of {NAMES}.",
            name.trim()
        )));
    };
    let digits = match format {
        None => 0,
        Some(_) if !field.takes_digits() => {
            return Err(TemplateError(
                "Only {Track} and {Disc} take a number format, like {Track:00}.".to_string(),
            ));
        }
        Some(format) if !format.is_empty() && format.len() <= 4 && format.chars().all(|c| c == '0') => {
            format.len()
        }
        Some(_) => {
            return Err(TemplateError(
                "Write the number format as zeros, like {Track:00} for 01.".to_string(),
            ));
        }
    };
    Ok(Part::Name {
        field,
        digits,
        before: before.to_string(),
        after: after.to_string(),
    })
}

/// A rendered piece: text from the template, or a name's value (empty when it has none).
enum Piece {
    Text(String),
    Value(String),
}

fn render_segment(parts: &[Part], values: &LayoutValues, is_file: bool) -> Option<String> {
    let mut pieces: Vec<Piece> = parts
        .iter()
        .map(|part| match part {
            Part::Text(text) => Piece::Text(text.clone()),
            Part::Name {
                field,
                digits,
                before,
                after,
            } => {
                let value = value_of(*field, *digits, values, is_file);
                if value.is_empty() {
                    Piece::Value(value)
                } else {
                    Piece::Value(format!("{before}{value}{after}"))
                }
            }
        })
        .collect();
    let has_names = pieces.iter().any(|piece| matches!(piece, Piece::Value(_)));
    if has_names && pieces.iter().all(is_blank_piece) {
        return None;
    }
    for at in 0..pieces.len() {
        if !matches!(&pieces[at], Piece::Value(value) if value.is_empty()) {
            continue;
        }
        if remove_bracket_group(&mut pieces, at) {
            continue;
        }
        let after_opening = match at.checked_sub(1).map(|before| &pieces[before]) {
            Some(Piece::Text(text)) => {
                let trimmed = text.trim_end_matches(' ');
                BRACKETS.iter().any(|(opening, _)| trimmed.ends_with(*opening))
            }
            _ => false,
        };
        let at_start = after_opening
            || pieces[..at].iter().all(|piece| match piece {
                Piece::Value(value) => value.is_empty(),
                Piece::Text(text) => text.chars().all(|c| SEPARATORS.contains(&c)),
            });
        if at_start {
            if let Some(Piece::Text(text)) = pieces.get_mut(at + 1) {
                *text = text.trim_start_matches(&SEPARATORS[..]).to_string();
            }
        } else if let Some(Piece::Text(text)) = at.checked_sub(1).map(|before| &mut pieces[before]) {
            *text = text.trim_end_matches(&SEPARATORS[..]).to_string();
        }
    }
    let joined: String = pieces
        .into_iter()
        .map(|piece| match piece {
            Piece::Text(text) | Piece::Value(text) => text,
        })
        .collect();
    Some(joined)
}

/// Text, or a name with no value.
fn is_blank_piece(piece: &Piece) -> bool {
    match piece {
        Piece::Text(_) => true,
        Piece::Value(value) => value.is_empty(),
    }
}

/// Removes the brackets around the empty name at `at`, everything between them and the spaces
/// before them, when nothing between them has a value: `{Album} (Disc {Disc})` on a one-disc
/// album gives `Album`. False when the name is not inside brackets.
fn remove_bracket_group(pieces: &mut [Piece], at: usize) -> bool {
    let mut open: Option<(usize, usize, char)> = None;
    for index in (0..at).rev() {
        match &pieces[index] {
            Piece::Value(value) if !value.is_empty() => return false,
            Piece::Value(_) => {}
            Piece::Text(text) => {
                for (offset, c) in text.char_indices().rev() {
                    if let Some((_, closing)) = BRACKETS.iter().find(|(opening, _)| *opening == c) {
                        open = Some((index, offset, *closing));
                        break;
                    }
                    if BRACKETS.iter().any(|(_, closing)| *closing == c) {
                        return false;
                    }
                }
                if open.is_some() {
                    break;
                }
            }
        }
    }
    let Some((open_index, open_offset, closing)) = open else {
        return false;
    };
    let mut close: Option<(usize, usize)> = None;
    for (index, piece) in pieces.iter().enumerate().skip(at + 1) {
        match piece {
            Piece::Value(value) if !value.is_empty() => return false,
            Piece::Value(_) => {}
            Piece::Text(text) => {
                for (offset, c) in text.char_indices() {
                    if c == closing {
                        close = Some((index, offset));
                        break;
                    }
                    if BRACKETS.iter().any(|(opening, _)| *opening == c) {
                        return false;
                    }
                }
                if close.is_some() {
                    break;
                }
            }
        }
    }
    let Some((close_index, close_offset)) = close else {
        return false;
    };
    if let Piece::Text(text) = &mut pieces[close_index] {
        *text = text[close_offset + closing.len_utf8()..].to_string();
    }
    for piece in &mut pieces[open_index + 1..close_index] {
        *piece = Piece::Text(String::new());
    }
    if let Piece::Text(text) = &mut pieces[open_index] {
        *text = text[..open_offset].trim_end_matches(' ').to_string();
    }
    true
}

/// A name's value, cleaned for a name: artists and the album as folder names (no trailing dots,
/// "Unknown" when blank), the title as a file name. The caller fills a blank artist with what it
/// wants shown ("Unknown Artist").
fn value_of(field: Field, digits: usize, values: &LayoutValues, in_file_name: bool) -> String {
    match field {
        Field::Artist => PathHelper::sanitize_folder_name(&values.artist),
        Field::TrackArtist => PathHelper::sanitize_folder_name(&values.track_artist),
        Field::AlbumArtist => match values.album_artist.as_deref().filter(|a| !a.trim().is_empty()) {
            Some(album_artist) => PathHelper::sanitize_folder_name(album_artist),
            None => PathHelper::sanitize_folder_name(&values.artist),
        },
        Field::Album if values.album.trim().is_empty() => {
            if in_file_name {
                String::new()
            } else {
                PathHelper::sanitize_folder_name(&values.title)
            }
        }
        Field::Album => PathHelper::sanitize_folder_name(&values.album),
        Field::Title => PathHelper::sanitize_file_name(&values.title),
        Field::Track => number(values.track, digits),
        Field::Disc if values.multi_disc => number(Some(values.disc.unwrap_or(1)), digits),
        Field::Disc => String::new(),
        Field::Year => values
            .year
            .filter(|year| *year > 0)
            .map(|year| year.to_string())
            .unwrap_or_default(),
        Field::AlbumType => values.album_type.clone().unwrap_or_default(),
    }
}

/// The shortest a value is cut to when a file name must shrink.
const SHRINK_FLOOR: usize = 20;

/// The longest a value renders, in UTF-16 units ([`PathHelper::sanitize_file_name`]).
const VALUE_CAP: usize = 100;

/// The text a name is filled from, before cleaning.
fn text_of(values: &LayoutValues, field: Field) -> String {
    match field {
        Field::Title => values.title.clone(),
        Field::Album => values.album.clone(),
        Field::Artist => values.artist.clone(),
        Field::TrackArtist => values.track_artist.clone(),
        Field::AlbumArtist => values
            .album_artist
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| values.artist.clone()),
        Field::AlbumType => values.album_type.clone().unwrap_or_default(),
        Field::Track | Field::Disc | Field::Year => String::new(),
    }
}

/// `values` with the text `field` is filled from set to `text`.
fn set_text(values: &mut LayoutValues, field: Field, text: String) {
    match field {
        Field::Title => values.title = text,
        Field::Album => values.album = text,
        Field::Artist => values.artist = text,
        Field::TrackArtist => values.track_artist = text,
        Field::AlbumArtist => values.album_artist = Some(text),
        Field::AlbumType => values.album_type = Some(text),
        Field::Track | Field::Disc | Field::Year => {}
    }
}

/// A track or disc number, padded to `digits` after any sign, as .NET's "D2" wrote it.
fn number(value: Option<i32>, digits: usize) -> String {
    match value {
        None => String::new(),
        Some(n) if n < 0 => format!("-{:0digits$}", n.unsigned_abs()),
        Some(n) => format!("{n:0digits$}"),
    }
}

/// Where a part of the settings went wrong, and the template used instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutProblem {
    /// The setting's name under Subsonic: FolderTemplate, FolderTemplateSingles or
    /// FolderTemplateCompilations.
    pub setting: &'static str,
    pub message: String,
}

/// The templates songs are filed by, one per kind, from the settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    album: Template,
    single: Template,
    compilation: Template,
    problems: Vec<LayoutProblem>,
    /// See [`Layout::cut_caps`].
    cut_caps: Vec<usize>,
}

impl Layout {
    /// One of the built-in layouts, for every kind.
    pub fn preset(structure: FolderStructure) -> Layout {
        let text = match structure {
            FolderStructure::Flat => FLAT,
            FolderStructure::ByArtist => BY_ARTIST,
            FolderStructure::Organized | FolderStructure::Custom => ORGANIZED,
        };
        let template = Template::parse(text).expect("the built-in layouts parse");
        Layout {
            album: template.clone(),
            single: template.clone(),
            compilation: template,
            problems: Vec::new(),
            cut_caps: Vec::new(),
        }
        .with_cut_caps(&[])
    }

    /// The layout the settings name. A blank singles or compilations template is the album one,
    /// and a blank album template is Organized. A custom template that does not parse is
    /// replaced the same way, and each replacement is listed in [`Layout::problems`]. The caps
    /// the settings remember join its [`Layout::cut_caps`], so a folder name an older template
    /// cut is still the same folder after the layout changes.
    pub fn from_settings(settings: &SubsonicSettings) -> Layout {
        Self::custom_or_preset(
            settings.folder_structure,
            &settings.folder_template,
            &settings.folder_template_singles,
            &settings.folder_template_compilations,
        )
        .with_cut_caps(&settings.layout_caps)
    }

    /// [`Layout::from_settings`] from the four values themselves (the dashboard's preview), with
    /// no remembered caps.
    pub fn custom_or_preset(
        structure: FolderStructure,
        album: &str,
        singles: &str,
        compilations: &str,
    ) -> Layout {
        if structure != FolderStructure::Custom {
            return Self::preset(structure);
        }
        let mut problems = Vec::new();
        // A blank album template is Organized, as a blank singles one is the album one: an
        // unset FOLDER_TEMPLATE reaches the settings as an empty value.
        let album = if album.trim().is_empty() { ORGANIZED } else { album };
        let album = match Template::parse(album) {
            Ok(template) => template,
            Err(error) => {
                problems.push(LayoutProblem {
                    setting: "FolderTemplate",
                    message: error.0,
                });
                Template::parse(ORGANIZED).expect("the built-in layouts parse")
            }
        };
        let mut other = |text: &str, setting: &'static str| {
            if text.trim().is_empty() {
                return album.clone();
            }
            match Template::parse(text) {
                Ok(template) => template,
                Err(error) => {
                    problems.push(LayoutProblem {
                        setting,
                        message: error.0,
                    });
                    album.clone()
                }
            }
        };
        let single = other(singles, "FolderTemplateSingles");
        let compilation = other(compilations, "FolderTemplateCompilations");
        Layout {
            album,
            single,
            compilation,
            problems,
            cut_caps: Vec::new(),
        }
        .with_cut_caps(&[])
    }

    /// Settings that did not parse, each replaced as [`Layout::from_settings`] says.
    pub fn problems(&self) -> &[LayoutProblem] {
        &self.problems
    }

    pub fn template(&self, kind: LayoutKind) -> &Template {
        match kind {
            LayoutKind::Album => &self.album,
            LayoutKind::Single => &self.single,
            LayoutKind::Compilation => &self.compilation,
        }
    }

    /// The three templates' texts, album first, as the File layout task keeps them with a preview.
    pub fn texts(&self) -> [String; 3] {
        [
            self.album.text().to_string(),
            self.single.text().to_string(),
            self.compilation.text().to_string(),
        ]
    }

    /// The song's folder names and file name without its extension, as its kind's template gives
    /// them within `budget` (see [`PATH_BUDGET`]).
    pub fn render(&self, values: &LayoutValues, extension: &str, budget: usize) -> Vec<RenderedName> {
        self.template(values.kind).render(values, extension, budget)
    }

    /// [`Self::render`] in the music folder's budget, names only: what the built-in layouts and
    /// the tidy's tests compare.
    pub fn render_names(&self, values: &LayoutValues, extension: &str) -> Vec<String> {
        self.render(values, extension, PATH_BUDGET)
            .into_iter()
            .map(|rendered| rendered.name)
            .collect()
    }

    /// Whether every template's top folder is the artist alone, or has no folder.
    pub fn top_folder_is_artist_or_none(&self) -> bool {
        [&self.album, &self.single, &self.compilation]
            .iter()
            .all(|template| template.top_folder_is_artist_or_none())
    }

    /// Every cap a folder name this layout files into may have been cut to, sorted: the built-in
    /// caps, the caps of the layout's three templates in the music folder's budget and the family
    /// pool's, and the caps the settings remember ([`SubsonicSettings::layout_caps`]). Each
    /// template's rendered names carry them ([`RenderedName::cut_caps`]).
    pub fn cut_caps(&self) -> &[usize] {
        &self.cut_caps
    }

    /// The layout with its cut caps worked out from its templates and `remembered`.
    fn with_cut_caps(mut self, remembered: &[usize]) -> Layout {
        let mut caps: Vec<usize> = built_in_caps()
            .chain(self.album.own_caps())
            .chain(self.single.own_caps())
            .chain(self.compilation.own_caps())
            .chain(remembered.iter().copied())
            .collect();
        caps.sort_unstable();
        caps.dedup();
        for template in [&mut self.album, &mut self.single, &mut self.compilation] {
            template.cut_caps = caps.clone();
        }
        self.cut_caps = caps;
        self
    }
}

/// The caps the templates in `texts` cut folder names to, in the music folder's budget and the
/// family pool's, for each that parses and has a folder, less the built-in caps every layout
/// knows anyway: what the settings store remembers for [`Layout::cut_caps`].
pub fn caps_to_remember(texts: [&str; 3]) -> BTreeSet<usize> {
    let built_in: BTreeSet<usize> = built_in_caps().collect();
    texts
        .iter()
        .filter(|text| !text.trim().is_empty())
        .filter_map(|text| Template::parse(text).ok())
        .flat_map(|template| template.own_caps())
        .filter(|cap| !built_in.contains(cap))
        .collect()
}

/// The length of a rendered path under the music folder, in UTF-16 units.
pub fn path_length(names: &[String], extension: &str) -> usize {
    names.iter().map(|name| utf16_len(name)).sum::<usize>()
        + names.len().saturating_sub(1)
        + utf16_len(extension)
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
