//! Which bytes one play of an outside song gets: the ready FLAC or the preview. A play keeps the
//! source it started on. Octo's apps keep what they streamed under the song's id, and a song
//! partly heard as the preview is later played on from where that stopped (`Range: bytes=N-`):
//! FLAC bytes there would follow the preview's in one file. So a request from the first byte
//! decides, and every later range of that play follows it.
//!
//! A play is one device's: the listener, the address and the app together. A play on a file
//! keeps that very file for as long as it exists, a pause of any length included; when the file
//! is gone, its later ranges are refused, so the player starts the song again from its first
//! byte rather than mixing two files. A play on the preview keeps it while it is heard, each
//! request renewing the song's length and ten minutes.
//!
//! Beside them, every play of an outside song notes the kind of answer it started with (the
//! source as it is, or a converted copy), so its later ranges and time seeks keep that kind,
//! for as long as it is heard by the same rule.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use octo_core::common::Clock;
use parking_lot::Mutex;

/// How long a preview play is kept beyond the song's own length, from its last request.
const GRACE: Duration = Duration::minutes(10);
/// The length assumed for a song whose length is not known.
const UNKNOWN_LENGTH: Duration = Duration::minutes(10);
/// Plays remembered at most; the oldest go first.
const MAX_PLAYS: usize = 10_000;

/// What one request of a play gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// This file, the one the play started on.
    File(PathBuf),
    /// The preview from the song's source.
    Preview,
    /// The file the play started on is gone: the player has to start the song again.
    Gone,
}

#[derive(Debug, Clone)]
enum Started {
    File(PathBuf),
    Preview,
}

type Plays = HashMap<(String, String), (Started, DateTime<Utc>)>;

/// The kind of answer a play of an outside song started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answered {
    /// The source as it is.
    Source,
    /// The converted copy of this key.
    Conversion(String),
}

type Answers = HashMap<(String, String), (Answered, DateTime<Utc>)>;

/// The source each device's play of each song started on.
pub struct PlaySources {
    clock: Clock,
    plays: Mutex<Plays>,
    answers: Mutex<Answers>,
}

impl PlaySources {
    pub fn new(clock: Clock) -> Self {
        PlaySources {
            clock,
            plays: Mutex::new(HashMap::new()),
            answers: Mutex::new(HashMap::new()),
        }
    }

    /// Whether a play of `id` from the device `keys` names is being heard now: its last
    /// request was less than the song's length and ten minutes ago.
    pub fn live(&self, keys: &[String], id: &str) -> bool {
        self.answered(keys, id).is_some()
    }

    /// The kind of answer the live play of `id` from the device `keys` names started with.
    pub fn answered(&self, keys: &[String], id: &str) -> Option<Answered> {
        let now = self.clock.now();
        let answers = self.answers.lock();
        keys.iter()
            .filter_map(|key| answers.get(&(key.clone(), id.to_string())))
            .find(|(_, until)| *until > now)
            .map(|(answered, _)| answered.clone())
    }

    /// Notes a request of a play of `id`: a new play under the device's first key, or the live
    /// play renewed under the key it started with, either way kept for the song's length and
    /// ten minutes from now, with the kind of answer it has.
    pub fn note(&self, keys: &[String], id: &str, length_seconds: Option<i32>, answered: Answered) {
        let Some(first) = keys.first() else {
            return;
        };
        let now = self.clock.now();
        let length = length_seconds
            .filter(|s| *s > 0)
            .map_or(UNKNOWN_LENGTH, |s| Duration::seconds(i64::from(s)));
        let until = now + length + GRACE;
        let mut answers = self.answers.lock();
        let key = keys
            .iter()
            .map(|key| (key.clone(), id.to_string()))
            .find(|key| answers.get(key).is_some_and(|(_, kept)| *kept > now))
            .unwrap_or_else(|| (first.clone(), id.to_string()));
        if !answers.contains_key(&key) && answers.len() >= MAX_PLAYS {
            trim_answers(&mut answers, now);
        }
        answers.insert(key, (answered, until));
    }

