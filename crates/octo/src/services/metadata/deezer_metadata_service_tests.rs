//! Tests of the Deezer metadata service, including its query variant lookups. The release type
//! cases are tested in `octo_core::metadata::deezer_metadata_service`.
//!
//! A mock server answers by URL substring, and the service asks it in place of api.deezer.com.
//! The rate limiter never meters these requests (the handler meters api.deezer.com only).

use octo_core::settings::{AppSettings, ExplicitFilter, MetadataSettings, SubsonicSettings};
use tracing::Level;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::test_support::{LogCapture, Matching, Routes, received, refused_address};

struct Harness {
    service: DeezerMetadataService,
    routes: Routes,
    server: MockServer,
}

impl Harness {
    /// Builds a service whose HTTP layer answers from a url-substring to body map.
    /// Any url with no match returns 404, which exercises the best-effort paths.
    async fn new(routes: Vec<(&str, &str)>) -> Harness {
        Self::with_language(Routes::single(routes), "en").await
    }

    /// Like [`Harness::new`], but each route serves its bodies in order and repeats the last
    /// one once exhausted, so a test can make a call fail and then succeed. Routes are matched
    /// by url SUBSTRING, so "/album/1/tracks" must be registered before "/album/1" or the
    /// shorter needle swallows both.
    async fn sequenced(routes: Vec<(&str, Vec<&str>)>) -> Harness {
        Self::with_language(Routes::new(routes, Matching::IgnoreCase), "en").await
    }

    async fn with_language(routes: Routes, language: &str) -> Harness {
        let server = routes.serve().await;
        let settings = SettingsStore::for_tests(AppSettings {
            metadata: MetadataSettings {
                language: language.to_string(),
                ..Default::default()
            },
            ..Default::default()
        });
        let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
        let service = DeezerMetadataService::with_base_url(http, Arc::new(settings), server.uri());
        Harness {
            service,
            routes,
            server,
        }
    }

    fn calls(&self, needle: &str) -> usize {
        self.routes.calls(needle)
    }

    /// The query string of every request, unescaped.
    async fn queries(&self) -> Vec<String> {
        received(&self.server)
            .await
            .iter()
            .map(|r| dotnet::unescape_data_string(r.url.query().unwrap_or("")))
            .collect()
    }

    async fn paths(&self) -> Vec<String> {
        received(&self.server)
            .await
            .iter()
            .map(|r| r.url.path().to_string())
            .collect()
    }
}

/// Deezer reports throttling as HTTP 200 with this body, which is the whole
/// reason a parsed document cannot be treated as a successful call.
const QUOTA_ENVELOPE: &str = r#"{"error":{"type":"Exception","message":"Quota limit exceeded","code":4}}"#;

/// Deezer's only "this really does not exist" answer, also HTTP 200.
const NO_DATA_ENVELOPE: &str = r#"{"error":{"type":"DataException","message":"no data","code":800}}"#;

const ALBUM_DETAIL_JSON: &str = r#"{"id":711108,"title":"Canciones Prohibidas","nb_tracks":10,"label":"WM Spain",
   "release_date":"1998-04-30","cover_xl":"https://cdn/xl.jpg",
   "artist":{"name":"Extremoduro"},"genres":{"data":[{"name":"Pop"}]}}"#;

fn tracks_json(count: usize) -> String {
    let tracks: Vec<String> = (1..=count)
        .map(|i| {
            format!(
                r#"{{"title":"Track {i}","duration":200,"track_position":{i},
                     "disk_number":1,"artist":{{"name":"Extremoduro"}}}}"#
            )
        })
        .collect();
    format!(r#"{{"total":{count},"data":[{}]}}"#, tracks.join(","))
}

#[tokio::test]
async fn search_albums_async_maps_fields_and_drops_singles() {
    // Arrange: one real album, one EP, and a one-track "single" that must be dropped.
    let json = r#"{"data":[
        {"id":14880659,"title":"In Rainbows","record_type":"album","nb_tracks":10,
         "cover_xl":"https://cdn/xl.jpg","artist":{"name":"Radiohead"}},
        {"id":14880561,"title":"In Rainbows (Disk 2)","record_type":"ep","nb_tracks":8,
         "cover_xl":"https://cdn/ep.jpg","artist":{"name":"Radiohead"}},
        {"id":999,"title":"Nude","record_type":"single","nb_tracks":1,
         "cover_xl":"https://cdn/s.jpg","artist":{"name":"Radiohead"}}
    ]}"#;
    let h = Harness::new(vec![("/search/album", json)]).await;

    // Act
    let hits = h.service.search_albums("In Rainbows", 10, false).await;

    // Assert
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].deezer_id, "14880659");
    assert_eq!(hits[0].title, "In Rainbows");
    assert_eq!(hits[0].artist, "Radiohead");
    assert_eq!(hits[0].cover_url.as_deref(), Some("https://cdn/xl.jpg"));
    assert_eq!(hits[0].track_count, 10);
    assert!(!hits.iter().any(|h| h.title == "Nude"));
}

#[tokio::test]
async fn search_albums_async_keeps_multi_track_single() {
    // Only a 1-2 track "single" is noise; a longer one is a real release.
    let json = r#"{"data":[{"id":5,"title":"Long Single","record_type":"single",
        "nb_tracks":6,"artist":{"name":"X"}}]}"#;
    let h = Harness::new(vec![("/search/album", json)]).await;

    let hits = h.service.search_albums("q", 10, false).await;

    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn get_album_detail_async_orders_by_disc_then_track_position() {
    // Arrange: deliberately out of order, spanning two discs.
    let album = r#"{"id":1,"title":"Test Album","cover_xl":"https://cdn/a.jpg",
        "release_date":"1997-05-21","label":"Label X",
        "artist":{"name":"Test Artist"},"genres":{"data":[{"name":"Rock"}]}}"#;
    let tracks = r#"{"total":4,"data":[
        {"title":"D2T1","duration":100,"track_position":1,"disk_number":2,"artist":{"name":"Test Artist"}},
        {"title":"D1T2","duration":200,"track_position":2,"disk_number":1,"isrc":"ABC","artist":{"name":"Test Artist"}},
        {"title":"D1T1","duration":300,"track_position":1,"disk_number":1,"artist":{"name":"Test Artist"}},
        {"title":"D2T2","duration":150,"track_position":2,"disk_number":2,"artist":{"name":"Test Artist"}}
    ]}"#;
    let h = Harness::new(vec![("/album/1/tracks", tracks), ("/album/1", album)]).await;

    // Act
    let detail = h.service.get_album_detail("1").await.expect("a detail");

    // Assert
    assert_eq!(detail.title, "Test Album");
    assert_eq!(detail.artist, "Test Artist");
    assert_eq!(detail.year, Some(1997));
    assert_eq!(detail.genre.as_deref(), Some("Rock"));
    assert_eq!(detail.label.as_deref(), Some("Label X"));
    assert_eq!(
        detail.tracks.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        ["D1T1", "D1T2", "D2T1", "D2T2"]
    );
    assert_eq!(detail.tracks[1].isrc.as_deref(), Some("ABC"));
    assert_eq!(detail.tracks[0].duration, Some(300));
}

#[tokio::test]
async fn get_album_detail_async_malformed_payload_returns_null_without_throwing() {
    let h = Harness::new(vec![("/album/", "{ this is not json")]).await;

    let detail = h.service.get_album_detail("1").await;

    assert_eq!(detail, None);
}

#[tokio::test]
async fn get_album_detail_async_unreachable_api_returns_null() {
    // No routes registered, so every request 404s.
    let h = Harness::new(vec![]).await;

    assert_eq!(h.service.get_album_detail("1").await, None);
}

#[tokio::test]
async fn find_album_id_async_returns_first_match() {
    let json = r#"{"data":[{"id":14880659,"title":"In Rainbows"}]}"#;
    let h = Harness::new(vec![("/search/album", json)]).await;

    let id = h.service.find_album_id("Radiohead", "In Rainbows").await;

    assert_eq!(id.as_deref(), Some("14880659"));
}

#[tokio::test]
async fn find_album_id_async_no_album_name_returns_null_without_calling_api() {
    let h = Harness::new(vec![]).await;

    assert_eq!(h.service.find_album_id("Radiohead", "").await, None);
    assert!(received(&h.server).await.is_empty());
}

#[tokio::test]
async fn search_albums_async_empty_query_or_zero_limit_returns_empty() {
    let h = Harness::new(vec![]).await;

    assert!(h.service.search_albums("", 10, false).await.is_empty());
    assert!(h.service.search_albums("q", 0, false).await.is_empty());
}

// ---- Quota-poisoning regression tests (issue #8) ------------------------
// Deezer answers HTTP 200 even when refusing a call, so a parsed document is not
// a successful call. Caching one of those refusals is what made an album report
// songCount 0 permanently.

/// The exact reported failure: the album call succeeds and the TRACKLIST call is
/// throttled, which used to build a valid AlbumDetail carrying real title/year/genre
/// with an empty tracklist and cache it forever.
#[tokio::test]
async fn get_album_detail_async_tracklist_throttled_not_cached_and_recovers_on_retry() {
    let tracks = tracks_json(10);
    let h = Harness::sequenced(vec![
        // Longest needle first: "/album/711108" is a prefix of the tracks url.
        ("/album/711108/tracks", vec![QUOTA_ENVELOPE, tracks.as_str()]),
        ("/album/711108", vec![ALBUM_DETAIL_JSON]),
    ])
    .await;

    let first = h.service.get_album_detail("711108").await;
    assert_eq!(first, None);

    let second = h.service.get_album_detail("711108").await.expect("recovered");
    assert_eq!(second.tracks.len(), 10);
    assert_eq!(second.title, "Canciones Prohibidas");
    assert_eq!(second.year, Some(1998));

    // Proves it retried rather than serving a cached failure.
    assert_eq!(h.calls("/album/711108/tracks"), 2);
}

