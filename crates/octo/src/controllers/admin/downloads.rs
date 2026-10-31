//! The download log and Find songs routes: the dashboard's side of what the Octo apps already
//! do, for admins whose listeners use other Subsonic apps. A download's full log on Fetched
//! songs, Find songs (a search run again by hand, every copy listed, one picked), and a held
//! download's Keep anyway and Stop waiting for Lidarr for one of the people it was held for.
//! Recently removed and Put back (`trash`, `trash/restore`) are the library edits' (see `trash`).
//!
//! Everything here names peers, files or people, or changes the library, so every call needs a
//! Navidrome admin's own dashboard sign-in (never the recovery code) and acts as that person:
//! their Find songs limits, and for a library file the library actions gates (switched on, on
//! the allowed list, dry run only rehearses), exactly as in the apps.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use octo_core::common::dotnet::{self, is_blank};
use octo_core::soulseek::RoutingKind;
use octo_media::cover::cover_image;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::json_body::bind_optional_body;
use super::json_body::{FromBody, Kind, Member, Shape, text_record};
use super::plumbing::{BROWSE_COOKIE_NAME, BROWSE_TOKEN_HEADER, request_cookie};
use super::request_binding::{error, ok, query, sign_in, split};
use crate::app::AppState;
use crate::controllers::subsonic::downloads::{
    NAME_THE_PERSON, NOTHING_WAITING, NOTHING_WAITS_FOR_LIDARR, STOP_NAME_THE_PERSON,
};
use crate::http::error::problem;
use crate::http::routes::RouteSet;
use crate::services::common::acquisition_tracker::AcquisitionSnapshot;
use crate::services::common::heart_acquisition_coordinator::KeepAnyway;
use crate::services::lidarr::StopWaiting;
use crate::services::local::DownloadHistoryService;
use crate::services::soulseek::SoulseekMetadataService;
use crate::services::subsonic::subsonic_response_builder::{
    acquisition_json, event_json, found_songs_json, pick_json,
};

/// Outside songs a search on the Find songs page lists.
pub const OUTSIDE_LIMIT: usize = 20;

const COVER_SIDE: u32 = 160;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/downloads/log", get(download_log))
        .route("/api/admin/acquisitions/holders", get(held_for))
        .route("/api/admin/acquisitions/keep", post(keep_anyway))
        .route("/api/admin/acquisitions/stop-waiting", post(stop_waiting))
        .route("/api/admin/find", post(start_find))
        .route("/api/admin/find/outside", get(outside_songs))
        .route("/api/admin/find/cover", get(find_cover))
        .route("/api/admin/find/{search}", get(get_find))
        .route("/api/admin/find/{search}/pick", post(pick_found))
}

/// The Navidrome admin signed in on this browser, or None (no sign-in, or the recovery code).
fn admin(state: &AppState, headers: &axum::http::HeaderMap) -> Option<String> {
    let token = request_cookie(headers, BROWSE_COOKIE_NAME).or_else(|| {
        headers
            .get(BROWSE_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    });
    state.browse_sessions.navidrome_user_of(token.as_deref())
}

// ---- A download's log ---------------------------------------------------------------------

/// One download's log, by its row key. With `at` (a Fetched songs entry's downloadedAt), the log
/// saved with that entry wins; otherwise, or when none was saved, the live row's log, when the
/// live list still has this run of it. kept says which: "saved", "live", or "none".
pub async fn download_log(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = split(request).await;
    if admin(&state, &parts.headers).is_none() {
        return sign_in();
    }
    let Some(key) = query(&parts.uri, "key").filter(|k| !is_blank(k)) else {
        return error(StatusCode::BAD_REQUEST, "Which download? The key is missing.");
    };
    let key = key.trim().to_string();
    let at = query(&parts.uri, "at").filter(|a| !is_blank(a));

    let entry = at.and_then(|at| state.download_history.find(&key, Some(at.trim())));
    if let Some(saved) = entry
        .as_ref()
        .and_then(|e| e.log.as_ref())
        .filter(|log| !log.is_empty())
    {
        let lines: Vec<Value> = saved.iter().map(|line| Value::Object(event_json(line))).collect();
        return ok(&json!({ "key": key, "kept": "saved", "row": null, "event": lines }));
    }

    let row: Option<AcquisitionSnapshot> = state.acquisition_tracker.detail(&key, None);
    // A live row that began after this entry was written is a later download of the same song.
    let same_run = row.as_ref().is_some_and(|row| match &entry {
        None => true,
        Some(entry) => DownloadHistoryService::written_at(entry)
            .is_none_or(|written| row.started_at <= written + chrono::TimeDelta::seconds(1)),
    });
    if let Some(row) = row.filter(|_| same_run) {
        let mut json = acquisition_json(&row);
        json.insert("provider".into(), json!(row.provider));
        json.insert("externalId".into(), json!(row.external_id));
        let lines: Vec<Value> = row
            .events
            .iter()
            .flatten()
            .map(|line| Value::Object(event_json(line)))
            .collect();
        return ok(&json!({ "key": key, "kept": "live", "row": json, "event": lines }));
    }
    ok(&json!({ "key": key, "kept": "none", "row": null, "event": [] }))
}

// ---- Keep anyway and Stop waiting for Lidarr ------------------------------------------------

/// `HeldRequest(string? Key, string? User)`: a held download's row key and the person to act for.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HeldRequest {
    key: Option<String>,
    user: Option<String>,
}

