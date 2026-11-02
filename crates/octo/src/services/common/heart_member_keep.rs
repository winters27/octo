//! Lands a kept copy a Lidarr follow held in a family member's own library, when that member stops
//! waiting for Lidarr with Family on: the copy goes through the acquisition queue as a keep, into
//! their library, as Keep anyway lands a held file for one person.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardReading, HeartGate};
use octo_core::settings::DownloadSource;
use tracing::{info, warn};

use super::HeartAcquisitionCoordinator;
use crate::services::common::AcquisitionTarget;
use crate::services::lidarr::OwnLibraryKeeper;

/// The coordinator as the Lidarr hearts' keeper, held weakly: the coordinator holds the Lidarr
/// hearts service that holds this.
struct MemberKeeper(Weak<HeartAcquisitionCoordinator>);

impl HeartAcquisitionCoordinator {
    /// What lands a kept copy in a family member's own library for the Lidarr hearts.
    pub fn member_keeper(self: &Arc<Self>) -> Arc<dyn OwnLibraryKeeper> {
        Arc::new(MemberKeeper(Arc::downgrade(self)))
    }
}

#[async_trait]
impl OwnLibraryKeeper for MemberKeeper {
    fn own_library(&self, user: &str) -> bool {
        self.0.upgrade().is_some_and(|coordinator| {
            matches!(coordinator.import_target(user), Ok(AcquisitionTarget::Member(_)))
        })
    }

    async fn keep_for(&self, provider: &str, external_id: &str, file: &str, user: &str) -> bool {
        let Some(coordinator) = self.0.upgrade() else {
            return false;
        };
        let target = match coordinator.import_target(user) {
            Ok(target @ AcquisitionTarget::Member(_)) => target,
            _ => return false,
        };
        let gate = HeartGate {
            user: user.to_string(),
            origin: AcquisitionOrigin::Heart,
            r: None,
            heard: None,
            heard_seconds: None,
            reading: HeardReading::NotHeard,
            stand_in: None,
        };
        let (completion, joined) = coordinator.direct_queue.enqueue_keeping(
            AcquisitionOrigin::Heart,
            vec![gate],
            vec![file.to_string()],
            &target,
            provider,
            external_id,
            Some(DownloadSource::Soulseek),
            Some(user),
        );
        match completion.wait().await {
            // A download of the song already running took the request: the file was not kept.
            Ok(_) if joined => false,
            Ok(path) => {
                info!("The kept copy of {provider}:{external_id} landed in {user}'s own library: {path}");
                true
            }
            Err(failure) => {
                warn!(
                    "The kept copy of {provider}:{external_id} could not land in {user}'s own library: {failure}"
                );
                false
            }
        }
    }
}
