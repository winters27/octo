//! Everything Octo is working on, in one shape, for the dashboard's progress toasts: a library
//! re-tag, a covers or lyrics run, better copies, hearted downloads, an import's downloads, a
//! duplicate scan, an slskd rescan and Navidrome's own scan. The dashboard asks often while
//! something runs and seldom while nothing does, so one cheap read replaces a poller per page.
//!
//! A job that needs the Navidrome sign-in on its own page is listed only for a signed-in
//! browser. A job just finished stays listed for [`RECENT_WINDOW`], so a toast can say how it
//! ended. Behind the same guard as the rest of `/api/admin`.
//!
//! Some jobs are read through small views of their own ([`FindLook`], [`ImportsActivity`],
//! [`HealthActivity`], [`RescanReport`]).

use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::get;
use chrono::{DateTime, TimeDelta, Utc};
use octo_core::json::datetime;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::lyrics::{LyricsLibraryMode, LyricsLibraryRun, LyricsLibraryStatus};
use octo_core::soulseek::soulseek_sharing::SharingReport;
use serde::Serialize;

use super::plumbing::{BROWSE_COOKIE_NAME, BROWSE_TOKEN_HEADER, header_value, request_cookie};
use super::request_binding::ok;
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::common::acquisition_tracker::{AcquisitionSnapshot, AcquisitionState};
use crate::services::common::song_finder::FindSnapshot;
use crate::services::cover_art::{CoverUpgradeMode, CoverUpgradeRun, CoverUpgradeStatus};
use crate::services::health::HealthRun;
use crate::services::imports::import_views::ImportOverview;
use crate::services::library::duplicate_scan_worker::DuplicateScanResult;
use crate::services::library::tidy::{TidyMode, TidyRun, TidyStatus, TidyTask};
use crate::services::library::upgrade_queue::{UpgradeJob, UpgradeStates};
use crate::services::metadata::genre_backfill_state::{GenreBackfillRun, GenreBackfillStatus};

pub fn routes() -> RouteSet {
    RouteSet::new().route("/api/admin/activity", get(get_activity))
}

/// How long a finished job stays listed.
pub const RECENT_WINDOW: TimeDelta = TimeDelta::minutes(2);
/// How long after a rescan from the dashboard slskd's share scan is watched.
pub const RESCAN_WATCH: TimeDelta = TimeDelta::minutes(15);
/// slskd starts a rescan a moment after it is asked, so the first seconds read as starting.
pub const RESCAN_START: TimeDelta = TimeDelta::seconds(15);

/// One job. `state` is running, done, stopped (canceled or interrupted) or failed. `done` of
/// `total` counts `unit`; `fraction` is set when only a share is known. `page` is the dashboard
/// page that shows it in full.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityItem {
    pub id: String,
    pub kind: &'static str,
    pub page: &'static str,
    pub title: String,
    pub state: &'static str,
    pub done: Option<i32>,
    pub total: Option<i32>,
    pub unit: Option<&'static str>,
    pub fraction: Option<f64>,
    pub detail: Option<String>,
    #[serde(with = "datetime::utc_option")]
    pub started_utc: Option<DateTime<Utc>>,
    #[serde(with = "datetime::utc_option")]
    pub finished_utc: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

impl ActivityItem {
    /// The record's required members; the rest default to null, as its optional parameters did.
    fn new(
        id: impl Into<String>,
        kind: &'static str,
        page: &'static str,
        title: impl Into<String>,
        state: &'static str,
    ) -> Self {
        ActivityItem {
            id: id.into(),
            kind,
            page,
            title: title.into(),
            state,
            done: None,
            total: None,
            unit: None,
            fraction: None,
            detail: None,
            started_utc: None,
            finished_utc: None,
            error: None,
        }
    }

    fn counted(mut self, done: i32, total: i32, unit: &'static str) -> Self {
        self.done = Some(done);
        self.total = Some(total);
        self.unit = Some(unit);
        self
    }

