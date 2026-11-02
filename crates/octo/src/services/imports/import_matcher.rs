//! Matches imported songs against the library, with `LibraryIndex`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use octo_core::common::Clock;
use octo_core::common::dotnet::is_blank;
use octo_core::common::romanize::{has_non_ascii_letters, romanized_key};
use octo_core::common::song_identity::{SongIdentity, SongRef};
use octo_core::common::version_gap::{self, Against};
use octo_core::common::{live_version, version_variant};
use parking_lot::Mutex;
use tracing::{debug, warn};

use super::ImportTrack;
use super::import_keys::ImportKeys;
use super::text_list::{Orientation, TextLine, choose_orientation};
use crate::services::family::library_index::row_full_path;
use crate::services::family::{LibraryScope, ScopeResolver};
use crate::services::fingerprint::IsrcLookup;
use crate::services::library::NavidromePlaylistApi;
use crate::services::library::duplicate_scan_worker::is_lossless_file;
use crate::services::library::library_ownership::LibraryOwnership;
use crate::services::library::quality_upgrade_worker::LibrarySongRow;

/// One page of the library; None when Navidrome could not be read. Tests answer from a list.
pub type ReadPage =
    Arc<dyn Fn(i32, i32) -> BoxFuture<'static, Option<(Vec<LibrarySongRow>, usize)>> + Send + Sync>;

/// One recording an ISRC names in the music database: its id, its title and its length.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct IsrcRecording {
    pub id: String,
    pub title: String,
    pub seconds: Option<i32>,
}

/// The recordings the music database lists for an ISRC; None when it could not be asked.
pub type IsrcRecordings = Arc<dyn Fn(String) -> BoxFuture<'static, Option<Vec<IsrcRecording>>> + Send + Sync>;

/// What one round of asking the music database about ISRCs settled.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IsrcLearning {
    /// ISRCs answered for good, with or without recordings.
    pub settled: usize,
    /// Of those, the ISRCs that named a recording a library file is tagged with: the lists may
    /// match differently now.
    pub in_library: usize,
    /// Whether an ask could not get an answer, so the round stopped there.
    pub failed: bool,
}

/// What the last look through a song's ISRC found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsrcOwnership {
    /// The library's copy.
    Owned(String),
    /// No copy, and every ISRC of the song has its answer.
    Missing,
    /// No copy yet, but the music database could not be asked about an ISRC of the song.
    Unanswered,
    /// The library could not be read, so nothing could be looked for yet.
    LibraryUnread,
}

/// What the music database said about each ISRC asked about, as kept on disk.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct IsrcFile {
    recordings: HashMap<String, Vec<IsrcRecording>>,
}

/// What the music database was asked about at one moment, kept apart so a forget drops only
/// what it saw.
#[derive(Debug, Default, Clone)]
pub struct KnownIsrcs {
    pub answered: HashSet<String>,
    pub failed: HashSet<String>,
}

/// Whether the library already has each imported song, the way Octo decides it everywhere else:
/// one ISRC, or one MatchKey (the primary artist and the title, one version, so a live take or a
/// remix is its own song) and a length within LibraryOwnership's 8 seconds when both lengths are
/// known. A song those miss gets a second look through SongIdentity.Same among the songs with
/// its title, its title in Latin letters, or a name of its credit, which forgives a credit
/// written another way ("A, B" against "B feat. A", "Fonsi, Luis", an accent, a renamed artist)
/// and a title written in another script ("Камин" and "Kamin").
///
/// The whole library is read once, through Navidrome's own song list, and kept a few minutes, so
/// a list of thousands is matched in one pass rather than a search per song. With Family on, a
/// person's list is matched only against the songs they can see: the shared library and their
/// own, never another member's.
pub struct ImportMatcher {
    gate: tokio::sync::Mutex<()>,
    index: Mutex<Option<(Arc<LibraryIndex>, DateTime<Utc>)>>,
    problem: Mutex<Option<String>>,
    read_page: Mutex<ReadPage>,
    clock: Mutex<Clock>,
    scope: Option<ScopeResolver>,
    /// The music database's recordings for each ISRC asked about; the answers that name recordings are kept in the answers file across restarts.
    isrc_recordings: Arc<Mutex<HashMap<String, Vec<IsrcRecording>>>>,
    ask_isrc: Mutex<Option<IsrcRecordings>>,
    /// The ISRCs whose last ask came back with nothing, each with the turn it failed on: asked after the others, the oldest failure first.
    isrc_failed: Mutex<HashMap<String, u64>>,
    /// Counts the failed asks, so the oldest failure is asked first.
    failures: std::sync::atomic::AtomicU64,
    /// Whether the music database's latest answer was none: the trickle holds the songs it has
    /// to check until it answers again.
    music_database_out: std::sync::atomic::AtomicBool,
    /// Where what was learned is kept across restarts, if anywhere.
    isrc_file: Option<std::path::PathBuf>,
    /// One writer of the answers file at a time.
    saving: Mutex<()>,
}

