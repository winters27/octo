//! Looks again, once per start, at the songs whose video an older ranking picked. Each is
//! checked with the current pick as background work on the shim; a video the pick agrees with is
//! marked as picked by the current ranking, and a different pick is only noted on the song
//! (`pending_video`), to be applied when a play of the song next starts at its first byte, so no
//! player is ever given the bytes of two videos.
//!
//! A check the shim could not answer leaves the pin unmarked, so the next start looks at it
//! again. The shim being busy or unreachable, from either call, ends the pass, since every later
//! check would meet the same.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use octo_core::soulseek::{PlayBinding, SoulseekRouting};
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::you_tube_resolver::{Failure, Lookup, YouTubeHit, YouTubeResolver};
use crate::services::i_download_service::StreamPlay;
use crate::services::soulseek::ExternalIdRegistry;

/// Whether a request reads from the first byte on: no `Range`, or a single range `bytes=0-`
/// with no end. Only such a request may be given another video than the one a player already
/// has bytes of.
pub fn starts_at_first_byte(range: Option<&str>) -> bool {
    let Some(range) = range.map(str::trim).filter(|r| !r.is_empty()) else {
        return true;
    };
    range
        .strip_prefix("bytes=")
        .and_then(|spec| spec.split_once('-'))
        .is_some_and(|(from, to)| from.trim() == "0" && to.trim().is_empty())
}

/// What one stream request is to its play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayStart {
    /// The request starts a play. With `switch` it starts from the first byte, so the song may
    /// move to the video chosen or waiting for it; without, the play takes the video the song
    /// has.
    Start { switch: bool },
    /// The request goes on with a play already bound to its video.
    Continue,
    /// A look at the song (a HEAD, or a scout read): it starts nothing.
    Peek,
}

/// Whether a Range asks from the first byte: none, blank, or any `bytes=0-...`.
fn from_byte_zero(range: Option<&str>) -> bool {
    match range.map(str::trim) {
        None | Some("") => true,
        Some(range) => range
            .get(..8)
            .is_some_and(|start| start.eq_ignore_ascii_case("bytes=0-")),
    }
}

/// Whether a Range is AVPlayer's opening two-byte read, `bytes=0-1`.
fn opening_probe(range: Option<&str>) -> bool {
    let Some(range) = range.map(str::trim) else {
        return false;
    };
    let Some(spec) = range
        .get(..6)
        .filter(|unit| unit.eq_ignore_ascii_case("bytes="))
        .map(|_| &range[6..])
    else {
        return false;
    };
    spec.split_once('-')
        .is_some_and(|(from, to)| from.trim() == "0" && to.trim() == "1")
}

/// The one rule for whether a stream request starts a play, used for the switch to a chosen or
/// waiting video, for the play's binding and for the file a play of an Octo app keeps.
///
/// A HEAD or a scout read only looks. A request with a play token goes on with the binding of
/// that token, and a new token starts a play (switching from the first byte only). With no token,
/// an unbound request starts a play, switching from the first byte; a bound one goes on, unless
/// it asks from the first byte with no end (or has no Range), or is AVPlayer's opening
/// `bytes=0-1`, which start the song again. A bound Octo app that sends no token (an older one)
/// goes on, since its own scouts read the song the way a restart does, until its play is over:
/// once the binding is `finished` (as the registry hands it back: a body reached the last byte and
/// none streams now), a read from the first byte with no end starts the song again. An offline
/// read (`X-Octo-Purpose: offline`) is never a scout and takes the plain rule.
pub fn play_start(play: &StreamPlay, range: Option<&str>, binding: Option<&PlayBinding>) -> PlayStart {
    if play.peeks() {
        return PlayStart::Peek;
    }
    if let Some(token) = play.token.as_deref() {
        if binding.is_some_and(|bound| bound.token.as_deref() == Some(token)) {
            return PlayStart::Continue;
        }
        return PlayStart::Start {
            switch: from_byte_zero(range),
        };
    }
    let Some(bound) = binding else {
        return PlayStart::Start {
            switch: from_byte_zero(range),
        };
    };
    if play.octo_device && play.purpose.as_deref() != Some("offline") {
        if bound.finished && starts_at_first_byte(range) {
            return PlayStart::Start { switch: true };
        }
        return PlayStart::Continue;
    }
    let restarts =
        starts_at_first_byte(range) || (opening_probe(range) && play.user_agent.contains("AppleCoreMedia"));
    if restarts {
        PlayStart::Start { switch: true }
    } else {
        PlayStart::Continue
    }
}

/// What one pass came to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassReport {
    /// Songs looked at.
    pub checked: usize,
    /// Pins the current pick agrees with, or that nothing better exists for now.
    pub kept: usize,
    /// Pins the current pick disagrees with: a better video is waiting for a play.
    pub better: usize,
    /// Checks the shim could not answer.
    pub failed: usize,
}

