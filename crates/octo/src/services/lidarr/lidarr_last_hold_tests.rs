//! With Lidarr last in a heart's chain, a walk MusicBrainz could not answer holds the heart,
//! saved, until the re-asker gets the answer, and the heart then runs again; an import goes back on
//! its trickle instead. With a source after Lidarr the chain just goes on. Lidarr's failing
//! indexers hold the heart the same way, until they search again, the person hearts again, or Octo
//! restarts.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use octo_core::common::Clock;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, WaitingFor};
use octo_core::settings::{
    AppSettings, HeartDownloadSource, HeartDownloadStep, SettingsStore, SubsonicSettings,
};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr, until};
use crate::services::common::{
    AcquisitionRequest, AcquisitionTarget, AcquisitionTracker, HeartAcquisitionCoordinator,
    HeartCoordinatorExtras, SoulseekHoldResumer, SoulseekHoldStore, TrackAcquisitionQueue, TrackOutcome,
};
use crate::services::fingerprint::MusicBrainzClient;
use crate::services::fingerprint::music_brainz_client::HeartBusy;
use crate::services::imports::trickle_worker::TrickleCheckLater;
use crate::services::lidarr::LidarrWaits;

/// The question the walk got no answer to.
const QUESTION: &str = "recording/0d7b6ba8-6ff0-4a5c-a6f9-cc9c6b8c3c37?inc=releases&fmt=json";

fn sources(steps: &[HeartDownloadSource]) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            heart_download_sources: steps
                .iter()
                .map(|source| HeartDownloadStep {
                    source: *source,
                    song_enabled: Some(true),
                    album_enabled: Some(true),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
        ..Default::default()
    }))
}

struct Rig {
    coordinator: Arc<HeartAcquisitionCoordinator>,
    queue: Arc<TrackAcquisitionQueue>,
    holds: Arc<SoulseekHoldStore>,
    tracker: Arc<AcquisitionTracker>,
    lidarr: Arc<FakeLidarr>,
    _server: MockServer,
}

/// The coordinator over these sources, a Lidarr whose first walk got no answer from MusicBrainz
/// and whose next try lands, and a MusicBrainz that answers the question when asked again.
async fn rig(steps: &[HeartDownloadSource]) -> Rig {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/recording/0d7b6ba8-6ff0-4a5c-a6f9-cc9c6b8c3c37"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{\"releases\":[]}", "application/json"))
        .mount(&server)
        .await;
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    let quick = Duration::from_millis(5);
    let music_brainz =
        Arc::new(MusicBrainzClient::with_base_url(base).with_pace(quick, Duration::from_secs(5), quick));
    let calls = Arc::new(Mutex::new(0));
    let lidarr = Arc::new(FakeLidarr::default().tracks(move |_, _, _| {
        let mut calls = calls.lock();
        *calls += 1;
        Ok(*calls > 1)
    }));
    *lidarr.waits.lock() = Some(LidarrWaits::Answer(HeartBusy::MusicBrainz(QUESTION.into())));
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let holds = Arc::new(SoulseekHoldStore::new(None::<PathBuf>));
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let coordinator = HeartAcquisitionCoordinator::new(
        sources(steps),
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            holds: Some(holds.clone()),
            music_brainz: Some(music_brainz),
            ..Default::default()
        },
    );
    Rig {
        coordinator,
        queue,
        holds,
        tracker,
        lidarr,
        _server: server,
    }
}

type Landed = Arc<Mutex<Vec<Arc<AcquisitionRequest>>>>;

/// Lands every request the queue hands out until the token is canceled.
fn land_everything(queue: &Arc<TrackAcquisitionQueue>) -> (Landed, CancellationToken) {
    let landed: Landed = Arc::default();
    let stop = CancellationToken::new();
    let (queue, seen, token) = (queue.clone(), landed.clone(), stop.clone());
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            seen.lock().push(request.clone());
            queue.release(&request);
            request
                .completion
                .try_set_result(format!("/music/{}.flac", request.external_id));
        }
    });
    (landed, stop)
}

