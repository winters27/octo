//! Songs fetched ahead on the download path: one lands in the prefetch folder with its facts and
//! nothing else, and a heart afterwards (or one that was waiting on it) keeps that file, finalized
//! like a fresh download, without a second transfer.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octo_core::common::Clock;
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, SettingsStore, SoulseekSettings, SubsonicSettings};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::test_support::{Harness, build, catalog};
use super::*;
use crate::services::common::DownloadConcurrency;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::prefetch::{PrefetchStore, sidecar_path};

fn song(id: &str) -> Song {
    Song {
        title: format!("Song {id}"),
        artist: "Artist".into(),
        album: "Album".into(),
        duration: Some(1),
        external_provider: Some("test".into()),
        external_id: Some(id.into()),
        ..Default::default()
    }
}

struct Ahead {
    harness: Harness,
    store: Arc<PrefetchStore>,
    root: tempfile::TempDir,
    folder: tempfile::TempDir,
}

impl Ahead {
    fn new() -> Ahead {
        let root = tempfile::tempdir().expect("the music folder");
        let folder = tempfile::tempdir().expect("the prefetch folder");
        let in_folder = folder.path().to_path_buf();
        let store = Arc::new(PrefetchStore::new(
            Arc::new(move || in_folder.clone()),
            Clock::system(),
        ));
        let harness = build(
            root.path(),
            AppSettings::default(),
            None,
            Some(catalog(vec![("t1", song("t1"))])),
            DownloadServices {
                prefetched: Some(store.clone() as Arc<dyn PrefetchedFiles>),
                ..Default::default()
            },
        );
        // The transfer lands a real FLAC in a staging folder, as a Soulseek job would, and
        // says what its checks found.
        let staging = root.path().join(".octo-incoming").join("job");
        harness.backend.land(move |song, id| {
            std::fs::create_dir_all(&staging).expect("staging");
            let path = staging.join(format!("{id}.flac"));
            std::fs::write(&path, crate::services::test_support::flac()).expect("the file");
            song.source_peer = Some("peer-1".into());
            song.verification = Some(Box::new(octo_core::fingerprint::VerificationResult {
                reason: octo_core::fingerprint::InconclusiveReason::NoEntry,
                duration_seconds: 1,
                ..Default::default()
            }));
            path.to_string_lossy().into_owned()
        });
        Ahead {
            harness,
            store,
            root,
            folder,
        }
    }

    async fn ahead(&self, id: &str) -> crate::services::i_download_service::Ahead {
        self.harness
            .service
            .download_ahead("test", id, self.folder.path(), &CancellationToken::new())
            .await
            .expect("downloaded ahead")
    }

    async fn heart(&self, id: &str) -> String {
        heart(&self.harness.service, id).await
    }
}

async fn heart(service: &BaseDownloadService, id: &str) -> String {
    service
        .execute_acquisition(
            "test",
            id,
            false,
            true,
            None,
            &CancellationToken::new(),
            Some(vec!["alice".into()]),
            false,
            None,
        )
        .await
        .expect("hearted")
}

#[tokio::test]
async fn a_download_ahead_lands_in_the_folder_with_its_facts_and_nothing_else() {
    let w = Ahead::new();
    let crate::services::i_download_service::Ahead::Kept { path } = w.ahead("t1").await else {
        panic!("kept");
    };
    assert_eq!(path, w.folder.path().join("test_t1.flac"));
    assert!(path.is_file());
    let facts: crate::services::prefetch::AheadSidecar =
        serde_json::from_slice(&std::fs::read(sidecar_path(&path)).expect("the facts")).expect("JSON");
    assert_eq!(facts.external_id, "t1");
    assert_eq!(facts.format, "FLAC");
    assert_eq!(facts.source_peer.as_deref(), Some("peer-1"));
    assert_eq!(
        facts.verification.as_ref().map(|v| v.reason.as_str()),
        Some("NoEntry")
    );
    // No fetched-songs entry, no record left behind, and nothing in the music folder.
    assert!(w.harness.history.get_recent(10).is_empty());
    assert!(w.harness.service.active_download("ext-test-t1").is_none());
    assert!(!w.root.path().join("Artist").exists());
    assert_eq!(w.store.ready("test", "t1"), Some(path));
}

#[tokio::test]
async fn a_heart_afterwards_keeps_the_file_without_a_second_transfer() {
    let w = Ahead::new();
    w.ahead("t1").await;
    assert_eq!(w.harness.backend.transfers(), 1);

    let landed = w.heart("t1").await;
    assert_eq!(w.harness.backend.transfers(), 1, "no second download");
    assert!(Path::new(&landed).starts_with(w.root.path()), "{landed}");
    assert!(Path::new(&landed).is_file());
    assert!(!landed.contains(".octo-incoming"), "{landed}");
    // Gone from the folder, facts and all.
    assert!(w.store.ready("test", "t1").is_none());
    assert!(!w.folder.path().join("test_t1.flac").exists());
    assert!(!sidecar_path(&w.folder.path().join("test_t1.flac")).exists());
    // Finalized as a fresh download: in the fetched-songs log, for whoever hearted it.
    let history = w.harness.history.get_recent(10);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].path, landed);
    assert_eq!(history[0].format, "FLAC");
    assert_eq!(
        history[0].requested_by.as_deref(),
        Some(&["alice".to_string()][..])
    );
}

