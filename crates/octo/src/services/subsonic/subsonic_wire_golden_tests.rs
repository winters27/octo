//! Golden tests: the answers Octo builds itself, as the reference recording holds them, rebuilt
//! here from the same input and compared byte for byte, with their content type and length.
//!
//! The input is what the recorded server had at that point: the models the controller handed the
//! builder (the catalog stub's artist, albums and songs), Navidrome's JSON or XML for the merges,
//! and the clock for `created`. Navidrome's part of a merged answer is taken from the recording
//! itself, the library rows less what Octo added to them (`isExternal`), so a golden checks
//! everything Octo does to a page: parsing, the row shapes, the merge, the order, escaping and
//! layout. The external ids are minted by the real registry, so they also pin its hashing.

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::header;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use octo_core::common::Clock;
use octo_core::lyrics::lyrics_choices::LyricsChoiceCandidate;
use octo_core::lyrics::lyrics_models::LyricsResult;
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::search::SearchResult;
use octo_core::settings::{LibraryActionSettings, SubsonicSettings};
use octo_subsonic::subsonic_model_mapper::Row;
use octo_subsonic::subsonic_response_builder::{SUBSONIC_NAMESPACE, SubsonicReply, SubsonicResponseBuilder};
use octo_subsonic::xml::XElement;
use octo_subsonic::{ReplyKind, SubsonicModelMapper};
use serde_json::{Map, Value, json};

use super::subsonic_response_builder::{SubsonicResponseBuilderExt, new_subsonic_response_builder};
use crate::services::library::{LibraryActionOutcome, LibraryActionState};
use crate::services::soulseek::ExternalIdRegistry;

/// One recorded answer: its content type, its `Content-Length` when it had one, its body.
struct Recorded {
    content_type: String,
    content_length: Option<String>,
    body: String,
}

fn recorded(file: &str, name: &str) -> Recorded {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../docs/rust-migration/fixtures/wire/csharp/{file}.json"
    ));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let recording: Value = serde_json::from_str(&text).expect("a recording");
    let entry = recording["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no {name} in {file}"));
    let header = |wanted: &str| {
        entry["headers"]
            .as_array()
            .expect("headers")
            .iter()
            .find_map(|pair| {
                (pair[0].as_str()?.eq_ignore_ascii_case(wanted))
                    .then(|| pair[1].as_str().unwrap_or("").to_string())
            })
    };
    Recorded {
        content_type: header("Content-Type").expect("a content type"),
        content_length: header("Content-Length"),
        body: entry["body"]["text"].as_str().expect("a text body").to_string(),
    }
}

/// The reply is the recorded answer: same body bytes, same content type, and the same length.
fn assert_golden(reply: SubsonicReply, file: &str, name: &str) {
    assert_recorded(reply, recorded(file, name), file, name);
}

/// A recorded extensions list with the extensions added since the 2026.10.03.2 recording:
/// octoImports (versions 1 and 2) and octoLists (versions 1 and 2) after the extensions it
/// lists, and octoAcquisitions at versions 1 to 4.
/// These golden tests keep the older recording and add those entries to it.
fn recorded_with_ported_extensions(file: &str, name: &str) -> Recorded {
    let mut want = recorded(file, name);
    let (old, new) = if want.content_type.contains("json") {
        let at = want.body.rfind("]}}").expect("the end of the list");
        want.body.insert_str(
            at,
            r#",{"name":"octoImports","versions":[1,2]},{"name":"octoLists","versions":[1,2]}"#,
        );
        (
            r#"{"name":"octoAcquisitions","versions":[1]}"#,
            r#"{"name":"octoAcquisitions","versions":[1,2,3,4]}"#,
        )
    } else {
        let at = want
            .body
            .rfind("</subsonic-response>")
            .expect("the end of the answer");
        want.body.insert_str(
            at,
            concat!(
                "  <openSubsonicExtensions name=\"octoImports\">\n    <versions>1</versions>\n    <versions>2</versions>\n  </openSubsonicExtensions>\n",
                "  <openSubsonicExtensions name=\"octoLists\">\n    <versions>1</versions>\n    <versions>2</versions>\n  </openSubsonicExtensions>\n",
            ),
        );
        (
            "<openSubsonicExtensions name=\"octoAcquisitions\">\n    <versions>1</versions>\n",
            "<openSubsonicExtensions name=\"octoAcquisitions\">\n    <versions>1</versions>\n    <versions>2</versions>\n    <versions>3</versions>\n    <versions>4</versions>\n",
        )
    };
    assert!(
        want.body.contains(old),
        "{file}/{name}: octoAcquisitions at version 1"
    );
    want.body = want.body.replace(old, new);
    want.content_length = None;
    want
}

