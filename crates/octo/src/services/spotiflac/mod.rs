//! The person's own Mornye (SpotiFLAC before its rename) server, reached through its web API (`spotiflac --web`): the
//! client the download path asks for songs, the gate that keeps to the number of songs asked for
//! at once, and the dashboard's Test, which says plainly what is missing.
//!
//! Octo sends Mornye a catalog link and a folder, and takes the file Mornye writes there.
//! How Mornye gets the audio is up to the extensions the person installed in it.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub mod assist;
pub mod check;
pub mod waiting;

pub use assist::{AssistRun, CannotPass, Frame, RunState};

pub use check::{CheckSong, CheckTimes, CheckView, Meanwhile, MornyeCheck, MornyeCheckStore, push_words};
pub use waiting::{MornyeWaiting, mornye_waited};

use octo_core::common::dotnet::escape_data_string;
use octo_core::settings::{SettingsStore, SpotiflacSettings};
use octo_core::spotiflac::check::{
    CheckRow, Reading, RpcAnswer, ScanAnswer, ServiceRow, ServicesAnswer, SessionsAnswer, check_rows,
    extension_id, names_missing_utilities, plain_dashes, service_rows, until_words,
};
use octo_core::spotiflac::{
    CatalogTrack, ErrorAnswer, ExtensionsAnswer, Job, SearchAnswer, ServerInfo, path_for_spotiflac,
};
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::Notify;

use crate::services::http_client_factory::{
    self, connect_failure_message, status_failure_message, timeout_message,
};

/// The cookies the web token is sent in: Mornye's, and SpotiFLAC's from before its rename, so
/// either one reads it.
pub const TOKEN_COOKIES: [&str; 2] = ["mornye_web_token", "spotiflac_web_token"];

/// Where Mornye's downloads land, under Octo's music folder: a dot folder, which Navidrome's
/// scanner skips.
pub const SPOTIFLAC_FOLDER: &str = ".octo-incoming/spotiflac";

/// A folder name Octo gives: a job id (32 hex characters), or a folder check's `octo-check-<id>`.
pub(crate) fn octo_folder_name(name: &str) -> bool {
    let id = name.strip_prefix("octo-check-").unwrap_or(name);
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// How long one call to Mornye's API may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// What a read says when Mornye answers that it has no such call (an older Mornye or SpotiFLAC).
const NO_CHECK_CALL: &str = "this Mornye does not report its check";
const NO_SERVICES_CALL: &str = "this Mornye does not list its download services";

/// A call to Mornye that did not work, in words a person can act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpotiflacError {
    #[error("Octo could not reach Mornye at {url}. {why}")]
    Unreachable { url: String, why: String },
    #[error(
        "Mornye refused the web token. Set the token Mornye was started with: SPOTIFLAC_WEB_TOKEN with Octo's spotiflac service, otherwise its --web-token or MORNYE_WEB_TOKEN."
    )]
    Unauthorized,
    #[error(
        "{url} answers, but not as Mornye's web API (or an older SpotiFLAC's). Check that Mornye runs with --web and that the address points at it."
    )]
    NotSpotiflac { url: String },
    #[error("Mornye said: {0}")]
    Said(String),
}

/// One connection's address and token, as the settings or a Test hold them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub url: String,
    pub token: String,
}

impl Connection {
    pub fn of(settings: &SpotiflacSettings) -> Connection {
        Connection {
            url: settings.effective_url(),
            token: settings.token.trim().to_string(),
        }
    }
}

/// The client. Settings are read on every call, so a change on the dashboard applies at once.
pub struct SpotiflacClient {
    settings: Arc<SettingsStore>,
    http: reqwest::Client,
    busy: Mutex<usize>,
    freed: Notify,
    poll_interval: Duration,
    song_timeout: Option<Duration>,
    /// Mornye's check: the cached read, its episodes and check songs, the push.
    pub check: MornyeCheck,
    /// Jobs asked of Mornye since start, to tell whose failure an extension's last error is.
    started_jobs: AtomicU64,
    /// "Pass Mornye's check": the live or last run.
    assist: assist::Assist,
    /// How often a run looks at Mornye (tests look quickly).
    assist_look_every: Option<Duration>,
}

