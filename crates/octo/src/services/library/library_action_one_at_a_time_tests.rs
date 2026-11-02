//! A library replacement runs end to end here: a stand-in Navidrome (wiremock) resolves the song,
//! the real queue carries the request, and the test plays the download, so the swap's edges can
//! be timed. One action per song at a time, a joined download is never judged, the original must
//! be the file the action started with, and a replacement already in place is never undone.

use std::path::PathBuf;

use octo_core::settings::{
    AppSettings, LibraryActionDefinition, LibraryActionSettings, LidarrSettings, SoulseekSettings,
    SubsonicSettings,
};
use octo_media::tags::TagFile;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{any, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::test_fakes::{OfflineLink, until};
use crate::services::common::track_acquisition_queue::AcquisitionRequest;
use crate::services::library::library_action_test_support::{
    Extras, RecordingLibrary, executor, slash_root, store,
};
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::library::replacement_handoff::ArrivedTags;

const FILE_NAME: &str = "Massive Attack - Teardrop";
/// The Soulseek download service's `INCOMING_FOLDER_NAME`.
const INCOMING_FOLDER_NAME: &str = ".octo-incoming";

/// An MP3 of twenty silent MPEG-1 Layer III frames, 128 kbps, 44.1 kHz.
fn mp3() -> Vec<u8> {
    let mut bytes = vec![0u8; 417 * 20];
    for frame in 0..20 {
        bytes[frame * 417..frame * 417 + 4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    }
    bytes
}

/// A FLAC of one STREAMINFO block describing two seconds of 16-bit stereo, no frames.
fn flac() -> Vec<u8> {
    let mut bytes = b"fLaC".to_vec();
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let packed: u64 = (44100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 88200u64;
    bytes.extend_from_slice(&packed.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    original: String,
    original_size: i64,
    queue: Arc<TrackAcquisitionQueue>,
    ids: Arc<ExternalIdRegistry>,
    journal: Arc<LibraryActionJournal>,
    library: Arc<RecordingLibrary>,
    navidrome: MockServer,
}

impl Fixture {
    async fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("the music root");
        let original = root.join(format!("{FILE_NAME}.mp3"));
        std::fs::write(&original, mp3()).expect("written");
        {
            let mut file = TagFile::open(&original).expect("an MP3");
            file.set_title(Some("Teardrop"));
            file.set_album(Some("Mezzanine"));
            file.set_album_artists(&["Massive Attack".to_string()]);
            file.save().expect("saved");
        }
        let original_size = std::fs::metadata(&original).expect("there").len() as i64;

        // Navidrome's view of the song: the original, at the size it was scanned at.
        let navidrome = MockServer::start().await;
        Mock::given(path("/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"token":"jwt","isAdmin":true,"username":"admin"}"#,
                "application/json",
            ))
            .mount(&navidrome)
            .await;
        Mock::given(path("/api/song/nd-1"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!(
                    r#"{{"id":"nd-1","path":"{FILE_NAME}.mp3","size":{original_size},"title":"Teardrop","artist":"Massive Attack","album":"Mezzanine","suffix":"mp3","duration":330}}"#
                ),
                "application/json",
            ))
            .mount(&navidrome)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_raw("[]", "application/json"))
            .mount(&navidrome)
            .await;

        Fixture {
            original: slash_root(&original),
            root,
            _dir: dir,
            original_size,
            queue: Arc::new(TrackAcquisitionQueue::new()),
            ids: Arc::new(ExternalIdRegistry::new(None::<&Path>)),
            journal: Arc::new(LibraryActionJournal::new()),
            library: Arc::new(RecordingLibrary::default()),
            navidrome,
        }
    }

    fn revealed(&self) -> String {
        slash_root(&self.root.join(format!("{FILE_NAME}.flac")))
    }

    fn executor(
        &self,
        keep_originals: bool,
        soulseek_link: Option<Arc<dyn ISoulseekLink>>,
        sources: Option<Arc<UpgradeSources>>,
    ) -> Arc<LibraryActionExecutor> {
        let settings = store(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                dry_run: false,
                allowed_users: vec!["alice".into()],
                keep_replaced_originals: keep_originals,
                actions: vec![
                    LibraryActionDefinition {
                        action: LibraryAction::WrongVersion,
                        enabled: true,
                        ..Default::default()
                    },
                    LibraryActionDefinition {
                        action: LibraryAction::BetterQuality,
                        enabled: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                url: Some(self.navidrome.uri()),
                admin_username: Some("admin".into()),
                admin_password: Some("secret".into()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            ..Default::default()
        });
        settings.set_raw("Library:DownloadPath", Some(&slash_root(&self.root)));
        let executor = executor(
            &settings,
            Extras {
                journal: Some(self.journal.clone()),
                library: Some(self.library.clone()),
                queue: Some(self.queue.clone()),
                ids: Some(self.ids.clone()),
                soulseek_link,
                sources,
                ..Default::default()
            },
        );
        executor.configure(|seams| {
            seams.history_poll = Duration::from_millis(1);
            seams.history_attempts = 1;
            seams.shows_at = Some(Arc::new(|_, _| async { true }.boxed()));
            seams.force_scan = Some(Arc::new(|| async { true }.boxed()));
        });
        executor
    }

    fn plain_executor(&self) -> Arc<LibraryActionExecutor> {
        self.executor(false, None, None)
    }

    async fn next_download(&self) -> Arc<AcquisitionRequest> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.queue.dequeue(&CancellationToken::new()),
        )
        .await
        .expect("a download in time")
        .expect("a download")
    }

    fn staged(&self, bytes: &[u8], extension: &str) -> String {
        let incoming = self.root.join(INCOMING_FOLDER_NAME);
        std::fs::create_dir_all(&incoming).expect("the incoming folder");
        let staged = incoming.join(format!(
            "replacement-{}{extension}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&staged, bytes).expect("written");
        slash_root(&staged)
    }

    /// A lossless file larger than the original, as Better quality demands.
    fn lossless_staged(&self) -> String {
        self.staged(&vec![0u8; self.original_size as usize + 4096], ".flac")
    }

    fn finish(&self, download: &Arc<AcquisitionRequest>, path: &str) {
        self.queue.release(download);
        download.completion.try_set_result(path);
    }

    fn fail(&self, download: &Arc<AcquisitionRequest>, error: anyhow::Error) {
        self.queue.release(download);
        download.completion.try_set_error(error);
    }

    fn quarantined(&self) -> Vec<String> {
        let root = self
            .root
            .join(LibraryActionSettings::default().effective_quarantine_directory());
        let mut found = Vec::new();
        fn walk(dir: &Path, found: &mut Vec<String>) {
            let Ok(items) = std::fs::read_dir(dir) else {
                return;
            };
            for item in items.flatten() {
                let path = item.path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path.extension().is_some_and(|e| e == "mp3") {
                    found.push(slash_root(&path));
                }
            }
        }
        walk(&root, &mut found);
        found
    }

    /// The action's own entry, not a busy one.
    fn action(&self) -> LibraryActionEntry {
        let entries: Vec<_> = self
            .journal
            .recent(200)
            .into_iter()
            .filter(|entry| !entry.key.contains("busy:"))
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries.into_iter().next().expect("one")
    }

    fn forgotten(&self) -> Vec<String> {
        self.library.forgotten.lock().clone()
    }
}

/// What `BaseDownloadService::reveal_replacement` does with the handoff.
async fn reveal(download: &AcquisitionRequest, staged: &str) -> Result<String, ReplacementRejectedException> {
    let handoff = download.replacement.clone().expect("a handoff");
    if let Some(problem) = (handoff.before_reveal)(staged.to_string()).await {
        std::fs::remove_file(staged).expect("deleted");
        return Err(ReplacementRejectedException::new(problem));
    }
    let target = handoff.target_for(&extension(staged)).expect("a free name");
    std::fs::rename(staged, &target).expect("moved in");
    handoff.set_revealed_path(target.clone());
    if let Some(revealed) = &handoff.on_revealed {
        revealed(&target);
    }
    Ok(target)
}

fn wrong_version() -> LibraryActionRequest {
    LibraryActionRequest::new(LibraryAction::WrongVersion, "nd-1", "alice")
}

fn better_quality() -> LibraryActionRequest {
    LibraryActionRequest::new(LibraryAction::BetterQuality, "nd-1", "alice")
}

fn spawn_apply(
    executor: &Arc<LibraryActionExecutor>,
    request: LibraryActionRequest,
) -> tokio::task::JoinHandle<LibraryActionOutcome> {
    let executor = executor.clone();
    tokio::spawn(async move { executor.apply(request).await.expect("an outcome") })
}

async fn outcome_of(action: tokio::task::JoinHandle<LibraryActionOutcome>) -> LibraryActionOutcome {
    tokio::time::timeout(Duration::from_secs(10), action)
        .await
        .expect("an outcome in time")
        .expect("no panic")
}

fn not_found(message: &str) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        message.to_string(),
    ))
}

#[tokio::test]
async fn better_quality_during_a_soulseek_outage_touches_nothing_and_stays_asked() {
    let f = Fixture::new().await;
    let executor = f.executor(false, Some(Arc::new(OfflineLink)), None);

    let outcome = executor.apply(better_quality()).await.expect("an outcome");

    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert_eq!(
        outcome.code.as_deref(),
        Some(LibraryActionCodes::SOULSEEK_OFFLINE)
    );
    assert_eq!(outcome.detail.as_deref(), Some(OFFLINE_TEXT));
    assert!(!outcome.consumed());
    assert!(f.queue.is_idle());
    assert!(Path::new(&f.original).exists());
    assert_eq!(
        std::fs::metadata(&f.original).expect("there").len() as i64,
        f.original_size
    );
    assert!(f.journal.recent(10).is_empty());
}

#[tokio::test]
async fn wrong_song_honors_a_picked_source() {
    let f = Fixture::new().await;
    let executor = f.executor(false, Some(Arc::new(OfflineLink)), None);

    for action in [LibraryAction::WrongSong, LibraryAction::WrongVersion] {
        // Soulseek picked and out: nothing to try. Any other pick is the only source tried.
        assert!(
            executor
                .sources_for(action, Some(DownloadSource::Soulseek))
                .await
                .is_empty()
        );
        assert_eq!(
            executor.sources_for(action, Some(DownloadSource::Lidarr)).await,
            vec![Some(DownloadSource::Lidarr)]
        );
        assert_eq!(executor.sources_for(action, None).await, vec![None]);
    }

    // With nothing to try, the request waits as Better quality does: not consumed, nothing touched.
    let outcome = executor
        .apply(LibraryActionRequest {
            only_source: Some(DownloadSource::Soulseek),
            ..wrong_version()
        })
        .await
        .expect("an outcome");
    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert_eq!(
        outcome.code.as_deref(),
        Some(LibraryActionCodes::SOULSEEK_OFFLINE)
    );
    assert!(!outcome.consumed());
    assert!(f.queue.is_idle());
    assert!(Path::new(&f.original).exists());
    assert!(f.journal.recent(10).is_empty());
}

#[tokio::test]
async fn a_second_action_on_the_same_song_is_skipped_while_the_first_runs() {
    let f = Fixture::new().await;
    let executor = f.plain_executor();
    let first = spawn_apply(&executor, wrong_version());
    let download = f.next_download().await;

    // The rating worker, the playlist worker or the weekly upgrade asking at the same time.
    let second = tokio::time::timeout(Duration::from_secs(10), executor.apply(wrong_version()))
        .await
        .expect("in time")
        .expect("an outcome");

    assert_eq!(second.state, LibraryActionState::Skipped);
    assert_eq!(second.detail.as_deref(), Some(LibraryActionExecutor::BUSY_TEXT));
    assert!(Path::new(&f.original).exists());
    assert!(
        f.journal
            .recent(200)
            .iter()
            .any(|entry| entry.detail.as_deref() == Some(LibraryActionExecutor::BUSY_TEXT))
    );

    let placed = reveal(&download, &f.staged(&flac(), ".flac"))
        .await
        .expect("admitted");
    f.finish(&download, &placed);
    let outcome = outcome_of(first).await;

    assert_eq!(outcome.state, LibraryActionState::Applied);
    assert!(Path::new(&f.revealed()).exists());
    assert!(!Path::new(&f.original).exists());
    assert_eq!(f.action().state, LibraryActionState::Applied);
    assert_eq!(f.forgotten(), [get_full_path(&f.original)]);
}

/// The request joined a download already in flight, so its handoff was never used and the file
/// that came back belongs to whoever started it. Here that was another replacement, which took
/// the original out and put its own file in its place.
#[tokio::test]
async fn a_joined_download_leaves_the_file_it_returned_alone() {
    let f = Fixture::new().await;
    let external_id = f.ids.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Massive Attack".into()),
        title: Some("Teardrop".into()),
        album: Some("Mezzanine".into()),
        duration: Some(330),
        ..Default::default()
    });
    let _ = f.queue.enqueue(
        SoulseekMetadataService::PROVIDER_NAME,
        &external_id,
        true,
        false,
        true,
        None,
        true,
        None,
        false,
        None,
    );
    let download = f.next_download().await;
    assert!(download.replacement.is_none());

    let action = spawn_apply(&f.plain_executor(), wrong_version());
    until(|| download.requested_by().iter().any(|user| user == "alice")).await;

    // The other action's swap: the original out, its replacement in.
    std::fs::rename(&f.original, f.root.join("elsewhere.mp3.bak")).expect("moved out");
    std::fs::write(f.revealed(), flac()).expect("moved in");
    f.finish(&download, &f.revealed());
    let outcome = outcome_of(action).await;

    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert_eq!(
        outcome.detail.as_deref(),
        Some(LibraryActionExecutor::JOINED_TEXT)
    );
    assert!(Path::new(&f.revealed()).exists());
    assert!(f.quarantined().is_empty());
}

