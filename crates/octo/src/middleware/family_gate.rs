//! The family gate: one layer every request passes while Family is on. It works out who is
//! asking, signs members in to Navidrome with their hidden passwords, and puts a
//! [`FamilyCaller`] on the request for the handlers and the family rules. With Family off it
//! hands every request on untouched before looking at anything.
//!
//! - **One reading**: the gate reads each request once, query and body, and puts what it read
//!   on the request as a [`GateReading`]: the parameters every handler reads, which sign-in
//!   keys were there, and who it is from. A `/rest` body it cannot read (a multipart form with
//!   no usable boundary, JSON that is not one object) is refused with Subsonic error 0. Anyone
//!   outside the family, the owner included, is checked only through that reading: Navidrome is
//!   asked about exactly the sign-in the gate read (a cached ping), and only then is their
//!   device noted.
//! - **Subsonic (`/rest`)**: a request whose `u` names a member is checked against the
//!   member's one account password (`p`, plain or `enc:` hex, or `t` with `s`, or a browser
//!   token Octo signed for a web player session) and rewritten, in the query and in a url-encoded or JSON body, to `u` with a
//!   fresh `s` and `t` made from the hidden password. An API key or a `jwt` that turns out to be
//!   a member's is rewritten the same way. A wrong sign-in is answered with Subsonic error 40
//!   and never reaches Navidrome; twenty wrong ones from one address for one member in 15
//!   minutes, or a hundred from the peer that connected, or a hundred for the member from
//!   addresses that never signed in as them, and the rest are refused unchecked. Addresses that
//!   did sign in as the member count their wrong tries apart (a hundred more together), so a
//!   flood from strangers never locks out an app that already has the password. The owner and
//!   anyone outside the family pass through exactly as they came. A member's device is recorded
//!   only once its sign-in checked out.
//! - **Paths**: a request is judged by its path as Navidrome may end up reading it
//!   ([`gate_path`]), so `/rest/%67etNowPlaying`, `/rest/x/../getNowPlaying` and the like meet
//!   the rules of the endpoint they reach.
//! - **Navidrome's web player**: a member signs in on `/auth/login` with their account
//!   password. Octo signs in to Navidrome for them and answers with a token of its
//!   own; the real one stays on the member's web device and is swapped in on the way to
//!   Navidrome, and every fresh one Navidrome hands back is swapped for a fresh Octo token.
//! - **Radio streams** name their listener through the stream's session.
//! - **Audio**: every request that hands out audio, a radio stream included, passes the audio
//!   gate ([`audio_gate`]), which may refuse it, sets its quality and tracks its body.
//!
//! When Navidrome refuses a member's rewritten sign-in (someone changed the password there),
//! the gate sets a new hidden password once and tries again.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, OriginalUri, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use chrono::Utc;
use futures::StreamExt;
use http_body_util::BodyExt;
use indexmap::IndexMap;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::family::codes::subsonic_token;
use octo_core::family::device::{Device, NetworkPlace, client_origin, decode_device_name};
use octo_core::family::endpoints::rest_endpoint_name;
use octo_subsonic::SubsonicCredential;
use octo_subsonic::subsonic_request_parser::{
    Parameters, RequestParts, RequestReading, parse_query, read_request, unescape_data_string,
};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::app::AppState;
use crate::http::error::json_relaxed_response;
use crate::middleware::admin_guard;
use crate::services::family::audio_gate;
use crate::services::family::device_registry::detected_device_id;
use crate::services::family::member_bridge::{self, MAX_FAILURES, Rewritten, SignIn, TryKeys, Verdict};
use crate::services::family::web_session::{self, WebClaims};
use crate::services::family::{AnswerContext, FamilyCaller, Member, Sighting, passwords, request_rules};
use crate::services::subsonic::IncomingRequest;

mod reading;
pub use reading::{GateReading, Unverified, Verified, credential_of};

/// The largest body the gate reads (30 MB, the same limit the handlers use).
const MAX_BODY: usize = 30_000_000;
/// The largest body an `importFile` call may carry: the file reader's own limit, and room for
/// the form around it.
const IMPORT_FILE_BODY: usize =
    crate::services::imports::import_file_reader::ImportFileReader::MAX_BYTES as usize + 64 * 1024;

/// The largest body the gate reads for a `/rest` call at `gate_path`.
fn body_limit(gate_path: &str) -> usize {
    match rest_endpoint_name(gate_path) {
        Some(name) if eq_ignore_case(name, "importFile") => IMPORT_FILE_BODY,
        _ => MAX_BODY,
    }
}

/// The largest body the web player's sign-in may carry: a username and a password.
const SIGN_IN_BODY: usize = 16 * 1024;
/// How long an answer waits, at most, for Navidrome to confirm an unchecked sign-in so the
/// device it came from can be noted. The check carries on after that; only the noting waits.
const NOTE_WAIT: Duration = Duration::from_secs(1);
/// How much of an answer is read to see whether Navidrome refused the sign-in. Refusals are a
/// few hundred bytes; anything longer is not one.
const PEEK_LIMIT: usize = 8 * 1024;
/// The largest answer an answer rule is given.
const ANSWER_LIMIT: usize = 64 * 1024 * 1024;
/// How long a browser token lasts when Navidrome's own does not say.
const DEFAULT_WEB_SECONDS: i64 = 48 * 3600;

const WRONG_SIGN_IN: &str = "Wrong username or password";
const PAUSED: &str = "This account is paused by the family owner.";
const NEEDS_REPAIR: &str = "This account needs a repair by the family owner.";
const MULTIPART_SIGN_IN: &str = "Octo cannot read a sign-in inside a multipart form. Send it in the address.";
const UNREADABLE: &str = "Octo could not read this request.";
const WEB_DEVICE_NAME: &str = "Navidrome in a browser";

pub async fn family_gate(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if !state.settings.current().family.enabled {
        return next.run(req).await;
    }
    let sent_path = req.uri().path().to_string();
    let path = gate_path(&sent_path);
    if skipped(&sent_path) && skipped(&path) {
        return next.run(req).await;
    }
    let whereabouts = Whereabouts::of(&state, &req);
    if let Some(token) = radio_token(&sent_path) {
        return radio(&state, req, next, &token, &whereabouts).await;
    }
    if under(&path, "/rest") {
        return rest(state, req, next, whereabouts, &path).await;
    }
    if path.trim_end_matches('/').eq_ignore_ascii_case("/auth/login") && req.method() == Method::POST {
        return web_sign_in(state, req, next, whereabouts).await;
    }
    native(state, req, next, whereabouts, &path).await
}

