//! Radio stations as Subsonic playlists, as internet radio stations with an opaque stream URL,
//! the stream itself, and as Navidrome-native playlists.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, StatusCode};
use chrono::{TimeDelta, TimeZone, Utc};
use futures::StreamExt;
use http_body_util::BodyExt;
use octo_core::last_fm::RadioAudioProfile;
use octo_core::last_fm::last_fm_radio_state_store::station_id;
use octo_core::last_fm::last_fm_radio_stream_service::READY_POOL_SIZE;
use octo_core::models::domain::Song;
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioStationKind, LastFmRadioTrack};
use octo_core::settings::{AppSettings, ExplicitFilter, LastFmSettings, SubsonicSettings};
use parking_lot::Mutex;
use tempfile::TempDir;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::{Reply, TestMetadata, app, get, get_string, query_value, send, until};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;
use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::last_fm::icy_metadata_stream::DEFAULT_INTERVAL;
use crate::services::last_fm::{ILastFmRadioAudioTranscoder, IRadioTuneInSelector};
use crate::services::library::ReplacementHandoff;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::DownloadSource;

/// A `TaskCompletionSource`: set once, awaited by anyone.
struct Gate(watch::Sender<bool>);

impl Gate {
    fn new() -> Arc<Gate> {
        Arc::new(Gate(watch::channel(false).0))
    }

    fn set(&self) {
        self.0.send_replace(true);
    }

    async fn wait(&self) {
        let mut seen = self.0.subscribe();
        let _ = seen.wait_for(|set| *set).await;
    }

    fn is_set(&self) -> bool {
        *self.0.borrow()
    }
}

/// A radio transcoder a test can hold at its gates, make fail a set number of times, and count.
struct BlockingTranscoder {
    last_bitrate_kbps: AtomicI32,
    failures_before_success: AtomicUsize,
    complete_calls: AtomicUsize,
    calls: AtomicUsize,
    started: Mutex<Arc<Gate>>,
    before_write_gate: Mutex<Option<Arc<Gate>>>,
    completion_gate: Mutex<Option<Arc<Gate>>>,
}

