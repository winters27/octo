//! The acquisition tracker: the live progress list behind the app's download ring. It watches
//! the pipeline and must tell the truth about it: one row per hearted song however many sources
//! it falls through, failed only when the last one gives up, and gone three hours after it ends.
//!
//! The pure helpers (progress from bytes or a percentage, short errors that keep server paths
//! home) are tested in `octo_core::common::acquisition_tracker`. The slskd transfer parsing of
//! the last three transfer tests is tested with `SoulseekClient`; their tracker half is here.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::TimeZone;
use futures::FutureExt;
use octo_core::fingerprint::heart_gate::WaitingFor;
use octo_core::settings::{
    AppSettings, HeartDownloadSource, HeartDownloadStep, SettingsStore, SubsonicSettings,
};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr};
use crate::services::common::{
    AcquisitionRequest, HeartAcquisitionCoordinator, HeartCoordinatorExtras, HeldAcquisition,
    TrackAcquisitionQueue,
};

/// A clock the test moves by hand.
#[derive(Clone)]
struct ManualClock(Arc<Mutex<DateTime<Utc>>>);

impl ManualClock {
    fn new() -> ManualClock {
        ManualClock(Arc::new(Mutex::new(
            Utc.with_ymd_and_hms(2026, 9, 26, 18, 0, 0).unwrap(),
        )))
    }

    fn advance(&self, by: TimeDelta) {
        *self.0.lock() += by;
    }

    fn clock(&self) -> Clock {
        let now = self.0.clone();
        Clock::new(move || *now.lock())
    }
}

fn new_tracker() -> Arc<AcquisitionTracker> {
    Arc::new(AcquisitionTracker::new(None, Clock::system()))
}

fn tracker_at(clock: &ManualClock) -> Arc<AcquisitionTracker> {
    Arc::new(AcquisitionTracker::new(None, clock.clock()))
}

fn only(tracker: &AcquisitionTracker, user: &str) -> AcquisitionSnapshot {
    let rows = tracker.for_user(user);
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows.into_iter().next().expect("one row")
}

fn begin(tracker: &AcquisitionTracker, id: &str, user: &str) {
    tracker.begin("soulseek", id, Some(id), Some(user), None, None, None);
}

fn tracks(
    list: &[(&str, &str, &str, &str)],
) -> Vec<(String, Option<String>, Option<String>, Option<String>)> {
    list.iter()
        .map(|(id, a, t, al)| {
            (
                id.to_string(),
                Some(a.to_string()),
                Some(t.to_string()),
                Some(al.to_string()),
            )
        })
        .collect()
}

fn lookup(answer: impl Fn() -> anyhow::Result<Option<String>> + Send + Sync + 'static) -> LibraryLookup {
    let answer = Arc::new(answer);
    Arc::new(move |_, _, _| {
        let answer = answer.clone();
        async move { answer() }.boxed()
    })
}

#[tokio::test]
async fn a_star_walks_through_every_stage_to_done() {
    let tracker = new_tracker();

    tracker.begin(
        "soulseek",
        "abc",
        Some("abc"),
        Some("alice"),
        Some("Daft Punk"),
        Some("Da Funk"),
        Some("Homework"),
    );
    assert_eq!(only(&tracker, "alice").state, AcquisitionState::Queued);

    tracker.stage(
        "soulseek",
        "abc",
        AcquisitionState::Searching,
        Some("Soulseek"),
        None,
    );
    assert_eq!(only(&tracker, "alice").state, AcquisitionState::Searching);
    assert_eq!(only(&tracker, "alice").source.as_deref(), Some("Soulseek"));

    tracker.transfer("soulseek", "abc", Some(7_250_000), Some(29_000_000), None, None);
    let downloading = only(&tracker, "alice");
    assert_eq!(downloading.state, AcquisitionState::Downloading);
    assert_eq!(downloading.progress, Some(0.25));
    assert_eq!(downloading.bytes_done, Some(7_250_000));
    assert_eq!(downloading.bytes_total, Some(29_000_000));

    tracker.stage("soulseek", "abc", AcquisitionState::Verifying, None, None);
    let verifying = only(&tracker, "alice");
    assert_eq!(verifying.state, AcquisitionState::Verifying);
    assert_eq!(verifying.progress, None);
    assert_eq!(verifying.bytes_total, Some(29_000_000));

    tracker.stage("soulseek", "abc", AcquisitionState::Importing, None, None);
    assert_eq!(only(&tracker, "alice").state, AcquisitionState::Importing);

    // No lookup is available here, so an imported song is done at once.
    tracker.imported(
        "soulseek",
        "abc",
        Some("Daft Punk"),
        Some("Da Funk"),
        Some("/music/Daft Punk/Da Funk.flac"),
    );
    let done = only(&tracker, "alice");
    assert_eq!(done.state, AcquisitionState::Done);
    assert_eq!(done.error, None);
    assert_eq!(done.artist.as_deref(), Some("Daft Punk"));
    assert_eq!(done.title.as_deref(), Some("Da Funk"));
    assert_eq!(done.album.as_deref(), Some("Homework"));
}

