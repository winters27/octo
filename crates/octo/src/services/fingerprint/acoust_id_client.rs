//! The AcoustID client, its HTTP half. The records and the reading of the answer are in
//! `octo_core::fingerprint::acoust_id_client`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::anyhow;
use octo_core::common::dotnet;
pub use octo_core::fingerprint::acoust_id_client::{
    AcoustIdAnswer, AcoustIdCredit, AcoustIdLookup, AcoustIdOutcome, AcoustIdRecording, AcoustIdRelease,
    AcoustIdResult, AcoustIdSubmission, AnswerError, META_FIELDS,
};
use octo_core::fingerprint::acoust_id_client::{
    build_lookup_form, build_submit_form, encode_form, error_message_or, parse_answer, parse_lookup,
};
use octo_core::json::element::{get_string, try_get_property};
use octo_core::settings::{AppSettings, SettingsStore, SoulseekSettings};
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde_json::Value;
use tracing::{debug, warn};

use super::acoust_id_rate_limit_handler::AcoustIdRateLimitHandler;
use super::acoust_id_rate_limiter::{
    AcoustIdRateLimiter, BusyKind, CallEnd, CallPermit, KeyLane, LookupClass,
};
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::parse_json;

/// The error codes AcoustID lists for one fingerprint or sample it cannot check: an unknown
/// format, missing or bad parameters, a bad fingerprint or length, a request too large.
const UNREADABLE_CODES: [i64; 9] = [1, 2, 3, 7, 8, 9, 10, 11, 19];

/// What one of AcoustID's error codes means for the call that got it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeMeans {
    /// Code 5 (an internal error) or 13: AcoustID is failing. Code 14: too many calls.
    Busy(BusyKind),
    /// Codes 4 (an invalid key), 12 (not allowed), 16 and 17 (an unknown application): the same for
    /// every call made with the key, so the key is set aside rather than any file refused.
    RejectedKey,
    /// Code 6 on a submission: the user's own submission key is invalid.
    RejectedSubmissionKey,
    /// Anything else: this one fingerprint could not be checked by its sound.
    Unreadable,
}

fn code_means(code: Option<i64>, submitting: bool) -> CodeMeans {
    match code {
        Some(5 | 13) => CodeMeans::Busy(BusyKind::Outage),
        Some(14) => CodeMeans::Busy(BusyKind::Overload),
        Some(4 | 12 | 16 | 17) => CodeMeans::RejectedKey,
        Some(6) if submitting => CodeMeans::RejectedSubmissionKey,
        _ => CodeMeans::Unreadable,
    }
}

/// What one call came to before its body is read as an answer.
enum Reply {
    /// No answer to read: a timeout, a network error, a 429, a 5xx, or a body that is not JSON.
    Busy(BusyKind),
    Body(Value),
}

impl Reply {
    fn of(answer: anyhow::Result<HttpAnswer>) -> Self {
        match answer {
            Err(_) => Self::Busy(BusyKind::Outage),
            Ok(answer) if answer.status == StatusCode::TOO_MANY_REQUESTS => Self::Busy(BusyKind::Overload),
            Ok(answer) if answer.status.is_server_error() => Self::Busy(BusyKind::Outage),
            Ok(answer) => match parse_json(&answer.body) {
                Ok(doc) => Self::Body(doc),
                Err(_) => Self::Busy(BusyKind::Outage),
            },
        }
    }
}

/// The error code of an answer whose status is not `ok`; None for an `ok` answer.
fn error_code(doc: &Value) -> Option<Option<i64>> {
    let ok = doc
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status.eq_ignore_ascii_case("ok"));
    (!ok).then(|| {
        doc.get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_i64)
    })
}

/// How a call that got a body ended for its lane: busy for codes 5, 13 and 14, else answered.
fn end_of(code: Option<Option<i64>>) -> CallEnd {
    match code.map(|code| code_means(code, false)) {
        Some(CodeMeans::Busy(kind)) => CallEnd::Busy(kind),
        _ => CallEnd::Answered,
    }
}