#[tokio::test]
async fn an_original_that_changed_during_the_download_is_not_swapped_out() {
    let f = Fixture::new().await;
    let action = spawn_apply(&f.plain_executor(), wrong_version());
    let download = f.next_download().await;

    // Rewritten while the replacement downloaded: same size, new modified time.
    std::fs::File::options()
        .write(true)
        .open(&f.original)
        .expect("open")
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(3600))
        .expect("touched");
    let staged = f.staged(&flac(), ".flac");
    let refused = reveal(&download, &staged).await.expect_err("refused");
    f.fail(&download, anyhow::Error::new(refused));
    let outcome = outcome_of(action).await;

    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert!(
        outcome
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("changed while it was downloading"),
        "{outcome:?}"
    );
    assert!(Path::new(&f.original).exists());
    assert!(!Path::new(&f.revealed()).exists());
    assert!(f.quarantined().is_empty());
}

/// A refused replacement leaves the original exactly as it was, mapping included, so who sent it
/// is still known and a favorite does not download it again.
#[tokio::test]
async fn a_refused_replacement_keeps_the_originals_mapping() {
    let f = Fixture::new().await;
    let action = spawn_apply(&f.plain_executor(), wrong_version());
    let download = f.next_download().await;

    let staged = f.staged(&[], ".flac");
    let refused = reveal(&download, &staged).await.expect_err("refused");
    f.fail(&download, anyhow::Error::new(refused));
    let outcome = outcome_of(action).await;

    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert!(Path::new(&f.original).exists());
    assert!(f.forgotten().is_empty());
}

