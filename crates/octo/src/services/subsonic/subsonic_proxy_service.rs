//! Handles proxying requests to the underlying Subsonic server.
//!
//! A relay reads the client's own request, to forward the method, body and headers and to send
//! repeated query keys upstream as the client sent them. That request is an explicit
//! [`IncomingRequest`]: the [`SubsonicProxyService`] in `AppState` has none (background work),
//! and [`SubsonicProxyService::with_request`] makes the request-scoped copy a handler uses. Both
//! are cheap handles onto one shared client.

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use indexmap::IndexMap;
use octo_core::settings::SettingsStore;
use octo_subsonic::subsonic_request_parser::{
    self as parser, Parameters, RequestParts, StringValuesCollection, escape_data_string, join_values,
};
use serde_json::json;

use crate::http::error::{AppError, json_status, problem};
use crate::middleware::family_gate::GateReading;
use crate::services::http_client_factory::{
    DEFAULT_TIMEOUT, connect_failure_message, default_client, streaming_client, timeout_message,
};
use crate::services::i_download_service::DirectStreamInfo;

/// The message for a Navidrome URL that is missing or not absolute.
pub const NOT_CONFIGURED_MESSAGE: &str = "Octo has no valid Navidrome URL. Set SUBSONIC_URL (Subsonic__Url) to your \
     Navidrome server, e.g. http://192.168.1.10:4533. It must be an absolute URL the Octo \
     container can reach, not localhost.";

/// The message for a request URI that is not absolute.
const INVALID_REQUEST_URI: &str = "An invalid request URI was provided. Either the request URI must be an \
     absolute URI or BaseAddress must be set.";

/// A relay that failed. `Display` is its message, which callers put in front of users ("Error
/// connecting to Subsonic server: ...").
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayError {
    /// Octo's upstream Navidrome URL is missing or not an absolute URL. The global handler
    /// surfaces its message to the client as an actionable Subsonic error instead of an opaque
    /// "Invalid request".
    #[error("{0}")]
    NotConfigured(String),
    /// No connection, or a non-success status from the relay.
    #[error("{0}")]
    Http(String),
    /// The client's timeout ran out.
    #[error("{0}")]
    Canceled(String),
    /// A URL whose scheme is not http or https (`localhost:4533` is the scheme `localhost`).
    #[error("{0}")]
    NotSupported(String),
    /// A request URI that is not absolute (a URL such as `/music` passes the absolute-URL check,
    /// as an implicit file path, and fails here).
    #[error("{0}")]
    InvalidOperation(String),
}

impl RelayError {
    /// The failure's type name, as log lines name it.
    pub fn type_name(&self) -> &'static str {
        match self {
            RelayError::NotConfigured(_) => "OctoNotConfiguredException",
            RelayError::Http(_) => "HttpRequestException",
            RelayError::Canceled(_) => "TaskCanceledException",
            RelayError::NotSupported(_) => "NotSupportedException",
            RelayError::InvalidOperation(_) => "InvalidOperationException",
        }
    }

    fn from_reqwest(error: reqwest::Error) -> RelayError {
        if error.is_timeout() {
            RelayError::Canceled(timeout_message(DEFAULT_TIMEOUT))
        } else if error.is_connect() {
            RelayError::Http(connect_failure_message(&error))
        } else {
            RelayError::Http(error.to_string())
        }
    }
}

/// How the global error handler answers each of these when one escapes a handler.
impl From<RelayError> for AppError {
    fn from(error: RelayError) -> AppError {
        match error {
            RelayError::NotConfigured(m) => AppError::NotConfigured(m),
            RelayError::Http(m) => AppError::Http(m),
            // An invalid operation answers 400 "Operation not valid".
            RelayError::InvalidOperation(m) => AppError::InvalidOperation(m),
            RelayError::Canceled(m) | RelayError::NotSupported(m) => AppError::Internal(anyhow::anyhow!(m)),
        }
    }
}

/// The client's request as the request-scoped proxy sees it, with the body already read.
pub struct IncomingRequest {
    pub method: Method,
    pub headers: HeaderMap,
    /// The raw query string, without its `?`.
    pub query: Option<String>,
    /// The whole body as the client sent it.
    pub body: Bytes,
    query_values: OnceLock<StringValuesCollection>,
    /// The family gate's reading of this request, while Family is on: its parameters are the
    /// ones every handler reads.
    gate: Option<GateReading>,
}

impl IncomingRequest {
    pub fn new(method: Method, headers: HeaderMap, query: Option<String>, body: Bytes) -> Self {
        IncomingRequest {
            method,
            headers,
            query,
            body,
            query_values: OnceLock::new(),
            gate: None,
        }
    }

    /// The request a handler was given, with the family gate's reading of it when there is one.
    pub fn from_parts(parts: &axum::http::request::Parts, body: Bytes) -> Self {
        let mut incoming = Self::new(
            parts.method.clone(),
            parts.headers.clone(),
            parts.uri.query().map(str::to_string),
            body,
        );
        incoming.gate = GateReading::of(&parts.extensions).cloned();
        incoming
    }