impl ImportMatcher {
    pub const KEEP_INDEX_FOR: TimeDelta = TimeDelta::minutes(10);
    const PAGE_SIZE: i32 = 1000;

    pub const CANNOT_READ_LIBRARY: &str = "Octo could not read your Navidrome library, so it cannot tell which songs you have. Octo needs a Navidrome admin sign-in, set under Music server.";

    /// Over Navidrome's song list.
    pub fn new(navidrome: Arc<NavidromePlaylistApi>) -> Self {
        Self::with_pages(Arc::new(move |start, count| {
            let navidrome = navidrome.clone();
            Box::pin(async move { navidrome.list_songs(start, count).await })
        }))
    }

    /// Over any source of pages.
    pub fn with_pages(read_page: ReadPage) -> Self {
        ImportMatcher {
            gate: tokio::sync::Mutex::new(()),
            index: Mutex::new(None),
            problem: Mutex::new(None),
            read_page: Mutex::new(read_page),
            clock: Mutex::new(Clock::system()),
            scope: None,
            isrc_recordings: Arc::new(Mutex::new(HashMap::new())),
            ask_isrc: Mutex::new(None),
            isrc_failed: Mutex::new(HashMap::new()),
            failures: std::sync::atomic::AtomicU64::new(0),
            music_database_out: std::sync::atomic::AtomicBool::new(false),
            isrc_file: None,
            saving: Mutex::new(()),
        }
    }

    /// Asks the music database which recordings an imported song's ISRC names, so a library
    /// file tagged with one of those recordings is found when it carries no ISRC itself.
    pub fn with_isrc_recordings(self, ask: IsrcRecordings) -> Self {
        *self.ask_isrc.lock() = Some(ask);
        self
    }

    /// Keeps what the music database said about each ISRC in `path`, and reads it back now, so
    /// a restart neither asks again nor forgets which copy a recording found.
    pub fn with_isrc_file(mut self, path: Option<std::path::PathBuf>) -> Self {
        if let Some(path) = &path {
            let kept: IsrcFile = super::load_or_default(path, "the ISRC answers");
            *self.isrc_recordings.lock() = kept.recordings;
        }
        self.isrc_file = path;
        self
    }

    /// Writes the answers that name recordings to the answers file, one writer at a time, so
    /// the latest map is the one on disk. An answer that names none stays in memory only: the
    /// music database often learns a new code later, and a restart asks it again.
    fn keep_isrc_answers(&self) {
        let Some(path) = &self.isrc_file else {
            return;
        };
        let _saving = self.saving.lock();
        let file = IsrcFile {
            recordings: self
                .isrc_recordings
                .lock()
                .iter()
                .filter(|(_, recordings)| !recordings.is_empty())
                .map(|(isrc, recordings)| (isrc.clone(), recordings.clone()))
                .collect(),
        };
        let _ = super::save(path, &file, "the ISRC answers");
    }

    /// Forgets every answer and failure for ISRCs not in `wanted` (the ISRCs some list still
    /// holds), and writes the file when anything went.
    pub fn keep_only(&self, wanted: &HashSet<String>) {
        self.forget_unheld(&self.known_isrcs(), wanted);
    }

    /// What the music database was asked about, at one moment: the ISRCs it answered and the
    /// ones whose last ask failed.
    pub fn known_isrcs(&self) -> KnownIsrcs {
        KnownIsrcs {
            answered: self.isrc_recordings.lock().keys().cloned().collect(),
            failed: self.isrc_failed.lock().keys().cloned().collect(),
        }
    }

    /// Forgets the answers and failures `known` saw that `held` does not hold. An answer that
    /// came in since `known` was taken, even for an ISRC that had failed then, stays. Writes
    /// the file when an answer went.
    pub fn forget_unheld(&self, known: &KnownIsrcs, held: &HashSet<String>) {
        let mut dropped = false;
        self.isrc_recordings.lock().retain(|isrc, _| {
            let keep = held.contains(isrc) || !known.answered.contains(isrc);
            dropped |= !keep;
            keep
        });
        self.isrc_failed
            .lock()
            .retain(|isrc, _| held.contains(isrc) || !known.failed.contains(isrc));
        if dropped {
            self.keep_isrc_answers();
        }
    }

    /// Puts an answer in as if the music database had given it. Only tests need it.
    #[cfg(test)]
    pub fn answer_for_tests(&self, isrc: &str, recordings: Vec<IsrcRecording>) {
        self.isrc_failed.lock().remove(isrc);
        self.isrc_recordings.lock().insert(isrc.to_string(), recordings);
    }

