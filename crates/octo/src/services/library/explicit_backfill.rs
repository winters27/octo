//! The dashboard's "Mark explicit songs".
//!
//! Marks the songs already in the library explicit or clean, the way new downloads are marked
//! (ITUNESADVISORY on MP3 and FLAC, the rtng atom on M4A), so the apps can show it. Navidrome
//! reads the advisory into each song's explicitStatus at its next scan.
//!
//! Three steps, like the genre re-tag. A preview looks up every song Navidrome lists with no
//! explicit status and whose file carries no advisory (`explicit_lookup`, at a polite pace on
//! the catalog's background lane), and writes nothing. Apply writes only what that preview found
//! explicit or clean, in place (Navidrome keeps each song's id, plays and playlist places),
//! skipping a file changed since; "not explicit" is counted but not written, since a missing mark
//! already means that and every write sends the whole file back to a network mount. Each write
//! goes in the tag-edit journal, so Undo (or an app's own undo of that song) takes it out again.
//! Then one normal Navidrome scan.
//!
//! The run's state file is `explicit_backfill_store`; this module adds the store that keeps
//! it in memory and flushes it, the worker, and its requests.

use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use futures::future::BoxFuture;
use octo_core::common::dotnet::{eq_ignore_case, is_blank};
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::settings::SettingsStore;
use octo_core::tagging::explicit_advisory;
use octo_core::tagging::explicit_lookup::{
    self, ExplicitFinding, ExplicitOutcome, IExplicitCatalog, LibrarySongFacts,
};
use octo_media::tags::tag_writer_extras::{self, TagFields};
use octo_media::tags::{TagError, TagFile};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::explicit_backfill_store::{self as state, MAX_ERRORS, MAX_ROWS};
pub use super::explicit_backfill_store::{
    ExplicitBackfillMode, ExplicitBackfillRow, ExplicitBackfillRun, ExplicitBackfillSong,
    ExplicitBackfillStatus, writes,
};
use super::library_tag_edits::{self, TagEditEntry, TagEditJournal, tag_edit_kinds};
use super::library_walk;
use super::library_write_gate::LibraryWriteGate;
use super::navidrome_song_list::{self, NavidromeSongEntry};
use crate::services::local::ILocalLibraryService;
use crate::services::subsonic::NavidromeIdentityService;

/// What a run does: look songs up and write nothing (Preview), write what a finished preview
/// found (Apply), or take out what the last write put in (Undo). `username` is who asked, for
/// the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitBackfillRequest {
    pub mode: ExplicitBackfillMode,
    pub username: String,
}

impl ExplicitBackfillRequest {
    pub fn new(mode: ExplicitBackfillMode, username: impl Into<String>) -> Self {
        Self {
            mode,
            username: username.into(),
        }
    }

    /// A request from the dashboard, the default user.
    pub fn of(mode: ExplicitBackfillMode) -> Self {
        Self::new(mode, "dashboard")
    }
}

/// The run's state, beside the settings, so a preview survives a restart. It is flushed every
/// 10 s and once more at shutdown by [`ExplicitBackfillStore::run_flusher`], registered as a
/// worker by the app state.
pub struct ExplicitBackfillStore {
    path: Option<PathBuf>,
    run: Mutex<ExplicitBackfillRun>,
    dirty: AtomicBool,
}

impl ExplicitBackfillStore {
    pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

