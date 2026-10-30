//! `star`, `unstar`, `getStarred`, `getStarred2`, `setRating`, `scrobble` and `reportPlayback`.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use octo_core::common::dotnet::is_blank;
use octo_core::common::playlist_id_helper;
use octo_core::last_fm::last_fm_radio_refresh_policy;
use octo_core::last_fm::last_fm_scrobble_service::LastFmTrack;
use octo_core::models::domain::Song;
use octo_core::models::radio::LastFmRadioPlay;
use octo_core::settings::{HeartDownloadStep, LibraryAction, LibraryRatingScope};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_subsonic::SubsonicCredential;
use octo_subsonic::xml::XElement;
use serde_json::Value;
use tracing::{debug, info};

use super::caller_checks::{
    MAX_REQUEST_BODY, SubsonicCall, favorite_credential, file, is_successful_subsonic_response, is_true,
    listener_name, refuse_unless_signed_in, requester_for, signed_in_user,
};
use crate::app::AppState;
use crate::http::error::{AppError, AppResult};
use crate::http::routes::RouteSet;
use crate::services::common::heart_acquisition_coordinator::track_base_key;
use crate::services::common::star_on_arrival::WaitingStar;
use crate::services::common::{AcquisitionTarget, StarOnArrival};
use crate::services::family::list_rules::{Edit, edit};
use crate::services::family::{FamilyCaller, HeartOutcome, SavedAlbum, SavedSong, family_caller};
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::LastFmRadioTrackResolver;
use crate::services::last_fm::last_fm_radio_track_resolver::ScrobbleDetails;
use crate::services::library::{IgnoredRating, RatingActionRequest, RatingIgnored};
use crate::services::listening::{self, AlbumLookup, Play as ListeningPlay};
use crate::services::soulseek::SoulseekMetadataService;
use crate::services::subsonic::RelayError;
use octo_core::family::abilities::RequestQuality;
use octo_core::family::request::RequestKind;

/// How many outside songs after a scrobbled one have their stream readied to play at once.
const NEXT_STREAMS_WARMED: usize = 3;

/// The rating that counts as a heart for the listening log.
const TOP_RATING: i32 = 5;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("star", get(star).post(star))
        .subsonic("unstar", get(unstar).post(unstar))
        .subsonic("getStarred", get(get_starred).post(get_starred))
        .subsonic("getStarred2", get(get_starred).post(get_starred))
        .subsonic("setRating", get(set_rating).post(set_rating))
        .subsonic("scrobble", get(scrobble).post(scrobble))
        .subsonic("reportPlayback", get(report_playback).post(report_playback))
}

/// The radio's resolver over this request's proxy, so it is built per request.
fn resolver(state: &AppState, call: &SubsonicCall) -> LastFmRadioTrackResolver {
    LastFmRadioTrackResolver::new(
        call.proxy.clone(),
        state.music_metadata.clone(),
        state.external_id_registry.clone(),
    )
}

/// What a favorite of an outside artist answers: Octo can only favorite what is in the library.
pub const ARTIST_NOT_IN_LIBRARY: &str = "That artist isn't in your library, so it can't be a favorite yet.";

/// Every value of `key` the request sent, repeated keys and comma lists alike, trimmed, empty
/// ones left out.
fn sent_ids(call: &SubsonicCall, key: &str) -> Vec<String> {
    let mut values = call.incoming.parameter_values(key);
    if values.is_empty()
        && let Some(single) = call.param_opt(key)
    {
        values = vec![single.to_string()];
    }
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// The routing kind the registry keeps for `id`, when it is one of Octo's short ids.
fn routing_kind(state: &AppState, id: &str) -> Option<RoutingKind> {
    state
        .external_id_registry
        .lookup(id)
        .map(|routing| routing.snapshot().kind)
}

/// Whether `id` is an artist outside the library: a registry artist, or an `ext-` artist id.
fn is_outside_artist(state: &AppState, id: &str) -> bool {
    match routing_kind(state, id) {
        Some(kind) => kind == RoutingKind::Artist,
        None => matches!(
            state.local_library.parse_external_id(id),
            (true, _, Some(kind), _) if kind == "artist"
        ),
    }
}

/// Stars (favorites) items. For playlists, triggers download. For external songs and
/// albums, triggers a download and, outside Octo's own apps, favorites it once it arrives.
///
/// Repeated ids and comma lists are taken one by one: outside songs and albums are queued each
/// on their own, and the library ids go to Navidrome together in one call. An outside artist
/// cannot be a favorite until it is in the library, so it is refused with error 70 and nothing
/// is queued.
pub async fn star(State(state): State<AppState>, req: Request) -> AppResult {
    // Who the family gate found behind this request, read before the request is taken apart.
    let caller = family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = call.format();
    let builder = &state.subsonic_response_builder;

    let ids = sent_ids(&call, "id");
    state.imports.forget_cached_songs(&ids);
    let hearted_ids = ids.clone();
    let album_ids = sent_ids(&call, "albumId");
    let artist_ids = sent_ids(&call, "artistId");
    // What a heart on an outside song or album does for this person: a download into the
    // shared library (everyone outside the family, as always), into a member's own library, or
    // a saved link.
    let heart = state.family_write_gate.heart(caller.as_ref());

    // Check if this is a playlist
    if ids
        .iter()
        .any(|id| playlist_id_helper::is_external_playlist(Some(id)))
    {
        // Starring an outside playlist is not supported.
        return Ok(builder
            .create_error(&format, 0, "Playlist functionality is not enabled")
            .into_response());
    }

    let sources = state
        .settings
        .current()
        .subsonic
        .effective_heart_download_sources();
    // A saved link needs no download source.
    let saves = matches!(heart, HeartOutcome::Save { .. });
    let song_hearts = saves || sources.iter().any(|step| step.song_enabled == Some(true));

    // The REGISTRY decides whether an id is an album, because ParseSongId reports every
    // registry id as a "song" and would otherwise send an entire album down the single-track
    // download path. Subsonic sends an album under albumId, though some clients reuse id, so
    // either is read.
    let mut outside_artist = false;
    let mut albums: Vec<String> = Vec::new();
    let mut songs: Vec<String> = Vec::new();
    let mut library: Vec<(&str, String)> = Vec::new();
    for id in ids {
        if is_outside_artist(&state, &id) {
            outside_artist = true;
        } else if routing_kind(&state, &id) == Some(RoutingKind::Album) {
            albums.push(id);
        } else if song_hearts && state.local_library.parse_song_id(&id).0 {
            songs.push(id);
        } else {
            library.push(("id", id));
        }
    }
    for id in album_ids {
        if routing_kind(&state, &id) == Some(RoutingKind::Album) {
            albums.push(id);
        } else {
            library.push(("albumId", id));
        }
    }
    for id in artist_ids {
        if is_outside_artist(&state, &id) {
            outside_artist = true;
        } else {
            library.push(("artistId", id));
        }
    }

    if outside_artist || !albums.is_empty() || !songs.is_empty() {
        // Navidrome never sees these ids, so nothing else would check who is asking.
        if let Some(refused) = refuse_unless_signed_in(&state, &call, &format).await? {
            return Ok(refused);
        }
    }
    if outside_artist {
        info!("A favorite of an outside artist was refused: it is not in the library");
        return Ok(builder
            .create_error(&format, 70, ARTIST_NOT_IN_LIBRARY)
            .into_response());
    }
    let split = !albums.is_empty() || !songs.is_empty();
    // Who the outside hearts are for, named once before anything is queued, so a name that
    // cannot be read leaves nothing half queued under the error.
    let who = if split && !saves {
        Some(signed_in_user(&state, &call).await?)
    } else {
        None
    };
    // An outside song whose library copy Octo knows (its download landed, or a heart already
    // made the copy a favorite) is a heart on a song you have: Navidrome favorites the copy now
    // and nothing is fetched again.
    let holder = match (&heart, who.as_deref(), favorite_credential(&call)) {
        (HeartOutcome::Download(AcquisitionTarget::Shared), Some(who), Some(credential)) => {
            Some(StarOnArrival::holder_name(&credential, Some(who)))
        }
        _ => None,
    };
    let mut owned: Vec<(String, String)> = Vec::new();
    if let Some(holder) = &holder {
        songs.retain(|id| match library_copy_of(&state, id, holder) {
            Some(copy) => {
                owned.push((id.clone(), copy));
                false
            }
            None => true,
        });
        library.extend(owned.iter().map(|(_, copy)| ("id", copy.clone())));
    }
    let outside = OutsideHearts {
        heart: &heart,
        sources: &sources,
        who: who.as_deref(),
        albums: &albums,
        songs: &songs,
    };
    if split && library.is_empty() {
        // Navidrome has never seen these ids, so relaying the star would just error.
        outside.queue(&state, &call).await;
        note_listening_hearts(&state, &call, &hearted_ids).await;
        return Ok(builder.create_response(&format, "starred").into_response());
    }

    // Library items go to Navidrome: the request as sent when nothing was taken out of it,
    // otherwise everything else it carried with only the library ids.
    let relayed = if split {
        let mut pairs: Vec<(String, String)> = call
            .parameters
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "id" | "albumId" | "artistId"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        pairs.extend(library.iter().map(|(key, id)| (key.to_string(), id.clone())));
        call.proxy
            .relay("rest/star", pairs.iter().map(|(k, v)| (k, v)))
            .await
    } else {
        call.proxy.relay("rest/star", &call.parameters).await
    };
    match relayed {
        Ok(result) => {
            let accepted = is_successful_subsonic_response(&result.body, &format);
            // The outside ids are queued only once Navidrome took the library ones, so a star
            // it refused leaves nothing queued or held.
            if accepted {
                outside.queue(&state, &call).await;
                note_listening_hearts(&state, &call, &hearted_ids).await;
                if let Some(holder) = &holder {
                    for (song, copy) in &owned {
                        info!("{song} is in the library as {copy}; favorited that copy for {holder}");
                        state.star_on_arrival.remember_landed(song, copy, holder);
                    }
                }
            }
            let username = if accepted {
                listener(&state, &call).await
            } else {
                None
            };
            if let Some(username) = username.as_deref() {
                state.generated_playlists.forget_marks(username);
                let resolver = resolver(&state, &call);
                for (_, song_id) in library.iter().filter(|(key, _)| *key == "id") {
                    if let Some(song) = resolver.resolve_scrobble(song_id, &call.parameters).await {
                        state
                            .last_fm_radio_state
                            .mark_heart(username, song_id, &song.artist, &song.title);
                    }
                }
            }
            let content_type = result
                .content_type
                .unwrap_or_else(|| format!("application/{format}"));
            Ok(file(result.body, &content_type))
        }
        Err(RelayError::Http(message)) => Ok(builder
            .create_error(
                &format,
                0,
                &format!("Error connecting to Subsonic server: {message}"),
            )
            .into_response()),
        Err(other) => Err(other.into()),
    }
}

/// The library copy of the outside song `id` for the person filed as `holder`: the copy a heart
/// of theirs made the favorite, else the one its download placed while the downloads log keeps
/// that row.
fn library_copy_of(state: &AppState, id: &str, holder: &str) -> Option<String> {
    if let Some(copy) = state.star_on_arrival.landed_copy(id, holder) {
        return Some(copy);
    }
    let (_, provider, external_id) = state.local_library.parse_song_id(id);
    state
        .acquisition_tracker
        .landed_library_id(&provider?, &external_id?)
}

/// The name the caller's hearts are filed under, when some outside song's library copy is a
/// favorite through a heart; None otherwise, or when the caller cannot be named.
async fn landed_holder(state: &AppState, call: &SubsonicCall) -> Option<String> {
    if !state.star_on_arrival.has_landed() {
        return None;
    }
    let credential = SubsonicCredential::from(&call.parameters)?;
    let who = signed_in_user(state, call).await.ok()?;
    Some(StarOnArrival::holder_name(&credential, Some(&who)))
}

/// The outside songs and albums one star carried, with what a heart does for this person and
/// who it is for (None only when every heart is a saved link).
struct OutsideHearts<'a> {
    heart: &'a HeartOutcome,
    sources: &'a [HeartDownloadStep],
    who: Option<&'a str>,
    albums: &'a [String],
    songs: &'a [String],
}

