//! Animated art for the Octo apps, from Apple Music. An album is matched to its Apple
//! release (by barcode when one is known, else by a strict search on artist and title), and its
//! Apple Music page names the loops Apple plays there: a square one where the cover sits and a
//! tall one for a full-screen player, each an HLS playlist with a still frame and a background
//! color. Every answer is kept in `animated-art.json`, "none" included, so an album costs Apple
//! one page a month at most (a week while it has none).
//!
//! `direct` hands the apps Apple's own addresses, which play from Apple's video network.
//! `relay` hands them `animated/{token}/...` on this server instead, where [`AnimatedArt`]
//! fetches the playlists (rewritten so every address in them points back here), the video and
//! the stills. A token is random per album and only ever leads to that album's loops on Apple's
//! video host, so the relay fetches nothing else.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use futures::StreamExt;
use octo_core::charts::apple_pages::{ArtLoop, LOOP_VIDEO_HOST, PageLoops, is_loop_video};
use octo_core::common::SongIdentity;
use octo_core::settings::{AnimatedArtMode as Mode, SettingsStore};
use parking_lot::RwLock;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::ITunesCoverArtLookup;
use crate::services::charts::AppleMusicPages;
use crate::services::charts::apple_music_pages::{LaneCell, PageError};
use crate::services::framework::InFlight;
use crate::services::framework::http::client_builder;
use crate::services::framework::memory_cache::MemoryCache;
use crate::services::state_file;

/// How long found artwork is trusted before its page is read again.
const FOUND_FOR_SECS: i64 = 30 * 24 * 60 * 60;
/// How long "no animated art" is trusted: Apple adds them to older albums now and then.
const NONE_FOR_SECS: i64 = 7 * 24 * 60 * 60;
/// The most albums kept. Past it the oldest tenth goes in one pass.
const MAX_ALBUMS: usize = 20_000;
/// How often the kept answers are written, when any changed.
const FLUSH_EVERY: Duration = Duration::from_secs(15);
/// Where the relay path starts, relative to the server address as `rest/` is.
pub const RELAY_ROOT: &str = "animated";
/// Every loop file lives under this path on Apple's video host.
const ASSET_ROOT: &str = "/itunes-assets/";
/// The longest playlist the relay rewrites; Apple's are a few kilobytes.
const MAX_PLAYLIST: usize = 1024 * 1024;
/// How long a rewritten playlist is served from memory. Apple's loop playlists never change.
const PLAYLIST_FOR: Duration = Duration::from_secs(6 * 60 * 60);
/// Memory the rewritten playlists may take.
const PLAYLIST_MEMORY: u64 = 16 * 1024 * 1024;
/// How long after start the first library sweep waits, so a restart is not slowed by it.
const SWEEP_START: Duration = Duration::from_secs(2 * 60);
/// How often the library is swept again; only albums whose answer went stale are asked.
const SWEEP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// Apple not answering this many albums in a row pauses the sweep for [`SWEEP_PAUSE`].
const SWEEP_MISSES: u32 = 5;
const SWEEP_PAUSE: Duration = Duration::from_secs(10 * 60);
/// How long the sweep waits on one album before going on (its lookup still finishes).
const SWEEP_WAIT: Duration = Duration::from_secs(120);

/// The library's albums, as `(artist, album)`, or `None` while the library cannot be read.
pub type LibraryAlbums =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, Option<Vec<(String, String)>>> + Send + Sync>;

/// The library sweep's progress, for the dashboard.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepProgress {
    pub running: bool,
    pub checked: usize,
    pub total: usize,
    pub found: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_utc: Option<String>,
}

/// One loop as the apps get it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clip {
    pub video: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub width: u32,
    pub height: u32,
}

impl From<ArtLoop> for Clip {
    fn from(clip: ArtLoop) -> Self {
        Clip {
            video: clip.video,
            poster: clip.poster.filter(|p| is_still(p)),
            color: clip.color,
            width: clip.width,
            height: clip.height,
        }
    }
}

/// An album's answer as `animated-art.json` keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kept {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    square: Option<Clip>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tall: Option<Clip>,
    /// When the page was read, in seconds since 1970.
    at: i64,
    /// The relay token, for found artwork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    /// The album as it was asked for, for the dashboard's list.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    artist: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    album: String,
    /// Hidden from the dashboard: no app is given this album's artwork. Kept when the page is
    /// read again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
}

