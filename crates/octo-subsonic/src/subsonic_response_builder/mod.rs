//! Building Subsonic API responses in both XML and JSON formats, the lyrics and library
//! actions answers included.
//!
//! Everything here needs only the models. The parts that take app-service types (an
//! acquisition snapshot, a generated mix, a library-action outcome, an upgrade job) are in
//! `octo::services::subsonic::subsonic_response_builder`, which adds them to this builder.
//!
//! Field order is part of the shape: an object's fields are a [`Fields`] map in insertion
//! order (removals use `shift_remove`, so the order of what stays is kept), and every answer is
//! a [`SubsonicReply`] carrying the exact bytes and content type it goes out with.

mod library_actions;
mod lyrics;
mod places;
mod reply;
mod top_songs;
mod transition_profile;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Datelike, Utc};
use octo_core::common::Clock;
use octo_core::common::dotnet::{is_blank, to_lower_invariant, to_upper_char};
use octo_core::json::dom::Node;
use octo_core::json::format_double;
use octo_core::models::domain::{Album, Artist, Song, explicit_status};
use octo_core::models::radio::LastFmRadioStation;
use octo_core::models::subsonic::ExternalPlaylist;
use octo_core::settings::SubsonicSettings;
use octo_core::soulseek::{RoutingKind, SoulseekRouting, StreamFacts};
use serde_json::{Map, Number, Value, json};

use crate::xml::{XElement, XValue};

pub use library_actions::{
    CHECK_VERSION_ACTION, COVER_ACTION, COVER_SWAP_ACTION, EDIT_ACTIONS, JOIN_ALBUM_ACTION,
    LIBRARY_ACTIONS_EXTENSION, LIBRARY_ACTIONS_EXTENSION_VERSION, LOOKUP_ACTION, LYRICS_ACTION,
    REMOVE_ACTION, RESTORE_ACTION, RETAG_ACTION, UNDO_ACTION, UPGRADE_ACTION,
};
pub use lyrics::{CueLineOut, CueWord, LYRICS_EXTENSION, LYRICS_EXTENSION_VERSION};
pub use places::{PLACES_EXTENSION, PLACES_EXTENSION_VERSION};
pub use reply::{ReplyKind, SubsonicReply};
pub use top_songs::{
    ChartChoiceRow, ChartsHeader, POPULAR_COMMENT, RankedRow, TopSongRow, TopSongsHeader,
    chart_playlist_comment, json_as_xml, library_song_xml,
};

/// Response fields, in insertion order.
pub type Fields = Map<String, Value>;

pub const SUBSONIC_NAMESPACE: &str = "http://subsonic.org/restapi";
pub const SUBSONIC_VERSION: &str = "1.16.1";

/// The OpenSubsonic extension a client checks for before it asks for getAcquisitions. Version 2
/// adds each row's log (getAcquisition), clearing finished rows (clearAcquisitions) and Find
/// songs (findSongs, getFoundSongs, pickFoundSong). Version 3: pickFoundSong takes `anyway` and
/// may answer `confirm`, asking before a library song is replaced by a copy no better than it.
/// Version 4: a held row carries `keepAnyway` for a caller who may keep it and an album row
/// `heldTracks`, every hearted album's row is of `kind` "album" whatever its state, and
/// keepAcquisitionAnyway keeps a held row's audio anyway; a row whose kept copy
/// waits for Lidarr carries `stopWaiting` for a caller whose heart kept it, and
/// stopWaitingForLidarr lands that copy now.
pub const ACQUISITIONS_EXTENSION: &str = "octoAcquisitions";
pub const ACQUISITIONS_EXTENSION_VERSION: i32 = 4;

/// getArtistTopSongs and getTopChart. Version 2: getTopChart takes `chart` (Best New Songs,
/// Trending Songs, a genre) and getCharts lists the charts. Version 3: getCharts and getTopChart
/// take `country`, getCharts says the caller's `setting`, `pinned`, the `serverCountry`, Apple's
/// `countries` and `maxPinned`, and setChartCountry and pinChartCountry save the caller's own.
pub const TOP_SONGS_EXTENSION: &str = "octoTopSongs";
pub const TOP_SONGS_EXTENSION_VERSION: i32 = 3;

/// The extensions Octo answers itself, with their versions. songLyrics is here because Octo
/// answers getLyricsBySongId for every song, and for the ones it answers itself it honors
/// enhanced=true (version 2) with word cues; a Navidrome that lists fewer versions has them
/// added, never removed.
pub const OWN_EXTENSIONS: &[(&str, &[i32])] = &[
    (ACQUISITIONS_EXTENSION, &[1, 2, 3, ACQUISITIONS_EXTENSION_VERSION]),
    (LYRICS_EXTENSION, &[LYRICS_EXTENSION_VERSION]),
    (
        LIBRARY_ACTIONS_EXTENSION,
        &[1, 2, LIBRARY_ACTIONS_EXTENSION_VERSION],
    ),
    ("songLyrics", &[1, 2]),
    // Listed after songLyrics.
    (TOP_SONGS_EXTENSION, &[1, 2, TOP_SONGS_EXTENSION_VERSION]),
    (IMPORTS_EXTENSION, &[1, IMPORTS_EXTENSION_VERSION]),
    (RADIO_SOURCES_EXTENSION, &[RADIO_SOURCES_EXTENSION_VERSION]),
    (LISTS_EXTENSION, &[1, LISTS_EXTENSION_VERSION]),
    (TRANSITIONS_EXTENSION, &[TRANSITIONS_EXTENSION_VERSION]),
    (STATIONS_EXTENSION, &[STATIONS_EXTENSION_VERSION]),
    (PLACES_EXTENSION, &[PLACES_EXTENSION_VERSION]),
    (ANIMATED_ART_EXTENSION, &[ANIMATED_ART_EXTENSION_VERSION]),
];

/// The OpenSubsonic extension a client checks for before it offers imports (getImports,
/// getImport, importAction). Version 2: getImportServices, importFile and importText.
pub const IMPORTS_EXTENSION: &str = "octoImports";
pub const IMPORTS_EXTENSION_VERSION: i32 = 2;

/// Octo's radio answers name the source of each song (`octoSuggestedBy`): Last.fm, YouTube
/// Music, ListenBrainz, Sounds alike or Your library.
pub const RADIO_SOURCES_EXTENSION: &str = "octoRadioSources";
pub const RADIO_SOURCES_EXTENSION_VERSION: i32 = 1;

/// octoLists v1: Made for you (New Releases, Rediscover, Deep Cuts) are read-only playlists whose
/// entry in getPlaylists and getPlaylist carries `octoList`, the list's kind (and Liked Songs
/// carries `octoList: "liked"`). Version 2: every Octo list's entry also carries `octoGroup`,
/// the group it belongs to: `genre`, `decade`, `year`, `country` or `artist` for a mix, `chart`,
/// `forYou` for Made for you, `liked` and `station`. `octoList` is unchanged.
pub const LISTS_EXTENSION: &str = "octoLists";
pub const LISTS_EXTENSION_VERSION: i32 = 2;

/// getTransitionProfile: each song's transition profile (where it can blend into the next
/// song), made by octo-sonic. Listed while Sounds alike runs, since the same reading makes both.
pub const TRANSITIONS_EXTENSION: &str = "octoTransitions";
pub const TRANSITIONS_EXTENSION_VERSION: i32 = 1;

