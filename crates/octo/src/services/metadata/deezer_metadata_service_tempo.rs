//! A song's published tempo from the catalog, by its ISRC. Every track record the catalog sends
//! leaves its tempo in the cache under the track's ISRC, so a downloaded song's tempo is
//! usually known already; otherwise one lookup on the background lane finds it.

use std::sync::Arc;

use async_trait::async_trait;
use octo_core::common::SongIdentity;
use octo_core::common::dotnet::escape_data_string;
use octo_core::json::element::{ElementResult, get_double, try_get_property};
use serde_json::Value;

use super::{Cached, DeezerMetadataService, NEGATIVE_TTL, POSITIVE_TTL};
use crate::services::sonic::IPublishedTempo;

/// The cache key of the tempo of the track with this (normalized) ISRC.
pub(super) fn tempo_key(isrc: &str) -> String {
    format!("bpm|{isrc}")
}

/// A track record's tempo: its `bpm` when above 0. The catalog sends 0 for a track it has
/// no tempo for.
pub(super) fn tempo_of(track: &Value) -> ElementResult<Option<f64>> {
    Ok(match try_get_property(track, "bpm")? {
        Some(bpm @ Value::Number(_)) => Some(get_double(bpm)?).filter(|bpm| bpm.is_finite() && *bpm > 0.0),
        _ => None,
    })
}

/// What a tempo lookup found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TempoLookup {
    /// The catalog's tempo, or `None` when it has the track without one, or no such track.
    Known(Option<f64>),
    /// The catalog did not answer this time.
    Busy,
}

impl DeezerMetadataService {
    /// The tempo of the track with this ISRC, from the cache when any track record carried it,
    /// else from the track's record on the background lane.
    pub async fn tempo_by_isrc(&self, isrc: &str) -> TempoLookup {
        let Some(code) = SongIdentity::normalize_isrc(isrc) else {
            return TempoLookup::Known(None);
        };
        let generation = self.generation();
        let key = tempo_key(&code);
        if let Some(Cached::Tempo(bpm)) = self.get_cached(&key) {
            return TempoLookup::Known(bpm);
        }
        let r = self
            .get_json(
                &format!("{}/track/isrc:{}", self.base(), escape_data_string(&code)),
                true,
            )
            .await;
        if r.transient {
            return TempoLookup::Busy;
        }
        let bpm = match r.doc.as_ref() {
            Some(root @ Value::Object(_)) => tempo_of(root).unwrap_or(None),
            _ => None,
        };
        let ttl = if bpm.is_some() { POSITIVE_TTL } else { NEGATIVE_TTL };
        self.put(generation, key, Cached::Tempo(bpm), ttl);
        TempoLookup::Known(bpm)
    }
}

/// The published tempo of songs, from Deezer.
pub struct DeezerPublishedTempo {
    deezer: Arc<DeezerMetadataService>,
}

impl DeezerPublishedTempo {
    pub fn new(deezer: Arc<DeezerMetadataService>) -> Self {
        Self { deezer }
    }
}