    /// The request's content type.
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
    }

    /// The request's content length.
    pub fn content_length(&self) -> Option<u64> {
        self.headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
    }

    /// The request as the parser reads it.
    pub fn request_parts(&self) -> RequestParts<'_> {
        RequestParts {
            query: self.query.as_deref(),
            content_type: self.content_type(),
            content_length: self.content_length(),
            body: &self.body,
        }
    }

    /// Every parameter of the request: query, form and body, or while Family is on the
    /// parameters the family gate read (its rewrites and quality included).
    pub fn parameters(&self) -> Parameters {
        match &self.gate {
            Some(gate) => gate.parameters().clone(),
            None => parser::extract_all_parameters(&self.request_parts()),
        }
    }

    /// Every value the request sent for `name`, from the family gate's reading while Family is
    /// on.
    pub fn parameter_values(&self, name: &str) -> Vec<String> {
        match &self.gate {
            Some(gate) => gate.parameter_values(name),
            None => parser::extract_parameter_values(&self.request_parts(), name),
        }
    }

    /// The family gate's reading of this request, while Family is on.
    pub fn gate(&self) -> Option<&GateReading> {
        self.gate.as_ref()
    }

    /// The request's query string, parsed.
    pub fn query_collection(&self) -> &StringValuesCollection {
        self.query_values
            .get_or_init(|| parser::parse_query(self.query.as_deref().unwrap_or("")))
    }

    /// The captured body, when it holds something: the body of every POST, PUT and PATCH is
    /// captured, and the relay forwards it only when it is not empty.
    pub fn raw_body(&self) -> Option<&Bytes> {
        let captured =
            self.method == Method::POST || self.method == Method::PUT || self.method == Method::PATCH;
        (captured && !self.body.is_empty()).then_some(&self.body)
    }

    /// The client's form fields, or `None` when the request carries none: the form as read, or
    /// when that fails the captured body read as a query string, as the request parser falls
    /// back.
    fn form(&self) -> Option<StringValuesCollection> {
        let content_type = self.content_type();
        if !parser::has_form_content_type(content_type) {
            return None;
        }
        match parser::read_form(content_type.unwrap_or(""), &self.body) {
            Ok(form) => Some(form),
            Err(_) => self
                .raw_body()
                .map(|bytes| parser::parse_query_helpers(&String::from_utf8_lossy(bytes))),
        }
    }
}

/// `(byte[] Body, string? ContentType)`: an upstream answer read in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayResponse {
    pub body: Bytes,
    pub content_type: Option<String>,
}

/// Result of a faithful (method/body/status/header-preserving) relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRelayResult {
    pub status: u16,
    pub body: Bytes,
    pub content_type: Option<String>,
    /// The allowlisted response headers, one entry per value (a `Vary` list is one entry per
    /// member). When they are written, the last of each name wins.
    pub response_headers: Vec<(String, String)>,
}

/// Headers forwarded to Navidrome so its native /api/* endpoints (used by Navidrome-mode
/// clients like Feishin) authenticate and behave correctly.
const FORWARD_REQUEST_HEADERS: [&str; 8] = [
    "Authorization",
    "X-Nd-Client-Unique-Id",
    "X-Nd-Authorization",
    "Accept",
    "User-Agent",
    "If-None-Match",
    "If-Modified-Since",
    "Range",
];

/// Response headers passed back to the client (notably the rotated ND token).
///
/// This is an allowlist, so anything missing from it is silently dropped, and X-Total-Count
/// was. Navidrome's native list endpoints report their length only in that header, and
/// Navidrome-mode clients size their virtualised lists from it: with no header, Feishin's
/// Albums, Artists and Tracks pages have nothing to size against and render empty, while Home
/// and Search, which do not paginate, look perfectly fine (issue #34). The body was always
/// correct, which is why it read as a client bug.
/// Content-Disposition carries a download's file name; without it rest/download saved the song
/// with no name.
const FORWARD_RESPONSE_HEADERS: [&str; 10] = [
    "X-Nd-Authorization",
    "ETag",
    "Last-Modified",
    "Cache-Control",
    "Content-Range",
    "Accept-Ranges",
    "Vary",
    "X-Total-Count",
    "Access-Control-Expose-Headers",
    "Content-Disposition",
];

const STREAMING_REQUIRED_HEADERS: [&str; 5] = [
    "Accept-Ranges",
    "Content-Range",
    "Content-Length",
    "ETag",
    "Last-Modified",
];

struct ProxyShared {
    client: reqwest::Client,
    stream_client: reqwest::Client,
    // The admin UI writes settings.json and the config provider reloads it, so the URL is read at
    // the point of use. A captured copy would serve startup values until a restart while the
    // admin UI SHOWED the new value.
    settings: Arc<SettingsStore>,
}

