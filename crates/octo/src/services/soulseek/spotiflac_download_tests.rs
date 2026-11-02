//! Mornye as a download source, end to end through the download service, against a stand-in
//! Mornye that writes a FLAC into the folder it is asked to and a stand-in slskd that finds
//! nothing: off by default, a found file checked and placed, a wrong file refused, a job that
//! fails or never ends, and where Mornye sits against Soulseek.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use octo_core::models::domain::Song;
use octo_core::notifications::NotificationEventType;
use octo_core::settings::{
    AppSettings, DownloadSource, SettingsStore, SoulseekSettings, SpotiflacPlacement, SpotiflacSettings,
    SubsonicSettings,
};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::common::{AcquisitionTracker, DownloadConcurrency, DownloadCore, DownloadServices};
use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
use crate::services::i_download_service::IDownloadService;
use crate::services::local::DownloadHistoryService;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::notifications::{INotificationSink, NotificationService};
use crate::services::soulseek::soulseek_client::Timings;
use crate::services::spotiflac::mornye_waited;
use crate::services::subsonic::NavidromeIdentityService;
use octo_media::audio::AudioFingerprinter;

/// A FLAC whose STREAMINFO says it lasts this long, with enough bytes after it to be read.
fn flac(seconds: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"fLaC");
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    let (sample_rate, channels_minus_one, bits_minus_one): (u64, u64, u64) = (44100, 1, 15);
    let total_samples = 44100u64 * seconds;
    let packed = (sample_rate << 44) | (channels_minus_one << 41) | (bits_minus_one << 36) | total_samples;
    bytes.extend_from_slice(&packed.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    // Some audio after the STREAMINFO: the tag reader reads no length from a FLAC without any.
    bytes.extend_from_slice(&[0u8; 48_000]);
    bytes
}

fn json_answer(body: &Value, status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.to_string().into_bytes(), "application/json")
}

/// What happened, across both stand-ins, in order.
type Events = Arc<Mutex<Vec<String>>>;

/// Mornye as far as one song needs.
#[derive(Clone)]
struct FakeSpotiflac {
    tracks: Arc<dyn Fn(&str) -> Vec<Value> + Send + Sync>,
    /// The file a download writes, its name and bytes; None writes nothing.
    file: Option<(String, Vec<u8>)>,
    /// The state every look at the job answers, and its error.
    status: &'static str,
    error: Option<&'static str>,
    /// The state the job's item answers when it is not the job's own.
    item_status: Option<&'static str>,
    /// The folder the last download was asked to write into.
    job_folder: Arc<Mutex<String>>,
    events: Events,
    /// Mornye's signed sessions, one answer per read, the last one repeating; none answers 404.
    sessions: Vec<Value>,
    sessions_read: Arc<AtomicUsize>,
    /// How long Mornye takes to answer a check read, from read number `sessions_delay_from` on.
    sessions_delay: Option<Duration>,
    sessions_delay_from: usize,
    /// Mornye's download services; None answers 404.
    services: Option<Value>,
    /// Mornye's extension health, one answer per read, the last one repeating; none answers 404.
    extensions: Vec<Value>,
    extensions_read: Arc<AtomicUsize>,
    /// The extension a failed job's item names.
    provider: Option<&'static str>,
    /// Every search answers 500, as Mornye 1.1 does without its utilities extension.
    search_fails: bool,
    /// After the first health read, example-ext's last try failed writing into the job's folder.
    write_refused: bool,
}

/// The answer for read number `read` of `answers`: the last one repeats.
fn in_turn(answers: &[Value], read: &AtomicUsize) -> ResponseTemplate {
    let n = read.fetch_add(1, Ordering::SeqCst);
    match answers.get(n).or(answers.last()) {
        Some(answer) => json_answer(answer, 200),
        None => ResponseTemplate::new(404),
    }
}

impl Respond for FakeSpotiflac {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let method = request.method.as_str();
        if path == "/api/get_signed_sessions" {
            let read = self.sessions_read.load(Ordering::SeqCst);
            let answer = in_turn(&self.sessions, &self.sessions_read);
            return match self.sessions_delay {
                Some(delay) if read >= self.sessions_delay_from => answer.set_delay(delay),
                _ => answer,
            };
        }
        if path == "/api/get_download_services" {
            return self
                .services
                .as_ref()
                .map_or_else(|| ResponseTemplate::new(404), |answer| json_answer(answer, 200));
        }
        if path == "/api/v1/extensions" && self.write_refused {
            let read = self.extensions_read.fetch_add(1, Ordering::SeqCst);
            let folder = self.job_folder.lock().clone();
            return if read == 0 {
                json_answer(&health(3, 1, "old"), 200)
            } else {
                json_answer(
                    &health(
                        4,
                        2,
                        &format!("EACCES: permission denied, open '{folder}/x.flac'"),
                    ),
                    200,
                )
            };
        }
        if path == "/api/v1/extensions" {
            return in_turn(&self.extensions, &self.extensions_read);
        }
        if path == "/api/v1/search" && self.search_fails {
            return json_answer(
                &json!({"error": "internal_error", "detail": "search failed"}),
                500,
            );
        }
        if path == "/api/v1/search" {
            let query = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "q")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            self.events.lock().push(format!("spotiflac search {query}"));
            return json_answer(&json!({"query": query, "tracks": (self.tracks)(&query)}), 200);
        }
        if path == "/api/v1/downloads" && method == "POST" {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let folder = body["output_dir"].as_str().unwrap_or_default().to_string();
            self.events.lock().push(format!(
                "spotiflac download {}",
                body["url"].as_str().unwrap_or_default()
            ));
            *self.job_folder.lock() = folder.clone();
            if let Some((name, bytes)) = &self.file {
                std::fs::write(std::path::Path::new(&folder).join(name), bytes).expect("written");
            }
            return json_answer(
                &json!({"id": "job-1", "status": "queued", "created_at": 1.0}),
                202,
            );
        }
        if path == "/api/v1/downloads/job-1" {
            if let Some(item_status) = self.item_status {
                let result = format!("{}/song.flac", self.job_folder.lock());
                return json_answer(
                    &json!({"id": "job-1", "status": self.status, "created_at": 1.0,
                        "payload": {"items_detail": [{"status": item_status, "result": result}]}}),
                    200,
                );
            }
            return json_answer(
                &json!({"id": "job-1", "status": self.status, "created_at": 1.0, "error": self.error,
                    "payload": {"items_detail": [{"status": self.status.to_uppercase(), "error": self.error,
                        "provider": self.provider}]}}),
                200,
            );
        }
        if path == "/api/v1/downloads/job-1/cancel" {
            self.events.lock().push("spotiflac cancel".into());
            return json_answer(
                &json!({"id": "job-1", "status": "cancelled", "created_at": 1.0}),
                200,
            );
        }
        ResponseTemplate::new(404)
    }
}

