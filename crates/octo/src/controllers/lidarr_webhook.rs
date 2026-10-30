//! `POST /api/lidarr/webhook`: Lidarr's Webhook connection tells Octo when it imported an
//! album, so a heart waiting on it looks at once, and an album that came in after its heart
//! stopped waiting (or after a restart) is still taken through the download pipeline.
//!
//! Off until `Lidarr:WebhookSecret` is set: until then the address answers 404. Lidarr sends
//! the secret as the password of HTTP Basic authentication (any user name); it is compared in
//! constant time. Wrong passwords are throttled per address as dashboard sign-ins are: after ten
//! in fifteen minutes the address gets 429 until the oldest is fifteen minutes old.

use std::net::SocketAddr;

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::admin::SignInThrottle;
use crate::services::admin::sign_in_throttle::key_of;
use crate::services::lidarr::{LidarrImportEvent, LidarrImportOutcome};

/// The address Lidarr posts to.
pub const WEBHOOK_PATH: &str = "/api/lidarr/webhook";

pub fn routes() -> RouteSet {
    RouteSet::new().route(WEBHOOK_PATH, post(lidarr_webhook))
}

async fn lidarr_webhook(
    State(state): State<AppState>,
    remote: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let secret = state
        .settings
        .current()
        .lidarr
        .webhook_secret
        .clone()
        .unwrap_or_default();
    if secret.trim().is_empty() {
        return plain(StatusCode::NOT_FOUND, Value::Null);
    }
    let throttle = &state.lidarr_webhook_throttle;
    let address = key_of(remote.map(|Extension(ConnectInfo(addr))| addr));
    if let Some(wait) = throttle.retry_after(&address) {
        let mut response = plain(
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "error": format!(
                    "Too many wrong webhook passwords. Try again in {}.",
                    SignInThrottle::minutes(wait)
                )
            }),
        );
        if let Ok(value) = HeaderValue::from_str(&SignInThrottle::retry_after_seconds(wait).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    if !authorized(&headers, &secret) {
        throttle.failed(&address);
        let mut response = plain(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "Wrong webhook password." }),
        );
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Octo\""),
        );
        return response;
    }
    throttle.succeeded(&address);
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return plain(
            StatusCode::BAD_REQUEST,
            json!({ "error": "The body is not JSON." }),
        );
    };
    let event_type = payload
        .get("eventType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "Test" => {
            info!("Lidarr's webhook test reached Octo");
            plain(StatusCode::OK, json!({ "ok": true }))
        }
        "Download" => {
            let Some(event) = import_event(&payload) else {
                return plain(StatusCode::OK, json!({ "ok": true, "taken": "ignored" }));
            };
            let outcome = state.lidarr_hearts.lidarr_imported(event.clone()).await;
            let taken = match outcome {
                LidarrImportOutcome::Woke => "waiting",
                LidarrImportOutcome::Late => "late",
                LidarrImportOutcome::NotOurs => "ignored",
            };
            if outcome != LidarrImportOutcome::NotOurs {
                info!(
                    "Lidarr imported {} file(s) of '{} - {}' ({})",
                    event.paths.len(),
                    event.artist.as_deref().unwrap_or(""),
                    event.album.as_deref().unwrap_or(""),
                    event.foreign_album_id
                );
            }
            plain(StatusCode::OK, json!({ "ok": true, "taken": taken }))
        }
        // Lidarr's health came back, or it grabbed a release: a heart waiting for its indexers
        // looks whether they search again.
        "HealthRestored" | "Grab" => {
            state.lidarr_hearts.lidarr_health_event();
            plain(StatusCode::OK, json!({ "ok": true }))
        }
        other => {
            if other.is_empty() {
                warn!("Lidarr's webhook sent no eventType");
            }
            plain(StatusCode::OK, json!({ "ok": true, "taken": "ignored" }))
        }
    }
}

/// The import a Download payload describes: the album (singular `album`, with `mbId`), the
/// paths of `trackFiles`, `isUpgrade`, and the download it came from (`downloadId`) and its
/// client (`downloadClient`, left out for a folder import). None without an album id.
pub fn import_event(payload: &Value) -> Option<LidarrImportEvent> {
    let album = payload.get("album")?;
    let foreign_album_id = album
        .get("mbId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())?
        .to_string();
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_string);
    let paths = payload
        .get("trackFiles")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|file| file.get("path").and_then(Value::as_str))
                .filter(|path| !path.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let present = |value: Option<&Value>| text(value).filter(|value| !value.trim().is_empty());
    Some(LidarrImportEvent {
        foreign_album_id,
        artist: text(payload.get("artist").and_then(|artist| artist.get("name"))),
        album: text(album.get("title")),
        paths,
        is_upgrade: payload.get("isUpgrade").and_then(Value::as_bool).unwrap_or(false),
        download_id: present(payload.get("downloadId")),
        download_client: present(payload.get("downloadClient")),
    })
}

/// Whether the request carries HTTP Basic credentials whose password is the secret. Both are
/// hashed first, so the comparison takes the same time whatever was sent.
pub fn authorized(headers: &HeaderMap, secret: &str) -> bool {
    let Some(sent) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, rest) = value.trim().split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("basic")
                .then(|| rest.trim().to_string())
        })
        .and_then(|encoded| base64::engine::general_purpose::STANDARD.decode(encoded).ok())
    else {
        return false;
    };
    let password = match sent.iter().position(|&b| b == b':') {
        Some(colon) => &sent[colon + 1..],
        None => return false,
    };
    let sent_hash = Sha256::digest(password);
    let secret_hash = Sha256::digest(secret.as_bytes());
    sent_hash
        .iter()
        .zip(secret_hash.iter())
        .fold(0u8, |differs, (a, b)| differs | (a ^ b))
        == 0
}

fn plain(status: StatusCode, body: Value) -> Response {
    let mut response = if body.is_null() {
        Response::new(Body::empty())
    } else {
        let mut response = Response::new(Body::from(body.to_string()));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        response
    };
    *response.status_mut() = status;
    response
}

#[cfg(test)]
#[path = "lidarr_webhook_tests.rs"]
mod tests;