/// A place among the songs asked for at once, given back when dropped.
pub struct Slot<'a> {
    client: &'a SpotiflacClient,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut busy = self.client.busy.lock();
        *busy = busy.saturating_sub(1);
        drop(busy);
        self.client.freed.notify_waiters();
    }
}

impl SpotiflacClient {
    pub fn new(settings: Arc<SettingsStore>) -> SpotiflacClient {
        Self::with_poll_interval(settings, Duration::from_secs(2))
    }

    /// The same, asking about a job this often (tests ask quickly).
    pub fn with_poll_interval(settings: Arc<SettingsStore>, poll_interval: Duration) -> SpotiflacClient {
        SpotiflacClient {
            settings,
            http: http_client_factory::builder()
                .timeout(CALL_TIMEOUT)
                .build()
                .expect("the Mornye client builds"),
            busy: Mutex::new(0),
            freed: Notify::new(),
            poll_interval,
            song_timeout: None,
            check: MornyeCheck::in_memory(),
            started_jobs: AtomicU64::new(0),
            assist: assist::Assist::default(),
            assist_look_every: None,
        }
    }

    /// The same, a Pass run looking this often (tests look quickly).
    pub fn with_assist_look_every(mut self, every: Duration) -> SpotiflacClient {
        self.assist_look_every = Some(every);
        self
    }

    fn http_client(&self) -> &reqwest::Client {
        &self.http
    }

    /// The same, keeping Mornye's check in `check` (its file and its push).
    pub fn with_check(mut self, check: MornyeCheck) -> SpotiflacClient {
        self.check = check;
        self
    }

    /// How many songs are being asked of Mornye now, and how many jobs were asked since start.
    pub fn activity(&self) -> (usize, u64) {
        (*self.busy.lock(), self.started_jobs.load(Ordering::SeqCst))
    }

    /// Counts a job about to be asked of Mornye.
    pub fn count_job(&self) {
        self.started_jobs.fetch_add(1, Ordering::SeqCst);
    }

    /// Mornye's check for the saved connection: the kept read when it fits (started after
    /// `after`, or fresh), else a new one, one at a time. None while Mornye is off or has no
    /// address. A new read is kept only while its connection is still the saved one and no later
    /// read is kept; a kept read opens episodes and may push.
    pub async fn check_view(&self, after: Option<Instant>) -> Option<Arc<CheckView>> {
        let settings = self.settings();
        if !settings.is_on() {
            return None;
        }
        let connection = Connection::of(&settings);
        if connection.url.is_empty() {
            return None;
        }
        if let Some(view) = self.check.cached(&connection, after) {
            return Some(view);
        }
        let _turn = self.check.refresh_lock().lock().await;
        if let Some(view) = self.check.cached(&connection, after) {
            return Some(view);
        }
        let started = Instant::now();
        let read_utc = chrono::Utc::now();
        let bound = self.check.times.call_bound;
        let (rows, services) = tokio::join!(
            bounded(bound, self.signed_sessions(&connection, read_utc)),
            bounded(bound, self.download_services(&connection)),
        );
        let no_such_call = matches!(&rows, Reading::CannotTell(why) if why == NO_CHECK_CALL)
            || matches!(&services, Reading::CannotTell(why) if why == NO_SERVICES_CALL);
        let mut view = CheckView::new(connection.clone(), started, read_utc, rows, services);
        view.reached |= no_such_call;
        let view = Arc::new(view);
        if Connection::of(&self.settings()) == connection && self.check.keep(&view) {
            let all = self.settings.current();
            let meanwhile = Meanwhile::of(&all);
            let public_url = all.server.public_url.clone();
            self.check.note(&view, |labels, view| {
                let now = chrono::Utc::now();
                let until = view
                    .rows
                    .known()
                    .into_iter()
                    .flatten()
                    .filter(|row| labels.contains(&row.label) && row.paused_at(now))
                    .filter_map(|row| row.paused_until)
                    .min();
                push_words(labels, until, meanwhile, &public_url, now)
            });
        }
        // A read that reached Mornye wakes the worker that ends song jobs stopped Pass runs left
        // there; no reader waits on it.
        if view.reached {
            self.left_pass_jobs_may_end();
        }
        Some(view)
    }

    /// The same, with one song's time fixed instead of read from the settings (tests wait
    /// briefly).
    pub fn with_song_timeout(mut self, timeout: Duration) -> SpotiflacClient {
        self.song_timeout = Some(timeout);
        self
    }

