//! The plan for "Tidy artists": bring files already in the library in line with how a new
//! download is written. Pure: file facts and MusicBrainz answers in, plan out, so every rule is
//! a table test.
//!
//! Three things, each only on evidence:
//!
//! 1. Split an artist folder named after a list of artists ("Bizarrap, Rauw Alejandro") to its
//!    first artist, by the same rule a new download uses. The evidence is the files' own: an
//!    ARTISTS tag, or the MusicBrainz credit of the recording a file is tagged with. Without
//!    either, a folder splits only when every part is already an artist in this library AND
//!    MusicBrainz does not know the whole name as one artist. MusicBrainz alone never splits.
//! 2. One spelling per artist. Spellings Navidrome already treats as one artist (curly and
//!    straight quotes, dashes) are re-tagged freely. Spellings that are two artists to Navidrome
//!    (accents, "P!nk" and "Pink") are "Rename" rows, off until ticked, because the rename costs
//!    every user their stars and ratings on that artist. A spelling that only differs in
//!    capitals needs no re-tag at all. Files tagged from MusicBrainz are never re-tagged, and
//!    spellings whose files carry different MusicBrainz artists are never merged.
//! 3. Guests: a "(feat. X)" in a title or in an ARTIST value adds X to the ARTISTS tag, primary
//!    artist first. Titles and ARTIST values are never rewritten.
//!
//! Moves are whole album folders, and only when every song in the folder is in the run.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use octo_core::common::artist_credit::primary_credit;
use octo_core::common::navidrome_artist_key::{navidrome_artist_parts, navidrome_artist_text};
use octo_core::common::path_helper::fold_name;
use octo_core::common::{PathHelper, SongIdentity};
pub use octo_core::fingerprint::music_brainz_client::CreditPart;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

/// The three artist fields the tidy reads and writes, each as its list of values.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TidyTags {
    #[serde(default)]
    pub artist: Vec<String>,
    #[serde(default)]
    pub album_artist: Vec<String>,
    #[serde(default)]
    pub artists: Vec<String>,
}

/// One audio file as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TidyFile {
    /// The absolute path, with "/" separators.
    pub path: String,
    pub tags: TidyTags,
    /// The MusicBrainz artist ids the file carries. A file with any is never re-tagged.
    pub artist_ids: Vec<String>,
    pub recording_id: Option<String>,
    pub title: Option<String>,
    /// Out-of-scope files vote on spellings and count as library artists, but are never
    /// changed, and an album folder holding one never moves.
    pub in_scope: bool,
    /// False when the tags could not be read. Such a file still moves with its folder.
    pub readable: bool,
    /// An ID3v2.3 artist frame holding a "/": its values cannot be written back as they were.
    pub slash_artist: bool,
}

/// What the planner works from besides the files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TidyAnswers {
    /// MusicBrainz artists by an exact name, keyed by [`artist_lookup_key`]: the names those
    /// artists are spelled with, empty when there is none. A name missing here was not asked.
    pub artists: HashMap<String, Vec<String>>,
    /// MusicBrainz artist credits keyed by lowercase recording id. Missing means not asked.
    pub credits: HashMap<String, Vec<CreditPart>>,
}

/// The settings of one plan.
#[derive(Debug, Clone, Default)]
pub struct TidyOptions {
    /// The filesystem treats "a" and "A" as one name, so a folder cannot be renamed by case.
    pub case_insensitive: bool,
    /// Ids of the opt-in rows the person ticked.
    pub ticked: HashSet<String>,
    /// When set, every opt-in row counts as ticked: the plan an apply is checked against.
    pub tick_all: bool,
    /// Folders under the root (relative, "/" separated) the person left out of the run.
    pub excluded: Vec<String>,
    /// The file layout does not file songs in artist folders at the top, so artist folders are
    /// left as they are: only tags change.
    pub leave_folders: bool,
}

/// The kind of a preview row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TidyKind {
    /// An artist folder named after a list of artists, filed under its first artist.
    Split,
    /// One spelling per artist where Navidrome already sees one artist: tags and folders only.
    Respell,
    /// One spelling per artist where Navidrome sees two: opt-in, loses stars on the old one.
    Rename,
    /// Guests added to a song's ARTISTS tag.
    Guest,
    /// Songs of one folder moved or renamed into the file layout.
    Layout,
    /// Left alone, with the reason.
    Skip,
}

/// One line of the preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TidyRow {
    /// Stable across previews of the same library: what a tick and an exclusion refer to.
    pub id: String,
    pub kind: TidyKind,
    /// Relative to the music folder: a folder, a file, or empty for a spelling.
    pub path: String,
    pub before: String,
    pub after: String,
    pub evidence: String,
    /// Files the row changes.
    pub files: usize,
    /// A row that only runs when ticked.
    pub opt_in: bool,
    pub ticked: bool,
}

/// A file's artist fields rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TidyRetag {
    pub path: String,
    pub before: TidyTags,
    pub after: TidyTags,
}

/// An album folder moved, whole, to the folder a new download of it would use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TidyMove {
    pub from: String,
    pub to: String,
    /// The row this move belongs to.
    pub row: String,
    /// The audio files in the folder, as planned.
    pub files: Vec<String>,
}

