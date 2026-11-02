//! Holds, joiners and origins: a heart whose gate cannot be built yet holds, saved, its row held;
//! an answer, a restart or a second heart runs it again, one request per holder with their own
//! gate; an un-heart takes one person off; an import's busy identity goes back on the trickle; a
//! download that holds moves to the hold path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use octo_core::common::Clock;
use octo_core::fingerprint::heart_gate::HeardReading;
use octo_core::fingerprint::heart_release::{CatalogAlbum, CatalogAlbumHit, CatalogTrack};
use octo_core::settings::{AppSettings, HeartDownloadSource, HeartDownloadStep, SubsonicSettings};
use url::Url;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::GateOutcome;
use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr, until};
use crate::services::common::{AcquisitionRequest, SoulseekHoldResumer};
use crate::services::fingerprint::music_brainz_client::{CatalogQuestions, HeartBusy, HeartQuestions, Lane};
use crate::services::metadata::deezer_metadata_service::DeezerAsked;

/// Yesterday's ISRC, whose code lookup a heart's identity asks.
const CODE: &str = "GBAYE0601477";

fn youtube_only() -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            heart_download_sources: vec![HeartDownloadStep {
                source: HeartDownloadSource::YouTube,
                song_enabled: Some(true),
                album_enabled: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    }))
}

fn gate(holder: &Holder, origin: AcquisitionOrigin) -> HeartGate {
    HeartGate {
        user: holder.user.clone(),
        origin,
        r: None,
        heard: holder.heard.clone(),
        heard_seconds: None,
        reading: HeardReading::NotHeard,
        stand_in: None,
    }
}

/// Deezer, never asked.
struct NoCatalog;

#[async_trait]
impl CatalogQuestions for NoCatalog {
    async fn album_search(&self, _: &str, _: &str, _: Lane) -> DeezerAsked<Vec<CatalogAlbumHit>> {
        DeezerAsked::NoData
    }

    async fn album(&self, _: &str, _: Lane) -> DeezerAsked<(CatalogAlbum, Vec<CatalogTrack>)> {
        DeezerAsked::NoData
    }
}

/// Every gate is built from the song's code lookup on the heart lane: a question MusicBrainz does
/// not answer holds for it, naming the question.
struct CodeGates(HeartQuestions);

#[async_trait]
impl HeartGates for CodeGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        let gate = gate(holder, origin);
        match self.0.code_recordings(CODE).await {
            Ok(_) => GateStart::Ready(gate),
            Err(HeartBusy::MusicBrainz(question)) => GateStart::Hold {
                gate,
                waiting_for: WaitingFor::MusicBrainz,
                line: "Waiting for MusicBrainz".into(),
                question: Some(question),
            },
            Err(HeartBusy::Deezer) => GateStart::Hold {
                gate,
                waiting_for: WaitingFor::Deezer,
                line: "Waiting for Deezer".into(),
                question: None,
            },
        }
    }
}

#[test]
fn a_row_waiting_on_the_catalog_names_no_one_catalog() {
    // Deezer or Apple Music answers the catalog's questions, depending on the server's country.
    assert_eq!(
        crate::services::common::heart_acquisition_coordinator::waiting_line(WaitingFor::Deezer),
        "Waiting for the music catalog"
    );
}

/// Gates answered from a list per person: a listed wait holds, anyone else is ready.
#[derive(Default)]
struct ListedGates {
    holds: Mutex<HashMap<String, WaitingFor>>,
    built: Mutex<Vec<(String, AcquisitionOrigin)>>,
}

impl ListedGates {
    fn hold(&self, user: &str, waiting: WaitingFor) {
        self.holds.lock().insert(user.to_string(), waiting);
    }

    fn ready(&self, user: &str) {
        self.holds.lock().remove(user);
    }
}

#[async_trait]
impl HeartGates for ListedGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        self.built.lock().push((holder.user.clone(), origin));
        let gate = gate(holder, origin);
        let waiting = self.holds.lock().get(&holder.user).copied();
        match waiting {
            Some(waiting_for) => GateStart::Hold {
                gate,
                waiting_for,
                line: format!("Waiting for {}", waiting_for.name()),
                question: None,
            },
            None => GateStart::Ready(gate),
        }
    }
}

struct Rig {
    coordinator: Arc<HeartAcquisitionCoordinator>,
    queue: Arc<TrackAcquisitionQueue>,
    holds: Arc<SoulseekHoldStore>,
    tracker: Arc<AcquisitionTracker>,
    lidarr: Arc<FakeLidarr>,
}

fn rig(
    gates: Arc<dyn HeartGates>,
    music_brainz: Option<Arc<MusicBrainzClient>>,
    holds: Arc<SoulseekHoldStore>,
) -> Rig {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let lidarr = Arc::new(FakeLidarr::default());
    let coordinator = HeartAcquisitionCoordinator::new(
        youtube_only(),
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            holds: Some(holds.clone()),
            gates: Some(gates),
            music_brainz,
            ..Default::default()
        },
    );
    Rig {
        coordinator,
        queue,
        holds,
        tracker,
        lidarr,
    }
}

fn memory_holds() -> Arc<SoulseekHoldStore> {
    Arc::new(SoulseekHoldStore::new(None::<PathBuf>))
}

type Taken = Arc<Mutex<Vec<Arc<AcquisitionRequest>>>>;

/// Lands every request the queue hands out, keeping each one, until the token is canceled; each
/// gate on it accepts the file, as the download's check point writes it.
fn land_everything(queue: &Arc<TrackAcquisitionQueue>) -> (Taken, CancellationToken) {
    let taken: Taken = Arc::default();
    let stop = CancellationToken::new();
    let (queue, seen, token) = (queue.clone(), taken.clone(), stop.clone());
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            seen.lock().push(request.clone());
            for gate in request.gates() {
                request
                    .completion
                    .ledger()
                    .record(&gate.user, gate.origin, GateOutcome::Accepted);
            }
            queue.release(&request);
            request
                .completion
                .try_set_result(format!("/music/{}.flac", request.external_id));
        }
    });
    (taken, stop)
}

fn quick_music_brainz(server: &MockServer, timeout: Duration) -> Arc<MusicBrainzClient> {
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    let quick = Duration::from_millis(5);
    Arc::new(MusicBrainzClient::with_base_url(base).with_pace(quick, timeout, quick))
}

/// Yesterday's code lookup, as MusicBrainz answered it.
fn yesterday_code() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../octo-core/tests/fixtures/heart_album/rows/beatles-yesterday_a0/isrc-lookup.json"
    ))
    .expect("the recorded code lookup")
}

fn holder(user: &str) -> Holder {
    Holder {
        user: user.into(),
        ..Default::default()
    }
}

fn held(id: &str, origin: AcquisitionOrigin, waiting: WaitingFor, users: &[&str]) -> HeldAcquisition {
    HeldAcquisition {
        provider: "soulseek".into(),
        external_id: id.into(),
        requested_by: users.first().map(|user| user.to_string()),
        held_since_utc: Utc::now(),
        origin: Some(origin),
        waiting_for: Some(waiting),
        holders: users.iter().map(|user| holder(user)).collect(),
        line: Some(format!("Waiting for {}", waiting.name())),
        ..Default::default()
    }
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
        .filter(|event| {
            event.kind == crate::services::common::acquisition_tracker::acquisition_event_kinds::NOTE
        })
        .map(|event| event.text)
        .collect()
}

#[tokio::test]
async fn a_heart_whose_question_times_out_holds_and_the_answer_resumes_it() {
    let server = MockServer::builder().start().await;
    // The question and its three retries time out; the re-asker's next ask is answered.
    Mock::given(path(format!("/ws/2/isrc/{CODE}")))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .up_to_n_times(4)
        .mount(&server)
        .await;
    Mock::given(path(format!("/ws/2/isrc/{CODE}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(yesterday_code(), "application/json"))
        .mount(&server)
        .await;
    let music_brainz = quick_music_brainz(&server, Duration::from_millis(150));
    let questions = HeartQuestions::new(music_brainz.clone(), Arc::new(NoCatalog), Lane::Heart);
    let rig = rig(Arc::new(CodeGates(questions)), Some(music_brainz), memory_holds());
    let (landed, stop) = land_everything(&rig.queue);

    let outcome = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            "yesterday",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert_eq!(outcome, TrackOutcome::Held(WaitingFor::MusicBrainz));

    // MusicBrainz's answer to the held question resumes the heart: it lands, and its hold and
    // its row's hold end.
    until(|| landed.lock().len() == 1 && rig.holds.snapshot().is_empty()).await;
    let request = landed.lock()[0].clone();
    assert_eq!(request.external_id, "yesterday");
    assert_eq!(request.origin, Some(AcquisitionOrigin::Heart));
    let gated: Vec<String> = request.gates().into_iter().map(|gate| gate.user).collect();
    assert_eq!(gated, ["brandon"]);
    assert_eq!(
        notes(&rig.tracker, "soulseek:yesterday"),
        ["Waiting for MusicBrainz"]
    );
    until(|| !rig.tracker.is_held("soulseek", "yesterday")).await;
    let asked = server.received_requests().await.unwrap_or_default().len();
    assert!(asked >= 5, "four timed out before the answer: {asked}");
    stop.cancel();
}

#[tokio::test]
async fn an_import_whose_identity_answers_503_four_times_goes_back_on_the_trickle_then_runs_next_turn() {
    let server = MockServer::builder().start().await;
    Mock::given(path(format!("/ws/2/isrc/{CODE}")))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(4)
        .mount(&server)
        .await;
    Mock::given(path(format!("/ws/2/isrc/{CODE}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(yesterday_code(), "application/json"))
        .mount(&server)
        .await;
    let music_brainz = quick_music_brainz(&server, Duration::from_secs(5));
    let questions = HeartQuestions::new(music_brainz.clone(), Arc::new(NoCatalog), Lane::Heart);
    let rig = rig(Arc::new(CodeGates(questions)), Some(music_brainz), memory_holds());

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
    assert_eq!(later.detail, "Waiting for MusicBrainz to answer");
    assert!(later.turn_back);
    assert!(
        rig.holds.snapshot().is_empty(),
        "an import never holds for MusicBrainz"
    );
    assert!(!rig.tracker.is_held("soulseek", "imported"));
    assert_eq!(server.received_requests().await.unwrap_or_default().len(), 4);

    // Its next turn: MusicBrainz answers, and the song is fetched as an import.
    let (landed, stop) = land_everything(&rig.queue);
    rig.coordinator
        .acquire_track_for(
            "soulseek",
            "imported",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Import,
        )
        .await
        .expect("fetched");
    assert_eq!(landed.lock().len(), 1);
    assert_eq!(landed.lock()[0].origin, Some(AcquisitionOrigin::Import));
    stop.cancel();
}

#[tokio::test]
async fn a_download_that_waits_for_a_bulk_turn_moves_to_the_hold_path_and_its_job_ends() {
    let rig = rig(Arc::new(ListedGates::default()), None, memory_holds());
    let running = rig.coordinator.clone();
    let import = tokio::spawn(async move {
        running
            .acquire_track_for(
                "soulseek",
                "imported",
                Some("brandon"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Import,
            )
            .await
    });
    let request = rig
        .queue
        .dequeue(&CancellationToken::new())
        .await
        .expect("a request");
    assert_eq!(request.place().origin, Some(AcquisitionOrigin::Import));
    rig.queue.release(&request);
    let file = "/downloads/.octo-incoming/held/Track_soulseek_imported/a.flac";
    request
        .completion
        .try_set_error(anyhow::Error::new(AcquisitionHeld {
            waiting_for: WaitingFor::AcoustId,
            line: "Waiting for AcoustID".into(),
            files: vec![file.into()],
            lidarr: Vec::new(),
            users: Vec::new(),
        }));
    import
        .await
        .expect("joined")
        .expect("the job ends without an error");

    let record = rig
        .holds
        .get("Track:soulseek:imported:AcoustId")
        .expect("held for AcoustID");
    assert_eq!(record.origin(), AcquisitionOrigin::Import);
    assert_eq!(record.held_files, [file]);
    assert_eq!(users(&record), ["brandon"]);
    let row = rig.tracker.detail("soulseek:imported", None).expect("its row");
    assert_eq!(row.state, AcquisitionState::Searching);
    assert_eq!(row.note.as_deref(), Some("Waiting for AcoustID"));
    assert!(rig.tracker.is_held("soulseek", "imported"));
}

/// A file Lidarr brought that waits for AcoustID is filed on its hold with where its labels are
/// read from, in the same write as the file; the resumed hold hands both to its download; and once
/// the hold ends, nothing of them is left anywhere.
#[tokio::test]
async fn a_held_lidarr_files_labels_are_filed_with_it_handed_back_on_resume_and_end_with_the_hold() {
    use crate::services::common::soulseek_hold_store::HeldLidarrFile;

    let rig = rig(Arc::new(ListedGates::default()), None, memory_holds());
    let running = rig.coordinator.clone();
    let import = tokio::spawn(async move {
        running
            .acquire_track_for(
                "soulseek",
                "imported",
                Some("brandon"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Import,
            )
            .await
    });
    let request = rig
        .queue
        .dequeue(&CancellationToken::new())
        .await
        .expect("a request");
    rig.queue.release(&request);
    let file = "/downloads/.octo-incoming/held/Track_soulseek_imported/a.flac";
    let brought = HeldLidarrFile {
        path: file.into(),
        delivered: Some("/downloads/complete/a.flac".into()),
    };
    request
        .completion
        .try_set_error(anyhow::Error::new(AcquisitionHeld {
            waiting_for: WaitingFor::AcoustId,
            line: "Waiting for AcoustID".into(),
            files: vec![file.into()],
            lidarr: vec![brought.clone()],
            users: Vec::new(),
        }));
    import
        .await
        .expect("joined")
        .expect("the job ends without an error");
    let key = "Track:soulseek:imported:AcoustId";
    let record = rig.holds.get(key).expect("held for AcoustID");
    assert_eq!(record.held_files, [file]);
    assert_eq!(record.lidarr_files, std::slice::from_ref(&brought));

    rig.coordinator.resume_held(key);
    let again = tokio::time::timeout(
        Duration::from_secs(10),
        rig.queue.dequeue(&CancellationToken::new()),
    )
    .await
    .expect("the chain runs again")
    .expect("a request");
    assert_eq!(again.place().held_files, [file]);
    assert_eq!(again.place().held_lidarr, [brought]);
    rig.queue.release(&again);
    again.completion.try_set_result("/music/imported.flac");

    until(|| rig.holds.snapshot().is_empty()).await;
    assert!(
        rig.holds
            .snapshot()
            .iter()
            .all(|record| record.lidarr_files.is_empty())
    );
}

#[tokio::test]
async fn a_second_heart_joins_the_hold_and_one_unheart_leaves_the_others() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::NotYetProven);
    gates.hold("anna", WaitingFor::NotYetProven);
    let rig = rig(gates.clone(), None, memory_holds());
    let heart = |user: &'static str| {
        let coordinator = rig.coordinator.clone();
        async move {
            coordinator
                .acquire_track_outcome(
                    "soulseek",
                    "help",
                    Some(user),
                    None,
                    &AcquisitionTarget::Shared,
                    AcquisitionOrigin::Heart,
                )
                .await
                .expect("ran")
        }
    };
    assert_eq!(
        heart("brandon").await,
        TrackOutcome::Held(WaitingFor::NotYetProven)
    );
    // A second person's heart joins the hold and asks again at once, each person's own gate.
    assert_eq!(heart("anna").await, TrackOutcome::Held(WaitingFor::NotYetProven));
    let key = "Track:soulseek:help:NotYetProven";
    until(|| !rig.coordinator.resuming.lock().contains(key)).await;
    let record = rig.holds.get(key).expect("held");
    assert_eq!(users(&record), ["brandon", "anna"]);
    assert_eq!(rig.holds.snapshot().len(), 1, "one record");
    let asked: Vec<String> = gates.built.lock().iter().map(|(user, _)| user.clone()).collect();
    assert_eq!(
        asked,
        ["brandon", "brandon", "anna"],
        "the resume built both gates"
    );
    assert!(rig.queue.is_idle(), "nothing downloads while every gate holds");

    assert_eq!(rig.coordinator.leave_holds("soulseek", "help", "Anna"), 1);
    assert_eq!(
        users(&rig.holds.get(key).expect("brandon still holds it")),
        ["brandon"]
    );
    assert!(rig.tracker.is_held("soulseek", "help"));
    assert_eq!(
        rig.coordinator.leave_holds("soulseek", "help", "anna"),
        0,
        "not a holder now"
    );
    assert_eq!(rig.coordinator.leave_holds("soulseek", "help", "brandon"), 1);
    assert!(rig.holds.snapshot().is_empty());
    assert!(!rig.tracker.is_held("soulseek", "help"));
}

#[tokio::test]
async fn a_record_with_two_holders_resumes_as_one_request_per_holder_with_their_own_gate() {
    let gates = Arc::new(ListedGates::default());
    let holds = memory_holds();
    holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::AcoustId,
        &["brandon", "anna"],
    ));
    let rig = rig(gates.clone(), None, holds);
    let (landed, stop) = land_everything(&rig.queue);

    let resumer = Arc::new(SoulseekHoldResumer::new(
        rig.holds.clone(),
        rig.coordinator.clone(),
    ));
    resumer.resume_waiting(WaitingFor::AcoustId);
    until(|| rig.holds.snapshot().is_empty()).await;

    let taken = landed.lock().clone();
    assert_eq!(taken.len(), 2, "one request per person");
    let mut gated: Vec<Vec<String>> = taken
        .iter()
        .map(|request| request.gates().into_iter().map(|gate| gate.user).collect())
        .collect();
    gated.sort();
    assert_eq!(gated, [vec!["anna"], vec!["brandon"]], "each with their own gate");
    assert!(
        taken
            .iter()
            .all(|request| request.origin == Some(AcquisitionOrigin::Heart))
    );
    stop.cancel();
}

#[tokio::test]
async fn a_holder_whose_gate_holds_again_stays_held_while_the_other_lands() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("anna", WaitingFor::AcoustId);
    let holds = memory_holds();
    holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::AcoustId,
        &["brandon", "anna"],
    ));
    let rig = rig(gates.clone(), None, holds);
    let (landed, stop) = land_everything(&rig.queue);
    rig.coordinator.resume_held("Track:soulseek:help:AcoustId");
    until(|| landed.lock().len() == 1).await;
    until(|| {
        !rig.coordinator
            .resuming
            .lock()
            .contains("Track:soulseek:help:AcoustId")
    })
    .await;

    let gated: Vec<String> = landed.lock()[0]
        .gates()
        .into_iter()
        .map(|gate| gate.user)
        .collect();
    assert_eq!(gated, ["brandon"]);
    let record = rig
        .holds
        .get("Track:soulseek:help:AcoustId")
        .expect("anna stays held");
    assert_eq!(users(&record), ["anna"]);
    assert!(rig.tracker.is_held("soulseek", "help"));

    // Her answer comes: the record ends with her file.
    gates.ready("anna");
    rig.coordinator.resume_held("Track:soulseek:help:AcoustId");
    until(|| rig.holds.snapshot().is_empty()).await;
    assert_eq!(landed.lock().len(), 2);
    until(|| !rig.tracker.is_held("soulseek", "help")).await;
    stop.cancel();
}

