//! The pure half of the top songs service: how a catalog's list and Last.fm's play counts are
//! merged into one ranked list, what the library is searched with for a row, and which library
//! hit stands for which row. Fetching and the library searches are
//! `octo::services::common::top_songs_service`.

use serde_json::Value;

use super::apple_pages::{AppleSong, artwork_at};
use crate::common::song_identity::{SongIdentity, SongMatchOptions};
use crate::last_fm::last_fm_radio_track_resolver::is_same_recording;

/// The most rows an artist's list is built with. Callers take the prefix they want. Charts
/// have their own sizes (`chart_settings::max_songs`).
pub const MAX_ROWS: usize = 50;

pub const DEEZER_SOURCE: &str = "deezer";
pub const LASTFM_SOURCE: &str = "lastfm";
pub const APPLE_SOURCE: &str = "apple";

/// The cover side an Apple row is shown at.
pub const APPLE_COVER_SIDE: u32 = 1200;

/// Deezer's `explicit_content_lyrics` and `explicit_lyrics` as one number: 1 explicit, 3 the
/// clean edit, 0 neither, None when it does not say. A partly explicit album (4) counts as
/// explicit; the other values (2 unknown, 5, 6, 7 no advice) say nothing, so only the plain flag
/// can answer.
pub fn explicit_from_catalog(content: Option<i32>, explicit_lyrics: Option<bool>) -> Option<i32> {
    match content {
        Some(1 | 4) => Some(1),
        Some(3) => Some(3),
        Some(0) => Some(0),
        _ if explicit_lyrics == Some(true) => Some(1),
        _ => None,
    }
}

/// A catalog's track: a row of Deezer's chart or artist top list, or a row of an Apple chart
/// or room.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CatalogTrack {
    /// The catalog's own id for the track.
    pub catalog_id: String,
    pub title: String,
    /// The main artist.
    pub artist: String,
    /// Every credited artist when there is more than one, main artist first; empty for one.
    pub artists: Vec<String>,
    /// Deezer's artist id, when the catalog is Deezer.
    pub artist_id: Option<String>,
    pub album: Option<String>,
    /// Deezer's album id, when the catalog is Deezer.
    pub album_id: Option<String>,
    pub cover_url: Option<String>,
    pub duration: Option<i32>,
    pub explicit: Option<bool>,
    /// Deezer's `explicit_content_lyrics` (1 and 4 explicit, 3 clean, 0 none).
    pub explicit_content: Option<i32>,
}

impl CatalogTrack {
    /// An Apple row as a catalog track: the main artist is the first Apple lists, the cover is
    /// Apple's artwork at [`APPLE_COVER_SIDE`], and the explicit badge is all Apple says about
    /// explicitness (no badge reads as "none", never as a clean edit). Apple's album and artist
    /// ids stay out: [`Self::artist_id`] and [`Self::album_id`] are Deezer's, which an outside
    /// album or artist page is opened by.
    pub fn from_apple(song: &AppleSong) -> Self {
        let artists = if song.artists.len() > 1 {
            song.artists.clone()
        } else {
            Vec::new()
        };
        Self {
            catalog_id: song.song_id.clone().unwrap_or_default(),
            title: song.title.clone(),
            artist: song.artists.first().cloned().unwrap_or_default(),
            artists,
            artist_id: None,
            album: song.album.clone(),
            album_id: None,
            cover_url: song.artwork.as_deref().map(|t| artwork_at(t, APPLE_COVER_SIDE)),
            duration: song.duration_secs,
            explicit: Some(song.explicit),
            explicit_content: Some(if song.explicit { 1 } else { 0 }),
        }
    }
}

/// Last.fm's ranked track.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CountedTrack {
    pub artist: String,
    pub title: String,
    pub plays: Option<i64>,
    pub listeners: Option<i64>,
}

/// A row the merge settled on: the song as the ranking named it, the catalog track that
/// dresses it when there is one, and its counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRow {
    pub artist: String,
    pub title: String,
    pub catalog: Option<CatalogTrack>,
    pub plays: Option<i64>,
    pub listeners: Option<i64>,
}

/// The same song, for telling a list's rows apart and pairing the two sources: a radio edit is
/// the song, as it is for a station.
fn same_song() -> SongMatchOptions {
    SongMatchOptions {
        length_tolerance_seconds: None,
        also_neutral: vec!["radio edit".to_string()],
        ..SongMatchOptions::default()
    }
}

fn same(
    a_artist: &str,
    a_title: &str,
    b_artist: &str,
    b_title: &str,
    one_artist: bool,
    options: &SongMatchOptions,
) -> bool {
    if one_artist {
        SongIdentity::same_title(a_title, b_title, Some(options)).is_same()
    } else {
        SongIdentity::same_text(a_title, a_artist, b_title, b_artist, Some(options)).is_same()
    }
}