/// slskd, logged in, finding nothing for any search.
#[derive(Clone)]
struct EmptySlskd {
    root: String,
    events: Events,
}

impl Respond for EmptySlskd {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let method = request.method.as_str();
        if path == "/api/v0/session" {
            return json_answer(&json!({"token": "jwt", "expires": 4102444800i64}), 200);
        }
        if path == "/api/v0/options" {
            return json_answer(
                &json!({"directories": {"downloads": self.root, "incomplete": "/app/incomplete"}}),
                200,
            );
        }
        if path == "/api/v0/searches" && method == "POST" {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            self.events.lock().push(format!(
                "slskd search {}",
                body["searchText"].as_str().unwrap_or_default()
            ));
            return json_answer(&json!({}), 200);
        }
        if path.starts_with("/api/v0/searches/") {
            return if path.ends_with("/responses") {
                json_answer(&json!([]), 200)
            } else {
                json_answer(
                    &json!({"state": "Completed, TimedOut", "endedAt": "2026-10-04T12:00:00Z", "responseCount": 0}),
                    200,
                )
            };
        }
        json_answer(&json!({}), 200)
    }
}

/// Every notification sent, by its kind.
#[derive(Default)]
struct SentNotifications(Mutex<Vec<NotificationEventType>>, Mutex<Vec<String>>);

#[async_trait::async_trait]
impl INotificationSink for SentNotifications {
    fn name(&self) -> &str {
        "recorded"
    }

    fn is_configured(&self) -> bool {
        true
    }

    async fn send(&self, message: &octo_core::notifications::NotificationMessage) -> anyhow::Result<()> {
        self.0.lock().push(message.r#type);
        self.1.lock().push(message.body.clone());
        Ok(())
    }
}

/// The song, the service and the stand-ins.
struct Fetch {
    service: Arc<BaseDownloadService>,
    song: Song,
    events: Events,
    root: tempfile::TempDir,
    spotiflac: Arc<SpotiflacClient>,
    sent: Arc<SentNotifications>,
    tracker: Arc<AcquisitionTracker>,
    _spotiflac: MockServer,
    _slskd: MockServer,
}

struct Setup {
    spotiflac: SpotiflacSettings,
    soulseek_on: bool,
    fake: FakeSpotiflac,
    artist: &'static str,
    title: &'static str,
    seconds: i32,
    isrc: Option<&'static str>,
    /// How long a read of Mornye's check stays fresh (zero reads it again at every look).
    check_fresh_for: Duration,
    /// How long each call of a check read may take.
    check_call_bound: Duration,
}

const ISRC: &str = "USCM51100524";

fn take_care(title: &str, seconds: i64, isrc: &str) -> Value {
    json!({"id": "6LxSe8YmdPxy095Ux6znaQ", "title": title, "artists": "Drake, Rihanna", "album": "Take Care",
        "isrc": isrc, "duration_ms": seconds * 1000, "external_url": "https://open.spotify.com/track/6LxSe8YmdPxy095Ux6znaQ"})
}

fn on() -> SpotiflacSettings {
    SpotiflacSettings {
        enabled: true,
        url: "set below".into(),
        ..Default::default()
    }
}

impl Setup {
    fn new(events: &Events) -> Setup {
        Setup {
            spotiflac: on(),
            soulseek_on: false,
            fake: FakeSpotiflac {
                tracks: Arc::new(|_| vec![take_care("Take Care", 277, ISRC)]),
                file: Some(("Drake - Take Care.flac".into(), flac(277))),
                status: "done",
                error: None,
                item_status: None,
                job_folder: Arc::default(),
                events: events.clone(),
                sessions: Vec::new(),
                sessions_read: Arc::default(),
                sessions_delay: None,
                sessions_delay_from: 0,
                services: None,
                extensions: Vec::new(),
                extensions_read: Arc::default(),
                provider: None,
                search_fails: false,
                write_refused: false,
            },
            artist: "Drake",
            title: "Take Care",
            seconds: 277,
            isrc: Some(ISRC),
            check_fresh_for: Duration::from_secs(60),
            check_call_bound: Duration::from_millis(200),
        }
    }

