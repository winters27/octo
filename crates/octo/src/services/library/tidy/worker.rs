//! Runs the library tidy: a preview, an apply, or the resume of a stopped apply, one at a time,
//! for one of its two tasks, Artists or File layout.
//!
//! An Artists apply is two phases with a Navidrome scan between them. Navidrome keeps a song's
//! stars, plays and playlist entries across a move when it can match the moved file by its tags or
//! by its path, but not when both change in one scan. So the tags are rewritten first, then the
//! tidy waits for a scan that STARTED after the last write and finished, and only then moves
//! album folders, each only while every song in it is exactly as that scan saw it. Octo's own
//! scan triggers are held back meanwhile, and the music folder's one-writer gate is held from
//! the first write to the last move.
//!
//! A File layout apply writes no tags: it renames folders, then moves files one folder at a time,
//! each only while the file is as Navidrome's last scan saw it.
//!
//! Every change is journaled and synced before it is made; a journal that cannot be written
//! stops the run.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use octo_core::common::layout::is_audio_name;
use octo_core::common::path_helper::{PATH_BUDGET, POOL_BUDGET, fold_name};
use octo_core::common::{Layout, LayoutValues, PathHelper};
use octo_core::settings::SettingsStore;
use octo_media::tags::TagFile;
use octo_media::tags::tag_writer_extras::{self as extras, TagFields};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::journal::{self, TidyJournal, TidyJournalEntry};
use super::layout_pass::{self, LayoutDisk, LayoutFile, LayoutMapping, LayoutOptions};
use super::planner::{
    self, TidyAnswers, TidyCopy, TidyFile, TidyFileMove, TidyKind, TidyMove, TidyOptions, TidyRetag, TidyRow,
    TidyTags, counted,
};
use super::store::{
    TidyLayout, TidyLidarr, TidyMode, TidyPlanFile, TidyRun, TidyScope, TidyStatus, TidyStore, TidyTask,
    TidyWork,
};
use crate::services::common::album_identity::read_layout_tags;
use crate::services::common::base_download_service::{BaseDownloadService, PLAYLISTS};
use crate::services::cover_art::CoverUpgradeJournal;
use crate::services::family::members_place;
use crate::services::family::pool::{FamilyPool, PoolFilesHeld, RelayoutOutcome};
use crate::services::fingerprint::music_brainz_client::MusicBrainzClient;
use crate::services::library::explicit_backfill::ExplicitBackfillStore;
use crate::services::library::library_action_journal::LibraryActionJournal;
use crate::services::library::library_action_quarantine::LibraryActionQuarantine;
use crate::services::library::library_tag_edits::TagEditJournal;
use crate::services::library::library_walk;
use crate::services::library::library_write_gate::{LibraryJob, LibraryWriteGate};
use crate::services::library::navidrome_song_list::NavidromeSongEntry;
use crate::services::library::notice_queue::NoticeQueue;
use crate::services::lidarr::LidarrClient;
use crate::services::lidarr::lidarr_pending_albums::LidarrPendingAlbums;
use crate::services::local::scan_wait::{self, ScanWaitError, ScanWaitTiming};
use crate::services::local::{DownloadHistoryService, ILocalLibraryService, LocalSongMapping};
use crate::services::lyrics::{LyricsPathRemap, LyricsUndoJournal};
use crate::services::metadata::genre_backfill_journal::GenreBackfillJournal;
use crate::services::state_file;
use crate::services::subsonic::navidrome_identity_service::NavidromeIdentityService;
use crate::services::subsonic::navidrome_version::{self, TidyGate};

/// What the dashboard asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TidyRequest {
    pub mode: TidyMode,
    pub scope: TidyScope,
    pub task: TidyTask,
    /// Carry on with the work of a stopped apply instead of starting from the preview.
    pub resume: bool,
    /// The opt-in rows the person ticked (an apply).
    pub ticked: Vec<String>,
    /// Folders under the music folder to leave out (a preview).
    pub excluded: Vec<String>,
    /// Undo every run the journal holds, not only the last.
    pub all_runs: bool,
    /// The run the person saw Undo would put back. An Undo of the last run is refused when the
    /// last run is another one by now (an apply from another page meanwhile).
    pub run: Option<String>,
}

/// Navidrome's song list by full path, for the check after the moves.
pub type ListSongs = Arc<dyn Fn() -> BoxFuture<'static, HashMap<String, NavidromeSongEntry>> + Send + Sync>;

/// What the tidy reaches besides its store and journal. A service left out is one the run copes
/// without; the library is needed for an apply.
#[derive(Clone, Default)]
pub struct TidyServices {
    pub library: Option<Arc<dyn ILocalLibraryService>>,
    pub music_brainz: Option<Arc<MusicBrainzClient>>,
    pub notices: Option<Arc<NoticeQueue>>,
    pub genre_journal: Option<Arc<GenreBackfillJournal>>,
    pub tag_edits: Option<Arc<TagEditJournal>>,
    pub actions: Option<Arc<LibraryActionJournal>>,
    pub history: Option<Arc<DownloadHistoryService>>,
    pub list_songs: Option<ListSongs>,
    pub cover_journal: Option<Arc<CoverUpgradeJournal>>,
    pub lyrics_undo: Option<Arc<LyricsUndoJournal>>,
    pub explicit: Option<Arc<ExplicitBackfillStore>>,
    pub quarantine: Option<Arc<LibraryActionQuarantine>>,
    /// Lidarr, asked which files it keeps before the File layout task moves any.
    pub lidarr: Option<Arc<LidarrClient>>,
    /// The open follows of Octo's Lidarr searches, which hold files back from the library.
    pub lidarr_pending: Option<Arc<LidarrPendingAlbums>>,
    /// The music folder Navidrome reads, which the settings' download path may only stand in for.
    pub identity: Option<Arc<NavidromeIdentityService>>,
    /// The family pool: its files are filed by the layout too, each with every member's link,
    /// and its records of shared files follow the library's moves.
    pub family_pool: Option<Arc<FamilyPool>>,
    /// The table the lyrics writer's waiting jobs follow the tidy's moves through.
    pub lyrics_remap: Option<LyricsPathRemap>,
}

/// How one change went.
enum Outcome {
    Done,
    /// Done by an earlier run (a resume finds it so).
    AlreadyDone,
    Held(String),
    Failed(String),
    JournalFailed(String),
}

/// Why a run stopped early.
enum Stop {
    Cancelled,
    ShuttingDown,
}

const AUDIO_EXTENSIONS: [&str; 14] = [
    "flac", "mp3", "m4a", "aac", "alac", "ogg", "opus", "wav", "aiff", "aif", "ape", "wv", "wma", "dsf",
];

const PLAYLIST_EXTENSIONS: [&str; 2] = ["m3u", "m3u8"];

pub struct TidyWorker {
    store: Arc<TidyStore>,
    journal: Arc<TidyJournal>,
    settings: Arc<SettingsStore>,
    services: TidyServices,
    write_gate: LibraryWriteGate,
    timing: ScanWaitTiming,
    max_lookups: AtomicUsize,
    cancel_requested: AtomicBool,
    pending: AtomicBool,
    gate_seen: Mutex<Option<(tokio::time::Instant, TidyGate)>>,
    sender: mpsc::Sender<TidyRequest>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<TidyRequest>>,
    /// Run in the playlist follow, on its blocking thread, holding the playlists lock (tests).
    #[cfg(test)]
    playlist_walk_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// How long the job lets the gate go before it asks again for family pool files another
    /// change is working on.
    pool_retry: Mutex<Duration>,
}

impl TidyWorker {
    /// What the dashboard calls this job when another one has to wait for it, for both tasks.
    pub const JOB_NAME: &'static str = "The library tidy";

    /// MusicBrainz allows about one request a second. What is not asked in one preview is
    /// reported, and the next preview asks the next batch.
    pub const MAX_LOOKUPS: usize = 60;

    /// Files in a row that could not be written before the run gives up.
    const MAX_CONSECUTIVE_FAILURES: usize = 10;

    pub fn new(
        store: Arc<TidyStore>,
        journal: Arc<TidyJournal>,
        settings: Arc<SettingsStore>,
        services: TidyServices,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(1);
        Self {
            store,
            journal,
            settings,
            services,
            write_gate: LibraryWriteGate::new(),
            timing: ScanWaitTiming::default(),
            max_lookups: AtomicUsize::new(Self::MAX_LOOKUPS),
            cancel_requested: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            gate_seen: Mutex::new(None),
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            #[cfg(test)]
            playlist_walk_hook: Mutex::new(None),
            pool_retry: Mutex::new(Duration::from_secs(5)),
        }
    }

    /// How long the job lets the gate go before it asks again for family pool files another
    /// change is working on (tests wait less).
    pub fn set_pool_retry(&self, wait: Duration) {
        *self.pool_retry.lock() = wait;
    }

    fn pool_retry(&self) -> Duration {
        *self.pool_retry.lock()
    }

    /// The gate shared with the other jobs that write the music folder.
    pub fn with_write_gate(mut self, gate: LibraryWriteGate) -> Self {
        self.write_gate = gate;
        self
    }

    /// How often and how long to wait for Navidrome's scans (tests wait less).
    pub fn with_timing(mut self, timing: ScanWaitTiming) -> Self {
        self.timing = timing;
        self
    }

    /// The MusicBrainz lookups one preview may make (tests ask fewer).
    pub fn set_max_lookups(&self, max: usize) {
        self.max_lookups.store(max, Ordering::SeqCst);
    }

    pub fn current(&self) -> TidyRun {
        self.store.current()
    }

    pub fn store(&self) -> &Arc<TidyStore> {
        &self.store
    }

    pub fn journal(&self) -> &Arc<TidyJournal> {
        &self.journal
    }

    pub fn is_running(&self) -> bool {
        self.store.current().status == TidyStatus::Running
    }