/// Keys AcoustID rejected, each set aside until the settings change.
#[derive(Default)]
struct SetAside {
    /// The settings snapshot they were set aside under; any newer one tries them all again.
    under: Option<Arc<AppSettings>>,
    user_key: bool,
    built_in: bool,
    submission_key: bool,
}

type OnListener = Arc<dyn Fn(bool) + Send + Sync>;

/// Which key each call uses, and whether AcoustID is on.
struct Keys {
    settings: Arc<SettingsStore>,
    built_in: Option<String>,
    set_aside: Mutex<SetAside>,
    listeners: Mutex<Vec<OnListener>>,
    last_on: Mutex<Option<bool>>,
    watching: AtomicBool,
}

impl Keys {
    /// The settings now, with every set-aside key tried again when they changed.
    fn current(&self) -> (Arc<AppSettings>, (bool, bool, bool)) {
        let settings = self.settings.current();
        let mut aside = self.set_aside.lock();
        if aside
            .under
            .as_ref()
            .is_some_and(|under| !Arc::ptr_eq(under, &settings))
        {
            *aside = SetAside::default();
        }
        let flags = (aside.user_key, aside.built_in, aside.submission_key);
        (settings, flags)
    }

    /// The key a call of `class` uses now, and its lane. One person's calls read
    /// [`SoulseekSettings::acoust_id_key_for_heart`] (the user's key, else Octo's own); bulk work
    /// reads [`SoulseekSettings::acoust_id_key_for_bulk`] (the user's key), and without one takes
    /// Octo's own key's lane at its lowest place. A key AcoustID rejected is passed over until the
    /// settings change. None when AcoustID is off or no key is left.
    fn key_for(&self, class: LookupClass) -> Option<(String, KeyLane)> {
        let (settings, (user_aside, built_in_aside, _)) = self.current();
        let soulseek = &settings.soulseek;
        if soulseek.acoust_id_off() {
            return None;
        }
        let wanted = match class {
            LookupClass::Bulk => soulseek.acoust_id_key_for_bulk(),
            _ => soulseek.key_for_heart_with(self.built_in.as_deref()),
        };
        let user = soulseek.acoust_id_user_key();
        if let Some(key) = wanted.filter(|key| !user_aside && user.as_ref() == Some(key)) {
            return Some((key, KeyLane::User));
        }
        self.built_in
            .clone()
            .filter(|_| !built_in_aside)
            .map(|key| (key, KeyLane::BuiltIn))
    }

    /// The lane a given key's calls take.
    fn lane_of(&self, key: &str) -> KeyLane {
        if self.built_in.as_deref() == Some(key.trim()) {
            KeyLane::BuiltIn
        } else {
            KeyLane::User
        }
    }

    fn is_on(&self) -> bool {
        self.key_for(LookupClass::Heart).is_some()
    }

    fn set_aside(&self, lane: KeyLane) {
        let settings = self.settings.current();
        let mut aside = self.set_aside.lock();
        aside.under = Some(settings);
        match lane {
            KeyLane::User => aside.user_key = true,
            KeyLane::BuiltIn => aside.built_in = true,
        }
    }

    fn set_aside_submission_key(&self) {
        let settings = self.settings.current();
        let mut aside = self.set_aside.lock();
        aside.under = Some(settings);
        aside.submission_key = true;
    }

    /// Tells every listener when AcoustID turned on or off since the last time anyone looked.
    fn note(&self) {
        let on = self.is_on();
        let changed = {
            let mut last = self.last_on.lock();
            let changed = last.is_some_and(|was| was != on);
            *last = Some(on);
            changed
        };
        if changed {
            let listeners: Vec<OnListener> = self.listeners.lock().clone();
            for listener in listeners {
                listener(on);
            }
        }
    }

