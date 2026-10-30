//! Tests for AcoustID's lanes: the user's own key's budget, Octo's own key's person and bulk
//! gaps, class order, the backing off and coming back that follow AcoustID's answers, and held
//! lookups woken again. Time is paused, so the gaps are the real ones and the tests take no time.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use parking_lot::Mutex;
use tokio::time::Instant;

use super::*;

fn steady() -> Arc<AcoustIdRateLimiter> {
    Arc::new(AcoustIdRateLimiter::new().with_jitter(|| 1.0))
}

/// One call of `class` on `lane`, sent as soon as its turn comes and answered at once; the time
/// its turn came.
async fn call(limiter: &AcoustIdRateLimiter, lane: KeyLane, class: LookupClass) -> Instant {
    let turn = limiter.permit(lane, class, true).await.expect("a turn");
    let at = Instant::now();
    turn.end(CallEnd::Answered);
    at
}

/// The sweep never queues: with the budget spent it is refused at once, while a download's
/// lookup waits for the next permit.
#[tokio::test(start_paused = true)]
async fn the_background_lane_never_waits() {
    let limiter = AcoustIdRateLimiter::new();
    for _ in 0..3 {
        assert!(limiter.acquire().await.is_acquired());
    }

    let refused = AcoustIdRateLimiter::in_background(async { limiter.acquire().now_or_never() }).await;
    assert!(!refused.expect("answered at once").is_acquired());
    assert!(!AcoustIdRateLimiter::in_background_now());

    assert!(limiter.acquire().await.is_acquired());
}

/// With only Octo's own key, an import's lookup waits while a heart's is queued, goes after it,
/// and never sooner than the bulk gap after the last bulk call.
#[tokio::test(start_paused = true)]
async fn an_import_waits_behind_a_heart_and_its_own_bulk_gap() {
    let limiter = steady();
    let start = Instant::now();
    call(&limiter, KeyLane::BuiltIn, LookupClass::Bulk).await;
    call(&limiter, KeyLane::BuiltIn, LookupClass::Heart).await;

    let order = Arc::new(Mutex::new(Vec::new()));
    let queue = |class: LookupClass| {
        let (limiter, order) = (Arc::clone(&limiter), Arc::clone(&order));
        tokio::spawn(async move {
            let at = call(&limiter, KeyLane::BuiltIn, class).await;
            order.lock().push((class, at));
        })
    };
    let import = queue(LookupClass::Bulk);
    tokio::task::yield_now().await;
    let heart = queue(LookupClass::Heart);
    import.await.unwrap();
    heart.await.unwrap();

    let order = order.lock().clone();
    assert_eq!(order[0].0, LookupClass::Heart);
    assert_eq!(order[0].1 - start, AcoustIdRateLimiter::PERSON_GAP);
    assert_eq!(order[1].0, LookupClass::Bulk);
    assert_eq!(order[1].1 - start, AcoustIdRateLimiter::BULK_GAP);
}

/// Waiters on Octo's own key go in class order, hearts first, then a person's clicks, then family
/// requests, then bulk work, each class in the order it asked, one person gap apart.
#[tokio::test(start_paused = true)]
async fn waiters_on_octos_own_key_go_in_class_order() {
    let limiter = steady();
    let start = Instant::now();
    call(&limiter, KeyLane::BuiltIn, LookupClass::Bulk).await;
    call(&limiter, KeyLane::BuiltIn, LookupClass::Heart).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for (n, class) in [
        LookupClass::Bulk,
        LookupClass::Family,
        LookupClass::Person,
        LookupClass::Heart,
        LookupClass::Person,
    ]
    .into_iter()
    .enumerate()
    {
        let (limiter, order) = (Arc::clone(&limiter), Arc::clone(&order));
        tasks.push(tokio::spawn(async move {
            let at = call(&limiter, KeyLane::BuiltIn, class).await;
            order.lock().push((n, at - start));
        }));
        tokio::task::yield_now().await;
    }
    for task in tasks {
        task.await.unwrap();
    }
    let gap = AcoustIdRateLimiter::PERSON_GAP;
    assert_eq!(
        *order.lock(),
        [
            (3, gap),
            (2, gap * 2),
            (4, gap * 3),
            (1, gap * 4),
            (0, AcoustIdRateLimiter::BULK_GAP)
        ]
    );
}

