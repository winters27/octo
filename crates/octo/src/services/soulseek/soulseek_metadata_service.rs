//! The Soulseek metadata service. The routing model is
//! `octo_core::soulseek::soulseek_metadata_service`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use futures::FutureExt;
use futures::StreamExt;
use futures::future::{BoxFuture, Shared};
use octo_core::common::{SongIdentity, dotnet};
use octo_core::metadata::deezer_metadata_service::{
    AlbumAnswer, AlbumHit, ArtistHit, TrackMeta, release_types,
};
use octo_core::models::domain::{Album, Artist, Song, explicit_status};
use octo_core::models::search::SearchResult;
use octo_core::models::subsonic::ExternalPlaylist;
use octo_core::soulseek::{LengthSource, RoutingKind, SongLength, SoulseekRouting};
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::external_id_registry::{ExternalIdRegistry, SharedRouting};
use crate::services::cover_art::CoverArtAggregator;
use crate::services::framework::InFlight;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::metadata::DeezerMetadataService;
use crate::services::metadata::deezer_metadata_service::fold_editions;
use crate::services::you_tube::{YouTubeHit, YouTubeResolver};

/// The two members of `LastFmService` the length lookup uses: whether it has an API key, and
/// the duration `track.getInfo` gives a song, in seconds. A seam rather than the service.
///
/// Implemented by `LastFmService`; tests substitute a fake.
#[async_trait]
pub trait LastFmTrackLengths: Send + Sync {
    /// Whether Last.fm has an API key.
    fn has_api_key(&self) -> bool;

    /// The duration of the song's `get_track_info` answer, if any.
    async fn track_duration(&self, artist: &str, title: &str) -> Option<i32>;
}

/// Music metadata service for the YouTube-first / Soulseek-on-star architecture.
///
/// Radio queue creation is YouTube-only and lightweight: one yt-dlp search per
/// Last.fm similar track. We do NOT query Soulseek here: Soulseek is reserved
/// for the explicit "user wants to keep this" action (star / permanent download)
/// in SoulseekDownloadService.
///
/// External IDs are kept short (~30-80 chars) so Subsonic clients accept them.
/// Format:  yt|{videoId}|{artist_b64}|{title_b64}|{durationSec}
///
/// Cheap to clone: the clones share their gates and walks.
#[derive(Clone)]
pub struct SoulseekMetadataService {
    inner: Arc<Inner>,
}

/// An artist's releases and the track counts shown beside them.
#[derive(Debug, Clone, Default)]
struct ArtistWalk {
    releases: Vec<AlbumHit>,
    counts: Vec<Option<i32>>,
}

struct Inner {
    youtube: Arc<YouTubeResolver>,
    id_registry: Arc<ExternalIdRegistry>,
    deezer: Arc<DeezerMetadataService>,
    cover_art: Arc<CoverArtAggregator>,
    last_fm: Option<Arc<dyn LastFmTrackLengths>>,

    /// Ids with a lookup queued or running, so several clients loading one station
    /// at once do not each look its songs up again.
    length_lookups: Mutex<HashSet<String>>,

    /// The most recent background lookup, so a test can wait for it.
    last_length_warm: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,

    /// The most recent pass finishing a search's rows, so a test can wait for it.
    last_rows_finish: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,

    // Shared across ALL invocations, not created per call. The shim runs 5
    // yt-dlp processes at a time; per-invocation semaphores let the three
    // prewarm triggers (radio, scrobble, external search) stack to 12
    // concurrent /search against it, and the old value of 6 here exceeded the
    // whole gate on its own. Sized to the shim's background capacity
    // (MAX_CONCURRENT_YTDLP - GATE_RESERVE_INTERACTIVE). This service is
    // registered as a singleton, so an instance field is already process-wide
    // without being static (which would make parallel test runs hostile).
    prewarm_gate: Semaphore,

    // Cover art never touches the shim: it hits Deezer/iTunes/Last.fm over HTTP, and
    // Deezer's own background lane (DeezerRateLimiter::BACKGROUND_PERMITS) already bounds
    // that traffic. It needs its own gate, not prewarm_gate above: sharing that one meant
    // 24 cover-art tasks and 12 YouTube tasks fought over 3 permits with a 2s bounded
    // wait, so most cover fetches timed out and the ones that won starved YouTube prewarm.
    cover_art_prewarm_gate: Semaphore,

    // The length pass that runs after a search has its own, smaller gate. On prewarm_gate its
    // eight lookups took every permit, and a getSong right after the search waited out its two
    // seconds and showed Deezer's length instead of the video's.
    background_duration_gate: Semaphore,

    /// The catalog artist each library artist's page settled on, by the page's artist
    /// and library titles. Never on the shared routing: see find_artist_releases.
    library_picks: Mutex<HashMap<String, String>>,

    /// Walks of an artist's catalog in progress: the releases alone, and the releases with
    /// their counts filled. Keyed by the artist's id and the library titles, since those decide
    /// which artist of a name is meant. A client opens an artist's page with two requests at
    /// once, and each used to walk the catalog on its own: twice the calls against a quota
    /// search and playback share. Everything a walk asks is cached once it is back, so a
    /// finished walk leaves this and the next visit reads the cache.
    release_walks: InFlight<Vec<AlbumHit>>,
    count_walks: InFlight<ArtistWalk>,
}

/// What the foreground durations pass settled before its answer went out: whether it went out,
/// and the length each row resolved in time shows, by its place in the answer.
#[derive(Default)]
struct DurationsAnswer {
    sent: bool,
    lengths: Vec<(usize, i32)>,
}

/// How long a single song's answer waits for its album year before going out without it.
const SONG_YEAR_WAIT: Duration = Duration::from_millis(1500);

/// A search row left for after the answer: one its single track search missed, or the
/// album whose year it skipped.
enum RowToFinish {
    Missed {
        id: String,
        artist: String,
        title: String,
    },
    Year(i64),
}

/// Marks the answer sent when dropped, however the wait for it ends.
struct SentOnDrop(Arc<Mutex<DurationsAnswer>>);

impl Drop for SentOnDrop {
    fn drop(&mut self) {
        self.0.lock().sent = true;
    }
}

/// Deezer's real ceiling is ~50 requests per 5 seconds, and this runs on search3's
/// critical path, so the set that blocks a response stays small. 60 rows at 8-way
/// concurrency was roughly 65 requests/second on its own, which is what exhausted the
/// quota and poisoned the metadata caches (issue #8). DeezerRateLimiter now holds that
/// budget centrally, so this figure is about how long a user waits, not about safety.
///
/// 12 is the same "first page" figure `prewarm_you_tube_ids` already uses. It must stay above
/// `TOP_DURATION_RESOLVE_LIMIT`, or the rows that get a YouTube length hint would be reading a
/// duration nobody resolved.
const SEARCH_ENRICH_LIMIT: usize = 12;

/// The whole slice a search can return. Everything between the first page and this is filled from
/// cache and warmed for next time, because a row with no enrichment goes out with no length at
/// all, and a page of approximate lengths is better than a page of none. Deezer's length is the
/// approximation (it is not always the recording that plays), and the exact value is resolved
/// when a row is opened or played, where getSong and the native detail endpoint both call
/// `resolve_top_durations`.
const BACKGROUND_ENRICH_LIMIT: usize = 60;

// ---- Lengths for rows that went out without one ---------------------------------
//
// About half of all outside songs would reach clients with no length: search rows past
// the first page, and every station row Last.fm gave no length for. The
// station path never looked a length up at all, and what the search warm fetched only
// reached Deezer's in-memory cache, so a song got its length back on a repeat search at
// best and lost it again on a restart.
//
// Nothing here holds up a response. The rows a response can complete for free
// (registry, Deezer's cache) are completed inline; the rest are looked up in the
// background and stored on the registry, so the NEXT response for the song carries the
// length. Sources are tried in order of how far their length can be trusted, and the
// first one to answer ends the chain.

/// Most rows one station response queues for a lookup. The next response
/// queues the next ones, since those done by then are no longer cold.
const STATION_LENGTH_WARM_LIMIT: usize = 20;

/// Ceiling on one song's lookup chain. Last.fm has no client timeout of its
/// own, and one hung call would otherwise stall every song queued behind it.
const LENGTH_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

// Resolve the ACTUAL YouTube video for the top of the list at search time and
// use its duration. Deezer's duration is a different recording (e.g. "Fade"
// is 3:13 on Deezer but the YouTube upload that plays is 3:45), so the scrub
// bar overran and the client's advance logic broke. Storing the videoId also
// means playback reuses this exact video (durations match) and it is prewarmed.
const TOP_DURATION_RESOLVE_LIMIT: usize = 8;

const PREWARM_QUEUE_WAIT: Duration = Duration::from_secs(2);

/// How long an artist's page waits for its albums' track counts.
const TRACK_COUNT_WAIT: Duration = Duration::from_secs(2);

/// How many unknown track counts one visit to an artist's page asks for, and how
/// many at once: gentle on the catalog's quota, which search and playback share.
const TRACK_COUNTS_PER_VISIT: usize = 20;
const TRACK_COUNTS_AT_ONCE: usize = 4;

/// How many catalog artists a name search weighs. The first hit is not reliably
/// the artist asked for: a name another artist shares, or a bigger act that contains it,
/// can come first.
const ARTIST_CANDIDATES: i32 = 5;

/// How many artists of one name a library artist's page compares against the
/// library. Each is one listing call, cached, and only made when a name is shared.
const ARTISTS_COMPARED: usize = 3;

/// How many library pages' choices are kept. Past it they start over: a choice
/// lost costs one comparison against listings the catalog cache still holds.
const LIBRARY_PICKS_KEPT: usize = 2048;

