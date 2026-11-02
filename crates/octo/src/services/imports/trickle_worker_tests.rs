//! The trickle worker.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use octo_core::settings::{AppSettings, ImportSettings};

use super::super::ImportTrack;
use super::super::trickle_queue::tests::{moving_clock, song};
use super::*;

struct Rig {
    now: Arc<Mutex<DateTime<Utc>>>,
    queue: Arc<TrickleQueue>,
    settings: Arc<SettingsStore>,
    worker: Arc<TrickleWorker>,
    rows: Arc<Mutex<HashMap<String, AcquisitionSnapshot>>>,
    started: Arc<Mutex<Vec<String>>>,
    finished: Arc<Mutex<Vec<TrickleJob>>>,
    busy: Arc<AtomicBool>,
    soulseek_out: Arc<AtomicBool>,
    mornye_down: Arc<AtomicBool>,
    mornye_out: Arc<AtomicBool>,
    held: Arc<Mutex<HashSet<String>>>,
}

type Owned = Arc<dyn Fn(&TrickleJob) -> Option<String> + Send + Sync>;
type Outcome = Arc<dyn Fn(&TrickleJob) -> Option<AcquisitionState> + Send + Sync>;

fn row(key: &str, state: AcquisitionState) -> AcquisitionSnapshot {
    AcquisitionSnapshot {
        id: key.into(),
        provider: "soulseek".into(),
        external_id: key.into(),
        artist: Some("Artist".into()),
        title: Some("Song".into()),
        requested_by: vec!["alice".into()],
        source: Some("Soulseek".into()),
        state,
        started_at: Utc::now(),
        updated_at: Utc::now(),
        error: (state == AcquisitionState::Failed).then(|| "Nobody on Soulseek had it.".to_string()),
        library_id: (state == AcquisitionState::Done).then(|| format!("nd-{key}")),
        ..Default::default()
    }
}