    async fn build(mut self, events: Events) -> Fetch {
        let root = tempfile::Builder::new()
            .prefix("octo-spotiflac-")
            .tempdir()
            .expect("a temp folder");
        let root_path = root.path().to_string_lossy().into_owned();
        let spotiflac_server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(self.fake.clone())
            .mount(&spotiflac_server)
            .await;
        let slskd_server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(EmptySlskd {
                root: root_path.clone(),
                events: events.clone(),
            })
            .mount(&slskd_server)
            .await;
        if self.spotiflac.url == "set below" {
            self.spotiflac.url = spotiflac_server.uri();
        }

        let registry = Arc::new(ExternalIdRegistry::in_memory());
        let id = registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some(self.artist.into()),
            title: Some(self.title.into()),
            album: Some("Album".into()),
            duration: Some(self.seconds),
            isrc: self.isrc.map(str::to_string),
            ..Default::default()
        });
        let song = Song {
            id: format!("ext-soulseek-{id}"),
            external_provider: Some("soulseek".into()),
            external_id: Some(id.clone()),
            title: self.title.into(),
            artist: self.artist.into(),
            album: "Album".into(),
            duration: Some(self.seconds),
            isrc: self.isrc.map(str::to_string),
            ..Default::default()
        };
        let soulseek = SoulseekSettings {
            base_url: self.soulseek_on.then(|| slskd_server.uri()),
            username: Some("u".into()),
            password: Some("p".into()),
            min_file_size_bytes: 0,
            album_folders: false,
            parallel_downloads: 1,
            detect_transcodes: false,
            verify_downloads: false,
            download_timeout_seconds: 30,
            ..Default::default()
        };
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            soulseek: soulseek.clone(),
            subsonic: SubsonicSettings {
                auto_detect_download_path: false,
                download_source: DownloadSource::Soulseek,
                ..Default::default()
            },
            spotiflac: self.spotiflac.clone(),
            notifications: octo_core::settings::NotificationSettings {
                notify_download_started: true,
                notify_lossless_fallback: true,
                ..Default::default()
            },
            ..Default::default()
        }));
        settings.set_raw("Library:DownloadPath", Some(&root_path));
        let sent = Arc::new(SentNotifications::default());
        let client = SoulseekClient::with_timings(
            &soulseek,
            Timings {
                search_poll_interval: Duration::from_millis(5),
                poll_interval: Duration::from_millis(5),
                min_search_spacing: Duration::ZERO,
                ..Default::default()
            },
        );
        let metadata = FakeMetadata::default();
        metadata
            .songs
            .lock()
            .insert(("soulseek".into(), id), song.clone());
        let core = DownloadCore {
            settings: Arc::clone(&settings),
            local_library: Arc::new(FakeLocalLibrary::default()),
            metadata: Arc::new(metadata),
            navidrome_identity: NavidromeIdentityService::new(Arc::clone(&settings), reqwest::Client::new()),
            history: Arc::new(DownloadHistoryService::new(root.path().join("history.json"))),
            notifications: Arc::new(NotificationService::new(
                vec![Arc::clone(&sent) as Arc<dyn INotificationSink>],
                Arc::clone(&settings),
                None,
            )),
        };
        let tracker = Arc::new(AcquisitionTracker::new(None, octo_core::common::Clock::system()));
        // The song's row, so its download log lines are kept.
        let row_id = song.external_id.clone().expect("an id");
        tracker.begin(
            "soulseek",
            &row_id,
            Some(&row_id),
            Some("alice"),
            None,
            None,
            None,
        );
        let services = DownloadServices {
            concurrency: Some(Arc::new(DownloadConcurrency::new(Arc::clone(&settings)))),
            tracker: Some(Arc::clone(&tracker)),
            ..Default::default()
        };
        let verification = Arc::new(DownloadVerificationService::new(
            Arc::new(AudioFingerprinter::new()),
            Arc::new(AcoustIdClient::new(
                Arc::new(AcoustIdRateLimitHandler::new(
                    Arc::new(AcoustIdRateLimiter::new()),
                )),
                Arc::clone(&settings),
            )),
            Arc::clone(&settings),
            None,
            None,
        ));
        // Check reads bound to a fifth of a second unless a test says, so a hung read is seen in
        // a test's time.
        let check = crate::services::spotiflac::MornyeCheck::new(
            Arc::new(crate::services::spotiflac::MornyeCheckStore::new(None)),
            crate::services::spotiflac::CheckTimes {
                fresh_for: self.check_fresh_for,
                call_bound: self.check_call_bound,
            },
            None,
        );
        let spotiflac = Arc::new(
            SpotiflacClient::with_poll_interval(Arc::clone(&settings), Duration::from_millis(10))
                .with_song_timeout(Duration::from_millis(400))
                .with_check(check),
        );
        let parts = SoulseekDownloadParts {
            slskd: client,
            rejected_peers: Arc::new(RejectedPeerRegistry::in_memory()),
            verification,
            youtube: Arc::new(YouTubeResolver::with_base_url(Some("http://127.0.0.1:1"))),
            id_registry: registry,
            soulseek_link: None,
            lidarr_imports: None,
            lidarr_fetcher: None,
            spotiflac: Some(Arc::clone(&spotiflac)),
            preview_copies: None,
        };
        let (service, _) = SoulseekDownloadService::build(core, services, parts);
        Fetch {
            service,
            song,
            events,
            root,
            spotiflac,
            sent,
            tracker,
            _spotiflac: spotiflac_server,
            _slskd: slskd_server,
        }
    }
}

