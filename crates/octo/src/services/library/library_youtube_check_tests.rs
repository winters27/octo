//! The one-time YouTube check asks about a song that sounds nothing like a believed YouTube preview
//! and does nothing else, passes over what AcoustID confirms, adds its finding to a question
//! AcoustID left open, goes only when it should, answers each song once, never takes a failure
//! of the shim for an answer, and never waits on one song for ever.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use octo_core::fingerprint::{InconclusiveReason, VerificationResult, VerificationVerdict};
use octo_core::settings::{AppSettings, LibraryActionSettings, NoticeKind, SubsonicSettings};

use super::*;
use crate::services::library::library_identity_sweep::CheckedSong;
use crate::services::library::notice_queue::NoticeState;

/// Answers one by one from a script, and remembers which files it was asked about.
#[derive(Default)]
struct FakeChecker {
    script: Mutex<VecDeque<Compared>>,
    asked: Mutex<Vec<String>>,
    hang: AtomicBool,
    shim_down: AtomicBool,
}

impl FakeChecker {
    fn answering(answers: Vec<Compared>) -> Arc<Self> {
        Arc::new(FakeChecker {
            script: Mutex::new(answers.into()),
            asked: Mutex::new(Vec::new()),
            hang: AtomicBool::new(false),
            shim_down: AtomicBool::new(false),
        })
    }

    /// Answers from the script, then never again.
    fn hanging_after(answers: Vec<Compared>) -> Arc<Self> {
        let checker = Self::answering(answers);
        checker.hang.store(true, Ordering::SeqCst);
        checker
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }
}

#[async_trait]
impl YoutubeChecker for FakeChecker {
    async fn check(&self, path: &Path, _: &Song) -> Compared {
        self.asked
            .lock()
            .push(path.file_name().unwrap().to_string_lossy().into_owned());
        let next = self.script.lock().pop_front();
        match next {
            Some(answer) => answer,
            // An empty script past its end: a shim that never answers.
            None if self.hang.load(Ordering::SeqCst) => std::future::pending().await,
            None => Compared::NoCopy,
        }
    }

    async fn shim_answers(&self) -> bool {
        !self.shim_down.load(Ordering::SeqCst)
    }
}

/// AcoustID's answers, one by one from a script (Confirmed past its end), counting lookups.
struct FakeVerifier {
    ready: bool,
    script: Mutex<VecDeque<VerificationResult>>,
    lookups: AtomicUsize,
}

impl FakeVerifier {
    fn answering(answers: Vec<VerificationResult>) -> Arc<Self> {
        Arc::new(FakeVerifier {
            ready: true,
            script: Mutex::new(answers.into()),
            lookups: AtomicUsize::new(0),
        })
    }

    fn without_a_key() -> Arc<Self> {
        Arc::new(FakeVerifier {
            ready: false,
            script: Mutex::new(VecDeque::new()),
            lookups: AtomicUsize::new(0),
        })
    }

    fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl IReviewSweepVerifier for FakeVerifier {
    fn is_ready(&self) -> bool {
        self.ready
    }

    async fn verify(&self, _: &str, _: Option<&str>, _: Option<&str>) -> VerificationResult {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        self.script.lock().pop_front().unwrap_or_else(confirmed)
    }

    async fn isrc_agrees(&self, _: &VerificationResult, _: &str) -> Option<bool> {
        None
    }
}

fn confirmed() -> VerificationResult {
    VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        recording_id: Some("rec-1".into()),
        ..Default::default()
    }
}

fn unsettled(reason: InconclusiveReason) -> VerificationResult {
    VerificationResult {
        verdict: VerificationVerdict::Inconclusive,
        reason,
        ..Default::default()
    }
}

fn sounds_like_another() -> VerificationResult {
    VerificationResult {
        verdict: VerificationVerdict::Mismatch,
        recording_id: Some("rec-other".into()),
        ..Default::default()
    }
}

#[derive(Default)]
struct FakeActivity {
    busy: AtomicBool,
}

impl IAcquisitionActivity for FakeActivity {
    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

fn heard(heard: Heard, bit_error: f64) -> Compared {
    Compared::Heard {
        heard,
        bit_error,
        album_agrees: true,
    }
}

fn settings_of(per_hour: i32) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            review_enabled: true,
            youtube_check_per_hour: per_hour,
            allowed_users: vec!["bob".into()],
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            admin_username: Some("bob".into()),
            ..Default::default()
        },
        ..Default::default()
    }))
}

struct Check {
    root: tempfile::TempDir,
    stores: Mutex<Vec<Arc<YoutubeCheckStore>>>,
}

impl Check {
    fn new() -> Self {
        Check {
            root: tempfile::tempdir().expect("a temp dir"),
            stores: Mutex::new(Vec::new()),
        }
    }

    fn state_path(&self) -> PathBuf {
        self.root.path().join(".state").join("youtube-check.json")
    }