    /// How many ISRCs one round of learning asks the music database about, one a second: a
    /// long list is asked about over several rounds.
    pub const ISRC_ASKS_PER_MATCH: usize = 20;

    /// The ISRCs of the unmatched tracks the music database has not answered yet, at most
    /// `most`, the ones whose last ask came back with nothing after the others. An ISRC a
    /// library file carries needs no asking, except with the libraries kept apart, where the file may
    /// be another member's.
    fn pending_isrcs(
        &self,
        index: Option<&LibraryIndex>,
        tracks: &[ImportTrack],
        most: usize,
    ) -> Vec<String> {
        if most == 0 || self.ask_isrc.lock().is_none() {
            return Vec::new();
        }
        // With the libraries kept apart, the file that carries an ISRC may be another member's.
        let carried_counts = !self
            .scope
            .as_ref()
            .map(ScopeResolver::view)
            .is_some_and(|view| view.apart());
        let mut seen: HashSet<String> = HashSet::new();
        let mut fresh: Vec<String> = Vec::new();
        let mut again: Vec<(u64, String)> = Vec::new();
        for track in tracks.iter().filter(|track| track.library_id.is_none()) {
            for isrc in SongIdentity::isrcs(track.isrc.iter()) {
                if !seen.insert(isrc.clone()) || self.isrc_recordings.lock().contains_key(&isrc) {
                    continue;
                }
                if carried_counts && index.is_some_and(|index| index.by_isrc.contains_key(&isrc)) {
                    continue;
                }
                if let Some(turn) = self.isrc_failed.lock().get(&isrc).copied() {
                    again.push((turn, isrc));
                } else {
                    fresh.push(isrc);
                    if fresh.len() >= most {
                        return fresh;
                    }
                }
            }
        }
        again.sort_by_key(|(turn, _)| *turn);
        fresh.extend(again.into_iter().map(|(_, isrc)| isrc));
        fresh.truncate(most);
        fresh
    }

    /// Asks the music database about up to [`Self::ISRC_ASKS_PER_MATCH`] pending ISRCs of these
    /// tracks, stopping at the first that could not be asked, and keeps the answers.
    pub async fn learn_isrcs(&self, tracks: &[ImportTrack]) -> IsrcLearning {
        let Some(ask) = self.ask_isrc.lock().clone() else {
            return IsrcLearning::default();
        };
        // While the library could not be read, ask with what is kept and leave the library
        // alone; it is read again only once an answer could match one of its files.
        let unread = self.problem.lock().is_some();
        let index = if unread {
            self.index.lock().as_ref().map(|(index, _)| Arc::clone(index))
        } else {
            self.library_index().await
        };
        let wanted = self.pending_isrcs(index.as_deref(), tracks, Self::ISRC_ASKS_PER_MATCH);
        let mut learning = IsrcLearning::default();
        let mut learned: Vec<Vec<IsrcRecording>> = Vec::new();
        for isrc in wanted {
            match ask(isrc.clone()).await {
                Some(recordings) => {
                    self.music_database_out
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    self.isrc_failed.lock().remove(&isrc);
                    learning.settled += 1;
                    learned.push(recordings.clone());
                    self.isrc_recordings.lock().insert(isrc, recordings);
                }
                None => {
                    self.music_database_out
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    let turn = self.failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.isrc_failed.lock().insert(isrc, turn);
                    learning.failed = true;
                    break;
                }
            }
        }
        let index = if unread && !learned.is_empty() {
            self.library_index().await
        } else {
            index
        };
        if let Some(index) = &index {
            learning.in_library = learned
                .iter()
                .filter(|recordings| {
                    recordings.iter().any(|recording| {
                        index
                            .by_recording
                            .contains_key(&recording.id.trim().to_ascii_lowercase())
                    })
                })
                .count();
        }
        if learning.settled > 0 {
            self.keep_isrc_answers();
        }
        learning
    }

    /// Whether any unmatched song of these still has an ISRC the music database has not
    /// answered.
    pub fn has_pending_isrcs(&self, tracks: &[ImportTrack]) -> bool {
        let index = self.index.lock().as_ref().map(|(index, _)| Arc::clone(index));
        !self.pending_isrcs(index.as_deref(), tracks, 1).is_empty()
    }

