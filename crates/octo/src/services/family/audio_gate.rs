//! The audio gate: every rule about audio bytes while Family is on.
//!
//! Every `/rest` request that hands out audio ([`serves_audio`]: `stream`, `download`, `hls`,
//! `getTranscodeStream`) and every radio stream passes [`admit`] (or [`admit_radio`]) before
//! it goes on. For people Octo manages it refuses audio away from home when that is off, an
//! explicit song for a clean-only profile, an offline copy without that ability, and a device
//! a newer one ended while the newer one still plays. For every caller, the owner included, it
//! works out the quality to ask Navidrome for: the client's own `format` and `maxBitRate`
//! first, then the account's quality for the place (when the device follows the account),
//! then the family limit. The lowest bitrate wins and nothing ever raises what the client asked
//! for. The answer is an [`AudioPlan`]: [`AudioPlan::apply_to`] rewrites the request and
//! [`AudioPlan::track`] hands the answer's body to the [`StreamTracker`].
//!
//! Octo's own file serving for outside songs asks [`ceiling`] too, and relays the library copy
//! with the ceiling instead of sending a file whenever one applies.
//!
//! With Family off the family gate never runs, so none of this does.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use indexmap::IndexMap;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::family::abilities::Role;
use octo_core::family::device::NetworkPlace;
use octo_core::family::policy::{ClientAsk, GateKind, Refusal, StreamRewrite, cap_for, stream_rewrite};
use octo_core::settings::SettingsStore;
use octo_subsonic::subsonic_request_parser::{
    Parameters, RequestParts, RequestReading, escape_data_string, json_object, read_request,
    unescape_data_string,
};
use parking_lot::Mutex;
use serde_json::Value;
use tracing::info;

use super::caller::FamilyCaller;
use super::family_store::FamilyStore;
use super::hooks;
use super::library_index::LibraryIndex;
use super::stream_tracker::{SongName, StreamPurpose, StreamStart, StreamTracker};
use crate::app::AppState;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::local::ILocalLibraryService;
use crate::services::prefetch::convert_rules::{GateClient, OwnAsk};
use crate::services::subsonic::SubsonicProxyService;

pub const NOT_AVAILABLE: &str = "Not available on this account.";
pub const DOWNLOADS_OFF: &str = "Downloads are off for this account.";
pub const SHARES_OFF: &str = "Share links are off for this account.";
pub const OFFLINE_OFF: &str = "Offline copies are off for this account.";
pub const NOT_ON_PROFILE: &str = "Not available on this profile.";
pub const PAUSED: &str = "This account is paused by the family owner.";
pub const AUDIO_BODY_REFUSED: &str =
    "Send the parameters of this request in the address, a url-encoded form or JSON.";
pub const ONE_SONG_AT_A_TIME: &str = "Ask for one song at a time.";
pub const LIBRARY_SONGS_ONLY: &str = "This profile can download single songs from the library only.";

/// How long what Octo learned about a song (its explicit mark, its bitrate) is kept.
const FACTS_FOR: Duration = Duration::from_secs(60 * 60);
/// How many songs' facts are kept.
const FACTS_KEPT: usize = 4096;
/// How many radio sessions' openers are kept.
const RADIO_KEPT: usize = 4096;
/// How often one refused unknown endpoint is logged.
const LOG_EVERY: Duration = Duration::from_secs(60 * 60);
/// How many log keys are kept at most; the names of unknown endpoints come from clients.
const LOG_KEYS: usize = 1024;
/// The bitrate assumed for a song outside the library (the source sends lossy audio).
const OUTSIDE_KBPS: u32 = 160;
/// The bitrate assumed for a library song whose own is not known.
const UNKNOWN_KBPS: u32 = 1411;

/// What Octo knows of a song's explicit mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplicitMark {
    Explicit,
    Clean,
    /// No mark either way.
    Unrated,
}

impl ExplicitMark {
    /// OpenSubsonic's `explicitStatus` (`explicit`, `clean`, or anything else for none).
    pub fn from_status(status: &str) -> Self {
        match status.trim().to_ascii_lowercase().as_str() {
            "explicit" => ExplicitMark::Explicit,
            "clean" => ExplicitMark::Clean,
            _ => ExplicitMark::Unrated,
        }
    }

    /// A catalog's `explicit_content_lyrics`: 1 explicit, 0 and 3 clean, anything else none.
    pub fn from_catalog(value: Option<i32>) -> Self {
        match value {
            Some(1) => ExplicitMark::Explicit,
            Some(0 | 3) => ExplicitMark::Clean,
            _ => ExplicitMark::Unrated,
        }
    }

    /// Whether a clean-only profile is kept from it. A song with no mark is kept from them only
    /// when `unrated_is_explicit`.
    pub fn blocks_clean_only(self, unrated_is_explicit: bool) -> bool {
        match self {
            ExplicitMark::Explicit => true,
            ExplicitMark::Clean => false,
            ExplicitMark::Unrated => unrated_is_explicit,
        }
    }
}

/// What a `getSong` told Octo about a song.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SongFacts {
    explicit: ExplicitMark,
    bit_rate: Option<u32>,
}

/// Where a radio session was opened from.
#[derive(Debug, Clone)]
struct RadioSession {
    device_id: Option<String>,
    /// Where its stream was last asked for from.
    place: Option<NetworkPlace>,
}

/// The audio gate's memory: the stream tracker, songs' facts, radio sessions' openers.
pub struct AudioGate {
    settings: Arc<SettingsStore>,
    store: Arc<FamilyStore>,
    index: Arc<LibraryIndex>,
    tracker: Arc<StreamTracker>,
    facts: Mutex<IndexMap<String, (SongFacts, Instant)>>,
    radio: Mutex<IndexMap<String, RadioSession>>,
    logged: Mutex<HashMap<String, Instant>>,
}

impl AudioGate {
    pub fn new(settings: Arc<SettingsStore>, store: Arc<FamilyStore>, index: Arc<LibraryIndex>) -> Self {
        AudioGate {
            settings,
            store,
            index,
            tracker: Arc::new(StreamTracker::new()),
            facts: Mutex::new(IndexMap::new()),
            radio: Mutex::new(IndexMap::new()),
            logged: Mutex::new(HashMap::new()),
        }
    }

    /// The live streams.
    pub fn tracker(&self) -> &Arc<StreamTracker> {
        &self.tracker
    }

    /// Whether Family is on.
    pub fn active(&self) -> bool {
        self.settings.current().family.enabled
    }

