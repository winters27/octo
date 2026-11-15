//! Turns a ready generated station into one long MP3 response. Recommendation state and stream
//! orchestration stay in core Octo; ffmpeg is only a codec adapter. The flow picker and the
//! kinship tags are `octo_core::last_fm::last_fm_radio_stream_service`.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, LazyLock, OnceLock};

use chrono::{TimeDelta, Utc};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use octo_core::common::{dotnet, skipped_takes};
use octo_core::last_fm::LastFmTrack;
use octo_core::last_fm::RadioAudioProfile;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::models::domain::Song;
use octo_core::models::radio::{LastFmRadioPlay, LastFmRadioStation, LastFmRadioTrack};
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub use octo_core::last_fm::last_fm_radio_stream_service::{
    FLOW_WINDOW, KINSHIP_WEIGHT, READY_POOL_SIZE, choose_by_flow, estrangement, flow_distance, kinship_tags,
};

use super::OperationCanceled;
use super::icy_metadata_stream::{DEFAULT_INTERVAL, IcyMetadataStream};
use super::last_fm_radio_audio_transcoder::ILastFmRadioAudioTranscoder;
use super::last_fm_radio_recommendation_service::LastFmRadioRecommendationService;
use super::last_fm_radio_refresh_queue::LastFmRadioRefreshQueue;
use super::last_fm_radio_state_store::{LastFmRadioStateStore, route_track};
use super::last_fm_radio_stream_session_store::{
    LastFmRadioStreamSession, LastFmRadioStreamSessionStore, PreparedRadioTrack,
};
use super::last_fm_radio_track_cache::{LastFmRadioTrackCache, Producer};
use super::last_fm_radio_track_resolver::LastFmRadioTrackResolver;
use super::radio_tune_in_selector::IRadioTuneInSelector;
use super::{LastFmScrobbleService, LastFmService};
use crate::services::family::AudioGate;
use crate::services::i_download_service::{DirectStreamInfo, IDownloadService};
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::listen_brainz::ListenBrainzService;
use crate::services::listening::{ListeningLog, Play as ListeningPlay};
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::{ExternalIdRegistry, RadioQueueStore, SoulseekMetadataService};
use crate::services::subsonic::SubsonicProxyService;

/// The radio streams playing at once, process-wide: 8, or more with Family on (see
/// [`LastFmRadioStreamService::stream_cap`]).
static STREAM_SLOTS: LazyLock<StreamSlots> = LazyLock::new(StreamSlots::default);

/// How many new songs a stream asks its station for at once.
const CONTINUE_WITH: usize = 25;

/// How few songs a stream may have left before it asks its station for more.
const CONTINUE_BEFORE: usize = 20;

/// The most new songs one stream adds before it plays its station from the top again.
const CONTINUE_AT_MOST: usize = 1000;

/// How many of the songs it played a stream leaves out when it asks for more.
const PLAYED_KEPT: usize = 200;

/// A count of playing radio streams with a limit read each time a stream asks to start.
#[derive(Default)]
pub struct StreamSlots {
    playing: Mutex<usize>,
    freed: Notify,
}

impl StreamSlots {
    /// Waits until fewer than `cap()` streams play, then counts this one until the slot is
    /// dropped.
    pub async fn take(&self, cap: impl Fn() -> usize) -> StreamSlot<'_> {
        loop {
            let freed = self.freed.notified();
            {
                let mut playing = self.playing.lock();
                if *playing < cap().max(1) {
                    *playing += 1;
                    return StreamSlot { slots: self };
                }
            }
            freed.await;
        }
    }

    /// How many streams play now.
    pub fn playing(&self) -> usize {
        *self.playing.lock()
    }
}

/// One playing radio stream's place in [`StreamSlots`].
pub struct StreamSlot<'a> {
    slots: &'a StreamSlots,
}

impl Drop for StreamSlot<'_> {
    fn drop(&mut self) {
        *self.slots.playing.lock() -= 1;
        self.slots.freed.notify_waiters();
    }
}

/// What a warm of a listener's stored stations managed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioWarmupResult {
    pub station_count: usize,
    pub ready_station_count: usize,
    pub ready_track_count: usize,
}

/// The services the stream service is built over.
pub struct LastFmRadioStreamServiceParts {
    pub state: Arc<LastFmRadioStateStore>,
    /// The Last.fm settings.
    pub settings: Arc<SettingsStore>,
    pub library: Arc<dyn ILocalLibraryService>,
    pub proxy: SubsonicProxyService,
    pub downloads: Arc<dyn IDownloadService>,
    pub transcoder: Arc<dyn ILastFmRadioAudioTranscoder>,
    pub cache: Arc<LastFmRadioTrackCache>,
    pub sessions: Arc<LastFmRadioStreamSessionStore>,
    pub registry: Arc<ExternalIdRegistry>,
    pub metadata: Arc<dyn IMusicMetadataService>,
    pub queues: Arc<RadioQueueStore>,
    pub refresh_queue: Arc<LastFmRadioRefreshQueue>,
    pub tune_in: Arc<dyn IRadioTuneInSelector>,
    pub last_fm: Option<Arc<LastFmService>>,
    pub listen_brainz: Option<Arc<ListenBrainzService>>,
    pub last_fm_scrobbles: Option<Arc<LastFmScrobbleService>>,
    /// The stations' picker, which a stream asks for more songs once its snapshot runs out;
    /// None plays the snapshot round and round.
    pub recommendations: Option<Arc<LastFmRadioRecommendationService>>,
}

