//! The data half of the AcoustID client: the records an AcoustID lookup answers with
//! (`VerificationResult` and the release chooser hold them), and the pure reading of its answer
//! ([`parse_lookup`]) and building of its forms. The HTTP client and its rate limiter
//! are in `octo::services::fingerprint`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::dotnet;
use crate::common::octo_user_agent;
use crate::fingerprint::music_brainz_client;
use crate::json::element::{
    ElementResult, array_length, enumerate_array, get_double, get_int32, get_string, str_prop, string_prop,
    try_get_int32, try_get_property,
};

/// One credited artist and the text MusicBrainz joins it to the next one with.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AcoustIdCredit {
    pub name: String,
    pub artist_id: Option<String>,
    pub join_phrase: String,
}

impl AcoustIdCredit {
    pub fn new(name: impl Into<String>, artist_id: Option<&str>, join_phrase: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            artist_id: artist_id.map(str::to_string),
            join_phrase: join_phrase.into(),
        }
    }
}

/// The release a recording was matched on. It supplies what names the album, numbers the
/// track and finds the cover, and it is chosen per recording by `pick_release`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AcoustIdRelease {
    pub release_id: Option<String>,
    pub release_group_id: Option<String>,
    pub title: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<i32>,
    pub track_count: Option<i32>,
    pub disc_number: Option<i32>,
    pub album_artist: Option<String>,
    pub is_compilation: bool,

    /// The rest of what the service says about a release, kept for every release of
    /// every group so the chooser can weigh them: the group's title and kind, the release's own
    /// title, date and country, the track's own id, and the album artists' ids.
    pub group_title: Option<String>,
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
    pub country: Option<String>,
    pub date: Option<String>,
    pub release_track_id: Option<String>,
    pub disc_count: Option<i32>,
    pub album_artist_ids: Vec<String>,
}

/// One recording AcoustID matched, with the MusicBrainz fields that come back in the same
/// lookup. There is no separate MusicBrainz client on purpose: AcoustID's metadata IS
/// MusicBrainz data, and asking for it via meta= costs nothing extra on a call already
/// being made and already inside a rate budget.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AcoustIdRecording {
    pub recording_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album_title: Option<String>,
    pub year: Option<i32>,

    pub credits: Vec<AcoustIdCredit>,
    pub release: Option<AcoustIdRelease>,
    pub duration_seconds: Option<i32>,
    /// The same length to the millisecond, as the music database lists it. Takes of one song
    /// that share a fingerprint are often told apart only here.
    pub duration_ms: Option<i64>,

    /// Every release of every group the recording is on, bounded, for the chooser.
    /// [`release`](Self::release) stays the one pick the old fields are read from.
    pub releases: Vec<AcoustIdRelease>,

    /// The codes the music database lists for the recording, normalized.
    pub isrcs: Vec<String>,

    /// How many submissions tie the fingerprint to this recording.
    pub sources: i32,
}

impl AcoustIdRecording {
    /// The recording's core fields; everything else starts empty.
    pub fn new<S: Into<String>>(
        recording_id: impl Into<String>,
        title: impl Into<String>,
        artists: impl IntoIterator<Item = S>,
        album_title: Option<&str>,
        year: Option<i32>,
    ) -> Self {
        Self {
            recording_id: recording_id.into(),
            title: title.into(),
            artists: artists.into_iter().map(Into::into).collect(),
            album_title: album_title.map(str::to_string),
            year,
            ..Default::default()
        }
    }

    /// The credit as MusicBrainz prints it, join phrases and all. Never a bare comma join:
    /// Navidrome does not split artists on commas, so "Bizarrap, Rauw Alejandro" became one
    /// artist and one folder that neither of them owns (#49).
    pub fn artist_credit(&self) -> String {
        if self.credits.is_empty() {
            Self::join_names(&self.artists)
        } else {
            Self::join_credits(&self.credits)
        }
    }

    /// The first credited artist: the one a folder is named after.
    pub fn primary_artist(&self) -> Option<&str> {
        match self.credits.first() {
            Some(credit) => Some(&credit.name),
            None => self.artists.first().map(String::as_str),
        }
    }

