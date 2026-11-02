//! "Pass Mornye's check": Octo ends Mornye's own pause for the paused gateways, asks Mornye for a
//! song it refused so Mornye opens its check, and relays the person's pointer into Mornye's own
//! check browser through Octo's assist helper beside Mornye. Octo never sends input of its own;
//! every pointer event comes from the person's dashboard.
//!
//! One run at a time per Mornye. It ends on work: a session of an armed gateway turns active
//! (passed), every armed gateway is paused again after the press (closed), the song's job ends
//! with no browser ever opened (no check), or Mornye stops answering or its settings change.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use octo_core::spotiflac::check::{extension_id, until_words, usable};
use octo_core::spotiflac::{Job, JobState};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use super::check::PassJob;
use super::octo_folder_name;
use super::{CheckSong, Connection, SPOTIFLAC_FOLDER, SpotiflacClient, TOKEN_COOKIES, make_shared_folder};

/// How often a run looks at Mornye, its helper and the song's job.
pub const RUN_LOOK_EVERY: Duration = Duration::from_secs(2);

/// What the helper's refusal of the web token reads as.
pub const HELPER_REFUSED_TOKEN: &str =
    "Octo's helper beside Mornye refused the web token. Octo and Mornye must share SPOTIFLAC_WEB_TOKEN.";

/// Failed looks in a row that end a run as "Mornye stopped answering".
const LOOKS_THAT_MAY_FAIL: u32 = 5;

/// The longest tab or page name a dashboard may give; longer ones are cut to this.
const VIEWER_NAME_MAX: usize = 64;

/// One dashboard page on a Pass run: its tab, which stays the same across a reload, and the page
/// load itself, which is new each time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Viewer {
    pub tab: String,
    pub page: String,
}

impl Viewer {
    pub fn new(tab: &str, page: &str) -> Self {
        let cut = |name: &str| name.trim().chars().take(VIEWER_NAME_MAX).collect::<String>();
        Self {
            tab: cut(tab),
            page: cut(page),
        }
    }
}

/// A browser the helper sees, opened after the latest arm.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HelperBrowser {
    pub id: String,
    pub url: String,
    pub seq: u64,
    pub width: u32,
    pub height: u32,
}

/// One of Mornye's pause files, as the helper reads it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HelperPause {
    pub file: String,
    pub gateway: String,
    pub seconds: i64,
    pub gateway_asked: bool,
    pub stamped_after_arm: bool,
}

/// `GET /assist`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct HelperState {
    pub browsers: Vec<HelperBrowser>,
    pub paused: Vec<HelperPause>,
}

/// A pause the gateway itself asked for, which the helper keeps.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayAsked {
    pub gateway: String,
    pub until: Option<DateTime<Utc>>,
}

/// `POST /assist/arm`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ArmAnswer {
    pub ended: Vec<String>,
    pub gateway_asked: Vec<GatewayAsked>,
}

/// A frame of Mornye's check browser.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub jpeg: Vec<u8>,
    pub seq: u64,
    pub width: u32,
    pub height: u32,
}

/// Where a run stands, as the dashboard reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunState {
    Opening,
    Live,
    Passed,
    Closed,
    NoCheck,
    /// It ended for another reason, said in `why`.
    Unavailable,
}

/// One "Pass Mornye's check".
pub struct AssistRun {
    /// This run's name, which each stop from the dashboard names.
    pub id: String,
    pub connection: Connection,
    pub started: DateTime<Utc>,
    /// The usable extensions the paused checks kept, which the arm asked to free.
    pub extensions: Vec<String>,
    /// The session rows that kept them at the press: one of them turning active is a pass.
    pub covering_keys: BTreeSet<String>,
    /// The pause files the arm ended: all of them stamped again after it is a closed check.
    pub ended_files: BTreeSet<String>,
    pub gateway_asked: Vec<GatewayAsked>,
    pub song: CheckSong,
    state: Mutex<(RunState, Option<String>)>,
    browser: Mutex<Option<HelperBrowser>>,
    browser_seen: AtomicBool,
    seq: AtomicU64,
    /// Cancelled when the last person watching stops the run from the dashboard.
    stop: tokio_util::sync::CancellationToken,
    /// The dashboard pages on this run, as tab to page: each Pass takes its tab's seat (a reload
    /// takes the old page's seat over), each Close or page leaving gives up only its own.
    viewers: Mutex<HashMap<String, String>>,
    /// Woken when the run ends.
    ended: tokio::sync::Notify,
    /// The song job the run's follower took at Mornye, once it took one.
    job: Mutex<Option<PassJob>>,
}