/// One file moved or renamed by the File layout task: a song, or a file beside it that takes its
/// name (its lyrics), right after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TidyFileMove {
    pub from: String,
    pub to: String,
    pub row: String,
    /// For a file beside a song, the song's own `from`.
    #[serde(default)]
    pub song: Option<String>,
    /// The folder move this file's `from` lies in, which runs first: the file is held when that
    /// move is.
    #[serde(default)]
    pub after: Option<String>,
}

/// A cover copied, never moved, into a folder an album moved into whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TidyCopy {
    pub from: String,
    pub to: String,
    pub row: String,
}

/// The plan.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TidyPlan {
    pub rows: Vec<TidyRow>,
    pub retags: Vec<TidyRetag>,
    pub moves: Vec<TidyMove>,
    /// Names MusicBrainz should be asked about before the plan can settle them.
    pub wanted_artists: Vec<String>,
    /// Recording ids whose MusicBrainz credit is wanted.
    pub wanted_recordings: Vec<String>,
    /// File layout: files moved or renamed, after the folder moves.
    #[serde(default)]
    pub file_moves: Vec<TidyFileMove>,
    /// File layout: covers copied into folders an album moved into.
    #[serde(default)]
    pub copies: Vec<TidyCopy>,
    /// File layout: family pool files moved, each with every member's link.
    #[serde(default)]
    pub pool_moves: Vec<TidyFileMove>,
}

impl TidyPlan {
    /// A short fingerprint of everything the plan would write and move. An apply plans again
    /// and refuses when this differs from the preview's, so it only does what was shown.
    pub fn hash(&self) -> String {
        let mut text = String::new();
        let mut retags: Vec<&TidyRetag> = self.retags.iter().collect();
        retags.sort_by(|a, b| a.path.cmp(&b.path));
        for retag in retags {
            text.push_str(&format!(
                "t|{}|{}|{}|{}\n",
                retag.path,
                retag.after.artist.join("\u{1f}"),
                retag.after.album_artist.join("\u{1f}"),
                retag.after.artists.join("\u{1f}")
            ));
        }
        let mut moves: Vec<&TidyMove> = self.moves.iter().collect();
        moves.sort_by(|a, b| a.from.cmp(&b.from));
        for movement in moves {
            text.push_str(&format!(
                "m|{}|{}|{}\n",
                movement.from,
                movement.to,
                movement.files.join("\u{1f}")
            ));
        }
        // Only a File layout plan has these, so an artist plan's fingerprint is as it was.
        for (tag, list) in [("f", &self.file_moves), ("p", &self.pool_moves)] {
            for movement in list {
                text.push_str(&format!("{tag}|{}|{}\n", movement.from, movement.to));
            }
        }
        for copy in &self.copies {
            text.push_str(&format!("c|{}|{}\n", copy.from, copy.to));
        }
        hex::encode_upper(Sha256::digest(text.as_bytes()))[..16].to_string()
    }
}

/// The key a MusicBrainz artist answer is filed under: its search ignores case.
pub fn artist_lookup_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// A guest named in a title in a way the title reader does not take apart.
static GUEST_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:feat|ft|featuring)\b\.?").expect("the guest word pattern is valid")
});

/// "X: Some Music Session": a second artist with nothing to say which part is the guest.
static SESSION_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[^:]{1,60}:\s.*\bsessions?\b").expect("the session title pattern is valid")
});

const NOT_ASKED: &str = "MusicBrainz was not asked yet (the lookup limit for one preview was reached, or it could not be reached). Preview again to ask.";

/// Plans a tidy of `files` under `root`.
pub fn plan(root: &str, files: &[TidyFile], answers: &TidyAnswers, options: &TidyOptions) -> TidyPlan {
    let root = root.trim_end_matches('/');
    let files: Vec<TidyFile> = files
        .iter()
        .filter(|file| relative(root, &file.path).is_some_and(|segments| !is_hidden(&segments)))
        .map(|file| {
            let mut file = file.clone();
            if file.in_scope && is_excluded(root, &file.path, &options.excluded) {
                file.in_scope = false;
            }
            file
        })
        .collect();
    let mut out = Planned::default();

    let spelling = spellings(&files, answers, options, &mut out);
    let folders = folders(root, &files, answers, &spelling, &mut out);
    retags(root, &files, &spelling, &folders, &mut out);
    if options.leave_folders {
        // The layout's top folder is not the artist, so artist folders are not this task's.
        out.rows.retain(|row| {
            row.kind != TidyKind::Split
                && !["split|", "folder|", "hidden|"]
                    .iter()
                    .any(|prefix| row.id.starts_with(prefix))
        });
    } else {
        moves(root, &files, &folders, options, &mut out);
    }

    let mut wanted_artists: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for name in out.wanted_artists {
        if seen.insert(artist_lookup_key(&name)) {
            wanted_artists.push(name);
        }
    }
    let wanted_recordings: Vec<String> = out.wanted_recordings.into_iter().collect();
    TidyPlan {
        rows: out.rows,
        retags: out.retags,
        moves: out.moves,
        wanted_artists,
        wanted_recordings,
        ..TidyPlan::default()
    }
}

#[derive(Default)]
struct Planned {
    rows: Vec<TidyRow>,
    retags: Vec<TidyRetag>,
    moves: Vec<TidyMove>,
    wanted_artists: Vec<String>,
    wanted_recordings: BTreeSet<String>,
}