    /// A run accepted and not yet started.
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }

    /// The music folder: the one Navidrome reads when Octo has found it, else as the settings name
    /// it, where downloads go too.
    pub fn music_root(&self) -> String {
        let configured = self
            .settings
            .raw("Library:DownloadPath")
            .unwrap_or_else(|| "./downloads".to_string());
        let root = match &self.services.identity {
            Some(identity) => identity.effective_download_path(&configured),
            None => configured,
        };
        let trimmed = root.trim_end_matches('/');
        if trimmed.is_empty() {
            root
        } else {
            trimmed.to_string()
        }
    }

    /// False when a run is going or waiting to start.
    pub fn try_enqueue(&self, request: TidyRequest) -> bool {
        if self.is_running()
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

    /// Whether an apply may run against the Navidrome in front of Octo. Remembered a minute,
    /// so a polling dashboard does not ping on every refresh.
    pub async fn check_navidrome(&self, fresh: bool) -> TidyGate {
        if !fresh
            && let Some((when, gate)) = self.gate_seen.lock().clone()
            && when.elapsed() < Duration::from_secs(60)
        {
            return gate;
        }
        let info = match &self.services.library {
            Some(library) => library.get_server_info().await,
            None => None,
        };
        let gate = navidrome_version::gate(info.as_ref());
        *self.gate_seen.lock() = Some((tokio::time::Instant::now(), gate.clone()));
        gate
    }

    /// The hosted loop: one request at a time until `stopping`.
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
            self.run_once(request, &stopping).await;
        }
        Ok(())
    }

    /// One request, start to finish.
    pub async fn run_once(&self, request: TidyRequest, stopping: &CancellationToken) {
        self.cancel_requested.store(false, Ordering::SeqCst);
        match (request.mode, request.task) {
            (TidyMode::Preview, TidyTask::Artists) => self.preview(&request, stopping).await,
            (TidyMode::Preview, TidyTask::FileLayout) => self.preview_layout(&request, stopping).await,
            (TidyMode::Apply, TidyTask::Artists) => self.apply(&request, stopping).await,
            (TidyMode::Apply, TidyTask::FileLayout) => self.apply_layout(&request, stopping).await,
            (TidyMode::Undo, _) => self.undo(&request, stopping).await,
        }
        if self.store.current().status == TidyStatus::Running {
            self.finish_with(
                TidyStatus::Failed,
                "The run ended without saying how.".to_string(),
            );
        }
        self.pending.store(false, Ordering::SeqCst);
    }

    // ---- preview ----------------------------------------------------------------------------

    async fn preview(&self, request: &TidyRequest, stopping: &CancellationToken) {
        let root = self.music_root();
        self.start(
            TidyMode::Preview,
            request.scope,
            TidyTask::Artists,
            "Reading the library",
        );
        info!("Library tidy preview started: scope {}", request.scope.name());
        let Some(files) = self.read_library(&root, request.scope, stopping).await else {
            return;
        };
        let case_insensitive = case_insensitive(&root);
        self.store
            .update(|run| run.phase = "Asking MusicBrainz".to_string());
        let options = TidyOptions {
            case_insensitive,
            excluded: request.excluded.clone(),
            leave_folders: self.leaves_artist_folders(),
            ..TidyOptions::default()
        };
        let Some((answers, asked, not_asked)) = self.ask(&root, &files, &options, stopping).await else {
            return;
        };
        let shown = planner::plan(&root, &files, &answers, &options);
        let full = planner::plan(
            &root,
            &files,
            &answers,
            &TidyOptions {
                tick_all: true,
                ..options.clone()
            },
        );
        let (retags, moves) = (shown.retags.len(), shown.moves.len());
        let saved = self.store.save_plan(TidyPlanFile {
            root: root.clone(),
            scope: request.scope,
            excluded: request.excluded.clone(),
            case_insensitive,
            plan: shown,
            answers,
            ..TidyPlanFile::default()
        });
        if let Err(failure) = saved {
            self.finish_with(
                TidyStatus::Failed,
                format!("The preview could not be saved: {failure}"),
            );
            return;
        }
        self.store.update(|run| {
            run.planned_retags = retags;
            run.planned_moves = moves;
            run.plan_hash = Some(full.hash());
            run.lookups_asked = asked;
            run.lookups_not_asked = not_asked;
        });
        self.finish_with(TidyStatus::Completed, "Nothing has been written.".to_string());
    }

    /// Plan, ask MusicBrainz what the plan wants to know, and plan again, a few rounds at most:
    /// a folder asks about its whole name first and its parts only after. Answers already known
    /// cost nothing; new questions stop at the budget.
    async fn ask(
        &self,
        root: &str,
        files: &[TidyFile],
        options: &TidyOptions,
        stopping: &CancellationToken,
    ) -> Option<(TidyAnswers, usize, usize)> {
        let mut answers = TidyAnswers::default();
        let mut not_asked: HashSet<String> = HashSet::new();
        let mut asked = 0;
        let budget = self.max_lookups.load(Ordering::SeqCst);
        for _round in 0..5 {
            let plan = planner::plan(root, files, &answers, options);
            let artists: Vec<String> = plan
                .wanted_artists
                .into_iter()
                .filter(|name| {
                    let key = planner::artist_lookup_key(name);
                    !answers.artists.contains_key(&key) && !not_asked.contains(&key)
                })
                .collect();
            let recordings: Vec<String> = plan
                .wanted_recordings
                .into_iter()
                .filter(|id| !answers.credits.contains_key(id) && !not_asked.contains(id))
                .collect();
            if artists.is_empty() && recordings.is_empty() {
                break;
            }
            let Some(client) = &self.services.music_brainz else {
                not_asked.extend(artists.iter().map(|name| planner::artist_lookup_key(name)));
                not_asked.extend(recordings);
                continue;
            };
            for name in artists {
                if let Some(stop) = self.should_stop(stopping) {
                    self.stopped(Some(stop));
                    return None;
                }
                let key = planner::artist_lookup_key(&name);
                let known = client.known_artists_named(&name);
                let answer = match known {
                    Some(known) => Some(known),
                    None if asked < budget => {
                        asked += 1;
                        client.find_artists_named(&name).await.ok().flatten()
                    }
                    None => None,
                };
                match answer {
                    Some(found) => {
                        answers
                            .artists
                            .insert(key, found.into_iter().map(|artist| artist.name).collect());
                    }
                    None => {
                        not_asked.insert(key);
                    }
                }
            }
            let mut unknown: Vec<String> = Vec::new();
            for id in recordings {
                match client.known_credits(&id) {
                    Some(credit) => {
                        answers.credits.insert(id, credit);
                    }
                    None => unknown.push(id),
                }
            }
            for batch in unknown.chunks(100) {
                if asked >= budget {
                    not_asked.extend(batch.iter().cloned());
                    continue;
                }
                asked += 1;
                match client.recording_credits(batch).await.ok().flatten() {
                    Some(found) => answers.credits.extend(found),
                    None => not_asked.extend(batch.iter().cloned()),
                }
            }
            self.store.update(|run| run.lookups_asked = asked);
        }
        Some((answers, asked, not_asked.len()))
    }

    // ---- apply ------------------------------------------------------------------------------

    async fn apply(&self, request: &TidyRequest, stopping: &CancellationToken) {
        let previous = self.store.current();
        let plan_file = self.store.plan();
        self.start(
            TidyMode::Apply,
            request.scope,
            TidyTask::Artists,
            "Checking Navidrome",
        );
        let refuse = |why: String| self.finish_with(TidyStatus::Failed, why);

        let Some(plan_file) = plan_file else {
            refuse(PREVIEW_FIRST.to_string());
            return;
        };
        if plan_file.task != TidyTask::Artists || previous.task != TidyTask::Artists {
            refuse(PREVIEW_FIRST.to_string());
            return;
        }
        if request.resume {
            if !previous.can_resume() || plan_file.work.is_none() {
                refuse("There is no stopped tidy to resume.".to_string());
                return;
            }
        } else if !previous.is_finished_preview() || plan_file.scope != request.scope {
            refuse(PREVIEW_FIRST.to_string());
            return;
        }
        let gate = self.check_navidrome(true).await;
        if !gate.apply_allowed {
            refuse(gate.reason.unwrap_or_default());
            return;
        }
        let Some(library) = self.services.library.clone() else {
            refuse("Octo has no library service to ask Navidrome for scans.".to_string());
            return;
        };
        let mut job = match self.write_gate.begin_job(Self::JOB_NAME).await {
            Ok(job) => job,
            Err(holder) => {
                refuse(LibraryWriteGate::busy_message(&holder));
                return;
            }
        };

        let root = plan_file.root.clone();
        let work = if request.resume {
            plan_file.work.clone().unwrap_or_default()
        } else {
            self.store
                .update(|run| run.phase = "Reading the library".to_string());
            let Some(files) = self.read_library(&root, plan_file.scope, stopping).await else {
                return;
            };
            let options = TidyOptions {
                case_insensitive: plan_file.case_insensitive,
                excluded: plan_file.excluded.clone(),
                leave_folders: self.leaves_artist_folders(),
                ..TidyOptions::default()
            };
            let full = planner::plan(
                &root,
                &files,
                &plan_file.answers,
                &TidyOptions {
                    tick_all: true,
                    ..options.clone()
                },
            );
            if Some(full.hash()) != previous.plan_hash {
                refuse(
                    "The library changed since the preview, so Apply stopped before touching anything. Preview again."
                        .to_string(),
                );
                return;
            }
            let opt_in: HashSet<&String> = full
                .rows
                .iter()
                .filter(|row| row.opt_in)
                .map(|row| &row.id)
                .collect();
            let ticked: Vec<String> = request
                .ticked
                .iter()
                .filter(|id| opt_in.contains(id))
                .cloned()
                .collect();
            let chosen = planner::plan(
                &root,
                &files,
                &plan_file.answers,
                &TidyOptions {
                    ticked: ticked.iter().cloned().collect(),
                    ..options
                },
            );
            let work = TidyWork {
                run_id: self.store.current().run_id,
                ticked,
                retags: chosen.retags,
                moves: chosen.moves,
                ..TidyWork::default()
            };
            let mut kept = plan_file.clone();
            kept.work = Some(work.clone());
            if let Err(failure) = self.store.save_plan(kept) {
                refuse(format!(
                    "The work could not be saved, so nothing was changed: {failure}"
                ));
                return;
            }
            work
        };
        self.store.update(|run| {
            run.planned_retags = work.retags.len();
            run.planned_moves = work.moves.len();
            run.plan_hash = previous.plan_hash.clone();
        });
        info!(
            "Library tidy apply started: {} file(s) to re-tag, {} album folder(s) to move",
            work.retags.len(),
            work.moves.len()
        );
        self.execute(&root, &work, library, stopping, &mut job).await;
    }

    async fn execute(
        &self,
        root: &str,
        work: &TidyWork,
        library: Arc<dyn ILocalLibraryService>,
        stopping: &CancellationToken,
        job: &mut LibraryJob,
    ) {
        let hold = library.scan_hold().map(|hold| hold.hold());
        let before = self.list_songs().await;
        let stopped = self
            .write_and_move(root, work, library.as_ref(), stopping, job)
            .await;
        // Nothing is written from here on.
        job.pause();

        let wanted = hold.map(|hold| hold.release()).unwrap_or(false);
        if self.store.current().changed_anything() || wanted {
            library.start_scan().await;
        }
        if let Some(stop) = stopped {
            self.stopped(Some(stop));
            return;
        }
        if self.store.current().status != TidyStatus::Running {
            return;
        }
        if self.store.current().moved > 0 {
            let moved: Vec<(String, String)> = work
                .moves
                .iter()
                .map(|movement| (movement.from.clone(), movement.to.clone()))
                .collect();
            let files: Vec<String> = work
                .moves
                .iter()
                .flat_map(|movement| movement.files.clone())
                .collect();
            self.check_what_navidrome_sees(&files, &moved, before, library.as_ref(), stopping)
                .await;
        }
        let run = self.store.current();
        self.complete_with_summary(
            format!(
                "Re-tagged {} and moved {}; {} held back.",
                counted(run.retagged, "file", "files"),
                counted(run.moved, "album folder", "album folders"),
                run.held
            ),
            None,
        );
    }

    /// The tags, the scan between, the moves and the playlists. Some when the run was stopped.
    async fn write_and_move(
        &self,
        root: &str,
        work: &TidyWork,
        library: &dyn ILocalLibraryService,
        stopping: &CancellationToken,
        job: &mut LibraryJob,
    ) -> Option<Stop> {
        self.store
            .update(|run| run.phase = "Waiting for Navidrome to finish scanning".to_string());
        // Downloads may land while Navidrome scans; the tidy measures every file before it
        // touches it.
        job.pause();
        let idle = scan_wait::wait_while_scanning(library, self.timing, stopping).await;
        job.resume().await;
        if !idle {
            if let Some(stop) = self.should_stop(stopping) {
                return Some(stop);
            }
            self.finish_with(
                TidyStatus::Failed,
                "Navidrome kept scanning, or did not answer, so nothing was changed. Apply again later."
                    .to_string(),
            );
            return None;
        }

        // ---- phase 1: tags ----
        self.store.update(|run| run.phase = "Writing tags".to_string());
        let mut last_write: Option<DateTime<Utc>> = None;
        let mut stamps: HashMap<String, (u64, DateTime<Utc>)> = self.stamps_of(&work.run_id);
        let mut in_a_row = 0;
        for retag in &work.retags {
            if let Some(stop) = self.should_stop(stopping) {
                return Some(stop);
            }
            let (journal, retag_for_write, run_id) =
                (self.journal.clone(), retag.clone(), work.run_id.clone());
            let outcome =
                tokio::task::spawn_blocking(move || retag_file(&retag_for_write, &run_id, &journal))
                    .await
                    .unwrap_or_else(|panic| Outcome::Failed(panic.to_string()));
            match outcome {
                Outcome::Done => {
                    in_a_row = 0;
                    last_write = Some(Utc::now());
                    if let Some(stamp) = stamp_of(&retag.path) {
                        stamps.insert(retag.path.clone(), stamp);
                    }
                    self.store.update(|run| run.retagged += 1);
                }
                Outcome::AlreadyDone => in_a_row = 0,
                Outcome::Held(why) => {
                    in_a_row = 0;
                    self.hold(root, &retag.path, why);
                }
                Outcome::Failed(why) => {
                    in_a_row += 1;
                    self.fail(&retag.path, &why);
                    if in_a_row >= Self::MAX_CONSECUTIVE_FAILURES {
                        self.finish_with(
                            TidyStatus::Failed,
                            format!("{in_a_row} files in a row could not be written. Is the music folder read-only?"),
                        );
                        return None;
                    }
                }
                Outcome::JournalFailed(why) => {
                    self.finish_with(TidyStatus::Failed, journal_failed(&why));
                    return None;
                }
            }
        }
        if work.moves.is_empty() {
            return None;
        }

        // ---- the scan between ----
        self.store
            .update(|run| run.phase = "Waiting for Navidrome to scan".to_string());
        // Every write of this work, an earlier run's too when resuming, must be older than the
        // start of the scan the moves are measured against.
        let newest = stamps
            .values()
            .map(|(_, modified)| *modified)
            .chain(last_write)
            .max();
        // A move goes ahead only while every song in the folder is as this scan saw it, so the
        // folder is open to downloads until the scan is done.
        job.pause();
        let seen = library
            .get_scan_status()
            .await
            .and_then(|status| status.last_scan_started_at());
        let scan_start = match newest {
            Some(_)
                if seen
                    .zip(newest)
                    .is_some_and(|(started, written)| started > written) =>
            {
                seen
            }
            Some(written) => {
                if !library.start_scan().await {
                    self.finish_with(
                        TidyStatus::Failed,
                        "Navidrome would not start a scan, so nothing was moved. Check that Octo has Navidrome admin credentials."
                            .to_string(),
                    );
                    return None;
                }
                match scan_wait::wait_for_scan(library, written, self.timing, stopping).await {
                    Ok(_) => library
                        .get_scan_status()
                        .await
                        .and_then(|status| status.last_scan_started_at()),
                    Err(ScanWaitError::Cancelled) => {
                        return self.should_stop(stopping).or(Some(Stop::Cancelled));
                    }
                    Err(ScanWaitError::NoStatus) => {
                        self.finish_with(TidyStatus::Failed, NO_STATUS.to_string());
                        return None;
                    }
                    Err(ScanWaitError::TimedOut) => {
                        self.finish_with(
                            TidyStatus::Failed,
                            format!(
                                "Navidrome did not finish a scan within {} minutes. The tags are written; nothing was moved. Resume once it has scanned.",
                                self.timing.timeout.as_secs() / 60
                            ),
                        );
                        return None;
                    }
                }
            }
            None => seen,
        };
        let Some(scan_start) = scan_start else {
            self.finish_with(TidyStatus::Failed, NO_STATUS.to_string());
            return None;
        };
        job.resume().await;

        // ---- phase 2: album folders ----
        self.store.update(|run| run.phase = "Moving files".to_string());
        let mut moved: Vec<(String, String)> = Vec::new();
        let mut in_a_row = 0;
        for movement in &work.moves {
            if let Some(stop) = self.should_stop(stopping) {
                self.follow_playlists(root, &moved, &work.run_id).await;
                return Some(stop);
            }
            job.pause();
            let idle = scan_wait::wait_while_scanning(library, self.timing, stopping).await;
            job.resume().await;
            if !idle {
                if let Some(stop) = self.should_stop(stopping) {
                    self.follow_playlists(root, &moved, &work.run_id).await;
                    return Some(stop);
                }
                self.hold(
                    root,
                    &movement.from,
                    "Navidrome was scanning or did not answer. Resume once it is idle.".to_string(),
                );
                continue;
            }
            let (journal, movement_for_move, run_id, stamps_for_move) = (
                self.journal.clone(),
                movement.clone(),
                work.run_id.clone(),
                stamps.clone(),
            );
            let outcome = tokio::task::spawn_blocking(move || {
                move_folder(
                    &movement_for_move,
                    scan_start,
                    &stamps_for_move,
                    &run_id,
                    &journal,
                )
            })
            .await
            .unwrap_or_else(|panic| Outcome::Failed(panic.to_string()));
            match outcome {
                Outcome::Done => {
                    in_a_row = 0;
                    self.follow(library, &movement.from, &movement.to).await;
                    let parent = Path::new(&movement.from)
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned());
                    BaseDownloadService::try_remove_empty_parents(parent.as_deref(), root);
                    moved.push((movement.from.clone(), movement.to.clone()));
                    self.store.update(|run| run.moved += 1);
                }
                Outcome::AlreadyDone => {
                    // Done by an earlier run that may have stopped before its records followed.
                    in_a_row = 0;
                    self.follow(library, &movement.from, &movement.to).await;
                    moved.push((movement.from.clone(), movement.to.clone()));
                }
                Outcome::Held(why) => {
                    in_a_row = 0;
                    self.hold(root, &movement.from, why);
                }
                Outcome::Failed(why) => {
                    in_a_row += 1;
                    self.fail(&movement.from, &why);
                    if in_a_row >= Self::MAX_CONSECUTIVE_FAILURES {
                        self.follow_playlists(root, &moved, &work.run_id).await;
                        self.finish_with(
                            TidyStatus::Failed,
                            format!("{in_a_row} folders in a row could not be moved. Is the music folder read-only?"),
                        );
                        return None;
                    }
                }
                Outcome::JournalFailed(why) => {
                    self.follow_playlists(root, &moved, &work.run_id).await;
                    self.finish_with(TidyStatus::Failed, journal_failed(&why));
                    return None;
                }
            }
        }
        self.store
            .update(|run| run.phase = "Updating playlists".to_string());
        self.follow_playlists(root, &moved, &work.run_id).await;
        None
    }

    /// Stamps the journal already holds for this work's run: a resume measures files against
    /// what the earlier run left.
    fn stamps_of(&self, run_id: &str) -> HashMap<String, (u64, DateTime<Utc>)> {
        self.journal
            .read_all()
            .into_iter()
            .filter_map(|entry| match entry {
                TidyJournalEntry::Stamp {
                    path,
                    size,
                    modified,
                    run,
                } if run == run_id => Some((path, (size, modified))),
                _ => None,
            })
            .collect()
    }

    /// Octo's own records of where a file is follow the folder.
    async fn follow(&self, library: &dyn ILocalLibraryService, from: &str, to: &str) {
        self.follow_all(library, &[(from.to_string(), to.to_string())])
            .await;
    }

    /// Octo's own records of where a file is follow every move of `moved`, in order, each store
    /// saved once for them all; and the lyrics jobs waiting are told, while the job still holds
    /// the gate, so none of them writes to a path the tidy just left.
    async fn follow_all(&self, library: &dyn ILocalLibraryService, moved: &[(String, String)]) {
        if moved.is_empty() {
            return;
        }
        if let Some(remap) = &self.services.lyrics_remap {
            remap.record(moved);
        }
        self.follow_records(library, moved).await;
    }

    /// [`Self::follow_all`] for moves the lyrics jobs were already told of (the family pool tells
    /// them of each move as it makes it).
    async fn follow_records(&self, library: &dyn ILocalLibraryService, moved: &[(String, String)]) {
        if moved.is_empty() {
            return;
        }
        let first = &moved[0].0;
        if let Err(failure) = library.update_local_paths(moved).await {
            warn!("Library tidy moved {first} but its download records could not follow: {failure}");
        }
        if let Some(notices) = &self.services.notices {
            notices.move_paths_batch(moved);
        }
        if let Some(journal) = &self.services.genre_journal {
            journal.move_paths_batch(moved);
        }
        if let Some(journal) = &self.services.tag_edits {
            journal.move_paths_batch(moved);
        }
        if let Some(journal) = &self.services.actions {
            journal.move_paths_batch(moved);
        }
        if let Some(history) = &self.services.history {
            history.move_paths_batch(moved);
        }
        if let Some(journal) = &self.services.cover_journal {
            journal.move_paths_batch(moved);
        }
        if let Some(journal) = &self.services.lyrics_undo
            && let Err(failure) = journal.move_paths_batch(moved)
        {
            warn!("Library tidy moved {first} but the lyrics undo could not follow: {failure}");
        }
        if let Some(store) = &self.services.explicit {
            store.move_paths_batch(moved);
        }
        if let Some(quarantine) = &self.services.quarantine {
            quarantine.move_paths_batch(&self.music_root(), moved);
        }
        // A shared file's pool copy names the file it was copied from.
        if let Some(pool) = &self.services.family_pool {
            pool.store().move_paths_batch(moved);
        }
    }

    /// Playlist files under the music folder that name a moved song name it where it is now,
    /// each file's text journaled before it is rewritten: the walk and the writes run on a
    /// blocking thread, holding [`PLAYLISTS`], which a download adding a song to a playlist takes
    /// too.
    async fn follow_playlists(&self, root: &str, moved: &[(String, String)], run_id: &str) {
        if moved.is_empty() {
            return;
        }
        let (store, journal) = (self.store.clone(), self.journal.clone());
        let (root, moved, run_id) = (root.to_string(), moved.to_vec(), run_id.to_string());
        #[cfg(test)]
        let hook = self.playlist_walk_hook.lock().clone();
        let followed = tokio::task::spawn_blocking(move || {
            let _playlists = PLAYLISTS.blocking_lock();
            #[cfg(test)]
            if let Some(hook) = hook {
                hook();
            }
            follow_playlists_in(&store, &journal, &root, &moved, &run_id);
        })
        .await;
        if let Err(failure) = followed {
            warn!("Library tidy could not follow the playlists: {failure}");
        }
    }

    /// After the moves: a scan, then any moved song Navidrome reports missing, or under a new
    /// id, is listed, since its stars and plays may not have followed it. Each of `files` is
    /// looked for where every move of `moved`, in order, took it.
    async fn check_what_navidrome_sees(
        &self,
        files: &[String],
        moved: &[(String, String)],
        before: HashMap<String, NavidromeSongEntry>,
        library: &dyn ILocalLibraryService,
        stopping: &CancellationToken,
    ) {
        if before.is_empty() {
            return;
        }
        self.store
            .update(|run| run.phase = "Checking what Navidrome sees".to_string());
        let after_moves = Utc::now() - chrono::TimeDelta::seconds(1);
        if scan_wait::wait_for_scan(library, after_moves, self.timing, stopping)
            .await
            .is_err()
        {
            self.store.update(|run| {
                run.errors.push(
                    "Navidrome did not finish a scan after the moves, so Octo could not check that every song kept its place."
                        .to_string(),
                )
            });
            return;
        }
        let now = self.list_songs().await;
        let root = self.music_root();
        for file in files {
            let Some(was) = before.get(file) else {
                continue;
            };
            let target = final_path(file, moved);
            if target == *file {
                continue;
            }
            let lost = now.get(file).is_some_and(|entry| entry.missing);
            let new_id = match (now.get(&target).and_then(|e| e.id.as_deref()), was.id.as_deref()) {
                (Some(now_id), Some(was_id)) => now_id != was_id,
                _ => false,
            };
            if lost || new_id {
                self.hold(
                    &root,
                    &target,
                    "Navidrome did not match this moved song to the one it was, so its stars, plays and playlist entries may not have followed. Undo puts it back.".to_string(),
                );
            }
        }
    }

    async fn list_songs(&self) -> HashMap<String, NavidromeSongEntry> {
        match &self.services.list_songs {
            Some(list) => list().await,
            None => HashMap::new(),
        }
    }

    // ---- reading ----------------------------------------------------------------------------

    async fn read_library(
        &self,
        root: &str,
        scope: TidyScope,
        stopping: &CancellationToken,
    ) -> Option<Vec<TidyFile>> {
        if !Path::new(root).is_dir() {
            self.finish_with(TidyStatus::Failed, format!("There is no music folder at {root}."));
            return None;
        }
        let owned: Option<HashSet<String>> = match scope {
            TidyScope::WholeLibrary => None,
            TidyScope::OctoDownloads => {
                let Some(library) = &self.services.library else {
                    self.finish_with(
                        TidyStatus::Failed,
                        "Octo has no record of its downloads to go on.".to_string(),
                    );
                    return None;
                };
                Some(
                    library
                        .get_mappings()
                        .await
                        .into_iter()
                        .filter(|mapping| !mapping.external_provider.eq_ignore_ascii_case("lidarr"))
                        .map(|mapping| mapping.local_path)
                        .filter(|path| !path.is_empty())
                        .collect(),
                )
            }
        };
        let walked = {
            let root = root.to_string();
            tokio::task::spawn_blocking(move || walk(Path::new(&root), &AUDIO_EXTENSIONS))
                .await
                .unwrap_or_else(|panic| Err(std::io::Error::other(panic.to_string())))
        };
        let paths = match walked {
            Ok(paths) => paths,
            Err(failure) => {
                self.finish_with(
                    TidyStatus::Failed,
                    format!("Could not list the music folder: {failure}"),
                );
                return None;
            }
        };
        let mut files = Vec::with_capacity(paths.len());
        for chunk in paths.chunks(50) {
            if let Some(stop) = self.should_stop(stopping) {
                self.stopped(Some(stop));
                return None;
            }
            let chunk: Vec<(String, bool)> = chunk
                .iter()
                .map(|path| {
                    (
                        path.clone(),
                        owned.as_ref().is_none_or(|owned| owned.contains(path)),
                    )
                })
                .collect();
            let read = tokio::task::spawn_blocking(move || {
                chunk
                    .into_iter()
                    .map(|(path, in_scope)| read_file(&path, in_scope))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            files.extend(read);
            let (count, in_scope) = (files.len(), files.iter().filter(|file| file.in_scope).count());
            self.store.update(|run| {
                run.files = count;
                run.in_scope = in_scope;
            });
        }
        Some(files)
    }

    // ---- bookkeeping ------------------------------------------------------------------------

    fn start(&self, mode: TidyMode, scope: TidyScope, task: TidyTask, phase: &str) {
        self.store.replace(TidyRun {
            run_id: new_run_id(),
            status: TidyStatus::Running,
            mode,
            scope,
            task,
            phase: phase.to_string(),
            started_utc: Some(Utc::now()),
            ..TidyRun::default()
        });
    }

    /// The file layout the settings name now.
    fn layout(&self) -> Layout {
        Layout::from_settings(&self.settings.current().subsonic)
    }

    /// Whether the layout files songs in something other than artist folders at the top, so the
    /// Artists task leaves folders alone.
    fn leaves_artist_folders(&self) -> bool {
        !self.layout().top_folder_is_artist_or_none()
    }

    fn should_stop(&self, stopping: &CancellationToken) -> Option<Stop> {
        if stopping.is_cancelled() {
            Some(Stop::ShuttingDown)
        } else if self.cancel_requested.load(Ordering::SeqCst) {
            Some(Stop::Cancelled)
        } else {
            None
        }
    }

    fn stopped(&self, stop: Option<Stop>) {
        match stop {
            Some(Stop::ShuttingDown) => self.finish_with(
                TidyStatus::Interrupted,
                "Octo is shutting down. Resume carries on where it stopped.".to_string(),
            ),
            Some(Stop::Cancelled) => self.finish_with(
                TidyStatus::Cancelled,
                "Cancelled from the dashboard. What was done is in the journal, and Undo reverses it."
                    .to_string(),
            ),
            None => {}
        }
    }

    fn hold(&self, root: &str, path: &str, why: String) {
        hold_in(&self.store, root, path, why);
    }

    fn fail(&self, path: &str, why: &str) {
        fail_in(&self.store, path, why);
    }

    fn finish_with(&self, status: TidyStatus, reason: String) {
        if status == TidyStatus::Failed {
            error!("Library tidy stopped: {reason}");
        } else {
            info!("Library tidy {}: {reason}", status.name().to_lowercase());
        }
        self.store.update(|run| {
            if run.status != TidyStatus::Running {
                return;
            }
            run.status = status;
            run.reason = Some(reason);
            run.phase = String::new();
            run.finished_utc = Some(Utc::now());
        });
    }

    /// Completes the run and logs `summary`. The card already shows the counts a summary
    /// repeats, so `reason` is what the run says on top of them, if anything.
    fn complete_with_summary(&self, summary: String, reason: Option<String>) {
        info!("Library tidy completed: {summary}");
        self.store.update(|run| {
            if run.status != TidyStatus::Running {
                return;
            }
            run.status = TidyStatus::Completed;
            run.reason = reason;
            run.phase = String::new();
            run.finished_utc = Some(Utc::now());
        });
    }
}

const PREVIEW_FIRST: &str =
    "Preview first. Apply only does what a finished preview of the same files showed.";

/// Holds `path` back in the run being worked on, with `why`.
fn hold_in(store: &TidyStore, root: &str, path: &str, why: String) {
    let shown = planner::relative(root, path).map_or_else(|| path.to_string(), |s| s.join("/"));
    store.update(|run| {
        run.held += 1;
        run.held_rows.push(TidyRow {
            id: format!("held|{shown}"),
            kind: TidyKind::Skip,
            path: shown.clone(),
            before: shown,
            after: String::new(),
            evidence: why,
            files: 0,
            opt_in: false,
            ticked: false,
        });
    });
}

/// Counts `path` as a file the run being worked on could not change, with `why`.
fn fail_in(store: &TidyStore, path: &str, why: &str) {
    warn!("Library tidy could not change {path}: {why}");
    store.update(|run| {
        run.failed += 1;
        run.errors.push(format!("{path}: {why}"));
    });
}

/// Playlist files under the music folder that name a moved song name it where it is now. Each
/// file's text is journaled before it is rewritten.
fn follow_playlists_in(
    store: &TidyStore,
    journal: &TidyJournal,
    root: &str,
    moved: &[(String, String)],
    run_id: &str,
) {
    let playlists = match walk(Path::new(root), &PLAYLIST_EXTENSIONS) {
        Ok(found) => found,
        Err(failure) => {
            warn!("Library tidy could not look for playlists: {failure}");
            return;
        }
    };
    for playlist in playlists {
        let Ok(bytes) = std::fs::read(&playlist) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            hold_in(
                store,
                root,
                &playlist,
                "This playlist is not UTF-8 text, so the songs it names were not updated.".to_string(),
            );
            continue;
        };
        let Some(rewritten) = rewrite_playlist(&playlist, &text, moved) else {
            continue;
        };
        let entry = TidyJournalEntry::Playlist {
            path: playlist.clone(),
            before: text,
            run: run_id.to_string(),
            at: Utc::now(),
        };
        if let Err(failure) = journal.append(&entry) {
            fail_in(store, &playlist, &journal_failed(&failure.to_string()));
            return;
        }
        if let Err(failure) = state_file::write_atomic(Path::new(&playlist), rewritten.as_bytes()) {
            fail_in(store, &playlist, &failure.to_string());
        }
    }
}