impl BlockingTranscoder {
    fn new() -> Self {
        BlockingTranscoder {
            last_bitrate_kbps: AtomicI32::new(0),
            failures_before_success: AtomicUsize::new(0),
            complete_calls: AtomicUsize::new(READY_POOL_SIZE),
            calls: AtomicUsize::new(0),
            started: Mutex::new(Gate::new()),
            before_write_gate: Mutex::new(None),
            completion_gate: Mutex::new(None),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn reset_started(&self) {
        *self.started.lock() = Gate::new();
    }

    fn started(&self) -> Arc<Gate> {
        self.started.lock().clone()
    }
}

#[async_trait]
impl ILastFmRadioAudioTranscoder for BlockingTranscoder {
    async fn transcode_to_mp3(
        &self,
        _input: AudioStream,
        output: &mut (dyn AsyncWrite + Unpin + Send),
        bitrate_kbps: i32,
        _target_lufs: Option<f64>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<RadioAudioProfile>> {
        self.last_bitrate_kbps.store(bitrate_kbps, Ordering::SeqCst);
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.started().set();
        let before = self.before_write_gate.lock().clone();
        if let Some(gate) = before {
            tokio::select! {
                () = gate.wait() => {}
                () = cancellation_token.cancelled() => anyhow::bail!("The operation was canceled."),
            }
        }
        let failures = self.failures_before_success.load(Ordering::SeqCst);
        if call <= failures {
            anyhow::bail!("fixture source failed");
        }
        output.write_all(b"MP3").await?;
        output.flush().await?;
        let completion = self.completion_gate.lock().clone();
        if let Some(gate) = completion {
            tokio::select! {
                () = gate.wait() => {}
                () = cancellation_token.cancelled() => anyhow::bail!("The operation was canceled."),
            }
        }
        if call <= failures + self.complete_calls.load(Ordering::SeqCst) {
            return Ok(None);
        }
        cancellation_token.cancelled().await;
        anyhow::bail!("The operation was canceled.")
    }
}

/// A download service whose direct stream comes from the yt-dlp shim (`external-source-audio`),
/// standing in for the real one.
struct ShimDownloads;

#[async_trait]
impl IDownloadService for ShimDownloads {
    async fn download_song(&self, _: &str, _: &str, _: &CancellationToken) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_and_stream(
        &self,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
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
        Ok(false)
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
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        Ok(Some(DirectStreamInfo {
            audio_stream: Box::pin(futures::stream::iter([Ok(bytes::Bytes::from_static(
                b"external-source-audio",
            ))])),
            content_type: "audio/mp4".into(),
            content_length: None,
            quality: None,
            status_code: 200,
            content_range: None,
        }))
    }
}

/// Tune-in start the tests can pin. 0 keeps snapshot order, the pre-rotation behavior.
#[derive(Default)]
struct FixedTuneIn {
    next: AtomicUsize,
}

impl IRadioTuneInSelector for FixedTuneIn {
    fn start(&self, candidate_count: usize) -> usize {
        if candidate_count == 0 {
            0
        } else {
            self.next.load(Ordering::SeqCst) % candidate_count
        }
    }
}

/// The radio tests' Navidrome.
#[derive(Clone, Default)]
struct RadioUpstream {
    relayed_scrobble_ids: Arc<Mutex<Vec<String>>>,
    return_no_local_matches: Arc<std::sync::atomic::AtomicBool>,
}

fn ok_json(fields: &str) -> String {
    format!(
        r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1"{}}}}}"#,
        if fields.is_empty() {
            String::new()
        } else {
            format!(",{fields}")
        }
    )
}

const FAILED_JSON: &str = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#;
const OK_XML: &str =
    r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"/>"#;
const FAILED_XML: &str = r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed" version="1.16.1"><error code="40" message="Wrong username or password"/></subsonic-response>"#;

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

impl Respond for RadioUpstream {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let reply = |body: &str, content_type: &str| {
            ResponseTemplate::new(200).set_body_raw(body, format!("{content_type}; charset=utf-8").as_str())
        };
        let json = |body: &str| reply(body, "application/json");
        let path = request.url.path().trim_matches('/').to_lowercase();
        let format = query_value(request, "f").unwrap_or_else(|| "json".into());
        let xml = format == "xml";
        let username = query_value(request, "u").unwrap_or_default();
        if username == "bad" {
            return json(if xml { FAILED_XML } else { FAILED_JSON });
        }
        if path == "rest/scrobble" {
            *self.relayed_scrobble_ids.lock() = request
                .url
                .query_pairs()
                .filter(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned())
                .collect();
        }
        match path.as_str() {
            "rest/getsong" => {
                let id = query_value(request, "id").unwrap_or_else(|| "song".into());
                json(&ok_json(&format!(
                    r#""song":{{"id":"{id}","artist":"Artist {id}","title":"Title {id}","album":"Album {id}","genre":"Rock","duration":180}}"#
                )))
            }
            "rest/search3" => {
                if self.return_no_local_matches.load(Ordering::SeqCst) {
                    return json(&ok_json(r#""searchResult3":{"song":[]}"#));
                }
                let search = query_value(request, "query").unwrap_or_default();
                let lower = search.to_lowercase();
                let ordinal = if lower.contains("four") {
                    "four"
                } else if lower.contains("three") {
                    "three"
                } else if lower.contains("two") {
                    "two"
                } else {
                    "one"
                };
                // Answer with the recording that was asked for.
                let prefix = if lower.starts_with("new ") { "New " } else { "" };
                let suffix = if lower.ends_with(" refreshed") {
                    " Refreshed"
                } else {
                    ""
                };
                let id = format!(
                    "local-{}{ordinal}{}",
                    if prefix.is_empty() { "" } else { "new-" },
                    if suffix.is_empty() { "" } else { "-refreshed" }
                );
                let artist = format!("{prefix}Artist {}", capitalized(ordinal));
                let title = format!("{prefix}Song {}{suffix}", capitalized(ordinal));
                json(&ok_json(&format!(
                    r#""searchResult3":{{"song":[{{"id":"{id}","artist":"{artist}","title":"{title}","album":"Album","duration":180}}]}}"#
                )))
            }
            "rest/getplaylists" => {
                if xml {
                    reply(
                        r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><playlists><playlist id="native-1" name="Native Playlist" owner="alice" songCount="1" duration="180"/></playlists></subsonic-response>"#,
                        "application/xml",
                    )
                } else {
                    json(&ok_json(
                        r#""playlists":{"playlist":[{"id":"native-1","name":"Native Playlist","owner":"alice","songCount":1,"duration":180}]}"#,
                    ))
                }
            }
            "rest/getinternetradiostations" => {
                if xml {
                    reply(
                        r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><internetRadioStations><internetRadioStation id="native-radio" name="Native Radio" streamUrl="https://radio.test/live"/></internetRadioStations></subsonic-response>"#,
                        "application/xml",
                    )
                } else {
                    json(&ok_json(
                        r#""internetRadioStations":{"internetRadioStation":[{"id":"native-radio","name":"Native Radio","streamUrl":"https://radio.test/live"}]}"#,
                    ))
                }
            }
            "rest/stream" => reply("source-audio", "application/octet-stream"),
            "rest/getplaylist" => json(&ok_json(
                r#""playlist":{"id":"native-1","name":"Native Playlist","entry":[]}"#,
            )),
            "rest/ping" | "rest/scrobble" => {
                if xml {
                    reply(OK_XML, "application/xml")
                } else {
                    json(&ok_json(r#""scrobble":{}"#))
                }
            }
            "api/playlist" => json(r#"[{"id":"native-1","name":"Native Playlist","songCount":1}]"#),
            "api/playlist/native-1" => json(r#"{"id":"native-1","name":"Native Playlist"}"#),
            _ => {
                if xml {
                    reply(OK_XML, "application/xml")
                } else {
                    json(&ok_json(""))
                }
            }
        }
    }
}

struct Options {
    explicit_filter: ExplicitFilter,
    expose_playlists: bool,
    expose_streams: bool,
    enable_icy_metadata: bool,
    starter_publish_timeout_seconds: Option<i32>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            explicit_filter: ExplicitFilter::All,
            expose_playlists: true,
            expose_streams: true,
            enable_icy_metadata: true,
            starter_publish_timeout_seconds: None,
        }
    }
}

/// The app over the radio tests' Navidrome and services.
struct RadioFixture {
    _dir: TempDir,
    _navidrome: MockServer,
    upstream: RadioUpstream,
    metadata: Arc<TestMetadata>,
    transcoder: Arc<BlockingTranscoder>,
    tune_in: Arc<FixedTuneIn>,
    state: AppState,
    app: App,
}

impl RadioFixture {
    async fn new(options: Options) -> RadioFixture {
        RadioFixture::with_metadata(options, TestMetadata::default()).await
    }

    async fn with_metadata(options: Options, metadata: TestMetadata) -> RadioFixture {
        let dir = TempDir::new().expect("temp dir");
        let upstream = RadioUpstream::default();
        let navidrome = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(upstream.clone())
            .mount(&navidrome)
            .await;
        let defaults = LastFmSettings::default();
        let settings = AppSettings {
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                auto_detect_download_path: false,
                explicit_filter: options.explicit_filter,
                ..Default::default()
            },
            last_fm: LastFmSettings {
                starter_publish_timeout_seconds: options
                    .starter_publish_timeout_seconds
                    .unwrap_or(defaults.starter_publish_timeout_seconds),
                enable_radio: true,
                enable_personalized_stations: true,
                enable_discovery_stations: true,
                expose_radio_as_playlists: options.expose_playlists,
                expose_radio_as_streams: options.expose_streams,
                radio_stream_bitrate_kbps: 192,
                enable_icy_metadata: options.enable_icy_metadata,
                ..defaults
            },
            ..Default::default()
        };
        let metadata = Arc::new(metadata);
        let transcoder = Arc::new(BlockingTranscoder::new());
        let tune_in = Arc::new(FixedTuneIn::default());
        let state = AppState::for_tests_with(
            settings,
            TestServices {
                metadata: Some(metadata.clone()),
                radio_transcoder: Some(transcoder.clone()),
                radio_tune_in: Some(tune_in.clone()),
                radio_cache_dir: Some(dir.path().join("radio-cache")),
                config_dir: Some(dir.path().join("config")),
                download_service: Some(Arc::new(ShimDownloads)),
                ..Default::default()
            },
        );
        RadioFixture {
            _dir: dir,
            _navidrome: navidrome,
            upstream,
            metadata,
            transcoder,
            tune_in,
            app: app(state.clone()),
            state,
        }
    }

    fn station_id(&self) -> String {
        station_id("alice", "your-mix")
    }

    fn install_station(&self, title_suffix: &str) {
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 8, 25, h, 0, 0).unwrap();
        let track = |artist: &str, title: &str, duration: i32| LastFmRadioTrack {
            artist: artist.into(),
            title: format!("{title}{title_suffix}"),
            duration: Some(duration),
            ..Default::default()
        };
        self.state.last_fm_radio_state.replace_stations(
            "alice",
            &[LastFmRadioStation {
                id: self.station_id(),
                key: "your-mix".into(),
                name: "Your Mix".into(),
                owner: "alice".into(),
                kind: LastFmRadioStationKind::YourMix,
                personalized: true,
                created_utc: at(1),
                changed_utc: at(2) + TimeDelta::minutes(title_suffix.chars().count() as i64),
                valid_until_utc: Utc.with_ymd_and_hms(2026, 8, 26, 2, 0, 0).unwrap(),
                tracks: vec![
                    track("Artist One", "Song One", 180),
                    track("Artist Two", "Song Two", 200),
                    track("Artist Three", "Song Three", 210),
                    track("Artist Four", "Song Four", 220),
                ],
                ..Default::default()
            }],
        );
    }

    fn install_second_station(&self) {
        let first = self
            .state
            .last_fm_radio_state
            .find_station("alice", &self.station_id())
            .expect("Install the primary station first");
        let track = |artist: &str, title: &str, duration: i32| LastFmRadioTrack {
            artist: artist.into(),
            title: title.into(),
            duration: Some(duration),
            ..Default::default()
        };
        self.state.last_fm_radio_state.replace_stations(
            "alice",
            &[
                first,
                LastFmRadioStation {
                    id: station_id("alice", "discovery"),
                    key: "discovery".into(),
                    name: "Discovery Mix".into(),
                    owner: "alice".into(),
                    kind: LastFmRadioStationKind::Discovery,
                    personalized: true,
                    created_utc: Utc.with_ymd_and_hms(2026, 8, 25, 1, 0, 0).unwrap(),
                    changed_utc: Utc.with_ymd_and_hms(2026, 8, 25, 3, 0, 0).unwrap(),
                    valid_until_utc: Utc.with_ymd_and_hms(2026, 8, 26, 3, 0, 0).unwrap(),
                    tracks: vec![
                        track("New Artist One", "New Song One", 180),
                        track("New Artist Two", "New Song Two", 200),
                        track("New Artist Three", "New Song Three", 210),
                    ],
                    ..Default::default()
                },
            ],
        );
    }

    async fn get(&self, uri: &str) -> Reply {
        get(&self.app, uri).await
    }

    async fn station_list(&self, uri: &str) -> String {
        get_string(&self.app, uri).await
    }

    /// The stream URL the station list published for "Your Mix".
    async fn your_mix_stream_path(&self) -> String {
        let body = self
            .station_list("/rest/getInternetRadioStations?u=alice&t=token&s=salt&f=json")
            .await;
        let list: serde_json::Value = serde_json::from_str(&body).unwrap();
        let url = list["subsonic-response"]["internetRadioStations"]["internetRadioStation"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "Your Mix")
            .expect("Your Mix is listed")["streamUrl"]
            .as_str()
            .unwrap()
            .to_string();
        let path_start = url.find("/radio/stream/").expect("a stream URL");
        url[path_start..].to_string()
    }

    fn token_of(path: &str) -> String {
        path.rsplit('/').next().unwrap().to_string()
    }

    fn pool_titles(&self, token: &str) -> Vec<String> {
        self.state
            .last_fm_radio_stream_sessions
            .get(token, None)
            .and_then(|session| session.ready_pool)
            .unwrap_or_default()
            .iter()
            .map(|item| item.track.title.clone())
            .collect()
    }

    fn pool_keys(&self, token: &str) -> Vec<String> {
        self.state
            .last_fm_radio_stream_sessions
            .get(token, None)
            .and_then(|session| session.ready_pool)
            .unwrap_or_default()
            .iter()
            .map(|item| item.cache_key.clone())
            .collect()
    }

    /// Waits until the transcoder has been called `at_least` times. The deadline is generous
    /// (10 s) because `cargo test --workspace` on a loaded machine can starve these tasks;
    /// returning early used to let a late background transcode land after a test had moved
    /// on, which made the pool counts below flaky.
    async fn wait_for_calls(&self, at_least: usize) {
        for _ in 0..1000 {
            if self.transcoder.calls() >= at_least {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "the transcoder was called {} times, expected at least {at_least}",
            self.transcoder.calls()
        );
    }
}

/// A streamed GET, its first bytes read; the response is handed back so the caller decides
/// when the listener hangs up.
async fn open_stream(app: &App, path: &str, headers: &[(&str, &str)]) -> axum::response::Response {
    let mut request = axum::http::Request::builder().method(Method::GET).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .expect("infallible")
}

async fn first_bytes(response: &mut axum::response::Response, count: usize) -> Vec<u8> {
    let mut read = Vec::new();
    let body = response.body_mut();
    while read.len() < count {
        let frame = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("bytes within ten seconds")
            .expect("more body")
            .expect("a frame");
        if let Ok(data) = frame.into_data() {
            read.extend_from_slice(&data);
        }
    }
    read.truncate(count);
    read
}

#[tokio::test]
async fn subsonic_playlist_list_merges_ready_station_after_successful_authentication() {
    for format in ["json", "xml"] {
        let fixture = RadioFixture::new(Options::default()).await;
        fixture.install_station("");
        let response = fixture
            .get(&format!("/rest/getPlaylists?u=alice&t=token&s=salt&f={format}"))
            .await;
        assert_eq!(response.status, StatusCode::OK, "{format}");
        let body = &response.body;
        assert!(body.contains("Native Playlist"), "{format}: {body}");
        assert!(body.contains("Your Mix"), "{format}: {body}");
        assert!(body.contains(&fixture.station_id()), "{format}");
        assert!(
            body.find("Native Playlist").unwrap() < body.find("Your Mix").unwrap(),
            "{format}"
        );
    }
}

#[tokio::test]
async fn subsonic_playlist_detail_is_owned_read_only_stable_and_prewarmed() {
    for format in ["json", "xml"] {
        let fixture = RadioFixture::new(Options::default()).await;
        fixture.install_station("");
        let response = fixture
            .get(&format!(
                "/rest/getPlaylist?id={}&u=alice&t=token&s=salt&f={format}",
                fixture.station_id()
            ))
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let body = &response.body;
        assert!(body.contains("Your Mix"), "{format}: {body}");
        assert!(body.contains("local-one"), "{format}: {body}");
        assert!(body.contains("readonly"), "{format}");
        assert!(body.contains("validUntil"), "{format}");
        let metadata = fixture.metadata.clone();
        until("the prewarm", || !metadata.prewarms.lock().is_empty()).await;
        assert_eq!(*fixture.metadata.prewarms.lock(), [8], "{format}");
    }
}

#[tokio::test]
async fn subsonic_playlist_detail_rejects_wrong_user_and_reserved_mutations() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    let wrong_owner = fixture
        .get(&format!(
            "/rest/getPlaylist?id={}&u=bob&t=token&s=salt&f=json",
            fixture.station_id()
        ))
        .await
        .body;
    assert!(!wrong_owner.contains("Your Mix"), "{wrong_owner}");

    for endpoint in ["updatePlaylist", "deletePlaylist"] {
        let body = fixture
            .get(&format!(
                "/rest/{endpoint}?playlistId={}&u=alice&t=token&s=salt&f=json",
                fixture.station_id()
            ))
            .await
            .body;
        assert!(body.contains("read-only"), "{endpoint}: {body}");
        assert!(body.contains("failed"), "{endpoint}: {body}");
    }
}

/// A failed sign-in does not expose the stations.
#[tokio::test]
async fn authentication_failure_does_not_expose_stations() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    let list = fixture.get("/rest/getPlaylists?u=bad&f=json").await.body;
    assert!(!list.contains("Your Mix"), "{list}");
    assert!(list.contains("failed"), "{list}");
}

#[tokio::test]
async fn playlist_materialization_applies_configured_explicit_filter() {
    let metadata = TestMetadata::answering(|artist, title, duration| {
        vec![Song {
            id: if title == "Song One" { "explicit" } else { "clean" }.into(),
            artist: artist.into(),
            title: title.into(),
            album: title.into(),
            duration,
            is_local: false,
            explicit_content_lyrics: Some(if title == "Song One" { 1 } else { 0 }),
            ..Default::default()
        }]
    });
    let fixture = RadioFixture::with_metadata(
        Options {
            explicit_filter: ExplicitFilter::CleanOnly,
            ..Default::default()
        },
        metadata,
    )
    .await;
    fixture
        .upstream
        .return_no_local_matches
        .store(true, Ordering::SeqCst);
    fixture.install_station("");
    let body = fixture
        .get(&format!(
            "/rest/getPlaylist?id={}&u=alice&t=token&s=salt&f=json",
            fixture.station_id()
        ))
        .await
        .json();
    let ids: Vec<&str> = body["subsonic-response"]["playlist"]["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|song| song["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&"explicit"), "{ids:?}");
    assert!(ids.contains(&"clean"), "{ids:?}");
}

#[tokio::test]
async fn kid_station_is_clean() {
    let metadata = TestMetadata::answering(|artist, title, duration| {
        vec![Song {
            id: if title == "Song One" { "explicit" } else { "clean" }.into(),
            artist: artist.into(),
            title: title.into(),
            album: title.into(),
            duration,
            is_local: false,
            explicit_content_lyrics: Some(if title == "Song One" { 1 } else { 0 }),
            ..Default::default()
        }]
    });
    // The server lets everything through; the station's owner is a kid profile.
    let fixture = RadioFixture::with_metadata(Options::default(), metadata).await;
    fixture
        .upstream
        .return_no_local_matches
        .store(true, Ordering::SeqCst);
    fixture.install_station("");
    let state = &fixture.state;
    let station = state
        .last_fm_radio_state
        .find_station("alice", &fixture.station_id())
        .expect("installed");
    let auth: octo_subsonic::subsonic_request_parser::Parameters =
        [("u", "alice"), ("t", "token"), ("s", "salt")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    let materialized = |songs: Vec<Song>| songs.into_iter().map(|song| song.id).collect::<Vec<_>>();

    let mut kid = crate::services::family::member_credentials::tests::member("alice", String::new());
    kid.preset = octo_core::family::abilities::Preset::Kid;
    state.family_store.upsert_member(kid);
    let everyone = materialized(
        crate::controllers::subsonic::action_helpers::materialize_station(
            state,
            &state.subsonic_proxy,
            &station,
            &auth,
        )
        .await,
    );
    assert!(
        everyone.contains(&"explicit".to_string()),
        "Family off: {everyone:?}"
    );

    let mut on = state.settings.current().as_ref().clone();
    on.family.enabled = true;
    state.settings.set(on);
    let clean = materialized(
        crate::controllers::subsonic::action_helpers::materialize_station(
            state,
            &state.subsonic_proxy,
            &station,
            &auth,
        )
        .await,
    );
    assert!(!clean.contains(&"explicit".to_string()), "{clean:?}");
    assert!(clean.contains(&"clean".to_string()), "{clean:?}");
}

#[tokio::test]
async fn internet_radio_list_merges_ordinary_and_generated_stations_with_opaque_stream_url() {
    for format in ["json", "xml"] {
        let fixture = RadioFixture::new(Options::default()).await;
        fixture.install_station("");
        let body = send(
            &fixture.app,
            Method::GET,
            &format!("/rest/getInternetRadioStations?u=alice&t=token&s=salt&f={format}"),
            &[("X-Forwarded-Proto", "https"), ("Host", "localhost")],
            Body::empty(),
        )
        .await
        .body;
        assert!(body.contains("Native Radio"), "{format}: {body}");
        assert!(body.contains("Your Mix"), "{format}: {body}");
        assert!(body.contains("/radio/stream/"), "{format}");
        let cover = if format == "json" {
            format!("\"coverArt\":\"{}\"", fixture.station_id())
        } else {
            format!("coverArt=\"{}\"", fixture.station_id())
        };
        assert!(body.contains(&cover), "{format}: {body}");
        assert!(
            body.contains("https://localhost/radio/stream/"),
            "{format}: {body}"
        );
        assert!(!body.contains("t=token"), "{format}");
        assert!(!body.contains("u=alice"), "{format}");
    }
}

#[tokio::test]
async fn internet_radio_list_publishes_starter_in_same_response_then_warms_runway() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    let url = "/rest/getInternetRadioStations?u=alice&t=token&s=salt&f=json";

    assert!(fixture.station_list(url).await.contains("Your Mix"));
    fixture.wait_for_calls(READY_POOL_SIZE).await;
    assert_eq!(fixture.transcoder.calls(), READY_POOL_SIZE);
    assert!(fixture.station_list(url).await.contains("Your Mix"));
    assert_eq!(fixture.transcoder.calls(), READY_POOL_SIZE);
}

#[tokio::test]
async fn startup_warm_caches_persisted_station_before_client_lists_it() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");

    let result = fixture
        .state
        .last_fm_radio_warmup
        .process("alice", &CancellationToken::new())
        .await
        .expect("warmed");
    assert_eq!(result.station_count, 1);
    assert_eq!(result.ready_station_count, 1);
    assert_eq!(result.ready_track_count, READY_POOL_SIZE);
    assert_eq!(fixture.transcoder.calls(), READY_POOL_SIZE);

    let body = fixture
        .station_list("/rest/getInternetRadioStations?u=alice&t=token&s=salt&f=json")
        .await;
    assert!(body.contains("Your Mix"));
    assert_eq!(fixture.transcoder.calls(), READY_POOL_SIZE);
}

#[tokio::test]
async fn internet_radio_list_waits_for_starter_and_publishes_in_that_same_response() {
    // 0 is the unbounded wait: the original contract, kept available as a setting.
    let fixture = RadioFixture::new(Options {
        starter_publish_timeout_seconds: Some(0),
        ..Default::default()
    })
    .await;
    fixture.install_station("");
    let gate = Gate::new();
    *fixture.transcoder.completion_gate.lock() = Some(gate.clone());

    let app = fixture.app.clone();
    let listing = tokio::spawn(async move {
        get_string(
            &app,
            "/rest/getInternetRadioStations?u=alice&t=token&s=salt&f=json",
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), fixture.transcoder.started().wait())
        .await
        .expect("the starter began");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!listing.is_finished());

    gate.set();
    assert!(listing.await.unwrap().contains("Your Mix"));
}

#[tokio::test]
async fn internet_radio_list_answers_inside_the_starter_bound_and_publishes_on_the_next_refresh() {
    let fixture = RadioFixture::new(Options {
        starter_publish_timeout_seconds: Some(1),
        ..Default::default()
    })
    .await;
    fixture.install_station("");
    let gate = Gate::new();
    *fixture.transcoder.completion_gate.lock() = Some(gate.clone());

    let url = "/rest/getInternetRadioStations?u=alice&t=token&s=salt&f=json";
    let first = tokio::time::timeout(Duration::from_secs(10), fixture.station_list(url))
        .await
        .expect("answered inside the bound");
    tokio::time::timeout(Duration::from_secs(10), fixture.transcoder.started().wait())
        .await
        .expect("the starter began");
    assert!(!first.contains("Your Mix"), "{first}");

    // The transcode the first request stopped waiting for is still the one that
    // makes the station ready; nothing has to start it again.
    gate.set();
    let mut second = String::new();
    for _ in 0..100 {
        if second.contains("Your Mix") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        second = fixture.station_list(url).await;
    }
    assert!(second.contains("Your Mix"), "{second}");
}

#[tokio::test]
async fn tune_in_starts_where_the_selector_says_and_wraps_through_the_cached_tracks() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    fixture
        .state
        .last_fm_radio_warmup
        .process("alice", &CancellationToken::new())
        .await
        .expect("warmed");
    fixture.tune_in.next.store(1, Ordering::SeqCst);

