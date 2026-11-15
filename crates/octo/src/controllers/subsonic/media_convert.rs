//! Songs found online in the format and size a client asks for: `format`, `maxBitRate` (or
//! `bitrate` on `download`), or a family ceiling. Every answer for such a song passes through
//! here: the source as it is, a copy made in the asked format (built once from the play's own
//! source and read while it grows), or, for a time seek (`timeOffset`), the song from that
//! time. What the listener heard stays the source, never the copy made of it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;
use octo_core::fingerprint::heart_gate::{HeardRecord, HeardSource};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::caller_checks::{SubsonicCall, is_first_byte_request};
use super::media::{
    content_type_for_file, direct_play_response, heard_by_whom, keep_heard, name_video, sample_class,
    serve_file, serve_handle, song_seconds, stand_in_video, stream_response,
};
use crate::app::AppState;
use crate::http::error::AppError;
use crate::services::i_download_service::{AudioStream, StreamPlay};
use crate::services::prefetch::convert_rules::{
    self, Ask, AskOf, Codec, Container, GateClient, Plan, SourceAudio, SourceCodec, Target,
};
use crate::services::prefetch::converted_copies::{
    BuildInput, BuildState, OwnedPermit, RunningJob, build_key, source_key_video,
};
use crate::services::prefetch::play_source::Answered;
use crate::services::prefetch::preview_copies::CopyFeed;
use crate::services::prefetch::{Capture, from_start};
use crate::services::soulseek::soulseek_download_service::played_size;
use crate::services::you_tube::pin_recheck::{PlayStart, play_start};

/// The channels Octo describes a song found online with.
const CHANNELS: u32 = 2;

/// What the funnel read from one request for a song found online, once, before any answer.
#[derive(Debug, Clone)]
pub(crate) struct OutsideAsk {
    /// The song's id as the client named it.
    pub id: String,
    pub ask: AskOf,
    pub client: GateClient,
    /// The play the request belongs to, as Octo tells plays apart: its device's key, its play
    /// token, its address and what it is for.
    pub request: StreamPlay,
    /// Whether this request starts a play: from the first byte with no time seek, or a time
    /// seek of a song no play of this device has open (a song restored at its place).
    pub play_start: bool,
    /// When the request arrived; every wait counts from here.
    pub arrived: Instant,
}

/// How the funnel answered.
pub(crate) enum Funnel {
    /// This answer, with what the listener heard when it keeps a record.
    Answer(Response, Option<HeardRecord>),
    /// The source as it is, is what the client asked for.
    AsIs,
    /// What was asked cannot be had now: the request takes today's path.
    Today,
}

/// What a build came to for one request.
enum Built {
    Served(Response),
    Busy(Response),
    /// The source answers this request instead (a failed build, or an Octo app's own playback
    /// past the wait).
    Source,
}

/// Reads what the client asks of an outside song and whether the request starts a play.
/// `request` is the play the request belongs to; `forced` is an ask `getTranscodeStream`
/// decoded from its parameters.
pub(crate) fn read_outside(
    state: &AppState,
    call: &SubsonicCall,
    request: &StreamPlay,
    id: &str,
    endpoint: &str,
    forced: Option<AskOf>,
    arrived: Instant,
) -> OutsideAsk {
    let user_agent = call.header("User-Agent").unwrap_or_default();
    let client = call
        .own_ask
        .as_ref()
        .map_or_else(|| GateClient::of(call.param("c"), &user_agent), |own| own.client);
    let ask = forced
        .unwrap_or_else(|| convert_rules::ask_of(&call.parameters, endpoint, call.own_ask.as_ref(), client));
    // A look at the song (a HEAD, a scout read) starts no play.
    let first =
        !request.peeks() && is_first_byte_request(call.method().as_str(), call.header("Range").as_deref());
    let play_start = first
        && (ask.offset == 0
            || !state
                .prefetch
                .sources()
                .live(std::slice::from_ref(&request.key), id));
    OutsideAsk {
        id: id.to_string(),
        ask,
        client,
        request: request.clone(),
        play_start,
        arrived,
    }
}

/// Keeps what this request's play got, renewing its time.
fn note(state: &AppState, outside: &OutsideAsk, length: Option<i32>, answered: Answered) {
    state.prefetch.sources().note(
        std::slice::from_ref(&outside.request.key),
        &outside.id,
        length,
        answered,
    );
}

/// What a later request of a play got at its start; None for a request that starts a play, or a
/// HEAD.
fn continuing(state: &AppState, outside: &OutsideAsk, head: bool) -> Option<Answered> {
    if outside.play_start || head {
        return None;
    }
    state
        .prefetch
        .sources()
        .answered(std::slice::from_ref(&outside.request.key), &outside.id)
}

/// A file Octo holds for an outside song, as the client asked it: the file as it is (today's
/// answer), a copy made in the asked format, or the song from a time seek's place. The caller
/// keeps what was heard from the file, as before.
pub(crate) async fn serve_outside_file(
    state: &AppState,
    call: &SubsonicCall,
    external_id: &str,
    path: &str,
) -> Result<Response, AppError> {
    let Some(outside) = call.outside_ask().cloned() else {
        return serve_file(call, path).await;
    };
    let length = song_seconds(state, external_id);
    if matches!(outside.ask.ask, Ask::None) {
        note(state, &outside, length, Answered::Source);
        return serve_file(call, path).await;
    }
    let head = call.method() == Method::HEAD;
    let continuing = continuing(state, &outside, head);
    if let Some(Answered::Conversion(key)) = &continuing
        && outside.ask.offset == 0
        && let Some(response) = serve_built(state, call, key).await
    {
        note(state, &outside, length, Answered::Conversion(key.clone()));
        return Ok(response);
    }
    let file = PathBuf::from(path);
    let Some((source_key, source)) = state.converted_copies.file_source(&file).await else {
        // A time seek is never answered from the file's first byte under a clock at its place.
        if outside.ask.offset > 0 && !head {
            return Ok(cannot_seek());
        }
        if !head {
            note(state, &outside, length, Answered::Source);
        }
        return serve_file(call, path).await;
    };
    let file_type = content_type_for_file(path);
    let plan = match continuing {
        Some(Answered::Source) => Plan::AsIs,
        _ => usable_plan(state, &outside.ask.ask, &source),
    };
    let converted = &state.converted_copies;
    if plan == Plan::AsIs {
        if head {
            return serve_file(call, path).await;
        }
        note(state, &outside, length, Answered::Source);
        if outside.ask.offset > 0 {
            return Ok(seek(
                state,
                &outside,
                &Plan::AsIs,
                &source,
                file_type,
                None,
                SeekFrom::Path(file),
            )
            .await
            .unwrap_or_else(cannot_seek));
        }
        return serve_file(call, path).await;
    }
    let key = build_key(&source_key, &plan);
    let target_type = convert_rules::answer_type(&plan, file_type);
    let continuing_conversion = matches!(continuing, Some(Answered::Conversion(_)));
    if head {
        if converted.failed(&key) {
            return serve_file(call, path).await;
        }
        return Ok(match serve_built(state, call, &key).await {
            Some(response) => response,
            None => type_only(&target_type),
        });
    }
    if converted.failed(&key) && !continuing_conversion {
        note(state, &outside, length, Answered::Source);
        if outside.ask.offset > 0 {
            return Ok(seek(
                state,
                &outside,
                &Plan::AsIs,
                &source,
                file_type,
                None,
                SeekFrom::Path(file),
            )
            .await
            .unwrap_or_else(cannot_seek));
        }
        return serve_file(call, path).await;
    }
    if outside.ask.offset > 0 {
        note(state, &outside, length, Answered::Conversion(key.clone()));
        let built = converted.built_path(&key);
        return Ok(seek(
            state,
            &outside,
            &plan,
            &source,
            file_type,
            built,
            SeekFrom::Path(file),
        )
        .await
        .unwrap_or_else(cannot_seek));
    }
    let rx = converted.join_or_start(
        &key,
        BuildInput::File(file),
        plan,
        outside.play_start && !offline(call),
    );
    if continuing_conversion {
        // A later request of a play that got this conversion: its bytes (the running build, or
        // one rebuild of a failed one, which makes the same bytes), or a refusal.
        note(state, &outside, length, Answered::Conversion(key.clone()));
        return Ok(finished(state, call, &key, rx).await.unwrap_or_else(unavailable));
    }
    match answer_build(state, call, &outside, &key, &target_type, file_type, rx).await {
        Built::Served(response) => {
            note(state, &outside, length, Answered::Conversion(key));
            Ok(response)
        }
        Built::Busy(response) => Ok(response),
        Built::Source => {
            note(state, &outside, length, Answered::Source);
            serve_file(call, path).await
        }
    }
}

