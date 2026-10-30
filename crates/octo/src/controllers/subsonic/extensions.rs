//! Octo's own Subsonic extensions and the extension list: acquisitions, the OpenSubsonic
//! extensions, library actions and edits, the trash, upgrades, and jukebox control.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use indexmap::IndexMap;
use octo_core::common::dotnet::{self, is_blank};
use octo_core::settings::LibraryAction;
use octo_subsonic::subsonic_response_builder::{
    CHECK_VERSION_ACTION, COVER_ACTION, COVER_SWAP_ACTION, EDIT_ACTIONS, JOIN_ALBUM_ACTION, LOOKUP_ACTION,
    LYRICS_ACTION, REMOVE_ACTION, RETAG_ACTION, UNDO_ACTION, UPGRADE_ACTION,
};
use serde_json::Value;
use tracing::info;

use super::caller_checks::{
    SubsonicCall, check_caller, is_successful_subsonic_response, listener_name, signed_in_user,
};
use crate::app::AppState;
use crate::http::error::{AppError, AppResult};
use crate::http::routes::RouteSet;
use crate::services::common::song_finder::UpgradeGate;
use crate::services::family::{audio_gate, family_caller};
use crate::services::library::library_tag_edits::song_tag_fields;
use crate::services::library::{
    LibraryActionOutcome, LibraryActionRequest, LibraryActionState, LibraryEditOutcome, SongLookup,
    UpgradeAsk,
};
use crate::services::subsonic::SubsonicResponseBuilderExt;

/// The state of an upgrade job that is running.
const UPGRADE_WORKING: &str = "working";

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("getAcquisitions", get(get_acquisitions).post(get_acquisitions))
        .subsonic(
            "getOpenSubsonicExtensions",
            get(get_open_subsonic_extensions).post(get_open_subsonic_extensions),
        )
        .subsonic(
            "getLibraryActions",
            get(get_library_actions).post(get_library_actions),
        )
        .subsonic("getLibraryTrash", get(get_library_trash).post(get_library_trash))
        .subsonic("getUpgrades", get(get_upgrades).post(get_upgrades))
        .subsonic(
            "libraryAction",
            get(apply_library_action).post(apply_library_action),
        )
        .subsonic("jukeboxControl", get(jukebox_control).post(jukebox_control))
}

/// The caller's own hearted downloads, while they run and for half an hour after, so the
/// app can draw progress on the button that started one. Octo answers this itself, so it
/// works wherever the Subsonic API does, including away from home, unlike the admin API.
///
/// Credentials are checked the way a station or a mix checks them: a ping to Navidrome with
/// the caller's own. Only rows that user asked for come back, admin or not.
pub async fn get_acquisitions(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    const FORMAT: &str = "json";
    let builder = &state.subsonic_response_builder;

    let mut auth = call.parameters.clone();
    auth.insert("f".into(), FORMAT.into());
    let Some(check) = call.proxy.relay_safe("rest/ping", &auth).await else {
        return Ok(builder
            .create_error(FORMAT, 0, "Octo can't reach Navidrome to check who is asking")
            .into_response());
    };
    if !is_successful_subsonic_response(&check.body, FORMAT) {
        return Ok(builder
            .create_error(FORMAT, 40, "Wrong username or password")
            .into_response());
    }

    let username = signed_in_user(&state, &call).await?;
    let rows = if is_blank(&username) {
        Vec::new()
    } else {
        // Keep anyway is offered only to someone who may keep the row, read from the holds.
        let marks = state.heart_acquisition_coordinator.keep_marks(&username);
        let mut rows = state.acquisition_tracker.for_user(&username);
        for row in &mut rows {
            marks.mark(row);
        }
        rows
    };
    Ok(builder.create_acquisitions_response(&rows).into_response())
}