    /// A FLAC tagged with an artist and title.
    fn tagged(&self, name: &str) -> String {
        let path = self.root.path().join(name);
        std::fs::write(&path, crate::services::test_support::flac()).expect("written");
        let mut file = TagFile::open(&path).expect("opens");
        file.set_performers(&["Massive Attack".to_string()]);
        file.set_title(Some("Teardrop"));
        file.save().expect("saved");
        path.to_string_lossy().into_owned()
    }

    /// A file with no tags at all.
    fn untagged(&self, name: &str) -> String {
        let path = self.root.path().join(name);
        std::fs::write(&path, crate::services::test_support::flac()).expect("written");
        path.to_string_lossy().into_owned()
    }

    fn worker(
        &self,
        checker: Arc<FakeChecker>,
        notices: Arc<NoticeQueue>,
        settings: Arc<SettingsStore>,
        activity: Arc<FakeActivity>,
    ) -> LibraryYoutubeCheck {
        // A worker built after another is a restart, and a stopping host flushes the store.
        for previous in self.stores.lock().iter() {
            previous.flush();
        }
        let store = Arc::new(YoutubeCheckStore::new(Some(self.state_path())));
        self.stores.lock().push(store.clone());
        let root = self.root.path().to_string_lossy().into_owned();
        LibraryYoutubeCheck::new(
            store,
            notices,
            checker,
            activity,
            settings,
            Box::new(move || root.clone()),
            Clock::system(),
        )
    }

    fn simple(&self, checker: Arc<FakeChecker>) -> (LibraryYoutubeCheck, Arc<NoticeQueue>) {
        let notices = Arc::new(NoticeQueue::new());
        let worker = self.worker(
            checker,
            notices.clone(),
            settings_of(60),
            Arc::new(FakeActivity::default()),
        );
        (worker, notices)
    }
}

/// Ticks until the run is over, a bounded number of times.
async fn run_to_the_end(worker: &LibraryYoutubeCheck) {
    for _ in 0..50 {
        worker.tick().await;
        if !worker.store().read(|s| s.running) {
            return;
        }
    }
    panic!("the run did not finish");
}

#[tokio::test]
async fn nothing_happens_until_a_person_starts_it() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let (worker, _) = check.simple(checker.clone());

    worker.tick().await;

    assert!(checker.asked().is_empty());
    assert_eq!(worker.status().state, "Idle");
}

#[tokio::test]
async fn a_song_that_sounds_nothing_like_its_preview_is_asked_about_and_one_that_does_is_not() {
    let check = Check::new();
    check.tagged("a-wrong.flac");
    check.tagged("b-right.flac");
    let checker = FakeChecker::answering(vec![heard(Heard::Mismatch, 0.41), heard(Heard::Match, 0.97)]);
    let (worker, notices) = check.simple(checker);

    worker.start();
    run_to_the_end(&worker).await;

    let questions = notices.for_user("bob", NoticeKind::Review);
    assert_eq!(questions.len(), 1);
    let question = &questions[0];
    assert!(question.local_path.ends_with("a-wrong.flac"));
    assert_eq!(question.origin, NoticeOrigin::LibrarySweep);
    // A cause every earlier release reads, so going back a release keeps the queue.
    assert_eq!(question.cause, InconclusiveReason::SoundsLikeAnother);
    assert_eq!(question.reason, "Sounds nothing like the song on YouTube");
    assert!(question.fingerprint.is_none());
    let (asked, alike, done) = worker.store().read(|s| (s.asked, s.alike, s.done));
    assert_eq!((asked, alike, done), (1, 1, 2));
}

#[tokio::test]
async fn an_unclear_song_and_one_with_no_preview_are_left_alone() {
    let check = Check::new();
    check.tagged("a.flac");
    check.tagged("b.flac");
    let checker = FakeChecker::answering(vec![heard(Heard::Unclear, 0.64), Compared::NoCopy]);
    let (worker, notices) = check.simple(checker);

    worker.start();
    run_to_the_end(&worker).await;

    assert!(notices.for_user("bob", NoticeKind::Review).is_empty());
    let (unsure, no_copy) = worker.store().read(|s| (s.unsure, s.no_copy));
    assert_eq!((unsure, no_copy), (1, 1));
}

#[tokio::test]
async fn a_song_with_no_tags_is_not_looked_up() {
    let check = Check::new();
    check.untagged("nameless.flac");
    let checker = FakeChecker::answering(vec![]);
    let (worker, _) = check.simple(checker.clone());

    worker.start();
    run_to_the_end(&worker).await;

    assert!(checker.asked().is_empty());
    assert_eq!(worker.store().read(|s| s.no_copy), 1);
}

#[tokio::test]
async fn each_song_is_answered_once_and_the_run_does_not_start_again() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![heard(Heard::Match, 0.95)]);
    let (worker, _) = check.simple(checker.clone());

    worker.start();
    run_to_the_end(&worker).await;
    for _ in 0..5 {
        worker.tick().await;
    }

    assert_eq!(checker.asked(), ["a.flac"]);
    let finished = worker.store().read(|s| (s.running, s.finished_utc.is_some()));
    assert_eq!(finished, (false, true));
    assert_eq!(worker.status().state, "Idle");
}

