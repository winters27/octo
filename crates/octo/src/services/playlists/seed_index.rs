//! The playlist seed index: the routing each playlist entry was added with, by its registry id,
//! so an outside id a playlist lists still resolves after the registry let it go (a bound
//! entry's fallback, an id a phone cached long ago). Filled per key with the same keys and
//! revisions as `ExternalIdRegistry::set_kept`, behind its own lock (never a store mutex), and
//! asked on a registry lookup miss before the imports hook.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use octo_core::soulseek::SoulseekRouting;
use parking_lot::RwLock;

use crate::services::soulseek::external_id_registry::RoutingRecovery;

/// The key a held playlist's entries are kept and indexed under.
pub fn held_key(playlist_id: &str) -> String {
    format!("h{playlist_id}")
}

/// The key a playlist's `Replaced` record is kept and indexed under.
pub fn replaced_key(playlist_id: &str) -> String {
    format!("r{playlist_id}")
}

/// The key a family playlist's entries are kept and indexed under.
pub fn family_key(playlist_id: &str) -> String {
    format!("f{playlist_id}")
}

#[derive(Default)]
pub struct SeedIndex {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Each key's last revision and its seeds.
    by_key: HashMap<String, (u64, Vec<(String, Arc<SoulseekRouting>)>)>,
    /// Each id's holder count and its latest seed.
    by_id: HashMap<String, (usize, Arc<SoulseekRouting>)>,
}

impl SeedIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `key` hold exactly `seeds` as of `revision`; a call older than the key's last is
    /// ignored, and the last revision is remembered after the key empties. An id stays findable
    /// while any key holds it.
    pub fn set(&self, key: &str, revision: u64, seeds: Vec<(String, Arc<SoulseekRouting>)>) {
        let mut seen = HashSet::new();
        let mut unique: Vec<(String, Arc<SoulseekRouting>)> = seeds
            .into_iter()
            .rev()
            .filter(|(id, _)| !id.is_empty() && seen.insert(id.clone()))
            .collect();
        unique.reverse();
        let mut guard = self.inner.write();
        let inner = &mut *guard;
        if inner.by_key.get(key).is_some_and(|(last, _)| revision < *last) {
            return;
        }
        if let Some((_, old)) = inner.by_key.remove(key) {
            for (id, _) in old {
                if let Entry::Occupied(mut held) = inner.by_id.entry(id) {
                    held.get_mut().0 -= 1;
                    if held.get().0 == 0 {
                        held.remove();
                    }
                }
            }
        }
        for (id, seed) in &unique {
            let held = inner
                .by_id
                .entry(id.clone())
                .or_insert_with(|| (0, Arc::clone(seed)));
            held.0 += 1;
            held.1 = Arc::clone(seed);
        }
        inner.by_key.insert(key.to_string(), (revision, unique));
    }

    /// A copy of the seed `id` was added with, while any key holds it.
    pub fn find(&self, id: &str) -> Option<SoulseekRouting> {
        self.inner
            .read()
            .by_id
            .get(id)
            .map(|(_, seed)| SoulseekRouting::clone(seed))
    }

    /// How many keys hold `id`.
    pub fn holders(&self, id: &str) -> usize {
        self.inner.read().by_id.get(id).map_or(0, |(count, _)| *count)
    }

    /// The index as a registry recovery source (`ExternalIdRegistry::set_recovery_sources`).
    pub fn recovery(self: &Arc<Self>) -> RoutingRecovery {
        let index = Arc::clone(self);
        Arc::new(move |id: &str| index.find(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(title: &str) -> Arc<SoulseekRouting> {
        Arc::new(SoulseekRouting {
            artist: Some("Artist".into()),
            title: Some(title.into()),
            ..Default::default()
        })
    }

    #[test]
    fn an_id_stays_findable_while_any_key_holds_it() {
        let index = SeedIndex::new();
        index.set(
            &held_key("p1"),
            1,
            vec![("a".into(), seed("A")), ("b".into(), seed("B"))],
        );
        index.set(&family_key("f1"), 1, vec![("a".into(), seed("A2"))]);
        assert_eq!(index.holders("a"), 2);
        assert_eq!(index.find("a").and_then(|r| r.title).as_deref(), Some("A2"));
        index.set(&held_key("p1"), 2, vec![("b".into(), seed("B"))]);
        assert_eq!(index.holders("a"), 1);
        index.set(&family_key("f1"), 2, Vec::new());
        assert!(index.find("a").is_none());
        assert!(index.find("b").is_some());
    }

    #[test]
    fn an_older_revision_is_ignored_also_after_the_key_empties() {
        let index = SeedIndex::new();
        index.set(&replaced_key("p1"), 5, vec![("a".into(), seed("A"))]);
        index.set(&replaced_key("p1"), 4, Vec::new());
        assert!(index.find("a").is_some());
        index.set(&replaced_key("p1"), 6, Vec::new());
        index.set(&replaced_key("p1"), 5, vec![("a".into(), seed("A"))]);
        assert!(index.find("a").is_none());
    }

    #[test]
    fn a_key_naming_an_id_twice_holds_it_once_and_the_recovery_answers() {
        let index = Arc::new(SeedIndex::new());
        index.set(
            &held_key("p1"),
            1,
            vec![("a".into(), seed("A")), ("a".into(), seed("A again"))],
        );
        assert_eq!(index.holders("a"), 1);
        let recover = index.recovery();
        assert_eq!(recover("a").and_then(|r| r.title).as_deref(), Some("A again"));
        assert!(recover("zzz").is_none());
        assert_eq!(held_key("p1"), "hp1");
        assert_ne!(held_key("p1"), replaced_key("p1"));
        assert_ne!(held_key("p1"), family_key("p1"));
    }
}
