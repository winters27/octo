//! `LyricsJob`, `LyricsWriteOutcome`, `LyricsWrite` and the writer, one shared instance that is
//! also a background queue worker.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use octo_core::common::dotnet::{eq_ignore_case, is_null_or_white_space, to_lower_invariant};
use octo_core::lyrics::song_lyrics::OCTO_MARK;
use octo_core::lyrics::{LyricsLookup, LyricsQuery, LyricsResult, LyricsTiming, SongLyrics, SongLyricsPlace};
use octo_core::settings::{LyricsSaveTo, SettingsStore};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::lyrics_service::LyricsService;
use super::lyrics_undo_journal::{LyricsUndoEntry, LyricsUndoJournal};
use super::song_lyrics;
use crate::services::common::AcquisitionTracker;
use crate::services::common::acquisition_tracker::acquisition_event_kinds;
use crate::services::library::library_write_gate::LibraryWriteGate;
use crate::services::local::ILocalLibraryService;

/// One song to find lyrics for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsJob {
    pub audio_path: PathBuf,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_seconds: Option<i32>,
    /// 1 for the first try.
    pub attempt: i32,
    /// The downloads log row ("provider:id") that hears how the look went, when a download asked
    /// for it.
    pub acquisition: Option<String>,
}

impl LyricsJob {
    pub fn new(
        audio_path: impl Into<PathBuf>,
        artist: impl Into<String>,
        title: impl Into<String>,
        album: Option<String>,
        duration_seconds: Option<i32>,
    ) -> Self {
        Self {
            audio_path: audio_path.into(),
            artist: artist.into(),
            title: title.into(),
            album,
            duration_seconds,
            attempt: 1,
            acquisition: None,
        }
    }

    /// `job with { Acquisition = row }`.
    pub fn with_acquisition(mut self, row: Option<String>) -> Self {
        self.acquisition = row;
        self
    }

    /// `job with { Attempt = attempt }`.
    pub fn with_attempt(mut self, attempt: i32) -> Self {
        self.attempt = attempt;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LyricsWriteOutcome {
    Written,
    Instrumental,
    NotFound,
    AlreadyThere,
    Gone,
    Retrying,
    GaveUp,
    Upgraded,
}

/// What a write did, and the lyrics it wrote, for the library job's review list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsWrite {
    pub outcome: LyricsWriteOutcome,
    pub result: Option<LyricsResult>,
}

impl LyricsWrite {
    pub fn new(outcome: LyricsWriteOutcome, result: Option<LyricsResult>) -> Self {
        Self { outcome, result }
    }
}

/// The lyrics in a song's own tags, and its length. A seam, so tests can hold a song's tags in
/// memory.
pub trait LyricsTagAccess: Send + Sync {
    /// The lyrics tag, or an error when the file cannot be read as audio.
    fn read_lyrics(&self, path: &Path) -> io::Result<Option<String>>;

    /// Writes the lyrics tag and saves the file. None clears them.
    fn write_lyrics(&self, path: &Path, lyrics: Option<&str>) -> anyhow::Result<()>;

    /// The audio's length, rounded to whole seconds; None when unknown or not positive.
    fn duration_seconds(&self, path: &Path) -> Option<i32>;

    /// What the library job reads of a song: its tags and length. `Ok(None)` when the file cannot
    /// be read as audio (the job skips the song); `Err` for an I/O error, such as a missing
    /// permission (a scan counts it as failed).
    fn read_song(&self, path: &Path) -> io::Result<Option<LyricsSongTags>>;
}

/// A song's tags as the library job reads them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LyricsSongTags {
    /// The first performer, None when there is none.
    pub first_performer: Option<String>,
    /// The first album artist.
    pub first_album_artist: Option<String>,
    /// The title tag.
    pub title: Option<String>,
    /// The album tag.
    pub album: Option<String>,
    /// The lyrics tag.
    pub lyrics: Option<String>,
    /// The audio's length, rounded to whole seconds; None when not positive.
    pub duration_seconds: Option<i32>,
}

/// The song's tags, through the tags port (`octo_media::tags`).
pub struct TagLibLyricsTags;

