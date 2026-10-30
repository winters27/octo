//! Tests of the Subsonic response shapes, including the lyrics answers and their word timing,
//! with exact byte checks of the lyrics, info, error and song row answers.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use octo_core::common::Clock;
use octo_core::lyrics::lyrics_choices::LyricsChoiceCandidate;
use octo_core::lyrics::lyrics_models::LyricsResult;
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioTrack};
use octo_core::models::subsonic::ExternalPlaylist;
use octo_core::settings::{LibraryActionSettings, SubsonicSettings};
use octo_core::soulseek::SoulseekRouting;
use serde_json::{Value, json};

use super::*;
use crate::xml::XElement;

/// Stands in for `ExternalIdRegistry`: the same id for the same routing.
struct TestIds;

impl IdRegistry for TestIds {
    fn register(&self, routing: SoulseekRouting) -> String {
        format!(
            "{:?}:{}:{}",
            routing.kind,
            routing.artist.unwrap_or_default(),
            routing.album.unwrap_or_default()
        )
    }
}

const NS: Option<&str> = Some(SUBSONIC_NAMESPACE);

fn builder() -> SubsonicResponseBuilder {
    builder_with(false)
}

fn builder_with(wait_for_lossless: bool) -> SubsonicResponseBuilder {
    SubsonicResponseBuilder::new(
        Arc::new(TestIds),
        &SubsonicSettings {
            wait_for_lossless_on_play: wait_for_lossless,
            ..Default::default()
        },
    )
}

fn json_of(reply: &SubsonicReply) -> Value {
    assert_eq!(reply.kind, ReplyKind::Json, "a JsonResult");
    reply.json_value().expect("JSON")
}

fn xml_of(reply: &SubsonicReply) -> XElement {
    assert_eq!(reply.kind, ReplyKind::Content, "a ContentResult");
    assert_eq!(reply.content_type, "application/xml");
    XElement::parse(&reply.text()).expect("XML")
}

fn child<'a>(root: &'a XElement, name: &str) -> &'a XElement {
    root.elements()
        .find(|e| e.is(NS, name))
        .unwrap_or_else(|| panic!("no {name}"))
}

#[test]
fn create_response_json_format_returns_json_with_ok_status() {
    let json = json_of(&builder().create_response("json", "testElement"));
    assert_eq!(json["subsonic-response"]["status"], "ok");
    assert_eq!(json["subsonic-response"]["version"], "1.16.1");
}

#[test]
fn create_response_xml_format_returns_xml_with_ok_status() {
    let root = xml_of(&builder().create_response("xml", "testElement"));
    assert_eq!(root.name, "subsonic-response");
    assert_eq!(root.attribute("status"), Some("ok"));
    assert_eq!(root.attribute("version"), Some("1.16.1"));
}

#[test]
fn create_error_json_format_returns_json_with_error() {
    let json = json_of(&builder().create_error("json", 70, "Test error message"));
    let response = &json["subsonic-response"];
    assert_eq!(response["status"], "failed");
    assert_eq!(response["error"]["code"], 70);
    assert_eq!(response["error"]["message"], "Test error message");
}

#[test]
fn create_error_xml_format_returns_xml_with_error() {
    let root = xml_of(&builder().create_error("xml", 70, "Test error message"));
    assert_eq!(root.attribute("status"), Some("failed"));
    let error = child(&root, "error");
    assert_eq!(error.attribute("code"), Some("70"));
    assert_eq!(error.attribute("message"), Some("Test error message"));
}

#[test]
fn create_song_response_json_format_returns_song_data() {
    let song = Song {
        id: "song123".into(),
        title: "Test Song".into(),
        artist: "Test Artist".into(),
        album: "Test Album".into(),
        duration: Some(180),
        track: Some(5),
        year: Some(2023),
        genre: Some("Rock".into()),
        local_path: Some("/music/test.mp3".into()),
        ..Default::default()
    };
    let json = json_of(&builder().create_song_response("json", &song));
    let data = &json["subsonic-response"]["song"];
    assert_eq!(data["id"], "song123");
    assert_eq!(data["title"], "Test Song");
    assert_eq!(data["artist"], "Test Artist");
    assert_eq!(data["album"], "Test Album");
}

#[test]
fn create_song_response_xml_format_returns_song_data() {
    let song = Song {
        id: "song123".into(),
        title: "Test Song".into(),
        artist: "Test Artist".into(),
        album: "Test Album".into(),
        duration: Some(180),
        ..Default::default()
    };
    let root = xml_of(&builder().create_song_response("xml", &song));
    let element = child(&root, "song");
    assert_eq!(element.attribute("id"), Some("song123"));
    assert_eq!(element.attribute("title"), Some("Test Song"));
}

#[test]
fn an_outside_song_with_no_known_length_goes_out_without_one() {
    for duration in [None, Some(0), Some(-1)] {
        let song = Song {
            id: "ext-1".into(),
            title: "Genesis".into(),
            artist: "Justice".into(),
            duration,
            is_local: false,
            ..Default::default()
        };
        let json = json_of(&builder().create_song_response("json", &song));
        let data = &json["subsonic-response"]["song"];
        assert!(data.get("duration").is_none(), "{duration:?}: {data}");
        assert_eq!(data["id"], "ext-1");
        let root = xml_of(&builder().create_song_response("xml", &song));
        assert_eq!(child(&root, "song").attribute("duration"), None, "{duration:?}");
    }
}

#[test]
fn an_outside_song_with_a_known_length_carries_it() {
    let song = Song {
        id: "ext-1".into(),
        title: "Genesis".into(),
        artist: "Justice".into(),
        duration: Some(234),
        is_local: false,
        ..Default::default()
    };
    let json = json_of(&builder().create_song_response("json", &song));
    assert_eq!(json["subsonic-response"]["song"]["duration"], 234);
    let root = xml_of(&builder().create_song_response("xml", &song));
    assert_eq!(child(&root, "song").attribute("duration"), Some("234"));
}

fn song(id: &str, title: &str, duration: Option<i32>) -> Song {
    Song {
        id: id.into(),
        title: title.into(),
        duration,
        ..Default::default()
    }
}

#[test]
fn create_album_response_json_format_returns_album_with_songs() {
    let album = Album {
        id: "album123".into(),
        title: "Test Album".into(),
        artist: "Test Artist".into(),
        year: Some(2023),
        songs: vec![
            song("song1", "Song 1", Some(180)),
            song("song2", "Song 2", Some(200)),
        ],
        ..Default::default()
    };
    let json = json_of(&builder().create_album_response("json", &album));
    let data = &json["subsonic-response"]["album"];
    assert_eq!(data["id"], "album123");
    assert_eq!(data["name"], "Test Album");
    assert_eq!(data["songCount"], 2);
    assert_eq!(data["duration"], 380);
}

#[test]
fn create_album_response_xml_format_returns_album_with_songs() {
    let album = Album {
        id: "album123".into(),
        title: "Test Album".into(),
        artist: "Test Artist".into(),
        song_count: Some(2),
        songs: vec![song("song1", "Song 1", None), song("song2", "Song 2", None)],
        ..Default::default()
    };
    let root = xml_of(&builder().create_album_response("xml", &album));
    let element = child(&root, "album");
    assert_eq!(element.attribute("id"), Some("album123"));
    assert_eq!(element.attribute("songCount"), Some("2"));
}

/// An external album knows its count from the tracklist, not SongCount. The XML
/// branch used to report 0 in that case while the JSON branch reported the real
/// number, so an XML client saw an empty-looking album.
#[test]
fn create_album_response_xml_format_derives_song_count_from_tracklist() {
    let album = Album {
        id: "album123".into(),
        title: "Test Album".into(),
        artist: "Test Artist".into(),
        songs: vec![
            song("song1", "Song 1", None),
            song("song2", "Song 2", None),
            song("song3", "Song 3", None),
        ],
        ..Default::default()
    };
    let root = xml_of(&builder().create_album_response("xml", &album));
    assert_eq!(child(&root, "album").attribute("songCount"), Some("3"));
}