impl Kept {
    fn found(&self) -> bool {
        self.square.is_some() || self.tall.is_some()
    }

    fn fresh(&self, now: i64) -> bool {
        let keep = if self.found() {
            FOUND_FOR_SECS
        } else {
            NONE_FOR_SECS
        };
        now.saturating_sub(self.at) < keep
    }

    fn cover(&self) -> Option<FoundArtwork> {
        (self.found() && !self.hidden).then(|| FoundArtwork {
            square: self.square.clone(),
            tall: self.tall.clone(),
            token: self.token.clone().unwrap_or_default(),
        })
    }
}

/// Found artwork: either loop may be missing, never both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundArtwork {
    pub square: Option<Clip>,
    pub tall: Option<Clip>,
    pub token: String,
}

/// What a lookup came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ArtLookup {
    Found(FoundArtwork),
    /// Apple has no animated art for the album, or no such album.
    None,
    /// Off, or Apple could not be asked this time; nothing was kept.
    #[default]
    Unavailable,
}

/// What a relayed file name stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayFile {
    /// An HLS playlist, rewritten before it is served.
    Playlist(Url),
    /// Video or a still, passed through as it comes.
    Media(Url),
}

/// Forgets a lookup's lane when dropped, with the lookup.
struct ForgetLane(Arc<AnimatedArt>, String);

impl Drop for ForgetLane {
    fn drop(&mut self) {
        self.0.urgent.lock().remove(&self.1);
    }
}

pub struct AnimatedArt {
    settings: Arc<SettingsStore>,
    itunes: Arc<ITunesCoverArtLookup>,
    pages: Arc<AppleMusicPages>,
    path: Option<PathBuf>,
    kept: RwLock<HashMap<String, Kept>>,
    /// Relay token to album key, so a relayed request finds its album in one step.
    tokens: RwLock<HashMap<String, String>>,
    dirty: AtomicBool,
    flights: InFlight<ArtLookup>,
    /// The lane each running lookup reads in, by album key, so a request that joins a background
    /// lookup can turn it urgent.
    urgent: parking_lot::Mutex<HashMap<String, LaneCell>>,
    playlists: MemoryCache<Bytes>,
    relay_client: reqwest::Client,
    /// Tests only: the origin standing in for Apple's video host.
    video_origin: Option<String>,
    sweep: parking_lot::Mutex<SweepProgress>,
    /// Wakes the sweep at once (the dashboard's Look up my library now).
    kick: tokio::sync::Notify,
}

impl AnimatedArt {
    pub fn new(
        settings: Arc<SettingsStore>,
        itunes: Arc<ITunesCoverArtLookup>,
        pages: Arc<AppleMusicPages>,
        path: Option<PathBuf>,
    ) -> Self {
        let kept: HashMap<String, Kept> = path
            .as_deref()
            .and_then(|path| match state_file::read_text(path) {
                Ok(Some(text)) => serde_json::from_str(&text)
                    .inspect_err(|e| {
                        warn!(
                            "{} could not be read ({e}); animated art is asked again",
                            path.display()
                        )
                    })
                    .ok(),
                Ok(None) => None,
                Err(e) => {
                    warn!(
                        "{} could not be read ({e}); animated art is asked again",
                        path.display()
                    );
                    None
                }
            })
            .unwrap_or_default();
        let tokens = kept
            .iter()
            .filter_map(|(key, k)| Some((k.token.clone()?, key.clone())))
            .collect();
        let relay_client = client_builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .build()
            .expect("the relay client builds");
        Self {
            settings,
            itunes,
            pages,
            path,
            kept: RwLock::new(kept),
            tokens: RwLock::new(tokens),
            dirty: AtomicBool::new(false),
            flights: InFlight::new(),
            urgent: parking_lot::Mutex::new(HashMap::new()),
            playlists: MemoryCache::new(PLAYLIST_MEMORY),
            relay_client,
            video_origin: None,
            sweep: parking_lot::Mutex::new(SweepProgress::default()),
            kick: tokio::sync::Notify::new(),
        }
    }