/// A replacement runs as a replacement: work nobody waits on, whose file's check takes the bulk
/// turn. A file left waiting for that turn in the one hold path goes on with the same source once
/// AcoustID's lane gives turns again, and the replacement lands from it.
#[tokio::test]
async fn a_replacement_runs_as_one_and_its_file_waits_for_the_bulk_turn() {
    let f = Fixture::new().await;
    // The lane gives turns again at once here.
    f.queue
        .use_turns(Arc::new(|wake: Box<dyn FnOnce() + Send>| wake()));
    let action = spawn_apply(&f.plain_executor(), wrong_version());
    let download = f.next_download().await;
    assert_eq!(
        download.origin,
        Some(octo_core::fingerprint::heart_gate::AcquisitionOrigin::Replacement)
    );
    let staged = f.staged(&flac(), ".flac");
    f.fail(
        &download,
        anyhow::Error::new(crate::services::common::AcquisitionHeld {
            waiting_for: octo_core::fingerprint::heart_gate::WaitingFor::AcoustId,
            line: "Waiting for AcoustID".into(),
            files: vec![staged.clone()],
            users: Vec::new(),
        }),
    );

    let again = f.next_download().await;
    assert_eq!(again.place().held_files, std::slice::from_ref(&staged));
    assert!(again.replacement.is_some(), "still the same replacement");
    let landed = reveal(&again, &staged).await.expect("admitted");
    f.finish(&again, &landed);
    let outcome = outcome_of(action).await;
    assert_eq!(outcome.state, LibraryActionState::Applied);
    assert!(Path::new(&f.revealed()).exists());
}

