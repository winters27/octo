//! Keep anyway, in the coordinator: only a person waiting in a hold that offers it sees it or may
//! ask for it, an album's held tracks are counted on its row, and the request is saved before
//! anything is carried out. "Keep these anyway" lands every held track's file for the people who
//! hearted the album; a song keeps only the caller's own heard audio and leaves the others held;
//! YouTube not answering for the heard video keeps the request waiting for the shim's round; a gone
//! video ends that person's heart with the plain line; a restart in the middle finishes the keep;
//! and a held row a day old is still listed and still kept.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use octo_core::common::Clock;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardRecord, HeardRecording, HeardSource};
use octo_core::settings::{AppSettings, DownloadSource, SettingsStore};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::super::waiting_line;
use super::*;
use crate::services::common::acquisition_tracker::acquisition_event_kinds;
use crate::services::common::album_gates::tracks_wait_line;
use crate::services::common::soulseek_hold_store::{HeldAlbumTrack, SoulseekHoldStore};
use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr, until};
use crate::services::common::{
    AcquisitionRequest, AcquisitionState, AcquisitionTracker, HeartCoordinatorExtras, SoulseekHoldResumer,
    TrackAcquisitionQueue,
};

struct Rig {
    coordinator: Arc<HeartAcquisitionCoordinator>,
    queue: Arc<TrackAcquisitionQueue>,
    holds: Arc<SoulseekHoldStore>,
    tracker: Arc<AcquisitionTracker>,
}

/// A coordinator over `holds` and `tracker`, its downloads queued on a queue of its own.
fn rig_over(holds: Arc<SoulseekHoldStore>, tracker: Arc<AcquisitionTracker>) -> Rig {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let coordinator = HeartAcquisitionCoordinator::new(
        Arc::new(SettingsStore::for_tests(AppSettings::default())),
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        Arc::new(FakeLidarr::default()),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            holds: Some(holds.clone()),
            ..Default::default()
        },
    );
    Rig {
        coordinator,
        queue,
        holds,
        tracker,
    }
}

/// A coordinator whose holds are kept in memory and whose rows go by the system clock.
fn memory_rig() -> Rig {
    rig_over(
        Arc::new(SoulseekHoldStore::new(None::<PathBuf>)),
        Arc::new(AcquisitionTracker::new(None, Clock::system())),
    )
}

type Taken = Arc<Mutex<Vec<Arc<AcquisitionRequest>>>>;

/// Hands every request the queue gives out to `answer`, keeping each: a path lands it there (its
/// row done, as the download's placement leaves it), an error fails it.
fn work(
    rig: &Rig,
    answer: impl Fn(&AcquisitionRequest) -> anyhow::Result<String> + Send + Sync + 'static,
) -> (Taken, CancellationToken) {
    let taken: Taken = Arc::default();
    let stop = CancellationToken::new();
    let (queue, seen, token, tracker) = (
        rig.queue.clone(),
        taken.clone(),
        stop.clone(),
        rig.tracker.clone(),
    );
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            seen.lock().push(request.clone());
            let answered = answer(&request);
            queue.release(&request);
            match answered {
                Ok(path) => {
                    tracker.complete(&request.provider, &request.external_id, None);
                    request.completion.try_set_result(path);
                }
                Err(failure) => {
                    request.completion.try_set_error(failure);
                }
            }
        }
    });
    (taken, stop)
}

/// Lands each request: a held file where it is, the heard audio as an m4a.
fn lands(request: &AcquisitionRequest) -> anyhow::Result<String> {
    Ok(request
        .place()
        .held_files
        .first()
        .cloned()
        .unwrap_or_else(|| format!("/music/{}.m4a", request.external_id)))
}

/// The video `user` heard.
fn heard(user: &str) -> HeardRecord {
    HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Video {
            id: format!("{user}-video"),
            size: Some(1000),
            content_type: "audio/mp4".into(),
            itag: Some(140),
        },
        sample_id: None,
    }
}

/// The song `id` held for `waiting`, each of `users` waiting in it with the video they heard.
fn song_hold(id: &str, waiting: WaitingFor, users: &[&str]) -> HeldAcquisition {
    HeldAcquisition {
        kind: HeldKind::Track,
        provider: "soulseek".into(),
        external_id: id.into(),
        requested_by: users.first().map(|user| user.to_string()),
        held_since_utc: Utc::now(),
        origin: Some(AcquisitionOrigin::Heart),
        waiting_for: Some(waiting),
        holders: users
            .iter()
            .map(|user| Holder {
                user: user.to_string(),
                heard: Some(heard(user)),
                ..Default::default()
            })
            .collect(),
        line: Some(waiting_line(waiting).to_string()),
        ..Default::default()
    }
}

