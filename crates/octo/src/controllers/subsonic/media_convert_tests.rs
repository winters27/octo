//! Songs found online in the asked format and size, through the app: the funnel's answers to
//! asks, ranges, time seeks, HEAD, failures, the family gate's ask reaching the funnel, and the
//! OpenSubsonic transcoding endpoints. Sources are real audio ffmpeg makes; with
//! `OCTO_TESTS_NEED_FFMPEG=1` a missing ffmpeg fails a test instead of skipping it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant as StdInstant};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use octo_core::common::Clock;
use octo_core::fingerprint::heart_gate::HeardSource;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::DownloadSource;
use octo_core::soulseek::StreamFacts;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer};

use super::caller_checks::SubsonicCall;
use super::test_app::*;
use crate::app::AppState;
use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService, StreamPlay};
use crate::services::library::ReplacementHandoff;
use crate::services::prefetch::PreviewCopies;
use crate::services::prefetch::convert_rules::{self, Ask, Codec, GateClient, OwnAsk, Plan, Target};
use crate::services::prefetch::converted_copies::{
    ConvertedCopies, FfmpegRunner, Runner, RunningJob, Timings,
};
use crate::services::prefetch::preview_copies::{PreviewSource, WholePreview};

const VIDEO: &str = "AAAAAAAAAA1";
const OTHER_VIDEO: &str = "BBBBBBBBBB2";
/// A third-party player.
const SIGNED: &str = "u=alice&t=good&s=salt&v=1.16.1&c=test";
/// One of Octo's apps (its User-Agent says which).
const OCTO: &str = "u=alice&t=good&s=salt&v=1.16.1&c=Octo";
const PHONE: &str = "Octo/1.13.0 (Android)";

// ---- Audio ------------------------------------------------------------------------------

/// Audio ffmpeg makes from the lavfi `source` with `args`, or None without ffmpeg (with
/// `OCTO_TESTS_NEED_FFMPEG=1` a missing ffmpeg fails instead).
fn made(source: &str, args: &[&str], extension: &str) -> Option<Vec<u8>> {
    let dir = tempfile::tempdir().ok()?;
    let out = dir.path().join(format!("a.{extension}"));
    let ran = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", source])
        .args(args)
        .arg(&out)
        .status();
    match ran {
        Ok(status) if status.success() => std::fs::read(&out).ok(),
        _ if std::env::var("OCTO_TESTS_NEED_FFMPEG").is_ok_and(|v| v == "1") => {
            panic!("ffmpeg is needed for this test")
        }
        _ => None,
    }
}

macro_rules! made_or_skip {
    ($source:expr, $args:expr, $extension:expr) => {
        match made($source, $args, $extension) {
            Some(bytes) => bytes,
            None => {
                eprintln!("skipped: ffmpeg is not on the PATH");
                return;
            }
        }
    };
}

/// A tone that steps up 40 Hz every second from 440 Hz, `seconds` long.
fn steps(seconds: u32) -> String {
    format!("aevalsrc=sin(2*PI*(440+40*floor(t))*t):s=44100:d={seconds}")
}

/// YouTube's format 140: AAC at 128 kbps in fragmented MP4.
const M4A: &[&str] = &[
    "-c:a",
    "aac",
    "-b:a",
    "128k",
    "-movflags",
    "+empty_moov+default_base_moof",
    "-frag_duration",
    "2000000",
    "-f",
    "mp4",
];
/// YouTube's format 251: Opus in WebM.
const WEBM: &[&str] = &["-c:a", "libopus", "-b:a", "128k", "-f", "webm"];
const FLAC: &[&str] = &["-c:a", "flac", "-f", "flac"];

fn is_mp3(bytes: &[u8]) -> bool {
    bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0 && (bytes[1] >> 1) & 0x3 == 0x1
}

fn is_ogg_opus(bytes: &[u8]) -> bool {
    bytes.starts_with(b"OggS") && bytes.windows(8).take(256).any(|w| w == b"OpusHead")
}

fn is_adts(bytes: &[u8]) -> bool {
    bytes.len() > 2 && bytes[0] == 0xFF && bytes[1] & 0xF6 == 0xF0
}

/// An answer decoded by ffmpeg to mono 44.1 kHz samples.
fn decoded(bytes: &[u8]) -> Vec<i16> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let input = dir.path().join("in");
    std::fs::write(&input, bytes).expect("written");
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&input)
        .args(["-f", "s16le", "-ac", "1", "-ar", "44100", "pipe:1"])
        .output()
        .expect("ffmpeg ran");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (pairs, _) = out.stdout.as_chunks::<2>();
    pairs.iter().map(|pair| i16::from_le_bytes(*pair)).collect()
}

/// The tone in the 100 ms of `samples` from `from` seconds, in Hz, by its zero crossings.
fn tone_at(samples: &[i16], from: f64) -> f64 {
    let start = (from * 44_100.0) as usize;
    let window = &samples[start..start + 4_410];
    let crossings = window
        .windows(2)
        .filter(|pair| (pair[0] < 0) != (pair[1] < 0))
        .count();
    crossings as f64 / 2.0 / 0.1
}

// ---- The world --------------------------------------------------------------------------

/// A door a test opens: runs wait at it until then.
#[derive(Default)]
struct Door {
    open: AtomicBool,
    opened: Notify,
}

impl Door {
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.opened.notify_waiters();
    }

    async fn pass(&self) {
        loop {
            let opened = self.opened.notified();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            opened.await;
        }
    }
}