fn two_albums() -> Vec<Album> {
    vec![
        Album {
            id: "album1".into(),
            title: "Album 1".into(),
            ..Default::default()
        },
        Album {
            id: "album2".into(),
            title: "Album 2".into(),
            ..Default::default()
        },
    ]
}

#[test]
fn create_artist_response_json_format_returns_artist_data() {
    let artist = Artist {
        id: "artist123".into(),
        name: "Test Artist".into(),
        ..Default::default()
    };
    let json = json_of(&builder().create_artist_response("json", &artist, &two_albums()));
    let data = &json["subsonic-response"]["artist"];
    assert_eq!(data["id"], "artist123");
    assert_eq!(data["name"], "Test Artist");
    assert_eq!(data["albumCount"], 2);
}

#[test]
fn create_artist_response_xml_format_returns_artist_data() {
    let artist = Artist {
        id: "artist123".into(),
        name: "Test Artist".into(),
        ..Default::default()
    };
    let root = xml_of(&builder().create_artist_response("xml", &artist, &two_albums()));
    let element = child(&root, "artist");
    assert_eq!(element.attribute("id"), Some("artist123"));
    assert_eq!(element.attribute("name"), Some("Test Artist"));
    assert_eq!(element.attribute("albumCount"), Some("2"));
}

#[test]
fn create_song_response_song_with_null_values_handles_gracefully() {
    let json = json_of(&builder().create_song_response("json", &song("song123", "Test Song", None)));
    let data = &json["subsonic-response"]["song"];
    assert_eq!(data["id"], "song123");
    assert_eq!(data["title"], "Test Song");
}

#[test]
fn create_album_response_empty_song_list_returns_zero_counts() {
    let album = Album {
        id: "album123".into(),
        title: "Empty Album".into(),
        artist: "Test Artist".into(),
        ..Default::default()
    };
    let json = json_of(&builder().create_album_response("json", &album));
    let data = &json["subsonic-response"]["album"];
    assert_eq!(data["songCount"], 0);
    assert_eq!(data["duration"], 0);
}

// ---- Declared format must match the bytes that will arrive -----------------
// A Subsonic client picks its decoder from suffix/contentType, so declaring one
// thing and serving another makes playback fail silently rather than error. The
// comment above ConvertSongToJson records that this was already shipped wrong once.

fn external_song() -> Song {
    Song {
        id: "abc123".into(),
        title: "Teardrop".into(),
        artist: "Massive Attack".into(),
        duration: Some(330),
        is_local: false,
        external_provider: Some("soulseek".into()),
        external_id: Some("abc123".into()),
        ..Default::default()
    }
}

#[test]
fn external_song_declares_lossy_when_not_waiting_for_lossless() {
    let row = builder_with(false).convert_song_to_json(&external_song());
    assert_eq!(row["suffix"], "m4a");
    assert_eq!(row["contentType"], "audio/mp4");
    assert_eq!(row["bitRate"], 128);
}

/// With the wait enabled, /rest/stream serves the fetched FLAC under this same id,
/// so the row has to say so. Declaring m4a here is the exact mismatch that leaves
/// players stuck on "loading".
#[test]
fn external_song_declares_lossless_when_waiting_for_lossless() {
    let row = builder_with(true).convert_song_to_json(&external_song());
    assert_eq!(row["suffix"], "flac");
    assert_eq!(row["contentType"], "audio/flac");
    // A FLAC's rate is unknown until it is fetched, so none is claimed.
    assert!(!row.contains_key("bitRate"));
}

/// `TestIds` that also knows the stream of some songs, and notes the ones asked about.
#[derive(Default)]
struct StreamIds {
    facts: std::collections::HashMap<String, StreamFacts>,
    wanted: std::sync::Mutex<Vec<String>>,
    listed: std::sync::Mutex<Vec<String>>,
}

impl IdRegistry for StreamIds {
    fn register(&self, routing: SoulseekRouting) -> String {
        TestIds.register(routing)
    }

    fn stream_facts(&self, song_id: &str) -> Option<StreamFacts> {
        self.facts.get(song_id).cloned()
    }

    fn size_wanted(&self, song_id: &str) {
        self.wanted
            .lock()
            .expect("not poisoned")
            .push(song_id.to_string());
    }

    fn outside_listed(&self, song_id: &str) {
        self.listed
            .lock()
            .expect("not poisoned")
            .push(song_id.to_string());
    }
}

fn builder_over(ids: Arc<StreamIds>, wait_for_lossless: bool) -> SubsonicResponseBuilder {
    SubsonicResponseBuilder::new(
        ids,
        &SubsonicSettings {
            wait_for_lossless_on_play: wait_for_lossless,
            ..Default::default()
        },
    )
}

fn known_stream(itag: &str, ext: &str, size: u64, bit_rate: Option<i32>) -> Arc<StreamIds> {
    let mut ids = StreamIds::default();
    ids.facts.insert(
        "abc123".into(),
        StreamFacts {
            size: Some(size),
            itag: Some(itag.into()),
            ext: Some(ext.into()),
            bit_rate,
            ..Default::default()
        },
    );
    Arc::new(ids)
}

#[test]
fn an_outside_song_with_a_known_stream_carries_its_true_size_in_json_and_xml() {
    let ids = known_stream("140", "m4a", 6_755_585, Some(129));
    let builder = builder_over(ids.clone(), false);
    let row = builder.convert_song_to_json(&external_song());
    assert_eq!(row["size"], 6_755_585);
    assert_eq!(row["suffix"], "m4a");
    assert_eq!(row["contentType"], "audio/mp4");
    assert_eq!(row["bitRate"], 128);
    // In the place a library song carries it, and still no other file facts.
    let keys: Vec<&str> = row.keys().map(String::as_str).collect();
    let at = |key: &str| keys.iter().position(|k| *k == key);
    assert!(at("size") < at("contentType"), "{keys:?}");
    assert!(!row.contains_key("path") && !row.contains_key("created"));

    let xml = builder.convert_song_to_xml(&external_song(), NS);
    assert_eq!(xml.attribute("size"), Some("6755585"));
    assert_eq!(xml.attribute("suffix"), Some("m4a"));
    assert!(ids.wanted.lock().expect("not poisoned").is_empty());
}

#[test]
fn an_outside_song_served_as_opus_says_so() {
    let builder = builder_over(known_stream("251", "webm", 7_012_345, Some(160)), false);
    let row = builder.convert_song_to_json(&external_song());
    assert_eq!(row["size"], 7_012_345);
    assert_eq!(row["suffix"], "webm");
    assert_eq!(row["contentType"], "audio/webm");
    assert_eq!(row["bitRate"], 160);

    let builder = builder_over(known_stream("251", "webm", 7_012_345, None), false);
    assert!(
        !builder
            .convert_song_to_json(&external_song())
            .contains_key("bitRate")
    );
}

#[test]
fn an_outside_song_without_a_known_stream_has_no_size_and_is_asked_about() {
    let ids = Arc::new(StreamIds::default());
    let row = builder_over(ids.clone(), false).convert_song_to_json(&external_song());
    assert!(!row.contains_key("size"));
    assert_eq!(row["suffix"], "m4a");
    assert_eq!(
        *ids.wanted.lock().expect("not poisoned"),
        vec!["abc123".to_string()]
    );
}

/// The registry hears of every outside row an answer lists, sized or not, so it can tell a
/// short list from a whole catalog.
#[test]
fn every_outside_row_is_counted_as_listed_sized_or_not() {
    let sized = known_stream("140", "m4a", 6_755_585, Some(129));
    builder_over(sized.clone(), false).convert_song_to_json(&external_song());
    assert_eq!(
        *sized.listed.lock().expect("not poisoned"),
        vec!["abc123".to_string()]
    );

    let unsized_ids = Arc::new(StreamIds::default());
    builder_over(unsized_ids.clone(), false).convert_song_to_xml(&external_song(), NS);
    assert_eq!(
        *unsized_ids.listed.lock().expect("not poisoned"),
        vec!["abc123".to_string()]
    );
}