/// A recorded answer with the outside albums' `explicitStatus`, which Octo 2026.10.05 sends right
/// after `releaseTypes` on every outside album, "" when the catalog did not say. The 2026.10.03.2
/// recording predates it; these inputs carry no explicit word, so the field is empty everywhere.
fn recorded_with_explicit_status(file: &str, name: &str) -> Recorded {
    let mut want = recorded(file, name);
    if want.content_type.contains("json") {
        let mut out = String::new();
        let mut rest = want.body.as_str();
        while let Some(at) = rest.find(r#""releaseTypes":["#) {
            let end = at + rest[at..].find(']').expect("the end of the list") + 1;
            out.push_str(&rest[..end]);
            out.push_str(r#","explicitStatus":"""#);
            rest = &rest[end..];
        }
        out.push_str(rest);
        want.body = out;
    } else {
        // An album row has it before its sortName, the album detail after its created.
        let lines: Vec<String> = want
            .body
            .split('\n')
            .map(|line| {
                if !line.trim_start().starts_with("<album ") {
                    line.to_string()
                } else if line.contains(" sortName=") {
                    line.replacen(" sortName=", " explicitStatus=\"\" sortName=", 1)
                } else {
                    let at = line.find(" created=\"").expect("a created");
                    let end = at + 10 + line[at + 10..].find('"').expect("its end") + 1;
                    format!("{} explicitStatus=\"\"{}", &line[..end], &line[end..])
                }
            })
            .collect();
        want.body = lines.join("\n");
    }
    want.content_length = None;
    want
}

fn assert_golden_with_explicit_status(reply: SubsonicReply, file: &str, name: &str) {
    assert_recorded(reply, recorded_with_explicit_status(file, name), file, name);
}

fn assert_recorded(reply: SubsonicReply, want: Recorded, file: &str, name: &str) {
    assert_eq!(reply.text(), want.body, "{file}/{name}: body");
    assert_eq!(
        reply.content_type, want.content_type,
        "{file}/{name}: content type"
    );
    let response = reply.into_response();
    assert_eq!(response.status(), 200, "{file}/{name}: status");
    assert_eq!(
        response.headers()[header::CONTENT_TYPE].to_str().expect("ASCII"),
        want.content_type,
        "{file}/{name}: Content-Type header"
    );
    let length = response.headers()[header::CONTENT_LENGTH]
        .to_str()
        .expect("ASCII")
        .to_string();
    assert_eq!(
        length,
        want.body.len().to_string(),
        "{file}/{name}: Content-Length"
    );
    if let Some(recorded_length) = want.content_length {
        assert_eq!(length, recorded_length, "{file}/{name}: recorded Content-Length");
    }
}

fn builder() -> SubsonicResponseBuilder {
    new_subsonic_response_builder(
        Arc::new(ExternalIdRegistry::in_memory()),
        &SubsonicSettings::default(),
    )
}

/// A builder whose "now" is the instant the recording wrote as `created`.
fn builder_at(created: &str) -> SubsonicResponseBuilder {
    let at: DateTime<Utc> = octo_core::json::datetime::parse_utc(created).expect("a time");
    builder().with_clock(Clock::fixed(at))
}

/// The JSON object at `path` in a recorded body.
fn recorded_json(file: &str, name: &str, path: &[&str]) -> Value {
    let mut value: Value = serde_json::from_str(&recorded(file, name).body).expect("JSON");
    for key in path {
        value = value[*key].take();
    }
    value
}

/// Navidrome's row as it came, before Octo marked it: the recorded row less its `isExternal`.
fn navidrome_row(mut row: Value) -> Value {
    if let Value::Object(fields) = &mut row {
        fields.shift_remove("isExternal");
    }
    row
}

// ---- Errors and the empty ok -------------------------------------------------------------

#[test]
fn errors_match_the_recordings() {
    let cases: &[(&str, &str, &str, i32, &str)] = &[
        ("01-system", "jukebox-xml", "xml", 0, "Jukebox is not supported"),
        ("01-system", "jukebox-json", "json", 0, "Jukebox is not supported"),
        (
            "01-system",
            "jukebox-noauth-json",
            "json",
            0,
            "Jukebox is not supported",
        ),
        (
            "02-browsing",
            "song-missing-id-json",
            "json",
            10,
            "Missing id parameter",
        ),
        (
            "02-browsing",
            "song-missing-id-xml",
            "xml",
            10,
            "Missing id parameter",
        ),
        (
            "02-browsing",
            "artist-missing-id-xml",
            "xml",
            10,
            "Missing id parameter",
        ),
        (
            "02-browsing",
            "album-missing-id-json",
            "json",
            10,
            "Missing id parameter",
        ),
        (
            "02-browsing",
            "song-legacy-ext-json",
            "json",
            70,
            "Song not found",
        ),
        (
            "02-browsing",
            "album-legacy-ext-json",
            "json",
            70,
            "Album not found",
        ),
        (
            "02-browsing",
            "album-external-playlist-json",
            "json",
            70,
            "Playlist not found",
        ),
        (
            "02-browsing",
            "similar-songs2-missing-id-json",
            "json",
            10,
            "Missing id parameter",
        ),
        (
            "04-media",
            "stream-external-badpass",
            "xml",
            40,
            "Wrong username or password",
        ),
        (
            "04-media",
            "stream-external-badpass-json",
            "json",
            40,
            "Wrong username or password",
        ),
        (
            "05-playlists",
            "create-playlist-octo-id-readonly-json",
            "json",
            70,
            "Octo's generated playlists are read-only",
        ),
        (
            "05-playlists",
            "update-playlist-octo-id-readonly-xml",
            "xml",
            70,
            "Octo's generated playlists are read-only",
        ),
        (
            "06-lyrics",
            "candidates-missing-id",
            "json",
            10,
            "Required parameter is missing: id",
        ),
        (
            "06-lyrics",
            "candidates-unknown-song",
            "json",
            70,
            "Song not found",
        ),
        (
            "06-lyrics",
            "candidates-badpass",
            "json",
            40,
            "Wrong username or password",
        ),
        (
            "06-lyrics",
            "set-choice-missing",
            "json",
            10,
            "Required parameter is missing: id and candidate",
        ),
        (
            "06-lyrics",
            "set-choice-bogus-candidate",
            "json",
            70,
            "Those lyrics could not be found; ask for the candidates again",
        ),
        (
            "07-octo-extensions",
            "acquisitions-noauth",
            "json",
            40,
            "Wrong username or password",
        ),
        (
            "07-octo-extensions",
            "library-action-missing",
            "json",
            10,
            "Required parameter is missing: id and action",
        ),
        (
            "07-octo-extensions",
            "library-action-unknown",
            "json",
            0,
            "Unknown action \"explode\"; this server knows remove and upgrade",
        ),
        (
            "90-mutations",
            "star-external-badpass",
            "json",
            40,
            "Wrong username or password",
        ),
    ];
    for (file, name, format, code, message) in cases {
        assert_golden(builder().create_error(format, *code, message), file, name);
    }
}

#[test]
fn synthetic_oks_match_the_recordings() {
    let cases: &[(&str, &str, &str, &str)] = &[
        ("04-media", "download-external-xml", "xml", "download"),
        ("04-media", "download-external-json", "json", "download"),
        (
            "08-catchall-native",
            "unstar-external-safety-net-xml",
            "xml",
            "unstar",
        ),
        (
            "08-catchall-native",
            "unstar-external-safety-net-json",
            "json",
            "unstar",
        ),
        (
            "08-catchall-native",
            "get-foo-external-element-name",
            "xml",
            "fooBar",
        ),
        ("90-mutations", "set-rating-external-json", "json", "setRating"),
        (
            "90-mutations",
            "report-playback-external-xml",
            "xml",
            "reportPlayback",
        ),
        ("90-mutations", "star-external-song-heart", "json", "star"),
    ];
    for (file, name, format, element) in cases {
        assert_golden(builder().create_response(format, element), file, name);
    }
    // getTranscodeDecision for an outside song: always JSON, built by the controller.
    let decision = builder().create_json_response(json!({
        "status": "ok",
        "version": "1.16.1",
        "transcodeDecision": { "canDirectPlay": true, "canTranscode": false },
    }));
    assert_golden(decision, "04-media", "transcode-decision-external");
}

// ---- The catalog's artist, album and song ------------------------------------------------

const ZEPHYR: &str = "NdCCz1eNDVR3vKwUWVaAO1";
const GLASS_HARBOR: &str = "wIfqpf1IeFS337TdaMfNC3";

fn glass_harbor_song(id: &str, title: &str, track: Option<i32>, duration: i32, isrc: Option<&str>) -> Song {
    Song {
        id: id.into(),
        title: title.into(),
        artist: "Zephyr Echo".into(),
        album: "Glass Harbor".into(),
        track,
        duration: Some(duration),
        isrc: isrc.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn an_outside_song_matches_the_recordings() {
    // getSong for a song first seen as a search row: its album id and artist id are the ones
    // the registry mints from its names, so they are left for the builder to mint.
    let song = glass_harbor_song("CwoXcVhGn95tWIOvqaFKt6", "Glass Harbor", Some(1), 3, None);
    assert_golden(
        builder().create_song_response("json", &song),
        "02-browsing",
        "song-external-json",
    );
    assert_golden(
        builder().create_song_response("xml", &song),
        "02-browsing",
        "song-external-xml",
    );
}

fn zephyr_albums() -> Vec<Album> {
    vec![
        Album {
            id: GLASS_HARBOR.into(),
            title: "Glass Harbor".into(),
            artist: "Zephyr Echo".into(),
            song_count: Some(3),
            year: Some(2019),
            release_types: vec!["album".into()],
            ..Default::default()
        },
        Album {
            id: "qbGtnk6ZRHdnMJ5BEflIt3".into(),
            title: "Neon Tide".into(),
            artist: "Zephyr Echo".into(),
            song_count: Some(1),
            year: Some(2021),
            release_types: vec!["single".into()],
            ..Default::default()
        },
    ]
}

#[test]
fn an_outside_artist_matches_the_recordings() {
    let artist = Artist {
        id: ZEPHYR.into(),
        name: "Zephyr Echo".into(),
        image_url: Some(
            "https://e-cdns-images.dzcdn.net/images/artist/zephyr/1000x1000-000000-80-0-0.jpg".into(),
        ),
        album_count: Some(2),
        ..Default::default()
    };
    assert_golden_with_explicit_status(
        builder_at("2026-10-04T08:07:45.947Z").create_artist_response("json", &artist, &zephyr_albums()),
        "02-browsing",
        "artist-external-json",
    );
    assert_golden_with_explicit_status(
        builder_at("2026-10-04T08:07:45.952Z").create_artist_response("xml", &artist, &zephyr_albums()),
        "02-browsing",
        "artist-external-xml",
    );
}

#[test]
fn an_outside_album_matches_the_recordings() {
    let songs = vec![
        Song {
            year: Some(2019),
            genre: Some("Alternative".into()),
            ..glass_harbor_song(
                "WwNjVlSZUyQ4mnNfzQuIE2",
                "Glass Harbor",
                Some(1),
                241,
                Some("QZPAR1900001"),
            )
        },
        Song {
            year: Some(2019),
            genre: Some("Alternative".into()),
            ..glass_harbor_song(
                "hG8GzI7uHYDHVaItnVVwf2",
                "Neon Tide",
                Some(2),
                215,
                Some("QZPAR1900002"),
            )
        },
        Song {
            year: Some(2019),
            genre: Some("Alternative".into()),
            ..glass_harbor_song(
                "3sifqfVllshUGUHcQSf9W3",
                "Paper Moons",
                Some(3),
                244,
                Some("QZPAR1900003"),
            )
        },
    ];
    let album = Album {
        id: GLASS_HARBOR.into(),
        title: "Glass Harbor".into(),
        artist: "Zephyr Echo".into(),
        artist_id: Some(ZEPHYR.into()),
        genre: Some("Alternative".into()),
        year: Some(2019),
        release_types: vec!["album".into()],
        songs,
        ..Default::default()
    };
    assert_golden_with_explicit_status(
        builder_at("2026-10-04T08:07:46.052Z").create_album_response("json", &album),
        "02-browsing",
        "album-external-json",
    );
    assert_golden_with_explicit_status(
        builder_at("2026-10-04T08:07:46.056Z").create_album_response("xml", &album),
        "02-browsing",
        "album-external-xml",
    );
}

#[test]
fn info_answers_match_the_recordings() {
    let cover = "https://e-cdns-images.dzcdn.net/images/cover/glass/1000x1000-000000-80-0-0.jpg";
    let album_info = [
        ("notes", ""),
        ("smallImageUrl", cover),
        ("mediumImageUrl", cover),
        ("largeImageUrl", cover),
    ];
    assert_golden(
        builder().create_info_response("json", "albumInfo", &album_info),
        "02-browsing",
        "album-info2-external-json",
    );
    assert_golden(
        builder().create_info_response("xml", "albumInfo", &album_info),
        "02-browsing",
        "album-info2-external-xml",
    );
    let image = "https://e-cdns-images.dzcdn.net/images/artist/zephyr/1000x1000-000000-80-0-0.jpg";
    let artist_info = [
        ("biography", ""),
        ("smallImageUrl", image),
        ("mediumImageUrl", image),
        ("largeImageUrl", image),
    ];
    assert_golden(
        builder().create_info_response("json", "artistInfo2", &artist_info),
        "02-browsing",
        "artist-info2-external-json",
    );
    assert_golden(
        builder().create_info_response("xml", "artistInfo2", &artist_info),
        "02-browsing",
        "artist-info-v1-external-xml",
    );
}

// ---- Library albums and artists: Navidrome's JSON, answered in the format asked for --------

/// getAlbum/getArtist for a library id: Navidrome's JSON converted as the controller converts
/// it, then the merged response.
fn merged(element: &str, navidrome: &Value, format: &str) -> SubsonicReply {
    let builder = builder();
    let data = builder
        .convert_subsonic_json_element(navidrome, true)
        .expect("an object");
    builder.create_merged_response(format, element, &data)
}

#[test]
fn library_albums_match_the_recordings_in_every_format() {
    for (json_file, json_name, xml_names) in [
        (
            "00-setup",
            "album-flac",
            &["album-local-flac-xml", "album-local-jsonp"][..],
        ),
        ("00-setup", "album-m4a", &["album-local-m4a-xml"][..]),
        ("02-browsing", "album-local-mp3-json", &[][..]),
    ] {
        let navidrome = navidrome_row(recorded_json(
            json_file,
            json_name,
            &["subsonic-response", "album"],
        ));
        assert_golden(merged("album", &navidrome, "json"), json_file, json_name);
        for xml_name in xml_names {
            let format = if xml_name.ends_with("jsonp") {
                "jsonp"
            } else {
                "xml"
            };
            assert_golden(merged("album", &navidrome, format), "02-browsing", xml_name);
        }
    }
}

#[test]
fn library_artists_match_the_recordings_in_every_format() {
    let navidrome = navidrome_row(recorded_json(
        "02-browsing",
        "artist-local-json",
        &["subsonic-response", "artist"],
    ));
    assert_golden(
        merged("artist", &navidrome, "json"),
        "02-browsing",
        "artist-local-json",
    );
    let navidrome = navidrome_row(recorded_json(
        "02-browsing",
        "artist-local-cjk-json",
        &["subsonic-response", "artist"],
    ));
    assert_golden(
        merged("artist", &navidrome, "json"),
        "02-browsing",
        "artist-local-cjk-json",
    );

    // The XML requests asked Navidrome again, and it listed the roles in another order.
    let mut navidrome = navidrome_row(recorded_json(
        "02-browsing",
        "artist-local-json",
        &["subsonic-response", "artist"],
    ));
    navidrome["roles"] = json!(["albumartist", "artist", "maincredit"]);
    assert_golden(
        merged("artist", &navidrome, "xml"),
        "02-browsing",
        "artist-local-xml",
    );
    let recorded_roles = XElement::parse(&recorded("02-browsing", "artist-local-jsonp").body)
        .expect("XML")
        .elements()
        .next()
        .expect("artist")
        .elements()
        .filter(|e| e.name == "roles")
        .map(|e| e.value())
        .collect::<Vec<_>>();
    // The f=jsonp request is for another artist, whose JSON was not recorded: Navidrome's
    // artist page has the same shape for every artist, so it is the first one's with this
    // one's values.
    let mut navidrome = navidrome_row(recorded_json(
        "02-browsing",
        "artist-local-json",
        &["subsonic-response", "artist"],
    ));
    navidrome["id"] = json!("17qiGGo3gM1YI35DUCa4fC");
    navidrome["name"] = json!("Bjørn Ålund");
    navidrome["sortName"] = json!("bjorn alund");
    navidrome["roles"] = json!(recorded_roles);
    let album = &mut navidrome["album"][0];
    album["id"] = json!("3sYt5hQaQpB2FKOkr3RxZT");
    album["name"] = json!("Ágætis Prófun");
    album["artist"] = json!("Bjørn Ålund");
    album["artistId"] = json!("17qiGGo3gM1YI35DUCa4fC");
    album["coverArt"] = json!("al-3sYt5hQaQpB2FKOkr3RxZT_e0d327805ce38bc7");
    album["songCount"] = json!(3);
    album["duration"] = json!(8);
    album["created"] = json!("2026-10-04T07:40:52.392626057Z");
    album["year"] = json!(2018);
    album["genre"] = json!("Post-Rock");
    album["genres"] = json!([{ "name": "Post-Rock" }]);
    album["sortName"] = json!("agaetis profun");
    album["artists"] = json!([{ "id": "17qiGGo3gM1YI35DUCa4fC", "name": "Bjørn Ålund" }]);
    album["displayArtist"] = json!("Bjørn Ålund");
    assert_golden(
        merged("artist", &navidrome, "jsonp"),
        "02-browsing",
        "artist-local-jsonp",
    );
}

// ---- search3 / search2: the merge -------------------------------------------------------

/// The controller's envelope around merged search rows.
fn search_reply(
    builder: &SubsonicResponseBuilder,
    envelope: &str,
    is_json: bool,
    rows: (Vec<Row>, Vec<Row>, Vec<Row>),
) -> SubsonicReply {
    let (songs, albums, artists) = rows;
    if is_json {
        let list = |rows: Vec<Row>| Value::Array(rows.into_iter().filter_map(Row::into_json).collect());
        let mut result = Map::new();
        result.insert("song".into(), list(songs));
        result.insert("album".into(), list(albums));
        result.insert("artist".into(), list(artists));
        let mut body = Map::new();
        body.insert("status".into(), "ok".into());
        body.insert("version".into(), "1.16.1".into());
        body.insert(envelope.into(), Value::Object(result));
        return builder.create_json_response(Value::Object(body));
    }
    let result = XElement::ns(SUBSONIC_NAMESPACE, envelope).children(
        artists
            .into_iter()
            .chain(albums)
            .chain(songs)
            .filter_map(Row::into_xml),
    );
    SubsonicReply::xml(
        &XElement::ns(SUBSONIC_NAMESPACE, "subsonic-response")
            .attr("status", "ok")
            .attr("version", "1.16.1")
            .child(result),
    )
}

/// Navidrome's JSON page: the recorded rows Octo did not add, without the mark Octo put on them.
fn navidrome_json_page(file: &str, name: &str, envelope: &str) -> String {
    let result = recorded_json(file, name, &["subsonic-response", envelope]);
    let mut page = Map::new();
    for kind in ["song", "album", "artist"] {
        let rows: Vec<Value> = result[kind]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|row| row["isExternal"] == false)
                    .cloned()
                    .map(navidrome_row)
                    .collect()
            })
            .unwrap_or_default();
        page.insert(kind.into(), Value::Array(rows));
    }
    json!({ "subsonic-response": {
        "status": "ok", "version": "1.16.1", "type": "navidrome", "serverVersion": "0.64.2 (10114574)",
        "openSubsonic": true, envelope: page,
    }})
    .to_string()
}