#[tokio::test]
async fn holds_of_every_origin_survive_a_restart_and_come_back_with_their_rows() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let file = directory.path().join("soulseek-holds.json");
    {
        let before = SoulseekHoldStore::new(Some(&file));
        before.hold(held(
            "hearted",
            AcquisitionOrigin::Heart,
            WaitingFor::MusicBrainz,
            &["brandon", "anna"],
        ));
        before.hold(held(
            "imported",
            AcquisitionOrigin::Import,
            WaitingFor::AcoustId,
            &["brandon"],
        ));
    }
    // After the restart nothing answers yet: both are held again, rows and all.
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::MusicBrainz);
    gates.hold("anna", WaitingFor::MusicBrainz);
    let rig = rig(gates.clone(), None, Arc::new(SoulseekHoldStore::new(Some(&file))));
    let resumer = Arc::new(SoulseekHoldResumer::new(
        rig.holds.clone(),
        rig.coordinator.clone(),
    ));
    resumer.clone().run(CancellationToken::new()).await.expect("ran");

    let rows = rig.tracker.all();
    let mut seen: Vec<(String, Option<String>, Vec<String>)> = rows
        .iter()
        .map(|row| {
            (
                row.external_id.clone(),
                row.note.clone(),
                row.requested_by.clone(),
            )
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            (
                "hearted".to_string(),
                Some("Waiting for MusicBrainz".to_string()),
                vec!["anna".to_string(), "brandon".to_string()]
            ),
            (
                "imported".to_string(),
                Some("Waiting for AcoustId".to_string()),
                vec!["brandon".to_string()]
            ),
        ]
    );
    assert!(rig.tracker.is_held("soulseek", "hearted") && rig.tracker.is_held("soulseek", "imported"));
    until(|| rig.coordinator.resuming.lock().is_empty()).await;

    // Their answers come: each is fetched with its own origin, the heart once per person in it,
    // and every hold ends.
    let (landed, stop) = land_everything(&rig.queue);
    gates.ready("brandon");
    gates.ready("anna");
    resumer.resume_waiting(WaitingFor::MusicBrainz);
    resumer.resume("Track:soulseek:imported:AcoustId");
    until(|| rig.holds.snapshot().is_empty()).await;
    let mut origins: Vec<(String, Option<AcquisitionOrigin>)> = landed
        .lock()
        .iter()
        .map(|request| (request.external_id.clone(), request.origin))
        .collect();
    origins.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        origins,
        [
            ("hearted".to_string(), Some(AcquisitionOrigin::Heart)),
            ("hearted".to_string(), Some(AcquisitionOrigin::Heart)),
            ("imported".to_string(), Some(AcquisitionOrigin::Import)),
        ]
    );
    assert!(
        SoulseekHoldStore::new(Some(&file)).snapshot().is_empty(),
        "saved as ended"
    );
    stop.cancel();
}

#[tokio::test]
async fn a_play_handed_to_lidarr_says_it_is_a_play_and_a_play_never_holds() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::MusicBrainz);
    let rig = rig(gates.clone(), None, memory_holds());
    rig.coordinator
        .hand_play_to_lidarr("soulseek:played", "soulseek", "played", Some("brandon"), &[])
        .await;
    assert_eq!(
        rig.lidarr.origins.lock().as_slice(),
        [(AcquisitionOrigin::Play, 0)]
    );

    // A play's gate that would hold is no gate at all: the play fetches nothing, and nothing is
    // held.
    let held = rig
        .coordinator
        .play_gates("soulseek", "played", Some("brandon"), None)
        .await;
    assert!(held.is_err(), "{held:?}");
    assert!(rig.holds.snapshot().is_empty());

    // A ready gate is the play's only gate.
    gates.ready("brandon");
    let ready = rig
        .coordinator
        .play_gates("soulseek", "played", Some("brandon"), None)
        .await
        .expect("a ready gate");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].origin, AcquisitionOrigin::Play);
}

// ---- Held files, the resumed row, heard copies and plays ------------------------------------

#[tokio::test]
async fn a_resumed_hold_brings_its_held_files_and_its_row_leaves_the_hold_line_at_once() {
    let holds = memory_holds();
    let rig = rig(Arc::new(ListedGates::default()), None, holds.clone());
    let mut record = held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::AcoustId,
        &["brandon"],
    );
    record.held_files = vec!["/downloads/.octo-incoming/held/Track_soulseek_help/help.flac".into()];
    let record = holds.hold(record);
    rig.coordinator.hold_row_for(&record);
    assert_eq!(
        rig.tracker.note_of("soulseek", "help").as_deref(),
        Some("Waiting for AcoustId")
    );

    rig.coordinator.resume_held(&record.key());
    let request = tokio::time::timeout(
        Duration::from_secs(10),
        rig.queue.dequeue(&CancellationToken::new()),
    )
    .await
    .expect("the chain runs again")
    .expect("a request");
    // The row left its line when the resume started, before the download staged anything.
    assert_eq!(rig.tracker.note_of("soulseek", "help"), None);
    assert!(rig.tracker.is_held("soulseek", "help"), "held until the run ends");
    assert_eq!(request.place().held_files, record.held_files);
    for gate in request.gates() {
        request
            .completion
            .ledger()
            .record(&gate.user, gate.origin, GateOutcome::Accepted);
    }
    rig.queue.release(&request);
    request.completion.try_set_result("/music/help.flac");
    until(|| holds.snapshot().is_empty()).await;
    until(|| !rig.tracker.is_held("soulseek", "help")).await;
}

#[tokio::test]
async fn two_people_held_in_one_record_hold_one_heard_copy_and_one_unheart_leaves_it_held() {
    use crate::services::prefetch::heard_samples::tests::rig_in;
    use crate::services::soulseek::SharedRouting;
    use octo_core::fingerprint::heart_gate::{HeardRecord, HeardSource};
    use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};

    const VIDEO: &str = "AAAAAAAAAA1";
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("copies")).unwrap();
    std::fs::write(
        dir.path().join("copies").join(format!("{VIDEO}.preview")),
        vec![0u8; 10],
    )
    .unwrap();
    let holds = memory_holds();
    let heard = rig_in(dir.path(), "http://127.0.0.1:1", holds.clone(), 16, 1 << 20);
    let id = heard.registry.register(SharedRouting::new(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("Help!".into()),
        ..Default::default()
    }));
    for user in ["brandon", "anna"] {
        heard.registry.remember_heard(
            &id,
            user,
            HeardRecord {
                heard_at: Utc::now(),
                source: HeardSource::Video {
                    id: VIDEO.into(),
                    size: Some(10),
                    content_type: "audio/mp4".into(),
                    itag: None,
                },
                sample_id: None,
            },
            None,
        );
    }
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::NotYetProven);
    gates.hold("anna", WaitingFor::NotYetProven);
    let coordinator = HeartAcquisitionCoordinator::new(
        youtube_only(),
        Arc::new(TrackAcquisitionQueue::new()),
        Arc::new(FakeDownloads::default()),
        Arc::new(FakeLidarr::default()),
        HeartCoordinatorExtras {
            tracker: Some(Arc::new(AcquisitionTracker::new(None, Clock::system()))),
            id_registry: Some(heard.registry.clone()),
            holds: Some(holds.clone()),
            gates: Some(gates),
            heard: Some(heard.samples.clone()),
            ..Default::default()
        },
    );
    for user in ["brandon", "anna"] {
        coordinator
            .acquire_track_outcome(
                "soulseek",
                &id,
                Some(user),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
            .expect("ran");
    }
    let key = format!("Track:soulseek:{id}:NotYetProven");
    until(|| !coordinator.resuming.lock().contains(&key)).await;
    assert_eq!(users(&holds.get(&key).expect("held")), ["brandon", "anna"]);
    assert!(heard.copies.is_held(VIDEO));

    assert_eq!(coordinator.leave_holds("soulseek", &id, "anna"), 1);
    assert!(heard.copies.is_held(VIDEO), "brandon still holds it");
    heard.copies.drop_copy(VIDEO);
    assert!(heard.copies.is_ready(VIDEO));
    assert_eq!(coordinator.leave_holds("soulseek", &id, "brandon"), 1);
    assert!(!heard.copies.is_held(VIDEO), "nobody holds it now");
}

/// A coordinator for plays with DownloadOnPlay and LidarrAlbumOnPlay on, AcoustID `on` or off.
fn play_rig(on: bool, gates: Arc<dyn HeartGates>) -> Rig {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            download_on_play: true,
            lidarr_album_on_play: true,
            heart_download_sources: vec![HeartDownloadStep {
                source: HeartDownloadSource::YouTube,
                song_enabled: Some(true),
                album_enabled: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    }));
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let lidarr = Arc::new(FakeLidarr::default());
    let holds = memory_holds();
    let coordinator = HeartAcquisitionCoordinator::new(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            holds: Some(holds.clone()),
            gates: Some(gates),
            acoust_id_on: Some(Arc::new(move || on)),
            ..Default::default()
        },
    );
    Rig {
        coordinator,
        queue,
        holds,
        tracker,
        lidarr,
    }
}

fn heard_video() -> HeardRecord {
    HeardRecord {
        heard_at: Utc::now(),
        source: octo_core::fingerprint::heart_gate::HeardSource::Video {
            id: "AAAAAAAAAA1".into(),
            size: Some(10),
            content_type: "audio/mp4".into(),
            itag: None,
        },
        sample_id: None,
    }
}

#[tokio::test]
async fn with_acoust_id_off_a_play_that_heard_nothing_fetches_nothing_from_any_source() {
    let rig = play_rig(false, Arc::new(ListedGates::default()));
    rig.coordinator.queue_play_heard(
        "soulseek",
        "played",
        Some("brandon"),
        Some("c"),
        Some("brandon"),
        None,
    );
    rig.coordinator
        .queue_play_ahead("soulseek", "next", Some("brandon"), Some("c"), Some("brandon"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.queue.waiting_plays(), 0, "no download");
    assert!(rig.lidarr.origins.lock().is_empty(), "no Lidarr album");

    // A play that heard its audio still runs both, judged against what it heard.
    let heard = heard_video();
    rig.coordinator.queue_play_heard(
        "soulseek",
        "played",
        Some("brandon"),
        Some("c"),
        Some("brandon"),
        Some(heard.clone()),
    );
    until(|| rig.queue.waiting_plays() == 1 && rig.lidarr.origins.lock().len() == 1).await;
    // The play's album through Lidarr is judged by the play's own gate too.
    assert_eq!(
        rig.lidarr.origins.lock().as_slice(),
        [(AcquisitionOrigin::Play, 1)]
    );
    let request = tokio::time::timeout(
        Duration::from_secs(10),
        rig.queue.dequeue(&CancellationToken::new()),
    )
    .await
    .expect("queued")
    .expect("the play's download");
    assert_eq!(request.origin, Some(AcquisitionOrigin::Play));
    let gates = request.gates();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0].heard, Some(heard));
}

#[tokio::test]
async fn a_play_whose_gate_would_wait_downloads_nothing_and_holds_nothing() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::AcoustIdOff);
    let rig = play_rig(false, gates.clone());
    rig.coordinator.queue_play_heard(
        "soulseek",
        "played",
        Some("brandon"),
        Some("c"),
        Some("brandon"),
        Some(heard_video()),
    );
    until(|| !gates.built.lock().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rig.queue.waiting_plays(), 0);
    assert!(rig.lidarr.origins.lock().is_empty());
    assert!(rig.holds.snapshot().is_empty(), "a play never holds");
    assert_eq!(
        gates.built.lock()[0],
        ("brandon".to_string(), AcquisitionOrigin::Play)
    );
}

// ---- Each holder's own verdict on a file that landed ------------------------------------------

type Accepting = Arc<Mutex<Vec<String>>>;

/// Lands every request the queue hands out once `askers` gates are on it, writing each gate's
/// verdict as the download's check point does: accepted for a user listed in `accepting`, not
/// accepted for anyone else.
fn land_judged(
    queue: &Arc<TrackAcquisitionQueue>,
    askers: usize,
    accepting: Accepting,
) -> (Taken, CancellationToken) {
    let taken: Taken = Arc::default();
    let stop = CancellationToken::new();
    let (queue, seen, token) = (queue.clone(), taken.clone(), stop.clone());
    tokio::spawn(async move {
        let mut first = true;
        while let Some(request) = queue.dequeue(&token).await {
            // The first request waits for everyone who joins it; a later one is someone's own.
            while first && request.gates().len() < askers {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            first = false;
            seen.lock().push(request.clone());
            let ledger = request.completion.ledger().clone();
            for gate in request.gates() {
                let outcome = if accepting.lock().contains(&gate.user) {
                    GateOutcome::Accepted
                } else {
                    GateOutcome::NotAccepted("another mix".into())
                };
                ledger.record(&gate.user, gate.origin, outcome);
            }
            queue.release(&request);
            request
                .completion
                .try_set_result(format!("/music/{}.flac", request.external_id));
        }
    });
    (taken, stop)
}

/// A record with two holders who heard two mixes runs one request per person: the file of the
/// first's mix lands for the first, the second's own requests land nothing that is their mix and
/// they stay held, and the record ends only when the second's own file lands.
#[tokio::test]
async fn two_holders_who_heard_two_mixes_each_wait_for_their_own_file() {
    let gates = Arc::new(ListedGates::default());
    let holds = memory_holds();
    holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::NotYetProven,
        &["brandon", "anna"],
    ));
    let rig = rig(gates.clone(), None, holds);
    let accepting: Accepting = Arc::new(Mutex::new(vec!["brandon".to_string()]));
    let (landed, stop) = land_judged(&rig.queue, 1, accepting.clone());
    let key = "Track:soulseek:help:NotYetProven";
    rig.coordinator.resume_held(key);
    until(|| !landed.lock().is_empty()).await;
    until(|| !rig.coordinator.resuming.lock().contains(key)).await;
    assert!(
        landed.lock().iter().all(|request| request.gates().len() == 1),
        "one person per request"
    );
    let record = rig.holds.get(key).expect("anna stays held");
    assert_eq!(users(&record), ["anna"]);
    assert!(rig.tracker.is_held("soulseek", "help"));

    // Her own mix lands: the record ends.
    let before = landed.lock().len();
    *accepting.lock() = vec!["anna".to_string()];
    rig.coordinator.resume_held(key);
    until(|| rig.holds.snapshot().is_empty()).await;
    assert_eq!(landed.lock().len(), before + 1);
    until(|| !rig.tracker.is_held("soulseek", "help")).await;
    stop.cancel();
}

/// Two people with different R heart one song: two requests, each with their own gate. The file
/// lands for the one whose gate passed it, and never counts as the other's, whose own heart goes
/// on down its own chain, and with nothing proven for a heart never played it holds for a play.
#[tokio::test]
async fn two_hearts_of_one_song_each_get_their_own() {
    let rig = rig(Arc::new(ListedGates::default()), None, memory_holds());
    let accepting: Accepting = Arc::new(Mutex::new(vec!["brandon".to_string()]));
    let (landed, stop) = land_judged(&rig.queue, 1, accepting);
    let heart = |user: &'static str| {
        let coordinator = rig.coordinator.clone();
        tokio::spawn(async move {
            coordinator
                .acquire_track_outcome(
                    "soulseek",
                    "help",
                    Some(user),
                    None,
                    &AcquisitionTarget::Shared,
                    AcquisitionOrigin::Heart,
                )
                .await
                .expect("ran")
        })
    };
    let brandon = heart("brandon");
    assert_eq!(
        brandon.await.expect("ran"),
        TrackOutcome::Landed("/music/help.flac".into())
    );
    let anna = heart("anna");
    assert_eq!(
        anna.await.expect("joined"),
        TrackOutcome::Held(WaitingFor::NotYetProven),
        "the 2009 file is not what anna heard, and her own copy proved nothing"
    );
    let landed = landed.lock().clone();
    let gated: Vec<Vec<String>> = landed
        .iter()
        .map(|request| request.gates().into_iter().map(|gate| gate.user).collect())
        .collect();
    assert_eq!(gated[0], ["brandon"], "brandon's own request");
    assert!(gated[1..].iter().all(|users| users == &["anna"]), "{gated:?}");
    let record = rig
        .holds
        .get("Track:soulseek:help:NotYetProven")
        .expect("anna holds for a play");
    assert_eq!(users(&record), ["anna"]);
    stop.cancel();
}