#[test]
fn an_outside_song_with_measured_loudness_sends_its_replay_gain() {
    let mut ids = StreamIds::default();
    ids.facts.insert(
        "abc123".into(),
        StreamFacts {
            size: Some(6_755_585),
            itag: Some("140".into()),
            track_gain: Some(-10.0),
            track_peak: Some(1.135011),
            ..Default::default()
        },
    );
    let ids = Arc::new(ids);
    let builder = builder_over(ids.clone(), false);
    let row = builder.convert_song_to_json(&external_song());
    assert_eq!(
        row["replayGain"],
        json!({"trackGain": -10.0, "trackPeak": 1.135011})
    );
    let xml = builder.convert_song_to_xml(&external_song(), NS);
    let gain = xml
        .elements()
        .find(|e| e.is(NS, "replayGain"))
        .expect("a replayGain element");
    assert_eq!(gain.attribute("trackGain"), Some("-10"));
    assert_eq!(gain.attribute("trackPeak"), Some("1.135011"));

    // Not measured yet: the empty object every row carries.
    let row = builder_over(Arc::new(StreamIds::default()), false).convert_song_to_json(&external_song());
    assert_eq!(row["replayGain"], json!({}));
    // Waiting for lossless: the FLAC is not the stream that was measured.
    let row = builder_over(ids, true).convert_song_to_json(&external_song());
    assert_eq!(row["replayGain"], json!({}));
}

#[test]
fn a_lossless_wait_never_sends_a_size_even_when_the_stream_is_known() {
    let ids = known_stream("140", "m4a", 6_755_585, Some(129));
    let row = builder_over(ids.clone(), true).convert_song_to_json(&external_song());
    assert!(!row.contains_key("size"));
    assert_eq!(row["suffix"], "flac");
    assert!(ids.wanted.lock().expect("not poisoned").is_empty());
}

#[test]
fn local_song_always_declares_flac_regardless_of_setting() {
    let local = Song {
        is_local: true,
        ..external_song()
    };
    for waiting in [false, true] {
        let row = builder_with(waiting).convert_song_to_json(&local);
        assert_eq!(row["suffix"], "flac", "waiting {waiting}");
        assert_eq!(row["bitRate"], 1411, "waiting {waiting}");
    }
}

// ---- ISRCs: OpenSubsonic's isrc is a list, in both formats ---------------------------

#[test]
fn external_song_with_an_isrc_lists_it() {
    let song = Song {
        isrc: Some("gb-a1b-98-00001".into()),
        ..external_song()
    };
    assert_eq!(
        builder().convert_song_to_json(&song)["isrc"],
        json!(["GBA1B9800001"])
    );
}

#[test]
fn external_song_with_no_valid_isrc_lists_none() {
    let mut song = external_song();
    assert_eq!(builder().convert_song_to_json(&song)["isrc"], json!([]));
    song.isrc = Some("not an isrc".into());
    assert_eq!(builder().convert_song_to_json(&song)["isrc"], json!([]));
}

/// A library song Octo rebuilt from Navidrome's answer goes back out with the
/// codes it came in with, untouched, not normalized and not dropped.
#[test]
fn library_song_keeps_navidromes_isrcs_untouched() {
    let song = Song {
        is_local: true,
        isrc: Some("USRC17600001".into()),
        isrcs: vec!["GBA1B9800001".into(), "us-rc1-76-07839".into()],
        ..external_song()
    };
    assert_eq!(
        builder().convert_song_to_json(&song)["isrc"],
        json!(["GBA1B9800001", "us-rc1-76-07839"])
    );
}

#[test]
fn xml_song_lists_each_isrc_as_a_child_element() {
    let song = Song {
        is_local: true,
        isrcs: vec!["GBA1B9800001".into(), "USRC17607839".into()],
        ..external_song()
    };
    let xml = builder().convert_song_to_xml(&song, NS);
    let values: Vec<String> = xml.elements_named(NS, "isrc").map(XElement::value).collect();
    assert_eq!(values, ["GBA1B9800001", "USRC17607839"]);
    assert_eq!(xml.attribute("isrc"), None);
}

// ---- Issue #35: the album DETAIL shape was missing `created` -----------------
// Strict OpenSubsonic clients validate before playing: Music Assistant rejected every
// external album with "Field created of type str is missing in AlbumID3WithSongs".
// The album ROW shape (BuildAlbumFields) always sent it, so the two disagreed and only
// the detail call broke.

fn plain_album() -> Album {
    Album {
        id: "album123".into(),
        title: "Test Album".into(),
        artist: "Test Artist".into(),
        ..Default::default()
    }
}

#[test]
fn create_album_response_json_format_carries_created() {
    let json = json_of(&builder().create_album_response("json", &plain_album()));
    let created = json["subsonic-response"]["album"]["created"]
        .as_str()
        .expect("created");
    assert!(
        octo_core::json::datetime::parse_utc(created).is_some(),
        "{created}"
    );
}

#[test]
fn create_album_response_xml_format_carries_created() {
    let root = xml_of(&builder().create_album_response("xml", &plain_album()));
    let created = child(&root, "album").attribute("created").expect("created");
    assert!(
        octo_core::json::datetime::parse_utc(created).is_some(),
        "{created}"
    );
}

#[test]
fn convert_album_to_json_carries_duration() {
    let album = Album {
        songs: vec![
            song("", "", Some(200)),
            song("", "", None),
            song("", "", Some(100)),
        ],
        ..plain_album()
    };
    assert_eq!(builder().convert_album_to_json(&album)["duration"], 300);
}

#[test]
fn convert_album_to_xml_carries_duration_even_without_known_songs() {
    let element = builder().convert_album_to_xml(&plain_album(), NS);
    assert_eq!(element.attribute("duration"), Some("0"));
}

// ---- OpenSubsonic releaseTypes ------------------------------------------------------
// What lets a client group an artist's page into albums, EPs and singles.

#[test]
fn album_row_carries_its_release_types_in_both_formats() {
    let album = Album {
        id: "al1".into(),
        title: "Live Set".into(),
        artist: "A".into(),
        release_types: vec!["ep".into()],
        ..Default::default()
    };
    assert_eq!(
        builder().convert_album_to_json(&album)["releaseTypes"],
        json!(["ep"])
    );

    // A list of text is one element per value in XML, the way the upstream server writes it.
    let xml = builder().convert_album_to_xml(&album, NS);
    let values: Vec<String> = xml
        .elements_named(NS, "releaseTypes")
        .map(XElement::value)
        .collect();
    assert_eq!(values, ["ep"]);
    assert_eq!(xml.attribute("releaseTypes"), None);
}

#[test]
fn album_row_with_no_known_type_sends_an_empty_list() {
    // OpenSubsonic asks for the field even when empty, so a client knows it is supported.
    let album = Album {
        id: "al1".into(),
        title: "Mystery".into(),
        artist: "A".into(),
        ..Default::default()
    };
    assert_eq!(builder().convert_album_to_json(&album)["releaseTypes"], json!([]));
    assert_eq!(
        builder()
            .convert_album_to_xml(&album, NS)
            .elements_named(NS, "releaseTypes")
            .count(),
        0
    );
}

#[test]
fn create_album_response_carries_its_release_types_in_both_formats() {
    let album = Album {
        id: "al1".into(),
        title: "Hit".into(),
        artist: "A".into(),
        release_types: vec!["single".into()],
        ..Default::default()
    };
    let json = json_of(&builder().create_album_response("json", &album));
    assert_eq!(
        json["subsonic-response"]["album"]["releaseTypes"],
        json!(["single"])
    );

    let root = xml_of(&builder().create_album_response("xml", &album));
    let values: Vec<String> = child(&root, "album")
        .elements_named(NS, "releaseTypes")
        .map(XElement::value)
        .collect();
    assert_eq!(values, ["single"]);
}

// ---- The structured lyrics the app parses: lines and word timing -------------------------

fn structured(result: &LyricsResult, enhanced: bool) -> Value {
    let json = json_of(&builder().create_lyrics_list_response("json", Some(result), "A", "T", enhanced));
    json["subsonic-response"]["lyricsList"]["structuredLyrics"][0].clone()
}

/// The app's own example: "Café naïve 日本", whose cues are bytes 0..5, 6..12,
/// 13..15 and 16..18, both ends included.
fn cafe() -> LyricsResult {
    LyricsResult::new(
        "KuGou",
        Some("[00:01.00]<00:01.00>Café <00:01.50>naïve <00:02.50>日<00:03.00>本<00:03.50>".into()),
        None,
        false,
    )
}

