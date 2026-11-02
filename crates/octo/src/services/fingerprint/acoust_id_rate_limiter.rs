//! The AcoustID request budget: the user's own key's lane, and Octo's own key's lane, which every
//! install without a key of its own shares.

use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use octo_core::fingerprint::heart_gate::AcquisitionOrigin;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::services::framework::{RateLimitLease, SlidingWindowRateLimiter, SlidingWindowRateLimiterOptions};

tokio::task_local! {
    /// Set for the work run by [`AcoustIdRateLimiter::in_background`]. It does not reach a task
    /// that work spawns; the lookups are awaited inline, so nothing needs it to.
    static BACKGROUND_FLOW: bool;
    /// Set for the work run by [`AcoustIdRateLimiter::with_class`].
    static CALL_CLASS: LookupClass;
    /// Set for the work run by [`AcoustIdRateLimiter::at_least`].
    static CLASS_FLOOR: LookupClass;
    /// Set for the work run by [`AcoustIdRateLimiter::if_free`]: whether a call in it found no
    /// free turn.
    static TURNED_AWAY: Arc<AtomicBool>;
}

/// Whose AcoustID call this is, which decides the key it uses and its place in the queue: earlier
/// classes go first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LookupClass {
    /// A heart's or a play's gate, and the re-checks of what a heart kept or holds.
    Heart,
    /// A lookup a person starts with one click: the library editor's check, the tag preview, a
    /// Find songs pick's file check.
    Person,
    /// A family member's request.
    Family,
    /// Work nobody waits on: imports, version checks, the upgrade queue, the Review sweep and the
    /// finder's sort of library files by take.
    Bulk,
}

impl LookupClass {
    /// The class an acquisition's file checks take: a heart's, an album heart's and a play's go
    /// first, a Find songs pick is a person's click, a family request its own, and imports and
    /// replacements are work nobody waits on.
    pub fn for_origin(origin: AcquisitionOrigin) -> Self {
        match origin {
            AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Play => Self::Heart,
            AcquisitionOrigin::SongFinderPick => Self::Person,
            AcquisitionOrigin::Request => Self::Family,
            AcquisitionOrigin::Import | AcquisitionOrigin::Replacement => Self::Bulk,
        }
    }

    /// Hearts, plays, a person's clicks and family requests share one pace; bulk work has its own.
    fn is_bulk(self) -> bool {
        self == Self::Bulk
    }
}

/// Which lane a key's calls take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyLane {
    /// The user's own application key: 3 calls a second.
    User,
    /// Octo's own key, shared by every install without one: a person gap and a bulk gap.
    BuiltIn,
}

/// Why AcoustID did not answer a call, which sets how far the call's lane backs off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyKind {
    /// Too many calls on the key: a 429, or code 14.
    Overload,
    /// AcoustID is failing: a 5xx, code 5 or 13, a timeout, a network error or a body that cannot
    /// be read.
    Outage,
}

/// How one call on a lane ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEnd {
    /// AcoustID answered: `status: ok`, or an error code other than 5, 13 and 14, with no 429
    /// and no 5xx.
    Answered,
    Busy(BusyKind),
}

/// The gap between two calls of one pace. Every busy answer doubles it, with a random 25% either
/// way so installs that backed off together do not return together, up to a cap by kind (an
/// overload up to an hour, an outage much less, since an outage is not caused by load); every
/// answered call halves it again, down to its floor. A pace AcoustID's own answers set, never a
/// hold timer.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Gap {
    floor: Duration,
    current: Duration,
    outage_cap: Duration,
}

impl Gap {
    const OVERLOAD_CAP: Duration = Duration::from_secs(3_600);

    fn new(floor: Duration, outage_cap: Duration) -> Self {
        Self {
            floor,
            current: floor,
            outage_cap,
        }
    }