/// The first call Deezer answers after one it did not (a quota envelope) runs the hook that wakes
/// what waits for Deezer, once; answers in a row run nothing more.
#[tokio::test]
async fn the_first_answer_after_a_call_deezer_did_not_answer_wakes_what_waits_for_it() {
    let tracks = tracks_json(10);
    let h = Harness::sequenced(vec![
        ("/album/711108/tracks", vec![QUOTA_ENVELOPE, tracks.as_str()]),
        ("/album/711108", vec![ALBUM_DETAIL_JSON]),
    ])
    .await;
    let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = Arc::clone(&woken);
    h.service.on_answered_again(Arc::new(move || {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    assert_eq!(h.service.answering(), Some(0));
    assert_eq!(h.service.get_album_detail("711108").await, None);
    assert_eq!(h.service.answering(), None, "its last call went unanswered");
    assert!(h.service.get_album_detail("711108").await.is_some());
    assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(h.service.answering(), Some(1), "answered again once");
    h.service.clear_caches();
    assert!(h.service.get_album_detail("711108").await.is_some());
    assert_eq!(
        woken.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no call failed between"
    );
    assert_eq!(h.service.answering(), Some(1));
}

/// The other poisoning branch: the album call itself is throttled.
#[tokio::test]
async fn get_album_detail_async_album_call_throttled_not_cached_and_recovers_on_retry() {
    let tracks = tracks_json(10);
    let h = Harness::sequenced(vec![
        ("/album/711108/tracks", vec![tracks.as_str()]),
        ("/album/711108", vec![QUOTA_ENVELOPE, ALBUM_DETAIL_JSON]),
    ])
    .await;

    assert_eq!(h.service.get_album_detail("711108").await, None);

    let second = h.service.get_album_detail("711108").await.expect("recovered");
    assert_eq!(second.tracks.len(), 10);
}

/// Int() returns int?, and a lifted "nbTracks > 0" is FALSE when nb_tracks is absent.
/// Testing that alone would let an empty tracklist through and cache it, leaving the
/// reported bug fixed only for albums that happen to report a track count.
#[tokio::test]
async fn get_album_detail_async_empty_tracklist_and_no_track_count_not_cached() {
    let no_nb_tracks = r#"{"id":711108,"title":"Canciones Prohibidas","artist":{"name":"Extremoduro"}}"#;
    let tracks = tracks_json(10);
    let h = Harness::sequenced(vec![
        ("/album/711108/tracks", vec![r#"{"data":[]}"#, tracks.as_str()]),
        ("/album/711108", vec![no_nb_tracks, ALBUM_DETAIL_JSON]),
    ])
    .await;

    assert_eq!(h.service.get_album_detail("711108").await, None);

    let second = h.service.get_album_detail("711108").await.expect("recovered");
    assert_eq!(second.tracks.len(), 10);
}

/// The case the guard must NOT break: Deezer says the album has zero tracks and
/// returns zero tracks. That is an answer, so it is cached rather than refetched
/// on every getAlbum forever.
#[tokio::test]
async fn get_album_detail_async_genuinely_empty_album_is_answered_and_cached() {
    let zero_tracks = r#"{"id":42,"title":"Empty","nb_tracks":0,"artist":{"name":"Nobody"}}"#;
    let h = Harness::sequenced(vec![
        ("/album/42/tracks", vec![r#"{"total":0,"data":[]}"#]),
        ("/album/42", vec![zero_tracks]),
    ])
    .await;

    let first = h.service.get_album_detail("42").await.expect("answered");
    assert!(first.tracks.is_empty());

    h.service.get_album_detail("42").await;
    assert_eq!(h.calls("/album/42/tracks"), 1);
}

/// A definitive "no data" is cacheable, unlike a throttle.
#[tokio::test]
async fn get_album_detail_async_definitive_no_data_is_cached() {
    let tracks = tracks_json(3);
    let h = Harness::sequenced(vec![
        ("/album/999/tracks", vec![tracks.as_str()]),
        ("/album/999", vec![NO_DATA_ENVELOPE, ALBUM_DETAIL_JSON]),
    ])
    .await;

    assert_eq!(h.service.get_album_detail("999").await, None);
    assert_eq!(h.service.get_album_detail("999").await, None);

    // One call only: a definitive answer is allowed to stick.
    assert_eq!(h.calls("/album/999"), 1);
    assert_eq!(h.calls("/album/999/tracks"), 1);
}

/// getAlbum lists the songs filed under an album when Deezer answered that it has
/// no such album or no tracks for it, never when it failed to answer. So the lookup says
/// which, and a cached "no such album" still says so.
#[tokio::test]
async fn look_up_album_detail_async_says_why_there_is_no_detail() {
    let tracks = tracks_json(10);
    let h = Harness::sequenced(vec![
        (
            "/album/711108/tracks",
            vec![QUOTA_ENVELOPE, r#"{"data":[]}"#, tracks.as_str()],
        ),
        ("/album/711108", vec![ALBUM_DETAIL_JSON]),
        // The tracklist is asked beside the album, and dropped when there is no album.
        ("/album/999/tracks", vec![r#"{"data":[]}"#]),
        ("/album/999", vec![NO_DATA_ENVELOPE]),
    ])
    .await;

    assert_eq!(
        h.service.look_up_album_detail("711108").await.answer,
        AlbumAnswer::Unavailable
    );
    assert_eq!(
        h.service.look_up_album_detail("711108").await.answer,
        AlbumAnswer::NoTracks
    );
    let found = h.service.look_up_album_detail("711108").await;
    assert_eq!(found.answer, AlbumAnswer::Found);
    assert_eq!(found.detail.expect("a detail").tracks.len(), 10);

    assert_eq!(
        h.service.look_up_album_detail("999").await.answer,
        AlbumAnswer::NoSuchAlbum
    );
    assert_eq!(
        h.service.look_up_album_detail("999").await.answer,
        AlbumAnswer::NoSuchAlbum
    );
    assert_eq!(h.calls("/album/999"), 1);
}

/// A throttled album search used to cache an empty list, so external albums silently
/// stopped appearing in search3 for the life of the process.
#[tokio::test]
async fn search_albums_async_throttled_not_cached_and_recovers_on_retry() {
    let results = r#"{"data":[{"id":711108,"title":"Canciones Prohibidas","record_type":"album",
       "nb_tracks":10,"cover_xl":"https://cdn/xl.jpg","artist":{"name":"Extremoduro"}}]}"#;
    let h = Harness::sequenced(vec![("/search/album", vec![QUOTA_ENVELOPE, results])]).await;

    assert!(
        h.service
            .search_albums("extremoduro golfa", 10, false)
            .await
            .is_empty()
    );

    let second = h.service.search_albums("extremoduro golfa", 10, false).await;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].deezer_id, "711108");
}

/// Entries now expire on their own, but a definitive negative still sticks for its TTL.
/// ClearCaches is the lever that turns "wait for the TTL" into "fixed now", so it has
/// to actually drop cached answers.
#[tokio::test]
async fn clear_caches_forces_a_refetch() {
    let tracks = tracks_json(3);
    let h = Harness::sequenced(vec![
        ("/album/999/tracks", vec![tracks.as_str()]),
        ("/album/999", vec![NO_DATA_ENVELOPE, ALBUM_DETAIL_JSON]),
    ])
    .await;

    assert_eq!(h.service.get_album_detail("999").await, None);
    assert_eq!(h.service.get_album_detail("999").await, None);
    assert_eq!(h.calls("/album/999"), 1);

    h.service.clear_caches();

    assert!(h.service.get_album_detail("999").await.is_some());
    assert_eq!(h.calls("/album/999"), 2);
}

// ---- Metadata language (issue #24) ---------------------------------------
// Deezer localizes genre names to the caller's IP country, so the request
// must pin the language or a non-English host writes localized genre tags.

#[tokio::test]
async fn requests_carry_configured_accept_language() {
    let h = Harness::with_language(Routes::single(vec![]), "en").await;

    h.service.get_album_detail("1").await;

    let seen = received(&h.server).await;
    assert!(!seen.is_empty());
    for request in seen {
        let header = request
            .headers
            .get("accept-language")
            .map(|v| v.to_str().unwrap_or(""));
        assert_eq!(header, Some("en"));
    }
}

#[tokio::test]
async fn requests_omit_accept_language_when_language_empty() {
    let h = Harness::with_language(Routes::single(vec![]), "").await;

    h.service.get_album_detail("1").await;

    let seen = received(&h.server).await;
    assert!(!seen.is_empty());
    for request in seen {
        assert!(request.headers.get("accept-language").is_none());
    }
}

/// A throttled track search must not be cached as "this track has no metadata".
#[tokio::test]
async fn enrich_track_async_throttled_not_cached_and_recovers_on_retry() {
    let track = r#"{"data":[{"title":"Golfa","duration":359,
       "album":{"id":711108,"title":"Canciones Prohibidas","cover_xl":"https://cdn/xl.jpg"},
       "artist":{"name":"Extremoduro"}}]}"#;
    let h = Harness::sequenced(vec![
        ("/search?q=", vec![QUOTA_ENVELOPE, track]),
        ("/album/711108", vec![ALBUM_DETAIL_JSON]),
    ])
    .await;

    assert_eq!(
        h.service.enrich_track("Extremoduro", "Golfa", true, false).await,
        None
    );

    let second = h
        .service
        .enrich_track("Extremoduro", "Golfa", true, false)
        .await
        .expect("recovered");
    assert_eq!(second.album_title.as_deref(), Some("Canciones Prohibidas"));
}

/// Rust-only: two lookups asking Deezer the same thing at once share one request, so the
/// budget a search shares with everything else is spent once.
#[tokio::test]
async fn the_same_call_asked_twice_at_once_is_one_request() {
    let json = r#"{"data":[{"title":"Reckoner","duration":290,
        "album":{"id":1,"title":"In Rainbows"},"artist":{"name":"Radiohead"}}]}"#;
    let h = Harness::new(vec![("/search", json)]).await;

    let (first, second) = tokio::join!(
        h.service.enrich_track("Radiohead", "Reckoner", false, false),
        h.service.enrich_track("Radiohead", "Reckoner", false, false),
    );

    assert_eq!(first.and_then(|m| m.duration), Some(290));
    assert_eq!(second.and_then(|m| m.duration), Some(290));
    assert_eq!(h.calls("/search"), 1);
}

