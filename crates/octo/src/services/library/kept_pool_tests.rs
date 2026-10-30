//! One order for every item a heart's proof checks again: files kept as heard, hearts held "not
//! yet proven" and hearted albums' held tracks. Each proof checks exactly one item, the one
//! checked longest ago across all three kinds (never checked counts as longest; ties go to the
//! one kept or held longest), and an item checked by any other event goes to the back.

use chrono::TimeZone;
use octo_core::settings::AppSettings;
use url::Url;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::fingerprint::{AcoustIdRateLimitHandler, AcoustIdRateLimiter};
use crate::services::library::library_action_test_support::RecordingLibrary;

struct Pool {
    _dir: tempfile::TempDir,
    acoust_id: MockServer,
    rechecks: Arc<KeptRechecks>,
    /// The holds checked again, in order.
    held_checks: Arc<Mutex<Vec<String>>>,
}

impl Pool {
    async fn kept_lookups(&self) -> usize {
        self.acoust_id.received_requests().await.unwrap_or_default().len()
    }
}

fn at(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 29, 12, minute, 0).unwrap()
}

/// A kept-as-heard file kept at minute 10, a heart held "not yet proven" since minute 5 and a
/// hearted album's held track since minute 20.
async fn pool() -> Pool {
    let acoust_id = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"status":"ok","results":[]}"#, "application/json"),
        )
        .mount(&acoust_id)
        .await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("kept.m4a").to_string_lossy().replace('\\', "/");
    std::fs::write(&path, b"the audio that played").expect("written");
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let limiter = Arc::new(
        AcoustIdRateLimiter::with_gaps(Duration::from_millis(1), Duration::from_millis(1))
            .with_jitter(|| 1.0),
    );
    let base = Url::parse(&format!("{}/", acoust_id.uri())).expect("parses");
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::with_base_url(limiter, base)),
        settings.clone(),
        Some("octo-key"),
    ));
    let library = Arc::new(RecordingLibrary::default());
    library.mappings.lock().push(LocalSongMapping {
        external_provider: "soulseek".into(),
        external_id: "kept".into(),
        local_path: path,
        downloaded_at: at(10),
        heart: Some(KeptHeart {
            users: vec!["brandon".into()],
            as_heard: true,
            print: Some("AQADtEqk".into()),
            seconds: Some(139),
            ..Default::default()
        }),
        ..Default::default()
    });
    let rechecks = Arc::new(KeptRechecks::new(
        library,
        client,
        settings,
        Arc::new(ExternalIdRegistry::in_memory()),
    ));
    let held_checks: Arc<Mutex<Vec<String>>> = Arc::default();
    let told = held_checks.clone();
    rechecks.use_holds(
        Arc::new(|| {
            vec![
                ("Track:soulseek:album-track:NotYetProven".to_string(), at(20)),
                ("Track:soulseek:held-heart:NotYetProven".to_string(), at(5)),
            ]
        }),
        Arc::new(move |key: &str| told.lock().push(key.to_string())),
    );
    Pool {
        _dir: dir,
        acoust_id,
        rechecks,
        held_checks,
    }
}

/// Each proof checks exactly one item, the oldest across the three kinds, never one of each kind:
/// the held heart (minute 5), then the kept file (minute 10), then the held album track (minute
/// 20), then the held heart again.
#[tokio::test]
async fn each_proof_checks_exactly_one_item_the_oldest_across_every_kind() {
    let pool = pool().await;

    pool.rechecks.after_a_proof().await;
    assert_eq!(
        pool.held_checks.lock().as_slice(),
        ["Track:soulseek:held-heart:NotYetProven"]
    );
    assert_eq!(pool.kept_lookups().await, 0, "one item, not one of each kind");

    pool.rechecks.after_a_proof().await;
    assert_eq!(pool.held_checks.lock().len(), 1);
    assert_eq!(pool.kept_lookups().await, 1, "the kept file's one lookup");

    pool.rechecks.after_a_proof().await;
    assert_eq!(
        pool.held_checks.lock().as_slice(),
        [
            "Track:soulseek:held-heart:NotYetProven",
            "Track:soulseek:album-track:NotYetProven"
        ]
    );
    assert_eq!(pool.kept_lookups().await, 1);

    pool.rechecks.after_a_proof().await;
    assert_eq!(
        pool.held_checks.lock().last().map(String::as_str),
        Some("Track:soulseek:held-heart:NotYetProven"),
        "every item had its turn: the one checked longest ago is next"
    );
}

/// An item another event checked (a play, a heart, the start) goes to the back: the next proof
/// checks the one checked longest ago instead.
#[tokio::test]
async fn an_item_another_event_checked_goes_to_the_back() {
    let pool = pool().await;
    pool.rechecks
        .checked_hold("Track:soulseek:held-heart:NotYetProven");

    pool.rechecks.after_a_proof().await;

    assert!(
        pool.held_checks.lock().is_empty(),
        "the held heart was just checked"
    );
    assert_eq!(
        pool.kept_lookups().await,
        1,
        "the kept file, never checked, goes first"
    );
}