    fn end(&mut self, end: CallEnd, jitter: f64) {
        match end {
            CallEnd::Answered => self.current = (self.current / 2).max(self.floor),
            CallEnd::Busy(kind) => {
                let cap = match kind {
                    BusyKind::Overload => Self::OVERLOAD_CAP,
                    BusyKind::Outage => self.outage_cap,
                };
                let doubled = scaled(self.current, 2.0 * jitter.clamp(0.75, 1.25));
                self.current = self.current.max(doubled.min(cap));
            }
        }
    }
}

/// A duration times a factor, to the nearest nanosecond.
fn scaled(duration: Duration, factor: f64) -> Duration {
    Duration::from_nanos((duration.as_nanos() as f64 * factor).round() as u64)
}

/// A lookup held until AcoustID answers again, woken to ask once more.
type Wake = Box<dyn FnOnce() + Send>;

/// The calls one lane is pacing, and the held lookups it wakes.
struct Pacing {
    /// The person pace, then the bulk pace. The user's lane paces every class as one.
    gaps: [Gap; 2],
    last_sent: [Option<Instant>; 2],
    /// Calls waiting for their turn, in class order, then in the order they asked.
    waiting: BTreeSet<(LookupClass, u64)>,
    next_ticket: u64,
    in_flight: usize,
    held: VecDeque<Wake>,
    /// When the lane last woke a held lookup, so it wakes at most one each person gap.
    last_woken: Option<Instant>,
    /// A task is waking held lookups while the lane is idle.
    waking: bool,
    /// Held lookups answered calls have earned a wake for, each woken once its call can go.
    owed: usize,
    /// A task is waking the held lookups answered calls earned.
    paying: bool,
}

/// One lane: its pacing, and for the user's lane the 3-a-second budget it keeps.
struct Lane {
    kind: KeyLane,
    pacing: Mutex<Pacing>,
    changed: Notify,
    window: Option<SlidingWindowRateLimiter>,
}

impl Lane {
    /// The pace a class's calls take on this lane.
    fn pace(&self, class: LookupClass) -> usize {
        usize::from(self.kind == KeyLane::BuiltIn && class.is_bulk())
    }

    /// When the call holding `ticket` may go: now, at a time, or once the calls ahead of it have.
    fn turn(&self, pacing: &Pacing, class: LookupClass, ticket: u64, now: Instant) -> Turn {
        if self.kind == KeyLane::BuiltIn && pacing.waiting.first() != Some(&(class, ticket)) {
            return Turn::Behind;
        }
        let pace = self.pace(class);
        let gap = pacing.gaps[pace];
        // The user's lane keeps its 3 a second while AcoustID answers; only a backed-off gap spaces
        // its calls further.
        let spacing = if self.kind == KeyLane::User && gap.current <= gap.floor {
            Duration::ZERO
        } else {
            gap.current
        };
        match pacing.last_sent[pace] {
            Some(sent) if now < sent + spacing => Turn::At(sent + spacing),
            _ => Turn::Go,
        }
    }

    /// Whether nothing is waiting or in flight, so only the lane itself can wake a held lookup.
    fn idle(pacing: &Pacing) -> bool {
        pacing.waiting.is_empty() && pacing.in_flight == 0
    }
}

enum Turn {
    Go,
    At(Instant),
    Behind,
}

/// A turn on a lane: the call it was taken for is sent now. Tell the lane how the call ended with
/// [`CallPermit::end`]; one dropped without that frees its turn and changes no pace.
pub struct CallPermit {
    limiter: Arc<Lanes>,
    lane: KeyLane,
    class: LookupClass,
    ended: bool,
}

impl CallPermit {
    pub fn lane(&self) -> KeyLane {
        self.lane
    }

    pub fn class(&self) -> LookupClass {
        self.class
    }

    /// The call's answer sets its pace: a busy answer backs it off, an answered one lets it come
    /// back and wakes the oldest held lookup.
    pub fn end(mut self, end: CallEnd) {
        self.ended = true;
        self.limiter.end(self.lane, self.class, Some(end));
    }
}

impl Drop for CallPermit {
    fn drop(&mut self) {
        if !self.ended {
            self.limiter.end(self.lane, self.class, None);
        }
    }
}

