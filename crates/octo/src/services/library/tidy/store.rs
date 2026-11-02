//! The tidy's state on disk: the run the dashboard shows (`tidy-run.json`) and the whole plan
//! of the last preview (`tidy-plan.json`), with the MusicBrainz answers it was made from, so
//! an Apply after a restart does exactly what the preview showed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::planner::{TidyAnswers, TidyCopy, TidyFileMove, TidyMove, TidyPlan, TidyRetag, TidyRow};
use crate::services::state_file;

/// What a tidy does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TidyTask {
    /// Artist folders, spellings and guests.
    #[default]
    Artists,
    /// Folders and file names moved into the file layout.
    FileLayout,
}

impl TidyTask {
    pub fn name(self) -> &'static str {
        match self {
            TidyTask::Artists => "Artists",
            TidyTask::FileLayout => "FileLayout",
        }
    }

    pub fn parse(value: Option<&str>) -> TidyTask {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("FileLayout") => TidyTask::FileLayout,
            _ => TidyTask::Artists,
        }
    }
}

/// The file layout a File layout preview was made with: the structure and the three templates'
/// texts (albums, singles, compilations).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TidyLayout {
    pub structure: String,
    pub templates: Vec<String>,
}

/// What Lidarr said when a File layout preview asked: its root folders, its artists' folders
/// and every track file, each in Lidarr's own paths.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TidyLidarr {
    pub roots: Vec<String>,
    /// Lidarr's id and folder of each artist.
    pub artists: Vec<(i32, String)>,
    /// Each track file's path and size in bytes.
    pub files: Vec<(String, u64)>,
}

/// Which files a run may change. Every file is read either way: how an artist is spelled is
/// decided by the whole library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TidyScope {
    /// Files Octo downloaded, from its mappings.
    #[default]
    OctoDownloads,
    /// Every audio file under the music folder. Apply asks for the path typed back.
    WholeLibrary,
}

impl TidyScope {
    pub fn name(self) -> &'static str {
        match self {
            TidyScope::OctoDownloads => "OctoDownloads",
            TidyScope::WholeLibrary => "WholeLibrary",
        }
    }

    pub fn parse(value: Option<&str>) -> TidyScope {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("WholeLibrary") => TidyScope::WholeLibrary,
            _ => TidyScope::OctoDownloads,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TidyStatus {
    #[default]
    Idle,
    Running,
    Completed,
    Cancelled,
    /// Octo stopped mid-run. Resume carries on with the same plan.
    Interrupted,
    /// Stopped on purpose: a refused precondition, a journal that could not be written, too
    /// many failures, or a Navidrome scan that never came.
    Failed,
}

impl TidyStatus {
    pub fn name(self) -> &'static str {
        match self {
            TidyStatus::Idle => "Idle",
            TidyStatus::Running => "Running",
            TidyStatus::Completed => "Completed",
            TidyStatus::Cancelled => "Cancelled",
            TidyStatus::Interrupted => "Interrupted",
            TidyStatus::Failed => "Failed",
        }
    }
}

/// What a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TidyMode {
    #[default]
    Preview,
    Apply,
    Undo,
}

impl TidyMode {
    pub fn name(self) -> &'static str {
        match self {
            TidyMode::Preview => "Preview",
            TidyMode::Apply => "Apply",
            TidyMode::Undo => "Undo",
        }
    }
}

/// The run the dashboard polls.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TidyRun {
    pub run_id: String,
    pub status: TidyStatus,
    pub mode: TidyMode,
    pub scope: TidyScope,
    pub task: TidyTask,
    /// What the run is doing now, in words the dashboard shows as they are.
    pub phase: String,
    pub started_utc: Option<DateTime<Utc>>,
    pub finished_utc: Option<DateTime<Utc>>,
    /// Audio files read, and how many of them were in the run.
    pub files: usize,
    pub in_scope: usize,
    /// What the plan holds with the rows as ticked.
    pub planned_retags: usize,
    /// Folders to move or rename, and files to move or rename.
    pub planned_moves: usize,
    pub planned_file_moves: usize,
    /// What an apply, resume or undo actually did: folders moved, and files moved.
    pub retagged: usize,
    pub moved: usize,
    pub moved_files: usize,
    pub held: usize,
    pub failed: usize,
    pub lookups_asked: usize,
    pub lookups_not_asked: usize,
    /// The fingerprint of the preview's plan with every opt-in row ticked. An apply plans
    /// again and refuses when the library no longer gives the same one.
    pub plan_hash: Option<String>,
    pub reason: Option<String>,
    pub errors: Vec<String>,
    /// What an apply, resume or undo left alone at the last moment, and why.
    pub held_rows: Vec<TidyRow>,
}