/// Navidrome's extension list with octoAcquisitions added, so a client can tell this server
/// answers getAcquisitions before it asks, octoLibraryActions while library actions are on,
/// octoTransitions while octo-sonic makes transition profiles, octoStations while radio
/// stations are served as playlists, and octoPlaces while the place data can be read.
/// Relayed, then merged; no credentials are needed, as the OpenSubsonic spec has it.
///
/// A caller whose audio has a ceiling (a family limit, or their own quality for where they
/// are) is not offered `transcoding`: its decisions would let a client play a file as it is.
pub async fn get_open_subsonic_extensions(State(state): State<AppState>, req: Request) -> Response {
    let capped = family_caller(req.extensions()).is_some_and(|caller| audio_gate::ceiling(caller).is_some());
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format();
    let relay = call
        .proxy
        .relay_safe("rest/getOpenSubsonicExtensions", &call.parameters)
        .await;
    let settings = state.settings.current();
    let mut reply = state
        .subsonic_response_builder
        .merge_open_subsonic_extensions_with(
            &format,
            relay.as_ref().map(|r| r.body.as_ref()),
            relay.as_ref().and_then(|r| r.content_type.as_deref()),
            settings.metadata.fetch_lyrics,
            settings.library_actions.enabled,
            settings.subsonic.enable_search_discovery,
            state.song_radio.enabled(),
            super::transitions::offered(&state),
            super::stations::offered(&state),
            places_available(&state).await,
            state.animated_art.offered(),
        );
    if capped && let Some(body) = audio_gate::without_extension(&reply.body, format == "json", "transcoding")
    {
        reply.body = body;
    }
    // octoFamily is listed only while Family is on.
    if settings.family.enabled {
        return super::family::with_family_extension(reply, &format).into_response();
    }
    reply.into_response()
}

/// Whether the place data can be read, so getPlace can find anything.
async fn places_available(state: &AppState) -> bool {
    let places = std::sync::Arc::clone(&state.places);
    tokio::task::spawn_blocking(move || places.available())
        .await
        .unwrap_or(false)
}

/// Whether a source Better quality searches (Soulseek, Lidarr) is set up here at all.
fn upgrade_ready(state: &AppState) -> bool {
    state.upgrade_sources.ready()
}

/// Where Better quality looks, in words: "Soulseek", "Lidarr", or "Soulseek or Lidarr".
fn upgrade_source_name(state: &AppState) -> String {
    state.upgrade_sources.name()
}

/// octoLibraryActions v1: what the caller may do to library files from the app, read from the
/// settings as they are now. Always JSON. Credentials are checked with a ping to Navidrome, as
/// getAcquisitions checks them.
pub async fn get_library_actions(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    let name = listener_name(&state, &call).await;
    let admin = is_caller_admin(&call, &name).await;
    state
        .subsonic_response_builder
        .create_library_actions_response(
            &state.settings.current().library_actions,
            named(&name),
            state.download_concurrency.current(),
            upgrade_ready(&state),
            &upgrade_source_name(&state),
            admin,
            // Library edits are always available.
            true,
            state.library_edits.has_cover_finder(),
        )
        .into_response()
}

/// octoLibraryActions v3: the songs the server holds in its trash, newest first, for an app to
/// offer putting one back. Always JSON; credentials checked like getLibraryActions, and listed
/// only to someone who could put them back.
pub async fn get_library_trash(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    let name = listener_name(&state, &call).await;
    let admin = is_caller_admin(&call, &name).await;
    let allowed = state.library_edits.refusal(named(&name), admin).is_none();
    let songs = if allowed {
        state.library_edits.trash()
    } else {
        Vec::new()
    };
    state
        .subsonic_response_builder
        .create_library_trash_response(
            &songs,
            state
                .settings
                .current()
                .library_actions
                .effective_quarantine_retention_days(),
        )
        .into_response()
}

/// octoLibraryActions v2: the caller's upgrade jobs and how each is going, read by the apps while
/// an upgrade they asked for is running. Always JSON; credentials checked like getLibraryActions.
pub async fn get_upgrades(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    let name = listener_name(&state, &call).await;
    let jobs = match named(&name) {
        Some(username) => state.upgrade_queue.snapshot_for(Some(username)),
        None => Vec::new(),
    };
    let mut progress: HashMap<String, Option<f64>> = HashMap::new();
    for row in state.acquisition_tracker.all() {
        progress
            .entry(format!("{}:{}", row.provider, row.external_id))
            .or_insert(row.progress);
    }
    let rows: Vec<_> = jobs
        .into_iter()
        .map(|job| {
            let live = match &job.acquisition_key {
                Some(key) if job.state == UPGRADE_WORKING => progress.get(key).copied().flatten(),
                _ => None,
            };
            (job, live)
        })
        .collect();
    state
        .subsonic_response_builder
        .create_upgrades_response(&rows)
        .into_response()
}