    let path = fixture.your_mix_stream_path().await;
    let token = RadioFixture::token_of(&path);
    assert_eq!(
        fixture.pool_titles(&token),
        ["Song Two", "Song Three", "Song One"]
    );
}

#[tokio::test]
async fn internet_radio_list_returns_ready_stations_without_waiting_for_cache_misses() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    fixture
        .state
        .last_fm_radio_warmup
        .process("alice", &CancellationToken::new())
        .await
        .expect("warmed");
    fixture.install_second_station();
    fixture.transcoder.reset_started();
    let gate = Gate::new();
    *fixture.transcoder.completion_gate.lock() = Some(gate.clone());

    let listing = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.station_list("/rest/getInternetRadioStations?u=alice&t=token&s=salt&f=json"),
    )
    .await;
    let started = tokio::time::timeout(Duration::from_secs(10), fixture.transcoder.started().wait()).await;
    gate.set();
    let body = listing.expect("the listing did not wait for the cold station");
    assert!(body.contains("Your Mix"), "{body}");
    assert!(!body.contains("Discovery Mix"), "{body}");
    started.expect("the cold station is warming");
}

#[tokio::test]
async fn published_stream_keeps_its_ready_starter_across_snapshot_refresh() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    let path = fixture.your_mix_stream_path().await;
    // The listing answered only once its starter was ready and attached to this URL.
    // The refreshed songs resolve and play, but every new transcode waits at the gate,
    // so the first bytes can only come from the starter the listing published.
    fixture.install_station(" Refreshed");
    fixture.transcoder.reset_started();
    let gate = Gate::new();
    *fixture.transcoder.before_write_gate.lock() = Some(gate.clone());

    let mut response = open_stream(&fixture.app, &path, &[]).await;
    assert_eq!(first_bytes(&mut response, 3).await, b"MP3");
    gate.set();
}