impl FromBody for HeldRequest {
    const SHAPE: &'static Shape = text_record!(
        "Octo.Controllers.AdminDownloadsController+HeldRequest",
        "key",
        "user"
    );
}

/// The row key and the person a Keep anyway or Stop waiting names, trimmed, or the answer that
/// says which is missing (`unnamed` when it is the person).
fn held_request(request: Option<HeldRequest>, unnamed: &str) -> Result<(String, String), Response> {
    let (key, user) = request.map_or((None, None), |r| (r.key, r.user));
    let Some(key) = key.filter(|key| !is_blank(key)) else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Which download? The key is missing.",
        ));
    };
    let Some(user) = user.filter(|user| !is_blank(user)) else {
        return Err(error(StatusCode::BAD_REQUEST, unnamed));
    };
    Ok((key.trim().to_string(), user.trim().to_string()))
}

/// The holder menus of one held row: who may keep it anyway now (`keepers`) and whose kept copy
/// of its song waits for Lidarr (`stopWaiters`), each in the order they joined.
pub async fn held_for(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = split(request).await;
    if admin(&state, &parts.headers).is_none() {
        return sign_in();
    }
    let Some(key) = query(&parts.uri, "key").filter(|k| !is_blank(k)) else {
        return error(StatusCode::BAD_REQUEST, "Which download? The key is missing.");
    };
    let key = key.trim().to_string();
    let keepers = state.heart_acquisition_coordinator.keepers(&key);
    let stop_waiters = key
        .split_once(':')
        .map(|(provider, id)| state.lidarr_hearts.stop_waiters(provider, id))
        .unwrap_or_default();
    ok(&json!({ "key": key, "keepers": keepers, "stopWaiters": stop_waiters }))
}

/// Keep anyway (Keep these anyway on a hearted album's row) for one person waiting in the row's
/// holds, the admin's own way to what keepAcquisitionAnyway does for the apps with `user`: the
/// keep is saved with the holds before this answers a plain ok, then carried out. A conflict when
/// nothing of the row waits to be kept for that person.
pub async fn keep_anyway(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: Option<HeldRequest> = match bind_optional_body(&parts.headers, &body) {
        Ok(request) => request,
        Err(answer) => return answer,
    };
    let Some(admin) = admin(&state, &parts.headers) else {
        return sign_in();
    };
    let (key, user) = match held_request(request, NAME_THE_PERSON) {
        Ok(named) => named,
        Err(answer) => return answer,
    };
    match state.heart_acquisition_coordinator.keep_anyway(&key, &user) {
        KeepAnyway::Keeping => {
            info!("{admin} kept {key} anyway for {user} from the dashboard");
            ok(&json!({ "status": "ok" }))
        }
        _ => error(StatusCode::CONFLICT, NOTHING_WAITING),
    }
}