/// Removes a waiting call from its lane's queue when it gives up its place.
struct Place<'a> {
    lane: &'a Lane,
    entry: (LookupClass, u64),
    taken: bool,
}

impl Drop for Place<'_> {
    fn drop(&mut self) {
        if !self.taken {
            self.lane.pacing.lock().waiting.remove(&self.entry);
            self.lane.changed.notify_waiters();
        }
    }
}

struct Lanes {
    user: Lane,
    built_in: Lane,
    jitter: Box<dyn Fn() -> f64 + Send + Sync>,
}

impl Lanes {
    fn lane(&self, kind: KeyLane) -> &Lane {
        match kind {
            KeyLane::User => &self.user,
            KeyLane::BuiltIn => &self.built_in,
        }
    }

    fn end(self: &Arc<Self>, kind: KeyLane, class: LookupClass, end: Option<CallEnd>) {
        let lane = self.lane(kind);
        let jitter = (self.jitter)();
        let earned = {
            let mut pacing = lane.pacing.lock();
            pacing.in_flight = pacing.in_flight.saturating_sub(1);
            if let Some(end) = end {
                let pace = lane.pace(class);
                pacing.gaps[pace].end(end, jitter);
            }
            // An answer earns the oldest held lookup a wake, paid once its call can go at once:
            // woken before the person gap has passed, its call would be turned away and it would
            // be held again behind the others.
            let earned = end == Some(CallEnd::Answered) && pacing.owed < pacing.held.len();
            if earned {
                pacing.owed += 1;
            }
            earned
        };
        lane.changed.notify_waiters();
        if earned {
            self.pay_wakes(kind);
        }
        self.wake_when_idle(kind);
    }

    /// Wakes the held lookups answers earned, oldest first, each once the lane's person gap has
    /// passed since its last call and its last wake.
    fn pay_wakes(self: &Arc<Self>, kind: KeyLane) {
        {
            let mut pacing = self.lane(kind).pacing.lock();
            if pacing.paying || pacing.owed == 0 {
                return;
            }
            pacing.paying = true;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.lane(kind).pacing.lock().paying = false;
            return;
        };
        let lanes = Arc::clone(self);
        runtime.spawn(async move {
            loop {
                let woken = {
                    let mut pacing = lanes.lane(kind).pacing.lock();
                    if pacing.held.is_empty() {
                        pacing.owed = 0;
                    }
                    if pacing.owed == 0 {
                        pacing.paying = false;
                        return;
                    }
                    let gap = pacing.gaps[0].current;
                    let since = pacing.last_sent[0].max(pacing.last_woken);
                    match since.map(|at| at + gap) {
                        Some(due) if Instant::now() < due => Err(due),
                        _ => {
                            pacing.owed -= 1;
                            pacing.last_woken = Some(Instant::now());
                            Ok(pacing.held.pop_front())
                        }
                    }
                };
                match woken {
                    Err(due) => tokio::time::sleep_until(due).await,
                    Ok(wake) => {
                        if let Some(wake) = wake {
                            wake();
                        }
                    }
                }
            }
        });
    }

    /// Holds a lookup until the lane answers a call, or, while nothing else is sent, until the
    /// lane's person gap has passed since its last call.
    fn hold(self: &Arc<Self>, kind: KeyLane, wake: Wake) {
        self.lane(kind).pacing.lock().held.push_back(wake);
        self.wake_when_idle(kind);
    }