/// A fresh run id.
fn new_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

const NO_STATUS: &str = "Navidrome did not say when it last scanned, so nothing was moved. Check that Octo has Navidrome admin credentials.";

const NAVIDROME_KEPT_SCANNING: &str =
    "Navidrome kept scanning, or did not answer, so nothing was moved. Apply again later.";

fn journal_failed(why: &str) -> String {
    format!(
        "The undo journal could not be written ({why}), so the tidy stopped. Nothing further was changed."
    )
}

// ---- one file, one folder -------------------------------------------------------------------

fn read_tags(file: &TagFile) -> TidyTags {
    let clean = |values: Vec<String>| -> Vec<String> {
        values
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect()
    };
    TidyTags {
        artist: clean(file.performers()),
        album_artist: clean(file.album_artists()),
        artists: extras::read_values(file, TagFields::ARTISTS),
    }
}

/// One audio file's facts. A file whose tags cannot be read still moves with its folder.
pub(crate) fn read_file(path: &str, in_scope: bool) -> TidyFile {
    match TagFile::open(path) {
        Ok(mut file) => TidyFile {
            path: path.to_string(),
            tags: read_tags(&file),
            artist_ids: extras::read_artist_ids(&file),
            recording_id: extras::read_recording_id(&mut file),
            title: file.title(),
            in_scope,
            readable: true,
            slash_artist: extras::artist_frame_splits_on_slash(&file),
        },
        Err(_) => TidyFile {
            path: path.to_string(),
            in_scope,
            readable: false,
            ..TidyFile::default()
        },
    }
}

/// Writes only the fields that differ, so a field read differently from how it is stored is
/// never written back in that reading.
fn write_tags(file: &mut TagFile, from: &TidyTags, to: &TidyTags) {
    if from.artist != to.artist {
        file.set_performers(&to.artist);
    }
    if from.album_artist != to.album_artist {
        file.set_album_artists(&to.album_artist);
    }
    if from.artists != to.artists {
        if to.artists.is_empty() {
            extras::set_or_clear_text(file, TagFields::ARTISTS, None);
        } else {
            extras::set_multi_value(file, "ARTISTS", &to.artists);
        }
    }
}

fn retag_file(retag: &TidyRetag, run_id: &str, journal: &TidyJournal) -> Outcome {
    let mut file = match TagFile::open(&retag.path) {
        Ok(file) => file,
        Err(failure) => {
            return if Path::new(&retag.path).exists() {
                Outcome::Failed(failure.to_string())
            } else {
                Outcome::Held("It is no longer there.".to_string())
            };
        }
    };
    let now = read_tags(&file);
    if now == retag.after {
        return Outcome::AlreadyDone;
    }
    if now != retag.before {
        return Outcome::Held("Its artist tags changed after the preview. Preview again.".to_string());
    }
    let entry = TidyJournalEntry::Tags {
        path: retag.path.clone(),
        before: retag.before.clone(),
        after: retag.after.clone(),
        run: run_id.to_string(),
        at: Utc::now(),
    };
    if let Err(failure) = journal.append(&entry) {
        return Outcome::JournalFailed(failure.to_string());
    }
    write_tags(&mut file, &retag.before, &retag.after);
    if let Err(failure) = file.save() {
        return Outcome::Failed(failure.to_string());
    }
    let Some((size, modified)) = stamp_of(&retag.path) else {
        return Outcome::Failed("It could not be read after it was written.".to_string());
    };
    let stamp = TidyJournalEntry::Stamp {
        path: retag.path.clone(),
        size,
        modified,
        run: run_id.to_string(),
    };
    match journal.append(&stamp) {
        Ok(()) => Outcome::Done,
        Err(failure) => Outcome::JournalFailed(failure.to_string()),
    }
}

/// A file's size and last write time.
fn stamp_of(path: &str) -> Option<(u64, DateTime<Utc>)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), DateTime::<Utc>::from(meta.modified().ok()?)))
}

