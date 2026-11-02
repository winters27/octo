//! The dashboard's import page. Add a public link, an exported file or a pasted list, see what
//! the library has and what it is missing, keep a list as a playlist, and the trickle that
//! fetches the rest. Lists belong to the Navidrome user signed in on the page, the same person an
//! Octo app signed in as sees them, so it needs the dashboard's sign-in.
//!
//! Every endpoint also takes `for=<Navidrome username>` in the query: the import, its lists,
//! playlists, downloads and status are then that person's, exactly as if they had imported it
//! themselves.

use axum::extract::{DefaultBodyLimit, FromRequest, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::Response;
use axum::routing::{get, post};
use octo_core::common::dotnet::{eq_ignore_case, to_lower_invariant};
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use super::json_body::bind_body;
use super::json_body::{FromBody, Kind, Member, Shape};
use super::plumbing::{error_json, header_value, object_result, ok, request_cookie};
use super::request_binding::query;
use crate::app::AppState;
use crate::http::error::{AppError, AppResult, problem};
use crate::http::routes::RouteSet;
use crate::services::imports::ImportActionResult;
use crate::services::imports::import_file_reader::ImportFileReader;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/imports", get(overview))
        .route("/api/admin/imports/people", get(people))
        .route("/api/admin/imports/lists/{id}", get(list).delete(remove))
        .route("/api/admin/imports/link", post(link))
        .route(
            "/api/admin/imports/file",
            // [RequestSizeLimit(ImportFileReader.MaxBytes)]
            post(file).layer(DefaultBodyLimit::max(ImportFileReader::MAX_BYTES as usize)),
        )
        .route("/api/admin/imports/text", post(text))
        .route("/api/admin/imports/services", get(services))
        .route("/api/admin/imports/lists/{id}/playlist", post(playlist))
        .route("/api/admin/imports/lists/{id}/stream", post(stream))
        .route("/api/admin/imports/lists/{id}/fetch", post(fetch))
        .route("/api/admin/imports/lists/{id}/songs", post(songs))
        .route("/api/admin/imports/lists/{id}/refresh", post(refresh))
        .route("/api/admin/imports/trickle/pause", post(pause))
        .route("/api/admin/imports/trickle/retry", post(retry))
        .route("/api/admin/imports/trickle/skip", post(skip))
        .route("/api/admin/imports/trickle/clear", post(clear))
}

/// `SignedIn(header)`: `NavidromeUserOf(cookie ?? header)`. Never the recovery code, and the
/// cookie is not renewed here.
fn signed_in(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let token = request_cookie(headers, super::plumbing::BROWSE_COOKIE_NAME)
        .or_else(|| header_value(headers, super::plumbing::BROWSE_TOKEN_HEADER));
    state.browse_sessions.navidrome_user_of(token.as_deref())
}

/// `Unauthorized(new { error = ... })`.
fn sign_in() -> Response {
    error_json(
        StatusCode::UNAUTHORIZED,
        "Sign in with your Navidrome account first.",
    )
}

/// The person a request acts for: the Navidrome user its `for` names, spelled the way Navidrome
/// spells them, or the signed-in person when `for` is absent or names them. A name Navidrome
/// has no user for is answered 400.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
async fn acting_for(state: &AppState, uri: &Uri, signed_in: String) -> Result<String, Response> {
    let Some(wanted) = query(uri, "for")
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
    else {
        return Ok(signed_in);
    };
    if eq_ignore_case(&wanted, &signed_in) {
        return Ok(signed_in);
    }
    let users = state.navidrome_admin.list_users().await.map_err(|e| {
        error_json(
            StatusCode::BAD_GATEWAY,
            &format!("Octo could not read the users on this server: {e}"),
        )
    })?;
    users
        .iter()
        .map(|user| user.user_name())
        .find(|name| eq_ignore_case(name, &wanted))
        .map(str::to_string)
        .ok_or_else(|| {
            error_json(
                StatusCode::BAD_REQUEST,
                &format!("There is no user named {wanted} on this server."),
            )
        })
}

