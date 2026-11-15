//! getTransitionProfile (octoTransitions): a song's transition profile, which the Octo apps
//! use to plan the blend into and out of it without reading the song themselves. Octo only
//! hands out what octo-sonic made beforehand. A song from outside the library is answered only
//! while its profile was read from the file its play gets now; when that file is another, or
//! changed since, it is queued to be read again.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::caller_checks::{SubsonicCall, listener_name, refuse_unless_signed_in};
use super::stream_versions::kept_file;
use crate::app::AppState;
use crate::http::error::AppResult;
use crate::http::routes::RouteSet;
use crate::services::common::acquisition_tracker::key_of;
use crate::services::family::{FamilyCaller, family_caller};
use crate::services::prefetch::{AheadSidecar, sidecar_path};
use crate::services::sonic::{SonicFile, TransitionEntry, TransitionLookup};
use crate::services::you_tube::stream_versions::copy_is_another_version;

pub fn routes() -> RouteSet {
    RouteSet::new().subsonic(
        "getTransitionProfile",
        get(get_transition_profile).post(get_transition_profile),
    )
}

/// Whether the extension is listed: once octo-sonic has said it makes profiles, which it does
/// whether or not Sounds alike is on.
pub fn offered(state: &AppState) -> bool {
    state.sonic_worker.transitions().current_version().is_some()
}

/// The profile of the song `id` (a library id, or the id of a song from outside the library
/// whose file Octo has fetched), or `ready: false`. Always JSON; the caller must be signed in.
pub async fn get_transition_profile(State(state): State<AppState>, req: Request) -> AppResult {
    let caller = family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = refuse_unless_signed_in(&state, &call, "json").await? {
        return Ok(refused);
    }
    let id = call.param_opt("id").unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Ok(state
            .subsonic_response_builder
            .create_error("json", 10, "Required parameter is missing: id")
            .into_response());
    }
    Ok(answer(&state, &call, caller.as_ref(), &id).await)
}

async fn answer(state: &AppState, call: &SubsonicCall, caller: Option<&FamilyCaller>, id: &str) -> Response {
    let store = state.sonic_worker.transitions();
    let key = key_for(state, id);
    let mut found = match store.lookup(&key) {
        TransitionLookup::Ready(profile) => Some(profile),
        TransitionLookup::NotReady => None,
    };
    if let (true, Some(provider), Some(external_id)) = state.local_library.parse_song_id(id) {
        let user = listener_name(state, call).await;
        let served = served_file(state, caller, &user, &provider, &external_id).await;
        if !read_from(store.entry(&key).as_ref(), served.as_deref()) {
            found = None;
            if let Some(path) = &served {
                state.sonic_worker.arrived_outside(&key, path);
            }
        }
    }
    state
        .subsonic_response_builder
        .create_transition_profile_response(id, found.as_ref(), store.current_version())
        .into_response()
}

/// Whether a profile was read from `served` as the file is now: the same path and stamp.
fn read_from(entry: Option<&TransitionEntry>, served: Option<&str>) -> bool {
    let (Some(entry), Some(served)) = (entry, served) else {
        return false;
    };
    entry.path.as_deref() == Some(served)
        && SonicFile::stat(served)
            .is_some_and(|file| format!("{}:{}", file.length, file.last_write_ticks) == entry.stamp)
}

/// The file an Octo app's play of an outside song gets now for this caller: the copy fetched
/// ahead (while getting songs ahead is on, for a caller whose plays keep copies, and only one
/// that is the version chosen for the song), else the library file kept for them. None when the
/// play streams.
async fn served_file(
    state: &AppState,
    caller: Option<&FamilyCaller>,
    user: &str,
    provider: &str,
    external_id: &str,
) -> Option<String> {
    if state.settings.current().prefetch.enabled
        && state.prefetch.may_keep_copies(user, caller)
        && let Some(path) = state.prefetch.store().ready(provider, external_id)
    {
        let judged = std::fs::read(sidecar_path(&path))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<AheadSidecar>(&bytes).ok())
            .and_then(|sidecar| sidecar.judged_video);
        if !copy_is_another_version(&state.external_id_registry, external_id, judged.as_deref()) {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    kept_file(state, caller, user, provider, external_id).await
}

/// A library song is kept by its id, a song from outside the library by its acquisition key.
fn key_for(state: &AppState, id: &str) -> String {
    match state.local_library.parse_song_id(id) {
        (true, Some(provider), Some(external_id)) => key_of(&provider, &external_id),
        _ => id.to_string(),
    }
}