fn settings(per_hour: i32) -> AppSettings {
    AppSettings {
        imports: ImportSettings {
            songs_per_hour: per_hour,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn rig_with(owned: Owned, outcome: Outcome) -> Rig {
    let (now, clock) = moving_clock();
    let queue = Arc::new(TrickleQueue::default());
    queue.set_clock(clock.clone());
    let settings = Arc::new(SettingsStore::for_tests(settings(12)));
    let rows: Arc<Mutex<HashMap<String, AcquisitionSnapshot>>> = Arc::default();
    let started: Arc<Mutex<Vec<String>>> = Arc::default();
    let busy = Arc::new(AtomicBool::new(false));
    let out = Arc::new(AtomicBool::new(false));
    let mornye_down = Arc::new(AtomicBool::new(false));
    let mornye_out = Arc::new(AtomicBool::new(false));
    let held: Arc<Mutex<HashSet<String>>> = Arc::default();
    let seams = TrickleSeams {
        owned: Arc::new(move |job| {
            let found = owned(&job).map(|id| OwnedSong(Some(id)));
            Box::pin(async move { Ok(found) })
        }),
        start: {
            let (queue, rows, started) = (queue.clone(), rows.clone(), started.clone());
            Arc::new(move |job| {
                let key = format!("soulseek:{}", job.key);
                queue.update(&job.owner, &job.key, |j| j.acquisition_key = Some(key.clone()));
                started.lock().push(job.key.clone());
                if let Some(state) = outcome(&job) {
                    rows.lock().insert(key.clone(), row(&key, state));
                }
                Box::pin(async move { Ok(key) })
            })
        },
        row: {
            let rows = rows.clone();
            Arc::new(move |key| rows.lock().get(key).cloned())
        },
        people_downloading: {
            let busy = busy.clone();
            Arc::new(move || busy.load(Ordering::SeqCst))
        },
        soulseek_out: {
            let out = out.clone();
            Arc::new(move || {
                let value = out.load(Ordering::SeqCst);
                Box::pin(async move { value })
            })
        },
        mornye_down: {
            let down = mornye_down.clone();
            Arc::new(move || {
                let value = down.load(Ordering::SeqCst);
                Box::pin(async move { value })
            })
        },
        mornye_out: {
            let out = mornye_out.clone();
            Arc::new(move || {
                let value = out.load(Ordering::SeqCst);
                Box::pin(async move { value })
            })
        },
        held: {
            let held = held.clone();
            Arc::new(move |jobs: Vec<TrickleJob>| {
                let keys = held.lock().clone();
                let places: HashSet<usize> = jobs
                    .iter()
                    .enumerate()
                    .filter(|(_, job)| keys.contains(&job.key))
                    .map(|(at, _)| at)
                    .collect();
                Box::pin(async move { places })
            })
        },
        clock,
    };
    let worker = TrickleWorker::new(queue.clone(), settings.clone(), seams);
    let finished: Arc<Mutex<Vec<TrickleJob>>> = Arc::default();
    let seen = finished.clone();
    worker.on_finished(Arc::new(move |job| seen.lock().push(job.clone())));
    Rig {
        now,
        queue,
        settings,
        worker,
        rows,
        started,
        finished,
        busy,
        soulseek_out: out,
        mornye_down,
        mornye_out,
        held,
    }
}

fn rig() -> Rig {
    rig_with(Arc::new(|_| None), Arc::new(|_| Some(AcquisitionState::Done)))
}

impl Rig {
    async fn tick(&self) -> bool {
        let started = self.worker.tick().await;
        self.worker.drain().await;
        started
    }

    fn add(&self, keys: &[&str]) {
        let songs: Vec<ImportTrack> = keys.iter().map(|k| song(k)).collect();
        self.queue.add("alice", &songs);
    }

    fn advance(&self, by: TimeDelta) {
        *self.now.lock() += by;
    }
}

#[tokio::test]
async fn songs_start_no_closer_than_the_setting_allows() {
    let rig = rig();
    rig.add(&["a", "b"]);
    assert!(rig.tick().await);
    assert!(!rig.tick().await);
    rig.advance(TimeDelta::minutes(4));
    assert!(!rig.tick().await);
    let status = rig.worker.status_for("alice");
    assert_eq!(status.state, trickle_states::RUNNING);
    assert_eq!(status.next_utc, Some(*rig.now.lock() + TimeDelta::minutes(1)));
    rig.advance(TimeDelta::minutes(1));
    assert!(rig.tick().await);
    assert_eq!(*rig.started.lock(), ["a", "b"]);
}

#[tokio::test]
async fn a_fetched_song_is_done_with_its_library_id() {
    let rig = rig();
    rig.add(&["a"]);
    rig.tick().await;
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::DONE);
    assert_eq!(job.library_id.as_deref(), Some("nd-soulseek:a"));
    assert_eq!(job.detail.as_deref(), Some("Fetched from Soulseek"));
    assert_eq!(rig.finished.lock().len(), 1);
}

#[tokio::test]
async fn a_song_that_throws_says_why_in_plain_words_with_no_server_path() {
    let rig = rig();
    rig.worker.configure(|seams| {
        seams.start = Arc::new(|_| {
            Box::pin(async {
                Err(anyhow::anyhow!(
                    "Could not find file '/data/config/octo/history.json'. See the inner exception.\n   at Octo.Something()"
                ))
            })
        });
    });
    rig.add(&["a"]);
    rig.tick().await;
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::NOT_FOUND);
    let detail = job.detail.unwrap_or_default();
    assert!(!detail.contains("/data/config"), "{detail}");
    assert!(!detail.contains("Octo.Something"), "{detail}");
}

#[tokio::test]
async fn a_song_already_in_the_library_is_never_downloaded() {
    let rig = rig_with(
        Arc::new(|_| Some("nd-owned".to_string())),
        Arc::new(|_| Some(AcquisitionState::Done)),
    );
    rig.add(&["a"]);
    rig.tick().await;
    assert!(rig.started.lock().is_empty());
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::DONE);
    assert_eq!(job.library_id.as_deref(), Some("nd-owned"));
    assert_eq!(job.detail.as_deref(), Some("Already in your library"));
}

#[tokio::test]
async fn a_song_no_source_had_is_not_found_with_the_reason() {
    let rig = rig_with(Arc::new(|_| None), Arc::new(|_| Some(AcquisitionState::Failed)));
    rig.add(&["a"]);
    rig.tick().await;
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::NOT_FOUND);
    assert_eq!(job.detail.as_deref(), Some("Nobody on Soulseek had it."));
}

