//! The release a heart downloads from, worked out without asking anything: the questions a heart
//! asks MusicBrainz and Deezer, and their answers read into compact rows. The callers ask the
//! questions and pass the rows in.
//!
//! [`RecordedAnswers`] is the one part that reads files: answers recorded for tests.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde_json::Value;

use crate::common::{SongIdentity, dotnet};
use crate::fingerprint::music_brainz_client::is_mbid;
use crate::models::domain::explicit_status;

/// The page size of the carriers question.
pub const CARRIERS_PAGE: u32 = 100;

/// The page size of the carriers question asked again after a timeout.
pub const CARRIERS_PAGE_AFTER_TIMEOUT: u32 = 25;

/// How many album search results the catalog album is looked for among: the search asks for
/// this many.
pub const ALBUM_SEARCH_RESULTS: usize = 5;

// ---- the questions --------------------------------------------------------------------------

/// The code lookup: every recording MusicBrainz files under one ISRC, with its artist credit.
/// None when the value is not an ISRC.
pub fn isrc_lookup_url(isrc: &str) -> Option<String> {
    let code = SongIdentity::normalize_isrc(isrc)?;
    Some(format!("isrc/{code}?inc=artist-credits&fmt=json"))
}

/// The barcode search for a catalog album's UPC. A 12-digit UPC is also asked with a leading 0,
/// and a 13-digit code that starts with 0 also without it, since MusicBrainz files either form;
/// the forms are sorted as text. None for a blank code.
pub fn barcode_search_url(upc: &str) -> Option<String> {
    let upc = upc.trim();
    if upc.is_empty() {
        return None;
    }
    let digits = upc.bytes().all(|b| b.is_ascii_digit());
    let mut forms = vec![upc.to_string()];
    if digits && upc.len() == 12 {
        forms.push(format!("0{upc}"));
    } else if digits && upc.len() == 13 && upc.starts_with('0') {
        forms.push(upc[1..].to_string());
    }
    forms.sort();
    let query = format!("barcode:({})", forms.join(" OR "));
    Some(format!(
        "release/?query={}&fmt=json&limit=25",
        dotnet::escape_data_string(&query)
    ))
}

/// The lookup of one release with its tracks, their recordings, its group and the artist
/// credits. None when the id is not a MusicBrainz id.
pub fn release_lookup_url(release_id: &str) -> Option<String> {
    let id = release_id.trim();
    is_mbid(id).then(|| {
        format!(
            "release/{}?inc=recordings+release-groups+artist-credits+isrcs&fmt=json",
            dotnet::escape_data_string(id)
        )
    })
}

/// One page of the official releases carrying a recording, with their groups and artist
/// credits: `limit` releases from `offset` on. None when the id is not a MusicBrainz id.
pub fn carriers_url(recording_id: &str, offset: u32, limit: u32) -> Option<String> {
    let id = recording_id.trim();
    is_mbid(id).then(|| {
        format!(
            "release?recording={}&inc=release-groups+artist-credits&status=official&limit={limit}&offset={offset}&fmt=json",
            dotnet::escape_data_string(id)
        )
    })
}

/// One Deezer album: its UPC, record type and track count.
pub fn deezer_album_url(album_id: &str) -> String {
    format!("album/{}", dotnet::escape_data_string(album_id.trim()))
}

/// One Deezer album's tracks.
pub fn deezer_album_tracks_url(album_id: &str) -> String {
    format!(
        "album/{}/tracks?limit=300",
        dotnet::escape_data_string(album_id.trim())
    )
}

/// Deezer's album search by artist and album title, as plain words (Deezer reads field
/// qualifiers as text). None when the album title is blank.
pub fn deezer_album_search_url(artist: &str, album: &str) -> Option<String> {
    if dotnet::is_blank(album) {
        return None;
    }
    let words: Vec<&str> = [artist, album]
        .into_iter()
        .filter(|part| !dotnet::is_blank(part))
        .map(str::trim)
        .collect();
    Some(format!(
        "search/album?q={}&limit={ALBUM_SEARCH_RESULTS}",
        dotnet::escape_data_string(&words.join(" "))
    ))
}

// ---- the rows -------------------------------------------------------------------------------

/// One artist of a MusicBrainz credit: the name the credit prints, and the artist it names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreditedArtist {
    pub name: String,
    pub artist_id: String,
    pub artist_name: String,
}