    fn detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }

    fn started(mut self, started: Option<DateTime<Utc>>) -> Self {
        self.started_utc = started;
        self
    }

    fn finished(mut self, finished: Option<DateTime<Utc>>) -> Self {
        self.finished_utc = finished;
        self
    }

    fn error(mut self, error: Option<String>) -> Self {
        self.error = error;
        self
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivityAnswer {
    items: Vec<ActivityItem>,
    signed_in: bool,
}

async fn get_activity(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    // The cookie first, the X-Octo-Browse-Token header second.
    let browse = request_cookie(&parts.headers, BROWSE_COOKIE_NAME)
        .or_else(|| header_value(&parts.headers, BROWSE_TOKEN_HEADER));
    let browse = browse.as_deref();
    let sessions = &state.browse_sessions;
    let signed = sessions.validate(browse);
    let now = Utc::now();
    let mut items = Vec::new();

    if signed {
        let genre_worker = &state.genre_backfill_worker;
        items.extend(genre(&genre_worker.current(), genre_worker.is_pending(), now));
        let tidy_worker = &state.tidy_worker;
        items.extend(tidy(&tidy_worker.current(), tidy_worker.is_pending(), now));
        let covers_worker = &state.cover_upgrade_worker;
        items.extend(covers(&covers_worker.current(), covers_worker.is_busy(), now));
        let lyrics_worker = &state.lyrics_library_worker;
        items.extend(lyrics(&lyrics_worker.current(), lyrics_worker.is_running(), now));
        items.extend(upgrades(&state.upgrade_queue.snapshot(), now));
        if let Some(user) = sessions.user_of(browse) {
            let overview = state.imports.overview(&user);
            items.extend(imports(&ImportsActivity::from(&overview)));
        }
        if let Some(admin) = sessions.navidrome_user_of(browse) {
            let looks: Vec<FindLook> = state
                .song_finder
                .for_user(&admin)
                .iter()
                .map(FindLook::from)
                .collect();
            items.extend(find(&looks, now));
        }
        let library_health = &state.library_health;
        let run = library_health.run().as_deref().map(HealthActivity::from);
        items.extend(health(run.as_ref(), library_health.checking_since(), now));
    }
    items.extend(downloads(&state.acquisition_tracker.all(), now));
    let worker = &state.duplicate_scan_worker;
    items.extend(duplicates(
        &DuplicateScan {
            scanning: worker.is_scanning(),
            requested: worker.is_requested(),
            started_utc: worker.scan_started_utc(),
            last: worker.last_result(),
        },
        now,
    ));
    if let Some(requested) = state.soulseek_sharing.last_rescan_utc()
        && now - requested < RESCAN_WATCH
    {
        // slskd away is a report that says so (the report never fails): the Soulseek page tells
        // the rest.
        let report = state.soulseek_sharing.report(false, None).await;
        items.push(rescan(&RescanReport::from(&report), requested, now));
    }
    if let Some(scan) = state.local_library.get_scan_status().await
        && scan.scanning
    {
        let mut item = ActivityItem::new(
            "navidrome-scan",
            "scan",
            "library",
            "Navidrome is scanning your library",
            "running",
        );
        item.done = scan.count;
        item.unit = Some("songs");
        items.push(item);
    }

    ok(&ActivityAnswer {
        items,
        signed_in: signed,
    })
}

/// `Ended`: how a run that stopped running ended, from its status's name.
pub fn ended(status: &str) -> &'static str {
    match status {
        "Completed" => "done",
        "Failed" => "failed",
        _ => "stopped",
    }
}

fn recent(finished: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    finished.is_some_and(|f| now - f < RECENT_WINDOW)
}

/// A whole number with its thousands separated by commas.
fn n0(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if value < 0 { format!("-{out}") } else { out }
}

fn genre_status_name(status: GenreBackfillStatus) -> &'static str {
    match status {
        GenreBackfillStatus::Idle => "Idle",
        GenreBackfillStatus::Running => "Running",
        GenreBackfillStatus::Completed => "Completed",
        GenreBackfillStatus::Cancelled => "Cancelled",
        GenreBackfillStatus::Interrupted => "Interrupted",
        GenreBackfillStatus::Failed => "Failed",
    }
}

