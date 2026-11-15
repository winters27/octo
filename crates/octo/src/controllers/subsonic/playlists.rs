//! `getPlaylists`, `getPlaylist`, and the playlist and internet radio mutations, which refuse
//! Octo's own read-only ids.
//!
//! While Family is on, family playlists (`of` ids) are listed for the owner and every member,
//! served per viewer, and changed through `updatePlaylist` and `deletePlaylist` by whoever
//! may (see [`crate::services::family::family_playlists`]).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use indexmap::IndexMap;
use octo_core::charts::chart_settings;
use octo_core::common::dotnet;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::json::dom::Node;
use octo_core::library::for_you_lists;
use octo_core::settings::GeneratedPlaylistSettings;
use octo_core::settings::NoticeKind;
use octo_subsonic::SubsonicReply;
use octo_subsonic::subsonic_request_parser::Parameters;
use octo_subsonic::subsonic_response_builder::chart_playlist_comment;
use octo_subsonic::xml::XElement;
use serde_json::{Value, json};
use tracing::warn;

use super::action_helpers::{
    RequestAborted, SubsonicCall, bootstrap_radio_profile, is_octo_playlist_id,
    is_successful_subsonic_response, materialize_station, playlist_names, playlist_stations,
    queue_refresh_if_stale, serve_station_songs, suggested_sources, xml_value,
};
use super::caller_checks::listener_name_of;
use crate::app::AppState;
use crate::middleware::family_gate::{GateReading, Unverified};
use crate::services::family::audio_gate::ExplicitMark;
use crate::services::family::family_playlists::{
    self, FAMILY_OWNER, NewEntry, PlaylistError, SHARED_SCOPE, Serve, ShownEntry, personal_scope,
};
use crate::services::family::rules::AnswerContext;
use crate::services::family::{FamilyCaller, FamilyPlaylist, FamilyPlaylistEntry, family_caller, kid_filter};
use crate::services::library::library_action_playlist_provisioner::{
    REMOVE_PLAYLIST, REQUEST_FLAC_PLAYLIST, REQUEST_PLAYLIST,
};
use crate::services::library::{GeneratedPlaylist, LibraryActionPlaylistProvisioner};
use crate::services::playlists::held_playlists::ListedPlaylist;
use crate::services::playlists::held_store::HeldEntry;
use crate::services::subsonic::SubsonicResponseBuilderExt;
use octo_core::family::abilities::RequestQuality;
use tracing::info;

/// How many songs of a family playlist are read at once.
const FAMILY_SONGS_AT_ONCE: usize = 8;

pub async fn get_playlists(State(state): State<AppState>, req: Request) -> Response {
    let caller = family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.clone();
    let builder = &state.subsonic_response_builder;
    let relay = call.proxy.relay_safe("rest/getPlaylists", &call.parameters).await;
    let relay = match relay {
        Some(relay) if !relay.body.is_empty() && is_successful_subsonic_response(&relay.body, &format) => {
            relay
        }
        Some(relay) if !relay.body.is_empty() => {
            return call.file(&relay.body, relay.content_type.as_deref());
        }
        _ => {
            return builder
                .create_error(&format, 0, "Unable to authenticate with Navidrome")
                .into_response();
        }
    };

    let username = listener_name_of(&state, &call.parameters, &call.proxy, &call.parts.headers).await;

    // The row pass: this listener's notice playlists carry Octo's glyph cover, their notice and
    // upkeep playlists are marked, and a playlist Octo holds counts the songs found online it
    // holds, as this viewer is shown them.
    let listed = listed_rows(&relay.body, &format);
    let marks = RowMarks::for_caller(&state, &call, &username).with_held(&state, caller.as_ref(), &listed);
    let mut relay = relay;
    if let Some(body) = marks.apply(&relay.body, &format) {
        relay.body = body.into();
    }

    // Navidrome answered ok above, so `u` is authenticated. Ensure this user's action
    // playlists exist, using the body we already have so the common case costs no extra
    // request. What is missing is worked out here, without I/O; only creating runs apart
    // (it must never delay the listing), and nothing is spawned when all are there.
    let names = playlist_names(Some(&relay.body), &format);
    let provisioner = LibraryActionPlaylistProvisioner::new(call.proxy.clone(), state.settings.clone())
        .with_family(state.family_store.clone());
    let missing = provisioner.missing(&username, &names);
    if !missing.is_empty() {
        let ensured_user = username.clone();
        let ensured_parameters = call.parameters.clone();
        tokio::spawn(async move {
            provisioner
                .create(&ensured_user, missing, &ensured_parameters)
                .await;
        });
    }

    bootstrap_radio_profile(&state, &call.proxy, &username, &call.parameters).await;
    let stations = playlist_stations(&state, &username);
    queue_refresh_if_stale(&state, &username);
    let mix_settings = state.settings.current().generated_playlists.clone();
    // The mixes, and Made for you, which is on by default whatever the mixes switch says.
    let generated = if mix_settings.enabled || mix_settings.any_for_you() {
        state.generated_playlists.list(&username, &call.parameters).await
    } else {
        Vec::new()
    };
    // The chart lists, on whether or not the mixes are: after the stations, before the mixes.
    let charts = state.chart_playlists.list(&username, &call.parameters).await;
    // Liked Songs, listed before every other playlist.
    let liked = state
        .generated_playlists
        .liked_row(&username, &call.parameters)
        .await
        .map(|row| liked_playlist_fields(builder, &row, &mix_settings));
    let family = family_rows(&state, caller.as_ref()).await;
    let import_owner = state
        .request_identity
        .username(&call.parameters, &call.proxy)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    // The owner's held playlists missing from this listing are checked, and creations left
    // unfinished are attached, in the background.
    super::held::after_listing(&state, &import_owner, listed);
    let imports = state.imports.streaming_lists(&import_owner);
    if stations.is_empty()
        && family.is_empty()
        && charts.is_empty()
        && generated.is_empty()
        && liked.is_none()
        && imports.is_empty()
    {
        return call.file(&relay.body, relay.content_type.as_deref());
    }

    let rows: Vec<serde_json::Map<String, Value>> = stations
        .iter()
        .map(|station| builder.radio_playlist_fields(station))
        .chain(family)
        .chain(imports.iter().map(import_row_fields))
        .chain(charts.iter().map(|list| chart_playlist_fields(builder, list)))
        .chain(
            generated
                .iter()
                .map(|mix| builder.generated_playlist_fields(mix, &mix_settings)),
        )
        .collect();
    let merged = if format.eq_ignore_ascii_case("json") {
        merge_json(
            &relay.body,
            "playlists",
            "playlist",
            rows.into_iter().map(Value::Object),
        )
        .and_then(|body| match &liked {
            Some(row) => prepend_json(&body, "playlists", "playlist", Value::Object(row.clone())),
            None => Some(body),
        })
        .map(|body| SubsonicReply::file(body, "application/json"))
    } else {
        merge_xml(
            &relay.body,
            "playlists",
            "playlist",
            rows.iter().map(field_attributes),
        )
        .and_then(|body| match &liked {
            Some(row) => prepend_xml(&body, "playlists", "playlist", field_attributes(row)),
            None => Some(body),
        })
        .map(|body| SubsonicReply::file(body, "application/xml"))
    };
    match merged {
        Some(reply) => reply.into_response(),
        None => {
            warn!("Could not merge Radio stations and mixes into getPlaylists");
            call.file(&relay.body, relay.content_type.as_deref())
        }
    }
}

