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
/// it, and never counts as the other's, whose own heart goes on down its own chain: the step runs
/// again as theirs, and with nothing proven for a heart never played it holds for a play.
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
    assert_eq!(
        anna.await.expect("joined"),
        TrackOutcome::Held(WaitingFor::NotYetProven),
        "the 2009 file is not what anna heard, and her own copy proved nothing"
    );
    let landed = landed.lock().clone();
    assert_eq!(landed.len(), 2, "one download for both, then anna's own");
    let theirs: Vec<String> = landed[1].gates().into_iter().map(|gate| gate.user).collect();
    assert_eq!(theirs, ["anna"]);
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
    {
        let before = SoulseekHoldStore::new(Some(&file));
        before.hold(HeldAcquisition {
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