/// The play's video in the format the client asked, called first by the direct stream, and by
/// the FLAC stand-in (`stand_in`) when a format or a cap is asked. [`Funnel::AsIs`] and
/// [`Funnel::Today`] leave the request to the caller's own path; in the stand-in a failed
/// conversion is [`Funnel::Today`], so the stand-in is built as before.
pub(crate) async fn converted_stream(
    state: &AppState,
    call: &SubsonicCall,
    provider: &str,
    external_id: &str,
    stand_in: bool,
) -> anyhow::Result<Funnel> {
    let Some(outside) = call.outside_ask().cloned() else {
        return Ok(Funnel::Today);
    };
    let length = song_seconds(state, external_id);
    if matches!(outside.ask.ask, Ask::None) {
        note(state, &outside, length, Answered::Source);
        return Ok(Funnel::Today);
    }
    let head = call.method() == Method::HEAD;
    let continuing = continuing(state, &outside, head);
    if let Some(Answered::Conversion(key)) = &continuing
        && outside.ask.offset == 0
        && let Some(mut response) = serve_built(state, call, key).await
    {
        note(state, &outside, length, Answered::Conversion(key.clone()));
        let bound = state
            .external_id_registry
            .bound_play(external_id, &outside.request);
        name_video(&mut response, bound.as_ref().map(|bound| bound.video.as_str()));
        return Ok(Funnel::Answer(response, None));
    }
    let Some(named) = named_video(state, call, &outside, provider, external_id).await? else {
        // A time seek never falls back to the stream from its first byte: 503, which Octo's
        // desktop turns into a reopen at its place.
        if outside.ask.offset > 0 && !head {
            return Ok(Funnel::Answer(busy(), None));
        }
        if !head {
            note(state, &outside, length, Answered::Source);
        }
        return Ok(Funnel::Today);
    };
    let source = convert_rules::source_of_type(&named.content_type, named.kbps);
    let plan = match continuing {
        Some(Answered::Source) => Plan::AsIs,
        _ => usable_plan(state, &outside.ask.ask, &source),
    };
    let target_type = convert_rules::answer_type(&plan, &named.content_type);
    if head {
        let funnel = head_of_video(state, call, &named, &plan, &target_type).await;
        return Ok(from_video(state, &outside, external_id, &named, None, funnel));
    }
    if plan == Plan::AsIs && outside.ask.offset == 0 {
        note(state, &outside, length, Answered::Source);
        return Ok(Funnel::AsIs);
    }
    let converted = &state.converted_copies;
    let deadline = outside.arrived + converted.timings().first_output;
    let Some(feed) = state
        .preview_copies
        .feed_copy(&named.video, named.total, deadline.into_std())
        .await
    else {
        if outside.ask.offset > 0 {
            return Ok(Funnel::Answer(busy(), None));
        }
        note(state, &outside, length, Answered::Source);
        return Ok(Funnel::Today);
    };
    // An MP4 whose first boxes put `mdat` before `moov` (or do not tell yet) cannot be read by
    // ffmpeg from a pipe: it is read from the complete copy instead, waited for within the
    // deadline. YouTube's own M4A is fragmented and streams from its start.
    let feed = if feed.complete_path.is_none() && is_mp4_type(&feed.content_type) {
        let streams = state
            .preview_copies
            .told_head(&named.video, 64 * 1024, deadline)
            .await
            .and_then(|head| convert_rules::mp4_streams_from_start(&head));
        if streams == Some(true) {
            feed
        } else {
            let (video, total, copy_type) = (named.video.clone(), feed.total, feed.content_type.clone());
            drop(feed);
            // An Octo app's own playback waits within the first-output deadline (then plays the
            // source, which it reads whatever it is); any other client waits as long as it may
            // for the format it asked.
            let own = own_playback(call, &outside, &copy_type);
            let until = if own {
                deadline
            } else {
                outside.arrived + converted.timings().others
            };
            let whole = tokio::time::timeout_at(until, async {
                state.preview_copies.wait_complete(&video).await?;
                state
                    .preview_copies
                    .feed_copy(&video, Some(total), until.into_std())
                    .await
            })
            .await
            .ok()
            .flatten()
            .filter(|whole| whole.complete_path.is_some());
            match whole {
                Some(whole) => whole,
                None => {
                    // The build starts once the copy is whole, so the player's retry (or the next
                    // play) joins it instead of waiting all over again.
                    build_when_whole(state, &video, total, plan);
                    if outside.ask.offset > 0 || !own {
                        return Ok(Funnel::Answer(busy(), None));
                    }
                    note(state, &outside, length, Answered::Source);
                    return Ok(Funnel::Today);
                }
            }
        }
    } else {
        feed
    };
    let (total, copy_type, complete) = (feed.total, feed.content_type.clone(), feed.complete_path.clone());
    let key = build_key(&source_key_video(&named.video, total), &plan);
    let continuing_conversion = matches!(continuing, Some(Answered::Conversion(_)));
    let usable = plan != Plan::AsIs && !(converted.failed(&key) && !continuing_conversion);
    if outside.ask.offset > 0 {
        let (plan, built) = if usable {
            (plan, converted.built_path(&key))
        } else {
            (Plan::AsIs, None)
        };
        let answered = if plan == Plan::AsIs {
            Answered::Source
        } else {
            Answered::Conversion(key.clone())
        };
        note(state, &outside, length, answered);
        let response = seek(
            state,
            &outside,
            &plan,
            &source,
            &copy_type,
            built,
            SeekFrom::Copy(feed),
        )
        .await
        .unwrap_or_else(cannot_seek);
        let heard = hear_video(
            state,
            call,
            provider,
            external_id,
            &named.video,
            total,
            &copy_type,
            complete,
        )
        .await;
        let funnel = Funnel::Answer(response, heard);
        return Ok(from_video(
            state,
            &outside,
            external_id,
            &named,
            Some(total),
            funnel,
        ));
    }
    if !usable {
        note(state, &outside, length, Answered::Source);
        if stand_in {
            return Ok(Funnel::Today);
        }
        let funnel = copy_answer(
            state,
            call,
            provider,
            external_id,
            &named.video,
            total,
            &copy_type,
            complete,
        )
        .await;
        return Ok(from_video(
            state,
            &outside,
            external_id,
            &named,
            Some(total),
            funnel,
        ));
    }
    let (input, holder) = held_input(state, &named.video, &key, feed);
    let rx = converted.join_or_start(&key, input, plan, outside.play_start && !offline(call));
    release_when_done(state, &named.video, holder, rx.clone());
    if continuing_conversion {
        note(state, &outside, length, Answered::Conversion(key.clone()));
        let response = finished(state, call, &key, rx).await.unwrap_or_else(unavailable);
        let funnel = Funnel::Answer(response, None);
        return Ok(from_video(
            state,
            &outside,
            external_id,
            &named,
            Some(total),
            funnel,
        ));
    }
    match answer_build(state, call, &outside, &key, &target_type, &copy_type, rx).await {
        Built::Served(response) => {
            note(state, &outside, length, Answered::Conversion(key));
            let heard = hear_video(
                state,
                call,
                provider,
                external_id,
                &named.video,
                total,
                &copy_type,
                complete,
            )
            .await;
            let funnel = Funnel::Answer(response, heard);
            Ok(from_video(
                state,
                &outside,
                external_id,
                &named,
                Some(total),
                funnel,
            ))
        }
        Built::Busy(response) => Ok(Funnel::Answer(response, None)),
        Built::Source => {
            note(state, &outside, length, Answered::Source);
            if stand_in {
                return Ok(Funnel::Today);
            }
            let funnel = copy_answer(
                state,
                call,
                provider,
                external_id,
                &named.video,
                total,
                &copy_type,
                complete,
            )
            .await;
            Ok(from_video(
                state,
                &outside,
                external_id,
                &named,
                Some(total),
                funnel,
            ))
        }
    }
}

