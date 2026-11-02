//! Builds canonical station snapshots from bounded listening signals, asking Last.fm for the
//! candidates. The weighing, the draw and the shaping are
//! `octo_core::last_fm::last_fm_radio_recommendation_service`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use octo_core::common::{Clock, dotnet};
use octo_core::last_fm::SimilarTrack;
use octo_core::last_fm::last_fm_radio_recommendation_service::{
    ARTIST_RADIO_NEIGHBOURS, Candidate, FRONTIER_ARTIST_CAP, FRONTIER_ARTISTS, FRONTIER_FEWEST_SONGS,
    FRONTIER_NEIGHBOURS, FRONTIER_SEEDS, FRONTIER_TRACKS_EACH, FrontierScores, HEARTED_SEED_AFFINITY,
    IgnoreCaseKeys, NEIGHBOUR_ARTIST_AFFINITY, Randomizer, SECOND_STEP_SEEDS, SECOND_STEP_WEIGHT,
    SEED_RECENCY_DECAY, SEEDS_AT_ONCE, artist_cap, artist_key, blocked_tracks, create, default_randomizer,
    definition_version, discount_known, is_learned, key, known_artists, ranked, score_artists,
    score_local_tags, seed_score, shape, shape_walk, shape_with_reserve, still_unavailable,
    suppress_station_overlap, title, to_candidate, unknown_neighbours_first,
};
use octo_core::last_fm::last_fm_radio_seed_normalizer as seed_normalizer;
use octo_core::last_fm::last_fm_radio_state_store::user_key;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::models::domain::Song;
use octo_core::models::radio::{
    LastFmRadioPlay, LastFmRadioStation, LastFmRadioStationKind, LastFmRadioTrack, LastFmRadioUserState,
};
use octo_core::radio::RadioSeed;
use octo_core::radio::radio_blend;
use octo_core::radio::radio_provider::LAST_FM;
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::last_fm_radio_state_store::LastFmRadioStateStore;
use super::lookup_trouble;
use super::{LastFmService, OperationCanceled};
use crate::services::radio::RadioSourceSet;

/// Provider expansion has a hard fan-out and deadline. Partial results are useful.
const PROVIDER_BUDGET: Duration = Duration::from_secs(25);

/// The similar-artist walk of Discovery Radio has this long of its own, within the build's budget.
const DISCOVERY_WALK_BUDGET: Duration = Duration::from_secs(8);

/// How many songs each source is asked for when a station goes on past its snapshot.
const CONTINUATION_CANDIDATES: i32 = 100;

/// How many stations' new songs are kept at once; the one walked on least recently goes first.
const CONTINUATIONS_KEPT: usize = 64;

/// How many new songs one station keeps; the oldest go first.
const CONTINUATION_SONGS: usize = 500;

/// How many of the newest songs left out seed a walk on from songs like them.
const CONTINUATION_SEEDS: usize = 3;

/// How many served songs are remembered for one listener; the oldest go first.
const SERVED_KEPT: usize = 5000;

/// How many listeners' served songs are remembered at once; the one served least recently goes
/// first.
const SERVED_LISTENERS: usize = 64;

/// How long a song a station served stays out of that station's songs.
pub const STATION_MEMORY: TimeDelta = TimeDelta::hours(24);

/// How many served songs one station remembers; the oldest go first.
pub const STATION_MEMORY_SONGS: usize = 1000;

/// How many stations' served songs are remembered at once; the one served least recently goes
/// first.
const STATIONS_REMEMBERED: usize = 256;

/// A song a station served, and when.
#[derive(Debug, Clone)]
struct StationServed {
    /// The song's key, folded for comparing.
    key: String,
    track: LastFmRadioTrack,
    /// The id the song went out as, when it is not the track's own.
    served_as: Option<String>,
    at: chrono::DateTime<Utc>,
}

/// Songs left out of a pick: by artist and title, and by the ids they were served as.
#[derive(Default)]
pub struct LeftOut {
    keys: IgnoreCaseKeys,
    ids: HashSet<String>,
}

impl LeftOut {
    /// The tracks, each with the id it went out as when that is not its own.
    fn of<'a>(tracks: impl IntoIterator<Item = (&'a LastFmRadioTrack, Option<&'a str>)>) -> Self {
        let mut left_out = LeftOut::default();
        for (track, served_as) in tracks {
            left_out.keys.insert(&track_key(track));
            left_out.ids.extend(
                track
                    .resolved_id
                    .as_deref()
                    .into_iter()
                    .chain(served_as)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string),
            );
        }
        left_out
    }

    /// Whether `track` is one of these songs: the same artist and title, or the same id.
    pub fn contains(&self, track: &LastFmRadioTrack) -> bool {
        self.keys.contains(&track_key(track))
            || track.resolved_id.as_ref().is_some_and(|id| self.ids.contains(id))
    }
}

/// Ends a walk on's place among those under way, however the walk ends.
struct WalkDone {
    walks: Arc<Mutex<HashMap<String, Walk>>>,
    key: String,
}

impl Drop for WalkDone {
    fn drop(&mut self) {
        self.walks.lock().remove(&self.key);
    }
}

/// The caller's token with a 25 s deadline on top: either ends the build.
struct Budget<'a> {
    caller: &'a CancellationToken,
    deadline: Instant,
}

impl<'a> Budget<'a> {
    /// This budget cut to at most `limit` from now: the same caller, the earlier deadline.
    fn within(&self, limit: Duration) -> Budget<'a> {
        Budget {
            caller: self.caller,
            deadline: self.deadline.min(Instant::now() + limit),
        }
    }

    /// The call, or [`OperationCanceled`] once the caller cancels or the deadline passes. (A
    /// canceled call is dropped, which cancels its request.) A call cut off this way is a lookup
    /// that did not answer.
    async fn run<F: Future>(&self, call: F) -> Result<F::Output, OperationCanceled> {
        let output = tokio::select! {
            biased;
            () = self.caller.cancelled() => Err(OperationCanceled),
            () = tokio::time::sleep_until(self.deadline) => Err(OperationCanceled),
            output = call => Ok(output),
        };
        if output.is_err() {
            lookup_trouble::note_core();
        }
        output
    }
}

/// A walk on past a station's snapshot under way, which every ask for the same station joins.
type Walk = Shared<BoxFuture<'static, ()>>;

/// What a full build made, and what it carried over from the listener's stations.
#[derive(Debug, Clone, Default)]
pub struct BuiltStations {
    /// The stations to save, in build order: the ones made and the ones carried over, kept apart
    /// from each other by the overlap pass.
    pub stations: Vec<LastFmRadioStation>,
    /// The same stations before the overlap pass, for a retry that puts some of them back
    /// among saved stations they were not built with.
    pub unshared: Vec<LastFmRadioStation>,
    /// The keys of the stations carried over.
    pub carried: Vec<String>,
    /// Whether a station the build was asked for got no answer and was not made, carried over
    /// or not.
    pub unanswered: bool,
}

