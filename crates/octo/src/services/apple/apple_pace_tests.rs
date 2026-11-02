//! Tests of the Apple pace, on paused time with no HTTP: each turn is a task that records when and
//! in which order it was let go.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::*;

fn quick(capacity: u32) -> PaceConfig {
    PaceConfig {
        spacing: Duration::from_millis(10),
        cover_spacing: Duration::ZERO,
        bucket_capacity: capacity,
        refill_every: Duration::from_secs(3),
        backoff_max: Duration::from_secs(8),
        answers_to_halve: 50,
        share_turns: 4,
        row_cap: 8,
        background_cap: 32,
    }
}

/// Starts a turn of `class` named `name` that reports `(name, ms since start)` when let go, or
/// `(name + " refused", ms)` when refused.
fn start(
    pace: &Arc<ApplePace>,
    class: AppleClass,
    name: &str,
    out: &mpsc::UnboundedSender<(String, u128)>,
    began: Instant,
) -> tokio::task::JoinHandle<()> {
    let (pace, name, out) = (Arc::clone(pace), name.to_string(), out.clone());
    tokio::spawn(async move {
        let label = match pace.turn(class).await {
            Ok(()) => name,
            Err(Refused) => format!("{name} refused"),
        };
        let _ = out.send((label, began.elapsed().as_millis()));
    })
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

async fn drain(rx: &mut mpsc::UnboundedReceiver<(String, u128)>, count: usize) -> Vec<(String, u128)> {
    let mut seen = Vec::new();
    while seen.len() < count {
        seen.push(rx.recv().await.expect("a turn reports"));
    }
    seen
}

#[tokio::test(start_paused = true)]
async fn a_page_goes_before_queued_rows_covers_and_background() {
    let pace = Arc::new(ApplePace::new(quick(100)));
    // One turn first, so the next four queue behind its spacing.
    pace.turn(AppleClass::Page).await.unwrap();
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for (class, name) in [
        (AppleClass::Background, "background"),
        (AppleClass::Cover, "cover"),
        (AppleClass::Row, "row"),
        (AppleClass::Page, "page"),
    ] {
        start(&pace, class, name, &tx, began);
    }
    settle().await;
    let order: Vec<String> = drain(&mut rx, 4)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(order, ["page", "row", "cover", "background"]);
}

#[tokio::test(start_paused = true)]
async fn the_bucket_lets_six_go_then_one_every_three_seconds() {
    let mut config = quick(6);
    config.spacing = Duration::ZERO;
    let pace = Arc::new(ApplePace::new(config));
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for at in 0..9 {
        start(&pace, AppleClass::Page, &format!("p{at}"), &tx, began);
    }
    settle().await;
    let times: Vec<u128> = drain(&mut rx, 9).await.into_iter().map(|(_, ms)| ms).collect();
    assert_eq!(&times[..6], &[0, 0, 0, 0, 0, 0]);
    assert_eq!(&times[6..], &[3000, 6000, 9000]);
}

#[tokio::test(start_paused = true)]
async fn covers_keep_their_own_spacing_and_need_no_token_while_open() {
    let mut config = quick(0);
    config.spacing = Duration::ZERO;
    config.cover_spacing = Duration::from_secs(1);
    let pace = Arc::new(ApplePace::new(config));
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for at in 0..3 {
        start(&pace, AppleClass::Cover, &format!("c{at}"), &tx, began);
    }
    settle().await;
    let times: Vec<u128> = drain(&mut rx, 3).await.into_iter().map(|(_, ms)| ms).collect();
    assert_eq!(
        times,
        [0, 1000, 2000],
        "an empty bucket never holds a cover while Open"
    );
}

#[tokio::test(start_paused = true)]
async fn any_two_turns_are_half_a_second_apart() {
    let mut config = quick(100);
    config.spacing = Duration::from_millis(500);
    let pace = Arc::new(ApplePace::new(config));
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for at in 0..3 {
        start(&pace, AppleClass::Page, &format!("p{at}"), &tx, began);
    }
    settle().await;
    let times: Vec<u128> = drain(&mut rx, 3).await.into_iter().map(|(_, ms)| ms).collect();
    assert_eq!(times, [0, 500, 1000]);
}

#[tokio::test(start_paused = true)]
async fn pages_keep_four_of_every_five_turns_under_cover_and_background_backlogs() {
    let pace = Arc::new(ApplePace::new(quick(1000)));
    pace.turn(AppleClass::Page).await.unwrap();
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for at in 0..32 {
        start(&pace, AppleClass::Cover, &format!("c{at}"), &tx, began);
        start(&pace, AppleClass::Background, &format!("b{at}"), &tx, began);
    }
    let mut served = Vec::new();
    for at in 0..25 {
        start(&pace, AppleClass::Page, &format!("p{at}"), &tx, began);
        settle().await;
        served.push(rx.recv().await.expect("a turn").0);
    }
    let pages = served.iter().filter(|name| name.starts_with('p')).count();
    assert!(pages >= 20, "{pages} of 25 turns were pages: {served:?}");
    assert!(
        served.iter().any(|name| name.starts_with('c')),
        "covers still get turns: {served:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_lower_class_waiting_gets_the_fifth_turn() {
    let pace = Arc::new(ApplePace::new(quick(1000)));
    pace.turn(AppleClass::Page).await.unwrap();
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    start(&pace, AppleClass::Background, "background", &tx, began);
    // A steady stream of pages: one new page arrives for every turn given.
    let mut served = Vec::new();
    for at in 0..30 {
        start(&pace, AppleClass::Page, &format!("p{at}"), &tx, began);
        settle().await;
        let (name, _) = rx.recv().await.expect("a turn");
        served.push(name);
        if served.iter().any(|name| name == "background") {
            break;
        }
    }
    let at = served
        .iter()
        .position(|name| name == "background")
        .expect("the background waiter is served");
    assert_eq!(at, 4, "the fifth turn is the lower class's share: {served:?}");
}

#[tokio::test(start_paused = true)]
async fn a_row_is_served_within_eight_turns_under_pages_and_a_background_backlog() {
    let pace = Arc::new(ApplePace::new(quick(1000)));
    pace.turn(AppleClass::Page).await.unwrap();
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for at in 0..32 {
        start(&pace, AppleClass::Background, &format!("b{at}"), &tx, began);
    }
    start(&pace, AppleClass::Row, "row", &tx, began);
    let mut served = Vec::new();
    for at in 0..40 {
        start(&pace, AppleClass::Page, &format!("p{at}"), &tx, began);
        settle().await;
        served.push(rx.recv().await.expect("a turn").0);
        if served.iter().any(|name| name == "row") {
            break;
        }
    }
    let at = served
        .iter()
        .position(|name| name == "row")
        .expect("the row is served");
    assert!(at < 8, "served after {at} turns: {served:?}");
}

#[tokio::test(start_paused = true)]
async fn a_cover_goes_while_a_page_waits_for_a_token() {
    let mut config = quick(1);
    config.spacing = Duration::ZERO;
    let pace = Arc::new(ApplePace::new(config));
    pace.turn(AppleClass::Page).await.unwrap(); // the only token
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    start(&pace, AppleClass::Page, "page", &tx, began);
    settle().await;
    start(&pace, AppleClass::Cover, "cover", &tx, began);
    settle().await;
    let seen = drain(&mut rx, 2).await;
    assert_eq!(seen[0], ("cover".to_string(), 0));
    assert_eq!(seen[1], ("page".to_string(), 3000));
}

#[tokio::test(start_paused = true)]
async fn while_strict_covers_take_tokens_and_all_turns_stay_within_the_bucket() {
    let mut config = quick(6);
    config.spacing = Duration::ZERO;
    let pace = Arc::new(ApplePace::new(config));
    pace.set_strict(true);
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for at in 0..10 {
        let class = if at % 2 == 0 {
            AppleClass::Cover
        } else {
            AppleClass::Page
        };
        start(&pace, class, &format!("t{at}"), &tx, began);
    }
    settle().await;
    let times: Vec<u128> = drain(&mut rx, 10).await.into_iter().map(|(_, ms)| ms).collect();
    for (at, ms) in times.iter().enumerate() {
        let allowed = 6 + (*ms / 3000) as usize;
        assert!(at < allowed, "turn {at} at {ms} ms is past the bucket: {times:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_full_row_or_background_queue_refuses_its_oldest_waiter() {
    let mut config = quick(1000);
    config.row_cap = 2;
    config.background_cap = 1;
    let pace = Arc::new(ApplePace::new(config));
    pace.turn(AppleClass::Page).await.unwrap();
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for name in ["r1", "r2", "r3"] {
        start(&pace, AppleClass::Row, name, &tx, began);
        settle().await;
    }
    for name in ["b1", "b2"] {
        start(&pace, AppleClass::Background, name, &tx, began);
        settle().await;
    }
    let seen: Vec<String> = drain(&mut rx, 5)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(seen.contains(&"r1 refused".to_string()), "{seen:?}");
    assert!(seen.contains(&"b1 refused".to_string()), "{seen:?}");
    for name in ["r2", "r3", "b2"] {
        assert!(seen.contains(&name.to_string()), "{seen:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn pages_and_covers_are_never_refused() {
    let mut config = quick(1000);
    config.row_cap = 0;
    config.background_cap = 0;
    let pace = Arc::new(ApplePace::new(config));
    for _ in 0..20 {
        assert!(pace.turn(AppleClass::Page).await.is_ok());
        assert!(pace.turn(AppleClass::Cover).await.is_ok());
    }
}

#[tokio::test(start_paused = true)]
async fn a_refusal_doubles_the_backoff_empties_the_bucket_and_fifty_answers_halve_it() {
    let mut config = quick(6);
    config.spacing = Duration::ZERO;
    config.cover_spacing = Duration::from_secs(1);
    let pace = ApplePace::new(config);
    pace.refused_by_apple();
    assert_eq!(pace.backoff(), Duration::from_secs(2));
    pace.refused_by_apple();
    assert_eq!(pace.backoff(), Duration::from_secs(4));
    for _ in 0..5 {
        pace.refused_by_apple();
    }
    assert_eq!(pace.backoff(), Duration::from_secs(8), "never past the maximum");
    // The bucket is empty: the next catalog turn waits for a token.
    let began = Instant::now();
    pace.turn(AppleClass::Page).await.unwrap();
    assert_eq!(began.elapsed(), Duration::from_secs(3));
    for _ in 0..49 {
        pace.answered_by_apple();
    }
    assert_eq!(pace.backoff(), Duration::from_secs(8));
    pace.answered_by_apple();
    assert_eq!(pace.backoff(), Duration::from_secs(4));
    for _ in 0..100 {
        pace.answered_by_apple();
    }
    assert_eq!(
        pace.backoff(),
        Duration::ZERO,
        "halved to nothing once it is down to the floor"
    );
}

#[tokio::test(start_paused = true)]
async fn the_backoff_widens_every_spacing() {
    let mut config = quick(100);
    config.spacing = Duration::from_millis(500);
    config.cover_spacing = Duration::from_secs(1);
    let pace = Arc::new(ApplePace::new(config));
    pace.refused_by_apple();
    // Refills after the refusal emptied the bucket.
    tokio::time::sleep(Duration::from_secs(30)).await;
    let began = Instant::now();
    pace.turn(AppleClass::Page).await.unwrap();
    pace.turn(AppleClass::Page).await.unwrap();
    assert_eq!(
        began.elapsed(),
        Duration::from_secs(2),
        "the backoff (2 s) is the spacing now"
    );
}

#[tokio::test(start_paused = true)]
async fn a_ticket_raised_to_page_goes_before_a_waiting_row() {
    let pace = Arc::new(ApplePace::new(quick(1000)));
    pace.turn(AppleClass::Page).await.unwrap();
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let ticket = pace.ticket(AppleClass::Background);
    start(&pace, AppleClass::Row, "row", &tx, began);
    settle().await;
    ticket.raise(AppleClass::Page);
    let raised = {
        let tx = tx.clone();
        tokio::spawn(async move {
            ticket.wait().await.unwrap();
            let _ = tx.send(("raised".to_string(), began.elapsed().as_millis()));
        })
    };
    settle().await;
    let order: Vec<String> = drain(&mut rx, 2)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(order, ["raised", "row"]);
    raised.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_ticket_dropped_before_its_turn_leaves_the_queue() {
    let pace = Arc::new(ApplePace::new(quick(1000)));
    let ticket = pace.ticket(AppleClass::Row);
    assert_eq!(pace.state.lock().waiters.len(), 1);
    drop(ticket);
    assert!(pace.state.lock().waiters.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_dropped_waiter_leaves_the_queue() {
    let pace = Arc::new(ApplePace::new(quick(1000)));
    pace.turn(AppleClass::Page).await.unwrap();
    let began = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let dropped = start(&pace, AppleClass::Page, "dropped", &tx, began);
    settle().await;
    dropped.abort();
    settle().await;
    start(&pace, AppleClass::Background, "after", &tx, began);
    settle().await;
    let seen = drain(&mut rx, 1).await;
    assert_eq!(seen[0].0, "after");
    assert!(pace.state.lock().waiters.is_empty());
}