    pub fn join_credits(credits: &[AcoustIdCredit]) -> String {
        let mut builder = String::new();
        for (i, credit) in credits.iter().enumerate() {
            builder.push_str(&credit.name);
            if i == credits.len() - 1 {
                break;
            }
            // compress drops a join phrase the parent level already carries, so a missing one
            // is read the way MusicBrainz most often prints it.
            let join = credit.join_phrase.as_str();
            builder.push_str(if !join.is_empty() {
                join
            } else if i == credits.len() - 2 {
                " & "
            } else {
                ", "
            });
        }
        builder
    }

    pub fn join_names(names: &[String]) -> String {
        match names {
            [] => String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} & {last}", rest.join(", ")),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcoustIdResult {
    pub score: f64,
    pub recordings: Vec<AcoustIdRecording>,
    /// The service's own id for the fingerprint, written to a confirmed file.
    pub id: Option<String>,
}

impl AcoustIdResult {
    pub fn new(score: f64, recordings: Vec<AcoustIdRecording>) -> Self {
        Self {
            score,
            recordings,
            id: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcoustIdLookup {
    pub is_ok: bool,
    pub error: Option<String>,
    pub results: Vec<AcoustIdResult>,
}

impl AcoustIdLookup {
    pub fn new(is_ok: bool, error: Option<&str>, results: Vec<AcoustIdResult>) -> Self {
        Self {
            is_ok,
            error: error.map(str::to_string),
            results,
        }
    }
}

/// One artist an answer credits on a recording, with its MusicBrainz id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnswerArtist {
    pub id: Option<String>,
    pub name: String,
}

/// One recording a result of AcoustID's whole answer links.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnswerRecording {
    pub id: String,
    pub title: String,
    pub artists: Vec<AnswerArtist>,
    /// The recording's length in seconds.
    pub duration_seconds: Option<f64>,
    /// The ISRCs AcoustID copies from MusicBrainz for the recording, normalized.
    pub isrcs: Vec<String>,
    /// How many submissions tie the fingerprint to the recording.
    pub sources: i64,
}

/// One result of AcoustID's whole answer: a fingerprint, its score and every recording it links.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnswerResult {
    /// AcoustID's own id for the fingerprint.
    pub id: Option<String>,
    pub score: f64,
    pub recordings: Vec<AnswerRecording>,
}

/// AcoustID's whole lookup answer with `status: ok`: every result and every recording, with no cut.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcoustIdAnswer {
    pub results: Vec<AnswerResult>,
}

/// An answer whose status is not `ok`: AcoustID's error code and message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnswerError {
    pub code: Option<i64>,
    pub message: Option<String>,
}

/// What one AcoustID lookup came to, as the heart gate reads it.
#[derive(Debug, Clone, PartialEq)]
pub enum AcoustIdOutcome {
    /// AcoustID answered `status: ok`.
    Answered(AcoustIdAnswer),
    /// AcoustID has not answered yet: a 5xx, a 429, codes 5, 13 or 14, a timeout, a network error
    /// or a body that cannot be read. Asked again later.
    Busy,
    /// AcoustID could not check this one fingerprint by its sound (codes 1, 2, 3, 7, 8, 9, 10, 11
    /// and 19, any code it does not list, or none). Never sent again.
    Unreadable { code: Option<i64> },
    /// AcoustID is off: the `off` setting, or every key Octo has was rejected.
    Off,
}

impl AcoustIdRelease {
    /// The release's core fields; everything else starts empty.
    pub fn new(
        release_id: Option<String>,
        release_group_id: Option<String>,
        title: Option<String>,
        year: Option<i32>,
        track_number: Option<i32>,
        track_count: Option<i32>,
        disc_number: Option<i32>,
        album_artist: Option<String>,
        is_compilation: bool,
    ) -> Self {
        Self {
            release_id,
            release_group_id,
            title,
            year,
            track_number,
            track_count,
            disc_number,
            album_artist,
            is_compilation,
            ..Default::default()
        }
    }
}

/// One confirmed fingerprint to send back, with the recording a person vouched for.
#[derive(Debug, Clone, PartialEq)]
pub struct AcoustIdSubmission {
    pub fingerprint: String,
    pub duration_seconds: i32,
    pub recording_id: String,
    pub file_format: Option<String>,
}

