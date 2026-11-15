//! The private helpers the Subsonic actions call: reading a request, the success check on a
//! Navidrome answer, the caller checks (a ping, the credential check), who a request is, and the
//! small result helpers.

use std::sync::{Arc, OnceLock};

use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use octo_subsonic::SubsonicCredential;
use octo_subsonic::subsonic_request_parser::Parameters;
use octo_subsonic::subsonic_response_builder::SubsonicReply;
use octo_subsonic::xml::XElement;

use super::media_convert::OutsideAsk;
use crate::app::AppState;
use crate::http::error::AppError;
use crate::services::common::StarOnArrival;
use crate::services::prefetch::convert_rules::OwnAsk;
use crate::services::subsonic::{CredentialVerdict, IncomingRequest, SubsonicProxyService};

/// The largest request body accepted, in bytes.
pub(crate) const MAX_REQUEST_BODY: usize = 30_000_000;

/// One Subsonic request as an action reads it: the client's request (for the proxy, which
/// forwards repeated keys and headers as they came), its parameters, and the request-scoped
/// proxy.
pub(crate) struct SubsonicCall {
    pub incoming: Arc<IncomingRequest>,
    pub parameters: Parameters,
    pub proxy: SubsonicProxyService,
    /// The client's own audio ask, as the family gate read it before setting its quality.
    pub own_ask: Option<OwnAsk>,
    /// What the client asks of a song found online, read once by `stream`.
    pub outside: OnceLock<OutsideAsk>,
}

impl SubsonicCall {
    /// Reads the body and the parameters (query, then form or JSON body); while Family is on,
    /// the parameters are the ones the family gate read.
    #[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
    pub async fn read(state: &AppState, req: Request) -> Result<SubsonicCall, Response> {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, MAX_REQUEST_BODY)
            .await
            .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE.into_response())?;
        Ok(Self::from_parts(state, &parts, body))
    }

    /// The call over a request already read: its parts and its whole body.
    pub fn from_parts(
        state: &AppState,
        parts: &axum::http::request::Parts,
        body: axum::body::Bytes,
    ) -> SubsonicCall {
        let incoming = Arc::new(IncomingRequest::from_parts(parts, body));
        let parameters = incoming.parameters();
        let proxy = state.subsonic_proxy.with_request(Arc::clone(&incoming));
        SubsonicCall {
            incoming,
            parameters,
            proxy,
            own_ask: parts.extensions.get::<OwnAsk>().cloned(),
            outside: OnceLock::new(),
        }
    }

    /// What the client asks of a song found online, once `stream` read it.
    pub fn outside_ask(&self) -> Option<&OutsideAsk> {
        self.outside.get()
    }

    /// Whether this request starts a play: for a song found online as `stream` read it (a time
    /// seek of a playing song is not a new play), for any other request from its first byte. A
    /// scout read (`X-Octo-Purpose: scout`), an Octo app analyzing a song it plays or will play,
    /// starts none.
    pub fn starts_play(&self) -> bool {
        match self.outside.get() {
            Some(outside) => outside.play_start,
            None => {
                let scout = self
                    .header("X-Octo-Purpose")
                    .is_some_and(|purpose| purpose.trim().eq_ignore_ascii_case("scout"));
                !scout && is_first_byte_request(self.method().as_str(), self.header("Range").as_deref())
            }
        }
    }

    /// The parameter `key`, or "" when it was not sent.
    pub fn param(&self, key: &str) -> &str {
        self.parameters.get(key).map_or("", String::as_str)
    }

    /// The parameter `key`: `None` when the key was not sent.
    pub fn param_opt(&self, key: &str) -> Option<&str> {
        self.parameters.get(key).map(String::as_str)
    }

    /// The response format, `f`, or "xml" when it was not sent.
    pub fn format(&self) -> String {
        self.parameters.get("f").map_or("xml", String::as_str).to_string()
    }

    pub fn method(&self) -> &Method {
        &self.incoming.method
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.incoming.headers
    }

    /// Every value of the header, comma-joined, or `None` when it was not sent.
    pub fn header(&self, name: &str) -> Option<String> {
        let values: Vec<String> = self
            .incoming
            .headers
            .get_all(name)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect();
        (!values.is_empty()).then(|| values.join(","))
    }

    /// The header's first value.
    fn first_header(&self, name: &str) -> Option<String> {
        self.incoming
            .headers
            .get(name)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
    }
}

/// `body` as `content_type`: 200 with the bytes as they are.
pub(crate) fn file(body: impl Into<Vec<u8>>, content_type: &str) -> Response {
    SubsonicReply::file(body.into(), content_type).into_response()
}