/// octoLibraryActions v1: remove one song, exactly as putting it in the Delete action playlist
/// does. The executor applies every gate: the master switch, the allowlist, Delete being on,
/// the dry run, and a file it can prove is this song, which goes to quarantine.
///
/// Never through a rating: where rating actions are on, one star can remove a song, and this
/// is how an app removes one without giving it a rating.
pub async fn apply_library_action(State(state): State<AppState>, req: Request) -> AppResult {
    let caller = crate::services::family::family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    const FORMAT: &str = "json";
    let builder = &state.subsonic_response_builder;
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }

    let id = call.param("id").trim().to_string();
    let action = call.param("action").trim().to_string();
    if id.is_empty() || action.is_empty() {
        return Ok(builder
            .create_error(FORMAT, 10, "Required parameter is missing: id and action")
            .into_response());
    }
    // A family member's library actions: remove takes a song out of their own library only,
    // and a better copy of a shared song is asked for.
    if let Some(member) = caller.filter(|c| c.managed) {
        return Ok(member_library_action(&state, &member.user, &id, &action).await);
    }
    if dotnet::eq_ignore_case(&action, UPGRADE_ACTION) {
        let name = listener_name(&state, &call).await;
        return Ok(queue_upgrade(&state, &id, named(&name)));
    }
    if let Some(edit) = EDIT_ACTIONS
        .iter()
        .find(|edit| dotnet::eq_ignore_case(edit, &action))
    {
        return Ok(apply_library_edit(&state, &call, &id, edit).await);
    }
    if let Some((answer, name)) = [
        (LibraryAction::WrongLyrics, LYRICS_ACTION),
        (LibraryAction::WrongCover, COVER_SWAP_ACTION),
        (LibraryAction::CheckVersion, CHECK_VERSION_ACTION),
    ]
    .into_iter()
    .find(|(_, name)| dotnet::eq_ignore_case(name, &action))
    {
        return apply_song_answer(&state, &call, &id, answer, name).await;
    }
    if !dotnet::eq_ignore_case(&action, REMOVE_ACTION) {
        return Ok(builder
            .create_error(
                FORMAT,
                0,
                &format!(
                    "Unknown action \"{action}\"; this server knows remove, upgrade, {LYRICS_ACTION}, {COVER_SWAP_ACTION}, {CHECK_VERSION_ACTION}, {}",
                    EDIT_ACTIONS.join(", ")
                ),
            )
            .into_response());
    }

    // The listener the accepted credential belongs to: the sign-in name, or its API key's owner.
    // When nobody can be named the executor is not asked at all.
    let name = listener_name(&state, &call).await;
    let Some(username) = named(&name).map(str::to_string) else {
        return Ok(builder
            .create_library_action_outcome_response(
                &id,
                &LibraryActionOutcome::new(
                    LibraryActionState::Skipped,
                    Some(
                        "Sign in with a username to remove songs; an API key alone does not say who is asking."
                            .into(),
                    ),
                ),
                REMOVE_ACTION,
            )
            .into_response());
    };

    // Taking a file off the disk is for the server's admins, whatever the allowed list says.
    if !is_caller_admin(&call, &username).await {
        return Ok(builder
            .create_library_action_outcome_response(
                &id,
                &LibraryActionOutcome::new(
                    LibraryActionState::Skipped,
                    Some("Only a server admin can remove songs from the library.".into()),
                ),
                REMOVE_ACTION,
            )
            .into_response());
    }

    // Not tied to the request: a client that hangs up must not stop a move halfway.
    // copy=true: Library health removing a second copy, so the song itself is still wanted.
    let only_a_copy = matches!(call.param("copy"), "true" | "1");
    let executor = Arc::clone(&state.library_action_executor);
    let request = LibraryActionRequest::new(LibraryAction::Delete, id.clone(), username.clone())
        .only_a_copy(only_a_copy);
    let outcome = tokio::spawn(async move { executor.apply(request).await })
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
        .map_err(AppError::Internal)?;
    info!(
        "Library action Delete for {id} by {username} from the app: {} - {}",
        outcome.state.name(),
        outcome.detail.as_deref().unwrap_or("")
    );
    // Navidrome drops the song on its next scan; one is asked for now rather than whenever.
    if outcome.state == LibraryActionState::Applied {
        state.library_rescan.soon();
    }
    Ok(builder
        .create_library_action_outcome_response(&id, &outcome, REMOVE_ACTION)
        .into_response())
}