/// The songs-filed-under lookup's limit (`SongsFiledUnder`'s default).
const SONGS_FILED_UNDER_LIMIT: usize = 50;

/// A permit from `gate`, or none when it is not free within the bounded queue wait.
async fn bounded_wait(gate: &Semaphore) -> Option<tokio::sync::SemaphorePermit<'_>> {
    tokio::time::timeout(PREWARM_QUEUE_WAIT, gate.acquire())
        .await
        .ok()
        .and_then(Result::ok)
}

/// `string.Equals(a, "soulseek", OrdinalIgnoreCase)`.
fn is_ours(external_provider: &str) -> bool {
    dotnet::eq_ignore_case(external_provider, SoulseekMetadataService::PROVIDER_NAME)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// A catalog record type as release types, a fresh list for each album.
fn release_type_list(record_type: Option<&str>) -> Vec<String> {
    release_types(record_type).iter().map(|t| t.to_string()).collect()
}

/// How many of an artist's releases the library holds, by the matcher's key.
fn shared_count(releases: &[AlbumHit], owned: &HashSet<String>) -> usize {
    releases
        .iter()
        .filter(|release| owned.contains(&SongIdentity::key(&release.title)))
        .count()
}

/// The artist more people follow; the catalog's own order on a tie.
fn most_followed<'a>(hits: impl IntoIterator<Item = &'a ArtistHit>) -> Option<&'a ArtistHit> {
    let mut best: Option<&ArtistHit> = None;
    for hit in hits {
        if best.is_none_or(|b| hit.fans > b.fans) {
            best = Some(hit);
        }
    }
    best
}

/// The catalog artists a name search answered that bear exactly this name.
fn same_name(hits: Vec<ArtistHit>, name: &str) -> Vec<ArtistHit> {
    hits.into_iter()
        .filter(|hit| SongIdentity::same_artist_name(&hit.name, name))
        .collect()
}

/// Standard base64 that accepts unused bits in the last character (the decoder strips
/// whitespace before it).
const DOTNET_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

fn b64_url_encode(s: &str) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(s.as_bytes())
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

