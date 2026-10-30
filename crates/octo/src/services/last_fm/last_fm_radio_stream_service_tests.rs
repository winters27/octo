//! The stream service end to end over fakes. These pin the orchestration: snapshot order, the
//! ready pool, failures rejected and skipped, plays recorded, profiles written, and the startup
//! warm.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use bytes::Bytes;
use octo_core::models::domain::Song;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::{AppSettings, DownloadSource, LastFmSettings, SubsonicSettings};
use tempfile::TempDir;
use tokio::io::AsyncWrite;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::i_download_service::AudioStream;
use crate::services::library::ReplacementHandoff;
use crate::services::local::LocalLibraryService;
use crate::services::subsonic::NavidromeIdentityService;

/// Direct streams: the id's own bytes (and `padding` zeros), except for the ids that are broken.
#[derive(Default)]
struct Downloads {
    broken: Mutex<HashSet<String>>,
    asked: Mutex<Vec<(String, String)>>,
    padding: Mutex<usize>,
}

#[async_trait]
impl IDownloadService for Downloads {
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
        provider: &str,
        id: &str,
        _: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        self.asked.lock().push((provider.to_string(), id.to_string()));
        if self.broken.lock().contains(id) {
            anyhow::bail!("no peer has {id}");
        }
        let mut body = format!("<{id}>").into_bytes();
        body.resize(body.len() + *self.padding.lock(), 0);
        Ok(Some(DirectStreamInfo {
            audio_stream: Box::pin(futures::stream::iter([Ok(Bytes::from(body))])),
            content_type: "audio/flac".into(),
            content_length: None,
            quality: None,
            status_code: 200,
            content_range: None,
            video_id: None,
            itag: None,
        }))
    }
}

/// "MP3:" and the input, with a profile when there is a target.
struct Transcoder;

