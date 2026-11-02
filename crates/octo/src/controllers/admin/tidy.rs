//! The library tidy, for its two tasks (Artists, File layout): preview, apply, resume, cancel and
//! undo, and the paged preview.
//!
//! Every endpoint needs a Navidrome admin sign-in: these read and change library files. Apply
//! does only what a finished preview of the same task and files showed, never against a
//! Navidrome older than 0.55, never while another library job writes the music folder, for the
//! File layout only with the layout the preview was made with, and for the whole library only with
//! the music path typed back.

use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use super::json_body::{FromBody, Kind, Member, Shape, bind_optional_body};
use super::request_binding::{bind_body, error, ok, query, sign_in, signed, split, status, utc_opt};
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::library::library_write_gate::LibraryWriteGate;
use crate::services::library::tidy::planner::{TidyKind, TidyRow};
use crate::services::library::tidy::{TidyMode, TidyRequest, TidyScope, TidyStatus, TidyTask, TidyWorker};
use crate::services::subsonic::navidrome_version;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/tidy", get(get_tidy))
        .route("/api/admin/tidy/rows", get(get_rows))
        .route("/api/admin/tidy/preview", post(preview))
        .route("/api/admin/tidy/apply", post(apply))
        .route("/api/admin/tidy/resume", post(resume))
        .route("/api/admin/tidy/cancel", post(cancel))
        .route("/api/admin/tidy/undo", post(undo))
}

const ALREADY_RUNNING: &str = "A library tidy is already running.";

/// What the dashboard sends.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TidyBody {
    scope: Option<String>,
    task: Option<String>,
    confirm: Option<String>,
    ticked: Option<Vec<String>>,
    excluded: Option<Vec<String>>,
    all_runs: bool,
    /// The run an Undo was shown for (`undoRun`).
    run: Option<String>,
}

impl FromBody for TidyBody {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Tidy.Request",
        by_constructor: true,
        members: &[
            Member {
                name: "scope",
                kind: Kind::Text,
            },
            Member {
                name: "task",
                kind: Kind::Text,
            },
            Member {
                name: "confirm",
                kind: Kind::Text,
            },
            Member {
                name: "ticked",
                kind: Kind::TextList,
            },
            Member {
                name: "excluded",
                kind: Kind::TextList,
            },
            Member {
                name: "allRuns",
                kind: Kind::Bool,
            },
            Member {
                name: "run",
                kind: Kind::Text,
            },
        ],
    };
}

fn worker(state: &AppState) -> &TidyWorker {
    &state.tidy_worker
}

/// 409 while another library job writes the music folder.
fn other_job_writing(state: &AppState) -> Option<Response> {
    state
        .library_write_gate
        .job()
        .filter(|holder| holder != TidyWorker::JOB_NAME)
        .map(|holder| error(StatusCode::CONFLICT, LibraryWriteGate::busy_message(&holder)))
}

/// The music path typed back, for a run that changes files Octo never downloaded.
fn path_typed_back(state: &AppState, scope: TidyScope, confirm: Option<&str>) -> Option<Response> {
    let root = worker(state).music_root();
    (scope == TidyScope::WholeLibrary && confirm.map(str::trim) != Some(root.as_str())).then(|| {
        error(
            StatusCode::BAD_REQUEST,
            format!("To tidy the whole library, type the music path exactly: {root}"),
        )
    })
}