    /// Tests only: Apple's video host is `origin` (a mock server), over http.
    #[cfg(test)]
    pub fn with_video_origin(mut self, origin: &str) -> Self {
        self.video_origin = Some(origin.trim_end_matches('/').to_string());
        self
    }

    /// Tests only: keep `cover` for an album as if its page had been read now.
    #[cfg(test)]
    pub fn keep(&self, artist: &str, album: &str, square: Option<Clip>, tall: Option<Clip>) -> String {
        let art = LoopsKept { square, tall };
        self.remember(&album_key(artist, album), artist, album, art);
        self.kept.read()[&album_key(artist, album)]
            .token
            .clone()
            .unwrap_or_default()
    }

    pub fn mode(&self) -> Mode {
        self.settings.current().metadata.animated_art()
    }

    /// Whether the apps are told of animated art at all.
    pub fn offered(&self) -> bool {
        self.mode() != Mode::Off
    }

    /// What is kept for the album, without asking Apple; when nothing fresh is kept, a lookup
    /// starts in the background for the next time. Never waits.
    pub fn peek(self: &Arc<Self>, artist: &str, album: &str, barcode: Option<&str>) -> Option<FoundArtwork> {
        if !self.offered() {
            return None;
        }
        let key = album_key(artist, album);
        if key.is_empty() {
            return None;
        }
        if let Some(kept) = self.kept.read().get(&key).filter(|k| k.fresh(now())) {
            return kept.cover();
        }
        drop(self.start(key, artist, album, barcode, &LaneCell::behind()));
        None
    }

    /// [`Self::find_on`] in the background lane.
    pub async fn find(
        self: &Arc<Self>,
        artist: &str,
        album: &str,
        barcode: Option<&str>,
        wait: Duration,
    ) -> ArtLookup {
        self.find_on(artist, album, barcode, wait, &LaneCell::behind())
            .await
    }

    /// The album's artwork, asking Apple when nothing fresh is kept, for at most `wait`. A lookup
    /// still running then goes on, and its answer is kept. The Apple reads go in `lane`; asking
    /// in the interactive lane for an album a background lookup is already reading turns that
    /// lookup interactive.
    pub async fn find_on(
        self: &Arc<Self>,
        artist: &str,
        album: &str,
        barcode: Option<&str>,
        wait: Duration,
        lane: &LaneCell,
    ) -> ArtLookup {
        if !self.offered() {
            return ArtLookup::Unavailable;
        }
        let key = album_key(artist, album);
        if key.is_empty() {
            return ArtLookup::None;
        }
        if let Some(kept) = self.kept.read().get(&key).filter(|k| k.fresh(now())) {
            return kept.cover().map_or(ArtLookup::None, ArtLookup::Found);
        }
        let lookup = self.start(key, artist, album, barcode, lane);
        tokio::time::timeout(wait, lookup)
            .await
            .unwrap_or(ArtLookup::Unavailable)
    }

