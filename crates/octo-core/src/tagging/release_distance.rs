//! How far a release candidate is from what a download asked for, key by key.

use std::collections::BTreeSet;

use chrono::Datelike;

use crate::common::dotnet::{
    eq_ignore_case, is_blank, is_digit_utf16, is_letter_utf16, to_lower_invariant, utf16_len,
};
use crate::common::song_identity::{ArtistAgreement, SongIdentity, SongVerdict};
use crate::fingerprint::recording_identity::RecordingKind;
use crate::fingerprint::track_match_comparer::TrackMatchComparer;
use crate::tagging::matching_settings::MatchingSettings;
use crate::tagging::tag_evidence::{ReleaseCandidate, TagEvidence, TagSource};
use crate::tagging::tag_plan::{BreakdownPart, ScoredCandidate};

/// How far one candidate is from what was asked for and what landed, as a number from 0 (the
/// same) to 1 (nothing agrees). Each key adds a penalty times its weight; the result is the
/// weighted sum over the weights of the keys that could be judged, so a question with no
/// answer on either side neither helps nor hurts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Distance {
    parts: Vec<BreakdownPart>,
}

impl Distance {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, key: &str, penalty: f64, weight: f64) {
        self.parts.push(BreakdownPart {
            key: key.to_string(),
            penalty: penalty.clamp(0.0, 1.0),
            weight,
        });
    }

    pub fn value(&self) -> f64 {
        if self.parts.is_empty() {
            return 1.0;
        }
        let sum: f64 = self.parts.iter().map(|p| p.penalty * p.weight).sum();
        let weights: f64 = self.parts.iter().map(|p| p.weight).sum();
        sum / weights
    }

    pub fn breakdown(&self) -> &[BreakdownPart] {
        &self.parts
    }

    pub fn into_breakdown(self) -> Vec<BreakdownPart> {
        self.parts
    }

    /// The penalty one key added, or None when that key could not be judged.
    pub fn penalty_of(&self, key: &str) -> Option<f64> {
        self.parts.iter().find(|p| p.key == key).map(|p| p.penalty)
    }
}

/// The keys a candidate is judged on, their weights, and each key's penalty function. The
/// weights follow the taggers that solved this before Octo where the key exists there; the
/// identifiers (fingerprint, code, barcode) weigh most because they name a recording outright.
pub struct ReleaseDistance;

impl ReleaseDistance {
    pub const TITLE_WEIGHT: f64 = 3.0;
    pub const ARTIST_WEIGHT: f64 = 2.0;
    pub const LENGTH_WEIGHT: f64 = 2.0;
    pub const ALBUM_WEIGHT: f64 = 3.0;
    pub const FILE_ALBUM_WEIGHT: f64 = 1.0;
    pub const FILE_ALBUM_WEIGHT_WHEN_KEPT: f64 = 3.0;
    pub const TRACK_WEIGHT: f64 = 1.0;
    pub const FINGERPRINT_WEIGHT: f64 = 5.0;
    pub const ISRC_WEIGHT: f64 = 5.0;
    pub const BARCODE_WEIGHT: f64 = 5.0;
    pub const TYPE_WEIGHT: f64 = 2.0;
    pub const ORIGINAL_WEIGHT: f64 = 1.0;
    pub const STATUS_WEIGHT: f64 = 0.5;
    pub const SOURCE_WEIGHT: f64 = 2.0;
    pub const YEAR_WEIGHT: f64 = 1.0;
    pub const COUNTRY_WEIGHT: f64 = 0.5;
    pub const TAKE_WEIGHT: f64 = 6.0;

    pub const LENGTH_GRACE_SECONDS: i32 = 5;
    pub const LENGTH_MAX_SECONDS: i32 = 30;

    /// How many years of later first release count as "a whole generation later".
    pub const ORIGINAL_SPAN_YEARS: f64 = 25.0;

    /// The most an album title may cost and still name the same album: the same name, or the
    /// same once stylized characters are letters. Another edition costs more.
    pub const SAME_ALBUM_PENALTY: f64 = 0.1;

    /// The shortest title key allowed to match as a prefix or substring of a longer one.
    const MIN_PREFIX_CORE: usize = 6;

