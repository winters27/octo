//! The heart gate at the sources, through the real download service against the fake slskd and
//! shim: each file's sound is named in its bytes, printed and decoded from that name, and looked
//! up at an AcoustID that answers each print by its name. A heart's file is kept only on A0, A1 or
//! A2; the heard master is checked against the heard video fetched once per round; the heard
//! video's own stream copy is the heard audio only at its heard format and size; a song fetched
//! ahead is the heard audio only as the very copy that played; and a heart that joins another
//! origin's download is judged by its own gate.

use std::path::PathBuf;

use octo_core::fingerprint::acoust_id_client::{AcoustIdOutcome, parse_answer};
use octo_core::fingerprint::heart_gate::{HeardAnswerReading, HeardRecord, HeardSource, read_heard_answer};
use wiremock::matchers::{path, query_param};

use super::*;
use crate::services::common::soulseek_hold_store::SoulseekHoldStore;
use crate::services::common::{AcquisitionPlace, GateLedger, GateOutcome, acquisition_key};
use crate::services::fingerprint::catalog_clip::CatalogClip;
use crate::services::fingerprint::download_verification_service::HeartParts;
use crate::services::fingerprint::heard_master_check::HeardMasterCheck;
use crate::services::fingerprint::heart_test_support::{
    AcoustIdByPrint, NoCatalog, SoundDecoder, SoundPrinter, flac_header, frozen_body, gate_fixture,
    sound_bytes, sound_flac,
};
use crate::services::metadata::deezer_metadata_service::DeezerMetadataService;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::prefetch::heard_samples::tests::{Rig, items_of, rig_in};
use crate::services::prefetch::heard_samples::{HeardPrints, sample_id_of};
use crate::services::prefetch::{PrefetchStore, PrefetchedFiles};

/// The video the person heard.
const HEARD: &str = "HEARDVIDEO1";
const FIRST: &str = r"A\Drake\04 - Started.flac";
const SECOND: &str = r"B\Drake\04 - Started.flac";

/// A peer offering one file of a song, the faster peers tried first.
struct Peer {
    user: &'static str,
    file: &'static str,
    bytes: Vec<u8>,
    speed: i64,
    /// The song title whose search lists it, and the file's length.
    title: &'static str,
    seconds: i64,
}

/// A peer offering a 180 s file of "Started".
fn peer(user: &'static str, file: &'static str, bytes: Vec<u8>, speed: i64) -> Peer {
    Peer {
        user,
        file,
        bytes,
        speed,
        title: "Started",
        seconds: 180,
    }
}

/// How a gated walk is set up beyond its source and its peers.
#[derive(Default)]
struct Options {
    /// The user's AcoustID key, or `off`; empty uses Octo's own key on `limiter`'s lane.
    key: &'static str,
    /// The lanes Octo's own key's calls take turns on.
    limiter: Option<Arc<AcoustIdRateLimiter>>,
    /// Downloads nobody gates are verified as today.
    verify: bool,
}

/// A 180 s FLAC whose sound is named, with `isrc` as its own tag.
fn named_flac(print: &str, master: &str, answer: &str, isrc: Option<&str>) -> Vec<u8> {
    flac_of(180, print, master, answer, isrc)
}

/// A FLAC of `seconds` whose sound is named, with `isrc` as its own tag.
fn flac_of(seconds: u64, print: &str, master: &str, answer: &str, isrc: Option<&str>) -> Vec<u8> {
    match isrc {
        None => {
            let mut bytes = flac_header(seconds);
            bytes.extend_from_slice(&sound_bytes(print, master, answer));
            bytes
        }
        Some(isrc) => {
            let dir = tempfile::tempdir().expect("a temp dir");
            let path = dir.path().join("tagged.flac");
            sound_flac(&path, seconds, &sound_bytes(print, master, answer), Some(isrc));
            std::fs::read(&path).expect("the tagged file")
        }
    }
}