    /// The source for one request for `id` from the device `who` names: `who[0]` is the play's
    /// key, and any after it are keys the same play may have started under (the address alone,
    /// when the listener could not be named then). A request from the first byte gets `ready`
    /// when there is one, and the preview otherwise; a later range gets what its play started
    /// on. With `remember`, a request from the first byte starts a play.
    pub fn choose(
        &self,
        who: &[String],
        id: &str,
        from_start: bool,
        ready: Option<&Path>,
        length_seconds: Option<i32>,
        remember: bool,
    ) -> Source {
        let now = self.clock.now();
        let length = length_seconds
            .filter(|s| *s > 0)
            .map_or(UNKNOWN_LENGTH, |s| Duration::seconds(i64::from(s)));
        let until = now + length + GRACE;
        let Some(first) = who.first() else {
            return Source::Preview;
        };
        let mut plays = self.plays.lock();
        if from_start {
            let (started, source) = match ready {
                Some(path) => (
                    Started::File(path.to_path_buf()),
                    Source::File(path.to_path_buf()),
                ),
                None => (Started::Preview, Source::Preview),
            };
            if remember {
                if plays.len() >= MAX_PLAYS {
                    trim(&mut plays, now);
                }
                plays.insert((first.clone(), id.to_string()), (started, until));
            }
            return source;
        }
        let Some(key) = who
            .iter()
            .map(|w| (w.clone(), id.to_string()))
            .find(|key| plays.contains_key(key))
        else {
            return Source::Preview;
        };
        let (started, kept_until) = plays[&key].clone();
        match started {
            Started::File(path) if path.is_file() => {
                plays.insert(key, (Started::File(path.clone()), until));
                Source::File(path)
            }
            Started::File(_) => {
                plays.remove(&key);
                Source::Gone
            }
            Started::Preview if kept_until > now => {
                plays.insert(key, (Started::Preview, until));
                Source::Preview
            }
            Started::Preview => {
                plays.remove(&key);
                Source::Preview
            }
        }
    }
}

/// Room for one more play: the plays over go (a preview past its time, a file gone), then the
/// play heard longest ago.
fn trim(plays: &mut Plays, now: DateTime<Utc>) {
    plays.retain(|_, (started, until)| match started {
        Started::File(path) => path.is_file(),
        Started::Preview => *until > now,
    });
    if plays.len() >= MAX_PLAYS
        && let Some(oldest) = plays
            .iter()
            .min_by_key(|(_, (_, until))| *until)
            .map(|(key, _)| key.clone())
    {
        plays.remove(&oldest);
    }
}

/// Room for one more answered play: the plays over go, then the play heard longest ago.
fn trim_answers(answers: &mut Answers, now: DateTime<Utc>) {
    answers.retain(|_, (_, until)| *until > now);
    if answers.len() >= MAX_PLAYS
        && let Some(oldest) = answers
            .iter()
            .min_by_key(|(_, (_, until))| *until)
            .map(|(key, _)| key.clone())
    {
        answers.remove(&oldest);
    }
}

