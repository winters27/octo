//! The evidence the release chooser weighs: the request, the file's own facts and the
//! candidates.

use std::collections::BTreeSet;
use std::fmt;

use crate::common::dotnet::{is_blank, to_lower_invariant};
pub use crate::settings::IgnoreCaseSet;
use crate::tagging::net::{parse_int, utf16_prefix};

/// Where a candidate came from, from the most to the least trusted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TagSource {
    #[default]
    Fingerprint,
    Database,
    Catalog,
    FileTags,
    Request,
}

impl TagSource {
    /// The member's name, which the report and the field table carry.
    pub fn name(self) -> &'static str {
        match self {
            Self::Fingerprint => "Fingerprint",
            Self::Database => "Database",
            Self::Catalog => "Catalog",
            Self::FileTags => "FileTags",
            Self::Request => "Request",
        }
    }
}

impl fmt::Display for TagSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a download was asked for, captured before anything corrects it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagRequest {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub track: Option<i32>,
    pub disc: Option<i32>,
    pub duration_seconds: Option<i32>,
    pub isrc: Option<String>,
    pub catalog_album_id: Option<String>,
    pub catalog_track_id: Option<String>,
    pub version_markers: BTreeSet<String>,
    /// One track of a whole album being fetched: only a release that is that album by name
    /// lends its facts, never a larger edition holding the same recording.
    pub whole_album: bool,
    /// For a request with no album: the release group the song's ISRC picks among the releases
    /// carrying its recordings, weighed as the album asked for. None when nothing was picked.
    pub album_group_id: Option<String>,
    /// That group's title, which weighs a release that names no group.
    pub album_group_title: Option<String>,
}

impl TagRequest {
    /// The request named an album, so the album, track and disc are its to keep.
    pub fn owns_album(&self) -> bool {
        !self.album.as_deref().is_none_or(is_blank)
    }
}

/// What landed on disk: its length and format, and the tags it arrived with.
/// Tag fields are empty for a file whose tags are not evidence (a video site upload).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileFacts {
    pub duration_seconds: i32,
    pub extension: String,
    pub sample_rate: i32,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<i32>,
    pub track: Option<i32>,
    pub disc: Option<i32>,
    pub isrcs: Vec<String>,
    pub barcode: Option<String>,
    pub catalog_number: Option<String>,
    pub label: Option<String>,
    pub recording_id: Option<String>,
    pub release_id: Option<String>,
    pub is_compilation: bool,
    pub tags_are_evidence: bool,
}

impl FileFacts {
    pub fn unknown(extension: &str) -> Self {
        Self {
            extension: extension.to_string(),
            ..Default::default()
        }
    }
}

/// One recording on one release, from one source. None means the source did not say.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReleaseCandidate {
    pub source: TagSource,
    pub recording_title: String,
    pub artist_credit: String,

    pub recording_id: Option<String>,
    pub primary_artist: Option<String>,
    pub artists: Vec<String>,
    pub artist_ids: Vec<String>,
    pub length_seconds: Option<i32>,
    pub isrcs: Vec<String>,
    pub release_id: Option<String>,
    pub release_group_id: Option<String>,
    pub release_title: Option<String>,
    pub group_title: Option<String>,
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
    pub status: Option<String>,
    pub country: Option<String>,
    pub release_date: Option<String>,
    pub group_first_release_date: Option<String>,
    pub barcode: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub track_number: Option<i32>,
    pub track_count: Option<i32>,
    pub disc_number: Option<i32>,
    pub disc_count: Option<i32>,
    pub album_artist: Option<String>,
    pub album_artist_ids: Vec<String>,
    pub release_track_id: Option<String>,
    pub is_compilation: bool,
    pub fingerprint_id: Option<String>,
    pub sources: i32,
    pub cover_url: Option<String>,
    pub genre: Option<String>,
    pub explicit: Option<bool>,
    /// The catalog's word for this version's lyrics: 0 not explicit, 1 explicit, 3 the clean
    /// edit; any other value says it does not know.
    pub explicit_content: Option<i32>,
    pub catalog_album_id: Option<String>,
    pub catalog_track_id: Option<String>,
}