/// A build answered for one request: an exact ask (`estimateContentLength=true`, or a range
/// with an end, or a later byte) gets the complete file; any other request from the first
/// byte gets the bytes as they are written. Past the wait for the first output, an Octo app's
/// own playback gets the source; any other client waits on, then gets 503 with a time to try
/// again while the build goes on.
async fn answer_build(
    state: &AppState,
    call: &SubsonicCall,
    outside: &OutsideAsk,
    key: &str,
    target_type: &str,
    source_type: &str,
    mut rx: watch::Receiver<BuildState>,
) -> Built {
    let timings = state.converted_copies.timings().clone();
    let range = call.header("Range");
    let exact = outside.ask.exact || bounded_first_range(range.as_deref()) || later_byte(range.as_deref());
    let first_until = outside.arrived + timings.first_output;
    let others_until = outside.arrived + timings.others;
    let own = own_playback(call, outside, source_type);
    if exact {
        let done = match completed(&mut rx, first_until).await {
            None if own => return Built::Source,
            None => completed(&mut rx, others_until).await,
            done => done,
        };
        return match done {
            Some(true) => serve_built(state, call, key)
                .await
                .map_or(Built::Source, Built::Served),
            Some(false) => Built::Source,
            None => Built::Busy(busy()),
        };
    }
    let started = match first_output(&mut rx, first_until).await {
        None if own => return Built::Source,
        None => first_output(&mut rx, others_until).await,
        started => started,
    };
    match started {
        Some(true) => Built::Served(growing_answer(state, key, rx, target_type)),
        Some(false) => Built::Source,
        None => Built::Busy(busy()),
    }
}

/// Waits until the build has ended or `until`: Some(true) when it completed, Some(false) when it
/// failed, None when the time ran out.
async fn completed(rx: &mut watch::Receiver<BuildState>, until: Instant) -> Option<bool> {
    match tokio::time::timeout_at(until, rx.wait_for(|build| build.ended.is_some())).await {
        Ok(Ok(build)) => Some(matches!(build.ended, Some(Ok(_)))),
        Ok(Err(_)) => Some(false),
        Err(_) => None,
    }
}

/// Waits until the build has written its first bytes or ended, or `until`: Some(true) when
/// bytes exist and it has not failed, Some(false) when it failed first, None when the time ran
/// out.
async fn first_output(rx: &mut watch::Receiver<BuildState>, until: Instant) -> Option<bool> {
    match tokio::time::timeout_at(
        until,
        rx.wait_for(|build| build.written > 0 || build.ended.is_some()),
    )
    .await
    {
        Ok(Ok(build)) => Some(build.written > 0 && !matches!(build.ended, Some(Err(_)))),
        Ok(Err(_)) => Some(false),
        Err(_) => None,
    }
}

/// A later request of a converted play: the build complete within the wait for other clients,
/// served from its file; None when it failed or did not end in time.
async fn finished(
    state: &AppState,
    call: &SubsonicCall,
    key: &str,
    mut rx: watch::Receiver<BuildState>,
) -> Option<Response> {
    let until = Instant::now() + state.converted_copies.timings().others;
    if completed(&mut rx, until).await == Some(true) {
        serve_built(state, call, key).await
    } else {
        None
    }
}

/// The complete conversion `key` from its opened file (exact length, one range), or None.
async fn serve_built(state: &AppState, call: &SubsonicCall, key: &str) -> Option<Response> {
    let (file, length) = state.converted_copies.complete(key)?;
    serve_handle(call, file, length, type_of_key(key)).await.ok()
}

/// The type a conversion is served as, by its target (the part of its key after the last dot).
fn type_of_key(key: &str) -> &'static str {
    let target = key.rsplit('.').next().unwrap_or("");
    if target.starts_with("mp3") {
        "audio/mpeg"
    } else if target.starts_with("aac") {
        "audio/aac"
    } else {
        "audio/ogg"
    }
}

/// The bytes of a running build as they are written: the target's type and no length.
fn growing_answer(
    state: &AppState,
    key: &str,
    rx: watch::Receiver<BuildState>,
    content_type: &str,
) -> Response {
    let mut response = Response::new(state.converted_copies.growing(key, rx));
    let headers = response.headers_mut();
    if let Ok(content_type) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, content_type);
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
}

/// A HEAD of an outside song asked in another format: the complete conversion's headers, or the
/// target's type alone; the source's (today's path) while the conversion is remembered as
/// failed. It starts nothing.
async fn head_of_video(
    state: &AppState,
    call: &SubsonicCall,
    named: &NamedVideo,
    plan: &Plan,
    target_type: &str,
) -> Funnel {
    if *plan == Plan::AsIs {
        return Funnel::AsIs;
    }
    let Some((size, _)) = state.preview_copies.copy_facts(&named.video) else {
        return Funnel::Answer(type_only(target_type), None);
    };
    let key = build_key(&source_key_video(&named.video, size), plan);
    if state.converted_copies.failed(&key) {
        return Funnel::Today;
    }
    match serve_built(state, call, &key).await {
        Some(response) => Funnel::Answer(response, None),
        None => Funnel::Answer(type_only(target_type), None),
    }
}