#[tokio::test]
async fn a_restart_carries_on_where_it_stopped() {
    let check = Check::new();
    check.tagged("a.flac");
    check.tagged("b.flac");
    check.tagged("c.flac");
    let first = FakeChecker::answering(vec![heard(Heard::Match, 0.95)]);
    let (worker, notices) = check.simple(first.clone());
    worker.start();
    worker.tick().await;
    assert_eq!(first.asked(), ["a.flac"]);
    drop(worker);

    let second = FakeChecker::answering(vec![]);
    let worker = check.worker(
        second.clone(),
        notices,
        settings_of(60),
        Arc::new(FakeActivity::default()),
    );
    run_to_the_end(&worker).await;

    assert_eq!(second.asked(), ["b.flac", "c.flac"]);
}

#[tokio::test]
async fn a_failure_of_the_shim_is_not_an_answer() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![Compared::Unavailable, heard(Heard::Match, 0.95)]);
    let (worker, _) = check.simple(checker.clone());

    worker.start();
    worker.tick().await;
    // Not counted, the cursor has not moved past it, and it is tried again.
    assert_eq!(
        worker.store().read(|s| (s.done, s.cursor.clone())),
        (0, String::new())
    );
    run_to_the_end(&worker).await;

    assert_eq!(checker.asked(), ["a.flac", "a.flac"]);
    assert_eq!(worker.store().read(|s| s.alike), 1);
}

#[tokio::test]
async fn a_busy_shim_rests_nothing_and_gives_up_nothing() {
    let check = Check::new();
    for name in ["a.flac", "b.flac"] {
        check.tagged(name);
    }
    // Four failures, a busy answer, then a fifth failure: the busy answer reset nothing, so the
    // fifth is what makes the check rest.
    let mut script = vec![Compared::Unavailable; 4];
    script.push(Compared::Busy);
    script.push(Compared::Unavailable);
    let checker = FakeChecker::answering(script);
    let (worker, _) = check.simple(checker.clone());

    worker.start();
    for _ in 0..4 {
        worker.tick().await;
    }
    let wait = worker.tick().await;
    assert_eq!(wait, Duration::from_secs(3600 / 60));
    let status = worker.status();
    assert_eq!(status.state, "Waiting");
    assert!(
        status
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("busy with plays")),
        "{status:?}"
    );
    // Not counted, no cursor moved, nothing given up on.
    assert_eq!(
        worker.store().read(|s| (s.done, s.no_copy, s.cursor.clone())),
        (0, 0, String::new())
    );

    let wait = worker.tick().await;
    assert_eq!(wait, LibraryYoutubeCheck::REST_WHEN_UNAVAILABLE);
    assert_eq!(checker.asked(), ["a.flac"; 6]);
}

#[tokio::test]
async fn an_unreadable_library_file_is_tried_three_times_then_has_no_copy() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![Compared::FileUnreadable; 3]);
    let (worker, _) = check.simple(checker.clone());

    worker.start();
    for _ in 0..2 {
        worker.tick().await;
        let status = worker.status();
        assert_eq!(status.state, "Running");
        assert!(
            status
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("fpcalc could not read")),
            "{status:?}"
        );
        assert_eq!(
            worker.store().read(|s| (s.done, s.cursor.clone())),
            (0, String::new())
        );
    }
    worker.tick().await;

    assert_eq!(checker.asked(), ["a.flac"; 3]);
    assert_eq!(worker.store().read(|s| (s.done, s.no_copy)), (1, 1));
}

#[tokio::test]
async fn a_read_that_works_resets_the_count_of_unreadable_tries() {
    let check = Check::new();
    check.tagged("a.flac");
    check.tagged("b.flac");
    let checker = FakeChecker::answering(vec![
        Compared::FileUnreadable,
        Compared::FileUnreadable,
        heard(Heard::Match, 0.95),
        Compared::FileUnreadable,
        Compared::FileUnreadable,
        heard(Heard::Match, 0.95),
    ]);
    let (worker, _) = check.simple(checker);

    worker.start();
    run_to_the_end(&worker).await;

    assert_eq!(worker.store().read(|s| (s.alike, s.no_copy)), (2, 0));
}

#[tokio::test]
async fn five_failures_in_a_row_make_it_rest_an_hour() {
    let check = Check::new();
    for name in ["a.flac", "b.flac"] {
        check.tagged(name);
    }
    let checker = FakeChecker::answering(vec![Compared::Unavailable; 5]);
    let (worker, _) = check.simple(checker);

    worker.start();
    let mut wait = Duration::ZERO;
    for _ in 0..5 {
        wait = worker.tick().await;
    }

    assert_eq!(wait, LibraryYoutubeCheck::REST_WHEN_UNAVAILABLE);
    assert_eq!(worker.status().state, "Waiting");
    assert_eq!(worker.store().read(|s| s.done), 0);
}