// ---- Keeping the heard audio when nothing is proven -------------------------------------------

/// Gates of people who heard the song: ready, reading `reading` once something was heard (their
/// own heard source, else the one listed for them), never played otherwise; held for `hold`
/// when set.
#[derive(Default)]
struct HeardGates {
    reading: Mutex<Option<HeardReading>>,
    heard: Mutex<HashMap<String, HeardRecord>>,
    hold: Mutex<Option<WaitingFor>>,
}

impl HeardGates {
    fn reading(reading: HeardReading) -> Arc<Self> {
        let gates = HeardGates::default();
        *gates.reading.lock() = Some(reading);
        Arc::new(gates)
    }

    fn heard(&self, user: &str, record: HeardRecord) {
        self.heard.lock().insert(user.to_string(), record);
    }
}

#[async_trait]
impl HeartGates for HeardGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        let mut gate = gate(holder, origin);
        gate.heard = holder
            .heard
            .clone()
            .or_else(|| self.heard.lock().get(&holder.user).cloned());
        if gate.heard.is_some() {
            gate.reading = self
                .reading
                .lock()
                .clone()
                .unwrap_or(HeardReading::OnlyHeardOrMaster);
        }
        match *self.hold.lock() {
            Some(waiting_for) => GateStart::Hold {
                gate,
                waiting_for,
                line: waiting_line(waiting_for).to_string(),
                question: None,
            },
            None => GateStart::Ready(gate),
        }
    }
}

/// A heart coordinator whose chain runs these sources, with these gates.
fn rig_with(sources: &[HeartDownloadSource], gates: Arc<dyn HeartGates>) -> Rig {
    rig_checking(sources, gates, None)
}

/// [`rig_with`], checking these kept files again on the events their checks follow.
fn rig_checking(
    sources: &[HeartDownloadSource],
    gates: Arc<dyn HeartGates>,
    kept: Option<Arc<crate::services::library::upgrade_queue::KeptRechecks>>,
) -> Rig {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            heart_download_sources: sources
                .iter()
                .map(|&source| HeartDownloadStep {
                    source,
                    song_enabled: Some(true),
                    album_enabled: Some(true),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let lidarr = Arc::new(FakeLidarr::default());
    let holds = memory_holds();
    let coordinator = HeartAcquisitionCoordinator::new(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            holds: Some(holds.clone()),
            gates: Some(gates),
            kept,
            ..Default::default()
        },
    );
    Rig {
        coordinator,
        queue,
        holds,
        tracker,
        lidarr,
    }
}

/// What the worker does with the `n`th request it is handed: None lands it, accepted by every
/// gate on it (and closes its row, as the download's placement does); Some fails it with these
/// words, writing `Undecided` for every gate when the words say "undecided".
type Judge = Arc<dyn Fn(&AcquisitionRequest, usize) -> Option<String> + Send + Sync>;

fn work(rig: &Rig, judge: Judge) -> (Taken, CancellationToken) {
    let taken: Taken = Arc::default();
    let stop = CancellationToken::new();
    let (queue, seen, token, tracker) = (
        rig.queue.clone(),
        taken.clone(),
        stop.clone(),
        rig.tracker.clone(),
    );
    tokio::spawn(async move {
        let mut n = 0;
        while let Some(request) = queue.dequeue(&token).await {
            seen.lock().push(request.clone());
            let ledger = request.completion.ledger().clone();
            let verdict = judge(&request, n);
            n += 1;
            queue.release(&request);
            match verdict {
                None => {
                    for gate in request.gates() {
                        ledger.record(&gate.user, gate.origin, GateOutcome::Accepted);
                    }
                    tracker.complete(&request.provider, &request.external_id, None);
                    request
                        .completion
                        .try_set_result(format!("/music/{}.m4a", request.external_id));
                }
                Some(words) => {
                    if words.contains("undecided") {
                        for gate in request.gates() {
                            ledger.record(&gate.user, gate.origin, GateOutcome::Undecided);
                        }
                    }
                    request.completion.try_set_error(anyhow::anyhow!(words));
                }
            }
        }
    });
    (taken, stop)
}

/// Every source fails; the heard audio is had.
fn sources_fail_heard_lands() -> Judge {
    Arc::new(|request: &AcquisitionRequest, _| {
        let keeping = request.source_override == Some(DownloadSource::YouTube)
            && request
                .gates()
                .iter()
                .all(|gate| gate.reading == HeardReading::OnlyHeardOrMaster);
        (!keeping).then(|| "nothing it could prove".to_string())
    })
}

/// What a gate's person heard, whenever they heard it.
fn heard_of(gate: &HeartGate) -> Option<octo_core::fingerprint::heart_gate::HeardSource> {
    gate.heard.as_ref().map(|heard| heard.source.clone())
}

async fn heart_of(rig: &Rig, user: &str) -> TrackOutcome {
    rig.coordinator
        .acquire_track_outcome(
            "soulseek",
            "help",
            Some(user),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran")
}

/// Every source ran and none brought a copy that could be proven: the heart keeps the very audio
/// the person heard, by its identity, kept as heard, its row saying so in one line. A source that
/// can search means Octo keeps looking.
#[tokio::test]
async fn a_heart_with_nothing_proven_keeps_the_heard_audio_with_its_line() {
    let gates = HeardGates::reading(HeardReading::RStays { tag: None });
    gates.heard("brandon", heard_video());
    let rig = rig_with(&[HeartDownloadSource::Soulseek], gates);
    rig.tracker.begin(
        "soulseek",
        "help",
        None,
        Some("brandon"),
        Some("The Beatles"),
        Some("Help!"),
        None,
    );
    let (taken, stop) = work(&rig, sources_fail_heard_lands());

    let outcome = heart_of(&rig, "brandon").await;

    assert_eq!(outcome, TrackOutcome::KeptAsHeard("/music/help.m4a".into()));
    let taken = taken.lock().clone();
    assert_eq!(taken.len(), 2, "the source, then the heard audio itself");
    assert_eq!(taken[0].source_override, Some(DownloadSource::Soulseek));
    assert_eq!(taken[1].source_override, Some(DownloadSource::YouTube));
    let kept = taken[1].gates();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        kept[0].reading,
        HeardReading::OnlyHeardOrMaster,
        "only the heard audio passes"
    );
    assert_eq!(heard_of(&kept[0]), Some(heard_video().source));
    assert_eq!(
        rig.tracker.note_of("soulseek", "help").as_deref(),
        Some(KEPT_AS_HEARD)
    );
    assert!(rig.holds.snapshot().is_empty());
    stop.cancel();
}

/// The heart's sources cannot search (YouTube alone): the line says to add one. A copy AcoustID
/// could not tell apart from another version: the line says Octo could not prove which this is.
#[tokio::test]
async fn the_kept_line_says_when_no_source_can_search_and_when_a_copy_could_not_be_told_apart() {
    for (sources, first_fails, line) in [
        (
            vec![HeartDownloadSource::YouTube],
            "nothing it could prove",
            KEPT_NO_SOURCE,
        ),
        (
            vec![HeartDownloadSource::Soulseek],
            "undecided: the version could not be told apart",
            KEPT_UNDECIDED,
        ),
    ] {
        let gates = HeardGates::reading(HeardReading::RStays { tag: None });
        gates.heard("brandon", heard_video());
        let rig = rig_with(&sources, gates);
        rig.tracker.begin(
            "soulseek",
            "help",
            None,
            Some("brandon"),
            None,
            Some("Help!"),
            None,
        );
        let (_, stop) = work(
            &rig,
            Arc::new(move |_: &AcquisitionRequest, n| (n == 0).then(|| first_fails.to_string())),
        );

        assert_eq!(
            heart_of(&rig, "brandon").await,
            TrackOutcome::KeptAsHeard("/music/help.m4a".into())
        );
        assert_eq!(rig.tracker.note_of("soulseek", "help").as_deref(), Some(line));
        stop.cancel();
    }
}

/// Lidarr as the heart's only source: it can look for a better copy only for an artist with no
/// music in the user's Lidarr. For an artist that has music there, Lidarr is left alone, and the
/// line says to add a source that can search.
#[tokio::test]
async fn the_kept_line_counts_lidarr_only_for_an_artist_it_may_search() {
    for (left_alone, line) in [(false, KEPT_AS_HEARD), (true, KEPT_NO_SOURCE)] {
        let gates = HeardGates::reading(HeardReading::RStays { tag: None });
        gates.heard("brandon", heard_video());
        let rig = rig_with(&[HeartDownloadSource::Lidarr], gates);
        *rig.lidarr.left_alone.lock() = left_alone;
        rig.tracker.begin(
            "soulseek",
            "help",
            None,
            Some("brandon"),
            None,
            Some("Help!"),
            None,
        );
        let (_, stop) = work(&rig, Arc::new(|_: &AcquisitionRequest, _| None));

        let outcome = heart_of(&rig, "brandon").await;

        assert!(matches!(outcome, TrackOutcome::KeptAsHeard(_)), "{outcome:?}");
        assert_eq!(rig.lidarr.track_calls_for("soulseek", "help", true), 1);
        assert_eq!(
            rig.tracker.note_of("soulseek", "help").as_deref(),
            Some(line),
            "left alone: {left_alone}"
        );
        stop.cancel();
    }
}

/// When even the heard audio cannot be had (the video gone), the heart ends with the plain line.
#[tokio::test]
async fn a_heart_whose_heard_video_is_gone_ends_with_the_plain_line() {
    let gates = HeardGates::reading(HeardReading::OnlyHeardOrMaster);
    gates.heard("brandon", heard_video());
    let rig = rig_with(&[HeartDownloadSource::Soulseek], gates);
    rig.tracker.begin(
        "soulseek",
        "help",
        None,
        Some("brandon"),
        None,
        Some("Help!"),
        None,
    );
    let (taken, stop) = work(
        &rig,
        Arc::new(|_: &AcquisitionRequest, _| Some("gone".to_string())),
    );

    assert_eq!(
        heart_of(&rig, "brandon").await,
        TrackOutcome::Failed(HEARD_GONE.to_string())
    );
    assert_eq!(taken.lock().len(), 2, "the heard audio was asked for");
    let row = rig.tracker.detail("soulseek:help", None).expect("the row");
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.error.as_deref(), Some(HEARD_GONE));
    assert!(rig.holds.snapshot().is_empty());
    stop.cancel();
}

/// A list heart of a song AcoustID has no entry for, never played, with every source refused,
/// holds with "Play it once"; one play then keeps the heard audio and the hold ends.
#[tokio::test]
async fn a_list_heart_never_played_holds_play_it_once_and_one_play_keeps_the_heard_audio() {
    let gates = HeardGates::reading(HeardReading::OnlyHeardOrMaster);
    let rig = rig_with(&[HeartDownloadSource::Soulseek], gates);
    let (taken, stop) = work(&rig, sources_fail_heard_lands());

    assert_eq!(
        heart_of(&rig, "brandon").await,
        TrackOutcome::Held(WaitingFor::NotYetProven)
    );
    let key = "Track:soulseek:help:NotYetProven";
    let record = rig.holds.get(key).expect("held for a play");
    assert_eq!(record.line.as_deref(), Some(NEVER_PLAYED));
    assert_eq!(taken.lock().len(), 1, "the source ran, and nothing was kept");
    assert_eq!(
        rig.tracker.note_of("soulseek", "help").as_deref(),
        Some(NEVER_PLAYED)
    );

    // The person plays it: the hold runs with what they heard, and keeps it.
    rig.coordinator.heard_again("help", "brandon", &heard_video());
    until(|| rig.holds.snapshot().is_empty()).await;
    let taken = taken.lock().clone();
    assert_eq!(taken.len(), 3, "the source again, then the heard audio");
    assert_eq!(taken[2].source_override, Some(DownloadSource::YouTube));
    assert_eq!(heard_of(&taken[2].gates()[0]), Some(heard_video().source));
    stop.cancel();
}

/// A search heart whose stand-in AcoustID does not know has nothing to prove a copy by: its gate
/// holds before any source runs, so nothing is downloaded or kept until the person streams it.
#[tokio::test]
async fn a_search_heart_with_an_unknown_stand_in_keeps_nothing_until_it_is_streamed() {
    let gates = Arc::new(HeardGates::default());
    *gates.hold.lock() = Some(WaitingFor::NotYetProven);
    let rig = rig_with(&[HeartDownloadSource::Soulseek], gates.clone());
    let (taken, stop) = work(&rig, sources_fail_heard_lands());

    assert_eq!(
        heart_of(&rig, "brandon").await,
        TrackOutcome::Held(WaitingFor::NotYetProven)
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(taken.lock().is_empty(), "nothing downloaded");

    *gates.hold.lock() = None;
    rig.coordinator.heard_again("help", "brandon", &heard_video());
    until(|| rig.holds.snapshot().is_empty()).await;
    assert_eq!(taken.lock().len(), 2, "the source, then the heard audio");
    stop.cancel();
}

/// A held album track waits for a person's proof; after a restart it is asked again once, its
/// row back with its line, without anyone playing it first.
#[tokio::test]
async fn a_held_album_track_is_asked_again_after_a_restart() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let file = directory.path().join("soulseek-holds.json");
    let line = "1 track waits: play it once, or keep it anyway";
    let held_file = directory.path().join("help.flac");
    std::fs::write(&held_file, b"the held track").expect("written");
    {
        let before = SoulseekHoldStore::new(Some(&file));
        before.hold(HeldAcquisition {
            held_files: vec![held_file.to_string_lossy().to_string()],
            album_track: Some(crate::services::common::soulseek_hold_store::HeldAlbumTrack {
                album_key: "soulseek:help-album".into(),
                song_ids: vec!["help".into()],
                artist: "The Beatles".into(),
                title: "Help!".into(),
                duration_seconds: Some(139),
                r: Default::default(),
            }),
            line: Some(line.into()),
            ..held(
                "help",
                AcquisitionOrigin::AlbumHeart,
                WaitingFor::NotYetProven,
                &["brandon"],
            )
        });
    }
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::NotYetProven);
    let rig = rig(gates.clone(), None, Arc::new(SoulseekHoldStore::new(Some(&file))));
    Arc::new(SoulseekHoldResumer::new(
        rig.holds.clone(),
        rig.coordinator.clone(),
    ))
    .run(CancellationToken::new())
    .await
    .expect("ran");

    until(|| !gates.built.lock().is_empty()).await;
    assert_eq!(
        gates.built.lock().clone(),
        [("brandon".to_string(), AcquisitionOrigin::AlbumHeart)],
        "asked again once"
    );
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    let record = rig
        .holds
        .snapshot()
        .into_iter()
        .find(|record| record.album_track.is_some())
        .expect("still held, as nothing proved it");
    assert_eq!(users(&record), ["brandon"]);
    assert!(rig.tracker.is_held("soulseek", "help"));
}

// ---- Already in your library -----------------------------------------------------------------

/// `HeardGates` whose verdict on a library copy is `passes`, each copy judged noted.
struct OwnedGates {
    heard: Arc<HeardGates>,
    passes: bool,
    judged: Mutex<Vec<String>>,
}

#[async_trait]
impl HeartGates for OwnedGates {
    async fn build(&self, provider: &str, id: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        self.heard.build(provider, id, holder, origin).await
    }

    async fn owned_passes(&self, path: &str, _: &HeartGate) -> bool {
        self.judged.lock().push(path.to_string());
        self.passes
    }
}

/// A coordinator whose library holds a copy of Help! (the 2015 mix, though nothing says so), at a
/// temp file, with the owned check on, Soulseek as the heart's source and DownloadOnPlay on.
fn owned_rig(gates: Arc<dyn HeartGates>) -> (Rig, tempfile::TempDir, String) {
    owned_rig_with(gates, None)
}