fn covers_status_name(status: CoverUpgradeStatus) -> &'static str {
    match status {
        CoverUpgradeStatus::Idle => "Idle",
        CoverUpgradeStatus::Running => "Running",
        CoverUpgradeStatus::Completed => "Completed",
        CoverUpgradeStatus::Cancelled => "Cancelled",
        CoverUpgradeStatus::Interrupted => "Interrupted",
        CoverUpgradeStatus::Failed => "Failed",
    }
}

fn lyrics_status_name(status: LyricsLibraryStatus) -> &'static str {
    match status {
        LyricsLibraryStatus::Idle => "Idle",
        LyricsLibraryStatus::Running => "Running",
        LyricsLibraryStatus::Completed => "Completed",
        LyricsLibraryStatus::Cancelled => "Cancelled",
        LyricsLibraryStatus::Interrupted => "Interrupted",
        LyricsLibraryStatus::Failed => "Failed",
    }
}

/// A library re-tag. `pending` is a run accepted and still reading the library before it turns
/// Running.
pub fn genre(run: &GenreBackfillRun, pending: bool, now: DateTime<Utc>) -> Option<ActivityItem> {
    let title = if run.dry_run {
        "Previewing genre changes"
    } else {
        "Re-tagging genres"
    };
    if pending && run.status != GenreBackfillStatus::Running {
        return Some(
            ActivityItem::new("genre:pending", "genre", "tags", title, "running")
                .detail(Some("Reading your library".to_string())),
        );
    }
    let id = format!("genre:{}", run.run_id);
    if run.status == GenreBackfillStatus::Running {
        return Some(
            ActivityItem::new(id, "genre", "tags", title, "running")
                .counted(run.processed, run.total, "songs")
                .detail((run.changed > 0).then(|| format!("{} changed so far", n0(run.changed.into()))))
                .started(run.started_utc),
        );
    }
    if run.status == GenreBackfillStatus::Idle || !recent(run.finished_utc, now) {
        return None;
    }
    let state = ended(genre_status_name(run.status));
    let detail = if state == "done" {
        Some(if run.dry_run {
            format!("{} songs would change", n0(run.changed.into()))
        } else {
            format!("{} songs re-tagged", n0(run.changed.into()))
        })
    } else {
        run.reason.clone()
    };
    Some(
        ActivityItem::new(id, "genre", "tags", title, state)
            .counted(run.processed, run.total, "songs")
            .detail(detail)
            .started(run.started_utc)
            .finished(run.finished_utc)
            .error(if state == "failed" {
                run.reason.clone()
            } else {
                None
            }),
    )
}

/// A library tidy run, its words from its task: a preview, an apply, or an undo.
pub fn tidy(run: &TidyRun, pending: bool, now: DateTime<Utc>) -> Option<ActivityItem> {
    let title = match (run.task, run.mode) {
        (TidyTask::Artists, TidyMode::Preview) => "Previewing the artist tidy",
        (TidyTask::Artists, TidyMode::Apply) => "Tidying artists",
        (TidyTask::Artists, TidyMode::Undo) => "Undoing the artist tidy",
        (TidyTask::FileLayout, TidyMode::Preview) => "Previewing the File layout tidy",
        (TidyTask::FileLayout, TidyMode::Apply) => "Moving songs into the File layout",
        (TidyTask::FileLayout, TidyMode::Undo) => "Undoing the File layout tidy",
    };
    if pending && run.status != TidyStatus::Running {
        return Some(
            ActivityItem::new("tidy:pending", "tidy", "tags", "The library tidy", "running")
                .detail(Some("Starting".to_string())),
        );
    }
    let id = format!("tidy:{}", run.run_id);
    if run.status == TidyStatus::Running {
        return Some(
            ActivityItem::new(id, "tidy", "tags", title, "running")
                .detail(Some(run.phase.clone()).filter(|phase| !phase.is_empty()))
                .started(run.started_utc),
        );
    }
    if run.status == TidyStatus::Idle || !recent(run.finished_utc, now) {
        return None;
    }
    let state = ended(run.status.name());
    Some(
        ActivityItem::new(id, "tidy", "tags", title, state)
            .detail(run.reason.clone())
            .started(run.started_utc)
            .finished(run.finished_utc)
            .error(if state == "failed" {
                run.reason.clone()
            } else {
                None
            }),
    )
}