    /// True once an hour per `key`: whether to log it now.
    pub fn may_log(&self, key: &str) -> bool {
        let mut logged = self.logged.lock();
        let now = Instant::now();
        logged.retain(|_, at| now.duration_since(*at) < LOG_EVERY);
        if logged.contains_key(key) {
            return false;
        }
        crate::services::admin::sign_in_throttle::trim_oldest(&mut logged, LOG_KEYS - 1, |at| Some(*at));
        logged.insert(key.to_string(), now);
        true
    }

    /// How many log keys are kept now.
    pub fn logged_len(&self) -> usize {
        self.logged.lock().len()
    }

    /// The radio streams Octo plays at once: 8, or two (by default) per person in the family
    /// counting the owner when that is more and Family is on.
    pub fn radio_cap(&self) -> usize {
        let settings = self.settings.current();
        radio_stream_cap(
            settings.family.enabled,
            settings.family.radio_streams_per_person,
            self.store.members().len(),
        )
    }

    /// Notes the radio stream tokens in an answer that listed stations, as opened by `caller`'s
    /// device.
    pub fn note_radio_sessions(&self, answer: &[u8], caller: &FamilyCaller) {
        let tokens = radio_tokens(&String::from_utf8_lossy(answer));
        if tokens.is_empty() {
            return;
        }
        let mut radio = self.radio.lock();
        for token in tokens {
            radio.shift_remove(&token);
            radio.insert(
                token,
                RadioSession {
                    device_id: caller.device_id.clone(),
                    place: None,
                },
            );
        }
        while radio.len() > RADIO_KEPT {
            radio.shift_remove_index(0);
        }
    }

    /// The device that opened a radio session, if Octo saw it.
    pub fn radio_opener(&self, token: &str) -> Option<String> {
        self.radio.lock().get(token).and_then(|s| s.device_id.clone())
    }

    fn note_radio_place(&self, token: &str, place: NetworkPlace) {
        let mut radio = self.radio.lock();
        match radio.get_mut(token) {
            Some(session) => session.place = Some(place),
            None => {
                radio.insert(
                    token.to_string(),
                    RadioSession {
                        device_id: None,
                        place: Some(place),
                    },
                );
                while radio.len() > RADIO_KEPT {
                    radio.shift_remove_index(0);
                }
            }
        }
    }

    /// Why a radio stream of `user`'s may not play now, if it may not: a paused member, or a
    /// member away from home (where the stream was last asked for from) with away listening
    /// off. None while Family is off and for everyone else.
    pub fn radio_refusal(&self, user: &str, token: &str) -> Option<&'static str> {
        if !self.active() {
            return None;
        }
        let member = self.store.member(user.trim())?;
        if member.suspended {
            return Some(PAUSED);
        }
        let place = self.radio.lock().get(token).and_then(|s| s.place);
        if place == Some(NetworkPlace::Away) && !member.abilities().away {
            return Some(Refusal::Away.message());
        }
        None
    }

    /// Whether a member's clean-only profile keeps `song_id` out of their radio. False for
    /// everyone else and while Family is off.
    pub async fn radio_keeps_out(
        &self,
        user: &str,
        song_id: &str,
        authentication: &IndexMap<String, String>,
        proxy: &SubsonicProxyService,
        metadata: &dyn IMusicMetadataService,
        library: &dyn ILocalLibraryService,
    ) -> bool {
        if !self.active() || song_id.trim().is_empty() {
            return false;
        }
        let Some(member) = self.store.member(user.trim()) else {
            return false;
        };
        if !member.abilities().clean_only {
            return false;
        }
        let mark = self
            .explicit_mark(song_id, authentication, proxy, metadata, library)
            .await;
        mark.blocks_clean_only(self.settings.current().family.kid_unrated_is_explicit)
    }

    fn cached_facts(&self, song_id: &str) -> Option<SongFacts> {
        let facts = self.facts.lock();
        let (found, at) = facts.get(song_id)?;
        (at.elapsed() < FACTS_FOR).then_some(*found)
    }

    fn keep_facts(&self, song_id: &str, found: SongFacts) {
        let mut facts = self.facts.lock();
        facts.shift_remove(song_id);
        facts.insert(song_id.to_string(), (found, Instant::now()));
        while facts.len() > FACTS_KEPT {
            facts.shift_remove_index(0);
        }
    }

    /// A library song's facts from `getSong`, asked as the caller (kept an hour).
    async fn song_facts(
        &self,
        song_id: &str,
        sign_in: &IndexMap<String, String>,
        proxy: &SubsonicProxyService,
    ) -> Option<SongFacts> {
        if let Some(found) = self.cached_facts(song_id) {
            return Some(found);
        }
        let mut asking: Vec<(String, String)> = sign_in
            .iter()
            .filter(|(k, _)| {
                ["u", "p", "t", "s", "apiKey", "jwt", "v", "c"]
                    .iter()
                    .any(|key| eq_ignore_case(k, key))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        asking.push(("id".into(), song_id.to_string()));
        asking.push(("f".into(), "json".into()));
        let answer = proxy
            .relay("rest/getSong", asking.iter().map(|(k, v)| (k, v)))
            .await
            .ok()?;
        let value: Value = serde_json::from_slice(&answer.body).ok()?;
        let song = value.get("subsonic-response")?.get("song")?;
        let found = SongFacts {
            explicit: ExplicitMark::from_status(
                song.get("explicitStatus").and_then(Value::as_str).unwrap_or(""),
            ),
            bit_rate: song
                .get("bitRate")
                .and_then(Value::as_u64)
                .and_then(|rate| u32::try_from(rate).ok())
                .filter(|rate| *rate > 0),
        };
        self.keep_facts(song_id, found);
        Some(found)
    }

    /// The explicit mark of a song the library index knows; None for any other id.
    pub fn indexed_mark(&self, song_id: &str) -> Option<ExplicitMark> {
        if song_id.trim().is_empty() {
            return None;
        }
        self.index
            .song(song_id)
            .map(|indexed| ExplicitMark::from_status(&indexed.explicit_status))
    }

    /// A song's explicit mark: a library song's from the library index, else from `getSong` as
    /// the caller; an outside song's from its catalog.
    pub async fn explicit_mark(
        &self,
        song_id: &str,
        sign_in: &IndexMap<String, String>,
        proxy: &SubsonicProxyService,
        metadata: &dyn IMusicMetadataService,
        library: &dyn ILocalLibraryService,
    ) -> ExplicitMark {
        let (outside, provider, external_id) = library.parse_song_id(song_id);
        if outside {
            if let Some(found) = self.cached_facts(song_id) {
                return found.explicit;
            }
            let song = metadata
                .get_song(
                    provider.as_deref().unwrap_or_default(),
                    external_id.as_deref().unwrap_or_default(),
                )
                .await;
            let mark = ExplicitMark::from_catalog(song.and_then(|s| s.explicit_content_lyrics));
            self.keep_facts(
                song_id,
                SongFacts {
                    explicit: mark,
                    bit_rate: Some(OUTSIDE_KBPS),
                },
            );
            return mark;
        }
        if let Some(indexed) = self.index.song(song_id) {
            return ExplicitMark::from_status(&indexed.explicit_status);
        }
        self.song_facts(song_id, sign_in, proxy)
            .await
            .map_or(ExplicitMark::Unrated, |facts| facts.explicit)
    }

    /// The bitrate a song plays at, for pacing.
    async fn bit_rate(
        &self,
        song_id: &str,
        sign_in: &IndexMap<String, String>,
        proxy: &SubsonicProxyService,
        library: &dyn ILocalLibraryService,
    ) -> u32 {
        if song_id.trim().is_empty() {
            return UNKNOWN_KBPS;
        }
        if library.parse_song_id(song_id).0 {
            return OUTSIDE_KBPS;
        }
        self.song_facts(song_id, sign_in, proxy)
            .await
            .and_then(|facts| facts.bit_rate)
            .unwrap_or(UNKNOWN_KBPS)
    }

    /// A device's name as its person reads it.
    fn device_name(&self, device_id: Option<&str>) -> String {
        let Some(id) = device_id else {
            return "another device".to_string();
        };
        let device = self
            .store
            .device(id)
            .or_else(|| self.store.detected().into_iter().find(|d| d.id == id));
        match device {
            Some(device) if !device.name.trim().is_empty() => device.name,
            Some(device) if !device.app.trim().is_empty() => device.app,
            _ => "another device".to_string(),
        }
    }
}

/// The radio streams Octo plays at once: 8, or `per_person` for each member and the owner when
/// Family is on and that is more.
pub fn radio_stream_cap(family_on: bool, per_person: u32, members: usize) -> usize {
    if !family_on {
        return 8;
    }
    (per_person as usize).saturating_mul(members + 1).max(8)
}

/// The radio stream tokens (`/radio/stream/<48 characters>`) in a text.
fn radio_tokens(text: &str) -> Vec<String> {
    const MARK: &str = "/radio/stream/";
    let mut tokens = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(MARK) {
        rest = &rest[at + MARK.len()..];
        let token: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        if token.len() == 48 && !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    tokens
}

/// Whether a managed caller is held to the strict endpoint rules: a kid, or anyone with a
/// family limit on their quality. Never the owner or an account outside the family.
pub fn is_strict(caller: &FamilyCaller) -> bool {
    caller.managed
        && (caller.role == Role::Kid
            || caller.abilities.stream_cap.kbps().is_some()
            || caller.abilities.away_cap > 0)
}

/// The highest bitrate the caller's audio may arrive at now: the lower of the account's quality
/// for the place (when the device follows the account) and the family limit. None when neither
/// applies.
pub fn ceiling(caller: &FamilyCaller) -> Option<u32> {
    let account = caller.account_quality().and_then(|quality| quality.kbps());
    let family = if caller.managed {
        cap_for(&caller.abilities, caller.place)
    } else {
        None
    };
    [account, family].into_iter().flatten().min()
}

/// The endpoints whose answer is audio bytes.
pub fn serves_audio(endpoint: &str) -> bool {
    ["stream", "download", "hls", "getTranscodeStream"]
        .iter()
        .any(|name| eq_ignore_case(endpoint, name))
}

/// The endpoints that count as a device playing.
fn plays(endpoint: &str) -> bool {
    ["stream", "hls", "getTranscodeStream"]
        .iter()
        .any(|name| eq_ignore_case(endpoint, name))
}

fn parameter<'a>(parameters: &'a Parameters, name: &str) -> Option<&'a str> {
    parameters
        .iter()
        .find(|(key, _)| eq_ignore_case(key, name))
        .map(|(_, value)| value.as_str())
}

