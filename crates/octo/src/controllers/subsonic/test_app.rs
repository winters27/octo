//! What the Subsonic controller tests share: the assembled app (the pipeline around the
//! Subsonic routes) over a wiremock Navidrome, and fakes for the services those tests replace.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use octo_core::models::domain::Song;
use octo_core::models::download::DownloadInfo;
use octo_core::models::subsonic::ScanStatus;
use octo_core::settings::{AppSettings, DownloadSource, SubsonicSettings};
use parking_lot::Mutex;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::{MockServer, Request as MockRequest, ResponseTemplate};

use crate::app::{AppInner, AppState};
use crate::http::pipeline::{App, build_with};
use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::ReplacementHandoff;
use crate::services::local::i_local_library_service::{ParsedExternalId, ParsedSongId};
use crate::services::local::{ILocalLibraryService, LocalSongMapping};

/// One answer, read in full.
pub(crate) struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// `subsonic-response` of a JSON answer.
    pub fn envelope(&self) -> Value {
        self.json()["subsonic-response"].clone()
    }

    /// The media type of the answer, without parameters.
    pub fn media_type(&self) -> Option<String> {
        self.header("content-type")
            .map(|ct| ct.split(';').next().unwrap_or("").trim().to_string())
    }
}

/// Settings pointing Octo at `url` (the wiremock Navidrome) as `Subsonic:Url`.
pub(crate) fn settings(url: &str) -> AppSettings {
    AppSettings {
        subsonic: SubsonicSettings {
            url: Some(url.to_string()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// `AppState::for_tests`, with some of its services swapped. Only the controller's own view
/// changes: services built over the originals keep them.
pub(crate) fn state_with(settings: AppSettings, change: impl FnOnce(&mut AppInner)) -> AppState {
    let state = AppState::for_tests(settings);
    let Ok(mut inner) = Arc::try_unwrap(state.inner) else {
        panic!("a fresh test state is held once");
    };
    change(&mut inner);
    AppState {
        inner: Arc::new(inner),
    }
}

/// The app as a client sees it: the middleware around the Subsonic routes and the catch-all.
pub(crate) fn app(state: &AppState) -> App {
    build_with(state.clone(), super::routes())
}

/// Sends one request through the app and reads the answer.
pub(crate) async fn send(
    app: &App,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: impl Into<Body>,
) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(body.into()).expect("a valid request"))
        .await
        .expect("the app is infallible");
    let (parts, body) = response.into_parts();
    let body = body.collect().await.expect("the body reads").to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body,
    }
}

pub(crate) async fn get(app: &App, uri: &str) -> Reply {
    send(app, Method::GET, uri, &[], Body::empty()).await
}

/// A Subsonic JSON answer as Navidrome writes it.
pub(crate) fn navidrome_json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

pub(crate) const NAVIDROME_OK: &str =
    r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"#;
pub(crate) const NAVIDROME_WRONG_PASSWORD: &str = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#;

/// The query of a request Navidrome received, as a map (a repeated key keeps its last value).
pub(crate) fn query_of(request: &MockRequest) -> HashMap<String, String> {
    request.url.query_pairs().into_owned().collect()
}

/// The paths of every request the mock server received, in order.
pub(crate) async fn received_paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect()
}

/// A Subsonic sign-in: `u`, the token `t` (Navidrome stand-ins accept `good`), a salt, the
/// version and the client.
pub(crate) fn auth(user: &str, token: &str, client: &str) -> String {
    format!("u={user}&t={token}&s=salt&v=1.16.1&c={client}")
}

// ---------------------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------------------

/// What a [`FakeStreams`] answers for a direct stream.
pub(crate) type StreamAnswer = Box<dyn Fn() -> Option<DirectStreamInfo> + Send + Sync>;

/// A download service as the playback tests set it up: a direct stream (or none) for one
/// provider and id, and a record of what was asked.
#[derive(Default)]
pub(crate) struct FakeStreams {
    /// `(provider, external id)` → the stream to answer with.
    answers: Mutex<HashMap<(String, String), Arc<StreamAnswer>>>,
    /// Every `get_direct_stream` call: provider, external id, the Range passed on.
    pub stream_calls: Mutex<Vec<(String, String, Option<String>)>>,
    pub download_and_stream_calls: Mutex<usize>,
}