/// Rust-only: the free-text track search answers its hits in Deezer's order and remembers each
/// one's track; a song is explicit when any hit of it in the same version is.
#[tokio::test]
async fn the_track_search_remembers_each_hit_and_its_explicit_word() {
    let json = r#"{"data":[
        {"title":"Dracula","duration":210,"explicit_lyrics":false,"explicit_content_lyrics":3,
         "artist":{"name":"Tame Impala"},"album":{"id":5,"title":"Dracula"}},
        {"title":"Dracula","duration":210,"explicit_lyrics":true,"explicit_content_lyrics":1,
         "artist":{"name":"Tame Impala"},"album":{"id":6,"title":"Dracula"}},
        {"title":"Borderline","duration":237,"explicit_lyrics":false,"explicit_content_lyrics":0,
         "artist":{"name":"Tame Impala"},"album":{"id":7,"title":"The Slow Rush"}}]}"#;
    let h = Harness::new(vec![("/search", json)]).await;

    let rows = h
        .service
        .search_tracks("tame impala", 50)
        .await
        .expect("answered");

    assert_eq!(
        rows,
        [
            ("Tame Impala".to_string(), "Dracula".to_string()),
            ("Tame Impala".to_string(), "Dracula".to_string()),
            ("Tame Impala".to_string(), "Borderline".to_string()),
        ]
    );
    let dracula = h
        .service
        .cached_track("Tame Impala", "Dracula")
        .expect("remembered");
    assert_eq!(dracula.explicit_content, Some(explicit_status::EXPLICIT));
    assert_eq!(dracula.album_id, Some(5));
    let borderline = h
        .service
        .cached_track("Tame Impala", "Borderline")
        .expect("remembered");
    assert_eq!(borderline.duration, Some(237));
    assert_eq!(borderline.explicit_content, Some(explicit_status::NOT_EXPLICIT));
}

// ---- Plain-query regression (Deezer dropped field-qualified search) ------
// Octo used to ask for artist:"X" track:"Y". Deezer now reads that as free text, so
// the literal words "artist" and "track" had to appear in the record and nothing ever
// matched: every external song lost its album, year and duration and fell back to a
// flat 180s, and every download was written with bare tags. These tests assert the
// query SHAPE, because the stub routes on path alone and stayed green throughout.

#[tokio::test]
async fn enrich_track_async_sends_plain_terms_without_field_qualifiers() {
    let json = r#"{"data":[{"title":"Reckoner","duration":290,
        "album":{"id":1,"title":"In Rainbows"},"artist":{"name":"Radiohead"}}]}"#;
    let h = Harness::new(vec![("/search", json)]).await;

    h.service
        .enrich_track("Radiohead", "Reckoner", false, false)
        .await;

    let query = &h.queries().await[0];
    assert!(!query.contains("artist:"));
    assert!(!query.contains("track:"));
    assert!(query.contains("Radiohead Reckoner"));
}

#[tokio::test]
async fn enrich_track_full_async_sends_plain_terms_without_field_qualifiers() {
    let json = r#"{"data":[{"title":"Teardrop","duration":330,"isrc":"X",
        "album":{"id":1,"title":"Mezzanine"},"artist":{"name":"Massive Attack"}}]}"#;
    let h = Harness::new(vec![("/album/1", r#"{"id":1}"#), ("/search", json)]).await;

    h.service.enrich_track_full("Massive Attack", "Teardrop").await;

    let query = &h.queries().await[0];
    assert!(!query.contains("artist:"));
    assert!(!query.contains("track:"));
}

/// The search hit names only the main artist and carries no track position, so a
/// collaboration was tagged as one artist (#49) and the track number was never written (#48).
/// Both come from the track's own record. Shape taken from a live answer, 2026-09-25.
#[tokio::test]
async fn enrich_track_full_async_reads_contributors_and_position_from_the_track() {
    let search = r#"{"data":[{"id":2334934765,"title":"Rauw Alejandro: Bzrp Music Sessions, Vol. 56/66",
        "duration":210,"album":{"id":1,"title":"Session 56"},"artist":{"name":"Bizarrap"}}]}"#;
    let track = r#"{"id":2334934765,"track_position":1,"disk_number":1,"contributors":[
        {"name":"Bizarrap","role":"Main"},{"name":"Rauw Alejandro","role":"Main"}]}"#;
    let h = Harness::new(vec![
        ("/track/2334934765", track),
        ("/album/1", r#"{"id":1,"nb_tracks":1}"#),
        ("/search", search),
    ])
    .await;

    let meta = h
        .service
        .enrich_track_full(
            "Bizarrap, Rauw Alejandro",
            "Rauw Alejandro: Bzrp Music Sessions, Vol. 56/66",
        )
        .await
        .expect("found");

    assert_eq!(meta.artist_name.as_deref(), Some("Bizarrap"));
    assert_eq!(
        meta.contributors,
        Some(vec!["Bizarrap".to_string(), "Rauw Alejandro".to_string()])
    );
    assert_eq!(meta.track_number, Some(1));
    assert_eq!(meta.disc_number, Some(1));
}

/// Deezer reports most compilations as record_type "album" (checked live on three), so the
/// album artist is the signal a compilation is recognized by, and it is the album artist
/// rather than the track's that belongs in the album-artist tag.
#[tokio::test]
async fn enrich_track_full_async_reads_the_album_artist_and_record_type() {
    let search = r#"{"data":[{"title":"Song","duration":200,
        "album":{"id":5,"title":"Summer Hits"},"artist":{"name":"Artist"}}]}"#;
    let album = r#"{"id":5,"record_type":"album","artist":{"id":5080,"name":"Various Artists"}}"#;
    let h = Harness::new(vec![("/album/5", album), ("/search", search)]).await;

    let meta = h
        .service
        .enrich_track_full("Artist", "Song")
        .await
        .expect("found");

    assert_eq!(meta.album_artist_name.as_deref(), Some("Various Artists"));
    assert_eq!(meta.record_type.as_deref(), Some("album"));
    assert_eq!(meta.artist_name.as_deref(), Some("Artist"));
}

#[tokio::test]
async fn find_album_id_async_sends_plain_terms_without_field_qualifiers() {
    let json = r#"{"data":[{"id":7,"title":"Discovery","artist":{"name":"Daft Punk"}}]}"#;
    let h = Harness::new(vec![("/search/album", json)]).await;

    h.service.find_album_id("Daft Punk", "Discovery").await;

    let query = &h.queries().await[0];
    assert!(!query.contains("artist:"));
    assert!(!query.contains("album:"));
}

// ---- The guard that makes a plain query safe --------------------------------

#[tokio::test]
async fn enrich_track_async_skips_a_hit_by_another_artist() {
    // A plain query is fuzzy enough to put a cover at position 0. Taking it would
    // attach the wrong album and length to the song as fact.
    let json = r#"{"data":[
        {"title":"Creep","duration":120,"album":{"id":9,"title":"Karaoke Hits"},
         "artist":{"name":"Karaoke All Stars"}},
        {"title":"Creep","duration":238,"album":{"id":1,"title":"Pablo Honey"},
         "artist":{"name":"Radiohead"}}]}"#;
    let h = Harness::new(vec![("/search", json)]).await;

    let meta = h
        .service
        .enrich_track("Radiohead", "Creep", false, false)
        .await
        .expect("found");

    assert_eq!(meta.album_title.as_deref(), Some("Pablo Honey"));
    assert_eq!(meta.duration, Some(238));
}

#[tokio::test]
async fn enrich_track_async_accepts_a_decorated_title() {
    // Deezer decorates titles; an exact compare would reject the right recording.
    let json = r#"{"data":[{"title":"Reckoner (Remastered 2016)","duration":290,
        "album":{"id":1,"title":"In Rainbows"},"artist":{"name":"Radiohead"}}]}"#;
    let h = Harness::new(vec![("/search", json)]).await;

    let meta = h
        .service
        .enrich_track("Radiohead", "Reckoner", false, false)
        .await
        .expect("found");

    assert_eq!(meta.duration, Some(290));
}

#[tokio::test]
async fn enrich_track_async_rejects_a_hit_that_matches_nothing() {
    // A hit stating neither the artist nor the title is not a match by default.
    let json = r#"{"data":[{"duration":111,"album":{"id":9,"title":"Something Else"}}]}"#;
    let h = Harness::new(vec![("/search", json)]).await;

    assert_eq!(
        h.service
            .enrich_track("Radiohead", "Reckoner", false, false)
            .await,
        None
    );
}

