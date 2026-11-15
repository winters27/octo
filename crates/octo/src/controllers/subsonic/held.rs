//! Playlists that hold songs found online, through the Subsonic API: the caller checks, the
//! refusals, the relays and the answers of `getPlaylist`, `updatePlaylist`, `createPlaylist` and
//! `deletePlaylist` for a playlist Octo holds or may come to hold, and the songs found online a
//! sync catalog lists for them. The locked work (classification, sync, takeover, pairing, mirror
//! write, release) is [`crate::services::playlists::held_playlists`]'s; this file hands it the
//! built entries, the caller's relays and the caller's served list.
//!
//! Every change runs on its own task, which the request awaits, so a client that goes away never
//! stops a change half way.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use futures::future::{BoxFuture, FutureExt};
use indexmap::IndexMap;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::json::dom::Node;
use octo_core::models::domain::Song;
use octo_subsonic::SubsonicReply;
use octo_subsonic::subsonic_request_parser::Parameters;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tracing::warn;

use super::action_helpers::{SubsonicCall, is_successful_subsonic_response};
use super::playlists::{PlaylistChange, RowMarks, listed_rows, outside_song_json, signed_in};
use crate::app::AppState;
use crate::services::family::audio_gate::ExplicitMark;
use crate::services::family::family_playlists::{self, Serve};
use crate::services::family::hooks::caller_for;
use crate::services::family::rules::AnswerContext;
use crate::services::family::{FamilyCaller, family_caller, kid_filter};
use crate::services::library::LibraryActionPlaylistProvisioner;
use crate::services::playlists::entries::Built;
use crate::services::playlists::held_playlists::{
    self, CreateOutcome, CreateRequest, HeldCaller, HeldError, HeldView, ListedPlaylist, Pick, Relay,
    RelayAnswer, ReplaceOutcome, ReplaceRequest, ServedView, UpdateOutcome, UpdateRequest, streams,
};
use crate::services::playlists::held_store::HeldEntry;
use crate::services::subsonic::{HeldCatalog, SubsonicProxyService};

const UPDATE_FAILED: &str = "Unable to update playlist";
const WRONG_SIGN_IN: &str = "Wrong username or password";
const BOTH_REMOVALS: &str = "Use octoSongToRemove or songIndexToRemove, not both.";

/// The sign-in parameters a relay made for the caller carries.
const AUTH_KEYS: [&str; 8] = ["u", "p", "t", "s", "v", "c", "apiKey", "jwt"];

/// How many outside songs of a family playlist are read from the catalog at once.
const CATALOG_SONGS_AT_ONCE: usize = 8;

// ---------------------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------------------

/// A Subsonic error in `format`.
fn error(state: &AppState, format: &str, code: i32, message: &str) -> Response {
    state
        .subsonic_response_builder
        .create_error(format, code, message)
        .into_response()
}

/// `subsonic-response` of a JSON body edited in place, written back; None for a body that is not
/// a JSON envelope.
fn json_edited(body: &[u8], edit: impl FnOnce(&mut IndexMap<String, Node>)) -> Option<String> {
    let mut root = Node::parse(std::str::from_utf8(body).ok()?).ok()?;
    let response = root
        .as_object_mut()?
        .get_mut("subsonic-response")?
        .as_object_mut()?;
    edit(response);
    Some(root.to_json_string(false))
}

/// A held playlist error in `format`; the stamp mismatch carries `octoConflict: true` in JSON.
fn held_error(state: &AppState, format: &str, failure: HeldError) -> Response {
    let reply = state
        .subsonic_response_builder
        .create_error(format, failure.code(), failure.message());
    if !failure.conflict() {
        return reply.into_response();
    }
    let edited = json_edited(&reply.body, |response| {
        if let Some(Node::Object(answer)) = response.get_mut("error") {
            answer.insert("octoConflict".into(), Node::Bool(true));
        }
    });
    match edited {
        Some(text) => SubsonicReply::json_text(text).into_response(),
        None => reply.into_response(),
    }
}

/// `reply`, with `octoSkipped` listing the ids left out when it is JSON.
fn with_skipped(reply: SubsonicReply, format: &str, skipped: &[String]) -> Response {
    if skipped.is_empty() || !format.eq_ignore_ascii_case("json") {
        return reply.into_response();
    }
    let ids = Node::Array(skipped.iter().map(|id| Node::String(id.clone())).collect());
    match json_edited(&reply.body, |response| {
        response.insert("octoSkipped".into(), ids);
    }) {
        Some(text) => SubsonicReply::json_text(text).into_response(),
        None => reply.into_response(),
    }
}

/// Navidrome's own error in a JSON answer, in the caller's format.
fn passed_error(state: &AppState, format: &str, body: &[u8]) -> Response {
    let answer: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let failure = &answer["subsonic-response"]["error"];
    let code = failure["code"]
        .as_i64()
        .and_then(|code| i32::try_from(code).ok())
        .unwrap_or(0);
    let message = failure["message"].as_str().unwrap_or(UPDATE_FAILED);
    error(state, format, code, message)
}

/// A relayed answer passed back as it came, or error 0 when Navidrome did not answer.
fn relayed(state: &AppState, call: &SubsonicCall, answer: Option<RelayAnswer>) -> Response {
    match answer {
        Some(answer) => call.file(&Bytes::from(answer.body), answer.content_type.as_deref()),
        None => error(state, &call.format, 0, UPDATE_FAILED),
    }
}

