//! `stream`, `download`, `getCoverArt` and `getTranscodeDecision`.

use std::io::SeekFrom;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use futures::FutureExt;
use octo_core::common::dotnet::{self, eq_ignore_case, is_blank};
use octo_core::common::{SongIdentity, playlist_id_helper};
use octo_core::fingerprint::heart_gate::{
    AcquisitionOrigin, HeardRecord, HeardSource, NATIVE_CLIENT, PlayedMark,
};
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioStationKind};
use octo_core::settings::{AppSettings, HeartDownloadSource, NoticeKind};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_media::cover::{CoverGlyph, CoverSeed, ListCover, list_kinds};
use octo_subsonic::SubsonicCredential;
use octo_subsonic::subsonic_request_parser::Parameters;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::action_helpers::MAX_REQUEST_BODY;
use super::caller_checks::{
    SubsonicCall, file, is_first_byte_request, is_successful_subsonic_response, refuse_unless_signed_in,
    requester_for, signed_in_user,
};
use crate::app::AppState;
use crate::http::catch_all;
use crate::http::error::{AppError, AppResult, json_status, problem};
use crate::http::routes::RouteSet;
use crate::http::static_files::{RangeOutcome, parse_range};
use crate::services::common::heart_acquisition_coordinator::track_base_key;
use crate::services::common::track_acquisition_queue::{Completion, PlayOutcome};
use crate::services::cover_art::local_cover_cache::{CoverPicture, Fetched, NotKept};
use crate::services::family::{FamilyCaller, LibraryScope, audio_gate, family_caller};
use crate::services::fingerprint::music_brainz_client::heart_song;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::{GeneratedPlaylist, navidrome_song_list};
use crate::services::prefetch::{Capture, FpcalcClass, Source, from_start, sample_cap, sample_id_of};
use crate::services::soulseek::soulseek_download_service::played_size;
use crate::services::subsonic::subsonic_proxy_service::content_type_of;
use crate::services::subsonic::{CredentialVerdict, SubsonicProxyService};

pub fn routes() -> RouteSet {
    // The audio endpoints take HEAD themselves (see `audio_head`), at both spellings.
    let audio = [
        ("stream", get(stream).post(stream).head(audio_head)),
        ("download", get(download).post(download).head(audio_head)),
    ];
    let mut set = RouteSet::new();
    for (name, methods) in audio {
        for template in [format!("/rest/{name}"), format!("/rest/{name}.view")] {
            set = set.route_with_head(&template, methods.clone());
        }
    }
    set.subsonic("getCoverArt", get(get_cover_art).post(get_cover_art))
        .subsonic(
            "getTranscodeDecision",
            get(get_transcode_decision).post(get_transcode_decision),
        )
}

// ---------------------------------------------------------------------------------------
// stream
// ---------------------------------------------------------------------------------------

/// Downloads on-the-fly if needed, or streams directly in Stream mode.
pub async fn stream(State(state): State<AppState>, req: Request) -> AppResult {
    let caller = family_caller(req.extensions()).cloned();
    let address = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip());
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = call.param("id").to_string();
    // Needed so a failure here can answer with a real Subsonic error envelope instead
    // of the bare JSON this method used to emit.
    let format = call.format();

    if is_blank(&id) {
        return Ok(json_status(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "Missing id parameter" }),
        ));
    }

    let (is_external, provider, external_id) = state.local_library.parse_song_id(&id);

    // Verbose entry log: every stream call gets a single line tagged with
    // the client + id + isExternal + Range + UA + key headers. Diagnostics
    // for "client X never plays external songs": if a tap doesn't even
    // reach this log line, the client is filtering on its side.
    let client_name = call.param_opt("c").unwrap_or("?").to_string();
    let range_in = call.header("Range").unwrap_or_else(|| "(none)".into());
    let ua_in = call.header("User-Agent").unwrap_or_else(|| "(none)".into());
    info!(
        "STREAM-IN client={client_name} id={id} isExternal={} range={range_in} ua={ua_in}",
        if is_external { "True" } else { "False" }
    );

    if !is_external {
        let response = call.proxy.relay_stream(&call.parameters).await;
        // Navidrome took the sign-in, so a play of a library song moves the songs fetched ahead
        // along its queue too.
        if response.status().is_success() {
            note_playing(&state, &call, caller.as_ref(), &id, None);
        }
        return Ok(response);
    }

    // Navidrome checks the sign-in on everything relayed to it, but it never sees an outside
    // song, so without this anyone who can reach Octo could play through it with no account.
    if let Some(refused) = refuse_unless_signed_in(&state, &call, &format).await? {
        return Ok(refused);
    }

    let provider = provider.unwrap_or_default();
    let external_id = external_id.unwrap_or_default();

    if is_first_byte_request(call.method().as_str(), call.header("Range").as_deref()) {
        // A stream and a heart of one song id meet on one registered routing.
        state.external_id_registry.resolve_song(&external_id, true);
        warm_up_playing(&state, &external_id);
        // A play of a held song is one of the shim's round events.
        if !state
            .soulseek_holds
            .records_for(&track_base_key(&provider, &external_id))
            .is_empty()
        {
            state.heard_samples.shim_event();
        }
    }

    // Octo's apps read the bytes they get, whatever the listing declared, so a song with a file
    // here (fetched ahead, or kept by a heart) plays as that file there, for every play that
    // starts on it, and every other play of theirs keeps the preview it started on. With getting
    // songs ahead off and Wait for lossless on play on, every play takes the path below, which
    // serves a kept file and waits for a copy not kept yet.
    let prefetching = state.settings.current().prefetch.enabled;
    let waits_for_lossless = state.settings.current().subsonic.wait_for_lossless_on_play;
    let octo_app = draws_its_own_marks(&call);
    let owned = octo_app
        && !prefetching
        && !waits_for_lossless
        && state
            .local_library
            .get_local_path_for_external_song(&provider, &external_id)
            .await
            .is_some_and(|path| Path::new(&path).is_file());
    if prefetching || owned {
        // Who is asking only names the play; a lookup that fails leaves it to the address.
        let who = signed_in_user(&state, &call).await.unwrap_or_default();
        note_playing(&state, &call, caller.as_ref(), &id, Some(who.clone()));
        if octo_app {
            let play = OctoPlay {
                caller: caller.as_ref(),
                who: &who,
                address,
                provider: &provider,
                external_id: &external_id,
                id: &id,
                format: &format,
            };
            return match play_in_octo_app(&state, &call, &play).await {
                Ok(response) => Ok(response),
                Err(error) => {
                    error!("Failed to stream track {id}: {error:#}");
                    Ok(json_status(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &json!({ "error": format!("Failed to stream: {error}") }),
                    ))
                }
            };
        }
    }

    // A local file may only be served under an external id when this session DECLARES
    // that id as lossless. search3 already told the client a suffix, bitrate and size,
    // and a player picks its decoder from those, so handing back different bytes is
    // what makes tracks silently refuse to start. With the default settings the
    // lossless copy is reached as its own library track after the rescan instead.
    if state.settings.current().subsonic.wait_for_lossless_on_play {
        let listener = signed_in_user(&state, &call).await.ok();
        let mut local_path = own_version(&state, listener.as_deref(), &provider, &external_id).await;
        // A song fetched ahead is that lossless copy too, while getting songs ahead is on, for a
        // caller whose plays keep copies.
        let mut ahead = false;
        if local_path.is_none()
            && prefetching
            && let Some(ready) = state.prefetch.store().ready(&provider, &external_id)
            && state.prefetch.may_keep_copies(
                &signed_in_user(&state, &call).await.unwrap_or_default(),
                caller.as_ref(),
            )
        {
            local_path = Some(ready.to_string_lossy().into_owned());
            ahead = true;
        }
        if let Some(local_path) = local_path {
            match outside_file(&state, &call, caller.as_ref(), &local_path).await {
                OutsideFile::Serve => {
                    let source = file_source(&state, &provider, &external_id, &local_path, ahead);
                    if ahead {
                        state.prefetch.store().touch(&provider, &external_id);
                    }
                    heard_file(&state, &call, &provider, &external_id, source, &local_path).await;
                    return serve_file(&call, &local_path).await;
                }
                OutsideFile::Relayed(response) => return Ok(response),
                // Not this caller's to have as a file: it plays from its source below.
                OutsideFile::Withheld => {}
            }
        }
    }

    match play_external(
        &state,
        &call,
        caller.as_ref(),
        &provider,
        &external_id,
        &id,
        &format,
    )
    .await
    {
        Ok(response) => Ok(response),
        Err(error) => {
            error!("Failed to stream track {id}: {error:#}");
            Ok(json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": format!("Failed to stream: {error}") }),
            ))
        }
    }
}

/// A play from the first byte tells the songs fetched ahead what is playing, in the background:
/// the stream never waits for it. `who` is the signed-in listener when already known.
fn note_playing(
    state: &AppState,
    call: &SubsonicCall,
    caller: Option<&FamilyCaller>,
    id: &str,
    who: Option<String>,
) {
    if !state.settings.current().prefetch.enabled
        || !is_first_byte_request(call.method().as_str(), call.header("Range").as_deref())
    {
        return;
    }
    let (state, caller, id) = (state.clone(), caller.cloned(), id.to_string());
    let (parameters, proxy) = (call.parameters.clone(), call.proxy.clone());
    tokio::spawn(async move {
        let who = match who {
            Some(who) => Some(who),
            None => state
                .request_identity
                .username(&parameters, &proxy)
                .await
                .ok()
                .flatten(),
        };
        if let Some(who) = who.filter(|w| !w.is_empty()) {
            state.prefetch.on_playing(&who, &id, caller.as_ref()).await;
        }
    });
}

