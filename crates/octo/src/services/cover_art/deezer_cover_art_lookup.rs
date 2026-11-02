//! Cover art from Deezer's public search.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use octo_core::common::cover_names::{
    cover_name_score, release_artist_fits, release_fits, release_title_fits,
};
use octo_core::common::{SongIdentity, dotnet};
use octo_core::json::element::{array_length, enumerate_array, get_string, str_prop, try_get_property};
use octo_core::metadata::accept_language_header;
use octo_core::metadata::deezer_image::deezer_image;
use octo_core::settings::MetadataSettings;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use serde_json::Value;
use tracing::debug;

use super::i_cover_art_source::ICoverArtSource;
use crate::services::framework::HttpAnswer;
use crate::services::metadata::{AskFor, DeezerRateLimitHandler, Reach};

/// Cover art via Deezer's public search API. No key, no auth, very broad
/// catalog including international/non-Western releases: fills the gaps
/// where iTunes' US-skewed catalog whiffs.
///
/// Endpoints used:
///   GET https://api.deezer.com/search?q=Artist Title&limit=10
///   GET https://api.deezer.com/search/album?q=...                       (album mode)
///   GET https://api.deezer.com/search/artist?q=...                      (artist mode)
///
/// Hit shapes contain a nested album with cover_xl (1000x1000), cover_big (500),
/// cover_medium (250). We grab cover_xl for quality, falling back if absent.
pub struct DeezerCoverArtLookup {
    /// Through the shared Deezer client, so API calls go through the shared rate limiter. This
    /// same client also fetches the image bytes from the CDN, which the handler leaves unmetered.
    http: Arc<DeezerRateLimitHandler>,
    /// From the metadata settings, captured once at construction.
    accept_language: Option<String>,
    base: String,
}

impl DeezerCoverArtLookup {
    pub const BASE: &'static str = "https://api.deezer.com";

    pub fn new(http: Arc<DeezerRateLimitHandler>, metadata: &MetadataSettings) -> Self {
        Self::with_base_url(http, metadata, Self::BASE)
    }