impl AssistRun {
    pub fn state(&self) -> (RunState, Option<String>) {
        self.state.lock().clone()
    }

    pub fn browser(&self) -> Option<HelperBrowser> {
        self.browser.lock().clone()
    }

    pub fn finished(&self) -> bool {
        !matches!(self.state.lock().0, RunState::Opening | RunState::Live)
    }

    fn end(&self, state: RunState, why: Option<String>) {
        let mut slot = self.state.lock();
        if matches!(slot.0, RunState::Opening | RunState::Live) {
            *slot = (state, why);
        }
        drop(slot);
        self.ended.notify_waiters();
    }

    /// Waits for the run to end (its job cancelled and its folder removed).
    async fn finish(&self) {
        loop {
            let ended = self.ended.notified();
            tokio::pin!(ended);
            ended.as_mut().enable();
            if self.finished() {
                return;
            }
            ended.await;
        }
    }

    /// The next pointer event's number: minted here, so a reloaded page never starts over.
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// Ends its run as unavailable when the task following the run stops before ending it (a panic),
/// so a later Pass opens a new run instead of joining one nothing follows. What the run made goes
/// first: its folder at once, and its song's job cancelled in a task of its own, which ends the
/// run once the cancel has answered (or as it is dropped, when no runtime is left to run it).
struct EndsTheRun {
    run: Arc<AssistRun>,
    client: Arc<SpotiflacClient>,
    /// The song's job folder, once made.
    dir: Option<String>,
    /// The song's job at Mornye, once it took one Octo can cancel.
    job: Option<PassJob>,
}

impl EndsTheRun {
    fn new(run: &Arc<AssistRun>, client: &Arc<SpotiflacClient>) -> Self {
        Self {
            run: Arc::clone(run),
            client: Arc::clone(client),
            dir: None,
            job: None,
        }
    }
}

impl Drop for EndsTheRun {
    fn drop(&mut self) {
        if self.run.finished() {
            return;
        }
        // A Pass from now on waits for this run to end and opens a new one.
        self.run.stop.cancel();
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
        let Some(job) = self.job.take() else {
            self.run.end(
                RunState::Unavailable,
                Some("Octo stopped following Mornye's check.".into()),
            );
            return;
        };
        // Holds no job of its own, so dropping it ends the run. A job that is not over yet, or
        // whose end never gets asked, stays kept for the cleanup worker.
        let ends = EndsTheRun::new(&self.run, &self.client);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if ends.client.end_job(&ends.run.connection, &job.id).await {
                        ends.client.check.store.forget_pass_job(&job);
                    }
                    drop(ends);
                });
            }
            Err(_) => drop(ends),
        }
    }
}

impl SpotiflacClient {
    /// Ends job `id` at Mornye if it still runs, in no longer than one read of Mornye's check
    /// may take. Answers whether it is over: Mornye took the cancel, or reports the job done,
    /// failed, cancelled or unknown. A job still working, or a Mornye that did not answer, is not
    /// over.
    pub(super) async fn end_job(&self, connection: &Connection, id: &str) -> bool {
        let ask = async {
            if self.cancel(connection, id).await {
                return true;
            }
            match self.job(connection, id).await {
                Ok(None) => true,
                Ok(Some(job)) => job.state() != JobState::Working,
                Err(_) => false,
            }
        };
        tokio::time::timeout(self.check.times.call_bound, ask)
            .await
            .unwrap_or(false)
    }

