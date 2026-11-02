//! The Last.fm, ListenBrainz and radio actions: the radio's state, the ListenBrainz token check,
//! the scrobbling Connect / Finish / Disconnect flow, the Last.fm credentials check, and the
//! radio refresh and reset.

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use octo_core::common::dotnet::is_blank;
use octo_core::last_fm::last_fm_radio_recommendation_service::carried_note;
use octo_core::last_fm::last_fm_radio_refresh_policy::FAILURE_RETRY_DELAY;
use octo_core::last_fm::last_fm_scrobble_service::LastFmScrobbleException;
use octo_core::settings::SettingsWriteError;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use super::json_body::bind_body;
use super::json_body::{FromBody, Kind, Member, Shape, text_record};
use super::plumbing::{SECRET_PLACEHOLDER, accepted, error_json, ok, query_value};
use crate::app::AppState;
use crate::http::error::validation_problem;
use crate::middleware::admin_sign_in_gate::{Caller, caller_of};
use crate::services::admin::AdminCallerKind;
use crate::services::last_fm::last_fm_scrobble_service::{LastFmConnectError, LastFmScrobbleService};

/// `GET /api/admin/lastfm/radio`: the radio's switches, its listeners, and the selected
/// listener's learning progress and stations (the first known listener when none is asked for).
/// The Octo app with a listener's own sign-in sees that listener and nobody else.
pub async fn get_last_fm_radio(
    State(state): State<AppState>,
    caller: Caller,
    RawQuery(query): RawQuery,
) -> Response {
    let radio = &state.last_fm_radio_state;
    let listener = caller_of(&caller)
        .filter(|caller| caller.kind == AdminCallerKind::Listener)
        .and_then(|caller| caller.user.clone());
    let user = listener.clone().or_else(|| query_value(query.as_deref(), "user"));
    let summaries: Vec<_> = radio
        .get_summaries()
        .into_iter()
        .filter(|summary| {
            listener
                .as_deref()
                .is_none_or(|listener| octo_core::common::dotnet::eq_ignore_case(&summary.username, listener))
        })
        .collect();
    let selected = match user.as_deref() {
        Some(user) if !is_blank(user) => Some(user.trim().to_string()),
        _ => summaries.first().map(|summary| summary.username.clone()),
    };
    let user_state = selected.as_deref().map(|user| radio.get_user(user));
    let settings = state.settings.current().last_fm.clone();

    let learning = user_state.as_ref().map(|user| {
        let learned = user.plays.iter().filter(|play| play.learned_signal).count() as i32;
        let source = if learned > 0 {
            "completed scrobbles and accessible stars"
        } else if !user.plays.is_empty() {
            "accessible random Starter seeds"
        } else {
            "waiting for completed scrobbles"
        };
        // Stations the last refresh kept as they were, while something will try them again.
        let trying = settings.enable_radio
            && (settings.enable_personalized_stations || settings.enable_discovery_stations);
        let carried: Vec<&str> = user
            .stations
            .iter()
            .filter(|station| {
                user.carried_station_keys
                    .iter()
                    .any(|key| octo_core::common::dotnet::eq_ignore_case(key, &station.key))
            })
            .map(|station| station.name.as_str())
            .collect();
        let note = if trying { carried_note(&carried) } else { None };
        json!({
            "plays": learned,
            "needed": (settings.effective_minimum_plays() - learned).max(0),
            "source": source,
            "refreshing": user.refreshing,
            "lastRefreshAttemptUtc": user.last_refresh_attempt_utc.as_ref().map(octo_core::json::datetime::format_utc),
            "lastRefreshSuccessUtc": user.last_refresh_success_utc.as_ref().map(octo_core::json::datetime::format_utc),
            // The apps show this line as it is, so a refresh that kept stations says so here too.
            "lastRefreshError": user.last_refresh_error.clone().or_else(|| note.clone()),
            "carriedNote": note,
            "carriedRetryMinutes": FAILURE_RETRY_DELAY.num_minutes(),
        })
    });
    let stations: Vec<serde_json::Value> = user_state
        .as_ref()
        .map(|user| {
            user.stations
                .iter()
                .map(|station| {
                    json!({
                        "id": station.id,
                        "name": station.name,
                        "kind": format!("{:?}", station.kind),
                        "personalized": station.personalized,
                        "trackCount": station.tracks.len(),
                        "seeds": station.seeds,
                        "createdUtc": octo_core::json::datetime::format_utc(&station.created_utc),
                        "changedUtc": octo_core::json::datetime::format_utc(&station.changed_utc),
                        "validUntilUtc": octo_core::json::datetime::format_utc(&station.valid_until_utc),
                        "preview": station.tracks.iter().take(5)
                            .map(|track| json!({ "artist": track.artist, "title": track.title }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    ok(&json!({
        "enabled": settings.enable_radio,
        "hasApiKey": !is_blank(&settings.api_key),
        "personalizedEnabled": settings.enable_personalized_stations,
        "discoveryEnabled": settings.enable_discovery_stations,
        "playlistsEnabled": settings.expose_radio_as_playlists,
        "streamsEnabled": settings.expose_radio_as_streams,
        "streamBitrateKbps": settings.effective_radio_stream_bitrate_kbps(),
        "icyMetadataEnabled": settings.enable_icy_metadata,
        "minimumPlays": settings.effective_minimum_plays(),
        "selectedUser": selected,
        "users": summaries,
        "learning": learning,
        "stations": stations,
    }))
}

/// Checks the ListenBrainz token that applies to a listener (or the default) against
/// ListenBrainz, so a mistyped token shows up before a play is lost.
async fn validate_listen_brainz(state: &AppState, user: Option<&str>, token: Option<&str>) -> Response {
    // The page only ever holds the placeholder for a saved token; checking it means the saved one.
    let candidate = match token {
        Some(token) if !is_blank(token) && token != SECRET_PLACEHOLDER => token.to_string(),
        _ => {
            let user = user.unwrap_or("");
            state
                .settings
                .current()
                .listen_brainz
                .token_for_listener(user, state.family_credentials.is_member(user))
                .unwrap_or_default()
        }
    };
    if candidate.is_empty() {
        return ok(&json!({
            "configured": false,
            "valid": false,
            "detail": "No token configured.",
        }));
    }
    let check = state.listen_brainz.validate_token(&candidate).await;
    ok(&json!({
        "configured": true,
        "valid": check.valid,
        "userName": check.user_name,
        "detail": check.detail,
    }))
}

/// `GET /api/admin/listenbrainz/validate`.
pub async fn validate_listen_brainz_get(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Response {
    let user = query_value(query.as_deref(), "user");
    let token = query_value(query.as_deref(), "token");
    validate_listen_brainz(&state, user.as_deref(), token.as_deref()).await
}

#[derive(Debug, Default, Deserialize)]
pub struct ListenBrainzValidateRequest {
    pub user: Option<String>,
    pub token: Option<String>,
}

impl FromBody for ListenBrainzValidateRequest {
    const SHAPE: &'static Shape = text_record!(
        "Octo.Controllers.AdminController+ListenBrainzValidateRequest",
        "user",
        "token"
    );
}

/// `POST /api/admin/listenbrainz/validate`: the same check as the GET, with the token in the
/// body so a typed token never lands in a URL, a proxy log or the browser history. The GET
/// stays for compatibility.
pub async fn validate_listen_brainz_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: ListenBrainzValidateRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    validate_listen_brainz(&state, request.user.as_deref(), request.token.as_deref()).await
}

/// `GET /api/admin/lastfm/scrobble`: who can scrobble outside plays to Last.fm: every
/// Navidrome user Octo knows of, with the Last.fm account each is connected to. Session keys
/// never leave the server.
pub async fn get_last_fm_scrobbling(State(state): State<AppState>) -> Response {
    let settings = state.settings.current();
    let lastfm = &settings.last_fm;
    let mut known: Vec<String> = state
        .last_fm_radio_state
        .get_summaries()
        .into_iter()
        .map(|summary| summary.username)
        .collect();
    known.extend(settings.listen_brainz.user_tokens.keys().cloned());
    ok(&json!({
        "available": true,
        "hasApiKey": !is_blank(&lastfm.api_key),
        "hasApiSecret": !is_blank(&lastfm.api_secret),
        "enabled": lastfm.scrobble_external_plays,
        "libraryPlays": lastfm.scrobble_library_plays,
        "users": state.last_fm_scrobbles.users(known),
    }))
}

/// `LastFmScrobbleUserRequest`: `{ user }`, an empty string when left out.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LastFmScrobbleUserRequest {
    pub user: Option<String>,
}

impl FromBody for LastFmScrobbleUserRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.AdminController+LastFmScrobbleUserRequest",
        by_constructor: false,
        members: &[Member {
            name: "user",
            kind: Kind::Text,
        }],
    };
}

impl LastFmScrobbleUserRequest {
    fn user(&self) -> &str {
        self.user.as_deref().unwrap_or("")
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LastFmCredentialsRequest {
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
}

impl FromBody for LastFmCredentialsRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.AdminController+LastFmCredentialsRequest",
        by_constructor: false,
        members: &[
            Member {
                name: "apiKey",
                kind: Kind::Text,
            },
            Member {
                name: "apiSecret",
                kind: Kind::Text,
            },
        ],
    };
}

/// `POST /api/admin/lastfm/check`: checks an API key and shared secret with Last.fm before
/// Save writes them. A blank or placeholder value means the saved one, so the page can check a
/// secret it never sees.
pub async fn check_last_fm_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: LastFmCredentialsRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    let typed = |value: &Option<String>| {
        value
            .as_deref()
            .filter(|v| *v != SECRET_PLACEHOLDER)
            .map(str::to_string)
    };
    let check = state
        .last_fm_scrobbles
        .check_credentials(
            typed(&request.api_key).as_deref(),
            typed(&request.api_secret).as_deref(),
        )
        .await;
    ok(&json!({ "key": check.key, "secret": check.secret, "message": check.message }))
}

/// `POST /api/admin/lastfm/scrobble/cancel`: stops waiting on a Connect nobody is going to
/// approve.
pub async fn cancel_last_fm_connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: LastFmScrobbleUserRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    state.last_fm_scrobbles.cancel_connect(request.user());
    ok(&json!({ "ok": true }))
}

/// `POST /api/admin/lastfm/scrobble/connect`: step one of connecting, the page on last.fm
/// where the admin, signed in as the listener, approves Octo.
pub async fn connect_last_fm(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request: LastFmScrobbleUserRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    match state.last_fm_scrobbles.begin_connect(request.user()).await {
        Ok(url) => ok(&json!({ "user": request.user().trim(), "url": url })),
        Err(e) => error_json(StatusCode::BAD_REQUEST, &e.message),
    }
}

/// `POST /api/admin/lastfm/scrobble/finish`: step two, once Octo has been approved on last.fm:
/// saves the session. 409 while Last.fm has not seen the approval yet, so the dashboard can
/// say "not yet" and let the admin press Finish again.
pub async fn finish_last_fm(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request: LastFmScrobbleUserRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    match state.last_fm_scrobbles.finish_connect(request.user()).await {
        Ok(session) => ok(&json!({
            "ok": true,
            "user": request.user().trim(),
            "lastFmUser": session.last_fm_user,
        })),
        Err(LastFmConnectError::Refused(e))
            if e.code == LastFmScrobbleService::ERROR_TOKEN_NOT_AUTHORIZED =>
        {
            error_json(StatusCode::CONFLICT, &e.message)
        }
        Err(LastFmConnectError::Refused(e)) => error_json(StatusCode::BAD_REQUEST, &e.message),
        Err(LastFmConnectError::Settings(SettingsWriteError::Corrupt(e))) => error_json(
            StatusCode::CONFLICT,
            &format!(
                "{} Fix it in Raw config, or on disk at {}, then Connect again.",
                e.message,
                e.path.display()
            ),
        ),
        Err(LastFmConnectError::Settings(SettingsWriteError::Io(e))) => {
            // Last.fm has already handed over the session; only saving it failed (the file is
            // locked, read-only, or the disk is full). Nothing was saved, so say so plainly.
            let path = state.settings_writer.file_path().display().to_string();
            warn!("Could not save the Last.fm session to {path}: {e}");
            error_json(
                StatusCode::CONFLICT,
                &format!(
                    "Last.fm approved Octo, but the connection could not be saved to {path} ({e}). Make sure Octo can write that file, then press Finish again, or Connect again if the link has expired."
                ),
            )
        }
    }
}

/// `POST /api/admin/lastfm/scrobble/disconnect`.
pub async fn disconnect_last_fm(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request: LastFmScrobbleUserRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    let user = request.user().trim().to_string();
    let from_file = match state.last_fm_scrobbles.disconnect(&user) {
        Ok(from_file) => from_file,
        Err(LastFmScrobbleException { message, .. }) => return error_json(StatusCode::BAD_REQUEST, &message),
    };
    let from_environment = !from_file && state.settings.current().last_fm.session_for(&user).is_some();
    let message = if from_environment {
        format!(
            "Stopped for now. This session is set in the environment (LASTFM__USERSESSIONS__{user}__SESSIONKEY), so remove it there as well or it comes back after a restart."
        )
    } else {
        "Disconnected. To revoke Octo on Last.fm too, remove it from that account's applications.".to_string()
    };
    ok(&json!({ "ok": true, "user": user, "message": message }))
}

/// `RadioUserRequest`: `{ user, stationId? }`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RadioUserRequest {
    pub user: Option<String>,
    pub station_id: Option<String>,
}

impl FromBody for RadioUserRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.AdminController+RadioUserRequest",
        by_constructor: false,
        members: &[
            Member {
                name: "user",
                kind: Kind::Text,
            },
            Member {
                name: "stationId",
                kind: Kind::Text,
            },
        ],
    };
}

