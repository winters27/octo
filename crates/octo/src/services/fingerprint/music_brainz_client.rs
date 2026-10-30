//! The MusicBrainz client, its HTTP half: the one-a-second gate and its three lanes, the cache,
//! the "musicbrainz" client (base address https://musicbrainz.org/ws/2/, a 10 s timeout, Octo's
//! User-Agent), and the re-asker of the questions a heart holds for. The query builders and the
//! readers are in `octo_core::fingerprint::music_brainz_client`.
//!
//! [`HeartQuestions`] asks a heart's questions of MusicBrainz and Deezer and keeps their answers
//! as the compact rows `octo_core::fingerprint::heart_release` picks a release from.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use octo_core::common::{SongIdentity, dotnet, octo_user_agent};
use octo_core::fingerprint::acoust_id_client::{AnswerArtist, AnswerRecording};
use octo_core::fingerprint::heart_gate::MusicBrainzFact;
use octo_core::fingerprint::heart_release::{
    self as release, BarcodeRelease, Carriers, CatalogAlbum, CatalogAlbumHit, CatalogTrack, CodeRecording,
    HeardIdentity, HeartSong,
};
use octo_core::fingerprint::music_brainz_client as pure;
use octo_core::fingerprint::recording_identity::RecordingFacts;
use octo_core::json::element::ElementError;
use octo_core::models::domain::explicit_status;
use octo_core::soulseek::SoulseekRouting;
use octo_core::tagging::{ReleaseDetails, ReleaseLookup};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tracing::{debug, warn};
use url::Url;

use crate::services::framework::http::{client_builder, parse_json, resolve};
use crate::services::framework::{HttpAnswer, MemoryCache};
use crate::services::metadata::deezer_metadata_service::{DeezerAsked, DeezerMetadataService};

/// The client's errors, which callers catch.
#[derive(Debug, thiserror::Error)]
pub enum MusicBrainzError {
    /// The client's timeout elapsed. It reaches the caller rather than reading as "nothing to
    /// say".
    #[error("No answer came within 10 seconds, so Octo stopped waiting.")]
    TimedOut,
    /// The answer could not be read: a field of the wrong kind.
    #[error(transparent)]
    Malformed(#[from] ElementError),
}

#[derive(Clone)]
enum Cached {
    Release(Arc<ReleaseDetails>),
    Json(Value),
    Text(String),
}

/// How MusicBrainz answered one question.
#[derive(Debug, Clone)]
enum Asked {
    /// A 200 with a readable body.
    Answer(Value),
    /// A 404, or a 400 saying the id is not a MusicBrainz id: MusicBrainz knows nothing by it.
    NotKnown,
    /// Any other 4xx but 429: MusicBrainz does not take the question as it was asked.
    Refused,
    /// A 429, a 5xx, a body that could not be read, or no answer at all: worth asking again, after
    /// `retry_after` when MusicBrainz said how long to wait.
    Busy { retry_after: Option<Duration> },
}

impl Asked {
    /// The answer, or None for every way of having none.
    fn found(self) -> Option<Value> {
        match self {
            Asked::Answer(doc) => Some(doc),
            Asked::NotKnown | Asked::Refused | Asked::Busy { .. } => None,
        }
    }

    fn is_busy(&self) -> bool {
        matches!(self, Asked::Busy { .. })
    }
}

/// What MusicBrainz said about one code.
#[derive(Debug, Clone, PartialEq)]
pub enum IsrcLookup {
    /// The recordings that carry the code, as `{"isrc": <code>, "recordings": [...]}`.
    Found(Value),
    /// MusicBrainz knows no recording with the code.
    NotKnown,
    /// MusicBrainz could not be asked: busy or unreachable. Worth asking again.
    Unanswered,
    /// MusicBrainz refused the question as Octo asked it: a definite answer, never worth
    /// asking again the same way.
    Refused,
}

/// The releases kept per recording of a code: the candidate reader takes no more.
const MOST_RELEASES_PER_RECORDING: usize = 25;

/// Which queue a MusicBrainz question waits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// A download being checked or a person waiting on the answer.
    Foreground,
    /// A heart's identity, its walk and its label lookups: after the gate is let go, in order,
    /// alternating with download checks, ahead of Background, with retries.
    Heart,
    /// Work nobody waits on.
    Background,
}

/// How a question held for MusicBrainz ended for the one who waited on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReAsked {
    /// MusicBrainz answered it.
    Answered,
    /// It failed three times while MusicBrainz answered other questions: one MusicBrainz cannot
    /// answer. Asked once more, it answers that nothing is known by it.
    GaveUp,
}

/// How many times a heart's question is asked again after a busy answer or a timeout.
const HEART_RETRIES: u32 = 3;

/// The longest `Retry-After` a heart's question waits out before it asks again: four of the
/// gate's turns. A longer one makes the question busy at once, and the re-asker waits it out.
const LONGEST_HEART_WAIT: Duration = Duration::from_millis(4400);

/// The longest `Retry-After` kept: MusicBrainz never asks for more, and a bigger number is junk.
const LONGEST_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// How many times a held question may fail while MusicBrainz answers other questions before it
/// is one MusicBrainz cannot answer.
const HELD_FAILURES: u32 = 3;

/// The question that tells whether MusicBrainz answers at all: the Various Artists artist, sent
/// past every cache.
const PROBE: &str = "artist/89ad4ac3-39f7-470e-963a-56509c546377?fmt=json";

/// The most answers of held questions kept for the next ask of each.
const SETTLED_MAX: usize = 64;

/// The most compact rows a heart's answers are kept as, on every lane together.
const HEART_ROWS_MAX: usize = 20_000;

/// The most songs waiting for a warm-up: one more puts out the song that has waited longest.
const WARM_UPS_MAX: usize = 8;

/// One question for MusicBrainz, asked only when a person keeps a track AcoustID had never heard
/// of: which recording is this? That is the MusicBrainz id a confirmed fingerprint needs before
/// AcoustID will take it (#47). And one more, asked only while verifying a download whose
/// fingerprint named a recording that reads differently from the request: which ISRCs does
/// that recording carry?
///
/// MusicBrainz allows about one request a second and wants a User-Agent naming the application,
/// and it holds near-duplicate recordings (two "Teardrop" by Massive Attack, 27 ms apart), so an
/// answer is only returned when exactly one recording fits. Anything else is a guess, and a
/// guess is never submitted with someone's name on it.
pub struct MusicBrainzClient {
    client: reqwest::Client,
    base: Url,
    /// One call at a time, and when the last one was made and on which lane. A download check
    /// holds it across its wait and its call; a heart's question takes its turn and lets go before
    /// it sends (see [`Self::heart_turn`]); the background lane only ever tries it (see
    /// [`Self::get_background`]).
    gate: tokio::sync::Mutex<Option<LastCall>>,
    /// The least time between two calls.
    gap: Duration,
    /// How long the background lane waits when the gate is busy.
    background_backoff: Duration,
    /// Heart questions ask one at a time, in the order they came, their retries included.
    heart_line: tokio::sync::Mutex<()>,
    /// Heart questions asking or waiting to: the background lane steps aside while there is one.
    hearts_waiting: AtomicUsize,
    /// When MusicBrainz's last `Retry-After` ends: heart questions and the re-asker send nothing
    /// before it.
    not_before: Mutex<Option<Instant>>,
    /// How many definite answers MusicBrainz has given, on every lane: the re-asker reads from it
    /// whether MusicBrainz answered other questions.
    answers: watch::Sender<u64>,
    /// The questions held until MusicBrainz answers them, oldest first.
    held: Mutex<HeldQuestions>,
    /// A question held again while the re-asker waits: it tries again at once.
    nudge: Notify,
    /// What held questions came to, each kept for the next ask of that question.
    settled: Mutex<VecDeque<(String, Asked)>>,
    /// Code lookups on their way, by cache key: a caller asking a code another caller is asking
    /// waits for that answer.
    code_flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<CodeFlight>>>>>,
    /// A heart's answers, as compact rows.
    heart_rows: Mutex<HeartRows>,
    /// The songs waiting to have a heart's first questions asked ahead, and the one being asked.
    warm_ups: Mutex<WarmUps>,
    /// Release lookups are remembered a day, since an album's tracks ask for the same
    /// release one by one; searches six hours. Every entry counts as one, so the limit is a count.
    cache: MemoryCache<Cached>,
    /// Exact-name artist answers by lowercase name, kept for the life of the process.
    artists: Mutex<HashMap<String, Vec<pure::MusicBrainzArtist>>>,
    /// Recording credits by lowercase recording id, kept for the life of the process.
    credits: Mutex<HashMap<String, Vec<pure::CreditPart>>>,
}

