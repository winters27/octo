//! The pure half of the radio recommendation service: how candidates are weighed, drawn and
//! shaped into a station, and how a listener's plays become seeds, artists and tags. The service
//! that asks Last.fm is `octo::services::last_fm::last_fm_radio_recommendation_service`.
//!
//! Which candidates make the cut is a weighted draw rather than a fixed top-N, so two refreshes
//! of the same profile give two different stations; the previous snapshot is demoted so a
//! refresh rotates the station instead of restating it.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use sha2::{Digest, Sha256};

use crate::common::dotnet;
use crate::common::dotnet_random::DotnetRandom;
use crate::last_fm::last_fm_radio_placement::{self, Spacing};
use crate::last_fm::last_fm_radio_seed_normalizer as seed_normalizer;
use crate::last_fm::last_fm_radio_state_store::station_id;
use crate::last_fm::radio_filler;
use crate::last_fm::{SimilarArtist, SimilarTrack};
use crate::metadata::genre_normalizer::to_title_case;
use crate::models::radio::{
    LastFmRadioPlay, LastFmRadioStation, LastFmRadioStationKind, LastFmRadioTrack, LastFmRadioUserState,
};
use crate::radio::radio_provider;
use crate::settings::{DiscoveryStationSettings, LastFmSettings};

/// Share of its weight a track keeps when it was already in this station's previous snapshot.
/// Low enough that a refresh is mostly new, high enough that a strong match can still come back.
pub const PREVIOUS_SNAPSHOT_WEIGHT: f64 = 0.35;

/// How fast a provider's ranking fades. Rank r keeps 1 / (1 + r / depth) of its weight, so the
/// top of a tag's or an artist's list still leads every draw and the tail is where refreshes
/// differ. 20 is the middle of ListenBrainz's easy/medium/hard popularity windows: reachable,
/// not bottom of the barrel.
pub const RANK_HALF_DEPTH: f64 = 20.0;

/// Top tracks of an artist similar to the seed, relative to the seed's own.
pub const NEIGHBOUR_ARTIST_AFFINITY: f64 = 0.6;

/// Each older seed contributes this much of the previous one; hearts count half again.
pub const SEED_RECENCY_DECAY: f64 = 0.85;
pub const HEARTED_SEED_AFFINITY: f64 = 1.5;

pub const FAMILIAR_SOURCE: &str = "history";

/// How much a play counts as a listening signal, by where it came from. A track the listener
/// chose and scrobbled is the real signal. A track the radio played to the end says less: they
/// did not pick it, they only did not switch it off. Left at full weight, a station slowly
/// trains itself on its own output. A play recorded at bootstrap from a random library track
/// says less still.
pub const RADIO_PLAY_WEIGHT: f64 = 0.4;
pub const RANDOM_BOOTSTRAP_WEIGHT: f64 = 0.5;

/// A song heard this recently waits until nothing else fits.
pub const HEARD_LATELY: TimeDelta = TimeDelta::days(8);

/// How many songs apart one artist must be, and one title (a cover counts). Never more than the
/// station's candidates can give. The artist gap must stay at least the stream's flow window
/// (`last_fm_radio_stream_service::FLOW_WINDOW`): the stream may reorder within that window, and
/// the gap is what keeps an artist from landing back to back after it.
pub const ARTIST_GAP: i32 = 4;
pub const TITLE_GAP: usize = 8;

/// Weight kept by an artist with a song the listener rated one star.
pub const DISLIKED_ARTIST_WEIGHT: f64 = 0.5;

/// Seeds asked at once when several sources answer: the sources' own gates still pace them.
pub const SEEDS_AT_ONCE: usize = 3;

/// The source of a build's draw: a double in [0, 1).
pub trait RadioRandom: Send {
    fn next_double(&mut self) -> f64;
}

impl RadioRandom for DotnetRandom {
    fn next_double(&mut self) -> f64 {
        DotnetRandom::next_double(self)
    }
}

/// A fresh draw source per build. Tests replace it with a seeded one so a build repeats exactly.
pub type Randomizer = Arc<dyn Fn() -> Box<dyn RadioRandom> + Send + Sync>;

/// An unseeded draw, different every build.
pub fn default_randomizer() -> Randomizer {
    Arc::new(|| Box::new(DotnetRandom::new(rand::random::<i32>())) as Box<dyn RadioRandom>)
}

/// A draw from `seed`, the same every build.
pub fn seeded_randomizer(seed: i32) -> Randomizer {
    Arc::new(move || Box::new(DotnetRandom::new(seed)) as Box<dyn RadioRandom>)
}

/// A candidate with its final draw weight (match x rank decay x seed affinity) and the list it
/// came from: a seed track, a tag, an artist, or the listener's history.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub track: SimilarTrack,
    pub weight: f64,
    pub source: String,
}

/// How much a play counts, by where it came from (see [`RADIO_PLAY_WEIGHT`]).
pub fn source_weight(play: &LastFmRadioPlay) -> f64 {
    match play.source.as_str() {
        "internet-radio" => RADIO_PLAY_WEIGHT,
        "bootstrap-random" => RANDOM_BOOTSTRAP_WEIGHT,
        _ => 1.0,
    }
}

const DENIED_TAGS: [&str; 29] = [
    // Last.fm's tags as listeners write them, in both spellings: these are data, not Octo's words.
    "seen live",
    "favorites",
    "favourites",
    "owned",
    "spotify",
    "albums i own",
    "under 2000 listeners",
    "awesome",
    "love",
    "best",
    // Sentiment and superlatives say how a listener felt, not what the music is.
    "favorite song",
    "favourite song",
    "favorite songs",
    "favourite songs",
    "my love",
    "love at first listen",
    "beautiful",
    "epic",
    "legendary",
    "classic",
    "amazing",
    "perfect",
    "masterpiece",
    "good",
    "great",
    "catchy",
    "fun",
    "chill",
    "cool",
];

const TAG_ALIASES: [(&str, &str); 6] = [
    ("electronica", "electronic"),
    ("hip hop", "hip-hop"),
    ("hiphop", "hip-hop"),
    ("rnb", "r&b"),
    ("rhythm and blues", "r&b"),
    ("alt rock", "alternative rock"),
];

/// One spelling per tag, shared by seeding and by kinship: lower case, single spaces, the alias
/// table applied ("hip hop" and "hiphop" are "hip-hop"), and empty for tags that describe the
/// listener rather than the music ("seen live", "owned", "favorites").
pub fn canonical_tag(value: &str) -> String {
    let mut tag = DiscoveryStationSettings::normalize_tag(value);
    if let Some((_, alias)) = TAG_ALIASES
        .iter()
        .find(|(from, _)| dotnet::eq_ignore_case(from, &tag))
    {
        tag = (*alias).to_string();
    }
    if DENIED_TAGS
        .iter()
        .any(|denied| dotnet::eq_ignore_case(denied, &tag))
    {
        String::new()
    } else {
        tag
    }
}

/// How many tracks one source list may hold: an even share and a half, never fewer than three.
/// Six seeds feeding a 50-track mix each get at most 13; a station built from one tag is
/// unconstrained by it.
pub fn source_cap(target: i32, sources: usize) -> i32 {
    let share = 1.5 * f64::from(target) / sources.max(1) as f64;
    3.max(share.ceil() as i32)
}