/// A request path as an upstream server may end up reading it once Octo relays it, which is
/// the path the gate judges a request by: escapes decoded (again while any are left, so an
/// escaped escape counts too), tabs and line breaks dropped and everything from a `?` or `#`
/// cut off (as a URL parser does), `\` read as `/`, empty and `.` segments dropped and `..`
/// taking the segment before it away. Whatever spelling reaches an endpoint upstream, the
/// gate sees that endpoint.
pub(crate) fn gate_path(sent: &str) -> String {
    let mut path = sent.to_string();
    for _ in 0..4 {
        let decoded = unescape_data_string(&path);
        if decoded == path {
            break;
        }
        path = decoded;
    }
    let path: String = path
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .take_while(|c| !matches!(c, '?' | '#'))
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut out = format!("/{}", segments.join("/"));
    if path.ends_with('/') && !segments.is_empty() {
        out.push('/');
    }
    out
}

/// Whether `path` is `prefix` or below it, ignoring case, at a segment boundary.
fn under(path: &str, prefix: &str) -> bool {
    path.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        && (path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/')
}

/// Paths with their own sign-in, or none: the dashboard, the family page and its API, and
/// static assets.
fn skipped(path: &str) -> bool {
    admin_guard::is_admin_path(path)
        || under(path, "/admin")
        || under(path, "/family")
        || under(path, "/api/family")
        || under(path, "/assets")
}

/// The token of a `/radio/stream/{token}` path.
fn radio_token(path: &str) -> Option<String> {
    let prefix = "/radio/stream/";
    let head = path.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let token = path[prefix.len()..].trim_end_matches('/');
    (!token.is_empty() && !token.contains('/')).then(|| token.to_string())
}

/// Where a request comes from: the client address behind trusted proxies, and home or away.
#[derive(Debug, Clone, Copy)]
struct Whereabouts {
    address: IpAddr,
    place: NetworkPlace,
    /// The address Octo can vouch for, whatever hops the request names.
    peer: IpAddr,
}

impl Whereabouts {
    fn of(state: &AppState, req: &Request) -> Self {
        let settings = state.settings.current();
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let forwarded: Vec<String> = req
            .headers()
            .get_all("x-forwarded-for")
            .iter()
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
            .collect();
        let forwarded = (!forwarded.is_empty()).then(|| forwarded.join(","));
        let origin = client_origin(
            peer,
            forwarded.as_deref(),
            &settings.family.trusted_proxy_list(),
            &settings.family.home_network_list(),
        );
        Whereabouts {
            address: origin.address,
            place: origin.place,
            peer: origin.peer,
        }
    }
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_string())
        .filter(|value| !value.is_empty())
}

/// What the request says about its device.
pub(crate) fn sighting(headers: &HeaderMap, parameters: &Parameters) -> Sighting {
    Sighting {
        client: parameters.get("c").cloned().unwrap_or_default(),
        user_agent: header_text(headers, "user-agent").unwrap_or_default(),
        octo_device_id: header_text(headers, "x-octo-device-id"),
        name: header_text(headers, "x-octo-device-name")
            .map(|name| decode_device_name(&name))
            .filter(|name| !name.is_empty()),
        navidrome_client_id: header_text(headers, "x-nd-client-unique-id"),
    }
}

/// A member, with their abilities, preference and device.
fn member_caller(
    state: &AppState,
    member: &Member,
    device_id: Option<String>,
    at: &Whereabouts,
) -> FamilyCaller {
    let mut caller = FamilyCaller::member(&member.username, member.preset, member.abilities());
    caller.device_quality = device_id
        .as_deref()
        .map(|id| state.family_store.device_quality(id))
        .unwrap_or_default();
    caller.device_id = device_id;
    caller.place = at.place;
    caller.address = at.address;
    caller.quality = state.family_store.preference(&member.username);
    caller
}

/// The owner or an account outside the family, with the device the request came from.
fn other_caller(state: &AppState, user: &str, seen: &Sighting, at: &Whereabouts) -> FamilyCaller {
    let mut caller = FamilyCaller::unmanaged(user, state.family_store.is_owner(user));
    let device_id = detected_device_id(user, seen);
    caller.device_quality = state.family_store.device_quality(&device_id);
    caller.device_id = Some(device_id);
    caller.place = at.place;
    caller.address = at.address;
    caller.quality = state.family_store.preference(user);
    caller
}

/// The caller for a username, a member or not.
fn caller_for(
    state: &AppState,
    user: &str,
    device_id: Option<String>,
    seen: &Sighting,
    at: &Whereabouts,
) -> FamilyCaller {
    match state.family_store.member(user) {
        Some(member) => member_caller(state, &member, device_id, at),
        None => other_caller(state, user, seen, at),
    }
}

/// The member's account password, opened: None when they have none kept the sealed way.
fn account_password(state: &AppState, member: &Member) -> Option<String> {
    passwords::open(&state.family_vault, &member.account_password)
}

/// The member's web player session a browser token names, when it is still on: the device it
/// was made for is theirs and still holds its real Navidrome token.
fn web_session_device(state: &AppState, user: &str, claims: &WebClaims) -> Option<Device> {
    state
        .family_store
        .device(&claims.did)
        .filter(|d| eq_ignore_case(&d.username, user) && d.navidrome_jwt.is_some())
}

/// A browser token Octo signed, still good.
fn web_claims(state: &AppState, token: &str) -> Option<WebClaims> {
    let key = state.family_vault.jwt_key()?;
    web_session::verify(&key, token, Utc::now().timestamp())
}

/// How a Subsonic answer is written: `f`, and the `callback` of `jsonp`.
struct Format {
    f: String,
    callback: Option<String>,
}

impl Format {
    fn of(parameters: &Parameters) -> Self {
        Format {
            f: parameters.get("f").map_or("xml", String::as_str).to_string(),
            callback: parameters.get("callback").cloned(),
        }
    }
}

/// A Subsonic error in the format the request asked for (`jsonp` wrapped in its callback).
fn subsonic_error(state: &AppState, format: &Format, code: i32, message: &str) -> Response {
    let builder = &state.subsonic_response_builder;
    if format.f == "jsonp" {
        let reply = builder.create_error("json", code, message);
        let callback = format.callback.as_deref().filter(|c| {
            !c.is_empty()
                && c.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '$'))
        });
        let Some(callback) = callback else {
            return reply.into_response();
        };
        let body = format!("{callback}({});", reply.text());
        let mut response = Response::new(Body::from(body));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/javascript"),
        );
        return response;
    }
    let f = if format.f == "json" { "json" } else { "xml" };
    builder.create_error(f, code, message).into_response()
}

/// A JSON refusal for Navidrome's native API and its web player's sign-in.
fn native_refusal(status: StatusCode, message: &str) -> Response {
    json_relaxed_response(status, &json!({ "error": message }), "application/json").into_response()
}