/// Every busy answer doubles the gap of the call that got it: an overload up to an hour, an
/// outage up to 64 s for a person's calls and 640 s for bulk work. Each answered call halves it,
/// down to its floor.
#[tokio::test(start_paused = true)]
async fn busy_answers_double_the_gap_and_answers_halve_it() {
    let secs = |values: &[u64]| values.iter().map(|s| Duration::from_secs(*s)).collect::<Vec<_>>();
    let limiter = steady();
    let outage = CallEnd::Busy(BusyKind::Outage);
    assert_eq!(
        gaps_after(&limiter, KeyLane::BuiltIn, LookupClass::Heart, outage, 7).await,
        secs(&[2, 4, 8, 16, 32, 64, 64, 64])
    );
    // A family request's answers bring back the person pace it shares with hearts.
    let back = gaps_after(
        &limiter,
        KeyLane::BuiltIn,
        LookupClass::Family,
        CallEnd::Answered,
        7,
    )
    .await;
    assert_eq!(back, secs(&[64, 32, 16, 8, 4, 2, 2, 2]));
    // The bulk gap is its own.
    assert_eq!(
        limiter.gap(KeyLane::BuiltIn, LookupClass::Bulk),
        AcoustIdRateLimiter::BULK_GAP
    );
}

async fn gaps_after(
    limiter: &AcoustIdRateLimiter,
    lane: KeyLane,
    class: LookupClass,
    end: CallEnd,
    calls: usize,
) -> Vec<Duration> {
    let mut gaps = vec![limiter.gap(lane, class)];
    for _ in 0..calls {
        let turn = limiter.permit(lane, class, true).await.expect("a turn");
        turn.end(end);
        gaps.push(limiter.gap(lane, class));
    }
    gaps
}

/// An overload backs off up to an hour on both of Octo's own key's paces; an outage stops at 640
/// s for bulk work.
#[tokio::test(start_paused = true)]
async fn an_overload_backs_off_to_an_hour_and_bulk_outages_to_640_s() {
    let secs = |values: &[u64]| values.iter().map(|s| Duration::from_secs(*s)).collect::<Vec<_>>();
    let overload = CallEnd::Busy(BusyKind::Overload);
    let outage = CallEnd::Busy(BusyKind::Outage);
    let limiter = steady();
    assert_eq!(
        gaps_after(&limiter, KeyLane::BuiltIn, LookupClass::Heart, overload, 12).await,
        secs(&[2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 3600, 3600])
    );
    let limiter = steady();
    assert_eq!(
        gaps_after(&limiter, KeyLane::BuiltIn, LookupClass::Bulk, outage, 7).await,
        secs(&[20, 40, 80, 160, 320, 640, 640, 640])
    );
    let limiter = steady();
    assert_eq!(
        gaps_after(&limiter, KeyLane::BuiltIn, LookupClass::Bulk, overload, 9).await,
        secs(&[20, 40, 80, 160, 320, 640, 1280, 2560, 3600, 3600])
    );
    // An outage after an overload never shortens the gap.
    let limiter = steady();
    gaps_after(&limiter, KeyLane::BuiltIn, LookupClass::Heart, overload, 8).await;
    assert_eq!(
        gaps_after(&limiter, KeyLane::BuiltIn, LookupClass::Heart, outage, 1).await,
        secs(&[512, 512])
    );
}