impl OutsideHearts<'_> {
    /// Saves or queues each outside album and song. With DownloadAlbumOnSongStar on, the hearted
    /// songs of one album go down their chains together and the album follows them once, so a
    /// request hearting several songs of an album walks it once. An album the request hearted
    /// itself is already on its way, so no song brings it again.
    async fn queue(&self, state: &AppState, call: &SubsonicCall) {
        for album in self.albums {
            star_outside_album(state, call, self.heart, self.sources, self.who, album).await;
        }
        let mut ready = Vec::new();
        for song in self.songs {
            if let Some(heart) = star_outside_song(state, call, self.heart, self.who, song).await {
                ready.push(heart);
            }
        }
        let Some(who) = self.who else {
            return;
        };
        let requested_by = requester_for(state, Some(who));
        let mut follows: Vec<(String, Vec<&SongHeart>)> = Vec::new();
        for heart in &ready {
            let album = album_to_follow(state, self.sources, &heart.item_id, &heart.external_id);
            match album {
                Some(album) if self.albums.contains(&album) => {
                    info!(
                        "Starred song {} with its album {album} in the same request; the album heart fetches it",
                        heart.item_id
                    );
                    heart.queue_alone(state, requested_by.as_deref());
                }
                Some(album) => match follows.iter_mut().find(|(id, _)| *id == album) {
                    Some((_, songs)) => songs.push(heart),
                    None => follows.push((album, vec![heart])),
                },
                None => heart.queue_alone(state, requested_by.as_deref()),
            }
        }
        for (album, songs) in follows {
            let hearted: Vec<(String, String)> = songs
                .iter()
                .map(|heart| (heart.provider.clone(), heart.external_id.clone()))
                .collect();
            state.heart_acquisition_coordinator.queue_tracks_then_album_for(
                &hearted,
                &songs[0].provider,
                &album,
                requested_by.as_deref(),
                Some(who),
                songs[0].target.clone(),
            );
        }
    }
}

/// An outside song heart that is held and has its progress row, ready to go down the song chain.
struct SongHeart {
    provider: String,
    external_id: String,
    item_id: String,
    target: AcquisitionTarget,
}

impl SongHeart {
    /// Down the song chain, with no album after it.
    fn queue_alone(&self, state: &AppState, requested_by: Option<&str>) {
        state.heart_acquisition_coordinator.queue_track_for(
            &self.provider,
            &self.external_id,
            requested_by,
            self.target.clone(),
        );
    }
}