#[tokio::test]
async fn playlist_and_internet_radio_publication_are_independent_and_reserved_radio_is_read_only() {
    let fixture = RadioFixture::new(Options {
        expose_playlists: false,
        expose_streams: true,
        ..Default::default()
    })
    .await;
    fixture.install_station("");
    let playlists = fixture
        .get("/rest/getPlaylists?u=alice&t=token&s=salt&f=json")
        .await
        .body;
    assert!(!playlists.contains("Your Mix"), "{playlists}");
    let radios = fixture
        .station_list("/rest/getInternetRadioStations?u=alice&t=token&s=salt&f=json")
        .await;
    assert!(radios.contains("Your Mix"), "{radios}");
    let mutation = fixture
        .get(&format!(
            "/rest/deleteInternetRadioStation?id={}&u=alice&t=token&s=salt&f=json",
            fixture.station_id()
        ))
        .await
        .body;
    assert!(mutation.contains("read-only"), "{mutation}");
}

#[tokio::test]
async fn opaque_internet_radio_url_streams_mp3_and_cancels_when_client_disconnects() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    let path = fixture.your_mix_stream_path().await;

    let mut response = open_stream(&fixture.app, &path, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "audio/mpeg");
    assert!(response.headers().get("icy-metaint").is_none());
    assert_eq!(first_bytes(&mut response, 3).await, b"MP3");
    assert_eq!(fixture.transcoder.last_bitrate_kbps.load(Ordering::SeqCst), 192);
    // The play and its relayed scrobble are recorded off the streaming request.
    let state = fixture.state.clone();
    let scrobbles = fixture.upstream.relayed_scrobble_ids.clone();
    until("the play and its relayed scrobble", || {
        !state.last_fm_radio_state.get_user("alice").plays.is_empty() && !scrobbles.lock().is_empty()
    })
    .await;
    drop(response);
}