impl Fetch {
    async fn acquire(&self) -> anyhow::Result<String> {
        self.acquire_from(DownloadSource::Soulseek).await
    }

    async fn acquire_from(&self, source: DownloadSource) -> anyhow::Result<String> {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.service.execute_acquisition(
                "soulseek",
                self.song.external_id.as_deref().expect("an id"),
                false,
                true,
                Some(source),
                &CancellationToken::new(),
                None,
                false,
                None,
            ),
        )
        .await
        .expect("within a minute")
    }

    /// The song's download log, as text and detail.
    fn log_lines(&self) -> Vec<(String, Option<String>)> {
        let key = format!("soulseek:{}", self.song.external_id.as_deref().expect("an id"));
        self.tracker
            .detail(&key, None)
            .and_then(|row| row.events)
            .unwrap_or_default()
            .into_iter()
            .map(|line| (line.text, line.detail))
            .collect()
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().clone()
    }

    fn spotiflac_events(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|event| event.starts_with("spotiflac"))
            .collect()
    }

    /// Every file left in Mornye's job folders.
    fn left_in_job_folders(&self) -> Vec<String> {
        let jobs = self.root.path().join(".octo-incoming/spotiflac");
        if !jobs.is_dir() {
            return Vec::new();
        }
        files_under(&jobs.to_string_lossy()).unwrap_or_default()
    }
}

#[tokio::test]
async fn spotiflac_is_off_by_default_and_never_asked() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.spotiflac = SpotiflacSettings {
        url: "set below".into(),
        ..Default::default()
    };
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("nothing to download with");

    assert!(error.to_string().contains("Soulseek is off"), "{error}");
    assert!(fetch.spotiflac_events().is_empty(), "{:?}", fetch.events());
}

#[tokio::test]
async fn a_found_file_passes_the_checks_and_lands_in_the_library() {
    let events: Events = Arc::default();
    let fetch = Setup::new(&events).build(events).await;

    let path = fetch.acquire().await.expect("Mornye's copy is taken");

    assert!(path.ends_with(".flac"), "{path}");
    assert!(
        !path.contains(".octo-incoming"),
        "placed out of the job folder: {path}"
    );
    assert!(std::path::Path::new(&path).is_file());
    assert_eq!(
        fetch.spotiflac_events(),
        [
            format!("spotiflac search isrc:{ISRC}"),
            "spotiflac download https://open.spotify.com/track/6LxSe8YmdPxy095Ux6znaQ".to_string(),
        ]
    );
    let history = std::fs::read_to_string(fetch.root.path().join("history.json")).expect("history");
    assert!(history.contains("\"Mornye\""), "{history}");
}

#[tokio::test]
async fn a_skipped_item_lands_like_a_done_one() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.file = Some(("song.flac".into(), flac(277)));
    setup.fake.item_status = Some("SKIPPED");
    let fetch = setup.build(events).await;

    let path = fetch.acquire().await.expect("a skipped song is taken");

    assert!(path.ends_with(".flac"), "{path}");
    assert!(
        !path.contains(".octo-incoming"),
        "placed out of the job folder: {path}"
    );
    assert!(std::path::Path::new(&path).is_file());
    assert!(fetch.left_in_job_folders().is_empty());
}

#[tokio::test]
async fn an_isrc_answer_without_isrcs_is_picked_on_the_isrc_query() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.tracks = Arc::new(|_| vec![take_care("Take Care", 277, "")]);
    let fetch = setup.build(events).await;

    fetch.acquire().await.expect("the ISRC search answer is taken");

    assert_eq!(
        fetch.spotiflac_events(),
        [
            format!("spotiflac search isrc:{ISRC}"),
            "spotiflac download https://open.spotify.com/track/6LxSe8YmdPxy095Ux6znaQ".to_string(),
        ]
    );
}

#[tokio::test]
async fn a_file_of_the_wrong_length_is_refused_and_thrown_away() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.file = Some(("Drake - Take Care.flac".into(), flac(331)));
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("refused");

    assert!(
        error.to_string().contains("331s file for the 277s song"),
        "{error}"
    );
    assert!(
        fetch.left_in_job_folders().is_empty(),
        "{:?}",
        fetch.left_in_job_folders()
    );
}

#[tokio::test]
async fn a_file_that_calls_itself_another_version_is_refused() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    let scratch = tempfile::tempdir().expect("a folder");
    let tagged = scratch.path().join("remix.flac");
    std::fs::write(&tagged, flac(277)).expect("written");
    let mut file = TagFile::open(&tagged).expect("a FLAC");
    file.set_title(Some("Take Care (Jamie xx Remix)"));
    file.save().expect("saved");
    setup.fake.file = Some((
        "Drake - Take Care.flac".into(),
        std::fs::read(&tagged).expect("read"),
    ));
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("refused");

    assert!(error.to_string().contains("another version"), "{error}");
    assert!(fetch.left_in_job_folders().is_empty());
}