struct Shared_ {
    state: Arc<LastFmRadioStateStore>,
    settings: Arc<SettingsStore>,
    library: Arc<dyn ILocalLibraryService>,
    downloads: Arc<dyn IDownloadService>,
    transcoder: Arc<dyn ILastFmRadioAudioTranscoder>,
    cache: Arc<LastFmRadioTrackCache>,
    sessions: Arc<LastFmRadioStreamSessionStore>,
    registry: Arc<ExternalIdRegistry>,
    metadata: Arc<dyn IMusicMetadataService>,
    queues: Arc<RadioQueueStore>,
    refresh_queue: Arc<LastFmRadioRefreshQueue>,
    tune_in: Arc<dyn IRadioTuneInSelector>,
    last_fm: Option<Arc<LastFmService>>,
    listen_brainz: Option<Arc<ListenBrainzService>>,
    last_fm_scrobbles: Option<Arc<LastFmScrobbleService>>,
    recommendations: Option<Arc<LastFmRadioRecommendationService>>,
    /// The audio gate's rules, once the app hands them over.
    family: OnceLock<Arc<AudioGate>>,
    /// The listening log each finished song is written to, once the app hands it over.
    listening: OnceLock<Arc<ListeningLog>>,
}

/// The songs a stream plays after its station's snapshot, asked of the station's picker a few
/// songs before the snapshot runs out, and the songs the stream has played.
#[derive(Default)]
struct Continuation {
    /// The new songs, each with a resolved id, in the order they play after the snapshot.
    tracks: Vec<LastFmRadioTrack>,
    /// What the stream played, the newest last, at most [`PLAYED_KEPT`].
    played: VecDeque<LastFmRadioTrack>,
    /// The ask under way.
    asking: Option<tokio::task::JoinHandle<Vec<LastFmRadioTrack>>>,
    /// The last ask brought nothing new: no other until the stream starts from the top again.
    dry: bool,
}

impl Continuation {
    /// Counts a song the stream played.
    fn count_played(&mut self, track: &LastFmRadioTrack) {
        self.played.push_back(track.clone());
        if self.played.len() > PLAYED_KEPT {
            self.played.pop_front();
        }
    }

    /// Adds the songs an ask found that are not here yet, each given an id to play by.
    fn add(&mut self, found: Vec<LastFmRadioTrack>, registry: &ExternalIdRegistry) {
        let before = self.tracks.len();
        let mut known: HashSet<String> = self.tracks.iter().map(folded_key).collect();
        for mut track in found {
            if !known.insert(folded_key(&track)) {
                continue;
            }
            if !has_resolved_id(&track) {
                route_track(registry, &mut track);
            }
            self.tracks.push(track);
        }
        self.dry = self.tracks.len() == before;
    }
}

impl Drop for Continuation {
    fn drop(&mut self) {
        if let Some(asking) = self.asking.take() {
            asking.abort();
        }
    }
}

/// A snapshot track with a resolved id, its place among those, and its cache key.
#[derive(Debug, Clone)]
struct RadioCandidate {
    track: LastFmRadioTrack,
    index: usize,
    key: String,
}

impl RadioCandidate {
    fn prepared(&self, path: std::path::PathBuf) -> PreparedRadioTrack {
        PreparedRadioTrack::new(path, self.track.clone(), self.index, self.key.clone())
    }
}

/// The stream service for one request: over that request's proxy, with its own set of pool
/// warmers, sharing the rest. [`LastFmRadioStreamService::scoped`] makes one.
#[derive(Clone)]
pub struct LastFmRadioStreamService {
    shared: Arc<Shared_>,
    proxy: SubsonicProxyService,
    resolver: LastFmRadioTrackResolver,
    /// The pool warms running from this service.
    pool_warmers: Arc<Mutex<HashSet<String>>>,
}

type Replenishment = Shared<BoxFuture<'static, Result<Option<PreparedRadioTrack>, String>>>;

/// Where a stream writes: the response itself, or the response with ICY metadata framing.
enum RadioOutput<'a, W: AsyncWrite + Unpin> {
    Plain(&'a mut W),
    Icy(IcyMetadataStream<&'a mut W>),
}

impl<W: AsyncWrite + Unpin> RadioOutput<'_, W> {
    fn set_track(&mut self, track: &LastFmRadioTrack) {
        if let RadioOutput::Icy(icy) = self {
            icy.set_track(track);
        }
    }

    async fn write(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        match self {
            RadioOutput::Plain(output) => output.write_all(buffer).await,
            RadioOutput::Icy(icy) => icy.write(buffer).await,
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            RadioOutput::Plain(output) => output.flush().await,
            RadioOutput::Icy(icy) => icy.flush().await,
        }
    }
}