/// getStationSongs: a radio station is not a fixed list. It answers `stationSongs` with the
/// station's `id`, `name` and `entry` songs (shaped as getPlaylist lists a station's), `count` of
/// them (1 to 100, 50 when not sent), none of the ids sent as `after` (repeated, the songs already
/// queued, oldest first; the last 200 are read). The station's own songs come first, then new
/// songs of its kind, without end. A song the station served the caller in the last 24 hours (here
/// or on its stream, the newest 1,000) is not served again unless nothing new is left. Error 70
/// for a station that is not the caller's or not there any more. Listed while radio stations are
/// served as playlists.
pub const STATIONS_EXTENSION: &str = "octoStations";
pub const STATIONS_EXTENSION_VERSION: i32 = 1;

/// getAnimatedArt, and `animatedArt` on getAlbum for an Octo app: an album's animated art from
/// Apple Music, a square loop and a tall one, each an HLS playlist with a still frame and a
/// background color. Listed while animated art is on.
pub const ANIMATED_ART_EXTENSION: &str = "octoAnimatedArt";
pub const ANIMATED_ART_EXTENSION_VERSION: i32 = 1;

/// Fields only a file in the library can truthfully have.
pub const FILE_ONLY_FIELDS: &[&str] = &[
    "path",
    "size",
    "created",
    "bitDepth",
    "samplingRate",
    "channelCount",
];

/// What the builder needs from `ExternalIdRegistry`: a short id for a routing, the same id for
/// the same routing every time. The registry itself lives in the `octo` crate, which
/// implements this for it.
pub trait IdRegistry: Send + Sync {
    fn register(&self, routing: SoulseekRouting) -> String;

    /// What is known of the stream an outside song under `song_id` plays, when anything is.
    fn stream_facts(&self, _song_id: &str) -> Option<StreamFacts> {
        None
    }

    /// An outside song went out without its size: a chance to learn it for next time.
    fn size_wanted(&self, _song_id: &str) {}

    /// An outside song that streams from YouTube is in the answer being built, with its size
    /// or without, so the registry knows how many such rows one answer lists.
    fn outside_listed(&self, _song_id: &str) {}
}

/// Handles building Subsonic API responses in both XML and JSON formats.
#[derive(Clone)]
pub struct SubsonicResponseBuilder {
    id_registry: Arc<dyn IdRegistry>,

    /// Whether an external id resolves to a lossless file. Read once at construction on
    /// purpose: it decides what every search result DECLARES, so it must not change under
    /// a client that has already cached those rows. The setting is restart-required.
    externals_are_lossless: bool,

    /// The clock, for the `created` of album rows. A seam for tests.
    clock: Clock,
}

impl std::fmt::Debug for SubsonicResponseBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubsonicResponseBuilder")
            .field("externals_are_lossless", &self.externals_are_lossless)
            .finish_non_exhaustive()
    }
}

impl SubsonicResponseBuilder {
    /// `subsonic` is read here and not again: see `externals_are_lossless`.
    pub fn new(id_registry: Arc<dyn IdRegistry>, subsonic: &SubsonicSettings) -> Self {
        Self {
            id_registry,
            externals_are_lossless: subsonic.wait_for_lossless_on_play,
            clock: Clock::system(),
        }
    }

    /// The same builder reading `clock` for "now".
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Creates a generic Subsonic response with status "ok".
    ///
    /// The JSON answer leaves the element out entirely, while the XML one writes it empty.
    pub fn create_response(&self, format: &str, element_name: &str) -> SubsonicReply {
        if format == "json" {
            return self.create_json_response(json!({ "status": "ok", "version": SUBSONIC_VERSION }));
        }
        SubsonicReply::xml(&envelope("ok").child(subsonic_element(element_name)))
    }