/// What one song's check came to.
enum Checks {
    Kept,
    Better,
    /// The shim answered this song oddly (a timeout, another status, not its JSON); the next
    /// song may still be asked.
    Failed,
    /// The shim was busy or unreachable, so the pass ends.
    Stop,
    /// The song changed under the check; nothing to record.
    Moved,
}

pub struct PinRecheck {
    registry: Arc<ExternalIdRegistry>,
    resolver: Arc<YouTubeResolver>,
    /// How long to wait between two songs, so the pass never crowds the shim.
    pause: Duration,
}

impl PinRecheck {
    /// How long a pass waits between two songs unless told otherwise.
    pub const PAUSE: Duration = Duration::from_secs(1);

    pub fn new(registry: Arc<ExternalIdRegistry>, resolver: Arc<YouTubeResolver>) -> Self {
        PinRecheck {
            registry,
            resolver,
            pause: Self::PAUSE,
        }
    }

    /// The wait between two songs.
    pub fn with_pause(mut self, pause: Duration) -> Self {
        self.pause = pause;
        self
    }

    /// One pass over the stale pins, as the worker; ends when the pass is done or `token` is.
    /// A busy or unreachable shim from either call (YouTube Music or `/search`) leaves that pin
    /// unstamped and ends the pass; it starts again at the next start. Any other failure (a
    /// timeout, another status, a non-JSON answer) leaves that pin unstamped, counts it as failed
    /// and goes on to the next pin. A failed YouTube Music check never falls back to `/search`.
    pub async fn run(self: Arc<Self>, token: CancellationToken) -> anyhow::Result<()> {
        let report = self.pass(&token).await;
        info!(
            "Pin recheck: {} checked, {} kept, {} with a better video, {} not answered",
            report.checked, report.kept, report.better, report.failed
        );
        Ok(())
    }

    /// Walks the stale pins one at a time, each at most once.
    pub async fn pass(&self, token: &CancellationToken) -> PassReport {
        let mut report = PassReport::default();
        let mut checked: HashSet<String> = HashSet::new();
        for id in self.registry.stale_pins() {
            if token.is_cancelled() {
                break;
            }
            if !checked.insert(id.clone()) {
                continue;
            }
            let outcome = tokio::select! {
                _ = token.cancelled() => break,
                outcome = self.check(&id) => outcome,
            };
            match outcome {
                Checks::Kept => {
                    report.checked += 1;
                    report.kept += 1;
                }
                Checks::Better => {
                    report.checked += 1;
                    report.better += 1;
                }
                Checks::Failed => {
                    report.checked += 1;
                    report.failed += 1;
                }
                Checks::Stop => {
                    report.checked += 1;
                    report.failed += 1;
                    break;
                }
                Checks::Moved => {}
            }
            if !self.pause.is_zero() {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(self.pause) => {}
                }
            }
        }
        report
    }

    async fn check(&self, id: &str) -> Checks {
        let Some(routing) = self.registry.lookup(id) else {
            return Checks::Moved;
        };
        let r = routing.snapshot();
        let (Some(pin), Some(artist), Some(title)) = (
            r.you_tube_id.clone().filter(|v| !v.is_empty()),
            r.artist.clone(),
            r.title.clone(),
        ) else {
            return Checks::Moved;
        };
        if r.video_pick >= SoulseekRouting::VIDEO_PICK {
            return Checks::Moved;
        }

        let hit = match self
            .resolver
            .music_song_checked(&artist, &title, r.duration)
            .await
        {
            Lookup::Found(hit) if !r.refuses_video(&hit.video_id) => Some(hit),
            // YouTube Music answered with nothing, or with a video the song refuses: the shim's
            // ranked search decides.
            Lookup::Found(_) | Lookup::None => {
                match self
                    .resolver
                    .search_ranked(&format!("{artist} {title}"), r.duration, &r.refused_videos)
                    .await
                {
                    Lookup::Found(hit) if !r.refuses_video(&hit.video_id) => Some(hit),
                    Lookup::Found(_) | Lookup::None => None,
                    Lookup::Failed(Failure::Other) => return Checks::Failed,
                    // Only a fresh lookup answers gone or refused; here they fail the check as
                    // any other answer it cannot use does.
                    Lookup::Failed(Failure::Gone | Failure::Refused) => return Checks::Failed,
                    Lookup::Failed(Failure::Busy | Failure::Unreachable) => return Checks::Stop,
                }
            }
            Lookup::Failed(Failure::Busy | Failure::Unreachable) => return Checks::Stop,
            Lookup::Failed(Failure::Other) => return Checks::Failed,
            Lookup::Failed(Failure::Gone | Failure::Refused) => return Checks::Failed,
        };

        match hit {
            Some(YouTubeHit { video_id, .. }) if video_id != pin => {
                if self.registry.set_pending_video(id, &pin, &video_id) {
                    Checks::Better
                } else {
                    Checks::Moved
                }
            }
            // The pick agrees, or nothing better exists now: the pin stands.
            _ => {
                self.registry.stamp_pick(id, &pin);
                Checks::Kept
            }
        }
    }
}

#[cfg(test)]
#[path = "pin_recheck_tests.rs"]
mod tests;
