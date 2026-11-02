//! Apple's catalog answers (the iTunes Search API and Apple's chart feed) in the shape of Deezer's:
//! the album, track and artist objects, list answers and "no data" answers that Octo's Deezer readers
//! already read. An Apple id rides in Deezer's numeric id fields as the Apple id plus
//! [`APPLE_ID_BASE`], so every reader keeps its meaning and an Apple id never reaches Deezer.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::common::dotnet;
use crate::tagging::release_distance::barcode_forms;

/// Added to an Apple id so it rides in Deezer's id fields: positive, plain digits, and far above
/// Deezer's own ids (about 3.5e9 in 2026; some of Deezer's are negative, and stay Deezer's).
pub const APPLE_ID_BASE: u64 = 10_000_000_000_000;

/// The key every answer built from Apple's catalog carries at its top level, with
/// [`MARKER_VALUE`].
pub const MARKER: &str = "octo_catalog";
pub const MARKER_VALUE: &str = "apple";

static RELEASE_TAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s+-\s+(single|ep)\s*$").expect("the release tail pattern compiles"));

static ART_SIZE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/\d+x\d+(bb|cc|-\d+)?\.(jpg|jpeg|png|webp)$").expect("the artwork size pattern compiles")
});

static ART_BARCODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/(\d{12,14})(?:[-_.][^/]*)?\.(?:jpg|jpeg|png|tif|tiff)/")
        .expect("the artwork barcode pattern compiles")
});

/// The Deezer-field id of an Apple id.
pub fn code_apple_id(apple: u64) -> u64 {
    apple.saturating_add(APPLE_ID_BASE)
}

/// The Apple id a Deezer-field id stands for, or None for one of Deezer's own ids (negative ones
/// included) or text that is no id.
pub fn apple_id(id: &str) -> Option<u64> {
    let number: u64 = id.trim().parse().ok()?;
    number.checked_sub(APPLE_ID_BASE).filter(|apple| *apple > 0)
}

/// Whether a Deezer-field id is an Apple id.
pub fn is_apple_id(id: &str) -> bool {
    apple_id(id).is_some()
}

/// The catalog that answered for a Deezer-field id, as people read it: "Apple Music" for an Apple
/// id, else "Deezer".
pub fn catalog_name(id: &str) -> &'static str {
    if is_apple_id(id) { "Apple Music" } else { "Deezer" }
}

/// [`is_apple_id`] for an id held as a number.
pub fn is_apple_number(id: i64) -> bool {
    u64::try_from(id).is_ok_and(|id| id > APPLE_ID_BASE)
}

/// Whether an answer was built from Apple's catalog.
pub fn is_marked(answer: &Value) -> bool {
    answer.get(MARKER).and_then(Value::as_str) == Some(MARKER_VALUE)
}

/// A Deezer list answer: the items in order, with how many there are in all.
pub fn list_answer(items: Vec<Value>, total: usize) -> Value {
    json!({ "data": items, "total": total, MARKER: MARKER_VALUE })
}

/// Deezer's own "no data" answer (code 800), which Octo reads as "there is no such thing".
pub fn no_data_answer() -> Value {
    json!({
        "error": { "type": "DataException", "message": "no data", "code": 800 },
        MARKER: MARKER_VALUE,
    })
}

/// `object` with the marker at its top level.
pub fn marked(mut object: Value) -> Value {
    if let Value::Object(map) = &mut object {
        map.insert(MARKER.into(), MARKER_VALUE.into());
    }
    object
}

/// How Apple's answer read.
#[derive(Debug, Clone, PartialEq)]
pub enum AppleRead<T> {
    /// What it holds (possibly nothing, when Apple found nothing).
    Read(T),
    /// It names results, but none of them can be read: Apple's answer has changed shape.
    Unreadable,
}

/// An album's name without the " - Single" or " - EP" Apple adds, and the record type that tail
/// names.
pub fn without_release_tail(name: &str) -> (String, Option<&'static str>) {
    match RELEASE_TAIL.captures(name) {
        Some(found) => {
            let whole = found.get(0).expect("a match has its whole text");
            let kind = if found[1].eq_ignore_ascii_case("single") {
                "single"
            } else {
                "ep"
            };
            (name[..whole.start()].trim_end().to_string(), Some(kind))
        }
        None => (name.trim().to_string(), None),
    }
}

