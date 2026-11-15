//! Fixing a set of copies so the best sound ends up in the copy people marked. Navidrome keeps a
//! song's id, and with it every favorite, rating, play and playlist place, for as long as a file
//! stays at the same path. So the copy whose id carries the most marks keeps its path, the
//! best-sounding audio moves onto that path under that copy's own identity tags, and the
//! best-sounding copy's old file goes to the trash. Playlist entries of the copy that went are
//! moved onto the one that stayed, each in its own place, once Navidrome shows the moved song as
//! the same song; when it does not, both copies go back as they were. Every copy a fix removes
//! has its stars, ratings and playlist places carried onto the copy that stays, and putting it
//! back takes them off again. With every rated copy kept, a copy anyone starred or rated is never
//! removed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use futures::future::{BoxFuture, join_all};
use indexmap::IndexMap;
use octo_core::common::dotnet::{eq_ignore_case, is_blank};
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::settings::{LibraryAction, SettingsStore};
use octo_media::tags::TagFile;
use octo_media::tags::kept_identity::{ARTISTS_KEYS, KeptIdentityTags, NavidromeView};
use octo_media::tags::tag_writer_extras::{self as extras, TagField, TagFields};
use parking_lot::Mutex;
use tracing::{info, warn};

use super::health_fixes::{self as fixes, DuplicateFix, FixAnswer};
use super::library_health::{DuplicateSet, IHealthFields};
use super::library_health_service::Row;
use crate::services::library::library_tag_edits::{self as tag_edits, song_tag_fields};
use crate::services::library::library_write_gate::{LibraryWrite, LibraryWriteGate};
use crate::services::library::navidrome_playlist_api::{PlaylistKind, SongMarks};
use crate::services::library::{
    LibraryActionEntry, LibraryActionExecutor, LibraryActionJournal, LibraryActionQuarantine,
    LibraryActionRequest, LibraryActionState, NavidromePlaylistApi, NavidromeSongPathResolver,
    ResolvedSongFile,
};
use crate::services::local::ILocalLibraryService;
use crate::services::playlists::held_playlists::HeldPlaylists;

/// One copy of a set, as the pick reads it: its id and when Navidrome first saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRef {
    pub id: String,
    pub created: Option<DateTime<Utc>>,
}

/// What one reader's marks on a copy are worth: a favorite 3, any rating 2, and each play 1 up
/// to 50, so one person's habit of replaying a song never outweighs everyone's favorites.
pub fn marks_score(marks: &SongMarks) -> i64 {
    let starred = if marks.starred { 3 } else { 0 };
    let rated = if marks.rating > 0 { 2 } else { 0 };
    starred + rated + marks.plays.clamp(0, 50)
}

/// Whether one reader starred or rated a song. Plays and playlist places are not counted here:
/// a fix moves playlist places to the copy that stays.
pub fn starred_or_rated(marks: &SongMarks) -> bool {
    marks.starred || marks.rating > 0
}

/// Who left marks on a song: the admin Octo signs in as, or a Family member by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Reader {
    Admin,
    Member(String),
}

/// One reader's marks on one song, or None when they could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderMarks {
    pub reader: Reader,
    pub marks: Option<SongMarks>,
}

impl ReaderMarks {
    pub fn read(reader: Reader, marks: SongMarks) -> Self {
        ReaderMarks {
            reader,
            marks: Some(marks),
        }
    }
}

/// What every reader left on each copy of a set, by copy id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetMarks(pub HashMap<String, Vec<ReaderMarks>>);

impl SetMarks {
    /// Every reader's marks on one copy.
    pub fn of(&self, id: &str) -> &[ReaderMarks] {
        self.0.get(id).map_or(&[], Vec::as_slice)
    }

    /// Every reader's marks on one copy, summed; marks that could not be read count nothing.
    pub fn score(&self, id: &str) -> i64 {
        self.of(id)
            .iter()
            .filter_map(|read| read.marks.as_ref())
            .map(marks_score)
            .sum()
    }

    /// Whether someone starred or rated the copy.
    pub fn starred_or_rated(&self, id: &str) -> bool {
        self.of(id)
            .iter()
            .filter_map(|read| read.marks.as_ref())
            .any(starred_or_rated)
    }

    /// Whether some reader's marks on the copy could not be read.
    pub fn unread(&self, id: &str) -> bool {
        self.of(id).iter().any(|read| read.marks.is_none())
    }

    /// Whether the copy has to stay when every rated copy is kept: someone starred or rated it,
    /// or someone's marks on it could not be read, so it may be rated.
    pub fn keeps(&self, id: &str) -> bool {
        self.starred_or_rated(id) || self.unread(id)
    }

    /// Each copy's score, by id.
    pub fn scores(&self) -> HashMap<String, i64> {
        self.0.keys().map(|id| (id.clone(), self.score(id))).collect()
    }
}

/// The marks people other than the admin left on a song, each person's read and written with
/// their own sign-in.
pub trait MemberMarks: Send + Sync {
    /// Each member's marks on one song by name; None for a member whose marks could not be read.
    fn marks_of<'a>(&'a self, song_id: &'a str) -> BoxFuture<'a, Vec<(String, Option<SongMarks>)>>;
    /// Sets one member's rating on a song, 0 to clear it. True when Navidrome said ok.
    fn set_rating<'a>(&'a self, member: &'a str, song_id: &'a str, rating: i32) -> BoxFuture<'a, bool>;
    /// Stars or unstars a song as one member. True when Navidrome said ok.
    fn set_starred<'a>(&'a self, member: &'a str, song_id: &'a str, starred: bool) -> BoxFuture<'a, bool>;
}

/// What carrying one copy's marks onto another changes, per reader: the higher of the two
/// ratings, and a star when either copy had one. Each rating raised comes with the rating the
/// kept copy had and the one it gets. Marks that could not be read carry nothing, and plays are
/// never carried.
pub fn carry_plan(from: &[ReaderMarks], into: &[ReaderMarks]) -> (Vec<(Reader, i32, i32)>, Vec<Reader>) {
    let mut ratings = Vec::new();
    let mut stars = Vec::new();
    for read in from {
        let Some(gone) = &read.marks else {
            continue;
        };
        let kept = into.iter().find(|other| other.reader == read.reader);
        // The kept copy's marks for this reader must be known before anything is written over them.
        let Some(kept) = kept.map_or(Some(SongMarks::default()), |kept| kept.marks) else {
            continue;
        };
        if gone.rating > kept.rating {
            ratings.push((read.reader.clone(), kept.rating, gone.rating));
        }
        if gone.starred && !kept.starred {
            stars.push(read.reader.clone());
        }
    }
    (ratings, stars)
}

