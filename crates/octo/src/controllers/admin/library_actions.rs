//! The library actions, questions and checks: the action history and its Allow downloading again,
//! the ratings not acted on, the notices, the duplicate scan, the library Review sweep, the weekly upgrade's status and the
//! song path resolver.
//!
//! Session-gated, because these list filenames and usernames. Every /api/admin endpoint sits
//! behind the dashboard sign-in, and the rule for anything that changes library files on demand
//! is: a Navidrome admin's own sign-in (never the recovery code), acting as that person, who must
//! pass the same gates as in the apps (library actions on, on the allowed list, dry run only
//! rehearses). Nothing here ever deletes a quarantined file; the retention sweep alone does.

use std::path::Path;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use serde_json::{Value, json};

use serde::Deserialize;

use super::json_body::{FromBody, Shape, text_record};
use super::request_binding::{
    bind_body, browse_user, error, ok, query, sign_in, split, status, utc, utc_opt,
};
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::library::library_action_journal::action_name;
use crate::services::library::navidrome_song_path_resolver::PathSource;
use crate::services::library::notice_queue::NamedEnum;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/library-actions", get(get_library_actions))
        .route("/api/admin/library-actions/allow-download", post(allow_download))
        .route(
            "/api/admin/library-actions/ignored-ratings",
            get(get_ignored_ratings),
        )
        .route("/api/admin/notices", get(get_notices))
        .route("/api/admin/duplicates/scan", post(scan_duplicates))
        .route("/api/admin/review-sweep", get(get_review_sweep))
        .route("/api/admin/review-sweep/start", post(start_review_sweep))
        .route("/api/admin/review-sweep/pause", post(pause_review_sweep))
        .route("/api/admin/review-sweep/reset", post(reset_review_sweep))
        .route(
            "/api/admin/review-sweep/recheck-octo",
            post(recheck_octo_downloads),
        )
        .route("/api/admin/youtube-check", get(get_youtube_check))
        .route("/api/admin/youtube-check/start", post(start_youtube_check))
        .route("/api/admin/youtube-check/pause", post(pause_youtube_check))
        .route("/api/admin/youtube-check/resume", post(resume_youtube_check))
        .route("/api/admin/quality-upgrade", get(get_quality_upgrade))
        .route("/api/admin/library/resolve", get(resolve_library_song))
}

/// A 202 Accepted with this body.
fn accepted(body: &Value) -> Response {
    status(StatusCode::ACCEPTED, body)
}

/// The name a path source is written with.
pub fn path_source_name(source: PathSource) -> &'static str {
    match source {
        PathSource::NativeApi => "NativeApi",
        PathSource::SubsonicGetSong => "SubsonicGetSong",
        PathSource::LocalMappings => "LocalMappings",
        PathSource::None => "None",
    }
}

/// `relative` joined under `root`; a rooted `relative` (or an empty `root`) is taken as is.
fn path_combine(root: &str, relative: &str) -> String {
    if relative.starts_with('/') || root.is_empty() {
        return relative.to_string();
    }
    Path::new(root).join(relative).to_string_lossy().into_owned()
}

/// The library actions history, the 200 most recent.
async fn get_library_actions(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let settings = state.settings.current().library_actions.clone();
    let entries: Vec<Value> = state
        .library_action_journal
        .recent(200)
        .iter()
        .map(|entry| {
            json!({
                "action": action_name(entry.action),
                "navidromeId": entry.navidrome_id,
                "username": entry.username,
                "title": entry.title,
                "artist": entry.artist,
                "album": entry.album,
                "state": entry.state.name(),
                "detail": entry.detail,
                "dryRun": entry.dry_run,
                "sourcePath": entry.source_path,
                "quarantinePath": entry.quarantine_path,
                "resolution": entry.resolution.map(path_source_name),
                "atUtc": utc(&entry.at_utc),
                "key": entry.key,
                // A song removed from disk that a download would refuse; Allow downloading again
                // lifts it.
                "blocksDownloads": entry.blocks_downloads(),
            })
        })
        .collect();
    browse.finish(ok(&json!({
        "enabled": settings.enabled,
        "dryRun": settings.dry_run,
        "hasAdminIdentity": state.navidrome_identity.has_admin_identity(),
        "allowedUsers": settings.allowed_users,
        "quarantine": path_combine(
            &state.navidrome_song_path_resolver.music_root(),
            &settings.effective_quarantine_directory()
        ),
        "entries": entries,
    })))
}