/// The replacement moved in and then the bookkeeping after it failed (saving the mappings, say).
/// Restoring the original now would put a duplicate beside it, or be refused at a shared path;
/// the swap is done, so it counts as applied.
#[tokio::test]
async fn a_failure_after_the_replacement_moved_in_does_not_undo_it() {
    let f = Fixture::new().await;
    let action = spawn_apply(&f.executor(true, None, None), wrong_version());
    let download = f.next_download().await;

    reveal(&download, &f.staged(&flac(), ".flac"))
        .await
        .expect("admitted");
    assert_eq!(
        f.action().revealed_path.as_deref().map(get_full_path),
        Some(get_full_path(&f.revealed()))
    );
    f.fail(
        &download,
        anyhow::Error::new(std::io::Error::other("the mappings could not be saved")),
    );
    let outcome = outcome_of(action).await;

    assert_eq!(outcome.state, LibraryActionState::Applied);
    assert_eq!(outcome.detail, Some(format!("Replaced with {FILE_NAME}.flac.")));
    assert!(Path::new(&f.revealed()).exists());
    assert!(!Path::new(&f.original).exists());
    assert_eq!(f.quarantined().len(), 1);
}

fn slskd_set_up() -> SoulseekSettings {
    SoulseekSettings {
        base_url: Some("http://slskd:5030".into()),
        username: Some("u".into()),
        password: Some("p".into()),
        ..Default::default()
    }
}

fn lidarr_set_up() -> LidarrSettings {
    LidarrSettings {
        base_url: Some("http://lidarr:8686".into()),
        api_key: Some("k".into()),
        root_folder_path: Some("/music".into()),
        quality_profile_id: 1,
        metadata_profile_id: 1,
        ..Default::default()
    }
}