impl TidyRun {
    /// A preview that finished and can be applied.
    pub fn is_finished_preview(&self) -> bool {
        self.status == TidyStatus::Completed && self.mode == TidyMode::Preview && self.plan_hash.is_some()
    }

    /// An apply stopped part way, which Resume carries on.
    pub fn can_resume(&self) -> bool {
        self.mode == TidyMode::Apply
            && matches!(
                self.status,
                TidyStatus::Interrupted | TidyStatus::Cancelled | TidyStatus::Failed
            )
    }

    /// Folders and files the run changed, both counted.
    pub fn changed_anything(&self) -> bool {
        self.retagged > 0 || self.moved > 0 || self.moved_files > 0
    }
}

/// What an apply was asked to do, kept so Resume finishes the same work.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TidyWork {
    pub run_id: String,
    pub task: TidyTask,
    pub ticked: Vec<String>,
    pub retags: Vec<TidyRetag>,
    /// Whole folders moved or renamed.
    pub moves: Vec<TidyMove>,
    /// Files moved or renamed, each sidecar right after its song.
    pub file_moves: Vec<TidyFileMove>,
    /// Covers copied into a folder an album moved into.
    pub copies: Vec<TidyCopy>,
    /// Family pool files moved, with every member's link.
    pub pool_moves: Vec<TidyFileMove>,
}

impl TidyWork {
    /// Whether the work writes and moves nothing.
    pub fn is_empty(&self) -> bool {
        self.retags.is_empty()
            && self.moves.is_empty()
            && self.file_moves.is_empty()
            && self.pool_moves.is_empty()
            && self.copies.is_empty()
    }
}

/// The last preview's plan, whole.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TidyPlanFile {
    pub root: String,
    pub scope: TidyScope,
    pub task: TidyTask,
    pub excluded: Vec<String>,
    pub case_insensitive: bool,
    pub plan: TidyPlan,
    pub answers: TidyAnswers,
    /// The file layout a File layout preview was made with.
    pub layout: Option<TidyLayout>,
    /// Each File layout row's fingerprint, by row id: an apply runs only the rows whose
    /// fingerprint is the same again.
    pub row_hashes: BTreeMap<String, String>,
    /// What Lidarr said for a File layout preview, when Lidarr is set up.
    pub lidarr: Option<TidyLidarr>,
    /// The work of the last apply, while one is under way or was stopped.
    pub work: Option<TidyWork>,
}

/// The run and the plan, each written whole with a temporary file and a rename.
pub struct TidyStore {
    run_path: Option<PathBuf>,
    plan_path: Option<PathBuf>,
    run: Mutex<TidyRun>,
    plan: Mutex<Option<TidyPlanFile>>,
}

impl TidyStore {
    /// Rows the run keeps of what it held back; the plan file keeps every row.
    pub const MAX_HELD_ROWS: usize = 500;
    const MAX_ERRORS: usize = 20;

    /// The store beside the settings; None for both keeps it in memory (tests).
    pub fn new(run_path: Option<PathBuf>, plan_path: Option<PathBuf>) -> Self {
        let mut run: TidyRun = run_path.as_deref().and_then(load).unwrap_or_default();
        if run.status == TidyStatus::Running {
            run.status = TidyStatus::Interrupted;
            run.phase = String::new();
            run.reason = Some(
                "Octo restarted while this run was in progress. Resume carries on where it stopped."
                    .to_string(),
            );
        }
        let plan = plan_path.as_deref().and_then(load);
        Self {
            run_path,
            plan_path,
            run: Mutex::new(run),
            plan: Mutex::new(plan),
        }
    }

    /// The run the dashboard shows: the last one asked for there.
    pub fn current(&self) -> TidyRun {
        self.run.lock().clone()
    }

    pub fn update(&self, change: impl FnOnce(&mut TidyRun)) {
        {
            let mut run = self.run.lock();
            change(&mut run);
            if run.errors.len() > Self::MAX_ERRORS {
                let extra = run.errors.len() - Self::MAX_ERRORS;
                run.errors.drain(..extra);
            }
            run.held_rows.truncate(Self::MAX_HELD_ROWS);
        }
        self.flush();
    }

    pub fn replace(&self, run: TidyRun) {
        *self.run.lock() = run;
        self.flush();
    }

    pub fn plan(&self) -> Option<TidyPlanFile> {
        self.plan.lock().clone()
    }

    /// Keeps the plan, on disk too. An error means it could not be written.
    pub fn save_plan(&self, plan: TidyPlanFile) -> std::io::Result<()> {
        let written = match &self.plan_path {
            Some(path) => state_file::write_atomic(
                path,
                serde_json::to_string(&plan)
                    .map_err(std::io::Error::other)?
                    .as_bytes(),
            ),
            None => Ok(()),
        };
        *self.plan.lock() = Some(plan);
        written
    }