/// While song hearts can go to Lidarr, a play of an outside song from its first byte asks ahead
/// what a heart of it asks MusicBrainz and Deezer first, on their background lanes, so a heart
/// that follows finds those answers kept. The stream never waits for it.
fn warm_up_playing(state: &AppState, external_id: &str) {
    if !lidarr_takes_song_hearts(&state.settings.current()) {
        return;
    }
    let Some(routing) = state
        .external_id_registry
        .resolve_song(external_id, false)
        .map(|routing| routing.snapshot())
        .filter(|routing| routing.kind == RoutingKind::Song)
    else {
        return;
    };
    // The album the song was played or listed from: the play's context, the most recently
    // played first, else the most recently listed.
    let contexts = state.external_id_registry.contexts(external_id);
    let album = contexts
        .iter()
        .filter(|context| context.played_at.is_some())
        .max_by_key(|context| context.played_at)
        .or_else(|| contexts.iter().max_by_key(|context| context.listed_at))
        .map(|context| context.catalog_album_id.clone())
        .or_else(|| routing.external_album_id.clone());
    state.music_brainz.warm_up(
        external_id,
        heart_song(&routing),
        album,
        state.deezer_metadata.clone(),
    );
}

/// Who a first-byte play of an outside song is heard by: the signed-in listener, when there is
/// one and the request starts at the first byte.
async fn heard_by_whom(state: &AppState, call: &SubsonicCall) -> Option<String> {
    if !is_first_byte_request(call.method().as_str(), call.header("Range").as_deref()) {
        return None;
    }
    signed_in_user(state, call)
        .await
        .ok()
        .map(|user| user.trim().to_string())
        .filter(|user| !user.is_empty())
}

/// Which of the listener's album contexts a play marks played: the stream's album when it names
/// one, else the most recent one its client listed. Navidrome's own web UI, and a client that
/// listed the album natively, mark the native context.
fn played_mark(state: &AppState, call: &SubsonicCall, external_id: &str, user: &str) -> PlayedMark {
    let catalog_album_id = call
        .param_opt("albumId")
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let contexts = state.external_id_registry.contexts(external_id);
    let listed_by = |client: &str| {
        contexts.iter().any(|context| {
            eq_ignore_case(context.user.trim(), user)
                && context
                    .client
                    .as_deref()
                    .is_some_and(|listed| eq_ignore_case(listed, client))
        })
    };
    let client = match call.param_opt("c").map(str::trim).filter(|c| !c.is_empty()) {
        Some(client) if eq_ignore_case(client, "NavidromeUI") => Some(NATIVE_CLIENT.to_string()),
        Some(client) if !listed_by(client) && listed_by(NATIVE_CLIENT) => Some(NATIVE_CLIENT.to_string()),
        Some(client) => Some(client.to_string()),
        None if listed_by(NATIVE_CLIENT) => Some(NATIVE_CLIENT.to_string()),
        None => None,
    };
    PlayedMark {
        client,
        catalog_album_id,
    }
}

/// The song's length as the routing knows it.
fn song_seconds(state: &AppState, external_id: &str) -> Option<i32> {
    state
        .external_id_registry
        .resolve_song(external_id, false)
        .and_then(|routing| routing.snapshot().duration)
        .filter(|seconds| *seconds > 0)
}

/// Whose `fpcalc` run a play's sample is: a heart's when the listener holds the song.
fn sample_class(state: &AppState, provider: &str, external_id: &str, user: &str) -> FpcalcClass {
    let holds = state
        .soulseek_holds
        .records_for(&track_base_key(provider, external_id));
    if holds
        .iter()
        .any(|record| record.holders.iter().any(|h| eq_ignore_case(h.user.trim(), user)))
    {
        FpcalcClass::Heart
    } else {
        FpcalcClass::Other
    }
}

/// The song's file here for `user`: the version kept for them, else the one kept for nobody in
/// particular, else the most recently landed one, so a person with no version of their own still
/// plays the song's file.
async fn own_version(
    state: &AppState,
    user: Option<&str>,
    provider: &str,
    external_id: &str,
) -> Option<String> {
    let user = user.filter(|user| !user.trim().is_empty());
    let mine = match user {
        Some(user) => {
            state
                .local_library
                .get_local_path_for_external_song_as(Some(user), provider, external_id)
                .await
        }
        None => None,
    };
    let found = match mine {
        Some(path) => Some(path),
        None => {
            state
                .local_library
                .get_local_path_for_external_song(provider, external_id)
                .await
        }
    };
    found.filter(|path| Path::new(path).is_file())
}

/// Keeps `source` as what `user` heard of the song now, with the played mark, and the id of its
/// sample when its prints are already stored. Answers the record kept.
fn keep_heard(
    state: &AppState,
    call: &SubsonicCall,
    external_id: &str,
    user: &str,
    source: HeardSource,
) -> HeardRecord {
    let sample_id = sample_id_of(&source);
    let stored = state.heard_samples.prints(&sample_id).is_some();
    let record = HeardRecord {
        heard_at: Utc::now(),
        source,
        sample_id: stored.then_some(sample_id),
    };
    state.external_id_registry.remember_heard(
        external_id,
        user,
        record.clone(),
        Some(played_mark(state, call, external_id, user)),
    );
    record
}

/// A file of an outside song served from its first byte: kept as what the listener heard, and
/// printed as their sample unless a whole print of it is stored. None when nobody is heard.
async fn heard_file(
    state: &AppState,
    call: &SubsonicCall,
    provider: &str,
    external_id: &str,
    source: HeardSource,
    path: &str,
) -> Option<HeardRecord> {
    let user = heard_by_whom(state, call).await?;
    let record = keep_heard(state, call, external_id, &user, source);
    let whole = state
        .heard_samples
        .prints(&sample_id_of(&record.source))
        .is_some_and(|prints| prints.whole);
    if !whole {
        state.heard_samples.sample_file(
            std::path::PathBuf::from(path),
            Capture {
                external_id: external_id.to_string(),
                user: user.clone(),
                record: record.clone(),
                limit: 0,
                total: None,
                song_seconds: song_seconds(state, external_id),
                class: sample_class(state, provider, external_id, &user),
            },
        );
    }
    Some(record)
}

/// What a played file of an outside song is: a song fetched ahead is that copy, any other a
/// library file.
fn file_source(state: &AppState, provider: &str, external_id: &str, path: &str, ahead: bool) -> HeardSource {
    if ahead
        && let Some((ready_at, source_file)) = state.prefetch.store().copy_identity(provider, external_id)
    {
        return HeardSource::Copy {
            key: format!("{provider}:{external_id}"),
            ready_at: Some(ready_at),
            source_file,
        };
    }
    HeardSource::Library {
        path: path.to_string(),
    }
}

/// Whether a song heart can go to Lidarr: Lidarr is one of the song hearts' sources, and its
/// address and key are set.
fn lidarr_takes_song_hearts(settings: &AppSettings) -> bool {
    let lidarr = &settings.lidarr;
    lidarr.base_url.as_deref().is_some_and(|url| !is_blank(url))
        && lidarr.api_key.as_deref().is_some_and(|key| !is_blank(key))
        && settings
            .subsonic
            .effective_heart_download_sources()
            .iter()
            .any(|step| step.source == HeartDownloadSource::Lidarr && step.song_enabled == Some(true))
}

/// One play of an outside song in an Octo app, as `stream` read it.
struct OctoPlay<'a> {
    caller: Option<&'a FamilyCaller>,
    who: &'a str,
    address: Option<IpAddr>,
    provider: &'a str,
    external_id: &'a str,
    id: &'a str,
    format: &'a str,
}

/// The keys a play is kept under: its device (the listener, the address and the app), then the
/// address and app alone, which a play started under when the listener could not be named.
fn play_keys(who: &str, address: Option<IpAddr>, client: &str) -> Vec<String> {
    let place = format!(
        "{}|{}",
        address.map(|a| a.to_string()).unwrap_or_default(),
        client.trim().to_lowercase()
    );
    let unnamed = format!("|{place}");
    if who.trim().is_empty() {
        vec![unnamed]
    } else {
        vec![format!("{}|{place}", who.trim().to_lowercase()), unnamed]
    }
}