/// One recording the code lookup files under an ISRC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeRecording {
    pub id: String,
    pub title: String,
    /// Its length in milliseconds, None when MusicBrainz has none.
    pub length_ms: Option<u32>,
    /// Its disambiguation, lowercased, its spaces collapsed.
    pub note: String,
    pub isrcs: Vec<String>,
    /// Its first release date as MusicBrainz gives it ("1965", "1965-08-06"), "" when unknown.
    pub first_release_date: String,
    pub credits: Vec<CreditedArtist>,
}

/// One release of a barcode search, in search order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BarcodeHit {
    pub id: String,
    pub barcode: String,
}

/// What the pick reads of one release group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupFacts {
    pub id: String,
    pub title: String,
    /// "Album", "Single", "EP", ... or "" when MusicBrainz gives none.
    pub primary_type: String,
    pub secondary_types: Vec<String>,
}

/// One track of a looked-up release.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseTrack {
    pub position: Option<u32>,
    pub title: String,
    /// The recording's length in milliseconds, else the track's own; None when neither is known.
    pub length_ms: Option<u32>,
    pub recording_id: String,
    pub credits: Vec<CreditedArtist>,
}

/// One medium (disc) of a looked-up release.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseMedium {
    pub position: Option<u32>,
    pub tracks: Vec<ReleaseTrack>,
}

/// One release a barcode names, looked up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BarcodeRelease {
    pub id: String,
    pub title: String,
    pub date: String,
    pub credits: Vec<CreditedArtist>,
    pub group: GroupFacts,
    pub media: Vec<ReleaseMedium>,
}

/// One official release carrying a recording.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarrierRelease {
    pub id: String,
    pub title: String,
    pub date: String,
    pub country: String,
    pub group: GroupFacts,
    /// The artist its credit names first, "" when it names none.
    pub lead_artist_id: String,
}

/// One page of the carriers question.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarrierPage {
    /// How many releases carry the recording in all.
    pub release_count: u32,
    /// How many releases this page listed, those without a group included.
    pub listed: u32,
    /// The page's releases that belong to a group.
    pub releases: Vec<CarrierRelease>,
}

/// Every official release carrying one recording, its pages in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Carriers {
    pub recording_id: String,
    pub releases: Vec<CarrierRelease>,
}

/// One album of Deezer's album search, in search order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogAlbumHit {
    pub id: String,
    pub title: String,
    pub artist: String,
}

/// One Deezer album.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogAlbum {
    pub id: String,
    pub upc: String,
    pub record_type: String,
    pub track_count: Option<u32>,
}

/// One track of a Deezer album.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogTrack {
    pub id: String,
    pub title: String,
    pub disc: Option<u32>,
    pub position: Option<u32>,
    /// Its ISRC, normalized; "" when it has none.
    pub isrc: String,
    pub seconds: u32,
    /// Whether its words are explicit, as `explicit_status::from_catalog` reads it.
    pub explicit: Option<i32>,
}

// ---- the readers ----------------------------------------------------------------------------

/// The recordings of a code lookup's answer, DJ-mix segments left out: such a segment is the
/// song inside someone's mix, never the song itself.
pub fn read_code_recordings(answer: &Value) -> Vec<CodeRecording> {
    list(answer, "recordings")
        .iter()
        .filter(|recording| !is_dj_mix_segment(&text(recording, "disambiguation")))
        .map(|recording| CodeRecording {
            id: text(recording, "id"),
            title: text(recording, "title"),
            length_ms: length(recording, "length"),
            note: note_of(&text(recording, "disambiguation")),
            isrcs: list(recording, "isrcs")
                .iter()
                .filter_map(Value::as_str)
                .filter_map(SongIdentity::normalize_isrc)
                .collect(),
            first_release_date: text(recording, "first-release-date"),
            credits: credits(recording),
        })
        .filter(|recording| !recording.id.is_empty())
        .collect()
}

/// The releases of a barcode search's answer, in search order.
pub fn read_barcode_hits(answer: &Value) -> Vec<BarcodeHit> {
    list(answer, "releases")
        .iter()
        .map(|release| BarcodeHit {
            id: text(release, "id"),
            barcode: text(release, "barcode"),
        })
        .filter(|hit| !hit.id.is_empty())
        .collect()
}