/// Navidrome's XML page: the recorded library rows, unmarked, compact as Navidrome writes.
fn navidrome_xml_page(file: &str, name: &str, envelope: &str) -> String {
    let answer = XElement::parse(&recorded(file, name).body).expect("XML");
    let result = answer.elements().next().expect("the envelope");
    let mut page = XElement::ns(SUBSONIC_NAMESPACE, envelope);
    for row in result.elements() {
        if row.attribute("isExternal") != Some("false") {
            continue;
        }
        let mut row = row.clone();
        row.attributes.retain(|(name, _)| name != "isExternal");
        page.push(row);
    }
    let text = XElement::ns(SUBSONIC_NAMESPACE, "subsonic-response")
        .attr("status", "ok")
        .attr("version", "1.16.1")
        .attr("type", "navidrome")
        .child(page)
        .to_xml_string();
    // Navidrome writes no whitespace between elements, which the read drops anyway.
    text.lines().map(str::trim_start).collect::<Vec<_>>().join("")
}

fn borealis() -> SearchResult {
    SearchResult {
        // Last.fm's row: no album, no length, no track; ids minted by the registry.
        songs: vec![Song {
            id: "rhcHCXhQyCj24j4PjWSZF4".into(),
            title: "Borealis".into(),
            artist: "Aurora Vale".into(),
            ..Default::default()
        }],
        albums: vec![],
        // The library already has this artist, so the outside one is left out.
        artists: vec![Artist {
            id: "x".into(),
            name: "aurora vale".into(),
            ..Default::default()
        }],
    }
}