/// The real ffmpeg behind a door, counting every build (a run with no `-ss`). `fail_when`
/// makes runs whose arguments hold that word end in failure with no sound; `short` makes every
/// build write 4 KB and then fail; `broken` makes every run fail to start; `seeks_hang` makes
/// every time seek (a run with `-ss`) send 4 KB and then nothing until it is killed;
/// `first_seek_silent` makes the first time seek send nothing until it is killed.
#[derive(Default)]
struct TestRunner {
    builds: AtomicUsize,
    seeks: AtomicUsize,
    door: Door,
    shut: bool,
    fail_when: Option<&'static str>,
    short: bool,
    broken: bool,
    seeks_hang: bool,
    first_seek_silent: bool,
}

impl TestRunner {
    fn open() -> Self {
        let runner = TestRunner::default();
        runner.door.open();
        runner
    }

    fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }

    fn seeks(&self) -> usize {
        self.seeks.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Runner for TestRunner {
    async fn spawn(&self, args: Vec<String>) -> std::io::Result<RunningJob> {
        let build = !args.iter().any(|arg| arg == "-ss");
        if build {
            self.builds.fetch_add(1, Ordering::SeqCst);
        }
        if self.broken {
            return Err(std::io::Error::other("no ffmpeg here"));
        }
        if self.shut {
            self.door.pass().await;
        }
        if self.short && build {
            // Its 4 KB are written well before it fails, so a request sees the first bytes
            // first and the failure after.
            return Ok(RunningJob {
                stdin: None,
                stdout: Box::new(std::io::Cursor::new(vec![0xFFu8; 4096])),
                done: Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Ok(false)
                }),
                kill: Box::new(|| {}),
            });
        }
        if self
            .fail_when
            .is_some_and(|word| args.iter().any(|arg| arg == word))
        {
            return Ok(RunningJob {
                stdin: None,
                stdout: Box::new(tokio::io::empty()),
                done: Box::pin(async { Ok(false) }),
                kill: Box::new(|| {}),
            });
        }
        if !build {
            let number = self.seeks.fetch_add(1, Ordering::SeqCst);
            if self.seeks_hang || (self.first_seek_silent && number == 0) {
                let sound = if self.seeks_hang {
                    vec![0xFFu8; 4096]
                } else {
                    Vec::new()
                };
                // The writer lives in the kill, so the run's output ends only when it is killed.
                let (writer, reader) = tokio::io::duplex(64);
                return Ok(RunningJob {
                    stdin: None,
                    stdout: Box::new(tokio::io::AsyncReadExt::chain(
                        std::io::Cursor::new(sound),
                        reader,
                    )),
                    done: Box::pin(std::future::pending()),
                    kill: Box::new(move || drop(writer)),
                });
            }
        }
        FfmpegRunner.spawn(args).await
    }
}

/// A shim that never answers: copies come only from the files a test writes.
struct NoShim;

#[async_trait]
impl PreviewSource for NoShim {
    async fn open_whole(&self, _: &str) -> Option<WholePreview> {
        None
    }
}

/// A shim that sends `bytes` in 8 KB pieces, one every `pace`.
struct Paced {
    bytes: Vec<u8>,
    pace: Duration,
}

#[async_trait]
impl PreviewSource for Paced {
    async fn open_whole(&self, _: &str) -> Option<WholePreview> {
        let pace = self.pace;
        let chunks: Vec<Bytes> = self.bytes.chunks(8192).map(Bytes::copy_from_slice).collect();
        let body = futures::stream::unfold(chunks.into_iter(), move |mut rest| async move {
            let next = rest.next()?;
            tokio::time::sleep(pace).await;
            Some((Ok(next), rest))
        });
        Some(WholePreview {
            content_type: "audio/mp4".into(),
            total: self.bytes.len() as u64,
            body: Box::pin(body),
        })
    }
}

/// The download service with its stream lookups slowed by `delay`.
struct SlowStreams {
    inner: Arc<FakeStreams>,
    delay: Duration,
}

#[async_trait]
impl IDownloadService for SlowStreams {
    async fn download_song(&self, p: &str, e: &str, t: &CancellationToken) -> anyhow::Result<String> {
        self.inner.download_song(p, e, t).await
    }

    async fn download_and_stream(
        &self,
        p: &str,
        e: &str,
        t: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        self.inner.download_and_stream(p, e, t).await
    }

