//! The process and status actions: the service health dots and the downloads card (`status`), the
//! library path chain (`library-status`), server discovery, Lidarr's choices and connection test,
//! the restart, the test notification, the fetched-songs log, the hearted downloads, and the two
//! recovery levers (clear rejected peers, clear the metadata caches).

use std::collections::HashSet;
use std::time::Duration;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use chrono::Utc;
use octo_core::common::dotnet::is_blank;
use octo_core::fingerprint::heart_gate::WaitingFor;
use octo_core::lidarr::lidarr_heart_acquisition_service::{IndexerHealth, indexer_health};
use octo_core::settings::HeartDownloadSource;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use super::json_body::bind_body;
use super::json_body::{FromBody, Shape, text_record};
use super::plumbing::{camel_case_value, object_result, ok};
use crate::app::AppState;
use crate::http::error::json_ok;
use crate::middleware::admin_sign_in_gate::{Caller, caller_of};
use crate::services::admin::AdminCaller;
use crate::services::admin::directory_browser::is_writable;
use crate::services::common::acquisition_tracker::key_of;
use crate::services::common::heart_acquisition_coordinator::waiting_line;
use crate::services::http_client_factory::{connect_failure_message, timeout_message};
use crate::services::lidarr::NO_INDEXER_SEARCHES;
use crate::services::soulseek::soulseek_link::ISoulseekLink;
use crate::services::subsonic::subsonic_response_builder::acquisition_json;
use crate::services::you_tube::you_tube_resolver::FRESH_HEADER;

/// The downloads card's line while imported downloads go unchecked, with the switch that turns
/// the checks on.
pub const VERIFICATION_OFF: &str = "Octo is not checking imported downloads by their sound. Turn it on";
/// The downloads card's line while AcoustID is off.
pub const ACOUST_ID_OFF: &str = "AcoustID is off: Octo checks what played against the catalog's own clip of each song, and keeps a copy only when it is the very master you played";
/// The downloads card's line while a heart waits for AcoustID to answer.
pub const ACOUST_ID_HOLDS: &str = "AcoustID is not answering; hearts wait for it";
/// The downloads card's line once AcoustID refused the user's submission key.
pub const SUBMISSION_KEY_REFUSED: &str = "AcoustID refused your submission key";
/// The yt-dlp shim's line when it is too old to answer a fresh lookup.
pub const OLD_SHIM: &str = "Update the yt-dlp shim so Octo can check YouTube";
/// The setting the verification line's switch turns on.
const VERIFY_DOWNLOADS_SETTING: &str = "Soulseek.VerifyDownloads";

/// `GET /api/admin/discover-servers`: scans the local network for Subsonic/Navidrome servers so
/// the setup UI can offer a detected upstream URL instead of requiring it typed by hand. Empty
/// when Octo cannot see the LAN (a Docker bridge network, for example).
pub async fn discover_servers(State(state): State<AppState>) -> Response {
    let servers = state.subsonic_discovery.scan().await;
    ok(&json!({ "servers": servers }))
}

/// `GET /api/admin/library-status`: where downloads will actually land, and why. Octo fronts
/// Navidrome, so the library Navidrome scans is the source of truth and the default; this
/// states the whole chain (what Navidrome reports, whether Octo can see it, what is therefore
/// in effect) so "my downloads went nowhere" is answerable from the UI instead of the logs.
pub async fn library_status(State(state): State<AppState>) -> Response {
    let subsonic = state.settings.current().subsonic.clone();
    let configured = state.settings.raw("Library:DownloadPath").unwrap_or_default();
    let identity = &state.navidrome_identity;

    // Cheap when already detected: this is cached inside the service.
    identity.detect_music_folder(false).await;

    let reported = identity.detected_music_folder();
    let effective = identity.effective_download_path(&configured);
    let is_dir = |path: &str| std::path::Path::new(path).is_dir();
    let libraries: Vec<serde_json::Value> = identity
        .known_libraries()
        .into_iter()
        .map(|library| {
            json!({
                "id": library.id,
                "name": library.name,
                "folder": library.folder,
                "visible": is_dir(&library.folder),
            })
        })
        .collect();

    ok(&json!({
        "autoDetect": subsonic.auto_detect_download_path,
        "pinnedLibraryPath": subsonic.library_path,
        "navidromeReports": reported,
        // Navidrome describes paths as IT sees them. Whether Octo can see the same path is the
        // difference between downloads being scanned and vanishing, so it is stated rather than
        // implied.
        "visibleToOcto": reported.as_deref().is_some_and(|r| !r.is_empty() && is_dir(r)),
        "configuredFallback": configured,
        "effectiveDownloadPath": effective,
        "writable": !effective.is_empty() && is_dir(&effective) && is_writable(&effective),
        "rescanAuthenticated": identity.get_scan_auth().is_some(),
        "libraries": libraries,
    }))
}