// ---------------------------------------------------------------------------------------
// Reading the request and the caller
// ---------------------------------------------------------------------------------------

/// The request's sign-in parameters, without anything else it sent.
fn auth_pairs(call: &SubsonicCall) -> Vec<(String, String)> {
    call.parameters
        .iter()
        .filter(|(key, _)| AUTH_KEYS.iter().any(|name| name.eq_ignore_ascii_case(key)))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Every value the request sent for `name`, trimmed, empty ones left out.
fn values(call: &SubsonicCall, name: &str) -> Vec<String> {
    call.incoming
        .parameter_values(name)
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// `octoSongToRemove=<id>@<n>`: the n-th time (from 1) song `id` shows in the list.
fn song_removals(call: &SubsonicCall) -> Vec<(String, usize)> {
    call.incoming
        .parameter_values("octoSongToRemove")
        .iter()
        .filter_map(|value| {
            let (id, n) = value.trim().rsplit_once('@')?;
            let n: usize = n.trim().parse().ok().filter(|n| *n > 0)?;
            (!id.is_empty()).then(|| (id.to_string(), n))
        })
        .collect()
}

/// Whether any id reads as Octo's (a song, album or artist found online): such a change goes
/// through the held routes, where only songs are taken.
fn looks_outside(state: &AppState, ids: &[String]) -> bool {
    ids.iter()
        .any(|id| state.local_library.parse_song_id(id).0 || state.local_library.parse_external_id(id).0)
}

/// The endpoint a relay of this request names: `rest/<the path's last segment>`.
fn relay_endpoint(call: &SubsonicCall) -> String {
    let path = call.path();
    let endpoint = path.split('/').next_back().unwrap_or("").replace(".view", "");
    format!("rest/{endpoint}")
}

/// A relay of these parameters as the caller, run when the held service awaits it, answering in
/// the caller's format.
fn relay_later(call: &SubsonicCall, endpoint: String, pairs: Vec<(String, String)>) -> Relay {
    let proxy = call.proxy.clone();
    let format = call.format.clone();
    async move {
        proxy
            .relay_safe(&endpoint, pairs.iter().map(|(k, v)| (k, v)))
            .await
            .map(|relay| RelayAnswer {
                ok: is_successful_subsonic_response(&relay.body, &format),
                body: relay.body.to_vec(),
                content_type: relay.content_type,
            })
    }
    .boxed()
}

/// The request relayed as it came, without `octoSongToRemove`.
fn plain_relay(call: &SubsonicCall) -> Relay {
    let pairs: Vec<(String, String)> = call
        .parameters
        .iter()
        .filter(|(key, _)| !key.eq_ignore_ascii_case("octoSongToRemove"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    relay_later(call, relay_endpoint(call), pairs)
}

/// The caller's Navidrome username, or empty when Octo cannot tell.
pub(crate) async fn caller_name(state: &AppState, call: &SubsonicCall) -> String {
    state
        .request_identity
        .username(&call.parameters, &call.proxy)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// An RFC 3339 instant.
pub(crate) fn parse_instant(text: &str) -> Option<DateTime<Utc>> {
    held_playlists::parse_instant(Some(text))
}

/// Whether `id` is a held playlist now.
pub(crate) fn held_now(state: &AppState, id: &str) -> bool {
    state.held_playlists.contains(id)
}

/// Whether Octo has a part in a change to `id` beyond relaying it: the playlist is held now,
/// kept aside by the gone check, or keeps a `Replaced` record.
pub(crate) fn has_part(state: &AppState, id: &str) -> bool {
    state.held_playlists.has_part(id)
}

/// The playlist as its caller's own `getPlaylist` shows it.
pub(crate) struct CallerView {
    /// The playlist row without its songs.
    pub fields: serde_json::Map<String, Value>,
    /// The songs Navidrome describes to the caller, in order.
    pub entries: Vec<Value>,
    pub owner: String,
    pub name: String,
    pub changed: Option<DateTime<Utc>>,
    /// The caller's Navidrome username.
    pub username: String,
    /// Whether the caller owns the playlist.
    pub owned: bool,
}

/// `getPlaylist` of `id` as the caller, in JSON: the playlist object, or the answer to send
/// (Navidrome's own error in `format`, error 0 with `failure` when it did not answer).
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
async fn caller_playlist(
    state: &AppState,
    format: &str,
    proxy: &SubsonicProxyService,
    auth: &[(String, String)],
    id: &str,
    failure: &str,
) -> Result<serde_json::Map<String, Value>, Response> {
    let mut ask = auth.to_vec();
    ask.push(("f".into(), "json".into()));
    ask.push(("id".into(), id.to_string()));
    let Some(relay) = proxy
        .relay_safe("rest/getPlaylist", ask.iter().map(|(k, v)| (k, v)))
        .await
    else {
        return Err(error(state, format, 0, failure));
    };
    if !is_successful_subsonic_response(&relay.body, "json") {
        return Err(passed_error(state, format, &relay.body));
    }
    let answer: Value = serde_json::from_slice(&relay.body).unwrap_or(Value::Null);
    match answer["subsonic-response"].get("playlist").cloned() {
        Some(Value::Object(fields)) => Ok(fields),
        _ => Err(error(state, format, 70, held_playlists::NOT_FOUND)),
    }
}

/// The songs of a `getPlaylist` playlist object, taken out of it.
fn take_entries(fields: &mut serde_json::Map<String, Value>) -> Vec<Value> {
    match fields.remove("entry") {
        Some(Value::Array(entries)) => entries,
        Some(one @ Value::Object(_)) => vec![one],
        _ => Vec::new(),
    }
}

/// Reads playlist `id` as the caller: Navidrome checks the sign-in and what the caller may see.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
pub(crate) async fn caller_view(
    state: &AppState,
    call: &SubsonicCall,
    id: &str,
    failure: &str,
) -> Result<CallerView, Response> {
    let mut fields =
        caller_playlist(state, &call.format, &call.proxy, &auth_pairs(call), id, failure).await?;
    let entries = take_entries(&mut fields);
    let text = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let owner = text("owner");
    let name = text("name");
    let changed = fields
        .get("changed")
        .and_then(Value::as_str)
        .and_then(parse_instant);
    let username = caller_name(state, call).await;
    let owned = !username.trim().is_empty() && eq_ignore_case(owner.trim(), username.trim());
    Ok(CallerView {
        fields,
        entries,
        owner,
        name,
        changed,
        username,
        owned,
    })
}

/// Who a request is, as the held service and a `Replaced` record know it.
fn held_caller(call: &SubsonicCall, username: &str) -> HeldCaller {
    HeldCaller {
        username: username.trim().to_string(),
        user: call
            .param("u")
            .map(str::trim)
            .filter(|user| !user.is_empty())
            .unwrap_or(username.trim())
            .to_string(),
        client: call.param("c").unwrap_or("").trim().to_string(),
    }
}

/// Whether `user`'s playlist named `name` refuses songs found online: one of their library
/// action or notice playlists. Their family action playlists take them, as requests.
pub(crate) fn refuses_outside(
    state: &AppState,
    proxy: &SubsonicProxyService,
    user: &str,
    name: &str,
) -> bool {
    !user.trim().is_empty()
        && LibraryActionPlaylistProvisioner::new(proxy.clone(), state.settings.clone())
            .with_family(state.family_store.clone())
            .upkeep_titles(user)
            .refuses_outside(name.trim())
}

/// Whether a built list brings a streamed song in.
pub(crate) fn brings_streamed(built: &[Built]) -> bool {
    built.iter().any(|song| match song {
        Built::Entry { entry, .. } => streams(entry),
        Built::Takeover(_) | Built::Skipped(_) => false,
    })
}

/// The refusals a song found online meets before anything is changed: the caller's upkeep
/// playlist (error 0), then a smart or synced playlist (error 50). None lets it go on.
async fn refuse_for_outside(
    state: &AppState,
    call: &SubsonicCall,
    id: &str,
    view: &CallerView,
) -> Option<Response> {
    let format = call.format.as_str();
    if refuses_outside(state, &call.proxy, &view.username, &view.name) {
        return Some(held_error(state, format, HeldError::Upkeep));
    }
    match state.navidrome_playlist_api.playlist_info(id).await {
        Ok(Some(info)) if info.smart || info.synced => {
            Some(held_error(state, format, HeldError::MadeByRules))
        }
        Ok(Some(_)) => None,
        Ok(None) => Some(held_error(state, format, HeldError::NotFound)),
        Err(_) => Some(held_error(state, format, HeldError::Busy)),
    }
}

// ---------------------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------------------

/// The entry's song as `getPlaylist` writes an outside song, with the cover its snapshot kept.
fn snapshot_json(state: &AppState, song: &Song, entry: &HeldEntry) -> Value {
    let mut fields = state.subsonic_response_builder.convert_song_to_json(song);
    if let Some(cover) = entry
        .song
        .as_ref()
        .and_then(|snapshot| snapshot.cover_art.clone())
        .filter(|cover| !cover.is_empty())
    {
        fields.insert("coverArt".into(), cover.into());
    }
    Value::Object(fields)
}

/// One song a held playlist serves its caller, with the place of its entry and the id it shows.
#[derive(Debug, Clone)]
pub(crate) struct ServedRow {
    pub entry: usize,
    pub id: String,
    pub song: Value,
}

/// The held entries as the caller is served them (the service's `walk`): a library entry
/// takes the next unused song Navidrome described to the caller with that id, else its outside
/// fallback; a streamed entry is its snapshot, else the catalog; an entry with neither is left
/// out.
pub(crate) async fn served_rows(
    state: &AppState,
    entries: &[HeldEntry],
    caller_songs: &[Value],
) -> Vec<ServedRow> {
    let caller_ids: Vec<String> = caller_songs
        .iter()
        .map(|song| {
            song.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let mut rows = Vec::with_capacity(entries.len());
    for pick in held_playlists::walk(entries, &caller_ids) {
        match pick {
            Pick::Library { entry, row } => rows.push(ServedRow {
                entry,
                id: caller_ids[row].clone(),
                song: caller_songs[row].clone(),
            }),
            Pick::Outside { entry, id } => {
                let held = &entries[entry];
                let song = match held_playlists::snapshot_song(held).filter(|song| song.id == id) {
                    Some(song) => Some(snapshot_json(state, &song, held)),
                    None => outside_song_json(state, &id).await,
                };
                if let Some(song) = song {
                    rows.push(ServedRow { entry, id, song });
                }
            }
        }
    }
    rows
}

/// The served rows the caller's answer keeps once the family answer rules ran on it (a kid
/// profile's explicit songs go): what `songIndexToRemove` and a replace count over.
pub(crate) fn visible_rows(
    state: &AppState,
    caller: Option<&FamilyCaller>,
    id: &str,
    rows: Vec<ServedRow>,
) -> Vec<ServedRow> {
    let Some(caller) = caller else {
        return rows;
    };
    let body = json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "playlist": {
        "id": id,
        "entry": rows.iter().map(|row| row.song.clone()).collect::<Vec<Value>>(),
    }}});
    let mut parameters = Parameters::new();
    parameters.insert("f".into(), "json".into());
    let context = AnswerContext {
        state,
        caller,
        endpoint: "getPlaylist",
        path: "/rest/getPlaylist",
        parameters: &parameters,
        content_type: Some("application/json"),
    };
    let mut answer = Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
    for rule in state.family_answer_rules.for_endpoint("getPlaylist") {
        answer = rule(&context, answer);
    }
    let listed: Vec<String> = serde_json::from_slice::<Value>(&answer)
        .ok()
        .and_then(|v| v["subsonic-response"]["playlist"]["entry"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|song| song.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut kept = Vec::with_capacity(listed.len());
    let mut rest = rows.into_iter();
    for id in listed {
        for row in rest.by_ref() {
            if row.id == id {
                kept.push(row);
                break;
            }
        }
    }
    kept
}

/// The caller's served list, worked out under the playlist's lock with a fresh read as the
/// caller: what the held service counts places over, and a plain playlist's `changed`.
struct CallerList {
    state: AppState,
    proxy: SubsonicProxyService,
    auth: Vec<(String, String)>,
    caller: Option<FamilyCaller>,
    id: String,
}

impl CallerList {
    fn of(state: &AppState, call: &SubsonicCall, id: &str) -> Arc<CallerList> {
        Arc::new(CallerList {
            state: state.clone(),
            proxy: call.proxy.clone(),
            auth: auth_pairs(call),
            caller: family_caller(&call.parts.extensions).cloned(),
            id: id.to_string(),
        })
    }

    /// The playlist's `changed` and songs as the caller reads them now; None when Navidrome did
    /// not answer.
    async fn read(&self) -> Option<(Option<DateTime<Utc>>, Vec<Value>)> {
        let mut fields = caller_playlist(
            &self.state,
            "json",
            &self.proxy,
            &self.auth,
            &self.id,
            UPDATE_FAILED,
        )
        .await
        .ok()?;
        let entries = take_entries(&mut fields);
        let changed = fields
            .get("changed")
            .and_then(Value::as_str)
            .and_then(parse_instant);
        Some((changed, entries))
    }
}

#[async_trait]
impl ServedView for CallerList {
    async fn served(&self, entries: &[HeldEntry]) -> Option<Vec<usize>> {
        let (_, songs) = self.read().await?;
        let rows = served_rows(&self.state, entries, &songs).await;
        Some(
            visible_rows(&self.state, self.caller.as_ref(), &self.id, rows)
                .into_iter()
                .map(|row| row.entry)
                .collect(),
        )
    }

    async fn changed(&self) -> Option<Option<DateTime<Utc>>> {
        self.read().await.map(|(changed, _)| changed)
    }
}

/// A held playlist's `getPlaylist` answer: the caller's row with `changed` the newer of
/// Navidrome's and the list's, its marks, and every song in its place. The builder counts the
/// songs and their length.
pub(crate) async fn serve_held(
    state: &AppState,
    call: &SubsonicCall,
    view: &CallerView,
    playlist: &HeldView,
) -> SubsonicReply {
    let rows = served_rows(state, &playlist.entries, &view.entries).await;
    let mut fields = view.fields.clone();
    let changed = playlist.served_changed(view.changed);
    fields.insert("changed".into(), super::playlists::utc_millis(&changed).into());
    RowMarks::for_caller(state, call, &view.username).mark_fields(&mut fields);
    let entries: Vec<Node> = rows.iter().map(|row| Node::from_value(&row.song)).collect();
    state
        .subsonic_response_builder
        .create_entries_playlist_response(&call.format, fields, &entries)
}

/// A plain playlist's answer from the caller's own read, with its marks.
fn plain_reply(state: &AppState, call: &SubsonicCall, view: &CallerView) -> SubsonicReply {
    let mut fields = view.fields.clone();
    RowMarks::for_caller(state, call, &view.username).mark_fields(&mut fields);
    let entries: Vec<Node> = view.entries.iter().map(Node::from_value).collect();
    state
        .subsonic_response_builder
        .create_entries_playlist_response(&call.format, fields, &entries)
}

/// `getPlaylist` of a playlist Octo holds: the caller's read first (sign-in, visibility, the
/// library songs as Navidrome describes them to the caller), then the held entries. None when it
/// turned out not to be held, for the plain relay.
pub(super) async fn get_held(state: &AppState, call: &SubsonicCall, id: &str) -> Option<Response> {
    let view = match caller_view(state, call, id, held_playlists::NOT_FOUND).await {
        Ok(view) => view,
        Err(answer) => return Some(answer),
    };
    match state.held_playlists.serve(id).await {
        Ok(Some(playlist)) => Some(serve_held(state, call, &view, &playlist).await.into_response()),
        Ok(None) => None,
        Err(failure) => Some(held_error(state, &call.format, failure)),
    }
}

/// The playlist as the caller reads it after a change, with the ids that were skipped.
async fn answer_playlist(state: &AppState, call: &SubsonicCall, id: &str, skipped: &[String]) -> Response {
    let format = call.format.as_str();
    let view = match caller_view(state, call, id, UPDATE_FAILED).await {
        Ok(view) => view,
        Err(answer) => return answer,
    };
    let reply = match state.held_playlists.view_of(id) {
        Some(playlist) => serve_held(state, call, &view, &playlist).await,
        None => plain_reply(state, call, &view),
    };
    with_skipped(reply, format, skipped)
}

// ---------------------------------------------------------------------------------------
// Changes
// ---------------------------------------------------------------------------------------

/// Runs a playlist change on its own task and answers what it answers.
pub(super) async fn route(
    state: AppState,
    call: SubsonicCall,
    id: String,
    change: PlaylistChange,
) -> Response {
    let format = call.format.clone();
    let answering = state.clone();
    let task = tokio::spawn(async move {
        match change {
            PlaylistChange::Create if id.trim().is_empty() => create(&state, &call).await,
            PlaylistChange::Create => replace(&state, &call, &id).await,
            PlaylistChange::Update => update(&state, &call, &id).await,
            PlaylistChange::Delete => delete(&state, &call, &id).await,
        }
    });
    match task.await {
        Ok(answer) => answer,
        Err(failure) => {
            warn!("A playlist change stopped: {failure}");
            error(&answering, &format, 0, UPDATE_FAILED)
        }
    }
}

/// `updatePlaylist`. A change Octo has no part in (library songs only, no
/// `octoSongToRemove`, a playlist that is not held, kept aside or holding a `Replaced` record) is
/// Navidrome's own and relayed as it came.
/// Otherwise: the caller's read and the owner check first, the entries built outside the lock,
/// the refusals for songs found online, then the held service classifies and applies the change
/// under the lock (removals over the served list, the name, comment and public relayed, songs
/// added at the end; or the caller's request relayed when the playlist stays plain).
async fn update(state: &AppState, call: &SubsonicCall, id: &str) -> Response {
    let format = call.format.as_str();
    let adds = values(call, "songIdToAdd");
    let index_values = values(call, "songIndexToRemove");
    let remove_songs = song_removals(call);
    if !remove_songs.is_empty() && !index_values.is_empty() {
        return error(state, format, 10, BOTH_REMOVALS);
    }
    if !looks_outside(state, &adds) && remove_songs.is_empty() && !has_part(state, id) {
        return relayed(state, call, plain_relay(call).await);
    }
    let remove_indexes: Vec<usize> = index_values
        .iter()
        .filter_map(|value| value.parse().ok())
        .collect();
    let view = match caller_view(state, call, id, UPDATE_FAILED).await {
        Ok(view) => view,
        Err(answer) => return answer,
    };
    if !view.owned {
        return held_error(state, format, HeldError::NotOwner);
    }
    let built = match state
        .held_playlists
        .build_entries(&view.owner, &adds, &state.held_playlists.record_ids(id))
        .await
    {
        Ok(built) => built,
        Err(failure) => return held_error(state, format, failure),
    };
    if brings_streamed(&built)
        && let Some(answer) = refuse_for_outside(state, call, id, &view).await
    {
        return answer;
    }
    let meta: Vec<(String, String)> = ["name", "comment", "public"]
        .iter()
        .filter_map(|key| call.param(key).map(|value| (key.to_string(), value.to_string())))
        .collect();
    let fields = (!meta.is_empty()).then(|| {
        let mut pairs = auth_pairs(call);
        pairs.push(("f".into(), call.format.clone()));
        pairs.push(("playlistId".into(), id.to_string()));
        pairs.extend(meta);
        relay_later(call, "rest/updatePlaylist".into(), pairs)
    });
    let request = UpdateRequest {
        id: id.to_string(),
        caller: held_caller(call, &view.username),
        owner: view.owner.clone(),
        remove_indexes,
        remove_songs,
        adds: built,
        fields,
        plain: plain_relay(call),
        view: CallerList::of(state, call, id),
    };
    match state.held_playlists.update(request).await {
        Ok(UpdateOutcome::Held { skipped, .. }) => with_skipped(
            state
                .subsonic_response_builder
                .create_response(format, "updatePlaylist"),
            format,
            &skipped,
        ),
        Ok(UpdateOutcome::Plain(answer)) => relayed(state, call, answer),
        Ok(UpdateOutcome::Relayed(answer)) => relayed(state, call, Some(answer)),
        Err(failure) => held_error(state, format, failure),
    }
}

/// `createPlaylist playlistId=`. A replace Octo has no part in (library songs only, no
/// stamp, a playlist that is not held, kept aside or holding a `Replaced` record) is relayed as it
/// came. Otherwise: the caller's read and
/// the owner check first, the songs built outside the lock (the ids the playlist answers to are
/// taken over, not built), the refusals, then the held service checks the stamp and replaces
/// under the lock.
async fn replace(state: &AppState, call: &SubsonicCall, id: &str) -> Response {
    let format = call.format.as_str();
    let sent = values(call, "songId");
    let stamp = call
        .param("octoIfStamp")
        .map(str::trim)
        .filter(|stamp| !stamp.is_empty());
    if !looks_outside(state, &sent) && stamp.is_none() && !has_part(state, id) {
        return relayed(state, call, plain_relay(call).await);
    }
    let if_stamp = match stamp {
        Some(stamp) => match parse_instant(stamp) {
            Some(at) => Some(at),
            None => return held_error(state, format, HeldError::Changed),
        },
        None => None,
    };
    let own_ids = call.param("octoOwnIds").is_some_and(|value| value.trim() == "1");
    let pairing = !own_ids
        && call
            .param("c")
            .is_some_and(|client| client.trim().eq_ignore_ascii_case("Octo"));
    let view = match caller_view(state, call, id, UPDATE_FAILED).await {
        Ok(view) => view,
        Err(answer) => return answer,
    };
    if !view.owned {
        return held_error(state, format, HeldError::NotOwner);
    }
    let songs = match state
        .held_playlists
        .build_entries(&view.owner, &sent, &state.held_playlists.takeover_ids(id))
        .await
    {
        Ok(songs) => songs,
        Err(failure) => return held_error(state, format, failure),
    };
    if brings_streamed(&songs)
        && let Some(answer) = refuse_for_outside(state, call, id, &view).await
    {
        return answer;
    }
    let request = ReplaceRequest {
        id: id.to_string(),
        caller: held_caller(call, &view.username),
        owner: view.owner.clone(),
        songs,
        own_ids,
        pairing,
        if_stamp,
        view: CallerList::of(state, call, id),
        plain: plain_relay(call),
    };
    match state.held_playlists.replace(request).await {
        Ok(ReplaceOutcome::Held { skipped, .. }) => answer_playlist(state, call, id, &skipped).await,
        Ok(ReplaceOutcome::Plain(answer)) => relayed(state, call, answer),
        Err(failure) => held_error(state, format, failure),
    }
}

/// `createPlaylist` without `playlistId`: a create with no song found online is relayed as
/// it came. One that holds one is built, refused for an upkeep title, and handed to the held
/// service, which saves the intent, relays the create with the library songs only and holds the
/// new playlist.
async fn create(state: &AppState, call: &SubsonicCall) -> Response {
    let format = call.format.as_str();
    let sent = values(call, "songId");
    if !looks_outside(state, &sent) {
        return relayed(state, call, plain_relay(call).await);
    }
    if !signed_in(state, call, format).await {
        return error(state, format, 40, WRONG_SIGN_IN);
    }
    let owner = caller_name(state, call).await;
    if owner.trim().is_empty() {
        return error(state, format, 40, WRONG_SIGN_IN);
    }
    let built = match state
        .held_playlists
        .build_entries(&owner, &sent, &HashSet::new())
        .await
    {
        Ok(built) => built,
        Err(failure) => return held_error(state, format, failure),
    };
    let skipped = Built::skipped(&built);
    let entries: Vec<HeldEntry> = built
        .into_iter()
        .filter_map(|song| match song {
            Built::Entry { entry, .. } => Some(entry),
            Built::Takeover(_) | Built::Skipped(_) => None,
        })
        .collect();
    let library: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry.library_id.clone())
        .collect();
    let name = call.param("name").unwrap_or("").trim().to_string();
    if !entries.iter().any(streams) {
        // Nothing found online is left: a plain playlist of the library songs.
        let mut pairs = auth_pairs(call);
        pairs.push(("f".into(), call.format.clone()));
        pairs.push(("name".into(), name));
        pairs.extend(library.into_iter().map(|id| ("songId".to_string(), id)));
        return match relay_later(call, "rest/createPlaylist".into(), pairs).await {
            Some(answer) => with_skipped(
                SubsonicReply::file(
                    answer.body,
                    answer.content_type.as_deref().unwrap_or("application/json"),
                ),
                format,
                &skipped,
            ),
            None => error(state, format, 0, UPDATE_FAILED),
        };
    }
    if refuses_outside(state, &call.proxy, &owner, &name) {
        return held_error(state, format, HeldError::Upkeep);
    }
    let Some(before) = same_name_ids(call, &owner, &name).await else {
        return error(state, format, 0, UPDATE_FAILED);
    };
    let made: Arc<Mutex<Option<String>>> = Arc::default();
    let request = CreateRequest {
        owner: owner.clone(),
        name: name.clone(),
        entries,
        skipped,
        before,
        relay: relay_create(call, name, library, Arc::clone(&made)),
    };
    match state.held_playlists.create_with_outside(request).await {
        Ok(CreateOutcome::Held { skipped, .. }) => {
            let new_id = made.lock().clone();
            match new_id {
                Some(new_id) => answer_playlist(state, call, &new_id, &skipped).await,
                None => error(state, format, 0, UPDATE_FAILED),
            }
        }
        Ok(CreateOutcome::Relayed(Some(answer))) => passed_error(state, format, &answer.body),
        Ok(CreateOutcome::Relayed(None)) => error(state, format, 0, UPDATE_FAILED),
        Err(failure) => held_error(state, format, failure),
    }
}

/// `createPlaylist` relayed as the caller with only `name` and the library songs, in JSON: the new
/// playlist's id (also kept in `made`), or Navidrome's answer.
fn relay_create(
    call: &SubsonicCall,
    name: String,
    library: Vec<String>,
    made: Arc<Mutex<Option<String>>>,
) -> BoxFuture<'static, Result<String, Option<RelayAnswer>>> {
    let proxy = call.proxy.clone();
    let mut ask = auth_pairs(call);
    ask.push(("f".into(), "json".into()));
    ask.push(("name".into(), name));
    ask.extend(library.into_iter().map(|id| ("songId".to_string(), id)));
    async move {
        let Some(relay) = proxy
            .relay_safe("rest/createPlaylist", ask.iter().map(|(k, v)| (k, v)))
            .await
        else {
            return Err(None);
        };
        let ok = is_successful_subsonic_response(&relay.body, "json");
        let id = ok
            .then(|| {
                serde_json::from_slice::<Value>(&relay.body)
                    .ok()?
                    .pointer("/subsonic-response/playlist/id")?
                    .as_str()
                    .map(str::to_string)
            })
            .flatten();
        match id {
            Some(id) => {
                *made.lock() = Some(id.clone());
                Ok(id)
            }
            None => Err(Some(RelayAnswer {
                ok,
                body: relay.body.to_vec(),
                content_type: relay.content_type,
            })),
        }
    }
    .boxed()
}

/// The ids of the owner's playlists named `name`, as the caller's `getPlaylists` lists them. None
/// when Navidrome did not answer.
async fn same_name_ids(call: &SubsonicCall, owner: &str, name: &str) -> Option<Vec<String>> {
    let mut ask = auth_pairs(call);
    ask.push(("f".into(), "json".into()));
    let relay = call
        .proxy
        .relay_safe("rest/getPlaylists", ask.iter().map(|(k, v)| (k, v)))
        .await?;
    if !is_successful_subsonic_response(&relay.body, "json") {
        return None;
    }
    Some(
        listed_rows(&relay.body, "json")
            .into_iter()
            .filter(|row| row.name == name && eq_ignore_case(&row.owner, owner))
            .map(|row| row.id)
            .collect(),
    )
}

/// `deletePlaylist`: relayed under the lock by the held service, which drops the held file
/// on ok; once the lock is released the caller's placement of the playlist goes too.
async fn delete(state: &AppState, call: &SubsonicCall, id: &str) -> Response {
    let format = call.format.as_str();
    let owner = caller_name(state, call).await;
    match state.held_playlists.delete(id, plain_relay(call)).await {
        Ok(answer) => {
            if answer.as_ref().is_some_and(|answer| answer.ok) && !owner.trim().is_empty() {
                state
                    .playlist_folders
                    .remove_places(&owner, &[id.to_string()], Utc::now());
            }
            relayed(state, call, answer)
        }
        Err(failure) => held_error(state, format, failure),
    }
}

// ---------------------------------------------------------------------------------------
// After a listing
// ---------------------------------------------------------------------------------------

/// After an owner's `getPlaylists`: the gone check, whose confirmed playlists' placements
/// leave the folder store, and the creation attach pass, both in the background.
pub(crate) fn after_listing(state: &AppState, owner: &str, listed: Vec<ListedPlaylist>) {
    let owner = owner.trim().to_string();
    if owner.is_empty() {
        return;
    }
    let ids: Vec<String> = listed.iter().map(|row| row.id.clone()).collect();
    let placed: Vec<String> = state
        .playlist_folders
        .state(&owner)
        .places
        .into_iter()
        .map(|(playlist, _)| playlist)
        .collect();
    if let Some(check) = state.held_playlists.check_gone(&owner, ids, placed) {
        let folders = Arc::clone(&state.playlist_folders);
        let owner = owner.clone();
        tokio::spawn(async move {
            if let Ok(report) = check.await
                && !report.confirmed.is_empty()
            {
                folders.remove_places(&owner, &report.confirmed, Utc::now());
            }
        });
    }
    let held = Arc::clone(&state.held_playlists);
    tokio::spawn(async move { held.attach_pass(Some(&owner), listed).await });
}

// ---------------------------------------------------------------------------------------
// Sync catalog
// ---------------------------------------------------------------------------------------

/// Whether anyone's playlists may hold songs found online for a sync catalog.
pub(crate) fn may_have_catalog_songs(state: &AppState) -> bool {
    !state.held_playlists.is_empty() || state.settings.current().family.enabled
}

/// A family playlist's outside song from the catalog, filed under the id the playlist lists.
async fn family_catalog_song(state: &AppState, id: &str) -> Option<Song> {
    let (external, provider, external_id) = state.local_library.parse_song_id(id);
    if !external {
        return None;
    }
    let mut song = state
        .metadata_service
        .get_song(
            provider.as_deref().unwrap_or_default(),
            external_id.as_deref().unwrap_or_default(),
        )
        .await?;
    song.id = id.to_string();
    Some(song)
}

/// The songs found online in the playlists `username` can see, for the sync catalog: held
/// playlists' streamed entries (from the caller's own `getPlaylists`) and family playlists'
/// outside entries (from the family visibility rules), each under the id its playlist lists. A
/// kid profile is not handed explicit ones. The fingerprint is a hash of the ids, so any add,
/// bind, removal, release, delete or visibility change moves it; empty when there are no songs.
pub(crate) async fn sync_catalog_songs(state: &AppState, call: &SubsonicCall, username: &str) -> HeldCatalog {
    if username.trim().is_empty() || !may_have_catalog_songs(state) {
        return HeldCatalog::default();
    }
    let settings = state.settings.current();
    let family_on = settings.family.enabled;
    let unrated = settings.family.kid_unrated_is_explicit;
    let viewer = family_caller(&call.parts.extensions)
        .cloned()
        .or_else(|| family_on.then(|| caller_for(&state.family_store, username)));
    let kid = viewer.as_ref().is_some_and(kid_filter::is_kid);
    let blocked = |song: &Song| {
        kid && ExplicitMark::from_catalog(song.explicit_content_lyrics).blocks_clean_only(unrated)
    };
    let mut songs: Vec<Song> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if !state.held_playlists.is_empty() {
        let mut ask = auth_pairs(call);
        ask.push(("f".into(), "json".into()));
        let relay = call
            .proxy
            .relay_safe("rest/getPlaylists", ask.iter().map(|(k, v)| (k, v)))
            .await;
        if let Some(relay) = relay.filter(|relay| is_successful_subsonic_response(&relay.body, "json")) {
            for row in listed_rows(&relay.body, "json") {
                if !state.held_playlists.contains(&row.id) {
                    continue;
                }
                let Some(view) = state.held_playlists.view_of(&row.id) else {
                    continue;
                };
                for song in view
                    .entries
                    .iter()
                    .filter(|entry| streams(entry))
                    .filter_map(held_playlists::snapshot_song)
                {
                    if !blocked(&song) && seen.insert(song.id.clone()) {
                        songs.push(song);
                    }
                }
            }
        }
    }
    if family_on && let Some(viewer) = &viewer {
        let outside: Vec<String> = family_playlists::visible_playlists(&state.family_playlists, viewer)
            .iter()
            .flat_map(|playlist| family_playlists::shown_entries(playlist, viewer))
            .filter_map(|shown| match shown.serve {
                Serve::Outside(id) => Some(id),
                Serve::Library { .. } => None,
            })
            .filter(|id| seen.insert(id.clone()))
            .collect();
        let found: Vec<Option<Song>> = futures::stream::iter(outside)
            .map(|id| async move { family_catalog_song(state, &id).await })
            .buffered(CATALOG_SONGS_AT_ONCE)
            .collect()
            .await;
        songs.extend(found.into_iter().flatten().filter(|song| !blocked(song)));
    }
    if songs.is_empty() {
        return HeldCatalog::default();
    }
    let mut ids: Vec<&str> = songs.iter().map(|song| song.id.as_str()).collect();
    ids.sort_unstable();
    let mut hasher = DefaultHasher::new();
    ids.hash(&mut hasher);
    let fingerprint = format!("{:x}", hasher.finish());
    HeldCatalog { songs, fingerprint }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::playlists::held_store::SongSnapshot;

    fn entry(library: Option<&str>, outside: Option<&str>, title: &str) -> HeldEntry {
        HeldEntry {
            library_id: library.map(str::to_string),
            outside_id: outside.map(str::to_string),
            provider: outside.map(|_| "deezer".to_string()),
            external_id: outside.map(|id| id.trim_start_matches("ext-deezer-song-").to_string()),
            key: None,
            seed_id: None,
            seed: None,
            song: Some(SongSnapshot {
                title: title.into(),
                artist: "Somebody".into(),
                artist_id: None,
                album: "Elsewhere".into(),
                album_id: None,
                seconds: Some(200),
                track: None,
                disc: None,
                year: None,
                genre: None,
                isrcs: Vec::new(),
                explicit_status: None,
                cover_art: Some("cover-1".into()),
            }),
            added: Utc::now(),
        }
    }

    #[tokio::test]
    async fn a_bound_entry_the_caller_cannot_read_is_served_by_its_outside_id() {
        let entries = vec![
            entry(Some("s-a"), None, "Alpha"),
            entry(Some("s-theirs"), Some("ext-deezer-song-1"), "Glory"),
            entry(None, Some("ext-deezer-song-2"), "Streamed"),
        ];
        let caller = vec![json!({"id": "s-a", "title": "Alpha"})];
        let state = AppState::for_tests(Default::default());
        let rows = served_rows(&state, &entries, &caller).await;
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["s-a", "ext-deezer-song-1", "ext-deezer-song-2"]);
        assert_eq!(rows[2].entry, 2);
        assert_eq!(rows[1].song["coverArt"], "cover-1");
        assert_eq!(rows[1].song["isExternal"], true);
    }

    #[tokio::test]
    async fn without_a_family_caller_every_served_row_is_visible() {
        let state = AppState::for_tests(Default::default());
        let rows = vec![ServedRow {
            entry: 0,
            id: "s-a".into(),
            song: json!({"id": "s-a"}),
        }];
        assert_eq!(visible_rows(&state, None, "p", rows).len(), 1);
    }
}