fn pairs(cues: &[Value], a: &str, b: &str) -> Vec<(i64, i64)> {
    cues.iter()
        .map(|cue| (cue[a].as_i64().expect(a), cue[b].as_i64().expect(b)))
        .collect()
}

#[test]
fn cues_use_inclusive_utf8_byte_offsets_into_the_cue_line_value() {
    let lyrics = structured(&cafe(), true);
    assert_eq!(lyrics["kind"], "main");
    let cue_line = &lyrics["cueLine"][0];
    assert_eq!(cue_line["index"], 0);
    assert_eq!(cue_line["value"], "Café naïve 日本");
    assert_eq!(cue_line["start"], 1000);
    assert_eq!(cue_line["end"], 3500);
    let cues = cue_line["cue"].as_array().expect("cues");
    assert_eq!(
        pairs(cues, "byteStart", "byteEnd"),
        [(0, 5), (6, 12), (13, 15), (16, 18)]
    );
    let values: Vec<&str> = cues.iter().map(|c| c["value"].as_str().expect("value")).collect();
    assert_eq!(values, ["Café ", "naïve ", "日", "本"]);
    let starts: Vec<i64> = cues.iter().map(|c| c["start"].as_i64().expect("start")).collect();
    assert_eq!(starts, [1000, 1500, 2500, 3000]);
    let ends: Vec<i64> = cues.iter().map(|c| c["end"].as_i64().expect("end")).collect();
    assert_eq!(ends, [1500, 2500, 3000, 3500]);
}

/// The Octo app's ServerLyrics.kt Utf8Positions, copied line for line (it works in UTF-16
/// units), so these tests hold the server to what the app actually does with a cue.
fn app_char_range(text: &str, byte_start: i64, byte_end: i64) -> Option<(usize, usize)> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut starts = Vec::new();
    let mut index = 0;
    while index < units.len() {
        let pair = index + 1 < units.len()
            && (0xD800..0xDC00).contains(&units[index])
            && (0xDC00..0xE000).contains(&units[index + 1]);
        let point = if pair {
            0x10000 + ((u32::from(units[index]) - 0xD800) << 10) + (u32::from(units[index + 1]) - 0xDC00)
        } else {
            u32::from(units[index])
        };
        let bytes = if point < 0x80 {
            1
        } else if point < 0x800 {
            2
        } else if point < 0x10000 {
            3
        } else {
            4
        };
        for _ in 0..bytes {
            starts.push(index);
        }
        index += if pair { 2 } else { 1 };
    }
    if byte_start < 0 || byte_end < byte_start || byte_end as usize >= starts.len() {
        return None;
    }
    let from = starts[byte_start as usize];
    let last = starts[byte_end as usize];
    let last_is_pair = last + 1 < units.len() && (0xD800..0xDC00).contains(&units[last]);
    Some((from, last + if last_is_pair { 2 } else { 1 }))
}

#[test]
fn cues_read_by_the_apps_rules_give_the_words() {
    let cue_line = structured(&cafe(), true)["cueLine"][0].clone();
    let value = cue_line["value"].as_str().expect("value").to_string();
    let ranges: Vec<(usize, usize)> = cue_line["cue"]
        .as_array()
        .expect("cues")
        .iter()
        .map(|cue| {
            app_char_range(
                &value,
                cue["byteStart"].as_i64().expect("start"),
                cue["byteEnd"].as_i64().expect("end"),
            )
            .expect("a range")
        })
        .collect();
    assert_eq!(ranges, [(0, 5), (5, 11), (11, 12), (12, 13)]);
    let units: Vec<u16> = value.encode_utf16().collect();
    let words: Vec<String> = ranges
        .iter()
        .map(|(from, to)| String::from_utf16_lossy(&units[*from..*to]))
        .collect();
    assert_eq!(words, ["Café ", "naïve ", "日", "本"]);
}

#[test]
fn cues_an_emoji_takes_four_bytes_and_two_chars() {
    let lyrics = structured(
        &LyricsResult::new(
            "x",
            Some("[00:00.00]<00:00.00>🎵 <00:00.50>la<00:00.90>".into()),
            None,
            false,
        ),
        true,
    );
    let cue_line = &lyrics["cueLine"][0];
    let value = cue_line["value"].as_str().expect("value");
    let cues = cue_line["cue"].as_array().expect("cues");
    assert_eq!(pairs(cues, "byteStart", "byteEnd"), [(0, 4), (5, 6)]);
    assert_eq!(app_char_range(value, 0, 4), Some((0, 3)));
}

/// A strict client that did not ask for word timing gets byte for byte what it
/// got before this change: no kind, no cues, no word tags in the text.
#[test]
fn not_enhanced_is_exactly_the_old_shape() {
    let reply = builder().create_lyrics_list_response(
        "json",
        Some(&LyricsResult::new(
            "LRCLIB",
            Some("[00:01.50]first\n[00:03.00]second".into()),
            None,
            false,
        )),
        "A",
        "T",
        false,
    );
    assert_eq!(
        reply.text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","lyricsList":{"structuredLyrics":[{"lang":"xxx","synced":true,"displayArtist":"A","displayTitle":"T","offset":0,"line":[{"start":1500,"value":"first"},{"start":3000,"value":"second"}]}]}}}"#
    );

    let words = structured(&cafe(), false);
    assert!(words.get("cueLine").is_none());
    assert!(words.get("kind").is_none());
    assert_eq!(words["line"][0]["value"], "Café naïve 日本");
}

#[test]
fn not_enhanced_xml_has_no_cues_enhanced_xml_has_them() {
    let plain = builder().create_lyrics_list_response("xml", Some(&cafe()), "A", "T", false);
    let rich = builder().create_lyrics_list_response("xml", Some(&cafe()), "A", "T", true);
    let (plain, rich) = (plain.text(), rich.text());

    assert!(!plain.contains("cueLine"));
    assert!(plain.contains("<line start=\"1000\">Café naïve 日本</line>"));
    assert!(rich.contains("kind=\"main\""));
    assert!(rich.contains("byteStart=\"6\" byteEnd=\"12\""));
}

#[test]
fn legacy_get_lyrics_is_plain_text_without_times() {
    let json = json_of(&builder().create_lyrics_response("json", Some(&cafe()), "A", "T"));
    assert_eq!(json["subsonic-response"]["lyrics"]["value"], "Café naïve 日本");
}

#[test]
fn lyrics_list_xml_carries_timed_lines() {
    let reply = builder().create_lyrics_list_response(
        "xml",
        Some(&LyricsResult::new(
            "lrclib",
            Some("[00:02.00]hello".into()),
            None,
            false,
        )),
        "A",
        "T",
        false,
    );
    let text = reply.text();
    assert!(text.contains("<line start=\"2000\">hello</line>"));
    assert!(text.contains("synced=\"true\""));
}

// ---- Exact shapes: lyrics, errors, info, song rows, playlists and library actions -----------