    fn download_remaining_album_tracks_in_background(&self, a: &str, b: &str, c: &str) {
        self.inner.download_remaining_album_tracks_in_background(a, b, c);
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_acquisition(
        &self,
        a: &str,
        b: &str,
        c: bool,
        d: bool,
        e: Option<DownloadSource>,
        f: &CancellationToken,
        g: Option<Vec<String>>,
        h: bool,
        i: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        self.inner.execute_acquisition(a, b, c, d, e, f, g, h, i).await
    }

    async fn download_album_with_source(
        &self,
        a: &str,
        b: &str,
        c: DownloadSource,
        d: bool,
        e: &CancellationToken,
        f: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        self.inner.download_album_with_source(a, b, c, d, e, f).await
    }

    fn get_download_status(&self, id: &str) -> Option<DownloadInfo> {
        self.inner.get_download_status(id)
    }

    async fn get_local_path_if_exists(&self, a: &str, b: &str) -> Option<String> {
        self.inner.get_local_path_if_exists(a, b).await
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn get_direct_stream(
        &self,
        provider: &str,
        external_id: &str,
        range: Option<&str>,
        play: Option<&StreamPlay>,
        token: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        tokio::time::sleep(self.delay).await;
        self.inner
            .get_direct_stream(provider, external_id, range, play, token)
            .await
    }
}

/// The waits, short enough for tests: a first output within 1 s, other clients 2 s.
fn timings() -> Timings {
    Timings {
        stall: Duration::from_secs(10),
        failure_memory: Duration::from_secs(600),
        first_output: Duration::from_millis(1000),
        others: Duration::from_millis(2000),
        seek_input: Duration::from_millis(2000),
        recent_use: Duration::from_secs(3600),
        unused_for: chrono::Duration::hours(24),
    }
}

#[derive(Default)]
struct Setup {
    /// Whole copies written before the copies start, by video.
    copies: Vec<(&'static str, Vec<u8>)>,
    runner: TestRunner,
    /// The shim the copies fill from (none answers when unset).
    shim: Option<Arc<dyn PreviewSource>>,
    wait_for_lossless: bool,
    /// The song has no pinned video: a probe names it.
    unpinned: bool,
    slow_probe: Option<Duration>,
    /// The store's permits for builds and for time seeks (2 when unset).
    permits: Option<usize>,
}

struct World {
    state: AppState,
    downloads: Arc<FakeStreams>,
    library: Arc<FakeLibrary>,
    runner: Arc<TestRunner>,
    id: String,
    dir: tempfile::TempDir,
    _navidrome: MockServer,
}

async fn ping_ok() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(navidrome_json(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
        ))
        .mount(&server)
        .await;
    server
}

/// Help!, listed as `external-track`, 60 s long, pinned to `VIDEO` unless `unpinned`, with its
/// copies in a temp folder and the conversions beside them.
async fn world(setup: Setup) -> World {
    let navidrome = ping_ok().await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let folder = dir.path().join("copies");
    std::fs::create_dir_all(&folder).expect("a copies folder");
    for (video, bytes) in &setup.copies {
        std::fs::write(folder.join(format!("{video}.preview")), bytes).expect("a copy");
    }
    let shim = setup.shim.unwrap_or_else(|| Arc::new(NoShim));
    let preview_copies = PreviewCopies::new(folder.clone(), shim, Clock::system(), || 0);
    let runner = Arc::new(setup.runner);
    let converted = ConvertedCopies::with(
        folder.join("converted"),
        Clock::system(),
        || 0,
        runner.clone(),
        setup.permits.unwrap_or(2),
        1 << 30,
        timings(),
    );
    let downloads = Arc::new(FakeStreams::default());
    let library = Arc::new(FakeLibrary::default());
    let mut settings = settings(&navidrome.uri());
    settings.subsonic.wait_for_lossless_on_play = setup.wait_for_lossless;
    settings.soulseek.base_url = Some(format!(
        "http://{}",
        crate::services::test_support::refused_address()
    ));
    // AcoustID off: a Wait for lossless play plays its stand-in at once.
    settings.soulseek.acoust_id_api_key = "off".into();
    let service: Arc<dyn IDownloadService> = match setup.slow_probe {
        Some(delay) => Arc::new(SlowStreams {
            inner: downloads.clone(),
            delay,
        }),
        None => downloads.clone(),
    };
    let state = state_with(settings, |inner| {
        inner.download_service = service;
        inner.local_library = library.clone();
        inner.preview_copies = preview_copies;
        inner.converted_copies = converted;
    });
    let id = state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("Help!".into()),
        duration: Some(60),
        ..Default::default()
    });
    if !setup.unpinned {
        state.external_id_registry.set_you_tube_id(&id, VIDEO);
    }
    library.add_external("external-track", "soulseek", &id);
    World {
        state,
        downloads,
        library,
        runner,
        id,
        dir,
        _navidrome: navidrome,
    }
}

impl World {
    /// The shim answers this song with `bytes` of `video`, whole.
    fn shim_answers(&self, video: &'static str, bytes: &[u8]) {
        let bytes = bytes.to_vec();
        self.downloads.answer_with(
            "soulseek",
            &self.id,
            Box::new(move || {
                let chunk: std::io::Result<Bytes> = Ok(Bytes::from(bytes.clone()));
                Some(DirectStreamInfo {
                    audio_stream: Box::pin(futures::stream::iter([chunk])),
                    content_type: "audio/mp4".into(),
                    content_length: Some(bytes.len() as u64),
                    quality: None,
                    status_code: 200,
                    content_range: None,
                    video_id: Some(video.to_string()),
                    itag: None,
                })
            }),
        );
    }

    /// The stream is YouTube's format `itag` at `rate` kbps, `size` bytes when confirmed.
    fn facts(&self, itag: &str, rate: Option<i32>, size: Option<u64>) {
        self.state.external_id_registry.remember_stream(
            &self.id,
            VIDEO,
            &StreamFacts {
                itag: Some(itag.into()),
                bit_rate: rate,
                size,
                size_confirmed: size.is_some(),
                ..Default::default()
            },
        );
    }

    async fn ask(&self, query: &str, headers: &[(&str, &str)]) -> Reply {
        send(
            &app(&self.state),
            Method::GET,
            &format!("/rest/stream?id=external-track&{query}"),
            headers,
            Body::empty(),
        )
        .await
    }

    /// The answer to a stream ask, its body left unread.
    async fn open(&self, query: &str) -> axum::response::Response {
        let request = Request::builder()
            .uri(format!("/rest/stream?id=external-track&{query}"))
            .body(Body::empty())
            .expect("a request");
        app(&self.state).oneshot(request).await.expect("infallible")
    }

    fn heard_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.state
            .external_id_registry
            .heard_by(&self.id, "alice")
            .map(|record| record.heard_at)
    }
}

fn length_of(reply: &Reply) -> Option<usize> {
    reply
        .header("content-length")
        .and_then(|length| length.parse().ok())
}

// ---- Asks and their answers --------------------------------------------------------------

#[tokio::test]
async fn an_mp3_ask_with_a_length_gets_the_whole_mp3_and_its_ranges() {
    let m4a = made_or_skip!(&steps(6), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    let query = format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}");
    let whole = w.ask(&query, &[]).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.header("content-type"), Some("audio/mpeg"));
    assert_eq!(length_of(&whole), Some(whole.body.len()));
    assert!(is_mp3(&whole.body), "{:02x?}", &whole.body[..4]);
    let part = w.ask(&query, &[("Range", "bytes=1000-1999")]).await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.body.as_ref(), &whole.body[1000..2000]);
    assert_eq!(
        part.header("content-range"),
        Some(format!("bytes 1000-1999/{}", whole.body.len()).as_str())
    );
    assert!(
        w.downloads.stream_calls().is_empty(),
        "made from the copy, never the shim"
    );
    assert_eq!(w.runner.builds(), 1);
}

