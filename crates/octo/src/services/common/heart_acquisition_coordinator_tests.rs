//! The heart acquisition coordinator. The settings-only tests (the legacy fallback's order, a
//! configured order with missing sources appended disabled) live with the settings, in
//! `octo_core::settings::subsonic`.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::Clock;
use octo_core::settings::{
    AppSettings, DownloadSource, HeartDownloadSource, HeartDownloadStep, SubsonicSettings,
};

use super::*;
use crate::services::common::AcquisitionRequest;
use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr, until};

fn store(subsonic: SubsonicSettings) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic,
        ..Default::default()
    }))
}

fn legacy(source: DownloadSource) -> SubsonicSettings {
    SubsonicSettings {
        download_source: source,
        ..Default::default()
    }
}

fn steps(list: &[(HeartDownloadSource, bool)]) -> SubsonicSettings {
    SubsonicSettings {
        heart_download_sources: list
            .iter()
            .map(|(source, enabled)| HeartDownloadStep {
                source: *source,
                enabled: Some(*enabled),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn step(source: HeartDownloadSource, song: bool, album: bool) -> HeartDownloadStep {
    HeartDownloadStep {
        source,
        song_enabled: Some(song),
        album_enabled: Some(album),
        ..Default::default()
    }
}

fn coordinator(
    settings: Arc<SettingsStore>,
    queue: Arc<TrackAcquisitionQueue>,
    direct: Arc<FakeDownloads>,
    lidarr: Arc<FakeLidarr>,
) -> Arc<HeartAcquisitionCoordinator> {
    HeartAcquisitionCoordinator::new(settings, queue, direct, lidarr, HeartCoordinatorExtras::default())
}

fn new_queue() -> Arc<TrackAcquisitionQueue> {
    Arc::new(TrackAcquisitionQueue::new())
}

/// The next queued request, or `None` within 300 ms.
async fn next_queued(queue: &TrackAcquisitionQueue) -> Option<Arc<AcquisitionRequest>> {
    tokio::time::timeout(
        Duration::from_millis(300),
        queue.dequeue(&CancellationToken::new()),
    )
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn lidarr_source_routes_track_and_album_without_calling_direct_downloader() {
    let lidarr = Arc::new(
        FakeLidarr::default()
            .tracks(|p, id, notify| Ok(p == "soulseek" && id == "track-id" && notify))
            .albums(|p, id, notify| Ok(p == "soulseek" && id == "album-id" && notify)),
    );
    let direct = Arc::new(FakeDownloads::default());
    let coordinator = coordinator(
        store(legacy(DownloadSource::Lidarr)),
        new_queue(),
        direct.clone(),
        lidarr.clone(),
    );

    coordinator
        .acquire_track("soulseek", "track-id", None, None)
        .await
        .expect("ran");
    coordinator
        .acquire_album("soulseek", "album-id", None, None)
        .await
        .expect("ran");

    assert_eq!(lidarr.track_calls_for("soulseek", "track-id", true), 1);
    assert_eq!(lidarr.album_calls_for("soulseek", "album-id", true), 1);
    assert_eq!(*direct.remaining_calls.lock(), 0);
}

#[tokio::test]
async fn soulseek_source_keeps_album_on_existing_direct_path() {
    let lidarr = Arc::new(FakeLidarr::default());
    let direct = Arc::new(FakeDownloads::albums(|source, suppress| {
        Ok(source == DownloadSource::Soulseek && !suppress)
    }));
    let coordinator = coordinator(
        store(legacy(DownloadSource::Soulseek)),
        new_queue(),
        direct.clone(),
        lidarr.clone(),
    );

    coordinator
        .acquire_album("soulseek", "album-id", None, None)
        .await
        .expect("ran");

    let calls = direct.album_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str(), calls[0].2, calls[0].3),
        ("soulseek", "album-id", DownloadSource::Soulseek, false)
    );
    assert_eq!(lidarr.calls(), 0);
}

#[tokio::test]
async fn download_source_change_takes_effect_without_rebuilding_coordinator() {
    let lidarr = Arc::new(FakeLidarr::default().albums(|_, id, notify| Ok(id == "second-album" && notify)));
    let direct = Arc::new(FakeDownloads::albums(|source, suppress| {
        Ok(source == DownloadSource::Soulseek && !suppress)
    }));
    let settings = store(legacy(DownloadSource::Soulseek));
    let coordinator = coordinator(settings.clone(), new_queue(), direct.clone(), lidarr.clone());

    coordinator
        .acquire_album("soulseek", "first-album", None, None)
        .await
        .expect("ran");
    settings.set(AppSettings {
        subsonic: legacy(DownloadSource::Lidarr),
        ..Default::default()
    });
    coordinator
        .acquire_album("soulseek", "second-album", None, None)
        .await
        .expect("ran");

    let calls = direct.album_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "first-album");
    assert_eq!(lidarr.album_calls_for("soulseek", "second-album", true), 1);
}

#[tokio::test]
async fn album_priority_stops_at_first_successful_source() {
    let lidarr = Arc::new(FakeLidarr::default());
    let direct = Arc::new(FakeDownloads::albums(|source, suppress| {
        Ok(source == DownloadSource::Soulseek && suppress)
    }));
    let settings = store(steps(&[
        (HeartDownloadSource::YouTube, false),
        (HeartDownloadSource::Soulseek, true),
        (HeartDownloadSource::Lidarr, true),
    ]));
    let coordinator = coordinator(settings, new_queue(), direct, lidarr.clone());

    coordinator
        .acquire_album("soulseek", "album-id", None, None)
        .await
        .expect("ran");

    assert_eq!(lidarr.calls(), 0);
}

#[tokio::test]
async fn album_priority_falls_through_to_lidarr_after_direct_failure() {
    let lidarr = Arc::new(
        FakeLidarr::default().albums(|p, id, notify| Ok(p == "soulseek" && id == "album-id" && notify)),
    );
    let direct = Arc::new(FakeDownloads::albums(|_, _| Ok(false)));
    let settings = store(steps(&[
        (HeartDownloadSource::Soulseek, true),
        (HeartDownloadSource::YouTube, false),
        (HeartDownloadSource::Lidarr, true),
    ]));
    let coordinator = coordinator(settings, new_queue(), direct.clone(), lidarr.clone());

    coordinator
        .acquire_album("soulseek", "album-id", None, None)
        .await
        .expect("ran");

    assert_eq!(lidarr.album_calls_for("soulseek", "album-id", true), 1);
    assert!(!direct.album_sources().contains(&DownloadSource::YouTube));
}

#[tokio::test]
async fn album_priority_tries_direct_sources_in_order_and_stops_on_success() {
    let direct = Arc::new(FakeDownloads::albums(|source, _| {
        Ok(source == DownloadSource::YouTube)
    }));
    let lidarr = Arc::new(FakeLidarr::default());
    let settings = store(steps(&[
        (HeartDownloadSource::Soulseek, true),
        (HeartDownloadSource::YouTube, true),
        (HeartDownloadSource::Lidarr, true),
    ]));
    let coordinator = coordinator(settings, new_queue(), direct.clone(), lidarr.clone());

    coordinator
        .acquire_album("soulseek", "album-id", None, None)
        .await
        .expect("ran");

    assert_eq!(
        direct.album_sources(),
        [DownloadSource::Soulseek, DownloadSource::YouTube]
    );
    assert_eq!(lidarr.calls(), 0);
}

#[tokio::test]
async fn track_priority_falls_through_from_soulseek_to_lidarr() {
    let lidarr = Arc::new(
        FakeLidarr::default().tracks(|p, id, notify| Ok(p == "soulseek" && id == "track-id" && notify)),
    );
    let queue = new_queue();
    let settings = store(steps(&[
        (HeartDownloadSource::Soulseek, true),
        (HeartDownloadSource::YouTube, false),
        (HeartDownloadSource::Lidarr, true),
    ]));
    let coordinator = coordinator(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
    );

    let running = coordinator.clone();
    let acquisition =
        tokio::spawn(async move { running.acquire_track("soulseek", "track-id", None, None).await });
    let request = queue.dequeue(&CancellationToken::new()).await.expect("a request");
    assert_eq!(request.source_override, Some(DownloadSource::Soulseek));
    assert!(!request.notify_on_failure);
    queue.release(&request);
    request.completion.try_set_error(anyhow::anyhow!("no peer"));
    acquisition.await.expect("joined").expect("ran");

    assert_eq!(lidarr.track_calls_for("soulseek", "track-id", true), 1);
}

#[tokio::test]
async fn song_and_album_hearts_use_their_own_per_source_switches() {
    let lidarr = Arc::new(
        FakeLidarr::default().albums(|p, id, notify| Ok(p == "soulseek" && id == "album-id" && notify)),
    );
    let queue = new_queue();
    let settings = store(SubsonicSettings {
        heart_download_sources: vec![
            step(HeartDownloadSource::Soulseek, true, false),
            step(HeartDownloadSource::Lidarr, false, true),
            step(HeartDownloadSource::YouTube, false, false),
        ],
        ..Default::default()
    });
    let coordinator = coordinator(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
    );

    let running = coordinator.clone();
    let track = tokio::spawn(async move { running.acquire_track("soulseek", "track-id", None, None).await });
    let request = queue.dequeue(&CancellationToken::new()).await.expect("a request");
    assert_eq!(request.source_override, Some(DownloadSource::Soulseek));
    request.completion.try_set_result("/music/track.flac");
    queue.release(&request);
    track.await.expect("joined").expect("ran");
    coordinator
        .acquire_album("soulseek", "album-id", None, None)
        .await
        .expect("ran");

    assert!(lidarr.track_calls.lock().is_empty());
    assert_eq!(lidarr.album_calls_for("soulseek", "album-id", true), 1);
}

fn play_settings(
    download_on_play: bool,
    lidarr_album_on_play: bool,
    list: Vec<HeartDownloadStep>,
) -> SubsonicSettings {
    SubsonicSettings {
        download_on_play,
        lidarr_album_on_play,
        heart_download_sources: if list.is_empty() {
            vec![
                step(HeartDownloadSource::Lidarr, true, true),
                step(HeartDownloadSource::YouTube, true, true),
                step(HeartDownloadSource::Soulseek, false, false),
            ]
        } else {
            list
        },
        ..Default::default()
    }
}

fn with_tracker(
    settings: SubsonicSettings,
    queue: Arc<TrackAcquisitionQueue>,
    lidarr: Arc<FakeLidarr>,
    tracker: Option<Arc<AcquisitionTracker>>,
) -> Arc<HeartAcquisitionCoordinator> {
    HeartAcquisitionCoordinator::new(
        store(settings),
        queue,
        Arc::new(FakeDownloads::default()),
        lidarr,
        HeartCoordinatorExtras {
            tracker,
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn play_starts_nothing_by_default() {
    let lidarr = Arc::new(FakeLidarr::default());
    let queue = new_queue();
    let coordinator = with_tracker(
        play_settings(false, false, Vec::new()),
        queue.clone(),
        lidarr.clone(),
        None,
    );

    coordinator.queue_play("soulseek", "track-id", None, None, None);

    assert!(next_queued(&queue).await.is_none());
    assert_eq!(lidarr.calls(), 0);
}

#[tokio::test]
async fn download_on_play_queues_the_first_direct_song_source_and_skips_lidarr() {
    let lidarr = Arc::new(FakeLidarr::default());
    let queue = new_queue();
    let coordinator = with_tracker(
        play_settings(true, false, Vec::new()),
        queue.clone(),
        lidarr.clone(),
        None,
    );

    coordinator.queue_play("soulseek", "track-id", Some("felix"), None, None);

    let request = next_queued(&queue).await.expect("queued");
    assert_eq!(request.external_id, "track-id");
    assert!(!request.is_star);
    assert!(request.force_permanent);
    assert_eq!(request.source_override, Some(DownloadSource::YouTube));
    assert_eq!(request.requested_by(), ["felix"]);
    assert_eq!(lidarr.calls(), 0);
}

#[tokio::test]
async fn lidarr_album_on_play_hands_each_track_to_lidarr_once() {
    let lidarr = Arc::new(
        FakeLidarr::default().tracks(|p, id, notify| Ok(p == "soulseek" && id == "track-id" && !notify)),
    );
    let queue = new_queue();
    let coordinator = with_tracker(
        play_settings(false, true, Vec::new()),
        queue.clone(),
        lidarr.clone(),
        None,
    );

    coordinator.queue_play("soulseek", "track-id", None, None, None);
    coordinator.queue_play("soulseek", "track-id", None, None, None);

    assert!(next_queued(&queue).await.is_none());
    assert_eq!(lidarr.track_calls_for("soulseek", "track-id", false), 1);
}

#[tokio::test]
async fn download_on_play_leaves_the_track_to_wait_for_lossless_on_play() {
    let mut settings = play_settings(true, false, Vec::new());
    settings.wait_for_lossless_on_play = true;
    let queue = new_queue();
    with_tracker(settings, queue.clone(), Arc::new(FakeLidarr::default()), None)
        .queue_play("soulseek", "track-id", None, None, None);
    assert_eq!(queue.waiting_plays(), 0);
}

#[tokio::test]
async fn soulseek_first_plays_fall_back_to_you_tube_only_when_it_is_on() {
    for (you_tube, expected) in [
        (false, DownloadSource::Soulseek),
        (true, DownloadSource::SoulseekThenYouTube),
    ] {
        let queue = new_queue();
        with_tracker(
            play_settings(
                true,
                false,
                vec![
                    step(HeartDownloadSource::Soulseek, true, false),
                    step(HeartDownloadSource::YouTube, you_tube, false),
                ],
            ),
            queue.clone(),
            Arc::new(FakeLidarr::default()),
            None,
        )
        .queue_play("soulseek", "track-id", None, None, None);
        assert_eq!(
            next_queued(&queue).await.expect("queued").source_override,
            Some(expected),
            "YouTube {you_tube}"
        );
    }
}

/// The hand-off is a task, so each play waits for it to settle before the next.
#[tokio::test]
async fn lidarr_album_on_play_tries_again_after_a_hand_off_that_did_not_take() {
    for throws in [false, true] {
        let mut answers = if throws {
            vec![Err(anyhow::anyhow!("down")), Ok(true)]
        } else {
            vec![Ok(false), Ok(true)]
        }
        .into_iter();
        let lidarr =
            Arc::new(FakeLidarr::default().tracks(move |_, _, _| answers.next().unwrap_or(Ok(false))));
        let coordinator = with_tracker(
            play_settings(false, true, Vec::new()),
            new_queue(),
            lidarr.clone(),
            None,
        );

        for i in 0..3 {
            coordinator.queue_play("soulseek", "track-id", None, None, None);
            let settled = coordinator.clone();
            let lidarr = lidarr.clone();
            until(move || {
                let calls = lidarr.track_calls_for("soulseek", "track-id", false);
                // The first hand-off did not take and lets go of the track; the second took.
                match i {
                    0 => calls == 1 && settled.lidarr_plays.lock().is_empty(),
                    _ => calls == 2,
                }
            })
            .await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            lidarr.track_calls_for("soulseek", "track-id", false),
            2,
            "throws {throws}"
        );
    }
}

#[tokio::test]
async fn only_one_played_song_waits_and_a_skipped_one_can_be_asked_for_again() {
    let queue = new_queue();
    let coordinator = with_tracker(
        play_settings(true, false, Vec::new()),
        queue.clone(),
        Arc::new(FakeLidarr::default()),
        None,
    );
    coordinator.queue_play("soulseek", "first", None, None, None);
    coordinator.queue_play("soulseek", "second", None, None, None);
    assert_eq!(queue.waiting_plays(), 1);
    assert_eq!(next_queued(&queue).await.expect("queued").external_id, "first");

    coordinator.queue_play("soulseek", "second", None, None, None);
    assert_eq!(next_queued(&queue).await.expect("queued").external_id, "second");
}

#[tokio::test]
async fn a_heart_goes_before_a_waiting_play_and_upgrades_one_it_joins() {
    let queue = new_queue();
    with_tracker(
        play_settings(true, false, Vec::new()),
        queue.clone(),
        Arc::new(FakeLidarr::default()),
        None,
    )
    .queue_play("soulseek", "played", None, None, None);
    let _ = queue.enqueue(
        "soulseek", "hearted", true, false, true, None, true, None, false, None,
    );
    assert_eq!(next_queued(&queue).await.expect("queued").external_id, "hearted");

    let _ = queue.enqueue(
        "soulseek",
        "played",
        true,
        false,
        true,
        None,
        true,
        Some("felix"),
        false,
        None,
    );
    let joined = next_queued(&queue).await.expect("queued");
    assert!(joined.heart_joined());
    assert!(joined.notifies_on_failure());
    assert!(joined.requested_by().contains(&"felix".to_string()));
}

#[tokio::test]
async fn a_played_download_has_a_row_that_fails_with_it() {
    let queue = new_queue();
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let coordinator = with_tracker(
        play_settings(true, false, Vec::new()),
        queue.clone(),
        Arc::new(FakeLidarr::default()),
        Some(tracker.clone()),
    );
    coordinator.queue_play("soulseek", "track-id", None, Some("ext-1"), Some("felix"));
    coordinator.queue_play("soulseek", "skipped", None, Some("ext-2"), Some("felix"));
    let rows = tracker.for_user("felix");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, AcquisitionState::Queued);

    let request = next_queued(&queue).await.expect("queued");
    queue.release(&request);
    request.completion.try_set_error(anyhow::anyhow!("no peers"));
    let watched = tracker.clone();
    until(move || watched.for_user("felix")[0].state == AcquisitionState::Failed).await;
}

#[tokio::test]
async fn a_dropped_play_never_strands_a_heart() {
    // The heart for B arrives at the worst moment: while the play for B is being dropped.
    let queue = new_queue();
    let heart: Arc<parking_lot::Mutex<Option<crate::services::common::Completion>>> = Arc::default();
    let (slot, hearted) = (heart.clone(), Arc::downgrade(&queue));
    queue.set_on_skipped_play(Arc::new(move || {
        if let Some(queue) = hearted.upgrade() {
            *slot.lock() =
                Some(queue.enqueue("soulseek", "B", true, false, true, None, true, None, false, None));
        }
    }));
    assert!(
        queue
            .try_enqueue_play("soulseek", "A", DownloadSource::YouTube, None, None)
            .is_some()
    );

    assert!(
        queue
            .try_enqueue_play("soulseek", "B", DownloadSource::YouTube, None, None)
            .is_none()
    );
    let heart = heart.lock().clone().expect("the heart was queued");

    // The heart has its own request, queued and taken first, not a released play's.
    let next = next_queued(&queue).await.expect("queued");
    if next.external_id == "B" {
        queue.release(&next);
        next.completion.try_set_result("/music/b.flac");
    }
    let path = tokio::time::timeout(Duration::from_secs(2), heart.wait())
        .await
        .expect("in time")
        .expect("downloaded");
    assert_eq!(path, "/music/b.flac");
    assert!(next.is_star);
}

fn soulseek_off(
    settings: SubsonicSettings,
    queue: Arc<TrackAcquisitionQueue>,
    direct: Arc<FakeDownloads>,
) -> Arc<HeartAcquisitionCoordinator> {
    HeartAcquisitionCoordinator::new(
        store(settings),
        queue,
        direct,
        Arc::new(FakeLidarr::default()),
        HeartCoordinatorExtras {
            soulseek: Some(Arc::new(crate::services::common::test_fakes::OffLink)),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn with_soulseek_off_an_album_heart_goes_straight_to_the_next_source() {
    let direct = Arc::new(FakeDownloads::albums(|source, _| {
        Ok(source == DownloadSource::YouTube)
    }));
    let settings = steps(&[
        (HeartDownloadSource::Soulseek, true),
        (HeartDownloadSource::YouTube, true),
    ]);

    soulseek_off(settings, new_queue(), direct.clone())
        .acquire_album("soulseek", "album-id", None, None)
        .await
        .expect("ran");

    assert_eq!(direct.album_sources(), [DownloadSource::YouTube]);
}

#[tokio::test]
async fn with_soulseek_off_a_soulseek_first_play_is_a_you_tube_play() {
    let queue = new_queue();
    soulseek_off(
        play_settings(
            true,
            false,
            vec![
                step(HeartDownloadSource::Soulseek, true, false),
                step(HeartDownloadSource::YouTube, true, false),
            ],
        ),
        queue.clone(),
        Arc::new(FakeDownloads::default()),
    )
    .queue_play("soulseek", "track-id", None, None, None);

    assert_eq!(
        next_queued(&queue).await.expect("queued").source_override,
        Some(DownloadSource::YouTube)
    );
}

// ---- A hearted song whose album follows it (DownloadAlbumOnSongStar) ----------------------

/// A coordinator whose album walks go to `direct`, with a tracker and a short wait for the
/// hearted song.
fn following(
    settings: SubsonicSettings,
    queue: Arc<TrackAcquisitionQueue>,
    direct: Arc<FakeDownloads>,
    lidarr: Arc<FakeLidarr>,
    tracker: Arc<AcquisitionTracker>,
    max_wait: Duration,
) -> Arc<HeartAcquisitionCoordinator> {
    let coordinator = HeartAcquisitionCoordinator::new(
        store(settings),
        queue,
        direct,
        lidarr,
        HeartCoordinatorExtras {
            tracker: Some(tracker),
            ..Default::default()
        },
    );
    coordinator.set_landed_song_wait(LandedSongWait {
        poll: Duration::from_millis(10),
        max_wait,
    });
    coordinator
}

fn song_and_album_from_youtube() -> SubsonicSettings {
    SubsonicSettings {
        heart_download_sources: vec![step(HeartDownloadSource::YouTube, true, true)],
        ..Default::default()
    }
}

fn hearted(id: &str) -> Vec<(String, String)> {
    vec![("soulseek".to_string(), id.to_string())]
}

fn begin_row(tracker: &AcquisitionTracker, id: &str) {
    tracker.begin("soulseek", id, Some(id), Some("felix"), None, None, None);
}

#[tokio::test]
async fn a_hearted_songs_album_waits_for_the_song_to_land_and_show() {
    let queue = new_queue();
    let direct = Arc::new(FakeDownloads::default());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let coordinator = following(
        song_and_album_from_youtube(),
        queue.clone(),
        direct.clone(),
        Arc::new(FakeLidarr::default()),
        tracker.clone(),
        Duration::from_secs(30),
    );
    begin_row(&tracker, "song-id");

    coordinator.queue_tracks_then_album_for(
        &hearted("song-id"),
        "soulseek",
        "album-id",
        Some("felix"),
        Some("felix"),
        AcquisitionTarget::Shared,
    );

    // The song goes down its own chain first, and the album waits for it.
    let request = next_queued(&queue).await.expect("the song is queued");
    assert_eq!(request.external_id, "song-id");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        direct.album_calls.lock().is_empty(),
        "the album waits for the song"
    );

    // Landed, but not yet shown in the library: the album still waits.
    queue.release(&request);
    request.completion.try_set_result("/music/song.flac".to_string());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        direct.album_calls.lock().is_empty(),
        "the album waits for the library"
    );

    tracker.complete("soulseek", "song-id", Some("nd-1"));
    until(|| !direct.album_calls.lock().is_empty()).await;
    let calls = direct.album_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str(), calls[0].2),
        ("soulseek", "album-id", DownloadSource::YouTube)
    );
    assert_eq!(calls[0].4, Some(vec!["felix".to_string()]));
    assert!(next_queued(&queue).await.is_none(), "the song is asked for once");
}

#[tokio::test]
async fn a_hearted_songs_album_waits_no_longer_than_the_limit() {
    let queue = new_queue();
    let direct = Arc::new(FakeDownloads::default());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let coordinator = following(
        song_and_album_from_youtube(),
        queue.clone(),
        direct.clone(),
        Arc::new(FakeLidarr::default()),
        tracker.clone(),
        Duration::from_millis(200),
    );
    begin_row(&tracker, "song-id");

    coordinator.queue_tracks_then_album_for(
        &hearted("song-id"),
        "soulseek",
        "album-id",
        None,
        None,
        AcquisitionTarget::Shared,
    );
    let request = next_queued(&queue).await.expect("the song is queued");
    queue.release(&request);
    request.completion.try_set_result("/music/song.flac".to_string());

    // The song never shows in the library, and the album goes on once the wait is up.
    until(|| !direct.album_calls.lock().is_empty()).await;
    assert!(tracker.is_running("soulseek:song-id"));
}

#[tokio::test]
async fn a_hearted_song_that_failed_still_brings_its_album() {
    let queue = new_queue();
    let direct = Arc::new(FakeDownloads::default());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let coordinator = following(
        song_and_album_from_youtube(),
        queue.clone(),
        direct.clone(),
        Arc::new(FakeLidarr::default()),
        tracker.clone(),
        Duration::from_secs(30),
    );
    begin_row(&tracker, "song-id");

    coordinator.queue_tracks_then_album_for(
        &hearted("song-id"),
        "soulseek",
        "album-id",
        None,
        None,
        AcquisitionTarget::Shared,
    );
    let request = next_queued(&queue).await.expect("the song is queued");
    queue.release(&request);
    request.completion.try_set_error(anyhow::anyhow!("no peers"));

    // Nothing landed, so nothing is waited for.
    until(|| !direct.album_calls.lock().is_empty()).await;
}

#[tokio::test]
async fn a_hearted_song_lidarr_took_on_brings_no_album_walk() {
    let queue = new_queue();
    let direct = Arc::new(FakeDownloads::default());
    let lidarr = Arc::new(FakeLidarr::default().tracks(|_, id, _| Ok(id == "song-id")));
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let coordinator = following(
        SubsonicSettings {
            heart_download_sources: vec![
                step(HeartDownloadSource::Lidarr, true, false),
                step(HeartDownloadSource::YouTube, false, true),
            ],
            ..Default::default()
        },
        queue.clone(),
        direct.clone(),
        lidarr.clone(),
        tracker,
        Duration::from_secs(30),
    );

    coordinator
        .acquire_tracks_then_album(
            &hearted("song-id"),
            "soulseek",
            "album-id",
            None,
            None,
            &AcquisitionTarget::Shared,
        )
        .await;

    // Lidarr brings the whole album, so no walk goes after it as well.
    assert_eq!(lidarr.track_calls_for("soulseek", "song-id", true), 1);
    assert_eq!(lidarr.calls(), 1, "Lidarr was asked for the song only");
    assert!(direct.album_calls.lock().is_empty());
    assert!(next_queued(&queue).await.is_none());
}

/// Songs downloaded ahead: every song asked about is ready.
struct AllReady;

impl crate::services::prefetch::PrefetchedFiles for AllReady {
    fn has(&self, _: &str, _: &str) -> bool {
        true
    }

    fn take(&self, _: &str, _: &str, _: &std::path::Path) -> Option<crate::services::prefetch::Taken> {
        None
    }

    fn kept(
        &self,
        _: &str,
        _: &str,
        _: &std::path::Path,
        _: Option<Box<octo_core::fingerprint::VerificationResult>>,
    ) {
    }
}

#[tokio::test]
async fn a_heart_on_a_song_downloaded_ahead_is_kept_at_once_without_the_source_chain() {
    let lidarr = Arc::new(FakeLidarr::default().tracks(|_, _, _| Ok(true)));
    let queue = new_queue();
    // Lidarr first, and Soulseek out of its login: neither may hold this heart up.
    let settings = store(steps(&[
        (HeartDownloadSource::Lidarr, true),
        (HeartDownloadSource::Soulseek, true),
    ]));
    let coordinator = HeartAcquisitionCoordinator::new(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            soulseek: Some(Arc::new(crate::services::common::test_fakes::OfflineLink)),
            ahead: Some(Arc::new(AllReady)),
            ..Default::default()
        },
    );
    let running = coordinator.clone();
    let acquisition = tokio::spawn(async move {
        running
            .acquire_track_outcome(
                "soulseek",
                "track-id",
                None,
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
    });
    let request = tokio::time::timeout(Duration::from_secs(5), queue.dequeue(&CancellationToken::new()))
        .await
        .expect("queued at once, with no wait for Soulseek")
        .expect("a request");
    assert_eq!(request.source_override, Some(DownloadSource::Soulseek));
    queue.release(&request);
    request.completion.try_set_result("/music/kept.flac");
    let outcome = acquisition.await.expect("joined").expect("ran");
    assert_eq!(outcome, TrackOutcome::Landed("/music/kept.flac".into()));
    assert!(lidarr.track_calls.lock().is_empty(), "never handed to Lidarr");
}
