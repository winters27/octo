//! Tests for Apple's answers in Deezer's shape, on answers recorded from Apple's Indian store.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/apple")
}

fn recorded(name: &str) -> Value {
    let text = std::fs::read_to_string(fixtures().join(name)).expect("the recorded answer reads");
    serde_json::from_str(&text).expect("the recorded answer is JSON")
}

fn read<T>(answer: AppleRead<T>) -> T {
    match answer {
        AppleRead::Read(items) => items,
        AppleRead::Unreadable => panic!("a recorded answer reads"),
    }
}

const INTERSTELLAR: u64 = 1_533_983_552;

#[test]
fn an_apple_album_becomes_a_deezer_album_object() {
    let albums = read(albums(&recorded("search-album-interstellar-in.json")));
    let album = albums
        .iter()
        .find(|album| album["id"] == json!(code_apple_id(INTERSTELLAR)))
        .expect("the Hans Zimmer soundtrack is listed");
    assert_eq!(album["id"], json!(10_001_533_983_552_u64));
    assert_eq!(
        album["title"],
        "Interstellar (Original Motion Picture Soundtrack) [Expanded Edition]"
    );
    assert_eq!(album["artist"]["name"], "Hans Zimmer");
    assert_eq!(album["artist"]["id"], json!(code_apple_id(454_295_032)));
    assert_eq!(album["nb_tracks"], 30);
    assert_eq!(album["release_date"], "2014-11-18");
    assert_eq!(album["record_type"], "album");
    assert_eq!(album["explicit_lyrics"], false);
    assert_eq!(album["explicit_content_lyrics"], 0);
    assert_eq!(album["genres"]["data"][0]["name"], "Soundtrack");
    assert_eq!(album["type"], "album");
    let cover = album["cover_xl"].as_str().expect("a cover");
    assert!(cover.ends_with("/794043201943.jpg/1000x1000bb.jpg"), "{cover}");
    assert!(
        album["cover_medium"]
            .as_str()
            .unwrap()
            .ends_with("/250x250bb.jpg")
    );
    assert!(album.get("upc").is_none() && album.get("label").is_none());
}

#[test]
fn a_single_or_an_ep_loses_its_tail_and_says_what_it_is() {
    let albums = read(albums(&recorded("search-album-anuv-jain-in.json")));
    let arz = albums
        .iter()
        .find(|album| album["id"] == json!(code_apple_id(1_831_237_583)))
        .expect("the Coke Studio single is listed");
    assert_eq!(arz["title"], "Arz Kiya Hai - Coke Studio Bharat");
    assert_eq!(arz["record_type"], "single");
    let ep = album_object(&json!({
        "wrapperType": "collection", "collectionId": 7, "collectionName": "Night Drive - EP",
        "artistName": "Someone", "collectionType": "Album", "trackCount": 5,
    }))
    .expect("an EP reads");
    assert_eq!(ep["title"], "Night Drive");
    assert_eq!(ep["record_type"], "ep");
}

#[test]
fn a_compilation_or_various_artists_is_compile() {
    let compilation = album_object(&json!({
        "wrapperType": "collection", "collectionId": 8, "collectionName": "Now 100",
        "artistName": "Many", "collectionType": "Compilation",
    }))
    .unwrap();
    assert_eq!(compilation["record_type"], "compile");
    let various = album_object(&json!({
        "wrapperType": "collection", "collectionId": 9, "collectionName": "La La Land",
        "artistName": "Various Artists", "collectionType": "Album",
    }))
    .unwrap();
    assert_eq!(various["record_type"], "compile");
}

