//! The Deezer metadata service. The records it answers with are in
//! `octo_core::metadata::deezer_metadata_service`.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use octo_core::common::SongIdentity;
use octo_core::common::dotnet::{self, escape_data_string, is_blank};
use octo_core::fingerprint::heart_release::{
    CatalogAlbum, CatalogAlbumHit, CatalogTrack, deezer_album_search_url, deezer_album_tracks_url,
    deezer_album_url, read_album_search, read_catalog_album, read_catalog_tracks,
};
use octo_core::json::element::{
    ElementResult, array_length, enumerate_array, get_double, get_int32, get_int64, str_prop,
    try_get_property,
};
use octo_core::metadata::accept_language_header;
use octo_core::metadata::deezer_image::deezer_image;
use octo_core::metadata::deezer_metadata_service::normalized_record_type;
pub use octo_core::metadata::deezer_metadata_service::{
    AlbumAnswer, AlbumDetail, AlbumHit, AlbumLookup, AlbumTrack, ArtistHit, ArtistMeta, CatalogCandidates,
    FullTrackMeta, TrackMeta, release_rank, release_types,
};
use octo_core::models::domain::explicit_status;
use octo_core::settings::{ExplicitFilter, SettingsStore};
use octo_core::tagging::CatalogLookup;
use regex::Regex;
use serde_json::Value;
use tracing::{debug, info, warn};

use super::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::framework::{InFlight, MemoryCache};
use crate::services::library::album_ownership::AlbumOwnership;

const MAX_CACHE: u64 = 4096;

/// The only Deezer error code meaning "this genuinely does not exist". Everything
/// else, including quota (code 4) and any code we do not recognize, is treated as
/// transient. Caching an error we do not understand is exactly how one throttled
/// call turned into an album that reported zero tracks for the life of the process.
const DEFINITIVE_ERROR_CODE: i32 = 800;

/// Good answers are stable, so this only needs to be short enough that a
/// long-lived container eventually picks up catalog corrections.
const POSITIVE_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// "Deezer answered and this does not exist." Still expires, because the
/// catalog gains releases.
const NEGATIVE_TTL: Duration = Duration::from_secs(5 * 60);

/// A tracklist we know is incomplete. Usable now, refetched soon.
const PARTIAL_TTL: Duration = Duration::from_secs(10 * 60);

/// How many searches one track lookup may make: the song as asked, then written
/// another way, then the title alone. Each is one request against Deezer's shared budget,
/// and only a miss makes the next.
const TRACK_SEARCHES: usize = 3;

/// How many hits to weigh before giving up. A plain query is fuzzier than a
/// qualified one was, so it puts covers, karaoke and live takes next to the real
/// recording and the first row is not reliably the right one.
const MATCH_CANDIDATES: i32 = 5;

/// How many of an artist's top songs a track search miss looks through.
const ARTIST_TOP_SONGS: i32 = 50;

/// How many songs of an album's tracklist a track search miss looks through.
const ALBUM_TRACKS_LIMIT: i32 = 300;

/// How many releases an artist's page asks for: the catalog's page size, enough for
/// all but the longest careers.
const ARTIST_ALBUMS_LIMIT: i32 = 100;

/// What the cache holds, one variant per kind of answer. `Option` payloads keep a cached
/// "nothing" distinguishable from a cache miss. Keys carry a prefix per kind, so a key never
/// meets another kind's value.
#[derive(Clone)]
enum Cached {
    Track(Option<TrackMeta>),
    Full(Option<FullTrackMeta>),
    Candidates(Arc<CatalogCandidates>),
    Artist(Option<ArtistMeta>),
    ArtistHits(Vec<ArtistHit>),
    AlbumHits(Vec<AlbumHit>),
    Number(Option<i32>),
    Id(Option<String>),
    Text(String),
    Album(AlbumLookup),
    Chart(Arc<Vec<octo_core::charts::top_songs::CatalogTrack>>),
    Tempo(Option<f64>),
}

/// Result of one Deezer call. Deezer answers HTTP 200 even when it is refusing the
/// request, so "we parsed a document" is not the same as "the call succeeded", and
/// callers must never cache anything derived from a transient failure.
#[derive(Clone)]
struct DeezerResponse {
    doc: Option<Value>,

    /// Failed in a way that may succeed later. Nothing about this call
    /// may be written to a cache.
    transient: bool,
}

impl DeezerResponse {
    fn transient() -> Self {
        Self {
            doc: None,
            transient: true,
        }
    }

    /// The call as the clip check and a heart's questions read it: a call that may succeed later
    /// is Busy, Deezer's own "no data" is NoData, and a document is the answer.
    fn asked(self) -> DeezerAsked<Value> {
        match self {
            DeezerResponse { transient: true, .. } => DeezerAsked::Busy,
            DeezerResponse { doc: None, .. } => DeezerAsked::NoData,
            DeezerResponse { doc: Some(doc), .. } => DeezerAsked::Answer(doc),
        }
    }
}

/// A call that never answered (its task ended early) is transient, never "nothing found".
impl Default for DeezerResponse {
    fn default() -> Self {
        Self::transient()
    }
}

/// How Deezer answered one of the clip check's or a heart's questions.
#[derive(Debug, Clone, PartialEq)]
pub enum DeezerAsked<T> {
    Answer(T),
    /// Deezer's own "no data" (error code 800), or an answer with nothing in it.
    NoData,
    /// `transient`, a 429, the limiter's wait running out, a network error or a 5xx.
    Busy,
}

/// One Deezer track as the clip check reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeezerTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub duration_seconds: Option<i32>,
    pub isrc: Option<String>,
    /// The `preview` field: the signed, short-lived URL of the track's 30 s clip.
    pub clip_url: Option<String>,
}

/// Enriches external (YouTube-resolved) tracks with real album/artist metadata
/// from Deezer's public API. Keyless and no ARL: the ARL that expires on the
/// music bot is only for Deezer AUDIO; metadata endpoints are open.
///
/// Everything here is best-effort and cached: a Deezer outage, throttle, or miss
/// returns null, and callers fall back to a synthetic entity. Nothing on this
/// path ever blocks or fails playback.
///
/// Cheap to clone: the clones share one cache.
#[derive(Clone)]
pub struct DeezerMetadataService {
    inner: Arc<Inner>,
}

struct Inner {
    http: Arc<DeezerRateLimitHandler>,
    settings: Arc<SettingsStore>,
    base: String,

    // Owned rather than shared: metadata records are tens of bytes and
    // cover-art blobs are hundreds of kilobytes, so a single shared SizeLimit cannot
    // be right for both. Every entry counts as 1, so the limit is an entry count.
    cache: MemoryCache<Cached>,

    // Single-flight the album-year fetch: many tracks in one search share an album (a whole
    // album's tracks), so concurrent lookups collapse onto one HTTP call. None is a fetch that
    // failed.
    album_year_tasks: InFlight<Option<(Option<i32>, bool)>>,

    /// Requests on their way to the catalog, by cache key. The cache only answers
    /// once a request is back, so two callers asking the same thing at once each asked.
    in_flight_artists: InFlight<Vec<ArtistHit>>,
    in_flight_counts: InFlight<Option<i32>>,
    in_flight_albums: InFlight<AlbumLookup>,

    /// Calls running, by lane and address: a caller asking for what another is already
    /// asking joins it instead of spending a second request of the budget.
    in_flight_calls: InFlight<DeezerResponse>,
    /// The chart and artist top lists on their way (`deezer_charts.rs`).
    in_flight_charts: InFlight<Option<Arc<Vec<octo_core::charts::top_songs::CatalogTrack>>>>,

    /// A call did not answer since the last one that did.
    unanswered: std::sync::atomic::AtomicBool,
    /// Runs on the first answered call after one that did not: what waits for Deezer asks again.
    on_answered: parking_lot::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// Flow out of a block that can return early.
enum Flow<T> {
    Return(T),
    Done,
}

impl DeezerMetadataService {
    pub const BASE: &'static str = "https://api.deezer.com";

    pub fn new(http: Arc<DeezerRateLimitHandler>, settings: Arc<SettingsStore>) -> Self {
        Self::with_base_url(http, settings, Self::BASE)
    }

