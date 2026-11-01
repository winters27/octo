//! "Pass Mornye's check" against a stand-in Mornye and a stand-in helper: when it may start, and
//! how a run ends by work (an armed gateway's session turns active, every ended pause is stamped
//! again, the song's job ends with no browser).

use std::sync::atomic::{AtomicUsize, Ordering};

use octo_core::settings::{AppSettings, SettingsStore, SpotiflacSettings};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;

fn sessions(state: &str, paused_s: f64) -> serde_json::Value {
    json!({"result": {"sessions": [
        {"key": "gateway-v2-abc-6836abb49bb0e8c9", "kind": "gateway", "label": "example-ext",
         "extensions": ["example-ext"], "state": state, "auth_paused_s": paused_s},
        {"key": "gateway-v2-abc-faf6adcf28256950", "kind": "gateway", "label": "example-ext-two",
         "extensions": ["example-ext-two"], "state": "active", "auth_paused_s": paused_s}
    ]}})
}

/// The dashboard page most tests pass from.
fn tab_a() -> Viewer {
    Viewer::new("tab-a", "page-1")
}

/// A second tab on the same dashboard.
fn tab_b() -> Viewer {
    Viewer::new("tab-b", "page-2")
}

fn services() -> serde_json::Value {
    json!({"result": {"services": [{"id": "ext:example-ext", "label": "Example", "extensions": ["example-ext"]}]}})
}

/// Answers `first` for the first `switch_after` calls, then `then`.
struct Turns {
    calls: AtomicUsize,
    switch_after: usize,
    first: serde_json::Value,
    then: serde_json::Value,
}

impl Respond for Turns {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let body = if n < self.switch_after {
            &self.first
        } else {
            &self.then
        };
        ResponseTemplate::new(200).set_body_json(body.clone())
    }
}

struct World {
    client: Arc<SpotiflacClient>,
    mornye: MockServer,
    helper: MockServer,
    music: tempfile::TempDir,
}

async fn world(
    sessions_after_arm: serde_json::Value,
    helper_state: serde_json::Value,
    job: serde_json::Value,
) -> World {
    world_with(sessions_after_arm, helper_state, job, None).await
}

