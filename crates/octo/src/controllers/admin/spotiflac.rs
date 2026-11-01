//! `POST /api/admin/spotiflac/test`: the SpotiFLAC page's Test. It checks what the page holds,
//! saved or not: that SpotiFLAC answers at the address with the token, that it has an extension
//! to download with, and that Octo can make the folder downloads land in. Nothing is downloaded.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use super::plumbing::{SECRET_PLACEHOLDER, error_json, ok};
use crate::app::AppState;
use crate::services::spotiflac::Connection;
use crate::services::spotiflac::assist::Viewer;

/// What the page sends: its fields as they are.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SpotiflacTestRequest {
    pub url: Option<String>,
    pub token: Option<String>,
    pub folder: Option<String>,
    pub enabled: Option<bool>,
}

/// The token the Test sends: the page only ever holds the placeholder for a saved token, and
/// testing it means the saved one, but only at the saved address, so a saved token is never sent
/// to an address someone typed. Anything other than the placeholder is the token as typed.
fn test_token(
    typed: Option<&str>,
    typed_url: &str,
    saved: &str,
    saved_url: &str,
) -> Result<String, &'static str> {
    match typed {
        Some(token) if token == SECRET_PLACEHOLDER => {
            if typed_url.eq_ignore_ascii_case(saved_url) {
                Ok(saved.trim().to_string())
            } else {
                Err("Retype the Mornye web token for the new address.")
            }
        }
        Some(token) => Ok(token.trim().to_string()),
        None => Ok(String::new()),
    }
}

pub async fn test_spotiflac(State(state): State<AppState>, body: Bytes) -> Response {
    let request: SpotiflacTestRequest = if body.is_empty() {
        SpotiflacTestRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(_) => {
                return error_json(
                    StatusCode::BAD_REQUEST,
                    "The Test needs the page's fields as JSON.",
                );
            }
        }
    };
    let saved = state.settings.current().spotiflac.clone();
    let url = octo_core::settings::normalize_server_url(request.url.as_deref().unwrap_or(""));
    let token = match test_token(
        request.token.as_deref(),
        &url,
        &saved.token,
        &saved.effective_url(),
    ) {
        Ok(token) => token,
        Err(words) => {
            return ok(&serde_json::json!({
                "ok": false,
                "message": words,
                "checks": [{ "state": "fail", "text": words }],
            }));
        }
    };
    let folder = octo_core::settings::SpotiflacSettings {
        folder: request.folder.unwrap_or_default(),
        ..Default::default()
    }
    .effective_folder();
    let download_path = download_path(&state);
    let report = state
        .spotiflac
        .test(
            &Connection { url, token },
            request.enabled.unwrap_or(saved.enabled),
            folder.as_deref(),
            &download_path,
        )
        .await;
    ok(&report.to_json())
}

/// Octo's music folder, as the Test and the Pass run use it.
fn download_path(state: &AppState) -> String {
    let fallback = state
        .settings
        .raw("Library:DownloadPath")
        .unwrap_or_else(|| "/music".to_string());
    state.navidrome_identity.effective_download_path(&fallback)
}

/// The host the dashboard was reached at, without its port, for Mornye's own page's address.
fn dashboard_host(headers: &axum::http::HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim();
    if let Some(literal) = host.strip_prefix('[') {
        return literal
            .split(']')
            .next()
            .map(|h| format!("[{h}]"))
            .unwrap_or_default();
    }
    host.split(':').next().unwrap_or_default().to_string()
}

/// `GET /api/admin/spotiflac/status`: how Mornye is doing, for the Mornye page's card.
pub async fn spotiflac_status(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    ok(&mornye_status(&state, &dashboard_host(&headers)).await)
}