/// The catalog recording of the album track `id`.
fn recording_of(id: &str) -> HeardRecording {
    HeardRecording {
        ids: vec![format!("rec-{id}")],
        title: id.into(),
        artist: "The Beatles".into(),
        ..Default::default()
    }
}

/// The held track `id` of the hearted album "help-album", holding `file`, for `users`.
fn album_track(id: &str, file: &str, users: &[&str]) -> HeldAcquisition {
    HeldAcquisition {
        kind: HeldKind::Track,
        provider: "soulseek".into(),
        external_id: id.into(),
        requested_by: users.first().map(|user| user.to_string()),
        held_since_utc: Utc::now(),
        origin: Some(AcquisitionOrigin::AlbumHeart),
        waiting_for: Some(WaitingFor::NotYetProven),
        holders: users
            .iter()
            .map(|user| Holder {
                user: user.to_string(),
                ..Default::default()
            })
            .collect(),
        held_files: vec![file.into()],
        album_track: Some(HeldAlbumTrack {
            album_key: "soulseek:help-album".into(),
            song_ids: vec![id.into()],
            artist: "The Beatles".into(),
            title: id.into(),
            duration_seconds: Some(139),
            r: recording_of(id),
        }),
        line: Some(tracks_wait_line(1)),
        ..Default::default()
    }
}

/// A held file named `name` in `dir`.
fn held_file(dir: &tempfile::TempDir, name: &str) -> String {
    let path = dir.path().join(format!("{name}.flac"));
    std::fs::write(&path, b"a held track").expect("written");
    path.to_string_lossy().to_string()
}

fn users(record: &HeldAcquisition) -> Vec<String> {
    record.holders.iter().map(|holder| holder.user.clone()).collect()
}

fn notes(tracker: &AcquisitionTracker, key: &str) -> Vec<String> {
    tracker
        .detail(key, None)
        .and_then(|row| row.events)
        .unwrap_or_default()
        .into_iter()
        .filter(|event| event.kind == acquisition_event_kinds::NOTE)
        .map(|event| event.text)
        .collect()
}

/// Keep anyway is offered to a person waiting in a hold that offers it (a song held while
/// AcoustID is off, a hearted album's held track, on its own row and its album's), never on a
/// hold for anything else, never to anyone else (an admin included), and no more once they asked.
/// An album row counts its held tracks, whoever looks; a song row and a track's own row count none.
#[tokio::test]
async fn keep_anyway_is_offered_only_to_a_holder_and_an_album_row_counts_its_held_tracks() {
    let rig = memory_rig();
    let dir = tempfile::tempdir().expect("a temp dir");
    rig.holds
        .hold(song_hold("help", WaitingFor::AcoustIdOff, &["brandon"]));
    rig.holds
        .hold(song_hold("yesterday", WaitingFor::MusicBrainz, &["brandon"]));
    rig.holds
        .hold(album_track("night", &held_file(&dir, "night"), &["brandon"]));
    rig.holds
        .hold(album_track("day", &held_file(&dir, "day"), &["brandon"]));

    let marks = rig.coordinator.keep_marks("Brandon");
    assert!(marks.keeps("soulseek:help"));
    assert!(!marks.keeps("soulseek:yesterday"), "a hold for MusicBrainz");
    assert!(marks.keeps("soulseek:help-album") && marks.keeps("soulseek:night"));
    assert_eq!(marks.held_tracks("soulseek:help-album"), Some(2));
    assert_eq!(marks.held_tracks("soulseek:help"), None, "a song row");
    assert_eq!(marks.held_tracks("soulseek:night"), None, "a track's own row");
    let mut row = AcquisitionSnapshot {
        key: "soulseek:help-album".into(),
        ..Default::default()
    };
    marks.mark(&mut row);
    assert!(row.keep_anyway);
    assert_eq!(row.held_tracks, Some(2));

    let admin = rig.coordinator.keep_marks("admin");
    assert!(!admin.keeps("soulseek:help") && !admin.keeps("soulseek:help-album"));
    assert_eq!(admin.held_tracks("soulseek:help-album"), Some(2));
    assert_eq!(rig.coordinator.keepers("soulseek:help"), ["brandon"]);

    rig.holds
        .update("Track:soulseek:help:AcoustIdOff", |record| {
            record.keep_requested.push("brandon".into())
        })
        .expect("held");
    assert!(!rig.coordinator.keep_marks("brandon").keeps("soulseek:help"));
    assert!(rig.coordinator.keepers("soulseek:help").is_empty());
}