#[tokio::test]
async fn zero_an_hour_is_off() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let notices = Arc::new(NoticeQueue::new());
    let worker = check.worker(
        checker.clone(),
        notices,
        settings_of(0),
        Arc::new(FakeActivity::default()),
    );

    worker.start();
    worker.tick().await;

    assert!(checker.asked().is_empty());
    assert_eq!(worker.status().state, "Off");
}

#[tokio::test]
async fn a_download_in_flight_makes_it_wait() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let activity = Arc::new(FakeActivity::default());
    activity.busy.store(true, Ordering::SeqCst);
    let worker = check.worker(
        checker.clone(),
        Arc::new(NoticeQueue::new()),
        settings_of(60),
        activity.clone(),
    );

    worker.start();
    worker.tick().await;
    assert!(checker.asked().is_empty());
    assert_eq!(worker.status().state, "Waiting");

    activity.busy.store(false, Ordering::SeqCst);
    worker.tick().await;
    assert_eq!(checker.asked(), ["a.flac"]);
}

#[tokio::test]
async fn nobody_on_the_allowlist_means_nobody_is_asked() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![heard(Heard::Mismatch, 0.3)]);
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            review_enabled: true,
            youtube_check_per_hour: 60,
            allowed_users: Vec::new(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let worker = check.worker(
        checker.clone(),
        Arc::new(NoticeQueue::new()),
        settings,
        Arc::new(FakeActivity::default()),
    );

    worker.start();
    worker.tick().await;

    assert!(checker.asked().is_empty());
    assert_eq!(worker.status().state, "Paused");
}

#[tokio::test]
async fn fifty_open_questions_pause_it() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let notices = Arc::new(NoticeQueue::new());
    for i in 0..LibraryYoutubeCheck::MAX_OPEN_QUESTIONS {
        let question = VerificationResult {
            reason: InconclusiveReason::NoEntry,
            ..Default::default()
        };
        notices.add_review_from(
            "bob",
            &format!("/music/{i}.flac"),
            &Song::default(),
            &question,
            NoticeOrigin::LibrarySweep,
        );
    }
    let worker = check.worker(
        checker.clone(),
        notices,
        settings_of(60),
        Arc::new(FakeActivity::default()),
    );

    worker.start();
    worker.tick().await;

    assert!(checker.asked().is_empty());
    assert_eq!(worker.status().state, "Paused");
}

#[tokio::test]
async fn a_song_with_an_open_question_acoustid_settled_is_not_compared() {
    let check = Check::new();
    let path = check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let notices = Arc::new(NoticeQueue::new());
    let question = VerificationResult {
        reason: InconclusiveReason::SoundsLikeAnother,
        ..Default::default()
    };
    notices.add_review_from(
        "bob",
        &path,
        &Song::default(),
        &question,
        NoticeOrigin::LibrarySweep,
    );
    let worker = check.worker(
        checker.clone(),
        notices,
        settings_of(60),
        Arc::new(FakeActivity::default()),
    );

    worker.start();
    run_to_the_end(&worker).await;

    assert!(checker.asked().is_empty());
}

#[tokio::test]
async fn pressing_start_again_forgets_the_answers() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![heard(Heard::Match, 0.95), heard(Heard::Match, 0.95)]);
    let (worker, _) = check.simple(checker.clone());
    worker.start();
    run_to_the_end(&worker).await;

    worker.start();
    run_to_the_end(&worker).await;

    assert_eq!(checker.asked(), ["a.flac", "a.flac"]);
}

#[test]
fn a_corrupt_state_file_is_set_aside_and_the_check_starts_over() {
    let check = Check::new();
    let path = check.state_path();
    std::fs::create_dir_all(path.parent().unwrap()).expect("folder");
    std::fs::write(&path, "{ not json").expect("written");

    let store = YoutubeCheckStore::new(Some(path.clone()));

    assert!(!store.read(|s| s.running));
    assert!(!path.exists());
}

#[tokio::test]
async fn a_song_that_cannot_be_compared_after_two_rests_is_counted_as_having_no_copy() {
    let check = Check::new();
    check.tagged("a-region-locked.flac");
    check.tagged("b-fine.flac");
    // Ten failures on the first song (two rests of five), then the second answers.
    let mut script = vec![Compared::Unavailable; 10];
    script.push(heard(Heard::Match, 0.95));
    let checker = FakeChecker::answering(script);
    let (worker, _) = check.simple(checker.clone());

    worker.start();
    run_to_the_end(&worker).await;

    let (no_copy, alike, done) = worker.store().read(|s| (s.no_copy, s.alike, s.done));
    assert_eq!((no_copy, alike, done), (1, 1, 2));
    assert_eq!(checker.asked().last().map(String::as_str), Some("b-fine.flac"));
    assert!(!worker.store().read(|s| s.running));
}