#[async_trait]
impl IPublishedTempo for DeezerPublishedTempo {
    async fn bpm(&self, isrcs: &[String]) -> Option<f64> {
        for isrc in SongIdentity::isrcs(isrcs) {
            match self.deezer.tempo_by_isrc(&isrc).await {
                TempoLookup::Known(Some(bpm)) => return Some(bpm),
                TempoLookup::Known(None) => {}
                TempoLookup::Busy => return None,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use octo_core::settings::{AppSettings, SettingsStore};
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::metadata::{DeezerRateLimitHandler, DeezerRateLimiter};

    fn service(server: &MockServer) -> Arc<DeezerMetadataService> {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
        let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
        Arc::new(DeezerMetadataService::with_base_url(http, settings, server.uri()))
    }

    async fn answer(server: &MockServer, at: &str, body: &str, times: u64) {
        Mock::given(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(times)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_tempo_is_read_by_isrc_once_and_then_kept() {
        let server = MockServer::start().await;
        answer(&server, "/track/isrc:USUM71702791", r#"{"id":9,"bpm":128.4}"#, 1).await;
        let deezer = service(&server);
        assert_eq!(
            deezer.tempo_by_isrc("usum71702791").await,
            TempoLookup::Known(Some(128.4))
        );
        assert_eq!(
            deezer.tempo_by_isrc("US-UM7-17-02791").await,
            TempoLookup::Known(Some(128.4))
        );
    }

    #[tokio::test]
    async fn a_tempo_of_0_is_none_and_kept_as_none() {
        let server = MockServer::start().await;
        answer(&server, "/track/isrc:USUM71702791", r#"{"id":9,"bpm":0}"#, 1).await;
        let tempo = DeezerPublishedTempo::new(service(&server));
        assert_eq!(tempo.bpm(&["USUM71702791".to_string()]).await, None);
        assert_eq!(tempo.bpm(&["USUM71702791".to_string()]).await, None);
    }

    #[tokio::test]
    async fn the_next_isrc_is_tried_when_the_first_has_no_tempo() {
        let server = MockServer::start().await;
        answer(
            &server,
            "/track/isrc:USUM71702791",
            r#"{"error":{"code":800}}"#,
            1,
        )
        .await;
        answer(&server, "/track/isrc:GBAYE0601498", r#"{"id":3,"bpm":96}"#, 1).await;
        let tempo = DeezerPublishedTempo::new(service(&server));
        let isrcs = ["USUM71702791".to_string(), "GBAYE0601498".to_string()];
        assert_eq!(tempo.bpm(&isrcs).await, Some(96.0));
    }

    #[tokio::test]
    async fn a_throttled_catalog_is_no_tempo_and_not_kept() {
        let server = MockServer::start().await;
        answer(&server, "/track/isrc:USUM71702791", r#"{"error":{"code":4}}"#, 2).await;
        let deezer = service(&server);
        assert_eq!(deezer.tempo_by_isrc("USUM71702791").await, TempoLookup::Busy);
        assert_eq!(deezer.tempo_by_isrc("USUM71702791").await, TempoLookup::Busy);
    }

    #[tokio::test]
    async fn no_isrc_asks_nothing() {
        let server = MockServer::start().await;
        let tempo = DeezerPublishedTempo::new(service(&server));
        assert_eq!(tempo.bpm(&[]).await, None);
        assert_eq!(tempo.bpm(&["not a code".to_string()]).await, None);
        assert!(server.received_requests().await.unwrap_or_default().is_empty());
    }

    /// The download tagger's lookup already reads the track's record; its tempo is kept under
    /// the track's ISRC, so a landed song's tempo costs no request of its own.
    #[tokio::test]
    async fn the_taggers_track_lookup_leaves_the_tempo_for_later() {
        let server = MockServer::start().await;
        answer(
            &server,
            "/search",
            r#"{"data":[{"id":9,"title":"Teardrop","duration":330,"artist":{"name":"Massive Attack"},
                "album":{"id":4,"title":"Mezzanine"}}]}"#,
            1,
        )
        .await;
        answer(
            &server,
            "/album/4",
            r#"{"id":4,"title":"Mezzanine","nb_tracks":11}"#,
            1,
        )
        .await;
        answer(
            &server,
            "/track/9",
            r#"{"id":9,"isrc":"GBAAA9800001","bpm":77.1}"#,
            1,
        )
        .await;
        answer(&server, "/track/isrc:GBAAA9800001", r#"{"id":9,"bpm":1}"#, 0).await;
        let deezer = service(&server);
        let meta = deezer
            .enrich_track_full("Massive Attack", "Teardrop")
            .await
            .expect("a track");
        assert_eq!(meta.bpm, Some(77.1));
        let tempo = DeezerPublishedTempo::new(deezer);
        assert_eq!(tempo.bpm(&["GBAAA9800001".to_string()]).await, Some(77.1));
    }
}