/// The last star ratings that asked for an action and got none, or only a rehearsal, newest
/// first, each with the reason. Gated like the action history: it names songs and people.
async fn get_ignored_ratings(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let entries: Vec<Value> = state
        .library_action_rating_worker
        .ignored()
        .iter()
        .map(|rating| {
            json!({
                "atUtc": utc(&rating.at_utc),
                "username": rating.username,
                "songId": rating.song_id,
                "artist": rating.artist,
                "title": rating.title,
                "rating": rating.rating,
                "action": action_name(rating.action),
                "reason": rating.reason,
            })
        })
        .collect();
    browse.finish(ok(&json!({ "entries": entries })))
}

/// The allow-download body: an optional `key`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AllowDownloadRequest {
    key: Option<String>,
}

impl FromBody for AllowDownloadRequest {
    const SHAPE: &'static Shape =
        text_record!("Octo.Controllers.AdminController+AllowDownloadRequest", "key");
}

/// The history's Allow downloading again: a song removed with Delete from disk may be downloaded
/// once more. Its file stays in the trash. Gated like the history it sits in.
async fn allow_download(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: AllowDownloadRequest = match bind_body(&parts, &body) {
        Ok(r) => r,
        Err(answer) => return *answer,
    };
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let key = request.key.filter(|key| !key.trim().is_empty());
    if key.is_none_or(|key| state.library_action_journal.allow_downloads(&key) == 0) {
        return browse.finish(error(
            StatusCode::BAD_REQUEST,
            "That song is not kept from downloading.",
        ));
    }
    state.library_action_journal.flush();
    browse.finish(ok(&json!({ "ok": true })))
}

/// What Octo has asked people about (#47, #53) and how they answered, newest first. Gated like
/// the action history: it names files and people.
async fn get_notices(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let entries: Vec<Value> = state
        .notice_queue
        .recent(200)
        .iter()
        .map(|entry| {
            let mut json = json!({
                "kind": entry.kind.name(),
                "username": entry.username,
                "artist": entry.artist,
                "title": entry.title,
                "album": entry.album,
                "state": entry.state.name(),
                "reason": entry.reason,
                "origin": entry.origin.name(),
                "submitted": entry.submitted,
                "createdUtc": utc(&entry.created_utc),
                "resolvedUtc": utc_opt(&entry.resolved_utc),
            });
            // AcoustID's numbers and what it matched or came closest to, only when they are known.
            if let Some(fields) = json.as_object_mut() {
                if let Some(score) = entry.acoust_id_score {
                    fields.insert("acoustIdScore".into(), json!(score));
                }
                if let Some(threshold) = entry.threshold {
                    fields.insert("threshold".into(), json!(threshold));
                }
                if let Some(matched) = entry.matched.as_deref().filter(|m| !m.is_empty()) {
                    fields.insert("matched".into(), json!(matched));
                }
                if let Some(closest) = entry.closest.as_deref().filter(|c| !c.is_empty()) {
                    fields.insert("closest".into(), json!(closest));
                }
            }
            json
        })
        .collect();
    let duplicate_scan = state.duplicate_scan_worker.last_result().map(|r| {
        json!({
            "atUtc": utc(&r.at_utc),
            "tracks": r.tracks,
            "groups": r.groups,
            "added": r.added,
            "complete": r.complete,
        })
    });
    browse.finish(ok(&json!({
        "entries": entries,
        "duplicateScan": duplicate_scan,
    })))
}