impl Planned {
    fn skip(
        &mut self,
        id: String,
        path: impl Into<String>,
        before: impl Into<String>,
        evidence: impl Into<String>,
    ) {
        self.rows.push(TidyRow {
            id,
            kind: TidyKind::Skip,
            path: path.into(),
            before: before.into(),
            after: String::new(),
            evidence: evidence.into(),
            files: 0,
            opt_in: false,
            ticked: false,
        });
    }
}

// ---- spelling -------------------------------------------------------------------------------

/// The spelling decisions: which value becomes which, and the row each change belongs to.
#[derive(Default)]
struct Spelling {
    /// Value -> (winner, row id), only for changes that run (respells, and ticked renames).
    active: HashMap<String, (String, String)>,
    /// Value -> (winner, row id) for every change, ticked or not.
    all: HashMap<String, (String, String)>,
}

impl Spelling {
    fn respell(&self, value: &str) -> String {
        self.active
            .get(value.trim())
            .map_or_else(|| value.to_string(), |(winner, _)| winner.clone())
    }
}

fn spellings(
    files: &[TidyFile],
    answers: &TidyAnswers,
    options: &TidyOptions,
    out: &mut Planned,
) -> Spelling {
    let mut tracks: BTreeMap<String, usize> = BTreeMap::new();
    let mut tagged: HashMap<String, usize> = HashMap::new();
    let mut in_scope: HashSet<String> = HashSet::new();
    let mut ids: HashMap<String, BTreeSet<String>> = HashMap::new();
    for file in files.iter().filter(|file| file.readable) {
        let values: BTreeSet<String> = all_values(&file.tags).collect();
        for value in &values {
            *tracks.entry(value.clone()).or_default() += 1;
            let named = file
                .tags
                .artist
                .iter()
                .chain(&file.tags.artists)
                .any(|v| v.trim() == value);
            if !file.artist_ids.is_empty() && named {
                *tagged.entry(value.clone()).or_default() += 1;
            }
            if file.in_scope {
                in_scope.insert(value.clone());
            }
        }
        // One artist and one id: that id is this spelling's MusicBrainz artist.
        if file.artist_ids.len() == 1 && file.tags.artist.len() == 1 && file.tags.artists.len() <= 1 {
            ids.entry(file.tags.artist[0].trim().to_string())
                .or_default()
                .insert(file.artist_ids[0].to_lowercase());
        }
    }

    // A credit Navidrome reads as several artists ("A; B") is not one artist's spelling: merging
    // it with a credit Navidrome keeps whole would join or part artists, so it stays out.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut combined: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for value in tracks.keys() {
        let key = SongIdentity::loose_key(value);
        if key.is_empty() {
            continue;
        }
        if navidrome_artist_parts(std::slice::from_ref(value)).len() > 1 {
            combined.entry(key).or_default().push(value.clone());
        } else {
            groups.entry(key).or_default().push(value.clone());
        }
    }
    for (key, credits) in &combined {
        let alike = credits.len() + groups.get(key).map_or(0, Vec::len);
        if alike < 2
            || !credits
                .iter()
                .chain(groups.get(key).into_iter().flatten())
                .any(|v| in_scope.contains(v))
        {
            continue;
        }
        let why = if credits.len() == 1 {
            "Navidrome lists this credit as several artists, so its spelling is left alone."
        } else {
            "Navidrome lists each of these credits as several artists, so their spelling is left alone."
        };
        out.skip(format!("combined|{key}"), "", quote_list(credits), why);
    }

    let mut spelling = Spelling::default();
    for (key, mut variants) in groups.clone() {
        if variants.len() < 2 || !variants.iter().any(|v| in_scope.contains(v)) {
            continue;
        }
        variants.sort_by(|a, b| tracks[b].cmp(&tracks[a]).then_with(|| a.cmp(b)));
        let shown = quote_list(&variants);

        let distinct_ids: BTreeSet<&String> = variants.iter().filter_map(|v| ids.get(v)).flatten().collect();
        if distinct_ids.len() > 1 {
            out.skip(
                format!("spelling|{key}"),
                "",
                shown,
                "Files tagged from MusicBrainz name different artists with these spellings, so they are not merged.",
            );
            continue;
        }

        let most = tracks[&variants[0]];
        let best_tagged = variants
            .iter()
            .map(|v| tagged.get(v).copied().unwrap_or(0))
            .max()
            .unwrap_or(0);
        let chosen: Option<(String, String)> = if best_tagged > 0 {
            let by_tags: Vec<&String> = variants
                .iter()
                .filter(|v| tagged.get(*v).copied().unwrap_or(0) == best_tagged)
                .collect();
            (by_tags.len() == 1).then(|| {
                (
                    by_tags[0].clone(),
                    format!("Files tagged from MusicBrainz spell it \"{}\".", by_tags[0]),
                )
            })
        } else {
            None
        };
        let chosen = chosen.or_else(|| {
            let tied: Vec<&String> = variants.iter().filter(|v| tracks[*v] == most).collect();
            if tied.len() == 1 {
                let others: Vec<String> = variants[1..]
                    .iter()
                    .map(|v| format!("\"{v}\" on {}", tracks[v]))
                    .collect();
                return Some((
                    tied[0].clone(),
                    format!(
                        "\"{}\" is on {}; {}.",
                        tied[0],
                        counted(most, "track", "tracks"),
                        join_words(&others)
                    ),
                ));
            }
            None
        });
        let chosen = match chosen {
            Some(chosen) => chosen,
            None => {
                let tied: Vec<String> = variants.iter().filter(|v| tracks[*v] == most).cloned().collect();
                let unasked: Vec<String> = tied
                    .iter()
                    .filter(|v| !answers.artists.contains_key(&artist_lookup_key(v)))
                    .cloned()
                    .collect();
                let known: Vec<&String> = tied
                    .iter()
                    .filter(|v| {
                        answers
                            .artists
                            .get(&artist_lookup_key(v))
                            .is_some_and(|names| names.iter().any(|name| name == *v))
                    })
                    .collect();
                if known.len() == 1 {
                    (
                        known[0].clone(),
                        format!(
                            "{} tie on {}; MusicBrainz spells it \"{}\".",
                            quote_list(&tied),
                            counted(most, "track", "tracks"),
                            known[0]
                        ),
                    )
                } else {
                    let why = if unasked.is_empty() {
                        format!(
                            "These spellings tie on {} each, and MusicBrainz does not settle it.",
                            counted(most, "track", "tracks")
                        )
                    } else {
                        out.wanted_artists.extend(unasked);
                        format!(
                            "These spellings tie on {} each. {NOT_ASKED}",
                            counted(most, "track", "tracks")
                        )
                    };
                    out.skip(format!("spelling|{key}"), "", quote_list(&tied), why);
                    continue;
                }
            }
        };
        let (winner, why) = chosen;

        for loser in variants.iter().filter(|v| **v != winner) {
            let same_artist = navidrome_artist_text(loser) == navidrome_artist_text(&winner);
            if same_artist && loser.to_lowercase() == winner.to_lowercase() {
                out.skip(
                    format!("case|{}", navidrome_artist_text(loser)),
                    "",
                    format!("\"{loser}\""),
                    format!("Only the capitals differ from \"{winner}\", and Navidrome already lists them as one artist."),
                );
                continue;
            }
            let (kind, id, evidence) = if same_artist {
                (
                    TidyKind::Respell,
                    format!("respell|{}|{}", loser, winner),
                    format!("{why} Navidrome already lists them as one artist."),
                )
            } else {
                (
                    TidyKind::Rename,
                    format!(
                        "rename|{}|{}",
                        navidrome_artist_text(loser),
                        navidrome_artist_text(&winner)
                    ),
                    format!(
                        "{why} Navidrome lists these as two artists, so stars and ratings on \"{loser}\" and its albums will be lost for every user."
                    ),
                )
            };
            let opt_in = kind == TidyKind::Rename;
            let ticked = !opt_in || options.tick_all || options.ticked.contains(&id);
            out.rows.push(TidyRow {
                id: id.clone(),
                kind,
                path: String::new(),
                before: loser.clone(),
                after: winner.clone(),
                evidence,
                files: 0,
                opt_in,
                ticked,
            });
            spelling.all.insert(loser.clone(), (winner.clone(), id.clone()));
            if ticked {
                spelling.active.insert(loser.clone(), (winner.clone(), id));
            }
        }
    }

    // Anything looser than the loose key is reported, never merged: "The Weeknd" and "Weeknd"
    // may be one artist, but that is a guess.
    let mut near: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (_, variants) in groups {
        if variants.iter().any(|v| in_scope.contains(v)) {
            let first = variants[0].clone();
            let key = SongIdentity::loose_key(without_the(&first));
            if !key.is_empty() {
                near.entry(key).or_default().push(first);
            }
        }
    }
    for (key, names) in near.into_iter().filter(|(_, names)| names.len() > 1) {
        out.skip(
            format!("near|{key}"),
            "",
            quote_list(&names),
            "These look like one artist but only match once \"The\" is dropped, so they are left as they are.",
        );
    }
    spelling
}