/// The request rebuilt from `parts` with a new query (kept on [`OriginalUri`] too, so a
/// handler that relays the path as sent relays the new query) and body.
fn assemble(parts: &Parts, query: Option<&str>, body: Bytes, body_changed: bool) -> Request {
    let mut parts = parts.clone();
    let with_query = |uri: &Uri| -> Option<Uri> {
        let path_and_query = match query {
            Some(query) => format!("{}?{query}", uri.path()),
            None => uri.path().to_string(),
        };
        let mut uri_parts = uri.clone().into_parts();
        uri_parts.path_and_query = Some(path_and_query.parse().ok()?);
        Uri::from_parts(uri_parts).ok()
    };
    if let Some(uri) = with_query(&parts.uri) {
        parts.uri = uri;
    }
    if let Some(OriginalUri(original)) = parts.extensions.get::<OriginalUri>().cloned()
        && let Some(uri) = with_query(&original)
    {
        parts.extensions.insert(OriginalUri(uri));
    }
    if body_changed {
        parts.headers.remove(header::TRANSFER_ENCODING);
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    }
    Request::from_parts(parts, Body::from(body))
}

/// What an answer came to, as far as its first few kilobytes say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Subsonic error 40: the sign-in was refused.
    Refused,
    /// Any other error, or an HTTP status that is not a success.
    Failed,
    /// Anything else: an ok answer, audio, or an answer too long to be a refusal.
    Fine,
}

/// Endpoints that answer without a sign-in, so their answer says nothing about one.
fn needs_no_sign_in(endpoint: &str) -> bool {
    eq_ignore_case(endpoint, "getOpenSubsonicExtensions")
}

/// Reads up to [`PEEK_LIMIT`] of a Subsonic answer to see what it came to. Answers the
/// response, whole again, and its [`Outcome`].
async fn peek_outcome(response: Response) -> (Response, Outcome) {
    if !response.status().is_success() {
        return (response, Outcome::Failed);
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(content_type.contains("xml") || content_type.contains("json") || content_type.contains("javascript"))
    {
        return (response, Outcome::Fine);
    }
    let (parts, mut body) = response.into_parts();
    let mut seen = BytesMut::new();
    let mut ended = false;
    while seen.len() <= PEEK_LIMIT {
        match body.frame().await {
            None => {
                ended = true;
                break;
            }
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    seen.extend_from_slice(&data);
                }
            }
            Some(Err(_)) => {
                ended = true;
                break;
            }
        }
    }
    let seen = seen.freeze();
    if ended {
        let outcome = outcome_of(&seen);
        return (Response::from_parts(parts, Body::from(seen)), outcome);
    }
    let first = futures::stream::once(async move { Ok::<Bytes, axum::Error>(seen) });
    let rest = first.chain(body.into_data_stream());
    (
        Response::from_parts(parts, Body::from_stream(rest)),
        Outcome::Fine,
    )
}

/// What a whole Subsonic answer came to, in any format.
fn outcome_of(body: &[u8]) -> Outcome {
    let text = String::from_utf8_lossy(body);
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let failed = compact.contains("\"status\":\"failed\"") || compact.contains("status=\"failed\"");
    if !failed {
        return Outcome::Fine;
    }
    if compact.contains("\"code\":40,") || compact.contains("\"code\":40}") || compact.contains("code=\"40\"")
    {
        Outcome::Refused
    } else {
        Outcome::Failed
    }
}

/// Runs the answer rules registered for `endpoint` over the plain answer.
async fn apply_answer_rules(
    state: &AppState,
    response: Response,
    caller: &FamilyCaller,
    method: &Method,
    endpoint: &str,
    path: &str,
    parameters: &Parameters,
) -> Response {
    let rules = state.family_answer_rules.for_endpoint(endpoint);
    if rules.is_empty() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    // A HEAD answer has no body for a rule to read, and its length would be the length of the
    // answer before the rules, so neither goes out.
    if method == Method::HEAD {
        parts.headers.remove(header::CONTENT_LENGTH);
        parts.headers.remove(header::TRANSFER_ENCODING);
        parts.headers.remove(header::CONTENT_ENCODING);
        return Response::from_parts(parts, Body::empty());
    }
    let Ok(bytes) = axum::body::to_bytes(body, ANSWER_LIMIT).await else {
        warn!("FAMILY an answer to {endpoint} was too large to check, so it was not sent");
        return StatusCode::BAD_GATEWAY.into_response();
    };
    // An answer the rules cannot read never goes out unread: a compressed one is opened first,
    // and one in an unknown coding is not sent.
    let coding = parts
        .headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty() && v != "identity");
    let mut bytes = match coding {
        None => bytes,
        Some(coding) => match decoded(&coding, &bytes) {
            Some(plain) => {
                parts.headers.remove(header::CONTENT_ENCODING);
                Bytes::from(plain)
            }
            None => {
                warn!(
                    "FAMILY an answer to {endpoint} came in a coding Octo cannot read ({coding}), so it was not sent"
                );
                return StatusCode::BAD_GATEWAY.into_response();
            }
        },
    };
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let context = AnswerContext {
        state,
        caller,
        endpoint,
        path,
        parameters,
        content_type: content_type.as_deref(),
    };
    for rule in rules {
        bytes = rule(&context, bytes);
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::TRANSFER_ENCODING);
    Response::from_parts(parts, Body::from(bytes))
}

/// A compressed answer opened (`gzip`, `deflate` or `br`), at most [`ANSWER_LIMIT`] bytes of
/// it. None for any other coding, a broken body, or one that opens larger than that.
fn decoded(coding: &str, body: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let limit = ANSWER_LIMIT as u64 + 1;
    let mut plain = Vec::new();
    let read = match coding {
        "gzip" | "x-gzip" => flate2::read::MultiGzDecoder::new(body)
            .take(limit)
            .read_to_end(&mut plain),
        "deflate" => {
            // Servers send deflate both wrapped (zlib) and raw.
            match flate2::read::ZlibDecoder::new(body)
                .take(limit)
                .read_to_end(&mut plain)
            {
                Ok(n) => Ok(n),
                Err(_) => {
                    plain.clear();
                    flate2::read::DeflateDecoder::new(body)
                        .take(limit)
                        .read_to_end(&mut plain)
                }
            }
        }
        "br" => brotli::Decompressor::new(body, 4096)
            .take(limit)
            .read_to_end(&mut plain),
        _ => return None,
    };
    (read.is_ok() && plain.len() <= ANSWER_LIMIT).then_some(plain)
}

/// The parts of a `/rest` request the gate works from.
struct RestRequest {
    parts: Parts,
    body: Bytes,
    content_type: Option<String>,
    query: Option<String>,
    endpoint: String,
    path: String,
    format: Format,
    seen: Sighting,
    at: Whereabouts,
    /// The request as the gate read it, once.
    reading: RequestReading,
    parameters: Parameters,
}

/// A query and body read once, as every handler reads them.
fn read_once(query: Option<&str>, content_type: Option<&str>, body: &[u8]) -> RequestReading {
    read_request(&RequestParts {
        query,
        content_type,
        content_length: Some(body.len() as u64),
        body,
    })
}