struct Gated {
    walk: Walk,
    rig: Rig,
    printer: Arc<SoundPrinter>,
    answers: AcoustIdByPrint,
    verification: Arc<DownloadVerificationService>,
    holds: Arc<SoulseekHoldStore>,
    prefetch: Arc<PrefetchStore>,
    prefetch_dir: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

/// The walk of [`build_all`], with the heart gate wired: heard samples over the fake shim printed
/// by name, the heard master decoded by name, the walk's own preview copies, and AcoustID answering
/// each print by its name with `key` (a user key, or `off`). Downloads nobody gates keep today's
/// checks with verification off; only gates judge.
async fn gated(root: &Root, source: DownloadSource, key: &'static str, peers: Vec<Peer>) -> Gated {
    gated_with(
        root,
        source,
        peers,
        Options {
            key,
            ..Default::default()
        },
    )
    .await
}

async fn gated_with(root: &Root, source: DownloadSource, peers: Vec<Peer>, options: Options) -> Gated {
    let key = options.key;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let songs: Vec<Song> = TITLES
        .iter()
        .enumerate()
        .map(|(i, (title, seconds))| {
            let id = registry.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some("Drake".into()),
                title: Some(title.to_string()),
                album: Some("Nothing Was the Same".into()),
                duration: Some(*seconds),
                track: Some(i as i32 + 1),
                ..Default::default()
            });
            Song {
                id: format!("ext-soulseek-{id}"),
                external_provider: Some("soulseek".into()),
                external_id: Some(id),
                title: title.to_string(),
                artist: "Drake".into(),
                album: "Nothing Was the Same".into(),
                duration: Some(*seconds),
                track: Some(i as i32 + 1),
                ..Default::default()
            }
        })
        .collect();

    let listed: Vec<(&'static str, Value)> = peers
        .iter()
        .map(|peer| {
            (
                peer.title,
                json!({
                    "username": peer.user, "uploadSpeed": peer.speed, "queueLength": 0, "hasFreeUploadSlot": true,
                    "files": [{"filename": peer.file, "size": peer.bytes.len() as i64, "length": peer.seconds, "extension": "flac"}],
                }),
            )
        })
        .collect();
    let slskd = FakeSlskd {
        root: root.path(),
        responses_for: Arc::new(move |text: &str| {
            listed
                .iter()
                .filter(|(title, _)| text.contains(title))
                .map(|(_, listing)| listing.clone())
                .collect()
        }),
        folders: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(SlskdState::default())),
    };
    {
        let mut state = slskd.state.lock();
        for peer in &peers {
            state.sound.insert(peer.file.to_string(), peer.bytes.clone());
        }
    }
    let server = MockServer::builder().start().await;
    let answers = AcoustIdByPrint::default();
    Mock::given(path("/v2/lookup"))
        .respond_with(answers.clone())
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(slskd.clone())
        .mount(&server)
        .await;

    let soulseek = SoulseekSettings {
        enabled: true,
        base_url: Some(server.uri()),
        username: Some("u".into()),
        password: Some("p".into()),
        min_file_size_bytes: 0,
        album_folders: false,
        parallel_downloads: 3,
        detect_transcodes: false,
        verify_downloads: options.verify,
        acoust_id_api_key: key.into(),
        download_timeout_seconds: 30,
        ..Default::default()
    };
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: soulseek.clone(),
        subsonic: SubsonicSettings {
            auto_detect_download_path: false,
            download_source: source,
            heart_download_sources: vec![octo_core::settings::HeartDownloadStep {
                source: octo_core::settings::HeartDownloadSource::Soulseek,
                song_enabled: Some(true),
                album_enabled: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    }));
    settings.set_raw("Library:DownloadPath", Some(&root.path()));
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
    for song in &songs {
        metadata.songs.lock().insert(
            ("soulseek".into(), song.external_id.clone().expect("an id")),
            song.clone(),
        );
    }
    let core = DownloadCore {
        settings: Arc::clone(&settings),
        local_library: Arc::new(FakeLocalLibrary::default()),
        metadata: Arc::new(metadata),
        navidrome_identity: NavidromeIdentityService::new(Arc::clone(&settings), reqwest::Client::new()),
        history: Arc::new(DownloadHistoryService::new(root.0.path().join("history.json"))),
        notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
    };
    let prefetch_dir = tempfile::tempdir().expect("the prefetch folder");
    let in_folder = prefetch_dir.path().to_path_buf();
    let prefetch = Arc::new(PrefetchStore::new(
        Arc::new(move || in_folder.clone()),
        octo_core::common::Clock::system(),
    ));
    let reviews = Arc::new(AskedReviews::default());
    let services = DownloadServices {
        concurrency: Some(Arc::new(DownloadConcurrency::new(Arc::clone(&settings)))),
        review_queue: Some(reviews.clone()),
        prefetched: Some(prefetch.clone() as Arc<dyn PrefetchedFiles>),
        ..Default::default()
    };
    let acoust_id = url::Url::parse(&format!("{}/", server.uri())).expect("parses");
    let acoust_id_client = match options.limiter {
        Some(limiter) => AcoustIdClient::with_built_in_key(
            Arc::new(AcoustIdRateLimitHandler::with_base_url(limiter, acoust_id)),
            Arc::clone(&settings),
            Some(crate::services::fingerprint::heart_test_support::OCTO_KEY),
        ),
        None => AcoustIdClient::new(
            Arc::new(AcoustIdRateLimitHandler::with_base_url(
                Arc::new(AcoustIdRateLimiter::new()),
                acoust_id,
            )),
            Arc::clone(&settings),
        ),
    };
    let verification = Arc::new(DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        Arc::new(acoust_id_client),
        Arc::clone(&settings),
        None,
        None,
    ));
    let dir = tempfile::tempdir().expect("the heard samples' folder");
    let holds = Arc::new(SoulseekHoldStore::new(None::<PathBuf>));
    let rig = rig_in(dir.path(), &server.uri(), holds.clone(), 16, 64 << 20);
    let printer = Arc::new(SoundPrinter::default());
    rig.samples.use_printer(printer.clone());
    let master = Arc::new(HeardMasterCheck::new(rig.samples.clone()));
    master.use_decoder(Arc::new(SoundDecoder::default()));
    let youtube = Arc::new(YouTubeResolver::with_base_url(Some(&server.uri())));
    let rejected = Arc::new(RejectedPeerRegistry::in_memory());
    verification.use_heart(HeartParts {
        registry: registry.clone(),
        samples: rig.samples.clone(),
        master,
        clip: Arc::new(CatalogClip::new(
            Arc::new(DeezerMetadataService::with_base_url(
                Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
                settings.clone(),
                server.uri(),
            )),
            rig.samples.clone(),
            youtube.clone(),
        )),
        catalog: Arc::new(NoCatalog),
        holds: holds.clone(),
        rejected: rejected.clone(),
    });
    let parts = SoulseekDownloadParts {
        slskd: client,
        rejected_peers: Arc::clone(&rejected),
        verification: verification.clone(),
        youtube,
        id_registry: registry.clone(),
        soulseek_link: None,
        lidarr_imports: None,
        lidarr_fetcher: None,
        spotiflac: None,
        preview_copies: Some(rig.copies.clone()),
    };
    let (service, _) = SoulseekDownloadService::build(core, services, parts);
    Gated {
        walk: Walk {
            service,
            registry,
            slskd,
            songs,
            album_id: "album-1".into(),
            rejected,
            settings,
            reviews,
            _server: server,
        },
        rig,
        printer,
        answers,
        verification,
        holds,
        prefetch,
        prefetch_dir,
        _dir: dir,
    }
}