    /// The lookup for `key`: the one running, or a new one reading in `lane`.
    fn start(
        self: &Arc<Self>,
        key: String,
        artist: &str,
        album: &str,
        barcode: Option<&str>,
        lane: &LaneCell,
    ) -> futures::future::Shared<futures::future::BoxFuture<'static, ArtLookup>> {
        let this = Arc::clone(self);
        let (artist, album, barcode) = (artist.to_string(), album.to_string(), barcode.map(str::to_string));
        let flight_key = key.clone();
        let ask_lane = lane.clone();
        // The lane is recorded under the same lock that starts the lookup, so one that ends at
        // once cannot leave its lane behind.
        let mut urgent = self.urgent.lock();
        let (running, joined) = self.flights.run_joined(&flight_key, async move {
            let _forget = ForgetLane(Arc::clone(&this), key.clone());
            this.ask(&key, &artist, &album, barcode.as_deref(), &ask_lane)
                .await
        });
        if joined {
            let started = urgent.get(&flight_key).cloned();
            drop(urgent);
            if lane.is_now()
                && let Some(started) = started
                && started.make_now()
            {
                self.pages.gate().wake_behind();
            }
        } else {
            urgent.insert(flight_key, lane.clone());
            drop(urgent);
        }
        running
    }

    async fn ask(
        &self,
        key: &str,
        artist: &str,
        album: &str,
        barcode: Option<&str>,
        lane: &LaneCell,
    ) -> ArtLookup {
        let id = match self.itunes.album_collection_id(artist, album, barcode).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                self.remember(key, artist, album, LoopsKept::default());
                return ArtLookup::None;
            }
            Err(e) => {
                debug!("Animated art for '{artist} - {album}' not looked up: {e}");
                return ArtLookup::Unavailable;
            }
        };
        let country = self
            .pages
            .detected_country_on(lane)
            .await
            .unwrap_or_else(|| "us".to_string());
        let art = match self.pages.album_animated_art_on(&country, id, lane).await {
            Ok(art) => LoopsKept::from(art),
            Err(PageError::NoSuchPage) => LoopsKept::default(),
            Err(e) => {
                debug!(
                    "Apple Music page of '{artist} - {album}' not read: {}",
                    e.describe()
                );
                return ArtLookup::Unavailable;
            }
        };
        if art.square.is_some() || art.tall.is_some() {
            info!("Animated art found for '{artist} - {album}'");
        }
        self.remember(key, artist, album, art);
        self.kept
            .read()
            .get(key)
            .and_then(Kept::cover)
            .map_or(ArtLookup::None, ArtLookup::Found)
    }

    fn remember(&self, key: &str, artist: &str, album: &str, art: LoopsKept) {
        let mut kept = self.kept.write();
        let found = art.square.is_some() || art.tall.is_some();
        let hidden = kept.get(key).is_some_and(|k| k.hidden);
        // Artwork found again keeps its token, so the addresses an app holds still work.
        let token = found.then(|| {
            kept.get(key)
                .and_then(|k| k.token.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string())
        });
        let previous = kept.insert(
            key.to_string(),
            Kept {
                square: art.square,
                tall: art.tall,
                at: now(),
                token: token.clone(),
                artist: artist.trim().to_string(),
                album: album.trim().to_string(),
                hidden,
            },
        );
        {
            let mut tokens = self.tokens.write();
            if let Some(old) = previous
                .and_then(|p| p.token)
                .filter(|old| Some(old) != token.as_ref())
            {
                tokens.remove(&old);
            }
            if let Some(token) = token {
                tokens.insert(token, key.to_string());
            }
        }
        if kept.len() > MAX_ALBUMS {
            self.trim(&mut kept);
        }
        self.dirty.store(true, Ordering::Release);
    }

    /// Drops exactly the oldest tenth (albums read in the same second tie, so this goes by
    /// count, not by a cutoff time). One linear pass, after which the next tenth of new albums
    /// trims nothing.
    fn trim(&self, kept: &mut HashMap<String, Kept>) {
        let cut = (kept.len() / 10).max(1);
        let mut ages: Vec<(i64, String)> = kept.iter().map(|(key, k)| (k.at, key.clone())).collect();
        ages.select_nth_unstable_by_key(cut - 1, |(at, _)| *at);
        let mut tokens = self.tokens.write();
        for (_, key) in ages.into_iter().take(cut) {
            if let Some(token) = kept.remove(&key).and_then(|gone| gone.token) {
                tokens.remove(&token);
            }
        }
    }

    /// Writes the kept answers every [`FLUSH_EVERY`] when any changed, and once more when
    /// `stopping` is canceled.
    pub async fn run_flush_loop(&self, stopping: CancellationToken) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                () = stopping.cancelled() => {
                    self.flush().await;
                    return Ok(());
                }
                () = tokio::time::sleep(FLUSH_EVERY) => self.flush().await,
            }
        }
    }

    async fn flush(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let text = match serde_json::to_string(&*self.kept.read()) {
            Ok(text) => text,
            Err(e) => {
                warn!("Animated art could not be written: {e}");
                return;
            }
        };
        let written = tokio::task::spawn_blocking(move || state_file::save_atomic(&path, &text)).await;
        if !matches!(written, Ok(Ok(()))) {
            self.dirty.store(true, Ordering::Release);
            warn!("Animated art could not be written; tried again shortly");
        }
    }

    /// `animatedArt` as the apps read it: `square` and `tall`, each with `video`, `poster`,
    /// `color`, `width` and `height`. In relay mode the addresses are relative to the server
    /// address, as `rest/` is.
    pub fn field(&self, cover: &FoundArtwork) -> Value {
        let relay = self.mode() == Mode::Relay;
        let mut field = Map::new();
        for (shape, clip) in [("square", &cover.square), ("tall", &cover.tall)] {
            let Some(clip) = clip else {
                continue;
            };
            let (video, poster) = if relay {
                (
                    format!("{RELAY_ROOT}/{}/{shape}.m3u8", cover.token),
                    clip.poster
                        .as_ref()
                        .map(|_| format!("{RELAY_ROOT}/{}/{shape}.jpg", cover.token)),
                )
            } else {
                (clip.video.clone(), clip.poster.clone())
            };
            let mut entry = Map::new();
            entry.insert("video".into(), json!(video));
            if let Some(poster) = poster {
                entry.insert("poster".into(), json!(poster));
            }
            if let Some(color) = &clip.color {
                entry.insert("color".into(), json!(color));
            }
            entry.insert("width".into(), json!(clip.width));
            entry.insert("height".into(), json!(clip.height));
            field.insert(shape.into(), Value::Object(entry));
        }
        Value::Object(field)
    }

    /// What `animated/{token}/{file}` stands for, while relaying is on: the top playlists
    /// (`square.m3u8`, `tall.m3u8`), the stills (`square.jpg`, `tall.jpg`), or a name a relayed
    /// playlist gave a file under Apple's asset path.
    pub fn relay_file(&self, token: &str, file: &str) -> Option<RelayFile> {
        if self.mode() != Mode::Relay {
            return None;
        }
        let key = self.tokens.read().get(token)?.clone();
        let named = {
            let kept = self.kept.read();
            let entry = kept.get(&key)?;
            let clip = |shape: &str| match shape {
                "square" => entry.square.as_ref(),
                "tall" => entry.tall.as_ref(),
                _ => None,
            };
            match file.rsplit_once('.') {
                Some((shape, "m3u8")) if clip(shape).is_some() => {
                    Some(RelayFile::Playlist(self.on_video_host(&clip(shape)?.video)?))
                }
                Some((shape, "jpg")) if clip(shape).is_some() => {
                    let poster = clip(shape)?.poster.as_deref()?;
                    Some(RelayFile::Media(
                        Url::parse(poster).ok().filter(|u| is_still(u.as_str()))?,
                    ))
                }
                _ => None,
            }
        };
        named.or_else(|| self.asset_named(file))
    }

    /// A file a relayed playlist named: base64url of its path under [`ASSET_ROOT`], then its
    /// extension. The host and root are always Apple's, whatever the name decodes to.
    fn asset_named(&self, file: &str) -> Option<RelayFile> {
        let (encoded, extension) = file.rsplit_once('.')?;
        let path = String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()?;
        let url = Url::parse(&format!("{}{ASSET_ROOT}{path}", self.video_origin())).ok()?;
        if !url.path().starts_with(ASSET_ROOT) {
            return None;
        }
        Some(if extension == "m3u8" {
            RelayFile::Playlist(url)
        } else {
            RelayFile::Media(url)
        })
    }

    fn video_origin(&self) -> String {
        self.video_origin
            .clone()
            .unwrap_or_else(|| format!("https://{LOOP_VIDEO_HOST}"))
    }

    /// A loop address Apple gave, as the relay fetches it (the test origin in tests).
    fn on_video_host(&self, video: &str) -> Option<Url> {
        if !is_loop_video(video) {
            return None;
        }
        let url = Url::parse(video).ok()?;
        match &self.video_origin {
            None => Some(url),
            Some(origin) => Url::parse(&format!("{origin}{}", url.path())).ok(),
        }
    }

    /// Sweeps the library: after [`SWEEP_START`], then every [`SWEEP_EVERY`] or when the
    /// dashboard asks, every album without a fresh answer is looked up, one after another at
    /// Apple's own pace, until `stopping` is canceled. Nothing runs while covers are off.
    pub async fn run_sweep_loop(
        self: Arc<Self>,
        albums: LibraryAlbums,
        stopping: CancellationToken,
    ) -> anyhow::Result<()> {
        tokio::select! {
            () = stopping.cancelled() => return Ok(()),
            () = tokio::time::sleep(SWEEP_START) => {}
            () = self.kick.notified() => {}
        }
        loop {
            if self.offered() {
                self.sweep(&albums, &stopping).await;
            }
            tokio::select! {
                () = stopping.cancelled() => return Ok(()),
                () = tokio::time::sleep(SWEEP_EVERY) => {}
                () = self.kick.notified() => {}
            }
        }
    }

    /// Starts a sweep now, or the next one as soon as the running one ends.
    pub fn sweep_now(&self) {
        self.kick.notify_one();
    }

    pub fn sweep_progress(&self) -> SweepProgress {
        self.sweep.lock().clone()
    }

    async fn sweep(self: &Arc<Self>, albums: &LibraryAlbums, stopping: &CancellationToken) {
        let Some(list) = albums().await else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        let list: Vec<(String, String, String)> = list
            .into_iter()
            .filter_map(|(artist, album)| {
                let key = album_key(&artist, &album);
                (!key.is_empty() && seen.insert(key.clone())).then_some((key, artist, album))
            })
            .collect();
        *self.sweep.lock() = SweepProgress {
            running: true,
            total: list.len(),
            ..Default::default()
        };
        let mut misses = 0;
        for (key, artist, album) in list {
            if stopping.is_cancelled() || !self.offered() {
                break;
            }
            let known = self
                .kept
                .read()
                .get(&key)
                .filter(|k| k.fresh(now()))
                .map(Kept::found);
            let found = match known {
                Some(found) => found,
                None => match self.find(&artist, &album, None, SWEEP_WAIT).await {
                    ArtLookup::Found(_) => {
                        misses = 0;
                        true
                    }
                    ArtLookup::None => {
                        misses = 0;
                        false
                    }
                    ArtLookup::Unavailable => {
                        misses += 1;
                        if misses >= SWEEP_MISSES {
                            misses = 0;
                            debug!("Apple is not answering; the animated art sweep waits a while");
                            tokio::select! {
                                () = stopping.cancelled() => break,
                                () = tokio::time::sleep(SWEEP_PAUSE) => {}
                            }
                        }
                        false
                    }
                },
            };
            let mut progress = self.sweep.lock();
            progress.checked += 1;
            progress.found += usize::from(found);
        }
        let mut progress = self.sweep.lock();
        progress.running = false;
        progress.finished_utc = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        info!(
            "Animated art sweep: {} of {} albums checked, {} with animated art",
            progress.checked, progress.total, progress.found
        );
    }

    /// The dashboard's card: the mode, how many albums are known, found and hidden, and the
    /// sweep.
    pub fn summary(&self) -> Value {
        let (known, found, hidden) = {
            let kept = self.kept.read();
            (
                kept.len(),
                kept.values().filter(|k| k.found()).count(),
                kept.values().filter(|k| k.found() && k.hidden).count(),
            )
        };
        json!({
            "mode": match self.mode() {
                Mode::Direct => "direct",
                Mode::Relay => "relay",
                Mode::Off => "off",
            },
            "known": known,
            "found": found,
            "hidden": hidden,
            "sweep": self.sweep_progress(),
        })
    }

    /// One page of the albums with animated art, by artist then album: each with its key,
    /// names, the still to show, whether it has a tall loop, and whether it is hidden. Also the
    /// count of them all.
    pub fn found_page(&self, offset: usize, limit: usize) -> (usize, Vec<Value>) {
        let kept = self.kept.read();
        let mut found: Vec<(&String, &Kept)> = kept.iter().filter(|(_, k)| k.found()).collect();
        found
            .sort_by_cached_key(|(key, k)| (k.artist.to_lowercase(), k.album.to_lowercase(), (*key).clone()));
        let total = found.len();
        let page = found
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(key, k)| {
                let still = still_of_kept(k);
                // Where Octo serves the still, so the dashboard never loads it from Apple.
                let served = still.as_ref().map(|_| {
                    let key: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
                    format!("/api/admin/animated-art/still?key={key}")
                });
                json!({
                    "key": key,
                    "artist": k.artist,
                    "album": k.album,
                    "poster": still,
                    "still": served,
                    "tall": k.tall.is_some(),
                    "hidden": k.hidden,
                })
            })
            .collect();
        (total, page)
    }

    /// The still of an album with animated art, by its key: its square loop's, else its tall
    /// loop's. None for a key with none.
    pub fn still_of(&self, key: &str) -> Option<String> {
        self.kept
            .read()
            .get(key)
            .filter(|k| k.found())
            .and_then(still_of_kept)
    }

    /// Hides or shows an album's animated art in every app. False for a key with none.
    pub fn set_hidden(&self, key: &str, hidden: bool) -> bool {
        let changed = match self.kept.write().get_mut(key).filter(|k| k.found()) {
            Some(kept) => {
                kept.hidden = hidden;
                true
            }
            None => false,
        };
        if changed {
            self.dirty.store(true, Ordering::Release);
        }
        changed
    }

    /// The relay's client: no overall timeout, so video is read as the player reads it.
    pub fn relay_client(&self) -> &reqwest::Client {
        &self.relay_client
    }

    /// A playlist with every address in it pointing back at the relay, from memory when it
    /// was rewritten lately. `None` when Apple did not give one.
    pub async fn relayed_playlist(&self, url: &Url) -> Option<Bytes> {
        if let Some(text) = self.playlists.get(url.as_str()) {
            return Some(text);
        }
        let response = self.relay_client.get(url.clone()).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            if body.len() + chunk.len() > MAX_PLAYLIST {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        let text = Bytes::from(rewrite_playlist(
            &String::from_utf8_lossy(&body),
            url,
            &self.video_origin(),
        ));
        self.playlists
            .set(url.as_str(), text.clone(), text.len() as u64, PLAYLIST_FOR);
        Some(text)
    }
}

