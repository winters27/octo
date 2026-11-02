//! The controller's shared plumbing: reading a request's parameters the way every action does,
//! the small private helpers, and the radio helpers the playlist, internet radio, search and
//! native radio answers share.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{OriginalUri, Request};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::{TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::{dotnet, skipped_takes};
use octo_core::last_fm::last_fm_radio_refresh_policy;
use octo_core::models::domain::Song;
use octo_core::models::radio::{
    LastFmRadioPlay, LastFmRadioStation, LastFmRadioStationKind, LastFmRadioTrack,
};
use octo_core::radio::radio_provider;
use octo_core::settings::ExplicitFilter;
use octo_subsonic::xml::XElement;
use octo_subsonic::{Parameters, SubsonicReply};
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::app::AppState;
use crate::http::catch_all::endpoint_of;
use crate::middleware::forwarded::RequestScheme;
use crate::services::last_fm::LastFmRadioTrackResolver;
use crate::services::subsonic::{IncomingRequest, SubsonicProxyService};

/// The largest request body accepted, in bytes.
pub const MAX_REQUEST_BODY: usize = 30_000_000;

/// One Subsonic request as an action saw it: the request itself, its parameters (from the query,
/// form and body), the format, and the request-scoped proxy.
pub struct SubsonicCall {
    pub parts: Parts,
    pub incoming: Arc<IncomingRequest>,
    pub parameters: Parameters,
    pub format: String,
    pub proxy: SubsonicProxyService,
}

impl SubsonicCall {
    /// Reads the body and the parameters (while Family is on, the ones the family gate read).
    /// A body over [`MAX_REQUEST_BODY`] is refused with 413. (The refusal is a whole response,
    /// for the action to return early with.)
    #[allow(clippy::result_large_err)]
    pub async fn read(state: &AppState, req: Request) -> Result<SubsonicCall, Response> {
        let (parts, body) = req.into_parts();
        let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
            return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
        };
        let incoming = Arc::new(IncomingRequest::from_parts(&parts, body));
        let parameters = incoming.parameters();
        let format = parameters.get("f").map_or("xml", String::as_str).to_string();
        let proxy = state.subsonic_proxy.with_request(Arc::clone(&incoming));
        Ok(SubsonicCall {
            parts,
            incoming,
            parameters,
            format,
            proxy,
        })
    }

    /// The parameter `name`, or `fallback` when it was not sent.
    pub fn param_or<'a>(&'a self, name: &str, fallback: &'a str) -> &'a str {
        self.parameters.get(name).map_or(fallback, String::as_str)
    }

    /// The parameter `name`, if it was sent.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.parameters.get(name).map(String::as_str)
    }

    /// The request's path as the client spelled it.
    pub fn path(&self) -> String {
        request_path(&self.parts)
    }

    /// The request's endpoint as the client spelled it, without its first `/`: the form a relay
    /// to Navidrome names (`rest/getSimilarSongs2.view`).
    pub fn endpoint(&self) -> String {
        request_endpoint(&self.parts)
    }

    /// The request's scheme, after forwarded headers are applied.
    pub fn scheme(&self) -> String {
        request_scheme(&self.parts)
    }

    /// The Host header as sent.
    pub fn host(&self) -> String {
        request_host(&self.parts)
    }

    /// `body` with its content type, else `application/{format}`.
    pub fn file(&self, body: &Bytes, content_type: Option<&str>) -> Response {
        file_reply(body, content_type, &self.format)
    }
}

/// The request's path, for a request the pipeline may have re-spelled onto a route template.
pub fn request_path(parts: &Parts) -> String {
    format!("/{}", request_endpoint(parts))
}

/// The request's path without its first `/`, for a request the pipeline may have re-spelled
/// onto a route template.
pub fn request_endpoint(parts: &Parts) -> String {
    let path = parts
        .extensions
        .get::<OriginalUri>()
        .map_or_else(|| parts.uri.path().to_string(), |uri| uri.0.path().to_string());
    endpoint_of(&path)
}

pub fn request_scheme(parts: &Parts) -> String {
    parts
        .extensions
        .get::<RequestScheme>()
        .map_or_else(|| "http".to_string(), |scheme| scheme.0.clone())
}

pub fn request_host(parts: &Parts) -> String {
    parts
        .headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| parts.uri.authority().map(|a| a.to_string()))
        .unwrap_or_default()
}

/// `body` with its content type, else `application/{format}`: 200 with an explicit length.
pub fn file_reply(body: &Bytes, content_type: Option<&str>, format: &str) -> Response {
    let content_type = content_type.map_or_else(|| format!("application/{format}"), str::to_string);
    SubsonicReply::file(body.to_vec(), &content_type).into_response()
}

