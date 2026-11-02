//! The Last.fm radio state store: `lastfm-radio-state.json`.
//!
//! Bounded local state for Last.fm radio. This follows DownloadHistoryService's locked cache +
//! temporary-file rename pattern and is deliberately single-writer. The station ids and the
//! bounds are `octo_core::last_fm::last_fm_radio_state_store`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::{Clock, dotnet};
use octo_core::last_fm::last_fm_radio_recommendation_service::{
    IgnoreCaseKeys, blocked_tracks, strip_blocked,
};
use octo_core::last_fm::last_fm_radio_seed_normalizer as seed_normalizer;
use octo_core::last_fm::last_fm_radio_state_store::{
    CURRENT_VERSION, DUPLICATE_WINDOW, MAX_PLAYS_PER_USER, MAX_RADIO_BANS_PER_USER,
    MAX_UNAVAILABLE_TRACKS_PER_USER, MAX_USERS, UNAVAILABLE_TRACK_COOLDOWN, user_key,
};
use octo_core::models::radio::{
    LastFmRadioBan, LastFmRadioPlay, LastFmRadioStateDocument, LastFmRadioStation, LastFmRadioTrack,
    LastFmRadioUnavailableTrack, LastFmRadioUserState, LastFmRadioUserSummary,
};
use octo_core::settings::SettingsStore;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use serde::Serialize;
use serde::ser::SerializeMap;
use tracing::warn;

use crate::services::framework::DotnetDictionary;
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};
use crate::services::state_file;

pub use octo_core::last_fm::last_fm_radio_state_store::{station_id, to_base62};

/// The loaded document. `users` is keyed by the ignore-case key, each entry holding the key as
/// it was written and the user, in the order a [`DotnetDictionary`] enumerates.
struct Document {
    version: i32,
    users: DotnetDictionary<(String, LastFmRadioUserState)>,
}

impl Document {
    fn empty() -> Self {
        Document {
            version: CURRENT_VERSION,
            users: DotnetDictionary::new(),
        }
    }

    fn get(&self, key: &str) -> Option<&LastFmRadioUserState> {
        self.users
            .get(&dotnet::ordinal_ignore_case_key(key))
            .map(|(_, user)| user)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut LastFmRadioUserState> {
        self.users
            .get_mut(&dotnet::ordinal_ignore_case_key(key))
            .map(|(_, user)| user)
    }
}

/// Writes the document as `LastFmRadioStateDocument` would be written: `Version`, then `Users`
/// in dictionary order.
impl Serialize for Document {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Users<'a>(&'a DotnetDictionary<(String, LastFmRadioUserState)>);
        impl Serialize for Users<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut map = serializer.serialize_map(Some(self.0.len()))?;
                for (key, user) in self.0.values() {
                    map.serialize_entry(key, user)?;
                }
                map.end()
            }
        }
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("Version", &self.version)?;
        map.serialize_entry("Users", &Users(&self.users))?;
        map.end()
    }
}

pub struct LastFmRadioStateStore {
    path: PathBuf,
    /// The Last.fm settings: the retention is read at every prune.
    settings: Arc<SettingsStore>,
    registry: Arc<ExternalIdRegistry>,
    clock: Clock,
    state: Mutex<Option<Document>>,
}

impl LastFmRadioStateStore {
    pub fn new(
        path: impl Into<PathBuf>,
        settings: Arc<SettingsStore>,
        registry: Arc<ExternalIdRegistry>,
    ) -> Self {
        Self::with_clock(path, settings, registry, Clock::system())
    }

