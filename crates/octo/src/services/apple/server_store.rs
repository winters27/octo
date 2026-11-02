//! Which of Apple's country stores the catalog stand-in asks: the server's own. The Charts
//! country when one is set, else the country Deezer's `/infos` names (waited for while Deezer's
//! reach is still being learned), else the one Apple's chart pages put the server in; each only
//! when Apple Music has that country. `AppleCatalog` falls back to the store it chose last, then
//! `us`, and remembers its choice.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octo_core::charts::chart_settings::ChartCountry;
use octo_core::settings::SettingsStore;

use super::StoreSource;
use crate::services::metadata::{DeezerReach, Reach};

/// The country Apple's chart pages last put the server in, without asking.
pub type PagesCountry = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Whether Apple Music has a country.
pub type AppleHas = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// How long the store waits for `/infos` while Deezer's reach is still being learned.
const INFOS_WAIT: Duration = Duration::from_secs(4);

pub struct ServerStore {
    settings: Arc<SettingsStore>,
    pages_country: PagesCountry,
    apple_has: AppleHas,
    reach: Arc<DeezerReach>,
    infos_wait: Duration,
}

impl ServerStore {
    pub fn new(
        settings: Arc<SettingsStore>,
        pages_country: PagesCountry,
        apple_has: AppleHas,
        reach: Arc<DeezerReach>,
    ) -> Self {
        Self {
            settings,
            pages_country,
            apple_has,
            reach,
            infos_wait: INFOS_WAIT,
        }
    }

    /// The same store waiting at most `wait` for `/infos` (a test's short wait).
    pub fn with_infos_wait(mut self, wait: Duration) -> Self {
        self.infos_wait = wait;
        self
    }

    fn apple_store(&self, code: &str) -> Option<String> {
        store_code(code).filter(|code| (self.apple_has)(code))
    }
}

#[async_trait]
impl StoreSource for ServerStore {
    fn pinned(&self) -> Option<String> {
        match self.settings.current().generated_playlists.chart_country() {
            ChartCountry::Fixed(code) => self.apple_store(&code),
            ChartCountry::Auto => None,
        }
    }

    async fn learned(&self) -> Option<String> {
        if self.reach.reach() == Reach::Unknown {
            let _ = tokio::time::timeout(self.infos_wait, self.reach.read_infos()).await;
        }
        let from_infos = match self.reach.reach() {
            Reach::Open { country_iso, .. } | Reach::Closed { country_iso, .. } => {
                self.apple_store(&country_iso)
            }
            Reach::Unknown | Reach::Unreachable { .. } => None,
        };
        from_infos.or_else(|| (self.pages_country)().and_then(|code| self.apple_store(&code)))
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