    /// `path` None (or blank) keeps the run in memory only.
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.filter(|path| !is_blank(&path.to_string_lossy()));
        let run = path.as_deref().and_then(state::load).unwrap_or_default();
        Self {
            path,
            run: Mutex::new(run),
            dirty: AtomicBool::new(false),
        }
    }

    pub fn current(&self) -> ExplicitBackfillRun {
        self.run.lock().clone()
    }

    /// Reads the run without copying it (its rows can be thousands).
    pub fn read<R>(&self, read: impl FnOnce(&ExplicitBackfillRun) -> R) -> R {
        read(&self.run.lock())
    }

    pub fn update(&self, change: impl FnOnce(&mut ExplicitBackfillRun)) {
        {
            let mut run = self.run.lock();
            change(&mut run);
            if run.errors.len() > MAX_ERRORS {
                let excess = run.errors.len() - MAX_ERRORS;
                run.errors.drain(..excess);
            }
            run.rows.truncate(MAX_ROWS);
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Follow a folder the library tidy moved: the run's songs name where their files are now.
    /// Answers how many followed.
    pub fn move_paths(&self, from: &str, to: &str) -> usize {
        self.move_paths_batch(&[(from.to_string(), to.to_string())])
    }

    /// [`Self::move_paths`] for each `(from, to)` in order, so `a` to `b` then `b` to `c` ends at
    /// `c`, written once. Answers how many followed, each move counted on its own.
    pub fn move_paths_batch(&self, moves: &[(String, String)]) -> usize {
        let mut moved = 0;
        {
            let mut run = self.run.lock();
            let run = &mut *run;
            for (from, to) in moves {
                let paths = run
                    .rows
                    .iter_mut()
                    .map(|row| &mut row.path)
                    .chain(run.queue.iter_mut().map(|song| &mut song.path));
                for path in paths {
                    if let Some(now) = octo_core::common::PathHelper::moved_path(path, from, to) {
                        *path = now;
                        moved += 1;
                    }
                }
            }
        }
        if moved > 0 {
            self.dirty.store(true, Ordering::SeqCst);
            self.flush();
        }
        moved
    }

    pub fn replace(&self, run: ExplicitBackfillRun) {
        *self.run.lock() = run;
        self.dirty.store(true, Ordering::SeqCst);
        self.flush();
    }

    /// Writes the run when it changed since the last write.
    pub fn flush(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let run = self.current();
        if !state::save(path, &run) {
            self.dirty.store(true, Ordering::SeqCst);
        }
    }

    /// Flushes every [`Self::FLUSH_INTERVAL`] until canceled, then once more.
    pub async fn run_flusher(&self, stopping: CancellationToken) {
        if self.path.is_none() {
            return;
        }
        let start = tokio::time::Instant::now() + Self::FLUSH_INTERVAL;
        let mut ticks = tokio::time::interval_at(start, Self::FLUSH_INTERVAL);
        loop {
            tokio::select! {
                _ = ticks.tick() => self.flush(),
                () = stopping.cancelled() => break,
            }
        }
        self.flush();
    }
}

/// The songs to look at, when not Navidrome's list (tests): each song and the explicit status
/// Navidrome has for it.
pub type ListSongs =
    Arc<dyn Fn() -> BoxFuture<'static, Vec<(ExplicitBackfillSong, Option<String>)>> + Send + Sync>;

/// What the worker reaches besides its store, journal and catalog. A service left out is one
/// the host did not register, which the run copes without.
#[derive(Clone, Default)]
pub struct ExplicitBackfillServices {
    pub identity: Option<Arc<NavidromeIdentityService>>,
    pub http: Option<reqwest::Client>,
    pub library: Option<Arc<dyn ILocalLibraryService>>,
}

/// Stopped by the host's shutdown: the run counts as interrupted.
struct Stopped;

/// The marking itself: a background worker, one run at a time.
pub struct ExplicitBackfill {
    store: Arc<ExplicitBackfillStore>,
    journal: Arc<TagEditJournal>,
    catalog: Arc<dyn IExplicitCatalog>,
    /// `Library:DownloadPath` and the Navidrome address, read at use.
    settings: Arc<SettingsStore>,
    services: ExplicitBackfillServices,
    cancel_requested: AtomicBool,
    pending: AtomicBool,
    sender: mpsc::Sender<ExplicitBackfillRequest>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<ExplicitBackfillRequest>>,
    list_songs: Option<ListSongs>,
    pace: Duration,
    busy_wait: Duration,
    /// Apply and Undo write the library one job at a time with the other library jobs.
    write_gate: LibraryWriteGate,
    /// With Family on, explicit marks are written to the shared library only.
    scope: Option<crate::services::family::ScopeResolver>,
}

impl ExplicitBackfill {
    /// Between two songs' lookups: each is one to three catalog requests, and a search someone
    /// is waiting on must never queue behind this.
    pub const PACE: Duration = Duration::from_millis(700);

    /// How long to wait when the catalog does not answer, and how often to try.
    pub const BUSY_WAIT: Duration = Duration::from_secs(30);
    const BUSY_TRIES: usize = 3;

    /// Journal and state are saved every this many writes.
    const SAVE_EVERY: usize = 25;

    pub fn new(
        store: Arc<ExplicitBackfillStore>,
        journal: Arc<TagEditJournal>,
        catalog: Arc<dyn IExplicitCatalog>,
        settings: Arc<SettingsStore>,
        services: ExplicitBackfillServices,
    ) -> Self {
        // Capacity 1, and a request past it is dropped: `pending` keeps a second one from ever
        // being written while one waits.
        let (sender, receiver) = mpsc::channel(1);
        Self {
            store,
            journal,
            catalog,
            settings,
            services,
            cancel_requested: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            list_songs: None,
            pace: Self::PACE,
            busy_wait: Self::BUSY_WAIT,
            write_gate: LibraryWriteGate::new(),
            scope: None,
        }
    }

    /// Keeps the marking to the shared library while Family is on.
    pub fn with_library_scope(mut self, scope: crate::services::family::ScopeResolver) -> Self {
        self.scope = Some(scope);
        self
    }

    /// The songs the marking may touch: all of them, or the shared library's while members
    /// have libraries of their own (Family on, or turned off with their libraries still there).
    fn shared_only(
        &self,
        listed: Vec<(ExplicitBackfillSong, Option<String>)>,
    ) -> Vec<(ExplicitBackfillSong, Option<String>)> {
        match self.scope.as_ref().filter(|scope| scope.keeps_shared()) {
            Some(scope) => {
                let view = scope.shared_view();
                listed
                    .into_iter()
                    .filter(|(song, _)| {
                        view.allows(
                            &crate::services::family::LibraryScope::Shared,
                            song.navidrome_id.as_deref(),
                            Some(&song.path),
                        )
                    })
                    .collect()
            }
            None => listed,
        }
    }

    /// The gate shared with the other jobs that write the music folder.
    pub fn with_write_gate(mut self, gate: LibraryWriteGate) -> Self {
        self.write_gate = gate;
        self
    }

    /// What the dashboard calls this job when another one has to wait for it.
    pub const JOB_NAME: &'static str = "Explicit marking";

    /// The songs to look at instead of Navidrome's list, and no waits (tests).
    pub fn for_tests(mut self, list_songs: ListSongs) -> Self {
        self.list_songs = Some(list_songs);
        self.pace = Duration::ZERO;
        self.busy_wait = Duration::ZERO;
        self
    }

    pub fn current(&self) -> ExplicitBackfillRun {
        self.store.current()
    }

    pub fn store(&self) -> &Arc<ExplicitBackfillStore> {
        &self.store
    }

    pub fn is_busy(&self) -> bool {
        self.store
            .read(|run| run.status == ExplicitBackfillStatus::Running)
            || self.pending.load(Ordering::SeqCst)
    }

    /// Whether the last Apply left anything for Undo to take out.
    pub fn can_undo(&self) -> bool {
        self.store
            .read(|run| run.applied_run_id.clone())
            .is_some_and(|run| !self.journal.of_run(&run).is_empty())
    }

    /// False when a run is going or waiting to start.
    pub fn try_enqueue(&self, request: ExplicitBackfillRequest) -> bool {
        if self
            .store
            .read(|run| run.status == ExplicitBackfillStatus::Running)
            || self
                .pending
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return false;
        }
        if self.sender.try_send(request).is_ok() {
            return true;
        }
        self.pending.store(false, Ordering::SeqCst);
        false
    }

    pub fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
    }

    fn cancelled(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    /// The music folder the run walks when Navidrome cannot list it.
    pub fn music_path(&self) -> String {
        let fallback = self
            .settings
            .raw("Library:DownloadPath")
            .unwrap_or_else(|| "./downloads".to_string());
        match &self.services.identity {
            Some(identity) => identity.effective_download_path(&fallback),
            None => fallback,
        }
    }

    /// The worker loop: one request at a time until the host stops.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let mut queue = self.receiver.lock().await;
        loop {
            let request = tokio::select! {
                request = queue.recv() => request,
                () = stopping.cancelled() => break,
            };
            let Some(request) = request else {
                break;
            };
            self.cancel_requested.store(false, Ordering::SeqCst);
            let outcome = self.run_one(&request, &stopping).await;
            let interrupted = matches!(outcome, Err(Stopped));
            if interrupted {
                self.store
                    .update(|run| run.status = ExplicitBackfillStatus::Interrupted);
            }
            self.journal.flush();
            self.store.flush();
            self.pending.store(false, Ordering::SeqCst);
            if interrupted {
                break;
            }
        }
        Ok(())
    }

    /// Runs one request to the end, for tests.
    pub async fn run_to_end(&self, request: ExplicitBackfillRequest) {
        self.pending.store(true, Ordering::SeqCst);
        self.cancel_requested.store(false, Ordering::SeqCst);
        let _ = self.run_one(&request, &CancellationToken::new()).await;
        self.journal.flush();
        self.pending.store(false, Ordering::SeqCst);
    }

    async fn run_one(
        &self,
        request: &ExplicitBackfillRequest,
        stopping: &CancellationToken,
    ) -> Result<(), Stopped> {
        let _job = match request.mode {
            ExplicitBackfillMode::Preview => None,
            _ => match self.write_gate.begin_job(Self::JOB_NAME).await {
                Ok(job) => Some(job),
                Err(holder) => {
                    let busy = LibraryWriteGate::busy_message(&holder);
                    warn!("Explicit marking did not start: {busy}");
                    self.store.update(|run| run.reason = Some(busy));
                    return Ok(());
                }
            },
        };
        let outcome = match request.mode {
            ExplicitBackfillMode::Preview => self.preview(stopping).await,
            ExplicitBackfillMode::Apply => self.apply(&request.username, stopping).await,
            ExplicitBackfillMode::Undo => self.undo(stopping).await,
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(Failure::Stopped) => Err(Stopped),
            Err(Failure::Error(failure)) => {
                error!("Explicit marking failed: {failure:#}");
                self.store.update(|run| {
                    run.status = ExplicitBackfillStatus::Failed;
                    run.reason = Some(failure.to_string());
                    run.finished_utc = Some(Utc::now());
                });
                Ok(())
            }
        }
    }

    // ---- Preview ---------------------------------------------------------------------------

    async fn preview(&self, stopping: &CancellationToken) -> Result<(), Failure> {
        let current = self.store.current();
        if current.can_resume() {
            self.store.update(|run| {
                run.status = ExplicitBackfillStatus::Running;
                run.reason = None;
            });
            info!(
                "Explicit marking preview resuming at {}/{}",
                current.cursor,
                current.queue.len()
            );
        } else {
            let listed = self.shared_only(self.songs(stopping).await?);
            let queue: Vec<ExplicitBackfillSong> = listed
                .iter()
                .filter(|(_, status)| status.as_deref().is_none_or(str::is_empty))
                .map(|(song, _)| song.clone())
                .collect();
            let marked = listed.len() - queue.len();
            info!(
                "Explicit marking preview started: {} song(s) to look up, {marked} already marked",
                queue.len()
            );
            self.store.replace(ExplicitBackfillRun {
                run_id: new_run_id(),
                status: ExplicitBackfillStatus::Running,
                mode: ExplicitBackfillMode::Preview,
                started_utc: Some(Utc::now()),
                total: count(queue.len()),
                already_marked: count(marked),
                queue,
                applied_run_id: current.applied_run_id,
                ..ExplicitBackfillRun::default()
            });
        }

        let songs = self.store.read(|run| run.queue.clone());
        let start = usize::try_from(self.store.read(|run| run.cursor)).unwrap_or(0);
        for index in start..songs.len() {
            if self.cancelled() {
                self.store.update(|run| {
                    run.status = ExplicitBackfillStatus::Cancelled;
                    run.finished_utc = Some(Utc::now());
                    run.reason = Some("Stopped from the dashboard.".to_string());
                });
                return Ok(());
            }
            let song = &songs[index];
            let row = self.look_up(song, stopping).await?;
            self.store.update(|run| {
                run.cursor = count(index + 1);
                run.processed += 1;
                run.last_song = Some(match row.as_ref().and_then(|row| row.title.as_ref()) {
                    Some(title) => format!(
                        "{title} by {}",
                        row.as_ref().and_then(|row| row.artist.as_deref()).unwrap_or("")
                    ),
                    None => song.path.clone(),
                });
                let Some(row) = row else {
                    run.already_marked += 1;
                    return;
                };
                match row.outcome.as_str() {
                    "explicit" => run.explicit += 1,
                    "clean" => run.clean += 1,
                    "notExplicit" => run.not_explicit += 1,
                    "failed" => {
                        run.failed += 1;
                        run.errors.push(format!("{}: {}", song.path, row.how));
                        return;
                    }
                    _ => run.unsure += 1,
                }
                run.rows.push(row);
            });
            if !self.pace.is_zero() && index + 1 < songs.len() {
                sleep(self.pace, stopping).await?;
            }
        }

        self.store.update(|run| {
            run.status = ExplicitBackfillStatus::Completed;
            run.finished_utc = Some(Utc::now());
            run.queue = Vec::new();
        });
        let done = self.store.current();
        info!(
            "Explicit marking preview finished: {} explicit, {} clean, {} not explicit, {} unsure, {} already marked, {} unreadable",
            done.explicit, done.clean, done.not_explicit, done.unsure, done.already_marked, done.failed
        );
        Ok(())
    }

    /// One song's row, or None when its file already carries an advisory.
    async fn look_up(
        &self,
        song: &ExplicitBackfillSong,
        stopping: &CancellationToken,
    ) -> Result<Option<ExplicitBackfillRow>, Failure> {
        let mut row = ExplicitBackfillRow {
            path: song.path.clone(),
            navidrome_id: song.navidrome_id.clone(),
            ..ExplicitBackfillRow::default()
        };
        let path = song.path.clone();
        let read = tokio::task::spawn_blocking(move || read_facts(&path))
            .await
            .map_err(|panic| Failure::Error(anyhow::anyhow!(panic.to_string())))?;
        let facts = match read {
            Ok(None) => return Ok(None),
            Ok(Some((facts, size, ticks))) => {
                row.artist = facts.artist.clone();
                row.title = facts.title.clone();
                row.size = size;
                row.write_ticks = ticks;
                facts
            }
            Err(why) => {
                row.outcome = "failed".to_string();
                row.how = why;
                return Ok(Some(row));
            }
        };

        let mut finding = ExplicitFinding::busy();
        for attempt in 0..Self::BUSY_TRIES {
            finding = tokio::select! {
                finding = explicit_lookup::find(&facts, self.catalog.as_ref()) => finding,
                () = stopping.cancelled() => return Err(Failure::Stopped),
            };
            if !finding.catalog_busy {
                break;
            }
            if attempt + 1 < Self::BUSY_TRIES {
                sleep(self.busy_wait, stopping).await?;
            }
        }
        row.outcome = match finding.outcome {
            ExplicitOutcome::Explicit => "explicit",
            ExplicitOutcome::Clean => "clean",
            ExplicitOutcome::NotExplicit => "notExplicit",
            ExplicitOutcome::Unsure => "unsure",
        }
        .to_string();
        row.how = finding.how;
        row.catalog_track_id = finding.catalog_track_id;
        Ok(Some(row))
    }

    async fn songs(
        &self,
        stopping: &CancellationToken,
    ) -> Result<Vec<(ExplicitBackfillSong, Option<String>)>, Failure> {
        if let Some(list) = &self.list_songs {
            return Ok(list().await);
        }
        let root = self.music_path();
        let base_url = self.settings.current().subsonic.url.clone();
        let listed = tokio::select! {
            listed = navidrome_song_list::list(
                self.services.identity.as_deref(),
                self.services.http.as_ref(),
                base_url.as_deref(),
                &root,
            ) => listed,
            () = stopping.cancelled() => return Err(Failure::Stopped),
        };
        // The trash is not part of the library: a trash folder with no leading dot is one
        // Navidrome lists.
        let trash = library_walk::trash_folder(Path::new(&root), &self.settings.current().library_actions);
        if !listed.is_empty() {
            return Ok(from_navidrome(listed, &trash));
        }

        // Navidrome could not be asked: every music file under the folder, each opened to see.
        info!("Explicit marking could not list Navidrome's songs; walking {root} instead");
        if !Path::new(&root).is_dir() {
            return Ok(Vec::new());
        }
        let walked =
            tokio::task::spawn_blocking(move || library_walk::all_files(Path::new(&root), Some(&trash)))
                .await
                .map_err(|panic| Failure::Error(anyhow::anyhow!(panic.to_string())))?
                .map_err(|failure| Failure::Error(failure.into()))?;
        const KINDS: [&str; 5] = [".flac", ".mp3", ".m4a", ".ogg", ".opus"];
        let mut paths: Vec<String> = walked
            .into_iter()
            .filter(|path| KINDS.iter().any(|kind| eq_ignore_case(kind, &extension_of(path))))
            .filter(|path| !path.contains("/.octo-"))
            .collect();
        paths.sort_by(|a, b| compare_ordinal(a, b));
        Ok(paths
            .into_iter()
            .map(|path| {
                (
                    ExplicitBackfillSong {
                        path,
                        navidrome_id: None,
                    },
                    None,
                )
            })
            .collect())
    }

    // ---- Apply -----------------------------------------------------------------------------

    async fn apply(&self, username: &str, stopping: &CancellationToken) -> Result<(), Failure> {
        let preview = self.store.current();
        if !preview.can_apply() {
            self.store
                .update(|run| run.reason = Some("Nothing to write: preview first.".to_string()));
            return Ok(());
        }
        let apply_id = new_run_id();
        let rows: Vec<ExplicitBackfillRow> = preview
            .rows
            .iter()
            .filter(|row| !row.written && writes(&row.outcome))
            .cloned()
            .collect();
        self.store.update(|run| {
            run.status = ExplicitBackfillStatus::Running;
            run.mode = ExplicitBackfillMode::Apply;
            run.started_utc = Some(Utc::now());
            run.finished_utc = None;
            run.step_total = count(rows.len());
            run.step_done = 0;
            run.written = 0;
            run.left_alone = 0;
            run.reason = None;
            run.applied_run_id = Some(apply_id.clone());
        });
        info!("Explicit marking: writing {} file(s)", rows.len());

        let mut since = 0;
        for row in &rows {
            if self.cancelled() || stopping.is_cancelled() {
                break;
            }
            let (written_row, user, run_id) = (row.clone(), username.to_string(), apply_id.clone());
            let journal = self.journal.clone();
            let outcome =
                tokio::task::spawn_blocking(move || write_one(&written_row, &user, &run_id, &journal))
                    .await
                    .map_err(|panic| Failure::Error(anyhow::anyhow!(panic.to_string())))?;
            self.store.update(|run| {
                run.step_done += 1;
                run.last_song = Some(format!(
                    "{} by {}",
                    row.title.as_deref().unwrap_or(""),
                    row.artist.as_deref().unwrap_or("")
                ));
                match &outcome {
                    None => {
                        run.written += 1;
                        if let Some(kept) = run.rows.iter_mut().find(|kept| kept.path == row.path) {
                            kept.written = true;
                        }
                    }
                    Some(why) if why.is_empty() => run.left_alone += 1,
                    Some(why) => {
                        run.failed += 1;
                        run.errors.push(format!("{}: {why}", row.path));
                    }
                }
            });
            since += 1;
            if since >= Self::SAVE_EVERY {
                since = 0;
                self.journal.flush();
                self.store.flush();
                tokio::task::yield_now().await;
            }
        }
        self.journal.flush();

        let stopped = self.cancelled() || stopping.is_cancelled();
        self.store.update(|run| {
            // Back to a preview, so what is left can still be written and the rows stay.
            run.mode = ExplicitBackfillMode::Preview;
            run.status = ExplicitBackfillStatus::Completed;
            run.finished_utc = Some(Utc::now());
            let mut reason = format!(
                "{}Marked {} song(s)",
                if stopped { "Stopped. " } else { "" },
                run.written
            );
            if run.left_alone > 0 {
                reason += &format!(
                    "; {} changed since the preview and were left alone",
                    run.left_alone
                );
            }
            if run.failed > 0 {
                reason += &format!("; {} could not be written", run.failed);
            }
            run.reason = Some(reason + ".");
        });
        if self.store.read(|run| run.written) > 0 {
            self.rescan().await;
        }
        Ok(())
    }

    // ---- Undo ------------------------------------------------------------------------------

    async fn undo(&self, stopping: &CancellationToken) -> Result<(), Failure> {
        let Some(applied) = self.store.read(|run| run.applied_run_id.clone()) else {
            return Ok(());
        };
        let entries = self.journal.of_run(&applied);
        self.store.update(|run| {
            run.status = ExplicitBackfillStatus::Running;
            run.mode = ExplicitBackfillMode::Undo;
            run.started_utc = Some(Utc::now());
            run.finished_utc = None;
            run.step_total = count(entries.len());
            run.step_done = 0;
            run.written = 0;
            run.left_alone = 0;
            run.reason = None;
        });

        let mut since = 0;
        for entry in &entries {
            if self.cancelled() || stopping.is_cancelled() {
                break;
            }
            let (target, journal) = (entry.clone(), self.journal.clone());
            let (left_alone, problem, now) = tokio::task::spawn_blocking(move || undo_one(&target, &journal))
                .await
                .map_err(|panic| Failure::Error(anyhow::anyhow!(panic.to_string())))?;
            self.store.update(|run| {
                run.step_done += 1;
                run.last_song = Some(entry.path.clone());
                if left_alone {
                    run.left_alone += 1;
                } else if let Some(problem) = &problem {
                    run.failed += 1;
                    run.errors.push(format!("{}: {problem}", entry.path));
                } else {
                    run.written += 1;
                }
                if !left_alone
                    && problem.is_none()
                    && let Some(row) = run.rows.iter_mut().find(|row| row.path == entry.path)
                {
                    row.written = false;
                    (row.size, row.write_ticks) = now;
                }
            });
            since += 1;
            if since >= Self::SAVE_EVERY {
                since = 0;
                self.journal.flush();
                tokio::task::yield_now().await;
            }
        }
        self.journal.flush();

        self.store.update(|run| {
            run.mode = ExplicitBackfillMode::Preview;
            run.status = ExplicitBackfillStatus::Completed;
            run.finished_utc = Some(Utc::now());
            let mut reason = format!("Took the mark off {} song(s)", run.written);
            if run.left_alone > 0 {
                reason += &format!(
                    "; {} changed since or missing, so left as they are (they stay undoable)",
                    run.left_alone
                );
            }
            run.reason = Some(reason + ".");
        });
        if self.store.read(|run| run.written) > 0 {
            self.rescan().await;
        }
        Ok(())
    }

    /// One normal scan (not a full one), forced past the debounce so it is not lost.
    async fn rescan(&self) {
        match &self.services.library {
            Some(library) => {
                library.trigger_library_scan(true).await;
            }
            None => warn!(
                "Explicit marking could not ask Navidrome to scan: No service for type \
                 'Octo.Services.Local.ILocalLibraryService' has been registered."
            ),
        }
    }
}

