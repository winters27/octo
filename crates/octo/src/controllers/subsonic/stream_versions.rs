//! `getStreamVersions`, `setStreamVersion` and `getStreamVersionChanges` (octoStreamVersions,
//! octoStreamVersionChanges): the YouTube videos an outside song can stream from, choosing the one
//! it plays for everyone on the server or going back to Octo's own pick, and the songs whose
//! choice changed after a cursor, so a device drops its saved copies of them. Always JSON; the
//! caller must be signed in.

use std::path::Path;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use octo_core::common::dotnet::{eq_ignore_case, is_blank};
use octo_core::family::abilities::Role;
use octo_core::fingerprint::heart_gate::HeardSource;
use octo_core::soulseek::{RoutingKind, SoulseekRouting};

use super::caller_checks::{SubsonicCall, check_caller, listener_name};
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::family::{FamilyCaller, LibraryScope, family_caller};
use crate::services::library::navidrome_song_list;
use crate::services::soulseek::external_id_registry::{ChooseError, PickState};
use crate::services::soulseek::stream_choices::{StreamChoice, StreamChoices};
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};
use crate::services::subsonic::SubsonicResponseBuilderExt;
use crate::services::you_tube::stream_versions::{
    ListedSong, Picked, PlayingVersion, PlaysFrom, VersionRow, VersionsAnswer, octo_pick_kind, view,
};
use crate::services::you_tube::you_tube_resolver::{Failure, Lookup};

const FORMAT: &str = "json";

/// `video` that goes back to Octo's own pick.
pub const AUTO: &str = "auto";

/// The most changes one getStreamVersionChanges answer lists.
pub const CHANGES_LIMIT: usize = 500;

pub const CANT_CHOOSE: &str = "This song can't be given another version";
pub const PICK_REFUSED: &str = "The version chosen for a song plays for everyone on this server, so this account cannot choose one. Ask a family manager.";
pub const PLAYS_FROM_DOWNLOAD: &str =
    "This server adds every song it plays to the library, so a song's version can't be chosen here.";
pub const VIDEO_REFUSED: &str = "That video can't be played here. Pick another.";
pub const NOT_OFFERED: &str = "That version is not offered for this song any more. Open the list again.";
pub const BEING_ADDED: &str =
    "This song is being added to your library right now. You can choose its version once that finishes.";
pub const NO_OCTO_PICK: &str = "Could not reach YouTube to find Octo's pick. Try again in a moment.";

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic(
            "getStreamVersions",
            get(get_stream_versions).post(get_stream_versions),
        )
        .subsonic(
            "setStreamVersion",
            get(set_stream_version).post(set_stream_version),
        )
        .subsonic(
            "getStreamVersionChanges",
            get(get_stream_version_changes).post(get_stream_version_changes),
        )
}

/// Whether a caller may choose a song's version: everyone but a family's managed Listener and
/// Kid accounts, as for lyrics choices, since a choice plays for the whole family.
pub fn may_pick(caller: Option<&FamilyCaller>) -> bool {
    !caller.is_some_and(|caller| caller.managed && matches!(caller.role, Role::Listener | Role::Kid))
}

/// An outside song as these endpoints read it: the id the app asked by, the registry's id, its
/// routing and its song key.
struct OutsideSong {
    id: String,
    external_id: String,
    routing: SoulseekRouting,
    key: String,
}

/// The outside song `id`: one Octo streams (its provider is the Soulseek one), a song, with a
/// song key. None for a library song, another provider's id, an id the registry does not know,
/// or a song without an artist and a title.
fn outside_song(state: &AppState, id: &str) -> Option<OutsideSong> {
    let (is_external, provider, external_id) = state.local_library.parse_song_id(id);
    let external_id = external_id.filter(|_| is_external)?;
    if !provider
        .as_deref()
        .is_some_and(|provider| eq_ignore_case(provider, SoulseekMetadataService::PROVIDER_NAME))
    {
        return None;
    }
    let routing = state
        .external_id_registry
        .resolve_song(&external_id, false)?
        .snapshot();
    if routing.kind != RoutingKind::Song {
        return None;
    }
    let key = StreamChoices::key_of(&routing)?;
    Some(OutsideSong {
        id: id.to_string(),
        external_id,
        routing,
        key,
    })
}