/// The values of a sign-in key, each once.
fn distinct_values(values: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for value in values {
        if !out.contains(&value.as_str()) {
            out.push(value);
        }
    }
    out
}

/// Who a `/rest` request that is not a member's is from.
enum Caller {
    /// Checked already: Navidrome named the owner of the request's API key.
    Checked(FamilyCaller),
    /// Named by the request. The credential is its sign-in exactly as read, for Navidrome to
    /// check; None when it cannot be checked at all.
    Unchecked(FamilyCaller, Option<SubsonicCredential>),
}

/// A `/rest` request. `gate_path` is its path as [`gate_path`] reads it, which names the
/// endpoint.
async fn rest(state: AppState, req: Request, next: Next, at: Whereabouts, gate_path: &str) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, body_limit(gate_path)).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let content_type = header_text(&parts.headers, "content-type");
    let query = parts.uri.query().map(str::to_string);
    let reading = read_once(query.as_deref(), content_type.as_deref(), &body);
    let parameters = reading.parameters();
    let path = parts.uri.path().to_string();
    let request = RestRequest {
        endpoint: rest_endpoint_name(gate_path).unwrap_or("").to_string(),
        format: Format::of(&parameters),
        seen: sighting(&parts.headers, &parameters),
        content_type,
        query,
        path,
        parts,
        body,
        at,
        reading,
        parameters,
    };
    // Every handler works from this one reading, so a body it cannot read would leave the gate
    // and the handlers each guessing at it differently.
    if request.reading.unreadable() {
        return subsonic_error(&state, &request.format, 0, UNREADABLE);
    }
    // A new device redeeming a sign-in hand-over has no sign-in yet, so nobody is named.
    if ["redeemFamilySignIn", "getFamilySignInRedeem"]
        .iter()
        .any(|open| eq_ignore_case(&request.endpoint, open))
    {
        return pass_rest(&state, request, next, None).await;
    }
    let sign_in = SignIn::of(&request.reading, request.content_type.as_deref());
    let users = sign_in.named_users();

    if let Some(member) = users.iter().find_map(|user| state.family_store.member(user)) {
        return member_rest(&state, request, next, &sign_in, member, None).await;
    }
    let mut caller = None;
    if users.is_empty() {
        let api_keys = distinct_values(&sign_in.api_keys);
        if let [api_key] = api_keys.as_slice() {
            let mut ask: IndexMap<String, String> = IndexMap::new();
            ask.insert("apiKey".into(), api_key.to_string());
            for name in ["v", "c"] {
                if let Some(value) = request.parameters.get(name) {
                    ask.insert(name.into(), value.clone());
                }
            }
            if let Ok(Some(owner)) = state.request_identity.username(&ask, &state.subsonic_proxy).await {
                if let Some(member) = state.family_store.member(&owner) {
                    return member_rest(&state, request, next, &sign_in, member, Some(None)).await;
                }
                // Navidrome named the key's owner, so the key is good.
                caller = Some(Caller::Checked(other_caller(
                    &state,
                    &owner,
                    &request.seen,
                    &request.at,
                )));
            }
        } else if api_keys.is_empty() && !sign_in.jwts.is_empty() {
            let signed = sign_in.jwts.iter().find_map(|jwt| {
                let claims = web_claims(&state, jwt)?;
                let member = state.family_store.member(&claims.sub)?;
                web_session_device(&state, &member.username, &claims).map(|device| (member, device.id))
            });
            if let Some((member, device_id)) = signed {
                return member_rest(&state, request, next, &sign_in, member, Some(Some(device_id))).await;
            }
            let mut subjects: Vec<String> = Vec::new();
            for subject in sign_in
                .jwts
                .iter()
                .filter_map(|jwt| web_session::unverified_subject(jwt))
            {
                if !subjects.contains(&subject) {
                    subjects.push(subject);
                }
            }
            // A member never holds a Navidrome token, and an Octo token that did not check out
            // proves nothing, so a token naming a member is refused before anything reads it.
            if let Some(member) = subjects.iter().find_map(|user| state.family_store.member(user)) {
                let keys = try_keys(&state, &request.at, &member.username);
                state.family_bridge.throttle.failed_by(&keys);
                log_refusal(&state, &member.username, &request.at, "a token Octo did not sign");
                return subsonic_error(&state, &request.format, 40, WRONG_SIGN_IN);
            }
            if let [user] = subjects.as_slice() {
                // Navidrome checks its own token; until then the gate only notes whose it is.
                let credential = reading::credential_of(&request.reading, &request.parameters, None);
                caller = Some(Caller::Unchecked(
                    other_caller(&state, user, &request.seen, &request.at),
                    credential,
                ));
            }
        }
    } else if let [user] = users.as_slice() {
        let credential = reading::credential_of(&request.reading, &request.parameters, Some(user));
        caller = Some(Caller::Unchecked(
            other_caller(&state, user, &request.seen, &request.at),
            credential,
        ));
    }
    pass_rest(&state, request, next, caller).await
}

/// What an [`audio_gate::AudioPlan`] leaves of a request: its query, its body when that
/// changed, and the reading of them with its values set.
struct Planned {
    query: Option<String>,
    body: Option<Bytes>,
    reading: RequestReading,
    parameters: Parameters,
}

/// The request an [`audio_gate::AudioPlan`] leaves. None when it changes nothing; Err when a
/// body that carries the quality cannot be rewritten.
fn planned(
    audio: &audio_gate::AudioPlan,
    query: Option<&str>,
    content_type: Option<&str>,
    body: &Bytes,
) -> Result<Option<Planned>, audio_gate::UnreadableBody> {
    let Some(applied) = audio.apply_to(query, content_type, body)? else {
        return Ok(None);
    };
    let reading = read_once(
        applied.query.as_deref(),
        content_type,
        applied.body.as_ref().unwrap_or(body),
    );
    let mut parameters = reading.parameters();
    audio.rewrite().apply_to(&mut parameters);
    Ok(Some(Planned {
        query: applied.query,
        body: applied.body,
        reading,
        parameters,
    }))
}

