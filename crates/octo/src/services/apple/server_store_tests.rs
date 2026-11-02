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
        Arc::clone(&reach),
    );
    (store, reach)
}

#[tokio::test]
async fn the_charts_country_pins_the_store() {
    let (pinned, _) = server_store("GB", Some("in"));
    assert_eq!(pinned.pinned().as_deref(), Some("gb"));
    let (automatic, _) = server_store("auto", Some("in"));
    assert_eq!(automatic.pinned(), None);
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