    /// Hears every settings change, so the `off` setting saved, or a key set aside and then tried
    /// again, reaches the listeners without any call.
    fn watch(self: &Arc<Self>) {
        if self.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.watching.store(false, Ordering::SeqCst);
            return;
        };
        let mut changes = self.settings.subscribe();
        let keys: Weak<Keys> = Arc::downgrade(self);
        runtime.spawn(async move {
            while changes.changed().await.is_ok() {
                let Some(keys) = keys.upgrade() else {
                    return;
                };
                keys.note();
            }
        });
    }
}

pub struct AcoustIdClient {
    http: Arc<AcoustIdRateLimitHandler>,
    keys: Arc<Keys>,
}

impl AcoustIdClient {
    /// The client over Octo's own AcoustID key ([`SoulseekSettings::acoust_id_built_in_key`]) and
    /// the user's settings.
    pub fn new(http: Arc<AcoustIdRateLimitHandler>, settings: Arc<SettingsStore>) -> Self {
        Self::with_built_in_key(http, settings, SoulseekSettings::acoust_id_built_in_key())
    }

    /// The client with another built-in key, or none.
    pub fn with_built_in_key(
        http: Arc<AcoustIdRateLimitHandler>,
        settings: Arc<SettingsStore>,
        built_in: Option<&str>,
    ) -> Self {
        let built_in = built_in.map(str::trim).filter(|key| !key.is_empty());
        Self {
            http,
            keys: Arc::new(Keys {
                settings,
                built_in: built_in.map(str::to_string),
                set_aside: Mutex::new(SetAside::default()),
                listeners: Mutex::new(Vec::new()),
                last_on: Mutex::new(None),
                watching: AtomicBool::new(false),
            }),
        }
    }

    /// Whether some key is usable for a lookup: AcoustID is not `off`, and the user's key or
    /// Octo's own is set and not rejected. Needs no network.
    pub fn is_on(&self) -> bool {
        self.keys.is_on()
    }

    /// The key a call of `class` would use now; None when AcoustID is off for it.
    pub fn key_for(&self, class: LookupClass) -> Option<String> {
        self.keys.key_for(class).map(|(key, _)| key)
    }

    /// Hears every change between on and off: the `off` setting saved, the last key rejected, or a
    /// key usable again.
    pub fn subscribe_on(&self, listener: Arc<dyn Fn(bool) + Send + Sync>) {
        {
            let mut last = self.keys.last_on.lock();
            if last.is_none() {
                *last = Some(self.keys.is_on());
            }
        }
        self.keys.listeners.lock().push(listener);
        self.keys.watch();
    }

    /// Holds a lookup that met a busy AcoustID until its lane answers again: `wake` runs once,
    /// after an answered call on the lane a heart's lookup now takes, or at that lane's person gap
    /// while nothing else is sent on it.
    pub fn hold_lookup(&self, wake: Box<dyn FnOnce() + Send>) {
        let lane = self
            .keys
            .key_for(LookupClass::Heart)
            .map_or(KeyLane::BuiltIn, |(_, lane)| lane);
        self.http.limiter().hold(lane, wake);
    }

    /// Whether AcoustID refused the user's submission key, so submissions pause until the settings
    /// change.
    pub fn submission_key_refused(&self) -> bool {
        self.keys.current().1.2
    }

    fn timeout_seconds(&self) -> i32 {
        self.keys
            .settings
            .current()
            .soulseek
            .effective_acoust_id_timeout_seconds()
    }

    /// Per-call rather than the client's own Timeout, so Soulseek:AcoustIdTimeoutSeconds
    /// takes effect without a restart. The client keeps a generous ceiling behind this. The call
    /// already holds its turn, so the timer measures AcoustID alone.
    async fn post_within(
        &self,
        turn: &CallPermit,
        relative: &str,
        body: String,
        timeout_seconds: i32,
    ) -> anyhow::Result<HttpAnswer> {
        if timeout_seconds < 0 {
            return Err(anyhow!(
                "The value needs to be either -1 (signifying an infinite timeout), 0 or a positive integer. (Parameter 'delay')"
            ));
        }
        let timeout = Duration::from_secs(timeout_seconds as u64);
        match tokio::time::timeout(timeout, self.http.post_form(turn, relative, body)).await {
            Ok(answer) => answer,
            Err(_) => Err(anyhow!("A task was canceled.")),
        }
    }