/// Deezer's record type for an Apple collection: "single" or "ep" from its name's tail, "compile"
/// for a compilation or a Various Artists record, else "album".
pub fn record_type(collection: &Value) -> &'static str {
    let name = text(collection, "collectionName").unwrap_or_default();
    if let (_, Some(kind)) = without_release_tail(&name) {
        return kind;
    }
    let compilation = text(collection, "collectionType")
        .is_some_and(|kind| kind.eq_ignore_ascii_case("compilation"))
        || text(collection, "artistName")
            .is_some_and(|artist| artist.trim().eq_ignore_ascii_case("various artists"));
    if compilation { "compile" } else { "album" }
}

/// Deezer's explicit words for Apple's: (explicit_lyrics, explicit_content_lyrics).
pub fn explicit_words(apple: Option<&str>) -> (bool, i32) {
    match apple.map(str::trim) {
        Some(word) if word.eq_ignore_ascii_case("explicit") => (true, 1),
        Some(word) if word.eq_ignore_ascii_case("cleaned") => (false, 3),
        _ => (false, 0),
    }
}

/// Apple's artwork at `side` pixels square: the size at the end of its address replaced. The
/// address as given when it names no size.
pub fn artwork(url: &str, side: u32) -> String {
    match ART_SIZE.captures(url) {
        Some(found) => {
            let whole = found.get(0).expect("a match has its whole text");
            let extension = &found[2];
            format!("{}/{side}x{side}bb.{extension}", &url[..whole.start()])
        }
        None => url.to_string(),
    }
}

/// The barcode forms (`barcode_forms`) of the 12 to 14 digit number Apple's artwork file name
/// carries, or none when it carries none.
pub fn artwork_barcodes(url: &str) -> Vec<String> {
    ART_BARCODE
        .captures(url)
        .map(|found| barcode_forms(Some(&found[1])))
        .unwrap_or_default()
}

/// The date part (yyyy-MM-dd) of Apple's release time.
pub fn release_date(apple: Option<&str>) -> Option<String> {
    let date: String = apple?.trim().chars().take(10).collect();
    let bytes = date.as_bytes();
    (bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && date
            .chars()
            .enumerate()
            .all(|(at, c)| at == 4 || at == 7 || c.is_ascii_digit()))
    .then_some(date)
}

/// A length in milliseconds as Deezer's whole seconds, None for none.
pub fn seconds(milliseconds: Option<f64>) -> Option<i64> {
    let milliseconds = milliseconds.filter(|ms| ms.is_finite() && *ms > 0.0)?;
    Some(dotnet::round(milliseconds / 1000.0, 0) as i64)
}

/// The Deezer album object for an Apple collection result, or None when it is not one.
pub fn album_object(collection: &Value) -> Option<Value> {
    if text(collection, "wrapperType").as_deref() != Some("collection") {
        return None;
    }
    let id = number(collection, "collectionId")?;
    let name = text(collection, "collectionName").filter(|name| !dotnet::is_blank(name))?;
    let (title, _) = without_release_tail(&name);
    let (explicit, explicit_content) = explicit_words(text(collection, "collectionExplicitness").as_deref());
    let mut album = Map::new();
    album.insert("id".into(), code_apple_id(id).into());
    album.insert("title".into(), title.into());
    if let Some(artist) = artist_of(collection, "artistId", "artistName") {
        album.insert("artist".into(), artist);
    }
    insert_covers(&mut album, collection);
    if let Some(count) = number(collection, "trackCount") {
        album.insert("nb_tracks".into(), count.into());
    }
    if let Some(date) = release_date(text(collection, "releaseDate").as_deref()) {
        album.insert("release_date".into(), date.into());
    }
    album.insert("record_type".into(), record_type(collection).into());
    album.insert("explicit_lyrics".into(), explicit.into());
    album.insert("explicit_content_lyrics".into(), explicit_content.into());
    if let Some(genre) = text(collection, "primaryGenreName").filter(|genre| !dotnet::is_blank(genre)) {
        album.insert("genres".into(), json!({ "data": [{ "name": genre }] }));
    }
    if let Some(link) = text(collection, "collectionViewUrl") {
        album.insert("link".into(), link.into());
    }
    album.insert("type".into(), "album".into());
    Some(Value::Object(album))
}