#[tokio::test]
async fn a_hand_off_to_lidarr_is_followed_without_holding_the_next_song_back() {
    let rig = rig_with(
        Arc::new(|_| None),
        Arc::new(|job| {
            Some(if job.key == "a" {
                AcquisitionState::Downloading
            } else {
                AcquisitionState::Done
            })
        }),
    );
    rig.add(&["a", "b"]);
    rig.tick().await;
    assert_eq!(
        rig.queue.get("alice", "a").expect("a").state,
        import_track_states::DOWNLOADING
    );

    rig.advance(TimeDelta::minutes(5));
    assert!(rig.tick().await);
    assert_eq!(
        rig.queue.get("alice", "b").expect("b").state,
        import_track_states::DONE
    );
    assert_eq!(
        rig.queue.get("alice", "a").expect("a").state,
        import_track_states::DOWNLOADING
    );

    rig.rows
        .lock()
        .insert("soulseek:a".into(), row("soulseek:a", AcquisitionState::Done));
    rig.tick().await;
    assert_eq!(
        rig.queue.get("alice", "a").expect("a").state,
        import_track_states::DONE
    );
}

#[tokio::test]
async fn it_waits_while_peoples_own_downloads_run_and_while_soulseek_is_out() {
    let rig = rig();
    rig.busy.store(true, Ordering::SeqCst);
    rig.add(&["a"]);
    assert!(!rig.tick().await);
    assert_eq!(rig.worker.status_for("alice").state, trickle_states::YIELDING);

    rig.busy.store(false, Ordering::SeqCst);
    rig.soulseek_out.store(true, Ordering::SeqCst);
    assert!(!rig.tick().await);
    assert_eq!(
        rig.worker.status_for("alice").state,
        trickle_states::WAITING_FOR_SOULSEEK
    );

    rig.soulseek_out.store(false, Ordering::SeqCst);
    assert!(rig.tick().await);
}

#[tokio::test]
async fn it_waits_while_mornyes_check_keeps_the_only_source() {
    let rig = rig();
    rig.mornye_out.store(true, Ordering::SeqCst);
    rig.add(&["a"]);
    assert!(!rig.tick().await);
    assert_eq!(
        rig.worker.status_for("alice").state,
        trickle_states::WAITING_FOR_MORNYE_CHECK
    );
    assert_eq!(
        trickle_states::label(trickle_states::WAITING_FOR_MORNYE_CHECK, false),
        "Waiting for Mornye's check"
    );
    rig.mornye_out.store(false, Ordering::SeqCst);
    assert!(rig.tick().await);
}

#[tokio::test]
async fn it_waits_while_mornye_is_the_only_source_and_does_not_answer() {
    let rig = rig();
    rig.mornye_down.store(true, Ordering::SeqCst);
    rig.add(&["a"]);
    assert!(!rig.tick().await);
    assert!(rig.started.lock().is_empty(), "no song is started to fail");
    assert_eq!(
        rig.worker.status_for("alice").state,
        trickle_states::WAITING_FOR_MORNYE
    );
    assert_eq!(
        trickle_states::label(trickle_states::WAITING_FOR_MORNYE, false),
        "Waiting for Mornye to answer"
    );
    rig.mornye_down.store(false, Ordering::SeqCst);
    assert!(rig.tick().await);
}

#[tokio::test]
async fn mornye_is_down_for_the_step_only_when_it_is_on_slskd_is_not_set_up_and_it_cannot_stand_in() {
    use crate::services::common::test_fakes::{OffLink, OfflineLink};
    let clear = mornye("active").await;
    let down = |url: String, enabled: bool, link: Arc<dyn ISoulseekLink>| async move {
        let mut all = AppSettings::default();
        all.spotiflac.enabled = enabled;
        all.spotiflac.url = url;
        let settings = Arc::new(SettingsStore::for_tests(all));
        let client = SpotiflacClient::new(Arc::clone(&settings));
        mornye_down_with_no_slskd(&settings, link.as_ref(), Some(&client)).await
    };
    assert!(down("http://127.0.0.1:1".into(), true, Arc::new(OffLink)).await);
    assert!(
        !down(clear.uri(), true, Arc::new(OffLink)).await,
        "an answering Mornye carries the step"
    );
    assert!(
        !down("http://127.0.0.1:1".into(), false, Arc::new(OffLink)).await,
        "with Mornye off, imports run as they did without it"
    );
    assert!(
        !down("http://127.0.0.1:1".into(), true, Arc::new(OfflineLink)).await,
        "a set-up slskd has its own wait"
    );
}