/// A release lookup's answer, None when it names no release.
pub fn read_barcode_release(answer: &Value) -> Option<BarcodeRelease> {
    let id = text(answer, "id");
    if id.is_empty() {
        return None;
    }
    let media = list(answer, "media")
        .iter()
        .map(|medium| ReleaseMedium {
            position: whole(medium, "position"),
            tracks: list(medium, "tracks")
                .iter()
                .map(|track| {
                    let recording = track.get("recording").unwrap_or(&Value::Null);
                    ReleaseTrack {
                        position: whole(track, "position"),
                        title: text(track, "title"),
                        length_ms: length(recording, "length").or_else(|| length(track, "length")),
                        recording_id: text(recording, "id"),
                        credits: credits(track),
                    }
                })
                .collect(),
        })
        .collect();
    Some(BarcodeRelease {
        id,
        title: text(answer, "title"),
        date: text(answer, "date"),
        credits: credits(answer),
        group: group_facts(answer),
        media,
    })
}

/// One page of the carriers question's answer.
pub fn read_carrier_page(answer: &Value) -> CarrierPage {
    let listed = list(answer, "releases");
    CarrierPage {
        release_count: whole(answer, "release-count").unwrap_or(0),
        listed: u32::try_from(listed.len()).unwrap_or(u32::MAX),
        releases: listed
            .iter()
            .map(|release| CarrierRelease {
                id: text(release, "id"),
                title: text(release, "title"),
                date: text(release, "date"),
                country: text(release, "country"),
                group: group_facts(release),
                lead_artist_id: credits(release)
                    .into_iter()
                    .next()
                    .map(|credit| credit.artist_id)
                    .unwrap_or_default(),
            })
            .filter(|release| !release.group.id.is_empty())
            .collect(),
    }
}

/// The offset of the carriers page after this one, None when this one was the last.
pub fn next_carrier_offset(offset: u32, page: &CarrierPage) -> Option<u32> {
    let next = offset.saturating_add(page.listed);
    (page.listed > 0 && next < page.release_count).then_some(next)
}

/// The albums of Deezer's album search answer, in search order.
pub fn read_album_search(answer: &Value) -> Vec<CatalogAlbumHit> {
    list(answer, "data")
        .iter()
        .map(|album| CatalogAlbumHit {
            id: id_text(album, "id"),
            title: text(album, "title"),
            artist: album
                .get("artist")
                .map(|artist| text(artist, "name"))
                .unwrap_or_default(),
        })
        .filter(|hit| !hit.id.is_empty())
        .collect()
}

/// A Deezer album's answer, None when it names no album.
pub fn read_catalog_album(answer: &Value) -> Option<CatalogAlbum> {
    let id = id_text(answer, "id");
    if id.is_empty() {
        return None;
    }
    Some(CatalogAlbum {
        id,
        upc: text(answer, "upc"),
        record_type: text(answer, "record_type"),
        track_count: whole(answer, "nb_tracks").filter(|count| *count > 0),
    })
}

/// The tracks of a Deezer album's tracks answer, in order.
pub fn read_catalog_tracks(answer: &Value) -> Vec<CatalogTrack> {
    list(answer, "data")
        .iter()
        .map(|track| CatalogTrack {
            id: id_text(track, "id"),
            title: text(track, "title"),
            disc: whole(track, "disk_number"),
            position: whole(track, "track_position"),
            isrc: SongIdentity::normalize_isrc(&text(track, "isrc")).unwrap_or_default(),
            seconds: whole(track, "duration").unwrap_or(0),
            explicit: explicit_status::from_catalog(
                track
                    .get("explicit_content_lyrics")
                    .and_then(Value::as_i64)
                    .and_then(|said| i32::try_from(said).ok()),
                track.get("explicit_lyrics").and_then(Value::as_bool),
            ),
        })
        .collect()
}

fn text(value: &Value, field: &str) -> String {
    value.get(field).and_then(Value::as_str).unwrap_or("").to_string()
}

fn id_text(value: &Value, field: &str) -> String {
    match value.get(field) {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(id)) => id.trim().to_string(),
        _ => String::new(),
    }
}

fn whole(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
}

fn length(value: &Value, field: &str) -> Option<u32> {
    whole(value, field).filter(|ms| *ms > 0)
}

fn list<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn credits(value: &Value) -> Vec<CreditedArtist> {
    list(value, "artist-credit")
        .iter()
        .map(|part| {
            let artist = part.get("artist").unwrap_or(&Value::Null);
            CreditedArtist {
                name: text(part, "name"),
                artist_id: text(artist, "id"),
                artist_name: text(artist, "name"),
            }
        })
        .collect()
}

