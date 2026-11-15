//! The bound on songs found online that playlists keep: at most [`PlaylistCaps::PERSON`] counted
//! entries a person and [`PlaylistCaps::SERVER`] on the server. Counted: a person's held entries
//! that stream with a registry id, and family entries they added with no library id and a
//! registry id. A change that would raise a count past its cap is refused; one that lowers or
//! keeps it always goes through.
//!
//! Lock order: playlist lock, owner cap lock ([`PlaylistCaps::owner_lock`]), the counts
//! ([`PlaylistCaps::lock`]), store mutex. Cap locks are plain mutexes, never held across an await,
//! a file write or a Navidrome call.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use octo_core::common::dotnet;
use parking_lot::{Mutex, MutexGuard};

/// Which cap a change would pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapRefusal {
    Person,
    Server,
}

impl CapRefusal {
    /// The words a refused add answers with (error 0).
    pub fn message(self) -> &'static str {
        match self {
            CapRefusal::Person => "Your playlists hold as many songs found online as Octo keeps.",
            CapRefusal::Server => "This server holds as many songs found online in playlists as Octo keeps.",
        }
    }
}

#[derive(Default)]
struct Counts {
    by_owner: HashMap<String, usize>,
    total: usize,
}

pub struct PlaylistCaps {
    person: AtomicUsize,
    server: AtomicUsize,
    counts: Mutex<Counts>,
    owners: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

/// The counts, held: every count-changing write works out its change while holding this.
pub struct CapCounts<'a> {
    caps: &'a PlaylistCaps,
    counts: MutexGuard<'a, Counts>,
}

impl PlaylistCaps {
    pub const PERSON: usize = 15_000;
    pub const SERVER: usize = 30_000;

    pub fn new() -> Self {
        Self::with_limits(Self::PERSON, Self::SERVER)
    }

    pub fn with_limits(person: usize, server: usize) -> Self {
        PlaylistCaps {
            person: AtomicUsize::new(person),
            server: AtomicUsize::new(server),
            counts: Mutex::new(Counts::default()),
            owners: Mutex::new(HashMap::new()),
        }
    }

    /// Tests: these bounds from now on; the counts are kept.
    #[cfg(test)]
    pub fn set_limits(&self, person: usize, server: usize) {
        self.person.store(person, Ordering::Relaxed);
        self.server.store(server, Ordering::Relaxed);
    }

    /// The owner cap lock of `owner` (one per person, ignoring case), taken by a write that
    /// raises that person's count, before [`Self::lock`].
    pub fn owner_lock(&self, owner: &str) -> Arc<Mutex<()>> {
        let mut owners = self.owners.lock();
        if owners.len() > 256 {
            owners.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        Arc::clone(owners.entry(owner_key(owner)).or_default())
    }

    /// The counts, held until the guard drops.
    pub fn lock(&self) -> CapCounts<'_> {
        CapCounts {
            caps: self,
            counts: self.counts.lock(),
        }
    }

    /// Sets every count from a full count (at start, after the family backfill).
    pub fn reset(&self, counts: HashMap<String, usize>) {
        let mut held = self.counts.lock();
        held.by_owner.clear();
        for (owner, count) in counts {
            if count > 0 {
                *held.by_owner.entry(owner_key(&owner)).or_default() += count;
            }
        }
        held.total = held.by_owner.values().sum();
    }

    /// Applies per-owner deltas; refuses only a rise past a cap.
    pub fn apply(&self, deltas: &[(String, i64)]) -> Result<(), CapRefusal> {
        self.lock().apply(deltas)
    }

    /// Applies per-owner deltas whatever they make the counts (merges and move-backs).
    pub fn force(&self, deltas: &[(String, i64)]) {
        self.lock().force(deltas);
    }

    pub fn count(&self, owner: &str) -> usize {
        self.lock().count(owner)
    }

    pub fn total(&self) -> usize {
        self.lock().total()
    }
}

impl Default for PlaylistCaps {
    fn default() -> Self {
        Self::new()
    }
}

impl CapCounts<'_> {
    /// Whether `deltas` may be applied: refused only when a count would rise and end past its
    /// cap, the person's checked first.
    pub fn check(&self, deltas: &[(String, i64)]) -> Result<(), CapRefusal> {
        let summed = summed(deltas);
        for (owner, delta) in &summed {
            let now = self.count(owner);
            let next = moved(now, *delta);
            if next > now && next > self.caps.person.load(Ordering::Relaxed) {
                return Err(CapRefusal::Person);
            }
        }
        let total = self.counts.total;
        let next = moved(total, summed.values().sum());
        if next > total && next > self.caps.server.load(Ordering::Relaxed) {
            return Err(CapRefusal::Server);
        }
        Ok(())
    }

    /// [`Self::check`], then [`Self::force`].
    pub fn apply(&mut self, deltas: &[(String, i64)]) -> Result<(), CapRefusal> {
        self.check(deltas)?;
        self.force(deltas);
        Ok(())
    }

    /// Applies `deltas` whatever they make the counts.
    pub fn force(&mut self, deltas: &[(String, i64)]) {
        for (owner, delta) in summed(deltas) {
            let now = self.count(&owner);
            let next = moved(now, delta);
            if next == 0 {
                self.counts.by_owner.remove(&owner);
            } else {
                self.counts.by_owner.insert(owner, next);
            }
            self.counts.total = self.counts.total + next - now;
        }
    }

    pub fn count(&self, owner: &str) -> usize {
        self.counts.by_owner.get(&owner_key(owner)).copied().unwrap_or(0)
    }

    pub fn total(&self) -> usize {
        self.counts.total
    }
}

