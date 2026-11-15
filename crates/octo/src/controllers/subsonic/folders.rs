//! Playlist folders: each person's folders and which playlists sit in them, kept by Octo
//! and read by Octo's apps. `getPlaylistFolders`, `savePlaylistFolder`, `deletePlaylistFolder` and
//! `setPlaylistFolder`; every one answers the person's whole folder state as `playlistFolders`, in
//! JSON or XML. The caller is the signed-in listener.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use serde_json::{Value, json};

use super::action_helpers::SubsonicCall;
use super::held::caller_name;
use super::playlists::{signed_in, utc_millis};
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::playlists::{FolderError, FolderState};

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic(
            "getPlaylistFolders",
            get(get_playlist_folders).post(get_playlist_folders),
        )
        .subsonic(
            "savePlaylistFolder",
            get(save_playlist_folder).post(save_playlist_folder),
        )
        .subsonic(
            "deletePlaylistFolder",
            get(delete_playlist_folder).post(delete_playlist_folder),
        )
        .subsonic(
            "setPlaylistFolder",
            get(set_playlist_folder).post(set_playlist_folder),
        )
}

const WRONG_SIGN_IN: &str = "Wrong username or password";

fn error(state: &AppState, format: &str, code: i32, message: &str) -> Response {
    state
        .subsonic_response_builder
        .create_error(format, code, message)
        .into_response()
}

/// A folder rule's refusal: error 10, or 70 for a folder that is not there.
fn folder_error(state: &AppState, format: &str, failure: FolderError) -> Response {
    error(state, format, failure.code(), failure.message())
}

/// The person's folder state as `playlistFolders`.
fn answer(state: &AppState, format: &str, folders: &FolderState) -> Response {
    let folder: Vec<Value> = folders
        .folders
        .iter()
        .map(|folder| {
            let mut row = json!({
                "id": folder.id,
                "name": folder.name,
                "created": utc_millis(&folder.created),
            });
            if let Some(parent) = &folder.parent {
                row["parent"] = json!(parent);
            }
            row
        })
        .collect();
    let place: Vec<Value> = folders
        .places
        .iter()
        .map(|(playlist, folder)| json!({ "playlist": playlist, "folder": folder }))
        .collect();
    let data = json!({
        "changed": utc_millis(&folders.changed),
        "folder": folder,
        "place": place,
    });
    state
        .subsonic_response_builder
        .create_merged_response(format, "playlistFolders", &data)
        .into_response()
}

/// The signed-in caller's Navidrome username, or the error to send.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
async fn owner(state: &AppState, call: &SubsonicCall) -> Result<String, Response> {
    let format = call.format.as_str();
    if !signed_in(state, call, format).await {
        return Err(error(state, format, 40, WRONG_SIGN_IN));
    }
    let name = caller_name(state, call).await;
    if name.trim().is_empty() {
        return Err(error(state, format, 40, WRONG_SIGN_IN));
    }
    Ok(name.trim().to_string())
}

/// A parameter's value, trimmed; None when missing or empty.
fn given<'a>(call: &'a SubsonicCall, name: &str) -> Option<&'a str> {
    call.param(name).map(str::trim).filter(|value| !value.is_empty())
}

async fn get_playlist_folders(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let owner = match owner(&state, &call).await {
        Ok(owner) => owner,
        Err(answer) => return answer,
    };
    answer(&state, &call.format, &state.playlist_folders.state(&owner))
}

async fn save_playlist_folder(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let owner = match owner(&state, &call).await {
        Ok(owner) => owner,
        Err(answer) => return answer,
    };
    let format = call.format.as_str();
    let Some(id) = given(&call, "id") else {
        return folder_error(&state, format, FolderError::Id);
    };
    let name = call.param("name").unwrap_or("");
    match state
        .playlist_folders
        .save_folder(&owner, id, name, given(&call, "parentId"), Utc::now())
    {
        Ok(folders) => answer(&state, format, &folders),
        Err(failure) => folder_error(&state, format, failure),
    }
}

async fn delete_playlist_folder(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let owner = match owner(&state, &call).await {
        Ok(owner) => owner,
        Err(answer) => return answer,
    };
    let format = call.format.as_str();
    let Some(id) = given(&call, "id") else {
        return folder_error(&state, format, FolderError::Id);
    };
    match state.playlist_folders.delete_folder(&owner, id, Utc::now()) {
        Ok(folders) => answer(&state, format, &folders),
        Err(failure) => folder_error(&state, format, failure),
    }
}

async fn set_playlist_folder(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let owner = match owner(&state, &call).await {
        Ok(owner) => owner,
        Err(answer) => return answer,
    };
    let format = call.format.as_str();
    let playlists: Vec<String> = call
        .incoming
        .parameter_values("playlistId")
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    if playlists.is_empty() {
        return error(&state, format, 10, "Required parameter is missing: playlistId");
    }
    // Octo's generated lists and Liked Songs stay in their own block.
    let liked = state.generated_playlists.liked_id(&owner);
    if playlists.iter().any(|id| {
        *id == liked || (id.chars().count() == 22 && (id.starts_with("or") || id.starts_with("og")))
    }) {
        return folder_error(&state, format, FolderError::OwnList);
    }
    match state
        .playlist_folders
        .set_places(&owner, &playlists, given(&call, "folderId"), Utc::now())
    {
        Ok(folders) => answer(&state, format, &folders),
        Err(failure) => folder_error(&state, format, failure),
    }
}