/// Moves one album folder whole, by renaming it, only while every song in it is as planned and
/// as Navidrome's last scan saw it.
fn move_folder(
    movement: &TidyMove,
    scan_start: DateTime<Utc>,
    stamps: &HashMap<String, (u64, DateTime<Utc>)>,
    run_id: &str,
    journal: &TidyJournal,
) -> Outcome {
    let (from, to) = (Path::new(&movement.from), Path::new(&movement.to));
    let from_meta = std::fs::symlink_metadata(from);
    let to_exists = std::fs::symlink_metadata(to).is_ok();
    match &from_meta {
        Err(_) if to_exists => return Outcome::AlreadyDone,
        Err(_) => return Outcome::Held("It is no longer there.".to_string()),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Outcome::Held(
                "It is a link or not a folder, and the tidy only moves real folders.".to_string(),
            );
        }
        Ok(_) => {}
    }
    if to_exists {
        return Outcome::Held(format!(
            "{} is already there, so both stay as they are.",
            movement.to
        ));
    }
    if let Err(why) = target_is_free(&movement.to, &movement.from, false) {
        return Outcome::Held(why);
    }
    let planned: HashSet<&String> = movement.files.iter().collect();
    let now = match walk_all(from, &planned) {
        Ok(found) => found,
        Err(failure) => return Outcome::Failed(failure.to_string()),
    };
    if now.links {
        return Outcome::Held("It holds a link, and the tidy never moves one.".to_string());
    }
    let audio: HashSet<&String> = now.audio.iter().collect();
    if planned != audio {
        return Outcome::Held("Its songs changed since the preview. Preview again.".to_string());
    }
    for file in &movement.files {
        let Some((size, modified)) = stamp_of(file) else {
            return Outcome::Held("A song in it could not be read.".to_string());
        };
        if let Some(stamp) = stamps.get(file)
            && *stamp != (size, modified)
        {
            return Outcome::Held(
                "A song in it changed after Octo re-tagged it. Resume once Navidrome has scanned it."
                    .to_string(),
            );
        }
        if modified >= scan_start {
            return Outcome::Held(
                "A song in it changed after Navidrome's last scan began, so moving it now could cost its stars and plays. Resume once Navidrome has scanned.".to_string(),
            );
        }
    }
    let Some(parent) = to.parent() else {
        return Outcome::Failed("The new folder has no parent.".to_string());
    };
    if !same_device(from, parent) {
        return Outcome::Held(
            "The new folder would be on another disk, and the tidy only renames within one.".to_string(),
        );
    }
    let entry = TidyJournalEntry::Move {
        from: movement.from.clone(),
        to: movement.to.clone(),
        run: run_id.to_string(),
        at: Utc::now(),
        file: false,
        pool: false,
    };
    if let Err(failure) = journal.append(&entry) {
        return Outcome::JournalFailed(failure.to_string());
    }
    if let Err(failure) = std::fs::create_dir_all(parent).and_then(|()| std::fs::rename(from, to)) {
        return Outcome::Failed(failure.to_string());
    }
    Outcome::Done
}

/// Whether two paths are on one filesystem, measured at the nearest folder that exists.
#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let device = |path: &Path| {
        path.ancestors()
            .find_map(|ancestor| std::fs::metadata(ancestor).ok())
            .map(|meta| meta.dev())
    };
    match (device(a), device(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_device(_: &Path, _: &Path) -> bool {
    true
}

/// What is under a folder: its audio files, and whether any entry is a link.
struct Contents {
    audio: Vec<String>,
    links: bool,
}

/// Dot folders and dot files are left out, as [`walk`] leaves them out: a half-made copy Octo
/// keeps hidden beside a song is not one of the album's songs. A hidden file the plan read
/// (`planned`: Octo's records name it) counts.
fn walk_all(folder: &Path, planned: &HashSet<&String>) -> std::io::Result<Contents> {
    let mut contents = Contents {
        audio: Vec::new(),
        links: false,
    };
    let mut folders = vec![folder.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder)? {
            let entry = entry?;
            let path = joined(&folder, &entry.file_name().to_string_lossy());
            if PathHelper::is_hidden_name(&entry.file_name().to_string_lossy())
                && !planned.contains(&path.to_string_lossy().into_owned())
            {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                contents.links = true;
            } else if kind.is_dir() {
                folders.push(path);
            } else if has_extension(&path, &AUDIO_EXTENSIONS) {
                contents.audio.push(path.to_string_lossy().into_owned());
            }
        }
    }
    Ok(contents)
}

/// The files under `root` with one of `extensions`, in path order. Dot folders and dot files
/// (Octo's trash, a NAS's metadata) are left out, as Navidrome leaves them out, and links are
/// never followed.
fn walk(root: &Path, extensions: &[&str]) -> std::io::Result<Vec<String>> {
    let mut found = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder)? {
            let entry = entry?;
            if PathHelper::is_hidden_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            let path = joined(&folder, &entry.file_name().to_string_lossy());
            if kind.is_dir() {
                folders.push(path);
            } else if has_extension(&path, extensions) {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    found.sort();
    Ok(found)
}

/// `folder/name` with a `/`, as every path the tidy keeps is written, whatever the platform.
fn joined(folder: &Path, name: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}/{name}",
        folder.to_string_lossy().trim_end_matches('/')
    ))
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extensions
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
}

/// Whether the filesystem under `root` treats "a" and "A" as one name, told by looking up an
/// existing entry under its name in other capitals. Writes nothing.
pub(crate) fn case_insensitive(root: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let swapped: String = name
            .chars()
            .map(|ch| {
                if ch.is_lowercase() {
                    ch.to_uppercase().next().unwrap_or(ch)
                } else {
                    ch.to_lowercase().next().unwrap_or(ch)
                }
            })
            .collect();
        if swapped == name {
            continue;
        }
        let other = Path::new(root).join(&swapped);
        return match (std::fs::metadata(entry.path()), std::fs::metadata(&other)) {
            (Ok(a), Ok(b)) => same_file(&a, &b),
            _ => false,
        };
    }
    false
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

// ---- playlists ------------------------------------------------------------------------------

/// The playlist's text with every line naming a moved song naming it where it is now, through
/// every move of `moved` in order (a folder renamed, then a file in it), written the way it was (a
/// relative line stays relative). None when nothing changes.
pub(crate) fn rewrite_playlist(playlist: &str, text: &str, moved: &[(String, String)]) -> Option<String> {
    let folder = Path::new(playlist).parent()?;
    let mut changed = false;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            let (body, cr) = match line.strip_suffix('\r') {
                Some(body) => (body, "\r"),
                None => (line, ""),
            };
            let trimmed = body.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return line.to_string();
            }
            let relative = !trimmed.starts_with('/');
            let absolute = if relative {
                normalize(&folder.join(trimmed))
            } else {
                normalize(Path::new(trimmed))
            };
            let now = final_path(&absolute, moved);
            if now == absolute {
                return line.to_string();
            }
            changed = true;
            let written = if relative {
                relative_to(folder, Path::new(&now))
            } else {
                now
            };
            format!("{written}{cr}")
        })
        .collect();
    changed.then(|| lines.join("\n"))
}

fn normalize(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    format!("/{}", parts.join("/"))
}

fn relative_to(folder: &Path, target: &Path) -> String {
    let from: Vec<String> = normalize(folder)
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let to: Vec<String> = normalize(target)
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(to[common..].iter().cloned());
    let joined: PathBuf = parts.iter().collect();
    joined.to_string_lossy().into_owned()
}

// ---- file layout ----------------------------------------------------------------------------

const LAYOUT_CHANGED: &str = "The File layout changed since the preview. Preview again.";
const ROW_CHANGED: &str = "Its songs changed since the preview. Preview again.";
const LIDARR_SILENT: &str =
    "Lidarr did not answer, so Octo cannot tell which songs Lidarr keeps. Preview again once it answers.";
const LIDARR_SILENT_APPLY: &str = "Lidarr did not answer, so Octo cannot tell which songs Lidarr keeps, and nothing was moved. Apply again once it answers.";
const LIDARR_HERE_NOW: &str = "Lidarr keeps a song here now.";
const FOLDER_HELD: &str = "Its folder could not be moved, so this song stays where it is.";
const SCANNING: &str = "Navidrome was scanning or did not answer. Resume once it is idle.";
const CHANGED_SINCE_SCAN: &str = "It changed after Navidrome's last scan began, so moving it now could cost its stars and plays. Resume once Navidrome has scanned.";

/// The first problem of the layout, in the dashboard's words: the File layout task never plans
/// with a stand-in template.
fn layout_problem(layout: &Layout) -> Option<String> {
    let problem = layout.problems().first()?;
    let name = match problem.setting {
        "FolderTemplateSingles" => "Singles",
        "FolderTemplateCompilations" => "Compilations",
        _ => "Albums",
    };
    Some(format!(
        "The {name} layout has a problem: {} Fix it on the File layout card first.",
        problem.message.trim_end()
    ))
}

impl TidyWorker {
    /// Why a File layout preview, apply or resume may not run now: the layout has a problem, or
    /// (not for a preview) it is not the layout the preview was made with.
    pub fn layout_refusal(&self, mode: TidyMode) -> Option<String> {
        if let Some(why) = layout_problem(&self.layout()) {
            return Some(why);
        }
        if mode == TidyMode::Preview {
            return None;
        }
        let plan = self
            .store
            .plan()
            .filter(|plan| plan.task == TidyTask::FileLayout)?;
        (plan.layout.as_ref() != Some(&self.saved_layout())).then(|| LAYOUT_CHANGED.to_string())
    }

    /// The layout as a File layout preview keeps it.
    fn saved_layout(&self) -> TidyLayout {
        let structure = self.settings.current().subsonic.folder_structure;
        TidyLayout {
            structure: format!("{structure:?}"),
            templates: self.layout().texts().to_vec(),
        }
    }

    fn layout_options(
        &self,
        layout: Layout,
        root: &str,
        case_insensitive: bool,
        excluded: &[String],
        lidarr: Option<&TidyLidarr>,
        files: &[LayoutFile],
    ) -> LayoutOptions {
        let mut options = LayoutOptions::new(layout);
        options.budget = PATH_BUDGET;
        options.case_insensitive = case_insensitive;
        options.excluded = excluded.to_vec();
        if let Some(answer) = lidarr {
            let configured = self.settings.current().lidarr.root_folder_path.clone();
            options.lidarr = layout_pass::lidarr_view(root, configured.as_deref(), answer, files);
        }
        if let Some(pending) = self.services.lidarr_pending.clone() {
            options.follow = Some(Arc::new(move |artists: &[String], album: &str, target: &str| {
                !pending.covering(artists, album, target).is_empty()
            }));
        }
        options
    }

    async fn preview_layout(&self, request: &TidyRequest, stopping: &CancellationToken) {
        let root = self.music_root();
        self.start(
            TidyMode::Preview,
            request.scope,
            TidyTask::FileLayout,
            "Checking the File layout",
        );
        info!(
            "Library tidy File layout preview started: scope {}",
            request.scope.name()
        );
        let layout = self.layout();
        if let Some(why) = layout_problem(&layout) {
            self.finish_with(TidyStatus::Failed, why);
            return;
        }
        self.store.update(|run| run.phase = "Asking Lidarr".to_string());
        let lidarr = match self.ask_lidarr(None).await {
            Ok(answer) => answer,
            Err(failure) => {
                warn!("Lidarr did not answer the File layout preview: {failure}");
                self.finish_with(TidyStatus::Failed, LIDARR_SILENT.to_string());
                return;
            }
        };
        self.store
            .update(|run| run.phase = "Reading the library".to_string());
        let Some((files, disk)) = self.read_layout_library(&root, request.scope, stopping).await else {
            return;
        };
        let case_insensitive = case_insensitive(&root);
        let options = self.layout_options(
            layout.clone(),
            &root,
            case_insensitive,
            &request.excluded,
            lidarr.as_ref(),
            &files,
        );
        let pool = self.plan_pool(&layout, case_insensitive, &request.excluded).await;
        let planned = with_pool(layout_pass::plan(&root, &files, &disk, &options), pool);
        let (folders, songs) = counts(
            &planned.plan.moves,
            &planned.plan.file_moves,
            &planned.plan.pool_moves,
        );
        let hash = planned.plan.hash();
        let saved = self.store.save_plan(TidyPlanFile {
            root: root.clone(),
            scope: request.scope,
            task: TidyTask::FileLayout,
            excluded: request.excluded.clone(),
            case_insensitive,
            plan: planned.plan,
            layout: Some(self.saved_layout()),
            row_hashes: planned.row_hashes,
            lidarr,
            ..TidyPlanFile::default()
        });
        if let Err(failure) = saved {
            self.finish_with(
                TidyStatus::Failed,
                format!("The preview could not be saved: {failure}"),
            );
            return;
        }
        self.store.update(|run| {
            run.planned_moves = folders;
            run.planned_file_moves = songs;
            run.plan_hash = Some(hash);
        });
        self.finish_with(TidyStatus::Completed, "Nothing has been written.".to_string());
    }

    async fn apply_layout(&self, request: &TidyRequest, stopping: &CancellationToken) {
        let previous = self.store.current();
        let plan_file = self.store.plan();
        self.start(
            TidyMode::Apply,
            request.scope,
            TidyTask::FileLayout,
            "Checking the File layout",
        );
        let refuse = |why: String| self.finish_with(TidyStatus::Failed, why);
        let Some(plan_file) = plan_file else {
            refuse(PREVIEW_FIRST.to_string());
            return;
        };
        if plan_file.task != TidyTask::FileLayout || previous.task != TidyTask::FileLayout {
            refuse(PREVIEW_FIRST.to_string());
            return;
        }
        if request.resume {
            if !previous.can_resume() || plan_file.work.is_none() {
                refuse("There is no stopped tidy to resume.".to_string());
                return;
            }
        } else if !previous.is_finished_preview() || plan_file.scope != request.scope {
            refuse(PREVIEW_FIRST.to_string());
            return;
        }
        let layout = self.layout();
        if let Some(why) = layout_problem(&layout) {
            refuse(why);
            return;
        }
        if plan_file.layout.as_ref() != Some(&self.saved_layout()) {
            refuse(LAYOUT_CHANGED.to_string());
            return;
        }
        let gate = self.check_navidrome(true).await;
        if !gate.apply_allowed {
            refuse(gate.reason.unwrap_or_default());
            return;
        }
        let Some(library) = self.services.library.clone() else {
            refuse("Octo has no library service to ask Navidrome for scans.".to_string());
            return;
        };
        let mut job = match self.write_gate.begin_job(Self::JOB_NAME).await {
            Ok(job) => job,
            Err(holder) => {
                refuse(LibraryWriteGate::busy_message(&holder));
                return;
            }
        };
        let root = plan_file.root.clone();
        let mut work = if request.resume {
            plan_file.work.clone().unwrap_or_default()
        } else {
            self.store
                .update(|run| run.phase = "Reading the library".to_string());
            let Some((files, disk)) = self.read_layout_library(&root, plan_file.scope, stopping).await else {
                return;
            };
            let options = self.layout_options(
                layout.clone(),
                &root,
                plan_file.case_insensitive,
                &plan_file.excluded,
                plan_file.lidarr.as_ref(),
                &files,
            );
            let pool = self
                .plan_pool(&layout, plan_file.case_insensitive, &plan_file.excluded)
                .await;
            let planned = with_pool(layout_pass::plan(&root, &files, &disk, &options), pool);
            // Only the rows the preview showed run, each only while it is as it was then; a row
            // whose songs changed, or are no longer to move, is held, and a new row never runs.
            let mut kept: HashSet<String> = HashSet::new();
            for row in plan_file
                .plan
                .rows
                .iter()
                .filter(|row| row.kind == TidyKind::Layout)
            {
                match (plan_file.row_hashes.get(&row.id), planned.row_hashes.get(&row.id)) {
                    (Some(was), Some(now)) if was == now => {
                        kept.insert(row.id.clone());
                    }
                    _ => self.hold_row(&row.path, ROW_CHANGED),
                }
            }
            let work = work_of(&self.store.current().run_id, &planned.plan, &kept);
            let mut saved = plan_file.clone();
            saved.work = Some(work.clone());
            if let Err(failure) = self.store.save_plan(saved) {
                refuse(format!(
                    "The work could not be saved, so nothing was changed: {failure}"
                ));
                return;
            }
            work
        };
        // Lidarr again, never under the exclusive gate: a row holding a file Lidarr now keeps, or
        // moving into a folder of one, stays.
        job.pause();
        self.store.update(|run| run.phase = "Asking Lidarr".to_string());
        let lidarr_rows = self.rows_lidarr_keeps(&root, &work).await;
        job.resume().await;
        match lidarr_rows {
            Ok(rows) => {
                for (row, path) in rows {
                    self.hold_row(&path, LIDARR_HERE_NOW);
                    work = without_row(work, &row);
                }
            }
            Err(failure) => {
                warn!("Lidarr did not answer the File layout apply: {failure}");
                refuse(LIDARR_SILENT_APPLY.to_string());
                return;
            }
        }
        let (folders, songs) = counts(&work.moves, &work.file_moves, &work.pool_moves);
        self.store.update(|run| {
            run.planned_moves = folders;
            run.planned_file_moves = songs;
            run.plan_hash = previous.plan_hash.clone();
        });
        info!("Library tidy File layout apply started: {folders} folder(s) and {songs} song(s) to move");
        let keep = self.lidarr_folders(&root, plan_file.lidarr.as_ref());
        self.execute_layout(&root, &work, library, stopping, &mut job, &keep)
            .await;
    }