    /// The cleanup worker: each time a read of Mornye's check reaches Mornye while song jobs are
    /// kept, it ends the ones stopped Pass runs left there. Off every reader's way, one sweep at
    /// a time. Ends when Octo stops.
    pub async fn clean_left_pass_jobs(
        self: Arc<Self>,
        stopping: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                _ = stopping.cancelled() => return Ok(()),
                _ = self.assist.left_jobs.notified() => {}
            }
            self.end_left_pass_jobs().await;
        }
    }

    /// Wakes the cleanup worker when song jobs are kept; called after a read that reached Mornye.
    pub(super) fn left_pass_jobs_may_end(&self) {
        if self.check.store.keeps_pass_jobs() {
            self.assist.left_jobs.notify_one();
        }
    }

    /// Ends the song jobs Pass runs left at Mornye when Octo stopped following them (a restart,
    /// or an end Mornye did not confirm), and removes their folders; the live run's own job is
    /// left to it. A job not over yet stays kept for a later sweep; one kept for a Mornye now at
    /// another address cannot be asked, so it is let go.
    pub(super) async fn end_left_pass_jobs(&self) {
        let kept = self.check.store.pass_jobs();
        if kept.is_empty() {
            return;
        }
        let live = self
            .assist
            .run
            .lock()
            .as_ref()
            .filter(|run| !run.finished())
            .map(|run| run.job.lock().clone());
        let connection = Connection::of(&self.settings());
        for left in kept {
            if live.as_ref().is_some_and(|job| job.as_ref() == Some(&left)) {
                continue;
            }
            if connection.url == left.url && !self.end_job(&connection, &left.id).await {
                continue;
            }
            remove_check_folder(&left.dir);
            self.check.store.forget_pass_job(&left);
            info!("Pass Mornye's check: ended a song job a stopped run left at Mornye");
        }
    }
}