#[tokio::test]
async fn internet_radio_stream_negotiates_configured_icy_metadata() {
    for (metadata_enabled, expect_metadata) in [(true, true), (false, false)] {
        let fixture = RadioFixture::new(Options {
            enable_icy_metadata: metadata_enabled,
            ..Default::default()
        })
        .await;
        fixture.install_station("");
        let path = fixture.your_mix_stream_path().await;
        let response = open_stream(&fixture.app, &path, &[("Icy-MetaData", "1")]).await;
        assert_eq!(response.status(), StatusCode::OK);
        let metaint = response.headers().get("icy-metaint");
        assert_eq!(metaint.is_some(), expect_metadata, "enabled: {metadata_enabled}");
        if expect_metadata {
            assert_eq!(metaint.unwrap().to_str().unwrap(), DEFAULT_INTERVAL.to_string());
        }
    }
}

#[tokio::test]
async fn published_stream_consumes_and_replenishes_three_track_session_pool() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    let path = fixture.your_mix_stream_path().await;
    let token = RadioFixture::token_of(&path);
    fixture.wait_for_calls(READY_POOL_SIZE).await;
    // A transcode counts as a call when it starts, so the runway's last song may still be on
    // its way into the cache. The stream takes what is cached when it opens; wait for all of it.
    let session = fixture
        .state
        .last_fm_radio_stream_sessions
        .get(&token, None)
        .expect("a session");
    for _ in 0..1000 {
        if fixture.state.last_fm_radio_streams.get_ready_pool(&session).len() >= READY_POOL_SIZE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let original = fixture.pool_keys(&token);
    let calls_before = fixture.transcoder.calls();
    assert_eq!(original.len(), 1, "{original:?}");

    fixture.transcoder.reset_started();
    fixture
        .transcoder
        .complete_calls
        .store(READY_POOL_SIZE + 1, Ordering::SeqCst);
    let gate = Gate::new();
    *fixture.transcoder.before_write_gate.lock() = Some(gate.clone());
    let response = open_stream(&fixture.app, &path, &[]).await;
    // The body has to be read for the stream to start writing.
    let mut body = response.into_body().into_data_stream();
    let reading = tokio::spawn(async move { while body.next().await.is_some() {} });
    tokio::time::timeout(Duration::from_secs(10), fixture.transcoder.started().wait())
        .await
        .expect("a replenishing transcode began");
    // Polls allow 10 s: a loaded test machine can delay the stream task well past 1 s.
    for _ in 0..1000 {
        if fixture.pool_keys(&token).len() == READY_POOL_SIZE - 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let consumed = fixture.pool_keys(&token);
    assert_eq!(
        consumed.len(),
        READY_POOL_SIZE - 1,
        "{consumed:?} after {} transcodes ({calls_before} before the stream)",
        fixture.transcoder.calls()
    );
    assert!(!consumed.contains(&original[0]));

    gate.set();
    reading.abort();
    for _ in 0..1000 {
        if fixture.pool_keys(&token).len() >= READY_POOL_SIZE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fixture.pool_keys(&token).len(), READY_POOL_SIZE);
    assert!(gate.is_set());
}

#[tokio::test]
async fn internet_radio_preparation_skips_failed_source_and_caches_playable_fallback() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture
        .transcoder
        .failures_before_success
        .store(1, Ordering::SeqCst);
    fixture
        .transcoder
        .complete_calls
        .store(READY_POOL_SIZE, Ordering::SeqCst);
    fixture.install_station("");
    let url = "/rest/getInternetRadioStations?u=alice&t=token&s=salt&f=json";
    let list = fixture.station_list(url).await;
    assert!(list.contains("Your Mix"), "{list}");
    fixture.wait_for_calls(READY_POOL_SIZE + 1).await;
    assert_eq!(fixture.transcoder.calls(), READY_POOL_SIZE + 1);
    assert!(fixture.station_list(url).await.contains("Your Mix"));
    assert_eq!(fixture.transcoder.calls(), READY_POOL_SIZE + 1);
    let station = fixture
        .state
        .last_fm_radio_state
        .find_station("alice", &fixture.station_id())
        .unwrap();
    assert!(!station.tracks.iter().any(|track| track.title == "Song One"));
    let job = tokio::time::timeout(
        Duration::from_secs(1),
        fixture
            .state
            .last_fm_radio_refresh_queue
            .dequeue(&CancellationToken::new()),
    )
    .await
    .expect("a refresh was queued")
    .expect("a job");
    assert_eq!(job.username, "alice");
}