/// The signed-in person and the person the request acts for, or the answer that refuses it.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
async fn whose(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Result<(String, String), Response> {
    let admin = signed_in(state, headers).ok_or_else(sign_in)?;
    let user = acting_for(state, uri, admin.clone()).await?;
    Ok((admin, user))
}

/// Logs a new import the signed-in person started for someone else.
fn note_import_for(admin: &str, user: &str) {
    if !eq_ignore_case(admin, user) {
        info!("Import for {user} started by {admin}");
    }
}

/// `result.Ok ? Ok(result) : BadRequest(result)`.
fn answer(result: &ImportActionResult) -> Response {
    object_result(
        if result.ok {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        },
        result,
    )
}

/// A failure the import service did not handle, as the error envelope answers it.
pub(crate) fn app_error(e: anyhow::Error) -> AppError {
    match e.downcast::<reqwest::Error>() {
        Ok(e) => AppError::from_reqwest(e),
        Err(e) => AppError::Internal(e),
    }
}

// ---- Request bodies ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct OnRequest {
    on: bool,
}

impl FromBody for OnRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.ImportsController+OnRequest",
        by_constructor: true,
        members: &[Member {
            name: "on",
            kind: Kind::Bool,
        }],
    };
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PausedRequest {
    paused: bool,
}

impl FromBody for PausedRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.ImportsController+PausedRequest",
        by_constructor: true,
        members: &[Member {
            name: "paused",
            kind: Kind::Bool,
        }],
    };
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct KeysRequest {
    keys: Option<Vec<Option<String>>>,
}

impl FromBody for KeysRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.ImportsController+KeysRequest",
        by_constructor: true,
        members: &[Member {
            name: "keys",
            kind: Kind::TextList,
        }],
    };
}

impl KeysRequest {
    fn keys(&self) -> Option<Vec<String>> {
        self.keys
            .as_ref()
            .map(|keys| keys.iter().map(|k| k.clone().unwrap_or_default()).collect())
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LinkRequest {
    url: Option<String>,
}

impl FromBody for LinkRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.ImportsController+LinkRequest",
        by_constructor: true,
        members: &[Member {
            name: "url",
            kind: Kind::Text,
        }],
    };
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TextRequest {
    name: Option<String>,
    text: Option<String>,
}

impl FromBody for TextRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.ImportsController+TextRequest",
        by_constructor: true,
        members: &[
            Member {
                name: "name",
                kind: Kind::Text,
            },
            Member {
                name: "text",
                kind: Kind::Text,
            },
        ],
    };
}

async fn split(request: Request) -> (axum::http::request::Parts, axum::body::Bytes) {
    super::request_binding::split(request).await
}

// ---- The endpoints ----------------------------------------------------------------------------

/// Where a free AcoustID application key is had.
pub const ACOUST_ID_NEW_KEY: &str = "https://acoustid.org/new-application";

/// The import card's line while an import's songs are checked with Octo's own AcoustID key, at the
/// pace work nobody waits on goes, for want of a key of the user's own.
pub fn built_in_key_pace() -> String {
    format!(
        "Octo checks these songs with its own AcoustID key, one every {} seconds. A free key of your own in Settings checks them faster",
        crate::services::fingerprint::AcoustIdRateLimiter::BULK_GAP.as_secs()
    )
}

