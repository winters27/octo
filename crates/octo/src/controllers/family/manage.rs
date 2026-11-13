//! What managers do on the Family page: members (add, change, pause, remove, new sign-in link,
//! repair), the requests inbox, everyone's devices, family playlists, the load panel, the
//! libraries members can see, the explicit marks card, and turning Family on.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use octo_core::family::abilities::Preset;
use octo_core::family::request::RequestState;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Web, body_as, family_error, no_content, ok, refusal, require_on};
use crate::app::AppState;
use crate::services::family::family_service::{AddMember, EditMember, FilesChoice, ShimLoad};
use crate::services::family::public_address;
use crate::services::family::{FamilyService, SharedLibraries};

/// Reads a preset by name, any case.
pub fn preset_named(text: &str) -> Option<Preset> {
    match text.trim().to_ascii_lowercase().as_str() {
        "owner" => Some(Preset::Owner),
        "coadmin" | "co-admin" => Some(Preset::CoAdmin),
        "member" => Some(Preset::Member),
        "listener" => Some(Preset::Listener),
        "kid" => Some(Preset::Kid),
        _ => None,
    }
}

/// Reads `"All"` or a list of library ids.
pub fn shared_libraries(value: &Value) -> Option<SharedLibraries> {
    match value {
        Value::String(text) if text.trim().eq_ignore_ascii_case("all") => Some(SharedLibraries::All),
        Value::Array(items) => items
            .iter()
            .map(Value::as_i64)
            .collect::<Option<Vec<_>>>()
            .map(SharedLibraries::Ids),
        _ => None,
    }
}