// ---- A song track search does not show --------------------------------------

#[tokio::test]
async fn enrich_track_async_a_song_missing_from_track_search_is_found_on_its_album() {
    let h = Harness::new(vec![
        (
            "/search/album",
            r#"{"data":[{"id":7,"title":"Hidden Song","record_type":"single",
                "artist":{"id":5,"name":"Some Artist"}}]}"#,
        ),
        ("/search", r#"{"data":[]}"#),
        (
            "/album/7/tracks",
            r#"{"data":[{"id":77,"title":"Hidden Song","duration":201,"artist":{"id":5,"name":"Some Artist"}}]}"#,
        ),
        (
            "/track/77",
            r#"{"id":77,"title":"Hidden Song","duration":201,
                "album":{"id":7,"title":"Hidden Song"},"artist":{"id":5,"name":"Some Artist"}}"#,
        ),
    ])
    .await;

    let meta = h
        .service
        .enrich_track("Some Artist", "Hidden Song", false, false)
        .await
        .expect("found on its album");

    assert_eq!(meta.album_title.as_deref(), Some("Hidden Song"));
    assert_eq!(meta.duration, Some(201));
    assert_eq!(h.calls("/artist/5/top"), 0);
}

#[tokio::test]
async fn enrich_track_async_a_song_only_on_the_artists_top_songs_is_found_there() {
    // Track search shows only the demo; the original is among the artist's top songs.
    let h = Harness::new(vec![
        ("/search/album", r#"{"data":[]}"#),
        (
            "/search",
            r#"{"data":[{"id":1,"title":"Real Song (Demo)","duration":150,
                "album":{"id":2,"title":"Demos"},"artist":{"id":5,"name":"Some Artist"}}]}"#,
        ),
        (
            "/artist/5/top",
            r#"{"data":[{"id":3,"title":"Real Song","duration":222,
                "album":{"id":4,"title":"The Record"},"artist":{"id":5,"name":"Some Artist"}}]}"#,
        ),
    ])
    .await;

    let meta = h
        .service
        .enrich_track("Some Artist", "Real Song", false, false)
        .await
        .expect("found on the top songs");

    assert_eq!(meta.album_title.as_deref(), Some("The Record"));
    assert_eq!(meta.duration, Some(222));
    assert!(
        h.queries().await.iter().any(|q| q.contains("limit=50")),
        "the top songs are read 50 at a time"
    );
}

#[tokio::test]
async fn enrich_track_async_a_song_found_nowhere_costs_one_extra_step() {
    let h = Harness::new(vec![
        (
            "/search/album",
            r#"{"data":[{"id":7,"title":"Other Record","artist":{"id":5,"name":"Some Artist"}}]}"#,
        ),
        (
            "/search",
            r#"{"data":[{"id":1,"title":"Another Song","artist":{"id":5,"name":"Some Artist"}}]}"#,
        ),
        ("/artist/5/top", r#"{"data":[]}"#),
        ("/album/7/tracks", r#"{"data":[]}"#),
    ])
    .await;

    assert_eq!(
        h.service
            .enrich_track("Some Artist", "Missing Song", false, false)
            .await,
        None
    );

    // One album search (its album is not the song's, so no tracklist) and one top songs read.
    assert_eq!(h.calls("/search/album"), 1);
    assert_eq!(h.calls("/artist/5/top"), 1);
    assert_eq!(h.calls("/album/7/tracks"), 0);
}

// ---- A throttled year must not cost the whole track -------------------------

#[tokio::test]
async fn enrich_track_async_throttled_year_keeps_album_and_duration() {
    // The album detail carries only the year. Returning null when it was throttled
    // threw away an album title and duration already in hand, which is what left a
    // song with no length at all.
    let track = r#"{"data":[{"title":"Creep","duration":238,
        "album":{"id":1,"title":"Pablo Honey"},"artist":{"name":"Radiohead"}}]}"#;
    let h = Harness::new(vec![("/album/1", QUOTA_ENVELOPE), ("/search", track)]).await;

    let meta = h
        .service
        .enrich_track("Radiohead", "Creep", true, false)
        .await
        .expect("kept");

    assert_eq!(meta.album_title.as_deref(), Some("Pablo Honey"));
    assert_eq!(meta.duration, Some(238));
    assert_eq!(meta.year, None);
}

#[tokio::test]
async fn enrich_track_async_throttled_year_is_not_cached() {
    // ...and because the year is still unknown, the answer must not be remembered,
    // or one throttle blip becomes "this track has no year" for the life of the entry.
    let h = Harness::sequenced(vec![
        (
            "/album/1",
            vec![QUOTA_ENVELOPE, r#"{"id":1,"release_date":"1993-02-22"}"#],
        ),
        (
            "/search",
            vec![
                r#"{"data":[{"title":"Creep","duration":238,
                "album":{"id":1,"title":"Pablo Honey"},"artist":{"name":"Radiohead"}}]}"#,
            ],
        ),
    ])
    .await;

    let first = h.service.enrich_track("Radiohead", "Creep", true, false).await;
    assert_eq!(first.expect("kept").year, None);
    let second = h.service.enrich_track("Radiohead", "Creep", true, false).await;
    assert_eq!(second.expect("kept").year, Some(1993));
}

// ---- External artist search -------------------------------------------------
// search3's merge has always known how to fold external artists in, but nothing
// populated the list, so the artist column only ever showed the local library.

#[tokio::test]
async fn search_artists_async_maps_name_image_and_album_count() {
    let json = r#"{"data":[
        {"id":399,"name":"Radiohead","picture_xl":"https://cdn/r.jpg","nb_album":24,"nb_fan":6100000},
        {"id":27,"name":"Daft Punk","picture_medium":"https://cdn/d.jpg","nb_album":11}]}"#;
    let h = Harness::new(vec![("/search/artist", json)]).await;

    let hits = h.service.search_artists("radiohead", 10).await;

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].deezer_id, "399");
    assert_eq!(hits[0].name, "Radiohead");
    assert_eq!(hits[0].picture_url.as_deref(), Some("https://cdn/r.jpg"));
    assert_eq!(hits[0].album_count, 24);
    // What tells two artists of one name apart when nothing better is known.
    assert_eq!(hits[0].fans, 6_100_000);
    assert_eq!(hits[1].fans, 0);
    // Falls back to the medium picture when there is no xl.
    assert_eq!(hits[1].picture_url.as_deref(), Some("https://cdn/d.jpg"));
}

/// An artist Deezer has no picture for comes back with its stand-in (an empty hash in the
/// address); that is no picture, so the apps draw their own empty tile.
#[tokio::test]
async fn search_artists_leaves_out_deezers_stand_in() {
    let json = r#"{"data":[
        {"id":5181383,"name":"LINKIN PARK x STEVE AOKI","picture_xl":"https://cdn-images.dzcdn.net/images/artist//1000x1000-000000-80-0-0.jpg","picture_medium":"https://cdn-images.dzcdn.net/images/artist//250x250-000000-80-0-0.jpg","nb_album":1}]}"#;
    let h = Harness::new(vec![("/search/artist", json)]).await;

    let hits = h.service.search_artists("linkin park steve aoki", 10).await;

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].picture_url, None);
}

#[tokio::test]
async fn search_artists_async_throttled_returns_empty_without_caching() {
    // Caching an empty list on a refusal is what would make external artists vanish
    // from search3 for the rest of the process (issue #8's shape).
    let h = Harness::sequenced(vec![(
        "/search/artist",
        vec![QUOTA_ENVELOPE, r#"{"data":[{"id":399,"name":"Radiohead"}]}"#],
    )])
    .await;

    assert!(h.service.search_artists("radiohead", 10).await.is_empty());
    assert_eq!(h.service.search_artists("radiohead", 10).await.len(), 1);
}

#[tokio::test]
async fn search_artists_async_sends_plain_query() {
    let h = Harness::new(vec![("/search/artist", r#"{"data":[]}"#)]).await;

    h.service.search_artists("radiohead", 10).await;

    assert!(!h.queries().await[0].contains("artist:"));
}

#[tokio::test]
async fn get_artist_albums_async_groups_albums_then_eps_then_singles_then_compilations() {
    // The catalog's own shape for an artist's releases: no artist and no track counts,
    // a clean and an explicit copy of one album, a single, and a compilation of theirs.
    let json = r#"{"data":[
        {"id":1,"title":"First","record_type":"album","release_date":"1997-01-20","cover_xl":"https://cdn/1.jpg"},
        {"id":2,"title":"Second","record_type":"album","release_date":"2001-03-12"},
        {"id":3,"title":"Second","record_type":"album","release_date":"2001-03-12"},
        {"id":4,"title":"A Single","record_type":"single","release_date":"2005-01-01"},
        {"id":5,"title":"The Best Of","record_type":"compile","release_date":"2010-06-01"},
        {"id":6,"title":"Live Set","record_type":"ep","release_date":"2003-09-09"}
    ]}"#;
    let h = Harness::new(vec![("/artist/42/albums", json)]).await;

    let hits = h.service.get_artist_albums("42", "Test Artist").await;

    // The way the apps group a discography, newest first within each group.
    assert_eq!(
        hits.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(),
        ["Second", "First", "Live Set", "A Single", "The Best Of"]
    );
    assert_eq!(
        hits.iter()
            .map(|h| h.record_type.as_deref().unwrap_or(""))
            .collect::<Vec<_>>(),
        ["album", "album", "ep", "single", "compile"]
    );
    assert!(hits.iter().all(|h| h.artist == "Test Artist"));
    let by_title = |t: &str| hits.iter().find(|h| h.title == t).expect("listed").clone();
    assert_eq!(by_title("Second").deezer_id, "2");
    assert_eq!(by_title("First").year, Some(1997));
    assert_eq!(by_title("First").cover_url.as_deref(), Some("https://cdn/1.jpg"));
}

#[tokio::test]
async fn get_artist_albums_async_singles_are_newest_first_too() {
    let json = r#"{"data":[
        {"id":1,"title":"Song A","record_type":"single","release_date":"2020-01-01"},
        {"id":2,"title":"Song B","record_type":"single","release_date":"2022-01-01"}
    ]}"#;
    let h = Harness::new(vec![("/artist/9/albums", json)]).await;

    let hits = h.service.get_artist_albums("9", "New Artist").await;

    assert_eq!(
        hits.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(),
        ["Song B", "Song A"]
    );
}