/// `POST /api/admin/lastfm/radio/refresh`.
pub async fn refresh_last_fm_radio(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut request: RadioUserRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    // A listener signed in from the Octo app rebuilds their own stations, whoever the body names.
    if let Some(caller) = caller_of(&caller)
        && caller.kind == AdminCallerKind::Listener
        && let Some(listener) = &caller.user
    {
        request.user = Some(listener.clone());
    }
    let user = request.user.as_deref().unwrap_or("");
    if is_blank(user) {
        return error_json(StatusCode::BAD_REQUEST, "A known Navidrome user is required");
    }
    let queued = state
        .last_fm_radio_refresh_queue
        .enqueue(user, request.station_id.as_deref());
    accepted(&json!({ "ok": true, "queued": queued }))
}

/// `DELETE /api/admin/lastfm/radio/history?user=`: forgets a listener's plays and stations,
/// and the radio source outcomes counted for them. `user` is required: missing or empty, it is the 400 validation problem.
pub async fn reset_last_fm_radio(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let Some(user) = query_value(query.as_deref(), "user") else {
        return validation_problem(&[("user", &["The user field is required."])]);
    };
    if is_blank(&user) {
        return error_json(StatusCode::BAD_REQUEST, "A known Navidrome user is required");
    }
    let radio = &state.last_fm_radio_state;
    let before = radio.get_user(&user);
    let removed = radio.reset(&user);
    state.radio_outcomes.forget_listener(&user);
    ok(&json!({
        "ok": removed,
        "user": user,
        "removedPlays": if removed { before.plays.len() } else { 0 },
        "removedStations": if removed { before.stations.len() } else { 0 },
        "message": "Radio history and generated snapshots were removed. Downloaded music was untouched.",
    }))
}