/// A play of an outside song in an Octo app, while getting songs ahead is on or the song is kept
/// here. A request from the first byte gets the song's FLAC when one is ready for this caller
/// (downloaded ahead, or kept by a heart), and the
/// preview otherwise; a later range gets what its play started on, and a refusal (503) when that
/// file has gone, so the player starts again. The play's own acquisition (DownloadOnPlay,
/// LidarrAlbumOnPlay, the lossless copy of Wait for lossless on play) is started either way, but
/// never waited for here: the play keeps its source.
async fn play_in_octo_app(
    state: &AppState,
    call: &SubsonicCall,
    play: &OctoPlay<'_>,
) -> anyhow::Result<Response> {
    let ahead = state
        .prefetch
        .store()
        .ready(play.provider, play.external_id)
        .filter(|_| state.settings.current().prefetch.enabled)
        .filter(|_| state.prefetch.may_keep_copies(play.who, play.caller))
        .map(|path| path.to_string_lossy().into_owned());
    let ready = match &ahead {
        Some(path) => Some(path.clone()),
        None => own_version(state, Some(play.who), play.provider, play.external_id).await,
    };
    let range = call.header("Range");
    let length = state
        .external_id_registry
        .lookup(play.external_id)
        .and_then(|routing| routing.snapshot().duration);
    let source = state.prefetch.sources().choose(
        &play_keys(play.who, play.address, call.param("c")),
        play.id,
        from_start(range.as_deref()),
        ready.as_deref().map(Path::new),
        length,
        !call.method().as_str().eq_ignore_ascii_case("HEAD"),
    );
    // The play's own acquisition starts once its source is chosen, carrying what it played.
    let (response, heard) = match source {
        Source::File(file) => {
            let file = file.to_string_lossy().into_owned();
            match outside_file(state, call, play.caller, &file).await {
                OutsideFile::Serve => {
                    let fetched_ahead = ahead.as_deref() == Some(file.as_str());
                    let heard_source =
                        file_source(state, play.provider, play.external_id, &file, fetched_ahead);
                    state.prefetch.store().touch(play.provider, play.external_id);
                    info!("Playing {} from its FLAC copy ({file})", play.id);
                    let heard =
                        heard_file(state, call, play.provider, play.external_id, heard_source, &file).await;
                    let response = serve_file(call, &file)
                        .await
                        .unwrap_or_else(IntoResponse::into_response);
                    (response, heard)
                }
                OutsideFile::Relayed(response) => (response, None),
                OutsideFile::Withheld => preview_or_none(state, call, play).await?,
            }
        }
        Source::Gone => {
            info!(
                "The FLAC copy {} started on is gone; the player is asked to start again",
                play.id
            );
            (StatusCode::SERVICE_UNAVAILABLE.into_response(), None)
        }
        Source::Preview => preview_or_none(state, call, play).await?,
    };
    start_play_acquisition(state, call, play, heard).await?;
    Ok(response)
}

/// The preview with what it played, or the 404 Subsonic error when no source has one.
async fn preview_or_none(
    state: &AppState,
    call: &SubsonicCall,
    play: &OctoPlay<'_>,
) -> anyhow::Result<(Response, Option<HeardRecord>)> {
    if let Some(preview) = direct_stream(state, call, play.provider, play.external_id, play.id).await? {
        return Ok(preview);
    }
    warn!("Direct stream not available for {}", play.id);
    let mut response = state
        .subsonic_response_builder
        .create_error(play.format, 70, "No playable source found for this track")
        .into_response();
    *response.status_mut() = StatusCode::NOT_FOUND;
    Ok((response, None))
}

/// What a play starts besides its bytes, as `play_external` starts it: from the first byte the
/// play itself (DownloadOnPlay, LidarrAlbumOnPlay) with what it played, and with Wait for
/// lossless on play the lossless copy, for a caller whose plays keep one. Nothing is waited for.
async fn start_play_acquisition(
    state: &AppState,
    call: &SubsonicCall,
    play: &OctoPlay<'_>,
    heard: Option<HeardRecord>,
) -> anyhow::Result<()> {
    let settings = state.settings.current().subsonic.clone();
    let range = call.header("Range");
    let first_byte = is_first_byte_request(call.method().as_str(), range.as_deref());
    if !first_byte && !settings.wait_for_lossless_on_play {
        return Ok(());
    }
    let who = signed_in_user(state, call).await.map_err(anyhow::Error::new)?;
    let proven_somehow = heard.is_some() || state.acoust_id.is_on();
    if first_byte {
        state.heart_acquisition_coordinator.queue_play_heard(
            play.provider,
            play.external_id,
            requester_for(state, Some(&who)).as_deref(),
            Some(play.id),
            Some(&who),
            heard,
        );
    }
    // With AcoustID off and nothing heard, nothing could prove the lossless copy.
    if settings.wait_for_lossless_on_play
        && proven_somehow
        && let Some(target) = state.family_write_gate.play_target(Some(&who))
    {
        let _ = queue_lossless_play(state, &target, play.provider, play.external_id, &who);
    }
    Ok(())
}

/// The lossless copy a Wait for lossless play asks for, run as a play: its gate is built inside
/// the acquisition, and its completion always resolves.
fn queue_lossless_play(
    state: &AppState,
    target: &crate::services::common::AcquisitionTarget,
    provider: &str,
    external_id: &str,
    who: &str,
) -> Completion {
    state.track_acquisition_queue.enqueue_as(
        Some(AcquisitionOrigin::Play),
        Vec::new(),
        target,
        false,
        provider,
        external_id,
        false,
        false,
        true,
        None,
        true,
        requester_for(state, Some(who)).as_deref(),
        false,
        None,
    )
}

/// HEAD of `stream` or `download`. Players on iPhone send one before playing, for the type and
/// size. Navidrome has never heard of an outside song, so for one of those the GET path runs
/// here and its answer goes back with the body swapped out, which also closes the source. Any
/// other id is Navidrome's to answer. `is_first_byte_request` never counts a HEAD as a play.
async fn audio_head(State(state): State<AppState>, req: Request) -> AppResult {
    let (parts, body) = req.into_parts();
    let id = SubsonicCall::from_parts(&state, &parts, Default::default())
        .param("id")
        .to_string();
    let req = Request::from_parts(parts, body);
    if !state.local_library.parse_song_id(&id).0 {
        return Ok(catch_all::catch_all(State(state), req).await);
    }
    let mut reply = stream(State(state), req).await?;
    // An answer that names no length (a stand-in not built yet) keeps naming none: an empty body
    // of unknown size, so no length of 0 is made up for it.
    *reply.body_mut() = if reply.headers().contains_key(header::CONTENT_LENGTH) {
        Body::empty()
    } else {
        Body::from_stream(futures::stream::empty::<Result<bytes::Bytes, std::io::Error>>())
    };
    Ok(reply)
}

// ---------------------------------------------------------------------------------------
// download
// ---------------------------------------------------------------------------------------

/// An outside song downloads exactly as it streams: the bytes Octo relays, or the copy it
/// waits for, are the only file it has. A library song goes to Navidrome's own `download`
/// through the faithful relay.
pub async fn download(State(state): State<AppState>, req: Request) -> AppResult {
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
        return Ok(StatusCode::PAYLOAD_TOO_LARGE.into_response());
    };
    let call = SubsonicCall::from_parts(&state, &parts, body.clone());
    let outside = state.local_library.parse_song_id(call.param("id")).0;
    let req = Request::from_parts(parts, Body::from(body));
    if outside {
        stream(State(state), req).await
    } else {
        Ok(catch_all::catch_all(State(state), req).await)
    }
}

/// Queues the play, waits for a lossless copy when asked to, else serves the preview. An error
/// here is the 500 `{"error":"Failed to stream: ..."}`. (A client that hangs up drops this
/// future, and gets nothing.)
async fn play_external(
    state: &AppState,
    call: &SubsonicCall,
    caller: Option<&FamilyCaller>,
    provider: &str,
    external_id: &str,
    id: &str,
    format: &str,
) -> anyhow::Result<Response> {
    let settings = state.settings.current().subsonic.clone();
    // Lossless-on-play remains an explicit opt-in. Normal playback starts no
    // acquisition unless DownloadOnPlay or LidarrAlbumOnPlay are on: owned ids
    // already went to Navidrome above, and missing ids stream from YouTube below.
    // Hearts are the normal permanent-copy gesture.
    // Only a request from the first byte is a play. Clients ask again with a later
    // Range on every seek and while buffering. A transcoded request (format,
    // maxBitRate) is still a play and counts.
    let range = call.header("Range");
    let first_byte = is_first_byte_request(call.method().as_str(), range.as_deref());
    // The play is queued once its source is chosen, with what that source played.
    let queue_play = |heard: Option<HeardRecord>| async move {
        if !first_byte {
            return Ok::<(), anyhow::Error>(());
        }
        let who = signed_in_user(state, call).await.map_err(anyhow::Error::new)?;
        state.heart_acquisition_coordinator.queue_play_heard(
            provider,
            external_id,
            requester_for(state, Some(&who)).as_deref(),
            Some(id),
            Some(&who),
            heard,
        );
        Ok(())
    };
    if settings.wait_for_lossless_on_play {
        let who = signed_in_user(state, call).await.map_err(anyhow::Error::new)?;
        // A family member's play keeps a copy only when they may add songs at once, and then
        // in their own library; otherwise it plays from the source below.
        if let Some(target) = state.family_write_gate.play_target(Some(&who)) {
            // With AcoustID off nothing could prove a lossless copy: the wait is skipped and the
            // stand-in plays.
            let acquisition = state
                .acoust_id
                .is_on()
                .then(|| queue_lossless_play(state, &target, provider, external_id, &who));
            let (response, heard) = serve_acquired(
                state,
                call,
                caller,
                acquisition,
                provider,
                external_id,
                id,
                format,
                true,
            )
            .await?;
            queue_play(heard).await?;
            return Ok(response);
        }
    }

    if let Some((direct, heard)) = direct_stream(state, call, provider, external_id, id).await? {
        queue_play(heard).await?;
        return Ok(direct);
    }

    // No playable source: the play is still queued, with nothing heard.
    queue_play(None).await?;
    warn!("Direct stream not available for {id}");
    // A 404 with the Subsonic error body: a player that only reads the status still sees the
    // play failed, instead of trying to decode an error envelope as audio.
    let mut response = state
        .subsonic_response_builder
        .create_error(format, 70, "No playable source found for this track")
        .into_response();
    *response.status_mut() = StatusCode::NOT_FOUND;
    Ok(response)
}