#[tokio::test]
async fn get_artist_albums_async_a_career_of_singles_shows_them_after_its_album() {
    // Singles used to be hidden whenever there was one album, which hid most of a career
    // like this one.
    let json = r#"{"data":[
        {"id":1,"title":"Hit Three","record_type":"single","release_date":"2024-01-01"},
        {"id":2,"title":"Hit Two","record_type":"single","release_date":"2023-01-01"},
        {"id":3,"title":"The Album","record_type":"album","release_date":"2021-01-01"},
        {"id":4,"title":"Hit One","record_type":"single","release_date":"2020-01-01"}
    ]}"#;
    let h = Harness::new(vec![("/artist/9/albums", json)]).await;

    let hits = h.service.get_artist_albums("9", "Singles Artist").await;

    assert_eq!(
        hits.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(),
        ["The Album", "Hit Three", "Hit Two", "Hit One"]
    );
}

#[tokio::test]
async fn get_artist_albums_async_one_title_is_one_row_the_album_kept() {
    // An album and its own EP or single can share a title. An outside album's id is made
    // from the artist and title, so both rows would open one release: whichever came last.
    let cases = [
        ["album", "ep", "single"],
        ["single", "ep", "album"],
        ["ep", "album", "single"],
    ];
    for [first, second, third] in cases {
        let json = format!(
            r#"{{"data":[
            {{"id":1,"title":"Same Name","record_type":"{first}","release_date":"2020-01-01"}},
            {{"id":2,"title":"Same Name","record_type":"{second}","release_date":"2020-01-01"}},
            {{"id":3,"title":"same name!","record_type":"{third}","release_date":"2019-01-01"}}
        ]}}"#
        );
        let h = Harness::new(vec![("/artist/9/albums", json.as_str())]).await;

        let hits = h.service.get_artist_albums("9", "Some Artist").await;

        assert_eq!(hits.len(), 1, "{first} {second} {third}");
        let hit = &hits[0];
        assert_eq!(
            hit.record_type.as_deref(),
            Some("album"),
            "{first} {second} {third}"
        );
        let album_at = [first, second, third]
            .iter()
            .position(|t| *t == "album")
            .expect("one is the album")
            + 1;
        assert_eq!(hit.deezer_id, album_at.to_string(), "{first} {second} {third}");
    }
}

#[tokio::test]
async fn get_artist_albums_async_repeat_editions_are_one_row() {
    // The catalog lists a record once per edition. The page shows it once, the edition with
    // the most songs; a live or remix album is another record and keeps its row, and a single
    // stays a single after the records.
    let json = r#"{"data":[
        {"id":1,"title":"Take Care","record_type":"album","release_date":"2011-11-15","nb_tracks":17,"explicit_lyrics":true},
        {"id":2,"title":"Take Care (Deluxe)","record_type":"album","release_date":"2011-11-15","nb_tracks":19,"explicit_lyrics":true},
        {"id":3,"title":"Take Care - Remastered 2021","record_type":"album","release_date":"2021-01-01","nb_tracks":17,"explicit_lyrics":true},
        {"id":4,"title":"Take Care (Live)","record_type":"album","release_date":"2012-06-01","nb_tracks":12},
        {"id":5,"title":"Take Care (Remixes)","record_type":"album","release_date":"2012-02-01","nb_tracks":6},
        {"id":6,"title":"Take Care (FOMO)","record_type":"album","release_date":"2013-02-01","nb_tracks":4},
        {"id":7,"title":"Headlines","record_type":"single","release_date":"2011-08-01","nb_tracks":1},
        {"id":8,"title":"Headlines (Explicit)","record_type":"single","release_date":"2011-08-01","nb_tracks":1}
    ]}"#;
    let h = Harness::new(vec![("/artist/9/albums", json)]).await;

    let hits = h.service.get_artist_albums("9", "Drake").await;

    assert_eq!(
        hits.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(),
        [
            "Take Care (FOMO)",
            "Take Care (Live)",
            "Take Care (Remixes)",
            "Take Care (Deluxe)",
            "Headlines",
        ]
    );
}

#[tokio::test]
async fn get_artist_albums_a_remaster_folds_into_its_record_and_a_single_keeps_its_row() {
    // A remaster and the original are one record, shown as the first listed of the two; a
    // single of another title is never folded into it and never takes its place.
    let json = r#"{"data":[
        {"id":2901,"title":"Northern Lights (2020 Remaster)","record_type":"album","nb_tracks":4,"release_date":"2020-11-20"},
        {"id":2902,"title":"Northern Lights","record_type":"album","nb_tracks":4,"release_date":"2020-03-06"},
        {"id":2903,"title":"Borealis","record_type":"single","nb_tracks":1,"release_date":"2022-01-14"}
    ]}"#;
    let h = Harness::new(vec![("/artist/1902/albums", json)]).await;

    let hits = h.service.get_artist_albums("1902", "Aurora Vale").await;

    assert_eq!(
        hits.iter()
            .map(|h| (h.deezer_id.as_str(), h.record_type.as_deref(), h.track_count))
            .collect::<Vec<_>>(),
        [("2901", Some("album"), 4), ("2903", Some("single"), 1)]
    );
}

#[tokio::test]
async fn get_artist_albums_on_more_pages_keeps_every_edition() {
    // New Releases reads the catalog's own listing: a new deluxe edition is news there.
    let json = r#"{"data":[
        {"id":1,"title":"Take Care","record_type":"album","release_date":"2011-11-15","nb_tracks":17},
        {"id":2,"title":"Take Care (Deluxe)","record_type":"album","release_date":"2024-11-15","nb_tracks":19}
    ]}"#;
    let h = Harness::new(vec![("/artist/9/albums", json)]).await;

    let hits = h.service.get_artist_albums_on("9", "Drake", true, 3).await;

    assert_eq!(hits.len(), 2);
}

#[test]
fn fold_editions_keeps_a_clean_copy_beside_its_explicit_record_when_asked() {
    let hit = |id: &str, title: &str, tracks: i32, explicit: Option<bool>| AlbumHit {
        deezer_id: id.into(),
        title: title.into(),
        artist: "Drake".into(),
        cover_url: None,
        year: None,
        track_count: tracks,
        record_type: Some("album".into()),
        release_date: None,
        explicit_lyrics: explicit,
    };
    let hits = vec![
        hit("1", "Views", 20, Some(true)),
        hit("2", "Views", 20, Some(false)),
        hit("3", "Views (Deluxe Edition)", 22, Some(true)),
        hit("4", "Nothing Was the Same", 13, Some(true)),
        hit("5", "Nothing Was the Same (Deluxe)", 15, None),
    ];

    let ids = |folded: Vec<AlbumHit>| folded.into_iter().map(|h| h.deezer_id).collect::<Vec<_>>();
    // Search keeps the clean copy, the one of the record's own title, after the record.
    assert_eq!(ids(fold_editions(hits.clone(), true)), ["3", "2", "4"]);
    // An artist page lists one copy of each record.
    assert_eq!(ids(fold_editions(hits, false)), ["3", "4"]);
}
#[tokio::test]
async fn get_artist_albums_async_says_so_when_the_catalog_has_more_than_the_listing() {
    let json = r#"{"total":250,"data":[
        {"id":1,"title":"Newest","record_type":"album","release_date":"2024-01-01"}]}"#;
    let h = Harness::new(vec![("/artist/9/albums", json)]).await;
    let (logs, _guard) = LogCapture::start();

    assert_eq!(h.service.get_artist_albums("9", "Long Career").await.len(), 1);

    let warnings = logs.at(Level::WARN);
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains("has 250 releases; the page lists the first 1"))
            .count(),
        1
    );
}

#[tokio::test]
async fn get_artist_albums_async_a_whole_listing_warns_of_nothing() {
    let json = r#"{"total":1,"data":[
        {"id":1,"title":"Only","record_type":"album","release_date":"2024-01-01"}]}"#;
    let h = Harness::new(vec![("/artist/9/albums", json)]).await;
    let (logs, _guard) = LogCapture::start();

    h.service.get_artist_albums("9", "Short Career").await;

    assert!(logs.at(Level::WARN).is_empty());
}

// ---- Ranked candidates for the release chooser ----------------------------------------

const TWO_HIT_SEARCH: &str = r#"{"data":[
    {"id":11,"title":"Teardrop","duration":330,"isrc":"GBAAA9800001","explicit_lyrics":false,
     "album":{"id":1,"title":"Mezzanine","cover_xl":"https://cdn/mezz.jpg"},"artist":{"name":"Massive Attack"}},
    {"id":22,"title":"Teardrop","duration":330,"isrc":"GBAAA9800001",
     "album":{"id":2,"title":"Collected","cover_xl":"https://cdn/col.jpg"},"artist":{"name":"Massive Attack"}},
    {"id":33,"title":"Teardrop (Live)","duration":340,
     "album":{"id":3,"title":"Live at Wembley"},"artist":{"name":"Massive Attack"}}
]}"#;

