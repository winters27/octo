//! What Octo reads from and decides about a SpotiFLAC server's web API (`/api/v1`): the answers'
//! shapes, which catalog track to ask for, where a job's file lands, and what a job's state
//! means. No I/O here; the client in the `octo` crate does the asking.

use serde::Deserialize;

use crate::common::song_identity::SongIdentity;
use crate::common::version_gap::{Against, other_version};
use crate::fingerprint::track_match_comparer::TrackMatchComparer;
use crate::soulseek::soulseek_download_service::DURATION_TOLERANCE_SECONDS;

pub mod check;

/// `GET /api/v1/info`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub api_version: String,
    pub multiuser: bool,
    pub authenticated: bool,
}

impl ServerInfo {
    /// Whether the answer is SpotiFLAC's own, or Mornye's (its name since the rename): the
    /// name, or at least the API version.
    pub fn is_spotiflac(&self) -> bool {
        self.name.eq_ignore_ascii_case("SpotiFLAC")
            || self.name.eq_ignore_ascii_case("Mornye")
            || self.api_version == "v1"
    }
}

/// One track of SpotiFLAC's catalog, as `/api/v1/search` lists it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct CatalogTrack {
    pub id: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub isrc: String,
    pub duration_ms: i64,
    pub external_url: String,
}

impl CatalogTrack {
    /// The link a download is asked for by: the track's own, or one made from its id.
    pub fn link(&self) -> Option<String> {
        let url = self.external_url.trim();
        if url.starts_with("http://") || url.starts_with("https://") {
            return Some(url.to_string());
        }
        let id = self.id.trim();
        (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
            .then(|| format!("https://open.spotify.com/track/{id}"))
    }

    /// The length in whole seconds, or None when the catalog gave none.
    pub fn seconds(&self) -> Option<i32> {
        (self.duration_ms > 0).then(|| ((self.duration_ms + 500) / 1000) as i32)
    }
}

/// `GET /api/v1/search`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SearchAnswer {
    pub tracks: Vec<CatalogTrack>,
}

/// One song of a job, as the job lists it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct JobItem {
    pub status: String,
    /// The file's path on SpotiFLAC's side, once the song is done.
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    /// The extension the song went through (Mornye; `none` when the job failed before any, and
    /// null or missing before one is tried).
    pub provider: Option<String>,
}

impl JobItem {
    /// The path the item names as its file, when it names one.
    pub fn file(&self) -> Option<&str> {
        match &self.result {
            Some(serde_json::Value::String(path)) if path.contains('/') || path.contains('\\') => Some(path),
            _ => None,
        }
    }
}

/// What a job carries beside its state.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct JobPayload {
    pub items_detail: Vec<JobItem>,
}

/// `POST /api/v1/downloads` and `GET /api/v1/downloads/{id}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Job {
    pub id: String,
    pub status: String,
    pub error: Option<String>,
    pub payload: JobPayload,
}

/// Where a job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Queued, running, paused or being retried.
    Working,
    Done,
    Failed,
    Cancelled,
}

impl Job {
    pub fn state(&self) -> JobState {
        match self.status.trim().to_ascii_lowercase().as_str() {
            "done" | "completed" | "skipped" => JobState::Done,
            "failed" | "error" => JobState::Failed,
            "cancelled" | "canceled" => JobState::Cancelled,
            _ => JobState::Working,
        }
    }

    /// Whether the job can be asked about again by its id: SpotiFLAC answers `direct` for a
    /// download it started without a queue.
    pub fn is_pollable(&self) -> bool {
        let id = self.id.trim();
        !id.is_empty() && id != "direct"
    }

    /// Why the job failed, in SpotiFLAC's words, when it said.
    pub fn failure(&self) -> Option<String> {
        self.payload
            .items_detail
            .iter()
            .filter_map(|item| item.error.as_deref())
            .chain(self.error.as_deref())
            .map(str::trim)
            .find(|text| !text.is_empty())
            .map(str::to_string)
    }

