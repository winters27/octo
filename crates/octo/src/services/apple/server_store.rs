//! Which of Apple's country stores the catalog stand-in asks: the server's own. The Charts
//! country when one is set, else the country Deezer's `/infos` names, else the one Apple's chart
//! pages put the server in; `AppleCatalog` falls back to `us` and remembers its choice.

use std::sync::Arc;

use async_trait::async_trait;
use octo_core::charts::chart_settings::ChartCountry;
use octo_core::settings::SettingsStore;

use super::StoreSource;
use crate::services::metadata::{DeezerReach, Reach};

/// The country Apple's chart pages last put the server in, without asking.
pub type PagesCountry = Arc<dyn Fn() -> Option<String> + Send + Sync>;

pub struct ServerStore {
    settings: Arc<SettingsStore>,
    pages_country: PagesCountry,
    reach: Arc<DeezerReach>,
}

impl ServerStore {
    pub fn new(settings: Arc<SettingsStore>, pages_country: PagesCountry, reach: Arc<DeezerReach>) -> Self {
        Self {
            settings,
            pages_country,
            reach,
        }
    }
}

#[async_trait]
impl StoreSource for ServerStore {
    fn pinned(&self) -> Option<String> {
        match self.settings.current().generated_playlists.chart_country() {
            ChartCountry::Fixed(code) => store_code(&code),
            ChartCountry::Auto => None,
        }
    }

    async fn learned(&self) -> Option<String> {
        let from_infos = match self.reach.reach() {
            Reach::Open { country_iso, .. } | Reach::Closed { country_iso, .. } => store_code(&country_iso),
            Reach::Unknown | Reach::Unreachable { .. } => None,
        };
        from_infos.or_else(|| (self.pages_country)().and_then(|code| store_code(&code)))
    }
}

/// A country code as Apple's store names it: two lowercase letters.
fn store_code(code: &str) -> Option<String> {
    let code = code.trim().to_ascii_lowercase();
    (code.len() == 2 && code.chars().all(|c| c.is_ascii_lowercase())).then_some(code)
}

#[cfg(test)]
#[path = "server_store_tests.rs"]
mod tests;
