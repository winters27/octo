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
async fn a_follower_that_panics_cancels_the_job_it_took_then_ends_the_run() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    // Mornye takes a while to answer the cancel, so what happens meanwhile is seen.
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads/job-1/cancel"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({}))
                .set_delay(Duration::from_millis(300)),
        )
        .with_priority(1)
        .mount(&w.mornye)
        .await;
    w.client.assist.stop_after_queue.store(true, Ordering::SeqCst);
    let folder = w.music.path().to_string_lossy().into_owned();
    let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");

    let mut cancelled = false;
    for _ in 0..300 {
        if requests_to(&w.mornye, "/api/v1/downloads/job-1/cancel").await > 0 {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cancelled, "the job the follower took is cancelled");
    assert!(!run.finished(), "the run ends only once the cancel has answered");
    assert_eq!(octo_check_folders(&w), 0, "its folder went at once");
    let first = w.client.check.store.pass_jobs();
    assert_eq!(first.len(), 1, "kept until the cancel is answered");

    // A Pass while that cancel is on its way waits, then opens a run of its own.
    w.client.assist.stop_after_queue.store(false, Ordering::SeqCst);
    let next = w
        .client
        .start_pass(&folder, &tab_b())
        .await
        .expect("starts again");
    assert!(!Arc::ptr_eq(&run, &next));
    assert_eq!(run.state().0, RunState::Unavailable);
    assert_eq!(
        run.state().1.as_deref(),
        Some("Octo stopped following Mornye's check.")
    );
    assert!(
        !w.client.check.store.pass_jobs().contains(&first[0]),
        "the cancelled job is no longer kept"
    );
    assert_eq!(
        requests_to(&w.mornye, "/api/v1/downloads/job-1/cancel").await,
        1,
        "the guard's cancel was taken, so no read asks again"
    );
}

#[tokio::test]
async fn a_guard_whose_cancel_mornye_refuses_keeps_the_job_for_a_later_read() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads/job-1/cancel"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&w.mornye)
        .await;
    w.client.assist.stop_after_queue.store(true, Ordering::SeqCst);
    let folder = w.music.path().to_string_lossy().into_owned();
    let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");

    assert_eq!(until_finished(&run).await, RunState::Unavailable);
    assert_eq!(
        w.client
            .check
            .store
            .pass_jobs()
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        vec!["job-1"]
    );
}

/// Mornye refuses to cancel job-1 because it no longer runs.
async fn refuse_cancel_of_job_1(mornye: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads/job-1/cancel"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error": "not running"})))
        .with_priority(1)
        .mount(mornye)
        .await;
}

#[tokio::test]
async fn a_run_forgets_its_job_once_mornye_says_it_is_over_though_it_refuses_the_cancel() {
    let w = world(
        sessions("unverified", 900.0),
        no_browser(),
        json!({"id": "job-1", "status": "failed", "error": "download_failed"}),
    )
    .await;
    refuse_cancel_of_job_1(&w.mornye).await;
    let run = w
        .client
        .start_pass(&w.music.path().to_string_lossy(), &tab_a())
        .await
        .expect("starts");

    assert_eq!(until_finished(&run).await, RunState::NoCheck);
    assert_eq!(requests_to(&w.mornye, "/api/v1/downloads/job-1/cancel").await, 1);
    assert!(
        w.client.check.store.pass_jobs().is_empty(),
        "{:?}",
        w.client.check.store.pass_jobs()
    );
}

#[tokio::test]
async fn a_guard_forgets_a_job_mornye_already_ended_though_it_refuses_the_cancel() {
    let w = world(
        sessions("unverified", 900.0),
        no_browser(),
        json!({"id": "job-1", "status": "failed", "error": "download_failed"}),
    )
    .await;
    refuse_cancel_of_job_1(&w.mornye).await;
    w.client.assist.stop_after_queue.store(true, Ordering::SeqCst);
    let folder = w.music.path().to_string_lossy().into_owned();
    let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");

    assert_eq!(until_finished(&run).await, RunState::Unavailable);
    assert_eq!(requests_to(&w.mornye, "/api/v1/downloads/job-1/cancel").await, 1);
    assert!(
        w.client.check.store.pass_jobs().is_empty(),
        "{:?}",
        w.client.check.store.pass_jobs()
    );
}

/// A Pass run's song folder name, as Octo makes them.
fn check_folder_name() -> String {
    format!("octo-check-{}", uuid::Uuid::new_v4().simple())
}

/// The song job a restart in the middle of a Pass leaves in mornye-check.json, for `client`'s
/// Mornye, in a folder made under `dir`.
fn left_job(client: &SpotiflacClient, dir: &std::path::Path) -> PassJob {
    let job_dir = dir.join(check_folder_name());
    std::fs::create_dir_all(&job_dir).expect("made");
    PassJob {
        url: Connection::of(&client.settings()).url,
        id: "left-job".into(),
        dir: job_dir.to_string_lossy().into_owned(),
    }
}

