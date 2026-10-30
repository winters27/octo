//! Lidarr's indexers. When every pick that searched for a song grabbed nothing and Lidarr is the
//! last source of its chain, Octo asks Lidarr to run its health checks, waits for that check on
//! its own state, and reads Lidarr's health: failing indexers hold the heart ("Waiting for Lidarr's
//! indexers"), no indexer that searches sends it on with its line, and anything else was a plain
//! miss. While a heart waits for the indexers, their health is read again on every call Octo makes
//! to Lidarr, on Lidarr's webhook saying its health came back or it grabbed, and at the import
//! poll pace; once they search again every heart waiting for them runs again. Hearting again and
//! a restart run a held heart again on their own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use octo_core::lidarr::lidarr_heart_acquisition_service::{IndexerHealth, indexer_health};
use parking_lot::Mutex;
use tokio::sync::Notify;
use tracing::{info, warn};

use super::{IndexersBack, Inner};

/// Whether anyone still waits for Lidarr's indexers.
pub type IndexersAwaited = Arc<dyn Fn() -> bool + Send + Sync>;

/// Lidarr's indexers, while a heart waits for them.
#[derive(Default)]
pub(super) struct IndexerWatch {
    /// A heart waits for the indexers.
    failing: AtomicBool,
    /// A watch reads Lidarr's health now.
    watching: AtomicBool,
    /// Something happened that may mean the indexers search again.
    nudged: Notify,
    /// Who runs the waiting hearts again, and who says whether any still waits.
    pub(super) back: Mutex<Option<(IndexersBack, Option<IndexersAwaited>)>>,
}

impl IndexerWatch {
    /// Something happened that may mean the indexers search again: a waiting watch reads now.
    pub(super) fn nudge(&self) {
        if self.failing.load(Ordering::SeqCst) {
            self.nudged.notify_one();
        }
    }
}

impl Inner {
    /// What Lidarr's health says of its indexers after every pick that searched grabbed nothing:
    /// Lidarr runs its health checks first, the check waited for on its own state at the import
    /// poll pace. A Lidarr that does not answer counts as healthy: the search simply missed.
    pub(super) async fn indexers_after_misses(&self) -> IndexerHealth {
        let poll = self.poll(&self.settings.current().lidarr);
        let command = match self.client.check_health().await {
            Ok(command) => command,
            Err(e) => {
                warn!("Lidarr would not check its health: {e}");
                return IndexerHealth::Healthy;
            }
        };
        loop {
            match self.client.command(command.id).await {
                Ok(Some(running)) if running.is_active() => tokio::time::sleep(poll).await,
                Ok(_) => break,
                Err(e) => {
                    warn!("Could not read Lidarr's health check: {e}");
                    break;
                }
            }
        }
        match self.client.health().await {
            Ok(checks) => {
                let health = indexer_health(&checks);
                info!("Lidarr's indexers after a search that grabbed nothing: {health:?}");
                health
            }
            Err(e) => {
                warn!("Could not read Lidarr's health: {e}");
                IndexerHealth::Healthy
            }
        }
    }

    /// A heart now waits for Lidarr's indexers: one watch reads their health until they search
    /// again, or nobody waits for them any more.
    pub(super) fn indexers_failing(self: &Arc<Self>) {
        self.indexers.failing.store(true, Ordering::SeqCst);
        if self.indexers.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.indexers.watching.store(false, Ordering::SeqCst);
            return;
        };
        runtime.spawn(self.clone().watch_indexers());
    }

    async fn watch_indexers(self: Arc<Self>) {
        loop {
            let poll = self.poll(&self.settings.current().lidarr);
            let ticked = tokio::select! {
                () = tokio::time::sleep(poll) => true,
                () = self.indexers.nudged.notified() => false,
                () = self.stopping.cancelled() => {
                    self.indexers.watching.store(false, Ordering::SeqCst);
                    return;
                }
            };
            // Whether anyone still waits is read at the poll pace: a heart told to wait holds a
            // moment after it is told.
            let told = self
                .busy
                .lock()
                .values()
                .any(|waits| *waits == super::LidarrWaits::Indexers);
            let awaited = self
                .indexers
                .back
                .lock()
                .as_ref()
                .and_then(|(_, awaited)| awaited.clone());
            if ticked && !told && awaited.is_some_and(|awaited| !awaited()) {
                info!("Nobody waits for Lidarr's indexers any more; Octo stops reading their health");
                self.indexers.failing.store(false, Ordering::SeqCst);
                self.indexers.watching.store(false, Ordering::SeqCst);
                return;
            }
            match self.client.health().await {
                Ok(checks) if indexer_health(&checks) != IndexerHealth::Failing => break,
                Ok(_) => {}
                Err(e) => warn!("Could not read Lidarr's health: {e}"),
            }
        }
        info!("Lidarr's indexers search again; every heart waiting for them runs again");
        self.indexers.failing.store(false, Ordering::SeqCst);
        self.indexers.watching.store(false, Ordering::SeqCst);
        let back = self.indexers.back.lock().as_ref().map(|(back, _)| back.clone());
        if let Some(back) = back {
            back();
        }
    }

    /// Lidarr answered a call of Octo's: news for a heart waiting on its indexers, unless it was
    /// the health read itself.
    pub(super) fn lidarr_answered(&self, path: &str) {
        if !path.trim_end_matches('/').ends_with("/api/v1/health") {
            self.indexers.nudge();
        }
    }
}