fn both_sources(link: Option<Arc<dyn ISoulseekLink>>) -> Option<Arc<UpgradeSources>> {
    Some(Arc::new(UpgradeSources::new(
        store(AppSettings {
            soulseek: slskd_set_up(),
            lidarr: lidarr_set_up(),
            ..Default::default()
        }),
        link,
    )))
}

#[tokio::test]
async fn better_quality_asks_lidarr_when_soulseek_finds_nothing() {
    let f = Fixture::new().await;
    let action = spawn_apply(&f.executor(false, None, both_sources(None)), better_quality());

    let soulseek = f.next_download().await;
    assert_eq!(soulseek.source_override, Some(DownloadSource::Soulseek));
    assert!(soulseek.upgrade_search());
    f.fail(&soulseek, not_found("no peer had it"));

    let lidarr = f.next_download().await;
    assert_eq!(lidarr.source_override, Some(DownloadSource::Lidarr));
    assert!(lidarr.upgrade_search());
    let placed = reveal(&lidarr, &f.lossless_staged()).await.expect("admitted");
    f.finish(&lidarr, &placed);
    let outcome = outcome_of(action).await;

    assert_eq!(outcome.state, LibraryActionState::Applied);
    assert!(Path::new(&f.revealed()).exists());
    assert!(!Path::new(&f.original).exists());
}

/// A wait read from Mornye's words beside a working read: it watches no check, and shares
/// Example's.
fn kept_by_mornye() -> anyhow::Error {
    let pause = octo_core::spotiflac::check::CheckPause {
        until_utc: chrono::Utc::now() + chrono::Duration::minutes(15),
        labels: Vec::new(),
        holding: vec!["Example".into()],
    };
    crate::services::spotiflac::MornyeWaiting::new(pause, false, chrono::Utc::now()).into_error()
}

#[tokio::test]
async fn better_quality_whose_copy_mornyes_check_kept_waits_for_mornye() {
    let f = Fixture::new().await;
    let action = spawn_apply(&f.plain_executor(), better_quality());

    let download = f.next_download().await;
    f.fail(&download, kept_by_mornye());
    let outcome = outcome_of(action).await;

    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert_eq!(outcome.code.as_deref(), Some(LibraryActionCodes::MORNYE_KEPT));
    assert!(
        outcome.detail.as_deref().unwrap_or_default().starts_with(
            "Waiting for Mornye's check, which it could not pass for Example; it allows its next try at "
        ),
        "{outcome:?}"
    );
    assert!(!outcome.consumed());
    assert!(Path::new(&f.original).exists());
}

#[tokio::test]
async fn a_mornye_wait_on_the_first_source_still_waits_when_lidarr_has_nothing_either() {
    let f = Fixture::new().await;
    let action = spawn_apply(&f.executor(false, None, both_sources(None)), better_quality());

    let soulseek = f.next_download().await;
    f.fail(&soulseek, kept_by_mornye());
    let lidarr = f.next_download().await;
    assert_eq!(lidarr.source_override, Some(DownloadSource::Lidarr));
    f.fail(&lidarr, not_found("Lidarr found no release"));
    let outcome = outcome_of(action).await;

    assert_eq!(
        outcome.code.as_deref(),
        Some(LibraryActionCodes::MORNYE_KEPT),
        "{outcome:?}"
    );
    assert!(!outcome.consumed());
}

#[tokio::test]
async fn better_quality_waits_for_mornyes_check_when_every_source_is_out() {
    // slskd is logged out and Mornye's check keeps it from every extension.
    let mornye = MockServer::builder().start().await;
    for (at, body) in [
        (
            "/api/get_signed_sessions",
            serde_json::json!({"result": {"sessions": [
            {"key": "gateway-v2-abc-6836abb49bb0e8c9", "kind": "gateway", "label": "Example",
             "extensions": ["example-ext"], "state": "unverified", "auth_paused_s": 900.0}]}}),
        ),
        (
            "/api/get_download_services",
            serde_json::json!({"result": {"services": [
            {"id": "ext:example-ext", "label": "Example", "extensions": ["example-ext"]}]}}),
        ),
    ] {
        Mock::given(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&mornye)
            .await;
    }
    let settings = store(AppSettings {
        soulseek: slskd_set_up(),
        spotiflac: octo_core::settings::SpotiflacSettings {
            enabled: true,
            url: mornye.uri(),
            ..Default::default()
        },
        ..Default::default()
    });
    let sources = UpgradeSources::new(Arc::clone(&settings), Some(Arc::new(OfflineLink))).with_spotiflac(
        Arc::new(crate::services::spotiflac::SpotiflacClient::new(settings)),
    );
    let f = Fixture::new().await;
    let executor = f.executor(false, Some(Arc::new(OfflineLink)), Some(Arc::new(sources)));

    let outcome = executor.apply(better_quality()).await.expect("an outcome");

    assert_eq!(
        outcome.code.as_deref(),
        Some(LibraryActionCodes::MORNYE_CHECK),
        "{outcome:?}"
    );
    assert!(
        outcome
            .detail
            .as_deref()
            .unwrap_or_default()
            .starts_with("Waiting for Mornye's check"),
        "{outcome:?}"
    );
    assert!(f.queue.is_idle(), "nothing was asked");
    assert!(Path::new(&f.original).exists());
}