/// Why a step ended early.
enum Failure {
    /// The host is stopping.
    Stopped,
    Error(anyhow::Error),
}

/// Waits `delay`, or stops early when the host stops.
async fn sleep(delay: Duration, stopping: &CancellationToken) -> Result<(), Failure> {
    tokio::select! {
        () = tokio::time::sleep(delay) => Ok(()),
        () = stopping.cancelled() => Err(Failure::Stopped),
    }
}

/// Navidrome's songs as the run looks at them: one per song id (or per file, without one), the
/// first of its entries whose file is there, in path order, with Navidrome's explicit status.
fn from_navidrome(
    listed: HashMap<String, NavidromeSongEntry>,
    trash: &Path,
) -> Vec<(ExplicitBackfillSong, Option<String>)> {
    let mut entries: Vec<NavidromeSongEntry> = listed.into_values().collect();
    // GroupBy keeps the order the entries came in; the map has none, so path order stands in
    // (the groups are sorted by path below either way).
    entries.sort_by(|a, b| compare_ordinal(&a.full_path, &b.full_path));
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Option<NavidromeSongEntry>> = Vec::new();
    for entry in entries {
        let key = entry.id.clone().unwrap_or_else(|| entry.full_path.clone());
        let exists = Path::new(&entry.full_path).is_file();
        match seen.get(&key) {
            None => {
                seen.insert(key, groups.len());
                groups.push(exists.then_some(entry));
            }
            Some(&at) if groups[at].is_none() && exists => groups[at] = Some(entry),
            Some(_) => {}
        }
    }
    let mut chosen: Vec<NavidromeSongEntry> = groups
        .into_iter()
        .flatten()
        .filter(|entry| !library_walk::is_trash(Path::new(&entry.full_path), Some(trash)))
        .collect();
    chosen.sort_by(|a, b| compare_ordinal(&a.full_path, &b.full_path));
    chosen
        .into_iter()
        .map(|entry| {
            (
                ExplicitBackfillSong {
                    path: entry.full_path,
                    navidrome_id: entry.id,
                },
                entry.explicit_status,
            )
        })
        .collect()
}