    /// Whether the music database is out: its latest question went unanswered.
    pub fn music_database_out(&self) -> bool {
        self.music_database_out.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Which of these tracks wait for the music database while it is out, by their place in
    /// `tracks`: an ISRC of theirs has no answer yet. Empty while it answers, so nothing waits.
    /// Reads only the library index already kept, never the library itself: without one, a
    /// song the library carries an ISRC for waits too, and its last look still finds it.
    pub fn waiting_on_music_database(&self, tracks: &[ImportTrack]) -> HashSet<usize> {
        if !self.music_database_out() {
            return HashSet::new();
        }
        let index = self.index.lock().as_ref().map(|(index, _)| Arc::clone(index));
        let pending: HashSet<String> = self
            .pending_isrcs(index.as_deref(), tracks, usize::MAX)
            .into_iter()
            .collect();
        if pending.is_empty() {
            return HashSet::new();
        }
        tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| {
                track.library_id.is_none()
                    && SongIdentity::isrcs(track.isrc.iter())
                        .iter()
                        .any(|isrc| pending.contains(isrc))
            })
            .map(|(at, _)| at)
            .collect()
    }

    /// The library's copy of a song, through every way a match has, the recordings its ISRC
    /// names included: asked about now when the music database was not asked yet. The last look
    /// before a song is fetched, so a copy only its ISRC can find is never fetched again; when
    /// the music database could not be asked, it says so instead of guessing.
    pub async fn owned_by_isrc(&self, owner: &str, track: &ImportTrack) -> IsrcOwnership {
        let isrcs = SongIdentity::isrcs(track.isrc.iter());
        if isrcs.is_empty() {
            return IsrcOwnership::Missing;
        }
        self.learn_isrcs(std::slice::from_ref(track)).await;
        let Some(index) = self.library_index().await else {
            return IsrcOwnership::LibraryUnread;
        };
        let recordings = self.known_recordings(track);
        let found = self.within(owner, |visible| {
            index
                .find_known(track, &recordings, visible)
                .map(|row| row.id.clone())
        });
        if let Some(id) = found {
            return IsrcOwnership::Owned(id);
        }
        // A code the library already answers, or one with no music database to ask, is as good
        // as answered; only a code still waiting for its answer holds the song.
        if self
            .pending_isrcs(Some(&index), std::slice::from_ref(track), 1)
            .is_empty()
        {
            IsrcOwnership::Missing
        } else {
            IsrcOwnership::Unanswered
        }
    }

    #[cfg(test)]
    pub fn knows_isrc_for_tests(&self, isrc: &str) -> bool {
        self.isrc_recordings.lock().contains_key(isrc)
    }

    /// What the music database said about the track's ISRCs, as far as it was asked.
    fn known_recordings(&self, track: &ImportTrack) -> Vec<IsrcRecording> {
        let known = self.isrc_recordings.lock();
        SongIdentity::isrcs(track.isrc.iter())
            .into_iter()
            .filter_map(|isrc| known.get(&isrc).cloned())
            .flatten()
            .collect()
    }

    /// Matches each person's lists against their own scope of the library.
    pub fn with_library_scope(mut self, scope: ScopeResolver) -> Self {
        self.scope = Some(scope);
        self
    }

    pub fn set_read_page(&self, read_page: ReadPage) {
        *self.read_page.lock() = read_page;
    }

    pub fn set_clock(&self, clock: Clock) {
        *self.clock.lock() = clock;
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.lock().now()
    }

    /// Why the last match could not read the library, in words, or None when it could.
    pub fn problem(&self) -> Option<String> {
        self.problem.lock().clone()
    }

    /// Read the library again on the next match: something was just added to it.
    pub fn forget(&self) {
        *self.index.lock() = None;
    }

    /// Sets each track's LibraryId to the library's song for it, or None. False when the library
    /// could not be read, in which case the tracks keep what an earlier match found.
    pub async fn match_tracks(&self, tracks: &mut [ImportTrack]) -> bool {
        let Some(index) = self.library_index().await else {
            return false;
        };
        for track in tracks.iter_mut() {
            let recordings = self.known_recordings(track);
            track.library_id = index
                .find_known(track, &recordings, &|_| true)
                .map(|row| row.id.clone());
        }
        true
    }

    /// [`Self::match_tracks`] for one person's lists: only songs that person can see count as
    /// theirs, so a song in another member's own library is still missing for them.
    pub async fn match_tracks_for(&self, owner: &str, tracks: &mut [ImportTrack]) -> bool {
        let Some(index) = self.library_index().await else {
            return false;
        };
        let known: Vec<Vec<IsrcRecording>> =
            tracks.iter().map(|track| self.known_recordings(track)).collect();
        self.within(owner, |visible| {
            for (track, recordings) in tracks.iter_mut().zip(&known) {
                track.library_id = index
                    .find_known(track, recordings, visible)
                    .map(|row| row.id.clone());
            }
        });
        true
    }