impl FakeStreams {
    /// Answers `bytes` as a 200 of `content_type` for this provider and id.
    pub fn streaming(
        provider: &str,
        external_id: &str,
        bytes: &'static [u8],
        content_type: &'static str,
    ) -> Self {
        let fake = FakeStreams::default();
        fake.answers.lock().insert(
            (provider.to_string(), external_id.to_string()),
            Arc::new(Box::new(move || {
                let chunk: std::io::Result<Bytes> = Ok(Bytes::from_static(bytes));
                Some(DirectStreamInfo {
                    audio_stream: Box::pin(futures::stream::iter([chunk])),
                    content_type: content_type.to_string(),
                    content_length: Some(bytes.len() as u64),
                    quality: None,
                    status_code: 200,
                    content_range: None,
                    video_id: None,
                    itag: None,
                })
            })),
        );
        fake
    }

    /// Answers this provider and id with `answer` from now on.
    pub fn answer_with(&self, provider: &str, external_id: &str, answer: StreamAnswer) {
        self.answers
            .lock()
            .insert((provider.to_string(), external_id.to_string()), Arc::new(answer));
    }

    pub fn stream_calls(&self) -> Vec<(String, String, Option<String>)> {
        self.stream_calls.lock().clone()
    }
}

#[async_trait]
impl IDownloadService for FakeStreams {
    async fn download_song(&self, _: &str, _: &str, _: &CancellationToken) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_and_stream(
        &self,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        *self.download_and_stream_calls.lock() += 1;
        anyhow::bail!("not set up")
    }

    fn download_remaining_album_tracks_in_background(&self, _: &str, _: &str, _: &str) {}

    async fn execute_acquisition(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: bool,
        _: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        _: bool,
        _: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_album_with_source(
        &self,
        _: &str,
        _: &str,
        _: DownloadSource,
        _: bool,
        _: &CancellationToken,
        _: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        anyhow::bail!("not set up")
    }

    fn get_download_status(&self, _: &str) -> Option<DownloadInfo> {
        None
    }

    async fn get_local_path_if_exists(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn get_direct_stream(
        &self,
        provider: &str,
        external_id: &str,
        range_header: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        self.stream_calls.lock().push((
            provider.to_string(),
            external_id.to_string(),
            range_header.map(str::to_string),
        ));
        let answer = self
            .answers
            .lock()
            .get(&(provider.to_string(), external_id.to_string()))
            .cloned();
        Ok(answer.and_then(|answer| answer()))
    }
}

/// A local library service: ids it was told are external parse as such; everything else is a
/// library id.
#[derive(Default)]
pub(crate) struct FakeLibrary {
    external: Mutex<HashMap<String, (String, String)>>,
    pub local_paths: Mutex<HashMap<(String, String), String>>,
}

impl FakeLibrary {
    pub fn with_external(id: &str, provider: &str, external_id: &str) -> Self {
        let fake = FakeLibrary::default();
        fake.add_external(id, provider, external_id);
        fake
    }

    pub fn add_external(&self, id: &str, provider: &str, external_id: &str) {
        self.external
            .lock()
            .insert(id.to_string(), (provider.to_string(), external_id.to_string()));
    }
}

#[async_trait]
impl ILocalLibraryService for FakeLibrary {
    async fn get_local_path_for_external_song(&self, provider: &str, external_id: &str) -> Option<String> {
        self.local_paths
            .lock()
            .get(&(provider.to_string(), external_id.to_string()))
            .cloned()
    }

    async fn register_downloaded_song(&self, _: &Song, _: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_local_id_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    fn parse_song_id(&self, song_id: &str) -> ParsedSongId {
        match self.external.lock().get(song_id) {
            Some((provider, external_id)) => (true, Some(provider.clone()), Some(external_id.clone())),
            None => (false, None, None),
        }
    }

    fn parse_external_id(&self, _: &str) -> ParsedExternalId {
        (false, None, None, None)
    }

    async fn get_mappings(&self) -> Vec<LocalSongMapping> {
        Vec::new()
    }

    async fn find_mapping_by_tags(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Option<LocalSongMapping> {
        None
    }

    async fn forget_mapping(&self, _: &str) -> anyhow::Result<Vec<crate::services::local::LocalSongMapping>> {
        Ok(Vec::new())
    }

    async fn trigger_library_scan(&self, _: bool) -> bool {
        false
    }

    async fn get_scan_status(&self) -> Option<ScanStatus> {
        None
    }
}