/// Wait for a queued acquisition and serve the file, with what the play heard. With no
/// acquisition (AcoustID is off, so nothing could prove a lossless copy) the wait is skipped.
///
/// Wherever the play would serve nothing lossless (no acquisition, a copy the gate could not
/// prove, a bounded wait that ran out) the preview stands in as a whole FLAC file, so the
/// listing's promise holds and no lossy bytes go out under it.
///
/// Abandoning the WAIT leaves the transfer running, so a client that gives up costs it
/// nothing.
#[allow(clippy::too_many_arguments)]
async fn serve_acquired(
    state: &AppState,
    call: &SubsonicCall,
    caller: Option<&FamilyCaller>,
    acquisition: Option<Completion>,
    provider: &str,
    external_id: &str,
    id: &str,
    format: &str,
    allow_preview_fallback: bool,
) -> anyhow::Result<(Response, Option<HeardRecord>)> {
    let Some(acquisition) = acquisition else {
        info!("AcoustID is off, so no lossless copy of {id} can be proven; the FLAC stand-in plays");
        return match flac_stand_in(state, call, provider, external_id).await? {
            Some(served) => Ok(served),
            None => Ok((
                state
                    .subsonic_response_builder
                    .create_error(format, 70, "No playable source found for this track")
                    .into_response(),
                None,
            )),
        };
    };
    // Above 0, the wait is bounded and the preview stands in while the fetch keeps
    // running in the background; the next play of this id serves the landed file.
    let timeout = state
        .settings
        .current()
        .subsonic
        .lossless_wait_timeout_seconds
        .max(0);
    let fallback = allow_preview_fallback && timeout > 0;

    let outcome = if fallback {
        match tokio::time::timeout(Duration::from_secs(timeout as u64), acquisition.wait()).await {
            Ok(outcome) => outcome.map_err(|e| (e.to_string(), false, not_proven(&e))),
            Err(_) => Err(("The operation has timed out.".to_string(), true, false)),
        }
    } else {
        acquisition
            .wait()
            .await
            .map_err(|e| (e.to_string(), false, not_proven(&e)))
    };

    let path = match outcome {
        Ok(path) => path,
        Err((message, timed_out, unproven)) => {
            if fallback || unproven {
                let reason = if timed_out {
                    format!("timeout {timeout}s")
                } else {
                    message.clone()
                };
                info!(
                    "Lossless wait ended early for {id} ({reason}); the FLAC stand-in plays while the fetch continues"
                );
                if let Some(served) = flac_stand_in(state, call, provider, external_id).await? {
                    return Ok(served);
                }
                // The user opted into a bounded wait, trading the declared-lossless contract for
                // playback that starts; timeout 0 keeps the contract exact.
                if fallback
                    && let Some(preview) = direct_stream(state, call, provider, external_id, id).await?
                {
                    return Ok(preview);
                }
            } else {
                // Never fall back to the lossy stream here. This session declared the id
                // lossless, so lossy bytes would be the same contract violation in reverse.
                warn!("Lossless acquisition failed for {id}: {message}");
            }
            return Ok((
                state
                    .subsonic_response_builder
                    .create_error(format, 70, &format!("Could not fetch a lossless copy: {message}"))
                    .into_response(),
                None,
            ));
        }
    };

    if !Path::new(&path).is_file() {
        return Ok((
            state
                .subsonic_response_builder
                .create_error(format, 70, "Lossless copy is no longer on disk")
                .into_response(),
            None,
        ));
    }
    match outside_file(state, call, caller, &path).await {
        OutsideFile::Serve => {
            let heard = heard_file(
                state,
                call,
                provider,
                external_id,
                HeardSource::Acquired { path: path.clone() },
                &path,
            )
            .await;
            Ok((serve_file(call, &path).await.map_err(anyhow::Error::new)?, heard))
        }
        OutsideFile::Relayed(response) => Ok((response, None)),
        OutsideFile::Withheld => {
            if let Some(preview) = direct_stream(state, call, provider, external_id, id).await? {
                return Ok(preview);
            }
            Ok((
                state
                    .subsonic_response_builder
                    .create_error(format, 70, "No playable source found for this track")
                    .into_response(),
                None,
            ))
        }
    }
}

/// Whether an acquisition ended because the gate could not prove the copy is what played.
fn not_proven(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PlayOutcome>() == Some(&PlayOutcome::NotProven)
}

/// The song's preview as a whole FLAC file: its whole copy converted once into
/// `<preview copies>/flac/<video id>.flac` and served as a file, so a `Range` read gets its real
/// length and a seek works. A HEAD reads the built file's headers, or before it is built gets the
/// type alone and builds nothing; a play from the first byte builds it and is heard from that
/// video. None when there is no video or the file could not be made.
async fn flac_stand_in(
    state: &AppState,
    call: &SubsonicCall,
    provider: &str,
    external_id: &str,
) -> anyhow::Result<Option<(Response, Option<HeardRecord>)>> {
    let pinned = state
        .external_id_registry
        .resolve_song(external_id, false)
        .and_then(|routing| routing.snapshot().you_tube_id)
        .filter(|video| !video.trim().is_empty());
    if call.method().as_str().eq_ignore_ascii_case("HEAD") {
        if let Some(built) = pinned
            .as_deref()
            .and_then(|video| state.preview_copies.built_flac(video))
        {
            return Ok(Some((serve_file(call, &built.to_string_lossy()).await?, None)));
        }
        let mut response = Response::new(Body::empty());
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/flac"));
        response
            .headers_mut()
            .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        return Ok(Some((response, None)));
    }
    let confirmed = state
        .external_id_registry
        .stream_facts(external_id)
        .filter(|facts| facts.size_confirmed)
        .and_then(|facts| facts.size);
    let (video, total) = match pinned {
        Some(video) => (video, confirmed),
        None => {
            // Nothing pinned yet: the stream a play would get names the video.
            let probe = state
                .download_service
                .get_direct_stream(
                    provider,
                    external_id,
                    Some("bytes=0-1"),
                    &CancellationToken::new(),
                )
                .await?;
            let Some(probe) = probe else {
                return Ok(None);
            };
            let total = played_size(
                probe.status_code,
                probe.content_length,
                probe.content_range.as_deref(),
            );
            let Some(video) = probe.video_id else {
                return Ok(None);
            };
            (video, total)
        }
    };
    let Some(flac) = state.preview_copies.flac_stand_in(&video, total).await else {
        warn!("No FLAC stand-in could be made of {video}");
        return Ok(None);
    };
    let mut heard = None;
    if let Some(user) = heard_by_whom(state, call).await
        && let Some((size, content_type)) = state.preview_copies.copy_facts(&video)
    {
        let record = keep_heard(
            state,
            call,
            external_id,
            &user,
            HeardSource::Video {
                id: video.clone(),
                size: Some(size),
                content_type,
                itag: None,
            },
        );
        if let Some(copy) = state.preview_copies.copy_path_of(&video, Some(size), None) {
            state.heard_samples.sample_file(
                copy,
                Capture {
                    external_id: external_id.to_string(),
                    user: user.clone(),
                    record: record.clone(),
                    limit: 0,
                    total: Some(size),
                    song_seconds: song_seconds(state, external_id),
                    class: sample_class(state, provider, external_id, &user),
                },
            );
        }
        heard = Some(record);
    }
    let response = serve_file(call, &flac.to_string_lossy()).await?;
    Ok(Some((response, heard)))
}

/// What Octo does with a file it holds for an outside song, for one caller.
enum OutsideFile {
    /// Send the file, as with Family off.
    Serve,
    /// The library copy, relayed from Navidrome with the caller's quality.
    Relayed(Response),
    /// Not for this caller as a file: play the song from its source instead.
    Withheld,
}

/// Whether to send a file Octo holds for an outside song. With Family on, a file in another
/// member's own library (or the family pool) is never sent, and when the caller's audio has a
/// ceiling the library copy is relayed with it instead, so no file goes out above it. A file
/// the library index does not know yet cannot be relayed, so it is withheld then.
async fn outside_file(
    state: &AppState,
    call: &SubsonicCall,
    caller: Option<&FamilyCaller>,
    path: &str,
) -> OutsideFile {
    let Some(caller) = caller else {
        return OutsideFile::Serve;
    };
    let full = navidrome_song_list::full_path(path);
    let song = state.library_index.find_by_path(&full);
    let scope = if caller.managed {
        LibraryScope::Viewer(caller.user.clone())
    } else {
        LibraryScope::Shared
    };
    if !state
        .library_scope
        .allows(&scope, song.as_ref().map(|s| s.id.as_str()), Some(&full))
    {
        return OutsideFile::Withheld;
    }
    if audio_gate::ceiling(caller).is_none() {
        return OutsideFile::Serve;
    }
    let Some(song) = song else {
        return OutsideFile::Withheld;
    };
    let mut parameters = call.parameters.clone();
    parameters.insert("id".into(), song.id);
    audio_gate::rewrite_parameters(caller, "stream", &mut parameters);
    OutsideFile::Relayed(call.proxy.relay_stream(&parameters).await)
}