    /// The file the job names, when one of its items is done.
    pub fn file(&self) -> Option<&str> {
        self.payload.items_detail.iter().find_map(JobItem::file)
    }

    /// The extension the job's song went through or failed in, when an item names one.
    pub fn provider(&self) -> Option<&str> {
        self.payload
            .items_detail
            .iter()
            .find_map(|item| item.provider.as_deref().and_then(check::named_provider))
    }
}

/// One row of `GET /api/v1/extensions`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ExtensionRow {
    pub installed: bool,
    /// The extension's name (Mornye; an older SpotiFLAC leaves it out).
    pub provider: String,
    pub attempts: i64,
    pub failures: i64,
    /// `success` or `failure`, for its last try.
    pub last_outcome: String,
    /// What its last failed try said, cut to 300 characters by Mornye.
    pub last_error: String,
}

impl ExtensionsAnswer {
    /// The row that counts extension `name`'s tries. Mornye keeps them on a row of the bare
    /// name beside the installed `ext:` row, so of the rows with the same id the one with the
    /// most tries wins, the exact name breaking a tie.
    pub fn row(&self, name: &str) -> Option<&ExtensionRow> {
        let id = check::extension_id(name);
        self.providers
            .iter()
            .filter(|row| check::extension_id(&row.provider) == id)
            .max_by_key(|row| (row.attempts, row.provider == name))
    }
}

/// `GET /api/v1/extensions`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ExtensionsAnswer {
    pub providers: Vec<ExtensionRow>,
}

impl ExtensionsAnswer {
    /// How many extensions SpotiFLAC has installed.
    pub fn installed(&self) -> usize {
        self.providers.iter().filter(|row| row.installed).count()
    }
}

/// The error shape SpotiFLAC answers every failure with.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ErrorAnswer {
    pub error: String,
    pub detail: Option<String>,
}

/// The song a download asks for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Wanted<'a> {
    pub artist: &'a str,
    pub title: &'a str,
    pub album: Option<&'a str>,
    pub isrc: Option<&'a str>,
    pub seconds: Option<i32>,
}

impl Wanted<'_> {
    fn isrc(&self) -> Option<String> {
        self.isrc
            .map(|isrc| isrc.trim().to_ascii_uppercase())
            .filter(|isrc| !isrc.is_empty())
    }

    fn seconds(&self) -> Option<i32> {
        self.seconds.filter(|seconds| *seconds > 0)
    }
}

/// The searches to run for a song, in order: its ISRC first, when it has one, then its artist
/// and title.
pub fn search_queries(want: &Wanted<'_>) -> Vec<String> {
    let mut queries = Vec::new();
    if let Some(isrc) = want.isrc() {
        queries.push(format!("isrc:{isrc}"));
    }
    let words = format!("{} {}", want.artist.trim(), want.title.trim());
    let words = words.trim();
    if !words.is_empty() {
        queries.push(words.to_string());
    }
    queries
}

/// Mornye's catalog search leaves `isrc` empty even when the query was `isrc:XXXX`, so a track
/// found by that query is taken as having the ISRC asked for. Tracks that already name an ISRC
/// keep their own.
pub fn with_query_isrc(mut tracks: Vec<CatalogTrack>, query: &str) -> Vec<CatalogTrack> {
    if let Some(isrc) = query
        .strip_prefix("isrc:")
        .map(str::trim)
        .filter(|isrc| !isrc.is_empty())
    {
        for track in &mut tracks {
            if track.isrc.trim().is_empty() {
                track.isrc = isrc.to_string();
            }
        }
    }
    tracks
}

/// The catalog track a search answer offers for the song, and in words why it fits; or why
/// none does.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    Track { index: usize, why: String },
    None { why: String },
}