/// Marks carried from a removed copy onto the kept one, so Put back can take them off again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Carried {
    /// The kept copy.
    pub into: String,
    /// Each reader whose rating on the kept copy was raised: the rating it had, and the one set.
    pub ratings: Vec<(Reader, i32, i32)>,
    /// Each reader who has the kept copy starred because they starred the removed one.
    pub stars: Vec<Reader>,
    /// The playlist places moved from the removed copy onto the kept one: playlist and position.
    pub places: Vec<(String, usize)>,
}

impl Carried {
    fn is_empty(&self) -> bool {
        self.ratings.is_empty() && self.stars.is_empty() && self.places.is_empty()
    }
}

/// Why the copy that keeps its id was picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetReason {
    /// People starred, rated or played it more than the others.
    Marks,
    /// No marks anywhere, and more playlist entries than the others.
    Playlists,
    /// No marks or playlist entries anywhere: the oldest copy, the likeliest holder of marks
    /// nobody here can read.
    Oldest,
    /// Nothing tells the copies apart: the first in the set's own order.
    First,
}

/// The copy that keeps its id, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPick {
    pub target: String,
    pub reason: TargetReason,
}

/// The copy whose id carries the most: the highest marks score, then the most playlist entries,
/// then the oldest, then the set's own order. None for no copies.
pub fn pick_target(
    copies: &[CopyRef],
    scores: &HashMap<String, i64>,
    entries: &HashMap<String, usize>,
) -> Option<TargetPick> {
    let score = |copy: &CopyRef| scores.get(&copy.id).copied().unwrap_or(0);
    let listed = |copy: &CopyRef| entries.get(&copy.id).copied().unwrap_or(0);
    let mut order: Vec<usize> = (0..copies.len()).collect();
    order.sort_by(|a, b| {
        let (a_copy, b_copy) = (&copies[*a], &copies[*b]);
        score(b_copy)
            .cmp(&score(a_copy))
            .then(listed(b_copy).cmp(&listed(a_copy)))
            .then(match (a_copy.created, b_copy.created) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then(a.cmp(b))
    });
    let top = &copies[*order.first()?];
    let reason = if score(top) > 0 {
        TargetReason::Marks
    } else if listed(top) > 0 {
        TargetReason::Playlists
    } else if copies.iter().any(|copy| copy.created.is_some())
        && copies.iter().any(|copy| copy.created != top.created)
    {
        TargetReason::Oldest
    } else {
        TargetReason::First
    };
    Some(TargetPick {
        target: top.id.clone(),
        reason,
    })
}

/// How many entries each song has across the playlists people made. Smart playlists are left
/// out: their tracks follow their rules, not anyone's choice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistUses(pub HashMap<String, usize>);

/// Whether two files are the same format by extension. Navidrome's id follows the path, and a
/// changed extension is a changed path.
pub fn same_extension(a: &str, b: &str) -> bool {
    let ext = |path: &str| {
        Path::new(path)
            .extension()
            .map(|ext| ext.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let (a, b) = (ext(a), ext(b));
    !a.is_empty() && eq_ignore_case(&a, &b)
}

/// The plan for a set of copies that keeps people's marks: with `pick`, the best sound moves
/// into the picked copy when both are the same format, and the picked copy is kept as it is when
/// they are not. Without a pick, or when the best-sounding copy is the picked one, the usual
/// plan.
pub fn plan(
    set: &DuplicateSet<Row>,
    fields: &dyn IHealthFields<Row>,
    pick: Option<&TargetPick>,
) -> DuplicateFix<Row> {
    let target = pick.and_then(|pick| set.copies.iter().find(|copy| copy.id == pick.target));
    match (pick, target) {
        (Some(pick), Some(target)) => {
            let same_format = same_extension(&target.path, &set.best().path);
            fixes::duplicate_fix_keeping_marks(set, fields, target, pick.reason, same_format)
        }
        _ => fixes::duplicate_fix(set, fields, None),
    }
}

/// The copy of a set whose id keeps the most people's marks, from marks already read: among
/// every copy, or with `rated_only` among the copies someone starred or rated.
pub fn pick(
    set: &DuplicateSet<Row>,
    marks: &SetMarks,
    uses: &PlaylistUses,
    rated_only: bool,
) -> Option<TargetPick> {
    let copies: Vec<CopyRef> = set
        .copies
        .iter()
        .filter(|copy| !rated_only || marks.keeps(&copy.id))
        .map(|copy| CopyRef {
            id: copy.id.clone(),
            created: copy.added_utc,
        })
        .collect();
    pick_target(&copies, &marks.scores(), &uses.0)
}

/// The plan for a set of copies from every reader's marks, when they were read: with
/// `keep_rated`, the one that keeps every rated copy (None when every copy is rated); without,
/// the one that keeps people's marks, or the copy a person `picked`. With no marks read, the
/// best copy or the picked one.
pub fn plan_with_marks(
    set: &DuplicateSet<Row>,
    fields: &dyn IHealthFields<Row>,
    marks: Option<&SetMarks>,
    uses: &PlaylistUses,
    keep_rated: bool,
    picked: Option<&Row>,
) -> Option<DuplicateFix<Row>> {
    match (marks, keep_rated) {
        (Some(marks), true) => plan_keeping_rated(set, fields, marks, uses, picked),
        (Some(marks), false) => Some(match picked {
            Some(keep) => fixes::duplicate_fix(set, fields, Some(keep)),
            None => plan(set, fields, pick(set, marks, uses, false).as_ref()),
        }),
        (None, _) => Some(fixes::duplicate_fix(set, fields, picked)),
    }
}

/// The plan for a set of copies that keeps every copy someone starred or rated, or None when
/// nothing would go because every copy is starred or rated. Without a rated copy, the plan that
/// keeps people's marks. When the best-sounding copy is rated, it is the copy kept first; when it
/// is not, the best sound moves into the rated copy with the most marks, as the plan that keeps
/// people's marks does. `picked` is the copy a person picked to keep first. Every other rated
/// copy stays as it is, and a copy whose marks could not be read for everyone stays too; when
/// such copies are the only ones kept for that reason, the best sound stays in its own copy.
pub fn plan_keeping_rated(
    set: &DuplicateSet<Row>,
    fields: &dyn IHealthFields<Row>,
    marks: &SetMarks,
    uses: &PlaylistUses,
    picked: Option<&Row>,
) -> Option<DuplicateFix<Row>> {
    let rated = |copy: &Row| marks.keeps(&copy.id);
    let any_rated = set.copies.iter().any(&rated);
    let mut fix = match picked {
        Some(keep) => fixes::duplicate_fix(set, fields, Some(keep)),
        None if !any_rated => plan(set, fields, pick(set, marks, uses, false).as_ref()),
        None if rated(set.best()) => fixes::duplicate_fix(set, fields, None),
        None => plan(set, fields, pick(set, marks, uses, true).as_ref()),
    };
    fixes::keep_as_well(&mut fix, fields, rated);
    (!fix.remove.is_empty() || fix.audio.is_some()).then_some(fix)
}

/// The ARTISTS field Navidrome reads every track artist from.
const ARTISTS: TagField = TagField {
    id3_frame: None,
    id3_description: Some("ARTISTS"),
    vorbis: "ARTISTS",
    mp4: "ARTISTS",
};

const ARTIST_ID_KEYS: [&str; 2] = ["musicbrainz_artistid", "musicbrainz artist id"];

fn view_values(view: &NavidromeView, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|key| view.get(*key).filter(|values| !values.is_empty()))
        .cloned()
        .unwrap_or_default()
}

/// Writes `from`'s identity onto `onto`: title, track artists, album, album artists, the
/// MusicBrainz ids, and the track and disc numbers, everything Navidrome files the song by. The
/// audio and every other tag of `onto` stay. `album_artist` is the album artist Navidrome stored
/// for `from`, used only when its file names none.
pub fn carry_identity(from: &Path, onto: &Path, album_artist: Option<&str>) -> Result<(), String> {
    let identity =
        KeptIdentityTags::read(from, album_artist).ok_or("the marked copy's tags could not be read")?;
    let mut source = TagFile::open(from).map_err(|e| tag_edits::why(&e))?;
    let performers = source.performers();
    let view = KeptIdentityTags::navidrome_view(&source);
    let artists = view_values(&view, &ARTISTS_KEYS);
    let artist_ids = view_values(&view, &ARTIST_ID_KEYS);
    let recording = extras::read_recording_id(&mut source);
    KeptIdentityTags::apply(onto, &identity).map_err(|e| tag_edits::why(&e))?;
    let mut file = TagFile::open(onto).map_err(|e| tag_edits::why(&e))?;
    file.set_performers(&performers);
    extras::set_exact(&mut file, ARTISTS, &artists);
    extras::set_exact(&mut file, TagFields::ARTIST_ID, &artist_ids);
    if let Some(recording) = recording {
        extras::set_recording_id(&mut file, &recording);
    }
    file.save().map_err(|e| tag_edits::why(&e))
}

/// The best audio copied beside the marked copy under a hidden name Navidrome's scanner skips,
/// carrying the marked copy's identity and the tags a person picked. Nothing in the library has
/// changed when this fails, and nothing is left behind.
fn stage(
    audio: &Path,
    target: &Path,
    album_artist: Option<&str>,
    changes: &IndexMap<String, Option<String>>,
) -> Result<PathBuf, String> {
    let folder = target.parent().ok_or("the marked copy has no folder")?;
    let extension = target
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default();
    let staged = folder.join(format!(
        "{STAGING_PREFIX}{}.{extension}",
        uuid::Uuid::new_v4().simple()
    ));
    let made = (|| {
        let copied = std::fs::copy(audio, &staged).map_err(|e| e.to_string())?;
        let size = std::fs::metadata(audio).map_err(|e| e.to_string())?.len();
        if copied != size {
            return Err("the copy did not match the original's size".to_string());
        }
        carry_identity(target, &staged, album_artist)?;
        if !changes.is_empty()
            && let Some(error) = tag_edits::write(&staged, changes).error
        {
            return Err(error);
        }
        Ok(())
    })();
    match made {
        Ok(()) => Ok(staged),
        Err(why) => {
            let _ = std::fs::remove_file(&staged);
            Err(why)
        }
    }
}

/// One move done here: both copies as they were, where each waits in the trash, and the file
/// that moved in.
#[derive(Clone)]
struct Moved {
    target: ResolvedSongFile,
    audio: ResolvedSongFile,
    user: String,
    target_key: String,
    target_trash: String,
    audio_key: String,
    audio_trash: String,
    size: u64,
    modified: Option<SystemTime>,
    /// The playlist places moved from the dropped copy to the kept one: playlist and position.
    rewritten: Vec<(String, usize)>,
    /// Every reader's marks on the best-sounding copy, read before it went.
    audio_marks: Vec<ReaderMarks>,
    /// The stars and ratings carried from the best-sounding copy onto the kept one.
    carried: Carried,
}

/// How the playlist entries of one song moved onto another.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Rewrite {
    /// Each place moved: the playlist and the entry's position, unchanged.
    pub moved: Vec<(String, usize)>,
    /// Smart and synced playlists holding the song, whose tracks Navidrome does not let anyone
    /// edit, by name.
    pub skipped: Vec<String>,
    /// Playlists where moving an entry failed part way, by name.
    pub failed: Vec<String>,
}

pub const FORMAT_DIFFERS: &str = "The best-sounding copy is a different format, so moving it would change the song's path and lose its marks. Nothing changed.";
pub const CHANGED_SINCE: &str = "The file has changed since, so it was left as it is.";
pub const CHECKING: &str = "Checking that Navidrome kept the moved songs";
/// A step that removes only an unrated copy found it starred or rated now.
pub const RATED_SINCE: &str = "Kept: someone starred or rated this copy since the preview.";
/// A step that removes only an unrated copy could not read everyone's marks on it.
pub const MARKS_UNREAD: &str = "Kept: Octo could not read everyone's stars and ratings on this copy.";

/// The step parameter that has a remove or a move go only while nobody starred or rated the copy
/// that goes.
pub const UNRATED_ONLY: &str = "unratedOnly";
/// The step parameter that names the copy a removed copy's marks move to.
pub const INTO: &str = "into";

/// Whether a step's parameter is set.
pub fn flag(value: Option<&str>) -> bool {
    matches!(value, Some("true" | "1" | "True"))
}

/// Why a step that removes only an unrated copy leaves this one, or None when it may go.
pub fn kept_because(marks: &[ReaderMarks]) -> Option<&'static str> {
    if marks
        .iter()
        .filter_map(|read| read.marks.as_ref())
        .any(starred_or_rated)
    {
        Some(RATED_SINCE)
    } else if marks.iter().any(|read| read.marks.is_none()) {
        Some(MARKS_UNREAD)
    } else {
        None
    }
}

