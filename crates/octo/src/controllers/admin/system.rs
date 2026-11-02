//! The process and status actions: the service health dots (`status`), the library path chain
//! (`library-status`), server discovery, Lidarr's choices and connection test, the restart, the
//! test notification, the fetched-songs log, the hearted downloads, and the two recovery levers
//! (clear rejected peers, clear the metadata caches).

use std::time::Duration;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use chrono::Utc;
use octo_core::common::dotnet::is_blank;
use octo_core::settings::HeartDownloadSource;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use super::json_body::bind_body;
use super::json_body::{FromBody, Shape, text_record};
use super::plumbing::{camel_case_value, object_result, ok};
use super::request_binding::query;
use crate::app::AppState;
use crate::http::error::json_ok;
use crate::middleware::admin_sign_in_gate::{Caller, caller_of};
use crate::services::admin::AdminCaller;
use crate::services::admin::directory_browser::is_writable;
use crate::services::http_client_factory::{connect_failure_message, timeout_message};
use crate::services::soulseek::soulseek_link::ISoulseekLink;
use crate::services::subsonic::subsonic_response_builder::acquisition_json;

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
/// says one was kept, and `downloads/log` reads it. With `q`, the kept entries matching every
/// word of it instead (artist, title, album or path), so an older download can be looked up.
pub async fn downloads(State(state): State<AppState>, uri: Uri) -> Response {
    let recent = match query(&uri, "q").filter(|q| !is_blank(q)) {
        Some(q) => state.download_history.search(&q, DOWNLOAD_SEARCH_LIMIT),
        None => state.download_history.get_recent(200),
    };
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

/// The most entries a search of the fetched-songs log answers with.
const DOWNLOAD_SEARCH_LIMIT: usize = 50;

/// `GET /api/admin/acquisitions`: every hearted download in flight or ended in the last half
/// hour, everyone's, newest first. The rows the app reads through getAcquisitions, plus the
/// provider key, and who asked only while Record who asked is on, the rule the fetched-songs
/// log follows too.
pub async fn acquisitions(State(state): State<AppState>) -> Response {
    let show_askers = state.settings.current().subsonic.record_requested_by;
    let rows: Vec<serde_json::Value> = state
        .acquisition_tracker
        .all()
        .iter()
        .map(|row| {
            let mut fields = acquisition_json(row);
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
/// up, which the dashboard shows as off rather than as a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ServiceProbe {
    ok: bool,
    detail: String,
    warning: bool,
    configured: bool,
}

impl ServiceProbe {
    fn new(ok: bool, detail: impl Into<String>) -> Self {
        ServiceProbe {
            ok,
            detail: detail.into(),
            warning: false,
            configured: true,
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
    let ok = state.lidarr_client.is_reachable().await;
    ServiceProbe::new(
        ok,
        if ok {
            "reachable"
        } else {
            "unreachable / API key invalid"
        },
    )
}

async fn probe_you_tube_shim(state: &AppState) -> ServiceProbe {
    let shim = state
        .settings
        .raw("YouTube:ShimUrl")
        .unwrap_or_else(|| "http://yt-dlp-shim:8080".to_string());
    let target = format!("{}/health", shim.trim_end_matches('/'));
    match probe_get(state, &target).await {
        Ok(status) => ServiceProbe::new(status.is_success(), format!("HTTP {}", status.as_u16())),
        Err(message) => ServiceProbe::new(false, message),
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
/// and Last.fm. Open without a sign-in, but then it answers only `ok` and `configured` per service
/// (see [`anonymous_status`]).
pub async fn get_status(State(state): State<AppState>, caller: Caller) -> Response {
    if !caller_of(&caller).is_some_and(AdminCaller::is_admin) {
        return json_ok(&anonymous_status(&state).await);
    }
    json_ok(&json!({
        "octo": ServiceProbe::new(true, "Octo is responding"),
        "services": probe_all(&state).await,
        "time": round_trip_offset(Utc::now()),
    }))
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