#[tokio::test]
async fn a_finished_row_is_never_reopened_by_a_late_stage() {
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");
    tracker.fail("soulseek", "abc", Some("No Soulseek FLAC found for 'A - B'"));

    // A play of the same song an hour later reports stages too; they belong to no heart.
    tracker.stage("soulseek", "abc", AcquisitionState::Downloading, None, None);
    tracker.transfer("soulseek", "abc", Some(1), Some(2), None, None);
    tracker.imported("soulseek", "abc", Some("A"), Some("B"), Some("/music/b.flac"));

    let row = only(&tracker, "alice");
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.error.as_deref(), Some("No Soulseek FLAC found for 'A - B'"));
}

#[test]
fn a_stage_for_a_song_nobody_hearted_creates_nothing() {
    let tracker = new_tracker();

    tracker.stage(
        "soulseek",
        "played",
        AcquisitionState::Searching,
        Some("Soulseek"),
        None,
    );
    tracker.transfer("soulseek", "played", Some(5), Some(10), None, None);

    assert!(tracker.all().is_empty());
}

#[test]
fn a_second_heart_joins_the_running_row_and_a_heart_after_the_end_restarts_it() {
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");
    tracker.transfer("soulseek", "abc", Some(5), Some(10), None, None);

    begin(&tracker, "abc", "bob");
    let all = tracker.all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].requested_by, ["alice", "bob"]);
    assert_eq!(all[0].state, AcquisitionState::Downloading);

    tracker.fail("soulseek", "abc", Some("gone"));
    begin(&tracker, "abc", "carol");
    let all = tracker.all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].state, AcquisitionState::Queued);
    assert_eq!(all[0].requested_by, ["carol"]);
    assert_eq!(all[0].error, None);
    assert!(tracker.for_user("alice").is_empty());
}

#[test]
fn each_user_sees_only_their_own_rows() {
    let tracker = new_tracker();
    begin(&tracker, "a1", "alice");
    begin(&tracker, "b1", "bob");

    assert_eq!(only(&tracker, "alice").id, "a1");
    assert_eq!(only(&tracker, "bob").id, "b1");
    assert_eq!(only(&tracker, "ALICE").id, "a1");
    assert!(tracker.for_user("mallory").is_empty());
    assert!(tracker.for_user("").is_empty());
    assert_eq!(tracker.all().len(), 2);
}

#[test]
fn the_row_carries_the_id_the_client_starred() {
    let tracker = new_tracker();
    tracker.begin(
        "soulseek",
        "12345",
        Some("ext-soulseek-song-12345"),
        Some("alice"),
        None,
        None,
        None,
    );

    let row = only(&tracker, "alice");
    assert_eq!(row.id, "ext-soulseek-song-12345");
    assert_eq!(row.external_id, "12345");
}

// --- The source chain -----------------------------------------------------------------