#[tokio::test]
async fn another_version_in_the_catalog_is_never_asked_for() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.tracks = Arc::new(|_| {
        vec![
            take_care("Take Care (Instrumental)", 277, ISRC),
            take_care("Take Care - Sped Up", 230, ""),
        ]
    });
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("nothing fits");

    assert!(error.to_string().contains("Mornye has no copy"), "{error}");
    assert!(
        !fetch
            .spotiflac_events()
            .iter()
            .any(|event| event.contains("download")),
        "{:?}",
        fetch.events()
    );
}

#[tokio::test]
async fn a_failed_job_says_why_in_spotiflacs_words() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error = Some("no extension had this track");
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("failed");

    assert!(
        error.to_string().contains("no extension had this track"),
        "{error}"
    );
}

#[tokio::test]
async fn a_job_that_never_ends_times_out_and_is_cancelled() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.file = None;
    setup.fake.status = "running";
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("timed out");

    assert!(error.to_string().contains("so Octo stopped waiting"), "{error}");
    assert_eq!(
        fetch.spotiflac_events().last().map(String::as_str),
        Some("spotiflac cancel")
    );
}

#[tokio::test]
async fn a_done_job_with_no_file_says_the_folders_differ() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.file = None;
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("no file");

    assert!(error.to_string().contains("nothing arrived"), "{error}");
}

#[tokio::test]
async fn a_spotiflac_that_does_not_answer_fails_the_song_plainly() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.spotiflac.url = "http://127.0.0.1:1".into();
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("unreachable");

    assert!(error.to_string().contains("could not reach Mornye"), "{error}");
}

#[tokio::test]
async fn after_soulseek_spotiflac_is_asked_once_soulseek_finds_nothing() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.soulseek_on = true;
    let fetch = setup.build(events).await;

    fetch.acquire().await.expect("Mornye's copy is taken");

    let events = fetch.events();
    let first_slskd = events
        .iter()
        .position(|e| e.starts_with("slskd"))
        .expect("Soulseek asked");
    let first_spotiflac = events
        .iter()
        .position(|e| e.starts_with("spotiflac"))
        .expect("Mornye asked");
    assert!(first_slskd < first_spotiflac, "{events:?}");
}

#[tokio::test]
async fn before_soulseek_spotiflac_is_asked_first_and_soulseek_not_at_all_when_it_brings_the_song() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.soulseek_on = true;
    setup.spotiflac.placement = SpotiflacPlacement::BeforeSoulseek;
    let fetch = setup.build(events).await;

    fetch.acquire().await.expect("Mornye's copy is taken");

    assert!(
        !fetch.events().iter().any(|e| e.starts_with("slskd")),
        "{:?}",
        fetch.events()
    );
}

#[tokio::test]
async fn songs_in_asian_scripts_go_to_spotiflac_first_and_others_to_soulseek() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.soulseek_on = true;
    setup.spotiflac.placement = SpotiflacPlacement::BeforeSoulseekForAsianScripts;
    setup.artist = "아이유";
    setup.title = "밤편지";
    setup.seconds = 253;
    setup.isrc = Some("KRA381700418");
    setup.fake.tracks = Arc::new(|_| {
        vec![
            json!({"id": "3P3UA61WRQqwCXaoFOTENd", "title": "밤편지", "artists": "아이유", "album": "Palette",
            "isrc": "KRA381700418", "duration_ms": 253_000, "external_url": ""}),
        ]
    });
    setup.fake.file = Some(("밤편지.flac".into(), flac(253)));
    let fetch = setup.build(events).await;

    fetch.acquire().await.expect("Mornye's copy is taken");

    assert!(
        !fetch.events().iter().any(|e| e.starts_with("slskd")),
        "{:?}",
        fetch.events()
    );

    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.soulseek_on = true;
    setup.spotiflac.placement = SpotiflacPlacement::BeforeSoulseekForAsianScripts;
    let fetch = setup.build(events).await;

    fetch
        .acquire()
        .await
        .expect("Mornye's copy is taken after Soulseek");

    let events = fetch.events();
    assert!(events[0].starts_with("slskd"), "{events:?}");
}

/// Mornye's one gateway session, for extension example-ext, in `state`.
fn gateway(state: &str) -> Value {
    let paused_s = if state == "active" { 0.0 } else { 900.0 };
    json!({"result": {"sessions": [
        {"key": "gateway-v2-abc-6836abb49bb0e8c9", "kind": "gateway", "label": "Example",
         "extensions": ["example-ext"], "state": state, "auth_paused_s": paused_s}
    ]}})
}

fn example_service() -> Value {
    json!({"result": {"services": [{"id": "ext:example-ext", "label": "Example", "extensions": ["example-ext"]}]}})
}

/// Mornye's health row for example-ext.
fn health(attempts: i64, failures: i64, last_error: &str) -> Value {
    json!({"providers": [
        {"provider": "example-ext", "installed": false, "attempts": attempts, "failures": failures,
         "last_outcome": if failures > 0 { "failure" } else { "" }, "last_error": last_error},
        {"provider": "ext:example-ext", "installed": true, "attempts": 0, "failures": 0, "last_outcome": "", "last_error": ""}
    ]})
}

impl Fetch {
    /// The kinds of notification sent, once the background sends have run.
    async fn sent(&self) -> Vec<NotificationEventType> {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.sent.0.lock().clone()
    }

    fn check_songs(&self) -> usize {
        self.spotiflac.check.store.snapshot().check_songs.len()
    }
}