// ---------------------------------------------------------------------------------------
// Navidrome-native playlists
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn feishin_list_detail_and_paged_tracks_have_native_shape_headers_and_ownership() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    fixture
        .state
        .navidrome_identity
        .capture_login(br#"{"token":"native-token","username":"alice","isAdmin":false}"#);
    let auth = [("X-Nd-Authorization", "Bearer native-token")];

    let list = send(
        &fixture.app,
        Method::GET,
        "/api/playlist?_start=0&_end=20",
        &auth,
        Body::empty(),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(list.header("X-Total-Count"), Some("2"));
    let rows = list.json();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    let radio = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == fixture.station_id().as_str())
        .expect("the station is listed");
    assert_eq!(radio["readonly"], true);

    let detail = send(
        &fixture.app,
        Method::GET,
        &format!("/api/playlist/{}", fixture.station_id()),
        &auth,
        Body::empty(),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(detail.json()["name"], "Your Mix");

    let tracks = send(
        &fixture.app,
        Method::GET,
        &format!("/api/playlist/{}/tracks?_start=1&_end=2", fixture.station_id()),
        &auth,
        Body::empty(),
    )
    .await;
    assert_eq!(tracks.status, StatusCode::OK);
    assert_eq!(tracks.header("X-Total-Count"), Some("4"));
    let tracks = tracks.json();
    assert_eq!(tracks.as_array().unwrap().len(), 1);
    assert_eq!(tracks[0]["id"], "local-two");
}

#[tokio::test]
async fn navidrome_ui_unbounded_playlist_lists_return_every_row() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    fixture
        .state
        .navidrome_identity
        .capture_login(br#"{"token":"native-token","username":"alice","isAdmin":false}"#);
    let auth = [("X-Nd-Authorization", "Bearer native-token")];

    for query in [
        "_end=0&_sort=name&_start=-100",
        "_end=-1&_order=ASC&_sort=name&_start=0",
    ] {
        let list = send(
            &fixture.app,
            Method::GET,
            &format!("/api/playlist?{query}"),
            &auth,
            Body::empty(),
        )
        .await;
        assert_eq!(list.status, StatusCode::OK, "{}", list.body);
        assert_eq!(list.json().as_array().unwrap().len(), 2, "{query}");
    }

    let writable = send(
        &fixture.app,
        Method::GET,
        "/api/playlist?_end=-1&_order=ASC&_sort=name&_start=0&smart=false",
        &auth,
        Body::empty(),
    )
    .await;
    assert_eq!(writable.status, StatusCode::OK, "{}", writable.body);
    let rows = writable.json();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["id"], "native-1");
}