#[tokio::test]
async fn better_quality_asks_lidarr_when_soulseeks_copy_fails_the_checks() {
    let f = Fixture::new().await;
    let action = spawn_apply(&f.executor(false, None, both_sources(None)), better_quality());

    let soulseek = f.next_download().await;
    // An MP3 renamed: no larger than the original, so refused before it was ever placed.
    let staged = f.staged(&[0u8; 10], ".flac");
    let refused = reveal(&soulseek, &staged).await.expect_err("refused");
    f.fail(&soulseek, anyhow::Error::new(refused));

    let lidarr = f.next_download().await;
    assert_eq!(lidarr.source_override, Some(DownloadSource::Lidarr));
    let placed = reveal(&lidarr, &f.lossless_staged()).await.expect("admitted");
    f.finish(&lidarr, &placed);

    assert_eq!(outcome_of(action).await.state, LibraryActionState::Applied);
    assert!(!Path::new(&f.original).exists());
}

#[tokio::test]
async fn better_quality_during_a_soulseek_outage_goes_straight_to_lidarr() {
    let f = Fixture::new().await;
    let action = spawn_apply(
        &f.executor(
            false,
            Some(Arc::new(OfflineLink)),
            both_sources(Some(Arc::new(OfflineLink))),
        ),
        better_quality(),
    );

    let download = f.next_download().await;
    assert_eq!(download.source_override, Some(DownloadSource::Lidarr));
    let placed = reveal(&download, &f.lossless_staged()).await.expect("admitted");
    f.finish(&download, &placed);

    assert_eq!(outcome_of(action).await.state, LibraryActionState::Applied);
}

#[tokio::test]
async fn better_quality_when_every_source_misses_says_what_each_found() {
    let f = Fixture::new().await;
    let action = spawn_apply(&f.executor(false, None, both_sources(None)), better_quality());

    let soulseek = f.next_download().await;
    f.fail(&soulseek, not_found("no peer had it"));
    let lidarr = f.next_download().await;
    f.fail(
        &lidarr,
        not_found("Lidarr found no lossless copy within 30 minutes."),
    );
    let outcome = outcome_of(action).await;

    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert_eq!(outcome.code.as_deref(), Some(LibraryActionCodes::NO_REPLACEMENT));
    let detail = outcome.detail.unwrap_or_default();
    assert!(detail.contains("Lidarr found no lossless copy"), "{detail}");
    assert!(
        detail.contains("Before that, Soulseek: no peer had it"),
        "{detail}"
    );
    assert!(Path::new(&f.original).exists());
    assert!(f.quarantined().is_empty());
}

#[tokio::test]
async fn better_quality_with_no_source_set_up_fails_without_downloading() {
    let f = Fixture::new().await;
    let none = Some(Arc::new(UpgradeSources::new(store(AppSettings::default()), None)));

    let outcome = f
        .executor(false, None, none)
        .apply(better_quality())
        .await
        .expect("an outcome");

    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert!(outcome.detail.unwrap_or_default().contains("no source set up"));
    assert!(f.queue.is_idle());
    assert!(Path::new(&f.original).exists());
}

// ---- Replaced anyway ----------------------------------------------------------------------

fn replaced_anyway(song: AnywaySong) -> LibraryActionRequest {
    LibraryActionRequest {
        only_source: Some(DownloadSource::Soulseek),
        anyway: Some(song),
        ..better_quality()
    }
}

fn plain_teardrop(length: i32) -> AnywaySong {
    AnywaySong {
        title: "Teardrop".into(),
        album: None,
        length: Some(length),
        retitle: true,
    }
}

#[tokio::test]
async fn a_copy_replaced_anyway_goes_in_though_it_is_no_better_and_the_original_is_kept() {
    let f = Fixture::new().await;
    let action = spawn_apply(
        &f.executor(true, None, None),
        replaced_anyway(AnywaySong {
            title: "Teardrop".into(),
            album: Some("Mezzanine".into()),
            length: Some(330),
            retitle: false,
        }),
    );

    let download = f.next_download().await;
    assert_eq!(download.source_override, Some(DownloadSource::Soulseek));
    // The same MP3 as the original: refused by Better quality, taken when the person said so.
    let staged = f.staged(&mp3(), ".mp3");
    let placed = reveal(&download, &staged)
        .await
        .expect("admitted though not better");
    f.finish(&download, &placed);
    let outcome = outcome_of(action).await;

    assert_eq!(outcome.state, LibraryActionState::Applied, "{outcome:?}");
    assert!(Path::new(&placed).exists());
    // The original waits in the trash folder, and the journal says where.
    let kept = f.quarantined();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(f.action().quarantine_path.as_deref(), Some(kept[0].as_str()));
    assert_eq!(outcome.quarantine_path.as_deref(), Some(kept[0].as_str()));
}