/// [`owned_rig`] whose library's own records of the files it placed are `library`'s.
fn owned_rig_with(
    gates: Arc<dyn HeartGates>,
    library: Option<Arc<dyn crate::services::local::ILocalLibraryService>>,
) -> (Rig, tempfile::TempDir, String) {
    use crate::services::common::test_fakes::FakeMetadata;
    use crate::services::library::library_ownership::{Candidate, search_from};
    use crate::services::library::{HeartOwnership, LibraryOwnership};
    use futures::FutureExt;

    let dir = tempfile::tempdir().expect("a temp dir");
    let owned_file = dir.path().join("Help!.flac").to_string_lossy().into_owned();
    std::fs::write(&owned_file, b"the 2015 mix").expect("written");
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            skip_owned_songs: true,
            download_on_play: true,
            heart_download_sources: vec![HeartDownloadStep {
                source: HeartDownloadSource::Soulseek,
                song_enabled: Some(true),
                album_enabled: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    }));
    let metadata = Arc::new(FakeMetadata::default());
    metadata.songs.lock().insert(
        ("soulseek".into(), "help".into()),
        octo_core::models::domain::Song {
            title: "Help!".into(),
            artist: "The Beatles".into(),
            album: "Help!".into(),
            duration: Some(139),
            ..Default::default()
        },
    );
    let ownership = LibraryOwnership::new(
        settings.clone(),
        None,
        Arc::new(crate::services::local::test_support::FakeLocalLibrary::default()),
    );
    ownership.set_search(search_from(Some(vec![Candidate {
        id: "nd-help".into(),
        artist: "The Beatles".into(),
        title: "Help!".into(),
        album: Some("Help!".into()),
        duration: Some(139),
        suffix: "flac".into(),
        bit_rate: 900,
        isrcs: Vec::new(),
    }])));
    let path = owned_file.clone();
    ownership.set_resolve(Arc::new(move |_| {
        let path = path.clone();
        async move { Ok(Some(path)) }.boxed()
    }));
    let owned = Arc::new(HeartOwnership::new(
        Arc::new(ownership),
        metadata,
        settings.clone(),
        None,
        None,
    ));
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let lidarr = Arc::new(FakeLidarr::default());
    let holds = memory_holds();
    let coordinator = HeartAcquisitionCoordinator::new(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            holds: Some(holds.clone()),
            gates: Some(gates),
            owned: Some(owned),
            library,
            ..Default::default()
        },
    );
    (
        Rig {
            coordinator,
            queue,
            holds,
            tracker,
            lidarr,
        },
        dir,
        owned_file,
    )
}

fn owned_gates(passes: bool) -> Arc<OwnedGates> {
    let heard = HeardGates::reading(HeardReading::RStays { tag: None });
    heard.heard("brandon", heard_video());
    Arc::new(OwnedGates {
        heard,
        passes,
        judged: Mutex::new(Vec::new()),
    })
}

/// The 2015 Help! owned and the 2009 mix heard: nothing proves the owned copy is the version
/// heard, so it is left untouched, never counted as the heart's (not favorited), and the heard
/// audio is kept beside it, the row saying so.
#[tokio::test]
async fn a_heart_over_an_owned_copy_nothing_proves_keeps_the_heard_audio_beside_it() {
    let gates = owned_gates(false);
    let (rig, _dir, owned_file) = owned_rig(gates.clone());
    rig.tracker.begin(
        "soulseek",
        "help",
        None,
        Some("brandon"),
        None,
        Some("Help!"),
        None,
    );
    let (taken, stop) = work(&rig, sources_fail_heard_lands());

    let outcome = heart_of(&rig, "brandon").await;

    assert_eq!(outcome, TrackOutcome::KeptAsHeard("/music/help.m4a".into()));
    assert_eq!(
        gates.judged.lock().as_slice(),
        std::slice::from_ref(&owned_file),
        "the owned copy was judged"
    );
    assert_eq!(std::fs::read(&owned_file).expect("still there"), b"the 2015 mix");
    assert_eq!(taken.lock().len(), 2, "the source, then the heard audio");
    assert_eq!(
        rig.tracker.note_of("soulseek", "help").as_deref(),
        Some(KEPT_BESIDE_OWNED)
    );
    let row = rig.tracker.detail("soulseek:help", None).expect("the row");
    assert_ne!(
        row.library_id.as_deref(),
        Some("nd-help"),
        "never closed on the owned copy"
    );
    stop.cancel();
}

/// An owned copy the heart's own gate proves is the heart's: nothing is downloaded.
#[tokio::test]
async fn a_heart_over_an_owned_copy_its_gate_proves_downloads_nothing() {
    let (rig, _dir, _) = owned_rig(owned_gates(true));
    let (taken, stop) = work(&rig, sources_fail_heard_lands());

    assert_eq!(heart_of(&rig, "brandon").await, TrackOutcome::AlreadyThere);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(taken.lock().is_empty());
    stop.cancel();
}

/// A play over an owned copy downloads nothing, its row saying nothing checked it is the version
/// heard.
#[tokio::test]
async fn a_play_over_an_owned_copy_downloads_nothing() {
    let (rig, _dir, _) = owned_rig(owned_gates(false));
    let (taken, stop) = work(&rig, Arc::new(|_: &AcquisitionRequest, _| None));

    rig.coordinator.queue_play_heard(
        "soulseek",
        "help",
        Some("brandon"),
        Some("c"),
        Some("brandon"),
        Some(heard_video()),
    );
    until(|| {
        rig.tracker
            .detail("soulseek:help", None)
            .is_some_and(|row| row.state == AcquisitionState::Done)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(taken.lock().is_empty(), "nothing downloaded");
    assert_eq!(
        rig.tracker.note_of("soulseek", "help").as_deref(),
        Some(PLAY_OVER_OWNED)
    );
    stop.cancel();
}

/// A DownloadOnPlay play whose download nothing could prove keeps what it played instead: the
/// heard video's own stream, judged by its identity alone.
#[tokio::test]
async fn a_download_on_play_play_whose_download_is_not_proven_keeps_the_heard_audio() {
    let rig = play_rig(true, HeardGates::reading(HeardReading::RStays { tag: None }));
    let (taken, stop) = work(
        &rig,
        Arc::new(|_: &AcquisitionRequest, n| (n == 0).then(|| "not proven".to_string())),
    );
    rig.coordinator.queue_play_heard(
        "soulseek",
        "played",
        Some("brandon"),
        Some("c"),
        Some("brandon"),
        Some(heard_video()),
    );
    until(|| taken.lock().len() == 2).await;
    let kept = taken.lock()[1].clone();
    assert_eq!(kept.origin, Some(AcquisitionOrigin::Play));
    assert_eq!(kept.source_override, Some(DownloadSource::YouTube));
    let gates = kept.gates();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0].reading, HeardReading::OnlyHeardOrMaster);
    assert_eq!(heard_of(&gates[0]), Some(heard_video().source));
    until(|| kept.completion.is_completed()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(taken.lock().len(), 2, "nothing else is fetched");
    stop.cancel();
}

/// A DownloadOnPlay play whose heard lookup AcoustID answers busy fetches no download from any
/// source, and keeps the very audio it played.
#[tokio::test]
async fn a_download_on_play_play_with_acoust_id_busy_keeps_the_heard_audio_and_no_download() {
    let gates = HeardGates::reading(HeardReading::Busy);
    *gates.hold.lock() = Some(WaitingFor::AcoustId);
    let rig = play_rig(true, gates);
    let (taken, stop) = work(&rig, Arc::new(|_: &AcquisitionRequest, _| None));
    rig.coordinator.queue_play_heard(
        "soulseek",
        "played",
        Some("brandon"),
        Some("c"),
        Some("brandon"),
        Some(heard_video()),
    );
    until(|| taken.lock().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let taken = taken.lock().clone();
    assert_eq!(taken.len(), 1, "only the heard audio");
    assert_eq!(taken[0].source_override, Some(DownloadSource::YouTube));
    assert_eq!(taken[0].gates()[0].reading, HeardReading::OnlyHeardOrMaster);
    assert!(rig.lidarr.origins.lock().is_empty(), "no Lidarr album");
    assert!(rig.holds.snapshot().is_empty(), "a play never holds");
    stop.cancel();
}

/// AcoustID working again runs every hold the clip check left through the heard answer, and
/// AcoustID turning off runs every hold waiting for it through the catalog's clip; each leaves the
/// other wait's holds alone.
#[tokio::test]
async fn acoust_id_turning_on_or_off_runs_the_holds_that_wait_on_it_again() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("anna", WaitingFor::AcoustIdOff);
    gates.hold("brandon", WaitingFor::AcoustId);
    let holds = memory_holds();
    holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::AcoustIdOff,
        &["anna"],
    ));
    holds.hold(held(
        "yesterday",
        AcquisitionOrigin::Heart,
        WaitingFor::AcoustId,
        &["brandon"],
    ));
    let rig = rig(gates.clone(), None, holds);

    rig.coordinator.resume_waiting(WaitingFor::AcoustIdOff);
    until(|| rig.coordinator.resuming.lock().is_empty() && !gates.built.lock().is_empty()).await;
    let built: Vec<String> = gates.built.lock().iter().map(|(user, _)| user.clone()).collect();
    assert_eq!(built, ["anna"]);

    rig.coordinator.resume_waiting(WaitingFor::AcoustId);
    until(|| rig.coordinator.resuming.lock().is_empty() && gates.built.lock().len() == 2).await;
    let built: Vec<String> = gates.built.lock().iter().map(|(user, _)| user.clone()).collect();
    assert_eq!(built, ["anna", "brandon"]);
}

/// A held song streamed again brings its holder's new heard source to the hold, which runs again
/// with it. Another person's play only checks the hold again with what its holder heard: their
/// own heard source is never taken into it.
#[tokio::test]
async fn a_held_song_streamed_again_runs_its_hold_with_what_was_heard_now() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::NotYetProven);
    let holds = memory_holds();
    holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::NotYetProven,
        &["brandon"],
    ));
    let rig = rig(gates.clone(), None, holds);
    let played = HeardRecord {
        heard_at: Utc::now(),
        source: octo_core::fingerprint::heart_gate::HeardSource::Video {
            id: "AAAAAAAAAA1".into(),
            size: Some(1000),
            content_type: "audio/mp4".into(),
            itag: None,
        },
        sample_id: None,
    };
    rig.coordinator.heard_again("help", "anna", &played);
    until(|| !gates.built.lock().is_empty()).await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    assert_eq!(
        gates.built.lock().clone(),
        [("brandon".to_string(), AcquisitionOrigin::Heart)],
        "anna's play checks brandon's hold again, once"
    );
    let record = rig
        .holds
        .get("Track:soulseek:help:NotYetProven")
        .expect("held again");
    assert_eq!(users(&record), ["brandon"]);
    assert_eq!(
        record.holders[0].heard, None,
        "anna's heard source is never taken"
    );

    rig.coordinator.heard_again("help", "Brandon", &played);
    until(|| gates.built.lock().len() == 2).await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    let record = rig
        .holds
        .get("Track:soulseek:help:NotYetProven")
        .expect("held again");
    assert_eq!(
        record.holders[0].heard.as_ref().map(|heard| heard.source.clone()),
        Some(played.source)
    );
}

// ---- Kept files checked again ------------------------------------------------------------------

/// Octo's kept-as-heard file of `song`, its print kept with it, checked again against an AcoustID
/// that does not know it; the AcoustID that counts the lookups.
async fn kept_file_checks(
    dir: &std::path::Path,
    song: &str,
) -> (
    Arc<crate::services::library::upgrade_queue::KeptRechecks>,
    MockServer,
) {
    use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
    use crate::services::library::library_action_test_support::RecordingLibrary;
    use crate::services::local::{KeptHeart, LocalSongMapping};

    let acoust_id = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"status":"ok","results":[]}"#, "application/json"),
        )
        .mount(&acoust_id)
        .await;
    let path = dir.join("kept.m4a");
    std::fs::write(&path, b"the audio that played").expect("written");
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(1), Duration::from_millis(1))
            .with_jitter(|| 1.0),
    );
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::with_base_url(
            limiter,
            Url::parse(&format!("{}/", acoust_id.uri())).expect("parses"),
        )),
        settings.clone(),
        Some("octo-key"),
    ));
    let library = Arc::new(RecordingLibrary::default());
    library.mappings.lock().push(LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: song.into(),
        local_path: path.to_string_lossy().replace('\\', "/"),
        heart: Some(KeptHeart {
            users: vec!["brandon".into()],
            as_heard: true,
            print: Some("AQADtEqk".into()),
            seconds: Some(139),
            ..Default::default()
        }),
        ..Default::default()
    });
    let kept = Arc::new(crate::services::library::upgrade_queue::KeptRechecks::new(
        library,
        client,
        settings,
        Arc::new(ExternalIdRegistry::in_memory()),
    ));
    (kept, acoust_id)
}

/// A heart whose kept copy a check found to be another song holds "not yet proven" with the line a
/// stream that stood in with another song gets, its finished row held again with that line; a
/// second such check joins the same hold.
#[tokio::test]
async fn a_heart_whose_kept_copy_is_another_song_holds_for_a_play_with_its_line() {
    let rig = rig_with(&[HeartDownloadSource::Soulseek], Arc::new(ListedGates::default()));
    rig.tracker.begin(
        "soulseek",
        "help",
        None,
        Some("brandon"),
        Some("The Beatles"),
        Some("Help!"),
        None,
    );
    rig.tracker.done_line("soulseek", "help", KEPT_AS_HEARD);
    rig.tracker.complete("soulseek", "help", None);

    for _ in 0..2 {
        rig.coordinator
            .hold_not_yet_proven("soulseek", "help", &["brandon".into()], ANOTHER_SONG_PLAYED);
    }

    let records = rig.holds.snapshot();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].key(), "Track:soulseek:help:NotYetProven");
    assert_eq!(users(&records[0]), ["brandon"]);
    assert_eq!(records[0].line.as_deref(), Some(ANOTHER_SONG_PLAYED));
    assert!(rig.tracker.is_held("soulseek", "help"));
    assert_eq!(
        rig.tracker.note_of("soulseek", "help").as_deref(),
        Some(ANOTHER_SONG_PLAYED)
    );
}

/// How many lookups `acoust_id` has had once `wanted` arrived, or after a short wait for none.
async fn lookups_settle(acoust_id: &MockServer, wanted: usize) -> usize {
    for _ in 0..200 {
        let seen = acoust_id.received_requests().await.unwrap_or_default().len();
        if wanted > 0 && seen >= wanted {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    acoust_id.received_requests().await.unwrap_or_default().len()
}

/// Lands every request, each gate on it accepting the file by `by`, as the download's check point
/// writes it.
fn accept_by(rig: &Rig, by: octo_core::fingerprint::heart_gate::Acceptance) -> (Taken, CancellationToken) {
    let taken: Taken = Arc::default();
    let stop = CancellationToken::new();
    let (queue, seen, token) = (rig.queue.clone(), taken.clone(), stop.clone());
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            seen.lock().push(request.clone());
            let ledger = request.completion.ledger().clone();
            for gate in request.gates() {
                ledger.record_accepted(&gate.user, gate.origin, by);
            }
            queue.release(&request);
            request
                .completion
                .try_set_result(format!("/music/{}.flac", request.external_id));
        }
    });
    (taken, stop)
}

/// Only a heart AcoustID's clear reading accepted checks the kept file checked longest ago; a heart
/// the master heard or the very audio heard proved checks none.
#[tokio::test]
async fn only_a_clear_readings_proof_checks_a_kept_file_again() {
    use octo_core::fingerprint::heart_gate::Acceptance;

    for (by, checks) in [
        (Acceptance::Sound, 1),
        (Acceptance::HeardMaster, 0),
        (Acceptance::HeardAudio, 0),
    ] {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (kept, acoust_id) = kept_file_checks(dir.path(), "yesterday").await;
        let gates = HeardGates::reading(HeardReading::RStays { tag: None });
        gates.heard("brandon", heard_video());
        let rig = rig_checking(&[HeartDownloadSource::Soulseek], gates, Some(kept));
        let (_taken, stop) = accept_by(&rig, by);

        assert_eq!(
            heart_of(&rig, "brandon").await,
            TrackOutcome::Landed("/music/help.flac".into()),
            "{by:?}"
        );
        assert_eq!(
            lookups_settle(&acoust_id, checks).await,
            checks,
            "{by:?}: the kept file is checked again only after a clear reading's proof"
        );
        stop.cancel();
    }
}

/// The person's own Retry on a downloads row (the apps star the song again) is one of the events a
/// song's kept files and holds are checked on: their kept-as-heard file of it is checked again with
/// one lookup, and their heart held "not yet proven" runs again.
#[tokio::test]
async fn the_persons_own_retry_checks_their_kept_file_and_their_held_heart_again() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (kept, acoust_id) = kept_file_checks(dir.path(), "help").await;
    let gates = Arc::new(HeardGates::default());
    *gates.hold.lock() = Some(WaitingFor::NotYetProven);
    let rig = rig_checking(&[HeartDownloadSource::Soulseek], gates, Some(kept));
    rig.holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::NotYetProven,
        &["brandon"],
    ));
    let (taken, stop) = work(&rig, sources_fail_heard_lands());

    // Retry: the same person's heart of the same song.
    assert_eq!(
        heart_of(&rig, "brandon").await,
        TrackOutcome::Held(WaitingFor::NotYetProven),
        "it joins the hold it already waits in"
    );

    assert_eq!(
        lookups_settle(&acoust_id, 1).await,
        1,
        "the kept file's print is looked up once"
    );
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    let record = rig
        .holds
        .get("Track:soulseek:help:NotYetProven")
        .expect("still held, nothing proven");
    assert_eq!(users(&record), ["brandon"]);
    assert!(taken.lock().is_empty(), "its gate holds again before any source");
    stop.cancel();
}

// ---- Joiners, runs again, rows, routes and what a run holds -----------------------------------

