//! The catch-all's own answers: native Radio playlists, the external-id safety net, and the
//! Navidrome-native answers for Octo's outside songs, albums and artists, with discovery appended
//! to native searches.
//!
//! `http::catch_all` answers Octo-owned paths before reading the request, and relays the
//! request faithfully when none of these answers.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use octo_core::common::{dotnet, skipped_takes};
use octo_core::fingerprint::heart_gate::NATIVE_CLIENT;
use octo_core::json::datetime::format_utc;
use octo_core::json::dom::Node;
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::radio::LastFmRadioStation;
use octo_core::soulseek::{RoutingKind, StreamFacts};
use octo_subsonic::Parameters;
use octo_subsonic::subsonic_model_mapper::album_key;
use octo_subsonic::subsonic_response_builder::IdRegistry;
use serde_json::{Value, json};

use super::action_helpers::{
    error_object, file_reply, int_try_parse, is_octo_playlist_id, materialize_station, native_username,
    playlist_stations, queue_refresh_if_stale, written_body,
};
use super::held;
use super::playlists::{family_action_of, held_row, run_family_action, to_node};
use crate::app::AppState;
use crate::http::error::AppError;
use crate::services::family::audio_gate::ExplicitMark;
use crate::services::family::hooks::caller_for;
use crate::services::family::{FamilyCaller, kid_filter};
use crate::services::playlists::entries::Built;
use crate::services::playlists::held_playlists::{
    self, HeldCaller, HeldError, Pick, RelayAnswer, ServedView, UpdateOutcome, UpdateRequest,
};
use crate::services::playlists::held_store::HeldEntry;
use crate::services::soulseek::SoulseekMetadataService;
use crate::services::subsonic::{IncomingRequest, RawRelayResult, SubsonicProxyService};
use parking_lot::Mutex;
use tracing::warn;

/// The request as the catch-all read it.
pub struct NativeRequest<'a> {
    pub endpoint: &'a str,
    pub method: &'a Method,
    pub headers: &'a HeaderMap,
    pub parameters: &'a Parameters,
    pub format: &'a str,
    pub proxy: &'a SubsonicProxyService,
    /// The body as the client sent it.
    pub body: &'a Bytes,
}

/// The catch-all's own answers, tried in order. `None` when the request goes on to the faithful
/// relay.
pub async fn answer(state: &AppState, request: &NativeRequest<'_>) -> Result<Option<Response>, AppError> {
    if let Some(held) = try_serve_native_held(state, request).await? {
        return Ok(Some(held));
    }
    if let Some(radio) = try_serve_native_radio(state, request).await? {
        return Ok(Some(radio));
    }

    // Safety net (client-agnostic): any endpoint we don't explicitly handle,
    // called with one of our external ids, would relay to Navidrome and come
    // back "data not found": Navidrome has no such id. Degrade to a graceful
    // ok so a client we haven't specifically tested never errors on external
    // tracks. Endpoints that need real external data have their own handlers.
    if has_external_id(state, request.parameters) {
        return Ok(Some(
            state
                .subsonic_response_builder
                .create_response(request.format, &element_for(request.endpoint))
                .into_response(),
        ));
    }

    // Navidrome-native single-song detail for an external id. Native clients
    // load the now-playing view via GET /api/song/{id}; the id lives in the
    // path, not the query, so the check above (query-only) misses it and a
    // relay would 500. Serve the synthetic native object instead.
    if let Some(song) = try_serve_native_external_song(state, request.endpoint).await {
        return Ok(Some(song));
    }

    // Navidrome-native discovery. Navidrome-mode clients (e.g. Feishin) search
    // songs via GET /api/song?title=..., which otherwise relays straight through
    // and only ever surfaces the local library. This mirrors the Subsonic
    // search3 hijack onto the native API using the same discovery core, so
    // discovery is a property of the request shape, not the client's mode.
    if let Some(search) = try_inject_native_song_search(state, request).await {
        return Ok(Some(search));
    }

    // Native album detail for an external id. Same path-vs-query problem as the
    // song case above.
    if let Some(album) = try_serve_native_external_album(state, request).await {
        return Ok(Some(album));
    }

    // Native artist page for an outside artist: the artist (id in the path again) and
    // its albums, which the client asks for by artist_id, a key the safety net never reads.
    if let Some(artist) = try_serve_native_external_artist(state, request.endpoint).await {
        return Ok(Some(artist));
    }
    if let Some(albums) = try_serve_native_artist_albums(state, request).await {
        return Ok(Some(albums));
    }

    // Native album search, the twin of the search3 album injection.
    if let Some(search) = try_inject_native_album_search(state, request).await {
        return Ok(Some(search));
    }

    // Native album tracklist. In Navidrome mode a client does NOT get an album's
    // tracks from the album object; it asks for them separately by album_id. Note
    // the parameter is snake_case, so the safety net (which checks "albumId") never
    // intercepts it and this handler gets its chance.
    Ok(try_serve_native_album_songs(state, request).await)
}

/// True if any id-shaped parameter is one of Octo's external ids.
pub(crate) fn has_external_id(state: &AppState, parameters: &Parameters) -> bool {
    ["id", "mediaId", "albumId", "artistId"].iter().any(|key| {
        parameters
            .get(*key)
            .is_some_and(|value| !value.is_empty() && state.local_library.parse_song_id(value).0)
    })
}

/// rest/getSomething -> "something"; best-effort element name for an empty-ok response
/// (JSON ignores it; XML just needs a well-formed element).
pub fn element_for(endpoint: &str) -> String {
    let name = endpoint
        .split('/')
        .next_back()
        .unwrap_or("response")
        .replace(".view", "");
    let name = if dotnet::starts_with_ignore_case(&name, "get") && dotnet::utf16_len(&name) > 3 {
        let mut rest = name[3..].chars();
        match rest.next() {
            Some(first) => format!("{}{}", dotnet::to_lower_char(first), rest.as_str()),
            None => name,
        }
    } else {
        name
    };
    if name.is_empty() {
        "response".to_string()
    } else {
        name
    }
}

/// `_start`/`_end` as the native list reads them: `None` when missing or not a number.
fn page_number(parameters: &Parameters, name: &str) -> Option<i32> {
    parameters.get(name).and_then(|text| int_try_parse(text))
}

/// The rows from `start` up to `end`, the rest of the list when `end` is not past `start`:
/// the Navidrome UI asks for every playlist with `_end=0` or `_end=-1`.
fn page<T: Clone>(rows: &[T], start: i32, end: i32) -> Vec<T> {
    let skip = usize::try_from(start).unwrap_or(0);
    let take = if end > start {
        usize::try_from(i64::from(end) - i64::from(start)).unwrap_or(0)
    } else {
        usize::MAX
    };
    rows.iter().skip(skip).take(take).cloned().collect()
}

/// `body` as `application/json`, with `X-Total-Count` set.
fn file_with_total(body: Vec<u8>, total: usize) -> Response {
    let mut response = file_reply(&Bytes::from(body), Some("application/json"), "json");
    response
        .headers_mut()
        .insert(HeaderName::from_static("x-total-count"), HeaderValue::from(total));
    response
}

/// A JSON body with status 200 and `content_type`, written with no length.
fn written_json(body: Vec<u8>, extra: HeaderMap, content_type: &str) -> Response {
    let mut headers = extra;
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap_or_else(|_| HeaderValue::from_static("application/json")),
    );
    written_body(StatusCode::OK, headers, body)
}

