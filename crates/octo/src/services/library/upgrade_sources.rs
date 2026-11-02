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
    /// slskd is not logged in, and Mornye is off or cannot stand in for it.
    Soulseek,
    /// Mornye is the Soulseek step's only source (slskd is not in use, by
    /// [`UpgradeSources::slskd_in_use_with`]), and it is not answering or cannot download.
    Mornye,
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

    /// Mornye's check keeps it from every extension a song could use now.
    pub async fn mornye_kept(&self) -> bool {
        self.mornye_pause().await.is_some()
    }

    /// Whether a wait on the check of the Mornye at `made_at`, on the checks `labels`, may end
    /// before its pause does: that Mornye is off or has another address now, or a read shows the
    /// checks passing (a Pass, or Mornye's own next try that passed). With no Mornye at all,
    /// nothing keeps the song.
    pub async fn mornye_wait_over(&self, made_at: &str, labels: &[String]) -> bool {
        match self.spotiflac.as_ref() {
            Some(client) => client.wait_over(made_at, labels).await,
            None => true,
        }
    }

    /// slskd can take a search: it is in use and not known to be logged out.
    async fn slskd_usable(&self) -> bool {
        if !self.slskd_in_use() {
            return false;
        }
        match self.soulseek_link.as_ref() {
            Some(link) => {
                link.read(false).await.map(|reading| reading.link) != Some(SoulseekLinkState::NotLoggedIn)
            }
            None => true,
        }
    }

    /// slskd is in use on this server, by [`Self::slskd_in_use_with`].
    pub fn slskd_in_use(&self) -> bool {
        Self::slskd_in_use_with(&self.settings.current().soulseek, self.soulseek_link.as_deref())
    }

    /// slskd is in use: its link is not off, the same thing the heart chain's slskd client reads
    /// (switched on with an address, at startup; switching it is a restart setting). With no link,
    /// the settings say. A sign-in is not needed: slskd can run without one. The one rule Better
    /// quality, imports, prefetch and Find songs share, so a server never gets two answers.
    pub fn slskd_in_use_with(settings: &SoulseekSettings, link: Option<&dyn ISoulseekLink>) -> bool {
        match link {
            Some(link) => !link.is_off(),
            None => settings.is_on(),
        }
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
        match (self.slskd_in_use(), self.settings.current().spotiflac.is_on()) {
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

    /// The Soulseek step can take a search: slskd can, or Mornye answers, can download, and is
    /// not kept by its check.
    async fn soulseek_step_usable(&self) -> bool {
        if self.slskd_usable().await {
            return true;
        }
        let Some(client) = self.spotiflac.as_ref() else {
            return false;
        };
        client.stands_in().await && self.mornye_pause().await.is_none()
    }

    /// Why the request has nothing to search, when the Soulseek step is what is out.
    pub async fn waiting_for(&self) -> Option<SourcesOut> {
        if !self.plan().contains(&DownloadSource::Soulseek) || !self.available().await.is_empty() {
            return None;
        }
        Some(match self.mornye_pause().await {
            Some(pause) => SourcesOut::MornyeCheck(pause),
            None if !self.slskd_in_use() => SourcesOut::Mornye,
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
            DownloadSource::Soulseek => self.slskd_in_use() || settings.spotiflac.is_on(),
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

    /// slskd's link in one state, off as Octo's own link is when slskd was not switched on with
    /// an address at startup.
    struct FixedLink {
        state: SoulseekLinkState,
        off: bool,
    }

    /// The link Octo would build for `soulseek`, reading `state`.
    fn link_for(soulseek: &SoulseekSettings, state: SoulseekLinkState) -> Arc<dyn ISoulseekLink> {
        Arc::new(FixedLink {
            state,
            off: !soulseek.is_on(),
        })
    }

    #[async_trait]
    impl ISoulseekLink for FixedLink {
        async fn read(&self, _fresh: bool) -> Option<SoulseekServerReading> {
            Some(SoulseekServerReading::new(self.state, None, None, None))
        }

        fn is_off(&self) -> bool {
            self.off
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
        let soulseek = soulseek.unwrap_or_default();
        let link = link_for(&soulseek, link);
        UpgradeSources::new(
            Arc::new(SettingsStore::for_tests(AppSettings {
                library_actions: LibraryActionSettings {
                    upgrade_source: choice,
                    ..Default::default()
                },
                soulseek,
                lidarr: lidarr.unwrap_or_default(),
                ..Default::default()
            })),
            Some(link),
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
        let soulseek = soulseek.unwrap_or_default();
        let link = link_for(&soulseek, link);
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            soulseek,
            spotiflac: octo_core::settings::SpotiflacSettings {
                enabled: true,
                url: url.into(),
                ..Default::default()
            },
            ..Default::default()
        }));
        UpgradeSources::new(Arc::clone(&settings), Some(link))
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
                    assert_eq!(pause.url, server.uri(), "the pause names its Mornye");
                }
                other => panic!("waits for Mornye's check, not {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_mornye_that_does_not_answer_carries_nothing() {
        let sources = with_mornye(
            "http://127.0.0.1:1",
            Some(slskd()),
            SoulseekLinkState::NotLoggedIn,
        );
        assert!(sources.available().await.is_empty());
        assert_eq!(sources.waiting_for().await, Some(SourcesOut::Soulseek));
    }

    /// slskd is in use exactly when its link is on, the same thing the heart chain's client
    /// reads: a sign-in is not needed, and switched off its sign-in can stay saved. With no link
    /// the settings say.
    #[tokio::test]
    async fn slskd_is_in_use_exactly_when_its_link_is_on() {
        let mornye_refusing = |soulseek: SoulseekSettings| {
            Arc::new(SettingsStore::for_tests(AppSettings {
                soulseek,
                spotiflac: octo_core::settings::SpotiflacSettings {
                    enabled: true,
                    url: "http://127.0.0.1:1".into(),
                    ..Default::default()
                },
                ..Default::default()
            }))
        };

        // Off at startup, its sign-in still saved: not in use; the step is Mornye's alone.
        let link_off: Arc<dyn ISoulseekLink> = Arc::new(crate::services::common::test_fakes::OffLink);
        assert!(!UpgradeSources::slskd_in_use_with(
            &slskd(),
            Some(link_off.as_ref())
        ));
        let settings = mornye_refusing(slskd());
        let sources = UpgradeSources::new(Arc::clone(&settings), Some(link_off))
            .with_spotiflac(Arc::new(SpotiflacClient::new(settings)));
        assert_eq!(sources.step_word(DownloadSource::Soulseek), "Mornye");
        assert!(sources.available().await.is_empty());
        assert_eq!(sources.waiting_for().await, Some(SourcesOut::Mornye));

        // On with an address and no sign-in, as an slskd without one runs: in use.
        let no_sign_in = SoulseekSettings {
            username: None,
            password: None,
            ..slskd()
        };
        let link_on = link_for(&no_sign_in, SoulseekLinkState::LoggedIn);
        assert!(UpgradeSources::slskd_in_use_with(
            &no_sign_in,
            Some(link_on.as_ref())
        ));
        let settings = mornye_refusing(no_sign_in);
        let sources = UpgradeSources::new(Arc::clone(&settings), Some(link_on))
            .with_spotiflac(Arc::new(SpotiflacClient::new(settings)));
        assert_eq!(sources.step_word(DownloadSource::Soulseek), "Soulseek or Mornye");
        assert_eq!(sources.available().await, [DownloadSource::Soulseek]);

        // The settings and the running link disagree until a restart; the link wins, as it does
        // for the downloads. With no Mornye, switched off since startup is still offered...
        let only_slskd = |soulseek: SoulseekSettings| {
            Arc::new(SettingsStore::for_tests(AppSettings {
                soulseek,
                ..Default::default()
            }))
        };
        let since_off = SoulseekSettings {
            enabled: false,
            ..slskd()
        };
        let running = link_for(&slskd(), SoulseekLinkState::LoggedIn);
        assert!(UpgradeSources::slskd_in_use_with(
            &since_off,
            Some(running.as_ref())
        ));
        let sources = UpgradeSources::new(only_slskd(since_off), Some(running));
        assert_eq!(sources.plan(), [DownloadSource::Soulseek]);
        assert_eq!(sources.step_word(DownloadSource::Soulseek), "Soulseek");
        assert_eq!(sources.available().await, [DownloadSource::Soulseek]);
        // ...and switched on since startup is not, since nothing would download it.
        let not_running: Arc<dyn ISoulseekLink> = Arc::new(crate::services::common::test_fakes::OffLink);
        let sources = UpgradeSources::new(only_slskd(slskd()), Some(not_running));
        assert!(sources.plan().is_empty());
        assert!(!sources.ready());

        // No link: switched on with an address is in use, switched off is not.
        assert!(UpgradeSources::slskd_in_use_with(&slskd(), None));
        let switched_off = SoulseekSettings {
            enabled: false,
            ..slskd()
        };
        assert!(!UpgradeSources::slskd_in_use_with(&switched_off, None));
    }

    #[tokio::test]
    async fn with_no_slskd_a_mornye_that_does_not_answer_is_what_the_request_waits_for() {
        let sources = with_mornye("http://127.0.0.1:1", None, SoulseekLinkState::NotLoggedIn);
        assert_eq!(sources.plan(), [DownloadSource::Soulseek]);
        assert!(sources.available().await.is_empty());
        assert_eq!(sources.waiting_for().await, Some(SourcesOut::Mornye));
    }

    /// Mornye switched off, or switched on with no address: Better quality answers exactly as it
    /// does with no Mornye at all, and Mornye is never asked.
    #[tokio::test]
    async fn mornye_off_or_without_an_address_changes_nothing_and_is_never_asked() {
        use wiremock::matchers::any;
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let off = [
            octo_core::settings::SpotiflacSettings {
                enabled: false,
                url: server.uri(),
                ..Default::default()
            },
            octo_core::settings::SpotiflacSettings {
                enabled: true,
                url: "  ".into(),
                ..Default::default()
            },
        ];
        for spotiflac in off {
            for soulseek in [None, Some(slskd())] {
                for lidarr in [None, Some(lidarr())] {
                    for link in [SoulseekLinkState::LoggedIn, SoulseekLinkState::NotLoggedIn] {
                        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
                            soulseek: soulseek.clone().unwrap_or_default(),
                            lidarr: lidarr.clone().unwrap_or_default(),
                            spotiflac: spotiflac.clone(),
                            ..Default::default()
                        }));
                        let slskd_settings = soulseek.clone().unwrap_or_default();
                        let without =
                            UpgradeSources::new(Arc::clone(&settings), Some(link_for(&slskd_settings, link)));
                        let with =
                            UpgradeSources::new(Arc::clone(&settings), Some(link_for(&slskd_settings, link)))
                                .with_spotiflac(Arc::new(SpotiflacClient::new(settings)));
                        assert_eq!(with.plan(), without.plan());
                        assert_eq!(with.name(), without.name());
                        assert_eq!(with.step_word(DownloadSource::Soulseek), "Soulseek");
                        assert_eq!(with.available().await, without.available().await);
                        assert_eq!(with.waiting_for().await, without.waiting_for().await);
                    }
                }
            }
        }
        server.verify().await;
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