#[tokio::test]
async fn an_answer_between_failures_starts_the_count_again() {
    let check = Check::new();
    check.tagged("a.flac");
    check.tagged("b.flac");
    // Four failures, an answer, four more: never five in a row, so it never rests.
    let mut script = vec![Compared::Unavailable; 4];
    script.push(heard(Heard::Match, 0.95));
    script.extend(vec![Compared::Unavailable; 4]);
    script.push(heard(Heard::Match, 0.95));
    let (worker, _) = check.simple(FakeChecker::answering(script));

    worker.start();
    for _ in 0..20 {
        let wait = worker.tick().await;
        assert_ne!(wait, LibraryYoutubeCheck::REST_WHEN_UNAVAILABLE);
        if !worker.store().read(|s| s.running) {
            break;
        }
    }

    assert_eq!(worker.store().read(|s| (s.alike, s.running)), (2, false));
}

#[tokio::test]
async fn pressing_start_says_so_at_once() {
    let check = Check::new();
    check.tagged("a.flac");
    let (worker, _) = check.simple(FakeChecker::answering(vec![]));

    worker.start();
    let status = worker.status();

    assert!(status.running);
    assert_eq!(status.state, "Waiting");
    assert!(
        status.reason.as_deref().is_some_and(|r| r.contains("Starting")),
        "{status:?}"
    );
}

#[tokio::test]
async fn a_finished_run_stays_finished_after_a_restart() {
    let check = Check::new();
    check.tagged("a.flac");
    let first = FakeChecker::answering(vec![heard(Heard::Match, 0.95)]);
    let (worker, notices) = check.simple(first);
    worker.start();
    run_to_the_end(&worker).await;
    drop(worker);

    let second = FakeChecker::answering(vec![]);
    let worker = check.worker(
        second.clone(),
        notices,
        settings_of(60),
        Arc::new(FakeActivity::default()),
    );
    for _ in 0..5 {
        worker.tick().await;
    }

    assert!(second.asked().is_empty());
    assert_eq!(worker.status().state, "Idle");
    assert!(worker.store().read(|s| s.finished_utc.is_some() && !s.running));
}

#[tokio::test(start_paused = true)]
async fn a_song_that_never_answers_is_given_up_on_and_not_waited_for() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::hanging_after(vec![]);
    let (worker, _) = check.simple(checker.clone());

    worker.start();
    let started = tokio::time::Instant::now();
    worker.tick().await;

    // The tick came back after the timeout, not never, and the song is not counted.
    assert!(started.elapsed() >= LibraryYoutubeCheck::CHECK_TIMEOUT);
    assert_eq!(checker.asked(), ["a.flac"]);
    assert_eq!(worker.store().read(|s| (s.done, s.running)), (0, true));
}

