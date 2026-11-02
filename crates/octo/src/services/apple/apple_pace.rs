//! One pace for every request Octo sends to Apple's iTunes Search API and chart feed.
//!
//! Waiters queue by class, Page above Row above Cover above Background, first come first served
//! within a class. A class shares its turns with the classes below it: after
//! [`PaceConfig::share_turns`] turns in a row to one class while a lower class waits, the next turn
//! goes to the next lower class waiting, which shares its own turns the same way. So Page keeps at
//! least four of every five turns and every class is served within a bounded number of turns. The
//! best waiter that may go now goes: one held for a token never blocks one that needs none.
//!
//! Catalog turns (Page, Row, Background) take a token from one bucket, Apple's documented rate of
//! about 20 a minute. Cover turns (the iTunes cover lookup) take a token only while Octo answers
//! Deezer's searches from Apple ([`ApplePace::set_strict`]); otherwise they keep the cover lookup's
//! own spacing alone, as before this pace existed. Every turn waits at least [`PaceConfig::spacing`]
//! after the one before, and a Cover turn [`PaceConfig::cover_spacing`] after the last Cover turn;
//! after Apple refuses a request (403 or 429), both spacings are at least the backoff, which doubles
//! on each refusal (to [`PaceConfig::backoff_max`]) and halves after
//! [`PaceConfig::answers_to_halve`] answers in a row.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::time::Instant;

/// Who is waiting for Apple, in priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppleClass {
    /// A catalog answer someone is waiting for on a page.
    Page,
    /// A search row's details while a search waits.
    Row,
    /// The iTunes cover lookup.
    Cover,
    /// Catalog work no one is waiting for.
    Background,
}

impl AppleClass {
    fn rank(self) -> u8 {
        match self {
            Self::Page => 0,
            Self::Row => 1,
            Self::Cover => 2,
            Self::Background => 3,
        }
    }
}

/// The pace refused a waiter: its class's queue was full and it was the oldest waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

/// The pace's numbers. Every one is settable, so a test runs in milliseconds.
#[derive(Debug, Clone)]
pub struct PaceConfig {
    /// The least time between any two turns.
    pub spacing: Duration,
    /// The least time between two Cover turns, and the floor a refusal's backoff doubles from.
    pub cover_spacing: Duration,
    /// Tokens the bucket holds when full.
    pub bucket_capacity: u32,
    /// One token comes back each time this passes.
    pub refill_every: Duration,
    /// The longest backoff.
    pub backoff_max: Duration,
    /// Answers in a row that halve the backoff.
    pub answers_to_halve: u32,
    /// Turns in a row one class takes while a lower class waits, before it gives that class one.
    pub share_turns: u32,
    /// Row waiters at most; one more refuses the oldest.
    pub row_cap: usize,
    /// Background waiters at most; one more refuses the oldest.
    pub background_cap: usize,
}

impl PaceConfig {
    /// A pace that never makes a turn wait (a test's own).
    pub fn unpaced() -> Self {
        Self {
            spacing: Duration::ZERO,
            cover_spacing: Duration::ZERO,
            bucket_capacity: 1000,
            refill_every: Duration::from_millis(1),
            ..Self::default()
        }
    }
}

