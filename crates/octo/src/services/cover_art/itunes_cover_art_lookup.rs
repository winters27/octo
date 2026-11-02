//! Cover art from Apple's iTunes catalog. The barcode forms, which tagging reads too, are
//! `octo_core::tagging::release_distance::barcode_forms`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use indexmap::IndexMap;
use octo_core::common::cover_names::{cover_name_score, release_artist_fits, release_title_fits};
use octo_core::common::{Clock, SongIdentity, SongMatchOptions, dotnet};
// `BarcodeForms` came over with tagging, which reads it too.
use octo_core::json::element::{array_length, enumerate_array, get_string, str_prop, try_get_property};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
pub use octo_core::tagging::release_distance::barcode_forms;
use parking_lot::Mutex;
use regex::Regex;
use reqwest::StatusCode;
use reqwest::header::RANGE;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::i_cover_art_source::ICoverArtSource;
use crate::services::apple::{AppleClass, ApplePace, PaceConfig};
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::client_builder;

/// The size asked of Apple's CDN for a master. It answers with the original when
/// that is smaller, so this reads as "as large as there is". 5000, as sacad asks: 3000
/// capped the masters that are larger.
pub const MASTER_SIDE: u32 = 5000;

/// The tile a preview shows, made by Apple, so a preview never downloads a master.
pub const PROBE_THUMB_SIDE: u32 = 320;

/// Barcodes asked in one lookup. 30 came back in under a second, 29 matched.
pub const UPC_BATCH: usize = 40;

/// How long a match is trusted, and a miss (a search may find it later).
const MASTER_URL_TTL: chrono::Duration = chrono::Duration::days(30);
const MASTER_MISS_TTL: chrono::Duration = chrono::Duration::days(1);
const MASTER_URL_CAP: usize = 20_000;

/// How often the matches are written to disk.
const FLUSH_EVERY: Duration = Duration::from_secs(15);

static RELEASE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s+-\s+(?:Single|EP)\s*$").expect("the release suffix regex compiles"));

/// Song matching for album titles: no length tolerance.
fn album_titles() -> SongMatchOptions {
    SongMatchOptions {
        length_tolerance_seconds: None,
        ..Default::default()
    }
}

// Every search and lookup this class sends for a master waits its turn on the process's Apple
// pace (`ApplePace::shared`, class Cover), however many albums are being worked on at once and
// whichever instance sends it. Apple documents about 20 a minute, but answered 30 searches at 2 a
// second on 2026-10-01, and sacad asks up to 10 a second; so one a second, doubling (to 8 s at
// most) each time Apple refuses, and halving again after 50 answers in a row. The pace also holds
// Octo's catalog questions to Apple, and while those run (Deezer's searches answered from Apple)
// the cover searches below take their turns too.

/// How long a refused master search waits before it tries once more.
const RETRY_WAIT: Duration = Duration::from_secs(60);

/// One row of itunes-masters.json: a key, an optional URL and when it was found, with PascalCase
/// names matched case-sensitively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedMaster {
    #[serde(rename = "Key")]
    pub key: Option<String>,
    #[serde(rename = "Url")]
    pub url: Option<String>,
    #[serde(
        rename = "At",
        with = "octo_core::json::datetime::utc",
        default = "octo_core::json::datetime::min_value"
    )]
    pub at: DateTime<Utc>,
}

/// Cover art via Apple's free iTunes Search API. No key, 1200x1200 JPEGs after
/// CDN substitution (and the full master for downloads, see `try_fetch_album_master`), very
/// high coverage for mainstream Western releases: weaker for international, indie, and
/// underground.
///
/// Improvements over the original implementation:
/// - `country` filter dropped: US-only filtering kept missing releases that
///   are non-US-exclusive (lots of UK/EU/JP/KR catalog and re-releases).
/// - `limit` raised to 5: we then pick the result whose artist string
///   matches the routing best, instead of trusting iTunes' first hit blindly.
///   The first hit is often a "Karaoke Version" or different-artist cover that
///   shares the title.
/// - For routings whose Album is just the song title (the "single" convention
///   we use for placeholder songs), fall back from entity=album to entity=song
///   when the album-style query whiffs.
pub struct ITunesCoverArtLookup {
    client: reqwest::Client,
    base: String,
    clock: Clock,
    pace: Arc<ApplePace>,
    retry_wait: Duration,
    /// The matched master URLs (`None` is a remembered miss) and when each was found, in the
    /// order they were added.
    master_urls: Mutex<IndexMap<String, (Option<String>, DateTime<Utc>)>>,