/// A covers run: albums while looking covers up, folders while scanning.
pub fn covers(run: &CoverUpgradeRun, busy: bool, now: DateTime<Utc>) -> Option<ActivityItem> {
    let title = if run.undo {
        "Putting covers back"
    } else {
        match run.mode {
            CoverUpgradeMode::Scan => "Looking for soft covers",
            CoverUpgradeMode::Preview => "Finding better covers",
            _ => "Replacing covers",
        }
    };
    let by_album = run.mode != CoverUpgradeMode::Scan && run.albums_total > 0;
    let (done, total) = if by_album {
        (run.albums_done, run.albums_total)
    } else {
        (run.processed, run.total)
    };
    let unit = if by_album { "albums" } else { "folders" };
    if busy && run.status != CoverUpgradeStatus::Running {
        return Some(
            ActivityItem::new("covers:pending", "covers", "covers", title, "running")
                .detail(Some("Getting started".to_string())),
        );
    }
    let id = format!("covers:{}", run.run_id);
    if run.status == CoverUpgradeStatus::Running {
        return Some(
            ActivityItem::new(id, "covers", "covers", title, "running")
                .counted(done, total, unit)
                .started(run.started_utc),
        );
    }
    if run.status == CoverUpgradeStatus::Idle || !recent(run.finished_utc, now) {
        return None;
    }
    let state = ended(covers_status_name(run.status));
    let detail = if state != "done" {
        run.reason.clone()
    } else if run.undo {
        Some("The old covers are back".to_string())
    } else {
        Some(match run.mode {
            CoverUpgradeMode::Scan if run.soft == 0 => "Every cover is sharp".to_string(),
            CoverUpgradeMode::Scan => format!("{} albums have soft covers", n0(run.soft.into())),
            CoverUpgradeMode::Preview => format!("{} larger covers found", n0(run.upgraded.into())),
            _ => format!("{} covers replaced", n0(run.files.into())),
        })
    };
    Some(
        ActivityItem::new(id, "covers", "covers", title, state)
            .counted(done, total, unit)
            .detail(detail)
            .started(run.started_utc)
            .finished(run.finished_utc)
            .error(if state == "failed" {
                run.reason.clone()
            } else {
                None
            }),
    )
}

/// A lyrics run.
pub fn lyrics(run: &LyricsLibraryRun, running: bool, now: DateTime<Utc>) -> Option<ActivityItem> {
    let title = match run.mode {
        LyricsLibraryMode::Undo => "Putting lyrics back",
        LyricsLibraryMode::Save => "Saving lyrics",
        LyricsLibraryMode::Preview => "Finding better lyrics",
        LyricsLibraryMode::Scan => "Reading your lyrics",
        LyricsLibraryMode::Walk => "Finding lyrics",
    };
    if running && run.status != LyricsLibraryStatus::Running {
        return Some(
            ActivityItem::new("lyrics:pending", "lyrics", "lyrics", title, "running")
                .detail(Some("Getting started".to_string())),
        );
    }
    let id = format!("lyrics:{}", run.run_id);
    if run.status == LyricsLibraryStatus::Running {
        return Some(
            ActivityItem::new(id, "lyrics", "lyrics", title, "running")
                .counted(run.processed, run.total, "songs")
                .detail((run.written > 0).then(|| format!("{} written so far", n0(run.written.into()))))
                .started(run.started_utc),
        );
    }
    if run.status == LyricsLibraryStatus::Idle || !recent(run.finished_utc, now) {
        return None;
    }
    let state = ended(lyrics_status_name(run.status));
    let detail = if state != "done" {
        run.reason.clone()
    } else {
        Some(match run.mode {
            LyricsLibraryMode::Scan | LyricsLibraryMode::Preview => {
                format!("{} songs read", n0(run.processed.into()))
            }
            LyricsLibraryMode::Undo => "The old lyrics are back".to_string(),
            _ => format!("{} songs got lyrics", n0(run.written.into())),
        })
    };
    Some(
        ActivityItem::new(id, "lyrics", "lyrics", title, state)
            .counted(run.processed, run.total, "songs")
            .detail(detail)
            .started(run.started_utc)
            .finished(run.finished_utc)
            .error(if state == "failed" {
                run.reason.clone()
            } else {
                None
            }),
    )
}