    /// Lidarr's artist folders in Octo's view, never removed when a move empties them.
    fn lidarr_folders(&self, root: &str, answer: Option<&TidyLidarr>) -> Vec<String> {
        let Some(answer) = answer else {
            return Vec::new();
        };
        let configured = self.settings.current().lidarr.root_folder_path.clone();
        layout_pass::lidarr_view(root, configured.as_deref(), answer, &[]).artist_folders
    }

    /// What Lidarr keeps: None when Lidarr is not set up, an error when it does not answer. With
    /// `covering`, only the track files of artists whose folder holds one of those paths are
    /// asked for.
    async fn ask_lidarr(&self, covering: Option<(&str, &[String])>) -> Result<Option<TidyLidarr>, String> {
        let Some(client) = &self.services.lidarr else {
            return Ok(None);
        };
        let settings = self.settings.current().lidarr.clone();
        let blank = |value: &Option<String>| value.as_deref().is_none_or(|value| value.trim().is_empty());
        if blank(&settings.base_url) || blank(&settings.api_key) {
            return Ok(None);
        }
        let roots = client.root_folders().await.map_err(|e| e.to_string())?;
        let artists = client.artists().await.map_err(|e| e.to_string())?;
        let mut answer = TidyLidarr {
            roots: roots.into_iter().map(|root| root.path).collect(),
            artists: artists
                .iter()
                .filter_map(|artist| artist.path.clone().map(|path| (artist.id, path)))
                .collect(),
            files: Vec::new(),
        };
        for artist in &artists {
            if let Some((root, paths)) = covering {
                let folder = artist.path.as_deref().and_then(|path| {
                    layout_pass::lidarr_path_in_octo(
                        root,
                        settings.root_folder_path.as_deref(),
                        &answer.roots,
                        path,
                    )
                });
                let covers = folder.is_some_and(|folder| {
                    paths
                        .iter()
                        .any(|path| *path == folder || path.starts_with(&format!("{folder}/")))
                });
                if !covers {
                    continue;
                }
            }
            let files = client
                .artist_track_files(artist.id)
                .await
                .map_err(|e| e.to_string())?;
            answer.files.extend(
                files
                    .into_iter()
                    .filter_map(|file| file.path.map(|path| (path, file.size_bytes.max(0) as u64))),
            );
        }
        Ok(Some(answer))
    }