/// Says when it is asked, then answers a mismatch only once let go.
struct GatedChecker {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl YoutubeChecker for GatedChecker {
    async fn check(&self, _: &Path, _: &Song) -> Compared {
        self.entered.notify_one();
        self.release.notified().await;
        heard(Heard::Mismatch, 0.45)
    }
}

#[tokio::test]
async fn an_answer_from_a_run_started_over_meanwhile_is_dropped() {
    let check = Check::new();
    check.tagged("a.flac");
    let gate = Arc::new(GatedChecker {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let notices = Arc::new(NoticeQueue::new());
    let store = Arc::new(YoutubeCheckStore::new(None));
    let root = check.root.path().to_string_lossy().into_owned();
    let worker = Arc::new(LibraryYoutubeCheck::new(
        store,
        notices.clone(),
        gate.clone(),
        Arc::new(FakeActivity::default()),
        settings_of(60),
        Box::new(move || root.clone()),
        Clock::system(),
    ));
    worker.start();

    let ticking = {
        let worker = worker.clone();
        tokio::spawn(async move { worker.tick().await })
    };
    gate.entered.notified().await;
    worker.start();
    gate.release.notify_one();
    ticking.await.expect("the tick ends");

    assert!(notices.for_user("bob", NoticeKind::Review).is_empty());
    assert_eq!(worker.store().read(|s| (s.done, s.asked)), (0, 0));
}

// ---- A second opinion on what AcoustID left open -----------------------------------------

#[tokio::test]
async fn an_open_question_acoustid_could_not_settle_gets_the_finding_and_loses_its_fingerprint() {
    let check = Check::new();
    let path = check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![heard(Heard::Mismatch, 0.44)]);
    let notices = Arc::new(NoticeQueue::new());
    let download = VerificationResult {
        verdict: VerificationVerdict::Inconclusive,
        reason: InconclusiveReason::NoEntry,
        fingerprint: Some("AQAAx".into()),
        duration_seconds: 200,
        ..Default::default()
    };
    assert!(notices.add_review("alice", &path, &Song::default(), &download));
    let worker = check.worker(
        checker.clone(),
        notices.clone(),
        settings_of(60),
        Arc::new(FakeActivity::default()),
    );

    worker.start();
    run_to_the_end(&worker).await;

    assert_eq!(checker.asked(), ["a.flac"]);
    // No second question: the open one took the finding.
    assert!(notices.for_user("bob", NoticeKind::Review).is_empty());
    let question = &notices.for_user("alice", NoticeKind::Review)[0];
    assert_eq!(
        question.reason,
        "AcoustID has never heard this recording, and it sounds nothing like the song on YouTube"
    );
    assert_eq!(question.cause, InconclusiveReason::NoEntry);
    // A Keep can no longer send AcoustID a sound YouTube heard as another song.
    assert!(question.fingerprint.is_none());
    assert_eq!(worker.store().read(|s| s.asked), 1);
}

#[tokio::test]
async fn a_question_someone_answered_is_not_compared_again() {
    let check = Check::new();
    let path = check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let notices = Arc::new(NoticeQueue::new());
    notices.add_review(
        "alice",
        &path,
        &Song::default(),
        &unsettled(InconclusiveReason::NoEntry),
    );
    notices.resolve(&NoticeQueue::review_key("alice", &path), NoticeState::Kept);
    let worker = check.worker(
        checker.clone(),
        notices,
        settings_of(60),
        Arc::new(FakeActivity::default()),
    );

    worker.start();
    run_to_the_end(&worker).await;

    assert!(checker.asked().is_empty());
}

// ---- AcoustID first ----------------------------------------------------------------------

#[tokio::test]
async fn a_song_acoustid_confirms_is_not_compared_with_youtube() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let verifier = FakeVerifier::answering(vec![confirmed()]);
    let (worker, _) = check.simple(checker.clone());
    let worker = worker.with_acoustid(Arc::new(IdentitySweepStore::new(None)), verifier.clone());

    worker.start();
    let wait = worker.tick().await;

    assert_eq!(wait, LibraryYoutubeCheck::KNOWN_STEP);
    assert!(checker.asked().is_empty());
    assert_eq!(verifier.lookups(), 1);
    assert_eq!(worker.store().read(|s| (s.known, s.done)), (1, 1));
    assert_eq!(worker.status().known, 1);
}

#[tokio::test]
async fn a_song_the_tag_check_confirmed_needs_no_lookup_until_it_changes() {
    let check = Check::new();
    let path = check.tagged("a.flac");
    let identities = Arc::new(IdentitySweepStore::new(None));
    identities.update(|s| {
        s.songs.insert(
            "nd-1".into(),
            CheckedSong {
                stamp: stamp_of(&path).expect("a stamp"),
                path: path.clone(),
                recording_id: Some("rec-1".into()),
                confirmed_by_two: true,
                ..Default::default()
            },
        );
    });
    let checker = FakeChecker::answering(vec![heard(Heard::Match, 0.05)]);
    let verifier = FakeVerifier::answering(vec![unsettled(InconclusiveReason::NoEntry)]);
    let (worker, _) = check.simple(checker.clone());
    let worker = worker.with_acoustid(identities.clone(), verifier.clone());

    worker.start();
    let wait = worker.tick().await;

    assert_eq!(wait, LibraryYoutubeCheck::STORED_STEP);
    assert_eq!(verifier.lookups(), 0);
    assert!(checker.asked().is_empty());

    // The file changed since the tag check heard it: AcoustID is asked again.
    identities.update(|s| s.songs.get_mut("nd-1").expect("kept").stamp = "1:2".into());
    worker.start();
    run_to_the_end(&worker).await;

    assert_eq!(verifier.lookups(), 1);
    assert_eq!(checker.asked(), ["a.flac"]);
}

/// A confirmation the tag check stored before one person's submissions stopped confirming a file
/// is not taken: AcoustID is asked again.
#[tokio::test]
async fn a_confirmation_stored_before_the_two_submission_rule_is_looked_up_again() {
    let check = Check::new();
    let path = check.tagged("a.flac");
    let identities = Arc::new(IdentitySweepStore::new(None));
    identities.update(|s| {
        s.songs.insert(
            "nd-1".into(),
            CheckedSong {
                stamp: stamp_of(&path).expect("a stamp"),
                path: path.clone(),
                recording_id: Some("rec-1".into()),
                ..Default::default()
            },
        );
    });
    let checker = FakeChecker::answering(vec![heard(Heard::Match, 0.05)]);
    let verifier = FakeVerifier::answering(vec![unsettled(InconclusiveReason::NoEntry)]);
    let (worker, _) = check.simple(checker.clone());
    let worker = worker.with_acoustid(identities, verifier.clone());

    worker.start();
    run_to_the_end(&worker).await;

    assert_eq!(verifier.lookups(), 1);
    assert_eq!(checker.asked(), ["a.flac"]);
}

#[tokio::test]
async fn songs_acoustid_cannot_vouch_for_are_compared() {
    let check = Check::new();
    for name in ["a.flac", "b.flac", "c.flac"] {
        check.tagged(name);
    }
    let checker = FakeChecker::answering(vec![]);
    let verifier = FakeVerifier::answering(vec![
        unsettled(InconclusiveReason::NoEntry),
        unsettled(InconclusiveReason::BelowThreshold),
        sounds_like_another(),
    ]);
    let (worker, _) = check.simple(checker.clone());
    let worker = worker.with_acoustid(Arc::new(IdentitySweepStore::new(None)), verifier);

    worker.start();
    run_to_the_end(&worker).await;

    assert_eq!(checker.asked(), ["a.flac", "b.flac", "c.flac"]);
    assert_eq!(worker.store().read(|s| s.known), 0);
}

#[tokio::test]
async fn without_an_acoustid_key_every_song_is_compared() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let verifier = FakeVerifier::without_a_key();
    let (worker, _) = check.simple(checker.clone());
    let worker = worker.with_acoustid(Arc::new(IdentitySweepStore::new(None)), verifier.clone());