fn chain(
    tracker: &Arc<AcquisitionTracker>,
    order: &[HeartDownloadSource],
) -> (Arc<HeartAcquisitionCoordinator>, Arc<TrackAcquisitionQueue>) {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            heart_download_sources: order
                .iter()
                .map(|source| HeartDownloadStep {
                    source: *source,
                    enabled: Some(true),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let coordinator = HeartAcquisitionCoordinator::new(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        Arc::new(FakeLidarr::default()),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            ..Default::default()
        },
    );
    (coordinator, queue)
}

async fn next(queue: &TrackAcquisitionQueue) -> Arc<AcquisitionRequest> {
    tokio::time::timeout(Duration::from_secs(10), queue.dequeue(&CancellationToken::new()))
        .await
        .expect("a request within ten seconds")
        .expect("a request")
}

#[tokio::test]
async fn a_fallback_chain_stays_one_row_and_fails_only_at_the_end() {
    let tracker = new_tracker();
    let (coordinator, queue) = chain(
        &tracker,
        &[HeartDownloadSource::Soulseek, HeartDownloadSource::YouTube],
    );
    tracker.begin(
        "soulseek",
        "abc",
        Some("abc"),
        Some("alice"),
        Some("Radiohead"),
        Some("Creep"),
        None,
    );

    let running = coordinator.clone();
    let acquisition = tokio::spawn(async move {
        running
            .acquire_track("soulseek", "abc", Some("alice"), None)
            .await
    });

    let first = next(&queue).await;
    let queued = only(&tracker, "alice");
    assert_eq!(queued.state, AcquisitionState::Queued);
    assert_eq!(queued.source.as_deref(), Some("Soulseek"));

    // What the Soulseek path reports before its last peer gives up.
    tracker.stage(
        "soulseek",
        "abc",
        AcquisitionState::Searching,
        Some("Soulseek"),
        None,
    );
    tracker.transfer("soulseek", "abc", Some(400), Some(1000), None, None);
    queue.release(&first);
    first
        .completion
        .try_set_error(anyhow::anyhow!("All 5 Soulseek peer attempts failed"));

    let second = next(&queue).await;
    let fell_back = only(&tracker, "alice");
    assert_eq!(fell_back.state, AcquisitionState::Searching);
    assert_eq!(fell_back.source.as_deref(), Some("YouTube"));
    assert_eq!(
        fell_back.note.as_deref(),
        Some("Soulseek couldn't get it, trying YouTube")
    );
    assert_eq!(fell_back.error, None);
    assert_eq!(fell_back.progress, None);
    assert_eq!(fell_back.bytes_done, None);

    queue.release(&second);
    second
        .completion
        .try_set_error(anyhow::anyhow!("No YouTube match for 'Radiohead - Creep'"));
    acquisition.await.expect("joined").expect("the chain ran");

    let all = tracker.all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].state, AcquisitionState::Failed);
    assert_eq!(
        all[0].error.as_deref(),
        Some("No YouTube match for 'Radiohead - Creep'")
    );
    assert_eq!(all[0].source.as_deref(), Some("YouTube"));
}

#[tokio::test]
async fn a_fallback_that_succeeds_never_shows_the_earlier_failure() {
    let tracker = new_tracker();
    let (coordinator, queue) = chain(
        &tracker,
        &[HeartDownloadSource::Soulseek, HeartDownloadSource::YouTube],
    );
    begin(&tracker, "abc", "alice");

    let running = coordinator.clone();
    let acquisition = tokio::spawn(async move {
        running
            .acquire_track("soulseek", "abc", Some("alice"), None)
            .await
    });
    let first = next(&queue).await;
    queue.release(&first);
    first.completion.try_set_error(anyhow::anyhow!("no peer"));

    let second = next(&queue).await;
    tracker.imported("soulseek", "abc", Some("A"), Some("B"), Some("/music/b.mp3"));
    queue.release(&second);
    second.completion.try_set_result("/music/b.mp3");
    acquisition.await.expect("joined").expect("the chain ran");

    let all = tracker.all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].state, AcquisitionState::Done);
    assert_eq!(all[0].error, None);
}

// --- Albums --------------------------------------------------------------------------

#[test]
fn an_album_heart_lists_every_track_for_whoever_hearted_it() {
    let tracker = new_tracker();
    tracker.begin_album("soulseek", "alb", Some("bob"));

    tracker.announce(
        "soulseek",
        Some("alb"),
        None,
        &tracks(&[
            ("t1", "Air", "La Femme d'Argent", "Moon Safari"),
            ("t2", "Air", "Sexy Boy", "Moon Safari"),
        ]),
    );

    let rows = tracker.for_user("bob");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.state == AcquisitionState::Queued));
    assert!(rows.iter().all(|row| row.album.as_deref() == Some("Moon Safari")));
}

#[test]
fn a_walk_nobody_hearted_lists_nothing() {
    let tracker = new_tracker();

    tracker.announce("soulseek", Some("alb"), None, &tracks(&[("t1", "A", "B", "C")]));

    assert!(tracker.all().is_empty());
}

#[test]
fn a_walk_started_by_a_hearted_song_belongs_to_its_owner() {
    let tracker = new_tracker();
    begin(&tracker, "t0", "carol");

    tracker.announce(
        "soulseek",
        Some("alb"),
        Some("t0"),
        &tracks(&[("t1", "A", "B", "C")]),
    );

    assert_eq!(tracker.for_user("carol").len(), 2);
}

fn by_id(rows: Vec<AcquisitionSnapshot>) -> std::collections::HashMap<String, AcquisitionSnapshot> {
    rows.into_iter().map(|row| (row.id.clone(), row)).collect()
}