impl Gated {
    fn started(&self) -> String {
        self.walk.songs[3].external_id.clone().expect("an id")
    }

    /// `HEARD` at `size`, whose stored whole print is `print`'s, and whose fresh stream the shim
    /// sends as `master` (at any format, unless `itag` names one).
    async fn heard_video(&self, size: u64, itag: Option<u32>, print: &str, master: &str) -> HeardRecord {
        let source = HeardSource::Video {
            id: HEARD.into(),
            size: Some(size),
            content_type: "audio/mp4".into(),
            itag,
        };
        let sample = sample_id_of(&source);
        self.rig.samples.store(
            &sample,
            &HeardPrints {
                raw: self.printer.items(print),
                compressed: "heard".into(),
                seconds: 240,
                whole: true,
                reaches_end: false,
            },
            Some(HEARD),
        );
        self.shim_streams(HEARD, None, sound_bytes(print, master, "heard"))
            .await;
        HeardRecord {
            heard_at: Utc::now(),
            source,
            sample_id: Some(sample),
        }
    }

    /// The shim's fresh `/stream` of `video` (at `itag` when named) sends `bytes`.
    async fn shim_streams(&self, video: &str, itag: Option<u32>, bytes: Vec<u8>) {
        let mut mock = Mock::given(path("/stream"))
            .and(query_param("id", video))
            .and(query_param("fresh", "1"));
        if let Some(itag) = itag {
            mock = mock.and(query_param("itag", itag.to_string()));
        }
        mock.respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Octo-Fresh", "1")
                .set_body_raw(bytes, "audio/mp4"),
        )
        .with_priority(1)
        .mount(&self.walk._server)
        .await;
    }

    /// How often the shim's fresh `/stream` of `video` was asked.
    async fn fresh_streams(&self, video: &str) -> usize {
        self.walk
            ._server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/stream")
            .filter(|r| r.url.query_pairs().any(|(k, v)| k == "id" && v == video))
            .filter(|r| r.url.query_pairs().any(|(k, v)| k == "fresh" && v == "1"))
            .count()
    }

    fn batch_users(&self) -> Vec<String> {
        self.walk.slskd.batches().into_iter().map(|(u, ..)| u).collect()
    }
}

/// The heart gate of `user` for "Started", who heard `heard` and whose heard answer reads `reading`.
fn gate_of(user: &str, heard: Option<HeardRecord>, reading: HeardReading) -> HeartGate {
    HeartGate {
        user: user.into(),
        heard,
        reading,
        ..started_gate()
    }
}

/// Runs the song's acquisition for `place`'s origin and gates, from `source`.
async fn acquire_as(g: &Gated, source: DownloadSource, place: AcquisitionPlace) -> anyhow::Result<String> {
    tokio::time::timeout(
        Duration::from_secs(60),
        g.walk.service.execute_acquisition_to(
            "soulseek",
            &g.started(),
            false,
            true,
            Some(source),
            &CancellationToken::new(),
            None,
            false,
            None,
            place,
        ),
    )
    .await
    .expect("within a minute")
}

fn heart_place(gates: Vec<HeartGate>, ledger: &GateLedger) -> AcquisitionPlace {
    AcquisitionPlace {
        origin: Some(AcquisitionOrigin::Heart),
        gates,
        ledger: ledger.clone(),
        ..Default::default()
    }
}

/// A clear AcoustID answer for "Started": its catalog recording, by many submissions.
fn started_clear() -> String {
    json!({"status": "ok", "results": [{"id": "fp", "score": 0.97, "recordings": [{
        "id": "rec-started", "title": "Started", "artists": [{"name": "Drake"}],
        "duration": 180, "sources": 40,
    }]}]})
    .to_string()
}

// ---- A2 at Soulseek ------------------------------------------------------------------------------