    pub fn measure(
        evidence: &TagEvidence,
        candidate: &ReleaseCandidate,
        settings: &MatchingSettings,
        earliest_group_year_for_recording: Option<i32>,
        this_year: Option<i32>,
    ) -> ScoredCandidate {
        let request = &evidence.request;
        let file = &evidence.file;
        let mut distance = Distance::new();

        distance.add(
            "title",
            Self::title_penalty(
                &request.title,
                &candidate.recording_title,
                &request.version_markers,
                &candidate.secondary_types,
            ),
            Self::TITLE_WEIGHT,
        );
        distance.add(
            "artist",
            Self::artist_penalty(&request.artist, &candidate.artist_credit, &candidate.artists),
            Self::ARTIST_WEIGHT,
        );

        // A release whose name says it is another take than the audio ("The Black Parade
        // (instrumentals)" for the studio take) is never the file's album, whatever else agrees.
        if let Some(identity) = &evidence.identity {
            let asked = RecordingKind::requested_by(&request.title);
            let names_another = [
                candidate.release_title.as_deref(),
                candidate.group_title.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|title| identity.contradicted_by(title, &asked).is_some());
            if names_another {
                distance.add("take", 1.0, Self::TAKE_WEIGHT);
            }
        }

        if file.duration_seconds > 0
            && let Some(length) = candidate.length_seconds.filter(|&length| length > 0)
        {
            distance.add(
                "length",
                Self::length_penalty(file.duration_seconds, length),
                Self::LENGTH_WEIGHT,
            );
        }

        let candidate_album = candidate.album_title().filter(|album| !is_blank(album));
        if let Some(candidate_album) = candidate_album {
            if request.owns_album() {
                let album = request.album.as_deref().unwrap_or("");
                distance.add(
                    "album",
                    Self::album_penalty(album, candidate_album),
                    Self::ALBUM_WEIGHT,
                );
            } else if let Some(group) = request.album_group_id.as_deref() {
                let same_group = candidate
                    .release_group_id
                    .as_deref()
                    .is_some_and(|id| eq_ignore_case(id, group));
                let penalty = if same_group {
                    0.0
                } else {
                    Self::album_penalty(
                        request.album_group_title.as_deref().unwrap_or(""),
                        candidate_album,
                    )
                };
                distance.add("album", penalty, Self::ALBUM_WEIGHT);
            } else if file.tags_are_evidence
                && let Some(file_album) = file.album.as_deref().filter(|album| !is_blank(album))
            {
                distance.add(
                    "file_album",
                    Self::album_penalty(file_album, candidate_album),
                    if settings.prefer_original_album {
                        Self::FILE_ALBUM_WEIGHT
                    } else {
                        Self::FILE_ALBUM_WEIGHT_WHEN_KEPT
                    },
                );
            }
        }

        if let (Some(track), Some(number)) = (
            request.track.filter(|&n| n > 0),
            candidate.track_number.filter(|&n| n > 0),
        ) {
            distance.add(
                "track",
                if track == number { 0.0 } else { 1.0 },
                Self::TRACK_WEIGHT,
            );
        }

        if !evidence.fingerprinted_recording_ids.is_empty() {
            let named = candidate
                .recording_id
                .as_deref()
                .is_some_and(|id| !id.is_empty() && evidence.fingerprinted_recording_ids.contains(id));
            distance.add(
                "fingerprint",
                if named { 0.0 } else { 1.0 },
                Self::FINGERPRINT_WEIGHT,
            );
        }

        let known_isrcs = SongIdentity::isrcs(
            file.isrcs
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(request.isrc.as_deref().unwrap_or(""))),
        );
        if !known_isrcs.is_empty() && !candidate.isrcs.is_empty() {
            let shared = SongIdentity::shares_isrc(&known_isrcs, &candidate.isrcs);
            distance.add("isrc", if shared { 0.0 } else { 0.5 }, Self::ISRC_WEIGHT);
        }

        if file.tags_are_evidence
            && let Some(barcode) =
                Self::barcode_penalty(file.barcode.as_deref(), candidate.barcode.as_deref())
        {
            distance.add("barcode", barcode, Self::BARCODE_WEIGHT);
        }

