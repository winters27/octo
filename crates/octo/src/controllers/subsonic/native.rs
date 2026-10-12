//! The catch-all's own answers: native Radio playlists, the external-id safety net, and the
//! Navidrome-native answers for Octo's outside songs, albums and artists, with discovery appended
//! to native searches.
//!
//! `http::catch_all` answers Octo-owned paths before reading the request, and relays the
//! request faithfully when none of these answers.

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use octo_core::common::dotnet;
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
use super::playlists::to_node;
use crate::app::AppState;
use crate::http::error::AppError;
use crate::services::soulseek::SoulseekMetadataService;
use crate::services::subsonic::{RawRelayResult, SubsonicProxyService};

/// The request as the catch-all read it.
pub struct NativeRequest<'a> {
    pub endpoint: &'a str,
    pub method: &'a Method,
    pub headers: &'a HeaderMap,
    pub parameters: &'a Parameters,
    pub format: &'a str,
    pub proxy: &'a SubsonicProxyService,
}

/// The catch-all's own answers, tried in order. `None` when the request goes on to the faithful
/// relay.
pub async fn answer(state: &AppState, request: &NativeRequest<'_>) -> Result<Option<Response>, AppError> {
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
    if let Some(album) = try_serve_native_external_album(state, request.endpoint).await {
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

    let external_albums = state.metadata_service.search_albums(&term, target as i32).await;
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

/// Native single-album detail: GET /api/album/{id} for one of Octo's album ids.
async fn try_serve_native_external_album(state: &AppState, endpoint: &str) -> Option<Response> {
    let id = leaf_id(endpoint, "api/album/")?;
    if routing_kind(state, &id) != Some(RoutingKind::Album) {
        return None;
    }
    let album = state
        .metadata_service
        .get_album(SoulseekMetadataService::PROVIDER_NAME, &id)
        .await?;
    let body = octo_core::json::to_string(&native_album_object(&album, 1));
    Some(written_json(
        body.into_bytes(),
        HeaderMap::new(),
        "application/json",
    ))
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
