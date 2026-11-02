//! /api/admin/tidy through the assembled pipeline: a Navidrome admin sign-in for everything, the
//! admin header for writes, one run at a time, the music path typed back for the whole library,
//! and Apply only after a finished preview against a Navidrome new enough.

use axum::body::Body;
use axum::http::{Method, Request};
use http_body_util::BodyExt;
use octo_core::settings::AppSettings;
use tower::ServiceExt;

use super::*;
use crate::http::pipeline::build;
use crate::http::static_files::StaticAssets;
use crate::services::library::tidy::planner::{TidyKind, TidyPlan, TidyRow};
use crate::services::library::tidy::store::{TidyLayout, TidyPlanFile, TidyRun};

struct Reply {
    status: StatusCode,
    body: String,
}

fn request(
    method: Method,
    uri: &str,
    token: Option<&str>,
    admin_header: bool,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if admin_header {
        builder = builder.header("X-Octo-Admin", "1");
    }
    if let Some(token) = token {
        builder = builder.header("X-Octo-Browse-Token", token);
    }
    match body {
        Some(body) => builder
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("a request"),
        None => builder.body(Body::empty()).expect("a request"),
    }
}

async fn send(state: &AppState, request: Request<Body>) -> Reply {
    let app = build(state.clone(), &StaticAssets::default(), 0);
    let response = app.oneshot(request).await.expect("the app answers");
    let status = response.status();
    let bytes = response.into_body().collect().await.expect("a body").to_bytes();
    Reply {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn post(state: &AppState, token: &str, uri: &str, body: Value) -> Reply {
    send(state, request(Method::POST, uri, Some(token), true, Some(body))).await
}

fn previewed(state: &AppState, scope: TidyScope) {
    previewed_task(state, scope, TidyTask::Artists, None);
}

/// A finished preview of `task`, made with `layout` (a File layout preview's).
fn previewed_task(state: &AppState, scope: TidyScope, task: TidyTask, layout: Option<TidyLayout>) {
    let store = state.tidy_worker.store();
    let kind = match task {
        TidyTask::Artists => TidyKind::Guest,
        TidyTask::FileLayout => TidyKind::Layout,
    };
    store
        .save_plan(TidyPlanFile {
            root: state.tidy_worker.music_root(),
            scope,
            task,
            layout,
            plan: TidyPlan {
                rows: ["a", "b", "c"]
                    .iter()
                    .enumerate()
                    .map(|(at, id)| TidyRow {
                        id: id.to_string(),
                        kind: if at == 2 { TidyKind::Skip } else { kind },
                        path: format!("A/{id}.flac"),
                        before: String::new(),
                        after: String::new(),
                        evidence: String::new(),
                        files: 1,
                        opt_in: false,
                        ticked: true,
                    })
                    .collect(),
                ..TidyPlan::default()
            },
            ..TidyPlanFile::default()
        })
        .expect("kept");
    store.replace(TidyRun {
        run_id: "p1".into(),
        status: TidyStatus::Completed,
        mode: TidyMode::Preview,
        scope,
        task,
        planned_file_moves: 2,
        plan_hash: Some("ABCDEF0123456789".into()),
        ..TidyRun::default()
    });
}

/// The layout as a File layout preview made now keeps it.
fn layout_now(state: &AppState) -> TidyLayout {
    let settings = state.settings.current();
    TidyLayout {
        structure: format!("{:?}", settings.subsonic.folder_structure),
        templates: octo_core::common::Layout::from_settings(&settings.subsonic)
            .texts()
            .to_vec(),
    }
}

#[tokio::test]
async fn a_file_layout_preview_is_asked_for_by_its_task() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");

    let reply = post(
        &state,
        &token,
        "/api/admin/tidy/preview",
        json!({ "scope": "OctoDownloads", "task": "FileLayout" }),
    )
    .await;

    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.body);
    let body: Value = serde_json::from_str(&reply.body).expect("json");
    assert_eq!(body["task"], json!("FileLayout"));
}