#[test]
fn an_apple_song_becomes_a_deezer_track_object() {
    let lists = read(tracklists(&recorded("lookup-1533983552-songs-in.json")));
    let (album, songs) = lists.get(&INTERSTELLAR).expect("the album's songs");
    assert_eq!(album.as_ref().expect("its album object")["nb_tracks"], 30);
    assert_eq!(songs.len(), 30);
    let stay = &songs[4];
    assert_eq!(stay["title"], "Stay");
    assert_eq!(stay["track_position"], 5);
    assert_eq!(stay["disk_number"], 1);
    assert_eq!(stay["duration"], 412);
    assert_eq!(stay["readable"], true);
    assert!(
        stay["preview"]
            .as_str()
            .unwrap()
            .starts_with("https://audio-ssl.itunes.apple.com/")
    );
    assert!(apple_id(&stay["id"].to_string()).is_some(), "{}", stay["id"]);
    assert_eq!(stay["album"]["id"], json!(code_apple_id(INTERSTELLAR)));
    assert_eq!(stay["album"]["record_type"], "album");
    assert_eq!(stay["artist"]["name"], "Hans Zimmer");
    assert!(stay.get("isrc").is_none() && stay.get("rank").is_none());
    let positions: Vec<i64> = songs
        .iter()
        .map(|song| song["track_position"].as_i64().unwrap())
        .collect();
    assert_eq!(positions, (1..=30).collect::<Vec<_>>(), "disc and track order");
    assert_eq!(songs[14]["title"], "S.T.A.Y.");
    assert_eq!(songs[14]["duration"], 384);
}

#[test]
fn explicit_and_cleaned_songs_carry_deezers_words() {
    let song = |word: &str| {
        track_object(
            &json!({
                "wrapperType": "track", "kind": "song", "trackId": 11, "trackName": "Song",
                "trackExplicitness": word, "collectionId": 12, "collectionName": "Record",
            }),
            None,
        )
        .unwrap()
    };
    assert_eq!(
        (
            song("explicit")["explicit_lyrics"].clone(),
            song("explicit")["explicit_content_lyrics"].clone()
        ),
        (json!(true), json!(1))
    );
    assert_eq!(
        (
            song("cleaned")["explicit_lyrics"].clone(),
            song("cleaned")["explicit_content_lyrics"].clone()
        ),
        (json!(false), json!(3))
    );
    assert_eq!(song("notExplicit")["explicit_content_lyrics"], 0);
}

#[test]
fn music_videos_are_left_out() {
    let answer = json!({ "resultCount": 3, "results": [
        { "wrapperType": "collection", "collectionId": 5, "collectionName": "Record", "artistName": "A" },
        { "wrapperType": "track", "kind": "song", "trackId": 51, "trackName": "One", "collectionId": 5, "collectionName": "Record", "trackNumber": 1 },
        { "wrapperType": "track", "kind": "music-video", "trackId": 52, "trackName": "One (Video)", "collectionId": 5, "collectionName": "Record" },
    ]});
    let tracks = read(tracks(&answer));
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0]["title"], "One");
    let lists = read(tracklists(&answer));
    assert_eq!(lists[&5].1.len(), 1);
}

#[test]
fn empty_answers_read_as_nothing_and_no_data_is_deezers_800() {
    assert_eq!(
        read(albums(&json!({ "resultCount": 0, "results": [] }))),
        Vec::<Value>::new()
    );
    let list = list_answer(Vec::new(), 0);
    assert_eq!(list["data"], json!([]));
    assert_eq!(list["total"], 0);
    assert!(is_marked(&list));
    let none = no_data_answer();
    assert_eq!(none["error"]["code"], 800);
    assert!(is_marked(&none));
    assert!(!is_marked(&json!({ "data": [] })));
}