#[async_trait]
impl ILastFmRadioAudioTranscoder for Transcoder {
    async fn transcode_to_mp3(
        &self,
        mut input: AudioStream,
        output: &mut (dyn AsyncWrite + Unpin + Send),
        _: i32,
        target_lufs: Option<f64>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<RadioAudioProfile>> {
        use futures::StreamExt;
        output.write_all(b"MP3:").await?;
        while let Some(chunk) = input.next().await {
            output.write_all(&chunk?).await?;
        }
        Ok(target_lufs.map(|_| RadioAudioProfile::new(-12.0, 6.0, -1.0, -4.0, 2000.0, 0.1, 6000.0)))
    }
}

/// Always starts at the same place.
struct FixedStart(usize);

impl IRadioTuneInSelector for FixedStart {
    fn start(&self, _: usize) -> usize {
        self.0
    }
}

/// A client: collects what it is sent and hangs up after `limit` bytes.
struct Client {
    received: Arc<Mutex<Vec<u8>>>,
    limit: usize,
    hang_up: CancellationToken,
}

impl AsyncWrite for Client {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buffer: &[u8]) -> Poll<std::io::Result<usize>> {
        let mut received = self.received.lock();
        received.extend_from_slice(buffer);
        if received.len() >= self.limit {
            self.hang_up.cancel();
        }
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct Fixture {
    _dir: TempDir,
    _navidrome: MockServer,
    state: Arc<LastFmRadioStateStore>,
    sessions: Arc<LastFmRadioStreamSessionStore>,
    downloads: Arc<Downloads>,
    cache: Arc<LastFmRadioTrackCache>,
    refresh_queue: Arc<LastFmRadioRefreshQueue>,
    service: LastFmRadioStreamService,
}

/// Eight tracks: the flow picker's window of four never wraps round to the track playing.
const TITLES: [&str; 8] = ["One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight"];

async fn fixture(loudness_target: i32) -> Fixture {
    let dir = TempDir::new().unwrap();
    let navidrome = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[]}}}"#),
        )
        .mount(&navidrome)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(navidrome.uri()),
            ..Default::default()
        },
        last_fm: LastFmSettings {
            enable_radio: true,
            expose_radio_as_streams: true,
            radio_loudness_target_lufs: loudness_target,
            ..Default::default()
        },
        ..Default::default()
    }));
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let state = Arc::new(LastFmRadioStateStore::new(
        dir.path().join("state.json"),
        settings.clone(),
        registry.clone(),
    ));
    let metadata = Arc::new(FakeMetadata::default());
    for (index, title) in TITLES.iter().enumerate() {
        metadata.hits.lock().insert(
            (format!("Artist {index}"), title.to_string()),
            Song {
                id: format!("ext-soulseek-song-{title}"),
                artist: format!("Artist {index}"),
                title: title.to_string(),
                ..Default::default()
            },
        );
    }
    let http = reqwest::Client::new();
    let library = Arc::new(LocalLibraryService::new(
        settings.clone(),
        http.clone(),
        registry.clone(),
        NavidromeIdentityService::new(settings.clone(), http),
    ));
    let downloads = Arc::new(Downloads::default());
    let cache = Arc::new(LastFmRadioTrackCache::with_root(dir.path().join("radio")));
    let sessions = Arc::new(LastFmRadioStreamSessionStore::new());
    let refresh_queue = Arc::new(LastFmRadioRefreshQueue::new());
    let service = LastFmRadioStreamService::new(LastFmRadioStreamServiceParts {
        state: state.clone(),
        settings: settings.clone(),
        library,
        proxy: SubsonicProxyService::new(settings.clone()),
        downloads: downloads.clone(),
        transcoder: Arc::new(Transcoder),
        cache: cache.clone(),
        sessions: sessions.clone(),
        registry,
        metadata,
        queues: Arc::new(RadioQueueStore::new()),
        refresh_queue: refresh_queue.clone(),
        tune_in: Arc::new(FixedStart(0)),
        last_fm: None,
        listen_brainz: None,
        last_fm_scrobbles: None,
        recommendations: None,
    });
    state.replace_stations(
        "alice",
        &[LastFmRadioStation {
            id: "or-station".into(),
            name: "Your Radio".into(),
            owner: "alice".into(),
            personalized: true,
            tracks: TITLES
                .iter()
                .enumerate()
                .map(|(index, title)| LastFmRadioTrack {
                    artist: format!("Artist {index}"),
                    title: title.to_string(),
                    genre: Some("Rock".into()),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }],
    );
    Fixture {
        _dir: dir,
        _navidrome: navidrome,
        state,
        sessions,
        downloads,
        cache,
        refresh_queue,
        service,
    }
}

fn session(fixture: &Fixture) -> LastFmRadioStreamSession {
    let token = fixture
        .sessions
        .issue("alice", "or-station", [("u", "alice")], None);
    fixture.sessions.get(&token, None).unwrap()
}

#[tokio::test]
async fn a_station_streams_in_snapshot_order_and_records_what_played() {
    let fixture = fixture(-16).await;
    let session = session(&fixture);
    let hang_up = CancellationToken::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut client = Client {
        received: received.clone(),
        limit: "MP3:<One>MP3:<Two>MP3:<Three>".len(),
        hang_up: hang_up.clone(),
    };
    fixture
        .service
        .stream(&session, &mut client, &hang_up, false)
        .await
        .expect("the stream ends when the client hangs up");

    let text = String::from_utf8(received.lock().clone()).unwrap();
    assert_eq!(text, "MP3:<One>MP3:<Two>MP3:<Three>");
    let plays = fixture.state.get_user("alice").plays;
    assert!(plays.len() >= 2, "{plays:?}");
    assert!(
        plays
            .iter()
            .all(|play| play.source == "internet-radio" && !play.is_local)
    );
    // The producer measured each track, and the sidecar carries the genre and (no Last.fm) no tags.
    let key = fixture.cache.key(
        "alice",
        "",
        fixture.state.get_user("alice").stations[0].tracks[0]
            .resolved_id
            .as_deref()
            .unwrap(),
        192,
    );
    let path = fixture.cache.get_ready_path(&key).expect("cached");
    let profile = fixture.cache.get_profile(&path).expect("a profile");
    assert_eq!(profile.genre.as_deref(), Some("Rock"));
    assert_eq!(profile.tags, Some(Vec::new()));
}

/// A failed track is rejected from the station, so the candidates the next replenishment counts
/// from are one shorter: the next pick starts at the old index in the new list, so "Four" (now
/// at the index "Five" had) is passed over.
#[tokio::test]
async fn an_unplayable_track_is_rejected_and_skipped() {
    let fixture = fixture(0).await;
    fixture.downloads.broken.lock().insert("Two".into());
    let session = session(&fixture);
    let hang_up = CancellationToken::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut client = Client {
        received: received.clone(),
        limit: "MP3:<One>MP3:<Three>MP3:<Five>".len(),
        hang_up: hang_up.clone(),
    };
    fixture
        .service
        .stream(&session, &mut client, &hang_up, false)
        .await
        .unwrap();

    assert_eq!(
        String::from_utf8(received.lock().clone()).unwrap(),
        "MP3:<One>MP3:<Three>MP3:<Five>"
    );
    let user = fixture.state.get_user("alice");
    assert_eq!(user.unavailable_tracks.len(), 1);
    assert_eq!(user.unavailable_tracks[0].title, "Two");
    assert!(!user.stations[0].tracks.iter().any(|track| track.title == "Two"));
    // The listener's stations are refreshed to fill the gap.
    let job = fixture
        .refresh_queue
        .dequeue(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(job.username, "alice");
    // No loudness target, no profile.
    let key = fixture.cache.key(
        "alice",
        "",
        user.stations[0].tracks[0].resolved_id.as_deref().unwrap(),
        192,
    );
    let path = fixture.cache.get_ready_path(&key).unwrap();
    assert!(fixture.cache.get_profile(&path).is_none());
}

#[tokio::test]
async fn icy_metadata_names_the_track_that_is_playing() {
    let fixture = fixture(0).await;
    *fixture.downloads.padding.lock() = 7000;
    let session = session(&fixture);
    let hang_up = CancellationToken::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut client = Client {
        received: received.clone(),
        limit: DEFAULT_INTERVAL + 1,
        hang_up: hang_up.clone(),
    };
    // Nothing is framed until 16 KiB of audio have gone out; the third track crosses it.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        fixture.service.stream(&session, &mut client, &hang_up, true),
    )
    .await;
    assert!(result.is_ok(), "the stream ended");
    let bytes = received.lock().clone();
    let block = &bytes[DEFAULT_INTERVAL..];
    let length = block[0] as usize * 16;
    let title = String::from_utf8_lossy(&block[1..1 + length]);
    assert!(title.starts_with("StreamTitle='Artist 2 - Three';"), "{title}");
}

#[tokio::test]
async fn nothing_playable_ends_the_stream_with_the_reason() {
    let fixture = fixture(0).await;
    for title in TITLES {
        fixture.downloads.broken.lock().insert(title.into());
    }
    let session = session(&fixture);
    let hang_up = CancellationToken::new();
    let mut client = Client {
        received: Arc::new(Mutex::new(Vec::new())),
        limit: usize::MAX,
        hang_up: hang_up.clone(),
    };
    let error = fixture
        .service
        .stream(&session, &mut client, &hang_up, false)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Radio station has no cached ready track");
    // The next attempt finds the station emptied out.
    let error = fixture
        .service
        .stream(&session, &mut client, &hang_up, false)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Radio station is no longer available");
}

#[tokio::test]
async fn the_ready_pool_rotates_from_the_tune_in_start_and_the_warm_uses_external_routes() {
    let fixture = fixture(0).await;
    let result = fixture
        .service
        .warm_stored_stations("alice", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        result,
        RadioWarmupResult {
            station_count: 1,
            ready_station_count: 1,
            ready_track_count: 3,
        }
    );
    // The warm asked for the registered routes, never the listener's library.
    let asked = fixture.downloads.asked.lock().clone();
    assert_eq!(asked.len(), 3);
    assert!(asked.iter().all(|(provider, _)| provider == "soulseek"));

    let session = session(&fixture);
    let pool: Vec<String> = fixture
        .service
        .get_ready_pool(&session)
        .into_iter()
        .map(|item| item.track.title)
        .collect();
    assert_eq!(pool, ["One", "Two", "Three"]);
    let rotated = LastFmRadioStreamService {
        shared: Arc::new(Shared_ {
            tune_in: Arc::new(FixedStart(2)),
            ..clone_shared(&fixture.service.shared)
        }),
        ..fixture.service.clone()
    };
    let pool: Vec<String> = rotated
        .get_ready_pool(&session)
        .into_iter()
        .map(|item| item.track.title)
        .collect();
    assert_eq!(pool, ["Three", "One", "Two"]);
    let published = fixture
        .service
        .prepare_for_publication(&session, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(published.len(), 3);
}

fn clone_shared(shared: &Shared_) -> Shared_ {
    Shared_ {
        state: shared.state.clone(),
        settings: shared.settings.clone(),
        library: shared.library.clone(),
        downloads: shared.downloads.clone(),
        transcoder: shared.transcoder.clone(),
        cache: shared.cache.clone(),
        sessions: shared.sessions.clone(),
        registry: shared.registry.clone(),
        metadata: shared.metadata.clone(),
        queues: shared.queues.clone(),
        refresh_queue: shared.refresh_queue.clone(),
        tune_in: shared.tune_in.clone(),
        last_fm: shared.last_fm.clone(),
        listen_brainz: shared.listen_brainz.clone(),
        last_fm_scrobbles: shared.last_fm_scrobbles.clone(),
        recommendations: shared.recommendations.clone(),
        family: shared.family.clone(),
        listening: shared.listening.clone(),
    }
}

/// A station whose kind is switched off, or radio streams switched off, is not served.
#[tokio::test]
async fn switched_off_stations_are_not_served() {
    let fixture = fixture(0).await;
    let session = session(&fixture);
    assert!(fixture.service.resolve(&session).is_some());
    fixture.service.shared.settings.set(AppSettings {
        last_fm: LastFmSettings {
            enable_radio: true,
            expose_radio_as_streams: true,
            enable_personalized_stations: false,
            ..Default::default()
        },
        ..Default::default()
    });
    assert!(fixture.service.resolve(&session).is_none());
    assert!(fixture.service.get_ready_pool(&session).is_empty());
}

/// The warmers of one service instance collapse; another instance has its own.
#[tokio::test]
async fn a_scope_warms_a_station_once() {
    let fixture = fixture(0).await;
    let session = session(&fixture);
    fixture.service.warm_ready_pool(&session);
    assert_eq!(fixture.service.pool_warmers.lock().len(), 1);
    fixture.service.warm_ready_pool(&session);
    assert_eq!(fixture.service.pool_warmers.lock().len(), 1);
    let other = fixture
        .service
        .scoped(SubsonicProxyService::new(fixture.service.shared.settings.clone()));
    assert!(other.pool_warmers.lock().is_empty());
    crate::services::common::test_fakes::until(|| fixture.service.pool_warmers.lock().is_empty()).await;
    assert_eq!(fixture.service.get_ready_pool(&session).len(), READY_POOL_SIZE);
}

/// LastFmRadioWarmupService: a persisted profile is warmed at startup without a listener, and
/// the queue holds a listener once, whatever the case of the name.
#[tokio::test]
async fn the_warmup_readies_persisted_stations_at_startup() {
    use crate::services::last_fm::last_fm_radio_warmup_service::LastFmRadioWarmupService;

    let fixture = fixture(0).await;
    let warmup = Arc::new(LastFmRadioWarmupService::new(
        fixture.service.clone(),
        fixture.state.clone(),
    ));
    assert!(!warmup.queue_user("  "));
    let stopping = CancellationToken::new();
    let run = tokio::spawn(warmup.clone().run(stopping.clone()));
    let session = session(&fixture);
    crate::services::common::test_fakes::until(|| {
        fixture.service.get_ready_pool(&session).len() == READY_POOL_SIZE
    })
    .await;
    // Taken off the queue once warmed, so it can be queued again; twice is once.
    crate::services::common::test_fakes::until(|| warmup.queue_user("Alice")).await;
    assert!(!warmup.queue_user("alice"));
    stopping.cancel();
    run.await.unwrap().unwrap();
    // The warm never records a play.
    assert!(fixture.state.get_user("alice").plays.is_empty());
}

/// The fixture's stream service with the stations' picker over a Last.fm fake whose rock chart
/// is twenty "Tag" songs, each found outside, and the station made a rock station. The fake
/// server rides along so it lives as long as the service.
async fn with_picker(
    fixture: &Fixture,
) -> (
    LastFmRadioStreamService,
    Arc<LastFmRadioRecommendationService>,
    MockServer,
) {
    let last_fm_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(|request: &wiremock::Request| {
            let method = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "method")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            let body = if method == "tag.gettoptracks" {
                let tracks: Vec<serde_json::Value> = (0..20)
                    .map(|index| {
                        serde_json::json!({
                            "name": format!("Tag{index}"), "duration": 180000,
                            "artist": {"name": format!("Tag Artist {index}")}
                        })
                    })
                    .collect();
                serde_json::json!({"tracks": {"track": tracks}})
            } else {
                serde_json::json!({})
            };
            ResponseTemplate::new(200).set_body_string(body.to_string())
        })
        .mount(&last_fm_server)
        .await;
    let settings = fixture.service.shared.settings.clone();
    let mut keyed = settings.current().as_ref().clone();
    keyed.last_fm.api_key = "key".into();
    settings.set(keyed);
    let metadata = Arc::new(FakeMetadata::default());
    for (index, title) in TITLES.iter().enumerate() {
        metadata.hits.lock().insert(
            (format!("Artist {index}"), title.to_string()),
            Song {
                id: format!("ext-soulseek-song-{title}"),
                artist: format!("Artist {index}"),
                title: title.to_string(),
                ..Default::default()
            },
        );
    }
    for index in 0..20 {
        metadata.hits.lock().insert(
            (format!("Tag Artist {index}"), format!("Tag{index}")),
            Song {
                id: format!("ext-soulseek-song-Tag{index}"),
                artist: format!("Tag Artist {index}"),
                title: format!("Tag{index}"),
                ..Default::default()
            },
        );
    }
    let mut station = fixture.state.get_user("alice").stations[0].clone();
    station.kind = octo_core::models::radio::LastFmRadioStationKind::Genre;
    station.seeds = vec!["rock".into()];
    fixture.state.replace_stations("alice", &[station]);
    let last_fm = Arc::new(LastFmService::with_base_url(
        settings.clone(),
        &format!("{}/", last_fm_server.uri()),
    ));
    let recommendations = Arc::new(LastFmRadioRecommendationService::new(
        last_fm,
        fixture.state.clone(),
        settings.clone(),
        None,
    ));
    let shared = Shared_ {
        metadata: metadata.clone(),
        recommendations: Some(recommendations.clone()),
        ..clone_shared(&fixture.service.shared)
    };
    let service = LastFmRadioStreamService {
        resolver: LastFmRadioTrackResolver::new(
            fixture.service.proxy.clone(),
            metadata,
            shared.registry.clone(),
        ),
        shared: Arc::new(shared),
        ..fixture.service.clone()
    };
    (service, recommendations, last_fm_server)
}

/// What a client hears from the stream until it has `limit` bytes.
async fn listen(
    service: &LastFmRadioStreamService,
    session: &LastFmRadioStreamSession,
    limit: usize,
) -> String {
    let hang_up = CancellationToken::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut client = Client {
        received: received.clone(),
        limit,
        hang_up: hang_up.clone(),
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        service.stream(session, &mut client, &hang_up, false),
    )
    .await
    .expect("the stream ended")
    .expect("the stream ends when the client hangs up");
    String::from_utf8(received.lock().clone()).unwrap()
}

/// Past the snapshot's last song the stream goes on with new songs of the station's kind, asked
/// of the stations' picker, instead of starting again from the first song.
#[tokio::test]
async fn the_stream_goes_on_past_the_snapshot_with_new_songs() {
    let fixture = fixture(0).await;
    let (service, _, _last_fm) = with_picker(&fixture).await;

    let session = session(&fixture);
    let snapshot: String = TITLES.iter().map(|title| format!("MP3:<{title}>")).collect();
    let text = listen(&service, &session, snapshot.len() + "MP3:<Tag".len()).await;

    assert!(text.starts_with(&snapshot), "{text}");
    assert!(text[snapshot.len()..].starts_with("MP3:<Tag"), "{text}");
}

/// After a getStationSongs page served the whole snapshot, a new stream plays its first ready
/// song at once and goes straight on with new songs: the songs served lately are passed over
/// before they are prepared, never prepared and then dropped.
#[tokio::test]
async fn a_stream_after_a_served_page_plays_at_once_and_prepares_no_served_song() {
    let fixture = fixture(0).await;
    let (service, recommendations, _last_fm) = with_picker(&fixture).await;
    let station = fixture.state.get_user("alice").stations[0].clone();
    recommendations.record_station_served("alice", &station.id, station.tracks.iter().cloned());

    let session = session(&fixture);
    let text = listen(&service, &session, "MP3:<One>".len() + "MP3:<Tag".len()).await;

    assert!(text.starts_with("MP3:<One>MP3:<Tag"), "{text}");
    let asked: Vec<String> = fixture
        .downloads
        .asked
        .lock()
        .iter()
        .map(|(_, id)| id.clone())
        .collect();
    for title in &TITLES[1..] {
        assert!(
            !asked.iter().any(|id| id == title),
            "{title} was prepared: {asked:?}"
        );
    }
}

/// A song the stream could not send whole is not remembered as served by the station.
#[tokio::test]
async fn a_song_cut_off_is_not_remembered_as_served() {
    let fixture = fixture(0).await;
    let (service, recommendations, _last_fm) = with_picker(&fixture).await;
    let station = fixture.state.get_user("alice").stations[0].clone();

    let session = session(&fixture);
    let text = listen(&service, &session, "MP3:<One>MP3:<Tw".len()).await;
    assert!(text.starts_with("MP3:<One>MP3:<Tw"), "{text}");

    let served: Vec<String> = recommendations
        .station_served("alice", &station.id)
        .into_iter()
        .map(|track| track.title)
        .collect();
    assert_eq!(served, ["One"]);
}