#[tokio::test]
async fn apply_is_refused_for_the_other_task() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");
    previewed(&state, TidyScope::OctoDownloads);

    let reply = post(
        &state,
        &token,
        "/api/admin/tidy/apply",
        json!({ "scope": "OctoDownloads", "task": "FileLayout" }),
    )
    .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("Preview first"), "{}", reply.body);
}

#[tokio::test]
async fn apply_is_refused_for_a_layout_changed_since_the_preview() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");
    let mut then = layout_now(&state);
    then.structure = "Organized".into();
    then.templates = vec![octo_core::common::layout::ORGANIZED.to_string(); 3];
    previewed_task(&state, TidyScope::OctoDownloads, TidyTask::FileLayout, Some(then));

    let reply = post(
        &state,
        &token,
        "/api/admin/tidy/apply",
        json!({ "scope": "OctoDownloads", "task": "FileLayout" }),
    )
    .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply
            .body
            .contains("The File layout changed since the preview. Preview again."),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_layout_with_a_problem_is_refused_for_preview_and_apply() {
    let mut settings = AppSettings::default();
    settings.subsonic.folder_structure = octo_core::settings::FolderStructure::Custom;
    settings.subsonic.folder_template = "{Artst}/{Title}".into();
    let state = AppState::for_tests(settings);
    let token = state.browse_sessions.create("admin");

    let preview = post(
        &state,
        &token,
        "/api/admin/tidy/preview",
        json!({ "scope": "OctoDownloads", "task": "FileLayout" }),
    )
    .await;
    previewed_task(
        &state,
        TidyScope::OctoDownloads,
        TidyTask::FileLayout,
        Some(layout_now(&state)),
    );
    let apply = post(
        &state,
        &token,
        "/api/admin/tidy/apply",
        json!({ "scope": "OctoDownloads", "task": "FileLayout" }),
    )
    .await;

    for reply in [preview, apply] {
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
        assert!(
            reply.body.contains("The Albums layout has a problem"),
            "{}",
            reply.body
        );
    }
    assert!(!state.tidy_worker.is_pending());
}

#[tokio::test]
async fn the_status_carries_the_task_and_counts_layout_rows() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");
    previewed_task(
        &state,
        TidyScope::OctoDownloads,
        TidyTask::FileLayout,
        Some(layout_now(&state)),
    );

    let reply = send(
        &state,
        request(Method::GET, "/api/admin/tidy", Some(&token), false, None),
    )
    .await;

    let status: Value = serde_json::from_str(&reply.body).expect("json");
    assert_eq!(status["task"], json!("FileLayout"));
    assert_eq!(status["rows"]["Layout"], json!(2));
    assert_eq!(status["plannedFileMoves"], json!(2));
    assert_eq!(status["movedFiles"], json!(0));
    assert_eq!(status["layoutRefusal"], Value::Null);
    let page = send(
        &state,
        request(
            Method::GET,
            "/api/admin/tidy/rows?kind=Layout",
            Some(&token),
            false,
            None,
        ),
    )
    .await;
    let page: Value = serde_json::from_str(&page.body).expect("json");
    assert_eq!(page["total"], json!(2));
    assert_eq!(page["rows"][0]["kind"], json!("Layout"));
}

async fn tidy_status(state: &AppState, token: &str) -> Value {
    let reply = send(
        state,
        request(Method::GET, "/api/admin/tidy", Some(token), false, None),
    )
    .await;
    serde_json::from_str(&reply.body).expect("json")
}

