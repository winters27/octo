//! The Lidarr albums a heart or an upgrade has claimed.

use std::collections::{HashMap, HashSet};

use octo_core::common::dotnet;
use parking_lot::Mutex;
use tokio::sync::Notify;

/// Which Lidarr albums a heart or an upgrade is working on, by MusicBrainz release group id. The
/// two must not share an album: a heart moves every imported file into Octo's layout, which
/// would pull the file an upgrade is waiting on out from under it, and an upgrade deletes the
/// files its search brought in, which would take a heart's songs.
#[derive(Default)]
pub struct LidarrAlbumClaims {
    // Keyed ignoring case (ordinal case folding).
    hearts: Mutex<HashSet<String>>,
    upgrades: Mutex<HashMap<String, i32>>,
    /// Woken whenever an upgrade lets go of an album.
    released: Notify,
}

impl LidarrAlbumClaims {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn heart_started(&self, album_id: &str) {
        self.hearts
            .lock()
            .insert(dotnet::ordinal_ignore_case_key(album_id));
    }

    pub fn heart_ended(&self, album_id: &str) {
        self.hearts
            .lock()
            .remove(&dotnet::ordinal_ignore_case_key(album_id));
    }

    pub fn heart_busy(&self, album_id: &str) -> bool {
        self.hearts
            .lock()
            .contains(&dotnet::ordinal_ignore_case_key(album_id))
    }

    pub fn upgrade_started(&self, album_id: &str) {
        *self
            .upgrades
            .lock()
            .entry(dotnet::ordinal_ignore_case_key(album_id))
            .or_insert(0) += 1;
    }

    pub fn upgrade_ended(&self, album_id: &str) {
        let key = dotnet::ordinal_ignore_case_key(album_id);
        {
            let mut upgrades = self.upgrades.lock();
            // AddOrUpdate(albumId, 0, count - 1), then removed only while it reads 0.
            let count = upgrades.entry(key.clone()).and_modify(|c| *c -= 1).or_insert(0);
            if *count == 0 {
                upgrades.remove(&key);
            }
        }
        self.released.notify_waiters();
    }

    pub fn upgrade_busy(&self, album_id: &str) -> bool {
        self.upgrades
            .lock()
            .get(&dotnet::ordinal_ignore_case_key(album_id))
            .is_some_and(|count| *count > 0)
    }

    /// Waits until no upgrade borrows the album: ended by the last upgrade letting go of it, and
    /// never by a clock.
    pub async fn upgrades_let_go(&self, album_id: &str) {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if !self.upgrade_busy(album_id) {
                return;
            }
            released.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn claims_are_counted_per_album_ignoring_case() {
        let claims = LidarrAlbumClaims::new();
        claims.upgrade_started("RG-1");
        claims.upgrade_started("rg-1");
        claims.upgrade_ended("rg-1");
        assert!(claims.upgrade_busy("Rg-1"));
        claims.upgrade_ended("RG-1");
        assert!(!claims.upgrade_busy("rg-1"));
        // An end with no start leaves nothing behind.
        claims.upgrade_ended("rg-1");
        assert!(!claims.upgrade_busy("rg-1"));

        claims.heart_started("RG-2");
        assert!(claims.heart_busy("rg-2"));
        claims.heart_ended("rg-2");
        assert!(!claims.heart_busy("RG-2"));
    }

    /// A wait for an upgrade to let go has no clock of its own: with the clock seven hours on it
    /// still waits, and the upgrade's end ends it.
    #[tokio::test(start_paused = true)]
    async fn a_wait_for_an_upgrade_ends_only_when_it_lets_go() {
        let claims = Arc::new(LidarrAlbumClaims::new());
        claims.upgrade_started("rg-1");
        claims.upgrade_started("rg-1");
        let waiting = {
            let claims = claims.clone();
            tokio::spawn(async move { claims.upgrades_let_go("RG-1").await })
        };

        tokio::time::advance(Duration::from_secs(7 * 60 * 60)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "a claim outlives six hours");
        claims.upgrade_ended("rg-1");
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "one upgrade still borrows it");

        claims.upgrade_ended("rg-1");
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the last upgrade's end ends the wait")
            .expect("the task");
        // Nothing borrows it: no wait at all.
        claims.upgrades_let_go("rg-1").await;
    }
}
