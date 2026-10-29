//! The client for the yt-dlp shim.

use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, Shared};
use octo_core::settings::SettingsStore;
use octo_core::soulseek::{SoulseekRouting, StreamFacts};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use super::you_tube_music_client::{YouTubeMusicClient, YtmAnswer, YtmRow, pick_song};
use crate::services::framework::InFlight;

/// One video the shim found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct YouTubeHit {
    pub video_id: String,
    pub title: Option<String>,
    pub duration: Option<i32>,
    pub channel: Option<String>,
    /// The album YouTube Music files the row under. None for a video yt-dlp found.
    pub album: Option<String>,
    /// The audio stream the shim picked for this video, when the run that found it had one
    /// (/search does; /meta never picks a stream).
    pub stream: StreamFacts,
    /// The version of the ranking that chose this video (0 when the answer names none), at most
    /// [`SoulseekRouting::VIDEO_PICK`].
    pub pick: u8,
}

/// An open `/stream` response. The caller owns it and reads the body from `response`
/// (`bytes_stream()`), which stays open for the whole song.
#[derive(Debug)]
pub struct ShimStream {
    pub response: reqwest::Response,
    pub content_type: String,
    pub content_length: Option<u64>,
    pub status_code: u16,
    pub content_range: Option<String>,
}

/// What a lookup came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<T> {
    Found(T),
    /// It answered: there is none.
    None,
    /// It could not answer, and why.
    Failed(Failure),
}

impl<T> Lookup<T> {
    /// The found value, or None when there is none or the lookup could not answer.
    pub fn found(self) -> Option<T> {
        match self {
            Lookup::Found(value) => Some(value),
            _ => None,
        }
    }
}

/// A lookup that never finished (its task panicked) could not answer, so it is never read as "there is none".
impl<T> Default for Lookup<T> {
    fn default() -> Self {
        Lookup::Failed(Failure::Other)
    }
}

/// Whether a refusal's body is the shim's `{"error":"busy"}`.
fn is_busy_body(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .is_some_and(|root| root.get("error").and_then(Value::as_str) == Some("busy"))
}

/// A `/stream` the shim did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShimRefused {
    /// The shim said its lane is full (HTTP 503 with `{"error":"busy"}`).
    pub busy: bool,
}

/// What a fresh `/facts` answered for one video: its stream's facts and the channel that uploaded
/// it, as YouTube names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreshFacts {
    pub facts: StreamFacts,
    pub channel: Option<String>,
}

/// Pure HTTP client for the yt-dlp-shim sidecar service. Octo never spawns
/// yt-dlp itself: the shim wraps it behind /search and /stream endpoints
/// in its own container, so any process-management quirks stay isolated.
pub struct YouTubeResolver {
    /// `yt-dlp-shim-search`: 60 s rather than 30 s because back-to-back search3 prewarm
    /// bursts can fill the shim's yt-dlp gate (MAX_CONCURRENT_YTDLP, which ships as 5) and
    /// queue requests behind 5-8 s yt-dlp ytsearch1: invocations. 30 s was canceling the tail
    /// of every prewarm batch.
    search_client: reqwest::Client,
    /// `yt-dlp-shim-stream`: no timeout, because /stream stays open for the whole song and a
    /// client timeout would kill the read mid-track.
    stream_client: reqwest::Client,
    base_url: String,
    /// The last `/health` answer and when it was read (None inside when the shim did not
    /// answer). Held while a read is in flight, so callers at the same moment share one read.
    busy_cache: tokio::sync::Mutex<Option<(tokio::time::Instant, Option<ShimBusy>)>>,
    /// YouTube Music through the same shim; off while the shim's address is not set.
    music: Arc<YouTubeMusicClient>,
    /// YouTube Music searches running, by query.
    music_lookups: InFlight<Lookup<Vec<YtmRow>>>,
    /// YouTube Music searches sent to the shim at once: the shim's own YTM_MAX_CONCURRENT.
    music_gate: Arc<Semaphore>,
    /// One background YouTube Music lookup at a time, as the shim has one background slot;
    /// background lookups never take `music_gate`, so a play lookup never waits behind them.
    music_bg_gate: Arc<Semaphore>,
}

/// Why a lookup through the shim got no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The shim answered 503 `{"error":"busy"}`.
    Busy,
    /// The request never got an HTTP answer (connect error or refused).
    Unreachable,
    /// Anything else (a timeout, another status, a non-JSON answer).
    Other,
    /// A fresh resolve found the video removed (410 `video_unavailable`), or no longer served at
    /// the format asked for (404 `itag_unavailable`).
    Gone,
    /// A fresh resolve was refused for good for this video (403 `video_refused`): an age gate, a
    /// region block, a members-only video.
    Refused,
}

/// How busy the shim is, from its `/health`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShimBusy {
    /// `/stream` bodies being sent now.
    pub active_streams: u32,
    /// yt-dlp processes running now.
    pub ytdlp_running: u32,
    /// How many yt-dlp processes may run at once.
    pub ytdlp_limit: u32,
}

/// What a query escapes: everything but the RFC 3986 unreserved characters.
const DATA_STRING: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn escape(text: &str) -> String {
    utf8_percent_encode(text, DATA_STRING).to_string()
}

impl YouTubeResolver {
    /// The two clients' names.
    pub const SEARCH_CLIENT_NAME: &'static str = "yt-dlp-shim-search";
    pub const STREAM_CLIENT_NAME: &'static str = "yt-dlp-shim-stream";

    pub const DEFAULT_BASE_URL: &'static str = "http://yt-dlp-shim:8080";

    pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);

    /// How long a `/health` read may take before the shim counts as not answering.
    pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

    /// How long one `/health` answer is reused.
    pub const BUSY_CACHE: Duration = Duration::from_secs(5);
    /// How long a YouTube Music lookup may take before the yt-dlp lookup is asked instead.
    pub const MUSIC_TIMEOUT: Duration = Duration::from_secs(3);

    /// Rows one YouTube Music lookup asks for.
    pub const MUSIC_ROWS: i32 = 5;

    /// YouTube Music lookups sent to the shim at once.
    pub const MUSIC_AT_ONCE: usize = 3;

    /// Background YouTube Music lookups sent to the shim at once: its one background slot.
    pub const MUSIC_BACKGROUND_AT_ONCE: usize = 1;

    /// Reads `YouTube:ShimUrl` once: the address is deliberately captured at startup (it is a
    /// restart-only setting; see RestartTracker).
    pub fn new(settings: &SettingsStore) -> Self {
        Self::with_base_url(settings.raw("YouTube:ShimUrl").as_deref())
    }

    /// A resolver on the given shim address (blank → the compose service name).
    pub fn with_base_url(configured: Option<&str>) -> Self {
        // Neither client decompresses: /stream passes the shim's bytes and Content-Length
        // straight through.
        let search_client = reqwest::Client::builder()
            .timeout(Self::SEARCH_TIMEOUT)
            .no_gzip()
            .no_deflate()
            .build()
            .expect("a plain HTTP client builds");
        let stream_client = reqwest::Client::builder()
            .no_gzip()
            .no_deflate()
            .build()
            .expect("a plain HTTP client builds");
        let shim = configured.map(str::trim).filter(|c| !c.is_empty());
        YouTubeResolver {
            search_client,
            stream_client,
            base_url: Self::resolve_base_url(configured),
            busy_cache: tokio::sync::Mutex::new(None),
            music: Arc::new(YouTubeMusicClient::with_shim_url(shim)),
            music_lookups: InFlight::new(),
            music_gate: Arc::new(Semaphore::new(Self::MUSIC_AT_ONCE)),
            music_bg_gate: Arc::new(Semaphore::new(Self::MUSIC_BACKGROUND_AT_ONCE)),
        }
    }

    /// How busy the shim is now, from its `/health`; None when it does not answer within
    /// [`Self::HEALTH_TIMEOUT`] or answers something else. One answer, either way, is reused
    /// for [`Self::BUSY_CACHE`].
    pub async fn busy(&self) -> Option<ShimBusy> {
        let mut cache = self.busy_cache.lock().await;
        if let Some((at, busy)) = *cache
            && at.elapsed() < Self::BUSY_CACHE
        {
            return busy;
        }
        let busy = self.read_health().await;
        *cache = Some((tokio::time::Instant::now(), busy));
        busy
    }

    async fn read_health(&self) -> Option<ShimBusy> {
        let url = format!("{}/health", self.base_url);
        let outcome: anyhow::Result<ShimBusy> = async {
            let resp = self
                .search_client
                .get(&url)
                .timeout(Self::HEALTH_TIMEOUT)
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!("HTTP {}", resp.status().as_u16());
            }
            let root: Value = serde_json::from_str(&resp.text().await?)?;
            let count = |name: &str| {
                root.get(name)
                    .and_then(Value::as_u64)
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            };
            Ok(ShimBusy {
                active_streams: count("active_streams")
                    .ok_or_else(|| anyhow::anyhow!("no active_streams"))?,
                ytdlp_running: count("ytdlp_running").unwrap_or_default(),
                ytdlp_limit: count("ytdlp_limit").unwrap_or_default(),
            })
        }
        .await;
        outcome
            .inspect_err(|e| debug!("shim /health did not answer: {e}"))
            .ok()
    }

    /// The shim address every request is built on. Falls back to the compose
    /// service name when the setting is absent OR blank: the admin UI saves a
    /// cleared field as "", and "" is not null, so a plain null-coalesce left
    /// every request relative and the factory client with no base address.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn resolve_base_url(configured: Option<&str>) -> String {
        let value = match configured {
            Some(c) if !c.trim().is_empty() => c.trim(),
            _ => Self::DEFAULT_BASE_URL,
        };
        value.trim_end_matches('/').to_string()
    }

    fn query_url(&self, endpoint: &str, query: &str, duration_hint: Option<i32>, background: bool) -> String {
        let mut url = format!("{}/{endpoint}?q={}", self.base_url, escape(query));
        if let Some(dh) = duration_hint.filter(|&d| d > 0) {
            url.push_str(&format!("&duration={dh}"));
        }
        if background {
            url.push_str("&bg=1");
        }
        url
    }

    /// Resolves "Artist - Title" to a single best YouTube hit via the shim.
    ///
    /// `background` is true only for fire-and-forget prewarm. The shim keeps a slice of its
    /// yt-dlp gate unreachable to background work, so a user pressing play never queues behind
    /// a prewarm burst. Absence means interactive, so a call site that forgets this fails
    /// safe: slower prewarm, never a slower play.
    pub async fn search(
        &self,
        query: &str,
        duration_hint: Option<i32>,
        background: bool,
    ) -> Option<YouTubeHit> {
        self.search_skipping(query, duration_hint, background, &[]).await
    }

    /// [`Self::search`], leaving the videos in `skip` out of the shim's candidates.
    pub async fn search_skipping(
        &self,
        query: &str,
        duration_hint: Option<i32>,
        background: bool,
        skip: &[String],
    ) -> Option<YouTubeHit> {
        self.search_lookup(query, duration_hint, background, skip)
            .await
            .found()
    }

    /// [`Self::search_skipping`] that keeps "the shim has no hit" (HTTP 404, or an answer
    /// without a video) apart from "the shim could not answer" (any other failure; `busy` for
    /// HTTP 503 with `{"error":"busy"}`).
    pub async fn search_lookup(
        &self,
        query: &str,
        duration_hint: Option<i32>,
        background: bool,
        skip: &[String],
    ) -> Lookup<YouTubeHit> {
        if query.trim().is_empty() {
            return Lookup::None;
        }
        let mut url = self.query_url("search", query, duration_hint, background);
        if !skip.is_empty() {
            url.push_str(&format!("&skip={}", escape(&skip.join(","))));
        }
        let outcome: anyhow::Result<Lookup<YouTubeHit>> = async {
            let resp = self.search_client.get(&url).send().await?;
            let status = resp.status();
            if !status.is_success() {
                debug!("shim /search HTTP {} for '{query}'", status.as_u16());
                if status.as_u16() == 404 {
                    return Ok(Lookup::None);
                }
                let busy = status.as_u16() == 503 && is_busy_body(&resp.text().await.unwrap_or_default());
                return Ok(Lookup::Failed(if busy { Failure::Busy } else { Failure::Other }));
            }
            let root: Value = serde_json::from_str(&resp.text().await?)?;
            Ok(parse_hit(&root, true)?.map_or(Lookup::None, Lookup::Found))
        }
        .await;
        outcome.unwrap_or_else(|e| {
            warn!("shim /search failed for '{query}': {e}");
            let connect = e
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_connect);
            Lookup::Failed(if connect {
                Failure::Unreachable
            } else {
                Failure::Other
            })
        })
    }

    /// The video of `artist - title` on YouTube Music, with its length: the row of a songs search
    /// that is the same song ([`pick_song`]), `length` being the length known for the song. None
    /// when no row is, when YouTube Music cannot answer within [`Self::MUSIC_TIMEOUT`] (busy,
    /// failed or unreachable), or when the shim's address is not set; the caller then asks
    /// yt-dlp. The hit names no stream, since YouTube Music only searches.
    /// `background` is true for work nobody waits on: it goes in the shim's background lane.
    pub async fn music_song(
        &self,
        artist: &str,
        title: &str,
        length: Option<i32>,
        background: bool,
    ) -> Option<YouTubeHit> {
        self.music_song_lookup(artist, title, length, background)
            .await
            .found()
    }

    /// [`Self::music_song`] that keeps "no row is the song" apart from "YouTube Music could not
    /// answer" (`busy` when the shim said so). With no shim address set, or no names to search
    /// by, nothing is asked and the answer is [`Lookup::None`]. A `background` lookup goes
    /// through the shim's background slot and is never shared with a play's lookup.
    pub async fn music_song_lookup(
        &self,
        artist: &str,
        title: &str,
        length: Option<i32>,
        background: bool,
    ) -> Lookup<YouTubeHit> {
        let Some(rows) = self.music_rows(artist, title, background) else {
            return Lookup::None;
        };
        let rows = match rows.await {
            Lookup::Found(rows) => rows,
            Lookup::None => Vec::new(),
            Lookup::Failed(failure) => return Lookup::Failed(failure),
        };
        let query = format!("{artist} {title}");
        let Some(row) = pick_song(artist, title, length, &rows) else {
            debug!("no YouTube Music row is '{query}' (length {length:?})");
            return Lookup::None;
        };
        Lookup::Found(YouTubeHit {
            video_id: row.video_id.clone(),
            title: Some(row.title.clone()),
            duration: row.duration_seconds,
            channel: None,
            album: row.album.clone(),
            stream: StreamFacts::default(),
            pick: SoulseekRouting::VIDEO_PICK,
        })
    }

    /// [`Self::music_song`] as a background check, which tells "no row is the song" from "could
    /// not ask": it goes through the shim's background slot (`bg=1`), is not shared with other
    /// lookups, and a busy answer, a failure, an unreachable shim, an address that is not set or
    /// no answer within [`Self::MUSIC_TIMEOUT`] is [`Lookup::Failed`] with its [`Failure`].
    pub async fn music_song_checked(
        &self,
        artist: &str,
        title: &str,
        length: Option<i32>,
    ) -> Lookup<YouTubeHit> {
        if !self.music.configured() || (artist.trim().is_empty() && title.trim().is_empty()) {
            return Lookup::Failed(Failure::Other);
        }
        let query = format!("{artist} {title}");
        let lookup = async {
            let _permit = self.music_bg_gate.acquire().await;
            self.music
                .search_limited_background(&query, "songs", Self::MUSIC_ROWS)
                .await
        };
        let rows = match tokio::time::timeout(Self::MUSIC_TIMEOUT, lookup).await {
            Ok(Ok(rows)) => rows,
            Ok(Err(failure)) => {
                debug!("YouTube Music could not answer the background check for '{query}'");
                return Lookup::Failed(failure);
            }
            Err(_) => {
                debug!("YouTube Music timed out on the background check for '{query}'");
                return Lookup::Failed(Failure::Other);
            }
        };
        match pick_song(artist, title, length, &rows) {
            Some(row) => Lookup::Found(YouTubeHit {
                video_id: row.video_id.clone(),
                title: Some(row.title.clone()),
                duration: row.duration_seconds,
                pick: SoulseekRouting::VIDEO_PICK,
                ..Default::default()
            }),
            None => Lookup::None,
        }
    }

    /// The shim's ranked pick for `query` as a background check, leaving the videos in `skip`
    /// out: `/search` with `bg=1` and `resolve=0`, so it ranks only (no stream URL, no cache).
    /// A 404 (no candidate) is [`Lookup::None`]; any other failure is [`Lookup::Failed`]
    /// with its [`Failure`].
    pub async fn search_ranked(
        &self,
        query: &str,
        duration_hint: Option<i32>,
        skip: &[String],
    ) -> Lookup<YouTubeHit> {
        if query.trim().is_empty() {
            return Lookup::Failed(Failure::Other);
        }
        let mut url = self.query_url("search", query, duration_hint, true);
        url.push_str("&resolve=0");
        if !skip.is_empty() {
            url.push_str(&format!("&skip={}", escape(&skip.join(","))));
        }
        let resp = match self.search_client.get(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                debug!("shim ranked /search failed for '{query}': {e}");
                return Lookup::Failed(if e.is_connect() {
                    Failure::Unreachable
                } else {
                    Failure::Other
                });
            }
        };
        let status = resp.status();
        if status.as_u16() == 404 {
            return Lookup::None;
        }
        let body = resp.text().await.unwrap_or_default();
        let root: Option<Value> = serde_json::from_str(&body).ok();
        if status.as_u16() == 503
            && root.as_ref().and_then(|r| r.get("error")).and_then(Value::as_str) == Some("busy")
        {
            return Lookup::Failed(Failure::Busy);
        }
        if !status.is_success() {
            debug!("shim ranked /search HTTP {} for '{query}'", status.as_u16());
            return Lookup::Failed(Failure::Other);
        }
        let parsed = root
            .ok_or_else(|| anyhow::anyhow!("not JSON"))
            .and_then(|root| parse_hit(&root, true));
        match parsed {
            Ok(Some(hit)) => Lookup::Found(hit),
            Ok(None) => Lookup::None,
            Err(e) => {
                debug!("shim ranked /search failed for '{query}': {e}");
                Lookup::Failed(Failure::Other)
            }
        }
    }

    /// Starts YouTube Music's songs search for `artist - title` without waiting for it, so a
    /// [`Self::music_song`] for the song moments later joins it instead of starting its own.
    pub fn prefetch_music_song(&self, artist: &str, title: &str) {
        let _ = self.music_rows(artist, title, false);
    }

    /// YouTube Music's songs rows for `artist - title`, from one request shared by every caller
    /// asking while it runs. It waits at most [`Self::MUSIC_TIMEOUT`] for its turn
    /// ([`Self::MUSIC_AT_ONCE`]) and as long again for the answer. None (no future) when the shim's
    /// address is not set or there is nothing to search for; the future answers
    /// [`Lookup::Failed`] when YouTube Music could not answer, and [`Lookup::None`] when it
    /// answered with no rows. A `background` request takes the one background permit
    /// ([`Self::MUSIC_BACKGROUND_AT_ONCE`]) instead and asks the shim's background slot, and is
    /// kept apart from a play's request for the same song.
    fn music_rows(
        &self,
        artist: &str,
        title: &str,
        background: bool,
    ) -> Option<Shared<BoxFuture<'static, Lookup<Vec<YtmRow>>>>> {
        if !self.music.configured() || (artist.trim().is_empty() && title.trim().is_empty()) {
            return None;
        }
        let query = format!("{artist} {title}");
        let music = Arc::clone(&self.music);
        let gate = Arc::clone(if background {
            &self.music_bg_gate
        } else {
            &self.music_gate
        });
        let key = format!(
            "{}|{}",
            query.to_lowercase(),
            if background { "bg" } else { "play" }
        );
        Some(self.music_lookups.run(&key, async move {
            let failed = Lookup::Failed(Failure::Other);
            // As many at once as the shim runs, so the time cap counts the lookup itself and
            // not a wait in the shim's queue.
            let Ok(Ok(_permit)) = tokio::time::timeout(Self::MUSIC_TIMEOUT, gate.acquire_owned()).await
            else {
                return failed;
            };
            if background {
                let lookup = music.search_limited_background(&query, "songs", Self::MUSIC_ROWS);
                return match tokio::time::timeout(Self::MUSIC_TIMEOUT, lookup).await {
                    Ok(Ok(rows)) if rows.is_empty() => Lookup::None,
                    Ok(Ok(rows)) => Lookup::Found(rows),
                    Ok(Err(Failure::Busy)) => {
                        debug!("YouTube Music was busy for the background '{query}'");
                        Lookup::Failed(Failure::Busy)
                    }
                    Ok(Err(_)) | Err(_) => {
                        debug!("YouTube Music could not answer the background '{query}'");
                        failed
                    }
                };
            }
            let lookup = music.search_limited(&query, "songs", Self::MUSIC_ROWS);
            match tokio::time::timeout(Self::MUSIC_TIMEOUT, lookup).await {
                Ok(Ok(YtmAnswer::Rows(rows))) if rows.is_empty() => Lookup::None,
                Ok(Ok(YtmAnswer::Rows(rows))) => Lookup::Found(rows),
                Ok(Ok(YtmAnswer::Busy)) => {
                    debug!("YouTube Music was busy for '{query}'");
                    Lookup::Failed(Failure::Busy)
                }
                Ok(Ok(YtmAnswer::Failed)) => {
                    debug!("YouTube Music could not answer for '{query}'");
                    failed
                }
                Ok(Err(e)) => {
                    debug!("YouTube Music failed for '{query}': {e}");
                    failed
                }
                Err(_) => {
                    debug!(
                        "YouTube Music did not answer within {:?} for '{query}'",
                        Self::MUSIC_TIMEOUT
                    );
                    failed
                }
            }
        }))
    }

    /// Fast metadata-only lookup via the shim's /meta (flat search, no URL
    /// resolution). Returns the top video's id + duration for showing an accurate
    /// length without paying the full /search extraction.
    ///
    /// `background` means the same as on [`Self::search`]: true only for fire-and-forget
    /// prewarm, so the shim's gate never makes an interactive caller queue behind it.
    pub async fn meta(
        &self,
        query: &str,
        duration_hint: Option<i32>,
        background: bool,
    ) -> Option<YouTubeHit> {
        if query.trim().is_empty() {
            return None;
        }
        let url = self.query_url("meta", query, duration_hint, background);
        let outcome: anyhow::Result<Option<YouTubeHit>> = async {
            let resp = self.search_client.get(&url).send().await?;
            if !resp.status().is_success() {
                return Ok(None);
            }
            let root: Value = serde_json::from_str(&resp.text().await?)?;
            parse_hit(&root, false)
        }
        .await;
        outcome.ok().flatten()
    }

    /// What the shim's /stream would send for `video_id`: its format, container, bit rate and
    /// exact size. The shim resolves the video when it has not yet (as /stream would, so a play
    /// after this is a cache hit). None when the shim could not.
    pub async fn facts(&self, video_id: &str, background: bool) -> Option<StreamFacts> {
        if video_id.trim().is_empty() {
            return None;
        }
        let mut url = format!("{}/facts?id={}", self.base_url, escape(video_id));
        if background {
            url.push_str("&bg=1");
        }
        let outcome: anyhow::Result<Option<StreamFacts>> = async {
            let resp = self.search_client.get(&url).send().await?;
            if !resp.status().is_success() {
                debug!("shim /facts HTTP {} for {video_id}", resp.status().as_u16());
                return Ok(None);
            }
            let root: Value = serde_json::from_str(&resp.text().await?)?;
            Ok(Some(parse_stream(&root)).filter(|facts| !facts.is_empty()))
        }
        .await;
        outcome.unwrap_or_else(|e| {
            warn!("shim /facts failed for {video_id}: {e}");
            None
        })
    }

    /// Opens a streaming connection to the shim's /stream endpoint. The shim
    /// proxies bytes from YouTube's CDN to us; we hand the resulting stream
    /// off to the handler, which forwards it to the Subsonic client.
    ///
    /// `range_header` is forwarded verbatim to the shim, which passes it on to googlevideo.
    /// iOS Subsonic clients (Arpeggi, Narjo) probe with `Range: bytes=0-1` and won't play
    /// audio/mp4 unless the server returns 206 with a valid Content-Range, so this passthrough
    /// is required for them to even attempt playback.
    ///
    /// None on failure. The caller owns the returned response.
    pub async fn open_stream(&self, video_id: &str, range_header: Option<&str>) -> Option<ShimStream> {
        self.open_stream_with(video_id, range_header, false).await
    }

    /// [`Self::open_stream`] of the whole video for work nobody waits on: the shim resolves it in
    /// its background lane, so a play never queues behind it.
    pub async fn open_stream_in_background(&self, video_id: &str) -> Option<ShimStream> {
        self.open_stream_with(video_id, None, true).await
    }

    /// [`Self::open_stream_in_background`] that tells a full background lane (HTTP 503 with
    /// `{"error":"busy"}`) from any other refusal.
    pub async fn try_open_stream_in_background(&self, video_id: &str) -> Result<ShimStream, ShimRefused> {
        self.try_open_stream_with(video_id, None, true).await
    }

    /// [`Self::open_stream`] that tells a full background lane from any other refusal.
    pub async fn try_open_stream(&self, video_id: &str) -> Result<ShimStream, ShimRefused> {
        self.try_open_stream_with(video_id, None, false).await
    }

    async fn open_stream_with(
        &self,
        video_id: &str,
        range_header: Option<&str>,
        background: bool,
    ) -> Option<ShimStream> {
        self.try_open_stream_with(video_id, range_header, background)
            .await
            .ok()
    }

    async fn try_open_stream_with(
        &self,
        video_id: &str,
        range_header: Option<&str>,
        background: bool,
    ) -> Result<ShimStream, ShimRefused> {
        let refused = ShimRefused { busy: false };
        if video_id.trim().is_empty() {
            return Err(refused);
        }
        let mut url = format!("{}/stream?id={}", self.base_url, escape(video_id));
        if background {
            url.push_str("&bg=1");
        }
        let mut request = self.stream_client.get(&url);
        if let Some(range) = range_header.filter(|r| !r.is_empty()) {
            request = request.header(reqwest::header::RANGE, range);
        }
        let resp = match request.send().await {
            Ok(resp) => resp,
            Err(e) => {
                warn!("shim /stream failed for {video_id}: {e}");
                return Err(refused);
            }
        };
        // Accept 200 (full body), 206 (partial content) and 416 (a range past the end, whose
        // `bytes */<size>` is the client's answer). Anything else means the upstream/shim
        // couldn't satisfy the request.
        let status = resp.status().as_u16();
        if !matches!(status, 200 | 206 | 416) {
            warn!("shim /stream HTTP {status} for {video_id}");
            let busy = status == 503 && is_busy_body(&resp.text().await.unwrap_or_default());
            return Err(ShimRefused { busy });
        }
        let header = |name: reqwest::header::HeaderName| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_else(|| "audio/mp4".to_string());
        let content_range = header(reqwest::header::CONTENT_RANGE);
        let content_length = resp.content_length();
        Ok(ShimStream {
            response: resp,
            content_type,
            content_length,
            status_code: status,
            content_range,
        })
    }

    /// Downloads a YouTube video as MP3 to `{dest_without_ext}.mp3` via the shim's
    /// /download endpoint, passing clean artist/title so the file is tagged for
    /// the library. Returns the saved path, or None on failure. Uses the
    /// no-timeout stream client because a download can take a while.
    pub async fn download(
        &self,
        video_id: &str,
        dest_without_ext: &str,
        artist: Option<&str>,
        title: Option<&str>,
    ) -> Option<String> {
        if video_id.trim().is_empty() || dest_without_ext.trim().is_empty() {
            return None;
        }
        let mut url = format!(
            "{}/download?id={}&dest={}",
            self.base_url,
            escape(video_id),
            escape(dest_without_ext)
        );
        if let Some(artist) = artist.filter(|a| !a.is_empty()) {
            url.push_str(&format!("&artist={}", escape(artist)));
        }
        if let Some(title) = title.filter(|t| !t.is_empty()) {
            url.push_str(&format!("&title={}", escape(title)));
        }
        let outcome: anyhow::Result<Option<String>> = async {
            let resp = self.stream_client.get(&url).send().await?;
            if !resp.status().is_success() {
                warn!(
                    "shim /download HTTP {} for vid={video_id}",
                    resp.status().as_u16()
                );
                return Ok(None);
            }
            let root: Value = serde_json::from_str(&resp.text().await?)?;
            json_string(&root, "path")
        }
        .await;
        outcome.unwrap_or_else(|e| {
            warn!("shim /download failed for {video_id}: {e}");
            None
        })
    }
}