/// "Keep these anyway" is saved in every held track's hold before it answers, then each track's
/// file lands as a keep for the people who hearted the album, nothing heard read for them; the
/// album's row lets go once none waits. Another member is no holder; asking twice, or once nothing
/// waits, keeps nothing more.
#[tokio::test]
async fn keep_these_anyway_lands_every_held_track_for_the_hearting_user() {
    let rig = memory_rig();
    let dir = tempfile::tempdir().expect("a temp dir");
    let night = rig
        .holds
        .hold(album_track("night", &held_file(&dir, "night"), &["brandon"]));
    let day = rig
        .holds
        .hold(album_track("day", &held_file(&dir, "day"), &["brandon"]));
    rig.coordinator.hold_row_for(&night);
    rig.coordinator.hold_row_for(&day);
    rig.holds
        .hold(song_hold("yesterday", WaitingFor::MusicBrainz, &["brandon"]));
    assert!(rig.tracker.is_held("soulseek", "help-album"));

    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help-album", "anna"),
        KeepAnyway::NotAHolder
    );
    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:yesterday", "brandon"),
        KeepAnyway::NothingWaiting,
        "a hold for MusicBrainz keeps nothing"
    );
    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help-album", "brandon"),
        KeepAnyway::Keeping
    );
    for key in [night.key(), day.key()] {
        assert_eq!(
            rig.holds.get(&key).expect("held").keep_requested,
            ["brandon"],
            "saved before the answer"
        );
    }
    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help-album", "brandon"),
        KeepAnyway::NothingWaiting,
        "asked already"
    );

    let (taken, stop) = work(&rig, lands);
    until(|| rig.holds.records_for("Track:soulseek:night").is_empty()).await;
    until(|| rig.holds.records_for("Track:soulseek:day").is_empty()).await;
    until(|| !rig.tracker.is_held("soulseek", "help-album")).await;

    let taken = taken.lock().clone();
    let mut kept: Vec<(String, Vec<String>)> = taken
        .iter()
        .map(|request| (request.external_id.clone(), request.place().held_files))
        .collect();
    kept.sort();
    assert_eq!(
        kept,
        [
            ("day".to_string(), vec![day.held_files[0].clone()]),
            ("night".to_string(), vec![night.held_files[0].clone()]),
        ]
    );
    for request in &taken {
        assert!(request.place().keep_anyway, "a keep");
        assert_eq!(request.origin, Some(AcquisitionOrigin::AlbumHeart));
        assert_eq!(request.source_override, Some(DownloadSource::Soulseek));
        assert_eq!(request.requested_by(), ["brandon"]);
        let gates = request.gates();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].user, "brandon");
        assert_eq!(gates[0].heard, None, "nothing heard is read");
        assert_eq!(gates[0].r, Some(recording_of(&request.external_id)));
    }
    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help-album", "brandon"),
        KeepAnyway::NothingHeld,
        "nothing waits once kept"
    );
    stop.cancel();
}