/// The same request, asking Navidrome for JSON, which is what the merges read.
pub(crate) fn as_json(parameters: &Parameters) -> Parameters {
    let mut json = parameters.clone();
    json.insert("f".into(), "json".into());
    json
}

/// `IsTrue`: "true" in any case, or "1".
pub(crate) fn is_true(value: &str) -> bool {
    value.eq_ignore_ascii_case("true") || value == "1"
}

/// Whether Navidrome's answer is an ok envelope, read in the format asked for (JSON when `f`
/// is "json" in any case, XML otherwise). Anything unreadable is not.
pub(crate) fn is_successful_subsonic_response(body: &[u8], format: &str) -> bool {
    if format.eq_ignore_ascii_case("json") {
        let Ok(document) = serde_json::from_slice::<serde_json::Value>(body) else {
            return false;
        };
        // A root that is not an object, or a status that is not a string, reads as not ok.
        return matches!(
            document
                .as_object()
                .and_then(|root| root.get("subsonic-response"))
                .and_then(|response| response.as_object())
                .and_then(|response| response.get("status")),
            Some(serde_json::Value::String(status)) if status == "ok"
        );
    }
    let text = String::from_utf8_lossy(body);
    XElement::parse(&text).is_ok_and(|root| root.attribute("status") == Some("ok"))
}

/// `None` when Navidrome accepts the caller's credentials, or the error to send (always JSON).
pub(crate) async fn check_caller(state: &AppState, call: &SubsonicCall) -> Option<Response> {
    let mut auth = call.parameters.clone();
    auth.insert("f".into(), "json".into());
    let Some(check) = call.proxy.relay_safe("rest/ping", &auth).await else {
        return Some(
            state
                .subsonic_response_builder
                .create_error("json", 0, "Octo can't reach Navidrome to check who is asking")
                .into_response(),
        );
    };
    if !is_successful_subsonic_response(&check.body, "json") {
        return Some(
            state
                .subsonic_response_builder
                .create_error("json", 40, "Wrong username or password")
                .into_response(),
        );
    }
    None
}

/// `None` when Navidrome accepts the request's sign-in, else the error to answer with, in the
/// format asked for. An outage refuses too: an outside song is Octo fetching from the internet
/// for whoever asks, and a broken Navidrome must not make that anyone at all.
pub(crate) async fn refuse_unless_signed_in(
    state: &AppState,
    call: &SubsonicCall,
    format: &str,
) -> Result<Option<Response>, AppError> {
    let credential = SubsonicCredential::from(&call.parameters);
    let verdict = state
        .credential_check
        .check(credential.as_ref(), &call.proxy)
        .await?;
    Ok(match verdict {
        CredentialVerdict::Accepted => None,
        CredentialVerdict::Unreachable => Some(
            state
                .subsonic_response_builder
                .create_error(format, 0, "Octo can't reach Navidrome to check who is asking")
                .into_response(),
        ),
        _ => Some(
            state
                .subsonic_response_builder
                .create_error(format, 40, "Wrong username or password")
                .into_response(),
        ),
    })
}

/// Who an accepted request signed in as: u, or its API key's owner as Navidrome names it, else
/// the native token's name. Ask only after the sign-in is accepted.
pub(crate) async fn signed_in_user(state: &AppState, call: &SubsonicCall) -> Result<String, AppError> {
    match state
        .request_identity
        .username(&call.parameters, &call.proxy)
        .await?
    {
        Some(username) => Ok(username),
        None => Ok(native_username(state, call)),
    }
}

/// The listener's name for a per-listener feature: [`signed_in_user`], or "" when it is unknown
/// or cannot be asked. Never an error.
pub(crate) async fn listener_name(state: &AppState, call: &SubsonicCall) -> String {
    signed_in_user(state, call).await.unwrap_or_default()
}

/// [`listener_name`] for a call held as its parts: the parameters, the request-scoped proxy and
/// the headers.
pub(crate) async fn listener_name_of(
    state: &AppState,
    parameters: &Parameters,
    proxy: &SubsonicProxyService,
    headers: &HeaderMap,
) -> String {
    match state.request_identity.username(parameters, proxy).await {
        Ok(Some(username)) => username,
        Ok(None) => super::action_helpers::native_username(state, parameters, headers),
        Err(_) => String::new(),
    }
}

