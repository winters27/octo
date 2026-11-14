//! octoAcquisitions version 4, the apps' downloads drawer. One download's whole log, clearing
//! finished ones, Find songs (a search run again by hand, every copy listed, one picked, and a
//! library song replaced anyway once the person says so), and keeping a held download anyway.
//! Always JSON, and every call is checked with a ping to Navidrome as getAcquisitions is. Each
//! person sees and picks only their own.

use axum::extract::{Request, State};
use axum::response::IntoResponse;
use axum::routing::get;
use octo_core::common::dotnet::{self, is_blank};
use tokio_util::sync::CancellationToken;

use super::action_helpers::int_try_parse;
use super::caller_checks::{SubsonicCall, check_caller, signed_in_user};
use crate::app::AppState;
use crate::http::error::AppResult;
use crate::http::routes::RouteSet;
use crate::services::common::heart_acquisition_coordinator::KeepAnyway;
use crate::services::common::song_finder::{PickOutcome, library_action_states};
use crate::services::family::{WriteDecision, family_caller};
use crate::services::subsonic::SubsonicResponseBuilderExt;
use octo_core::family::abilities::RequestQuality;
use octo_core::family::request::RequestKind;

const FORMAT: &str = "json";

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("getAcquisition", get(get_acquisition).post(get_acquisition))
        .subsonic(
            "keepAcquisitionAnyway",
            get(keep_acquisition_anyway).post(keep_acquisition_anyway),
        )
        .subsonic(
            "clearAcquisitions",
            get(clear_acquisitions).post(clear_acquisitions),
        )
        .subsonic("findSongs", get(find_songs).post(find_songs))
        .subsonic("getFoundSongs", get(get_found_songs).post(get_found_songs))
        .subsonic("pickFoundSong", get(pick_found_song).post(pick_found_song))
}

/// The trimmed value of a parameter, empty when it is missing.
fn trimmed(call: &SubsonicCall, name: &str) -> String {
    call.param_opt(name).unwrap_or_default().trim().to_string()
}

/// One of the caller's downloads with its log, by the key getAcquisitions lists.
pub async fn get_acquisition(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }
    let builder = &state.subsonic_response_builder;
    let key = trimmed(&call, "key");
    if key.is_empty() {
        return Ok(builder
            .create_error(FORMAT, 10, "Required parameter is missing: key")
            .into_response());
    }
    let username = signed_in_user(&state, &call).await?;
    let row = if is_blank(&username) {
        None
    } else {
        state
            .acquisition_tracker
            .detail(&key, Some(&username))
            .map(|mut row| {
                state
                    .heart_acquisition_coordinator
                    .keep_marks(&username)
                    .mark(&mut row);
                row
            })
    };
    Ok(match row {
        Some(row) => builder.create_acquisition_response(&row).into_response(),
        None => builder
            .create_error(FORMAT, 70, "No such download")
            .into_response(),
    })
}

/// What keepAcquisitionAnyway answers someone who may not keep the row.
const KEEP_NOT_YOURS: &str = "Only someone who hearted this can keep it";
/// What it answers the admin who names nobody.
pub const NAME_THE_PERSON: &str = "Name the person whose copy to keep";
/// What it answers when nothing of the row waits to be kept for that person.
pub const NOTHING_WAITING: &str = "Nothing is waiting to be kept";

