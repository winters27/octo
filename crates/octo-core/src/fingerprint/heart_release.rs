//! The release a heart downloads from, worked out without asking anything: the questions a heart
//! asks MusicBrainz and Deezer, their answers read into compact rows, the recordings the person
//! heard, the release they listened from, and the release groups Lidarr is asked for, in order.
//! The callers ask the questions and pass the rows in.
//!
//! [`RecordedAnswers`] is the one part that reads files: answers recorded for tests.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value;

use crate::common::{SongIdentity, dotnet};
use crate::fingerprint::music_brainz_client::{is_earlier_date, is_mbid};
use crate::models::domain::explicit_status;

/// The page size of the carriers question.
pub const CARRIERS_PAGE: u32 = 100;

/// The page size of the carriers question asked again after a timeout.
pub const CARRIERS_PAGE_AFTER_TIMEOUT: u32 = 25;

/// How many album search results the catalog album is looked for among: the search asks for
/// this many.
pub const ALBUM_SEARCH_RESULTS: usize = 5;

/// How many barcode search results, in search order, are looked up.
pub const BARCODE_RESULTS_LOOKED_UP: usize = 4;

/// The most recordings the heard recording set holds.
pub const MOST_HEARD_RECORDINGS: usize = 6;

/// The most release groups the walk takes after the listened-from one.
pub const MOST_PICKS_AFTER_LISTENED: usize = 3;

/// MusicBrainz's Various Artists: the credit of a compilation, never the heard artist.
pub const VARIOUS_ARTISTS_ID: &str = "89ad4ac3-39f7-470e-963a-56509c546377";

/// The countries whose artists count a release issued for all of Europe ("XE") as issued at home.
const EUROPE: &[&str] = &[
    "AD", "AL", "AT", "BA", "BE", "BG", "BY", "CH", "CY", "CZ", "DE", "DK", "EE", "ES", "FI", "FO", "FR",
    "GB", "GG", "GI", "GR", "HR", "HU", "IE", "IM", "IS", "IT", "JE", "LI", "LT", "LU", "LV", "MC", "MD",
    "ME", "MK", "MT", "NL", "NO", "PL", "PT", "RO", "RS", "RU", "SE", "SI", "SK", "SM", "UA", "VA", "XK",
];

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

// ---- the song -------------------------------------------------------------------------------

/// What a heart knows of the song: the catalog row it was hearted from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeartSong {
    pub artist: String,
    pub title: String,
    /// The album title, "" when the heart has none.
    pub album: String,
    pub disc: Option<u32>,
    pub position: Option<u32>,
    /// How many tracks the catalog album has, None when unknown.
    pub total_tracks: Option<u32>,
    /// The ISRC, "" when the routing has none.
    pub isrc: String,
    /// The length in seconds, 0 when unknown.
    pub seconds: u32,
    /// Whether its words are explicit, as `explicit_status::from_catalog` reads it.
    pub explicit: Option<i32>,
}

impl HeartSong {
    fn code(&self) -> Option<String> {
        SongIdentity::normalize_isrc(&self.isrc)
    }
}

// ---- the catalog album from its title -------------------------------------------------------

/// The album search results the catalog album may be, in search order: the first
/// [`ALBUM_SEARCH_RESULTS`], and of those an album another artist leads only when the song has
/// an ISRC to prove it by.
pub fn catalog_album_candidates<'a>(
    song: &HeartSong,
    hits: &'a [CatalogAlbumHit],
) -> Vec<&'a CatalogAlbumHit> {
    let has_code = song.code().is_some();
    let artist = SongIdentity::key(&song.artist);
    hits.iter()
        .take(ALBUM_SEARCH_RESULTS)
        .filter(|hit| has_code || SongIdentity::key(&hit.artist) == artist)
        .collect()
}