#[tokio::test]
async fn with_lidarr_last_a_heart_whose_walk_got_no_answer_holds_and_resumes_on_the_answer() {
    let rig = rig(&[HeartDownloadSource::Lidarr]).await;

    let outcome = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            "eye-of-the-tiger",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");

    assert_eq!(outcome, TrackOutcome::Held(WaitingFor::MusicBrainz));
    // The answer to the held question runs the heart again; Lidarr takes it this time, and the
    // hold and its row's hold end.
    until(|| rig.lidarr.track_calls.lock().len() == 2 && rig.holds.snapshot().is_empty()).await;
    until(|| !rig.tracker.is_held("soulseek", "eye-of-the-tiger")).await;
    let notes: Vec<String> = rig
        .tracker
        .detail("soulseek:eye-of-the-tiger", None)
        .and_then(|row| row.events)
        .unwrap_or_default()
        .into_iter()
        .map(|event| event.text)
        .collect();
    assert!(
        notes.iter().any(|note| note == "Waiting for MusicBrainz"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn with_a_source_after_lidarr_the_chain_goes_on() {
    let rig = rig(&[HeartDownloadSource::Lidarr, HeartDownloadSource::YouTube]).await;
    let (landed, stop) = land_everything(&rig.queue);

    let outcome = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            "eye-of-the-tiger",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");

    assert_eq!(
        outcome,
        TrackOutcome::Landed("/music/eye-of-the-tiger.flac".into())
    );
    assert_eq!(landed.lock().len(), 1);
    assert!(rig.holds.snapshot().is_empty());
    assert!(
        rig.lidarr.waits.lock().is_some(),
        "what a step with a source after it waited on is never asked"
    );
    stop.cancel();
}

/// A heart of Eye of the Tiger whose Lidarr step, run last, found Lidarr's indexers failing: held,
/// saved, with its line.
async fn held_for_indexers(rig: &Rig) {
    *rig.lidarr.waits.lock() = Some(LidarrWaits::Indexers);
    let outcome = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            "eye-of-the-tiger",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert_eq!(outcome, TrackOutcome::Held(WaitingFor::LidarrIndexers));
    let held = rig.holds.snapshot();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].waiting_for, Some(WaitingFor::LidarrIndexers));
    assert!(rig.tracker.is_held("soulseek", "eye-of-the-tiger"));
    assert_eq!(
        rig.tracker.note_of("soulseek", "eye-of-the-tiger").as_deref(),
        Some("Waiting for Lidarr's indexers")
    );
}

/// With Lidarr last, its failing indexers hold the heart until they search again: then every heart
/// waiting for them runs again, Lidarr takes it this time, and the hold ends.
#[tokio::test]
async fn with_lidarr_last_failing_indexers_hold_the_heart_until_they_search_again() {
    let rig = rig(&[HeartDownloadSource::Lidarr]).await;
    held_for_indexers(&rig).await;

    rig.coordinator.resume_waiting(WaitingFor::LidarrIndexers);

    until(|| rig.lidarr.track_calls.lock().len() == 2 && rig.holds.snapshot().is_empty()).await;
    until(|| !rig.tracker.is_held("soulseek", "eye-of-the-tiger")).await;
}

/// The person's own heart, given again, asks Lidarr again at once.
#[tokio::test]
async fn hearting_again_asks_lidarr_again_while_its_indexers_held_the_heart() {
    let rig = rig(&[HeartDownloadSource::Lidarr]).await;
    held_for_indexers(&rig).await;

    let again = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            "eye-of-the-tiger",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");

    assert_eq!(again, TrackOutcome::Held(WaitingFor::LidarrIndexers));
    until(|| rig.lidarr.track_calls.lock().len() == 2 && rig.holds.snapshot().is_empty()).await;
}

/// A restart runs a heart held for Lidarr's indexers again.
#[tokio::test]
async fn a_restart_runs_a_heart_held_for_lidarrs_indexers_again() {
    let rig = rig(&[HeartDownloadSource::Lidarr]).await;
    held_for_indexers(&rig).await;

    Arc::new(SoulseekHoldResumer::new(
        rig.holds.clone(),
        rig.coordinator.clone(),
    ))
    .run(CancellationToken::new())
    .await
    .expect("ran");

    until(|| rig.lidarr.track_calls.lock().len() == 2 && rig.holds.snapshot().is_empty()).await;
}

#[tokio::test]
async fn with_lidarr_last_an_imports_unanswered_walk_goes_back_on_its_trickle() {
    let rig = rig(&[HeartDownloadSource::Lidarr]).await;

    let error = rig
        .coordinator
        .acquire_track_for(
            "soulseek",
            "imported",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Import,
        )
        .await
        .expect_err("put back");

    let later = error.downcast_ref::<TrickleCheckLater>().expect("a check later");
    assert!(later.turn_back);
    assert!(
        rig.holds.snapshot().is_empty(),
        "an import never holds for MusicBrainz"
    );
}