/// A heart on an outside album: a saved link for a member, else the whole album downloaded
/// and, outside Octo's own apps, favorited once it lands.
async fn star_outside_album(
    state: &AppState,
    call: &SubsonicCall,
    heart: &HeartOutcome,
    sources: &[HeartDownloadStep],
    who: Option<&str>,
    album_id: &str,
) {
    if let HeartOutcome::Save { user, request } = heart {
        save_album(state, user, album_id).await;
        if let Some(full) = request {
            ask_for_copy(state, user, album_id, RequestKind::Album, full).await;
        }
        return;
    }

    if !sources.iter().any(|step| step.album_enabled == Some(true)) {
        info!("Starred album {album_id} but no album-heart source is enabled; ignoring");
        return;
    }
    let Some(who) = who else {
        info!("Starred album {album_id} but nobody was named to download it for; ignoring");
        return;
    };

    // No storage-mode gate here, unlike the song branch. That gate exists because
    // Permanent mode already downloads a song when it is played; an album star is
    // a request for tracks the user has NOT played, so it must work in every mode.
    let album_provider = state
        .local_library
        .parse_song_id(album_id)
        .1
        .unwrap_or_else(|| SoulseekMetadataService::PROVIDER_NAME.to_string());

    info!("Starring external album {album_id}, triggering full album download");

    let metadata = state.music_metadata.clone();
    let (provider, album) = (album_provider.clone(), album_id.to_string());
    tokio::spawn(async move {
        // Log the size up front: downloads are serialized, so a large album is
        // a multi-hour job and the user should be able to see what they started.
        let album = metadata.get_album(&provider, &album).await;
        info!(
            "Album star: '{}' by {} has {} track(s); downloads run one at a time",
            album.as_ref().map_or("", |a| a.title.as_str()),
            album.as_ref().map_or("", |a| a.artist.as_str()),
            album.as_ref().map_or(0, |a| a.songs.len())
        );
    });

    // An empty exclude means "download every track". The engine already skips
    // tracks that are downloaded or in flight and isolates per-track failures.
    //
    // The progress list is claimed first, so the chain's first step already has a row
    // to move. Its name is the one the request signed in as (for an API key, its owner
    // as Navidrome names it), not RequesterFor: it decides who may see the row, and it
    // is never written anywhere.
    let target = match heart {
        HeartOutcome::Download(target) => target.clone(),
        HeartOutcome::Save { .. } => AcquisitionTarget::Shared,
    };
    // Held before the download is queued, so one that finishes at once still finds it. A
    // member's album songs are starred for them one by one where they land.
    if target == AcquisitionTarget::Shared
        && let Some(credential) = favorite_credential(call)
    {
        state
            .star_on_arrival
            .hold_album(&album_provider, album_id, &credential, Some(who));
    }
    state
        .acquisition_tracker
        .begin_album(&album_provider, album_id, Some(who));
    state.heart_acquisition_coordinator.queue_album_for(
        &album_provider,
        album_id,
        requester_for(state, Some(who)).as_deref(),
        target,
    );
}

/// A heart on an outside song: a saved link for a member, else a permanent download and,
/// outside Octo's own apps, a favorite once it lands. Answers the download, held and tracked,
/// for the caller to queue; `None` for a saved link or a heart nobody was named for.
async fn star_outside_song(
    state: &AppState,
    call: &SubsonicCall,
    heart: &HeartOutcome,
    who: Option<&str>,
    item_id: &str,
) -> Option<SongHeart> {
    let (_, provider, external_id) = state.local_library.parse_song_id(item_id);
    let provider = provider.unwrap_or_default();
    let external_id = external_id.unwrap_or_default();

    if let HeartOutcome::Save { user, request } = heart {
        save_song(state, user, item_id, &provider, &external_id).await;
        if let Some(full) = request {
            ask_for_copy(state, user, item_id, RequestKind::Song, full).await;
        }
        return None;
    }
    let Some(who) = who else {
        info!("Starred song {item_id} but nobody was named to download it for; ignoring");
        return None;
    };
    let target = match heart {
        HeartOutcome::Download(target) => target.clone(),
        HeartOutcome::Save { .. } => AcquisitionTarget::Shared,
    };

    // No storage-mode gate any more. It used to exclude Permanent on the grounds
    // that playing a track there already downloads it, but that was only ever true
    // through the blocking play path: so in Permanent mode a star fell through to
    // the relay and errored on an id Navidrome has never seen.
    //
    // Stars ride the queue's own channel: explicit user intent is never shed under
    // load the way a play is, and the request carries the album-walk flag rather
    // than inheriting whatever a concurrent play happened to ask for.
    info!("Starring external song {item_id}, queueing permanent download");

    // Held before the download is queued, so one that finishes at once still finds it. The
    // starred id is kept with it, so the caller's starred lists show the song meanwhile.
    if let Some(credential) = favorite_credential(call) {
        state
            .star_on_arrival
            .hold_starred_song(&provider, &external_id, item_id, &credential, Some(who));
    }
    // Keyed by what the pipeline knows, labeled with the id the client starred so the
    // app can find its row. Named from the routing, which a heart keeps in the registry (a
    // decoded id is registered now, so what was heard of it is found).
    let routing = state
        .external_id_registry
        .resolve_song(&external_id, true)
        .map(|r| r.snapshot());
    state.acquisition_tracker.begin(
        &provider,
        &external_id,
        Some(item_id),
        Some(who),
        routing.as_ref().and_then(|r| r.artist.as_deref()),
        routing.as_ref().and_then(|r| r.title.as_deref()),
        routing.as_ref().and_then(|r| r.album.as_deref()),
    );
    Some(SongHeart {
        provider,
        external_id,
        item_id: item_id.to_string(),
        target,
    })
}

/// The album that follows a hearted outside song, when DownloadAlbumOnSongStar is on, the song
/// names one, and a source takes album hearts.
fn album_to_follow(
    state: &AppState,
    sources: &[HeartDownloadStep],
    item_id: &str,
    external_id: &str,
) -> Option<String> {
    if !state.settings.current().subsonic.download_album_on_song_star {
        return None;
    }
    let Some(album_id) = album_of_song(state, external_id) else {
        info!(
            "Starred song {item_id} with DownloadAlbumOnSongStar on, but Octo knows no album for it; only the song is fetched"
        );
        return None;
    };
    if !sources.iter().any(|step| step.album_enabled == Some(true)) {
        info!(
            "Starred song {item_id} with DownloadAlbumOnSongStar on, but no album-heart source is enabled; only the song is fetched"
        );
        return None;
    }
    info!("Starred song {item_id} with DownloadAlbumOnSongStar on; album {album_id} follows it");
    Some(album_id)
}

/// The album id a client sees beside an outside song, minted the same way the song row mints
/// it: the song's album, or its title when it has none (a single, which the album lookup then
/// resolves to the real release). `None` for an id the registry does not know.
fn album_of_song(state: &AppState, external_id: &str) -> Option<String> {
    let routing = state.external_id_registry.lookup(external_id)?.snapshot();
    let artist = routing.artist.filter(|a| !is_blank(a))?;
    let album = routing
        .album
        .filter(|a| !is_blank(a))
        .or(routing.title)
        .filter(|a| !is_blank(a))?;
    Some(state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some(artist),
        album: Some(album),
        ..Default::default()
    }))
}

/// Saves an outside song for a family member as a link (it plays from its source and shows in
/// their starred lists).
async fn save_song(state: &AppState, user: &str, item_id: &str, provider: &str, external_id: &str) {
    let song = state.metadata_service.get_song(provider, external_id).await;
    let routing = state
        .external_id_registry
        .lookup(external_id)
        .map(|r| r.snapshot());
    let text = |from_song: Option<&str>, from_routing: Option<&str>| {
        from_song
            .filter(|t| !t.is_empty())
            .or(from_routing)
            .unwrap_or_default()
            .to_string()
    };
    let saved = SavedSong {
        id: item_id.to_string(),
        title: text(
            song.as_ref().map(|s| s.title.as_str()),
            routing.as_ref().and_then(|r| r.title.as_deref()),
        ),
        artist: text(
            song.as_ref().map(|s| s.artist.as_str()),
            routing.as_ref().and_then(|r| r.artist.as_deref()),
        ),
        album: text(
            song.as_ref().map(|s| s.album.as_str()),
            routing.as_ref().and_then(|r| r.album.as_deref()),
        ),
        cover_art: item_id.to_string(),
        duration: song
            .as_ref()
            .and_then(|s| s.duration)
            .or_else(|| routing.as_ref().and_then(|r| r.duration))
            .map_or(0, i64::from),
        explicit: song.as_ref().and_then(|s| s.explicit_content_lyrics).unwrap_or(0),
        saved: Utc::now(),
    };
    if state.family_saved.add(user, saved) {
        info!("FAMILY {user} saved {item_id}");
    }
}