/// Runs the cleanup worker until the returned token is cancelled.
fn cleanup_worker(client: &Arc<SpotiflacClient>) -> tokio_util::sync::CancellationToken {
    let stopping = tokio_util::sync::CancellationToken::new();
    tokio::spawn(Arc::clone(client).clean_left_pass_jobs(stopping.clone()));
    stopping
}

#[tokio::test]
async fn a_read_that_reaches_mornye_has_the_worker_end_a_job_a_stopped_run_left() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let dir = tempfile::tempdir().expect("a folder");
    let left = left_job(&w.client, dir.path());
    w.client.check.store.keep_pass_job(left.clone());
    let stopping = cleanup_worker(&w.client);

    let _ = w.client.check_view(Some(Instant::now())).await;
    for _ in 0..200 {
        if !w.client.check.store.keeps_pass_jobs() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stopping.cancel();

    assert_eq!(
        requests_to(&w.mornye, "/api/v1/downloads/left-job/cancel").await,
        1
    );
    assert!(!std::path::Path::new(&left.dir).exists());
    assert!(w.client.check.store.pass_jobs().is_empty());
}

#[tokio::test]
async fn a_left_job_still_working_after_a_refused_cancel_stays_kept_and_a_gone_one_is_forgotten() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads/left-job/cancel"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&w.mornye)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/downloads/left-job"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": "left-job", "status": "running"})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&w.mornye)
        .await;
    let dir = tempfile::tempdir().expect("a folder");
    let left = left_job(&w.client, dir.path());
    w.client.check.store.keep_pass_job(left.clone());

    w.client.end_left_pass_jobs().await;
    assert_eq!(
        w.client.check.store.pass_jobs(),
        vec![left.clone()],
        "a busy Mornye with the job still running is asked again"
    );
    assert!(std::path::Path::new(&left.dir).exists());

    // Mornye no longer knows the job (404 to both calls): it is over.
    w.client.end_left_pass_jobs().await;
    assert!(w.client.check.store.pass_jobs().is_empty());
    assert!(!std::path::Path::new(&left.dir).exists());
}

#[tokio::test]
async fn a_left_job_mornye_will_not_cancel_because_it_already_failed_is_forgotten() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads/left-job/cancel"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error": "not running"})))
        .with_priority(1)
        .mount(&w.mornye)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/downloads/left-job"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "left-job", "status": "failed"})))
        .with_priority(1)
        .mount(&w.mornye)
        .await;
    let dir = tempfile::tempdir().expect("a folder");
    let left = left_job(&w.client, dir.path());
    w.client.check.store.keep_pass_job(left.clone());

    w.client.end_left_pass_jobs().await;

    assert!(w.client.check.store.pass_jobs().is_empty());
    assert!(!std::path::Path::new(&left.dir).exists());
}

#[tokio::test]
async fn a_left_job_mornye_will_not_cancel_yet_stays_kept_beside_a_new_runs_job() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads/left-job/cancel"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&w.mornye)
        .await;
    let dir = tempfile::tempdir().expect("a folder");
    let left = left_job(&w.client, dir.path());
    w.client.check.store.keep_pass_job(left.clone());
    let folder = w.music.path().to_string_lossy().into_owned();

    let _run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
    let mut kept = Vec::new();
    for _ in 0..200 {
        kept = w.client.check.store.pass_jobs();
        if kept.len() > 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(kept.contains(&left), "{kept:?}");
    assert!(kept.iter().any(|job| job.id == "job-1"), "{kept:?}");
}

#[tokio::test]
async fn a_left_job_for_a_mornye_now_elsewhere_is_let_go_without_a_cancel() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let dir = tempfile::tempdir().expect("a folder");
    let left = PassJob {
        url: "http://old-mornye:8010".into(),
        ..left_job(&w.client, dir.path())
    };
    w.client.check.store.keep_pass_job(left.clone());

    w.client.end_left_pass_jobs().await;

    assert_eq!(
        requests_to(&w.mornye, "/api/v1/downloads/left-job/cancel").await,
        0
    );
    assert!(w.client.check.store.pass_jobs().is_empty());
    assert!(!std::path::Path::new(&left.dir).exists());
}

#[tokio::test]
async fn a_left_job_naming_a_folder_octo_did_not_make_leaves_that_folder() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let dir = tempfile::tempdir().expect("a folder");
    let theirs = dir.path().join("my-music");
    std::fs::create_dir_all(&theirs).expect("made");
    let left = PassJob {
        dir: theirs.to_string_lossy().into_owned(),
        ..left_job(&w.client, dir.path())
    };
    w.client.check.store.keep_pass_job(left);

    w.client.end_left_pass_jobs().await;

    assert!(w.client.check.store.pass_jobs().is_empty(), "the job is over");
    assert!(theirs.exists(), "a folder Octo did not name stays");
}