/// Handles proxying requests to the underlying Subsonic server.
#[derive(Clone)]
pub struct SubsonicProxyService {
    shared: Arc<ProxyShared>,
    incoming: Option<Arc<IncomingRequest>>,
}

impl SubsonicProxyService {
    pub fn new(settings: Arc<SettingsStore>) -> Self {
        Self::with_clients(settings, default_client(), streaming_client())
    }

    pub fn with_clients(
        settings: Arc<SettingsStore>,
        client: reqwest::Client,
        stream_client: reqwest::Client,
    ) -> Self {
        SubsonicProxyService {
            shared: Arc::new(ProxyShared {
                client,
                stream_client,
                settings,
            }),
            incoming: None,
        }
    }

    /// The request-scoped proxy for one client request.
    pub fn with_request(&self, incoming: Arc<IncomingRequest>) -> Self {
        SubsonicProxyService {
            shared: Arc::clone(&self.shared),
            incoming: Some(incoming),
        }
    }

    pub fn incoming(&self) -> Option<&IncomingRequest> {
        self.incoming.as_deref()
    }

    fn url_setting(&self) -> Option<String> {
        self.shared.settings.current().subsonic.url.clone()
    }

    /// The configured URL, or [`RelayError::NotConfigured`] when it is blank or not absolute.
    fn configured_url(&self) -> Result<String, RelayError> {
        match self.url_setting() {
            Some(url) if !url.trim().is_empty() && is_absolute_uri(&url) => Ok(url),
            _ => Err(RelayError::NotConfigured(NOT_CONFIGURED_MESSAGE.to_string())),
        }
    }

    /// Relays a request to the Subsonic server and returns the response: `GET
    /// {Url}/{endpoint}?{query}` with no forwarded headers and no body. A non-success status is a
    /// [`RelayError::Http`]. Takes the parameter dictionary or, for batch endpoints whose repeated
    /// keys the dictionary cannot hold, a list of pairs.
    pub async fn relay<I, K, V>(&self, endpoint: &str, parameters: I) -> Result<RelayResponse, RelayError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let base = self.configured_url()?;
        let query = self.build_query(parameters, false);
        let url = format!("{}/{endpoint}?{query}", base.trim_end_matches('/'));
        let target = request_url(&url)?;