async fn try_serve_native_radio(
    state: &AppState,
    request: &NativeRequest<'_>,
) -> Result<Option<Response>, AppError> {
    const PREFIX: &str = "api/playlist";
    let endpoint = request.endpoint;
    let is_list = endpoint.eq_ignore_ascii_case(PREFIX);
    if !is_list && !dotnet::starts_with_ignore_case(endpoint, &format!("{PREFIX}/")) {
        return Ok(None);
    }

    let tail = if endpoint.len() == PREFIX.len() {
        String::new()
    } else {
        endpoint[PREFIX.len() + 1..].trim_matches('/').to_string()
    };
    let id = tail
        .split('/')
        .find(|part| !part.is_empty())
        .unwrap_or("")
        .to_string();
    let reserved = is_octo_playlist_id(&id, state.settings.current().family.enabled);
    let is_get = request.method == Method::GET;
    if reserved && !is_get {
        return Ok(Some(error_object(
            StatusCode::METHOD_NOT_ALLOWED,
            "Octo's generated playlists are read-only",
        )));
    }
    if !tail.is_empty() && !reserved {
        return Ok(None);
    }

    // A successful upstream list validates the native bearer token before Octo
    // reveals per-user state. The JWT payload only identifies the profile after
    // Navidrome has accepted its signature and expiry.
    let relay_endpoint = if tail.is_empty() { endpoint } else { PREFIX };
    let mut relay_parameters = request.parameters.clone();
    if tail.is_empty() && is_get {
        // Page after merging, so Radio rows cannot disappear merely because the
        // upstream page was already full.
        relay_parameters.insert("_start".into(), "0".into());
        relay_parameters.insert("_end".into(), "1000".into());
    }
    // Not caught: a relay failure is the global handler's.
    let raw = request.proxy.relay_raw(relay_endpoint, &relay_parameters).await?;
    if !(200..300).contains(&raw.status) {
        let mut response = file_reply(
            &raw.body,
            Some(raw.content_type.as_deref().unwrap_or("application/json")),
            "json",
        );
        *response.status_mut() = StatusCode::from_u16(raw.status).unwrap_or(StatusCode::BAD_GATEWAY);
        return Ok(Some(response));
    }
    let username = native_username(state, request.parameters, request.headers);
    let stations = playlist_stations(state, &username);
    let unchanged = || {
        file_reply(
            &raw.body,
            Some(raw.content_type.as_deref().unwrap_or("application/json")),
            "json",
        )
    };
    if tail.is_empty() {
        let Ok(text) = std::str::from_utf8(&raw.body) else {
            return Ok(Some(unchanged()));
        };
        let Ok(Node::Array(mut rows)) = Node::parse(text) else {
            return Ok(Some(unchanged()));
        };
        if !state.held_playlists.is_empty() {
            let viewer = native_viewer(state, request);
            for row in rows.iter_mut() {
                if let Node::Object(fields) = row {
                    count_native_row(state, &viewer, fields);
                }
            }
        }
        let writable_only = request
            .parameters
            .get("smart")
            .is_some_and(|smart| smart.eq_ignore_ascii_case("false"));
        if !writable_only {
            rows.extend(stations.iter().map(native_station));
        }
        let total = rows.len();
        let start = page_number(request.parameters, "_start").unwrap_or(0).max(0);
        let end = page_number(request.parameters, "_end").unwrap_or(total as i32);
        let page = Node::Array(page(&rows, start, end));
        queue_refresh_if_stale(state, &username);
        return Ok(Some(file_with_total(
            page.to_json_string(false).into_bytes(),
            total,
        )));
    }

    let Some(station) = stations.into_iter().find(|station| station.id == id) else {
        return Ok(Some(error_object(
            StatusCode::NOT_FOUND,
            "Radio station not found for this user",
        )));
    };
    if dotnet::to_lower_invariant(&tail).ends_with("/tracks") {
        let mut songs = materialize_station(state, request.proxy, &station, request.parameters).await;
        state.metadata_service.complete_song_lengths(&mut songs);
        state
            .radio_queues
            .register(songs.iter().map(|song| song.id.clone()));
        state
            .prefetch
            .note_radio(&songs.iter().map(|song| song.id.clone()).collect::<Vec<_>>());
        let metadata = Arc::clone(&state.metadata_service);
        let prewarm = songs.clone();
        tokio::spawn(async move { metadata.prewarm_you_tube_ids(&prewarm, 8).await });
        let start = page_number(request.parameters, "_start").unwrap_or(0).max(0);
        let end = page_number(request.parameters, "_end").unwrap_or(songs.len() as i32);
        let rows: Vec<Node> = page(&songs, start, end)
            .iter()
            .map(|song| to_node(&native_song_object(state, song)))
            .collect();
        return Ok(Some(file_with_total(
            Node::Array(rows).to_json_string(false).into_bytes(),
            songs.len(),
        )));
    }
    Ok(Some(file_reply(
        &Bytes::from(native_station(&station).to_json_string(false)),
        Some("application/json"),
        "json",
    )))
}

fn native_station(station: &LastFmRadioStation) -> Node {
    let duration: i64 = station
        .tracks
        .iter()
        .map(|track| i64::from(track.duration.unwrap_or(0).max(0)))
        .sum();
    to_node(&json!({
        "id": station.id,
        "name": station.name,
        "comment": "Generated by Octo Radio",
        "ownerName": station.owner,
        "public": false,
        "songCount": station.tracks.len(),
        "duration": duration,
        "createdAt": format_utc(&station.created_utc),
        "updatedAt": format_utc(&station.changed_utc),
        "path": "",
        "smartPlaylist": true,
        "readonly": true,
        "validUntil": format_utc(&station.valid_until_utc),
    }))
}

/// Native single-song fetch for one of Octo's external ids. Navidrome-mode clients load the
/// now-playing detail via GET /api/song/{id}; relaying an external id to Navidrome 500s (it has
/// no such song). Rebuild the song from the id via the same metadata core getSong uses and
/// return it in native shape. `None` (fall through to relay) for anything but a leaf
/// external-id fetch.
async fn try_serve_native_external_song(state: &AppState, endpoint: &str) -> Option<Response> {
    let id = leaf_id(endpoint, "api/song/")?;
    let (is_external, provider, external_id) = state.local_library.parse_song_id(&id);
    if !is_external {
        return None;
    }
    let song = state
        .metadata_service
        .get_song(
            provider.as_deref().unwrap_or(""),
            external_id.as_deref().unwrap_or(""),
        )
        .await?;

    // Enrich (Deezer album/year/art) so the detail matches the search-list row exactly;
    // without this a client that refreshes now-playing from the detail would blank the
    // album. Cached, so this is cheap after the initial search.
    let mut one = vec![song];
    state.metadata_service.enrich_external_songs(&mut one).await;
    // A search answers before its rows' album years, and the track lookup it remembered has
    // none; this one song gets its year now.
    state.metadata_service.complete_song_year(&mut one[0]).await;

    // Lazy-resolve the accurate YouTube duration at play. Navidrome-mode clients re-fetch
    // this endpoint when a track starts, so this is where the scrub bar gets the real length
    // for results past the search's top-N (which are already resolved).
    state
        .metadata_service
        .resolve_top_durations(&mut one, false)
        .await;
    // The video's length above is what plays, so it stays; only a song still without a
    // length takes one the registry remembered.
    if one[0].duration.is_none_or(|d| d <= 0) {
        state.metadata_service.complete_song_lengths(&mut one);
    }

    let body = octo_core::json::to_string(&native_song_object(state, &one[0]));
    Some(written_json(
        body.into_bytes(),
        HeaderMap::new(),
        "application/json",
    ))
}