/// Better copies of lossy songs: one item for the whole queue while any is open.
pub fn upgrades(jobs: &[UpgradeJob], now: DateTime<Utc>) -> Option<ActivityItem> {
    let open: Vec<&UpgradeJob> = jobs.iter().filter(|j| UpgradeStates::open(&j.state)).collect();
    if open.is_empty() {
        let ended: Vec<&UpgradeJob> = jobs
            .iter()
            .filter(|j| !UpgradeStates::open(&j.state) && now - j.updated_utc < RECENT_WINDOW)
            .collect();
        let last = ended.iter().map(|j| j.updated_utc).max()?;
        let upgraded = ended
            .iter()
            .filter(|j| j.state == UpgradeStates::UPGRADED)
            .count();
        let failed = ended.iter().filter(|j| j.state == UpgradeStates::FAILED).count();
        let mut detail = format!("{} upgraded", n0(upgraded as i64));
        if failed > 0 {
            detail.push_str(&format!(", {} failed", n0(failed as i64)));
        }
        let count = ended.len() as i32;
        return Some(
            ActivityItem::new(
                format!("upgrades:{}", dotnet_ticks(last)),
                "upgrades",
                "lossy",
                "Finding better copies",
                if failed > 0 && upgraded == 0 {
                    "failed"
                } else {
                    "done"
                },
            )
            .counted(count, count, "songs")
            .detail(Some(detail))
            .finished(Some(last)),
        );
    }
    let detail = match open.iter().find(|j| j.state == UpgradeStates::WORKING) {
        Some(working) => {
            let mut text = format!(
                "{} · {}",
                working.artist.as_deref().unwrap_or_default(),
                working.title.as_deref().unwrap_or_default()
            );
            if open.len() > 1 {
                text.push_str(&format!(", {} more waiting", n0(open.len() as i64 - 1)));
            }
            text
        }
        None => format!("{} waiting", n0(open.len() as i64)),
    };
    let started = open.iter().map(|j| j.started_utc.unwrap_or(j.queued_utc)).min();
    Some(
        ActivityItem::new(
            "upgrades",
            "upgrades",
            "lossy",
            "Finding better copies",
            "running",
        )
        .detail(Some(detail))
        .started(started),
    )
}

/// Hearted downloads: one item while any is on its way, the song in front named.
pub fn downloads(all: &[AcquisitionSnapshot], now: DateTime<Utc>) -> Option<ActivityItem> {
    let text = |value: &Option<String>| value.clone().unwrap_or_default();
    let moving: Vec<&AcquisitionSnapshot> = all
        .iter()
        .filter(|a| !matches!(a.state, AcquisitionState::Done | AcquisitionState::Failed))
        .collect();
    if moving.is_empty() {
        let landed: Vec<&AcquisitionSnapshot> = all
            .iter()
            .filter(|a| a.state == AcquisitionState::Done && now - a.updated_at < RECENT_WINDOW)
            .collect();
        let failed: Vec<&AcquisitionSnapshot> = all
            .iter()
            .filter(|a| a.state == AcquisitionState::Failed && now - a.updated_at < RECENT_WINDOW)
            .collect();
        // MaxBy keeps the first of equal times.
        let mut last: Option<&AcquisitionSnapshot> = None;
        for a in landed.iter().chain(failed.iter()) {
            if last.is_none_or(|l| a.updated_at > l.updated_at) {
                last = Some(a);
            }
        }
        let last = last?;
        let title = if landed.len() == 1 && failed.is_empty() {
            format!("Added {}", text(&landed[0].title))
        } else if !landed.is_empty() {
            format!("Added {} songs", n0(landed.len() as i64))
        } else {
            format!("{} could not be fetched", text(&failed[0].title))
        };
        let detail = if landed.len() == 1 && failed.is_empty() {
            landed[0].artist.clone()
        } else if !failed.is_empty() && !landed.is_empty() {
            Some(format!("{} could not be fetched", n0(failed.len() as i64)))
        } else if !failed.is_empty() {
            failed[0].error.clone()
        } else {
            None
        };
        return Some(
            ActivityItem::new(
                format!("downloads:{}", dotnet_ticks(last.updated_at)),
                "downloads",
                "fetched",
                title,
                if landed.is_empty() { "failed" } else { "done" },
            )
            .detail(detail)
            .finished(Some(last.updated_at))
            .error(if landed.is_empty() {
                failed[0].error.clone()
            } else {
                None
            }),
        );
    }
    let front = moving
        .iter()
        .find(|a| a.state == AcquisitionState::Downloading)
        .unwrap_or(&moving[0]);
    let name = format!("{} · {}", text(&front.artist), text(&front.title));
    let title = if moving.len() == 1 {
        format!("Fetching {}", text(&front.title))
    } else {
        format!("Fetching {} songs", n0(moving.len() as i64))
    };
    let detail = if moving.len() == 1 {
        format!("{} · {}", text(&front.artist), state_words(front.state))
    } else {
        format!("{name} · {}", state_words(front.state))
    };
    let mut item = ActivityItem::new("downloads", "downloads", "fetched", title, "running")
        .detail(Some(detail))
        .started(moving.iter().map(|a| a.started_at).min());
    item.fraction = if front.state == AcquisitionState::Downloading {
        front.progress
    } else {
        None
    };
    Some(item)
}