        let response = self
            .shared
            .client
            .get(target)
            .send()
            .await
            .map_err(RelayError::from_reqwest)?;
        let status = response.status();
        if !status.is_success() {
            return Err(RelayError::Http(not_success_message(status)));
        }
        let content_type = content_type_of(response.headers());
        let body = response.bytes().await.map_err(RelayError::from_reqwest)?;
        Ok(RelayResponse { body, content_type })
    }

    /// Safely relays a request to the Subsonic server, returning `None` on any failure.
    pub async fn relay_safe<I, K, V>(&self, endpoint: &str, parameters: I) -> Option<RelayResponse>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.relay(endpoint, parameters).await.ok()
    }

    /// Faithful relay: forwards the caller's HTTP method + body + content-type and returns the
    /// upstream status verbatim (a failure status is not an error). This is what makes non-GET
    /// / body-carrying endpoints work through Octo: e.g. Navidrome's native POST /auth/login
    /// that some clients use to sign in. The body is read in full.
    pub async fn relay_raw<I, K, V>(
        &self,
        endpoint: &str,
        parameters: I,
    ) -> Result<RawRelayResult, RelayError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let base = self.configured_url()?;
        let incoming = self.incoming.as_deref();
        let method = incoming.map_or(Method::GET, |r| r.method.clone());
        let raw_body = incoming.and_then(IncomingRequest::raw_body);

        let parameters = owned_pairs(parameters);
        let query = self.build_query(parameters.iter().map(|(k, v)| (k, v)), raw_body.is_some());
        let url = format!("{}/{endpoint}?{query}", base.trim_end_matches('/'));
        let target = request_url(&url)?;
        let mut request = self.shared.client.request(method, target);

        // Forward the raw request body captured from the client.
        if let Some(body) = raw_body {
            request = request.body(body.clone());
            if let Some(content_type) = incoming
                .and_then(IncomingRequest::content_type)
                .filter(|c| !c.is_empty())
            {
                request = request.header(header::CONTENT_TYPE, content_type);
            }
        }

        // Forward auth + conditional headers so native Navidrome endpoints work.
        if let Some(incoming) = incoming {
            for name in FORWARD_REQUEST_HEADERS {
                if let Some(value) = joined_request_header(&incoming.headers, name) {
                    request = request.header(name, value);
                }
            }
        }

        let response = request.send().await.map_err(RelayError::from_reqwest)?;
        let status = response.status().as_u16();
        let content_type = content_type_of(response.headers());
        let mut response_headers = Vec::new();
        for name in FORWARD_RESPONSE_HEADERS {
            for value in response_header_values(response.headers(), name) {
                response_headers.push((name.to_string(), value));
            }
        }
        // Read knowing whether the length is Navidrome's estimate (rest/download with a format
        // and estimateContentLength). The client's timeout still covers the whole body.
        let body = if length_may_be_estimate(response.status(), response.headers(), &parameters) {
            let mut upstream = Box::pin(tolerate_early_end(response, endpoint));
            let mut buffer = bytes::BytesMut::new();
            while let Some(chunk) = upstream.next().await {
                buffer.extend_from_slice(&chunk.map_err(RelayError::from_reqwest)?);
            }
            buffer.freeze()
        } else {
            response.bytes().await.map_err(RelayError::from_reqwest)?
        };
        Ok(RawRelayResult {
            status,
            body,
            content_type,
            response_headers,
        })
    }

    /// Builds the upstream query string from the lookup dictionary and the client's own
    /// request.
    fn build_query<I, K, V>(&self, parameters: I, body_forwarded: bool) -> String
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let incoming = self.incoming.as_deref();
        let form = incoming.and_then(IncomingRequest::form);
        // Navidrome reads a url-encoded body, not a multipart one, so only the first can carry
        // the fields for the query.
        let url_encoded = incoming
            .and_then(IncomingRequest::content_type)
            .is_some_and(|ct| {
                octo_core::common::dotnet::starts_with_ignore_case(ct, "application/x-www-form-urlencoded")
            });
        let pairs = restore_repeated_parameters(
            parameters,
            incoming.map(IncomingRequest::query_collection),
            form.as_ref(),
            body_forwarded && url_encoded && form.is_some(),
        );
        pairs
            .iter()
            .map(|(k, v)| format!("{}={}", escape_data_string(k), escape_data_string(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Relays a stream request to the Subsonic server, answering the client directly: the
    /// upstream status (206 for a range), the headers a player needs to seek, and the body
    /// streamed as it arrives. A non-success status is a bare status answer (a ProblemDetails
    /// from 400 up, an empty answer below); any failure is a 500
    /// `{"error": "Error streaming from Subsonic: ..."}`.
    pub async fn relay_stream<I, K, V>(&self, parameters: I) -> Response
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        // Get HTTP context for request/response forwarding
        let Some(incoming) = self.incoming.as_deref() else {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "HTTP context not available" }),
            );
        };
        match self.open_stream(incoming, parameters).await {
            Ok(response) => response,
            Err(error) => json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": format!("Error streaming from Subsonic: {error}") }),
            ),
        }
    }

    async fn open_stream<I, K, V>(
        &self,
        incoming: &IncomingRequest,
        parameters: I,
    ) -> Result<Response, RelayError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let parameters = owned_pairs(parameters);
        let query = self.build_query(parameters.iter().map(|(k, v)| (k, v)), false);
        // Not checked: none at all is a relative URI. The settings read Navidrome's address
        // without its trailing slashes (`normalize_server_url`), so a URL ending in '/' still
        // gives one slash before rest/stream.
        let url = format!("{}/rest/stream?{query}", self.url_setting().unwrap_or_default());
        let target = request_url(&url)?;
        let mut request = self.shared.stream_client.get(target);

        // Forward Range headers for progressive streaming support (iOS clients)
        for name in ["Range", "If-Range"] {
            if let Some(value) = joined_request_header(&incoming.headers, name) {
                request = request.header(name, value);
            }
        }

        // The timeout bounds the wait for the headers; the body then streams freely.
        let response = match tokio::time::timeout(DEFAULT_TIMEOUT, request.send()).await {
            Ok(result) => result.map_err(RelayError::from_reqwest)?,
            Err(_) => return Err(RelayError::Canceled(timeout_message(DEFAULT_TIMEOUT))),
        };
        let status = response.status();
        if !status.is_success() {
            let mut result = status_code_result(status);
            // A 416 names the real size ("bytes */N"). A player that asked past the end of a
            // transcode, usually because it believed an estimate, learns where the end is;
            // ExoPlayer reads that as the end of the song.
            if status == StatusCode::RANGE_NOT_SATISFIABLE {
                for value in response_header_values(response.headers(), "Content-Range") {
                    if let Ok(value) = HeaderValue::from_str(&value) {
                        result.headers_mut().append(header::CONTENT_RANGE, value);
                    }
                }
            }
            return Ok(result);
        }

        // A live transcode's Content-Length is Navidrome's guess, and the real output usually
        // comes out 2 to 3% smaller. Promising the guess to the client meant the copy failed
        // after the last real byte and the connection was cut, so that length is left out and
        // the body goes chunked.
        let estimate = length_may_be_estimate(status, response.headers(), &parameters);

        // Forward HTTP status code (e.g., 206 Partial Content for range requests) and the
        // streaming-required headers from the upstream response.
        let mut builder = Response::builder().status(status);
        for name in STREAMING_REQUIRED_HEADERS {
            if estimate && name == "Content-Length" {
                continue;
            }
            for value in response_header_values(response.headers(), name) {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    builder = builder.header(name, value);
                }
            }
        }
        let content_type = content_type_of(response.headers()).unwrap_or_else(|| "audio/mpeg".to_string());
        builder = builder.header(header::CONTENT_TYPE, content_type);

        // FileStreamResult over a stream that cannot seek: no range processing, but its
        // preconditions still ran, against no ETag and no Last-Modified.
        match file_result_precondition(&incoming.headers) {
            Precondition::NotModified => {
                return Ok(builder
                    .status(StatusCode::NOT_MODIFIED)
                    .body(Body::empty())
                    .unwrap_or_default());
            }
            Precondition::Failed => {
                return Ok(builder
                    .status(StatusCode::PRECONDITION_FAILED)
                    .body(Body::empty())
                    .unwrap_or_default());
            }
            Precondition::Serve => {}
        }
        let body = if estimate {
            Body::from_stream(tolerate_early_end(response, "rest/stream"))
        } else {
            Body::from_stream(response.bytes_stream())
        };
        Ok(builder
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
    }

    /// Relays Navidrome's server-sent events (`api/events`), passing each event on as it
    /// arrives: the stream never ends, so a relay that reads the body first never answers.
    pub async fn relay_events<I, K, V>(&self, endpoint: &str, parameters: I) -> Response
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        match self.open_events(endpoint, parameters).await {
            Ok(response) => response,
            Err(_) => StatusCode::BAD_GATEWAY.into_response(),
        }
    }

    async fn open_events<I, K, V>(&self, endpoint: &str, parameters: I) -> Result<Response, RelayError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let base = self.configured_url()?;
        let parameters = owned_pairs(parameters);
        let query = self.build_query(parameters.iter().map(|(k, v)| (k, v)), false);
        let url = format!("{}/{endpoint}?{query}", base.trim_end_matches('/'));
        let mut request = self.shared.stream_client.get(request_url(&url)?);
        if let Some(incoming) = self.incoming.as_deref() {
            for name in FORWARD_REQUEST_HEADERS {
                if let Some(value) = joined_request_header(&incoming.headers, name) {
                    request = request.header(name, value);
                }
            }
        }
        let response = match tokio::time::timeout(DEFAULT_TIMEOUT, request.send()).await {
            Ok(result) => result.map_err(RelayError::from_reqwest)?,
            Err(_) => return Err(RelayError::Canceled(timeout_message(DEFAULT_TIMEOUT))),
        };

        let mut builder = Response::builder().status(response.status());
        for name in FORWARD_RESPONSE_HEADERS {
            for value in response_header_values(response.headers(), name) {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    builder = builder.header(name, value);
                }
            }
        }
        let content_type =
            content_type_of(response.headers()).unwrap_or_else(|| "text/event-stream".to_string());
        builder = builder
            .header(header::CONTENT_TYPE, content_type)
            .header("X-Accel-Buffering", "no");
        Ok(builder
            .body(Body::from_stream(response.bytes_stream()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
    }

    /// Opens a full upstream track body without binding it to the current HTTP response.
    /// Continuous Radio feeds this stream into its in-process transcoder, so it must own the
    /// upstream response until the track ends. `None` when there is no usable URL or Navidrome
    /// does not answer with success.
    pub async fn open_audio_stream(
        &self,
        parameters: &IndexMap<String, String>,
    ) -> Result<Option<DirectStreamInfo>, RelayError> {
        let Some(base) = self
            .url_setting()
            .filter(|u| !u.trim().is_empty() && is_absolute_uri(u))
        else {
            return Ok(None);
        };
        let query = parameters
            .iter()
            .map(|(k, v)| format!("{}={}", escape_data_string(k), escape_data_string(v)))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("{}/rest/stream?{query}", base.trim_end_matches('/'));
        let target = request_url(&url)?;
        let response =
            match tokio::time::timeout(DEFAULT_TIMEOUT, self.shared.stream_client.get(target).send()).await {
                Ok(result) => result.map_err(RelayError::from_reqwest)?,
                Err(_) => return Err(RelayError::Canceled(timeout_message(DEFAULT_TIMEOUT))),
            };
        if !response.status().is_success() {
            return Ok(None);
        }
        let content_type =
            content_type_of(response.headers()).unwrap_or_else(|| "application/octet-stream".to_string());
        let content_length = response.content_length();
        let stream = response.bytes_stream().map_err(std::io::Error::other);
        Ok(Some(DirectStreamInfo {
            audio_stream: Box::pin(stream),
            content_type,
            content_length,
            quality: Some("navidrome-raw".to_string()),
            status_code: 200,
            content_range: None,
        }))
    }
}

/// The relay's parameters as owned pairs, read once for the query and once for
/// [`length_may_be_estimate`].
fn owned_pairs<I, K, V>(parameters: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    parameters
        .into_iter()
        .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string()))
        .collect()
}

