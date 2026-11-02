//! The admin sign-in gate: nothing under /api/admin answers without a sign-in, apart
//! from the endpoints listed in [`OPEN_ENDPOINTS`]. The dashboard signs in with a Navidrome admin
//! account (`browse/auth`) or the recovery code; a script does the same with a cookie jar; the
//! Octo app signs the calls in [`APP_CALL_ENDPOINTS`] with its own Subsonic sign-in.
//!
//! One layer in front of every admin route, denying by default. axum has no endpoint attributes
//! to read, so the open and app routes are named here, by method and template, and everything
//! else, routes added later and paths no route knows included, asks for a sign-in. A test walks
//! every registered admin route to prove it (`admin_sign_in_tests.rs`).
//!
//! Runs after the request guard (`admin_guard`): that stops other websites, this stops people.
//! Both are needed: a page on a sibling subdomain or another port of the same host is same-site,
//! so the browser sends it the cookie, and only the guard's header rule stops its writes.

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use indexmap::IndexMap;
use octo_subsonic::SubsonicCredential;
use serde_json::json;

use super::admin_guard::is_admin_path;
use super::forwarded::RequestScheme;
use crate::app::AppState;
use crate::controllers::admin::plumbing::{
    BROWSE_COOKIE_NAME, BROWSE_TOKEN_HEADER, browse_cookie_for, header_value, is_https, request_cookie,
};
use crate::http::error::json_relaxed_response;
use crate::http::routes::PathCanon;
use crate::services::admin::sign_in_throttle::key_of;
use crate::services::admin::{AdminCaller, AdminCallerKind, BrowseSessionStore, SessionKind};
use crate::services::subsonic::{CredentialVerdict, IncomingRequest, navidrome_admin_role};

/// The admin endpoints that answer without a sign-in: signing in and out, who is signed in, and a
/// bare status for `install.sh` and the app's Octo check.
pub const OPEN_ENDPOINTS: &[(Method, &str)] = &[
    (Method::POST, "/api/admin/auth/recovery"),
    (Method::POST, "/api/admin/browse/auth"),
    (Method::GET, "/api/admin/browse/session"),
    (Method::POST, "/api/admin/browse/signout"),
    (Method::GET, "/api/admin/status"),
];

/// The admin endpoints the Octo app may call with its own Subsonic sign-in, and whether only a
/// Navidrome admin's sign-in will do (`adminOnly`); a listener's reaches the others, scoped to
/// themselves.
pub const APP_CALL_ENDPOINTS: &[(Method, &str, bool)] = &[
    (Method::GET, "/api/admin/downloads", true),
    (Method::GET, "/api/admin/lastfm/radio", false),
    (Method::POST, "/api/admin/lastfm/radio/refresh", false),
    (Method::GET, "/api/admin/listening", false),
    (Method::GET, "/api/admin/library-status", true),
    (Method::GET, "/api/admin/status", true),
    (Method::GET, "/api/admin/update", true),
    (Method::POST, "/api/admin/update", true),
];

/// The words a Family page sign-in is refused with on every dashboard route.
pub const FAMILY_SESSION_REFUSED: &str = "This page is for the server owner.";

/// The words most library-file routes refuse a sign-in that is not a Navidrome admin's with.
pub const SIGN_IN_FIRST: &str = "Sign in with your Navidrome admin account first.";