        if !request.owns_album() {
            distance.add(
                "type",
                Self::type_penalty(
                    candidate.primary_type.as_deref(),
                    &candidate.secondary_types,
                    &request.version_markers,
                ),
                Self::TYPE_WEIGHT,
            );
            if let (Some(group_year), Some(earliest)) =
                (candidate.original_year(), earliest_group_year_for_recording)
            {
                distance.add(
                    "original",
                    Self::original_penalty(group_year, earliest),
                    Self::ORIGINAL_WEIGHT,
                );
            }
        }

        if let Some(status) = candidate.status.as_deref().filter(|status| !is_blank(status)) {
            distance.add("status", Self::status_penalty(Some(status)), Self::STATUS_WEIGHT);
        }

        distance.add(
            "source",
            Self::source_penalty(candidate.source),
            Self::SOURCE_WEIGHT,
        );

        if file.tags_are_evidence
            && let (Some(known), Some(year)) =
                (file.year.filter(|&y| y > 0), candidate.year().filter(|&y| y > 0))
        {
            let this_year = this_year.unwrap_or_else(|| chrono::Utc::now().year());
            distance.add(
                "year",
                Self::year_penalty(known, Some(year), candidate.original_year(), this_year),
                Self::YEAR_WEIGHT,
            );
        }

        if !settings.preferred_countries.is_empty()
            && let Some(country) = candidate.country.as_deref().filter(|country| !is_blank(country))
        {
            distance.add(
                "country",
                Self::country_penalty(Some(country), &settings.preferred_countries),
                Self::COUNTRY_WEIGHT,
            );
        }