/// The library file of an outside song this caller's play is served now: the version kept for
/// them, else the one kept for nobody in particular or the most recently landed one, when it is
/// a file they may have. None when their play streams.
pub(crate) async fn kept_file(
    state: &AppState,
    caller: Option<&FamilyCaller>,
    user: &str,
    provider: &str,
    external_id: &str,
) -> Option<String> {
    let user = user.trim();
    let mine = if user.is_empty() {
        None
    } else {
        state
            .local_library
            .get_local_path_for_external_song_as(Some(user), provider, external_id)
            .await
    };
    let found = match mine {
        Some(path) => Some(path),
        None => {
            state
                .local_library
                .get_local_path_for_external_song(provider, external_id)
                .await
        }
    };
    let path = found.filter(|path| Path::new(path).is_file())?;
    may_have_file(state, caller, &path).then_some(path)
}

/// Whether a caller may be sent a library file, as a play of an outside song decides it: with
/// Family on, never a file in another member's own library.
fn may_have_file(state: &AppState, caller: Option<&FamilyCaller>, path: &str) -> bool {
    let Some(caller) = caller else {
        return true;
    };
    let full = navidrome_song_list::full_path(path);
    let song = state.library_index.find_by_path(&full);
    let scope = if caller.managed {
        LibraryScope::Viewer(caller.user.clone())
    } else {
        LibraryScope::Shared
    };
    state
        .library_scope
        .allows(&scope, song.as_ref().map(|s| s.id.as_str()), Some(&full))
}

/// Where the song plays from for this caller.
async fn plays_from(
    state: &AppState,
    caller: Option<&FamilyCaller>,
    user: &str,
    external_id: &str,
) -> PlaysFrom {
    let settings = state.settings.current();
    if downloads_every_play(&settings.subsonic) {
        return PlaysFrom::Download;
    }
    match kept_file(
        state,
        caller,
        user,
        SoulseekMetadataService::PROVIDER_NAME,
        external_id,
    )
    .await
    {
        Some(_) => PlaysFrom::File,
        None => PlaysFrom::Stream,
    }
}

/// Download on play and Wait for lossless on play add every played song to the library.
fn downloads_every_play(subsonic: &octo_core::settings::SubsonicSettings) -> bool {
    subsonic.download_on_play || subsonic.wait_for_lossless_on_play
}

/// The pool an answer is built from: listed (kept or new), or the one last offered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PoolFrom {
    Listing,
    Offered,
}

/// One answer for the song: where it plays from, the video playing, the caller's heard video,
/// busy, the versions from the pool, and every id.
async fn answer_for(
    state: &AppState,
    caller: Option<&FamilyCaller>,
    user: &str,
    song: &OutsideSong,
    from: PoolFrom,
) -> VersionsAnswer {
    let plays_from = plays_from(state, caller, user, &song.external_id).await;
    let (rows, missed) = match (plays_from, from) {
        (PlaysFrom::Stream, PoolFrom::Listing) => {
            let pool = state
                .stream_versions
                .pool(&song.key, &ListedSong::of(&song.routing), false)
                .await;
            (pool.rows, pool.missed)
        }
        (PlaysFrom::Stream, PoolFrom::Offered) => (
            state.stream_versions.offered(&song.key).unwrap_or_default(),
            Vec::new(),
        ),
        _ => (Vec::new(), Vec::new()),
    };
    // Read after the listing: a pick that landed meanwhile is the one shown.
    let registry = &state.external_id_registry;
    let routing = registry
        .resolve_song(&song.external_id, false)
        .map(|shared| shared.snapshot())
        .unwrap_or_else(|| song.routing.clone());
    let ids = registry.song_ids(&song.external_id);
    let playing_video = registry.song_video(&routing);
    let playing = playing_video
        .as_deref()
        .map(|video| playing_version(registry, &routing, video, &rows, user));
    let heard_video_id = latest_heard_video(registry, &song.external_id, &ids, user)
        .filter(|heard| Some(heard) != playing_video.as_ref());
    let versions = if plays_from == PlaysFrom::Stream {
        view(
            &rows,
            playing_video.as_deref(),
            |video| routing.refuses_video(video),
            state.stream_versions.caps().shown,
        )
    } else {
        Vec::new()
    };
    let busy = !user.trim().is_empty() && state.heart_acquisition_coordinator.running_own(&ids, user);
    VersionsAnswer {
        id: song.id.clone(),
        plays_from,
        playing,
        heard_video_id,
        busy,
        versions,
        missed,
        ids,
    }
}