/// Fixing duplicates by keeping the best sound in the copy people marked, and carrying the marks
/// of every copy that goes onto the one that stays.
pub struct KeepMarks {
    api: Arc<NavidromePlaylistApi>,
    resolver: Arc<NavidromeSongPathResolver>,
    quarantine: Arc<LibraryActionQuarantine>,
    journal: Arc<LibraryActionJournal>,
    library: Arc<dyn ILocalLibraryService>,
    executor: Arc<LibraryActionExecutor>,
    settings: Arc<SettingsStore>,
    members: Mutex<Option<Arc<dyn MemberMarks>>>,
    write_gate: Option<LibraryWriteGate>,
    moves: Mutex<HashMap<String, Moved>>,
    returning: Mutex<HashMap<String, Moved>>,
    /// What was carried from each removed copy, by its id, until it is put back.
    carries: Mutex<HashMap<String, Carried>>,
    /// Copies put back whose playlist places go back at the end of the run.
    uncarrying: Mutex<Vec<(String, Carried)>>,
    /// Every song's playlist entries, read once a run.
    uses: Mutex<Option<PlaylistUses>>,
    /// Things worth saying at the end of the run.
    notes: Mutex<Vec<String>>,
    /// The held playlists, once they are built: every playlist swap runs under the playlist's
    /// lock, and a held playlist is swapped through them.
    held: Mutex<Option<Arc<HeldPlaylists>>>,
}