fn group_facts(release: &Value) -> GroupFacts {
    let group = release.get("release-group").unwrap_or(&Value::Null);
    GroupFacts {
        id: text(group, "id"),
        title: text(group, "title"),
        primary_type: text(group, "primary-type"),
        secondary_types: list(group, "secondary-types")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    }
}

/// A disambiguation lowercased, its spaces collapsed, so two spellings of one note are one.
fn note_of(disambiguation: &str) -> String {
    disambiguation
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a disambiguation marks one segment of a DJ mix ("part of ... DJ-mix", however the
/// hyphen or the space is written).
fn is_dj_mix_segment(disambiguation: &str) -> bool {
    let squeezed: String = disambiguation
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && !('\u{2010}'..='\u{2015}').contains(c))
        .collect();
    squeezed.contains("djmix")
}

// ---- recorded answers -----------------------------------------------------------------------

/// One line of a recorded folder's `urls.tsv`: the answer file's name and the exact question it
/// answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedQuestion {
    pub name: String,
    pub question: String,
}

/// How a recorded question was answered.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordedAnswer {
    /// The answer's body.
    Found(Value),
    /// The question was answered 404.
    NotFound,
}

/// Answers recorded from MusicBrainz and Deezer, served for tests by their exact question. A
/// folder's `urls.tsv` lists `<name>`, a tab and the question, one per line; the answer is
/// `<name>.json`, or `<name>.404` for a question answered 404. A name `albumsearch <row>` is
/// the file `deezer-albumsearch/<row>.json`, a `#` in the row written `_`. Every question served
/// is remembered, so a test can tell which recorded answers were never asked.
pub struct RecordedAnswers {
    folder: PathBuf,
    questions: Vec<RecordedQuestion>,
    by_question: HashMap<String, usize>,
    asked: Mutex<HashSet<String>>,
}

impl RecordedAnswers {
    /// The folder's recorded answers, from its `urls.tsv`.
    pub fn open(folder: &Path) -> std::io::Result<RecordedAnswers> {
        let listing = std::fs::read_to_string(folder.join("urls.tsv"))?;
        let questions: Vec<RecordedQuestion> = listing
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(name, question)| RecordedQuestion {
                name: name.to_string(),
                question: question.to_string(),
            })
            .collect();
        let mut by_question = HashMap::new();
        for (at, line) in questions.iter().enumerate() {
            by_question.entry(line.question.clone()).or_insert(at);
        }
        Ok(RecordedAnswers {
            folder: folder.to_path_buf(),
            questions,
            by_question,
            asked: Mutex::new(HashSet::new()),
        })
    }

    /// Every line of the folder's `urls.tsv`, in order.
    pub fn questions(&self) -> &[RecordedQuestion] {
        &self.questions
    }

    /// The recorded answer to `question`, None when the folder recorded no such question.
    pub fn get(&self, question: &str) -> Option<RecordedAnswer> {
        let line = &self.questions[*self.by_question.get(question)?];
        self.asked.lock().insert(question.to_string());
        let body = match line.name.strip_prefix("albumsearch ") {
            Some(row) => self
                .folder
                .join("deezer-albumsearch")
                .join(format!("{}.json", row.replace('#', "_"))),
            None => self.folder.join(format!("{}.json", line.name)),
        };
        if let Ok(text) = std::fs::read_to_string(&body) {
            let answer = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{} does not parse: {error}", body.display()));
            return Some(RecordedAnswer::Found(answer));
        }
        let missing = self.folder.join(format!("{}.404", line.name));
        assert!(
            missing.exists(),
            "{} lists {question} but holds neither {} nor {}",
            self.folder.display(),
            body.display(),
            missing.display()
        );
        Some(RecordedAnswer::NotFound)
    }

    /// The recorded answer to a question the folder must hold: any other question fails.
    pub fn answer(&self, question: &str) -> RecordedAnswer {
        self.get(question)
            .unwrap_or_else(|| panic!("{} recorded no answer to {question}", self.folder.display()))
    }

    /// The recorded questions nothing has asked yet, in `urls.tsv` order.
    pub fn unasked(&self) -> Vec<&RecordedQuestion> {
        let asked = self.asked.lock();
        self.questions
            .iter()
            .filter(|line| !asked.contains(&line.question))
            .collect()
    }
}

#[cfg(test)]
#[path = "heart_release_tests.rs"]
mod tests;