/// The Duplicates card's "Scan now". Read-only, like a radio refresh, so the admin request
/// guard is enough; the walk runs in the background and the result shows with the questions.
async fn scan_duplicates(State(state): State<AppState>) -> Response {
    let settings = state.settings.current().library_actions.clone();
    if !settings.enabled || !settings.duplicates_enabled {
        return error(
            StatusCode::BAD_REQUEST,
            "Turn on library actions and the Duplicates playlist first.",
        );
    }
    if !state.navidrome_identity.has_admin_identity() {
        return error(
            StatusCode::BAD_REQUEST,
            "Octo needs a Navidrome admin credential to read the whole library.",
        );
    }
    state.duplicate_scan_worker.request_scan();
    accepted(&json!({ "ok": true, "queued": true }))
}

/// The library Review sweep (#72): how far it has got and why it is waiting. Counts only. The
/// replacing, the re-check of Octo's downloads and a changed match score are told only when
/// there is something to tell.
async fn get_review_sweep(State(state): State<AppState>) -> Response {
    let s = state.library_review_sweep_worker.status();
    let mut body = json!({
        "state": s.state,
        "reason": s.reason,
        "paused": s.paused,
        "position": s.position,
        "total": s.total,
        "pass": s.pass,
        "found": s.found,
        "open": s.open,
        "fine": s.fine,
        "undecodable": s.undecodable,
        "keeper": s.keeper,
        "perHour": s.per_hour,
        "lastCheckedUtc": utc_opt(&s.last_checked_utc),
        "passFinishedUtc": utc_opt(&s.pass_finished_utc),
    });
    if let Some(fields) = body.as_object_mut() {
        if s.replaced > 0 {
            fields.insert("replaced".into(), json!(s.replaced));
        }
        if s.auto_replace {
            fields.insert(
                "autoReplace".into(),
                json!({
                    "perDay": s.auto_replace_per_day,
                    "replacedToday": s.today.replaced,
                    "failedToday": s.today.failed,
                }),
            );
        }
        if let Some(recheck) = &s.recheck {
            fields.insert(
                "recheck".into(),
                json!({
                    "running": recheck.running,
                    "position": recheck.position,
                    "total": recheck.total,
                    "checked": recheck.checked,
                    "found": recheck.found,
                    "replaced": recheck.replaced,
                    "threshold": recheck.threshold,
                    "startedUtc": utc_opt(&recheck.started_utc),
                    "finishedUtc": utc_opt(&recheck.finished_utc),
                }),
            );
        }
        if s.threshold_changed {
            fields.insert("thresholdChanged".into(), json!(true));
        }
    }
    ok(&body)
}

async fn start_review_sweep(State(state): State<AppState>) -> Response {
    let settings = state.settings.current().library_actions.clone();
    if !settings.enabled || !settings.review_enabled || settings.effective_review_sweep_per_hour() == 0 {
        return error(
            StatusCode::BAD_REQUEST,
            "Turn on library actions and Review, and set how many songs an hour to check, first.",
        );
    }
    state.library_review_sweep_worker.set_paused(false);
    accepted(&json!({ "ok": true }))
}

async fn pause_review_sweep(State(state): State<AppState>) -> Response {
    state.library_review_sweep_worker.set_paused(true);
    accepted(&json!({ "ok": true }))
}

/// Check every song again from the start. Songs already asked about stay answered.
async fn reset_review_sweep(State(state): State<AppState>) -> Response {
    state.library_review_sweep_worker.reset();
    accepted(&json!({ "ok": true }))
}

/// "Re-check Octo's downloads": forgets when Octo's own downloads were checked and checks
/// each once more, then stops, even with the sweep's own rate at 0. Songs already asked about
/// stay answered.
async fn recheck_octo_downloads(State(state): State<AppState>) -> Response {
    let settings = state.settings.current().library_actions.clone();
    if !settings.enabled || !settings.review_enabled {
        return error(
            StatusCode::BAD_REQUEST,
            "Turn on library actions and Review first.",
        );
    }
    let files = state.library_review_sweep_worker.recheck_octo().await;
    accepted(&json!({ "ok": true, "files": files }))
}