/// A chart list as a playlist row: the keys a mix has, read-only, with its own comment and its
/// real song count.
pub(crate) fn chart_playlist_fields(
    builder: &octo_subsonic::SubsonicResponseBuilder,
    list: &GeneratedPlaylist,
) -> serde_json::Map<String, Value> {
    let wide = GeneratedPlaylistSettings {
        track_count: 500,
        ..Default::default()
    };
    let mut fields = builder.generated_playlist_fields(list, &wide);
    fields.insert("songCount".into(), list.pool_size.into());
    let (chart, pinned) = chart_settings::list_from_key(&list.key).unwrap_or_default();
    let kind = chart_settings::kind_word(&chart).unwrap_or("overall");
    let mut genre = list.name.strip_prefix("Top ").unwrap_or(&list.name);
    if pinned.is_some() {
        genre = genre.rsplit_once(" · ").map_or(genre, |(name, _)| name);
    }
    let country = pinned.as_deref().map(chart_settings::country_name);
    fields.insert(
        "comment".into(),
        chart_playlist_comment(kind, genre, country.as_deref()).into(),
    );
    // octoCoverTitle: the words the Octo apps put on the list's cover in place of its name.
    if let Some(title) = super::media::chart_cover_words(&chart, pinned.as_deref(), &list.name) {
        fields.insert("octoCoverTitle".into(), title.into());
    }
    fields
}

/// Where a notice playlist's cover is drawn: `getCoverArt` paints it with the notice's glyph.
pub(crate) const NOTICE_COVER_PREFIX: &str = "oc-notice-";

/// The notice kinds a playlist row can be marked with, as `octoNotice` names them.
pub(crate) const NOTICE_REVIEW: &str = "review";
pub(crate) const NOTICE_DUPLICATES: &str = "duplicates";

/// The kind's word as rows and cover ids carry it.
pub(crate) fn notice_word(kind: NoticeKind) -> &'static str {
    match kind {
        NoticeKind::Review => NOTICE_REVIEW,
        NoticeKind::Duplicates => NOTICE_DUPLICATES,
    }
}

/// The notice kind a word names.
pub(crate) fn notice_kind(word: &str) -> Option<NoticeKind> {
    match word {
        NOTICE_REVIEW => Some(NoticeKind::Review),
        NOTICE_DUPLICATES => Some(NoticeKind::Duplicates),
        _ => None,
    }
}

/// The notice playlists that are on, by title, with their kind's word. Empty for no listener.
fn notice_titles(state: &AppState, username: &str) -> Vec<(String, &'static str)> {
    if username.trim().is_empty() {
        return Vec::new();
    }
    let settings = state.settings.current().library_actions.clone();
    settings
        .enabled_notice_kinds()
        .into_iter()
        .map(|kind| (settings.notice_title(kind), notice_word(kind)))
        .collect()
}