fn state_words(state: AcquisitionState) -> &'static str {
    match state {
        AcquisitionState::Queued => "waiting its turn",
        AcquisitionState::Searching => "searching",
        AcquisitionState::Downloading => "downloading",
        AcquisitionState::Verifying => "checking the file",
        AcquisitionState::Importing => "adding it to your library",
        // `state.ToString().ToLowerInvariant()`.
        AcquisitionState::Done => "done",
        AcquisitionState::Failed => "failed",
    }
}

/// What the activity view reads of one Find songs look (`FindSnapshot`).
#[derive(Debug, Clone, PartialEq)]
pub struct FindLook {
    pub id: String,
    /// `FindStates`: searching, done or failed.
    pub state: String,
    /// The song looked for (`Target.Title`).
    pub target_title: String,
    /// How many copies it has found so far.
    pub copies: usize,
    /// Each source's name and state, in order.
    pub sources: Vec<(String, String)>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub error: Option<String>,
}

impl From<&FindSnapshot> for FindLook {
    fn from(look: &FindSnapshot) -> Self {
        FindLook {
            id: look.id.clone(),
            state: look.state.clone(),
            target_title: look.target.title.clone(),
            copies: look.copies.len(),
            sources: look
                .sources
                .iter()
                .map(|source| (source.name.clone(), source.state.clone()))
                .collect(),
            started_at: look.started_at,
            updated_at: look.updated_at,
            error: look.error.clone(),
        }
    }
}

const FIND_SEARCHING: &str = "searching";
const FIND_FAILED: &str = "failed";

/// Find songs from the dashboard: the newest search this admin has running, or the newest one
/// that ended a moment ago, with how many copies it has found. A pick from it is a download (or,
/// for a library song, a better copy) and gets that card.
pub fn find(looks: &[FindLook], now: DateTime<Utc>) -> Option<ActivityItem> {
    let look = looks
        .iter()
        .find(|l| l.state == FIND_SEARCHING)
        .or_else(|| looks.iter().find(|l| now - l.updated_at < RECENT_WINDOW))?;
    let id = format!("find:{}", look.id);
    let title = format!("Finding copies of {}", look.target_title);
    let copies = look.copies;
    let count = format!(
        "{} {}",
        n0(copies as i64),
        if copies == 1 { "copy" } else { "copies" }
    );
    if look.state == FIND_SEARCHING {
        let asking: Vec<&str> = look
            .sources
            .iter()
            .filter(|(_, state)| state == FIND_SEARCHING)
            .map(|(name, _)| name.as_str())
            .collect();
        let asking = asking.join(" and ");
        let detail = if copies > 0 {
            Some(format!("{count} so far"))
        } else if !asking.is_empty() {
            Some(format!("Asking {asking}"))
        } else {
            None
        };
        return Some(
            ActivityItem::new(id, "find", "fetched", title, "running")
                .detail(detail)
                .started(Some(look.started_at)),
        );
    }
    Some(if look.state == FIND_FAILED {
        ActivityItem::new(id, "find", "fetched", title, "failed")
            .detail(look.error.clone())
            .started(Some(look.started_at))
            .finished(Some(look.updated_at))
            .error(look.error.clone())
    } else {
        ActivityItem::new(
            id,
            "find",
            "fetched",
            format!("Looked for copies of {}", look.target_title),
            "done",
        )
        .detail(Some(if copies > 0 {
            format!("{count} found. Pick one on Fetched songs")
        } else {
            "No copies were found".to_string()
        }))
        .started(Some(look.started_at))
        .finished(Some(look.updated_at))
    })
}

