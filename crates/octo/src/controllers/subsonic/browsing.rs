//! `getSong`, `getArtist` and `getAlbum`: an outside id is answered from the catalog, a library
//! artist or album is Navidrome's merged with what the catalog has that the library lacks.

use std::collections::HashSet;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use octo_core::common::dotnet::{self, is_blank};
use octo_core::common::playlist_id_helper;
use octo_core::common::{AlbumFillIn, LibraryTrack, SongIdentity};
use octo_core::fingerprint::heart_gate::AlbumContext;
use octo_core::models::domain::Album;
use serde_json::Value;
use tracing::{debug, error};

use super::album_ownership::{outside_album_with_owned_songs, scope_of, settle_artist_albums};
use super::caller_checks::{SubsonicCall, as_json, file, listener_name};
use crate::app::AppState;
use crate::http::error::{AppError, AppResult};
use crate::http::routes::RouteSet;
use crate::services::i_music_metadata_service::IMusicMetadataService;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("getSong", get(get_song).post(get_song))
        .subsonic("getArtist", get(get_artist).post(get_artist))
        .subsonic("getAlbum", get(get_album).post(get_album))
}

/// Records, on each outside song an album listing names, that `user` listed it from that album
/// on `client`: the catalog's own album id (never sent to a client), the song's place on it and
/// when. Nothing for an unnamed user or an album whose catalog id is unknown.
pub(crate) fn remember_listed_album(
    state: &AppState,
    user: &str,
    client: Option<&str>,
    album_id: &str,
    album: &Album,
) {
    let user = user.trim();
    if user.is_empty() {
        return;
    }
    let Some(catalog_album_id) = state
        .external_id_registry
        .lookup(album_id)
        .and_then(|routing| routing.lock().external_album_id.clone())
        .filter(|id| !is_blank(id))
    else {
        return;
    };
    let listed_at = chrono::Utc::now();
    let client = client.map(str::trim).filter(|c| !c.is_empty());
    let total = album.song_count.or(Some(album.songs.len() as i32));
    for song in &album.songs {
        let Some(id) = song.external_id.as_deref().filter(|id| !is_blank(id)) else {
            continue;
        };
        state.external_id_registry.remember_context(
            id,
            AlbumContext {
                user: user.to_string(),
                client: client.map(str::to_string),
                catalog_album_id: catalog_album_id.clone(),
                album: Some(album.title.clone()).filter(|title| !is_blank(title)),
                disc: song.disc_number,
                track: song.track,
                total: song.total_tracks.or(total),
                isrc: song.isrc.clone(),
                explicit: song.explicit_content_lyrics,
                listed_at,
                played_at: None,
            },
        );
    }
}

/// Returns external song info if needed.
pub async fn get_song(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = call.param("id").to_string();
    let format = call.format();
    let builder = &state.subsonic_response_builder;

    if is_blank(&id) {
        return Ok(builder
            .create_error(&format, 10, "Missing id parameter")
            .into_response());
    }

    let (is_external, provider, external_id) = state.local_library.parse_song_id(&id);

    if !is_external {
        // Exceptions are not caught here: they reach the global handler.
        let result = call.proxy.relay("rest/getSong", &call.parameters).await?;
        let content_type = result
            .content_type
            .unwrap_or_else(|| format!("application/{format}"));
        return Ok(file(result.body, &content_type));
    }

    let song = state
        .music_metadata
        .get_song(
            provider.as_deref().unwrap_or_default(),
            external_id.as_deref().unwrap_or_default(),
        )
        .await;

    let Some(song) = song else {
        return Ok(builder
            .create_error(&format, 70, "Song not found")
            .into_response());
    };
    // The routing's own length is the one that plays, so it stays; only a song without
    // one takes the length the registry remembered for display.
    let mut one = [song];
    if one[0].duration.is_none_or(|d| d <= 0) {
        state.music_metadata.complete_song_lengths(&mut one);
    }
    // Nothing known at all: look it up now, since this answer is all the client will show.
    if one[0].duration.is_none_or(|d| d <= 0) {
        state.music_metadata.resolve_song_length(&mut one[0]).await;
    }
    // A search answers before its rows' album years; this one song gets its year now.
    state.music_metadata.complete_song_year(&mut one[0]).await;
    let [song] = one;

    Ok(builder.create_song_response(&format, &song).into_response())
}