impl LastFmRadioStreamService {
    pub fn new(parts: LastFmRadioStreamServiceParts) -> Self {
        let resolver = LastFmRadioTrackResolver::new(
            parts.proxy.clone(),
            parts.metadata.clone(),
            parts.registry.clone(),
        );
        LastFmRadioStreamService {
            shared: Arc::new(Shared_ {
                state: parts.state,
                settings: parts.settings,
                library: parts.library,
                downloads: parts.downloads,
                transcoder: parts.transcoder,
                cache: parts.cache,
                sessions: parts.sessions,
                registry: parts.registry,
                metadata: parts.metadata,
                queues: parts.queues,
                refresh_queue: parts.refresh_queue,
                tune_in: parts.tune_in,
                last_fm: parts.last_fm,
                listen_brainz: parts.listen_brainz,
                last_fm_scrobbles: parts.last_fm_scrobbles,
                recommendations: parts.recommendations,
                family: OnceLock::new(),
                listening: OnceLock::new(),
            }),
            proxy: parts.proxy,
            resolver,
            pool_warmers: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The service for one request: over `proxy` (the request's own), with no pool warmers of
    /// its own yet.
    pub fn scoped(&self, proxy: SubsonicProxyService) -> Self {
        LastFmRadioStreamService {
            shared: Arc::clone(&self.shared),
            resolver: LastFmRadioTrackResolver::new(
                proxy.clone(),
                self.shared.metadata.clone(),
                self.shared.registry.clone(),
            ),
            proxy,
            pool_warmers: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The service for background work (the warmup's): the same proxy, and no pool warmers of
    /// its own yet.
    pub fn new_scope(&self) -> Self {
        LastFmRadioStreamService {
            shared: Arc::clone(&self.shared),
            proxy: self.proxy.clone(),
            resolver: self.resolver.clone(),
            pool_warmers: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn resolver(&self) -> &LastFmRadioTrackResolver {
        &self.resolver
    }

    /// Hands over the audio gate's rules: the stream limit grows with the family, and each
    /// stream follows its listener's family rules.
    pub fn use_audio_gate(&self, gate: Arc<AudioGate>) {
        let _ = self.shared.family.set(gate);
    }

    /// Hands over the listening log: each song a stream plays through is written to its
    /// listener's.
    pub fn use_listening(&self, log: Arc<ListeningLog>) {
        let _ = self.shared.listening.set(log);
    }

    /// The radio streams that may play at once now: 8, or the family's share when Family is on
    /// and that is more.
    pub fn stream_cap(&self) -> usize {
        self.shared.family.get().map_or(8, |gate| gate.radio_cap())
    }

    /// Whether this track is kept out of the stream: an outside cover, karaoke or instrumental take while
    /// Skip covers and instrumentals is on, or a song the listener's clean-only profile keeps out.
    async fn keeps_out(&self, session: &LastFmRadioStreamSession, track: &LastFmRadioTrack) -> bool {
        if self.skips_take(track) {
            return true;
        }
        let (Some(gate), Some(id)) = (self.shared.family.get(), track.resolved_id.as_deref()) else {
            return false;
        };
        gate.radio_keeps_out(
            &session.username,
            id,
            &session.authentication,
            &self.proxy,
            self.shared.metadata.as_ref(),
            self.shared.library.as_ref(),
        )
        .await
    }

    /// An outside cover, karaoke or instrumental take, by its title or album, while Skip covers and instrumentals is
    /// on. A library song always plays.
    fn skips_take(&self, track: &LastFmRadioTrack) -> bool {
        if !self
            .shared
            .settings
            .current()
            .soulseek
            .skip_covers_and_instrumentals
        {
            return false;
        }
        let outside = track
            .resolved_id
            .as_deref()
            .is_some_and(|id| self.shared.library.parse_song_id(id).0);
        outside && skipped_takes::song_names_skipped(&track.title, track.album.as_deref().unwrap_or(""))
    }

    /// The session's station while radio streams are on and its kind is switched on, if it still
    /// has tracks.
    pub fn resolve(&self, session: &LastFmRadioStreamSession) -> Option<LastFmRadioStation> {
        let settings = self.shared.settings.current();
        let last_fm = &settings.last_fm;
        if !last_fm.enable_radio || !last_fm.expose_radio_as_streams {
            return None;
        }
        let station = self
            .shared
            .state
            .find_station(&session.username, &session.station_id)?;
        let switched_off = if station.personalized {
            !last_fm.enable_personalized_stations
        } else {
            !last_fm.enable_discovery_stations
        };
        if switched_off || station.tracks.is_empty() {
            return None;
        }
        Some(station)
    }

    /// Streams the station into `output` until `cancellation_token` is canceled (the client
    /// went away), which ends it with `Ok`. An error means the station is gone, or nothing in it
    /// can be played.
    pub async fn stream<W: AsyncWrite + Unpin + Send>(
        &self,
        session: &LastFmRadioStreamSession,
        output: &mut W,
        cancellation_token: &CancellationToken,
        include_icy_metadata: bool,
    ) -> anyhow::Result<()> {
        let _slot = tokio::select! {
            biased;
            () = cancellation_token.cancelled() => return Err(OperationCanceled.into()),
            slot = STREAM_SLOTS.take(|| self.stream_cap()) => slot,
        };
        if let Some(gate) = self.shared.family.get()
            && let Some(refusal) = gate.radio_refusal(&session.username, &session.token)
        {
            anyhow::bail!("{refusal}");
        }
        let mut stream_output = if include_icy_metadata {
            RadioOutput::Icy(IcyMetadataStream::new(output, DEFAULT_INTERVAL))
        } else {
            RadioOutput::Plain(output)
        };
        let station = self
            .resolve(session)
            .ok_or_else(|| anyhow::anyhow!("Radio station is no longer available"))?;
        let tracks: Vec<&LastFmRadioTrack> = station
            .tracks
            .iter()
            .filter(|track| has_resolved_id(track))
            .collect();
        if tracks.is_empty() {
            anyhow::bail!("Radio station has no playable tracks");
        }
        let ids: Vec<String> = tracks
            .iter()
            .filter_map(|track| track.resolved_id.clone())
            .collect();
        self.shared.queues.register(ids.clone());
        self.shared.queues.note_radio(ids.clone());
        self.prewarm(ids);
        // A published session starts with three complete MP3 segments. Keep those exact tracks
        // even if the recommendation snapshot changes before tune-in; the next replenishment
        // crosses onto the current snapshot cleanly.
        let mut ready: Vec<PreparedRadioTrack> = session
            .ready_pool
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|item| self.shared.cache.is_ready_path(&item.path))
            .collect();
        for cached in self.get_ready_pool(session) {
            if ready.len() < READY_POOL_SIZE && ready.iter().all(|item| item.cache_key != cached.cache_key) {
                ready.push(cached);
            }
        }
        if ready.is_empty() {
            ready = self
                .prepare_ready_pool(session, 1, cancellation_token, true, false)
                .await?;
        }
        if ready.is_empty() {
            anyhow::bail!("Radio station has no cached ready track");
        }
        self.shared
            .sessions
            .attach_ready_pool(&session.token, &ready, None);

        let mut next_index = ready[ready.len() - 1].index + 1;
        let mut queue: VecDeque<PreparedRadioTrack> = ready.into();
        let mut failures = 0usize;
        let mut kept_out = 0usize;
        let mut continuation = Continuation::default();

        while !cancellation_token.is_cancelled() {
            next_index = self
                .continue_from(session, &mut continuation, &queue, next_index)
                .await;
            if queue.is_empty() {
                let emergency = self
                    .prepare_next(
                        session,
                        next_index,
                        &HashSet::new(),
                        &continuation.tracks,
                        cancellation_token,
                        None,
                    )
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("No tracks in this Radio snapshot have a playable source")
                    })?;
                next_index = emergency.index + 1;
                queue.push_back(emergency);
            }

            let prepared = queue.pop_front().expect("the queue was filled above");
            self.shared
                .sessions
                .consume_ready_track(&session.token, &prepared.cache_key);
            // A clean-only profile skips explicit songs and Skip covers and instrumentals outside covers and instrumentals;
            // a station of nothing else ends.
            if self.keeps_out(session, &prepared.track).await {
                kept_out += 1;
                if kept_out >= tracks.len() {
                    anyhow::bail!("No songs in this Radio station are available on this profile");
                }
                continue;
            }
            kept_out = 0;
            continuation.count_played(&prepared.track);
            let reserved: HashSet<String> = queue.iter().map(|item| item.cache_key.clone()).collect();
            let replenishment = self.start_replenishment(
                session,
                next_index,
                reserved,
                continuation.tracks.clone(),
                self.shared.cache.get_profile(&prepared.path),
            );
            self.persist_replenishment(session.token.clone(), replenishment.clone());

            let played = async {
                stream_output.set_track(&prepared.track);
                let mut cached = self.shared.cache.open_read(&prepared.path).await?;
                let mut buffer = vec![0u8; 81920];
                loop {
                    let read = tokio::select! {
                        biased;
                        () = cancellation_token.cancelled() => return Err(OperationCanceled.into()),
                        read = cached.read(&mut buffer) => read?,
                    };
                    if read == 0 {
                        break;
                    }
                    tokio::select! {
                        biased;
                        () = cancellation_token.cancelled() => return Err(OperationCanceled.into()),
                        written = stream_output.write(&buffer[..read]) => written?,
                    }
                }
                stream_output.flush().await?;
                anyhow::Ok(())
            }
            .await;
            let result = match played {
                Ok(()) => {
                    failures = 0;
                    // Only a song that went out whole counts as served by the station.
                    if let Some(recommendations) = &self.shared.recommendations {
                        recommendations.record_station_served(
                            &session.username,
                            &session.station_id,
                            [prepared.track.clone()],
                        );
                    }
                    async {
                        let song = self
                            .resolver
                            .resolve(
                                &prepared.track.artist,
                                &prepared.track.title,
                                prepared.track.duration,
                                &session.authentication,
                                None,
                            )
                            .await;
                        if let Some(song) = song {
                            self.record_completion(session, &prepared.track, &song).await;
                        }

                        let replacement = tokio::select! {
                            biased;
                            () = cancellation_token.cancelled() => return Err(OperationCanceled.into()),
                            replacement = replenishment => replacement.map_err(anyhow::Error::msg)?,
                        };
                        if let Some(replacement) = replacement
                            && queue.iter().all(|item| item.cache_key != replacement.cache_key)
                        {
                            next_index = replacement.index + 1;
                            queue.push_back(replacement);
                        }

                        let upcoming_tracks = self.candidates(session, &continuation.tracks);
                        let upcoming: Vec<String> = (0..8.min(upcoming_tracks.len()))
                            .filter_map(|offset| {
                                upcoming_tracks[(next_index + offset) % upcoming_tracks.len()]
                                    .track
                                    .resolved_id
                                    .clone()
                            })
                            .collect();
                        self.prewarm(upcoming);
                        anyhow::Ok(())
                    }
                    .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                if cancellation_token.is_cancelled() && error.is::<OperationCanceled>() {
                    break;
                }
                failures += 1;
                self.reject_and_refill(session, &prepared.track);
                warn!(
                    "Skipping unavailable continuous Radio track {} - {}: {error:#}",
                    prepared.track.artist, prepared.track.title
                );
                if failures >= tracks.len() {
                    return Err(error.context("No tracks in this Radio snapshot have a playable source"));
                }
            }
        }
        Ok(())
    }

    /// Where the stream goes on from `next_index`: on into the songs the station adds after its
    /// snapshot, with more asked for once [`CONTINUE_BEFORE`] or fewer songs the station has not
    /// served lately are left, or back to the top of the station when nothing new comes. Without
    /// the stations' picker the snapshot plays round and round.
    async fn continue_from(
        &self,
        session: &LastFmRadioStreamSession,
        continuation: &mut Continuation,
        queue: &VecDeque<PreparedRadioTrack>,
        next_index: usize,
    ) -> usize {
        let candidates = self.candidates(session, &continuation.tracks);
        let Some(recommendations) = self.shared.recommendations.clone() else {
            return if candidates.is_empty() {
                next_index
            } else {
                next_index % candidates.len()
            };
        };
        let left_out = recommendations.station_left_out(&session.username, &session.station_id);
        let fresh_after = |candidates: &[RadioCandidate]| {
            candidates
                .iter()
                .skip(next_index)
                .filter(|candidate| !left_out.contains(&candidate.track))
                .count()
        };
        let mut total = candidates.len();
        let mut fresh = fresh_after(&candidates);
        let finished = continuation
            .asking
            .as_ref()
            .is_some_and(|asking| asking.is_finished());
        if finished || (fresh == 0 && continuation.asking.is_some()) {
            if let Some(asking) = continuation.asking.take() {
                continuation.add(asking.await.unwrap_or_default(), &self.shared.registry);
            }
            let candidates = self.candidates(session, &continuation.tracks);
            total = candidates.len();
            fresh = fresh_after(&candidates);
        }
        if continuation.asking.is_none()
            && !continuation.dry
            && continuation.tracks.len() < CONTINUE_AT_MOST
            && fresh <= CONTINUE_BEFORE
        {
            // Everything played, queued or still to come is left out of the ask.
            let exclude: Vec<LastFmRadioTrack> = continuation
                .played
                .iter()
                .cloned()
                .chain(queue.iter().map(|item| item.track.clone()))
                .chain(
                    self.candidates(session, &continuation.tracks)
                        .into_iter()
                        .skip(next_index)
                        .map(|candidate| candidate.track),
                )
                .collect();
            let (username, station_id) = (session.username.clone(), session.station_id.clone());
            continuation.asking = Some(tokio::spawn(async move {
                recommendations
                    .next_for_station(
                        &username,
                        &station_id,
                        &exclude,
                        CONTINUE_WITH,
                        &CancellationToken::new(),
                    )
                    .await
            }));
            if fresh == 0
                && let Some(asking) = continuation.asking.take()
            {
                continuation.add(asking.await.unwrap_or_default(), &self.shared.registry);
                total = self.candidates(session, &continuation.tracks).len();
            }
        }
        if next_index < total {
            return next_index;
        }
        // Nothing new: the station plays from the top, and may go on afresh once it ends again.
        continuation.tracks.clear();
        continuation.dry = false;
        0
    }

    /// Returns up to three current-snapshot tracks that already satisfy the existing radio-cache
    /// retention and size policy. This method never performs I/O beyond checking the cache, so
    /// station listings remain responsive. The scan starts where the tune-in selector says, so
    /// two listens open on different cached tracks once more than three are cached; playback
    /// continues in snapshot order from wherever the pool ends.
    pub fn get_ready_pool(&self, session: &LastFmRadioStreamSession) -> Vec<PreparedRadioTrack> {
        // Rotate among the tracks that are actually cached, not among the whole snapshot: with
        // three cached out of twenty, a start drawn over twenty lands on the first cached track
        // seventeen times in twenty.
        let cached: Vec<PreparedRadioTrack> = self
            .candidates(session, &[])
            .into_iter()
            .filter_map(|candidate| {
                self.shared
                    .cache
                    .get_ready_path(&candidate.key)
                    .map(|path| candidate.prepared(path))
            })
            .collect();
        if cached.is_empty() {
            return cached;
        }
        let start = self.shared.tune_in.start(cached.len()) % cached.len();
        (0..cached.len())
            .take(READY_POOL_SIZE)
            .map(|offset| cached[(start + offset) % cached.len()].clone())
            .collect()
    }

    /// Waits for the minimum publication guarantee: one complete MP3. The remaining runway is
    /// filled in the background after the station URL is returned, so clients that fetch their
    /// Radio list only once still see it.
    pub async fn prepare_for_publication(
        &self,
        session: &LastFmRadioStreamSession,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Vec<PreparedRadioTrack>> {
        let ready = self.get_ready_pool(session);
        if !ready.is_empty() {
            return Ok(ready);
        }
        self.prepare_ready_pool(session, 1, cancellation_token, true, false)
            .await
    }

    /// Warms persisted snapshots without a listener request. Startup has no user credentials by
    /// design, so it uses the registered external preview route; authenticated playback
    /// replenishment remains local-first.
    pub async fn warm_stored_stations(
        &self,
        username: &str,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<RadioWarmupResult> {
        let user = self.shared.state.get_user(username);
        let settings = self.shared.settings.current();
        let stations: Vec<LastFmRadioStation> = user
            .stations
            .into_iter()
            .filter(|station| {
                !station.tracks.is_empty()
                    && if station.personalized {
                        settings.last_fm.enable_personalized_stations
                    } else {
                        settings.last_fm.enable_discovery_stations
                    }
            })
            .collect();
        let warms = stations.iter().map(|station| async move {
            let session = LastFmRadioStreamSession::new(
                format!("warm-{}", uuid::Uuid::new_v4().simple()),
                username,
                station.id.clone(),
                IndexMap::new(),
                Utc::now() + TimeDelta::minutes(30),
            );
            let mut ready = self.get_ready_pool(&session);
            if ready.len() < READY_POOL_SIZE {
                ready = self
                    .prepare_ready_pool(&session, READY_POOL_SIZE, cancellation_token, false, true)
                    .await?;
            }
            anyhow::Ok(ready.len())
        });
        let counts = futures::future::join_all(warms)
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<usize>>>()?;
        Ok(RadioWarmupResult {
            station_count: stations.len(),
            ready_station_count: counts.iter().filter(|count| **count > 0).count(),
            ready_track_count: counts.iter().sum(),
        })
    }

    /// Starts one deduplicated background fill for this station snapshot. Request cancellation
    /// deliberately does not own cache production: a client that refreshes or navigates away
    /// must not discard work needed by its next listing.
    pub fn warm_ready_pool(&self, session: &LastFmRadioStreamSession) {
        let Some(station) = self.resolve(session) else {
            return;
        };
        let key = [
            session.username.clone(),
            station.id.clone(),
            dotnet_ticks(station.changed_utc).to_string(),
            self.shared
                .settings
                .current()
                .last_fm
                .effective_radio_stream_bitrate_kbps()
                .to_string(),
        ]
        .join("|");
        if !self.pool_warmers.lock().insert(key.clone()) {
            return;
        }
        let service = self.clone();
        let session = session.clone();
        tokio::spawn(async move {
            match service
                .prepare_ready_pool(&session, READY_POOL_SIZE, &CancellationToken::new(), true, false)
                .await
            {
                Ok(ready) => info!(
                    "Radio ready pool {}/{} for {}",
                    ready.len(),
                    READY_POOL_SIZE,
                    station.name
                ),
                Err(error) => warn!("Could not warm Radio ready pool for {}: {error:#}", station.name),
            }
            service.pool_warmers.lock().remove(&key);
        });
    }

    async fn prepare_ready_pool(
        &self,
        session: &LastFmRadioStreamSession,
        target: usize,
        cancellation_token: &CancellationToken,
        reject_failures: bool,
        external_only: bool,
    ) -> anyhow::Result<Vec<PreparedRadioTrack>> {
        let mut prepared = Vec::new();
        let mut rejected_any = false;
        for candidate in self.candidates(session, &[]) {
            match self
                .prepare_candidate(session, &candidate, cancellation_token, external_only)
                .await
            {
                Ok(track) => {
                    prepared.push(track);
                    if prepared.len() == target {
                        break;
                    }
                }
                Err(error) if cancellation_token.is_cancelled() && error.is::<OperationCanceled>() => {
                    return Err(error);
                }
                Err(error) => {
                    if reject_failures {
                        rejected_any |=
                            self.shared
                                .state
                                .reject_track(&session.username, &candidate.track, None)
                                > 0;
                        warn!(
                            "Could not prepare Radio pool track {} - {}; trying the next track: {error:#}",
                            candidate.track.artist, candidate.track.title
                        );
                    } else {
                        debug!(
                            "Startup Radio warm could not prepare {} - {}: {error:#}",
                            candidate.track.artist, candidate.track.title
                        );
                    }
                }
            }
        }
        if reject_failures && rejected_any {
            self.shared.refresh_queue.enqueue(&session.username, None);
        }
        Ok(prepared)
    }

    /// The replenishment of the pool, started now on its own task with no caller's token: a
    /// listener skipping ahead must not throw away the next segment.
    fn start_replenishment(
        &self,
        session: &LastFmRadioStreamSession,
        start_index: usize,
        reserved: HashSet<String>,
        continuation: Vec<LastFmRadioTrack>,
        current: Option<RadioAudioProfile>,
    ) -> Replenishment {
        let service = self.clone();
        let session = session.clone();
        let task = tokio::spawn(async move {
            service
                .prepare_next(
                    &session,
                    start_index,
                    &reserved,
                    &continuation,
                    &CancellationToken::new(),
                    current,
                )
                .await
                .map_err(|error| format!("{error:#}"))
        });
        async move { task.await.map_err(|error| error.to_string())? }
            .boxed()
            .shared()
    }

    /// The next playable track from `start_index` of the snapshot and, after it, `continuation`.
    async fn prepare_next(
        &self,
        session: &LastFmRadioStreamSession,
        start_index: usize,
        reserved: &HashSet<String>,
        continuation: &[LastFmRadioTrack],
        cancellation_token: &CancellationToken,
        current: Option<RadioAudioProfile>,
    ) -> anyhow::Result<Option<PreparedRadioTrack>> {
        let candidates = self.candidates(session, continuation);
        if candidates.is_empty() {
            return Ok(None);
        }
        let mut start_index = start_index;
        // A song the station served lately, through getStationSongs or a stream, is passed over
        // before anything is prepared, unless every song left to prepare is one of those.
        let left_out =
            self.shared.recommendations.as_ref().map(|recommendations| {
                recommendations.station_left_out(&session.username, &session.station_id)
            });
        let any_fresh = candidates.iter().any(|candidate| {
            !reserved.contains(&candidate.key)
                && left_out
                    .as_ref()
                    .is_none_or(|left_out| !left_out.contains(&candidate.track))
        });
        let passed_over = |candidate: &RadioCandidate| {
            reserved.contains(&candidate.key)
                || (any_fresh
                    && left_out
                        .as_ref()
                        .is_some_and(|left_out| left_out.contains(&candidate.track)))
        };

        // Flow: look at the next few unreserved tracks and, where their profiles are known, lead
        // with the one that follows the current track most smoothly. The rest of the window is
        // not skipped, only deferred: the scan below starts at the chosen one and wraps, so an
        // unchosen track is still next in line.
        let window: Vec<&RadioCandidate> = (0..candidates.len())
            .map(|offset| &candidates[(start_index + offset) % candidates.len()])
            .filter(|candidate| !passed_over(candidate))
            .take(FLOW_WINDOW)
            .collect();
        let profiles: Vec<Option<RadioAudioProfile>> = window
            .iter()
            .map(|candidate| {
                self.shared
                    .cache
                    .get_ready_path(&candidate.key)
                    .and_then(|path| self.shared.cache.get_profile(&path))
            })
            .collect();
        let lead = choose_by_flow(current.as_ref(), &profiles);
        if lead > 0 {
            debug!(
                "Radio flow chose {} - {} over the next in snapshot order",
                window[lead].track.artist, window[lead].track.title
            );
            start_index = window[lead].index;
        }

        // The songs served lately are tried last, when none of the others could be prepared.
        let order = (0..candidates.len())
            .map(|offset| &candidates[(start_index + offset) % candidates.len()])
            .filter(|candidate| !passed_over(candidate))
            .chain(
                (0..candidates.len())
                    .map(|offset| &candidates[(start_index + offset) % candidates.len()])
                    .filter(|candidate| {
                        any_fresh && !reserved.contains(&candidate.key) && passed_over(candidate)
                    }),
            );
        for candidate in order {
            match self
                .prepare_candidate(session, candidate, cancellation_token, false)
                .await
            {
                Ok(prepared) => return Ok(Some(prepared)),
                Err(error) if cancellation_token.is_cancelled() && error.is::<OperationCanceled>() => {
                    return Err(error);
                }
                Err(error) => {
                    self.reject_and_refill(session, &candidate.track);
                    warn!(
                        "Could not replenish Radio pool with {} - {}: {error:#}",
                        candidate.track.artist, candidate.track.title
                    );
                }
            }
        }
        Ok(None)
    }

    async fn prepare_candidate(
        &self,
        session: &LastFmRadioStreamSession,
        candidate: &RadioCandidate,
        cancellation_token: &CancellationToken,
        external_only: bool,
    ) -> anyhow::Result<PreparedRadioTrack> {
        let settings = self.shared.settings.current();
        let bitrate_kbps = settings.last_fm.effective_radio_stream_bitrate_kbps();
        let target = settings.last_fm.effective_radio_loudness_target();
        let profile: Arc<Mutex<Option<RadioAudioProfile>>> = Arc::new(Mutex::new(None));
        let producer: Producer = {
            let service = self.clone();
            let track = candidate.track.clone();
            let authentication = session.authentication.clone();
            let profile = Arc::clone(&profile);
            Box::new(move |mut output, token| {
                async move {
                    let opened = if external_only {
                        service.open_external_track(&track, &token).await?
                    } else {
                        service.open_track(&track, &authentication, &token).await?
                    };
                    let Some(source) = opened else {
                        anyhow::bail!("No playable source");
                    };
                    let measured = service
                        .shared
                        .transcoder
                        .transcode_to_mp3(source.audio_stream, &mut output, bitrate_kbps, target, &token)
                        .await?;
                    *profile.lock() = measured;
                    Ok(output)
                }
                .boxed()
            })
        };
        let path = self
            .shared
            .cache
            .get_or_create(&candidate.key, producer, cancellation_token)
            .await?;
        // Only the producer holds a profile; joiners of the same single-flight get the path and
        // read the sidecar the producer writes here. The tags ride along so the flow picker can
        // weigh what a track is next to how it sounds.
        let measured = profile.lock().take();
        if let Some(measured) = measured {
            let tags = self.tags_for(&candidate.track, cancellation_token).await?;
            self.shared.cache.save_profile(
                &path,
                &measured.with_kinship(candidate.track.genre.as_deref(), Some(tags)),
            );
        }
        Ok(candidate.prepared(path))
    }

    /// Last.fm's top tags for the track, the artist's when the track has none. Both are cached
    /// by [`LastFmService`]; a miss is an empty list, never a failure. Only the caller's
    /// cancellation fails it.
    async fn tags_for(
        &self,
        track: &LastFmRadioTrack,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Vec<String>> {
        let Some(last_fm) = self
            .shared
            .last_fm
            .as_ref()
            .filter(|last_fm| last_fm.has_api_key())
        else {
            return Ok(Vec::new());
        };
        let lookup = async {
            let mut tags = last_fm
                .get_track_top_tags(&track.artist, &track.title, 10)
                .await?;
            if tags.is_empty() {
                tags = last_fm.get_artist_top_tags(&track.artist, 10).await?;
            }
            Ok::<_, octo_core::json::element::ElementError>(kinship_tags(&tags, &track.artist))
        };
        let answer = tokio::select! {
            biased;
            () = cancellation_token.cancelled() => return Err(OperationCanceled.into()),
            answer = lookup => answer,
        };
        Ok(answer.unwrap_or_else(|error| {
            debug!("No Last.fm tags for {} - {}: {error}", track.artist, track.title);
            Vec::new()
        }))
    }

    /// The station's snapshot tracks that can play, then the `continuation` after them, each
    /// with its place in that order.
    fn candidates(
        &self,
        session: &LastFmRadioStreamSession,
        continuation: &[LastFmRadioTrack],
    ) -> Vec<RadioCandidate> {
        let Some(station) = self.resolve(session) else {
            return Vec::new();
        };
        let bitrate_kbps = self
            .shared
            .settings
            .current()
            .last_fm
            .effective_radio_stream_bitrate_kbps();
        // The cache key is the resolved source, not the station: a track that survives a
        // refresh, or sits in two of a listener's stations, is transcoded once.
        station
            .tracks
            .into_iter()
            .chain(continuation.iter().cloned())
            .filter(has_resolved_id)
            .enumerate()
            .map(|(index, track)| RadioCandidate {
                key: self.shared.cache.key(
                    &session.username,
                    "",
                    track.resolved_id.as_deref().unwrap_or_default(),
                    bitrate_kbps,
                ),
                track,
                index,
            })
            .collect()
    }

    fn persist_replenishment(&self, token: String, replenishment: Replenishment) {
        let sessions = Arc::clone(&self.shared.sessions);
        tokio::spawn(async move {
            match replenishment.await {
                Ok(Some(track)) => sessions.append_ready_track(&token, track, READY_POOL_SIZE),
                Ok(None) => {}
                Err(error) => debug!("Radio pool replenishment ended without a ready track: {error}"),
            }
        });
    }

    fn reject_and_refill(&self, session: &LastFmRadioStreamSession, track: &LastFmRadioTrack) {
        if self.shared.state.reject_track(&session.username, track, None) == 0 {
            return;
        }
        self.shared.refresh_queue.enqueue(&session.username, None);
    }

    /// Prewarms the YouTube ids of the first 8 songs in the background, without waiting.
    fn prewarm(&self, ids: Vec<String>) {
        let metadata = Arc::clone(&self.shared.metadata);
        tokio::spawn(async move { metadata.prewarm_you_tube_ids_for_song_ids(&ids, 8).await });
    }

    async fn open_track(
        &self,
        track: &LastFmRadioTrack,
        authentication: &IndexMap<String, String>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        let Some(song) = self
            .resolver
            .resolve(&track.artist, &track.title, track.duration, authentication, None)
            .await
        else {
            return Ok(None);
        };
        let id = song.id.clone();
        let (external, provider, external_id) = self.shared.library.parse_song_id(&id);
        if external {
            let provider = provider
                .or_else(|| song.external_provider.clone())
                .or_else(|| track.external_provider.clone())
                .unwrap_or_else(|| "lastfm".to_string());
            let external_id = external_id.or_else(|| song.external_id.clone()).unwrap_or(id);
            return self
                .shared
                .downloads
                .get_direct_stream(&provider, &external_id, None, None, cancellation_token)
                .await;
        }

        let parameters = with_parameters(authentication, &[("id", &id), ("format", "raw")]);
        Ok(self.proxy.open_audio_stream(&parameters).await?)
    }

    async fn open_external_track(
        &self,
        track: &LastFmRadioTrack,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        let Some(resolved_id) = track.resolved_id.as_deref().filter(|id| !dotnet::is_blank(id)) else {
            return Ok(None);
        };
        let provider = track
            .external_provider
            .clone()
            .unwrap_or_else(|| SoulseekMetadataService::PROVIDER_NAME.to_string());
        self.shared
            .downloads
            .get_direct_stream(&provider, resolved_id, None, None, cancellation_token)
            .await
    }

    async fn record_completion(
        &self,
        session: &LastFmRadioStreamSession,
        track: &LastFmRadioTrack,
        song: &Song,
    ) {
        let id = song.id.clone();
        if let Some(log) = self.shared.listening.get().filter(|log| log.recording()) {
            let seconds = i64::from(track.duration.or(song.duration).unwrap_or(0).max(0));
            let play = ListeningPlay {
                song_id: id.clone(),
                cover_id: id.clone(),
                artist: track.artist.clone(),
                title: track.title.clone(),
                album: track.album.clone().unwrap_or_default(),
                seconds,
                is_local: song.is_local,
                app: "Internet radio".to_string(),
                played_at: Utc::now() - chrono::TimeDelta::seconds(seconds),
                source: "internet-radio".to_string(),
                ..Default::default()
            };
            let genres: Vec<String> = track
                .genre
                .clone()
                .or_else(|| song.genre.clone())
                .into_iter()
                .collect();
            if let Some(ended) = log.finished(&session.username, play, genres) {
                match &self.shared.last_fm {
                    Some(last_fm) => crate::services::listening::write_settled(
                        Arc::clone(log),
                        Arc::clone(last_fm),
                        vec![ended],
                    ),
                    None => {
                        let log = Arc::clone(log);
                        let genre = ended.genres.first().cloned();
                        tokio::task::spawn_blocking(move || log.write(ended, genre));
                    }
                }
            }
        }
        self.shared.state.record_play(
            &session.username,
            LastFmRadioPlay {
                song_id: id.clone(),
                artist: track.artist.clone(),
                title: track.title.clone(),
                album: track.album.clone(),
                genre: track.genre.clone().or_else(|| song.genre.clone()),
                duration: track.duration.or(song.duration),
                is_local: song.is_local,
                source: "internet-radio".to_string(),
                played_at_utc: Utc::now(),
                ..Default::default()
            },
        );
        if !song.is_local {
            // A local track's play is scrobbled by Navidrome below; an external one would
            // otherwise vanish from the listener's history. Not awaited: the stream is mid-song
            // and a listen is a record, not a step.
            if let Some(listen_brainz) = &self.shared.listen_brainz {
                let listen_brainz = Arc::clone(listen_brainz);
                let username = session.username.clone();
                let track = track.clone();
                let duration = track.duration.or(song.duration);
                tokio::spawn(async move {
                    listen_brainz
                        .submit_listen(
                            &username,
                            &track.artist,
                            &track.title,
                            track.album.as_deref(),
                            duration,
                            Utc::now(),
                        )
                        .await
                });
            }
            // Last.fm dates a scrobble from when the song started, and this one just ended. The
            // station picked it, not the listener, and Last.fm is told as much.
            let duration = track.duration.or(song.duration);
            if let Some(scrobbles) = &self.shared.last_fm_scrobbles {
                scrobbles.scrobble(
                    &session.username,
                    LastFmTrack::new(
                        track.artist.clone(),
                        track.title.clone(),
                        track.album.as_deref(),
                        duration,
                    ),
                    Utc::now() - TimeDelta::seconds(i64::from(duration.unwrap_or(0))),
                    false,
                );
            }
            return;
        }
        let time = Utc::now().timestamp_millis().to_string();
        let parameters = with_parameters(
            &session.authentication,
            &[("id", &id), ("submission", "true"), ("time", &time)],
        );
        self.proxy.relay_safe("rest/scrobble", &parameters).await;
    }
}

/// A track's key, as the stations match songs.
fn track_key(track: &LastFmRadioTrack) -> String {
    octo_core::last_fm::last_fm_radio_seed_normalizer::track_key(&track.artist, &track.title)
}

/// A track's key folded for comparing ignoring case.
fn folded_key(track: &LastFmRadioTrack) -> String {
    dotnet::ordinal_ignore_case_key(&track_key(track))
}

fn has_resolved_id(track: &LastFmRadioTrack) -> bool {
    track
        .resolved_id
        .as_deref()
        .is_some_and(|id| !dotnet::is_blank(id))
}

/// The session's credentials as a case-insensitive dictionary, with `extra` set over them.
fn with_parameters(
    authentication: &IndexMap<String, String>,
    extra: &[(&str, &str)],
) -> IndexMap<String, String> {
    let mut parameters: IndexMap<String, String> = IndexMap::new();
    for (key, value) in authentication {
        set_ignore_case(&mut parameters, key, value);
    }
    for (key, value) in extra {
        set_ignore_case(&mut parameters, key, value);
    }
    parameters
}

fn set_ignore_case(parameters: &mut IndexMap<String, String>, key: &str, value: &str) {
    match parameters
        .iter_mut()
        .find(|(existing, _)| dotnet::eq_ignore_case(existing, key))
    {
        Some((_, existing)) => *existing = value.to_string(),
        None => {
            parameters.insert(key.to_string(), value.to_string());
        }
    }
}

#[cfg(test)]
#[path = "last_fm_radio_stream_service_tests.rs"]
mod tests;

#[cfg(test)]
mod slot_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn a_stream_waits_for_a_free_slot() {
        let slots = Arc::new(StreamSlots::default());
        let cap = Arc::new(AtomicUsize::new(1));
        let first = slots.take(|| cap.load(Ordering::SeqCst)).await;
        assert_eq!(slots.playing(), 1);
        let waiting = {
            let (slots, cap) = (Arc::clone(&slots), Arc::clone(&cap));
            tokio::spawn(async move {
                let _slot = slots.take(|| cap.load(Ordering::SeqCst)).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiting.is_finished(), "the second stream waits");
        drop(first);
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("it starts once the first ends")
            .expect("joins");
        assert_eq!(slots.playing(), 0);
    }

    #[tokio::test]
    async fn a_larger_family_lets_more_streams_play() {
        let slots = StreamSlots::default();
        let mut held = Vec::new();
        for _ in 0..12 {
            held.push(
                tokio::time::timeout(Duration::from_secs(1), slots.take(|| 12))
                    .await
                    .expect("a slot"),
            );
        }
        assert_eq!(slots.playing(), 12);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), slots.take(|| 12))
                .await
                .is_err(),
            "the thirteenth waits"
        );
    }
}