/// A Mornye whose one gateway session is in `state`.
async fn mornye(state: &str) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::builder().start().await;
    let paused_s = if state == "active" { 0.0 } else { 900.0 };
    Mock::given(method("POST"))
        .and(path("/api/get_signed_sessions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"sessions": [
                {"key": "gateway-v2-abc-6836abb49bb0e8c9", "kind": "gateway", "label": "Example",
                 "extensions": ["example-ext"], "state": state, "auth_paused_s": paused_s}
            ]}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/get_download_services"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"services": [
                {"id": "ext:example-ext", "label": "Example", "extensions": ["example-ext"]}
            ]}})),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn mornye_keeps_the_step_only_when_it_is_the_source_left_and_its_check_is_paused() {
    use crate::services::common::test_fakes::{OffLink, OfflineLink};
    let paused = mornye("unverified").await;
    let clear = mornye("active").await;
    let store = |url: &str, lidarr_after: bool| {
        let mut all = AppSettings::default();
        all.spotiflac.enabled = true;
        all.spotiflac.url = url.into();
        if lidarr_after {
            all.subsonic.heart_download_sources = vec![
                octo_core::settings::HeartDownloadStep {
                    source: HeartDownloadSource::Soulseek,
                    song_enabled: Some(true),
                    ..Default::default()
                },
                octo_core::settings::HeartDownloadStep {
                    source: HeartDownloadSource::Lidarr,
                    song_enabled: Some(true),
                    ..Default::default()
                },
            ];
        }
        Arc::new(SettingsStore::for_tests(all))
    };
    let keeps = |settings: Arc<SettingsStore>, link: Arc<dyn ISoulseekLink>| async move {
        let client = SpotiflacClient::new(Arc::clone(&settings));
        mornye_keeps_the_step(&settings, link.as_ref(), Some(&client)).await
    };
    assert!(keeps(store(&paused.uri(), false), Arc::new(OffLink)).await);
    assert!(keeps(store(&paused.uri(), false), Arc::new(OfflineLink)).await);
    assert!(
        !keeps(store(&clear.uri(), false), Arc::new(OffLink)).await,
        "the check passes"
    );
    assert!(
        !keeps(store(&paused.uri(), true), Arc::new(OffLink)).await,
        "Lidarr comes after"
    );
    assert!(
        !keeps(store("http://127.0.0.1:1", false), Arc::new(OffLink)).await,
        "Mornye does not answer"
    );
}

#[tokio::test]
async fn a_mornye_that_cannot_download_does_not_stand_in_for_a_logged_out_slskd() {
    use crate::services::common::test_fakes::OfflineLink;
    let clear = mornye("active").await;
    let soulseek_keeps = |url: String| async move {
        let mut all = AppSettings::default();
        all.spotiflac.enabled = true;
        all.spotiflac.url = url;
        let settings = Arc::new(SettingsStore::for_tests(all));
        let client = SpotiflacClient::new(Arc::clone(&settings));
        soulseek_keeps_the_step(&settings, &OfflineLink, Some(&client)).await
    };
    assert!(
        !soulseek_keeps(clear.uri()).await,
        "an answering Mornye stands in"
    );
    assert!(
        soulseek_keeps("http://127.0.0.1:1".into()).await,
        "one that does not answer leaves the wait for Soulseek"
    );
}

#[tokio::test]
async fn mornye_off_leaves_the_wait_for_soulseek_as_it_was_and_a_logged_in_slskd_never_asks_mornye() {
    use crate::services::common::test_fakes::{LoggedInLink, OfflineLink};
    let server = wiremock::MockServer::builder().start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let settings = |enabled: bool| {
        let mut all = AppSettings::default();
        all.spotiflac.enabled = enabled;
        all.spotiflac.url = server.uri();
        Arc::new(SettingsStore::for_tests(all))
    };

    // Off: a logged-out slskd keeps the step exactly as with no Mornye, and no check keeps it.
    let off = settings(false);
    let client = SpotiflacClient::new(Arc::clone(&off));
    assert!(soulseek_keeps_the_step(&off, &OfflineLink, Some(&client)).await);
    assert!(soulseek_keeps_the_step(&off, &OfflineLink, None).await);
    assert!(!mornye_keeps_the_step(&off, &OfflineLink, Some(&client)).await);

    // On, with slskd logged in: nothing keeps the step, and Mornye is not asked.
    let on = settings(true);
    let client = SpotiflacClient::new(Arc::clone(&on));
    assert!(!soulseek_keeps_the_step(&on, &LoggedInLink, Some(&client)).await);
    assert!(!mornye_keeps_the_step(&on, &LoggedInLink, Some(&client)).await);
    server.verify().await;
}