/// `POST /api/admin/test-notification`: sends a test notification through every sink so URLs
/// and tokens can be verified without waiting for a real download. Reports each sink's outcome,
/// including the transport's real error text on failure.
pub async fn test_notification(State(state): State<AppState>) -> Response {
    let results = state.notifications.send_test().await;
    ok(&json!({ "results": results }))
}

/// `GET /api/admin/downloads`: the running log of songs Octo has fetched, newest first. Each
/// entry's saved download log stays out of the list (it can run to hundreds of lines); `hasLog`
/// says one was kept, and `downloads/log` reads it.
pub async fn downloads(State(state): State<AppState>) -> Response {
    let recent = state.download_history.get_recent(200);
    // The entries are written to disk PascalCase; the API answer is camelCase, with the
    // report's dictionaries keeping their keys.
    let downloads: Vec<serde_json::Value> = recent
        .iter()
        .map(|entry| {
            let value = serde_json::to_value(entry).unwrap_or_default();
            let mut value = camel_case_value(value, &["fields", "stageSeconds"]);
            if let Some(fields) = value.as_object_mut() {
                fields.remove("log");
                fields.insert(
                    "hasLog".into(),
                    serde_json::Value::Bool(entry.log.as_ref().is_some_and(|log| !log.is_empty())),
                );
            }
            value
        })
        .collect();
    ok(&json!({ "downloads": downloads }))
}

/// `GET /api/admin/acquisitions`: every hearted download in flight or ended in the last half
/// hour, everyone's, newest first. The rows the app reads through getAcquisitions, plus the
/// provider key, and who asked only while Record who asked is on, the rule the fetched-songs
/// log follows too. A row says `keepAnyway` while someone waiting in its holds may keep it
/// anyway, `heldTracks` on a hearted album holding tracks, and `stopWaiting` while a kept copy
/// of its song waits for Lidarr; who they are is read from `acquisitions/holders`.
pub async fn acquisitions(State(state): State<AppState>) -> Response {
    let show_askers = state.settings.current().subsonic.record_requested_by;
    let coordinator = &state.heart_acquisition_coordinator;
    let held = coordinator.keep_marks("");
    // The rows any hold lists: a hold's own song's, and a held album track's album's.
    let listed: HashSet<String> = state
        .soulseek_holds
        .snapshot()
        .iter()
        .flat_map(|record| {
            let album = record
                .album_track
                .as_ref()
                .and_then(|track| track.album_key.split_once(':'))
                .map(|(provider, id)| key_of(provider, id));
            std::iter::once(key_of(&record.provider, &record.external_id)).chain(album)
        })
        .collect();
    let rows: Vec<serde_json::Value> = state
        .acquisition_tracker
        .all()
        .into_iter()
        .map(|mut row| {
            // The holds decide, never the row's own flag, which stays set after a keep is asked.
            row.keep_anyway = listed.contains(&row.key) && !coordinator.keepers(&row.key).is_empty();
            row.held_tracks = held
                .held_tracks(&row.key)
                .map(|count| i32::try_from(count).unwrap_or(i32::MAX));
            row.stop_waiting = !state
                .lidarr_hearts
                .stop_waiters(&row.provider, &row.external_id)
                .is_empty();
            row
        })
        .map(|row| {
            let mut fields = acquisition_json(&row);
            fields.insert("provider".into(), json!(row.provider));
            fields.insert("externalId".into(), json!(row.external_id));
            if show_askers && !row.requested_by.is_empty() {
                fields.insert("requestedBy".into(), json!(row.requested_by));
            }
            serde_json::Value::Object(fields)
        })
        .collect();
    ok(&json!({ "acquisitions": rows }))
}

/// Health of one backing service. `configured` is false for an optional service nobody set
/// up, which the dashboard shows as off rather than as a failure. `indexers` (sent only when
/// set) says Lidarr answers and its warning is about its indexers, not its setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ServiceProbe {
    ok: bool,
    detail: String,
    warning: bool,
    configured: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    indexers: bool,
}