/// Merges local and Deezer albums.
pub async fn get_artist(State(state): State<AppState>, req: Request) -> AppResult {
    let scope = scope_of(crate::services::family::family_caller(req.extensions()));
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = call.param("id").to_string();
    let format = call.format();
    let builder = &state.subsonic_response_builder;
    let metadata = &state.metadata_service;

    if is_blank(&id) {
        return Ok(builder
            .create_error(&format, 10, "Missing id parameter")
            .into_response());
    }

    let (is_external, provider, external_id) = state.local_library.parse_song_id(&id);

    if is_external {
        let provider = provider.unwrap_or_default();
        let external_id = external_id.unwrap_or_default();
        let Some(artist) = metadata.get_artist(&provider, &external_id).await else {
            return Ok(builder
                .create_error(&format, 70, "Artist not found")
                .into_response());
        };

        let mut albums = metadata.get_artist_albums(&provider, &external_id).await;

        // Fill artist info for each album (Deezer API doesn't include it in artist/albums endpoint)
        for album in &mut albums {
            if album.artist.is_empty() {
                album.artist = artist.name.clone();
            }
            if album.artist_id.as_deref().unwrap_or("").is_empty() {
                album.artist_id = Some(artist.id.clone());
            }
        }

        // Each says how much of it the library holds. None is left out: the page lists no
        // library album to stand in for one.
        let albums = settle_artist_albums(&state, albums, &[], scope.clone()).await;

        return Ok(builder
            .create_artist_response(&format, &artist, &albums)
            .into_response());
    }

    // Merged from Navidrome's JSON whatever the client asked for, then answered in the
    // client's format: see `create_merged_response`.
    let Some(navidrome) = call
        .proxy
        .relay_safe("rest/getArtist", &as_json(&call.parameters))
        .await
    else {
        return Ok(builder
            .create_error(&format, 70, "Artist not found")
            .into_response());
    };

    let mut artist_name = String::new();
    let local_artist_id = id.clone(); // Keep the local artist ID for merged albums
    let mut local_albums: Vec<Value> = Vec::new();
    let mut artist_data: Option<Value> = None;

    if navidrome
        .content_type
        .as_deref()
        .is_some_and(|ct| ct.contains("json"))
    {
        let document = parse_json(&navidrome.body)?;
        if let Some(artist_element) = property(property(Some(&document), "subsonic-response")?, "artist")? {
            artist_name = string_property(artist_element, "name")?;
            artist_data = Some(convert_element(&state, artist_element)?);

            if let Some(albums) = property(Some(artist_element), "album")? {
                for album in enumerate_array(albums)? {
                    local_albums.push(convert_element(&state, album)?);
                }
            }
        }
    }

    let Some(mut artist_data) = artist_data.filter(|_| !artist_name.is_empty()) else {
        return Ok(relay_as_asked(
            &state,
            &call,
            "rest/getArtist",
            &format,
            &navidrome.body,
            navidrome.content_type.as_deref(),
        )
        .await);
    };

    let local_album_titles: Vec<String> = local_albums
        .iter()
        .filter_map(|album| album.as_object()?.get("name").and_then(to_text))
        .collect();

    // The first hit is not reliably this artist: a bigger act whose name contains this one
    // can come first. Of the few asked for, the one with this exact name is.
    let deezer_artists: Vec<_> = metadata
        .search_artists(&artist_name, 5)
        .await
        .into_iter()
        .filter(|found| SongIdentity::same_artist_name(&found.name, &artist_name))
        .collect();
    let mut deezer_albums: Vec<Album> = Vec::new();

    if let Some(deezer_artist) = deezer_artists.first()
        && SongIdentity::same_artist_name(&deezer_artist.name, &artist_name)
    {
        // The provider must come from the artist found, as for albums: a hardcoded
        // "deezer" never matches the metadata service's name, so this was always empty.
        // The library's own albums go along, to tell two artists of one name apart.
        deezer_albums = metadata
            .get_artist_albums_for_library(
                deezer_artist.external_provider.as_deref().unwrap_or_default(),
                deezer_artist.external_id.as_deref().unwrap_or_default(),
                Some(&local_album_titles),
            )
            .await;

        // Fill artist info for each album (Deezer API doesn't include it in artist/albums endpoint)
        // Use local artist ID and name so albums link back to the local artist
        for album in &mut deezer_albums {
            if album.artist.is_empty() {
                album.artist = artist_name.clone();
            }
            if album.artist_id.as_deref().unwrap_or("").is_empty() {
                album.artist_id = Some(local_artist_id.clone());
            }
        }
    }

    // An owned album is one the library has by the matcher's key, so "Discovery" in the
    // library hides the catalog's "Discovery" however either is spelled or punctuated. One
    // with an owned count was already weighed against the library and kept, so it stays.
    let local_album_names: HashSet<String> =
        local_album_titles.iter().map(|t| SongIdentity::key(t)).collect();

    // Each outside album says how much of it the library holds; one the page's own albums hold
    // whole, or by name, is left out.
    let deezer_albums = settle_artist_albums(&state, deezer_albums, &local_albums, scope.clone()).await;

    let mut merged_albums = local_albums;
    for deezer_album in &deezer_albums {
        if deezer_album.owned_count.is_some()
            || !local_album_names.contains(&SongIdentity::key(&deezer_album.title))
        {
            merged_albums.push(Value::Object(builder.convert_album_to_json(deezer_album)));
        }
    }

    if let Some(artist_dict) = artist_data.as_object_mut() {
        let count = merged_albums.len();
        artist_dict.insert("album".into(), Value::Array(merged_albums));
        artist_dict.insert("albumCount".into(), count.into());
    }

    Ok(builder
        .create_merged_response(&format, "artist", &artist_data)
        .into_response())
}