/// The video a play gets, found by the one play-start rule as the FLAC stand-in finds it, and
/// what is known of it: its type, its whole size, its rate, and the request's place in its play.
struct NamedVideo {
    video: String,
    content_type: String,
    total: Option<u64>,
    kbps: Option<u32>,
    decision: PlayStart,
}

/// Names the play's video. A request that starts a play from its first byte takes the better
/// video waiting for the song first, as the direct stream does; then it gets the video
/// `stand_in_video` gives it (a chosen version at a play's start, the video a running play is
/// bound to, else the pin), as every request does. A time seek of a running play keeps its
/// play's video and switches nothing. Type and size come from the video's copy, else from the
/// stream's facts while they are that video's, else from a first-bytes probe of the stream,
/// which also names a video when none is pinned. None when no video can be named.
async fn named_video(
    state: &AppState,
    call: &SubsonicCall,
    outside: &OutsideAsk,
    provider: &str,
    external_id: &str,
) -> anyhow::Result<Option<NamedVideo>> {
    let registry = &state.external_id_registry;
    let range = call.header("Range");
    let (decision, video, confirmed) = if !outside.play_start && outside.ask.offset > 0 {
        match registry.bound_play(external_id, &outside.request) {
            Some(bound) => (PlayStart::Continue, Some(bound.video), bound.size),
            None => (
                PlayStart::Continue,
                pinned_video(state, external_id),
                confirmed_size(state, external_id),
            ),
        }
    } else {
        if outside.play_start {
            let binding = registry.bound_play(external_id, &outside.request);
            if matches!(
                play_start(&outside.request, range.as_deref(), binding.as_ref()),
                PlayStart::Start { switch: true }
            ) {
                take_pending_video(state, external_id);
            }
        }
        stand_in_video(state, &outside.request, range.as_deref(), external_id)
    };
    // The stream's facts tell of the song's pinned video only.
    let facts = registry
        .stream_facts(external_id)
        .filter(|_| video.is_some() && pinned_video(state, external_id) == video);
    let declared = facts
        .as_ref()
        .filter(|facts| facts.itag.is_some())
        .and_then(|facts| facts.declared());
    let kbps = declared
        .as_ref()
        .and_then(|declared| declared.bit_rate)
        .and_then(|rate| u32::try_from(rate).ok());
    if let Some(video) = &video {
        if let Some((size, content_type)) = state.preview_copies.copy_facts(video) {
            return Ok(Some(NamedVideo {
                video: video.clone(),
                content_type,
                total: Some(size),
                kbps,
                decision,
            }));
        }
        if let Some(declared) = declared {
            return Ok(Some(NamedVideo {
                video: video.clone(),
                content_type: declared.content_type,
                total: confirmed,
                kbps,
                decision,
            }));
        }
    }
    let probe = state
        .download_service
        .get_direct_stream(
            provider,
            external_id,
            Some("bytes=0-1"),
            None,
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
    let Some(probed) = probe.video_id.clone() else {
        return Ok(None);
    };
    // The probe reads the song's own video, which is not the one a running play is bound to once
    // the song has moved on: such a play is left to the direct stream, which keeps its video.
    if video.as_ref().is_some_and(|video| *video != probed) {
        return Ok(None);
    }
    Ok(Some(NamedVideo {
        video: probed,
        content_type: probe.content_type.clone(),
        total,
        kbps,
        decision,
    }))
}

/// The song's pinned video, when it has one.
fn pinned_video(state: &AppState, external_id: &str) -> Option<String> {
    state
        .external_id_registry
        .resolve_song(external_id, false)
        .and_then(|routing| routing.snapshot().you_tube_id)
        .filter(|video| !video.trim().is_empty())
}

/// The whole size a play confirmed of the video the song plays.
fn confirmed_size(state: &AppState, external_id: &str) -> Option<u64> {
    state
        .external_id_registry
        .song_stream_facts(external_id)
        .filter(|facts| facts.size_confirmed)
        .and_then(|facts| facts.size)
}

/// An answer made from the play's video: it names the video (`X-Octo-Video`), and a request
/// that starts a play binds the play to that video and its copy's size, as the FLAC stand-in
/// does. A refusal, and a request left to the caller's own path, are handed back as they are.
fn from_video(
    state: &AppState,
    outside: &OutsideAsk,
    external_id: &str,
    named: &NamedVideo,
    total: Option<u64>,
    funnel: Funnel,
) -> Funnel {
    let (mut response, heard) = match funnel {
        Funnel::Answer(response, heard) if response.status().is_success() => (response, heard),
        other => return other,
    };
    name_video(&mut response, Some(&named.video));
    if matches!(named.decision, PlayStart::Start { .. }) {
        let registry = &state.external_id_registry;
        registry.note_play(external_id);
        registry.bind_play(external_id, &outside.request, &named.video);
        if let Some(total) = total {
            registry.confirm_play_size(external_id, &outside.request, total);
        }
    }
    Funnel::Answer(response, heard)
}

/// The plan for `ask`, as the source as it is when ffmpeg has no encoder for its target.
fn usable_plan(state: &AppState, ask: &Ask, source: &SourceAudio) -> Plan {
    let plan = convert_rules::plan(ask, source);
    match &plan {
        Plan::Convert(target) if !state.converted_copies.encoders_ready(target.codec) => Plan::AsIs,
        _ => plan,
    }
}

/// A play from the first byte takes the better video waiting for the song, as the direct stream
/// does, and the copy of the video it left goes.
fn take_pending_video(state: &AppState, external_id: &str) {
    if let Some(routing) = state.external_id_registry.resolve_song(external_id, false)
        && let Some(left) = state.external_id_registry.apply_pending_video(&routing)
    {
        state.preview_copies.drop_copy(&left);
    }
}

/// The build's input: a complete copy by its path (held for the build), or the filling copy fed
/// as it grows. Answers the input and the hold to let go when the build ends.
fn held_input(state: &AppState, video: &str, key: &str, feed: CopyFeed) -> (BuildInput, Option<String>) {
    match feed.complete_path.clone() {
        Some(path) => {
            let holder = format!("play:convert:{key}");
            let held = state.preview_copies.hold(video, &holder);
            (BuildInput::File(path), held.then_some(holder))
        }
        None => (BuildInput::Copy(feed), None),
    }
}

/// Starts the build of `plan` from the video's copy once the copy is whole, held for the build, in
/// the background: a request that could not wait for the copy leaves the build to the next one.
/// Nothing for a plan that converts nothing, or a key remembered as failed.
fn build_when_whole(state: &AppState, video: &str, total: u64, plan: Plan) {
    if plan == Plan::AsIs {
        return;
    }
    let key = build_key(&source_key_video(video, total), &plan);
    if state.converted_copies.failed(&key) {
        return;
    }
    let copies = Arc::clone(&state.preview_copies);
    let converted = Arc::clone(&state.converted_copies);
    let video = video.to_string();
    tokio::spawn(async move {
        let Some(path) = copies.wait_complete(&video).await else {
            return;
        };
        let holder = format!("play:convert:{key}");
        let held = copies.hold(&video, &holder);
        let mut rx = converted.join_or_start(&key, BuildInput::File(path), plan, false);
        let _ = rx.wait_for(|build| build.ended.is_some()).await;
        if held {
            copies.release(&video, &holder);
        }
    });
}

/// Lets go of the copy's hold once the build has ended.
fn release_when_done(
    state: &AppState,
    video: &str,
    holder: Option<String>,
    mut rx: watch::Receiver<BuildState>,
) {
    let Some(holder) = holder else {
        return;
    };
    let (copies, video) = (Arc::clone(&state.preview_copies), video.to_string());
    tokio::spawn(async move {
        let _ = rx.wait_for(|build| build.ended.is_some()).await;
        copies.release(&video, &holder);
    });
}

/// The source answered from the video's copy (whole or filling), never a second fetch from the
/// shim; [`Funnel::Today`] when the copy has nothing to answer with.
#[allow(clippy::too_many_arguments)]
async fn copy_answer(
    state: &AppState,
    call: &SubsonicCall,
    provider: &str,
    external_id: &str,
    video: &str,
    total: u64,
    copy_type: &str,
    complete: Option<PathBuf>,
) -> Funnel {
    let range = call.header("Range");
    let Some(info) = state
        .preview_copies
        .serve(video, range.as_deref(), Some(total))
        .await
    else {
        return Funnel::Today;
    };
    let heard = hear_video(
        state,
        call,
        provider,
        external_id,
        video,
        total,
        copy_type,
        complete,
    )
    .await;
    Funnel::Answer(stream_response(info), heard)
}

/// What a play that starts here heard: the video's copy (its whole size and type), exactly the
/// record the FLAC stand-in keeps, with the copy sampled once it is whole. None for a request
/// that starts no play, or nobody signed in.
#[allow(clippy::too_many_arguments)]
async fn hear_video(
    state: &AppState,
    call: &SubsonicCall,
    provider: &str,
    external_id: &str,
    video: &str,
    total: u64,
    copy_type: &str,
    complete: Option<PathBuf>,
) -> Option<HeardRecord> {
    let user = heard_by_whom(state, call).await?;
    let record = keep_heard(
        state,
        call,
        external_id,
        &user,
        HeardSource::Video {
            id: video.to_string(),
            size: Some(total),
            content_type: copy_type.to_string(),
            itag: None,
        },
    );
    state.stream_size_warmer.after_play(external_id);
    state.heard_samples.stream_ok();
    let capture = Capture {
        external_id: external_id.to_string(),
        user: user.clone(),
        record: record.clone(),
        limit: 0,
        total: Some(total),
        song_seconds: song_seconds(state, external_id),
        class: sample_class(state, provider, external_id, &user),
    };
    let (state, video) = (state.clone(), video.to_string());
    tokio::spawn(async move {
        let path = match complete {
            Some(path) => Some(path),
            None => state.preview_copies.wait_complete(&video).await,
        };
        if let Some(path) = path {
            state.heard_samples.sample_file(path, capture);
        }
    });
    Some(record)
}

/// Where a time seek reads the source from.
enum SeekFrom {
    Path(PathBuf),
    Copy(CopyFeed),
}

/// The song from a time seek's place (`timeOffset`): read from the finished conversion when there
/// is one (its frames copied), else made from the source now by `plan` (an as-is plan copies the
/// source's frames into a container that can start anywhere). None when no container can carry
/// the source's frames.
async fn seek(
    state: &AppState,
    outside: &OutsideAsk,
    plan: &Plan,
    source: &SourceAudio,
    source_type: &str,
    built: Option<PathBuf>,
    from: SeekFrom,
) -> Option<Response> {
    let offset = outside.ask.offset;
    // Starting this run cancels the play's seek that arrived earlier, which stops waiting for a
    // permit or its first sound at once (and lets go of what it holds) before this one waits for
    // anything. A seek that arrived earlier than the running one starts stopped.
    let run = state
        .seek_runs
        .start(&outside.request.key, &outside.id, outside.arrived);
    if run.token.is_cancelled() {
        return Some(unavailable());
    }
    if let Some(built) = built
        && let Some(target) = target_as_source(plan)
    {
        let (_, content_type) = convert_rules::copy_container_of(&target)?;
        let args = convert_rules::offset_args(&Plan::AsIs, &target, &built.to_string_lossy(), offset);
        return Some(seek_answer(state, outside, args, None, content_type, None, run).await);
    }
    let content_type = match plan {
        Plan::AsIs => convert_rules::copy_container_of(source)?.1.to_string(),
        _ => convert_rules::answer_type(plan, source_type),
    };
    let (input, feed) = match from {
        SeekFrom::Path(path) => (path.to_string_lossy().into_owned(), None),
        SeekFrom::Copy(feed) => match feed.complete_path {
            Some(path) => (path.to_string_lossy().into_owned(), None),
            None => ("pipe:0".to_string(), Some(feed.body)),
        },
    };
    // A re-encode from the source takes one of the seeks' own permits while it starts (never a
    // build's), within the seek's wait counted from the request's arrival, else 503 (which Octo's
    // desktop turns into a reopen at its place). A copy of frames needs none.
    let permit = match plan {
        Plan::Convert(_) => {
            let converted = &state.converted_copies;
            let until = outside.arrived + converted.timings().seek_input;
            // A newer seek of the same play ends this wait, so a stale seek never holds a permit.
            tokio::select! {
                () = run.token.cancelled() => return Some(unavailable()),
                got = tokio::time::timeout_at(until, converted.seek_permit()) => match got {
                    Ok(permit) => Some(permit),
                    Err(_) => return Some(busy()),
                },
            }
        }
        _ => None,
    };
    let args = convert_rules::offset_args(plan, source, &input, offset);
    Some(seek_answer(state, outside, args, feed, &content_type, permit, run).await)
}

/// A finished conversion read as a source: the target's codec in its own container.
fn target_as_source(plan: &Plan) -> Option<SourceAudio> {
    match plan {
        Plan::Convert(Target { codec, kbps }) => Some(match codec {
            Codec::Mp3 => SourceAudio {
                codec: SourceCodec::Mp3,
                container: Container::Mpeg,
                kbps: Some(*kbps),
            },
            Codec::Opus => SourceAudio {
                codec: SourceCodec::Opus,
                container: Container::Ogg,
                kbps: Some(*kbps),
            },
            Codec::Aac => SourceAudio {
                codec: SourceCodec::Aac,
                container: Container::Adts,
                kbps: Some(*kbps),
            },
        }),
        Plan::CopyOgg => Some(SourceAudio {
            codec: SourceCodec::Opus,
            container: Container::Ogg,
            kbps: None,
        }),
        Plan::AsIs => None,
    }
}

/// Kills a run when its answer goes.
struct KillOnDrop(Option<Box<dyn FnOnce() + Send>>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(kill) = self.0.take() {
            kill();
        }
    }
}