/// Wrong lyrics or Wrong cover for one song, exactly as its action playlist or star does. The
/// executor applies every gate: the master switch, the allowlist, the action being on and the
/// dry run. Neither touches the song's audio.
async fn apply_song_answer(
    state: &AppState,
    call: &SubsonicCall,
    id: &str,
    answer: LibraryAction,
    name: &str,
) -> AppResult {
    let builder = &state.subsonic_response_builder;
    let listener = listener_name(state, call).await;
    let Some(username) = named(&listener).map(str::to_string) else {
        return Ok(builder
            .create_library_action_outcome_response(
                id,
                &LibraryActionOutcome::new(
                    LibraryActionState::Skipped,
                    Some(
                        "Sign in with a username for this; an API key alone does not say who is asking."
                            .into(),
                    ),
                ),
                name,
            )
            .into_response());
    };
    // Not tied to the request: a client that hangs up must not stop a write halfway.
    let executor = Arc::clone(&state.library_action_executor);
    let request = LibraryActionRequest::new(answer, id.to_string(), username.clone());
    let outcome = tokio::spawn(async move { executor.apply(request).await })
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
        .map_err(AppError::Internal)?;
    info!(
        "Library action {answer:?} for {id} by {username} from the app: {} - {}",
        outcome.state.name(),
        outcome.detail.as_deref().unwrap_or("")
    );
    Ok(builder
        .create_library_action_outcome_response(id, &outcome, name)
        .into_response())
}

/// `libraryAction` for a family member. `remove` takes a song out of their own library (a
/// shared song answers error 50); `upgrade` asks for a better copy of a shared song, unless
/// their account only saves songs; every other action is for the server owner.
async fn member_library_action(state: &AppState, user: &str, id: &str, action: &str) -> Response {
    const FORMAT: &str = "json";
    let builder = &state.subsonic_response_builder;
    if dotnet::eq_ignore_case(action, REMOVE_ACTION) {
        return match state.personal_library.remove_from_my_library(user, id).await {
            Ok(()) => builder
                .create_library_action_outcome_response(
                    id,
                    &LibraryActionOutcome::new(
                        LibraryActionState::Applied,
                        Some("Removed from your library.".into()),
                    ),
                    REMOVE_ACTION,
                )
                .into_response(),
            Err(e) => builder.create_error(FORMAT, e.code, &e.message).into_response(),
        };
    }
    if dotnet::eq_ignore_case(action, UPGRADE_ACTION) {
        if let crate::services::family::WriteDecision::Refuse { message, .. } =
            state.family_write_gate.decide(Some(user))
        {
            return builder.create_error(FORMAT, 50, &message).into_response();
        }
        return match state
            .family_request_service
            .create(
                user,
                id,
                octo_core::family::abilities::RequestQuality::Best,
                Some(octo_core::family::request::RequestKind::Upgrade),
            )
            .await
        {
            Ok(request) => builder
                .create_library_action_outcome_response(
                    id,
                    &LibraryActionOutcome::new(
                        LibraryActionState::Pending,
                        Some(format!("Asked for a better copy ({}).", request.id)),
                    ),
                    UPGRADE_ACTION,
                )
                .into_response(),
            Err(e) => builder.create_error(FORMAT, e.code, &e.message).into_response(),
        };
    }
    builder
        .create_error(FORMAT, 50, "Only the server owner changes library files.")
        .into_response()
}

/// octoLibraryActions v3: change a song's file without removing it, or put a removed one back.
/// retag writes the tags sent (title, artist, album, albumArtist, year, genre, track, disc,
/// isrc; an empty one clears it), joinAlbum puts the song on the album of the song named by
/// `like`, cover puts a found cover inside a file that has none (`preview=true` only says what
/// was found), lookup finds the tags a download would get and writes nothing, undo puts back the
/// song's last change, and restore takes a removed song out of the trash. All of them only for an
/// admin on the allowed list; a dry run rehearses.
async fn apply_library_edit(state: &AppState, call: &SubsonicCall, id: &str, action: &str) -> Response {
    let builder = &state.subsonic_response_builder;
    let name = listener_name(state, call).await;
    let username = named(&name).map(str::to_string);
    let edits = Arc::clone(&state.library_edits);
    let admin = is_caller_admin(call, &name).await;
    if let Some(refusal) = edits.refusal(username.as_deref(), admin) {
        return if action == LOOKUP_ACTION {
            builder
                .create_lookup_response(id, &SongLookup::said("skipped", refusal))
                .into_response()
        } else {
            builder
                .create_library_edit_response(id, action, &LibraryEditOutcome::new("skipped", refusal))
                .into_response()
        };
    }
    let username = username.unwrap_or_default();

    if action == LOOKUP_ACTION {
        let lookup = edits.lookup(id).await;
        return builder.create_lookup_response(id, &lookup).into_response();
    }
    // Not tied to the request either: a write that has started finishes.
    let (task_id, task_user, task_action) = (id.to_string(), username.clone(), action.to_string());
    let outcome = match action {
        RETAG_ACTION => {
            let changes: IndexMap<String, Option<String>> = song_tag_fields::ALL
                .iter()
                .filter_map(|field| {
                    call.param_opt(field)
                        .map(|value| (field.to_string(), Some(value.to_string())))
                })
                .collect();
            tokio::spawn(async move { edits.retag(&task_id, &task_user, changes).await }).await
        }
        JOIN_ALBUM_ACTION => {
            let like = call.param("like").trim().to_string();
            if like.is_empty() {
                return builder
                    .create_error("json", 10, "Required parameter is missing: like")
                    .into_response();
            }
            tokio::spawn(async move { edits.join_album(&task_id, &like, &task_user).await }).await
        }
        COVER_ACTION => {
            let preview = matches!(call.param("preview"), "true" | "1");
            tokio::spawn(async move { edits.add_cover(&task_id, &task_user, preview).await }).await
        }
        UNDO_ACTION => tokio::spawn(async move { edits.undo(&task_id, &task_user).await }).await,
        _ => tokio::spawn(async move { edits.restore(&task_id, &task_user).await }).await,
    }
    .unwrap_or_else(|e| LibraryEditOutcome::new("failed", format!("The change did not finish ({e}).")));
    info!(
        "Library {task_action} for {id} by {username} from the app: {} - {}",
        outcome.state,
        outcome.detail.as_deref().unwrap_or("")
    );
    builder
        .create_library_edit_response(id, action, &outcome)
        .into_response()
}