/// A recorded mixed search with the outside row's guessed length taken out. The recording wrote
/// a song of unknown length as 180 seconds; it now goes out with no `duration` at all.
fn recorded_without_a_guessed_length(file: &str, name: &str) -> Recorded {
    let mut want = recorded(file, name);
    let row = r#""id":"rhcHCXhQyCj24j4PjWSZF4""#;
    let xml_row = r#"<song id="rhcHCXhQyCj24j4PjWSZF4""#;
    let (start, guess) = match (want.body.find(row), want.body.find(xml_row)) {
        (Some(at), _) => (at, r#""duration":180,"#),
        (None, Some(at)) => (at, r#" duration="180""#),
        (None, None) => panic!("{file}/{name}: no outside row"),
    };
    let at = start + want.body[start..].find(guess).expect("the guessed length");
    want.body.replace_range(at..at + guess.len(), "");
    want.content_length = None;
    want
}

#[test]
fn a_mixed_search_matches_the_recordings() {
    let mapper = SubsonicModelMapper::new(builder());
    let page = navidrome_json_page("03-search", "s3-mixed-json", "searchResult3");
    let local = mapper.parse_search_response(page.as_bytes(), Some("application/json"));
    let rows = mapper.merge_search_results(local.0, local.1, local.2, &borealis(), &[], true, None);
    assert_recorded(
        search_reply(&builder(), "searchResult3", true, rows),
        recorded_without_a_guessed_length("03-search", "s3-mixed-json"),
        "03-search",
        "s3-mixed-json",
    );

    for (name, envelope) in [
        ("s3-mixed-xml", "searchResult3"),
        ("s2-mixed-xml", "searchResult2"),
    ] {
        let page = navidrome_xml_page("03-search", name, envelope);
        let local = mapper.parse_search_response(page.as_bytes(), Some("application/xml"));
        let rows = mapper.merge_search_results(local.0, local.1, local.2, &borealis(), &[], false, None);
        assert_recorded(
            search_reply(&builder(), envelope, false, rows),
            recorded_without_a_guessed_length("03-search", name),
            "03-search",
            name,
        );
    }
}

#[test]
fn a_library_only_search_matches_the_recordings() {
    let mapper = SubsonicModelMapper::new(builder());
    for (name, envelope) in [
        ("s3-local-only-json", "searchResult3"),
        ("s3-diacritics-json", "searchResult3"),
        ("s3-cjk-json", "searchResult3"),
        ("s2-local-json", "searchResult2"),
    ] {
        let page = navidrome_json_page("03-search", name, envelope);
        let local = mapper.parse_search_response(page.as_bytes(), Some("application/json"));
        let rows = mapper.merge_search_results(
            local.0,
            local.1,
            local.2,
            &SearchResult::default(),
            &[],
            true,
            None,
        );
        assert_golden(search_reply(&builder(), envelope, true, rows), "03-search", name);
    }
    let page = navidrome_xml_page("03-search", "s3-local-only-xml", "searchResult3");
    let local = mapper.parse_search_response(page.as_bytes(), Some("application/xml"));
    let rows = mapper.merge_search_results(
        local.0,
        local.1,
        local.2,
        &SearchResult::default(),
        &[],
        false,
        None,
    );
    assert_golden(
        search_reply(&builder(), "searchResult3", false, rows),
        "03-search",
        "s3-local-only-xml",
    );
}

fn zephyr_search() -> SearchResult {
    let song = |id: &str, title: &str, duration: i32| Song {
        id: id.into(),
        title: title.into(),
        artist: "Zephyr Echo".into(),
        album: "Glass Harbor".into(),
        duration: Some(duration),
        year: Some(2019),
        ..Default::default()
    };
    SearchResult {
        songs: vec![
            song("CwoXcVhGn95tWIOvqaFKt6", "Glass Harbor", 241),
            song("yy5PCYFGn1qhbOho4j6OF1", "Neon Tide", 215),
            song("DUlkxONpTyX8xFkBJo1fi4", "Paper Moons", 244),
        ],
        albums: vec![Album {
            id: GLASS_HARBOR.into(),
            title: "Glass Harbor".into(),
            artist: "Zephyr Echo".into(),
            song_count: Some(3),
            release_types: vec!["album".into()],
            ..Default::default()
        }],
        artists: vec![Artist {
            id: ZEPHYR.into(),
            name: "Zephyr Echo".into(),
            album_count: Some(2),
            ..Default::default()
        }],
    }
}

#[test]
fn a_discovery_only_search_matches_the_recordings() {
    let empty_json = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{}}}"#;
    for (name, created) in [
        ("s3-discovery-only-json-repeat", "2026-10-04T08:07:46.942Z"),
        ("discovery-zephyr", ""),
    ] {
        let file = if name == "discovery-zephyr" {
            "00-setup"
        } else {
            "03-search"
        };
        let created = if created.is_empty() {
            recorded_json(file, name, &["subsonic-response", "searchResult3", "album"])[0]["created"]
                .as_str()
                .expect("created")
                .to_string()
        } else {
            created.to_string()
        };
        let builder = builder_at(&created);
        let mapper = SubsonicModelMapper::new(builder.clone());
        let local = mapper.parse_search_response(empty_json.as_bytes(), Some("application/json"));
        let rows = mapper.merge_search_results(local.0, local.1, local.2, &zephyr_search(), &[], true, None);
        assert_golden_with_explicit_status(search_reply(&builder, "searchResult3", true, rows), file, name);
    }

    let builder = builder_at("2026-10-04T08:07:46.890Z");
    let mapper = SubsonicModelMapper::new(builder.clone());
    let empty_xml = r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><searchResult3></searchResult3></subsonic-response>"#;
    let local = mapper.parse_search_response(empty_xml.as_bytes(), Some("application/xml"));
    let rows = mapper.merge_search_results(local.0, local.1, local.2, &zephyr_search(), &[], false, None);
    assert_golden_with_explicit_status(
        search_reply(&builder, "searchResult3", false, rows),
        "03-search",
        "s3-discovery-only-xml",
    );
}