impl LyricsTagAccess for TagLibLyricsTags {
    fn read_lyrics(&self, path: &Path) -> io::Result<Option<String>> {
        song_lyrics::read_tag_lyrics(path)
    }

    fn write_lyrics(&self, path: &Path, lyrics: Option<&str>) -> anyhow::Result<()> {
        octo_media::tags::write_lyrics(path, lyrics)?;
        Ok(())
    }

    fn duration_seconds(&self, path: &Path) -> Option<i32> {
        let seconds = octo_media::tags::TagFile::open(path).ok()?.duration_seconds();
        (seconds > 0).then_some(seconds)
    }

    fn read_song(&self, path: &Path) -> io::Result<Option<LyricsSongTags>> {
        match octo_media::tags::TagFile::open(path) {
            Ok(file) => {
                let seconds = file.duration_seconds();
                Ok(Some(LyricsSongTags {
                    first_performer: file.first_performer(),
                    first_album_artist: file.first_album_artist(),
                    title: file.title(),
                    album: file.album(),
                    lyrics: file.lyrics(),
                    duration_seconds: (seconds > 0).then_some(seconds),
                }))
            }
            // An I/O failure is counted as a failed song; any other tag error (not audio, a
            // format the tags library cannot read) is skipped.
            Err(octo_media::tags::TagError::Io(e)) => Err(e),
            Err(_) => Ok(None),
        }
    }
}

/// Where the library tidy moved songs, for the lyrics jobs that were already waiting when it
/// moved them. A job keeps its age, the remap's clock when its path was last known true; when the
/// writer takes the gate's share for it, and on each try again, the job's path follows every move
/// recorded after that age, oldest first, so a move and then its undo lands where the file is. A
/// move is kept only while some queued or retrying job is older than it. Clones share one table.
#[derive(Clone, Default)]
pub struct LyricsPathRemap {
    inner: Arc<parking_lot::Mutex<RemapTable>>,
}

#[derive(Default)]
struct RemapTable {
    /// The age of the newest move recorded.
    clock: u64,
    /// The moves still needed, oldest first: (age, from, to).
    moves: std::collections::VecDeque<(u64, String, String)>,
    /// The ages of the jobs queued, being written, or waiting to be tried again, with how many
    /// jobs have each.
    waiting: std::collections::BTreeMap<u64, usize>,
}

impl RemapTable {
    /// Drops the moves no waiting job is older than.
    fn prune(&mut self) {
        let oldest = self.waiting.keys().next().copied();
        while let Some((age, ..)) = self.moves.front() {
            if oldest.is_some_and(|oldest| oldest < *age) {
                break;
            }
            self.moves.pop_front();
        }
    }

    fn release(&mut self, age: u64) {
        if let Some(count) = self.waiting.get_mut(&age) {
            *count -= 1;
            if *count == 0 {
                self.waiting.remove(&age);
            }
        }
        self.prune();
    }

    fn hold(&mut self) -> u64 {
        *self.waiting.entry(self.clock).or_default() += 1;
        self.clock
    }

    fn follow(&self, path: &str, since: u64) -> String {
        let mut path = path.to_string();
        for (_, from, to) in self.moves.iter().filter(|(age, ..)| *age > since) {
            if let Some(now) = octo_core::common::PathHelper::moved_path(&path, from, to) {
                path = now;
            }
        }
        path
    }
}

impl LyricsPathRemap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the tidy's moves (`from`, `to`), in the order it made them. Called while the tidy
    /// still holds the gate, so no job takes its share between a move and its record.
    pub fn record(&self, moves: &[(String, String)]) {
        let mut table = self.inner.lock();
        for (from, to) in moves {
            table.clock += 1;
            let age = table.clock;
            table.moves.push_back((age, from.clone(), to.clone()));
        }
        table.prune();
    }

    /// Where `path` is now, through every move kept, oldest first.
    pub fn map(&self, path: &str) -> String {
        self.inner.lock().follow(path, 0)
    }

    /// A new job's age: its path is true now. Every age handed out is let go of once
    /// ([`Self::release`]).
    fn hold(&self) -> u64 {
        self.inner.lock().hold()
    }

    /// Lets go of a job's age, and of the moves only it was waiting on.
    fn release(&self, age: u64) {
        self.inner.lock().release(age);
    }

    /// A job of age `age` at `path` once the gate's share is held: where its file is now, and its
    /// new age (the old one let go of).
    fn advance(&self, path: &Path, age: u64) -> (PathBuf, u64) {
        let mut table = self.inner.lock();
        let now = PathBuf::from(table.follow(&path.to_string_lossy(), age));
        let fresh = table.hold();
        table.release(age);
        (now, fresh)
    }

    /// How many moves are kept (tests).
    #[cfg(test)]
    pub(crate) fn kept(&self) -> usize {
        self.inner.lock().moves.len()
    }

    /// How many jobs hold an age (tests).
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.inner.lock().waiting.values().sum()
    }

    /// A job's age, held as a waiting job holds it (tests).
    #[cfg(test)]
    pub(crate) fn hold_for_test(&self) -> u64 {
        self.hold()
    }

    #[cfg(test)]
    pub(crate) fn release_for_test(&self, age: u64) {
        self.release(age);
    }
}