/// With an empty heard answer only A0 or A2 can keep a file: the first peer's upload of the song,
/// another master, is not kept and is refused for this heart only; the second peer's file, the very
/// master that played, is kept as the heard master. Both are checked against one fetch of the heard
/// video, the round this pass opened.
#[tokio::test]
async fn an_empty_heard_answer_keeps_the_soulseek_file_that_passes_a2_and_not_the_one_that_fails() {
    let root = Root::new("octo-gate-a2-");
    let g = gated(
        &root,
        DownloadSource::Soulseek,
        "key",
        vec![
            peer(
                "wrongpeer",
                FIRST,
                named_flac("started", "another-master", "nothing", None),
                2_000_000,
            ),
            peer(
                "rightpeer",
                SECOND,
                named_flac("started", "heard-master", "nothing", None),
                1_000_000,
            ),
        ],
    )
    .await;
    let heard = g.heard_video(4_000_000, None, "started", "heard-master").await;
    let gate = gate_of("brandon", Some(heard), HeardReading::OnlyHeardOrMaster);
    let key = acquisition_key("soulseek", &g.started(), false);
    let gates: &dyn crate::services::common::heart_acquisition_coordinator::HeartGates =
        g.verification.as_ref();
    let round = gates.open_rounds(&key, std::slice::from_ref(&gate));
    let ledger = GateLedger::default();

    let path = acquire_as(
        &g,
        DownloadSource::Soulseek,
        heart_place(vec![gate.clone()], &ledger),
    )
    .await
    .expect("the heard master lands");
    drop(round);
    assert!(std::path::Path::new(&path).is_file(), "{path}");
    assert_eq!(g.batch_users(), names(&["wrongpeer", "rightpeer"]));
    assert_eq!(g.fresh_streams(HEARD).await, 1, "one heard copy for the round");
    assert_eq!(
        ledger.outcome_for("brandon", AcquisitionOrigin::Heart),
        Some(GateOutcome::Accepted)
    );
    let song = g.started();
    let r = gate.r.clone().expect("R");
    assert!(
        !g.walk.rejected.is_denied(Some("wrongpeer"), Some(FIRST)),
        "never for imports"
    );
    assert!(
        g.walk
            .rejected
            .is_denied_for_heart(&song, &r, Some("wrongpeer"), Some(FIRST))
    );
    assert!(
        !g.walk
            .rejected
            .is_denied_for_heart(&song, &r, Some("rightpeer"), Some(SECOND))
    );
}

/// Another upload of the heard recording that AcoustID has no entry for, and that is not the master
/// that played, is refused for the heart, though it prints like what played.
#[tokio::test]
async fn another_upload_of_the_heard_recording_with_no_acoust_id_entry_is_refused() {
    let root = Root::new("octo-gate-upload-");
    let g = gated(
        &root,
        DownloadSource::Soulseek,
        "key",
        vec![peer(
            "uploader",
            FIRST,
            named_flac("started", "another-upload", "nothing", None),
            1_000_000,
        )],
    )
    .await;
    let heard = g.heard_video(4_000_000, None, "started", "heard-master").await;
    let gate = gate_of("brandon", Some(heard), HeardReading::RStays { tag: None });
    let ledger = GateLedger::default();
    let refused = acquire_as(
        &g,
        DownloadSource::Soulseek,
        heart_place(vec![gate.clone()], &ledger),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(g.answers.asked(), ["nothing"], "its own print was looked up");
    assert_eq!(
        g.fresh_streams(HEARD).await,
        1,
        "its master was compared with what played"
    );
    assert!(
        ledger.outcome_for("brandon", AcquisitionOrigin::Heart) != Some(GateOutcome::Accepted),
        "never kept for the heart"
    );
    assert!(g.walk.rejected.is_denied_for_heart(
        &g.started(),
        gate.r.as_ref().expect("R"),
        Some("uploader"),
        Some(FIRST)
    ));
    assert!(!g.walk.rejected.is_denied(Some("uploader"), Some(FIRST)));
}

/// A 2009 Help! heart, its heard answer read from the frozen 2009 stream: the first peer's 2015
/// mix, tagged as the 2015 mix, is refused by its own label and the next peer is tried; the second
/// peer's 2009 file is the master that played and lands.
#[tokio::test]
async fn a_2015_help_peer_is_refused_for_a_2009_heart_and_the_next_peer_is_tried() {
    let root = Root::new("octo-gate-help-");
    let g = gated(
        &root,
        DownloadSource::Soulseek,
        "key",
        vec![
            peer(
                "mix2015",
                FIRST,
                named_flac(
                    "help-2015-120",
                    "help-2015-master",
                    "help-2015-120",
                    Some("GBUM71505900"),
                ),
                2_000_000,
            ),
            peer(
                "mix2009",
                SECOND,
                named_flac(
                    "help-2009-120",
                    "help-2009-master",
                    "help-2009-120",
                    Some("GBAYE0601465"),
                ),
                1_000_000,
            ),
        ],
    )
    .await;
    let frozen = |name: &str| items_of(&gate_fixture(&format!("prints/{name}.json")));
    for name in ["help-2015-120", "help-2009-120", "help-2009-240"] {
        g.printer.register(name, frozen(name));
        g.answers.answer(name, &frozen_body(name));
    }
    let r = HeardRecording {
        ids: vec!["4416d38e-0f68-4c75-93b9-c1a793fccb49".into()],
        duplicates: vec!["4416d38e-0f68-4c75-93b9-c1a793fccb49".into()],
        isrcs: vec!["GBAYE0601465".into()],
        title: "Help! (Remastered 2009)".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    };
    let heard_answer = AcoustIdOutcome::Answered(
        parse_answer(&serde_json::from_str(&frozen_body("help-2009-240")).expect("JSON"))
            .expect("reads")
            .expect("ok"),
    );
    let reading = match read_heard_answer(&heard_answer, &r, 0.85) {
        HeardAnswerReading::RStays { tag } => HeardReading::RStays { tag },
        other => panic!("the 2009 stream for a 2009 row: {other:?}"),
    };
    let heard = g
        .heard_video(4_000_000, None, "help-2009-240", "help-2009-master")
        .await;
    let gate = HeartGate {
        r: Some(r.clone()),
        ..gate_of("brandon", Some(heard), reading)
    };
    let ledger = GateLedger::default();
    let path = acquire_as(&g, DownloadSource::Soulseek, heart_place(vec![gate], &ledger))
        .await
        .expect("the 2009 master lands");
    assert!(std::path::Path::new(&path).is_file(), "{path}");
    assert_eq!(g.batch_users(), names(&["mix2015", "mix2009"]));
    assert!(
        g.walk
            .rejected
            .is_denied_for_heart(&g.started(), &r, Some("mix2015"), Some(FIRST))
    );
    assert!(!g.walk.rejected.is_denied(Some("mix2015"), Some(FIRST)));
    let (recording, _) = octo_media::tags::tag_writer_extras::read_identity(std::path::Path::new(&path));
    assert_eq!(recording, None, "kept as heard: no recording tag claimed");
}

// ---- A0: the heard video's own stream -----------------------------------------------------------

/// Real AAC audio in mp4, as YouTube serves a stream at format 140.
fn aac_stream() -> Vec<u8> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dir.path().join("stream.mp4");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
        ])
        .args(["-c:a", "aac", "-b:a", "128k", "-f", "mp4"])
        .arg(&out)
        .status()
        .expect("ffmpeg runs: the gate needs it");
    assert!(made.success());
    std::fs::read(&out).expect("made")
}

