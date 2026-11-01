//! The sweep's replacement end to end: a song its YouTube preview says is another goes through the
//! real `ExecutorSweepReplacer` and `LibraryActionExecutor` to a download the test plays, leaves an
//! Acted note and the original in the trash, and Put back brings the original home again.

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;

use octo_core::settings::{LibraryActionDefinition, NoticeKind, SubsonicSettings};
use serde_json::json;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::TrackAcquisitionQueue;
use crate::services::common::test_fakes::until;
use crate::services::fingerprint::preview_check::{Comparison, PreviewFacts};
use crate::services::library::library_action_journal::LibraryActionJournal;
use crate::services::library::library_action_test_support::{
    Extras, RecordingLibrary, executor, slash_root, store,
};
use crate::services::library::library_edit_service::LibraryEditServices;
use crate::services::library::library_tag_edits::TagEditJournal;
use crate::services::library::{LibraryActionQuarantine, LibraryEditService, LibraryRescan, NoticeState};
use crate::services::local::LocalSongMapping;
use crate::services::subsonic::NavidromeIdentityService;

const FILE_NAME: &str = "Massive Attack - Teardrop.mp3";

/// An MP3 of `frames` silent MPEG-1 Layer III frames, 128 kbps, 44.1 kHz, tagged as Teardrop.
fn teardrop_mp3(path: &Path, frames: usize) {
    let mut bytes = vec![0u8; 417 * frames];
    for frame in 0..frames {
        bytes[frame * 417..frame * 417 + 4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    }
    std::fs::write(path, bytes).expect("written");
    let mut file = TagFile::open(path).expect("an MP3");
    file.set_title(Some("Teardrop"));
    file.set_album(Some("Mezzanine"));
    file.set_performers(&["Massive Attack".to_string()]);
    file.set_album_artists(&["Massive Attack".to_string()]);
    file.save().expect("saved");
}

/// AcoustID has never heard the file, and its YouTube preview, a YouTube Music row from the
/// file's own album, says it is another song.
#[derive(Default)]
struct HeardAsAnother {
    verified: AtomicUsize,
}

#[async_trait]
impl IReviewSweepVerifier for HeardAsAnother {
    fn is_ready(&self) -> bool {
        true
    }

    async fn verify(&self, _: &str, _: Option<&str>, _: Option<&str>) -> VerificationResult {
        self.verified.fetch_add(1, Ordering::SeqCst);
        VerificationResult {
            reason: InconclusiveReason::NoEntry,
            fingerprint: Some("AQADtEqk".into()),
            duration_seconds: 330,
            ..Default::default()
        }
    }

    async fn isrc_agrees(&self, _: &VerificationResult, _: &str) -> Option<bool> {
        None
    }

    async fn hear_preview(&self, _: &str, _: &PreviewRequest<'_>, _: &VerificationResult) -> PreviewOutcome {
        PreviewOutcome::Compared {
            heard: Heard::Mismatch,
            comparison: Comparison {
                bit_error: 0.5,
                offset_seconds: 0.0,
            },
            trusted: true,
            preview: PreviewFacts {
                album: Some("Mezzanine".into()),
                ..PreviewFacts::bare("vid")
            },
        }
    }
}

struct Idle;

impl IAcquisitionActivity for Idle {
    fn is_busy(&self) -> bool {
        false
    }
}

/// A Navidrome whose admin left no mark on song nd-1, the only user, at `size` bytes.
async fn navidrome(size: i64) -> MockServer {
    let server = MockServer::start().await;
    let answer = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(path("/auth/login"))
        .respond_with(answer(json!({
            "token": "jwt", "isAdmin": true, "username": "admin",
            "subsonicToken": "st", "subsonicSalt": "ss",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/song/nd-1"))
        .respond_with(answer(json!({
            "id": "nd-1", "path": FILE_NAME, "size": size, "title": "Teardrop",
            "artist": "Massive Attack", "album": "Mezzanine", "suffix": "mp3", "duration": 330,
            "starred": false, "rating": 0, "playCount": 4,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/search3"))
        .respond_with(answer(json!({"subsonic-response": {
            "status": "ok",
            "searchResult3": {"song": [{"id": "nd-1", "size": size}]},
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .respond_with(answer(json!([{"id": "u1", "userName": "admin"}])))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw("[]", "application/json"))
        .mount(&server)
        .await;
    server
}

/// Every audio file under `dir`, walked.
fn mp3s_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(items) = std::fs::read_dir(dir) else {
        return found;
    };
    for item in items.flatten() {
        let path = item.path();
        if path.is_dir() {
            found.extend(mp3s_under(&path));
        } else if path.extension().is_some_and(|e| e == "mp3") {
            found.push(path);
        }
    }
    found
}

#[tokio::test]
async fn the_sweep_replaces_a_song_its_preview_says_is_another_and_put_back_restores_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("music");
    std::fs::create_dir_all(&root).expect("the music root");
    let song = root.join(FILE_NAME);
    teardrop_mp3(&song, 20);
    let original_bytes = std::fs::read(&song).expect("the original");
    let navidrome = navidrome(original_bytes.len() as i64).await;

    let settings = store(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run: false,
            review_enabled: true,
            review_sweep_per_hour: 3600,
            review_sweep_auto_replace_preview: true,
            keep_replaced_originals: true,
            allowed_users: vec!["admin".into()],
            actions: vec![LibraryActionDefinition {
                action: LibraryAction::WrongSong,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            url: Some(navidrome.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("secret".into()),
            auto_detect_download_path: false,
            download_source: DownloadSource::SoulseekThenYouTube,
            ..Default::default()
        },
        soulseek: SoulseekSettings {
            enabled: true,
            base_url: Some("http://slskd.test".into()),
            ..Default::default()
        },
        ..Default::default()
    });
    let music_root = slash_root(&root);
    settings.set_raw("Library:DownloadPath", Some(&music_root));

    let journal = Arc::new(LibraryActionJournal::new());
    let library = Arc::new(RecordingLibrary::default());
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let notices = Arc::new(NoticeQueue::new());
    let actions = executor(
        &settings,
        Extras {
            journal: Some(journal.clone()),
            library: Some(library.clone()),
            queue: Some(queue.clone()),
            notices: Some(notices.clone()),
            ..Default::default()
        },
    );
    actions.configure(|seams| {
        seams.history_poll = Duration::from_millis(1);
        seams.history_attempts = 1;
        seams.shows_at = Some(Arc::new(|_, _| async { true }.boxed()));
        seams.force_scan = Some(Arc::new(|| async { true }.boxed()));
    });
    let http = crate::services::http_client_factory::default_client();
    let identity = NavidromeIdentityService::new(settings.clone(), http.clone());
    let resolver = Arc::new(NavidromeSongPathResolver::new(
        identity.clone(),
        library.clone(),
        http.clone(),
        settings.clone(),
    ));
    let replacer = ExecutorSweepReplacer::new(
        resolver.clone(),
        actions,
        Arc::new(NavidromePlaylistApi::new(
            http.clone(),
            identity.clone(),
            settings.clone(),
        )),
        None,
        Arc::new(NavidromeOtherListeners::new(
            Arc::new(NavidromeAdmin::new(identity, http, settings.clone())),
            Arc::new(FamilyStore::in_memory()),
        )),
    );
    let verifier = Arc::new(HeardAsAnother::default());
    let swept_root = music_root.clone();
    let worker = Arc::new(
        LibraryReviewSweepWorker::new(
            LibraryReviewSweepParts {
                store: Arc::new(ReviewSweepStore::new(None)),
                notices: notices.clone(),
                verifier: verifier.clone(),
                activity: Arc::new(Idle),
                library: library.clone(),
                settings: settings.clone(),
                music_root: Box::new(move || swept_root.clone()),
            },
            Clock::system(),
        )
        .with_replacer(Arc::new(replacer)),
    );

    // Octo signs in to Navidrome as the admin before the sweep's search can name the song.
    assert!(resolver.resolve("nd-1").await.is_some());
    assert!(
        resolver
            .find_id_by_path("Massive Attack", "Teardrop", &slash_root(&song))
            .await
            .is_some()
    );

    let ticking = worker.clone();
    let mut tick = tokio::spawn(async move { ticking.tick().await });
    let never = CancellationToken::new();
    let download = tokio::select! {
        download = tokio::time::timeout(Duration::from_secs(10), queue.dequeue(&never)) => {
            download.expect("a download in time").expect("a download")
        }
        _ = &mut tick => panic!(
            "the tick ended without a download: {:?}; asked {}; notes {:?}; journal {:?}",
            worker.status(),
            verifier.verified.load(Ordering::SeqCst),
            notices.for_user("admin", NoticeKind::Review),
            journal.recent(10),
        ),
    };
    // Replaced by its preview: only from a source the preview checks, never the YouTube fallback.
    assert_eq!(download.source_override, Some(DownloadSource::Soulseek));

    // The download service's reveal: the check, then the right copy into the song's place.
    let incoming = root.join(".octo-incoming");
    std::fs::create_dir_all(&incoming).expect("the incoming folder");
    let staged = incoming.join("replacement.mp3");
    teardrop_mp3(&staged, 30);
    let right_bytes = std::fs::read(&staged).expect("the right copy");
    assert_ne!(right_bytes, original_bytes);
    let staged = slash_root(&staged);
    let handoff = download.replacement.clone().expect("a handoff");
    assert_eq!((handoff.before_reveal)(staged.clone()).await, None);
    let target = handoff.target_for(".mp3").expect("a free name");
    std::fs::rename(&staged, &target).expect("moved in");
    handoff.set_revealed_path(target.clone());
    if let Some(revealed) = &handoff.on_revealed {
        revealed(&target);
    }
    queue.release(&download);
    download.completion.try_set_result(&target);
    tokio::time::timeout(Duration::from_secs(10), tick)
        .await
        .expect("the tick in time")
        .expect("no panic");

    // The right copy is at the song's path, and the original sits in the trash.
    assert_eq!(std::fs::read(&song).expect("the song"), right_bytes);
    let trashed = mp3s_under(&root.join(".octo-trash"));
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    assert_eq!(
        std::fs::read(&trashed[0]).expect("the trashed file"),
        original_bytes
    );
    let notes = notices.for_user("admin", NoticeKind::Review);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].state, NoticeState::Acted);
    assert!(notes[0].reason.contains("YouTube stream"), "{notes:?}");
    assert_eq!(worker.store().read(|s| s.replaced), 1);
    // Navidrome kept the song: the action's last word is in before Put back reads it.
    until(|| {
        journal
            .recent(10)
            .iter()
            .any(|entry| entry.history_kept == Some(true))
    })
    .await;

    let edits = LibraryEditService::new(
        resolver,
        Arc::new(LibraryActionQuarantine::new(settings.clone())),
        journal,
        Arc::new(TagEditJournal::new(None)),
        Arc::new(LibraryRescan::new(None)),
        settings.clone(),
        LibraryEditServices {
            notices: Some(notices.clone()),
            ..Default::default()
        },
    );
    let put_back = edits.restore("nd-1", "admin").await;

    assert_eq!(put_back.state, "applied", "{put_back:?}");
    assert_eq!(std::fs::read(&song).expect("the song"), original_bytes);
    let note = notices
        .for_user("admin", NoticeKind::Review)
        .into_iter()
        .next()
        .expect("the note");
    assert_eq!(note.reason, NoticeQueue::PUT_BACK_REASON);
    assert_eq!(note.state, NoticeState::Acted);

    // Started over, the sweep still leaves the put-back original alone.
    worker.reset();
    worker.tick().await;
    worker.tick().await;
    assert_eq!(verifier.verified.load(Ordering::SeqCst), 1);
    assert!(queue.is_idle());
    assert_eq!(std::fs::read(&song).expect("the song"), original_bytes);
}

/// With "Re-check Octo's downloads" on, a file kept as heard whose YouTube preview measures 0.5
/// is left alone: the sweep never checks it, and the re-check of Octo's downloads never takes it.
/// Octo's other downloads are checked as before.
#[tokio::test]
async fn a_kept_as_heard_file_is_left_alone_when_the_sweep_checks_octos_downloads() {
    for kept in [false, true] {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("the music root");
        let song = root.join(FILE_NAME);
        teardrop_mp3(&song, 20);
        let settings = store(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                dry_run: false,
                review_enabled: true,
                review_sweep_per_hour: 3600,
                review_sweep_auto_replace_preview: true,
                review_sweep_octo_downloads: true,
                allowed_users: vec!["admin".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        let library = Arc::new(RecordingLibrary::default());
        library.mappings.lock().push(LocalSongMapping {
            external_provider: "soulseek".into(),
            external_id: "teardrop".into(),
            local_path: slash_root(&song),
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            proof_mark: kept.then(|| "as heard on YouTube".into()),
            heart: kept.then(|| crate::services::local::KeptHeart {
                users: vec!["admin".into()],
                as_heard: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let verifier = Arc::new(HeardAsAnother::default());
        let swept_root = slash_root(&root);
        let worker = Arc::new(LibraryReviewSweepWorker::new(
            LibraryReviewSweepParts {
                store: Arc::new(ReviewSweepStore::new(None)),
                notices: Arc::new(NoticeQueue::new()),
                verifier: verifier.clone(),
                activity: Arc::new(Idle),
                library: library.clone(),
                settings: settings.clone(),
                music_root: Box::new(move || swept_root.clone()),
            },
            Clock::system(),
        ));
        for _ in 0..3 {
            worker.tick().await;
        }

        let checked = verifier.verified.load(Ordering::SeqCst);
        let rechecked = worker.recheck_octo().await;
        if kept {
            assert_eq!(checked, 0, "the kept-as-heard file is never checked");
            assert_eq!(rechecked, 0, "nor taken by the re-check");
        } else {
            assert_eq!(checked, 1, "Octo's other download is checked");
            assert_eq!(rechecked, 1);
        }
    }
}

/// A person's `action` on Octo's kept-as-heard download of an outside song, the replacement
/// played as the download service plays it, then Put back. Answers the library's mappings after
/// the replacement and after Put back, and the song's path.
async fn replaced_then_put_back(
    action: LibraryAction,
) -> (Vec<LocalSongMapping>, Vec<LocalSongMapping>, String) {
    use crate::services::local::KeptHeart;
    use octo_core::fingerprint::heart_gate::{HeardRecord, HeardSource};

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("music");
    std::fs::create_dir_all(&root).expect("the music root");
    let song = root.join(FILE_NAME);
    teardrop_mp3(&song, 20);
    let original_bytes = std::fs::read(&song).expect("the original");
    let navidrome = navidrome(original_bytes.len() as i64).await;
    let settings = store(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run: false,
            keep_replaced_originals: true,
            allowed_users: vec!["admin".into()],
            actions: vec![LibraryActionDefinition {
                action,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            url: Some(navidrome.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("secret".into()),
            auto_detect_download_path: false,
            download_source: DownloadSource::Soulseek,
            ..Default::default()
        },
        soulseek: SoulseekSettings {
            enabled: true,
            base_url: Some("http://slskd.test".into()),
            ..Default::default()
        },
        ..Default::default()
    });
    let music_root = slash_root(&root);
    settings.set_raw("Library:DownloadPath", Some(&music_root));
    let song_path = slash_root(&song);
    let kept = LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: "teardrop".into(),
        local_path: song_path.clone(),
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        proof_mark: Some("as heard on YouTube".into()),
        heart: Some(KeptHeart {
            users: vec!["admin".into()],
            heard: Some(HeardRecord {
                heard_at: Utc::now(),
                source: HeardSource::Video {
                    id: "u7K72X4eo_s".into(),
                    size: Some(4_000_000),
                    content_type: "audio/mp4".into(),
                    itag: Some(140),
                },
                sample_id: None,
            }),
            as_heard: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let journal = Arc::new(LibraryActionJournal::new());
    let library = Arc::new(RecordingLibrary::default());
    library.mappings.lock().push(kept);
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let actions = executor(
        &settings,
        Extras {
            journal: Some(journal.clone()),
            library: Some(library.clone()),
            queue: Some(queue.clone()),
            ..Default::default()
        },
    );
    actions.configure(|seams| {
        seams.history_poll = Duration::from_millis(1);
        seams.history_attempts = 1;
        seams.shows_at = Some(Arc::new(|_, _| async { true }.boxed()));
        seams.force_scan = Some(Arc::new(|| async { true }.boxed()));
    });
    let http = crate::services::http_client_factory::default_client();
    let identity = NavidromeIdentityService::new(settings.clone(), http.clone());
    let resolver = Arc::new(NavidromeSongPathResolver::new(
        identity,
        library.clone(),
        http,
        settings.clone(),
    ));
    assert!(resolver.resolve("nd-1").await.is_some());

    let mut request = LibraryActionRequest::new(action, "nd-1", "admin");
    if action == LibraryAction::BetterQuality {
        request.anyway = Some(crate::services::common::download_picks::AnywaySong {
            title: "Teardrop".into(),
            album: Some("Mezzanine".into()),
            length: Some(330),
            retitle: false,
        });
    }
    let applying = actions.clone();
    let applied = tokio::spawn(async move { applying.apply(request).await });
    let never = CancellationToken::new();
    let download = tokio::time::timeout(Duration::from_secs(10), queue.dequeue(&never))
        .await
        .expect("a download in time")
        .expect("a download");
    let incoming = root.join(".octo-incoming");
    std::fs::create_dir_all(&incoming).expect("the incoming folder");
    let staged = incoming.join("replacement.mp3");
    teardrop_mp3(&staged, 30);
    let staged = slash_root(&staged);
    let handoff = download.replacement.clone().expect("a handoff");
    assert_eq!((handoff.before_reveal)(staged.clone()).await, None);
    let target = handoff.target_for(".mp3").expect("a free name");
    std::fs::rename(&staged, &target).expect("moved in");
    handoff.set_revealed_path(target.clone());
    if let Some(revealed) = &handoff.on_revealed {
        revealed(&target);
    }
    queue.release(&download);
    download.completion.try_set_result(&target);
    let outcome = tokio::time::timeout(Duration::from_secs(10), applied)
        .await
        .expect("applied in time")
        .expect("no panic")
        .expect("applied");
    assert_eq!(outcome.state, LibraryActionState::Applied, "{outcome:?}");
    let replaced = library.mappings.lock().clone();
    until(|| {
        journal
            .recent(10)
            .iter()
            .any(|entry| entry.history_kept == Some(true))
    })
    .await;

    let edits = LibraryEditService::new(
        resolver,
        Arc::new(LibraryActionQuarantine::new(settings.clone())),
        journal,
        Arc::new(TagEditJournal::new(None)),
        Arc::new(LibraryRescan::new(None)),
        settings.clone(),
        LibraryEditServices {
            library: Some(library.clone()),
            ..Default::default()
        },
    );
    let put_back = edits.restore("nd-1", "admin").await;
    assert_eq!(put_back.state, "applied", "{put_back:?}");
    assert_eq!(std::fs::read(&song).expect("the song"), original_bytes);
    let restored = library.mappings.lock().clone();
    (replaced, restored, song_path)
}

/// Better quality over Octo's kept-as-heard Teardrop, kept for anna, its heart checked against an
/// AcoustID that does not know it.
struct KeptRig {
    _dir: tempfile::TempDir,
    _servers: [MockServer; 2],
    song: PathBuf,
    song_path: String,
    original_bytes: Vec<u8>,
    r: octo_core::fingerprint::heart_gate::HeardRecording,
    library: Arc<RecordingLibrary>,
    queue: Arc<TrackAcquisitionQueue>,
    actions: Arc<LibraryActionExecutor>,
}

async fn kept_rig() -> KeptRig {
    use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
    use crate::services::library::upgrade_queue::KeptRechecks;
    use crate::services::local::KeptHeart;
    use octo_core::fingerprint::heart_gate::HeardRecording;

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("music");
    std::fs::create_dir_all(&root).expect("the music root");
    let song = root.join(FILE_NAME);
    teardrop_mp3(&song, 20);
    let original_bytes = std::fs::read(&song).expect("the original");
    let navidrome = navidrome(original_bytes.len() as i64).await;
    let acoust_id = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"status":"ok","results":[]}"#, "application/json"),
        )
        .mount(&acoust_id)
        .await;
    let settings = store(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run: false,
            keep_replaced_originals: true,
            allowed_users: vec!["admin".into()],
            actions: vec![LibraryActionDefinition {
                action: LibraryAction::BetterQuality,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            url: Some(navidrome.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("secret".into()),
            auto_detect_download_path: false,
            download_source: DownloadSource::Soulseek,
            ..Default::default()
        },
        soulseek: SoulseekSettings {
            enabled: true,
            base_url: Some("http://slskd.test".into()),
            ..Default::default()
        },
        ..Default::default()
    });
    settings.set_raw("Library:DownloadPath", Some(&slash_root(&root)));
    let song_path = slash_root(&song);
    let r = HeardRecording {
        ids: vec!["0a1b2c3d-0000-0000-0000-000000000001".into()],
        title: "Teardrop".into(),
        artist: "Massive Attack".into(),
        ..Default::default()
    };
    let library = Arc::new(RecordingLibrary::default());
    library.mappings.lock().push(LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: "teardrop".into(),
        local_path: song_path.clone(),
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        proof_mark: Some("as heard on YouTube".into()),
        heart: Some(KeptHeart {
            users: vec!["anna".into()],
            recording: Some(r.clone()),
            as_heard: true,
            print: Some("AQADtEqk".into()),
            seconds: Some(330),
            ..Default::default()
        }),
        ..Default::default()
    });
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let actions = executor(
        &settings,
        Extras {
            library: Some(library.clone()),
            queue: Some(queue.clone()),
            ..Default::default()
        },
    );
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(1), Duration::from_millis(1))
            .with_jitter(|| 1.0),
    );
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::with_base_url(
            limiter,
            url::Url::parse(&format!("{}/", acoust_id.uri())).expect("parses"),
        )),
        settings.clone(),
        Some("octo-key"),
    ));
    actions.use_kept_rechecks(Arc::new(KeptRechecks::new(
        library.clone(),
        client,
        settings.clone(),
        Arc::new(crate::services::soulseek::ExternalIdRegistry::in_memory()),
    )));
    let http = crate::services::http_client_factory::default_client();
    let resolver = NavidromeSongPathResolver::new(
        NavidromeIdentityService::new(settings.clone(), http.clone()),
        library.clone(),
        http,
        settings.clone(),
    );
    assert!(resolver.resolve("nd-1").await.is_some());
    KeptRig {
        _dir: dir,
        _servers: [navidrome, acoust_id],
        song,
        song_path,
        original_bytes,
        r,
        library,
        queue,
        actions,
    }
}

/// Better quality over a file kept as heard (the weekly upgrade's, the upgrade queue's and the
/// action's are all this one) is judged by the heart that kept it: AcoustID still does not know the
/// kept file, so only its own master may replace it, measured against the kept file itself. A copy
/// that gate refuses (an unprovable FLAC, then an other-mix FLAC) leaves the kept file and its
/// mapping where they were, both times.
#[tokio::test]
async fn better_quality_over_a_kept_as_heard_file_is_judged_by_its_heart_and_a_refused_copy_leaves_it() {
    use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardReading, HeardSource};

    let KeptRig {
        _dir,
        _servers,
        song,
        song_path,
        original_bytes,
        r,
        library,
        queue,
        actions,
    } = kept_rig().await;
    for offered in ["an unprovable FLAC", "an other-mix FLAC"] {
        let applying = actions.clone();
        let applied = tokio::spawn(async move {
            applying
                .apply(LibraryActionRequest::new(
                    LibraryAction::BetterQuality,
                    "nd-1",
                    "admin",
                ))
                .await
        });
        let never = CancellationToken::new();
        let download = tokio::time::timeout(Duration::from_secs(10), queue.dequeue(&never))
            .await
            .expect("a download in time")
            .expect("a download");
        assert_eq!(
            download.origin,
            Some(AcquisitionOrigin::Heart),
            "judged by the heart"
        );
        let gates = download.gates();
        assert_eq!(gates.len(), 1, "{gates:?}");
        assert_eq!(gates[0].user, "anna");
        assert_eq!(gates[0].r, Some(r.clone()));
        assert_eq!(gates[0].reading, HeardReading::OnlyHeardOrMaster);
        assert_eq!(
            gates[0].heard.as_ref().map(|heard| heard.source.clone()),
            Some(HeardSource::Library {
                path: song_path.clone()
            }),
            "measured against the kept file itself"
        );
        // The gate refuses what it was offered, as the download's check point does.
        queue.release(&download);
        download
            .completion
            .try_set_error(anyhow::anyhow!("{offered} is not the version you heard"));
        let outcome = tokio::time::timeout(Duration::from_secs(10), applied)
            .await
            .expect("ended in time")
            .expect("no panic")
            .expect("ran");
        assert_ne!(
            outcome.state,
            LibraryActionState::Applied,
            "{offered}: {outcome:?}"
        );
        assert_eq!(
            std::fs::read(&song).expect("the song"),
            original_bytes,
            "{offered}"
        );
        let mappings = library.mappings.lock().clone();
        assert_eq!(mappings.len(), 1, "{offered}: {mappings:?}");
        assert_eq!(mappings[0].local_path, song_path);
    }
}

/// An upgrade of a kept file never holds: a copy its heart's gate cannot judge now (AcoustID or
/// MusicBrainz not answering) is thrown away at once, nothing waits for AcoustID's turn, the kept
/// file stays with its mapping, and the action ends quietly, for the next check to try again.
#[tokio::test]
async fn an_upgrade_of_a_kept_file_that_meets_a_busy_lookup_stops_quietly_and_holds_nothing() {
    use crate::services::common::AcquisitionHeld;
    use octo_core::fingerprint::heart_gate::WaitingFor;

    for waiting_for in [WaitingFor::AcoustId, WaitingFor::MusicBrainz] {
        let rig = kept_rig().await;
        let waited = Arc::new(AtomicUsize::new(0));
        let counted = waited.clone();
        rig.queue
            .use_turns(Arc::new(move |_wake: Box<dyn FnOnce() + Send>| {
                counted.fetch_add(1, Ordering::SeqCst);
            }));
        let applying = rig.actions.clone();
        let applied = tokio::spawn(async move {
            applying
                .apply(LibraryActionRequest::new(
                    LibraryAction::BetterQuality,
                    "nd-1",
                    "admin",
                ))
                .await
        });
        let never = CancellationToken::new();
        let download = tokio::time::timeout(Duration::from_secs(10), rig.queue.dequeue(&never))
            .await
            .expect("a download in time")
            .expect("a download");
        // The check point held the copy aside, as it does for a heart.
        let held = rig.song.with_file_name("held.flac");
        std::fs::write(&held, b"a copy nobody could judge").expect("written");
        rig.queue.release(&download);
        download
            .completion
            .try_set_error(anyhow::Error::new(AcquisitionHeld {
                waiting_for,
                line: "Waiting for AcoustID".into(),
                files: vec![slash_root(&held)],
                lidarr: Vec::new(),
                users: Vec::new(),
            }));
        let outcome = tokio::time::timeout(Duration::from_secs(10), applied)
            .await
            .expect("ended at once, waiting for nothing")
            .expect("no panic")
            .expect("ran");

        assert_ne!(outcome.state, LibraryActionState::Applied, "{waiting_for:?}");
        assert_eq!(outcome.detail, None, "quietly: {outcome:?}");
        assert_eq!(waited.load(Ordering::SeqCst), 0, "no turn waited for");
        assert!(!held.exists(), "the copy is thrown away");
        assert_eq!(std::fs::read(&rig.song).expect("the song"), rig.original_bytes);
        let mappings = rig.library.mappings.lock().clone();
        assert_eq!(mappings.len(), 1, "{mappings:?}");
        assert!(mappings[0].heart.as_ref().is_some_and(|heart| heart.as_heard));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(rig.queue.is_idle(), "nothing queued again");

        // The next check's upgrade of the same file runs again: nothing counted it as done.
        let applying = rig.actions.clone();
        let again = tokio::spawn(async move {
            applying
                .apply(LibraryActionRequest::new(
                    LibraryAction::BetterQuality,
                    "nd-1",
                    "admin",
                ))
                .await
        });
        let download = tokio::time::timeout(Duration::from_secs(10), rig.queue.dequeue(&never))
            .await
            .expect("asked for again")
            .expect("a download");
        rig.queue.release(&download);
        download
            .completion
            .try_set_error(anyhow::anyhow!("nothing found"));
        tokio::time::timeout(Duration::from_secs(10), again)
            .await
            .expect("ended")
            .expect("no panic")
            .expect("ran");
    }
}

/// Wrong song on a kept-as-heard file, then Put back: while the replacement is in, the outside
/// song maps to nothing; once the original is back, its outside id maps to the restored file,
/// still kept as heard for the person who hearted it.
#[tokio::test]
async fn wrong_song_on_a_kept_as_heard_file_then_put_back_maps_its_outside_id_again() {
    let (replaced, restored, song) = replaced_then_put_back(LibraryAction::WrongSong).await;
    assert!(
        replaced.iter().all(|mapping| mapping.external_id != "teardrop"),
        "{replaced:?}"
    );
    let back: Vec<&LocalSongMapping> = restored
        .iter()
        .filter(|mapping| mapping.external_id == "teardrop")
        .collect();
    assert_eq!(back.len(), 1, "{restored:?}");
    assert!(same_file(&back[0].local_path, &song), "{restored:?}");
    let heart = back[0].heart.as_ref().expect("still kept as heard");
    assert!(heart.as_heard && heart.is_for("admin"));
    assert_eq!(back[0].proof_mark.as_deref(), Some("as heard on YouTube"));
}

/// The same for a plain Better quality replacement put back.
#[tokio::test]
async fn better_quality_on_a_kept_as_heard_file_then_put_back_maps_its_outside_id_again() {
    let (replaced, restored, song) = replaced_then_put_back(LibraryAction::BetterQuality).await;
    assert!(
        replaced.iter().all(|mapping| mapping.external_id != "teardrop"),
        "{replaced:?}"
    );
    let back: Vec<&LocalSongMapping> = restored
        .iter()
        .filter(|mapping| mapping.external_id == "teardrop")
        .collect();
    assert_eq!(back.len(), 1, "{restored:?}");
    assert!(same_file(&back[0].local_path, &song), "{restored:?}");
    assert!(back[0].heart.as_ref().is_some_and(|heart| heart.as_heard));
}

// ---- Wrong song and Wrong version, by the original's own sound ------------------------------

/// AcoustID's answer naming these recordings (id, title, artist, submissions, ISRC), every one at
/// the match score.
fn answer_naming(recordings: &[(&str, &str, &str, i64, &str)]) -> String {
    json!({"status": "ok", "results": [{
        "id": "fp", "score": 0.97,
        "recordings": recordings.iter().map(|(id, title, artist, sources, isrc)| json!({
            "id": id, "title": title, "artists": [{"name": artist}], "duration": 330,
            "sources": sources, "isrcs": if isrc.is_empty() { json!([]) } else { json!([isrc]) },
        })).collect::<Vec<_>>(),
    }]})
    .to_string()
}

/// The 2015 mix of a song AcoustID clusters with the 2009 one: the 2009 recording dominates, and
/// the 2015 one carries its own ISRC, so nothing says which version this is.
fn clustered() -> String {
    answer_naming(&[
        ("rec-2009", "Teardrop", "Massive Attack", 69, "GBAAA0900001"),
        ("rec-2015", "Teardrop", "Massive Attack", 31, "GBAAA1500001"),
    ])
}

/// One recording, clearly.
fn clear(id: &str, title: &str, artist: &str) -> String {
    answer_naming(&[(id, title, artist, 60, "GBAAA0000001")])
}

/// Teardrop's Wrong song and Wrong version, its own sound read by an AcoustID that answers
/// `answer`, its journal kept, and its file the silent frames of [`teardrop_mp3`] or, with
/// `audio`, that ffmpeg `lavfi` source as an MP3 (None where ffmpeg cannot make it).
struct WrongRig {
    _dir: tempfile::TempDir,
    _servers: [MockServer; 2],
    root: PathBuf,
    song: PathBuf,
    original_bytes: Vec<u8>,
    queue: Arc<TrackAcquisitionQueue>,
    journal: Arc<LibraryActionJournal>,
    actions: Arc<LibraryActionExecutor>,
}

async fn wrong_rig(answer: &str, audio: Option<&str>) -> Option<WrongRig> {
    use crate::services::fingerprint::heard_master_check::HeardMasterCheck;
    use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
    use crate::services::library::upgrade_queue::KeptRechecks;

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("music");
    std::fs::create_dir_all(&root).expect("the music root");
    let song = root.join(FILE_NAME);
    match audio {
        None => teardrop_mp3(&song, 20),
        Some(source) => {
            let made = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-f", "lavfi", "-i", source])
                .args(["-c:a", "libmp3lame", "-b:a", "128k"])
                .arg(&song)
                .status()
                .is_ok_and(|status| status.success());
            if !made {
                return None;
            }
            let mut file = TagFile::open(&song).expect("an MP3");
            file.set_title(Some("Teardrop"));
            file.set_album(Some("Mezzanine"));
            file.set_performers(&["Massive Attack".to_string()]);
            file.set_album_artists(&["Massive Attack".to_string()]);
            file.save().expect("saved");
        }
    }
    let original_bytes = std::fs::read(&song).expect("the original");
    let navidrome = navidrome(original_bytes.len() as i64).await;
    let acoust_id = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer.to_string(), "application/json"))
        .mount(&acoust_id)
        .await;
    let settings = store(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run: false,
            keep_replaced_originals: true,
            allowed_users: vec!["admin".into()],
            actions: [LibraryAction::WrongSong, LibraryAction::WrongVersion]
                .into_iter()
                .map(|action| LibraryActionDefinition {
                    action,
                    enabled: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            url: Some(navidrome.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("secret".into()),
            auto_detect_download_path: false,
            download_source: DownloadSource::Soulseek,
            ..Default::default()
        },
        soulseek: SoulseekSettings {
            enabled: true,
            base_url: Some("http://slskd.test".into()),
            ..Default::default()
        },
        ..Default::default()
    });
    settings.set_raw("Library:DownloadPath", Some(&slash_root(&root)));
    let journal = Arc::new(LibraryActionJournal::new());
    let library = Arc::new(RecordingLibrary::default());
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let actions = executor(
        &settings,
        Extras {
            journal: Some(journal.clone()),
            library: Some(library.clone()),
            queue: Some(queue.clone()),
            ..Default::default()
        },
    );
    actions.configure(|seams| {
        seams.history_poll = Duration::from_millis(1);
        seams.history_attempts = 1;
        seams.shows_at = Some(Arc::new(|_, _| async { true }.boxed()));
        seams.force_scan = Some(Arc::new(|| async { true }.boxed()));
    });
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(1), Duration::from_millis(1))
            .with_jitter(|| 1.0),
    );
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::with_base_url(
            limiter,
            url::Url::parse(&format!("{}/", acoust_id.uri())).expect("parses"),
        )),
        settings.clone(),
        Some("octo-key"),
    ));
    // The test printer prints any file, so AcoustID's answer is the one the test set.
    let samples = crate::services::prefetch::heard_samples::tests::rig_in(
        &dir.path().join("samples"),
        "http://127.0.0.1:9",
        Arc::new(crate::services::common::SoulseekHoldStore::new(None::<PathBuf>)),
        16,
        64 << 20,
    )
    .samples;
    let rechecks = KeptRechecks::new(
        library.clone(),
        client,
        settings.clone(),
        Arc::new(crate::services::soulseek::ExternalIdRegistry::in_memory()),
    );
    rechecks.use_samples(samples.clone());
    actions.use_kept_rechecks(Arc::new(rechecks));
    actions.use_master(Arc::new(HeardMasterCheck::new(samples)));
    let http = crate::services::http_client_factory::default_client();
    let resolver = NavidromeSongPathResolver::new(
        NavidromeIdentityService::new(settings.clone(), http.clone()),
        library,
        http,
        settings.clone(),
    );
    assert!(resolver.resolve("nd-1").await.is_some());
    Some(WrongRig {
        _dir: dir,
        _servers: [navidrome, acoust_id],
        root,
        song,
        original_bytes,
        queue,
        journal,
        actions,
    })
}

impl WrongRig {
    /// Runs `request` until its download is asked for: the action, still running, and the
    /// download, or the action's outcome when it asked for none.
    async fn until_download(
        &self,
        request: LibraryActionRequest,
    ) -> Result<
        (
            tokio::task::JoinHandle<anyhow::Result<LibraryActionOutcome>>,
            Arc<crate::services::common::AcquisitionRequest>,
        ),
        LibraryActionOutcome,
    > {
        let applying = self.actions.clone();
        let mut applied = tokio::spawn(async move { applying.apply(request).await });
        let never = CancellationToken::new();
        let download = tokio::select! {
            download = tokio::time::timeout(Duration::from_secs(10), self.queue.dequeue(&never)) => {
                download.expect("a download in time").expect("a download")
            }
            outcome = &mut applied => return Err(outcome.expect("no panic").expect("ran")),
        };
        Ok((applied, download))
    }

    /// The download's reveal, as the download service plays it: the check, then the copy in
    /// `staged` into the song's place when the check passes it. What the check said.
    async fn reveal(
        &self,
        download: &Arc<crate::services::common::AcquisitionRequest>,
        staged: PathBuf,
    ) -> Option<String> {
        let staged = slash_root(&staged);
        let handoff = download.replacement.clone().expect("a handoff");
        let refused = (handoff.before_reveal)(staged.clone()).await;
        self.queue.release(download);
        match &refused {
            Some(problem) => {
                let _ = std::fs::remove_file(&staged);
                download.completion.try_set_error(anyhow::Error::new(
                    crate::services::library::replacement_handoff::ReplacementRejectedException::new(problem),
                ));
            }
            None => {
                let target = handoff.target_for(".mp3").expect("a free name");
                std::fs::rename(&staged, &target).expect("moved in");
                handoff.set_revealed_path(target.clone());
                if let Some(revealed) = &handoff.on_revealed {
                    revealed(&target);
                }
                download.completion.try_set_result(&target);
            }
        }
        refused
    }

    fn incoming(&self) -> PathBuf {
        let incoming = self.root.join(".octo-incoming");
        std::fs::create_dir_all(&incoming).expect("the incoming folder");
        incoming
    }
}

async fn ended(
    applied: tokio::task::JoinHandle<anyhow::Result<LibraryActionOutcome>>,
) -> LibraryActionOutcome {
    tokio::time::timeout(Duration::from_secs(20), applied)
        .await
        .expect("ended in time")
        .expect("no panic")
        .expect("ran")
}

/// A Wrong version of a 2015-mix original AcoustID clusters with the 2009 recording downloads
/// nothing: it answers `skipped` with the words that send the person to Find songs, and the song
/// carries the "wrong version asked" mark on the action's own record.
#[tokio::test]
async fn a_wrong_version_of_a_clustered_original_downloads_nothing_and_marks_the_song() {
    let rig = wrong_rig(&clustered(), None).await.expect("a rig");

    let outcome = rig
        .until_download(LibraryActionRequest::new(
            LibraryAction::WrongVersion,
            "nd-1",
            "admin",
        ))
        .await
        .err()
        .expect("no download");

    assert_eq!(outcome.state, LibraryActionState::Skipped);
    assert_eq!(
        outcome.detail.as_deref(),
        Some(LibraryActionExecutor::WRONG_VERSION_FIND_SONGS)
    );
    assert!(rig.queue.is_idle(), "nothing downloaded");
    assert_eq!(std::fs::read(&rig.song).expect("the song"), rig.original_bytes);
    assert!(
        rig.journal
            .wrong_version_asked("nd-1", LibraryActionExecutor::WRONG_VERSION_FIND_SONGS)
    );
    // An original whose fingerprint AcoustID links to no recording is unknown too, and so is one
    // AcoustID has no result for at all.
    for answer in [
        r#"{"status":"ok","results":[{"id":"fp","score":0.97}]}"#,
        r#"{"status":"ok","results":[]}"#,
    ] {
        let rig = wrong_rig(answer, None).await.expect("a rig");
        let outcome = rig
            .until_download(LibraryActionRequest::new(
                LibraryAction::WrongVersion,
                "nd-1",
                "admin",
            ))
            .await
            .err()
            .expect("no download");
        assert_eq!(
            outcome.detail.as_deref(),
            Some(LibraryActionExecutor::WRONG_VERSION_FIND_SONGS),
            "{answer}"
        );
        assert!(rig.queue.is_idle(), "{answer}: nothing downloaded");
    }
}

/// A Wrong version or a Wrong song of an original AcoustID reads clearly refuses a file whose
/// dominant recording is the original's again; a Wrong song whose original reads ambiguous
/// refuses any copy of that same song; both take any other.
#[tokio::test]
async fn a_clear_original_is_refused_again_and_an_ambiguous_wrong_songs_song_too() {
    let file = |id: &str, title: &str| {
        let doc: serde_json::Value =
            serde_json::from_str(&answer_naming(&[(id, title, "Massive Attack", 60, "")])).expect("json");
        octo_core::fingerprint::acoust_id_client::parse_answer(&doc)
            .expect("parses")
            .expect("an answer")
    };
    for action in [LibraryAction::WrongVersion, LibraryAction::WrongSong] {
        let rig = wrong_rig(&clear("rec-2015", "Teardrop", "Massive Attack"), None)
            .await
            .expect("a rig");
        let (applied, download) = rig
            .until_download(LibraryActionRequest::new(action, "nd-1", "admin"))
            .await
            .expect("a download");
        let deny = download.place().deny.expect("refused by its sound");
        assert!(
            deny.refuses(&file("rec-2015", "Teardrop"), 0.8).is_some(),
            "{action:?}"
        );
        assert!(
            deny.refuses(&file("rec-2009", "Teardrop"), 0.8).is_none(),
            "{action:?}"
        );
        rig.queue.release(&download);
        download
            .completion
            .try_set_error(anyhow::anyhow!("nothing found"));
        assert_ne!(ended(applied).await.state, LibraryActionState::Applied);
    }

    // The original of a Wrong song is another song entirely: AcoustID clusters its versions.
    let ambiguous = answer_naming(&[
        ("rec-angel", "Angel", "Massive Attack", 60, "GBAAA0000002"),
        (
            "rec-angel-live",
            "Angel (Live)",
            "Massive Attack",
            20,
            "GBAAA0000003",
        ),
    ]);
    let rig = wrong_rig(&ambiguous, None).await.expect("a rig");
    let (applied, download) = rig
        .until_download(LibraryActionRequest::new(
            LibraryAction::WrongSong,
            "nd-1",
            "admin",
        ))
        .await
        .expect("a download");
    let deny = download.place().deny.expect("refused by its song");
    assert!(
        deny.refuses(&file("rec-angel-remaster", "Angel - 2012 Remaster"), 0.8)
            .is_some()
    );
    assert!(deny.refuses(&file("rec-teardrop", "Teardrop"), 0.8).is_none());
    rig.queue.release(&download);
    download
        .completion
        .try_set_error(anyhow::anyhow!("nothing found"));
    ended(applied).await;
}

/// A Wrong song original AcoustID does not know, tagged as the wanted song: nothing of its sound
/// is denied, never by its own tags, so a right copy lands.
#[tokio::test]
async fn a_wrong_song_original_acoustid_does_not_know_lands_a_right_copy() {
    let rig = wrong_rig(r#"{"status":"ok","results":[]}"#, None)
        .await
        .expect("a rig");
    let (applied, download) = rig
        .until_download(LibraryActionRequest::new(
            LibraryAction::WrongSong,
            "nd-1",
            "admin",
        ))
        .await
        .expect("a download");
    assert_eq!(download.place().deny, None, "nothing of its sound to deny");
    let staged = rig.incoming().join("replacement.mp3");
    teardrop_mp3(&staged, 30);
    let right = std::fs::read(&staged).expect("the right copy");

    assert_eq!(rig.reveal(&download, staged).await, None);
    assert_eq!(ended(applied).await.state, LibraryActionState::Applied);
    assert_eq!(std::fs::read(&rig.song).expect("the song"), right);
}

/// With the song marked, a copy the person picks in Find songs replaces it as Wrong version does,
/// with no quality rule (an MP3 over the MP3 here, which Better quality would refuse), once it is
/// fetched and is not the very master they said is the wrong version; a pick of another copy of
/// that master is refused and thrown away with the words for it, and the original stays.
#[tokio::test]
async fn a_pick_for_a_marked_song_replaces_it_unless_it_is_the_rejected_master() {
    const NOISE: &str = "anoisesrc=color=pink:duration=40:sample_rate=44100:amplitude=0.3";
    const SWEEP: &str =
        "aevalsrc=0.4*sin(2*PI*(220+40*t)*t)|0.4*sin(2*PI*(330+25*t)*t):sample_rate=44100:duration=40";
    let Some(rig) = wrong_rig(&clustered(), Some(NOISE)).await else {
        eprintln!("skipped: ffmpeg could not make the audio");
        return;
    };
    let picked = || LibraryActionRequest {
        picked: true,
        only_source: Some(DownloadSource::Soulseek),
        ..LibraryActionRequest::new(LibraryAction::WrongVersion, "nd-1", "admin")
    };

    // Another copy of the rejected master.
    let (applied, download) = rig.until_download(picked()).await.expect("a download");
    let same = rig.incoming().join("same.mp3");
    std::fs::copy(&rig.song, &same).expect("copied");
    assert_eq!(
        rig.reveal(&download, same.clone()).await.as_deref(),
        Some(LibraryActionExecutor::SAME_AS_REJECTED)
    );
    let outcome = ended(applied).await;
    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert_eq!(
        outcome.detail.as_deref(),
        Some(LibraryActionExecutor::SAME_AS_REJECTED)
    );
    assert!(!same.exists(), "thrown away");
    assert_eq!(std::fs::read(&rig.song).expect("the song"), rig.original_bytes);

    // Another version.
    let (applied, download) = rig.until_download(picked()).await.expect("a download");
    let other = rig.incoming().join("other.mp3");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", SWEEP])
        .args(["-c:a", "libmp3lame", "-b:a", "128k"])
        .arg(&other)
        .status()
        .is_ok_and(|status| status.success());
    assert!(made, "ffmpeg made the other version");
    let other_bytes = std::fs::read(&other).expect("the other version");
    assert_eq!(rig.reveal(&download, other).await, None);
    assert_eq!(ended(applied).await.state, LibraryActionState::Applied);
    assert_eq!(std::fs::read(&rig.song).expect("the song"), other_bytes);
}

/// A sweep replacer whose song carries the "wrong version asked" mark when `marked` says so.
struct MarkedReplacer {
    marked: bool,
    replaced: AtomicUsize,
}

#[async_trait]
impl SweepReplacer for MarkedReplacer {
    async fn song_id(&self, _: &str, _: &str, _: &str) -> Option<String> {
        Some("nd-1".into())
    }

    async fn marked(&self, _: &str) -> Option<bool> {
        Some(false)
    }

    async fn replace(&self, _: LibraryActionRequest) -> anyhow::Result<LibraryActionOutcome> {
        self.replaced.fetch_add(1, Ordering::SeqCst);
        Ok(LibraryActionOutcome::new(LibraryActionState::Applied, None))
    }

    async fn wrong_version_asked(&self, song_id: &str) -> bool {
        self.marked && song_id == "nd-1"
    }
}

/// The Review sweep never replaces a song its person said is the wrong version, which Octo cannot
/// tell apart by sound: it leaves it in Review with the words that say to pick the copy in Find
/// songs. The same song unmarked is replaced as before.
#[tokio::test]
async fn the_sweep_leaves_a_song_marked_wrong_version_in_review_to_pick_in_find_songs() {
    for marked in [true, false] {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("the music root");
        teardrop_mp3(&root.join(FILE_NAME), 20);
        let settings = store(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                dry_run: false,
                review_enabled: true,
                review_sweep_per_hour: 3600,
                review_sweep_auto_replace_preview: true,
                keep_replaced_originals: true,
                allowed_users: vec!["admin".into()],
                actions: vec![LibraryActionDefinition {
                    action: LibraryAction::WrongSong,
                    enabled: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                download_source: DownloadSource::Soulseek,
                ..Default::default()
            },
            soulseek: SoulseekSettings {
                enabled: true,
                base_url: Some("http://slskd.test".into()),
                ..Default::default()
            },
            ..Default::default()
        });
        let notices = Arc::new(NoticeQueue::new());
        let replacer = Arc::new(MarkedReplacer {
            marked,
            replaced: AtomicUsize::new(0),
        });
        let swept_root = slash_root(&root);
        let worker = LibraryReviewSweepWorker::new(
            LibraryReviewSweepParts {
                store: Arc::new(ReviewSweepStore::new(None)),
                notices: notices.clone(),
                verifier: Arc::new(HeardAsAnother::default()),
                activity: Arc::new(Idle),
                library: Arc::new(RecordingLibrary::default()),
                settings,
                music_root: Box::new(move || swept_root.clone()),
            },
            Clock::system(),
        )
        .with_replacer(replacer.clone());
        for _ in 0..3 {
            worker.tick().await;
        }

        let notes = notices.for_user("admin", NoticeKind::Review);
        if marked {
            assert_eq!(replacer.replaced.load(Ordering::SeqCst), 0, "never replaced");
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert_eq!(notes[0].reason, PICK_IN_FIND_SONGS);
        } else {
            assert_eq!(replacer.replaced.load(Ordering::SeqCst), 1);
        }
    }
}

/// Whether two spellings of a path name one file: the trash gives a restored path back without
/// its drive on Windows, so the paths are compared from the music folder down.
fn same_file(a: &str, b: &str) -> bool {
    let tail = |path: &str| {
        let path = path.replace('\\', "/");
        path.find("/music/")
            .map_or(path.clone(), |at| path[at..].to_string())
    };
    tail(a) == tail(b)
}