/// A queued job and its age in the [`LyricsPathRemap`].
struct Queued {
    job: LyricsJob,
    age: u64,
}

/// What a Save tells the undo journal before each write: the path, its kind
/// ([`LyricsUndoJournal::BESIDE`] or [`LyricsUndoJournal::INSIDE`]), and what was there.
pub type UndoRecord<'a> = &'a (dyn Fn(&Path, &str, Option<&str>) -> anyhow::Result<()> + Send + Sync);

/// Writes a lyrics file beside a download (#52), off the download path. Downloads run one at a
/// time under a lock, and LRCLIB can take seconds or shed load with a 503, so fetching lyrics
/// inline would stall every download queued behind this one. A sidecar Navidrome reads at
/// request time needs no rescan, so arriving a little later costs nothing.
///
/// Synced lyrics go in a .lrc, plain ones in a .txt: both are in Navidrome's default
/// LyricsPriority, and a .txt says plainly that there is no timing. Word-timed lyrics are
/// enhanced LRC: every line keeps its standard [mm:ss.xx] tag, so any player that reads .lrc
/// shows them line by line, and one that knows <mm:ss.xx> word tags (Navidrome among them,
/// which turns them into OpenSubsonic word cues) gets the words too.
///
/// A .lrc Octo writes opens with [re:Octo], LRC's own "made by" tag, which every reader skips,
/// and so do lyrics Octo writes inside a song (LYRICS_SAVE_TO inside or both). It is how Octo
/// knows lyrics are its own: an instrumental gets nothing, and lyrics Octo did not write, beside
/// the song or inside it, are never replaced. Asked to upgrade, Octo looks again for a song
/// whose lyrics are weaker than the sources would choose now (the song's own lyrics rank like
/// any source, see LYRICS_SOURCES): its own are replaced, and anyone else's get the better ones
/// beside them, as a .lrc, which Navidrome serves ahead of lyrics in the tags.
pub struct LyricsSidecarWriter {
    lyrics: Arc<LyricsService>,
    settings: Arc<SettingsStore>,
    tags: Arc<dyn LyricsTagAccess>,
    /// Set once the library service exists. Without it no scan is asked for (the tests).
    library: OnceLock<Arc<dyn ILocalLibraryService>>,
    sender: mpsc::Sender<Queued>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<Queued>>,
    /// Where the library tidy moved songs while their jobs waited.
    remap: LyricsPathRemap,
    /// How long a job whose services did not answer waits before it is tried again
    /// ([`Self::RETRY_DELAY`]; tests make it seconds).
    retry_delay: Duration,
    /// Tries again the full queue had no room for.
    dropped_retries: Arc<AtomicUsize>,
    scan_pending: Arc<AtomicBool>,
    /// The downloads log a download's look reports to; set once the tracker exists.
    tracker: OnceLock<Arc<AcquisitionTracker>>,
    /// The music folder's one-writer gate: a write waits while a library job holds it.
    write_gate: OnceLock<LibraryWriteGate>,
    /// Where a replacement someone asked for records what it wrote over, so the lyrics page's
    /// Undo puts it back.
    undo: OnceLock<Arc<LyricsUndoJournal>>,
}