/// What a YouTube heart kept from the heard video's own stream, with AcoustID off: the heard video
/// heard at `heard_size` and `heard_itag`, the shim serving it at format 140 only.
async fn stream_copy_kept(heard_size: Option<u64>, heard_itag: u32) -> anyhow::Result<String> {
    let root = Root::new("octo-gate-a0-");
    let g = gated(&root, DownloadSource::YouTube, "off", Vec::new()).await;
    let stream = aac_stream();
    let size = heard_size.unwrap_or(stream.len() as u64);
    g.shim_streams(HEARD, Some(140), stream).await;
    Mock::given(path("/stream"))
        .and(query_param("id", HEARD))
        .and(query_param("fresh", "1"))
        .and(query_param("itag", "251"))
        .respond_with(
            ResponseTemplate::new(404)
                .insert_header("X-Octo-Fresh", "1")
                .set_body_raw(r#"{"error":"itag_unavailable","fresh":true}"#, "application/json"),
        )
        .with_priority(1)
        .mount(&g.walk._server)
        .await;
    let heard = HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Video {
            id: HEARD.into(),
            size: Some(size),
            content_type: "audio/mp4".into(),
            itag: Some(heard_itag),
        },
        sample_id: None,
    };
    let gate = gate_of("brandon", Some(heard), HeardReading::OnlyHeardOrMaster);
    let kept = acquire_as(
        &g,
        DownloadSource::YouTube,
        heart_place(vec![gate], &GateLedger::default()),
    )
    .await;
    assert!(
        g.walk.slskd.state.lock().you_tube_downloads.is_empty(),
        "the shim's MP3 is never the heard audio"
    );
    if let Ok(path) = &kept {
        assert!(std::path::Path::new(path).is_file(), "{path}");
    }
    kept
}

/// The heard video's own stream copy at the heard format and size is the very audio that played:
/// kept with AcoustID off, as an m4a. At another size or another format it is not the heard file,
/// and nothing is kept.
#[tokio::test]
async fn the_heard_videos_own_stream_is_kept_only_at_its_heard_format_and_size() {
    let kept = stream_copy_kept(None, 140)
        .await
        .expect("the heard stream is kept");
    assert!(kept.ends_with(".m4a"), "{kept}");

    let other_size = stream_copy_kept(Some(1), 140).await;
    assert!(other_size.is_err(), "another size: {other_size:?}");
    let other_format = stream_copy_kept(None, 251).await;
    assert!(other_format.is_err(), "another format: {other_format:?}");
}

