//! Last.fm's web service as the radio and the search bar read it. The records are
//! `octo_core::last_fm::last_fm_service`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::json::element::{
    ElementResult, enumerate_array, get_double, get_int64, get_property, get_string, raw_text,
    try_get_property,
};
use octo_core::last_fm::{SimilarArtist, SimilarTrack, TrackInfo};
use octo_core::metadata::accept_language_header;
use octo_core::settings::{LastFmSettings, SettingsStore};
use parking_lot::Mutex;
use regex::Regex;
use reqwest::header::{ACCEPT_LANGUAGE, HeaderMap, HeaderValue};
use serde_json::Value;
use std::sync::LazyLock;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use super::lookup_trouble;
use super::net_parse::{parse_double, parse_long};
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::client_builder;
use crate::services::http_client_factory::DEFAULT_TIMEOUT;

/// What the radio cache holds, one kind of answer per variant.
#[derive(Clone)]
enum RadioValue {
    Artists(Vec<SimilarArtist>),
    Tags(Vec<String>),
    Tracks(Vec<SimilarTrack>),
    Info(Option<TrackInfo>),
    Count(i64),
    Text(Option<String>),
}

/// A value [`LastFmService::cached`] can keep: `cached.Value is T typed`.
trait Cacheable: Sized + Clone {
    fn wrap(self) -> RadioValue;
    fn unwrap(value: &RadioValue) -> Option<Self>;
}