    /// Writes the run. The run is small; it is written on every change, so the dashboard and
    /// a restart see where it is.
    pub fn flush(&self) {
        let Some(path) = &self.run_path else {
            return;
        };
        let json = match serde_json::to_string(&*self.run.lock()) {
            Ok(json) => json,
            Err(error) => {
                warn!("tidy run could not be serialized: {error}");
                return;
            }
        };
        if let Err(error) = state_file::write_atomic(path, json.as_bytes()) {
            warn!("tidy run could not be written: {error}");
        }
    }
}

fn load<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Option<T> {
    match state_file::read_text(path) {
        Ok(Some(text)) if !text.trim().is_empty() => match serde_json::from_str(&text) {
            Ok(value) => Some(value),
            Err(error) => {
                warn!("{} could not be read: {error}", path.display());
                None
            }
        },
        Ok(_) => None,
        Err(error) => {
            warn!("{} could not be read: {error}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_that_was_going_when_octo_stopped_reads_back_interrupted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let run_path = dir.path().join("tidy-run.json");
        let plan_path = dir.path().join("tidy-plan.json");
        let store = TidyStore::new(Some(run_path.clone()), Some(plan_path.clone()));
        store.replace(TidyRun {
            run_id: "r1".into(),
            status: TidyStatus::Running,
            mode: TidyMode::Apply,
            ..TidyRun::default()
        });
        store
            .save_plan(TidyPlanFile {
                root: "/music".into(),
                ..TidyPlanFile::default()
            })
            .expect("written");

        let again = TidyStore::new(Some(run_path), Some(plan_path));
        let run = again.current();
        assert_eq!(run.status, TidyStatus::Interrupted);
        assert!(run.can_resume());
        assert_eq!(again.plan().expect("kept").root, "/music");
    }

    /// The run, plan and journal an Octo from before the File layout task wrote read back as the
    /// Artists task's, with their folder moves.
    #[test]
    fn files_from_before_the_file_layout_task_read_as_artists_with_folder_moves() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let run_path = dir.path().join("tidy-run.json");
        let plan_path = dir.path().join("tidy-plan.json");
        std::fs::write(
            &run_path,
            r#"{"runId":"r1","status":"Interrupted","mode":"Apply","scope":"WholeLibrary","phase":"","startedUtc":null,"finishedUtc":null,"files":2,"inScope":2,"plannedRetags":0,"plannedMoves":1,"retagged":0,"moved":0,"held":0,"failed":0,"lookupsAsked":0,"lookupsNotAsked":0,"planHash":"ABC","reason":null,"errors":[],"heldRows":[]}"#,
        )
        .expect("written");
        std::fs::write(
            &plan_path,
            r#"{"root":"/music","scope":"WholeLibrary","excluded":[],"caseInsensitive":false,"plan":{"rows":[{"id":"folder|Beyonce","kind":"Respell","path":"Beyonce","before":"Beyonce","after":"Beyoncé","evidence":"","files":1,"opt_in":false,"ticked":true}],"retags":[],"moves":[{"from":"/music/Beyonce/A","to":"/music/Beyoncé/A","row":"folder|Beyonce","files":["/music/Beyonce/A/01 - S.flac"]}],"wanted_artists":[],"wanted_recordings":[]},"answers":{"artists":{},"credits":{}},"work":{"runId":"r1","ticked":[],"retags":[],"moves":[{"from":"/music/Beyonce/A","to":"/music/Beyoncé/A","row":"folder|Beyonce","files":["/music/Beyonce/A/01 - S.flac"]}]}}"#,
        )
        .expect("written");
        let journal_path = dir.path().join("tidy-journal.jsonl");
        std::fs::write(
            &journal_path,
            "{\"kind\":\"move\",\"from\":\"/music/Beyonce/A\",\"to\":\"/music/Beyonc\u{e9}/A\",\"run\":\"r1\",\"at\":\"2026-10-28T00:00:00Z\"}\n",
        )
        .expect("written");

        let store = TidyStore::new(Some(run_path), Some(plan_path));
        let run = store.current();
        assert_eq!(run.task, TidyTask::Artists);
        assert!(run.can_resume());
        let plan = store.plan().expect("read");
        assert_eq!(plan.task, TidyTask::Artists);
        assert_eq!(plan.plan.moves.len(), 1);
        assert!(plan.plan.file_moves.is_empty());
        let work = plan.work.expect("the work");
        assert_eq!(work.task, TidyTask::Artists);
        assert_eq!(work.moves[0].to, "/music/Beyonc\u{e9}/A");
        let entries = super::super::journal::TidyJournal::new(Some(journal_path)).read_all();
        assert!(matches!(
            &entries[..],
            [super::super::journal::TidyJournalEntry::Move {
                file: false,
                pool: false,
                ..
            }]
        ));
    }
}