/// Parameters to set on a request, each replacing any value of the same name (any case).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParamRewrite {
    pub set: Vec<(String, String)>,
}

impl ParamRewrite {
    fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    fn removes(&self, key: &str) -> bool {
        self.set.iter().any(|(name, _)| eq_ignore_case(name, key))
    }

    /// A query (or url-encoded form) without the names this sets.
    fn strip(&self, query: &str) -> String {
        query
            .split('&')
            .filter(|segment| !segment.is_empty())
            .filter(|segment| {
                let name = segment.split_once('=').map_or(*segment, |(name, _)| name);
                !self.removes(&unescape_data_string(name))
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// The query with this rewrite's values set.
    pub fn query(&self, query: &str) -> String {
        let mut segments: Vec<String> = Vec::new();
        let kept = self.strip(query);
        if !kept.is_empty() {
            segments.push(kept);
        }
        for (name, value) in &self.set {
            segments.push(format!("{name}={}", escape_data_string(value)));
        }
        segments.join("&")
    }

    /// Sets this rewrite's values on parsed parameters.
    pub fn apply_to(&self, parameters: &mut Parameters) {
        parameters.retain(|key, _| !self.removes(key));
        for (name, value) in &self.set {
            parameters.insert(name.clone(), value.clone());
        }
    }
}

/// The parameter a stream's rate is asked in: `bitrate` on `download`, `maxBitRate` otherwise.
fn rate_key(endpoint: &str) -> &'static str {
    if eq_ignore_case(endpoint, "download") {
        "bitrate"
    } else {
        "maxBitRate"
    }
}

/// The gate's own word for which Octo app asks.
pub fn gate_kind(client: GateClient) -> GateKind {
    match client {
        GateClient::OctoDesktop => GateKind::Desktop,
        GateClient::OctoPhone => GateKind::Phone,
        GateClient::Other => GateKind::Other,
    }
}

/// The quality rewrite for one audio request, or Err when the caller is away from home with
/// away listening off. `kind` is the app asking; `offline` an offline copy.
pub fn quality_rewrite(
    caller: &FamilyCaller,
    endpoint: &str,
    parameters: &Parameters,
    kind: GateKind,
    offline: bool,
) -> Result<ParamRewrite, Refusal> {
    let rate_key = rate_key(endpoint);
    let client = ClientAsk {
        format: parameter(parameters, "format"),
        max_bit_rate: parameter(parameters, rate_key).and_then(|v| v.trim().parse().ok()),
        client: kind,
        offline,
    };
    let rewrite: StreamRewrite = stream_rewrite(
        &caller.abilities,
        caller.place,
        caller.quality,
        caller.device_quality,
        client,
    )?;
    let mut set = Vec::new();
    if let Some(format) = rewrite.format {
        set.push(("format".to_string(), format.to_string()));
    }
    if let Some(rate) = rewrite.max_bit_rate {
        set.push((rate_key.to_string(), rate.to_string()));
    }
    if rewrite.estimate {
        set.push(("estimateContentLength".to_string(), "true".to_string()));
    }
    Ok(ParamRewrite { set })
}

/// Sets the caller's quality on parameters Octo relays itself (a library copy it plays in place
/// of a file). Does nothing when no ceiling applies.
pub fn rewrite_parameters(
    caller: &FamilyCaller,
    endpoint: &str,
    parameters: &mut Parameters,
    client: GateClient,
    offline: bool,
) {
    if let Ok(rewrite) = quality_rewrite(caller, endpoint, parameters, gate_kind(client), offline) {
        rewrite.apply_to(parameters);
    }
}

/// What the client itself asked of a stream, before the gate set its quality: its own `format`
/// and rate, the ceiling the gate applied (none when it set no rate) and which app asks.
pub fn own_ask_of(
    endpoint: &str,
    parameters: &Parameters,
    rewrite: &ParamRewrite,
    client: GateClient,
) -> OwnAsk {
    let rate_key = rate_key(endpoint);
    let ceiling = rewrite
        .set
        .iter()
        .find(|(name, _)| name == rate_key)
        .and_then(|(_, value)| value.parse().ok());
    OwnAsk {
        format: parameter(parameters, "format")
            .map(|format| format.trim().to_string())
            .filter(|format| !format.is_empty()),
        rate: parameter(parameters, rate_key)
            .and_then(|value| value.trim().parse().ok())
            .filter(|rate: &u32| *rate > 0),
        ceiling,
        client,
    }
}

/// A Subsonic error in the format the request asked for (`jsonp` wrapped in its callback).
pub fn subsonic_refusal(state: &AppState, parameters: &Parameters, code: i32, message: &str) -> Response {
    let builder = &state.subsonic_response_builder;
    let f = parameter(parameters, "f").unwrap_or("xml");
    if f == "jsonp" {
        let reply = builder.create_error("json", code, message);
        let callback = parameter(parameters, "callback").filter(|c| {
            !c.is_empty()
                && c.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '$'))
        });
        let Some(callback) = callback else {
            return reply.into_response();
        };
        let mut response = Response::new(Body::from(format!("{callback}({});", reply.text())));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/javascript"),
        );
        return response;
    }
    builder
        .create_error(if f == "json" { "json" } else { "xml" }, code, message)
        .into_response()
}