impl ServiceProbe {
    fn new(ok: bool, detail: impl Into<String>) -> Self {
        ServiceProbe {
            ok,
            detail: detail.into(),
            warning: false,
            configured: true,
            indexers: false,
        }
    }

    fn warning(mut self, warning: bool) -> Self {
        self.warning = warning;
        self
    }

    fn unconfigured(mut self) -> Self {
        self.configured = false;
        self
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The message of an HTTP client failure, for a probe's detail.
fn probe_failure(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        timeout_message(PROBE_TIMEOUT)
    } else {
        connect_failure_message(error)
    }
}

/// A GET with the default client and a 5 second timeout: the status, or what went wrong.
async fn probe_get(state: &AppState, url: &str) -> Result<reqwest::StatusCode, String> {
    let request = state
        .http
        .get(url)
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| format!("An invalid request URI was provided. {e}"))?;
    state
        .http
        .execute(request)
        .await
        .map(|response| response.status())
        .map_err(|e| probe_failure(&e))
}

async fn probe_navidrome(state: &AppState) -> ServiceProbe {
    let url = state.settings.current().subsonic.url.clone().unwrap_or_default();
    if is_blank(&url) {
        return ServiceProbe::new(false, "Not set up yet. Enter your server under Music server.");
    }
    // Navidrome's /rest/ping requires auth, but it returns 200 with an error body even on bad
    // credentials, which proves connectivity.
    let target = format!(
        "{}/rest/ping?u=probe&p=probe&v=1.16.1&c=octo&f=json",
        url.trim_end_matches('/')
    );
    match probe_get(state, &target).await {
        Ok(status) => ServiceProbe::new(
            status.is_success(),
            format!("HTTP {} from {url}", status.as_u16()),
        ),
        Err(message) => ServiceProbe::new(false, message),
    }
}

async fn probe_slskd(state: &AppState) -> ServiceProbe {
    if state.soulseek_link.is_off() {
        return ServiceProbe::new(true, "Off. Optional.").unconfigured();
    }
    // slskd answering is not slskd able to search: it can be up and out of Soulseek.
    let reading = state.soulseek_link.read(true).await;
    let hold = state.settings.current().soulseek.effective_outage_hold_hours();
    let (up, warning, detail) = octo_core::soulseek::soulseek_link::describe(reading.as_ref(), hold);
    ServiceProbe::new(up, detail).warning(warning)
}

async fn probe_lidarr(state: &AppState) -> ServiceProbe {
    let settings = state.settings.current();
    let lidarr = &settings.lidarr;
    let lidarr_enabled = settings
        .subsonic
        .effective_heart_download_sources()
        .iter()
        .any(|step| {
            step.source == HeartDownloadSource::Lidarr
                && (step.song_enabled == Some(true) || step.album_enabled == Some(true))
        });
    // An absent optional service is a calm state, not a warning: most installs never configure
    // Lidarr and their dashboard should not carry a permanent yellow dot for it. Yellow means
    // "you enabled it but haven't finished setting it up": incomplete config, as opposed to an
    // outage.
    if lidarr.base_url.as_deref().is_none_or(is_blank) || lidarr.api_key.as_deref().is_none_or(is_blank) {
        return if lidarr_enabled {
            ServiceProbe::new(true, "selected but not configured").warning(true)
        } else {
            ServiceProbe::new(true, "Not set up. Optional.").unconfigured()
        };
    }
    if lidarr_enabled
        && (lidarr.root_folder_path.as_deref().is_none_or(is_blank)
            || lidarr.quality_profile_id <= 0
            || lidarr.metadata_profile_id <= 0)
    {
        return ServiceProbe::new(true, "select a root folder and profiles").warning(true);
    }
    if !state.lidarr_client.is_reachable().await {
        return ServiceProbe::new(false, "unreachable / API key invalid");
    }
    match lidarr_indexer_lines(state).await {
        Some(lines) => {
            let mut probe = ServiceProbe::new(true, lines).warning(true);
            probe.indexers = true;
            probe
        }
        None => ServiceProbe::new(true, "reachable"),
    }
}