#[tokio::test]
async fn without_anyway_the_same_copy_is_refused_and_the_original_stays() {
    let f = Fixture::new().await;
    let action = spawn_apply(
        &f.executor(true, None, None),
        LibraryActionRequest {
            only_source: Some(DownloadSource::Soulseek),
            ..better_quality()
        },
    );

    let download = f.next_download().await;
    let staged = f.staged(&mp3(), ".mp3");
    let refused = reveal(&download, &staged).await.expect_err("refused");
    assert_eq!(refused.problem, "is not lossless");
    f.fail(&download, anyhow::Error::new(refused));

    assert_eq!(outcome_of(action).await.state, LibraryActionState::Failed);
    assert!(Path::new(&f.original).exists());
    assert!(f.quarantined().is_empty());
}

#[tokio::test]
async fn a_replacement_anyway_is_looked_for_and_checked_as_the_song_it_was_picked_as() {
    let f = Fixture::new().await;
    // The library's file is another take; the copy is held to the plain song's name and length.
    let action = spawn_apply(
        &f.executor(true, None, None),
        replaced_anyway(plain_teardrop(270)),
    );

    let download = f.next_download().await;
    let routing = f
        .ids
        .lookup(&download.external_id)
        .expect("the replacement's routing")
        .snapshot();
    assert_eq!(routing.title.as_deref(), Some("Teardrop"));
    assert_eq!(routing.album, None);
    assert_eq!(routing.duration, Some(270));
    assert_eq!(routing.artist.as_deref(), Some("Massive Attack"));
    f.fail(&download, not_found("no peer had it"));

    assert_eq!(outcome_of(action).await.state, LibraryActionState::Failed);
    assert!(Path::new(&f.original).exists());
}

/// An executor whose history check records the album the new file carried when Navidrome was
/// asked whether the song kept its place, and at which path it asked.
fn recording_history(
    f: &Fixture,
) -> (
    Arc<LibraryActionExecutor>,
    Arc<Mutex<Vec<(String, Option<String>)>>>,
) {
    let executor = f.executor(true, None, None);
    let asked: Arc<Mutex<Vec<(String, Option<String>)>>> = Arc::default();
    let heard = asked.clone();
    executor.configure(move |seams| {
        seams.shows_at = Some(Arc::new(move |_, path| {
            let album = TagFile::open(&path).ok().and_then(|file| file.album());
            heard.lock().push((path, album));
            async { true }.boxed()
        }));
    });
    (executor, asked)
}