    /// Playlist reads reuse the library index and already learned ISRCs. They never start
    /// catalog lookups, source refreshes or downloads on a player's request, and read the library
    /// only when `read_if_stale` says a song they miss has landed: false when no index read in
    /// the last [`Self::KEEP_INDEX_FOR`] is at hand otherwise.
    pub async fn match_streaming_tracks_for(
        &self,
        owner: &str,
        tracks: &mut [ImportTrack],
        read_if_stale: bool,
    ) -> bool {
        let fresh = self
            .index
            .lock()
            .clone()
            .filter(|(_, at)| self.now() - *at < Self::KEEP_INDEX_FOR)
            .map(|(index, _)| index);
        let index = match fresh {
            Some(index) => index,
            None if read_if_stale => match self.library_index().await {
                Some(index) => index,
                None => return false,
            },
            None => return false,
        };
        self.within(owner, |visible| {
            for track in tracks {
                track.library_id = index
                    .find_known(track, &self.known_recordings(track), visible)
                    .map(|row| row.id.clone());
            }
        });
        true
    }

    /// Which way round a plain list's lines are written, judged by which reading names more of
    /// the songs `owner` can see. Artist - Title when the library cannot be read.
    pub async fn orientation_for(&self, owner: &str, lines: &[TextLine]) -> Orientation {
        let Some(index) = self.library_index().await else {
            return Orientation::ArtistTitle;
        };
        self.within(owner, |visible| {
            choose_orientation(lines, |artist, title| {
                let track = ImportTrack {
                    key: ImportKeys::for_words(Some(artist), Some(title)),
                    artist: artist.to_string(),
                    title: title.to_string(),
                    ..Default::default()
                };
                index.knows_where(&track, visible)
            })
        })
    }

    /// Runs `work` with the test for the songs `owner` can see: every song, unless Family keeps
    /// members' libraries apart.
    fn within<R>(&self, owner: &str, work: impl FnOnce(&dyn Fn(&LibrarySongRow) -> bool) -> R) -> R {
        let view = self
            .scope
            .as_ref()
            .map(ScopeResolver::view)
            .filter(|view| view.apart());
        let Some(view) = view else {
            return work(&|_| true);
        };
        let scope = LibraryScope::Viewer(owner.to_string());
        let visible = |row: &LibrarySongRow| {
            let full = row_full_path(row.library_path.as_deref(), &row.path);
            view.allows(&scope, Some(&row.id), Some(&full))
        };
        work(&visible)
    }

    async fn library_index(&self) -> Option<Arc<LibraryIndex>> {
        let _gate = self.gate.lock().await;
        if let Some((fresh, at)) = self.index.lock().clone()
            && self.now() - at < Self::KEEP_INDEX_FOR
        {
            return Some(fresh);
        }
        let mut rows = Vec::new();
        let mut start = 0;
        loop {
            let read = self.read_page.lock().clone();
            let Some((page, count)) = read(start, Self::PAGE_SIZE).await else {
                warn!("Could not read the library to match imported songs");
                *self.problem.lock() = Some(Self::CANNOT_READ_LIBRARY.to_string());
                return None;
            };
            rows.extend(page);
            if (count as i32) < Self::PAGE_SIZE {
                break;
            }
            start += Self::PAGE_SIZE;
        }
        let count = rows.len();
        let index = Arc::new(LibraryIndex::new(rows));
        *self.index.lock() = Some((index.clone(), self.now()));
        *self.problem.lock() = None;
        debug!("Read {count} library songs to match imported songs against");
        Some(index)
    }
}

/// The library by ISRC, MatchKey, title, title in Latin letters and credited name, for matching
/// many songs at once.
pub struct LibraryIndex {
    rows: Vec<LibrarySongRow>,
    by_isrc: HashMap<String, Vec<usize>>,
    /// By MusicBrainz recording id, lowercased.
    by_recording: HashMap<String, Vec<usize>>,
    by_key: HashMap<String, Vec<usize>>,
    by_title: HashMap<String, Vec<usize>>,
    /// Titles with letters outside ASCII, by their Latin spelling.
    by_cross_key: HashMap<String, Vec<usize>>,
    /// Each credited name, by its key and its Latin spelling.
    by_name: HashMap<String, Vec<usize>>,
}

impl LibraryIndex {
    /// The most songs one lookup reads from the credited-name buckets: a name on thousands of
    /// songs stops there.
    pub const NAME_SCAN_CAP: usize = 2_000;

    pub fn new(rows: impl IntoIterator<Item = LibrarySongRow>) -> Self {
        let mut index = LibraryIndex {
            rows: Vec::new(),
            by_isrc: HashMap::new(),
            by_recording: HashMap::new(),
            by_key: HashMap::new(),
            by_title: HashMap::new(),
            by_cross_key: HashMap::new(),
            by_name: HashMap::new(),
        };
        for row in rows {
            if is_blank(&row.title) {
                continue;
            }
            let at = index.rows.len();
            for isrc in SongIdentity::isrcs(&row.isrcs) {
                add(&mut index.by_isrc, isrc, at);
            }
            if let Some(recording) = row.recording_id.as_deref().filter(|id| !is_blank(id)) {
                add(&mut index.by_recording, recording.trim().to_ascii_lowercase(), at);
            }
            add(
                &mut index.by_key,
                SongIdentity::match_key(&row.artist, &row.title),
                at,
            );
            let title = SongIdentity::parse_title(&row.title, Some(&row.artist));
            if has_non_ascii_letters(&title.keyed) {
                add(&mut index.by_cross_key, romanized_key(&title.keyed), at);
            }
            add(&mut index.by_title, title.key, at);
            for key in name_keys(&row.artist) {
                add(&mut index.by_name, key, at);
            }
            index.rows.push(row);
        }
        index
    }