/// One time seek's answer as it plays: the first bytes already read, the rest from ffmpeg, the
/// run killed when the answer goes or a newer seek of the play starts.
struct SeekBody {
    first: Option<Bytes>,
    stdout: Box<dyn AsyncRead + Send + Unpin>,
    feed_failed: CancellationToken,
    run: SeekRun,
    _guard: KillOnDrop,
    _done: BoxFuture<'static, std::io::Result<bool>>,
}

/// Runs ffmpeg for a time seek and answers once its first bytes exist: 200, the type, no length,
/// no ranges. 500 when ffmpeg cannot run or makes no sound; 503 when no sound came within the
/// wait (the input has not reached the place yet), which Octo's desktop turns into a reopen.
async fn seek_answer(
    state: &AppState,
    outside: &OutsideAsk,
    args: Vec<String>,
    feed: Option<AudioStream>,
    content_type: &str,
    permit: Option<OwnedPermit>,
    run: SeekRun,
) -> Response {
    let wait = state.converted_copies.timings().seek_input;
    let job = match state.converted_copies.runner().spawn(args).await {
        Ok(job) => job,
        Err(error) => {
            warn!("ffmpeg could not run for a time seek of {}: {error}", outside.id);
            return cannot_seek();
        }
    };
    let RunningJob {
        stdin,
        mut stdout,
        done,
        kill,
    } = job;
    let guard = KillOnDrop(Some(kill));
    let feed_failed = CancellationToken::new();
    if let (Some(mut sink), Some(mut body)) = (stdin, feed) {
        let failed = feed_failed.clone();
        tokio::spawn(async move {
            while let Some(chunk) = body.next().await {
                let Ok(bytes) = chunk else {
                    // Marked before stdin closes, so the answer ends with an error, never a
                    // clean end of a cut song.
                    failed.cancel();
                    return;
                };
                if sink.write_all(&bytes).await.is_err() {
                    return;
                }
            }
            let _ = sink.shutdown().await;
        });
    }
    // The wait for the first sound counts from the request's arrival, so a permit's wait and this
    // one together stay within one `seek_input`. A newer seek of the same play ends it at once:
    // returning drops the guard (ffmpeg is killed) and the permit. A container's opening (Ogg's
    // header pages, FLAC's metadata) is not sound: ffmpeg can write it before the input reaches
    // the place.
    let token = run.token.clone();
    let until = outside.arrived + wait;
    let mut first = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while !holds_sound(&first, content_type) {
        let read = tokio::select! {
            () = token.cancelled() => return unavailable(),
            read = tokio::time::timeout_at(until, stdout.read(&mut buf)) => read,
        };
        match read {
            Ok(Ok(read)) if read > 0 => first.extend_from_slice(&buf[..read]),
            Ok(_) => {
                warn!("ffmpeg made no sound for a time seek of {}", outside.id);
                return cannot_seek();
            }
            Err(_) => return unavailable(),
        }
    }
    // The permit bounds how many re-encodes start at once, not how many people listen: it goes once
    // the first sound exists.
    drop(permit);
    let body = SeekBody {
        first: Some(Bytes::from(first)),
        stdout,
        feed_failed,
        run,
        _guard: guard,
        _done: done,
    };
    let stream = futures::stream::unfold(Some(body), |body| async move {
        let mut body = body?;
        if let Some(first) = body.first.take() {
            return Some((Ok(first), Some(body)));
        }
        let mut buf = vec![0u8; 64 * 1024];
        let token = body.run.token.clone();
        let read = tokio::select! {
            () = token.cancelled() => None,
            read = body.stdout.read(&mut buf) => Some(read),
        };
        match read {
            None => None,
            Some(Ok(0)) if body.feed_failed.is_cancelled() => {
                Some((Err(std::io::Error::other("the song's copy stopped short")), None))
            }
            Some(Ok(0)) => None,
            Some(Ok(read)) => {
                buf.truncate(read);
                Some((Ok(Bytes::from(buf)), Some(body)))
            }
            Some(Err(error)) => Some((Err(error), None)),
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    if let Ok(content_type) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, content_type);
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    response
}

/// The time seeks of songs found online running now, one per play: a seek that arrived later
/// stops the play's one that arrived earlier, whose player has moved on.
#[derive(Default)]
pub struct SeekRuns {
    next: AtomicU64,
    runs: Mutex<HashMap<String, (u64, Instant, CancellationToken)>>,
}

impl SeekRuns {
    /// Starts the run of a seek that arrived at `arrived`. Newer is by arrival, never by who gets
    /// here first: a seek that arrived before the play's running one starts already stopped and
    /// leaves the running one alone.
    fn start(self: &Arc<Self>, play: &str, id: &str, arrived: Instant) -> SeekRun {
        let key = format!("{play}|{id}");
        let number = self.next.fetch_add(1, Ordering::Relaxed);
        let token = CancellationToken::new();
        let mut runs = self.runs.lock();
        match runs.get(&key) {
            Some((_, running, _)) if *running > arrived => token.cancel(),
            _ => {
                if let Some((_, _, older)) = runs.insert(key.clone(), (number, arrived, token.clone())) {
                    older.cancel();
                }
            }
        }
        drop(runs);
        SeekRun {
            runs: Arc::clone(self),
            key,
            number,
            token,
        }
    }
}

/// One running time seek; it leaves the list when its answer goes.
struct SeekRun {
    runs: Arc<SeekRuns>,
    key: String,
    number: u64,
    token: CancellationToken,
}

impl Drop for SeekRun {
    fn drop(&mut self) {
        let mut runs = self.runs.runs.lock();
        if runs
            .get(&self.key)
            .is_some_and(|(number, _, _)| *number == self.number)
        {
            runs.remove(&self.key);
        }
    }
}

/// Whether `bytes`, the start of a time seek's answer of `content_type`, hold sound and not only
/// the container's opening. MP3 and ADTS have no opening; bytes that are not the container's own
/// opening count as sound.
fn holds_sound(bytes: &[u8], content_type: &str) -> bool {
    match content_type {
        "audio/ogg" => ogg_sound_begins(bytes),
        "audio/flac" => flac_frames_begin(bytes),
        _ => !bytes.is_empty(),
    }
}

/// Whether an Ogg stream's bytes reach a page past its header pages, whose granule position is 0.
fn ogg_sound_begins(bytes: &[u8]) -> bool {
    let mut at = 0;
    loop {
        let page = &bytes[at.min(bytes.len())..];
        if page.len() >= 4 && !page.starts_with(b"OggS") {
            return true;
        }
        if page.len() < 27 {
            return false;
        }
        let granule = u64::from_le_bytes([
            page[6], page[7], page[8], page[9], page[10], page[11], page[12], page[13],
        ]);
        if granule != 0 && granule != u64::MAX {
            return true;
        }
        let segments = usize::from(page[26]);
        let Some(table) = page.get(27..27 + segments) else {
            return false;
        };
        at += 27 + segments + table.iter().map(|&size| usize::from(size)).sum::<usize>();
    }
}

/// Whether a FLAC stream's bytes reach past its metadata blocks to its first frame.
fn flac_frames_begin(bytes: &[u8]) -> bool {
    if bytes.len() >= 4 && !bytes.starts_with(b"fLaC") {
        return true;
    }
    let mut at = 4;
    loop {
        let Some(block) = bytes.get(at..at + 4) else {
            return false;
        };
        at += 4 + ((usize::from(block[1]) << 16) | (usize::from(block[2]) << 8) | usize::from(block[3]));
        if block[0] & 0x80 != 0 {
            return bytes.len() > at;
        }
    }
}

/// Whether a content type names an MP4 (M4A) file, parameters aside.
fn is_mp4_type(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("audio/mp4")
}

/// Whether the request is an offline copy (`X-Octo-Purpose: offline`).
fn offline(call: &SubsonicCall) -> bool {
    call.header("X-Octo-Purpose")
        .is_some_and(|purpose| purpose.trim().eq_ignore_ascii_case("offline"))
}

/// Whether this request is an Octo app's own playback of a source of `source_type` it plays:
/// Octo's phones and desktop play whatever bytes they get, except that the iPhone reads no WebM
/// or Ogg. An offline copy is not playback.
fn own_playback(call: &SubsonicCall, outside: &OutsideAsk, source_type: &str) -> bool {
    let octo = matches!(outside.client, GateClient::OctoPhone | GateClient::OctoDesktop);
    let iphone = call
        .header("User-Agent")
        .is_some_and(|agent| agent.contains("(iPhone)"));
    let bare = source_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let opens = !iphone || !(bare == "audio/webm" || bare == "audio/ogg");
    octo && !offline(call) && opens
}

/// Whether a Range asks from the first byte up to a set end (`bytes=0-1`): a request for an
/// exact length.
fn bounded_first_range(range: Option<&str>) -> bool {
    let Some(spec) = range.map(str::trim).and_then(|range| {
        range
            .get(..6)
            .filter(|unit| unit.eq_ignore_ascii_case("bytes="))
            .map(|_| &range[6..])
    }) else {
        return false;
    };
    let Some((from, to)) = spec.split_once('-') else {
        return false;
    };
    from.trim() == "0" && !to.trim().is_empty() && to.trim().bytes().all(|b| b.is_ascii_digit())
}

/// Whether a Range starts past the first byte.
fn later_byte(range: Option<&str>) -> bool {
    range.is_some_and(|range| !range.trim().is_empty()) && !from_start(range)
}

/// A HEAD's answer before a conversion is built: the type, ranges, no length.
fn type_only(content_type: &str) -> Response {
    let mut response = Response::new(Body::empty());
    if let Ok(content_type) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, content_type);
    }
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
}