/// The id after `prefix` when it is a leaf (`api/song/{id}` and nothing below it).
fn leaf_id(endpoint: &str, prefix: &str) -> Option<String> {
    if !dotnet::starts_with_ignore_case(endpoint, prefix) {
        return None;
    }
    let id = endpoint[prefix.len()..].trim_matches('/');
    (!id.is_empty() && !id.contains('/')).then(|| id.to_string())
}

/// Native-API twin of the Subsonic search3 hijack. When a Navidrome-mode client searches songs
/// (GET /api/song?title=...), relay the real query, then append external discovery results
/// serialized in Navidrome's native song shape. Play and cover art need no special handling:
/// native clients stream via /rest/stream and fetch art via /rest/getCoverArt using the
/// salt+token from login, and Octo's handlers already resolve its external ids there.
///
/// `None` to fall through to the normal faithful relay whenever this is not a first-page
/// native song search we should touch, so library browsing, paging, and every other native
/// endpoint stay pure passthrough.
async fn try_inject_native_song_search(state: &AppState, request: &NativeRequest<'_>) -> Option<Response> {
    if !request.endpoint.eq_ignore_ascii_case("api/song") {
        return None;
    }

    // Only a text search carries discovery intent. No title filter = library
    // browse; a non-zero _start = a later page. Both stay passthrough so we
    // never duplicate injected rows across pages or disturb navigation.
    let term = request
        .parameters
        .get("title")
        .map_or("", |t| t.trim())
        .to_string();
    if dotnet::is_blank(&term) || page_number(request.parameters, "_start").is_some_and(|start| start > 0) {
        return None;
    }

    // Relay the real query first; we append to whatever the library returned.
    // Upstream trouble lets the normal path surface it.
    let raw = request
        .proxy
        .relay_raw(request.endpoint, request.parameters)
        .await
        .ok()?;
    if raw.status != 200 {
        return None;
    }

    // Native list endpoints answer with a bare JSON array + X-Total-Count. If the
    // body is any other shape (error object, unexpected version), don't touch it.
    let mut rows = native_array(&raw)?;

    // Stay inside the page window the client asked for so a single page holds
    // everything and the client never pages into a duplicated injection.
    const MAX_EXTERNAL_NATIVE: i64 = 60;
    let end = page_number(request.parameters, "_end").map_or(rows.len() as i64 + 60, i64::from);
    let target = (end - rows.len() as i64).clamp(0, MAX_EXTERNAL_NATIVE);
    if target <= 0 {
        return None;
    }

    // Same discovery core as Subsonic search3: Last.fm fan-out, Deezer enrich,
    // accurate YouTube durations for the top of the list. Shared with search3, so a
    // client that searches both ways for one query only pays for it once.
    let built = state.external_search.get(&term).await;
    let external_songs: Vec<&Song> = built.iter().take(target as usize).collect();
    if external_songs.is_empty() {
        return None;
    }
    rows.extend(
        external_songs
            .iter()
            .map(|song| to_node(&native_song_object(state, song))),
    );

    // Register for the scrobble-driven prewarm, same as search3.
    state
        .radio_queues
        .register(external_songs.iter().map(|song| song.id.clone()));

    Some(written_with_upstream_headers(&raw, rows))
}

/// The upstream array, when the relayed body is one.
fn native_array(raw: &RawRelayResult) -> Option<Vec<Node>> {
    let text = std::str::from_utf8(&raw.body).ok()?;
    match Node::parse(text).ok()? {
        Node::Array(rows) => Some(rows),
        _ => None,
    }
}

/// The appended list with the upstream's allowlisted headers (but its own `X-Total-Count`),
/// written to the response body.
fn written_with_upstream_headers(raw: &RawRelayResult, rows: Vec<Node>) -> Response {
    let mut headers = HeaderMap::new();
    for (name, value) in &raw.response_headers {
        if name.eq_ignore_ascii_case("X-Total-Count") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers.insert(
        HeaderName::from_static("x-total-count"),
        HeaderValue::from(rows.len()),
    );
    let body = Node::Array(rows).to_json_string(false).into_bytes();
    written_json(
        body,
        headers,
        raw.content_type.as_deref().unwrap_or("application/json"),
    )
}

/// Serializes one external Song into Navidrome's native song JSON shape. Only the fields a
/// Navidrome-mode client reads to render and play a row are populated. The id is Octo's
/// external id, which /rest/stream and /rest/getCoverArt resolve.
fn native_song_object(state: &AppState, song: &Song) -> Value {
    let artist_id = match song.artist_id.as_deref() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => format!("{}-ar", song.id),
    };
    let album_id = match song.album_id.as_deref() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => format!("{}-al", song.id),
    };
    let duration = song.duration.unwrap_or(0);
    // Navidrome-mode clients take their contract from HERE and never from
    // SubsonicResponseBuilder, so this has to follow the same setting or the native
    // path keeps promising m4a while /rest/stream hands back a FLAC. Note the two
    // serializers are not symmetric: this one emits no contentType at all, and writes
    // an unknown duration as 0 where the Subsonic one leaves the field out.
    //
    // The size is the stream's true byte count once it is known, and absent until then (and
    // always absent while waiting for lossless, whose bytes are not known until the FLAC lands).
    // The suffix and bit rate follow the format the stream is served in, when known.
    let lossless = state.settings.current().subsonic.wait_for_lossless_on_play;
    let facts = if lossless {
        StreamFacts::default()
    } else {
        state
            .stream_size_warmer
            .stream_facts(&song.id)
            .unwrap_or_default()
    };
    if !lossless {
        state.stream_size_warmer.outside_listed(&song.id);
        if facts.size.is_none() {
            state.stream_size_warmer.size_wanted(&song.id);
        }
    }
    let declared = facts.declared();
    let suffix = match &declared {
        _ if lossless => "flac".to_string(),
        Some(declared) => declared.suffix.clone(),
        None => "m4a".to_string(),
    };
    // Format 140 AAC ~128 kbps; FLAC lands ~850-1000.
    let bit_rate: Option<i64> = match &declared {
        _ if lossless => Some(950),
        Some(declared) => declared.bit_rate.map(i64::from),
        None => Some(128),
    };
    let album_artist = match song.album_artist.as_deref() {
        Some(album_artist) if !album_artist.is_empty() => album_artist.to_string(),
        _ => song.artist.clone(),
    };
    let mut object = json!({
        "id": song.id,
        "path": format!("{}/{}/{}.{suffix}", sanitize(&song.artist), sanitize(&song.album), sanitize(&song.title)),
        "title": song.title,
        "album": song.album,
        "artist": song.artist,
        "artistId": artist_id,
        "albumArtist": album_artist,
        "albumArtistId": artist_id,
        "albumId": album_id,
        "hasCoverArt": true,
        "trackNumber": song.track.unwrap_or(0),
        "discNumber": song.disc_number.unwrap_or(1),
        // Typed native clients read these as numbers: an unknown one is 0, never missing.
        "size": facts.size.unwrap_or(0),
        "suffix": suffix,
        "duration": duration,
        "bitRate": bit_rate.unwrap_or(0),
        "playCount": 0,
        // Fixed old timestamp: injected tracks are not "recently added" library
        // items, so they should never crowd a client's recently-added view.
        "createdAt": "2020-01-01T00:00:00Z",
        "updatedAt": "2020-01-01T00:00:00Z",
    });
    let fields = object.as_object_mut().expect("an object");
    if let Some(year) = song.year.filter(|year| *year > 0) {
        fields.insert("year".into(), year.into());
    }
    if let Some(genre) = song.genre.as_deref().filter(|genre| !genre.is_empty()) {
        fields.insert("genre".into(), genre.into());
    }
    object
}