/// A `/rest` request that is not a member's: handed on as it came, with its caller and the
/// gate's reading.
async fn pass_rest(state: &AppState, request: RestRequest, next: Next, caller: Option<Caller>) -> Response {
    let RestRequest {
        mut parts,
        body,
        endpoint,
        path,
        seen,
        query,
        content_type,
        reading,
        parameters,
        ..
    } = request;
    let method = parts.method.clone();
    let Some(caller) = caller else {
        parts
            .extensions
            .insert(GateReading::unchecked(reading, parameters, None, None, seen));
        return next.run(Request::from_parts(parts, Body::from(body))).await;
    };
    let (caller, credential, checked) = match caller {
        Caller::Checked(caller) => (caller, None, true),
        Caller::Unchecked(caller, credential) => (caller, credential, false),
    };
    // A device is noted only for a sign-in that checks out: an unchecked one is checked beside
    // the call, so wrong sign-ins never make devices or new-device notices.
    if checked {
        state.family_devices.seen(&caller, &seen);
    }
    if let Some(refused) = request_rules(state, &caller, &parts.method, &endpoint, &parameters) {
        return refused;
    }
    let sent = audio_gate::Sent {
        query: query.as_deref(),
        content_type: content_type.as_deref(),
        body: &body,
        reading: Some(&reading),
    };
    let audio = match audio_gate::admit(
        state,
        &caller,
        &parts.method,
        &endpoint,
        &parameters,
        &parts.headers,
        &sent,
    )
    .await
    {
        Ok(audio) => audio,
        Err(refused) => return refused,
    };
    let Ok(plan) = planned(&audio, query.as_deref(), content_type.as_deref(), &body) else {
        return audio_gate::subsonic_refusal(state, &parameters, 0, audio_gate::AUDIO_BODY_REFUSED);
    };
    let rules = state.family_answer_rules.has(&endpoint);
    if rules {
        without_partial_answers(&mut parts.headers);
    }
    let (mut req, view_reading, view_parameters) = match plan {
        None => (
            Request::from_parts(parts, Body::from(body)),
            reading,
            parameters.clone(),
        ),
        Some(plan) => {
            let changed = plan.body.is_some();
            let req = assemble(&parts, plan.query.as_deref(), plan.body.unwrap_or(body), changed);
            (req, plan.reading, plan.parameters)
        }
    };
    let view = if checked {
        GateReading::checked(view_reading, view_parameters, caller.clone())
    } else {
        GateReading::unchecked(
            view_reading,
            view_parameters,
            Some(caller.clone()),
            credential,
            seen,
        )
    };
    req.extensions_mut().insert(caller.clone());
    req.extensions_mut().insert(view.clone());
    if let Some(own) = audio.own_ask() {
        req.extensions_mut().insert(own.clone());
    }
    let noting = note_when_checked(state, &view, &caller, !checked && !needs_no_sign_in(&endpoint));
    let response = next.run(req).await;
    if let Some(noting) = noting {
        let _ = tokio::time::timeout(NOTE_WAIT, noting).await;
    }
    let response = audio.track(state, response);
    if rules {
        apply_answer_rules(state, response, &caller, &method, &endpoint, &path, &parameters).await
    } else {
        response
    }
}

/// Takes off the headers that would have Navidrome answer in part or compressed, for a call
/// whose answer a rule reads whole.
fn without_partial_answers(headers: &mut HeaderMap) {
    headers.remove(header::ACCEPT_ENCODING);
    headers.remove(header::RANGE);
    headers.remove(header::IF_RANGE);
}

/// Starts checking an unchecked caller's sign-in beside the call, which notes their device
/// once Navidrome takes it. Nothing when `wanted` is false or the device was noted lately.
fn note_when_checked(
    state: &AppState,
    view: &GateReading,
    caller: &FamilyCaller,
    wanted: bool,
) -> Option<tokio::task::JoinHandle<()>> {
    let fresh = caller
        .device_id
        .as_deref()
        .is_some_and(|id| !state.family_devices.noted_lately(id));
    if !wanted || !fresh {
        return None;
    }
    let (view, state) = (view.clone(), state.clone());
    Some(tokio::spawn(async move {
        let _ = view.verify(&state).await;
    }))
}

/// A member's `/rest` request. `checked` is None when the sign-in still has to be checked, or
/// the web player session (if any) when an API key or a browser token already named the
/// member. Their device is the one the request describes, recorded once the sign-in checks out.
async fn member_rest(
    state: &AppState,
    request: RestRequest,
    next: Next,
    sign_in: &SignIn,
    member: Member,
    checked: Option<Option<String>>,
) -> Response {
    let user = member.username.clone();
    let keys = try_keys(state, &request.at, &user);
    let throttle = &state.family_bridge.throttle;
    let refuse = |message: &str| subsonic_error(state, &request.format, 40, message);
    let device_id = match checked {
        Some(device_id) => device_id,
        None => {
            if sign_in.named_users().len() > 1 {
                throttle.failed_by(&keys);
                return refuse(WRONG_SIGN_IN);
            }
            if sign_in.in_multipart {
                return refuse(MULTIPART_SIGN_IN);
            }
            if throttle.blocked_by(&keys) {
                log_refusal(state, &user, &request.at, "too many wrong tries");
                return refuse(WRONG_SIGN_IN);
            }
            let password = account_password(state, &member);
            let web_device = sign_in
                .jwts
                .iter()
                .filter_map(|jwt| web_claims(state, jwt))
                .filter(|claims| eq_ignore_case(&claims.sub, &user))
                .find_map(|claims| web_session_device(state, &user, &claims))
                .map(|device| device.id);
            match member_bridge::verify(password.as_deref(), sign_in, web_device.is_some()) {
                Verdict::Verified => web_device,
                Verdict::Missing => {
                    return subsonic_error(state, &request.format, 10, "Required parameter is missing.");
                }
                Verdict::Refused => {
                    throttle.failed_by(&keys);
                    log_refusal(state, &user, &request.at, "a wrong password");
                    return refuse(WRONG_SIGN_IN);
                }
            }
        }
    };
    // The app's own device, told apart by what it sends, unless a web player session named it.
    let device_id = device_id.or_else(|| Some(detected_device_id(&user, &request.seen)));
    // The sign-in checked out, so tries from this address no longer count with strangers'.
    state.family_bridge.known.signed_in(&request.at.address, &user);
    if member.suspended {
        return refuse(PAUSED);
    }
    let Some(hidden) = state.family_vault.open_sealed(&member.navidrome_password) else {
        if state.family_bridge.may_log(&format!("open|{user}")) {
            warn!("FAMILY {user}'s hidden Navidrome password cannot be opened (is family.key missing?)");
        }
        return refuse(NEEDS_REPAIR);
    };
    let caller = member_caller(state, &member, device_id, &request.at);
    state.family_devices.seen(&caller, &request.seen);

    let content_type = request.content_type.as_deref();
    let rewrite_with = |hidden: &str| {
        member_bridge::rewrite(
            request.query.as_deref(),
            content_type,
            &request.body,
            sign_in.user_in_body_only,
            &user,
            hidden,
        )
    };
    let rewritten = rewrite_with(&hidden);
    let parameters = read_once(
        rewritten.query.as_deref(),
        content_type,
        rewritten.body.as_ref().unwrap_or(&request.body),
    )
    .parameters();
    let method = &request.parts.method;
    if let Some(refused) = request_rules(state, &caller, method, &request.endpoint, &parameters) {
        return refused;
    }
    let sent = audio_gate::Sent {
        query: request.query.as_deref(),
        content_type,
        body: &request.body,
        reading: Some(&request.reading),
    };
    let audio = match audio_gate::admit(
        state,
        &caller,
        method,
        &request.endpoint,
        &parameters,
        &request.parts.headers,
        &sent,
    )
    .await
    {
        Ok(audio) => audio,
        Err(refused) => return refused,
    };
    let rules = state.family_answer_rules.has(&request.endpoint);
    // The request as it goes on, carrying the gate's reading of it: the sign-in rewritten,
    // then the quality set, and what every handler reads from the result.
    #[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
    let build = |rewritten: Rewritten| -> Result<Request, Response> {
        let mut changed = rewritten.body.is_some();
        let body = rewritten.body.unwrap_or_else(|| request.body.clone());
        let plan = match planned(&audio, rewritten.query.as_deref(), content_type, &body) {
            Ok(plan) => plan,
            Err(_) => {
                return Err(audio_gate::subsonic_refusal(
                    state,
                    &parameters,
                    0,
                    audio_gate::AUDIO_BODY_REFUSED,
                ));
            }
        };
        let (query, body, reading, view_parameters) = match plan {
            Some(plan) => {
                changed |= plan.body.is_some();
                (
                    plan.query,
                    plan.body.unwrap_or(body),
                    plan.reading,
                    plan.parameters,
                )
            }
            None => {
                let reading = read_once(rewritten.query.as_deref(), content_type, &body);
                let view_parameters = reading.parameters();
                (rewritten.query, body, reading, view_parameters)
            }
        };
        let mut req = assemble(&request.parts, query.as_deref(), body, changed);
        if rules {
            without_partial_answers(req.headers_mut());
        }
        req.extensions_mut().insert(caller.clone());
        req.extensions_mut()
            .insert(GateReading::checked(reading, view_parameters, caller.clone()));
        if let Some(own) = audio.own_ask() {
            req.extensions_mut().insert(own.clone());
        }
        Ok(req)
    };

    let first = match build(rewritten) {
        Ok(first) => first,
        Err(refused) => return refused,
    };
    let (mut response, outcome) = peek_outcome(next.clone().run(first).await).await;
    if outcome == Outcome::Refused {
        match state
            .family_bridge
            .after_refusal(
                &state.navidrome_admin,
                &state.family_store,
                &state.family_vault,
                &user,
                &hidden,
            )
            .await
        {
            Some(retry) => {
                let again = match build(rewrite_with(&retry)) {
                    Ok(again) => again,
                    Err(refused) => return refused,
                };
                let (again, after) = peek_outcome(next.run(again).await).await;
                if after == Outcome::Refused && state.family_bridge.may_log(&format!("repair|{user}")) {
                    warn!("FAMILY Navidrome still refuses {user}'s sign-in after a new hidden password");
                }
                response = again;
            }
            None => {
                if state.family_bridge.may_log(&format!("refused|{user}")) {
                    warn!("FAMILY Navidrome refuses {user}'s hidden password");
                }
            }
        }
    }
    let response = audio.track(state, response);
    if rules {
        apply_answer_rules(
            state,
            response,
            &caller,
            method,
            &request.endpoint,
            &request.path,
            &parameters,
        )
        .await
    } else {
        response
    }
}