/// `NativeUsername`: u, else the Navidrome JWT carried in `X-Nd-Authorization` or
/// `Authorization: Bearer`: first the token map captured from logins, then the payload's
/// `username`, `preferred_username`, `user`, `name` or `sub` claim. Empty when none says.
pub(crate) fn native_username(state: &AppState, call: &SubsonicCall) -> String {
    if let Some(username) = call.param_opt("u").filter(|u| !u.is_empty()) {
        return username.to_string();
    }
    let header = call
        .first_header("X-Nd-Authorization")
        .or_else(|| call.first_header("Authorization"));
    let token = header.map(|h| {
        if h.len() >= 7 && h.is_char_boundary(7) && h[..7].eq_ignore_ascii_case("Bearer ") {
            h[7..].to_string()
        } else {
            h
        }
    });
    if let Some(captured) = state
        .navidrome_identity
        .username_for_native_token(token.as_deref())
        .filter(|c| !c.is_empty())
    {
        return captured;
    }
    // An accepted but opaque token simply exposes no Radio rows.
    token.as_deref().and_then(jwt_claim_name).unwrap_or_default()
}

/// The name a JWT's payload carries: any failure on the way (no second segment, bad base64, bad
/// JSON, a claim that is not a string) gives nothing.
fn jwt_claim_name(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?.replace('-', "+").replace('_', "/");
    let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .ok()?;
    let document: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let root = document.as_object()?;
    for claim in ["username", "preferred_username", "user", "name", "sub"] {
        match root.get(claim) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::String(found)) if !found.is_empty() => return Some(found.clone()),
            Some(serde_json::Value::String(_)) => {}
            // Anything else ends the whole lookup.
            Some(_) => return None,
        }
    }
    None
}

/// `RequesterFor`: the user's name for attribution, only while `RecordRequestedBy` is on.
///
/// Gated here rather than at the history write, so with the setting off no username is
/// captured in the first place and nothing downstream is ever holding one.
pub(crate) fn requester_for(state: &AppState, username: Option<&str>) -> Option<String> {
    let record = state.settings.current().subsonic.record_requested_by;
    username
        .filter(|u| record && !octo_core::common::dotnet::is_blank(u))
        .map(str::to_string)
}

/// `FavoriteCredential`: the sign-in to favorite a hearted outside song or album with, or
/// `None`. Held for every heart from another app, not only while downloads are favorited: a
/// song that turns out to be in the library already is always that person's favorite. Octo's
/// own apps send star for Add, which asks for a copy and not a favorite.
pub(crate) fn favorite_credential(call: &SubsonicCall) -> Option<SubsonicCredential> {
    if StarOnArrival::is_octo_app(call.param_opt("c")) {
        return None;
    }
    SubsonicCredential::from(&call.parameters)
}

/// `IsFirstByteRequest`: no Range, or a Range from byte 0, starts a track. A HEAD plays
/// nothing; the route does not take HEAD today, and this keeps it that way if it ever does.
pub(crate) fn is_first_byte_request(method: &str, range: Option<&str>) -> bool {
    !method.eq_ignore_ascii_case("HEAD")
        && match range {
            None => true,
            Some(range) if octo_core::common::dotnet::is_blank(range) => true,
            Some(range) => octo_core::common::dotnet::starts_with_ignore_case(range.trim_start(), "bytes=0-"),
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_navidrome_answer_is_ok_only_when_its_envelope_says_so() {
        let cases: [(&[u8], &str, bool); 9] = [
            (br#"{"subsonic-response":{"status":"ok"}}"#, "json", true),
            (br#"{"subsonic-response":{"status":"ok"}}"#, "JSON", true),
            (br#"{"subsonic-response":{"status":"failed"}}"#, "json", false),
            (br#"{"subsonic-response":{"status":1}}"#, "json", false),
            (br#"[1]"#, "json", false),
            (b"not json", "json", false),
            (br#"<subsonic-response status="ok"/>"#, "xml", true),
            (br#"<subsonic-response status="failed"/>"#, "jsonp", false),
            (br#"{"subsonic-response":{"status":"ok"}}"#, "xml", false),
        ];
        for (body, format, ok) in cases {
            assert_eq!(
                is_successful_subsonic_response(body, format),
                ok,
                "{} as {format}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn a_jwt_names_its_user_from_the_first_claim_that_does() {
        let token = |payload: &str| {
            format!(
                "x.{}.y",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
            )
        };
        assert_eq!(
            jwt_claim_name(&token(r#"{"sub":"s","name":"n"}"#)).as_deref(),
            Some("n")
        );
        assert_eq!(
            jwt_claim_name(&token(r#"{"username":"","sub":"s"}"#)).as_deref(),
            Some("s")
        );
        // A claim that is not a string ends the lookup.
        assert_eq!(jwt_claim_name(&token(r#"{"username":7,"sub":"s"}"#)), None);
        assert_eq!(jwt_claim_name("opaque"), None);
    }
}