/// A Mornye whose check reads answer after `read_delay` and whose cancel of `left-job` answers
/// after `cancel_delay`, and a client that waits at most a fifth of a second for either.
async fn slow_mornye(read_delay: Duration, cancel_delay: Duration) -> (MockServer, Arc<SpotiflacClient>) {
    let mornye = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/api/get_signed_sessions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(sessions("active", 0.0))
                .set_delay(read_delay),
        )
        .mount(&mornye)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get_download_services"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(services())
                .set_delay(read_delay),
        )
        .mount(&mornye)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/downloads/left-job/cancel"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({}))
                .set_delay(cancel_delay),
        )
        .mount(&mornye)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        spotiflac: SpotiflacSettings {
            enabled: true,
            url: mornye.uri(),
            token: "probe-token".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let check = crate::services::spotiflac::MornyeCheck::new(
        Arc::new(crate::services::spotiflac::MornyeCheckStore::new(None)),
        crate::services::spotiflac::CheckTimes {
            fresh_for: Duration::ZERO,
            call_bound: Duration::from_millis(200),
        },
        None,
    );
    (mornye, Arc::new(SpotiflacClient::new(settings).with_check(check)))
}

#[tokio::test]
async fn a_read_leaves_left_jobs_to_the_worker_and_asks_no_cancel_itself() {
    let (mornye, client) = slow_mornye(Duration::ZERO, Duration::ZERO).await;
    let dir = tempfile::tempdir().expect("a folder");
    let left = left_job(&client, dir.path());
    client.check.store.keep_pass_job(left.clone());

    // No worker runs here: whatever the read itself does is all that happens.
    let view = client.check_view(Some(Instant::now())).await.expect("a read");

    assert!(view.reached);
    assert_eq!(requests_to(&mornye, "/api/v1/downloads/left-job/cancel").await, 0);
    assert_eq!(client.check.store.pass_jobs(), vec![left]);
}

#[tokio::test]
async fn a_sweep_gives_up_on_a_slow_cancel_within_its_bound_and_keeps_the_job() {
    let (mornye, client) = slow_mornye(Duration::ZERO, Duration::from_secs(2)).await;
    let dir = tempfile::tempdir().expect("a folder");
    let left = left_job(&client, dir.path());
    client.check.store.keep_pass_job(left.clone());

    let started = Instant::now();
    client.end_left_pass_jobs().await;

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(requests_to(&mornye, "/api/v1/downloads/left-job/cancel").await, 1);
    assert_eq!(
        client.check.store.pass_jobs(),
        vec![left],
        "asked again by a later sweep"
    );
}

#[tokio::test]
async fn a_read_mornye_does_not_answer_wakes_no_cleanup() {
    let (mornye, client) = slow_mornye(Duration::from_secs(1), Duration::ZERO).await;
    let dir = tempfile::tempdir().expect("a folder");
    let left = left_job(&client, dir.path());
    client.check.store.keep_pass_job(left.clone());
    let stopping = cleanup_worker(&client);

    let view = client.check_view(Some(Instant::now())).await.expect("a read");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stopping.cancel();

    assert!(!view.reached);
    assert_eq!(requests_to(&mornye, "/api/v1/downloads/left-job/cancel").await, 0);
    assert_eq!(client.check.store.pass_jobs(), vec![left]);
}

#[tokio::test]
async fn the_song_job_of_a_live_run_is_kept_and_left_alone_by_reads() {
    let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
    let folder = w.music.path().to_string_lossy().into_owned();
    let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
    let mut kept = None;
    for _ in 0..200 {
        kept = w.client.check.store.pass_jobs().into_iter().next();
        if kept.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(kept.map(|job| job.id), Some("job-1".to_string()));

    w.client.end_left_pass_jobs().await;

    assert!(!run.finished());
    assert_eq!(
        requests_to(&w.mornye, "/api/v1/downloads/job-1/cancel").await,
        0,
        "the run is still on it"
    );
    assert_eq!(w.client.check.store.pass_jobs().len(), 1);
}

/// The Pass job folders in the world's music folder.
fn octo_check_folders(w: &World) -> usize {
    std::fs::read_dir(w.music.path().join(SPOTIFLAC_FOLDER))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("octo-check-"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn a_run_whose_follower_is_dropped_partway_ends_and_leaves_no_folder() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let (w, run, reached) = runtime.block_on(async {
        let w = world(sessions("unverified", 900.0), no_browser(), running_job()).await;
        let folder = w.music.path().to_string_lossy().into_owned();
        let run = w.client.start_pass(&folder, &tab_a()).await.expect("starts");
        // Once Mornye has the song, so the follower has made its folder and holds the job.
        let mut reached = false;
        for _ in 0..200 {
            if requests_to(&w.mornye, "/api/v1/downloads").await > 0 {
                reached = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (w, run, reached)
    });
    assert!(reached, "Mornye was asked for the song");
    assert_eq!(octo_check_folders(&w), 1, "the follower made its folder");
    assert!(!run.finished());

    // The runtime goes, and the follower's task with it, partway through the run.
    drop(runtime);

    assert!(run.finished(), "{:?}", run.state());
    assert_eq!(
        run.state().1.as_deref(),
        Some("Octo stopped following Mornye's check.")
    );
    assert_eq!(octo_check_folders(&w), 0, "the job folder is removed");
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