/// Whether a searched album is the song's catalog album. When both track counts are known they
/// are equal, and the track at the song's disc and position carries its ISRC; with no ISRC, that
/// track has the song's title (tails and all), fits its length, and has its explicit value when
/// both say one. `album` is None when the album itself could not be read.
pub fn catalog_album_fits(song: &HeartSong, album: Option<&CatalogAlbum>, tracks: &[CatalogTrack]) -> bool {
    if let (Some(total), Some(count)) = (song.total_tracks, album.and_then(|album| album.track_count))
        && total != count
    {
        return false;
    }
    let (Some(disc), Some(position)) = (song.disc, song.position) else {
        return false;
    };
    let code = song.code();
    let title = SongIdentity::key(&song.title);
    tracks
        .iter()
        .filter(|track| track.disc == Some(disc) && track.position == Some(position))
        .any(|track| match &code {
            Some(code) => track.isrc == *code,
            None => {
                let same_edit = match (song.explicit, track.explicit) {
                    (Some(wanted), Some(said)) => wanted == said,
                    _ => true,
                };
                same_edit
                    && SongIdentity::key(&track.title) == title
                    && fits(track.seconds.checked_mul(1000), song.seconds)
            }
        })
}

// ---- the release listened from --------------------------------------------------------------

/// The releases of a barcode search worth looking up: of the first
/// [`BARCODE_RESULTS_LOOKED_UP`] in search order, those whose barcode is the UPC as a number.
pub fn releases_to_look_up(hits: &[BarcodeHit], upc: &str) -> Vec<String> {
    let Some(wanted) = as_number(upc) else {
        return Vec::new();
    };
    hits.iter()
        .take(BARCODE_RESULTS_LOOKED_UP)
        .filter(|hit| as_number(&hit.barcode) == Some(wanted))
        .map(|hit| hit.id.clone())
        .collect()
}

/// The release the person listened from, among the looked-up releases a barcode names: the one
/// carrying a recording the code lookup lists, else the one whose track at the song's disc and
/// position fits its length; then the earliest date; then the id.
pub fn listened_release<'a>(
    song: &HeartSong,
    releases: &'a [BarcodeRelease],
    code_recordings: &[CodeRecording],
) -> Option<&'a BarcodeRelease> {
    let code_ids: HashSet<&str> = code_recordings
        .iter()
        .map(|recording| recording.id.as_str())
        .collect();
    releases.iter().min_by_key(|release| {
        let carries_code = release
            .media
            .iter()
            .flat_map(|medium| &medium.tracks)
            .any(|track| code_ids.contains(track.recording_id.as_str()));
        let position_fits = track_at(release, song.disc, song.position)
            .is_some_and(|track| fits(track.length_ms, song.seconds));
        let date = if release.date.is_empty() {
            "9999"
        } else {
            release.date.as_str()
        };
        (!carries_code, !position_fits, date, release.id.as_str())
    })
}

// ---- the recordings the person heard --------------------------------------------------------

/// The recordings the person heard and where they listened from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeardIdentity {
    /// The recordings the person heard: the listened release's own track first, then the ISRC's;
    /// at most [`MOST_HEARD_RECORDINGS`]. Empty when the song gives no identity.
    pub recordings: Vec<String>,
    /// The same recordings before the cap of [`MOST_HEARD_RECORDINGS`].
    pub recordings_uncapped: Vec<String>,
    /// The listened release's track at the song's disc and position, when it is the song.
    pub heard_track: Option<String>,
    /// The release the person listened from.
    pub listened_release: Option<String>,
    /// Its release group: the first pick, known without the carriers. None when the recordings
    /// are not known.
    pub listened_group: Option<String>,
    /// The artist whose own releases come first, "" when none is known.
    pub artist_id: String,
    /// Every artist credited on the kept ISRC recordings and the heard track, in order.
    pub artist_ids: Vec<String>,
}