fn b64_url_decode(s: &str) -> Option<String> {
    let mut s = s.replace('-', "+").replace('_', "/");
    match s.len() % 4 {
        2 => s.push_str("=="),
        3 => s.push('='),
        _ => {}
    }
    let compact: String = s
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
        .collect();
    let bytes = DOTNET_BASE64.decode(compact).ok()?;
    // Whatever is not UTF-8 is replaced.
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// An integer with surrounding white space and a sign allowed.
fn parse_int(text: &str) -> Option<i32> {
    text.trim_matches(|c: char| matches!(c, '\u{9}'..='\u{D}' | ' '))
        .parse()
        .ok()
}

/// "artist title..." split at the first space; the title is None for a single word.
fn parse_query(query: &str) -> (String, Option<String>) {
    let trimmed = query.trim();
    match trimmed.find(' ') {
        Some(idx) if idx > 0 => (
            trimmed[..idx].to_string(),
            Some(trimmed[idx + 1..].trim().to_string()),
        ),
        _ => (trimmed.to_string(), None),
    }
}

impl SoulseekMetadataService {
    pub const PROVIDER_NAME: &'static str = "soulseek";

    pub fn new(
        youtube: Arc<YouTubeResolver>,
        id_registry: Arc<ExternalIdRegistry>,
        deezer: Arc<DeezerMetadataService>,
        cover_art: Arc<CoverArtAggregator>,
        last_fm: Option<Arc<dyn LastFmTrackLengths>>,
    ) -> Self {
        SoulseekMetadataService {
            inner: Arc::new(Inner {
                youtube,
                id_registry,
                deezer,
                cover_art,
                last_fm,
                length_lookups: Mutex::new(HashSet::new()),
                last_length_warm: Mutex::new(None),
                last_rows_finish: Mutex::new(None),
                prewarm_gate: Semaphore::new(3),
                cover_art_prewarm_gate: Semaphore::new(6),
                background_duration_gate: Semaphore::new(2),
                library_picks: Mutex::new(HashMap::new()),
                release_walks: InFlight::new(),
                count_walks: InFlight::new(),
            }),
        }
    }

    /// The most recent pass finishing a search's rows, for a test to wait on.
    pub async fn last_rows_finish(&self) {
        let finish = self.inner.last_rows_finish.lock().clone();
        if let Some(finish) = finish {
            finish.await;
        }
    }

    /// The most recent background length lookup, for a test to wait on (`LastLengthWarm`).
    pub async fn last_length_warm(&self) {
        let warm = self.inner.last_length_warm.lock().clone();
        if let Some(warm) = warm {
            warm.await;
        }
    }

    // ====== Short opaque ID format ======
    // Pipe-delimited fields, base64url where needed.
    //   yt|{videoId}|{artistB64}|{titleB64}|{durationSec}
    // Total length ~30-80 chars depending on artist/title length.

    pub fn encode_external_id(r: &SoulseekRouting) -> String {
        let artist = r.artist.as_deref().unwrap_or("");
        let title = r.title.as_deref().unwrap_or("");
        let dur = r.duration.map(|d| d.to_string()).unwrap_or_default();
        format!(
            "yt|{}|{}|{}|{dur}",
            r.you_tube_id.as_deref().unwrap_or(""),
            b64_url_encode(artist),
            b64_url_encode(title)
        )
    }

    pub fn try_decode_external_id(external_id: Option<&str>) -> Option<SoulseekRouting> {
        let external_id = external_id.filter(|id| !dotnet::is_blank(id))?;
        let parts: Vec<&str> = external_id.split('|').collect();
        if parts.len() < 4 || parts[0] != "yt" {
            return None;
        }
        let duration = parts.get(4).and_then(|d| parse_int(d));
        Some(SoulseekRouting {
            you_tube_id: Some(parts[1].to_string()),
            artist: Some(b64_url_decode(parts[2])?),
            title: Some(b64_url_decode(parts[3])?),
            duration,
            ..Default::default()
        })
    }
}

/// The catalog ids of the clean copies in an album listing: a release that is not marked
/// explicit beside one of the same artist and title that is. The listing alone does not tell a
/// clean edit from an album that never had explicit words, but its explicit twin does.
pub(crate) fn clean_copies(hits: &[AlbumHit]) -> HashSet<String> {
    let key = |hit: &AlbumHit| (SongIdentity::key(&hit.artist), SongIdentity::key(&hit.title));
    let explicit: HashSet<(String, String)> = hits
        .iter()
        .filter(|hit| hit.explicit_lyrics == Some(true))
        .map(key)
        .collect();
    hits.iter()
        .filter(|hit| hit.explicit_lyrics == Some(false) && explicit.contains(&key(hit)))
        .map(|hit| hit.deezer_id.clone())
        .collect()
}

impl Inner {
    /// Album search rows, each registered so it opens, plays and stars like any outside
    /// album. With `fold`, repeat editions of one record are one row (see `fold_editions`).
    async fn album_search(&self, query: &str, limit: i32, fold: bool) -> Vec<Album> {
        if dotnet::is_blank(query) || limit <= 0 {
            return Vec::new();
        }
        let hits = self.deezer.search_albums(query, limit, false).await;
        let hits = if fold { fold_editions(hits, true) } else { hits };
        let mut albums = Vec::with_capacity(hits.len());
        let clean_copies = clean_copies(&hits);

        for hit in hits {
            // The registry id is the external id everywhere: getAlbum, getCoverArt and
            // star all round-trip through it. The Deezer id rides along on the routing
            // so album detail can fetch the exact tracklist without a name lookup. A clean copy
            // listed beside its explicit original gets an id of its own, or the two rows would
            // open the same album.
            let words = if clean_copies.contains(hit.deezer_id.as_str()) {
                Some(explicit_status::CLEAN)
            } else {
                (hit.explicit_lyrics == Some(true)).then_some(explicit_status::EXPLICIT)
            };
            let album_id = self.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Album,
                artist: Some(hit.artist.clone()),
                album: Some(hit.title.clone()),
                external_album_id: Some(hit.deezer_id.clone()),
                explicit_content: words,
                version: explicit_status::version_of(words),
                ..Default::default()
            });
            let artist_id = self.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some(hit.artist.clone()),
                ..Default::default()
            });

            albums.push(Album {
                id: album_id.clone(),
                title: hit.title.clone(),
                artist: hit.artist.clone(),
                artist_id: Some(artist_id),
                year: hit.year,
                song_count: Some(hit.track_count),
                cover_art_url: hit.cover_url.clone(),
                release_types: release_type_list(hit.record_type.as_deref()),
                explicit_content_lyrics: words,
                is_local: false,
                external_provider: Some(SoulseekMetadataService::PROVIDER_NAME.to_string()),
                external_id: Some(album_id),
                ..Default::default()
            });
        }

        albums
    }

    fn lookup(&self, id: &str) -> Option<SharedRouting> {
        self.id_registry.lookup(id)
    }

    /// Reflect a catalog answer onto the song and its shared routing, so getSong stays
    /// consistent, and remember its length. The catalog's album only fills a blank one: the
    /// track it matched can sit on a compilation, and the album the song already names is the
    /// one it came from.
    fn apply_track_meta(&self, song: &mut Song, meta: &TrackMeta) {
        if let Some(d) = meta.duration.filter(|&d| d > 0) {
            song.duration = Some(d);
        }
        if dotnet::is_blank(&song.album)
            && let Some(album) = meta.album_title.as_deref().filter(|a| !dotnet::is_blank(a))
        {
            song.album = album.to_string();
        }
        if let Some(words) = meta.explicit_content {
            song.explicit_content_lyrics = Some(words);
        }
    }

    fn reflect_on_routing(&self, id: &str, meta: &TrackMeta) {
        // Reflect onto the shared routing so getSong stays consistent.
        if let Some(routing) = self.lookup(id) {
            let mut r = routing.lock();
            if let Some(d) = meta.duration.filter(|&d| d > 0) {
                r.duration = Some(d);
            }
            if r.album.as_deref().is_none_or(dotnet::is_blank)
                && let Some(album) = meta.album_title.as_deref().filter(|a| !dotnet::is_blank(a))
            {
                r.album = Some(album.to_string());
            }
            if let Some(said) = meta.explicit_content {
                r.explicit_content = Some(said);
            }
        }
        self.id_registry
            .remember_length(id, meta.duration, LengthSource::Deezer);
    }

    /// One song's video and its length from the shim's quick lookup: the video is pinned on
    /// the routing for playback and its length remembered as the video's. `background`: the
    /// song is already with the client, so only the routing is written and a video a play
    /// pinned meanwhile is kept.
    async fn resolve_video_length(&self, song: &mut Song, background: bool) {
        let refused = self
            .lookup(&song.id)
            .map(|r| r.lock().refused_videos.clone())
            .unwrap_or_default();
        let Some(hit) = self
            .find_video(&song.artist, &song.title, song.duration, &refused, background)
            .await
        else {
            return;
        };
        if let Some(shown) = self.keep_video(&song.id, &hit, background) {
            song.duration = Some(shown);
        }
    }

    /// The video for `artist - title` and its length, without its stream: YouTube Music's row
    /// for the song first, else the shim's metadata-only yt-dlp lookup (a flat search, no URL
    /// solve). `length` is the length known for the song: YouTube Music keeps to rows within a
    /// few seconds of it, and yt-dlp takes it as a hint for the closest-length video rather
    /// than a long-form or compilation upload. A video in `refused` is never the answer.
    async fn find_video(
        &self,
        artist: &str,
        title: &str,
        length: Option<i32>,
        refused: &[String],
        background: bool,
    ) -> Option<YouTubeHit> {
        if let Some(hit) = self
            .youtube
            .music_song(artist, title, length, background)
            .await
            .filter(|h| !refused.contains(&h.video_id))
        {
            debug!("'{artist} - {title}': video {} from YouTube Music", hit.video_id);
            return Some(hit);
        }
        let hit = self
            .youtube
            .meta(&format!("{artist} {title}"), length, background)
            .await
            .filter(|h| !h.video_id.is_empty() && !refused.contains(&h.video_id))?;
        debug!("'{artist} - {title}': video {} from yt-dlp", hit.video_id);
        Some(hit)
    }

    /// Pins `hit` on the song under `song_id` for playback and remembers its length as the
    /// video's, so the video whose length a row shows is the video that plays. Returns the
    /// length the song's row shows, only in the foreground and only inside the sane range (an
    /// hour-long upload is a mix or a live set, and its length is no better than the one the
    /// row has). `background`: the row is already with the client, so a video a play pinned
    /// meanwhile is kept, since the next Range request has to get the same video. getSong
    /// builds its answer from the routing's duration, so it gets this length either way.
    /// Nothing is kept for a video the song refuses.
    fn keep_video(&self, song_id: &str, hit: &YouTubeHit, background: bool) -> Option<i32> {
        let d = hit.duration.filter(|&d| d > 0)?;
        if self
            .lookup(song_id)
            .is_some_and(|r| r.lock().refuses_video(&hit.video_id))
        {
            return None;
        }
        if self.id_registry.pin_video(song_id, &hit.video_id, !background) {
            self.id_registry.remember_pick(song_id, &hit.video_id, hit.pick);
            if let Some(routing) = self.lookup(song_id) {
                routing.lock().duration = Some(d);
            }
        } else if background && self.lookup(song_id).is_some() {
            return None;
        }
        self.id_registry
            .remember_length(song_id, Some(d), LengthSource::Video);
        if background {
            None
        } else {
            SongLength::sane_video_length(Some(d))
        }
    }

    /// Picks the video for the song under `id` and pins it for playback, unless a play pinned
    /// one meanwhile: YouTube Music's row for the song first, its stream then resolved through
    /// the shim's facts, else the shim's yt-dlp search, which resolves the stream in the same
    /// run. Either way a play of the video starts from the shim's cache. Background work.
    async fn pick_and_warm_video(&self, id: &str, routing: &SharedRouting) {
        let (artist, title, length, refused) = {
            let r = routing.lock();
            (
                r.artist.clone().unwrap_or_default(),
                r.title.clone().unwrap_or_default(),
                r.duration,
                r.refused_videos.clone(),
            )
        };
        if let Some(hit) = self
            .youtube
            .music_song(&artist, &title, length, true)
            .await
            .filter(|h| !refused.contains(&h.video_id))
        {
            debug!("'{artist} - {title}': video {} from YouTube Music", hit.video_id);
            if !self.keep_prewarmed_video(id, routing, &hit) {
                return;
            }
            if let Some(facts) = self.youtube.facts(&hit.video_id, true).await {
                self.id_registry.remember_stream(id, &hit.video_id, &facts);
            }
            return;
        }
        let Some(hit) = self
            .youtube
            .search_skipping(&format!("{artist} {title}"), length, true, &refused)
            .await
            .filter(|h| !h.video_id.is_empty())
        else {
            return;
        };
        debug!("'{artist} - {title}': video {} from yt-dlp", hit.video_id);
        if self.keep_prewarmed_video(id, routing, &hit) {
            self.id_registry.remember_stream(id, &hit.video_id, &hit.stream);
        }
    }

    /// Pins a prewarmed video and remembers its length as the video's. False when a play
    /// pinned another video meanwhile, which keeps it.
    fn keep_prewarmed_video(&self, id: &str, routing: &SharedRouting, hit: &YouTubeHit) -> bool {
        if routing.lock().refuses_video(&hit.video_id) {
            return false;
        }
        if !self.id_registry.pin_video(id, &hit.video_id, false) {
            return false;
        }
        self.id_registry.remember_pick(id, &hit.video_id, hit.pick);
        if let Some(d) = hit.duration {
            routing.lock().duration = Some(d);
        }
        self.id_registry
            .remember_length(id, hit.duration, LengthSource::Video);
        true
    }

    /// After a search answered: every other query for the rows its one track search missed,
    /// and the album years it skipped, one at a time on the catalog's background lane. What
    /// is found goes on the routing and in the catalog's cache, where the next answer for the
    /// rows reads it. Rows still without a length then join the length lookup.
    fn finish_rows(self: &Arc<Self>, rows: Vec<RowToFinish>) {
        if rows.is_empty() {
            return;
        }
        let this = Arc::clone(self);
        let task = tokio::spawn(async move {
            let mut still_missing = Vec::new();
            for row in rows {
                match row {
                    RowToFinish::Missed { id, artist, title } => {
                        let meta = tokio::time::timeout(
                            LENGTH_LOOKUP_TIMEOUT,
                            this.deezer.enrich_track(&artist, &title, true, true),
                        )
                        .await
                        .ok()
                        .flatten();
                        if let Some(meta) = &meta {
                            this.reflect_on_routing(&id, meta);
                        }
                        if !meta.and_then(|m| m.duration).is_some_and(|d| d > 0) {
                            still_missing.push((id, artist, title));
                        }
                    }
                    RowToFinish::Year(album_id) => {
                        let _ = tokio::time::timeout(
                            LENGTH_LOOKUP_TIMEOUT,
                            this.deezer.warm_album_year(album_id),
                        )
                        .await;
                    }
                }
            }
            this.warm_lengths(still_missing);
        });
        *self.last_rows_finish.lock() = Some(task.map(|_| ()).boxed().shared());
    }

    /// The album year of one outside song without one: the year its track lookup found, else
    /// the album's year already known, else one album lookup on the interactive lane waited for
    /// at most `wait`. A lookup still going by then is left to finish on the background lane,
    /// where it fills the cache for the next answer, and the song keeps no year.
    async fn complete_song_year(&self, song: &mut Song, wait: Duration) {
        if song.is_local || song.year.is_some_and(|y| y > 0) {
            return;
        }
        let Some(meta) = self.deezer.cached_track(&song.artist, &song.title) else {
            return;
        };
        if let Some(year) = meta.year {
            song.year = Some(year);
            return;
        }
        let Some(album_id) = meta.album_id.filter(|&id| id > 0) else {
            return;
        };
        if let Some(known) = self.deezer.cached_album_year(album_id) {
            song.year = known;
            return;
        }
        match tokio::time::timeout(wait, self.deezer.album_year_now(album_id)).await {
            Ok(year) => song.year = year,
            Err(_) => {
                let deezer = Arc::clone(&self.deezer);
                tokio::spawn(async move {
                    deezer.warm_album_year(album_id).await;
                });
            }
        }
    }

    /// Complete the rows below the first page from what is already known, then fetch the
    /// rest off the critical path so the next search for this query can complete them too.
    ///
    /// Reading the cache is free, so it happens inline and the rows it answers are real in
    /// THIS response. The fetch is not free, and awaiting it was worse than the bug it
    /// fixed: a 4s budget added 4s to every search to fill about ten rows, and a page only
    /// converged after five searches. Off the critical path the same work costs nothing and
    /// the second search answers all of it from cache.
    ///
    /// The warm still writes nothing back to a Song: what it finds goes on the routing
    /// instead, where the next response for the song reads it.
    ///
    /// Returns the rows the cache could not answer, for the caller to hand to that warm.
    fn enrich_remaining(&self, songs: Vec<&mut Song>) -> Vec<(String, String, String)> {
        let mut cold = Vec::new();
        for song in songs {
            let Some(meta) = self.deezer.cached_track(&song.artist, &song.title) else {
                cold.push(key_of(song));
                continue;
            };
            self.apply_track_meta(song, &meta);
            self.reflect_on_routing(&song.id, &meta);
        }
        cold
    }

    /// Look lengths up off the request, one song at a time, and store what is found on the
    /// registry. Order: Deezer, then Last.fm's track.getInfo, then a YouTube video's length
    /// inside the sane range. Songs that already have a metadata length are skipped.
    fn warm_lengths(self: &Arc<Self>, songs: impl IntoIterator<Item = (String, String, String)>) {
        let mut queued = Vec::new();
        for (id, artist, title) in songs {
            if id.is_empty() || dotnet::is_blank(&title) {
                continue;
            }
            match self.lookup(&id) {
                Some(routing) if !SongLength::has_metadata_length(&routing.lock()) => {}
                _ => continue,
            }
            if !self.length_lookups.lock().insert(id.clone()) {
                continue;
            }
            queued.push((id, artist, title));
        }
        if queued.is_empty() {
            return;
        }

        let this = Arc::clone(self);
        let task = tokio::spawn(async move {
            // Sequential on purpose: this has no deadline, and fanning out here is what
            // would eat the Deezer quota a live search needs. The year is skipped because
            // it costs a second request per album and no row shows it.
            for (id, artist, title) in queued {
                // Best-effort; the song keeps the placeholder until next time.
                let _ =
                    tokio::time::timeout(LENGTH_LOOKUP_TIMEOUT, this.look_up_length(&id, &artist, &title))
                        .await;
                this.length_lookups.lock().remove(&id);
            }
        });
        *self.last_length_warm.lock() = Some(task.map(|_| ()).boxed().shared());
    }

    async fn look_up_length(&self, id: &str, artist: &str, title: &str) {
        let meta = self.deezer.enrich_track(artist, title, false, true).await;
        if self
            .id_registry
            .remember_length(id, meta.and_then(|m| m.duration), LengthSource::Deezer)
        {
            return;
        }

        if let Some(last_fm) = self.last_fm.as_ref().filter(|l| l.has_api_key()) {
            let duration = last_fm.track_duration(artist, title).await;
            if self
                .id_registry
                .remember_length(id, duration, LengthSource::LastFm)
            {
                return;
            }
        }

        // Last, and only while the shim has room for background work: a video's length is
        // the weakest guess there is, and not worth making a play wait for.
        if let Some(routing) = self.lookup(id)
            && SongLength::shown(&routing.lock()).1 >= LengthSource::Video
        {
            return;
        }
        let Some(_permit) = bounded_wait(&self.prewarm_gate).await else {
            return;
        };
        // Length only. The video is not pinned for playback, so which video plays and
        // what a download is checked against both stay as they were.
        let hit = self.youtube.meta(&format!("{artist} {title}"), None, true).await;
        let hit = hit.filter(|h| {
            !self
                .lookup(id)
                .is_some_and(|r| r.lock().refuses_video(&h.video_id))
        });
        self.id_registry
            .remember_length(id, hit.and_then(|h| h.duration), LengthSource::Video);
    }

    /// The releases of the catalog artist an outside artist's name stands for, or none when no
    /// catalog artist has that name. The catalog id Octo already holds wins over a name search:
    /// it is the artist the user tapped in search, or the one an earlier visit settled on. On a
    /// library artist's page it must also share an album with the library, because two artists
    /// can share a name and the library says which one is meant. Without an id, only artists
    /// with this exact name count; of several, the one sharing the most albums with the
    /// library, else the one more people follow.
    ///
    /// The choice is kept for the next visit, and where depends on the page. The artist's
    /// routing is shared by everyone who reaches that name, from search, an album or a song,
    /// so an outside page keeps its choice there. A library artist's page keeps its own apart,
    /// under `page_key`: written onto the routing, the library's namesake
    /// became every listener's search row and outside page for the name.
    async fn find_artist_releases(
        &self,
        routing: SharedRouting,
        name: String,
        owned: HashSet<String>,
        page_key: String,
    ) -> Vec<AlbumHit> {
        let library_page = !owned.is_empty();
        let picked = if library_page {
            self.library_picks.lock().get(&page_key).cloned()
        } else {
            None
        };
        let known_id = picked.or_else(|| routing.lock().external_artist_id.clone());

        let mut known_releases: Option<Vec<AlbumHit>> = None;
        if let Some(known) = known_id.filter(|k| !k.is_empty()) {
            let releases = self.deezer.get_artist_albums(&known, &name).await;
            if !library_page {
                return releases;
            }
            if shared_count(&releases, &owned) > 0 {
                self.keep_library_pick(&page_key, &known);
                return releases;
            }
            known_releases = Some(releases);
        }

        let candidates = same_name(self.deezer.search_artists(&name, ARTIST_CANDIDATES).await, &name);
        let Some(mut pick) = most_followed(&candidates).cloned() else {
            return known_releases.unwrap_or_default();
        };
        let mut releases: Option<Vec<AlbumHit>> = None;
        if candidates.len() > 1 && !owned.is_empty() {
            let mut best = 0;
            // OrderByDescending(Fans): a stable sort, ties in the catalog's order.
            let mut by_fans: Vec<&ArtistHit> = candidates.iter().collect();
            by_fans.sort_by_key(|hit| std::cmp::Reverse(hit.fans));
            for candidate in by_fans.into_iter().take(ARTISTS_COMPARED) {
                let theirs = self.deezer.get_artist_albums(&candidate.deezer_id, &name).await;
                let shared = shared_count(&theirs, &owned);
                if shared <= best {
                    continue;
                }
                best = shared;
                pick = candidate.clone();
                releases = Some(theirs);
            }
        }
        let releases = match releases {
            Some(releases) => releases,
            None => self.deezer.get_artist_albums(&pick.deezer_id, &name).await,
        };

        if library_page {
            self.keep_library_pick(&page_key, &pick.deezer_id);
        } else {
            let changed = {
                let mut r = routing.lock();
                if r.external_artist_id.as_deref() != Some(pick.deezer_id.as_str()) {
                    r.external_artist_id = Some(pick.deezer_id.clone());
                    true
                } else {
                    false
                }
            };
            if changed {
                self.id_registry.register(routing.clone());
            }
        }
        releases
    }

    fn keep_library_pick(&self, page_key: &str, deezer_artist_id: &str) {
        let mut picks = self.library_picks.lock();
        if picks.len() >= LIBRARY_PICKS_KEPT && !picks.contains_key(page_key) {
            picks.clear();
        }
        picks.insert(page_key.to_string(), deezer_artist_id.to_string());
    }

    /// The catalog artist of this exact name an outside artist stands for: the one
    /// Octo already settled on, else the one more people follow. None when none has the name.
    /// The search is the one the album listing makes, so it costs nothing more.
    async fn find_catalog_artist(&self, routing: &SoulseekRouting) -> Option<ArtistHit> {
        let name = non_empty(routing.artist.as_deref())?;
        let candidates = same_name(self.deezer.search_artists(name, ARTIST_CANDIDATES).await, name);
        candidates
            .iter()
            .find(|hit| routing.external_artist_id.as_deref() == Some(hit.deezer_id.as_str()))
            .or_else(|| most_followed(&candidates))
            .cloned()
    }

    /// The listing carries no track counts. Each album's own record has one. Counts already
    /// known cost nothing; of the rest, the first few on the page are asked a few at a time, and
    /// the page waits a moment for them. What arrives in time is shown and the rest are kept for
    /// the next visit, so a long career fills in over a visit or two without flooding the
    /// catalog's quota. Without `fill`, only the counts already known.
    async fn fill_track_counts(&self, releases: &[AlbumHit], fill: bool) -> Vec<Option<i32>> {
        let counts = Arc::new(Mutex::new(vec![None; releases.len()]));
        let mut lookups = Vec::new();
        // Lookups still waiting when the page answers keep using it.
        let gate = Arc::new(Semaphore::new(TRACK_COUNTS_AT_ONCE));
        for (i, release) in releases.iter().enumerate() {
            if release.track_count > 0 {
                counts.lock()[i] = Some(release.track_count);
            } else if let Some(known) = self.deezer.try_known_track_count(&release.deezer_id) {
                counts.lock()[i] = known;
            } else if fill && lookups.len() < TRACK_COUNTS_PER_VISIT {
                let (deezer, gate, counts, id) = (
                    Arc::clone(&self.deezer),
                    Arc::clone(&gate),
                    Arc::clone(&counts),
                    release.deezer_id.clone(),
                );
                lookups.push(tokio::spawn(async move {
                    let Ok(_permit) = gate.acquire().await else { return };
                    let count = deezer.album_track_count(&id).await;
                    counts.lock()[i] = count;
                }));
            }
        }
        if !lookups.is_empty() {
            let _ = tokio::time::timeout(TRACK_COUNT_WAIT, futures::future::join_all(lookups)).await;
        }
        counts.lock().clone()
    }

    fn to_albums(&self, name: &str, walk: &ArtistWalk) -> Vec<Album> {
        walk.releases
            .iter()
            .zip(&walk.counts)
            .map(|(release, count)| {
                let words = (release.explicit_lyrics == Some(true)).then_some(explicit_status::EXPLICIT);
                let album_id = self.id_registry.register(SoulseekRouting {
                    kind: RoutingKind::Album,
                    artist: Some(name.to_string()),
                    album: Some(release.title.clone()),
                    external_album_id: Some(release.deezer_id.clone()),
                    explicit_content: words,
                    ..Default::default()
                });
                Album {
                    id: album_id.clone(),
                    title: release.title.clone(),
                    year: release.year,
                    song_count: Some(count.unwrap_or(release.track_count)),
                    cover_art_url: release.cover_url.clone(),
                    release_types: release_type_list(release.record_type.as_deref()),
                    explicit_content_lyrics: words,
                    is_local: false,
                    external_provider: Some(SoulseekMetadataService::PROVIDER_NAME.to_string()),
                    external_id: Some(album_id),
                    ..Default::default()
                }
            })
            .collect()
    }

    async fn artist_albums(
        self: &Arc<Self>,
        external_provider: &str,
        external_id: &str,
        library_album_titles: Option<&[String]>,
        fill_counts: bool,
    ) -> Vec<Album> {
        if !is_ours(external_provider) {
            return Vec::new();
        }
        let Some(routing) = self.lookup(external_id) else {
            return Vec::new();
        };
        let Some(name) = non_empty(routing.lock().artist.as_deref()).map(str::to_string) else {
            return Vec::new();
        };

        let owned: HashSet<String> = library_album_titles
            .unwrap_or_default()
            .iter()
            .map(|title| SongIdentity::key(title))
            .filter(|key| !key.is_empty())
            .collect();
        let mut sorted: Vec<&String> = owned.iter().collect();
        sorted.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let key = format!(
            "{external_id}|{}",
            sorted
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\u{1f}")
        );

        let releases = {
            let (this, routing, name, owned, key) = (
                Arc::clone(self),
                routing.clone(),
                name.clone(),
                owned.clone(),
                key.clone(),
            );
            move || {
                let walk = this.clone();
                this.release_walks.run(&key.clone(), async move {
                    walk.find_artist_releases(routing, name, owned, key).await
                })
            }
        };

        let walk = if fill_counts {
            let this = Arc::clone(self);
            self.count_walks
                .run(&key, async move {
                    let found = releases().await;
                    let counts = this.fill_track_counts(&found, true).await;
                    ArtistWalk {
                        releases: found,
                        counts,
                    }
                })
                .await
        } else {
            let found = releases().await;
            let counts = self.fill_track_counts(&found, false).await;
            ArtistWalk {
                releases: found,
                counts,
            }
        };

        // Each caller gets albums of its own: a library artist's page relinks them to itself.
        self.to_albums(&name, &walk)
    }

    /// An album the catalog cannot list still has the songs Octo showed under it: at least the
    /// one whose row named it. Listing those instead of nothing is what keeps a client that
    /// opens the album of the song it is playing (Tempo, #59) from finding it empty.
    fn list_songs_filed_under(
        &self,
        album: &mut Album,
        routing: &SoulseekRouting,
        placeholder: &str,
        artist_id: &str,
    ) {
        let mut filed: Vec<(String, SoulseekRouting)> = self
            .id_registry
            .songs_filed_under(
                routing.artist.as_deref(),
                Some(placeholder),
                SONGS_FILED_UNDER_LIMIT,
            )
            .into_iter()
            .map(|(id, shared)| (id, shared.snapshot()))
            .collect();
        // OrderBy(disc ?? 1).ThenBy(track ?? int.MaxValue), stable.
        filed.sort_by_key(|(_, song)| (song.disc_number.unwrap_or(1), song.track.unwrap_or(i32::MAX)));
        for (id, song) in filed {
            album.songs.push(Song {
                id: id.clone(),
                title: song.title.clone().unwrap_or_default(),
                artist: song.artist.clone().unwrap_or_default(),
                artist_id: Some(artist_id.to_string()),
                album: album.title.clone(),
                album_id: Some(album.id.clone()),
                duration: SongLength::shown(&song).0.or(song.duration),
                track: song.track,
                disc_number: song.disc_number,
                isrc: song.isrc.clone(),
                year: album.year,
                cover_art_url: album.cover_art_url.clone(),
                is_local: false,
                external_provider: Some(SoulseekMetadataService::PROVIDER_NAME.to_string()),
                external_id: Some(id),
                ..Default::default()
            });
        }
        if !album.songs.is_empty() {
            album.song_count = Some(album.songs.len() as i32);
        }
    }
}