impl MusicBrainzClient {
    pub const CLIENT_NAME: &'static str = "musicbrainz";
    pub const BASE_ADDRESS: &'static str = "https://musicbrainz.org/ws/2/";
    const TIMEOUT: Duration = Duration::from_secs(10);
    const MINIMUM_GAP: Duration = Duration::from_millis(1100);
    const RELEASE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    const SEARCH_TTL: Duration = Duration::from_secs(6 * 60 * 60);
    const ARTIST_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    const BACKGROUND_BACKOFF: Duration = Duration::from_secs(5);

    pub fn new() -> Self {
        Self::with_base_url(Url::parse(Self::BASE_ADDRESS).expect("the MusicBrainz base address parses"))
    }

    /// A client for another base address: a test's mock server.
    pub fn with_base_url(base: Url) -> Self {
        Self {
            client: http_client(Self::TIMEOUT),
            base,
            gate: tokio::sync::Mutex::new(None),
            gap: Self::MINIMUM_GAP,
            background_backoff: Self::BACKGROUND_BACKOFF,
            heart_line: tokio::sync::Mutex::new(()),
            hearts_waiting: AtomicUsize::new(0),
            not_before: Mutex::new(None),
            answers: watch::Sender::new(0),
            held: Mutex::new(HeldQuestions::default()),
            nudge: Notify::new(),
            settled: Mutex::new(VecDeque::new()),
            code_flights: Mutex::new(HashMap::new()),
            heart_rows: Mutex::new(HeartRows::default()),
            warm_ups: Mutex::new(WarmUps::default()),
            cache: MemoryCache::new(2000),
            artists: Mutex::new(HashMap::new()),
            credits: Mutex::new(HashMap::new()),
        }
    }

    /// The same client with its gap between calls, its request timeout and its background
    /// step-back set, so a test of the lanes runs in milliseconds.
    #[cfg(test)]
    pub(crate) fn with_pace(
        mut self,
        gap: Duration,
        timeout: Duration,
        background_backoff: Duration,
    ) -> Self {
        self.client = http_client(timeout);
        self.gap = gap;
        self.background_backoff = background_backoff;
        self
    }

    /// One release in full: its label and catalog number, barcode, status, country, date, its
    /// group's kind and first release date, every track's position and id, and the genres people
    /// voted on. Remembered a day, since an album's tracks ask one by one.
    pub async fn lookup_release(
        &self,
        release_id: &str,
    ) -> Result<Option<Arc<ReleaseDetails>>, MusicBrainzError> {
        self.lookup_release_on(release_id, false).await
    }

    /// [`Self::lookup_release`] on the background lane, for the library sweep: it steps back
    /// whenever a download is asking.
    pub async fn lookup_release_in_background(
        &self,
        release_id: &str,
    ) -> Result<Option<Arc<ReleaseDetails>>, MusicBrainzError> {
        self.lookup_release_on(release_id, true).await
    }

    async fn lookup_release_on(
        &self,
        release_id: &str,
        background: bool,
    ) -> Result<Option<Arc<ReleaseDetails>>, MusicBrainzError> {
        if dotnet::is_blank(release_id) {
            return Ok(None);
        }
        let key = format!("release|{}", dotnet::to_lower_invariant(release_id.trim()));
        if let Some(Cached::Release(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }

        let url = format!(
            "release/{}?inc=labels+release-groups+artist-credits+recordings+isrcs+genres&fmt=json",
            dotnet::escape_data_string(release_id.trim())
        );
        let asked = if background {
            self.get_background(&url, "release lookup").await?
        } else {
            self.get(&url, "release lookup").await?
        };
        let Some(doc) = asked else {
            return Ok(None);
        };
        let details = ReleaseDetails::parse(&doc).map(Arc::new);
        if let Some(details) = &details {
            self.cache
                .set(key, Cached::Release(Arc::clone(details)), 1, Self::RELEASE_TTL);
        }
        Ok(details)
    }

    /// Recordings by name and length, for a download the fingerprint service could not name. Up
    /// to 25, each with its releases and their groups.
    pub async fn search_recordings(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> Result<Option<Value>, MusicBrainzError> {
        if dotnet::is_blank(artist) && dotnet::is_blank(title) {
            return Ok(None);
        }
        let key = format!(
            "search|{}|{duration_seconds}",
            SongIdentity::match_key(artist, title)
        );
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }

        let url = pure::build_recording_search_url(artist, title, duration_seconds);
        let Some(doc) = self.get(&url, "recording search").await? else {
            return Ok(None);
        };
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(Some(doc))
    }

    /// Every recording of one song by one artist, whatever its version or length: the version
    /// check reads which of them a file's length is. On the background lane; answers are kept
    /// as long as a search's.
    pub async fn song_recordings_in_background(
        &self,
        artist: &str,
        title: &str,
    ) -> Result<Option<Value>, MusicBrainzError> {
        if dotnet::is_blank(artist) || dotnet::is_blank(title) {
            return Ok(None);
        }
        let key = format!("song|{}", SongIdentity::match_key(artist, title));
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }
        let url = pure::build_recording_search_url(artist, title, 0);
        let Some(doc) = self.get_background(&url, "song recordings search").await? else {
            return Ok(None);
        };
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(Some(doc))
    }

    /// The recordings that carry one code, each with its releases, their groups and the code's
    /// track on them: a search by code, since the code lookup itself leaves the releases out and
    /// refuses their groups. A recording the search lists without the code, and a DJ-mix segment,
    /// are left out. Only a found answer is remembered.
    pub async fn lookup_isrc(&self, isrc: &str) -> Result<IsrcLookup, MusicBrainzError> {
        self.code_search(isrc).await
    }

    /// Every recording that carries one code, without its releases, asked on the background
    /// lane for work nobody waits on: the code lookup, which knows a code the moment it is
    /// entered and lists each recording once. A DJ-mix segment is left out. Only a found answer
    /// is remembered.
    pub async fn isrc_recordings_in_background(&self, isrc: &str) -> Result<IsrcLookup, MusicBrainzError> {
        self.code_recordings_on(isrc, Lane::Background).await
    }

    /// [`Self::isrc_recordings_in_background`] for a heart: the same question and the same
    /// remembered answer, asked on the heart lane with its retries.
    pub async fn isrc_recordings_for_heart(&self, isrc: &str) -> Result<IsrcLookup, MusicBrainzError> {
        self.code_recordings_on(isrc, Lane::Heart).await
    }