    /// With held lookups and nothing else on the lane, sends the oldest held one again at the
    /// lane's person gap, so an install with no other traffic still resumes once AcoustID
    /// recovers. Its own answer wakes the next.
    fn wake_when_idle(self: &Arc<Self>, kind: KeyLane) {
        {
            let mut pacing = self.lane(kind).pacing.lock();
            if pacing.waking || pacing.held.is_empty() || !Lane::idle(&pacing) {
                return;
            }
            pacing.waking = true;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.lane(kind).pacing.lock().waking = false;
            return;
        };
        let lanes = Arc::clone(self);
        runtime.spawn(async move {
            loop {
                let woken = {
                    let mut pacing = lanes.lane(kind).pacing.lock();
                    if pacing.held.is_empty() || !Lane::idle(&pacing) {
                        pacing.waking = false;
                        return;
                    }
                    let gap = pacing.gaps[0].current;
                    let since = pacing.last_sent[0].max(pacing.last_woken);
                    match since.map(|at| at + gap) {
                        Some(due) if Instant::now() < due => Err(due),
                        _ => {
                            pacing.last_woken = Some(Instant::now());
                            Ok(pacing.held.pop_front())
                        }
                    }
                };
                match woken {
                    Err(due) => tokio::time::sleep_until(due).await,
                    Ok(wake) => {
                        if let Some(wake) = wake {
                            wake();
                        }
                    }
                }
            }
        });
    }
}

/// Keeps Octo inside AcoustID's budget.
///
/// The same trap as Deezer, and it bites harder here: over-budget is not reliably a 429,
/// it is an error envelope in a 200 body. That parses as "no match", and this feature reads
/// "no match" as "accept the file", so exceeding the budget would silently turn
/// verification OFF rather than merely slow it down.
///
/// The user's own key has a lane of 3 requests a second with a background lane. A download's
/// lookup sits between a finished transfer and the library and waits in the queue. The library
/// sweep (#72) never waits: it takes a permit only when one is free and no download is queued
/// (OldestFirst refuses a non-queuing request while anyone waits), so it can never take a queue
/// slot a download needed.
///
/// Octo's own key is shared by every install without one, under AcoustID's limit of 10 calls a
/// second for one application, so its lane keeps two paces per install: hearts, plays, a person's
/// clicks and family requests share the person gap (2 s), and bulk work has its own (20 s, and it
/// also waits while any other call is queued). Its waiters go in class order, and nothing on it
/// is ever refused for a full queue.
pub struct AcoustIdRateLimiter {
    lanes: Arc<Lanes>,
}

impl AcoustIdRateLimiter {
    /// The name of the client that carries the limiter. A caller that uses any other client
    /// bypasses the budget entirely.
    pub const CLIENT_NAME: &'static str = "acoustid";

    const PERMITS_PER_SECOND: u32 = 3;

    /// The person gap on Octo's own key, and the furthest an outage backs it off.
    pub const PERSON_GAP: Duration = Duration::from_secs(2);
    pub const PERSON_OUTAGE_GAP: Duration = Duration::from_secs(64);
    /// The bulk gap on Octo's own key, never less, and the furthest an outage backs it off.
    pub const BULK_GAP: Duration = Duration::from_secs(20);
    pub const BULK_OUTAGE_GAP: Duration = Duration::from_secs(640);
    /// The user's own key's gap: its 3 a second.
    pub const USER_GAP: Duration = Duration::from_millis(334);

    pub fn new() -> Self {
        Self::with_gaps(Self::PERSON_GAP, Self::BULK_GAP)
    }