/// What Lidarr's card says of its indexers, or None when nothing is wrong with them: hearts
/// waiting for them (their holds), and Lidarr's own health read now saying no indexer can search.
/// A health read that fails says nothing; the card already says Lidarr answers.
async fn lidarr_indexer_lines(state: &AppState) -> Option<String> {
    let waiting = state
        .soulseek_holds
        .snapshot()
        .iter()
        .any(|record| record.waiting_for == Some(WaitingFor::LidarrIndexers));
    let none_can_search = state
        .lidarr_client
        .health()
        .await
        .is_ok_and(|checks| indexer_health(&checks) == IndexerHealth::NoneCanSearch);
    let lines: Vec<&str> = [
        waiting.then(|| waiting_line(WaitingFor::LidarrIndexers)),
        none_can_search.then_some(NO_INDEXER_SEARCHES),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!lines.is_empty()).then(|| lines.join(". "))
}

async fn probe_you_tube_shim(state: &AppState) -> ServiceProbe {
    let shim = state
        .settings
        .raw("YouTube:ShimUrl")
        .unwrap_or_else(|| "http://yt-dlp-shim:8080".to_string());
    let target = format!("{}/health", shim.trim_end_matches('/'));
    match probe_get(state, &target).await {
        Ok(status) if status.is_success() && shim_is_old(state, &shim).await => {
            ServiceProbe::new(true, OLD_SHIM).warning(true)
        }
        Ok(status) => ServiceProbe::new(status.is_success(), format!("HTTP {}", status.as_u16())),
        Err(message) => ServiceProbe::new(false, message),
    }
}

/// Whether the shim is one from before fresh lookups, which hearts need to check what played: a
/// shim that has them marks every answer to `fresh=1` with its header, its refusal of a request
/// naming no video included, so asking that resolves nothing. An older shim answers without it.
/// A shim that does not answer the question is not called old: its health already said it works.
async fn shim_is_old(state: &AppState, shim: &str) -> bool {
    let target = format!("{}/facts?fresh=1&bg=1", shim.trim_end_matches('/'));
    let Ok(request) = state.http.get(&target).timeout(PROBE_TIMEOUT).build() else {
        return false;
    };
    match state.http.execute(request).await {
        Ok(answer) => answer.headers().get(FRESH_HEADER).and_then(|v| v.to_str().ok()) != Some("1"),
        Err(_) => false,
    }
}

/// octo-sonic, which reads every song for its transition profile and, while Sounds alike is
/// on, for how it sounds. Probed either way: it runs for transitions with Sounds alike off.
async fn probe_sonic(state: &AppState) -> ServiceProbe {
    let radio = state.settings.current().radio_sources.clone();
    let target = format!("{}/health", radio.sonic_url.trim_end_matches('/'));
    let request = match state.http.get(&target).timeout(PROBE_TIMEOUT).build() {
        Ok(request) => request,
        Err(e) => return ServiceProbe::new(false, format!("An invalid request URI was provided. {e}")),
    };
    let response = match state.http.execute(request).await {
        Ok(response) => response,
        Err(e) => return ServiceProbe::new(false, probe_failure(&e)),
    };
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    let document =
        serde_json::from_slice::<serde_json::Value>(if body.is_empty() { b"{}" } else { &body }).ok();
    let says = |name: &str| {
        document
            .as_ref()
            .and_then(|d| d.get(name))
            .and_then(serde_json::Value::as_bool)
    };
    if status.is_success() {
        let sound = radio.sounds_alike && says("soundsAlike") != Some(false);
        let profiles = document
            .as_ref()
            .and_then(|d| d.get("transitionVersion"))
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|v| v > 0);
        return match (sound, profiles) {
            (true, _) => ServiceProbe::new(true, "reachable"),
            (false, true) => ServiceProbe::new(true, "running for transitions; Sounds alike is off"),
            (false, false) => ServiceProbe::new(true, "Sounds alike is off").unconfigured(),
        };
    }
    // An octo-sonic from before transition profiles, turned off in .env
    // (RADIO_SOUNDS_ALIKE=false): off, like the switch, not a fault.
    if says("off") == Some(true) {
        return ServiceProbe::new(true, "Sounds alike is off in .env").unconfigured();
    }
    ServiceProbe::new(
        false,
        if status.as_u16() == 503 {
            "cannot see the music folder".to_string()
        } else {
            format!("HTTP {}", status.as_u16())
        },
    )
}