/// Enriches local albums with Deezer songs.
pub async fn get_album(State(state): State<AppState>, req: Request) -> AppResult {
    let scope = scope_of(crate::services::family::family_caller(req.extensions()));
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = call.param("id").to_string();
    let format = call.format();
    let builder = &state.subsonic_response_builder;
    let metadata = &state.music_metadata;

    if is_blank(&id) {
        return Ok(builder
            .create_error(&format, 10, "Missing id parameter")
            .into_response());
    }

    // Check if this is an external playlist
    if playlist_id_helper::is_external_playlist(Some(&id)) {
        let Ok((provider, external_id)) = playlist_id_helper::parse_playlist_id(&id) else {
            error!("Error getting playlist {id}");
            return Ok(builder
                .create_error(&format, 70, "Playlist not found")
                .into_response());
        };

        // Get playlist metadata
        let Some(playlist) = metadata.get_playlist(&provider, &external_id).await else {
            return Ok(builder
                .create_error(&format, 70, "Playlist not found")
                .into_response());
        };

        // Get playlist tracks
        let tracks = metadata.get_playlist_tracks(&provider, &external_id).await;

        // Convert to album response (playlist as album)
        return Ok(builder
            .create_playlist_as_album_response(&format, &playlist, &tracks)
            .into_response());
    }

    let (is_external, album_provider, album_external_id) = state.local_library.parse_song_id(&id);

    if is_external {
        let album = metadata
            .get_album(
                album_provider.as_deref().unwrap_or_default(),
                album_external_id.as_deref().unwrap_or_default(),
            )
            .await;

        let Some(mut album) = album else {
            return Ok(builder
                .create_error(&format, 70, "Album not found")
                .into_response());
        };
        let lister = listener_name(&state, &call).await;
        remember_listed_album(
            &state,
            &lister,
            call.param_opt("c"),
            album_external_id.as_deref().unwrap_or_default(),
            &album,
        );
        let barcode = album.songs.iter().find_map(|song| song.barcode.clone());
        let animated = super::animated_art::album_field(
            &state,
            &call,
            &format,
            &album.artist,
            &album.title,
            barcode.as_deref(),
        );

        // The songs the library already holds go out as the library's own copies.
        if let Some(owned) = outside_album_with_owned_songs(
            &state,
            &call.proxy,
            &call.parameters,
            &mut album,
            &format,
            scope.clone(),
        )
        .await
        {
            return Ok(match &animated {
                Some(field) => super::animated_art::response_with_album_field(owned, field).await,
                None => owned,
            });
        }

        let mut reply = builder.create_album_response(&format, &album);
        if let Some(field) = &animated {
            reply.body = super::animated_art::with_album_field(std::mem::take(&mut reply.body), field);
        }
        return Ok(reply.into_response());
    }

    // Merged from Navidrome's JSON whatever the client asked for, then answered in the
    // client's format: see `create_merged_response`.
    let Some(navidrome) = call
        .proxy
        .relay_safe("rest/getAlbum", &as_json(&call.parameters))
        .await
    else {
        return Ok(builder
            .create_error(&format, 70, "Album not found")
            .into_response());
    };

    let mut album_name = String::new();
    let mut artist_name = String::new();
    let mut local_songs: Vec<Value> = Vec::new();
    let mut album_data: Option<Value> = None;

    if navidrome
        .content_type
        .as_deref()
        .is_some_and(|ct| ct.contains("json"))
    {
        let document = parse_json(&navidrome.body)?;
        if let Some(album_element) = property(property(Some(&document), "subsonic-response")?, "album")? {
            album_name = string_property(album_element, "name")?;
            artist_name = string_property(album_element, "artist")?;
            album_data = Some(convert_element(&state, album_element)?);

            if let Some(songs) = property(Some(album_element), "song")? {
                for song in enumerate_array(songs)? {
                    local_songs.push(convert_element(&state, song)?);
                }
            }
        }
    }

    let Some(mut album_data) = album_data.filter(|_| !album_name.is_empty() && !artist_name.is_empty())
    else {
        return Ok(relay_as_asked(
            &state,
            &call,
            "rest/getAlbum",
            &format,
            &navidrome.body,
            navidrome.content_type.as_deref(),
        )
        .await);
    };

    let library: Vec<LibraryTrack> = local_songs
        .iter()
        .map(|song| AlbumFillIn::from_subsonic(Some(song)))
        .collect();

    // The first catalog album by this name that holds the library's songs, known by ISRC
    // or by title and length. A name alone can belong to another record: "Nightcore" by
    // "Nightcore" (octo-player#1).
    let search_query = format!("{artist_name} {album_name}");
    let deezer_albums = metadata.search_albums(&search_query, 5).await;
    let library_songs = i32::try_from(AlbumFillIn::count_songs(&library)).unwrap_or(i32::MAX);
    let mut deezer_album: Option<Album> = None;
    for candidate in AlbumFillIn::candidates(&deezer_albums, &artist_name, &album_name, library_songs) {
        // The provider must come from the candidate. A hardcoded "deezer" never
        // matches the metadata service's provider name, so this always returned null.
        let detail = metadata
            .get_album(
                candidate.external_provider.as_deref().unwrap_or_default(),
                candidate.external_id.as_deref().unwrap_or_default(),
            )
            .await;
        let Some(detail) = detail.filter(|d| !d.songs.is_empty()) else {
            continue;
        };
        if AlbumFillIn::holds(&library, &detail.songs) {
            deezer_album = Some(detail);
            break;
        }
        debug!(
            "getAlbum '{artist_name} - {album_name}': catalog album {} shares the name but not the songs; not filled in from it",
            candidate.external_id.as_deref().unwrap_or("")
        );
    }

    if let Some(deezer_album) = deezer_album {
        // Every song on the page is filed under the album it was opened as, so an app that
        // groups songs by their album lists the outside ones with the rest.
        let album_id = album_data
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let mut merged_songs = local_songs;
        for deezer_song in &deezer_album.songs {
            if !AlbumFillIn::owned(&library, deezer_song) {
                let mut row = builder.convert_song_to_json(deezer_song);
                if let Some(album_id) = &album_id {
                    row.insert("albumId".into(), album_id.clone().into());
                    row.insert("parent".into(), album_id.clone().into());
                }
                merged_songs.push(Value::Object(row));
            }
        }

        // OrderBy: a stable sort on each song's track, 0 for none.
        let mut keyed = Vec::with_capacity(merged_songs.len());
        for song in merged_songs {
            let track = match song.as_object().and_then(|dict| dict.get("track")) {
                Some(track) => convert_to_int32(track)?,
                None => 0,
            };
            keyed.push((track, song));
        }
        keyed.sort_by_key(|(track, _)| *track);
        let merged_songs: Vec<Value> = keyed.into_iter().map(|(_, song)| song).collect();

        if let Some(album_dict) = album_data.as_object_mut() {
            let mut total_duration: i32 = 0;
            for song in &merged_songs {
                if let Some(duration) = song.as_object().and_then(|dict| dict.get("duration")) {
                    total_duration = total_duration.wrapping_add(convert_to_int32(duration)?);
                }
            }
            let count = merged_songs.len();
            album_dict.insert("song".into(), Value::Array(merged_songs));
            album_dict.insert("songCount".into(), count.into());
            album_dict.insert("duration".into(), total_duration.into());
        }
    }

    if let Some(field) =
        super::animated_art::album_field(&state, &call, &format, &artist_name, &album_name, None)
        && let Some(album_dict) = album_data.as_object_mut()
    {
        album_dict.insert("animatedArt".into(), field);
    }

    Ok(builder
        .create_merged_response(&format, "album", &album_data)
        .into_response())
}