#[tokio::test]
async fn a_paused_check_keeps_the_song_from_mornye_and_keeps_it_for_pass() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.sessions = vec![gateway("unverified")];
    setup.fake.services = Some(example_service());
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("Mornye's check is paused");

    let wait = mornye_waited(&error).expect("a Mornye wait");
    assert!(!wait.asked);
    assert!(
        format!("{error:#}")
            .contains("Mornye could not pass its check for Example, so Octo did not ask it for this song."),
        "{error:#}"
    );
    assert!(
        !fetch.spotiflac_events().iter().any(|e| e.contains("download")),
        "{:?}",
        fetch.events()
    );
    assert!(
        !fetch
            .sent()
            .await
            .contains(&NotificationEventType::DownloadStarted)
    );
    assert_eq!(fetch.check_songs(), 1);
}

#[tokio::test]
async fn a_wait_on_the_check_of_the_jobs_own_extension_names_its_mornye() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.sessions = vec![gateway("active"), gateway("unverified")];
    setup.fake.services = Some(example_service());
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error = Some("download_failed");
    setup.fake.provider = Some("ext:example-ext");
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("the check stopped it");

    let wait = mornye_waited(&error).expect("a Mornye wait");
    assert_eq!(wait.pause.labels, vec!["Example".to_string()]);
    assert_eq!(wait.pause.url, fetch.spotiflac.settings().effective_url());
}

#[tokio::test]
async fn a_job_that_runs_into_the_check_is_a_wait_and_its_song_is_kept() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    // Clear when the song is asked for, paused when the failure is read.
    setup.fake.sessions = vec![gateway("active"), gateway("unverified")];
    setup.fake.services = Some(example_service());
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error = Some("download_failed");
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("the check stopped it");

    let wait = mornye_waited(&error).expect("a Mornye wait");
    assert!(wait.asked);
    assert_eq!(wait.pause.url, fetch.spotiflac.settings().effective_url());
    assert_eq!(
        wait.shown,
        "Mornye could not pass its check for Example, so it could not get this song."
    );
    assert!(
        fetch
            .sent()
            .await
            .contains(&NotificationEventType::DownloadStarted)
    );
    assert_eq!(fetch.check_songs(), 1);
}

#[tokio::test]
async fn mornyes_own_paused_words_are_a_wait_when_it_does_not_report_its_check() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error =
        Some("RuntimeError: verification paused after a failed attempt \u{2014} next try in 4 min");
    setup.fake.provider = Some("example-ext");
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("paused");

    let wait = mornye_waited(&error).expect("a Mornye wait");
    assert_eq!(wait.pause.labels, vec!["example-ext".to_string()]);
    assert_eq!(wait.pause.url, fetch.spotiflac.settings().effective_url());
}

#[tokio::test]
async fn a_wait_from_mornyes_words_is_not_let_go_by_the_working_read_it_was_made_beside() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    // The read shows the extension's gateway working; only Mornye's words say it is paused.
    setup.fake.sessions = vec![gateway("active")];
    setup.fake.services = Some(example_service());
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error =
        Some("RuntimeError: verification paused after a failed attempt \u{2014} next try in 4 min");
    setup.fake.provider = Some("ext:example-ext");
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("paused");

    let wait = mornye_waited(&error).expect("a Mornye wait");
    assert!(wait.pause.labels.is_empty(), "{:?}", wait.pause.labels);
    assert_eq!(wait.pause.url, fetch.spotiflac.settings().effective_url());
    // It still shares the extension's gateway with other waits on it, and names it.
    assert_eq!(wait.pause.holding, vec!["Example".to_string()]);
    assert_eq!(
        wait.shown,
        "Mornye could not pass its check for Example, so it could not get this song."
    );
    // The same working read, asked again as a waiting job asks it each tick: not a pass.
    assert!(!fetch.spotiflac.checks_pass(&wait.pause.labels).await);
}

#[tokio::test]
async fn a_wait_from_mornyes_words_names_the_extensions_gateway_that_is_not_working() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    // Working when the song is asked for; when the failure is read the gateway needs its check
    // but shows no pause of its own, so only Mornye's words carry the wait.
    let unverified = json!({"result": {"sessions": [
        {"key": "gateway-v2-abc-6836abb49bb0e8c9", "kind": "gateway", "label": "Example",
         "extensions": ["example-ext"], "state": "unverified", "auth_paused_s": 0.0}
    ]}});
    setup.fake.sessions = vec![gateway("active"), unverified];
    setup.fake.services = Some(example_service());
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error =
        Some("RuntimeError: verification paused after a failed attempt \u{2014} next try in 4 min");
    setup.fake.provider = Some("ext:example-ext");
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("paused");

    let wait = mornye_waited(&error).expect("a Mornye wait");
    assert_eq!(wait.pause.labels, vec!["Example".to_string()]);
}

#[tokio::test]
async fn a_failure_of_this_song_alone_is_said_from_the_extensions_health() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.sessions = vec![gateway("active")];
    setup.fake.services = Some(example_service());
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error = Some("download_failed");
    setup.fake.provider = Some("example-ext");
    setup.fake.extensions = vec![
        health(3, 1, "old"),
        health(4, 2, "the catalog has no FLAC of this"),
    ];
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("failed");

    assert!(mornye_waited(&error).is_none());
    assert!(
        format!("{error:#}").contains(
            "Mornye's example-ext extension could not get the song: the catalog has no FLAC of this"
        ),
        "{error:#}"
    );
}