#[tokio::test]
async fn native_reserved_mutation_is_read_only_and_ordinary_detail_relays() {
    let fixture = RadioFixture::new(Options::default()).await;
    fixture.install_station("");
    let mutation = send(
        &fixture.app,
        Method::PUT,
        &format!("/api/playlist/{}", fixture.station_id()),
        &[("Content-Type", "application/json")],
        Body::from("{}"),
    )
    .await;
    assert_eq!(mutation.status, StatusCode::METHOD_NOT_ALLOWED);

    let ordinary = fixture.get("/api/playlist/native-1").await;
    assert_eq!(ordinary.status, StatusCode::OK);
    assert!(ordinary.body.contains("Native Playlist"));
}

/// Every station track not in the library comes back as an outside copy named after it.
fn outside_copies() -> TestMetadata {
    TestMetadata::answering(|artist, title, _| {
        vec![Song {
            id: format!("ext-{artist}-{title}"),
            artist: artist.into(),
            title: title.into(),
            is_local: false,
            ..Default::default()
        }]
    })
}

#[tokio::test]
async fn a_station_song_that_sounded_alike_but_is_no_longer_in_the_library_is_left_out() {
    use octo_core::radio::radio_provider::{SOUNDS_ALIKE, YOU_TUBE_MUSIC};
    let fixture = RadioFixture::with_metadata(Options::default(), outside_copies()).await;
    fixture.install_station("");
    let mut station = fixture
        .state
        .last_fm_radio_state
        .find_station("alice", &fixture.station_id())
        .expect("installed");
    station.tracks.push(LastFmRadioTrack {
        artist: "Gone Artist".into(),
        title: "Gone Song".into(),
        duration: Some(200),
        source: SOUNDS_ALIKE.into(),
        resolved_id: Some("ext-gone".into()),
        ..Default::default()
    });
    station.tracks.push(LastFmRadioTrack {
        artist: "Online Artist".into(),
        title: "Online Song".into(),
        duration: Some(200),
        source: YOU_TUBE_MUSIC.into(),
        resolved_id: Some("ext-online".into()),
        ..Default::default()
    });
    fixture
        .state
        .last_fm_radio_state
        .replace_stations("alice", &[station]);

    let body = get_string(
        &fixture.app,
        &format!(
            "/rest/getPlaylist?id={}&u=alice&t=token&s=salt&f=json",
            fixture.station_id()
        ),
    )
    .await;

    // Neither is in the fake library, so both resolve as outside songs: the one found online
    // plays from outside, the one that only sounded alike does not.
    assert!(!body.contains("Gone Song"), "{body}");
    assert!(body.contains("Online Song"), "{body}");
}