/// The keys a sign-in try at `user` from `at` counts under.
fn try_keys(state: &AppState, at: &Whereabouts, user: &str) -> TryKeys {
    state
        .family_bridge
        .try_keys(&at.address, &at.peer, user, MAX_FAILURES)
}

/// One log line per member a minute about refused sign-ins, naming home or away only.
fn log_refusal(state: &AppState, user: &str, at: &Whereabouts, why: &str) {
    if state.family_bridge.may_log(&format!("sign-in|{user}")) {
        let place = match at.place {
            NetworkPlace::Home => "home",
            NetworkPlace::Away => "away",
        };
        info!("FAMILY sign-in refused for {user} from {place} ({why})");
    }
}

/// A radio stream: its session names the listener. A session is only handed out in an answer
/// Navidrome gave to a sign-in it accepted, so its listener counts as checked.
async fn radio(state: &AppState, mut req: Request, next: Next, token: &str, at: &Whereabouts) -> Response {
    if let Some(session) = state.last_fm_radio_stream_sessions.get(token, None)
        && !session.username.trim().is_empty()
    {
        let seen = sighting(req.headers(), &Parameters::new());
        let mut caller = caller_for(state, &session.username, None, &seen, at);
        state.family_devices.seen(&caller, &seen);
        let audio =
            match audio_gate::admit_radio(state, &mut caller, token, &session.station_id, req.method()) {
                Ok(audio) => audio,
                Err(refused) => return refused,
            };
        req.extensions_mut().insert(caller);
        return audio.track(state, next.run(req).await);
    }
    next.run(req).await
}

/// The bearer tokens and `jwt` values a native request carries.
fn native_tokens(req: &Request) -> Vec<String> {
    let mut tokens = Vec::new();
    for name in ["x-nd-authorization", "authorization"] {
        for value in req.headers().get_all(name) {
            if let Some(token) = value.to_str().ok().and_then(web_session::bearer) {
                tokens.push(token.to_string());
            }
        }
    }
    if let Some(values) = parse_query(req.uri().query().unwrap_or("")).get("jwt") {
        tokens.extend(values.iter().filter(|v| !v.is_empty()).cloned());
    }
    tokens
}

/// Replaces `from` with `to` in the request's bearer headers and `jwt` query values.
fn swap_token(req: Request, from: &str, to: &str) -> Request {
    let (mut parts, body) = req.into_parts();
    for name in [
        header::HeaderName::from_static("x-nd-authorization"),
        header::AUTHORIZATION,
    ] {
        let values: Vec<HeaderValue> = parts.headers.get_all(&name).iter().cloned().collect();
        if values.is_empty() {
            continue;
        }
        parts.headers.remove(&name);
        for value in values {
            let swapped = match value.to_str().ok().and_then(web_session::bearer) {
                Some(token) if token == from => {
                    HeaderValue::from_str(&format!("Bearer {to}")).unwrap_or(value)
                }
                _ => value,
            };
            parts.headers.append(&name, swapped);
        }
    }
    let query = parts.uri.query().map(|query| {
        query
            .split('&')
            .map(|segment| match segment.split_once('=') {
                Some((name, value))
                    if eq_ignore_case(name, "jwt")
                        && octo_subsonic::subsonic_request_parser::unescape_data_string(value) == from =>
                {
                    format!(
                        "{name}={}",
                        octo_subsonic::subsonic_request_parser::escape_data_string(to)
                    )
                }
                _ => segment.to_string(),
            })
            .collect::<Vec<_>>()
            .join("&")
    });
    let original = parts.uri.query().map(str::to_string);
    if query == original {
        return Request::from_parts(parts, body);
    }
    let stub = assemble(&parts, query.as_deref(), Bytes::new(), false);
    let (parts, _) = stub.into_parts();
    Request::from_parts(parts, body)
}