/// Removes a Pass run's song folder, and only one Octo names that way (`octo-check-<id>`), so a
/// damaged file never removes anything else.
fn remove_check_folder(dir: &str) {
    let named_by_octo = std::path::Path::new(dir)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("octo-check-") && octo_folder_name(name));
    if named_by_octo {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Why "Pass Mornye's check" cannot start, in the words the dashboard shows.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CannotPass(pub String);

/// The runs of one client: the live or last one, and the songs runs tried and found no check in.
#[derive(Default)]
pub struct Assist {
    run: Mutex<Option<Arc<AssistRun>>>,
    tried: Mutex<BTreeSet<String>>,
    /// One start at a time: a second start waits, then answers the run the first one made.
    starting: tokio::sync::Mutex<()>,
    /// Wakes the worker that ends song jobs stopped runs left at Mornye.
    left_jobs: tokio::sync::Notify,
    /// Tests stop the follower partway, right after Mornye took the song.
    #[cfg(test)]
    stop_after_queue: AtomicBool,
}

impl SpotiflacClient {
    /// The helper's address and the token it needs, or the words saying why there is none.
    fn helper(&self) -> Result<(String, Connection), CannotPass> {
        let settings = self.settings();
        if !settings.is_on() {
            return Err(CannotPass("Mornye is off.".into()));
        }
        let connection = Connection::of(&settings);
        if connection.token.is_empty() {
            return Err(CannotPass(
                "Set a web token for Mornye (SPOTIFLAC_WEB_TOKEN) to pass its check from Octo.".into(),
            ));
        }
        let Some(url) = settings.effective_assist_url() else {
            return Err(CannotPass(
                "Passing Mornye's check from Octo needs Octo's spotiflac service, or SPOTIFLAC_ASSIST_URL."
                    .into(),
            ));
        };
        Ok((url, connection))
    }

    fn helper_request(&self, method: reqwest::Method, url: &str, token: &str) -> reqwest::RequestBuilder {
        self.http_client()
            .request(method, url)
            .timeout(self.check.times.call_bound)
            .header(
                reqwest::header::COOKIE,
                TOKEN_COOKIES
                    .iter()
                    .map(|name| format!("{name}={token}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            )
    }

    /// The helper's view: the browsers opened after the latest arm, and Mornye's pause files.
    pub async fn helper_state(&self) -> Result<HelperState, CannotPass> {
        let (base, connection) = self.helper()?;
        let answer = self
            .helper_request(reqwest::Method::GET, &format!("{base}/assist"), &connection.token)
            .send()
            .await
            .map_err(|_| CannotPass("Octo's helper beside Mornye does not answer.".into()))?;
        if !answer.status().is_success() {
            return Err(CannotPass(helper_refusal(answer.status())));
        }
        answer
            .json::<HelperState>()
            .await
            .map_err(|_| CannotPass("Octo's helper beside Mornye gave an answer Octo cannot read.".into()))
    }

    async fn helper_arm(&self, extensions: &[String]) -> Result<ArmAnswer, CannotPass> {
        let (base, connection) = self.helper()?;
        let answer = self
            .helper_request(
                reqwest::Method::POST,
                &format!("{base}/assist/arm"),
                &connection.token,
            )
            .json(&json!({ "extensions": extensions }))
            .send()
            .await
            .map_err(|_| CannotPass("Octo's helper beside Mornye does not answer.".into()))?;
        if !answer.status().is_success() {
            return Err(CannotPass(helper_refusal(answer.status())));
        }
        answer
            .json::<ArmAnswer>()
            .await
            .map_err(|_| CannotPass("Octo's helper beside Mornye gave an answer Octo cannot read.".into()))
    }

    /// The live run, or the last one, when it is for the saved connection.
    pub fn assist_run(&self) -> Option<Arc<AssistRun>> {
        let run = self.assist.run.lock().clone()?;
        (run.connection == Connection::of(&self.settings())).then_some(run)
    }

    /// One page leaves the live run; the last one to leave stops it (its song's job cancelled
    /// and its folder removed). `run` is the run the page was on, or None while its start had not
    /// answered yet. A stop for another run, or from a page whose tab a newer page has taken
    /// over, leaves the run as it is. Answers whether this stopped it.
    pub async fn stop_pass(&self, run: Option<&str>, viewer: &Viewer) -> bool {
        // After a start still on its way, so a Close while opening stops the run it makes.
        let _starting = self.assist.starting.lock().await;
        let named = run;
        let Some(run) = self.assist_run().filter(|run| !run.finished()) else {
            return false;
        };
        if named.is_some_and(|id| id != run.id) {
            return false;
        }
        let left = {
            let mut viewers = run.viewers.lock();
            if viewers.get(&viewer.tab) == Some(&viewer.page) {
                viewers.remove(&viewer.tab);
            }
            viewers.len()
        };
        if left > 0 {
            // Someone else is still passing it from another page.
            return false;
        }
        run.stop.cancel();
        true
    }

    /// How many refused songs a run could still ask for.
    pub fn check_songs_left(&self) -> usize {
        let tried = self.assist.tried.lock();
        self.check
            .store
            .snapshot()
            .check_songs
            .iter()
            .filter(|song| !tried.contains(&song.link))
            .count()
    }

    /// Starts "Pass Mornye's check", or answers the run that is already live. `download_path` is
    /// Octo's music folder, where the song's job folder goes; `viewer` is the page that asked.
    pub async fn start_pass(
        self: &Arc<Self>,
        download_path: &str,
        viewer: &Viewer,
    ) -> Result<Arc<AssistRun>, CannotPass> {
        let (_, connection) = self.helper()?;
        // Two starts at once (two tabs, Close then Pass) never arm twice: the second waits here
        // and answers the run the first made.
        let _starting = self.assist.starting.lock().await;
        if let Some(run) = self.assist_run().filter(|run| !run.finished()) {
            if !run.stop.is_cancelled() {
                // Another page joins the run already open, or a reloaded one takes its seat back.
                run.viewers.lock().insert(viewer.tab.clone(), viewer.page.clone());
                return Ok(run);
            }
            // A run just stopped is still cleaning up: a new one starts once it has.
            run.finish().await;
        }
        if download_path.trim().is_empty() {
            return Err(CannotPass("Octo has no music folder yet.".into()));
        }
        let view = self
            .check_view(Some(Instant::now()))
            .await
            .ok_or_else(|| CannotPass("Mornye is off.".into()))?;
        let now = Utc::now();
        let settings = self.settings();
        let usable_ids = usable(&settings.effective_services(), &view.services);
        if view.pause(&settings.effective_services(), now).is_none() {
            return Err(CannotPass(
                "Mornye's check is not paused, so there is nothing to pass.".into(),
            ));
        }
        let rows = view.rows.known().cloned().unwrap_or_default();
        let covering: Vec<_> = rows
            .iter()
            .filter(|row| usable_ids.iter().any(|id| row.covers(id, now)))
            .collect();
        let extensions: Vec<String> = usable_ids
            .iter()
            .filter(|id| covering.iter().any(|row| row.extensions.contains(*id)))
            .cloned()
            .collect();
        let covering_keys: BTreeSet<String> = covering.iter().map(|row| row.key.clone()).collect();
        let song = {
            let tried = self.assist.tried.lock();
            self.check
                .store
                .snapshot()
                .check_songs
                .into_iter()
                .find(|song| !tried.contains(&song.link))
        };
        let Some(song) = song else {
            return Err(CannotPass(
                "Nothing has asked Mornye since its check stopped passing; the next song that does opens it."
                    .into(),
            ));
        };
        let armed = self.helper_arm(&extensions).await?;
        if armed.ended.is_empty() {
            let words = match armed.gateway_asked.iter().filter_map(|asked| asked.until).min() {
                Some(until) => format!(
                    "The gateway asked Mornye to wait until {}, so Octo leaves the check until then.",
                    until_words(until, now)
                ),
                None => "Mornye's check has no pause Octo could end.".to_string(),
            };
            return Err(CannotPass(words));
        }
        // The pause is gone from Mornye's disk: the next read lets the song through.
        let _ = self.check_view(Some(Instant::now())).await;
        let run = Arc::new(AssistRun {
            id: uuid::Uuid::new_v4().simple().to_string(),
            connection,
            started: now,
            extensions,
            covering_keys,
            ended_files: armed.ended.iter().cloned().collect(),
            gateway_asked: armed.gateway_asked,
            song,
            state: Mutex::new((RunState::Opening, None)),
            browser: Mutex::new(None),
            browser_seen: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            stop: tokio_util::sync::CancellationToken::new(),
            viewers: Mutex::new(HashMap::from([(viewer.tab.clone(), viewer.page.clone())])),
            ended: tokio::sync::Notify::new(),
            job: Mutex::new(None),
        });
        *self.assist.run.lock() = Some(Arc::clone(&run));
        info!(
            "Pass Mornye's check: asking Mornye for '{} - {}'",
            run.song.artist, run.song.title
        );
        let client = Arc::clone(self);
        let follow = Arc::clone(&run);
        let download_path = download_path.to_string();
        tokio::spawn(async move { client.follow_pass(follow, download_path).await });
        Ok(run)
    }

    /// Asks Mornye for the run's song and follows it until the run ends, then cleans up.
    async fn follow_pass(self: Arc<Self>, run: Arc<AssistRun>, download_path: String) {
        let mut ends = EndsTheRun::new(&run, &self);
        let _slot = tokio::select! {
            slot = self.slot() => slot,
            _ = run.stop.cancelled() => {
                run.end(RunState::Unavailable, Some("Stopped from the dashboard.".into()));
                return;
            }
        };
        self.count_job();
        let settings = self.settings();
        let job_dir = format!(
            "{}/{SPOTIFLAC_FOLDER}/octo-check-{}",
            download_path.trim_end_matches('/'),
            uuid::Uuid::new_v4().simple()
        );
        if let Err(e) = make_shared_folder(&job_dir) {
            run.end(
                RunState::Unavailable,
                Some(format!("Octo cannot make a folder for the song: {e}.")),
            );
            return;
        }
        ends.dir = Some(job_dir.clone());
        let their_dir = octo_core::spotiflac::path_for_spotiflac(
            &job_dir,
            &download_path,
            settings.effective_folder().as_deref(),
        );
        let job = match self
            .queue(&run.connection, &run.song.link, &settings, &their_dir)
            .await
        {
            Ok(job) => job,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&job_dir);
                run.end(
                    RunState::Unavailable,
                    Some(format!("Mornye did not take the song: {e}")),
                );
                return;
            }
        };
        if job.is_pollable() {
            let kept = PassJob {
                url: run.connection.url.clone(),
                id: job.id.clone(),
                dir: job_dir.clone(),
            };
            *run.job.lock() = Some(kept.clone());
            self.check.store.keep_pass_job(kept.clone());
            ends.job = Some(kept);
        }
        #[cfg(test)]
        if self.assist.stop_after_queue.load(Ordering::SeqCst) {
            panic!("a test stops the follower here");
        }
        let outcome = self.watch_pass(&run, &job).await;
        if let Some(kept) = &ends.job
            && self.end_job(&run.connection, &job.id).await
        {
            self.check.store.forget_pass_job(kept);
        }
        let _ = std::fs::remove_dir_all(&job_dir);
        match outcome.0 {
            RunState::Passed => {
                self.assist.tried.lock().clear();
                for id in &run.extensions {
                    self.check.store.brought(Some(&extension_id(id)), &run.song.link);
                }
            }
            RunState::NoCheck => {
                self.assist.tried.lock().insert(run.song.link.clone());
            }
            _ => {}
        }
        info!("Pass Mornye's check ended: {:?}", outcome.0);
        // Last, so a finished run has already cancelled its job and removed its folder.
        run.end(outcome.0, outcome.1);
    }

    /// Looks at the helper, the song's job and Mornye's check until the run's outcome shows.
    async fn watch_pass(&self, run: &AssistRun, job: &Job) -> (RunState, Option<String>) {
        let mut failures = 0;
        // A look the helper did not answer: a job that then ends with no browser seen may have
        // opened one Octo could not see, so its song is not counted as tried.
        let mut helper_missed = false;
        let started = Instant::now();
        let bound = self.song_timeout();
        loop {
            tokio::select! {
                _ = run.stop.cancelled() => {
                    return (RunState::Unavailable, Some("Stopped from the dashboard.".into()));
                }
                _ = tokio::time::sleep(self.run_look_every()) => {}
            }
            if Connection::of(&self.settings()) != run.connection || !self.settings().is_on() {
                return (RunState::Unavailable, Some("Mornye's settings changed.".into()));
            }
            if started.elapsed() >= bound {
                return (
                    RunState::Unavailable,
                    Some(format!(
                        "Mornye took longer than {} seconds for the song, so Octo stopped.",
                        bound.as_secs()
                    )),
                );
            }
            let look_started = Instant::now();
            let helper = self.helper_state().await;
            helper_missed |= helper.is_err();
            let job_now = if job.is_pollable() {
                self.job(&run.connection, &job.id).await
            } else {
                Ok(None)
            };
            let view = self.check_view(Some(look_started)).await;
            if helper.is_err() && job_now.is_err() {
                failures += 1;
                if failures >= LOOKS_THAT_MAY_FAIL {
                    return (RunState::Unavailable, Some("Mornye stopped answering.".into()));
                }
                continue;
            }
            failures = 0;
            if let Ok(helper) = &helper {
                if let Some(browser) = helper.browsers.last() {
                    run.browser_seen.store(true, Ordering::SeqCst);
                    *run.browser.lock() = Some(browser.clone());
                    let mut state = run.state.lock();
                    if state.0 == RunState::Opening {
                        state.0 = RunState::Live;
                    }
                }
                let stamped: BTreeSet<&str> = helper
                    .paused
                    .iter()
                    .filter(|pause| pause.stamped_after_arm)
                    .map(|pause| pause.file.as_str())
                    .collect();
                if !run.ended_files.is_empty()
                    && run.ended_files.iter().all(|file| stamped.contains(file.as_str()))
                {
                    return (RunState::Closed, None);
                }
            }
            if let Some(view) = &view
                && let Some(rows) = view.rows.known()
                && rows
                    .iter()
                    .any(|row| run.covering_keys.contains(&row.key) && row.working())
            {
                return (RunState::Passed, None);
            }
            let ended = match &job_now {
                Ok(Some(now)) => !matches!(now.state(), JobState::Working),
                Ok(None) => job.is_pollable(),
                Err(_) => false,
            };
            if ended {
                return if run.browser_seen.load(Ordering::SeqCst) {
                    (RunState::Closed, None)
                } else if helper_missed {
                    (
                        RunState::Unavailable,
                        Some("Octo's helper beside Mornye did not answer while the song was asked.".into()),
                    )
                } else {
                    (RunState::NoCheck, None)
                };
            }
        }
    }

    /// A frame of the run's browser newer than `after`, or None.
    pub async fn assist_frame(&self, browser: &str, after: u64) -> Result<Option<Frame>, CannotPass> {
        if !is_browser_id(browser) {
            return Err(CannotPass("That browser is not one Octo knows.".into()));
        }
        let (base, connection) = self.helper()?;
        let answer = self
            .helper_request(
                reqwest::Method::GET,
                &format!("{base}/assist/frame?browser={browser}&after={after}"),
                &connection.token,
            )
            .send()
            .await
            .map_err(|_| CannotPass("Octo's helper beside Mornye does not answer.".into()))?;
        if answer.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !answer.status().is_success() {
            return Err(CannotPass(helper_refusal(answer.status())));
        }
        let header = |name: &str| {
            answer
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let (seq, width, height) = (
            header("x-frame-seq"),
            header("x-frame-width"),
            header("x-frame-height"),
        );
        let jpeg = answer
            .bytes()
            .await
            .map_err(|_| CannotPass("The frame did not arrive whole.".into()))?;
        Ok(Some(Frame {
            jpeg: jpeg.to_vec(),
            seq,
            width: width as u32,
            height: height as u32,
        }))
    }

    /// Passes one of the person's pointer events to the run's browser.
    pub async fn assist_input(&self, browser: &str, kind: &str, x: f64, y: f64) -> Result<(), CannotPass> {
        if !is_browser_id(browser) {
            return Err(CannotPass("That browser is not one Octo knows.".into()));
        }
        if !matches!(kind, "mouseMoved" | "mousePressed" | "mouseReleased")
            || !x.is_finite()
            || !y.is_finite()
        {
            return Err(CannotPass("Octo only passes moves, presses and releases.".into()));
        }
        let run = self
            .assist_run()
            .filter(|run| !run.finished())
            .ok_or_else(|| CannotPass("Nothing is being passed now.".into()))?;
        let (base, connection) = self.helper()?;
        let body = json!({ "browser": browser, "seq": run.next_seq(), "type": kind, "x": x, "y": y });
        let answer = self
            .helper_request(
                reqwest::Method::POST,
                &format!("{base}/assist/input"),
                &connection.token,
            )
            .json(&body)
            .send()
            .await
            .map_err(|_| CannotPass("Octo's helper beside Mornye does not answer.".into()))?;
        if answer.status().is_success() {
            return Ok(());
        }
        let status = answer.status();
        let said = answer
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|value| value.get("error").and_then(|e| e.as_str()).map(str::to_string));
        warn!("Octo's helper refused a pointer event: {status}");
        Err(CannotPass(said.unwrap_or_else(|| helper_refusal(status))))
    }

    fn run_look_every(&self) -> Duration {
        self.assist_look_every.unwrap_or(RUN_LOOK_EVERY)
    }
}

/// A browser id the helper gives: the browser's process id, digits only.
fn is_browser_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 10 && id.chars().all(|c| c.is_ascii_digit())
}

/// The helper's refusal, in words.
fn helper_refusal(status: reqwest::StatusCode) -> String {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        HELPER_REFUSED_TOKEN.into()
    } else {
        format!("Octo's helper beside Mornye answered {}.", status.as_u16())
    }
}

#[cfg(test)]
#[path = "assist_tests.rs"]
mod tests;