async fn probe_last_fm(state: &AppState) -> ServiceProbe {
    let key = state.settings.current().last_fm.api_key.clone();
    // Optional: without a key search still shows your library and Deezer albums, and radio
    // falls back to Navidrome. Red here read as a broken install.
    if key.is_empty() {
        return ServiceProbe::new(true, "No API key, so song discovery and radio are off. Optional.")
            .unconfigured();
    }
    let target = format!(
        "https://ws.audioscrobbler.com/2.0/?method=track.getInfo&artist=cher&track=believe&api_key={key}&format=json"
    );
    match probe_get(state, &target).await {
        Ok(status) => ServiceProbe::new(status.is_success(), format!("HTTP {}", status.as_u16())),
        Err(message) => ServiceProbe::new(false, message),
    }
}

/// `now` in the round-trip form with an offset: seven fraction digits and `+00:00`.
fn round_trip_offset(now: chrono::DateTime<Utc>) -> String {
    format!(
        "{}.{:07}+00:00",
        now.format("%Y-%m-%dT%H:%M:%S"),
        now.timestamp_subsec_nanos() / 100
    )
}

/// `GET /api/admin/status`: a quick health snapshot of each backing service, so the dashboard
/// shows at a glance whether Octo can reach Navidrome, slskd, Lidarr, the yt-dlp shim, octo-sonic
/// and Last.fm, and the downloads card's lines (`downloads`, see [`download_lines`]). Open without
/// a sign-in, but then it answers only `ok` and `configured` per service (see
/// [`anonymous_status`]).
pub async fn get_status(State(state): State<AppState>, caller: Caller) -> Response {
    if !caller_of(&caller).is_some_and(AdminCaller::is_admin) {
        return json_ok(&anonymous_status(&state).await);
    }
    let (mut services, mornye) = tokio::join!(probe_all(&state), mornye_row(&state));
    // Only for an admin, so a signed-out caller never learns that a Mornye exists, and only while
    // Mornye is switched on: a server that never set it up lists no row for it.
    if let Some(mornye) = mornye {
        services.insert("mornye".to_string(), mornye);
    }
    json_ok(&json!({
        "octo": ServiceProbe::new(true, "Octo is responding"),
        "services": services,
        "downloads": download_lines(&state),
        "time": round_trip_offset(Utc::now()),
    }))
}

/// Mornye's row of the admin status, kept between answers so a slow Mornye never holds the
/// status up: one probe at a time, each keeping its row for the address it asked.
#[derive(Default)]
pub struct MornyeRowKeeper {
    kept: parking_lot::Mutex<Option<(String, serde_json::Value)>>,
    asking: std::sync::atomic::AtomicBool,
    answered: tokio::sync::Notify,
}

impl MornyeRowKeeper {
    /// How long a status answer waits for a probe of Mornye before it uses the kept row.
    pub const WAIT: Duration = Duration::from_secs(1);

    /// The row the last probe of the Mornye at `url` kept.
    pub(super) fn kept_for(&self, url: &str) -> Option<serde_json::Value> {
        self.kept
            .lock()
            .as_ref()
            .filter(|(asked, _)| asked == url)
            .map(|(_, row)| row.clone())
    }
}

/// Ends a probe: lets the next one start and wakes the answers waiting for it, also when the
/// probe stopped short.
struct ProbeDone(AppState);

impl Drop for ProbeDone {
    fn drop(&mut self) {
        let keeper = &self.0.mornye_row;
        keeper.asking.store(false, std::sync::atomic::Ordering::Release);
        keeper.answered.notify_waiters();
    }
}

/// Mornye's row for an admin: None while Mornye is switched off. Switched on with no address, a
/// warning that says so. Otherwise the row of a probe that answers within
/// [`MornyeRowKeeper::WAIT`], else the row the last probe of this address kept, else a warning
/// that Mornye has not answered yet; a probe still asking keeps its row for the next answer.
async fn mornye_row(state: &AppState) -> Option<serde_json::Value> {
    let settings = state.spotiflac.settings();
    if !settings.enabled {
        return None;
    }
    if !settings.is_on() {
        return Some(json!(
            ServiceProbe::new(
                true,
                "Switched on, but it has no address. Set it on the Mornye page."
            )
            .warning(true)
        ));
    }
    let url = settings.effective_url();
    let keeper = &state.mornye_row;
    let answered = keeper.answered.notified();
    tokio::pin!(answered);
    answered.as_mut().enable();
    if !keeper.asking.swap(true, std::sync::atomic::Ordering::AcqRel) {
        let state = state.clone();
        let asked = url.clone();
        tokio::spawn(async move {
            let done = ProbeDone(state.clone());
            let row = probe_mornye(&state).await;
            *state.mornye_row.kept.lock() = Some((asked, row));
            drop(done);
        });
    }
    let _ = tokio::time::timeout(MornyeRowKeeper::WAIT, answered).await;
    Some(
        keeper
            .kept_for(&url)
            .unwrap_or_else(|| json!(ServiceProbe::new(true, "Mornye has not answered yet.").warning(true))),
    )
}