/// A body written with no length, so it is sent chunked.
pub fn written_body(status: StatusCode, headers: HeaderMap, body: Vec<u8>) -> Response {
    let chunk: Result<Bytes, std::io::Error> = Ok(Bytes::from(body));
    let mut response = Response::new(Body::from_stream(futures::stream::iter([chunk])));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// A JSON `{"error": message}` body with `status`, escaped relaxed, so `Octo's` goes out with
/// its apostrophe as is.
pub fn error_object(status: StatusCode, message: &str) -> Response {
    use octo_core::json::format::{Escaping, Options, to_string_with};
    let body = to_string_with(
        &serde_json::json!({ "error": message }),
        Options {
            escaping: Escaping::Relaxed,
            indented: false,
        },
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// Canceled when the client goes away before the action has answered (axum drops the
/// handler), and not when it answers.
pub struct RequestAborted {
    pub token: CancellationToken,
    guard: Option<DropGuard>,
}

impl RequestAborted {
    pub fn new() -> Self {
        let token = CancellationToken::new();
        let guard = Some(token.clone().drop_guard());
        RequestAborted { token, guard }
    }

    /// The action answered: the token is never canceled by this request any more.
    pub fn answered(mut self) {
        if let Some(guard) = self.guard.take() {
            guard.disarm();
        }
    }
}

impl Default for RequestAborted {
    fn default() -> Self {
        Self::new()
    }
}

/// An integer parsed from text: optional leading and trailing white space (tab to carriage
/// return, and space), an optional sign, ASCII digits, within `i32`.
pub fn int_try_parse(s: &str) -> Option<i32> {
    let is_white = |c: char| c == ' ' || ('\u{9}'..='\u{D}').contains(&c);
    let trimmed = s.trim_matches(is_white);
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: i64 = 0;
    for b in digits.bytes() {
        value = value * 10 + i64::from(b - b'0');
        if value > i64::from(i32::MAX) + 1 {
            return None;
        }
    }
    let value = if negative { -value } else { value };
    i32::try_from(value).ok()
}

/// The parameter `name` as an integer, or `fallback` when it is missing or does not parse.
pub fn int_param(parameters: &Parameters, name: &str, fallback: i32) -> i32 {
    match parameters.get(name) {
        Some(text) => int_try_parse(text).unwrap_or(fallback),
        None => fallback,
    }
}

/// Octo's own playlist ids: radio stations start "or", mixes "og", imports "oi", and while Family is on,
/// family playlists "of". Navidrome's ids are 22 characters of base62 that start with 0 to 7,
/// so none can ever be one of them.
pub fn is_octo_playlist_id(id: &str, family_on: bool) -> bool {
    dotnet::utf16_len(id) == 22
        && (id.starts_with("or")
            || id.starts_with("og")
            || id.starts_with("oi")
            || (family_on && id.starts_with("of")))
}

/// Whether a relayed body says `status="ok"`, read in the format the client asked for
/// (compared ignoring case). Anything unreadable is not a success.
pub fn is_successful_subsonic_response(body: &[u8], format: &str) -> bool {
    if format.eq_ignore_ascii_case("json") {
        let Ok(document) = serde_json::from_slice::<Value>(body) else {
            return false;
        };
        return document
            .get("subsonic-response")
            .and_then(|response| response.get("status"))
            .and_then(Value::as_str)
            == Some("ok");
    }
    match XElement::parse(&String::from_utf8_lossy(body)) {
        Ok(root) => root.attribute("status") == Some("ok"),
        Err(_) => false,
    }
}

/// True when a relayed body is a Subsonic error envelope. These arrive as HTTP 200
/// with `status="failed"` inside, so the status code alone cannot tell a rejected
/// request from an empty result set.
pub fn is_failed_subsonic_body(body: Option<&[u8]>, content_type: Option<&str>) -> bool {
    let Some(body) = body.filter(|b| !b.is_empty()) else {
        return false;
    };
    if content_type.is_some_and(|c| c.contains("json")) {
        let Ok(document) = serde_json::from_slice::<Value>(body) else {
            return false;
        };
        return document
            .get("subsonic-response")
            .and_then(|response| response.get("status"))
            .and_then(Value::as_str)
            .is_some_and(|status| dotnet::eq_ignore_case(status, "failed"));
    }
    // Unparseable is not the same as failed. Let the normal path handle it.
    match XElement::parse(&String::from_utf8_lossy(body)) {
        Ok(root) => root
            .attribute("status")
            .is_some_and(|status| dotnet::eq_ignore_case(status, "failed")),
        Err(_) => false,
    }
}

/// Pulls just the song-id strings out of a Subsonic search3 response body, preserving
/// response order. Both JSON and XML shapes are supported because Navidrome respects the f=
/// parameter the proxy forwards.
pub fn extract_local_song_ids(body: Option<&[u8]>, content_type: Option<&str>) -> Vec<String> {
    let Some(body) = body.filter(|b| !b.is_empty()) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    if content_type.is_some_and(|c| c.contains("xml")) {
        if let Ok(root) = XElement::parse(&String::from_utf8_lossy(body)) {
            let ns = root.namespace.clone();
            collect_song_ids(&root, ns.as_deref(), &mut ids);
        }
    } else if let Ok(document) = serde_json::from_slice::<Value>(body)
        && let Some(response) = document.get("subsonic-response")
        && let Some(result) = response
            .get("searchResult3")
            .or_else(|| response.get("searchResult2"))
        && let Some(Value::Array(songs)) = result.get("song")
    {
        for song in songs {
            if let Some(Value::String(id)) = song.get("id")
                && !id.is_empty()
            {
                ids.push(id.clone());
            }
        }
    }
    // A malformed upstream response gives whatever was read.
    ids
}

/// Every `song` element below `element` in the namespace `ns`, in document order.
fn collect_song_ids(element: &XElement, ns: Option<&str>, ids: &mut Vec<String>) {
    for child in element.elements() {
        if child.is(ns, "song")
            && let Some(id) = child.attribute("id").filter(|id| !id.is_empty())
        {
            ids.push(id.to_string());
        }
        collect_song_ids(child, ns, ids);
    }
}

/// A field's value as an XML attribute writes it.
pub fn xml_value(value: &Value) -> String {
    match value {
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n
            .as_f64()
            .filter(|_| !n.is_i64() && !n.is_u64())
            .map_or_else(|| n.to_string(), octo_core::json::format_double),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The playlist names in a relayed getPlaylists body, so provisioning can tell what is
/// already there without a second request. Best-effort: an unreadable body simply means
/// nothing is known to exist, and creating a duplicate is refused by Navidrome anyway.
pub fn playlist_names(body: Option<&[u8]>, format: &str) -> Vec<String> {
    let Some(body) = body.filter(|b| !b.is_empty()) else {
        return Vec::new();
    };
    // XML too: Navidrome does not refuse a second playlist with the same name, so a client
    // that asks for XML used to get every action playlist created again on each boot.
    if !format.eq_ignore_ascii_case("json") {
        let Ok(root) = XElement::parse(&String::from_utf8_lossy(body)) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        collect_playlist_names(&root, &mut names);
        return names;
    }
    let Ok(document) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let Some(Value::Array(rows)) = document
        .get("subsonic-response")
        .and_then(|r| r.get("playlists"))
        .and_then(|p| p.get("playlist"))
    else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for row in rows {
        match row.get("name") {
            // A name that is not a string gives nothing.
            Some(Value::String(name)) if !name.is_empty() => names.push(name.clone()),
            Some(Value::String(_)) | Some(Value::Null) | None => {}
            Some(_) => return Vec::new(),
        }
    }
    names
}

/// Every `playlist` element below `element`: the root itself does not count.
fn collect_playlist_names(element: &XElement, names: &mut Vec<String>) {
    for child in element.elements() {
        if child.name == "playlist"
            && let Some(name) = child.attribute("name").filter(|n| !n.is_empty())
        {
            names.push(name.to_string());
        }
        collect_playlist_names(child, names);
    }
}

// -------------------------------------------------------------------------------------
// Radio stations, as the controller exposes them.
// -------------------------------------------------------------------------------------

pub fn visible_stations(state: &AppState, username: &str) -> Vec<LastFmRadioStation> {
    let settings = state.settings.current();
    let last_fm = &settings.last_fm;
    if !last_fm.enable_radio || username.is_empty() {
        return Vec::new();
    }
    state
        .last_fm_radio_state
        .get_user(username)
        .stations
        .into_iter()
        .filter(|station| {
            if station.personalized {
                personalized_station_visible(state, station)
            } else {
                last_fm.enable_discovery_stations
            }
        })
        .collect()
}

/// Read-time half of the per-type station settings. The build gate decides what gets
/// made; this decides what a client sees, so switching a type off takes effect on the
/// next request instead of waiting for a rebuild. Same two-layer arrangement
/// EnablePersonalizedStations already uses.
fn personalized_station_visible(state: &AppState, station: &LastFmRadioStation) -> bool {
    let settings = state.settings.current();
    let last_fm = &settings.last_fm;
    if !last_fm.enable_personalized_stations {
        return false;
    }
    match station.kind {
        LastFmRadioStationKind::Starter | LastFmRadioStationKind::YourMix => last_fm.enable_your_mix,
        LastFmRadioStationKind::Discovery => last_fm.enable_discovery_mix,
        LastFmRadioStationKind::Artist => last_fm.effective_artist_station_count() > 0,
        LastFmRadioStationKind::Genre => last_fm.effective_genre_station_count() > 0,
        LastFmRadioStationKind::Pinned => true,
    }
}

pub fn playlist_stations(state: &AppState, username: &str) -> Vec<LastFmRadioStation> {
    if state.settings.current().last_fm.expose_radio_as_playlists {
        visible_stations(state, username)
    } else {
        Vec::new()
    }
}

pub fn stream_stations(state: &AppState, username: &str) -> Vec<LastFmRadioStation> {
    if state.settings.current().last_fm.expose_radio_as_streams {
        visible_stations(state, username)
    } else {
        Vec::new()
    }
}

pub fn queue_refresh_if_stale(state: &AppState, username: &str) {
    if username.is_empty() {
        return;
    }
    let user = state.last_fm_radio_state.get_user(username);
    let settings = state.settings.current();
    if user.stations.is_empty()
        || last_fm_radio_refresh_policy::is_stale(&user, &settings.last_fm, Utc::now())
    {
        state.last_fm_radio_refresh_queue.enqueue(username, None);
    }
}

/// A song read from one of Navidrome's song rows for the bootstrap: a field of the wrong kind
/// gives nothing at all.
fn bootstrap_song(item: &Value, id_required: bool) -> Option<Song> {
    let object = item.as_object()?;
    let text = |name: &str| -> Option<String> {
        match object.get(name) {
            None | Some(Value::Null) => Some(String::new()),
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => None,
        }
    };
    let id = if id_required {
        match object.get("id") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) => String::new(),
            _ => return None,
        }
    } else {
        text("id")?
    };
    let genre = match object.get("genre") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    let duration = match object.get("duration") {
        None => None,
        Some(Value::Number(n)) => n.as_i64().and_then(|v| i32::try_from(v).ok()),
        // A value that is not a number gives nothing.
        Some(_) => return None,
    };
    Some(Song {
        id,
        artist: text("artist")?,
        title: text("title")?,
        album: text("album")?,
        genre,
        duration,
        is_local: true,
        ..Default::default()
    })
}

/// The songs of a `subsonic-response.{container}.song` list.
fn bootstrap_songs(body: &[u8], container: &str, learned: bool) -> Vec<(Song, bool)> {
    let read = || -> Option<Vec<(Song, bool)>> {
        let document: Value = serde_json::from_slice(body).ok()?;
        let response = document.as_object()?.get("subsonic-response")?;
        let Some(parent) = response.as_object()?.get(container) else {
            return Some(Vec::new());
        };
        let Some(values) = parent.as_object().and_then(|p| p.get("song")) else {
            // A parent that is not an object gives nothing.
            return parent.as_object().map(|_| Vec::new());
        };
        let Value::Array(values) = values else {
            return Some(Vec::new());
        };
        values
            .iter()
            .take(20)
            .map(|item| bootstrap_song(item, false).map(|song| (song, learned)))
            .collect()
    };
    read().unwrap_or_default()
}

/// The first time a listener lists playlists or stations, Octo learns a starting profile
/// from what they starred, played often or recently, or failing that a few random songs. A
/// listener cleared from the dashboard gets none: their radio learns only from what they play.
pub async fn bootstrap_radio_profile(
    state: &AppState,
    proxy: &SubsonicProxyService,
    username: &str,
    authenticated_parameters: &Parameters,
) {
    let settings = state.settings.current();
    let last_fm = &settings.last_fm;
    if username.is_empty()
        || !last_fm.enable_radio
        || !last_fm.enable_personalized_stations
        || !state.last_fm_radio_state.wants_starting_profile(username)
    {
        return;
    }

    async fn fetch(
        proxy: &SubsonicProxyService,
        authenticated_parameters: &Parameters,
        endpoint: &str,
        container: &str,
        learned: bool,
    ) -> Vec<(Song, bool)> {
        let mut parameters = authenticated_parameters.clone();
        parameters.insert("f".into(), "json".into());
        if endpoint == "rest/getRandomSongs" {
            parameters.insert("size".into(), "12".into());
        }
        let Some(result) = proxy.relay_safe(endpoint, &parameters).await else {
            return Vec::new();
        };
        if result.body.is_empty() {
            return Vec::new();
        }
        bootstrap_songs(&result.body, container, learned)
    }

    async fn fetch_album_signals(
        proxy: &SubsonicProxyService,
        authenticated_parameters: &Parameters,
        list_type: &str,
    ) -> Vec<(Song, bool)> {
        let mut query = authenticated_parameters.clone();
        query.insert("f".into(), "json".into());
        query.insert("type".into(), list_type.into());
        query.insert("size".into(), "3".into());
        let Some(result) = proxy.relay_safe("rest/getAlbumList2", &query).await else {
            return Vec::new();
        };
        if result.body.is_empty() {
            return Vec::new();
        }
        // A missing or mistyped field anywhere below gives nothing at all, even for the albums
        // already read.
        let Some(albums) = serde_json::from_slice::<Value>(&result.body).ok().and_then(|d| {
            d.get("subsonic-response")?
                .get("albumList2")?
                .get("album")?
                .as_array()
                .cloned()
        }) else {
            return Vec::new();
        };
        let mut output = Vec::new();
        for album in albums.iter().take(3) {
            let Some(id) = album.as_object().and_then(|a| a.get("id")) else {
                return Vec::new();
            };
            let id = match id {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                _ => return Vec::new(),
            };
            let mut album_query = authenticated_parameters.clone();
            album_query.insert("f".into(), "json".into());
            album_query.insert("id".into(), id);
            let Some(detail) = proxy.relay_safe("rest/getAlbum", &album_query).await else {
                continue;
            };
            if detail.body.is_empty() {
                continue;
            }
            let Some(songs) = serde_json::from_slice::<Value>(&detail.body).ok().and_then(|d| {
                d.get("subsonic-response")?
                    .get("album")?
                    .get("song")?
                    .as_array()
                    .cloned()
            }) else {
                return Vec::new();
            };
            for item in songs.iter().take(4) {
                let Some(song) = bootstrap_song(item, true) else {
                    return Vec::new();
                };
                output.push((song, true));
            }
        }
        output
    }

    let mut seeds = fetch(
        proxy,
        authenticated_parameters,
        "rest/getStarred2",
        "starred2",
        true,
    )
    .await;
    if (seeds.len() as i32) < last_fm.effective_minimum_plays() {
        seeds.extend(fetch_album_signals(proxy, authenticated_parameters, "frequent").await);
        seeds.extend(fetch_album_signals(proxy, authenticated_parameters, "recent").await);
    }
    if seeds.is_empty() {
        seeds = fetch(
            proxy,
            authenticated_parameters,
            "rest/getRandomSongs",
            "randomSongs",
            false,
        )
        .await;
    }
    for (offset, (song, learned)) in seeds.into_iter().enumerate() {
        state.last_fm_radio_state.record_play(
            username,
            LastFmRadioPlay {
                song_id: song.id,
                artist: song.artist,
                title: song.title,
                album: Some(song.album),
                genre: song.genre,
                duration: song.duration,
                is_local: true,
                hearted: learned,
                learned_signal: learned,
                source: if learned {
                    "bootstrap-star"
                } else {
                    "bootstrap-random"
                }
                .to_string(),
                played_at_utc: Utc::now() - TimeDelta::minutes(offset as i64 * 6),
            },
        );
    }
}

/// A station's tracks as songs this listener can play: each resolved (the library's copy
/// first), the explicit filter and Skip covers and instrumentals applied, Discovery Radio held
/// to the Discovery balance's share of
/// library songs, and back-to-back repeats of an artist or an album dropped.
pub async fn materialize_station(
    state: &AppState,
    proxy: &SubsonicProxyService,
    station: &LastFmRadioStation,
    parameters: &Parameters,
) -> Vec<Song> {
    materialize_picks(
        state,
        proxy,
        station,
        &station.tracks,
        &station.reserve,
        parameters,
        &HashSet::new(),
        None,
    )
    .await
    .into_iter()
    .map(|(song, _)| song)
    .collect()
}

/// [`materialize_station`] for any of a station's songs, each beside the station track it was
/// resolved from: `tracks` resolved, `reserve` standing in for the library songs past Discovery
/// Radio's share, every song whose id is in `skip` left out, and the spacing carried on from
/// `previous` (the artist and album of the song played before these).
pub async fn materialize_picks(
    state: &AppState,
    proxy: &SubsonicProxyService,
    station: &LastFmRadioStation,
    tracks: &[LastFmRadioTrack],
    reserve: &[LastFmRadioTrack],
    parameters: &Parameters,
    skip: &HashSet<String>,
    previous: Option<(&str, &str)>,
) -> Vec<(Song, LastFmRadioTrack)> {
    let resolver = LastFmRadioTrackResolver::new(
        proxy.clone(),
        state.metadata_service.clone(),
        state.external_id_registry.clone(),
    );
    let snapshot = state.settings.current();
    // A station is its owner's: a kid profile's stations are clean whatever the server's filter.
    let explicit_filter = crate::services::family::kid_filter::explicit_filter_for(
        &snapshot,
        Some(&state.family_store),
        Some(&station.owner),
    );
    let skip_covers_and_instrumentals = snapshot.soulseek.skip_covers_and_instrumentals;
    let mut resolved = resolve_station_tracks(
        &resolver,
        tracks,
        parameters,
        explicit_filter,
        skip_covers_and_instrumentals,
    )
    .await;
    resolved.retain(|(song, _)| !skip.contains(&song.id));
    if station.kind == LastFmRadioStationKind::Discovery {
        let cap = dotnet::round(
            resolved.len() as f64 * f64::from(snapshot.last_fm.effective_discovery_percent()) / 100.0,
            0,
        ) as usize;
        keep_library_share(&mut resolved, cap, reserve, |tracks: Vec<LastFmRadioTrack>| {
            let resolver = &resolver;
            async move {
                let mut songs = resolve_station_tracks(
                    resolver,
                    &tracks,
                    parameters,
                    explicit_filter,
                    skip_covers_and_instrumentals,
                )
                .await;
                songs.retain(|(song, _)| !skip.contains(&song.id));
                songs
            }
        })
        .await;
    }
    let mut spaced = Vec::with_capacity(resolved.len());
    let mut previous_artist: Option<String> = previous.map(|(artist, _)| artist.to_string());
    let mut previous_album_key: Option<String> = previous
        .filter(|(_, album)| !dotnet::is_blank(album))
        .map(|(artist, album)| format!("{artist}|{album}"));
    for (song, track) in resolved {
        let album_key = (!dotnet::is_blank(&song.album)).then(|| format!("{}|{}", song.artist, song.album));
        let same_artist = previous_artist
            .as_deref()
            .is_some_and(|previous| dotnet::eq_ignore_case(previous, &song.artist));
        let same_album = album_key.as_deref().is_some_and(|key| {
            previous_album_key
                .as_deref()
                .is_some_and(|previous| dotnet::eq_ignore_case(previous, key))
        });
        if same_artist || same_album {
            continue;
        }
        previous_artist = Some(song.artist.clone());
        previous_album_key = album_key;
        spaced.push((song, track));
    }
    spaced
}

/// The source that suggested each song, by id, the first one for an id.
pub fn suggested_sources(songs: &[Song]) -> HashMap<String, String> {
    let mut suggested: HashMap<String, String> = HashMap::new();
    for song in songs {
        if let Some(by) = &song.suggested_by {
            suggested.entry(song.id.clone()).or_insert_with(|| by.clone());
        }
    }
    suggested
}

/// A station's resolved songs as they go out, and the record of serving them: lengths filled
/// in, a song this listener's sync catalog holds as the catalog describes it (keeping the source
/// in `suggested`), counted as served by radio, registered as radio songs, their YouTube ids
/// warmed in the background, and remembered for the station's next ask.
pub fn serve_station_songs(
    state: &AppState,
    username: &str,
    mut songs: Vec<Song>,
    suggested: &HashMap<String, String>,
) -> Vec<Song> {
    // Before the catalog swap below: these rows are this response's own, while a catalog
    // row is shared with every other response that serves it.
    state.metadata_service.complete_song_lengths(&mut songs);

    // A song this user's sync catalog also holds goes out as the catalog describes it:
    // same album, and filed under the library's own artist where there is one. A syncing
    // client stores whichever description it read last, so the two must not disagree.
    let songs: Vec<_> = songs
        .into_iter()
        .map(|song| {
            if song.is_local {
                return song;
            }
            state
                .sync_catalog
                .try_get_song(username, &song.id)
                .unwrap_or(song)
        })
        .collect();
    // The discovery blend and the catalog swap hand back other objects; the source rides on by
    // id, on a copy, since a catalog row is shared with other responses.
    let songs: Vec<_> = songs
        .into_iter()
        .map(|mut song| {
            if let Some(by) = suggested.get(&song.id)
                && song.suggested_by.as_ref() != Some(by)
            {
                song.suggested_by = Some(by.clone());
            }
            song
        })
        .collect();
    // A station refetched by a syncing client is not the listener choosing it, so it never
    // renews the window in which a start counts for radio.
    state.radio_outcomes.served(
        Some(username),
        songs.iter().map(|song| {
            (
                song.id.as_str(),
                radio_provider::from_display_name(song.suggested_by.as_deref()),
            )
        }),
        None,
        false,
    );
    state
        .radio_queues
        .register(songs.iter().map(|song| song.id.clone()));
    // A station's songs (the radio stations and the mixes on them) are radio songs for the ones
    // fetched ahead.
    state
        .prefetch
        .note_radio(&songs.iter().map(|song| song.id.clone()).collect::<Vec<_>>());
    state
        .last_fm_radio_recommendations
        .remember_served(username, &songs);
    let metadata = Arc::clone(&state.metadata_service);
    let prewarm = songs.clone();
    tokio::spawn(async move { metadata.prewarm_you_tube_ids(&prewarm, 8).await });
    songs
}

/// Holds a mix to at most `cap` library songs. Each library song past the first `cap` gives its
/// place to the next outside song the reserve resolves to, in the reserve's order; when the
/// reserve runs out, the library songs left keep their places, so a thin reserve still gives a
/// full mix. The reserve is resolved only when a place needs it.
async fn keep_library_share<F, Fut>(
    songs: &mut [(Song, LastFmRadioTrack)],
    cap: usize,
    reserve: &[LastFmRadioTrack],
    resolve: F,
) where
    F: Fn(Vec<LastFmRadioTrack>) -> Fut,
    Fut: std::future::Future<Output = Vec<(Song, LastFmRadioTrack)>>,
{
    let over: Vec<usize> = songs
        .iter()
        .enumerate()
        .filter(|(_, (song, _))| song.is_local)
        .map(|(index, _)| index)
        .skip(cap)
        .collect();
    if over.is_empty() {
        return;
    }
    let mut present: std::collections::HashSet<String> =
        songs.iter().map(|(song, _)| song.id.clone()).collect();
    let mut stand_ins = Vec::with_capacity(over.len());
    for chunk in reserve.chunks(over.len().max(4)) {
        if stand_ins.len() >= over.len() {
            break;
        }
        for (song, track) in resolve(chunk.to_vec()).await {
            if stand_ins.len() >= over.len() {
                break;
            }
            if !song.is_local && present.insert(song.id.clone()) {
                stand_ins.push((song, track));
            }
        }
    }
    for (index, stand_in) in over.into_iter().zip(stand_ins) {
        songs[index] = stand_in;
    }
}

/// Each track resolved, as a station plays it, beside the track: the library's copy first, else
/// the outside one; a sounds-alike song only from the library; the explicit filter applied, and
/// outside cover, karaoke and instrumental takes left out while Skip covers and instrumentals is
/// on. Tracks that resolve to nothing are left out.
async fn resolve_station_tracks(
    resolver: &LastFmRadioTrackResolver,
    tracks: &[LastFmRadioTrack],
    parameters: &Parameters,
    explicit_filter: ExplicitFilter,
    skip_covers_and_instrumentals: bool,
) -> Vec<(Song, LastFmRadioTrack)> {
    let gate = Semaphore::new(4);
    let tasks = tracks.iter().map(|track| {
        let gate = &gate;
        async move {
            let _permit = gate.acquire().await.expect("the gate is never closed");
            let mut song = match resolver
                .resolve(
                    &track.artist,
                    &track.title,
                    track.duration,
                    parameters,
                    track.you_tube_id.as_deref(),
                )
                .await
            {
                Some(song) => song,
                None => Song {
                    id: track.resolved_id.clone().unwrap_or_default(),
                    artist: track.artist.clone(),
                    title: track.title.clone(),
                    album: track.album.clone().unwrap_or_else(|| track.title.clone()),
                    genre: track.genre.clone(),
                    duration: track.duration,
                    year: track.year,
                    is_local: false,
                    external_provider: track.external_provider.clone(),
                    external_id: track.resolved_id.clone(),
                    ..Default::default()
                },
            };
            // A song that sounded like the seed was a library song; if it is gone, it is not
            // worth streaming a stranger's copy of it.
            if track.source == radio_provider::SOUNDS_ALIKE && !song.is_local {
                return (Song::default(), track.clone());
            }
            song.suggested_by = radio_provider::display_name(Some(&track.source)).map(str::to_string);
            (song, track.clone())
        }
    });
    futures::future::join_all(tasks)
        .await
        .into_iter()
        .filter(|(song, _)| !song.id.is_empty())
        .filter(|(song, _)| match explicit_filter {
            ExplicitFilter::CleanOnly => {
                octo_core::models::domain::explicit_status::clean_only_allows(song.explicit_content_lyrics)
            }
            ExplicitFilter::ExplicitOnly => song.explicit_content_lyrics != Some(3),
            ExplicitFilter::All => true,
        })
        .filter(|(song, _)| !skipped_takes::hides_song(song, skip_covers_and_instrumentals))
        .collect()
}

/// Who a native (Navidrome API) request is: `u` when it has one, else the owner of the
/// Navidrome JWT it carries (a sign-in Octo saw pass through, or the token's own claims).
pub fn native_username(state: &AppState, parameters: &Parameters, headers: &HeaderMap) -> String {
    if let Some(username) = parameters.get("u").filter(|u| !u.is_empty()) {
        return username.clone();
    }
    let first = |name: &str| {
        headers
            .get(name)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
    };
    let header = first("X-Nd-Authorization").or_else(|| first("Authorization"));
    let token = header.map(|header| {
        if dotnet::starts_with_ignore_case(&header, "Bearer ") {
            header["Bearer ".len()..].to_string()
        } else {
            header
        }
    });
    if let Some(captured) = state
        .navidrome_identity
        .username_for_native_token(token.as_deref())
        .filter(|u| !u.is_empty())
    {
        return captured;
    }
    // An accepted but opaque token simply exposes no Radio rows.
    jwt_username(token.as_deref()).unwrap_or_default()
}

/// The first of the JWT payload's name claims: a claim that is not a string gives no name at
/// all.
fn jwt_username(token: Option<&str>) -> Option<String> {
    use base64::Engine;
    let token = token?;
    let payload = token.split('.').nth(1)?;
    let payload = payload.replace('-', "+").replace('_', "/");
    let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
    let bytes = base64::engine::general_purpose::STANDARD.decode(padded).ok()?;
    let document: Value = serde_json::from_slice(&bytes).ok()?;
    let object = document.as_object()?;
    for claim in ["username", "preferred_username", "user", "name", "sub"] {
        match object.get(claim) {
            None => {}
            Some(Value::String(found)) if !found.is_empty() => return Some(found.clone()),
            Some(Value::String(_)) | Some(Value::Null) => {}
            Some(_) => return None,
        }
    }
    None
}

/// The parameters as `(&str, &str)` pairs, for the session store's `issue`.
pub fn pairs(parameters: &IndexMap<String, String>) -> impl Iterator<Item = (&str, &str)> {
    parameters.iter().map(|(k, v)| (k.as_str(), v.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_try_parse_allows_white_space_and_a_sign_within_32_bits() {
        for (text, expected) in [
            ("20", Some(20)),
            (" 7 ", Some(7)),
            ("-3", Some(-3)),
            ("+4", Some(4)),
            ("1.5", None),
            ("", None),
            ("2147483648", None),
            ("-2147483648", Some(i32::MIN)),
        ] {
            assert_eq!(int_try_parse(text), expected, "{text}");
        }
    }

    #[test]
    fn octo_playlist_ids_are_22_characters_starting_or_or_og() {
        assert!(is_octo_playlist_id("or12345678901234567890", false));
        assert!(is_octo_playlist_id("og12345678901234567890", false));
        assert!(!is_octo_playlist_id("oa12345678901234567890", false));
        assert!(!is_octo_playlist_id("or1234567890123456789", false));
        assert!(!is_octo_playlist_id("OR12345678901234567890", false));
    }

    #[test]
    fn of_ids_rejected_when_family_off() {
        assert!(!is_octo_playlist_id("of12345678901234567890", false));
        assert!(is_octo_playlist_id("of12345678901234567890", true));
        assert!(!is_octo_playlist_id("of1234567890123456789", true));
    }

    #[test]
    fn failed_bodies_are_told_apart_from_empty_and_unreadable_ones() {
        let failed_json = br#"{"subsonic-response":{"status":"FAILED"}}"#;
        assert!(is_failed_subsonic_body(
            Some(failed_json),
            Some("application/json")
        ));
        assert!(!is_failed_subsonic_body(Some(b"{}"), Some("application/json")));
        assert!(is_failed_subsonic_body(
            Some(br#"<subsonic-response status="failed"/>"#),
            Some("text/xml")
        ));
        assert!(!is_failed_subsonic_body(Some(b"not xml"), None));
        assert!(!is_failed_subsonic_body(None, None));
    }

    #[test]
    fn local_song_ids_come_in_response_order_from_either_shape() {
        let json =
            br#"{"subsonic-response":{"searchResult2":{"song":[{"id":"a"},{"id":""},{"id":3},{"id":"b"}]}}}"#;
        assert_eq!(
            extract_local_song_ids(Some(json), Some("application/json")),
            ["a", "b"]
        );
        let xml = br#"<subsonic-response xmlns="http://subsonic.org/restapi"><searchResult3><song id="x"/><album id="y"><song id="z"/></album></searchResult3></subsonic-response>"#;
        assert_eq!(extract_local_song_ids(Some(xml), Some("text/xml")), ["x", "z"]);
    }

    #[test]
    fn playlist_names_reads_xml_as_well_as_json() {
        let json = br#"{"subsonic-response":{"status":"ok","playlists":{"playlist":[{"id":"1","name":"Octo: Remove"},{"id":"2","name":"Mine"}]}}}"#;
        let xml = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok"><playlists><playlist id="1" name="Octo: Remove"/><playlist id="2" name="Mine"/></playlists></subsonic-response>"#;
        assert_eq!(playlist_names(Some(json), "json"), ["Octo: Remove", "Mine"]);
        assert_eq!(playlist_names(Some(xml), "xml"), ["Octo: Remove", "Mine"]);
        assert!(playlist_names(Some(b"garbage"), "xml").is_empty());
        assert!(playlist_names(None, "json").is_empty());
    }

    #[test]
    fn jwt_names_come_from_the_first_name_claim() {
        use base64::Engine;
        let encode = |json: &str| {
            format!(
                "h.{}.s",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
            )
        };
        assert_eq!(
            jwt_username(Some(&encode(r#"{"sub":"bob","username":"alice"}"#))).as_deref(),
            Some("alice")
        );
        assert_eq!(
            jwt_username(Some(&encode(r#"{"sub":"bob"}"#))).as_deref(),
            Some("bob")
        );
        assert_eq!(jwt_username(Some(&encode(r#"{"username":7,"sub":"bob"}"#))), None);
        assert_eq!(jwt_username(Some("opaque")), None);
    }
}
