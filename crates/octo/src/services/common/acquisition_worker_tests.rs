//! Parallel downloads, the worker part: the worker runs several requests only once slskd has
//! proven it files each download in its own folder, and never more than the width.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::{AppSettings, DownloadSource, SettingsStore, SoulseekSettings};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::services::common::test_fakes::until;
use crate::services::i_download_service::{AudioStream, DirectStreamInfo};
use crate::services::library::ReplacementHandoff;

/// A stand-in `IDownloadService` whose acquisitions wait for one release, counting how many run.
struct HeldDownloads {
    inside: AtomicI32,
    most: AtomicI32,
    started: AtomicI32,
    release: watch::Receiver<bool>,
}

#[async_trait]
impl IDownloadService for HeldDownloads {
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
        self.started.fetch_add(1, Ordering::SeqCst);
        let now = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        let mut release = self.release.clone();
        let _ = release.wait_for(|released| *released).await;
        self.inside.fetch_sub(1, Ordering::SeqCst);
        Ok("/music/x.flac".to_string())
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
        Ok(None)
    }
}

struct Running {
    queue: Arc<TrackAcquisitionQueue>,
    downloads: Arc<HeldDownloads>,
    release: watch::Sender<bool>,
    stopping: CancellationToken,
    worker: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn start_worker(width: i32) -> Running {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let (release, released) = watch::channel(false);
    let downloads = Arc::new(HeldDownloads {
        inside: AtomicI32::new(0),
        most: AtomicI32::new(0),
        started: AtomicI32::new(0),
        release: released,
    });
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: SoulseekSettings {
            parallel_downloads: width,
            ..Default::default()
        },
        ..Default::default()
    }));
    let concurrency = Arc::new(DownloadConcurrency::new(Arc::clone(&settings)));
    concurrency.prove();
    let worker = Arc::new(AcquisitionWorker::new(
        Arc::clone(&queue),
        Arc::clone(&downloads) as Arc<dyn IDownloadService>,
        Arc::new(ExternalIdRegistry::in_memory()),
        Arc::new(NotificationService::new(Vec::new(), settings, None)),
        Some(concurrency),
    ));
    let stopping = CancellationToken::new();
    let worker = tokio::spawn(worker.run(stopping.clone()));
    Running {
        queue,
        downloads,
        release,
        stopping,
        worker,
    }
}

async fn run(width: i32, requests: usize) {
    let running = start_worker(width);
    let done: Vec<_> = (0..requests)
        .map(|i| {
            running.queue.enqueue(
                "soulseek",
                &format!("song-{i}"),
                true,
                false,
                true,
                None,
                true,
                None,
                false,
                None,
            )
        })
        .collect();

    let downloads = Arc::clone(&running.downloads);
    until(move || downloads.started.load(Ordering::SeqCst) == width).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(running.downloads.started.load(Ordering::SeqCst), width);
    running.release.send_replace(true);
    let all = futures::future::join_all(done.iter().map(|completion| completion.wait()));
    for outcome in tokio::time::timeout(Duration::from_secs(10), all)
        .await
        .expect("every request finishes")
    {
        outcome.expect("the request succeeded");
    }
    assert_eq!(running.downloads.most.load(Ordering::SeqCst), width);
    running.stopping.cancel();
    running
        .worker
        .await
        .expect("the worker ran")
        .expect("the worker stopped cleanly");
}

#[tokio::test]
async fn the_worker_runs_up_to_the_width_and_the_rest_wait() {
    run(3, 5).await;
}

#[tokio::test]
async fn at_width_one_the_worker_is_strictly_one_at_a_time() {
    run(1, 3).await;
}

/// Downloads that fail: song "kept" because Mornye's check is paused, any other plainly.
struct FailingDownloads;

#[async_trait]
impl IDownloadService for FailingDownloads {
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
        external_id: &str,
        _: bool,
        _: bool,
        _: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        _: bool,
        _: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        if external_id == "kept" {
            let pause = octo_core::spotiflac::check::CheckPause {
                until_utc: chrono::Utc::now(),
                labels: vec!["Example".into()],
                holding: Vec::new(),
            };
            return Err(
                crate::services::spotiflac::MornyeWaiting::new(pause, false, chrono::Utc::now()).into_error(),
            );
        }
        anyhow::bail!("No Soulseek FLAC found")
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
        Ok(None)
    }
}

/// Every push sent, by its kind.
#[derive(Default)]
struct SentPushes(parking_lot::Mutex<Vec<NotificationEventType>>);

#[async_trait]
impl crate::services::notifications::INotificationSink for SentPushes {
    fn name(&self) -> &str {
        "recorded"
    }

    fn is_configured(&self) -> bool {
        true
    }

    async fn send(&self, message: &octo_core::notifications::NotificationMessage) -> anyhow::Result<()> {
        self.0.lock().push(message.r#type);
        Ok(())
    }
}

#[tokio::test]
async fn a_star_mornyes_check_kept_sends_no_failure_push_of_its_own() {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        notifications: octo_core::settings::NotificationSettings {
            notify_download_failed: true,
            ..Default::default()
        },
        ..Default::default()
    }));
    let sent = Arc::new(SentPushes::default());
    let worker = Arc::new(AcquisitionWorker::new(
        Arc::clone(&queue),
        Arc::new(FailingDownloads) as Arc<dyn IDownloadService>,
        Arc::new(ExternalIdRegistry::in_memory()),
        Arc::new(NotificationService::new(
            vec![Arc::clone(&sent) as Arc<dyn crate::services::notifications::INotificationSink>],
            settings,
            None,
        )),
        None,
    ));
    let stopping = CancellationToken::new();
    let running = tokio::spawn(worker.run(stopping.clone()));
    for id in ["kept", "missing"] {
        let done = queue.enqueue("soulseek", id, true, false, true, None, true, None, false, None);
        let outcome = tokio::time::timeout(Duration::from_secs(10), done.wait())
            .await
            .expect("the request finishes");
        assert!(outcome.is_err());
    }
    until({
        let sent = Arc::clone(&sent);
        move || !sent.0.lock().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // One push, the plain failure's: the check has its own push, once per stretch.
    assert_eq!(*sent.0.lock(), vec![NotificationEventType::DownloadFailed]);
    stopping.cancel();
    running
        .await
        .expect("the worker ran")
        .expect("the worker stopped cleanly");
}
