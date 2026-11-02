//! What `lastfm-radio-state.json` holds, per listener, for Last.fm radio, and the dashboard's
//! summary of a listener.
//!
//! The file is written indented by `LastFmRadioStateStore` (`octo::services::last_fm`). Field
//! names are PascalCase; each field's default is what a missing field reads as, including the
//! ones that are "now" (the current UTC time).

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::json::datetime;
use crate::models::null_as_default;

/// The whole file: a version and the listeners, keyed by trimmed lower-cased username.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioStateDocument {
    pub version: i32,
    #[serde(deserialize_with = "null_as_default")]
    pub users: IndexMap<String, LastFmRadioUserState>,
}

impl Default for LastFmRadioStateDocument {
    fn default() -> Self {
        LastFmRadioStateDocument {
            version: 1,
            users: IndexMap::new(),
        }
    }
}

/// One listener's plays, stations and unplayable tracks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioUserState {
    #[serde(deserialize_with = "null_as_default")]
    pub username: String,
    #[serde(with = "datetime::utc")]
    pub last_seen_utc: DateTime<Utc>,
    pub new_plays_since_refresh: i32,
    #[serde(deserialize_with = "null_as_default")]
    pub plays: Vec<LastFmRadioPlay>,
    #[serde(deserialize_with = "null_as_default")]
    pub stations: Vec<LastFmRadioStation>,
    #[serde(deserialize_with = "null_as_default")]
    pub unavailable_tracks: Vec<LastFmRadioUnavailableTrack>,
    /// Songs the listener rated one star: they never play on their radio.
    #[serde(deserialize_with = "null_as_default")]
    pub radio_bans: Vec<LastFmRadioBan>,
    #[serde(with = "datetime::utc_option")]
    pub last_refresh_attempt_utc: Option<DateTime<Utc>>,
    #[serde(with = "datetime::utc_option")]
    pub last_refresh_success_utc: Option<DateTime<Utc>>,
    pub last_refresh_error: Option<String>,
    pub refreshing: bool,
    /// When the dashboard last cleared this listener's history. From then on radio learns only
    /// from new plays, never from a starting profile. Absent from the file until set.
    #[serde(with = "datetime::utc_option", skip_serializing_if = "Option::is_none")]
    pub history_cleared_utc: Option<DateTime<Utc>>,
    /// The keys of the stations the last refresh kept as they were because their lookups did not
    /// answer. Each is tried again on its own until it is made or no longer wanted. Absent from
    /// the file while empty.
    #[serde(deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub carried_station_keys: Vec<String>,
}

impl Default for LastFmRadioUserState {
    fn default() -> Self {
        LastFmRadioUserState {
            username: String::new(),
            last_seen_utc: Utc::now(),
            new_plays_since_refresh: 0,
            plays: Vec::new(),
            stations: Vec::new(),
            unavailable_tracks: Vec::new(),
            radio_bans: Vec::new(),
            last_refresh_attempt_utc: None,
            last_refresh_success_utc: None,
            last_refresh_error: None,
            refreshing: false,
            history_cleared_utc: None,
            carried_station_keys: Vec::new(),
        }
    }
}

/// A track that failed to play, held out of the listener's stations until `retry_after_utc`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioUnavailableTrack {
    #[serde(deserialize_with = "null_as_default")]
    pub key: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    #[serde(with = "datetime::utc")]
    pub failed_at_utc: DateTime<Utc>,
    #[serde(with = "datetime::utc")]
    pub retry_after_utc: DateTime<Utc>,
}

impl Default for LastFmRadioUnavailableTrack {
    fn default() -> Self {
        let now = Utc::now();
        LastFmRadioUnavailableTrack {
            key: String::new(),
            artist: String::new(),
            title: String::new(),
            failed_at_utc: now,
            retry_after_utc: now + TimeDelta::hours(24),
        }
    }
}

/// A song the listener rated one star: it never plays on their radio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioBan {
    #[serde(deserialize_with = "null_as_default")]
    pub key: String,
    #[serde(deserialize_with = "null_as_default")]
    pub song_id: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    #[serde(with = "datetime::utc")]
    pub banned_at_utc: DateTime<Utc>,
}

impl Default for LastFmRadioBan {
    fn default() -> Self {
        LastFmRadioBan {
            key: String::new(),
            song_id: String::new(),
            artist: String::new(),
            title: String::new(),
            banned_at_utc: Utc::now(),
        }
    }
}

/// One play the radio learns from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioPlay {
    #[serde(deserialize_with = "null_as_default")]
    pub song_id: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub duration: Option<i32>,
    pub is_local: bool,
    pub hearted: bool,
    pub learned_signal: bool,
    #[serde(deserialize_with = "null_as_default")]
    pub source: String,
    #[serde(with = "datetime::utc")]
    pub played_at_utc: DateTime<Utc>,
}

impl Default for LastFmRadioPlay {
    fn default() -> Self {
        LastFmRadioPlay {
            song_id: String::new(),
            artist: String::new(),
            title: String::new(),
            album: None,
            genre: None,
            duration: None,
            is_local: false,
            hearted: false,
            learned_signal: true,
            source: "scrobble".to_string(),
            played_at_utc: Utc::now(),
        }
    }
}