/// What [`KeepMarks`] is built from.
pub struct KeepMarksParts {
    pub api: Arc<NavidromePlaylistApi>,
    pub resolver: Arc<NavidromeSongPathResolver>,
    pub quarantine: Arc<LibraryActionQuarantine>,
    pub journal: Arc<LibraryActionJournal>,
    pub library: Arc<dyn ILocalLibraryService>,
    pub executor: Arc<LibraryActionExecutor>,
    pub settings: Arc<SettingsStore>,
    pub members: Option<Arc<dyn MemberMarks>>,
    /// The music folder's one-writer gate: each move takes a share, so it waits while a job
    /// rewrites the folder.
    pub write_gate: Option<LibraryWriteGate>,
}

impl KeepMarks {
    pub fn new(parts: KeepMarksParts) -> Self {
        KeepMarks {
            api: parts.api,
            resolver: parts.resolver,
            quarantine: parts.quarantine,
            journal: parts.journal,
            library: parts.library,
            executor: parts.executor,
            settings: parts.settings,
            members: Mutex::new(parts.members),
            write_gate: parts.write_gate,
            moves: Mutex::new(HashMap::new()),
            returning: Mutex::new(HashMap::new()),
            carries: Mutex::new(HashMap::new()),
            uncarrying: Mutex::new(Vec::new()),
            uses: Mutex::new(None),
            notes: Mutex::new(Vec::new()),
            held: Mutex::new(None),
        }
    }

    /// Swaps playlist entries under the held playlists' locks from now on.
    pub fn use_held(&self, held: Arc<HeldPlaylists>) {
        *self.held.lock() = Some(held);
    }

    fn held(&self) -> Option<Arc<HeldPlaylists>> {
        self.held.lock().clone()
    }

    /// Reads and writes Family members' marks through `members` from now on.
    pub fn use_members(&self, members: Arc<dyn MemberMarks>) {
        *self.members.lock() = Some(members);
    }

    fn members(&self) -> Option<Arc<dyn MemberMarks>> {
        self.members.lock().clone()
    }

    /// A new run: playlists are read again, and nothing from the last run is left to say.
    pub fn begin_run(&self) {
        *self.uses.lock() = None;
        self.notes.lock().clear();
    }

