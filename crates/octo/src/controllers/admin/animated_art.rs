//! The dashboard's Animated art card. Behind the sign-in gate like every admin route.
//!
//! - `GET /api/admin/animated-art`: the mode, how many albums are known, found and hidden,
//!   and the library sweep's progress.
//! - `GET /api/admin/animated-art/albums?offset=&limit=`: a page of the albums with animated
//!   art (key, artist, album, the still, whether a tall loop is there, whether hidden), by
//!   artist then album, and `total`.
//! - `GET /api/admin/animated-art/still?key=&size=`: an album's still as a JPEG, fetched by
//!   Octo, so the browser never loads it from Apple.
//! - `POST /api/admin/animated-art/sweep`: look the library up now.
//! - `POST /api/admin/animated-art/hide` with `{"key": ..., "hidden": true|false}`: hide or
//!   show one album's animated art in every app.

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use super::plumbing::{accepted, cover_reply, error_json, ok, query_value};
use crate::app::AppState;
use crate::http::error::problem;
use crate::http::routes::RouteSet;
use crate::services::admin::fetched_covers::side_for;

/// The most albums one page lists.
const MAX_PAGE: usize = 200;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/animated-art", get(get_animated_art_summary))
        .route("/api/admin/animated-art/albums", get(get_animated_art_albums))
        .route("/api/admin/animated-art/still", get(get_animated_art_still))
        .route("/api/admin/animated-art/sweep", post(sweep_animated_art))
        .route("/api/admin/animated-art/hide", post(hide_animated_art))
}

pub async fn get_animated_art_summary(State(state): State<AppState>) -> Response {
    ok(&state.animated_art.summary())
}

pub async fn get_animated_art_albums(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let number = |name: &str, default: usize| {
        query_value(query.as_deref(), name)
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(default)
    };
    let (offset, limit) = (number("offset", 0), number("limit", 60).clamp(1, MAX_PAGE));
    let (total, albums) = state.animated_art.found_page(offset, limit);
    ok(&json!({ "total": total, "offset": offset, "albums": albums }))
}

pub async fn get_animated_art_still(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let Some(still) =
        query_value(query.as_deref(), "key").and_then(|key| state.animated_art.still_of(key.trim()))
    else {
        return problem(StatusCode::NOT_FOUND);
    };
    let side = side_for(query_value(query.as_deref(), "size").as_deref());
    match state.fetched_covers.outside_cover(&still, side).await {
        Some(picture) => cover_reply(picture),
        None => problem(StatusCode::NOT_FOUND),
    }
}

pub async fn sweep_animated_art(State(state): State<AppState>) -> Response {
    if !state.animated_art.offered() {
        return error_json(StatusCode::CONFLICT, "Animated art is off.");
    }
    state.animated_art.sweep_now();
    accepted(&json!({ "ok": true }))
}

#[derive(Deserialize)]
struct Hide {
    key: String,
    hidden: bool,
}

pub async fn hide_animated_art(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(hide) = serde_json::from_slice::<Hide>(&body) else {
        return error_json(StatusCode::BAD_REQUEST, "Send a key and hidden.");
    };
    if !state.animated_art.set_hidden(&hide.key, hide.hidden) {
        return error_json(StatusCode::NOT_FOUND, "No animated art is kept for that album.");
    }
    ok(&json!({ "ok": true, "key": hide.key, "hidden": hide.hidden }))
}