#[tokio::test]
async fn a_last_error_another_ask_may_have_left_is_said_as_the_extensions_last_try() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.sessions = vec![gateway("active")];
    setup.fake.services = Some(example_service());
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error = Some("download_failed");
    setup.fake.provider = Some("example-ext");
    // Two failures between the reads: not this song's alone.
    setup.fake.extensions = vec![health(3, 1, "old"), health(5, 3, "another song's trouble")];
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("failed");

    assert!(
        format!("{error:#}")
            .contains("Mornye's example-ext extension failed; its last try said: another song's trouble"),
        "{error:#}"
    );
}

#[tokio::test]
async fn soulseek_is_still_asked_when_mornyes_check_keeps_the_song_and_the_wait_is_kept_in_either_order() {
    for placement in [
        SpotiflacPlacement::AfterSoulseek,
        SpotiflacPlacement::BeforeSoulseek,
    ] {
        let events: Events = Arc::default();
        let mut setup = Setup::new(&events);
        setup.soulseek_on = true;
        setup.spotiflac.placement = placement;
        setup.fake.sessions = vec![gateway("unverified")];
        setup.fake.services = Some(example_service());
        let fetch = setup.build(events).await;

        let error = fetch.acquire().await.expect_err("no source had it");

        assert!(
            fetch.events().iter().any(|e| e.starts_with("slskd search")),
            "{placement:?}: {:?}",
            fetch.events()
        );
        assert!(
            mornye_waited(&error).is_some(),
            "{placement:?}: the wait is kept behind Soulseek's answer: {error:#}"
        );
        assert!(!fetch.spotiflac_events().iter().any(|e| e.contains("download")));
    }
}

#[tokio::test]
async fn a_check_read_that_hangs_never_holds_a_song_up() {
    // Mornye answers its check reads after a minute; the read is bound to a fifth of a second.
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.sessions = vec![gateway("unverified")];
    setup.fake.services = Some(example_service());
    setup.fake.sessions_delay = Some(Duration::from_secs(60));
    let fetch = setup.build(events).await;

    // Well inside the minute an unbounded read would wait.
    tokio::time::timeout(Duration::from_secs(30), fetch.acquire())
        .await
        .expect("the song did not wait for the read")
        .expect("Mornye is asked, since Octo cannot tell");
}

#[tokio::test]
async fn a_search_mornye_did_not_answer_goes_on_without_reading_its_check() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.spotiflac.url = "http://127.0.0.1:1".into();
    let fetch = setup.build(events).await;

    fetch.acquire().await.expect_err("Mornye is not there");

    // No read was kept while the song was asked for: this one is new.
    let after = std::time::Instant::now();
    let view = fetch.spotiflac.check_view(None).await.expect("Mornye is on");
    assert!(
        view.read_started >= after,
        "Mornye's check was read after a search it did not answer"
    );
}

