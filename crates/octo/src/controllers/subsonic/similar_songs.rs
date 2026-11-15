//! `getSimilarSongs` and `getSimilarSongs2`: song radio from every radio source
//! (`services::radio::SongRadioService`), the library's own copies preferred. A song, an album
//! or an artist id starts it, as the Subsonic API says.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use octo_core::common::dotnet;
use octo_core::models::domain::Song;
use octo_core::radio::RadioSeed;
use octo_core::soulseek::soulseek_metadata_service::RoutingKind;
use octo_subsonic::SubsonicReply;
use octo_subsonic::subsonic_response_builder::{SUBSONIC_NAMESPACE, SUBSONIC_VERSION};
use octo_subsonic::xml::XElement;
use serde_json::{Map, Value};
use tracing::{debug, info, warn};

use super::action_helpers::{SubsonicCall, int_param, is_successful_subsonic_response};
use crate::app::AppState;
use crate::services::last_fm::LastFmRadioTrackResolver;

/// The most songs Octo's own apps (`octoMix=client`) may ask one radio for. The sources are
/// asked for at most 100 too (`SongRadioService::build`).
const MOST_FOR_OCTO_APPS: i32 = 100;

pub async fn get_similar_songs(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    let builder = &state.subsonic_response_builder;
    let id = call.param_or("id", "").to_string();
    let settings = state.settings.current();
    // Octo's own apps mix the radio themselves and ask for the order of fit (octoMix=client);
    // for every other app this order is the radio, so outside songs are placed at random at the
    // radio's own mix (last_fm_radio_placement).
    let client_order = dotnet::eq_ignore_case(call.param_or("octoMix", ""), "client");
    // Octo's apps may ask for up to 100 songs, whatever RadioTrackCount is: at Lots and Mostly
    // new they need more outside songs than a 50-song answer holds. Every other app gets the
    // dashboard's count at most, as before.
    let most = if client_order {
        MOST_FOR_OCTO_APPS
    } else {
        settings.last_fm.effective_radio_track_count().max(1)
    };
    let count = int_param(&call.parameters, "count", 50).clamp(1, most);

    // Subsonic spec: getSimilarSongs.view → key "similarSongs"; getSimilarSongs2.view →
    // "similarSongs2". Clients (Arpeggi) parse the v2 key strictly and ignore v1-shaped
    // responses when they called v2: that's why the radio queue showed up empty.
    let endpoint = call.endpoint();
    let is_v2_request = dotnet::to_lower_invariant(&endpoint).contains("getsimilarsongs2");
    let response_key = if is_v2_request {
        "similarSongs2"
    } else {
        "similarSongs"
    };

    if dotnet::is_blank(&id) {
        return builder
            .create_error(format, 10, "Missing id parameter")
            .into_response();
    }

    // Check if radio is configured and enabled
    if !state.song_radio.enabled() {
        // Navidrome has never seen an outside id, so it would only answer "not found".
        if state.local_library.parse_song_id(&id).0 {
            return builder.create_response(format, response_key).into_response();
        }
        debug!("Last.fm radio not configured, relaying to upstream server");
        return match call.proxy.relay(&endpoint, &call.parameters).await {
            Ok(result) => call.file(&result.body, result.content_type.as_deref()),
            Err(_) => builder.create_response(format, response_key).into_response(),
        };
    }

    // Get the seed song metadata
    let mut artist_name = String::new();
    let mut track_title = String::new();
    let mut library_seed: Option<Song> = None;
    let mut seed_video: Option<String> = None;

    let resolver = LastFmRadioTrackResolver::new(
        call.proxy.clone(),
        state.metadata_service.clone(),
        state.external_id_registry.clone(),
    );
    let (is_external, provider, external_id) = state.local_library.parse_song_id(&id);
    // An outside artist (an artist row of a search or a chart) is a radio of the artists like
    // them, as a library artist is: the seed is the artist alone.
    let outside_artist = is_external
        .then(|| state.external_id_registry.lookup(&id))
        .flatten()
        .map(|routing| routing.snapshot())
        .filter(|routing| routing.kind == RoutingKind::Artist)
        .and_then(|routing| routing.artist)
        .filter(|artist| !dotnet::is_blank(artist));
    if let Some(artist) = &outside_artist {
        artist_name = artist.clone();
    } else if is_external {
        // External song - get metadata from our service
        if let Some(song) = state
            .metadata_service
            .get_song(
                provider.as_deref().unwrap_or(""),
                external_id.as_deref().unwrap_or(""),
            )
            .await
        {
            artist_name = song.artist;
            track_title = song.title;
        }
        // An outside song already playing from YouTube names its video: its radio needs no
        // search.
        seed_video = outside_seed_video(&state.external_id_registry, &id);
    } else {
        // A library song, or an album or artist, which Subsonic's getSimilarSongs also takes.
        // Read with the caller's own credentials, so it is their library being asked.
        let Some(seed) = resolver.read_seed(&id, &call.parameters).await else {
            warn!("Could not read radio seed {id} from Navidrome");
            return builder.create_response(format, response_key).into_response();
        };
        track_title = seed
            .song
            .as_ref()
            .map(|song| song.title.clone())
            .unwrap_or_default();
        library_seed = seed.song;
        artist_name = seed.artist;
    }

    if artist_name.is_empty() || (track_title.is_empty() && is_external && outside_artist.is_none()) {
        warn!("Could not get artist/title for song {id}");
        return builder.create_response(format, response_key).into_response();
    }

    // Who the radio is for, once Navidrome has accepted their sign-in: a library seed was just
    // read with it, an outside one is checked with a ping, as setRating does. Unchecked, the
    // radio applies no one-star bans, uses the base weights and teaches nothing, so nobody can
    // read or steer another listener's radio by naming them. Named as scrobbles name them.
    let mut verified = !is_external;
    if is_external {
        let mut auth = call.parameters.clone();
        auth.shift_remove("id");
        verified = call
            .proxy
            .relay_safe("rest/ping", &auth)
            .await
            .is_some_and(|check| is_successful_subsonic_response(&check.body, format));
    }
    let listener = if verified {
        state
            .request_identity
            .username(&call.parameters, &call.proxy)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let bans = listener
        .as_deref()
        .map(|listener| state.last_fm_radio_state.radio_ban_keys(listener))
        .unwrap_or_default();
    let seed = RadioSeed {
        artist: artist_name.clone(),
        title: track_title.clone(),
        duration: library_seed.as_ref().and_then(|song| song.duration),
        library_song: library_seed,
        you_tube_id: seed_video,
    };
    let mut resolved_songs = state
        .song_radio
        .build(
            &seed,
            count,
            listener.as_deref(),
            &call.parameters,
            &bans,
            &resolver,
            client_order,
        )
        .await;
    if resolved_songs.is_empty() {
        // Nothing from Octo's sources (no Last.fm key, say, and an artist none of the others
        // knows): Navidrome's own agents may still answer, as they did before Octo's radio.
        if !is_external
            && let Some(upstream) = call.proxy.relay_safe(&endpoint, &call.parameters).await
            && !upstream.body.is_empty()
            && is_successful_subsonic_response(&upstream.body, format)
        {
            return call.file(&upstream.body, upstream.content_type.as_deref());
        }
        return builder.create_response(format, response_key).into_response();
    }

    // Outside picks take the lengths already known for them; the rest are looked up in the
    // background for the next answer.
    state.metadata_service.complete_song_lengths(&mut resolved_songs);

    let local_count = resolved_songs.iter().filter(|song| song.is_local).count();
    info!(
        "Radio for '{artist_name} - {track_title}' -> {} songs ({local_count} local, {} external)",
        resolved_songs.len(),
        resolved_songs.len() - local_count
    );

    // Track this radio queue so scrobble events can drive the sliding-window
    // prewarm of upcoming externals.
    state
        .radio_queues
        .register(resolved_songs.iter().map(|song| song.id.clone()));
    // Song radio's songs are radio songs for the ones fetched ahead.
    state.prefetch.note_radio(
        &resolved_songs
            .iter()
            .map(|song| song.id.clone())
            .collect::<Vec<_>>(),
    );

    // Fire-and-forget prewarm for the top of the queue so the first few taps don't pay the
    // full cold yt-dlp resolve. Local songs are skipped by the prewarmer (they have no
    // registry entry).
    let metadata = Arc::clone(&state.metadata_service);
    let prewarm = resolved_songs.clone();
    tokio::spawn(async move { metadata.prewarm_you_tube_ids(&prewarm, 8).await });

    build_similar_songs_response(&state, format, &resolved_songs, response_key)
}

/// The video song radio seeds from for the outside song under `id`: the song's video for new
/// work (a person's chosen version included). None when none is known yet.
fn outside_seed_video(registry: &crate::services::soulseek::ExternalIdRegistry, id: &str) -> Option<String> {
    registry
        .lookup(id)
        .and_then(|routing| registry.song_video(&routing.snapshot()))
}

fn build_similar_songs_response(
    state: &AppState,
    format: &str,
    songs: &[Song],
    response_key: &str,
) -> Response {
    let builder = &state.subsonic_response_builder;
    if format == "json" {
        let json_songs: Vec<Value> = songs
            .iter()
            .map(|song| Value::Object(builder.convert_song_to_json(song)))
            .collect();
        let mut similar = Map::new();
        similar.insert("song".into(), Value::Array(json_songs));
        let mut body = Map::new();
        body.insert("status".into(), "ok".into());
        body.insert("version".into(), SUBSONIC_VERSION.into());
        body.insert(response_key.into(), Value::Object(similar));
        return builder.create_json_response(Value::Object(body)).into_response();
    }
    let mut similar = XElement::ns(SUBSONIC_NAMESPACE, response_key);
    for song in songs {
        similar.push(builder.convert_song_to_xml(song, Some(SUBSONIC_NAMESPACE)));
    }
    let document = XElement::ns(SUBSONIC_NAMESPACE, "subsonic-response")
        .attr("status", "ok")
        .attr("version", SUBSONIC_VERSION)
        .child(similar);
    SubsonicReply::xml(&document).into_response()
}

#[cfg(test)]
mod seed_tests {
    use super::*;
    use octo_core::soulseek::SoulseekRouting;

    #[test]
    fn song_radio_seeds_from_the_chosen_version() {
        let registry = crate::services::soulseek::ExternalIdRegistry::in_memory();
        let id = registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Drake".into()),
            title: Some("Started".into()),
            ..Default::default()
        });
        assert_eq!(outside_seed_video(&registry, &id), None);
        registry.set_you_tube_id(&id, "old");
        assert_eq!(outside_seed_video(&registry, &id).as_deref(), Some("old"));
        registry
            .choose_stream_video(
                &id,
                crate::services::soulseek::StreamChoice {
                    video: "new".into(),
                    by: "ede".into(),
                    at: chrono::Utc::now(),
                    ..Default::default()
                },
            )
            .expect("stored");
        assert_eq!(outside_seed_video(&registry, &id).as_deref(), Some("new"));
    }
}