    /// How long one song may take.
    pub fn song_timeout(&self) -> Duration {
        self.song_timeout
            .unwrap_or_else(|| Duration::from_secs(self.settings().effective_timeout_seconds()))
    }

    /// The settings as they are now.
    pub fn settings(&self) -> SpotiflacSettings {
        self.settings.current().spotiflac.clone()
    }

    /// On, with an address.
    pub fn is_on(&self) -> bool {
        self.settings.current().spotiflac.is_on()
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Waits for a place among the songs asked for at once.
    pub async fn slot(&self) -> Slot<'_> {
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            {
                let mut busy = self.busy.lock();
                if *busy < self.settings().effective_at_once() {
                    *busy += 1;
                    return Slot { client: self };
                }
            }
            freed.await;
        }
    }

    fn request(
        &self,
        connection: &Connection,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest::RequestBuilder {
        let builder = self
            .http
            .request(method, format!("{}{path}", connection.url.trim_end_matches('/')));
        if connection.token.is_empty() {
            builder
        } else {
            builder.header(
                reqwest::header::COOKIE,
                TOKEN_COOKIES
                    .iter()
                    .map(|name| format!("{name}={}", connection.token))
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        }
    }

    async fn send<T: DeserializeOwned>(
        &self,
        connection: &Connection,
        request: reqwest::RequestBuilder,
    ) -> Result<Option<T>, SpotiflacError> {
        let response = reach(connection, request).await?;
        read_answer(connection, response).await
    }

    async fn get<T: DeserializeOwned>(
        &self,
        connection: &Connection,
        path: &str,
    ) -> Result<Option<T>, SpotiflacError> {
        self.send(connection, self.request(connection, reqwest::Method::GET, path))
            .await
    }

    /// `GET /api/v1/info`.
    pub async fn info(&self, connection: &Connection) -> Result<ServerInfo, SpotiflacError> {
        match self.get::<ServerInfo>(connection, "/api/v1/info").await? {
            Some(info) if info.is_spotiflac() => Ok(info),
            _ => Err(SpotiflacError::NotSpotiflac {
                url: connection.url.clone(),
            }),
        }
    }

    /// `GET /api/v1/extensions`.
    pub async fn extensions(&self, connection: &Connection) -> Result<ExtensionsAnswer, SpotiflacError> {
        Ok(self
            .get::<ExtensionsAnswer>(connection, "/api/v1/extensions")
            .await?
            .unwrap_or_default())
    }

    /// `POST /api/{method}` of Mornye's page, with no arguments: its `result`. None when Mornye
    /// has no such call (an older SpotiFLAC, or a call off its list).
    async fn rpc<T: DeserializeOwned>(
        &self,
        connection: &Connection,
        method: &str,
    ) -> Result<Option<T>, SpotiflacError> {
        let request = self.request(connection, reqwest::Method::POST, &format!("/api/{method}"));
        Ok(self
            .send::<RpcAnswer<T>>(connection, request)
            .await?
            .map(|answer| answer.result))
    }

    /// Mornye's signed sessions and their pauses, read at `read_utc`.
    pub async fn signed_sessions(
        &self,
        connection: &Connection,
        read_utc: chrono::DateTime<chrono::Utc>,
    ) -> Reading<Vec<CheckRow>> {
        match self
            .rpc::<SessionsAnswer>(connection, "get_signed_sessions")
            .await
        {
            Ok(Some(answer)) => check_rows(&answer, read_utc),
            Ok(None) => Reading::CannotTell(NO_CHECK_CALL.into()),
            Err(e) => Reading::CannotTell(e.to_string()),
        }
    }

    /// The download services Mornye can offer.
    pub async fn download_services(&self, connection: &Connection) -> Reading<Vec<ServiceRow>> {
        match self
            .rpc::<ServicesAnswer>(connection, "get_download_services")
            .await
        {
            Ok(Some(answer)) => service_rows(&answer),
            Ok(None) => Reading::CannotTell(NO_SERVICES_CALL.into()),
            Err(e) => Reading::CannotTell(e.to_string()),
        }
    }

    /// Whether the checks a wait named (`labels`) pass now, from the kept read: a Pass, or
    /// Mornye's own next try that passed.
    pub async fn checks_pass(&self, labels: &[String]) -> bool {
        match self.check_view(None).await {
            Some(view) => octo_core::spotiflac::check::labels_pass(&view.rows, labels),
            None => false,
        }
    }

    /// The address Octo asks Mornye at now; None while Mornye is off or has no address.
    pub fn live_url(&self) -> Option<String> {
        let settings = self.settings();
        settings.is_on().then(|| settings.effective_url())
    }

    /// Whether a wait on the check of the Mornye at `made_at`, on the checks `labels`, may end
    /// before its pause does: that Mornye is off or has another address now, so nothing it said
    /// keeps the song, or a read shows the checks passing. Mornye is read only in the last case.
    pub async fn wait_over(&self, made_at: &str, labels: &[String]) -> bool {
        if octo_core::spotiflac::check::mornye_moved(made_at, self.live_url().as_deref()) {
            return true;
        }
        !labels.is_empty() && self.checks_pass(labels).await
    }

    /// Mornye can stand in for Soulseek now: on, answering, and able to download, from the kept
    /// read of its check.
    pub async fn stands_in(&self) -> bool {
        if !self.is_on() {
            return false;
        }
        match self.check_view(None).await {
            Some(view) => view.stands_in(&self.settings().effective_services()),
            None => false,
        }
    }

    /// `POST /api/v1/library/scan` of one folder on Mornye's side, not its subfolders: how many
    /// song files Mornye sees there.
    pub async fn scan(&self, connection: &Connection, their_dir: &str) -> Result<ScanSaw, SpotiflacError> {
        let request = self
            .request(connection, reqwest::Method::POST, "/api/v1/library/scan")
            .json(&json!({"path": their_dir, "recursive": false}));
        let response = reach(connection, request).await?;
        if response.status() == reqwest::StatusCode::BAD_REQUEST {
            return Ok(ScanSaw::Outside);
        }
        Ok(match read_answer::<ScanAnswer>(connection, response).await? {
            Some(scan) => ScanSaw::Songs(scan.scanned),
            None => ScanSaw::NoCall,
        })
    }

    /// `GET /api/v1/search?q=`: the catalog tracks Mornye finds for the words.
    pub async fn search(
        &self,
        connection: &Connection,
        query: &str,
    ) -> Result<Vec<CatalogTrack>, SpotiflacError> {
        let request = self
            .request(connection, reqwest::Method::GET, "/api/v1/search")
            .query(&[("q", query), ("limit", "10")]);
        Ok(self
            .send::<SearchAnswer>(connection, request)
            .await?
            .map(|answer| answer.tracks)
            .unwrap_or_default())
    }

    /// `POST /api/v1/downloads`: asks Mornye to save the track at `link` into `output_dir`, a
    /// folder on Mornye's side.
    pub async fn queue(
        &self,
        connection: &Connection,
        link: &str,
        settings: &SpotiflacSettings,
        output_dir: &str,
    ) -> Result<Job, SpotiflacError> {
        #[derive(Serialize)]
        struct Ask<'a> {
            url: &'a str,
            quality: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            services: Option<Vec<String>>,
            output_dir: &'a str,
        }
        let services = settings.effective_services();
        let ask = Ask {
            url: link,
            quality: settings.effective_quality(),
            services: (!services.is_empty()).then_some(services),
            output_dir,
        };
        let request = self
            .request(connection, reqwest::Method::POST, "/api/v1/downloads")
            .json(&ask);
        self.send::<Job>(connection, request)
            .await?
            .ok_or_else(|| SpotiflacError::NotSpotiflac {
                url: connection.url.clone(),
            })
    }

    /// `GET /api/v1/downloads/{id}`, or None when Mornye no longer knows the job.
    pub async fn job(&self, connection: &Connection, id: &str) -> Result<Option<Job>, SpotiflacError> {
        self.get::<Job>(
            connection,
            &format!("/api/v1/downloads/{}", escape_data_string(id)),
        )
        .await
    }

    /// `POST /api/v1/downloads/{id}/cancel`. Answers whether Mornye took the cancel; any other
    /// answer (a job it no longer runs, a refusal, a Mornye without the call, or none) is for the
    /// caller to look into.
    pub async fn cancel(&self, connection: &Connection, id: &str) -> bool {
        let request = self.request(
            connection,
            reqwest::Method::POST,
            &format!("/api/v1/downloads/{}/cancel", escape_data_string(id)),
        );
        match reach(connection, request).await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    tracing::debug!("Mornye did not cancel job {id}: it answered {status}");
                }
                status.is_success()
            }
            Err(e) => {
                tracing::debug!("Mornye did not cancel job {id}: {e}");
                false
            }
        }
    }

    /// The dashboard's Test: whether Mornye answers at the address, with the token, has an
    /// extension to download with, and whether Octo can make the folder downloads land in.
    /// Nothing is downloaded.
    pub async fn test(
        &self,
        connection: &Connection,
        enabled: bool,
        folder: Option<&str>,
        download_path: &str,
    ) -> TestReport {
        let mut report = TestReport::default();
        if connection.url.is_empty() {
            report.fail(
                "No address yet. Type the address of your Mornye server, for example http://spotiflac:8000.",
            );
            return report.finish();
        }
        if !(connection.url.starts_with("http://") || connection.url.starts_with("https://")) {
            report.fail("The address must start with http:// or https://.");
            return report.finish();
        }
        if let Some(problem) = SpotiflacSettings::token_problem(&connection.token) {
            report.fail(problem);
            return report.finish();
        }
        match self.info(connection).await {
            Ok(info) => {
                report.version = Some(info.version.clone()).filter(|v| !v.is_empty());
                let version = report
                    .version
                    .as_deref()
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default();
                report.pass(&format!("Mornye{version} answers at {}.", connection.url));
                if info.multiuser {
                    report.fail(
                        "Mornye runs with --web-multiuser, where it saves into each account's own folder. \
                         Run it with a web token instead (SPOTIFLAC_WEB_TOKEN with Octo's spotiflac service), so Octo can say where to save.",
                    );
                }
                if connection.token.is_empty() && !info.authenticated {
                    report.warn(
                        "Mornye has no web token, so anyone who can reach its address can use it, and its check \
                         cannot be passed from Octo. Set SPOTIFLAC_WEB_TOKEN (Mornye's own name: MORNYE_WEB_TOKEN) \
                         and the same token here.",
                    );
                }
                if !info.name.eq_ignore_ascii_case("Mornye") {
                    let version = plain_dashes(info.version.trim());
                    report.warn(&format!(
                        "This is SpotiFLAC {version}, from before its rename to Mornye. Run Mornye 1.0.4 or later."
                    ));
                } else if info.version.trim().is_empty() {
                    report.warn("Octo could not read this Mornye's version.");
                } else if info.cannot_pass_check() {
                    report.warn(&format!(
                        "This Mornye is {}; versions before 1.0.4 cannot pass the check. Update it.",
                        plain_dashes(info.version.trim())
                    ));
                }
            }
            Err(e) => {
                report.fail(&e.to_string());
                return report.finish();
            }
        }
        let now = chrono::Utc::now();
        let bound = self.check.times.call_bound;
        let (services, rows) = tokio::join!(
            bounded(bound, self.download_services(connection)),
            bounded(bound, self.signed_sessions(connection, now)),
        );
        let mut utilities_said = false;
        for reading in [cannot_tell_text(&services), cannot_tell_text(&rows)] {
            if reading.is_some_and(names_missing_utilities) && !utilities_said {
                utilities_said = true;
                report.fail(
                    "Mornye needs its utilities extension, which your registry does not list. Mornye 1.1 moved its \
                     search and its check into it; use a registry that lists it, or set SPOTIFLAC_IMAGE back to Mornye 1.0.5.",
                );
            }
        }
        match self.extensions(connection).await {
            Ok(extensions) => {
                let installed = extensions.installed();
                report.extensions = Some(installed);
                // Without its utilities extension Mornye 1.1 lists no download services at all,
                // which the utilities line already explains.
                if installed > 0
                    && !utilities_said
                    && let Reading::Known(services) = &services
                    && services.is_empty()
                {
                    report.fail("Mornye has extensions but none that downloads.");
                }
                // Mornye counts tries on a row of the bare extension name, beside the installed
                // `ext:` row; a removed extension's old counts are not warned about.
                let installed_ids: BTreeSet<String> = extensions
                    .providers
                    .iter()
                    .filter(|row| row.installed)
                    .map(|row| extension_id(&row.provider))
                    .collect();
                for row in extensions.providers.iter().filter(|row| {
                    installed_ids.contains(&extension_id(&row.provider))
                        && row.last_outcome.eq_ignore_ascii_case("failure")
                        && !row.last_error.trim().is_empty()
                }) {
                    report.warn(&format!(
                        "{} failed its last try: {}",
                        plain_dashes(&row.provider),
                        plain_dashes(row.last_error.trim())
                    ));
                }
                if installed == 0 {
                    report.fail(
                        "Mornye has no extensions installed, so it cannot download anything yet. \
                         Mornye installs every extension in your registry by itself, when it starts and again every hour: \
                         check the registry address (SPOTIFLAC_REGISTRIES with Octo's spotiflac service, MORNYE_REGISTRIES \
                         on a Mornye you run yourself), then restart Mornye.",
                    );
                } else {
                    report.pass(&format!(
                        "{installed} {} installed in Mornye.",
                        if installed == 1 { "extension" } else { "extensions" }
                    ));
                }
            }
            Err(e) => report.warn(&format!("Octo could not read Mornye's extensions. {e}")),
        }
        match &services {
            Reading::CannotTell(why) if !names_missing_utilities(why) => {
                report.warn("Octo could not read which extensions Mornye downloads with.")
            }
            _ => {}
        }
        match &rows {
            Reading::Known(rows) => {
                for row in rows.iter().filter(|row| row.paused_at(now)) {
                    if let Some(until) = row.paused_until {
                        report.warn(&format!(
                            "Mornye's check for {} is paused; it allows its next try at {}.",
                            row.label,
                            until_words(until, now)
                        ));
                    }
                }
            }
            Reading::CannotTell(why) if !names_missing_utilities(why) => {
                report.warn("This Mornye does not report its check.")
            }
            Reading::CannotTell(_) => {}
        }
        if download_path.trim().is_empty() {
            report.fail("Octo has no music folder yet, so it has nowhere for Mornye's downloads to land.");
        } else {
            let octo_side = format!("{}/{SPOTIFLAC_FOLDER}", download_path.trim_end_matches('/'));
            match make_shared_folder(&octo_side) {
                Ok(()) => {
                    let theirs = path_for_spotiflac(&octo_side, download_path, folder);
                    self.folder_check(&mut report, connection, &octo_side, &theirs)
                        .await;
                }
                Err(e) => report.fail(&format!("Octo cannot make the folder {octo_side}: {e}.")),
            }
        }
        if !enabled {
            report.warn("The switch is off, so downloads do not ask Mornye yet. Turn it on and save.");
        }
        report.finish()
    }
}