/// Mornye's status for the card and the status row: reachable or not, its name and version,
/// its download services, each installed extension's last outcome, the paused checks, the songs
/// a Pass could ask for, and whether Octo's helper answers.
pub async fn mornye_status(state: &AppState, dashboard_host: &str) -> serde_json::Value {
    use octo_core::spotiflac::check::{extension_id, plain_dashes};
    let client = &state.spotiflac;
    let settings = client.settings();
    if !settings.is_on() {
        return serde_json::json!({ "on": false });
    }
    let connection = Connection::of(&settings);
    let bound = client.check.times.call_bound;
    let (info, view, extensions, helper) = tokio::join!(
        tokio::time::timeout(bound, client.info(&connection)),
        client.check_view(None),
        tokio::time::timeout(bound, client.extensions(&connection)),
        client.helper_state(),
    );
    let now = chrono::Utc::now();
    let (reachable, name, version, problem) = match info {
        Ok(Ok(info)) => (true, plain_dashes(&info.name), plain_dashes(&info.version), None),
        Ok(Err(e)) => (false, String::new(), String::new(), Some(e.to_string())),
        Err(_) => (
            false,
            String::new(),
            String::new(),
            Some(format!(
                "Mornye did not answer within {} seconds.",
                bound.as_secs()
            )),
        ),
    };
    let rows = view.as_ref().and_then(|view| view.rows.known().cloned());
    let services: Vec<serde_json::Value> = view
        .as_ref()
        .and_then(|view| view.services.known().cloned())
        .unwrap_or_default()
        .iter()
        .map(|service| serde_json::json!({ "id": service.id, "label": plain_dashes(&service.label) }))
        .collect();
    let answer = match extensions {
        Ok(Ok(answer)) => Some(answer),
        _ => None,
    };
    let installed: Vec<serde_json::Value> = answer
        .iter()
        .flat_map(|answer| answer.providers.iter().filter(|row| row.installed))
        .map(|row| {
            let id = extension_id(&row.provider);
            let health = answer.as_ref().and_then(|answer| answer.row(&row.provider));
            let (state, last_error) = match health {
                Some(h) if h.last_outcome.eq_ignore_ascii_case("failure") => {
                    ("failing", plain_dashes(h.last_error.trim()))
                }
                Some(h) if h.attempts > 0 => ("ok", String::new()),
                _ => ("untried", String::new()),
            };
            serde_json::json!({ "name": id, "state": state, "lastError": last_error })
        })
        .collect();
    let helper_paused = helper.as_ref().map(|h| h.paused.clone()).unwrap_or_default();
    let namespace_of_file = |file: &str| -> String {
        let stem = file
            .trim_start_matches('.')
            .trim_end_matches(".auth-backoff.json");
        stem.rsplit_once('-')
            .map(|(ns, _)| ns.to_string())
            .unwrap_or_default()
    };
    // Every check that is not passing: paused now, or not passed since it last failed (its pause
    // over, the next song lets it try).
    let checks: Vec<serde_json::Value> = rows
        .iter()
        .flatten()
        .filter_map(|row| {
            let paused = row.paused_at(now);
            let since = client.check.store.since(std::slice::from_ref(&row.label));
            if !paused && (row.working() || since.is_none()) {
                return None;
            }
            let namespace = row.key.rsplit_once('-').map(|(ns, _)| ns).unwrap_or_default();
            let gateway_asked = paused
                && helper_paused
                    .iter()
                    .any(|pause| pause.gateway_asked && namespace_of_file(&pause.file) == namespace);
            Some(serde_json::json!({
                "label": row.label,
                "extensions": row.extensions,
                "pausedUntilUtc": row.paused_until.filter(|_| paused).map(|t| octo_core::json::datetime::to_iso_millis(&t)),
                "since": since.map(|t| octo_core::json::datetime::to_iso_millis(&t)),
                "gatewayAsked": gateway_asked,
            }))
        })
        .collect();
    let helper_word = match &helper {
        Ok(_) => "ok",
        Err(_) if connection.token.is_empty() => "no token",
        Err(why) if why.0 == crate::services::spotiflac::assist::HELPER_REFUSED_TOKEN => "refused",
        Err(_) => "absent",
    };
    // Paused: the check keeps every extension a song could use. Pass helps only with a pause
    // Mornye made itself; one the gateway asked for is kept until its time.
    let paused = view
        .as_ref()
        .is_some_and(|view| view.pause(&settings.effective_services(), now).is_some());
    let check_songs = client.check_songs_left();
    let passable = paused
        && helper.is_ok()
        && check_songs > 0
        && checks.iter().any(|check| {
            check.get("pausedUntilUtc").is_some_and(|until| !until.is_null())
                && check.get("gatewayAsked").and_then(|v| v.as_bool()) == Some(false)
        });
    serde_json::json!({
        "on": true,
        "reachable": reachable,
        "name": name,
        "version": version,
        "webUrl": settings.effective_web_url(dashboard_host),
        "problem": problem,
        "utilitiesMissing": view.as_ref().is_some_and(|view| view.utilities_missing),
        "reportsCheck": rows.is_some(),
        "services": services,
        "extensions": installed,
        "checks": checks,
        "paused": paused,
        "passable": passable,
        "checkSongs": check_songs,
        "helper": helper_word,
    })
}

/// The Pass run as the dashboard reads it.
async fn assist_json(state: &AppState) -> serde_json::Value {
    let client = &state.spotiflac;
    let checks_left = client.check_songs_left();
    match client.assist_run() {
        Some(run) => {
            let (run_state, why) = run.state();
            serde_json::json!({
                "run": run.id,
                "state": run_state,
                "why": why,
                "browser": run.browser().map(|b| serde_json::json!({
                    "id": b.id, "width": b.width, "height": b.height, "seq": b.seq,
                })),
                "gatewayAsked": run.gateway_asked,
                "checkSongs": checks_left,
            })
        }
        None => match client.helper_state().await {
            Ok(_) => {
                serde_json::json!({ "state": "idle", "why": null, "browser": null, "gatewayAsked": [], "checkSongs": checks_left })
            }
            Err(why) => {
                serde_json::json!({ "state": "unavailable", "why": why.0, "browser": null, "gatewayAsked": [], "checkSongs": checks_left })
            }
        },
    }
}