/// The seed ranking: provenance, hearts, and a 45-day recency decay.
pub fn seed_score(play: &LastFmRadioPlay, now: DateTime<Utc>) -> f64 {
    let days = (now - play.played_at_utc).num_microseconds().map_or_else(
        || (now - play.played_at_utc).num_milliseconds() as f64 / 86_400_000.0,
        |micros| micros as f64 / 86_400_000_000.0,
    );
    source_weight(play) * (if play.hearted { 2.0 } else { 1.0 }) * (-days / 45.0).exp()
}

/// How many tracks one artist may hold in a station. An artist station is about its artist, so a
/// quarter of it may be theirs; everywhere else an artist is a guest.
pub fn artist_cap(settings: &LastFmSettings, kind: LastFmRadioStationKind) -> i32 {
    if kind == LastFmRadioStationKind::Artist {
        3.max(settings.effective_radio_track_count() / 4)
    } else {
        2.max(settings.effective_radio_track_count() / 10)
    }
}

/// A play as a candidate: hearted counts double, provenance scales it, and its place in the
/// recency order decays the same way a provider rank does.
pub fn to_candidate(play: &LastFmRadioPlay, recency_rank: usize) -> Candidate {
    weigh(
        SimilarTrack::new(
            play.artist.clone(),
            play.title.clone(),
            if play.hearted { 2.0 } else { 1.0 },
        )
        .with_duration(play.duration),
        recency_rank,
        FAMILIAR_SOURCE,
        source_weight(play),
    )
}

fn rank_decay(rank: usize) -> f64 {
    1.0 / (1.0 + rank as f64 / RANK_HALF_DEPTH)
}

pub fn weigh(track: SimilarTrack, rank: usize, source: &str, affinity: f64) -> Candidate {
    let weight = track.r#match.max(0.05) * rank_decay(rank) * affinity;
    Candidate {
        track,
        weight,
        source: source.to_string(),
    }
}

/// Weights a provider's list in the order it came, which is the provider's ranking.
pub fn ranked(tracks: Vec<SimilarTrack>, source: &str, affinity: f64) -> Vec<Candidate> {
    tracks
        .into_iter()
        .enumerate()
        .map(|(rank, track)| weigh(track, rank, source, affinity))
        .collect()
}

/// A set of strings compared ordinally, ignoring case.
#[derive(Debug, Clone, Default)]
pub struct IgnoreCaseKeys(HashSet<String>);

impl IgnoreCaseKeys {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, value: &str) -> bool {
        self.0.insert(dotnet::ordinal_ignore_case_key(value))
    }

    pub fn contains(&self, value: &str) -> bool {
        self.0.contains(&dotnet::ordinal_ignore_case_key(value))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<S: AsRef<str>> FromIterator<S> for IgnoreCaseKeys {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        let mut keys = IgnoreCaseKeys::new();
        for value in iter {
            keys.insert(value.as_ref());
        }
        keys
    }
}

/// Selects the station's tracks from its weighted candidates. Filler and songs rated one star
/// never make it, and an artist with a one-star song keeps half its weight. One weighted draw
/// orders the pool (see [`weighted_order`]), then the walk applies the rules: nothing
/// unavailable, nothing just played when asked, no artist past its cap for this station kind, no
/// source list past its share, the familiar quota when set, an artist not within [`ARTIST_GAP`]
/// songs of itself and a title not within [`TITLE_GAP`]. A new song heard in the last
/// [`HEARD_LATELY`], or one the gaps refused, waits; if the walk falls short they fill the rest,
/// never the same artist twice in a row. The caps are the marginal-relevance idea applied
/// greedily, with artist and source repetition as the redundancy; and, with a familiar quota,
/// the two halves placed at random at the discovery share, paced to what the walk took, keeping
/// the walk's artist and title gaps.
pub fn shape(
    candidates: Vec<Candidate>,
    plays: &[LastFmRadioPlay],
    settings: &LastFmSettings,
    unavailable: &IgnoreCaseKeys,
    random: &mut dyn RadioRandom,
    previous: &IgnoreCaseKeys,
    artist_cap: i32,
    exclude_recent: bool,
    familiar_quota: Option<i32>,
    banned: Option<&IgnoreCaseKeys>,
    disliked: Option<&IgnoreCaseKeys>,
) -> Vec<LastFmRadioTrack> {
    shape_walk(
        candidates,
        plays,
        settings,
        unavailable,
        random,
        previous,
        artist_cap,
        exclude_recent,
        familiar_quota,
        banned,
        disliked,
        1,
    )
    .tracks
}

/// A station's tracks and, behind them, the walk's next picks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shaped {
    pub tracks: Vec<LastFmRadioTrack>,
    pub reserve: Vec<LastFmRadioTrack>,
}

/// [`shape`] without a familiar quota, the walk carried on past the station's size so that
/// tracks and reserve together hold up to twice as many. The tracks are exactly what [`shape`]
/// would take; the reserve is what the same walk would take next, under the same caps, for
/// whatever the station cannot use once its tracks are resolved.
pub fn shape_with_reserve(
    candidates: Vec<Candidate>,
    plays: &[LastFmRadioPlay],
    settings: &LastFmSettings,
    unavailable: &IgnoreCaseKeys,
    random: &mut dyn RadioRandom,
    previous: &IgnoreCaseKeys,
    artist_cap: i32,
    exclude_recent: bool,
    banned: Option<&IgnoreCaseKeys>,
    disliked: Option<&IgnoreCaseKeys>,
) -> Shaped {
    shape_walk(
        candidates,
        plays,
        settings,
        unavailable,
        random,
        previous,
        artist_cap,
        exclude_recent,
        None,
        banned,
        disliked,
        2,
    )
}