#[test]
fn lyrics_answers_keep_their_exact_shape() {
    // Plain lyrics: untimed, trimmed lines, in XML with no start.
    let plain = LyricsResult::new("x", None, Some("  one \r\n\ntwo".into()), false);
    assert_eq!(
        builder()
            .create_lyrics_list_response("xml", Some(&plain), "A & B", "T", false)
            .text(),
        concat!(
            "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n",
            "  <lyricsList>\n",
            "    <structuredLyrics lang=\"xxx\" synced=\"false\" displayArtist=\"A &amp; B\" displayTitle=\"T\" offset=\"0\">\n",
            "      <line>one</line>\n",
            "      <line></line>\n",
            "      <line>two</line>\n",
            "    </structuredLyrics>\n",
            "  </lyricsList>\n",
            "</subsonic-response>"
        )
    );
    // Nothing: an ok, empty list in both formats.
    assert_eq!(
        builder()
            .create_lyrics_list_response("JSON", None, "A", "T", true)
            .text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","lyricsList":{"structuredLyrics":[]}}}"#
    );
    assert_eq!(
        builder()
            .create_lyrics_list_response("xml", None, "A", "T", true)
            .text(),
        "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n  <lyricsList />\n</subsonic-response>"
    );
    // getLyrics with nothing, and an instrumental, is an empty value without artist or title.
    let instrumental = LyricsResult::new("x", Some("[00:01.00]la".into()), None, true);
    assert_eq!(
        builder()
            .create_lyrics_response("xml", Some(&instrumental), "A", "T")
            .text(),
        "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n  <lyrics></lyrics>\n</subsonic-response>"
    );
    assert_eq!(
        builder().create_lyrics_response("json", None, "A", "T").text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","lyrics":{"value":""}}}"#
    );
    // Enhanced XML: kind after the other attributes, cue lines after the lines.
    let rich = builder().create_lyrics_list_response(
        "xml",
        Some(&LyricsResult::new(
            "x",
            Some("[00:01.00]<00:01.00>a <00:02.00>b".into()),
            None,
            false,
        )),
        "A",
        "T",
        true,
    );
    assert_eq!(
        rich.text(),
        concat!(
            "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n",
            "  <lyricsList>\n",
            "    <structuredLyrics lang=\"xxx\" synced=\"true\" displayArtist=\"A\" displayTitle=\"T\" offset=\"0\" kind=\"main\">\n",
            "      <line start=\"1000\">a b</line>\n",
            "      <cueLine index=\"0\" start=\"1000\" value=\"a b\">\n",
            "        <cue start=\"1000\" end=\"2000\" byteStart=\"0\" byteEnd=\"1\">a </cue>\n",
            "        <cue start=\"2000\" byteStart=\"2\" byteEnd=\"2\">b</cue>\n",
            "      </cueLine>\n",
            "    </structuredLyrics>\n",
            "  </lyricsList>\n",
            "</subsonic-response>"
        )
    );
}

#[test]
fn lyrics_choice_answers_are_always_json() {
    let candidate = LyricsChoiceCandidate {
        id: "lrclib:9001".into(),
        source: "lrclib".into(),
        title: "Polar Night".into(),
        artist: "Aurora Vale".into(),
        album: None,
        duration_seconds: Some(3),
        kind: "line".into(),
        same_song: true,
        preview: vec!["Long dark sky".into()],
    };
    let reply = builder().create_lyrics_candidates_response("s1", "lrclib:9001", &[candidate]);
    assert_eq!(reply.content_type, "application/json; charset=utf-8");
    assert_eq!(
        reply.text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"octo","lyricsCandidates":{"id":"s1","choice":"lrclib:9001","candidate":[{"id":"lrclib:9001","source":"lrclib","title":"Polar Night","artist":"Aurora Vale","album":null,"duration":3,"kind":"line","sameSong":true,"chosen":true,"preview":["Long dark sky"]}]}}}"#
    );
    assert_eq!(
        builder().create_lyrics_choice_response("s1", "none").text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"octo","lyricsChoice":{"id":"s1","choice":"none"}}}"#
    );
}

#[test]
fn the_empty_ok_leaves_the_element_out_of_json_but_not_xml() {
    assert_eq!(
        builder().create_response("json", "download").text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#
    );
    assert_eq!(
        builder().create_response("jsonp", "download").text(),
        "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n  <download />\n</subsonic-response>"
    );
    // The format is compared case-sensitively here: JSON is XML.
    assert_eq!(builder().create_error("JSON", 10, "x").kind, ReplyKind::Content);
}

#[test]
fn errors_escape_as_each_writer_did() {
    assert_eq!(
        builder().create_error("json", 70, "Octo's \"x\" <&>").text(),
        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"Octo\u0027s \u0022x\u0022 \u003C\u0026\u003E"}}}"#
    );
    assert_eq!(
        builder().create_error("xml", 70, "Octo's \"x\" <&>").text(),
        "<subsonic-response status=\"failed\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n  <error code=\"70\" message=\"Octo's &quot;x&quot; &lt;&amp;&gt;\" />\n</subsonic-response>"
    );
}

#[test]
fn info_responses_carry_their_fields_in_both_formats() {
    let fields = [("notes", ""), ("smallImageUrl", "u")];
    assert_eq!(
        builder()
            .create_info_response("json", "albumInfo", &fields)
            .text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","albumInfo":{"notes":"","smallImageUrl":"u"}}}"#
    );
    assert_eq!(
        builder().create_info_response("xml", "albumInfo", &fields).text(),
        "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n  <albumInfo>\n    <notes></notes>\n    <smallImageUrl>u</smallImageUrl>\n  </albumInfo>\n</subsonic-response>"
    );
}

#[test]
fn song_rows_keep_their_field_order_and_values() {
    let now = Utc.with_ymd_and_hms(2026, 10, 4, 8, 7, 45).unwrap() + chrono::Duration::microseconds(947_999);
    let builder = builder().with_clock(Clock::fixed(now));
    let local = Song {
        id: "l1".into(),
        title: "Ünder".into(),
        artist: "A".into(),
        album: "  ".into(),
        suffix: Some(" MP3 ".into()),
        bit_rate: Some(320),
        duration: Some(180),
        is_local: true,
        year: Some(1995),
        ..Default::default()
    };
    assert_eq!(
        octo_core::json::to_string(&builder.convert_song_to_json(&local)),
        concat!(
            r#"{"id":"l1","parent":"Album:A:\u00DCnder","isDir":false,"title":"\u00DCnder","album":"\u00DCnder","#,
            r#""artist":"A","track":1,"genre":"","coverArt":"l1","size":7200000,"contentType":"audio/mpeg","#,
            r#""suffix":"mp3","duration":180,"bitRate":320,"path":"A/\u00DCnder/\u00DCnder.mp3","#,
            r#""created":"2026-10-04T08:07:45.947Z","albumId":"Album:A:\u00DCnder","artistId":"Artist:A:","#,
            r#""type":"music","isVideo":false,"mediaType":"song","channelCount":2,"samplingRate":44100,"#,
            r#""bitDepth":16,"artists":[{"id":"Artist:A:","name":"A"}],"displayArtist":"A","#,
            r#""albumArtists":[{"id":"Artist:A:","name":"A"}],"displayAlbumArtist":"A","contributors":[],"#,
            r#""explicitStatus":"","isrc":[],"genres":[],"moods":[],"replayGain":{},"sortName":"\u00FCnder","#,
            r#""isExternal":false,"year":1995}"#
        )
    );
    // XML: scalars as attributes; lists and objects are not, and an empty list writes nothing.
    assert_eq!(
        builder.convert_song_to_xml(&local, NS).to_xml_string(),
        concat!(
            "<song id=\"l1\" parent=\"Album:A:Ünder\" isDir=\"false\" title=\"Ünder\" album=\"Ünder\" artist=\"A\" ",
            "track=\"1\" genre=\"\" coverArt=\"l1\" size=\"7200000\" contentType=\"audio/mpeg\" suffix=\"mp3\" ",
            "duration=\"180\" bitRate=\"320\" path=\"A/Ünder/Ünder.mp3\" created=\"2026-10-04T08:07:45.947Z\" ",
            "albumId=\"Album:A:Ünder\" artistId=\"Artist:A:\" type=\"music\" isVideo=\"false\" mediaType=\"song\" ",
            "channelCount=\"2\" samplingRate=\"44100\" bitDepth=\"16\" displayArtist=\"A\" displayAlbumArtist=\"A\" ",
            "explicitStatus=\"\" sortName=\"ünder\" isExternal=\"false\" year=\"1995\" xmlns=\"http://subsonic.org/restapi\" />"
        )
    );
    let mut types = std::collections::HashMap::new();
    for (suffix, want) in [
        ("flac", "audio/flac"),
        ("m4a", "audio/mp4"),
        ("alac", "audio/mp4"),
        ("opus", "audio/ogg"),
        ("wav", "audio/wav"),
        ("wma", "application/octet-stream"),
    ] {
        types.insert(suffix, SubsonicResponseBuilder::content_type_for(suffix));
        assert_eq!(types[suffix], want, "{suffix}");
    }
}

