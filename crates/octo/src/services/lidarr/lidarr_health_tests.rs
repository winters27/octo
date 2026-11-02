//! Lidarr's indexers, against the fake Lidarr: when every pick that searched grabbed nothing and
//! Lidarr is the chain's last source, Lidarr runs its health checks and its health decides whether
//! the heart holds, goes on with its line, or simply missed; a pick that never searched, or a chain
//! with a source after Lidarr, never asks. A heart waiting for the indexers runs again once they
//! search again: on Lidarr's webhook, on any call Octo makes to Lidarr, and at the import poll pace,
//! only while a heart waits.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::*;

/// Lidarr's health with one check.
fn health(source: &str, kind: &str, fragment: &str) -> Value {
    json!([{
        "source": source,
        "type": kind,
        "message": format!("{source} says so"),
        "wikiUrl": format!("https://wiki.servarr.com/lidarr/system#{fragment}"),
    }])
}

const FAILURES: &str = "indexers-are-unavailable-due-to-failures";

/// Whether Octo asked Lidarr about its health: it ran Lidarr's health checks and read what they
/// found. (A pick also has Lidarr queue a health check, for the time Lidarr stamps on it, and never
/// reads what it found.)
fn asked_for_health(f: &Fixture) -> bool {
    let checked = f
        .lidarr
        .changes()
        .iter()
        .filter_map(|change| change.body.clone())
        .any(|body| body["name"] == "CheckHealth");
    checked && health_reads(f) > 0
}

/// How often Octo read Lidarr's health.
fn health_reads(f: &Fixture) -> usize {
    f.lidarr
        .requests()
        .iter()
        .filter(|request| request.method == "GET" && request.path.starts_with("/api/v1/health"))
        .count()
}

/// Teardrop's row as the downloads page has it.
fn row(f: &Fixture) -> Option<crate::services::common::acquisition_tracker::AcquisitionSnapshot> {
    f.tracker.detail("soulseek:teardrop-heart", None)
}

/// With Lidarr last, a search that grabbed nothing and Lidarr's indexers failing (every one at
/// error, every one failing for long, or every one that searches unavailable): Lidarr runs its
/// health checks, the heart waits for the indexers, and its row is not failed.
#[tokio::test]
async fn failing_indexers_after_a_search_that_grabbed_nothing_hold_the_heart_with_lidarr_last() {
    for (source, kind, fragment) in [
        ("IndexerStatusCheck", "error", FAILURES),
        (
            "IndexerLongTermStatusCheck",
            "error",
            "indexers-are-unavailable-due-to-failures-for-more-than-6-hours",
        ),
        ("IndexerSearchCheck", "warning", FAILURES),
    ] {
        let f = Fixture::new().await;
        f.lidarr.set_health(health(source, kind, fragment));
        let service = f.service(None, 30);

        assert!(!f.heart_teardrop(&service).await, "{source}");

        assert!(asked_for_health(&f), "{source}");
        assert_eq!(
            service.waits_on("soulseek", "teardrop-heart"),
            Some(LidarrWaits::Indexers),
            "{source}"
        );
        assert_ne!(
            row(&f).map(|row| row.state),
            Some(AcquisitionState::Failed),
            "{source}"
        );
        assert!(!service.left_alone("soulseek", "teardrop-heart"));
    }
}

/// No indexer has automatic search on: the heart does not wait, Lidarr counts as unable to look
/// for the song, and its row says why.
#[tokio::test]
async fn no_indexer_that_searches_sends_the_heart_on_with_its_line() {
    let f = Fixture::new().await;
    f.lidarr.set_health(health(
        "IndexerSearchCheck",
        "warning",
        "no-indexers-available-with-automatic-search-enabled-lidarr-will-not-provide-any-automatic-search-results",
    ));
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);

    assert!(asked_for_health(&f));
    assert_eq!(service.waits_on("soulseek", "teardrop-heart"), None);
    assert!(service.left_alone("soulseek", "teardrop-heart"));
    assert_eq!(
        row(&f).and_then(|row| row.error).as_deref(),
        Some(NO_INDEXER_SEARCHES)
    );
}

/// Indexers that only search interactively are no failure: the search simply missed.
#[tokio::test]
async fn an_indexer_that_only_searches_interactively_is_healthy() {
    let f = Fixture::new().await;
    f.lidarr.set_health(health(
        "IndexerSearchCheck",
        "warning",
        "no-indexers-available-with-interactive-search-enabled",
    ));
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);

    assert!(asked_for_health(&f));
    assert_eq!(service.waits_on("soulseek", "teardrop-heart"), None);
    assert!(!service.left_alone("soulseek", "teardrop-heart"));
    assert_eq!(
        row(&f).map(|row| row.state),
        Some(AcquisitionState::Failed),
        "a plain miss"
    );
}

/// A heart for an artist that already has music in Lidarr, while the indexers fail: nothing is
/// searched, so Lidarr is never asked about its health and nothing waits; the heart goes on at
/// once (and keeps the heard copy when Lidarr is last).
#[tokio::test]
async fn a_heart_for_an_artist_with_files_while_the_indexers_fail_sends_no_check_health() {
    let f = Fixture::new().await;
    f.lidarr.catalog_album(protection());
    f.lidarr.user_album("rg-protection", false);
    f.lidarr.import(40, "Karmacoma", "FLAC");
    f.lidarr
        .set_health(health("IndexerStatusCheck", "error", FAILURES));
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);

    assert_eq!(f.lidarr.searches(), 0);
    assert!(!asked_for_health(&f));
    assert_eq!(health_reads(&f), 0);
    assert_eq!(service.waits_on("soulseek", "teardrop-heart"), None);
    assert!(service.left_alone("soulseek", "teardrop-heart"));
}