/// A plain refusal for a stream that is not a Subsonic call (radio).
fn stream_refusal(message: &str) -> Response {
    let mut response = Response::new(Body::from(message.to_string()));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// An audio request as it was sent: its query and its body, so every value of a parameter can
/// be read wherever it was put.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sent<'a> {
    pub query: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
    /// The family gate's one reading of this query and body, which every value is read from.
    pub reading: Option<&'a RequestReading>,
}

impl Sent<'_> {
    fn media_type(&self) -> String {
        self.content_type
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    }

    /// Whether the body carries anything other than a url-encoded form or a JSON object, the
    /// two body kinds Octo rewrites: parameters in a multipart form or anything else could slip
    /// past the rewrite.
    pub fn has_other_body(&self) -> bool {
        if self.body.iter().all(u8::is_ascii_whitespace) {
            return false;
        }
        let media_type = self.media_type();
        media_type != "application/x-www-form-urlencoded" && !is_json_media_type(&media_type)
    }

    /// Every value of `name`, in any case, from the query and the body (a form of either kind,
    /// or a JSON object), each trimmed, blank ones left out.
    pub fn values(&self, name: &str) -> Vec<String> {
        match self.reading {
            Some(reading) => reading.every_value(name),
            None => read_request(&RequestParts {
                query: self.query,
                content_type: self.content_type,
                content_length: Some(self.body.len() as u64),
                body: self.body,
            })
            .every_value(name),
        }
    }
}

/// Whether a media type is JSON (`application/json`, or a `+json` type).
fn is_json_media_type(media_type: &str) -> bool {
    media_type == "application/json" || media_type.ends_with("+json")
}

/// A JSON object body without the members `rewrite` sets (any case), every other member kept
/// as written. The body is read as the parameters read it (a byte order mark, or bytes that are
/// not UTF-8, included), so no value the parameters would find stays behind. Ok(None) when it
/// has none of them; Err when it is not a JSON object at all.
fn json_without(rewrite: &ParamRewrite, body: &[u8]) -> Result<Option<Vec<u8>>, UnreadableBody> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let members = json_object(body).ok_or(UnreadableBody)?;
    if !members.keys().any(|key| rewrite.removes(key)) {
        return Ok(None);
    }
    let kept: IndexMap<String, Box<serde_json::value::RawValue>> = members
        .into_iter()
        .filter(|(key, _)| !rewrite.removes(key))
        .collect();
    serde_json::to_vec(&kept).map(Some).map_err(|_| UnreadableBody)
}

/// The parameter that names the song of an audio request, as Navidrome reads it.
fn song_key(endpoint: &str) -> &'static str {
    if eq_ignore_case(endpoint, "getTranscodeStream") {
        "mediaId"
    } else {
        "id"
    }
}

/// Every distinct song an audio request names, wherever and however often it names one.
pub fn songs_named(endpoint: &str, sent: &Sent<'_>) -> Vec<String> {
    let mut songs: Vec<String> = Vec::new();
    for value in sent.values(song_key(endpoint)) {
        if !songs.contains(&value) {
            songs.push(value);
        }
    }
    songs
}

/// Whether a stream marked `X-Octo-Purpose: offline` comes from one of the caller's own paired
/// Octo apps, the only devices that make offline copies: the caller's device is one whose app
/// Octo last saw as Octo.
fn from_octo_app(state: &AppState, caller: &FamilyCaller) -> bool {
    caller
        .device_id
        .as_deref()
        .and_then(|id| state.family_store.device(id))
        .is_some_and(|device| {
            eq_ignore_case(&device.username, &caller.user)
                && (device.app == "Octo" || device.app.starts_with("Octo "))
        })
}

/// A request's query and body after an [`AudioPlan`] set its quality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub query: Option<String>,
    /// The new body, when it changed.
    pub body: Option<Bytes>,
}

/// A body that carries parameters but cannot be read to rewrite them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnreadableBody;