/// A heart for a song an import is already fetching never rides on the import, whose checks never
/// judge her gate: it waits behind it as her own request, which carries the import's file for her
/// own gate to judge first, and what lands for her is what her own request lands.
#[tokio::test]
async fn a_heart_for_a_song_an_import_fetches_waits_behind_it_as_its_own() {
    let rig = rig(Arc::new(ListedGates::default()), None, memory_holds());
    let import = rig.queue.enqueue_holding(
        Some(AcquisitionOrigin::Import),
        Vec::new(),
        HeldFiles::default(),
        &AcquisitionTarget::Shared,
        false,
        "soulseek",
        "help",
        true,
        false,
        true,
        None,
        true,
        Some("brandon"),
        false,
        None,
    );
    let taken: Taken = Arc::default();
    let stop = CancellationToken::new();
    let (queue, seen, token) = (rig.queue.clone(), taken.clone(), stop.clone());
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            // The import runs while the heart is queued behind it.
            while request.gates().is_empty() && queue.waiting_behind("soulseek", "help") == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            for gate in request.gates() {
                request
                    .completion
                    .ledger()
                    .record(&gate.user, gate.origin, GateOutcome::Accepted);
            }
            seen.lock().push(request.clone());
            queue.release_landed(&request, Some("/music/help.flac"));
            request.completion.try_set_result("/music/help.flac");
        }
    });
    let outcome = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            "help",
            Some("anna"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert_eq!(outcome, TrackOutcome::Landed("/music/help.flac".into()));
    assert!(import.wait().await.is_ok());
    let taken = taken.lock().clone();
    assert_eq!(taken.len(), 2, "her own request ran");
    assert!(taken[0].gates().is_empty(), "her gate never rode on the import");
    let hers: Vec<String> = taken[1].gates().into_iter().map(|gate| gate.user).collect();
    assert_eq!(hers, ["anna"]);
    assert_eq!(taken[1].place().landed, ["/music/help.flac"]);
    stop.cancel();
}

/// Gates that are ready once `open` says so: a build waits for it.
struct WaitingGates {
    open: tokio::sync::watch::Receiver<bool>,
    built: Mutex<Vec<String>>,
}

#[async_trait]
impl HeartGates for WaitingGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        self.built.lock().push(holder.user.clone());
        let mut open = self.open.clone();
        let _ = open.wait_for(|open| *open).await;
        GateStart::Ready(gate(holder, origin))
    }
}

/// A heart that joins a hold while the hold runs again is not lost: the run ends with the first
/// person's file, then runs once more for the one who joined, whose own gate passes their file.
#[tokio::test]
async fn a_heart_joining_a_hold_while_it_runs_is_gated_and_settled_by_one_more_run() {
    let (open, opened) = tokio::sync::watch::channel(false);
    let gates = Arc::new(WaitingGates {
        open: opened,
        built: Mutex::new(Vec::new()),
    });
    let holds = memory_holds();
    holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::AcoustId,
        &["brandon"],
    ));
    let rig = rig(gates.clone(), None, holds);
    let (landed, stop) = land_everything(&rig.queue);
    let key = "Track:soulseek:help:AcoustId";
    rig.coordinator.resume_held(key);
    until(|| gates.built.lock().len() == 1).await;
    assert_eq!(
        rig.coordinator
            .acquire_track_outcome(
                "soulseek",
                "help",
                Some("anna"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
            .expect("ran"),
        TrackOutcome::Held(WaitingFor::AcoustId)
    );
    open.send_replace(true);
    until(|| rig.holds.snapshot().is_empty()).await;
    assert_eq!(*gates.built.lock(), ["brandon", "anna"]);
    assert_eq!(landed.lock().len(), 2);
    stop.cancel();
}

/// A check of a held heart run to its end while another run of it is going (Octo's start, an
/// answer MusicBrainz gave up on) waits for that run, and the one it owes, to end first.
#[tokio::test]
async fn a_recheck_of_a_hold_that_runs_waits_for_that_run_to_end() {
    let (open, opened) = tokio::sync::watch::channel(false);
    let gates = Arc::new(WaitingGates {
        open: opened,
        built: Mutex::new(Vec::new()),
    });
    let holds = memory_holds();
    holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::NotYetProven,
        &["brandon"],
    ));
    let rig = rig(gates.clone(), None, holds);
    let (_landed, stop) = land_everything(&rig.queue);
    let key = "Track:soulseek:help:NotYetProven";
    rig.coordinator.resume_held(key);
    until(|| gates.built.lock().len() == 1).await;
    let coordinator = rig.coordinator.clone();
    let recheck = tokio::spawn(async move { coordinator.recheck_held(key).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!recheck.is_finished(), "it waits for the run going");
    open.send_replace(true);
    tokio::time::timeout(Duration::from_secs(10), recheck)
        .await
        .expect("the recheck ends once the run has")
        .expect("ran");
    assert!(rig.coordinator.resuming.lock().is_empty());
    assert!(rig.holds.snapshot().is_empty(), "the run landed it");
    stop.cancel();
}

/// A wake of a hold that comes while one of its people's Keep anyway is carried out is not lost:
/// once the keep has landed, the hold runs again for whoever still waits in it.
#[tokio::test]
async fn a_wake_while_a_keep_is_carried_out_runs_the_hold_once_the_keep_has_landed() {
    let holds = memory_holds();
    let mut record = held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::AcoustIdOff,
        &["brandon", "anna"],
    );
    for holder in &mut record.holders {
        holder.heard = Some(heard_video());
    }
    let key = holds.hold(record).key();
    let gates = Arc::new(ListedGates::default());
    gates.hold("anna", WaitingFor::AcoustIdOff);
    let rig = rig(gates.clone(), None, holds);
    let (land, landing) = tokio::sync::watch::channel(false);
    let (queue, stop) = (rig.queue.clone(), CancellationToken::new());
    let token = stop.clone();
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            let mut go = landing.clone();
            let _ = go.wait_for(|go| *go).await;
            for gate in request.gates() {
                request
                    .completion
                    .ledger()
                    .record(&gate.user, gate.origin, GateOutcome::Accepted);
            }
            queue.release(&request);
            request.completion.try_set_result("/music/help.m4a");
        }
    });
    assert_eq!(
        rig.coordinator.keep_anyway("soulseek:help", "brandon"),
        KeepAnyway::Keeping
    );
    until(|| rig.queue.is_in_flight("soulseek", "help")).await;
    // A wake (AcoustID's key working again) while the keep is carried out.
    rig.coordinator.resume_held(&key);
    assert!(gates.built.lock().is_empty(), "the keep has the hold");
    land.send_replace(true);
    until(|| gates.built.lock().iter().any(|(user, _)| user == "anna")).await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    assert_eq!(users(&rig.holds.get(&key).expect("anna waits")), ["anna"]);
    stop.cancel();
}

/// A heart joining a held song takes only its person into the hold: the hold keeps its held files,
/// the labels of those Lidarr brought and the keeps asked for, and its run owes one more pass.
#[tokio::test]
async fn a_heart_joining_a_hold_keeps_its_files_labels_and_keeps() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("help.flac").to_string_lossy().into_owned();
    std::fs::write(&file, b"x").expect("written");
    let brought = HeldLidarrFile {
        path: file.clone(),
        delivered: Some("/downloads/Help!/04 - Help!.flac".into()),
    };
    let holds = memory_holds();
    let mut record = held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::AcoustIdOff,
        &["brandon"],
    );
    record.held_files = vec![file.clone()];
    record.lidarr_files = vec![brought.clone()];
    record.keep_requested = vec!["brandon".into()];
    let key = holds.hold(record).key();
    let rig = rig(Arc::new(ListedGates::default()), None, holds);
    // A run of the hold is going, so the join wakes it rather than running it here.
    rig.coordinator.resuming.lock().insert(key.clone());
    assert_eq!(
        rig.coordinator
            .acquire_track_outcome(
                "soulseek",
                "help",
                Some("anna"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
            .expect("ran"),
        TrackOutcome::Held(WaitingFor::AcoustIdOff)
    );
    let joined = rig.holds.get(&key).expect("held");
    assert_eq!(users(&joined), ["brandon", "anna"]);
    assert_eq!(joined.held_files, [file]);
    assert_eq!(joined.lidarr_files, [brought]);
    assert_eq!(joined.keep_requested, ["brandon"]);
    assert!(rig.coordinator.again.lock().contains(&key));
}

/// Gates that are ready, and pass a landed file nobody judged for them, noting each such file.
#[derive(Default)]
struct PassingGates {
    judged: Mutex<Vec<String>>,
}

#[async_trait]
impl HeartGates for PassingGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        GateStart::Ready(gate(holder, origin))
    }

    async fn judge_landed(
        &self,
        path: &str,
        _: LabelSource,
        _: &str,
        gates: &[HeartGate],
    ) -> Vec<GateVerdict> {
        self.judged.lock().push(path.to_string());
        gates
            .iter()
            .map(|_| GateVerdict::Accept {
                by: Acceptance::Sound,
                tags: octo_core::fingerprint::heart_gate::TagClaim::AsHeard,
            })
            .collect()
    }
}

/// A heart whose file landed with nobody judging it for them (they joined another acquisition)
/// has that file judged by their own gate first: passed, it is theirs, and nothing is fetched
/// again for them.
#[tokio::test]
async fn a_heart_counts_a_landed_file_nobody_judged_once_its_own_gate_passes_it() {
    let gates = Arc::new(PassingGates::default());
    let rig = rig(gates.clone(), None, memory_holds());
    let taken: Taken = Arc::default();
    let (queue, seen, stop) = (rig.queue.clone(), taken.clone(), CancellationToken::new());
    let token = stop.clone();
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            // The file lands for someone else's checks; nothing judged it for the heart.
            seen.lock().push(request.clone());
            queue.release(&request);
            request.completion.try_set_result("/music/help.flac");
        }
    });
    let outcome = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            "help",
            Some("anna"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert_eq!(outcome, TrackOutcome::Landed("/music/help.flac".into()));
    assert_eq!(taken.lock().len(), 1, "nothing queued again");
    assert_eq!(*gates.judged.lock(), ["/music/help.flac"]);
    stop.cancel();
}

/// After a file lands for one person of a hold, the row of the person still waiting in it is held
/// again with the hold's line, though the landing closed it as done.
#[tokio::test]
async fn the_row_of_a_person_still_held_is_held_again_after_anothers_file_lands() {
    let holds = memory_holds();
    holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::NotYetProven,
        &["brandon", "anna"],
    ));
    let rig = rig(Arc::new(ListedGates::default()), None, holds);
    let (queue, tracker, stop) = (rig.queue.clone(), rig.tracker.clone(), CancellationToken::new());
    let token = stop.clone();
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            let ledger = request.completion.ledger().clone();
            for gate in request.gates() {
                let outcome = if gate.user == "brandon" {
                    GateOutcome::Accepted
                } else {
                    GateOutcome::NotAccepted("another mix".into())
                };
                ledger.record(&gate.user, gate.origin, outcome);
            }
            // The download's placement closes the row as done.
            tracker.complete(&request.provider, &request.external_id, None);
            queue.release(&request);
            request.completion.try_set_result("/music/help.flac");
        }
    });
    let key = "Track:soulseek:help:NotYetProven";
    // The row is held with its line, as Octo's start leaves it.
    rig.coordinator.hold_row_for(&rig.holds.get(key).expect("held"));
    rig.coordinator.recheck_held(key).await;
    assert_eq!(users(&rig.holds.get(key).expect("anna stays held")), ["anna"]);
    let row = rig
        .tracker
        .all()
        .into_iter()
        .find(|row| row.external_id == "help")
        .expect("a row");
    assert_ne!(row.state, AcquisitionState::Done, "{row:?}");
    assert!(rig.tracker.is_held("soulseek", "help"));
    // Her own chain ran on to its end, and she waits with the line her hold says now.
    let line = rig.holds.get(key).and_then(|record| record.line);
    assert!(line.is_some());
    assert_eq!(rig.tracker.note_of("soulseek", "help"), line);
    stop.cancel();
}