impl LyricsSidecarWriter {
    /// How long a job whose services did not answer waits before it is tried again.
    pub const RETRY_DELAY: Duration = Duration::from_secs(10 * 60);
    pub const MAX_ATTEMPTS: i32 = 3;

    /// The first line of every .lrc Octo writes.
    pub const OCTO_MARK: &'static str = OCTO_MARK;

    /// How long after lyrics are written inside a song Navidrome is asked to scan, so a run of
    /// writes asks once.
    pub const SCAN_DELAY: Duration = Duration::from_secs(60);

    /// The queue holds this many jobs; a job past that is dropped (`DropWrite`).
    const QUEUE_CAPACITY: usize = 256;

    pub fn new(lyrics: Arc<LyricsService>, settings: Arc<SettingsStore>) -> Self {
        Self::with_tags(lyrics, settings, Arc::new(TagLibLyricsTags))
    }

    pub fn with_tags(
        lyrics: Arc<LyricsService>,
        settings: Arc<SettingsStore>,
        tags: Arc<dyn LyricsTagAccess>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(Self::QUEUE_CAPACITY);
        Self {
            lyrics,
            settings,
            tags,
            library: OnceLock::new(),
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            remap: LyricsPathRemap::new(),
            retry_delay: Self::RETRY_DELAY,
            dropped_retries: Arc::new(AtomicUsize::new(0)),
            scan_pending: Arc::new(AtomicBool::new(false)),
            tracker: OnceLock::new(),
            write_gate: OnceLock::new(),
            undo: OnceLock::new(),
        }
    }

    /// The writer following `remap`, the table the library tidy records its moves in, in place of
    /// a table of its own.
    pub fn with_remap(mut self, remap: LyricsPathRemap) -> Self {
        self.remap = remap;
        self
    }