#[tokio::test]
async fn a_real_flac_heart_takes_the_file_too() {
    let w = Ahead::new();
    w.ahead("t1").await;
    let landed = w
        .harness
        .service
        .download_song_internal(
            "test",
            "t1",
            DownloadOptions {
                force_permanent: true,
                place: AcquisitionPlace {
                    pool_members: None,
                    flac_only: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            &CancellationToken::new(),
        )
        .await
        .expect("kept");
    assert_eq!(w.harness.backend.transfers(), 1);
    assert!(Path::new(&landed).starts_with(w.root.path()));
}

/// A transfer that waits until the test lets it finish, counting how many ran. With
/// `only_flac_only`, only a transfer that must be a real FLAC (a download ahead's) waits.
struct GatedBackend {
    root: std::path::PathBuf,
    open: Arc<Notify>,
    started: Arc<Notify>,
    transfers: Arc<AtomicUsize>,
    only_flac_only: bool,
}

#[async_trait]
impl DownloadBackend for GatedBackend {
    fn provider_name(&self) -> &str {
        "test"
    }

    async fn is_available(&self, _: &BaseDownloadService) -> bool {
        true
    }

    async fn download_track(
        &self,
        _: &BaseDownloadService,
        download: &mut TrackDownload,
        _: &CancellationToken,
    ) -> anyhow::Result<String> {
        self.transfers.fetch_add(1, Ordering::SeqCst);
        if download.flac_only || !self.only_flac_only {
            self.started.notify_one();
            self.open.notified().await;
        }
        let staging = self.root.join(".octo-incoming").join("gated");
        std::fs::create_dir_all(&staging)?;
        let path = staging.join(format!("{}.flac", download.track_id));
        std::fs::write(&path, crate::services::test_support::flac())?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn extract_external_id_from_album_id(&self, _: &str) -> Option<String> {
        None
    }
}

/// A service over a [`GatedBackend`], with songs t1 and t2 of album a1 and a prefetch folder.
struct Gated {
    service: Arc<BaseDownloadService>,
    backend: Arc<GatedBackend>,
    store: Arc<PrefetchStore>,
    history: Arc<DownloadHistoryService>,
    root: tempfile::TempDir,
    folder: tempfile::TempDir,
}

impl Gated {
    fn new(only_flac_only: bool) -> Gated {
        let root = tempfile::tempdir().expect("the music folder");
        let folder = tempfile::tempdir().expect("the prefetch folder");
        let in_folder = folder.path().to_path_buf();
        let store = Arc::new(PrefetchStore::new(
            Arc::new(move || in_folder.clone()),
            Clock::system(),
        ));
        // Two transfers may run at once, so a download waits on the running one rather than on
        // the download lock.
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            subsonic: SubsonicSettings {
                auto_detect_download_path: false,
                ..Default::default()
            },
            soulseek: SoulseekSettings {
                parallel_downloads: 2,
                ..Default::default()
            },
            ..Default::default()
        }));
        settings.set_raw("Library:DownloadPath", Some(&root.path().to_string_lossy()));
        let concurrency = Arc::new(DownloadConcurrency::new(Arc::clone(&settings)));
        concurrency.prove();
        let history = Arc::new(DownloadHistoryService::new(root.path().join("history.json")));
        let metadata = FakeMetadata::default();
        for id in ["t1", "t2"] {
            metadata.songs.lock().insert(("test".into(), id.into()), song(id));
        }
        metadata.albums.lock().insert(
            ("test".into(), "a1".into()),
            octo_core::models::domain::Album {
                id: "a1".into(),
                title: "Album".into(),
                artist: "Artist".into(),
                songs: vec![song("t1"), song("t2")],
                ..Default::default()
            },
        );
        let backend = Arc::new(GatedBackend {
            root: root.path().to_path_buf(),
            open: Arc::new(Notify::new()),
            started: Arc::new(Notify::new()),
            transfers: Arc::new(AtomicUsize::new(0)),
            only_flac_only,
        });
        let service = BaseDownloadService::new(
            DownloadCore {
                settings: Arc::clone(&settings),
                local_library: Arc::new(FakeLocalLibrary::default()),
                metadata: Arc::new(metadata),
                navidrome_identity: NavidromeIdentityService::new(
                    Arc::clone(&settings),
                    reqwest::Client::new(),
                ),
                history: Arc::clone(&history),
                notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
            },
            DownloadServices {
                concurrency: Some(concurrency),
                prefetched: Some(store.clone() as Arc<dyn PrefetchedFiles>),
                ..Default::default()
            },
            backend.clone() as Arc<dyn DownloadBackend>,
        );
        Gated {
            service,
            backend,
            store,
            history,
            root,
            folder,
        }
    }

    /// A download ahead of `id` on its own task, once its transfer has started.
    async fn ahead(
        &self,
        id: &'static str,
    ) -> tokio::task::JoinHandle<anyhow::Result<crate::services::i_download_service::Ahead>> {
        let started = self.backend.started.notified();
        let service = Arc::clone(&self.service);
        let folder = self.folder.path().to_path_buf();
        let ahead = tokio::spawn(async move {
            service
                .download_ahead("test", id, &folder, &CancellationToken::new())
                .await
        });
        started.await;
        ahead
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_heart_waiting_on_a_download_ahead_keeps_its_file_rather_than_its_path() {
    let gated = Gated::new(false);
    let Gated {
        service,
        backend,
        store,
        history,
        root,
        folder,
    } = &gated;
    let ahead = gated.ahead("t1").await;
    let hearted = {
        let service = Arc::clone(service);
        tokio::spawn(async move { heart(&service, "t1").await })
    };
    // The heart is waiting on the running download now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!hearted.is_finished());
    backend.open.notify_one();

    let ahead = tokio::time::timeout(Duration::from_secs(20), ahead)
        .await
        .expect("in time")
        .expect("ran")
        .expect("downloaded ahead");
    assert!(matches!(
        ahead,
        crate::services::i_download_service::Ahead::Kept { .. }
    ));
    let landed = tokio::time::timeout(Duration::from_secs(20), hearted)
        .await
        .expect("in time")
        .expect("ran");
    assert_eq!(backend.transfers.load(Ordering::SeqCst), 1, "no second download");
    assert!(Path::new(&landed).starts_with(root.path()), "{landed}");
    assert!(!Path::new(&landed).starts_with(folder.path()), "{landed}");
    assert!(Path::new(&landed).is_file());
    assert!(store.ready("test", "t1").is_none());
    assert_eq!(history.get_recent(10).len(), 1);
}

/// A hearted album's walk that reaches a song while a download ahead of it runs waits for that
/// download and keeps its file, so every song of the album ends up in the library.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_album_walk_keeps_the_song_a_download_ahead_is_fetching() {
    let gated = Gated::new(true);
    let ahead = gated.ahead("t2").await;
    let walked = {
        let service = Arc::clone(&gated.service);
        tokio::spawn(async move {
            service
                .download_remaining_album_tracks(
                    "a1",
                    "",
                    None,
                    false,
                    &CancellationToken::new(),
                    Some(vec!["alice".into()]),
                )
                .await
        })
    };
    // The walk has fetched t1 and come to t2 by now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    gated.backend.open.notify_one();

    tokio::time::timeout(Duration::from_secs(20), ahead)
        .await
        .expect("in time")
        .expect("ran")
        .expect("downloaded ahead");
    let walked = tokio::time::timeout(Duration::from_secs(20), walked)
        .await
        .expect("in time")
        .expect("ran")
        .expect("walked");
    assert!(walked);
    let mut kept: Vec<String> = gated
        .history
        .get_recent(10)
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    kept.sort();
    assert_eq!(kept.len(), 2, "both songs are in the library: {kept:?}");
    for path in &kept {
        assert!(Path::new(path).starts_with(gated.root.path()), "{path}");
        assert!(Path::new(path).is_file(), "{path}");
    }
    assert_eq!(
        gated.backend.transfers.load(Ordering::SeqCst),
        2,
        "t2 is fetched once"
    );
    assert!(gated.store.ready("test", "t2").is_none());
}

#[tokio::test]
async fn a_download_ahead_never_writes_into_a_row_that_has_ended() {
    let root = tempfile::tempdir().expect("the music folder");
    let folder = tempfile::tempdir().expect("the prefetch folder");
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let harness = build(
        root.path(),
        AppSettings::default(),
        None,
        Some(catalog(vec![("t1", song("t1"))])),
        DownloadServices {
            tracker: Some(tracker.clone()),
            ..Default::default()
        },
    );
    let staging = root.path().join(".octo-incoming").join("job");
    harness.backend.land(move |_, id| {
        std::fs::create_dir_all(&staging).expect("staging");
        let path = staging.join(format!("{id}.flac"));
        std::fs::write(&path, crate::services::test_support::flac()).expect("the file");
        path.to_string_lossy().into_owned()
    });
    // A heart on the song ran earlier and ended.
    tracker.begin("test", "t1", None, Some("alice"), None, None, None);
    tracker.complete("test", "t1", None);
    let lines = |tracker: &AcquisitionTracker| {
        tracker
            .detail("test:t1", None)
            .and_then(|row| row.events)
            .map_or(0, |events| events.len())
    };
    let before = lines(&tracker);
    harness
        .service
        .download_ahead("test", "t1", folder.path(), &CancellationToken::new())
        .await
        .expect("downloaded ahead");
    assert_eq!(lines(&tracker), before, "the ended row is as it was");

    // Once the download ahead is over, lines that come after a row's end land as before.
    tracker.log("test", "t1", "note", "Lyrics found", None);
    assert_eq!(lines(&tracker), before + 1);
}