    /// Matches kept on disk, so a restart does not send a whole library back to
    /// Apple. Written at most every 15 seconds.
    cache_path: Option<PathBuf>,
    dirty: AtomicBool,
}

impl ITunesCoverArtLookup {
    pub const BASE: &'static str = "https://itunes.apple.com";

    /// The lookup, loading its matches from `cache_path` when there is one. Start
    /// [`Self::run_flush_loop`] to keep the file written.
    pub fn new(cache_path: Option<PathBuf>) -> Self {
        Self::with_base_url(cache_path, Self::BASE, Clock::system())
            .with_pace(ApplePace::shared())
            .with_retry_wait(RETRY_WAIT)
    }

    /// A lookup against another host (a test's mock server), on a given clock, with a pace of its
    /// own that never waits and no wait before a retry.
    pub fn with_base_url(cache_path: Option<PathBuf>, base: &str, clock: Clock) -> Self {
        // A master at full size runs to a few megabytes; 8 s was cut close on a slow line.
        let client = client_builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("the iTunes client builds");
        let lookup = Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            clock,
            pace: Arc::new(ApplePace::new(PaceConfig::unpaced())),
            retry_wait: Duration::ZERO,
            master_urls: Mutex::new(IndexMap::new()),
            cache_path: cache_path.filter(|p| !dotnet::is_blank(&p.to_string_lossy())),
            dirty: AtomicBool::new(false),
        };
        lookup.load_cache();
        lookup
    }

    /// The same lookup taking its turns on `pace`.
    pub fn with_pace(mut self, pace: Arc<ApplePace>) -> Self {
        self.pace = pace;
        self
    }

    /// The same lookup waiting `wait` before it retries a refused master search.
    pub fn with_retry_wait(mut self, wait: Duration) -> Self {
        self.retry_wait = wait;
        self
    }

    /// Writes the matches every 15 seconds, and once more when the token is canceled.
    pub async fn run_flush_loop(&self, stopping: CancellationToken) -> anyhow::Result<()> {
        if self.cache_path.is_none() {
            return Ok(());
        }
        let mut ticks = tokio::time::interval_at(Instant::now() + FLUSH_EVERY, FLUSH_EVERY);
        loop {
            tokio::select! {
                _ = ticks.tick() => self.flush_cache(),
                () = stopping.cancelled() => {
                    self.flush_cache();
                    return Ok(());
                }
            }
        }
    }

    async fn get(&self, url: &str) -> anyhow::Result<HttpAnswer> {
        Ok(HttpAnswer::read(self.client.get(url).send().await?).await?)
    }

    /// A search or lookup through the gate. A refusal (403 or 429) waits a minute and
    /// tries once more, rather than losing the album.
    async fn apple_get(&self, url: &str) -> anyhow::Result<Option<HttpAnswer>> {
        let pace = &self.pace;
        for attempt in 0..2 {
            // A Cover turn is never refused.
            let _ = pace.turn(AppleClass::Cover).await;
            let response = self.get(url).await?;
            if matches!(
                response.status,
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
            ) {
                // Slower from now on, for every album and every catalog question, not just this one.
                pace.refused_by_apple();
                if attempt > 0 {
                    return Ok(Some(response));
                }
                info!(
                    "Apple asked Octo to slow down; waiting {} s, then one every {} s",
                    self.retry_wait.as_secs(),
                    pace.backoff().as_secs_f64()
                );
                tokio::time::sleep(self.retry_wait).await;
                continue;
            }
            pace.answered_by_apple();
            return Ok(Some(response));
        }
        Ok(None)
    }

    fn master_key(artist: &str, release: &str, single: bool) -> String {
        format!(
            "{}{}",
            SongIdentity::match_key(artist, release),
            if single { "|single" } else { "|album" }
        )
    }

    /// Finds many albums' masters at once by barcode: one Apple lookup for up to [`UPC_BATCH`]
    /// albums, where a search costs one request an album. Each answer is matched back by
    /// artist and album name (Apple does not say which barcode it answered, and adds the odd
    /// stray), and kept where [`Self::try_fetch_album_master`] looks first. Returns how many
    /// were matched; an album left out is found by search as before. `progress` hears how many
    /// albums have been asked about so far.
    pub async fn prime_by_barcode(
        &self,
        albums: &[(String, String, String)],
        progress: Option<&(dyn Fn(usize) + Send + Sync)>,
    ) -> usize {
        let mut matched = 0;
        let mut done = 0;
        let with_codes: Vec<&(String, String, String)> = albums
            .iter()
            .filter(|(_, _, upc)| !dotnet::is_blank(upc))
            .collect();
        for batch in with_codes.chunks(UPC_BATCH) {
            let attempt = async {
                // A file's tag often has the 13-digit EAN ("0602475682233") where the store
                // keeps the 12-digit UPC; both are sent, and the answers matched back by name.
                let mut codes: Vec<String> = Vec::new();
                for (_, _, upc) in batch {
                    for code in barcode_forms(Some(upc)) {
                        if !codes.contains(&code) {
                            codes.push(code);
                        }
                    }
                }
                let url = format!(
                    "{}/lookup?entity=album&limit=200&upc={}",
                    self.base,
                    codes
                        .iter()
                        .map(|c| dotnet::escape_data_string(c))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                let mut matched_here = 0;
                if let Some(resp) = self.apple_get(&url).await?
                    && resp.is_success()
                {
                    let doc: Value = serde_json::from_str(&resp.text())?;
                    let collections = collections_of(&doc)?;
                    for (artist, album, _) in batch {
                        let mut candidates: Vec<&Collection> = collections
                            .iter()
                            .filter(|c| {
                                SongIdentity::same_text(
                                    album,
                                    artist,
                                    &c.name,
                                    c.by.as_deref().unwrap_or(""),
                                    Some(&album_titles()),
                                )
                                .is_same()
                            })
                            .collect();
                        candidates.sort_by_key(|c| c.clean);
                        let Some(hit) = candidates.first().map(|c| c.art.clone()) else {
                            continue;
                        };
                        // Kept for both ways a song can ask: as its album, and as a single
                        // named after itself. A barcode names one release either way.
                        self.remember(Self::master_key(artist, album, false), Some(hit.clone()));
                        self.remember(Self::master_key(artist, album, true), Some(hit));
                        matched_here += 1;
                    }
                }
                anyhow::Ok(matched_here)
            };
            match attempt.await {
                Ok(n) => matched += n,
                Err(e) => debug!("iTunes barcode lookup failed: {e}"),
            }
            done += batch.len();
            if let Some(progress) = progress {
                progress(done);
            }
        }
        matched
    }

    /// Apple's id for an album, which names its Apple Music page. The barcode's release when
    /// one is given and Apple carries it, else what a search finds; either way the artist and
    /// the title must be the same release, the explicit one ahead of a clean one. Asks go
    /// through the shared Apple pacing; callers keep the answer. An error when Apple answered
    /// none of the asks, so a caller does not keep that as "not on Apple".
    pub async fn album_collection_id(
        &self,
        artist: &str,
        album: &str,
        barcode: Option<&str>,
    ) -> anyhow::Result<Option<u64>> {
        let (artist, album) = (artist.trim(), album.trim());
        if artist.is_empty() || album.is_empty() {
            return Ok(None);
        }
        let codes = barcode_forms(barcode);
        let by_code = (!codes.is_empty()).then(|| {
            format!(
                "{}/lookup?entity=album&upc={}",
                self.base,
                codes
                    .iter()
                    .map(|c| dotnet::escape_data_string(c))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        });
        let by_name = format!(
            "{}/search?term={}&entity=album&limit=15",
            self.base,
            dotnet::escape_data_string(&format!("{artist} {album}"))
        );
        let mut answered = false;
        for url in by_code.into_iter().chain([by_name]) {
            let answer = match self.apple_get(&url).await {
                Ok(Some(answer)) if answer.is_success() => answer,
                Ok(_) => continue,
                Err(e) => {
                    debug!("iTunes album id lookup failed for {artist} - {album}: {e}");
                    continue;
                }
            };
            let Ok(doc) = serde_json::from_str::<Value>(&answer.text()) else {
                continue;
            };
            let Ok(collections) = collections_of(&doc) else {
                continue;
            };
            answered = true;
            let best = collections
                .iter()
                .filter(|c| c.id.is_some())
                .filter(|c| {
                    SongIdentity::same_text(
                        album,
                        artist,
                        &c.name,
                        c.by.as_deref().unwrap_or(""),
                        Some(&album_titles()),
                    )
                    .is_same()
                })
                .min_by_key(|c| c.clean);
            if let Some(found) = best {
                return Ok(found.id);
            }
        }
        if answered {
            Ok(None)
        } else {
            anyhow::bail!("Apple answered none of the album id asks")
        }
    }

    /// The album's own cover at the largest size Apple has, or None. Strict where
    /// [`ICoverArtSource::try_fetch`] is loose: the artist AND the album title must be the same
    /// release (a single is matched by its song), because this cover is written into files,
    /// where another album's art would be a wrong tag rather than a soft picture. The match is
    /// remembered, so an album's tracks ask Apple once between them.
    pub async fn try_fetch_album_master(
        &self,
        artist: Option<&str>,
        album: Option<&str>,
        title: Option<&str>,
    ) -> Option<Bytes> {
        let url = self.master_url(artist, album, title).await?;
        for side in [MASTER_SIDE, 1200] {
            let sized = url.replace("100x100bb", &format!("{side}x{side}bb"));
            match self.get(&sized).await {
                Ok(resp) if resp.is_success() => return Some(resp.body),
                Ok(_) => {}
                Err(e) => debug!("iTunes master {sized} failed: {e}"),
            }
        }
        None
    }

    /// What the master is, without downloading it: its size from the first 64 KB (Apple
    /// answers range requests) and Apple's own 320 px copy for a tile. A preview of a thousand
    /// albums is about 120 KB each this way, where whole masters were about 3.4 MB each.
    pub async fn try_probe_album_master(
        &self,
        artist: Option<&str>,
        album: Option<&str>,
        title: Option<&str>,
    ) -> Option<(u32, Bytes)> {
        let url = self.master_url(artist, album, title).await?;
        let attempt = async {
            let head = url.replace("100x100bb", &format!("{MASTER_SIDE}x{MASTER_SIDE}bb"));
            let first = self
                .client
                .get(&head)
                .header(RANGE, "bytes=0-65535")
                .send()
                .await?;
            if !first.status().is_success() {
                return anyhow::Ok(None);
            }
            let start = read_up_to(first, 65_536).await?;
            let Some((width, height)) = octo_media::cover::cover_image::measure(&start) else {
                return Ok(None);
            };

            let tile = self
                .get(&url.replace("100x100bb", &format!("{PROBE_THUMB_SIDE}x{PROBE_THUMB_SIDE}bb")))
                .await?;
            if !tile.is_success() {
                return Ok(None);
            }
            Ok(Some((width.min(height), tile.body)))
        };
        match attempt.await {
            Ok(probe) => probe,
            Err(e) => {
                debug!(
                    "iTunes master probe failed for {} - {}: {e}",
                    artist.unwrap_or(""),
                    album.unwrap_or("")
                );
                None
            }
        }
    }

    /// The album's master URL at 100 px (sizes are swapped into it), from the
    /// remembered matches or one search.
    async fn master_url(
        &self,
        artist: Option<&str>,
        album: Option<&str>,
        title: Option<&str>,
    ) -> Option<String> {
        let artist = artist.map(str::trim).filter(|a| !a.is_empty())?;
        let album_blank = album.is_none_or(dotnet::is_blank);
        let title_blank = title.is_none_or(dotnet::is_blank);
        let single = album_blank
            || (!title_blank
                && SongIdentity::same_text(
                    album.unwrap_or(""),
                    artist,
                    title.unwrap_or(""),
                    artist,
                    Some(&album_titles()),
                )
                .is_same());
        let release = if single { title.or(album) } else { album }
            .map(str::trim)
            .filter(|r| !r.is_empty())?;

        let key = Self::master_key(artist, release, single);
        let known = self.master_urls.lock().get(&key).cloned();
        if let Some((url, at)) = known
            && self.clock.now() - at
                < if url.is_none() {
                    MASTER_MISS_TTL
                } else {
                    MASTER_URL_TTL
                }
        {
            return url;
        }
        let url = self.find_master_url(artist, release, single).await;
        self.remember(key, url.clone());
        url
    }

    fn remember(&self, key: String, url: Option<String>) {
        let mut urls = self.master_urls.lock();
        if urls.len() > MASTER_URL_CAP {
            urls.clear();
        }
        urls.insert(key, (url, self.clock.now()));
        self.dirty.store(true, Ordering::SeqCst);
    }

    fn load_cache(&self) {
        let Some(path) = self.cache_path.as_deref() else {
            return;
        };
        if !path.exists() {
            return;
        }
        let rows = match read_rows(path) {
            Ok(rows) => rows,
            Err(e) => {
                debug!("iTunes match cache could not be read: {e}");
                return;
            }
        };
        let now = self.clock.now();
        let mut urls = self.master_urls.lock();
        for row in rows {
            let ttl = if row.url.is_none() {
                MASTER_MISS_TTL
            } else {
                MASTER_URL_TTL
            };
            if now - row.at >= ttl {
                continue;
            }
            let Some(key) = row.key else {
                // A row without a key stops the load; the rows before it stay loaded.
                debug!("iTunes match cache could not be read: Value cannot be null. (Parameter 'key')");
                return;
            };
            urls.insert(key, (row.url, row.at));
        }
    }

    /// The matches as the file holds them.
    pub fn cached_rows(&self) -> Vec<CachedMaster> {
        self.master_urls
            .lock()
            .iter()
            .map(|(key, (url, at))| CachedMaster {
                key: Some(key.clone()),
                url: url.clone(),
                at: *at,
            })
            .collect()
    }

    /// Writes the matches when any changed since the last write.
    pub fn flush_cache(&self) {
        let Some(path) = self.cache_path.as_deref() else {
            return;
        };
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = write_rows(path, &self.cached_rows()) {
            self.dirty.store(true, Ordering::SeqCst);
            debug!("iTunes match cache could not be written: {e}");
        }
    }

    async fn find_master_url(&self, artist: &str, release: &str, single: bool) -> Option<String> {
        // An album by its name; a single by its song, whose release iTunes names "Song - Single".
        let entity = if single { "song" } else { "album" };
        let attempt = async {
            let url = format!(
                "{}/search?term={}&entity={entity}&limit=15",
                self.base,
                dotnet::escape_data_string(&format!("{artist} {release}"))
            );
            let Some(resp) = self.apple_get(&url).await?.filter(HttpAnswer::is_success) else {
                return anyhow::Ok(None);
            };
            let doc: Value = serde_json::from_str(&resp.text())?;
            let Some(results) = try_get_property(&doc, "results")? else {
                return Ok(None);
            };

            let mut clean: Option<String> = None;
            for item in enumerate_array(results)? {
                let art = str_prop(item, "artworkUrl100")?;
                let by = str_prop(item, "artistName")?;
                let collection = RELEASE_SUFFIX
                    .replace(str_prop(item, "collectionName")?.unwrap_or(""), "")
                    .into_owned();
                let (Some(art), Some(by)) = (art, by) else {
                    continue;
                };
                if art.is_empty() || !art.contains("100x100bb") || by.is_empty() {
                    continue;
                }
                if !SongIdentity::same_text(release, artist, &collection, by, Some(&album_titles())).is_same()
                {
                    continue;
                }
                if single
                    && !SongIdentity::same_text(
                        release,
                        artist,
                        str_prop(item, "trackName")?.unwrap_or(""),
                        by,
                        Some(&album_titles()),
                    )
                    .is_same()
                {
                    continue;
                }
                // The explicit and clean releases share a cover almost always; the explicit one
                // first, since that is the one a library usually holds.
                if str_prop(item, "collectionExplicitness")? == Some("cleaned") {
                    clean.get_or_insert_with(|| art.to_string());
                    continue;
                }
                return Ok(Some(art.to_string()));
            }
            Ok(clean)
        };
        match attempt.await {
            Ok(url) => url,
            Err(e) => {
                debug!("iTunes master search failed for {artist} - {release}: {e}");
                None
            }
        }
    }

    /// Issue a search and rank the up-to-5 results by closeness of the artist
    /// match, returning the artworkUrl100 of the best one. Without scoring,
    /// "Drake: Hold On, We're Going Home" would frequently come back as the
    /// karaoke version's cover when it appeared first in the index. With `release`, only a
    /// hit whose album or track is that name, by the expected artist, counts: another artist's
    /// record of a near name is never the cover.
    async fn search_and_score(
        &self,
        term: &str,
        entity: &str,
        expected_artist: &str,
        release: Option<&str>,
    ) -> Option<String> {
        let attempt = async {
            let url = format!(
                "{}/search?term={}&entity={entity}&limit=5",
                self.base,
                dotnet::escape_data_string(term)
            );
            // Unpaced while Deezer answers Octo's searches; while Apple answers them, this search
            // takes its turn with them so the whole of Octo stays within Apple's rate.
            let pace = &self.pace;
            let paced = pace.strict();
            if paced {
                let _ = pace.turn(AppleClass::Cover).await;
            }
            let resp = self.get(&url).await?;
            if paced {
                if matches!(resp.status, StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS) {
                    pace.refused_by_apple();
                } else {
                    pace.answered_by_apple();
                }
            }
            if !resp.is_success() {
                return anyhow::Ok(None);
            }
            let doc: Value = serde_json::from_str(&resp.text())?;
            let Some(results) = try_get_property(&doc, "results")? else {
                return Ok(None);
            };
            if array_length(results)? == 0 {
                return Ok(None);
            }

            let mut best_url = None;
            let mut best_score = i32::MIN;
            for item in enumerate_array(results)? {
                let artist = match try_get_property(item, "artistName")? {
                    Some(a) => get_string(a)?.unwrap_or(""),
                    None => "",
                };
                let artwork = match try_get_property(item, "artworkUrl100")? {
                    Some(aw) => get_string(aw)?,
                    None => None,
                };
                let Some(artwork) = artwork.filter(|a| !a.is_empty()) else {
                    continue;
                };
                if let Some(release) = release {
                    let named = |field: &str| -> anyhow::Result<bool> {
                        Ok(match try_get_property(item, field)? {
                            Some(value) => {
                                get_string(value)?.is_some_and(|name| release_title_fits(release, name))
                            }
                            None => false,
                        })
                    };
                    if !release_artist_fits(&[expected_artist], artist)
                        || !(named("collectionName")? || named("trackName")?)
                    {
                        continue;
                    }
                }

                let score = i32::from(cover_name_score(expected_artist, artist));
                if score > best_score {
                    best_score = score;
                    best_url = Some(artwork.to_string());
                }
            }
            Ok(best_url)
        };
        match attempt.await {
            Ok(url) => url,
            Err(e) => {
                debug!("iTunes search failed for '{term}': {e}");
                None
            }
        }
    }

    /// Singles often have no proper "album" entry on iTunes: the song exists
    /// but only as a track. Re-query with entity=song so we still get cover art.
    async fn search_song_fallback(&self, artist: &str, album_or_title: &str) -> Option<Bytes> {
        let hit = self
            .search_and_score(
                &format!("{artist} {album_or_title}"),
                "song",
                artist,
                Some(album_or_title),
            )
            .await?;
        self.download_hi_res(&hit).await
    }

    async fn download_hi_res(&self, artwork_url100: &str) -> Option<Bytes> {
        // iTunes CDN serves arbitrary sizes by URL substring substitution. 1200x1200, so a
        // cover shown large (a phone's now playing screen, a desktop's full player) is
        // sharp; 600 was visibly soft there. Smaller covers are scaled down by the client.
        let hi_res = artwork_url100.replace("100x100bb", "1200x1200bb");
        match self.get(&hi_res).await {
            Ok(resp) if resp.is_success() => Some(resp.body),
            Ok(_) => None,
            Err(e) => {
                debug!("iTunes artwork download failed for {hi_res}: {e}");
                None
            }
        }
    }
}

#[async_trait]
impl ICoverArtSource for ITunesCoverArtLookup {
    fn name(&self) -> &str {
        "itunes"
    }

    async fn try_fetch(&self, routing: &SoulseekRouting, _background: bool) -> Option<Bytes> {
        let artist = routing.artist.as_deref().unwrap_or("").trim();
        match routing.kind {
            RoutingKind::Album => {
                let album = routing
                    .album
                    .as_deref()
                    .or(routing.title.as_deref())
                    .unwrap_or("")
                    .trim();
                if artist.is_empty() || album.is_empty() {
                    return None;
                }

                // Album-style lookup. For our placeholder "singles" the album is the
                // song title: iTunes may return a song-level hit shaped like an
                // album anyway, or fall through to song-entity fallback below.
                if let Some(hit) = self
                    .search_and_score(&format!("{artist} {album}"), "album", artist, Some(album))
                    .await
                {
                    return self.download_hi_res(&hit).await;
                }
                self.search_song_fallback(artist, album).await
            }
            RoutingKind::Artist => {
                if artist.is_empty() {
                    return None;
                }
                let hit = self.search_and_score(artist, "musicArtist", artist, None).await?;
                self.download_hi_res(&hit).await
            }
            RoutingKind::Song => {
                let title = routing.title.as_deref().unwrap_or("").trim();
                if artist.is_empty() || title.is_empty() {
                    return None;
                }
                let hit = self
                    .search_and_score(&format!("{artist} {title}"), "song", artist, Some(title))
                    .await?;
                self.download_hi_res(&hit).await
            }
        }
    }
}

/// One album of a barcode lookup's answer.
struct Collection {
    id: Option<u64>,
    name: String,
    by: Option<String>,
    art: String,
    clean: bool,
}

fn collections_of(doc: &Value) -> anyhow::Result<Vec<Collection>> {
    let mut collections = Vec::new();
    let Some(results) = try_get_property(doc, "results")? else {
        return Ok(collections);
    };
    for r in enumerate_array(results)? {
        if str_prop(r, "wrapperType")? != Some("collection") {
            continue;
        }
        let name = RELEASE_SUFFIX
            .replace(str_prop(r, "collectionName")?.unwrap_or(""), "")
            .into_owned();
        let by = str_prop(r, "artistName")?.map(str::to_string);
        let art = str_prop(r, "artworkUrl100")?;
        let clean = str_prop(r, "collectionExplicitness")? == Some("cleaned");
        let Some(art) = art.filter(|a| !a.is_empty() && a.contains("100x100bb")) else {
            continue;
        };
        collections.push(Collection {
            id: r.get("collectionId").and_then(Value::as_u64),
            name,
            by,
            art: art.to_string(),
            clean,
        });
    }
    Ok(collections)
}

async fn read_up_to(response: reqwest::Response, limit: usize) -> anyhow::Result<Vec<u8>> {
    let mut buffer = Vec::with_capacity(limit);
    let mut stream = response.bytes_stream();
    while buffer.len() < limit {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk?;
        let take = (limit - buffer.len()).min(chunk.len());
        buffer.extend_from_slice(&chunk[..take]);
    }
    Ok(buffer)
}

fn read_rows(path: &Path) -> anyhow::Result<Vec<CachedMaster>> {
    let text = std::fs::read_to_string(path)?;
    // A file holding a literal null reads as no rows.
    Ok(serde_json::from_str::<Option<Vec<CachedMaster>>>(&text)?.unwrap_or_default())
}

fn write_rows(path: &Path, rows: &[CachedMaster]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, octo_core::json::to_string(rows))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
#[path = "itunes_cover_art_lookup_tests.rs"]
mod tests;