/// Carries over the first station the listener has that `matches`, has songs and is not in the
/// build yet: it goes next in the build, in the place the station it stands in for would have.
/// Answers whether one was carried.
fn carry(
    previous: &[LastFmRadioStation],
    stations: &mut Vec<LastFmRadioStation>,
    carried: &mut Vec<String>,
    matches: impl Fn(&LastFmRadioStation) -> bool,
) -> bool {
    let Some(station) = previous.iter().find(|station| {
        !station.tracks.is_empty()
            && matches(station)
            && !stations
                .iter()
                .any(|built| dotnet::eq_ignore_case(&built.key, &station.key))
    }) else {
        return false;
    };
    carried.push(station.key.clone());
    stations.push(station.clone());
    true
}

/// Builds canonical station snapshots from bounded listening signals. Which candidates make the
/// cut is a weighted draw rather than a fixed top-N, so two refreshes of the same profile give
/// two different stations; the previous snapshot is demoted so a refresh rotates the station
/// instead of restating it. A clone shares everything with the original, so a walk on past a
/// snapshot can go on in the background.
#[derive(Clone)]
pub struct LastFmRadioRecommendationService {
    last_fm: Arc<LastFmService>,
    state: Arc<LastFmRadioStateStore>,
    /// The Last.fm settings.
    settings: Arc<SettingsStore>,
    /// Source of the draw. Tests replace it with a seeded one so a build repeats exactly.
    randomizer: Arc<Mutex<Randomizer>>,
    /// Every radio source; None keeps stations on Last.fm alone.
    sources: Option<Arc<RadioSourceSet>>,
    /// The new songs each station went on with, by listener, station and snapshot, the station
    /// walked on most recently last.
    continuations: Arc<Mutex<IndexMap<String, Vec<LastFmRadioTrack>>>>,
    /// The walks on under way, by the same key as `continuations`.
    walks: Arc<Mutex<HashMap<String, Walk>>>,
    /// The songs stations served lately, by listener (the one served least recently first) and
    /// then by song id (the oldest first).
    served: Arc<Mutex<IndexMap<String, IndexMap<String, LastFmRadioTrack>>>>,
    /// What each station served a listener through getStationSongs and its stream, by listener
    /// and station, the station served least recently first; each list oldest first.
    station_history: Arc<Mutex<IndexMap<String, VecDeque<StationServed>>>>,
    /// The clock the station history is kept by. Tests move it.
    clock: Arc<Mutex<Clock>>,
}

impl LastFmRadioRecommendationService {
    pub fn new(
        last_fm: Arc<LastFmService>,
        state: Arc<LastFmRadioStateStore>,
        settings: Arc<SettingsStore>,
        sources: Option<Arc<RadioSourceSet>>,
    ) -> Self {
        LastFmRadioRecommendationService {
            last_fm,
            state,
            settings,
            randomizer: Arc::new(Mutex::new(default_randomizer())),
            sources,
            continuations: Arc::default(),
            walks: Arc::default(),
            served: Arc::default(),
            station_history: Arc::default(),
            clock: Arc::new(Mutex::new(Clock::system())),
        }
    }

    /// `Randomizer = ...`.
    pub fn set_randomizer(&self, randomizer: Randomizer) {
        *self.randomizer.lock() = randomizer;
    }

    /// The clock the station history is kept by.
    pub fn set_clock(&self, clock: Clock) {
        *self.clock.lock() = clock;
    }