/// `GET /api/family/members`.
pub async fn members(State(state): State<AppState>, web: Web) -> Response {
    if let Err(res) = web.manager(&state).await {
        return res;
    }
    let list: Vec<Value> = state
        .family_store
        .members()
        .iter()
        .map(|m| state.family.member_full_json(&state, m))
        .collect();
    ok(&Value::Array(list))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AddMemberBody {
    pub username: String,
    pub display_name: String,
    pub preset: Option<String>,
    pub shared_libraries: Option<Value>,
    pub adopt: bool,
    /// May they listen away from home (on by default).
    pub away: Option<bool>,
}

/// `POST /api/family/members`: answers the member and their invite link.
pub async fn add_member(State(state): State<AppState>, web: Web, body: Bytes) -> Response {
    let signed = match web.manager(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let request: AddMemberBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let preset = match request.preset.as_deref() {
        None | Some("") => None,
        Some(text) => match preset_named(text) {
            Some(preset) => Some(preset),
            None => {
                return refusal(
                    StatusCode::BAD_REQUEST,
                    "Choose CoAdmin, Member, Listener or Kid.",
                );
            }
        },
    };
    let shared = match request.shared_libraries.as_ref() {
        None | Some(Value::Null) => None,
        Some(value) => match shared_libraries(value) {
            Some(shared) => Some(shared),
            None => {
                return refusal(
                    StatusCode::BAD_REQUEST,
                    "Shared libraries are \"All\" or a list of library ids.",
                );
            }
        },
    };
    let add = AddMember {
        username: request.username,
        display_name: request.display_name,
        preset,
        shared_libraries: shared,
        adopt: request.adopt,
        away: request.away,
    };
    match state
        .family
        .add_member(&state, &signed.actor, add, &web.base(&state))
        .await
    {
        Ok((member, link)) => {
            let bases = web.link_bases(&state, &member.username);
            let mut answer = public_address::invite_answer(&link, &bases, invite_days(&state));
            answer["member"] = state.family.member_full_json(&state, &member);
            ok(&answer)
        }
        Err(e) => family_error(&e),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EditMemberBody {
    pub display_name: Option<String>,
    pub preset: Option<String>,
    pub edits: Option<Map<String, Value>>,
    pub shared_libraries: Option<Value>,
    pub suspended: Option<bool>,
}

/// `PUT /api/family/members/{name}`.
pub async fn edit_member(
    State(state): State<AppState>,
    web: Web,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let signed = match web.manager(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let request: EditMemberBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let preset = match request.preset.as_deref() {
        None => None,
        Some(text) => match preset_named(text) {
            Some(preset) => Some(preset),
            None => {
                return refusal(
                    StatusCode::BAD_REQUEST,
                    "Choose CoAdmin, Member, Listener or Kid.",
                );
            }
        },
    };
    let shared = match request.shared_libraries.as_ref() {
        None | Some(Value::Null) => None,
        Some(value) => match shared_libraries(value) {
            Some(shared) => Some(shared),
            None => {
                return refusal(
                    StatusCode::BAD_REQUEST,
                    "Shared libraries are \"All\" or a list of library ids.",
                );
            }
        },
    };
    let edit = EditMember {
        display_name: request.display_name,
        preset,
        edits: request.edits,
        shared_libraries: shared,
        suspended: request.suspended,
    };
    match state.family.edit_member(&state, &signed.actor, &name, edit).await {
        Ok(member) => ok(&state.family.member_full_json(&state, &member)),
        Err(e) => family_error(&e),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoveMemberBody {
    pub confirm: String,
    pub files: String,
    /// Leave Family but keep their Navidrome account and password.
    pub keep_account: bool,
}

/// `DELETE /api/family/members/{name}`: needs the username typed as `confirm`, and what to do
/// with their files. With `keepAccount` their Navidrome account stays.
pub async fn remove_member(
    State(state): State<AppState>,
    web: Web,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let signed = match web.manager(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let request: RemoveMemberBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let Some(files) = FilesChoice::parse(&request.files) else {
        return refusal(
            StatusCode::BAD_REQUEST,
            "Choose keep, promote or delete for their files.",
        );
    };
    match state
        .family
        .remove_member(
            &state,
            &signed.actor,
            &name,
            &request.confirm,
            files,
            request.keep_account,
        )
        .await
    {
        Ok(()) => no_content(),
        Err(e) => family_error(&e),
    }
}

/// How many days a new sign-in link works, as the invite answers say it.
fn invite_days(state: &AppState) -> u32 {
    state.settings.current().family.invite_days.max(1)
}

/// `POST /api/family/members/{name}/invite`: a new sign-up link for a member who has not
/// signed up yet ("Send a new link").
pub async fn invite_member(State(state): State<AppState>, web: Web, Path(name): Path<String>) -> Response {
    if let Err(res) = web.manager(&state).await {
        return res;
    }
    match state.family.invite(&state, &name, &web.base(&state)) {
        Ok(link) => ok(&public_address::invite_answer(
            &link,
            &web.link_bases(&state, &name),
            invite_days(&state),
        )),
        Err(e) => family_error(&e),
    }
}

/// `POST /api/family/members/{name}/reset`: "Reset password". The member's password stops
/// working at once, every web player and Family page session of theirs ends, and the answer is
/// a new single-use sign-up link (the invite shape) to choose another.
pub async fn reset_member(State(state): State<AppState>, web: Web, Path(name): Path<String>) -> Response {
    let signed = match web.manager(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    match state
        .family
        .reset_password(&state, &signed.actor, &name, &web.base(&state))
    {
        Ok(link) => ok(&public_address::invite_answer(
            &link,
            &web.link_bases(&state, &name),
            invite_days(&state),
        )),
        Err(e) => family_error(&e),
    }
}

/// `POST /api/family/members/{name}/repair`.
pub async fn repair_member(State(state): State<AppState>, web: Web, Path(name): Path<String>) -> Response {
    let signed = match web.manager(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    match state.family.repair(&state, &signed.actor, &name).await {
        Ok(()) => no_content(),
        Err(e) => family_error(&e),
    }
}

/// Reads a request state by name, any case.
pub fn request_state(text: &str) -> Option<RequestState> {
    serde_json::from_value(Value::String(
        ["Pending", "Approved", "Declined", "Cancelled", "Done", "Failed"]
            .into_iter()
            .find(|s| s.eq_ignore_ascii_case(text.trim()))?
            .to_string(),
    ))
    .ok()
}

/// `GET /api/family/requests?state=`: every member's requests.
pub async fn requests(State(state): State<AppState>, web: Web, RawQuery(query): RawQuery) -> Response {
    if let Err(res) = web.approver(&state).await {
        return res;
    }
    let wanted = url::form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.trim().is_empty());
    let filter = match wanted.as_deref() {
        None => None,
        Some(text) => match request_state(text) {
            Some(found) => Some(found),
            None => {
                return refusal(
                    StatusCode::BAD_REQUEST,
                    "Use Pending, Approved, Declined, Cancelled, Done or Failed.",
                );
            }
        },
    };
    let list: Vec<Value> = FamilyService::requests(&state, None, filter)
        .iter()
        .map(|r| FamilyService::request_json(&state, r))
        .collect();
    ok(&Value::Array(list))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct NoteBody {
    pub note: Option<String>,
}

async fn decide(state: AppState, web: Web, id: String, body: Bytes, approve: bool) -> Response {
    let signed = match web.approver(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let request: NoteBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let note = request.note.unwrap_or_default();
    match state
        .family
        .hooks(&state)
        .requests
        .decide(&id, approve, signed.user(), note.trim())
        .await
    {
        Ok(decided) => ok(&FamilyService::request_json(&state, &decided)),
        Err(e) => family_error(&e),
    }
}

/// `POST /api/family/requests/{id}/approve`.
pub async fn approve(
    State(state): State<AppState>,
    web: Web,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    decide(state, web, id, body, true).await
}

/// `POST /api/family/requests/{id}/decline`.
pub async fn decline(
    State(state): State<AppState>,
    web: Web,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    decide(state, web, id, body, false).await
}

/// `GET /api/family/devices`: every member's devices and the devices seen for everyone else.
pub async fn devices(State(state): State<AppState>, web: Web) -> Response {
    if let Err(res) = web.manager(&state).await {
        return res;
    }
    let list: Vec<Value> = FamilyService::all_devices(&state)
        .iter()
        .map(|d| state.family.device_json(&state, d, None))
        .collect();
    ok(&Value::Array(list))
}

/// `DELETE /api/family/devices/{id}`: takes a member's device off the list.
pub async fn forget_device(State(state): State<AppState>, web: Web, Path(id): Path<String>) -> Response {
    let signed = match web.manager(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    match state.family.forget_device(&state, &signed.actor, &id, true) {
        Ok(()) => no_content(),
        Err(e) => family_error(&e),
    }
}

/// `GET /api/family/playlists`.
pub async fn playlists(State(state): State<AppState>, web: Web) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    ok(&json!(state.family.hooks(&state).playlists.list(signed.user())))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlaylistBody {
    pub name: Option<String>,
    pub approve_additions: Option<bool>,
}

/// `POST /api/family/playlists`: needs Family playlists editing; only managers set approval.
pub async fn create_playlist(State(state): State<AppState>, web: Web, body: Bytes) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let request: PlaylistBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let Some(name) = request.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
        return refusal(StatusCode::BAD_REQUEST, "Give the playlist a name.");
    };
    match state
        .family
        .hooks(&state)
        .playlists
        .create(name, signed.user(), request.approve_additions.unwrap_or(false))
        .await
    {
        Ok(playlist) => ok(&json!(playlist)),
        Err(e) => family_error(&e),
    }
}

/// `PUT /api/family/playlists/{id}`: the creator or a manager renames it; only managers turn
/// approval of additions on or off.
pub async fn change_playlist(
    State(state): State<AppState>,
    web: Web,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let request: PlaylistBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let name = request.name.as_deref().map(str::trim);
    if name.is_some_and(str::is_empty) {
        return refusal(StatusCode::BAD_REQUEST, "Give the playlist a name.");
    }
    match state
        .family
        .hooks(&state)
        .playlists
        .update(&id, signed.user(), name, request.approve_additions)
        .await
    {
        Ok(playlist) => ok(&json!(playlist)),
        Err(e) => family_error(&e),
    }
}

/// `DELETE /api/family/playlists/{id}`: managers.
pub async fn delete_playlist(State(state): State<AppState>, web: Web, Path(id): Path<String>) -> Response {
    let signed = match web.manager(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    match state
        .family
        .hooks(&state)
        .playlists
        .delete(&id, signed.user())
        .await
    {
        Ok(()) => no_content(),
        Err(e) => family_error(&e),
    }
}

/// `GET /api/family/load`: the live streams and queue, with the outside-song helper's slots
/// read from its health (`shim` is null when the helper did not answer).
pub async fn load(State(state): State<AppState>, web: Web) -> Response {
    if let Err(res) = web.manager(&state).await {
        return res;
    }
    let mut report = state.family.hooks(&state).streams.load();
    report.shim = state.you_tube_resolver.busy().await.map(ShimLoad::from);
    ok(&json!(report))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EnableBody {
    pub enabled: Option<bool>,
    /// Where members' music lives: `NextToMusic` or a full folder path. Absent keeps the setting.
    #[serde(rename = "membersPath")]
    pub members_path: Option<String>,
}

/// `POST /api/family/enable`: the owner (a Navidrome admin) turns Family on or off.
pub async fn enable(State(state): State<AppState>, web: Web, body: Bytes) -> Response {
    let signed = match web.owner(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let request: EnableBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let Some(on) = request.enabled else {
        return refusal(StatusCode::BAD_REQUEST, "Say enabled true or false.");
    };
    match state
        .family
        .enable(&state, signed.user(), on, request.members_path.as_deref())
        .await
    {
        Ok(report) => ok(&json!(report)),
        Err(e) => family_error(&e),
    }
}

/// `GET /api/family/libraries`.
pub async fn libraries(State(state): State<AppState>, web: Web) -> Response {
    if let Err(res) = web.manager(&state).await {
        return res;
    }
    // Members get any library added since they were last given theirs.
    let _ = state.family.sync_libraries(&state).await;
    match state.family.libraries_json(&state).await {
        Ok(list) => ok(&list),
        Err(e) => family_error(&e),
    }
}

/// `GET /api/family/unrated`: how many shared songs have no explicit mark (null while unknown).
pub async fn unrated(State(state): State<AppState>, web: Web) -> Response {
    if let Err(res) = web.manager(&state).await {
        return res;
    }
    let hooks = state.family.hooks(&state);
    let kids = state
        .family_store
        .members()
        .iter()
        .any(|m| m.abilities().clean_only);
    ok(&json!({ "count": hooks.unrated.unrated(), "kids": kids }))
}

/// `POST /api/family/unrated/mark`: starts marking them.
pub async fn mark_unrated(State(state): State<AppState>, web: Web) -> Response {
    let signed = match web.manager(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    if let Err(res) = require_on(&state) {
        return res;
    }
    match state.family.hooks(&state).unrated.mark_now(signed.user()).await {
        Ok(message) => ok(&json!({ "message": message })),
        Err(e) => family_error(&e),
    }
}