#[tokio::test]
async fn a_station_song_says_the_source_its_station_was_built_with() {
    use octo_core::radio::radio_provider::YOU_TUBE_MUSIC;
    let fixture = RadioFixture::with_metadata(Options::default(), outside_copies()).await;
    fixture.install_station("");
    let mut station = fixture
        .state
        .last_fm_radio_state
        .find_station("alice", &fixture.station_id())
        .expect("installed");
    for track in &mut station.tracks {
        track.source = YOU_TUBE_MUSIC.into();
    }
    fixture
        .state
        .last_fm_radio_state
        .replace_stations("alice", &[station]);

    let body = get_string(
        &fixture.app,
        &format!(
            "/rest/getPlaylist?id={}&u=alice&t=token&s=salt&f=json",
            fixture.station_id()
        ),
    )
    .await;
    let doc: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    let entries = doc["subsonic-response"]["playlist"]["entry"]
        .as_array()
        .expect("entries");
    assert!(!entries.is_empty());
    for entry in entries {
        assert_eq!(entry["octoSuggestedBy"], "YouTube Music", "{entry}");
    }
}

/// A Discovery Mix of ten: six library songs and four outside ones, and a reserve of outside
/// songs after them.
fn install_discovery(fixture: &RadioFixture, reserve: &[(&str, &str)]) -> String {
    let track = |artist: &str, title: &str| LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        duration: Some(200),
        ..Default::default()
    };
    let id = station_id("alice", "discovery");
    fixture.state.last_fm_radio_state.replace_stations(
        "alice",
        &[LastFmRadioStation {
            id: id.clone(),
            key: "discovery".into(),
            name: "Discovery Mix".into(),
            owner: "alice".into(),
            kind: LastFmRadioStationKind::Discovery,
            personalized: true,
            valid_until_utc: Utc::now() + TimeDelta::hours(12),
            tracks: vec![
                track("Artist One", "Song One"),
                track("Outside A", "Tune A"),
                track("Artist Two", "Song Two"),
                track("New Artist One", "New Song One"),
                track("Outside B", "Tune B"),
                track("Artist Three", "Song Three"),
                track("New Artist Two", "New Song Two"),
                track("Outside C", "Tune C"),
                track("Artist Four", "Song Four"),
                track("Outside D", "Tune D"),
            ],
            reserve: reserve
                .iter()
                .map(|(artist, title)| track(artist, title))
                .collect(),
            ..Default::default()
        }],
    );
    id
}

async fn discovery_entries(fixture: &RadioFixture, id: &str) -> Vec<serde_json::Value> {
    let body = get_string(
        &fixture.app,
        &format!("/rest/getPlaylist?id={id}&u=alice&t=token&s=salt&f=json"),
    )
    .await;
    let doc: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    doc["subsonic-response"]["playlist"]["entry"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn library_ids(entries: &[serde_json::Value]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .filter(|id| id.starts_with("local-"))
        .map(str::to_string)
        .collect()
}

// At the default Discovery balance of 35, a Discovery Mix of ten keeps four library songs (3.5
// rounds to even); the library songs past them give their places to the reserve's outside songs.
#[tokio::test]
async fn discovery_mix_keeps_library_songs_to_the_discovery_balance_share() {
    let fixture = RadioFixture::with_metadata(Options::default(), outside_copies()).await;
    let id = install_discovery(&fixture, &[("Outside E", "Tune E"), ("Outside F", "Tune F")]);

    let entries = discovery_entries(&fixture, &id).await;

    let titles: Vec<&str> = entries.iter().filter_map(|e| e["title"].as_str()).collect();
    assert_eq!(entries.len(), 10, "{titles:?}");
    assert_eq!(
        library_ids(&entries),
        ["local-one", "local-two", "local-new-one", "local-three"],
        "{titles:?}"
    );
    assert_eq!(titles[6], "Tune E", "{titles:?}");
    assert_eq!(titles[8], "Tune F", "{titles:?}");
}

// With nothing in reserve, the library songs past the share keep their places: a short reserve
// never makes a short mix.
#[tokio::test]
async fn discovery_mix_fills_from_library_songs_when_the_reserve_runs_out() {
    let fixture = RadioFixture::with_metadata(Options::default(), outside_copies()).await;
    let id = install_discovery(&fixture, &[("Outside E", "Tune E")]);

    let entries = discovery_entries(&fixture, &id).await;

    let titles: Vec<&str> = entries.iter().filter_map(|e| e["title"].as_str()).collect();
    assert_eq!(entries.len(), 10, "{titles:?}");
    assert_eq!(library_ids(&entries).len(), 5, "{titles:?}");
    assert_eq!(titles[6], "Tune E", "{titles:?}");
    assert_eq!(titles[8], "Song Four", "{titles:?}");
}