/// 503 with a time to try again: the build goes on, and the retry joins it.
fn busy() -> Response {
    let mut response = StatusCode::SERVICE_UNAVAILABLE.into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    response
}

/// 503: a later part of a converted play that cannot be had.
fn unavailable() -> Response {
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}

/// 500: a time seek ffmpeg could not answer, so the player fails the seek rather than playing
/// from the start under a clock at another place.
fn cannot_seek() -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

// ---------------------------------------------------------------------------------------
// OpenSubsonic transcoding for songs found online
// ---------------------------------------------------------------------------------------

/// What a client posts to `getTranscodeDecision` (bitrates in bits per second).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ClientInfo {
    max_audio_bitrate: u64,
    max_transcoding_audio_bitrate: u64,
    direct_play_profiles: Vec<DirectPlayProfile>,
    transcoding_profiles: Vec<TranscodingProfile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DirectPlayProfile {
    containers: Vec<String>,
    audio_codecs: Vec<String>,
    protocols: Vec<String>,
    max_audio_channels: u32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct TranscodingProfile {
    container: String,
    audio_codec: String,
    protocol: String,
    max_audio_channels: u32,
}

/// `getTranscodeDecision` for an outside song: direct play when one of the client's profiles
/// plays the stream as it is, else a transcode Octo makes (`transcodeParams`
/// `octo1-<codec>-<kbps>`), else neither. A request that posts no profile keeps today's answer.
pub(crate) fn transcode_decision(state: &AppState, call: &SubsonicCall, media_id: &str) -> Response {
    let info: ClientInfo = serde_json::from_slice(&call.incoming.body).unwrap_or_default();
    if info.direct_play_profiles.is_empty() && info.transcoding_profiles.is_empty() {
        return direct_play_response(state);
    }
    let (_, _, external_id) = state.local_library.parse_song_id(media_id);
    let declared = external_id
        .as_deref()
        .and_then(|id| state.external_id_registry.song_stream_facts(id))
        .and_then(|facts| facts.declared());
    // A stream not known yet is the m4a at 128 kbps the listing declared.
    let (content_type, kbps) = match declared {
        Some(declared) => (
            declared.content_type,
            declared.bit_rate.and_then(|rate| u32::try_from(rate).ok()),
        ),
        None => ("audio/mp4".to_string(), Some(128)),
    };
    let source = convert_rules::source_of_type(&content_type, kbps);
    state
        .subsonic_response_builder
        .create_json_response(json!({
            "status": "ok",
            "version": "1.16.1",
            "transcodeDecision": decide_transcode(&info, &source),
        }))
        .into_response()
}

fn decide_transcode(info: &ClientInfo, source: &SourceAudio) -> Value {
    let container = container_name(&source.container);
    let codec = codec_name(&source.codec);
    let rate_ok = info.max_audio_bitrate == 0
        || source
            .kbps
            .is_some_and(|kbps| u64::from(kbps) * 1000 <= info.max_audio_bitrate);
    let http = |protocol: &str| protocol.trim().is_empty() || protocol.trim().eq_ignore_ascii_case("http");
    let channels = |max: u32| max == 0 || max >= CHANNELS;
    let fits_container = |profile: &DirectPlayProfile| {
        profile
            .containers
            .iter()
            .any(|listed| same_container(listed, container))
    };
    let fits_codec = |profile: &DirectPlayProfile| {
        profile.audio_codecs.is_empty()
            || profile
                .audio_codecs
                .iter()
                .any(|listed| listed.trim().eq_ignore_ascii_case(codec))
    };
    let source_stream = stream_json(container, codec, source.kbps.map(|kbps| u64::from(kbps) * 1000));
    let direct = info.direct_play_profiles.iter().any(|profile| {
        fits_container(profile)
            && fits_codec(profile)
            && (profile.protocols.is_empty() || profile.protocols.iter().any(|p| http(p)))
            && channels(profile.max_audio_channels)
    });
    if direct && rate_ok {
        return json!({
            "canDirectPlay": true,
            "canTranscode": false,
            "sourceStream": source_stream,
        });
    }
    let mut reasons: Vec<&str> = Vec::new();
    if !info.direct_play_profiles.iter().any(fits_container) {
        reasons.push("ContainerNotSupported");
    } else if !info
        .direct_play_profiles
        .iter()
        .any(|profile| fits_container(profile) && fits_codec(profile))
    {
        reasons.push("AudioCodecNotSupported");
    }
    if !rate_ok {
        reasons.push("AudioBitrateNotSupported");
    }
    let made = info.transcoding_profiles.iter().find_map(|profile| {
        let codec = match profile.audio_codec.trim().to_ascii_lowercase().as_str() {
            "mp3" => Codec::Mp3,
            "opus" => Codec::Opus,
            "aac" => Codec::Aac,
            _ => return None,
        };
        (http(&profile.protocol) && channels(profile.max_audio_channels)).then_some(codec)
    });
    let Some(codec) = made else {
        return json!({
            "canDirectPlay": false,
            "canTranscode": false,
            "transcodeReason": reasons,
            "errorReason": "No transcoding profile Octo can make (mp3, opus or aac)",
            "sourceStream": source_stream,
        });
    };
    let ceiling = if info.max_transcoding_audio_bitrate > 0 {
        info.max_transcoding_audio_bitrate
    } else {
        info.max_audio_bitrate
    };
    let kbps = if ceiling > 0 {
        u32::try_from(ceiling / 1000).unwrap_or(320).clamp(32, 320)
    } else {
        codec.default_kbps()
    };
    let (name, made_container) = match codec {
        Codec::Mp3 => ("mp3", "mp3"),
        Codec::Opus => ("opus", "ogg"),
        Codec::Aac => ("aac", "aac"),
    };
    json!({
        "canDirectPlay": false,
        "canTranscode": true,
        "transcodeReason": reasons,
        "transcodeParams": format!("octo1-{name}-{kbps}"),
        "sourceStream": source_stream,
        "transcodeStream": stream_json(made_container, name, Some(u64::from(kbps) * 1000)),
    })
}

fn stream_json(container: &str, codec: &str, bitrate: Option<u64>) -> Value {
    json!({
        "protocol": "http",
        "container": container,
        "codec": codec,
        "audioChannels": CHANNELS,
        "audioBitrate": bitrate,
    })
}

fn container_name(container: &Container) -> &'static str {
    match container {
        Container::Mp4 => "mp4",
        Container::Webm => "webm",
        Container::Ogg => "ogg",
        Container::Mpeg => "mp3",
        Container::Adts => "aac",
        Container::Flac => "flac",
        Container::Other => "",
    }
}