/// The notice word for a playlist row: the listener's own playlist named like a notice that is on.
fn notice_of(
    owner: Option<&str>,
    name: Option<&str>,
    username: &str,
    notices: &[(String, &'static str)],
) -> Option<&'static str> {
    let (owner, name) = (owner?, name?);
    if !dotnet::eq_ignore_case(owner, username) {
        return None;
    }
    notices
        .iter()
        .find(|(title, _)| dotnet::eq_ignore_case(title, name))
        .map(|(_, word)| *word)
}

/// The playlist rows of a `getPlaylists` answer: id, name and owner of each.
pub(super) fn listed_rows(body: &[u8], format: &str) -> Vec<ListedPlaylist> {
    let mut rows = Vec::new();
    if format.eq_ignore_ascii_case("json") {
        let Ok(root) = serde_json::from_slice::<Value>(body) else {
            return rows;
        };
        let list = &root["subsonic-response"]["playlists"]["playlist"];
        let items: Vec<&Value> = match list {
            Value::Array(items) => items.iter().collect(),
            Value::Object(_) => vec![list],
            _ => Vec::new(),
        };
        for item in items {
            let text = |name: &str| {
                item.get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let id = text("id");
            if !id.is_empty() {
                rows.push(ListedPlaylist {
                    id,
                    name: text("name"),
                    owner: text("owner"),
                    created: super::held::parse_instant(&text("created")),
                });
            }
        }
        return rows;
    }
    let Ok(root) = XElement::parse(&String::from_utf8_lossy(body)) else {
        return rows;
    };
    for holder in root.elements().filter(|element| element.name == "playlists") {
        for row in holder.elements().filter(|row| row.name == "playlist") {
            let text = |name: &str| row.attribute(name).unwrap_or_default().to_string();
            let id = text("id");
            if !id.is_empty() {
                rows.push(ListedPlaylist {
                    id,
                    name: text("name"),
                    owner: text("owner"),
                    created: super::held::parse_instant(&text("created")),
                });
            }
        }
    }
    rows
}

/// A held playlist's streamed songs as one viewer is shown them, and when its list last changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HeldRow {
    pub songs: i64,
    pub seconds: i64,
    pub changed: DateTime<Utc>,
}

/// The streamed entries a viewer is shown of a held playlist whose list last changed at
/// `changed`: a kid profile skips the explicit ones, from each snapshot's explicit status, as
/// family playlist totals do.
pub(super) fn held_row(
    entries: &[HeldEntry],
    changed: DateTime<Utc>,
    kid: bool,
    unrated_is_explicit: bool,
) -> HeldRow {
    let mut row = HeldRow {
        songs: 0,
        seconds: 0,
        changed,
    };
    for entry in entries {
        if entry.library_id.is_some() || entry.outside_id.is_none() {
            continue;
        }
        let status = entry
            .song
            .as_ref()
            .and_then(|song| song.explicit_status.as_deref())
            .unwrap_or("");
        if kid && ExplicitMark::from_status(status).blocks_clean_only(unrated_is_explicit) {
            continue;
        }
        row.songs += 1;
        row.seconds += i64::from(
            entry
                .song
                .as_ref()
                .and_then(|song| song.seconds)
                .unwrap_or(0)
                .max(0),
        );
    }
    row
}

/// One field the row pass sets.
enum RowField {
    Text(String),
    Flag,
    Number(i64),
}

impl RowField {
    fn node(self) -> Node {
        match self {
            RowField::Text(text) => Node::String(text),
            RowField::Flag => Node::Bool(true),
            RowField::Number(number) => Node::Number(number.to_string()),
        }
    }

    fn text(self) -> String {
        match self {
            RowField::Text(text) => text,
            RowField::Flag => "true".to_string(),
            RowField::Number(number) => number.to_string(),
        }
    }

    fn value(self) -> Value {
        match self {
            RowField::Text(text) => Value::String(text),
            RowField::Flag => Value::Bool(true),
            RowField::Number(number) => number.into(),
        }
    }
}

/// A number field of a row as JSON or XML writes it, read whole.
fn whole_number(text: &str) -> i64 {
    text.trim().parse::<f64>().map_or(0, |number| number as i64)
}

/// What the row pass writes on one caller's playlist rows: the notice playlists' glyph cover and
/// `octoNotice`, `octoUpkeep` on the caller's own upkeep playlists (notices included), and the
/// streamed songs of the playlists Octo holds in `songCount`, `duration` and `changed`.
pub(super) struct RowMarks {
    username: String,
    notices: Vec<(String, &'static str)>,
    upkeep: crate::services::library::library_action_playlist_provisioner::UpkeepTitles,
    held: HashMap<String, HeldRow>,
}

impl RowMarks {
    /// The caller's notice and upkeep titles; no held rows.
    pub(super) fn for_caller(state: &AppState, call: &SubsonicCall, username: &str) -> RowMarks {
        let upkeep = if username.trim().is_empty() {
            Default::default()
        } else {
            LibraryActionPlaylistProvisioner::new(call.proxy.clone(), state.settings.clone())
                .with_family(state.family_store.clone())
                .upkeep_titles(username)
        };
        RowMarks {
            username: username.to_string(),
            notices: notice_titles(state, username),
            upkeep,
            held: HashMap::new(),
        }
    }

    /// With the streamed songs of every listed playlist Octo holds, as `viewer` is shown them.
    pub(super) fn with_held(
        mut self,
        state: &AppState,
        viewer: Option<&FamilyCaller>,
        rows: &[ListedPlaylist],
    ) -> RowMarks {
        let kid = viewer.is_some_and(kid_filter::is_kid);
        let unrated = state.settings.current().family.kid_unrated_is_explicit;
        for row in rows.iter().filter(|row| state.held_playlists.contains(&row.id)) {
            if let Some(view) = state.held_playlists.view_of(&row.id) {
                self.held.insert(
                    row.id.clone(),
                    held_row(&view.entries, view.changed, kid, unrated),
                );
            }
        }
        self
    }

    fn is_empty(&self) -> bool {
        self.notices.is_empty() && self.upkeep.is_empty() && self.held.is_empty()
    }

    /// The fields to set on one row, from what the row says now.
    fn changes(
        &self,
        id: Option<&str>,
        owner: Option<&str>,
        name: Option<&str>,
        songs: Option<i64>,
        seconds: Option<i64>,
        changed: Option<&str>,
    ) -> Vec<(&'static str, RowField)> {
        let mut out = Vec::new();
        if let Some(word) = notice_of(owner, name, &self.username, &self.notices) {
            out.push(("coverArt", RowField::Text(format!("{NOTICE_COVER_PREFIX}{word}"))));
            out.push(("octoNotice", RowField::Text(word.to_string())));
            out.push(("octoUpkeep", RowField::Flag));
            let notice = crate::services::library::library_action_playlist_provisioner::UpkeepKind::Notice;
            out.push(("octoUpkeepKind", RowField::Text(notice.wire().to_string())));
        } else if let (Some(owner), Some(name)) = (owner, name)
            && !self.username.trim().is_empty()
            && dotnet::eq_ignore_case(owner, &self.username)
            && let Some(kind) = self.upkeep.kind(name)
        {
            out.push(("octoUpkeep", RowField::Flag));
            out.push(("octoUpkeepKind", RowField::Text(kind.wire().to_string())));
        }
        if let Some(held) = id.and_then(|id| self.held.get(id)) {
            out.push(("songCount", RowField::Number(songs.unwrap_or(0) + held.songs)));
            out.push(("duration", RowField::Number(seconds.unwrap_or(0) + held.seconds)));
            let listed = changed.and_then(super::held::parse_instant);
            let newest = listed.map_or(held.changed, |at| at.max(held.changed));
            out.push(("changed", RowField::Text(utc_millis(&newest))));
        }
        out
    }

    /// The row pass over a getPlaylists or getPlaylist answer. None when nothing was marked, or
    /// the body is not one this reads.
    pub(super) fn apply(&self, body: &[u8], format: &str) -> Option<Vec<u8>> {
        if self.is_empty() {
            return None;
        }
        if format.eq_ignore_ascii_case("json") {
            let mut root = Node::parse(std::str::from_utf8(body).ok()?).ok()?;
            let response = root
                .as_object_mut()?
                .get_mut("subsonic-response")?
                .as_object_mut()?;
            let mut rows: Vec<&mut IndexMap<String, Node>> = Vec::new();
            for (key, value) in response.iter_mut() {
                match (key.as_str(), value) {
                    ("playlists", Node::Object(holder)) => match holder.get_mut("playlist") {
                        Some(Node::Array(list)) => {
                            rows.extend(list.iter_mut().filter_map(Node::as_object_mut))
                        }
                        Some(Node::Object(one)) => rows.push(one),
                        _ => {}
                    },
                    ("playlist", Node::Object(one)) => rows.push(one),
                    _ => {}
                }
            }
            let mut marked = false;
            for row in rows {
                let number = |name: &str| match row.get(name) {
                    Some(Node::Number(text)) => Some(whole_number(text)),
                    _ => None,
                };
                let changes = self.changes(
                    row.get("id").and_then(Node::as_str),
                    row.get("owner").and_then(Node::as_str),
                    row.get("name").and_then(Node::as_str),
                    number("songCount"),
                    number("duration"),
                    row.get("changed").and_then(Node::as_str),
                );
                for (name, field) in changes {
                    row.insert(name.into(), field.node());
                    marked = true;
                }
            }
            return marked.then(|| root.to_json_string(false).into_bytes());
        }
        if !format.eq_ignore_ascii_case("xml") {
            return None;
        }
        let mut root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
        let mut marked = false;
        let mut mark = |row: &mut XElement| {
            let changes = self.changes(
                row.attribute("id"),
                row.attribute("owner"),
                row.attribute("name"),
                row.attribute("songCount").map(whole_number),
                row.attribute("duration").map(whole_number),
                row.attribute("changed"),
            );
            for (name, field) in changes {
                row.set_attr(name, field.text());
                marked = true;
            }
        };
        for element in root.elements_mut() {
            if element.name == "playlists" {
                for row in element.elements_mut().filter(|row| row.name == "playlist") {
                    mark(row);
                }
            } else if element.name == "playlist" {
                mark(element);
            }
        }
        marked.then(|| root.to_xml_string().into_bytes())
    }

    /// The notice and upkeep marks on a row built as fields. Counts are left to the builder,
    /// which counts the songs it writes.
    pub(super) fn mark_fields(&self, fields: &mut serde_json::Map<String, Value>) {
        let owner = fields.get("owner").and_then(Value::as_str).map(str::to_string);
        let name = fields.get("name").and_then(Value::as_str).map(str::to_string);
        for (key, field) in self.changes(None, owner.as_deref(), name.as_deref(), None, None, None) {
            fields.insert(key.to_string(), field.value());
        }
    }
}

/// An imported snapshot's playlist row, marked `octoImport` so Octo's apps keep it read-only.
fn import_row_fields(list: &crate::services::imports::ImportList) -> serde_json::Map<String, Value> {
    let mut fields = crate::services::imports::import_streaming::playlist_fields(list);
    fields.insert("octoImport".into(), true.into());
    fields
}

/// Liked Songs as a playlist row: the keys a mix has, read-only, with its real song count and
/// `octoList: "liked"` so the Octo apps put it first on Home and draw its heart cover.
pub(crate) fn liked_playlist_fields(
    builder: &octo_subsonic::SubsonicResponseBuilder,
    list: &GeneratedPlaylist,
    settings: &GeneratedPlaylistSettings,
) -> serde_json::Map<String, Value> {
    let mut fields = builder.generated_playlist_fields(list, settings);
    fields.insert("songCount".into(), list.pool_size.into());
    fields.insert("duration".into(), (i64::from(list.pool_size) * 210).into());
    fields.insert("comment".into(), "Your hearted songs, newest first".into());
    // Always current, so there is no time it is good until.
    fields.remove("validUntil");
    fields.insert("octoList".into(), for_you_lists::LIKED_KIND.into());
    fields
}

/// [`merge_json`]'s row put first instead of last.
fn prepend_json(body: &[u8], container: &str, row: &str, value: Value) -> Option<Vec<u8>> {
    let mut root = Node::parse(std::str::from_utf8(body).ok()?).ok()?;
    let holder = root
        .as_object_mut()?
        .get_mut("subsonic-response")?
        .as_object_mut()?
        .get_mut(container)?
        .as_object_mut()?;
    let Some(Node::Array(list)) = holder.get_mut(row) else {
        return None;
    };
    list.insert(0, to_node(&value));
    Some(root.to_json_string(false).into_bytes())
}

/// [`merge_xml`]'s row put first instead of last.
fn prepend_xml(
    body: &[u8],
    container: &str,
    row: &str,
    attributes: Vec<(String, String)>,
) -> Option<Vec<u8>> {
    let mut root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    let ns = root.namespace.clone();
    let holder = root.elements_mut().find(|element| element.name == container)?;
    let mut element = XElement::in_namespace(ns.as_deref(), row);
    for (name, value) in attributes {
        element.set_attr(name, value);
    }
    holder.add_first(vec![element]);
    Some(root.to_xml_string().into_bytes())
}

/// The family playlists as playlist rows for this caller: none while Family is off or for an
/// account outside the family.
async fn family_rows(state: &AppState, caller: Option<&FamilyCaller>) -> Vec<serde_json::Map<String, Value>> {
    if !state.settings.current().family.enabled {
        return Vec::new();
    }
    let Some(caller) = caller else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for playlist in family_playlists::visible_playlists(&state.family_playlists, caller) {
        let (count, seconds) = family_playlist_totals(state, &playlist, caller).await;
        rows.push(family_playlist_fields(&playlist, caller, count, seconds));
    }
    rows
}

/// What a family playlist's row counts for `viewer`: the songs they are shown and how many
/// seconds those last. A kid profile's count and length leave out explicit songs, library and
/// outside alike, as their `getPlaylist` answer does.
async fn family_playlist_totals(
    state: &AppState,
    playlist: &FamilyPlaylist,
    viewer: &FamilyCaller,
) -> (usize, i64) {
    let kid = kid_filter::is_kid(viewer);
    let unrated = state.settings.current().family.kid_unrated_is_explicit;
    let index = &state.library_index;
    let songs: Vec<(i64, bool)> = futures::stream::iter(family_playlists::shown_entries(playlist, viewer))
        .map(|shown| async move {
            match shown.serve {
                Serve::Library { id, .. } => {
                    let seconds = index.song(&id).map_or(0, |song| i64::from(song.duration));
                    let explicit = kid && kid_filter::song_is_explicit(index, &id, None, unrated);
                    (seconds, explicit)
                }
                Serve::Outside(id) => {
                    let stored = playlist.entries.get(shown.stored);
                    let (seconds, mark) = outside_facts(state, stored, &id).await;
                    (seconds.unwrap_or(0), kid && mark.blocks_clean_only(unrated))
                }
            }
        })
        .buffered(FAMILY_SONGS_AT_ONCE)
        .collect()
        .await;
    songs
        .iter()
        .filter(|(_, explicit)| !explicit)
        .fold((0, 0), |(count, total), (seconds, _)| {
            (count + 1, total + seconds)
        })
}

/// An outside song's length and explicit mark: as the entry kept them when it was added, else
/// from the catalog now.
async fn outside_facts(
    state: &AppState,
    stored: Option<&FamilyPlaylistEntry>,
    id: &str,
) -> (Option<i64>, ExplicitMark) {
    if let Some(entry) = stored.filter(|e| e.outside_id.as_deref() == Some(id))
        && (entry.duration.is_some() || entry.explicit_status.is_some())
    {
        return (
            entry.duration,
            ExplicitMark::from_status(entry.explicit_status.as_deref().unwrap_or("")),
        );
    }
    let (duration, status) = outside_lookup(state, id).await;
    (
        duration,
        ExplicitMark::from_status(status.as_deref().unwrap_or("")),
    )
}

/// An outside song's length in seconds and its explicit mark word, from the catalog.
async fn outside_lookup(state: &AppState, id: &str) -> (Option<i64>, Option<String>) {
    let (external, provider, external_id) = state.local_library.parse_song_id(id);
    if !external {
        return (None, None);
    }
    let Some(song) = state
        .metadata_service
        .get_song(
            provider.as_deref().unwrap_or_default(),
            external_id.as_deref().unwrap_or_default(),
        )
        .await
    else {
        return (None, None);
    };
    let status = match ExplicitMark::from_catalog(song.explicit_content_lyrics) {
        ExplicitMark::Explicit => "explicit",
        ExplicitMark::Clean => "clean",
        ExplicitMark::Unrated => "",
    };
    (song.duration.map(i64::from), Some(status.to_string()))
}

pub(super) fn utc_millis(at: &DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// One family playlist's row as `viewer` sees it: owned by "Family", public, with the number
/// of songs this viewer is shown and how long they last. Read-only for a viewer who cannot
/// change it. It also carries `octoFamily` (true), `octoFamilyCreatedBy`, and, for this viewer,
/// `octoFamilyMayRename` and `octoFamilyMayDelete`. `changed` is the newest of when it was made,
/// when any song was added, and its own change time.
fn family_playlist_fields(
    playlist: &FamilyPlaylist,
    viewer: &FamilyCaller,
    count: usize,
    seconds: i64,
) -> serde_json::Map<String, Value> {
    let changed = playlist
        .entries
        .iter()
        .map(|entry| entry.added)
        .chain([playlist.created])
        .chain(playlist.changed)
        .max()
        .unwrap_or(playlist.created);
    let mut fields = serde_json::Map::new();
    fields.insert("id".into(), playlist.id.clone().into());
    fields.insert("name".into(), playlist.name.clone().into());
    fields.insert("comment".into(), "Family playlist".into());
    fields.insert("owner".into(), FAMILY_OWNER.into());
    fields.insert("public".into(), true.into());
    fields.insert("songCount".into(), count.into());
    fields.insert("duration".into(), seconds.into());
    fields.insert("created".into(), utc_millis(&playlist.created).into());
    fields.insert("changed".into(), utc_millis(&changed).into());
    fields.insert("readonly".into(), (!family_playlists::may_edit(viewer)).into());
    fields.insert("octoFamily".into(), true.into());
    fields.insert("octoFamilyCreatedBy".into(), playlist.created_by.clone().into());
    fields.insert(
        "octoFamilyMayRename".into(),
        family_playlists::may_rename(viewer, playlist).into(),
    );
    fields.insert(
        "octoFamilyMayDelete".into(),
        family_playlists::may_delete(viewer).into(),
    );
    fields
}

/// A row's fields as XML attributes (`XmlValue`).
fn field_attributes(fields: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|(name, value)| (name.clone(), xml_value(value)))
        .collect()
}

/// The JSON merge: `subsonic-response.{container}.{row}` made an array if it was not one, and
/// the rows appended. None when the body is not the object it should be.
pub(super) fn merge_json(
    body: &[u8],
    container: &str,
    row: &str,
    rows: impl IntoIterator<Item = Value>,
) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(body).ok()?;
    let mut root = Node::parse(text).ok()?;
    let response = root
        .as_object_mut()?
        .get_mut("subsonic-response")?
        .as_object_mut()?;
    if !matches!(response.get(container), Some(Node::Object(_))) {
        response.insert(container.to_string(), Node::object());
    }
    let holder = response.get_mut(container)?.as_object_mut()?;
    if !matches!(holder.get(row), Some(Node::Array(_))) {
        holder.insert(row.to_string(), Node::Array(Vec::new()));
    }
    let Some(Node::Array(list)) = holder.get_mut(row) else {
        return None;
    };
    list.extend(rows.into_iter().map(|value| to_node(&value)));
    Some(root.to_json_string(false).into_bytes())
}

/// The XML merge: the root's first `{container}` child (by local name), added if missing, and
/// one `{row}` element per attribute list in the root's namespace.
pub(super) fn merge_xml(
    body: &[u8],
    container: &str,
    row: &str,
    rows: impl IntoIterator<Item = Vec<(String, String)>>,
) -> Option<Vec<u8>> {
    let mut root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    let ns = root.namespace.clone();
    if !root.elements().any(|element| element.name == container) {
        root.push(XElement::in_namespace(ns.as_deref(), container));
    }
    let holder = root.elements_mut().find(|element| element.name == container)?;
    for attributes in rows {
        let mut element = XElement::in_namespace(ns.as_deref(), row);
        for (name, value) in attributes {
            element.set_attr(name, value);
        }
        holder.push(element);
    }
    Some(root.to_xml_string().into_bytes())
}

/// A serialized value as a node: doubles in their shortest form.
pub(super) fn to_node(value: &Value) -> Node {
    match value {
        Value::Null => Node::Null,
        Value::Bool(b) => Node::Bool(*b),
        Value::Number(n) if n.is_i64() || n.is_u64() => Node::Number(n.to_string()),
        Value::Number(n) => Node::Number(octo_core::json::format_double(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => Node::String(s.clone()),
        Value::Array(items) => Node::Array(items.iter().map(to_node).collect()),
        Value::Object(map) => Node::Object(map.iter().map(|(k, v)| (k.clone(), to_node(v))).collect()),
    }
}

pub async fn get_playlist(State(state): State<AppState>, req: Request) -> Response {
    let aborted = RequestAborted::new();
    let response = get_playlist_inner(&state, req, &aborted).await;
    aborted.answered();
    response
}

async fn get_playlist_inner(state: &AppState, req: Request, aborted: &RequestAborted) -> Response {
    let call = match SubsonicCall::read(state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    let builder = &state.subsonic_response_builder;
    let id = call.param_or("id", "").to_string();
    let username = listener_name_of(state, &call.parameters, &call.proxy, &call.parts.headers).await;

    if id.starts_with("oi") && is_octo_playlist_id(&id, false) {
        if !signed_in(state, &call, format).await {
            return builder
                .create_error(format, 40, "Wrong username or password")
                .into_response();
        }
        let owner = state
            .request_identity
            .username(&call.parameters, &call.proxy)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let Some(list) = state.imports.matched_streaming_list(&owner, &id).await else {
            return builder
                .create_error(format, 70, "Playlist not found")
                .into_response();
        };
        let call = &call;
        let owner = &owner;
        let entries: Vec<Node> = futures::stream::iter(list.tracks.clone())
            .map(|track| async move {
                if let Some(library_id) = &track.library_id {
                    if let Some(song) = state.imports.cached_song(owner, library_id) {
                        return Node::from_value(&song);
                    }
                    if let Some(song) = library_song_json(call, library_id).await {
                        state.imports.cache_song(owner, library_id, song.clone());
                        return Node::from_value(&song);
                    }
                }
                let song = crate::services::imports::import_streaming::preview_song(
                    &state.external_id_registry,
                    &track,
                );
                let mut fields = builder.convert_song_to_json(&song);
                // A list read from a file may have no lengths, and some players need the
                // field to be a number: 0 says the length is unknown.
                fields.entry("duration".to_string()).or_insert(0.into());
                Node::from_value(&Value::Object(fields))
            })
            .buffered(FAMILY_SONGS_AT_ONCE)
            .collect()
            .await;
        return builder
            .create_entries_playlist_response(format, import_row_fields(&list), &entries)
            .into_response();
    }

    // Liked Songs: the listener's hearts as Navidrome holds them now, newest first. Only for a
    // caller that names its listener.
    if !id.is_empty()
        && !octo_core::common::dotnet::is_blank(&username)
        && id == state.generated_playlists.liked_id(&username)
        && state.settings.current().generated_playlists.liked_songs
    {
        let mut auth_only = call.parameters.clone();
        auth_only.shift_remove("id");
        let check = call.proxy.relay_safe("rest/ping", &auth_only).await;
        if !check.is_some_and(|check| is_successful_subsonic_response(&check.body, format)) {
            return builder
                .create_error(format, 40, "Wrong username or password")
                .into_response();
        }
        let settings = state.settings.current().generated_playlists.clone();
        let Some(mut entries) = state
            .generated_playlists
            .liked_songs(&username, &call.parameters)
            .await
        else {
            return builder
                .create_error(format, 0, "Unable to read your hearted songs")
                .into_response();
        };
        if let Some(marks) = state.generated_playlists.marks(&username, &call.parameters).await {
            state.generated_playlists.overlay(&username, &mut entries, &marks);
        }
        let row = state
            .generated_playlists
            .liked_row(&username, &call.parameters)
            .await
            .unwrap_or_else(|| GeneratedPlaylist {
                id: id.clone(),
                key: for_you_lists::LIKED_KIND.to_string(),
                kind: for_you_lists::LIKED_KIND.to_string(),
                label: settings.effective_liked_songs_name(),
                name: settings.effective_liked_songs_name(),
                owner: username.trim().to_string(),
                pool_size: 0,
                period_start_utc: chrono::Utc::now(),
                period_end_utc: chrono::Utc::now(),
            });
        let fields = liked_playlist_fields(builder, &row, &settings);
        return builder
            .create_entries_playlist_response(format, fields, &entries)
            .into_response();
    }

    if is_family_playlist_id(state, &id) {
        return family_playlist(state, &call, &id).await;
    }

    // A mix is the listener's own library, served as a playlist Navidrome has never heard of.
    // The ping is the auth check, as for a station: nothing about a user is revealed first.
    if let Some(mix) = state.generated_playlists.find(&username, &id) {
        if !signed_in(state, &call, format).await {
            return builder
                .create_error(format, 40, "Wrong username or password")
                .into_response();
        }
        // An app that keeps its list of playlists and reads each by id still has the lists made
        // again when they are due; the refresh runs behind this answer.
        let _ = state.generated_playlists.list(&username, &call.parameters).await;
        // A failure here answers 500.
        let mut entries = match state
            .generated_playlists
            .materialize(&username, &mix, &call.parameters, &aborted.token)
            .await
        {
            Ok(entries) => entries,
            Err(error) => return crate::http::error::AppError::Internal(error).into_response(),
        };
        // Drawn earlier, so hearts and ratings given since are put on as they are now.
        if let Some(marks) = state.generated_playlists.marks(&username, &call.parameters).await {
            state.generated_playlists.overlay(&username, &mut entries, &marks);
        }
        let settings = state.settings.current().generated_playlists.clone();
        return builder
            .create_generated_playlist_response(format, &mix, &settings, &entries)
            .into_response();
    }

    // A chart list, with the same ping first.
    if let Some(chart) = state.chart_playlists.which(&username, &id) {
        if !signed_in(state, &call, format).await {
            return builder
                .create_error(format, 40, "Wrong username or password")
                .into_response();
        }
        let entries = state
            .chart_playlists
            .songs(&username, &chart, &call.parameters)
            .await;
        let outside = state.chart_playlists.outside_songs(&username, &chart);
        // Made hours ago, so the library songs' hearts and ratings are put on as they are now.
        let entries = match state.generated_playlists.marks(&username, &call.parameters).await {
            Some(marks) => entries
                .into_iter()
                .map(|entry| {
                    if entry.get("isExternal").and_then(Value::as_bool) == Some(true) {
                        return entry;
                    }
                    let mut one = [Node::from_value(&entry)];
                    state.generated_playlists.overlay(&username, &mut one, &marks);
                    let [one] = one;
                    one.to_value()
                })
                .collect(),
            None => entries,
        };
        // A song this listener's sync catalog holds goes out as the catalog describes it, as on
        // a station: a syncing client keeps whichever description it read last.
        let entries: Vec<Value> = entries
            .into_iter()
            .map(|entry| {
                let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                if !outside.iter().any(|song| song.id == id) {
                    return entry;
                }
                match state.sync_catalog.try_get_song(&username, id) {
                    Some(synced) => Value::Object(builder.convert_song_to_json(&synced)),
                    None => entry,
                }
            })
            .collect();
        state
            .radio_queues
            .register(outside.iter().map(|song| song.id.clone()));
        let metadata = Arc::clone(&state.metadata_service);
        tokio::spawn(async move { metadata.prewarm_you_tube_ids(&outside, 8).await });
        let list = state
            .chart_playlists
            .made_playlist(&username, &chart)
            .unwrap_or_else(|| {
                state
                    .chart_playlists
                    .playlist(&username, &chart, String::new(), 0)
            });
        let fields = chart_playlist_fields(builder, &list);
        return builder
            .create_chart_playlist_response(format, fields, &entries)
            .into_response();
    }

    // A playlist Octo holds, or kept aside (this read moves it back): every song in its place,
    // the library songs as Navidrome describes them to this caller.
    if super::held::has_part(state, &id)
        && let Some(answer) = super::held::get_held(state, &call, &id).await
    {
        return answer;
    }

    let Some(station) = playlist_stations(state, &username)
        .into_iter()
        .find(|item| item.id == id)
    else {
        return match call.proxy.relay_safe("rest/getPlaylist", &call.parameters).await {
            Some(relay) if is_successful_subsonic_response(&relay.body, format) => {
                let marks = RowMarks::for_caller(state, &call, &username);
                match marks.apply(&relay.body, format) {
                    Some(body) => call.file(&body.into(), relay.content_type.as_deref()),
                    None => call.file(&relay.body, relay.content_type.as_deref()),
                }
            }
            Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
            None => builder
                .create_error(format, 0, "Playlist not found")
                .into_response(),
        };
    };
    if !signed_in(state, &call, format).await {
        return builder
            .create_error(format, 40, "Wrong username or password")
            .into_response();
    }

    let songs = materialize_station(state, &call.proxy, &station, &call.parameters).await;
    let suggested = suggested_sources(&songs);
    let songs = state
        .generated_playlists
        .blend_into_discovery(&username, &station, songs, &call.parameters)
        .await;
    let songs = serve_station_songs(state, &username, songs, &suggested);
    queue_refresh_if_stale(state, &username);
    builder
        .create_radio_playlist_response(format, &station, &songs)
        .into_response()
}

pub async fn mutate_internet_radio_station(State(state): State<AppState>, req: Request) -> Response {
    mutate(
        state,
        req,
        "id",
        "updateInternetRadioStation",
        "Unable to update internet radio station",
    )
    .await
}

pub async fn mutate_playlist(State(state): State<AppState>, req: Request) -> Response {
    mutate(
        state,
        req,
        "playlistId",
        "updatePlaylist",
        "Unable to update playlist",
    )
    .await
}

/// What a family action playlist does with a song put in it.
pub(super) enum FamilyPlaylistAction {
    Request(RequestQuality),
    Remove,
}

/// The action of a family member's action playlist named `name` ("Request a copy", "Request a
/// copy in FLAC", "Remove from my library"); None for any other name.
pub(super) fn family_action_of(name: &str) -> Option<FamilyPlaylistAction> {
    let name = name.trim();
    if name.eq_ignore_ascii_case(REQUEST_PLAYLIST) {
        Some(FamilyPlaylistAction::Request(RequestQuality::Best))
    } else if name.eq_ignore_ascii_case(REQUEST_FLAC_PLAYLIST) {
        Some(FamilyPlaylistAction::Request(RequestQuality::Flac))
    } else if name.eq_ignore_ascii_case(REMOVE_PLAYLIST) {
        Some(FamilyPlaylistAction::Remove)
    } else {
        None
    }
}

/// Each song becomes a request, or leaves `user`'s library, as the action playlist `name` says.
pub(super) async fn run_family_action(
    state: &AppState,
    user: &str,
    name: &str,
    action: &FamilyPlaylistAction,
    songs: &[String],
) {
    for song in songs {
        match action {
            FamilyPlaylistAction::Request(quality) => {
                match state
                    .family_request_service
                    .create(user, song, *quality, None)
                    .await
                {
                    Ok(request) => info!("FAMILY {user} asked for {song} through {name} ({})", request.id),
                    Err(e) => info!("FAMILY {user} could not ask for {song}: {}", e.message),
                }
            }
            FamilyPlaylistAction::Remove => {
                if let Err(e) = state.personal_library.remove_from_my_library(user, song).await {
                    info!("FAMILY {user} could not remove {song}: {}", e.message);
                }
            }
        }
    }
}

/// `updatePlaylist` on one of a family member's own action playlists ("Request a copy",
/// "Request a copy in FLAC", "Remove from my library"): each song added becomes a request or
/// leaves their library, and so does anything already in the playlist; the playlist is then
/// emptied, since the requests are the record. None for any other playlist.
async fn family_action_playlist(
    state: &AppState,
    call: &SubsonicCall,
    id: &str,
    user: &str,
) -> Option<Response> {
    const AUTH_KEYS: [&str; 9] = ["u", "p", "t", "s", "v", "c", "apiKey", "jwt", "f"];
    if id.trim().is_empty() {
        return None;
    }
    let mut auth: Vec<(String, String)> = call
        .parameters
        .iter()
        .filter(|(key, _)| {
            AUTH_KEYS.iter().any(|k| k.eq_ignore_ascii_case(key)) && !key.eq_ignore_ascii_case("f")
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    auth.push(("f".into(), "json".into()));
    let mut ask = auth.clone();
    ask.push(("id".into(), id.to_string()));
    let read = call
        .proxy
        .relay_safe("rest/getPlaylist", ask.iter().map(|(k, v)| (k, v)))
        .await?;
    let body: Value = serde_json::from_slice(&read.body).ok()?;
    let playlist = body.pointer("/subsonic-response/playlist")?;
    let name = playlist.get("name").and_then(Value::as_str)?.trim().to_string();
    let action = family_action_of(&name)?;
    let already: Vec<String> = playlist
        .get("entry")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let added: Vec<String> = call
        .param("songIdToAdd")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|song| !song.is_empty())
        .map(str::to_string)
        .collect();
    let songs: Vec<String> = already.iter().chain(added.iter()).cloned().collect();
    run_family_action(state, user, &name, &action, &songs).await;
    if !already.is_empty() {
        let mut empty = auth.clone();
        empty.push(("playlistId".into(), id.to_string()));
        empty.extend((0..already.len()).map(|at| ("songIndexToRemove".to_string(), at.to_string())));
        if call
            .proxy
            .relay_safe("rest/updatePlaylist", empty.iter().map(|(k, v)| (k, v)))
            .await
            .is_none()
        {
            warn!("Could not empty {user}'s {name} playlist");
        }
    }
    Some(
        state
            .subsonic_response_builder
            .create_response(&call.format, "updatePlaylist")
            .into_response(),
    )
}

/// Refuses Octo's own ids, changes family playlists, and relays everything else as
/// `rest/<last path segment>` in the client's own spelling.
async fn mutate(state: AppState, req: Request, id_key: &str, _fallback: &str, failure: &str) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    // updatePlaylist names its playlist `playlistId`, falling back to `id`.
    let id = call
        .param(id_key)
        .or_else(|| call.param("id"))
        .unwrap_or("")
        .to_string();
    // A family member's request and remove playlists turn what is added to them into requests
    // or removals, and stay empty.
    if id_key == "playlistId"
        && let Some(member) = family_caller(&call.parts.extensions)
            .filter(|caller| caller.managed)
            .map(|caller| caller.user.clone())
        && let Some(answer) = family_action_playlist(&state, &call, &id, &member).await
    {
        return answer;
    }
    if is_family_playlist_id(&state, &id)
        && let Some(change) = playlist_change(&call)
    {
        return family_mutate(&state, &call, &id, change).await;
    }
    if is_octo_playlist_id(&id, state.settings.current().family.enabled) {
        return state
            .subsonic_response_builder
            .create_error(format, 70, "Octo's generated playlists are read-only")
            .into_response();
    }
    // Playlist changes. Octo's own ids, refused above, are never anyone's upkeep playlist; the
    // held routes refuse songs found online for the upkeep playlists first, then hold, convert or
    // relay the change under the playlist's lock.
    if id_key == "playlistId"
        && let Some(change) = playlist_change(&call)
    {
        return super::held::route(state, call, id, change).await;
    }
    // The endpoint is the path's last segment, without `.view`.
    let path = call.path();
    let endpoint = path.split('/').next_back().unwrap_or("").replace(".view", "");
    match call
        .proxy
        .relay_safe(&format!("rest/{endpoint}"), &call.parameters)
        .await
    {
        Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
        None => state
            .subsonic_response_builder
            .create_error(format, 0, failure)
            .into_response(),
    }
}

/// Whether `id` is a family playlist's while Family is on.
fn is_family_playlist_id(state: &AppState, id: &str) -> bool {
    id.starts_with("of") && is_octo_playlist_id(id, state.settings.current().family.enabled)
}

/// A playlist call that may name a family playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaylistChange {
    Create,
    Update,
    Delete,
}

/// Which playlist call this is, from the last segment of its path.
fn playlist_change(call: &SubsonicCall) -> Option<PlaylistChange> {
    let path = call.path();
    let name = path.rsplit('/').next().unwrap_or("");
    let name = name.strip_suffix(".view").unwrap_or(name);
    [
        ("createPlaylist", PlaylistChange::Create),
        ("updatePlaylist", PlaylistChange::Update),
        ("deletePlaylist", PlaylistChange::Delete),
    ]
    .into_iter()
    .find(|(endpoint, _)| eq_ignore_case(endpoint, name))
    .map(|(_, change)| change)
}

/// A Subsonic error in the call's format.
fn family_error(state: &AppState, call: &SubsonicCall, code: i32, message: &str) -> Response {
    state
        .subsonic_response_builder
        .create_error(&call.format, code, message)
        .into_response()
}

fn playlist_error(state: &AppState, call: &SubsonicCall, error: PlaylistError) -> Response {
    let code = match error {
        PlaylistError::NotFound => 70,
        PlaylistError::NotAllowed(_) => 50,
        PlaylistError::BlankName => 10,
        PlaylistError::Changed | PlaylistError::Refused(_) => 0,
    };
    family_error(state, call, code, error.message())
}

/// Whether the request's sign-in is good, before Octo answers with something of the caller's
/// own. While Family is on, the family gate's reading checks exactly the sign-in it read;
/// otherwise Navidrome is pinged with the request's own parameters, as it always was.
pub(super) async fn signed_in(state: &AppState, call: &SubsonicCall, format: &str) -> bool {
    if let Some(reading) = GateReading::of(&call.parts.extensions) {
        return reading.verify(state).await.is_ok();
    }
    let mut auth_only = call.parameters.clone();
    auth_only.shift_remove("id");
    let check = call.proxy.relay_safe("rest/ping", &auth_only).await;
    check.is_some_and(|check| is_successful_subsonic_response(&check.body, format))
}

/// The caller of a family playlist call, checked by the family gate's reading with exactly the
/// sign-in it read, or the answer to send instead.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
async fn checked_caller(state: &AppState, call: &SubsonicCall) -> Result<FamilyCaller, Response> {
    let Some(reading) = GateReading::of(&call.parts.extensions) else {
        return Err(playlist_error(state, call, PlaylistError::NotFound));
    };
    match reading.verify(state).await {
        Ok(verified) => Ok(verified.caller().clone()),
        Err(Unverified::Missing) => Err(playlist_error(state, call, PlaylistError::NotFound)),
        Err(Unverified::Refused) => Err(family_error(state, call, 40, "Wrong username or password")),
        Err(Unverified::Unreachable) => Err(family_error(
            state,
            call,
            0,
            "Octo can't reach Navidrome to check who is asking",
        )),
    }
}

/// A family playlist, each entry resolved for the caller: the library song (read as the caller)
/// when it is shared or the caller's own, otherwise its outside version.
async fn family_playlist(state: &AppState, call: &SubsonicCall, id: &str) -> Response {
    let caller = match checked_caller(state, call).await {
        Ok(caller) => caller,
        Err(answer) => return answer,
    };
    let caller = &caller;
    let Some(playlist) = family_playlists::find(&state.family_playlists, caller, id) else {
        return playlist_error(state, call, PlaylistError::NotFound);
    };

    let shown = family_playlists::shown_entries(&playlist, caller);
    let entries: Vec<Value> = served_entries(state, call, shown, false)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(_, song)| song)
        .collect();
    // The whole list's count and length; a kid profile's answer rule then takes out the
    // explicit songs and lowers both.
    let seconds = entries
        .iter()
        .map(|song| song.get("duration").and_then(Value::as_i64).unwrap_or(0))
        .sum();
    let fields = family_playlist_fields(&playlist, caller, entries.len(), seconds);
    state
        .subsonic_response_builder
        .create_chart_playlist_response(&call.format, fields, &entries)
        .into_response()
}

/// A family playlist's songs as `getPlaylist` answers them to its caller, each with its place in
/// the stored playlist: the library song (read as the caller) when it is shared or the caller's
/// own, else its outside version. An entry neither can be read for is left out. With `strict`, an
/// entry whose library song could not be read because Navidrome did not answer makes the whole
/// answer `None`, so nothing is decided from a list the caller was never shown.
async fn served_entries(
    state: &AppState,
    call: &SubsonicCall,
    shown: Vec<ShownEntry>,
    strict: bool,
) -> Option<Vec<(usize, Value)>> {
    let resolved: Vec<Result<Option<(usize, Value)>, ()>> = futures::stream::iter(shown)
        .map(|entry| async move {
            let stored = entry.stored;
            let song = match entry.serve {
                Serve::Library { id, outside } => match library_song_read(call, &id).await {
                    LibraryRead::Song(song) => Some(song),
                    read => {
                        if strict && matches!(read, LibraryRead::Unreachable) {
                            return Err(());
                        }
                        match outside {
                            Some(outside) => outside_song_json(state, &outside).await,
                            None => None,
                        }
                    }
                },
                Serve::Outside(outside) => outside_song_json(state, &outside).await,
            };
            Ok(song.map(|song| (stored, song)))
        })
        .buffered(FAMILY_SONGS_AT_ONCE)
        .collect()
        .await;
    let mut served = Vec::new();
    for entry in resolved {
        if let Some(song) = entry.ok()? {
            served.push(song);
        }
    }
    Some(served)
}

/// The stored places of the songs the caller's `getPlaylist` answer lists, in its order: what
/// `songIndexToRemove` counts over. `None` when Navidrome could not be reached for a library song.
async fn served_places(
    state: &AppState,
    call: &SubsonicCall,
    playlist: &FamilyPlaylist,
    caller: &FamilyCaller,
) -> Option<Vec<usize>> {
    let served = served_entries(
        state,
        call,
        family_playlists::shown_entries(playlist, caller),
        true,
    )
    .await?;
    // The songs getPlaylist's answer lists are the ones left once the answer rules that answer goes
    // through for this caller have run (a kid profile's explicit songs, the owner's privacy).
    let body = json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "playlist": {
        "id": playlist.id,
        "entry": served.iter().map(|(_, song)| song.clone()).collect::<Vec<Value>>(),
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
    // Each listed song, in order, matched to the served song it came from.
    let mut places = Vec::new();
    let mut next = 0;
    for id in listed {
        while next < served.len() && served[next].1.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            next += 1;
        }
        if next < served.len() {
            places.push(served[next].0);
            next += 1;
        }
    }
    Some(places)
}

/// What reading a library song as the caller gave.
enum LibraryRead {
    Song(Value),
    /// Navidrome answered that the caller has no such song (error 70).
    Refused,
    /// Navidrome did not answer, answered some other error, or answered something that is not a song.
    Unreachable,
}

/// Reads a library song as the caller, telling a song the caller does not have from a Navidrome that did not answer.
async fn library_song_read(call: &SubsonicCall, id: &str) -> LibraryRead {
    let mut request = call.parameters.clone();
    request.insert("id".into(), id.to_string());
    request.insert("f".into(), "json".into());
    request.shift_remove("callback");
    let Some(relay) = call.proxy.relay_safe("rest/getSong", &request).await else {
        return LibraryRead::Unreachable;
    };
    let Ok(answer) = serde_json::from_slice::<Value>(&relay.body) else {
        return LibraryRead::Unreachable;
    };
    let Some(response) = answer.get("subsonic-response") else {
        return LibraryRead::Unreachable;
    };
    if response.get("status").and_then(Value::as_str) != Some("ok") {
        let code = response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_i64);
        return if code == Some(70) {
            LibraryRead::Refused
        } else {
            LibraryRead::Unreachable
        };
    }
    match response.get("song").filter(|song| song.is_object()) {
        Some(song) => LibraryRead::Song(song.clone()),
        None => LibraryRead::Unreachable,
    }
}

/// A library song as Navidrome describes it to the caller, in JSON, when it can be read.
async fn library_song_json(call: &SubsonicCall, id: &str) -> Option<Value> {
    match library_song_read(call, id).await {
        LibraryRead::Song(song) => Some(song),
        _ => None,
    }
}

/// An outside song in the shape every outside song has.
pub(super) async fn outside_song_json(state: &AppState, id: &str) -> Option<Value> {
    let (external, provider, external_id) = state.local_library.parse_song_id(id);
    if !external {
        return None;
    }
    let song = state
        .metadata_service
        .get_song(
            provider.as_deref().unwrap_or_default(),
            external_id.as_deref().unwrap_or_default(),
        )
        .await?;
    Some(Value::Object(
        state.subsonic_response_builder.convert_song_to_json(&song),
    ))
}

/// `updatePlaylist` and `deletePlaylist` on a family playlist. `createPlaylist` never replaces
/// one.
async fn family_mutate(state: &AppState, call: &SubsonicCall, id: &str, change: PlaylistChange) -> Response {
    let store = &state.family_playlists;
    // Nothing here goes to Navidrome, so the caller is checked before a change is made in
    // anyone's name.
    let caller = match checked_caller(state, call).await {
        Ok(caller) => caller,
        Err(answer) => return answer,
    };
    let caller = &caller;
    let Some(playlist) = family_playlists::find(store, caller, id) else {
        return playlist_error(state, call, PlaylistError::NotFound);
    };
    let done = || {
        state
            .subsonic_response_builder
            .create_response(&call.format, "playlist")
            .into_response()
    };
    match change {
        PlaylistChange::Create => family_error(
            state,
            call,
            50,
            "A family playlist cannot be replaced. Add or remove its songs instead.",
        ),
        PlaylistChange::Delete => match family_playlists::delete(store, caller, id) {
            Ok(()) => done(),
            Err(error) => playlist_error(state, call, error),
        },
        PlaylistChange::Update => {
            if !family_playlists::may_edit(caller) {
                return playlist_error(
                    state,
                    call,
                    PlaylistError::NotAllowed("This account cannot change family playlists."),
                );
            }
            let name = call.param("name").map(str::trim).filter(|n| !n.is_empty());
            let renames = name.is_some_and(|name| name != playlist.name);
            let comments = call.param("comment").is_some_and(|c| !c.trim().is_empty());
            if (renames || comments) && !family_playlists::may_rename(caller, &playlist) {
                return playlist_error(
                    state,
                    call,
                    PlaylistError::NotAllowed(
                        "Only the person who made this playlist or a family manager can change it.",
                    ),
                );
            }
            let song_removals = call.incoming.parameter_values("octoSongToRemove");
            if !song_removals.is_empty() && !call.incoming.parameter_values("songIndexToRemove").is_empty() {
                return family_error(
                    state,
                    call,
                    10,
                    "Use octoSongToRemove or songIndexToRemove, not both.",
                );
            }
            let removals: Vec<usize> = call
                .incoming
                .parameter_values("songIndexToRemove")
                .iter()
                .filter_map(|index| index.trim().parse().ok())
                .collect();
            // Songs named by id are found in the stored list without reading Navidrome, so the
            // places cannot drift.
            let pairs: Vec<(String, usize)> = song_removals
                .iter()
                .filter_map(|value| {
                    let (id, n) = value.trim().rsplit_once('@')?;
                    let n: usize = n.trim().parse().ok().filter(|n| *n > 0)?;
                    (!id.is_empty()).then(|| (id.to_string(), n))
                })
                .collect();
            if !pairs.is_empty() {
                let stored = family_playlists::stored_places_of(&playlist, caller, &pairs);
                if let Err(error) =
                    family_playlists::remove_stored(store, caller, id, &playlist.entries, &stored)
                {
                    return playlist_error(state, call, error);
                }
            }
            // Places count over the songs the caller's answer lists (see served_places), worked out
            // and applied before the name changes, so a refused removal changes nothing.
            let removal_places = if removals.is_empty() {
                None
            } else {
                let Some(places) = served_places(state, call, &playlist, caller).await else {
                    return family_error(
                        state,
                        call,
                        0,
                        "Octo can't reach Navidrome to read this playlist's songs, so nothing was taken out.",
                    );
                };
                Some(
                    removals
                        .iter()
                        .filter_map(|at| places.get(*at).copied())
                        .collect::<Vec<usize>>(),
                )
            };
            if let Some(stored) = removal_places
                && let Err(error) =
                    family_playlists::remove_stored(store, caller, id, &playlist.entries, &stored)
            {
                return playlist_error(state, call, error);
            }
            if renames
                && let Some(name) = name
                && let Err(error) = family_playlists::rename(store, caller, id, name)
            {
                return playlist_error(state, call, error);
            }
            let mut additions = Vec::new();
            for song in call.incoming.parameter_values("songIdToAdd") {
                if let Some(entry) = new_entry(state, &song).await {
                    additions.push(entry);
                }
            }
            if !additions.is_empty() {
                let requests = state.family_playlist_requests.get();
                if let Err(error) =
                    family_playlists::add(store, requests.as_ref(), caller, id, additions, Utc::now())
                {
                    return playlist_error(state, call, error);
                }
            }
            done()
        }
    }
}

/// What a family playlist keeps for a song id: a library song with its scope (a member's own
/// library, else shared) and its outside id when Octo knows one, or an outside id alone.
async fn new_entry(state: &AppState, id: &str) -> Option<NewEntry> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let (external, _, _) = state.local_library.parse_song_id(id);
    if external {
        let (duration, explicit_status) = outside_lookup(state, id).await;
        return Some(NewEntry {
            outside_id: Some(id.to_string()),
            duration,
            explicit_status,
            ..NewEntry::default()
        });
    }
    let indexed = state.library_index.song(id);
    let scope = indexed
        .as_ref()
        .and_then(|song| song.library_id)
        .and_then(|library| state.library_scope.personal_library_owner(library))
        .map_or_else(|| SHARED_SCOPE.to_string(), |owner| personal_scope(&owner));
    let outside_id = outside_id_of(state, id, indexed.as_ref().map(|song| song.path.as_str())).await;
    let (duration, explicit_status) = match &outside_id {
        Some(outside) => outside_lookup(state, outside).await,
        None => (None, None),
    };
    Some(NewEntry {
        library_id: Some(id.to_string()),
        scope,
        outside_id,
        duration,
        explicit_status,
    })
}