/// octoAcquisitions version 4: keeps a held download anyway, by the key getAcquisitions lists:
/// Keep anyway on a song's row, Keep these anyway on a hearted album's. It is resolved through the
/// holds of that row, never the row itself: the caller must wait in one, and keeps their own. The
/// admin keeps a holder's by naming them with `user` (error 10 without one). The request is saved
/// before this answers a plain ok, then carried out; error 70 when nothing waits to be kept.
pub async fn keep_acquisition_anyway(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }
    let builder = &state.subsonic_response_builder;
    let key = trimmed(&call, "key");
    if key.is_empty() {
        return Ok(builder
            .create_error(FORMAT, 10, "Required parameter is missing: key")
            .into_response());
    }
    let username = signed_in_user(&state, &call).await?;
    if is_blank(&username) {
        return Ok(builder
            .create_error(FORMAT, 50, "Sign in with a username to keep a download")
            .into_response());
    }
    let named = trimmed(&call, "user");
    let coordinator = &state.heart_acquisition_coordinator;
    let refused = |code: i32, message: &str| builder.create_error(FORMAT, code, message).into_response();
    let kept = || {
        builder
            .create_response(FORMAT, "keepAcquisitionAnyway")
            .into_response()
    };
    // The admin keeps a holder's copy by naming them; anyone else keeps only their own.
    if !named.is_empty() && !dotnet::eq_ignore_case(&named, username.trim()) {
        if !super::extensions::is_caller_admin(&call, &username).await {
            return Ok(refused(50, KEEP_NOT_YOURS));
        }
        return Ok(match coordinator.keep_anyway(&key, &named) {
            KeepAnyway::Keeping => kept(),
            _ => refused(70, NOTHING_WAITING),
        });
    }
    let outcome = coordinator.keep_anyway(&key, &username);
    // The admin is asked to name a person only for a row others hold; with nothing held, nothing
    // waits to be kept.
    if outcome == KeepAnyway::NotAHolder
        && named.is_empty()
        && super::extensions::is_caller_admin(&call, &username).await
    {
        return Ok(refused(10, NAME_THE_PERSON));
    }
    // A row listed for the caller whose holds no longer name them has nothing waiting for them.
    let listed_for_them = || state.acquisition_tracker.detail(&key, Some(&username)).is_some();
    Ok(match outcome {
        KeepAnyway::Keeping => kept(),
        KeepAnyway::NothingWaiting | KeepAnyway::NothingHeld => refused(70, NOTHING_WAITING),
        KeepAnyway::NotAHolder if listed_for_them() => refused(70, NOTHING_WAITING),
        KeepAnyway::NotAHolder => refused(50, KEEP_NOT_YOURS),
    })
}

/// Takes the caller's finished downloads off their list, or the one named by key. A download
/// still running stays.
pub async fn clear_acquisitions(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }
    let key = call.param_opt("key").map(|k| k.trim().to_string());
    let username = signed_in_user(&state, &call).await?;
    let cleared = if is_blank(&username) {
        0
    } else {
        state
            .acquisition_tracker
            .clear(&username, key.as_deref().filter(|k| !k.is_empty()))
    };
    Ok(state
        .subsonic_response_builder
        .create_cleared_response(cleared)
        .into_response())
}

/// Find songs: starts a search for one song (an outside song's id, or a library song's) on the
/// caller's download sources, and answers at once while it runs. getFoundSongs follows it.
pub async fn find_songs(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }
    let builder = &state.subsonic_response_builder;
    let id = trimmed(&call, "id");
    if id.is_empty() {
        return Ok(builder
            .create_error(FORMAT, 10, "Required parameter is missing: id")
            .into_response());
    }
    let username = signed_in_user(&state, &call).await?;
    if is_blank(&username) {
        return Ok(builder
            .create_error(FORMAT, 50, "Sign in with a username to find songs")
            .into_response());
    }
    let found = state
        .song_finder
        .start(&id, &username, &CancellationToken::new())
        .await;
    Ok(match found {
        Some(found) => builder.create_find_songs_response(&found).into_response(),
        None => builder
            .create_error(FORMAT, 70, "No song with this id")
            .into_response(),
    })
}