#[tokio::test]
async fn none_an_hour_or_a_pause_starts_nothing() {
    let rig = rig();
    rig.settings.set(settings(0));
    rig.add(&["a"]);
    assert!(!rig.tick().await);
    assert_eq!(rig.worker.status_for("alice").state, trickle_states::OFF);

    rig.settings.set(settings(12));
    rig.queue.set_paused("alice", true);
    assert!(!rig.tick().await);
    assert_eq!(rig.worker.status_for("alice").state, trickle_states::PAUSED);
    assert!(rig.started.lock().is_empty());
}

#[tokio::test]
async fn a_download_nobody_reports_on_is_checked_again() {
    let rig = rig_with(Arc::new(|_| None), Arc::new(|_| None));
    rig.add(&["a"]);
    rig.tick().await;
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::QUEUED);
    assert_eq!(job.detail.as_deref(), Some("Checking again"));
}

#[tokio::test]
async fn the_last_look_finds_a_copy_only_the_isrc_names_and_trusts_the_name_check_first() {
    use super::super::import_matcher::{ImportMatcher, IsrcRecording};
    use super::super::test_support::row;
    use std::sync::atomic::AtomicUsize;

    let mut tagged = row(
        "nd-roads",
        "Portishead",
        "Track 05",
        Some(305),
        Some("Dummy"),
        "flac",
        900,
    );
    tagged.recording_id = Some("rec-roads".into());
    let pages = [tagged];
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let matcher = ImportMatcher::with_pages(Arc::new(move |start, count| {
        let page: Vec<_> = pages
            .iter()
            .skip(start as usize)
            .take(count as usize)
            .cloned()
            .collect();
        let n = page.len();
        Box::pin(async move { Some((page, n)) })
    }))
    .with_isrc_recordings(Arc::new(move |_isrc: String| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Some(vec![IsrcRecording {
                id: "rec-roads".into(),
                title: "Roads".into(),
                seconds: Some(305),
            }])
        })
    }));
    let job = TrickleJob {
        owner: "alice".into(),
        key: "k-roads".into(),
        title: "Roads".into(),
        artist: "Portishead".into(),
        seconds: Some(305),
        isrc: Some("GBAAA9400010".into()),
        ..Default::default()
    };

    // The name check found it: no question asked.
    let named = last_look(Some(OwnedSong(Some("by-name".into()))), Some(&matcher), &job)
        .await
        .expect("an answer");
    assert_eq!(named, Some(OwnedSong(Some("by-name".into()))));
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 0);

    // It did not: the ISRC's recordings find the numbered file.
    let found = last_look(None, Some(&matcher), &job).await.expect("an answer");
    assert_eq!(found, Some(OwnedSong(Some("nd-roads".into()))));
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);

    // No matcher (no music database): nothing more to look by.
    assert_eq!(last_look(None, None, &job).await.expect("an answer"), None);
}

#[tokio::test]
async fn a_song_whose_isrc_could_not_be_checked_is_always_put_back() {
    use super::super::import_matcher::ImportMatcher;
    let matcher = ImportMatcher::with_pages(Arc::new(|_start, _count| {
        Box::pin(async { Some((Vec::new(), 0)) })
    }))
    .with_isrc_recordings(Arc::new(|_isrc: String| Box::pin(async { None })));
    let mut job = TrickleJob {
        owner: "alice".into(),
        key: "k-roads".into(),
        title: "Roads".into(),
        artist: "Portishead".into(),
        isrc: Some("GBAAA9400010".into()),
        attempts: 1,
        ..Default::default()
    };
    let later = last_look(None, Some(&matcher), &job).await.expect_err("put back");
    assert!(later.downcast_ref::<TrickleCheckLater>().is_some(), "{later}");
    let later = later.downcast_ref::<TrickleCheckLater>().expect("a check later");
    assert_eq!(later.detail, WAITING_FOR_ANSWER);
    assert!(
        later.turn_back,
        "only songs like it wait, so it gives its turn back"
    );
    job.attempts = TrickleWorker::MAX_ATTEMPTS;
    let still = last_look(None, Some(&matcher), &job)
        .await
        .expect_err("still put back");
    assert!(
        still.downcast_ref::<TrickleCheckLater>().is_some(),
        "never fetched unchecked"
    );
}

#[tokio::test]
async fn a_job_put_back_for_a_check_is_queued_again_and_says_why() {
    let rig = rig();
    rig.worker.configure(|seams| {
        seams.owned = Arc::new(|_| {
            Box::pin(async {
                Err(TrickleCheckLater {
                    detail: CHECKING.to_string(),
                    turn_back: true,
                }
                .into())
            })
        });
    });
    rig.add(&["a"]);
    rig.tick().await;
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::QUEUED);
    assert_eq!(job.detail.as_deref(), Some(CHECKING));
    assert_eq!(job.attempts, 0, "a put-back is not an attempt");
    assert!(rig.started.lock().is_empty(), "nothing was fetched");
}