    pub fn count(&self) -> usize {
        self.rows.len()
    }

    /// The library's best copy of this song, or None.
    pub fn find(&self, track: &ImportTrack) -> Option<&LibrarySongRow> {
        self.find_where(track, &|_| true)
    }

    /// The best copy among the songs `visible` keeps, or None. Looked for by ISRC first (the
    /// closest length among the songs carrying it), then by MatchKey at a length within the
    /// ownership tolerance, then among the songs with the same title, the same title in Latin
    /// letters, or a credited name in common, each accepted only by [`SongIdentity::same`] with
    /// [`SongIdentity::owned`]: another script and a credit written another way count, a typo
    /// does not. Within each step the best fit by [`SongIdentity::score`] wins.
    pub fn find_where(
        &self,
        track: &ImportTrack,
        visible: &dyn Fn(&LibrarySongRow) -> bool,
    ) -> Option<&LibrarySongRow> {
        self.find_known(track, &[], visible)
    }

    /// [`Self::find_where`] with the recordings the music database lists for the song's ISRC:
    /// a file tagged with one of them is the song even when it carries no ISRC, and their
    /// length stands in for a list that gave none. Every step takes only a file of the version
    /// the song names: its title says no other version (a remix by someone else, a sped-up
    /// copy, the plain song for a remix), and for the steps by name, its album names no other
    /// version either ("Let's Get Lost" on "Let's Get Lost Remixes" is not the original).
    pub fn find_known(
        &self,
        track: &ImportTrack,
        recordings: &[IsrcRecording],
        visible: &dyn Fn(&LibrarySongRow) -> bool,
    ) -> Option<&LibrarySongRow> {
        for isrc in SongIdentity::isrcs(track.isrc.iter()) {
            if let Some(coded) = self.by_isrc.get(&isrc)
                && let Some(hit) = best(
                    self.rows_of(coded)
                        .filter(|row| visible(row))
                        .filter(|row| !version_gap::other_version(&track.title, &row.title, Against::Title)),
                    track,
                    None,
                )
            {
                return Some(hit);
            }
        }

        // A list row's own length, else the code's, when every recording the code names agrees
        // on one; recordings of several lengths (an excerpt, an edit) settle none.
        let code_lengths: Vec<i32> = recordings
            .iter()
            .filter_map(|recording| recording.seconds)
            .collect();
        let agreed = code_lengths.first().copied().filter(|first| {
            code_lengths
                .iter()
                .all(|length| (length - first).abs() <= LibraryOwnership::DURATION_TOLERANCE_SECONDS)
        });
        let seconds = track.seconds.filter(|s| *s > 0).or(agreed);
        let mut by_recording: Vec<usize> = Vec::new();
        for recording in recordings {
            if let Some(found) = self.by_recording.get(&recording.id.trim().to_ascii_lowercase()) {
                by_recording.extend(found);
            }
        }
        dedupe(&mut by_recording);
        if let Some(hit) = best(
            self.rows_of(&by_recording)
                .filter(|row| visible(row))
                .filter(|row| !version_gap::other_version(&track.title, &row.title, Against::Title))
                .filter(|row| {
                    // The list row's length, else that of the recording the file is tagged with.
                    let named = recordings
                        .iter()
                        .find(|recording| {
                            row.recording_id
                                .as_deref()
                                .is_some_and(|id| id.trim().eq_ignore_ascii_case(recording.id.trim()))
                        })
                        .and_then(|recording| recording.seconds);
                    length_fits(track.seconds.filter(|s| *s > 0).or(named), row.duration)
                }),
            track,
            None,
        ) {
            return Some(hit);
        }

        let wanted =
            SongRef::new(track.title.clone(), track.artist.clone()).with_seconds(seconds.map(f64::from));
        if let Some(same) = self
            .by_key
            .get(&SongIdentity::match_key(&track.artist, &track.title))
            && let Some(exact) = best(
                self.rows_of(same)
                    .filter(|row| visible(row))
                    .filter(|row| version_agrees(track, row))
                    .filter(|row| length_fits(seconds, row.duration)),
                track,
                Some(&wanted),
            )
        {
            return Some(exact);
        }

        let title = SongIdentity::parse_title(&track.title, Some(&track.artist));
        if title.key.is_empty() {
            return None;
        }
        let options = SongIdentity::owned(LibraryOwnership::DURATION_TOLERANCE_SECONDS);
        let accepted = |row: &&LibrarySongRow| {
            visible(row)
                && version_agrees(track, row)
                && SongIdentity::same(&wanted, &song_ref(row), Some(&options)).is_same()
        };

        if let Some(titled) = self.by_title.get(&title.key)
            && let Some(hit) = best(self.rows_of(titled).filter(accepted), track, Some(&wanted))
        {
            return Some(hit);
        }

        let mut crossed: Vec<usize> = Vec::new();
        if let Some(found) = self.by_cross_key.get(&title.key) {
            crossed.extend(found);
        }
        if has_non_ascii_letters(&title.keyed) {
            let latin = romanized_key(&title.keyed);
            for bucket in [self.by_title.get(&latin), self.by_cross_key.get(&latin)]
                .into_iter()
                .flatten()
            {
                crossed.extend(bucket);
            }
        }
        dedupe(&mut crossed);
        if let Some(hit) = best(self.rows_of(&crossed).filter(accepted), track, Some(&wanted)) {
            return Some(hit);
        }

        let mut named: Vec<usize> = Vec::new();
        for key in name_keys(&track.artist) {
            if let Some(found) = self.by_name.get(&key) {
                named.extend(found);
            }
        }
        dedupe(&mut named);
        if named.len() > Self::NAME_SCAN_CAP {
            debug!(
                "'{}' is credited on {} library songs; matching '{}' read the first {}",
                track.artist,
                named.len(),
                track.title,
                Self::NAME_SCAN_CAP
            );
            named.truncate(Self::NAME_SCAN_CAP);
        }
        best(self.rows_of(&named).filter(accepted), track, Some(&wanted))
    }