/// A held album track keeps a person by each heart of theirs that holds them in it: taking back
/// the song heart leaves them held by the album heart, and the other way round; the hold ends
/// with the last.
#[tokio::test]
async fn a_song_heart_and_an_album_heart_of_one_held_track_each_keep_the_person_in_it() {
    for song_first in [true, false] {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("help.flac");
        std::fs::write(&file, b"x").expect("written");
        let holds = memory_holds();
        let mut record = held(
            "help",
            AcquisitionOrigin::AlbumHeart,
            WaitingFor::NotYetProven,
            &["brandon"],
        );
        record.album_track = Some(crate::services::common::soulseek_hold_store::HeldAlbumTrack {
            album_key: "soulseek:album-1".into(),
            ..Default::default()
        });
        record.held_files = vec![file.to_string_lossy().into_owned()];
        holds.hold(record);
        let gates = Arc::new(ListedGates::default());
        gates.hold("brandon", WaitingFor::NotYetProven);
        let rig = rig(gates, None, holds);
        // Their song heart joins the held track.
        rig.coordinator
            .acquire_track_outcome(
                "soulseek",
                "help",
                Some("brandon"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
            .expect("ran");
        let key = "Track:soulseek:help:NotYetProven";
        until(|| !rig.coordinator.resuming.lock().contains(key)).await;
        if song_first {
            assert_eq!(rig.coordinator.leave_holds("soulseek", "help", "brandon"), 1);
            assert_eq!(
                users(&rig.holds.get(key).expect("the album heart keeps it")),
                ["brandon"]
            );
            assert_eq!(
                rig.coordinator
                    .leave_album_holds("soulseek", "album-1", "brandon"),
                1
            );
        } else {
            assert_eq!(
                rig.coordinator
                    .leave_album_holds("soulseek", "album-1", "brandon"),
                1
            );
            assert_eq!(
                users(&rig.holds.get(key).expect("the song heart keeps it")),
                ["brandon"]
            );
            assert_eq!(rig.coordinator.leave_holds("soulseek", "help", "brandon"), 1);
        }
        assert!(rig.holds.snapshot().is_empty(), "song first: {song_first}");
    }
}

/// A coordinator over `holds` with `gates` and the album gates of the album chains that run.
fn album_rig(gates: Arc<dyn HeartGates>, holds: Arc<SoulseekHoldStore>) -> (Rig, Arc<AlbumGates>) {
    let album_gates = Arc::new(AlbumGates::new());
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let lidarr = Arc::new(FakeLidarr::default());
    let coordinator = HeartAcquisitionCoordinator::new(
        youtube_only(),
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            holds: Some(holds.clone()),
            gates: Some(gates),
            album_gates: Some(album_gates.clone()),
            ..Default::default()
        },
    );
    (
        Rig {
            coordinator,
            queue,
            holds,
            tracker,
            lidarr,
        },
        album_gates,
    )
}

/// "Help!" as the album "album-1" lists it.
fn help_on_the_album() -> AlbumGateTrack {
    AlbumGateTrack {
        catalog_track_id: "help".into(),
        title: "Help!".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    }
}

/// A song heart held first and then the same person's album heart, whose walk held the track for a
/// play, are one hold: the song's hold becomes the album's held track, the person in it by both
/// hearts. Taking the album heart back leaves them held by the song, the hold the song's own again
/// and off the album's row; taking the song heart back then ends it.
#[tokio::test]
async fn a_song_heart_held_first_then_its_album_heart_each_keep_the_person_in_it() {
    use crate::services::common::album_gates::tracks_wait_line;
    use crate::services::common::soulseek_hold_store::HoldRoute;

    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("help.flac").to_string_lossy().into_owned();
    std::fs::write(&file, b"x").expect("written");
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::NotYetProven);
    let (rig, album_gates) = album_rig(gates, memory_holds());
    assert_eq!(
        rig.coordinator
            .acquire_track_outcome(
                "soulseek",
                "help",
                Some("brandon"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
            .expect("ran"),
        TrackOutcome::Held(WaitingFor::NotYetProven)
    );
    let key = "Track:soulseek:help:NotYetProven";
    assert!(rig.holds.get(key).expect("held").album_track.is_none());

    // The album heart's walk held the track for a play.
    let _album = album_gates.register(
        "soulseek",
        "album-1",
        AlbumGate {
            user: "brandon".into(),
            origin: AcquisitionOrigin::AlbumHeart,
            tracks: vec![help_on_the_album()],
        },
        Vec::<String>::new(),
    );
    let held = AcquisitionHeld {
        waiting_for: WaitingFor::NotYetProven,
        line: tracks_wait_line(1),
        files: vec![file.clone()],
        lidarr: Vec::new(),
        users: vec!["brandon".into()],
    };
    rig.coordinator.file_hold(
        "soulseek",
        "help",
        Some("brandon"),
        AcquisitionOrigin::AlbumHeart,
        WaitingFor::NotYetProven,
        vec![holder("brandon")],
        &held.line,
        Some(&held),
        None,
        &mut Vec::new(),
    );
    let both = rig.holds.get(key).expect("one hold");
    assert_eq!(
        both.album_track.as_ref().map(|track| track.album_key.as_str()),
        Some("soulseek:album-1")
    );
    assert_eq!(
        both.holders[0].routes_in(&both),
        [HoldRoute::Song, HoldRoute::Album]
    );
    assert_eq!(both.held_files, std::slice::from_ref(&file));

    assert_eq!(
        rig.coordinator
            .leave_album_holds("soulseek", "album-1", "brandon"),
        1
    );
    let song = rig.holds.get(key).expect("the song heart keeps it");
    assert_eq!(users(&song), ["brandon"]);
    assert!(song.album_track.is_none(), "off the album's row");
    assert!(!rig.tracker.is_held("soulseek", "album-1"));
    assert_eq!(rig.coordinator.leave_holds("soulseek", "help", "brandon"), 1);
    assert!(rig.holds.snapshot().is_empty());
}

/// Another person's album heart finds a song's own hold of one of its tracks holding a file: they
/// join it by their album heart, it becomes the album's held track, and no walk of the album
/// fetches the track again. The song's person stays in it by their song.
#[tokio::test]
async fn an_album_heart_joins_a_songs_own_hold_of_its_track_that_holds_a_file() {
    use crate::services::common::soulseek_hold_store::HoldRoute;

    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("help.flac").to_string_lossy().into_owned();
    std::fs::write(&file, b"x").expect("written");
    let holds = memory_holds();
    let mut record = held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::NotYetProven,
        &["brandon"],
    );
    record.held_files = vec![file];
    let key = holds.hold(record).key();
    let (rig, album_gates) = album_rig(Arc::new(ListedGates::default()), holds);
    let album = Album {
        id: "album-1".into(),
        title: "Help!".into(),
        artist: "The Beatles".into(),
        songs: vec![octo_core::models::domain::Song {
            external_provider: Some("soulseek".into()),
            external_id: Some("help".into()),
            title: "Help!".into(),
            artist: "The Beatles".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let _gate = rig
        .coordinator
        .album_gate_up(
            "soulseek",
            "album-1",
            &album,
            "anna",
            AcquisitionOrigin::AlbumHeart,
        )
        .await
        .expect("ready")
        .expect("a gate");

    let joined = rig.holds.get(&key).expect("held");
    assert_eq!(users(&joined), ["brandon", "anna"]);
    assert_eq!(
        joined.album_track.as_ref().map(|track| track.album_key.as_str()),
        Some("soulseek:album-1")
    );
    assert_eq!(joined.holders[0].routes_in(&joined), [HoldRoute::Song]);
    assert_eq!(joined.holders[1].routes_in(&joined), [HoldRoute::Album]);
    assert!(album_gates.settled("soulseek", "album-1", &["anna".to_string()], "help"));
}

/// An album heart's walk held a track for MusicBrainz before any file of it was fetched: once
/// MusicBrainz answers, the resumed hold fetches the track for the album heart, and the album's row
/// lets go of it.
#[tokio::test]
async fn an_album_track_held_before_any_file_was_fetched_is_fetched_when_it_resumes() {
    let (rig, album_gates) = album_rig(Arc::new(ListedGates::default()), memory_holds());
    let _album = album_gates.register(
        "soulseek",
        "album-1",
        AlbumGate {
            user: "anna".into(),
            origin: AcquisitionOrigin::AlbumHeart,
            tracks: vec![help_on_the_album()],
        },
        Vec::<String>::new(),
    );
    rig.coordinator.file_hold(
        "soulseek",
        "help",
        Some("anna"),
        AcquisitionOrigin::AlbumHeart,
        WaitingFor::MusicBrainz,
        vec![holder("anna")],
        "Waiting for MusicBrainz",
        None,
        None,
        &mut Vec::new(),
    );
    let held = rig.holds.snapshot().remove(0);
    assert!(held.album_track.is_some(), "held as the album's track");
    assert!(held.held_files.is_empty(), "nothing fetched yet");
    assert!(rig.tracker.is_held("soulseek", "album-1"));
    let (landed, stop) = land_everything(&rig.queue);

    rig.coordinator.resume_held(&held.key());

    until(|| rig.holds.snapshot().is_empty()).await;
    let taken = landed.lock().clone();
    assert_eq!(taken.len(), 1, "the track is fetched");
    assert_eq!(taken[0].external_id, "help");
    assert_eq!(taken[0].origin, Some(AcquisitionOrigin::AlbumHeart));
    let gated: Vec<String> = taken[0].gates().into_iter().map(|gate| gate.user).collect();
    assert_eq!(gated, ["anna"], "for anna's album heart");
    until(|| !rig.coordinator.resuming.lock().contains(&held.key())).await;
    assert!(
        !rig.tracker.is_held("soulseek", "album-1"),
        "the album's row lets go"
    );
    stop.cancel();
}

/// A hold whose sound `fpcalc` could not read for the clip check waits on its own: Deezer answering
/// again leaves it alone, and its holder's next play runs it again with what they heard. With
/// AcoustID on, a hold the clip check could not tell runs again on its holder's next play too;
/// with AcoustID off, a play leaves it waiting for the person.
#[tokio::test]
async fn a_holders_play_runs_again_the_holds_a_new_hearing_can_settle() {
    use octo_core::fingerprint::heart_gate::ClipReading;

    for (waiting, acoust_id_on, runs) in [
        (ClipReading::UNPRINTED_WAITS_FOR, true, true),
        (WaitingFor::AcoustIdOff, true, true),
        (WaitingFor::AcoustIdOff, false, false),
    ] {
        let gates = Arc::new(ListedGates::default());
        gates.hold("brandon", waiting);
        let holds = memory_holds();
        let key = holds
            .hold(held("help", AcquisitionOrigin::Heart, waiting, &["brandon"]))
            .key();
        let coordinator = HeartAcquisitionCoordinator::new(
            youtube_only(),
            Arc::new(TrackAcquisitionQueue::new()),
            Arc::new(FakeDownloads::default()),
            Arc::new(FakeLidarr::default()),
            HeartCoordinatorExtras {
                tracker: Some(Arc::new(AcquisitionTracker::new(None, Clock::system()))),
                holds: Some(holds.clone()),
                gates: Some(gates.clone()),
                acoust_id_on: Some(Arc::new(move || acoust_id_on)),
                ..Default::default()
            },
        );
        coordinator.resume_waiting(WaitingFor::Deezer);
        assert!(
            coordinator.resuming.lock().is_empty(),
            "{waiting:?}: Deezer's answer leaves it"
        );
        coordinator.heard_again("help", "brandon", &heard_video());
        if runs {
            until(|| !gates.built.lock().is_empty()).await;
            until(|| coordinator.resuming.lock().is_empty()).await;
            let record = holds.get(&key).expect("held again");
            assert_eq!(
                record.holders[0].heard.as_ref().map(|heard| &heard.source),
                Some(&heard_video().source),
                "{waiting:?}: with what they heard now"
            );
        } else {
            assert!(coordinator.resuming.lock().is_empty(), "{waiting:?}");
            assert!(gates.built.lock().is_empty(), "{waiting:?}");
        }
    }
}

/// Taking the last heart back of a song nothing else holds or fetches lets its row go, rather
/// than leaving it searching until it is pruned.
#[tokio::test]
async fn taking_the_last_heart_back_lets_the_row_go() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::NotYetProven);
    let rig = rig(gates, None, memory_holds());
    assert_eq!(
        rig.coordinator
            .acquire_track_outcome(
                "soulseek",
                "help",
                Some("brandon"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
            .expect("ran"),
        TrackOutcome::Held(WaitingFor::NotYetProven)
    );
    assert!(rig.tracker.is_held("soulseek", "help"));
    assert_eq!(rig.coordinator.leave_holds("soulseek", "help", "brandon"), 1);
    assert!(
        rig.tracker.all().iter().all(|row| row.external_id != "help"),
        "{:?}",
        rig.tracker.all()
    );
}

/// An import whose identity Deezer cannot answer goes back on the trickle to be asked again on its
/// next turn, as one MusicBrainz cannot answer does; it never holds.
#[tokio::test]
async fn an_import_deezer_cannot_answer_for_goes_back_on_the_trickle() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::Deezer);
    let rig = rig(gates, None, memory_holds());
    let back = rig
        .coordinator
        .acquire_track_for(
            "soulseek",
            "help",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Import,
        )
        .await
        .expect_err("put back");
    let later = back
        .downcast_ref::<crate::services::imports::trickle_worker::TrickleCheckLater>()
        .expect("put back");
    assert_eq!(later.detail, "Waiting for the music catalog");
    assert!(later.turn_back);
    assert!(rig.holds.snapshot().is_empty(), "nothing held");
}

/// A coordinator whose gates are `gates`, its downloads landing at once, and whose Deezer's last
/// call answered as `answering` says.
fn deezer_rig(
    gates: Arc<dyn HeartGates>,
    holds: Arc<SoulseekHoldStore>,
    answering: Option<Arc<Mutex<Option<u64>>>>,
) -> Rig {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let lidarr = Arc::new(FakeLidarr::default());
    let coordinator = HeartAcquisitionCoordinator::new(
        youtube_only(),
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            tracker: Some(tracker.clone()),
            holds: Some(holds.clone()),
            gates: Some(gates),
            deezer_answering: answering.map(|answering| {
                Arc::new(move || *answering.lock()) as Arc<dyn Fn() -> Option<u64> + Send + Sync>
            }),
            ..Default::default()
        },
    );
    Rig {
        coordinator,
        queue,
        holds,
        tracker,
        lidarr,
    }
}

/// Deezer answered again after a heart met it busy, before its hold was filed: the hold, filed
/// while Deezer's last call answered, runs once more at once, and no more until Deezer answers
/// again after a call it did not answer.
#[tokio::test]
async fn a_hold_filed_after_deezer_answered_again_runs_once_more() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::Deezer);
    let answering = Arc::new(Mutex::new(Some(1)));
    let rig = deezer_rig(gates.clone(), memory_holds(), Some(answering.clone()));
    assert_eq!(
        rig.coordinator
            .acquire_track_outcome(
                "soulseek",
                "help",
                Some("brandon"),
                None,
                &AcquisitionTarget::Shared,
                AcquisitionOrigin::Heart,
            )
            .await
            .expect("ran"),
        TrackOutcome::Held(WaitingFor::Deezer)
    );
    until(|| gates.built.lock().len() == 2).await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    assert_eq!(gates.built.lock().len(), 2, "once more, and no more");

    // Deezer's last call went unanswered: a hold filed now waits for its next answer.
    *answering.lock() = None;
    rig.coordinator
        .acquire_track_outcome(
            "soulseek",
            "yesterday",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert!(rig.coordinator.resuming.lock().is_empty());
    assert_eq!(gates.built.lock().len(), 3);
}

/// Gates for holds that wait for Deezer: each build is noted, anna's waits until `open`, and the
/// people in `busy` meet Deezer busy again.
struct DeezerGates {
    open: tokio::sync::watch::Receiver<bool>,
    built: Mutex<Vec<String>>,
    busy: Vec<&'static str>,
}

#[async_trait]
impl HeartGates for DeezerGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        self.built.lock().push(holder.user.clone());
        if holder.user == "anna" {
            let mut open = self.open.clone();
            let _ = open.wait_for(|open| *open).await;
        }
        if self.busy.iter().any(|busy| *busy == holder.user) {
            return GateStart::Hold {
                gate: gate(holder, origin),
                waiting_for: WaitingFor::Deezer,
                line: "Waiting for Deezer".into(),
                question: None,
            };
        }
        GateStart::Ready(gate(holder, origin))
    }
}

/// Deezer answering again runs the holds waiting for it one at a time, oldest first, each once the
/// one before got past Deezer; when one meets Deezer busy again, the rest wait for its next
/// answer.
#[tokio::test]
async fn deezer_answering_again_runs_its_holds_one_at_a_time_oldest_first() {
    for busy in [Vec::new(), vec!["ben"]] {
        let (open, opened) = tokio::sync::watch::channel(false);
        let gates = Arc::new(DeezerGates {
            open: opened,
            built: Mutex::new(Vec::new()),
            busy: busy.clone(),
        });
        let holds = memory_holds();
        let since = Utc::now();
        for (minute, (id, user)) in [("a", "anna"), ("b", "ben"), ("c", "cleo")]
            .into_iter()
            .enumerate()
        {
            let mut record = held(id, AcquisitionOrigin::Heart, WaitingFor::Deezer, &[user]);
            record.held_since_utc = since + chrono::TimeDelta::minutes(minute as i64);
            holds.hold(record);
        }
        let rig = deezer_rig(gates.clone(), holds, None);
        let (_landed, stop) = land_everything(&rig.queue);

        rig.coordinator.resume_waiting(WaitingFor::Deezer);
        until(|| *gates.built.lock() == ["anna"]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*gates.built.lock(), ["anna"], "the next waits for the oldest");
        open.send_replace(true);
        if busy.is_empty() {
            until(|| *gates.built.lock() == ["anna", "ben", "cleo"]).await;
            until(|| rig.holds.snapshot().is_empty()).await;
        } else {
            until(|| *gates.built.lock() == ["anna", "ben"]).await;
            until(|| rig.coordinator.resuming.lock().is_empty()).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(
                *gates.built.lock(),
                ["anna", "ben"],
                "cleo waits for Deezer's next answer"
            );
            assert_eq!(users_of_holds(&rig.holds), ["ben", "cleo"]);
        }
        stop.cancel();
    }
}

/// Everyone still held, oldest hold first.
fn users_of_holds(holds: &SoulseekHoldStore) -> Vec<String> {
    holds.snapshot().iter().flat_map(users).collect()
}

/// While a heart's chain runs, the copy of what its person heard is held: a sweep that drops
/// copies meanwhile leaves it, and once the chain ends with nothing held for them it goes free.
#[tokio::test]
async fn a_running_heart_holds_its_heard_copy_until_its_chain_ends() {
    use crate::services::prefetch::heard_samples::tests::rig_in;
    use crate::services::soulseek::SharedRouting;
    use octo_core::fingerprint::heart_gate::{HeardRecord, HeardSource};
    use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};

    const VIDEO: &str = "AAAAAAAAAA1";
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("copies")).unwrap();
    std::fs::write(
        dir.path().join("copies").join(format!("{VIDEO}.preview")),
        vec![0u8; 10],
    )
    .unwrap();
    let holds = memory_holds();
    let heard = rig_in(dir.path(), "http://127.0.0.1:1", holds.clone(), 16, 1 << 20);
    let id = heard.registry.register(SharedRouting::new(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("Help!".into()),
        ..Default::default()
    }));
    heard.registry.remember_heard(
        &id,
        "brandon",
        HeardRecord {
            heard_at: Utc::now(),
            source: HeardSource::Video {
                id: VIDEO.into(),
                size: Some(10),
                content_type: "audio/mp4".into(),
                itag: None,
            },
            sample_id: None,
        },
        None,
    );
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let coordinator = HeartAcquisitionCoordinator::new(
        youtube_only(),
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        Arc::new(FakeLidarr::default()),
        HeartCoordinatorExtras {
            tracker: Some(Arc::new(AcquisitionTracker::new(None, Clock::system()))),
            id_registry: Some(heard.registry.clone()),
            holds: Some(holds.clone()),
            gates: Some(Arc::new(ListedGates::default())),
            heard: Some(heard.samples.clone()),
            ..Default::default()
        },
    );
    let copies = heard.copies.clone();
    let kept_mid_chain = Arc::new(Mutex::new(None));
    let (seen, stop) = (kept_mid_chain.clone(), CancellationToken::new());
    let token = stop.clone();
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            // A sweep runs while the download does.
            copies.drop_copy(VIDEO);
            *seen.lock() = Some(copies.is_ready(VIDEO));
            for gate in request.gates() {
                request
                    .completion
                    .ledger()
                    .record(&gate.user, gate.origin, GateOutcome::Accepted);
            }
            queue.release(&request);
            request.completion.try_set_result("/music/help.flac");
        }
    });
    let outcome = coordinator
        .acquire_track_outcome(
            "soulseek",
            &id,
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert_eq!(outcome, TrackOutcome::Landed("/music/help.flac".into()));
    assert_eq!(
        *kept_mid_chain.lock(),
        Some(true),
        "the sweep left the heard copy"
    );
    assert!(!heard.copies.is_held(VIDEO), "free once the chain ended");
    stop.cancel();
}

/// The video `brandon` heard of "Help!" in the heard-copy tests.
const HEARD_VIDEO: &str = "AAAAAAAAAA1";

/// What `brandon` heard of "Help!": `HEARD_VIDEO`, whose copy is ready.
fn heard_help() -> HeardRecord {
    HeardRecord {
        heard_at: Utc::now(),
        source: octo_core::fingerprint::heart_gate::HeardSource::Video {
            id: HEARD_VIDEO.into(),
            size: Some(10),
            content_type: "audio/mp4".into(),
            itag: None,
        },
        sample_id: None,
    }
}

/// A coordinator over heard samples that hold the ready copy of what `brandon` heard of "Help!"
/// (`id` in their registry), its gates made from those samples.
struct HeardCopyRig {
    heard: crate::services::prefetch::heard_samples::tests::Rig,
    coordinator: Arc<HeartAcquisitionCoordinator>,
    queue: Arc<TrackAcquisitionQueue>,
    holds: Arc<SoulseekHoldStore>,
    id: String,
    _dir: tempfile::TempDir,
}