#[tokio::test]
async fn three_turns_while_the_music_database_is_busy_fetch_nothing() {
    let rig = rig();
    rig.worker.configure(|seams| {
        seams.owned = Arc::new(|_| {
            Box::pin(async {
                Err(TrickleCheckLater {
                    detail: CHECKING.to_string(),
                    turn_back: true,
                }
                .into())
            })
        });
    });
    rig.add(&["a"]);
    for _ in 0..3 {
        rig.tick().await;
        rig.advance(TimeDelta::minutes(10));
    }
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::QUEUED);
    assert_eq!(job.attempts, 0);
    assert!(rig.started.lock().is_empty(), "nothing was started");
}

#[tokio::test]
async fn songs_waiting_for_musicbrainz_are_held_and_the_rest_keep_going() {
    let looked = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = looked.clone();
    let rig = rig_with(
        Arc::new(move |job| {
            seen.lock().push(job.key.clone());
            None
        }),
        Arc::new(|_| Some(AcquisitionState::Done)),
    );
    let told = Arc::new(Mutex::new(Vec::<String>::new()));
    let heard = told.clone();
    rig.worker
        .on_held(Arc::new(move |owner| heard.lock().push(owner.to_string())));
    rig.held.lock().insert("a".into());
    rig.add(&["a", "b"]);

    // "b" has nothing to wait for and goes; "a" waits and says why.
    assert!(rig.tick().await);
    assert_eq!(*rig.started.lock(), ["b"]);
    let a = rig.queue.get("alice", "a").expect("a");
    assert_eq!(a.state, import_track_states::QUEUED);
    assert_eq!(a.detail.as_deref(), Some(WAITING_FOR_ANSWER));
    assert!(told.lock().iter().any(|owner| owner == "alice"));

    // Only "a" is left: the trickle waits for MusicBrainz and never looks at it again.
    for _ in 0..3 {
        rig.advance(TimeDelta::minutes(10));
        assert!(!rig.tick().await);
    }
    assert_eq!(
        *looked.lock(),
        ["b"],
        "a held song is not checked again while held"
    );
    assert_eq!(
        rig.worker.status_for("alice").state,
        trickle_states::WAITING_FOR_MUSIC_DATABASE
    );

    // MusicBrainz answers: the hold ends and the words come off.
    rig.held.lock().clear();
    rig.queue.set_paused("alice", true);
    assert!(!rig.tick().await);
    assert_eq!(rig.queue.get("alice", "a").expect("a").detail, None);
    assert_eq!(rig.worker.status_for("alice").state, trickle_states::PAUSED);
    rig.queue.set_paused("alice", false);
    assert!(rig.tick().await);
    assert_eq!(*rig.started.lock(), ["b", "a"]);
}

#[tokio::test]
async fn some_songs_held_still_reads_as_running() {
    let rig = rig();
    rig.held.lock().insert("a".into());
    rig.add(&["a", "b", "c"]);
    assert!(rig.tick().await);
    assert_eq!(rig.worker.status_for("alice").state, trickle_states::RUNNING);
}

#[tokio::test]
async fn a_song_taken_back_while_it_is_checked_never_starts_and_never_comes_back() {
    let rig = rig();
    let queue = rig.queue.clone();
    rig.worker.configure(move |seams| {
        seams.owned = Arc::new(move |job| {
            queue.cancel_queued(&job.owner, None);
            Box::pin(async { Ok(None) })
        });
    });
    rig.add(&["a"]);
    rig.tick().await;
    assert!(rig.started.lock().is_empty(), "nothing started");
    assert!(rig.queue.get("alice", "a").is_none(), "taken back");

    let again = self::rig();
    let queue = again.queue.clone();
    again.worker.configure(move |seams| {
        seams.owned = Arc::new(move |job| {
            queue.cancel_queued(&job.owner, None);
            Box::pin(async {
                Err(TrickleCheckLater {
                    detail: WAITING_FOR_ANSWER.to_string(),
                    turn_back: true,
                }
                .into())
            })
        });
    });
    again.add(&["a"]);
    again.tick().await;
    assert!(
        again.queue.get("alice", "a").is_none(),
        "a put-back brings nothing back"
    );
}