impl SpotiflacClient {
    /// The Test's folder round trip: Octo puts one small song-named file in a fresh folder of its
    /// own and asks Mornye how many songs it sees there. The folder is removed whatever happens.
    async fn folder_check(
        &self,
        report: &mut TestReport,
        connection: &Connection,
        octo_side: &str,
        theirs: &str,
    ) {
        struct RemoveOnDrop(String);
        impl Drop for RemoveOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let name = format!("octo-check-{}", uuid::Uuid::new_v4().simple());
        let octo_dir = format!("{}/{name}", octo_side.trim_end_matches('/'));
        let their_dir = format!("{}/{name}", theirs.trim_end_matches('/'));
        let _remove = RemoveOnDrop(octo_dir.clone());
        let written = make_shared_folder(&octo_dir)
            .and_then(|()| std::fs::write(format!("{octo_dir}/check.flac"), b"octo test"));
        if let Err(e) = written {
            report.fail(&format!("Octo cannot write into {octo_side}: {e}."));
            return;
        }
        let answer =
            tokio::time::timeout(self.check.times.call_bound, self.scan(connection, &their_dir)).await;
        match answer {
            Ok(Ok(ScanSaw::Songs(seen))) if seen >= 1 => report.pass(&format!(
                "Mornye sees Octo's folder: {theirs} in Mornye is {octo_side} in Octo."
            )),
            Ok(Ok(ScanSaw::Songs(_))) => report.fail(&format!(
                "Mornye does not see Octo's folder at {theirs}. Mount Octo's music folder in Mornye at that path, \
                 or set Music folder in Mornye under More settings."
            )),
            Ok(Ok(ScanSaw::Outside)) => report.warn(&format!(
                "Octo cannot check the folder from here, because it is outside Mornye's own download folder. \
                 Make sure {theirs} in Mornye is {octo_side} in Octo."
            )),
            _ => report.warn(&format!(
                "Octo cannot check the folder from here. Make sure {theirs} in Mornye is {octo_side} in Octo."
            )),
        }
    }
}