/// The Deezer track object for an Apple song result, or None when it is not one. `collection` is
/// the song's album result from the same answer when there is one, for its record type.
pub fn track_object(song: &Value, collection: Option<&Value>) -> Option<Value> {
    if text(song, "wrapperType").as_deref() != Some("track") || text(song, "kind").as_deref() != Some("song")
    {
        return None;
    }
    let id = number(song, "trackId")?;
    let title = text(song, "trackName").filter(|title| !dotnet::is_blank(title))?;
    let (explicit, explicit_content) = explicit_words(text(song, "trackExplicitness").as_deref());
    let mut track = Map::new();
    track.insert("id".into(), code_apple_id(id).into());
    track.insert("readable".into(), true.into());
    track.insert("title".into(), title.into());
    if let Some(duration) = seconds(song.get("trackTimeMillis").and_then(Value::as_f64)) {
        track.insert("duration".into(), duration.into());
    }
    if let Some(position) = number(song, "trackNumber") {
        track.insert("track_position".into(), position.into());
    }
    if let Some(disc) = number(song, "discNumber") {
        track.insert("disk_number".into(), disc.into());
    }
    if let Some(date) = release_date(text(song, "releaseDate").as_deref()) {
        track.insert("release_date".into(), date.into());
    }
    track.insert("explicit_lyrics".into(), explicit.into());
    track.insert("explicit_content_lyrics".into(), explicit_content.into());
    if let Some(clip) = text(song, "previewUrl").filter(|url| !dotnet::is_blank(url)) {
        track.insert("preview".into(), clip.into());
    }
    if let Some(link) = text(song, "trackViewUrl") {
        track.insert("link".into(), link.into());
    }
    if let Some(artist) = artist_of(song, "artistId", "artistName") {
        track.insert("artist".into(), artist);
    }
    if let Some(album) = album_of_song(song, collection) {
        track.insert("album".into(), album);
    }
    track.insert("type".into(), "track".into());
    Some(Value::Object(track))
}

/// The Deezer artist object for an Apple artist result, or None when it is not one.
pub fn artist_object(artist: &Value) -> Option<Value> {
    if text(artist, "wrapperType").as_deref() != Some("artist") {
        return None;
    }
    let id = number(artist, "artistId")?;
    let name = text(artist, "artistName").filter(|name| !dotnet::is_blank(name))?;
    Some(json!({
        "id": code_apple_id(id),
        "name": name,
        "nb_album": 0,
        "nb_fan": 0,
        "type": "artist",
    }))
}

/// The album objects of an Apple answer, in its order.
pub fn albums(answer: &Value) -> AppleRead<Vec<Value>> {
    read_results(answer, album_object)
}

/// The artist objects of an Apple answer, in its order.
pub fn artists(answer: &Value) -> AppleRead<Vec<Value>> {
    read_results(answer, artist_object)
}

/// The track objects of an Apple answer, in its order, each with its album's record type when
/// the answer carries the album too.
pub fn tracks(answer: &Value) -> AppleRead<Vec<Value>> {
    let collections = collections_by_id(answer);
    read_results(answer, |result| {
        let collection = number(result, "collectionId").and_then(|id| collections.get(&id).copied());
        track_object(result, collection)
    })
}

/// One album's songs from an Apple lookup that may hold several albums (`lookup?id=a,b`): by
/// Apple collection id, the album object and its songs in disc and track order.
pub fn tracklists(answer: &Value) -> AppleRead<HashMap<u64, (Option<Value>, Vec<Value>)>> {
    let Some(results) = answer.get("results").and_then(Value::as_array) else {
        return AppleRead::Unreadable;
    };
    let collections = collections_by_id(answer);
    let mut lists: HashMap<u64, (Option<Value>, Vec<Value>)> = HashMap::new();
    let mut readable = 0usize;
    for result in results {
        if let Some(album) = album_object(result) {
            readable += 1;
            if let Some(id) = number(result, "collectionId") {
                lists.entry(id).or_default().0 = Some(album);
            }
            continue;
        }
        let Some(collection_id) = number(result, "collectionId") else {
            continue;
        };
        if let Some(track) = track_object(result, collections.get(&collection_id).copied()) {
            readable += 1;
            lists.entry(collection_id).or_default().1.push(track);
        } else if text(result, "wrapperType").as_deref() == Some("track") {
            // A music video beside the songs: read, and left out.
            readable += 1;
        }
    }
    if readable == 0 && !results.is_empty() {
        return AppleRead::Unreadable;
    }
    for (_, songs) in lists.values_mut() {
        songs.sort_by_key(|track| {
            (
                track.get("disk_number").and_then(Value::as_i64).unwrap_or(1),
                track
                    .get("track_position")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX),
            )
        });
    }
    AppleRead::Read(lists)
}