    /// Whether the library has this song by its MatchKey or by its title, among the songs
    /// `visible` keeps: a quick test for reading a whole list, without the slower lookups.
    pub fn knows_where(&self, track: &ImportTrack, visible: &dyn Fn(&LibrarySongRow) -> bool) -> bool {
        if self
            .by_key
            .get(&SongIdentity::match_key(&track.artist, &track.title))
            .is_some_and(|same| self.rows_of(same).any(visible))
        {
            return true;
        }
        let title = SongIdentity::parse_title(&track.title, Some(&track.artist)).key;
        if title.is_empty() {
            return false;
        }
        let wanted = SongRef::new(track.title.clone(), track.artist.clone());
        let options = SongIdentity::owned(LibraryOwnership::DURATION_TOLERANCE_SECONDS);
        self.by_title.get(&title).is_some_and(|titled| {
            self.rows_of(titled).any(|row| {
                visible(row) && SongIdentity::same(&wanted, &song_ref(row), Some(&options)).is_same()
            })
        })
    }

    fn rows_of<'a, 'b>(&'a self, at: &'b [usize]) -> impl Iterator<Item = &'a LibrarySongRow> + use<'a, 'b> {
        at.iter().map(move |&i| &self.rows[i])
    }
}

/// Whether a library file is the version an imported song names, by the file's own title and
/// album: no version marker or remixer differs between the two titles, and an album that names
/// a version ("Remixes", "Sped Up", "Live at ...") names one the song's title or its own album
/// asks for, or the file's title says.
pub fn version_agrees(track: &ImportTrack, row: &LibrarySongRow) -> bool {
    if version_gap::other_version(&track.title, &row.title, Against::Title) {
        return false;
    }
    let Some(album) = row.album.as_deref().filter(|album| !is_blank(album)) else {
        return true;
    };
    let artist = Some(row.artist.as_str());
    let mut named = version_variant::in_folder(album, artist);
    if live_version::mentions(Some(&version_variant::without_artist(album, artist))) {
        named.insert("live".to_string());
    }
    if named.is_empty() {
        return true;
    }
    let mut allowed = version_variant::requested(Some(&track.title), track.album.as_deref());
    allowed.extend(version_variant::requested(Some(&row.title), None));
    if track
        .album
        .as_deref()
        .is_some_and(|own| live_version::mentions(Some(own)))
    {
        allowed.insert("live".to_string());
    }
    named.iter().all(|version| allowed.contains(version))
}

fn song_ref(row: &LibrarySongRow) -> SongRef {
    SongRef::new(row.title.clone(), row.artist.clone())
        .with_seconds(row.duration.filter(|d| *d > 0).map(f64::from))
}