/// The status says which run Undo puts back, the newest in the journal, with its task, and an
/// Undo sends that run back.
#[tokio::test]
async fn the_status_says_which_run_an_undo_puts_back() {
    use crate::services::library::tidy::TidyJournalEntry;

    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");
    let journal = state.tidy_worker.journal();
    let begin = |run: &str, task| TidyJournalEntry::Begin {
        run: run.into(),
        task,
        at: chrono::Utc::now(),
    };
    journal
        .append(&begin("r1", TidyTask::Artists))
        .expect("journaled");

    let first = tidy_status(&state, &token).await;
    assert_eq!(first["undoRun"], json!("r1"));
    assert_eq!(first["undoTask"], json!("Artists"));
    assert!(first.get("undoAlbum").is_none());

    journal
        .append(&begin("r2", TidyTask::FileLayout))
        .expect("journaled");
    let second = tidy_status(&state, &token).await;
    assert_eq!(second["undoRun"], json!("r2"));
    assert_eq!(second["undoTask"], json!("FileLayout"));

    let undo = post(&state, &token, "/api/admin/tidy/undo", json!({ "run": "r2" })).await;
    assert_eq!(undo.status, StatusCode::ACCEPTED, "{}", undo.body);
}

#[test]
fn the_job_and_activity_words_follow_the_task() {
    assert_eq!(TidyWorker::JOB_NAME, "The library tidy");
    let now = chrono::Utc::now();
    let run = |task, mode, status| TidyRun {
        run_id: "r1".into(),
        status,
        mode,
        task,
        started_utc: Some(now),
        finished_utc: Some(now),
        ..TidyRun::default()
    };
    let title = |run: TidyRun| {
        super::super::activity::tidy(&run, false, now)
            .expect("an item")
            .title
    };
    assert_eq!(
        title(run(TidyTask::FileLayout, TidyMode::Apply, TidyStatus::Running)),
        "Moving songs into the File layout"
    );
    assert_eq!(
        title(run(
            TidyTask::FileLayout,
            TidyMode::Preview,
            TidyStatus::Completed
        )),
        "Previewing the File layout tidy"
    );
    assert_eq!(
        title(run(TidyTask::FileLayout, TidyMode::Undo, TidyStatus::Running)),
        "Undoing the File layout tidy"
    );
    let waiting = super::super::activity::tidy(&TidyRun::default(), true, now).expect("an item");
    assert_eq!(waiting.title, "The library tidy");
}

#[tokio::test]
async fn every_tidy_endpoint_requires_a_navidrome_sign_in() {
    let state = AppState::for_tests(AppSettings::default());
    for (method, uri) in [
        (Method::GET, "/api/admin/tidy"),
        (Method::GET, "/api/admin/tidy/rows"),
        (Method::POST, "/api/admin/tidy/preview"),
        (Method::POST, "/api/admin/tidy/apply"),
        (Method::POST, "/api/admin/tidy/resume"),
        (Method::POST, "/api/admin/tidy/cancel"),
        (Method::POST, "/api/admin/tidy/undo"),
    ] {
        let body = (method == Method::POST).then(|| json!({ "scope": "OctoDownloads" }));
        let reply = send(&state, request(method.clone(), uri, None, true, body)).await;
        assert_eq!(
            reply.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri}: {}",
            reply.body
        );
    }
}

#[tokio::test]
async fn writes_without_the_admin_header_are_forbidden() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");
    for uri in [
        "/api/admin/tidy/preview",
        "/api/admin/tidy/apply",
        "/api/admin/tidy/undo",
    ] {
        let reply = send(
            &state,
            request(
                Method::POST,
                uri,
                Some(&token),
                false,
                Some(json!({ "scope": "OctoDownloads" })),
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri}: {}", reply.body);
    }
    assert!(!state.tidy_worker.is_pending());
}

#[tokio::test]
async fn a_second_run_while_one_is_waiting_is_409() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");

    let first = post(
        &state,
        &token,
        "/api/admin/tidy/preview",
        json!({ "scope": "OctoDownloads" }),
    )
    .await;
    let second = post(
        &state,
        &token,
        "/api/admin/tidy/preview",
        json!({ "scope": "WholeLibrary" }),
    )
    .await;

    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.body);
    assert_eq!(second.status, StatusCode::CONFLICT, "{}", second.body);
}