    /// Creates an ok response with a single element carrying string child fields,
    /// in BOTH json and xml (unlike CreateResponse, which emits an empty element).
    /// Used for albumInfo/artistInfo2 so the data survives regardless of format.
    pub fn create_info_response(
        &self,
        format: &str,
        element_name: &str,
        fields: &[(&str, &str)],
    ) -> SubsonicReply {
        if format == "json" {
            let mut body = Fields::new();
            body.insert("status".into(), "ok".into());
            body.insert("version".into(), SUBSONIC_VERSION.into());
            let element: Fields = fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), Value::from(*v)))
                .collect();
            body.insert(element_name.into(), Value::Object(element));
            return self.create_json_response(Value::Object(body));
        }

        let mut element = subsonic_element(element_name);
        for (key, value) in fields {
            element.push(subsonic_element(*key).text(*value));
        }
        SubsonicReply::xml(&envelope("ok").child(element))
    }

    /// [`Self::create_info_response`] for an artist, with the related artists as
    /// `similarArtist` entries after the fields (none written when there are none).
    pub fn create_artist_info_response(
        &self,
        format: &str,
        element_name: &str,
        fields: &[(&str, &str)],
        similar: &[Artist],
    ) -> SubsonicReply {
        if format == "json" {
            let mut body = Fields::new();
            body.insert("status".into(), "ok".into());
            body.insert("version".into(), SUBSONIC_VERSION.into());
            let mut element: Fields = fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), Value::from(*v)))
                .collect();
            if !similar.is_empty() {
                element.insert(
                    "similarArtist".into(),
                    Value::Array(
                        similar
                            .iter()
                            .map(|artist| Value::Object(build_artist_fields(artist)))
                            .collect(),
                    ),
                );
            }
            body.insert(element_name.into(), Value::Object(element));
            return self.create_json_response(Value::Object(body));
        }

        let mut element = subsonic_element(element_name);
        for (key, value) in fields {
            element.push(subsonic_element(*key).text(*value));
        }
        for artist in similar {
            let mut entry = subsonic_element("similarArtist");
            attributes(&mut entry, &build_artist_fields(artist));
            element.push(entry);
        }
        SubsonicReply::xml(&envelope("ok").child(element))
    }

    /// Creates a Subsonic error response.
    pub fn create_error(&self, format: &str, code: i32, message: &str) -> SubsonicReply {
        if format == "json" {
            return self.create_json_response(json!({
                "status": "failed",
                "version": SUBSONIC_VERSION,
                "error": { "code": code, "message": message },
            }));
        }
        SubsonicReply::xml(
            &envelope("failed").child(
                subsonic_element("error")
                    .attr("code", code)
                    .attr("message", message),
            ),
        )
    }

    /// Creates a Subsonic response containing a single song.
    pub fn create_song_response(&self, format: &str, song: &Song) -> SubsonicReply {
        if format == "json" {
            return self.create_json_response(json!({
                "status": "ok",
                "version": SUBSONIC_VERSION,
                "song": self.convert_song_to_json(song),
            }));
        }
        SubsonicReply::xml(&envelope("ok").child(self.convert_song_to_xml(song, Some(SUBSONIC_NAMESPACE))))
    }

    pub fn radio_playlist_fields(&self, station: &LastFmRadioStation) -> Fields {
        let duration: i64 = station
            .tracks
            .iter()
            .map(|track| i64::from(known_seconds(track.duration)))
            .sum();
        let mut fields = Fields::new();
        fields.insert("id".into(), station.id.clone().into());
        fields.insert("name".into(), station.name.clone().into());
        fields.insert("owner".into(), station.owner.clone().into());
        fields.insert("public".into(), false.into());
        fields.insert("songCount".into(), station.tracks.len().into());
        fields.insert("duration".into(), duration.into());
        fields.insert("created".into(), millis(&station.created_utc).into());
        fields.insert("changed".into(), millis(&station.changed_utc).into());
        // The station's own id, so each station gets its own cover rather than one shared one.
        fields.insert("coverArt".into(), station.id.clone().into());
        fields.insert("readonly".into(), true.into());
        fields.insert("validUntil".into(), millis(&station.valid_until_utc).into());
        // octoLists v2: the group of Octo's lists this row belongs to.
        fields.insert(
            "octoGroup".into(),
            octo_core::library::generated_playlist_service::STATION_GROUP.into(),
        );
        fields
    }

    pub fn create_radio_playlist_response(
        &self,
        format: &str,
        station: &LastFmRadioStation,
        songs: &[Song],
    ) -> SubsonicReply {
        let mut fields = self.radio_playlist_fields(station);
        fields.insert("songCount".into(), songs.len().into());
        let duration: i64 = songs
            .iter()
            .map(|song| i64::from(known_seconds(song.duration)))
            .sum();
        fields.insert("duration".into(), duration.into());
        if format.eq_ignore_ascii_case("json") {
            let mut playlist = fields;
            playlist.insert(
                "entry".into(),
                Value::Array(
                    songs
                        .iter()
                        .map(|song| Value::Object(self.convert_song_to_json(song)))
                        .collect(),
                ),
            );
            return self.create_json_response(json!({
                "status": "ok", "version": SUBSONIC_VERSION, "playlist": playlist,
            }));
        }
        let mut playlist = subsonic_element("playlist");
        attributes(&mut playlist, &fields);
        for song in songs {
            playlist.push(self.convert_song_to_xml(song, Some(SUBSONIC_NAMESPACE)));
        }
        SubsonicReply::xml(&envelope("ok").child(playlist))
    }

    /// getStationSongs's answer: the station's id and name, and its songs as getPlaylist lists a
    /// station's entries.
    pub fn create_station_songs_response(
        &self,
        format: &str,
        station: &LastFmRadioStation,
        songs: &[Song],
    ) -> SubsonicReply {
        let mut fields = Fields::new();
        fields.insert("id".into(), station.id.clone().into());
        fields.insert("name".into(), station.name.clone().into());
        if format.eq_ignore_ascii_case("json") {
            fields.insert(
                "entry".into(),
                Value::Array(
                    songs
                        .iter()
                        .map(|song| Value::Object(self.convert_song_to_json(song)))
                        .collect(),
                ),
            );
            return self.create_json_response(json!({
                "status": "ok", "version": SUBSONIC_VERSION, "stationSongs": fields,
            }));
        }
        let mut element = subsonic_element("stationSongs");
        attributes(&mut element, &fields);
        for song in songs {
            let mut entry = self.convert_song_to_xml(song, Some(SUBSONIC_NAMESPACE));
            entry.name = "entry".into();
            element.push(entry);
        }
        SubsonicReply::xml(&envelope("ok").child(element))
    }

    /// A playlist whose entries are Navidrome's own songs, passed through as Navidrome
    /// described them; in XML only their scalar fields, since Subsonic's XML entry is
    /// attribute-only. A generated mix's answer is built with this from the mix's fields on
    /// (the mix itself is an app type; see the `octo` crate).
    pub fn create_entries_playlist_response(
        &self,
        format: &str,
        mut fields: Fields,
        entries: &[Node],
    ) -> SubsonicReply {
        fields.insert("songCount".into(), entries.len().into());
        let duration: i64 = entries
            .iter()
            .map(|entry| match entry.get("duration") {
                Some(Node::Number(text)) => i64::from(text.parse::<i32>().unwrap_or(0)),
                _ => 0,
            })
            .sum();
        fields.insert("duration".into(), duration.into());
        if format.eq_ignore_ascii_case("json") {
            let mut playlist = match to_node(&Value::Object(fields)) {
                Node::Object(map) => map,
                _ => unreachable!("an object converts to an object"),
            };
            playlist.insert("entry".into(), Node::Array(entries.to_vec()));
            let mut response = indexmap::IndexMap::new();
            response.insert("status".to_string(), Node::String("ok".into()));
            response.insert("version".to_string(), Node::String(SUBSONIC_VERSION.into()));
            response.insert("playlist".to_string(), Node::Object(playlist));
            let mut root = indexmap::IndexMap::new();
            root.insert("subsonic-response".to_string(), Node::Object(response));
            return SubsonicReply::json_text(Node::Object(root).to_json_string(false));
        }
        let mut playlist = subsonic_element("playlist");
        attributes(&mut playlist, &fields);
        for entry in entries {
            let mut element = subsonic_element("entry");
            scalar_attributes(&mut element, entry);
            // OpenSubsonic's replayGain is a child element in XML; only a measured one is written.
            if let Some(gain) = entry
                .get("replayGain")
                .map(Node::to_value)
                .filter(|gain| gain.as_object().is_some_and(|g| !g.is_empty()))
            {
                element.push(json_shape_to_xml(Some(SUBSONIC_NAMESPACE), "replayGain", &gain));
            }
            playlist.push(element);
        }
        SubsonicReply::xml(&envelope("ok").child(playlist))
    }

    /// Creates a Subsonic response containing an album with songs.
    pub fn create_album_response(&self, format: &str, album: &Album) -> SubsonicReply {
        let fields = self.album_detail_fields(album);

        if format == "json" {
            let mut body = fields;
            body.insert(
                "song".into(),
                Value::Array(
                    album
                        .songs
                        .iter()
                        .map(|s| Value::Object(self.convert_song_to_json(s)))
                        .collect(),
                ),
            );
            return self.create_json_response(json!({
                "status": "ok",
                "version": SUBSONIC_VERSION,
                "album": body,
            }));
        }

        let mut element = subsonic_element("album");
        attributes(&mut element, &fields);
        text_lists(&mut element, &fields, Some(SUBSONIC_NAMESPACE));
        for song in &album.songs {
            element.push(self.convert_song_to_xml(song, Some(SUBSONIC_NAMESPACE)));
        }
        SubsonicReply::xml(&envelope("ok").child(element))
    }

    /// An outside album's own fields as its detail sends them, without its songs.
    pub fn album_detail_fields(&self, album: &Album) -> Fields {
        let song_count: i64 = if album.songs.is_empty() {
            i64::from(album.song_count.unwrap_or(0))
        } else {
            album.songs.len() as i64
        };
        let duration: i64 = album
            .songs
            .iter()
            .map(|s| i64::from(s.duration.unwrap_or(0)))
            .sum();
        let mut fields = Fields::new();
        fields.insert("id".into(), album.id.clone().into());
        fields.insert("name".into(), album.title.clone().into());
        fields.insert("artist".into(), album.artist.clone().into());
        fields.insert("coverArt".into(), album.id.clone().into());
        fields.insert("songCount".into(), song_count.into());
        fields.insert("duration".into(), duration.into());
        fields.insert("genre".into(), album.genre.clone().unwrap_or_default().into());
        fields.insert("isCompilation".into(), false.into());
        // Required by the OpenSubsonic schema, and strict clients validate the payload
        // before they play anything: Music Assistant rejected every external album with
        // "Field created of type str is missing in AlbumID3WithSongs" (issue #35).
        // Lenient clients never noticed, so this looked like a Music Assistant problem.
        // BuildAlbumFields, which renders the album ROWS in a search, has always sent
        // it; this builds the album DETAIL and did not, so the two disagreed.
        fields.insert("created".into(), millis(&self.clock.now()).into());
        fields.insert("releaseTypes".into(), album.release_types.clone().into());
        fields.insert(
            "explicitStatus".into(),
            explicit_status::for_clients(album.explicit_content_lyrics).into(),
        );
        if let Some(artist_id) = &album.artist_id {
            fields.insert("artistId".into(), artist_id.clone().into());
        }
        if let Some(year) = album.year {
            fields.insert("year".into(), year.into());
        }
        if let Some(owned) = album.owned_count {
            fields.insert("ownedCount".into(), owned.into());
        }
        fields
    }

    /// Creates a Subsonic response for a playlist represented as an album.
    /// Playlists appear as albums with genre "Playlist".
    pub fn create_playlist_as_album_response(
        &self,
        format: &str,
        playlist: &ExternalPlaylist,
        tracks: &[Song],
    ) -> SubsonicReply {
        let total_duration: i64 = tracks.iter().map(|s| i64::from(s.duration.unwrap_or(0))).sum();

        // Build artist name with emoji and curator
        let artist_name = curator_artist_name(playlist);
        let artist_id = curator_artist_id(playlist);

        if format == "json" {
            return self.create_json_response(json!({
                "status": "ok",
                "version": SUBSONIC_VERSION,
                "album": {
                    "id": playlist.id,
                    "name": playlist.name,
                    "artist": artist_name,
                    "artistId": artist_id,
                    "coverArt": playlist.id,
                    "songCount": tracks.len(),
                    "duration": total_duration,
                    "year": playlist.created_date.map(|d| year_of(&d)).unwrap_or(0),
                    "genre": "Playlist",
                    "isCompilation": false,
                    "created": playlist.created_date.map(|d| seconds(&d)),
                    "song": tracks.iter().map(|s| Value::Object(self.convert_song_to_json(s))).collect::<Vec<_>>(),
                }
            }));
        }

        let mut album = subsonic_element("album")
            .attr("id", &playlist.id)
            .attr("name", &playlist.name)
            .attr("artist", &artist_name)
            .attr("artistId", &artist_id)
            .attr("songCount", tracks.len())
            .attr("duration", total_duration)
            .attr("genre", "Playlist")
            .attr("coverArt", &playlist.id);

        if let Some(created) = &playlist.created_date {
            album.set_attr("year", year_of(created));
            album.set_attr("created", seconds(created));
        }

        // Add songs
        for song in tracks {
            album.push(self.convert_song_to_xml(song, Some(SUBSONIC_NAMESPACE)));
        }

        SubsonicReply::xml(&envelope("ok").child(album))
    }

    /// Creates a Subsonic response containing an artist with albums.
    pub fn create_artist_response(&self, format: &str, artist: &Artist, albums: &[Album]) -> SubsonicReply {
        if format == "json" {
            return self.create_json_response(json!({
                "status": "ok",
                "version": SUBSONIC_VERSION,
                "artist": {
                    "id": artist.id,
                    "name": artist.name,
                    "coverArt": artist.id,
                    "albumCount": albums.len(),
                    "artistImageUrl": artist.image_url,
                    "album": albums.iter().map(|a| Value::Object(self.convert_album_to_json(a))).collect::<Vec<_>>(),
                }
            }));
        }

        let element = subsonic_element("artist")
            .attr("id", &artist.id)
            .attr("name", &artist.name)
            .attr("coverArt", &artist.id)
            .attr("albumCount", albums.len())
            .children(
                albums
                    .iter()
                    .map(|a| self.convert_album_to_xml(a, Some(SUBSONIC_NAMESPACE))),
            );
        SubsonicReply::xml(&envelope("ok").child(element))
    }

    /// An ok response in the format the client asked for, from the JSON-shaped data Octo
    /// merges (Navidrome's JSON with outside songs or albums added). The merge only reads
    /// JSON, so a client asking for XML used to get Navidrome's own XML back, without the
    /// outside songs: half an album. XML is written the way OpenSubsonic writes it, see
    /// [`json_shape_to_xml`].
    pub fn create_merged_response(&self, format: &str, element_name: &str, data: &Value) -> SubsonicReply {
        if format == "json" {
            let mut body = Fields::new();
            body.insert("status".into(), "ok".into());
            body.insert("version".into(), SUBSONIC_VERSION.into());
            body.insert(element_name.into(), data.clone());
            return self.create_json_response(Value::Object(body));
        }
        SubsonicReply::xml(&envelope("ok").child(json_shape_to_xml(
            Some(SUBSONIC_NAMESPACE),
            element_name,
            data,
        )))
    }

    /// Creates a JSON Subsonic response with "subsonic-response" key (with hyphen).
    pub fn create_json_response(&self, response_content: Value) -> SubsonicReply {
        let mut response = Fields::new();
        response.insert("subsonic-response".into(), response_content);
        SubsonicReply::json(&Value::Object(response))
    }

    /// Navidrome's getOpenSubsonicExtensions with Octo's own added, in the format asked for.
    /// A failed answer passes through untouched; with no answer at all, Octo lists its own.
    ///
    /// octoTopSongs, octoRadioSources, octoTransitions, octoStations and octoPlaces are left out;
    /// [`Self::merge_open_subsonic_extensions_with`] lists them.
    pub fn merge_open_subsonic_extensions(
        &self,
        format: &str,
        upstream: Option<&[u8]>,
        content_type: Option<&str>,
        lyrics_choices: bool,
        library_actions: bool,
    ) -> SubsonicReply {
        self.merge_open_subsonic_extensions_with(
            format,
            upstream,
            content_type,
            lyrics_choices,
            library_actions,
            false,
            false,
            false,
            false,
            false,
            false,
        )
    }

    /// [`merge_open_subsonic_extensions`](Self::merge_open_subsonic_extensions) with octoTopSongs
    /// listed while `top_songs` (search discovery) is on, its lists being outside songs that
    /// discovery being off hides, octoRadioSources while radio can answer (`radio_sources`),
    /// octoTransitions while octo-sonic makes profiles (`transitions`), octoStations while
    /// radio stations are served as playlists (`stations`), octoPlaces while the place data
    /// can be read (`places`), and octoAnimatedArt while animated art is on
    /// (`animated_art`).
    pub fn merge_open_subsonic_extensions_with(
        &self,
        format: &str,
        upstream: Option<&[u8]>,
        content_type: Option<&str>,
        lyrics_choices: bool,
        library_actions: bool,
        top_songs: bool,
        radio_sources: bool,
        transitions: bool,
        stations: bool,
        places: bool,
        animated_art: bool,
    ) -> SubsonicReply {
        let json = format.eq_ignore_ascii_case("json");
        // octoLyrics is only listed while its lookups can run, so a client never offers a
        // "choose lyrics" that can only answer that lookups are off. octoLibraryActions is only
        // listed while library actions are on, octoRadioSources while radio can answer, and
        // octoPlaces while there is place data to answer from, for the same reason.
        let own: Vec<(&str, &[i32])> = OWN_EXTENSIONS
            .iter()
            .copied()
            .filter(|(name, _)| lyrics_choices || *name != LYRICS_EXTENSION)
            .filter(|(name, _)| library_actions || *name != LIBRARY_ACTIONS_EXTENSION)
            .filter(|(name, _)| top_songs || *name != TOP_SONGS_EXTENSION)
            .filter(|(name, _)| radio_sources || *name != RADIO_SOURCES_EXTENSION)
            .filter(|(name, _)| transitions || *name != TRANSITIONS_EXTENSION)
            .filter(|(name, _)| stations || *name != STATIONS_EXTENSION)
            .filter(|(name, _)| places || *name != PLACES_EXTENSION)
            .filter(|(name, _)| animated_art || *name != ANIMATED_ART_EXTENSION)
            .collect();

        if let Some(upstream) = upstream.filter(|u| !u.is_empty()) {
            let merged = if json {
                merge_extensions_json(upstream, &own)
                    .map(|text| SubsonicReply::content(text, "application/json"))
            } else {
                merge_extensions_xml(upstream, &own)
                    .map(|text| SubsonicReply::content(text, content_type.unwrap_or("application/xml")))
            };
            // Not something this can read. Hand it back as it came rather than break the call.
            return merged.unwrap_or_else(|| {
                SubsonicReply::file(
                    upstream.to_vec(),
                    content_type.unwrap_or(if json {
                        "application/json"
                    } else {
                        "application/xml"
                    }),
                )
            });
        }

        if json {
            return self.create_json_response(json!({
                "status": "ok",
                "version": SUBSONIC_VERSION,
                "type": "octo",
                "openSubsonicExtensions": own
                    .iter()
                    .map(|(name, versions)| json!({ "name": name, "versions": versions }))
                    .collect::<Vec<_>>(),
            }));
        }
        let mut response = envelope("ok");
        for (name, versions) in &own {
            response.push(
                subsonic_element("openSubsonicExtensions")
                    .attr("name", *name)
                    .children(
                        versions
                            .iter()
                            .map(|v| subsonic_element("versions").text(v.to_string())),
                    ),
            );
        }
        SubsonicReply::xml(&response)
    }

    /// Converts a Song domain model to Subsonic JSON format: the song shape both serializers
    /// render. See [`attributes`] for why there is only one of these.
    pub fn convert_song_to_json(&self, song: &Song) -> Fields {
        self.convert_song_fields(song)
    }

    /// The content type a client picks its decoder from, for a library file's suffix.
    pub fn content_type_for(suffix: &str) -> &'static str {
        match suffix {
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "m4a" | "aac" | "alac" | "mp4" => "audio/mp4",
            "ogg" | "opus" | "oga" => "audio/ogg",
            "wav" => "audio/wav",
            _ => "application/octet-stream",
        }
    }

    fn convert_song_fields(&self, song: &Song) -> Fields {
        // A song outside the library says so: isExternal is true, and it carries no
        // file facts (path, size, created, bit depth, sample rate, channels), because
        // there is no file. It used to borrow a whole library song's shape, with a
        // path, a size and "now" as the date added, so an album page showed songs the
        // server does not hold as if they were in the library. What it keeps is how it
        // streams: suffix and contentType, which a client picks its decoder from.
        // Generate Navidrome-shaped 22-char base62 ids for any external entity that
        // doesn't already have a real id. Registering here lets getCoverArt later
        // reverse-resolve the id to artist/album and look up artwork on iTunes.
        // Subsonic clients (Arpeggio in particular) drop entries whose cover-art
        // request 404s, so making these ids resolvable is what gets external songs
        // queued and played at all.
        // External (radio) songs stream as YouTube format-140 audio: m4a / AAC LC
        // inside an mp4 container, ~128kbps. The shim does NOT transcode: it
        // proxies the googlevideo bytes directly. Declared metadata MUST match the
        // real bytes, otherwise Subsonic clients prep the wrong decoder and the
        // play silently fails (Feishin holds at "loading", Arpeggi drops the entry
        // from the queue). Earlier versions claimed mp3/192k here; that was a lie.
        // With WaitForLosslessOnPlay on, /rest/stream serves the fetched FLAC under this
        // same id, so it has to be declared as one. 950 rather than 1411 because that
        // figure is uncompressed PCM and real FLAC compresses well below it: a measured
        // Mezzanine track came out at ~840kbps, where 1411 would have overstated its size
        // by about 70%. suffix and contentType are the contract a client picks its decoder
        // from and are exact. For an outside song the 950 is only an estimate, so it is
        // not sent (see the end of this method).
        //
        // A library song is declared as Navidrome described it when that is known, and as FLAC
        // only when it is not: radio and the Discovery blend put library MP3s here too.
        let lossless_external = !song.is_local && self.externals_are_lossless;
        let local_suffix = match song.suffix.as_deref() {
            Some(suffix) if !is_blank(suffix) => to_lower_invariant(suffix.trim()),
            _ => "flac".to_string(),
        };
        let bit_rate: i32 = if song.is_local {
            song.bit_rate.filter(|rate| *rate > 0).unwrap_or(1411)
        } else if lossless_external {
            950
        } else {
            128
        };
        let suffix = if song.is_local {
            local_suffix.clone()
        } else if lossless_external {
            "flac".to_string()
        } else {
            "m4a".to_string()
        };
        let content_type = if song.is_local {
            Self::content_type_for(&local_suffix)
        } else if lossless_external {
            "audio/flac"
        } else {
            "audio/mp4"
        };
        // A song whose length is not known goes out without one, never with a guess: a
        // player that plans a crossfade from a made-up length cuts the song short or leaves
        // a gap, while a missing length only waits for the stream to report the real one.
        let duration = song.duration.filter(|&d| d > 0);
        let est_size = i64::from(duration.unwrap_or(0)) * i64::from(bit_rate) * 125;

        // Resolve a real-looking album for placeholder songs. Last.fm's
        // track.search/getsimilar don't include album names, so song.Album is
        // the empty string for nearly every external song. iOS Subsonic
        // clients (Arpeggi, Narjo) silently drop songs with album="" because
        // their library views index by album: invisible album = invisible
        // song. Apple Music represents singles as "song-name = album-name", so
        // doing the same here makes each placeholder look like a single and
        // satisfies the album-required filter.
        let album_name = if is_blank(&song.album) {
            song.title.clone()
        } else {
            song.album.clone()
        };

        let artist_id = song.artist_id.clone().unwrap_or_else(|| {
            self.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some(song.artist.clone()),
                ..Default::default()
            })
        });
        let album_id = song.album_id.clone().unwrap_or_else(|| {
            self.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Album,
                artist: Some(song.artist.clone()),
                album: Some(album_name.clone()),
                ..Default::default()
            })
        });

        // Avoid empty path segments: clients that lex on '/' (Arpeggio in
        // particular) treat double-slash as malformed and quietly drop the entry.
        let path = format!("{}/{}/{}.{}", song.artist, album_name, song.title, suffix);
        let artist_list = json!([{ "id": artist_id, "name": song.artist }]);

        // The path and the file defaults below (bit depth, sample rate, channels) are for
        // library songs that reach here without them; an outside song drops them.
        //
        // Year is omitted entirely when unknown, rather than
        // defaulted. It used to fall back to the current year, which is not a plausible
        // default but a wrong one: a 1995 track was published to the client as this year's
        // release, and unlike a missing field that is something the user can see and
        // sort by. Every real library is full of untagged files, so a client that rejected
        // entries without a year would already be broken against the server it is pointed
        // at.
        let track = song.track.unwrap_or(1);
        let bit_depth = 16;

        let mut fields = Fields::new();
        fields.insert("id".into(), song.id.clone().into());
        fields.insert("parent".into(), album_id.clone().into());
        fields.insert("isDir".into(), false.into());
        fields.insert("title".into(), song.title.clone().into());
        fields.insert("album".into(), album_name.into());
        fields.insert("artist".into(), song.artist.clone().into());
        fields.insert("track".into(), track.into());
        fields.insert("genre".into(), song.genre.clone().unwrap_or_default().into());
        fields.insert("coverArt".into(), song.id.clone().into());
        fields.insert("size".into(), est_size.into());
        fields.insert("contentType".into(), content_type.into());
        fields.insert("suffix".into(), suffix.into());
        if let Some(duration) = duration {
            fields.insert("duration".into(), duration.into());
        }
        fields.insert("bitRate".into(), bit_rate.into());
        fields.insert("path".into(), path.into());
        fields.insert("created".into(), millis(&self.clock.now()).into());
        fields.insert("albumId".into(), album_id.into());
        fields.insert("artistId".into(), artist_id.into());
        fields.insert("type".into(), "music".into());
        fields.insert("isVideo".into(), false.into());
        fields.insert("mediaType".into(), "song".into());
        fields.insert("channelCount".into(), 2.into());
        fields.insert("samplingRate".into(), 44100.into());
        fields.insert("bitDepth".into(), bit_depth.into());
        fields.insert("artists".into(), artist_list.clone());
        fields.insert("displayArtist".into(), song.artist.clone().into());
        fields.insert("albumArtists".into(), artist_list);
        fields.insert("displayAlbumArtist".into(), song.artist.clone().into());
        fields.insert("contributors".into(), Value::Array(Vec::new()));
        // OpenSubsonic's word for the catalog's flag ("explicit", "clean" or ""), so a client can
        // mark an outside song before it is added. A library song that reaches here carries what
        // Navidrome said of it, or nothing.
        fields.insert(
            "explicitStatus".into(),
            explicit_status::for_clients(song.explicit_content_lyrics).into(),
        );
        // OpenSubsonic's isrc is a list. An album track Deezer described carries its code,
        // and a library song keeps the ones Navidrome gave it.
        fields.insert("isrc".into(), song.isrcs_for_clients().into());
        fields.insert("genres".into(), Value::Array(Vec::new()));
        fields.insert("moods".into(), Value::Array(Vec::new()));
        fields.insert("replayGain".into(), Value::Object(Fields::new()));
        fields.insert("sortName".into(), to_lower_invariant(&song.title).into());
        fields.insert("isExternal".into(), false.into());

        if let Some(year) = song.year {
            fields.insert("year".into(), year.into());
        }
        // Which radio source suggested the song; only radio answers carry it (octoRadioSources).
        if let Some(by) = song.suggested_by.as_deref().filter(|by| !by.is_empty()) {
            fields.insert("octoSuggestedBy".into(), by.into());
        }

        if !song.is_local {
            // A FLAC's rate is a guess until it is fetched; the 128 of the AAC stream is real.
            let size = if lossless_external {
                fields.shift_remove("bitRate");
                None
            } else {
                self.declare_outside_stream(&mut fields, &song.id)
            };
            // No file on the server, so nothing about one; the size of the stream it relays is
            // the one fact it can truthfully give, once known.
            for key in FILE_ONLY_FIELDS {
                match (*key, size) {
                    ("size", Some(size)) => {
                        fields.insert("size".into(), size.into());
                    }
                    _ => {
                        fields.shift_remove(*key);
                    }
                }
            }
            fields.insert("isExternal".into(), true.into());
        }

        fields
    }

    /// An outside song that streams from YouTube: once the stream is known, the suffix, content
    /// type and bit rate of the format it is served in, and its exact size (returned). A stream
    /// not known yet keeps the m4a declaration and goes without a size, and is asked about for
    /// next time. Wait-for-lossless rows never come here: their bytes are not known until the
    /// FLAC lands.
    fn declare_outside_stream(&self, fields: &mut Fields, song_id: &str) -> Option<u64> {
        let facts = self.id_registry.stream_facts(song_id).unwrap_or_default();
        if let Some(declared) = facts.declared() {
            fields.insert("suffix".into(), declared.suffix.into());
            fields.insert("contentType".into(), declared.content_type.into());
            match declared.bit_rate {
                Some(rate) => fields.insert("bitRate".into(), rate.into()),
                None => fields.shift_remove("bitRate"),
            };
        }
        // ReplayGain measured on the very stream that plays (OpenSubsonic's replayGain).
        if let Some(gain) = facts.track_gain {
            let mut replay_gain = Fields::new();
            replay_gain.insert("trackGain".into(), json!(gain));
            if let Some(peak) = facts.track_peak {
                replay_gain.insert("trackPeak".into(), json!(peak));
            }
            fields.insert("replayGain".into(), Value::Object(replay_gain));
        }
        self.id_registry.outside_listed(song_id);
        if facts.size.is_none() {
            self.id_registry.size_wanted(song_id);
        }
        facts.size
    }

    /// Converts an Album domain model to Subsonic JSON format.
    pub fn convert_album_to_json(&self, album: &Album) -> Fields {
        self.build_album_fields(album)
    }

    /// The album shape both serializers render. A client browsing by folder rather than by
    /// tags reads `title`, `isDir` and `parent`; emitting only `name` left injected albums
    /// looking unlike anything the upstream server returns.
    fn build_album_fields(&self, album: &Album) -> Fields {
        let artist_id = album.artist_id.clone().unwrap_or_else(|| {
            self.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some(album.artist.clone()),
                ..Default::default()
            })
        });
        let duration: i64 = album
            .songs
            .iter()
            .map(|s| i64::from(s.duration.unwrap_or(0)))
            .sum();

        let mut fields = Fields::new();
        fields.insert("id".into(), album.id.clone().into());
        fields.insert("parent".into(), artist_id.clone().into());
        fields.insert("isDir".into(), true.into());
        fields.insert("title".into(), album.title.clone().into());
        fields.insert("name".into(), album.title.clone().into());
        fields.insert("album".into(), album.title.clone().into());
        fields.insert("artist".into(), album.artist.clone().into());
        fields.insert("artistId".into(), artist_id.into());
        fields.insert("songCount".into(), album.song_count.unwrap_or(0).into());
        fields.insert("duration".into(), duration.into());
        fields.insert("genre".into(), album.genre.clone().unwrap_or_default().into());
        fields.insert("coverArt".into(), album.id.clone().into());
        fields.insert("created".into(), millis(&self.clock.now()).into());
        fields.insert("mediaType".into(), "album".into());
        fields.insert("displayArtist".into(), album.artist.clone().into());
        // OpenSubsonic asks for the list even when it is empty, so a client knows the server
        // speaks it. Only an outside album comes through here; a library album keeps the
        // types Navidrome gave it.
        fields.insert("releaseTypes".into(), album.release_types.clone().into());
        fields.insert(
            "explicitStatus".into(),
            explicit_status::for_clients(album.explicit_content_lyrics).into(),
        );
        fields.insert("sortName".into(), to_lower_invariant(&album.title).into());
        fields.insert("isExternal".into(), (!album.is_local).into());

        // Deezer's album search payload carries no release date; only the per-album detail
        // call does, and fetching that for every search row would be twenty extra requests
        // against a budget this project has already been burned by. So the year is genuinely
        // unknown here and is left out rather than reported as zero.
        if let Some(year) = album.year {
            fields.insert("year".into(), year.into());
        }
        if let Some(owned) = album.owned_count {
            fields.insert("ownedCount".into(), owned.into());
        }

        fields
    }

    /// Converts an Artist domain model to Subsonic JSON format.
    pub fn convert_artist_to_json(&self, artist: &Artist) -> Fields {
        build_artist_fields(artist)
    }

    /// Converts a Song domain model to Subsonic XML format.
    pub fn convert_song_to_xml(&self, song: &Song, ns: Option<&str>) -> XElement {
        let fields = self.convert_song_fields(song);
        let mut element = XElement::in_namespace(ns, "song");
        attributes(&mut element, &fields);
        text_lists(&mut element, &fields, ns);
        // OpenSubsonic's replayGain is a child element in XML; only a measured one is written.
        if let Some(gain) = fields
            .get("replayGain")
            .filter(|gain| gain.as_object().is_some_and(|g| !g.is_empty()))
        {
            element.push(json_shape_to_xml(ns, "replayGain", gain));
        }
        element
    }

    /// Converts an Album domain model to Subsonic XML format.
    pub fn convert_album_to_xml(&self, album: &Album, ns: Option<&str>) -> XElement {
        let fields = self.build_album_fields(album);
        let mut element = XElement::in_namespace(ns, "album");
        attributes(&mut element, &fields);
        text_lists(&mut element, &fields, ns);
        element
    }

    /// Converts an Artist domain model to Subsonic XML format.
    pub fn convert_artist_to_xml(&self, artist: &Artist, ns: Option<&str>) -> XElement {
        let mut element = XElement::in_namespace(ns, "artist");
        attributes(&mut element, &build_artist_fields(artist));
        element
    }

    /// Converts a Subsonic JSON element to a dictionary: every property converted as
    /// [`convert_json_value`] does, then `isExternal` set. None when the element is not an
    /// object.
    pub fn convert_subsonic_json_element(&self, element: &Value, is_local: bool) -> Option<Value> {
        let Value::Object(properties) = element else {
            return None;
        };
        let mut dict = Fields::new();
        for (name, value) in properties {
            dict.insert(name.clone(), convert_json_value(value));
        }
        dict.insert("isExternal".into(), (!is_local).into());
        Some(Value::Object(dict))
    }

    /// Converts a Subsonic XML element: a copy, marked as not external.
    pub fn convert_subsonic_xml_element(&self, element: &XElement, _type: &str) -> XElement {
        let mut copy = element.clone();
        copy.set_attr("isExternal", "false");
        copy
    }

    /// The clock the builder reads for "now".
    pub fn clock(&self) -> &Clock {
        &self.clock
    }
}