impl Default for PaceConfig {
    fn default() -> Self {
        Self {
            spacing: Duration::from_millis(500),
            cover_spacing: Duration::from_secs(1),
            bucket_capacity: 6,
            refill_every: Duration::from_secs(3),
            backoff_max: Duration::from_secs(8),
            answers_to_halve: 50,
            share_turns: 4,
            row_cap: 8,
            background_cap: 32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    Waiting,
    Granted,
    Refused,
}

#[derive(Debug)]
struct Waiter {
    id: u64,
    /// The class it asked as: caps count and refuse by it.
    class: AppleClass,
    /// Where it waits: its class's rank, or a higher class's when a joiner raised it.
    rank: u8,
    standing: Standing,
}

struct State {
    config: PaceConfig,
    strict: bool,
    tokens: u32,
    refilled_at: Option<Instant>,
    next_any: Option<Instant>,
    next_cover: Option<Instant>,
    backoff: Duration,
    answered: u32,
    waiters: Vec<Waiter>,
    next_id: u64,
    /// Turns in a row each rank has taken while a lower rank waited.
    streaks: [u32; 4],
}

/// What one look at the queue came to.
enum Look {
    /// Nothing can go before this time (or anything that wakes the queue).
    WaitUntil(Instant),
    /// Nothing is waiting that could ever go without something new happening.
    Idle,
}

impl State {
    fn new(config: PaceConfig) -> Self {
        let tokens = config.bucket_capacity;
        Self {
            config,
            strict: false,
            tokens,
            refilled_at: None,
            next_any: None,
            next_cover: None,
            backoff: Duration::ZERO,
            answered: 0,
            waiters: Vec::new(),
            next_id: 0,
            streaks: [0; 4],
        }
    }

    fn needs_token(&self, class: AppleClass) -> bool {
        class != AppleClass::Cover || self.strict
    }

    fn refill(&mut self, now: Instant) {
        let capacity = self.config.bucket_capacity;
        let Some(since) = self.refilled_at else {
            self.refilled_at = Some(now);
            return;
        };
        if self.tokens >= capacity || self.config.refill_every.is_zero() {
            self.tokens = capacity;
            self.refilled_at = Some(now);
            return;
        }
        let elapsed = now.saturating_duration_since(since);
        let earned = (elapsed.as_nanos() / self.config.refill_every.as_nanos()) as u64;
        if earned == 0 {
            return;
        }
        self.tokens = capacity.min(
            self.tokens
                .saturating_add(u32::try_from(earned).unwrap_or(u32::MAX)),
        );
        self.refilled_at = if self.tokens >= capacity {
            Some(now)
        } else {
            Some(since + self.config.refill_every * u32::try_from(earned).unwrap_or(u32::MAX))
        };
    }

    fn next_token_at(&self, now: Instant) -> Instant {
        match self.refilled_at {
            Some(since) => since + self.config.refill_every,
            None => now,
        }
    }

    fn register(&mut self, class: AppleClass) -> (u64, bool) {
        let cap = match class {
            AppleClass::Row => Some(self.config.row_cap),
            AppleClass::Background => Some(self.config.background_cap),
            AppleClass::Page | AppleClass::Cover => None,
        };
        let mut refused_one = false;
        if let Some(cap) = cap {
            let waiting = self
                .waiters
                .iter()
                .filter(|w| w.class == class && w.standing == Standing::Waiting)
                .count();
            if waiting >= cap
                && let Some(oldest) = self
                    .waiters
                    .iter_mut()
                    .filter(|w| w.class == class && w.standing == Standing::Waiting)
                    .min_by_key(|w| w.id)
            {
                oldest.standing = Standing::Refused;
                refused_one = true;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.waiters.push(Waiter {
            id,
            class,
            rank: class.rank(),
            standing: Standing::Waiting,
        });
        (id, refused_one)
    }

    /// Grants the best waiter that may go at `now`, if any. Returns whether one was granted and,
    /// when none could go, when to look again.
    fn look(&mut self, now: Instant) -> (bool, Look) {
        self.refill(now);
        if let Some(at) = self.next_any
            && now < at
        {
            return (false, Look::WaitUntil(at));
        }
        // The waiters able to go now; for the others, when they could.
        let mut able: Vec<usize> = Vec::new();
        let mut wake: Option<Instant> = None;
        for at in 0..self.waiters.len() {
            if self.waiters[at].standing != Standing::Waiting {
                continue;
            }
            let class = self.waiters[at].class;
            if class == AppleClass::Cover
                && let Some(cover_at) = self.next_cover
                && now < cover_at
            {
                wake = Some(wake.map_or(cover_at, |w| w.min(cover_at)));
                continue;
            }
            if self.needs_token(class) && self.tokens == 0 {
                let token_at = self.next_token_at(now);
                wake = Some(wake.map_or(token_at, |w| w.min(token_at)));
                continue;
            }
            able.push(at);
        }
        let Some(mut rank) = able.iter().map(|&at| self.waiters[at].rank).min() else {
            return (false, wake.map_or(Look::Idle, Look::WaitUntil));
        };
        // A rank that has had its run while a lower rank waits gives the next lower rank a turn,
        // and that rank shares its own turns the same way.
        let share = self.config.share_turns.max(1);
        loop {
            let lower = able
                .iter()
                .map(|&at| self.waiters[at].rank)
                .filter(|other| *other > rank)
                .min();
            let level = usize::from(rank.min(3));
            match lower {
                Some(lower) if self.streaks[level] >= share => {
                    self.streaks[level] = 0;
                    rank = lower;
                }
                Some(_) => {
                    self.streaks[level] += 1;
                    break;
                }
                None => break,
            }
        }
        let chosen = able
            .into_iter()
            .filter(|&at| self.waiters[at].rank == rank)
            .min_by_key(|&at| self.waiters[at].id)
            .expect("a rank chosen from the able waiters has one");
        self.grant(chosen, now);
        (true, Look::Idle)
    }

    fn grant(&mut self, at: usize, now: Instant) {
        let class = self.waiters[at].class;
        if self.needs_token(class) {
            self.tokens = self.tokens.saturating_sub(1);
        }
        let backoff = self.backoff;
        self.next_any = Some(now + self.config.spacing.max(backoff));
        if class == AppleClass::Cover {
            self.next_cover = Some(now + self.config.cover_spacing.max(backoff));
        }
        self.waiters[at].standing = Standing::Granted;
    }

    fn take(&mut self, id: u64) -> Option<Standing> {
        let at = self.waiters.iter().position(|w| w.id == id)?;
        let standing = self.waiters[at].standing;
        if standing != Standing::Waiting {
            self.waiters.remove(at);
        }
        Some(standing)
    }
}

/// One pace for Apple. [`ApplePace::shared`] is the process's; tests make their own.
pub struct ApplePace {
    state: Mutex<State>,
    changed: Notify,
}

static SHARED: LazyLock<Arc<ApplePace>> = LazyLock::new(|| Arc::new(ApplePace::new(PaceConfig::default())));

/// Takes a waiter that is still waiting out of the queue when its turn is dropped.
struct Leave<'a> {
    pace: &'a ApplePace,
    id: u64,
    done: bool,
}

impl Drop for Leave<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.pace.leave(self.id);
    }
}

/// A place in the pace's queue (see [`ApplePace::ticket`]). Dropped before its turn, it leaves.
pub struct Ticket {
    pace: Arc<ApplePace>,
    id: u64,
}

impl Ticket {
    /// Moves this place up to `class` when that is higher than where it stands.
    pub fn raise(&self, class: AppleClass) {
        self.pace.raise(self.id, class);
    }

