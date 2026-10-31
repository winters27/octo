//! The pure half of the Lidarr track fetcher: the request, which of the album's tracks is the
//! song, and whether a file is lossless. The fetcher is
//! `octo::services::lidarr::lidarr_track_fetcher`.

use std::fmt;
use std::sync::Arc;

use super::{LidarrImportedTrack, LidarrReleasePick};
use crate::common::SongIdentity;
use crate::common::dotnet;

/// One song to fetch through Lidarr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrTrackRequest {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_seconds: Option<i32>,
    /// Wait for a lossless file; any file will do otherwise.
    pub lossless_only: bool,
    /// The library file this replaces, when there is one: its tags can name the album exactly,
    /// and Lidarr may already be the one managing it.
    pub original_path: Option<String>,
    /// A release picked in Find songs, grabbed instead of Lidarr's own search.
    pub release: Option<LidarrReleasePick>,
    /// Told, in words for the download's log, what became of the picked release.
    pub step: Option<LidarrStep>,
    /// The download's own song, as `provider:id`.
    pub song: Option<String>,
    /// The recordings a Better quality's original is: a kept file's own, else what its own sound
    /// reads clear as, else its recording tag; none for a Wrong song or a Wrong version, whose
    /// original is not the recording wanted. Only a hint: the album is the walk's first pick for
    /// them when the original's release group tag and its album name find none, and the search
    /// is pinned to a release that carries them.
    pub recordings: Vec<String>,
}

/// Takes a line for the download's log, and its detail.
#[derive(Clone)]
pub struct LidarrStep(pub Arc<dyn Fn(&str, Option<&str>) + Send + Sync>);

impl LidarrStep {
    pub fn new(step: impl Fn(&str, Option<&str>) + Send + Sync + 'static) -> Self {
        LidarrStep(Arc::new(step))
    }

    pub fn tell(&self, text: &str, detail: Option<&str>) {
        (self.0)(text, detail);
    }
}

impl fmt::Debug for LidarrStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LidarrStep")
    }
}

/// A step is equal only to itself: the same callback.
impl PartialEq for LidarrStep {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for LidarrStep {}

const LOSSLESS_SUFFIXES: [&str; 6] = [".flac", ".wav", ".aiff", ".aif", ".ape", ".wv"];

/// The album's track for this song: by title, never by number, since the number came
/// from whatever release the request was made from. Several by title: the nearest length.
pub fn match_track<'a>(
    tracks: impl IntoIterator<Item = &'a LidarrImportedTrack>,
    request: &LidarrTrackRequest,
) -> Option<&'a LidarrImportedTrack> {
    let strict = SongIdentity::strict_titles();
    let by_title: Vec<&LidarrImportedTrack> = tracks
        .into_iter()
        .filter(|t| SongIdentity::same_title(&t.title, &request.title, Some(&strict)).is_same())
        .collect();
    let wanted = match request.duration_seconds {
        Some(d) if d > 0 && by_title.len() > 1 => d,
        _ => return by_title.first().copied(),
    };
    // `min_by_key` keeps the first of the nearest.
    by_title.into_iter().min_by_key(|t| match t.duration_seconds {
        Some(d) => (i64::from(d) - i64::from(wanted)).abs(),
        None => i64::from(i32::MAX),
    })
}

/// Lidarr's quality name when it gave one (FLAC, ALAC, WAV, APE), else the file's extension.
pub fn is_lossless(track: &LidarrImportedTrack) -> bool {
    match &track.quality {
        Some(quality) => ["FLAC", "ALAC", "WAV", "APE"]
            .iter()
            .any(|prefix| dotnet::starts_with_ignore_case(quality, prefix)),
        None => {
            let extension = get_extension(track.path.as_deref().unwrap_or(""));
            LOSSLESS_SUFFIXES
                .iter()
                .any(|s| dotnet::eq_ignore_case(s, extension))
        }
    }
}

/// The file's extension: from the file name's last dot, empty when there is none or the name ends
/// with it.
fn get_extension(path: &str) -> &str {
    let name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => &name[dot..],
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(number: i32, title: &str, seconds: i32) -> LidarrImportedTrack {
        LidarrImportedTrack {
            id: number,
            title: title.into(),
            track_number: Some(number),
            duration_seconds: Some(seconds),
            has_file: true,
            path: Some(format!("/data/music/{title}.flac")),
            size_bytes: 1,
            artist: Some("Massive Attack".into()),
            track_file_id: number,
            quality: None,
            foreign_recording_id: None,
        }
    }

    fn teardrop() -> LidarrTrackRequest {
        LidarrTrackRequest {
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            album: Some("Mezzanine".into()),
            duration_seconds: Some(330),
            lossless_only: true,
            original_path: None,
            release: None,
            step: None,
            song: None,
            recordings: Vec::new(),
        }
    }

    #[test]
    fn the_song_is_matched_by_title_never_by_number() {
        let tracks = [track(1, "Angel", 380), track(3, "Teardrop", 330)];

        assert_eq!(
            match_track(&tracks, &teardrop()).map(|t| t.title.as_str()),
            Some("Teardrop")
        );
        let unknown = LidarrTrackRequest {
            title: "Unknown".into(),
            album: None,
            duration_seconds: Some(380),
            ..teardrop()
        };
        assert!(match_track(&tracks, &unknown).is_none());
        // Two takes of one title: the nearer length.
        let takes = [track(3, "Teardrop", 200), track(4, "Teardrop", 331)];
        assert_eq!(match_track(&takes, &teardrop()).map(|t| t.id), Some(4));
    }

    #[test]
    fn lossless_is_read_from_the_quality_name_then_the_extension() {
        let with = |quality: Option<&str>, path: &str| LidarrImportedTrack {
            quality: quality.map(str::to_string),
            path: Some(path.into()),
            ..track(1, "Angel", 380)
        };
        assert!(is_lossless(&with(Some("FLAC 24bit"), "/a.mp3")));
        assert!(is_lossless(&with(Some("alac"), "/a.m4a")));
        assert!(!is_lossless(&with(Some("MP3-320"), "/a.flac")));
        assert!(is_lossless(&with(None, "/a.FLAC")));
        assert!(!is_lossless(&with(None, "/a.mp3")));
        assert!(!is_lossless(&with(None, "/a.")));
    }
}