/// The admin routes that read or change the music library's files, and what each answers a
/// recovery-code session: the recovery code fixes settings and nothing else. A path matches a
/// route that is not a prefix exactly, and a prefix route also below it (at a `/`), both
/// ignoring case.
///
/// The gate refuses a recovery session on every one of these alike, before any endpoint runs,
/// so no endpoint can forget the check and let the recovery code write into music files; an
/// endpoint that still checks finds nothing to refuse. The words are the ones each endpoint
/// answers, so nothing else changes.
pub const LIBRARY_FILE_ROUTES: &[(&str, bool, &str)] = &[
    // The folder picker.
    ("/api/admin/browse", false, "Browse session required."),
    // Try it on a song (reads a file's tags).
    ("/api/admin/tags/preview", false, SIGN_IN_FIRST),
    // The library actions' history and Allow downloading again.
    ("/api/admin/library-actions", true, SIGN_IN_FIRST),
    ("/api/admin/notices", false, SIGN_IN_FIRST),
    // Better quality: the lossy list and the upgrade queue, which replaces files.
    ("/api/admin/lossy", false, SIGN_IN_FIRST),
    ("/api/admin/upgrades", true, SIGN_IN_FIRST),
    ("/api/admin/library/resolve", false, SIGN_IN_FIRST),
    // Genre normalization's re-tag of the library.
    ("/api/admin/genre/backfill", true, SIGN_IN_FIRST),
    // The cover upgrade (writes covers into folders and files).
    ("/api/admin/covers", true, SIGN_IN_FIRST),
    // The Lyrics page (writes lyrics files beside songs).
    ("/api/admin/lyrics", true, SIGN_IN_FIRST),
    // Imports (lists, the trickle that downloads into the library).
    (
        "/api/admin/imports",
        true,
        "Sign in with your Navidrome account first.",
    ),
    // Downloads: a song's log, a held download's people, Keep anyway and Stop waiting for Lidarr
    // (which land files), Find songs and its picks, the trash and Put back.
    ("/api/admin/downloads/log", false, SIGN_IN_FIRST),
    ("/api/admin/acquisitions/holders", false, SIGN_IN_FIRST),
    ("/api/admin/acquisitions/keep", false, SIGN_IN_FIRST),
    ("/api/admin/acquisitions/stop-waiting", false, SIGN_IN_FIRST),
    // Put Lidarr back (places held copies, deletes Octo's own files and adds from Lidarr).
    ("/api/admin/lidarr/put-back", false, SIGN_IN_FIRST),
    ("/api/admin/find", true, SIGN_IN_FIRST),
    ("/api/admin/trash", true, SIGN_IN_FIRST),
    // Library health and its fixes (tags written in place, songs removed).
    ("/api/admin/health", true, SIGN_IN_FIRST),
    // The explicit marking (writes advisories into files).
    ("/api/admin/explicit", true, SIGN_IN_FIRST),
];

/// What a recovery-code session is answered on a library-file route, or None when the path is
/// not one.
pub fn library_file_refusal(path: &str) -> Option<&'static str> {
    let path = path.trim_end_matches('/');
    LIBRARY_FILE_ROUTES.iter().find_map(|(route, prefix, words)| {
        let head = path.get(..route.len())?;
        if !head.eq_ignore_ascii_case(route) {
            return None;
        }
        let rest = &path[route.len()..];
        (rest.is_empty() || (*prefix && rest.starts_with('/'))).then_some(*words)
    })
}

/// What the gate needs beside the request: the services, and the route templates to name the
/// endpoint a path reaches the way the router will.
#[derive(Clone)]
pub struct GateState {
    pub app: AppState,
    pub canon: Arc<PathCanon>,
}

/// The endpoint a request reaches, spelled as its template, when it reaches one of ours.
fn endpoint_of(canon: &PathCanon, path: &str) -> String {
    canon.canonical(path).unwrap_or_else(|| path.to_string())
}

fn is_open(method: &Method, endpoint: &str) -> bool {
    OPEN_ENDPOINTS
        .iter()
        .any(|(m, template)| m == method && *template == endpoint)
}

fn app_call(method: &Method, endpoint: &str) -> Option<bool> {
    APP_CALL_ENDPOINTS
        .iter()
        .find(|(m, template, _)| m == method && *template == endpoint)
        .map(|(_, _, admin_only)| *admin_only)
}

/// The sign-in token: the cookie first (the dashboard), the header second (curl, tests).
pub fn admin_token(req: &Request) -> Option<String> {
    request_cookie(req.headers(), BROWSE_COOKIE_NAME)
        .filter(|cookie| !cookie.is_empty())
        .or_else(|| header_value(req.headers(), BROWSE_TOKEN_HEADER))
}

/// The query as a dictionary: each name once (matched ignoring case, and spelled as it first
/// came), repeated values joined with commas.
fn query_dictionary(query: Option<&str>) -> IndexMap<String, String> {
    let mut values: IndexMap<String, Vec<String>> = IndexMap::new();
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        let existing = values.keys().find(|k| k.eq_ignore_ascii_case(&key)).cloned();
        values
            .entry(existing.unwrap_or_else(|| key.into_owned()))
            .or_default()
            .push(value.into_owned());
    }
    values
        .into_iter()
        .map(|(key, list)| (key, list.join(",")))
        .collect()
}

/// The gate's 401.
fn sign_in_required() -> Response {
    json_relaxed_response(
        StatusCode::UNAUTHORIZED,
        &json!({
            "error": "Sign in to Octo's dashboard with a Navidrome admin account.",
            "signIn": true,
        }),
        "application/json; charset=utf-8",
    )
    .into_response()
}