/// Saves an outside album for a family member as a link.
async fn save_album(state: &AppState, user: &str, album_id: &str) {
    let provider = state
        .local_library
        .parse_song_id(album_id)
        .1
        .unwrap_or_else(|| SoulseekMetadataService::PROVIDER_NAME.to_string());
    let album = state.metadata_service.get_album(&provider, album_id).await;
    let saved = SavedAlbum {
        id: album_id.to_string(),
        name: album.as_ref().map(|a| a.title.clone()).unwrap_or_default(),
        artist: album.as_ref().map(|a| a.artist.clone()).unwrap_or_default(),
        cover_art: album_id.to_string(),
        song_count: album.as_ref().map_or(0, |a| a.songs.len() as i64),
        year: album.as_ref().and_then(|a| a.year),
        saved: Utc::now(),
    };
    if state.family_saved.add_album(user, saved) {
        info!("FAMILY {user} saved album {album_id}");
    }
}

/// A family member whose library is full asks for a copy instead of downloading one.
async fn ask_for_copy(state: &AppState, user: &str, id: &str, kind: RequestKind, why: &str) {
    match state
        .family_request_service
        .create(user, id, RequestQuality::Best, Some(kind))
        .await
    {
        Ok(request) => info!("FAMILY {why}: {user} asked for a copy of {id} ({})", request.id),
        Err(e) => info!("FAMILY {user} could not ask for a copy of {id}: {}", e.message),
    }
}

/// `unstar`: repeated ids and comma lists are taken one by one. Outside ids are answered by
/// Octo: a family member's saved song or album is forgotten, and a heart still waiting for its
/// download is dropped, so the song leaves the caller's starred lists. Library ids go to
/// Navidrome together. A request with no outside id, or only outside ids Octo holds nothing
/// for, goes where it always went.
pub async fn unstar(State(state): State<AppState>, req: Request) -> axum::response::Response {
    let member = family_caller(req.extensions())
        .filter(|caller| caller.managed)
        .map(|caller| caller.user.clone());
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let call = SubsonicCall::from_parts(&state, &parts, body.clone());
    let format = call.format();
    state.imports.forget_cached_songs(&sent_ids(&call, "id"));
    let ids: Vec<(&str, String)> = ["id", "albumId", "artistId"]
        .into_iter()
        .flat_map(|key| sent_ids(&call, key).into_iter().map(move |id| (key, id)))
        .collect();
    let is_outside = |id: &str| {
        member
            .as_deref()
            .is_some_and(|user| state.family_saved.is_saved(user, id))
            || state.local_library.parse_song_id(id).0
    };
    let (sent_outside, mut library): (Vec<(&str, String)>, Vec<(&str, String)>) =
        ids.into_iter().partition(|(_, id)| is_outside(id));
    // An outside song whose library copy a heart made the favorite: taking the heart back takes
    // back the copy's favorite. Ids are forgotten once Navidrome takes the unstar.
    let landed = landed_holder(&state, &call).await;
    let mut outside: Vec<(&str, String)> = Vec::new();
    let mut taken_back: Vec<String> = Vec::new();
    for (key, id) in sent_outside {
        let copy = landed
            .as_deref()
            .filter(|_| key == "id")
            .and_then(|holder| state.star_on_arrival.landed_copy(&id, holder));
        match copy {
            Some(copy) => {
                library.push(("id", copy));
                taken_back.push(id);
            }
            None => outside.push((key, id)),
        }
    }
    let mapped = !taken_back.is_empty();
    if landed.is_some() {
        taken_back.extend(library.iter().map(|(_, id)| id.clone()));
    }
    let forget = landed.as_deref().map(|holder| (holder, taken_back.as_slice()));
    // The waiting hearts among the outside ids, by their pipeline key.
    let held: Vec<(String, String)> = outside
        .iter()
        .filter_map(|(_, id)| {
            let (_, provider, external_id) = state.local_library.parse_song_id(id);
            Some((provider?, external_id?))
        })
        .filter(|(provider, external_id)| state.star_on_arrival.holds_song(provider, external_id))
        .collect();
    // Hearts held for an answer, a proof or the person, from every client: the caller leaves
    // each hold of the song, and a hold nobody is left in ends.
    let in_holds: Vec<(String, String)> = outside
        .iter()
        .filter(|(key, _)| *key == "id")
        .filter_map(|(_, id)| {
            let (_, provider, external_id) = state.local_library.parse_song_id(id);
            Some((provider?, external_id?))
        })
        .filter(|(provider, external_id)| {
            !state
                .soulseek_holds
                .records_for(&track_base_key(provider, external_id))
                .is_empty()
        })
        .collect();
    if !in_holds.is_empty() {
        // Nothing here reaches Navidrome, so the sign-in is checked before a heart is dropped.
        if member.is_none() {
            match refuse_unless_signed_in(&state, &call, &format).await {
                Ok(None) => {}
                Ok(Some(refused)) => return refused,
                Err(error) => return error.into_response(),
            }
        }
        if let Ok(who) = signed_in_user(&state, &call).await {
            for (provider, external_id) in &in_holds {
                state
                    .heart_acquisition_coordinator
                    .leave_holds(provider, external_id, &who);
            }
        }
    }
    // Hearted albums with held tracks: the caller leaves each, and a held track nobody is left in
    // is deleted.
    let album_holds: Vec<(String, String)> = outside
        .iter()
        .filter(|(key, _)| *key == "albumId")
        .map(|(_, id)| {
            let provider = state
                .local_library
                .parse_song_id(id)
                .1
                .unwrap_or_else(|| SoulseekMetadataService::PROVIDER_NAME.to_string());
            (provider, id.clone())
        })
        .filter(|(provider, id)| {
            let album_key = crate::services::common::acquisition_tracker::key_of(provider, id);
            state.soulseek_holds.snapshot().iter().any(|record| {
                record
                    .album_track
                    .as_ref()
                    .is_some_and(|track| track.album_key == album_key)
                    || (record.kind == crate::services::common::HeldKind::Album && record.external_id == *id)
            })
        })
        .collect();
    if !album_holds.is_empty() {
        if member.is_none() {
            match refuse_unless_signed_in(&state, &call, &format).await {
                Ok(None) => {}
                Ok(Some(refused)) => return refused,
                Err(error) => return error.into_response(),
            }
        }
        if let Ok(who) = signed_in_user(&state, &call).await {
            for (provider, album_id) in &album_holds {
                state
                    .heart_acquisition_coordinator
                    .leave_album_holds(provider, album_id, &who);
            }
        }
    }

    if !mapped && (outside.is_empty() || (member.is_none() && library.is_empty() && held.is_empty())) {
        return match member {
            None => {
                if let Some((holder, ids)) = forget
                    && !ids.is_empty()
                    && matches!(refuse_unless_signed_in(&state, &call, &format).await, Ok(None))
                {
                    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                    state.star_on_arrival.forget_landed(&ids, holder);
                }
                crate::http::catch_all::catch_all(State(state), Request::from_parts(parts, Body::from(body)))
                    .await
            }
            Some(_) => relay_unstar(&state, &call, &format, None, forget).await,
        };
    }

    if !held.is_empty() {
        // Nothing here reaches Navidrome, so the sign-in is checked before a heart is dropped.
        if member.is_none() {
            match refuse_unless_signed_in(&state, &call, &format).await {
                Ok(None) => {}
                Ok(Some(refused)) => return refused,
                Err(error) => return error.into_response(),
            }
        }
        if let (Some(credential), Ok(who)) = (
            SubsonicCredential::from(&call.parameters),
            signed_in_user(&state, &call).await,
        ) {
            let name = StarOnArrival::holder_name(&credential, Some(&who));
            for (provider, external_id) in &held {
                if state.star_on_arrival.release_song(provider, external_id, &name) {
                    info!("{name} took back the heart on {provider}:{external_id} before it arrived");
                }
            }
        }
    }
    if let Some(user) = &member {
        for (_, id) in &outside {
            if state.family_saved.remove(user, id) {
                info!("FAMILY {user} unsaved {id}");
            }
        }
    }
    if library.is_empty() {
        return state
            .subsonic_response_builder
            .create_response(&format, "unstar")
            .into_response();
    }
    relay_unstar(&state, &call, &format, Some(library.as_slice()), forget).await
}