/// Proxy the lossy preview straight from the CDN, with what it played: a play from the first
/// byte keeps the video as what the listener heard and takes its sample from the bytes on their
/// way. `None` when no source resolved.
async fn direct_stream(
    state: &AppState,
    call: &SubsonicCall,
    provider: &str,
    external_id: &str,
    id: &str,
) -> anyhow::Result<Option<(Response, Option<HeardRecord>)>> {
    // Forward the client's Range header up the chain so the shim can
    // ask googlevideo for the requested byte range and we can return
    // a proper 206. iOS Subsonic clients refuse to play non-FLAC
    // audio without working byte-range support: our prior 200/none
    // response was what was making Arpeggi/Narjo silently drop
    // every external song from the queue.
    let range_header = call.header("Range");

    // A range that starts past the end of a stream whose size a play confirmed is refused here,
    // the way a file would refuse it, so a player probing the end learns the length without a
    // relay. A size a lookup only reported is not refused on.
    if let Some(size) = state
        .external_id_registry
        .stream_facts(external_id)
        .filter(|facts| facts.size_confirmed)
        .and_then(|facts| facts.size)
        && let Some(range) = range_header.as_deref()
        && matches!(parse_range(range, size), RangeOutcome::Unsatisfiable)
    {
        return Ok(Some((range_not_satisfiable(size), None)));
    }

    let Some(direct) = state
        .download_service
        .get_direct_stream(
            provider,
            external_id,
            range_header.as_deref(),
            &CancellationToken::new(),
        )
        .await?
    else {
        return Ok(None);
    };

    info!(
        "Direct streaming track {id} ({}, status={})",
        direct.quality.as_deref().unwrap_or(""),
        direct.status_code
    );
    // The shim measures a played song's loudness; ask for it once it has had time to.
    let played = matches!(direct.status_code, 200 | 206);
    if played && is_first_byte_request(call.method().as_str(), range_header.as_deref()) {
        state.stream_size_warmer.after_play(external_id);
    }

    // What played, for the listener: the video and its sample, taken from the bytes on their way.
    let mut body = direct.audio_stream;
    let mut heard = None;
    if played
        && let Some(video) = direct.video_id.clone()
        && let Some(user) = heard_by_whom(state, call).await
    {
        let size = played_size(
            direct.status_code,
            direct.content_length,
            direct.content_range.as_deref(),
        );
        let source = HeardSource::Video {
            id: video,
            size,
            content_type: direct.content_type.clone(),
            itag: direct.itag,
        };
        let record = keep_heard(state, call, external_id, &user, source);
        let song = song_seconds(state, external_id);
        let extension = if direct.content_type.contains("webm") {
            "webm"
        } else {
            "m4a"
        };
        body = state.heard_samples.capture(
            body,
            Capture {
                external_id: external_id.to_string(),
                user: user.clone(),
                record: record.clone(),
                limit: sample_cap(size, song),
                total: size,
                song_seconds: song,
                class: sample_class(state, provider, external_id, &user),
            },
            extension,
        );
        state.heard_samples.stream_ok();
        heard = Some(record);
    }

    // Manual stream copy: the network stream isn't seekable, so the upstream's status code +
    // Content-Range are forwarded verbatim and the bytes copied to the response body.
    let mut response = Response::new(Body::from_stream(body));
    *response.status_mut() = StatusCode::from_u16(direct.status_code).unwrap_or(StatusCode::OK);
    let headers = response.headers_mut();
    if let Some(length) = direct.content_length {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
    if let Ok(content_type) = HeaderValue::from_str(&direct.content_type) {
        headers.insert(header::CONTENT_TYPE, content_type);
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(range) = direct.content_range.filter(|r| !r.is_empty())
        && let Ok(range) = HeaderValue::from_str(&range)
    {
        headers.insert(header::CONTENT_RANGE, range);
    }
    Ok(Some((response, heard)))
}

/// 416 with `Content-Range: bytes */<size>`, as a file past its end answers.
fn range_not_satisfiable(size: u64) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    let headers = response.headers_mut();
    if let Ok(range) = HeaderValue::from_str(&format!("bytes */{size}")) {
        headers.insert(header::CONTENT_RANGE, range);
    }
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

/// The file, typed by its extension, with single-range support and no ETag or Last-Modified.
async fn serve_file(call: &SubsonicCall, path: &str) -> Result<Response, AppError> {
    let content_type = content_type_for_file(path);
    let mut file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();

    // An If-Range the file cannot be checked against (it has no validators) sends it whole.
    let range = call
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|_| !call.headers().contains_key(header::IF_RANGE))
        .map(|v| parse_range(v, length))
        .unwrap_or(RangeOutcome::Whole);

    let (status, from, count, content_range) = match range {
        RangeOutcome::Whole => (StatusCode::OK, 0, length, None),
        RangeOutcome::Partial(from, to) => (
            StatusCode::PARTIAL_CONTENT,
            from,
            to - from + 1,
            Some(format!("bytes {from}-{to}/{length}")),
        ),
        RangeOutcome::Unsatisfiable => return Ok(range_not_satisfiable(length)),
    };

    if from > 0 {
        file.seek(SeekFrom::Start(from)).await?;
    }
    let body = Body::from_stream(ReaderStream::new(file.take(count)));
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(range) = content_range.and_then(|r| HeaderValue::from_str(&r).ok()) {
        headers.insert(header::CONTENT_RANGE, range);
    }
    Ok(response)
}

/// `GetContentType`: the audio type of a file by its extension, `audio/mpeg` by default.
fn content_type_for_file(path: &str) -> &'static str {
    let extension = Path::new(path)
        .extension()
        .map(|e| dotnet::to_lower_invariant(&e.to_string_lossy()))
        .unwrap_or_default();
    match extension.as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "aac" => "audio/aac",
        _ => "audio/mpeg",
    }
}

// ---------------------------------------------------------------------------------------
// getCoverArt
// ---------------------------------------------------------------------------------------