/// A YouTube video other than the one heard, which AcoustID has no entry for and which is not the
/// master that played, is refused for this heart (each such video in turn), never for every user.
#[tokio::test]
async fn another_youtube_video_with_no_clear_reading_is_refused_for_the_heart() {
    let root = Root::new("octo-gate-yt-");
    let g = gated(&root, DownloadSource::YouTube, "key", Vec::new()).await;
    {
        let mut state = g.walk.slskd.state.lock();
        state.search_videos = vec!["OTHERVIDEO1".into(), "OTHERVIDEO2".into()];
        state.download_audio = Some(sound_bytes("started", "another-master", "nothing"));
    }
    // Heard at a size the shim no longer serves: no stream copy of it, but its master for A2.
    let heard = g.heard_video(1, None, "started", "heard-master").await;
    let gate = gate_of("brandon", Some(heard), HeardReading::RStays { tag: None });
    let refused = acquire_as(
        &g,
        DownloadSource::YouTube,
        heart_place(vec![gate.clone()], &GateLedger::default()),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(g.walk.slskd.state.lock().you_tube_downloads.len(), 2);
    let song = g.started();
    let r = gate.r.clone().expect("R");
    let routing = g
        .walk
        .registry
        .resolve_song(&song, false)
        .expect("registered")
        .snapshot();
    for video in ["OTHERVIDEO1", "OTHERVIDEO2"] {
        assert!(
            g.walk
                .rejected
                .is_denied_for_heart(&song, &r, Some(YOUTUBE_PEER), Some(video)),
            "{video}"
        );
        assert!(!routing.refuses_video(video), "{video} stays for everyone else");
    }
}

// ---- Songs fetched ahead -------------------------------------------------------------------------

/// The song fetched ahead: its copy and the facts it was made ready with.
async fn fetched_ahead(g: &Gated) -> (PathBuf, HeardSource) {
    let ahead = g
        .walk
        .service
        .download_ahead(
            "soulseek",
            &g.started(),
            g.prefetch_dir.path(),
            &CancellationToken::new(),
        )
        .await
        .expect("fetched ahead");
    let crate::services::i_download_service::Ahead::Kept { path } = ahead else {
        panic!("kept ahead");
    };
    let facts: crate::services::prefetch::AheadSidecar = serde_json::from_slice(
        &std::fs::read(crate::services::prefetch::sidecar_path(&path)).expect("the facts"),
    )
    .expect("JSON");
    let copy = HeardSource::Copy {
        key: format!("soulseek:{}", g.started()),
        ready_at: facts.ready_at,
        source_file: facts.source_file,
    };
    (path, copy)
}

fn heard_copy(source: HeardSource) -> HeardRecord {
    HeardRecord {
        heard_at: Utc::now(),
        source,
        sample_id: None,
    }
}

async fn ahead_walk(root: &Root, answer: &str) -> Gated {
    gated(
        root,
        DownloadSource::Soulseek,
        "key",
        vec![peer(
            "aheadpeer",
            FIRST,
            named_flac("started", "ahead-master", answer, None),
            1_000_000,
        )],
    )
    .await
}

/// The copy fetched ahead that the person heard is the very audio that played: kept with no second
/// transfer, though AcoustID has no entry for it.
#[tokio::test]
async fn a_heard_copy_fetched_ahead_is_kept_as_it_is() {
    let root = Root::new("octo-gate-ahead-");
    let g = ahead_walk(&root, "nothing").await;
    let (_, copy) = fetched_ahead(&g).await;
    assert_eq!(g.batch_users().len(), 1);
    let gate = gate_of(
        "brandon",
        Some(heard_copy(copy)),
        HeardReading::RStays { tag: None },
    );
    let ledger = GateLedger::default();
    let kept = acquire_as(&g, DownloadSource::Soulseek, heart_place(vec![gate], &ledger))
        .await
        .expect("the heard copy is kept");
    assert!(std::path::Path::new(&kept).is_file(), "{kept}");
    assert_eq!(g.batch_users().len(), 1, "no second transfer");
    assert_eq!(
        ledger.outcome_for("brandon", AcquisitionOrigin::Heart),
        Some(GateOutcome::Accepted)
    );
}

/// A copy fetched again under the heard key is another copy than the one that played: with no clear
/// AcoustID reading it is passed over and the chain runs.
#[tokio::test]
async fn a_copy_fetched_again_under_the_heard_key_needs_a_clear_reading() {
    let root = Root::new("octo-gate-ahead-");
    let g = ahead_walk(&root, "nothing").await;
    let (_, copy) = fetched_ahead(&g).await;
    let HeardSource::Copy { key, source_file, .. } = copy else {
        unreachable!("a copy");
    };
    let earlier = HeardSource::Copy {
        key,
        ready_at: Some(Utc::now() - chrono::Duration::hours(1)),
        source_file,
    };
    let gate = gate_of(
        "brandon",
        Some(heard_copy(earlier)),
        HeardReading::RStays { tag: None },
    );
    let refused = acquire_as(
        &g,
        DownloadSource::Soulseek,
        heart_place(vec![gate], &GateLedger::default()),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        g.batch_users().len(),
        2,
        "the chain ran after the copy was passed over"
    );
    assert!(
        g.prefetch.ready("soulseek", &g.started()).is_none(),
        "the copy is gone"
    );
}

/// Another person's copy fetched ahead is not what this person heard: it is kept for them only by
/// its own clear AcoustID reading, and passed over without one.
#[tokio::test]
async fn another_persons_copy_fetched_ahead_needs_a_clear_reading() {
    for (answer, kept) in [("clear", true), ("nothing", false)] {
        let root = Root::new("octo-gate-ahead-");
        let g = ahead_walk(&root, answer).await;
        g.answers.answer("clear", &started_clear());
        fetched_ahead(&g).await;
        let heard = g.heard_video(4_000_000, None, "started", "anna-heard").await;
        let gate = gate_of("anna", Some(heard), HeardReading::RStays { tag: None });
        let ledger = GateLedger::default();
        let got = acquire_as(&g, DownloadSource::Soulseek, heart_place(vec![gate], &ledger)).await;
        if kept {
            got.expect("kept by its own reading");
            assert_eq!(g.batch_users().len(), 1, "{answer}: no second transfer");
            assert_eq!(
                ledger.outcome_for("anna", AcquisitionOrigin::Heart),
                Some(GateOutcome::Accepted)
            );
        } else {
            assert!(got.is_err(), "{answer}: {got:?}");
            assert_eq!(g.batch_users().len(), 2, "{answer}: the chain ran");
        }
    }
}

// ---- A heart joining another origin's download --------------------------------------------------

/// A heart that joined an import's download is judged by its own gate: the import lands by its own
/// checks, and the heart counts the file as its own only when its gate passes it.
#[tokio::test]
async fn a_heart_joining_an_import_is_judged_by_its_own_gate() {
    for (master, accepted) in [("another-master", false), ("heard-master", true)] {
        let root = Root::new("octo-gate-join-");
        let g = gated(
            &root,
            DownloadSource::Soulseek,
            "key",
            vec![peer(
                "onlypeer",
                FIRST,
                named_flac("started", master, "nothing", None),
                1_000_000,
            )],
        )
        .await;
        let heard = g.heard_video(4_000_000, None, "started", "heard-master").await;
        let joiner = gate_of("anna", Some(heard), HeardReading::RStays { tag: None });
        let ledger = GateLedger::default();
        let place = AcquisitionPlace {
            origin: Some(AcquisitionOrigin::Import),
            gates: vec![import_gate(&["rec-started"]), joiner],
            ledger: ledger.clone(),
            ..Default::default()
        };
        let landed = acquire_as(&g, DownloadSource::Soulseek, place)
            .await
            .expect("the import lands by its own checks");
        assert!(std::path::Path::new(&landed).is_file(), "{landed}");
        assert_eq!(
            ledger.outcome_for("brandon", AcquisitionOrigin::Import),
            Some(GateOutcome::Accepted),
            "{master}"
        );
        let heart = ledger.outcome_for("anna", AcquisitionOrigin::Heart);
        if accepted {
            assert_eq!(heart, Some(GateOutcome::Accepted), "{master}");
        } else {
            assert!(
                matches!(heart, Some(GateOutcome::NotAccepted(_))),
                "{master}: {heart:?}"
            );
        }
        assert_eq!(
            g.fresh_streams(HEARD).await,
            1,
            "{master}: the heart's own check ran"
        );
    }
}

/// A song fetched ahead that an import keeps is judged by the gate of a heart that joined it: the
/// import keeps the copy, and the heart counts it as its own only when its own gate passes it.
#[tokio::test]
async fn a_heart_joining_an_import_of_a_song_fetched_ahead_is_judged_by_its_own_gate() {
    let root = Root::new("octo-gate-join-ahead-");
    let g = ahead_walk(&root, "nothing").await;
    fetched_ahead(&g).await;
    let heard = g.heard_video(4_000_000, None, "started", "not-the-copy").await;
    let joiner = gate_of("anna", Some(heard), HeardReading::RStays { tag: None });
    let ledger = GateLedger::default();
    let place = AcquisitionPlace {
        origin: Some(AcquisitionOrigin::Import),
        gates: vec![import_gate(&["rec-started"]), joiner],
        ledger: ledger.clone(),
        ..Default::default()
    };
    let kept = acquire_as(&g, DownloadSource::Soulseek, place)
        .await
        .expect("the import keeps the copy");
    assert!(std::path::Path::new(&kept).is_file(), "{kept}");
    assert_eq!(g.batch_users().len(), 1, "no second transfer");
    assert_eq!(
        ledger.outcome_for("brandon", AcquisitionOrigin::Import),
        Some(GateOutcome::Accepted)
    );
    let heart = ledger.outcome_for("anna", AcquisitionOrigin::Heart);
    assert!(matches!(heart, Some(GateOutcome::NotAccepted(_))), "{heart:?}");
}

/// What the file's ISRC tag reads back as, so a tagged peer file is known to carry its label.
#[test]
fn a_named_flac_keeps_its_isrc_tag_and_its_sound() {
    let bytes = named_flac("p", "m", "a", Some("GBUM71505900"));
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("t.flac");
    std::fs::write(&path, &bytes).expect("written");
    let facts = octo_media::tags::tag_writer_extras::read_facts(&path.to_string_lossy(), true);
    assert_eq!(facts.isrcs, ["GBUM71505900"]);
    assert!(
        bytes
            .windows(b"OCTO-SOUND[p|m|a]".len())
            .any(|w| w == b"OCTO-SOUND[p|m|a]")
    );
}

// ---- The bulk turn ------------------------------------------------------------------------------

/// Octo's own key's bulk pace backed off to its outage ceiling (640 s, scaled down here to 640 ms
/// with the floors), an import queued first and a heart behind it, one download worker: the
/// import's file cannot have its bulk turn now, so it waits in the hold path and its job ends; the
/// heart starts at once and lands; the import lands only once its bulk turn comes.
#[tokio::test]
async fn a_heart_behind_an_import_on_a_backed_off_bulk_gap_starts_at_once_and_the_import_lands_after_its_turn()
 {
    use crate::services::common::{
        AcquisitionTarget, AcquisitionTracker, AcquisitionWorker, HeartAcquisitionCoordinator,
        HeartCoordinatorExtras, HeartGates, TrackAcquisitionQueue, TrackOutcome,
    };
    use crate::services::fingerprint::acoust_id_rate_limiter::{BusyKind, CallEnd, KeyLane, LookupClass};

    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(200), Duration::from_millis(20))
            .with_jitter(|| 1.0),
    );
    let root = Root::new("octo-gate-bulk-");
    let g = gated_with(
        &root,
        DownloadSource::Soulseek,
        vec![
            Peer {
                title: "Intro",
                seconds: 90,
                ..peer(
                    "importpeer",
                    r"C\Drake\01 - Intro.flac",
                    flac_of(90, "intro", "intro-master", "intro", None),
                    1_000_000,
                )
            },
            peer(
                "heartpeer",
                SECOND,
                named_flac("started", "heard-master", "started", None),
                1_000_000,
            ),
        ],
        Options {
            limiter: Some(limiter.clone()),
            verify: true,
            ..Default::default()
        },
    )
    .await;
    let named = |id: &str, title: &str, seconds: i32| {
        json!({"status": "ok", "results": [{"id": "fp", "score": 0.97, "recordings": [{
            "id": id, "title": title, "artists": [{"name": "Drake"}], "duration": seconds, "sources": 40,
        }]}]})
        .to_string()
    };
    g.answers.answer("intro", &named("rec-intro", "Intro", 90));
    g.answers.answer("started", &named("rec-started", "Started", 180));
    g.answers.answer("heard", &named("rec-started", "Started", 180));
    let heard = g.heard_video(4_000_000, None, "started", "heard-master").await;
    let started = g.started();
    let intro = g.walk.songs[0].external_id.clone().expect("an id");
    assert!(g.walk.registry.remember_heard(&started, "anna", heard, None));

    let queue = Arc::new(TrackAcquisitionQueue::new());
    let coordinator = HeartAcquisitionCoordinator::new(
        g.walk.settings.clone(),
        queue.clone(),
        g.walk.service.clone(),
        Arc::new(crate::services::common::test_fakes::FakeLidarr::default()),
        HeartCoordinatorExtras {
            tracker: Some(Arc::new(AcquisitionTracker::new(
                None,
                octo_core::common::Clock::system(),
            ))),
            id_registry: Some(g.walk.registry.clone()),
            holds: Some(g.holds.clone()),
            gates: Some(g.verification.clone() as Arc<dyn HeartGates>),
            ..Default::default()
        },
    );
    let resumer = Arc::downgrade(&coordinator);
    g.verification.on_resume(Arc::new(move |key: &str| {
        if let Some(coordinator) = resumer.upgrade() {
            coordinator.resume_held(key);
        }
    }));
    let stop = CancellationToken::new();
    let worker = Arc::new(AcquisitionWorker::new(
        queue.clone(),
        g.walk.service.clone(),
        g.walk.registry.clone(),
        Arc::new(NotificationService::new(
            Vec::new(),
            g.walk.settings.clone(),
            None,
        )),
        None,
    ));
    tokio::spawn(worker.run(stop.clone()));

    // AcoustID failed five bulk calls in a row: the bulk pace is at its outage ceiling.
    for _ in 0..5 {
        limiter
            .permit(KeyLane::BuiltIn, LookupClass::Bulk, true)
            .await
            .expect("a turn")
            .end(CallEnd::Busy(BusyKind::Outage));
    }
    let turn = std::time::Instant::now() + Duration::from_millis(640);
    assert_eq!(
        limiter.gap(KeyLane::BuiltIn, LookupClass::Bulk),
        Duration::from_millis(640)
    );

    let importing = coordinator.clone();
    let import_id = intro.clone();
    let import = tokio::spawn(async move {
        importing
            .acquire_track_for(
                "soulseek",
                &import_id,
                Some("brandon"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Import,
            )
            .await
    });
    for _ in 0..200 {
        if queue.is_in_flight("soulseek", &intro) || !g.holds.snapshot().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let hearted = coordinator
        .acquire_track_outcome(
            "soulseek",
            &started,
            Some("anna"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("the heart ran");
    assert!(matches!(hearted, TrackOutcome::Landed(_)), "{hearted:?}");
    assert!(
        std::time::Instant::now() < turn,
        "the heart never waited for the import's bulk turn"
    );
    import
        .await
        .expect("joined")
        .expect("the import's job ended without an error");
    assert_eq!(placed_flacs(&root), 1, "only the heart's file is placed yet");
    let held: Vec<String> = g.holds.snapshot().iter().map(|record| record.key()).collect();
    assert_eq!(held, [format!("Track:soulseek:{intro}:AcoustId")]);

    for _ in 0..1000 {
        if placed_flacs(&root) == 2 && g.holds.snapshot().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(placed_flacs(&root), 2, "the import landed after its turn");
    assert!(g.holds.snapshot().is_empty());
    assert!(std::time::Instant::now() >= turn);
    stop.cancel();
}