/// The user's own key keeps its 3 a second while AcoustID answers, and backs off from that floor
/// on busy answers as Octo's own key does.
#[tokio::test(start_paused = true)]
async fn the_users_key_keeps_its_budget_and_backs_off_from_its_own_floor() {
    let limiter = steady();
    let start = Instant::now();
    for _ in 0..3 {
        assert_eq!(call(&limiter, KeyLane::User, LookupClass::Heart).await, start);
    }
    let floor = AcoustIdRateLimiter::USER_GAP;
    let limiter = steady();
    let outage = gaps_after(
        &limiter,
        KeyLane::User,
        LookupClass::Heart,
        CallEnd::Busy(BusyKind::Outage),
        9,
    )
    .await;
    assert_eq!(outage[..3], [floor, floor * 2, floor * 4]);
    assert_eq!(outage[9], AcoustIdRateLimiter::PERSON_OUTAGE_GAP);
    // Bulk work on the user's key shares the one pace.
    assert_eq!(
        limiter.gap(KeyLane::User, LookupClass::Bulk),
        AcoustIdRateLimiter::PERSON_OUTAGE_GAP
    );
    let limiter = steady();
    let overload = gaps_after(
        &limiter,
        KeyLane::User,
        LookupClass::Heart,
        CallEnd::Busy(BusyKind::Overload),
        16,
    )
    .await;
    assert_eq!(overload[16], Duration::from_secs(3_600));
    // Backed off, the next call waits the whole gap.
    let before = Instant::now();
    call(&limiter, KeyLane::User, LookupClass::Heart).await;
    assert_eq!(Instant::now() - before, Duration::from_secs(3_600));
}

/// The random spread is a quarter either way.
#[tokio::test(start_paused = true)]
async fn each_doubling_is_spread_by_a_quarter_either_way() {
    for (jitter, want) in [(0.75, 3_000), (1.25, 5_000), (3.0, 5_000), (0.1, 3_000)] {
        let limiter = AcoustIdRateLimiter::new().with_jitter(move || jitter);
        let gaps = gaps_after(
            &limiter,
            KeyLane::BuiltIn,
            LookupClass::Heart,
            CallEnd::Busy(BusyKind::Overload),
            1,
        )
        .await;
        assert_eq!(gaps[1], Duration::from_millis(want), "{jitter}");
    }
    let limiter = AcoustIdRateLimiter::new();
    let gaps = gaps_after(
        &limiter,
        KeyLane::BuiltIn,
        LookupClass::Heart,
        CallEnd::Busy(BusyKind::Overload),
        1,
    )
    .await;
    assert!(
        (Duration::from_secs(3)..=Duration::from_secs(5)).contains(&gaps[1]),
        "{:?}",
        gaps[1]
    );
}

/// A turn not free at once is refused at once when the caller cannot wait, with nothing left
/// queued, and a turn given up frees the lane and changes no pace.
#[tokio::test(start_paused = true)]
async fn a_turn_not_free_at_once_is_refused_and_a_dropped_one_changes_nothing() {
    let limiter = steady();
    let first = limiter
        .permit(KeyLane::BuiltIn, LookupClass::Heart, false)
        .await
        .expect("free at once");
    drop(first);
    assert!(
        limiter
            .permit(KeyLane::BuiltIn, LookupClass::Heart, false)
            .await
            .is_none()
    );
    assert!(
        AcoustIdRateLimiter::in_background(limiter.permit(KeyLane::BuiltIn, LookupClass::Bulk, true))
            .await
            .is_some(),
        "the bulk pace has its own first call"
    );
    assert!(
        AcoustIdRateLimiter::in_background(limiter.permit(KeyLane::BuiltIn, LookupClass::Bulk, true))
            .await
            .is_none()
    );
    assert_eq!(
        limiter.gap(KeyLane::BuiltIn, LookupClass::Heart),
        AcoustIdRateLimiter::PERSON_GAP
    );
    let start = Instant::now();
    call(&limiter, KeyLane::BuiltIn, LookupClass::Heart).await;
    assert_eq!(Instant::now() - start, AcoustIdRateLimiter::PERSON_GAP);
}