async fn album_becomes(path: &str, album: &str) -> bool {
    for _ in 0..200 {
        if TagFile::open(path).ok().and_then(|file| file.album()).as_deref() == Some(album) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn another_take_replaced_anyway_takes_the_plain_songs_names_once_its_place_is_kept() {
    let f = Fixture::new().await;
    let (executor, asked) = recording_history(&f);
    let action = spawn_apply(&executor, replaced_anyway(plain_teardrop(330)));

    let download = f.next_download().await;
    // The download tagged its file as the plain song before the original's identity went on.
    download
        .replacement
        .as_ref()
        .expect("a handoff")
        .set_arrived(ArrivedTags {
            title: Some("Teardrop".into()),
            album: Some("Mezzanine (Studio)".into()),
        });
    let staged = f.staged(&mp3(), ".mp3");
    {
        let mut file = TagFile::open(&staged).expect("an MP3");
        file.set_title(Some("Teardrop (Live)"));
        file.set_album(Some("Live in Bristol"));
        file.save().expect("saved");
    }
    let placed = reveal(&download, &staged).await.expect("admitted");
    f.finish(&download, &placed);
    assert_eq!(outcome_of(action).await.state, LibraryActionState::Applied);

    assert!(
        album_becomes(&placed, "Mezzanine (Studio)").await,
        "the plain song's album"
    );
    let file = TagFile::open(&placed).expect("the new file");
    assert_eq!(file.title().as_deref(), Some("Teardrop"));
    // Navidrome was asked about this same path while the file still carried the identity it
    // arrived with, and the new names went onto that path in place, where a song keeps its id.
    let asked = asked.lock().clone();
    assert!(!asked.is_empty());
    assert!(asked.iter().all(|(path, _)| *path == placed), "{asked:?}");
    assert!(
        asked
            .iter()
            .all(|(_, album)| album.as_deref() != Some("Mezzanine (Studio)")),
        "{asked:?}"
    );
    assert_eq!(
        get_full_path(&placed),
        get_full_path(&f.original),
        "the original's own path"
    );
}

#[tokio::test]
async fn a_same_quality_copy_replaced_anyway_keeps_the_old_names() {
    let f = Fixture::new().await;
    let (executor, asked) = recording_history(&f);
    let action = spawn_apply(
        &executor,
        replaced_anyway(AnywaySong {
            title: "Teardrop".into(),
            album: Some("Mezzanine".into()),
            length: Some(330),
            retitle: false,
        }),
    );

    let download = f.next_download().await;
    download
        .replacement
        .as_ref()
        .expect("a handoff")
        .set_arrived(ArrivedTags {
            title: Some("Teardrop".into()),
            album: Some("Mezzanine (Studio)".into()),
        });
    let staged = f.staged(&mp3(), ".mp3");
    {
        let mut file = TagFile::open(&staged).expect("an MP3");
        file.set_album(Some("Mezzanine"));
        file.save().expect("saved");
    }
    let placed = reveal(&download, &staged).await.expect("admitted");
    f.finish(&download, &placed);
    assert_eq!(outcome_of(action).await.state, LibraryActionState::Applied);

    until(|| !asked.lock().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        TagFile::open(&placed)
            .ok()
            .and_then(|file| file.album())
            .as_deref(),
        Some("Mezzanine")
    );
}

// ---- Delete, dry run and the write-ahead rule --------------------------------------------

/// A delete moves the file to quarantine, writes the journal before and after, forgets the
/// mapping, and a second request for the same file content is "Already done.".
#[tokio::test]
async fn a_delete_quarantines_the_file_and_is_never_done_twice() {
    let f = Fixture::new().await;
    let executor = f.executor(false, None, None);
    let settings_with_delete = AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run: false,
            allowed_users: vec!["alice".into()],
            actions: vec![LibraryActionDefinition {
                action: LibraryAction::Delete,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    executor.settings.set(AppSettings {
        subsonic: executor.settings.current().subsonic.clone(),
        ..settings_with_delete
    });

    let request = LibraryActionRequest::new(LibraryAction::Delete, "nd-1", "alice");
    let outcome = executor.apply(request.clone()).await.expect("an outcome");

    assert_eq!(outcome.state, LibraryActionState::Applied, "{outcome:?}");
    assert!(!Path::new(&f.original).exists());
    assert_eq!(f.quarantined().len(), 1);
    assert_eq!(
        outcome.quarantine_path.as_deref(),
        Some(f.quarantined()[0].as_str())
    );
    assert_eq!(f.forgotten(), [get_full_path(&f.original)]);
    assert!(
        f.journal
            .is_never_requested(Some("Massive Attack"), Some("Teardrop"))
    );

    // Navidrome still points at the old file, which is gone: nothing more is done.
    let again = executor.apply(request).await.expect("an outcome");
    assert_eq!(again.state, LibraryActionState::Unresolved);
}

/// A dry run records a rehearsal and leaves the request where it is.
#[tokio::test]
async fn a_dry_run_rehearses_and_is_not_consumed() {
    let f = Fixture::new().await;
    let executor = f.plain_executor();
    let mut current = (*executor.settings.current()).clone();
    current.library_actions.dry_run = true;
    executor.settings.set(current);

    let outcome = executor.apply(wrong_version()).await.expect("an outcome");

    assert_eq!(outcome.state, LibraryActionState::Rehearsed);
    assert!(!outcome.consumed());
    assert!(
        outcome
            .detail
            .unwrap_or_default()
            .starts_with("Dry run: would replace (wrong version) ")
    );
    assert!(f.queue.is_idle());
    assert!(f.journal.recent(10)[0].dry_run);
}

/// The write-ahead rule: when the Pending entry cannot reach the disk, the file is not touched.
#[tokio::test]
async fn a_journal_that_cannot_be_written_stops_the_action_before_the_file_moves() {
    let mut f = Fixture::new().await;
    // A folder where the journal file should be: the rename over it fails.
    let blocked = f
        .root
        .parent()
        .expect("the temp dir")
        .join("library-actions.json");
    std::fs::create_dir_all(blocked.join("in-the-way")).expect("a folder");
    f.journal = Arc::new(LibraryActionJournal::with_path(Some(blocked)));

    let outcome = f
        .plain_executor()
        .apply(wrong_version())
        .await
        .expect("an outcome");

    assert_eq!(outcome.state, LibraryActionState::Failed);
    assert_eq!(
        outcome.detail.as_deref(),
        Some("Could not write the action journal, so the file was not touched.")
    );
    assert!(Path::new(&f.original).exists());
    assert!(f.queue.is_idle());
    assert_eq!(f.action().state, LibraryActionState::Failed);
}