/// True when the upstream Content-Length may be Navidrome's estimate of a transcode rather
/// than the size of the body it will send. Navidrome serves a file, or a transcode it has
/// finished and cached, with Accept-Ranges: bytes and an exact length, and a 206 carries an
/// exact Content-Range. A transcode still being made says Accept-Ranges: none, and then has a
/// length only because the client asked for an estimate (Verified 2026-10-05: announced
/// 4,067,983, sent 3,974,093). With neither mark, the request decides: an estimate or a
/// transcode was asked for.
pub(crate) fn length_may_be_estimate(
    status: StatusCode,
    headers: &HeaderMap,
    parameters: &[(String, String)],
) -> bool {
    use octo_core::common::dotnet::eq_ignore_case;

    if status == StatusCode::PARTIAL_CONTENT {
        return false;
    }
    let accept_ranges = response_header_values(headers, "Accept-Ranges");
    if accept_ranges.iter().any(|v| eq_ignore_case(v, "bytes")) {
        return false;
    }
    if accept_ranges.iter().any(|v| eq_ignore_case(v, "none")) {
        return true;
    }

    let value = |key: &str| {
        parameters
            .iter()
            .find(|(k, _)| eq_ignore_case(k, key))
            .map(|(_, v)| v.as_str())
    };
    if value("estimateContentLength").is_some_and(|v| eq_ignore_case(v, "true")) {
        return true;
    }
    if value("format").is_some_and(|f| !f.is_empty() && !eq_ignore_case(f, "raw")) {
        return true;
    }
    value("maxBitRate")
        .and_then(crate::controllers::subsonic::action_helpers::int_try_parse)
        .is_some_and(|max_bit_rate| max_bit_rate > 0)
}