/// With a source after Lidarr the chain goes on, and with a grab that failed the search did find
/// something: neither asks Lidarr about its health.
#[tokio::test]
async fn a_source_after_lidarr_or_a_search_that_grabbed_asks_no_health() {
    let f = Fixture::new().await;
    f.lidarr
        .set_health(health("IndexerStatusCheck", "error", FAILURES));
    let service = f.service(None, 30);
    assert!(
        !within(service.try_acquire_track(
            "soulseek",
            "teardrop-heart",
            false,
            Some("alice"),
            AcquisitionOrigin::Heart,
            &[],
        ))
        .await
    );
    assert!(!asked_for_health(&f));
    assert_eq!(service.waits_on("soulseek", "teardrop-heart"), None);

    let f = Fixture::new().await;
    f.lidarr
        .set_health(health("IndexerStatusCheck", "error", FAILURES));
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_track(
                "soulseek",
                "teardrop-heart",
                true,
                Some("alice"),
                AcquisitionOrigin::Heart,
                &[],
            ))
            .await
        })
    };
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| entry.owns_grab("dl-1"))
    })
    .await;
    f.lidarr.fail("dl-1");
    assert!(!heart.await.expect("the task"));
    assert!(!asked_for_health(&f));
    assert_eq!(service.waits_on("soulseek", "teardrop-heart"), None);
}

/// A heart left waiting for Lidarr's indexers, with who runs the waiting hearts again counted.
async fn waiting_for_indexers(f: &Fixture) -> (LidarrHeartAcquisitionService, Arc<AtomicUsize>) {
    f.lidarr
        .set_health(health("IndexerStatusCheck", "error", FAILURES));
    let service = f.service(None, 30);
    let back = Arc::new(AtomicUsize::new(0));
    let counted = back.clone();
    service.on_indexers_back(
        Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }),
        None,
    );
    assert!(!f.heart_teardrop(&service).await);
    assert_eq!(
        service.waits_on("soulseek", "teardrop-heart"),
        Some(LidarrWaits::Indexers)
    );
    (service, back)
}

/// The watch reads the indexers only when nudged: its own pace is an hour.
async fn only_nudged(f: &Fixture, service: &LidarrHeartAcquisitionService, back: &AtomicUsize) {
    service.set_poll_override(Some(Duration::from_secs(3600)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.lidarr.set_health(json!([]));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(back.load(Ordering::SeqCst), 0, "nothing read the indexers yet");
}

/// Lidarr's `HealthRestored` (or `Grab`) webhook reads the indexers at once: they search again, so
/// every heart waiting for them runs again.
#[tokio::test]
async fn lidarrs_health_restored_or_grab_webhook_runs_the_waiting_hearts_again() {
    let f = Fixture::new().await;
    let (service, back) = waiting_for_indexers(&f).await;
    only_nudged(&f, &service, &back).await;

    service.lidarr_health_event();

    until(|| back.load(Ordering::SeqCst) == 1).await;
}

/// Any call Octo makes to Lidarr, here reading an artist, reads the indexers again.
#[tokio::test]
async fn any_lidarr_call_octo_makes_reads_the_indexers_again() {
    let f = Fixture::new().await;
    let (service, back) = waiting_for_indexers(&f).await;
    only_nudged(&f, &service, &back).await;

    service
        .artist_music()
        .read(ARTIST_ID)
        .await
        .expect("Lidarr answers");

    until(|| back.load(Ordering::SeqCst) == 1).await;
}

/// With nothing else happening, the indexers are read at the import poll pace while a heart waits
/// for them, and the waiting hearts run again once they search.
#[tokio::test]
async fn the_indexers_are_read_at_the_poll_pace_while_a_heart_waits() {
    let f = Fixture::new().await;
    let (_service, back) = waiting_for_indexers(&f).await;
    let reads = health_reads(&f);
    until(|| health_reads(&f) > reads + 2).await;
    assert_eq!(back.load(Ordering::SeqCst), 0, "still failing");

    f.lidarr.set_health(json!([]));

    until(|| back.load(Ordering::SeqCst) == 1).await;
    let reads = health_reads(&f);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(health_reads(&f), reads, "no reads once they search again");
}

/// Once nobody waits for the indexers (every heart taken back), their health is no longer read.
#[tokio::test]
async fn nobody_waiting_for_the_indexers_stops_the_reads() {
    let f = Fixture::new().await;
    f.lidarr
        .set_health(health("IndexerStatusCheck", "error", FAILURES));
    let service = f.service(None, 30);
    let back = Arc::new(AtomicUsize::new(0));
    let counted = back.clone();
    service.on_indexers_back(
        Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }),
        Some(Arc::new(|| false)),
    );
    assert!(!f.heart_teardrop(&service).await);
    assert_eq!(
        service.waits_on("soulseek", "teardrop-heart"),
        Some(LidarrWaits::Indexers)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let reads = health_reads(&f);

    f.lidarr.set_health(json!([]));
    service.lidarr_health_event();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(health_reads(&f), reads);
    assert_eq!(back.load(Ordering::SeqCst), 0);
}

/// A heart told to wait for the indexers while the watch decides to stop keeps the watch going:
/// whether anyone waits and whether a watch runs change together, so no hold is ever left without
/// a watch, and the next hold after a stop starts a new one.
#[test]
fn a_hold_that_comes_while_the_watch_decides_to_stop_keeps_it_going() {
    let watch = super::super::health::IndexerWatch::default();
    assert!(watch.hold(), "the first hold starts a watch");
    let seen = watch.told();

    assert!(!watch.hold(), "a watch runs already");
    assert!(!watch.stop(seen), "a hold came while it looked");

    let seen = watch.told();
    assert!(watch.stop(seen));
    assert!(watch.hold(), "the next hold starts a new watch");
}