    /// The store on another clock, for tests.
    pub fn with_clock(
        path: impl Into<PathBuf>,
        settings: Arc<SettingsStore>,
        registry: Arc<ExternalIdRegistry>,
        clock: Clock,
    ) -> Self {
        LastFmRadioStateStore {
            path: path.into(),
            settings,
            registry,
            clock,
            state: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record_play(&self, username: &str, mut play: LastFmRadioPlay) -> bool {
        if dotnet::is_blank(username) || dotnet::is_blank(&play.artist) || dotnet::is_blank(&play.title) {
            return false;
        }
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        let key = seed_normalizer::track_key(&play.artist, &play.title);
        let duplicate = user.plays.iter().any(|existing| {
            seed_normalizer::track_key(&existing.artist, &existing.title) == key
                && total_minutes(existing.played_at_utc - play.played_at_utc).abs()
                    <= total_minutes(DUPLICATE_WINDOW)
        });
        if duplicate {
            return false;
        }

        play.artist = seed_normalizer::artist(&play.artist);
        play.title = seed_normalizer::title(&play.title);
        user.plays.insert(0, play);
        user.new_plays_since_refresh += 1;
        user.last_seen_utc = now;
        self.prune_locked(state, now);
        self.save_locked(state);
        true
    }

    /// A copy of the listener's state, or an empty one named `username`.
    pub fn get_user(&self, username: &str) -> LastFmRadioUserState {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        match state.get(&user_key(username)) {
            Some(user) => user.clone(),
            None => LastFmRadioUserState {
                username: username.to_string(),
                ..Default::default()
            },
        }
    }

    pub fn mark_heart(&self, username: &str, song_id: &str, artist: &str, title: &str) -> bool {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let Some(user) = state.get_mut(&user_key(username)) else {
            return false;
        };
        let key = seed_normalizer::track_key(artist, title);
        let Some(play) = user.plays.iter_mut().find(|item| {
            item.song_id == song_id || seed_normalizer::track_key(&item.artist, &item.title) == key
        }) else {
            return false;
        };
        if play.hearted {
            return false;
        }
        play.hearted = true;
        self.save_locked(state);
        true
    }

    pub fn get_summaries(&self) -> Vec<LastFmRadioUserSummary> {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let mut users: Vec<&LastFmRadioUserState> = state.users.values().map(|(_, user)| user).collect();
        users.sort_by_key(|a| std::cmp::Reverse(a.last_seen_utc));
        users
            .into_iter()
            .map(|user| LastFmRadioUserSummary {
                username: user.username.clone(),
                play_count: user.plays.len() as i32,
                station_count: user.stations.len() as i32,
                new_plays_since_refresh: user.new_plays_since_refresh,
                last_refresh_success_utc: user.last_refresh_success_utc,
                last_refresh_error: user.last_refresh_error.clone(),
                refreshing: user.refreshing,
            })
            .collect()
    }

    /// Listeners radio builds for: everyone but a listener cleared from the dashboard who has
    /// not played since, who has nothing to build from.
    pub fn known_users(&self) -> Vec<String> {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        state
            .users
            .values()
            .filter_map(|(_, user)| {
                let idle = user.history_cleared_utc.is_some() && user.plays.is_empty();
                (!idle).then(|| user.username.clone())
            })
            .collect()
    }

    /// Whether a starting profile may be learned for the listener: they have no plays and were
    /// never cleared from the dashboard. Read in place, without copying the listener.
    pub fn wants_starting_profile(&self, username: &str) -> bool {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        state
            .get(&user_key(username))
            .is_none_or(|user| user.plays.is_empty() && user.history_cleared_utc.is_none())
    }

    pub fn find_station(&self, username: &str, station_id: &str) -> Option<LastFmRadioStation> {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        state
            .get(&user_key(username))?
            .stations
            .iter()
            .find(|item| item.id == station_id)
            .cloned()
    }

    /// Removes an unplayable song from every current station and prevents an immediate
    /// deterministic refresh from selecting it again.
    pub fn reject_track(
        &self,
        username: &str,
        track: &LastFmRadioTrack,
        now_utc: Option<DateTime<Utc>>,
    ) -> usize {
        if dotnet::is_blank(username) || dotnet::is_blank(&track.artist) || dotnet::is_blank(&track.title) {
            return 0;
        }
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = now_utc.unwrap_or_else(|| self.clock.now());
        let user = get_or_create(state, username, self.clock.now());
        let key = seed_normalizer::track_key(&track.artist, &track.title);
        let blocked: IgnoreCaseKeys = std::iter::once(key.as_str()).collect();
        // Counts listed songs only: a song gone from a reserve leaves the station as full.
        let mut removed = 0;
        for station in &mut user.stations {
            let before = station.tracks.len();
            let (tracks, reserve) = strip_blocked(station, &blocked);
            removed += before - station.tracks.len();
            if tracks || reserve {
                station.changed_utc = now;
            }
        }
        drop_emptied(user);
        let mut unavailable: Vec<LastFmRadioUnavailableTrack> =
            std::iter::once(LastFmRadioUnavailableTrack {
                key: key.clone(),
                artist: track.artist.clone(),
                title: track.title.clone(),
                failed_at_utc: now,
                retry_after_utc: now + UNAVAILABLE_TRACK_COOLDOWN,
            })
            .chain(
                std::mem::take(&mut user.unavailable_tracks)
                    .into_iter()
                    .filter(|item| item.retry_after_utc > now && item.key != key),
            )
            .collect();
        unavailable.sort_by_key(|a| std::cmp::Reverse(a.failed_at_utc));
        unavailable.truncate(MAX_UNAVAILABLE_TRACKS_PER_USER);
        user.unavailable_tracks = unavailable;
        user.last_seen_utc = now;
        self.save_locked(state);
        removed
    }

    /// A song rated one star never plays on this user's radio again and leaves the stations it is
    /// on now; any other rating lifts that. Returns whether anything changed.
    pub fn set_radio_ban(
        &self,
        username: &str,
        song_id: &str,
        artist: &str,
        title: &str,
        banned: bool,
    ) -> bool {
        if dotnet::is_blank(username) || dotnet::is_blank(artist) || dotnet::is_blank(title) {
            return false;
        }
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        if !banned && state.get(&user_key(username)).is_none() {
            return false;
        }
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        let key = seed_normalizer::track_key(artist, title);
        let before = user.radio_bans.len();
        user.radio_bans
            .retain(|ban| !(ban.key == key || (!song_id.is_empty() && ban.song_id == song_id)));
        let had = user.radio_bans.len() != before;
        if banned {
            user.radio_bans.insert(
                0,
                LastFmRadioBan {
                    key: key.clone(),
                    song_id: song_id.to_string(),
                    artist: artist.to_string(),
                    title: title.to_string(),
                    banned_at_utc: now,
                },
            );
            let blocked: IgnoreCaseKeys = std::iter::once(key.as_str()).collect();
            for station in &mut user.stations {
                let (tracks, reserve) = strip_blocked(station, &blocked);
                if tracks || reserve {
                    station.changed_utc = now;
                }
            }
            drop_emptied(user);
        } else if !had {
            return false;
        }
        self.save_locked(state);
        true
    }

    /// The keys of the songs this user rated one star, without copying the rest of their state.
    pub fn radio_ban_keys(&self, username: &str) -> IgnoreCaseKeys {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        state
            .get(&user_key(username))
            .map(|user| user.radio_bans.iter().map(|ban| ban.key.as_str()).collect())
            .unwrap_or_default()
    }

    pub fn replace_stations(&self, username: &str, stations: &[LastFmRadioStation]) {
        self.replace_stations_carrying(username, stations, &[]);
    }

    /// A full refresh's stations. `carried` holds the keys of the ones it kept as they were
    /// because their lookups did not answer; they are tried again on their own.
    pub fn replace_stations_carrying(
        &self,
        username: &str,
        stations: &[LastFmRadioStation],
        carried: &[String],
    ) {
        self.save_stations(username, stations, carried, Stamps::Refresh);
    }

    /// A retry of carried stations: saved like a refresh, but the refresh schedule is left as it
    /// was, so the stations it did not touch are drawn again on time.
    pub fn replace_carried(&self, username: &str, stations: &[LastFmRadioStation], carried: &[String]) {
        self.save_stations(username, stations, carried, Stamps::Retry);
    }

    /// Forgets which stations were carried, for a listener nothing will try them again for.
    pub fn clear_carried(&self, username: &str) {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let Some(user) = state.get_mut(&user_key(username)) else {
            return;
        };
        if user.carried_station_keys.is_empty() {
            return;
        }
        user.carried_station_keys.clear();
        self.save_locked(state);
    }

    fn save_stations(
        &self,
        username: &str,
        stations: &[LastFmRadioStation],
        carried: &[String],
        stamps: Stamps,
    ) {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        // Every refresh stamps its start (`mark_refreshing`). One that started before the
        // listener was cleared built from the history that was dropped, so its result is
        // discarded.
        if let Some(cleared) = user.history_cleared_utc
            && user
                .last_refresh_attempt_utc
                .is_none_or(|started| started <= cleared)
        {
            return;
        }
        // Songs rated one star or found unplayable while the refresh ran stay out, and a station
        // left with no song is not kept.
        let blocked = blocked_tracks(user, now);
        let existing = std::mem::take(&mut user.stations);
        let prior_of = |id: &str| existing.iter().find(|station| station.id == id);
        user.stations = stations
            .iter()
            .filter_map(|source| {
                let mut station = source.clone();
                strip_blocked(&mut station, &blocked);
                if station.tracks.is_empty() {
                    return None;
                }
                if let Some(prior) = prior_of(&station.id) {
                    station.created_utc = prior.created_utc;
                    if same_snapshot(prior, &station) {
                        station.changed_utc = prior.changed_utc;
                    } else if station.changed_utc < now {
                        // A copy that changed since it was read says so, even one made earlier.
                        station.changed_utc = now;
                    }
                }
                Some(station)
            })
            .collect();
        user.carried_station_keys = carried
            .iter()
            .filter(|key| {
                user.stations
                    .iter()
                    .any(|station| dotnet::eq_ignore_case(&station.key, key))
            })
            .cloned()
            .collect();
        match stamps {
            Stamps::Refresh => {
                user.new_plays_since_refresh = 0;
                user.last_refresh_success_utc = Some(now);
                user.last_refresh_attempt_utc = user.last_refresh_success_utc;
            }
            Stamps::Retry => user.last_refresh_attempt_utc = Some(now),
        }
        user.last_refresh_error = None;
        user.refreshing = false;
        user.last_seen_utc = now;
        rehydrate_routes(&self.registry, user);
        self.save_locked(state);
    }

    pub fn mark_refreshing(&self, username: &str) {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        user.refreshing = true;
        user.last_refresh_attempt_utc = Some(now);
        user.last_refresh_error = None;
        self.save_locked(state);
    }

    pub fn mark_refresh_failed(&self, username: &str, message: &str) {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        user.refreshing = false;
        user.last_refresh_attempt_utc = Some(now);
        user.last_refresh_error = Some(first_utf16_units(message, 500));
        self.save_locked(state);
    }

    /// Clears everything held for a listener (plays, stations, unplayable tracks, bans) but
    /// keeps the entry, stamped `history_cleared_utc`. Dropping the entry instead let the next
    /// playlist listing learn a starting profile from the listener's stars, which brought the
    /// history back at once. False for an unknown listener.
    pub fn reset(&self, username: &str) -> bool {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = self.clock.now();
        let Some(user) = state.get_mut(&user_key(username)) else {
            return false;
        };
        let name = std::mem::take(&mut user.username);
        *user = LastFmRadioUserState::default();
        user.username = name;
        user.last_seen_utc = now;
        user.history_cleared_utc = Some(now);
        self.save_locked(state);
        true
    }

    fn load_locked<'a>(&self, guard: &'a mut Option<Document>) -> &'a mut Document {
        if guard.is_none() {
            *guard = Some(self.load());
        }
        guard.as_mut().expect("loaded above")
    }