pub async fn admin_sign_in_gate(State(gate): State<GateState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if !is_admin_path(&path) {
        return next.run(req).await;
    }
    let state = &gate.app;
    // A Family page sign-in never opens the dashboard, whoever holds it.
    if let Some((user, kind)) = state
        .browse_sessions
        .family_session_of(admin_token(&req).as_deref())
    {
        let manager = kind == SessionKind::FamilyNavidrome && state.family_store.is_owner(&user)
            || state
                .family_store
                .member(&user)
                .is_some_and(|m| m.abilities().manage_family);
        let caller = AdminCaller::new(
            Some(user),
            if manager {
                AdminCallerKind::FamilyManager
            } else {
                AdminCallerKind::FamilyMember
            },
        );
        req.extensions_mut().insert(caller);
        return json_relaxed_response(
            StatusCode::FORBIDDEN,
            &json!({ "error": FAMILY_SESSION_REFUSED }),
            "application/json; charset=utf-8",
        )
        .into_response();
    }
    if state.settings.current().admin.sign_in_off() {
        req.extensions_mut()
            .insert(AdminCaller::new(None, AdminCallerKind::SignInOff));
        return next.run(req).await;
    }

    let endpoint = endpoint_of(&gate.canon, &path);
    let open = is_open(req.method(), &endpoint);
    let app_call = app_call(req.method(), &endpoint);

    if let Some(user) = state.browse_sessions.user_of(admin_token(&req).as_deref()) {
        let recovery = user == BrowseSessionStore::RECOVERY_USER;
        if !recovery && !state.admin_role_check.still_admin(&user).await {
            return if open {
                next.run(req).await
            } else {
                sign_in_required()
            };
        }
        let kind = if recovery {
            AdminCallerKind::Recovery
        } else {
            AdminCallerKind::Dashboard
        };
        // The cookie's age is set again on every signed-in call, so the browser keeps it as long
        // as the server keeps the session. Appended ahead of whatever the endpoint sets.
        let renew = request_cookie(req.headers(), BROWSE_COOKIE_NAME)
            .filter(|cookie| !cookie.is_empty())
            .map(|cookie| {
                let https = is_https(req.extensions().get::<RequestScheme>());
                let age = if recovery {
                    BrowseSessionStore::RECOVERY_TTL
                } else {
                    BrowseSessionStore::TTL
                };
                browse_cookie_for(&cookie, https, age)
            });
        if recovery && let Some(words) = library_file_refusal(&path) {
            let mut res = json_relaxed_response(
                StatusCode::UNAUTHORIZED,
                &json!({ "error": words }),
                "application/json; charset=utf-8",
            )
            .into_response();
            if let Some(cookie) = renew {
                prepend_set_cookie(&mut res, cookie);
            }
            return res;
        }
        req.extensions_mut().insert(AdminCaller::new(Some(user), kind));
        let mut res = next.run(req).await;
        if let Some(cookie) = renew {
            prepend_set_cookie(&mut res, cookie);
        }
        return res;
    }

    if let Some(admin_only) = app_call {
        let parameters = query_dictionary(req.uri().query());
        let try_key = key_of(
            req.extensions()
                .get::<ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0),
        );
        let credential = SubsonicCredential::from(&parameters);
        if let Some(credential) = credential
            && state.sign_in_throttle.retry_after(&try_key).is_none()
        {
            let relay = state.subsonic_proxy.with_request(Arc::new(IncomingRequest::new(
                req.method().clone(),
                req.headers().clone(),
                req.uri().query().map(str::to_string),
                Bytes::new(),
            )));
            // A URL the client cannot use at all counts as Navidrome not answering, so the
            // request is simply not signed in.
            let verdict = state
                .credential_check
                .check(Some(&credential), &relay)
                .await
                .unwrap_or(CredentialVerdict::Unreachable);
            if verdict == CredentialVerdict::Refused {
                state.sign_in_throttle.failed(&try_key);
            }
            if verdict == CredentialVerdict::Accepted
                && let Ok(Some(name)) = state.request_identity.username(&parameters, &relay).await
            {
                if navidrome_admin_role::is_admin(&credential, &name, &relay).await {
                    req.extensions_mut()
                        .insert(AdminCaller::new(Some(name), AdminCallerKind::NavidromeAdmin));
                    return next.run(req).await;
                }
                if !admin_only {
                    req.extensions_mut()
                        .insert(AdminCaller::new(Some(name), AdminCallerKind::Listener));
                    return next.run(req).await;
                }
                if !open {
                    return json_relaxed_response(
                        StatusCode::FORBIDDEN,
                        &json!({ "error": "That needs a Navidrome admin account." }),
                        "application/json; charset=utf-8",
                    )
                    .into_response();
                }
            }
        }
    }

    if open {
        return next.run(req).await;
    }
    sign_in_required()
}