async fn overview(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let (_, user) = match whose(&state, &headers, &uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    let mut overview = serde_json::to_value(state.imports.overview(&user)).unwrap_or_default();
    // An import's songs wait their turn on Octo's own AcoustID key: the card says a key of the
    // person's own checks them faster, and where to get one.
    if state.acoust_id.bulk_on_built_in_key()
        && let Some(fields) = overview.as_object_mut()
    {
        fields.insert(
            "acoustIdKey".into(),
            json!({ "text": built_in_key_pace(), "link": ACOUST_ID_NEW_KEY }),
        );
    }
    ok(&json!({ "user": user, "overview": overview }))
}

/// Who the page can import for: the signed-in person first, then every other Navidrome user by
/// name, each marked when they are a Family member (their songs go to their own library).
async fn people(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(admin) = signed_in(&state, &headers) else {
        return sign_in();
    };
    let family_on = state.settings.current().family.enabled;
    let member = |name: &str| family_on && state.family_store.member(name).is_some();
    let (mut users, problem) = match state.navidrome_admin.list_users().await {
        Ok(users) => (users, None),
        Err(e) => (
            Vec::new(),
            Some(format!("Octo could not read the users on this server: {e}")),
        ),
    };
    users.retain(|user| !user.user_name().trim().is_empty());
    users.sort_by_key(|user| to_lower_invariant(user.user_name()));
    let own_name = users
        .iter()
        .find(|user| eq_ignore_case(user.user_name(), &admin))
        .map(|user| user.name().to_string())
        .unwrap_or_default();
    let mut rows = vec![json!({
        "username": admin,
        "name": own_name,
        "member": member(&admin),
        "you": true,
    })];
    rows.extend(
        users
            .iter()
            .filter(|user| !eq_ignore_case(user.user_name(), &admin))
            .map(|user| {
                json!({
                    "username": user.user_name(),
                    "name": user.name(),
                    "member": member(user.user_name()),
                    "you": false,
                })
            }),
    );
    ok(&json!({ "signedIn": admin, "users": rows, "problem": problem }))
}

async fn list(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (_, user) = match whose(&state, &headers, &uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    match state.imports.detail(&user, &id) {
        Some(detail) => ok(&detail),
        None => error_json(StatusCode::NOT_FOUND, "There is no such list."),
    }
}

async fn link(State(state): State<AppState>, request: Request) -> AppResult {
    let (parts, body) = split(request).await;
    let request: LinkRequest = match bind_body::<LinkRequest>(&parts.headers, &body, "request") {
        Ok(request) => request,
        Err(refused) => return Ok(refused),
    };
    let (admin, user) = match whose(&state, &parts.headers, &parts.uri).await {
        Ok(people) => people,
        Err(refused) => return Ok(refused),
    };
    note_import_for(&admin, &user);
    // Not tied to the request: a closed tab must not leave a list half read.
    let imports = state.imports.clone();
    let url = request.url.unwrap_or_default();
    let result = tokio::spawn(async move { imports.add_link(&user, &url).await })
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .map_err(app_error)?;
    Ok(answer(&result))
}

async fn file(State(state): State<AppState>, request: Request) -> AppResult {
    // An uploaded file: the action takes a form only; any other body finds no action (404).
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase());
    if content_type
        .as_deref()
        .is_some_and(|media| media != "multipart/form-data")
    {
        return Ok(problem(StatusCode::NOT_FOUND));
    }
    let (admin, user) = match whose(&state, request.headers(), request.uri()).await {
        Ok(people) => people,
        Err(refused) => return Ok(refused),
    };
    let choose = || answer(&ImportActionResult::failed("Choose a file first."));
    if content_type.is_none() {
        return Ok(choose());
    }
    let mut form = match axum::extract::Multipart::from_request(request, &state).await {
        Ok(form) => form,
        Err(_) => return Ok(choose()),
    };
    let mut found: Option<(String, Vec<u8>)> = None;
    loop {
        let field = match form.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return Ok(problem(StatusCode::BAD_REQUEST)),
        };
        let wanted = found.is_none()
            && field.name().is_some_and(|n| n.eq_ignore_ascii_case("file"))
            && field.file_name().is_some();
        if !wanted {
            continue;
        }
        let name = field.file_name().unwrap_or_default().to_string();
        let bytes = match field.bytes().await {
            Ok(bytes) => bytes.to_vec(),
            Err(_) => return Ok(problem(StatusCode::BAD_REQUEST)),
        };
        found = Some((name, bytes));
    }
    let Some((name, bytes)) = found.filter(|(_, bytes)| !bytes.is_empty()) else {
        return Ok(choose());
    };
    note_import_for(&admin, &user);
    let imports = state.imports.clone();
    let result = tokio::spawn(async move { imports.add_file(&user, &name, &bytes).await })
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok(answer(&result))
}