/// Proxies external covers. Uses type from ID to determine which API to call.
/// Format: ext-{provider}-{type}-{id} (e.g., ext-deezer-artist-259, ext-deezer-album-96126)
pub async fn get_cover_art(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = without_navidrome_kind(&state, call.param("id"));

    if is_blank(&id) {
        return Ok(problem(StatusCode::NOT_FOUND));
    }

    // Stable, cached Octo-branded station artwork. Deliberately static: playlist
    // requests never perform a live cover mosaic build.
    if id.eq_ignore_ascii_case("octo-radio") {
        return Ok(serve_placeholder(&state, true).await);
    }

    // Review and Duplicates: Octo's painted cover with the notice's glyph, picked by the
    // notice's title. getPlaylists hands these ids out for the listener's notice playlists.
    if let Some(kind) = id
        .strip_prefix(super::playlists::NOTICE_COVER_PREFIX)
        .and_then(super::playlists::notice_kind)
    {
        let title = state.settings.current().library_actions.notice_title(kind);
        let glyph = match kind {
            NoticeKind::Review => CoverGlyph::Magnifier,
            NoticeKind::Duplicates => CoverGlyph::Copies,
        };
        let service = Arc::clone(&state.cover_art_service);
        let size = requested_cover_size(&call);
        let bytes = tokio::task::spawn_blocking(move || {
            service.get_glyph_cover(&title, list_kinds::NOTICE, glyph, size)
        })
        .await
        .unwrap_or_default();
        if bytes.is_empty() {
            return Ok(serve_placeholder(&state, false).await);
        }
        return Ok(file(bytes, "image/jpeg"));
    }

    // Generated radio IDs already resolve through the per-user state store,
    // so station artwork follows the current station name without creating
    // a parallel metadata record or exposing that name in the cover ID.
    let owner = cover_owner(&state, &call, &id).await;
    if let Some(station) = state.last_fm_radio_state.find_station(&owner, &id) {
        let seeds = station_cover_seeds(&state, &station);
        let list = ListCover {
            name: station.name.clone(),
            label: station_cover_label(&station),
            kind: list_kinds::RADIO.to_string(),
            seeds: Some(Arc::new(move || {
                let seeds = seeds.clone();
                async move { Ok(seeds) }.boxed()
            })),
            song_count: Some(i32::try_from(station.tracks.len()).unwrap_or(i32::MAX)),
            cover_title: None,
            glyph: None,
        };
        let bytes = state
            .cover_art_service
            .get_list_cover(&list, requested_cover_size(&call))
            .await;
        return Ok(file(bytes, "image/jpeg"));
    }

    // A made list's cover id (Liked Songs, a mix or a chart list) is its list id, then `_` and
    // a tag naming the picture as drawn.
    let list_id = match id.split_once('_') {
        Some((list, _)) if list.starts_with("og") => list,
        _ => id.as_str(),
    };

    // Liked Songs: Octo's painted cover with a heart, picked by its name.
    let cover_user = owner.clone();
    if !cover_user.is_empty() && list_id == state.generated_playlists.liked_id(&cover_user) {
        let name = state
            .settings
            .current()
            .generated_playlists
            .effective_liked_songs_name();
        let service = Arc::clone(&state.cover_art_service);
        let size = requested_cover_size(&call);
        let bytes = tokio::task::spawn_blocking(move || {
            service.get_glyph_cover(&name, list_kinds::LIKED, CoverGlyph::Heart, size)
        })
        .await
        .unwrap_or_default();
        if bytes.is_empty() {
            return Ok(serve_placeholder(&state, false).await);
        }
        return Ok(file(bytes, "image/jpeg"));
    }

    // A mix is the listener's own library, so its cover carries no Octo mark: the logo says
    // where a result came from, and this came from them.
    if let Some(mix) = state.generated_playlists.find(&cover_user, list_id) {
        let song_count = state
            .generated_playlists
            .drawn(&cover_user, &mix)
            .map(|songs| i32::try_from(songs.len()).unwrap_or(i32::MAX));
        let seeds_state = state.clone();
        let seeds_proxy = call.proxy.clone();
        let parameters = call.parameters.clone();
        let seeds_mix = mix.clone();
        let list = ListCover {
            name: mix.name.clone(),
            label: Some(mix.label.clone()),
            kind: list_kinds::MIX.to_string(),
            seeds: Some(Arc::new(move || {
                let state = seeds_state.clone();
                let proxy = seeds_proxy.clone();
                let parameters = parameters.clone();
                let mix = seeds_mix.clone();
                let user = cover_user.clone();
                async move { Ok(mix_cover_seeds(&state, &proxy, &user, &mix, &parameters).await) }.boxed()
            })),
            song_count,
            cover_title: None,
            glyph: None,
        };
        let bytes = state
            .cover_art_service
            .get_list_cover(&list, requested_cover_size(&call))
            .await;
        return Ok(file(bytes, "image/jpeg"));
    }

    // A chart list's cover, drawn like a mix's from its first songs' covers. It is only ever
    // drawn from a list already made: drawing it never makes one.
    if let Some(chart) = state.chart_playlists.cover(&owner, list_id) {
        let song_count = Some(i32::try_from(chart.entries.len()).unwrap_or(i32::MAX));
        let kind = match octo_core::charts::chart_settings::kind(&chart.chart) {
            Some(octo_core::charts::chart_settings::ChartKind::NewSongs) => list_kinds::NEW_SONGS,
            Some(octo_core::charts::chart_settings::ChartKind::Trending) => list_kinds::TRENDING,
            _ => list_kinds::CHART,
        };
        let seeds = chart_cover_seeds(&state, &call.proxy, &chart, &call.parameters);
        let list = ListCover {
            name: chart.name.clone(),
            label: None,
            kind: kind.to_string(),
            seeds: Some(Arc::new(move || {
                let seeds = seeds.clone();
                async move { Ok(seeds) }.boxed()
            })),
            song_count,
            cover_title: chart_cover_words(&chart.chart, chart.country.as_deref(), &chart.name),
            glyph: None,
        };
        let bytes = state
            .cover_art_service
            .get_list_cover(&list, requested_cover_size(&call))
            .await;
        return Ok(file(bytes, "image/jpeg"));
    }

    // Playlist covers haven't changed: keep the existing path.
    if playlist_id_helper::is_external_playlist(Some(&id)) {
        return Ok(match external_playlist_cover(&state, &id).await {
            Ok(Some(response)) => response,
            Ok(None) => serve_placeholder(&state, true).await,
            Err(e) => {
                error!("Error getting playlist cover art for {id}: {e:#}");
                serve_placeholder(&state, true).await
            }
        });
    }

    // Registry-backed id (song / album / artist). Resolve to artist+title via the
    // registry and look the cover up on iTunes. Watermark with the Octo logo so
    // radio-sourced art is visually distinct from local-library art.
    // The Octo app marks songs outside the library itself, so its covers come back
    // plain, and a missing one is a 404 it draws its own empty tile for. Every other
    // client keeps the badge, the only sign it gets that a song came from Octo's search.
    // With the badge turned off, every client gets the plain picture; a missing cover is still
    // the drawn placeholder for apps that do not draw their own.
    let plain = draws_its_own_marks(&call);
    let unbadged = plain || !state.settings.current().subsonic.cover_badge;
    let badged_key = state.cover_art_service.badged_key(&id);
    if let Some(routing) = state.external_id_registry.lookup(&id) {
        if !unbadged && let Some(kept) = state.cover_art_service.kept_badged(&badged_key) {
            return Ok(file(kept, "image/jpeg"));
        }
        let routing = routing.snapshot();
        let raw = state.cover_art_aggregator.get_cover(&routing, false).await;
        let Some(raw) = raw else {
            debug!(
                "cover art all-source miss for {:?} '{} - {}/{}', serving placeholder",
                routing.kind,
                routing.artist.as_deref().unwrap_or(""),
                routing.title.as_deref().unwrap_or(""),
                routing.album.as_deref().unwrap_or("")
            );
            return Ok(missing_cover(&state, plain).await);
        };
        let watermarked = if unbadged {
            raw.to_vec()
        } else {
            match badge(&state, raw.to_vec()).await {
                Some(badged) => {
                    state.cover_art_service.keep_badged(badged_key, &badged);
                    badged
                }
                None => raw.to_vec(),
            }
        };
        return Ok(file(watermarked, "image/jpeg"));
    }

    // Legacy "ext-album-{hash}" / "ext-artist-{hash}" ids that pre-date the
    // registry. We can't reverse-resolve them, but returning a 404 makes
    // Arpeggio drop the song, so serve the Octo placeholder instead.
    if dotnet::starts_with_ignore_case(&id, "ext-album-")
        || dotnet::starts_with_ignore_case(&id, "ext-artist-")
    {
        return Ok(missing_cover(&state, plain).await);
    }

    // Existing ext-{provider}-{type}-{id} path (Deezer/Tidal-era, kept for
    // compatibility with any in-flight clients).
    let (is_external, cover_provider, kind, cover_external_id) = state.local_library.parse_external_id(&id);
    if is_external {
        if !unbadged && let Some(kept) = state.cover_art_service.kept_badged(&badged_key) {
            return Ok(file(kept, "image/jpeg"));
        }
        let provider = cover_provider.unwrap_or_default();
        let external_id = cover_external_id.unwrap_or_default();
        let metadata = &state.music_metadata;
        let cover_url = match kind.as_deref() {
            Some("artist") => metadata
                .get_artist(&provider, &external_id)
                .await
                .and_then(|a| a.image_url),
            Some("album") => metadata
                .get_album(&provider, &external_id)
                .await
                .and_then(|a| a.cover_art_url),
            _ => match metadata
                .get_song(&provider, &external_id)
                .await
                .and_then(|s| s.cover_art_url)
            {
                Some(url) => Some(url),
                None => metadata
                    .get_album(&provider, &external_id)
                    .await
                    .and_then(|a| a.cover_art_url),
            },
        };

        if let Some(cover_url) = cover_url {
            // Not caught: a failed fetch reached the global handler.
            let response = state.http.get(&cover_url).send().await?;
            if response.status().is_success() {
                let image_bytes = response.bytes().await?.to_vec();
                let watermarked = if unbadged {
                    image_bytes
                } else {
                    match badge(&state, image_bytes.clone()).await {
                        Some(badged) => {
                            state.cover_art_service.keep_badged(badged_key, &badged);
                            badged
                        }
                        None => image_bytes,
                    }
                };
                return Ok(file(watermarked, "image/jpeg"));
            }
        }
        return Ok(missing_cover(&state, plain).await);
    }

    // Local library: from the cover cache when it can answer, else proxied to Navidrome
    // unchanged.
    match cached_library_cover(&state, &call, &id).await {
        Ok(picture) => return Ok(file(picture.bytes.as_ref().clone(), picture.content_type)),
        Err(NotKept::Unreachable) => return Ok(serve_placeholder(&state, false).await),
        Err(NotKept::Ask) => {}
    }
    match call.proxy.relay("rest/getCoverArt", &call.parameters).await {
        Ok(result) => Ok(file(
            result.body,
            result.content_type.as_deref().unwrap_or("image/jpeg"),
        )),
        Err(e) => {
            // Unbranded on purpose. This is the user's own file; stamping the Octo
            // logo on it makes Octo look like it is claiming a track the user
            // already owned. Reading embedded art off a cloud-backed mount can take
            // seconds cold, so this path is reached by ordinary slowness, not just
            // by missing art: all the more reason not to brand it.
            debug!("cover art relay failed for local id {id}: {e}");
            Ok(serve_placeholder(&state, false).await)
        }
    }
}

/// Navidrome's own web UI writes the kind before every cover id itself (`mf-` song, `al-`
/// album, `ar-` artist, `pl-` playlist), whatever id the answer carried. An outside id under
/// one of them is looked up bare; any other id keeps its prefix, which Navidrome reads.
fn without_navidrome_kind(state: &AppState, id: &str) -> String {
    const KINDS: [&str; 4] = ["mf-", "al-", "ar-", "pl-"];
    KINDS
        .iter()
        .find_map(|kind| id.strip_prefix(kind))
        .filter(|bare| {
            state.external_id_registry.lookup(bare).is_some()
                || playlist_id_helper::is_external_playlist(Some(bare))
                || dotnet::starts_with_ignore_case(bare, "ext-")
        })
        .unwrap_or(id)
        .to_string()
}

/// Whose generated covers a cover request asks for: `u` when sent, else, for a station or
/// made-list id only, the API key's owner once Navidrome accepts the key. A refused or unsigned
/// request never asks for the key's owner, and any other id has no owner.
async fn cover_owner(state: &AppState, call: &SubsonicCall, id: &str) -> String {
    let named = call.param("u");
    if !named.is_empty() {
        return named.to_string();
    }
    if !(id.starts_with("or") || id.starts_with("og")) || call.param("apiKey").is_empty() {
        return String::new();
    }
    let credential = SubsonicCredential::from(&call.parameters);
    match state
        .credential_check
        .check(credential.as_ref(), &call.proxy)
        .await
    {
        Ok(CredentialVerdict::Accepted) => super::caller_checks::listener_name(state, call).await,
        _ => String::new(),
    }
}