#[test]
fn an_empty_search_in_xml_is_an_empty_envelope() {
    let rows = (Vec::new(), Vec::new(), Vec::new());
    assert_golden(
        search_reply(&builder(), "searchResult3", false, rows),
        "03-search",
        "s3-jsonp",
    );
}

// ---- getOpenSubsonicExtensions -------------------------------------------------------------

#[test]
fn extensions_match_the_recordings() {
    // Navidrome's own XML, which the f=JSON request passed through untouched.
    let navidrome_xml = recorded("01-system", "extensions-f-JSON").body;
    for name in ["extensions-xml", "extensions-xml-badpass"] {
        assert_recorded(
            builder().merge_open_subsonic_extensions(
                "xml",
                Some(navidrome_xml.as_bytes()),
                Some("application/xml"),
                true,
                false,
            ),
            recorded_with_ported_extensions("01-system", name),
            "01-system",
            name,
        );
    }
    // f=JSON is JSON to the merge, which cannot read Navidrome's XML: passed through.
    let passed = builder().merge_open_subsonic_extensions(
        "JSON",
        Some(navidrome_xml.as_bytes()),
        Some("application/xml"),
        true,
        false,
    );
    assert_eq!(passed.kind, ReplyKind::File);
    assert_golden(passed, "01-system", "extensions-f-JSON");

    // Navidrome's JSON is the recorded answer without the two extensions Octo added.
    let mut navidrome: Value =
        serde_json::from_str(&recorded("01-system", "extensions-json").body).expect("JSON");
    navidrome["subsonic-response"]["openSubsonicExtensions"]
        .as_array_mut()
        .expect("extensions")
        .retain(|extension| !extension["name"].as_str().unwrap_or("").starts_with("octo"));
    let navidrome = navidrome.to_string();
    for name in ["extensions-json", "extensions-json-noauth"] {
        assert_recorded(
            builder().merge_open_subsonic_extensions(
                "json",
                Some(navidrome.as_bytes()),
                Some("application/json"),
                true,
                false,
            ),
            recorded_with_ported_extensions("01-system", name),
            "01-system",
            name,
        );
    }
}