/// The walk behind [`shape`] and [`shape_with_reserve`]: it takes up to `reach` times the
/// station's size, and everything past the size is the reserve.
pub fn shape_walk(
    candidates: Vec<Candidate>,
    plays: &[LastFmRadioPlay],
    settings: &LastFmSettings,
    unavailable: &IgnoreCaseKeys,
    random: &mut dyn RadioRandom,
    previous: &IgnoreCaseKeys,
    artist_cap: i32,
    exclude_recent: bool,
    familiar_quota: Option<i32>,
    banned: Option<&IgnoreCaseKeys>,
    disliked: Option<&IgnoreCaseKeys>,
    reach: i32,
) -> Shaped {
    let target = settings.effective_radio_track_count();
    let walk_target = target.saturating_mul(reach.max(1));
    let recent: IgnoreCaseKeys = plays
        .iter()
        .take(30)
        .map(|play| seed_normalizer::track_key(&play.artist, &play.title))
        .collect();
    let heard_after = Utc::now() - HEARD_LATELY;
    let heard: IgnoreCaseKeys = plays
        .iter()
        .filter(|play| play.played_at_utc > heard_after)
        .map(|play| seed_normalizer::track_key(&play.artist, &play.title))
        .collect();

    // Grouped by the track key (ordinal), keeping each group's heaviest, the first of equals.
    let mut groups: IndexMap<String, Candidate> = IndexMap::new();
    for mut item in candidates.into_iter().filter(|item| {
        !item.track.artist.is_empty()
            && !item.track.title.is_empty()
            && !radio_filler::is_filler(Some(&item.track.title), item.track.duration)
            && !banned.is_some_and(|banned| {
                banned.contains(&seed_normalizer::track_key(&item.track.artist, &item.track.title))
            })
    }) {
        if disliked.is_some_and(|disliked| disliked.contains(&artist_key(&item.track.artist))) {
            item.weight *= DISLIKED_ARTIST_WEIGHT;
        }
        let key = seed_normalizer::track_key(&item.track.artist, &item.track.title);
        match groups.get_mut(&key) {
            Some(best) => {
                if item.weight > best.weight {
                    *best = item;
                }
            }
            None => {
                groups.insert(key, item);
            }
        }
    }
    let distinct: Vec<Candidate> = groups.into_values().collect();
    // The cap is per seed, not per source list: three sources answering one seed are one seed's
    // songs, so a mix of six seeds keeps six shares however many sources answer. The familiar
    // plays have their own quota and sit outside it.
    let source_cap = source_cap(
        target,
        distinct
            .iter()
            .filter(|item| item.source != FAMILIAR_SOURCE)
            .map(|item| seed_group(&item.source))
            .collect::<IgnoreCaseKeys>()
            .len(),
    );
    let artist_gap = (distinct
        .iter()
        .map(|item| artist_key(&item.track.artist))
        .collect::<IgnoreCaseKeys>()
        .len() as i32
        - 1)
    .clamp(1, ARTIST_GAP) as usize;

    let mut walk = Walk {
        target,
        unavailable,
        recent: &recent,
        heard: &heard,
        exclude_recent,
        artist_cap,
        source_cap,
        familiar_quota,
        artist_gap,
        per_artist: HashMap::new(),
        per_source: HashMap::new(),
        familiar_taken: 0,
        output: Vec::new(),
        familiar_flags: Vec::new(),
        output_artists: Vec::new(),
        output_titles: Vec::new(),
    };
    let mut held = Vec::new();
    for candidate in weighted_order(distinct, random, previous) {
        if walk.output.len() as i32 >= walk_target {
            break;
        }
        if walk.admit(&candidate, true) == Verdict::Wait {
            held.push(candidate);
        }
    }
    for candidate in &held {
        if walk.output.len() as i32 >= walk_target {
            break;
        }
        walk.admit(candidate, false);
    }
    let Walk {
        mut output,
        familiar_flags,
        output_artists,
        output_titles,
        ..
    } = walk;
    let reserve = output.split_off(output.len().min(target.max(0) as usize));
    // Your Radio (and Starter Radio): the familiar tracks and the discoveries take turns at random
    // at the discovery share (last_fm_radio_placement), instead of the walk's order putting the
    // familiar first. The share is paced to what the walk took, so a short walk ends on no block
    // of discoveries. The placement keeps the walk's artist and title gaps. It is handed the
    // walk's keys, and folds the artist key again with the spacing's `artist_key`, which groups
    // artists as the walk does or more coarsely: a gap the placement keeps, the walk's keys
    // keep too.
    if let Some(quota) = familiar_quota.filter(|_| target > 0) {
        let share = f64::from(target - quota) / f64::from(target);
        let flagged: Vec<Placed> = output
            .into_iter()
            .zip(familiar_flags)
            .zip(output_artists.into_iter().zip(output_titles))
            .map(|((track, familiar), (artist, title))| (track, familiar, artist, title))
            .collect();
        let title_of = |(_, _, _, title): &Placed| title.clone();
        let tracks = last_fm_radio_placement::place(
            &flagged,
            |(_, familiar, _, _): &Placed| !*familiar,
            |(_, _, artist, _): &Placed| Some(artist.clone()),
            None,
            Spacing {
                artist_gap,
                title: Some((&title_of, TITLE_GAP)),
            },
            share,
            random,
        )
        .into_iter()
        .map(|(track, _, _, _)| track)
        .collect();
        return Shaped { tracks, reserve };
    }
    Shaped {
        tracks: output,
        reserve,
    }
}

/// A Your Radio track with what its placement needs: whether it is familiar, and the walk's
/// artist and title keys.
type Placed = (LastFmRadioTrack, bool, String, String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Taken,
    Skip,
    Wait,
}

/// The walk's running counts, shared by its two passes.
struct Walk<'a> {
    target: i32,
    unavailable: &'a IgnoreCaseKeys,
    recent: &'a IgnoreCaseKeys,
    heard: &'a IgnoreCaseKeys,
    exclude_recent: bool,
    artist_cap: i32,
    source_cap: i32,
    familiar_quota: Option<i32>,
    artist_gap: usize,
    per_artist: HashMap<String, i32>,
    per_source: HashMap<String, i32>,
    familiar_taken: i32,
    output: Vec<LastFmRadioTrack>,
    /// Beside each output track: whether it is a familiar one, for Your Radio's placement.
    familiar_flags: Vec<bool>,
    output_artists: Vec<String>,
    output_titles: Vec<String>,
}

impl Walk<'_> {
    fn admit(&mut self, candidate: &Candidate, spaced: bool) -> Verdict {
        let track = &candidate.track;
        let key = seed_normalizer::track_key(&track.artist, &track.title);
        let artist = artist_key(&track.artist);
        let artist_folded = dotnet::ordinal_ignore_case_key(&artist);
        if self.unavailable.contains(&key) {
            return Verdict::Skip;
        }
        if self.exclude_recent && self.recent.contains(&key) {
            return Verdict::Skip;
        }
        if self.per_artist.get(&artist_folded).copied().unwrap_or(0) >= self.artist_cap {
            return Verdict::Skip;
        }
        let is_familiar = candidate.source == FAMILIAR_SOURCE;
        let group = dotnet::ordinal_ignore_case_key(seed_group(&candidate.source));
        if !is_familiar && self.per_source.get(&group).copied().unwrap_or(0) >= self.source_cap {
            return Verdict::Skip;
        }
        if let Some(quota) = self.familiar_quota {
            if is_familiar && self.familiar_taken >= quota {
                return Verdict::Skip;
            }
            if !is_familiar && self.output.len() as i32 - self.familiar_taken >= self.target - quota {
                return Verdict::Skip;
            }
        }
        let title = dotnet::to_lower_invariant(&seed_normalizer::title(&track.title));
        if spaced {
            // A familiar song is there because it was heard; only new ones wait.
            if !is_familiar && self.heard.contains(&key) {
                return Verdict::Wait;
            }
            let from = self.output_artists.len().saturating_sub(self.artist_gap);
            if self.output_artists[from..]
                .iter()
                .any(|earlier| dotnet::eq_ignore_case(earlier, &artist))
            {
                return Verdict::Wait;
            }
            let from = self.output_titles.len().saturating_sub(TITLE_GAP);
            if self.output_titles[from..].contains(&title) {
                return Verdict::Wait;
            }
        } else if self
            .output_artists
            .last()
            .is_some_and(|last| dotnet::eq_ignore_case(last, &artist))
        {
            return Verdict::Skip;
        }
        self.output.push(LastFmRadioTrack {
            artist: seed_normalizer::artist(&track.artist),
            title: seed_normalizer::title(&track.title),
            duration: track.duration,
            score: track.r#match,
            you_tube_id: track.you_tube_id.clone(),
            // Which source suggested it, shown in the apps; a familiar song was the listener's
            // own pick.
            source: track.provider.clone().unwrap_or_else(|| {
                if is_familiar {
                    radio_provider::HISTORY.to_string()
                } else {
                    radio_provider::LAST_FM.to_string()
                }
            }),
            ..Default::default()
        });
        self.familiar_flags.push(is_familiar);
        self.output_artists.push(artist);
        self.output_titles.push(title);
        *self.per_artist.entry(artist_folded).or_insert(0) += 1;
        *self.per_source.entry(group).or_insert(0) += 1;
        if is_familiar {
            self.familiar_taken += 1;
        }
        Verdict::Taken
    }
}

