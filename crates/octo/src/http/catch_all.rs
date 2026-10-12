//! The catch-all `/{**endpoint}`: every method, every path no route claims, in this order.
//!
//! - `/` has no `endpoint`: a GET or HEAD is a 302 to `app/`, Navidrome's web app (#80), and
//!   any other method goes on with an empty endpoint;
//! - Octo-owned paths (lower-cased path starting with `admin` (which also matches
//!   `administrator`...) or `api/admin`, starting with `assets/`, or equal to `favicon.ico`)
//!   get the ProblemDetails 404 and are never relayed;
//! - native radio, the external-id safety net, the native catalog answers and the native
//!   search injections are `controllers::subsonic::native`;
//! - Navidrome's event stream (`api/events`) is streamed through as it arrives;
//! - everything else is relayed faithfully to Navidrome.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use octo_subsonic::subsonic_request_parser::unescape_data_string;

use super::error::problem;
use crate::app::AppState;
use crate::controllers::subsonic::action_helpers::is_successful_subsonic_response;
use crate::controllers::subsonic::native::{self, NativeRequest};
use crate::services::subsonic::{IncomingRequest, RawRelayResult};
use octo_subsonic::SubsonicResponseBuilder;

/// The largest request body read for a relay: 30 MB.
const MAX_REQUEST_BODY: usize = 30_000_000;

pub async fn catch_all(State(state): State<AppState>, req: Request) -> Response {
    let endpoint = endpoint_of(req.uri().path());
    // The bare address binds no endpoint at all, and Amperfy checks the server address before
    // signing in (#80). Navidrome answers it with its web app, so Octo does too; relative, so a
    // reverse proxy's path prefix is kept. Other methods go on with an empty endpoint.
    if endpoint.is_empty() && (req.method() == Method::GET || req.method() == Method::HEAD) {
        return redirect_to_app();
    }
    if is_octo_owned(&endpoint) {
        return problem(StatusCode::NOT_FOUND);
    }

    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let incoming = Arc::new(IncomingRequest::from_parts(&parts, body));
    let parameters = incoming.parameters();
    let format = parameters.get("f").map_or("xml", String::as_str).to_string();

    // Native radio, the external-id safety net, and the native answers.
    let relay = state.subsonic_proxy.with_request(Arc::clone(&incoming));
    if endpoint.eq_ignore_ascii_case("api/events") {
        return relay.relay_events(&endpoint, &parameters).await;
    }
    let native_request = NativeRequest {
        endpoint: &endpoint,
        method: &parts.method,
        headers: &parts.headers,
        parameters: &parameters,
        format: &format,
        proxy: &relay,
    };
    match native::answer(&state, &native_request).await {
        Ok(Some(response)) => return response,
        Ok(None) => {}
        Err(error) => return error.into_response(),
    }

    // Faithful relay: forward the caller's method + body + status so native Navidrome
    // endpoints (e.g. the POST /auth/login some clients use) work, not just GET-shaped
    // Subsonic calls.
    match relay.relay_raw(&endpoint, &parameters).await {
        Ok(raw) => {
            // Learn Octo's own Navidrome identity from a client's native sign-in as it passes
            // through, so background work (music-folder detection, an authenticated rescan)
            // has an admin token without any extra config.
            if raw.status == 200 && endpoint.eq_ignore_ascii_case("auth/login") {
                state.navidrome_identity.capture_login(&raw.body);
            }
            // An unheart reaches Navidrome through here: once Navidrome took it, the listener's
            // lists read their hearts again. A refusal is HTTP 200 too, so the envelope decides,
            // and an API key sign-in names its listener the way every request's identity does.
            if raw.status == 200
                && is_unstar(&endpoint)
                && is_successful_subsonic_response(&raw.body, &format)
                && let Ok(Some(username)) = state.request_identity.username(&parameters, &relay).await
                && !username.is_empty()
            {
                state.generated_playlists.forget_marks(&username);
            }
            write_raw_relay(raw, &format, &state.subsonic_response_builder)
        }
        Err(error) => state
            .subsonic_response_builder
            .create_error(
                &format,
                0,
                &format!("Error connecting to Subsonic server: {error}"),
            )
            .into_response(),
    }
}

fn is_unstar(endpoint: &str) -> bool {
    endpoint.eq_ignore_ascii_case("rest/unstar") || endpoint.eq_ignore_ascii_case("rest/unstar.view")
}

/// A 302 with an empty body, to Navidrome's web app beside this address.
fn redirect_to_app() -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::FOUND;
    res.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("app/"));
    res
}

/// The route value `{**endpoint}` holds: the path without its leading `/`, decoded (every
/// `%XX` but `%2F`, which stays encoded), in the client's casing.
pub fn endpoint_of(path: &str) -> String {
    let path = path.strip_prefix('/').unwrap_or(path);
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(at) = find_encoded_slash(rest) {
        out.push_str(&unescape_data_string(&rest[..at]));
        out.push_str(&rest[at..at + 3]);
        rest = &rest[at + 3..];
    }
    out.push_str(&unescape_data_string(rest));
    out
}

fn find_encoded_slash(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    lower.find("%2f")
}

/// Paths Octo answers itself and never relays.
pub fn is_octo_owned(endpoint: &str) -> bool {
    let lower = octo_core::common::dotnet::to_lower_invariant(endpoint);
    lower.starts_with("admin")
        || lower.starts_with("api/admin")
        || lower.starts_with("assets/")
        || lower == "favicon.ico"
        || lower == "family"
        || lower.starts_with("family/")
        || lower == "api/family"
        || lower.starts_with("api/family/")
}

/// The answer to a faithful relay: the upstream status, the allowlisted headers (the last value
/// of a name wins), the upstream content type or `application/{f}`, and the body, sent chunked.
fn write_raw_relay(raw: RawRelayResult, format: &str, builder: &SubsonicResponseBuilder) -> Response {
    let status = StatusCode::from_u16(raw.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = raw
        .content_type
        .clone()
        .unwrap_or_else(|| format!("application/{format}"));
    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in &raw.response_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    let Ok(content_type) = HeaderValue::from_str(&content_type) else {
        // A content type that is not a valid header value: the error envelope goes out with
        // the upstream status and headers already set.
        let mut response = builder
            .create_error(
                format,
                0,
                "Error connecting to Subsonic server: Invalid non-ASCII or control character in header.",
            )
            .into_response();
        *response.status_mut() = status;
        response.headers_mut().extend(headers);
        return response;
    };
    headers.insert(header::CONTENT_TYPE, content_type);

    let body = if status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED {
        Body::empty()
    } else {
        // A stream, so hyper frames it chunked, without a Content-Length.
        let chunk: Result<Bytes, std::io::Error> = Ok(raw.body);
        Body::from_stream(futures::stream::iter([chunk]))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
#[path = "catch_all_tests.rs"]
mod tests;