/// A pasted list, one song a line, kept under its name ("Pasted list" when it has none).
async fn text(State(state): State<AppState>, request: Request) -> AppResult {
    let (parts, body) = split(request).await;
    let request: TextRequest = match bind_body::<TextRequest>(&parts.headers, &body, "request") {
        Ok(request) => request,
        Err(refused) => return Ok(refused),
    };
    let (admin, user) = match whose(&state, &parts.headers, &parts.uri).await {
        Ok(people) => people,
        Err(refused) => return Ok(refused),
    };
    let text = request.text.unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(answer(&ImportActionResult::failed("Paste a list first.")));
    }
    note_import_for(&admin, &user);
    let imports = state.imports.clone();
    let name = request.name;
    let result = tokio::spawn(async move { imports.add_text(&user, name.as_deref(), &text).await })
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok(answer(&result))
}

/// Where people can bring their music from, for the dashboard's guide.
async fn services(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    if let Err(refused) = whose(&state, &headers, &uri).await {
        return refused;
    }
    ok(&state.imports.services())
}

async fn playlist(State(state): State<AppState>, Path(id): Path<String>, request: Request) -> AppResult {
    let (parts, body) = split(request).await;
    let request: OnRequest = match bind_body::<OnRequest>(&parts.headers, &body, "request") {
        Ok(request) => request,
        Err(refused) => return Ok(refused),
    };
    let (_, user) = match whose(&state, &parts.headers, &parts.uri).await {
        Ok(people) => people,
        Err(refused) => return Ok(refused),
    };
    let imports = state.imports.clone();
    let result = tokio::spawn(async move { imports.set_playlist(&user, &id, request.on).await })
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok(answer(&result))
}

async fn stream(State(state): State<AppState>, Path(id): Path<String>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: OnRequest = match bind_body::<OnRequest>(&parts.headers, &body, "request") {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    let (_, user) = match whose(&state, &parts.headers, &parts.uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    answer(&state.imports.set_streaming(&user, &id, request.on))
}

async fn fetch(State(state): State<AppState>, Path(id): Path<String>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: OnRequest = match bind_body::<OnRequest>(&parts.headers, &body, "request") {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    let (_, user) = match whose(&state, &parts.headers, &parts.uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    answer(&state.imports.set_get_missing(&user, &id, request.on))
}

async fn songs(State(state): State<AppState>, Path(id): Path<String>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: KeysRequest = match bind_body::<KeysRequest>(&parts.headers, &body, "request") {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    let (_, user) = match whose(&state, &parts.headers, &parts.uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    answer(
        &state
            .imports
            .get_songs(&user, &id, &request.keys().unwrap_or_default()),
    )
}

async fn refresh(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> AppResult {
    let (_, user) = match whose(&state, &headers, &uri).await {
        Ok(people) => people,
        Err(refused) => return Ok(refused),
    };
    let imports = state.imports.clone();
    let result = tokio::spawn(async move { imports.refresh(&user, &id).await })
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .map_err(app_error)?;
    Ok(answer(&result))
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (_, user) = match whose(&state, &headers, &uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    answer(&state.imports.remove(&user, &id))
}

async fn pause(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: PausedRequest = match bind_body::<PausedRequest>(&parts.headers, &body, "request") {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    let (_, user) = match whose(&state, &parts.headers, &parts.uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    answer(&state.imports.pause(&user, request.paused))
}

async fn retry(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: KeysRequest = match bind_body::<KeysRequest>(&parts.headers, &body, "request") {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    let (_, user) = match whose(&state, &parts.headers, &parts.uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    answer(&state.imports.retry(&user, request.keys().as_deref()))
}

async fn skip(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: KeysRequest = match bind_body::<KeysRequest>(&parts.headers, &body, "request") {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    let (_, user) = match whose(&state, &parts.headers, &parts.uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    answer(&state.imports.skip(&user, &request.keys().unwrap_or_default()))
}

async fn clear(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let (_, user) = match whose(&state, &headers, &uri).await {
        Ok(people) => people,
        Err(refused) => return refused,
    };
    answer(&state.imports.clear_finished(&user))
}

#[cfg(test)]
#[path = "imports_tests.rs"]
mod tests;
