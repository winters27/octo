//! The acquisition worker, a worker of the supervisor.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use octo_core::fingerprint::heart_gate::AcquisitionOrigin;
use octo_core::notifications::notification_event::{NotificationEvent, NotificationEventType};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::acquisition_tracker::key_of;
use super::{AcquisitionPlace, AcquisitionRequest, DownloadConcurrency, TrackAcquisitionQueue};
use crate::services::family::FamilyPlacement;
use crate::services::i_download_service::IDownloadService;
use crate::services::library::ReplacementRejectedException;
use crate::services::notifications::NotificationService;
use crate::services::soulseek::ExternalIdRegistry;

/// Drains [`TrackAcquisitionQueue`].
///
/// One dispatcher. Up to `DownloadConcurrency.current()` requests run at once, and that is 1
/// until slskd has proven it puts each download in its own folder: before job folders,
/// ResolveLocalPath matched a finished file by leaf name with a 64KB size tolerance across the
/// whole music directory, so two transfers could claim and move each other's files. The
/// existence check and the in-progress marker stay together under DownloadLock, and so does
/// placing the file.
pub struct AcquisitionWorker {
    queue: Arc<TrackAcquisitionQueue>,
    downloads: Arc<dyn IDownloadService>,
    id_registry: Arc<ExternalIdRegistry>,
    notifications: Arc<NotificationService>,

    /// Optional: without it the worker runs one request at a time, as it always did.
    concurrency: Option<Arc<DownloadConcurrency>>,

    /// The family pool: a finished download is linked for every member who asked, however
    /// late they joined. Without it, or with Family off, nothing changes.
    family: Option<Arc<dyn FamilyPlacement>>,
}

impl AcquisitionWorker {
    pub fn new(
        queue: Arc<TrackAcquisitionQueue>,
        downloads: Arc<dyn IDownloadService>,
        id_registry: Arc<ExternalIdRegistry>,
        notifications: Arc<NotificationService>,
        concurrency: Option<Arc<DownloadConcurrency>>,
    ) -> Self {
        AcquisitionWorker {
            queue,
            downloads,
            id_registry,
            notifications,
            concurrency,
            family: None,
        }
    }

    /// The same worker, settling members' downloads in the family pool.
    pub fn with_family(mut self, family: Arc<dyn FamilyPlacement>) -> Self {
        self.family = Some(family);
        self
    }

    /// The worker's loop, until `stopping`.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        info!("Acquisition worker started");

        let mut running = JoinSet::new();
        while !stopping.is_cancelled() {
            while running.try_join_next().is_some() {}
            // Read every time: the setting is live, and the width stays 1 until job folders are proven.
            let width = self.concurrency.as_ref().map_or(1, |c| c.current()).max(1) as usize;
            while running.len() >= width {
                running.join_next().await;
            }

            let Some(request) = self.queue.dequeue(&stopping).await else {
                break;
            };

            // Off the loop, so the next request can start while this one transfers.
            let worker = Arc::clone(&self);
            running.spawn(async move { worker.run_one(request).await });
        }
        while running.join_next().await.is_some() {}

