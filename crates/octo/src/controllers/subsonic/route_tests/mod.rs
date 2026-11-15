//! Handler tests for the Subsonic routes, through the assembled pipeline (`tower::oneshot`)
//! against Navidrome and the other upstreams stood in for by wiremock.

mod folders;
mod held_playlists;
mod native;
mod native_held;
mod radio;
mod search_paging;
mod similar_songs;
mod sync_walks;
mod various;

use std::collections::HashMap;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use http_body_util::BodyExt;
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::search::SearchResult;
use octo_core::models::subsonic::ExternalPlaylist;
use parking_lot::Mutex;
use tower::ServiceExt;

use crate::app::AppState;
use crate::http::pipeline::{App, build_with};
use crate::services::i_music_metadata_service::IMusicMetadataService;

/// The application with every route of the controller.
pub(super) fn app(state: AppState) -> App {
    build_with(state, super::routes())
}

pub(super) struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

pub(super) async fn send(
    app: &App,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("infallible");
    let (parts, body) = response.into_parts();
    let body = body.collect().await.expect("body").to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

pub(super) async fn get(app: &App, uri: &str) -> Reply {
    send(app, Method::GET, uri, &[], Body::empty()).await
}

pub(super) async fn get_string(app: &App, uri: &str) -> String {
    let reply = get(app, uri).await;
    assert!(
        reply.status.is_success(),
        "{uri}: {} {}",
        reply.status,
        reply.body
    );
    reply.body
}

type ArtistTitle = Box<dyn Fn(&str, &str, Option<i32>) -> Vec<Song> + Send + Sync>;

/// A metadata service that answers what a test sets and records what the controller asked.
#[derive(Default)]
pub(crate) struct TestMetadata {
    /// `search_songs_by_artist_title`; nothing when unset.
    pub by_artist_title: Mutex<Option<ArtistTitle>>,
    /// The `top_n` of every `prewarm_you_tube_ids` call.
    pub prewarms: Mutex<Vec<usize>>,
    /// The query of every `search_albums` call.
    pub album_searches: Mutex<Vec<String>>,
    pub songs: Mutex<HashMap<String, Song>>,
    /// When set, every `get_song` first waits for a permit from it (and gives it back).
    pub song_gate: Mutex<Option<std::sync::Arc<tokio::sync::Semaphore>>>,
}

impl TestMetadata {
    pub fn answering(answer: impl Fn(&str, &str, Option<i32>) -> Vec<Song> + Send + Sync + 'static) -> Self {
        let metadata = TestMetadata::default();
        *metadata.by_artist_title.lock() = Some(Box::new(answer));
        metadata
    }
}

#[async_trait]
impl IMusicMetadataService for TestMetadata {
    async fn search_songs(&self, _: &str, _: i32) -> Vec<Song> {
        Vec::new()
    }

    async fn search_songs_by_artist_title(
        &self,
        artist: &str,
        title: &str,
        _: i32,
        duration: Option<i32>,
    ) -> Vec<Song> {
        match self.by_artist_title.lock().as_ref() {
            Some(answer) => answer(artist, title, duration),
            None => Vec::new(),
        }
    }

    async fn prewarm_you_tube_ids(&self, _songs: &[Song], top_n: usize) {
        self.prewarms.lock().push(top_n);
    }

    async fn search_albums(&self, query: &str, _: i32) -> Vec<Album> {
        self.album_searches.lock().push(query.to_string());
        Vec::new()
    }

    async fn search_artists(&self, _: &str, _: i32) -> Vec<Artist> {
        Vec::new()
    }

    async fn search_all(&self, _: &str, _: i32, _: i32, _: i32) -> SearchResult {
        SearchResult::default()
    }

    async fn get_song(&self, _: &str, id: &str) -> Option<Song> {
        let gate = self.song_gate.lock().clone();
        if let Some(gate) = gate {
            let _permit = gate.acquire().await;
        }
        self.songs.lock().get(id).cloned()
    }

    async fn get_album(&self, _: &str, _: &str) -> Option<Album> {
        None
    }

    async fn get_artist(&self, _: &str, _: &str) -> Option<Artist> {
        None
    }

    async fn get_artist_albums(&self, _: &str, _: &str) -> Vec<Album> {
        Vec::new()
    }

    async fn search_playlists(&self, _: &str, _: i32) -> Vec<ExternalPlaylist> {
        Vec::new()
    }

    async fn get_playlist(&self, _: &str, _: &str) -> Option<ExternalPlaylist> {
        None
    }

    async fn get_playlist_tracks(&self, _: &str, _: &str) -> Vec<Song> {
        Vec::new()
    }
}

/// Waits up to ten seconds for a condition.
pub(super) async fn until(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{what} never happened");
}

pub(super) fn query_value(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}
