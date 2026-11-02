use std::sync::Arc;

use octo_core::settings::{AppSettings, SettingsStore};

use super::*;
use crate::services::apple::{ApplePace, PaceConfig};
use crate::services::metadata::Reach;
use crate::services::metadata::deezer_reach::{Infos, InfosReader};

fn server_store(chart_country: &str, pages: Option<&str>) -> (ServerStore, Arc<DeezerReach>) {
    let mut settings = AppSettings::default();
    settings.generated_playlists.chart_country = chart_country.to_string();
    let reach = Arc::new(DeezerReach::new(Arc::new(ApplePace::new(PaceConfig::default()))));
    let pages = pages.map(str::to_string);
    let store = ServerStore::new(
        Arc::new(SettingsStore::for_tests(settings)),
        Arc::new(move || pages.clone()),
        // Apple Music has every country but "zz".
        Arc::new(|code: &str| code != "zz"),
        Arc::clone(&reach),
    );
    (store, reach)
}

#[tokio::test]
async fn the_charts_country_pins_the_store_when_apple_has_it() {
    let (pinned, _) = server_store("GB", Some("in"));
    assert_eq!(pinned.pinned().as_deref(), Some("gb"));
    let (automatic, _) = server_store("auto", Some("in"));
    assert_eq!(automatic.pinned(), None);
    let (missing, _) = server_store("ZZ", Some("in"));
    assert_eq!(missing.pinned(), None, "a country Apple Music lacks pins nothing");
}

/// Answers `/infos` once `hold` is notified; never, when nothing notifies it.
struct Held(Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl InfosReader for Held {
    async fn read(&self, _background: bool) -> Infos {
        self.0.notified().await;
        Infos::Closed {
            country_iso: "IN".into(),
            country: "India".into(),
        }
    }
}

#[tokio::test]
async fn learning_waits_for_infos_while_the_reach_is_unknown() {
    let hold = Arc::new(tokio::sync::Notify::new());
    let (store, reach) = server_store("auto", Some("FR"));
    reach.set_reader(Arc::new(Held(Arc::clone(&hold))));
    let learned = tokio::spawn(async move { store.learned().await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(!learned.is_finished(), "the store waits for /infos");
    hold.notify_waiters();
    assert_eq!(learned.await.unwrap().as_deref(), Some("in"));
}

#[tokio::test]
async fn a_silent_infos_is_waited_for_no_longer_than_the_wait() {
    let (store, reach) = server_store("auto", Some("FR"));
    let store = store.with_infos_wait(Duration::from_millis(50));
    reach.set_reader(Arc::new(Held(Arc::new(tokio::sync::Notify::new()))));
    let learned = tokio::time::timeout(Duration::from_secs(5), store.learned()).await;
    assert_eq!(learned.expect("the wait ends").as_deref(), Some("fr"));
}

#[tokio::test]
async fn a_country_apple_music_lacks_is_never_learned() {
    let (store, reach) = server_store("auto", Some("ZZ"));
    reach.set_reader(Arc::new(Held(Arc::new(tokio::sync::Notify::new()))));
    let store = store.with_infos_wait(Duration::from_millis(10));
    assert_eq!(store.learned().await, None);
}

struct India;

#[async_trait::async_trait]
impl InfosReader for India {
    async fn read(&self, _background: bool) -> Infos {
        Infos::Closed {
            country_iso: "IN".into(),
            country: "India".into(),
        }
    }
}

#[tokio::test]
async fn learned_prefers_infos_then_the_chart_pages() {
    let (french, reach) = server_store("auto", Some("FR"));
    assert_eq!(
        french.learned().await.as_deref(),
        Some("fr"),
        "no /infos yet: the pages' country"
    );
    let (indian, indian_reach) = server_store("auto", Some("FR"));
    indian_reach.set_reader(Arc::new(India));
    indian_reach.read_infos().await;
    assert_eq!(
        indian.learned().await.as_deref(),
        Some("in"),
        "/infos names the country first"
    );
    reach.observe_rows();
    assert!(matches!(reach.reach(), Reach::Open { .. }));
    // Rows alone name no country: still the pages'.
    assert_eq!(french.learned().await.as_deref(), Some("fr"));
    let (unknown, _) = server_store("auto", None);
    assert_eq!(
        unknown.learned().await,
        None,
        "nothing known: the catalog falls back to us"
    );
}