/// What to ask AcoustID to return: recordings gives title and artists, releasegroups the
/// canonical album title, releases the year, and compress asks for a gzipped body (which is
/// why the HTTP client decompresses responses).
///
/// SPACE separated, and that is not cosmetic. The form encoding ([`encode_form`]) writes a
/// literal '+' as %2B, so writing these joined by '+' sends AcoustID one unknown token rather
/// than four fields. It answers 200 with a perfectly good score and NO metadata at all, every
/// result then has zero recordings, and the verdict is permanently Inconclusive: the feature
/// accepts every file forever while looking like it is working. Verified against the live API on
/// 2026-09-18, one track, both spellings.
///
/// tracks adds each release's mediums and the track's position on them, which is what numbers
/// a file named from its match (#48). It only takes effect beside releases.
///
/// isrcs and sources cost nothing on the same call: the recording's codes settle agreement
/// with a request's code without a second service, and the submission count breaks ties.
pub const META_FIELDS: &str = "recordings releasegroups releases tracks compress isrcs sources";

/// A recording on a long-lived catalog can carry hundreds of release groups, and one
/// fingerprint can match several ids. Both are bounded so a pathological response cannot
/// turn a download into a CPU burn.
const MAX_RESULTS: usize = 10;
const MAX_RECORDINGS: usize = 25;
const MAX_RELEASE_GROUPS: usize = 50;
const MAX_RELEASES_PER_GROUP: usize = 10;

/// The lookup form, its fields in a fixed order.
pub fn build_lookup_form(api_key: &str, fingerprint: &str, duration_seconds: i32) -> Vec<(String, String)> {
    vec![
        ("client".to_string(), api_key.to_string()),
        ("format".to_string(), "json".to_string()),
        ("duration".to_string(), duration_seconds.to_string()),
        ("fingerprint".to_string(), fingerprint.to_string()),
        ("meta".to_string(), META_FIELDS.to_string()),
    ]
}

pub fn build_submit_form(
    client_key: &str,
    user_key: &str,
    items: &[AcoustIdSubmission],
) -> Vec<(String, String)> {
    let mut form = vec![
        ("client".to_string(), client_key.to_string()),
        ("user".to_string(), user_key.to_string()),
        ("format".to_string(), "json".to_string()),
        (
            "clientversion".to_string(),
            format!("octo-{}", octo_user_agent::version()),
        ),
    ];
    for (i, item) in items.iter().enumerate() {
        form.push((format!("duration.{i}"), item.duration_seconds.to_string()));
        form.push((format!("fingerprint.{i}"), item.fingerprint.clone()));
        form.push((format!("mbid.{i}"), item.recording_id.clone()));
        if let Some(format) = item.file_format.as_deref().filter(|f| !f.is_empty()) {
            form.push((format!("fileformat.{i}"), format.to_string()));
        }
    }
    form
}