fn owner_key(owner: &str) -> String {
    dotnet::ordinal_ignore_case_key(owner.trim())
}

/// The deltas summed per owner, ignoring case.
fn summed(deltas: &[(String, i64)]) -> HashMap<String, i64> {
    let mut summed: HashMap<String, i64> = HashMap::new();
    for (owner, delta) in deltas {
        *summed.entry(owner_key(owner)).or_default() += delta;
    }
    summed
}

/// `count` moved by `delta`, never below zero.
fn moved(count: usize, delta: i64) -> usize {
    let next = i64::try_from(count).unwrap_or(i64::MAX).saturating_add(delta);
    usize::try_from(next.max(0)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(owner: &str, by: i64) -> (String, i64) {
        (owner.to_string(), by)
    }

    #[test]
    fn a_rise_past_the_persons_cap_is_refused_with_its_words() {
        let caps = PlaylistCaps::with_limits(3, 100);
        assert_eq!(caps.apply(&[delta("alice", 3)]), Ok(()));
        assert_eq!(caps.apply(&[delta("Alice", 1)]), Err(CapRefusal::Person));
        assert_eq!(caps.count("ALICE"), 3);
        assert_eq!(
            CapRefusal::Person.message(),
            "Your playlists hold as many songs found online as Octo keeps."
        );
    }

    #[test]
    fn a_rise_past_the_servers_cap_is_refused_with_its_own_words() {
        let caps = PlaylistCaps::with_limits(10, 4);
        assert_eq!(caps.apply(&[delta("alice", 2), delta("bob", 2)]), Ok(()));
        assert_eq!(caps.apply(&[delta("carol", 1)]), Err(CapRefusal::Server));
        assert_eq!(caps.total(), 4);
        assert_eq!(
            CapRefusal::Server.message(),
            "This server holds as many songs found online in playlists as Octo keeps."
        );
    }

    #[test]
    fn lowering_or_keeping_a_count_always_goes_through_even_past_a_cap() {
        let caps = PlaylistCaps::with_limits(3, 100);
        caps.force(&[delta("alice", 5)]);
        assert_eq!(caps.apply(&[delta("alice", -1)]), Ok(()));
        assert_eq!(caps.apply(&[delta("alice", 1), delta("ALICE", -1)]), Ok(()));
        assert_eq!(caps.count("alice"), 4);
        assert_eq!(caps.apply(&[delta("alice", -10)]), Ok(()));
        assert_eq!((caps.count("alice"), caps.total()), (0, 0));
    }

    #[test]
    fn reset_counts_ignoring_case_and_the_guard_checks_before_it_applies() {
        let caps = PlaylistCaps::with_limits(5, 6);
        caps.reset(HashMap::from([
            ("Alice".to_string(), 2),
            ("alice".to_string(), 1),
            ("bob".to_string(), 3),
        ]));
        assert_eq!((caps.count("alice"), caps.total()), (3, 6));
        let mut counts = caps.lock();
        assert_eq!(counts.check(&[delta("bob", 1)]), Err(CapRefusal::Server));
        assert_eq!(counts.apply(&[delta("bob", -1), delta("alice", 1)]), Ok(()));
        drop(counts);
        assert_eq!((caps.count("alice"), caps.count("bob"), caps.total()), (4, 2, 6));
    }

    #[test]
    fn one_owner_lock_per_person_ignoring_case() {
        let caps = PlaylistCaps::new();
        assert!(Arc::ptr_eq(
            &caps.owner_lock("alice"),
            &caps.owner_lock(" ALICE ")
        ));
        assert!(!Arc::ptr_eq(&caps.owner_lock("alice"), &caps.owner_lock("bob")));
    }

    #[test]
    fn new_limits_apply_from_now_on_and_keep_the_counts() {
        let caps = PlaylistCaps::with_limits(5, 10);
        assert_eq!(caps.apply(&[delta("alice", 2)]), Ok(()));
        caps.set_limits(2, 10);
        assert_eq!(caps.apply(&[delta("alice", 1)]), Err(CapRefusal::Person));
        caps.set_limits(10, 2);
        assert_eq!(caps.apply(&[delta("bob", 1)]), Err(CapRefusal::Server));
        assert_eq!((caps.count("alice"), caps.total()), (2, 2));
    }
}