/// A Find songs search as it stands, by the id findSongs answered.
pub async fn get_found_songs(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }
    let builder = &state.subsonic_response_builder;
    let search = trimmed(&call, "search");
    if search.is_empty() {
        return Ok(builder
            .create_error(FORMAT, 10, "Required parameter is missing: search")
            .into_response());
    }
    let username = signed_in_user(&state, &call).await?;
    let found = if is_blank(&username) {
        None
    } else {
        state.song_finder.get(&search, &username)
    };
    Ok(match found {
        Some(found) => builder.create_find_songs_response(&found).into_response(),
        None => builder
            .create_error(FORMAT, 70, "This search is gone. Search again.")
            .into_response(),
    })
}

/// Fetches exactly one copy a Find songs search listed, by its id there (copy). The copy's place
/// on the list (candidate) still works for older apps, once the search has ended.
///
/// `anyway` (octoAcquisitions version 3): an app that sends it can ask the person. With
/// `anyway=false`, a copy that would replace a library song but is no better than it, or is the
/// plain song in place of another take, answers `confirm` with the words to ask; picking it
/// again with `anyway=true` replaces the song anyway. Without it the pick is refused as before.
pub async fn pick_found_song(State(state): State<AppState>, req: Request) -> AppResult {
    let caller = family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }
    let builder = &state.subsonic_response_builder;
    let search = trimmed(&call, "search");
    let copy = trimmed(&call, "copy");
    let index = call.param_opt("candidate").and_then(int_try_parse);
    let anyway = call
        .param_opt("anyway")
        .map(|anyway| dotnet::eq_ignore_case(anyway.trim(), "true"));
    if search.is_empty() || (copy.is_empty() && index.is_none()) {
        return Ok(builder
            .create_error(FORMAT, 10, "Required parameter is missing: search and copy")
            .into_response());
    }
    let username = signed_in_user(&state, &call).await?;
    if is_blank(&username) {
        return Ok(builder
            .create_error(FORMAT, 50, "Sign in with a username to pick a song")
            .into_response());
    }
    // A family member who must ask first asks for the song instead; one who only saves songs
    // cannot pick one. Members who add songs at once get the copy where their music goes.
    if let Some(member) = caller.filter(|c| c.managed) {
        match state.family_write_gate.decide(Some(&member.user)) {
            WriteDecision::Shared | WriteDecision::Member(_) => {}
            WriteDecision::Request { full, .. } => {
                let outcome = ask_instead(&state, &member.user, &search, full).await;
                return Ok(builder.create_pick_response(&outcome).into_response());
            }
            WriteDecision::Refuse { message, .. } => {
                return Ok(builder
                    .create_pick_response(&PickOutcome::skipped(message))
                    .into_response());
            }
        }
    }
    let outcome = if copy.is_empty() {
        state
            .song_finder
            .pick_place(&search, index.unwrap_or(-1), &username, anyway)
            .await
    } else {
        state
            .song_finder
            .pick_copy(&search, &copy, &username, anyway)
            .await
    };
    Ok(builder.create_pick_response(&outcome).into_response())
}

/// A family member's pick, as a request for the song the look was for: a better copy of a
/// shared song, or a copy of an outside one.
async fn ask_instead(state: &AppState, user: &str, search: &str, full: Option<String>) -> PickOutcome {
    let Some(look) = state.song_finder.look_of(search, user) else {
        return PickOutcome::skipped("This search is gone. Search again.");
    };
    let (target, kind) = match (&look.library_id, &look.external_id) {
        (Some(library_id), _) => (library_id.clone(), RequestKind::Upgrade),
        (None, Some(outside)) => (outside.clone(), RequestKind::Song),
        (None, None) => return PickOutcome::skipped("This song cannot be asked for."),
    };
    match state
        .family_request_service
        .create(user, &target, RequestQuality::Best, Some(kind))
        .await
    {
        Ok(request) => PickOutcome::new(
            library_action_states::QUEUED,
            match full {
                Some(full) => format!("{full}. Asked for a copy instead ({}).", request.id),
                None => format!(
                    "Asked for a copy. You'll hear back once it's decided ({}).",
                    request.id
                ),
            },
        ),
        Err(e) => PickOutcome::skipped(e.message),
    }
}