    /// A limiter whose Octo's-own-key lane starts its person and bulk gaps at these floors, so a
    /// test runs in seconds.
    pub fn with_gaps(person: Duration, bulk: Duration) -> Self {
        let pacing = |gaps: [Gap; 2]| {
            Mutex::new(Pacing {
                gaps,
                last_sent: [None, None],
                waiting: BTreeSet::new(),
                next_ticket: 0,
                in_flight: 0,
                held: VecDeque::new(),
                last_woken: None,
                waking: false,
                owed: 0,
                paying: false,
            })
        };
        // A test's shorter floors keep the outage caps in the same proportion.
        let user_gap = Gap::new(Self::USER_GAP, Self::PERSON_OUTAGE_GAP);
        let cap = |cap: Duration, floor: Duration, base: Duration| {
            scaled(cap, floor.as_secs_f64() / base.as_secs_f64())
        };
        let person_gap = Gap::new(person, cap(Self::PERSON_OUTAGE_GAP, person, Self::PERSON_GAP));
        let bulk_gap = Gap::new(bulk, cap(Self::BULK_OUTAGE_GAP, bulk, Self::BULK_GAP));
        Self {
            lanes: Arc::new(Lanes {
                user: Lane {
                    kind: KeyLane::User,
                    pacing: pacing([user_gap, user_gap]),
                    changed: Notify::new(),
                    window: Some(SlidingWindowRateLimiter::new(SlidingWindowRateLimiterOptions {
                        permit_limit: Self::PERMITS_PER_SECOND,
                        window: Duration::from_secs(1),
                        segments_per_window: 3,
                        // Must be set explicitly: it defaults to 0, which makes `acquire` return a
                        // NON-acquired lease immediately instead of waiting. A dropped lookup here
                        // is an accepted file, so dropping is the one thing this must not do by
                        // default. 3/s drains 12 in four seconds.
                        queue_limit: 12,
                    })),
                },
                built_in: Lane {
                    kind: KeyLane::BuiltIn,
                    pacing: pacing([person_gap, bulk_gap]),
                    changed: Notify::new(),
                    window: None,
                },
                jitter: Box::new(|| rand::random_range(0.75..=1.25)),
            }),
        }
    }

    /// The same limiter with a fixed jitter factor (0.75 to 1.25) in place of a random one.
    pub fn with_jitter(self, jitter: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        let lanes = Arc::into_inner(self.lanes).expect("the jitter is set before the limiter is shared");
        Self {
            lanes: Arc::new(Lanes {
                jitter: Box::new(jitter),
                ..lanes
            }),
        }
    }

    /// Lookups started inside `work` use the background lane. The flag ends when `work` does
    /// and never reaches the caller.
    pub async fn in_background<T>(work: impl Future<Output = T>) -> T {
        BACKGROUND_FLOW.scope(true, work).await
    }

    /// Whether the current work runs in the background lane.
    pub fn in_background_now() -> bool {
        BACKGROUND_FLOW.try_with(|flag| *flag).unwrap_or(false)
    }

    /// Lookups started inside `work` are of `class`. The class ends when `work` does.
    pub async fn with_class<T>(class: LookupClass, work: impl Future<Output = T>) -> T {
        CALL_CLASS.scope(class, work).await
    }

    /// The current work's class: the one [`Self::with_class`] set, else work nobody waits on.
    pub fn class_now() -> LookupClass {
        CALL_CLASS.try_with(|class| *class).unwrap_or(LookupClass::Bulk)
    }

    /// Lookups started inside `work` take no earlier place than `floor`'s, whatever class they
    /// are asked with. The floor ends when `work` does.
    pub async fn at_least<T>(floor: LookupClass, work: impl Future<Output = T>) -> T {
        CLASS_FLOOR.scope(floor, work).await
    }

    /// `class`, or the current work's floor when that comes later.
    pub fn floored(class: LookupClass) -> LookupClass {
        CLASS_FLOOR.try_with(|floor| class.max(*floor)).unwrap_or(class)
    }

    /// Calls started inside `work` take a turn only when one is free at once, and never wait for
    /// one. The second value says whether one of them found none, so a caller tells "AcoustID was
    /// not asked" from AcoustID failing.
    pub async fn if_free<T>(work: impl Future<Output = T>) -> (T, bool) {
        let turned_away = Arc::new(AtomicBool::new(false));
        let done = TURNED_AWAY.scope(Arc::clone(&turned_away), work).await;
        (done, turned_away.load(Ordering::SeqCst))
    }

    /// Whether the current work takes only a turn that is free at once.
    fn if_free_now() -> bool {
        TURNED_AWAY.try_with(|_| true).unwrap_or(false)
    }

    /// Notes that a call of [`Self::if_free`]'s work found no free turn.
    fn note_turned_away() {
        let _ = TURNED_AWAY.try_with(|flag| flag.store(true, Ordering::SeqCst));
    }