fn without_the(value: &str) -> &str {
    let trimmed = value.trim();
    match trimmed.get(..4) {
        Some(head) if head.eq_ignore_ascii_case("the ") => &trimmed[4..],
        _ => trimmed,
    }
}

fn all_values(tags: &TidyTags) -> impl Iterator<Item = String> + '_ {
    tags.artist
        .iter()
        .chain(&tags.album_artist)
        .chain(&tags.artists)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ---- folders --------------------------------------------------------------------------------

/// What happens to one artist folder.
struct FolderDecision {
    /// The folder's name as it is.
    name: String,
    /// The folder's name after the plan, before sanitizing.
    target: String,
    /// The row the rename belongs to; None when the folder stays.
    row: Option<String>,
}

#[derive(Default)]
struct FolderResult {
    decisions: Vec<FolderDecision>,
    /// Credits proven to be a list of artists, with every artist in them, for ARTISTS.
    confirmed: HashMap<String, Vec<String>>,
}

enum Said {
    Whole,
    Split(String, Vec<String>),
    Contradicts,
}

fn folders(
    root: &str,
    files: &[TidyFile],
    answers: &TidyAnswers,
    spelling: &Spelling,
    out: &mut Planned,
) -> FolderResult {
    let mut result = FolderResult::default();
    let library_artists: HashSet<String> = files
        .iter()
        .filter(|file| file.readable)
        .flat_map(|file| all_values(&file.tags).collect::<Vec<_>>())
        .map(|value| navidrome_artist_text(&value))
        .collect();

    let mut by_folder: BTreeMap<String, Vec<&TidyFile>> = BTreeMap::new();
    for file in files {
        if let Some(segments) = relative(root, &file.path)
            && segments.len() >= 2
        {
            by_folder.entry(segments[0].clone()).or_default().push(file);
        }
    }

    for (folder, folder_files) in by_folder {
        let any_in_scope = folder_files.iter().any(|file| file.in_scope);
        let credits: Vec<String> = folder_files
            .iter()
            .filter(|file| file.readable)
            .flat_map(|file| [file.tags.artist.first(), file.tags.album_artist.first()])
            .flatten()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        let folder_key = SongIdentity::loose_key(&folder);
        let folded = fold_name(&folder, None);
        let credit = most_common(
            credits
                .iter()
                .filter(|value| fold_name(&PathHelper::sanitize_folder_name(value), None) == folded),
        )
        .or_else(|| {
            (!folder_key.is_empty())
                .then(|| {
                    most_common(credits.iter().filter(|value| {
                        SongIdentity::loose_key(&PathHelper::sanitize_folder_name(value)) == folder_key
                    }))
                })
                .flatten()
        });
        let Some(credit) = credit else {
            if any_in_scope && SongIdentity::parse_artists(&folder).names.len() > 1 {
                out.skip(
                    format!("folder|{folder}"),
                    folder.clone(),
                    folder.clone(),
                    "No song in this folder is tagged with the folder's name, so there is nothing to go on.",
                );
            }
            continue;
        };

        let pieces = SongIdentity::parse_artists(&credit).names;
        if pieces.len() < 2 {
            push_follow(&mut result, out, &folder, &credit, spelling, any_in_scope);
            continue;
        }

        // The files' own evidence, from the songs carrying this credit.
        let mut said: Vec<Said> = Vec::new();
        let mut unasked = false;
        for file in folder_files.iter().filter(|file| {
            file.readable
                && (same(file.tags.artist.first(), &credit) || same(file.tags.album_artist.first(), &credit))
        }) {
            if !file.tags.artists.is_empty() {
                said.push(read_list(&credit, &file.tags.artists));
            } else if let Some(recording) = file.recording_id.as_deref().map(str::to_lowercase) {
                match answers.credits.get(&recording) {
                    Some(parts) if !parts.is_empty() => {
                        let names: Vec<String> = parts.iter().map(|part| part.name.clone()).collect();
                        said.push(read_list(&credit, &names));
                    }
                    Some(_) => {}
                    None => {
                        unasked = true;
                        out.wanted_recordings.insert(recording);
                    }
                }
            } else if file.artist_ids.len() == 1 && same(file.tags.artist.first(), &credit) {
                said.push(Said::Whole);
            }
        }

        let folder_row = format!("split|{folder}");
        if !said.is_empty() {
            let primaries: BTreeSet<String> = said
                .iter()
                .filter_map(|s| match s {
                    Said::Split(primary, _) => Some(primary.to_lowercase()),
                    _ => None,
                })
                .collect();
            let all_split = said.iter().all(|s| matches!(s, Said::Split(..)));
            if all_split && primaries.len() == 1 {
                let Some(Said::Split(primary, names)) = said.into_iter().next() else {
                    continue;
                };
                let count = folder_files.len();
                result.confirmed.insert(credit.clone(), names);
                push_split(
                    &mut result,
                    out,
                    &folder,
                    &primary,
                    &folder_row,
                    format!(
                        "The songs' own artist credits ({}) name \"{primary}\" first.",
                        counted(count, "file", "files")
                    ),
                    spelling,
                );
            } else {
                push_follow(&mut result, out, &folder, &credit, spelling, any_in_scope);
                if any_in_scope {
                    let why = if said.iter().all(|s| matches!(s, Said::Whole)) {
                        format!("The songs' tags name \"{credit}\" as one artist, so it stays whole.")
                    } else if primaries.is_empty() {
                        "The songs' artist credits do not start with the folder's first artist, so it stays whole."
                            .to_string()
                    } else {
                        "The songs in this folder disagree about who the first artist is.".to_string()
                    };
                    out.skip(folder_row, folder.clone(), folder.clone(), why);
                }
            }
            continue;
        }

        push_follow(&mut result, out, &folder, &credit, spelling, any_in_scope);
        if !any_in_scope {
            continue;
        }
        if unasked {
            out.skip(folder_row, folder.clone(), folder.clone(), NOT_ASKED);
            continue;
        }
        // No credit in the songs: every part must already be an artist here, and MusicBrainz must
        // not know the whole name.
        if let Some(stranger) = pieces
            .iter()
            .find(|piece| !library_artists.contains(&navidrome_artist_text(piece)))
        {
            out.skip(
                folder_row,
                folder.clone(),
                folder.clone(),
                format!(
                    "The songs carry no artist credit, and \"{stranger}\" is not an artist in this library on its own, so nothing says the name is a list."
                ),
            );
            continue;
        }
        let Some(whole) = answers.artists.get(&artist_lookup_key(&credit)) else {
            out.wanted_artists.push(credit.clone());
            out.skip(folder_row, folder.clone(), folder.clone(), NOT_ASKED);
            continue;
        };
        if !whole.is_empty() {
            out.skip(
                folder_row,
                folder.clone(),
                folder.clone(),
                format!("MusicBrainz knows \"{credit}\" as one artist, so it stays whole."),
            );
            continue;
        }
        let first = primary_credit(&credit, &[Some(pieces[0].as_str())]);
        if first.to_lowercase() == credit.to_lowercase() {
            out.skip(
                folder_row,
                folder.clone(),
                folder.clone(),
                "The name does not split cleanly at its first artist.",
            );
            continue;
        }
        // Replace the follow decision pushed above.
        result.decisions.retain(|decision| decision.name != folder);
        result.confirmed.insert(credit.clone(), pieces.clone());
        push_split(
            &mut result,
            out,
            &folder,
            &first,
            &folder_row,
            format!(
                "{} are each already artists in this library, and MusicBrainz does not know \"{credit}\" as one artist.",
                quote_list(&pieces)
            ),
            spelling,
        );
    }
    result
}