    /// Waits for this place's turn, as [`ApplePace::turn`] does.
    pub async fn wait(&self) -> Result<(), Refused> {
        self.pace.wait_registered(self.id).await
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.pace.leave(self.id);
    }
}

impl ApplePace {
    pub fn new(config: PaceConfig) -> Self {
        Self {
            state: Mutex::new(State::new(config)),
            changed: Notify::new(),
        }
    }

    /// The process's pace, shared by every Apple client.
    pub fn shared() -> Arc<ApplePace> {
        Arc::clone(&SHARED)
    }

    /// Takes a waiter out of the queue, if it is still in it.
    fn leave(&self, id: u64) {
        let mut state = self.state.lock();
        let Some(at) = state.waiters.iter().position(|w| w.id == id) else {
            return;
        };
        state.waiters.remove(at);
        drop(state);
        self.changed.notify_waiters();
    }

    /// Whether Octo is answering Deezer's searches from Apple: Cover turns then take tokens too.
    pub fn set_strict(&self, strict: bool) {
        self.state.lock().strict = strict;
        self.changed.notify_waiters();
    }

    pub fn strict(&self) -> bool {
        self.state.lock().strict
    }

    /// The least time between two Cover turns (a test sets it to zero).
    pub fn set_cover_spacing(&self, spacing: Duration) {
        self.state.lock().config.cover_spacing = spacing;
        self.changed.notify_waiters();
    }