/// The recordings the person heard. The code lookup's recordings of the song's ISRC that are
/// credited to the heard artist (by name, or by an artist the listened release or its heard
/// track credits, never Various Artists) and fit the song's length; all of them when the song or
/// none of them has a length. Of several, none first released after the code's year (all when
/// every one is). When their notes still differ the code names more than one mix: the heard
/// track's recording when it is one of them, else those first released in the code's year, and
/// when the notes still differ the code gives nothing. The heard track comes first.
pub fn identify(
    song: &HeartSong,
    code_recordings: &[CodeRecording],
    listened: Option<&BarcodeRelease>,
) -> HeardIdentity {
    let artist_key = SongIdentity::key(&song.artist);
    let heard = listened.and_then(|release| heard_track(song, release));
    let heard_id = heard
        .map(|track| track.recording_id.clone())
        .filter(|id| !id.is_empty());

    let listened_artists: HashSet<&str> = listened
        .into_iter()
        .flat_map(|release| &release.credits)
        .chain(heard.into_iter().flat_map(|track| &track.credits))
        .map(|credit| credit.artist_id.as_str())
        .filter(|id| !id.is_empty() && *id != VARIOUS_ARTISTS_ID)
        .collect();
    let credited: Vec<&CodeRecording> = code_recordings
        .iter()
        .filter(|recording| {
            recording.credits.iter().any(|credit| {
                names_artist(credit, &artist_key) || listened_artists.contains(credit.artist_id.as_str())
            })
        })
        .collect();
    let with_length: Vec<&CodeRecording> = credited
        .iter()
        .copied()
        .filter(|recording| recording.length_ms.is_some())
        .collect();
    let mut kept: Vec<&CodeRecording> = if song.seconds > 0 && !with_length.is_empty() {
        with_length
            .into_iter()
            .filter(|recording| fits(recording.length_ms, song.seconds))
            .collect()
    } else {
        credited
    };

    let year = song.code().as_deref().and_then(code_year);
    if kept.len() > 1
        && let Some(year) = year
    {
        let before: Vec<&CodeRecording> = kept
            .iter()
            .copied()
            .filter(|recording| year_of(&recording.first_release_date).is_none_or(|first| first <= year))
            .collect();
        if !before.is_empty() {
            kept = before;
        }
    }
    if kept.len() > 1 && notes_differ(&kept) {
        if let Some(id) = &heard_id
            && kept.iter().any(|recording| recording.id == *id)
        {
            kept.retain(|recording| recording.id == *id);
        } else if let Some(year) = year {
            let same_year: Vec<&CodeRecording> = kept
                .iter()
                .copied()
                .filter(|recording| year_of(&recording.first_release_date) == Some(year))
                .collect();
            if !same_year.is_empty() {
                kept = same_year;
            }
        }
        if kept.len() > 1 && notes_differ(&kept) {
            kept.clear();
        }
    }

    let mut recordings_uncapped: Vec<String> = heard_id.iter().cloned().collect();
    recordings_uncapped.extend(
        kept.iter()
            .filter(|recording| heard_id.as_ref() != Some(&recording.id))
            .map(|recording| recording.id.clone()),
    );
    let recordings: Vec<String> = recordings_uncapped
        .iter()
        .take(MOST_HEARD_RECORDINGS)
        .cloned()
        .collect();

    let mut artist_id = kept
        .iter()
        .flat_map(|recording| &recording.credits)
        .chain(heard.into_iter().flat_map(|track| &track.credits))
        .chain(listened.into_iter().flat_map(|release| &release.credits))
        .filter(|credit| names_artist(credit, &artist_key))
        .map(|credit| credit.artist_id.as_str())
        .find(|id| !id.is_empty() && *id != VARIOUS_ARTISTS_ID)
        .or_else(|| {
            kept.first()
                .and_then(|recording| recording.credits.first())
                .map(|credit| credit.artist_id.as_str())
        })
        .unwrap_or_default()
        .to_string();
    if artist_id == VARIOUS_ARTISTS_ID {
        artist_id.clear();
    }

    let mut artist_ids: Vec<String> = Vec::new();
    for credit in kept
        .iter()
        .flat_map(|recording| &recording.credits)
        .chain(heard.into_iter().flat_map(|track| &track.credits))
    {
        if !credit.artist_id.is_empty() && !artist_ids.contains(&credit.artist_id) {
            artist_ids.push(credit.artist_id.clone());
        }
    }

    let listened_group = listened
        .filter(|_| !recordings.is_empty())
        .map(|release| release.group.id.clone())
        .filter(|id| !id.is_empty());
    HeardIdentity {
        recordings,
        recordings_uncapped,
        heard_track: heard_id,
        listened_release: listened.map(|release| release.id.clone()),
        listened_group,
        artist_id,
        artist_ids,
    }
}