    /// Asks AcoustID what this fingerprint is, for `class`'s work, and reads the whole answer by
    /// its error codes: Busy for a 5xx, a 429, codes 5, 13 and 14, a timeout, a network error or a
    /// body that cannot be read; a rejected key (codes 4, 12, 16 and 17) set aside and the next key
    /// tried, Off when none is left; any other code, or none, Unreadable, logged, and never sent
    /// again. The turn on the lane is taken before the call's own timer starts.
    pub async fn lookup_answer(
        &self,
        class: LookupClass,
        fingerprint: &str,
        duration_seconds: i32,
    ) -> AcoustIdOutcome {
        self.lookup_answer_on(class, fingerprint, duration_seconds, true)
            .await
            .unwrap_or(AcoustIdOutcome::Busy)
    }

    /// [`Self::lookup_answer`] when a turn is free at once; None at once otherwise, with nothing
    /// sent.
    pub async fn lookup_answer_if_free(
        &self,
        class: LookupClass,
        fingerprint: &str,
        duration_seconds: i32,
    ) -> Option<AcoustIdOutcome> {
        self.lookup_answer_on(class, fingerprint, duration_seconds, false)
            .await
    }

    async fn lookup_answer_on(
        &self,
        class: LookupClass,
        fingerprint: &str,
        duration_seconds: i32,
        wait: bool,
    ) -> Option<AcoustIdOutcome> {
        // The user's key, then Octo's own: each one AcoustID rejects is set aside.
        for _ in 0..2 {
            let Some((key, lane)) = self.keys.key_for(class) else {
                return Some(AcoustIdOutcome::Off);
            };
            let turn = self.http.limiter().permit(lane, class, wait).await;
            let Some(turn) = turn else {
                // A turn that is not free is never AcoustID's answer.
                return wait.then_some(AcoustIdOutcome::Busy);
            };
            let body = encode_form(&build_lookup_form(&key, fingerprint, duration_seconds));
            let reply = Reply::of(
                self.post_within(&turn, "v2/lookup", body, self.timeout_seconds())
                    .await,
            );
            let (end, outcome) = match reply {
                Reply::Busy(kind) => (CallEnd::Busy(kind), Some(AcoustIdOutcome::Busy)),
                Reply::Body(doc) => match parse_answer(&doc) {
                    Err(e) => {
                        warn!("acoustid answered a lookup Octo could not read: {e}");
                        (CallEnd::Busy(BusyKind::Outage), Some(AcoustIdOutcome::Busy))
                    }
                    Ok(Ok(answer)) => (CallEnd::Answered, Some(AcoustIdOutcome::Answered(answer))),
                    Ok(Err(error)) => match code_means(error.code, false) {
                        CodeMeans::Busy(kind) => (CallEnd::Busy(kind), Some(AcoustIdOutcome::Busy)),
                        CodeMeans::RejectedKey => (CallEnd::Answered, None),
                        CodeMeans::RejectedSubmissionKey | CodeMeans::Unreadable => {
                            log_unreadable(&error);
                            (
                                CallEnd::Answered,
                                Some(AcoustIdOutcome::Unreadable { code: error.code }),
                            )
                        }
                    },
                },
            };
            turn.end(end);
            if let Some(outcome) = outcome {
                self.keys.note();
                return Some(outcome);
            }
            warn!(
                "acoustid rejected {}; it is set aside until the settings change",
                key_name(lane)
            );
            self.keys.set_aside(lane);
            self.keys.note();
        }
        Some(AcoustIdOutcome::Off)
    }