/// A string property: absent or null → None; a value that is not a string is an error, and so
/// is a root that is not an object, which the callers handle.
fn json_string(root: &Value, name: &str) -> anyhow::Result<Option<String>> {
    let object = root
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("the answer is not a JSON object"))?;
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => anyhow::bail!("'{name}' is not a string"),
    }
}

/// The hit in a /search or /meta answer; None without a video id. `with_channel` for /search,
/// whose hit names the channel.
fn parse_hit(root: &Value, with_channel: bool) -> anyhow::Result<Option<YouTubeHit>> {
    let Some(video_id) = json_string(root, "video_id")?.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let title = json_string(root, "title")?;
    // A number must fit an `i32`, or the answer is an error; anything else is no duration.
    let duration = match root.get("duration") {
        Some(Value::Number(n)) => Some(
            n.as_i64()
                .and_then(|d| i32::try_from(d).ok())
                .ok_or_else(|| anyhow::anyhow!("'duration' is not an Int32"))?,
        ),
        _ => None,
    };
    let channel = if with_channel {
        json_string(root, "channel")?
    } else {
        None
    };
    let pick = root
        .get("rank")
        .and_then(Value::as_u64)
        .map_or(0, |rank| rank.min(u64::from(SoulseekRouting::VIDEO_PICK)) as u8);
    Ok(Some(YouTubeHit {
        video_id,
        title,
        duration,
        channel,
        album: None,
        stream: parse_stream(root),
        pick,
    }))
}