/// On a song record with two holders, Keep anyway keeps only the caller's own heard audio (A0 of
/// what they heard, nothing else fetched for it) and leaves the other held, still offered it.
#[tokio::test]
async fn on_a_song_with_two_holders_keep_anyway_keeps_only_the_callers_heard_audio() {
    let rig = memory_rig();
    let record = rig
        .holds
        .hold(song_hold("help", WaitingFor::AcoustIdOff, &["brandon", "anna"]));
    rig.coordinator.hold_row_for(&record);
    let (taken, stop) = work(&rig, lands);

    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help", "brandon"),
        KeepAnyway::Keeping
    );

    until(|| {
        rig.holds
            .get(&record.key())
            .is_some_and(|left| left.holders.len() == 1)
    })
    .await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    let left = rig.holds.get(&record.key()).expect("anna still waits");
    assert_eq!(users(&left), ["anna"]);
    assert!(left.keep_requested.is_empty());
    let taken = taken.lock().clone();
    assert_eq!(taken.len(), 1, "brandon's heard audio, nothing else");
    let request = &taken[0];
    assert!(request.place().keep_anyway);
    assert!(request.place().held_files.is_empty());
    assert_eq!(request.source_override, Some(DownloadSource::YouTube));
    assert_eq!(request.requested_by(), ["brandon"]);
    let gates = request.gates();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0].user, "brandon");
    assert_eq!(gates[0].reading, HeardReading::OnlyHeardOrMaster);
    assert_eq!(
        gates[0].heard.as_ref().map(|heard| heard.source.clone()),
        Some(heard("brandon").source)
    );
    assert!(rig.tracker.is_held("soulseek", "help"), "anna's row stays held");
    assert_eq!(
        rig.tracker.note_of("soulseek", "help").as_deref(),
        Some(waiting_line(WaitingFor::AcoustIdOff))
    );
    assert!(rig.coordinator.keep_marks("anna").keeps("soulseek:help"));
    assert!(!rig.coordinator.keep_marks("brandon").keeps("soulseek:help"));
    stop.cancel();
}

/// One person's keep ends their own hold of a song; the row stays held while another hold of the
/// song, another person's for MusicBrainz, still waits.
#[tokio::test]
async fn a_row_stays_held_while_another_hold_of_its_song_waits_after_a_keep() {
    let rig = memory_rig();
    let record = rig
        .holds
        .hold(song_hold("help", WaitingFor::AcoustIdOff, &["brandon"]));
    rig.coordinator.hold_row_for(&record);
    let other = rig
        .holds
        .hold(song_hold("help", WaitingFor::MusicBrainz, &["anna"]));
    rig.coordinator.hold_row_for(&other);
    let (_taken, stop) = work(&rig, lands);

    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help", "brandon"),
        KeepAnyway::Keeping
    );

    until(|| rig.holds.get(&record.key()).is_none()).await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    assert!(rig.holds.get(&other.key()).is_some());
    assert!(rig.tracker.is_held("soulseek", "help"));
    let row = rig.tracker.detail("soulseek:help", None).expect("the row");
    assert_eq!(row.state, AcquisitionState::Searching, "held for anna");
    assert_eq!(row.note.as_deref(), Some(waiting_line(WaitingFor::MusicBrainz)));
    stop.cancel();
}

/// A restart between saving the keeps and carrying them out finishes them on resume: a song held
/// while AcoustID is off keeps the heard audio, a hearted album's held track its file, and both
/// holds end, saved so.
#[tokio::test]
async fn a_restart_between_the_call_and_the_keep_finishes_the_keep() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("soulseek-holds.json");
    let night = held_file(&dir, "night");
    {
        let before = rig_over(
            Arc::new(SoulseekHoldStore::new(Some(&file))),
            Arc::new(AcquisitionTracker::new(None, Clock::system())),
        );
        before
            .holds
            .hold(song_hold("help", WaitingFor::AcoustIdOff, &["brandon"]));
        before.holds.hold(album_track("night", &night, &["brandon"]));
        // Nothing carries a keep out before the restart.
        assert_eq!(
            before.coordinator.keep_anyway("soulseek:help", "brandon"),
            KeepAnyway::Keeping
        );
        assert_eq!(
            before.coordinator.keep_anyway("soulseek:help-album", "brandon"),
            KeepAnyway::Keeping
        );
    }
    let saved = SoulseekHoldStore::new(Some(&file));
    let asked: Vec<Vec<String>> = saved
        .snapshot()
        .into_iter()
        .map(|record| record.keep_requested)
        .collect();
    assert_eq!(asked, [vec!["brandon".to_string()], vec!["brandon".to_string()]]);

    let after = rig_over(
        Arc::new(saved),
        Arc::new(AcquisitionTracker::new(None, Clock::system())),
    );
    let (taken, stop) = work(&after, lands);
    Arc::new(SoulseekHoldResumer::new(
        after.holds.clone(),
        after.coordinator.clone(),
    ))
    .run(CancellationToken::new())
    .await
    .expect("ran");

    until(|| after.holds.snapshot().is_empty()).await;
    let mut kept: Vec<(String, Option<DownloadSource>, bool)> = taken
        .lock()
        .iter()
        .map(|request| {
            (
                request.external_id.clone(),
                request.source_override,
                request.place().keep_anyway,
            )
        })
        .collect();
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        kept,
        [
            ("help".to_string(), Some(DownloadSource::YouTube), true),
            ("night".to_string(), Some(DownloadSource::Soulseek), true),
        ]
    );
    assert!(
        SoulseekHoldStore::new(Some(&file)).snapshot().is_empty(),
        "saved as ended"
    );
    stop.cancel();
}