/// One spelling per artist for caps and spacing: the credit without its featured guests, as
/// seeding spells it.
pub fn artist_key(artist: &str) -> String {
    dotnet::to_lower_invariant(seed_normalizer::artist(artist).trim())
}

/// The seed a source list answers: "youtube-music:tag:trip hop" and "tag:trip hop" are one
/// seed's lists. Only a provider's own prefix is taken off.
pub fn seed_group(source: &str) -> &str {
    match source.find(':') {
        Some(colon) if colon > 0 && radio_provider::is_known(&source[..colon]) => &source[colon + 1..],
        _ => source,
    }
}

/// One weighted draw over the candidates (Efraimidis-Spirakis: each candidate draws
/// u^(1/weight) and the pool is sorted by that), so a track's chance of landing near the front
/// is proportional to its weight and every build draws differently. Tracks that were in this
/// station's previous snapshot keep [`PREVIOUS_SNAPSHOT_WEIGHT`] of their weight. Ties fall back
/// to the stable hash so equal draws are not order-of-arrival.
pub fn weighted_order(
    candidates: Vec<Candidate>,
    random: &mut dyn RadioRandom,
    previous: &IgnoreCaseKeys,
) -> Vec<Candidate> {
    let mut drawn: Vec<(Candidate, f64, String)> = candidates
        .into_iter()
        .map(|item| {
            let draw = random.next_double().powf(1.0 / weight(&item, previous));
            let order = stable_order(&item.track.artist, &item.track.title);
            (item, draw, order)
        })
        .collect();
    drawn.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.2.cmp(&b.2))
    });
    drawn.into_iter().map(|(item, _, _)| item).collect()
}

fn weight(item: &Candidate, previous: &IgnoreCaseKeys) -> f64 {
    let weight = item.weight.max(0.01);
    if previous.contains(&seed_normalizer::track_key(&item.track.artist, &item.track.title)) {
        weight * PREVIOUS_SNAPSHOT_WEIGHT
    } else {
        weight
    }
}

/// A station keeps only the tracks no earlier station has, unless that would leave it with
/// fewer than ten (or all it had).
pub fn suppress_station_overlap(stations: &mut [LastFmRadioStation]) {
    let mut used = IgnoreCaseKeys::new();
    for station in stations.iter_mut() {
        let unique: Vec<LastFmRadioTrack> = station
            .tracks
            .iter()
            .filter(|track| !used.contains(&seed_normalizer::track_key(&track.artist, &track.title)))
            .cloned()
            .collect();
        if unique.len() >= 10.min(station.tracks.len()) {
            station.tracks = unique;
        }
        for track in &station.tracks {
            used.insert(&seed_normalizer::track_key(&track.artist, &track.title));
        }
    }
}

/// Invariant-culture string order: case is compared after the letters, lower case first.
pub fn invariant_culture_compare(a: &str, b: &str) -> Ordering {
    dotnet::to_lower_invariant(a)
        .cmp(&dotnet::to_lower_invariant(b))
        .then_with(|| {
            for (x, y) in a.chars().zip(b.chars()) {
                if x != y {
                    // Lower case sorts before upper case at the tertiary level.
                    return match (dotnet::is_lower(x), dotnet::is_lower(y)) {
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        _ => x.cmp(&y),
                    };
                }
            }
            a.len().cmp(&b.len())
        })
}

/// Each artist's three most recent plays, by seed score: the strongest artists first, then by
/// name. Keyed by the primary artist as the first play spelled it.
pub fn score_artists(plays: &[LastFmRadioPlay], now: DateTime<Utc>) -> Vec<(String, f64)> {
    // Grouped ignoring case: the first spelling names the group, in first-seen order.
    let mut groups: IndexMap<String, (String, Vec<&LastFmRadioPlay>)> = IndexMap::new();
    for play in plays {
        let name = seed_normalizer::artist(&play.artist);
        groups
            .entry(dotnet::ordinal_ignore_case_key(&name))
            .or_insert_with(|| (name, Vec::new()))
            .1
            .push(play);
    }
    let mut scores: Vec<(String, f64)> = groups
        .into_values()
        .map(|(name, plays)| {
            let score = plays.iter().take(3).map(|play| seed_score(play, now)).sum();
            (name, score)
        })
        .collect();
    scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| invariant_culture_compare(&a.0, &b.0))
    });
    scores
}

/// Discovery Radio walks Last.fm's similar-artist graph out from the listener's
/// [`FRONTIER_SEEDS`] strongest artists, [`FRONTIER_NEIGHBOURS`] neighbours each, then one more
/// step from the [`SECOND_STEP_SEEDS`] best new artists at [`SECOND_STEP_WEIGHT`]. The
/// [`FRONTIER_ARTISTS`] best new artists give [`FRONTIER_TRACKS_EACH`] top songs each, and at
/// most [`FRONTIER_ARTIST_CAP`] of an artist's songs make the station. Under
/// [`FRONTIER_FEWEST_SONGS`] songs, the listener's tag charts stand in.
pub const FRONTIER_SEEDS: usize = 10;
pub const FRONTIER_NEIGHBOURS: i32 = 30;
pub const SECOND_STEP_SEEDS: usize = 5;
pub const SECOND_STEP_WEIGHT: f64 = 0.5;
pub const FRONTIER_ARTISTS: usize = 30;
pub const FRONTIER_TRACKS_EACH: i32 = 3;
pub const FRONTIER_ARTIST_CAP: i32 = 2;
pub const FRONTIER_FEWEST_SONGS: usize = 5;

/// Weight kept by an artist the listener already plays, among a genre station's suggestions:
/// a genre station is for finding, and the listener's own artists would otherwise lead it.
pub const KNOWN_ARTIST_WEIGHT: f64 = 0.3;

/// Neighbours an artist station asks Last.fm for; the five it uses are the strongest after
/// known artists keep [`KNOWN_ARTIST_WEIGHT`] of their match, so new names come first.
pub const ARTIST_RADIO_NEIGHBOURS: i32 = 15;

/// The artists the listener knows, by [`artist_key`]: everyone in their plays.
pub fn known_artists(plays: &[LastFmRadioPlay]) -> IgnoreCaseKeys {
    plays.iter().map(|play| artist_key(&play.artist)).collect()
}

/// Candidates by an artist the listener knows keep [`KNOWN_ARTIST_WEIGHT`] of their weight.
pub fn discount_known(candidates: Vec<Candidate>, known: &IgnoreCaseKeys) -> Vec<Candidate> {
    candidates
        .into_iter()
        .map(|mut candidate| {
            if known.contains(&artist_key(&candidate.track.artist)) {
                candidate.weight *= KNOWN_ARTIST_WEIGHT;
            }
            candidate
        })
        .collect()
}