#[tokio::test]
async fn enrich_track_candidates_async_detail_fetches_the_best_two_and_reads_the_new_fields() {
    let h = Harness::new(vec![
        (
            "/album/1",
            r#"{"id":1,"record_type":"album","upc":"724384559922","label":"Virgin","release_date":"1998-04-20","nb_tracks":11,"artist":{"name":"Massive Attack"}}"#,
        ),
        (
            "/album/2",
            r#"{"id":2,"record_type":"compile","upc":"094636482323","release_date":"2006-03-27","artist":{"name":"Massive Attack"}}"#,
        ),
        ("/album/3", r#"{"id":3,"record_type":"album"}"#),
        (
            "/track/11",
            r#"{"id":11,"track_position":3,"disk_number":1,"gain":-9.8,"explicit_lyrics":false,"contributors":[{"name":"Massive Attack","role":"Main"},{"name":"Elizabeth Fraser","role":"Featured"}]}"#,
        ),
        ("/track/22", r#"{"id":22,"track_position":7,"disk_number":1}"#),
        ("/search", TWO_HIT_SEARCH),
    ])
    .await;

    let answer = h
        .service
        .enrich_track_candidates("Massive Attack", "Teardrop", 2)
        .await;

    assert!(!answer.did_not_answer);
    assert_eq!(answer.hits.len(), 2);
    let mezzanine = &answer.hits[0];
    assert_eq!(mezzanine.title.as_deref(), Some("Teardrop"));
    assert_eq!(mezzanine.track_id.as_deref(), Some("11"));
    assert_eq!(mezzanine.album_id.as_deref(), Some("1"));
    assert_eq!(mezzanine.barcode.as_deref(), Some("724384559922"));
    assert_eq!(mezzanine.explicit_lyrics, Some(false));
    assert_eq!(mezzanine.catalog_gain, Some(-9.8));
    assert_eq!(mezzanine.record_type.as_deref(), Some("album"));
    assert_eq!(
        mezzanine.contributors,
        Some(vec!["Massive Attack".to_string(), "Elizabeth Fraser".to_string()])
    );
    assert_eq!(answer.hits[1].record_type.as_deref(), Some("compile"));
    // The live take contradicts the title, so it is never a hit, and the best two cost two album details.
    let paths = h.paths().await;
    assert_eq!(paths.iter().filter(|p| p.starts_with("/album/")).count(), 2);
    assert!(!paths.iter().any(|p| p == "/album/3"));
}

#[tokio::test]
async fn enrich_track_candidates_async_one_hit_costs_one_album_detail_and_is_remembered() {
    let one = r#"{"data":[{"id":11,"title":"Teardrop","duration":330,"album":{"id":1,"title":"Mezzanine"},"artist":{"name":"Massive Attack"}}]}"#;
    let h = Harness::new(vec![
        ("/album/1", r#"{"id":1,"record_type":"album"}"#),
        ("/track/11", r#"{"id":11}"#),
        ("/search", one),
    ])
    .await;

    let first = h
        .service
        .enrich_track_candidates("Massive Attack", "Teardrop", 2)
        .await;
    let second = h
        .service
        .enrich_track_candidates("Massive Attack", "Teardrop", 2)
        .await;

    assert_eq!(first.hits.len(), 1);
    assert!(Arc::ptr_eq(&first, &second));
    let paths = h.paths().await;
    assert_eq!(paths.iter().filter(|p| p.starts_with("/album/")).count(), 1);
}

/// A throttled catalog gives no candidate, says so, and leaves nothing in the cache
/// to repeat the throttle for the next twelve hours.
#[tokio::test]
async fn enrich_track_candidates_async_throttled_answers_nothing_and_remembers_nothing() {
    let h = Harness::sequenced(vec![
        ("/album/1", vec![r#"{"id":1,"record_type":"album"}"#]),
        ("/track/11", vec![r#"{"id":11}"#]),
        (
            "/search",
            vec![
                QUOTA_ENVELOPE,
                r#"{"data":[{"id":11,"title":"Teardrop","duration":330,"album":{"id":1,"title":"Mezzanine"},"artist":{"name":"Massive Attack"}}]}"#,
            ],
        ),
    ])
    .await;

    let throttled = h
        .service
        .enrich_track_candidates("Massive Attack", "Teardrop", 2)
        .await;
    let later = h
        .service
        .enrich_track_candidates("Massive Attack", "Teardrop", 2)
        .await;

    assert!(throttled.did_not_answer);
    assert!(throttled.hits.is_empty());
    assert!(!later.did_not_answer);
    assert_eq!(later.hits.len(), 1);
    assert_eq!(h.calls("/search"), 2);
}

// ---- Query variant lookups ------------------------------------------------------------
// A source that indexes "Suicideboys" still finds a "$uicideboy$" song: each lookup tries the
// song as asked, then the other ways SongIdentity writes it, and holds whatever it finds to
// the song as asked.

fn variant_routes(routes: Vec<(&str, &str)>) -> Routes {
    Routes::new(
        routes
            .into_iter()
            .map(|(needle, body)| (needle, vec![body]))
            .collect(),
        Matching::Unescaped,
    )
    .with_not_found_body("{}")
}

#[tokio::test]
async fn deezer_finds_the_track_under_its_spelled_out_name() {
    let routes = variant_routes(vec![
        (
            "search?q=suicideboys SUICIDE",
            r#"{"data":[{"id":1,"title":"Suicide","duration":171,"artist":{"name":"Suicideboys"},"album":{"id":0,"title":"Dirtiest"}}]}"#,
        ),
        ("/search?q=", r#"{"data":[]}"#),
    ]);
    let h = Harness::with_language(routes, "en").await;

    let meta = h
        .service
        .enrich_track("$uicideboy$", "$UICIDE", false, false)
        .await;

    assert_eq!(meta.as_ref().and_then(|m| m.duration), Some(171));
    assert_eq!(
        meta.as_ref().and_then(|m| m.album_title.as_deref()),
        Some("Dirtiest")
    );
    let calls = received(&h.server).await;
    assert_eq!(
        calls
            .iter()
            .filter(|r| dotnet::unescape_data_string(r.url.as_str()).contains("/search?q="))
            .count(),
        2
    );
}

#[tokio::test]
async fn deezer_never_takes_another_versions_album_and_length() {
    let routes = variant_routes(vec![(
        "/search?q=",
        r#"{"data":[{"id":1,"title":"Creep (Live)","duration":260,"artist":{"name":"Radiohead"},"album":{"id":0,"title":"Live Album"}}]}"#,
    )]);
    let h = Harness::with_language(routes, "en").await;

    assert_eq!(
        h.service.enrich_track("Radiohead", "Creep", false, false).await,
        None
    );
}

// ---- The year prefix, shared requests and the catalog lookup ---------------------------

#[test]
fn year_prefix_parses_the_first_four_characters() {
    assert_eq!(year_prefix("1998-04-30"), Some(1998));
    assert_eq!(year_prefix(" 199-01"), Some(199));
    assert_eq!(year_prefix("199"), None);
    assert_eq!(year_prefix("19a8"), None);
    assert_eq!(year_prefix("-199"), Some(-199));
}

/// Two callers asking for one artist at once share one request.
#[tokio::test]
async fn concurrent_artist_searches_share_one_request() {
    let h = Harness::new(vec![(
        "/search/artist",
        r#"{"data":[{"id":399,"name":"Radiohead"}]}"#,
    )])
    .await;

    let (a, b) = tokio::join!(
        h.service.search_artists("radiohead", 10),
        h.service.search_artists("radiohead", 10)
    );

    assert_eq!(a, b);
    assert_eq!(h.calls("/search/artist"), 1);
}

/// The release identifier reaches the catalog through `CatalogLookup`.
#[tokio::test]
async fn the_service_is_the_identifiers_catalog_lookup() {
    let one = r#"{"data":[{"id":11,"title":"Teardrop","duration":330,"album":{"id":1,"title":"Mezzanine"},"artist":{"name":"Massive Attack"}}]}"#;
    let h = Harness::new(vec![
        ("/album/1", r#"{"id":1,"record_type":"album"}"#),
        ("/track/11", r#"{"id":11}"#),
        ("/search", one),
    ])
    .await;
    let lookup: &dyn CatalogLookup = &h.service;

    let answer = lookup
        .enrich_track_candidates("Massive Attack", "Teardrop", 2)
        .await
        .expect("never fails");

    assert_eq!(answer.hits.len(), 1);
    assert_eq!(answer.hits[0].album_title.as_deref(), Some("Mezzanine"));
}

#[tokio::test]
async fn artist_page_of_a_release_in_two_copies_lists_the_explicit_one() {
    let h = Harness::new(vec![(
        "/artist/",
        r#"{"total":2,"data":[
            {"id":200,"title":"GNX","record_type":"album","explicit_lyrics":false,"release_date":"2024-11-22"},
            {"id":100,"title":"GNX","record_type":"album","explicit_lyrics":true,"release_date":"2024-11-22"}]}"#,
    )])
    .await;

    let hits = h.service.get_artist_albums("525046", "Kendrick Lamar").await;

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].deezer_id, "100");
    assert_eq!(hits[0].explicit_lyrics, Some(true));
}

#[tokio::test]
async fn get_artist_albums_on_a_clean_only_server_keeps_the_clean_edition() {
    let json = r#"{"data":[
        {"id":1,"title":"Views","record_type":"album","release_date":"2016-04-29","nb_tracks":20,"explicit_lyrics":true},
        {"id":2,"title":"Views","record_type":"album","release_date":"2016-04-29","nb_tracks":20,"explicit_lyrics":false},
        {"id":3,"title":"Scorpion","record_type":"album","release_date":"2018-06-29","nb_tracks":25,"explicit_lyrics":true},
        {"id":4,"title":"Scorpion (Clean)","record_type":"album","release_date":"2018-06-29","nb_tracks":25,"explicit_lyrics":false}
    ]}"#;
    let routes = Routes::single(vec![("/artist/9/albums", json)]);
    let server = routes.serve().await;
    let settings = SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            explicit_filter: ExplicitFilter::CleanOnly,
            ..Default::default()
        },
        ..Default::default()
    });
    let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
    let service = DeezerMetadataService::with_base_url(http, Arc::new(settings), server.uri());

    let hits = service.get_artist_albums("9", "Drake").await;

    let ids: Vec<&str> = hits.iter().map(|hit| hit.deezer_id.as_str()).collect();
    assert_eq!(ids, ["4", "2"]);
}

// ---- How close a hit has to be ---------------------------------------------------------------

#[test]
fn a_short_title_is_not_found_inside_a_longer_one() {
    // "home" (4) inside "homecoming": too short to count.
    assert_eq!(compare_titles("Home", Some("Homecoming")), FieldVerdict::Mismatch);
    assert_eq!(compare_titles("Homecoming", Some("Home")), FieldVerdict::Mismatch);
    // Six characters or more still counts: Deezer's own decorations stay matched.
    assert_eq!(
        compare_titles("Teardrop", Some("Teardrop (From the Motion Picture)")),
        FieldVerdict::Match
    );
    // Equal keys need no floor.
    assert_eq!(compare_titles("Home", Some("HOME")), FieldVerdict::Match);
}

#[test]
fn a_short_artist_is_not_found_inside_a_longer_one() {
    assert_eq!(compare_artists("Muse", Some("Musette")), FieldVerdict::Mismatch);
    assert_eq!(
        compare_artists("Rihanna", Some("Rihanna Experience Band")),
        FieldVerdict::Match
    );
    assert_eq!(compare_artists("Drake", Some("Drake")), FieldVerdict::Match);
}

#[test]
fn a_hit_of_a_far_length_is_another_recording_when_both_lengths_are_known() {
    let doc = serde_json::json!({"data": [
        {"id": 1, "title": "Teardrop", "duration": 400, "artist": {"name": "Massive Attack"}},
        {"id": 2, "title": "Teardrop", "duration": 335, "artist": {"name": "Massive Attack"}},
        {"id": 3, "title": "Teardrop", "artist": {"name": "Massive Attack"}},
    ]});
    let ids = |seconds: Option<i32>| -> Vec<i64> {
        all_matches(Some(&doc), "Massive Attack", "Teardrop", seconds)
            .expect("read")
            .iter()
            .map(|hit| hit["id"].as_i64().expect("id"))
            .collect()
    };
    assert_eq!(ids(Some(330)), [2, 3]);
    assert_eq!(ids(Some(338)), [2, 3]);
    assert_eq!(ids(None), [1, 2, 3]);
    assert_eq!(ids(Some(0)), [1, 2, 3]);
}

// ---- The clip check's and the heart's lookups ---------------------------------------------

/// Yesterday as `track/<id>` and `track/isrc:<CODE>` answer it, cut to a few fields.
const YESTERDAY_TRACK: &str = r#"{"id":116348164,"readable":true,"title":"Yesterday (Remastered 2009)",
    "title_short":"Yesterday","isrc":"GBAYE0601477","duration":123,"track_position":13,"disk_number":1,
    "preview":"https://cdnt-preview.dzcdn.net/api/1/1/4/f/b/0/4fb0ce16.mp3?hdnea=exp=1792886716~hmac=cdec16e3",
    "artist":{"id":1,"name":"The Beatles"},"album":{"id":12047952,"title":"Help! (Remastered)"}}"#;

fn yesterday() -> DeezerTrack {
    DeezerTrack {
        id: "116348164".into(),
        title: "Yesterday (Remastered 2009)".into(),
        artist: "The Beatles".into(),
        duration_seconds: Some(123),
        isrc: Some("GBAYE0601477".into()),
        clip_url: Some(
            "https://cdnt-preview.dzcdn.net/api/1/1/4/f/b/0/4fb0ce16.mp3?hdnea=exp=1792886716~hmac=cdec16e3"
                .into(),
        ),
    }
}

const HELP_ALBUM: &str = r#"{"id":12047952,"title":"Help! (Remastered)","upc":"094638241256",
    "record_type":"album","nb_tracks":14,"artist":{"name":"The Beatles"}}"#;