#[tokio::test]
async fn an_mp3_ask_without_a_length_streams_the_same_bytes_as_they_are_made() {
    let m4a = made_or_skip!(&steps(6), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    let growing = w.ask(&format!("format=mp3&maxBitRate=128&{SIGNED}"), &[]).await;
    assert_eq!(growing.status, StatusCode::OK);
    assert_eq!(growing.header("content-type"), Some("audio/mpeg"));
    assert_eq!(growing.header("content-length"), None, "read while it grows");
    let whole = w
        .ask(
            &format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert_eq!(growing.body, whole.body);
    assert_eq!(w.runner.builds(), 1);
}

#[tokio::test]
async fn past_the_wait_only_an_octo_apps_own_playback_gets_the_source() {
    let m4a = made_or_skip!(&steps(6), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a.clone())],
        runner: TestRunner {
            shut: true,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let exact = "format=mp3&maxBitRate=128&estimateContentLength=true";
    let started = StdInstant::now();
    let phone = w.ask(&format!("{exact}&{OCTO}"), &[("User-Agent", PHONE)]).await;
    assert_eq!(phone.status, StatusCode::OK);
    assert_eq!(phone.header("content-type"), Some("audio/mp4"));
    assert_eq!(phone.body.as_ref(), m4a.as_slice());
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "{:?}",
        started.elapsed()
    );
    let other = w.ask(&format!("{exact}&{SIGNED}"), &[]).await;
    assert_eq!(other.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(other.header("retry-after"), Some("5"));
    let offline = w
        .ask(
            &format!("{exact}&{OCTO}"),
            &[("User-Agent", PHONE), ("X-Octo-Purpose", "offline")],
        )
        .await;
    assert_eq!(
        offline.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "an offline copy is no playback"
    );
    w.runner.door.open();
    let again = w.ask(&format!("{exact}&{SIGNED}"), &[]).await;
    assert_eq!(again.status, StatusCode::OK);
    assert!(is_mp3(&again.body));
    assert_eq!(
        w.runner.builds(),
        1,
        "every request joined the one build, which went on"
    );
}

#[tokio::test]
async fn the_wait_counts_from_the_request_so_a_slow_probe_still_answers_in_time() {
    let m4a = made_or_skip!(&steps(6), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a.clone())],
        runner: TestRunner {
            shut: true,
            ..Default::default()
        },
        unpinned: true,
        slow_probe: Some(Duration::from_millis(600)),
        ..Default::default()
    })
    .await;
    w.shim_answers(VIDEO, &m4a);
    let started = StdInstant::now();
    let desktop = w
        .ask(
            &format!("format=opus&maxBitRate=96&{OCTO}"),
            &[("User-Agent", "Octo")],
        )
        .await;
    let took = started.elapsed();
    assert_eq!(desktop.status, StatusCode::OK);
    assert_eq!(
        desktop.header("content-type"),
        Some("audio/mp4"),
        "the filling copy, as it is"
    );
    assert!(took < Duration::from_millis(1400), "{took:?}");
}

#[tokio::test]
async fn a_copy_that_cannot_be_made_answers_the_stream_as_it_is() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    w.shim_answers(VIDEO, &m4a);
    let reply = w.ask(&format!("format=mp3&maxBitRate=128&{SIGNED}"), &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("audio/mp4"));
    assert_eq!(reply.body.as_ref(), m4a.as_slice());
    assert_eq!(w.runner.builds(), 0);
}

#[tokio::test]
async fn an_ask_the_source_already_meets_gets_the_source() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a.clone())],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    w.facts("140", None, None);
    w.shim_answers(VIDEO, &m4a);
    for query in ["", "format=raw&", "format=aac&maxBitRate=256&", "maxBitRate=192&"] {
        let reply = w.ask(&format!("{query}{SIGNED}"), &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}");
        assert_eq!(reply.header("content-type"), Some("audio/mp4"), "{query}");
        assert_eq!(reply.body.as_ref(), m4a.as_slice(), "{query}");
    }
    assert_eq!(w.runner.builds(), 0);
}

#[tokio::test]
async fn opus_in_webm_asked_as_opus_is_copied_into_ogg() {
    let webm = made_or_skip!(&steps(4), WEBM, "webm");
    let w = world(Setup {
        copies: vec![(VIDEO, webm)],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    w.facts("251", Some(130), None);
    let reply = w
        .ask(
            &format!("format=opus&maxBitRate=160&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("audio/ogg"));
    assert!(is_ogg_opus(&reply.body));
}

#[tokio::test]
async fn head_before_a_build_names_the_type_and_starts_nothing() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    let uri = format!(
        "/rest/stream?id=external-track&format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}"
    );
    let head = send(&app(&w.state), Method::HEAD, &uri, &[], Body::empty()).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-type"), Some("audio/mpeg"));
    assert_eq!(head.header("content-length"), None);
    assert_eq!(w.runner.builds(), 0, "a HEAD starts nothing");
    let whole = get(&app(&w.state), &uri).await;
    let head = send(&app(&w.state), Method::HEAD, &uri, &[], Body::empty()).await;
    assert_eq!(length_of(&head), Some(whole.body.len()));
}

#[tokio::test]
async fn head_while_a_conversion_is_remembered_as_failed_names_the_source() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a.clone())],
        runner: TestRunner {
            fail_when: Some("libmp3lame"),
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    w.shim_answers(VIDEO, &m4a);
    let query = format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}");
    let first = w.ask(&query, &[]).await;
    assert_eq!(first.header("content-type"), Some("audio/mp4"));
    let head = send(
        &app(&w.state),
        Method::HEAD,
        &format!("/rest/stream?id=external-track&{query}"),
        &[],
        Body::empty(),
    )
    .await;
    assert_eq!(head.header("content-type"), Some("audio/mp4"));
}