/// An artist station's neighbours, strongest first once known artists keep
/// [`KNOWN_ARTIST_WEIGHT`] of their match; equal ones in Last.fm's order.
pub fn unknown_neighbours_first(similar: Vec<SimilarArtist>, known: &IgnoreCaseKeys) -> Vec<SimilarArtist> {
    let mut scored: Vec<(f64, SimilarArtist)> = similar
        .into_iter()
        .map(|artist| {
            let factor = if known.contains(&artist_key(&artist.name)) {
                KNOWN_ARTIST_WEIGHT
            } else {
                1.0
            };
            (artist.r#match * factor, artist)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
    scored.into_iter().map(|(_, artist)| artist).collect()
}

/// The new artists a similar-artist walk reached, each scored by the weight of the artist that
/// pointed to it times how similar Last.fm says they are, summed over every artist that did:
/// an artist several favorites point to leads. Known and disliked artists never score.
#[derive(Debug, Clone, Default)]
pub struct FrontierScores {
    scores: IndexMap<String, (String, f64)>,
}

impl FrontierScores {
    pub fn add(
        &mut self,
        similar: &[SimilarArtist],
        weight: f64,
        known: &IgnoreCaseKeys,
        disliked: &IgnoreCaseKeys,
    ) {
        for artist in similar {
            let key = artist_key(&artist.name);
            if key.is_empty() || known.contains(&key) || disliked.contains(&key) {
                continue;
            }
            self.scores
                .entry(dotnet::ordinal_ignore_case_key(&key))
                .or_insert_with(|| (artist.name.clone(), 0.0))
                .1 += weight * artist.r#match.max(0.05);
        }
    }

    /// The `n` best, strongest first, equal scores in the order they were first reached.
    pub fn top(&self, n: usize) -> Vec<(String, f64)> {
        let mut entries: Vec<(String, f64)> = self.scores.values().cloned().collect();
        entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        entries.truncate(n);
        entries
    }

    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }
}

/// Tag scores, keyed ignoring case, in the order the tags were first scored.
#[derive(Debug, Clone, Default)]
pub struct TagScores {
    scores: IndexMap<String, (String, f64)>,
}

impl TagScores {
    /// Adds to a tag's score under its canonical spelling, skipped when that is empty.
    pub fn add(&mut self, value: &str, score: f64) {
        let tag = canonical_tag(value);
        if tag.is_empty() {
            return;
        }
        self.scores
            .entry(dotnet::ordinal_ignore_case_key(&tag))
            .or_insert_with(|| (tag, 0.0))
            .1 += score;
    }

    /// The tags with their scores, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, f64)> {
        self.scores.values().map(|(tag, score)| (tag.as_str(), *score))
    }

    /// The `n` highest scores, equal scores by name: the discovery mix's tags.
    pub fn top_by_score_then_name(&self, n: usize) -> Vec<String> {
        let mut tags: Vec<(&str, f64)> = self.iter().collect();
        tags.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| invariant_culture_compare(a.0, b.0))
        });
        tags.into_iter().take(n).map(|(tag, _)| tag.to_string()).collect()
    }

    /// The `n` highest scores: the genre stations' tags, equal scores in the order they were
    /// first scored.
    pub fn top_by_score(&self, n: usize) -> Vec<String> {
        let mut tags: Vec<(&str, f64)> = self.iter().collect();
        tags.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        tags.into_iter().take(n).map(|(tag, _)| tag.to_string()).collect()
    }
}

/// The genres of the listener's plays, each worth two plays scaled by provenance.
pub fn score_local_tags(plays: &[LastFmRadioPlay]) -> TagScores {
    let mut tags = TagScores::default();
    for play in plays {
        if let Some(genre) = play.genre.as_deref().filter(|genre| !dotnet::is_blank(genre)) {
            tags.add(genre, 2.0 * source_weight(play));
        }
    }
    tags
}

/// A new station snapshot, valid for twelve hours until the build sets its own validity.
pub fn create<S: AsRef<str>>(
    username: &str,
    key: &str,
    name: &str,
    kind: LastFmRadioStationKind,
    personalized: bool,
    seeds: &[S],
    tracks: Vec<LastFmRadioTrack>,
    definition_version: i32,
    now: DateTime<Utc>,
) -> LastFmRadioStation {
    let mut distinct = IgnoreCaseKeys::new();
    LastFmRadioStation {
        id: station_id(username, key),
        key: key.to_string(),
        name: name.to_string(),
        owner: username.to_string(),
        kind,
        personalized,
        definition_version,
        created_utc: now,
        changed_utc: now,
        valid_until_utc: now + TimeDelta::hours(12),
        seeds: seeds
            .iter()
            .map(AsRef::as_ref)
            .filter(|seed| distinct.insert(seed))
            .map(str::to_string)
            .collect(),
        tracks,
        reserve: Vec::new(),
    }
}

/// A pinned station's version: the first four bytes of SHA-256 over its id, name and tags, so
/// editing a definition changes it.
pub fn definition_version(settings: &DiscoveryStationSettings) -> i32 {
    let text = format!("{}|{}|{}", settings.id, settings.name, settings.tags.join("|"));
    let hash = Sha256::digest(text.as_bytes());
    i32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]])
}

/// The first six bytes of SHA-256 over the lower-cased value, as lower-case hex: the station
/// key of an artist or genre station.
pub fn key(value: &str) -> String {
    let hash = Sha256::digest(dotnet::to_lower_invariant(value).as_bytes());
    hex::encode(&hash[..6])
}

/// The value lower-cased, then title-cased in the invariant culture.
pub fn title(value: &str) -> String {
    to_title_case(&dotnet::to_lower_invariant(value))
}

/// Whether radio has learned enough from the listener: their learned plays, each weighted by
/// where it came from ([`source_weight`]), add up to `LASTFM_MINIMUM_PLAYS`. Only then are
/// Your Radio, Discovery Radio and the artist and genre stations built.
pub fn is_learned(plays: &[LastFmRadioPlay], settings: &LastFmSettings) -> bool {
    let learned: f64 = plays
        .iter()
        .filter(|play| play.learned_signal)
        .map(source_weight)
        .sum();
    learned >= f64::from(settings.effective_minimum_plays())
}

/// The listener's songs that would not play and are still held out at `now`.
pub fn still_unavailable(user: &LastFmRadioUserState, now: DateTime<Utc>) -> IgnoreCaseKeys {
    user.unavailable_tracks
        .iter()
        .filter(|track| track.retry_after_utc > now)
        .map(|track| track.key.as_str())
        .collect()
}

/// The songs that must never be in or served from the listener's stations at `now`: the ones
/// they rated one star and the ones still held out because they would not play.
pub fn blocked_tracks(user: &LastFmRadioUserState, now: DateTime<Utc>) -> IgnoreCaseKeys {
    user.radio_bans
        .iter()
        .map(|ban| ban.key.as_str())
        .chain(
            user.unavailable_tracks
                .iter()
                .filter(|track| track.retry_after_utc > now)
                .map(|track| track.key.as_str()),
        )
        .collect()
}

/// Takes every song in `blocked` out of a station's tracks and its reserve. Answers whether the
/// tracks changed and whether the reserve changed.
pub fn strip_blocked(station: &mut LastFmRadioStation, blocked: &IgnoreCaseKeys) -> (bool, bool) {
    let allowed = |track: &LastFmRadioTrack| {
        !blocked.contains(&seed_normalizer::track_key(&track.artist, &track.title))
    };
    let tracks = station.tracks.len();
    station.tracks.retain(|track| allowed(track));
    let reserve = station.reserve.len();
    station.reserve.retain(|track| allowed(track));
    (station.tracks.len() != tracks, station.reserve.len() != reserve)
}