/// The same, with one song's time fixed.
async fn world_with(
    sessions_after_arm: serde_json::Value,
    helper_state: serde_json::Value,
    job: serde_json::Value,
    song_timeout: Option<Duration>,
) -> World {
    let mornye = MockServer::builder().start().await;
    let helper = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/api/get_signed_sessions"))
        .respond_with(Turns {
            calls: AtomicUsize::new(0),
            switch_after: 2,
            first: sessions("unverified", 900.0),
            then: sessions_after_arm,
        })
        .mount(&mornye)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get_download_services"))
        .respond_with(ResponseTemplate::new(200).set_body_json(services()))
        .mount(&mornye)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "job-1", "status": "queued"})))
        .mount(&mornye)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/downloads/job-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(job))
        .mount(&mornye)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads/job-1/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "job-1", "status": "cancelled"})))
        .mount(&mornye)
        .await;
    Mock::given(method("POST"))
        .and(path("/assist/arm"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ended": [".gateway-v2-abc-b97a94207d0a.auth-backoff.json"], "gatewayAsked": []
        })))
        .mount(&helper)
        .await;
    Mock::given(method("GET"))
        .and(path("/assist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helper_state))
        .mount(&helper)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        spotiflac: SpotiflacSettings {
            enabled: true,
            url: mornye.uri(),
            token: "probe-token".into(),
            assist_url: helper.uri(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let mut client = SpotiflacClient::with_poll_interval(settings, Duration::from_millis(5))
        .with_assist_look_every(Duration::from_millis(20));
    if let Some(timeout) = song_timeout {
        client = client.with_song_timeout(timeout);
    }
    let client = Arc::new(client);
    client.check.store.remember(CheckSong {
        link: "https://open.spotify.com/track/a".into(),
        artist: "Artist".into(),
        title: "Title".into(),
        utc: Utc::now(),
    });
    World {
        client,
        mornye,
        helper,
        music: tempfile::tempdir().unwrap(),
    }
}

fn no_browser() -> serde_json::Value {
    json!({"browsers": [], "paused": []})
}

fn running_job() -> serde_json::Value {
    json!({"id": "job-1", "status": "running"})
}

async fn until_finished(run: &Arc<AssistRun>) -> RunState {
    for _ in 0..500 {
        if run.finished() {
            return run.state().0;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the run never ended: {:?}", run.state());
}

async fn requests_to(server: &MockServer, wanted: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == wanted)
        .count()
}

#[tokio::test]
async fn a_session_of_an_armed_gateway_turning_active_is_a_pass() {
    let w = world(sessions("active", 0.0), no_browser(), running_job()).await;
    let run = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .expect("starts");
    assert_eq!(run.extensions, vec!["example-ext"]);
    assert_eq!(until_finished(&run).await, RunState::Passed);
    assert_eq!(
        requests_to(&w.mornye, "/api/v1/downloads/job-1/cancel").await,
        1,
        "the song's job is cancelled"
    );
    let leftovers: Vec<_> = std::fs::read_dir(w.music.path().join(SPOTIFLAC_FOLDER))
        .unwrap()
        .collect();
    assert!(leftovers.is_empty(), "the job folder is removed");
}

#[tokio::test]
async fn another_session_already_active_on_the_gateway_is_not_a_pass() {
    // example-ext-two is active on the paused gateway from the start; only example-ext's own row
    // turning active counts.
    let w = world(
        sessions("unverified", 900.0),
        no_browser(),
        json!({"id": "job-1", "status": "failed", "error": "download_failed"}),
    )
    .await;
    let run = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .expect("starts");
    assert_eq!(until_finished(&run).await, RunState::NoCheck);
}

#[tokio::test]
async fn every_ended_pause_stamped_again_after_the_press_is_a_closed_check() {
    let stamped = json!({"browsers": [{"id": "24", "url": "https://gateway.invalid/challenge", "seq": 3, "width": 1279, "height": 756}],
        "paused": [{"file": ".gateway-v2-abc-b97a94207d0a.auth-backoff.json", "gateway": "gateway-v2-abc", "seconds": 900, "gatewayAsked": false, "stampedAfterArm": true}]});
    let w = world(sessions("unverified", 900.0), stamped, running_job()).await;
    let run = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .expect("starts");
    assert_eq!(until_finished(&run).await, RunState::Closed);
}

#[tokio::test]
async fn a_job_that_ends_with_no_browser_never_reached_the_check_and_its_song_is_not_asked_again() {
    let w = world(
        sessions("unverified", 900.0),
        no_browser(),
        json!({"id": "job-1", "status": "failed", "error": "download_failed"}),
    )
    .await;
    assert_eq!(w.client.check_songs_left(), 1);
    let run = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .expect("starts");
    assert_eq!(until_finished(&run).await, RunState::NoCheck);
    assert_eq!(w.client.check_songs_left(), 0);
    let again = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await;
    assert!(again.is_err(), "no song left to ask for");
}

#[tokio::test]
async fn pass_needs_a_paused_check_a_song_and_a_token() {
    let w = world(sessions("active", 0.0), no_browser(), running_job()).await;
    // The first two reads are paused (the stand-in's turns); spend them so the check reads clear.
    let _ = w.client.check_view(Some(Instant::now())).await;
    let _ = w.client.check_view(Some(Instant::now())).await;
    let refused = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .err()
        .expect("refused");
    assert_eq!(
        refused.0,
        "Mornye's check is not paused, so there is nothing to pass."
    );
    assert_eq!(
        requests_to(&w.helper, "/assist/arm").await,
        0,
        "nothing was armed"
    );

    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        spotiflac: SpotiflacSettings {
            enabled: true,
            url: w.mornye.uri(),
            token: String::new(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let open = Arc::new(SpotiflacClient::new(settings));
    let refused = open.start_pass("/music", &tab_a()).await.err().expect("refused");
    assert_eq!(
        refused.0,
        "Set a web token for Mornye (SPOTIFLAC_WEB_TOKEN) to pass its check from Octo."
    );
}

#[tokio::test]
async fn a_pause_the_gateway_asked_for_is_left_alone() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    w.helper.reset().await;
    Mock::given(method("POST"))
        .and(path("/assist/arm"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ended": [], "gatewayAsked": [{"gateway": "gateway-v2-abc", "until": "2026-10-30T14:05:00Z"}]
        })))
        .mount(&w.helper)
        .await;
    let refused = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .err()
        .expect("refused");
    assert!(
        refused.0.starts_with("The gateway asked Mornye to wait until"),
        "{}",
        refused.0
    );
    assert_eq!(
        requests_to(&w.mornye, "/api/v1/downloads").await,
        0,
        "no song was asked for"
    );
}

#[tokio::test]
async fn pointer_events_carry_a_number_the_server_counts() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    Mock::given(method("POST"))
        .and(path("/assist/input"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&w.helper)
        .await;
    assert!(
        w.client
            .assist_input("24", "mousePressed", 1.0, 1.0)
            .await
            .is_err(),
        "no run yet"
    );
    let _run = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .expect("starts");
    w.client
        .assist_input("24", "mouseMoved", 60.0, 70.0)
        .await
        .expect("passed on");
    w.client
        .assist_input("24", "mousePressed", 68.0, 72.0)
        .await
        .expect("passed on");
    assert!(
        w.client
            .assist_input("../x", "mousePressed", 1.0, 1.0)
            .await
            .is_err()
    );
    assert!(w.client.assist_input("24", "keyDown", 1.0, 1.0).await.is_err());
    let seqs: Vec<u64> = w
        .helper
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/assist/input")
        .map(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["seq"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(seqs, vec![1, 2]);
}

#[tokio::test]
async fn two_starts_at_once_arm_once_and_share_the_run() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let folder = w.music.path().to_string_lossy().into_owned();
    let (a, b) = (tab_a(), tab_b());
    let (first, second) = tokio::join!(w.client.start_pass(&folder, &a), w.client.start_pass(&folder, &b));
    let (first, second) = (first.expect("starts"), second.expect("answers the same run"));
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(requests_to(&w.helper, "/assist/arm").await, 1);
}

#[tokio::test]
async fn a_run_stopped_from_the_dashboard_cancels_its_song() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let run = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .expect("starts");
    // Once Mornye has the song, as a person who closes the panel finds it.
    for _ in 0..200 {
        if requests_to(&w.mornye, "/api/v1/downloads").await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(w.client.stop_pass(Some(&run.id), &tab_a()).await);
    assert_eq!(until_finished(&run).await, RunState::Unavailable);
    assert_eq!(run.state().1.as_deref(), Some("Stopped from the dashboard."));
    assert_eq!(requests_to(&w.mornye, "/api/v1/downloads/job-1/cancel").await, 1);
    assert!(
        !w.client.stop_pass(Some(&run.id), &tab_a()).await,
        "nothing live to stop"
    );
}

#[tokio::test]
async fn a_close_while_the_check_is_opening_stops_the_run_it_makes() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    // The helper takes a while to arm.
    w.helper.reset().await;
    Mock::given(method("POST"))
        .and(path("/assist/arm"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(
                    json!({"ended": [".gateway-v2-abc-b97a94207d0a.auth-backoff.json"], "gatewayAsked": []}),
                )
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&w.helper)
        .await;
    Mock::given(method("GET"))
        .and(path("/assist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(no_browser()))
        .mount(&w.helper)
        .await;
    let folder = w.music.path().to_string_lossy().into_owned();
    let client = Arc::clone(&w.client);
    let starting = tokio::spawn(async move { client.start_pass(&folder, &tab_a()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The page has no run name yet: its start has not answered.
    assert!(
        w.client.stop_pass(None, &tab_a()).await,
        "the stop waits for the start, then stops its run"
    );
    let run = starting.await.expect("ran").expect("started");
    assert_eq!(until_finished(&run).await, RunState::Unavailable);
}

#[tokio::test]
async fn a_pass_right_after_a_close_opens_a_new_run() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let folder = w.music.path().to_string_lossy().into_owned();
    let first = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
    assert!(w.client.stop_pass(Some(&first.id), &tab_a()).await);
    let second = w
        .client
        .start_pass(&folder, &tab_a())
        .await
        .expect("starts again");
    assert!(!Arc::ptr_eq(&first, &second), "not the run being stopped");
    assert_ne!(first.id, second.id);
    assert!(first.finished());
    assert_eq!(requests_to(&w.helper, "/assist/arm").await, 2);
}

#[tokio::test]
async fn a_close_on_one_page_leaves_the_run_another_page_is_passing() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let folder = w.music.path().to_string_lossy().into_owned();
    let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
    let joined = w.client.start_pass(&folder, &tab_b()).await.expect("joins");
    assert!(Arc::ptr_eq(&run, &joined));
    assert!(
        !w.client.stop_pass(Some(&run.id), &tab_a()).await,
        "the other page is still on it"
    );
    assert!(!run.finished());
    assert!(
        w.client.stop_pass(Some(&run.id), &tab_b()).await,
        "the last page leaves"
    );
    assert_eq!(until_finished(&run).await, RunState::Unavailable);
}

#[tokio::test]
async fn a_reloaded_page_takes_its_old_seat_so_its_close_still_stops_the_run() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let folder = w.music.path().to_string_lossy().into_owned();
    let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
    // The page reloads and its leaving stop never reaches Octo; the new page passes again.
    let reloaded = Viewer::new("tab-a", "page-1-reloaded");
    let joined = w.client.start_pass(&folder, &reloaded).await.expect("joins");
    assert!(Arc::ptr_eq(&run, &joined));
    assert!(
        w.client.stop_pass(Some(&run.id), &reloaded).await,
        "one tab, one seat: its Close stops the run"
    );
    assert_eq!(until_finished(&run).await, RunState::Unavailable);
}

#[tokio::test]
async fn a_late_stop_from_the_page_before_a_reload_leaves_the_new_page_on() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let folder = w.music.path().to_string_lossy().into_owned();
    let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
    let reloaded = Viewer::new("tab-a", "page-1-reloaded");
    w.client.start_pass(&folder, &reloaded).await.expect("joins");
    // The old page's leaving stop arrives after the new page passed.
    assert!(!w.client.stop_pass(Some(&run.id), &tab_a()).await);
    assert!(!run.finished(), "the new page is still on it");
}

#[tokio::test]
async fn a_stop_for_an_earlier_run_leaves_the_run_open_now() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let folder = w.music.path().to_string_lossy().into_owned();
    let first = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
    assert!(w.client.stop_pass(Some(&first.id), &tab_a()).await);
    let second = w
        .client
        .start_pass(&folder, &tab_a())
        .await
        .expect("starts again");
    // The first Close, sent again late, names the first run.
    assert!(!w.client.stop_pass(Some(&first.id), &tab_a()).await);
    assert!(!second.finished(), "the run open now goes on");
}

#[tokio::test]
async fn a_follower_that_stops_partway_cancels_its_job_then_ends_the_run() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let folder = w.music.path().to_string_lossy().into_owned();
    let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
    let dir = tempfile::tempdir().expect("a folder");
    let job_dir = dir.path().join("octo-check-guard");
    std::fs::create_dir_all(&job_dir).expect("made");
    let mut ends = EndsTheRun::new(&run, &w.client);
    ends.dir = Some(job_dir.to_string_lossy().into_owned());
    ends.job = Some("job-guard".to_string());

    // As when the task following the run panics partway through.
    drop(ends);

    assert!(!job_dir.exists(), "its folder goes at once");
    assert_eq!(until_finished(&run).await, RunState::Unavailable);
    assert_eq!(
        requests_to(&w.mornye, "/api/v1/downloads/job-guard/cancel").await,
        1,
        "the job was cancelled before the run read as ended"
    );
    assert_eq!(
        run.state().1.as_deref(),
        Some("Octo stopped following Mornye's check.")
    );
}

#[test]
fn a_run_whose_follower_is_dropped_partway_ends_and_leaves_no_folder() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let (w, run) = runtime.block_on(async {
        let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
        let folder = w.music.path().to_string_lossy().into_owned();
        let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
        // Once Mornye has the song, so the follower has made its folder and holds the job.
        for _ in 0..200 {
            if requests_to(&w.mornye, "/api/v1/downloads").await > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (w, run)
    });
    assert!(!run.finished());

    // The runtime goes, and the follower's task with it, partway through the run.
    drop(runtime);

    assert!(run.finished(), "{:?}", run.state());
    assert_eq!(
        run.state().1.as_deref(),
        Some("Octo stopped following Mornye's check.")
    );
    let folders = std::fs::read_dir(w.music.path().join(SPOTIFLAC_FOLDER))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("octo-check-"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(folders, 0, "the job folder is removed");
    // A new Pass would not join it.
    assert!(w.client.assist_run().is_none_or(|run| run.finished()));
}

#[tokio::test]
async fn a_run_ends_once_the_song_takes_longer_than_a_song_may() {
    let w = world_with(
        sessions("unverified", 900.0),
        no_browser(),
        running_job(),
        Some(Duration::from_millis(150)),
    )
    .await;
    let run = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .expect("starts");
    assert_eq!(until_finished(&run).await, RunState::Unavailable);
    assert!(
        run.state()
            .1
            .unwrap_or_default()
            .starts_with("Mornye took longer than")
    );
    assert_eq!(requests_to(&w.mornye, "/api/v1/downloads/job-1/cancel").await, 1);
}

#[tokio::test]
async fn a_song_is_not_counted_as_tried_when_the_helper_missed_a_look() {
    let w = world(
        sessions("unverified", 900.0),
        no_browser(),
        json!({"id": "job-1", "status": "failed", "error": "download_failed"}),
    )
    .await;
    w.helper.reset().await;
    Mock::given(method("POST"))
        .and(path("/assist/arm"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ended": [".gateway-v2-abc-b97a94207d0a.auth-backoff.json"], "gatewayAsked": []
        })))
        .mount(&w.helper)
        .await;
    Mock::given(method("GET"))
        .and(path("/assist"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&w.helper)
        .await;
    let run = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .expect("starts");
    assert_eq!(until_finished(&run).await, RunState::Unavailable);
    assert_eq!(
        w.client.check_songs_left(),
        1,
        "the song can still open the check"
    );
}