#[tokio::test]
async fn a_kept_flac_asked_as_small_opus_is_made_opus_on_both_file_paths() {
    let flac = made_or_skip!(&steps(4), FLAC, "flac");
    // Wait for lossless on play serves the song's own file.
    let w = world(Setup {
        runner: TestRunner::open(),
        wait_for_lossless: true,
        ..Default::default()
    })
    .await;
    let path = w.dir.path().join("kept.flac");
    std::fs::write(&path, &flac).expect("a kept file");
    w.library.local_paths.lock().insert(
        ("soulseek".into(), w.id.clone()),
        path.to_string_lossy().into_owned(),
    );
    let reply = w
        .ask(
            &format!("format=opus&maxBitRate=96&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert_eq!(reply.header("content-type"), Some("audio/ogg"));
    assert!(is_ogg_opus(&reply.body));
    // Octo's desktop plays a kept file through its own path.
    let w = world(Setup {
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    let path = w.dir.path().join("kept.flac");
    std::fs::write(&path, &flac).expect("a kept file");
    w.library.local_paths.lock().insert(
        ("soulseek".into(), w.id.clone()),
        path.to_string_lossy().into_owned(),
    );
    let reply = w
        .ask(
            &format!("format=opus&maxBitRate=96&{OCTO}"),
            &[("User-Agent", "Octo")],
        )
        .await;
    assert_eq!(reply.header("content-type"), Some("audio/ogg"));
    assert!(is_ogg_opus(&reply.body));
}

#[tokio::test]
async fn a_converted_play_is_heard_as_its_video_never_its_copy() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a.clone())],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    let reply = w
        .ask(
            &format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert!(is_mp3(&reply.body));
    let record = w
        .state
        .external_id_registry
        .heard_by(&w.id, "alice")
        .expect("heard");
    assert_eq!(
        record.source,
        HeardSource::Video {
            id: VIDEO.into(),
            size: Some(m4a.len() as u64),
            content_type: "audio/mp4".into(),
            itag: None,
        }
    );
}

#[tokio::test]
async fn a_wait_for_lossless_play_asked_as_mp3_gets_mp3_from_its_video() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner::open(),
        wait_for_lossless: true,
        ..Default::default()
    })
    .await;
    let reply = w
        .ask(
            &format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert!(is_mp3(&reply.body), "MP3, not the FLAC stand-in");
}

#[tokio::test]
async fn a_failed_conversion_of_a_wait_for_lossless_play_gets_the_flac_stand_in() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner {
            fail_when: Some("libmp3lame"),
            ..Default::default()
        },
        wait_for_lossless: true,
        ..Default::default()
    })
    .await;
    let reply = w
        .ask(
            &format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert!(reply.body.starts_with(b"fLaC"), "the stand-in as before");
}

#[tokio::test]
async fn a_conversion_that_breaks_off_is_built_once_more_then_refused() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a.clone())],
        runner: TestRunner {
            short: true,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    w.shim_answers(VIDEO, &m4a);
    let request = Request::builder()
        .uri(format!(
            "/rest/stream?id=external-track&format=mp3&maxBitRate=128&{SIGNED}"
        ))
        .body(Body::empty())
        .expect("a request");
    let response = app(&w.state).oneshot(request).await.expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.into_body().collect().await.is_err(),
        "a cut transfer, never a clean end"
    );
    let later = w
        .ask(
            &format!("format=mp3&maxBitRate=128&{SIGNED}"),
            &[("Range", "bytes=100-")],
        )
        .await;
    assert_eq!(later.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(w.runner.builds(), 2, "one rebuild");
    let next = w
        .ask(
            &format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert_eq!(
        next.header("content-type"),
        Some("audio/mp4"),
        "a new play gets the source"
    );
    assert_eq!(w.runner.builds(), 2, "no build while the failure is remembered");
}

#[tokio::test]
async fn a_play_that_got_the_source_keeps_it_when_its_conversion_fails() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a.clone())],
        runner: TestRunner {
            shut: true,
            short: true,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    w.shim_answers(VIDEO, &m4a);
    let exact = format!("format=mp3&maxBitRate=128&estimateContentLength=true&{OCTO}");
    let first = w.ask(&exact, &[("User-Agent", PHONE)]).await;
    assert_eq!(first.header("content-type"), Some("audio/mp4"));
    w.runner.door.open();
    let later = w
        .ask(&exact, &[("User-Agent", PHONE), ("Range", "bytes=100-")])
        .await;
    assert!(later.status.is_success(), "{}", later.status);
    assert_eq!(later.header("content-type"), Some("audio/mp4"));
}

#[tokio::test]
async fn a_range_past_the_sources_end_but_inside_its_conversion_is_answered() {
    let m4a = made_or_skip!(&steps(6), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a.clone())],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    w.facts("140", None, Some(m4a.len() as u64));
    let range = format!("bytes={}-", m4a.len() + 10);
    let reply = w
        .ask(
            &format!("format=mp3&maxBitRate=320&{SIGNED}"),
            &[("Range", range.as_str())],
        )
        .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
}

#[tokio::test]
async fn a_download_honors_its_bitrate() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    let reply = get(
        &app(&w.state),
        &format!("/rest/download?id=external-track&format=mp3&bitrate=96&{SIGNED}"),
    )
    .await;
    assert!(is_mp3(&reply.body));
    // MPEG-1 Layer III bit rate index 7 is 96 kbps.
    assert_eq!(reply.body[2] >> 4, 0b0111);
}

