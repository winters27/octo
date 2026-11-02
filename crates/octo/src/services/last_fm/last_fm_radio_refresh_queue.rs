//! The bounded in-process refresh queue. Duplicate user jobs collapse while queued.

use std::collections::HashSet;

use octo_core::common::dotnet;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// One refresh: a listener's whole station list, one pinned definition of it, or another try
/// at the stations the last refresh kept as they were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastFmRadioRefreshJob {
    pub username: String,
    pub station_definition_id: Option<String>,
    /// Only the stations the last refresh carried over, and nothing else.
    pub carried_only: bool,
}

impl LastFmRadioRefreshJob {
    pub fn new(username: impl Into<String>) -> Self {
        LastFmRadioRefreshJob {
            username: username.into(),
            station_definition_id: None,
            carried_only: false,
        }
    }

    fn key(&self) -> (bool, String) {
        (
            self.carried_only,
            queued_key(&self.username, self.station_definition_id.as_deref()),
        )
    }
}

/// A channel of 100 that drops a write when full, one reader, and the queued jobs' kinds and
/// keys, the keys compared ignoring case.
pub struct LastFmRadioRefreshQueue {
    sender: mpsc::Sender<LastFmRadioRefreshJob>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<LastFmRadioRefreshJob>>,
    queued: Mutex<HashSet<(bool, String)>>,
}

impl Default for LastFmRadioRefreshQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl LastFmRadioRefreshQueue {
    pub const CAPACITY: usize = 100;

    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel(Self::CAPACITY);
        LastFmRadioRefreshQueue {
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            queued: Mutex::new(HashSet::new()),
        }
    }

    /// Queues a refresh unless the same one is already waiting or the queue is full.
    pub fn enqueue(&self, username: &str, station_definition_id: Option<&str>) -> bool {
        self.push(LastFmRadioRefreshJob {
            username: username.trim().to_string(),
            station_definition_id: station_definition_id.map(str::to_string),
            carried_only: false,
        })
    }

    /// Queues another try at the listener's carried stations, unless one is already waiting or
    /// the queue is full.
    pub fn enqueue_carried(&self, username: &str) -> bool {
        self.push(LastFmRadioRefreshJob {
            username: username.trim().to_string(),
            station_definition_id: None,
            carried_only: true,
        })
    }

    fn push(&self, job: LastFmRadioRefreshJob) -> bool {
        let key = job.key();
        if job.username.is_empty() || !self.queued.lock().insert(key.clone()) {
            return false;
        }
        if self.sender.try_send(job).is_ok() {
            return true;
        }
        self.queued.lock().remove(&key);
        false
    }

    /// The next job, or None once `cancellation_token` is canceled.
    pub async fn dequeue(&self, cancellation_token: &CancellationToken) -> Option<LastFmRadioRefreshJob> {
        let mut receiver = self.receiver.lock().await;
        let job = tokio::select! {
            biased;
            () = cancellation_token.cancelled() => None,
            job = receiver.recv() => job,
        }?;
        self.queued.lock().remove(&job.key());
        Some(job)
    }
}

/// `username + "|" + stationDefinitionId`, compared ignoring case.
fn queued_key(username: &str, station_definition_id: Option<&str>) -> String {
    dotnet::ordinal_ignore_case_key(&format!(
        "{username}|{}",
        station_definition_id.unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queue_collapses_identical_jobs_but_keeps_independent_pinned_refreshes() {
        let queue = LastFmRadioRefreshQueue::new();
        let none = CancellationToken::new();
        assert!(queue.enqueue("alice", None));
        assert!(!queue.enqueue("ALICE", None));
        assert!(queue.enqueue("alice", Some("rock")));
        assert!(queue.enqueue("alice", Some("jazz")));
        assert_eq!(queue.dequeue(&none).await.unwrap().station_definition_id, None);
        assert_eq!(
            queue
                .dequeue(&none)
                .await
                .unwrap()
                .station_definition_id
                .as_deref(),
            Some("rock")
        );
        assert_eq!(
            queue
                .dequeue(&none)
                .await
                .unwrap()
                .station_definition_id
                .as_deref(),
            Some("jazz")
        );
    }

    /// A dequeued job can be queued again, a full queue drops the write, and a blank
    /// name is refused.
    #[tokio::test]
    async fn a_full_queue_drops_and_a_taken_job_can_come_back() {
        let queue = LastFmRadioRefreshQueue::new();
        let none = CancellationToken::new();
        assert!(!queue.enqueue("  ", None));
        for index in 0..LastFmRadioRefreshQueue::CAPACITY {
            assert!(queue.enqueue(&format!("user{index}"), None));
        }
        assert!(!queue.enqueue("one more", None));
        assert!(!queue.enqueue("one more", None));
        assert_eq!(queue.dequeue(&none).await.unwrap().username, "user0");
        assert!(queue.enqueue("user0", None));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(queue.dequeue(&cancelled).await.is_none());
    }
}