        let value = distance.value();
        ScoredCandidate::new(candidate.clone(), value, distance.into_breakdown())
    }

    /// The same title is 0. A title that reads the same once stylized characters are letters, or
    /// agrees by the looser readings, is 0.1; one that holds the other is 0.3; another title
    /// is 1. A version the request never asked for (a remix, a live take) is 1 whatever the core
    /// says. A version the request asked for and the candidate's title lacks is 0.8, unless the
    /// release's kind carries it (a live album, a remix release): the music database writes
    /// "live" in a recording's disambiguation and the release's kind, not always in its title.
    ///
    /// `request_markers` is unused: the title's own markers are read again here.
    pub fn title_penalty(
        requested: &str,
        candidate: &str,
        _request_markers: &BTreeSet<String>,
        secondary_types: &[String],
    ) -> f64 {
        let want = SongIdentity::parse_title(requested, None);
        let got = SongIdentity::parse_title(candidate, None);
        if want.key.is_empty() || got.key.is_empty() {
            return 1.0;
        }

        let core: f64 = if want.key == got.key {
            0.0
        } else if want.loose_key == got.loose_key
            || SongIdentity::same_title(requested, candidate, None).verdict != SongVerdict::Different
        {
            0.1
        } else if Self::contains(&want.key, &got.key)
            || Self::contains(&letters_of(&want.key), &letters_of(&got.key))
        {
            0.3
        } else {
            return 1.0;
        };

        let wanted = SongIdentity::distinct_versions(&want, None);
        let offered = SongIdentity::distinct_versions(&got, None);
        if offered.iter().any(|version| !wanted.contains(version)) {
            return 1.0;
        }

        let missing: Vec<&String> = wanted
            .iter()
            .filter(|version| !offered.contains(*version))
            .collect();
        if missing.is_empty() {
            return core;
        }
        if missing
            .iter()
            .all(|version| Self::kind_carries(version, secondary_types))
        {
            core
        } else {
            core.max(0.8)
        }
    }

    /// Whether a release's kind says what a title marker says: a live album for "live", a
    /// remix release for "remix".
    fn kind_carries(marker: &str, secondary_types: &[String]) -> bool {
        match marker {
            "live" | "unplugged" => has(secondary_types, "Live"),
            "remix" | "mix" | "dub" | "edit" | "vip" | "rework" => has(secondary_types, "Remix"),
            "demo" => has(secondary_types, "Demo"),
            _ => false,
        }
    }

    fn contains(a: &str, b: &str) -> bool {
        let (shorter, longer) = if utf16_len(a) <= utf16_len(b) {
            (a, b)
        } else {
            (b, a)
        };
        utf16_len(shorter) >= Self::MIN_PREFIX_CORE && longer.contains(shorter)
    }

    /// The same artist is 0, the same once stylized characters are letters 0.1, one side
    /// unknown 0.5, a different guest list 1. A credit that only holds the other's name is 0.2.
    pub fn artist_penalty(requested: &str, credit: &str, credits: &[String]) -> f64 {
        match SongIdentity::compare_artists_with_credits(requested, credit, credits) {
            ArtistAgreement::Agree => return 0.0,
            ArtistAgreement::Loose => return 0.1,
            ArtistAgreement::Unknown => return 0.5,
            ArtistAgreement::Conflict => return 1.0,
            ArtistAgreement::None => {}
        }
        if TrackMatchComparer::artist_matches(requested, credit, credits) {
            0.2
        } else {
            1.0
        }
    }

    /// Within five seconds is 0; thirty seconds past that is 1.
    pub fn length_penalty(file: i32, candidate: i32) -> f64 {
        let over = (i64::from(file) - i64::from(candidate)).abs() - i64::from(Self::LENGTH_GRACE_SECONDS);
        over.clamp(0, i64::from(Self::LENGTH_MAX_SECONDS)) as f64 / f64::from(Self::LENGTH_MAX_SECONDS)
    }

    /// Album titles by the same reading as song titles, except that an edition the
    /// request never named ("Deluxe", "Remastered", "Anniversary") costs 0.3 rather than nothing.
    pub fn album_penalty(requested: &str, candidate: &str) -> f64 {
        let want = SongIdentity::parse_title(requested, None);
        let got = SongIdentity::parse_title(candidate, None);
        if want.key.is_empty() || got.key.is_empty() {
            return 1.0;
        }

        let core: f64 = if want.key == got.key {
            0.0
        } else if want.loose_key == got.loose_key {
            0.1
        } else if Self::contains(&want.key, &got.key) {
            0.3
        } else {
            return 1.0;
        };

        if has_edition(candidate) && !has_edition(requested) {
            core.max(0.3)
        } else {
            core
        }
    }

    /// Whether a candidate's album is the requested album by name, not another edition of it:
    /// read both ways, so the plain album is not the requested edition either.
    pub fn names_same_album(requested: &str, candidate: &str) -> bool {
        Self::album_penalty(requested, candidate) <= Self::SAME_ALBUM_PENALTY
            && Self::album_penalty(candidate, requested) <= Self::SAME_ALBUM_PENALTY
    }

    /// What kind of release a song without a requested album should be filed under. A plain
    /// request wants the studio album; a single or an EP is close; a compilation, a live album or
    /// a remix release is the wrong place. A request that asked for a live take wants a live
    /// release; one that asked for a remix wants a remix release or a single.
    pub fn type_penalty(
        primary: Option<&str>,
        secondary: &[String],
        request_markers: &BTreeSet<String>,
    ) -> f64 {
        let live = request_markers.contains("live") || request_markers.contains("unplugged");
        let remix = ["remix", "mix", "dub", "vip", "rework", "edit"]
            .iter()
            .any(|marker| request_markers.contains(*marker));
        let is_live = has(secondary, "Live");
        let is_remix = has(secondary, "Remix");
        let kind = primary.map(|p| to_lower_invariant(p.trim()));

        if live {
            return if is_live { 0.0 } else { 0.6 };
        }
        if remix {
            return if is_remix {
                0.0
            } else if kind.as_deref() == Some("single") {
                0.2
            } else {
                0.5
            };
        }

        if has(secondary, "Compilation") || is_live || is_remix {
            return 1.0;
        }
        if has(secondary, "DJ-mix") || has(secondary, "Mixtape/Street") {
            return 0.8;
        }
        if has(secondary, "Soundtrack") {
            return 0.5;
        }
        match kind.as_deref() {
            Some("album") => 0.0,
            Some("single" | "ep") => 0.2,
            _ => 0.5,
        }
    }

    /// A release group first issued long after the recording's first release is a reissue,
    /// a compilation or a later pressing: a generation later is the whole penalty.
    pub fn original_penalty(group_first_year: i32, earliest_group_first_year: i32) -> f64 {
        (f64::from(group_first_year - earliest_group_first_year) / Self::ORIGINAL_SPAN_YEARS).clamp(0.0, 1.0)
    }

    pub fn status_penalty(status: Option<&str>) -> f64 {
        match status.map(|s| to_lower_invariant(s.trim())).as_deref() {
            Some("official") => 0.0,
            Some("promotion") => 0.5,
            Some("bootleg" | "pseudo-release" | "withdrawn" | "cancelled") => 1.0,
            _ => 0.25,
        }
    }

    /// What each source is worth before anything is compared.
    pub fn source_penalty(source: TagSource) -> f64 {
        match source {
            TagSource::Fingerprint => 0.0,
            TagSource::Database => 0.1,
            TagSource::Catalog => 0.25,
            TagSource::FileTags => 0.5,
            TagSource::Request => 1.0,
        }
    }

    /// The known year against the candidate's: the same, or the same as the recording's
    /// first release, is 0. Otherwise the gap as a share of the years the candidate has existed.
    pub fn year_penalty(
        known: i32,
        candidate_year: Option<i32>,
        original_year: Option<i32>,
        this_year: i32,
    ) -> f64 {
        let Some(year) = candidate_year else { return 0.0 };
        if known == year || original_year == Some(known) {
            return 0.0;
        }
        let span = (i64::from(this_year) - i64::from(year)).abs().max(1);
        ((i64::from(known) - i64::from(year)).abs() as f64 / span as f64).clamp(0.0, 1.0)
    }

    /// The first preferred country is free, each one down the list costs a share, and a
    /// country not on the list costs everything.
    pub fn country_penalty(country: Option<&str>, preferred: &[String]) -> f64 {
        let Some(country) = country.filter(|c| !preferred.is_empty() && !is_blank(c)) else {
            return 0.0;
        };
        let country = country.trim();
        for (i, wanted) in preferred.iter().enumerate() {
            if eq_ignore_case(wanted, country) {
                return i as f64 / preferred.len() as f64;
            }
        }
        1.0
    }

    /// The same barcode in its 12 or 13 digit form is 0, another one is 1, and a side that
    /// is not a barcode at all cannot be judged.
    pub fn barcode_penalty(file: Option<&str>, candidate: Option<&str>) -> Option<f64> {
        let mine = barcode_forms(file);
        let theirs = barcode_forms(candidate);
        if mine.is_empty() || theirs.is_empty() {
            return None;
        }
        Some(if theirs.iter().any(|code| mine.contains(code)) {
            0.0
        } else {
            1.0
        })
    }
}