const HELP_TRACKS: &str = r#"{"total":2,"data":[
    {"id":116348150,"title":"Help! (Remastered 2009)","isrc":"GBAYE0601467","duration":138,
     "track_position":1,"disk_number":1,"explicit_lyrics":false,"explicit_content_lyrics":0},
    {"id":116348164,"title":"Yesterday (Remastered 2009)","isrc":"GBAYE0601477","duration":123,
     "track_position":13,"disk_number":1,"explicit_lyrics":false,"explicit_content_lyrics":0}
]}"#;

const ONE_MORE_TIME_HIT: &str = r#"{"data":[{"id":3135556,"title":"One More Time","duration":320,
    "artist":{"id":27,"name":"Daft Punk"},"album":{"id":302127,"title":"Discovery"}}]}"#;

const ONE_MORE_TIME_TRACK: &str = r#"{"id":3135556,"title":"One More Time","duration":320,
    "isrc":"GBDUW0000053","preview":"https://cdnt-preview.dzcdn.net/api/1/1/a/b/c/0/abc.mp3?hdnea=exp=1~hmac=x",
    "artist":{"id":27,"name":"Daft Punk"},"album":{"id":302127,"title":"Discovery"}}"#;

/// Every request's path after the base, with its query, as it was sent.
async fn asked(server: &MockServer) -> Vec<String> {
    received(server)
        .await
        .iter()
        .map(|r| match r.url.query() {
            Some(query) => format!("{}?{query}", &r.url.path()[1..]),
            None => r.url.path()[1..].to_string(),
        })
        .collect()
}

/// A service asking `base` in place of api.deezer.com, unmetered.
fn service_on(base: String) -> DeezerMetadataService {
    DeezerMetadataService::with_base_url(
        Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
        Arc::new(SettingsStore::for_tests(AppSettings::default())),
        base,
    )
}

/// The clip check's questions and the heart's album questions are asked byte for byte as the
/// heart's builders write them, and the heart's album search is `find_album_id`'s own question.
#[tokio::test]
async fn the_clip_checks_and_the_hearts_questions_are_asked_byte_for_byte() {
    // Every request 404s: only the questions are looked at here.
    let h = Harness::new(vec![]).await;
    assert_eq!(h.service.clip_track("116348164").await, DeezerAsked::Busy);
    assert_eq!(
        h.service.clip_track_by_isrc("gb-aye-06-01477").await,
        DeezerAsked::Busy
    );
    assert_eq!(h.service.album_rows("12047952", false).await, DeezerAsked::Busy);
    assert_eq!(
        h.service.album_search_hits("The Beatles", "Help!", false).await,
        DeezerAsked::Busy
    );
    assert_eq!(h.service.find_album_id("The Beatles", "Help!").await, None);

    let search = deezer_album_search_url("The Beatles", "Help!").expect("an album search");
    assert_eq!(search, "search/album?q=The%20Beatles%20Help%21&limit=5");
    let mut expected = vec![
        "track/116348164".to_string(),
        "track/isrc:GBAYE0601477".to_string(),
        deezer_album_url("12047952"),
        deezer_album_tracks_url("12047952"),
        search.clone(),
        search,
    ];
    expected.sort();
    let mut questions = asked(&h.server).await;
    questions.sort();
    assert_eq!(questions, expected);
}

/// A track answer gives its id, title, artist, length, ISRC and clip URL, and is asked afresh
/// every time: the clip's URL is signed and soon runs out.
#[tokio::test]
async fn a_track_reads_its_clip_and_is_asked_afresh_every_time() {
    let h = Harness::new(vec![
        ("/track/isrc:GBAYE0601477", YESTERDAY_TRACK),
        ("/track/116348164", YESTERDAY_TRACK),
        (
            "/track/7",
            r#"{"id":7,"title":"Song","duration":0,"preview":"","artist":{"name":"A"}}"#,
        ),
    ])
    .await;
    assert_eq!(
        h.service.clip_track("116348164").await,
        DeezerAsked::Answer(yesterday())
    );
    assert_eq!(
        h.service.clip_track(" 116348164 ").await,
        DeezerAsked::Answer(yesterday())
    );
    assert_eq!(
        h.service.clip_track_by_isrc("GBAYE0601477").await,
        DeezerAsked::Answer(yesterday())
    );
    assert_eq!(h.calls("/track/116348164"), 2);
    // An empty clip URL and a zero length are unknown.
    assert_eq!(
        h.service.clip_track("7").await,
        DeezerAsked::Answer(DeezerTrack {
            id: "7".into(),
            title: "Song".into(),
            artist: "A".into(),
            ..Default::default()
        })
    );
    // Nothing to ask: nothing is asked.
    assert_eq!(h.service.clip_track(" ").await, DeezerAsked::NoData);
    assert_eq!(
        h.service.clip_track_by_isrc("not a code").await,
        DeezerAsked::NoData
    );
    assert_eq!(received(&h.server).await.len(), 4);
}