#[tokio::test]
async fn a_new_play_takes_the_better_video_waiting_for_it() {
    let a = made_or_skip!(&steps(4), M4A, "m4a");
    let b = made_or_skip!("sine=frequency=220:duration=4", M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, a), (OTHER_VIDEO, b.clone())],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    assert!(
        w.state
            .external_id_registry
            .set_pending_video(&w.id, VIDEO, OTHER_VIDEO)
    );
    let reply = w
        .ask(
            &format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert!(is_mp3(&reply.body));
    let record = w
        .state
        .external_id_registry
        .heard_by(&w.id, "alice")
        .expect("heard");
    assert_eq!(record.source.video_id(), Some(OTHER_VIDEO));
}

// ---- Time seeks ----------------------------------------------------------------------------

#[tokio::test]
async fn a_time_seek_starts_the_sound_at_its_place() {
    let m4a = made_or_skip!(&steps(40), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    w.facts("140", None, None);
    let checks: [(&str, fn(&[u8]) -> bool); 3] = [
        ("format=opus&maxBitRate=96", is_ogg_opus),
        ("format=mp3&maxBitRate=128", is_mp3),
        ("format=aac&maxBitRate=256", is_adts),
    ];
    for (query, check) in checks {
        let reply = w.ask(&format!("{query}&timeOffset=30&{SIGNED}"), &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}");
        assert!(check(&reply.body), "{query}");
        assert_eq!(reply.header("content-length"), None, "{query}");
        let samples = decoded(&reply.body);
        let seconds = samples.len() as f64 / 44_100.0;
        assert!((seconds - 10.0).abs() < 0.15, "{query}: {seconds} s");
        let hz = tone_at(&samples, 0.05);
        assert!((hz - 1640.0).abs() < 164.0, "{query}: {hz} Hz");
    }
}

#[tokio::test]
async fn a_time_seek_of_a_file_asked_as_it_is_keeps_todays_bytes() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    w.shim_answers(VIDEO, &m4a);
    let reply = w.ask(&format!("format=raw&timeOffset=30&{SIGNED}"), &[]).await;
    assert_eq!(reply.body.as_ref(), m4a.as_slice());
    assert_eq!(reply.header("content-type"), Some("audio/mp4"));
}