#[tokio::test]
async fn whole_library_apply_needs_the_path_typed_back() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");
    previewed(&state, TidyScope::WholeLibrary);

    let reply = post(
        &state,
        &token,
        "/api/admin/tidy/apply",
        json!({ "scope": "WholeLibrary", "confirm": "/somewhere/else" }),
    )
    .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply.body.contains(&state.tidy_worker.music_root()),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn apply_without_a_finished_preview_is_refused() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");

    let reply = post(
        &state,
        &token,
        "/api/admin/tidy/apply",
        json!({ "scope": "OctoDownloads" }),
    )
    .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("Preview first"), "{}", reply.body);
}

/// With no Navidrome to ask, its version cannot be confirmed: Apply stays off after a preview,
/// and the status says why.
#[tokio::test]
async fn apply_without_a_confirmed_navidrome_version_is_refused() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");
    previewed(&state, TidyScope::OctoDownloads);

    let reply = post(
        &state,
        &token,
        "/api/admin/tidy/apply",
        json!({ "scope": "OctoDownloads" }),
    )
    .await;
    let status = send(
        &state,
        request(Method::GET, "/api/admin/tidy", Some(&token), false, None),
    )
    .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("Navidrome"), "{}", reply.body);
    let status: Value = serde_json::from_str(&status.body).expect("json");
    assert_eq!(status["applyAllowed"], json!(false));
    assert!(
        status["applyBlockedReason"]
            .as_str()
            .is_some_and(|why| !why.is_empty())
    );
    assert_eq!(status["previewed"], json!(true));
    assert_eq!(status["rows"]["Guest"], json!(2));
}

#[tokio::test]
async fn apply_while_another_library_job_writes_is_409() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");
    previewed(&state, TidyScope::OctoDownloads);
    let genre = state
        .library_write_gate
        .begin_job("The genre re-tag")
        .await
        .expect("free");

    let reply = post(
        &state,
        &token,
        "/api/admin/tidy/apply",
        json!({ "scope": "OctoDownloads" }),
    )
    .await;

    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert!(
        reply.body.contains("The genre re-tag is changing"),
        "{}",
        reply.body
    );
    drop(genre);
}

#[tokio::test]
async fn undo_with_nothing_journaled_is_refused() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");

    let reply = post(&state, &token, "/api/admin/tidy/undo", json!({})).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("no tidy to undo"), "{}", reply.body);
}

#[tokio::test]
async fn resume_with_nothing_stopped_is_refused() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");

    let reply = post(&state, &token, "/api/admin/tidy/resume", json!({})).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_preview_is_paged_with_true_totals() {
    let state = AppState::for_tests(AppSettings::default());
    let token = state.browse_sessions.create("admin");
    previewed(&state, TidyScope::OctoDownloads);

    let page = send(
        &state,
        request(
            Method::GET,
            "/api/admin/tidy/rows?kind=Changes&offset=1&limit=1",
            Some(&token),
            false,
            None,
        ),
    )
    .await;
    let skips = send(
        &state,
        request(
            Method::GET,
            "/api/admin/tidy/rows?kind=Skip",
            Some(&token),
            false,
            None,
        ),
    )
    .await;

    let page: Value = serde_json::from_str(&page.body).expect("json");
    assert_eq!(page["total"], json!(2));
    assert_eq!(page["rows"].as_array().map(Vec::len), Some(1));
    assert_eq!(page["rows"][0]["id"], json!("b"));
    let skips: Value = serde_json::from_str(&skips.body).expect("json");
    assert_eq!(skips["total"], json!(1));
}

#[test]
fn a_running_tidy_shows_its_phase_in_the_activity_list() {
    let now = chrono::Utc::now();
    let running = TidyRun {
        run_id: "r1".into(),
        status: TidyStatus::Running,
        mode: TidyMode::Apply,
        phase: "Waiting for Navidrome to scan".into(),
        started_utc: Some(now),
        ..TidyRun::default()
    };
    let item = super::super::activity::tidy(&running, false, now).expect("an item");
    assert_eq!(item.title, "Tidying artists");
    assert_eq!(item.detail.as_deref(), Some("Waiting for Navidrome to scan"));
    assert!(super::super::activity::tidy(&TidyRun::default(), false, now).is_none());
}
