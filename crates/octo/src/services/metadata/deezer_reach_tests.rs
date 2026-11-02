//! Tests of Deezer's reach: every transition, on scripted `/infos` answers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use tokio::sync::Notify;

use super::*;
use crate::services::apple::{ApplePace, PaceConfig};

fn closed() -> Infos {
    Infos::Closed {
        country_iso: "IN".into(),
        country: "India".into(),
    }
}

fn open() -> Infos {
    Infos::Open {
        country_iso: "US".into(),
        country: "United States of America".into(),
    }
}

/// Answers `/infos` with what the test sets, counting the reads and their lanes; `hold` keeps a
/// read waiting.
struct Scripted {
    answer: parking_lot::Mutex<Infos>,
    reads: AtomicUsize,
    background_reads: AtomicUsize,
    hold: Option<Arc<Notify>>,
}

#[async_trait::async_trait]
impl InfosReader for Scripted {
    async fn read(&self, background: bool) -> Infos {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if background {
            self.background_reads.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(hold) = &self.hold {
            hold.notified().await;
        }
        self.answer.lock().clone()
    }
}

fn reach_with(answer: Infos, hold: Option<Arc<Notify>>) -> (Arc<DeezerReach>, Arc<Scripted>, Arc<ApplePace>) {
    let pace = Arc::new(ApplePace::new(PaceConfig::default()));
    let reach = Arc::new(DeezerReach::new(Arc::clone(&pace)));
    let reader = Arc::new(Scripted {
        answer: parking_lot::Mutex::new(answer),
        reads: AtomicUsize::new(0),
        background_reads: AtomicUsize::new(0),
        hold,
    });
    reach.set_reader(Arc::clone(&reader) as Arc<dyn InfosReader>);
    (reach, reader, pace)
}

#[test]
fn infos_reads_open_closed_and_anything_else_as_silent() {
    assert_eq!(
        Infos::read(&json!({"country_iso":"IN","country":"India","open":false})),
        closed()
    );
    assert!(matches!(
        Infos::read(&json!({"country_iso":"US","open":true})),
        Infos::Open { .. }
    ));
    assert_eq!(Infos::read(&json!({"country_iso":"US"})), Infos::Silent);
}

#[tokio::test]
async fn infos_closed_makes_the_reach_closed_and_the_pace_strict() {
    let (reach, _, pace) = reach_with(closed(), None);
    assert_eq!(reach.read_infos().await, closed());
    assert!(reach.stands_in());
    assert!(pace.strict());
    assert_eq!(reach.generation(), 0, "the first settled state is no switch");
}

#[tokio::test]
async fn a_switch_between_open_and_closed_bumps_the_generation_once() {
    let (reach, reader, pace) = reach_with(open(), None);
    let mut switches = reach.switches();
    reach.read_infos().await;
    assert_eq!(reach.generation(), 0);
    *reader.answer.lock() = closed();
    reach.read_infos().await;
    assert_eq!(reach.generation(), 1);
    assert!(switches.has_changed().unwrap());
    assert_eq!(*switches.borrow_and_update(), 1);
    // Closed to Unreachable is no switch: both stand in.
    for _ in 0..FAILURES_TO_UNREACHABLE {
        reach.observe(&Outcome::NetworkFailed("connect refused".into()));
    }
    assert!(matches!(reach.reach(), Reach::Unreachable { .. }));
    assert_eq!(reach.generation(), 1);
    assert!(pace.strict());
}

#[tokio::test]
async fn three_network_failures_in_a_row_make_deezer_unreachable() {
    let (reach, _, pace) = reach_with(open(), None);
    reach.read_infos().await;
    reach.observe(&Outcome::NetworkFailed("timed out".into()));
    reach.observe(&Outcome::NetworkFailed("timed out".into()));
    reach.observe(&Outcome::Answered(200));
    reach.observe(&Outcome::NetworkFailed("timed out".into()));
    reach.observe(&Outcome::NetworkFailed("timed out".into()));
    assert!(
        matches!(reach.reach(), Reach::Open { .. }),
        "an answer in between resets the count"
    );
    reach.observe(&Outcome::NetworkFailed("timed out".into()));
    assert!(matches!(reach.reach(), Reach::Unreachable { .. }));
    assert!(pace.strict());
    assert_eq!(reach.generation(), 1);
}

#[tokio::test]
async fn requests_that_never_left_say_nothing_about_deezer() {
    let (reach, _, _) = reach_with(open(), None);
    reach.read_infos().await;
    for _ in 0..10 {
        reach.observe(&Outcome::NotSent);
    }
    assert!(matches!(reach.reach(), Reach::Open { .. }));
}

#[tokio::test]
async fn any_answer_ends_unreachable_and_reads_infos_again() {
    let (reach, reader, pace) = reach_with(closed(), None);
    for _ in 0..FAILURES_TO_UNREACHABLE {
        reach.observe(&Outcome::NetworkFailed("dns".into()));
    }
    assert!(matches!(reach.reach(), Reach::Unreachable { .. }));
    reach.observe(&Outcome::Answered(503));
    assert_eq!(reach.reach(), Reach::Unknown);
    assert!(!pace.strict());
    // The read it started lands Closed again.
    for _ in 0..50 {
        if reach.stands_in() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(matches!(reach.reach(), Reach::Closed { .. }));
    assert_eq!(reader.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rows_from_a_catalog_search_mean_open() {
    let (reach, _, _) = reach_with(closed(), None);
    reach.read_infos().await;
    reach.observe_rows();
    assert!(matches!(reach.reach(), Reach::Open { .. }));
    assert_eq!(reach.generation(), 1);
}

#[tokio::test]
async fn reads_at_once_share_one_request() {
    let hold = Arc::new(Notify::new());
    let (reach, reader, _) = reach_with(closed(), Some(Arc::clone(&hold)));
    let first = tokio::spawn({
        let reach = Arc::clone(&reach);
        async move { reach.read_infos().await }
    });
    let second = tokio::spawn({
        let reach = Arc::clone(&reach);
        async move { reach.read_infos().await }
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    hold.notify_waiters();
    assert_eq!(first.await.unwrap(), closed());
    assert_eq!(second.await.unwrap(), closed());
    assert_eq!(reader.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_infos_answer_older_than_an_observation_is_dropped() {
    let hold = Arc::new(Notify::new());
    let (reach, _, _) = reach_with(closed(), Some(Arc::clone(&hold)));
    let read = tokio::spawn({
        let reach = Arc::clone(&reach);
        async move { reach.read_infos().await }
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    // While the read waits, a search answers rows: Deezer serves this server.
    reach.observe_rows();
    hold.notify_waiters();
    assert_eq!(read.await.unwrap(), closed());
    assert!(
        matches!(reach.reach(), Reach::Open { .. }),
        "the older closed answer is dropped"
    );
}

async fn settle(reader: &Scripted, reads: usize) {
    for _ in 0..50 {
        if reader.reads.load(Ordering::SeqCst) >= reads {
            break;
        }
        tokio::task::yield_now().await;
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn pages_answered_from_apple_reread_infos_after_a_change_then_every_twentieth() {
    let (reach, reader, _) = reach_with(closed(), None);
    reach.read_infos().await;
    assert_eq!(reader.reads.load(Ordering::SeqCst), 1);
    // Closed, and nothing changed since: no read until the twentieth page.
    for _ in 0..PAGES_PER_REREAD - 1 {
        reach.page_answered();
    }
    settle(&reader, 1).await;
    assert_eq!(reader.reads.load(Ordering::SeqCst), 1);
    reach.page_answered();
    settle(&reader, 2).await;
    assert_eq!(reader.reads.load(Ordering::SeqCst), 2);
    assert_eq!(
        reader.background_reads.load(Ordering::SeqCst),
        1,
        "a read nothing waits on is background"
    );
    // A change: the next page reads at once.
    reach.observe_rows();
    *reader.answer.lock() = closed();
    reach.page_answered();
    settle(&reader, 3).await;
    assert_eq!(reader.reads.load(Ordering::SeqCst), 3);
    reach.page_answered();
    settle(&reader, 3).await;
    assert_eq!(reader.reads.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_start_read_never_sent_leaves_a_read_needed_for_the_next_page() {
    let (reach, reader, _) = reach_with(Infos::NotSent, None);
    reach.read_at_start();
    settle(&reader, 1).await;
    assert_eq!(
        reader.background_reads.load(Ordering::SeqCst),
        0,
        "the start read is interactive"
    );
    *reader.answer.lock() = closed();
    reach.page_answered();
    settle(&reader, 2).await;
    assert_eq!(reader.reads.load(Ordering::SeqCst), 2);
    assert!(matches!(reach.reach(), Reach::Closed { .. }));
    // Read and answered: the next page is no reason to read.
    reach.page_answered();
    settle(&reader, 2).await;
    assert_eq!(reader.reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_read_someone_waits_on_never_joins_a_background_read() {
    let hold = Arc::new(Notify::new());
    let (reach, reader, _) = reach_with(closed(), Some(Arc::clone(&hold)));
    reach.start_read();
    settle(&reader, 1).await;
    let waited = tokio::spawn({
        let reach = Arc::clone(&reach);
        async move { reach.read_infos().await }
    });
    settle(&reader, 2).await;
    assert_eq!(
        reader.reads.load(Ordering::SeqCst),
        2,
        "an interactive read of its own"
    );
    // A background read joins the interactive one in flight.
    reach.start_read();
    settle(&reader, 3).await;
    assert_eq!(reader.reads.load(Ordering::SeqCst), 2);
    hold.notify_waiters();
    assert_eq!(waited.await.unwrap(), closed());
}

#[tokio::test]
async fn with_no_reader_infos_is_silent_and_nothing_changes() {
    let pace = Arc::new(ApplePace::new(PaceConfig::default()));
    let reach = Arc::new(DeezerReach::new(pace));
    assert_eq!(reach.read_infos().await, Infos::Silent);
    assert_eq!(reach.reach(), Reach::Unknown);
}