    /// The rows of `work` Lidarr keeps a song in now, asked again: each row whose files Lidarr
    /// tracks, or which moves a song into a folder holding one, with the row's folder.
    async fn rows_lidarr_keeps(&self, root: &str, work: &TidyWork) -> Result<Vec<(String, String)>, String> {
        let mut paths: Vec<String> = Vec::new();
        for movement in &work.moves {
            paths.push(movement.from.clone());
            paths.push(movement.to.clone());
        }
        for movement in work.file_moves.iter().chain(&work.pool_moves) {
            paths.push(movement.from.clone());
            paths.push(movement.to.clone());
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let Some(answer) = self.ask_lidarr(Some((root, &paths))).await? else {
            return Ok(Vec::new());
        };
        let configured = self.settings.current().lidarr.root_folder_path.clone();
        let view = layout_pass::lidarr_view(root, configured.as_deref(), &answer, &[]);
        let dirs: HashSet<String> = view.tracked.iter().map(|path| parent_dir(path)).collect();
        let mut held: Vec<(String, String)> = Vec::new();
        let mut hold = |row: &str, folder: &str| {
            if !held.iter().any(|(seen, _)| seen == row) {
                let shown = planner::relative(root, folder).map_or_else(String::new, |s| s.join("/"));
                held.push((row.to_string(), shown));
            }
        };
        for movement in &work.moves {
            if dirs.contains(&movement.from) || dirs.contains(&movement.to) {
                hold(&movement.row, &movement.from);
            }
        }
        for movement in work.file_moves.iter().chain(&work.pool_moves) {
            if view.tracked.contains(&movement.from) || dirs.contains(&parent_dir(&movement.to)) {
                hold(&movement.row, &parent_dir(&movement.from));
            }
        }
        Ok(held)
    }

    /// Every audio file under `root`, with its layout tags and Octo's record of it, and every
    /// folder's listing.
    async fn read_layout_library(
        &self,
        root: &str,
        scope: TidyScope,
        stopping: &CancellationToken,
    ) -> Option<(Vec<LayoutFile>, LayoutDisk)> {
        if !Path::new(root).is_dir() {
            self.finish_with(TidyStatus::Failed, format!("There is no music folder at {root}."));
            return None;
        }
        let Some(library) = &self.services.library else {
            self.finish_with(
                TidyStatus::Failed,
                "Octo has no record of its downloads to go on.".to_string(),
            );
            return None;
        };
        let mut mappings: HashMap<String, LocalSongMapping> = HashMap::new();
        for mapping in library.get_mappings().await {
            if !mapping.local_path.is_empty() {
                mappings.insert(mapping.local_path.clone(), mapping);
            }
        }
        let settings = self.settings.current();
        let trash = library_walk::trash_folder(Path::new(root), &settings.library_actions);
        let members = PathBuf::from(members_place::resolve_in(&settings, root));
        let mapped: HashSet<String> = mappings.keys().cloned().collect();
        let walked = {
            let root = root.to_string();
            tokio::task::spawn_blocking(move || walk_layout(Path::new(&root), &mapped, &trash, &members))
                .await
                .unwrap_or_else(|panic| Err(std::io::Error::other(panic.to_string())))
        };
        let (paths, disk) = match walked {
            Ok(found) => found,
            Err(failure) => {
                self.finish_with(
                    TidyStatus::Failed,
                    format!("Could not list the music folder: {failure}"),
                );
                return None;
            }
        };
        let mut files = Vec::with_capacity(paths.len());
        for chunk in paths.chunks(50) {
            if let Some(stop) = self.should_stop(stopping) {
                self.stopped(Some(stop));
                return None;
            }
            let chunk: Vec<(String, Option<LocalSongMapping>)> = chunk
                .iter()
                .map(|path| (path.clone(), mappings.get(path).cloned()))
                .collect();
            let read = tokio::task::spawn_blocking(move || {
                chunk
                    .into_iter()
                    .map(|(path, mapping)| layout_file(&path, scope, mapping))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            files.extend(read);
            let (count, in_scope) = (files.len(), files.iter().filter(|file| file.in_scope).count());
            self.store.update(|run| {
                run.files = count;
                run.in_scope = in_scope;
            });
        }
        Some((files, disk))
    }

    /// The family pool's plan, as pool rows: its files planned like the library's (from the
    /// values placement kept, the shape of their path, or their tags) against the pool's folder,
    /// in the pool's smaller budget, every file Octo's. None when there is no pool to read.
    async fn plan_pool(
        &self,
        layout: &Layout,
        case_insensitive: bool,
        excluded: &[String],
    ) -> Option<layout_pass::LayoutPlan> {
        let pool = self.services.family_pool.clone()?;
        let root = pool.pool_root_path().trim_end_matches('/').to_string();
        if !Path::new(&root).is_dir() {
            return None;
        }
        let mut saved: HashMap<String, LayoutValues> = HashMap::new();
        if let Some(library) = &self.services.library {
            for mapping in library.get_mappings().await {
                if let Some(values) = mapping.layout {
                    saved.insert(mapping.local_path, values);
                }
            }
        }
        let walked = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || {
                let none = Path::new(&root).join(".octo-none");
                walk_layout(Path::new(&root), &HashSet::new(), &none, &none)
            })
            .await
            .unwrap_or_else(|panic| Err(std::io::Error::other(panic.to_string())))
        };
        let (paths, disk) = match walked {
            Ok(found) => found,
            Err(failure) => {
                warn!("Library tidy could not read the family pool: {failure}");
                return None;
            }
        };
        let files: Vec<LayoutFile> = {
            let records: Vec<_> = paths.iter().map(|path| pool.store().get(path)).collect();
            let values: Vec<_> = paths.iter().map(|path| saved.get(path).cloned()).collect();
            tokio::task::spawn_blocking(move || {
                paths
                    .into_iter()
                    .zip(records)
                    .zip(values)
                    .map(|((path, record), values)| LayoutFile {
                        size: std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
                        in_scope: true,
                        tags: read_layout_tags(Path::new(&path)),
                        mapping: record.map(|record| LayoutMapping {
                            title: record.title,
                            album: record.album,
                            artist: record.artist,
                            values,
                        }),
                        path,
                    })
                    .collect()
            })
            .await
            .unwrap_or_default()
        };
        let mut options = LayoutOptions::new(layout.clone());
        options.budget = POOL_BUDGET;
        options.case_insensitive = case_insensitive;
        options.excluded = layout_pass::pool_excluded(excluded);
        options.pool = true;
        Some(layout_pass::as_pool_rows(layout_pass::plan(
            &root, &files, &disk, &options,
        )))
    }

    /// The moves of `work`, then a scan and the check of what Navidrome sees.
    async fn execute_layout(
        &self,
        root: &str,
        work: &TidyWork,
        library: Arc<dyn ILocalLibraryService>,
        stopping: &CancellationToken,
        job: &mut LibraryJob,
        keep: &[String],
    ) {
        let hold = library.scan_hold().map(|hold| hold.hold());
        let before = self.list_songs().await;
        let mut moved: Vec<(String, String)> = Vec::new();
        let mut linked: Vec<String> = Vec::new();
        let stopped = self
            .move_into_layout(
                root,
                work,
                library.as_ref(),
                stopping,
                job,
                keep,
                (&mut moved, &mut linked),
            )
            .await;
        // Nothing in the music folder's songs is written from here on: downloads go on while
        // Navidrome scans.
        job.pause();
        self.drop_lonely_begin(&work.run_id);
        let wanted = hold.map(|hold| hold.release()).unwrap_or(false);
        if self.store.current().changed_anything() || wanted {
            library.start_scan().await;
        }
        if let Some(stop) = stopped {
            self.stopped(Some(stop));
            return;
        }
        if self.store.current().status != TidyStatus::Running {
            return;
        }
        if !moved.is_empty() {
            let mut seen = HashSet::new();
            let files: Vec<String> = work
                .moves
                .iter()
                .flat_map(|movement| movement.files.clone())
                .chain(
                    work.file_moves
                        .iter()
                        .filter(|movement| movement.song.is_none() && is_audio_name(&movement.from))
                        .map(|movement| final_origin(&movement.from, &work.moves)),
                )
                .chain(linked)
                .filter(|file| seen.insert(file.clone()))
                .collect();
            self.check_what_navidrome_sees(&files, &moved, before, library.as_ref(), stopping)
                .await;
        }
        let run = self.store.current();
        self.complete_with_summary(
            format!(
                "Moved {} and {} into the File layout; {} held back.",
                counted(run.moved, "folder", "folders"),
                counted(run.moved_files, "song", "songs"),
                run.held
            ),
            None,
        );
    }

    /// The folder renames, then the file moves one folder at a time (one Navidrome check, one
    /// gate resume and one round of record keeping a folder), then the family pool's files, then
    /// the playlists. Some when the run was stopped. `moved` gets each move done, in order, the
    /// members' links included; `linked` each member's link a pool move took, for the check.
    #[allow(clippy::too_many_arguments)]
    async fn move_into_layout(
        &self,
        root: &str,
        work: &TidyWork,
        library: &dyn ILocalLibraryService,
        stopping: &CancellationToken,
        job: &mut LibraryJob,
        keep: &[String],
        (moved, linked): (&mut Vec<(String, String)>, &mut Vec<String>),
    ) -> Option<Stop> {
        self.store
            .update(|run| run.phase = "Waiting for Navidrome to finish scanning".to_string());
        if !self.idle_for_moves(library, stopping, job).await {
            if let Some(stop) = self.should_stop(stopping) {
                return Some(stop);
            }
            self.finish_with(TidyStatus::Failed, NAVIDROME_KEPT_SCANNING.to_string());
            return None;
        }
        // A song changed after this scan began may not be the one Navidrome knows, so it stays.
        let Some(scan_start) = library
            .get_scan_status()
            .await
            .and_then(|status| status.last_scan_started_at())
        else {
            self.finish_with(TidyStatus::Failed, NO_STATUS.to_string());
            return None;
        };
        if work.is_empty() {
            return None;
        }
        // Undo says what it puts back from this first line of the run.
        let begun = self
            .journal
            .read_all()
            .iter()
            .any(|entry| matches!(entry, TidyJournalEntry::Begin { run, .. } if *run == work.run_id));
        if !begun {
            let begin = TidyJournalEntry::Begin {
                run: work.run_id.clone(),
                task: TidyTask::FileLayout,
                at: Utc::now(),
            };
            if let Err(failure) = self.journal.append(&begin) {
                self.finish_with(TidyStatus::Failed, journal_failed(&failure.to_string()));
                return None;
            }
        }

        // ---- folders ----
        self.store.update(|run| run.phase = "Moving folders".to_string());
        let mut held_folders: HashSet<String> = HashSet::new();
        let mut in_a_row = 0;
        for movement in &work.moves {
            if let Some(stop) = self.should_stop(stopping) {
                self.follow_playlists(root, moved, &work.run_id).await;
                return Some(stop);
            }
            if !self.idle_for_moves(library, stopping, job).await {
                if let Some(stop) = self.should_stop(stopping) {
                    self.follow_playlists(root, moved, &work.run_id).await;
                    return Some(stop);
                }
                held_folders.insert(movement.from.clone());
                self.hold(root, &movement.from, SCANNING.to_string());
                continue;
            }
            let (journal, movement_for_move, run_id) =
                (self.journal.clone(), movement.clone(), work.run_id.clone());
            let outcome = tokio::task::spawn_blocking(move || {
                move_folder(&movement_for_move, scan_start, &HashMap::new(), &run_id, &journal)
            })
            .await
            .unwrap_or_else(|panic| Outcome::Failed(panic.to_string()));
            match outcome {
                Outcome::Done | Outcome::AlreadyDone => {
                    let done = matches!(outcome, Outcome::Done);
                    in_a_row = 0;
                    self.follow(library, &movement.from, &movement.to).await;
                    if done {
                        remove_empty_up(&parent_dir(&movement.from), root, keep);
                        self.store.update(|run| run.moved += 1);
                    }
                    moved.push((movement.from.clone(), movement.to.clone()));
                }
                Outcome::Held(why) => {
                    in_a_row = 0;
                    held_folders.insert(movement.from.clone());
                    self.hold(root, &movement.from, why);
                }
                Outcome::Failed(why) => {
                    in_a_row += 1;
                    held_folders.insert(movement.from.clone());
                    self.fail(&movement.from, &why);
                    if in_a_row >= Self::MAX_CONSECUTIVE_FAILURES {
                        self.follow_playlists(root, moved, &work.run_id).await;
                        self.finish_with(
                            TidyStatus::Failed,
                            format!(
                                "{in_a_row} folders in a row could not be moved. Is the music folder read-only?"
                            ),
                        );
                        return None;
                    }
                }
                Outcome::JournalFailed(why) => {
                    self.follow_playlists(root, moved, &work.run_id).await;
                    self.finish_with(TidyStatus::Failed, journal_failed(&why));
                    return None;
                }
            }
        }

        // ---- files, one folder at a time ----
        self.store.update(|run| run.phase = "Moving files".to_string());
        let mut rows: Vec<&str> = Vec::new();
        for movement in &work.file_moves {
            if !rows.contains(&movement.row.as_str()) {
                rows.push(&movement.row);
            }
        }
        for copy in &work.copies {
            if !rows.contains(&copy.row.as_str()) {
                rows.push(&copy.row);
            }
        }
        let mut in_a_row = 0;
        for row in rows {
            if let Some(stop) = self.should_stop(stopping) {
                self.follow_playlists(root, moved, &work.run_id).await;
                return Some(stop);
            }
            let group: Vec<&TidyFileMove> = work.file_moves.iter().filter(|m| m.row == row).collect();
            if !self.idle_for_moves(library, stopping, job).await {
                if let Some(stop) = self.should_stop(stopping) {
                    self.follow_playlists(root, moved, &work.run_id).await;
                    return Some(stop);
                }
                for movement in group.iter().filter(|m| m.song.is_none()) {
                    self.hold(root, &movement.from, SCANNING.to_string());
                }
                continue;
            }
            let mut done: Vec<(String, String)> = Vec::new();
            let mut stayed: HashSet<String> = HashSet::new();
            let mut sources: Vec<String> = Vec::new();
            for movement in group {
                if let Some(song) = &movement.song
                    && stayed.contains(song)
                {
                    continue;
                }
                if movement
                    .after
                    .as_ref()
                    .is_some_and(|folder| held_folders.contains(folder))
                {
                    stayed.insert(movement.from.clone());
                    if movement.song.is_none() {
                        self.hold(root, &movement.from, FOLDER_HELD.to_string());
                    }
                    continue;
                }
                let (journal, movement_for_move, run_id) =
                    (self.journal.clone(), movement.clone(), work.run_id.clone());
                let outcome = tokio::task::spawn_blocking(move || {
                    move_one_file(&movement_for_move, scan_start, &run_id, &journal)
                })
                .await
                .unwrap_or_else(|panic| Outcome::Failed(panic.to_string()));
                match outcome {
                    Outcome::Done => {
                        in_a_row = 0;
                        done.push((movement.from.clone(), movement.to.clone()));
                        sources.push(parent_dir(&movement.from));
                        if movement.song.is_none() {
                            self.store.update(|run| run.moved_files += 1);
                        }
                    }
                    Outcome::AlreadyDone => {
                        in_a_row = 0;
                        done.push((movement.from.clone(), movement.to.clone()));
                    }
                    Outcome::Held(why) => {
                        in_a_row = 0;
                        stayed.insert(movement.from.clone());
                        self.hold(root, &movement.from, why);
                    }
                    Outcome::Failed(why) => {
                        in_a_row += 1;
                        stayed.insert(movement.from.clone());
                        self.fail(&movement.from, &why);
                        if in_a_row >= Self::MAX_CONSECUTIVE_FAILURES {
                            self.follow_all(library, &done).await;
                            moved.extend(done);
                            self.follow_playlists(root, moved, &work.run_id).await;
                            self.finish_with(
                                TidyStatus::Failed,
                                format!(
                                    "{in_a_row} files in a row could not be moved. Is the music folder read-only?"
                                ),
                            );
                            return None;
                        }
                    }
                    Outcome::JournalFailed(why) => {
                        self.follow_all(library, &done).await;
                        moved.extend(done);
                        self.follow_playlists(root, moved, &work.run_id).await;
                        self.finish_with(TidyStatus::Failed, journal_failed(&why));
                        return None;
                    }
                }
            }
            self.follow_all(library, &done).await;
            moved.extend(done);
            for copy in work.copies.iter().filter(|copy| copy.row == row) {
                let (journal, copy_for_copy, run_id) =
                    (self.journal.clone(), copy.clone(), work.run_id.clone());
                let outcome =
                    tokio::task::spawn_blocking(move || copy_cover(&copy_for_copy, &run_id, &journal))
                        .await
                        .unwrap_or_else(|panic| Outcome::Failed(panic.to_string()));
                match outcome {
                    Outcome::JournalFailed(why) => {
                        self.follow_playlists(root, moved, &work.run_id).await;
                        self.finish_with(TidyStatus::Failed, journal_failed(&why));
                        return None;
                    }
                    Outcome::Failed(why) => self.fail(&copy.to, &why),
                    _ => {}
                }
            }
            sources.sort();
            sources.dedup();
            for folder in sources {
                remove_empty_up(&folder, root, keep);
            }
        }

        // ---- the family pool, one folder at a time ----
        if let Some(stop) = self
            .move_pool_files(
                root,
                work,
                library,
                stopping,
                job,
                scan_start,
                (&mut *moved, linked),
            )
            .await
        {
            self.follow_playlists(root, moved, &work.run_id).await;
            return Some(stop);
        }
        self.store
            .update(|run| run.phase = "Updating playlists".to_string());
        self.follow_playlists(root, moved, &work.run_id).await;
        None
    }

    /// The pool's files, one folder at a time, each moved by the pool with every member's link
    /// while the job holds the gate whole, so no download, lyrics write or member placement sees
    /// a folder half moved; each journaled first and only while it is as Navidrome's last scan
    /// saw it. A folder whose files another pool change is working on waits for it: the job lets
    /// the gate go for [`Self::pool_retry`] and asks again, never waiting for a file while it holds
    /// the gate. Octo's records follow each folder's moves. Some when the run was stopped.
    #[allow(clippy::too_many_arguments)]
    async fn move_pool_files(
        &self,
        root: &str,
        work: &TidyWork,
        library: &dyn ILocalLibraryService,
        stopping: &CancellationToken,
        job: &mut LibraryJob,
        scan_start: DateTime<Utc>,
        (moved, linked): (&mut Vec<(String, String)>, &mut Vec<String>),
    ) -> Option<Stop> {
        let Some(pool) = self.services.family_pool.clone() else {
            for movement in &work.pool_moves {
                self.hold(root, &movement.from, "Family is not set up here now.".to_string());
            }
            return None;
        };
        if work.pool_moves.is_empty() {
            return None;
        }
        self.store
            .update(|run| run.phase = "Moving members' songs".to_string());
        let mut rows: Vec<&str> = Vec::new();
        for movement in &work.pool_moves {
            if !rows.contains(&movement.row.as_str()) {
                rows.push(&movement.row);
            }
        }
        for row in rows {
            let group: Vec<&TidyFileMove> = work.pool_moves.iter().filter(|m| m.row == row).collect();
            let paths: Vec<&str> = group
                .iter()
                .flat_map(|movement| [movement.from.as_str(), movement.to.as_str()])
                .collect();
            let held = match self.hold_pool_files(&pool, &paths, library, stopping, job).await {
                Ok(Some(held)) => held,
                Ok(None) => {
                    for movement in &group {
                        self.hold(root, &movement.from, SCANNING.to_string());
                    }
                    continue;
                }
                Err(stop) => return Some(stop),
            };
            let mut done: Vec<(String, String)> = Vec::new();
            for movement in group {
                if self.should_stop(stopping).is_some() {
                    break;
                }
                let there = std::fs::symlink_metadata(&movement.from).is_ok();
                if there {
                    let fresh = stamp_of(&movement.from).is_some_and(|(_, modified)| modified < scan_start);
                    if !fresh {
                        self.hold(root, &movement.from, CHANGED_SINCE_SCAN.to_string());
                        continue;
                    }
                    let entry = TidyJournalEntry::Move {
                        from: movement.from.clone(),
                        to: movement.to.clone(),
                        run: work.run_id.clone(),
                        at: Utc::now(),
                        file: true,
                        pool: true,
                    };
                    if let Err(failure) = self.journal.append(&entry) {
                        drop(held);
                        self.follow_records(library, &done).await;
                        moved.extend(done);
                        self.finish_with(TidyStatus::Failed, journal_failed(&failure.to_string()));
                        return None;
                    }
                }
                let remap = self.services.lyrics_remap.clone();
                match pool
                    .relayout_held(&held, &movement.from, &movement.to, remap)
                    .await
                {
                    Ok(RelayoutOutcome::Moved) => {
                        if there {
                            self.store.update(|run| run.moved_files += 1);
                        }
                        done.push((movement.from.clone(), movement.to.clone()));
                        // Each member's library holds the song at the new place now.
                        let names = pool
                            .store()
                            .get(&movement.to)
                            .map(|file| file.refs)
                            .unwrap_or_default();
                        for name in names {
                            if let (Some(was), Some(now)) = (
                                pool.link_path(&movement.from, &name),
                                pool.link_path(&movement.to, &name),
                            ) {
                                linked.push(was.clone());
                                done.push((was, now));
                            }
                        }
                    }
                    Ok(RelayoutOutcome::Held(words)) => self.hold(root, &movement.from, words),
                    Err(failure) => self.fail(&movement.from, &failure.to_string()),
                }
            }
            drop(held);
            // The pool told the lyrics jobs of each move as it made it.
            self.follow_records(library, &done).await;
            moved.extend(done);
        }
        None
    }

    /// Whether Navidrome is idle for the next group of moves: the job lets the gate go and waits
    /// while Navidrome scans.
    async fn idle_for_moves(
        &self,
        library: &dyn ILocalLibraryService,
        stopping: &CancellationToken,
        job: &mut LibraryJob,
    ) -> bool {
        job.pause();
        let idle = scan_wait::wait_while_scanning(library, self.timing, stopping).await;
        job.resume().await;
        idle
    }

    /// Holds the pool files at `paths` while the job holds the gate whole, once Navidrome is not
    /// scanning. While another pool change works on one of them (it may be waiting for the gate
    /// itself), the job lets the gate go for [`Self::pool_retry`] and asks again. None when
    /// Navidrome kept scanning; Err when the run was stopped.
    async fn hold_pool_files(
        &self,
        pool: &FamilyPool,
        paths: &[&str],
        library: &dyn ILocalLibraryService,
        stopping: &CancellationToken,
        job: &mut LibraryJob,
    ) -> Result<Option<PoolFilesHeld>, Stop> {
        loop {
            if let Some(stop) = self.should_stop(stopping) {
                return Err(stop);
            }
            if !self.idle_for_moves(library, stopping, job).await {
                return match self.should_stop(stopping) {
                    Some(stop) => Err(stop),
                    None => Ok(None),
                };
            }
            if let Some(held) = pool.try_hold(paths) {
                return Ok(Some(held));
            }
            job.pause();
            tokio::select! {
                () = tokio::time::sleep(self.pool_retry()) => {}
                () = stopping.cancelled() => {}
            }
        }
    }

    /// A Begin entry with nothing journaled after it for its run is taken out again, so Undo is
    /// not offered for a run that changed nothing.
    fn drop_lonely_begin(&self, run_id: &str) {
        let entries = self.journal.read_all();
        let of_run = entries.iter().filter(|entry| entry.run() == run_id).count();
        let lonely = of_run == 1
            && entries
                .iter()
                .any(|entry| matches!(entry, TidyJournalEntry::Begin { run, .. } if run == run_id));
        if lonely {
            let kept: Vec<TidyJournalEntry> = entries
                .into_iter()
                .filter(|entry| entry.run() != run_id)
                .collect();
            if let Err(failure) = self.journal.rewrite(&kept) {
                warn!("Library tidy could not tidy its journal: {failure}");
            }
        }
    }

    /// Holds a whole row at apply, with its folder.
    fn hold_row(&self, folder: &str, why: &str) {
        self.store.update(|run| {
            run.held += 1;
            run.held_rows.push(TidyRow {
                id: format!("held|{folder}"),
                kind: TidyKind::Skip,
                path: folder.to_string(),
                before: folder.to_string(),
                after: String::new(),
                evidence: why.to_string(),
                files: 0,
                opt_in: false,
                ticked: false,
            });
        });
    }

    /// Runs the test's hook in each playlist follow, on its blocking thread, under the playlists
    /// lock.
    #[cfg(test)]
    pub(crate) fn in_playlist_walk(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.playlist_walk_hook.lock() = Some(Arc::new(hook));
    }
}

/// The work of `plan`'s rows in `rows`, in the order they run.
fn work_of(run_id: &str, plan: &planner::TidyPlan, rows: &HashSet<String>) -> TidyWork {
    TidyWork {
        run_id: run_id.to_string(),
        task: TidyTask::FileLayout,
        moves: plan
            .moves
            .iter()
            .filter(|m| rows.contains(&m.row))
            .cloned()
            .collect(),
        file_moves: plan
            .file_moves
            .iter()
            .filter(|m| rows.contains(&m.row))
            .cloned()
            .collect(),
        copies: plan
            .copies
            .iter()
            .filter(|c| rows.contains(&c.row))
            .cloned()
            .collect(),
        pool_moves: plan
            .pool_moves
            .iter()
            .filter(|m| rows.contains(&m.row))
            .cloned()
            .collect(),
        ..TidyWork::default()
    }
}

/// `work` without the moves of `row`.
fn without_row(mut work: TidyWork, row: &str) -> TidyWork {
    work.moves.retain(|m| m.row != row);
    work.file_moves.retain(|m| m.row != row);
    work.copies.retain(|c| c.row != row);
    work.pool_moves.retain(|m| m.row != row);
    work
}

/// Folders to move, and songs to move (the files beside them not counted), the family pool's
/// songs included.
fn counts(moves: &[TidyMove], file_moves: &[TidyFileMove], pool_moves: &[TidyFileMove]) -> (usize, usize) {
    (
        moves.len(),
        file_moves
            .iter()
            .filter(|movement| movement.song.is_none())
            .count()
            + pool_moves.len(),
    )
}

/// The pool's plan beside the library's: its rows and pool moves added.
fn with_pool(
    mut planned: layout_pass::LayoutPlan,
    pool: Option<layout_pass::LayoutPlan>,
) -> layout_pass::LayoutPlan {
    if let Some(pool) = pool {
        planned.plan.rows.extend(pool.plan.rows);
        planned.plan.pool_moves = pool.plan.pool_moves;
        planned.row_hashes.extend(pool.row_hashes);
    }
    planned
}

/// One audio file as the File layout planner reads it.
fn layout_file(path: &str, scope: TidyScope, mapping: Option<LocalSongMapping>) -> LayoutFile {
    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let lidarr = mapping
        .as_ref()
        .is_some_and(|mapping| mapping.external_provider.eq_ignore_ascii_case("lidarr"));
    let octo = mapping.is_some() && !lidarr;
    LayoutFile {
        path: path.to_string(),
        size,
        in_scope: !lidarr && (scope == TidyScope::WholeLibrary || octo),
        tags: read_layout_tags(Path::new(path)),
        mapping: mapping.filter(|_| octo).map(|mapping| LayoutMapping {
            title: mapping.title,
            album: mapping.album,
            artist: mapping.artist,
            values: mapping.layout,
        }),
    }
}

/// The walk of the File layout task: every audio file under `root`, and each folder's listing.
/// Octo's own folders (`.octo-*`), `._*` files, the trash and the members folder are left out; a
/// name with one leading dot only when Octo's records name it; a folder with an `.ndignore` is
/// listed as ignored and not walked. Links are never followed.
fn walk_layout(
    root: &Path,
    mapped: &HashSet<String>,
    trash: &Path,
    members: &Path,
) -> std::io::Result<(Vec<String>, LayoutDisk)> {
    let mut disk = LayoutDisk::default();
    let mut audio = Vec::new();
    let named = |path: &str| {
        let inside = format!("{path}/");
        mapped.contains(path) || mapped.iter().any(|known| known.starts_with(&inside))
    };
    let list = |folder: &Path| -> std::io::Result<Vec<(String, bool, bool)>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(folder)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let kind = entry.file_type()?;
            names.push((name, kind.is_dir(), kind.is_symlink()));
        }
        names.sort();
        Ok(names)
    };
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        if folder != root && (library_walk::is_left_out(root, &folder, Some(trash)) || folder == members) {
            continue;
        }
        let names = list(&folder)?;
        let shown = folder.to_string_lossy().into_owned();
        let ignored = names
            .iter()
            .any(|(name, is_dir, _)| !is_dir && name == ".ndignore");
        disk.folders.insert(
            shown.clone(),
            names
                .iter()
                .map(|(name, is_dir, _)| (name.clone(), *is_dir))
                .collect(),
        );
        if ignored {
            disk.ignored.push(shown);
            continue;
        }
        for (name, is_dir, is_link) in names {
            if is_link || name.starts_with(".octo-") || name.starts_with("._") {
                continue;
            }
            let path = joined(&folder, &name);
            let path_text = path.to_string_lossy().into_owned();
            if PathHelper::is_hidden_name(&name) && !named(&path_text) {
                continue;
            }
            if is_dir {
                folders.push(path);
            } else if is_audio_name(&name) {
                audio.push(path_text);
            }
        }
    }
    audio.sort();
    Ok((audio, disk))
}