/// Relays an unstar to Navidrome: the request as sent, or with only `library` as its ids. Once
/// Navidrome took it, the listener's lists read their hearts again, and the ids in `forget` no
/// longer read as favorites through a heart.
async fn relay_unstar(
    state: &AppState,
    call: &SubsonicCall,
    format: &str,
    library: Option<&[(&str, String)]>,
    forget: Option<(&str, &[String])>,
) -> axum::response::Response {
    let relayed = match library {
        None => call.proxy.relay("rest/unstar", &call.parameters).await,
        Some(library) => {
            let mut pairs: Vec<(String, String)> = call
                .parameters
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "id" | "albumId" | "artistId"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            pairs.extend(library.iter().map(|(key, id)| (key.to_string(), id.clone())));
            call.proxy
                .relay("rest/unstar", pairs.iter().map(|(k, v)| (k, v)))
                .await
        }
    };
    match relayed {
        Ok(result) => {
            let accepted = is_successful_subsonic_response(&result.body, format);
            if accepted && let Some((holder, ids)) = forget {
                let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                state.star_on_arrival.forget_landed(&ids, holder);
            }
            if library.is_some()
                && accepted
                && let Some(username) = listener(state, call).await
            {
                state.generated_playlists.forget_marks(&username);
            }
            let content_type = result
                .content_type
                .unwrap_or_else(|| format!("application/{format}"));
            file(result.body, &content_type)
        }
        Err(e) => state
            .subsonic_response_builder
            .create_error(format, 0, &format!("Error connecting to Subsonic server: {e}"))
            .into_response(),
    }
}

/// `getStarred` and `getStarred2`: Navidrome's answer, with the outside songs the caller hearted
/// that are still waiting for their download, as outside songs are shaped everywhere else. An
/// app that reads its favorites from here keeps the heart until the file lands and Navidrome's
/// own favorite takes over. Only the caller's own waiting hearts are added; with none, the answer
/// is Navidrome's as it came.
pub async fn get_starred(State(state): State<AppState>, req: Request) -> axum::response::Response {
    if !state.star_on_arrival.holds_any_song() {
        return crate::http::catch_all::catch_all(State(state), req).await;
    }
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let call = SubsonicCall::from_parts(&state, &parts, body.clone());
    let container = if parts.uri.path().to_ascii_lowercase().contains("getstarred2") {
        "starred2"
    } else {
        "starred"
    };
    let answer =
        crate::http::catch_all::catch_all(State(state.clone()), Request::from_parts(parts, Body::from(body)))
            .await;
    with_waiting_hearts(&state, &call, container, answer).await
}

/// The answer with the caller's waiting hearts added, when it is a plain 200 Navidrome accepted.
async fn with_waiting_hearts(
    state: &AppState,
    call: &SubsonicCall,
    container: &str,
    answer: axum::response::Response,
) -> axum::response::Response {
    if answer.status() != StatusCode::OK || answer.headers().contains_key(header::CONTENT_ENCODING) {
        return answer;
    }
    let Some(credential) = SubsonicCredential::from(&call.parameters) else {
        return answer;
    };
    let Ok(who) = signed_in_user(state, call).await else {
        return answer;
    };
    let waiting = state
        .star_on_arrival
        .waiting_songs(&StarOnArrival::holder_name(&credential, Some(&who)));
    if waiting.is_empty() {
        return answer;
    }
    let (mut parts, body) = answer.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, STARRED_ANSWER_LIMIT).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let songs = waiting_hearts(state, &waiting).await;
    let builder = &state.subsonic_response_builder;
    let edited = edit(
        &bytes,
        |response| {
            if response.get("status").and_then(Value::as_str) != Some("ok") {
                return false;
            }
            let holder = response
                .entry(container)
                .or_insert_with(|| Value::Object(Default::default()));
            let Some(holder) = holder.as_object_mut() else {
                return false;
            };
            let list = holder.entry("song").or_insert_with(|| Value::Array(Vec::new()));
            let Some(list) = list.as_array_mut() else {
                return false;
            };
            let known: Vec<String> = list
                .iter()
                .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
                .collect();
            for (song, starred) in &songs {
                if known.contains(&song.id) {
                    continue;
                }
                let mut fields = builder.convert_song_to_json(song);
                fields.insert("starred".into(), starred_time(starred).into());
                list.push(Value::Object(fields));
            }
            true
        },
        |root| {
            if root.attribute("status") != Some("ok") {
                return false;
            }
            let ns = root.namespace.clone();
            if !root.elements().any(|e| e.name == container) {
                root.push(XElement::in_namespace(ns.as_deref(), container));
            }
            let Some(holder) = root.elements_mut().find(|e| e.name == container) else {
                return false;
            };
            let known: Vec<String> = holder
                .elements()
                .filter_map(|e| e.attribute("id").map(str::to_string))
                .collect();
            for (song, starred) in &songs {
                if known.contains(&song.id) {
                    continue;
                }
                let mut element = builder.convert_song_to_xml(song, ns.as_deref());
                element.set_attr("starred", starred_time(starred));
                holder.push(element);
            }
            true
        },
    );
    let bytes = match edited {
        Edit::Changed(changed) => Bytes::from(changed),
        Edit::Unchanged | Edit::Unreadable => bytes,
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::TRANSFER_ENCODING);
    axum::response::Response::from_parts(parts, Body::from(bytes))
}

/// The largest starred answer read to add the waiting hearts to.
const STARRED_ANSWER_LIMIT: usize = 64 * 1024 * 1024;

/// Each waiting heart as the outside song it is, with when it was hearted: what the metadata
/// already knows of it, else what its routing remembers. Nothing new is looked up.
async fn waiting_hearts(state: &AppState, waiting: &[WaitingStar]) -> Vec<(Song, DateTime<Utc>)> {
    let mut songs = Vec::new();
    let mut times = Vec::new();
    for heart in waiting {
        let (_, provider, external_id) = state.local_library.parse_song_id(&heart.song_id);
        let known = state
            .music_metadata
            .get_song(
                provider.as_deref().unwrap_or_default(),
                external_id.as_deref().unwrap_or_default(),
            )
            .await;
        let song = match known {
            Some(mut song) => {
                song.id = heart.song_id.clone();
                song
            }
            None => {
                let routing = state
                    .external_id_registry
                    .lookup(&heart.song_id)
                    .map(|r| r.snapshot());
                let text = |pick: fn(&SoulseekRouting) -> Option<&String>| {
                    routing.as_ref().and_then(pick).cloned().unwrap_or_default()
                };
                Song {
                    id: heart.song_id.clone(),
                    title: text(|r| r.title.as_ref()),
                    artist: text(|r| r.artist.as_ref()),
                    album: text(|r| r.album.as_ref()),
                    duration: routing.as_ref().and_then(|r| r.duration).filter(|d| *d > 0),
                    is_local: false,
                    ..Default::default()
                }
            }
        };
        songs.push(song);
        times.push(heart.starred_at);
    }
    state.music_metadata.refresh_outside_songs(&mut songs);
    songs.into_iter().zip(times).collect()
}