    /// The writer trying a job again `delay` after its services did not answer, in place of
    /// [`Self::RETRY_DELAY`] (tests).
    pub fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay;
        self
    }

    /// The table this writer's jobs follow the library tidy's moves through; clones share it.
    pub fn path_remap(&self) -> LyricsPathRemap {
        self.remap.clone()
    }

    /// How many tries again were dropped because the queue was full.
    pub fn dropped_retries(&self) -> usize {
        self.dropped_retries.load(Ordering::SeqCst)
    }

    /// How many jobs wait in the queue (tests).
    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        Self::QUEUE_CAPACITY - self.sender.capacity()
    }

    /// The lyrics undo journal a replacement records into. Only the first call counts.
    pub fn set_undo_journal(&self, journal: Arc<LyricsUndoJournal>) {
        let _ = self.undo.set(journal);
    }

    /// The gate shared with the library jobs. Only the first call counts.
    pub fn set_write_gate(&self, gate: LibraryWriteGate) {
        let _ = self.write_gate.set(gate);
    }

    /// The downloads log a download's look reports to. Only the first call counts.
    pub fn set_tracker(&self, tracker: Arc<AcquisitionTracker>) {
        let _ = self.tracker.set(tracker);
    }

    /// The library service, asked for a scan after lyrics are written inside songs. Only the
    /// first call counts.
    pub fn set_library(&self, library: Arc<dyn ILocalLibraryService>) {
        let _ = self.library.set(library);
    }

    fn save_to(&self) -> &'static str {
        LyricsSaveTo::normalize(Some(&self.settings.current().metadata.save_lyrics_to))
    }

    /// Queue a song. False when the queue is full and the job was dropped.
    pub fn try_enqueue(&self, job: LyricsJob) -> bool {
        let age = self.remap.hold();
        if self.sender.try_send(Queued { job, age }).is_err() {
            self.remap.release(age);
            return false;
        }
        true
    }

    /// The worker's loop: one job at a time until `stopping`.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let mut queue = self.receiver.lock().await;
        loop {
            let queued = tokio::select! {
                queued = queue.recv() => queued,
                () = stopping.cancelled() => break,
            };
            let Some(Queued { mut job, age }) = queued else {
                break;
            };
            let share = match self.write_gate.get() {
                Some(gate) => Some(gate.write().await),
                None => None,
            };
            // The tidy may have moved the song while the job waited; it cannot move it while the
            // share is held.
            let (now, age) = self.remap.advance(&job.audio_path, age);
            job.audio_path = now;
            let written = self.write(&job, false, &stopping).await;
            drop(share);
            // One job's failure is logged and the loop goes on.
            match written {
                _ if stopping.is_cancelled() => {
                    self.remap.release(age);
                    break;
                }
                Ok(write) => {
                    self.report(&job, &write);
                    if write.outcome == LyricsWriteOutcome::Retrying {
                        self.retry_later(job.clone().with_attempt(job.attempt + 1), age, stopping.clone());
                    } else {
                        self.remap.release(age);
                    }
                }
                Err(error) => {
                    self.remap.release(age);
                    warn!("Could not write lyrics for {}: {error}", job.audio_path.display());
                }
            }
        }
        Ok(())
    }

    /// Tells the download's log what the look found, in words, and where the lyrics went.
    pub fn report(&self, job: &LyricsJob, write: &LyricsWrite) {
        let (Some(tracker), Some(row)) = (self.tracker.get(), job.acquisition.as_deref()) else {
            return;
        };
        let Some((provider, id)) = row.split_once(':').filter(|(provider, _)| !provider.is_empty()) else {
            return;
        };
        let place = if job.audio_path.exists() {
            self.song_lyrics(&job.audio_path).place
        } else {
            SongLyricsPlace::None
        };
        let (Some(text), detail) = Self::report_text(job, write, place) else {
            return;
        };
        tracker.log(
            provider,
            id,
            acquisition_event_kinds::LYRICS,
            &text,
            detail.as_deref(),
        );
    }

    /// The log's words for one look, or None when there is nothing to say yet.
    pub fn report_text(
        job: &LyricsJob,
        write: &LyricsWrite,
        place: SongLyricsPlace,
    ) -> (Option<String>, Option<String>) {
        let said = |text: &str, detail: Option<String>| (Some(text.to_string()), detail);
        match (write.outcome, &write.result) {
            (LyricsWriteOutcome::Written | LyricsWriteOutcome::Upgraded, Some(found)) => {
                let timing = match found.timing() {
                    LyricsTiming::Word => "Word by word lyrics",
                    LyricsTiming::Line => "Synced lyrics",
                    _ => "Plain lyrics",
                };
                let detail = match place {
                    SongLyricsPlace::Inside => "Embedded in the song".to_string(),
                    SongLyricsPlace::Beside => format!(
                        "Saved beside the song as a {} file",
                        if found.has_synced() { ".lrc" } else { ".txt" }
                    ),
                    _ => "Saved".to_string(),
                };
                (Some(format!("{timing} from {}", found.source)), Some(detail))
            }
            (LyricsWriteOutcome::Instrumental, result) => said(
                "No lyrics: an instrumental",
                result
                    .as_ref()
                    .map(|said| format!("{} says it has no words", said.source)),
            ),
            (LyricsWriteOutcome::NotFound, _) => said(
                "No lyrics found",
                Some("None of the lyrics sources has this song".to_string()),
            ),
            (LyricsWriteOutcome::AlreadyThere, _) => said(
                "The song already has lyrics",
                Some("Left as they are".to_string()),
            ),
            (LyricsWriteOutcome::Retrying, _) => said(
                "No lyrics service answered",
                Some(format!(
                    "Trying again in {} minutes (try {} of {})",
                    Self::RETRY_DELAY.as_secs() / 60,
                    job.attempt,
                    Self::MAX_ATTEMPTS
                )),
            ),
            (LyricsWriteOutcome::GaveUp, _) => said(
                "No lyrics service answered",
                Some(format!("Gave up after {} tries", Self::MAX_ATTEMPTS)),
            ),
            _ => (None, None),
        }
    }

    /// Look the song up and save what was found. A song that already has lyrics is left alone,
    /// unless `upgrade`: then it is looked up with its own lyrics ranked among the sources, and
    /// better ones are saved (see the type's summary for where).
    pub async fn write(
        &self,
        job: &LyricsJob,
        upgrade: bool,
        ct: &CancellationToken,
    ) -> anyhow::Result<LyricsWrite> {
        if !job.audio_path.exists() {
            return Ok(LyricsWrite::new(LyricsWriteOutcome::Gone, None));
        }
        let has = self.song_lyrics(&job.audio_path);
        if has.place != SongLyricsPlace::None && (!upgrade || has.timing == LyricsTiming::Word || has.unknown)
        {
            return Ok(LyricsWrite::new(LyricsWriteOutcome::AlreadyThere, None));
        }

        let lookup = self.lyrics.find(&self.query_of(job), ct, has.timing).await;
        if lookup.transient {
            if job.attempt < Self::MAX_ATTEMPTS {
                return Ok(LyricsWrite::new(LyricsWriteOutcome::Retrying, None));
            }
            info!(
                "No lyrics service answered for '{} - {}' after {} tries",
                job.artist, job.title, job.attempt
            );
            return Ok(LyricsWrite::new(LyricsWriteOutcome::GaveUp, None));
        }

        if has.place != SongLyricsPlace::None {
            let Some(better) = lookup
                .result
                .filter(|better| !better.is_songs_own() && better.timing() > has.timing)
            else {
                return Ok(LyricsWrite::new(LyricsWriteOutcome::AlreadyThere, None));
            };
            if !self.save(&job.audio_path, &better, None).await? {
                return Ok(LyricsWrite::new(LyricsWriteOutcome::AlreadyThere, None));
            }
            info!(
                "Lyrics for '{} - {}' upgraded from {:?} to {:?} from {}",
                job.artist,
                job.title,
                has.timing,
                better.timing(),
                better.source
            );
            return Ok(LyricsWrite::new(LyricsWriteOutcome::Upgraded, Some(better)));
        }

        match lookup.result {
            Some(instrumental) if instrumental.instrumental => {
                info!(
                    "{} says '{} - {}' is instrumental; no lyrics file",
                    instrumental.source, job.artist, job.title
                );
                Ok(LyricsWrite::new(
                    LyricsWriteOutcome::Instrumental,
                    Some(instrumental),
                ))
            }
            Some(found) if !found.is_songs_own() && (found.has_synced() || found.has_plain()) => {
                if !self.save(&job.audio_path, &found, None).await? {
                    return Ok(LyricsWrite::new(LyricsWriteOutcome::AlreadyThere, None));
                }
                info!(
                    "Lyrics for '{} - {}' from {} ({})",
                    job.artist,
                    job.title,
                    found.source,
                    to_lower_invariant(&format!("{:?}", found.timing()))
                );
                Ok(LyricsWrite::new(LyricsWriteOutcome::Written, Some(found)))
            }
            _ => {
                info!("No lyrics found for '{} - {}'", job.artist, job.title);
                Ok(LyricsWrite::new(LyricsWriteOutcome::NotFound, None))
            }
        }
    }

    /// What the sources find for a song, its own lyrics ranked among them, without saving
    /// anything: the lyrics page's preview.
    pub async fn look_up(
        &self,
        job: &LyricsJob,
        songs_own: LyricsTiming,
        ct: &CancellationToken,
    ) -> LyricsLookup {
        self.lyrics.find(&self.query_of(job), ct, songs_own).await
    }

    /// The lyrics are looked up at the length the file really plays: synced lines timed for
    /// another take of another length never fit. The length the job was given counts only when
    /// the file cannot say.
    fn query_of(&self, job: &LyricsJob) -> LyricsQuery {
        LyricsQuery::new(
            job.artist.clone(),
            job.title.clone(),
            job.album.clone(),
            self.tags
                .duration_seconds(&job.audio_path)
                .or(job.duration_seconds),
        )
    }

    /// Replace a song's lyrics with lyrics someone chose. Only where Octo may write: where the
    /// song has no lyrics, or only Octo's; lyrics the owner put there are never touched.
    pub async fn replace(
        &self,
        audio_path: &Path,
        chosen: &LyricsResult,
        _ct: &CancellationToken,
    ) -> anyhow::Result<bool> {
        if !audio_path.exists() || (!chosen.has_synced() && !chosen.has_plain()) {
            return Ok(false);
        }
        let _share = match self.write_gate.get() {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        let has = self.song_lyrics(audio_path);
        if has.place != SongLyricsPlace::None && !has.octos {
            return Ok(false);
        }
        let Some(journal) = self.undo.get() else {
            return self.save(audio_path, chosen, None).await;
        };
        let run_id = super::lyrics_library_job::new_run_id();
        let record = |path: &Path, kind: &str, before: Option<&str>| -> anyhow::Result<()> {
            Ok(journal.record(&path.to_string_lossy(), kind, before, &run_id)?)
        };
        self.save(audio_path, chosen, Some(&record)).await
    }

    /// Save lyrics someone picked on the lyrics page, where LYRICS_SAVE_TO says and Octo may
    /// write, telling `record` what each write replaces (path, kind, what was there) before it
    /// happens. False when there was nowhere Octo may write.
    pub async fn save_chosen(
        &self,
        audio_path: &Path,
        found: &LyricsResult,
        record: UndoRecord<'_>,
        _ct: &CancellationToken,
    ) -> anyhow::Result<bool> {
        self.save(audio_path, found, Some(record)).await
    }

    /// Save lyrics where LYRICS_SAVE_TO says, replacing only Octo's own. Where that is not
    /// allowed (inside a song whose tags hold someone else's lyrics), they go beside it
    /// instead, which takes nothing away. False when there was nowhere to put them.
    async fn save(
        &self,
        audio_path: &Path,
        found: &LyricsResult,
        record: Option<UndoRecord<'_>>,
    ) -> anyhow::Result<bool> {
        let stem = song_lyrics::stem(audio_path);
        let save_to = self.save_to();
        let inside = save_to != LyricsSaveTo::BESIDE && self.may_write_inside(audio_path);
        let beside = save_to != LyricsSaveTo::INSIDE || !inside;
        let mut saved = false;
        if inside {
            if let Some(record) = record {
                record(
                    audio_path,
                    LyricsUndoJournal::INSIDE,
                    self.tag_lyrics(audio_path).as_deref(),
                )?;
            }
            if self.write_inside(audio_path, found) {
                saved = true;
                self.scan_soon();
            }
        }
        if beside && song_lyrics::may_write_beside(&stem, found.has_synced()) {
            let target = with_suffix(&stem, if found.has_synced() { ".lrc" } else { ".txt" });
            if let Some(record) = record {
                let before = if target.exists() {
                    Some(tokio::fs::read(&target).await.map(|bytes| utf8_text(&bytes))?)
                } else {
                    None
                };
                record(&target, LyricsUndoJournal::BESIDE, before.as_deref())?;
            }
            Self::write_file(&stem, found).await?;
            saved = true;
        }
        Ok(saved)
    }

    /// Put back what one Save replaced. A file Octo added is removed, and only while it is
    /// still Octo's; lyrics in a song's tags go back only while the tags hold Octo's. False when
    /// the file has changed since, and was left alone.
    pub fn restore(&self, entry: &LyricsUndoEntry) -> bool {
        let path = Path::new(&entry.path);
        let restored = (|| -> anyhow::Result<bool> {
            if entry.kind == LyricsUndoJournal::INSIDE {
                if !path.exists() || !self.may_write_inside(path) {
                    return Ok(false);
                }
                self.tags.write_lyrics(path, entry.before.as_deref())?;
                self.scan_soon();
                return Ok(true);
            }
            let ours = if ends_with_ignore_case(&entry.path, ".lrc") {
                Self::is_octos(path)
            } else {
                path.exists()
            };
            if !ours {
                return Ok(false);
            }
            match &entry.before {
                None => std::fs::remove_file(path)?,
                Some(before) => std::fs::write(path, before.as_bytes())?,
            }
            Ok(true)
        })();
        restored.unwrap_or_else(|error| {
            warn!("Could not put back the lyrics of {}: {error}", entry.path);
            false
        })
    }

    /// Whether a .lrc opens with Octo's own mark.
    pub fn is_octos(lrc_path: &Path) -> bool {
        song_lyrics::is_octos(lrc_path)
    }

    /// The tags seam the writer reads and writes songs through, which the library job reads
    /// songs through too.
    pub fn tags(&self) -> &Arc<dyn LyricsTagAccess> {
        &self.tags
    }

    /// What the song has now, with the tags read through the seam.
    pub fn song_lyrics(&self, audio_path: &Path) -> SongLyrics {
        let inside = self.tags.read_lyrics(audio_path).ok().flatten();
        song_lyrics::of_with_tag_lyrics(audio_path, inside.as_deref())
    }

    /// Whether Octo may write inside the song: the tags hold no lyrics, or Octo's.
    fn may_write_inside(&self, audio_path: &Path) -> bool {
        match self.tags.read_lyrics(audio_path) {
            Ok(Some(text)) if !is_null_or_white_space(Some(&text)) => SongLyrics::is_octos_text(&text),
            Ok(_) => true,
            Err(_) => false,
        }
    }

    /// `ReadTagLyrics`: the lyrics in the tags, None when there are none or they cannot be read.
    fn tag_lyrics(&self, audio_path: &Path) -> Option<String> {
        self.tags
            .read_lyrics(audio_path)
            .ok()
            .flatten()
            .filter(|text| !text.is_empty())
    }

    /// UTF-8 without a byte order mark; not written atomically.
    async fn write_file(stem: &Path, found: &LyricsResult) -> io::Result<()> {
        if found.has_synced() {
            let text = found.synced.as_deref().unwrap_or("").replace("\r\n", "\n");
            tokio::fs::write(
                with_suffix(stem, ".lrc"),
                format!("{OCTO_MARK}\n{}\n", text.trim()),
            )
            .await
        } else {
            let text = found.plain.as_deref().unwrap_or("").replace("\r\n", "\n");
            tokio::fs::write(with_suffix(stem, ".txt"), format!("{}\n", text.trim())).await
        }
    }

    /// The lyrics in the song's own tags, marked as Octo's. False when the file could not be
    /// written.
    fn write_inside(&self, audio_path: &Path, found: &LyricsResult) -> bool {
        let text = if found.has_synced() {
            found.synced.as_deref()
        } else {
            found.plain.as_deref()
        }
        .unwrap_or("")
        .replace("\r\n", "\n");
        match self
            .tags
            .write_lyrics(audio_path, Some(&format!("{OCTO_MARK}\n{}", text.trim())))
        {
            Ok(()) => true,
            Err(error) => {
                warn!("Could not write lyrics inside {}: {error}", audio_path.display());
                false
            }
        }
    }

    /// Navidrome reads a song's tags only when it scans, so one scan is asked for a little
    /// after lyrics are written inside, once for a run of them.
    fn scan_soon(&self) {
        let Some(library) = self.library.get().cloned() else {
            return;
        };
        if self.scan_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let pending = self.scan_pending.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            pending.store(false, Ordering::SeqCst);
            return;
        };
        runtime.spawn(async move {
            tokio::time::sleep(Self::SCAN_DELAY).await;
            pending.store(false, Ordering::SeqCst);
            // The answer is only whether a scan started, which nothing here needs.
            library.trigger_library_scan(false).await;
        });
    }

    /// Queues `job` again after the retry delay, keeping its age, so it follows the moves made
    /// meanwhile. A try again the full queue has no room for is dropped, counted and its age let
    /// go of.
    fn retry_later(&self, job: LyricsJob, age: u64, stopping: CancellationToken) {
        let (sender, remap, dropped, delay) = (
            self.sender.clone(),
            self.remap.clone(),
            self.dropped_retries.clone(),
            self.retry_delay,
        );
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(delay) => {
                    let path = job.audio_path.clone();
                    if sender.try_send(Queued { job, age }).is_err() {
                        dropped.fetch_add(1, Ordering::SeqCst);
                        remap.release(age);
                        warn!(
                            "The lyrics queue was full, so {} was not looked up again",
                            path.display()
                        );
                    }
                }
                // Shutting down.
                () = stopping.cancelled() => remap.release(age),
            }
        });
    }
}

/// Whether `s` ends with `suffix`, ignoring case.
fn ends_with_ignore_case(s: &str, suffix: &str) -> bool {
    let count = suffix.chars().count();
    let Some((start, _)) = s.char_indices().rev().nth(count.saturating_sub(1)) else {
        return false;
    };
    count > 0 && eq_ignore_case(&s[start..], suffix)
}

fn with_suffix(stem: &Path, suffix: &str) -> PathBuf {
    let mut path: OsString = stem.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// The text as UTF-8: a byte order mark skipped, bytes that are not UTF-8 as U+FFFD.
fn utf8_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
#[path = "lyrics_sidecar_writer_tests.rs"]
mod tests;