/// The loops of a page, as kept.
#[derive(Debug, Clone, Default)]
struct LoopsKept {
    square: Option<Clip>,
    tall: Option<Clip>,
}

impl From<PageLoops> for LoopsKept {
    fn from(art: PageLoops) -> Self {
        LoopsKept {
            square: art.square.map(Clip::from),
            tall: art.tall.map(Clip::from),
        }
    }
}

fn album_key(artist: &str, album: &str) -> String {
    if artist.trim().is_empty() || album.trim().is_empty() {
        return String::new();
    }
    SongIdentity::match_key(artist, album)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The still an album's tile shows: its square loop's, else its tall loop's.
fn still_of_kept(kept: &Kept) -> Option<String> {
    kept.square
        .as_ref()
        .or(kept.tall.as_ref())
        .and_then(|clip| clip.poster.clone())
}

/// A still on Apple's image hosts, over https.
fn is_still(url: &str) -> bool {
    url.strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| host.to_ascii_lowercase().ends_with(".mzstatic.com"))
}

/// `playlist` with each address (URI lines and `URI="..."` attributes) resolved against `base`
/// and, when it lies under Apple's asset path, swapped for the relay's name for it. A sibling
/// name, so the player resolves it against the relayed playlist's own address.
pub(crate) fn rewrite_playlist(playlist: &str, base: &Url, origin: &str) -> String {
    let origin = Url::parse(origin).ok().map(|o| o.origin());
    let rename = |reference: &str| -> Option<String> {
        let url = base.join(reference.trim()).ok()?;
        if Some(url.origin()) != origin {
            return None;
        }
        let path = url.path().strip_prefix(ASSET_ROOT)?;
        let extension = path
            .rsplit('/')
            .next()
            .and_then(|last| last.rsplit_once('.'))
            .map(|(_, ext)| ext)
            .filter(|ext| (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
            .unwrap_or("bin");
        let mut named = URL_SAFE_NO_PAD.encode(path);
        named.push('.');
        named.push_str(extension);
        Some(named)
    };
    let mut out = String::with_capacity(playlist.len() + playlist.len() / 2);
    for line in playlist.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push_str(line);
        } else if trimmed.starts_with('#') {
            let mut rest = line;
            while let Some(at) = rest.find("URI=\"") {
                let start = at + 5;
                let Some(length) = rest[start..].find('"') else {
                    break;
                };
                out.push_str(&rest[..start]);
                let value = &rest[start..start + length];
                out.push_str(&rename(value).unwrap_or_else(|| value.to_string()));
                out.push('"');
                rest = &rest[start + length + 1..];
            }
            out.push_str(rest);
        } else {
            out.push_str(&rename(trimmed).unwrap_or_else(|| trimmed.to_string()));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
#[path = "animated_art_tests.rs"]
mod tests;