/// What the activity view reads of a person's imports (`ImportOverview`): the trickle.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImportsActivity {
    pub trickle_downloading: i32,
    pub trickle_done: i32,
    pub trickle_queued: i32,
}

impl From<&ImportOverview> for ImportsActivity {
    fn from(overview: &ImportOverview) -> Self {
        ImportsActivity {
            trickle_downloading: overview.trickle.downloading,
            trickle_done: overview.trickle.done,
            trickle_queued: overview.trickle.queued,
        }
    }
}

/// The trickle while it fetches.
pub fn imports(overview: &ImportsActivity) -> Vec<ActivityItem> {
    let mut items = Vec::new();
    if overview.trickle_downloading > 0 {
        items.push(
            ActivityItem::new(
                "imports:trickle",
                "imports",
                "imports",
                "Fetching songs from your lists",
                "running",
            )
            .detail(Some(format!(
                "{} done, {} to go",
                n0(overview.trickle_done.into()),
                n0(overview.trickle_queued.into())
            ))),
        );
    }
    items
}

/// What the activity view reads of a Library health run (`HealthRun`).
#[derive(Debug, Clone, PartialEq)]
pub struct HealthActivity {
    pub id: String,
    /// fix, lookup or undo.
    pub kind: String,
    pub label: String,
    pub total: i32,
    pub done: i32,
    /// running, done, stopped or failed.
    pub state: String,
    pub started_utc: DateTime<Utc>,
    pub finished_utc: Option<DateTime<Utc>>,
    pub current: Option<String>,
    /// Lookups so far whose state is found.
    pub found: usize,
    /// `Outcome?.Summary()`.
    pub outcome_summary: Option<String>,
    pub error: Option<String>,
}

impl From<&HealthRun> for HealthActivity {
    fn from(run: &HealthRun) -> Self {
        let progress = run.progress();
        HealthActivity {
            id: run.id.clone(),
            kind: run.kind.clone(),
            label: run.label.clone(),
            total: i32::try_from(run.total).unwrap_or(i32::MAX),
            done: i32::try_from(progress.done).unwrap_or(i32::MAX),
            state: progress.state.clone(),
            started_utc: run.started_utc,
            finished_utc: progress.finished_utc,
            current: progress.current.clone(),
            found: run
                .lookups_now()
                .iter()
                .filter(|lookup| lookup.state == "found")
                .count(),
            outcome_summary: progress.outcome.as_ref().map(|outcome| outcome.summary()),
            error: progress.error,
        }
    }
}