// ---- Lyrics -------------------------------------------------------------------------------

#[test]
fn lyrics_answers_match_the_recordings() {
    let lrclib = LyricsResult::new(
        "lrclib",
        Some("[00:00.50]Long dark sky\n[00:01.80]Stars above".into()),
        Some("Long dark sky\nStars above".into()),
        false,
    );
    assert_golden(
        builder().create_lyrics_list_response("json", Some(&lrclib), "Aurora Vale", "Polar Night", false),
        "06-lyrics",
        "by-id-lrclib-json",
    );
    let plain = LyricsResult::new(
        "lyrics.ovh",
        None,
        Some("Glass harbor, glass harbor\nWaves of light".into()),
        false,
    );
    assert_golden(
        builder().create_lyrics_list_response("json", Some(&plain), "Zephyr Echo", "Glass Harbor", false),
        "06-lyrics",
        "by-id-external-json",
    );
    assert_golden(
        builder().create_lyrics_list_response("json", None, "Zephyr Echo", "Neon Tide", false),
        "06-lyrics",
        "by-id-external-octo-client-json",
    );
    assert_golden(
        builder().create_lyrics_list_response("json", None, "Bjørn Ålund", "Ljós í myrkri", false),
        "90-mutations",
        "lyrics-by-id-hidden-json",
    );
    assert_golden(
        builder().create_lyrics_response("json", Some(&plain), "Zephyr Echo", "Glass Harbor"),
        "06-lyrics",
        "lyrics-live-fill-json",
    );

    let candidate = LyricsChoiceCandidate {
        id: "lrclib:9001".into(),
        source: "lrclib".into(),
        title: "Polar Night".into(),
        artist: "Aurora Vale".into(),
        album: Some("Northern Lights".into()),
        duration_seconds: Some(3),
        kind: "line".into(),
        same_song: true,
        preview: vec!["Long dark sky".into(), "Stars above".into()],
    };
    assert_golden(
        builder().create_lyrics_candidates_response("6TOMTUtoEzhnB8tjXmNY5V", "auto", &[candidate]),
        "06-lyrics",
        "candidates-json",
    );
    assert_golden(
        builder().create_lyrics_candidates_response("2A1r2OKTiTVuvPfmtePIIe", "auto", &[]),
        "06-lyrics",
        "candidates-xml-still-json",
    );
    assert_golden(
        builder().create_lyrics_choice_response("doesnotexist000000000x", "auto"),
        "06-lyrics",
        "set-choice-auto-unknown-song",
    );
}