/// A browser token for a member's web device, lasting as long as Navidrome's `real` one.
fn mint_for(
    state: &AppState,
    member: &Member,
    device_id: &str,
    real: &str,
    uid: Option<&str>,
) -> Option<String> {
    let key = state.family_vault.jwt_key()?;
    let now = Utc::now().timestamp();
    let claims = WebClaims {
        iss: "ND".into(),
        sub: member.username.clone(),
        uid: uid
            .filter(|u| !u.is_empty())
            .map_or_else(|| member.navidrome_user_id.clone(), str::to_string),
        adm: false,
        iat: now,
        exp: web_session::unverified_expiry(real).unwrap_or(now + DEFAULT_WEB_SECONDS),
        did: device_id.to_string(),
    };
    Some(web_session::mint(&key, &claims))
}

/// Swaps a fresh Navidrome token in a member's answer for a fresh Octo token, keeping the
/// real one on the device. No real token ever goes back to a member, and a session that ended
/// while the call was with Navidrome (the password changed, say) stays ended: its fresh token is
/// dropped and no Octo token is made.
fn swap_answer_token(state: &AppState, response: &mut Response, member: &Member, device_id: &str) {
    let name = header::HeaderName::from_static("x-nd-authorization");
    let Some(value) = response.headers().get(&name).cloned() else {
        return;
    };
    response.headers_mut().remove(&name);
    let text = value.to_str().unwrap_or("").trim().to_string();
    let (prefix, real) = match web_session::bearer(&text) {
        Some(token) => ("Bearer ", token.to_string()),
        None => ("", text),
    };
    if real.is_empty() || web_claims(state, &real).is_some() {
        return;
    }
    let sealed = state.family_vault.seal(&real);
    if !state.family_store.renew_web_session(device_id, sealed) {
        return;
    }
    if let Some(octo) = mint_for(state, member, device_id, &real, None)
        && let Ok(value) = HeaderValue::from_str(&format!("{prefix}{octo}"))
    {
        response.headers_mut().insert(name, value);
    }
}

/// Navidrome's native API and anything else that is not `/rest`. `gate_path` is the path as
/// [`gate_path`] reads it, which the rules judge.
async fn native(state: AppState, req: Request, next: Next, at: Whereabouts, gate_path: &str) -> Response {
    let tokens = native_tokens(&req);
    if tokens.is_empty() {
        return next.run(req).await;
    }
    let seen = sighting(req.headers(), &Parameters::new());
    let path = gate_path.to_string();
    let octo = tokens
        .iter()
        .find_map(|token| web_claims(&state, token).map(|claims| (token.clone(), claims)));
    let Some((octo_token, claims)) = octo else {
        let subjects: Vec<(String, String)> = tokens
            .iter()
            .filter_map(|t| web_session::unverified_subject(t).map(|user| (user, t.clone())))
            .collect();
        // A member never holds a Navidrome token, and an Octo token that did not check out
        // proves nothing, so a token naming a member is refused before anything reads it.
        if let Some(member) = subjects
            .iter()
            .find_map(|(user, _)| state.family_store.member(user))
        {
            let keys = try_keys(&state, &at, &member.username);
            state.family_bridge.throttle.failed_by(&keys);
            log_refusal(&state, &member.username, &at, "a token Octo did not sign");
            return native_refusal(StatusCode::UNAUTHORIZED, "Sign in again.");
        }
        // A Navidrome token: Navidrome checks it; the gate only notes whose it is.
        let Some((user, token)) = subjects.first() else {
            return next.run(req).await;
        };
        let caller = other_caller(&state, user, &seen, &at);
        let mut sign_in: IndexMap<String, String> = IndexMap::new();
        sign_in.insert("u".into(), user.clone());
        sign_in.insert("jwt".into(), token.clone());
        let credential = SubsonicCredential::from(&sign_in);
        return pass_native(
            &state,
            req,
            next,
            caller,
            NativeCheck::Unchecked(credential),
            seen,
            &path,
        )
        .await;
    };
    let member = state.family_store.member(&claims.sub);
    let device = state.family_store.device(&claims.did);
    let (Some(member), Some(device)) = (member, device) else {
        return native_refusal(StatusCode::UNAUTHORIZED, "Sign in again.");
    };
    if !eq_ignore_case(&device.username, &member.username) {
        let keys = try_keys(&state, &at, &member.username);
        state.family_bridge.throttle.failed_by(&keys);
        return native_refusal(StatusCode::UNAUTHORIZED, "Sign in again.");
    }
    if member.suspended {
        return native_refusal(StatusCode::UNAUTHORIZED, PAUSED);
    }
    let Some(real) = device
        .navidrome_jwt
        .as_deref()
        .and_then(|sealed| state.family_vault.open_sealed(sealed))
    else {
        return native_refusal(StatusCode::UNAUTHORIZED, "Sign in again.");
    };
    let req = swap_token(req, &octo_token, &real);
    let caller = member_caller(&state, &member, Some(device.id.clone()), &at);
    let mut response = pass_native(&state, req, next, caller, NativeCheck::Checked, seen, &path).await;
    swap_answer_token(&state, &mut response, &member, &device.id);
    response
}

/// Whether a native request's caller is checked.
enum NativeCheck {
    /// A member whose Octo token checked out.
    Checked,
    /// Someone named by a Navidrome token, with the sign-in Navidrome checks it with.
    Unchecked(Option<SubsonicCredential>),
}