/// A library cover from the cover cache, for a request with a size and no `square`, from a
/// caller Navidrome accepts. The sign-in is checked here because a kept cover never reaches
/// Navidrome, which would otherwise be the one to check it. Anything else, a refused or
/// unchecked sign-in included, is `NotKept::Ask`: Navidrome answers it as before.
async fn cached_library_cover(
    state: &AppState,
    call: &SubsonicCall,
    id: &str,
) -> Result<CoverPicture, NotKept> {
    let cache = &state.local_cover_cache;
    let Some(size) = requested_cover_size(call).and_then(|size| u32::try_from(size).ok()) else {
        return Err(NotKept::Ask);
    };
    if !cache.is_on() || super::caller_checks::is_true(call.param("square")) {
        return Err(NotKept::Ask);
    }
    let credential = SubsonicCredential::from(&call.parameters);
    match state
        .credential_check
        .check(credential.as_ref(), &call.proxy)
        .await
    {
        Ok(CredentialVerdict::Accepted) => {}
        _ => return Err(NotKept::Ask),
    }
    let proxy = call.proxy.clone();
    let parameters = call.parameters.clone();
    cache
        .cover(id, size, move |side| fetch_library_cover(proxy, parameters, side))
        .await
}

/// A library cover from Navidrome at `side`, as the cover cache keeps it.
pub(crate) async fn fetch_library_cover(
    proxy: SubsonicProxyService,
    mut parameters: Parameters,
    side: u32,
) -> Fetched {
    parameters.insert("size".into(), side.to_string());
    match proxy.relay_dated_image("rest/getCoverArt", &parameters).await {
        Ok(Some(bytes)) => Fetched::Picture(bytes.to_vec()),
        Ok(None) => Fetched::Other,
        Err(e) => {
            debug!("cover fetch for the cover cache failed: {e}");
            Fetched::Unreachable
        }
    }
}

/// A `pl-{provider}-{id}` playlist's own picture, `None` for the placeholder.
async fn external_playlist_cover(state: &AppState, id: &str) -> anyhow::Result<Option<Response>> {
    let (provider, external_id) = playlist_id_helper::parse_playlist_id(id)?;
    let Some(playlist) = state.music_metadata.get_playlist(&provider, &external_id).await else {
        return Ok(None);
    };
    let Some(cover_url) = playlist.cover_url.filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let image = state.http.get(&cover_url).send().await?;
    if !image.status().is_success() {
        return Ok(None);
    }
    let content_type = content_type_of(image.headers()).unwrap_or_else(|| "image/jpeg".to_string());
    let bytes = image.bytes().await?;
    Ok(Some(file(bytes.to_vec(), &content_type)))
}

/// What a cover that could not be found answers: a 404 for the Octo app, which draws its own
/// tile, and the branded placeholder for everyone else.
async fn missing_cover(state: &AppState, plain: bool) -> Response {
    if plain {
        problem(StatusCode::NOT_FOUND)
    } else {
        serve_placeholder(state, true).await
    }
}

/// `_coverArtService.AddOctoBadge(raw)`, off the async threads. None when the badge could not
/// be drawn: the caller sends the picture as it came, and does not keep it.
async fn badge(state: &AppState, raw: Vec<u8>) -> Option<Vec<u8>> {
    let service = Arc::clone(&state.cover_art_service);
    tokio::task::spawn_blocking(move || service.try_add_octo_badge(&raw))
        .await
        .ok()
        .flatten()
}

/// The words a chart's cover shows in place of its name, when they differ: Popular right now
/// says "Popular".
pub(crate) fn chart_cover_title(chart: &str) -> Option<&'static str> {
    matches!(
        octo_core::charts::chart_settings::kind(chart),
        Some(octo_core::charts::chart_settings::ChartKind::Overall)
    )
    .then_some(octo_core::charts::chart_settings::OVERALL_COVER_TITLE)
}

/// The words a chart list's cover shows in place of its name, when they differ: Popular right
/// now says "Popular", and a pinned country's list adds its country ("Popular · Japan", or the
/// list's own name, "Top Pop · Japan"). The server's covers and the Octo apps' say the same.
pub(crate) fn chart_cover_words(chart: &str, pinned: Option<&str>, name: &str) -> Option<String> {
    match pinned {
        None => chart_cover_title(chart).map(str::to_string),
        Some(country) => Some(match chart_cover_title(chart) {
            Some(words) => octo_core::charts::chart_settings::pinned_name(words, country),
            None => name.to_string(),
        }),
    }
}

/// The size a client asked a cover at, if it asked.
fn requested_cover_size(call: &SubsonicCall) -> Option<i32> {
    call.param("size").parse::<i32>().ok().filter(|size| *size > 0)
}

/// A genre or pinned station's tag, which stands in for its color when its songs give none.
fn station_cover_label(station: &LastFmRadioStation) -> Option<String> {
    match station.kind {
        LastFmRadioStationKind::Genre
        | LastFmRadioStationKind::Pinned
        | LastFmRadioStationKind::Discovery => station.seeds.first().cloned(),
        _ => None,
    }
}

/// The songs whose covers color a station's cover: its seed artists' songs first (the
/// artist of an artist station, the top seeds of Your Radio), then its first songs, one per
/// album, four at most. Looked up like any song's cover outside the library.
pub(crate) fn station_cover_seeds(state: &AppState, station: &LastFmRadioStation) -> Vec<CoverSeed> {
    let seed_artists: Vec<String> = match station.kind {
        LastFmRadioStationKind::Artist
        | LastFmRadioStationKind::YourMix
        | LastFmRadioStationKind::Starter => station
            .seeds
            .iter()
            .take(3)
            .map(|seed| SongIdentity::key(seed))
            .filter(|key| !key.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let rank = |artist: &str| {
        let key = SongIdentity::key(artist);
        seed_artists
            .iter()
            .position(|seed| *seed == key)
            .unwrap_or(seed_artists.len())
    };
    let mut ordered: Vec<(usize, usize, &_)> = station
        .tracks
        .iter()
        .filter(|track| !is_blank(&track.artist) && !is_blank(&track.title))
        .enumerate()
        .map(|(order, track)| (rank(&track.artist), order, track))
        .collect();
    ordered.sort_by_key(|(rank, order, _)| (*rank, *order));

    let mut seen = std::collections::HashSet::new();
    let mut seeds = Vec::new();
    for (_, _, track) in ordered {
        let album_or_title = track.album.as_deref().unwrap_or(&track.title);
        let distinct = format!(
            "{}|{}",
            SongIdentity::key(&track.artist),
            SongIdentity::key(album_or_title)
        );
        if !seen.insert(distinct) {
            continue;
        }
        let routing = SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some(track.artist.clone()),
            title: Some(track.title.clone()),
            album: track.album.clone(),
            ..Default::default()
        };
        let covers = Arc::clone(&state.cover_art_aggregator);
        seeds.push(CoverSeed {
            identity: format!(
                "song|{}|{}",
                SongIdentity::key(&track.artist),
                SongIdentity::key(&track.title)
            ),
            fetch: Arc::new(move || {
                let covers = Arc::clone(&covers);
                let routing = routing.clone();
                async move { Ok(covers.get_cover(&routing, false).await.map(|b| b.to_vec())) }.boxed()
            }),
        });
        if seeds.len() == 4 {
            break;
        }
    }
    seeds
}

const CATALOG_IMAGE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// A cover the catalog's own image host serves over https (cdn-images.dzcdn.net and its kin).
pub(crate) fn is_catalog_image(url: &str) -> bool {
    let Ok(uri) = url::Url::parse(url) else {
        return false;
    };
    uri.scheme() == "https"
        && uri.host_str().is_some_and(|host| {
            let host = host.to_ascii_lowercase();
            ["dzcdn.net", "mzstatic.com"]
                .iter()
                .any(|catalog| host == *catalog || host.ends_with(&format!(".{catalog}")))
        })
}

/// A catalog picture through the catalog's client, 8 MB at most; None on any failure.
async fn catalog_image(
    deezer: &crate::services::metadata::DeezerRateLimitHandler,
    url: &str,
) -> Option<Vec<u8>> {
    let request = reqwest::Request::new(reqwest::Method::GET, url::Url::parse(url).ok()?);
    let read = async {
        let mut response = deezer.send(request, false).await.ok()?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > CATALOG_IMAGE_MAX_BYTES as u64)
        {
            return None;
        }
        let mut copy: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if copy.len() + chunk.len() > CATALOG_IMAGE_MAX_BYTES {
                return None;
            }
            copy.extend_from_slice(&chunk);
        }
        Some(copy)
    };
    tokio::time::timeout(crate::services::metadata::DeezerRateLimitHandler::TIMEOUT, read)
        .await
        .ok()
        .flatten()
}