/// The listener name when one is known, else None.
fn named(name: &str) -> Option<&str> {
    Some(name).filter(|name| !is_blank(name))
}

/// Whether the caller is one of Navidrome's admins, asked with their own sign-in. False when
/// Navidrome cannot say, so a change to the library's files is never allowed by an outage. Asked
/// every time: a role taken away counts at once, and it is one call to Navidrome.
pub(super) async fn is_caller_admin(call: &SubsonicCall, username: &str) -> bool {
    if is_blank(username) {
        return false;
    }
    let mut auth = call.parameters.clone();
    auth.insert("f".into(), "json".into());
    auth.insert("username".into(), username.to_string());
    let Some(relay) = call.proxy.relay_safe("rest/getUser", &auth).await else {
        return false;
    };
    let Ok(document) = serde_json::from_slice::<Value>(&relay.body) else {
        return false;
    };
    document
        .get("subsonic-response")
        .and_then(|root| root.get("user"))
        .and_then(|user| user.get("adminRole"))
        == Some(&Value::Bool(true))
}

/// octoLibraryActions v2: queue a higher quality copy of one song, and answer at once. The same
/// gates as the Better quality playlist, read now so the app hears a reason straight away; the
/// executor checks them all again when the job runs. A job is listed in getUpgrades before this
/// answers "queued".
fn queue_upgrade(state: &AppState, id: &str, username: Option<&str>) -> Response {
    let builder = &state.subsonic_response_builder;
    let settings = state.settings.current().library_actions.clone();
    let username = username.filter(|u| !is_blank(u));
    let refusal = UpgradeGate::refusal(
        Some(&settings),
        username,
        true,
        upgrade_ready(state),
        &upgrade_source_name(state),
    );
    if let Some(refusal) = refusal {
        return builder
            .create_library_action_response(id, "skipped", Some(&refusal), UPGRADE_ACTION)
            .into_response();
    }
    let username = username.unwrap_or_default();

    let (jobs, full) = state.upgrade_queue.add(
        vec![UpgradeAsk {
            navidrome_id: id.to_string(),
            ..Default::default()
        }],
        username,
        "app",
    );
    let Some(job) = jobs.first() else {
        return builder
            .create_library_action_response(id, "skipped", full.as_deref(), UPGRADE_ACTION)
            .into_response();
    };
    info!(
        "Higher quality for {id} asked by {username} from the app: {}",
        job.state
    );
    builder
        .create_library_action_response(
            id,
            "queued",
            Some(&format!(
                "Looking for a higher quality copy on {}.",
                upgrade_source_name(state)
            )),
            UPGRADE_ACTION,
        )
        .into_response()
}

/// Octo has no jukebox device. Relaying surfaced a misleading "Error connecting to Subsonic
/// server"; return a clean, plain "not supported" so the client just disables jukebox mode
/// instead of logging a scary error.
pub async fn jukebox_control(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    state
        .subsonic_response_builder
        .create_error(&call.format(), 0, "Jukebox is not supported")
        .into_response()
}