/// Why a catalog track is not the song, or None when it is.
fn refusal(track: &CatalogTrack, want: &Wanted<'_>, by_isrc: bool) -> Option<String> {
    if track.link().is_none() {
        return Some("has no link to ask for".into());
    }
    if !by_isrc && !TrackMatchComparer::title_matches(want.title, &track.title) {
        return Some("is another song".into());
    }
    if other_version(want.title, &track.title, Against::Title) {
        return Some("is another version".into());
    }
    if !by_isrc {
        let credits: Vec<&str> = track.artists.split(", ").map(str::trim).collect();
        if !TrackMatchComparer::artist_matches(want.artist, &track.artists, &credits) {
            return Some("is by another artist".into());
        }
    }
    if let (Some(asked), Some(found)) = (want.seconds(), track.seconds())
        && (asked - found).abs() > DURATION_TOLERANCE_SECONDS
    {
        return Some(format!("runs {found} s, not {asked} s"));
    }
    if !by_isrc && want.seconds().is_none() && SongIdentity::key(want.artist).is_empty() {
        return Some("cannot be told apart without an artist or a length".into());
    }
    None
}

/// Picks the catalog track that is the song asked for. A track with the song's ISRC is the
/// song, unless its title names another version or its length is off; any other track must
/// have the song's title and artist, no other version, and the song's length when both are
/// known. Of those that fit, an ISRC match wins, then the closest length, then the same album.
pub fn pick_track(tracks: &[CatalogTrack], want: &Wanted<'_>) -> Pick {
    let isrc = want.isrc();
    let mut best: Option<(usize, (u8, i32, u8))> = None;
    let mut last_refusal: Option<String> = None;
    for (index, track) in tracks.iter().enumerate() {
        let by_isrc = isrc
            .as_deref()
            .is_some_and(|isrc| track.isrc.trim().eq_ignore_ascii_case(isrc));
        if let Some(why) = refusal(track, want, by_isrc) {
            if last_refusal.is_none() {
                last_refusal = Some(format!("'{}' by {} {why}", track.title, track.artists));
            }
            continue;
        }
        let gap = match (want.seconds(), track.seconds()) {
            (Some(asked), Some(found)) => (asked - found).abs(),
            _ => DURATION_TOLERANCE_SECONDS,
        };
        let same_album = want
            .album
            .is_some_and(|album| SongIdentity::key(album) == SongIdentity::key(&track.album));
        let rank = (u8::from(!by_isrc), gap, u8::from(!same_album));
        if best.as_ref().is_none_or(|(_, held)| rank < *held) {
            best = Some((index, rank));
        }
    }
    match best {
        Some((index, (by_name, _, _))) => {
            let track = &tracks[index];
            let length = track
                .seconds()
                .map(|seconds| format!(", {}:{:02}", seconds / 60, seconds % 60))
                .unwrap_or_default();
            let why = if by_name == 0 {
                format!("'{}' by {}{length}, the same ISRC", track.title, track.artists)
            } else {
                format!("'{}' by {}{length}", track.title, track.artists)
            };
            Pick::Track { index, why }
        }
        None => Pick::None {
            why: match (tracks.is_empty(), last_refusal) {
                (true, _) | (false, None) => "Mornye's search found nothing".into(),
                (false, Some(refusal)) => format!("Nothing fits; for example {refusal}"),
            },
        },
    }
}

/// `path`, which is under Octo's music folder `octo_root`, as SpotiFLAC sees it when its music
/// folder is `folder`; the same path when `folder` is None or the path is not under the root.
pub fn path_for_spotiflac(path: &str, octo_root: &str, folder: Option<&str>) -> String {
    let Some(folder) = folder else {
        return path.to_string();
    };
    let root = octo_root.trim_end_matches('/');
    match path.strip_prefix(root) {
        Some(rest) if root.is_empty() || rest.is_empty() || rest.starts_with('/') => {
            format!("{}{rest}", folder.trim_end_matches('/'))
        }
        _ => path.to_string(),
    }
}

