//! The Soulseek hold resumer, a worker of the supervisor.

use std::sync::Arc;

use octo_core::fingerprint::heart_gate::WaitingFor;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::{HeartAcquisitionCoordinator, HeldKind, SoulseekHoldStore};
use crate::services::prefetch::HeardSamples;

/// Picks up the held acquisitions a restart interrupted, and resumes a held one when what it
/// waits for arrives.
pub struct SoulseekHoldResumer {
    store: Arc<SoulseekHoldStore>,
    hearts: Arc<HeartAcquisitionCoordinator>,
    /// The heard samples: at start the holds take their copies again and the shim's holds wait
    /// for a round.
    heard: Option<Arc<HeardSamples>>,
}

impl SoulseekHoldResumer {
    pub fn new(store: Arc<SoulseekHoldStore>, hearts: Arc<HeartAcquisitionCoordinator>) -> Self {
        SoulseekHoldResumer {
            store,
            hearts,
            heard: None,
        }
    }

    /// The same resumer, with the heard samples' start work.
    pub fn with_heard(mut self, heard: Arc<HeardSamples>) -> Self {
        self.heard = Some(heard);
        self
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
        // Every live hold holds its heard copies again, and saved copy holds no hold names go;
        // a hold that waits on the shim joins the round queue instead of running now.
        if let Some(heard) = &self.heard {
            heard.rehold_copies();
            heard.rejoin_holds();
        }
        let held = self.store.snapshot();
        if !held.is_empty() {
            info!("Resuming {} held downloads after the restart", held.len());
        }
        for entry in held {
            match (entry.waiting_for, entry.kind) {
                (None, HeldKind::Album) => self.hearts.resume_album(entry),
                (None, HeldKind::Track) => self.hearts.resume_track(entry),
                (Some(WaitingFor::Shim), _) if self.heard.is_some() => self.hearts.hold_row_for(&entry),
                (Some(_), _) => {
                    self.hearts.hold_row_for(&entry);
                    self.hearts.resume_held(&entry.key());
                }
            }
        }
        // Octo's start is one of the shim's round events.
        if let Some(heard) = &self.heard {
            heard.shim_event();
        }
        Ok(())
    }
}