#[test]
fn json_shape_to_xml_follows_opensubsonic_rules() {
    let data = json!({
        "id": "a1",
        "count": 3,
        "rate": 1.5,
        "big": 1e20,
        "flag": true,
        "nothing": null,
        "isrc": ["X1", "X2"],
        "genres": [{"name": "Rock"}, null],
        "replayGain": {},
        "nested": [[1]],
        "empty": [],
    });
    assert_eq!(
        json_shape_to_xml(NS, "album", &data).to_xml_string(),
        concat!(
            "<album id=\"a1\" count=\"3\" rate=\"1.5\" big=\"1E+20\" flag=\"true\" xmlns=\"http://subsonic.org/restapi\">\n",
            "  <isrc>X1</isrc>\n",
            "  <isrc>X2</isrc>\n",
            "  <genres name=\"Rock\" />\n",
            "  <replayGain />\n",
            "  <nested>System.Collections.Generic.List`1[System.Object]</nested>\n",
            "</album>"
        )
    );
    assert_eq!(
        json_shape_to_xml(NS, "x", &Value::Null).to_xml_string(),
        "<x xmlns=\"http://subsonic.org/restapi\" />"
    );
    assert_eq!(
        json_shape_to_xml(NS, "x", &json!(5)).to_xml_string(),
        "<x xmlns=\"http://subsonic.org/restapi\">5</x>"
    );
}

#[test]
fn convert_json_value_keeps_ints_and_makes_the_rest_doubles() {
    let converted = convert_json_value(
        &serde_json::from_str::<Value>(
            r#"{"a":1,"b":2147483648,"c":1.0,"d":1e2,"e":100000000000000000,"f":-2147483648,"g":[1.5]}"#,
        )
        .unwrap(),
    );
    assert_eq!(
        octo_core::json::to_string(&converted),
        r#"{"a":1,"b":2147483648,"c":1,"d":100,"e":1E+17,"f":-2147483648,"g":[1.5]}"#
    );
    assert!(converted["b"].is_f64() && converted["a"].is_i64() && converted["f"].is_i64());
    let builder = builder();
    let row = builder
        .convert_subsonic_json_element(&json!({"isExternal": "x", "id": "1"}), true)
        .expect("an object");
    assert_eq!(
        octo_core::json::to_string(&row),
        r#"{"isExternal":false,"id":"1"}"#
    );
    assert!(builder.convert_subsonic_json_element(&json!([1]), true).is_none());
}

fn playlist(curator: Option<&str>, created: bool, cover: bool) -> ExternalPlaylist {
    ExternalPlaylist {
        id: "pl-deezer-1".into(),
        name: "Mix".into(),
        curator_name: curator.map(str::to_string),
        provider: "deezer".into(),
        external_id: "1".into(),
        track_count: 2,
        duration: 400,
        cover_url: cover.then(|| "http://c".to_string()),
        created_date: created.then(|| Utc.with_ymd_and_hms(2024, 5, 6, 7, 8, 9).unwrap()),
        ..Default::default()
    }
}

#[test]
fn a_playlist_as_an_album_carries_its_curator_in_both_formats() {
    let tracks = [song("t1", "One", Some(100))];
    assert_eq!(
        builder()
            .create_playlist_as_album_response("json", &playlist(Some("Top Hits"), false, true), &[])
            .text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","album":{"id":"pl-deezer-1","name":"Mix","artist":"\uD83C\uDFB5 Deezer Top Hits","artistId":"curator-deezer-top-hits","coverArt":"pl-deezer-1","songCount":0,"duration":0,"year":0,"genre":"Playlist","isCompilation":false,"created":null,"song":[]}}}"#
    );
    let xml = builder().create_playlist_as_album_response("xml", &playlist(None, true, false), &tracks);
    let root = xml_of(&xml);
    let album = child(&root, "album");
    let attrs: Vec<(&str, &str)> = album
        .attributes
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        attrs,
        [
            ("id", "pl-deezer-1"),
            ("name", "Mix"),
            ("artist", "\u{1F3B5} Deezer"),
            ("artistId", "curator-deezer-unknown"),
            ("songCount", "1"),
            ("duration", "100"),
            ("genre", "Playlist"),
            ("coverArt", "pl-deezer-1"),
            ("year", "2024"),
            ("created", "2024-05-06T07:08:09"),
        ]
    );
    assert_eq!(album.elements_named(NS, "song").count(), 1);
}

#[test]
fn a_radio_station_is_a_read_only_playlist() {
    let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    let station = LastFmRadioStation {
        id: "or1".into(),
        name: "Radio".into(),
        owner: "alice".into(),
        created_utc: at,
        changed_utc: at,
        valid_until_utc: at,
        tracks: vec![
            LastFmRadioTrack {
                duration: Some(100),
                ..Default::default()
            },
            LastFmRadioTrack::default(),
        ],
        ..Default::default()
    };
    assert_eq!(
        octo_core::json::to_string(&builder().radio_playlist_fields(&station)),
        r#"{"id":"or1","name":"Radio","owner":"alice","public":false,"songCount":2,"duration":100,"created":"2026-01-02T03:04:05.000Z","changed":"2026-01-02T03:04:05.000Z","coverArt":"or1","readonly":true,"validUntil":"2026-01-02T03:04:05.000Z","octoGroup":"station"}"#
    );
    let songs = [song("s1", "One", None)];
    let json = json_of(&builder().create_radio_playlist_response("Json", &station, &songs));
    assert_eq!(json["subsonic-response"]["playlist"]["songCount"], 1);
    assert_eq!(json["subsonic-response"]["playlist"]["duration"], 0);
    assert_eq!(json["subsonic-response"]["playlist"]["entry"][0]["id"], "s1");
    let root = xml_of(&builder().create_radio_playlist_response("xml", &station, &songs));
    let playlist = child(&root, "playlist");
    assert_eq!(playlist.attribute("readonly"), Some("true"));
    assert_eq!(playlist.attribute("octoGroup"), Some("station"));
    assert_eq!(json["subsonic-response"]["playlist"]["octoGroup"], "station");
    assert_eq!(playlist.elements_named(NS, "song").count(), 1);
}

