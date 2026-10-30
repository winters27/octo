//! The Soulseek hold resumer, a worker of the supervisor.

use std::sync::Arc;

use octo_core::fingerprint::heart_gate::WaitingFor;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::{HeartAcquisitionCoordinator, HeldKind, SoulseekHoldStore};

/// Picks up the held acquisitions a restart interrupted, and resumes a held one when what it
/// waits for arrives.
pub struct SoulseekHoldResumer {
    store: Arc<SoulseekHoldStore>,
    hearts: Arc<HeartAcquisitionCoordinator>,
}

impl SoulseekHoldResumer {
    pub fn new(store: Arc<SoulseekHoldStore>, hearts: Arc<HeartAcquisitionCoordinator>) -> Self {
        SoulseekHoldResumer { store, hearts }
    }

    /// Runs the held acquisition `key` again now: one acquisition, each other holder joining
    /// with their own gate.
    pub fn resume(self: &Arc<Self>, key: &str) {
        self.hearts.resume_held(key);
    }

    /// Runs again now every held acquisition that waits for `waiting_for`.
    pub fn resume_waiting(self: &Arc<Self>, waiting_for: WaitingFor) {
        for record in self
            .store
            .snapshot()
            .into_iter()
            .filter(|record| record.waiting_for == Some(waiting_for))
        {
            self.hearts.resume_held(&record.key());
        }
    }

    /// Starts every held chain again and finishes at once. A heart waiting out a Soulseek outage
    /// keeps its saved start, so its deadline stays; every other hold gets its downloads row back
    /// (held, with its line and its holders) and runs again.
    pub async fn run(self: Arc<Self>, _stopping: CancellationToken) -> anyhow::Result<()> {
        let held = self.store.snapshot();
        if !held.is_empty() {
            info!("Resuming {} held downloads after the restart", held.len());
        }
        for entry in held {
            match (entry.waiting_for, entry.kind) {
                (None, HeldKind::Album) => self.hearts.resume_album(entry),
                (None, HeldKind::Track) => self.hearts.resume_track(entry),
                (Some(_), _) => {
                    self.hearts.hold_row_for(&entry);
                    self.hearts.resume_held(&entry.key());
                }
            }
        }
        Ok(())
    }
}