/// The listened release's track at the song's disc and position, when its title holds the
/// song's core title and (both lengths known) its length fits, or when its length fits and both
/// catalogs count the album's tracks alike.
fn heard_track<'a>(song: &HeartSong, release: &'a BarcodeRelease) -> Option<&'a ReleaseTrack> {
    let track = track_at(release, song.disc, song.position)?;
    let length_ok = track.length_ms.is_none() || song.seconds == 0 || fits(track.length_ms, song.seconds);
    let titled = SongIdentity::key(&track.title).contains(&core_key(&song.title));
    let track_count: usize = release.media.iter().map(|medium| medium.tracks.len()).sum();
    let same_count = song
        .total_tracks
        .is_some_and(|total| usize::try_from(total).is_ok_and(|total| total == track_count));
    ((titled && length_ok) || (same_count && fits(track.length_ms, song.seconds))).then_some(track)
}

/// The track at a disc and position: the first medium at that position holding a track at it.
fn track_at(release: &BarcodeRelease, disc: Option<u32>, position: Option<u32>) -> Option<&ReleaseTrack> {
    let (disc, position) = (disc?, position?);
    release
        .media
        .iter()
        .filter(|medium| medium.position == Some(disc))
        .flat_map(|medium| &medium.tracks)
        .find(|track| track.position == Some(position))
}

fn names_artist(credit: &CreditedArtist, artist_key: &str) -> bool {
    SongIdentity::key(&credit.name) == artist_key || SongIdentity::key(&credit.artist_name) == artist_key
}

fn notes_differ(recordings: &[&CodeRecording]) -> bool {
    recordings
        .iter()
        .any(|recording| recording.note != recordings[0].note)
}

/// Whether a length in milliseconds is a song's length in seconds: within 7 s or 4%, whichever
/// is more. False when either is unknown.
fn fits(length_ms: Option<u32>, seconds: u32) -> bool {
    let Some(ms) = length_ms.filter(|ms| *ms > 0) else {
        return false;
    };
    if seconds == 0 {
        return false;
    }
    let seconds = f64::from(seconds);
    (f64::from(ms) / 1000.0 - seconds).abs() <= f64::max(7.0, seconds * 0.04)
}

/// The year an ISRC was assigned, from its characters 6 and 7 (40 and up are 19xx).
fn code_year(code: &str) -> Option<i32> {
    let digits: String = code.trim().chars().skip(5).take(2).collect();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let short: i32 = digits.parse().ok()?;
    Some(if short >= 40 { 1900 + short } else { 2000 + short })
}

/// The year a date starts with, None when it does not start with one.
fn year_of(date: &str) -> Option<i32> {
    let digits = year_prefix(date);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn year_prefix(date: &str) -> &str {
    date.char_indices().nth(4).map_or(date, |(end, _)| &date[..end])
}

/// A barcode as a number, so "0602547673503" and "602547673503" are one code.
fn as_number(code: &str) -> Option<u128> {
    code.trim().parse().ok()
}

static BRACKETED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[(\[][^)\]]*[)\]]").expect("the bracket pattern parses"));
static DASH_TAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+-\s+.*$").expect("the dash tail pattern parses"));

/// A title's core: without its bracketed parts or a " - " tail, as [`SongIdentity::key`].
fn core_key(title: &str) -> String {
    let plain = BRACKETED.replace_all(title, " ");
    SongIdentity::key(&DASH_TAIL.replace(&plain, ""))
}

// ---- the order and the walk -----------------------------------------------------------------