/// `GET /api/admin/spotiflac/assist`: the Pass run, or idle.
pub async fn assist_state(State(state): State<AppState>) -> Response {
    ok(&assist_json(&state).await)
}

/// The dashboard page a Pass start or stop comes from, and for a stop the run it was on (null
/// while its start had not answered).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PassPage {
    pub tab: String,
    pub page: String,
    pub run: Option<String>,
}

impl PassPage {
    /// An empty body is a page with no name, which still has one seat.
    fn read(body: &[u8]) -> Option<Self> {
        if body.iter().all(u8::is_ascii_whitespace) {
            return Some(Self::default());
        }
        serde_json::from_slice(body).ok()
    }

    fn viewer(&self) -> Viewer {
        Viewer::new(&self.tab, &self.page)
    }
}

/// `POST /api/admin/spotiflac/assist/start`: starts "Pass Mornye's check", or answers why not.
pub async fn assist_start(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(page) = PassPage::read(&body) else {
        return error_json(StatusCode::BAD_REQUEST, "Send tab and page.");
    };
    let started = state
        .spotiflac
        .start_pass(&download_path(&state), &page.viewer())
        .await;
    let mut answer = assist_json(&state).await;
    if let Some(object) = answer.as_object_mut() {
        match started {
            Ok(_) => {
                object.insert("started".into(), serde_json::Value::Bool(true));
            }
            Err(why) => {
                object.insert("started".into(), serde_json::Value::Bool(false));
                object.insert("why".into(), serde_json::Value::String(why.0));
            }
        }
    }
    ok(&answer)
}

/// `POST /api/admin/spotiflac/assist/stop`: the page leaves the live Pass run, which stops once no
/// page is left on it (its song's job is cancelled and its folder removed), and answers the run
/// as it stands.
pub async fn assist_stop(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(page) = PassPage::read(&body) else {
        return error_json(StatusCode::BAD_REQUEST, "Send run, tab and page.");
    };
    let stopped = state
        .spotiflac
        .stop_pass(page.run.as_deref(), &page.viewer())
        .await;
    let mut answer = assist_json(&state).await;
    if let Some(object) = answer.as_object_mut() {
        object.insert("stopped".into(), serde_json::Value::Bool(stopped));
    }
    ok(&answer)
}

/// The frame a page asks for.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FrameQuery {
    pub browser: String,
    pub after: u64,
}

/// `GET /api/admin/spotiflac/assist/frame?browser=&after=`: the browser's newest screen, or 204.
pub async fn assist_frame(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<FrameQuery>,
) -> Response {
    use axum::response::IntoResponse;
    match state.spotiflac.assist_frame(&query.browser, query.after).await {
        Ok(Some(frame)) => (
            StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "image/jpeg".to_string()),
                (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
                (
                    axum::http::HeaderName::from_static("x-frame-seq"),
                    frame.seq.to_string(),
                ),
                (
                    axum::http::HeaderName::from_static("x-frame-width"),
                    frame.width.to_string(),
                ),
                (
                    axum::http::HeaderName::from_static("x-frame-height"),
                    frame.height.to_string(),
                ),
            ],
            frame.jpeg,
        )
            .into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(why) => error_json(StatusCode::CONFLICT, &why.0),
    }
}

/// One of the person's pointer events.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PointerEvent {
    pub browser: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub x: f64,
    pub y: f64,
}

/// `POST /api/admin/spotiflac/assist/input`: passes one pointer event to Mornye's check browser.
pub async fn assist_input(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(event) = serde_json::from_slice::<PointerEvent>(&body) else {
        return error_json(StatusCode::BAD_REQUEST, "Send browser, type, x and y.");
    };
    match state
        .spotiflac
        .assist_input(&event.browser, &event.kind, event.x, event.y)
        .await
    {
        Ok(()) => ok(&serde_json::json!({ "ok": true })),
        Err(why) => error_json(StatusCode::CONFLICT, &why.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_token_goes_only_to_the_saved_address() {
        assert_eq!(
            test_token(
                Some(SECRET_PLACEHOLDER),
                "http://spotiflac:8000",
                "saved",
                "http://spotiflac:8000"
            ),
            Ok("saved".to_string())
        );
        assert_eq!(
            test_token(
                Some(SECRET_PLACEHOLDER),
                "http://evil.example",
                "saved",
                "http://spotiflac:8000"
            ),
            Err("Retype the Mornye web token for the new address.")
        );
        assert_eq!(
            test_token(
                Some(" typed "),
                "http://evil.example",
                "saved",
                "http://spotiflac:8000"
            ),
            Ok("typed".to_string())
        );
        assert_eq!(
            test_token(None, "http://x", "saved", "http://x"),
            Ok(String::new())
        );
    }
}