/// What a song's ARTISTS (or its MusicBrainz credit) says about a folder's credit.
fn read_list(credit: &str, names: &[String]) -> Said {
    if names.len() == 1 && names[0].trim().to_lowercase() == credit.trim().to_lowercase() {
        return Said::Whole;
    }
    let primary = primary_credit(credit, &[names.first().map(String::as_str)]);
    if primary.to_lowercase() == credit.trim().to_lowercase() {
        Said::Contradicts
    } else {
        Said::Split(primary, names.to_vec())
    }
}

/// Why a folder may not be renamed to `target`: a name that starts with a dot is hidden from
/// Navidrome's scan, and an empty one is no name at all. None when the name is fine.
fn hidden_target(folder: &str, target: &str) -> Option<String> {
    let sanitized = PathHelper::sanitize_folder_name(target);
    if !target.trim().is_empty() && !PathHelper::is_hidden_name(&sanitized) {
        return None;
    }
    if nfc(&sanitized) == nfc(folder) {
        return None;
    }
    Some(format!(
        "The folder would be named \"{}\", and a name that is empty or would start with a dot is hidden from Navidrome's scan, so this folder stays as it is.",
        target.trim()
    ))
}

/// The folder follows its songs' credit, respelled when the spelling changes.
fn push_follow(
    result: &mut FolderResult,
    out: &mut Planned,
    folder: &str,
    credit: &str,
    spelling: &Spelling,
    in_scope: bool,
) {
    if let Some(why) = hidden_target(folder, &spelling.respell(credit)) {
        if in_scope {
            out.skip(format!("hidden|{folder}"), folder, folder, why);
        }
        return;
    }
    let row = match spelling.active.get(credit) {
        Some((_, row)) => Some(row.clone()),
        None if spelling.all.contains_key(credit) => None,
        None => Some(format!("folder|{folder}")),
    };
    result.decisions.push(FolderDecision {
        name: folder.to_string(),
        target: spelling.respell(credit),
        row,
    });
}

