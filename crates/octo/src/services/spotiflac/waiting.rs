//! A song Mornye did not bring because its check is paused: the error every route recognizes.
//!
//! It is always context over a "no copy here" error (`io::ErrorKind::NotFound`), so a caller that
//! only knows that error still reads it as a source with no copy, and over another source's
//! failure when that source was asked after Mornye. `mornye_waited` finds it in either.

use chrono::{DateTime, Utc};
use octo_core::spotiflac::check::{CheckPause, gate_words, names_list, until_words};

/// Mornye's check keeps the song from Mornye: Octo did not ask (`asked: false`), or Mornye's job
/// ran into the check (`asked: true`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{shown}")]
pub struct MornyeWaiting {
    pub pause: CheckPause,
    pub asked: bool,
    /// The sentence a row shows.
    pub shown: String,
    /// What the log adds: when Mornye allows its next try.
    pub detail: String,
}

impl MornyeWaiting {
    pub fn new(pause: CheckPause, asked: bool, now: DateTime<Utc>) -> MornyeWaiting {
        let (gate, detail) = gate_words(&pause, now);
        let shown = if asked {
            format!(
                "Mornye could not pass its check for {}, so it could not get this song.",
                names_list(pause.shared())
            )
        } else {
            gate
        };
        MornyeWaiting {
            pause,
            asked,
            shown,
            detail,
        }
    }

    /// The wait as an error: context over a "no copy here" error.
    pub fn into_error(self) -> anyhow::Error {
        let words = self.shown.clone();
        anyhow::Error::from(std::io::Error::new(std::io::ErrorKind::NotFound, words)).context(self)
    }

    /// The wait laid over another source's failure, asked after Mornye.
    pub fn over(self, other: anyhow::Error) -> anyhow::Error {
        other.context(self)
    }

    /// When Mornye allows its next try, for a sentence.
    pub fn next_try(&self, now: DateTime<Utc>) -> String {
        until_words(self.pause.until_utc, now)
    }
}

/// The Mornye wait in `error`, when Mornye's check is why it failed.
pub fn mornye_waited(error: &anyhow::Error) -> Option<&MornyeWaiting> {
    error.downcast_ref::<MornyeWaiting>()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn pause() -> CheckPause {
        CheckPause {
            until_utc: Utc.with_ymd_and_hms(2026, 10, 30, 14, 5, 0).unwrap(),
            labels: vec!["example-ext".into()],
            holding: Vec::new(),
            url: String::new(),
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 30, 12, 0, 0).unwrap()
    }

    #[test]
    fn the_wait_reads_as_no_copy_and_as_the_wait() {
        let error = MornyeWaiting::new(pause(), false, now()).into_error();
        assert_eq!(
            error.to_string(),
            "Mornye could not pass its check for example-ext, so Octo did not ask it for this song."
        );
        assert!(mornye_waited(&error).is_some());
        assert_eq!(
            error.downcast_ref::<std::io::Error>().map(|e| e.kind()),
            Some(std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn a_job_that_ran_into_the_check_says_so() {
        let wait = MornyeWaiting::new(pause(), true, now());
        assert_eq!(
            wait.shown,
            "Mornye could not pass its check for example-ext, so it could not get this song."
        );
        assert_eq!(
            wait.detail,
            "It allows its next try at 14:05 UTC; the next song after that lets it try."
        );
    }

    #[test]
    fn the_wait_survives_under_another_source_s_failure() {
        let soulseek = anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No Soulseek FLAC found",
        ));
        let error = MornyeWaiting::new(pause(), false, now()).over(soulseek);
        assert!(mornye_waited(&error).is_some());
        assert_eq!(
            error.downcast_ref::<std::io::Error>().map(|e| e.kind()),
            Some(std::io::ErrorKind::NotFound)
        );
        assert!(mornye_waited(&anyhow::anyhow!("other")).is_none());
    }
}