/// A station put back among stations it was not built with keeps only the songs none of them
/// has, unless that would leave it fewer than ten (or all it had), as
/// [`suppress_station_overlap`] does within a build. The others stay as they are.
pub fn trim_against(station: &mut LastFmRadioStation, others: &[LastFmRadioStation]) {
    let used: IgnoreCaseKeys = others
        .iter()
        .flat_map(|other| other.tracks.iter())
        .map(|track| seed_normalizer::track_key(&track.artist, &track.title))
        .collect();
    let unique: Vec<LastFmRadioTrack> = station
        .tracks
        .iter()
        .filter(|track| !used.contains(&seed_normalizer::track_key(&track.artist, &track.title)))
        .cloned()
        .collect();
    if unique.len() >= 10.min(station.tracks.len()) {
        station.tracks = unique;
    }
}

/// The longest station name the admin screens show by name in [`carried_note`]: the phone apps
/// print the note after their own words, in two lines at most.
pub const CARRIED_NAME_MOST: usize = 30;

/// What the admin screens say while a refresh kept stations as they were because their lookups
/// did not answer: one station by name when the name is short and plain, otherwise a count.
/// None for no station.
pub fn carried_note<S: AsRef<str>>(names: &[S]) -> Option<String> {
    match names {
        [] => None,
        [name] => {
            let name = name.as_ref().trim();
            let plain = !name.is_empty()
                && name.chars().count() <= CARRIED_NAME_MOST
                && !name.contains(['{', '[', '<', '\n', '\r'])
                && !name.contains("Exception");
            Some(if plain {
                format!("Couldn't update {name}, so it kept its songs")
            } else {
                "Couldn't update a station, so it kept its songs".to_string()
            })
        }
        _ => Some(format!(
            "Couldn't update {} stations, so they kept their songs",
            names.len()
        )),
    }
}