#[test]
fn an_entries_playlist_passes_navidromes_entries_through() {
    let entries = [
        Node::parse(r#"{"id":"n1","duration":200,"rate":1.50,"starred":true,"genres":[{"name":"x"}],"note":null,"title":"Caf\u00e9"}"#)
            .unwrap(),
        Node::parse(r#"{"id":"n2","duration":"7"}"#).unwrap(),
    ];
    let mut fields = Fields::new();
    fields.insert("id".into(), "og1".into());
    fields.insert("songCount".into(), 9.into());
    fields.insert("duration".into(), 9.into());
    assert_eq!(
        builder()
            .create_entries_playlist_response("json", fields.clone(), &entries)
            .text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","playlist":{"id":"og1","songCount":2,"duration":200,"entry":[{"id":"n1","duration":200,"rate":1.50,"starred":true,"genres":[{"name":"x"}],"note":null,"title":"Caf\u00E9"},{"id":"n2","duration":"7"}]}}}"#
    );
    assert_eq!(
        builder()
            .create_entries_playlist_response("xml", fields, &entries)
            .text(),
        concat!(
            "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n",
            "  <playlist id=\"og1\" songCount=\"2\" duration=\"200\">\n",
            "    <entry id=\"n1\" duration=\"200\" rate=\"1.50\" starred=\"true\" title=\"Café\" />\n",
            "    <entry id=\"n2\" duration=\"7\" />\n",
            "  </playlist>\n",
            "</subsonic-response>"
        )
    );
}

#[test]
fn an_entries_playlist_keeps_each_entrys_replay_gain_in_xml() {
    let entries = [
        Node::parse(r#"{"id":"n1","duration":200,"replayGain":{"trackGain":-6.5,"trackPeak":0.98}}"#)
            .unwrap(),
        Node::parse(r#"{"id":"n2","duration":100,"replayGain":{}}"#).unwrap(),
    ];

    let root = xml_of(&builder().create_entries_playlist_response("xml", Fields::new(), &entries));

    let playlist = child(&root, "playlist");
    let rows: Vec<&XElement> = playlist.elements_named(NS, "entry").collect();
    let gain = rows[0]
        .elements()
        .find(|e| e.is(NS, "replayGain"))
        .expect("a replayGain element");
    assert_eq!(gain.attribute("trackGain"), Some("-6.5"));
    assert_eq!(gain.attribute("trackPeak"), Some("0.98"));
    // Nothing measured, nothing written.
    assert!(rows[1].elements().next().is_none());
}

#[test]
fn library_actions_offer_what_is_enabled() {
    let off = LibraryActionSettings::default();
    assert_eq!(
        builder()
            .create_library_actions_response(&off, Some("admin"), 1, true, "Soulseek", false, false, false)
            .text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"octo","openSubsonic":true,"libraryActions":{"enabled":false,"allowed":false,"dryRun":true,"actions":[],"admin":false,"keepDays":30,"parallel":1,"upgradeSource":"Soulseek"}}}"#
    );
    let mut on = LibraryActionSettings {
        enabled: true,
        allowed_users: vec!["Admin".into()],
        ..Default::default()
    };
    on.actions = on
        .effective_actions()
        .into_iter()
        .map(|mut definition| {
            definition.enabled = true;
            definition
        })
        .collect();
    let json = json_of(&builder().create_library_actions_response(
        &on,
        Some(" admin "),
        3,
        true,
        "Lidarr",
        true,
        false,
        false,
    ));
    assert_eq!(json["subsonic-response"]["libraryActions"]["allowed"], true);
    assert_eq!(json["subsonic-response"]["libraryActions"]["admin"], true);
    assert_eq!(
        json["subsonic-response"]["libraryActions"]["actions"],
        json!(["remove", "upgrade", "lyrics", "cover-swap", "checkVersion"])
    );
    let json = json_of(
        &builder().create_library_actions_response(&on, None, 3, false, "Lidarr", true, false, false),
    );
    assert_eq!(
        json["subsonic-response"]["libraryActions"]["actions"],
        json!(["remove", "lyrics", "cover-swap", "checkVersion"])
    );
    // Version 3: remove only for a Navidrome admin; the edits whenever the server can make them,
    // cover last and only with a cover finder.
    let json =
        json_of(&builder().create_library_actions_response(&on, None, 3, true, "Lidarr", false, true, false));
    assert_eq!(
        json["subsonic-response"]["libraryActions"]["actions"],
        json!([
            "upgrade",
            "lyrics",
            "cover-swap",
            "checkVersion",
            "retag",
            "joinAlbum",
            "lookup",
            "undo",
            "restore"
        ])
    );
    let json = json_of(
        &builder().create_library_actions_response(&off, None, 1, false, "Soulseek", true, true, true),
    );
    assert_eq!(
        json["subsonic-response"]["libraryActions"]["actions"],
        json!(["retag", "joinAlbum", "lookup", "undo", "restore", "cover"])
    );
    assert_eq!(
        json["subsonic-response"]["libraryActions"]["upgradeSource"],
        Value::Null
    );
    assert_eq!(
        builder()
            .create_library_action_response("s1", "queued", None, UPGRADE_ACTION)
            .text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"octo","openSubsonic":true,"libraryAction":{"id":"s1","action":"upgrade","state":"queued","detail":null}}}"#
    );
}

#[test]
fn wrong_lyrics_and_wrong_cover_are_offered_only_when_switched_on() {
    use octo_core::settings::{LibraryAction, LibraryActionDefinition};
    let only = |action: LibraryAction| LibraryActionSettings {
        enabled: true,
        allowed_users: vec!["alice".into()],
        actions: vec![LibraryActionDefinition {
            action,
            enabled: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let offered = |settings: &LibraryActionSettings| {
        json_of(&builder().create_library_actions_response(
            settings,
            Some("alice"),
            1,
            false,
            "Soulseek",
            false,
            false,
            false,
        ))["subsonic-response"]["libraryActions"]["actions"]
            .clone()
    };
    assert_eq!(offered(&only(LibraryAction::WrongLyrics)), json!(["lyrics"]));
    assert_eq!(offered(&only(LibraryAction::WrongCover)), json!(["cover-swap"]));
    assert_eq!(offered(&only(LibraryAction::Keep)), json!([]));
}

// ---- getOpenSubsonicExtensions --------------------------------------------------------------

const NAVIDROME_JSON: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","openSubsonic":true,"openSubsonicExtensions":[{"name":"songLyrics","versions":[1]},{"name":"octoLyrics","versions":[0,1]}]}}"#;

#[test]
fn extensions_json_adds_octos_own_and_missing_versions() {
    let reply = builder().merge_open_subsonic_extensions(
        "json",
        Some(NAVIDROME_JSON.as_bytes()),
        Some("application/json"),
        true,
        true,
    );
    assert_eq!(reply.kind, ReplyKind::Content);
    assert_eq!(reply.content_type, "application/json");
    assert_eq!(
        reply.text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","openSubsonic":true,"openSubsonicExtensions":[{"name":"songLyrics","versions":[1,2]},{"name":"octoLyrics","versions":[1]},{"name":"octoAcquisitions","versions":[1,2,3,4]},{"name":"octoLibraryActions","versions":[1,2,3]},{"name":"octoImports","versions":[1,2]},{"name":"octoLists","versions":[1,2]}]}}"#
    );
    // Lyrics choices off and library actions off leave those two out.
    let reply = builder().merge_open_subsonic_extensions(
        "JSON",
        Some(br#"{"subsonic-response":{"status":"ok"}}"#),
        None,
        false,
        false,
    );
    assert_eq!(
        reply.text(),
        r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"octoAcquisitions","versions":[1,2,3,4]},{"name":"songLyrics","versions":[1,2]},{"name":"octoImports","versions":[1,2]},{"name":"octoLists","versions":[1,2]}]}}"#
    );
    // A failed answer is written back as it was read.
    let failed = r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"isn't"}}}"#;
    assert_eq!(
        builder()
            .merge_open_subsonic_extensions("json", Some(failed.as_bytes()), None, true, false)
            .text(),
        r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"isn\u0027t"}}}"#
    );
}

#[test]
fn extensions_xml_adds_octos_own_and_reindents() {
    let upstream = r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><openSubsonicExtensions name="songLyrics"><versions>1</versions></openSubsonicExtensions></subsonic-response>"#;
    let reply = builder().merge_open_subsonic_extensions(
        "xml",
        Some(upstream.as_bytes()),
        Some("application/xml"),
        false,
        false,
    );
    assert_eq!(reply.kind, ReplyKind::Content);
    assert_eq!(
        reply.text(),
        concat!(
            "<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\" version=\"1.16.1\">\n",
            "  <openSubsonicExtensions name=\"songLyrics\">\n",
            "    <versions>1</versions>\n",
            "    <versions>2</versions>\n",
            "  </openSubsonicExtensions>\n",
            "  <openSubsonicExtensions name=\"octoAcquisitions\">\n",
            "    <versions>1</versions>\n",
            "    <versions>2</versions>\n",
            "    <versions>3</versions>\n",
            "    <versions>4</versions>\n",
            "  </openSubsonicExtensions>\n",
            "  <openSubsonicExtensions name=\"octoImports\">\n",
            "    <versions>1</versions>\n",
            "    <versions>2</versions>\n",
            "  </openSubsonicExtensions>\n",
            "  <openSubsonicExtensions name=\"octoLists\">\n",
            "    <versions>1</versions>\n",
            "    <versions>2</versions>\n",
            "  </openSubsonicExtensions>\n",
            "</subsonic-response>"
        )
    );
}