/// What the length warm needs of a song: its id, artist and title.
fn key_of(song: &Song) -> (String, String, String) {
    (song.id.clone(), song.artist.clone(), song.title.clone())
}

#[async_trait]
impl IMusicMetadataService for SoulseekMetadataService {
    async fn search_songs(&self, query: &str, _limit: i32) -> Vec<Song> {
        if dotnet::is_blank(query) {
            return Vec::new();
        }
        let (query_artist, query_title) = parse_query(query);
        self.search_songs_by_artist_title(&query_artist, query_title.as_deref().unwrap_or(query), 1, None)
            .await
    }

    async fn search_songs_by_artist_title(
        &self,
        artist: &str,
        title: &str,
        _limit: i32,
        duration_seconds: Option<i32>,
    ) -> Vec<Song> {
        if dotnet::is_blank(artist) && dotnet::is_blank(title) {
            return Vec::new();
        }

        // INSTANT placeholder. We do NOT call YouTube here: at queue-build time we'd
        // rate-limit ourselves into oblivion (Arpeggio fans out 5-10 search3 calls
        // per radio session). YouTube resolution is deferred to /rest/stream where
        // it happens once per actual playback, sequentially as the user advances.
        let registry = &self.inner.id_registry;
        let external_id = registry.register(SoulseekRouting {
            // YouTubeId intentionally null: resolved lazily on play.
            artist: Some(artist.to_string()),
            title: Some(title.to_string()),
            duration: duration_seconds,
            ..Default::default()
        });

        debug!(
            "Placeholder song registered for '{artist} - {title}' (dur={duration_seconds:?}) -> id {external_id}"
        );

        // Every caller that hands in a length got it from Last.fm, or from a play's own
        // record. Stored by rank, so a Deezer length an earlier lookup found for this id
        // still wins, and that is the length this row goes out with.
        registry.remember_length(&external_id, duration_seconds, LengthSource::LastFm);
        let remembered = registry
            .lookup(&external_id)
            .and_then(|routing| SongLength::shown(&routing.lock()).0);

        // No length is known: the song goes out without one rather than with a guess. A
        // made-up length misleads a player planning a crossfade, while a missing one only
        // waits for the stream to report the real length.
        let effective_duration = remembered.or(duration_seconds.filter(|&d| d > 0));
        let explicit = registry
            .lookup(&external_id)
            .and_then(|routing| routing.lock().explicit_content);

        vec![Song {
            id: external_id.clone(),
            title: title.to_string(),
            artist: artist.to_string(),
            album: String::new(),
            duration: effective_duration,
            explicit_content_lyrics: explicit,
            is_local: false,
            external_provider: Some(Self::PROVIDER_NAME.to_string()),
            external_id: Some(external_id),
            ..Default::default()
        }]
    }