/// The song ids of Apple's chart feed, in its order.
pub fn chart_ids(feed: &Value) -> AppleRead<Vec<u64>> {
    let Some(results) = feed.pointer("/feed/results").and_then(Value::as_array) else {
        return AppleRead::Unreadable;
    };
    let ids: Vec<u64> = results
        .iter()
        .filter_map(|row| match row.get("id") {
            Some(Value::String(id)) => id.trim().parse().ok(),
            Some(Value::Number(id)) => id.as_u64(),
            _ => None,
        })
        .collect();
    if ids.is_empty() && !results.is_empty() {
        return AppleRead::Unreadable;
    }
    AppleRead::Read(ids)
}

fn read_results(answer: &Value, read: impl Fn(&Value) -> Option<Value>) -> AppleRead<Vec<Value>> {
    let Some(results) = answer.get("results").and_then(Value::as_array) else {
        return AppleRead::Unreadable;
    };
    let mut items = Vec::new();
    let mut known = 0usize;
    for result in results {
        if result.get("wrapperType").and_then(Value::as_str).is_some() {
            known += 1;
        }
        if let Some(item) = read(result) {
            items.push(item);
        }
    }
    if known == 0 && !results.is_empty() {
        return AppleRead::Unreadable;
    }
    AppleRead::Read(items)
}

fn collections_by_id(answer: &Value) -> HashMap<u64, &Value> {
    answer
        .get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter(|result| result.get("wrapperType").and_then(Value::as_str) == Some("collection"))
                .filter_map(|result| number(result, "collectionId").map(|id| (id, result)))
                .collect()
        })
        .unwrap_or_default()
}

/// The album a song result names: its own collection fields, the record type from `collection`
/// when given, else from the collection's name.
fn album_of_song(song: &Value, collection: Option<&Value>) -> Option<Value> {
    let id = number(song, "collectionId")?;
    let name = text(song, "collectionName").filter(|name| !dotnet::is_blank(name))?;
    let (title, tail) = without_release_tail(&name);
    let kind = match collection {
        Some(collection) => record_type(collection),
        None => match tail {
            Some(kind) => kind,
            None if text(song, "collectionArtistName")
                .is_some_and(|artist| artist.trim().eq_ignore_ascii_case("various artists")) =>
            {
                "compile"
            }
            None => "album",
        },
    };
    let mut album = Map::new();
    album.insert("id".into(), code_apple_id(id).into());
    album.insert("title".into(), title.into());
    insert_covers(&mut album, collection.unwrap_or(song));
    if let Some(date) = release_date(
        collection
            .and_then(|collection| text(collection, "releaseDate"))
            .or_else(|| text(song, "releaseDate"))
            .as_deref(),
    ) {
        album.insert("release_date".into(), date.into());
    }
    album.insert("record_type".into(), kind.into());
    if let Some(artist) = text(song, "collectionArtistName").or_else(|| text(song, "artistName")) {
        album.insert("artist".into(), json!({ "name": artist }));
    }
    album.insert("type".into(), "album".into());
    Some(Value::Object(album))
}

fn insert_covers(object: &mut Map<String, Value>, source: &Value) {
    let Some(art) = text(source, "artworkUrl100").filter(|url| !dotnet::is_blank(url)) else {
        return;
    };
    object.insert("cover_xl".into(), artwork(&art, 1000).into());
    object.insert("cover_big".into(), artwork(&art, 500).into());
    object.insert("cover_medium".into(), artwork(&art, 250).into());
}

fn artist_of(result: &Value, id_field: &str, name_field: &str) -> Option<Value> {
    let name = text(result, name_field).filter(|name| !dotnet::is_blank(name))?;
    Some(match number(result, id_field) {
        Some(id) => json!({ "id": code_apple_id(id), "name": name }),
        None => json!({ "name": name }),
    })
}

fn text(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_string)
}

fn number(value: &Value, field: &str) -> Option<u64> {
    match value.get(field)? {
        Value::Number(number) => number.as_u64().filter(|n| *n > 0),
        Value::String(text) => text.trim().parse().ok().filter(|n| *n > 0),
        _ => None,
    }
}

#[cfg(test)]
#[path = "apple_as_deezer_tests.rs"]
mod tests;