/// Stop waiting for Lidarr for one person whose kept copy of the row's song waits for an open
/// Lidarr follow, the admin's own way to what stopWaitingForLidarr does for the apps with `user`:
/// every follow that copy waits for ends and it lands now. A conflict when no copy of that
/// person's waits.
pub async fn stop_waiting(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: Option<HeldRequest> = match bind_optional_body(&parts.headers, &body) {
        Ok(request) => request,
        Err(answer) => return answer,
    };
    let Some(admin) = admin(&state, &parts.headers) else {
        return sign_in();
    };
    let (key, user) = match held_request(request, STOP_NAME_THE_PERSON) {
        Ok(named) => named,
        Err(answer) => return answer,
    };
    let Some((provider, id)) = key.split_once(':') else {
        return error(StatusCode::CONFLICT, NOTHING_WAITS_FOR_LIDARR);
    };
    let hearts = &state.lidarr_hearts;
    let waits = hearts
        .stop_waiters(provider, id)
        .iter()
        .any(|waiter| dotnet::eq_ignore_case(waiter.trim(), &user));
    if !waits {
        return error(StatusCode::CONFLICT, NOTHING_WAITS_FOR_LIDARR);
    }
    match hearts.stop_waiting_for_lidarr(provider, id).await {
        StopWaiting::Stopped => {
            info!("{admin} stopped waiting for Lidarr on {key} for {user} from the dashboard");
            ok(&json!({ "status": "ok" }))
        }
        StopWaiting::NothingWaits => error(StatusCode::CONFLICT, NOTHING_WAITS_FOR_LIDARR),
    }
}

// ---- Find songs ---------------------------------------------------------------------------

/// `FindRequest(string? Id)`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FindRequest {
    id: Option<String>,
}

impl FromBody for FindRequest {
    const SHAPE: &'static Shape = text_record!("Octo.Controllers.AdminDownloadsController+FindRequest", "id");
}

/// `PickRequest(string? Copy, bool Anyway)`: `anyway` as the apps' pickFoundSong takes it, left out
/// by a page that cannot ask.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PickRequest {
    copy: Option<String>,
    anyway: Option<bool>,
}

impl FromBody for PickRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.AdminDownloadsController+PickRequest",
        by_constructor: true,
        members: &[
            Member {
                name: "copy",
                kind: Kind::Text,
            },
            Member {
                name: "anyway",
                kind: Kind::Bool,
            },
        ],
    };
}

/// Starts a Find songs search for one song (an outside song's id or key, or a library song's
/// id) as the signed-in admin, on their download sources, and answers at once while it runs.
pub async fn start_find(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: Option<FindRequest> = match bind_optional_body(&parts.headers, &body) {
        Ok(request) => request,
        Err(answer) => return answer,
    };
    let Some(user) = admin(&state, &parts.headers) else {
        return sign_in();
    };
    let Some(id) = request.and_then(|r| r.id).filter(|id| !is_blank(id)) else {
        return error(StatusCode::BAD_REQUEST, "Which song? The id is missing.");
    };
    let Some(found) = state
        .song_finder
        .start(id.trim(), &user, &CancellationToken::new())
        .await
    else {
        return error(StatusCode::NOT_FOUND, "Octo could not find that song.");
    };
    info!(
        "{user} started Find songs for '{} - {}' from the dashboard",
        found.target.artist, found.target.title
    );
    ok(&found_songs_json(&found))
}

/// A Find songs search as it stands, for the admin who started it.
pub async fn get_find(
    State(state): State<AppState>,
    Path(search): Path<String>,
    request: Request,
) -> Response {
    let (parts, _) = split(request).await;
    let Some(user) = admin(&state, &parts.headers) else {
        return sign_in();
    };
    match state.song_finder.get(&search, &user) {
        Some(found) => ok(&found_songs_json(&found)),
        None => error(StatusCode::NOT_FOUND, "This search is gone. Search again."),
    }
}

/// Fetches exactly one copy the search listed, by its id there. A library song's copy goes
/// through Better quality, so only someone on the library actions allowed list may pick one, and
/// the original stays until the new file passes every check. `anyway` asks first and then
/// replaces anyway, as pickFoundSong's does.
pub async fn pick_found(
    State(state): State<AppState>,
    Path(search): Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = split(request).await;
    let request: Option<PickRequest> = match bind_optional_body(&parts.headers, &body) {
        Ok(request) => request,
        Err(answer) => return answer,
    };
    let Some(user) = admin(&state, &parts.headers) else {
        return sign_in();
    };
    let (copy, anyway) = request.map_or((None, None), |r| (r.copy, r.anyway));
    let Some(copy) = copy.filter(|copy| !is_blank(copy)) else {
        return error(StatusCode::BAD_REQUEST, "Which copy? The copy is missing.");
    };
    let outcome = state
        .song_finder
        .pick_copy(&search, copy.trim(), &user, anyway)
        .await;
    ok(&pick_json(&outcome))
}