/// A transcode whose Content-Length was an estimate. The HTTP client holds the body to the
/// length announced and fails when it ends sooner, which here only means the real output was
/// smaller than the guess, so that early end reads as the end of the song. Any other failure
/// still fails the body. Once ended, the stream stays ended.
fn tolerate_early_end(
    response: reqwest::Response,
    endpoint: &str,
) -> impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static {
    let announced = response.content_length();
    let endpoint = endpoint.to_string();
    let upstream = Box::pin(response.bytes_stream());
    futures::stream::unfold((upstream, 0u64, false), move |(mut upstream, sent, ended)| {
        let endpoint = endpoint.clone();
        async move {
            if ended {
                return None;
            }
            match upstream.next().await? {
                Ok(chunk) => {
                    let sent = sent + chunk.len() as u64;
                    Some((Ok(chunk), (upstream, sent, false)))
                }
                Err(error) if ended_early(&error) => {
                    tracing::debug!(
                        "{endpoint}: the transcode ended at {sent} bytes, short of the estimated {}",
                        announced.map_or_else(String::new, |n| n.to_string())
                    );
                    None
                }
                // A real failure: passed on, and nothing is read after it.
                Err(error) => Some((Err(error), (upstream, sent, true))),
            }
        }
    })
    .fuse()
}

/// Whether a body failed only because the connection closed before the announced Content-Length
/// was reached: hyper reports that as an I/O error of kind `UnexpectedEof` somewhere down the
/// error's sources.
fn ended_early(error: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
        {
            return true;
        }
        source = cause.source();
    }
    false
}

/// The parameter dictionary holds a repeated key as one comma-joined string (`id=A&id=B` reads
/// "A,B"), and Navidrome takes that as ONE id. A value that is still exactly what the client
/// sent goes out as the client's separate values again, in order; a value a handler changed or
/// added goes out as it is. With `form_in_body`, an unchanged form field is left out of the
/// query, because the forwarded body already carries it and Navidrome reads both.
pub fn restore_repeated_parameters<I, K, V>(
    parameters: I,
    query: Option<&StringValuesCollection>,
    form: Option<&StringValuesCollection>,
    form_in_body: bool,
) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    // Ordinal lookups by the spelling each key was first sent with.
    let lookup = |source: Option<&StringValuesCollection>, key: &str| -> Option<Vec<String>> {
        source?.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_vec())
    };
    let unchanged = |value: &str, sent: &[String]| !sent.is_empty() && value == join_values(sent);

    let mut result = Vec::new();
    for (key, value) in parameters {
        let (key, value) = (key.as_ref(), value.as_ref());
        if let Some(sent) = lookup(form, key)
            && unchanged(value, &sent)
        {
            if !form_in_body {
                result.extend(sent.into_iter().map(|v| (key.to_string(), v)));
            }
            continue;
        }
        if let Some(sent) = lookup(query, key)
            && unchanged(value, &sent)
        {
            result.extend(sent.into_iter().map(|v| (key.to_string(), v)));
            continue;
        }
        result.push((key.to_string(), value.to_string()));
    }
    result
}