    fn load(&self) -> Document {
        let text = match self.path.is_file() {
            false => return Document::empty(),
            true => state_file::read_all_text(&self.path),
        };
        let parsed = text.map_err(|e| e.to_string()).and_then(|text| {
            serde_json::from_str::<LastFmRadioStateDocument>(&text).map_err(|e| e.to_string())
        });
        let mut document = match parsed {
            Ok(document) => document,
            Err(message) => {
                warn!("Last.fm radio state load failed ({message}); starting clean");
                return Document::empty();
            }
        };
        if document.version != CURRENT_VERSION {
            warn!(
                "Unsupported Last.fm radio state version {}; starting clean",
                document.version
            );
            document = LastFmRadioStateDocument::default();
        }
        // Keyed ignoring case: two keys that differ only in case fail the load, and it starts
        // clean.
        let mut state = Document {
            version: document.version,
            users: DotnetDictionary::new(),
        };
        for (key, user) in document.users {
            let folded = dotnet::ordinal_ignore_case_key(&key);
            if state.users.contains_key(&folded) {
                warn!(
                    "Last.fm radio state load failed (An item with the same key has already been added. Key: {key}); starting clean"
                );
                return Document::empty();
            }
            // No refresh outlives the process that ran it.
            let mut user = user;
            user.refreshing = false;
            state.users.set(folded, (key, user));
        }
        self.prune_locked(&mut state, self.clock.now());
        for (_, user) in state.users.values_mut() {
            rehydrate_routes(&self.registry, user);
        }
        state
    }