/// What a file says about itself, its size and its last write: None when it already carries an
/// advisory; the reason in words when it cannot be read.
#[allow(clippy::type_complexity)]
fn read_facts(path: &str) -> Result<Option<(LibrarySongFacts, i64, i64)>, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let file = TagFile::open(path).map_err(|error| match error {
        TagError::Unsupported(_) | TagError::Read(_) => "the file could not be read as music".to_string(),
        other => other.to_string(),
    })?;
    if tag_writer_extras::read_advisory(&file).is_some() {
        return Ok(None);
    }
    let isrc = tag_writer_extras::read_text(&file, TagFields::ISRC).or_else(|| file.isrc());
    let performers = file.performers().join("; ");
    let seconds = file.duration().as_secs_f64();
    let facts = LibrarySongFacts {
        artist: (!performers.is_empty()).then_some(performers),
        title: file.title(),
        album: file.album(),
        seconds: (seconds > 0.0).then_some(seconds),
        isrcs: isrc
            .filter(|isrc| !is_blank(isrc))
            .map(|isrc| {
                isrc.split([';', '/', ','])
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        file_name: Some(file_stem(path)),
    };
    let ticks = last_write_ticks(&metadata);
    Ok(Some((
        facts,
        i64::try_from(metadata.len()).unwrap_or(i64::MAX),
        ticks,
    )))
}

/// None when written; "" when the file changed since the preview (left alone); otherwise why it
/// could not be written.
fn write_one(
    row: &ExplicitBackfillRow,
    username: &str,
    run_id: &str,
    journal: &TagEditJournal,
) -> Option<String> {
    let Some((size, ticks)) = file_state(&row.path) else {
        return Some(String::new());
    };
    if size != row.size || ticks != row.write_ticks {
        return Some(String::new());
    }
    let advisory = if row.outcome == "clean" {
        explicit_advisory::CLEAN
    } else {
        explicit_advisory::EXPLICIT
    };
    let result = library_tag_edits::set_advisory(Path::new(&row.path), Some(advisory));
    if let Some(error) = result.error {
        return Some(error);
    }
    if !result.changed {
        return Some(String::new());
    }
    let (size_after, write_ticks_after) = file_state(&row.path).unwrap_or_default();
    journal.record(
        TagEditEntry {
            id: uuid::Uuid::new_v4().simple().to_string(),
            navidrome_id: row.navidrome_id.clone().unwrap_or_default(),
            path: row.path.clone(),
            kind: tag_edit_kinds::ADVISORY.to_string(),
            username: username.to_string(),
            before: result.before,
            after: result.after,
            size_after,
            write_ticks_after,
            at_utc: Utc::now(),
            run_id: Some(run_id.to_string()),
            ..TagEditEntry::default()
        },
        false,
    );
    None
}

/// Takes one journaled mark out: whether the file was left alone (changed since or missing),
/// what went wrong, and the file's size and last write after.
fn undo_one(entry: &TagEditEntry, journal: &TagEditJournal) -> (bool, Option<String>, (i64, i64)) {
    let left_alone = file_state(&entry.path) != Some((entry.size_after, entry.write_ticks_after));
    if left_alone {
        return (true, None, (0, 0));
    }
    let before = entry
        .before
        .get(tag_edit_kinds::ADVISORY)
        .cloned()
        .flatten()
        .and_then(|was| was.trim().parse::<i32>().ok());
    let result = library_tag_edits::set_advisory(Path::new(&entry.path), before);
    if result.error.is_none() {
        journal.mark_undone(&entry.id, false);
    }
    (false, result.error, file_state(&entry.path).unwrap_or_default())
}

/// The file's length and last write time in ticks, or None when the file is not there.
fn file_state(path: &str) -> Option<(i64, i64)> {
    let metadata = std::fs::metadata(path).ok().filter(std::fs::Metadata::is_file)?;
    Some((
        i64::try_from(metadata.len()).unwrap_or(i64::MAX),
        last_write_ticks(&metadata),
    ))
}

fn last_write_ticks(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .map(|modified| dotnet_ticks(chrono::DateTime::<Utc>::from(modified)))
        .unwrap_or_default()
}

/// The file name up to its last dot.
fn file_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) => name[..dot].to_string(),
        None => name.to_string(),
    }
}

/// The extension: from the file name's last dot, or nothing.
fn extension_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot..].to_string(),
        _ => String::new(),
    }
}

/// Ordinal comparison: UTF-16 code unit order.
fn compare_ordinal(a: &str, b: &str) -> CmpOrdering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// The first 12 hex digits of a random UUID.
fn new_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// A list's length as the run's `i32` counters hold it, capped at `i32::MAX`.
fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[path = "explicit_backfill_tests.rs"]
mod tests;