    /// Whether a class's pace on a lane is backed off above where it starts: AcoustID answered a
    /// call of that pace Busy, and has not answered enough since to bring it back.
    pub fn backing_off(&self, lane: KeyLane, class: LookupClass) -> bool {
        let lane = self.lanes.lane(lane);
        let gap = lane.pacing.lock().gaps[lane.pace(class)];
        gap.current > gap.floor
    }

    /// A permit of the user's own key's budget, as the shipped lane gives it.
    pub async fn acquire(&self) -> RateLimitLease {
        let window = self
            .lanes
            .user
            .window
            .as_ref()
            .expect("the user's lane keeps its budget");
        if Self::in_background_now() {
            window.attempt_acquire()
        } else {
            window.acquire().await
        }
    }

    /// The current gap of a class's pace on a lane.
    pub fn gap(&self, lane: KeyLane, class: LookupClass) -> Duration {
        let lane = self.lanes.lane(lane);
        lane.pacing.lock().gaps[lane.pace(class)].current
    }

    /// A turn for one call, taken before the call's own timeout starts, so the wait is never
    /// counted against AcoustID. With `wait` false, in the background lane, or inside
    /// [`Self::if_free`], a turn that is not free at once is None, and nothing is queued. A heart's,
    /// a play's, a person's or a family request's call never waits while its pace is backed off
    /// ([`Self::backing_off`]): that gap grows to an hour, and the work that asked holds or takes
    /// its own busy path instead of keeping the download worker. On the user's lane a full queue is
    /// None too; Octo's own key's lane never refuses a call that waits.
    pub async fn permit(&self, lane: KeyLane, class: LookupClass, wait: bool) -> Option<CallPermit> {
        let if_free = Self::if_free_now();
        let wait = wait
            && !Self::in_background_now()
            && !if_free
            && (class.is_bulk() || !self.backing_off(lane, class));
        let shared = self.lanes.lane(lane);
        let entry = {
            let mut pacing = shared.pacing.lock();
            let ticket = pacing.next_ticket;
            pacing.next_ticket += 1;
            pacing.waiting.insert((class, ticket));
            (class, ticket)
        };
        let mut place = Place {
            lane: shared,
            entry,
            taken: false,
        };
        loop {
            let notified = shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let turn = {
                let mut pacing = shared.pacing.lock();
                let now = Instant::now();
                match shared.turn(&pacing, class, entry.1, now) {
                    Turn::Go => {
                        pacing.waiting.remove(&entry);
                        pacing.last_sent[shared.pace(class)] = Some(now);
                        pacing.in_flight += 1;
                        place.taken = true;
                        None
                    }
                    other => Some(other),
                }
            };
            match turn {
                None => break,
                Some(_) if !wait => {
                    if if_free {
                        Self::note_turned_away();
                    }
                    return None;
                }
                Some(Turn::At(at)) => {
                    tokio::select! {
                        _ = notified => {}
                        _ = tokio::time::sleep_until(at) => {}
                    }
                }
                Some(_) => notified.await,
            }
        }
        shared.changed.notify_waiters();
        let permit = CallPermit {
            limiter: Arc::clone(&self.lanes),
            lane,
            class,
            ended: false,
        };
        if let Some(window) = &shared.window {
            let lease = if wait {
                window.acquire().await
            } else {
                window.attempt_acquire()
            };
            if !lease.is_acquired() {
                if if_free {
                    Self::note_turned_away();
                }
                return None;
            }
        }
        Some(permit)
    }

    /// Holds a lookup on a lane until AcoustID answers a call there again: `wake` runs once, the
    /// oldest held first, after an answered call, or at the lane's person gap while nothing else
    /// is sent on it.
    pub fn hold(&self, lane: KeyLane, wake: Box<dyn FnOnce() + Send>) {
        self.lanes.hold(lane, wake);
    }
}

impl Default for AcoustIdRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "acoust_id_rate_limiter_tests.rs"]
mod tests;