fn heard_copy_rig(
    settings: Arc<SettingsStore>,
    gates: impl FnOnce(&crate::services::prefetch::heard_samples::tests::Rig) -> Arc<dyn HeartGates>,
) -> HeardCopyRig {
    use crate::services::prefetch::heard_samples::tests::rig_in;
    use crate::services::soulseek::SharedRouting;
    use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};

    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("copies")).expect("made");
    std::fs::write(
        dir.path().join("copies").join(format!("{HEARD_VIDEO}.preview")),
        vec![0u8; 10],
    )
    .expect("written");
    let holds = memory_holds();
    let heard = rig_in(dir.path(), "http://127.0.0.1:1", holds.clone(), 16, 1 << 20);
    let id = heard.registry.register(SharedRouting::new(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("Help!".into()),
        ..Default::default()
    }));
    heard.registry.remember_heard(&id, "brandon", heard_help(), None);
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let coordinator = HeartAcquisitionCoordinator::new(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        Arc::new(FakeLidarr::default()),
        HeartCoordinatorExtras {
            tracker: Some(Arc::new(AcquisitionTracker::new(None, Clock::system()))),
            id_registry: Some(heard.registry.clone()),
            holds: Some(holds.clone()),
            gates: Some(gates(&heard)),
            heard: Some(heard.samples.clone()),
            ..Default::default()
        },
    );
    HeardCopyRig {
        heard,
        coordinator,
        queue,
        holds,
        id,
        _dir: dir,
    }
}

/// Gates that drop the heard copy while each one is built, as a sweep could meanwhile, noting
/// whether the copy was still there after.
struct SweepingGates {
    copies: Arc<crate::services::prefetch::preview_copies::PreviewCopies>,
    kept: Mutex<Vec<bool>>,
}

impl SweepingGates {
    fn over(heard: &crate::services::prefetch::heard_samples::tests::Rig) -> Arc<SweepingGates> {
        Arc::new(SweepingGates {
            copies: heard.copies.clone(),
            kept: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl HeartGates for SweepingGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        self.copies.drop_copy(HEARD_VIDEO);
        self.kept.lock().push(self.copies.is_ready(HEARD_VIDEO));
        GateStart::Ready(gate(holder, origin))
    }
}

/// A heart's chain holds the copy of what its person heard from before their gate is built: a
/// sweep while the gate is built leaves it, and it goes free once the chain ends.
#[tokio::test]
async fn a_heart_holds_its_heard_copy_while_its_gate_is_built() {
    let mut sweeping = None;
    let rig = heard_copy_rig(youtube_only(), |heard| {
        let gates = SweepingGates::over(heard);
        sweeping = Some(gates.clone());
        gates
    });
    let gates = sweeping.expect("built");
    let (_landed, stop) = land_everything(&rig.queue);
    let outcome = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            &rig.id,
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    assert!(matches!(outcome, TrackOutcome::Landed(_)), "{outcome:?}");
    assert_eq!(*gates.kept.lock(), [true], "the sweep left the heard copy");
    assert!(
        !rig.heard.copies.is_held(HEARD_VIDEO),
        "free once the chain ended"
    );
    stop.cancel();
}

/// A DownloadOnPlay play holds the copy of what it played from before its gate is built: a sweep
/// while the gate is built leaves it.
#[tokio::test]
async fn a_play_holds_its_heard_copy_while_its_gate_is_built() {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            download_on_play: true,
            heart_download_sources: vec![HeartDownloadStep {
                source: HeartDownloadSource::YouTube,
                song_enabled: Some(true),
                album_enabled: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    }));
    let mut sweeping = None;
    let rig = heard_copy_rig(settings, |heard| {
        let gates = SweepingGates::over(heard);
        sweeping = Some(gates.clone());
        gates
    });
    let gates = sweeping.expect("built");
    let (landed, stop) = land_everything(&rig.queue);
    rig.coordinator.queue_play_heard(
        "soulseek",
        &rig.id,
        Some("brandon"),
        None,
        None,
        Some(heard_help()),
    );
    until(|| !landed.lock().is_empty()).await;
    assert_eq!(*gates.kept.lock(), [true], "the sweep left the heard copy");
    stop.cancel();
}

/// A heart waiting out a Soulseek outage when Octo restarted runs again as its person's own chain:
/// once it has landed and they have left the outage's hold, the copy of what they heard goes
/// free, whether the hold listed what they heard or not.
#[tokio::test]
async fn a_resumed_outage_hold_lets_go_of_the_heard_copy_once_its_person_leaves_it() {
    for listed in [false, true] {
        let rig = heard_copy_rig(youtube_only(), |_| Arc::new(ListedGates::default()));
        rig.holds.hold(HeldAcquisition {
            provider: "soulseek".into(),
            external_id: rig.id.clone(),
            requested_by: Some("brandon".into()),
            held_since_utc: Utc::now(),
            holders: vec![Holder {
                user: "brandon".into(),
                heard: listed.then(heard_help),
                ..Default::default()
            }],
            ..Default::default()
        });
        let (landed, stop) = land_everything(&rig.queue);
        Arc::new(SoulseekHoldResumer::new(
            rig.holds.clone(),
            rig.coordinator.clone(),
        ))
        .run(CancellationToken::new())
        .await
        .expect("ran");
        until(|| rig.holds.snapshot().is_empty()).await;
        assert_eq!(landed.lock().len(), 1, "listed: {listed}");
        until(|| !rig.heard.copies.is_held(HEARD_VIDEO)).await;
        stop.cancel();
    }
}

/// Two people's hearts waiting out one Soulseek outage when Octo restarted: each runs again as
/// their own, and the hold ends only once both have left it.
#[tokio::test]
async fn an_outage_hold_of_two_people_resumes_both_after_a_restart() {
    let holds = memory_holds();
    holds.hold(HeldAcquisition {
        provider: "soulseek".into(),
        external_id: "help".into(),
        requested_by: Some("brandon".into()),
        held_since_utc: Utc::now(),
        holders: vec![holder("brandon"), holder("anna")],
        ..Default::default()
    });
    let rig = rig(Arc::new(ListedGates::default()), None, holds);
    let (landed, stop) = land_everything(&rig.queue);
    let resumer = Arc::new(SoulseekHoldResumer::new(
        rig.holds.clone(),
        rig.coordinator.clone(),
    ));
    resumer.run(CancellationToken::new()).await.expect("ran");
    until(|| rig.holds.snapshot().is_empty()).await;
    let mut asked: Vec<String> = landed
        .lock()
        .iter()
        .flat_map(|request| request.gates().into_iter().map(|gate| gate.user))
        .collect();
    asked.sort();
    asked.dedup();
    assert_eq!(asked, ["anna", "brandon"], "each person's heart ran again");
    stop.cancel();
}

/// Gates that hold for AcoustID, noting each wake they are asked to set up.
#[derive(Default)]
struct BusyGates {
    wakes: Mutex<Vec<(String, WaitingFor, String)>>,
}

#[async_trait]
impl HeartGates for BusyGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        GateStart::Hold {
            gate: gate(holder, origin),
            waiting_for: WaitingFor::AcoustId,
            line: "Waiting for AcoustID".into(),
            question: None,
        }
    }

    fn wake_for(
        &self,
        key: &str,
        waiting_for: WaitingFor,
        user: &str,
        _: Option<&HeardRecord>,
        _: Option<String>,
    ) {
        self.wakes
            .lock()
            .push((key.to_string(), waiting_for, user.to_string()));
    }
}

/// A held album track checked again after its person's play, whose heard lookup AcoustID does not
/// answer, waits as it is and runs again once AcoustID answers: the play is not lost.
#[tokio::test]
async fn a_held_album_tracks_check_that_meets_a_busy_answer_runs_again_when_it_answers() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("help.flac");
    std::fs::write(&file, b"x").expect("written");
    let holds = memory_holds();
    let mut record = held(
        "help",
        AcquisitionOrigin::AlbumHeart,
        WaitingFor::NotYetProven,
        &["brandon"],
    );
    record.album_track = Some(crate::services::common::soulseek_hold_store::HeldAlbumTrack {
        album_key: "soulseek:album-1".into(),
        ..Default::default()
    });
    record.held_files = vec![file.to_string_lossy().into_owned()];
    let key = holds.hold(record).key();
    let gates = Arc::new(BusyGates::default());
    let rig = rig(gates.clone(), None, holds);
    rig.coordinator.recheck_held(&key).await;
    assert_eq!(
        *gates.wakes.lock(),
        [(key.clone(), WaitingFor::AcoustId, "brandon".to_string())]
    );
    assert!(rig.holds.get(&key).is_some(), "it waits as it is");
}

/// A play of a held song neither opens its own row over the held one nor fails it when its
/// download fails: the row keeps the line it waits with.
#[tokio::test]
async fn a_play_of_a_held_song_leaves_its_row_alone() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::NotYetProven);
    let rig = rig(gates, None, memory_holds());
    rig.coordinator
        .acquire_track_outcome(
            "soulseek",
            "help",
            Some("brandon"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::Heart,
        )
        .await
        .expect("ran");
    let line = rig.tracker.note_of("soulseek", "help");
    assert_eq!(line.as_deref(), Some("Waiting for NotYetProven"));
    let (queue, stop) = (rig.queue.clone(), CancellationToken::new());
    let token = stop.clone();
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            queue.release(&request);
            request
                .completion
                .try_set_error(anyhow::anyhow!("no peer had it"));
        }
    });
    rig.coordinator.queue_play_download(
        "soulseek",
        "help",
        DownloadSource::Soulseek,
        Some("anna"),
        None,
        None,
        &AcquisitionTarget::Shared,
        Vec::new(),
        None,
    );
    until(|| !rig.queue.is_in_flight("soulseek", "help")).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(rig.tracker.is_held("soulseek", "help"));
    assert_eq!(rig.tracker.note_of("soulseek", "help"), line);
    let row = rig
        .tracker
        .all()
        .into_iter()
        .find(|row| row.external_id == "help")
        .expect("a row");
    assert_ne!(row.state, AcquisitionState::Failed, "{row:?}");
    stop.cancel();
}

/// A keep that landed its file just before Octo stopped, its hold not yet ended: after the restart
/// its person leaves the hold as kept, never as a copy that could not be found. Only a file the
/// library keeps as kept anyway for that very person counts: the library's other copy of the song,
/// or a copy kept for someone else, ends nobody's keep.
#[tokio::test]
async fn a_keep_that_landed_before_a_restart_ends_its_hold_as_kept() {
    use crate::services::local::test_support::FakeLocalLibrary;
    use crate::services::local::{KeptHeart, LocalSongMapping};

    let kept_for = |user: &str| LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: "help".into(),
        local_path: format!("/music/help-{user}.m4a"),
        proof_mark: Some("kept by you, version not proven".into()),
        heart: Some(KeptHeart {
            users: vec![user.to_string()],
            ..Default::default()
        }),
        ..Default::default()
    };
    for (people, kept) in [(vec!["brandon"], true), (vec!["brandon", "anna"], false)] {
        let library = FakeLocalLibrary::default();
        library
            .by_tags
            .lock()
            .push(("The Beatles".into(), "Help!".into(), None, kept_for("brandon")));
        let (rig, dir, _) = owned_rig_with(Arc::new(ListedGates::default()), Some(Arc::new(library)));
        let mut record = held("help", AcquisitionOrigin::Heart, WaitingFor::AcoustIdOff, &people);
        record.held_files = vec![dir.path().join("gone.flac").to_string_lossy().into_owned()];
        record.keep_requested = people.iter().map(|user| user.to_string()).collect();
        let record = rig.holds.hold(record);
        rig.coordinator.hold_row_for(&record);
        rig.coordinator.finish_keeps(vec![record.key()]).await;
        assert!(rig.holds.snapshot().is_empty(), "the hold ended");
        let row = rig
            .tracker
            .all()
            .into_iter()
            .find(|row| row.external_id == "help")
            .expect("a row");
        if kept {
            assert_ne!(row.state, AcquisitionState::Failed, "{row:?}");
            assert_ne!(row.error.as_deref(), Some(HEARD_GONE));
        } else {
            // Anna's keep never landed and nothing she heard is known: her heart ends with the
            // plain line, which brandon's ended keep never wrote.
            assert_eq!(row.error.as_deref(), Some(HEARD_GONE), "{row:?}");
            let notes = notes(&rig.tracker, "soulseek:help");
            assert!(!notes.iter().any(|note| note == HEARD_GONE), "{notes:?}");
        }
    }
}

/// A keep that landed just before Octo stopped while someone else still holds the files: after the
/// restart its person leaves the hold as kept, and nothing is kept again for them; the other holder
/// keeps waiting with the files.
#[tokio::test]
async fn a_keep_that_landed_before_a_restart_is_not_kept_again_while_others_hold_the_files() {
    use crate::services::local::test_support::FakeLocalLibrary;
    use crate::services::local::{KeptHeart, LocalSongMapping};

    let library = FakeLocalLibrary::default();
    library.by_tags.lock().push((
        "The Beatles".into(),
        "Help!".into(),
        None,
        LocalSongMapping {
            external_provider: "soulseek".into(),
            external_id: "help".into(),
            local_path: "/music/help-brandon.flac".into(),
            proof_mark: Some("kept by you, version not proven".into()),
            heart: Some(KeptHeart {
                users: vec!["brandon".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        },
    ));
    let (rig, dir, _) = owned_rig_with(Arc::new(ListedGates::default()), Some(Arc::new(library)));
    let file = dir.path().join("held.flac");
    std::fs::write(&file, b"x").expect("written");
    let mut record = held(
        "help",
        AcquisitionOrigin::AlbumHeart,
        WaitingFor::NotYetProven,
        &["brandon", "anna"],
    );
    record.album_track = Some(crate::services::common::soulseek_hold_store::HeldAlbumTrack {
        album_key: "soulseek:album-1".into(),
        ..Default::default()
    });
    record.held_files = vec![file.to_string_lossy().into_owned()];
    record.keep_requested = vec!["brandon".to_string()];
    let record = rig.holds.hold(record);
    let taken: Taken = Arc::default();
    let (queue, seen, stop) = (rig.queue.clone(), taken.clone(), CancellationToken::new());
    let token = stop.clone();
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            seen.lock().push(request.clone());
            queue.release(&request);
            request.completion.try_set_result("/music/help-again.flac");
        }
    });

    rig.coordinator.finish_keeps(vec![record.key()]).await;

    assert!(taken.lock().is_empty(), "nothing is kept again for brandon");
    let left = rig.holds.get(&record.key()).expect("anna still waits");
    assert_eq!(users(&left), ["anna"]);
    assert!(left.keep_requested.is_empty());
    assert!(file.is_file(), "the files stay held for anna");
    stop.cancel();
}

/// A family request's gate is built as a request's: never holding, so a song fetched at one
/// quality is judged by the reading's refusals too.
#[tokio::test]
async fn a_family_requests_gate_is_built_and_never_holds() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("kim", WaitingFor::MusicBrainz);
    let rig = rig(gates, None, memory_holds());
    let built = rig.coordinator.request_gates("soulseek", "help", "kim").await;
    assert_eq!(built.len(), 1);
    assert_eq!(built[0].origin, AcquisitionOrigin::Request);
    assert_eq!(built[0].user, "kim");
    assert!(rig.holds.snapshot().is_empty());
}

// ---- Deezer's turns, runs that end, and what a run holds --------------------------------------

/// A hold Deezer keeps failing for its own reason never starves the others: each hold waiting for
/// Deezer has one turn per answer, the one served longest ago first, so after Deezer's next answer
/// the others run and land while the first still waits.
#[tokio::test]
async fn a_hold_deezer_keeps_failing_for_never_starves_the_others() {
    let (_open, opened) = tokio::sync::watch::channel(true);
    let gates = Arc::new(DeezerGates {
        open: opened,
        built: Mutex::new(Vec::new()),
        busy: vec!["anna"],
    });
    let holds = memory_holds();
    let since = Utc::now();
    for (minute, (id, user)) in [("a", "anna"), ("b", "ben"), ("c", "cleo")]
        .into_iter()
        .enumerate()
    {
        let mut record = held(id, AcquisitionOrigin::Heart, WaitingFor::Deezer, &[user]);
        record.held_since_utc = since + chrono::TimeDelta::minutes(minute as i64);
        holds.hold(record);
    }
    let answering = Arc::new(Mutex::new(Some(1)));
    let rig = deezer_rig(gates.clone(), holds, Some(answering.clone()));
    let (_landed, stop) = land_everything(&rig.queue);

    rig.coordinator.resume_waiting(WaitingFor::Deezer);
    until(|| !gates.built.lock().is_empty()).await;
    until(|| rig.coordinator.resuming.lock().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        gates.built.lock().iter().all(|user| user == "anna"),
        "{:?}",
        gates.built.lock()
    );
    assert_eq!(users_of_holds(&rig.holds), ["anna", "ben", "cleo"]);

    // Deezer answers another call: anna's failure was her own, and the others have their turn.
    *answering.lock() = Some(2);
    rig.coordinator.resume_waiting(WaitingFor::Deezer);
    until(|| users_of_holds(&rig.holds) == ["anna"]).await;
    let built = gates.built.lock().clone();
    assert!(built.contains(&"ben".to_string()) && built.contains(&"cleo".to_string()));
    stop.cancel();
}

/// The hold whose turn it is at Deezer, which stopped waiting before its run could claim it, gives
/// the turn up: the next hold waiting for Deezer runs, and a give-up line owed to it is dropped.
#[tokio::test]
async fn a_deezer_turn_whose_hold_stopped_waiting_passes_to_the_next() {
    let holds = memory_holds();
    holds.hold(held("b", AcquisitionOrigin::Heart, WaitingFor::Deezer, &["ben"]));
    let rig = deezer_rig(Arc::new(ListedGates::default()), holds, None);
    let (_landed, stop) = land_everything(&rig.queue);
    let gone = "Track:soulseek:gone:Deezer";
    *rig.coordinator.deezer_turn.lock() = Some(gone.to_string());
    rig.coordinator
        .gave_up
        .lock()
        .insert(gone.to_string(), ("soulseek".into(), "gone".into()));

    rig.coordinator.resume_held(gone);

    assert!(rig.coordinator.gave_up.lock().is_empty(), "nothing is owed to it");
    until(|| rig.holds.snapshot().is_empty()).await;
    stop.cancel();
}