impl Cacheable for Vec<SimilarArtist> {
    fn wrap(self) -> RadioValue {
        RadioValue::Artists(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        match value {
            RadioValue::Artists(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Cacheable for Option<String> {
    fn wrap(self) -> RadioValue {
        RadioValue::Text(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        match value {
            RadioValue::Text(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Cacheable for Vec<String> {
    fn wrap(self) -> RadioValue {
        RadioValue::Tags(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        match value {
            RadioValue::Tags(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Cacheable for Vec<SimilarTrack> {
    fn wrap(self) -> RadioValue {
        RadioValue::Tracks(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        match value {
            RadioValue::Tracks(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Cacheable for i64 {
    fn wrap(self) -> RadioValue {
        RadioValue::Count(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        match value {
            RadioValue::Count(v) => Some(*v),
            _ => None,
        }
    }
}

impl Cacheable for Option<TrackInfo> {
    fn wrap(self) -> RadioValue {
        RadioValue::Info(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        // A null TrackInfo was stored, but `null is TrackInfo?` is false: it never hits.
        match value {
            RadioValue::Info(Some(info)) => Some(Some(info.clone())),
            _ => None,
        }
    }
}

/// How a list of songs like a seed was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimilarSource {
    /// Nothing came back.
    None,
    /// Last.fm's songs like this very song.
    Song,
    /// Songs like the song a nightcore, sped up or remixed seed came from, found only once that
    /// version was taken off the title.
    Original,
    /// Top songs of artists like the seed's artist, with nothing for the song itself.
    SimilarArtists,
}

impl std::fmt::Display for SimilarSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

/// `artist_trusted`: the seed's artist is who Last.fm thinks: it has the song under them, or they
/// have a real audience there. False for a similar-artists list means the artist may only be
/// whoever uploaded the song.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarAnswer {
    pub tracks: Vec<SimilarTrack>,
    pub source: SimilarSource,
    pub artist_trusted: bool,
}

/// Listeners Last.fm counts for an artist before artists like them are worth a radio without the
/// song itself: a new song by a known artist, not yet on Last.fm.
pub const KNOWN_ARTIST_LISTENERS: i64 = 10_000;

/// "Artist - Title" at the front of an upload's title, split at the first spaced dash.
static UPLOAD_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?<artist>[^\-–—]+?)\s+[\-–—]\s+(?<title>.*\S)\s*$").expect("the pattern is valid")
});

/// Versions that change how a song sounds enough to be a style of their own: a nightcore or sped
/// up upload is not the song it came from, and songs like the original are not like it.
const STYLE_MARKERS: [&str; 11] = [
    "nightcore",
    "sped up",
    "slowed",
    "reverb",
    "8d",
    "lofi",
    "bass boosted",
    "remix",
    "vip",
    "bootleg",
    "rework",
];

pub fn style_versions(title: Option<&str>) -> Vec<String> {
    SongIdentity::parse_title(title.unwrap_or(""), None)
        .versions
        .into_iter()
        .filter(|version| STYLE_MARKERS.contains(&version.as_str()))
        .collect()
}

/// One cached track.getsimilar answer. Each entry keeps how many tracks were asked for, so a
/// short answer cached for a station refresh is not handed to a later request that wants more.
#[derive(Clone)]
struct CachedSimilar {
    expiry: DateTime<Utc>,
    asked: i32,
    tracks: Vec<SimilarTrack>,
    source: SimilarSource,
}

pub struct LastFmService {
    http: reqwest::Client,
    base_url: String,
    /// The Last.fm settings: read at every use.
    settings: Arc<SettingsStore>,

    // Behind locks because radio requests arrive on request tasks and this is written on
    // every miss.
    cache: Mutex<HashMap<String, CachedSimilar>>,
    radio_cache: Mutex<HashMap<String, (DateTime<Utc>, RadioValue)>>,
    provider_gate: Semaphore,
}

impl LastFmService {
    pub const BASE_URL: &'static str = "https://ws.audioscrobbler.com/2.0/";

    /// The default limits of each call.
    pub const SIMILAR_TRACKS_LIMIT: i32 = 50;
    pub const SEARCH_LIMIT: i32 = 30;
    pub const ARTIST_TOP_TRACKS_LIMIT: i32 = 30;
    pub const SIMILAR_ARTISTS_LIMIT: i32 = 20;
    pub const TOP_TAGS_LIMIT: i32 = 10;
    pub const TAG_TOP_TRACKS_LIMIT: i32 = 50;

    /// The production client: the default HTTP client, 100 s.
    pub fn new(settings: Arc<SettingsStore>) -> Self {
        Self::with_base_url(settings, Self::BASE_URL)
    }

    /// A client that asks another host: a test's mock server.
    pub fn with_base_url(settings: Arc<SettingsStore>, base_url: &str) -> Self {
        // The metadata settings' Accept-Language is applied once, at construction, deliberately.
        let mut headers = HeaderMap::new();
        if let Some(language) = accept_language_header::header_value(&settings.current().metadata)
            && let Ok(value) = HeaderValue::from_str(&language)
        {
            headers.insert(ACCEPT_LANGUAGE, value);
        }
        let http = client_builder()
            .timeout(DEFAULT_TIMEOUT)
            .default_headers(headers)
            .build()
            .expect("the Last.fm client builds");
        Self {
            http,
            base_url: base_url.to_string(),
            settings,
            cache: Mutex::new(HashMap::new()),
            radio_cache: Mutex::new(HashMap::new()),
            provider_gate: Semaphore::new(4),
        }
    }

    fn last_fm(&self) -> LastFmSettings {
        self.settings.current().last_fm.clone()
    }

    fn cache_expiry(&self) -> DateTime<Utc> {
        Utc::now() + TimeDelta::hours(i64::from(self.last_fm().effective_radio_cache_duration_hours()))
    }

    /// One request. A request that got no answer marks the radio build's lookup that made it
    /// (see [`lookup_trouble`]), when a key is set.
    async fn get(&self, url: &str) -> anyhow::Result<HttpAnswer> {
        let answer: anyhow::Result<HttpAnswer> =
            async { Ok(HttpAnswer::read(self.http.get(url).send().await?).await?) }.await;
        let unanswered = match &answer {
            Ok(answer) => did_not_answer(answer),
            Err(_) => true,
        };
        if unanswered && self.has_api_key() {
            lookup_trouble::note_core();
        }
        answer
    }

    /// `EnsureSuccessStatusCode`, then the body as JSON. A try-again error is an error, so
    /// nothing keeps it.
    async fn get_json(&self, url: &str) -> anyhow::Result<Value> {
        let answer = self.get(url).await?;
        ensure_success(&answer)?;
        let doc = answer.json()?;
        if says_try_again(&doc) {
            anyhow::bail!("Last.fm asked to try again later");
        }
        Ok(doc)
    }

    pub async fn get_similar_tracks(&self, artist: &str, title: &str, limit: i32) -> Vec<SimilarTrack> {
        self.find_similar_tracks(artist, title, limit).await.tracks
    }

    /// Songs like one song, and how Last.fm came by them, so a radio can tell a list for the song
    /// itself from a guess. A seed Last.fm has never heard of, a YouTube upload filed under its
    /// uploader for one, gets similar artists to that uploader's name, which can be anyone (#78).
    pub async fn find_similar_tracks(&self, artist: &str, title: &str, limit: i32) -> SimilarAnswer {
        // Last.fm answers nothing without a key, so nothing is asked, or kept.
        if !self.has_api_key() {
            return SimilarAnswer {
                tracks: Vec::new(),
                source: SimilarSource::None,
                artist_trusted: false,
            };
        }
        match self.find_similar_tracks_inner(artist, title, limit).await {
            Ok(answer) => answer,
            Err(e) => {
                error!(error = %e, "Error fetching similar tracks from Last.fm for {artist} - {title}");
                SimilarAnswer {
                    tracks: Vec::new(),
                    source: SimilarSource::None,
                    artist_trusted: false,
                }
            }
        }
    }

    async fn find_similar_tracks_inner(
        &self,
        artist: &str,
        title: &str,
        limit: i32,
    ) -> anyhow::Result<SimilarAnswer> {
        let cache_key = dotnet::to_lower_invariant(&format!("{artist}|{title}"));

        // Check cache. A list shorter than what was asked for is everything Last.fm has, so it
        // answers any size.
        let cached = self.cache.lock().get(&cache_key).cloned().filter(|cached| {
            cached.expiry > Utc::now()
                && (cached.asked >= limit || (cached.tracks.len() as i64) < i64::from(cached.asked))
        });
        let (tracks, source) = match cached {
            Some(cached) => {
                debug!(
                    "Returning {} cached similar tracks for {artist} - {title}",
                    cached.tracks.len()
                );
                (cached.tracks, cached.source)
            }
            None => {
                info!("Fetching similar tracks from Last.fm for {artist} - {title}");
                let (tracks, source) = self.fetch_similar_for_spellings(artist, title, limit).await?;
                info!("Found {} similar tracks from Last.fm", tracks.len());
                self.cache.lock().insert(
                    cache_key,
                    CachedSimilar {
                        expiry: self.cache_expiry(),
                        asked: limit,
                        tracks: tracks.clone(),
                        source,
                    },
                );
                (tracks, source)
            }
        };

        if !tracks.is_empty() {
            return Ok(SimilarAnswer {
                tracks: take(tracks, limit),
                source,
                artist_trusted: true,
            });
        }

        // If no similar tracks found, try getting top tracks from similar artists. For a
        // renamed artist that guess runs under the name Last.fm files them by: asked for
        // artists like "Ye", it answered for someone else, and a mix for "Ye - Crack Music"
        // came back as thirty Jon Anderson songs.
        let guess_from = SongIdentity::known_name(artist).unwrap_or_else(|| artist.to_string());
        info!("No similar tracks found, trying similar artists for {guess_from}");
        let guessed = self.top_tracks_from_similar_artists(&guess_from, limit).await;
        let known = !guessed.is_empty()
            && (self.knows_song(&guess_from, title).await
                || self.artist_listeners(&guess_from).await? >= KNOWN_ARTIST_LISTENERS);
        let source = if guessed.is_empty() {
            SimilarSource::None
        } else {
            SimilarSource::SimilarArtists
        };
        Ok(SimilarAnswer {
            tracks: take(guessed, limit),
            source,
            artist_trusted: known,
        })
    }

    /// track.getsimilar under each way the song might be filed, stopping at the first that
    /// answers.
    async fn fetch_similar_for_spellings(
        &self,
        artist: &str,
        title: &str,
        limit: i32,
    ) -> anyhow::Result<(Vec<SimilarTrack>, SimilarSource)> {
        // Last.fm knows a song under one spelling. "$uicideboy$ - $UICIDE" or a title
        // carrying "(feat. X)" or "- Remastered 2011" can come back empty where the same song
        // written plainly does not, so those are asked before falling back to similar artists.
        // A renamed artist is filed under one name only: Last.fm has "Kanye West - Crack
        // Music" and nothing under "Ye", so the name the artist is best known by comes last.
        let known_name = SongIdentity::known_name(artist);
        let mut queries: Vec<_> = SongIdentity::query_variants(title, artist)
            .into_iter()
            .filter(|variant| !variant.artist.is_empty())
            .take(3)
            .collect();
        if let Some(known) = &known_name {
            queries.extend(
                SongIdentity::query_variants(title, known)
                    .into_iter()
                    .filter(|variant| !variant.artist.is_empty())
                    .take(2),
            );
        }
        // An upload's artist is often the channel, with the singer in the title:
        // "Nightcore Galaxy" and "Alan Walker - Faded (Nightcore)". The title's artist goes last.
        if let Some(named) = UPLOAD_TITLE.captures(title)
            && SongIdentity::key(&named["artist"]) != SongIdentity::key(artist)
        {
            let mut added = 0;
            for variant in SongIdentity::query_variants(&named["title"], &named["artist"]) {
                if added == 2 {
                    break;
                }
                if !variant.artist.is_empty() && !queries.contains(&variant) {
                    queries.push(variant);
                    added += 1;
                }
            }
        }

        let styles = style_versions(Some(title));
        for variant in &queries {
            let tracks = self
                .fetch_similar_tracks(&variant.artist, &variant.title, limit)
                .await?;
            if tracks.is_empty() {
                continue;
            }
            let kept = style_versions(Some(&variant.title));
            let source = if styles.iter().all(|style| kept.contains(style)) {
                SimilarSource::Song
            } else {
                SimilarSource::Original
            };
            return Ok((tracks, source));
        }
        Ok((Vec::new(), SimilarSource::None))
    }

    /// Whether Last.fm has the song under this artist, so the artist is likely the one who made it
    /// and not the channel that uploaded it. A failed check counts as no.
    async fn knows_song(&self, artist: &str, title: &str) -> bool {
        for variant in SongIdentity::query_variants(title, artist)
            .into_iter()
            .filter(|variant| !variant.artist.is_empty())
            .take(2)
        {
            match self.get_track_info(&variant.artist, &variant.title).await {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(e) => debug!(
                    "Last.fm track.getInfo failed for {} - {}: {e}",
                    variant.artist, variant.title
                ),
            }
        }
        false
    }

    /// How many listeners Last.fm counts for an artist; 0 when it does not know them. An `Err`
    /// for an answer whose `artist` is not an object.
    async fn artist_listeners(&self, artist: &str) -> ElementResult<i64> {
        self.cached(&format!("artist-listeners|{artist}"), || async {
            let Some(doc) = self.get_document("artist.getInfo", &[("artist", artist)]).await else {
                return Ok(None);
            };
            let Some(found) = try_get_property(&doc, "artist")? else {
                return Ok(Some(0));
            };
            let Some(stats) = try_get_property(found, "stats")? else {
                return Ok(Some(0));
            };
            let listeners = try_get_property(stats, "listeners")?
                .map(element_text)
                .unwrap_or_default();
            Ok(Some(parse_long(&listeners).unwrap_or(0)))
        })
        .await
    }

    /// Last.fm's short biography of an artist, or None. Only when the artist Last.fm answers
    /// for is the one asked about: a near name is someone else's story.
    pub async fn artist_biography(&self, artist: &str) -> Option<String> {
        if dotnet::is_blank(artist) {
            return None;
        }
        let read = self
            .cached(&format!("artist-bio|{artist}"), || async {
                let Some(doc) = self.get_document("artist.getInfo", &[("artist", artist)]).await else {
                    return Ok(None);
                };
                let Some(found) = try_get_property(&doc, "artist")? else {
                    return Ok(Some(None));
                };
                let answered = try_get_property(found, "name")?
                    .map(element_text)
                    .unwrap_or_default();
                if SongIdentity::key(&answered) != SongIdentity::key(artist) {
                    return Ok(Some(None));
                }
                let Some(bio) = try_get_property(found, "bio")? else {
                    return Ok(Some(None));
                };
                let summary = try_get_property(bio, "summary")?
                    .map(element_text)
                    .unwrap_or_default();
                Ok(Some(
                    (!summary.trim().is_empty()).then(|| summary.trim().to_string()),
                ))
            })
            .await;
        read.unwrap_or_else(|e| {
            debug!("Last.fm artist.getInfo for {artist} could not be read: {e}");
            None
        })
    }

    /// Top songs of the artists Last.fm counts as like one artist: a radio for an artist. An
    /// answer is kept as long as the radio cache keeps answers; a failed one is not kept.
    pub async fn get_similar_artist_tracks(&self, artist: &str, limit: i32) -> Vec<SimilarTrack> {
        if !self.has_api_key() {
            return Vec::new();
        }
        let name = SongIdentity::known_name(artist).unwrap_or_else(|| artist.to_string());
        let key = dotnet::to_lower_invariant(&format!("similar-artist-tracks|{name}|{limit}"));
        let hit = self
            .radio_cache
            .lock()
            .get(&key)
            .filter(|(expiry, _)| *expiry > Utc::now())
            .and_then(|(_, value)| Vec::<SimilarTrack>::unwrap(value));
        if let Some(tracks) = hit {
            return tracks;
        }
        match self.top_tracks_from_similar_artists_inner(&name, limit).await {
            Ok((tracks, complete)) => {
                // A list missing an artist Last.fm did not answer for is used, never kept.
                if complete {
                    self.keep(key, tracks.clone().wrap());
                }
                tracks
            }
            Err(e) => {
                error!(error = %e, "Error fetching top tracks from similar artists for {name}");
                Vec::new()
            }
        }
    }

    /// One track.getsimilar request, read into tracks.
    async fn fetch_similar_tracks(
        &self,
        artist: &str,
        title: &str,
        limit: i32,
    ) -> anyhow::Result<Vec<SimilarTrack>> {
        let url = format!(
            "{}?method=track.getsimilar&artist={}&track={}&api_key={}&format=json&limit={limit}",
            self.base_url,
            dotnet::escape_data_string(artist),
            dotnet::escape_data_string(title),
            self.last_fm().api_key
        );
        let doc = self.get_json(&url).await?;
        Ok(read_similar_tracks(&doc)?)
    }

    async fn top_tracks_from_similar_artists(&self, artist: &str, limit: i32) -> Vec<SimilarTrack> {
        match self.top_tracks_from_similar_artists_inner(artist, limit).await {
            Ok((tracks, _)) => tracks,
            Err(e) => {
                error!(error = %e, "Error fetching top tracks from similar artists for {artist}");
                Vec::new()
            }
        }
    }

    /// Top songs of the artists like `artist`, and whether Last.fm answered for every one of
    /// them.
    async fn top_tracks_from_similar_artists_inner(
        &self,
        artist: &str,
        limit: i32,
    ) -> anyhow::Result<(Vec<SimilarTrack>, bool)> {
        // Get similar artists
        let artist_url = format!(
            "{}?method=artist.getsimilar&artist={}&api_key={}&format=json&limit=10",
            self.base_url,
            dotnet::escape_data_string(artist),
            self.last_fm().api_key
        );
        let artist_doc = self.get_json(&artist_url).await?;

        let mut similar_artists = Vec::new();
        if let Some(similar) = try_get_property(&artist_doc, "similarartists")?
            && let Some(array) = try_get_property(similar, "artist")?
        {
            for a in enumerate_array(array)?.iter().take(5) {
                if let Some(name) = get_string(get_property(a, "name")?)?.filter(|n| !n.is_empty()) {
                    similar_artists.push(name.to_string());
                }
            }
        }

        info!("Found {} similar artists for {artist}", similar_artists.len());

        // Get top tracks from each similar artist
        let mut tracks = Vec::new();
        let mut complete = true;
        for similar_artist in &similar_artists {
            let top_tracks_url = format!(
                "{}?method=artist.gettoptracks&artist={}&api_key={}&format=json&limit=10",
                self.base_url,
                dotnet::escape_data_string(similar_artist),
                self.last_fm().api_key
            );
            let top = self.get(&top_tracks_url).await?;
            if did_not_answer(&top) {
                complete = false;
                continue;
            }
            if !top.is_success() {
                continue;
            }
            let top_doc = top.json()?;
            if let Some(top_tracks) = try_get_property(&top_doc, "toptracks")?
                && let Some(array) = try_get_property(top_tracks, "track")?
            {
                for track in enumerate_array(array)?.iter().take(10) {
                    let track_name = get_string(get_property(track, "name")?)?.unwrap_or("");
                    if !track_name.is_empty() {
                        tracks.push(SimilarTrack::new(similar_artist.as_str(), track_name, 0.5));
                    }
                }
            }
        }

        info!("Found {} top tracks from similar artists", tracks.len());

        Ok((take(tracks, limit), complete))
    }

    /// Last.fm can answer at all. Search discovery needs only this: an API key.
    pub fn has_api_key(&self) -> bool {
        !self.settings.current().last_fm.api_key.is_empty()
    }

    /// Radio specifically is available. EnableRadio is a switch for the radio feature, so
    /// it belongs here and not on [`has_api_key`](Self::has_api_key): the two used to be one
    /// property, which meant turning radio off also silently emptied the search bar of
    /// discovery results, a setting doing something its name does not say.
    pub fn is_radio_enabled(&self) -> bool {
        self.has_api_key() && self.settings.current().last_fm.enable_radio
    }

    /// Free-form track search. Used by Search3 hijack so the search bar
    /// returns Last.fm-driven discovery results instead of just local hits.
    /// Last.fm's track.search is a fuzzy match: "drake" returns Drake tracks,
    /// "drake hotline" returns "Hotline Bling" first, etc.
    pub async fn search_tracks(&self, query: &str, limit: i32) -> Vec<SimilarTrack> {
        if dotnet::is_blank(query) || !self.has_api_key() {
            return Vec::new();
        }
        let url = format!(
            "{}?method=track.search&track={}&api_key={}&format=json&limit={limit}",
            self.base_url,
            dotnet::escape_data_string(query),
            self.last_fm().api_key
        );
        let result: anyhow::Result<Vec<SimilarTrack>> = async {
            let doc = self.get_json(&url).await?;
            Ok(read_search_tracks(&doc)?)
        }
        .await;
        match result {
            Ok(tracks) => {
                info!("Last.fm track.search '{query}' -> {} tracks", tracks.len());
                tracks
            }
            Err(e) => {
                warn!(error = %e, "Last.fm track.search failed for '{query}'");
                Vec::new()
            }
        }
    }

    /// Top tracks for a known artist. Used to pad a search when track.search
    /// returns thin results (e.g. one-word artist queries) and as the primary
    /// data source for "play this artist" radio behaviors.
    pub async fn get_artist_top_tracks(&self, artist: &str, limit: i32) -> Vec<SimilarTrack> {
        self.artist_top_tracks(artist, limit, true).await
    }

    /// [`Self::get_artist_top_tracks`] for a search someone is waiting on: it never queues
    /// behind the radio's Last.fm calls, which hold the gate for as long as a station takes to
    /// build. Same answers, same cache.
    pub async fn search_artist_top_tracks(&self, artist: &str, limit: i32) -> Vec<SimilarTrack> {
        self.artist_top_tracks(artist, limit, false).await
    }

    async fn artist_top_tracks(&self, artist: &str, limit: i32, gated: bool) -> Vec<SimilarTrack> {
        // Without a key Last.fm answers nothing, so nothing is asked, or kept.
        if dotnet::is_blank(artist) || !self.has_api_key() {
            return Vec::new();
        }
        let url = format!(
            "{}?method=artist.gettoptracks&artist={}&api_key={}&format=json&limit={limit}",
            self.base_url,
            dotnet::escape_data_string(artist),
            self.last_fm().api_key
        );
        // Kept like the other radio answers, and asked through the same gate unless a search
        // is waiting.
        let read = self
            .cached(&format!("artist-top-tracks|{artist}|{limit}"), || async {
                let result: anyhow::Result<Vec<SimilarTrack>> = async {
                    let doc = {
                        let _permit = if gated {
                            Some(
                                self.provider_gate
                                    .acquire()
                                    .await
                                    .expect("the gate is never closed"),
                            )
                        } else {
                            None
                        };
                        self.get_json(&url).await?
                    };
                    let mut tracks = Vec::new();
                    if let Some(top) = try_get_property(&doc, "toptracks")?
                        && let Some(array @ Value::Array(_)) = try_get_property(top, "track")?
                    {
                        for t in enumerate_array(array)? {
                            let name = match try_get_property(t, "name")? {
                                Some(n) => get_string(n)?,
                                None => None,
                            };
                            if let Some(name) = name.filter(|n| !n.is_empty()) {
                                tracks.push(SimilarTrack::new(artist, name, 1.0));
                            }
                        }
                    }
                    Ok(tracks)
                }
                .await;
                Ok(match result {
                    Ok(tracks) => Some(tracks),
                    Err(e) => {
                        warn!(error = %e, "Last.fm artist.gettoptracks failed for '{artist}'");
                        None
                    }
                })
            })
            .await;
        read.unwrap_or_default()
    }

    /// `Err` for an element of Last.fm's answer that is not the kind the reader needs (an artist
    /// that is not an object, say).
    pub async fn get_similar_artists(&self, artist: &str, limit: i32) -> ElementResult<Vec<SimilarArtist>> {
        self.cached(&format!("artist-similar|{artist}|{limit}"), || async {
            let doc = self
                .get_document(
                    "artist.getsimilar",
                    &[("artist", artist), ("limit", &limit.to_string())],
                )
                .await;
            let Some(doc) = doc else { return Ok(None) };
            let Some(values) = try_array(&doc, "similarartists", "artist")? else {
                return Ok(Some(Vec::new()));
            };
            take_valid(values, limit, |item| {
                let name = text(item, "name")?;
                let similar = SimilarArtist {
                    r#match: number(item, "match")?,
                    name,
                };
                Ok((!similar.name.is_empty()).then_some(similar))
            })
            .map(Some)
        })
        .await
    }

    pub async fn get_artist_top_tags(&self, artist: &str, limit: i32) -> ElementResult<Vec<String>> {
        self.get_tags(
            "artist.gettoptags",
            &[("artist", artist)],
            &format!("artist-tags|{artist}|{limit}"),
            limit,
        )
        .await
    }

    /// [`Self::get_artist_top_tags`], telling a call that failed (None: no key, no answer, an
    /// error status) from an artist Last.fm has no tags for (an empty list).
    pub async fn get_artist_top_tags_checked(
        &self,
        artist: &str,
        limit: i32,
    ) -> ElementResult<Option<Vec<String>>> {
        let failed = std::sync::atomic::AtomicBool::new(false);
        let tags = self
            .cached(&format!("artist-tags|{artist}|{limit}"), || async {
                let Some(doc) = self
                    .get_document("artist.gettoptags", &[("artist", artist)])
                    .await
                else {
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Ok(None);
                };
                tag_names(&doc, limit).map(Some)
            })
            .await?;
        Ok((!failed.load(std::sync::atomic::Ordering::Relaxed)).then_some(tags))
    }

    pub async fn get_track_top_tags(
        &self,
        artist: &str,
        title: &str,
        limit: i32,
    ) -> ElementResult<Vec<String>> {
        self.get_tags(
            "track.gettoptags",
            &[("artist", artist), ("track", title)],
            &format!("track-tags|{artist}|{title}|{limit}"),
            limit,
        )
        .await
    }

    pub async fn get_tag_top_tracks(&self, tag: &str, limit: i32) -> ElementResult<Vec<SimilarTrack>> {
        self.cached(&format!("tag-tracks|{tag}|{limit}"), || async {
            let doc = self
                .get_document("tag.gettoptracks", &[("tag", tag), ("limit", &limit.to_string())])
                .await;
            let Some(doc) = doc else { return Ok(None) };
            let Some(values) = try_array(&doc, "tracks", "track")? else {
                return Ok(Some(Vec::new()));
            };
            take_valid(values, limit, |item| {
                let track = SimilarTrack::new(text_in(item, "artist", "name")?, text(item, "name")?, 1.0)
                    .with_duration(duration_seconds(item)?);
                Ok((!track.artist.is_empty() && !track.title.is_empty()).then_some(track))
            })
            .map(Some)
        })
        .await
    }

    pub async fn get_track_info(&self, artist: &str, title: &str) -> ElementResult<Option<TrackInfo>> {
        self.cached(&format!("track-info|{artist}|{title}"), || async {
            let doc = self
                .get_document("track.getInfo", &[("artist", artist), ("track", title)])
                .await;
            let Some(doc) = doc else { return Ok(None) };
            let Some(track) = try_get_property(&doc, "track")? else {
                return Ok(Some(None));
            };
            let mut tags = Vec::new();
            if let Some(values) = try_array(track, "toptags", "tag")? {
                for item in values {
                    let name = text(item, "name")?;
                    if !name.is_empty() {
                        tags.push(name);
                    }
                }
            }
            let album = text_in(track, "album", "title")?;
            Ok(Some(Some(TrackInfo {
                artist: text_in(track, "artist", "name")?,
                title: text(track, "name")?,
                album: (!album.is_empty()).then_some(album),
                duration: duration_seconds(track)?,
                tags,
            })))
        })
        .await
    }

    async fn get_tags(
        &self,
        method: &str,
        parameters: &[(&str, &str)],
        cache_key: &str,
        limit: i32,
    ) -> ElementResult<Vec<String>> {
        self.cached(cache_key, || async {
            let Some(doc) = self.get_document(method, parameters).await else {
                return Ok(None);
            };
            tag_names(&doc, limit).map(Some)
        })
        .await
    }

    /// The answer kept under `key`, else `load`'s. `load` gives None for a call that failed
    /// (no key, no answer, an error status): that reads as empty and is not kept, so the next
    /// call asks again.
    async fn cached<T, F, Fut>(&self, key: &str, load: F) -> ElementResult<T>
    where
        T: Cacheable + Default,
        F: FnOnce() -> Fut,
        Fut: Future<Output = ElementResult<Option<T>>>,
    {
        let key = dotnet::to_lower_invariant(key);
        let hit = self
            .radio_cache
            .lock()
            .get(&key)
            .filter(|(expiry, _)| *expiry > Utc::now())
            .and_then(|(_, value)| T::unwrap(value));
        if let Some(typed) = hit {
            return Ok(typed);
        }
        let Some(value) = load().await? else {
            return Ok(T::default());
        };
        self.keep(key, value.clone().wrap());
        Ok(value)
    }

    /// Keeps an answer until the radio cache's lifetime is up, sweeping out the expired ones.
    fn keep(&self, key: String, value: RadioValue) {
        let now = Utc::now();
        let mut cache = self.radio_cache.lock();
        cache.retain(|_, (expiry, _)| *expiry > now);
        cache.insert(key, (self.cache_expiry(), value));
    }

    async fn get_document(&self, method: &str, parameters: &[(&str, &str)]) -> Option<Value> {
        if !self.has_api_key() {
            return None;
        }
        let _permit = self
            .provider_gate
            .acquire()
            .await
            .expect("the gate is never closed");
        let api_key = self.last_fm().api_key;
        // The caller's parameters, then method, api_key and format, each replacing a parameter
        // of the same name in place.
        let mut query: Vec<(&str, &str)> = Vec::new();
        for (name, value) in parameters.iter().copied().chain([
            ("method", method),
            ("api_key", api_key.as_str()),
            ("format", "json"),
        ]) {
            match query.iter_mut().find(|(existing, _)| *existing == name) {
                Some(pair) => pair.1 = value,
                None => query.push((name, value)),
            }
        }
        let url = format!(
            "{}?{}",
            self.base_url,
            query
                .iter()
                .map(|(name, value)| format!(
                    "{}={}",
                    dotnet::escape_data_string(name),
                    dotnet::escape_data_string(value)
                ))
                .collect::<Vec<_>>()
                .join("&")
        );
        let answer = match self.get(&url).await {
            Ok(answer) => answer,
            Err(e) => {
                warn!(error = %e, "Last.fm {method} failed");
                return None;
            }
        };
        if answer.status.as_u16() == 429 {
            warn!("Last.fm rate limited {method}");
            return None;
        }
        if !answer.is_success() {
            return None;
        }
        match answer.json() {
            // Never kept: the next ask tries again.
            Ok(doc) if says_try_again(&doc) => {
                warn!("Last.fm asked to try {method} again later");
                None
            }
            Ok(doc) => Some(doc),
            Err(e) => {
                warn!(error = %e, "Last.fm {method} failed");
                None
            }
        }
    }
}

/// Last.fm's error codes that mean "try again later": the operation failed, the service is
/// offline, it is temporarily unavailable, or the rate limit was exceeded.
const TRY_AGAIN_ERRORS: [i64; 4] = [8, 11, 16, 29];

/// The `error` code in a Last.fm JSON body, when there is one, sent as a number or as text.
fn error_code(doc: &Value) -> Option<i64> {
    let error = doc.get("error")?;
    error.as_i64().or_else(|| error.as_str()?.trim().parse().ok())
}

/// Whether a Last.fm JSON body is one of its try-again errors.
fn says_try_again(doc: &Value) -> bool {
    error_code(doc).is_some_and(|code| TRY_AGAIN_ERRORS.contains(&code))
}

/// Whether Last.fm gave no answer to the question: a 408, 429 or 5xx, one of its try-again error
/// codes whatever the status, a success whose body is not JSON, or any other 4xx without one of
/// Last.fm's error codes in it (a proxy's or firewall's page). A 4xx carrying another Last.fm
/// code is an answer: an artist it does not know, a key it refuses.
fn did_not_answer(answer: &HttpAnswer) -> bool {
    let status = answer.status.as_u16();
    if status == 408 || status == 429 || answer.status.is_server_error() {
        return true;
    }
    match answer.json() {
        Ok(doc) => match error_code(&doc) {
            Some(code) => TRY_AGAIN_ERRORS.contains(&code),
            None => !answer.is_success(),
        },
        Err(_) => true,
    }
}

/// The tag names of a `toptags` answer, the first `limit` of them.
fn tag_names(doc: &Value, limit: i32) -> ElementResult<Vec<String>> {
    let Some(values) = try_array(doc, "toptags", "tag")? else {
        return Ok(Vec::new());
    };
    take_valid(values, limit, |item| {
        let name = text(item, "name")?;
        Ok((!name.is_empty()).then_some(name))
    })
}

/// `EnsureSuccessStatusCode`, in plain words.
fn ensure_success(answer: &HttpAnswer) -> anyhow::Result<()> {
    if answer.is_success() {
        return Ok(());
    }
    anyhow::bail!(crate::services::http_client_factory::status_failure_message(
        answer.status
    ))
}

/// `list.Take(limit)`.
fn take<T>(mut items: Vec<T>, limit: i32) -> Vec<T> {
    items.truncate(usize::try_from(limit).unwrap_or(0));
    items
}

/// Reads each value, keeps the ones read as `Some`, and stops at `limit`. Lazy: an element after
/// the last one taken is never read, so it cannot fail the call.
fn take_valid<T>(
    values: &[Value],
    limit: i32,
    mut read: impl FnMut(&Value) -> ElementResult<Option<T>>,
) -> ElementResult<Vec<T>> {
    let limit = usize::try_from(limit).unwrap_or(0);
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    for value in values {
        if let Some(item) = read(value)? {
            out.push(item);
            if out.len() == limit {
                break;
            }
        }
    }
    Ok(out)
}

/// track.getsimilar's answer.
fn read_similar_tracks(doc: &Value) -> ElementResult<Vec<SimilarTrack>> {
    let mut tracks = Vec::new();
    let Some(similar_tracks) = try_get_property(doc, "similartracks")? else {
        return Ok(tracks);
    };
    let Some(track_array) = try_get_property(similar_tracks, "track")? else {
        return Ok(tracks);
    };
    for track in enumerate_array(track_array)? {
        let track_name = get_string(get_property(track, "name")?)?.unwrap_or("");
        let mut artist_name = "";
        if let Some(artist_obj) = try_get_property(track, "artist")? {
            artist_name = match try_get_property(artist_obj, "name")? {
                Some(name) => get_string(name)?.unwrap_or(""),
                None => "",
            };
        }

        let mut r#match = 0.0;
        if let Some(match_prop) = try_get_property(track, "match")? {
            // Last.fm returns match as a number, not a string
            match match_prop {
                Value::Number(_) => r#match = get_double(match_prop)?,
                Value::String(text) => r#match = parse_double(text, true).unwrap_or(0.0),
                _ => {}
            }
        }

        // Last.fm returns duration in milliseconds (sometimes a string,
        // sometimes a number, sometimes "0" when unknown; treat 0 as null
        // so we fall back to the placeholder default downstream).
        let mut duration_sec = None;
        if let Some(dur_el) = try_get_property(track, "duration")? {
            let dur_ms = match dur_el {
                Value::Number(_) => get_int64(dur_el)?,
                Value::String(text) => parse_long(text).unwrap_or(0),
                _ => 0,
            };
            if dur_ms > 1000 {
                // (int)(durMs / 1000): an unchecked narrowing.
                duration_sec = Some((dur_ms / 1000) as i32);
            }
        }

        if !track_name.is_empty() && !artist_name.is_empty() {
            tracks.push(SimilarTrack::new(artist_name, track_name, r#match).with_duration(duration_sec));
        }
    }
    Ok(tracks)
}

/// track.search's answer.
fn read_search_tracks(doc: &Value) -> ElementResult<Vec<SimilarTrack>> {
    let mut tracks = Vec::new();
    if let Some(results) = try_get_property(doc, "results")?
        && let Some(matches) = try_get_property(results, "trackmatches")?
        && let Some(track_array @ Value::Array(_)) = try_get_property(matches, "track")?
    {
        for t in enumerate_array(track_array)? {
            let name = match try_get_property(t, "name")? {
                Some(n) => get_string(n)?,
                None => None,
            };
            let artist = match try_get_property(t, "artist")? {
                Some(a) => get_string(a)?,
                None => None,
            };
            let listeners = match try_get_property(t, "listeners")? {
                Some(Value::String(text)) => parse_long(text),
                Some(other) => parse_long(&raw_text(other)),
                None => None,
            };
            if let (Some(name), Some(artist)) = (name, artist)
                && !name.is_empty()
                && !artist.is_empty()
            {
                tracks.push(SimilarTrack::new(artist, name, 1.0).with_listeners(listeners));
            }
        }
    }
    Ok(tracks)
}

fn try_array<'a>(root: &'a Value, container: &str, array: &str) -> ElementResult<Option<&'a [Value]>> {
    let Some(parent) = try_get_property(root, container)? else {
        return Ok(None);
    };
    match try_get_property(parent, array)? {
        Some(Value::Array(values)) => Ok(Some(values)),
        _ => Ok(None),
    }
}

/// A JSON value as text: a string's text, a number's raw text, `True`/`False`, nothing for
/// null, and the JSON of an object or array.
fn element_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(text) => text.clone(),
        other => raw_text(other),
    }
}

fn text(element: &Value, property: &str) -> ElementResult<String> {
    Ok(try_get_property(element, property)?
        .map(element_text)
        .unwrap_or_default())
}

fn text_in(element: &Value, parent: &str, property: &str) -> ElementResult<String> {
    match try_get_property(element, parent)? {
        Some(value @ Value::Object(_)) => text(value, property),
        _ => Ok(String::new()),
    }
}

fn number(element: &Value, property: &str) -> ElementResult<f64> {
    Ok(try_get_property(element, property)?
        .and_then(|value| parse_double(&element_text(value), false))
        .unwrap_or(0.0))
}

fn duration_seconds(element: &Value) -> ElementResult<Option<i32>> {
    let Some(value) = try_get_property(element, "duration")? else {
        return Ok(None);
    };
    match parse_long(&element_text(value)) {
        Some(milliseconds) if milliseconds > 0 => Ok(Some(if milliseconds > 1000 {
            (milliseconds / 1000) as i32
        } else {
            milliseconds as i32
        })),
        _ => Ok(None),
    }
}

// The play-count charts.
#[path = "last_fm_charts.rs"]
mod charts;

#[cfg(test)]
#[path = "last_fm_service_tests.rs"]
mod tests;

/// The song-length chain's Last.fm step.
///
/// An answer the radio readers refuse (`Err`) counts as no length, so the length lookup still
/// goes on to its last step, the yt-dlp probe.
#[async_trait::async_trait]
impl crate::services::soulseek::soulseek_metadata_service::LastFmTrackLengths for LastFmService {
    fn has_api_key(&self) -> bool {
        LastFmService::has_api_key(self)
    }

    async fn track_duration(&self, artist: &str, title: &str) -> Option<i32> {
        self.get_track_info(artist, title)
            .await
            .ok()
            .flatten()
            .and_then(|info| info.duration)
    }
}