/// Library health: a check of the library while one runs, and a run of fixes or lookups.
pub fn health(
    run: Option<&HealthActivity>,
    checking: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Vec<ActivityItem> {
    let mut items = Vec::new();
    if let Some(since) = checking {
        items.push(
            ActivityItem::new(
                "health:check",
                "health",
                "health",
                "Checking your library",
                "running",
            )
            .detail(Some("Reading every song from Navidrome".to_string()))
            .started(Some(since)),
        );
    }
    let Some(run) = run else {
        return items;
    };
    let unit = if run.kind == "lookup" { "songs" } else { "changes" };
    let id = format!("health:{}", run.id);
    if run.state == "running" {
        items.push(
            ActivityItem::new(id, "health", "health", run.label.clone(), "running")
                .counted(run.done, run.total, unit)
                .detail(run.current.clone())
                .started(Some(run.started_utc)),
        );
        return items;
    }
    if !recent(run.finished_utc, now) {
        return items;
    }
    let state = match run.state.as_str() {
        "done" => "done",
        "stopped" => "stopped",
        "failed" => "failed",
        _ => "running",
    };
    let detail = if run.state == "failed" {
        run.error.clone()
    } else if run.kind == "lookup" {
        Some(format!(
            "Found tags for {} of {} songs",
            n0(run.found as i64),
            n0(run.done.into())
        ))
    } else {
        run.outcome_summary.clone()
    };
    items.push(
        ActivityItem::new(id, "health", "health", run.label.clone(), state)
            .counted(run.done, run.total, unit)
            .detail(detail)
            .started(Some(run.started_utc))
            .finished(run.finished_utc)
            .error(if run.state == "failed" {
                run.error.clone()
            } else {
                None
            }),
    );
    items
}

/// What the activity view reads of the duplicate scan worker.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DuplicateScan {
    /// A scan walking the library right now.
    pub scanning: bool,
    /// "Scan now" pressed and not yet picked up.
    pub requested: bool,
    pub started_utc: Option<DateTime<Utc>>,
    pub last: Option<DuplicateScanResult>,
}

/// A duplicate scan while it runs, and its result a moment after.
pub fn duplicates(worker: &DuplicateScan, now: DateTime<Utc>) -> Option<ActivityItem> {
    if worker.scanning || worker.requested {
        return Some(
            ActivityItem::new(
                "duplicates",
                "duplicates",
                "libraryactions",
                "Looking for duplicates",
                "running",
            )
            .detail(Some(
                if worker.scanning {
                    "Reading every song's recording"
                } else {
                    "Starting"
                }
                .to_string(),
            ))
            .started(worker.started_utc),
        );
    }
    let last = worker
        .last
        .as_ref()
        .filter(|last| now - last.at_utc < RECENT_WINDOW)?;
    let detail = if last.groups == 0 {
        "No duplicates".to_string()
    } else {
        format!(
            "{} sets of duplicates, {} new",
            n0(last.groups as i64),
            n0(last.added as i64)
        )
    };
    Some(
        ActivityItem::new(
            format!("duplicates:{}", dotnet_ticks(last.at_utc)),
            "duplicates",
            "libraryactions",
            "Looked for duplicates",
            "done",
        )
        .detail(Some(if last.complete {
            detail
        } else {
            detail + " (part of the library)"
        }))
        .finished(Some(last.at_utc)),
    )
}

/// What the activity view reads of slskd's sharing report (`SharingReport`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RescanReport {
    pub scanning: bool,
    pub scan_progress: Option<f64>,
    pub scan_failed: bool,
    pub files: Option<i64>,
}

impl From<&SharingReport> for RescanReport {
    fn from(report: &SharingReport) -> Self {
        RescanReport {
            scanning: report.scanning,
            scan_progress: report.scan_progress,
            scan_failed: report.scan_failed,
            files: report.files.map(i64::from),
        }
    }
}

/// An slskd rescan the dashboard asked for at `requested`.
pub fn rescan(report: &RescanReport, requested: DateTime<Utc>, now: DateTime<Utc>) -> ActivityItem {
    let id = format!("share-rescan:{}", dotnet_ticks(requested));
    if report.scan_failed {
        return ActivityItem::new(
            id,
            "share",
            "soulseek",
            "slskd could not rescan your share",
            "failed",
        )
        .error(Some("slskd stopped the scan. Its log says why.".to_string()));
    }
    if report.scanning || now - requested < RESCAN_START {
        let mut item = ActivityItem::new(
            id,
            "share",
            "soulseek",
            "slskd is rescanning your share",
            "running",
        )
        .detail((!report.scanning).then(|| "Starting".to_string()))
        .started(Some(requested));
        item.fraction = if report.scanning {
            report.scan_progress
        } else {
            None
        };
        return item;
    }
    // No finish time: slskd does not say when it ended, so only a browser that saw it running
    // says done.
    ActivityItem::new(id, "share", "soulseek", "slskd rescanned your share", "done")
        .detail(report.files.map(|files| format!("{} files shared", n0(files))))
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