    /// A service that asks another host than api.deezer.com: a test's mock server.
    pub fn with_base_url(
        http: Arc<DeezerRateLimitHandler>,
        settings: Arc<SettingsStore>,
        base: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                http,
                settings,
                base: base.into().trim_end_matches('/').to_string(),
                cache: MemoryCache::new(MAX_CACHE),
                album_year_tasks: InFlight::new(),
                in_flight_artists: InFlight::new(),
                in_flight_counts: InFlight::new(),
                in_flight_albums: InFlight::new(),
                in_flight_calls: InFlight::new(),
                in_flight_charts: InFlight::new(),
                unanswered: std::sync::atomic::AtomicBool::new(false),
                on_answered: parking_lot::Mutex::new(None),
            }),
        }
    }

    /// Runs `answered` on the first call Deezer answers after one it did not (a `transient`
    /// answer, a 429, a 5xx, a network error): the held hearts that wait for Deezer ask again then.
    pub fn on_answered_again(&self, answered: Arc<dyn Fn() + Send + Sync>) {
        *self.inner.on_answered.lock() = Some(answered);
    }

    /// Notes how one call went: a call that did not answer is remembered, and the first answer
    /// after one runs the hook.
    fn note_answer(&self, transient: bool) {
        use std::sync::atomic::Ordering;
        if transient {
            self.inner.unanswered.store(true, Ordering::SeqCst);
            return;
        }
        if self.inner.unanswered.swap(false, Ordering::SeqCst) {
            let answered = self.inner.on_answered.lock().clone();
            if let Some(answered) = answered {
                answered();
            }
        }
    }

    fn base(&self) -> &str {
        &self.inner.base
    }

    fn get_cached(&self, key: &str) -> Option<Cached> {
        self.inner.cache.get(key)
    }

    fn put(&self, key: impl Into<String>, value: Cached, ttl: Duration) {
        self.inner.cache.set(key, value, 1, ttl);
    }

    /// Drop every cached answer. Exposed so a poisoned cache can be cleared
    /// without restarting the container.
    pub fn clear_caches(&self) {
        self.inner.cache.clear();
        self.inner.album_year_tasks.clear();
        info!("deezer metadata caches cleared");
    }

    fn track_key(artist: &str, title: &str) -> String {
        dotnet::to_lower_invariant(&format!("t|{artist}|{title}"))
    }

    /// What is already known about a track, or None when nothing is. Never makes a
    /// request, so a caller on a latency budget can complete the rows it knows without
    /// paying for the ones it does not. Shares the track key with [`Self::enrich_track`],
    /// because a lookup keyed differently from the write would silently never hit.
    pub fn cached_track(&self, artist: &str, title: &str) -> Option<TrackMeta> {
        if is_blank(artist) && is_blank(title) {
            return None;
        }
        match self.get_cached(&Self::track_key(artist, title)) {
            Some(Cached::Track(meta)) => meta,
            _ => None,
        }
    }

    /// Resolve "artist + title" to the real album + artist (name, art, year).
    /// Pass include_year=false to skip the extra album-detail call (bulk enrichment
    /// wants duration + album fast; the year is fetched lazily by the album view).
    pub async fn enrich_track(
        &self,
        artist: &str,
        title: &str,
        include_year: bool,
        background: bool,
    ) -> Option<TrackMeta> {
        self.look_up_track(artist, title, include_year, background, false)
            .await
    }

    /// [`Self::enrich_track`] for a search someone is waiting on: one track search (the first
    /// query [`Self::enrich_track`] would try), no year, on the interactive lane. A hit is the
    /// hit the full lookup would find, and is remembered as it would be. A miss is not
    /// remembered: the full lookup's other queries may still find the song, and are left to
    /// a caller that is not waiting ([`Self::enrich_track`] on the background lane).
    pub async fn enrich_track_quick(&self, artist: &str, title: &str) -> Option<TrackMeta> {
        self.look_up_track(artist, title, false, false, true).await
    }

    async fn look_up_track(
        &self,
        artist: &str,
        title: &str,
        include_year: bool,
        background: bool,
        first_query_only: bool,
    ) -> Option<TrackMeta> {
        if is_blank(artist) && is_blank(title) {
            return None;
        }
        let key = Self::track_key(artist, title);
        if let Some(Cached::Track(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut meta = None;
        // Set when the year alone could not be resolved. The rest of the record is still
        // good and is returned; it just must not be remembered, or a throttle blip would
        // be cached as "this track has no year" for the life of the entry.
        let mut year_unresolved = false;
        let attempt = async {
            let (transient, found) = if first_query_only {
                self.find_track_first_query(artist, title).await?
            } else {
                self.find_track(artist, title, background, false).await?
            };
            if transient {
                return anyhow::Ok(Flow::Return(None));
            }
            if first_query_only && found.is_none() {
                return Ok(Flow::Return(None));
            }
            if let Some((t, doc)) = found {
                let (mut alb_title, mut cover, mut art_name, mut art_img) = (None, None, None, None);
                let mut alb_id = 0;
                if let Some(alb) = try_get_property(&t, "album")? {
                    alb_title = str_prop(alb, "title")?;
                    cover = deezer_image(alb, &["cover_xl", "cover_medium"])?;
                    if let Some(aid @ Value::Number(_)) = try_get_property(alb, "id")? {
                        alb_id = get_int64(aid)?;
                    }
                }
                if let Some(art) = try_get_property(&t, "artist")? {
                    art_name = str_prop(art, "name")?;
                    art_img = deezer_image(art, &["picture_xl", "picture_medium"])?;
                }
                let duration = match try_get_property(&t, "duration")? {
                    Some(du @ Value::Number(_)) => Some(get_int32(du)?),
                    _ => None,
                };
                let mut year = None;
                if include_year && alb_id > 0 {
                    let (y, year_transient) = self.album_year_on(alb_id, background).await?;
                    // Degrade to "no year", never to "no track". Returning null here threw
                    // away an album title and duration the search had already fetched, so a
                    // throttled year left the song with no length at all.
                    year = y;
                    year_unresolved = year_transient;
                }
                meta = Some(TrackMeta {
                    album_id: (alb_id > 0).then_some(alb_id),
                    album_title: alb_title.map(str::to_string),
                    album_cover_url: cover.map(str::to_string),
                    year,
                    duration,
                    artist_name: art_name.map(str::to_string),
                    artist_image_url: art_img.map(str::to_string),
                    explicit_content: song_explicitness(doc.as_ref(), artist, title, &t)?,
                });
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer enrich track '{artist} - {title}' failed: {e}"),
        }

        // Usable now, refetched next time, so the year gets another chance.
        if year_unresolved {
            return meta;
        }

        let ttl = if meta.is_none() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::Track(meta.clone()), ttl);
        meta
    }

    /// Full track metadata for tagging a downloaded file: one track search (album,
    /// cover_xl, artist, duration, track_position, disk_number, isrc) plus one album
    /// detail call (release year, genre, total tracks, label). Cached; best-effort.
    pub async fn enrich_track_full(&self, artist: &str, title: &str) -> Option<FullTrackMeta> {
        if is_blank(artist) && is_blank(title) {
            return None;
        }
        let key = dotnet::to_lower_invariant(&format!("full|{artist}|{title}"));
        if let Some(Cached::Full(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut meta = None;
        // The album detail carries the year, genre and label. If only that call fails the
        // track itself is still worth having, so it is returned uncached rather than lost:
        // a tagger with an album title and cover art beats one with nothing.
        let mut detail_unresolved = false;
        let attempt = async {
            let (transient, found) = self.find_track(artist, title, false, false).await?;
            if transient {
                return anyhow::Ok(Flow::Return(None));
            }
            if let Some((t, _)) = found {
                let (full, unresolved) = self.build_full_meta(&t).await?;
                meta = Some(full);
                detail_unresolved = unresolved;
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer full enrich '{artist} - {title}' failed: {e}"),
        }

        // Usable now, refetched next time, so the album detail gets another chance.
        if detail_unresolved {
            return meta;
        }

        let ttl = if meta.is_none() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::Full(meta.clone()), ttl);
        meta
    }

    /// The ranked hits for one song, not only the first: every hit of the first search that
    /// finds any, in the catalog's order, with the detail calls made for the best `max`. The
    /// chooser weighs them against the other sources. A throttled catalog answers nothing and
    /// says so, and nothing from that call is remembered.
    pub async fn enrich_track_candidates(
        &self,
        artist: &str,
        title: &str,
        max: i32,
    ) -> Arc<CatalogCandidates> {
        if is_blank(artist) && is_blank(title) {
            return Arc::new(CatalogCandidates::default());
        }
        let max = max.clamp(1, MATCH_CANDIDATES);
        let key = dotnet::to_lower_invariant(&format!("cands|{artist}|{title}|{max}"));
        if let Some(Cached::Candidates(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut hits = Vec::new();
        let mut detail_unresolved = false;
        let attempt = async {
            let (transient, found) = self.find_track_hits(artist, title).await?;
            if transient {
                return anyhow::Ok(Flow::Return(()));
            }
            for hit in found.iter().take(max as usize) {
                let (meta, unresolved) = self.build_full_meta(hit).await?;
                detail_unresolved |= unresolved;
                hits.push(meta);
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(())) => {
                return Arc::new(CatalogCandidates {
                    hits: Vec::new(),
                    did_not_answer: true,
                });
            }
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer candidates '{artist} - {title}' failed: {e}"),
        }

        let empty = hits.is_empty();
        let answer = Arc::new(CatalogCandidates {
            hits,
            did_not_answer: false,
        });
        if !detail_unresolved {
            let ttl = if empty { NEGATIVE_TTL } else { POSITIVE_TTL };
            self.put(key, Cached::Candidates(Arc::clone(&answer)), ttl);
        }
        answer
    }

    /// One search hit made whole: the album's detail (year, genre, label, barcode,
    /// album artist, kind) and the track's own record (position, contributors, code, words,
    /// loudness). The second value says a detail call did not answer, so nothing is cached.
    async fn build_full_meta(&self, t: &Value) -> ElementResult<(FullTrackMeta, bool)> {
        let mut detail_unresolved = false;
        let (mut alb_title, mut cover, mut art_name) = (None, None, None);
        let mut isrc = str_prop(t, "isrc")?.map(str::to_string);
        let mut alb_id = 0;
        if let Some(alb) = try_get_property(t, "album")? {
            alb_title = str_prop(alb, "title")?.map(str::to_string);
            cover = deezer_image(alb, &["cover_xl", "cover_big", "cover_medium"])?.map(str::to_string);
            if let Some(aid @ Value::Number(_)) = try_get_property(alb, "id")? {
                alb_id = get_int64(aid)?;
            }
        }
        if let Some(art) = try_get_property(t, "artist")? {
            art_name = str_prop(art, "name")?.map(str::to_string);
        }

        let (mut year, mut total_tracks) = (None, None);
        let (mut genre, mut label, mut release_date, mut album_artist, mut record_type, mut barcode) =
            (None, None, None, None, None, None);
        if alb_id > 0 {
            let ar = self
                .get_json(&format!("{}/album/{alb_id}", self.base()), false)
                .await;
            detail_unresolved = ar.transient;
            if let Some(root) = &ar.doc {
                record_type = str_prop(root, "record_type")?.map(str::to_string);
                if let Some(album_art @ Value::Object(_)) = try_get_property(root, "artist")? {
                    album_artist = str_prop(album_art, "name")?.map(str::to_string);
                }
                release_date = str_prop(root, "release_date")?.map(str::to_string);
                if let Some(yr) = release_date.as_deref().and_then(year_prefix) {
                    year = Some(yr);
                }
                total_tracks = int(root, "nb_tracks")?;
                label = str_prop(root, "label")?.map(str::to_string);
                barcode = str_prop(root, "upc")?.map(str::to_string);
                genre = first_genre(root)?;
            }
        }

        // The search hit carries neither the track's position nor anyone but the main
        // artist, so the track number was never written (#48) and a collaboration was one
        // artist (#49). The track's own record has both.
        let mut track_number = int(t, "track_position")?;
        let mut disc_number = int(t, "disk_number")?;
        let mut contributors = None;
        let mut explicit_lyrics = match try_get_property(t, "explicit_lyrics")? {
            Some(Value::Bool(flag)) => Some(*flag),
            _ => None,
        };
        let mut explicit_content = int(t, "explicit_content_lyrics")?;
        let mut gain = None;
        let mut bpm = None;
        let mut track_id = None;
        if let Some(tid @ Value::Number(_)) = try_get_property(t, "id")? {
            let id = get_int64(tid)?;
            track_id = Some(id.to_string());
            let tr = self.get_json(&format!("{}/track/{id}", self.base()), false).await;
            detail_unresolved |= tr.transient;
            if let Some(track) = &tr.doc {
                if track_number.is_none() {
                    track_number = int(track, "track_position")?;
                }
                if disc_number.is_none() {
                    disc_number = int(track, "disk_number")?;
                }
                if isrc.is_none() {
                    isrc = str_prop(track, "isrc")?.map(str::to_string);
                }
                if let Some(Value::Bool(flag)) = try_get_property(track, "explicit_lyrics")? {
                    explicit_lyrics = Some(*flag);
                }
                explicit_content = int(track, "explicit_content_lyrics")?.or(explicit_content);
                if let Some(gn @ Value::Number(_)) = try_get_property(track, "gain")? {
                    gain = Some(get_double(gn)?);
                }
                bpm = tempo_of(track)?;
                if let Some(code) = isrc.as_deref().and_then(SongIdentity::normalize_isrc) {
                    self.put(tempo_key(&code), Cached::Tempo(bpm), POSITIVE_TTL);
                }
                if let Some(people @ Value::Array(_)) = try_get_property(track, "contributors")? {
                    let mut names: Vec<String> = Vec::new();
                    for person in enumerate_array(people)? {
                        if !matches!(str_prop(person, "role")?, None | Some("Main") | Some("Featured")) {
                            continue;
                        }
                        let Some(name) = str_prop(person, "name")?.filter(|n| !is_blank(n)) else {
                            continue;
                        };
                        if !names.iter().any(|known| dotnet::eq_ignore_case(known, name)) {
                            names.push(name.to_string());
                        }
                    }
                    contributors = Some(names);
                }
            }
        }

        let meta = FullTrackMeta {
            album_title: alb_title,
            album_cover_url: cover,
            year,
            duration: int(t, "duration")?,
            artist_name: art_name,
            track_number,
            disc_number,
            isrc,
            total_tracks,
            genre,
            label,
            release_date,
            contributors,
            album_artist_name: album_artist,
            record_type,
            title: str_prop(t, "title")?.map(str::to_string),
            track_id,
            album_id: (alb_id > 0).then(|| alb_id.to_string()),
            barcode: barcode.filter(|b| !is_blank(b)),
            explicit_lyrics,
            explicit_content,
            catalog_gain: gain,
            bpm,
        };
        Ok((meta, detail_unresolved))
    }

    /// Resolve an artist name to its Deezer name + image.
    pub async fn enrich_artist(&self, artist: &str) -> Option<ArtistMeta> {
        if is_blank(artist) {
            return None;
        }
        let key = dotnet::to_lower_invariant(&format!("a|{artist}"));
        if let Some(Cached::Artist(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut meta = None;
        let attempt = async {
            let q = escape_data_string(artist);
            let r = self
                .get_json(&format!("{}/search/artist?q={q}&limit=1", self.base()), false)
                .await;
            if r.transient {
                return anyhow::Ok(Flow::Return(None));
            }
            if let Some(a) = first_data(r.doc.as_ref())? {
                meta = Some(ArtistMeta {
                    name: str_prop(a, "name")?.map(str::to_string),
                    image_url: deezer_image(a, &["picture_xl", "picture_medium"])?.map(str::to_string),
                });
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer enrich artist '{artist}' failed: {e}"),
        }

        let ttl = if meta.is_none() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::Artist(meta.clone()), ttl);
        meta
    }

    /// Search the catalog for artists. Plain query: the artist endpoint takes a bare name
    /// and the qualified form is dead everywhere now.
    pub async fn search_artists(&self, query: &str, limit: i32) -> Vec<ArtistHit> {
        self.search_artists_on(query, limit, false).await
    }

    /// [`Self::search_artists`] on the catalog's background lane when `background` (New
    /// Releases), behind anything a listener is waiting on.
    pub async fn search_artists_on(&self, query: &str, limit: i32, background: bool) -> Vec<ArtistHit> {
        if is_blank(query) || limit <= 0 {
            return Vec::new();
        }
        let key = dotnet::to_lower_invariant(&format!("ars|{query}|{limit}"));
        if let Some(Cached::ArtistHits(cached)) = self.get_cached(&key) {
            return cached;
        }
        // An artist page names its artist and lists its albums in two requests at once, and
        // both search for the name. They share one search. It runs without any one caller:
        // a caller giving up stops waiting, and the others still get their answer.
        let this = self.clone();
        let (query, fetch_key) = (query.to_string(), key.clone());
        self.inner
            .in_flight_artists
            .run(&key, async move {
                this.fetch_artist_search(&query, limit, fetch_key, background)
                    .await
            })
            .await
    }

    async fn fetch_artist_search(
        &self,
        query: &str,
        limit: i32,
        key: String,
        background: bool,
    ) -> Vec<ArtistHit> {
        let mut hits = Vec::new();
        let attempt = async {
            let q = escape_data_string(query);
            let r = self
                .get_json(
                    &format!("{}/search/artist?q={q}&limit={limit}", self.base()),
                    background,
                )
                .await;
            // Caching an empty list on a refusal is what would make external artists
            // silently vanish from search3 for the rest of the process.
            if r.transient {
                return anyhow::Ok(Flow::Return(()));
            }
            hits = artist_hits(r.doc.as_ref())?;
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(())) => return Vec::new(),
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer artist search '{query}' failed: {e}"),
        }

        let ttl = if hits.is_empty() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::ArtistHits(hits.clone()), ttl);
        hits
    }

    /// The artists the catalog lists as related to one, most related first. Empty when it lists
    /// none or cannot be asked.
    pub async fn related_artists(&self, deezer_artist_id: &str, limit: i32) -> Vec<ArtistHit> {
        if is_blank(deezer_artist_id) || limit <= 0 {
            return Vec::new();
        }
        let key = dotnet::to_lower_invariant(&format!("rel|{deezer_artist_id}|{limit}"));
        if let Some(Cached::ArtistHits(cached)) = self.get_cached(&key) {
            return cached;
        }
        let r = self
            .get_json(
                &format!(
                    "{}/artist/{}/related?limit={limit}",
                    self.base(),
                    escape_data_string(deezer_artist_id)
                ),
                false,
            )
            .await;
        if r.transient {
            return Vec::new();
        }
        let mut hits = artist_hits(r.doc.as_ref()).unwrap_or_else(|e| {
            debug!("deezer related artists of {deezer_artist_id} failed: {e}");
            Vec::new()
        });
        hits.truncate(usize::try_from(limit).unwrap_or(0));
        let ttl = if hits.is_empty() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::ArtistHits(hits.clone()), ttl);
        hits
    }

    /// Search the album catalog. Single-track "albums" are dropped: a plain
    /// artist query returns a lot of them and they crowd out real records.
    pub async fn search_albums(&self, query: &str, limit: i32, keep_singles: bool) -> Vec<AlbumHit> {
        if is_blank(query) || limit <= 0 {
            return Vec::new();
        }
        let key = dotnet::to_lower_invariant(&format!(
            "as|{query}|{limit}|{}",
            if keep_singles { "True" } else { "False" }
        ));
        if let Some(Cached::AlbumHits(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut hits = Vec::new();
        let attempt = async {
            let q = escape_data_string(query);
            let r = self
                .get_json(
                    &format!("{}/search/album?q={q}&limit={limit}", self.base()),
                    false,
                )
                .await;
            // Caching an empty list here is what would make external albums silently
            // vanish from search3 for the rest of the process.
            if r.transient {
                return anyhow::Ok(Flow::Return(()));
            }
            if let Some(data) = data_array(r.doc.as_ref())? {
                for a in enumerate_array(data)? {
                    let id = match try_get_property(a, "id")? {
                        Some(aid @ Value::Number(_)) => Some(get_int64(aid)?.to_string()),
                        _ => None,
                    };
                    let title = str_prop(a, "title")?;
                    let (Some(id), Some(title)) = (id, title.filter(|t| !is_blank(t))) else {
                        continue;
                    };

                    let record_type = str_prop(a, "record_type")?;
                    let track_count = int(a, "nb_tracks")?.unwrap_or(0);
                    // Search lists albums; a one- or two-track single is a song there. The cover
                    // upgrade keeps them: a library of singles has their covers to replace.
                    if !keep_singles
                        && record_type.is_some_and(|t| dotnet::eq_ignore_case(t, "single"))
                        && track_count <= 2
                    {
                        continue;
                    }

                    let artist = match try_get_property(a, "artist")? {
                        Some(art) => str_prop(art, "name")?,
                        None => None,
                    };
                    hits.push(AlbumHit {
                        deezer_id: id,
                        title: title.to_string(),
                        artist: artist.unwrap_or("").to_string(),
                        cover_url: deezer_image(a, &["cover_xl", "cover_medium"])?.map(str::to_string),
                        year: None,
                        track_count,
                        record_type: record_type.map(str::to_string),
                        release_date: None,
                        explicit_lyrics: bool_prop(a, "explicit_lyrics")?,
                    });
                }
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(())) => return Vec::new(),
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer album search '{query}' failed: {e}"),
        }

        let ttl = if hits.is_empty() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::AlbumHits(hits.clone()), ttl);
        hits
    }

    /// An artist's own releases for their page: albums, then EPs, then singles, then their own
    /// compilations, newest first within each, the way the apps group a discography. One copy
    /// of each title, whatever its type: the catalog lists a clean and an explicit copy of many,
    /// and an album can share its title with its own single or EP. The album is the one kept.
    /// Two releases of one title would also open as one, since an outside album's id is made
    /// from the artist and title, and the second would take over the first's. Singles used to
    /// be left out unless there was nothing else, which hid half of a career that is mostly
    /// singles; grouped after the records, they no longer bury them. Repeat editions of one
    /// record (deluxe, remastered, explicit and clean copies) are one row (see
    /// [`fold_editions`]); a server that plays clean songs only shows the clean copy of a record
    /// that has one. The artist's name is not on this listing, so every hit carries the one
    /// given.
    pub async fn get_artist_albums(&self, deezer_artist_id: &str, artist_name: &str) -> Vec<AlbumHit> {
        let releases = self
            .get_artist_albums_on(deezer_artist_id, artist_name, false, 1)
            .await;
        let clean_only = self.inner.settings.current().subsonic.explicit_filter == ExplicitFilter::CleanOnly;
        let mut hits = fold_editions_preferring(releases, false, clean_only);
        sort_releases(&mut hits);
        hits
    }

    /// [`Self::get_artist_albums`] on the catalog's background lane when `background`, reading
    /// up to `pages` of the catalog's pages of 100 for a long career. The catalog lists albums
    /// before singles, so a new single of an artist with more than a page of releases is on a
    /// later page; New Releases reads up to three. Every edition the catalog lists is kept: a
    /// new deluxe edition is news there.
    pub async fn get_artist_albums_on(
        &self,
        deezer_artist_id: &str,
        artist_name: &str,
        background: bool,
        pages: i32,
    ) -> Vec<AlbumHit> {
        if is_blank(deezer_artist_id) {
            return Vec::new();
        }
        let pages = pages.clamp(1, 5);
        // A server that plays clean songs only keeps the clean copy of a title that has one.
        let clean_only = self.inner.settings.current().subsonic.explicit_filter == ExplicitFilter::CleanOnly;
        let key = dotnet::to_lower_invariant(&format!(
            "ara|{deezer_artist_id}{}{}",
            if pages > 1 {
                format!("|p{pages}")
            } else {
                String::new()
            },
            if clean_only { "|clean" } else { "" }
        ));
        if let Some(Cached::AlbumHits(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut releases: Vec<AlbumHit> = Vec::new();
        // Where each title sits in the list, so a better copy found later takes its place.
        let mut by_title: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut total: Option<i32> = None;
        let mut listed = 0usize;
        let mut partial = false;
        let attempt = async {
            let id = escape_data_string(deezer_artist_id);
            for page in 0..pages {
                let index = if page > 0 {
                    format!("&index={}", page * ARTIST_ALBUMS_LIMIT)
                } else {
                    String::new()
                };
                let r = self
                    .get_json(
                        &format!(
                            "{}/artist/{id}/albums?limit={ARTIST_ALBUMS_LIMIT}{index}",
                            self.base()
                        ),
                        background,
                    )
                    .await;
                // Caching an empty list on a refusal would leave the artist's page empty for
                // hours. A later page refused keeps the first pages, briefly.
                if r.transient {
                    if page == 0 {
                        return anyhow::Ok(Flow::Return(()));
                    }
                    partial = true;
                    break;
                }
                let Some(root) = &r.doc else { break };
                let Some(data) = data_array(Some(root))? else {
                    break;
                };
                if total.is_none() {
                    total = int(root, "total")?;
                }
                let count = array_length(data)?;
                listed += count;
                for a in enumerate_array(data)? {
                    let album_id = match try_get_property(a, "id")? {
                        Some(aid @ Value::Number(_)) => Some(get_int64(aid)?.to_string()),
                        _ => None,
                    };
                    let title = str_prop(a, "title")?;
                    let (Some(album_id), Some(title)) = (album_id, title.filter(|t| !is_blank(t))) else {
                        continue;
                    };

                    let record_type = str_prop(a, "record_type")?.map(str::to_string);
                    let released = str_prop(a, "release_date")?;
                    let year = released.and_then(year_prefix).filter(|&y| y > 0);
                    let hit = AlbumHit {
                        deezer_id: album_id,
                        title: title.to_string(),
                        artist: artist_name.to_string(),
                        cover_url: deezer_image(a, &["cover_xl", "cover_medium"])?.map(str::to_string),
                        year,
                        track_count: int(a, "nb_tracks")?.unwrap_or(0),
                        record_type,
                        release_date: released
                            .filter(|date| date.encode_utf16().count() >= 10)
                            .map(|date| {
                                String::from_utf16_lossy(&date.encode_utf16().take(10).collect::<Vec<_>>())
                            }),
                        explicit_lyrics: bool_prop(a, "explicit_lyrics")?,
                    };

                    let title_key = SongIdentity::key(title);
                    match by_title.get(&title_key) {
                        None => {
                            by_title.insert(title_key, releases.len());
                            releases.push(hit);
                        }
                        Some(&at) => {
                            let (rank, known) = (
                                release_rank(hit.record_type.as_deref()),
                                release_rank(releases[at].record_type.as_deref()),
                            );
                            if rank < known
                                // Of a clean and an explicit copy of one release, the explicit
                                // one: the original, and the one a search row of its songs
                                // stands for. A clean only server keeps the clean one.
                                || (rank == known
                                    && (hit.explicit_lyrics == Some(true)) != clean_only
                                    && (releases[at].explicit_lyrics == Some(true)) == clean_only)
                            {
                                releases[at] = hit;
                            }
                        }
                    }
                }
                match total {
                    Some(known) if count > 0 && listed < usize::try_from(known).unwrap_or(0) => {}
                    _ => break,
                }
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(())) => return Vec::new(),
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer artist albums '{deezer_artist_id}' failed: {e}"),
        }

        // Said out loud, because a page that stops short otherwise looks like a whole career.
        if let Some(all) = total
            && all >= 0
            && all as usize > listed
        {
            warn!(
                "deezer artist {deezer_artist_id} ('{artist_name}') has {all} releases; the page lists the first {listed}"
            );
        }

        let mut hits = releases;
        sort_releases(&mut hits);
        let ttl = if partial {
            PARTIAL_TTL
        } else if hits.is_empty() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::AlbumHits(hits.clone()), ttl);
        hits
    }

    /// A track count already known for an album, without asking the catalog. The outer
    /// `Option` is whether it is known; the inner one is the count, which may be known absent.
    pub fn try_known_track_count(&self, deezer_id: &str) -> Option<Option<i32>> {
        match self.get_cached(&format!("tc|{deezer_id}")) {
            Some(Cached::Number(count)) => Some(count),
            _ => None,
        }
    }

    /// How many tracks a catalog album has, from the album's own record, or None when it cannot
    /// be told. An artist's listing leaves the count out, and a page showing "0 songs" on every
    /// album reads as empty albums (some clients hide them). Asked in the interactive lane:
    /// someone is looking at the page, and the background lane is kept full by cache warming,
    /// which turned every one of these away. The caller keeps the number asked small.
    pub async fn album_track_count(&self, deezer_id: &str) -> Option<i32> {
        let key = format!("tc|{deezer_id}");
        if let Some(Cached::Number(cached)) = self.get_cached(&key) {
            return cached;
        }
        // Two visits to one page at once ask for the same albums; each album is asked once.
        let this = self.clone();
        let (id, fetch_key) = (deezer_id.to_string(), key.clone());
        self.inner
            .in_flight_counts
            .run(
                &key,
                async move { this.fetch_album_track_count(&id, fetch_key).await },
            )
            .await
    }

    async fn fetch_album_track_count(&self, deezer_id: &str, key: String) -> Option<i32> {
        let attempt = async {
            let r = self
                .get_json(
                    &format!("{}/album/{}", self.base(), escape_data_string(deezer_id)),
                    false,
                )
                .await;
            if r.transient {
                return anyhow::Ok(None);
            }
            let count = match &r.doc {
                Some(root) => int(root, "nb_tracks")?,
                None => None,
            };
            let ttl = if count.is_none() {
                NEGATIVE_TTL
            } else {
                POSITIVE_TTL
            };
            self.put(key, Cached::Number(count), ttl);
            Ok(count)
        };
        match attempt.await {
            Ok(count) => count,
            Err(e) => {
                debug!("deezer album {deezer_id} track count failed: {e}");
                None
            }
        }
    }

    /// Resolve an artist + album name to a Deezer album id. Needed because album
    /// ids minted from a song row carry no Deezer id, so the name is all we have.
    pub async fn find_album_id(&self, artist: &str, album: &str) -> Option<String> {
        if is_blank(album) {
            return None;
        }
        let key = dotnet::to_lower_invariant(&format!("ai|{artist}|{album}"));
        if let Some(Cached::Id(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut id = None;
        let attempt = async {
            // The question a heart asks for its catalog album: one builder, so the two never part.
            let Some(path) = deezer_album_search_url(artist, album) else {
                return anyhow::Ok(Flow::Return(None));
            };
            let r = self.get_json(&format!("{}/{path}", self.base()), false).await;
            if r.transient {
                return anyhow::Ok(Flow::Return(None));
            }
            if let Some(a) = best_match(r.doc.as_ref(), artist, album)?
                && let Some(aid @ Value::Number(_)) = try_get_property(&a, "id")?
            {
                id = Some(get_int64(aid)?.to_string());
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer album id lookup '{artist} - {album}' failed: {e}"),
        }

        let ttl = if id.is_none() { NEGATIVE_TTL } else { POSITIVE_TTL };
        self.put(key, Cached::Id(id.clone()), ttl);
        id
    }

    /// Album detail plus its full tracklist, ordered by disc then track position.
    /// One bounded request per resource; a release larger than the cap is reported as
    /// truncated rather than silently presented as complete.
    pub async fn get_album_detail(&self, deezer_id: &str) -> Option<AlbumDetail> {
        self.look_up_album_detail(deezer_id).await.detail
    }

    /// [`Self::get_album_detail`], saying also why there is no detail when there is none:
    /// Deezer has no such album, it has the album but no tracks for it, or it did not answer
    /// this time. Only the last may come right on its own a moment later.
    pub async fn look_up_album_detail(&self, deezer_id: &str) -> AlbumLookup {
        self.look_up_album_detail_on(deezer_id, false).await
    }

    /// The album's detail when it is already cached, without asking Deezer.
    pub fn cached_album_detail(&self, deezer_id: &str) -> Option<AlbumLookup> {
        if is_blank(deezer_id) {
            return None;
        }
        match self.get_cached(&format!("ad|{deezer_id}")) {
            Some(Cached::Album(cached)) => Some(cached),
            _ => None,
        }
    }

    /// [`Self::look_up_album_detail`] from the cache, or from one request shared by every caller
    /// asking at once. The request runs without any one caller, so a search that stops waiting
    /// still leaves the answer cached for the next one. `background` puts it behind anything a
    /// listener is waiting on.
    pub async fn shared_album_detail(&self, deezer_id: &str, background: bool) -> AlbumLookup {
        if is_blank(deezer_id) {
            return AlbumLookup {
                detail: None,
                answer: AlbumAnswer::NoSuchAlbum,
            };
        }
        let cache_key = format!("ad|{deezer_id}");
        if let Some(Cached::Album(cached)) = self.get_cached(&cache_key) {
            return cached;
        }
        let this = self.clone();
        let id = deezer_id.to_string();
        self.inner
            .in_flight_albums
            .run(&cache_key, async move {
                this.look_up_album_detail_on(&id, background).await
            })
            .await
    }

    /// [`Self::look_up_album_detail`] on the catalog's background lane when `background`.
    pub async fn look_up_album_detail_on(&self, deezer_id: &str, background: bool) -> AlbumLookup {
        if is_blank(deezer_id) {
            return AlbumLookup {
                detail: None,
                answer: AlbumAnswer::NoSuchAlbum,
            };
        }
        let cache_key = format!("ad|{deezer_id}");
        if let Some(Cached::Album(cached)) = self.get_cached(&cache_key) {
            return cached;
        }
        let unavailable = AlbumLookup {
            detail: None,
            answer: AlbumAnswer::Unavailable,
        };

        let mut detail = None;
        // A tracklist we know is short gets a shorter life than a complete one, so a
        // truncated or partly-skipped album repairs itself instead of sticking.
        let mut partial = false;
        let attempt = async {
            let (mut title, mut artist, mut genre, mut label, mut cover) = (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            );
            let mut record_type = None;
            let mut album_explicit = None;
            let mut year = None;
            // Kept from the album call: this is what tells an empty tracklist apart from an
            // album that genuinely has no tracks.
            let mut nb_tracks = None;

            // Both calls at once: the tracklist does not depend on the album's answer, and an
            // album opened or counted from a search waits on the slower of the two rather than on
            // both in turn. When the album call says there is nothing, the tracklist's answer is
            // dropped unread.
            let album_url = format!("{}/album/{deezer_id}", self.base());
            let tracks_url = format!("{}/album/{deezer_id}/tracks?limit=300", self.base());
            let (r, tr) = tokio::join!(
                self.get_json(&album_url, background),
                self.get_json(&tracks_url, background)
            );
            {
                if r.transient {
                    return anyhow::Ok(Flow::Return(unavailable.clone()));
                }
                if let Some(root) = &r.doc {
                    nb_tracks = int(root, "nb_tracks")?;
                    title = str_prop(root, "title")?.unwrap_or("").to_string();
                    cover = deezer_image(root, &["cover_xl", "cover_medium"])?
                        .unwrap_or("")
                        .to_string();
                    label = str_prop(root, "label")?.unwrap_or("").to_string();
                    record_type = str_prop(root, "record_type")?.map(str::to_string);
                    album_explicit = catalog_explicit(root)?;
                    if let Some(yr) = str_prop(root, "release_date")?.and_then(year_prefix) {
                        year = Some(yr);
                    }
                    if let Some(art) = try_get_property(root, "artist")? {
                        artist = str_prop(art, "name")?.unwrap_or("").to_string();
                    }
                    if let Some(name) = first_genre(root)? {
                        genre = name;
                    }
                }
            }

            // Deezer answered and there is no such album. Cacheable, but not forever.
            if is_blank(&title) {
                let none = AlbumLookup {
                    detail: None,
                    answer: AlbumAnswer::NoSuchAlbum,
                };
                self.put(cache_key.clone(), Cached::Album(none.clone()), NEGATIVE_TTL);
                return Ok(Flow::Return(none));
            }

            let mut tracks = Vec::new();
            {
                // The album call can succeed while the tracklist call is throttled. That
                // built a perfectly valid AlbumDetail carrying title, year and genre with
                // an empty tracklist, cached it permanently, and is why getAlbum reported
                // songCount 0 forever while still showing real metadata.
                if tr.transient {
                    return Ok(Flow::Return(unavailable.clone()));
                }
                if let Some(root) = &tr.doc
                    && let Some(data) = data_array(Some(root))?
                {
                    for t in enumerate_array(data)? {
                        let Some(t_title) = str_prop(t, "title")?.filter(|t| !is_blank(t)) else {
                            continue;
                        };
                        let t_artist = match try_get_property(t, "artist")? {
                            Some(ta) => str_prop(ta, "name")?,
                            None => None,
                        };
                        tracks.push(AlbumTrack {
                            title: t_title.to_string(),
                            artist: t_artist.map_or_else(|| artist.clone(), str::to_string),
                            duration: int(t, "duration")?,
                            track_position: int(t, "track_position")?,
                            disc_number: int(t, "disk_number")?,
                            isrc: str_prop(t, "isrc")?.map(str::to_string),
                            rank: int(t, "rank")?,
                            explicit_content: catalog_explicit(t)?,
                        });
                    }

                    let total = int(root, "total")?;
                    if let Some(n) = total
                        && n >= 0
                        && n as usize > tracks.len()
                    {
                        partial = true;
                        warn!(
                            "deezer album '{title}' ({deezer_id}) returned {} of {n} tracks; tracklist is truncated",
                            tracks.len()
                        );
                    }
                }
            }

            // An empty tracklist on an album Deezer says HAS tracks is a failure, not an
            // answer. Absent nb_tracks counts as "has tracks": testing `nb_tracks > 0` alone
            // would let the empty result through and cache it exactly as before.
            if tracks.is_empty() && nb_tracks.is_none_or(|n| n > 0) {
                let expected =
                    nb_tracks.map_or_else(|| "an unknown number of".to_string(), |n| n.to_string());
                warn!(
                    "deezer album '{title}' ({deezer_id}) reports {expected} track(s) but returned none; not caching"
                );
                return Ok(Flow::Return(AlbumLookup {
                    detail: None,
                    answer: AlbumAnswer::NoTracks,
                }));
            }

            // Fewer tracks than the album claims, e.g. entries skipped for a blank title.
            if let Some(expected) = nb_tracks
                && (tracks.len() as i64) < i64::from(expected)
            {
                partial = true;
            }

            tracks.sort_by(|a, b| {
                a.disc_number
                    .unwrap_or(1)
                    .cmp(&b.disc_number.unwrap_or(1))
                    .then_with(|| {
                        a.track_position
                            .unwrap_or(i32::MAX)
                            .cmp(&b.track_position.unwrap_or(i32::MAX))
                    })
            });

            detail = Some(AlbumDetail {
                deezer_id: deezer_id.to_string(),
                title,
                artist,
                cover_url: (!cover.is_empty()).then_some(cover),
                year,
                genre: (!genre.is_empty()).then_some(genre),
                label: (!label.is_empty()).then_some(label),
                tracks,
                record_type,
                explicit_content: album_explicit,
            });
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer album detail {deezer_id} failed: {e}"),
        }

        let ttl = match (&detail, partial) {
            (None, _) => NEGATIVE_TTL,
            (Some(_), true) => PARTIAL_TTL,
            (Some(_), false) => POSITIVE_TTL,
        };
        let lookup = match detail {
            None => unavailable,
            Some(detail) => AlbumLookup {
                detail: Some(detail),
                answer: AlbumAnswer::Found,
            },
        };
        self.put(cache_key, Cached::Album(lookup.clone()), ttl);
        lookup
    }

    /// The album's release year, and whether it could not be told this time. Shared across
    /// concurrent callers for the same album id (single-flight).
    async fn album_year_on(&self, album_id: i64, background: bool) -> ElementResult<(Option<i32>, bool)> {
        if let Some(Cached::Number(year)) = self.get_cached(&format!("y|{album_id}")) {
            return Ok((year, false));
        }
        let this = self.clone();
        let answer = self
            .inner
            .album_year_tasks
            .run(&album_id.to_string(), async move {
                this.fetch_album_year(album_id, background).await.ok()
            })
            .await;
        // The fetch failed (an album record of the wrong shape): the awaiting caller gets the
        // error.
        answer.ok_or(octo_core::json::element::ElementError::Format(
            "the album's release year",
        ))
    }

    /// An album's release year when it is already known: `Some(None)` when the album has none,
    /// None when it was never looked up.
    pub fn cached_album_year(&self, album_id: i64) -> Option<Option<i32>> {
        match self.get_cached(&format!("y|{album_id}")) {
            Some(Cached::Number(year)) => Some(year),
            _ => None,
        }
    }

    /// Looks an album's release year up on the background lane and remembers it, for the
    /// rows a search sent without one.
    pub async fn warm_album_year(&self, album_id: i64) -> Option<i32> {
        if album_id <= 0 {
            return None;
        }
        self.album_year_on(album_id, true)
            .await
            .ok()
            .and_then(|(year, _)| year)
    }

    /// Looks an album's release year up on the interactive lane and remembers it where
    /// [`Self::warm_album_year`] does, for a single song someone is waiting on.
    pub async fn album_year_now(&self, album_id: i64) -> Option<i32> {
        if album_id <= 0 {
            return None;
        }
        self.album_year_on(album_id, false)
            .await
            .ok()
            .and_then(|(year, _)| year)
    }

    async fn fetch_album_year(&self, album_id: i64, background: bool) -> ElementResult<(Option<i32>, bool)> {
        let r = self
            .get_json(&format!("{}/album/{album_id}", self.base()), background)
            .await;

        // This used to be a raw indexer write after a bare catch, so it bypassed the
        // cache helper entirely and a throttled year lookup stuck permanently.
        if r.transient {
            return Ok((None, true));
        }

        let rd = match &r.doc {
            Some(root) => str_prop(root, "release_date")?,
            None => None,
        };
        let year = rd.and_then(year_prefix);
        let ttl = if year.is_none() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(format!("y|{album_id}"), Cached::Number(year), ttl);
        Ok((year, false))
    }

    /// An album's barcode (UPC), which names one exact release in every store, so the cover
    /// upgrade can find the same release at Apple in a batch instead of one search an album.
    pub async fn get_album_upc(&self, deezer_id: &str) -> Option<String> {
        if is_blank(deezer_id) {
            return None;
        }
        let key = format!("upc|{deezer_id}");
        if let Some(Cached::Text(cached)) = self.get_cached(&key) {
            return (!cached.is_empty()).then_some(cached);
        }
        let attempt = async {
            let r = self
                .get_json(
                    &format!("{}/album/{}", self.base(), escape_data_string(deezer_id)),
                    false,
                )
                .await;
            if r.transient {
                return anyhow::Ok(None);
            }
            let upc = match &r.doc {
                Some(root) => str_prop(root, "upc")?.map(str::to_string),
                None => None,
            };
            let upc = upc.filter(|u| !u.is_empty());
            let ttl = if upc.is_none() { NEGATIVE_TTL } else { POSITIVE_TTL };
            self.put(key.clone(), Cached::Text(upc.clone().unwrap_or_default()), ttl);
            Ok(upc)
        };
        match attempt.await {
            Ok(upc) => upc,
            Err(e) => {
                debug!("deezer album {deezer_id} barcode failed: {e}");
                None
            }
        }
    }

    /// One track by its catalog id (`track/<id>`), with the URL of its 30 s clip as Deezer gave
    /// it. Asked on the interactive lane with its permit taken before the request's timer starts,
    /// and never cached: the clip's URL is signed and soon runs out.
    pub async fn clip_track(&self, id: &str) -> DeezerAsked<DeezerTrack> {
        let id = id.trim();
        if id.is_empty() {
            return DeezerAsked::NoData;
        }
        let url = format!("{}/track/{}", self.base(), escape_data_string(id));
        read_track(self.get_json_after_permit(&url, false).await.asked())
    }

    /// The track Deezer files under an ISRC (`track/isrc:<CODE>`), asked as [`Self::clip_track`]
    /// is. NoData, without asking, for a value that is not an ISRC.
    pub async fn clip_track_by_isrc(&self, isrc: &str) -> DeezerAsked<DeezerTrack> {
        let Some(code) = SongIdentity::normalize_isrc(isrc) else {
            return DeezerAsked::NoData;
        };
        let url = format!("{}/track/isrc:{code}", self.base());
        read_track(self.get_json_after_permit(&url, false).await.asked())
    }

    /// The track [`Self::enrich_track_full`] finds for an artist and title, by its own searches,
    /// then that track asked as [`Self::clip_track`] is. Every question takes its permit before
    /// its timer starts. A search answer that cannot be read is NoData.
    pub async fn clip_track_search(&self, artist: &str, title: &str) -> DeezerAsked<DeezerTrack> {
        if is_blank(artist) && is_blank(title) {
            return DeezerAsked::NoData;
        }
        let found = match self.find_track(artist, title, false, true).await {
            Ok((true, _)) => return DeezerAsked::Busy,
            Ok((false, found)) => found,
            Err(e) => {
                debug!("deezer clip track search '{artist} - {title}' could not be read: {e}");
                return DeezerAsked::NoData;
            }
        };
        match found.as_ref().map(|(hit, _)| number_id(hit)) {
            Some(Ok(Some(id))) => self.clip_track(&id).await,
            _ => DeezerAsked::NoData,
        }
    }

    /// One catalog album as a heart's rows hold it: the album (`album/<id>`, with its barcode,
    /// record type and track count) and its tracks (`album/<id>/tracks?limit=300`, each with its
    /// id), the two asked at once, each with its permit taken before its timer starts. NoData
    /// when Deezer has no such album or lists no tracks for it; Busy when either call did not
    /// answer. Not cached here: the heart keeps its own rows.
    pub async fn album_rows(
        &self,
        album_id: &str,
        background: bool,
    ) -> DeezerAsked<(CatalogAlbum, Vec<CatalogTrack>)> {
        if is_blank(album_id) {
            return DeezerAsked::NoData;
        }
        let album_url = format!("{}/{}", self.base(), deezer_album_url(album_id));
        let tracks_url = format!("{}/{}", self.base(), deezer_album_tracks_url(album_id));
        let (album, tracks) = tokio::join!(
            self.get_json_after_permit(&album_url, background),
            self.get_json_after_permit(&tracks_url, background)
        );
        let album = match album.asked() {
            DeezerAsked::Answer(doc) => read_catalog_album(&doc),
            DeezerAsked::NoData => None,
            DeezerAsked::Busy => return DeezerAsked::Busy,
        };
        let Some(album) = album else {
            return DeezerAsked::NoData;
        };
        match tracks.asked() {
            DeezerAsked::Answer(doc) => {
                // A tracklist Deezer cut short reads as an empty one, and a heart keeps the rows
                // it finds for good: an album with no tracks is never an answer.
                let tracks = read_catalog_tracks(&doc);
                if tracks.is_empty() {
                    DeezerAsked::NoData
                } else {
                    DeezerAsked::Answer((album, tracks))
                }
            }
            DeezerAsked::NoData => DeezerAsked::NoData,
            DeezerAsked::Busy => DeezerAsked::Busy,
        }
    }

    /// The albums Deezer's album search finds for an artist and album title, in search order:
    /// [`Self::find_album_id`]'s own question, asked with its permit taken before its timer
    /// starts. NoData for a blank album title or a search that finds no album.
    pub async fn album_search_hits(
        &self,
        artist: &str,
        album: &str,
        background: bool,
    ) -> DeezerAsked<Vec<CatalogAlbumHit>> {
        let Some(path) = deezer_album_search_url(artist, album) else {
            return DeezerAsked::NoData;
        };
        let url = format!("{}/{path}", self.base());
        match self.get_json_after_permit(&url, background).await.asked() {
            DeezerAsked::Answer(doc) => {
                let hits = read_album_search(&doc);
                if hits.is_empty() {
                    DeezerAsked::NoData
                } else {
                    DeezerAsked::Answer(hits)
                }
            }
            DeezerAsked::NoData => DeezerAsked::NoData,
            DeezerAsked::Busy => DeezerAsked::Busy,
        }
    }

    async fn get_json(&self, url: &str, background: bool) -> DeezerResponse {
        self.call_json(url, background, false).await
    }

    /// [`Self::get_json`] with the permit taken before the request's timer starts, so a busy
    /// queue is waited out, never read as a call that did not answer.
    async fn get_json_after_permit(&self, url: &str, background: bool) -> DeezerResponse {
        self.call_json(url, background, true).await
    }

    async fn call_json(&self, url: &str, background: bool, after_permit: bool) -> DeezerResponse {
        let this = self.clone();
        let address = url.to_string();
        // A call that waits out the queue never joins one that may give up inside it.
        let key = if after_permit {
            format!("{background}|after-permit|{url}")
        } else {
            format!("{background}|{url}")
        };
        self.inner
            .in_flight_calls
            .run(&key, async move {
                this.fetch_json(&address, background, after_permit).await
            })
            .await
    }

    async fn fetch_json(&self, url: &str, background: bool, after_permit: bool) -> DeezerResponse {
        let response = self.fetch_json_once(url, background, after_permit).await;
        self.note_answer(response.transient);
        response
    }

    async fn fetch_json_once(&self, url: &str, background: bool, after_permit: bool) -> DeezerResponse {
        // Genre names in album payloads localize to the caller's IP country
        // unless this header pins them. Read per request, so a settings
        // change reaches the next lookup without a restart.
        let language = accept_language_header::header_value(&self.inner.settings.current().metadata);
        let asked = if after_permit {
            self.inner
                .http
                .get_after_permit(url, background, language.as_deref())
                .await
        } else {
            self.inner.http.get(url, background, language.as_deref()).await
        };
        let answer = match asked {
            Ok(answer) => answer,
            Err(e) => {
                debug!("deezer request {url} failed: {e}");
                return DeezerResponse::transient();
            }
        };
        // A 429 here is usually our OWN limiter shedding load rather than Deezer's,
        // and either way it is transient, so nothing derived from it is cached.
        if !answer.is_success() {
            return DeezerResponse::transient();
        }

        let doc: Value = match serde_json::from_str(&answer.text()) {
            Ok(doc) => doc,
            Err(e) => {
                debug!("deezer request {url} failed: {e}");
                return DeezerResponse::transient();
            }
        };

        // Deezer reports throttling as 200 + {"error":{"code":4,...}}, which parses
        // perfectly and then reads as "the album has no tracks". Catching it here is
        // what stops a quota blip becoming permanent cached state.
        if let Some(err @ Value::Object(_)) = doc.get("error") {
            let read = || -> ElementResult<(Option<i32>, Option<&str>, Option<&str>)> {
                Ok((
                    int(err, "code")?,
                    str_prop(err, "type")?,
                    str_prop(err, "message")?,
                ))
            };
            let (code, kind, message) = match read() {
                Ok(fields) => fields,
                Err(e) => {
                    debug!("deezer request {url} failed: {e}");
                    return DeezerResponse::transient();
                }
            };
            let definitive = code == Some(DEFINITIVE_ERROR_CODE);
            warn!(
                "deezer refused {url}: {} \"{}\" (code {}, treated as {})",
                kind.unwrap_or("(null)"),
                message.unwrap_or("(null)"),
                code.map_or_else(|| "(null)".to_string(), |c| c.to_string()),
                if definitive { "definitive" } else { "transient" }
            );
            return DeezerResponse {
                doc: None,
                transient: !definitive,
            };
        }

        DeezerResponse {
            doc: Some(doc),
            transient: false,
        }
    }

    /// The first track search hit that is this song, trying the song as asked and then the
    /// other ways `SongIdentity::query_variants` writes it ("suicideboys SUICIDE" for
    /// "$uicideboy$ $UICIDE", the title without its guests, the primary artist, the title
    /// alone). Every hit is judged against the song as asked, so a looser query never means a
    /// looser match. The first value says a search did not answer: nothing may be cached.
    ///
    /// Track search leaves out some songs the catalog has: one listed only on its own album,
    /// or one whose search shows only a demo while the original is among the artist's top
    /// songs. A miss gets one bounded extra step for those: the album named after the song,
    /// then the top songs of the artist the searches named, at most four more requests.
    /// The first query [`Self::find_track`] tries, alone, on the interactive lane.
    async fn find_track_first_query(
        &self,
        artist: &str,
        title: &str,
    ) -> ElementResult<(bool, Option<(Value, Option<Value>)>)> {
        let Some(variant) = SongIdentity::query_variants(title, artist).into_iter().next() else {
            return Ok((false, None));
        };
        let url = format!(
            "{}/search?q={}&limit={MATCH_CANDIDATES}",
            self.base(),
            escape_data_string(&variant.text())
        );
        let r = self.get_json(&url, false).await;
        if r.transient {
            return Ok((true, None));
        }
        Ok((
            false,
            best_match(r.doc.as_ref(), artist, title)?.map(|hit| (hit, r.doc)),
        ))
    }

    /// Deezer's own track search for a free-text query, in Deezer's order: each hit's artist
    /// and title, its track remembered as [`Self::enrich_track`] would remember it (without a
    /// year) so the rows built from it need no lookup of their own. None when Deezer could not
    /// answer.
    pub async fn search_tracks(&self, query: &str, limit: i32) -> Option<Vec<(String, String)>> {
        if is_blank(query) || limit <= 0 {
            return Some(Vec::new());
        }
        let url = format!(
            "{}/search?q={}&limit={limit}",
            self.base(),
            escape_data_string(query.trim())
        );
        let r = self.get_json(&url, false).await;
        if r.transient {
            return None;
        }
        let read = || -> ElementResult<Vec<(String, String)>> {
            let mut found = Vec::new();
            let Some(data) = data_array(r.doc.as_ref())? else {
                return Ok(found);
            };
            // A song is explicit when any hit of it in the same version is: each hit's song and
            // version key read once, rather than every hit compared with every other.
            let song_key = |hit: &Value| -> ElementResult<Option<String>> {
                let title = str_prop(hit, "title")?.unwrap_or("");
                let artist = match try_get_property(hit, "artist")? {
                    Some(a) => str_prop(a, "name")?.unwrap_or(""),
                    None => "",
                };
                Ok((!is_blank(title)).then(|| SongIdentity::match_key(artist, title)))
            };
            let mut explicit_songs = std::collections::HashSet::new();
            for hit in enumerate_array(data)? {
                if catalog_explicit(hit)? == Some(explicit_status::EXPLICIT)
                    && let Some(key) = song_key(hit)?
                {
                    explicit_songs.insert(key);
                }
            }
            for hit in enumerate_array(data)? {
                let Some(title) = str_prop(hit, "title")?.filter(|t| !is_blank(t)) else {
                    continue;
                };
                let artist = match try_get_property(hit, "artist")? {
                    Some(a) => str_prop(a, "name")?,
                    None => None,
                };
                let Some(artist) = artist.filter(|a| !is_blank(a)) else {
                    continue;
                };
                let (mut alb_title, mut cover, mut alb_id) = (None, None, 0);
                if let Some(alb) = try_get_property(hit, "album")? {
                    alb_title = str_prop(alb, "title")?;
                    cover = deezer_image(alb, &["cover_xl", "cover_medium"])?;
                    if let Some(aid @ Value::Number(_)) = try_get_property(alb, "id")? {
                        alb_id = get_int64(aid)?;
                    }
                }
                let art_img = match try_get_property(hit, "artist")? {
                    Some(a) => deezer_image(a, &["picture_xl", "picture_medium"])?,
                    None => None,
                };
                let duration = match try_get_property(hit, "duration")? {
                    Some(du @ Value::Number(_)) => Some(get_int32(du)?),
                    _ => None,
                };
                let key = Self::track_key(artist, title);
                if self.get_cached(&key).is_none() {
                    let meta = TrackMeta {
                        album_id: (alb_id > 0).then_some(alb_id),
                        album_title: alb_title.map(str::to_string),
                        album_cover_url: cover.map(str::to_string),
                        year: None,
                        duration,
                        artist_name: Some(artist.to_string()),
                        artist_image_url: art_img.map(str::to_string),
                        explicit_content: if song_key(hit)?.is_some_and(|key| explicit_songs.contains(&key)) {
                            Some(explicit_status::EXPLICIT)
                        } else {
                            catalog_explicit(hit)?
                        },
                    };
                    self.put(key, Cached::Track(Some(meta)), POSITIVE_TTL);
                }
                found.push((artist.to_string(), title.to_string()));
            }
            Ok(found)
        };
        match read() {
            Ok(found) => Some(found),
            Err(e) => {
                debug!("deezer track search '{query}' could not be read: {e}");
                None
            }
        }
    }

    /// With `after_permit`, every question of the search takes its permit before its timer starts.
    async fn find_track(
        &self,
        artist: &str,
        title: &str,
        background: bool,
        after_permit: bool,
    ) -> ElementResult<(bool, Option<(Value, Option<Value>)>)> {
        let mut artist_id = None;
        for variant in SongIdentity::query_variants(title, artist)
            .into_iter()
            .take(TRACK_SEARCHES)
        {
            let url = format!(
                "{}/search?q={}&limit={MATCH_CANDIDATES}",
                self.base(),
                escape_data_string(&variant.text())
            );
            let r = self.call_json(&url, background, after_permit).await;
            if r.transient {
                return Ok((true, None));
            }
            if let Some(hit) = best_match(r.doc.as_ref(), artist, title)? {
                return Ok((false, Some((hit, r.doc))));
            }
            if artist_id.is_none() {
                artist_id = named_artist_id(r.doc.as_ref(), artist)?;
            }
        }

        let (transient, on_album, album_artist_id) = self
            .find_track_on_album(artist, title, background, after_permit)
            .await?;
        if transient {
            return Ok((true, None));
        }
        if on_album.is_some() {
            return Ok((false, on_album));
        }

        let Some(artist_id) = artist_id.or(album_artist_id) else {
            return Ok((false, None));
        };
        let url = format!(
            "{}/artist/{}/top?limit={ARTIST_TOP_SONGS}",
            self.base(),
            escape_data_string(&artist_id)
        );
        let r = self.call_json(&url, background, after_permit).await;
        if r.transient {
            return Ok((true, None));
        }
        Ok((
            false,
            best_match(r.doc.as_ref(), artist, title)?.map(|hit| (hit, r.doc)),
        ))
    }

    /// The song on the album named after it: the album search's first match for the artist and
    /// title, that album's tracklist, then the matching track's own record, which carries its
    /// album. Also the artist id the album search named, for the next step. The first value says
    /// a request did not answer.
    async fn find_track_on_album(
        &self,
        artist: &str,
        title: &str,
        background: bool,
        after_permit: bool,
    ) -> ElementResult<(bool, Option<(Value, Option<Value>)>, Option<String>)> {
        let query = plain_query(&[artist, title]);
        if query.is_empty() {
            return Ok((false, None, None));
        }
        let albums = self
            .call_json(
                &format!(
                    "{}/search/album?q={}&limit={MATCH_CANDIDATES}",
                    self.base(),
                    escape_data_string(&query)
                ),
                background,
                after_permit,
            )
            .await;
        if albums.transient {
            return Ok((true, None, None));
        }
        let artist_id = named_artist_id(albums.doc.as_ref(), artist)?;
        let album_id = match best_match(albums.doc.as_ref(), artist, title)? {
            Some(album) => number_id(&album)?,
            None => None,
        };
        let Some(album_id) = album_id else {
            return Ok((false, None, artist_id));
        };

        let tracklist = self
            .call_json(
                &format!(
                    "{}/album/{album_id}/tracks?limit={ALBUM_TRACKS_LIMIT}",
                    self.base()
                ),
                background,
                after_permit,
            )
            .await;
        if tracklist.transient {
            return Ok((true, None, None));
        }
        let track_id = match best_match(tracklist.doc.as_ref(), artist, title)? {
            Some(track) => number_id(&track)?,
            None => None,
        };
        let Some(track_id) = track_id else {
            return Ok((false, None, artist_id));
        };

        let detail = self
            .call_json(
                &format!("{}/track/{track_id}", self.base()),
                background,
                after_permit,
            )
            .await;
        if detail.transient {
            return Ok((true, None, None));
        }
        let Some(track) = detail.doc.filter(|doc| matches!(doc, Value::Object(_))) else {
            return Ok((false, None, artist_id));
        };
        if number_id(&track)?.as_deref() != Some(track_id.as_str()) {
            return Ok((false, None, artist_id));
        }
        Ok((false, Some((track, tracklist.doc)), artist_id))
    }

    /// Like [`Self::find_track`], but every hit that is this song from the first search that
    /// finds any, in the catalog's order.
    async fn find_track_hits(&self, artist: &str, title: &str) -> ElementResult<(bool, Vec<Value>)> {
        for variant in SongIdentity::query_variants(title, artist)
            .into_iter()
            .take(TRACK_SEARCHES)
        {
            let url = format!(
                "{}/search?q={}&limit={MATCH_CANDIDATES}",
                self.base(),
                escape_data_string(&variant.text())
            );
            let r = self.get_json(&url, false).await;
            if r.transient {
                return Ok((true, Vec::new()));
            }
            let hits = all_matches(r.doc.as_ref(), artist, title, None)?;
            if !hits.is_empty() {
                return Ok((false, hits));
            }
        }
        Ok((false, Vec::new()))
    }
}

/// The catalog call the release identifier makes. The service never fails; a throttled catalog
/// is `did_not_answer`.
#[async_trait::async_trait]
impl CatalogLookup for DeezerMetadataService {
    async fn enrich_track_candidates(
        &self,
        artist: &str,
        title: &str,
        max: i32,
    ) -> anyhow::Result<CatalogCandidates> {
        Ok((*DeezerMetadataService::enrich_track_candidates(self, artist, title, max).await).clone())
    }
}

/// Words that make a bracket or a dash tail on an album title another edition of the same
/// record: "Take Care (Deluxe)", "Views - Remastered 2021", "Nevermind [30th Anniversary]".
static EDITION_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:deluxe|edition|remaster(?:ed)?|expanded|anniversary|explicit|clean|bonus|special|collector'?s|reissue|super|platinum|complete|single|ep)\b",
    )
    .expect("a valid pattern")
});

/// Words that make it another record, whatever edition words sit beside them: a live album, a
/// remix album, an instrumental or acoustic take.
static OTHER_RECORD_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:live|remix(?:es|ed)?|rmx|mix(?:es)?|instrumentals?|acoustic|unplugged|karaoke|demos?)\b",
    )
    .expect("a valid pattern")
});

/// "(...)" or "[...]" at the end of an album title.
static EDITION_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[(\[]([^()\[\]]*)[)\]]\s*$").expect("a valid pattern"));

/// The tail after the last " - " of an album title.
static EDITION_DASH_TAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+[-–—]\s+([^-–—]*)$").expect("a valid pattern"));

/// `title` with its trailing edition markers taken off, and whether nothing but edition markers
/// was left trailing (so the record's own name is all that remains).
fn without_editions(title: &str) -> (String, bool) {
    let mut text = title.trim().to_string();
    loop {
        let mut stripped = false;
        for pattern in [&*EDITION_BRACKET, &*EDITION_DASH_TAIL] {
            let Some(found) = pattern.captures(&text) else {
                continue;
            };
            let whole = found.get(0).expect("a match has its whole text");
            let inner = found.get(1).map_or("", |m| m.as_str());
            if whole.start() > 0 && EDITION_WORDS.is_match(inner) && !OTHER_RECORD_WORDS.is_match(inner) {
                text = text[..whole.start()].trim_end().to_string();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    let only_editions = !EDITION_BRACKET.is_match(&text);
    (text, only_editions)
}

/// The record an album hit is an edition of: the artist, the record type, and the title without
/// its edition markers, read as `AlbumOwnership::base_key` reads album names when only edition
/// markers trail it. A title that keeps a bracket of another kind ("HABIBTI (FOMO)") is its own
/// record, and so is a live or remix album.
fn edition_key(hit: &AlbumHit) -> String {
    let (record, only_editions) = without_editions(&hit.title);
    let title_key = if only_editions {
        AlbumOwnership::base_key(Some(&record))
    } else {
        SongIdentity::key(&record)
    };
    format!(
        "{}|{}|{}",
        SongIdentity::key(&hit.artist),
        normalized_record_type(hit.record_type.as_deref()).unwrap_or_default(),
        title_key
    )
}

/// Repeat editions of one record (deluxe, remastered, anniversary, explicit and clean copies)
/// folded into one row, at the place of the first listed. The row is the edition the listing
/// ranks best: the better record type, then an explicit copy, then the one with the most songs,
/// then the first listed. Different record types never fold, so a single stays listed beside the
/// album it comes from. With `keep_clean_copies`, a record whose row is an explicit copy also
/// keeps one clean copy right after it, the one of the row's own title when there is one: a
/// search lists both, an artist's page one.
pub fn fold_editions(hits: Vec<AlbumHit>, keep_clean_copies: bool) -> Vec<AlbumHit> {
    fold_editions_preferring(hits, keep_clean_copies, false)
}

/// [`fold_editions`], where `prefer_clean` makes the row a copy that is not explicit when the
/// record has one, ahead of an explicit copy.
fn fold_editions_preferring(
    hits: Vec<AlbumHit>,
    keep_clean_copies: bool,
    prefer_clean: bool,
) -> Vec<AlbumHit> {
    let mut groups: Vec<Vec<AlbumHit>> = Vec::new();
    let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for hit in hits {
        let key = edition_key(&hit);
        match at.get(&key) {
            Some(&group) => groups[group].push(hit),
            None => {
                at.insert(key, groups.len());
                groups.push(vec![hit]);
            }
        }
    }

    let rank = |hit: &AlbumHit| {
        (
            release_rank(hit.record_type.as_deref()),
            (hit.explicit_lyrics == Some(true)) == prefer_clean,
            std::cmp::Reverse(hit.track_count),
        )
    };
    let mut folded = Vec::new();
    for mut group in groups {
        let best = (0..group.len())
            .min_by_key(|&i| rank(&group[i]))
            .expect("a group has a member");
        let row = group.remove(best);
        let clean = (keep_clean_copies && row.explicit_lyrics == Some(true))
            .then(|| {
                let own_title = SongIdentity::key(&row.title);
                (0..group.len())
                    .filter(|&i| group[i].explicit_lyrics == Some(false))
                    .min_by_key(|&i| (SongIdentity::key(&group[i].title) != own_title, rank(&group[i])))
            })
            .flatten();
        let clean = clean.map(|i| group.remove(i));
        folded.push(row);
        folded.extend(clean);
    }
    folded
}

/// An artist's releases in page order: albums, EPs, singles, compilations, newest first within
/// each.
fn sort_releases(hits: &mut [AlbumHit]) {
    hits.sort_by(|a, b| {
        release_rank(a.record_type.as_deref())
            .cmp(&release_rank(b.record_type.as_deref()))
            .then_with(|| b.year.unwrap_or(0).cmp(&a.year.unwrap_or(0)))
    });
}
/// Deezer no longer supports field-qualified search on the track endpoints. A query
/// like artist:"X" track:"Y" is now read as free text, so the literal words "artist"
/// and "track" have to appear in the record and nothing ever matches. Plain terms are
/// the only shape that still works.
fn plain_query(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !is_blank(p))
        .map(|p| p.trim())
        .collect::<Vec<_>>()
        .join(" ")
}

/// What one field of a hit says about the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldVerdict {
    Match,
    Absent,
    Mismatch,
}

/// Compare the titles by `SongIdentity`: the same key, or the same key once stylized
/// characters are read as letters, or one key containing the other, because Deezer decorates
/// titles in ways no list names. Containment counts only for a long enough shorter key, so
/// "Home" is not found in "Homecoming". Never another version: a live take or a remix carries its
/// own album and length, and attaching those to the original is wrong.
///
/// A field either side left empty is Absent, never a mismatch. Absent evidence is not
/// counter-evidence, and treating a field Deezer simply did not send as a contradiction would
/// throw away good hits the moment the payload shape changes.
fn compare_titles(want: &str, got: Option<&str>) -> FieldVerdict {
    let a = SongIdentity::parse_title(want, None);
    let b = SongIdentity::parse_title(got.unwrap_or(""), None);
    if a.key.is_empty() || b.key.is_empty() {
        return FieldVerdict::Absent;
    }
    if SongIdentity::distinct_versions(&a, None) != SongIdentity::distinct_versions(&b, None) {
        return FieldVerdict::Mismatch;
    }
    if a.key == b.key || a.loose_key == b.loose_key || one_holds_the_other(&a.key, &b.key) {
        FieldVerdict::Match
    } else {
        FieldVerdict::Mismatch
    }
}

/// The artists by `SongIdentity`, and one key containing the other (a long enough one), since
/// Deezer credits guests in the artist field.
fn compare_artists(want: &str, got: Option<&str>) -> FieldVerdict {
    let got = got.unwrap_or("");
    let a = SongIdentity::key(want);
    let b = SongIdentity::key(got);
    if a.is_empty() || b.is_empty() {
        return FieldVerdict::Absent;
    }
    if SongIdentity::artists_agree(want, got) || one_holds_the_other(&a, &b) {
        FieldVerdict::Match
    } else {
        FieldVerdict::Mismatch
    }
}

/// The shortest key that may count as held inside a longer one. Shorter keys turn up inside
/// unrelated names too often.
const MIN_CONTAINED_KEY: usize = 6;

/// One key containing the other, the shorter at least [`MIN_CONTAINED_KEY`] characters long.
fn one_holds_the_other(a: &str, b: &str) -> bool {
    let (short, long) = if a.chars().count() <= b.chars().count() {
        (a, b)
    } else {
        (b, a)
    };
    short.chars().count() >= MIN_CONTAINED_KEY && long.contains(short)
}

/// Lengths further apart than this are different recordings.
const LENGTH_TOLERANCE_SECONDS: i32 = 8;

/// The first hit that positively matches on artist or title and contradicts on
/// neither. This is the guard that makes a plain query safe to use in place of the
/// qualified one: without it a near-miss at position 0 would be attached to the song
/// as fact. Requiring at least one positive match is what stops a hit that states
/// nothing at all from matching everything.
fn best_match(doc: Option<&Value>, artist: &str, title: &str) -> ElementResult<Option<Value>> {
    Ok(all_matches(doc, artist, title, None)?.into_iter().next())
}

/// Every hit that positively matches on artist or title and contradicts on neither,
/// in the catalog's order. The first is what [`best_match`] returns. Given the song's length,
/// a hit whose own length is known and more than [`LENGTH_TOLERANCE_SECONDS`] away is another
/// recording.
fn all_matches(
    doc: Option<&Value>,
    artist: &str,
    title: &str,
    seconds: Option<i32>,
) -> ElementResult<Vec<Value>> {
    let mut hits = Vec::new();
    let Some(data) = data_array(doc)? else {
        return Ok(hits);
    };
    for hit in enumerate_array(data)? {
        let title_verdict = compare_titles(title, str_prop(hit, "title")?);
        let hit_artist = match try_get_property(hit, "artist")? {
            Some(a) => str_prop(a, "name")?,
            None => None,
        };
        let artist_verdict = compare_artists(artist, hit_artist);

        if title_verdict == FieldVerdict::Mismatch || artist_verdict == FieldVerdict::Mismatch {
            continue;
        }
        if let (Some(want), Some(got)) = (
            seconds.filter(|s| *s > 0),
            int(hit, "duration")?.filter(|s| *s > 0),
        ) && (want - got).abs() > LENGTH_TOLERANCE_SECONDS
        {
            continue;
        }
        if title_verdict == FieldVerdict::Match || artist_verdict == FieldVerdict::Match {
            hits.push(hit.clone());
        }
    }
    Ok(hits)
}

/// An element's numeric `id` as text, when it has one.
fn number_id(element: &Value) -> ElementResult<Option<String>> {
    Ok(match try_get_property(element, "id")? {
        Some(id @ Value::Number(_)) => Some(get_int64(id)?.to_string()),
        _ => None,
    })
}

/// A track answer as the clip check reads it: NoData when it names no track. Fields that are
/// missing or of another kind are left empty rather than failing the answer.
fn read_track(asked: DeezerAsked<Value>) -> DeezerAsked<DeezerTrack> {
    let doc = match asked {
        DeezerAsked::Answer(doc) => doc,
        DeezerAsked::NoData => return DeezerAsked::NoData,
        DeezerAsked::Busy => return DeezerAsked::Busy,
    };
    let text = |value: &Value, field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let id = match doc.get("id") {
        Some(Value::Number(id)) => id.as_i64().map(|id| id.to_string()).unwrap_or_default(),
        Some(Value::String(id)) => id.trim().to_string(),
        _ => String::new(),
    };
    if id.is_empty() {
        return DeezerAsked::NoData;
    }
    DeezerAsked::Answer(DeezerTrack {
        id,
        title: text(&doc, "title").unwrap_or_default(),
        artist: doc
            .get("artist")
            .and_then(|artist| text(artist, "name"))
            .unwrap_or_default(),
        duration_seconds: doc
            .get("duration")
            .and_then(Value::as_i64)
            .and_then(|seconds| i32::try_from(seconds).ok())
            .filter(|seconds| *seconds > 0),
        isrc: text(&doc, "isrc").and_then(|code| SongIdentity::normalize_isrc(&code)),
        clip_url: text(&doc, "preview"),
    })
}

/// The catalog id of the artist a search answer credits by exactly the name asked for (spaces
/// kept: "The Sun Days" and "The Sundays" are two bands), from the first hit that does.
fn named_artist_id(doc: Option<&Value>, artist: &str) -> ElementResult<Option<String>> {
    let wanted = artist.trim();
    if wanted.is_empty() {
        return Ok(None);
    }
    let Some(data) = data_array(doc)? else {
        return Ok(None);
    };
    for hit in enumerate_array(data)? {
        let Some(credit) = try_get_property(hit, "artist")? else {
            continue;
        };
        if str_prop(credit, "name")?.is_some_and(|name| dotnet::eq_ignore_case(name.trim(), wanted))
            && let Some(id) = number_id(credit)?
        {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// `root.data` when it is an array. Reading a property of a root that is not an object
/// fails.
/// The artists of a catalog list (`data`), each with an id and a name.
fn artist_hits(doc: Option<&Value>) -> ElementResult<Vec<ArtistHit>> {
    let mut hits = Vec::new();
    if let Some(data) = data_array(doc)? {
        for a in enumerate_array(data)? {
            let id = match try_get_property(a, "id")? {
                Some(aid @ Value::Number(_)) => Some(get_int64(aid)?.to_string()),
                _ => None,
            };
            let name = str_prop(a, "name")?;
            let (Some(id), Some(name)) = (id, name.filter(|n| !is_blank(n))) else {
                continue;
            };
            hits.push(ArtistHit {
                deezer_id: id,
                name: name.to_string(),
                picture_url: deezer_image(a, &["picture_xl", "picture_medium"])?.map(str::to_string),
                album_count: int(a, "nb_album")?.unwrap_or(0),
                fans: int(a, "nb_fan")?.unwrap_or(0),
            });
        }
    }
    Ok(hits)
}

fn data_array(doc: Option<&Value>) -> ElementResult<Option<&Value>> {
    let Some(root) = doc else {
        return Ok(None);
    };
    Ok(match try_get_property(root, "data")? {
        Some(data @ Value::Array(_)) => Some(data),
        _ => None,
    })
}

fn first_data(doc: Option<&Value>) -> ElementResult<Option<&Value>> {
    Ok(match data_array(doc)? {
        Some(Value::Array(items)) => items.first(),
        _ => None,
    })
}

/// `genres.data[0].name`, when the album lists a genre.
fn first_genre(root: &Value) -> ElementResult<Option<String>> {
    if let Some(genres) = try_get_property(root, "genres")?
        && let Some(gd @ Value::Array(items)) = try_get_property(genres, "data")?
        && array_length(gd)? > 0
    {
        return Ok(str_prop(&items[0], "name")?.map(str::to_string));
    }
    Ok(None)
}

/// A boolean property: a true or false, None for anything else.
fn bool_prop(element: &Value, name: &str) -> ElementResult<Option<bool>> {
    Ok(match try_get_property(element, name)? {
        Some(Value::Bool(said)) => Some(*said),
        _ => None,
    })
}

/// The explicit status (`explicit_status::from_catalog`) of a catalog record's two explicit
/// fields.
fn catalog_explicit(element: &Value) -> ElementResult<Option<i32>> {
    Ok(explicit_status::from_catalog(
        int(element, "explicit_content_lyrics")?,
        bool_prop(element, "explicit_lyrics")?,
    ))
}

/// Whether a search row's song is explicit. The row stands for the song, not one copy of it,
/// and the catalog lists the explicit original and its clean edit side by side under the same
/// title: when any hit of this very song (the same version as the one matched) is explicit, the
/// song is; otherwise the matched hit says.
pub(crate) fn song_explicitness(
    doc: Option<&Value>,
    artist: &str,
    title: &str,
    matched: &Value,
) -> ElementResult<Option<i32>> {
    let mine = catalog_explicit(matched)?;
    let versions_of = |hit: &Value| -> ElementResult<_> {
        let parsed = SongIdentity::parse_title(str_prop(hit, "title")?.unwrap_or(""), None);
        Ok(SongIdentity::distinct_versions(&parsed, None))
    };
    let version = versions_of(matched)?;
    for hit in all_matches(doc, artist, title, None)? {
        if versions_of(&hit)? != version {
            continue;
        }
        if catalog_explicit(&hit)? == Some(explicit_status::EXPLICIT) {
            return Ok(Some(explicit_status::EXPLICIT));
        }
    }
    Ok(mine)
}

/// An `i32` property: `get_int32` of a number, which fails for one that is not an integer in
/// range; None for anything else.
fn int(element: &Value, name: &str) -> ElementResult<Option<i32>> {
    match try_get_property(element, name)? {
        Some(value @ Value::Number(_)) => Ok(Some(get_int32(value)?)),
        _ => Ok(None),
    }
}

/// The first four UTF-16 units of a date at least that long, read as an integer, with leading and
/// trailing white space and a sign allowed.
fn year_prefix(release_date: &str) -> Option<i32> {
    if dotnet::utf16_len(release_date) < 4 {
        return None;
    }
    let mut prefix = String::new();
    let mut units = 0;
    for c in release_date.chars() {
        units += c.len_utf16();
        if units > 4 {
            // A surrogate pair cut in half is not a digit either way.
            return None;
        }
        prefix.push(c);
        if units == 4 {
            break;
        }
    }
    let white = |c: char| matches!(c, '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ');
    let text = prefix.trim_matches(white);
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[path = "deezer_metadata_service_explicit.rs"]
mod explicit;
#[path = "deezer_metadata_service_tempo.rs"]
mod tempo;

pub use tempo::DeezerPublishedTempo;
use tempo::{tempo_key, tempo_of};

pub use explicit::DeezerExplicitCatalog;
// The chart and artist top lists.
#[path = "deezer_charts.rs"]
mod charts;
pub use charts::CHART_LIMIT;

#[cfg(test)]
#[path = "deezer_metadata_service_tests.rs"]
mod tests;