/// What the gate does with an audio request that may go on.
#[derive(Debug, Clone, Default)]
pub struct AudioPlan {
    rewrite: ParamRewrite,
    start: Option<StreamStart>,
    /// What the client itself asked, for the handler (an audio request only).
    own: Option<OwnAsk>,
}

impl AudioPlan {
    /// The parameters the request gets.
    pub fn rewrite(&self) -> &ParamRewrite {
        &self.rewrite
    }

    /// What the client itself asked of the stream, before the gate set its quality.
    pub fn own_ask(&self) -> Option<&OwnAsk> {
        self.own.as_ref()
    }

    /// The query and body with this plan's quality set: in the query, and taken out of a
    /// url-encoded form or a JSON object body so the query's value is the one read. None when
    /// the plan sets nothing; Err when a JSON body cannot be read to take the values out.
    pub fn apply_to(
        &self,
        query: Option<&str>,
        content_type: Option<&str>,
        body: &Bytes,
    ) -> Result<Option<Applied>, UnreadableBody> {
        if self.rewrite.is_empty() {
            return Ok(None);
        }
        let query = Some(self.rewrite.query(query.unwrap_or("")));
        let media_type = content_type
            .map(|ct| ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
            .unwrap_or_default();
        let body = if media_type == "application/x-www-form-urlencoded" {
            let text = String::from_utf8_lossy(body);
            let stripped = self.rewrite.strip(&text);
            (stripped != text).then(|| Bytes::from(stripped.into_bytes()))
        } else if is_json_media_type(&media_type) {
            match json_without(&self.rewrite, body) {
                Ok(stripped) => stripped.map(Bytes::from),
                // A body the parameters read (a JSON content type) must be rewritten; one of a
                // JSON-like type they never read carries nothing to rewrite.
                Err(unreadable) if content_type.is_some_and(|ct| ct.contains("application/json")) => {
                    return Err(unreadable);
                }
                Err(_) => None,
            }
        } else {
            None
        };
        Ok(Some(Applied { query, body }))
    }

    /// The answer, its body tracked when it is audio.
    pub fn track(self, state: &AppState, response: Response) -> Response {
        let Some(start) = self.start else {
            return response;
        };
        if !response.status().is_success() {
            return response;
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        // A Subsonic error is not audio.
        if content_type.contains("xml")
            || content_type.contains("json")
            || content_type.contains("javascript")
        {
            return response;
        }
        name_outside_song(state, &start.song_id);
        let (parts, body) = response.into_parts();
        Response::from_parts(parts, state.family_audio.tracker().track(start, body))
    }
}

/// Starts looking up an outside song's title and artist for its live streams: what Octo holds in
/// memory first, else the metadata service, the way outside song entries are built. The lookup
/// runs on its own task and the stream never waits for it; a song Octo already knows, or one a
/// lookup has begun for, is not looked up again.
fn name_outside_song(state: &AppState, song_id: &str) {
    let (outside, provider, external_id) = state.local_library.parse_song_id(song_id);
    if !outside
        || hooks::known_names(
            &state.library_index,
            &state.external_id_registry,
            &state.acquisition_tracker,
            song_id,
        )
        .is_some()
    {
        return;
    }
    let tracker = Arc::clone(state.family_audio.tracker());
    if !tracker.start_naming(song_id) {
        return;
    }
    let metadata = Arc::clone(&state.metadata_service);
    let song_id = song_id.to_string();
    tokio::spawn(async move {
        let song = metadata
            .get_song(
                provider.as_deref().unwrap_or_default(),
                external_id.as_deref().unwrap_or_default(),
            )
            .await;
        let name = song
            .filter(|song| !song.title.trim().is_empty())
            .map(|song| SongName {
                title: song.title,
                artist: song.artist,
            });
        tracker.name_song(&song_id, name);
    });
}

/// Checks one `/rest` request from `caller` and answers its [`AudioPlan`]. A request that
/// hands out no audio goes on as it came.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
#[allow(clippy::too_many_arguments)]
pub async fn admit(
    state: &AppState,
    caller: &FamilyCaller,
    method: &Method,
    endpoint: &str,
    parameters: &Parameters,
    headers: &HeaderMap,
    sent: &Sent<'_>,
) -> Result<AudioPlan, Response> {
    if !serves_audio(endpoint) {
        return Ok(AudioPlan::default());
    }
    let gate = &state.family_audio;
    let refuse = |code: i32, message: &str| subsonic_refusal(state, parameters, code, message);
    // Only a member's own paired Octo app makes offline copies; from anything else the mark
    // is ignored and the audio counts as playing.
    let offline = headers
        .get("x-octo-purpose")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|purpose| purpose.trim().eq_ignore_ascii_case("offline"))
        && (!caller.managed || from_octo_app(state, caller));
    let songs = songs_named(endpoint, sent);
    if caller.managed {
        // The quality and the song are read from the query, a url-encoded form and a JSON
        // object, which Octo rewrites; parameters in a body of any other kind (a multipart
        // form) could slip past both.
        if sent.has_other_body() {
            return Err(refuse(0, AUDIO_BODY_REFUSED));
        }
        // Clients and Navidrome may each read a different one of several songs.
        if songs.len() > 1 {
            return Err(refuse(0, ONE_SONG_AT_A_TIME));
        }
    }
    let song_id = songs.into_iter().next().unwrap_or_default();
    let playing = plays(endpoint) && !offline;
    let abilities = &caller.abilities;
    if caller.managed {
        if offline && !abilities.offline_copies {
            return Err(refuse(50, OFFLINE_OFF));
        }
        if eq_ignore_case(endpoint, "download") && !abilities.download_files {
            return Err(refuse(50, DOWNLOADS_OFF));
        }
        if caller.place == NetworkPlace::Away && !abilities.away {
            return Err(refuse(50, Refusal::Away.message()));
        }
        if abilities.clean_only && eq_ignore_case(endpoint, "download") {
            // A download of an album, an artist or a playlist is a zip of every song in it, so
            // only one song the index knows, with a mark the profile may hear, goes through.
            let Some(mark) = gate.indexed_mark(&song_id) else {
                return Err(refuse(50, LIBRARY_SONGS_ONLY));
            };
            if mark.blocks_clean_only(state.settings.current().family.kid_unrated_is_explicit) {
                return Err(refuse(50, NOT_ON_PROFILE));
            }
        } else if abilities.clean_only && !song_id.trim().is_empty() {
            let mark = gate
                .explicit_mark(
                    &song_id,
                    parameters,
                    &state.subsonic_proxy,
                    state.music_metadata.as_ref(),
                    state.local_library.as_ref(),
                )
                .await;
            if mark.blocks_clean_only(state.settings.current().family.kid_unrated_is_explicit) {
                return Err(refuse(50, NOT_ON_PROFILE));
            }
        }
        if playing
            && abilities.devices_at_once > 0
            && let Some(name) = gate
                .tracker()
                .bumped_by(&caller.user, caller.device_id.as_deref())
        {
            return Err(refuse(50, &format!("Playing on {name} now.")));
        }
    }
    let client = GateClient::of(
        parameter(parameters, "c").unwrap_or(""),
        headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or(""),
    );
    let rewrite = quality_rewrite(caller, endpoint, parameters, gate_kind(client), offline)
        .map_err(|r| refuse(50, r.message()))?;
    let own = Some(own_ask_of(endpoint, parameters, &rewrite, client));
    if method == Method::HEAD {
        return Ok(AudioPlan {
            rewrite,
            start: None,
            own,
        });
    }
    let pace = if caller.managed && !abilities.download_files {
        let factor = u64::from(state.settings.current().family.fast_pull_factor);
        let kbps = match ceiling(caller) {
            Some(kbps) => kbps,
            None if factor > 0 => {
                gate.bit_rate(
                    &song_id,
                    parameters,
                    &state.subsonic_proxy,
                    state.local_library.as_ref(),
                )
                .await
            }
            None => 0,
        };
        Some(factor * u64::from(kbps) * 1000 / 8).filter(|rate| *rate > 0)
    } else {
        None
    };
    let purpose = if offline {
        StreamPurpose::Offline
    } else if playing {
        StreamPurpose::Play
    } else {
        StreamPurpose::Download
    };
    let start = StreamStart {
        user: caller.user.clone(),
        device_id: caller.device_id.clone(),
        device_name: gate.device_name(caller.device_id.as_deref()),
        song_id,
        place: caller.place,
        purpose,
        devices_at_once: if caller.managed {
            abilities.devices_at_once
        } else {
            0
        },
        pace_bytes_per_second: pace,
    };
    Ok(AudioPlan {
        rewrite,
        start: Some(start),
        own,
    })
}

/// Checks a radio stream and answers its [`AudioPlan`]. The stream is put on the device that
/// opened the radio session (or the person's unknown device slot).
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
pub fn admit_radio(
    state: &AppState,
    caller: &mut FamilyCaller,
    token: &str,
    station_id: &str,
    method: &Method,
) -> Result<AudioPlan, Response> {
    let gate = &state.family_audio;
    caller.device_id = gate.radio_opener(token);
    gate.note_radio_place(token, caller.place);
    if caller.managed {
        if let Some(message) = gate.radio_refusal(&caller.user, token) {
            return Err(stream_refusal(message));
        }
        if caller.abilities.devices_at_once > 0
            && let Some(name) = gate
                .tracker()
                .bumped_by(&caller.user, caller.device_id.as_deref())
        {
            return Err(stream_refusal(&format!("Playing on {name} now.")));
        }
    }
    if method == Method::HEAD {
        return Ok(AudioPlan::default());
    }
    let settings = state.settings.current();
    let pace = (caller.managed && !caller.abilities.download_files).then(|| {
        let kbps = u64::try_from(settings.last_fm.effective_radio_stream_bitrate_kbps()).unwrap_or(128);
        u64::from(settings.family.fast_pull_factor) * kbps * 1000 / 8
    });
    Ok(AudioPlan {
        rewrite: ParamRewrite::default(),
        own: None,
        start: Some(StreamStart {
            user: caller.user.clone(),
            device_id: caller.device_id.clone(),
            device_name: gate.device_name(caller.device_id.as_deref()),
            song_id: station_id.to_string(),
            place: caller.place,
            purpose: StreamPurpose::Radio,
            devices_at_once: if caller.managed {
                caller.abilities.devices_at_once
            } else {
                0
            },
            pace_bytes_per_second: pace.filter(|rate| *rate > 0),
        }),
    })
}

/// An OpenSubsonic extension list (`getOpenSubsonicExtensions`, JSON or XML) without the
/// extension `name`; None when the answer cannot be read.
pub fn without_extension(body: &[u8], json: bool, name: &str) -> Option<Vec<u8>> {
    if json {
        let mut value: Value = serde_json::from_slice(body).ok()?;
        let list = value
            .get_mut("subsonic-response")?
            .get_mut("openSubsonicExtensions")?
            .as_array_mut()?;
        list.retain(|entry| entry.get("name").and_then(Value::as_str) != Some(name));
        return Some(octo_core::json::to_string(&value).into_bytes());
    }
    let mut root = octo_subsonic::xml::XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    root.content.retain(|node| match node {
        octo_subsonic::xml::XNode::Element(element) => {
            !(element.name == "openSubsonicExtensions" && element.attribute("name") == Some(name))
        }
        _ => true,
    });
    Some(root.to_xml_string().into_bytes())
}

/// Logs a refused endpoint Octo does not know, once an hour per endpoint.
pub fn log_unknown(state: &AppState, caller: &FamilyCaller, endpoint: &str) {
    if state
        .family_audio
        .may_log(&format!("unknown|{}", endpoint.to_ascii_lowercase()))
    {
        info!(
            "FAMILY refused {endpoint} for {}: Octo does not know this endpoint",
            caller.user
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octo_core::family::abilities::{Abilities, Preset, StreamCap};
    use octo_core::family::quality::{DeviceQualityMode, QualityPreference, StreamQuality};

    #[test]
    fn the_log_of_unknown_endpoints_stays_bounded() {
        let settings = Arc::new(SettingsStore::for_tests(Default::default()));
        let store = Arc::new(FamilyStore::in_memory());
        let index = Arc::new(LibraryIndex::new(
            crate::services::subsonic::NavidromeIdentityService::new(
                settings.clone(),
                crate::services::http_client_factory::default_client(),
            ),
            crate::services::http_client_factory::default_client(),
            settings.clone(),
            Arc::new(tokio::sync::Notify::new()),
        ));
        let gate = AudioGate::new(settings, store, index);
        for n in 0..(LOG_KEYS * 3) {
            assert!(gate.may_log(&format!("unknown|getthing{n}")));
        }
        assert!(gate.logged_len() <= LOG_KEYS, "{}", gate.logged_len());
        assert!(!gate.may_log(&format!("unknown|getthing{}", LOG_KEYS * 3 - 1)));
    }

    #[test]
    fn every_value_of_a_parameter_is_read_wherever_it_was_put() {
        let sent = Sent {
            query: Some("id=a&ID=b"),
            content_type: Some("application/x-www-form-urlencoded"),
            body: b"iD=c&other=x",
            reading: None,
        };
        assert_eq!(sent.values("id"), ["a", "b", "c"]);
        assert!(!sent.has_other_body());
        let json = Sent {
            query: Some("id=a"),
            content_type: Some("application/json; charset=utf-8"),
            body: br#"{"Id":"d","format":"raw"}"#,
            reading: None,
        };
        assert_eq!(json.values("id"), ["a", "d"]);
        assert!(!json.has_other_body());
        let multipart = Sent {
            query: None,
            content_type: Some("multipart/form-data; boundary=XyZ"),
            body: b"--XyZ\r\nContent-Disposition: form-data; name=\"id\"\r\n\r\ne\r\n--XyZ--\r\n",
            reading: None,
        };
        assert_eq!(multipart.values("id"), ["e"]);
        assert!(multipart.has_other_body());
        assert!(
            !Sent {
                body: b"  ",
                ..multipart
            }
            .has_other_body()
        );
        assert_eq!(
            songs_named(
                "getTranscodeStream",
                &Sent {
                    query: Some("id=a&mediaId=m&MEDIAID=m"),
                    ..Default::default()
                }
            ),
            ["m"]
        );
        assert_eq!(
            songs_named(
                "stream",
                &Sent {
                    query: Some("id=a&mediaId=m"),
                    ..Default::default()
                }
            ),
            ["a"]
        );
    }

    #[test]
    fn a_plan_sets_its_values_in_the_query_and_takes_them_out_of_any_body_read() {
        assert_eq!(
            AudioPlan::default().apply_to(Some("id=1"), None, &Bytes::new()),
            Ok(None)
        );
        let plan = AudioPlan {
            rewrite: ParamRewrite {
                set: vec![("format".into(), "opus".into())],
            },
            start: None,
            own: None,
        };
        let form = plan
            .apply_to(
                Some("id=1&format=raw"),
                Some("application/x-www-form-urlencoded"),
                &Bytes::from_static(b"FORMAT=raw&x=1"),
            )
            .expect("read")
            .expect("applied");
        assert_eq!(form.query.as_deref(), Some("id=1&format=opus"));
        assert_eq!(form.body.as_deref(), Some(&b"x=1"[..]));
        let json = plan
            .apply_to(
                None,
                Some("application/json"),
                &Bytes::from_static("\u{feff}{\"format\":\"raw\",\"id\":\"1\"}".as_bytes()),
            )
            .expect("read")
            .expect("applied");
        assert_eq!(json.body.as_deref(), Some(&br#"{"id":"1"}"#[..]));
        assert_eq!(
            plan.apply_to(None, Some("application/json"), &Bytes::from_static(b"[1]")),
            Err(UnreadableBody)
        );
        // A JSON-like type the parameters never read has nothing to rewrite.
        let other = plan
            .apply_to(
                None,
                Some("application/problem+json"),
                &Bytes::from_static(b"[1]"),
            )
            .expect("read")
            .expect("applied");
        assert_eq!(other.body, None);
    }

    #[test]
    fn a_json_body_loses_only_the_rewritten_members() {
        let rewrite = ParamRewrite {
            set: vec![
                ("format".into(), "opus".into()),
                ("maxBitRate".into(), "192".into()),
            ],
        };
        let out = json_without(
            &rewrite,
            br#"{"FORMAT":"raw","id":"x","MaxBitRate":0,"n":[1, 2]}"#,
        )
        .expect("read")
        .expect("rewritten");
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"id":"x","n":[1, 2]}"#);
        assert_eq!(json_without(&rewrite, br#"{"id":"x"}"#), Ok(None));
        assert_eq!(json_without(&rewrite, b"not json"), Err(UnreadableBody));
        assert_eq!(json_without(&rewrite, b"  "), Ok(None));
        // A byte order mark, or bytes that are not UTF-8, are read as the parameters read them.
        let out = json_without(&rewrite, "\u{feff}{\"format\":\"raw\",\"id\":\"x\"}".as_bytes())
            .expect("read")
            .expect("rewritten");
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"id":"x"}"#);
        let out = json_without(&rewrite, b"{\"format\":\"raw\",\"x\":\"\xff\"}")
            .expect("read")
            .expect("rewritten");
        assert!(!String::from_utf8_lossy(&out).contains("raw"));
    }

    fn params(pairs: &[(&str, &str)]) -> Parameters {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn capped_listener() -> FamilyCaller {
        FamilyCaller::member(
            "alex",
            Preset::Listener,
            Abilities {
                stream_cap: StreamCap::Kbps(192),
                away_cap: 128,
                ..Abilities::preset(Preset::Listener)
            },
        )
    }

    #[test]
    fn strict_means_a_kid_or_a_family_limit() {
        assert!(!is_strict(&FamilyCaller::member(
            "alex",
            Preset::Listener,
            Abilities::preset(Preset::Listener)
        )));
        assert!(is_strict(&capped_listener()));
        assert!(is_strict(&FamilyCaller::member(
            "kim",
            Preset::Kid,
            Abilities::preset(Preset::Kid)
        )));
        let away_only = FamilyCaller::member(
            "alex",
            Preset::Member,
            Abilities {
                away_cap: 96,
                ..Abilities::preset(Preset::Member)
            },
        );
        assert!(is_strict(&away_only));
        assert!(!is_strict(&FamilyCaller::unmanaged("winters", true)));
    }

    #[test]
    fn ceilings_take_the_lowest_of_account_and_family() {
        let mut alex = capped_listener();
        assert_eq!(ceiling(&alex), Some(192));
        alex.place = NetworkPlace::Away;
        assert_eq!(ceiling(&alex), Some(128));
        alex.quality = QualityPreference {
            home: StreamQuality::Original,
            away: StreamQuality::DataSaver,
        };
        assert_eq!(ceiling(&alex), Some(96));
        alex.device_quality = DeviceQualityMode::App;
        assert_eq!(ceiling(&alex), Some(128));
        let mut owner = FamilyCaller::unmanaged("winters", true);
        assert_eq!(ceiling(&owner), None);
        owner.quality.home = StreamQuality::Standard;
        assert_eq!(ceiling(&owner), Some(160));
    }

    #[test]
    fn rewrites_replace_values_in_any_case() {
        let caller = capped_listener();
        let rewrite = quality_rewrite(
            &caller,
            "stream",
            &params(&[("id", "s1"), ("Format", "raw"), ("maxBitRate", "320")]),
            GateKind::Other,
            false,
        )
        .expect("allowed");
        assert_eq!(
            rewrite.set,
            vec![
                ("format".to_string(), "mp3".to_string()),
                ("maxBitRate".to_string(), "192".to_string())
            ]
        );
        assert_eq!(
            rewrite.query("u=alex&id=s1&Format=raw&maxBitRate=320&f=json"),
            "u=alex&id=s1&f=json&format=mp3&maxBitRate=192"
        );
        let download = quality_rewrite(
            &caller,
            "download",
            &params(&[("id", "s1")]),
            GateKind::Other,
            false,
        )
        .expect("allowed");
        assert_eq!(
            download.set,
            vec![
                ("format".to_string(), "mp3".to_string()),
                ("bitrate".to_string(), "192".to_string())
            ]
        );
        let mut relayed = params(&[("id", "s1"), ("format", "raw")]);
        rewrite_parameters(&caller, "stream", &mut relayed, GateClient::Other, false);
        assert_eq!(relayed.get("format").map(String::as_str), Some("mp3"));
        assert_eq!(relayed.get("maxBitRate").map(String::as_str), Some("192"));
        // Nothing to do for the owner with no preference.
        let owner = FamilyCaller::unmanaged("winters", true);
        assert!(
            quality_rewrite(
                &owner,
                "stream",
                &params(&[("format", "raw")]),
                GateKind::Other,
                false
            )
            .expect("allowed")
            .is_empty()
        );
    }

    #[test]
    fn each_octo_app_gets_its_own_capped_request() {
        let caller = capped_listener();
        let raw = params(&[("id", "s1"), ("format", "raw")]);
        let desktop = quality_rewrite(&caller, "stream", &raw, GateKind::Desktop, false).expect("allowed");
        assert_eq!(
            desktop.set,
            vec![
                ("format".to_string(), "opus".to_string()),
                ("maxBitRate".to_string(), "192".to_string())
            ]
        );
        let phone = quality_rewrite(&caller, "stream", &raw, GateKind::Phone, false).expect("allowed");
        assert_eq!(
            phone.set,
            vec![
                ("format".to_string(), "mp3".to_string()),
                ("maxBitRate".to_string(), "192".to_string()),
                ("estimateContentLength".to_string(), "true".to_string())
            ]
        );
        let offline = quality_rewrite(&caller, "stream", &raw, GateKind::Phone, true).expect("allowed");
        assert!(
            !offline
                .set
                .iter()
                .any(|(name, _)| name == "estimateContentLength")
        );
        assert_eq!(gate_kind(GateClient::OctoPhone), GateKind::Phone);
        assert_eq!(gate_kind(GateClient::OctoDesktop), GateKind::Desktop);
        assert_eq!(gate_kind(GateClient::Other), GateKind::Other);
    }

    #[test]
    fn the_clients_own_ask_is_read_before_the_gate_sets_its_quality() {
        let caller = capped_listener();
        let sent = params(&[("id", "s1"), ("format", "raw"), ("maxBitRate", "320")]);
        let rewrite = quality_rewrite(&caller, "stream", &sent, GateKind::Phone, false).expect("allowed");
        let own = own_ask_of("stream", &sent, &rewrite, GateClient::OctoPhone);
        assert_eq!(
            own,
            OwnAsk {
                format: Some("raw".into()),
                rate: Some(320),
                ceiling: Some(192),
                client: GateClient::OctoPhone,
            }
        );
        // A download names its rate `bitrate`; no ceiling leaves none.
        let owner = FamilyCaller::unmanaged("winters", true);
        let sent = params(&[("id", "s1"), ("format", "mp3"), ("bitrate", "96")]);
        let rewrite = quality_rewrite(&owner, "download", &sent, GateKind::Other, false).expect("allowed");
        let own = own_ask_of("download", &sent, &rewrite, GateClient::Other);
        assert_eq!(own.rate, Some(96));
        assert_eq!(own.ceiling, None);
        assert_eq!(own.format.as_deref(), Some("mp3"));
    }

    #[test]
    fn explicit_marks_read_both_ways() {
        assert_eq!(ExplicitMark::from_status("explicit"), ExplicitMark::Explicit);
        assert_eq!(ExplicitMark::from_status("Clean"), ExplicitMark::Clean);
        assert_eq!(ExplicitMark::from_status(""), ExplicitMark::Unrated);
        assert_eq!(ExplicitMark::from_catalog(Some(1)), ExplicitMark::Explicit);
        assert_eq!(ExplicitMark::from_catalog(Some(3)), ExplicitMark::Clean);
        assert_eq!(ExplicitMark::from_catalog(Some(6)), ExplicitMark::Unrated);
        assert!(ExplicitMark::Explicit.blocks_clean_only(false));
        assert!(!ExplicitMark::Unrated.blocks_clean_only(false));
        assert!(ExplicitMark::Unrated.blocks_clean_only(true));
        assert!(!ExplicitMark::Clean.blocks_clean_only(true));
    }

    #[test]
    fn radio_cap_scales_with_family() {
        assert_eq!(radio_stream_cap(false, 2, 20), 8);
        assert_eq!(radio_stream_cap(true, 2, 1), 8);
        assert_eq!(radio_stream_cap(true, 2, 5), 12);
        assert_eq!(radio_stream_cap(true, 3, 9), 30);
        assert_eq!(radio_stream_cap(true, 0, 9), 8);
    }

    #[test]
    fn radio_tokens_are_read_from_station_lists() {
        let token = "a".repeat(48);
        let other = "B1".repeat(24);
        let json = format!(
            r#"{{"streamUrl":"https://octo/radio/stream/{token}"}},{{"streamUrl":"http://h/radio/stream/{other}"}},{{"streamUrl":"/radio/stream/short"}}"#
        );
        assert_eq!(radio_tokens(&json), vec![token, other]);
    }

    #[test]
    fn transcoding_is_left_out_of_extension_lists() {
        let json = br#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"transcoding","versions":[1]},{"name":"songLyrics","versions":[1]}]}}"#;
        let out = without_extension(json, true, "transcoding").expect("read");
        let text = String::from_utf8(out).expect("text");
        assert!(
            !text.contains("transcoding") && text.contains("songLyrics"),
            "{text}"
        );
        let xml = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok"><openSubsonicExtensions name="transcoding"><versions>1</versions></openSubsonicExtensions><openSubsonicExtensions name="songLyrics"><versions>1</versions></openSubsonicExtensions></subsonic-response>"#;
        let out = without_extension(xml, false, "transcoding").expect("read");
        let text = String::from_utf8(out).expect("text");
        assert!(
            !text.contains("transcoding") && text.contains("songLyrics"),
            "{text}"
        );
    }
}