/// Whether a Range header asks from the first byte: none, blank, or `bytes=0-`.
pub fn from_start(range: Option<&str>) -> bool {
    match range.map(str::trim) {
        None | Some("") => true,
        Some(range) => range
            .get(..8)
            .is_some_and(|start| start.eq_ignore_ascii_case("bytes=0-")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct World {
        plays: PlaySources,
        now: Arc<Mutex<DateTime<Utc>>>,
        dir: tempfile::TempDir,
    }

    fn world() -> World {
        let now = Arc::new(Mutex::new(Utc::now()));
        let clock_now = Arc::clone(&now);
        World {
            plays: PlaySources::new(Clock::new(move || *clock_now.lock())),
            now,
            dir: tempfile::tempdir().expect("a temp dir"),
        }
    }

    impl World {
        fn file(&self, name: &str) -> PathBuf {
            let path = self.dir.path().join(name);
            std::fs::write(&path, b"fLaC").expect("a file");
            path
        }

        fn later(&self, by: Duration) {
            *self.now.lock() += by;
        }
    }

    fn alice() -> Vec<String> {
        vec!["alice|10.0.0.2|octo".to_string()]
    }

    #[test]
    fn a_play_from_the_start_takes_the_file_when_one_is_ready() {
        let w = world();
        let file = w.file("a.flac");
        assert_eq!(
            w.plays.choose(&alice(), "s1", true, Some(&file), Some(200), true),
            Source::File(file)
        );
        assert_eq!(
            w.plays.choose(&alice(), "s2", true, None, Some(200), true),
            Source::Preview
        );
    }

    #[test]
    fn a_seek_after_the_file_lands_stays_on_the_preview() {
        let w = world();
        assert_eq!(
            w.plays.choose(&alice(), "s1", true, None, Some(200), true),
            Source::Preview
        );
        let file = w.file("a.flac");
        assert_eq!(
            w.plays
                .choose(&alice(), "s1", false, Some(&file), Some(200), true),
            Source::Preview
        );
    }

    #[test]
    fn a_later_range_with_no_play_started_on_the_file_gets_the_preview() {
        let w = world();
        let file = w.file("a.flac");
        assert_eq!(
            w.plays
                .choose(&alice(), "s1", false, Some(&file), Some(200), true),
            Source::Preview
        );
    }

    #[test]
    fn a_play_that_started_on_the_file_keeps_that_file_for_its_later_ranges() {
        let w = world();
        let file = w.file("a.flac");
        w.plays.choose(&alice(), "s1", true, Some(&file), Some(200), true);
        w.later(Duration::seconds(150));
        // Another file is ready under the song now (a heart kept its own copy): this play stays
        // on the one it started on.
        let other = w.file("b.flac");
        assert_eq!(
            w.plays
                .choose(&alice(), "s1", false, Some(&other), Some(200), true),
            Source::File(file)
        );
    }

    #[test]
    fn each_device_has_its_own_play() {
        let w = world();
        let file = w.file("a.flac");
        w.plays.choose(&alice(), "s1", true, Some(&file), Some(200), true);
        let phone = vec!["alice|10.0.0.3|octo".to_string()];
        assert_eq!(
            w.plays.choose(&phone, "s1", false, Some(&file), Some(200), true),
            Source::Preview
        );
    }

    #[test]
    fn a_pause_longer_than_the_song_and_ten_minutes_resumes_on_the_same_source() {
        let w = world();
        let file = w.file("a.flac");
        w.plays
            .choose(&alice(), "flac", true, Some(&file), Some(60), true);
        w.plays.choose(&alice(), "preview", true, None, Some(60), true);
        w.later(Duration::minutes(45));
        assert_eq!(
            w.plays
                .choose(&alice(), "flac", false, Some(&file), Some(60), true),
            Source::File(file.clone())
        );
        let landed = w.file("b.flac");
        assert_eq!(
            w.plays
                .choose(&alice(), "preview", false, Some(&landed), Some(60), true),
            Source::Preview
        );
    }

    #[test]
    fn every_request_renews_a_preview_play() {
        let w = world();
        w.plays.choose(&alice(), "s1", true, None, Some(60), true);
        let file = w.file("a.flac");
        for _ in 0..5 {
            w.later(Duration::minutes(8));
            assert_eq!(
                w.plays.choose(&alice(), "s1", false, Some(&file), Some(60), true),
                Source::Preview
            );
        }
    }

    #[test]
    fn a_play_whose_file_is_gone_is_told_to_start_again() {
        let w = world();
        let file = w.file("a.flac");
        w.plays.choose(&alice(), "s1", true, Some(&file), Some(200), true);
        std::fs::remove_file(&file).expect("gone");
        assert_eq!(
            w.plays.choose(&alice(), "s1", false, None, Some(200), true),
            Source::Gone
        );
    }

    #[test]
    fn a_new_play_from_the_start_decides_again() {
        let w = world();
        w.plays.choose(&alice(), "s1", true, None, Some(200), true);
        let file = w.file("a.flac");
        assert_eq!(
            w.plays.choose(&alice(), "s1", true, Some(&file), Some(200), true),
            Source::File(file.clone())
        );
        assert_eq!(
            w.plays
                .choose(&alice(), "s1", false, Some(&file), Some(200), true),
            Source::File(file)
        );
    }

    #[test]
    fn a_play_started_without_a_name_is_found_by_its_address() {
        let w = world();
        let file = w.file("a.flac");
        let address = vec!["|10.0.0.2|octo".to_string()];
        w.plays.choose(&address, "s1", true, Some(&file), Some(200), true);
        let named = vec!["alice|10.0.0.2|octo".to_string(), "|10.0.0.2|octo".to_string()];
        assert_eq!(
            w.plays.choose(&named, "s1", false, Some(&file), Some(200), true),
            Source::File(file)
        );
    }

    #[test]
    fn a_look_that_is_not_a_play_remembers_nothing() {
        let w = world();
        let file = w.file("a.flac");
        assert_eq!(
            w.plays
                .choose(&alice(), "s1", true, Some(&file), Some(200), false),
            Source::File(file.clone())
        );
        assert_eq!(
            w.plays
                .choose(&alice(), "s1", false, Some(&file), Some(200), true),
            Source::Preview
        );
    }

    #[test]
    fn ranges_from_the_first_byte_are_told_apart() {
        for (range, start) in [
            (None, true),
            (Some(""), true),
            (Some("bytes=0-"), true),
            (Some("BYTES=0-1023"), true),
            (Some("bytes=500000-"), false),
            (Some("bytes=-500"), false),
        ] {
            assert_eq!(from_start(range), start, "{range:?}");
        }
    }

    #[test]
    fn a_noted_play_is_live_with_its_kind_until_its_time_runs_out() {
        let w = world();
        assert!(!w.plays.live(&alice(), "s1"));
        w.plays.note(
            &alice(),
            "s1",
            Some(60),
            Answered::Conversion("v-a-1.mp3-128".to_string()),
        );
        assert!(w.plays.live(&alice(), "s1"));
        assert_eq!(
            w.plays.answered(&alice(), "s1"),
            Some(Answered::Conversion("v-a-1.mp3-128".to_string()))
        );
        assert!(!w.plays.live(&alice(), "s2"));
        w.later(Duration::minutes(10));
        assert!(
            w.plays.live(&alice(), "s1"),
            "within the song's length and ten minutes"
        );
        w.later(Duration::minutes(2));
        assert!(
            !w.plays.live(&alice(), "s1"),
            "a pause past its time ends the play"
        );
    }

    #[test]
    fn every_request_renews_an_answered_play_under_the_key_it_started_with() {
        let w = world();
        let address = vec!["|10.0.0.2|octo".to_string()];
        w.plays.note(&address, "s1", Some(60), Answered::Source);
        let named = vec!["alice|10.0.0.2|octo".to_string(), "|10.0.0.2|octo".to_string()];
        for _ in 0..5 {
            w.later(Duration::minutes(8));
            assert_eq!(w.plays.answered(&named, "s1"), Some(Answered::Source));
            w.plays.note(&named, "s1", Some(60), Answered::Source);
        }
        assert_eq!(w.plays.answers.lock().len(), 1);
    }

    #[test]
    fn answered_plays_stay_under_their_bound() {
        let w = world();
        for n in 0..=MAX_PLAYS {
            w.plays
                .note(&alice(), &format!("s{n}"), Some(60), Answered::Source);
        }
        assert!(w.plays.answers.lock().len() <= MAX_PLAYS);
        assert!(
            w.plays.live(&alice(), &format!("s{MAX_PLAYS}")),
            "the newest play is kept"
        );
    }
}