/// The rows of a list, in rank order. With Last.fm's counts, Last.fm's order, each row dressed
/// by the catalog track of the same title when the catalog lists it. Without them, the catalog's
/// order, said to come from `catalog_source`. One row per song: a radio edit listed beside the
/// song is the song, and only its first, higher ranked, listing is kept. On one artist's list
/// titles alone say which song is which; on a chart the artist must agree too.
pub fn merge(
    catalog: Option<&[CatalogTrack]>,
    counts: Option<&[CountedTrack]>,
    one_artist: bool,
    max: usize,
    catalog_source: &'static str,
) -> (Vec<PlannedRow>, &'static str) {
    let options = same_song();
    let mut rows: Vec<PlannedRow> = Vec::new();
    let listed = |rows: &[PlannedRow], artist: &str, title: &str| {
        rows.iter()
            .any(|row| same(&row.artist, &row.title, artist, title, one_artist, &options))
    };

    if let Some(counts) = counts.filter(|c| !c.is_empty()) {
        let mut unused: Vec<CatalogTrack> = catalog.map(<[CatalogTrack]>::to_vec).unwrap_or_default();
        for track in counts {
            if rows.len() >= max {
                break;
            }
            if listed(&rows, &track.artist, &track.title) {
                continue;
            }
            let dressed = unused
                .iter()
                .position(|c| {
                    same(
                        &c.artist,
                        &c.title,
                        &track.artist,
                        &track.title,
                        one_artist,
                        &options,
                    )
                })
                .map(|at| unused.remove(at));
            rows.push(PlannedRow {
                artist: track.artist.clone(),
                title: track.title.clone(),
                catalog: dressed,
                plays: track.plays,
                listeners: track.listeners,
            });
        }
        return (rows, LASTFM_SOURCE);
    }

    for track in catalog.unwrap_or_default() {
        if rows.len() >= max {
            break;
        }
        if listed(&rows, &track.artist, &track.title) {
            continue;
        }
        rows.push(PlannedRow {
            artist: track.artist.clone(),
            title: track.title.clone(),
            catalog: Some(track.clone()),
            plays: None,
            listeners: None,
        });
    }
    let source = if rows.is_empty() { "" } else { catalog_source };
    (rows, source)
}

/// What the library is searched with for a row: its main artist and the title without brackets
/// or guests. Navidrome wants every word it is given, so "(feat. X)" or "(Radio Edit)" would
/// hide a library copy tagged without them.
pub fn library_query(artist: &str, title: &str) -> String {
    let main = SongIdentity::primary_artist(artist);
    let mut core = SongIdentity::parse_title(title, Some(artist)).core;
    if core.is_empty() {
        core = title.to_string();
    }
    format!("{main} {core}").trim().to_string()
}

/// How many songs a library search by artist and title asks for.
pub const LIBRARY_SONGS: usize = 5;

/// How many songs a library search by the bare title asks for: a title alone ("Intro",
/// "Stronger") finds other artists' songs first, so the owned copy is often past the first five.
pub const LIBRARY_TITLE_SONGS: usize = 20;

/// The bare title, the second library search for a row: Navidrome indexes a song under the
/// artist credit its file carries and wants every word it is given, so a copy credited to another
/// name of the artist ("Ye" for "Kanye West") is not found under the name asked, but is under the
/// title alone. [`library_hit`] still judges what it finds. None when it would repeat
/// [`library_query`] (no artist to drop) or has fewer than two letters or digits.
pub fn library_title_query(artist: &str, title: &str) -> Option<String> {
    let mut core = SongIdentity::parse_title(title, Some(artist)).core;
    if core.trim().is_empty() {
        core = title.to_string();
    }
    let core = core.trim();
    if core.chars().filter(|c| c.is_alphanumeric()).count() < 2
        || core.eq_ignore_ascii_case(&library_query(artist, title))
    {
        return None;
    }
    Some(core.to_string())
}

/// The bare-title search to make when the search by artist and title found no copy, only where it
/// can find one: the artist goes by other names (so the library may file the song under one), or
/// the first search's songs hold the title under another credit. None otherwise, so a song the
/// library simply lacks costs one search, not two.
pub fn library_title_retry(artist: &str, title: &str, first_hits: &[Value]) -> Option<String> {
    let query = library_title_query(artist, title)?;
    (SongIdentity::has_other_names(artist) || title_under_another_credit(artist, title, first_hits))
        .then_some(query)
}

/// Whether one of the songs is the title asked, by its own credit, but not the row's recording.
fn title_under_another_credit(artist: &str, title: &str, hits: &[Value]) -> bool {
    hits.iter().any(|hit| {
        let (hit_artist, hit_title) = (text(hit, "artist"), text(hit, "title"));
        !text(hit, "id").is_empty()
            && is_same_recording(hit_artist, title, hit_artist, hit_title)
            && !is_same_recording(artist, title, hit_artist, hit_title)
    })
}

/// The first of a library search's songs that is the row's own recording, by the rule radio
/// uses to play a library copy in place of an outside one. Hits without an id never count.
pub fn library_hit<'a>(artist: &str, title: &str, hits: &'a [Value]) -> Option<&'a Value> {
    hits.iter().find(|hit| {
        !text(hit, "id").is_empty()
            && is_same_recording(artist, title, text(hit, "artist"), text(hit, "title"))
    })
}

/// One library song stands for one row: the higher ranked one keeps it.
pub fn one_row_per_library_song(found: &mut [Option<Value>]) {
    let mut taken: Vec<String> = Vec::new();
    for slot in found.iter_mut() {
        let Some(id) = slot.as_ref().map(|hit| text(hit, "id").to_string()) else {
            continue;
        };
        if taken.contains(&id) {
            *slot = None;
        } else {
            taken.push(id);
        }
    }
}

fn text<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
#[path = "top_songs_tests.rs"]
mod tests;