    pub fn cover_spacing(&self) -> Duration {
        self.state.lock().config.cover_spacing
    }

    /// The backoff now added to every spacing, zero when Apple has not refused lately.
    pub fn backoff(&self) -> Duration {
        self.state.lock().backoff
    }

    /// Waits for this class's turn. Refused when the class's queue was full and this waiter was
    /// the oldest in it. Page and Cover are never refused.
    pub async fn turn(&self, class: AppleClass) -> Result<(), Refused> {
        let id = self.register(class);
        self.wait_registered(id).await
    }

    /// A place in the queue, taken now, that can be raised to a higher class while it waits: a
    /// request someone else joins goes at the joiner's class when that is higher.
    pub fn ticket(self: &Arc<Self>, class: AppleClass) -> Ticket {
        Ticket {
            id: self.register(class),
            pace: Arc::clone(self),
        }
    }

    fn register(&self, class: AppleClass) -> u64 {
        let (id, refused_one) = self.state.lock().register(class);
        if refused_one {
            self.changed.notify_waiters();
        }
        id
    }

    fn raise(&self, id: u64, class: AppleClass) {
        let mut state = self.state.lock();
        if let Some(waiter) = state
            .waiters
            .iter_mut()
            .find(|w| w.id == id && w.standing == Standing::Waiting)
            && class.rank() < waiter.rank
        {
            waiter.rank = class.rank();
        }
        drop(state);
        self.changed.notify_waiters();
    }

    async fn wait_registered(&self, id: u64) -> Result<(), Refused> {
        let mut leave = Leave {
            pace: self,
            id,
            done: false,
        };
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (granted_other, wake) = {
                let mut state = self.state.lock();
                match state.take(id) {
                    Some(Standing::Granted) => {
                        leave.done = true;
                        return Ok(());
                    }
                    Some(Standing::Refused) | None => {
                        leave.done = true;
                        return Err(Refused);
                    }
                    Some(Standing::Waiting) => {}
                }
                let (granted, look) = state.look(Instant::now());
                if granted {
                    match state.take(id) {
                        Some(Standing::Granted) => {
                            leave.done = true;
                            drop(state);
                            return Ok(());
                        }
                        _ => (true, None),
                    }
                } else {
                    match look {
                        Look::WaitUntil(at) => (false, Some(at)),
                        Look::Idle => (false, None),
                    }
                }
            };
            if granted_other {
                self.changed.notify_waiters();
                continue;
            }
            match wake {
                Some(at) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(at) => {}
                        () = &mut notified => {}
                    }
                }
                None => notified.await,
            }
        }
    }

    /// Apple refused a request (403 or 429): the backoff doubles from at least the cover spacing,
    /// to the maximum, and the bucket empties.
    pub fn refused_by_apple(&self) {
        let mut state = self.state.lock();
        let floor = state.config.cover_spacing;
        let doubled = (floor.max(state.backoff) * 2).min(state.config.backoff_max);
        state.backoff = doubled;
        state.answered = 0;
        state.tokens = 0;
        state.refilled_at = Some(Instant::now());
    }

    /// Apple answered a request: after enough answers in a row, the backoff halves (to nothing once
    /// it is no longer above the cover spacing).
    pub fn answered_by_apple(&self) {
        let mut state = self.state.lock();
        if state.backoff.is_zero() {
            return;
        }
        state.answered += 1;
        if state.answered >= state.config.answers_to_halve.max(1) {
            state.answered = 0;
            let floor = state.config.cover_spacing;
            state.backoff = if state.backoff / 2 <= floor {
                Duration::ZERO
            } else {
                state.backoff / 2
            };
        }
    }
}

#[cfg(test)]
#[path = "apple_pace_tests.rs"]
mod tests;
