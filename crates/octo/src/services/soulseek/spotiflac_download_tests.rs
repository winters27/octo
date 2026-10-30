//! Mornye as a download source, end to end through the download service, against a stand-in
//! Mornye that writes a FLAC into the folder it is asked to and a stand-in slskd that finds
//! nothing: off by default, a found file checked and placed, a wrong file refused, a job that
//! fails or never ends, and where Mornye sits against Soulseek.

use std::sync::Arc;
use std::time::Duration;

use octo_core::models::domain::Song;
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
use crate::services::common::{DownloadConcurrency, DownloadCore, DownloadServices};
use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
use crate::services::i_download_service::IDownloadService;
use crate::services::local::DownloadHistoryService;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::notifications::NotificationService;
use crate::services::soulseek::soulseek_client::Timings;
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
}

impl Respond for FakeSpotiflac {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let method = request.method.as_str();
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
                    "payload": {"items_detail": [{"status": self.status.to_uppercase(), "error": self.error}]}}),
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

/// The song, the service and the stand-ins.
struct Fetch {
    service: Arc<BaseDownloadService>,
    song: Song,
    events: Events,
    root: tempfile::TempDir,
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
            },
            artist: "Drake",
            title: "Take Care",
            seconds: 277,
            isrc: Some(ISRC),
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
            ..Default::default()
        }));
        settings.set_raw("Library:DownloadPath", Some(&root_path));
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
            notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
        };
        let services = DownloadServices {
            concurrency: Some(Arc::new(DownloadConcurrency::new(Arc::clone(&settings)))),
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
        let spotiflac = Arc::new(
            SpotiflacClient::with_poll_interval(Arc::clone(&settings), Duration::from_millis(10))
                .with_song_timeout(Duration::from_millis(400)),
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
            spotiflac: Some(spotiflac),
            preview_copies: None,
        };
        let (service, _) = SoulseekDownloadService::build(core, services, parts);
        Fetch {
            service,
            song,
            events,
            root,
            _spotiflac: spotiflac_server,
            _slskd: slskd_server,
        }
    }
}

impl Fetch {
    async fn acquire(&self) -> anyhow::Result<String> {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.service.execute_acquisition(
                "soulseek",
                self.song.external_id.as_deref().expect("an id"),
                false,
                true,
                Some(DownloadSource::Soulseek),
                &CancellationToken::new(),
                None,
                false,
                None,
            ),
        )
        .await
        .expect("within a minute")
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