/// Native-API twin of the search3 album injection. Navidrome filters albums with a full-text
/// "name" parameter. `None` for anything that is not a first-page album search, so library
/// browsing and paging stay pure passthrough.
///
/// NOTE: Feishin 1.3.0 does NOT reach this. Its album search goes through rest/search3.view
/// even in Navidrome mode, and every /api/album call it makes is browsing with no "name"
/// filter. This is kept for clients that DO filter by name; album detail (/api/album/{id}) and
/// its tracklist (/api/song?album_id=) are the two native handlers Feishin depends on.
async fn try_inject_native_album_search(state: &AppState, request: &NativeRequest<'_>) -> Option<Response> {
    if !request.endpoint.eq_ignore_ascii_case("api/album") {
        return None;
    }

    let term = request
        .parameters
        .get("name")
        .map_or("", |t| t.trim())
        .to_string();
    if dotnet::is_blank(&term) || page_number(request.parameters, "_start").is_some_and(|start| start > 0) {
        return None;
    }

    let raw = request
        .proxy
        .relay_raw(request.endpoint, request.parameters)
        .await
        .ok()?;
    if raw.status != 200 {
        return None;
    }
    let mut rows = native_array(&raw)?;

    const MAX_EXTERNAL_ALBUMS: i64 = 20;
    let end = page_number(request.parameters, "_end").map_or(rows.len() as i64 + 20, i64::from);
    let target = (end - rows.len() as i64).clamp(0, MAX_EXTERNAL_ALBUMS);
    if target <= 0 {
        return None;
    }

    let external_albums = skipped_takes::listed_albums(
        state.metadata_service.search_albums(&term, target as i32).await,
        state.settings.current().soulseek.skip_covers_and_instrumentals,
    );
    if external_albums.is_empty() {
        return None;
    }

    // Newer Navidrome is multi-library and rows carry a libraryId. Inherit it from a
    // real row rather than hardcoding, so injected albums belong to the same library.
    let library_id = match rows.first() {
        Some(first @ Node::Object(_)) => first
            .get("libraryId")
            .filter(|lib| !matches!(lib, Node::Null))
            .and_then(|lib| int_try_parse(&node_text(lib)))
            .unwrap_or(1),
        _ => 1,
    };

    // Don't inject an album the library already returned. The key is the one search3's
    // merge uses, so a title the catalog spells with a curly apostrophe or an accent the
    // library's tags lack is still the same album.
    let local_keys: std::collections::HashSet<String> = rows
        .iter()
        .filter(|row| row.is_object())
        .filter_map(|row| {
            let name = row.get("name").and_then(non_null_text);
            let album_artist = row.get("albumArtist").and_then(non_null_text);
            album_key(album_artist.as_deref(), name.as_deref())
        })
        .collect();

    let mut added = 0;
    for album in &external_albums {
        if album_key(Some(&album.artist), Some(&album.title)).is_some_and(|key| local_keys.contains(&key)) {
            continue;
        }
        rows.push(to_node(&native_album_object(album, library_id)));
        added += 1;
    }
    if added == 0 {
        return None;
    }
    Some(written_with_upstream_headers(&raw, rows))
}

/// A node's text: a string's own text, anything else as JSON.
fn node_text(node: &Node) -> String {
    match node {
        Node::String(s) => s.clone(),
        other => other.to_json_string(false),
    }
}

fn non_null_text(node: &Node) -> Option<String> {
    (!matches!(node, Node::Null)).then(|| node_text(node))
}

/// Native single-album detail: GET /api/album/{id} for one of Octo's album ids. Opening it
/// records the album as the reader's context of each of its songs.
async fn try_serve_native_external_album(state: &AppState, request: &NativeRequest<'_>) -> Option<Response> {
    let id = leaf_id(request.endpoint, "api/album/")?;
    if routing_kind(state, &id) != Some(RoutingKind::Album) {
        return None;
    }
    let album = state
        .metadata_service
        .get_album(SoulseekMetadataService::PROVIDER_NAME, &id)
        .await?;
    remember_native_listing(state, request, &id, &album);
    let body = octo_core::json::to_string(&native_album_object(&album, 1));
    Some(written_json(
        body.into_bytes(),
        HeaderMap::new(),
        "application/json",
    ))
}

/// An outside album read through Navidrome's own API, recorded as the reader's context of its
/// songs. That API sends no client name, so the context is filed under [`NATIVE_CLIENT`].
fn remember_native_listing(state: &AppState, request: &NativeRequest<'_>, album_id: &str, album: &Album) {
    let user = native_username(state, request.parameters, request.headers);
    super::browsing::remember_listed_album(state, &user, Some(NATIVE_CLIENT), album_id, album);
}

fn routing_kind(state: &AppState, id: &str) -> Option<RoutingKind> {
    state
        .external_id_registry
        .lookup(id)
        .map(|routing| routing.snapshot().kind)
}

/// Native album tracklist: GET /api/song?album_id={externalAlbumId}. A Navidrome-mode client
/// fetches an album's tracks separately from the album object, so without this an injected
/// album opens empty.
async fn try_serve_native_album_songs(state: &AppState, request: &NativeRequest<'_>) -> Option<Response> {
    if !request.endpoint.eq_ignore_ascii_case("api/song") {
        return None;
    }
    let album_id = request.parameters.get("album_id").map_or("", |id| id.trim());
    if album_id.is_empty() || routing_kind(state, album_id) != Some(RoutingKind::Album) {
        return None;
    }
    let album = state
        .metadata_service
        .get_album(SoulseekMetadataService::PROVIDER_NAME, album_id)
        .await?;
    remember_native_listing(state, request, album_id, &album);
    let rows: Vec<Value> = album
        .songs
        .iter()
        .map(|song| native_song_object(state, song))
        .collect();
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-total-count"),
        HeaderValue::from(album.songs.len()),
    );
    let body = octo_core::json::to_string(&Value::Array(rows));
    Some(written_json(body.into_bytes(), headers, "application/json"))
}

/// Native artist detail: GET /api/artist/{id} for one of Octo's artist ids. A Navidrome-mode
/// client opens an artist page with this. Relayed, Navidrome has no such artist, the relay
/// fails, and the page waits forever. A library artist falls through.
async fn try_serve_native_external_artist(state: &AppState, endpoint: &str) -> Option<Response> {
    let id = leaf_id(endpoint, "api/artist/")?;
    if routing_kind(state, &id) != Some(RoutingKind::Artist) {
        return None;
    }
    let artist = state
        .metadata_service
        .get_artist(SoulseekMetadataService::PROVIDER_NAME, &id)
        .await?;
    // The counts come from the same list the page shows, so they agree with it. Only the
    // track counts already known: the page asks for that list at the same moment, and
    // that request is the one that asks the catalog for the rest.
    let albums = outside_artist_albums(state, &id, &artist.name, true).await;
    let body = octo_core::json::to_string(&native_artist_object(&artist, &albums));
    Some(written_json(
        body.into_bytes(),
        HeaderMap::new(),
        "application/json",
    ))
}