/// The audio file extensions a SpotiFLAC download can be, lowercased.
pub const AUDIO_EXTENSIONS: [&str; 9] = ["flac", "m4a", "mp4", "alac", "mp3", "ogg", "opus", "wav", "aiff"];

/// Whether a file name is one of [`AUDIO_EXTENSIONS`].
pub fn is_audio_file(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && AUDIO_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))
    })
}

/// Of the files a job left (name and size), the song: the largest audio file, never a partial
/// download.
pub fn pick_audio_file(files: &[(String, u64)]) -> Option<&str> {
    files
        .iter()
        .filter(|(name, size)| {
            let leaf = name.rsplit('/').next().unwrap_or(name);
            *size > 0 && is_audio_file(leaf) && !leaf.starts_with('.')
        })
        .max_by_key(|(_, size)| *size)
        .map(|(name, _)| name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artists: &str, seconds: i64, isrc: &str) -> CatalogTrack {
        CatalogTrack {
            id: "4uLU6hMCjMI75M1A2tKUQC".into(),
            title: title.into(),
            artists: artists.into(),
            album: "Album".into(),
            isrc: isrc.into(),
            duration_ms: seconds * 1000,
            external_url: String::new(),
        }
    }

    fn want<'a>(artist: &'a str, title: &'a str, seconds: Option<i32>, isrc: Option<&'a str>) -> Wanted<'a> {
        Wanted {
            artist,
            title,
            album: None,
            isrc,
            seconds,
        }
    }

    #[test]
    fn the_isrc_is_searched_first() {
        assert_eq!(
            search_queries(&want("IU", "Blueming", None, Some(" kra381901080 "))),
            vec!["isrc:KRA381901080", "IU Blueming"]
        );
        assert_eq!(
            search_queries(&want("IU", "Blueming", None, None)),
            vec!["IU Blueming"]
        );
    }

    #[test]
    fn a_track_with_the_isrc_wins_over_a_name_match() {
        let tracks = [
            track("Blueming", "IU", 217, "OTHER"),
            track("Blueming", "IU", 218, "KRA381901080"),
        ];
        match pick_track(&tracks, &want("IU", "Blueming", Some(217), Some("KRA381901080"))) {
            Pick::Track { index, why } => {
                assert_eq!(index, 1);
                assert!(why.contains("the same ISRC"), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn another_version_is_never_picked_even_with_the_isrc() {
        let tracks = [
            track("Blueming (Instrumental)", "IU", 217, "KRA381901080"),
            track("Blueming - Sped Up", "IU", 180, ""),
        ];
        match pick_track(&tracks, &want("IU", "Blueming", Some(217), Some("KRA381901080"))) {
            Pick::None { why } => assert!(why.contains("another version"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_name_match_needs_the_artist_and_the_length() {
        let tracks = [
            track("Blueming", "Somebody Else", 217, ""),
            track("Blueming", "IU", 260, ""),
            track("Blueming", "IU", 219, ""),
        ];
        match pick_track(&tracks, &want("IU", "Blueming", Some(217), None)) {
            Pick::Track { index, .. } => assert_eq!(index, 2),
            other => panic!("{other:?}"),
        }
        match pick_track(&tracks[..2], &want("IU", "Blueming", Some(217), None)) {
            Pick::None { why } => assert!(why.contains("another artist"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_answer_says_so() {
        assert_eq!(
            pick_track(&[], &want("IU", "Blueming", None, None)),
            Pick::None {
                why: "Mornye's search found nothing".into()
            }
        );
    }

    #[test]
    fn an_isrc_query_fills_empty_isrcs_only() {
        let tracks = vec![
            track("Blueming", "IU", 217, ""),
            track("Blueming", "IU", 217, " "),
            track("Blueming", "IU", 217, "OTHER"),
        ];
        let filled = with_query_isrc(tracks.clone(), "isrc:USUM71703861");
        assert_eq!(filled[0].isrc, "USUM71703861");
        assert_eq!(filled[1].isrc, "USUM71703861");
        assert_eq!(filled[2].isrc, "OTHER");
        assert_eq!(with_query_isrc(tracks.clone(), "IU Blueming"), tracks);
        assert_eq!(with_query_isrc(tracks.clone(), "isrc: "), tracks);
    }

    #[test]
    fn a_track_link_is_its_own_or_made_from_its_id() {
        let mut t = track("x", "y", 1, "");
        assert_eq!(
            t.link().as_deref(),
            Some("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC")
        );
        t.external_url = "https://open.spotify.com/track/abc".into();
        assert_eq!(t.link().as_deref(), Some("https://open.spotify.com/track/abc"));
        t.external_url = "javascript:alert(1)".into();
        t.id = "../etc".into();
        assert_eq!(t.link(), None);
    }

    #[test]
    fn mornye_answers_as_the_same_web_api() {
        let info: ServerInfo = serde_json::from_str(
            r#"{"name":"Mornye","version":"1.0.3","multiuser":false,"authenticated":true}"#,
        )
        .unwrap();
        assert!(info.is_spotiflac());
        let other: ServerInfo = serde_json::from_str(r#"{"name":"Something else"}"#).unwrap();
        assert!(!other.is_spotiflac());
    }

    #[test]
    fn job_states_and_failures_read_from_the_answer() {
        let job: Job = serde_json::from_str(
            r#"{"id":"a1","status":"failed","error":"top","payload":{"items_detail":[{"status":"FAILED","error":"no provider had it"}]}}"#,
        )
        .unwrap();
        assert_eq!(job.state(), JobState::Failed);
        assert_eq!(job.failure().as_deref(), Some("no provider had it"));
        assert!(job.is_pollable());

        let done: Job = serde_json::from_str(
            r#"{"id":"a2","status":"done","payload":{"items_detail":[{"status":"DONE","result":"/music/x/a.flac"}]}}"#,
        )
        .unwrap();
        assert_eq!(done.state(), JobState::Done);
        assert_eq!(done.file(), Some("/music/x/a.flac"));

        let direct: Job = serde_json::from_str(r#"{"id":"direct","status":"running"}"#).unwrap();
        assert_eq!(direct.state(), JobState::Working);
        assert!(!direct.is_pollable());
    }

    #[test]
    fn paths_are_mapped_into_spotiflacs_folder() {
        assert_eq!(
            path_for_spotiflac("/music/.octo-incoming/spotiflac/j1", "/music", Some("/downloads")),
            "/downloads/.octo-incoming/spotiflac/j1"
        );
        assert_eq!(
            path_for_spotiflac("/music/.octo-incoming/spotiflac/j1", "/music/", None),
            "/music/.octo-incoming/spotiflac/j1"
        );
        assert_eq!(
            path_for_spotiflac("/musicians/j1", "/music", Some("/downloads")),
            "/musicians/j1"
        );
    }

    #[test]
    fn the_song_is_the_largest_audio_file() {
        let files = vec![
            ("cover.jpg".to_string(), 900_000),
            ("Song.lrc".to_string(), 2_000),
            ("Song.flac".to_string(), 30_000_000),
            (".Song.flac.part".to_string(), 40_000_000),
            ("Empty.flac".to_string(), 0),
        ];
        assert_eq!(pick_audio_file(&files), Some("Song.flac"));
        assert_eq!(pick_audio_file(&files[..2]), None);
        assert!(is_audio_file("a.M4A"));
        assert!(!is_audio_file(".flac"));
    }

    #[test]
    fn extensions_count_only_the_installed_ones() {
        let answer: ExtensionsAnswer = serde_json::from_str(
            r#"{"providers":[{"provider":"a","installed":true},{"provider":"b","installed":false},{"provider":"c"}],"totals":{}}"#,
        )
        .unwrap();
        assert_eq!(answer.installed(), 1);
    }
}