#[test]
fn fail_album_closes_only_tracks_still_running() {
    let tracker = new_tracker();
    tracker.begin_album("soulseek", "alb", Some("bob"));
    tracker.announce(
        "soulseek",
        Some("alb"),
        None,
        &tracks(&[("t1", "A", "One", "C"), ("t2", "A", "Two", "C")]),
    );
    tracker.imported("soulseek", "t1", Some("A"), Some("One"), Some("/music/one.flac"));

    tracker.fail_album(
        "soulseek",
        "alb",
        Some("Lidarr import timed out after 30 minute(s)."),
    );

    let rows = by_id(tracker.for_user("bob"));
    assert_eq!(rows["t1"].state, AcquisitionState::Done);
    assert_eq!(rows["t2"].state, AcquisitionState::Failed);
    assert_eq!(
        rows["t2"].error.as_deref(),
        Some("Lidarr import timed out after 30 minute(s)")
    );
}

#[test]
fn announce_requeues_a_failed_track_but_leaves_a_done_one_alone() {
    let tracker = new_tracker();
    let list = tracks(&[("t1", "A", "One", "C"), ("t2", "A", "Two", "C")]);
    tracker.begin_album("soulseek", "alb", Some("bob"));
    tracker.announce("soulseek", Some("alb"), None, &list);
    tracker.complete("soulseek", "t1", None);
    tracker.fail("soulseek", "t2", Some("no peer"));

    tracker.begin_album("soulseek", "alb", Some("bob"));
    tracker.announce("soulseek", Some("alb"), None, &list);

    let rows = by_id(tracker.for_user("bob"));
    assert_eq!(rows["t1"].state, AcquisitionState::Done);
    assert_eq!(rows["t2"].state, AcquisitionState::Queued);
}

// --- Expiry and the cap --------------------------------------------------------------

#[test]
fn a_finished_row_stays_three_hours_then_goes() {
    let clock = ManualClock::new();
    let tracker = tracker_at(&clock);
    begin(&tracker, "abc", "alice");
    tracker.complete("soulseek", "abc", None);

    clock.advance(TimeDelta::hours(3) - TimeDelta::minutes(1));
    assert_eq!(tracker.for_user("alice").len(), 1);

    clock.advance(TimeDelta::minutes(1));
    assert!(tracker.for_user("alice").is_empty());
}

#[test]
fn a_running_row_outlives_thirty_minutes_but_not_a_day_of_silence() {
    let clock = ManualClock::new();
    let tracker = tracker_at(&clock);
    begin(&tracker, "abc", "alice");

    clock.advance(TimeDelta::hours(2));
    assert_eq!(only(&tracker, "alice").state, AcquisitionState::Queued);

    clock.advance(TimeDelta::hours(22));
    assert!(tracker.all().is_empty());
}

#[test]
fn the_cap_drops_finished_rows_first_then_the_oldest() {
    let clock = ManualClock::new();
    let tracker = tracker_at(&clock);
    begin(&tracker, "finished", "alice");
    tracker.complete("soulseek", "finished", None);

    for i in 0..AcquisitionTracker::CAPACITY {
        clock.advance(TimeDelta::seconds(1));
        begin(&tracker, &format!("run{i}"), "alice");
    }

    let ids: std::collections::HashSet<String> = tracker.all().into_iter().map(|row| row.id).collect();
    assert_eq!(ids.len(), AcquisitionTracker::CAPACITY);
    assert!(!ids.contains("finished"));
    assert!(ids.contains("run0"));

    clock.advance(TimeDelta::seconds(1));
    begin(&tracker, "newest", "alice");

    let ids: std::collections::HashSet<String> = tracker.all().into_iter().map(|row| row.id).collect();
    assert_eq!(ids.len(), AcquisitionTracker::CAPACITY);
    assert!(!ids.contains("run0"));
    assert!(ids.contains("newest"));
}

// --- Held rows ---------------------------------------------------------------------------

fn held_record(id: &str, waiting: WaitingFor) -> HeldAcquisition {
    HeldAcquisition {
        provider: "soulseek".into(),
        external_id: id.into(),
        requested_by: Some("alice".into()),
        waiting_for: Some(waiting),
        ..Default::default()
    }
}