/// Moves one file, only while it is where the plan has it, as Navidrome's last scan saw it (an
/// audio file), and nothing at its target or any new folder above it is another spelling of a name
/// there.
fn move_one_file(
    movement: &TidyFileMove,
    scan_start: DateTime<Utc>,
    run_id: &str,
    journal: &TidyJournal,
) -> Outcome {
    let (from, to) = (Path::new(&movement.from), Path::new(&movement.to));
    let from_meta = std::fs::symlink_metadata(from);
    let to_there = std::fs::symlink_metadata(to).is_ok();
    match &from_meta {
        Err(_) if to_there => return Outcome::AlreadyDone,
        Err(_) => return Outcome::Held("It is no longer there.".to_string()),
        Ok(meta) if meta.file_type().is_symlink() || meta.is_dir() => {
            return Outcome::Held("It is a link or a folder, and the tidy only moves files.".to_string());
        }
        Ok(meta) => {
            let modified = meta.modified().ok().map(DateTime::<Utc>::from);
            if is_audio_name(&movement.from) && modified.is_none_or(|modified| modified >= scan_start) {
                return Outcome::Held(CHANGED_SINCE_SCAN.to_string());
            }
        }
    }
    if let Err(why) = target_is_free(&movement.to, &movement.from, true) {
        return Outcome::Held(why);
    }
    let Some(parent) = to.parent() else {
        return Outcome::Failed("The new file has no folder.".to_string());
    };
    if !same_device(from, parent) {
        return Outcome::Held(
            "The new folder would be on another disk, and the tidy only renames within one.".to_string(),
        );
    }
    let entry = TidyJournalEntry::Move {
        from: movement.from.clone(),
        to: movement.to.clone(),
        run: run_id.to_string(),
        at: Utc::now(),
        file: true,
        pool: false,
    };
    if let Err(failure) = journal.append(&entry) {
        return Outcome::JournalFailed(failure.to_string());
    }
    if let Err(failure) = std::fs::create_dir_all(parent).and_then(|()| std::fs::rename(from, to)) {
        return Outcome::Failed(failure.to_string());
    }
    Outcome::Done
}

/// Whether `to` (a file, or with `is_file` false a folder) is free on disk: no entry of another
/// spelling of its name in its folder (`from` itself aside, for a rename by capitals), and no
/// folder above it that has to be made has one in its parent. Err with the words that say what is
/// there.
fn target_is_free(to: &str, from: &str, is_file: bool) -> Result<(), String> {
    let target = Path::new(to);
    // A folder is there only by its own name in its parent's listing: on a disk that ignores
    // case, a folder of other capitals answers for it too.
    let there = |folder: &Path| match (folder.parent(), folder.file_name().and_then(|n| n.to_str())) {
        (Some(parent), Some(name)) => std::fs::read_dir(parent).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_str() == Some(name) && entry.path().is_dir())
        }),
        _ => folder.is_dir(),
    };
    let mut missing: Vec<&Path> = Vec::new();
    let mut existing = target.parent();
    while let Some(folder) = existing {
        if there(folder) {
            break;
        }
        missing.push(folder);
        existing = folder.parent();
    }
    if let Some(top) = missing.last()
        && let (Some(parent), Some(name)) = (top.parent(), top.file_name().and_then(|n| n.to_str()))
    {
        let folded = fold_name(name, None);
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let Some(there) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if there != name && fold_name(&there, None) == folded {
                    return Err(format!(
                        "{there} is already there with other capitals or characters, so these songs stay."
                    ));
                }
            }
        }
        return Ok(());
    }
    let (Some(folder), Some(name)) = (target.parent(), target.file_name().and_then(|n| n.to_str())) else {
        return Ok(());
    };
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let Some(there) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if there == name || PathHelper::same_name(&there, name, is_file) {
            let path = joined(folder, &there).to_string_lossy().into_owned();
            let same_file = std::fs::metadata(&path)
                .ok()
                .zip(std::fs::metadata(from).ok())
                .is_some_and(|(a, b)| same_file(&a, &b));
            if path != from && !same_file {
                return Err(if is_file {
                    format!("{there} is already there, so this song stays.")
                } else {
                    format!(
                        "{there} is already there with other capitals or characters, so these songs stay."
                    )
                });
            }
        }
    }
    Ok(())
}

/// Copies a cover into a folder an album moved into, journaled first; never over a file there.
fn copy_cover(copy: &TidyCopy, run_id: &str, journal: &TidyJournal) -> Outcome {
    let to = Path::new(&copy.to);
    if to.exists() {
        return Outcome::AlreadyDone;
    }
    if !Path::new(&copy.from).is_file() || !to.parent().is_some_and(Path::is_dir) {
        return Outcome::Held("The cover or its new folder is not there.".to_string());
    }
    let entry = TidyJournalEntry::Copy {
        from: copy.from.clone(),
        to: copy.to.clone(),
        run: run_id.to_string(),
        at: Utc::now(),
    };
    if let Err(failure) = journal.append(&entry) {
        return Outcome::JournalFailed(failure.to_string());
    }
    match std::fs::copy(&copy.from, to) {
        Ok(_) => Outcome::Done,
        Err(failure) => Outcome::Failed(failure.to_string()),
    }
}

/// Empty folders from `start` up, never `root`, never one of `keep`, never above `root`.
fn remove_empty_up(start: &str, root: &str, keep: &[String]) {
    let root = root.trim_end_matches('/');
    let mut folder = start.trim_end_matches('/').to_string();
    while folder.len() > root.len() && folder.starts_with(&format!("{root}/")) {
        if keep.iter().any(|kept| kept.trim_end_matches('/') == folder) {
            return;
        }
        let empty = std::fs::read_dir(&folder).is_ok_and(|mut entries| entries.next().is_none());
        if !empty || std::fs::remove_dir(&folder).is_err() {
            return;
        }
        folder = parent_dir(&folder);
    }
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(at) => path[..at].to_string(),
        None => String::new(),
    }
}

/// Where `path` is once every move of `moved`, in order, is done.
pub(crate) fn final_path(path: &str, moved: &[(String, String)]) -> String {
    let mut now = path.to_string();
    for (from, to) in moved {
        if let Some(moved) = PathHelper::moved_path(&now, from, to) {
            now = moved;
        }
    }
    now
}

/// The path a file had before the folder renames: a move inside a renamed folder names the file
/// where the rename put it.
fn final_origin(path: &str, renames: &[TidyMove]) -> String {
    for rename in renames {
        if let Some(before) = PathHelper::moved_path(path, &rename.to, &rename.from) {
            return before;
        }
    }
    path.to_string()
}

// ---- undo -----------------------------------------------------------------------------------