fn push_split(
    result: &mut FolderResult,
    out: &mut Planned,
    folder: &str,
    primary: &str,
    row: &str,
    evidence: String,
    spelling: &Spelling,
) {
    let target = spelling.respell(primary);
    if let Some(why) = hidden_target(folder, &target) {
        result.decisions.retain(|decision| decision.name != folder);
        out.skip(row.to_string(), folder, folder, why);
        return;
    }
    out.rows.push(TidyRow {
        id: row.to_string(),
        kind: TidyKind::Split,
        path: folder.to_string(),
        before: folder.to_string(),
        after: PathHelper::sanitize_folder_name(&target),
        evidence,
        files: 0,
        opt_in: false,
        ticked: true,
    });
    result.decisions.retain(|decision| decision.name != folder);
    result.decisions.push(FolderDecision {
        name: folder.to_string(),
        target,
        row: Some(row.to_string()),
    });
}

// ---- tags -----------------------------------------------------------------------------------

fn retags(root: &str, files: &[TidyFile], spelling: &Spelling, folders: &FolderResult, out: &mut Planned) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for file in files.iter().filter(|file| file.in_scope && file.readable) {
        let rel = relative(root, &file.path)
            .map(|s| s.join("/"))
            .unwrap_or_default();
        let before = &file.tags;
        let credit = before
            .artist
            .first()
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if !file.artist_ids.is_empty() {
            continue;
        }
        let rows: BTreeSet<&String> = all_values(before)
            .filter_map(|value| spelling.all.get(&value).map(|(_, row)| row))
            .collect();
        for row in rows {
            *counts.entry(row.clone()).or_default() += 1;
        }

        let artist: Vec<String> = before.artist.iter().map(|v| spelling.respell(v)).collect();
        let album_artist: Vec<String> = before.album_artist.iter().map(|v| spelling.respell(v)).collect();

        let mut guests_from_artist: Vec<String> = Vec::new();
        for value in &before.artist {
            guests_from_artist.extend(SongIdentity::parse_title(value, None).featured);
        }
        let guests_from_title: Vec<String> = match file.title.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(title) => SongIdentity::parse_title(title, Some(&credit)).featured,
            None => Vec::new(),
        };

        let confirmed = folders.confirmed.get(&credit);
        let mut names: Vec<String> = if !before.artists.is_empty() {
            before.artists.clone()
        } else if let Some(confirmed) = confirmed {
            confirmed.clone()
        } else {
            // The artists Navidrome already lists for the file: "A; B" is two of them.
            let credit: Vec<String> = before
                .artist
                .iter()
                .map(|value| {
                    if SongIdentity::parse_title(value, None).featured.is_empty() {
                        value.trim().to_string()
                    } else {
                        SongIdentity::strip_features(value)
                    }
                })
                .filter(|value| !value.is_empty())
                .collect();
            navidrome_artist_parts(&credit)
        };
        let mut added: Vec<String> = Vec::new();
        for guest in guests_from_artist.iter().chain(&guests_from_title) {
            if names.iter().any(|name| same_name(name, guest)) {
                continue;
            }
            names.push(guest.clone());
            added.push(guest.clone());
        }
        let mut seen = HashSet::new();
        let names: Vec<String> = names
            .into_iter()
            .map(|name| spelling.respell(&name))
            .filter(|name| seen.insert(name.clone()))
            .collect();
        // A new download writes ARTISTS only for a credit of more than one artist, and only
        // what the credit or a guest says.
        let lone = names.len() <= 1 || (added.is_empty() && confirmed.is_none());
        let artists = if before.artists.is_empty() && lone {
            Vec::new()
        } else {
            names
        };

        let after = TidyTags {
            artist,
            album_artist,
            artists,
        };
        if after != *before {
            if file.slash_artist {
                out.skip(
                    format!("slash|{rel}"),
                    rel.clone(),
                    before.artist.join("; "),
                    "This file's artist frame (ID3v2.3) holds a \"/\" that cannot be written back the way it is, so its tags are left alone.",
                );
                continue;
            }
            if after.artists != before.artists && !added.is_empty() {
                let why = if guests_from_title.iter().any(|g| added.contains(g)) {
                    format!("The title names {} as a guest.", join_words(&added))
                } else {
                    format!("The artist credit names {} as a guest.", join_words(&added))
                };
                out.rows.push(TidyRow {
                    id: format!("guest|{rel}"),
                    kind: TidyKind::Guest,
                    path: rel.clone(),
                    before: show(&before.artists),
                    after: show(&after.artists),
                    evidence: why,
                    files: 1,
                    opt_in: false,
                    ticked: true,
                });
            }
            out.retags.push(TidyRetag {
                path: file.path.clone(),
                before: before.clone(),
                after,
            });
        } else if added.is_empty()
            && let Some(title) = file.title.as_deref().filter(|t| !t.trim().is_empty())
        {
            if guests_from_title.is_empty() && GUEST_WORD.is_match(title) {
                out.skip(
                    format!("guestword|{rel}"),
                    rel.clone(),
                    title,
                    "The title seems to name a guest, but not in a form Octo reads, so ARTISTS is left alone.",
                );
            } else if SESSION_TITLE.is_match(title) {
                out.skip(
                    format!("session|{rel}"),
                    rel.clone(),
                    title,
                    "A session title may name a second artist, but nothing says which part is the guest.",
                );
            }
        }
    }
    for row in &mut out.rows {
        if let Some(count) = counts.get(&row.id) {
            row.files += count;
        }
    }
}