/// Deezer's own "no data" and an answer with nothing in it are no data; a refusal in any other
/// words, a 429, a 5xx and a network error are busy, never no data.
#[tokio::test]
async fn a_track_lookup_keeps_busy_apart_from_no_data() {
    let server = MockServer::start().await;
    let answers = [
        (
            "/track/1",
            ResponseTemplate::new(200).set_body_string(NO_DATA_ENVELOPE),
        ),
        ("/track/2", ResponseTemplate::new(200).set_body_string("{}")),
        (
            "/track/3",
            ResponseTemplate::new(200).set_body_string(QUOTA_ENVELOPE),
        ),
        ("/track/4", ResponseTemplate::new(429)),
        ("/track/5", ResponseTemplate::new(503)),
        (
            "/track/isrc:GBAYE0601477",
            ResponseTemplate::new(200).set_body_string(NO_DATA_ENVELOPE),
        ),
    ];
    for (route, answer) in answers {
        Mock::given(path(route)).respond_with(answer).mount(&server).await;
    }
    let service = service_on(server.uri());
    assert_eq!(service.clip_track("1").await, DeezerAsked::NoData);
    assert_eq!(service.clip_track("2").await, DeezerAsked::NoData);
    assert_eq!(service.clip_track("3").await, DeezerAsked::Busy);
    assert_eq!(service.clip_track("4").await, DeezerAsked::Busy);
    assert_eq!(service.clip_track("5").await, DeezerAsked::Busy);
    assert_eq!(
        service.clip_track_by_isrc("GBAYE0601477").await,
        DeezerAsked::NoData
    );
    let unreachable = service_on(format!("http://{}", refused_address()));
    assert_eq!(unreachable.clip_track("1").await, DeezerAsked::Busy);
    assert_eq!(
        unreachable.clip_track_by_isrc("GBAYE0601477").await,
        DeezerAsked::Busy
    );
}

/// The track search asks the full track lookup's own searches, then the track it found.
#[tokio::test]
async fn the_track_search_asks_the_full_lookups_own_searches_then_the_track() {
    let routes = || {
        vec![
            ("/track/3135556", ONE_MORE_TIME_TRACK),
            ("/search?", ONE_MORE_TIME_HIT),
            ("/album/302127", ALBUM_DETAIL_JSON),
        ]
    };
    let searched = Harness::new(routes()).await;
    assert_eq!(
        searched
            .service
            .clip_track_search("Daft Punk", "One More Time")
            .await,
        DeezerAsked::Answer(DeezerTrack {
            id: "3135556".into(),
            title: "One More Time".into(),
            artist: "Daft Punk".into(),
            duration_seconds: Some(320),
            isrc: Some("GBDUW0000053".into()),
            clip_url: Some(
                "https://cdnt-preview.dzcdn.net/api/1/1/a/b/c/0/abc.mp3?hdnea=exp=1~hmac=x".into()
            ),
        })
    );
    let full = Harness::new(routes()).await;
    let meta = full
        .service
        .enrich_track_full("Daft Punk", "One More Time")
        .await
        .expect("the full lookup finds it");
    assert_eq!(meta.track_id.as_deref(), Some("3135556"));

    let searches = |questions: &[String]| -> Vec<String> {
        questions
            .iter()
            .filter(|question| question.starts_with("search"))
            .cloned()
            .collect()
    };
    let by_search = asked(&searched.server).await;
    assert_eq!(searches(&by_search), searches(&asked(&full.server).await));
    assert_eq!(by_search.last().map(String::as_str), Some("track/3135556"));
}

/// A search that did not answer is busy; one that finds the song nowhere is no data.
#[tokio::test]
async fn the_track_search_keeps_busy_apart_from_no_track() {
    let busy = Harness::new(vec![("/search?", QUOTA_ENVELOPE)]).await;
    assert_eq!(
        busy.service.clip_track_search("Daft Punk", "One More Time").await,
        DeezerAsked::Busy
    );
    let nowhere = Harness::new(vec![("/search", r#"{"data":[]}"#)]).await;
    assert_eq!(
        nowhere
            .service
            .clip_track_search("Daft Punk", "One More Time")
            .await,
        DeezerAsked::NoData
    );
    assert_eq!(
        nowhere.service.clip_track_search(" ", "").await,
        DeezerAsked::NoData
    );
}

/// A catalog album comes back as the heart's rows, each track with its id.
#[tokio::test]
async fn a_catalog_album_carries_each_tracks_id() {
    let h = Harness::new(vec![
        ("/album/12047952/tracks", HELP_TRACKS),
        ("/album/12047952", HELP_ALBUM),
    ])
    .await;
    let DeezerAsked::Answer((album, tracks)) = h.service.album_rows("12047952", false).await else {
        panic!("the album answers");
    };
    assert_eq!(
        album,
        CatalogAlbum {
            id: "12047952".into(),
            upc: "094638241256".into(),
            record_type: "album".into(),
            track_count: Some(14),
        }
    );
    assert_eq!(
        tracks
            .iter()
            .map(|t| (t.id.as_str(), t.disc, t.position, t.isrc.as_str(), t.seconds))
            .collect::<Vec<_>>(),
        [
            ("116348150", Some(1), Some(1), "GBAYE0601467", 138),
            ("116348164", Some(1), Some(13), "GBAYE0601477", 123),
        ]
    );
}

/// No such album, or one with no tracks, is no data; a tracklist that did not answer is busy.
#[tokio::test]
async fn a_catalog_album_keeps_busy_apart_from_no_album() {
    let gone = Harness::new(vec![
        ("/album/1/tracks", HELP_TRACKS),
        ("/album/1", NO_DATA_ENVELOPE),
    ])
    .await;
    assert_eq!(gone.service.album_rows("1", false).await, DeezerAsked::NoData);
    let throttled = Harness::new(vec![
        ("/album/1/tracks", QUOTA_ENVELOPE),
        ("/album/1", HELP_ALBUM),
    ])
    .await;
    assert_eq!(throttled.service.album_rows("1", false).await, DeezerAsked::Busy);
    let empty = Harness::new(vec![
        ("/album/1/tracks", r#"{"total":0,"data":[]}"#),
        ("/album/1", HELP_ALBUM),
    ])
    .await;
    assert_eq!(empty.service.album_rows("1", true).await, DeezerAsked::NoData);
    assert_eq!(empty.service.album_rows(" ", false).await, DeezerAsked::NoData);
}

/// D2b's search gives its albums in search order; no album is no data, a refusal is busy, and a
/// blank album title asks nothing.
#[tokio::test]
async fn the_album_search_reads_its_albums_in_order_and_keeps_busy_apart() {
    let found = r#"{"data":[
        {"id":12047952,"title":"Help! (Remastered)","artist":{"name":"The Beatles"}},
        {"id":6157080,"title":"Help!","artist":{"name":"The Beatles"}}
    ]}"#;
    let h = Harness::new(vec![("/search/album", found)]).await;
    assert_eq!(
        h.service.album_search_hits("The Beatles", "Help!", false).await,
        DeezerAsked::Answer(vec![
            CatalogAlbumHit {
                id: "12047952".into(),
                title: "Help! (Remastered)".into(),
                artist: "The Beatles".into(),
            },
            CatalogAlbumHit {
                id: "6157080".into(),
                title: "Help!".into(),
                artist: "The Beatles".into(),
            },
        ])
    );
    assert_eq!(
        h.service.album_search_hits("The Beatles", " ", false).await,
        DeezerAsked::NoData
    );
    assert_eq!(h.calls("/search/album"), 1);
    let none = Harness::new(vec![("/search/album", r#"{"data":[]}"#)]).await;
    assert_eq!(
        none.service.album_search_hits("The Beatles", "Help!", true).await,
        DeezerAsked::NoData
    );
    let busy = Harness::new(vec![("/search/album", QUOTA_ENVELOPE)]).await;
    assert_eq!(
        busy.service
            .album_search_hits("The Beatles", "Help!", false)
            .await,
        DeezerAsked::Busy
    );
}

/// With the interactive window spent, the shipped `get` gives up while it waits for its permit;
/// the clip check's lookup waits the permit out and still has its whole timeout for the call.
#[tokio::test]
async fn a_busy_queue_never_times_a_track_lookup_out() {
    let server = MockServer::start().await;
    Mock::given(path("/track/116348164"))
        .respond_with(ResponseTemplate::new(200).set_body_string(YESTERDAY_TRACK))
        .mount(&server)
        .await;
    // The mock server is the API here, so every request is metered.
    let http = Arc::new(
        DeezerRateLimitHandler::with_api_host(Arc::new(DeezerRateLimiter::new()), "127.0.0.1")
            .with_timeout(Duration::from_millis(500)),
    );
    let service = DeezerMetadataService::with_base_url(
        Arc::clone(&http),
        Arc::new(SettingsStore::for_tests(AppSettings::default())),
        server.uri(),
    );
    // Every interactive permit spent at once: the next comes back with the window, about 5 s on.
    for _ in 0..30 {
        assert!(http.limiter().acquire(false).await.is_acquired());
    }

    let url = format!("{}/track/116348164", server.uri());
    let error = http
        .get(&url, false, None)
        .await
        .expect_err("the shipped call gives up inside the wait");
    assert!(error.to_string().starts_with("No answer came within"), "{error}");

    let started = std::time::Instant::now();
    assert_eq!(
        service.clip_track("116348164").await,
        DeezerAsked::Answer(yesterday())
    );
    assert!(
        started.elapsed() > Duration::from_millis(500),
        "the lookup waited past its timeout for the permit: {:?}",
        started.elapsed()
    );
    assert_eq!(received(&server).await.len(), 1);
}