async fn pass_native(
    state: &AppState,
    req: Request,
    next: Next,
    caller: FamilyCaller,
    check: NativeCheck,
    seen: Sighting,
    path: &str,
) -> Response {
    let checked = matches!(check, NativeCheck::Checked);
    if checked {
        state.family_devices.seen(&caller, &seen);
    }
    let (mut parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let content_type = header_text(&parts.headers, "content-type");
    let reading = read_once(parts.uri.query(), content_type.as_deref(), &body);
    // Navidrome's own API reads its bodies itself, so nothing is refused here: the parameters
    // are the ones the catch-all has always read from such a request.
    let parameters = IncomingRequest::new(
        parts.method.clone(),
        parts.headers.clone(),
        parts.uri.query().map(str::to_string),
        body.clone(),
    )
    .parameters();
    if let Some(refused) = request_rules(state, &caller, &parts.method, path, &parameters) {
        return refused;
    }
    let view = match check {
        NativeCheck::Checked => GateReading::checked(reading, parameters, caller.clone()),
        NativeCheck::Unchecked(credential) => {
            GateReading::unchecked(reading, parameters, Some(caller.clone()), credential, seen)
        }
    };
    parts.extensions.insert(caller.clone());
    parts.extensions.insert(view.clone());
    // An unchecked token's device is noted only once Navidrome has taken the token.
    let noting = note_when_checked(state, &view, &caller, !checked);
    let response = next.run(Request::from_parts(parts, Body::from(body))).await;
    if let Some(noting) = noting {
        let _ = tokio::time::timeout(NOTE_WAIT, noting).await;
    }
    response
}

/// Navidrome's own sign-in for a member, with the hidden password.
async fn navidrome_login(
    state: &AppState,
    headers: &HeaderMap,
    user: &str,
    hidden: &str,
) -> Option<(u16, Bytes)> {
    let mut sent = HeaderMap::new();
    sent.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    for name in ["user-agent", "x-nd-client-unique-id"] {
        if let Some(value) = headers.get(name) {
            sent.insert(header::HeaderName::from_static(name), value.clone());
        }
    }
    let body = json!({ "username": user, "password": hidden }).to_string();
    let incoming = IncomingRequest::new(Method::POST, sent, None, Bytes::from(body));
    let relay = state.subsonic_proxy.with_request(Arc::new(incoming));
    match relay
        .relay_raw("auth/login", std::iter::empty::<(&str, &str)>())
        .await
    {
        Ok(raw) => Some((raw.status, raw.body)),
        Err(e) => {
            warn!(
                "FAMILY could not reach Navidrome to sign {user} in: {}",
                e.type_name()
            );
            None
        }
    }
}

/// `POST /auth/login` from Navidrome's web player. Members sign in here; everyone else goes
/// on to Navidrome as before.
async fn web_sign_in(state: AppState, req: Request, next: Next, at: Whereabouts) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, SIGN_IN_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let sent: Option<Value> = serde_json::from_slice(&body).ok();
    let text = |name: &str| {
        sent.as_ref()
            .and_then(|v| v.get(name))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let (Some(username), Some(password)) = (text("username"), text("password")) else {
        return next.run(Request::from_parts(parts, Body::from(body))).await;
    };
    let Some(member) = state.family_store.member(username.trim()) else {
        return next.run(Request::from_parts(parts, Body::from(body))).await;
    };
    let user = member.username.clone();
    let keys = try_keys(&state, &at, &user);
    let throttle = &state.family_bridge.throttle;
    // The try counts before the password is checked, so tries sent all at once cannot each
    // pass the throttle while the others are still being checked. A right one is forgiven.
    if !throttle.attempt_by(&keys) {
        log_refusal(&state, &user, &at, "too many wrong tries");
        return native_refusal(StatusCode::UNAUTHORIZED, "Invalid username or password");
    }
    if !passwords::matches(&state.family_vault, &member.account_password, &password) {
        log_refusal(&state, &user, &at, "a wrong password");
        return native_refusal(StatusCode::UNAUTHORIZED, "Invalid username or password");
    }
    throttle.forgive_by(&keys);
    state.family_bridge.known.signed_in(&at.address, &user);
    if member.suspended {
        return native_refusal(StatusCode::UNAUTHORIZED, PAUSED);
    }
    if state.family_vault.jwt_key().is_none() {
        return native_refusal(StatusCode::UNAUTHORIZED, NEEDS_REPAIR);
    }
    let Some(hidden) = state.family_vault.open_sealed(&member.navidrome_password) else {
        return native_refusal(StatusCode::UNAUTHORIZED, NEEDS_REPAIR);
    };
    let Some(mut answer) = navidrome_login(&state, &parts.headers, &user, &hidden).await else {
        return native_refusal(StatusCode::BAD_GATEWAY, "Navidrome is not answering.");
    };
    if answer.0 == 401
        && let Some(retry) = state
            .family_bridge
            .after_refusal(
                &state.navidrome_admin,
                &state.family_store,
                &state.family_vault,
                &user,
                &hidden,
            )
            .await
        && let Some(again) = navidrome_login(&state, &parts.headers, &user, &retry).await
    {
        answer = again;
    }
    let (status, body) = answer;
    let navidrome: Option<Value> = serde_json::from_slice(&body).ok();
    let real = navidrome
        .as_ref()
        .and_then(|v| v.get("token"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let (200, Some(Value::Object(mut fields)), Some(real)) = (status, navidrome, real) else {
        if status == 401 && state.family_bridge.may_log(&format!("web|{user}")) {
            warn!("FAMILY Navidrome refuses {user}'s hidden password for the web player");
        }
        return native_refusal(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            "Navidrome did not sign this account in.",
        );
    };
    let seen = Sighting {
        client: "NavidromeUI".into(),
        ..sighting(&parts.headers, &Parameters::new())
    };
    let sealed_real = state.family_vault.seal(&real);
    // The password was checked before Navidrome was asked; a change or reset since then means
    // this sign-in makes no session.
    let Some(device) = web_device(&state, &member, &seen, &at, sealed_real) else {
        return native_refusal(StatusCode::UNAUTHORIZED, "Invalid username or password");
    };
    let uid = fields.get("id").and_then(Value::as_str).map(str::to_string);
    let Some(octo) = mint_for(&state, &member, &device.id, &real, uid.as_deref()) else {
        return native_refusal(StatusCode::UNAUTHORIZED, NEEDS_REPAIR);
    };
    let salt = hex::encode(rand::random::<[u8; 6]>());
    fields.insert("token".into(), Value::String(octo));
    // The web player signs its Subsonic calls with the account password the member typed.
    fields.insert(
        "subsonicToken".into(),
        Value::String(subsonic_token(&password, &salt)),
    );
    fields.insert("subsonicSalt".into(), Value::String(salt));
    let caller = member_caller(&state, &member, Some(device.id.clone()), &at);
    state.family_devices.seen(&caller, &seen);
    json_relaxed_response(StatusCode::OK, &Value::Object(fields), "application/json").into_response()
}

/// The member's web device for this sign-in (the browser, told apart by what it sends),
/// recorded if it is new; its web player session becomes `sealed_real`. None when the member's
/// password is no longer the one `member` was checked with, or they were paused meanwhile: then
/// no session starts.
fn web_device(
    state: &AppState,
    member: &Member,
    seen: &Sighting,
    at: &Whereabouts,
    sealed_real: Option<String>,
) -> Option<Device> {
    let store = &state.family_store;
    let id = detected_device_id(&member.username, seen);
    let app = octo_core::family::device::app_label(&seen.client, &seen.user_agent);
    if store.record_device(
        &id,
        &member.username,
        &app,
        at.place,
        Some(WEB_DEVICE_NAME),
        Utc::now(),
    ) {
        info!("FAMILY web player signed in for {}", member.username);
    }
    let started = store.start_web_session(&id, &member.username, &member.account_password, sealed_real);
    store.flush_if_dirty();
    if !started {
        return None;
    }
    store.device(&id)
}

#[cfg(test)]
#[path = "family_gate_tests.rs"]
mod tests;