#[tokio::test]
async fn a_time_seek_of_a_playing_song_is_not_a_new_play() {
    let m4a = made_or_skip!(&steps(40), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    let first = w
        .ask(
            &format!("format=opus&maxBitRate=96&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert!(is_ogg_opus(&first.body));
    let heard = w.heard_at().expect("the play is heard");
    let seek = w
        .ask(&format!("format=opus&maxBitRate=96&timeOffset=20&{SIGNED}"), &[])
        .await;
    assert_eq!(seek.status, StatusCode::OK);
    assert_eq!(w.heard_at(), Some(heard), "a seek keeps no new record");
}

#[tokio::test]
async fn a_song_restored_at_its_place_is_a_play() {
    let m4a = made_or_skip!(&steps(40), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    let restored = w
        .ask(&format!("format=opus&maxBitRate=96&timeOffset=25&{SIGNED}"), &[])
        .await;
    assert_eq!(restored.status, StatusCode::OK);
    let heard = w.heard_at().expect("a song opened at its place is heard");
    let seek = w
        .ask(&format!("format=opus&maxBitRate=96&timeOffset=30&{SIGNED}"), &[])
        .await;
    assert_eq!(seek.status, StatusCode::OK);
    assert_eq!(w.heard_at(), Some(heard));
}

#[tokio::test]
async fn a_time_seek_without_its_conversion_copies_the_source_from_its_place() {
    let m4a = made_or_skip!(&steps(40), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner {
            fail_when: Some("libopus"),
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let first = w
        .ask(
            &format!("format=opus&maxBitRate=96&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert_eq!(
        first.header("content-type"),
        Some("audio/mp4"),
        "the conversion failed"
    );
    let seek = w
        .ask(&format!("format=opus&maxBitRate=96&timeOffset=30&{SIGNED}"), &[])
        .await;
    assert_eq!(seek.status, StatusCode::OK);
    assert_eq!(seek.header("content-type"), Some("audio/aac"));
    assert!(is_adts(&seek.body));
    let seconds = decoded(&seek.body).len() as f64 / 44_100.0;
    assert!((seconds - 10.0).abs() < 0.15, "{seconds} s");
}

#[tokio::test]
async fn a_time_seek_ffmpeg_cannot_run_is_refused() {
    let m4a = made_or_skip!(&steps(40), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner {
            broken: true,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let seek = w
        .ask(&format!("format=opus&maxBitRate=96&timeOffset=30&{SIGNED}"), &[])
        .await;
    assert_eq!(seek.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn a_time_seek_past_the_filled_part_waits_for_it_then_gives_up() {
    let m4a = made_or_skip!(&steps(20), M4A, "m4a");
    // About 350 KB in 8 KB pieces: at 100 ms a piece the place at 15 s comes after 3 s.
    let w = world(Setup {
        runner: TestRunner::open(),
        shim: Some(Arc::new(Paced {
            bytes: m4a.clone(),
            pace: Duration::from_millis(100),
        })),
        ..Default::default()
    })
    .await;
    w.facts("140", None, Some(m4a.len() as u64));
    let started = StdInstant::now();
    let slow = w
        .ask(&format!("format=opus&maxBitRate=96&timeOffset=15&{SIGNED}"), &[])
        .await;
    assert_eq!(slow.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        started.elapsed() < Duration::from_millis(3500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_time_seek_that_cannot_name_its_video_is_refused_never_played_from_the_start() {
    let w = world(Setup {
        runner: TestRunner::open(),
        unpinned: true,
        ..Default::default()
    })
    .await;
    // No pin and a shim that names nothing: today's path would answer from the first byte.
    let seek = w
        .ask(
            &format!("format=opus&maxBitRate=96&timeOffset=30&{OCTO}"),
            &[("User-Agent", "Octo")],
        )
        .await;
    assert_eq!(seek.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(seek.header("retry-after"), Some("5"));
}

/// The same listener in a second app: another play of the song.
const SECOND_APP: &str = "u=alice&t=good&s=salt&v=1.16.1&c=second";

#[tokio::test]
async fn a_time_seek_lets_its_permit_go_at_its_first_sound() {
    let m4a = made_or_skip!(&steps(40), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner {
            seeks_hang: true,
            ..TestRunner::open()
        },
        permits: Some(1),
        ..Default::default()
    })
    .await;
    w.facts("140", None, None);
    // One seek permit: the first play's seek keeps playing (its body unread, its run alive).
    let playing = w
        .open(&format!("format=opus&maxBitRate=96&timeOffset=30&{SIGNED}"))
        .await;
    assert_eq!(playing.status(), StatusCode::OK);
    let other = w
        .open(&format!("format=opus&maxBitRate=96&timeOffset=20&{SECOND_APP}"))
        .await;
    assert_eq!(
        other.status(),
        StatusCode::OK,
        "the playing seek no longer holds the permit"
    );
    assert_eq!(w.runner.seeks(), 2);
    drop(playing);
}

#[tokio::test]
async fn a_newer_seek_of_a_play_stops_the_older_before_it_waits() {
    let m4a = made_or_skip!(&steps(40), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner {
            first_seek_silent: true,
            ..TestRunner::open()
        },
        permits: Some(1),
        ..Default::default()
    })
    .await;
    w.facts("140", None, None);
    let request = Request::builder()
        .uri(format!(
            "/rest/stream?id=external-track&format=opus&maxBitRate=96&timeOffset=30&{SIGNED}"
        ))
        .body(Body::empty())
        .expect("a request");
    let older = tokio::spawn(app(&w.state).oneshot(request));
    // The older seek holds the one permit and waits for a first sound that never comes.
    let until = StdInstant::now() + Duration::from_secs(5);
    while w.runner.seeks() == 0 {
        assert!(StdInstant::now() < until, "the older seek started its run");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = StdInstant::now();
    let newer = w
        .open(&format!("format=opus&maxBitRate=96&timeOffset=20&{SIGNED}"))
        .await;
    assert_eq!(newer.status(), StatusCode::OK);
    // Well inside the 2 s seek wait: the older seek let go of its permit when the newer one began.
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    let older = older.await.expect("the older seek ran").expect("infallible");
    assert_eq!(older.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(w.runner.seeks(), 2);
}

#[tokio::test]
async fn a_newer_seek_of_a_play_stops_the_older_while_it_waits_for_a_permit() {
    let m4a = made_or_skip!(&steps(40), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner {
            first_seek_silent: true,
            ..TestRunner::open()
        },
        permits: Some(1),
        ..Default::default()
    })
    .await;
    w.facts("140", None, None);
    let spawn = |query: String| {
        let request = Request::builder()
            .uri(format!("/rest/stream?id=external-track&{query}"))
            .body(Body::empty())
            .expect("a request");
        tokio::spawn(app(&w.state).oneshot(request))
    };
    let until = StdInstant::now() + Duration::from_secs(5);
    // Another play holds the one permit, waiting for a first sound that never comes.
    let holder = spawn(format!("format=opus&maxBitRate=96&timeOffset=30&{SECOND_APP}"));
    while w.runner.seeks() == 0 {
        assert!(StdInstant::now() < until, "the other play's seek started its run");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // This play's older seek waits for that permit.
    let older = spawn(format!("format=opus&maxBitRate=96&timeOffset=30&{SIGNED}"));
    while w.state.converted_copies.waiting_for_seek_permits() == 0 {
        assert!(StdInstant::now() < until, "the older seek waits for a permit");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = StdInstant::now();
    let newer = spawn(format!("format=opus&maxBitRate=96&timeOffset=20&{SIGNED}"));
    let older = older.await.expect("the older seek ran").expect("infallible");
    // Stopped by the newer seek (503 with no Retry-After), not given up after the 2 s wait (busy).
    assert_eq!(older.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(older.headers().get("retry-after"), None);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    let _ = newer.await;
    let _ = holder.await;
}

#[tokio::test]
async fn a_missing_encoder_plays_the_song_as_it_is_and_builds_nothing() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a.clone())],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    w.shim_answers(VIDEO, &m4a);
    // This ffmpeg makes MP3 only.
    w.state
        .converted_copies
        .note_encoders(" A....D libmp3lame           libmp3lame MP3 (MPEG audio layer 3)\n");
    let reply = w.ask(&format!("format=opus&maxBitRate=96&{SIGNED}"), &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("audio/mp4"));
    assert_eq!(reply.body.as_ref(), m4a.as_slice());
    assert_eq!(w.runner.builds(), 0, "no Opus build was tried");
    let mp3 = w
        .ask(
            &format!("format=mp3&maxBitRate=96&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert!(is_mp3(&mp3.body), "the encoder it has still converts");
}

/// An M4A with its `mdat` before its `moov`: not YouTube's layout, and not readable from a pipe.
const M4A_PLAIN: &[&str] = &["-c:a", "aac", "-b:a", "128k", "-f", "mp4"];

#[tokio::test]
async fn an_m4a_that_cannot_stream_from_its_start_is_converted_from_the_whole_copy() {
    // 20 s (about 300 KB): ffmpeg reads a short mdat-first file whole from a pipe and copes, so
    // the file must be longer than its read buffer for the pipe to fail.
    let plain = made_or_skip!(&steps(20), M4A_PLAIN, "m4a");
    assert_eq!(
        convert_rules::mp4_streams_from_start(&plain[..plain.len().min(64 * 1024)]),
        Some(false),
        "the fixture puts its mdat first"
    );
    let w = world(Setup {
        runner: TestRunner::open(),
        shim: Some(Arc::new(Paced {
            bytes: plain.clone(),
            pace: Duration::from_millis(5),
        })),
        ..Default::default()
    })
    .await;
    w.facts("140", None, Some(plain.len() as u64));
    let reply = w
        .ask(
            &format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}"),
            &[],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("audio/mpeg"));
    assert!(
        is_mp3(&reply.body),
        "fed through a pipe it fails and the M4A comes back"
    );
    assert_eq!(length_of(&reply), Some(reply.body.len()));
    let seconds = decoded(&reply.body).len() as f64 / 44_100.0;
    assert!(
        (seconds - 20.0).abs() < 0.2,
        "the whole song, not a cut one: {seconds} s"
    );
}

#[tokio::test]
async fn a_player_that_cannot_wait_for_a_whole_m4a_is_told_to_try_again_and_the_build_waits_for_it() {
    let plain = made_or_skip!(&steps(20), M4A_PLAIN, "m4a");
    // About 38 pieces at 100 ms: the copy is whole after the other clients' 2 s wait.
    let w = world(Setup {
        runner: TestRunner::open(),
        shim: Some(Arc::new(Paced {
            bytes: plain.clone(),
            pace: Duration::from_millis(100),
        })),
        ..Default::default()
    })
    .await;
    w.facts("140", None, Some(plain.len() as u64));
    let query = format!("format=mp3&maxBitRate=128&estimateContentLength=true&{SIGNED}");
    let first = w.ask(&query, &[]).await;
    assert_eq!(
        first.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "never the M4A it cannot play"
    );
    assert_eq!(first.header("retry-after"), Some("5"));
    w.state
        .preview_copies
        .wait_complete(VIDEO)
        .await
        .expect("the copy completes");
    let again = w.ask(&query, &[]).await;
    assert_eq!(again.status, StatusCode::OK);
    assert!(is_mp3(&again.body));
    assert_eq!(
        w.runner.builds(),
        1,
        "the retry joined the build the first request left"
    );
}

// ---- The family gate's ask reaches the funnel ------------------------------------------------

#[tokio::test]
async fn a_gate_ceiling_reaches_the_funnel_as_the_clients_own_ask() {
    let state = AppState::for_tests(settings("http://127.0.0.1:9"));
    let mut request = axum::http::Request::builder()
        .uri("/rest/stream?id=x&format=mp3&maxBitRate=96&estimateContentLength=true&c=Octo")
        .body(())
        .expect("a request");
    request.extensions_mut().insert(OwnAsk {
        format: Some("raw".into()),
        rate: None,
        ceiling: Some(96),
        client: GateClient::OctoPhone,
    });
    let (parts, ()) = request.into_parts();
    let call = SubsonicCall::from_parts(&state, &parts, Bytes::new());
    let own = call.own_ask.clone().expect("carried to the handler");
    let asked = convert_rules::ask_of(&call.parameters, "stream", Some(&own), own.client);
    assert_eq!(
        asked.ask,
        Ask::Cap {
            kbps: 96,
            prefer_opus: false
        },
        "the gate's mp3 is not the client's ask"
    );
    let m4a = convert_rules::source_of_type("audio/mp4", Some(128));
    assert_eq!(
        convert_rules::plan(
            &Ask::Cap {
                kbps: 160,
                prefer_opus: false
            },
            &m4a
        ),
        Plan::AsIs
    );
    let flac = convert_rules::source_of_type("audio/flac", None);
    assert_eq!(
        convert_rules::plan(&asked.ask, &flac),
        Plan::Convert(Target {
            codec: Codec::Mp3,
            kbps: 96
        })
    );
}

// ---- OpenSubsonic transcoding ------------------------------------------------------------

#[tokio::test]
async fn an_outside_song_gets_a_real_transcode_decision_and_stream() {
    let m4a = made_or_skip!(&steps(4), M4A, "m4a");
    let w = world(Setup {
        copies: vec![(VIDEO, m4a)],
        runner: TestRunner::open(),
        ..Default::default()
    })
    .await;
    w.facts("140", None, None);
    let decide = |body: &'static str| {
        let app = app(&w.state);
        async move {
            send(
                &app,
                Method::POST,
                &format!("/rest/getTranscodeDecision?mediaId=external-track&mediaType=song&f=json&{SIGNED}"),
                &[("Content-Type", "application/json")],
                Body::from(body),
            )
            .await
        }
    };
    let direct = decide(
        r#"{"directPlayProfiles":[{"containers":["mp4","m4a"],"audioCodecs":["aac"],"protocols":["http"],"maxAudioChannels":2}]}"#,
    )
    .await;
    assert_eq!(direct.envelope()["transcodeDecision"]["canDirectPlay"], true);
    let mp3_only = decide(
        r#"{"maxTranscodingAudioBitrate":128000,"directPlayProfiles":[{"containers":["mp3"],"audioCodecs":["mp3"]}],"transcodingProfiles":[{"container":"mp3","audioCodec":"mp3","protocol":"http","maxAudioChannels":2}]}"#,
    )
    .await;
    let decision = &mp3_only.envelope()["transcodeDecision"];
    assert_eq!(decision["canDirectPlay"], false);
    assert_eq!(decision["canTranscode"], true);
    assert_eq!(decision["transcodeParams"], "octo1-mp3-128");
    let stream = get(
        &app(&w.state),
        &format!("/rest/getTranscodeStream?mediaId=external-track&mediaType=song&transcodeParams=octo1-mp3-128&{SIGNED}"),
    )
    .await;
    assert_eq!(stream.status, StatusCode::OK);
    assert!(is_mp3(&stream.body));
    let bad = get(
        &app(&w.state),
        &format!(
            "/rest/getTranscodeStream?mediaId=external-track&mediaType=song&transcodeParams=tp&{SIGNED}"
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}