/// The heard video does not answer: the keep waits in a hold for the shim with its line, the
/// request saved there, offering no second keep; once the shim's round finds it answering, the
/// keep is carried out and the hold ends.
#[tokio::test]
async fn a_busy_shim_holds_the_keep_with_its_line_until_its_round_keeps_it() {
    let rig = memory_rig();
    let record = rig
        .holds
        .hold(song_hold("help", WaitingFor::AcoustIdOff, &["brandon"]));
    rig.coordinator.hold_row_for(&record);
    let busy = Arc::new(AtomicBool::new(true));
    let shim = busy.clone();
    let (taken, stop) = work(&rig, move |request| {
        if shim.load(Ordering::SeqCst) {
            return Err(AcquisitionHeld {
                waiting_for: WaitingFor::Shim,
                line: KEEP_WAITS_FOR_YOUTUBE.into(),
                files: Vec::new(),
                lidarr: Vec::new(),
                users: vec!["brandon".into()],
            }
            .into());
        }
        lands(request)
    });

    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help", "brandon"),
        KeepAnyway::Keeping
    );

    let waiting = "Track:soulseek:help:Shim";
    until(|| rig.holds.get(waiting).is_some() && rig.holds.get(&record.key()).is_none()).await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    let held = rig.holds.get(waiting).expect("waiting for the shim");
    assert_eq!(users(&held), ["brandon"]);
    assert_eq!(held.keep_requested, ["brandon"]);
    assert_eq!(held.line.as_deref(), Some(KEEP_WAITS_FOR_YOUTUBE));
    assert_eq!(
        held.holders[0].heard, record.holders[0].heard,
        "what they heard waits with it"
    );
    assert_eq!(
        rig.tracker.note_of("soulseek", "help").as_deref(),
        Some(KEEP_WAITS_FOR_YOUTUBE)
    );
    assert!(!rig.coordinator.keep_marks("brandon").keeps("soulseek:help"));
    assert_eq!(taken.lock().len(), 1);

    // The shim's round finds the video answering and resumes the hold.
    busy.store(false, Ordering::SeqCst);
    rig.coordinator.resume_held(waiting);
    until(|| rig.holds.snapshot().is_empty()).await;
    let taken = taken.lock().clone();
    assert_eq!(taken.len(), 2, "asked once more, once the shim answered");
    assert!(taken[1].place().keep_anyway);
    stop.cancel();
}

/// The heard video is gone: that person's heart ends with the plain line. Alone in the hold, the
/// row fails with it; beside another person, the row stays held for them with the line in its log.
#[tokio::test]
async fn a_gone_heard_video_ends_that_holders_heart_with_the_plain_line() {
    let gone = |_: &AcquisitionRequest| -> anyhow::Result<String> {
        Err(anyhow::anyhow!(
            "Only the audio heard or its master can be kept for 'The Beatles - Help!'; YouTube's copies are not tried"
        ))
    };

    let rig = memory_rig();
    let record = rig
        .holds
        .hold(song_hold("help", WaitingFor::AcoustIdOff, &["brandon"]));
    rig.coordinator.hold_row_for(&record);
    let (_taken, stop) = work(&rig, gone);
    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help", "brandon"),
        KeepAnyway::Keeping
    );
    until(|| rig.holds.snapshot().is_empty()).await;
    until(|| {
        rig.tracker
            .detail("soulseek:help", None)
            .is_some_and(|row| row.state == AcquisitionState::Failed)
    })
    .await;
    let row = rig.tracker.detail("soulseek:help", None).expect("the row");
    assert_eq!(row.error.as_deref(), Some(HEARD_GONE));
    assert!(!rig.tracker.is_held("soulseek", "help"));
    stop.cancel();

    let rig = memory_rig();
    let record = rig
        .holds
        .hold(song_hold("help", WaitingFor::AcoustIdOff, &["brandon", "anna"]));
    rig.coordinator.hold_row_for(&record);
    let (_taken, stop) = work(&rig, gone);
    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help", "brandon"),
        KeepAnyway::Keeping
    );
    until(|| notes(&rig.tracker, "soulseek:help").contains(&HEARD_GONE.to_string())).await;
    let left = rig.holds.get(&record.key()).expect("anna still waits");
    assert_eq!(users(&left), ["anna"]);
    assert!(rig.tracker.is_held("soulseek", "help"));
    stop.cancel();
}