/// Puts a `Set-Cookie` ahead of the ones the endpoint wrote, since the gate's renewal comes
/// before anything the endpoint sets.
///
/// When the endpoint deleted that same cookie (a sign-out), the renewal is left out and the
/// answer carries the deletion alone: a browser handed both might keep the renewed one.
fn prepend_set_cookie(res: &mut Response, cookie: HeaderValue) {
    let existing: Vec<HeaderValue> = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .cloned()
        .collect();
    let name = cookie
        .to_str()
        .ok()
        .and_then(|c| c.split_once('='))
        .map(|(name, _)| format!("{name}="))
        .unwrap_or_default();
    let deleted = existing
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.starts_with(&name) && v.to_ascii_lowercase().contains("expires=thu, 01 jan 1970"));
    if name.is_empty() || deleted {
        return;
    }
    res.headers_mut().remove(header::SET_COOKIE);
    res.headers_mut().append(header::SET_COOKIE, cookie);
    for value in existing {
        res.headers_mut().append(header::SET_COOKIE, value);
    }
}

/// The caller the gate let in, for an endpoint that answers differently.
pub type Caller = Option<Extension<AdminCaller>>;

/// The caller the gate let in, from the extractor.
pub fn caller_of(caller: &Caller) -> Option<&AdminCaller> {
    caller.as_ref().map(|Extension(caller)| caller)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_is_read_as_a_dictionary() {
        let parameters = query_dictionary(Some("u=admin&p=pw&U=again&c=octo&c=two&empty="));
        assert_eq!(parameters.get("u").map(String::as_str), Some("admin,again"));
        assert_eq!(parameters.get("c").map(String::as_str), Some("octo,two"));
        assert_eq!(parameters.get("empty").map(String::as_str), Some(""));
        assert!(query_dictionary(None).is_empty());
    }

    #[test]
    fn library_file_routes_match_exactly_or_below_a_prefix() {
        let cases: [(&str, Option<&str>); 17] = [
            ("/api/admin/browse", Some("Browse session required.")),
            ("/api/admin/browse/session", None),
            ("/api/admin/browse/signout", None),
            ("/API/Admin/Explicit/Backfill", Some(SIGN_IN_FIRST)),
            ("/api/admin/explicit/backfill/cancel", Some(SIGN_IN_FIRST)),
            (
                "/api/admin/imports/lists/x",
                Some("Sign in with your Navidrome account first."),
            ),
            ("/api/admin/downloads", None),
            ("/api/admin/downloads/log", Some(SIGN_IN_FIRST)),
            ("/api/admin/acquisitions", None),
            ("/api/admin/acquisitions/keep", Some(SIGN_IN_FIRST)),
            ("/api/admin/acquisitions/stop-waiting", Some(SIGN_IN_FIRST)),
            ("/api/admin/lidarr/put-back", Some(SIGN_IN_FIRST)),
            ("/api/admin/lidarr/test", None),
            ("/api/admin/genre/presets", None),
            ("/api/admin/genre/backfill/undo", Some(SIGN_IN_FIRST)),
            ("/api/admin/lyricsx", None),
            ("/api/admin/settings", None),
        ];
        for (path, expected) in cases {
            assert_eq!(library_file_refusal(path), expected, "{path}");
        }
    }

    #[test]
    fn only_the_listed_endpoints_are_open_or_app_calls() {
        assert!(is_open(&Method::GET, "/api/admin/status"));
        assert!(!is_open(&Method::HEAD, "/api/admin/status"));
        assert!(!is_open(&Method::POST, "/api/admin/status"));
        assert!(!is_open(&Method::GET, "/api/admin/browse/auth"));
        assert_eq!(app_call(&Method::GET, "/api/admin/lastfm/radio"), Some(false));
        assert_eq!(app_call(&Method::GET, "/api/admin/downloads"), Some(true));
        assert_eq!(app_call(&Method::GET, "/api/admin/settings"), None);
    }
}