    fn prune_locked(&self, state: &mut Document, now: DateTime<Utc>) {
        let retention = self.settings.current().last_fm.effective_history_retention_days();
        let cutoff = now - TimeDelta::days(i64::from(retention));
        for (_, user) in state.users.values_mut() {
            let mut plays: Vec<LastFmRadioPlay> = std::mem::take(&mut user.plays)
                .into_iter()
                .filter(|play| play.played_at_utc >= cutoff)
                .collect();
            plays.sort_by_key(|a| std::cmp::Reverse(a.played_at_utc));
            plays.truncate(MAX_PLAYS_PER_USER);
            user.plays = plays;
            let mut unavailable: Vec<LastFmRadioUnavailableTrack> =
                std::mem::take(&mut user.unavailable_tracks)
                    .into_iter()
                    .filter(|track| track.retry_after_utc > now)
                    .collect();
            unavailable.sort_by_key(|a| std::cmp::Reverse(a.failed_at_utc));
            unavailable.truncate(MAX_UNAVAILABLE_TRACKS_PER_USER);
            user.unavailable_tracks = unavailable;
            user.radio_bans
                .sort_by_key(|a| std::cmp::Reverse(a.banned_at_utc));
            user.radio_bans.truncate(MAX_RADIO_BANS_PER_USER);
        }

        let mut by_last_seen: Vec<(String, DateTime<Utc>)> = state
            .users
            .iter()
            .map(|(key, (_, user))| (key.clone(), user.last_seen_utc))
            .collect();
        by_last_seen.sort_by_key(|a| std::cmp::Reverse(a.1));
        for (key, _) in by_last_seen.into_iter().skip(MAX_USERS) {
            state.users.remove(&key);
        }
    }