/// Songs Octo can find outside the library for this search, the ones the apps' search lists
/// under the library's (Last.fm, with Deezer's covers). Empty, with available false, when there
/// is no Last.fm key to search with.
pub async fn outside_songs(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = split(request).await;
    if admin(&state, &parts.headers).is_none() {
        return sign_in();
    }
    let available = state.last_fm.has_api_key();
    let q = query(&parts.uri, "q").filter(|q| !is_blank(q));
    let Some(q) = q.filter(|_| available) else {
        return ok(&json!({ "available": available, "songs": [] }));
    };
    let songs = state.external_search.get(q.trim()).await;
    let songs: Vec<Value> = songs
        .iter()
        .take(OUTSIDE_LIMIT)
        .map(|song| {
            json!({
                "id": song
                    .external_id
                    .clone()
                    .filter(|id| !is_blank(id))
                    .unwrap_or_else(|| song.id.clone()),
                "title": song.title,
                "artist": song.artist,
                "album": (!is_blank(&song.album)).then(|| song.album.clone()),
                "duration": song.duration,
                "coverUrl": song.cover_art_url.clone().or_else(|| song.cover_art_url_large.clone()),
            })
        })
        .collect();
    ok(&json!({ "available": available, "songs": songs }))
}

/// A small cover for a song on the Find songs page: a library song's own picture, read from its
/// file, or for an outside song the catalog's cover (a redirect to it).
pub async fn find_cover(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = split(request).await;
    if admin(&state, &parts.headers).is_none() {
        return sign_in();
    }
    let Some(id) = query(&parts.uri, "id").filter(|id| !is_blank(id)) else {
        return problem(StatusCode::NOT_FOUND);
    };
    let mut raw = id.trim().to_string();
    let prefix = format!("{}:", SoulseekMetadataService::PROVIDER_NAME);
    if dotnet::starts_with_ignore_case(&raw, &prefix) {
        raw = raw[prefix.len()..].to_string();
    }
    if raw.starts_with("ext-")
        && let (true, _, Some(outside)) = state.local_library.parse_song_id(&raw)
    {
        raw = outside;
    }

    let routing = state
        .external_id_registry
        .lookup(&raw)
        .map(|shared| shared.snapshot())
        .or_else(|| SoulseekMetadataService::try_decode_external_id(Some(&raw)));
    if let Some(routing) = routing.filter(|r| r.kind == RoutingKind::Song && r.has_artist_title()) {
        let meta = state
            .deezer_metadata
            .enrich_track(
                routing.artist.as_deref().unwrap_or(""),
                routing.title.as_deref().unwrap_or(""),
                false,
                false,
            )
            .await;
        let cover = meta
            .and_then(|m| m.album_cover_url)
            .filter(|url| !url.is_empty())
            .and_then(|url| url::Url::parse(&url).ok())
            .filter(|url| matches!(url.scheme(), "https" | "http"));
        return match cover {
            Some(cover) => {
                let mut res = Response::new(Body::empty());
                *res.status_mut() = StatusCode::FOUND;
                if let Ok(location) = HeaderValue::from_str(cover.as_str()) {
                    res.headers_mut().insert(header::LOCATION, location);
                }
                res.headers_mut().insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("private, max-age=3600"),
                );
                res
            }
            None => problem(StatusCode::NOT_FOUND),
        };
    }

    let Some(file) = state.navidrome_song_path_resolver.resolve(&raw).await else {
        return problem(StatusCode::NOT_FOUND);
    };
    if !std::path::Path::new(&file.absolute_path).is_file() {
        return problem(StatusCode::NOT_FOUND);
    }
    let path = file.absolute_path.clone();
    let picture = tokio::task::spawn_blocking(move || {
        let tags = octo_media::tags::TagFile::open(&path).ok()?;
        octo_media::tags::front_cover(&tags).map(|picture| picture.data)
    })
    .await
    .ok()
    .flatten()
    .filter(|bytes| !bytes.is_empty());
    let Some(bytes) = picture else {
        debug!("No picture in {}", file.absolute_path);
        return problem(StatusCode::NOT_FOUND);
    };
    let jpeg = cover_image::to_jpeg(&cover_image::fit_within(&bytes, COVER_SIDE));
    let mut res = Response::new(Body::from(jpeg));
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    res
}