    /// The listener's stations, built now. An error is an answer Last.fm's reader refused, or
    /// [`OperationCanceled`] when the caller canceled or the budget ran out while an artist
    /// station was being built.
    pub async fn build(
        &self,
        username: &str,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Vec<LastFmRadioStation>> {
        Ok(self.build_stations(username, cancellation_token).await?.stations)
    }

    /// [`Self::build`], saying which stations it carried over. A station the listener has whose
    /// own lookups got no answer this time (see [`lookup_trouble`]) keeps its songs and takes
    /// the place the new one would have; one whose lookups answered with too little goes, as
    /// before.
    pub async fn build_stations(
        &self,
        username: &str,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<BuiltStations> {
        let snapshot = self.settings.current();
        let settings = &snapshot.last_fm;
        if !settings.enable_radio {
            return Ok(BuiltStations::default());
        }
        let user = self.state.get_user(username);
        let now = Utc::now();
        let randomizer = self.randomizer.lock().clone();
        let mut random = randomizer();
        // Grouped by key ignoring case: the first station with a key names its snapshot.
        let mut previous_snapshots: Vec<(String, IgnoreCaseKeys)> = Vec::new();
        for station in &user.stations {
            if !previous_snapshots
                .iter()
                .any(|(key, _)| dotnet::eq_ignore_case(key, &station.key))
            {
                let keys = station
                    .tracks
                    .iter()
                    .map(|track| seed_normalizer::track_key(&track.artist, &track.title))
                    .collect();
                previous_snapshots.push((station.key.clone(), keys));
            }
        }
        let empty = IgnoreCaseKeys::new();
        let previous = |station_key: &str| previous_snapshot(&previous_snapshots, &empty, station_key);
        let Signals {
            plays,
            unavailable,
            banned,
            disliked,
        } = Signals::of(&user);
        let known = known_artists(&plays);
        let refill_headroom = (unavailable.len() as i32).min(settings.effective_radio_track_count());
        let candidate_target = 100.min(settings.effective_radio_track_count() + refill_headroom + 10);
        let artist_scores = score_artists(&plays, now);
        let track_seeds = track_seeds(&plays, now);
        let mut tags = score_local_tags(&plays);

        // Provider expansion has a hard fan-out and deadline. Partial results are useful.
        let budget = Budget {
            caller: cancellation_token,
            deadline: Instant::now() + PROVIDER_BUDGET,
        };
        // The genre ranking: whether it got every answer decides whether genre stations the
        // listener has may stand in for ones it could not rank.
        let (ranked, ranking_trouble) = lookup_trouble::watch(async {
            for (artist, _) in artist_scores.iter().take(5) {
                match budget.run(self.last_fm.get_artist_top_tags(artist, 6)).await {
                    Ok(top_tags) => {
                        for tag in top_tags? {
                            tags.add(&tag, 1.0);
                        }
                    }
                    Err(canceled) if cancellation_token.is_cancelled() => {
                        return Err(anyhow::Error::from(canceled));
                    }
                    Err(_) => break,
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .await;
        ranked?;

        // What the listener has now: a station the build is asked for but gets no answer for
        // keeps its place with these songs.
        let previous_stations = &user.stations;
        let mut stations: Vec<LastFmRadioStation> = Vec::new();
        let mut carried: Vec<String> = Vec::new();
        let mut unanswered = false;
        if settings.enable_personalized_stations {
            let learned = is_learned(&plays, settings);
            let mix_key = if learned { "your-mix" } else { "starter" };
            if settings.enable_your_mix {
                let (mut mix_candidates, mix_trouble) = lookup_trouble::watch(self.tracks_from_seeds(
                    username,
                    &track_seeds[..track_seeds.len().min(6)],
                    12,
                    &budget,
                ))
                .await;
                // Nothing came back because nothing answered: the station would be the
                // listener's own plays alone.
                let fell_short = mix_candidates.is_empty() && mix_trouble.any();
                let familiar: Vec<Candidate> = plays
                    .iter()
                    .enumerate()
                    .map(|(rank, play)| to_candidate(play, rank))
                    .collect();
                // The familiar share of the mix is a quota the walk enforces, so both halves are
                // drawn over their whole pools rather than the top of each list.
                let familiar_quota = if mix_candidates.is_empty() {
                    None
                } else {
                    Some(familiar_quota(settings))
                };
                if mix_candidates.is_empty() {
                    mix_candidates.extend(familiar.iter().cloned());
                }
                let seeds: Vec<&str> = track_seeds.iter().map(|seed| seed.artist.as_str()).collect();
                let tracks = shape(
                    familiar.into_iter().chain(mix_candidates).collect(),
                    &plays,
                    settings,
                    &unavailable,
                    &mut *random,
                    previous(mix_key),
                    artist_cap(settings, LastFmRadioStationKind::YourMix),
                    false,
                    familiar_quota,
                    Some(&banned),
                    Some(&disliked),
                );
                let made = create(
                    username,
                    mix_key,
                    if learned { "Your Radio" } else { "Starter Radio" },
                    if learned {
                        LastFmRadioStationKind::YourMix
                    } else {
                        LastFmRadioStationKind::Starter
                    },
                    true,
                    &seeds,
                    tracks,
                    1,
                    Utc::now(),
                );
                if fell_short {
                    unanswered = true;
                }
                let kept = fell_short
                    && carry(previous_stations, &mut stations, &mut carried, |station| {
                        matches!(
                            station.kind,
                            LastFmRadioStationKind::Starter | LastFmRadioStationKind::YourMix
                        ) && (dotnet::eq_ignore_case(&station.key, "your-mix")
                            || dotnet::eq_ignore_case(&station.key, "starter"))
                    });
                if !kept {
                    stations.push(made);
                }
            }

            if learned {
                let top_tags = tags.top_by_score_then_name(3);
                // The artist and genre stations' candidates are asked for first, so a slow walk
                // for Discovery Radio never spends their time; the stations are still drawn and
                // listed in the same order as before.
                let mut artist_candidates: Vec<(String, Vec<Candidate>, bool)> = Vec::new();
                for (artist, _) in artist_scores
                    .iter()
                    .take(settings.effective_artist_station_count().max(0) as usize)
                {
                    let (candidates, trouble) = lookup_trouble::watch(self.tracks_from_artist(
                        username,
                        artist,
                        candidate_target,
                        &known,
                        &budget,
                    ))
                    .await;
                    artist_candidates.push((artist.clone(), candidates?, trouble.core));
                }
                let mut genre_candidates: Vec<(String, Vec<Candidate>, bool)> = Vec::new();
                for tag in tags.top_by_score(settings.effective_genre_station_count().max(0) as usize) {
                    // A genre station is for finding: the listener's own artists keep a third of
                    // their weight, so the genre's other names lead.
                    let (candidates, trouble) = lookup_trouble::watch(self.tracks_from_tags(
                        username,
                        std::slice::from_ref(&tag),
                        candidate_target,
                        &budget,
                    ))
                    .await;
                    let candidates = discount_known(candidates?, &known);
                    genre_candidates.push((tag, candidates, trouble.core));
                }

                if settings.enable_discovery_mix {
                    // New artists first, found through the ones the listener plays, within the
                    // walk's own few seconds; the tag charts only when the walk finds too few
                    // songs.
                    let (found, discovery_trouble) = lookup_trouble::watch(async {
                        let walk_budget = budget.within(DISCOVERY_WALK_BUDGET);
                        let mut discovery = self
                            .tracks_from_frontier(&artist_scores, &known, &disliked, &walk_budget)
                            .await;
                        let mut discovery_cap = FRONTIER_ARTIST_CAP;
                        if discovery.len() < FRONTIER_FEWEST_SONGS && !top_tags.is_empty() {
                            discovery = self
                                .tracks_from_tags(username, &top_tags, candidate_target, &budget)
                                .await?;
                            discovery_cap = artist_cap(settings, LastFmRadioStationKind::Discovery);
                        }
                        Ok::<_, anyhow::Error>((discovery, discovery_cap))
                    })
                    .await;
                    let (discovery, discovery_cap) = found?;
                    if discovery.len() < 5 && discovery_trouble.core {
                        unanswered = true;
                        carry(previous_stations, &mut stations, &mut carried, |station| {
                            station.kind == LastFmRadioStationKind::Discovery
                        });
                    }
                    if discovery.len() >= 5 {
                        // The reserve stands in for library songs past the Discovery balance's
                        // share, which only resolving the tracks can tell.
                        let shaped = shape_with_reserve(
                            discovery,
                            &plays,
                            settings,
                            &unavailable,
                            &mut *random,
                            previous("discovery"),
                            discovery_cap,
                            true,
                            Some(&banned),
                            Some(&disliked),
                        );
                        let mut station = create(
                            username,
                            "discovery",
                            "Discovery Radio",
                            LastFmRadioStationKind::Discovery,
                            true,
                            &top_tags,
                            shaped.tracks,
                            1,
                            Utc::now(),
                        );
                        station.reserve = shaped.reserve;
                        stations.push(station);
                    }
                }

                for (artist, candidates, troubled) in artist_candidates {
                    let station_key = format!("artist-{}", key(&artist));
                    if candidates.len() < 5 && troubled {
                        unanswered = true;
                        carry(previous_stations, &mut stations, &mut carried, |station| {
                            station.kind == LastFmRadioStationKind::Artist
                                && dotnet::eq_ignore_case(&station.key, &station_key)
                        });
                    }
                    if candidates.len() >= 5 {
                        let tracks = shape(
                            candidates,
                            &plays,
                            settings,
                            &unavailable,
                            &mut *random,
                            previous(&station_key),
                            artist_cap(settings, LastFmRadioStationKind::Artist),
                            false,
                            None,
                            Some(&banned),
                            Some(&disliked),
                        );
                        stations.push(create(
                            username,
                            &station_key,
                            &format!("{artist} Radio"),
                            LastFmRadioStationKind::Artist,
                            true,
                            &[artist.as_str()],
                            tracks,
                            1,
                            Utc::now(),
                        ));
                    }
                }

                for (tag, candidates, troubled) in genre_candidates {
                    let station_key = format!("genre-{}", key(&tag));
                    let artists: IgnoreCaseKeys =
                        candidates.iter().map(|item| item.track.artist.as_str()).collect();
                    if artists.len() < 4 && troubled {
                        unanswered = true;
                        carry(previous_stations, &mut stations, &mut carried, |station| {
                            station.kind == LastFmRadioStationKind::Genre
                                && dotnet::eq_ignore_case(&station.key, &station_key)
                        });
                    }
                    if artists.len() >= 4 {
                        let tracks = shape(
                            candidates,
                            &plays,
                            settings,
                            &unavailable,
                            &mut *random,
                            previous(&station_key),
                            artist_cap(settings, LastFmRadioStationKind::Genre),
                            false,
                            None,
                            Some(&banned),
                            Some(&disliked),
                        );
                        stations.push(create(
                            username,
                            &station_key,
                            &format!("{} Radio", title(&tag)),
                            LastFmRadioStationKind::Genre,
                            true,
                            &[tag.as_str()],
                            tracks,
                            1,
                            Utc::now(),
                        ));
                    }
                }

                // A ranking that got no answer may have left out the listener's genres, so the
                // genre stations they have fill the places it could not.
                if ranking_trouble.core {
                    let wanted = settings.effective_genre_station_count().max(0) as usize;
                    while stations
                        .iter()
                        .filter(|station| station.kind == LastFmRadioStationKind::Genre)
                        .count()
                        < wanted
                    {
                        if !carry(previous_stations, &mut stations, &mut carried, |station| {
                            station.kind == LastFmRadioStationKind::Genre
                        }) {
                            break;
                        }
                        unanswered = true;
                    }
                }
            }
        }

        if settings.enable_discovery_stations {
            for definition in settings
                .effective_discovery_stations()
                .into_iter()
                .filter(|item| item.enabled)
            {
                let (candidates, trouble) = lookup_trouble::watch(self.tracks_from_tags(
                    username,
                    &definition.tags,
                    candidate_target,
                    &budget,
                ))
                .await;
                let mut candidates = candidates?;
                let station_key = format!("pinned-{}", definition.id);
                // Last.fm's own tag charts make a pinned station; when they got no answer, the
                // station the listener has stands in for a thinner one, and for one made of
                // their plays.
                let charted = candidates
                    .iter()
                    .filter(|candidate| candidate.source.starts_with("tag:"))
                    .count();
                if trouble.core && charted < FRONTIER_FEWEST_SONGS {
                    unanswered = true;
                    let version = definition_version(&definition);
                    if carry(previous_stations, &mut stations, &mut carried, |station| {
                        station.kind == LastFmRadioStationKind::Pinned
                            && dotnet::eq_ignore_case(&station.key, &station_key)
                            && station.definition_version == version
                    }) {
                        continue;
                    }
                }
                if candidates.is_empty() {
                    candidates.extend(
                        plays
                            .iter()
                            .filter(|play| {
                                definition.tags.iter().any(|tag| {
                                    contains_ignore_case(play.genre.as_deref().unwrap_or_default(), tag)
                                })
                            })
                            .enumerate()
                            .map(|(rank, play)| to_candidate(play, rank)),
                    );
                }
                if !candidates.is_empty() {
                    let tracks = shape(
                        candidates,
                        &plays,
                        settings,
                        &unavailable,
                        &mut *random,
                        previous(&station_key),
                        artist_cap(settings, LastFmRadioStationKind::Pinned),
                        false,
                        None,
                        Some(&banned),
                        Some(&disliked),
                    );
                    stations.push(create(
                        username,
                        &station_key,
                        &definition.name,
                        LastFmRadioStationKind::Pinned,
                        false,
                        &definition.tags,
                        tracks,
                        definition_version(&definition),
                        Utc::now(),
                    ));
                }
            }
        }

        // Songs a carried station walked on to stay with it; any of them rated one star or found
        // unplayable since are dropped.
        let is_carried = |station: &LastFmRadioStation| {
            carried
                .iter()
                .any(|key| dotnet::eq_ignore_case(key, &station.key))
        };
        if !carried.is_empty() {
            let blocked = blocked_tracks(&user, Utc::now());
            for station in stations.iter().filter(|station| is_carried(station)) {
                self.drop_blocked_walked(&continuation_key(username, station), &blocked);
            }
        }
        let interval = TimeDelta::hours(i64::from(settings.effective_refresh_interval_hours()));
        for station in &mut stations {
            // A carried station is as good as the stations made with it, until the next refresh.
            let from = if is_carried(station) {
                now
            } else {
                station.changed_utc
            };
            station.valid_until_utc = from + interval;
        }
        let unshared = stations.clone();
        suppress_station_overlap(&mut stations);
        let made = stations.len() - carried.len();
        info!("Built {made} Last.fm radio stations for {username}");
        if !carried.is_empty() {
            info!(
                "Kept {} of {username}'s Last.fm radio stations as they were: their lookups got no answer",
                carried.len()
            );
        }
        let has_songs = |station: &LastFmRadioStation| !station.tracks.is_empty();
        Ok(BuiltStations {
            stations: stations.into_iter().filter(has_songs).collect(),
            unshared: unshared.into_iter().filter(has_songs).collect(),
            carried,
            unanswered,
        })
    }

    /// Drops songs that may not play from the songs one station snapshot walked on to.
    fn drop_blocked_walked(&self, key: &str, blocked: &IgnoreCaseKeys) {
        if let Some(tracks) = self.continuations.lock().get_mut(key) {
            tracks.retain(|track| !blocked.contains(&track_key(track)));
        }
    }

    /// Up to `count` songs for the listener's station `station_id`, none of them in `exclude`
    /// (matched by artist and title, or by the id a song was served as) and none the station
    /// served the listener in the last [`STATION_MEMORY`]: first the station's own tracks and
    /// then its reserve, in their order, then new songs of the station's kind. The new songs are
    /// drawn and shaped as a build draws the station, with everything the station holds and
    /// everything left out counted as already in it, and are kept for the station's snapshot, so
    /// Last.fm is asked again only once those run out. Once nothing new is found, the songs
    /// served longest ago come round again (never one in `exclude`). Empty while radio is off and
    /// for a station the listener does not have; Last.fm is waited on no longer than a build
    /// waits.
    pub async fn next_for_station(
        &self,
        username: &str,
        station_id: &str,
        exclude: &[LastFmRadioTrack],
        count: usize,
        cancellation_token: &CancellationToken,
    ) -> Vec<LastFmRadioTrack> {
        self.next_for_station_within(
            username,
            station_id,
            exclude,
            count,
            true,
            PROVIDER_BUDGET,
            cancellation_token,
        )
        .await
    }

    /// [`Self::next_for_station`] waiting at most `wait` for new songs, and with the songs served
    /// longest ago coming round again only with `fall_back`. A station walks on once at a time:
    /// an ask while a walk is under way joins it, and a walk that outlasts the wait goes on in
    /// the background, its songs kept for the next ask. A zero wait never waits; it only starts
    /// the walk. Whenever fewer new songs are left than this ask took, the next walk starts in
    /// the background at once.
    #[allow(clippy::too_many_arguments)]
    pub async fn next_for_station_within(
        &self,
        username: &str,
        station_id: &str,
        exclude: &[LastFmRadioTrack],
        count: usize,
        fall_back: bool,
        wait: Duration,
        cancellation_token: &CancellationToken,
    ) -> Vec<LastFmRadioTrack> {
        if count == 0 || !self.settings.current().last_fm.enable_radio {
            return Vec::new();
        }
        let Some(station) = self.state.find_station(username, station_id) else {
            return Vec::new();
        };
        // What the station served in the last day stays out too, as if the caller had sent it.
        let served = self.station_entries(username, station_id);
        let remembered: Vec<LastFmRadioTrack> = served.iter().map(|entry| entry.track.clone()).collect();
        let asked_out = LeftOut::of(exclude.iter().map(|track| (track, None)));
        let left_out = LeftOut::of(
            exclude.iter().map(|track| (track, None)).chain(
                served
                    .iter()
                    .map(|entry| (&entry.track, entry.served_as.as_deref())),
            ),
        );
        let walk_exclude: Vec<LastFmRadioTrack> = remembered.iter().chain(exclude).cloned().collect();
        let key = continuation_key(username, &station);
        let mut picks = Vec::new();
        let mut taken = IgnoreCaseKeys::new();
        let kept = self.kept(&key);
        pick_fresh(
            &mut picks,
            &mut taken,
            &left_out,
            station.tracks.iter().chain(&station.reserve).chain(&kept),
            count,
        );
        if picks.len() < count {
            let walk = self.walk_on_once(&key, username, &station, &walk_exclude, &kept);
            if !wait.is_zero() {
                tokio::select! {
                    biased;
                    () = cancellation_token.cancelled() => {}
                    _ = tokio::time::timeout(wait, walk) => {}
                }
                let kept = self.kept(&key);
                pick_fresh(&mut picks, &mut taken, &left_out, &kept, count);
            }
        }
        if picks.is_empty() && fall_back {
            // Nothing new anywhere: the songs served longest ago come round again rather than none.
            pick_fresh(&mut picks, &mut taken, &asked_out, &remembered, count);
            return picks;
        }
        let fresh_left = station
            .tracks
            .iter()
            .chain(&station.reserve)
            .chain(&self.kept(&key))
            .filter(|track| !left_out.contains(track) && !taken.contains(&track_key(track)))
            .count();
        if fresh_left < count {
            let kept = self.kept(&key);
            drop(self.walk_on_once(&key, username, &station, &walk_exclude, &kept));
        }
        picks
    }

    /// The new songs kept for the station snapshot `key`.
    fn kept(&self, key: &str) -> Vec<LastFmRadioTrack> {
        self.continuations.lock().get(key).cloned().unwrap_or_default()
    }

    /// The walk on under way for the station snapshot `key`, or a new one started now on its own
    /// task, whose songs are kept for the snapshot once it ends. It is never canceled by an
    /// asker: one that stops waiting leaves it to go on.
    fn walk_on_once(
        &self,
        key: &str,
        username: &str,
        station: &LastFmRadioStation,
        exclude: &[LastFmRadioTrack],
        kept: &[LastFmRadioTrack],
    ) -> Walk {
        let mut walks = self.walks.lock();
        if let Some(walk) = walks.get(key) {
            return walk.clone();
        }
        let service = self.clone();
        let (key_owned, username, station) = (key.to_string(), username.to_string(), station.clone());
        let (exclude, kept) = (exclude.to_vec(), kept.to_vec());
        let task = tokio::spawn(async move {
            // However the walk ends, the next ask may start another.
            let _done = WalkDone {
                walks: Arc::clone(&service.walks),
                key: key_owned.clone(),
            };
            let walked = service
                .walk_on(&username, &station, &exclude, &kept, &CancellationToken::new())
                .await;
            if !walked.is_empty() {
                service.keep_walked(key_owned, &walked);
            }
        });
        let walk: Walk = async move {
            let _ = task.await;
        }
        .boxed()
        .shared();
        walks.insert(key.to_string(), walk.clone());
        walk
    }

    /// Waits until no walk on is under way.
    pub async fn settled(&self) {
        loop {
            let walks: Vec<Walk> = self.walks.lock().values().cloned().collect();
            if walks.is_empty() {
                return;
            }
            futures::future::join_all(walks).await;
        }
    }

    /// Remembers that the listener's station served these songs now, so for
    /// [`STATION_MEMORY`] it serves none of them again (at most [`STATION_MEMORY_SONGS`] a
    /// station, the oldest forgotten first).
    pub fn record_station_served(
        &self,
        username: &str,
        station_id: &str,
        tracks: impl IntoIterator<Item = LastFmRadioTrack>,
    ) {
        self.record_station_served_as(
            username,
            station_id,
            tracks.into_iter().map(|track| (track, None)),
        );
    }

    /// [`Self::record_station_served`] for station tracks that went out as songs of another id
    /// (the library's copy of an outside track): the song is then left out by that id as well as
    /// by the track's own artist and title.
    pub fn record_station_served_as(
        &self,
        username: &str,
        station_id: &str,
        served: impl IntoIterator<Item = (LastFmRadioTrack, Option<String>)>,
    ) {
        let now = self.clock.lock().now();
        let key = history_key(username, station_id);
        let mut history = self.station_history.lock();
        let mut entries = history.shift_remove(&key).unwrap_or_default();
        for (track, served_as) in served {
            let song = dotnet::ordinal_ignore_case_key(&track_key(&track));
            entries.retain(|earlier| earlier.key != song);
            entries.push_back(StationServed {
                key: song,
                track,
                served_as,
                at: now,
            });
        }
        while entries.len() > STATION_MEMORY_SONGS {
            entries.pop_front();
        }
        history.insert(key, entries);
        while history.len() > STATIONS_REMEMBERED {
            history.shift_remove_index(0);
        }
    }

    /// The songs the listener's station served in the last [`STATION_MEMORY`], oldest first.
    pub fn station_served(&self, username: &str, station_id: &str) -> Vec<LastFmRadioTrack> {
        self.station_entries(username, station_id)
            .into_iter()
            .map(|entry| entry.track)
            .collect()
    }

    /// The songs the listener's station served in the last [`STATION_MEMORY`], as a test of
    /// whether a track is one of them.
    pub fn station_left_out(&self, username: &str, station_id: &str) -> LeftOut {
        let served = self.station_entries(username, station_id);
        LeftOut::of(
            served
                .iter()
                .map(|entry| (&entry.track, entry.served_as.as_deref())),
        )
    }

    /// What the listener's station served in the last [`STATION_MEMORY`], oldest first.
    fn station_entries(&self, username: &str, station_id: &str) -> Vec<StationServed> {
        let since = self.clock.lock().now() - STATION_MEMORY;
        self.station_history
            .lock()
            .get(&history_key(username, station_id))
            .map(|served| served.iter().filter(|entry| entry.at > since).cloned().collect())
            .unwrap_or_default()
    }

    /// New songs of the station's kind, none of them already in the station, in `kept` or in
    /// `exclude`. The station's own seeds are asked first (its artist, its tags, the listener's
    /// strongest artists or their recent songs); once those have nothing left, songs like the
    /// newest ones left out stand in, or like the station's last tracks when nothing was left
    /// out.
    async fn walk_on(
        &self,
        username: &str,
        station: &LastFmRadioStation,
        exclude: &[LastFmRadioTrack],
        kept: &[LastFmRadioTrack],
        cancellation_token: &CancellationToken,
    ) -> Vec<LastFmRadioTrack> {
        let snapshot = self.settings.current();
        let settings = &snapshot.last_fm;
        let user = self.state.get_user(username);
        let now = Utc::now();
        let Signals {
            plays,
            mut unavailable,
            banned,
            disliked,
        } = Signals::of(&user);
        for track in station
            .tracks
            .iter()
            .chain(&station.reserve)
            .chain(kept)
            .chain(exclude)
        {
            unavailable.insert(&track_key(track));
        }
        let previous: IgnoreCaseKeys = station.tracks.iter().chain(exclude).map(track_key).collect();
        let known = known_artists(&plays);
        let budget = Budget {
            caller: cancellation_token,
            deadline: Instant::now() + PROVIDER_BUDGET,
        };
        let newest: Vec<&LastFmRadioTrack> = if exclude.is_empty() {
            station.tracks.iter().rev().take(CONTINUATION_SEEDS).collect()
        } else {
            exclude.iter().rev().take(CONTINUATION_SEEDS).collect()
        };
        let newest: Vec<LastFmRadioPlay> = newest.into_iter().map(as_seed).collect();

        let kind = station.kind;
        let mut cap = artist_cap(settings, kind);
        let mut exclude_recent = false;
        let mut quota = None;
        let candidates = match kind {
            LastFmRadioStationKind::Artist => match station.seeds.first() {
                Some(artist) => self
                    .tracks_from_artist(username, artist, CONTINUATION_CANDIDATES, &known, &budget)
                    .await
                    .unwrap_or_default(),
                None => Vec::new(),
            },
            LastFmRadioStationKind::Genre => discount_known(
                self.tracks_from_tags(username, &station.seeds, CONTINUATION_CANDIDATES, &budget)
                    .await
                    .unwrap_or_default(),
                &known,
            ),
            LastFmRadioStationKind::Pinned => self
                .tracks_from_tags(username, &station.seeds, CONTINUATION_CANDIDATES, &budget)
                .await
                .unwrap_or_default(),
            LastFmRadioStationKind::Discovery => {
                exclude_recent = true;
                cap = FRONTIER_ARTIST_CAP;
                let artist_scores = score_artists(&plays, now);
                let walk_budget = budget.within(DISCOVERY_WALK_BUDGET);
                let mut found = self
                    .tracks_from_frontier(&artist_scores, &known, &disliked, &walk_budget)
                    .await;
                if found.len() < FRONTIER_FEWEST_SONGS && !station.seeds.is_empty() {
                    found = self
                        .tracks_from_tags(username, &station.seeds, CONTINUATION_CANDIDATES, &budget)
                        .await
                        .unwrap_or_default();
                    cap = artist_cap(settings, kind);
                }
                found
            }
            LastFmRadioStationKind::YourMix | LastFmRadioStationKind::Starter => {
                // The newest songs left out lead the seeds, so the mix follows where it went.
                let mut seen = IgnoreCaseKeys::new();
                let seeds: Vec<LastFmRadioPlay> = newest
                    .iter()
                    .cloned()
                    .chain(track_seeds(&plays, now))
                    .filter(|seed| seen.insert(&seed_normalizer::track_key(&seed.artist, &seed.title)))
                    .take(6)
                    .collect();
                let mut mix = self.tracks_from_seeds(username, &seeds, 12, &budget).await;
                let familiar: Vec<Candidate> = plays
                    .iter()
                    .enumerate()
                    .map(|(rank, play)| to_candidate(play, rank))
                    .collect();
                if mix.is_empty() {
                    mix.extend(familiar.iter().cloned());
                } else {
                    quota = Some(familiar_quota(settings));
                }
                familiar.into_iter().chain(mix).collect()
            }
        };
        let randomizer = self.randomizer.lock().clone();
        let mut random = randomizer();
        let mut walked = shape_walk(
            candidates,
            &plays,
            settings,
            &unavailable,
            &mut *random,
            &previous,
            cap,
            exclude_recent,
            quota,
            Some(&banned),
            Some(&disliked),
            1,
        )
        .tracks;
        let seeded_from_newest = matches!(
            kind,
            LastFmRadioStationKind::YourMix | LastFmRadioStationKind::Starter
        );
        if walked.is_empty() && !seeded_from_newest {
            let mut like = self.tracks_from_seeds(username, &newest, 12, &budget).await;
            if matches!(
                kind,
                LastFmRadioStationKind::Discovery | LastFmRadioStationKind::Genre
            ) {
                like = discount_known(like, &known);
            }
            walked = shape_walk(
                like,
                &plays,
                settings,
                &unavailable,
                &mut *random,
                &previous,
                artist_cap(settings, kind),
                exclude_recent,
                None,
                Some(&banned),
                Some(&disliked),
                1,
            )
            .tracks;
        }
        walked
    }

    /// Adds `walked` to what the station keeps for its snapshot, and makes it the station walked
    /// on most recently.
    fn keep_walked(&self, key: String, walked: &[LastFmRadioTrack]) {
        let mut continuations = self.continuations.lock();
        let mut kept = continuations.shift_remove(&key).unwrap_or_default();
        let mut known: IgnoreCaseKeys = kept.iter().map(track_key).collect();
        kept.extend(
            walked
                .iter()
                .filter(|track| known.insert(&track_key(track)))
                .cloned(),
        );
        if kept.len() > CONTINUATION_SONGS {
            kept.drain(..kept.len() - CONTINUATION_SONGS);
        }
        continuations.insert(key, kept);
        let over = continuations.len().saturating_sub(CONTINUATIONS_KEPT);
        if over > 0 {
            continuations.drain(..over);
        }
    }

    /// Remembers which song each served id stands for, so a later ask that leaves the id out
    /// leaves the song out.
    pub fn remember_served<'a>(&self, username: &str, songs: impl IntoIterator<Item = &'a Song>) {
        self.remember_served_tracks(
            username,
            songs
                .into_iter()
                .map(|song| (song.id.clone(), served_song_track(song))),
        );
    }

    /// Remembers which station track each served id stands for: an ask that leaves the id out
    /// leaves the track out by its own artist and title, as the station spells them, and by the
    /// id. At most [`SERVED_KEPT`] ids a listener, the oldest forgotten first, and a song served
    /// again counts as served now.
    pub fn remember_served_tracks(
        &self,
        username: &str,
        tracks: impl IntoIterator<Item = (String, LastFmRadioTrack)>,
    ) {
        let listener = dotnet::ordinal_ignore_case_key(&user_key(username));
        let mut served = self.served.lock();
        let mut ids = served.shift_remove(&listener).unwrap_or_default();
        for (id, mut track) in tracks.into_iter().filter(|(id, _)| !id.is_empty()) {
            track.resolved_id = Some(id.clone());
            ids.shift_remove(&id);
            ids.insert(id, track);
        }
        let over = ids.len().saturating_sub(SERVED_KEPT);
        if over > 0 {
            ids.drain(..over);
        }
        served.insert(listener, ids);
        let over = served.len().saturating_sub(SERVED_LISTENERS);
        if over > 0 {
            served.drain(..over);
        }
    }

    /// The song a served id stands for, when one was remembered for this listener.
    pub fn served_track(&self, username: &str, id: &str) -> Option<LastFmRadioTrack> {
        self.served
            .lock()
            .get(&dotnet::ordinal_ignore_case_key(&user_key(username)))?
            .get(id)
            .cloned()
    }

    /// Tracks similar to each seed, the seed's own weight riding along: the newest seed leads,
    /// each older one contributes [`SEED_RECENCY_DECAY`] of the previous, and a hearted seed
    /// counts half again. With Last.fm alone, its match score stays the per-track signal; with
    /// several sources, each source's list is ranked by position and weighted by its provider
    /// (see [`RadioSourceSet::weights`]), and seeds are asked [`SEEDS_AT_ONCE`] at a time so a
    /// slow source costs one timeout per three seeds.
    async fn tracks_from_seeds(
        &self,
        username: &str,
        seeds: &[LastFmRadioPlay],
        each: i32,
        budget: &Budget<'_>,
    ) -> Vec<Candidate> {
        let ordered: Vec<(&LastFmRadioPlay, f64)> = seeds
            .iter()
            .enumerate()
            .map(|(position, seed)| {
                (
                    seed,
                    SEED_RECENCY_DECAY.powi(position as i32)
                        * if seed.hearted { HEARTED_SEED_AFFINITY } else { 1.0 },
                )
            })
            .collect();
        let Some(sources) = &self.sources else {
            let mut result = Vec::new();
            for (seed, affinity) in ordered {
                let source = seed_normalizer::track_key(&seed.artist, &seed.title);
                match budget
                    .run(self.last_fm.get_similar_tracks(&seed.artist, &seed.title, each))
                    .await
                {
                    Ok(tracks) => result.extend(ranked(tracks, &source, affinity)),
                    Err(_) => break,
                }
            }
            return result;
        };

        let weights = sources.weights(Some(username), false);
        let gate = Semaphore::new(SEEDS_AT_ONCE);
        let empty = IndexMap::new();
        let lists = futures::future::join_all(ordered.into_iter().map(|(seed, affinity)| {
            let (gate, weights, empty) = (&gate, &weights, &empty);
            async move {
                let Ok(_permit) = budget.run(gate.acquire()).await else {
                    return Vec::new();
                };
                let source = seed_normalizer::track_key(&seed.artist, &seed.title);
                let library_song = (seed.is_local && !seed.song_id.is_empty()).then(|| Song {
                    id: seed.song_id.clone(),
                    title: seed.title.clone(),
                    artist: seed.artist.clone(),
                    is_local: true,
                    ..Default::default()
                });
                let radio_seed = RadioSeed::new(
                    seed.artist.clone(),
                    seed.title.clone(),
                    seed.duration,
                    library_song,
                );
                let Ok(answers) = budget.run(sources.ask_all(&radio_seed, each, empty, None)).await else {
                    return Vec::new();
                };
                let mut found = Vec::new();
                for answer in answers {
                    let factor = radio_blend::station_factor(answer.r#match)
                        * radio_blend::thin(&answer, each)
                        * weights.get(&answer.provider).copied().unwrap_or(1.0);
                    if factor <= 0.0 {
                        continue;
                    }
                    // Sources score on different scales (Last.fm's match, a flat 1 from YouTube
                    // Music), so with several sources each list is ranked by position alone, as
                    // song radio does, and the provider weights say how much each list counts.
                    let tracks: Vec<SimilarTrack> = answer
                        .library_songs
                        .iter()
                        .map(|song| {
                            SimilarTrack::new(song.artist.clone(), song.title.clone(), 1.0)
                                .with_duration(song.duration)
                                .with_provider(Some(answer.provider.clone()))
                        })
                        .chain(answer.tracks.iter().map(|track| SimilarTrack {
                            r#match: 1.0,
                            ..track.clone()
                        }))
                        .collect();
                    found.extend(ranked(
                        tracks,
                        &format!("{}:{source}", answer.provider),
                        affinity * factor,
                    ));
                }
                found
            }
        }))
        .await;
        lists.into_iter().flatten().collect()
    }

    async fn tracks_from_tags(
        &self,
        username: &str,
        tags: &[String],
        each: i32,
        budget: &Budget<'_>,
    ) -> anyhow::Result<Vec<Candidate>> {
        let mut result = Vec::new();
        for tag in tags.iter().take(5) {
            match budget.run(self.last_fm.get_tag_top_tracks(tag, each)).await {
                Ok(tracks) => result.extend(ranked(tracks?, &format!("tag:{tag}"), 1.0)),
                Err(_) => break,
            }
            // Last.fm's tag chart is the line above; the other sources' answers for the tag (LB
            // Radio, with a ListenBrainz token) join it, each list weighted by its provider.
            if let Some(sources) = &self.sources {
                let weights = sources.weights(Some(username), false);
                match budget.run(sources.tag_all(tag, each)).await {
                    Ok(answers) => {
                        for answer in answers.into_iter().filter(|answer| answer.provider != LAST_FM) {
                            let weight = weights.get(&answer.provider).copied().unwrap_or(1.0);
                            let source = format!("{}:tag:{tag}", answer.provider);
                            result.extend(ranked(answer.tracks, &source, weight));
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        Ok(result)
    }

    /// The seed artist's own top tracks lead; similar artists' top tracks ride at
    /// [`NEIGHBOUR_ARTIST_AFFINITY`] so the station stays about who it is named for. Of
    /// [`ARTIST_RADIO_NEIGHBOURS`] neighbours, the five used are the strongest with the ones the
    /// listener knows counted at a third of their match: a small artist whose closest neighbours
    /// are all familiar still leads somewhere new.
    async fn tracks_from_artist(
        &self,
        username: &str,
        artist: &str,
        candidate_target: i32,
        known: &IgnoreCaseKeys,
        budget: &Budget<'_>,
    ) -> anyhow::Result<Vec<Candidate>> {
        let mut result = Vec::new();
        // The build's budget running out keeps what was found so far: the artist's own top
        // tracks alone still make a station.
        let Ok(own) = budget
            .run(
                self.last_fm
                    .get_artist_top_tracks(artist, 50.min(candidate_target)),
            )
            .await
        else {
            return Ok(result);
        };
        result.extend(ranked(own, &format!("artist:{artist}"), 1.0));
        let Ok(similar) = budget
            .run(self.last_fm.get_similar_artists(artist, ARTIST_RADIO_NEIGHBOURS))
            .await
        else {
            return Ok(result);
        };
        for similar in unknown_neighbours_first(similar?, known).into_iter().take(5) {
            let Ok(tracks) = budget
                .run(
                    self.last_fm
                        .get_artist_top_tracks(&similar.name, 20.min(6.max(candidate_target / 5))),
                )
                .await
            else {
                return Ok(result);
            };
            result.extend(ranked(
                tracks,
                &format!("artist:{}", similar.name),
                NEIGHBOUR_ARTIST_AFFINITY,
            ));
        }
        // The other sources' radio for the artist (YouTube Music's, LB Radio's), beside the
        // similar artists' songs. Last.fm is not asked again: its answer is the lines above.
        if let Some(sources) = &self.sources {
            let weights = sources.weights(Some(username), false);
            let seed = RadioSeed::new(artist, "", None, None);
            let empty = IndexMap::new();
            let not_last_fm =
                |source: &dyn crate::services::radio::IRadioSource| source.provider() != LAST_FM;
            let Ok(answers) = budget
                .run(sources.ask_all(&seed, candidate_target, &empty, Some(&not_last_fm)))
                .await
            else {
                return Ok(result);
            };
            for answer in answers.into_iter().filter(|answer| answer.provider != LAST_FM) {
                let weight = weights.get(&answer.provider).copied().unwrap_or(1.0);
                let source = format!("{}:artist:{artist}", answer.provider);
                result.extend(ranked(answer.tracks, &source, NEIGHBOUR_ARTIST_AFFINITY * weight));
            }
        }
        Ok(result)
    }

    /// Songs by artists the listener does not know yet, found through the ones they play (see
    /// [`FRONTIER_SEEDS`]): the walk scores each new neighbour of the strongest artists, steps
    /// once more from the best of them, then asks the best new artists for their top songs, each
    /// list weighted by its artist's score. The build's budget running out keeps what the walk
    /// has found so far.
    async fn tracks_from_frontier(
        &self,
        artist_scores: &[(String, f64)],
        known: &IgnoreCaseKeys,
        disliked: &IgnoreCaseKeys,
        budget: &Budget<'_>,
    ) -> Vec<Candidate> {
        let seeds: Vec<&(String, f64)> = artist_scores.iter().take(FRONTIER_SEEDS).collect();
        let Some(strongest) = seeds.first().map(|(_, score)| score.max(1e-9)) else {
            return Vec::new();
        };
        let mut scores = FrontierScores::default();
        let first: Vec<(String, f64)> = seeds
            .iter()
            .map(|(artist, score)| (artist.clone(), score / strongest))
            .collect();
        for (similar, weight) in self.neighbours(&first, budget).await {
            scores.add(&similar, weight, known, disliked);
        }
        let step = scores.top(SECOND_STEP_SEEDS);
        if let Some(best) = step.first().map(|(_, score)| score.max(1e-9)) {
            let from: Vec<(String, f64)> = step
                .iter()
                .map(|(artist, score)| (artist.clone(), SECOND_STEP_WEIGHT * score / best))
                .collect();
            for (similar, weight) in self.neighbours(&from, budget).await {
                scores.add(&similar, weight, known, disliked);
            }
        }
        let picked = scores.top(FRONTIER_ARTISTS);
        let Some(top) = picked.first().map(|(_, score)| score.max(1e-9)) else {
            return Vec::new();
        };
        let gate = Semaphore::new(SEEDS_AT_ONCE);
        let lists = futures::future::join_all(picked.iter().map(|(artist, score)| {
            let gate = &gate;
            async move {
                let Ok(_permit) = budget.run(gate.acquire()).await else {
                    return Vec::new();
                };
                let Ok(tracks) = budget
                    .run(self.last_fm.get_artist_top_tracks(artist, FRONTIER_TRACKS_EACH))
                    .await
                else {
                    return Vec::new();
                };
                ranked(tracks, &format!("frontier:{artist}"), score / top)
            }
        }))
        .await;
        lists.into_iter().flatten().collect()
    }

    /// Each artist's similar artists with the weight its answer carries; an artist whose
    /// answer did not come in time, or could not be read, adds nothing.
    async fn neighbours(
        &self,
        from: &[(String, f64)],
        budget: &Budget<'_>,
    ) -> Vec<(Vec<octo_core::last_fm::SimilarArtist>, f64)> {
        futures::future::join_all(from.iter().map(|(artist, weight)| async move {
            match budget
                .run(self.last_fm.get_similar_artists(artist, FRONTIER_NEIGHBOURS))
                .await
            {
                Ok(Ok(similar)) => Some((similar, *weight)),
                _ => None,
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect()
    }
}

/// What a listener's state says about what may play: their plays newest first, the songs that
/// would not play lately, the songs they rated one star, and those songs' artists.
struct Signals {
    plays: Vec<LastFmRadioPlay>,
    unavailable: IgnoreCaseKeys,
    banned: IgnoreCaseKeys,
    disliked: IgnoreCaseKeys,
}

impl Signals {
    fn of(user: &LastFmRadioUserState) -> Self {
        let mut plays = user.plays.clone();
        plays.sort_by_key(|a| std::cmp::Reverse(a.played_at_utc));
        let unavailable = still_unavailable(user, Utc::now());
        let banned: IgnoreCaseKeys = user.radio_bans.iter().map(|ban| ban.key.as_str()).collect();
        let disliked: IgnoreCaseKeys = user
            .radio_bans
            .iter()
            .map(|ban| artist_key(&ban.artist))
            .collect();
        Signals {
            plays,
            unavailable,
            banned,
            disliked,
        }
    }
}

/// The eight strongest recent signals among `plays`, one per song. Seeds are not simply the
/// newest plays: a heart outranks a play, a chosen play outranks one the radio served, and age
/// decays.
fn track_seeds(plays: &[LastFmRadioPlay], now: chrono::DateTime<Utc>) -> Vec<LastFmRadioPlay> {
    let mut by_seed_score: Vec<&LastFmRadioPlay> = plays.iter().collect();
    by_seed_score.sort_by(|a, b| {
        seed_score(b, now)
            .partial_cmp(&seed_score(a, now))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut seen_seeds = std::collections::HashSet::new();
    by_seed_score
        .into_iter()
        .filter(|play| seen_seeds.insert(seed_normalizer::track_key(&play.artist, &play.title)))
        .take(8)
        .cloned()
        .collect()
}

/// How many of Your Radio's songs are the listener's own: what the Discovery balance leaves.
fn familiar_quota(settings: &octo_core::settings::LastFmSettings) -> i32 {
    let count = settings.effective_radio_track_count();
    count
        - dotnet::round(
            f64::from(count) * f64::from(settings.effective_discovery_percent()) / 100.0,
            0,
        ) as i32
}

/// A track's key, as the walk matches songs.
fn track_key(track: &LastFmRadioTrack) -> String {
    seed_normalizer::track_key(&track.artist, &track.title)
}

/// Adds the `tracks` not left out and not already taken to `picks`, in order, until it holds
/// `count`.
fn pick_fresh<'a>(
    picks: &mut Vec<LastFmRadioTrack>,
    taken: &mut IgnoreCaseKeys,
    left_out: &LeftOut,
    tracks: impl IntoIterator<Item = &'a LastFmRadioTrack>,
    count: usize,
) {
    for track in tracks {
        if picks.len() >= count {
            return;
        }
        if left_out.contains(track) || !taken.insert(&track_key(track)) {
            continue;
        }
        picks.push(track.clone());
    }
}

/// Where a station's new songs are kept: its listener, its id and its snapshot, so a rebuilt
/// station starts afresh.
fn continuation_key(username: &str, station: &LastFmRadioStation) -> String {
    format!(
        "{}|{}|{}",
        dotnet::ordinal_ignore_case_key(&user_key(username)),
        station.id,
        dotnet_ticks(station.changed_utc)
    )
}

/// Where a station's served songs are remembered: its listener and its id.
fn history_key(username: &str, station_id: &str) -> String {
    format!(
        "{}|{station_id}",
        dotnet::ordinal_ignore_case_key(&user_key(username))
    )
}

/// A served song as the station memory keeps it.
pub fn served_song_track(song: &Song) -> LastFmRadioTrack {
    LastFmRadioTrack {
        artist: song.artist.clone(),
        title: song.title.clone(),
        album: (!dotnet::is_blank(&song.album)).then(|| song.album.clone()),
        genre: song.genre.clone(),
        duration: song.duration,
        resolved_id: Some(song.id.clone()),
        is_local: song.is_local,
        ..Default::default()
    }
}

/// A track as a seed for songs like it.
fn as_seed(track: &LastFmRadioTrack) -> LastFmRadioPlay {
    LastFmRadioPlay {
        artist: track.artist.clone(),
        title: track.title.clone(),
        album: track.album.clone(),
        genre: track.genre.clone(),
        duration: track.duration,
        ..Default::default()
    }
}

/// The station's previous snapshot, matched by key ignoring case, or `empty`.
fn previous_snapshot<'a>(
    snapshots: &'a [(String, IgnoreCaseKeys)],
    empty: &'a IgnoreCaseKeys,
    station_key: &str,
) -> &'a IgnoreCaseKeys {
    snapshots
        .iter()
        .find(|(key, _)| dotnet::eq_ignore_case(key, station_key))
        .map_or(empty, |(_, keys)| keys)
}

/// Whether `value` appears in `text`, ignoring case.
fn contains_ignore_case(text: &str, value: &str) -> bool {
    dotnet::ordinal_ignore_case_key(text).contains(&dotnet::ordinal_ignore_case_key(value))
}

#[cfg(test)]
#[path = "last_fm_radio_recommendation_service_tests.rs"]
mod tests;