/// The keys a credit's names are found by: each name and guest, as a key and, when it has
/// letters outside ASCII, in Latin letters. Names that credit nobody are left out.
fn name_keys(artist: &str) -> Vec<String> {
    let credit = SongIdentity::parse_artists(artist);
    let mut keys: Vec<String> = Vec::new();
    for name in credit.names.iter().chain(&credit.featured) {
        if SongIdentity::is_placeholder_artist(name) {
            continue;
        }
        let mut push = |key: String| {
            if !key.is_empty() && !keys.contains(&key) {
                keys.push(key);
            }
        };
        push(SongIdentity::key(name));
        if has_non_ascii_letters(name) {
            push(romanized_key(name));
        }
    }
    keys
}

fn dedupe(at: &mut Vec<usize>) {
    let mut seen = HashSet::new();
    at.retain(|i| seen.insert(*i));
}

fn add(map: &mut HashMap<String, Vec<usize>>, key: String, at: usize) {
    if key.is_empty() {
        return;
    }
    let bucket = map.entry(key).or_default();
    if bucket.last() != Some(&at) {
        bucket.push(at);
    }
}

fn length_fits(wanted: Option<i32>, found: Option<i32>) -> bool {
    match (wanted.filter(|w| *w > 0), found.filter(|f| *f > 0)) {
        (Some(wanted), Some(found)) => (wanted - found).abs() <= LibraryOwnership::DURATION_TOLERANCE_SECONDS,
        _ => true,
    }
}

/// With `wanted`, the best fit by [`SongIdentity::score`] first; then the closest in length,
/// the same album, lossless, and the highest bitrate.
fn best<'a>(
    rows: impl Iterator<Item = &'a LibrarySongRow>,
    track: &ImportTrack,
    wanted: Option<&SongRef>,
) -> Option<&'a LibrarySongRow> {
    let album_key = track.album.as_deref().map(SongIdentity::key);
    let mut rows: Vec<(&LibrarySongRow, f64)> = rows
        .map(|row| {
            let fit = wanted.map_or(0.0, |wanted| SongIdentity::score(wanted, &song_ref(row)));
            (row, fit)
        })
        .collect();
    // Stable, as OrderBy and its ThenBys are.
    rows.sort_by(|(a, fit_a), (b, fit_b)| {
        let distance = |row: &LibrarySongRow| match (
            track.seconds.filter(|s| *s > 0),
            row.duration.filter(|d| *d > 0),
        ) {
            (Some(wanted), Some(found)) => (wanted - found).abs(),
            _ => 0,
        };
        let same_album = |row: &LibrarySongRow| {
            album_key
                .as_ref()
                .is_some_and(|key| SongIdentity::key(row.album.as_deref().unwrap_or("")) == *key)
        };
        fit_b
            .total_cmp(fit_a)
            .then_with(|| distance(a).cmp(&distance(b)))
            .then_with(|| same_album(b).cmp(&same_album(a)))
            .then_with(|| {
                is_lossless_file(&b.suffix, b.bit_rate).cmp(&is_lossless_file(&a.suffix, a.bit_rate))
            })
            .then_with(|| b.bit_rate.cmp(&a.bit_rate))
    });
    rows.into_iter().next().map(|(row, _)| row)
}

/// [`IsrcRecordings`] over MusicBrainz, asked on its background lane: the recordings that
/// carry the code, in the order MusicBrainz lists them. A code MusicBrainz does not know is no
/// recordings, kept; a timeout, an unreadable answer or a question MusicBrainz could not take
/// is None, so the next learning round or last look asks again.
pub fn isrc_recordings_from(
    music_brainz: Arc<crate::services::fingerprint::music_brainz_client::MusicBrainzClient>,
) -> IsrcRecordings {
    Arc::new(move |isrc: String| {
        let music_brainz = music_brainz.clone();
        Box::pin(async move {
            match music_brainz.isrc_recordings_in_background(&isrc).await.ok()? {
                IsrcLookup::Found(doc) => Some(isrc_recordings_of(&doc)),
                IsrcLookup::NotKnown => Some(Vec::new()),
                IsrcLookup::Refused => Some(Vec::new()),
                IsrcLookup::Unanswered | IsrcLookup::GaveUp => None,
            }
        })
    })
}

/// The recordings of a MusicBrainz code lookup's answer.
pub fn isrc_recordings_of(doc: &serde_json::Value) -> Vec<IsrcRecording> {
    doc.get("recordings")
        .and_then(serde_json::Value::as_array)
        .map(|recordings| {
            recordings
                .iter()
                .filter_map(|recording| {
                    let id = recording.get("id")?.as_str()?.to_string();
                    let title = recording
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let seconds = recording
                        .get("length")
                        .and_then(serde_json::Value::as_i64)
                        .filter(|ms| *ms > 0)
                        .map(|ms| ((ms + 500) / 1000) as i32);
                    Some(IsrcRecording {
                        id,
                        title: title.to_string(),
                        seconds,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "import_matcher_tests.rs"]
mod tests;