/// Whether `url` is an absolute URI: an http(s) URL that parses, any other `scheme:`
/// (`localhost:4533` is the scheme `localhost`), or a rooted path, which is an implicit
/// `file://` URI.
pub fn is_absolute_uri(url: &str) -> bool {
    let url = url.trim();
    if url.starts_with('/') {
        return true;
    }
    match scheme_of(url) {
        Some(scheme) if scheme == "http" || scheme == "https" => {
            reqwest::Url::parse(url).is_ok_and(|u| u.host_str().is_some_and(|h| !h.is_empty()))
        }
        Some(_) => true,
        None => false,
    }
}

/// The scheme of `scheme:rest`, lower-cased, when the text starts with one. A single letter
/// is a drive (`c:`), read as a file path.
fn scheme_of(url: &str) -> Option<String> {
    let colon = url.find(':')?;
    let scheme = &url[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic()
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    if scheme.len() == 1 {
        return Some("file".to_string());
    }
    Some(scheme.to_ascii_lowercase())
}

/// A request URI string as the HTTP client takes it: an absolute http(s) URL to send, or the
/// error for one it cannot send.
fn request_url(url: &str) -> Result<reqwest::Url, RelayError> {
    let trimmed = url.trim();
    if trimmed.starts_with('/') {
        return Err(RelayError::InvalidOperation(INVALID_REQUEST_URI.to_string()));
    }
    match scheme_of(trimmed) {
        None => Err(RelayError::InvalidOperation(INVALID_REQUEST_URI.to_string())),
        Some(scheme) if scheme != "http" && scheme != "https" => Err(RelayError::NotSupported(format!(
            "The '{scheme}' scheme is not supported."
        ))),
        Some(_) => reqwest::Url::parse(trimmed).map_err(|_| {
            RelayError::InvalidOperation("Invalid URI: The hostname could not be parsed.".to_string())
        }),
    }
}

/// `EnsureSuccessStatusCode`'s failure, in plain words.
fn not_success_message(status: StatusCode) -> String {
    crate::services::http_client_factory::status_failure_message(status)
}

/// A request header's values as one line: each line is a value, and they are joined with ", "
/// (a space for `User-Agent`, a product list).
fn joined_request_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<String> = headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect();
    if values.is_empty() {
        return None;
    }
    let separator = if name.eq_ignore_ascii_case("User-Agent") {
        " "
    } else {
        ", "
    };
    Some(values.join(separator))
}

/// `response.Content.Headers.ContentType?.ToString()`: the media type as
/// `MediaTypeHeaderValue` re-wrote it (`application/json;charset=UTF-8` became
/// `application/json; charset=UTF-8`), or `None` when it is missing or does not parse.
pub fn content_type_of(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::CONTENT_TYPE)?;
    normalize_media_type(&String::from_utf8_lossy(raw.as_bytes()))
}

fn normalize_media_type(raw: &str) -> Option<String> {
    let mut parts = raw.split(';');
    let media_type = parts.next()?.trim();
    let (kind, subtype) = media_type.split_once('/')?;
    let is_token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?={}".contains(&b))
    };
    if !is_token(kind.trim()) || !is_token(subtype.trim()) {
        return None;
    }
    let mut out = format!("{}/{}", kind.trim(), subtype.trim());
    for parameter in parts.map(str::trim).filter(|p| !p.is_empty()) {
        out.push_str("; ");
        match parameter.split_once('=') {
            Some((name, value)) => {
                out.push_str(name.trim());
                out.push('=');
                out.push_str(value.trim());
            }
            None => out.push_str(parameter),
        }
    }
    Some(out)
}

/// A response header's values: list headers (`Vary`, `Accept-Ranges`) split into their members,
/// `Cache-Control` merged into one re-written value, `Last-Modified` re-written as an RFC 1123
/// date, anything else one value per line as received.
fn response_header_values(headers: &HeaderMap, name: &str) -> Vec<String> {
    let lines: Vec<String> = headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect();
    if lines.is_empty() {
        return lines;
    }
    match name.to_ascii_lowercase().as_str() {
        "vary" | "accept-ranges" => lines
            .iter()
            .flat_map(|line| line.split(','))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect(),
        "cache-control" => match normalize_cache_control(&lines) {
            Some(value) => vec![value],
            None => lines,
        },
        "last-modified" => lines
            .into_iter()
            .map(|line| reformat_http_date(&line).unwrap_or(line))
            .collect(),
        "content-length" => lines.into_iter().map(|l| l.trim().to_string()).collect(),
        _ => lines,
    }
}

/// A date header as `HttpHeaders` re-wrote it: parsed leniently (a one-digit day is fine) and
/// written back in the RFC 1123 form, `ddd, dd MMM yyyy HH:mm:ss GMT`.
fn reformat_http_date(text: &str) -> Option<String> {
    let text = text.trim();
    if let Ok(time) = httpdate::parse_http_date(text) {
        return Some(httpdate::fmt_http_date(time));
    }
    let parsed = chrono::DateTime::parse_from_rfc2822(&text.replace("GMT", "+0000")).ok()?;
    Some(
        parsed
            .with_timezone(&chrono::Utc)
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string(),
    )
}