fn starred_time(at: &DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Star ratings.
///
/// Two jobs, in this order and never the other way round:
///
/// 1. ALWAYS relay to Navidrome and return ITS response verbatim. A rating the server did
///    not record snaps back in the client on the next refresh, and rating a track is a
///    legitimate thing to do for its own sake. Octo reading meaning into it must never cost
///    the user the rating itself.
/// 2. Only then, if library actions are on, the rating maps to an enabled action and the
///    user is on the allowlist, queue it. Queue, not execute: nothing that removes a file
///    runs inside a request.
///
/// Before this there was no handler at all and setRating fell through to the catch-all,
/// where an Octo id became a synthetic OK that never reached Navidrome. That behavior is
/// preserved exactly.
pub async fn set_rating(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = call.format();
    let item_id = call.param("id").to_string();
    state.imports.forget_cached_songs(std::slice::from_ref(&item_id));
    let rating_text = call.param("rating").to_string();
    let builder = &state.subsonic_response_builder;

    // An external id is not a Navidrome song, and relaying one errors "data not found".
    if !item_id.is_empty() && state.local_library.parse_song_id(&item_id).0 {
        // Nothing to relay, so a ping is the auth check before the listener's radio changes.
        if let Some(external_stars) = try_parse_int(&rating_text) {
            let mut auth = call.parameters.clone();
            auth.shift_remove("id");
            auth.shift_remove("rating");
            if call
                .proxy
                .relay_safe("rest/ping", &auth)
                .await
                .is_some_and(|check| is_successful_subsonic_response(&check.body, &format))
            {
                let external_rater = listener_name(&state, &call).await;
                if !external_rater.is_empty() {
                    let external_rater = external_rater.as_str();
                    record_radio_ban(&state, &call, external_rater, &item_id, external_stars).await;
                    if external_stars == TOP_RATING {
                        state
                            .listening
                            .heart(external_rater, std::slice::from_ref(&item_id), Utc::now());
                    }
                }
            }
        }
        return Ok(builder.create_response(&format, "setRating").into_response());
    }

    let result = match call.proxy.relay("rest/setRating", &call.parameters).await {
        Ok(result) => result,
        Err(RelayError::Http(message)) => {
            return Ok(builder
                .create_error(
                    &format,
                    0,
                    &format!("Error connecting to Subsonic server: {message}"),
                )
                .into_response());
        }
        Err(other) => return Err(other.into()),
    };

    let accepted = is_successful_subsonic_response(&result.body, &format);
    let stars = try_parse_int(&rating_text).filter(|_| accepted && !item_id.is_empty());
    let rater = match stars {
        Some(_) => listener(&state, &call).await,
        None => None,
    };
    if let (Some(stars), Some(rater)) = (stars, rater.as_deref()) {
        state.generated_playlists.rated(rater, &item_id, i64::from(stars));
        record_radio_ban(&state, &call, rater, &item_id, stars).await;
        if stars == TOP_RATING {
            state
                .listening
                .heart(rater, std::slice::from_ref(&item_id), Utc::now());
        }
    }

    let settings = state.settings.current().library_actions.clone();
    if let Some(rating) = stars
        && let Some(definition) = settings.action_for_rating(rating)
    {
        act_on_rating(
            &state,
            &call,
            &item_id,
            rating,
            definition.action,
            rater.as_deref(),
        )
        .await;
    }

    let content_type = result
        .content_type
        .unwrap_or_else(|| format!("application/{format}"));
    Ok(file(result.body, &content_type))
}

/// Queues the action a rating asks for, signed in the way the rating came: u with t and s, a
/// password turned into a token first, or an API key. Navidrome accepting the rating with that
/// sign-in IS the auth check; Octo trusts it exactly as far as Navidrome just did. A rating that
/// gets no action, or only a rehearsal, is kept for the dashboard with the reason.
async fn act_on_rating(
    state: &AppState,
    call: &SubsonicCall,
    item_id: &str,
    rating: i32,
    action: LibraryAction,
    rater: Option<&str>,
) {
    let worker = &state.library_action_rating_worker;
    let credential = SubsonicCredential::from(&call.parameters).map(|c| c.without_password());
    let queued = worker.check(rater).and_then(|()| {
        let (Some(username), Some(credential)) = (rater, credential) else {
            return Err(RatingIgnored::UnknownUser);
        };
        if !rating_in_scope(state, username, item_id) {
            return Err(RatingIgnored::OutsideNotices);
        }
        worker.try_enqueue(RatingActionRequest {
            action,
            navidrome_id: item_id.to_string(),
            username: username.to_string(),
            credential,
        })
    });
    let ignored = match queued {
        // A Keep rating is most often a plain "I love this song", so one that does nothing is
        // never kept or logged: it would crowd out the ratings that asked for a change.
        Err(_) if action == LibraryAction::Keep => return,
        Err(reason) => reason,
        // Keep touches no file, so a rehearsal changes nothing about it.
        Ok(()) if action != LibraryAction::Keep && state.settings.current().library_actions.dry_run => {
            RatingIgnored::DryRun
        }
        Ok(()) => return,
    };
    let username = rater.unwrap_or_default().to_string();
    let at_utc = Utc::now();
    worker.note_ignored(IgnoredRating {
        at_utc,
        reason: ignored.reason(&username),
        username: username.clone(),
        song_id: item_id.to_string(),
        artist: None,
        title: None,
        rating,
        action,
    });
    // The song's name is looked up after the reply, so the rating never waits on it.
    let worker = worker.clone();
    let resolver = resolver(state, call);
    let parameters = call.parameters.clone();
    let song_id = item_id.to_string();
    tokio::spawn(async move {
        if let Some(song) = resolver.resolve_scrobble(&song_id, &parameters).await {
            worker.name_ignored(&username, &song_id, at_utc, &song.artist, &song.title);
        }
    });
}

/// The listener a request Navidrome accepted speaks for, named the way every request's identity
/// is: `u`, or for an API key sign-in its owner as Navidrome names it. None when there is nobody
/// to name or Navidrome could not be asked.
async fn listener(state: &AppState, call: &SubsonicCall) -> Option<String> {
    state
        .request_identity
        .username(&call.parameters, &call.proxy)
        .await
        .ok()
        .flatten()
        .filter(|u| !u.is_empty())
}

/// One star keeps a song off this listener's radio; any other rating lifts that. Never fails the
/// rating itself: the radio is a side effect of it.
async fn record_radio_ban(state: &AppState, call: &SubsonicCall, username: &str, item_id: &str, stars: i32) {
    let resolver = resolver(state, call);
    if let Some(rated) = resolver.resolve_scrobble(item_id, &call.parameters).await {
        state
            .last_fm_radio_state
            .set_radio_ban(username, item_id, &rated.artist, &rated.title, stars == 1);
    }
    if stars == 1 {
        state.radio_outcomes.disliked(username, item_id, Utc::now());
    }
}

/// Whether a star is a command here. NoticeOnly: only on a track in one of Octo's notice
/// playlists, where the only reason to rate it is to answer (#47). Global: any track.
fn rating_in_scope(state: &AppState, username: &str, item_id: &str) -> bool {
    state.settings.current().library_actions.effective_ratings_scope() == LibraryRatingScope::Global
        || state.notice_queue.is_queued(username, item_id)
}

/// An integer parsed from text: surrounding white space and a sign are allowed.
fn try_parse_int(text: &str) -> Option<i32> {
    text.trim().parse::<i32>().ok()
}

/// Scrobbles a play. Plays of external songs go to Last.fm (when configured) and the radio's
/// listening profile; once Navidrome accepts the caller, the first id also prewarms the next
/// songs of a queue.
///
/// Library songs are relayed to Navidrome too, because real scrobbling
/// (last-played stats, the Now Playing panel) is the upstream's job. Outside
/// songs are not: Navidrome has no such media to record.
pub async fn scrobble(State(state): State<AppState>, req: Request) -> AppResult {
    let caller = family_caller(req.extensions()).cloned();
    // The device the family gate found, read before the request is taken apart.
    let device_id = family_caller(req.extensions()).and_then(|caller| caller.device_id.clone());
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let mut ids = call.incoming.parameter_values("id");
    let submissions = call.incoming.parameter_values("submission");
    let mut times = call.incoming.parameter_values("time");
    if ids.is_empty()
        && let Some(single_id) = call.param_opt("id").filter(|id| !id.is_empty())
    {
        ids = vec![single_id.to_string()];
    }
    let id = ids.first().cloned().unwrap_or_default();
    let format = call.format();

    // The next songs of a radio queue start resolving only once Navidrome has accepted the
    // caller: an unchecked request never spends YouTube lookups.
    let prewarm_queue = || {
        if id.is_empty() {
            return;
        }
        let upcoming = state.radio_queues.get_upcoming_from(&id, 16);
        if !upcoming.is_empty() {
            debug!("scrobble {id}: prewarming next {} from queue", upcoming.len());
            let metadata = state.music_metadata.clone();
            // The next three outside songs first, with their streams resolved, so whichever
            // plays next starts from the shim's cache; then videos for the rest.
            tokio::spawn(async move {
                let readied = metadata
                    .prewarm_next_streams(&upcoming, NEXT_STREAMS_WARMED)
                    .await;
                let rest: Vec<String> = upcoming
                    .into_iter()
                    .filter(|song| !readied.contains(song))
                    .collect();
                metadata.prewarm_you_tube_ids_for_song_ids(&rest, 8).await;
            });
        }
    };

    // Library songs pass through so Navidrome's last-played/Now Playing stays accurate.
    // An outside id is not Navidrome's: relayed, it logs "data not found" on every play
    // and records nothing (#60). So only library ids go upstream, each keeping its
    // own submission and time when the client sent one per id.
    let library: Vec<usize> = (0..ids.len())
        .filter(|index| !state.local_library.parse_song_id(&ids[*index]).0)
        .collect();
    // Times that do not pair up with the ids are dropped, not relayed. Navidrome refuses
    // such a scrobble outright, and leaving the outside ids out could make the counts
    // match by accident and pin an outside song's time on a library song.
    if !times.is_empty() && times.len() != ids.len() {
        times = Vec::new();
    }
    let library_only = |values: &[String]| -> Vec<String> {
        if values.len() == ids.len() {
            library.iter().map(|index| values[*index].clone()).collect()
        } else {
            values.to_vec()
        }
    };

    let mut relay_parameters: Vec<(String, String)> = call
        .parameters
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "id" | "submission" | "time"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let outcome: Result<axum::response::Response, RelayError> = async {
        if !ids.is_empty() && library.is_empty() {
            // Nothing for Navidrome to record, but its answer was also the credential
            // check that gates learning below. A ping checks the same credentials and
            // answers in the very shape a scrobble does, failures included.
            let check = call
                .proxy
                .relay("rest/ping", relay_parameters.iter().map(|(k, v)| (k, v)))
                .await?;
            if is_successful_subsonic_response(&check.body, &format) {
                prewarm_queue();
                note_now_playing(&state, &call, caller.as_ref(), &ids, &submissions);
                learn_from_scrobbles(&state, &call, &ids, &submissions, &times, device_id.as_deref()).await?;
            }
            let content_type = check
                .content_type
                .unwrap_or_else(|| format!("application/{format}"));
            return Ok(file(check.body, &content_type));
        }
        relay_parameters.extend(library_only(&ids).into_iter().map(|v| ("id".to_string(), v)));
        relay_parameters.extend(
            library_only(&submissions)
                .into_iter()
                .map(|v| ("submission".to_string(), v)),
        );
        relay_parameters.extend(library_only(&times).into_iter().map(|v| ("time".to_string(), v)));
        let result = call
            .proxy
            .relay("rest/scrobble", relay_parameters.iter().map(|(k, v)| (k, v)))
            .await?;
        if is_successful_subsonic_response(&result.body, &format) {
            prewarm_queue();
            note_now_playing(&state, &call, caller.as_ref(), &ids, &submissions);
            learn_from_scrobbles(&state, &call, &ids, &submissions, &times, device_id.as_deref()).await?;
        }
        let content_type = result
            .content_type
            .unwrap_or_else(|| format!("application/{format}"));
        Ok(file(result.body, &content_type))
    }
    .await;

    match outcome {
        Ok(response) => Ok(response),
        // Even if upstream is briefly unhappy, return 200 so the client
        // doesn't think scrobble is broken.
        Err(RelayError::Http(_)) => Ok(state
            .subsonic_response_builder
            .create_response(&format, "scrobble")
            .into_response()),
        Err(other) => Err(AppError::from(other)),
    }
}

/// A now-playing scrobble (`submission=false`) tells the songs fetched ahead what is playing,
/// in the background, so the answer never waits for it.
fn note_now_playing(
    state: &AppState,
    call: &SubsonicCall,
    caller: Option<&FamilyCaller>,
    ids: &[String],
    submissions: &[String],
) {
    if !state.settings.current().prefetch.enabled {
        return;
    }
    let playing: Vec<String> = ids
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            let submission = if submissions.len() == 1 {
                submissions.first()
            } else {
                submissions.get(*index)
            };
            submission.is_some_and(|s| !is_true(s))
        })
        .map(|(_, id)| id.clone())
        .filter(|id| !id.is_empty())
        .collect();
    if playing.is_empty() {
        return;
    }
    let (state, caller) = (state.clone(), caller.cloned());
    let (parameters, proxy) = (call.parameters.clone(), call.proxy.clone());
    tokio::spawn(async move {
        let Some(who) = state
            .request_identity
            .username(&parameters, &proxy)
            .await
            .ok()
            .flatten()
            .filter(|u| !u.is_empty())
        else {
            return;
        };
        for id in playing {
            state.prefetch.on_playing(&who, &id, caller.as_ref()).await;
        }
    });
}