/// The video playing, with its facts from the pool, else from the record that named it, else the
/// song's shown length, and who picked it.
fn playing_version(
    registry: &ExternalIdRegistry,
    routing: &SoulseekRouting,
    video: &str,
    rows: &[VersionRow],
    user: &str,
) -> PlayingVersion {
    let pick = registry.pick_for(routing);
    let row = rows.iter().find(|row| row.video_id == video);
    let record = match &pick {
        PickState::Person(choice) | PickState::Octo(choice) if choice.video == video => Some(choice),
        _ => None,
    };
    let (picked, picked_by, mine, picked_at) = match &pick {
        PickState::Person(choice) => (
            Picked::Person,
            Some(choice.by.trim().to_string()).filter(|by| !by.is_empty()),
            !user.trim().is_empty() && eq_ignore_case(choice.by.trim(), user.trim()),
            Some(choice.at),
        ),
        PickState::Earlier => (Picked::Earlier, None, false, None),
        PickState::Octo(_) | PickState::None => (Picked::Octo, None, false, None),
    };
    PlayingVersion {
        video_id: video.to_string(),
        title: row
            .map(|row| row.title.clone())
            .or_else(|| record.and_then(|choice| choice.title.clone())),
        channel: row
            .and_then(|row| row.channel.clone())
            .or_else(|| record.and_then(|choice| choice.channel.clone())),
        seconds: row
            .and_then(|row| row.seconds)
            .or_else(|| record.and_then(|choice| choice.seconds))
            .or(routing.shown_duration),
        kind: row
            .map(|row| row.kind)
            .or_else(|| record.map(|choice| choice.kind)),
        picked,
        picked_by,
        mine,
        picked_at,
    }
}

/// The video the caller heard last on any id of the song, when what they heard was a video.
fn latest_heard_video(
    registry: &ExternalIdRegistry,
    external_id: &str,
    ids: &[String],
    user: &str,
) -> Option<String> {
    if user.trim().is_empty() {
        return None;
    }
    std::iter::once(external_id)
        .chain(ids.iter().map(String::as_str))
        .filter_map(|id| registry.heard_by(id, user))
        .max_by_key(|record| record.heard_at)
        .and_then(|record| match record.source {
            HeardSource::Video { id, .. } => Some(id),
            _ => None,
        })
}

/// The chooser's heard records of the song go, on every id and in their holds, so their next
/// play's record is the one a heart of it is judged by.
fn forget_heard(state: &AppState, ids: &[String], user: &str) {
    if user.trim().is_empty() {
        return;
    }
    state.external_id_registry.forget_heard_of_song(ids, user);
    state
        .heart_acquisition_coordinator
        .forget_heard_of_song(ids, user);
}

fn error(state: &AppState, code: i32, message: &str) -> Response {
    state
        .subsonic_response_builder
        .create_error(FORMAT, code, message)
        .into_response()
}

fn choose_refused(state: &AppState, refusal: ChooseError) -> Response {
    match refusal {
        ChooseError::Refused => error(state, 70, VIDEO_REFUSED),
        ChooseError::NotASong | ChooseError::NoKey => error(state, 70, CANT_CHOOSE),
    }
}

fn answer(state: &AppState, answer: &VersionsAnswer) -> Response {
    state
        .subsonic_response_builder
        .create_stream_versions_response(answer)
        .into_response()
}

/// octoStreamVersions v1: the video the outside song `id` plays from and the other videos it
/// can play from, listed from YouTube Music and the shim (kept half an hour once every source
/// answered).
pub async fn get_stream_versions(State(state): State<AppState>, req: Request) -> Response {
    let caller = family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    let id = call.param("id").trim().to_string();
    if is_blank(&id) {
        return error(&state, 10, "Required parameter is missing: id");
    }
    let Some(song) = outside_song(&state, &id) else {
        return error(&state, 70, CANT_CHOOSE);
    };
    let user = listener_name(&state, &call).await;
    let built = answer_for(&state, caller.as_ref(), &user, &song, PoolFrom::Listing).await;
    answer(&state, &built)
}