/// A run whose hold another run took after a wake leaves the wake, the turn at Deezer and the
/// give-up line to that run, whose last pass settles them.
#[tokio::test]
async fn a_run_whose_hold_another_run_took_leaves_its_turn_and_line_to_that_run() {
    let holds = memory_holds();
    holds.hold(held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::Deezer,
        &["brandon"],
    ));
    let rig = rig(Arc::new(ListedGates::default()), None, holds);
    let key = "Track:soulseek:help:Deezer";
    *rig.coordinator.deezer_turn.lock() = Some(key.to_string());
    rig.coordinator
        .gave_up
        .lock()
        .insert(key.to_string(), ("soulseek".into(), "help".into()));
    // A wake came during the run, and another run took the hold before this one could again.
    rig.coordinator.again.lock().insert(key.to_string());
    rig.coordinator.resuming.lock().insert(key.to_string());

    assert!(!rig.coordinator.run_again(key));

    assert!(
        rig.coordinator.again.lock().contains(key),
        "the wake is that run's"
    );
    assert_eq!(rig.coordinator.deezer_turn.lock().as_deref(), Some(key));
    assert!(rig.coordinator.gave_up.lock().contains_key(key));
}

/// Two chains of one person and song (a song heart's and an album walk's) each hold the copy of
/// what they heard under a run of their own: the first to end leaves it held for the other, and it
/// goes free once both have.
#[tokio::test]
async fn two_chains_of_one_person_and_song_each_hold_the_heard_copy_for_their_own_run() {
    let rig = heard_copy_rig(youtube_only(), |_| Arc::new(ListedGates::default()));
    let brandon = Holder {
        user: "brandon".into(),
        heard: Some(heard_help()),
        ..Default::default()
    };
    let first = rig
        .coordinator
        .hold_while_running("soulseek", &rig.id, std::slice::from_ref(&brandon), false)
        .expect("held");
    let second = rig
        .coordinator
        .hold_while_running("soulseek", &rig.id, &[brandon], false)
        .expect("held");
    assert_ne!(first.copy_ids(), second.copy_ids());
    drop(first);
    assert!(
        rig.heard.copies.is_held(HEARD_VIDEO),
        "the other chain still holds it"
    );
    drop(second);
    assert!(!rig.heard.copies.is_held(HEARD_VIDEO));
}

/// An import whose sound `fpcalc` could not read goes back on its trickle, as one waiting for
/// MusicBrainz or Deezer does, rather than waiting in a hold.
#[test]
fn an_import_whose_sound_could_not_be_read_goes_back_on_its_trickle() {
    assert!(trickle_waits(WaitingFor::SoundUnread));
    assert!(!files_hold(AcquisitionOrigin::Import, WaitingFor::SoundUnread));
    assert!(files_hold(AcquisitionOrigin::Heart, WaitingFor::SoundUnread));
}

// ---- One request per person, and a hold that serves two routes --------------------------------

/// Gates that are ready, noting each build's person and origin, and judge a held album track's
/// file by the album's rule (`judge_held`) or the song's (`judge_landed`), noting which, each
/// accepting it only for the people listed in `accepting`.
#[derive(Default)]
struct RouteGates {
    built: Mutex<Vec<(String, AcquisitionOrigin)>>,
    judged: Mutex<Vec<(String, &'static str)>>,
    accepting: Mutex<Vec<String>>,
}

impl RouteGates {
    fn verdicts(&self, gates: &[HeartGate], rule: &'static str) -> Vec<GateVerdict> {
        gates
            .iter()
            .map(|gate| {
                self.judged.lock().push((gate.user.clone(), rule));
                if self.accepting.lock().contains(&gate.user) {
                    GateVerdict::Accept {
                        by: Acceptance::Sound,
                        tags: octo_core::fingerprint::heart_gate::TagClaim::AsHeard,
                    }
                } else {
                    GateVerdict::Refuse(octo_core::fingerprint::heart_gate::Refusal::AnotherRecording)
                }
            })
            .collect()
    }
}

#[async_trait]
impl HeartGates for RouteGates {
    async fn build(&self, _: &str, _: &str, holder: &Holder, origin: AcquisitionOrigin) -> GateStart {
        self.built.lock().push((holder.user.clone(), origin));
        GateStart::Ready(gate(holder, origin))
    }

    async fn judge_held(&self, _: &str, _: LabelSource, _: &str, gates: &[HeartGate]) -> Vec<GateVerdict> {
        self.verdicts(gates, "album")
    }

    async fn judge_landed(&self, _: &str, _: LabelSource, _: &str, gates: &[HeartGate]) -> Vec<GateVerdict> {
        self.verdicts(gates, "song")
    }
}

/// "Help!" alone, as the album "album-1".
fn help_album() -> Album {
    Album {
        id: "album-1".into(),
        title: "Help!".into(),
        artist: "The Beatles".into(),
        songs: vec![octo_core::models::domain::Song {
            external_provider: Some("soulseek".into()),
            external_id: Some("help".into()),
            title: "Help!".into(),
            artist: "The Beatles".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// brandon's song heart held "not yet proven" with a held file, which anna's album heart then
/// joins: one hold, now the album's held track.
async fn song_hold_joined_by_an_album_heart(
    gates: Arc<RouteGates>,
) -> (Rig, Arc<AlbumGates>, tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("help.flac").to_string_lossy().into_owned();
    std::fs::write(&file, b"x").expect("written");
    let holds = memory_holds();
    let mut record = held(
        "help",
        AcquisitionOrigin::Heart,
        WaitingFor::NotYetProven,
        &["brandon"],
    );
    record.held_files = vec![file];
    let key = holds.hold(record).key();
    let (rig, album_gates) = album_rig(gates, holds);
    let _gate = rig
        .coordinator
        .album_gate_up(
            "soulseek",
            "album-1",
            &help_album(),
            "anna",
            AcquisitionOrigin::AlbumHeart,
        )
        .await
        .expect("ready")
        .expect("a gate");
    (rig, album_gates, dir, key)
}

/// A song's hold that an album heart turned into the album's held track is the album heart's, and
/// each person in it is judged by the heart that keeps them there: the album's by the album's rule,
/// the song's as a song heart, the song's own gate built as a heart's. When every gate refuses its
/// file, the album heart is done with the track, and the song's person goes on with the song's own
/// hold, whose chain runs again as a heart's.
#[tokio::test]
async fn a_merged_hold_every_gate_refuses_ends_the_album_heart_and_runs_the_song_heart_again() {
    let gates = Arc::new(RouteGates::default());
    let (rig, _album_gates, _dir, key) = song_hold_joined_by_an_album_heart(gates.clone()).await;
    let (landed, stop) = land_everything(&rig.queue);

    rig.coordinator.recheck_held(&key).await;

    until(|| landed.lock().len() == 1).await;
    until(|| rig.holds.snapshot().is_empty()).await;
    let built = gates.built.lock().clone();
    assert!(
        built.contains(&("anna".into(), AcquisitionOrigin::AlbumHeart)),
        "{built:?}"
    );
    assert!(
        built.contains(&("brandon".into(), AcquisitionOrigin::Heart)),
        "{built:?}"
    );
    let judged = gates.judged.lock().clone();
    assert!(judged.contains(&("anna".into(), "album")), "{judged:?}");
    assert!(judged.contains(&("brandon".into(), "song")), "{judged:?}");
    let ran = landed.lock()[0].clone();
    assert_eq!(ran.origin, Some(AcquisitionOrigin::Heart), "a song heart's chain");
    let users: Vec<String> = ran.gates().into_iter().map(|gate| gate.user).collect();
    assert_eq!(users, ["brandon"], "anna's album heart is done with the track");
    stop.cancel();
}

/// When only the song's person accepts the held file, it lands for them alone, by their own gate
/// into their own library, and the album heart's held track waits on without it.
#[tokio::test]
async fn a_merged_holds_file_only_the_song_heart_accepts_lands_for_it_alone() {
    let gates = Arc::new(RouteGates::default());
    gates.accepting.lock().push("brandon".into());
    let (rig, _album_gates, _dir, key) = song_hold_joined_by_an_album_heart(gates.clone()).await;
    let (landed, stop) = land_everything(&rig.queue);

    rig.coordinator.recheck_held(&key).await;

    until(|| !landed.lock().is_empty()).await;
    let ran = landed.lock()[0].clone();
    let users: Vec<String> = ran.gates().into_iter().map(|gate| gate.user).collect();
    assert_eq!(users, ["brandon"], "the landing carries his gate alone");
    assert_eq!(ran.requested_by(), ["brandon"]);
    until(|| {
        rig.holds
            .get(&key)
            .is_none_or(|record| users_of(&record) == ["anna"])
    })
    .await;
    stop.cancel();
}

fn users_of(record: &HeldAcquisition) -> Vec<String> {
    users(record)
}

/// A song's hold an album heart turns into the album's held track runs as an album heart's.
#[tokio::test]
async fn a_song_hold_an_album_heart_joins_runs_as_an_album_hearts() {
    let gates = Arc::new(RouteGates::default());
    let (rig, _album_gates, _dir, key) = song_hold_joined_by_an_album_heart(gates).await;
    let merged = rig.holds.get(&key).expect("one hold");
    assert!(merged.album_track.is_some());
    assert_eq!(merged.origin, Some(AcquisitionOrigin::AlbumHeart));
}

/// A hearted album's held track that a song heart joined, and that the album is then un-hearted
/// from, goes on as the song's own hold of a heart, and its next run runs the song's person's
/// chain as a heart's.
#[tokio::test]
async fn an_album_track_left_to_its_song_heart_runs_as_a_hearts_hold() {
    use crate::services::common::soulseek_hold_store::HoldRoute;

    let holds = memory_holds();
    let mut record = held(
        "help",
        AcquisitionOrigin::AlbumHeart,
        WaitingFor::NotYetProven,
        &["brandon", "anna"],
    );
    record.album_track = Some(crate::services::common::soulseek_hold_store::HeldAlbumTrack {
        album_key: "soulseek:album-1".into(),
        song_ids: vec!["help".into()],
        ..Default::default()
    });
    record.holders[0].routes = vec![HoldRoute::Song];
    let key = holds.hold(record).key();
    let (rig, _album_gates) = album_rig(Arc::new(RouteGates::default()), holds);
    assert_eq!(
        rig.coordinator.leave_album_holds("soulseek", "album-1", "anna"),
        1
    );
    let song = rig.holds.get(&key).expect("brandon's song heart keeps it");
    assert!(song.album_track.is_none());
    assert_eq!(song.origin, Some(AcquisitionOrigin::Heart));
    let (landed, stop) = land_everything(&rig.queue);
    rig.coordinator.recheck_held(&key).await;
    until(|| !landed.lock().is_empty()).await;
    assert_eq!(landed.lock()[0].origin, Some(AcquisitionOrigin::Heart));
    stop.cancel();
}

/// An album heart never turns an import's hold of one of its tracks into the album's held track.
#[tokio::test]
async fn an_album_heart_never_takes_an_imports_hold_of_its_track() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("help.flac").to_string_lossy().into_owned();
    std::fs::write(&file, b"x").expect("written");
    let holds = memory_holds();
    let mut record = held(
        "help",
        AcquisitionOrigin::Import,
        WaitingFor::AcoustId,
        &["brandon"],
    );
    record.held_files = vec![file];
    let key = holds.hold(record).key();
    let (rig, _album_gates) = album_rig(Arc::new(RouteGates::default()), holds);
    let _gate = rig
        .coordinator
        .album_gate_up(
            "soulseek",
            "album-1",
            &help_album(),
            "anna",
            AcquisitionOrigin::AlbumHeart,
        )
        .await
        .expect("ready")
        .expect("a gate");
    let import = rig.holds.get(&key).expect("held");
    assert!(import.album_track.is_none());
    assert_eq!(users(&import), ["brandon"]);
}

/// brandon's song heart held for MusicBrainz with no file, which anna's album heart of "album-1"
/// joins while a run of the hold is going (so it runs once that one ends).
async fn album_heart_joins_a_song_hold_with_no_file(gates: Arc<ListedGates>) -> (Rig, AlbumGateHold, String) {
    let holds = memory_holds();
    let key = holds
        .hold(held(
            "help",
            AcquisitionOrigin::Heart,
            WaitingFor::MusicBrainz,
            &["brandon"],
        ))
        .key();
    let (rig, album_gates) = album_rig(gates, holds);
    // The album's gate stays registered while the test holds it, as its walk's chain does.
    let album = album_gates.register(
        "soulseek",
        "album-1",
        AlbumGate {
            user: "anna".into(),
            origin: AcquisitionOrigin::AlbumHeart,
            tracks: vec![help_on_the_album()],
        },
        Vec::<String>::new(),
    );
    rig.coordinator.resuming.lock().insert(key.clone());
    let outcome = rig
        .coordinator
        .acquire_track_outcome(
            "soulseek",
            "help",
            Some("anna"),
            None,
            &AcquisitionTarget::Shared,
            AcquisitionOrigin::AlbumHeart,
        )
        .await
        .expect("ran");
    assert_eq!(outcome, TrackOutcome::Held(WaitingFor::MusicBrainz));
    (rig, album, key)
}

/// An album heart that joins a song's hold holding no file (it waits for MusicBrainz) stays in it
/// by the album, the album named, and the hold stays the song's: when it runs, each person's own
/// chain runs, the album heart's as an album heart's, and the song's person still waits for
/// MusicBrainz until it answers.
#[tokio::test]
async fn an_album_heart_joining_a_song_hold_with_no_file_runs_its_own_chain() {
    use crate::services::common::soulseek_hold_store::HoldRoute;

    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::MusicBrainz);
    let (rig, _album, key) = album_heart_joins_a_song_hold_with_no_file(gates.clone()).await;
    let joined = rig.holds.get(&key).expect("one hold");
    assert!(joined.album_track.is_none(), "still the song's hold");
    assert_eq!(users(&joined), ["brandon", "anna"]);
    assert_eq!(joined.holders[1].routes_in(&joined), [HoldRoute::Album]);
    assert_eq!(
        joined.holders[1]
            .context
            .as_ref()
            .map(|context| context.catalog_album_id.as_str()),
        Some("album-1")
    );

    // The run that was going ends; the hold runs again for both.
    rig.coordinator.resuming.lock().remove(&key);
    let (landed, stop) = land_everything(&rig.queue);
    rig.coordinator.recheck_held(&key).await;
    until(|| !landed.lock().is_empty()).await;
    let ran: Vec<(Vec<String>, Option<AcquisitionOrigin>)> = landed
        .lock()
        .iter()
        .map(|request| {
            (
                request.gates().into_iter().map(|gate| gate.user).collect(),
                request.origin,
            )
        })
        .collect();
    assert_eq!(
        ran,
        [(vec!["anna".to_string()], Some(AcquisitionOrigin::AlbumHeart))]
    );
    until(|| {
        rig.holds
            .get(&key)
            .is_some_and(|record| users(&record) == ["brandon"])
    })
    .await;

    // MusicBrainz answers: brandon's own chain runs as a heart's.
    gates.ready("brandon");
    rig.coordinator.recheck_held(&key).await;
    until(|| rig.holds.snapshot().is_empty()).await;
    assert_eq!(landed.lock()[1].origin, Some(AcquisitionOrigin::Heart));
    stop.cancel();
}

/// Taking back an album heart that joined a song's hold holding no file takes them off it.
#[tokio::test]
async fn taking_back_an_album_heart_leaves_the_song_hold_it_joined_with_no_file() {
    let gates = Arc::new(ListedGates::default());
    gates.hold("brandon", WaitingFor::MusicBrainz);
    let (rig, _album, key) = album_heart_joins_a_song_hold_with_no_file(gates).await;
    assert_eq!(
        rig.coordinator.leave_album_holds("soulseek", "album-1", "anna"),
        1
    );
    assert_eq!(users(&rig.holds.get(&key).expect("brandon's")), ["brandon"]);
}

/// A hold with two people runs one request per person, each carrying that person's gate alone and
/// going into that person's own library: a download that carries a sound check is one person's.
#[tokio::test]
async fn a_hold_of_two_people_runs_one_request_per_person() {
    let holds = memory_holds();
    let key = holds
        .hold(held(
            "help",
            AcquisitionOrigin::Heart,
            WaitingFor::NotYetProven,
            &["brandon", "anna"],
        ))
        .key();
    let rig = rig(Arc::new(ListedGates::default()), None, holds);
    let (landed, stop) = land_everything(&rig.queue);
    rig.coordinator.recheck_held(&key).await;
    until(|| rig.holds.snapshot().is_empty()).await;
    let mut ran: Vec<(Vec<String>, Vec<String>)> = landed
        .lock()
        .iter()
        .map(|request| {
            (
                request.gates().into_iter().map(|gate| gate.user).collect(),
                request.requested_by(),
            )
        })
        .collect();
    ran.sort();
    assert_eq!(
        ran,
        [
            (vec!["anna".to_string()], vec!["anna".to_string()]),
            (vec!["brandon".to_string()], vec!["brandon".to_string()]),
        ]
    );
    stop.cancel();
}