#[test]
fn a_held_row_outlives_a_day_and_its_album_claim_stays_with_it() {
    let clock = ManualClock::new();
    let tracker = tracker_at(&clock);
    tracker.begin_album("soulseek", "help-album", Some("alice"));
    tracker.announce(
        "soulseek",
        Some("help-album"),
        None,
        &tracks(&[("held", "The Beatles", "Yesterday", "Help!")]),
    );
    tracker.hold_row(
        "soulseek",
        "held",
        &["alice".to_string(), "bob".to_string()],
        "Waiting for MusicBrainz",
        false,
    );
    begin(&tracker, "plain", "alice");

    clock.advance(TimeDelta::hours(25));
    let rows = tracker.all();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].external_id, "held");
    assert_eq!(rows[0].state, AcquisitionState::Searching);
    assert_eq!(rows[0].note.as_deref(), Some("Waiting for MusicBrainz"));
    assert_eq!(rows[0].requested_by, ["alice", "bob"]);
    assert!(!rows[0].keep_anyway);
    // The album the held track is listed under still lists it for whoever hearted it.
    tracker.begin_album("soulseek", "help-album", Some("carol"));
    tracker.announce(
        "soulseek",
        Some("help-album"),
        None,
        &tracks(&[("other", "The Beatles", "Help!", "Help!")]),
    );
    let other = tracker
        .for_user("alice")
        .into_iter()
        .find(|row| row.external_id == "other")
        .expect("listed for alice, whose album claim survived the day");
    assert_eq!(other.state, AcquisitionState::Queued);
}

#[test]
fn five_hundred_and_ten_rows_with_twenty_held_keep_every_held_one() {
    let clock = ManualClock::new();
    let tracker = tracker_at(&clock);
    for i in 0..20 {
        clock.advance(TimeDelta::seconds(1));
        tracker.hold_row(
            "soulseek",
            &format!("held{i}"),
            &["alice".to_string()],
            "Waiting",
            true,
        );
    }
    for i in 0..490 {
        clock.advance(TimeDelta::seconds(1));
        begin(&tracker, &format!("run{i}"), "alice");
    }
    let rows = tracker.all();
    assert_eq!(rows.len(), AcquisitionTracker::CAPACITY);
    let held: Vec<&AcquisitionSnapshot> = rows.iter().filter(|row| row.keep_anyway).collect();
    assert_eq!(held.len(), 20, "every held row kept");
}

#[test]
fn a_held_row_is_let_go_only_once_no_hold_of_its_song_is_left() {
    let clock = ManualClock::new();
    let tracker = tracker_at(&clock);
    let holds = SoulseekHoldStore::new(None::<std::path::PathBuf>);
    let mb = holds.hold(held_record("song", WaitingFor::MusicBrainz));
    holds.hold(held_record("song", WaitingFor::AcoustIdOff));
    tracker.hold_row(
        "soulseek",
        "song",
        &["alice".to_string()],
        "AcoustID is off",
        true,
    );
    assert!(only(&tracker, "alice").keep_anyway);

    holds.release(&mb.key());
    tracker.release_row("soulseek", "song", &holds);
    assert!(
        tracker.is_held("soulseek", "song"),
        "the AcoustIdOff hold is still there"
    );

    holds.release(&held_record("song", WaitingFor::AcoustIdOff).key());
    tracker.release_row("soulseek", "song", &holds);
    assert!(!tracker.is_held("soulseek", "song"));
    assert!(!only(&tracker, "alice").keep_anyway);
    clock.advance(TimeDelta::hours(25));
    assert!(tracker.all().is_empty(), "back to the usual rules");
}

/// A copy that waits aside for a Lidarr follow: the done row says so over its own line, never
/// expires while it waits, and says its own line again once the wait ends, or keeps the line
/// that says why it ended.
#[test]
fn a_row_whose_copy_waits_aside_says_so_outlives_a_day_and_ends_with_its_own_line() {
    const WAITS: &str = "Plays now; joins your library when Lidarr's download for this artist finishes";
    const PROVEN: &str = "Octo stopped waiting for Lidarr: another copy was proven";
    let clock = ManualClock::new();
    let tracker = tracker_at(&clock);
    begin(&tracker, "kept", "alice");
    tracker.done_line("soulseek", "kept", "Kept the version you heard");
    tracker.complete("soulseek", "kept", None);
    tracker.wait_aside("soulseek", "kept", &["alice".to_string()], WAITS);
    assert_eq!(only(&tracker, "alice").note.as_deref(), Some(WAITS));
    assert_eq!(only(&tracker, "alice").state, AcquisitionState::Done);

    clock.advance(TimeDelta::hours(25));
    assert_eq!(
        only(&tracker, "alice").note.as_deref(),
        Some(WAITS),
        "still listed after a day"
    );

    tracker.end_aside("soulseek", "kept", None);
    assert_eq!(
        only(&tracker, "alice").note.as_deref(),
        Some("Kept the version you heard"),
        "its own line again"
    );
    clock.advance(TimeDelta::hours(4));
    assert!(tracker.all().is_empty(), "back to the usual rules");

    // A row a restart left out comes back, done, for the people the copy waits for; a proven
    // copy that ended the wait is its last line.
    tracker.wait_aside("soulseek", "again", &["bob".to_string()], WAITS);
    let row = only(&tracker, "bob");
    assert_eq!(row.state, AcquisitionState::Done);
    assert_eq!(row.note.as_deref(), Some(WAITS));
    tracker.end_aside("soulseek", "again", Some(PROVEN));
    assert_eq!(only(&tracker, "bob").note.as_deref(), Some(PROVEN));
    let lines: Vec<String> = tracker
        .detail("soulseek:again", None)
        .and_then(|row| row.events)
        .unwrap_or_default()
        .into_iter()
        .map(|event| event.text)
        .collect();
    assert_eq!(lines, [WAITS, PROVEN]);
}