/// Every `Cache-Control` line normalized into one value: the known directives in a fixed order,
/// then the others as written. `None` when a known directive's value does not parse (the value
/// then goes through unparsed).
fn normalize_cache_control(lines: &[String]) -> Option<String> {
    #[derive(Default)]
    struct Directives {
        no_store: bool,
        no_transform: bool,
        only_if_cached: bool,
        public: bool,
        must_revalidate: bool,
        proxy_revalidate: bool,
        no_cache: Option<Vec<String>>,
        max_age: Option<u64>,
        s_maxage: Option<u64>,
        max_stale: Option<Option<u64>>,
        min_fresh: Option<u64>,
        private: Option<Vec<String>>,
        extensions: Vec<String>,
    }
    let seconds = |v: Option<&str>| v.and_then(|v| v.trim().parse::<u64>().ok());
    let fields = |v: Option<&str>| -> Vec<String> {
        v.map(|v| {
            v.trim()
                .trim_matches('"')
                .split(',')
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
    };
    let mut d = Directives::default();
    for item in lines.iter().flat_map(|line| split_outside_quotes(line)) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (name, value) = match item.split_once('=') {
            Some((n, v)) => (n.trim(), Some(v.trim())),
            None => (item, None),
        };
        match name.to_ascii_lowercase().as_str() {
            "no-store" => d.no_store = true,
            "no-transform" => d.no_transform = true,
            "only-if-cached" => d.only_if_cached = true,
            "public" => d.public = true,
            "must-revalidate" => d.must_revalidate = true,
            "proxy-revalidate" => d.proxy_revalidate = true,
            "no-cache" => d.no_cache.get_or_insert_with(Vec::new).extend(fields(value)),
            "private" => d.private.get_or_insert_with(Vec::new).extend(fields(value)),
            "max-age" => d.max_age = Some(seconds(value)?),
            "s-maxage" => d.s_maxage = Some(seconds(value)?),
            "min-fresh" => d.min_fresh = Some(seconds(value)?),
            "max-stale" => {
                d.max_stale = Some(match value {
                    Some(_) => Some(seconds(value)?),
                    None => None,
                })
            }
            _ => d.extensions.push(item.to_string()),
        }
    }
    let mut out: Vec<String> = Vec::new();
    for (on, text) in [
        (d.no_store, "no-store"),
        (d.no_transform, "no-transform"),
        (d.only_if_cached, "only-if-cached"),
        (d.public, "public"),
        (d.must_revalidate, "must-revalidate"),
        (d.proxy_revalidate, "proxy-revalidate"),
    ] {
        if on {
            out.push(text.to_string());
        }
    }
    let with_fields = |name: &str, fields: &[String]| {
        if fields.is_empty() {
            name.to_string()
        } else {
            format!("{name}=\"{}\"", fields.join(", "))
        }
    };
    if let Some(fields) = &d.no_cache {
        out.push(with_fields("no-cache", fields));
    }
    if let Some(v) = d.max_age {
        out.push(format!("max-age={v}"));
    }
    if let Some(v) = d.s_maxage {
        out.push(format!("s-maxage={v}"));
    }
    if let Some(limit) = d.max_stale {
        out.push(match limit {
            Some(v) => format!("max-stale={v}"),
            None => "max-stale".to_string(),
        });
    }
    if let Some(v) = d.min_fresh {
        out.push(format!("min-fresh={v}"));
    }
    if let Some(fields) = &d.private {
        out.push(with_fields("private", fields));
    }
    out.extend(d.extensions);
    Some(out.join(", "))
}

fn split_outside_quotes(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ',' if !quoted => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
}

/// A bodiless status answer: a ProblemDetails from 400 up, an empty answer below.
fn status_code_result(status: StatusCode) -> Response {
    if status.as_u16() >= 400 {
        problem(status)
    } else {
        status.into_response()
    }
}

enum Precondition {
    Serve,
    NotModified,
    Failed,
}

/// `FileResultExecutorBase`'s precondition check for a file with no ETag and no
/// Last-Modified: `If-Match` other than `*` fails (412), `If-None-Match: *` is not modified
/// (304); the date conditions need a date and do nothing.
fn file_result_precondition(headers: &HeaderMap) -> Precondition {
    let tags = |name: &str| -> Vec<String> {
        headers
            .get_all(name)
            .iter()
            .flat_map(|v| {
                String::from_utf8_lossy(v.as_bytes())
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .collect::<Vec<_>>()
            })
            .filter(|t| !t.is_empty())
            .collect()
    };
    let if_match = tags("If-Match");
    if !if_match.is_empty() && !if_match.iter().any(|t| t == "*") {
        return Precondition::Failed;
    }
    let if_none_match = tags("If-None-Match");
    if if_none_match.iter().any(|t| t == "*") {
        return Precondition::NotModified;
    }
    Precondition::Serve
}

#[cfg(test)]
#[path = "subsonic_proxy_service_tests.rs"]
mod tests;