fn has(types: &[String], kind: &str) -> bool {
    types.iter().any(|t| eq_ignore_case(t, kind))
}

fn letters_of(key: &str) -> String {
    key.chars().filter(|&c| is_letter_utf16(c)).collect()
}

fn has_edition(title: &str) -> bool {
    title.contains('(') || title.contains('[') || title.contains(" - ")
}

/// A barcode as given, and as the 12-digit UPC when it is a longer form with leading zeros.
/// Only digits count; anything else is not a barcode. The distance needs it, and the cover
/// lookups can call this one.
pub fn barcode_forms(code: Option<&str>) -> Vec<String> {
    let digits: String = code
        .unwrap_or("")
        .chars()
        .filter(|&c| is_digit_utf16(c))
        .collect();
    let length = digits.chars().count();
    if !(8..=14).contains(&length) {
        return Vec::new();
    }
    let trimmed = digits.trim_start_matches('0');
    let pad = 12usize.saturating_sub(trimmed.chars().count());
    let upc = "0".repeat(pad) + trimmed;
    let mut forms = vec![digits.clone()];
    if length > 12 && upc.chars().count() == 12 && upc != digits {
        forms.push(upc);
    }
    forms
}

#[cfg(test)]
#[path = "release_distance_tests.rs"]
mod tests;