/// Native albums by artist: GET /api/album?artist_id={outside artist id}, the list an artist
/// page shows. The rows are the same albums getArtist lists for that artist.
async fn try_serve_native_artist_albums(state: &AppState, request: &NativeRequest<'_>) -> Option<Response> {
    if !request.endpoint.eq_ignore_ascii_case("api/album") {
        return None;
    }
    let artist_id = request.parameters.get("artist_id").map_or("", |id| id.trim());
    if artist_id.is_empty() {
        return None;
    }
    let routing = state.external_id_registry.lookup(artist_id)?.snapshot();
    if routing.kind != RoutingKind::Artist {
        return None;
    }
    let albums =
        outside_artist_albums(state, artist_id, routing.artist.as_deref().unwrap_or(""), false).await;

    // Feishin asks for a whole discography with _end=-1, so an end that is not past the
    // start means the rest of the list rather than nothing.
    let total = albums.len() as i32;
    let start = match page_number(request.parameters, "_start") {
        Some(start) if start > 0 => start.min(total),
        _ => 0,
    };
    let end = match page_number(request.parameters, "_end") {
        Some(end) if end > start => end.min(total),
        _ => total,
    };
    let rows: Vec<Value> = albums
        .iter()
        .skip(start as usize)
        .take((end - start).max(0) as usize)
        .map(|album| native_album_object(album, 1))
        .collect();
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-total-count"),
        HeaderValue::from(albums.len()),
    );
    let body = octo_core::json::to_string(&Value::Array(rows));
    Some(written_json(body.into_bytes(), headers, "application/json"))
}

/// An outside artist's albums, each naming the artist and linking back to the artist's own
/// id, as getArtist fills them: the catalog's listing carries neither.
async fn outside_artist_albums(
    state: &AppState,
    artist_id: &str,
    artist_name: &str,
    known_counts_only: bool,
) -> Vec<Album> {
    let metadata = &state.metadata_service;
    let mut albums = if known_counts_only {
        metadata
            .get_artist_albums_known_counts(SoulseekMetadataService::PROVIDER_NAME, artist_id)
            .await
    } else {
        metadata
            .get_artist_albums(SoulseekMetadataService::PROVIDER_NAME, artist_id)
            .await
    };
    for album in &mut albums {
        if album.artist.is_empty() {
            album.artist = artist_name.to_string();
        }
        if album.artist_id.as_deref().is_none_or(str::is_empty) {
            album.artist_id = Some(artist_id.to_string());
        }
    }
    albums
}

/// Serializes one outside Artist into Navidrome's native artist JSON shape. Counts go out both
/// flat (older Navidrome) and under "stats" by role (newer Navidrome), since clients read one
/// or the other. The image URLs are the three getArtistInfo2 gives.
fn native_artist_object(artist: &Artist, albums: &[Album]) -> Value {
    let song_count: i64 = albums
        .iter()
        .map(|album| i64::from(album.song_count.unwrap_or(0)))
        .sum();
    let stats = || json!({ "albumCount": albums.len(), "songCount": song_count, "size": 0 });
    let mut object = json!({
        "id": artist.id,
        "name": artist.name,
        "albumCount": albums.len(),
        "songCount": song_count,
        "size": 0,
        "stats": { "albumartist": stats(), "artist": stats() },
        "playCount": 0,
        "missing": false,
        // Fixed old timestamp, same reasoning as the native song object.
        "createdAt": "2020-01-01T00:00:00Z",
        "updatedAt": "2020-01-01T00:00:00Z",
    });
    if let Some(image) = artist.image_url.as_deref().filter(|url| !url.is_empty()) {
        let fields = object.as_object_mut().expect("an object");
        fields.insert("smallImageUrl".into(), image.into());
        fields.insert("mediumImageUrl".into(), image.into());
        fields.insert("largeImageUrl".into(), image.into());
    }
    object
}

/// Serializes one external Album into Navidrome's native album JSON shape. Navidrome's album
/// model has NO "artist"/"artistId" field, it uses albumArtist/albumArtistId, and
/// "duration" is seconds as a float.
pub(crate) fn native_album_object(album: &Album, library_id: i32) -> Value {
    let duration: i64 = album
        .songs
        .iter()
        .map(|song| i64::from(song.duration.unwrap_or(0)))
        .sum();
    let song_count = if album.songs.is_empty() {
        album.song_count.unwrap_or(0) as usize
    } else {
        album.songs.len()
    };
    let year = album.year.unwrap_or(0);
    let album_artist_id = match album.artist_id.as_deref() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => format!("{}-ar", album.id),
    };
    let mut object = json!({
        "id": album.id,
        "libraryId": library_id,
        "name": album.title,
        "albumArtist": album.artist,
        "albumArtistId": album_artist_id,
        "maxYear": year,
        "minYear": year,
        "compilation": false,
        // Explicitly not missing: a client that respects this flag hides rows otherwise.
        "missing": false,
        "songCount": song_count,
        "duration": duration as f64,
        "size": 0,
        "playCount": 0,
        // Fixed old timestamp, same reasoning as the native song object: injected rows
        // must never crowd a client's recently-added view.
        "createdAt": "2020-01-01T00:00:00Z",
        "updatedAt": "2020-01-01T00:00:00Z",
    });
    let fields = object.as_object_mut().expect("an object");
    if let Some(genre) = album.genre.as_deref().filter(|genre| !genre.is_empty()) {
        fields.insert("genre".into(), genre.into());
    }
    // Navidrome's own album JSON carries the release types among its tags, in the tag's
    // lowercase words, and a Navidrome-mode client groups an artist's page by them.
    if !album.release_types.is_empty() {
        fields.insert(
            "tags".into(),
            json!({
                "releasetype": album
                    .release_types
                    .iter()
                    .map(|kind| dotnet::to_lower_invariant(kind))
                    .collect::<Vec<_>>(),
            }),
        );
    }
    object
}

fn sanitize(text: &str) -> String {
    if text.is_empty() {
        "Unknown".to_string()
    } else {
        text.replace(['/', '\\'], "_")
    }
}

// ---------------------------------------------------------------------------------------
// Held playlists through Navidrome's own API
// ---------------------------------------------------------------------------------------

/// The native caller as the family rules see them, and whether they hear clean songs only.
#[derive(Clone)]
struct NativeViewer {
    user: String,
    caller: FamilyCaller,
    kid: bool,
    unrated: bool,
}

fn native_viewer(state: &AppState, request: &NativeRequest<'_>) -> NativeViewer {
    let user = native_username(state, request.parameters, request.headers);
    let caller = caller_for(&state.family_store, &user);
    let settings = state.settings.current();
    NativeViewer {
        kid: settings.family.enabled && kid_filter::is_kid(&caller),
        unrated: settings.family.kid_unrated_is_explicit,
        user,
        caller,
    }
}

impl NativeViewer {
    /// Who the held service records a native change as: native requests name no client.
    fn held_caller(&self) -> HeldCaller {
        HeldCaller {
            username: self.user.trim().to_string(),
            user: self.user.trim().to_string(),
            client: String::new(),
        }
    }
}