// ---- Octo's own extensions ----------------------------------------------------------------

#[test]
fn octo_extensions_match_the_recordings() {
    for name in ["acquisitions-json", "acquisitions-xml-still-json"] {
        assert_golden(
            builder().create_acquisitions_response(&[]),
            "07-octo-extensions",
            name,
        );
    }
    // Octo 2026.10.05 adds "admin" after the actions and offers the six edits even while library
    // actions are off; the 2026.10.03.2 recording is kept and those two changes made to it.
    let mut want = recorded("07-octo-extensions", "library-actions-json");
    want.body = want.body.replace(
        r#""actions":[],"#,
        r#""actions":["retag","joinAlbum","lookup","undo","restore","cover"],"admin":true,"#,
    );
    want.content_length = None;
    assert_recorded(
        builder().create_library_actions_response(
            &LibraryActionSettings::default(),
            Some("admin"),
            1,
            true,
            "Soulseek",
            true,
            true,
            true,
        ),
        want,
        "07-octo-extensions",
        "library-actions-json",
    );
    for name in ["upgrades-json", "upgrades-listener"] {
        assert_golden(
            builder().create_upgrades_response(&[]),
            "07-octo-extensions",
            name,
        );
    }
    let off = LibraryActionOutcome::new(
        LibraryActionState::Skipped,
        Some("Library actions are off.".into()),
    );
    assert_golden(
        builder().create_library_action_outcome_response("2A1r2OKTiTVuvPfmtePIIe", &off, "remove"),
        "07-octo-extensions",
        "library-action-remove-off",
    );
    assert_golden(
        builder().create_library_action_outcome_response("79UbMHtfpdvxM9dnNYPHLu", &off, "upgrade"),
        "07-octo-extensions",
        "library-action-upgrade-off",
    );
}