/// Sends `request` to Mornye; a request that never got an answer is "unreachable".
async fn reach(
    connection: &Connection,
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, SpotiflacError> {
    request.send().await.map_err(|e| SpotiflacError::Unreachable {
        url: connection.url.clone(),
        why: if e.is_timeout() {
            timeout_message(CALL_TIMEOUT)
        } else {
            connect_failure_message(&e)
        },
    })
}

/// Mornye's answer read as `T`: None for a call it does not have, its own words for a refusal.
async fn read_answer<T: DeserializeOwned>(
    connection: &Connection,
    response: reqwest::Response,
) -> Result<Option<T>, SpotiflacError> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(SpotiflacError::Unauthorized);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        let said = serde_json::from_str::<ErrorAnswer>(&body)
            .ok()
            .map(|answer| match answer.detail.filter(|d| !d.trim().is_empty()) {
                Some(detail) if !answer.error.is_empty() => format!("{} {detail}", answer.error),
                Some(detail) => detail,
                None => answer.error,
            })
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| status_failure_message(status));
        return Err(SpotiflacError::Said(said));
    }
    serde_json::from_str::<T>(&body)
        .map(Some)
        .map_err(|_| SpotiflacError::NotSpotiflac {
            url: connection.url.clone(),
        })
}

/// What Mornye's library scan said about one folder on its side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanSaw {
    /// How many song files it sees there.
    Songs(i64),
    /// It refused the folder (Mornye scans only inside its own download folder).
    Outside,
    /// It has no such call.
    NoCall,
}