/// A clock the test moves by hand.
#[derive(Clone)]
struct ManualClock(Arc<Mutex<DateTime<Utc>>>);

impl ManualClock {
    fn new() -> ManualClock {
        ManualClock(Arc::new(Mutex::new(
            Utc.with_ymd_and_hms(2026, 10, 30, 9, 0, 0).unwrap(),
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

/// With the clock 25 hours on, the held row is still listed with its line, offers Keep anyway to
/// its holder, and the keep works.
#[tokio::test]
async fn with_the_test_clock_at_25_hours_the_held_row_is_listed_and_keep_anyway_works() {
    let clock = ManualClock::new();
    let rig = rig_over(
        Arc::new(SoulseekHoldStore::new(None::<PathBuf>)),
        Arc::new(AcquisitionTracker::new(None, clock.clock())),
    );
    let record = rig
        .holds
        .hold(song_hold("help", WaitingFor::AcoustIdOff, &["brandon"]));
    rig.coordinator.hold_row_for(&record);

    clock.advance(TimeDelta::hours(25));
    let rows = rig.tracker.for_user("brandon");
    assert_eq!(rows.len(), 1, "{rows:?}");
    let mut row = rows[0].clone();
    rig.coordinator.keep_marks("brandon").mark(&mut row);
    assert_eq!(row.state, AcquisitionState::Searching);
    assert_eq!(row.note.as_deref(), Some(waiting_line(WaitingFor::AcoustIdOff)));
    assert!(row.keep_anyway);

    let (taken, stop) = work(&rig, lands);
    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help", "brandon"),
        KeepAnyway::Keeping
    );
    until(|| rig.holds.snapshot().is_empty()).await;
    assert_eq!(taken.lock().len(), 1);
    stop.cancel();
}

/// A held track whose keep cannot land stays held: the request is cleared, so its row, back on the
/// line it waits with, offers the keep again, and asking again tries again.
#[tokio::test]
async fn a_keep_that_cannot_land_leaves_the_track_held_and_offers_the_keep_again() {
    let rig = memory_rig();
    let dir = tempfile::tempdir().expect("a temp dir");
    let night = rig
        .holds
        .hold(album_track("night", &held_file(&dir, "night"), &["brandon"]));
    rig.coordinator.hold_row_for(&night);
    let (taken, stop) = work(&rig, |_| Err(anyhow::anyhow!("the library could not take it")));

    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help-album", "brandon"),
        KeepAnyway::Keeping
    );
    until(|| !taken.lock().is_empty()).await;
    until(|| {
        rig.holds
            .get(&night.key())
            .is_some_and(|record| record.keep_requested.is_empty())
    })
    .await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;

    assert!(rig.coordinator.keep_marks("brandon").keeps("soulseek:help-album"));
    assert!(rig.tracker.is_held("soulseek", "night"));
    assert_eq!(
        rig.tracker.note_of("soulseek", "night").as_deref(),
        Some(tracks_wait_line(1).as_str())
    );
    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help-album", "brandon"),
        KeepAnyway::Keeping,
        "asked again, it tries again"
    );
    stop.cancel();
}

/// A keep's copies go in a folder named by a hash of the person, ignoring case: two people whose
/// names differ only by a character a folder name cannot hold never share one, and no name shows.
#[test]
fn a_keeps_copies_go_in_a_folder_named_by_a_hash_of_the_person() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let held = held_file(&dir, "night");
    let folder_of = |user: &str| {
        let copies = copies_for(std::slice::from_ref(&held), user).expect("copied");
        Path::new(&copies[0])
            .parent()
            .and_then(Path::file_name)
            .expect("a folder")
            .to_string_lossy()
            .into_owned()
    };
    let (dotted, underscored) = (folder_of("a.b"), folder_of("a_b"));
    assert_ne!(dotted, underscored);
    assert!(dotted.starts_with(KEPT_FOR_PREFIX), "{dotted}");
    assert!(!dotted.contains("a.b") && !underscored.contains("a_b"));
    assert_eq!(folder_of(" A.B "), dotted, "the same person, whatever the case");
}

/// MusicBrainz gave up on the question a held heart waited on: the heart runs again, and once it
/// ends with nothing (nothing landed, nothing of the song held), its row says MusicBrainz could not
/// answer for this song.
#[tokio::test]
async fn a_heart_whose_question_musicbrainz_gave_up_on_ends_with_that_line() {
    use super::super::MUSIC_BRAINZ_GAVE_UP;

    let rig = memory_rig();
    let held = rig
        .holds
        .hold(song_hold("yesterday", WaitingFor::MusicBrainz, &["brandon"]));
    rig.coordinator.hold_row_for(&held);
    let (_taken, stop) = work(&rig, |_| Err(anyhow::anyhow!("no source has it")));

    rig.coordinator.resume_after_gave_up(&held.key());

    until(|| {
        rig.tracker
            .detail("soulseek:yesterday", None)
            .and_then(|row| row.error)
            .as_deref()
            == Some(MUSIC_BRAINZ_GAVE_UP)
    })
    .await;
    assert!(rig.holds.records_for("Track:soulseek:yesterday").is_empty());
    stop.cancel();
}

/// MusicBrainz gives up while a run of the held heart goes on already: the line is decided by the
/// last pass of that run (which runs once more for the wake), never before it ends.
#[tokio::test]
async fn a_give_up_during_a_run_says_its_line_when_that_run_ends_with_nothing() {
    use super::super::MUSIC_BRAINZ_GAVE_UP;

    let rig = memory_rig();
    let held = rig
        .holds
        .hold(song_hold("yesterday", WaitingFor::MusicBrainz, &["brandon"]));
    rig.coordinator.hold_row_for(&held);
    let (_taken, stop) = work(&rig, |_| Err(anyhow::anyhow!("no source has it")));
    // A run of it goes on.
    rig.coordinator.resuming.lock().insert(held.key());

    rig.coordinator.resume_after_gave_up(&held.key());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        rig.tracker
            .detail("soulseek:yesterday", None)
            .and_then(|row| row.error)
            .is_none(),
        "nothing is said while the run goes on"
    );
    // The run going on ends; the wake runs it once more.
    rig.coordinator.run_claimed(&held.key()).await;