    worker.start();
    run_to_the_end(&worker).await;

    assert_eq!(checker.asked(), ["a.flac"]);
    assert_eq!(verifier.lookups(), 0);
}

#[tokio::test]
async fn a_lookup_acoustid_did_not_answer_is_tried_again_before_youtube() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let missed = unsettled(InconclusiveReason::LookupFailed);
    let verifier = FakeVerifier::answering(vec![missed.clone(), missed.clone(), missed]);
    let (worker, _) = check.simple(checker.clone());
    let worker = worker.with_acoustid(Arc::new(IdentitySweepStore::new(None)), verifier.clone());

    worker.start();
    for _ in 1..LibraryYoutubeCheck::MAX_LOOKUP_MISSES {
        assert_eq!(worker.tick().await, LibraryYoutubeCheck::KNOWN_STEP);
        assert!(checker.asked().is_empty());
    }
    worker.tick().await;

    assert_eq!(verifier.lookups(), 3);
    assert_eq!(checker.asked(), ["a.flac"]);
}

#[tokio::test]
async fn a_file_fpcalc_cannot_read_is_not_sent_to_youtube() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![]);
    let verifier = FakeVerifier::answering(
        (0..LibraryYoutubeCheck::MAX_UNREADABLE_TRIES)
            .map(|_| unsettled(InconclusiveReason::NotFingerprinted))
            .collect(),
    );
    let (worker, _) = check.simple(checker.clone());
    let worker = worker.with_acoustid(Arc::new(IdentitySweepStore::new(None)), verifier.clone());

    worker.start();
    run_to_the_end(&worker).await;

    // Read three times before the song is counted as having no copy.
    assert_eq!(verifier.lookups(), 3);
    assert!(checker.asked().is_empty());
    assert_eq!(worker.store().read(|s| s.no_copy), 1);
}

// ---- Outages and waking --------------------------------------------------------------------

#[tokio::test]
async fn an_outage_never_gives_up_on_a_song() {
    let check = Check::new();
    check.tagged("a.flac");
    let mut script = vec![Compared::Unavailable; 10];
    script.push(heard(Heard::Match, 0.05));
    let checker = FakeChecker::answering(script);
    checker.shim_down.store(true, Ordering::SeqCst);
    let (worker, _) = check.simple(checker.clone());

    worker.start();
    for _ in 0..10 {
        worker.tick().await;
    }
    assert_eq!(worker.status().state, "Waiting");
    assert_eq!(worker.store().read(|s| s.done), 0);
    run_to_the_end(&worker).await;

    assert_eq!(checker.asked().len(), 11);
    assert_eq!(worker.store().read(|s| (s.no_copy, s.alike)), (0, 1));
}

/// Waits, on the paused clock, until `done` holds or ten seconds have gone.
async fn within_ten_seconds(done: impl Fn() -> bool) -> bool {
    for _ in 0..100 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    done()
}

#[tokio::test(start_paused = true)]
async fn start_cuts_the_startup_wait_short() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![heard(Heard::Match, 0.05)]);
    let (worker, _) = check.simple(checker.clone());
    let worker = Arc::new(worker);
    let stopping = CancellationToken::new();
    let running = tokio::spawn(worker.clone().run(stopping.clone()));
    tokio::time::sleep(Duration::from_secs(1)).await;

    worker.start();

    assert!(within_ten_seconds(|| !checker.asked().is_empty()).await);
    stopping.cancel();
    running.await.expect("the loop ends").expect("cleanly");
}