    fn save_locked(&self, state: &Document) {
        let json = octo_core::json::to_string_indented(state);
        if let Err(error) = state_file::save_atomic(&self.path, &json) {
            warn!("Last.fm radio state save failed: {error}");
        }
    }
}

fn get_or_create<'a>(
    state: &'a mut Document,
    username: &str,
    now: DateTime<Utc>,
) -> &'a mut LastFmRadioUserState {
    let key = user_key(username);
    let folded = dotnet::ordinal_ignore_case_key(&key);
    if !state.users.contains_key(&folded) {
        let user = LastFmRadioUserState {
            username: username.trim().to_string(),
            last_seen_utc: now,
            ..Default::default()
        };
        state.users.set(folded.clone(), (key, user));
    }
    state
        .users
        .get_mut(&folded)
        .map(|(_, user)| user)
        .expect("added above")
}

/// After a load and after every install, the external tracks are registered again so their
/// short ids resolve: the registry may have forgotten them.
fn rehydrate_routes(registry: &ExternalIdRegistry, user: &mut LastFmRadioUserState) {
    for track in user
        .stations
        .iter_mut()
        .flat_map(|station| station.tracks.iter_mut())
        .filter(|track| !track.is_local)
    {
        route_track(registry, track);
    }
}

/// Gives an outside track the short id of its external route, registered now, and the provider
/// that plays it when it names none.
pub fn route_track(registry: &ExternalIdRegistry, track: &mut LastFmRadioTrack) {
    track.resolved_id = Some(registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some(track.artist.clone()),
        title: Some(track.title.clone()),
        album: track.album.clone(),
        duration: track.duration,
        you_tube_id: track.you_tube_id.clone(),
        ..Default::default()
    }));
    if track.external_provider.is_none() {
        track.external_provider = Some(SoulseekMetadataService::PROVIDER_NAME.to_string());
    }
}