/// Navidrome's answer untouched, when there is nothing to merge: the JSON already in hand
/// for a JSON client, otherwise asked again in the client's own format so an XML client
/// never gets JSON.
async fn relay_as_asked(
    state: &AppState,
    call: &SubsonicCall,
    endpoint: &str,
    format: &str,
    json_body: &[u8],
    json_content_type: Option<&str>,
) -> Response {
    if format == "json" {
        return file(
            json_body.to_vec(),
            json_content_type.unwrap_or("application/json"),
        );
    }
    match call.proxy.relay_safe(endpoint, &call.parameters).await {
        Some(asked) => file(
            asked.body,
            asked.content_type.as_deref().unwrap_or("application/xml"),
        ),
        None => state
            .subsonic_response_builder
            .create_error(format, 70, "Not found")
            .into_response(),
    }
}

/// The body as JSON: malformed JSON is an internal error.
fn parse_json(body: &[u8]) -> Result<Value, AppError> {
    serde_json::from_slice(body)
        .map_err(|e| AppError::Internal(anyhow::Error::new(e).context("Navidrome's answer is not JSON")))
}

/// A member of an element that may be absent: an error on anything but an object.
fn property<'a>(element: Option<&'a Value>, name: &str) -> Result<Option<&'a Value>, AppError> {
    match element {
        None => Ok(None),
        Some(Value::Object(fields)) => Ok(fields.get(name)),
        Some(_) => Err(AppError::InvalidOperation(format!(
            "The requested operation requires an element of type 'Object' (looking for '{name}')."
        ))),
    }
}