/// A chart list's cover pictures: its first songs, one per album, four at most. An outside song's
/// picture comes from the cover sources (a chart's own artwork first), a library song's from
/// Navidrome, asked as the listener.
fn chart_cover_seeds(
    state: &AppState,
    proxy: &crate::services::subsonic::SubsonicProxyService,
    chart: &crate::services::library::ChartCover,
    parameters: &octo_subsonic::Parameters,
) -> Vec<CoverSeed> {
    let auth: octo_subsonic::Parameters = parameters
        .iter()
        .filter(|(key, _)| key.as_str() != "id" && key.as_str() != "size")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let text = |entry: &serde_json::Value, name: &str| {
        entry
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut albums = std::collections::HashSet::new();
    let mut seeds = Vec::new();
    for entry in &chart.entries {
        let (id, artist, title, album) = (
            text(entry, "id"),
            text(entry, "artist"),
            text(entry, "title"),
            text(entry, "album"),
        );
        let distinct = format!(
            "{}|{}",
            SongIdentity::key(&artist),
            SongIdentity::key(if album.is_empty() { &title } else { &album })
        );
        if !albums.insert(distinct) {
            continue;
        }
        if let Some(song) = chart.outside.iter().find(|song| song.id == id) {
            let routing = SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some(song.artist.clone()),
                title: Some(song.title.clone()),
                album: Some(song.album.clone()).filter(|a| !a.is_empty()),
                ..Default::default()
            };
            let covers = Arc::clone(&state.cover_art_aggregator);
            seeds.push(CoverSeed {
                identity: format!(
                    "song|{}|{}",
                    SongIdentity::key(&song.artist),
                    SongIdentity::key(&song.title)
                ),
                fetch: Arc::new(move || {
                    let covers = Arc::clone(&covers);
                    let routing = routing.clone();
                    async move { Ok(covers.get_cover(&routing, true).await.map(|b| b.to_vec())) }.boxed()
                }),
            });
        } else {
            let cover = text(entry, "coverArt");
            if cover.is_empty() {
                continue;
            }
            let proxy = proxy.clone();
            let mut asking = auth.clone();
            asking.insert("id".into(), cover.clone());
            asking.insert("size".into(), "128".into());
            seeds.push(CoverSeed {
                identity: format!("navidrome|{cover}"),
                fetch: Arc::new(move || {
                    let proxy = proxy.clone();
                    let asking = asking.clone();
                    async move {
                        let picture = proxy.relay("rest/getCoverArt", &asking).await?;
                        let image = picture
                            .content_type
                            .as_deref()
                            .is_some_and(|ct| dotnet::starts_with_ignore_case(ct, "image/"));
                        Ok(image.then(|| picture.body.to_vec()))
                    }
                    .boxed()
                }),
            });
        }
        if seeds.len() == 4 {
            break;
        }
    }
    seeds
}

/// The songs whose covers color a mix's cover: the first of this period's draw, one per
/// album cover, four at most, read from Navidrome as the listener. A period's first draw is
/// made only for a caller Navidrome accepts, as opening the mix would.
async fn mix_cover_seeds(
    state: &AppState,
    proxy: &crate::services::subsonic::SubsonicProxyService,
    username: &str,
    mix: &GeneratedPlaylist,
    parameters: &octo_subsonic::Parameters,
) -> Vec<CoverSeed> {
    // New Releases is mostly albums Navidrome has never heard of: its covers come from the
    // catalog's own image host, as the album pages of outside albums do. Only that host, over
    // https, through the catalog's client (8 second limit), and no picture bigger than 8 MB.
    if mix.kind == octo_core::library::for_you_lists::NEW_RELEASES_KIND {
        return state
            .generated_playlists
            .for_you_covers(username)
            .into_iter()
            .filter(|url| is_catalog_image(url))
            .map(|url| {
                let deezer = Arc::clone(&state.deezer_client);
                CoverSeed {
                    identity: format!("catalog|{url}"),
                    fetch: Arc::new(move || {
                        let deezer = Arc::clone(&deezer);
                        let url = url.clone();
                        async move { Ok(catalog_image(&deezer, &url).await) }.boxed()
                    }),
                }
            })
            .collect();
    }
    let auth: octo_subsonic::Parameters = parameters
        .iter()
        .filter(|(key, _)| key.as_str() != "id" && key.as_str() != "size")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let songs = match state.generated_playlists.drawn(username, mix) {
        Some(songs) => songs,
        None => {
            let ping = proxy.relay_safe("rest/ping", &auth).await;
            let format = auth.get("f").map_or("xml", String::as_str);
            if !ping.is_some_and(|ping| is_successful_subsonic_response(&ping.body, format)) {
                return Vec::new();
            }
            // Not canceled with the cover: a draw that finishes late still serves the next
            // request.
            let service = Arc::clone(&state.generated_playlists);
            let (user, mix, parameters) = (username.to_string(), mix.clone(), parameters.clone());
            let draw = tokio::spawn(async move {
                service
                    .materialize(&user, &mix, &parameters, &CancellationToken::new())
                    .await
            });
            match draw.await {
                Ok(Ok(songs)) => songs,
                _ => return Vec::new(),
            }
        }
    };

    let mut covers: Vec<String> = Vec::new();
    for song in &songs {
        let cover = match song.get("coverArt") {
            None | Some(octo_core::json::dom::Node::Null) => continue,
            Some(octo_core::json::dom::Node::String(text)) => text.clone(),
            Some(other) => other.to_json_string(false),
        };
        if cover.is_empty() || covers.contains(&cover) {
            continue;
        }
        covers.push(cover);
        if covers.len() == 4 {
            break;
        }
    }
    covers
        .into_iter()
        .map(|cover| {
            let proxy = proxy.clone();
            let mut asking = auth.clone();
            asking.insert("id".into(), cover.clone());
            asking.insert("size".into(), "128".into());
            CoverSeed {
                identity: format!("navidrome|{cover}"),
                fetch: Arc::new(move || {
                    let proxy = proxy.clone();
                    let asking = asking.clone();
                    async move {
                        let picture = proxy.relay("rest/getCoverArt", &asking).await?;
                        let image = picture
                            .content_type
                            .as_deref()
                            .is_some_and(|ct| dotnet::starts_with_ignore_case(ct, "image/"));
                        Ok(image.then(|| picture.body.to_vec()))
                    }
                    .boxed()
                }),
            }
        })
        .collect()
}

/// Whether the request comes from the Octo app, which shows on its own artwork that a song
/// is not in the library, so it wants covers without the badge. Told apart by the Subsonic
/// client name it sends on every call.
pub(crate) fn draws_its_own_marks(call: &SubsonicCall) -> bool {
    draws_its_own_marks_in(&call.parameters)
}

/// [`draws_its_own_marks`] over the request's parameters (`DrawsItsOwnMarks`).
pub(crate) fn draws_its_own_marks_in(parameters: &octo_subsonic::Parameters) -> bool {
    parameters
        .get("c")
        .map_or("", String::as_str)
        .trim()
        .eq_ignore_ascii_case("Octo")
}

/// Returns a 200 response with the Octo placeholder JPEG. Used in every code path that
/// previously returned 404: Subsonic clients (Arpeggio especially) drop play-queue entries
/// whose cover-art request fails, so we always serve something rather than fail.
pub(crate) async fn serve_placeholder(state: &AppState, branded: bool) -> Response {
    let service = Arc::clone(&state.cover_art_service);
    let bytes = tokio::task::spawn_blocking(move || service.get_placeholder_cover(branded))
        .await
        .unwrap_or_default();
    if bytes.is_empty() {
        return problem(StatusCode::NOT_FOUND);
    }
    file(bytes, "image/jpeg")
}

// ---------------------------------------------------------------------------------------
// getTranscodeDecision
// ---------------------------------------------------------------------------------------

/// OpenSubsonic transcoding extension. Feishin posts here before /rest/stream to ask the
/// server "should I transcode this or play it directly?" Navidrome implements this for local
/// songs. For external (Octo placeholder) songs the upstream relay returns nothing useful and
/// Feishin gets stuck: won't even issue the /rest/stream call. So we hijack: external IDs
/// always direct-play, local IDs pass through to Navidrome's real implementation.
pub async fn get_transcode_decision(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let media_id = call.param("mediaId").to_string();
    let (is_external, _, _) = state.local_library.parse_song_id(&media_id);

    if is_external {
        debug!("getTranscodeDecision: direct-play for external id {media_id}");
        return Ok(direct_play_response(&state));
    }

    match call
        .proxy
        .relay("rest/getTranscodeDecision.view", &call.parameters)
        .await
    {
        Ok(result) => Ok(file(
            result.body,
            result.content_type.as_deref().unwrap_or("application/json"),
        )),
        Err(crate::services::subsonic::RelayError::Http(message)) => {
            // Navidrome may be stock-Subsonic without the OpenSubsonic transcoding
            // extension. Returning a non-200 also makes Feishin fall back to the
            // direct stream URL, but a positive direct-play decision is cleaner.
            debug!("getTranscodeDecision local relay failed ({message}); returning direct-play");
            Ok(direct_play_response(&state))
        }
        Err(other) => Err(other.into()),
    }
}

/// canDirectPlay:true is the only field Feishin's controller checks on the happy path: see
/// Feishin's subsonic-controller.ts: requiresTranscoding = !td?.canDirectPlay. Returning the
/// minimal envelope lets it advance to /rest/stream which is where our own controller takes
/// over for externals.
fn direct_play_response(state: &AppState) -> Response {
    state
        .subsonic_response_builder
        .create_json_response(json!({
            "status": "ok",
            "version": "1.16.1",
            "transcodeDecision": { "canDirectPlay": true, "canTranscode": false },
        }))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_typed_by_its_extension() {
        for (path, content_type) in [
            ("/m/a.mp3", "audio/mpeg"),
            ("/m/a.FLAC", "audio/flac"),
            ("/m/a.ogg", "audio/ogg"),
            ("/m/a.m4a", "audio/mp4"),
            ("/m/a.wav", "audio/wav"),
            ("/m/a.aac", "audio/aac"),
            ("/m/a.opus", "audio/mpeg"),
            ("/m/a", "audio/mpeg"),
        ] {
            assert_eq!(content_type_for_file(path), content_type, "{path}");
        }
    }
}