/// Classes follow an acquisition's origin.
#[test]
fn origins_take_their_classes() {
    use octo_core::fingerprint::heart_gate::AcquisitionOrigin as Origin;
    let classes: Vec<LookupClass> = Origin::ALL
        .iter()
        .map(|origin| LookupClass::for_origin(*origin))
        .collect();
    assert_eq!(
        classes,
        [
            LookupClass::Heart,
            LookupClass::Heart,
            LookupClass::Heart,
            LookupClass::Bulk,
            LookupClass::Family,
            LookupClass::Bulk,
            LookupClass::Person,
        ]
    );
}

/// Work outside a class is bulk; inside one it is that class, and only for that work.
#[tokio::test]
async fn work_takes_the_class_it_runs_in() {
    assert_eq!(AcoustIdRateLimiter::class_now(), LookupClass::Bulk);
    let inside =
        AcoustIdRateLimiter::with_class(LookupClass::Person, async { AcoustIdRateLimiter::class_now() })
            .await;
    assert_eq!(inside, LookupClass::Person);
    assert_eq!(AcoustIdRateLimiter::class_now(), LookupClass::Bulk);
}

fn waker(woken: &Arc<Mutex<Vec<(u32, Instant)>>>, n: u32) -> Box<dyn FnOnce() + Send> {
    let woken = Arc::clone(woken);
    Box::new(move || woken.lock().push((n, Instant::now())))
}

/// A held lookup is woken after an answered call on its lane, the oldest first, one per answer;
/// a busy answer wakes none.
#[tokio::test(start_paused = true)]
async fn an_answered_call_wakes_the_oldest_held_lookup() {
    let limiter = steady();
    let woken = Arc::new(Mutex::new(Vec::new()));
    let busy_turn = limiter
        .permit(KeyLane::BuiltIn, LookupClass::Heart, true)
        .await
        .expect("a turn");
    limiter.hold(KeyLane::BuiltIn, waker(&woken, 1));
    limiter.hold(KeyLane::BuiltIn, waker(&woken, 2));
    tokio::task::yield_now().await;
    assert!(woken.lock().is_empty(), "nothing wakes while a call is out");
    busy_turn.end(CallEnd::Busy(BusyKind::Outage));
    assert!(woken.lock().is_empty(), "a busy answer wakes nothing at once");

    let start = Instant::now();
    let turn = limiter
        .permit(KeyLane::BuiltIn, LookupClass::Heart, true)
        .await
        .expect("a turn");
    assert_eq!(Instant::now() - start, Duration::from_secs(4));
    turn.end(CallEnd::Answered);
    assert_eq!(woken.lock().iter().map(|(n, _)| *n).collect::<Vec<_>>(), [1]);
}

/// With held lookups and nothing else sent, the lane wakes the oldest at its person gap since its
/// last call, then one more each gap, so an idle install resumes once AcoustID recovers.
#[tokio::test(start_paused = true)]
async fn an_idle_lane_wakes_one_held_lookup_each_person_gap() {
    let limiter = steady();
    let woken = Arc::new(Mutex::new(Vec::new()));
    let start = Instant::now();
    let turn = limiter
        .permit(KeyLane::BuiltIn, LookupClass::Heart, true)
        .await
        .expect("a turn");
    turn.end(CallEnd::Busy(BusyKind::Outage));
    limiter.hold(KeyLane::BuiltIn, waker(&woken, 1));
    limiter.hold(KeyLane::BuiltIn, waker(&woken, 2));
    tokio::time::sleep(Duration::from_secs(20)).await;
    let woken = woken.lock().clone();
    assert_eq!(
        woken,
        [
            (1, start + Duration::from_secs(4)),
            (2, start + Duration::from_secs(8))
        ]
    );
}