#[tokio::test]
async fn a_song_starts_with_its_check_words_cleared_and_its_attempt_counted() {
    let rig = rig();
    let at_start: Arc<Mutex<Option<TrickleJob>>> = Arc::default();
    let (queue, seen) = (rig.queue.clone(), at_start.clone());
    let checking = rig.queue.clone();
    rig.worker.configure(move |seams| {
        seams.owned = Arc::new(move |job| {
            checking.update(&job.owner, &job.key, |j| j.detail = Some(CHECKING.to_string()));
            Box::pin(async { Ok(None) })
        });
        seams.start = Arc::new(move |job| {
            *seen.lock() = queue.get(&job.owner, &job.key);
            Box::pin(async { Err(anyhow::anyhow!("Stopped for the test.")) })
        });
    });
    rig.add(&["a"]);
    rig.tick().await;
    let started = at_start.lock().clone().expect("it started");
    assert_eq!(started.detail, None);
    assert_eq!(started.attempts, 1);
}

#[tokio::test]
async fn a_song_put_back_while_the_library_cannot_be_read_waits_for_its_next_turn() {
    let rig = rig();
    rig.worker.configure(|seams| {
        seams.owned = Arc::new(|_| {
            Box::pin(async {
                Err(TrickleCheckLater {
                    detail: READING_LIBRARY.to_string(),
                    turn_back: false,
                }
                .into())
            })
        });
    });
    rig.add(&["a"]);
    assert!(rig.tick().await);
    assert!(!rig.tick().await, "no second look until its next turn");
    assert_eq!(
        rig.queue.get("alice", "a").expect("a").detail.as_deref(),
        Some(READING_LIBRARY)
    );
    rig.advance(TimeDelta::minutes(5));
    assert!(rig.tick().await);
}

#[tokio::test]
async fn the_last_look_waits_to_read_the_library_when_it_cannot() {
    use super::super::import_matcher::ImportMatcher;
    let matcher = ImportMatcher::with_pages(Arc::new(|_start, _count| Box::pin(async { None })))
        .with_isrc_recordings(Arc::new(|_isrc: String| Box::pin(async { Some(Vec::new()) })));
    let job = TrickleJob {
        owner: "alice".into(),
        key: "k-roads".into(),
        title: "Roads".into(),
        artist: "Portishead".into(),
        isrc: Some("GBAAA9400010".into()),
        ..Default::default()
    };
    let later = last_look(None, Some(&matcher), &job).await.expect_err("put back");
    let later = later.downcast_ref::<TrickleCheckLater>().expect("a check later");
    assert_eq!(later.detail, READING_LIBRARY);
    assert!(!later.turn_back, "every song would wait the same way");
}

#[tokio::test]
async fn another_persons_song_with_the_same_key_and_no_isrc_is_never_held() {
    use super::super::import_matcher::ImportMatcher;
    let matcher = Arc::new(
        ImportMatcher::with_pages(Arc::new(|_start, _count| {
            Box::pin(async { Some((Vec::new(), 0)) })
        }))
        .with_isrc_recordings(Arc::new(|_isrc: String| Box::pin(async { None }))),
    );
    let coded = ImportTrack {
        key: "song:portishead roads".into(),
        title: "Roads".into(),
        artist: "Portishead".into(),
        seconds: Some(305),
        isrc: Some("GBAAA9400010".into()),
        ..Default::default()
    };
    let plain = ImportTrack {
        isrc: None,
        ..coded.clone()
    };
    matcher.learn_isrcs(std::slice::from_ref(&coded)).await;
    let rig = rig();
    let seam = held_by(Some(matcher));
    rig.worker.configure(move |seams| seams.held = seam);
    rig.queue.add("alice", std::slice::from_ref(&coded));
    rig.queue.add("bob", std::slice::from_ref(&plain));
    assert!(rig.tick().await);
    assert_eq!(
        rig.queue.get("bob", &plain.key).expect("bob's").state,
        import_track_states::DONE
    );
    assert_eq!(
        rig.queue.get("alice", &coded.key).expect("alice's").state,
        import_track_states::QUEUED
    );
    assert_eq!(
        rig.worker.status_for("bob").state,
        trickle_states::IDLE,
        "nothing of bob's waits on MusicBrainz"
    );
}

#[tokio::test]
async fn every_song_held_reads_as_waiting_for_musicbrainz_even_while_others_download() {
    let rig = rig();
    rig.held.lock().insert("a".into());
    rig.add(&["a"]);
    rig.tick().await;
    rig.busy.store(true, Ordering::SeqCst);
    rig.advance(TimeDelta::minutes(10));
    rig.tick().await;
    assert_eq!(
        rig.worker.status_for("alice").state,
        trickle_states::WAITING_FOR_MUSIC_DATABASE
    );
}