/// Every release group carrying the heard recordings, in the order a listener expects them:
/// the listened-from group first; then own albums issued at home (Album with no secondary type,
/// Soundtrack or Live), own singles and EPs named for the song issued at home, own albums first
/// issued elsewhere, own singles and EPs named for the song first issued elsewhere, other own
/// singles and EPs, own compilations, other own groups, and groups other artists lead. A group
/// is issued at home when a release of it came out in the artist's country (or worldwide, or
/// across Europe for a European artist) in the year the group first carried the recording.
/// Within each kind, groups with a release in or after the ISRC's year come first, then the
/// earliest release date (a more precise date of the same day, month or year first), then the
/// group id. `home_country` is the artist's country, "" when unknown; `carriers` holds every
/// official release carrying each recording of `identity.recordings` (others are passed over).
pub fn order(
    song: &HeartSong,
    identity: &HeardIdentity,
    home_country: &str,
    carriers: &[Carriers],
) -> Vec<String> {
    let ranked = rank(song, identity, home_country, carriers);
    ranked
        .listened
        .iter()
        .cloned()
        .chain(
            ranked
                .groups
                .into_iter()
                .filter(|(id, _)| ranked.listened.as_ref() != Some(id))
                .map(|(id, _)| id),
        )
        .collect()
}

/// The release groups Lidarr is asked for, in order: the listened-from group, then at most
/// [`MOST_PICKS_AFTER_LISTENED`] groups the artist leads; groups other artists lead only when
/// the artist leads none but the listened-from one. Arguments as [`order`].
pub fn walk(
    song: &HeartSong,
    identity: &HeardIdentity,
    home_country: &str,
    carriers: &[Carriers],
) -> Vec<String> {
    let ranked = rank(song, identity, home_country, carriers);
    let rest: Vec<(String, bool)> = ranked
        .groups
        .into_iter()
        .filter(|(id, _)| ranked.listened.as_ref() != Some(id))
        .collect();
    let any_own = rest.iter().any(|(_, own)| *own);
    let mut picks: Vec<String> = ranked.listened.into_iter().collect();
    picks.extend(
        rest.into_iter()
            .filter(|(_, own)| *own == any_own)
            .take(MOST_PICKS_AFTER_LISTENED)
            .map(|(id, _)| id),
    );
    picks
}

/// The listened-from group and every carrier group in order, each with whether the artist
/// leads it.
struct Ranked {
    listened: Option<String>,
    groups: Vec<(String, bool)>,
}

/// One release group as the carriers describe it.
struct CarrierGroup<'a> {
    facts: &'a GroupFacts,
    own: bool,
    /// The earliest release date, "" when none is known.
    date: &'a str,
    /// The latest release year, 0 when none is known.
    latest_year: i32,
    /// The years a release of the group came out at home.
    home_years: HashSet<&'a str>,
    release_titles: Vec<&'a str>,
}

fn rank(song: &HeartSong, identity: &HeardIdentity, home_country: &str, carriers: &[Carriers]) -> Ranked {
    // Recordings in id order, so the release a group is first seen on does not depend on the
    // order the caller passes them in.
    let mut chosen: Vec<&Carriers> = carriers
        .iter()
        .filter(|carried| identity.recordings.contains(&carried.recording_id))
        .collect();
    chosen.sort_by_key(|carried| carried.recording_id.as_str());

    let mut groups: Vec<CarrierGroup> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    for release in chosen.iter().flat_map(|carried| &carried.releases) {
        let index = *at.entry(release.group.id.as_str()).or_insert_with(|| {
            groups.push(CarrierGroup {
                facts: &release.group,
                own: release.lead_artist_id == identity.artist_id,
                date: "",
                latest_year: 0,
                home_years: HashSet::new(),
                release_titles: Vec::new(),
            });
            groups.len() - 1
        });
        let group = &mut groups[index];
        if let Some(year) = year_of(&release.date) {
            group.latest_year = group.latest_year.max(year);
        }
        group.release_titles.push(release.title.as_str());
        if !release.date.is_empty() {
            if group.date.is_empty() || is_earlier_date(&release.date, group.date) {
                group.date = release.date.as_str();
            }
            if at_home(&release.country, home_country) {
                group.home_years.insert(year_prefix(&release.date));
            }
        }
    }

    let listened = identity
        .listened_group
        .clone()
        .or_else(|| titled_group(song, identity, &groups));

    let core = core_key(&song.title);
    let year = song.code().as_deref().and_then(code_year);
    let mut placed: Vec<Placed> = groups
        .iter()
        .map(|group| Placed {
            first: u8::from(listened.as_deref() != Some(group.facts.id.as_str())),
            kind: kind(group, &core) + u8::from(year.is_some_and(|year| group.latest_year < year)),
            date: sort_date(group),
            id: &group.facts.id,
            own: group.own,
        })
        .collect();
    placed.sort_by_key(|group| (group.first, group.kind, group.date, group.id));
    // Dates of one kind compare by the part both have, the more precise date first, which text
    // order does not give: each group moves ahead of the ones it is earlier than.
    let mut ordered: Vec<Placed> = Vec::with_capacity(placed.len());
    for group in placed {
        let mut at = ordered.len();
        while at > 0 && group.goes_ahead_of(&ordered[at - 1]) {
            at -= 1;
        }
        ordered.insert(at, group);
    }
    Ranked {
        listened,
        groups: ordered
            .into_iter()
            .map(|group| (group.id.to_string(), group.own))
            .collect(),
    }
}