fn build_artist_fields(artist: &Artist) -> Fields {
    let mut fields = Fields::new();
    fields.insert("id".into(), artist.id.clone().into());
    fields.insert("name".into(), artist.name.clone().into());
    fields.insert("albumCount".into(), artist.album_count.unwrap_or(0).into());
    fields.insert("coverArt".into(), artist.id.clone().into());
    fields.insert("isExternal".into(), (!artist.is_local).into());
    fields
}

/// A song's length for a list's total: an unknown length counts as nothing.
fn known_seconds(duration: Option<i32>) -> i32 {
    duration.unwrap_or(0).max(0)
}

/// The `subsonic-response` root in the Subsonic namespace, with its status and version.
fn envelope(status: &str) -> XElement {
    XElement::ns(SUBSONIC_NAMESPACE, "subsonic-response")
        .attr("status", status)
        .attr("version", SUBSONIC_VERSION)
}

/// An element in the Subsonic namespace.
fn subsonic_element(name: impl Into<String>) -> XElement {
    XElement::ns(SUBSONIC_NAMESPACE, name)
}

/// `yyyy-MM-ddTHH:mm:ss.fffZ`: milliseconds cut, not rounded.
pub(crate) fn millis(value: &DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `yyyy-MM-ddTHH:mm:ss`.
fn seconds(value: &DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn year_of(value: &DateTime<Utc>) -> i32 {
    value.year()
}

/// `"🎵 {Provider with a capital}{ curator}"`, the artist a playlist shown as an album gets.
/// (Every provider is a fixed non-empty name.)
pub(crate) fn curator_artist_name(playlist: &ExternalPlaylist) -> String {
    let mut chars = playlist.provider.chars();
    let mut name = String::from("\u{1F3B5} ");
    if let Some(first) = chars.next() {
        name.push(to_upper_char(first));
        name.push_str(chars.as_str());
    }
    if let Some(curator) = playlist.curator_name.as_deref().filter(|c| !c.is_empty()) {
        name.push(' ');
        name.push_str(curator);
    }
    name
}

/// `curator-{provider}-{curator, lowercase, dashes for spaces}`, or `-unknown` without one.
pub(crate) fn curator_artist_id(playlist: &ExternalPlaylist) -> String {
    let curator = playlist
        .curator_name
        .as_deref()
        .map(|c| to_lower_invariant(c).replace(' ', "-"))
        .unwrap_or_else(|| "unknown".to_string());
    format!("curator-{}-{}", playlist.provider, curator)
}

/// JSON-shaped data as a Subsonic XML element, by OpenSubsonic's rules: a plain value is
/// an attribute, one object is a child element (replayGain), a list of objects is one child
/// element per object named for the list (song, genres, artists), and a list of plain
/// values is one text element per value (isrc). Missing values are left out.
pub fn json_shape_to_xml(ns: Option<&str>, name: &str, value: &Value) -> XElement {
    let mut element = XElement::in_namespace(ns, name);
    let Value::Object(fields) = value else {
        if !value.is_null() {
            element = element.text(scalar(value));
        }
        return element;
    };

    for (key, field) in fields {
        match field {
            Value::Null => {}
            Value::String(text) => element.set_attr(key.as_str(), text.as_str()),
            Value::Object(_) => element.push(json_shape_to_xml(ns, key, field)),
            Value::Array(list) => {
                for item in list {
                    if !item.is_null() {
                        element.push(json_shape_to_xml(ns, key, item));
                    }
                }
            }
            _ => element.set_attr(key.as_str(), scalar(field)),
        }
    }
    element
}

/// Renders the shared field set as XML attributes.
///
/// The two serializers used to be written out by hand, separately, and drifted badly:
/// XML emitted nine attributes for a song where JSON emitted twenty-seven, so an
/// XML-only client received external tracks with no `suffix`, `contentType`
/// or `bitRate` at all: the fields a client picks its decoder from, and the ones
/// this file already warns must describe the bytes that will actually arrive. Deriving
/// one from the other is what stops that happening again.
pub(crate) fn attributes(element: &mut XElement, fields: &Fields) {
    for (name, value) in fields {
        match value {
            Value::Null => {}
            // Subsonic carries collections as child elements, not attributes. The lists of
            // plain text among them are written by text_lists; the rest are emitted empty.
            Value::Array(_) | Value::Object(_) => {}
            _ => element.set_attr(name.as_str(), scalar(value)),
        }
    }
}

/// A list of plain text, such as OpenSubsonic's `isrc`, as one child element per value:
/// `<isrc>USRC17607839</isrc>`, the shape the upstream server writes. A list holding anything
/// but strings is skipped; every such list the builder makes holds objects.
pub(crate) fn text_lists(element: &mut XElement, fields: &Fields, ns: Option<&str>) {
    for (name, value) in fields {
        let Value::Array(items) = value else { continue };
        if !items.iter().all(Value::is_string) {
            continue;
        }
        for item in items {
            if let Value::String(text) = item {
                element.push(XElement::in_namespace(ns, name.as_str()).text(text.as_str()));
            }
        }
    }
}

/// The scalar fields of a Navidrome entry as attributes: strings, numbers as written, and
/// booleans. Objects, lists and nulls are left out.
pub(crate) fn scalar_attributes(element: &mut XElement, entry: &Node) {
    let Some(fields) = entry.as_object() else { return };
    for (name, node) in fields {
        let text = match node {
            Node::String(s) => s.clone(),
            Node::Number(n) => n.clone(),
            Node::Bool(true) => "true".to_string(),
            Node::Bool(false) => "false".to_string(),
            _ => continue,
        };
        element.set_attr(name.as_str(), text);
    }
}

/// Invariant rendering. A comma decimal separator under a European locale would produce
/// numbers no Subsonic client can parse.
pub(crate) fn scalar(value: &Value) -> String {
    match value {
        Value::Bool(b) => b.to_xml_value(),
        Value::Number(n) => number_text(n),
        Value::String(s) => s.clone(),
        // A list or an object has no scalar text; it is written as a fixed type name.
        Value::Array(_) => "System.Collections.Generic.List`1[System.Object]".to_string(),
        Value::Object(_) => {
            "System.Collections.Generic.Dictionary`2[System.String,System.Object]".to_string()
        }
        Value::Null => String::new(),
    }
}

/// A number as text: integers as they are, doubles in the round-trip format.
pub(crate) fn number_text(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else {
        format_double(n.as_f64().unwrap_or(0.0))
    }
}

/// A converted JSON value as text: what the merge compares artist and title keys by.
/// Booleans are `True`/`False`.
pub(crate) fn text_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::Bool(true) => Some("True".to_string()),
        Value::Bool(false) => Some("False".to_string()),
        other => Some(scalar(other)),
    }
}

