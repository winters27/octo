//! Holds, joiners and origins: a heart whose gate cannot be built yet holds, saved, its row held;
//! an answer, a restart or a second heart runs it again as one acquisition with every holder's
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

/// Lands every request the queue hands out, keeping each one, until the token is canceled.
fn land_everything(queue: &Arc<TrackAcquisitionQueue>) -> (Taken, CancellationToken) {
    let taken: Taken = Arc::default();
    let stop = CancellationToken::new();
    let (queue, seen, token) = (queue.clone(), taken.clone(), stop.clone());
    tokio::spawn(async move {
        while let Some(request) = queue.dequeue(&token).await {
            seen.lock().push(request.clone());
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
    // A second person's heart joins the hold and asks again at once, as one acquisition.
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
async fn a_record_with_two_holders_resumes_as_one_acquisition_with_each_holders_gate() {
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
    assert_eq!(taken.len(), 1, "one acquisition");
    let gated: Vec<String> = taken[0].gates().into_iter().map(|gate| gate.user).collect();
    assert_eq!(gated, ["brandon", "anna"], "each joined with their own gate");
    assert_eq!(taken[0].origin, Some(AcquisitionOrigin::Heart));
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

    // Their answers come: each is fetched once with its own origin, and every hold ends.
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
        .hand_play_to_lidarr("soulseek:played", "soulseek", "played", Some("brandon"))
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
    assert_eq!(held, None);
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
        while let Some(request) = queue.dequeue(&token).await {
            while request.gates().len() < askers {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
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

/// A record with two holders who heard two mixes resumes as one acquisition: the file of the
/// first's mix lands for the first, the second stays held, and the record ends only when the
/// second's own file lands.
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
    until(|| landed.lock().len() == 1).await;
    until(|| !rig.coordinator.resuming.lock().contains(key)).await;
    assert_eq!(landed.lock()[0].gates().len(), 2, "one acquisition, both gates");
    let record = rig.holds.get(key).expect("anna stays held");
    assert_eq!(users(&record), ["anna"]);
    assert!(rig.tracker.is_held("soulseek", "help"));

    // Her own mix lands: the record ends.
    *accepting.lock() = vec!["anna".to_string()];
    rig.coordinator.resume_held(key);
    until(|| rig.holds.snapshot().is_empty()).await;
    assert_eq!(landed.lock().len(), 2);
    until(|| !rig.tracker.is_held("soulseek", "help")).await;
    stop.cancel();
}

/// Two people with different R join one request: the file lands for the one whose gate passed
/// it, and never counts as the other's.
#[tokio::test]
async fn two_hearts_joining_one_request_each_get_their_own() {
    let rig = rig(Arc::new(ListedGates::default()), None, memory_holds());
    let accepting: Accepting = Arc::new(Mutex::new(vec!["brandon".to_string()]));
    let (landed, stop) = land_judged(&rig.queue, 2, accepting);
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
    until(|| rig.queue.is_in_flight("soulseek", "help")).await;
    let anna = heart("anna");
    assert_eq!(
        brandon.await.expect("joined"),
        TrackOutcome::Landed("/music/help.flac".into())
    );
    assert!(
        matches!(anna.await.expect("joined"), TrackOutcome::Failed(_)),
        "the 2009 file is not what anna heard"
    );
    assert_eq!(landed.lock().len(), 1, "one download for both");
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
/// with it; a hold of another wait, or another person's play, changes nothing.
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(gates.built.lock().is_empty(), "anna holds nothing here");

    rig.coordinator.heard_again("help", "Brandon", &played);
    until(|| !gates.built.lock().is_empty()).await;
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
