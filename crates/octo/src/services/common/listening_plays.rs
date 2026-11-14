//! Plays whose download waits until they have been listened to (DownloadOnPlayAfterSeconds).
//!
//! Each player has at most one play waiting. The wait ends one of three ways: the time runs out
//! and the download starts; the player starts another song first, or scrobbles a skip, and the
//! download is dropped; or the player scrobbles the song as finished after the skip window, and
//! the download starts at once. Bytes streamed never count, since players buffer ahead.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Notify;
use tracing::info;

/// A finished scrobble this soon after the play started is a player scrobbling the skip, as
/// radio reads it.
pub const SKIP_WINDOW: Duration = Duration::from_secs(30);

/// The clock the waits run on: tokio's own, or in tests one moved by hand.
#[derive(Clone)]
enum ListeningClock {
    Tokio(tokio::time::Instant),
    #[cfg(test)]
    Hand(Arc<tokio::sync::watch::Sender<Duration>>),
}

impl ListeningClock {
    fn now(&self) -> Duration {
        match self {
            ListeningClock::Tokio(origin) => origin.elapsed(),
            #[cfg(test)]
            ListeningClock::Hand(time) => *time.borrow(),
        }
    }

    async fn sleep_until(&self, at: Duration) {
        match self {
            ListeningClock::Tokio(origin) => tokio::time::sleep_until(*origin + at).await,
            #[cfg(test)]
            ListeningClock::Hand(time) => {
                let mut moved = time.subscribe();
                let _ = moved.wait_for(|now| *now >= at).await;
            }
        }
    }
}

/// A clock a test moves by hand, in seconds.
#[cfg(test)]
#[derive(Clone)]
pub struct HandClock(Arc<tokio::sync::watch::Sender<Duration>>);

#[cfg(test)]
impl HandClock {
    pub fn advance(&self, seconds: u64) {
        self.0.send_modify(|now| *now += Duration::from_secs(seconds));
    }
}

struct Waiting {
    song: String,
    started: Duration,
    token: u64,
    wake: Arc<Notify>,
}

/// The plays waiting to be listened to, one per player.
pub struct ListeningPlays {
    waiting: Mutex<HashMap<String, Waiting>>,
    next: AtomicU64,
    clock: Mutex<ListeningClock>,
}

impl Default for ListeningPlays {
    fn default() -> Self {
        ListeningPlays {
            waiting: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            clock: Mutex::new(ListeningClock::Tokio(tokio::time::Instant::now())),
        }
    }
}

/// One play's wait, awaited by the task that starts its download.
pub struct ListeningTicket {
    plays: Arc<ListeningPlays>,
    player: String,
    token: u64,
    until: Duration,
    wake: Arc<Notify>,
    clock: ListeningClock,
}

impl ListeningTicket {
    /// Waits out the play. True when its download should start now: the time ran out, or the
    /// player scrobbled it finished. False when it was dropped.
    pub async fn listened(self) -> bool {
        tokio::select! {
            () = self.clock.sleep_until(self.until) => {}
            () = self.wake.notified() => {}
        }
        let mut waiting = self.plays.waiting.lock();
        if waiting
            .get(&self.player)
            .is_some_and(|play| play.token == self.token)
        {
            waiting.remove(&self.player);
            true
        } else {
            false
        }
    }
}

impl ListeningPlays {
    /// Who is playing, on which device and in which app, as one key: the signed-in listener, the
    /// address the request came from and the app's `c`.
    pub fn player_key(who: &str, address: Option<IpAddr>, client: &str) -> String {
        format!(
            "{}|{}|{}",
            who.trim().to_lowercase(),
            address.map(|a| a.to_string()).unwrap_or_default(),
            client.trim().to_lowercase()
        )
    }

    /// Whether any play waits; with none, nothing a player does needs looking at.
    pub fn is_empty(&self) -> bool {
        self.waiting.lock().is_empty()
    }

    /// The song a player's play waits on, if any.
    pub fn waiting_song(&self, player: &str) -> Option<String> {
        self.waiting.lock().get(player).map(|play| play.song.clone())
    }

    /// Starts the wait of `song` on `player` for `wait`. None when the player already waits on
    /// this song: a play started again keeps the first play's time.
    pub fn hold(self: &Arc<Self>, player: &str, song: &str, wait: Duration) -> Option<ListeningTicket> {
        let clock = self.clock.lock().clone();
        let started = clock.now();
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        let wake = Arc::new(Notify::new());
        {
            let mut waiting = self.waiting.lock();
            if waiting.get(player).is_some_and(|play| play.song == song) {
                return None;
            }
            if let Some(before) = waiting.insert(
                player.to_string(),
                Waiting {
                    song: song.to_string(),
                    started,
                    token,
                    wake: Arc::clone(&wake),
                },
            ) {
                info!(
                    "Download on play: {} was not listened to long enough; another song started",
                    before.song
                );
                before.wake.notify_one();
            }
        }
        Some(ListeningTicket {
            plays: Arc::clone(self),
            player: player.to_string(),
            token,
            until: started + wait,
            wake,
            clock,
        })
    }

    /// The player started `song` (a stream from its first byte, or a now-playing scrobble): the
    /// wait of any other song on it is dropped.
    pub fn started(&self, player: &str, song: &str) {
        let dropped = {
            let mut waiting = self.waiting.lock();
            match waiting.get(player) {
                Some(play) if play.song != song => waiting.remove(player),
                _ => None,
            }
        };
        if let Some(play) = dropped {
            info!(
                "Download on play: {} was not listened to long enough; {song} started",
                play.song
            );
            play.wake.notify_one();
        }
    }

    /// The player scrobbled `song`, `finished` or as now playing. A finish within
    /// [`SKIP_WINDOW`] of the play's start is a skip and drops its wait; a later one ends the
    /// wait at once.
    pub fn scrobbled(&self, player: &str, song: &str, finished: bool) {
        if !finished {
            self.started(player, song);
            return;
        }
        let now = self.clock.lock().now();
        let mut waiting = self.waiting.lock();
        let Some(play) = waiting.get(player).filter(|play| play.song == song) else {
            return;
        };
        if now.saturating_sub(play.started) < SKIP_WINDOW {
            if let Some(play) = waiting.remove(player) {
                info!("Download on play: {song} was skipped");
                play.wake.notify_one();
            }
        } else {
            play.wake.notify_one();
        }
    }

    /// Runs the waits on a clock the test moves by hand.
    #[cfg(test)]
    pub fn hand_clock(&self) -> HandClock {
        let time = Arc::new(tokio::sync::watch::channel(Duration::ZERO).0);
        *self.clock.lock() = ListeningClock::Hand(Arc::clone(&time));
        HandClock(time)
    }
}

#[cfg(test)]
#[path = "listening_plays_tests.rs"]
mod tests;