    async fn enrich_external_songs(&self, songs: &mut [Song]) {
        let inner = &self.inner;
        let mut external: Vec<&mut Song> = songs.iter_mut().filter(|s| !s.is_local).collect();
        let rest = external.split_off(external.len().min(SEARCH_ENRICH_LIMIT));
        let first_keys: Vec<(String, String, String)> = external.iter().map(|s| key_of(s)).collect();

        // First-page rows Deezer gave no length for. They join the background lookup below
        // rather than going out with no length for good.
        let missed: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
        futures::stream::iter(external)
            .for_each_concurrent(8, |song| {
                let missed = &missed;
                async move {
                    let meta = inner
                        .deezer
                        .enrich_track(&song.artist, &song.title, true, false)
                        .await;
                    if !meta.as_ref().and_then(|m| m.duration).is_some_and(|d| d > 0) {
                        missed.lock().insert(song.id.clone());
                    }
                    let Some(meta) = meta else { return };
                    inner.apply_track_meta(song, &meta);
                    if let Some(y) = meta.year {
                        song.year = Some(y);
                    }
                    inner.reflect_on_routing(&song.id, &meta);
                }
            })
            .await;

        let rest: Vec<&mut Song> = rest
            .into_iter()
            .take(BACKGROUND_ENRICH_LIMIT - SEARCH_ENRICH_LIMIT)
            .collect();
        let cold = inner.enrich_remaining(rest);
        let missed = missed.into_inner();
        let first_missed = first_keys.into_iter().filter(|(id, _, _)| missed.contains(id));
        inner.warm_lengths(first_missed.chain(cold));
    }

    async fn search_catalog_tracks(&self, query: &str, limit: i32) -> Option<Vec<(String, String)>> {
        self.inner.deezer.search_tracks(query, limit).await
    }