/// The outside id a library song was downloaded from: from Octo's record of downloaded songs,
/// else from the download history by the file's path.
async fn outside_id_of(state: &AppState, id: &str, path: Option<&str>) -> Option<String> {
    let mapped = state
        .local_library
        .get_mappings()
        .await
        .into_iter()
        .find(|mapping| {
            mapping.local_subsonic_id.as_deref() == Some(id)
                || path.is_some_and(|path| mapping.local_path == path)
        });
    if let Some(mapping) = mapped
        && !mapping.external_provider.is_empty()
        && !mapping.external_id.is_empty()
    {
        return Some(outside_song_id(
            state,
            &mapping.external_provider,
            &mapping.external_id,
        ));
    }
    let path = path?;
    state
        .download_history
        .get_recent(i32::MAX)
        .into_iter()
        .find(|entry| entry.path == path)
        .and_then(|entry| entry.key)
        .and_then(|key| {
            let (provider, external_id) = key.split_once(':')?;
            (!provider.is_empty() && !external_id.is_empty())
                .then(|| outside_song_id(state, provider, external_id))
        })
}

/// The song id an outside song plays by: a Soulseek song Octo knows by its short id, any other
/// as `ext-<provider>-song-<id>`.
fn outside_song_id(state: &AppState, provider: &str, external_id: &str) -> String {
    if eq_ignore_case(provider, "soulseek") && state.external_id_registry.lookup(external_id).is_some() {
        return external_id.to_string();
    }
    format!("ext-{provider}-song-{external_id}")
}