/// What a completed scrobble teaches: the radio profile (when personalised radio is
/// on), the listener's Last.fm history (outside songs, and library songs unless they are
/// left to Navidrome) and, for an external track, ListenBrainz. A start-of-play event
/// becomes Last.fm's Now Playing.
async fn learn_from_scrobbles(
    state: &AppState,
    call: &SubsonicCall,
    ids: &[String],
    submissions: &[String],
    times: &[String],
    device_id: Option<&str>,
) -> Result<(), RelayError> {
    // Called only once Navidrome has accepted these credentials. An API key sign-in has no
    // u, so its owner is asked of Navidrome; nobody to name means nothing is learned.
    let Some(username) = state
        .request_identity
        .username(&call.parameters, &call.proxy)
        .await?
        .filter(|u| !u.is_empty())
    else {
        return Ok(());
    };
    // What radio learns: a song it suggested, started and then played through, or started and
    // never finished. Before the early return below, so it works with stations off.
    state
        .radio_outcomes
        .observe(&username, ids, submissions, Utc::now());
    let last_fm_settings = state.settings.current().last_fm.clone();
    let learning = last_fm_settings.enable_radio && last_fm_settings.enable_personalized_stations;
    let submitting = state.listen_brainz.is_enabled_for(&username);
    let scrobbling = state.last_fm_scrobbles.is_enabled_for(&username);
    let listening = state.listening.recording();
    if !learning && !submitting && !scrobbling && !listening {
        return Ok(());
    }
    let resolver = resolver(state, call);
    let mut recorded = false;
    for (index, song_id) in ids.iter().enumerate() {
        // Explicit start/now-playing scrobbles are not completed plays. Clients that
        // omit submission are accepted because many only send one credible event. One
        // submission for several ids is for all of them, as Navidrome reads it.
        let completed = if submissions.len() == 1 {
            is_true(&submissions[0])
        } else {
            index >= submissions.len() || is_true(&submissions[index])
        };
        // Last.fm hears about outside songs, and library songs too unless the admin left
        // those to a Navidrome that scrobbles them itself (else they would count twice).
        let outside = state.local_library.parse_song_id(song_id).0;
        let last_fm_takes = scrobbling && (outside || state.last_fm_scrobbles.takes_library_plays());
        // A start of play ends the play before it, which the listening log then writes down.
        if !completed && listening {
            let ended = state.listening.now_playing(&username, song_id, Utc::now());
            listening::write_settled(state.listening.clone(), state.last_fm.clone(), ended);
        }
        if !completed && !last_fm_takes {
            continue;
        }
        // A client that sends the same completed play again is not playing it again.
        let time = times.get(index).map(String::as_str);
        let reported_at = state.recent_scrobbles.now();
        if completed
            && !state
                .recent_scrobbles
                .first_report(&username, song_id, time, reported_at)
        {
            debug!("Scrobble of {song_id} for {username} repeats one already learned from");
            continue;
        }
        // Until something has learned from the play, it is not taken: a song that could not
        // be looked up this time is withdrawn, so the client's retry counts.
        let resolved = resolver
            .resolve_scrobble_with_details(song_id, &call.parameters)
            .await;
        let (song, details) = match resolved {
            Some((song, details)) => (Some(song), details),
            None => (None, ScrobbleDetails::default()),
        };
        let Some(song) = song.filter(|s| !s.artist.is_empty() && !s.title.is_empty()) else {
            if completed {
                state
                    .recent_scrobbles
                    .withdraw(&username, song_id, time, reported_at);
            }
            continue;
        };
        let track = LastFmTrack::new(
            song.artist.clone(),
            song.title.clone(),
            Some(&song.album),
            song.duration,
        );
        if !completed {
            if last_fm_takes {
                state.last_fm_scrobbles.now_playing(&username, track);
            }
            continue;
        }
        let sent_time = time
            .and_then(try_parse_long)
            .and_then(from_unix_time_milliseconds);
        let played_at = sent_time.unwrap_or_else(Utc::now);
        if listening {
            record_listening(state, call, &username, &song, details, sent_time, device_id);
        }
        if learning {
            recorded |= state.last_fm_radio_state.record_play(
                &username,
                LastFmRadioPlay {
                    song_id: song_id.clone(),
                    artist: song.artist.clone(),
                    title: song.title.clone(),
                    album: Some(song.album.clone()),
                    genre: song.genre.clone(),
                    duration: song.duration,
                    is_local: song.is_local,
                    played_at_utc: played_at,
                    source: "scrobble".into(),
                    ..Default::default()
                },
            );
        }
        // Queued, not awaited: the client's answer never waits on Last.fm.
        if last_fm_takes {
            state
                .last_fm_scrobbles
                .scrobble(&username, track, played_at, true);
        }
        if submitting && !song.is_local {
            state
                .listen_brainz
                .submit_listen(
                    &username,
                    &song.artist,
                    &song.title,
                    Some(&song.album),
                    song.duration,
                    played_at,
                )
                .await;
        }
    }
    if !learning || !recorded {
        return Ok(());
    }
    let user = state.last_fm_radio_state.get_user(&username);
    if last_fm_radio_refresh_policy::should_refresh_after_play(&user, &last_fm_settings, Utc::now()) {
        state.last_fm_radio_refresh_queue.enqueue(&username, None);
    }
    Ok(())
}