/// Whether two copies of a station hold the same songs. The reserve counts: Discovery Radio
/// serves reserve songs in place of library songs past its share.
fn same_snapshot(left: &LastFmRadioStation, right: &LastFmRadioStation) -> bool {
    let keys = |tracks: &[LastFmRadioTrack]| -> Vec<String> {
        tracks
            .iter()
            .map(|track| seed_normalizer::track_key(&track.artist, &track.title))
            .collect()
    };
    left.definition_version == right.definition_version
        && left.name == right.name
        && keys(&left.tracks) == keys(&right.tracks)
        && keys(&left.reserve) == keys(&right.reserve)
}

/// Which stamps a station save writes.
#[derive(Debug, Clone, Copy)]
enum Stamps {
    /// A refresh: the success that the next scheduled one counts from.
    Refresh,
    /// A retry of carried stations: only the attempt.
    Retry,
}

/// Leaves out the stations that no longer hold a song, and forgets that any of them was carried.
fn drop_emptied(user: &mut LastFmRadioUserState) {
    user.stations.retain(|station| !station.tracks.is_empty());
    let stations = &user.stations;
    user.carried_station_keys.retain(|key| {
        stations
            .iter()
            .any(|station| dotnet::eq_ignore_case(&station.key, key))
    });
}

/// The span in fractional minutes.
fn total_minutes(span: TimeDelta) -> f64 {
    span.num_microseconds().map_or_else(
        || span.num_seconds() as f64 / 60.0,
        |micros| micros as f64 / 60_000_000.0,
    )
}

/// `message[..500]` when longer: the first 500 UTF-16 code units.
fn first_utf16_units(message: &str, units: usize) -> String {
    if dotnet::utf16_len(message) <= units {
        return message.to_string();
    }
    let utf16: Vec<u16> = message.encode_utf16().take(units).collect();
    String::from_utf16_lossy(&utf16)
}

#[cfg(test)]
#[path = "last_fm_radio_state_store_tests.rs"]
mod tests;

#[cfg(test)]
mod carried_saves {
    use octo_core::settings::AppSettings;

    use super::*;

    fn store(dir: &tempfile::TempDir) -> LastFmRadioStateStore {
        LastFmRadioStateStore::new(
            dir.path().join("state.json"),
            Arc::new(SettingsStore::for_tests(AppSettings::default())),
            Arc::new(ExternalIdRegistry::in_memory()),
        )
    }

    fn station(key: &str) -> LastFmRadioStation {
        LastFmRadioStation {
            id: key.into(),
            key: key.into(),
            tracks: vec![LastFmRadioTrack {
                artist: "A".into(),
                title: key.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A retry of carried stations begun before a history reset saves nothing.
    #[test]
    fn a_retry_begun_before_a_reset_saves_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(&dir);
        store.replace_stations_carrying("alice", &[station("discovery")], &["discovery".into()]);
        store.mark_refreshing("alice");
        assert!(store.reset("alice"));
        store.replace_carried("alice", &[station("discovery")], &["discovery".into()]);
        let user = store.get_user("alice");
        assert!(user.stations.is_empty());
        assert!(user.carried_station_keys.is_empty());
    }

    /// The carried keys are those of saved stations, and a retry leaves the success stamp alone.
    #[test]
    fn carried_keys_follow_the_saved_stations_and_a_retry_keeps_the_schedule() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(&dir);
        store.replace_stations_carrying(
            "alice",
            &[station("discovery"), station("rock")],
            &["discovery".into(), "gone".into()],
        );
        let user = store.get_user("alice");
        assert_eq!(user.carried_station_keys, ["discovery"]);
        let succeeded = user.last_refresh_success_utc;
        assert!(succeeded.is_some());

        store.mark_refreshing("alice");
        store.replace_carried("alice", &[station("discovery"), station("rock")], &[]);
        let user = store.get_user("alice");
        assert!(user.carried_station_keys.is_empty());
        assert_eq!(user.last_refresh_success_utc, succeeded);
        assert!(!user.refreshing);

        store.replace_stations_carrying("alice", &[station("discovery")], &["discovery".into()]);
        assert!(store.set_radio_ban("alice", "", "A", "discovery", true));
        let user = store.get_user("alice");
        assert!(user.stations.is_empty(), "the only song was rated one star");
        assert!(user.carried_station_keys.is_empty());
    }
}