    async fn enrich_search_rows(&self, songs: &mut [Song]) {
        let inner = &self.inner;
        let mut external: Vec<&mut Song> = songs.iter_mut().filter(|s| !s.is_local).collect();
        let rest = external.split_off(external.len().min(SEARCH_ENRICH_LIMIT));

        // One track search per row while the search waits; the year and every other query for
        // a row it missed run after the answer, on the background lane.
        let finish: Mutex<Vec<RowToFinish>> = Mutex::new(Vec::new());
        futures::stream::iter(external)
            .for_each_concurrent(8, |song| {
                let finish = &finish;
                async move {
                    let meta = inner.deezer.enrich_track_quick(&song.artist, &song.title).await;
                    let (id, artist, title) = key_of(song);
                    let Some(meta) = meta else {
                        finish.lock().push(RowToFinish::Missed { id, artist, title });
                        return;
                    };
                    inner.apply_track_meta(song, &meta);
                    if let Some(y) = meta.year {
                        song.year = Some(y);
                    }
                    inner.reflect_on_routing(&song.id, &meta);
                    if !meta.duration.is_some_and(|d| d > 0) {
                        finish.lock().push(RowToFinish::Missed { id, artist, title });
                    } else if meta.year.is_none()
                        && let Some(album_id) = meta.album_id
                    {
                        finish.lock().push(RowToFinish::Year(album_id));
                    }
                }
            })
            .await;

        let rest: Vec<&mut Song> = rest
            .into_iter()
            .take(BACKGROUND_ENRICH_LIMIT - SEARCH_ENRICH_LIMIT)
            .collect();
        let cold = inner.enrich_remaining(rest);
        inner.warm_lengths(cold);
        inner.finish_rows(finish.into_inner());
    }

    fn remember_file_length(&self, external_id: &str, seconds: i32) {
        self.inner
            .id_registry
            .remember_length(external_id, Some(seconds), LengthSource::File);
    }

    fn complete_song_lengths(&self, songs: &mut [Song]) {
        let inner = &self.inner;
        let mut cold = Vec::new();
        for song in songs.iter_mut() {
            if song.is_local || song.id.is_empty() {
                continue;
            }
            let Some(routing) = inner.lookup(&song.id) else {
                continue;
            };
            {
                let r = routing.lock();
                if r.kind != RoutingKind::Song || !r.has_artist_title() {
                    continue;
                }
                // A song rebuilt from its id (getSong) carries only the routing's own length,
                // so a row with none takes the length the registry remembered for display.
                if song.duration.is_none_or(|d| d <= 0) {
                    song.duration = SongLength::shown(&r).0.or(r.duration).filter(|&d| d > 0);
                }
                if SongLength::has_metadata_length(&r) {
                    continue;
                }
            }

            // Free: a search for the same song may have asked Deezer already.
            if let Some(d) = inner
                .deezer
                .cached_track(&song.artist, &song.title)
                .and_then(|m| m.duration)
                .filter(|&d| d > 0)
            {
                song.duration = Some(d);
                inner
                    .id_registry
                    .remember_length(&song.id, Some(d), LengthSource::Deezer);
                continue;
            }
            cold.push(key_of(song));
        }
        inner.warm_lengths(cold.into_iter().take(STATION_LENGTH_WARM_LIMIT));
    }

    async fn resolve_top_durations(&self, songs: &mut [Song], background: bool) {
        let inner = &self.inner;
        let tasks = songs
            .iter_mut()
            .filter(|s| !s.is_local)
            .take(TOP_DURATION_RESOLVE_LIMIT)
            .map(|song| async move {
                // The background pass runs after the client has the results, so a play may already
                // have pinned a video for this song. It keeps it, and the shim is spared the lookup.
                if background
                    && inner
                        .lookup(&song.id)
                        .is_some_and(|r| r.lock().you_tube_id.as_deref().is_some_and(|y| !y.is_empty()))
                {
                    return;
                }
                let gate = if background {
                    &inner.background_duration_gate
                } else {
                    &inner.prewarm_gate
                };
                let Some(_permit) = bounded_wait(gate).await else {
                    return;
                };
                inner.resolve_video_length(song, background).await;
            });
        futures::future::join_all(tasks).await;
    }

    fn prefetch_top_videos(&self, songs: &[Song]) {
        for song in songs
            .iter()
            .filter(|s| !s.is_local)
            .take(TOP_DURATION_RESOLVE_LIMIT)
        {
            self.inner.youtube.prefetch_music_song(&song.artist, &song.title);
        }
    }