/// Mornye's row: off (optional), unreachable, its check paused (with "pass" when the dashboard can
/// pass it), its utilities extension missing, or its name and version.
pub(super) async fn probe_mornye(state: &AppState) -> serde_json::Value {
    if !state.spotiflac.settings().is_on() {
        return json!(ServiceProbe::new(true, "Off. Optional.").unconfigured());
    }
    let status = match tokio::time::timeout(PROBE_TIMEOUT, super::spotiflac::mornye_status(state, "")).await {
        Ok(status) => status,
        Err(_) => {
            return json!(ServiceProbe::new(
                false,
                "Mornye did not answer within 5 seconds."
            ));
        }
    };
    let text = |key: &str| {
        status
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    if !status.get("reachable").and_then(|v| v.as_bool()).unwrap_or(false) {
        return json!(ServiceProbe::new(false, text("problem")));
    }
    if status
        .get("utilitiesMissing")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return json!(
            ServiceProbe::new(
                true,
                "Mornye needs its utilities extension, which your registry does not list."
            )
            .warning(true)
        );
    }
    let now = Utc::now();
    let next_try = status
        .get("checks")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|check| check.get("pausedUntilUtc").and_then(|v| v.as_str()))
        .filter_map(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|t| t.with_timezone(&Utc))
        .min();
    let flag = |key: &str| status.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    if flag("paused") {
        let detail = match next_try {
            Some(until) => format!(
                "Check paused; next try {}",
                octo_core::spotiflac::check::until_words(until, now)
            ),
            None => "Check paused".to_string(),
        };
        let mut row = json!(ServiceProbe::new(true, detail).warning(true));
        // The same rule as the dashboard's Pass button, decided once in the status.
        if flag("passable")
            && let Some(object) = row.as_object_mut()
        {
            object.insert("action".to_string(), json!("pass"));
        }
        return row;
    }
    let version = text("version");
    json!(ServiceProbe::new(
        true,
        if version.is_empty() {
            "Mornye".to_string()
        } else {
            format!("Mornye {version}")
        }
    ))
}

/// The downloads card's lines: what Octo's checks of downloads by their sound cannot do right
/// now, in the order the card shows them, each with its `id` and whether it is a `warning` (AcoustID
/// switched off is a choice, so it only informs). The line for imported downloads going unchecked
/// names the setting its switch turns on (`turnOn`), the same one the Verify downloads card's
/// switch saves. Empty when there is nothing to say.
fn download_lines(state: &AppState) -> Vec<serde_json::Value> {
    let holds = state.soulseek_holds.snapshot();
    let waits_for_acoust_id = holds
        .iter()
        .any(|record| record.waiting_for == Some(WaitingFor::AcoustId));
    let lines = [
        (
            !state.settings.current().soulseek.verify_downloads,
            "verificationOff",
            VERIFICATION_OFF,
            true,
        ),
        (!state.acoust_id.is_on(), "acoustIdOff", ACOUST_ID_OFF, false),
        (waits_for_acoust_id, "acoustIdHolds", ACOUST_ID_HOLDS, true),
        (
            state.acoust_id.submission_key_refused(),
            "submissionKeyRefused",
            SUBMISSION_KEY_REFUSED,
            true,
        ),
    ];
    lines
        .into_iter()
        .filter(|(shown, ..)| *shown)
        .map(|(_, id, text, warning)| {
            let mut line = json!({ "id": id, "text": text, "warning": warning });
            if id == "verificationOff" {
                line["turnOn"] = json!(VERIFY_DOWNLOADS_SETTING);
            }
            line
        })
        .collect()
}