    until(|| {
        rig.tracker
            .detail("soulseek:yesterday", None)
            .and_then(|row| row.error)
            .as_deref()
            == Some(MUSIC_BRAINZ_GAVE_UP)
    })
    .await;
    stop.cancel();
}

/// "Keep these anyway" on a held track two people hold keeps it for the one who asked, alone:
/// their own gate and their own request, from a copy of the held file, and only they leave the
/// hold. The other still waits in it with the held file; when they keep it too, the held file
/// itself lands for them and the hold ends.
#[tokio::test]
async fn keep_these_anyway_keeps_a_shared_held_track_for_the_caller_alone() {
    let rig = memory_rig();
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = held_file(&dir, "night");
    let night = rig.holds.hold(album_track("night", &file, &["brandon", "anna"]));
    rig.coordinator.hold_row_for(&night);
    let (taken, stop) = work(&rig, lands);

    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help-album", "anna"),
        KeepAnyway::Keeping
    );
    until(|| {
        rig.holds
            .get(&night.key())
            .is_some_and(|record| users(&record) == ["brandon"])
    })
    .await;

    let first = taken.lock()[0].clone();
    assert_eq!(first.requested_by(), ["anna"]);
    let gates = first.gates();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0].user, "anna");
    let handed = first.place().held_files;
    assert_eq!(handed.len(), 1);
    assert_ne!(handed[0], file, "a copy of the held file");
    assert!(Path::new(&file).is_file(), "brandon's held file stays");

    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help-album", "brandon"),
        KeepAnyway::Keeping
    );
    until(|| rig.holds.get(&night.key()).is_none()).await;
    let second = taken.lock()[1].clone();
    assert_eq!(second.requested_by(), ["brandon"]);
    assert_eq!(second.gates()[0].user, "brandon");
    assert_eq!(second.place().held_files, [file]);
    stop.cancel();
}