    async fn resolve_top_durations_within(
        &self,
        songs: &mut [Song],
        budget: Duration,
    ) -> Option<JoinHandle<()>> {
        let targets: Vec<(usize, Song)> = songs
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.is_local)
            .take(TOP_DURATION_RESOLVE_LIMIT)
            .map(|(index, song)| (index, song.clone()))
            .collect();
        if targets.is_empty() {
            return None;
        }
        // Each row is settled under this lock: either before the answer goes out, and then its
        // length goes with the answer, or after, and then only the routing is written, as the
        // background pass does.
        let answer = Arc::new(Mutex::new(DurationsAnswer::default()));
        // A caller that stops waiting (its search was superseded) leaves the rest to run as
        // background lookups.
        let _sent_when_dropped = SentOnDrop(answer.clone());
        let this = self.clone();
        let shared = answer.clone();
        let mut pass = tokio::spawn(async move {
            let inner = &this.inner;
            let tasks = targets.into_iter().map(|(index, song)| {
                let answer = shared.clone();
                async move {
                    let Some(_permit) = bounded_wait(&inner.prewarm_gate).await else {
                        return;
                    };
                    // A row that starts its lookup after the answer went out is a background
                    // lookup: a video a play pinned meanwhile is kept, and the shim's background
                    // lane is used.
                    let late = answer.lock().sent;
                    if late
                        && inner
                            .lookup(&song.id)
                            .is_some_and(|r| r.lock().you_tube_id.as_deref().is_some_and(|y| !y.is_empty()))
                    {
                        return;
                    }
                    let refused = inner
                        .lookup(&song.id)
                        .map(|r| r.lock().refused_videos.clone())
                        .unwrap_or_default();
                    let Some(hit) = inner
                        .find_video(&song.artist, &song.title, song.duration, &refused, late)
                        .await
                    else {
                        return;
                    };
                    let mut answer = answer.lock();
                    let background = answer.sent;
                    if let Some(shown) = inner.keep_video(&song.id, &hit, background) {
                        answer.lengths.push((index, shown));
                    }
                }
            });
            futures::future::join_all(tasks).await;
        });
        let finished = tokio::time::timeout(budget, &mut pass).await.is_ok();
        {
            let mut answer = answer.lock();
            answer.sent = true;
            for &(index, shown) in &answer.lengths {
                songs[index].duration = Some(shown);
            }
        }
        (!finished).then_some(pass)
    }

    async fn complete_song_year(&self, song: &mut Song) {
        self.inner.complete_song_year(song, SONG_YEAR_WAIT).await;
    }

    fn refresh_outside_songs(&self, songs: &mut [Song]) {
        for song in songs.iter_mut() {
            if song.is_local || song.id.is_empty() {
                continue;
            }
            let Some(routing) = self.inner.lookup(&song.id) else {
                continue;
            };
            let r = routing.lock();
            if r.kind != RoutingKind::Song {
                continue;
            }
            // What the background found for a row the search sent without it: the album and
            // the explicit word on the routing, the year from the catalog's cache.
            if dotnet::is_blank(&song.album)
                && let Some(album) = r.album.as_deref().filter(|a| !dotnet::is_blank(a))
            {
                song.album = album.to_string();
            }
            if song.explicit_content_lyrics.is_none() {
                song.explicit_content_lyrics = r.explicit_content;
            }
            if song.year.is_none() {
                song.year = self
                    .inner
                    .deezer
                    .cached_track(&song.artist, &song.title)
                    .and_then(|meta| {
                        meta.year.or_else(|| {
                            meta.album_id
                                .and_then(|id| self.inner.deezer.cached_album_year(id))
                                .flatten()
                        })
                    });
            }
            // A pinned video's length is the one the durations pass writes on the routing, shown
            // only inside the sane range as that pass shows it. A row with no length takes the
            // remembered one, as a new search row does; never a length nothing vouched for.
            let pinned = r.you_tube_id.as_deref().is_some_and(|y| !y.is_empty());
            if pinned && let Some(video) = SongLength::sane_video_length(r.duration) {
                song.duration = Some(video);
            } else if song.duration.is_none_or(|d| d <= 0) {
                song.duration = SongLength::shown(&r).0.filter(|&d| d > 0);
            }
        }
    }

    async fn resolve_song_length(&self, song: &mut Song) {
        if song.is_local || song.id.is_empty() || song.duration.is_some_and(|d| d > 0) {
            return;
        }
        let Some(routing) = self.inner.lookup(&song.id) else {
            return;
        };
        {
            let r = routing.lock();
            if r.kind != RoutingKind::Song
                || !r.has_artist_title()
                || r.you_tube_id.as_deref().is_some_and(|y| !y.is_empty())
            {
                return;
            }
        }
        // Someone is waiting on this answer, so it never queues behind prewarm work.
        self.inner.resolve_video_length(song, false).await;
    }

    /// Fire-and-forget background prewarm: resolve the YouTube videoId (and via
    /// shim's automatic prefetch, the stream URL) for the first `top_n`
    /// placeholder songs from a search. Without this, Arpeggi's ~10s HTTP timeout
    /// fires while the cold yt-dlp ytsearch1: + yt-dlp -g chain is still running,
    /// the client cancels, and external songs never play.
    ///
    /// Only the top hits matter: search clients render in order and users almost
    /// never click past the first screen of results. Resolving 150 placeholders
    /// would saturate the shim's yt-dlp gate and waste work.
    async fn prewarm_you_tube_ids(&self, songs: &[Song], top_n: usize) {
        let ids: Vec<String> = songs
            .iter()
            .filter(|s| !s.id.is_empty())
            .map(|s| s.id.clone())
            .collect();
        self.prewarm_you_tube_ids_for_song_ids(&ids, top_n).await;
    }

    async fn prewarm_you_tube_ids_for_song_ids(&self, song_ids: &[String], top_n: usize) {
        let inner = &self.inner;
        // Skip ids whose YouTube resolution is already cached on the routing:
        // those are already warm and don't need a yt-dlp roundtrip. This is the
        // path used by the scrobble-driven sliding window: as the user advances
        // through a queue most upcoming items will still be cold, but if they
        // jump back to one we resolved earlier we don't burn shim cycles re-doing it.
        let targets: Vec<(String, SharedRouting)> = song_ids
            .iter()
            .filter(|id| !id.is_empty())
            .filter_map(|id| inner.lookup(id).map(|routing| (id.clone(), routing)))
            .filter(|(_, routing)| {
                let r = routing.lock();
                r.you_tube_id.as_deref().is_none_or(str::is_empty) && r.has_artist_title()
            })
            .take(top_n)
            .collect();
        if targets.is_empty() {
            return;
        }

        let tasks = targets.into_iter().map(|(id, routing)| async move {
            // Bounded wait, then drop. With a shared limiter an unbounded wait
            // lets a skip-happy user pile up prewarm tasks for songs they left
            // behind five tracks ago. Prewarm is best-effort by design, so its
            // queueing is best-effort too.
            let Some(_permit) = bounded_wait(&inner.prewarm_gate).await else {
                return;
            };
            if routing
                .lock()
                .you_tube_id
                .as_deref()
                .is_some_and(|y| !y.is_empty())
            {
                return;
            }
            inner.pick_and_warm_video(&id, &routing).await;
        });
        futures::future::join_all(tasks).await;
    }

    async fn prewarm_next_streams(&self, song_ids: &[String], count: usize) -> Vec<String> {
        let inner = &self.inner;
        let next: Vec<(String, SharedRouting)> = song_ids
            .iter()
            .filter(|id| !id.is_empty())
            .filter_map(|id| inner.lookup(id).map(|routing| (id.clone(), routing)))
            .filter(|(_, routing)| {
                let r = routing.lock();
                r.kind == RoutingKind::Song && r.has_artist_title()
            })
            .take(count)
            .collect();
        let readied: Vec<String> = next.iter().map(|(id, _)| id.clone()).collect();
        let tasks = next.into_iter().map(|(id, routing)| async move {
            let (pinned, resolved) = {
                let r = routing.lock();
                (
                    r.you_tube_id.clone().filter(|y| !y.is_empty()),
                    r.stream_size.is_some(),
                )
            };
            if pinned.is_some() && resolved {
                return;
            }
            let Some(_permit) = bounded_wait(&inner.prewarm_gate).await else {
                return;
            };
            match pinned {
                // The video is known but its stream was never resolved: asking for its facts
                // resolves it, as a play would.
                Some(video) => {
                    if let Some(facts) = inner.youtube.facts(&video, true).await {
                        inner.id_registry.remember_stream(&id, &video, &facts);
                    }
                }
                None => inner.pick_and_warm_video(&id, &routing).await,
            }
        });
        futures::future::join_all(tasks).await;
        readied
    }

    /// Fire-and-forget background prewarm of cover art for the first `top_n`
    /// songs of a search, so a client that renders them a moment later finds the image
    /// already in [`CoverArtAggregator`]'s cache. Uses its own gate, separate from the
    /// shim-bound YouTube prewarm gate, and passes background: true through to the cover
    /// sources so this can never queue behind a live search or getCoverArt request.
    async fn prewarm_cover_art(&self, songs: &[Song], top_n: usize) {
        let inner = &self.inner;
        let targets: Vec<&Song> = songs.iter().filter(|s| !s.is_local).take(top_n).collect();
        if targets.is_empty() {
            return;
        }
        let tasks = targets.into_iter().map(|song| async move {
            let Some(_permit) = bounded_wait(&inner.cover_art_prewarm_gate).await else {
                return;
            };
            let routing = match inner.lookup(&song.id) {
                Some(routing) => routing.snapshot(),
                None => SoulseekRouting {
                    kind: RoutingKind::Song,
                    artist: Some(song.artist.clone()),
                    title: Some(song.title.clone()),
                    ..Default::default()
                },
            };
            inner.cover_art.get_cover(&routing, true).await;
        });
        futures::future::join_all(tasks).await;
    }

    async fn search_albums(&self, query: &str, limit: i32) -> Vec<Album> {
        self.inner.album_search(query, limit, false).await
    }

    async fn search_album_rows(&self, query: &str, limit: i32) -> Vec<Album> {
        self.inner.album_search(query, limit, true).await
    }

    async fn search_artists(&self, query: &str, limit: i32) -> Vec<Artist> {
        if dotnet::is_blank(query) || limit <= 0 {
            return Vec::new();
        }
        let inner = &self.inner;
        let hits = inner.deezer.search_artists(query, limit).await;

        // Two catalog artists of one name get one id, so they are one row: two rows opening
        // the same page would only confuse. The row is the artist Octo already settled on for
        // that name, else the one more people follow. (GroupBy keeps first-appearance order.)
        let mut groups: Vec<(String, Vec<ArtistHit>)> = Vec::new();
        for hit in hits {
            match groups.iter_mut().find(|(name, _)| *name == hit.name) {
                Some((_, group)) => group.push(hit),
                None => groups.push((hit.name.clone(), vec![hit])),
            }
        }

        let mut artists = Vec::with_capacity(groups.len());
        for (name, same) in groups {
            // Same registry id an album row mints for its artist, because the seed is the
            // artist name alone. So an artist found here and the same artist reached from
            // an album are one entity, and getArtist answers for both.
            let id = inner.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some(name),
                ..Default::default()
            });
            let routing = inner.lookup(&id);
            let settled = routing.as_ref().and_then(|r| r.lock().external_artist_id.clone());
            let hit = same
                .iter()
                .find(|h| settled.as_deref() == Some(h.deezer_id.as_str()))
                .or_else(|| most_followed(&same))
                .expect("a group is never empty")
                .clone();
            // Remembered only when nothing is yet: an earlier search or visit to the artist's
            // page already settled which artist of the name this is. An album never settles
            // it: the name's entry is everyone's, and the first album to show a little-known
            // namesake would have decided the name for all of them.
            if let Some(routing) = routing {
                let unsettled = {
                    let mut r = routing.lock();
                    let unsettled = r.external_artist_id.is_none();
                    if unsettled {
                        r.external_artist_id = Some(hit.deezer_id.clone());
                    }
                    unsettled
                };
                if unsettled {
                    inner.id_registry.register(routing);
                }
            }

            artists.push(Artist {
                id: id.clone(),
                name: hit.name.clone(),
                image_url: hit.picture_url.clone(),
                album_count: Some(hit.album_count),
                is_local: false,
                external_provider: Some(Self::PROVIDER_NAME.to_string()),
                external_id: Some(id),
            });
        }

        artists
    }

    async fn search_all(
        &self,
        query: &str,
        song_limit: i32,
        _album_limit: i32,
        _artist_limit: i32,
    ) -> SearchResult {
        let songs = self.search_songs(query, song_limit).await;
        SearchResult {
            songs,
            albums: Vec::new(),
            artists: Vec::new(),
        }
    }

    async fn get_song(&self, external_provider: &str, external_id: &str) -> Option<Song> {
        if !is_ours(external_provider) {
            return None;
        }

        // Never registers: ids a client sends must not fill the registry.
        let routing = self
            .inner
            .id_registry
            .resolve_song(external_id, false)?
            .snapshot();

        Some(Song {
            id: external_id.to_string(),
            title: routing.title.clone().unwrap_or_default(),
            artist: routing.artist.clone().unwrap_or_default(),
            // Carried from the routing so an album download tags the album the user
            // actually hearted rather than whatever Deezer guesses from artist+title,
            // and keeps its position so the album stays in order.
            album: routing.album.clone().unwrap_or_default(),
            track: routing.track,
            disc_number: routing.disc_number,
            total_tracks: routing.total_tracks,
            duration: routing.duration,
            isrc: routing.isrc.clone(),
            explicit_content_lyrics: routing.explicit_content,
            is_local: false,
            external_provider: Some(Self::PROVIDER_NAME.to_string()),
            external_id: Some(external_id.to_string()),
            ..Default::default()
        })
    }

    async fn get_album(&self, external_provider: &str, external_id: &str) -> Option<Album> {
        if !is_ours(external_provider) {
            return None;
        }
        let inner = &self.inner;
        let routing = inner.lookup(external_id)?.snapshot();

        let placeholder = routing
            .album
            .clone()
            .or_else(|| routing.title.clone())
            .unwrap_or_default();
        // Enrich by the track title (the placeholder "album" is the song title) so
        // Deezer returns the REAL album (e.g. "Creep" -> "Pablo Honey"). Degrades
        // to the placeholder name if Deezer misses or is unreachable.
        let artist_id = inner.id_registry.register(SoulseekRouting {
            kind: RoutingKind::Artist,
            artist: routing.artist.clone(),
            ..Default::default()
        });
        let artist = routing.artist.as_deref().unwrap_or("");

        // Resolve the Deezer album two ways. A search-derived routing already knows the
        // exact id. One minted from a song row does not, so recover the REAL album name
        // first (the placeholder is the song title, e.g. "Creep" -> "Pablo Honey") and
        // look the id up by name.
        let mut meta: Option<TrackMeta> = None;
        let mut deezer_album_id = routing.external_album_id.clone();
        if deezer_album_id.as_deref().is_none_or(str::is_empty) {
            let track = routing
                .album
                .as_deref()
                .or(routing.title.as_deref())
                .unwrap_or("");
            meta = inner.deezer.enrich_track(artist, track, true, false).await;
            let album_name = meta
                .as_ref()
                .and_then(|m| m.album_title.clone())
                .unwrap_or_else(|| placeholder.clone());
            deezer_album_id = inner.deezer.find_album_id(artist, &album_name).await;
            // Kept on the album, so a listing of it names the catalog's album as its songs'
            // context.
            if let Some(found) = deezer_album_id.as_deref().filter(|id| !id.is_empty())
                && let Some(shared) = inner.lookup(external_id)
            {
                let mut album = shared.lock();
                if album.external_album_id.as_deref().is_none_or(str::is_empty) {
                    album.external_album_id = Some(found.to_string());
                }
            }
        }

        let mut album = Album {
            id: external_id.to_string(),
            title: meta
                .as_ref()
                .and_then(|m| m.album_title.clone())
                .unwrap_or_else(|| placeholder.clone()),
            artist: artist.to_string(),
            artist_id: Some(artist_id.clone()),
            year: meta.as_ref().and_then(|m| m.year),
            cover_art_url: meta.as_ref().and_then(|m| m.album_cover_url.clone()),
            is_local: false,
            external_provider: Some(Self::PROVIDER_NAME.to_string()),
            external_id: Some(external_id.to_string()),
            ..Default::default()
        };

        let Some(deezer_album_id) = deezer_album_id.filter(|id| !id.is_empty()) else {
            // Logged rather than silent: this is what a user sees as an album that opens
            // with no tracks, and without a line here there is nothing to diagnose from.
            inner.list_songs_filed_under(&mut album, &routing, &placeholder, &artist_id);
            warn!(
                "getAlbum '{artist} - {placeholder}' ({external_id}): no Deezer album id resolved; listing the {} song(s) filed under it",
                album.songs.len()
            );
            return Some(album);
        };

        let lookup = inner.deezer.look_up_album_detail(&deezer_album_id).await;
        // An album with no resolvable tracklist must still render, so fall through with
        // whatever we already have rather than failing the request.
        let Some(detail) = lookup.detail else {
            // Deezer only failed to answer this time: no songs are filed in, because a
            // partial list would be taken for the whole album by a client that caches what
            // it syncs. When Deezer answered that it has no such album, or no tracks for it,
            // the songs filed under it are all there is to list.
            let stand_in = lookup.answer != AlbumAnswer::Unavailable;
            if stand_in {
                inner.list_songs_filed_under(&mut album, &routing, &placeholder, &artist_id);
            }
            let outcome = if stand_in {
                format!("listing the {} song(s) filed under it", album.songs.len())
            } else {
                "returning album without a tracklist".to_string()
            };
            warn!(
                "getAlbum '{artist} - {placeholder}' ({external_id}): Deezer album {deezer_album_id} returned no usable detail ({:?}; see the deezer warning above for why); {outcome}",
                lookup.answer
            );
            return Some(album);
        };

        album.title = detail.title.clone();
        album.year = detail.year.or(album.year);
        album.genre = detail.genre.clone();
        album.cover_art_url = detail.cover_url.clone().or(album.cover_art_url);
        album.release_types = release_type_list(detail.record_type.as_deref());
        album.explicit_content_lyrics = detail.explicit_content.or(routing.explicit_content);
        // Defence in depth: the Deezer layer no longer returns a tracklist-less album,
        // but if one ever gets through, reporting zero is worse than saying nothing.
        if !detail.tracks.is_empty() {
            album.song_count = Some(detail.tracks.len() as i32);
        }
        if !dotnet::is_blank(&detail.artist) {
            album.artist = detail.artist.clone();
        }
        // Deezer says the album has no tracks at all: the songs filed under it stand in.
        if detail.tracks.is_empty() {
            inner.list_songs_filed_under(&mut album, &routing, &placeholder, &artist_id);
        }

        for track in &detail.tracks {
            // Album is carried on the ROUTING as well as the Song. The download path re-resolves
            // each track by id through `get_song`, and without this the tagger re-derives the
            // album from artist+title alone, which for a well known single often lands on a
            // greatest-hits record instead of this one.
            let track_id = inner.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some(track.artist.clone()),
                title: Some(track.title.clone()),
                album: Some(detail.title.clone()),
                duration: track.duration,
                track: track.track_position,
                disc_number: track.disc_number,
                total_tracks: Some(detail.tracks.len() as i32),
                isrc: track.isrc.clone(),
                // A clean edit's track has an id of its own, apart from the explicit album's.
                explicit_content: track.explicit_content,
                version: explicit_status::version_of(track.explicit_content),
                ..Default::default()
            });

            album.songs.push(Song {
                id: track_id.clone(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                artist_id: Some(artist_id.clone()),
                album: detail.title.clone(),
                album_id: Some(external_id.to_string()),
                duration: track.duration,
                track: track.track_position,
                disc_number: track.disc_number,
                isrc: track.isrc.clone(),
                explicit_content_lyrics: track.explicit_content,
                year: detail.year,
                genre: detail.genre.clone(),
                cover_art_url: detail.cover_url.clone(),
                cover_art_url_large: detail.cover_url.clone(),
                album_artist: Some(detail.artist.clone()),
                label: detail.label.clone(),
                is_local: false,
                external_provider: Some(Self::PROVIDER_NAME.to_string()),
                external_id: Some(track_id),
                ..Default::default()
            });
        }

        Some(album)
    }

    async fn get_artist(&self, external_provider: &str, external_id: &str) -> Option<Artist> {
        if !is_ours(external_provider) {
            return None;
        }
        let inner = &self.inner;
        let routing = inner.lookup(external_id)?.snapshot();

        // The name and picture of the catalog artist this page lists, as the album listing
        // settles it: the id already held, else the one of this exact name more people
        // follow. The first search hit can be a bigger act whose name contains this one.
        let hit = inner.find_catalog_artist(&routing).await;
        let meta = match hit {
            None => {
                inner
                    .deezer
                    .enrich_artist(routing.artist.as_deref().unwrap_or(""))
                    .await
            }
            Some(_) => None,
        };
        Some(Artist {
            id: external_id.to_string(),
            name: hit
                .as_ref()
                .map(|h| h.name.clone())
                .or_else(|| meta.as_ref().and_then(|m| m.name.clone()))
                .or_else(|| routing.artist.clone())
                .unwrap_or_default(),
            image_url: hit
                .as_ref()
                .and_then(|h| h.picture_url.clone())
                .or_else(|| meta.as_ref().and_then(|m| m.image_url.clone())),
            is_local: false,
            external_provider: Some(Self::PROVIDER_NAME.to_string()),
            external_id: Some(external_id.to_string()),
            ..Default::default()
        })
    }

    async fn get_related_artists(
        &self,
        external_provider: &str,
        external_id: &str,
        limit: i32,
    ) -> Vec<Artist> {
        if !is_ours(external_provider) || limit <= 0 {
            return Vec::new();
        }
        let inner = &self.inner;
        let Some(routing) = inner.lookup(external_id).map(|r| r.snapshot()) else {
            return Vec::new();
        };
        if routing.kind != RoutingKind::Artist {
            return Vec::new();
        }
        let Some(hit) = inner.find_catalog_artist(&routing).await else {
            return Vec::new();
        };
        let mut related = Vec::new();
        for other in inner.deezer.related_artists(&hit.deezer_id, limit).await {
            // The same id the artist's search row has, so the row opens that artist's page.
            let id = inner.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some(other.name.clone()),
                ..Default::default()
            });
            if let Some(shared) = inner.lookup(&id) {
                let unsettled = {
                    let mut r = shared.lock();
                    let unsettled = r.external_artist_id.is_none();
                    if unsettled {
                        r.external_artist_id = Some(other.deezer_id.clone());
                    }
                    unsettled
                };
                if unsettled {
                    inner.id_registry.register(shared);
                }
            }
            related.push(Artist {
                id: id.clone(),
                name: other.name,
                image_url: other.picture_url,
                album_count: Some(other.album_count),
                is_local: false,
                external_provider: Some(Self::PROVIDER_NAME.to_string()),
                external_id: Some(id),
            });
        }
        related
    }

    /// An outside artist's releases, for their page and for filling out a library artist's
    /// page. Each album is registered the way album search registers one, so it opens, plays
    /// and stars like any other outside album. The artist name and id are left for the caller:
    /// a library artist's page links its albums back to the library artist.
    async fn get_artist_albums(&self, external_provider: &str, external_id: &str) -> Vec<Album> {
        self.get_artist_albums_for_library(external_provider, external_id, None)
            .await
    }

    /// The same, for a library artist's page: the library's album titles say which of two
    /// artists of one name the page is about.
    async fn get_artist_albums_for_library(
        &self,
        external_provider: &str,
        external_id: &str,
        library_album_titles: Option<&[String]>,
    ) -> Vec<Album> {
        self.inner
            .artist_albums(external_provider, external_id, library_album_titles, true)
            .await
    }

    /// The same list with only the track counts already known, for the counts an artist's own
    /// record shows. The page asks for its album list at the same moment, and that request
    /// asks the catalog for the missing counts; asking again here doubled the traffic.
    async fn get_artist_albums_known_counts(&self, external_provider: &str, external_id: &str) -> Vec<Album> {
        self.inner
            .artist_albums(external_provider, external_id, None, false)
            .await
    }

    async fn search_playlists(&self, _query: &str, _limit: i32) -> Vec<ExternalPlaylist> {
        Vec::new()
    }

    async fn get_playlist(&self, _external_provider: &str, _external_id: &str) -> Option<ExternalPlaylist> {
        None
    }

    async fn get_playlist_tracks(&self, _external_provider: &str, _external_id: &str) -> Vec<Song> {
        Vec::new()
    }
}

#[cfg(test)]
#[path = "soulseek_metadata_service_tests.rs"]
mod tests;