/// What a station is built from. Written as its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum LastFmRadioStationKind {
    #[default]
    Starter = 0,
    YourMix = 1,
    Discovery = 2,
    Artist = 3,
    Genre = 4,
    Pinned = 5,
}

/// One station: a read-only playlist of Last.fm suggestions for one listener.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioStation {
    #[serde(deserialize_with = "null_as_default")]
    pub id: String,
    #[serde(deserialize_with = "null_as_default")]
    pub key: String,
    #[serde(deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(deserialize_with = "null_as_default")]
    pub owner: String,
    pub kind: LastFmRadioStationKind,
    pub personalized: bool,
    pub definition_version: i32,
    #[serde(with = "datetime::utc")]
    pub created_utc: DateTime<Utc>,
    #[serde(with = "datetime::utc")]
    pub changed_utc: DateTime<Utc>,
    #[serde(with = "datetime::utc")]
    pub valid_until_utc: DateTime<Utc>,
    #[serde(deserialize_with = "null_as_default")]
    pub seeds: Vec<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub tracks: Vec<LastFmRadioTrack>,
    /// Further suggestions, in the walk's order, that stand in for a track the station may not
    /// use once it is resolved (a library song past Discovery Radio's share). Never listed.
    #[serde(deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub reserve: Vec<LastFmRadioTrack>,
}

impl Default for LastFmRadioStation {
    fn default() -> Self {
        let now = Utc::now();
        LastFmRadioStation {
            id: String::new(),
            key: String::new(),
            name: String::new(),
            owner: String::new(),
            kind: LastFmRadioStationKind::Starter,
            personalized: false,
            definition_version: 0,
            created_utc: now,
            changed_utc: now,
            valid_until_utc: now,
            seeds: Vec::new(),
            tracks: Vec::new(),
            reserve: Vec::new(),
        }
    }
}

/// One suggestion on a station.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioTrack {
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub duration: Option<i32>,
    pub year: Option<i32>,
    pub score: f64,
    #[serde(deserialize_with = "null_as_default")]
    pub source: String,
    pub resolved_id: Option<String>,
    pub is_local: bool,
    pub external_provider: Option<String>,
    pub you_tube_id: Option<String>,
}

/// The dashboard's line for one listener.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastFmRadioUserSummary {
    pub username: String,
    pub play_count: i32,
    pub station_count: i32,
    pub new_plays_since_refresh: i32,
    #[serde(with = "datetime::utc_option")]
    pub last_refresh_success_utc: Option<DateTime<Utc>>,
    pub last_refresh_error: Option<String>,
    pub refreshing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/lastfm-radio-state.json"
        ))
        .expect("the fixture is in the repo")
    }

    /// The file reads and writes back byte for byte, indented.
    #[test]
    fn the_fixture_round_trips_byte_for_byte() {
        let text = fixture();
        let document: LastFmRadioStateDocument = serde_json::from_str(&text).expect("the fixture reads");
        assert_eq!(document.version, 1);
        let user = &document.users["brandon"];
        assert_eq!(user.stations[0].kind, LastFmRadioStationKind::Artist);
        assert!(!user.plays[1].learned_signal);
        assert_eq!(crate::json::to_string_indented(&document), text);
    }

    /// Each field's default is what a missing field reads as.
    #[test]
    fn missing_fields_read_as_their_defaults() {
        let before = Utc::now();
        let document: LastFmRadioStateDocument = serde_json::from_str(
            r#"{"Users":{"a":{"Plays":[{}],"Stations":[{"Tracks":[{}]}],"UnavailableTracks":[{}]}}}"#,
        )
        .expect("reads");
        assert_eq!(document.version, 1);
        let user = &document.users["a"];
        assert!(user.last_seen_utc >= before);
        let play = &user.plays[0];
        assert!(play.learned_signal);
        assert_eq!(play.source, "scrobble");
        assert!(play.played_at_utc >= before);
        let unavailable = &user.unavailable_tracks[0];
        assert_eq!(
            unavailable.retry_after_utc - unavailable.failed_at_utc,
            TimeDelta::hours(24)
        );
        assert_eq!(user.stations[0].tracks[0].source, "");
    }

    /// A `null` in a string or list field reads as empty.
    #[test]
    fn nulls_in_string_and_list_fields_are_read() {
        let document: LastFmRadioStateDocument = serde_json::from_str(
            r#"{"Version":1,"Users":{"a":{"Username":null,"Plays":null,"Stations":[{"Seeds":null,"Tracks":null}]}}}"#,
        )
        .expect("reads");
        let user = &document.users["a"];
        assert_eq!(user.username, "");
        assert!(user.plays.is_empty());
        assert!(user.stations[0].tracks.is_empty());
        let empty: LastFmRadioStateDocument = serde_json::from_str(r#"{"Users":null}"#).expect("reads");
        assert!(empty.users.is_empty());
    }

    /// An empty document is written indented, with its version.
    #[test]
    fn an_empty_document_is_written_indented() {
        assert_eq!(
            crate::json::to_string_indented(&LastFmRadioStateDocument::default()),
            "{\n  \"Version\": 1,\n  \"Users\": {}\n}"
        );
    }
}