    /// A lookup that asks another host than api.deezer.com: a test's mock server.
    pub fn with_base_url(http: Arc<DeezerRateLimitHandler>, metadata: &MetadataSettings, base: &str) -> Self {
        Self {
            http,
            accept_language: accept_language_header::header_value(metadata),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    async fn fetch(&self, routing: &SoulseekRouting, background: bool) -> anyhow::Result<Option<Bytes>> {
        let artist = routing.artist.as_deref().unwrap_or("").trim();
        let album = routing
            .album
            .as_deref()
            .or(routing.title.as_deref())
            .unwrap_or("")
            .trim();
        let cover_url = match routing.kind {
            // An album whose catalog id is known has its cover fetched by that id: exact,
            // and no search to miss it.
            RoutingKind::Album
                if routing
                    .external_album_id
                    .as_deref()
                    .is_some_and(|id| !dotnet::is_blank(id)) =>
            {
                let id = routing.external_album_id.as_deref().unwrap_or("");
                match self.album_cover_by_id(id, background).await? {
                    Some(url) => Some(url),
                    None => self.resolve_album_cover(artist, album, background).await?,
                }
            }
            RoutingKind::Album => self.resolve_album_cover(artist, album, background).await?,
            RoutingKind::Artist => self.resolve_artist_cover(artist, background).await?,
            RoutingKind::Song => {
                let title = routing.title.as_deref().unwrap_or("").trim();
                self.resolve_track_cover(artist, title, background).await?
            }
        };
        let Some(cover_url) = cover_url.filter(|u| !u.is_empty()) else {
            return Ok(None);
        };

        let answer = self.send(&cover_url, background).await?;
        Ok(answer.is_success().then_some(answer.body))
    }

    async fn resolve_track_cover(
        &self,
        artist: &str,
        title: &str,
        background: bool,
    ) -> anyhow::Result<Option<String>> {
        // While Apple answers Deezer's searches, the iTunes source next in the chain asks Apple
        // this same question: asking it here too would ask twice.
        if artist.is_empty() || title.is_empty() || self.http.searches_stand_in() {
            return Ok(None);
        }
        // A plain query: the catalog stopped answering field-qualified ones
        // (`artist:"X" track:"Y"`), which left every lookup here empty and handed covers to
        // a smaller source. The picking below does the matching instead.
        let q = format!("{artist} {title}");
        let url = format!(
            "{}/search?q={}&limit=10",
            self.base,
            dotnet::escape_data_string(&q)
        );
        let Some(doc) = self.get_json(&url, background).await? else {
            return Ok(None);
        };
        let Some(data) = non_empty_data(&doc)? else {
            return Ok(None);
        };
        pick_best_album_cover(data, artist, title)
    }

    async fn resolve_album_cover(
        &self,
        artist: &str,
        album: &str,
        background: bool,
    ) -> anyhow::Result<Option<String>> {
        if artist.is_empty() || album.is_empty() || self.http.searches_stand_in() {
            return Ok(None);
        }
        // Plain, for the same reason as a track's.
        let q = format!("{artist} {album}");
        let url = format!(
            "{}/search/album?q={}&limit=10",
            self.base,
            dotnet::escape_data_string(&q)
        );
        let Some(doc) = self.get_json(&url, background).await? else {
            return Ok(None);
        };
        match non_empty_data(&doc)? {
            // Fallback to a track-based search for "albums" that are really singles.
            None => Box::pin(self.resolve_track_cover(artist, album, background)).await,
            Some(data) => Ok(pick_best_direct_cover(data, artist, album)?),
        }
    }

    /// An album's own cover by its catalog id, or None when the catalog has none.
    async fn album_cover_by_id(&self, album_id: &str, background: bool) -> anyhow::Result<Option<String>> {
        let url = format!(
            "{}/album/{}",
            self.base,
            dotnet::escape_data_string(album_id.trim())
        );
        let Some(doc) = self.get_json(&url, background).await? else {
            return Ok(None);
        };
        Ok(deezer_image(&doc, &["cover_xl", "cover_big", "cover_medium"])?.map(str::to_string))
    }

    async fn resolve_artist_cover(&self, artist: &str, background: bool) -> anyhow::Result<Option<String>> {
        // Deezer's artist search answers wherever Deezer can be reached, its closed countries too.
        let unreachable = self
            .http
            .reach()
            .is_some_and(|reach| matches!(reach.reach(), Reach::Unreachable { .. }));
        if artist.is_empty() || unreachable {
            return Ok(None);
        }
        let url = format!(
            "{}/search/artist?q={}&limit=5",
            self.base,
            dotnet::escape_data_string(artist)
        );
        let Some(doc) = self.get_json(&url, background).await? else {
            return Ok(None);
        };
        let Some(data) = non_empty_data(&doc)? else {
            return Ok(None);
        };
        // Artist endpoint returns picture_xl directly on each item.
        let mut best = None;
        let mut best_score = i32::MIN;
        for item in enumerate_array(data)? {
            let name = match try_get_property(item, "name")? {
                Some(n) => get_string(n)?.unwrap_or(""),
                None => "",
            };
            let pic = deezer_image(item, &["picture_xl", "picture_big", "picture"])?;
            let Some(pic) = pic.filter(|p| !p.is_empty()) else {
                continue;
            };
            let score = i32::from(cover_name_score(artist, name));
            if score > best_score {
                best_score = score;
                best = Some(pic.to_string());
            }
        }
        Ok(best)
    }

    async fn get_json(&self, url: &str, background: bool) -> anyhow::Result<Option<Value>> {
        let answer = self.send(url, background).await?;
        if !answer.is_success() {
            return Ok(None);
        }
        Ok(serde_json::from_str(&answer.text()).ok())
    }

    /// GETs through the shared Deezer client, marking the request for the background
    /// rate-limit lane when this call is a prewarm. Only api.deezer.com is metered by
    /// [`DeezerRateLimitHandler`], so marking a CDN image request is harmless but pointless;
    /// done uniformly for simplicity.
    async fn send(&self, url: &str, background: bool) -> anyhow::Result<HttpAnswer> {
        self.http
            .get_for(url, AskFor::cover(background), self.accept_language.as_deref())
            .await
    }
}

#[async_trait]
impl ICoverArtSource for DeezerCoverArtLookup {
    fn name(&self) -> &str {
        "deezer"
    }

    async fn try_fetch(&self, routing: &SoulseekRouting, background: bool) -> Option<Bytes> {
        match self.fetch(routing, background).await {
            Ok(bytes) => bytes,
            Err(e) => {
                debug!(
                    "deezer lookup failed for {:?} {}/{}/{}: {e}",
                    routing.kind,
                    routing.artist.as_deref().unwrap_or(""),
                    routing.title.as_deref().unwrap_or(""),
                    routing.album.as_deref().unwrap_or("")
                );
                None
            }
        }
    }
}

/// `root.data` when the root has it and it is not empty (`GetArrayLength` throws when it is
/// not an array).
fn non_empty_data(doc: &Value) -> anyhow::Result<Option<&Value>> {
    match try_get_property(doc, "data")? {
        Some(data) if array_length(data)? > 0 => Ok(Some(data)),
        _ => Ok(None),
    }
}

/// The name of `item.artist`, or "" when it has none.
fn artist_name(item: &Value) -> anyhow::Result<&str> {
    if let Some(artist) = try_get_property(item, "artist")?
        && let Some(name) = try_get_property(artist, "name")?
    {
        return Ok(get_string(name)?.unwrap_or(""));
    }
    Ok("")
}

/// Pick best track-result cover by artist scoring; reads from `album.cover_xl`.
fn pick_best_album_cover(
    data: &Value,
    expected_artist: &str,
    expected_title: &str,
) -> anyhow::Result<Option<String>> {
    let mut best = None;
    let mut best_score = i32::MIN;
    for item in enumerate_array(data)? {
        let artist = artist_name(item)?;
        let album = try_get_property(item, "album")?;
        let cover = match album {
            Some(album) => deezer_image(album, &["cover_xl", "cover_big", "cover_medium"])?,
            None => None,
        };
        let Some(cover) = cover.filter(|c| !c.is_empty()) else {
            continue;
        };
        // The asked-for title can be the track's or, for a single, its album's.
        let album_title = match album {
            Some(a) => str_prop(a, "title")?,
            None => None,
        };
        // Only the asked-for track's or release's own art, by its own artist: never another
        // artist's record of a near name.
        let track_title = str_prop(item, "title")?;
        let fits = release_artist_fits(&[expected_artist], artist)
            && (track_title.is_some_and(|t| release_title_fits(expected_title, t))
                || album_title.is_some_and(|t| release_title_fits(expected_title, t)));
        if !fits {
            continue;
        }
        let score = i32::from(cover_name_score(expected_artist, artist))
            + title_bonus(expected_title, track_title).max(title_bonus(expected_title, album_title));
        if score > best_score {
            best_score = score;
            best = Some(cover.to_string());
        }
    }
    Ok(best)
}

/// Pick best album-result cover by artist scoring; reads from `cover_xl` directly on the result.
fn pick_best_direct_cover(
    data: &Value,
    expected_artist: &str,
    expected_title: &str,
) -> anyhow::Result<Option<String>> {
    let mut best = None;
    let mut best_score = i32::MIN;
    for item in enumerate_array(data)? {
        let artist = artist_name(item)?;
        let cover = deezer_image(item, &["cover_xl", "cover_big", "cover_medium"])?;
        let Some(cover) = cover.filter(|c| !c.is_empty()) else {
            continue;
        };
        let title = str_prop(item, "title")?;
        if !title.is_some_and(|t| release_fits(expected_title, &[expected_artist], t, artist)) {
            continue;
        }
        let score = i32::from(cover_name_score(expected_artist, artist)) + title_bonus(expected_title, title);
        if score > best_score {
            best_score = score;
            best = Some(cover.to_string());
        }
    }
    Ok(best)
}

/// With plain queries a result can be another record by the same artist: the one
/// with the asked-for title wins.
fn title_bonus(expected: &str, actual: Option<&str>) -> i32 {
    match actual {
        Some(actual) if !actual.is_empty() && SongIdentity::key(actual) == SongIdentity::key(expected) => 50,
        _ => 0,
    }
}

#[cfg(test)]
#[path = "deezer_cover_art_lookup_tests.rs"]
mod tests;