#[tokio::test]
async fn the_second_look_and_the_health_read_before_a_job_go_side_by_side() {
    // Every look reads Mornye again; the second look takes as long as its bound allows, and the
    // health read before the job must not wait for it.
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.check_fresh_for = Duration::ZERO;
    setup.fake.sessions_delay = Some(Duration::from_secs(60));
    setup.fake.sessions_delay_from = 1;
    setup.check_call_bound = Duration::from_secs(30);
    let health_reads = setup.fake.extensions_read.clone();
    let fetch = setup.build(events).await;

    let read = async {
        while health_reads.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let side_by_side = tokio::select! {
        _ = fetch.acquire() => false,
        read = tokio::time::timeout(Duration::from_secs(5), read) => read.is_ok(),
    };

    assert!(side_by_side, "the health read waited for the second look");
}

#[tokio::test]
async fn the_words_for_a_failed_job_read_mornyes_check_and_its_health_side_by_side() {
    // The check read after the failure takes as long as its bound allows; the health read must
    // not wait for it.
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.status = "failed";
    setup.fake.error = Some("Provider error");
    setup.fake.sessions_delay = Some(Duration::from_secs(60));
    setup.fake.sessions_delay_from = 1;
    setup.check_call_bound = Duration::from_secs(30);
    let health_reads = setup.fake.extensions_read.clone();
    let fetch = setup.build(events).await;

    let both = async {
        while health_reads.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let side_by_side = tokio::select! {
        _ = fetch.acquire() => false,
        read = tokio::time::timeout(Duration::from_secs(5), both) => read.is_ok(),
    };

    assert!(side_by_side, "the health read waited for the check read");
}

#[tokio::test]
async fn the_check_is_looked_at_again_once_the_song_has_its_turn() {
    // Every look reads Mornye again: clear before the turn, paused once the song has it, as when
    // a song ahead of it ran into the check meanwhile.
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.check_fresh_for = Duration::ZERO;
    setup.fake.sessions = vec![gateway("active"), gateway("unverified")];
    setup.fake.services = Some(example_service());
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("the second look refuses");

    assert!(mornye_waited(&error).is_some_and(|wait| !wait.asked), "{error:#}");
    assert!(
        !fetch.spotiflac_events().iter().any(|e| e.contains("download")),
        "{:?}",
        fetch.events()
    );
    assert!(
        !fetch
            .sent()
            .await
            .contains(&NotificationEventType::DownloadStarted)
    );
}

#[tokio::test]
async fn the_download_log_says_why_mornye_was_not_asked() {
    // Plays turn every failure into "not proven", so the log is where the reason shows.
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.sessions = vec![gateway("unverified")];
    setup.fake.services = Some(example_service());
    let fetch = setup.build(events).await;

    fetch.acquire().await.expect_err("Mornye's check is paused");

    let lines = fetch.log_lines();
    let wait = lines
        .iter()
        .find(|(text, _)| {
            text == "Mornye could not pass its check for Example, so Octo did not ask it for this song."
        })
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert!(
        wait.1
            .as_deref()
            .is_some_and(|detail| detail.starts_with("It allows its next try at ")),
        "{wait:?}"
    );
}

#[tokio::test]
async fn a_failed_jobs_own_error_reaches_the_download_log_without_server_paths() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.sessions = vec![gateway("active")];
    setup.fake.services = Some(example_service());
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error = Some("download_failed");
    setup.fake.provider = Some("example-ext");
    setup.fake.extensions = vec![
        health(3, 1, "old"),
        health(
            4,
            2,
            "the catalog has no FLAC of this. It looked in /srv/mornye/cache/x",
        ),
    ];
    let fetch = setup.build(events).await;

    fetch.acquire().await.expect_err("failed");

    let lines = fetch.log_lines();
    let note = lines
        .iter()
        .find(|(text, _)| text == "Mornye brought no file")
        .unwrap_or_else(|| panic!("{lines:?}"));
    let detail = note.1.clone().unwrap_or_default();
    assert!(
        detail.starts_with(
            "Mornye's example-ext extension could not get the song: the catalog has no FLAC of this"
        ),
        "{detail}"
    );
    assert!(!detail.contains("/srv/"), "{detail}");
}

#[tokio::test]
async fn a_refused_write_into_octos_folder_names_the_test_and_logs_the_systems_words() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.sessions = vec![gateway("active")];
    setup.fake.services = Some(example_service());
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error = Some("download_failed");
    setup.fake.provider = Some("example-ext");
    setup.fake.write_refused = true;
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("failed");

    assert!(
        format!("{error:#}").contains(
            "Mornye could not write the song into Octo's music folder; the Test on the Mornye page checks that folder."
        ),
        "{error:#}"
    );
    let lines = fetch.log_lines();
    let note = lines
        .iter()
        .find(|(text, _)| text == "Mornye brought no file")
        .unwrap_or_else(|| panic!("{lines:?}"));
    let detail = note.1.clone().unwrap_or_default();
    assert!(
        detail.contains("EACCES: permission denied"),
        "the system's own words: {detail}"
    );
}

#[tokio::test]
async fn the_mp3_fallback_says_soulseek_brought_none_and_mornyes_check_kept_it() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.soulseek_on = true;
    setup.fake.sessions = vec![gateway("unverified")];
    setup.fake.services = Some(example_service());
    let fetch = setup.build(events).await;

    // YouTube is not reachable in the test: only the fallback's words matter here.
    let _ = fetch.acquire_from(DownloadSource::SoulseekThenYouTube).await;

    let bodies = {
        let _ = fetch.sent().await;
        fetch.sent.1.lock().clone()
    };
    assert!(
        bodies
            .iter()
            .any(|body| body
                .contains("Soulseek brought no FLAC and Mornye could not pass its check for Example")),
        "{bodies:?}"
    );
}

#[tokio::test]
async fn the_mp3_fallback_names_the_check_of_a_wait_read_from_mornyes_words() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.soulseek_on = true;
    // The read shows the gateway working; only Mornye's words say its check is paused.
    setup.fake.sessions = vec![gateway("active")];
    setup.fake.services = Some(example_service());
    setup.fake.file = None;
    setup.fake.status = "failed";
    setup.fake.error =
        Some("RuntimeError: verification paused after a failed attempt \u{2014} next try in 4 min");
    setup.fake.provider = Some("ext:example-ext");
    let fetch = setup.build(events).await;

    let _ = fetch.acquire_from(DownloadSource::SoulseekThenYouTube).await;

    let bodies = {
        let _ = fetch.sent().await;
        fetch.sent.1.lock().clone()
    };
    assert!(
        bodies
            .iter()
            .any(|body| body
                .contains("Soulseek brought no FLAC and Mornye could not pass its check for Example")),
        "{bodies:?}"
    );
}

#[tokio::test]
async fn mornye_1_1_without_its_utilities_says_so_when_its_search_fails() {
    let events: Events = Arc::default();
    let mut setup = Setup::new(&events);
    setup.fake.sessions = vec![
        json!({"result": {"sessions": [], "error": "The optional utilities extension is not installed; 'mornye.core.signed_session_status' requires 'utilities.sflx' to be available."}}),
    ];
    setup.fake.services = Some(json!({"result": {"services": []}}));
    setup.fake.search_fails = true;
    let fetch = setup.build(events).await;

    let error = fetch.acquire().await.expect_err("Mornye cannot search");

    assert!(
        format!("{error:#}")
            .contains("Mornye needs its utilities extension, which your registry does not list."),
        "{error:#}"
    );
    assert!(
        fetch.log_lines().iter().any(|(_, detail)| detail
            .as_deref()
            .is_some_and(|d| d.contains("SPOTIFLAC_IMAGE back to Mornye 1.0.5"))),
        "{:?}",
        fetch.log_lines()
    );
}