/// Tells the listening log an app reported the play as played. It is written once it ends,
/// its genre worked out then, after the reply.
fn record_listening(
    state: &AppState,
    call: &SubsonicCall,
    username: &str,
    song: &Song,
    details: ScrobbleDetails,
    sent_time: Option<DateTime<Utc>>,
    device_id: Option<&str>,
) {
    let now = Utc::now();
    let user_agent = call.header("user-agent").unwrap_or_default();
    let client = call.param_opt("c").unwrap_or("");
    let album = details
        .album_id
        .clone()
        .filter(|_| song.is_local)
        .map(|album_id| AlbumLookup {
            proxy: call.proxy.clone(),
            parameters: call.parameters.clone(),
            album_id,
        });
    let mut genres = details.genres;
    if genres.is_empty()
        && let Some(genre) = song.genre.as_deref().filter(|g| !g.trim().is_empty())
    {
        genres.push(genre.to_string());
    }
    let play = ListeningPlay {
        song_id: song.id.clone(),
        cover_id: details.cover_art.unwrap_or_else(|| song.id.clone()),
        artist: song.artist.clone(),
        album_artist: details.album_artist,
        title: song.title.clone(),
        album: song.album.clone(),
        is_local: song.is_local,
        app: octo_core::family::device::app_label(client, &user_agent),
        platform: listening::platform_of(&user_agent, client).to_string(),
        device_id: device_id.map(str::to_string),
        source: "scrobble".to_string(),
        ..Default::default()
    };
    let ended = state.listening.completed(
        username,
        listening::Draft { play, genres, album },
        song.duration.map(i64::from),
        sent_time,
        now,
    );
    listening::write_settled(state.listening.clone(), state.last_fm.clone(), ended);
}

/// The song ids a star or a top rating named, written down as hearts in the listening log so
/// a play on at the time is marked hearted while playing.
async fn note_listening_hearts(state: &AppState, call: &SubsonicCall, ids: &[String]) {
    if ids.is_empty() || !state.listening.recording() {
        return;
    }
    let at = Utc::now();
    if let Some(username) = listener(state, call).await {
        state.listening.heart(&username, ids, at);
    }
}

/// A 64-bit integer: surrounding white space and a sign are allowed.
fn try_parse_long(text: &str) -> Option<i64> {
    text.trim().parse::<i64>().ok()
}

/// A time from Unix milliseconds: `None` outside years 1 to 9999.
fn from_unix_time_milliseconds(unix: i64) -> Option<DateTime<Utc>> {
    const MIN: i64 = -62_135_596_800_000;
    const MAX: i64 = 253_402_300_799_999;
    if !(MIN..=MAX).contains(&unix) {
        return None;
    }
    DateTime::from_timestamp_millis(unix)
}

/// OpenSubsonic reportPlayback: Feishin pings this on play-start and during playback (176
/// hits in one session). For external ids Navidrome has no such media and returns an error,
/// so we ack with ok; local ids relay through so Navidrome's now-playing stays accurate.
pub async fn report_playback(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = call.format();
    let media_id = call
        .param_opt("mediaId")
        .unwrap_or_else(|| call.param("id"))
        .to_string();
    let (is_external, _, _) = state.local_library.parse_song_id(&media_id);

    if !is_external
        && !media_id.is_empty()
        && let Some(relay) = call
            .proxy
            .relay_safe("rest/reportPlayback", &call.parameters)
            .await
    {
        let content_type = relay
            .content_type
            .unwrap_or_else(|| format!("application/{format}"));
        return Ok(file(relay.body, &content_type));
    }
    Ok(state
        .subsonic_response_builder
        .create_response(&format, "reportPlayback")
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_milliseconds_outside_years_1_to_9999_are_refused() {
        assert!(from_unix_time_milliseconds(0).is_some());
        assert!(from_unix_time_milliseconds(253_402_300_799_999).is_some());
        assert!(from_unix_time_milliseconds(253_402_300_800_000).is_none());
        assert!(from_unix_time_milliseconds(-62_135_596_800_001).is_none());
        assert_eq!(try_parse_long(" 12 "), Some(12));
        assert_eq!(try_parse_int("+3"), Some(3));
        assert_eq!(try_parse_int("x"), None);
    }
}

#[cfg(test)]
#[path = "stars_family_tests.rs"]
mod family_tests;

#[cfg(test)]
#[path = "stars_hearts_tests.rs"]
mod hearts_tests;

#[cfg(test)]
#[path = "family_library_tests.rs"]
mod family_library_tests;

#[cfg(test)]
#[path = "stars_album_tests.rs"]
mod album_tests;

#[cfg(test)]
#[path = "stars_landing_tests.rs"]
mod landing_tests;