#[test]
fn the_places_extension_is_listed_last_only_with_place_data() {
    let listed = |places: bool| {
        builder()
            .merge_open_subsonic_extensions_with(
                "json",
                Some(NAVIDROME_JSON.as_bytes()),
                Some("application/json"),
                true,
                true,
                false,
                false,
                false,
                false,
                places,
                false,
            )
            .text()
            .to_string()
    };
    assert_eq!(
        listed(true),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","openSubsonic":true,"openSubsonicExtensions":[{"name":"songLyrics","versions":[1,2]},{"name":"octoLyrics","versions":[1]},{"name":"octoAcquisitions","versions":[1,2,3,4]},{"name":"octoLibraryActions","versions":[1,2,3]},{"name":"octoImports","versions":[1,2]},{"name":"octoLists","versions":[1,2]},{"name":"octoPlaces","versions":[1]}]}}"#
    );
    assert!(!listed(false).contains("octoPlaces"));
    let own = builder()
        .merge_open_subsonic_extensions_with(
            "xml", None, None, false, false, false, false, false, false, true, false,
        )
        .text()
        .to_string();
    assert!(
        own.ends_with(concat!(
            "  <openSubsonicExtensions name=\"octoPlaces\">\n",
            "    <versions>1</versions>\n",
            "  </openSubsonicExtensions>\n",
            "</subsonic-response>"
        )),
        "{own}"
    );
}

#[test]
fn extensions_it_cannot_read_pass_through_untouched() {
    for (format, body, content_type, want_type) in [
        ("json", "<xml/>", Some("text/xml"), "text/xml"),
        ("json", "[1]", None, "application/json"),
        (
            "json",
            r#"{"subsonic-response":{"status":1}}"#,
            None,
            "application/json",
        ),
        (
            "json",
            r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"songLyrics","versions":["1"]}]}}"#,
            None,
            "application/json",
        ),
        ("xml", "{\"json\":1}", None, "application/xml"),
    ] {
        let reply = builder().merge_open_subsonic_extensions(
            format,
            Some(body.as_bytes()),
            content_type,
            true,
            false,
        );
        assert_eq!(reply.kind, ReplyKind::File, "{body}");
        assert_eq!(reply.content_type, want_type, "{body}");
        assert_eq!(reply.text(), body);
    }
}

#[test]
fn extensions_without_an_answer_list_octos_own() {
    assert_eq!(
        builder()
            .merge_open_subsonic_extensions("json", None, None, true, true)
            .text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"octo","openSubsonicExtensions":[{"name":"octoAcquisitions","versions":[1,2,3,4]},{"name":"octoLyrics","versions":[1]},{"name":"octoLibraryActions","versions":[1,2,3]},{"name":"songLyrics","versions":[1,2]},{"name":"octoImports","versions":[1,2]},{"name":"octoLists","versions":[1,2]}]}}"#
    );
    assert_eq!(
        builder()
            .merge_open_subsonic_extensions("xml", Some(b""), None, false, false)
            .text(),
        concat!(
            "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n",
            "  <openSubsonicExtensions name=\"octoAcquisitions\">\n",
            "    <versions>1</versions>\n",
            "    <versions>2</versions>\n",
            "    <versions>3</versions>\n",
            "    <versions>4</versions>\n",
            "  </openSubsonicExtensions>\n",
            "  <openSubsonicExtensions name=\"songLyrics\">\n",
            "    <versions>1</versions>\n",
            "    <versions>2</versions>\n",
            "  </openSubsonicExtensions>\n",
            "  <openSubsonicExtensions name=\"octoImports\">\n",
            "    <versions>1</versions>\n",
            "    <versions>2</versions>\n",
            "  </openSubsonicExtensions>\n",
            "  <openSubsonicExtensions name=\"octoLists\">\n",
            "    <versions>1</versions>\n",
            "    <versions>2</versions>\n",
            "  </openSubsonicExtensions>\n",
            "</subsonic-response>"
        )
    );
}

#[test]
fn merged_responses_write_json_as_it_is_and_xml_by_opensubsonic_rules() {
    let data = json!({"id": "a", "song": [{"id": "s", "isrc": []}]});
    assert_eq!(
        builder().create_merged_response("json", "album", &data).text(),
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","album":{"id":"a","song":[{"id":"s","isrc":[]}]}}}"#
    );
    assert_eq!(
        builder().create_merged_response("xml", "album", &data).text(),
        "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n  <album id=\"a\">\n    <song id=\"s\" />\n  </album>\n</subsonic-response>"
    );
}

// ---- Explicit and clean outside songs: what a client reads ----------------------------------

#[test]
fn responses_say_explicit_or_clean_for_outside_songs_and_albums_in_json_and_xml() {
    let builder = builder();
    let song = Song {
        id: "s1".into(),
        artist: "Tame Impala".into(),
        title: "Dracula".into(),
        explicit_content_lyrics: Some(1),
        ..Default::default()
    };
    let plain = Song {
        id: "s2".into(),
        artist: "A".into(),
        title: "T".into(),
        ..Default::default()
    };
    let album = Album {
        id: "a1".into(),
        artist: "Kendrick Lamar".into(),
        title: "GNX".into(),
        explicit_content_lyrics: Some(3),
        ..Default::default()
    };

    assert_eq!(builder.convert_song_to_json(&song)["explicitStatus"], "explicit");
    assert_eq!(builder.convert_song_to_json(&plain)["explicitStatus"], "");
    assert_eq!(builder.convert_album_to_json(&album)["explicitStatus"], "clean");
    assert_eq!(
        builder.convert_song_to_xml(&song, NS).attribute("explicitStatus"),
        Some("explicit")
    );
}

// ---- getStationSongs ------------------------------------------------------------------------

#[test]
fn the_stations_extension_is_listed_last_and_only_when_asked() {
    let names = |stations: bool| -> Vec<String> {
        let reply = builder().merge_open_subsonic_extensions_with(
            "json", None, None, true, true, true, true, true, stations, false, false,
        );
        let answer: Value = serde_json::from_str(&reply.text()).unwrap();
        answer["subsonic-response"]["openSubsonicExtensions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|extension| extension["name"].as_str().unwrap().to_string())
            .collect()
    };
    let listed = names(true);
    assert_eq!(listed.last().map(String::as_str), Some(STATIONS_EXTENSION));
    assert_eq!(STATIONS_EXTENSION, "octoStations");
    assert!(!names(false).iter().any(|name| name == STATIONS_EXTENSION));
    // The short form never lists it.
    let short = builder()
        .merge_open_subsonic_extensions("json", None, None, true, true)
        .text()
        .into_owned();
    assert!(!short.contains("octoStations"), "{short}");
}

#[test]
fn station_songs_carry_the_station_and_its_entries_in_both_formats() {
    let station = LastFmRadioStation {
        id: "or12345678901234567890".into(),
        name: "Your Radio".into(),
        tracks: vec![LastFmRadioTrack::default()],
        ..Default::default()
    };
    let songs = [Song {
        id: "local-1".into(),
        title: "Title".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        duration: Some(200),
        is_local: true,
        ..Default::default()
    }];
    let json: Value = serde_json::from_str(
        &builder()
            .create_station_songs_response("json", &station, &songs)
            .text(),
    )
    .unwrap();
    let answer = &json["subsonic-response"];
    assert_eq!(answer["status"], "ok");
    assert_eq!(answer["stationSongs"]["id"], "or12345678901234567890");
    assert_eq!(answer["stationSongs"]["name"], "Your Radio");
    // Shaped as any song row; `created` is the clock's now, so it is left out of the comparison.
    let mut entry = answer["stationSongs"]["entry"][0].clone();
    let mut row = Value::Object(builder().convert_song_to_json(&songs[0]));
    for value in [&mut entry, &mut row] {
        value.as_object_mut().unwrap().remove("created");
    }
    assert_eq!(entry, row);
    let empty: Value = serde_json::from_str(
        &builder()
            .create_station_songs_response("json", &station, &[])
            .text(),
    )
    .unwrap();
    assert_eq!(empty["subsonic-response"]["stationSongs"]["entry"], json!([]));

    let xml = builder()
        .create_station_songs_response("xml", &station, &songs)
        .text()
        .into_owned();
    let root = XElement::parse(&xml).unwrap();
    let element = root
        .elements()
        .find(|e| e.name == "stationSongs")
        .expect("stationSongs");
    assert_eq!(element.attribute("id"), Some("or12345678901234567890"));
    assert_eq!(element.attribute("name"), Some("Your Radio"));
    let entries: Vec<&XElement> = element.elements().collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "entry");
    assert_eq!(entries[0].attribute("id"), Some("local-1"));
    assert_eq!(entries[0].attribute("title"), Some("Title"));
}