#[cfg(test)]
mod tests {
    use octo_core::models::radio::{LastFmRadioStation, LastFmRadioTrack};
    use octo_core::settings::{AppSettings, LastFmSettings, SubsonicSettings};

    use super::super::test_app::{app, get};
    use crate::app::AppState;

    fn station(key: &str, name: &str) -> LastFmRadioStation {
        LastFmRadioStation {
            id: key.into(),
            key: key.into(),
            name: name.into(),
            owner: "alice".into(),
            tracks: vec![LastFmRadioTrack {
                artist: "A".into(),
                title: name.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn settings(last_fm: LastFmSettings) -> AppSettings {
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some("http://127.0.0.1:1".into()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            last_fm,
            ..Default::default()
        }
    }

    /// The Radio page and the apps' admin screens say which stations the last refresh kept as
    /// they were, while something will try them again.
    #[tokio::test]
    async fn the_radio_status_names_the_kept_stations_while_they_are_tried_again() {
        let state = AppState::for_tests(settings(LastFmSettings::default()));
        state.last_fm_radio_state.replace_stations_carrying(
            "alice",
            &[
                station("your-mix", "Your Radio"),
                station("discovery", "Discovery Radio"),
            ],
            &["discovery".into()],
        );
        let app = app(state.clone());
        let radio = get(&app, "/api/admin/lastfm/radio?user=alice").await.json();
        let note = "Couldn't update Discovery Radio, so it kept its songs";
        assert_eq!(radio["learning"]["carriedNote"], note);
        assert_eq!(radio["learning"]["lastRefreshError"], note);

        state.settings.set(settings(LastFmSettings {
            enable_radio: false,
            ..Default::default()
        }));
        let radio = get(&app, "/api/admin/lastfm/radio?user=alice").await.json();
        assert!(
            radio["learning"]["carriedNote"].is_null(),
            "radio off tries nothing again"
        );
        assert!(radio["learning"]["lastRefreshError"].is_null());
    }
}