/// A string member, "" when it is missing or null: an error on anything but a string or a null.
fn string_property(element: &Value, name: &str) -> Result<String, AppError> {
    match property(Some(element), name)? {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(AppError::InvalidOperation(format!(
            "The requested operation requires an element of type 'String' ('{name}')."
        ))),
    }
}

/// The items of an array: an error on anything but an array.
fn enumerate_array(element: &Value) -> Result<&Vec<Value>, AppError> {
    element.as_array().ok_or_else(|| {
        AppError::InvalidOperation("The requested operation requires an element of type 'Array'.".into())
    })
}

/// The converted element: an error on anything but an object.
fn convert_element(state: &AppState, element: &Value) -> Result<Value, AppError> {
    state
        .subsonic_response_builder
        .convert_subsonic_json_element(element, true)
        .ok_or_else(|| {
            AppError::InvalidOperation("The requested operation requires an element of type 'Object'.".into())
        })
}

/// The text form of a converted value: a string as it is, a number in its shortest form, a
/// bool as `True`/`False`, a null as nothing.
fn to_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(true) => Some("True".into()),
        Value::Bool(false) => Some("False".into()),
        Value::Number(n) => Some(n.to_string()),
        // A list or a dictionary printed its type name.
        Value::Array(_) => Some("System.Collections.Generic.List`1[System.Object]".into()),
        Value::Object(_) => {
            Some("System.Collections.Generic.Dictionary`2[System.String,System.Object]".into())
        }
    }
}

/// A converted JSON value as an `i32`: null is 0, a double is rounded to even, a string is
/// parsed, and anything that cannot be converted is an error.
fn convert_to_int32(value: &Value) -> Result<i32, AppError> {
    match value {
        Value::Null => Ok(0),
        Value::Bool(b) => Ok(i32::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return i32::try_from(i).map_err(|_| {
                    AppError::Internal(anyhow::anyhow!(
                        "Value was either too large or too small for an Int32."
                    ))
                });
            }
            let x = n.as_f64().unwrap_or(0.0);
            let rounded = dotnet::round(x, 0);
            if rounded > f64::from(i32::MAX) || rounded < f64::from(i32::MIN) || rounded.is_nan() {
                return Err(AppError::Internal(anyhow::anyhow!(
                    "Value was either too large or too small for an Int32."
                )));
            }
            Ok(rounded as i32)
        }
        Value::String(text) => text
            .trim()
            .parse::<i32>()
            .map_err(|_| AppError::Format(format!("The input string '{text}' was not in a correct format."))),
        _ => Err(AppError::Internal(anyhow::anyhow!(
            "Unable to cast object to type 'System.IConvertible'."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_to_int32_reads_what_convert_did() {
        assert_eq!(convert_to_int32(&Value::Null).ok(), Some(0));
        assert_eq!(convert_to_int32(&serde_json::json!(3)).ok(), Some(3));
        assert_eq!(convert_to_int32(&serde_json::json!(2.5)).ok(), Some(2));
        assert_eq!(convert_to_int32(&serde_json::json!(3.5)).ok(), Some(4));
        assert_eq!(convert_to_int32(&serde_json::json!(" 7 ")).ok(), Some(7));
        assert!(convert_to_int32(&serde_json::json!("x")).is_err());
        assert!(convert_to_int32(&serde_json::json!([1])).is_err());
    }
}