/// octoStreamVersions v1: makes `video` (one of the videos offered for the song, checked
/// playable now) the one the outside song `id` plays from for everyone, from each play's next
/// start, or with `video=auto` goes back to Octo's own pick. Answers like getStreamVersions,
/// with `ids` every id the song has.
pub async fn set_stream_version(State(state): State<AppState>, req: Request) -> Response {
    let caller = family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    if !may_pick(caller.as_ref()) {
        return error(&state, 50, PICK_REFUSED);
    }
    let id = call.param("id").trim().to_string();
    let video = call.param("video").trim().to_string();
    if is_blank(&id) || video.is_empty() {
        return error(&state, 10, "Required parameter is missing: id and video");
    }
    let Some(song) = outside_song(&state, &id) else {
        return error(&state, 70, CANT_CHOOSE);
    };
    if downloads_every_play(&state.settings.current().subsonic) {
        return error(&state, 0, PLAYS_FROM_DOWNLOAD);
    }
    let user = listener_name(&state, &call).await;
    if video.eq_ignore_ascii_case(AUTO) {
        return undo(&state, caller.as_ref(), &user, &song).await;
    }
    if song.routing.refuses_video(&video) {
        return error(&state, 70, VIDEO_REFUSED);
    }
    let registry = &state.external_id_registry;
    if registry.song_video(&song.routing).as_deref() == Some(video.as_str()) {
        let built = answer_for(&state, caller.as_ref(), &user, &song, PoolFrom::Offered).await;
        return answer(&state, &built);
    }

    // The network steps: the video is one offered (listed once more when it is not), and it plays.
    let row = match state.stream_versions.offered_row(&song.key, &video) {
        Some(row) => row,
        None => {
            let pool = state
                .stream_versions
                .pool(&song.key, &ListedSong::of(&song.routing), true)
                .await;
            match pool.rows.into_iter().find(|row| row.video_id == video) {
                Some(row) => row,
                None => return error(&state, 70, NOT_OFFERED),
            }
        }
    };
    if matches!(
        state.stream_versions.resolver().facts_fresh(&video).await,
        Lookup::Failed(Failure::Gone | Failure::Refused)
    ) {
        return error(&state, 0, VIDEO_REFUSED);
    }

    // No await from here until the pick and the clears stand: the busy check, the store and the
    // clears see one state.
    let ids = registry.song_ids(&song.external_id);
    if !user.trim().is_empty() && state.heart_acquisition_coordinator.running_own(&ids, &user) {
        return error(&state, 0, BEING_ADDED);
    }
    let choice = StreamChoice {
        video: row.video_id.clone(),
        title: Some(row.title.clone()),
        channel: row.channel.clone(),
        seconds: row.seconds,
        kind: row.kind,
        song_seconds: StreamChoices::catalog_seconds(&song.routing),
        by: user.trim().to_string(),
        at: Utc::now(),
        auto: false,
        seq: 0,
    };
    match registry.choose_stream_video(&song.external_id, choice) {
        Ok(ids) => forget_heard(&state, &ids, &user),
        Err(refusal) => return choose_refused(&state, refusal),
    }
    let built = answer_for(&state, caller.as_ref(), &user, &song, PoolFrom::Offered).await;
    answer(&state, &built)
}

/// `video=auto`: Octo's own pick is found now (YouTube Music's row, else the shim's search, the
/// refused videos left out, the catalog length as hint) and stored as Octo's, so the song never
/// goes without a video. With no person's pick standing, nothing changes. When YouTube cannot be
/// reached, the pick stays.
async fn undo(state: &AppState, caller: Option<&FamilyCaller>, user: &str, song: &OutsideSong) -> Response {
    let registry = &state.external_id_registry;
    if !matches!(
        registry.pick_for(&song.routing),
        PickState::Person(_) | PickState::Earlier
    ) {
        let built = answer_for(state, caller, user, song, PoolFrom::Offered).await;
        return answer(state, &built);
    }
    let routing = &song.routing;
    let length = StreamChoices::catalog_seconds(routing);
    let found = state
        .stream_versions
        .resolver()
        .auto_pick(
            routing.artist.as_deref().unwrap_or_default(),
            routing.title.as_deref().unwrap_or_default(),
            length,
            &routing.refused_videos,
        )
        .await;
    let Lookup::Found(hit) = found else {
        return error(state, 0, NO_OCTO_PICK);
    };
    let octo = StreamChoice {
        video: hit.video_id.clone(),
        title: hit.title.clone(),
        channel: hit.channel.clone(),
        seconds: hit.duration,
        kind: octo_pick_kind(&hit),
        song_seconds: length,
        by: String::new(),
        at: Utc::now(),
        auto: true,
        seq: 0,
    };
    match registry.undo_stream_choice(&song.external_id, octo) {
        Ok(ids) => forget_heard(state, &ids, user),
        Err(refusal) => return choose_refused(state, refusal),
    }
    let built = answer_for(state, caller, user, song, PoolFrom::Offered).await;
    answer(state, &built)
}

/// octoStreamVersionChanges v1: the songs whose chosen version changed after `since` (every
/// one when not sent), at most [`CHANGES_LIMIT`], each with its change number and every id it
/// has, and the `epoch` and `seq` to send next time. `complete: false` asks the app to drop
/// every saved outside song: another epoch, a cursor older than what is kept, or more changes
/// than one answer lists.
pub async fn get_stream_version_changes(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    let epoch = call
        .param_opt("epoch")
        .map(str::trim)
        .filter(|epoch| !epoch.is_empty());
    let since = call
        .param_opt("since")
        .and_then(|since| since.trim().parse::<u64>().ok());
    let feed = state
        .external_id_registry
        .changes_since(epoch, since, CHANGES_LIMIT);
    state
        .subsonic_response_builder
        .create_stream_version_changes_response(&feed.epoch, feed.seq, feed.complete, &feed.changes)
        .into_response()
}
