//! Deezer's popularity lists, an artist's most played songs and the chart of the moment. Both
//! keyless, and each track comes with its album, cover and length, so a row needs no further
//! lookup to be shown. The chart is Popular right now's backup, for when Apple's cannot be read.

use std::sync::Arc;
use std::time::Duration;

use octo_core::charts::top_songs::{CatalogTrack, explicit_from_catalog};
use octo_core::common::dotnet::{escape_data_string, is_blank};
use octo_core::metadata::deezer_image::deezer_image;
use serde_json::Value;

use super::{Cached, DeezerMetadataService, NEGATIVE_TTL};

/// The most Deezer lists for an artist, and for its chart.
pub const CHART_LIMIT: i32 = 100;

/// An artist's top songs move slowly; the chart of the moment moves daily.
const ARTIST_TOP_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const CHART_TTL: Duration = Duration::from_secs(60 * 60);

impl DeezerMetadataService {
    /// The catalog artist's most played songs, most played first. None when Deezer did not
    /// answer this time, so a caller can try another source and must cache nothing; empty when
    /// it answered that the artist has none.
    pub async fn get_artist_top(&self, deezer_artist_id: &str, limit: i32) -> Option<Arc<Vec<CatalogTrack>>> {
        if is_blank(deezer_artist_id) || limit <= 0 {
            return Some(Arc::new(Vec::new()));
        }
        let take = limit.min(CHART_LIMIT);
        let url = format!(
            "{}/artist/{}/top?limit={take}",
            self.base(),
            escape_data_string(deezer_artist_id)
        );
        self.chart_list(format!("top|{deezer_artist_id}|{take}"), url, ARTIST_TOP_TTL)
            .await
    }

    /// Deezer's chart of the moment, most played first. Deezer picks the country from where the
    /// request comes from. None when Deezer did not answer this time.
    pub async fn get_chart(&self, limit: i32) -> Option<Arc<Vec<CatalogTrack>>> {
        let take = limit.clamp(1, CHART_LIMIT);
        let url = format!("{}/chart/0/tracks?limit={take}", self.base());
        self.chart_list(format!("chart|{take}"), url, CHART_TTL).await
    }

    async fn chart_list(&self, key: String, url: String, ttl: Duration) -> Option<Arc<Vec<CatalogTrack>>> {
        let generation = self.generation();
        if let Some(Cached::Chart(cached)) = self.get_cached(&key) {
            return Some(cached);
        }
        let this = self.clone();
        let fetch_key = key.clone();
        self.inner
            .in_flight_charts
            .run(&super::in_generation(generation, &key), async move {
                let r = this.get_json(&url, false).await;
                if r.transient {
                    return None;
                }
                let tracks: Vec<CatalogTrack> = r
                    .doc
                    .as_ref()
                    .and_then(|doc| doc.get("data"))
                    .and_then(Value::as_array)
                    .map(|data| data.iter().filter_map(read_chart_track).collect())
                    .unwrap_or_default();
                let ttl = if tracks.is_empty() { NEGATIVE_TTL } else { ttl };
                let tracks = Arc::new(tracks);
                this.put(generation, fetch_key, Cached::Chart(tracks.clone()), ttl);
                Some(tracks)
            })
            .await
    }
}

/// A track as the popularity lists give it. Its artist and album ride inside it.
pub(super) fn read_chart_track(t: &Value) -> Option<CatalogTrack> {
    let id = t.get("id").and_then(Value::as_i64)?.to_string();
    let title = text(t, "title").filter(|s| !is_blank(s))?;
    if t.get("readable") == Some(&Value::Bool(false)) {
        return None;
    }
    let artist_node = t.get("artist").filter(|a| a.is_object());
    let artist = artist_node
        .and_then(|a| text(a, "name"))
        .filter(|s| !is_blank(s))?;
    let artist_id = artist_node
        .and_then(|a| a.get("id"))
        .and_then(Value::as_i64)
        .map(|n| n.to_string());
    let album_node = t.get("album").filter(|a| a.is_object());
    let album = album_node.and_then(|a| text(a, "title"));
    let album_id = album_node
        .and_then(|a| a.get("id"))
        .and_then(Value::as_i64)
        .map(|n| n.to_string());
    let cover = album_node.and_then(|a| {
        deezer_image(a, &["cover_xl", "cover_big", "cover_medium"])
            .ok()
            .flatten()
            .map(str::to_string)
    });
    let explicit = t.get("explicit_lyrics").and_then(Value::as_bool);
    let content = t
        .get("explicit_content_lyrics")
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok());
    Some(CatalogTrack {
        catalog_id: id,
        title,
        artist,
        artists: Vec::new(),
        artist_id,
        album,
        album_id,
        cover_url: cover,
        duration: t
            .get("duration")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .filter(|n| *n > 0),
        explicit,
        explicit_content: explicit_from_catalog(content, explicit),
    })
}

fn text(v: &Value, name: &str) -> Option<String> {
    v.get(name).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
#[path = "deezer_charts_tests.rs"]
mod tests;
