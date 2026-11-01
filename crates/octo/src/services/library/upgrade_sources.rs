use std::sync::Arc;

use octo_core::common::dotnet;
use octo_core::settings::{
    DownloadSource, LidarrSettings, SettingsStore, SoulseekSettings, UpgradeSourceChoice,
};

use crate::services::soulseek::ISoulseekLink;
use crate::services::soulseek::soulseek_link::SoulseekLinkState;
use crate::services::spotiflac::SpotiflacClient;

/// Where Better quality looks for a lossless copy, decided in one place so the dashboard, the
/// apps, the queue and the weekly worker never disagree. Only lossless sources count: YouTube can
/// only produce an MP3, which a Better quality check refuses anyway.
pub struct UpgradeSources {
    /// The library action, Soulseek and Lidarr settings, read at every use.
    settings: Arc<SettingsStore>,
    soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    /// Mornye, which rides on the Soulseek step: the step counts when Mornye is on, and Mornye
    /// drops out of it while its check keeps every extension.
    spotiflac: Option<Arc<SpotiflacClient>>,
}

/// Why every source is out, so a request waits instead of failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourcesOut {
    /// slskd is not logged in, and Mornye is off.
    Soulseek,
    /// Mornye's check keeps it from every extension, and slskd cannot be used either.
    MornyeCheck(octo_core::spotiflac::check::CheckPause),
}

impl UpgradeSources {
    pub fn new(settings: Arc<SettingsStore>, soulseek_link: Option<Arc<dyn ISoulseekLink>>) -> Self {
        UpgradeSources {
            settings,
            soulseek_link,
            spotiflac: None,
        }
    }

    /// The same, with Mornye on the Soulseek step.
    pub fn with_spotiflac(mut self, spotiflac: Arc<SpotiflacClient>) -> Self {
        self.spotiflac = Some(spotiflac);
        self
    }

    /// Mornye's check, when Mornye is on and its check keeps every extension a song could use.
    async fn mornye_pause(&self) -> Option<octo_core::spotiflac::check::CheckPause> {
        let client = self.spotiflac.as_ref().filter(|client| client.is_on())?;
        let view = client.check_view(None).await?;
        view.pause(&client.settings().effective_services(), chrono::Utc::now())
    }

    /// slskd can take a search: it is set up and not known to be logged out.
    async fn slskd_usable(&self) -> bool {
        if !Self::soulseek_set_up(&self.settings.current().soulseek) {
            return false;
        }
        match self.soulseek_link.as_ref() {
            Some(link) => {
                link.read(false).await.map(|reading| reading.link) != Some(SoulseekLinkState::NotLoggedIn)
            }
            None => true,
        }
    }

    /// slskd's address and sign-in are set.
    pub fn soulseek_set_up(settings: &SoulseekSettings) -> bool {
        !dotnet::is_null_or_white_space(settings.base_url.as_deref())
            && !dotnet::is_null_or_white_space(settings.username.as_deref())
            && !dotnet::is_null_or_white_space(settings.password.as_deref())
    }

    /// Lidarr's address, key and the choices it needs to add an album are set.
    pub fn lidarr_set_up(settings: &LidarrSettings) -> bool {
        !dotnet::is_null_or_white_space(settings.base_url.as_deref())
            && !dotnet::is_null_or_white_space(settings.api_key.as_deref())
            && !dotnet::is_null_or_white_space(settings.root_folder_path.as_deref())
            && settings.quality_profile_id > 0
            && settings.metadata_profile_id > 0
    }

    /// The sources the setting allows, in the order they are tried, set up or not.
    pub fn wanted(&self) -> Vec<DownloadSource> {
        match self.settings.current().library_actions.upgrade_source {
            UpgradeSourceChoice::Soulseek => vec![DownloadSource::Soulseek],
            UpgradeSourceChoice::Lidarr => vec![DownloadSource::Lidarr],
            _ => vec![DownloadSource::Soulseek, DownloadSource::Lidarr],
        }
    }

    /// The allowed sources that are set up, in the order they are tried.
    pub fn plan(&self) -> Vec<DownloadSource> {
        self.wanted().into_iter().filter(|s| self.set_up(*s)).collect()
    }