/// Every service's probe, by name. They run in parallel: the total is the slowest, not the sum.
async fn probe_all(state: &AppState) -> serde_json::Map<String, serde_json::Value> {
    let (navidrome, slskd, lidarr, shim, sonic, lastfm) = tokio::join!(
        probe_navidrome(state),
        probe_slskd(state),
        probe_lidarr(state),
        probe_you_tube_shim(state),
        probe_sonic(state),
        probe_last_fm(state),
    );
    [
        ("navidrome", navidrome),
        ("slskd", slskd),
        ("lidarr", lidarr),
        ("ytDlpShim", shim),
        ("sonic", sonic),
        ("lastfm", lastfm),
    ]
    .into_iter()
    .map(|(name, probe)| (name.to_string(), json!(probe)))
    .collect()
}

/// The signed-out status answer, kept ten seconds (`AnonymousStatusFor`), with when it was made.
pub struct AnonymousStatusCache {
    kept: tokio::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>>,
}

impl Default for AnonymousStatusCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AnonymousStatusCache {
    pub const KEPT_FOR: Duration = Duration::from_secs(10);

    pub fn new() -> Self {
        AnonymousStatusCache {
            kept: tokio::sync::Mutex::new(None),
        }
    }
}

/// Before a sign-in, status says only whether each service is up: enough for install.sh and for
/// the app to tell this is Octo, and nothing about addresses or errors. Kept ten seconds, because
/// the Last.fm probe spends the API key and this answers anyone. One probe round at a time: a
/// caller arriving meanwhile waits for it and gets its answer.
async fn anonymous_status(state: &AppState) -> serde_json::Value {
    let mut kept = state.anonymous_status.kept.lock().await;
    if let Some((at, answer)) = kept.as_ref()
        && at.elapsed() < AnonymousStatusCache::KEPT_FOR
    {
        return answer.clone();
    }
    let probes = probe_all(state).await;
    // configured too, so an optional service nobody set up reads as off, not as working.
    let services: serde_json::Map<String, serde_json::Value> = probes
        .into_iter()
        .map(|(name, probe)| {
            let ok = probe.get("ok").cloned().unwrap_or(serde_json::Value::Bool(false));
            let configured = probe
                .get("configured")
                .cloned()
                .unwrap_or(serde_json::Value::Bool(true));
            (name, json!({ "ok": ok, "configured": configured }))
        })
        .collect();
    let answer = json!({
        "octo": { "ok": true },
        "services": services,
        "time": round_trip_offset(Utc::now()),
        "signedIn": false,
    });
    *kept = Some((std::time::Instant::now(), answer.clone()));
    answer
}

/// `GET /api/admin/lidarr/options`: the choices the connected Lidarr offers for add-album
/// defaults.
pub async fn get_lidarr_options(State(state): State<AppState>) -> Response {
    match state.lidarr_client.get_options().await {
        Ok(options) => json_ok(&options),
        Err(e) => object_result(StatusCode::BAD_REQUEST, &json!({ "error": e.to_string() })),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LidarrConnectionTestRequest {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

impl FromBody for LidarrConnectionTestRequest {
    const SHAPE: &'static Shape = text_record!(
        "Octo.Controllers.AdminController+LidarrConnectionTestRequest",
        "baseUrl",
        "apiKey"
    );
}

/// `POST /api/admin/lidarr/test`: tests entered credentials without saving them.
pub async fn test_lidarr_connection(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: LidarrConnectionTestRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    let current = state.settings.current();
    let key = match lidarr_test_key(
        request.api_key.as_deref(),
        request.base_url.as_deref(),
        current.lidarr.api_key.as_deref(),
        current.lidarr.base_url.as_deref(),
    ) {
        Ok(key) => key,
        Err(words) => {
            return object_result(StatusCode::BAD_REQUEST, &json!({ "ok": false, "error": words }));
        }
    };
    match state
        .lidarr_client
        .test_connection(request.base_url.as_deref().unwrap_or(""), key.unwrap_or(""))
        .await
    {
        Ok(options) => ok(&json!({
            "ok": true,
            "message": "Connected to Lidarr. Choices loaded.",
            "options": options,
        })),
        Err(e) => object_result(
            StatusCode::BAD_REQUEST,
            &json!({ "ok": false, "error": e.to_string() }),
        ),
    }
}

/// The key `lidarr/test` sends: the page only ever holds the placeholder for a saved key, and
/// testing it means the saved one, but only at the saved address. Sending the saved key to
/// whatever address the request named would let anyone who can reach the admin API have it
/// sent to a server of their own, so the saved key follows the saved address alone, as a save's
/// does (`secrets::secret_sent_elsewhere`, the same comparison), and a new address needs the
/// key typed. Anything other than the placeholder is the key as typed.
fn lidarr_test_key<'a>(
    typed_key: Option<&'a str>,
    typed_url: Option<&str>,
    saved_key: Option<&'a str>,
    saved_url: Option<&str>,
) -> Result<Option<&'a str>, &'static str> {
    if typed_key != Some(super::plumbing::SECRET_PLACEHOLDER) {
        return Ok(typed_key);
    }
    let address = |url: Option<&str>| url.unwrap_or("").trim().trim_end_matches('/').to_string();
    if octo_core::common::dotnet::eq_ignore_case(&address(typed_url), &address(saved_url)) {
        Ok(saved_key)
    } else {
        Err("Retype the Lidarr API key for the new address.")
    }
}

/// `POST /api/admin/restart`: exits the process so docker compose's restart policy brings the
/// container back up with refreshed config. The caller gets its 202 before the shutdown fires.
pub async fn restart(State(state): State<AppState>, caller: Option<Extension<AdminCaller>>) -> Response {
    warn!(
        "{} restarted Octo from the dashboard; it exits in 1s",
        AdminCaller::describe(caller_of(&caller))
    );
    // Fire-and-forget so the response can be returned first.
    let inner = state.inner.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        inner.stop_application();
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Belt and braces: if the graceful stop has not finished in 2s, exit hard so
        // docker-compose treats it as a crash and restarts.
        std::process::exit(1);
    });
    object_result(
        StatusCode::ACCEPTED,
        &json!({ "ok": true, "message": "restarting" }),
    )
}