    /// What is worth saying about the run, taken.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock())
    }

    // ---- which copy keeps its id ----------------------------------------------------------

    /// Every song's entries across the playlists people made, or `None` when Navidrome could not
    /// list them.
    pub async fn playlist_uses(&self) -> Option<PlaylistUses> {
        let playlists = self.api.list_playlist_kinds().await?;
        let mut uses: HashMap<String, usize> = HashMap::new();
        for playlist in playlists.iter().filter(|playlist| !playlist.smart) {
            let Ok(Some(tracks)) = self.api.read_all_tracks(&playlist.id).await else {
                continue;
            };
            for track in tracks {
                *uses.entry(track.media_file_id).or_default() += 1;
            }
        }
        Some(PlaylistUses(uses))
    }

    /// Every reader's marks on one song: the admin's own, then each member's.
    pub async fn marks(&self, song_id: &str) -> Vec<ReaderMarks> {
        let members = self.members();
        let (admin, members) = futures::join!(self.api.song_marks(song_id), async {
            match &members {
                Some(members) => members.marks_of(song_id).await,
                None => Vec::new(),
            }
        });
        std::iter::once(ReaderMarks {
            reader: Reader::Admin,
            marks: admin,
        })
        .chain(members.into_iter().map(|(name, marks)| ReaderMarks {
            reader: Reader::Member(name),
            marks,
        }))
        .collect()
    }

    /// Every reader's marks on one copy, summed: the admin's own, then each member's.
    pub async fn score(&self, song_id: &str) -> i64 {
        self.marks(song_id)
            .await
            .iter()
            .filter_map(|read| read.marks.as_ref())
            .map(marks_score)
            .sum()
    }

    /// Every reader's marks on each copy of a set, read side by side.
    pub async fn read_set(&self, set: &DuplicateSet<Row>) -> SetMarks {
        let read = join_all(set.copies.iter().map(|copy| self.marks(&copy.id))).await;
        SetMarks(set.copies.iter().map(|copy| copy.id.clone()).zip(read).collect())
    }

    /// The copy of `copies` whose id keeps the most people's marks.
    pub async fn marked_target(&self, copies: &[CopyRef], uses: &PlaylistUses) -> Option<TargetPick> {
        let scores: Vec<i64> = join_all(copies.iter().map(|copy| self.score(&copy.id))).await;
        let scores: HashMap<String, i64> = copies
            .iter()
            .zip(scores)
            .map(|(copy, score)| (copy.id.clone(), score))
            .collect();
        pick_target(copies, &scores, &uses.0)
    }

    /// The copy of a set of duplicates whose id keeps the most people's marks.
    pub async fn pick_for(&self, set: &DuplicateSet<Row>, uses: &PlaylistUses) -> Option<TargetPick> {
        pick(set, &self.read_set(set).await, uses, false)
    }

    /// Takes off the rating whoever `credential` signs in as gave `song_id`.
    pub async fn clear_rating_as(
        &self,
        credential: &octo_subsonic::SubsonicCredential,
        song_id: &str,
    ) -> bool {
        self.api.set_rating_as(credential, song_id, 0).await
    }

    // ---- carrying marks over --------------------------------------------------------------

    async fn set_rating(&self, reader: &Reader, song_id: &str, rating: i32) -> bool {
        match reader {
            Reader::Admin => self.api.set_admin_rating(song_id, rating).await,
            Reader::Member(name) => match self.members() {
                Some(members) => members.set_rating(name, song_id, rating).await,
                None => false,
            },
        }
    }

    async fn set_starred(&self, reader: &Reader, song_id: &str, starred: bool) -> bool {
        match reader {
            Reader::Admin => self.api.set_admin_starred(song_id, starred).await,
            Reader::Member(name) => match self.members() {
                Some(members) => members.set_starred(name, song_id, starred).await,
                None => false,
            },
        }
    }

    /// Whether `song_id` has an entry in a playlist people made, from one read of the playlists
    /// a run. True when the playlists could not be read, so the rewrite looks for itself.
    async fn listed(&self, song_id: &str) -> bool {
        let known = self.uses.lock().clone();
        let uses = match known {
            Some(uses) => uses,
            None => match self.playlist_uses().await {
                Some(uses) => {
                    *self.uses.lock() = Some(uses.clone());
                    uses
                }
                None => return true,
            },
        };
        uses.0.get(song_id).copied().unwrap_or(0) > 0
    }

    /// Puts `from`'s stars and ratings on `into` for each reader, the higher rating and a star
    /// when either had one, given every reader's marks on `from` read before it went. Answers
    /// what was put on, and how many could not be.
    async fn carry_marks(&self, from_marks: &[ReaderMarks], into: &str) -> (Carried, usize) {
        let mut carried = Carried {
            into: into.to_string(),
            ..Carried::default()
        };
        let worth = from_marks
            .iter()
            .filter_map(|read| read.marks.as_ref())
            .any(starred_or_rated);
        if !worth {
            return (carried, 0);
        }
        let into_marks = self.marks(into).await;
        let (ratings, stars) = carry_plan(from_marks, &into_marks);
        let mut failed = 0;
        for (reader, before, rating) in ratings {
            if self.set_rating(&reader, into, rating).await {
                carried.ratings.push((reader, before, rating));
            } else {
                failed += 1;
            }
        }
        for reader in stars {
            if self.set_starred(&reader, into, true).await {
                carried.stars.push(reader);
            } else {
                failed += 1;
            }
        }
        (carried, failed)
    }

    /// After `from` was removed as a copy of `into`: every reader's star and rating on `from`
    /// goes onto `into` (the higher rating, a star when either had one) and each of `from`'s
    /// playlist places becomes `into`'s, in the same place. Plays stay with `from`: Navidrome
    /// adds plays only one at a time, as listens it also passes on to Last.fm or ListenBrainz,
    /// and never takes one away. Remembered so putting `from` back takes it all off again.
    pub async fn carry(&self, from: &str, title: &str, into: &str, from_marks: &[ReaderMarks]) {
        let (mut carried, failed) = self.carry_marks(from_marks, into).await;
        let mut notes = Vec::new();
        if failed > 0 {
            notes.push(format!(
                "Some stars or ratings of {title} could not be moved to the copy that stays."
            ));
        }
        if self.listed(from).await {
            let rewrite = self.rewrite(from, into, None).await;
            notes.extend(rewrite_notes(title, &rewrite));
            carried.places = rewrite.moved;
        }
        self.notes.lock().extend(notes);
        if !carried.is_empty() {
            info!(
                "Carried {} rating(s), {} star(s) and {} playlist place(s) from {from} onto {into}",
                carried.ratings.len(),
                carried.stars.len(),
                carried.places.len()
            );
            self.carries.lock().insert(from.to_string(), carried);
        }
    }

    /// Takes off the kept copy what a carry put on it: each rating raised goes back to what it
    /// was and each star added comes off, only while the kept copy still says what the carry
    /// set, so a mark someone gave since stays. Answers how many could not be taken off.
    async fn take_off(&self, carried: &Carried) -> usize {
        if carried.ratings.is_empty() && carried.stars.is_empty() {
            return 0;
        }
        let now = self.marks(&carried.into).await;
        let current = |reader: &Reader| {
            now.iter()
                .find(|read| &read.reader == reader)
                .and_then(|read| read.marks)
        };
        let mut failed = 0;
        for (reader, before, set) in &carried.ratings {
            match current(reader) {
                Some(marks) if marks.rating == *set => {
                    if !self.set_rating(reader, &carried.into, *before).await {
                        failed += 1;
                    }
                }
                Some(_) => {}
                None => failed += 1,
            }
        }
        for reader in &carried.stars {
            match current(reader) {
                Some(marks) if marks.starred => {
                    if !self.set_starred(reader, &carried.into, false).await {
                        failed += 1;
                    }
                }
                Some(_) => {}
                None => failed += 1,
            }
        }
        failed
    }

    /// After `from` was put back: what its removal carried onto the copy that stayed comes off
    /// again. Its stars, ratings and plays are its own again as soon as it is back, since
    /// Navidrome keeps them with its path; its playlist places go back at the end of the run,
    /// once Navidrome shows it again ([`Self::finish_run`]).
    pub async fn uncarry(&self, from: &str, title: &str) {
        let Some(carried) = self.carries.lock().remove(from) else {
            return;
        };
        if self.take_off(&carried).await > 0 {
            self.notes.lock().push(format!(
                "Some stars or ratings moved over from {title} could not be taken off the copy that stayed."
            ));
        }
        if !carried.places.is_empty() {
            self.uncarrying.lock().push((from.to_string(), carried));
        }
    }

    // ---- the move -------------------------------------------------------------------------

    /// A share of the music folder's gate for one move: waits while a job holds it.
    async fn write_share(&self) -> Option<LibraryWrite> {
        match &self.write_gate {
            Some(gate) => Some(gate.write().await),
            None => None,
        }
    }

    fn entry(
        key: &str,
        action: LibraryAction,
        file: &ResolvedSongFile,
        user: &str,
        copy: bool,
    ) -> LibraryActionEntry {
        LibraryActionEntry {
            key: key.to_string(),
            action,
            navidrome_id: file.navidrome_id.clone(),
            username: user.to_string(),
            title: file.title.clone(),
            artist: file.artist.clone(),
            album: file.album.clone(),
            source_path: Some(file.absolute_path.clone()),
            quarantine_path: None,
            resolution: Some(file.source),
            state: LibraryActionState::Pending,
            detail: None,
            dry_run: false,
            at_utc: Utc::now(),
            history_kept: None,
            revealed_path: None,
            copy,
            download_allowed: false,
            denied_source: None,
        }
    }

    /// Moves `audio_id`'s audio onto `target_id`'s path, under `target_id`'s identity tags and
    /// `tags` besides, and moves `audio_id`'s own file to the trash as a removed copy. The marked
    /// copy waits in the trash until Navidrome shows the moved song as the same one
    /// ([`Self::finish_run`]).
    pub async fn move_sound(
        &self,
        target_id: &str,
        audio_id: &str,
        user: &str,
        tags: &IndexMap<String, String>,
    ) -> FixAnswer {
        let settings = self.settings.current().library_actions.clone();
        if !settings.enabled {
            return FixAnswer::new("skipped", Some("Library actions are off."));
        }
        let removing = settings
            .effective_actions()
            .into_iter()
            .any(|entry| entry.action == LibraryAction::Delete && entry.enabled);
        if !removing {
            return FixAnswer::new(
                "skipped",
                Some("Removing songs is off: turn on the Delete action under Library actions."),
            );
        }
        if target_id == audio_id {
            return FixAnswer::new("skipped", Some("The best sound is already in that copy."));
        }
        let audio_marks = self.marks(audio_id).await;
        if flag(tags.get(UNRATED_ONLY).map(String::as_str))
            && let Some(why) = kept_because(&audio_marks)
        {
            return FixAnswer::new("skipped", Some(why));
        }
        let (target, audio) = (
            self.resolver.resolve(target_id).await,
            self.resolver.resolve(audio_id).await,
        );
        let (Some(target), Some(audio)) = (target, audio) else {
            return FixAnswer::new(
                "unresolved",
                Some("Could not work out which files these are, so nothing was touched."),
            );
        };
        if !same_extension(&target.absolute_path, &audio.absolute_path) {
            return FixAnswer::new("failed", Some(FORMAT_DIFFERS));
        }
        let Some(_claim) = self
            .executor
            .claim_songs(&[&target.absolute_path, &audio.absolute_path])
        else {
            return FixAnswer::new("skipped", Some(LibraryActionExecutor::BUSY_TEXT));
        };
        if settings.dry_run {
            return FixAnswer::new(
                "rehearsed",
                Some(&format!(
                    "Dry run: would move the sound of {} into {}",
                    audio.absolute_path, target.absolute_path
                )),
            );
        }
        let changes: IndexMap<String, Option<String>> = tags
            .iter()
            .filter_map(|(key, value)| {
                song_tag_fields::canonical(key).map(|tag| (tag.to_string(), Some(value.clone())))
            })
            .collect();

        let _share = self.write_share().await;
        let staged = {
            let (audio_path, target_path) = (
                PathBuf::from(&audio.absolute_path),
                PathBuf::from(&target.absolute_path),
            );
            let album_artist = target.album_artist.clone();
            tokio::task::spawn_blocking(move || {
                stage(&audio_path, &target_path, album_artist.as_deref(), &changes)
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        };
        let staged = match staged {
            Ok(staged) => staged,
            Err(why) => {
                return FixAnswer::new("failed", Some(&format!("Nothing changed: {why}.")));
            }
        };

        let root = self.resolver.music_root();
        let stamp = format!("keep-marks:{}", dotnet_ticks(Utc::now()));
        let target_key = LibraryActionJournal::make_key(LibraryAction::BetterQuality, target_id, &stamp);
        self.journal.record(Self::entry(
            &target_key,
            LibraryAction::BetterQuality,
            &target,
            user,
            false,
        ));
        let failed = |why: &str| {
            let _ = std::fs::remove_file(&staged);
            self.journal.complete(
                &target_key,
                LibraryActionState::Failed,
                Some(why),
                None,
                None,
                None,
            );
            self.journal.flush();
            FixAnswer::new("failed", Some(why))
        };
        if !self.journal.flush() {
            return failed("Could not write the action journal, so the files were not touched.");
        }
        let out = self
            .quarantine
            .move_file(&target, &root, LibraryAction::BetterQuality, user);
        let Some(target_trash) = out.quarantine_path.filter(|_| out.moved) else {
            return failed(&format!(
                "Nothing changed: the marked copy could not be set aside ({}).",
                out.error.unwrap_or_default()
            ));
        };
        self.journal.complete(
            &target_key,
            LibraryActionState::Pending,
            Some("Moved to quarantine; finishing."),
            Some(&target_trash),
            None,
            None,
        );
        self.journal.flush();
        if let Err(e) = std::fs::rename(&staged, &target.absolute_path) {
            self.quarantine.restore(&target_trash, Some(&root));
            return failed(&format!(
                "Nothing changed: the best sound could not be moved in ({e})."
            ));
        }
        self.journal.complete(
            &target_key,
            LibraryActionState::Pending,
            Some("Replacement placed; finishing."),
            Some(&target_trash),
            None,
            Some(&target.absolute_path),
        );
        self.journal.flush();

        // The best-sounding copy's own file goes to the trash as a removed copy, with its lyrics.
        let audio_key = LibraryActionJournal::make_key(LibraryAction::Delete, audio_id, &stamp);
        self.journal
            .record(Self::entry(&audio_key, LibraryAction::Delete, &audio, user, true));
        self.journal.flush();
        let gone = self
            .quarantine
            .move_file_with_sidecars(&audio, &root, LibraryAction::Delete, user, true);
        let Some(audio_trash) = gone.quarantine_path.filter(|_| gone.moved) else {
            let why = format!(
                "Nothing changed: the best-sounding copy could not be moved to the trash ({}).",
                gone.error.unwrap_or_default()
            );
            self.journal.complete(
                &audio_key,
                LibraryActionState::Failed,
                Some(&why),
                None,
                None,
                None,
            );
            // The moved-in file is a copy made here a moment ago; the best-sounding copy is
            // still in place, so the copy goes and the marked copy comes back.
            let _ = std::fs::remove_file(&target.absolute_path);
            self.quarantine.restore(&target_trash, Some(&root));
            self.journal.complete(
                &target_key,
                LibraryActionState::Failed,
                Some(&why),
                Some(&target_trash),
                None,
                None,
            );
            self.journal.flush();
            return FixAnswer::new("failed", Some(&why));
        };
        self.journal.complete(
            &audio_key,
            LibraryActionState::Applied,
            Some("Removed this copy. Its sound moved into the copy people marked."),
            Some(&audio_trash),
            None,
            None,
        );
        self.journal.flush();
        match self.library.forget_mapping(&audio.absolute_path).await {
            Ok(forgotten) => self.quarantine.record_mappings(&audio_trash, &forgotten),
            Err(e) => warn!(
                "The outside songs of {} could not be forgotten: {e}",
                audio.absolute_path
            ),
        }

        let metadata = std::fs::metadata(&target.absolute_path).ok();
        info!(
            "{user} moved the sound of {} into {} to keep its marks",
            audio.absolute_path, target.absolute_path
        );
        let words = format!(
            "Moved the sound of {} into {}.",
            file_name(&audio.absolute_path),
            file_name(&target.absolute_path)
        );
        self.moves.lock().insert(
            target_id.to_string(),
            Moved {
                target,
                audio,
                user: user.to_string(),
                target_key,
                target_trash,
                audio_key,
                audio_trash,
                size: metadata.as_ref().map_or(0, std::fs::Metadata::len),
                modified: metadata.and_then(|m| m.modified().ok()),
                rewritten: Vec::new(),
                audio_marks,
                carried: Carried::default(),
            },
        );
        FixAnswer::new("applied", Some(&words))
    }

    /// Puts both copies of one move back as they were: the marked copy's own file at its path,
    /// the best-sounding copy's out of the trash. Only while the moved-in file has not changed
    /// since. Playlist places moved over go back at the end of the run ([`Self::finish_run`]).
    pub async fn move_back(&self, target_id: &str, user: &str) -> FixAnswer {
        let Some(moved) = self.moves.lock().get(target_id).cloned() else {
            return FixAnswer::new("skipped", Some("There is no move of this song to put back."));
        };
        if self.settings.current().library_actions.dry_run {
            return FixAnswer::new(
                "rehearsed",
                Some(&format!(
                    "Dry run: would put back {} and {}",
                    moved.target.absolute_path, moved.audio.absolute_path
                )),
            );
        }
        match self.put_back(&moved, &format!("Put back by {user}.")).await {
            Ok(()) => {
                self.moves.lock().remove(target_id);
                if self.take_off(&moved.carried).await > 0 {
                    self.notes.lock().push(format!(
                        "Some stars or ratings moved over from the other copy of {} could not be taken off it.",
                        moved.target.title
                    ));
                }
                if !moved.rewritten.is_empty() {
                    self.returning.lock().insert(target_id.to_string(), moved);
                }
                FixAnswer::new("applied", Some("Put both copies back as they were."))
            }
            Err(why) => FixAnswer::new("failed", Some(&why)),
        }
    }

    /// The marked copy back at its path and the best-sounding copy out of the trash. Only while
    /// the file moved in is exactly as the move left it: anything newer at the path stays.
    async fn put_back(&self, moved: &Moved, detail: &str) -> Result<(), String> {
        let Some(_claim) = self
            .executor
            .claim_songs(&[&moved.target.absolute_path, &moved.audio.absolute_path])
        else {
            return Err(LibraryActionExecutor::BUSY_TEXT.to_string());
        };
        let _share = self.write_share().await;
        let now = std::fs::metadata(&moved.target.absolute_path).ok();
        let unchanged = now
            .as_ref()
            .is_some_and(|m| m.len() == moved.size && m.modified().ok() == moved.modified);
        if !unchanged {
            return Err(CHANGED_SINCE.to_string());
        }
        let root = self.resolver.music_root();
        let path = Path::new(&moved.target.absolute_path);
        let aside = path.with_file_name(format!(
            "{STAGING_PREFIX}back-{}.{}",
            uuid::Uuid::new_v4().simple(),
            path.extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        std::fs::rename(path, &aside).map_err(|e| format!("Could not put it back: {e}."))?;
        let restored = self.quarantine.restore(&moved.target_trash, Some(&root));
        if !restored.moved {
            let _ = std::fs::rename(&aside, path);
            return Err(format!(
                "Could not put back the marked copy: {}.",
                restored.error.unwrap_or_default()
            ));
        }
        // A copy made here; the best-sounding copy's own file comes out of the trash next.
        let _ = std::fs::remove_file(&aside);
        self.journal.complete(
            &moved.target_key,
            LibraryActionState::Restored,
            Some(detail),
            None,
            None,
            None,
        );
        let audio = self.quarantine.restore(&moved.audio_trash, Some(&root));
        if !audio.moved {
            self.journal.flush();
            return Err(format!(
                "Put back the marked copy, but the best-sounding copy is still in the trash: {}.",
                audio.error.unwrap_or_default()
            ));
        }
        if let Some(mappings) = audio.mappings.as_ref().filter(|m| !m.is_empty())
            && let Err(e) = self
                .library
                .restore_mappings(mappings, &moved.audio.absolute_path)
                .await
        {
            warn!(
                "The outside songs of {} could not be linked to it again: {e}",
                moved.audio.absolute_path
            );
        }
        self.journal.complete(
            &moved.audio_key,
            LibraryActionState::Restored,
            Some(detail),
            None,
            None,
            None,
        );
        self.journal.flush();
        info!(
            "Put back {} and {} as they were",
            moved.target.absolute_path, moved.audio.absolute_path
        );
        Ok(())
    }

    /// Removes the hidden copies a move makes beside a song (`.octo-keep-…`), which a stop part
    /// way leaves behind. Run at startup, when no move is going. Answers how many went.
    pub async fn sweep_staging(&self) -> usize {
        let root = PathBuf::from(self.resolver.music_root());
        tokio::task::spawn_blocking(move || sweep_staging_under(&root))
            .await
            .unwrap_or(0)
    }

    // ---- the end of a run -----------------------------------------------------------------

    /// After a run: every song moved into is checked until Navidrome shows it as the same song,
    /// and its playlist places are moved over then; a move Navidrome does not keep is put back.
    /// Every song put back gets its playlist places back once Navidrome shows it again. Answers
    /// the moves that were put back, each with why, and notes for the run's summary.
    pub async fn finish_run(
        &self,
        moved: &[String],
        back: &[String],
    ) -> (Vec<(String, String)>, Vec<String>) {
        let records: Vec<Moved> = {
            let moves = self.moves.lock();
            moved.iter().filter_map(|id| moves.get(id).cloned()).collect()
        };
        let checked = join_all(records.into_iter().map(|record| async move {
            let request = LibraryActionRequest::new(
                LibraryAction::BetterQuality,
                record.target.navidrome_id.clone(),
                record.user.clone(),
            );
            let kept = self
                .executor
                .confirm_history_kept(
                    &request,
                    &record.target,
                    &record.target.absolute_path,
                    &record.target_key,
                    false,
                )
                .await;
            (record, kept)
        }))
        .await;
        let mut failed = Vec::new();
        let mut notes = Vec::new();
        for (record, kept) in checked {
            let id = record.target.navidrome_id.clone();
            if !kept {
                let why =
                    "Navidrome did not show the moved song as the same song, so both copies were put back.";
                match self.put_back(&record, why).await {
                    Ok(()) => {
                        self.journal.complete(
                            &record.target_key,
                            LibraryActionState::Failed,
                            Some(why),
                            None,
                            Some(false),
                            None,
                        );
                        self.journal.flush();
                        failed.push((id.clone(), why.to_string()));
                    }
                    Err(problem) if problem == CHANGED_SINCE => failed.push((
                        id.clone(),
                        "Navidrome did not show the moved song as the same song, but the file has changed since, so both copies were left as they are."
                            .to_string(),
                    )),
                    Err(problem) => failed.push((id.clone(), format!("{why} {problem}"))),
                }
                self.moves.lock().remove(&id);
                continue;
            }
            let rewrite = self.rewrite(&record.audio.navidrome_id, &id, None).await;
            notes.extend(rewrite_notes(&record.target.title, &rewrite));
            let (carried, unmoved) = self.carry_marks(&record.audio_marks, &id).await;
            if unmoved > 0 {
                notes.push(format!(
                    "Some stars or ratings of the other copy of {} could not be moved to the copy that stays.",
                    record.target.title
                ));
            }
            if let Some(stored) = self.moves.lock().get_mut(&id) {
                stored.rewritten = rewrite.moved;
                stored.carried = carried;
            }
        }
        let returning: Vec<Moved> = {
            let mut returning = self.returning.lock();
            back.iter().filter_map(|id| returning.remove(id)).collect()
        };
        for record in returning {
            let shown = self
                .executor
                .wait_until_shown(&record.audio.navidrome_id, &record.audio.absolute_path)
                .await;
            if !shown {
                notes.push(format!(
                    "Navidrome has not shown {} again yet, so its playlist places stay with the other copy.",
                    record.audio.title
                ));
                continue;
            }
            let rewrite = self
                .rewrite(
                    &record.target.navidrome_id,
                    &record.audio.navidrome_id,
                    Some(&record.rewritten),
                )
                .await;
            if !rewrite.failed.is_empty() {
                notes.push(format!(
                    "Some playlist places of {} could not be put back, in {}.",
                    record.audio.title,
                    rewrite.failed.join(", ")
                ));
            }
        }
        let uncarrying: Vec<(String, Carried)> = std::mem::take(&mut *self.uncarrying.lock());
        for (from, carried) in uncarrying {
            let back = self.resolver.resolve(&from).await;
            let shown = match &back {
                Some(file) => self.executor.wait_until_shown(&from, &file.absolute_path).await,
                None => false,
            };
            let title = back
                .as_ref()
                .map_or_else(|| from.clone(), |file| file.title.clone());
            if !shown {
                notes.push(format!(
                    "Navidrome has not shown {title} again yet, so its playlist places stay with the other copy."
                ));
                continue;
            }
            let rewrite = self.rewrite(&carried.into, &from, Some(&carried.places)).await;
            if !rewrite.failed.is_empty() {
                notes.push(format!(
                    "Some playlist places of {title} could not be put back, in {}.",
                    rewrite.failed.join(", ")
                ));
            }
        }
        notes.extend(self.take_notes());
        (failed, notes)
    }

    // ---- playlists ------------------------------------------------------------------------

    /// Moves every playlist entry of `from` onto `to`, each in its own place: or only the
    /// places in `only`. Smart and synced playlists are left as they are and named.
    pub async fn rewrite(&self, from: &str, to: &str, only: Option<&[(String, usize)]>) -> Rewrite {
        let mut done = Rewrite::default();
        let Some(playlists) = self.api.list_playlist_kinds().await else {
            if only.is_some_and(|only| !only.is_empty()) {
                done.failed.push("every playlist".to_string());
            }
            return done;
        };
        for playlist in &playlists {
            let wanted: Option<Vec<usize>> = only.map(|only| {
                only.iter()
                    .filter(|(id, _)| *id == playlist.id)
                    .map(|(_, position)| *position)
                    .collect()
            });
            if wanted.as_ref().is_some_and(Vec::is_empty) {
                continue;
            }
            // Every playlist is swapped under its lock; one that holds songs found online has its
            // entries swapped by the held service, each keeping its place and its fallback.
            let held = self.held();
            let guard = match &held {
                Some(held) => match held.lock(&playlist.id).await {
                    Ok(guard) => Some(guard),
                    Err(_) => {
                        done.failed.push(playlist.name.clone());
                        continue;
                    }
                },
                None => None,
            };
            if let (Some(held), Some(guard)) = (&held, &guard)
                && held.is_held(guard)
            {
                match held.swap_library(guard, from, to, wanted.as_deref()).await {
                    Ok(moved) => done
                        .moved
                        .extend(moved.into_iter().map(|position| (playlist.id.clone(), position))),
                    Err(_) => done.failed.push(playlist.name.clone()),
                }
                continue;
            }
            let Ok(Some(tracks)) = self.api.read_all_tracks(&playlist.id).await else {
                if wanted.is_some() {
                    done.failed.push(playlist.name.clone());
                }
                continue;
            };
            let mut positions: Vec<usize> = tracks
                .iter()
                .filter(|track| track.media_file_id == from)
                .filter_map(|track| track.position.trim().parse().ok())
                .collect();
            if let Some(wanted) = &wanted {
                positions.retain(|position| wanted.contains(position));
            }
            if positions.is_empty() {
                continue;
            }
            if !playlist.editable() {
                done.skipped.push(playlist.name.clone());
                continue;
            }
            positions.sort_unstable();
            for position in positions {
                if self.swap_entry(playlist, position, from, to).await {
                    done.moved.push((playlist.id.clone(), position));
                } else {
                    done.failed.push(playlist.name.clone());
                    break;
                }
            }
        }
        done
    }

    /// The entry of `from` at `position` becomes `to`, in the same place: `to` is added at the
    /// end, moved up to `position`, and `from`, one place down now, is removed.
    async fn swap_entry(&self, playlist: &PlaylistKind, position: usize, from: &str, to: &str) -> bool {
        let at = |tracks: &[crate::services::library::library_action_playlist_worker::PlaylistTrackRow],
                  place: usize| {
            tracks
                .iter()
                .find(|track| track.position.trim().parse::<usize>().ok() == Some(place))
                .map(|track| track.media_file_id.clone())
        };
        if !matches!(
            self.api.add_tracks(&playlist.id, &[to.to_string()]).await,
            Ok(true)
        ) {
            return false;
        }
        let Ok(Some(tracks)) = self.api.read_all_tracks(&playlist.id).await else {
            return false;
        };
        let last = tracks
            .iter()
            .filter_map(|track| track.position.trim().parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        if at(&tracks, last).as_deref() != Some(to) {
            return false;
        }
        if last != position && !matches!(self.api.move_track(&playlist.id, last, position).await, Ok(true)) {
            return false;
        }
        let Ok(Some(tracks)) = self.api.read_all_tracks(&playlist.id).await else {
            return false;
        };
        if at(&tracks, position).as_deref() != Some(to) || at(&tracks, position + 1).as_deref() != Some(from)
        {
            return false;
        }
        matches!(
            self.api
                .remove_positions(&playlist.id, &[(position + 1).to_string()])
                .await,
            Ok(true)
        )
    }
}

/// What a playlist rewrite leaves for the run's summary, in plain words.
fn rewrite_notes(title: &str, rewrite: &Rewrite) -> Vec<String> {
    let mut notes = Vec::new();
    if !rewrite.skipped.is_empty() {
        notes.push(format!(
            "{} stays as it is in {}: smart and synced playlists cannot be edited.",
            title,
            rewrite.skipped.join(", ")
        ));
    }
    if !rewrite.failed.is_empty() {
        notes.push(format!(
            "Some playlist places of {} could not be moved over, in {}.",
            title,
            rewrite.failed.join(", ")
        ));
    }
    notes
}

/// The hidden copies a move makes beside a song.
const STAGING_PREFIX: &str = ".octo-keep-";

/// Every `.octo-keep-…` file under `root`, removed; links are never followed. Answers how many.
pub fn sweep_staging_under(root: &Path) -> usize {
    let mut removed = 0;
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                folders.push(path);
            } else if kind.is_file() && entry.file_name().to_string_lossy().starts_with(STAGING_PREFIX) {
                match std::fs::remove_file(&path) {
                    Ok(()) => removed += 1,
                    Err(e) => warn!("Could not remove {}: {e}", path.display()),
                }
            }
        }
    }
    if removed > 0 {
        info!("Removed {removed} copy(ies) a stopped move left beside songs");
    }
    removed
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |name| name.to_string_lossy().into_owned())
}

/// Whether a step's tag names are all ones a fix may write, apart from `from` and
/// `unratedOnly`.
pub fn unknown_tag(tags: &IndexMap<String, String>) -> Option<&str> {
    tags.keys()
        .map(String::as_str)
        .filter(|key| *key != "from" && *key != UNRATED_ONLY)
        .find(|key| song_tag_fields::canonical(key).is_none() || is_blank(key))
}

#[cfg(test)]
#[path = "keep_marks_tests.rs"]
mod tests;