    /// Whether any allowed source is set up, so Better quality can be offered at all.
    pub fn ready(&self) -> bool {
        !self.plan().is_empty()
    }

    /// The sources in words, for "Looking for a higher quality copy on X": the ones that are set
    /// up, or, when none is, the ones the setting asks for.
    pub fn name(&self) -> String {
        let plan = self.plan();
        let sources = if plan.is_empty() { self.wanted() } else { plan };
        let mut words: Vec<&str> = Vec::new();
        for source in sources {
            let word = self.step_word(source);
            if !words.contains(&word) {
                words.push(word);
            }
        }
        words.join(" or ")
    }

    /// A source in words, as this server sets it up: the Soulseek step is "Soulseek", "Mornye",
    /// or "Soulseek or Mornye".
    pub fn step_word(&self, source: DownloadSource) -> &'static str {
        if source != DownloadSource::Soulseek {
            return Self::word(source);
        }
        let settings = self.settings.current();
        match (
            Self::soulseek_set_up(&settings.soulseek),
            settings.spotiflac.is_on(),
        ) {
            (false, true) => "Mornye",
            (true, true) => "Soulseek or Mornye",
            _ => "Soulseek",
        }
    }

    /// The sources that can take a search right now. The Soulseek step drops out while slskd
    /// cannot be used and Mornye is off or kept by its check; empty means every source is out,
    /// and the request should wait rather than fail.
    pub async fn available(&self) -> Vec<DownloadSource> {
        let plan = self.plan();
        if !plan.contains(&DownloadSource::Soulseek) {
            return plan;
        }
        if self.soulseek_step_usable().await {
            plan
        } else {
            plan.into_iter()
                .filter(|s| *s != DownloadSource::Soulseek)
                .collect()
        }
    }

    /// The Soulseek step can take a search: slskd can, or Mornye is on and not kept by its check.
    async fn soulseek_step_usable(&self) -> bool {
        if self.slskd_usable().await {
            return true;
        }
        self.spotiflac.as_ref().is_some_and(|client| client.is_on()) && self.mornye_pause().await.is_none()
    }

    /// Why the request has nothing to search, when the Soulseek step is what is out.
    pub async fn waiting_for(&self) -> Option<SourcesOut> {
        if !self.plan().contains(&DownloadSource::Soulseek) || !self.available().await.is_empty() {
            return None;
        }
        Some(match self.mornye_pause().await {
            Some(pause) => SourcesOut::MornyeCheck(pause),
            None => SourcesOut::Soulseek,
        })
    }

    /// Whether the only thing between this request and a search is a Soulseek or Mornye outage.
    pub async fn waiting_for_soulseek(&self) -> bool {
        self.waiting_for().await.is_some()
    }

    pub fn words(sources: impl IntoIterator<Item = DownloadSource>) -> String {
        let mut words: Vec<&str> = Vec::new();
        for source in sources {
            let word = Self::word(source);
            if !words.contains(&word) {
                words.push(word);
            }
        }
        words.join(" or ")
    }

    pub fn word(source: DownloadSource) -> &'static str {
        match source {
            DownloadSource::Lidarr => "Lidarr",
            DownloadSource::YouTube => "YouTube",
            _ => "Soulseek",
        }
    }

    fn set_up(&self, source: DownloadSource) -> bool {
        let settings = self.settings.current();
        match source {
            DownloadSource::Soulseek => {
                Self::soulseek_set_up(&settings.soulseek) || settings.spotiflac.is_on()
            }
            DownloadSource::Lidarr => Self::lidarr_set_up(&settings.lidarr),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Where Better quality looks for a lossless copy: Soulseek, Lidarr, or both in that order.
    //! One place decides it, so the dashboard, the apps and the workers never disagree. Only
    //! sources that are set up count, and a Soulseek outage leaves Lidarr rather than stopping
    //! everything.

    use async_trait::async_trait;
    use chrono::{DateTime, TimeDelta, Utc};
    use octo_core::settings::{AppSettings, LibraryActionSettings};

    use super::*;
    use crate::services::soulseek::soulseek_link::SoulseekServerReading;

    fn slskd() -> SoulseekSettings {
        SoulseekSettings {
            base_url: Some("http://slskd:5030".into()),
            username: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        }
    }

    fn lidarr() -> LidarrSettings {
        LidarrSettings {
            base_url: Some("http://lidarr:8686".into()),
            api_key: Some("k".into()),
            root_folder_path: Some("/music".into()),
            quality_profile_id: 1,
            metadata_profile_id: 1,
            ..Default::default()
        }
    }

    struct FixedLink(SoulseekLinkState);

    #[async_trait]
    impl ISoulseekLink for FixedLink {
        async fn read(&self, _fresh: bool) -> Option<SoulseekServerReading> {
            Some(SoulseekServerReading::new(self.0, None, None, None))
        }

        fn hold_limit(&self) -> TimeDelta {
            TimeDelta::zero()
        }

        fn utc_now(&self) -> DateTime<Utc> {
            Utc::now()
        }

        async fn wait_for_login(&self, _until: DateTime<Utc>) -> bool {
            true
        }
    }

    fn sources(
        choice: UpgradeSourceChoice,
        soulseek: Option<SoulseekSettings>,
        lidarr: Option<LidarrSettings>,
        link: SoulseekLinkState,
    ) -> UpgradeSources {
        UpgradeSources::new(
            Arc::new(SettingsStore::for_tests(AppSettings {
                library_actions: LibraryActionSettings {
                    upgrade_source: choice,
                    ..Default::default()
                },
                soulseek: soulseek.unwrap_or_default(),
                lidarr: lidarr.unwrap_or_default(),
                ..Default::default()
            })),
            Some(Arc::new(FixedLink(link))),
        )
    }

    fn logged_in(
        choice: UpgradeSourceChoice,
        soulseek: Option<SoulseekSettings>,
        lidarr: Option<LidarrSettings>,
    ) -> UpgradeSources {
        sources(choice, soulseek, lidarr, SoulseekLinkState::LoggedIn)
    }

    #[test]
    fn auto_tries_soulseek_then_lidarr() {
        let sources = logged_in(UpgradeSourceChoice::Auto, Some(slskd()), Some(lidarr()));

        assert_eq!(sources.plan(), [DownloadSource::Soulseek, DownloadSource::Lidarr]);
        assert!(sources.ready());
        assert_eq!(sources.name(), "Soulseek or Lidarr");
    }

    #[test]
    fn auto_uses_only_what_is_set_up() {
        assert_eq!(
            logged_in(UpgradeSourceChoice::Auto, None, Some(lidarr())).plan(),
            [DownloadSource::Lidarr]
        );
        assert_eq!(
            logged_in(UpgradeSourceChoice::Auto, None, Some(lidarr())).name(),
            "Lidarr"
        );
        assert_eq!(
            logged_in(UpgradeSourceChoice::Auto, Some(slskd()), None).plan(),
            [DownloadSource::Soulseek]
        );
    }

    #[test]
    fn a_chosen_source_is_the_only_one() {
        assert_eq!(
            logged_in(UpgradeSourceChoice::Lidarr, Some(slskd()), Some(lidarr())).plan(),
            [DownloadSource::Lidarr]
        );
        assert_eq!(
            logged_in(UpgradeSourceChoice::Soulseek, Some(slskd()), Some(lidarr())).plan(),
            [DownloadSource::Soulseek]
        );
    }

    #[test]
    fn nothing_set_up_is_not_ready_and_names_what_it_wants() {
        let sources = logged_in(UpgradeSourceChoice::Lidarr, Some(slskd()), None);

        assert!(sources.plan().is_empty());
        assert!(!sources.ready());
        assert_eq!(sources.name(), "Lidarr");
    }

    #[test]
    fn lidarr_needs_its_root_folder_and_profiles_to_add_an_album() {
        let half = LidarrSettings {
            base_url: Some("http://lidarr:8686".into()),
            api_key: Some("k".into()),
            ..Default::default()
        };

        assert!(!UpgradeSources::lidarr_set_up(&half));
        assert!(UpgradeSources::lidarr_set_up(&lidarr()));
    }

    #[tokio::test]
    async fn a_soulseek_outage_leaves_lidarr() {
        let sources = sources(
            UpgradeSourceChoice::Auto,
            Some(slskd()),
            Some(lidarr()),
            SoulseekLinkState::NotLoggedIn,
        );

        assert_eq!(sources.available().await, [DownloadSource::Lidarr]);
        assert!(!sources.waiting_for_soulseek().await);
    }

    #[tokio::test]
    async fn with_only_soulseek_an_outage_means_wait() {
        let sources = sources(
            UpgradeSourceChoice::Auto,
            Some(slskd()),
            None,
            SoulseekLinkState::NotLoggedIn,
        );

        assert!(sources.available().await.is_empty());
        assert!(sources.waiting_for_soulseek().await);
    }

    /// A Mornye whose one gateway session is in `state`.
    async fn mornye(state: &str) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::builder().start().await;
        let paused_s = if state == "active" { 0.0 } else { 900.0 };
        Mock::given(method("POST"))
            .and(path("/api/get_signed_sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"result": {"sessions": [
                    {"key": "gateway-v2-abc-6836abb49bb0e8c9", "kind": "gateway", "label": "Example",
                     "extensions": ["example-ext"], "state": state, "auth_paused_s": paused_s}
                ]}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/get_download_services"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"result": {"services": [
                    {"id": "ext:example-ext", "label": "Example", "extensions": ["example-ext"]}
                ]}}),
            ))
            .mount(&server)
            .await;
        server
    }

    /// Better quality with Mornye at `url` and this slskd.
    fn with_mornye(url: &str, soulseek: Option<SoulseekSettings>, link: SoulseekLinkState) -> UpgradeSources {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            soulseek: soulseek.unwrap_or_default(),
            spotiflac: octo_core::settings::SpotiflacSettings {
                enabled: true,
                url: url.into(),
                ..Default::default()
            },
            ..Default::default()
        }));
        UpgradeSources::new(Arc::clone(&settings), Some(Arc::new(FixedLink(link))))
            .with_spotiflac(Arc::new(SpotiflacClient::new(settings)))
    }

    #[tokio::test]
    async fn mornye_rides_on_the_soulseek_step_and_is_named() {
        let server = mornye("active").await;
        let alone = with_mornye(&server.uri(), None, SoulseekLinkState::LoggedIn);
        assert_eq!(alone.plan(), [DownloadSource::Soulseek]);
        assert_eq!(alone.name(), "Mornye");
        let both = with_mornye(&server.uri(), Some(slskd()), SoulseekLinkState::LoggedIn);
        assert_eq!(both.name(), "Soulseek or Mornye");
    }

    #[tokio::test]
    async fn mornye_carries_the_step_while_slskd_is_logged_out() {
        let server = mornye("active").await;
        let sources = with_mornye(&server.uri(), Some(slskd()), SoulseekLinkState::NotLoggedIn);
        assert_eq!(sources.available().await, [DownloadSource::Soulseek]);
        assert_eq!(sources.waiting_for().await, None);
    }

    #[tokio::test]
    async fn mornyes_paused_check_with_slskd_out_means_wait_for_the_check() {
        let server = mornye("unverified").await;
        for soulseek in [None, Some(slskd())] {
            let sources = with_mornye(&server.uri(), soulseek, SoulseekLinkState::NotLoggedIn);
            assert!(sources.available().await.is_empty());
            match sources.waiting_for().await {
                Some(SourcesOut::MornyeCheck(pause)) => {
                    assert_eq!(pause.labels, vec!["Example".to_string()]);
                    assert!(pause.until_utc > chrono::Utc::now());
                }
                other => panic!("waits for Mornye's check, not {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_logged_in_slskd_carries_the_step_while_mornyes_check_is_paused() {
        let server = mornye("unverified").await;
        let sources = with_mornye(&server.uri(), Some(slskd()), SoulseekLinkState::LoggedIn);
        assert_eq!(sources.available().await, [DownloadSource::Soulseek]);
        assert_eq!(sources.waiting_for().await, None);
    }

    #[tokio::test]
    async fn lidarr_alone_never_waits_for_soulseek() {
        let sources = sources(
            UpgradeSourceChoice::Lidarr,
            Some(slskd()),
            Some(lidarr()),
            SoulseekLinkState::NotLoggedIn,
        );

        assert_eq!(sources.available().await, [DownloadSource::Lidarr]);
        assert!(!sources.waiting_for_soulseek().await);
    }
}