/// A held playlist's native row with its streamed songs counted for this viewer, and
/// `updatedAt` the newer of Navidrome's and the list's own.
fn count_native_row(state: &AppState, viewer: &NativeViewer, row: &mut indexmap::IndexMap<String, Node>) {
    let Some(id) = row
        .get("id")
        .and_then(Node::as_str)
        .filter(|id| state.held_playlists.contains(id))
        .map(str::to_string)
    else {
        return;
    };
    let Some(view) = state.held_playlists.view_of(&id) else {
        return;
    };
    let counted = held_row(&view.entries, view.changed, viewer.kid, viewer.unrated);
    let number = |node: Option<&Node>| match node {
        Some(Node::Number(text)) => text.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    let songs = number(row.get("songCount")) as i64 + counted.songs;
    let seconds = number(row.get("duration")) + counted.seconds as f64;
    let listed = row
        .get("updatedAt")
        .and_then(Node::as_str)
        .and_then(held::parse_instant);
    row.insert("songCount".into(), Node::Number(songs.to_string()));
    row.insert(
        "duration".into(),
        Node::Number(octo_core::json::format_double(seconds)),
    );
    let newest = listed.map_or(counted.changed, |at| at.max(counted.changed));
    row.insert("updatedAt".into(), Node::String(format_utc(&newest)));
}

/// A JSON answer with status 200.
fn native_json(value: &Value) -> Response {
    written_json(
        octo_core::json::to_string(value).into_bytes(),
        HeaderMap::new(),
        "application/json",
    )
}

/// The native answer to a held playlist error: its HTTP status and `{"error": message}`.
fn native_held_error(failure: HeldError) -> Response {
    let status = StatusCode::from_u16(failure.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
    error_object(status, failure.message())
}

/// A relayed answer as it came: status, the allowlisted headers and the body.
fn relayed_raw(state: &AppState, raw: RawRelayResult) -> Response {
    crate::http::catch_all::write_raw_relay(raw, "json", &state.subsonic_response_builder)
}

/// A proxy that calls Navidrome's own API as the native caller (their sign-in headers), with
/// its own method and JSON body.
fn caller_proxy(
    state: &AppState,
    headers: &HeaderMap,
    method: Method,
    body: Option<Bytes>,
) -> SubsonicProxyService {
    let mut headers = headers.clone();
    headers.remove(header::CONTENT_LENGTH);
    if body.is_some() {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    } else {
        headers.remove(header::CONTENT_TYPE);
    }
    let incoming = IncomingRequest::new(method, headers, None, body.unwrap_or_default());
    state.subsonic_proxy.with_request(Arc::new(incoming))
}

/// A playlist as the caller's native read shows it.
struct NativePlaylist {
    name: String,
    owner: String,
    smart: bool,
    synced: bool,
}

/// `GET api/playlist/{id}` as the caller. The error is the answer to send.
async fn native_playlist(
    state: &AppState,
    request: &NativeRequest<'_>,
    id: &str,
) -> Result<NativePlaylist, Response> {
    let raw = caller_proxy(state, request.headers, Method::GET, None)
        .relay_raw(&format!("api/playlist/{id}"), std::iter::empty::<(&str, &str)>())
        .await
        .map_err(|failure| AppError::from(failure).into_response())?;
    if !(200..300).contains(&raw.status) {
        return Err(relayed_raw(state, raw));
    }
    let value: Value = serde_json::from_slice(&raw.body).unwrap_or(Value::Null);
    let text = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let smart = value.get("rules").is_some_and(|rules| match rules {
        Value::Null => false,
        Value::Object(fields) => !fields.is_empty(),
        _ => true,
    });
    Ok(NativePlaylist {
        name: text("name"),
        owner: text("ownerName"),
        smart,
        synced: value.get("sync").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// The library rows of a playlist Navidrome lets the caller see, as their own native read; None
/// when Navidrome did not answer, the answer to send when it refused.
async fn caller_tracks(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
) -> Result<Vec<Value>, Option<Response>> {
    let raw = caller_proxy(state, headers, Method::GET, None)
        .relay_raw(
            &format!("api/playlist/{id}/tracks"),
            std::iter::empty::<(&str, &str)>(),
        )
        .await
        .map_err(|_| None)?;
    if !(200..300).contains(&raw.status) {
        return Err(Some(relayed_raw(state, raw)));
    }
    Ok(serde_json::from_slice::<Value>(&raw.body)
        .ok()
        .and_then(|rows| rows.as_array().cloned())
        .unwrap_or_default())
}

/// One row of a held playlist as the native API serves it, with the place of its entry.
struct NativeRow {
    entry: usize,
    row: Value,
}

/// The held entries as the native caller is served them (the service's `walk` over the caller's
/// own rows): a library entry takes the next unused row the caller's read gave for that song,
/// else its outside fallback; a streamed entry is its snapshot in native shape. A managed kid is
/// not served explicit songs. `id` is the 1-based place over this list.
fn held_rows(
    state: &AppState,
    entries: &[HeldEntry],
    library: Vec<Value>,
    viewer: &NativeViewer,
    id: &str,
) -> Vec<NativeRow> {
    let media: Vec<String> = library
        .iter()
        .map(|row| {
            row.get("mediaFileId")
                .and_then(Value::as_str)
                .or_else(|| row.get("id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let mut rows = Vec::new();
    for pick in held_playlists::walk(entries, &media) {
        match pick {
            Pick::Library { entry, row } => {
                let answered = library[row].get("explicitStatus").and_then(Value::as_str);
                if viewer.kid
                    && kid_filter::song_is_explicit(
                        &state.library_index,
                        &media[row],
                        answered,
                        viewer.unrated,
                    )
                {
                    continue;
                }
                rows.push(NativeRow {
                    entry,
                    row: library[row].clone(),
                });
            }
            Pick::Outside { entry, id: outside } => {
                let Some(song) =
                    held_playlists::snapshot_song(&entries[entry]).filter(|song| song.id == outside)
                else {
                    continue;
                };
                if viewer.kid
                    && ExplicitMark::from_catalog(song.explicit_content_lyrics)
                        .blocks_clean_only(viewer.unrated)
                {
                    continue;
                }
                let mut row = native_song_object(state, &song);
                if let Some(fields) = row.as_object_mut() {
                    fields.insert("mediaFileId".into(), outside.into());
                }
                rows.push(NativeRow { entry, row });
            }
        }
    }
    for (place, served) in rows.iter_mut().enumerate() {
        if let Some(fields) = served.row.as_object_mut() {
            fields.insert("id".into(), (place + 1).to_string().into());
            fields.insert("playlistId".into(), id.into());
        }
    }
    rows
}

/// The native caller's served list, worked out under the playlist's lock with a fresh read of
/// their own rows: what the held service counts places over.
struct NativeList {
    state: AppState,
    headers: HeaderMap,
    viewer: NativeViewer,
    id: String,
}

#[async_trait]
impl ServedView for NativeList {
    async fn served(&self, entries: &[HeldEntry]) -> Option<Vec<usize>> {
        let library = caller_tracks(&self.state, &self.headers, &self.id).await.ok()?;
        Some(
            held_rows(&self.state, entries, library, &self.viewer, &self.id)
                .into_iter()
                .map(|served| served.entry)
                .collect(),
        )
    }
}

/// A held playlist read, changed or deleted through Navidrome's own API, a playlist deleted, or
/// songs added to any playlist. None for the faithful relay.
async fn try_serve_native_held(
    state: &AppState,
    request: &NativeRequest<'_>,
) -> Result<Option<Response>, AppError> {
    const PREFIX: &str = "api/playlist/";
    let endpoint = request.endpoint;
    if !dotnet::starts_with_ignore_case(endpoint, PREFIX) {
        return Ok(None);
    }
    let parts: Vec<&str> = endpoint[PREFIX.len()..]
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let Some(id) = parts.first().map(|id| id.to_string()) else {
        return Ok(None);
    };
    if is_octo_playlist_id(&id, state.settings.current().family.enabled) {
        return Ok(None);
    }
    let tracks = parts
        .get(1)
        .is_some_and(|part| part.eq_ignore_ascii_case("tracks"));
    let method = request.method;
    match (parts.len(), tracks) {
        (1, _) if method == Method::GET => native_read(state, request, &id).await,
        (1, _) if method == Method::DELETE => native_delete_playlist(state, request, &id).await.map(Some),
        (2, true) if method == Method::GET => native_tracks(state, request, &id).await,
        (2, true) if method == Method::POST => native_add(state, request, &id).await,
        (2, true) if method == Method::DELETE => native_remove(state, request, &id).await,
        (3, true) if method == Method::PUT => native_move(state, request, &id, parts[2]).await,
        _ => Ok(None),
    }
}

/// `GET api/playlist/{id}` of a held playlist: Navidrome's row with the streamed songs counted.
async fn native_read(
    state: &AppState,
    request: &NativeRequest<'_>,
    id: &str,
) -> Result<Option<Response>, AppError> {
    if !held::held_now(state, id) {
        return Ok(None);
    }
    let raw = request
        .proxy
        .relay_raw(request.endpoint, request.parameters)
        .await?;
    if !(200..300).contains(&raw.status) {
        return Ok(Some(relayed_raw(state, raw)));
    }
    let parsed = std::str::from_utf8(&raw.body)
        .ok()
        .and_then(|text| Node::parse(text).ok());
    let Some(Node::Object(mut row)) = parsed else {
        return Ok(Some(relayed_raw(state, raw)));
    };
    let viewer = native_viewer(state, request);
    count_native_row(state, &viewer, &mut row);
    let mut headers = HeaderMap::new();
    for (name, value) in &raw.response_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    Ok(Some(written_json(
        Node::Object(row).to_json_string(false).into_bytes(),
        headers,
        "application/json",
    )))
}

/// `GET api/playlist/{id}/tracks` of a held playlist: the caller's own read first (the sign-in
/// and the library rows they may see), then every entry in place, paged with `X-Total-Count`
/// over the served list.
async fn native_tracks(
    state: &AppState,
    request: &NativeRequest<'_>,
    id: &str,
) -> Result<Option<Response>, AppError> {
    if !held::held_now(state, id) {
        return Ok(None);
    }
    let library = match caller_tracks(state, request.headers, id).await {
        Ok(rows) => rows,
        Err(Some(answer)) => return Ok(Some(answer)),
        Err(None) => return Ok(Some(native_held_error(HeldError::Busy))),
    };
    let playlist = match state.held_playlists.serve(id).await {
        Ok(Some(playlist)) => playlist,
        Ok(None) => return Ok(None),
        Err(failure) => return Ok(Some(native_held_error(failure))),
    };
    let viewer = native_viewer(state, request);
    let rows: Vec<Value> = held_rows(state, &playlist.entries, library, &viewer, id)
        .into_iter()
        .map(|served| served.row)
        .collect();
    let total = rows.len();
    let start = page_number(request.parameters, "_start").unwrap_or(0).max(0);
    let end = page_number(request.parameters, "_end").unwrap_or(total as i32);
    let page: Vec<Node> = page(&rows, start, end).iter().map(to_node).collect();
    Ok(Some(file_with_total(
        Node::Array(page).to_json_string(false).into_bytes(),
        total,
    )))
}

/// A managed member's own family action playlist reached natively: what is added, and what the
/// playlist already holds, become requests or removals, and the playlist is emptied, as the
/// Subsonic route does. None for any other playlist or caller.
async fn native_family_action(
    state: &AppState,
    request: &NativeRequest<'_>,
    id: &str,
    ids: &[String],
    viewer: &NativeViewer,
) -> Option<Response> {
    if !state.settings.current().family.enabled || !viewer.caller.managed {
        return None;
    }
    let playlist = native_playlist(state, request, id).await.ok()?;
    if !dotnet::eq_ignore_case(playlist.owner.trim(), viewer.user.trim()) {
        return None;
    }
    let action = family_action_of(&playlist.name)?;
    let already: Vec<String> = caller_tracks(state, request.headers, id)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|row| row.get("mediaFileId").and_then(Value::as_str).map(str::to_string))
        .collect();
    let songs: Vec<String> = already.iter().chain(ids.iter()).cloned().collect();
    run_family_action(state, &viewer.caller.user, playlist.name.trim(), &action, &songs).await;
    if !already.is_empty() {
        let places: Vec<(String, String)> = (1..=already.len())
            .map(|place| ("id".to_string(), place.to_string()))
            .collect();
        let emptied = caller_proxy(state, request.headers, Method::DELETE, None)
            .relay_raw(
                &format!("api/playlist/{id}/tracks"),
                places.iter().map(|(k, v)| (k, v)),
            )
            .await;
        if !emptied.is_ok_and(|raw| (200..300).contains(&raw.status)) {
            warn!("Could not empty {}'s {} playlist", viewer.user, playlist.name);
        }
    }
    Some(native_json(&json!({ "added": 0 })))
}

/// The native caller's request relayed as it came, run when the held service awaits it.
fn native_plain(request: &NativeRequest<'_>) -> held_playlists::Relay {
    let proxy = request.proxy.clone();
    let endpoint = request.endpoint.to_string();
    let parameters = request.parameters.clone();
    Box::pin(async move {
        let raw = proxy.relay_raw(&endpoint, &parameters).await.ok()?;
        Some(RelayAnswer {
            ok: (200..300).contains(&raw.status),
            body: raw.body.to_vec(),
            content_type: raw.content_type,
        })
    })
}

/// `POST api/playlist/{id}/tracks` (`ids`, `albumIds`, `artistIds`) in this order: a managed
/// member's family action playlist first; outside albums and artists refused; then, for songs
/// found online or any add to a held playlist, the caller's upkeep playlists and smart or synced
/// playlists refused, the owner checked, and the held service adds them (converting a plain
/// playlist) under the lock. Library songs added to a plain playlist are relayed as they came.
async fn native_add(
    state: &AppState,
    request: &NativeRequest<'_>,
    id: &str,
) -> Result<Option<Response>, AppError> {
    let body: Value = serde_json::from_slice(request.body).unwrap_or(Value::Null);
    let list = |name: &str| -> Vec<String> {
        body.get(name)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let (ids, albums, artists) = (list("ids"), list("albumIds"), list("artistIds"));
    let viewer = native_viewer(state, request);
    if let Some(answer) = native_family_action(state, request, id, &ids, &viewer).await {
        return Ok(Some(answer));
    }
    let outside = |item: &String| {
        state.local_library.parse_song_id(item).0 || state.local_library.parse_external_id(item).0
    };
    if albums.iter().chain(&artists).any(outside) {
        return Ok(Some(error_object(
            StatusCode::BAD_REQUEST,
            held_playlists::ONE_BY_ONE,
        )));
    }
    if !ids.iter().any(outside) && !held::held_now(state, id) {
        return Ok(None);
    }
    let playlist = match native_playlist(state, request, id).await {
        Ok(playlist) => playlist,
        Err(answer) => return Ok(Some(answer)),
    };
    let own = dotnet::eq_ignore_case(playlist.owner.trim(), viewer.user.trim());
    let built = if own {
        match state
            .held_playlists
            .build_entries(&playlist.owner, &ids, &state.held_playlists.record_ids(id))
            .await
        {
            Ok(built) => built,
            Err(failure) => return Ok(Some(native_held_error(failure))),
        }
    } else {
        Vec::new()
    };
    if held::brings_streamed(&built) {
        if held::refuses_outside(state, request.proxy, &viewer.user, &playlist.name) {
            return Ok(Some(native_held_error(HeldError::Upkeep)));
        }
        if playlist.smart || playlist.synced {
            return Ok(Some(native_held_error(HeldError::MadeByRules)));
        }
    }
    if !own {
        return Ok(Some(native_held_error(HeldError::NotOwner)));
    }
    let added = built
        .iter()
        .filter(|song| matches!(song, Built::Entry { .. } | Built::Takeover(_)))
        .count();
    let change = UpdateRequest {
        id: id.to_string(),
        caller: viewer.held_caller(),
        owner: playlist.owner.clone(),
        remove_indexes: Vec::new(),
        remove_songs: Vec::new(),
        adds: built,
        fields: None,
        plain: native_plain(request),
        view: Arc::new(NativeList {
            state: state.clone(),
            headers: request.headers.clone(),
            viewer: viewer.clone(),
            id: id.to_string(),
        }),
    };
    match state.held_playlists.update(change).await {
        Ok(UpdateOutcome::Held { skipped, .. }) => Ok(Some(native_json(
            &json!({ "added": added.saturating_sub(skipped.len()) }),
        ))),
        Ok(UpdateOutcome::Plain(Some(answer)) | UpdateOutcome::Relayed(answer)) => {
            let mut response = file_reply(
                &Bytes::from(answer.body),
                Some(answer.content_type.as_deref().unwrap_or("application/json")),
                "json",
            );
            if !answer.ok {
                *response.status_mut() = StatusCode::BAD_REQUEST;
            }
            Ok(Some(response))
        }
        Ok(UpdateOutcome::Plain(None)) => Ok(Some(native_held_error(HeldError::Busy))),
        Err(failure) => Ok(Some(native_held_error(failure))),
    }
}

/// The owner, the served list and who is asking, for a native change by place. The error is the
/// answer to send.
async fn native_change_start(
    state: &AppState,
    request: &NativeRequest<'_>,
    id: &str,
) -> Result<(NativeViewer, Arc<NativeList>), Response> {
    let viewer = native_viewer(state, request);
    let playlist = native_playlist(state, request, id).await?;
    if !dotnet::eq_ignore_case(playlist.owner.trim(), viewer.user.trim()) {
        return Err(native_held_error(HeldError::NotOwner));
    }
    let view = Arc::new(NativeList {
        state: state.clone(),
        headers: request.headers.clone(),
        viewer: viewer.clone(),
        id: id.to_string(),
    });
    Ok((viewer, view))
}

/// `DELETE api/playlist/{id}/tracks?id=<place>...` on a held playlist: the places count over the
/// caller's served list and come out of the entries under the lock. None for a plain playlist,
/// which is relayed.
async fn native_remove(
    state: &AppState,
    request: &NativeRequest<'_>,
    id: &str,
) -> Result<Option<Response>, AppError> {
    if !held::held_now(state, id) {
        return Ok(None);
    }
    let (viewer, view) = match native_change_start(state, request, id).await {
        Ok(start) => start,
        Err(answer) => return Ok(Some(answer)),
    };
    let places: Vec<usize> = request
        .parameters
        .get("id")
        .map(|value| {
            value
                .split(',')
                .filter_map(|place| place.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    match state
        .held_playlists
        .native_remove(id, viewer.held_caller(), places.clone(), view)
        .await
    {
        Ok(Some(_)) => {
            let ids: Vec<String> = places.iter().map(usize::to_string).collect();
            Ok(Some(native_json(&json!({ "ids": ids }))))
        }
        Ok(None) => Ok(None),
        Err(failure) => Ok(Some(native_held_error(failure))),
    }
}

/// `PUT api/playlist/{id}/tracks/{place}` (`insert_before`) on a held playlist: the song at that
/// place over the caller's served list moves to `insert_before` under the lock. None for a plain
/// playlist, which is relayed.
async fn native_move(
    state: &AppState,
    request: &NativeRequest<'_>,
    id: &str,
    place: &str,
) -> Result<Option<Response>, AppError> {
    if !held::held_now(state, id) {
        return Ok(None);
    }
    let body: Value = serde_json::from_slice(request.body).unwrap_or(Value::Null);
    let insert_before = match body.get("insert_before") {
        Some(Value::String(text)) => text.trim().parse::<usize>().ok(),
        Some(Value::Number(number)) => number.as_u64().and_then(|n| usize::try_from(n).ok()),
        _ => None,
    };
    let (Some(from), Some(insert_before)) = (place.trim().parse::<usize>().ok(), insert_before) else {
        return Ok(Some(native_held_error(HeldError::NotFound)));
    };
    let (viewer, view) = match native_change_start(state, request, id).await {
        Ok(start) => start,
        Err(answer) => return Ok(Some(answer)),
    };
    match state
        .held_playlists
        .native_move(id, viewer.held_caller(), from, insert_before, view)
        .await
    {
        Ok(Some(_)) => Ok(Some(native_json(&json!({ "id": place.trim() })))),
        Ok(None) => Ok(None),
        Err(failure) => Ok(Some(native_held_error(failure))),
    }
}

/// `DELETE api/playlist/{id}`: relayed under the lock by the held service, which drops the held
/// file on success; once the lock is released the caller's placement of the playlist goes too.
async fn native_delete_playlist(
    state: &AppState,
    request: &NativeRequest<'_>,
    id: &str,
) -> Result<Response, AppError> {
    let user = native_username(state, request.parameters, request.headers);
    let answered: Arc<Mutex<Option<RawRelayResult>>> = Arc::default();
    let relay = {
        let proxy = request.proxy.clone();
        let endpoint = request.endpoint.to_string();
        let parameters = request.parameters.clone();
        let answered = Arc::clone(&answered);
        Box::pin(async move {
            let raw = proxy.relay_raw(&endpoint, &parameters).await.ok()?;
            let answer = RelayAnswer {
                ok: (200..300).contains(&raw.status),
                body: raw.body.to_vec(),
                content_type: raw.content_type.clone(),
            };
            *answered.lock() = Some(raw);
            Some(answer)
        })
    };
    match state.held_playlists.delete(id, relay).await {
        Ok(answer) => {
            if answer.as_ref().is_some_and(|answer| answer.ok) && !user.trim().is_empty() {
                state
                    .playlist_folders
                    .remove_places(&user, &[id.to_string()], chrono::Utc::now());
            }
            let raw = answered.lock().take();
            match raw {
                Some(raw) => Ok(relayed_raw(state, raw)),
                None => Ok(native_held_error(HeldError::Busy)),
            }
        }
        Err(failure) => Ok(native_held_error(failure)),
    }
}