/// The `application/x-www-form-urlencoded` body for a form, fields joined by `&`.
pub fn encode_form(form: &[(String, String)]) -> String {
    form.iter()
        .map(|(name, value)| {
            format!(
                "{}={}",
                dotnet::form_url_encode(name),
                dotnet::form_url_encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// AcoustID answers some refusals with HTTP 200 and an error envelope, the exact shape
/// that made over-budget Deezer calls silently destructive. `is_ok` is therefore read from
/// the body, never from the status code.
pub fn parse_lookup(root: &Value) -> ElementResult<AcoustIdLookup> {
    let status = match try_get_property(root, "status")? {
        Some(s) => get_string(s)?,
        None => None,
    };
    if !status.is_some_and(|s| dotnet::eq_ignore_case(s, "ok")) {
        let message = error_message_or(root, status)?;
        return Ok(AcoustIdLookup {
            is_ok: false,
            error: Some(message.unwrap_or_else(|| "unknown error".to_string())),
            results: Vec::new(),
        });
    }

    let mut results = Vec::new();
    if let Some(rs @ Value::Array(_)) = try_get_property(root, "results")? {
        for result in enumerate_array(rs)?.iter().take(MAX_RESULTS) {
            let score = match try_get_property(result, "score")? {
                Some(sc @ Value::Number(_)) => get_double(sc)?,
                _ => 0.0,
            };
            let recordings = parse_recordings(result)?;
            results.push(AcoustIdResult {
                score,
                recordings,
                id: string_prop(result, "id")?,
            });
        }
    }

    Ok(AcoustIdLookup {
        is_ok: true,
        error: None,
        results,
    })
}

/// The error envelope's message when the root has an `error` object with a `message` (even a
/// null one), otherwise the fallback.
pub fn error_message_or(root: &Value, fallback: Option<&str>) -> ElementResult<Option<String>> {
    if let Some(err @ Value::Object(_)) = try_get_property(root, "error")?
        && let Some(m) = try_get_property(err, "message")?
    {
        return Ok(get_string(m)?.map(str::to_string));
    }
    Ok(fallback.map(str::to_string))
}

fn parse_recordings(result: &Value) -> ElementResult<Vec<AcoustIdRecording>> {
    let Some(recs @ Value::Array(_)) = try_get_property(result, "recordings")? else {
        return Ok(Vec::new());
    };

    let mut recordings = Vec::new();
    for rec in enumerate_array(recs)?.iter().take(MAX_RECORDINGS) {
        let id = match try_get_property(rec, "id")? {
            Some(i) => get_string(i)?.unwrap_or("").to_string(),
            None => String::new(),
        };
        let title = match try_get_property(rec, "title")? {
            Some(t) => get_string(t)?.unwrap_or("").to_string(),
            None => String::new(),
        };

        let mut artists = Vec::new();
        let mut credits = Vec::new();
        if let Some(arts @ Value::Array(_)) = try_get_property(rec, "artists")? {
            for artist in enumerate_array(arts)? {
                if let Some(n) = try_get_property(artist, "name")?
                    && let Some(name) = get_string(n)?.filter(|name| !name.is_empty())
                {
                    artists.push(name.to_string());
                    credits.push(AcoustIdCredit {
                        name: name.to_string(),
                        artist_id: string_prop(artist, "id")?,
                        join_phrase: string_prop(artist, "joinphrase")?.unwrap_or_default(),
                    });
                }
            }
        }

        let exact = match try_get_property(rec, "duration")? {
            Some(d @ Value::Number(_)) => Some(get_double(d)?),
            _ => None,
        };
        let duration = exact.map(|seconds| dotnet::round(seconds, 0) as i32);
        let duration_ms = exact
            .filter(|seconds| *seconds > 0.0)
            .map(|seconds| (seconds * 1000.0).round() as i64);

        let (album, year, detail) = pick_release(rec)?;
        recordings.push(AcoustIdRecording {
            recording_id: id,
            title,
            artists,
            album_title: album,
            year,
            credits,
            release: detail,
            duration_seconds: duration,
            duration_ms,
            releases: all_releases(rec)?,
            isrcs: music_brainz_client::parse_isrcs(rec)?,
            sources: int(rec, "sources")?.unwrap_or(0),
        });
    }
    Ok(recordings)
}

/// Prefer a plain studio album: a release group with no secondarytypes. Otherwise a
/// compilation or a live album supplies the album name and year for a studio track.
/// The year is the EARLIEST release in the chosen group, because a 2011 reissue is not
/// the track's year. Within that group the earliest dated release is the one whose ids and
/// track position are kept, so the tracks of one album converge on one release.
fn pick_release(recording: &Value) -> ElementResult<(Option<String>, Option<i32>, Option<AcoustIdRelease>)> {
    let Some(groups @ Value::Array(_)) = try_get_property(recording, "releasegroups")? else {
        return Ok((None, None, None));
    };

    let mut chosen: Option<&Value> = None;
    for group in enumerate_array(groups)?.iter().take(MAX_RELEASE_GROUPS) {
        chosen.get_or_insert(group);
        let is_album = match try_get_property(group, "type")? {
            Some(ty) => get_string(ty)?.is_some_and(|t| dotnet::eq_ignore_case(t, "Album")),
            None => false,
        };
        let has_secondary = match try_get_property(group, "secondarytypes")? {
            Some(sec @ Value::Array(_)) => array_length(sec)? > 0,
            _ => false,
        };
        if is_album && !has_secondary {
            chosen = Some(group);
            break;
        }
    }
    let Some(pick) = chosen else {
        return Ok((None, None, None));
    };

    let album = str_prop(pick, "title")?;
    let mut is_compilation = false;
    if let Some(types @ Value::Array(_)) = try_get_property(pick, "secondarytypes")? {
        // Any() stops at the first match, so a bad entry after it is never read.
        for kind in enumerate_array(types)? {
            if get_string(kind)?.is_some_and(|t| dotnet::eq_ignore_case(t, "Compilation")) {
                is_compilation = true;
                break;
            }
        }
    }

    let mut album_artist = None;
    if let Some(group_artists @ Value::Array(_)) = try_get_property(pick, "artists")? {
        let credits = credits_of(group_artists)?;
        if !credits.is_empty() {
            album_artist = Some(AcoustIdRecording::join_credits(&credits));
        }
    }
    if album_artist
        .as_deref()
        .is_some_and(|a| dotnet::eq_ignore_case(a, "Various Artists"))
    {
        is_compilation = true;
    }

    let mut year: Option<i32> = None;
    let mut earliest: Option<&Value> = None;
    let mut earliest_date = (i32::MAX, 0, 0);
    if let Some(releases @ Value::Array(_)) = try_get_property(pick, "releases")? {
        for release in enumerate_array(releases)? {
            earliest.get_or_insert(release);
            let Some(date @ Value::Object(_)) = try_get_property(release, "date")? else {
                continue;
            };
            let Some(y @ Value::Number(_)) = try_get_property(date, "year")? else {
                continue;
            };
            let candidate = get_int32(y)?;
            if candidate <= 0 {
                continue;
            }
            if year.is_none_or(|known| candidate < known) {
                year = Some(candidate);
            }
            let when = (
                candidate,
                int(date, "month")?.unwrap_or(0),
                int(date, "day")?.unwrap_or(0),
            );
            if when < earliest_date {
                earliest_date = when;
                earliest = Some(release);
            }
        }
    }

    let (mut track_number, mut track_count, mut disc) = (None, None, None);
    let (mut release_id, mut release_title) = (None, None);
    if let Some(rel) = earliest {
        release_id = string_prop(rel, "id")?;
        // compress drops a release title equal to its group's.
        release_title = string_prop(rel, "title")?;
        if let Some(mediums @ Value::Array(_)) = try_get_property(rel, "mediums")? {
            for medium in enumerate_array(mediums)? {
                let Some(first) = first_track(medium)? else {
                    continue;
                };
                track_number = int(first, "position")?;
                track_count = int(medium, "track_count")?;
                disc = int(medium, "position")?;
                break;
            }
        }
    }

    let clean = album.filter(|a| !dotnet::is_blank(a)).map(str::to_string);
    let detail = AcoustIdRelease::new(
        release_id,
        string_prop(pick, "id")?,
        release_title.or_else(|| clean.clone()),
        year,
        track_number,
        track_count,
        disc,
        album_artist,
        is_compilation,
    );
    Ok((clean, year, Some(detail)))
}

/// The first track of a medium, when it lists any: `medium.tracks` is a non-empty array.
fn first_track(medium: &Value) -> ElementResult<Option<&Value>> {
    match try_get_property(medium, "tracks")? {
        Some(tracks @ Value::Array(_)) => Ok(enumerate_array(tracks)?.first()),
        _ => Ok(None),
    }
}

/// Every release of every group, bounded, each with what the chooser weighs: the group's
/// kind, the release's date and country, the track's position and id. The same reading as
/// [`pick_release`], over all of them instead of the one it picks.
fn all_releases(recording: &Value) -> ElementResult<Vec<AcoustIdRelease>> {
    let Some(groups @ Value::Array(_)) = try_get_property(recording, "releasegroups")? else {
        return Ok(Vec::new());
    };

    let mut releases = Vec::new();
    for group in enumerate_array(groups)?.iter().take(MAX_RELEASE_GROUPS) {
        let group_id = string_prop(group, "id")?;
        let group_title = string_prop(group, "title")?;
        let primary_type = string_prop(group, "type")?;
        let mut secondary_types = Vec::new();
        if let Some(sec @ Value::Array(_)) = try_get_property(group, "secondarytypes")? {
            for kind in enumerate_array(sec)? {
                if let Some(text) = get_string(kind)? {
                    secondary_types.push(text.to_string());
                }
            }
        }
        let (album_artist, album_artist_ids) = group_credit(group)?;
        let is_compilation = secondary_types
            .iter()
            .any(|t| dotnet::eq_ignore_case(t, "Compilation"))
            || album_artist
                .as_deref()
                .is_some_and(|a| dotnet::eq_ignore_case(a, "Various Artists"));

        let Some(list @ Value::Array(_)) = try_get_property(group, "releases")? else {
            releases.push(AcoustIdRelease {
                group_title: group_title.clone(),
                primary_type,
                secondary_types,
                album_artist_ids,
                ..AcoustIdRelease::new(
                    None,
                    group_id,
                    group_title,
                    None,
                    None,
                    None,
                    None,
                    album_artist,
                    is_compilation,
                )
            });
            continue;
        };

        for release in enumerate_array(list)?.iter().take(MAX_RELEASES_PER_GROUP) {
            let mut year = None;
            let mut date = None;
            if let Some(when @ Value::Object(_)) = try_get_property(release, "date")?
                && let Some(y) = int(when, "year")?.filter(|&y| y > 0)
            {
                year = Some(y);
                let month = int(when, "month")?;
                let day = int(when, "day")?;
                date = Some(match (month.filter(|&m| m > 0), day.filter(|&d| d > 0)) {
                    (Some(m), Some(d)) => format!("{y:04}-{m:02}-{d:02}"),
                    (Some(m), None) => format!("{y:04}-{m:02}"),
                    (None, _) => format!("{y:04}"),
                });
            }

            let (mut track_number, mut track_count, mut disc, mut track_id) = (None, None, None, None);
            if let Some(mediums @ Value::Array(_)) = try_get_property(release, "mediums")? {
                for medium in enumerate_array(mediums)? {
                    let Some(first) = first_track(medium)? else {
                        continue;
                    };
                    track_number = int(first, "position")?;
                    track_id = string_prop(first, "id")?;
                    track_count = int(medium, "track_count")?;
                    disc = int(medium, "position")?;
                    break;
                }
            }

            // compress drops a release title equal to its group's.
            let title = string_prop(release, "title")?.or_else(|| group_title.clone());
            releases.push(AcoustIdRelease {
                group_title: group_title.clone(),
                primary_type: primary_type.clone(),
                secondary_types: secondary_types.clone(),
                country: string_prop(release, "country")?,
                date,
                release_track_id: track_id,
                disc_count: int(release, "medium_count")?,
                album_artist_ids: album_artist_ids.clone(),
                ..AcoustIdRelease::new(
                    string_prop(release, "id")?,
                    group_id.clone(),
                    title,
                    year,
                    track_number,
                    track_count,
                    disc,
                    album_artist.clone(),
                    is_compilation,
                )
            });
        }
    }
    Ok(releases)
}

/// The credits of a group's `artists` array: every entry with a non-empty name.
fn credits_of(artists: &Value) -> ElementResult<Vec<AcoustIdCredit>> {
    let mut credits = Vec::new();
    for artist in enumerate_array(artists)? {
        if let Some(name) = str_prop(artist, "name")?.filter(|n| !n.is_empty()) {
            credits.push(AcoustIdCredit {
                name: name.to_string(),
                artist_id: string_prop(artist, "id")?,
                join_phrase: string_prop(artist, "joinphrase")?.unwrap_or_default(),
            });
        }
    }
    Ok(credits)
}

fn group_credit(group: &Value) -> ElementResult<(Option<String>, Vec<String>)> {
    let Some(artists @ Value::Array(_)) = try_get_property(group, "artists")? else {
        return Ok((None, Vec::new()));
    };
    let credits = credits_of(artists)?;
    if credits.is_empty() {
        return Ok((None, Vec::new()));
    }
    let ids = credits.iter().filter_map(|c| c.artist_id.clone()).collect();
    Ok((Some(AcoustIdRecording::join_credits(&credits)), ids))
}

/// An `i32` property: absent, not a number, or a number that is not an integer reads as none.
fn int(element: &Value, name: &str) -> ElementResult<Option<i32>> {
    match try_get_property(element, name)? {
        Some(value @ Value::Number(_)) => try_get_int32(value),
        _ => Ok(None),
    }
}

#[cfg(test)]
#[path = "acoust_id_client_tests.rs"]
mod tests;