    /// Ask AcoustID what this fingerprint is. Returns None on any transport or parse
    /// failure, which the caller reads as "no verdict" and keeps the file. The call takes the
    /// key's lane at the current work's class ([`AcoustIdRateLimiter::with_class`]); a key
    /// AcoustID rejects is set aside, so the next call takes the next key.
    pub async fn lookup(
        &self,
        api_key: &str,
        fingerprint: &str,
        duration_seconds: i32,
        timeout_seconds: i32,
    ) -> Option<AcoustIdLookup> {
        // POST rather than GET: a Chromaprint fingerprint is kilobytes of base64 and would
        // blow past URL length limits.
        let body = encode_form(&build_lookup_form(api_key, fingerprint, duration_seconds));
        let lane = self.keys.lane_of(api_key);
        let Some(turn) = self
            .http
            .limiter()
            .permit(lane, AcoustIdRateLimiter::class_now(), true)
            .await
        else {
            // The sweep yielding to a download, not AcoustID failing.
            if AcoustIdRateLimiter::in_background_now() {
                debug!("acoustid background lookup deferred");
            } else {
                warn!("acoustid rate limiter rejected a lookup");
            }
            return None;
        };
        let answer = self.post_within(&turn, "v2/lookup", body, timeout_seconds).await;
        let (end, lookup) = match answer {
            Err(e) => {
                warn!("acoustid lookup failed: {e}");
                (CallEnd::Busy(BusyKind::Outage), None)
            }
            Ok(answer) if !answer.is_success() => {
                warn!("acoustid lookup answered {}", answer.status.as_u16());
                (busy_status(answer.status), None)
            }
            Ok(answer) => {
                match read_lookup(&answer.body) {
                    // Warning, not Debug. A parse failure here reads downstream as "accept every
                    // file", so a silent degradation to a no-op is the worst outcome this feature
                    // can have and must be visible in the log.
                    Err(e) => {
                        warn!("acoustid lookup failed: {e}");
                        (CallEnd::Busy(BusyKind::Outage), None)
                    }
                    Ok((code, lookup)) => {
                        if code.is_some_and(|code| code_means(code, false) == CodeMeans::RejectedKey) {
                            warn!(
                                "acoustid rejected {}; it is set aside until the settings change",
                                key_name(lane)
                            );
                            self.keys.set_aside(lane);
                        }
                        (end_of(code), Some(lookup))
                    }
                }
            }
        };
        turn.end(end);
        self.keys.note();
        lookup
    }