impl TidyWorker {
    /// Puts back what the journal holds for the last run (or every run): folders moved back
    /// first, newest first, each only where the old path is free and the songs are as Navidrome
    /// last saw them; the playlists; then a scan that saw the folders back; then the tags, each
    /// only on a file that still has the tags the tidy wrote. What cannot be put back stays in
    /// the journal for another Undo.
    async fn undo(&self, request: &TidyRequest, stopping: &CancellationToken) {
        let entries = self.journal.read_all();
        let last = entries.last().map(|entry| entry.run().to_string());
        let runs: HashSet<String> = if request.all_runs {
            entries.iter().map(|entry| entry.run().to_string()).collect()
        } else {
            last.clone().into_iter().collect()
        };
        // What the newest run did says what Undo puts back.
        let task = last
            .as_deref()
            .map(|run| journal::task_of(&entries, run))
            .unwrap_or_default();
        self.start(
            TidyMode::Undo,
            TidyScope::WholeLibrary,
            task,
            "Checking Navidrome",
        );
        if runs.is_empty() {
            self.finish_with(TidyStatus::Failed, "There is no tidy to undo.".to_string());
            return;
        }
        if !request.all_runs
            && let Some(seen) = &request.run
            && last.as_ref() != Some(seen)
        {
            self.finish_with(
                TidyStatus::Failed,
                "The last tidy changed since this page showed Undo, so nothing was put back. Look at what Undo puts back now."
                    .to_string(),
            );
            return;
        }
        let gate = self.check_navidrome(true).await;
        if !gate.apply_allowed {
            self.finish_with(TidyStatus::Failed, gate.reason.unwrap_or_default());
            return;
        }
        let Some(library) = self.services.library.clone() else {
            self.finish_with(
                TidyStatus::Failed,
                "Octo has no library service to ask Navidrome for scans.".to_string(),
            );
            return;
        };
        let mut job = match self.write_gate.begin_job(Self::JOB_NAME).await {
            Ok(job) => job,
            Err(holder) => {
                self.finish_with(TidyStatus::Failed, LibraryWriteGate::busy_message(&holder));
                return;
            }
        };
        let root = self.music_root();
        let chosen: Vec<usize> = (0..entries.len())
            .rev()
            .filter(|at| runs.contains(entries[*at].run()))
            .collect();
        self.store.update(|run| {
            run.planned_moves = chosen
                .iter()
                .filter(|at| matches!(entries[**at], TidyJournalEntry::Move { file: false, .. }))
                .count();
            run.planned_file_moves = chosen
                .iter()
                .filter(|at| matches!(&entries[**at], TidyJournalEntry::Move { file: true, to, .. } if is_audio_name(to)))
                .count();
            run.planned_retags = chosen
                .iter()
                .filter(|at| matches!(entries[**at], TidyJournalEntry::Tags { .. }))
                .count();
        });
        info!("Library tidy undo started: {} journal entries", chosen.len());

        let hold = library.scan_hold().map(|hold| hold.hold());
        let mut done: HashSet<usize> = HashSet::new();
        let stopped = self
            .put_back(
                &root,
                &entries,
                &chosen,
                &mut done,
                library.as_ref(),
                stopping,
                &mut job,
            )
            .await;

        let remaining = remaining(&entries, &done);
        if let Err(failure) = self.journal.rewrite(&remaining) {
            self.store.update(|run| {
                run.errors
                    .push(format!("The journal could not be updated: {failure}"))
            });
        }
        if let Some(mut plan) = self.store.plan()
            && plan.work.take().is_some()
            && let Err(failure) = self.store.save_plan(plan)
        {
            warn!("Library tidy could not forget the undone work: {failure}");
        }
        let wanted = hold.map(|hold| hold.release()).unwrap_or(false);
        if self.store.current().changed_anything() || wanted {
            library.start_scan().await;
        }
        if let Some(stop) = stopped {
            self.stopped(Some(stop));
            return;
        }
        let run = self.store.current();
        let mut summary = match task {
            TidyTask::Artists => format!(
                "Moved {} back and restored the tags on {}.",
                counted(run.moved, "album folder", "album folders"),
                counted(run.retagged, "file", "files")
            ),
            TidyTask::FileLayout => format!(
                "Moved {} and {} back where they were.",
                counted(run.moved, "folder", "folders"),
                counted(run.moved_files, "song", "songs")
            ),
        };
        if !remaining.iter().all(|entry| !runs.contains(entry.run())) {
            let left = "Some changes could not be undone yet (the old path is taken, the files changed, or Navidrome had not scanned them); they stay in the journal, so Undo again later.";
            summary.push(' ');
            summary.push_str(left);
            self.complete_with_summary(summary, Some(left.to_string()));
        } else {
            self.complete_with_summary(summary, None);
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_back(
        &self,
        root: &str,
        entries: &[TidyJournalEntry],
        chosen: &[usize],
        done: &mut HashSet<usize>,
        library: &dyn ILocalLibraryService,
        stopping: &CancellationToken,
        job: &mut LibraryJob,
    ) -> Option<Stop> {
        self.store
            .update(|run| run.phase = "Waiting for Navidrome to finish scanning".to_string());
        job.pause();
        let idle = scan_wait::wait_while_scanning(library, self.timing, stopping).await;
        job.resume().await;
        if !idle {
            if let Some(stop) = self.should_stop(stopping) {
                return Some(stop);
            }
            self.finish_with(
                TidyStatus::Failed,
                "Navidrome kept scanning, or did not answer, so nothing was put back. Undo again later."
                    .to_string(),
            );
            return None;
        }
        let Some(scan_start) = library
            .get_scan_status()
            .await
            .and_then(|status| status.last_scan_started_at())
        else {
            self.finish_with(TidyStatus::Failed, NO_STATUS.to_string());
            return None;
        };

        // ---- moves back, newest run first: its covers, its files (each song before the files
        // beside it), then its folders ----
        self.store
            .update(|run| run.phase = "Moving files back".to_string());
        let mut moved_back: Vec<(String, String)> = Vec::new();
        let keep = self.lidarr_folders(root, self.store.plan().and_then(|plan| plan.lidarr).as_ref());
        let mut runs: Vec<&str> = Vec::new();
        for at in chosen {
            if !runs.contains(&entries[*at].run()) {
                runs.push(entries[*at].run());
            }
        }
        for run_id in runs {
            let of_run: Vec<usize> = chosen
                .iter()
                .copied()
                .filter(|at| entries[*at].run() == run_id)
                .collect();
            for at in &of_run {
                let TidyJournalEntry::Copy { from, to, .. } = &entries[*at] else {
                    continue;
                };
                let (from_for, to_for) = (from.clone(), to.clone());
                let outcome = tokio::task::spawn_blocking(move || remove_copy(&from_for, &to_for))
                    .await
                    .unwrap_or_else(|panic| Outcome::Failed(panic.to_string()));
                match outcome {
                    Outcome::Done | Outcome::AlreadyDone => {
                        done.insert(*at);
                    }
                    Outcome::Held(why) => self.hold(root, to, why),
                    Outcome::Failed(why) | Outcome::JournalFailed(why) => self.fail(to, &why),
                }
            }
            // The family pool's files, each put back by the pool with every member's link while
            // the job holds the gate whole, the run's pool files held together first.
            let pool_back: Vec<(usize, &str, &str)> = of_run
                .iter()
                .filter_map(|at| match &entries[*at] {
                    TidyJournalEntry::Move {
                        from, to, pool: true, ..
                    } => Some((*at, from.as_str(), to.as_str())),
                    _ => None,
                })
                .collect();
            if !pool_back.is_empty() {
                match self.services.family_pool.clone() {
                    None => {
                        for (_, _, to) in &pool_back {
                            self.hold(root, to, "Family is not set up here now.".to_string());
                        }
                    }
                    Some(pool) => {
                        let paths: Vec<&str> =
                            pool_back.iter().flat_map(|(_, from, to)| [*from, *to]).collect();
                        let held = match self.hold_pool_files(&pool, &paths, library, stopping, job).await {
                            Ok(held) => held,
                            Err(stop) => return Some(stop),
                        };
                        for (at, from, to) in &pool_back {
                            if let Some(stop) = self.should_stop(stopping) {
                                return Some(stop);
                            }
                            let Some(held) = &held else {
                                self.hold(root, to, SCANNING.to_string());
                                continue;
                            };
                            let remap = self.services.lyrics_remap.clone();
                            match pool.relayout_held(held, to, from, remap).await {
                                Ok(RelayoutOutcome::Moved) => {
                                    done.insert(*at);
                                    self.follow_records(library, &[(to.to_string(), from.to_string())])
                                        .await;
                                    self.store.update(|run| run.moved_files += 1);
                                }
                                Ok(RelayoutOutcome::Held(words)) => self.hold(root, to, words),
                                Err(failure) => self.fail(to, &failure.to_string()),
                            }
                        }
                    }
                }
            }
            let files: Vec<usize> = of_run
                .iter()
                .copied()
                .filter(|at| {
                    matches!(
                        entries[*at],
                        TidyJournalEntry::Move {
                            file: true,
                            pool: false,
                            ..
                        }
                    )
                })
                .collect();
            let (songs, beside): (Vec<usize>, Vec<usize>) = files.into_iter().partition(
                |at| matches!(&entries[*at], TidyJournalEntry::Move { to, .. } if is_audio_name(to)),
            );
            let mut stayed: Vec<String> = Vec::new();
            for at in songs.iter().chain(&beside) {
                let TidyJournalEntry::Move { from, to, .. } = &entries[*at] else {
                    continue;
                };
                if let Some(stop) = self.should_stop(stopping) {
                    return Some(stop);
                }
                // A file beside a song goes back with it, never without it.
                let with_stuck_song = !is_audio_name(to)
                    && stayed.iter().any(|song| {
                        let (dir, stem) = (parent_dir(song), stem_of(song));
                        parent_dir(to) == dir && name_of_path(to).starts_with(&format!("{stem}."))
                    });
                if with_stuck_song {
                    continue;
                }
                let (back_from, back_to) = (to.clone(), from.clone());
                let outcome =
                    tokio::task::spawn_blocking(move || move_file_back(&back_from, &back_to, scan_start))
                        .await
                        .unwrap_or_else(|panic| Outcome::Failed(panic.to_string()));
                match outcome {
                    Outcome::Done => {
                        done.insert(*at);
                        self.follow(library, to, from).await;
                        remove_empty_up(&parent_dir(to), root, &keep);
                        moved_back.push((to.clone(), from.clone()));
                        if is_audio_name(to) {
                            self.store.update(|run| run.moved_files += 1);
                        }
                    }
                    Outcome::AlreadyDone => {
                        done.insert(*at);
                        self.follow(library, to, from).await;
                    }
                    Outcome::Held(why) => {
                        stayed.push(to.clone());
                        self.hold(root, to, why);
                    }
                    Outcome::Failed(why) | Outcome::JournalFailed(why) => {
                        stayed.push(to.clone());
                        self.fail(to, &why);
                    }
                }
            }
            for at in &of_run {
                let TidyJournalEntry::Move {
                    from,
                    to,
                    file: false,
                    pool: false,
                    ..
                } = &entries[*at]
                else {
                    continue;
                };
                if let Some(stop) = self.should_stop(stopping) {
                    return Some(stop);
                }
                let back = TidyMove {
                    from: to.clone(),
                    to: from.clone(),
                    row: String::new(),
                    files: Vec::new(),
                };
                let outcome = tokio::task::spawn_blocking(move || move_back(&back, scan_start))
                    .await
                    .unwrap_or_else(|panic| Outcome::Failed(panic.to_string()));
                match outcome {
                    Outcome::Done => {
                        done.insert(*at);
                        self.follow(library, to, from).await;
                        let parent = Path::new(to).parent().map(|p| p.to_string_lossy().into_owned());
                        BaseDownloadService::try_remove_empty_parents(parent.as_deref(), root);
                        moved_back.push((to.clone(), from.clone()));
                        self.store.update(|run| run.moved += 1);
                    }
                    Outcome::AlreadyDone => {
                        // Put back by an earlier undo that may have stopped before its records
                        // followed.
                        done.insert(*at);
                        self.follow(library, to, from).await;
                    }
                    Outcome::Held(why) => self.hold(root, to, why),
                    Outcome::Failed(why) | Outcome::JournalFailed(why) => self.fail(to, &why),
                }
            }
        }

        // ---- the playlists, under the lock every playlist change of the tidy takes ----
        let held_playlists = PLAYLISTS.lock().await;
        for at in chosen {
            let TidyJournalEntry::Playlist {
                path, before, run, ..
            } = &entries[*at]
            else {
                continue;
            };
            // The moves of the playlist's run, oldest first, as the run made them.
            let moves_of_run: Vec<(String, String)> = (0..entries.len())
                .filter_map(|index| match &entries[index] {
                    TidyJournalEntry::Move {
                        from, to, run: of, ..
                    } if of == run => Some((from.clone(), to.clone())),
                    _ => None,
                })
                .collect();
            let now = std::fs::read_to_string(path).ok();
            let wrote = rewrite_playlist(path, before, &moves_of_run);
            if now.is_some() && now == wrote {
                match state_file::write_atomic(Path::new(path), before.as_bytes()) {
                    Ok(()) => {
                        done.insert(*at);
                    }
                    Err(failure) => self.fail(path, &failure.to_string()),
                }
            } else if now.as_deref() == Some(before.as_str()) {
                done.insert(*at);
            } else {
                self.hold(
                    root,
                    path,
                    "This playlist changed since the tidy updated it, so it is left as it is.".to_string(),
                );
            }
        }
        drop(held_playlists);

        // ---- the scan between, then the tags ----
        let tags: Vec<usize> = chosen
            .iter()
            .copied()
            .filter(|at| matches!(entries[*at], TidyJournalEntry::Tags { .. }))
            .collect();
        if tags.is_empty() {
            return None;
        }
        if !moved_back.is_empty() {
            self.store
                .update(|run| run.phase = "Waiting for Navidrome to scan".to_string());
            let after = Utc::now();
            job.pause();
            if !library.start_scan().await {
                self.finish_with(
                    TidyStatus::Failed,
                    "Navidrome would not start a scan, so the tags were not restored. Undo again later."
                        .to_string(),
                );
                return None;
            }
            let scanned = scan_wait::wait_for_scan(library, after, self.timing, stopping).await;
            job.resume().await;
            match scanned {
                Ok(_) => {}
                Err(ScanWaitError::Cancelled) => {
                    return self.should_stop(stopping).or(Some(Stop::Cancelled));
                }
                Err(_) => {
                    self.finish_with(
                        TidyStatus::Failed,
                        "Navidrome did not finish a scan after the folders moved back, so the tags were not restored yet. Undo again once it has scanned."
                            .to_string(),
                    );
                    return None;
                }
            }
        }
        self.store.update(|run| run.phase = "Restoring tags".to_string());
        for at in tags {
            if let Some(stop) = self.should_stop(stopping) {
                return Some(stop);
            }
            let TidyJournalEntry::Tags {
                path, before, after, ..
            } = entries[at].clone()
            else {
                continue;
            };
            let shown = path.clone();
            let outcome = tokio::task::spawn_blocking(move || restore_tags(&path, &before, &after))
                .await
                .unwrap_or_else(|panic| Outcome::Failed(panic.to_string()));
            match outcome {
                Outcome::Done => {
                    done.insert(at);
                    self.store.update(|run| run.retagged += 1);
                }
                Outcome::AlreadyDone => {
                    done.insert(at);
                }
                Outcome::Held(why) => self.hold(root, &shown, why),
                Outcome::Failed(why) | Outcome::JournalFailed(why) => self.fail(&shown, &why),
            }
        }
        None
    }
}

/// Moves a folder back, only where the old path is free and its songs are as Navidrome last
/// saw them.
fn move_back(back: &TidyMove, scan_start: DateTime<Utc>) -> Outcome {
    let (from, to) = (Path::new(&back.from), Path::new(&back.to));
    let from_there = std::fs::symlink_metadata(from).is_ok();
    let to_there = std::fs::symlink_metadata(to).is_ok();
    if !from_there && to_there {
        return Outcome::AlreadyDone;
    }
    if !from_there {
        return Outcome::Held(format!("It is no longer at {}.", back.from));
    }
    if to_there {
        return Outcome::Held("Something else is at its old path now, so it stays where it is.".to_string());
    }
    let contents = match walk_all(from, &HashSet::new()) {
        Ok(contents) => contents,
        Err(failure) => return Outcome::Failed(failure.to_string()),
    };
    if contents.links {
        return Outcome::Held("It holds a link, and the tidy never moves one.".to_string());
    }
    for file in &contents.audio {
        match stamp_of(file) {
            Some((_, modified)) if modified < scan_start => {}
            _ => {
                return Outcome::Held(
                    "A song in it changed after Navidrome's last scan began. Undo again once Navidrome has scanned."
                        .to_string(),
                );
            }
        }
    }
    let Some(parent) = to.parent() else {
        return Outcome::Failed("The old folder has no parent.".to_string());
    };
    if let Err(failure) = std::fs::create_dir_all(parent).and_then(|()| std::fs::rename(from, to)) {
        return Outcome::Failed(failure.to_string());
    }
    Outcome::Done
}

/// Moves a file back, only where its old name (and any other spelling of it) is free and, for a
/// song, it is as Navidrome last saw it.
fn move_file_back(from: &str, to: &str, scan_start: DateTime<Utc>) -> Outcome {
    let from_there = std::fs::symlink_metadata(from).is_ok();
    let to_there = std::fs::symlink_metadata(to).is_ok();
    if !from_there && to_there {
        return Outcome::AlreadyDone;
    }
    if !from_there {
        return Outcome::Held(format!("It is no longer at {from}."));
    }
    if to_there || target_is_free(to, from, true).is_err() {
        return Outcome::Held("Something else is at its old path now, so it stays where it is.".to_string());
    }
    if is_audio_name(from) {
        match stamp_of(from) {
            Some((_, modified)) if modified < scan_start => {}
            _ => {
                return Outcome::Held(
                    "It changed after Navidrome's last scan began. Undo again once Navidrome has scanned."
                        .to_string(),
                );
            }
        }
    }
    let Some(parent) = Path::new(to).parent() else {
        return Outcome::Failed("The old folder has no parent.".to_string());
    };
    if let Err(failure) = std::fs::create_dir_all(parent).and_then(|()| std::fs::rename(from, to)) {
        return Outcome::Failed(failure.to_string());
    }
    Outcome::Done
}

/// Takes out a cover the tidy copied, only while it is still the copy.
fn remove_copy(from: &str, to: &str) -> Outcome {
    let Ok(copied) = std::fs::read(to) else {
        return Outcome::AlreadyDone;
    };
    let source = std::fs::read(from).ok();
    if source.as_deref() != Some(copied.as_slice()) {
        return Outcome::Held("This cover changed since the tidy copied it, so it stays.".to_string());
    }
    match std::fs::remove_file(to) {
        Ok(()) => Outcome::Done,
        Err(failure) => Outcome::Failed(failure.to_string()),
    }
}

fn name_of_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A file's name without its extension.
fn stem_of(path: &str) -> String {
    let name = name_of_path(path);
    match name.rfind('.') {
        Some(at) if at > 0 => name[..at].to_string(),
        _ => name.to_string(),
    }
}

/// Puts a file's artist tags back, only while they are still the ones the tidy wrote: a
/// different file now at that path, or tags someone edited since, are never overwritten.
fn restore_tags(path: &str, before: &TidyTags, after: &TidyTags) -> Outcome {
    let mut file = match TagFile::open(path) {
        Ok(file) => file,
        Err(_) if !Path::new(path).exists() => {
            return Outcome::Held("It is not at the path it was re-tagged at.".to_string());
        }
        Err(failure) => return Outcome::Failed(failure.to_string()),
    };
    let now = read_tags(&file);
    if now == *before {
        return Outcome::AlreadyDone;
    }
    if now != *after {
        return Outcome::Held(
            "Its artist tags are not the ones the tidy wrote, so they are left as they are.".to_string(),
        );
    }
    write_tags(&mut file, after, before);
    match file.save() {
        Ok(()) => Outcome::Done,
        Err(failure) => Outcome::Failed(failure.to_string()),
    }
}

/// The journal entries an undo did not put back, oldest first. A tag restore supersedes every
/// NEWER tag entry for the same file: the older entry's before is the original, and keeping a
/// newer one would put an in-between value back next time. A stamp goes with its file's tags.
pub(crate) fn remaining(oldest_first: &[TidyJournalEntry], done: &HashSet<usize>) -> Vec<TidyJournalEntry> {
    let restored: Vec<(usize, &str)> = done
        .iter()
        .filter_map(|at| match &oldest_first[*at] {
            TidyJournalEntry::Tags { path, .. } => Some((*at, path.as_str())),
            _ => None,
        })
        .collect();
    let superseded = |at: usize, path: &str| {
        restored
            .iter()
            .any(|(restored_at, restored_path)| *restored_at <= at && *restored_path == path)
    };
    let kept: Vec<TidyJournalEntry> = oldest_first
        .iter()
        .enumerate()
        .filter(|(at, entry)| {
            if done.contains(at) {
                return false;
            }
            match entry {
                TidyJournalEntry::Tags { path, .. } | TidyJournalEntry::Stamp { path, .. } => {
                    !superseded(*at, path)
                }
                _ => true,
            }
        })
        .map(|(_, entry)| entry.clone())
        .collect();
    // A run's first line goes once nothing else of the run is left.
    let left: HashSet<String> = kept
        .iter()
        .filter(|entry| !matches!(entry, TidyJournalEntry::Begin { .. }))
        .map(|entry| entry.run().to_string())
        .collect();
    kept.into_iter()
        .filter(|entry| !matches!(entry, TidyJournalEntry::Begin { run, .. } if !left.contains(run)))
        .collect()
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