/// The stream facts in a /search or /facts answer (`size`, `itag`, `ext`, `abr`, and, once a play
/// measured it, `track_gain` and `track_peak`). A field that is missing or not what it should be
/// is left unknown rather than failing the answer.
fn parse_stream(root: &Value) -> StreamFacts {
    let text = |name: &str| {
        root.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let number = |name: &str| root.get(name).and_then(Value::as_f64).filter(|n| n.is_finite());
    let track_gain = number("track_gain");
    StreamFacts {
        track_gain,
        // A peak means nothing without the gain it was measured with.
        track_peak: track_gain.and(number("track_peak")).filter(|peak| *peak > 0.0),
        size: root.get("size").and_then(Value::as_u64).filter(|size| *size > 0),
        // Reported, not played: only the bytes of a play confirm it.
        size_confirmed: false,
        itag: text("itag"),
        ext: text("ext"),
        bit_rate: root
            .get("abr")
            .and_then(Value::as_f64)
            .map(f64::round)
            .filter(|kbps| *kbps > 0.0 && *kbps < f64::from(i32::MAX))
            .map(|kbps| kbps as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ---- The shim's base URL ------------------------------------------------------------

    fn build(shim_url: Option<&str>) -> YouTubeResolver {
        let store = SettingsStore::for_tests(Default::default());
        if let Some(url) = shim_url {
            store.set_raw("YouTube:ShimUrl", Some(url));
        }
        YouTubeResolver::new(&store)
    }

    #[test]
    fn a_lookup_that_never_finished_reads_as_one_that_could_not_answer() {
        assert_eq!(Lookup::<i32>::default(), Lookup::Failed(Failure::Other));
    }

    #[tokio::test]
    async fn a_search_that_cannot_reach_the_shim_is_unreachable() {
        let resolver = build(Some("http://127.0.0.1:1"));
        assert_eq!(
            resolver.search_lookup("Artist Song", None, true, &[]).await,
            Lookup::Failed(Failure::Unreachable)
        );
    }

    #[test]
    fn absent_setting_uses_the_compose_service_name() {
        assert_eq!(build(None).base_url(), YouTubeResolver::DEFAULT_BASE_URL);
    }

    #[test]
    fn blank_setting_uses_the_compose_service_name() {
        for shim_url in ["", "   "] {
            assert_eq!(
                build(Some(shim_url)).base_url(),
                YouTubeResolver::DEFAULT_BASE_URL,
                "{shim_url:?}"
            );
        }
    }

    #[test]
    fn configured_setting_is_kept_without_a_trailing_slash() {
        for (shim_url, expected) in [
            ("http://shim.local:8080", "http://shim.local:8080"),
            ("http://shim.local:8080/", "http://shim.local:8080"),
            ("  http://shim.local:8080/  ", "http://shim.local:8080"),
        ] {
            assert_eq!(build(Some(shim_url)).base_url(), expected, "{shim_url:?}");
        }
    }

    // ---- The shim's endpoints ------------------------------------------------------------

    #[test]
    fn queries_are_escaped_except_unreserved_characters() {
        // The reference escaping of "a b+c/é~*'()!".
        assert_eq!(escape("a b+c/é~*'()!"), "a%20b%2Bc%2F%C3%A9~%2A%27%28%29%21");
    }

    #[tokio::test]
    async fn search_asks_with_the_hint_and_background_flag_and_reads_the_hit() {
        let shim = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "Daft Punk Emotion"))
            .and(query_param("duration", "417"))
            .and(query_param("bg", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"video_id":"abc","title":"Emotion","duration":417,"channel":"Daft Punk - Topic"}"#,
            ))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&format!("{}/", shim.uri())));
        let hit = resolver.search("Daft Punk Emotion", Some(417), true).await;
        assert_eq!(
            hit,
            Some(YouTubeHit {
                video_id: "abc".into(),
                title: Some("Emotion".into()),
                duration: Some(417),
                channel: Some("Daft Punk - Topic".into()),
                album: None,
                stream: StreamFacts::default(),
                pick: 0,
            })
        );
        assert!(resolver.search("  ", None, false).await.is_none());
    }

    #[tokio::test]
    async fn a_ranked_answer_names_the_ranking_that_chose_the_video() {
        let shim = MockServer::start().await;
        Mock::given(path("/search"))
            .and(query_param("q", "Ranked Song"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"video_id":"abc","rank":3}"#))
            .mount(&shim)
            .await;
        Mock::given(path("/search"))
            .and(query_param("q", "Newer Song"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"video_id":"abc","rank":9}"#))
            .mount(&shim)
            .await;
        Mock::given(path("/search"))
            .and(query_param("q", "Plain Song"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"video_id":"abc"}"#))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let pick = |q: &'static str| {
            let resolver = &resolver;
            async move { resolver.search(q, None, false).await.map(|h| h.pick) }
        };
        assert_eq!(pick("Ranked Song").await, Some(3));
        assert_eq!(pick("Newer Song").await, Some(SoulseekRouting::VIDEO_PICK));
        assert_eq!(pick("Plain Song").await, Some(0));
    }

    #[tokio::test]
    async fn search_names_the_videos_to_leave_out_only_when_there_are_some() {
        let shim = MockServer::start().await;
        Mock::given(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"video_id":"abc"}"#))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        resolver
            .search_skipping("Song", Some(200), false, &["a".to_string(), "b".to_string()])
            .await
            .expect("a hit");
        resolver.search("Song", Some(200), false).await.expect("a hit");

        let requests = shim.received_requests().await.expect("recorded");
        let queries: Vec<String> = requests
            .iter()
            .map(|r| r.url.query().unwrap_or_default().to_string())
            .collect();
        assert_eq!(queries.len(), 2);
        assert!(queries[0].contains("skip=a%2Cb"), "{}", queries[0]);
        assert!(!queries[1].contains("skip"), "{}", queries[1]);
    }

    #[tokio::test]
    async fn search_reads_the_stream_the_shim_picked() {
        let shim = MockServer::start().await;
        Mock::given(path("/search"))
            .and(query_param("q", "Daft Punk Emotion"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"video_id":"abc","duration":417,"size":6755585,"itag":"140","ext":"m4a","abr":129.486}"#,
            ))
            .mount(&shim)
            .await;
        Mock::given(path("/search"))
            .and(query_param("q", "odd stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"video_id":"x","size":"big","itag":140,"abr":null}"#),
            )
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let hit = resolver
            .search("Daft Punk Emotion", None, false)
            .await
            .expect("a hit");
        assert_eq!(
            hit.stream,
            StreamFacts {
                size: Some(6_755_585),
                itag: Some("140".into()),
                ext: Some("m4a".into()),
                bit_rate: Some(129),
                ..Default::default()
            }
        );
        // Odd stream fields are unknown, and the hit still counts.
        let odd = resolver.search("odd stream", None, false).await.expect("a hit");
        assert_eq!(odd.stream, StreamFacts::default());
    }

    #[tokio::test]
    async fn facts_asks_for_the_video_in_the_background_and_reads_its_stream() {
        let shim = MockServer::start().await;
        Mock::given(path("/facts"))
            .and(query_param("id", "abc"))
            .and(query_param("bg", "1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"video_id":"abc","size":7012345,"itag":"251","ext":"webm","abr":160}"#,
                ),
            )
            .mount(&shim)
            .await;
        Mock::given(path("/facts"))
            .and(query_param("id", "gone"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&shim)
            .await;
        Mock::given(path("/facts"))
            .and(query_param("id", "empty"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"video_id":"empty"}"#))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let facts = resolver.facts("abc", true).await.expect("facts");
        assert_eq!(facts.size, Some(7_012_345));
        assert_eq!((facts.track_gain, facts.track_peak), (None, None));
        assert_eq!(facts.itag.as_deref(), Some("251"));
        assert_eq!(facts.bit_rate, Some(160));
        assert!(resolver.facts("gone", true).await.is_none());
        assert!(resolver.facts("empty", true).await.is_none());
        assert!(resolver.facts(" ", true).await.is_none());
    }

    #[test]
    fn facts_read_the_measured_replay_gain_and_its_peak() {
        let facts = parse_stream(&serde_json::json!({
            "video_id": "abc", "size": 6755585, "track_gain": -10.0, "track_peak": 1.135011
        }));
        assert_eq!(facts.track_gain, Some(-10.0));
        assert_eq!(facts.track_peak, Some(1.135011));
        // A silent track has no peak; a peak alone is not taken.
        let silent = parse_stream(&serde_json::json!({"track_gain": 3.5, "track_peak": null}));
        assert_eq!((silent.track_gain, silent.track_peak), (Some(3.5), None));
        let peak_only = parse_stream(&serde_json::json!({"track_peak": 0.9}));
        assert_eq!(peak_only, StreamFacts::default());
    }

    #[tokio::test]
    async fn search_and_meta_answer_none_on_a_miss_an_error_or_an_odd_answer() {
        let shim = MockServer::start().await;
        Mock::given(path("/search"))
            .and(query_param("q", "missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&shim)
            .await;
        Mock::given(path("/search"))
            .and(query_param("q", "no id"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"video_id":"","title":"x"}"#))
            .mount(&shim)
            .await;
        Mock::given(path("/search"))
            .and(query_param("q", "odd"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"video_id":"v","duration":1.5}"#))
            .mount(&shim)
            .await;
        Mock::given(path("/meta"))
            .and(query_param("q", "meta"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"video_id":"m","duration":"200","channel":"ignored"}"#),
            )
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        assert!(resolver.search("missing", None, false).await.is_none());
        assert!(resolver.search("no id", None, false).await.is_none());
        assert!(resolver.search("odd", None, false).await.is_none());
        let meta = resolver.meta("meta", Some(0), false).await.expect("a hit");
        assert_eq!(
            (meta.video_id.as_str(), meta.duration, meta.channel),
            ("m", None, None)
        );
        assert!(resolver.meta("other", None, false).await.is_none());
    }

    #[tokio::test]
    async fn open_stream_passes_the_range_and_reports_the_partial_answer() {
        let shim = MockServer::start().await;
        Mock::given(path("/stream"))
            .and(query_param("id", "abc"))
            .and(header("range", "bytes=0-1"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("content-range", "bytes 0-1/1000")
                    .set_body_raw(vec![1u8, 2], "audio/webm"),
            )
            .mount(&shim)
            .await;
        Mock::given(path("/stream"))
            .and(query_param("id", "gone"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let stream = resolver
            .open_stream("abc", Some("bytes=0-1"))
            .await
            .expect("a stream");
        assert_eq!(stream.status_code, 206);
        assert_eq!(stream.content_type, "audio/webm");
        assert_eq!(stream.content_length, Some(2));
        assert_eq!(stream.content_range.as_deref(), Some("bytes 0-1/1000"));
        assert_eq!(
            stream.response.bytes().await.expect("the body").as_ref(),
            &[1u8, 2]
        );
        assert!(resolver.open_stream("gone", None).await.is_none());
        assert!(resolver.open_stream(" ", None).await.is_none());
    }

    #[tokio::test]
    async fn open_stream_hands_back_a_range_past_the_end_with_the_size() {
        let shim = MockServer::start().await;
        Mock::given(path("/stream"))
            .and(query_param("id", "abc"))
            .respond_with(ResponseTemplate::new(416).insert_header("content-range", "bytes */1000"))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let stream = resolver
            .open_stream("abc", Some("bytes=5000-"))
            .await
            .expect("the refusal is an answer");
        assert_eq!(stream.status_code, 416);
        assert_eq!(stream.content_range.as_deref(), Some("bytes */1000"));
    }

    #[tokio::test]
    async fn download_names_the_destination_and_tags_and_returns_the_path() {
        let shim = MockServer::start().await;
        Mock::given(path("/download"))
            .and(query_param("id", "abc"))
            .and(query_param("dest", "/music/A/B"))
            .and(query_param("artist", "A & B"))
            .and(query_param("title", "T"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"path":"/music/A/B.mp3"}"#))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        assert_eq!(
            resolver
                .download("abc", "/music/A/B", Some("A & B"), Some("T"))
                .await
                .as_deref(),
            Some("/music/A/B.mp3")
        );
        assert!(
            resolver
                .download("abc", "/elsewhere", None, Some(""))
                .await
                .is_none()
        );
        assert!(resolver.download("", "/x", None, None).await.is_none());
    }

    #[tokio::test]
    async fn busy_reads_the_counts_from_the_shims_health() {
        let shim = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"ok":true,"active_streams":3,"ytdlp_running":2,"ytdlp_limit":128}"#),
            )
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        assert_eq!(
            resolver.busy().await,
            Some(ShimBusy {
                active_streams: 3,
                ytdlp_running: 2,
                ytdlp_limit: 128,
            })
        );
    }

    #[tokio::test]
    async fn busy_is_none_when_the_shim_does_not_answer() {
        let closed = format!("http://{}", crate::services::test_support::refused_address());
        assert_eq!(YouTubeResolver::with_base_url(Some(&closed)).busy().await, None);

        let failing = MockServer::start().await;
        Mock::given(path("/health"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&failing)
            .await;
        assert_eq!(
            YouTubeResolver::with_base_url(Some(&failing.uri())).busy().await,
            None
        );

        let not_counts = MockServer::start().await;
        Mock::given(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&not_counts)
            .await;
        assert_eq!(
            YouTubeResolver::with_base_url(Some(&not_counts.uri()))
                .busy()
                .await,
            None
        );

        let slow = MockServer::start().await;
        Mock::given(path("/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"ok":true,"active_streams":1,"ytdlp_running":0,"ytdlp_limit":5}"#)
                    .set_delay(YouTubeResolver::HEALTH_TIMEOUT + Duration::from_secs(3)),
            )
            .mount(&slow)
            .await;
        let started = std::time::Instant::now();
        assert_eq!(
            YouTubeResolver::with_base_url(Some(&slow.uri())).busy().await,
            None
        );
        assert!(
            started.elapsed() < YouTubeResolver::HEALTH_TIMEOUT + Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn busy_reuses_one_answer_until_it_is_five_seconds_old() {
        let shim = MockServer::start().await;
        Mock::given(path("/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"ok":true,"active_streams":1,"ytdlp_running":1,"ytdlp_limit":5}"#),
            )
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let reads = || async { shim.received_requests().await.expect("recorded").len() };
        let first = resolver.busy().await;
        assert!(first.is_some());
        for _ in 0..3 {
            assert_eq!(resolver.busy().await, first);
        }
        assert_eq!(reads().await, 1);

        tokio::time::pause();
        tokio::time::advance(YouTubeResolver::BUSY_CACHE + Duration::from_secs(1)).await;
        tokio::time::resume();
        assert_eq!(resolver.busy().await, first);
        assert_eq!(reads().await, 2);
    }

    #[tokio::test]
    async fn busy_reuses_a_missing_answer_too() {
        let shim = MockServer::start().await;
        Mock::given(path("/health"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        assert_eq!(resolver.busy().await, None);
        assert_eq!(resolver.busy().await, None);
        assert_eq!(shim.received_requests().await.expect("recorded").len(), 1);
    }

    const HELLO_ROWS: &str = r#"{"tracks":[
        {"videoId":"omv1","title":"Hello","artists":["Adele"],"durationSeconds":366,"videoType":"MUSIC_VIDEO_TYPE_OMV"},
        {"videoId":"atv1","title":"Hello","artists":["Adele"],"durationSeconds":295,"videoType":"MUSIC_VIDEO_TYPE_ATV"}
    ]}"#;

    /// YouTube Music is asked for five songs rows, and its release track comes back with its
    /// length, naming no stream.
    #[tokio::test]
    async fn music_song_reads_the_release_track_of_a_songs_search() {
        let shim = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ytm/search"))
            .and(query_param("q", "Adele Hello"))
            .and(query_param("filter", "songs"))
            .and(query_param("limit", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_string(HELLO_ROWS))
            .expect(1)
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let hit = resolver.music_song("Adele", "Hello", Some(296), false).await;
        assert_eq!(
            hit,
            Some(YouTubeHit {
                video_id: "atv1".into(),
                title: Some("Hello".into()),
                duration: Some(295),
                channel: None,
                album: None,
                stream: StreamFacts::default(),
                pick: SoulseekRouting::VIDEO_PICK,
            })
        );
    }

    /// A lookup started ahead is joined by the pick that follows it: one request to the shim.
    #[tokio::test]
    async fn a_prefetched_lookup_is_joined_by_the_pick() {
        let shim = MockServer::start().await;
        Mock::given(path("/ytm/search"))
            .and(query_param("q", "Adele Hello"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(HELLO_ROWS)
                    .set_delay(Duration::from_millis(300)),
            )
            .expect(1)
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));

        resolver.prefetch_music_song("Adele", "Hello");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        let hit = resolver.music_song("Adele", "Hello", Some(296), false).await;

        assert_eq!(hit.map(|h| h.video_id).as_deref(), Some("atv1"));
        assert!(started.elapsed() < Duration::from_millis(300));
    }

    /// A busy or failed YouTube Music, a row too far from the known length, a slow answer and a
    /// shim with no address set are all no hit, so the caller asks yt-dlp.
    #[tokio::test]
    async fn music_song_is_none_when_youtube_music_cannot_say() {
        let shim = MockServer::start().await;
        Mock::given(path("/ytm/search"))
            .and(query_param("q", "Busy Song"))
            .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"busy"}"#))
            .mount(&shim)
            .await;
        Mock::given(path("/ytm/search"))
            .and(query_param("q", "Broken Song"))
            .respond_with(ResponseTemplate::new(502).set_body_string(r#"{"error":"ytm_failed"}"#))
            .mount(&shim)
            .await;
        Mock::given(path("/ytm/search"))
            .and(query_param("q", "Slow Song"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        r#"{"tracks":[{"videoId":"s","title":"Song","artists":["Slow"],"durationSeconds":200}]}"#,
                    )
                    .set_delay(YouTubeResolver::MUSIC_TIMEOUT + Duration::from_millis(500)),
            )
            .mount(&shim)
            .await;
        Mock::given(path("/ytm/search"))
            .and(query_param("q", "Adele Hello"))
            .respond_with(ResponseTemplate::new(200).set_body_string(HELLO_ROWS))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        assert!(resolver.music_song("Busy", "Song", None, false).await.is_none());
        assert!(resolver.music_song("Broken", "Song", None, false).await.is_none());
        assert!(
            resolver
                .music_song("Adele", "Hello", Some(250), false)
                .await
                .is_none()
        );
        let started = std::time::Instant::now();
        assert!(resolver.music_song("Slow", "Song", None, false).await.is_none());
        assert!(started.elapsed() < YouTubeResolver::MUSIC_TIMEOUT + Duration::from_millis(400));
        assert!(
            YouTubeResolver::with_base_url(None)
                .music_song("Adele", "Hello", None, false)
                .await
                .is_none()
        );
    }

    /// The request's queries as the shim received them, one string each.
    async fn queries(shim: &MockServer) -> Vec<String> {
        shim.received_requests()
            .await
            .expect("recorded")
            .iter()
            .map(|r| r.url.query().unwrap_or("").to_string())
            .collect()
    }

    /// A background lookup asks for the shim's background slot; a play's does not.
    #[tokio::test]
    async fn a_background_lookup_carries_bg_and_a_play_does_not() {
        let shim = MockServer::start().await;
        Mock::given(path("/ytm/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(HELLO_ROWS))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let background = resolver.music_song("Adele", "Hello", Some(296), true).await;
        assert_eq!(background.map(|h| h.video_id).as_deref(), Some("atv1"));
        resolver.music_song("Adele", "Hello", Some(296), false).await;
        let asked = queries(&shim).await;
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked[0].contains("bg=1"), "{asked:?}");
        assert!(!asked[1].contains("bg="), "{asked:?}");
    }

    /// A play never joins a background lookup of the same song, nor the other way round.
    #[tokio::test]
    async fn a_background_and_a_play_lookup_of_one_song_are_two_requests() {
        let shim = MockServer::start().await;
        Mock::given(path("/ytm/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(HELLO_ROWS)
                    .set_delay(Duration::from_millis(200)),
            )
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let (background, play) = tokio::join!(
            resolver.music_song("Adele", "Hello", Some(296), true),
            resolver.music_song("Adele", "Hello", Some(296), false)
        );
        assert!(background.is_some() && play.is_some());
        assert_eq!(queries(&shim).await.len(), 2);
    }

    /// While a background lookup holds the one background permit, a play lookup goes through at
    /// once, and a second background lookup gets the permit when the first ends.
    #[tokio::test]
    async fn a_held_background_permit_never_makes_a_play_wait() {
        let shim = MockServer::start().await;
        Mock::given(path("/ytm/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(HELLO_ROWS))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        let held = resolver
            .music_bg_gate
            .clone()
            .acquire_owned()
            .await
            .expect("the gate is open");

        let started = std::time::Instant::now();
        let play = resolver.music_song("Adele", "Hello", Some(296), false).await;
        assert!(play.is_some());
        assert!(started.elapsed() < Duration::from_millis(500));

        let second = resolver.music_song("Adele", "Hello", Some(296), true);
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut second)
                .await
                .is_err(),
            "a second background lookup waits for the permit"
        );
        drop(held);
        let hit = tokio::time::timeout(YouTubeResolver::MUSIC_TIMEOUT, second)
            .await
            .expect("it gets the permit when the first ends");
        assert_eq!(hit.map(|h| h.video_id).as_deref(), Some("atv1"));
    }

    /// The shim's busy answer to a background lookup is a busy failure, not "no row".
    #[tokio::test]
    async fn a_busy_background_lookup_is_a_busy_failure() {
        let shim = MockServer::start().await;
        Mock::given(path("/ytm/search"))
            .and(query_param("bg", "1"))
            .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"busy"}"#))
            .mount(&shim)
            .await;
        let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
        assert_eq!(
            resolver.music_song_lookup("Adele", "Hello", None, true).await,
            Lookup::Failed(Failure::Busy)
        );
    }
}