fn stable_order(artist: &str, title: &str) -> String {
    key(&format!("{artist}|{title}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference values.
    #[test]
    fn keys_titles_and_versions_match_the_reference_values() {
        assert_eq!(key("Björk"), "590a34408cfe");
        assert_eq!(key("Fresh"), "d098ab5e44b9");
        assert_eq!(key("A|T"), "f115f662b5f4");
        assert_eq!(title("hip-hop"), "Hip-Hop");
        assert_eq!(title("r&b"), "R&B");
        assert_eq!(title("alternative rock"), "Alternative Rock");
        let rock = DiscoveryStationSettings {
            id: "rock".into(),
            name: "Rock".into(),
            tags: vec!["rock".into()],
            ..Default::default()
        };
        assert_eq!(definition_version(&rock), 1653469986);
        let fusion = DiscoveryStationSettings {
            id: "fusion".into(),
            name: "Fusion".into(),
            tags: vec!["rock".into(), "idm".into()],
            ..Default::default()
        };
        assert_eq!(definition_version(&fusion), -596081354);
    }

    #[test]
    fn canonical_tags_apply_aliases_and_the_deny_list() {
        assert_eq!(canonical_tag(" Hip  Hop "), "hip-hop");
        assert_eq!(canonical_tag("Electronica"), "electronic");
        assert_eq!(canonical_tag("Seen Live"), "");
        assert_eq!(canonical_tag("RnB"), "r&b");
        assert_eq!(canonical_tag("Shoegaze"), "shoegaze");
    }

    #[test]
    fn caps_follow_the_station_size() {
        assert_eq!(source_cap(50, 6), 13);
        assert_eq!(source_cap(10, 1), 15);
        assert_eq!(source_cap(10, 20), 3);
        assert_eq!(source_cap(10, 0), 15);
        let settings = LastFmSettings {
            radio_track_count: 50,
            ..Default::default()
        };
        assert_eq!(artist_cap(&settings, LastFmRadioStationKind::Artist), 12);
        assert_eq!(artist_cap(&settings, LastFmRadioStationKind::YourMix), 5);
    }

    #[test]
    fn plays_from_the_radio_count_less() {
        let play = |source: &str| LastFmRadioPlay {
            source: source.into(),
            ..Default::default()
        };
        assert_eq!(source_weight(&play("internet-radio")), 0.4);
        assert_eq!(source_weight(&play("bootstrap-random")), 0.5);
        assert_eq!(source_weight(&play("scrobble")), 1.0);
    }

    /// A flat draw orders by weight; the previous snapshot falls behind fresh candidates.
    #[test]
    fn a_flat_draw_orders_by_weight_and_demotes_the_previous_snapshot() {
        struct Flat;
        impl RadioRandom for Flat {
            fn next_double(&mut self) -> f64 {
                0.5
            }
        }
        let candidates = ranked(
            (0..4)
                .map(|i| SimilarTrack::new(format!("A{i}"), format!("T{i}"), 1.0))
                .collect(),
            "tag:x",
            1.0,
        );
        let order: Vec<String> = weighted_order(candidates.clone(), &mut Flat, &IgnoreCaseKeys::new())
            .into_iter()
            .map(|c| c.track.title)
            .collect();
        assert_eq!(order, ["T0", "T1", "T2", "T3"]);
        let previous: IgnoreCaseKeys = [seed_normalizer::track_key("A0", "T0")].into_iter().collect();
        let order: Vec<String> = weighted_order(candidates, &mut Flat, &previous)
            .into_iter()
            .map(|c| c.track.title)
            .collect();
        assert_eq!(order, ["T1", "T2", "T3", "T0"]);
    }

    #[test]
    fn overlapping_stations_keep_only_new_tracks_when_enough_are_left() {
        let track = |artist: &str| LastFmRadioTrack {
            artist: artist.into(),
            title: "T".into(),
            ..Default::default()
        };
        let mut stations = vec![
            LastFmRadioStation {
                tracks: vec![track("A"), track("B")],
                ..Default::default()
            },
            LastFmRadioStation {
                tracks: vec![track("a"), track("C")],
                ..Default::default()
            },
        ];
        suppress_station_overlap(&mut stations);
        // Only one unique track of two: fewer than min(10, 2), so the second keeps both.
        assert_eq!(stations[1].tracks.len(), 2);
        let mut stations = vec![
            LastFmRadioStation {
                tracks: vec![track("A")],
                ..Default::default()
            },
            LastFmRadioStation {
                tracks: (0..12)
                    .map(|i| track(&format!("X{i}")))
                    .chain([track("A")])
                    .collect(),
                ..Default::default()
            },
        ];
        suppress_station_overlap(&mut stations);
        assert_eq!(stations[1].tracks.len(), 12);
    }

    /// Placement keeps the walk's gaps: with plenty of artists, no artist comes back within
    /// [`ARTIST_GAP`] songs of itself and no title within [`TITLE_GAP`], in any build.
    #[test]
    fn your_mix_keeps_its_artist_and_title_gaps_after_placement() {
        let settings = LastFmSettings {
            radio_track_count: 50,
            ..Default::default()
        };
        // Sixty artists with two songs each, and every title sung by two of them: enough that the walk
        // itself keeps every gap, as a real listener's many artists let it.
        let candidate = |artist: String, title: String, weight: f64, source: &str| Candidate {
            track: SimilarTrack::new(artist, title, 1.0),
            weight,
            source: source.to_string(),
        };
        let candidates: Vec<Candidate> = (0..60)
            .map(|i| {
                candidate(
                    format!("Familiar {}", i % 30),
                    format!("Song {}", i / 2),
                    2.0,
                    FAMILIAR_SOURCE,
                )
            })
            .chain(
                (0..60)
                    .map(|i| candidate(format!("New {}", i % 30), format!("Tune {}", i / 2), 0.3, "tag:x")),
            )
            .collect();
        let none = IgnoreCaseKeys::new();
        for seed in 0..100 {
            let tracks = shape(
                candidates.clone(),
                &[],
                &settings,
                &none,
                &mut DotnetRandom::new(seed),
                &none,
                artist_cap(&settings, LastFmRadioStationKind::YourMix),
                false,
                Some(32),
                None,
                None,
            );
            assert_eq!(tracks.len(), 50, "seed {seed}");
            let artists: Vec<String> = tracks.iter().map(|track| artist_key(&track.artist)).collect();
            let titles: Vec<String> = tracks
                .iter()
                .map(|track| dotnet::to_lower_invariant(&seed_normalizer::title(&track.title)))
                .collect();
            for i in 0..tracks.len() {
                for j in i + 1..tracks.len() {
                    assert!(
                        !(j - i <= ARTIST_GAP as usize && artists[i] == artists[j]),
                        "seed {seed}: {} at {i} and {j}",
                        artists[i]
                    );
                    assert!(
                        !(j - i <= TITLE_GAP && titles[i] == titles[j]),
                        "seed {seed}: {} at {i} and {j}",
                        titles[i]
                    );
                }
            }
        }
    }

    /// Your Radio places its familiar tracks and its discoveries at random at the discovery
    /// share, so the familiar ones do not gather at the front, and the order is not a fixed
    /// every-third-song schedule either.
    #[test]
    fn your_mix_mixes_familiar_and_discovery_throughout() {
        let settings = LastFmSettings {
            radio_track_count: 50,
            ..Default::default()
        };
        let candidate = |artist: String, title: String, weight: f64, source: &str| Candidate {
            track: SimilarTrack::new(artist, title, 1.0),
            weight,
            source: source.to_string(),
        };
        let candidates: Vec<Candidate> = (0..40)
            .map(|i| candidate(format!("Familiar {i}"), format!("Song {i}"), 2.0, FAMILIAR_SOURCE))
            .chain((0..40).map(|i| candidate(format!("New {i}"), format!("Tune {i}"), 0.3, "tag:x")))
            .collect();
        let none = IgnoreCaseKeys::new();
        let schedule = "FFD".repeat(17);
        let mut mixed_early = 0;
        for seed in 0..100 {
            let tracks = shape(
                candidates.clone(),
                &[],
                &settings,
                &none,
                &mut DotnetRandom::new(seed),
                &none,
                50,
                false,
                Some(32),
                None,
                None,
            );
            let pattern: String = tracks
                .iter()
                .map(|track| {
                    if track.artist.starts_with("Familiar") {
                        'F'
                    } else {
                        'D'
                    }
                })
                .collect();
            // Placement changes the order only: the quota still holds.
            assert_eq!(pattern.len(), 50, "seed {seed}: {pattern}");
            assert_eq!(pattern.matches('D').count(), 18, "seed {seed}: {pattern}");
            assert_ne!(pattern, schedule[..50], "seed {seed} reads the old schedule");
            if pattern[..10].matches('D').count() >= 2 {
                mixed_early += 1;
            }
        }
        assert!(
            mixed_early >= 90,
            "only {mixed_early} of 100 mixes had 2 discoveries in their first 10"
        );
    }

    /// A walk that falls short of its quota, 5 familiar tracks and 18 discoveries where Your
    /// Radio wants 32 and 18 of 50, is paced to what the walk took rather than at the quota's
    /// 36%: it ends on 6 discoveries in a row at most over 200 seeds, and no more than 2
    /// familiar tracks come in a row.
    #[test]
    fn a_short_your_mix_walk_ends_on_no_long_run_of_discoveries() {
        let settings = LastFmSettings {
            radio_track_count: 50,
            ..Default::default()
        };
        let candidate = |artist: String, title: String, weight: f64, source: &str| Candidate {
            track: SimilarTrack::new(artist, title, 1.0),
            weight,
            source: source.to_string(),
        };
        let candidates: Vec<Candidate> = (0..5)
            .map(|i| candidate(format!("Familiar {i}"), format!("Song {i}"), 2.0, FAMILIAR_SOURCE))
            .chain((0..18).map(|i| candidate(format!("New {i}"), format!("Tune {i}"), 0.3, "tag:x")))
            .collect();
        let none = IgnoreCaseKeys::new();
        for seed in 0..200 {
            let tracks = shape(
                candidates.clone(),
                &[],
                &settings,
                &none,
                &mut DotnetRandom::new(seed),
                &none,
                50,
                false,
                Some(32),
                None,
                None,
            );
            let pattern: String = tracks
                .iter()
                .map(|track| {
                    if track.artist.starts_with("Familiar") {
                        'F'
                    } else {
                        'D'
                    }
                })
                .collect();
            assert_eq!(pattern.len(), 23, "seed {seed}: {pattern}");
            let tail = pattern.len() - pattern.trim_end_matches('D').len();
            assert!(tail <= 6, "seed {seed} ends on {tail} discoveries: {pattern}");
            assert!(!pattern.contains("FFF"), "seed {seed}: {pattern}");
        }
    }

    /// The reserve is what the same walk takes next: the tracks are exactly what `shape` takes,
    /// and tracks and reserve together hold at most twice the station's size.
    #[test]
    fn the_reserve_follows_the_station_and_leaves_its_tracks_as_they_were() {
        let settings = LastFmSettings {
            radio_track_count: 20,
            ..Default::default()
        };
        let candidates: Vec<Candidate> = (0..80)
            .map(|i| Candidate {
                track: SimilarTrack::new(format!("Artist {}", i % 40), format!("Tune {i}"), 1.0),
                weight: 1.0 / f64::from(i + 1),
                source: format!("tag:{}", i % 3),
            })
            .collect();
        let none = IgnoreCaseKeys::new();
        for seed in 0..20 {
            let plain = shape(
                candidates.clone(),
                &[],
                &settings,
                &none,
                &mut DotnetRandom::new(seed),
                &none,
                2,
                true,
                None,
                None,
                None,
            );
            let shaped = shape_with_reserve(
                candidates.clone(),
                &[],
                &settings,
                &none,
                &mut DotnetRandom::new(seed),
                &none,
                2,
                true,
                None,
                None,
            );
            assert_eq!(shaped.tracks, plain, "seed {seed}");
            assert_eq!(shaped.tracks.len(), 20, "seed {seed}");
            assert!(!shaped.reserve.is_empty(), "seed {seed}");
            assert!(shaped.reserve.len() <= 20, "seed {seed}");
            let keys: IgnoreCaseKeys = shaped
                .tracks
                .iter()
                .chain(&shaped.reserve)
                .map(|track| seed_normalizer::track_key(&track.artist, &track.title))
                .collect();
            assert_eq!(
                keys.len(),
                shaped.tracks.len() + shaped.reserve.len(),
                "seed {seed}"
            );
        }
    }

    fn similar(names: &[(&str, f64)]) -> Vec<SimilarArtist> {
        names
            .iter()
            .map(|(name, r#match)| SimilarArtist {
                name: (*name).to_string(),
                r#match: *r#match,
            })
            .collect()
    }

    /// An artist two favorites point to beats one only the strongest points to, and known or
    /// disliked artists never score, however similar.
    #[test]
    fn the_frontier_scores_new_artists_by_who_points_to_them() {
        let known: IgnoreCaseKeys = ["known band"].into_iter().collect();
        let disliked: IgnoreCaseKeys = ["disliked band"].into_iter().collect();
        let mut scores = FrontierScores::default();
        scores.add(
            &similar(&[
                ("Known Band", 1.0),
                ("Solo Pick", 0.9),
                ("Shared Pick", 0.6),
                ("Disliked Band", 1.0),
            ]),
            1.0,
            &known,
            &disliked,
        );
        scores.add(&similar(&[("shared pick", 0.6)]), 0.8, &known, &disliked);
        let top = scores.top(10);
        let names: Vec<&str> = top.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["Shared Pick", "Solo Pick"]);
        assert!((top[0].1 - (0.6 + 0.48)).abs() < 1e-9);
        assert_eq!(scores.top(1).len(), 1);
    }

    #[test]
    fn known_artists_keep_a_third_of_their_weight_and_go_after_new_neighbours() {
        let plays = [LastFmRadioPlay {
            artist: "Known Band feat. Guest".into(),
            title: "Song".into(),
            ..Default::default()
        }];
        let known = known_artists(&plays);
        let candidates = discount_known(
            vec![
                Candidate {
                    track: SimilarTrack::new("KNOWN BAND", "Hit", 1.0),
                    weight: 1.0,
                    source: "tag:x".into(),
                },
                Candidate {
                    track: SimilarTrack::new("New Band", "Hit", 1.0),
                    weight: 1.0,
                    source: "tag:x".into(),
                },
            ],
            &known,
        );
        assert_eq!(candidates[0].weight, KNOWN_ARTIST_WEIGHT);
        assert_eq!(candidates[1].weight, 1.0);
        let order: Vec<String> = unknown_neighbours_first(
            similar(&[("Known Band", 0.9), ("New One", 0.5), ("New Two", 0.2)]),
            &known,
        )
        .into_iter()
        .map(|artist| artist.name)
        .collect();
        assert_eq!(order, ["New One", "Known Band", "New Two"]);
    }

    #[test]
    fn learned_counts_plays_by_where_they_came_from() {
        let settings = LastFmSettings {
            minimum_plays: 3,
            ..Default::default()
        };
        let play = |source: &str| LastFmRadioPlay {
            source: source.into(),
            ..Default::default()
        };
        assert!(!is_learned(&[play("scrobble"), play("scrobble")], &settings));
        assert!(is_learned(
            &[play("scrobble"), play("scrobble"), play("scrobble")],
            &settings
        ));
        // A song the radio played counts 0.4: seven make 2.8, eight make 3.2.
        assert!(!is_learned(&vec![play("internet-radio"); 7], &settings));
        assert!(is_learned(&vec![play("internet-radio"); 8], &settings));
        let unlearned = LastFmRadioPlay {
            learned_signal: false,
            ..play("scrobble")
        };
        assert!(!is_learned(&vec![unlearned; 5], &settings));
    }

    #[test]
    fn blocked_songs_are_the_one_star_ones_and_the_unplayable_ones_still_held_out() {
        use crate::models::radio::{LastFmRadioBan, LastFmRadioUnavailableTrack};
        let now = Utc::now();
        let key = seed_normalizer::track_key;
        let user = LastFmRadioUserState {
            radio_bans: vec![LastFmRadioBan {
                key: key("A", "Banned"),
                ..Default::default()
            }],
            unavailable_tracks: vec![
                LastFmRadioUnavailableTrack {
                    key: key("B", "Held"),
                    retry_after_utc: now + TimeDelta::minutes(5),
                    ..Default::default()
                },
                LastFmRadioUnavailableTrack {
                    key: key("C", "Free"),
                    retry_after_utc: now - TimeDelta::minutes(5),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let blocked = blocked_tracks(&user, now);
        assert!(blocked.contains(&key("A", "Banned")) && blocked.contains(&key("B", "Held")));
        assert!(!blocked.contains(&key("C", "Free")));
        let held = still_unavailable(&user, now);
        assert!(held.contains(&key("B", "Held")));
        assert!(!held.contains(&key("A", "Banned")) && !held.contains(&key("C", "Free")));
    }

    #[test]
    fn strip_blocked_says_which_list_changed() {
        let track = |title: &str| LastFmRadioTrack {
            artist: "A".into(),
            title: title.into(),
            ..Default::default()
        };
        let mut station = LastFmRadioStation {
            tracks: vec![track("One"), track("Two")],
            reserve: vec![track("Three")],
            ..Default::default()
        };
        let blocked: IgnoreCaseKeys = [seed_normalizer::track_key("A", "Three")].into_iter().collect();
        assert_eq!(strip_blocked(&mut station, &blocked), (false, true));
        assert!(station.reserve.is_empty());
        let blocked: IgnoreCaseKeys = [seed_normalizer::track_key("a", "ONE")].into_iter().collect();
        assert_eq!(strip_blocked(&mut station, &blocked), (true, false));
        assert_eq!(station.tracks, vec![track("Two")]);
        assert_eq!(strip_blocked(&mut station, &blocked), (false, false));
    }

    #[test]
    fn a_station_put_back_keeps_only_songs_the_stations_beside_it_lack_unless_too_few_are_left() {
        let tracks = |prefix: &str, count: usize| -> Vec<LastFmRadioTrack> {
            (0..count)
                .map(|index| LastFmRadioTrack {
                    artist: "A".into(),
                    title: format!("{prefix}{index}"),
                    ..Default::default()
                })
                .collect()
        };
        let other = LastFmRadioStation {
            tracks: tracks("shared", 3),
            ..Default::default()
        };
        let mut station = LastFmRadioStation {
            tracks: [tracks("shared", 3), tracks("own", 12)].concat(),
            ..Default::default()
        };
        trim_against(&mut station, std::slice::from_ref(&other));
        assert_eq!(station.tracks, tracks("own", 12));
        assert_eq!(other.tracks.len(), 3, "the stations beside it stay as they are");
        let mut short = LastFmRadioStation {
            tracks: [tracks("shared", 3), tracks("own", 5)].concat(),
            ..Default::default()
        };
        trim_against(&mut short, std::slice::from_ref(&other));
        assert_eq!(
            short.tracks.len(),
            8,
            "fewer than ten would be left, so it stays whole"
        );
    }

    #[test]
    fn the_kept_note_names_one_plain_station_and_counts_more() {
        assert_eq!(carried_note::<&str>(&[]), None);
        assert_eq!(
            carried_note(&["Discovery Radio"]).as_deref(),
            Some("Couldn't update Discovery Radio, so it kept its songs")
        );
        assert_eq!(
            carried_note(&["Discovery Radio", "Rock Radio"]).as_deref(),
            Some("Couldn't update 2 stations, so they kept their songs")
        );
        for odd in [
            "A Name Longer Than Thirty Chars Radio",
            "{weird} Radio",
            "[x] Radio",
            "<b> Radio",
            "Exception Radio",
            "Two\nLines",
            "   ",
        ] {
            assert_eq!(
                carried_note(&[odd]).as_deref(),
                Some("Couldn't update a station, so it kept its songs"),
                "{odd:?}"
            );
        }
        let thirty = format!("{} Radio", "A".repeat(24));
        assert_eq!(thirty.chars().count(), CARRIED_NAME_MOST);
        assert!(carried_note(&[thirty.as_str()]).unwrap().contains(&thirty));
    }

    #[test]
    fn invariant_culture_order_puts_case_after_letters() {
        assert_eq!(invariant_culture_compare("a", "B"), Ordering::Less);
        assert_eq!(invariant_culture_compare("b", "A"), Ordering::Greater);
        assert_eq!(invariant_culture_compare("a", "A"), Ordering::Less);
        assert_eq!(invariant_culture_compare("rock", "rock"), Ordering::Equal);
    }
}