#[tokio::test]
async fn a_song_found_in_the_library_during_its_check_is_not_taken_back_by_a_cancel() {
    let rig = rig_with(
        Arc::new(|_| Some("nd-owned".to_string())),
        Arc::new(|_| Some(AcquisitionState::Done)),
    );
    rig.add(&["a", "b"]);
    rig.tick().await;
    let slot = rig.queue.last_start_utc();
    assert_eq!(rig.queue.cancel_queued("alice", None), 1, "only b, still queued");
    assert_eq!(
        rig.queue.get("alice", "a").expect("a").state,
        import_track_states::DONE
    );
    assert_eq!(rig.queue.last_start_utc(), slot, "the used turn stays used");
}

#[tokio::test]
async fn a_song_put_back_while_a_tick_works_out_the_hold_is_not_taken_again_in_that_tick() {
    let rig = rig();
    let gate = Arc::new(tokio::sync::Notify::new());
    let waited = gate.clone();
    rig.worker.configure(move |seams| {
        seams.owned = Arc::new(move |_| {
            let waited = waited.clone();
            Box::pin(async move {
                waited.notified().await;
                Err(TrickleCheckLater {
                    detail: WAITING_FOR_ANSWER.to_string(),
                    turn_back: true,
                }
                .into())
            })
        });
    });
    rig.add(&["a", "b"]);
    assert!(rig.worker.tick().await, "a is taken");
    let (queue, release) = (rig.queue.clone(), gate.clone());
    rig.worker.configure(move |seams| {
        seams.held = Arc::new(move |jobs| {
            let (queue, release) = (queue.clone(), release.clone());
            Box::pin(async move {
                // Lets the song's check end, and waits until it is put back.
                release.notify_one();
                let mut put_back = false;
                for _ in 0..10_000 {
                    if queue
                        .get("alice", "a")
                        .is_some_and(|job| job.state == import_track_states::QUEUED)
                    {
                        put_back = true;
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                assert!(put_back, "a was put back");
                jobs.iter()
                    .enumerate()
                    .filter(|(_, job)| job.key == "b")
                    .map(|(at, _)| at)
                    .collect::<HashSet<usize>>()
            })
        });
    });
    assert!(
        !rig.worker.tick().await,
        "the tick that began while a ran does not take it again"
    );
    assert_eq!(
        rig.queue.get("alice", "a").expect("a").state,
        import_track_states::QUEUED
    );
    assert_eq!(
        rig.queue.get("alice", "b").expect("b").state,
        import_track_states::QUEUED
    );
}

#[tokio::test]
async fn a_song_queued_while_the_hold_is_worked_out_is_checked_before_it_starts() {
    let rig = rig();
    rig.held.lock().insert("x".into());
    rig.held.lock().insert("late".into());
    rig.add(&["x"]);
    let (queue, added) = (rig.queue.clone(), Arc::new(AtomicBool::new(false)));
    let keys = rig.held.clone();
    rig.worker.configure(move |seams| {
        seams.held = Arc::new(move |jobs: Vec<TrickleJob>| {
            // The first look at the queue: a song arrives right after it.
            if !added.swap(true, Ordering::SeqCst) {
                queue.add("alice", &[song("late")]);
            }
            let keys = keys.lock().clone();
            let places: HashSet<usize> = jobs
                .iter()
                .enumerate()
                .filter(|(_, job)| keys.contains(&job.key))
                .map(|(at, _)| at)
                .collect();
            Box::pin(async move { places })
        });
    });
    assert!(!rig.tick().await, "nothing starts");
    assert!(rig.started.lock().is_empty());
    let late = rig.queue.get("alice", "late").expect("late");
    assert_eq!(late.state, import_track_states::QUEUED);
    assert_eq!(late.detail.as_deref(), Some(WAITING_FOR_ANSWER));
}

#[test]
fn every_state_has_words_and_an_unknown_one_reads_as_work() {
    use trickle_states::*;
    assert_eq!(label(IDLE, false), "Nothing to fetch");
    assert_eq!(label(RUNNING, true), "Fetching");
    assert_eq!(label(RUNNING, false), "Waiting for the next turn");
    assert_eq!(
        label(WAITING_FOR_MUSIC_DATABASE, false),
        "Waiting for MusicBrainz"
    );
    assert_eq!(label("somethingNew", false), "Working on it");
}