#[test]
fn apple_ids_round_trip_and_deezer_ids_are_never_apples() {
    assert_eq!(
        apple_id(&code_apple_id(INTERSTELLAR).to_string()),
        Some(INTERSTELLAR)
    );
    assert_eq!(apple_id("10001533983552"), Some(INTERSTELLAR));
    for deezer in [
        "185320622",
        "3500000000",
        "-3135556",
        "0",
        "",
        "abc",
        "10000000000000",
    ] {
        assert_eq!(apple_id(deezer), None, "{deezer}");
        assert!(!is_apple_id(deezer), "{deezer}");
    }
    assert!(is_apple_number(10_001_533_983_552));
    assert!(!is_apple_number(185_320_622));
    assert!(!is_apple_number(-3_135_556));
    // Apple ids are carried as plain digits, so no reader that cuts at a dash ever cuts one.
    assert!(!code_apple_id(INTERSTELLAR).to_string().contains('-'));
}

#[test]
fn the_catalog_that_answered_is_named_by_its_id() {
    assert_eq!(
        catalog_name(&code_apple_id(INTERSTELLAR).to_string()),
        "Apple Music"
    );
    assert_eq!(catalog_name("185320622"), "Deezer");
    assert_eq!(catalog_name(""), "Deezer");
}

#[test]
fn the_artwork_file_name_names_the_barcode() {
    let art = "https://is1-ssl.mzstatic.com/image/thumb/Music125/v4/f4/5b/73/f45b735a-8d7a-9713-b217-0f8e1593c28b/794043201943.jpg/100x100bb.jpg";
    assert_eq!(artwork_barcodes(art), vec!["794043201943".to_string()]);
    let padded =
        "https://is1-ssl.mzstatic.com/image/thumb/Music/v4/aa/bb/cc/00050086144778.jpg/100x100bb.jpg";
    assert_eq!(
        artwork_barcodes(padded),
        vec!["00050086144778".to_string(), "050086144778".to_string()]
    );
    let none = "https://is1-ssl.mzstatic.com/image/thumb/Music/v4/aa/bb/cc/mzi.abcdefgh.jpg/100x100bb.jpg";
    assert!(artwork_barcodes(none).is_empty());
    assert_eq!(artwork(art, 600).rsplit('/').next(), Some("600x600bb.jpg"));
    assert_eq!(
        artwork("https://example.com/cover.png", 600),
        "https://example.com/cover.png"
    );
}

#[test]
fn an_answer_whose_shape_changed_is_unreadable() {
    let renamed = json!({ "resultCount": 2, "results": [
        { "type": "collection", "id": 5, "name": "Record" },
        { "type": "collection", "id": 6, "name": "Other" },
    ]});
    assert_eq!(albums(&renamed), AppleRead::Unreadable);
    assert_eq!(tracks(&renamed), AppleRead::Unreadable);
    assert_eq!(tracklists(&renamed), AppleRead::Unreadable);
    assert_eq!(albums(&json!({ "error": "x" })), AppleRead::Unreadable);
    assert_eq!(
        chart_ids(&json!({ "feed": { "results": [{ "x": 1 }] } })),
        AppleRead::Unreadable
    );
}

#[test]
fn the_chart_feed_names_its_songs_in_order() {
    let ids = read(chart_ids(&recorded("rss-most-played-in-10.json")));
    assert_eq!(ids.len(), 10);
    assert!(ids.iter().all(|id| *id > 0));
}

#[test]
fn a_musicartist_result_becomes_an_artist_object() {
    let artists = read(artists(&recorded("search-artist-hans-zimmer-in.json")));
    let first = &artists[0];
    assert_eq!(first["name"], "Hans Zimmer");
    assert_eq!(first["id"], json!(code_apple_id(454_295_032)));
    assert_eq!(first["type"], "artist");
}

#[test]
fn release_dates_and_lengths_read_as_deezers() {
    assert_eq!(
        release_date(Some("2014-11-18T08:00:00Z")).as_deref(),
        Some("2014-11-18")
    );
    assert_eq!(release_date(Some("2014")), None);
    assert_eq!(release_date(None), None);
    assert_eq!(seconds(Some(412_424.0)), Some(412));
    assert_eq!(seconds(Some(383_513.0)), Some(384));
    assert_eq!(seconds(Some(0.0)), None);
}