        info!("Acquisition worker stopped");
        Ok(())
    }

    async fn run_one(&self, request: Arc<AcquisitionRequest>) {
        // Per-item catch is mandatory: one failure must not stop the worker. A panic in the
        // download path is caught too, so the claim and the waiters are still settled.
        //
        // A fresh token, never the stopping token. The transfer must not be cancellable by
        // anything other than the process ending: that is the difference between "the client
        // left" and "the download is lost".
        // Where the file goes, read now for the same reason. A request nobody in the family
        // asked for, and every request with Family off, goes the way it always has.
        let place = request.place();
        let token = CancellationToken::new();
        let attempt = if place == AcquisitionPlace::default() {
            AssertUnwindSafe(self.downloads.execute_acquisition(
                &request.provider,
                &request.external_id,
                request.trigger_album_download,
                request.force_permanent,
                request.source_override,
                &token,
                // Read here rather than at enqueue time: a second user can join this
                // request right up until it is dequeued, and they asked for the file
                // just as much as whoever queued it.
                Some(request.requested_by()),
                request.upgrade_search(),
                request.replacement.clone(),
            ))
            .catch_unwind()
            .await
        } else {
            AssertUnwindSafe(self.downloads.execute_acquisition_to(
                &request.provider,
                &request.external_id,
                request.trigger_album_download,
                request.force_permanent,
                request.source_override,
                &token,
                Some(request.requested_by()),
                request.upgrade_search(),
                request.replacement.clone(),
                place,
            ))
            .catch_unwind()
            .await
        }
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the acquisition panicked")));

        // Members who joined while it ran get their link too, a file the shared library came to
        // want moves there, and a member who cannot see the shared library a file landed in
        // gets a pool copy of it.
        let attempt = match (attempt, &self.family) {
            (Ok(path), Some(family)) => {
                let members = request.pool_members();
                if members.is_empty() && !request.wants_shared() {
                    Ok(path)
                } else {
                    let key = key_of(&request.provider, &request.external_id);
                    Ok(family
                        .settle(&path, &members, request.wants_shared(), Some(&key))
                        .await)
                }
            }
            (attempt, _) => attempt,
        };

        match attempt {
            Ok(path) => {
                request.completion.try_set_result(path.clone());
                info!(
                    "Acquisition finished for {}:{} -> {path}",
                    request.provider, request.external_id
                );
            }
            Err(e) => {
                // Radio asks for every track it plays, so a play's copy failing is routine. A star, or a
                // play a heart joined, is someone's explicit ask.
                if e.downcast_ref::<ReplacementRejectedException>().is_some() {
                    info!(
                        "Replacement for {}:{} refused: {e}",
                        request.provider, request.external_id
                    );
                } else if request.is_star || request.heart_joined() {
                    error!(error = ?e, "Acquisition failed for {}:{}", request.provider, request.external_id);
                } else {
                    warn!(
                        "Play acquisition failed for {}:{}: {e}",
                        request.provider, request.external_id
                    );
                }
                // Stars only: a shed play-triggered acquisition is a hint, a failed
                // star is the user's explicit ask going unmet. This is the one place
                // a terminal failure surfaces exactly once per gesture (album-walk
                // per-track failures are caught inside the walk and aggregate into
                // its summary instead).
                if request.notifies_on_failure() {
                    self.notify_failed(&request, &e);
                }
                // Release before completing: an ordered heart fallback may
                // immediately enqueue the same track for its next source.
                self.queue.release(&request);
                request.completion.try_set_error(e);
            }
        }
        // Every terminal outcome, or the next request for this track joins a job
        // that has already finished and will never complete again.
        self.queue.release(&request);
    }

    /// Kept apart on purpose: a notification error must never disturb the bookkeeping around it
    /// (`notify` cannot fail). Metadata comes from the same routing pair the download path itself
    /// uses, so the names match what the user starred.
    fn notify_failed(&self, request: &AcquisitionRequest, error: &anyhow::Error) {
        // A heart's or a play's song is kept in the registry; any other id is read unsaved.
        let register = matches!(
            request.origin,
            Some(AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Play)
        );
        let routing = self
            .id_registry
            .resolve_song(&request.external_id, register)
            .map(|shared| shared.snapshot());
        let askers = request.requested_by();
        self.notifications.notify(NotificationEvent {
            artist: routing.as_ref().and_then(|r| r.artist.clone()),
            title: routing.as_ref().and_then(|r| r.title.clone()),
            album: routing.as_ref().and_then(|r| r.album.clone()),
            detail: Some(error.to_string()),
            // A failed star is the one notification where knowing whose it was
            // matters most, since nothing else will tell them.
            requested_by: (!askers.is_empty()).then_some(askers),
            ..NotificationEvent::new(NotificationEventType::DownloadFailed)
        });
    }
}

#[cfg(test)]
#[path = "acquisition_worker_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "acquisition_worker_family_tests.rs"]
mod family_tests;