// --- Seeing it in Navidrome ------------------------------------------------------------

async fn wait_for(tracker: &AcquisitionTracker, user: &str, state: AcquisitionState) -> AcquisitionSnapshot {
    for _ in 0..1000 {
        let rows = tracker.for_user(user);
        if rows.len() == 1 && rows[0].state == state {
            return rows.into_iter().next().expect("one row");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never reached {state:?}");
}

#[tokio::test]
async fn imported_waits_for_navidrome_and_records_the_library_id() {
    let tracker = new_tracker();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    tracker.configure_watch(|watch| {
        watch.visibility_poll = Duration::from_millis(10);
        watch.library_lookup = Some(lookup(move || {
            Ok((counted.fetch_add(1, Ordering::SeqCst) + 1 >= 3).then(|| "nd-song-1".to_string()))
        }));
    });
    begin(&tracker, "abc", "alice");

    tracker.imported("soulseek", "abc", Some("A"), Some("B"), Some("/music/b.flac"));
    assert_eq!(only(&tracker, "alice").state, AcquisitionState::Importing);

    let row = wait_for(&tracker, "alice", AcquisitionState::Done).await;
    assert_eq!(row.library_id.as_deref(), Some("nd-song-1"));
}

#[tokio::test]
async fn imported_is_done_without_an_id_when_navidrome_never_shows_it() {
    let tracker = new_tracker();
    tracker.configure_watch(|watch| {
        watch.visibility_poll = Duration::from_millis(5);
        watch.visibility_attempts = 3;
        watch.slow_visibility_attempts = 0;
        watch.library_lookup = Some(lookup(|| Err(anyhow::anyhow!("navidrome down"))));
    });
    begin(&tracker, "abc", "alice");

    tracker.imported("soulseek", "abc", Some("A"), Some("B"), Some("/music/b.flac"));

    let row = wait_for(&tracker, "alice", AcquisitionState::Done).await;
    assert_eq!(row.library_id, None);
}

#[tokio::test]
async fn a_song_still_missing_after_a_minute_asks_navidrome_to_scan_once_and_is_found_after_it() {
    let tracker = new_tracker();
    let scans = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (scanned, looked, seen) = (scans.clone(), calls.clone(), scans.clone());
    tracker.configure_watch(|watch| {
        watch.visibility_poll = Duration::from_millis(2);
        watch.slow_visibility_poll = Duration::from_millis(2);
        watch.visibility_attempts = 4;
        watch.slow_visibility_attempts = 10;
        watch.rescan_after_attempts = 6;
        watch.rescan = Some(Arc::new(move || {
            scanned.fetch_add(1, Ordering::SeqCst);
            async {}.boxed()
        }));
        // Navidrome shows the song only once the forced scan has run.
        watch.library_lookup = Some(lookup(move || {
            looked.fetch_add(1, Ordering::SeqCst);
            Ok((seen.load(Ordering::SeqCst) > 0).then(|| "nd-song-9".to_string()))
        }));
    });
    begin(&tracker, "abc", "alice");

    tracker.imported("soulseek", "abc", Some("A"), Some("B"), Some("/music/b.flac"));

    let row = wait_for(&tracker, "alice", AcquisitionState::Done).await;
    assert_eq!(row.library_id.as_deref(), Some("nd-song-9"));
    assert_eq!(scans.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 7);
}

#[tokio::test]
async fn the_watch_outlasts_the_fast_polls_before_giving_up() {
    let tracker = new_tracker();
    let calls = Arc::new(AtomicUsize::new(0));
    let looked = calls.clone();
    tracker.configure_watch(|watch| {
        watch.visibility_poll = Duration::from_millis(2);
        watch.slow_visibility_poll = Duration::from_millis(2);
        watch.visibility_attempts = 3;
        watch.slow_visibility_attempts = 5;
        watch.library_lookup = Some(lookup(move || {
            looked.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }));
    });
    begin(&tracker, "abc", "alice");

    tracker.imported("soulseek", "abc", Some("A"), Some("B"), Some("/music/b.flac"));

    let row = wait_for(&tracker, "alice", AcquisitionState::Done).await;
    assert_eq!(row.library_id, None);
    assert_eq!(calls.load(Ordering::SeqCst), 8);
}

// --- Words beside the ring -------------------------------------------------------------

#[test]
fn a_transfer_that_has_not_moved_a_byte_has_no_progress_rather_than_zero() {
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");

    tracker.transfer(
        "soulseek",
        "abc",
        Some(0),
        Some(29_000_000),
        Some(0.0),
        Some("Soulseek"),
    );
    let connecting = only(&tracker, "alice");
    assert_eq!(connecting.state, AcquisitionState::Downloading);
    assert_eq!(connecting.progress, None);
    assert_eq!(connecting.bytes_total, Some(29_000_000));

    tracker.transfer(
        "soulseek",
        "abc",
        Some(2_900_000),
        Some(29_000_000),
        Some(10.0),
        Some("Soulseek"),
    );
    assert_eq!(only(&tracker, "alice").progress, Some(0.1));
}

#[test]
fn a_queued_song_counts_the_downloads_ahead_of_it_whoever_asked_for_them() {
    let tracker = new_tracker();
    begin(&tracker, "first", "bob");
    tracker.announce(
        "soulseek",
        None,
        Some("first"),
        &tracks(&[("t1", "A", "One", "Album"), ("t2", "A", "Two", "Album")]),
    );
    begin(&tracker, "mine", "alice");

    assert_eq!(only(&tracker, "alice").ahead, Some(3));

    // Running and finished rows have no place in the queue, and finished ones free a slot.
    tracker.transfer("soulseek", "first", Some(1), Some(2), None, None);
    tracker.fail("soulseek", "t1", Some("gone"));
    let bob = by_id(tracker.for_user("bob"));
    assert_eq!(bob["first"].ahead, None);
    assert_eq!(only(&tracker, "alice").ahead, Some(2));
    assert_eq!(bob["t2"].ahead, Some(1));
}

#[test]
fn a_note_stays_until_another_replaces_it_and_goes_when_the_song_arrives() {
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");

    tracker.stage(
        "soulseek",
        "abc",
        AcquisitionState::Searching,
        Some("YouTube"),
        Some("Soulseek couldn't get it, trying YouTube"),
    );
    tracker.stage("soulseek", "abc", AcquisitionState::Searching, None, None);
    assert_eq!(
        only(&tracker, "alice").note.as_deref(),
        Some("Soulseek couldn't get it, trying YouTube")
    );

    tracker.complete("soulseek", "abc", Some("nd-1"));
    assert_eq!(only(&tracker, "alice").note, None);
}

/// The tracker half of a transfer poll: slskd's figures for a transfer part way through
/// (12,180,000 of 29,000,000 bytes, 42%) become the row's progress. Reading them out of slskd's
/// answer is tested with `SoulseekClient`.
#[test]
fn a_transfer_poll_becomes_the_rows_progress() {
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");
    tracker.transfer(
        "soulseek",
        "abc",
        Some(12_180_000),
        Some(29_000_000),
        Some(42.0),
        Some("Soulseek"),
    );

    let row = only(&tracker, "alice");
    assert_eq!(row.state, AcquisitionState::Downloading);
    assert_eq!(row.progress, Some(0.42));
    assert_eq!(row.bytes_done, Some(12_180_000));
    assert_eq!(row.bytes_total, Some(29_000_000));
}

// --- listeners and keys ---------------------------------------------------------------

/// Listeners hear each end once, outside the lock; one that panics is skipped and the next is
/// still told; an unsubscribed one hears nothing.
#[test]
fn listeners_hear_each_end_and_a_panicking_one_is_skipped() {
    let tracker = new_tracker();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let record = heard.clone();
    tracker.subscribe_ended(Arc::new(|_| panic!("a listener that throws")));
    let id = tracker.subscribe_ended(Arc::new(move |end: &AcquisitionEnd| {
        record.lock().push(end.clone())
    }));
    tracker.begin_album("soulseek", "alb", Some("bob"));
    tracker.announce("soulseek", Some("alb"), None, &tracks(&[("t1", "A", "One", "C")]));

    tracker.complete("soulseek", "t1", Some("nd-1"));
    tracker.complete("soulseek", "t1", Some("nd-2"));

    let heard_now = heard.lock().clone();
    assert_eq!(heard_now.len(), 1);
    assert_eq!(heard_now[0].key, "soulseek:t1");
    assert!(heard_now[0].done);
    assert_eq!(heard_now[0].library_id.as_deref(), Some("nd-1"));
    assert_eq!(heard_now[0].album_keys, ["soulseek:alb"]);

    tracker.unsubscribe_ended(id);
    begin(&tracker, "t2", "bob");
    tracker.fail("soulseek", "t2", None);
    assert_eq!(heard.lock().len(), 1);
    assert_eq!(
        by_id(tracker.for_user("bob"))["t2"].error.as_deref(),
        Some("The download failed.")
    );
}

/// The key is the provider trimmed and lower case, the id as it is.
#[test]
fn the_key_is_the_provider_in_lower_case_and_the_id_as_given() {
    assert_eq!(key_of(" SoulSeek ", "AbC"), "soulseek:AbC");
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");
    tracker.stage("SOULSEEK", "abc", AcquisitionState::Searching, None, None);
    assert_eq!(only(&tracker, "alice").state, AcquisitionState::Searching);
    assert_eq!(AcquisitionState::Searching.wire_name(), "searching");
}

/// A check line keeps the other recordings AcoustID heard; a line without any keeps none.
#[test]
fn a_check_line_keeps_what_acoust_id_heard() {
    let tracker = new_tracker();
    tracker.begin(
        "soulseek",
        "t1",
        None,
        Some("alice"),
        Some("Massive Attack"),
        Some("Teardrop"),
        None,
    );
    let heard = vec![AcquisitionHeard {
        title: "Angel".into(),
        artist: "Massive Attack".into(),
        score: 40,
    }];
    tracker.log_heard(
        "soulseek",
        "t1",
        acquisition_event_kinds::CHECK,
        "Passed the checks",
        Some("the right length; AcoustID: the same recording, 92% sure (needs 85%)"),
        heard.clone(),
    );
    tracker.log_heard(
        "soulseek",
        "t1",
        acquisition_event_kinds::NOTE,
        "Nothing heard",
        None,
        Vec::new(),
    );

    let row = tracker.detail(&key_of("soulseek", "t1"), None).expect("the row");
    let events = row.events.expect("the log");
    let check = events
        .iter()
        .find(|e| e.text == "Passed the checks")
        .expect("the check line");
    assert_eq!(check.heard.as_ref(), Some(&heard));
    assert_eq!(
        check.detail.as_deref(),
        Some("the right length; AcoustID: the same recording, 92% sure (needs 85%)")
    );
    let plain = events
        .iter()
        .find(|e| e.text == "Nothing heard")
        .expect("the plain line");
    assert_eq!(plain.heard, None);
}

/// A heart that ended with nothing says why: a row that already failed says it instead of what it
/// failed with, a row left looking fails with it, and a row with a file on its way or landed keeps
/// its line.
#[test]
fn a_heart_that_ended_with_nothing_says_why_on_its_row() {
    let tracker = new_tracker();
    let why = "MusicBrainz could not answer for this song";
    for id in ["failed", "looking", "landing", "landed"] {
        tracker.begin("soulseek", id, None, Some("alice"), None, Some(id), None);
    }
    tracker.fail("soulseek", "failed", Some("No source had it"));
    tracker.stage("soulseek", "landing", AcquisitionState::Importing, None, None);
    tracker.complete("soulseek", "landed", Some("nd-1"));

    for id in ["failed", "looking", "landing", "landed"] {
        tracker.ended_with_nothing("soulseek", id, why);
    }

    let row = |id: &str| tracker.detail(&key_of("soulseek", id), None).expect("the row");
    assert_eq!(row("failed").error.as_deref(), Some(why));
    assert_eq!(row("looking").state, AcquisitionState::Failed);
    assert_eq!(row("looking").error.as_deref(), Some(why));
    assert_eq!(row("landing").state, AcquisitionState::Importing);
    assert_eq!(row("landed").state, AcquisitionState::Done);
    assert_ne!(row("landed").error.as_deref(), Some(why));
}