/// `POST /api/admin/soulseek/rejected-peers/clear`: forgets every peer and file that download
/// verification rejected. The recovery lever for a wrong denial: entries lapse on their own
/// after 30 days, but a user watching a track stop being fetchable should not have to wait a
/// month to find out whether this list is why.
pub async fn clear_rejected_peers(State(state): State<AppState>) -> Response {
    let cleared = state.rejected_peers.clear();
    info!("Rejected-peer memory cleared by admin request ({cleared} entries)");
    ok(&json!({ "cleared": cleared }))
}

/// `POST /api/admin/clear-metadata-cache`: drops every cached metadata answer and cover image.
/// Cached entries expire on their own, so this is a recovery lever rather than routine
/// maintenance: it turns "wait for the TTL" into "fixed now" when a run of throttled upstream
/// calls has left albums or covers looking wrong.
pub async fn clear_metadata_cache(State(state): State<AppState>) -> Response {
    state.deezer_metadata.clear_caches();
    state.cover_art_aggregator.clear_cache();
    info!("Metadata and cover-art caches cleared by admin request");
    ok(&json!({ "cleared": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn status_time_is_written_as_round_trip_with_an_offset() {
        let now = Utc.with_ymd_and_hms(2026, 10, 4, 8, 7, 57).unwrap()
            + chrono::TimeDelta::nanoseconds(635_173_300);
        assert_eq!(round_trip_offset(now), "2026-10-04T08:07:57.6351733+00:00");
    }
}

#[cfg(test)]
mod lidarr_test_key_tests {
    use super::lidarr_test_key;
    use crate::controllers::admin::plumbing::SECRET_PLACEHOLDER;

    /// The saved key follows the saved address only; a typed key goes anywhere it is sent.
    #[test]
    fn the_saved_key_follows_the_saved_address_only() {
        let saved = (Some("saved-key"), Some("http://lidarr:8686"));
        let test = |key, url| lidarr_test_key(key, url, saved.0, saved.1);
        assert_eq!(
            test(Some(SECRET_PLACEHOLDER), Some("http://lidarr:8686")),
            Ok(Some("saved-key"))
        );
        assert_eq!(
            test(Some(SECRET_PLACEHOLDER), Some(" HTTP://LIDARR:8686/ ")),
            Ok(Some("saved-key"))
        );
        assert_eq!(
            test(Some(SECRET_PLACEHOLDER), Some("http://evil.example:8686")),
            Err("Retype the Lidarr API key for the new address.")
        );
        assert_eq!(
            test(Some(SECRET_PLACEHOLDER), None),
            Err("Retype the Lidarr API key for the new address.")
        );
        assert_eq!(
            test(Some("typed"), Some("http://evil.example:8686")),
            Ok(Some("typed"))
        );
        assert_eq!(test(None, Some("http://lidarr:8686")), Ok(None));
    }
}