/// The reason a reading cannot tell, when it cannot.
fn cannot_tell_text<T>(reading: &Reading<T>) -> Option<&str> {
    match reading {
        Reading::CannotTell(why) => Some(why),
        Reading::Known(_) => None,
    }
}

/// `call`, or "cannot tell" when it takes longer than `bound`.
async fn bounded<T>(bound: Duration, call: impl Future<Output = Reading<T>>) -> Reading<T> {
    tokio::time::timeout(bound, call).await.unwrap_or_else(|_| {
        Reading::CannotTell(format!(
            "Mornye did not answer within {} seconds",
            bound.as_secs()
        ))
    })
}

/// Makes a folder Mornye writes into. Mornye's container usually runs as another user
/// than Octo's, so the folder is open to every user.
pub fn make_shared_folder(path: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        let mut at = Path::new(path);
        // The folder and its parents up to the incoming folder.
        for _ in 0..3 {
            let _ = std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o777));
            if at.file_name().is_some_and(|name| name == ".octo-incoming") {
                break;
            }
            match at.parent() {
                Some(parent) => at = parent,
                None => break,
            }
        }
    }
    Ok(())
}

/// One line of a Test: passed, a warning, or what is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestCheck {
    pub state: &'static str,
    pub text: String,
}

/// What a Test found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TestReport {
    pub ok: bool,
    pub message: String,
    pub version: Option<String>,
    pub extensions: Option<usize>,
    pub checks: Vec<TestCheck>,
}

impl TestReport {
    fn pass(&mut self, text: &str) {
        self.checks.push(TestCheck {
            state: "ok",
            text: text.to_string(),
        });
    }

    fn warn(&mut self, text: &str) {
        self.checks.push(TestCheck {
            state: "warn",
            text: text.to_string(),
        });
    }

    fn fail(&mut self, text: &str) {
        self.checks.push(TestCheck {
            state: "fail",
            text: text.to_string(),
        });
    }

    fn finish(mut self) -> TestReport {
        let failed = self.checks.iter().find(|check| check.state == "fail");
        self.ok = failed.is_none();
        let warnings = self.checks.iter().filter(|check| check.state == "warn").count();
        self.message = match failed {
            Some(check) => check.text.clone(),
            None if warnings == 1 => "Mornye is ready, with a warning below.".to_string(),
            None if warnings > 1 => "Mornye is ready, with warnings below.".to_string(),
            None => "Mornye is ready.".to_string(),
        };
        self
    }

    pub fn to_json(&self) -> serde_json::Value {
        json!(self)
    }
}

#[cfg(test)]
#[path = "spotiflac_client_tests.rs"]
mod tests;