/// Preview: read every file and list what would change and what is left alone. Writes nothing.
async fn preview(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    if !signed(&state, &parts) {
        return sign_in();
    }
    let body: TidyBody = match bind_optional_body(&parts.headers, &body) {
        Ok(body) => body.unwrap_or_default(),
        Err(answer) => return answer,
    };
    let scope = TidyScope::parse(body.scope.as_deref());
    let task = TidyTask::parse(body.task.as_deref());
    if task == TidyTask::FileLayout
        && let Some(why) = worker(&state).layout_refusal(TidyMode::Preview)
    {
        return error(StatusCode::BAD_REQUEST, why);
    }
    let request = TidyRequest {
        mode: TidyMode::Preview,
        scope,
        task,
        excluded: body.excluded.unwrap_or_default(),
        ..TidyRequest::default()
    };
    if !worker(&state).try_enqueue(request) {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    info!(
        "Library tidy preview requested: {} task, scope {}",
        task.name(),
        scope.name()
    );
    status(
        StatusCode::ACCEPTED,
        &json!({ "started": true, "scope": scope.name(), "task": task.name(), "mode": "Preview" }),
    )
}

/// Apply what the last preview showed, with the opt-in rows the person ticked.
async fn apply(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    if !signed(&state, &parts) {
        return sign_in();
    }
    let body: TidyBody = match bind_body(&parts, &body) {
        Ok(body) => body,
        Err(answer) => return *answer,
    };
    let scope = TidyScope::parse(body.scope.as_deref());
    let task = TidyTask::parse(body.task.as_deref());
    if let Some(refused) = path_typed_back(&state, scope, body.confirm.as_deref()) {
        return refused;
    }
    let tidy = worker(&state);
    if tidy.is_running() || tidy.is_pending() {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    let run = tidy.current();
    if !run.is_finished_preview() || run.scope != scope || run.task != task {
        return error(
            StatusCode::BAD_REQUEST,
            "Preview first. Apply only does what a finished preview of the same files showed.",
        );
    }
    if task == TidyTask::FileLayout
        && let Some(why) = tidy.layout_refusal(TidyMode::Apply)
    {
        return error(StatusCode::BAD_REQUEST, why);
    }
    if let Some(busy) = other_job_writing(&state) {
        return busy;
    }
    let gate = tidy.check_navidrome(true).await;
    if !gate.apply_allowed {
        return error(StatusCode::BAD_REQUEST, gate.reason.unwrap_or_default());
    }
    let request = TidyRequest {
        mode: TidyMode::Apply,
        scope,
        task,
        ticked: body.ticked.unwrap_or_default(),
        ..TidyRequest::default()
    };
    if !tidy.try_enqueue(request) {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    info!(
        "Library tidy apply requested: {} task, scope {}",
        task.name(),
        scope.name()
    );
    status(
        StatusCode::ACCEPTED,
        &json!({ "started": true, "scope": scope.name(), "task": task.name(), "mode": "Apply" }),
    )
}

/// Carry on with an apply that was stopped, from where it stopped.
async fn resume(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    if !signed(&state, &parts) {
        return sign_in();
    }
    let body: TidyBody = match bind_optional_body(&parts.headers, &body) {
        Ok(body) => body.unwrap_or_default(),
        Err(answer) => return answer,
    };
    let tidy = worker(&state);
    let run = tidy.current();
    if !run.can_resume() || tidy.store().plan().and_then(|plan| plan.work).is_none() {
        return error(StatusCode::BAD_REQUEST, "There is no stopped tidy to resume.");
    }
    if let Some(refused) = path_typed_back(&state, run.scope, body.confirm.as_deref()) {
        return refused;
    }
    if run.task == TidyTask::FileLayout
        && let Some(why) = tidy.layout_refusal(TidyMode::Apply)
    {
        return error(StatusCode::BAD_REQUEST, why);
    }
    if let Some(busy) = other_job_writing(&state) {
        return busy;
    }
    let request = TidyRequest {
        mode: TidyMode::Apply,
        scope: run.scope,
        task: run.task,
        resume: true,
        ..TidyRequest::default()
    };
    if !tidy.try_enqueue(request) {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    status(
        StatusCode::ACCEPTED,
        &json!({ "started": true, "mode": "Resume" }),
    )
}

async fn cancel(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    worker(&state).request_cancel();
    status(StatusCode::ACCEPTED, &json!({ "cancelling": true }))
}

/// Put back the last tidy (or every one the journal holds): folders first, then the tags.
async fn undo(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    if !signed(&state, &parts) {
        return sign_in();
    }
    let body: TidyBody = match bind_optional_body(&parts.headers, &body) {
        Ok(body) => body.unwrap_or_default(),
        Err(answer) => return answer,
    };
    let tidy = worker(&state);
    if !tidy.journal().exists() {
        return error(StatusCode::BAD_REQUEST, "There is no tidy to undo.");
    }
    if let Some(busy) = other_job_writing(&state) {
        return busy;
    }
    let request = TidyRequest {
        mode: TidyMode::Undo,
        all_runs: body.all_runs,
        run: body.run.filter(|run| !run.trim().is_empty()),
        ..TidyRequest::default()
    };
    if !tidy.try_enqueue(request) {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    info!("Library tidy undo requested");
    status(StatusCode::ACCEPTED, &json!({ "started": true, "mode": "Undo" }))
}

fn kind_name(kind: TidyKind) -> &'static str {
    match kind {
        TidyKind::Split => "Split",
        TidyKind::Respell => "Respell",
        TidyKind::Rename => "Rename",
        TidyKind::Guest => "Guest",
        TidyKind::Layout => "Layout",
        TidyKind::Skip => "Skip",
    }
}

fn row_json(row: &TidyRow) -> Value {
    json!({
        "id": row.id,
        "kind": kind_name(row.kind),
        "path": row.path,
        "before": row.before,
        "after": row.after,
        "evidence": row.evidence,
        "files": row.files,
        "optIn": row.opt_in,
        "ticked": row.ticked,
    })
}

/// The run, what the last preview holds by kind, and whether Apply is allowed.
async fn get_tidy(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    let tidy = worker(&state);
    // A short wait: the dashboard polls this, and a Navidrome that is down must not hang it.
    let gate = tokio::time::timeout(Duration::from_secs(3), tidy.check_navidrome(false))
        .await
        .unwrap_or_else(|_| navidrome_version::gate(None));
    let run = tidy.current();
    let plan = tidy.store().plan();
    let mut counts = serde_json::Map::new();
    for kind in [
        TidyKind::Split,
        TidyKind::Respell,
        TidyKind::Rename,
        TidyKind::Guest,
        TidyKind::Layout,
        TidyKind::Skip,
    ] {
        let count = plan.as_ref().map_or(0, |plan| {
            plan.plan.rows.iter().filter(|row| row.kind == kind).count()
        });
        counts.insert(kind_name(kind).to_string(), json!(count));
    }
    let held: Vec<Value> = run.held_rows.iter().map(row_json).collect();
    ok(&json!({
        "runId": run.run_id,
        "status": run.status.name(),
        "mode": run.mode.name(),
        "scope": run.scope.name(),
        "task": run.task.name(),
        "pending": tidy.is_pending(),
        "phase": run.phase,
        "startedUtc": utc_opt(&run.started_utc),
        "finishedUtc": utc_opt(&run.finished_utc),
        "files": run.files,
        "inScope": run.in_scope,
        "plannedRetags": run.planned_retags,
        "plannedMoves": run.planned_moves,
        "plannedFileMoves": run.planned_file_moves,
        "retagged": run.retagged,
        "moved": run.moved,
        "movedFiles": run.moved_files,
        "held": run.held,
        "failed": run.failed,
        "lookupsAsked": run.lookups_asked,
        "lookupsNotAsked": run.lookups_not_asked,
        "reason": run.reason,
        "errors": run.errors,
        "heldRows": held,
        "rows": Value::Object(counts),
        "excluded": plan.as_ref().map(|plan| plan.excluded.clone()).unwrap_or_default(),
        "previewed": run.is_finished_preview(),
        "canResume": run.can_resume() && plan.as_ref().is_some_and(|plan| plan.work.is_some()),
        "canUndo": tidy.journal().exists(),
        "undoTask": tidy.journal().last_run_task().map(TidyTask::name),
        // The run Undo puts back, which an Undo sends back, so a page another apply has gone past
        // puts back nothing.
        "undoRun": tidy.journal().last_run(),
        "layoutRefusal": tidy.layout_refusal(TidyMode::Apply),
        "running": run.status == TidyStatus::Running,
        "otherJob": state.library_write_gate.job().filter(|holder| holder != TidyWorker::JOB_NAME),
        "navidromeVersion": gate.version,
        "applyAllowed": gate.apply_allowed,
        "applyBlockedReason": gate.reason,
        "musicPath": tidy.music_root(),
    }))
}

/// One page of the last preview's rows: `kind` is a row kind, or "Changes" for every row that
/// changes something. Totals are the plan's, not the page's.
async fn get_rows(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    let kind = query(&parts.uri, "kind").unwrap_or_else(|| "Changes".to_string());
    let offset: usize = query(&parts.uri, "offset")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let limit: usize = query(&parts.uri, "limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let Some(plan) = worker(&state).store().plan() else {
        return ok(&json!({ "total": 0, "offset": 0, "rows": [] }));
    };
    let matching: Vec<&TidyRow> = plan
        .plan
        .rows
        .iter()
        .filter(|row| {
            if kind.eq_ignore_ascii_case("Changes") {
                row.kind != TidyKind::Skip
            } else {
                kind_name(row.kind).eq_ignore_ascii_case(&kind)
            }
        })
        .collect();
    let page: Vec<Value> = matching
        .iter()
        .skip(offset)
        .take(limit)
        .map(|row| row_json(row))
        .collect();
    ok(&json!({ "total": matching.len(), "offset": offset, "rows": page }))
}

#[cfg(test)]
#[path = "tidy_tests.rs"]
mod tests;