    /// AcoustID's ids for the sound of one MusicBrainz recording: every fingerprint someone tied
    /// to it. Empty when nobody did; None on any transport or parse failure.
    pub async fn tracks_of_recording(
        &self,
        api_key: &str,
        recording_id: &str,
        timeout_seconds: i32,
    ) -> Option<Vec<String>> {
        if dotnet::is_blank(recording_id) {
            return None;
        }
        let body = encode_form(&[
            ("client".to_string(), api_key.to_string()),
            ("format".to_string(), "json".to_string()),
            ("mbid".to_string(), recording_id.trim().to_string()),
        ]);
        let lane = self.keys.lane_of(api_key);
        let Some(turn) = self
            .http
            .limiter()
            .permit(lane, AcoustIdRateLimiter::class_now(), true)
            .await
        else {
            debug!("acoustid could not list the sound of recording {recording_id}: no turn was free");
            return None;
        };
        let answer = self
            .post_within(&turn, "v2/track/list_by_mbid", body, timeout_seconds)
            .await;
        let (end, tracks) = match Reply::of(answer) {
            Reply::Busy(kind) => {
                debug!("acoustid could not list the sound of recording {recording_id}: busy");
                (CallEnd::Busy(kind), None)
            }
            Reply::Body(doc) => {
                let code = error_code(&doc);
                if code.is_some_and(|code| code_means(code, false) == CodeMeans::RejectedKey) {
                    self.keys.set_aside(lane);
                }
                let tracks = doc
                    .get("tracks")
                    .and_then(Value::as_array)
                    .filter(|_| code.is_none())
                    .map(|tracks| {
                        tracks
                            .iter()
                            .filter_map(|track| track.get("id").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    });
                (end_of(code), tracks)
            }
        };
        turn.end(end);
        tracks
    }

    /// Send confirmed fingerprints back to AcoustID (#47). Each carries a MusicBrainz recording a
    /// person vouched for by keeping the track; nothing is ever sent on Octo's own judgment.
    /// Posted through the same client as lookups, on the user's own key's lane. True when AcoustID
    /// accepted the batch. A user submission key AcoustID refused (code 6) pauses submissions
    /// until the settings change, with nothing sent meanwhile.
    pub async fn submit(
        &self,
        client_key: &str,
        user_key: &str,
        items: &[AcoustIdSubmission],
        timeout_seconds: i32,
    ) -> bool {
        if items.is_empty() {
            return true;
        }
        if self.submission_key_refused() {
            debug!("acoustid submissions wait for a new submission key");
            return false;
        }
        let body = encode_form(&build_submit_form(client_key, user_key, items));
        let lane = self.keys.lane_of(client_key);
        let Some(turn) = self.http.limiter().permit(lane, LookupClass::Bulk, true).await else {
            warn!("acoustid rate limiter rejected a submission");
            return false;
        };
        let attempt = async {
            let answer = self
                .post_within(&turn, "v2/submit", body, timeout_seconds)
                .await?;
            let doc = parse_json(&answer.body)?;
            let status = match try_get_property(&doc, "status")? {
                Some(s) => get_string(s)?,
                None => None,
            };
            if status.is_some_and(|s| dotnet::eq_ignore_case(s, "ok")) {
                return anyhow::Ok((None, true));
            }

            // "invalid user API key" arrives as a 200 with an error envelope, like lookup refusals.
            let message = error_message_or(&doc, status)?;
            warn!(
                "acoustid refused the submission: {}",
                message.as_deref().unwrap_or("(null)")
            );
            Ok((error_code(&doc), false))
        };
        let (end, accepted) = match attempt.await {
            Ok((code, accepted)) => {
                match code.map(|code| code_means(code, true)) {
                    Some(CodeMeans::RejectedSubmissionKey) => self.keys.set_aside_submission_key(),
                    Some(CodeMeans::RejectedKey) => self.keys.set_aside(lane),
                    _ => {}
                }
                (end_of(code), accepted)
            }
            Err(e) => {
                warn!("acoustid submission failed: {e}");
                (CallEnd::Busy(BusyKind::Outage), false)
            }
        };
        turn.end(end);
        accepted
    }
}

/// A lookup's body: its error code when its status is not `ok`, and the shipped reading of it.
fn read_lookup(body: &[u8]) -> anyhow::Result<(Option<Option<i64>>, AcoustIdLookup)> {
    let doc = parse_json(body)?;
    let lookup = parse_lookup(&doc)?;
    Ok((error_code(&doc), lookup))
}

/// A non-success status a lookup got: busy for a 429 or a 5xx, else an answer.
fn busy_status(status: StatusCode) -> CallEnd {
    if status == StatusCode::TOO_MANY_REQUESTS {
        CallEnd::Busy(BusyKind::Overload)
    } else if status.is_server_error() {
        CallEnd::Busy(BusyKind::Outage)
    } else {
        CallEnd::Answered
    }
}

fn key_name(lane: KeyLane) -> &'static str {
    match lane {
        KeyLane::User => "the AcoustID key in Settings",
        KeyLane::BuiltIn => "Octo's own AcoustID key",
    }
}