    /// The code lookup on one lane. A caller asking a code another caller is already asking
    /// waits for that answer and takes it, unless it was busy on a lane that does not ask again
    /// while this one does.
    async fn code_recordings_on(&self, isrc: &str, lane: Lane) -> Result<IsrcLookup, MusicBrainzError> {
        let Some(code) = SongIdentity::normalize_isrc(isrc) else {
            return Ok(IsrcLookup::NotKnown);
        };
        let key = format!("isrc-recordings|{code}");
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(IsrcLookup::Found(cached));
        }
        let flight = Arc::clone(self.code_flights.lock().entry(key.clone()).or_default());
        let mut shared = flight.lock().await;
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(IsrcLookup::Found(cached));
        }
        if let Some(earlier) = shared.as_ref()
            && earlier.settles(lane)
        {
            return earlier.answer.clone().ok_or(MusicBrainzError::TimedOut);
        }
        let answer = self.ask_code(&code, key.clone(), lane).await;
        *shared = Some(CodeFlight {
            lane,
            answer: answer.as_ref().ok().cloned(),
        });
        let mut flights = self.code_flights.lock();
        if flights
            .get(&key)
            .is_some_and(|running| Arc::ptr_eq(running, &flight))
        {
            flights.remove(&key);
        }
        answer
    }

    async fn ask_code(&self, code: &str, key: String, lane: Lane) -> Result<IsrcLookup, MusicBrainzError> {
        let lookup = code_question(code).unwrap_or_default();
        let recordings = match self.ask_on(&lookup, "code lookup", lane).await? {
            Asked::Answer(doc) => recordings_of(&doc, |_| true),
            Asked::NotKnown => Vec::new(),
            Asked::Refused => return Ok(IsrcLookup::Refused),
            Asked::Busy { .. } => return Ok(IsrcLookup::Unanswered),
        };
        if recordings.is_empty() {
            return Ok(IsrcLookup::NotKnown);
        }
        let doc = serde_json::json!({ "isrc": code, "recordings": recordings });
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(IsrcLookup::Found(doc))
    }

    /// The id MusicBrainz files one recording under now, asked on the heart lane: a merged-away
    /// id answers with the recording it became. Not known for an id that is not a MusicBrainz id,
    /// a 404 or a refused question; busy when MusicBrainz did not answer after the heart lane's
    /// retries. Answers are kept as a heart's rows are.
    pub async fn current_recording(&self, recording_id: &str) -> MusicBrainzFact<String> {
        let id = recording_id.trim().to_lowercase();
        if !pure::is_mbid(&id) {
            return MusicBrainzFact::NotKnown;
        }
        let key = format!("recording|{id}");
        let kept = self.heart_rows.lock().get(&key);
        if let Some(HeartRow::Recording(current)) = kept {
            return MusicBrainzFact::Known(current);
        }
        let question = recording_question(&id).unwrap_or_default();
        match self.ask_on(&question, "recording lookup", Lane::Heart).await {
            Ok(Asked::Answer(doc)) => {
                let current = doc
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|current| current.trim().to_lowercase())
                    .filter(|current| pure::is_mbid(current));
                match current {
                    Some(current) => {
                        self.heart_rows
                            .lock()
                            .put(key, HeartRow::Recording(current.clone()), 1);
                        MusicBrainzFact::Known(current)
                    }
                    None => MusicBrainzFact::NotKnown,
                }
            }
            Ok(Asked::NotKnown | Asked::Refused) => MusicBrainzFact::NotKnown,
            Ok(Asked::Busy { .. }) | Err(_) => MusicBrainzFact::Busy,
        }
    }

    /// The recordings MusicBrainz files under one code, as AcoustID's answers carry theirs: id,
    /// title, artists with their ids, length in seconds and the code, with no submissions. What a
    /// file's ISRC label names when AcoustID has no answer for the file; the same question and
    /// remembered answer as [`Self::isrc_recordings_for_heart`].
    pub async fn isrc_songs_for_heart(&self, isrc: &str) -> MusicBrainzFact<Vec<AnswerRecording>> {
        let Some(code) = SongIdentity::normalize_isrc(isrc) else {
            return MusicBrainzFact::NotKnown;
        };
        match self.isrc_recordings_for_heart(&code).await {
            Ok(IsrcLookup::Found(doc)) => MusicBrainzFact::Known(
                release::read_code_recordings(&doc)
                    .into_iter()
                    .map(|recording| answer_recording(recording, &code))
                    .collect(),
            ),
            Ok(IsrcLookup::NotKnown | IsrcLookup::Refused) => MusicBrainzFact::NotKnown,
            Ok(IsrcLookup::Unanswered) | Err(_) => MusicBrainzFact::Busy,
        }
    }

    async fn code_search(&self, isrc: &str) -> Result<IsrcLookup, MusicBrainzError> {
        let Some(code) = SongIdentity::normalize_isrc(isrc) else {
            return Ok(IsrcLookup::NotKnown);
        };
        let key = format!("isrc|{code}");
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(IsrcLookup::Found(cached));
        }
        let search = format!("recording?query=isrc:{code}&limit=100&fmt=json");
        let recordings = match self.ask(&search, "code search").await? {
            Asked::Answer(doc) => recordings_of(&doc, |recording| carries(recording, &code)),
            Asked::NotKnown => Vec::new(),
            Asked::Refused => return Ok(IsrcLookup::Refused),
            Asked::Busy { .. } => return Ok(IsrcLookup::Unanswered),
        };
        if recordings.is_empty() {
            return Ok(IsrcLookup::NotKnown);
        }
        let doc = serde_json::json!({ "isrc": code, "recordings": recordings });
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(IsrcLookup::Found(doc))
    }

    /// The one recording that is this song, this version, by this artist, at this length.
    pub async fn find_recording(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> Result<Option<String>, MusicBrainzError> {
        let Some(url) = pure::find_recording_url(artist, title, duration_seconds) else {
            return Ok(None);
        };
        match self.get(&url, "recording search").await? {
            Some(doc) => Ok(pure::pick(&doc, artist, title, duration_seconds)?),
            None => Ok(None),
        }
    }

    /// The ISRCs MusicBrainz lists for one recording: empty when it lists none, None when it
    /// could not be asked. One lookup by id with inc=isrcs, inside the same one-a-second budget
    /// as the search above.
    pub async fn fetch_isrcs(&self, recording_id: &str) -> Result<Option<Vec<String>>, MusicBrainzError> {
        if dotnet::is_blank(recording_id) {
            return Ok(None);
        }
        let url = format!(
            "recording/{}?inc=isrcs&fmt=json",
            dotnet::escape_data_string(recording_id)
        );
        match self.get(&url, "ISRC lookup").await? {
            Some(doc) => Ok(Some(pure::parse_isrcs(&doc)?)),
            None => Ok(None),
        }
    }

    /// What the music database says one recording is: its disambiguation and every release it
    /// is on. None when it could not be asked. `background` asks on the background lane, for
    /// the library sweep. Answers are remembered a day, since a recording's releases rarely
    /// change.
    pub async fn recording_facts(
        &self,
        recording_id: &str,
        background: bool,
    ) -> Result<Option<RecordingFacts>, MusicBrainzError> {
        let Some(url) = pure::recording_facts_url(recording_id) else {
            return Ok(None);
        };
        let key = format!("facts|{}", dotnet::to_lower_invariant(recording_id.trim()));
        let doc = match self.cache.get(&key) {
            Some(Cached::Json(cached)) => cached,
            _ => {
                let asked = if background {
                    self.get_background(&url, "recording lookup").await?
                } else {
                    self.get(&url, "recording lookup").await?
                };
                let Some(doc) = asked else {
                    return Ok(None);
                };
                self.cache
                    .set(key, Cached::Json(doc.clone()), 1, Self::ARTIST_TTL);
                doc
            }
        };
        Ok(Some(pure::parse_recording_facts(&doc)?))
    }

    /// The release group of the oldest official studio album a song appears on, or of a
    /// soundtrack when no studio album has it. None when MusicBrainz knows neither.
    pub async fn find_studio_album(
        &self,
        artist: &str,
        title: &str,
    ) -> Result<Option<String>, MusicBrainzError> {
        let Some((url, plain_title)) = pure::studio_album_search(artist, title) else {
            return Ok(None);
        };
        match self.get(&url, "studio album search").await? {
            Some(doc) => Ok(pure::pick_studio_album(&doc, &plain_title)?),
            None => Ok(None),
        }
    }

    /// Where the artist with this MusicBrainz id is from: a two-letter country, empty when
    /// MusicBrainz names none, None when it could not be asked. On the background lane;
    /// answers are remembered a day.
    pub async fn artist_country(&self, mbid: &str) -> Result<Option<String>, MusicBrainzError> {
        let Some(url) = pure::artist_lookup_url(mbid) else {
            return Ok(None);
        };
        let key = format!("artist|{}", dotnet::to_lower_invariant(mbid.trim()));
        if let Some(Cached::Text(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }
        let Some(doc) = self.get_background(&url, "artist lookup").await? else {
            return Ok(None);
        };
        let country = pure::artist_country(&doc);
        self.cache
            .set(key, Cached::Text(country.clone()), 1, Self::ARTIST_TTL);
        Ok(Some(country))
    }

    /// Where the artist of this name is from, by a name search that takes only the same name
    /// (`pure::searched_artist_country`): a two-letter country, empty when no result is that
    /// artist or it names no country, None when MusicBrainz could not be asked. On the
    /// background lane; answers are remembered a day.
    pub async fn search_artist_country(&self, name: &str) -> Result<Option<String>, MusicBrainzError> {
        let Some(url) = pure::artist_search_url(name) else {
            return Ok(None);
        };
        let key = format!("artist-search|{}", SongIdentity::key(name));
        if let Some(Cached::Text(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }
        let Some(doc) = self.get_background(&url, "artist search").await? else {
            return Ok(None);
        };
        let country = pure::searched_artist_country(&doc, name);
        self.cache
            .set(key, Cached::Text(country.clone()), 1, Self::ARTIST_TTL);
        Ok(Some(country))
    }

    /// The MusicBrainz artists named exactly `name`, by name or alias, case and accents aside:
    /// empty when there is none, None when MusicBrainz could not be asked. Answers are kept for
    /// the life of the process, so a second preview asks nothing it asked before; a failed
    /// request is not kept, so it is "could not ask", never "no such artist".
    pub async fn find_artists_named(
        &self,
        name: &str,
    ) -> Result<Option<Vec<pure::MusicBrainzArtist>>, MusicBrainzError> {
        let wanted = name.trim();
        if wanted.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if let Some(known) = self.known_artists_named(wanted) {
            return Ok(Some(known));
        }
        let Some(doc) = self
            .get(&pure::artists_named_url(wanted), "artist search")
            .await?
        else {
            return Ok(None);
        };
        let found = pure::parse_artists_named(&doc, wanted);
        let mut artists = self.artists.lock();
        make_room(&mut artists, 1, KNOWN_MAX);
        artists.insert(wanted.to_lowercase(), found.clone());
        Ok(Some(found))
    }

    /// The answer [`Self::find_artists_named`] already has for `name`, without asking.
    pub fn known_artists_named(&self, name: &str) -> Option<Vec<pure::MusicBrainzArtist>> {
        self.artists.lock().get(&name.trim().to_lowercase()).cloned()
    }

    /// The artist credits of these recordings, asked 100 to a search: every id asked gets an
    /// answer, empty when MusicBrainz does not know the recording. None when it could not be
    /// asked. Answers are kept for the life of the process.
    pub async fn recording_credits(
        &self,
        ids: &[String],
    ) -> Result<Option<HashMap<String, Vec<pure::CreditPart>>>, MusicBrainzError> {
        let wanted: Vec<String> = ids
            .iter()
            .map(|id| id.trim().to_lowercase())
            .filter(|id| pure::is_mbid(id) && self.known_credits(id).is_none())
            .collect();
        // Room for every answer this call adds, so none of them is dropped before it is read.
        make_room(&mut self.credits.lock(), wanted.len(), KNOWN_MAX);
        for batch in wanted.chunks(100) {
            let Some(url) = pure::recording_credits_url(batch) else {
                continue;
            };
            let Some(doc) = self.get(&url, "recording credits").await? else {
                return Ok(None);
            };
            let found = pure::parse_recording_credits(&doc);
            let mut credits = self.credits.lock();
            for id in batch {
                credits.insert(id.clone(), found.get(id).cloned().unwrap_or_default());
            }
        }
        let credits = self.credits.lock();
        Ok(Some(
            ids.iter()
                .map(|id| id.trim().to_lowercase())
                .filter_map(|id| credits.get(&id).map(|parts| (id, parts.clone())))
                .collect(),
        ))
    }

    /// The credit [`Self::recording_credits`] already has for one recording, without asking.
    pub fn known_credits(&self, id: &str) -> Option<Vec<pure::CreditPart>> {
        self.credits.lock().get(&id.trim().to_lowercase()).cloned()
    }

    /// One request, spaced at least the gap after the last. None on any failure, which every
    /// caller reads as "MusicBrainz had nothing to say"; the timeout is the one error that
    /// reaches the caller.
    async fn get(&self, relative: &str, what: &str) -> Result<Option<Value>, MusicBrainzError> {
        Ok(self.ask(relative, what).await?.found())
    }

    /// One request on the background lane, for work nobody is waiting on (see [`Self::ask_background`]).
    async fn get_background(&self, relative: &str, what: &str) -> Result<Option<Value>, MusicBrainzError> {
        Ok(self.ask_background(relative, what).await?.found())
    }

    /// [`Self::ask`], [`Self::ask_heart`] or [`Self::ask_background`]. What a held question came
    /// to is the answer of the next ask of it, on any lane.
    async fn ask_on(&self, relative: &str, what: &str, lane: Lane) -> Result<Asked, MusicBrainzError> {
        if lane == Lane::Heart {
            return Ok(self.ask_heart(what, |_| relative.to_string()).await.0);
        }
        if let Some(settled) = self.take_settled(relative) {
            return Ok(settled);
        }
        match lane {
            Lane::Background => self.ask_background(relative, what).await,
            Lane::Foreground | Lane::Heart => self.ask(relative, what).await,
        }
    }

    /// [`Self::get`], telling apart how MusicBrainz answered.
    async fn ask(&self, relative: &str, what: &str) -> Result<Asked, MusicBrainzError> {
        let mut last_call = self.gate.lock().await;
        if let Some(last) = *last_call {
            let due = last.at + self.gap;
            if due > Instant::now() {
                tokio::time::sleep_until(due).await;
            }
        }
        *last_call = Some(LastCall {
            at: Instant::now(),
            lane: Lane::Foreground,
        });
        self.send(relative, what).await
    }

    /// One heart question, asked again after a busy answer or a timeout, at most
    /// [`HEART_RETRIES`] times, each at the gate's next turn once MusicBrainz's `Retry-After` has
    /// passed. A `Retry-After` longer than [`LONGEST_HEART_WAIT`] makes it busy at once, as does
    /// one still running from an earlier answer. Heart questions ask one at a time, in the order
    /// they came. `question(timed_out)` is what to send, `timed_out` true once an attempt has
    /// timed out, so a carriers page can ask a smaller page; the bool returned says whether one
    /// did.
    async fn ask_heart(&self, what: &str, question: impl Fn(bool) -> String) -> (Asked, bool) {
        let _waiting = HeartWaiting::new(&self.hearts_waiting);
        let _line = self.heart_line.lock().await;
        let mut timed_out = false;
        let mut retries = 0;
        loop {
            let asking = question(timed_out);
            if let Some(settled) = self.take_settled(&asking) {
                return (settled, timed_out);
            }
            if let Some(left) = self.retry_after_left() {
                if left > LONGEST_HEART_WAIT {
                    return (
                        Asked::Busy {
                            retry_after: Some(left),
                        },
                        timed_out,
                    );
                }
                tokio::time::sleep(left).await;
            }
            self.heart_turn().await;
            let asked = match self.send(&asking, what).await {
                Ok(asked) => asked,
                // The one error `send` gives is its timeout.
                Err(_) => {
                    timed_out = true;
                    Asked::Busy { retry_after: None }
                }
            };
            let wait_too_long =
                matches!(asked, Asked::Busy { retry_after: Some(wait) } if wait > LONGEST_HEART_WAIT);
            if !asked.is_busy() || wait_too_long || retries == HEART_RETRIES {
                return (asked, timed_out);
            }
            retries += 1;
        }
    }

    /// A heart question's turn at the gate. It waits out the gap without holding the gate, so a
    /// download check that comes meanwhile takes the next turn, then holds the gate for the rest
    /// of its wait, so the turn after that is its own. It lets go before it sends.
    async fn heart_turn(&self) {
        let mut last_call = self.gate.lock().await;
        if let Some(last) = *last_call
            && last.at + self.gap > Instant::now()
        {
            let due = last.at + self.gap;
            drop(last_call);
            tokio::time::sleep_until(due).await;
            last_call = self.gate.lock().await;
            if let Some(last) = *last_call {
                tokio::time::sleep_until(last.at + self.gap).await;
            }
        }
        *last_call = Some(LastCall {
            at: Instant::now(),
            lane: Lane::Heart,
        });
    }

    /// One request on the background lane, for work nobody is waiting on. It never queues on
    /// the gate: while a heart question waits, while the gate is held, or while a call that is
    /// not its own went out less than the gap ago, it steps back for the background step-back and
    /// tries again, so a download being verified and a heart go first. It takes its turn and lets
    /// go of the gate before it sends, so a caller arriving meanwhile waits one gap at most.
    async fn ask_background(&self, relative: &str, what: &str) -> Result<Asked, MusicBrainzError> {
        loop {
            let wait = if self.hearts_waiting.load(Ordering::Acquire) > 0 {
                self.background_backoff
            } else {
                match self.gate.try_lock() {
                    Err(_) => self.background_backoff,
                    Ok(mut last_call) => {
                        let now = Instant::now();
                        match *last_call {
                            Some(last) if last.at + self.gap > now => {
                                if last.lane == Lane::Background {
                                    last.at + self.gap - now
                                } else {
                                    self.background_backoff
                                }
                            }
                            _ => {
                                *last_call = Some(LastCall {
                                    at: now,
                                    lane: Lane::Background,
                                });
                                break;
                            }
                        }
                    }
                }
            };
            tokio::time::sleep(wait).await;
        }
        self.send(relative, what).await
    }

    /// The request itself, once its turn has come, and how MusicBrainz answered it. A refused
    /// question is one MusicBrainz does not take as Octo asked it, so it is logged as a warning
    /// naming the question. A busy answer's `Retry-After` is kept for the heart lane, and every
    /// definite answer is counted for the re-asker.
    async fn send(&self, relative: &str, what: &str) -> Result<Asked, MusicBrainzError> {
        let attempt = async {
            let url = resolve(&self.base, relative)?;
            let response = self.client.get(url).send().await?;
            let answer = HttpAnswer::read(response).await?;
            if answer.is_success() {
                return anyhow::Ok(Asked::Answer(parse_json(&answer.body)?));
            }
            let status = answer.status.as_u16();
            let said: String = String::from_utf8_lossy(&answer.body).chars().take(300).collect();
            Ok(match status {
                404 => Asked::NotKnown,
                // An id that is not a MusicBrainz id, such as a junk tag: nothing is known by it.
                400 if said.contains("Invalid mbid") || asks_by_a_non_mbid(relative) => Asked::NotKnown,
                429 => self.busy(answer.retry_after),
                400..=499 => {
                    warn!("MusicBrainz refused the {what} question {relative} ({status}): {said}");
                    Asked::Refused
                }
                _ => {
                    debug!("musicbrainz {what} answered {status}");
                    self.busy(answer.retry_after)
                }
            })
        };
        let asked = match attempt.await {
            Ok(asked) => asked,
            Err(e)
                if e.downcast_ref::<reqwest::Error>()
                    .is_some_and(reqwest::Error::is_timeout) =>
            {
                return Err(MusicBrainzError::TimedOut);
            }
            Err(e) => {
                debug!("musicbrainz {what} failed: {e}");
                Asked::Busy { retry_after: None }
            }
        };
        if !asked.is_busy() {
            self.answers.send_modify(|answers| *answers += 1);
        }
        Ok(asked)
    }

    /// A busy answer, keeping MusicBrainz's `Retry-After` (at most a day) as the time before
    /// which heart questions and the re-asker send nothing.
    fn busy(&self, retry_after: Option<Duration>) -> Asked {
        if let Some(wait) = retry_after {
            let until = Instant::now() + wait.min(LONGEST_RETRY_AFTER);
            let mut not_before = self.not_before.lock();
            if not_before.is_none_or(|known| known < until) {
                *not_before = Some(until);
            }
        }
        Asked::Busy { retry_after }
    }

    /// What is left of MusicBrainz's last `Retry-After`; None once it has passed.
    fn retry_after_left(&self) -> Option<Duration> {
        let until = (*self.not_before.lock())?;
        let left = until.saturating_duration_since(Instant::now());
        (!left.is_zero()).then_some(left)
    }

    /// What a held question came to, taken by the next ask of that question.
    fn take_settled(&self, relative: &str) -> Option<Asked> {
        let mut settled = self.settled.lock();
        let at = settled.iter().position(|(question, _)| question == relative)?;
        settled.remove(at).map(|(_, asked)| asked)
    }

    /// Keeps what a held question came to for the next ask of it, the oldest dropped past
    /// [`SETTLED_MAX`].
    fn settle(&self, relative: &str, asked: Asked) {
        let mut settled = self.settled.lock();
        settled.retain(|(question, _)| question != relative);
        if settled.len() >= SETTLED_MAX {
            settled.pop_front();
        }
        settled.push_back((relative.to_string(), asked));
    }

    /// Holds one question that answered busy until MusicBrainz answers it: asked again at the
    /// heart lane's next turn, oldest first, once any `Retry-After` has passed, one entry per
    /// question (another `wake` for a held question joins it and asks again at once). `wake`
    /// runs once, on the re-asker's task, with how it ended: [`ReAsked::Answered`], whose answer
    /// is then the next ask's of that question, or [`ReAsked::GaveUp`] after [`HELD_FAILURES`]
    /// failures while MusicBrainz answered other questions. While MusicBrainz answers nothing at
    /// all, no failure counts and the question waits.
    pub fn re_ask(self: &Arc<Self>, relative: &str, wake: Box<dyn FnOnce(ReAsked) + Send>) {
        let seen = *self.answers.borrow();
        let start = {
            let mut held = self.held.lock();
            match held.queue.iter_mut().find(|waiting| waiting.question == relative) {
                Some(waiting) => waiting.wakes.push(wake),
                None => held.queue.push_back(HeldQuestion {
                    question: relative.to_string(),
                    wakes: vec![wake],
                    failures: 0,
                    seen,
                }),
            }
            !std::mem::replace(&mut held.running, true)
        };
        if start {
            let this = Arc::clone(self);
            tokio::spawn(async move { this.re_ask_held().await });
        } else {
            self.nudge.notify_one();
        }
    }

    /// The re-asker: asks the held questions again until none is left. A question that fails
    /// while MusicBrainz answered other questions since it was last asked counts a failure and
    /// goes behind the others; when nothing else was answered, [`PROBE`] tells whether MusicBrainz
    /// answers at all, and its answer counts as another. When the probe fails too, MusicBrainz
    /// answers nothing: the re-asker waits for its next answer to any question, a question held
    /// again, or the end of a `Retry-After`.
    async fn re_ask_held(self: Arc<Self>) {
        let mut answers = self.answers.subscribe();
        loop {
            let first = {
                let mut held = self.held.lock();
                let first = held
                    .queue
                    .front()
                    .map(|waiting| (waiting.question.clone(), waiting.seen));
                if first.is_none() {
                    held.running = false;
                }
                first
            };
            let Some((question, seen)) = first else {
                return;
            };
            let asked = self.ask_held(&question).await;
            if !asked.is_busy() {
                self.settle(&question, asked);
                self.end_held(&question, ReAsked::Answered);
                continue;
            }
            let answered_since = *answers.borrow_and_update() > seen;
            let musicbrainz_answers = answered_since || !self.ask_held(PROBE).await.is_busy();
            let now_seen = *answers.borrow_and_update();
            if musicbrainz_answers {
                if self.count_failure(&question, now_seen) {
                    self.settle(&question, Asked::NotKnown);
                    self.end_held(&question, ReAsked::GaveUp);
                }
            } else {
                self.mark_seen(&question, now_seen);
                self.wait_for_work(&mut answers).await;
            }
        }
    }

    /// One attempt at a held question, or at the probe, on the heart lane once any `Retry-After`
    /// has passed. It asks MusicBrainz itself: no cache and no kept answer stands in for it.
    async fn ask_held(&self, question: &str) -> Asked {
        if let Some(left) = self.retry_after_left() {
            tokio::time::sleep(left).await;
        }
        let _waiting = HeartWaiting::new(&self.hearts_waiting);
        let _line = self.heart_line.lock().await;
        self.heart_turn().await;
        self.send(question, "held question")
            .await
            .unwrap_or(Asked::Busy { retry_after: None })
    }

    /// Counts a failure of a held question and puts it behind the others; true when that was its
    /// last.
    fn count_failure(&self, question: &str, seen: u64) -> bool {
        let mut held = self.held.lock();
        let Some(at) = held.queue.iter().position(|waiting| waiting.question == question) else {
            return false;
        };
        let Some(mut waiting) = held.queue.remove(at) else {
            return false;
        };
        waiting.failures += 1;
        waiting.seen = seen;
        let last = waiting.failures >= HELD_FAILURES;
        held.queue.push_back(waiting);
        last
    }

    /// Notes how many answers MusicBrainz had given when a held question was last asked.
    fn mark_seen(&self, question: &str, seen: u64) {
        let mut held = self.held.lock();
        if let Some(waiting) = held.queue.iter_mut().find(|waiting| waiting.question == question) {
            waiting.seen = seen;
        }
    }

    /// Ends a held question for everyone waiting on it.
    fn end_held(&self, question: &str, ended: ReAsked) {
        let wakes = {
            let mut held = self.held.lock();
            let at = held.queue.iter().position(|waiting| waiting.question == question);
            at.and_then(|at| held.queue.remove(at))
                .map(|waiting| waiting.wakes)
                .unwrap_or_default()
        };
        for wake in wakes {
            wake(ended);
        }
    }

    /// Waits for MusicBrainz to answer any question, for a question held again, or for the end of
    /// MusicBrainz's `Retry-After`.
    async fn wait_for_work(&self, answers: &mut watch::Receiver<u64>) {
        let left = self.retry_after_left();
        let retry_after_ends = async move {
            match left {
                Some(left) => tokio::time::sleep(left).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = answers.changed() => {}
            () = self.nudge.notified() => {}
            () = retry_after_ends => {}
        }
    }

    /// Asks ahead what a heart of this song asks first, so its answers are kept as a heart's rows
    /// by the time the person hearts it: the catalog album (`catalog_album_id`, else found from
    /// the song's album title), the recordings its ISRC names, the release its barcode names and
    /// where its artist is from. Never the carriers. Every question goes on the background lanes
    /// of MusicBrainz and Deezer, and a busy answer ends the song's turn: nothing holds for it.
    ///
    /// Songs wait in one queue of [`WARM_UPS_MAX`], the newest asked first; one more puts out the
    /// song that has waited longest. A song is in it once: `key` names it, a song asked again
    /// while it waits moves to the front, and one asked again while its own questions are out is
    /// not queued. One song is asked at a time.
    pub fn warm_up(
        self: &Arc<Self>,
        key: &str,
        song: HeartSong,
        catalog_album_id: Option<String>,
        catalog: Arc<dyn CatalogQuestions>,
    ) {
        let start = {
            let mut warm_ups = self.warm_ups.lock();
            if warm_ups.warming.as_deref() == Some(key) {
                return;
            }
            warm_ups.waiting.retain(|waiting| waiting.key != key);
            if warm_ups.waiting.len() >= WARM_UPS_MAX {
                warm_ups.waiting.pop_front();
            }
            warm_ups.waiting.push_back(WaitingWarmUp {
                key: key.to_string(),
                song,
                catalog_album_id,
                catalog,
            });
            !std::mem::replace(&mut warm_ups.running, true)
        };
        if start {
            let this = Arc::clone(self);
            tokio::spawn(async move { this.warm_waiting().await });
        }
    }

    /// The warm-up's one task: asks each waiting song's questions, the newest first, until none is
    /// left.
    async fn warm_waiting(self: Arc<Self>) {
        loop {
            let next = {
                let mut warm_ups = self.warm_ups.lock();
                let next = warm_ups.waiting.pop_back();
                warm_ups.warming = next.as_ref().map(|waiting| waiting.key.clone());
                if next.is_none() {
                    warm_ups.running = false;
                }
                next
            };
            let Some(next) = next else {
                return;
            };
            let questions = HeartQuestions::new(Arc::clone(&self), next.catalog, Lane::Background);
            let warmed = match questions
                .identity(&next.song, next.catalog_album_id.as_deref())
                .await
            {
                Ok(identity) => questions.home_country(&identity.artist_id).await.map(drop),
                Err(busy) => Err(busy),
            };
            if let Err(busy) = warmed {
                debug!("the warm-up of {} stopped: {busy:?} did not answer", next.key);
            }
        }
    }

    /// Waits until the warm-up has no song left, waiting or being asked.
    #[cfg(test)]
    pub(crate) async fn warmed_up(&self) {
        for _ in 0..2000 {
            if !self.warm_ups.lock().running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the warm-up never finished");
    }
}

/// Whether a question looks something up by an id that is not a MusicBrainz id
/// (`release/<id>`, `recording/<id>`, `artist/<id>`, ...): MusicBrainz has nothing by it.
/// The question [`MusicBrainzClient::current_recording`] asks of one recording id, as
/// [`MusicBrainzClient::re_ask`] takes it; None for an id that is no MusicBrainz id.
pub fn recording_question(recording_id: &str) -> Option<String> {
    let id = recording_id.trim().to_lowercase();
    pure::is_mbid(&id).then(|| format!("recording/{id}?fmt=json"))
}

/// The code lookup's question for one ISRC, as [`MusicBrainzClient::re_ask`] takes it; None for
/// a code that is no ISRC.
pub fn code_question(isrc: &str) -> Option<String> {
    SongIdentity::normalize_isrc(isrc).map(|code| format!("isrc/{code}?inc=artist-credits&fmt=json"))
}

fn asks_by_a_non_mbid(relative: &str) -> bool {
    let path = relative.split('?').next().unwrap_or("");
    let mut parts = path.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(resource), Some(id), None)
            if !id.is_empty()
                && matches!(
                    resource,
                    "release" | "recording" | "artist" | "release-group" | "label" | "work"
                ) =>
        {
            !pure::is_mbid(&dotnet::unescape_data_string(id))
        }
        _ => false,
    }
}

/// The recordings of an answer that `keep` lets through, without DJ-mix segments, each with at
/// most [`MOST_RELEASES_PER_RECORDING`] releases.
fn recordings_of(doc: &Value, keep: impl Fn(&Value) -> bool) -> Vec<Value> {
    doc.get("recordings")
        .and_then(Value::as_array)
        .map(|recordings| {
            recordings
                .iter()
                .filter(|recording| keep(recording) && !is_dj_mix_segment(recording))
                .map(|recording| {
                    let mut recording = recording.clone();
                    if let Some(releases) = recording.get_mut("releases").and_then(Value::as_array_mut) {
                        releases.truncate(MOST_RELEASES_PER_RECORDING);
                    }
                    recording
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether one recording carries the code.
fn carries(recording: &Value, code: &str) -> bool {
    recording
        .get("isrcs")
        .and_then(Value::as_array)
        .is_some_and(|codes| {
            codes
                .iter()
                .filter_map(Value::as_str)
                .any(|carried| carried.eq_ignore_ascii_case(code))
        })
}

/// A recording MusicBrainz keeps as one segment of a DJ mix ("part of … DJ-mix", however the
/// hyphen or the space is written): the song inside someone's mix, never the song itself.
fn is_dj_mix_segment(recording: &Value) -> bool {
    recording
        .get("disambiguation")
        .and_then(Value::as_str)
        .is_some_and(|said| {
            let squeezed: String = said
                .to_lowercase()
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '-' && !('\u{2010}'..='\u{2015}').contains(c))
                .collect();
            squeezed.contains("djmix")
        })
}

/// When the last request went out, and on which lane.
#[derive(Debug, Clone, Copy)]
struct LastCall {
    at: Instant,
    lane: Lane,
}

/// The "musicbrainz" HTTP client with its request timeout.
fn http_client(timeout: Duration) -> reqwest::Client {
    client_builder()
        .timeout(timeout)
        .user_agent(octo_user_agent::value())
        .build()
        .expect("the MusicBrainz HTTP client builds")
}

/// One recording of a code lookup as AcoustID's answers carry recordings.
fn answer_recording(recording: CodeRecording, code: &str) -> AnswerRecording {
    AnswerRecording {
        id: recording.id,
        title: recording.title,
        artists: recording
            .credits
            .into_iter()
            .map(|credit| AnswerArtist {
                id: Some(credit.artist_id).filter(|id| !id.is_empty()),
                name: if credit.name.is_empty() {
                    credit.artist_name
                } else {
                    credit.name
                },
            })
            .collect(),
        duration_seconds: recording.length_ms.map(|ms| f64::from(ms) / 1000.0),
        isrcs: vec![code.to_string()],
        sources: 0,
    }
}

/// Counts one heart question as waiting for as long as it lives.
struct HeartWaiting<'a>(&'a AtomicUsize);

impl<'a> HeartWaiting<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        HeartWaiting(count)
    }
}

impl Drop for HeartWaiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What the first of the callers asking one code came to, for the others waiting on it.
#[derive(Debug)]
struct CodeFlight {
    lane: Lane,
    /// None when it timed out.
    answer: Option<IsrcLookup>,
}

impl CodeFlight {
    /// Whether a caller on `lane` takes this answer as its own: a definite one always, a busy one
    /// unless the caller's lane asks again and this one's did not.
    fn settles(&self, lane: Lane) -> bool {
        let busy = matches!(self.answer, None | Some(IsrcLookup::Unanswered));
        !busy || lane != Lane::Heart || self.lane == Lane::Heart
    }
}

/// One question held until MusicBrainz answers it.
struct HeldQuestion {
    question: String,
    wakes: Vec<Box<dyn FnOnce(ReAsked) + Send>>,
    /// Its failures while MusicBrainz answered other questions.
    failures: u32,
    /// How many answers MusicBrainz had given when it was last asked.
    seen: u64,
}

/// The re-asker's questions, oldest first, and whether its task is running.
#[derive(Default)]
struct HeldQuestions {
    queue: VecDeque<HeldQuestion>,
    running: bool,
}

/// One song waiting for its warm-up.
struct WaitingWarmUp {
    key: String,
    song: HeartSong,
    catalog_album_id: Option<String>,
    catalog: Arc<dyn CatalogQuestions>,
}

/// The warm-up's songs, the newest last, the one being asked, and whether its task is running.
#[derive(Default)]
struct WarmUps {
    waiting: VecDeque<WaitingWarmUp>,
    warming: Option<String>,
    running: bool,
}

/// What a heart knows of a song from its routing: the catalog row and the album it was listed
/// from. A count that is not above zero is unknown.
pub fn heart_song(routing: &SoulseekRouting) -> HeartSong {
    let text = |value: &Option<String>| value.as_deref().map(str::trim).unwrap_or_default().to_string();
    let count = |value: Option<i32>| {
        value
            .and_then(|count| u32::try_from(count).ok())
            .filter(|count| *count > 0)
    };
    HeartSong {
        artist: text(&routing.artist),
        title: text(&routing.title),
        album: text(&routing.album),
        disc: count(routing.disc_number),
        position: count(routing.track),
        total_tracks: count(routing.total_tracks),
        isrc: text(&routing.isrc),
        seconds: count(routing.duration).unwrap_or(0),
        explicit: explicit_status::from_catalog(routing.explicit_content, None),
    }
}

/// The music database calls the release identifier makes. A timeout is the error the
/// identifier sees; every other failure is already None.
#[async_trait::async_trait]
impl ReleaseLookup for MusicBrainzClient {
    async fn lookup_release(&self, release_id: &str) -> anyhow::Result<Option<ReleaseDetails>> {
        Ok(MusicBrainzClient::lookup_release(self, release_id)
            .await?
            .map(|details| (*details).clone()))
    }

    async fn search_recordings(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> anyhow::Result<Option<Value>> {
        Ok(MusicBrainzClient::search_recordings(self, artist, title, duration_seconds).await?)
    }

    async fn lookup_isrc(&self, isrc: &str) -> anyhow::Result<Option<Value>> {
        match MusicBrainzClient::lookup_isrc(self, isrc).await? {
            IsrcLookup::Found(doc) => Ok(Some(doc)),
            IsrcLookup::NotKnown => Ok(None),
            IsrcLookup::Unanswered => anyhow::bail!("MusicBrainz could not be asked about the code"),
            IsrcLookup::Refused => anyhow::bail!("MusicBrainz refused the question about the code"),
        }
    }
}

impl Default for MusicBrainzClient {
    fn default() -> Self {
        Self::new()
    }
}

/// The most artist searches, and the most recording credits, kept in memory.
const KNOWN_MAX: usize = 10_000;

/// Drops every kept answer when `adding` more would pass `max`: answers MusicBrainz gives again,
/// so the map stays bounded however long the process runs.
fn make_room<K, V>(map: &mut HashMap<K, V>, adding: usize, max: usize) {
    if map.len() + adding > max {
        map.clear();
    }
}

/// One of a heart's answers, kept as compact rows.
#[derive(Debug, Clone)]
enum HeartRow {
    /// `album|<deezer id>`: the album, and a row per track.
    Album(CatalogAlbum, Vec<CatalogTrack>),
    /// `barcode|<upc>`: each looked-up release, and a row per track.
    Releases(Vec<BarcodeRelease>),
    /// `carriers|<recording id>`: a row per carrier release.
    Carriers(Carriers),
    /// `artist|<id>`: where the artist is from.
    Country(String),
    /// `recording|<id>`: the id MusicBrainz files the recording under now.
    Recording(String),
}

/// A heart's answers by key, the least recently used first out, at most [`HEART_ROWS_MAX`] rows
/// in all, never expiring.
#[derive(Default)]
struct HeartRows {
    /// Each answer, its rows and when it was last used.
    entries: HashMap<String, (HeartRow, usize, u64)>,
    /// The keys by when they were last used.
    by_use: BTreeMap<u64, String>,
    rows: usize,
    clock: u64,
}

impl HeartRows {
    fn get(&mut self, key: &str) -> Option<HeartRow> {
        self.clock += 1;
        let clock = self.clock;
        let (row, _, used) = self.entries.get_mut(key)?;
        self.by_use.remove(&*used);
        *used = clock;
        self.by_use.insert(clock, key.to_string());
        Some(row.clone())
    }

    /// Keeps an answer of `size` rows, dropping the least recently used until it fits. An answer
    /// bigger than the whole cache is not kept.
    fn put(&mut self, key: String, row: HeartRow, size: usize) {
        let size = size.max(1);
        if size > HEART_ROWS_MAX {
            return;
        }
        if let Some((_, old_size, used)) = self.entries.remove(&key) {
            self.by_use.remove(&used);
            self.rows -= old_size;
        }
        while self.rows + size > HEART_ROWS_MAX {
            let Some((_, oldest)) = self.by_use.pop_first() else {
                break;
            };
            if let Some((_, old_size, _)) = self.entries.remove(&oldest) {
                self.rows -= old_size;
            }
        }
        self.clock += 1;
        self.by_use.insert(self.clock, key.clone());
        self.entries.insert(key, (row, size, self.clock));
        self.rows += size;
    }
}

/// Which service did not answer one of a heart's questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartBusy {
    /// MusicBrainz did not answer this question (its address relative to the base address): the
    /// one a hold re-asks through [`MusicBrainzClient::re_ask`].
    MusicBrainz(String),
    /// Deezer did not answer.
    Deezer,
}

/// Deezer's side of a heart's questions: its album search, and one album with its tracks, each
/// on Deezer's background lane for [`Lane::Background`].
#[async_trait::async_trait]
pub trait CatalogQuestions: Send + Sync {
    /// The albums Deezer's album search finds for an artist and an album title.
    async fn album_search(&self, artist: &str, album: &str, lane: Lane) -> DeezerAsked<Vec<CatalogAlbumHit>>;
    /// One album and its tracks.
    async fn album(&self, album_id: &str, lane: Lane) -> DeezerAsked<(CatalogAlbum, Vec<CatalogTrack>)>;
}

#[async_trait::async_trait]
impl CatalogQuestions for DeezerMetadataService {
    async fn album_search(&self, artist: &str, album: &str, lane: Lane) -> DeezerAsked<Vec<CatalogAlbumHit>> {
        self.album_search_hits(artist, album, lane == Lane::Background)
            .await
    }

    async fn album(&self, album_id: &str, lane: Lane) -> DeezerAsked<(CatalogAlbum, Vec<CatalogTrack>)> {
        self.album_rows(album_id, lane == Lane::Background).await
    }
}

/// A heart's questions to MusicBrainz and Deezer on one lane, each answer read into the compact
/// rows `octo_core::fingerprint::heart_release` picks from and kept as those rows: at most 20,000
/// rows on every lane together, found answers only, the least recently used first out, never
/// expiring. Cheap to make: each caller makes one on its own lane, and every one reads and fills
/// the client's rows. A busy answer names the question to hold; a question MusicBrainz could not
/// answer three times answers with what is known without it.
pub struct HeartQuestions {
    music_brainz: Arc<MusicBrainzClient>,
    catalog: Arc<dyn CatalogQuestions>,
    lane: Lane,
}

impl HeartQuestions {
    pub fn new(music_brainz: Arc<MusicBrainzClient>, catalog: Arc<dyn CatalogQuestions>, lane: Lane) -> Self {
        Self {
            music_brainz,
            catalog,
            lane,
        }
    }

    /// The lane its questions wait in.
    pub fn lane(&self) -> Lane {
        self.lane
    }

    /// The recordings MusicBrainz files under the song's ISRC: none when it is not an ISRC, when
    /// MusicBrainz knows no recording by it, or refused the question. Kept as the code lookup's
    /// own answer is.
    pub async fn code_recordings(&self, isrc: &str) -> Result<Vec<CodeRecording>, HeartBusy> {
        let Some(question) = release::isrc_lookup_url(isrc) else {
            return Ok(Vec::new());
        };
        match self.music_brainz.code_recordings_on(isrc, self.lane).await {
            Ok(IsrcLookup::Found(doc)) => Ok(release::read_code_recordings(&doc)),
            Ok(IsrcLookup::NotKnown | IsrcLookup::Refused) => Ok(Vec::new()),
            Ok(IsrcLookup::Unanswered) | Err(_) => Err(HeartBusy::MusicBrainz(question)),
        }
    }

    /// One Deezer album and its tracks: None when Deezer has no such album.
    pub async fn catalog_album(
        &self,
        album_id: &str,
    ) -> Result<Option<(CatalogAlbum, Vec<CatalogTrack>)>, HeartBusy> {
        let id = album_id.trim();
        if id.is_empty() {
            return Ok(None);
        }
        let key = format!("album|{id}");
        if let Some(HeartRow::Album(album, tracks)) = self.kept(&key) {
            return Ok(Some((album, tracks)));
        }
        match self.catalog.album(id, self.lane).await {
            DeezerAsked::Answer((album, tracks)) => {
                let size = 1 + tracks.len();
                self.keep(key, HeartRow::Album(album.clone(), tracks.clone()), size);
                Ok(Some((album, tracks)))
            }
            DeezerAsked::NoData => Ok(None),
            DeezerAsked::Busy => Err(HeartBusy::Deezer),
        }
    }

    /// The albums Deezer's album search finds for an artist and an album title: none for a blank
    /// title.
    pub async fn catalog_album_hits(
        &self,
        artist: &str,
        album: &str,
    ) -> Result<Vec<CatalogAlbumHit>, HeartBusy> {
        if dotnet::is_blank(album) {
            return Ok(Vec::new());
        }
        match self.catalog.album_search(artist, album, self.lane).await {
            DeezerAsked::Answer(hits) => Ok(hits),
            DeezerAsked::NoData => Ok(Vec::new()),
            DeezerAsked::Busy => Err(HeartBusy::Deezer),
        }
    }

    /// The releases a catalog album's UPC names, each looked up: of the barcode search's first
    /// results, those whose barcode is the UPC as a number, and of their lookups those that name
    /// the release asked.
    pub async fn barcode_releases(&self, upc: &str) -> Result<Vec<BarcodeRelease>, HeartBusy> {
        let upc = upc.trim();
        let Some(search) = release::barcode_search_url(upc) else {
            return Ok(Vec::new());
        };
        let key = format!("barcode|{upc}");
        if let Some(HeartRow::Releases(releases)) = self.kept(&key) {
            return Ok(releases);
        }
        let Asked::Answer(doc) = self.ask(&search, "barcode search").await? else {
            return Ok(Vec::new());
        };
        let mut releases = Vec::new();
        for id in release::releases_to_look_up(&release::read_barcode_hits(&doc), upc) {
            let Some(lookup) = release::release_lookup_url(&id) else {
                continue;
            };
            if let Asked::Answer(doc) = self.ask(&lookup, "release lookup").await?
                && let Some(found) = release::read_barcode_release(&doc)
                && found.id == id
            {
                releases.push(found);
            }
        }
        if !releases.is_empty() {
            let size = releases
                .iter()
                .map(|found| {
                    1 + found
                        .media
                        .iter()
                        .map(|medium| medium.tracks.len())
                        .sum::<usize>()
                })
                .sum();
            self.keep(key, HeartRow::Releases(releases.clone()), size);
        }
        Ok(releases)
    }

    /// Where the artist with this MusicBrainz id is from: "" when MusicBrainz names no country,
    /// does not know the artist, or the id is blank.
    pub async fn home_country(&self, artist_id: &str) -> Result<String, HeartBusy> {
        let Some(question) = pure::artist_lookup_url(artist_id) else {
            return Ok(String::new());
        };
        let key = artist_key(artist_id);
        if let Some(HeartRow::Country(country)) = self.kept(&key) {
            return Ok(country);
        }
        let Asked::Answer(doc) = self.ask(&question, "artist lookup").await? else {
            return Ok(String::new());
        };
        let country = pure::artist_country(&doc);
        self.keep(key, HeartRow::Country(country.clone()), 1);
        Ok(country)
    }

    /// Every official release that carries one recording, its pages asked in order to the end.
    /// On the heart lane a page that timed out is asked again in pages of
    /// `CARRIERS_PAGE_AFTER_TIMEOUT` from the same offset, and the pages after it keep that size.
    /// A page MusicBrainz could not answer is left out. Kept only when every page answered.
    pub async fn carriers(&self, recording_id: &str) -> Result<Carriers, HeartBusy> {
        let id = recording_id.trim();
        let mut carriers = Carriers {
            recording_id: id.to_string(),
            releases: Vec::new(),
        };
        if !pure::is_mbid(id) {
            return Ok(carriers);
        }
        let key = carriers_key(id);
        if let Some(HeartRow::Carriers(kept)) = self.kept(&key) {
            return Ok(kept);
        }
        let mut offset = 0;
        let mut limit = release::CARRIERS_PAGE;
        let mut release_count = None;
        let mut whole = true;
        loop {
            let page = |limit: u32| release::carriers_url(id, offset, limit).unwrap_or_default();
            let (asked, timed_out) = match self.lane {
                Lane::Heart => {
                    self.music_brainz
                        .ask_heart("carriers page", |timed_out| {
                            page(if timed_out {
                                release::CARRIERS_PAGE_AFTER_TIMEOUT
                            } else {
                                limit
                            })
                        })
                        .await
                }
                lane => {
                    let asked = self
                        .music_brainz
                        .ask_on(&page(limit), "carriers page", lane)
                        .await;
                    (asked.unwrap_or(Asked::Busy { retry_after: None }), false)
                }
            };
            if timed_out {
                limit = release::CARRIERS_PAGE_AFTER_TIMEOUT;
            }
            match asked {
                Asked::Answer(doc) => {
                    let read = release::read_carrier_page(&doc);
                    release_count = Some(read.release_count);
                    let next = release::next_carrier_offset(offset, &read);
                    carriers.releases.extend(read.releases);
                    match next {
                        Some(next) => offset = next,
                        None => break,
                    }
                }
                Asked::Busy { .. } => return Err(HeartBusy::MusicBrainz(page(limit))),
                Asked::NotKnown | Asked::Refused => {
                    whole = false;
                    match release_count {
                        Some(count) if offset + limit < count => offset += limit,
                        _ => break,
                    }
                }
            }
        }
        if whole && !carriers.releases.is_empty() {
            let size = carriers.releases.len();
            self.keep(key, HeartRow::Carriers(carriers.clone()), size);
        }
        Ok(carriers)
    }

    /// What the song identifies as from one album context: the catalog album (its Deezer id,
    /// else found from the album title, else none), the recordings its ISRC names, the release
    /// its UPC names that the person listened from, and from those the recordings heard.
    pub async fn identity(
        &self,
        song: &HeartSong,
        catalog_album_id: Option<&str>,
    ) -> Result<HeardIdentity, HeartBusy> {
        let catalog = match catalog_album_id.map(str::trim).filter(|id| !id.is_empty()) {
            Some(id) => self.catalog_album(id).await?,
            None => self.catalog_album_by_title(song).await?,
        };
        let code_recordings = self.code_recordings(&song.isrc).await?;
        let releases = match &catalog {
            Some((album, _)) => self.barcode_releases(&album.upc).await?,
            None => Vec::new(),
        };
        let listened = release::listened_release(song, &releases, &code_recordings);
        Ok(release::identify(song, &code_recordings, listened))
    }

    /// The release groups Lidarr is asked for, in order: the artist's home country and the
    /// carriers of every heard recording, then the walk. Its first entry is the listened
    /// release's group when there is one, which needs none of these
    /// (`HeardIdentity::listened_group`).
    pub async fn walk(&self, song: &HeartSong, identity: &HeardIdentity) -> Result<Vec<String>, HeartBusy> {
        let home = self.home_country(&identity.artist_id).await?;
        let mut carriers = Vec::with_capacity(identity.recordings.len());
        for id in &identity.recordings {
            carriers.push(self.carriers(id).await?);
        }
        Ok(release::walk(song, identity, &home, &carriers))
    }

    /// The release group a song with no album is filed under, with its title: the walk's first
    /// pick for the song's ISRC alone. Only the code lookup is asked; the carriers and the
    /// artist's country are read from what is kept, so with nothing kept there is no pick. None
    /// when the song has no ISRC, MusicBrainz is busy, or the walk picks no titled group.
    pub async fn no_album_pick(&self, song: &HeartSong) -> Option<(String, String)> {
        let codes = self.code_recordings(&song.isrc).await.ok()?;
        let identity = release::identify(song, &codes, None);
        let home = self.known_home_country(&identity.artist_id).unwrap_or_default();
        let carriers: Vec<Carriers> = identity
            .recordings
            .iter()
            .filter_map(|id| self.known_carriers(id))
            .collect();
        let group = release::walk(song, &identity, &home, &carriers)
            .into_iter()
            .next()?;
        let title = carriers
            .iter()
            .flat_map(|carried| &carried.releases)
            .find(|carrier| dotnet::eq_ignore_case(&carrier.group.id, &group))
            .map(|carrier| carrier.group.title.trim().to_string())
            .filter(|title| !title.is_empty())?;
        Some((group, title))
    }

    /// The carriers of one recording when they are kept, without asking.
    pub fn known_carriers(&self, recording_id: &str) -> Option<Carriers> {
        match self.kept(&carriers_key(recording_id.trim())) {
            Some(HeartRow::Carriers(carriers)) => Some(carriers),
            _ => None,
        }
    }

    /// Where an artist is from when it is kept, without asking.
    pub fn known_home_country(&self, artist_id: &str) -> Option<String> {
        match self.kept(&artist_key(artist_id)) {
            Some(HeartRow::Country(country)) => Some(country),
            _ => None,
        }
    }

    /// The catalog album found from the song's album title: the first of the search's candidates
    /// whose album and tracks fit the song.
    async fn catalog_album_by_title(
        &self,
        song: &HeartSong,
    ) -> Result<Option<(CatalogAlbum, Vec<CatalogTrack>)>, HeartBusy> {
        let hits = self.catalog_album_hits(&song.artist, &song.album).await?;
        for hit in release::catalog_album_candidates(song, &hits) {
            if let Some((album, tracks)) = self.catalog_album(&hit.id).await?
                && release::catalog_album_fits(song, Some(&album), &tracks)
            {
                return Ok(Some((album, tracks)));
            }
        }
        Ok(None)
    }

    /// One MusicBrainz question on this lane: its answer, or the question to hold.
    async fn ask(&self, question: &str, what: &str) -> Result<Asked, HeartBusy> {
        match self.music_brainz.ask_on(question, what, self.lane).await {
            Ok(asked) if !asked.is_busy() => Ok(asked),
            Ok(_) | Err(_) => Err(HeartBusy::MusicBrainz(question.to_string())),
        }
    }

    fn kept(&self, key: &str) -> Option<HeartRow> {
        self.music_brainz.heart_rows.lock().get(key)
    }

    fn keep(&self, key: String, row: HeartRow, size: usize) {
        self.music_brainz.heart_rows.lock().put(key, row, size);
    }
}

fn carriers_key(recording_id: &str) -> String {
    format!("carriers|{}", recording_id.to_lowercase())
}

fn artist_key(artist_id: &str) -> String {
    format!("artist|{}", dotnet::to_lower_invariant(artist_id.trim()))
}

#[cfg(test)]
#[path = "music_brainz_client_tests.rs"]
mod tests;