#[tokio::test(start_paused = true)]
async fn start_cuts_an_hours_rest_short() {
    let check = Check::new();
    check.tagged("a.flac");
    let checker = FakeChecker::answering(vec![Compared::Unavailable; 5]);
    let (worker, _) = check.simple(checker.clone());
    let worker = Arc::new(worker);
    let stopping = CancellationToken::new();
    worker.start();
    let running = tokio::spawn(worker.clone().run(stopping.clone()));
    let resting = || {
        worker
            .status()
            .reason
            .is_some_and(|reason| reason.contains("not answering"))
    };
    for _ in 0..100 {
        if resting() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    assert!(resting(), "{:?}", worker.status());
    assert_eq!(checker.asked().len(), 5);

    worker.start();

    assert!(within_ten_seconds(|| checker.asked().len() == 6).await);
    stopping.cancel();
    running.await.expect("the loop ends").expect("cleanly");
}

// ---- The comparison with a real preview -----------------------------------------------

/// A shim that sends `audio` as the preview of every song, or nothing.
struct OneVideo {
    audio: Option<PathBuf>,
}

#[async_trait]
impl PreviewSource for OneVideo {
    async fn find(&self, _request: &PreviewRequest<'_>) -> Result<Option<PreviewFacts>, Failure> {
        Ok(Some(Self::default_facts()))
    }

    async fn fetch(&self, _video_id: &str, dest: &Path) -> bool {
        self.audio
            .as_ref()
            .is_some_and(|audio| std::fs::copy(audio, dest).is_ok())
    }
}

/// 40 seconds of pink noise as FLAC, or None where ffmpeg or fpcalc is missing.
fn noise_flac(dir: &Path, name: &str, color: &str) -> Option<PathBuf> {
    let on_path = |program: &str| {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                dir.join(format!("{program}{}", std::env::consts::EXE_SUFFIX))
                    .is_file()
            })
        })
    };
    if !on_path("ffmpeg") || !on_path("fpcalc") {
        return None;
    }
    let path = dir.join(name);
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "anoisesrc=color={color}:duration=40:sample_rate=44100:amplitude=0.3"
        ))
        .arg(&path)
        .status()
        .is_ok_and(|status| status.success());
    made.then_some(path)
}

fn request() -> PreviewRequest<'static> {
    PreviewRequest {
        artist: "Massive Attack",
        title: "Teardrop",
        seconds: Some(40),
        video_id: None,
        background: false,
    }
}

#[tokio::test]
async fn a_file_heard_against_its_own_sound_matches_and_a_shim_that_sends_nothing_is_unavailable() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let Some(file) = noise_flac(dir.path(), "file.flac", "pink") else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    let fingerprinter = AudioFingerprinter::new();
    let preview = OneVideo::default_facts();

    let same = OneVideo {
        audio: Some(file.clone()),
    };
    let answer = hear(&same, &fingerprinter, &file, &request(), "", preview.clone(), 30).await;
    assert!(
        matches!(
            answer,
            Compared::Heard {
                heard: Heard::Match,
                ..
            }
        ),
        "{answer:?}"
    );

    let down = OneVideo { audio: None };
    let answer = hear(&down, &fingerprinter, &file, &request(), "", preview, 30).await;
    assert_eq!(answer, Compared::Unavailable);
}

#[tokio::test]
async fn a_file_fpcalc_cannot_read_has_no_preview_to_compare_with() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let broken = dir.path().join("broken.flac");
    std::fs::write(&broken, b"not audio").expect("written");
    let shim = OneVideo { audio: None };

    let answer = hear(
        &shim,
        &AudioFingerprinter::new(),
        &broken,
        &request(),
        "",
        OneVideo::default_facts(),
        30,
    )
    .await;

    assert_eq!(answer, Compared::FileUnreadable);
}

impl OneVideo {
    fn default_facts() -> PreviewFacts {
        PreviewFacts {
            video_id: "vid".into(),
            title: Some("Teardrop".into()),
            artist: Some("Massive Attack".into()),
            seconds: Some(40),
            album: None,
        }
    }
}

/// A shim nothing answers on is not an answer: the song is tried again later.
#[tokio::test]
async fn an_unreachable_shim_is_unavailable_not_no_copy() {
    let closed = format!("http://{}", crate::services::test_support::refused_address());
    let checker = AppYoutubeChecker {
        fingerprinter: Arc::new(AudioFingerprinter::new()),
        youtube: Arc::new(YouTubeResolver::with_base_url(Some(&closed))),
        settings: settings_of(60),
    };
    let check = Check::new();
    let path = check.tagged("a.flac");
    let song = Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        ..Default::default()
    };

    assert_eq!(
        checker.check(Path::new(&path), &song).await,
        Compared::Unavailable
    );
}

/// The one-time check asks YouTube Music in the shim's background lane.
#[tokio::test]
async fn the_check_asks_youtube_music_in_the_background_lane() {
    use wiremock::matchers::{method, path as url_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let shim = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/ytm/search"))
        .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"busy"}"#))
        .mount(&shim)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/search"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&shim)
        .await;
    let checker = AppYoutubeChecker {
        fingerprinter: Arc::new(AudioFingerprinter::new()),
        youtube: Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
        settings: settings_of(60),
    };
    let check = Check::new();
    let file = check.tagged("a.flac");
    let song = Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        ..Default::default()
    };

    checker.check(Path::new(&file), &song).await;

    let asked: Vec<String> = shim
        .received_requests()
        .await
        .expect("recorded")
        .iter()
        .filter(|r| r.url.path() == "/ytm/search")
        .map(|r| r.url.query().unwrap_or("").to_string())
        .collect();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].contains("bg=1"), "{asked:?}");
}