/// The one-time YouTube check: how far it has got and why it is waiting. Counts and the name of
/// whoever is asked, never a file or a song, like the Review sweep.
async fn get_youtube_check(State(state): State<AppState>) -> Response {
    let s = state.library_youtube_check.status();
    ok(&json!({
        "state": s.state,
        "reason": s.reason,
        "running": s.running,
        "paused": s.paused,
        "position": s.position,
        "total": s.total,
        "done": s.done,
        "asked": s.asked,
        "replaced": s.replaced,
        "alike": s.alike,
        "unsure": s.unsure,
        "noCopy": s.no_copy,
        "known": s.known,
        "open": s.open,
        "keeper": s.keeper,
        "perHour": s.per_hour,
        "startedUtc": utc_opt(&s.started_utc),
        "finishedUtc": utc_opt(&s.finished_utc),
    }))
}

/// "Check every song against YouTube": forgets earlier answers and starts one run over the
/// library. Needs the same things as the Review sweep, and a person to ask.
async fn start_youtube_check(State(state): State<AppState>) -> Response {
    let current = state.settings.current();
    let settings = &current.library_actions;
    let keeper = crate::services::library::LibraryReviewSweepWorker::keeper(
        settings,
        current.subsonic.admin_username.as_deref(),
    );
    if !settings.enabled
        || !settings.review_enabled
        || settings.effective_youtube_check_per_hour() == 0
        || keeper.is_none()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Turn on library actions and Review, set how many songs an hour to check, and put someone on the allowed list, first.",
        );
    }
    state.library_youtube_check.start();
    accepted(&json!({ "ok": true }))
}

async fn pause_youtube_check(State(state): State<AppState>) -> Response {
    state.library_youtube_check.set_paused(true);
    accepted(&json!({ "ok": true }))
}

/// Carries on where a pause stopped it, with the answers it has.
async fn resume_youtube_check(State(state): State<AppState>) -> Response {
    state.library_youtube_check.set_paused(false);
    accepted(&json!({ "ok": true }))
}

/// The weekly upgrade's last run and next one. Times and an outcome only, no file or person, so
/// the admin request guard is enough, like the duplicate scan.
async fn get_quality_upgrade(State(state): State<AppState>) -> Response {
    let s = state.quality_upgrade_worker.status();
    ok(&json!({
        "perWeek": s.per_week,
        "off": s.off,
        "lastRunUtc": utc_opt(&s.last_run_utc),
        "lastOutcome": s.last_outcome,
        "nextDueUtc": utc_opt(&s.next_due_utc),
        "tried": s.tried,
    }))
}

/// Ask what file a Navidrome song id resolves to, and say which leg answered.
///
/// Read-only, and deliberately shipped before anything that acts on the answer: Navidrome's
/// Subsonic `path` is synthesized from tags unless a player opts in, so on some libraries it
/// names a file that exists and is a different recording. This is how that gets checked against
/// a real library before any of it is load-bearing.
///
/// Session-gated because the answer is a filesystem path.
async fn resolve_library_song(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let Some(id) = query(&parts.uri, "id").filter(|id| !id.trim().is_empty()) else {
        return browse.finish(error(
            StatusCode::BAD_REQUEST,
            "Pass the Navidrome song id as ?id=",
        ));
    };
    let resolved = state.navidrome_song_path_resolver.resolve(&id).await;
    browse.finish(ok(&json!({
        "id": id,
        "resolved": resolved.is_some(),
        "musicRoot": state.navidrome_song_path_resolver.music_root(),
        "hasAdminIdentity": state.navidrome_identity.has_admin_identity(),
        "path": resolved.as_ref().map(|r| r.absolute_path.clone()),
        "source": path_source_name(resolved.as_ref().map_or(PathSource::None, |r| r.source)),
        "sizeBytes": resolved.as_ref().map(|r| r.size_bytes),
        "artist": resolved.as_ref().map(|r| r.artist.clone()),
        "title": resolved.as_ref().map(|r| r.title.clone()),
        "album": resolved.as_ref().map(|r| r.album.clone()),
    })))
}