fn codec_name(codec: &SourceCodec) -> &'static str {
    match codec {
        SourceCodec::Aac => "aac",
        SourceCodec::Opus => "opus",
        SourceCodec::Mp3 => "mp3",
        SourceCodec::Vorbis => "vorbis",
        SourceCodec::Lossless => "flac",
        SourceCodec::Unknown => "",
    }
}

/// Whether a client's container name is ours (`m4a` is MP4).
fn same_container(listed: &str, ours: &str) -> bool {
    let listed = listed.trim().to_ascii_lowercase();
    !ours.is_empty() && (listed == ours || (ours == "mp4" && listed == "m4a"))
}

/// The ask `getTranscodeStream` carries in the `transcodeParams` Octo's decision made
/// (`octo1-<codec>-<kbps>`), with its `offset` in seconds; None for any other shape.
pub(crate) fn ask_from_params(params: &str, offset: &str) -> Option<AskOf> {
    let mut parts = params.trim().split('-');
    if parts.next()? != "octo1" {
        return None;
    }
    let codec = match parts.next()? {
        "mp3" => Codec::Mp3,
        "opus" => Codec::Opus,
        "aac" => Codec::Aac,
        _ => return None,
    };
    let kbps: u32 = parts
        .next()?
        .parse()
        .ok()
        .filter(|kbps| (32..=320).contains(kbps))?;
    if parts.next().is_some() {
        return None;
    }
    Some(AskOf {
        ask: Ask::Format(Target { codec, kbps }),
        offset: offset.trim().parse().unwrap_or(0),
        exact: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_with_an_end_from_the_first_byte_asks_for_a_length() {
        assert!(bounded_first_range(Some("bytes=0-1")));
        assert!(bounded_first_range(Some("Bytes=0-65535")));
        assert!(!bounded_first_range(Some("bytes=0-")));
        assert!(!bounded_first_range(Some("bytes=100-200")));
        assert!(!bounded_first_range(None));
        assert!(later_byte(Some("bytes=100-")));
        assert!(!later_byte(Some("bytes=0-")));
        assert!(!later_byte(None));
    }

    #[test]
    fn octos_own_transcode_params_read_back_and_nothing_else_does() {
        let ask = ask_from_params("octo1-mp3-128", "30").expect("ours");
        assert_eq!(
            ask.ask,
            Ask::Format(Target {
                codec: Codec::Mp3,
                kbps: 128
            })
        );
        assert_eq!(ask.offset, 30);
        for other in [
            "tp",
            "octo1-flac-128",
            "octo1-mp3-16",
            "octo1-mp3-128-x",
            "octo2-mp3-128",
            "",
        ] {
            assert!(ask_from_params(other, "").is_none(), "{other}");
        }
    }

    /// One Ogg page with `granule` and `body`.
    fn ogg_page(granule: u64, body: &[u8]) -> Vec<u8> {
        let mut page = b"OggS\0\0".to_vec();
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&[0; 12]);
        page.push(1);
        page.push(u8::try_from(body.len()).expect("one segment"));
        page.extend_from_slice(body);
        page
    }

    #[test]
    fn a_containers_opening_is_not_yet_sound() {
        let mut ogg = ogg_page(0, b"OpusHead");
        ogg.extend(ogg_page(0, b"OpusTags"));
        assert!(!holds_sound(&ogg, "audio/ogg"), "the header pages alone");
        assert!(!holds_sound(&ogg[..10], "audio/ogg"), "a cut header page");
        ogg.extend(ogg_page(960, b"sound"));
        assert!(holds_sound(&ogg, "audio/ogg"));
        assert!(holds_sound(&[0xFF; 8], "audio/ogg"), "bytes that are no Ogg page");
        let mut flac = b"fLaC".to_vec();
        flac.extend_from_slice(&[0x80, 0, 0, 2, 7, 7]);
        assert!(!holds_sound(&flac, "audio/flac"), "the metadata alone");
        flac.extend_from_slice(&[0xFF, 0xF8]);
        assert!(holds_sound(&flac, "audio/flac"));
        assert!(!holds_sound(&[], "audio/mpeg"));
        assert!(holds_sound(&[0xFF, 0xFB], "audio/mpeg"));
    }

    #[test]
    fn a_conversion_is_served_as_its_target() {
        assert_eq!(type_of_key("v-AAAAAAAAAA1-100.mp3-128"), "audio/mpeg");
        assert_eq!(type_of_key("v-AAAAAAAAAA1-100.aac-256"), "audio/aac");
        assert_eq!(type_of_key("v-AAAAAAAAAA1-100.opus-96"), "audio/ogg");
        assert_eq!(type_of_key("f-0123456789abcdef01234567.ogg-copy"), "audio/ogg");
    }

    #[test]
    fn a_seek_that_arrived_earlier_never_stops_the_plays_newer_one() {
        use std::time::Duration;
        let runs = Arc::new(SeekRuns::default());
        let now = Instant::now();
        let newer = runs.start("play", "song", now + Duration::from_millis(5));
        let stale = runs.start("play", "song", now);
        assert!(stale.token.is_cancelled(), "it arrived before the running seek");
        assert!(!newer.token.is_cancelled());
        drop(stale);
        let next = runs.start("play", "song", now + Duration::from_millis(10));
        assert!(
            newer.token.is_cancelled(),
            "the stale run's drop left the newer one listed"
        );
        assert!(!next.token.is_cancelled());
        let other = runs.start("another play", "song", now);
        assert!(!other.token.is_cancelled(), "plays never stop each other");
    }
}