/// One carrier group's place in the order.
struct Placed<'a> {
    /// 0 for the listened-from group, 1 for every other.
    first: u8,
    kind: u8,
    /// The earliest release date, "9999" when none is known.
    date: &'a str,
    id: &'a str,
    own: bool,
}

impl Placed<'_> {
    /// Whether this group goes ahead of `other`, which text order put first: the same kind, and
    /// an earlier date by the part both have.
    fn goes_ahead_of(&self, other: &Placed) -> bool {
        (self.first, self.kind) == (other.first, other.kind) && is_earlier_date(self.date, other.date)
    }
}

/// A group's earliest release date, "9999" when none is known, so undated groups sort last.
fn sort_date<'a>(group: &CarrierGroup<'a>) -> &'a str {
    if group.date.is_empty() { "9999" } else { group.date }
}

/// Without a listened-from release: the carrier group a release of which carries the album's
/// title, brackets and " - " tails aside; the earliest, then the group id.
fn titled_group(song: &HeartSong, identity: &HeardIdentity, groups: &[CarrierGroup]) -> Option<String> {
    let wanted = core_key(&song.album);
    if wanted.is_empty() || identity.recordings.is_empty() {
        return None;
    }
    let mut named: Vec<&CarrierGroup> = groups
        .iter()
        .filter(|group| group.release_titles.iter().any(|title| core_key(title) == wanted))
        .collect();
    named.sort_by_key(|group| (sort_date(group), group.facts.id.as_str()));
    let mut best: Option<&CarrierGroup> = None;
    for group in named {
        let better = match best {
            None => true,
            Some(known) => {
                !group.date.is_empty() && (known.date.is_empty() || is_earlier_date(group.date, known.date))
            }
        };
        if better {
            best = Some(group);
        }
    }
    best.map(|group| group.facts.id.clone())
}

/// A group's kind, four steps apart so the step for an older group fits between two kinds.
fn kind(group: &CarrierGroup, core: &str) -> u8 {
    let facts = group.facts;
    let primary = facts.primary_type.as_str();
    let plain = facts.secondary_types.is_empty();
    let home = !group.date.is_empty() && group.home_years.contains(year_prefix(group.date));
    if !group.own {
        28
    } else if primary == "Album"
        && facts
            .secondary_types
            .iter()
            .all(|secondary| secondary == "Soundtrack" || secondary == "Live")
    {
        if home { 4 } else { 12 }
    } else if (primary == "Single" || primary == "EP")
        && plain
        && SongIdentity::key(&facts.title).contains(core)
    {
        if home { 8 } else { 16 }
    } else if (primary == "Single" || primary == "EP") && plain {
        20
    } else if primary == "Album"
        && facts
            .secondary_types
            .iter()
            .any(|secondary| secondary == "Compilation")
    {
        24
    } else {
        26
    }
}

/// Whether a release's country is the artist's home: the artist's own country, worldwide, or
/// all of Europe for a European artist.
fn at_home(country: &str, home: &str) -> bool {
    !home.is_empty()
        && !country.is_empty()
        && (country == home || country == "XW" || (country == "XE" && EUROPE.contains(&home)))
}

#[cfg(test)]
#[path = "heart_release_tests.rs"]
mod tests;