impl ReleaseCandidate {
    /// A candidate from its required facts; everything else starts unsaid.
    pub fn new(
        source: TagSource,
        recording_title: impl Into<String>,
        artist_credit: impl Into<String>,
    ) -> Self {
        Self {
            source,
            recording_title: recording_title.into(),
            artist_credit: artist_credit.into(),
            ..Default::default()
        }
    }

    /// The album this candidate files the song under: the release's own title, else its group's.
    pub fn album_title(&self) -> Option<&str> {
        match self.release_title.as_deref() {
            Some(title) if !is_blank(title) => Some(title),
            _ => self.group_title.as_deref(),
        }
    }

    pub fn year(&self) -> Option<i32> {
        Self::year_of(self.release_date.as_deref())
    }

    pub fn original_year(&self) -> Option<i32> {
        Self::year_of(self.group_first_release_date.as_deref()).or_else(|| self.year())
    }

    pub(crate) fn year_of(date: Option<&str>) -> Option<i32> {
        parse_int(utf16_prefix(date?, 4)?).filter(|&year| year > 0)
    }

    /// How the source describes the release's kind, for the report: "album", "album; compilation".
    pub fn kind_text(&self) -> Option<String> {
        if self.primary_type.is_none() && self.secondary_types.is_empty() {
            return None;
        }
        Some(
            self.primary_type
                .iter()
                .chain(&self.secondary_types)
                .filter(|kind| !is_blank(kind))
                .map(|kind| to_lower_invariant(kind))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// Everything the chooser weighs a candidate against.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagEvidence {
    pub request: TagRequest,
    pub file: FileFacts,
    pub fingerprint_threshold: f64,
    /// Compared ordinally, ignoring case.
    pub fingerprinted_recording_ids: IgnoreCaseSet,
    /// The one recording the file's audio was found to be, when verification decided it. The
    /// album is then chosen among that recording's releases only.
    pub identity: Option<crate::fingerprint::recording_identity::Identity>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_years_and_kind_text() {
        let candidate = ReleaseCandidate {
            release_date: Some("2011-09-19".into()),
            group_first_release_date: Some("1991".into()),
            primary_type: Some("Album".into()),
            secondary_types: vec!["Compilation".into(), " ".into()],
            ..ReleaseCandidate::new(TagSource::Database, "t", "a")
        };
        assert_eq!(candidate.year(), Some(2011));
        assert_eq!(candidate.original_year(), Some(1991));
        assert_eq!(candidate.kind_text().as_deref(), Some("album; compilation"));

        let bare = ReleaseCandidate::new(TagSource::Catalog, "t", "a");
        assert_eq!(bare.kind_text(), None);
        assert_eq!(bare.year(), None);
        assert_eq!(ReleaseCandidate::year_of(Some("0000-01-01")), None);
        assert_eq!(ReleaseCandidate::year_of(Some("199")), None);
        assert_eq!(
            ReleaseCandidate {
                primary_type: Some(" ".into()),
                ..bare.clone()
            }
            .kind_text()
            .as_deref(),
            Some("")
        );

        let titled = ReleaseCandidate {
            release_title: Some(" ".into()),
            group_title: Some("Group".into()),
            ..bare
        };
        assert_eq!(titled.album_title(), Some("Group"));
    }

    #[test]
    fn ignore_case_set_and_owns_album() {
        let set: IgnoreCaseSet = ["rec-A", "REC-a", "rec-b"].into_iter().collect();
        assert_eq!(set.len(), 2);
        assert!(set.contains("Rec-B"));
        assert_eq!(set.iter().collect::<Vec<_>>(), ["rec-A", "rec-b"]);

        let mut request = TagRequest::default();
        assert!(!request.owns_album());
        request.album = Some("  ".into());
        assert!(!request.owns_album());
        request.album = Some("Discovery".into());
        assert!(request.owns_album());
    }
}