/// A JSON value as the merge keeps it. Numbers become an `i32` when they are an integer that fits
/// one, and an `f64` otherwise, which is how they are written back (`3000000000` and `1.0` come
/// back as `3000000000` and `1`, and `1e20` as `1E+20`).
pub fn convert_json_value(value: &Value) -> Value {
    match value {
        Value::Number(n) => match n.as_i64() {
            Some(i) if i32::try_from(i).is_ok() => Value::from(i),
            _ => n
                .as_f64()
                .and_then(Number::from_f64)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        },
        Value::Array(items) => Value::Array(items.iter().map(convert_json_value).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), convert_json_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A builder-made value as a [`Node`], numbers written as [`number_text`] writes them.
pub(crate) fn to_node(value: &Value) -> Node {
    match value {
        Value::Null => Node::Null,
        Value::Bool(b) => Node::Bool(*b),
        Value::Number(n) => Node::Number(number_text(n)),
        Value::String(s) => Node::String(s.clone()),
        Value::Array(items) => Node::Array(items.iter().map(to_node).collect()),
        Value::Object(fields) => Node::Object(fields.iter().map(|(k, v)| (k.clone(), to_node(v))).collect()),
    }
}

/// A JSON node as an `i32`: an integer literal that fits, or nothing.
fn node_int(node: &Node) -> Option<i32> {
    match node {
        Node::Number(text) => text.parse::<i32>().ok(),
        _ => None,
    }
}

/// The JSON branch of the extensions merge. None for a body it cannot read or does not
/// recognize, which hands the body back untouched.
fn merge_extensions_json(upstream: &[u8], own: &[(&str, &[i32])]) -> Option<String> {
    let text = std::str::from_utf8(upstream).ok()?;
    let mut root = Node::parse(text).ok()?;
    let root_map = root.as_object_mut()?;
    let envelope = root_map.get_mut("subsonic-response")?.as_object_mut()?;
    let status = match envelope.get("status") {
        None | Some(Node::Null) => None,
        Some(Node::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    if status.as_deref() == Some("ok") {
        if !matches!(envelope.get("openSubsonicExtensions"), Some(Node::Array(_))) {
            envelope.insert("openSubsonicExtensions".into(), Node::Array(Vec::new()));
        }
        let Some(Node::Array(list)) = envelope.get_mut("openSubsonicExtensions") else {
            return None;
        };
        for (name, versions) in own {
            let mut existing_index = None;
            for (index, item) in list.iter().enumerate() {
                let item_name = match item {
                    Node::Null => None,
                    Node::Object(fields) => match fields.get("name") {
                        None | Some(Node::Null) => None,
                        Some(Node::String(s)) => Some(s.as_str()),
                        Some(_) => return None,
                    },
                    _ => return None,
                };
                if item_name == Some(*name) {
                    existing_index = Some(index);
                    break;
                }
            }
            let existing = existing_index.and_then(|index| match &list[index] {
                Node::Object(_) => Some(index),
                _ => None,
            });
            let mut have: Vec<i32> = Vec::new();
            if let Some(index) = existing
                && let Some(Node::Array(listed)) = list[index].get("versions")
            {
                for version in listed {
                    let number = match version {
                        Node::Null => 0,
                        other => node_int(other)?,
                    };
                    if !have.contains(&number) {
                        have.push(number);
                    }
                }
            }
            let mut all: Vec<i32> = have
                .iter()
                .chain(versions.iter())
                .copied()
                .filter(|v| *v > 0)
                .collect();
            all.sort_unstable();
            all.dedup();
            let as_nodes = || Node::Array(all.iter().map(|v| Node::Number(v.to_string())).collect());
            match existing {
                None => {
                    let mut entry = indexmap::IndexMap::new();
                    entry.insert("name".to_string(), Node::String((*name).to_string()));
                    entry.insert("versions".to_string(), as_nodes());
                    list.push(Node::Object(entry));
                }
                Some(index) if all.len() != have.len() => {
                    if let Node::Object(fields) = &mut list[index] {
                        fields.insert("versions".to_string(), as_nodes());
                    }
                }
                Some(_) => {}
            }
        }
    }
    Some(root.to_json_string(false))
}

/// The XML branch of the extensions merge. None when the body is not XML.
fn merge_extensions_xml(upstream: &[u8], own: &[(&str, &[i32])]) -> Option<String> {
    let text = String::from_utf8_lossy(upstream);
    let mut response = XElement::parse(&text).ok()?;
    if response.attribute("status") == Some("ok") {
        let ns = response.namespace.clone();
        for (name, versions) in own {
            let existing = response.content.iter().position(|node| {
                matches!(node, crate::xml::XNode::Element(e)
                    if e.is(ns.as_deref(), "openSubsonicExtensions") && e.attribute("name") == Some(*name))
            });
            let Some(index) = existing else {
                response.push(
                    XElement::in_namespace(ns.as_deref(), "openSubsonicExtensions")
                        .attr("name", *name)
                        .children(
                            versions.iter().map(|v| {
                                XElement::in_namespace(ns.as_deref(), "versions").text(v.to_string())
                            }),
                        ),
                );
                continue;
            };
            let crate::xml::XNode::Element(existing) = &mut response.content[index] else {
                continue;
            };
            let have: HashSet<i32> = existing
                .elements_named(ns.as_deref(), "versions")
                .map(|version| dotnet_int_parse(&version.value()).unwrap_or(0))
                .collect();
            for version in versions.iter().filter(|v| !have.contains(v)) {
                existing.push(XElement::in_namespace(ns.as_deref(), "versions").text(version.to_string()));
            }
        }
    }
    Some(response.to_xml_string())
}

/// A 32-bit integer: optional surrounding whitespace and a leading sign, then digits.
fn dotnet_int_parse(text: &str) -> Option<i32> {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix('+').unwrap_or(trimmed);
    if digits.starts_with('+') {
        return None;
    }
    digits.parse::<i32>().ok()
}