// ---- moves ----------------------------------------------------------------------------------

fn moves(root: &str, files: &[TidyFile], folders: &FolderResult, options: &TidyOptions, out: &mut Planned) {
    let folder_key = |rel: &str| {
        if options.case_insensitive {
            nfc_lower(rel)
        } else {
            nfc(rel)
        }
    };
    let taken: HashSet<String> = files
        .iter()
        .filter_map(|file| relative(root, &file.path))
        .filter(|segments| segments.len() >= 3)
        .map(|segments| folder_key(&format!("{}/{}", segments[0], segments[1])))
        .collect();
    let mut decisions: Vec<&FolderDecision> = folders.decisions.iter().collect();
    decisions.sort_by(|a, b| a.name.cmp(&b.name));
    for decision in decisions {
        let Some(row) = decision.row.clone() else {
            continue;
        };
        if decision.target.trim().is_empty() {
            continue;
        }
        let target = PathHelper::sanitize_folder_name(&decision.target);
        // A folder that only differs from its target by what cleaning changes (a character
        // Windows refuses) is the File layout task's to rename, never a respelling.
        if nfc(&target) == nfc(&decision.name)
            || PathHelper::safe_name(&target, None)
                == PathHelper::safe_name(&PathHelper::sanitize_folder_name(&decision.name), None)
        {
            continue;
        }
        let folder_files: Vec<&TidyFile> = files
            .iter()
            .filter(|file| relative(root, &file.path).is_some_and(|s| s.len() >= 2 && s[0] == decision.name))
            .collect();
        if !folder_files.iter().any(|file| file.in_scope) {
            continue;
        }
        if options.case_insensitive && target.to_lowercase() == decision.name.to_lowercase() {
            out.skip(
                format!("case|{}", decision.name),
                decision.name.clone(),
                decision.name.clone(),
                format!(
                    "Only the capitals differ from \"{target}\", and this filesystem cannot rename a folder by capitals alone."
                ),
            );
            continue;
        }

        let loose: Vec<&TidyFile> = folder_files
            .iter()
            .copied()
            .filter(|file| relative(root, &file.path).is_some_and(|s| s.len() == 2))
            .collect();
        if !loose.is_empty() {
            out.skip(
                format!("loose|{}", decision.name),
                decision.name.clone(),
                decision.name.clone(),
                format!(
                    "{} directly in the artist folder rather than in an album folder, so {}.",
                    counted(loose.len(), "song sits", "songs sit"),
                    if loose.len() == 1 {
                        "it stays where it is"
                    } else {
                        "they stay where they are"
                    }
                ),
            );
        }

        let mut albums: BTreeMap<String, Vec<&TidyFile>> = BTreeMap::new();
        for file in folder_files {
            if let Some(segments) = relative(root, &file.path)
                && segments.len() >= 3
            {
                albums.entry(segments[1].clone()).or_default().push(file);
            }
        }
        let mut moved = 0;
        for (album, album_files) in albums {
            let from_rel = format!("{}/{album}", decision.name);
            let to_rel = format!("{target}/{album}");
            let outside = album_files.iter().filter(|file| !file.in_scope).count();
            if outside > 0 {
                out.skip(
                    format!("album|{from_rel}"),
                    from_rel.clone(),
                    from_rel,
                    format!(
                        "{outside} of the {} in this album folder {} not in this run, and a folder only moves whole.",
                        counted(album_files.len(), "song", "songs"),
                        if outside == 1 { "is" } else { "are" }
                    ),
                );
                continue;
            }
            if folder_key(&to_rel) != folder_key(&from_rel) && taken.contains(&folder_key(&to_rel)) {
                out.skip(
                    format!("album|{from_rel}"),
                    from_rel.clone(),
                    from_rel,
                    format!("There is already an album folder at {to_rel}, so both stay as they are."),
                );
                continue;
            }
            moved += album_files.len();
            out.moves.push(TidyMove {
                from: format!("{root}/{from_rel}"),
                to: format!("{root}/{to_rel}"),
                row: row.clone(),
                files: album_files.iter().map(|file| file.path.clone()).collect(),
            });
        }
        if moved == 0 {
            continue;
        }
        if let Some(existing) = out.rows.iter_mut().find(|r| r.id == row) {
            existing.files += moved;
            if existing.kind == TidyKind::Split || existing.path.is_empty() {
                continue;
            }
        }
        if !out.rows.iter().any(|r| r.id == row) {
            out.rows.push(TidyRow {
                id: row,
                kind: TidyKind::Respell,
                path: decision.name.clone(),
                before: decision.name.clone(),
                after: target.clone(),
                evidence: format!("The songs in this folder are tagged \"{}\".", decision.target),
                files: moved,
                opt_in: false,
                ticked: true,
            });
        }
    }
}