/// An error code that means this one fingerprint could not be checked; one AcoustID does not
/// list is logged with its code, so it can never hold a heart forever unnoticed.
fn log_unreadable(error: &AnswerError) {
    let message = error.message.as_deref().unwrap_or("");
    match error.code {
        Some(code) if UNREADABLE_CODES.contains(&code) => {
            debug!("acoustid could not check this fingerprint (code {code}): {message}")
        }
        Some(code) => {
            warn!("acoustid answered error code {code}, read as a fingerprint it cannot check: {message}")
        }
        None => {
            warn!("acoustid answered an error with no code, read as a fingerprint it cannot check: {message}")
        }
    }
}

#[cfg(test)]
mod tests {
    use octo_core::settings::AppSettings;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn client(server: &MockServer) -> AcoustIdClient {
        let base = url::Url::parse(&format!("{}/", server.uri())).expect("parses");
        let handler = AcoustIdRateLimitHandler::with_base_url(Arc::new(AcoustIdRateLimiter::new()), base);
        AcoustIdClient::new(
            Arc::new(handler),
            Arc::new(SettingsStore::for_tests(AppSettings::default())),
        )
    }

    /// The lookup posts the form AcoustID reads, with the meta fields space separated (a '+'
    /// on the wire), and reads the gzipped answer the `compress` field asks for.
    #[tokio::test]
    async fn lookup_posts_the_form_and_reads_a_gzipped_answer() {
        use std::io::Write;
        let server = MockServer::builder().start().await;
        let json = r#"{"status":"ok","results":[{"id":"x","score":0.9,"recordings":[{"id":"r1","title":"Song","artists":[{"name":"A"}]}]}]}"#;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(json.as_bytes()).expect("compresses");
        let compressed = gz.finish().expect("compresses");
        Mock::given(method("POST"))
            .and(path("/v2/lookup"))
            .and(body_string_contains(
                "meta=recordings+releasegroups+releases+tracks+compress+isrcs+sources",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(compressed),
            )
            .mount(&server)
            .await;

        let lookup = client(&server)
            .await
            .lookup("key", "AQAD", 200, 10)
            .await
            .expect("answered");

        assert!(lookup.is_ok);
        assert_eq!(lookup.results[0].recordings[0].title, "Song");
        let request = &server.received_requests().await.expect("recorded")[0];
        let body = String::from_utf8_lossy(&request.body);
        assert_eq!(
            body,
            "client=key&format=json&duration=200&fingerprint=AQAD&meta=recordings+releasegroups+releases+tracks+compress+isrcs+sources"
        );
        assert_eq!(
            request.headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/x-www-form-urlencoded")
        );
        assert!(
            request
                .headers
                .get("accept-encoding")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("gzip"))
        );
    }

    #[tokio::test]
    async fn a_refusal_or_an_unreadable_answer_is_no_verdict() {
        let server = MockServer::start().await;
        Mock::given(path("/v2/lookup"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/v2/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let client = client(&server).await;

        assert!(client.lookup("key", "AQAD", 200, 10).await.is_none());
        assert!(client.lookup("key", "AQAD", 200, 10).await.is_none());
    }

    #[tokio::test]
    async fn submit_reads_ok_from_the_body_not_the_status() {
        let server = MockServer::start().await;
        Mock::given(path("/v2/submit"))
            .and(body_string_contains("user=bad"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"status":"error","error":{"message":"invalid user API key"}}"#),
            )
            .mount(&server)
            .await;
        Mock::given(path("/v2/submit"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;
        let client = client(&server).await;
        let items = [AcoustIdSubmission {
            fingerprint: "AQAD".into(),
            duration_seconds: 200,
            recording_id: "mbid".into(),
            file_format: None,
        }];

        assert!(client.submit("key", "good", &items, 10).await);
        assert!(!client.submit("key", "bad", &items, 10).await);
        assert!(client.submit("key", "bad", &[], 10).await);
    }
}

#[cfg(test)]
#[path = "acoust_id_client_tests.rs"]
mod answer_tests;