// ---- helpers --------------------------------------------------------------------------------

/// The path's folders and file under the root, or None outside it.
pub fn relative(root: &str, path: &str) -> Option<Vec<String>> {
    let root = root.trim_end_matches('/');
    let rest = path.strip_prefix(root)?.strip_prefix('/')?;
    let segments: Vec<String> = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (!segments.is_empty()).then_some(segments)
}

/// A dot folder or dot file (the trash, a NAS's metadata) is never tidied or counted; Navidrome
/// does not read them either. A name of two dots or more (`...And Justice for All`) is listed.
fn is_hidden(segments: &[String]) -> bool {
    segments.iter().any(|segment| PathHelper::is_hidden_name(segment))
}

pub(crate) fn is_excluded(root: &str, path: &str, excluded: &[String]) -> bool {
    let Some(segments) = relative(root, path) else {
        return false;
    };
    let rel = segments.join("/");
    excluded.iter().any(|folder| {
        let folder = folder.trim_matches('/');
        !folder.is_empty() && (rel == folder || rel.starts_with(&format!("{folder}/")))
    })
}

fn nfc(value: &str) -> String {
    value.nfc().collect()
}

fn nfc_lower(value: &str) -> String {
    nfc(value).to_lowercase()
}

fn most_common<'a>(values: impl Iterator<Item = &'a String>) -> Option<String> {
    let mut counts: BTreeMap<&String, usize> = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    let most = counts.values().copied().max()?;
    counts
        .into_iter()
        .find(|(_, count)| *count == most)
        .map(|(value, _)| value.clone())
}

fn same(a: Option<&String>, b: &str) -> bool {
    a.is_some_and(|a| a.trim().to_lowercase() == b.trim().to_lowercase())
}

fn same_name(a: &str, b: &str) -> bool {
    let key = SongIdentity::loose_key(a);
    !key.is_empty() && key == SongIdentity::loose_key(b)
}

fn show(values: &[String]) -> String {
    if values.is_empty() {
        "(none)".to_string()
    } else {
        values.join(" \u{b7} ")
    }
}

fn quote_list(values: &[String]) -> String {
    let quoted: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
    join_words(&quoted)
}

// "1 song" or "3 songs".
pub(super) fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn join_words(values: &[String]) -> String {
    match values {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
#[path = "planner_tests.rs"]
mod tests;
