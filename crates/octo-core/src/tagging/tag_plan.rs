//! The chooser's decision, how it is applied to a song, and the report `downloads-history.json`
//! carries (`TagReport`, `TagReportCandidate` and `FieldDecision` round-trip that file byte for
//! byte).

use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::common::dotnet::{is_blank, round, to_lower_invariant};
use crate::common::song_identity::SongIdentity;
use crate::fingerprint::verification::VerificationVerdict;
use crate::metadata::deezer_metadata_service::FullTrackMeta;
use crate::models::domain::song::Song;
use crate::tagging::matching_settings::MatchingSettings;
use crate::tagging::net::{fixed, signed_fixed};
use crate::tagging::release_details::ReleaseDetails;
use crate::tagging::release_distance::ReleaseDistance;
use crate::tagging::tag_evidence::{FileFacts, ReleaseCandidate, TagEvidence, TagRequest, TagSource};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TagConfidence {
    #[default]
    None,
    Low,
    Ambiguous,
    Medium,
    Strong,
}

impl TagConfidence {
    /// The member's name, which the report carries.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Low => "Low",
            Self::Ambiguous => "Ambiguous",
            Self::Medium => "Medium",
            Self::Strong => "Strong",
        }
    }
}

impl fmt::Display for TagConfidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One field's value and where it came from, for the report.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FieldDecision {
    pub value: Option<String>,
    pub source: Option<String>,
}

impl FieldDecision {
    pub fn new(value: Option<&str>, source: Option<&str>) -> Self {
        Self {
            value: value.map(str::to_string),
            source: source.map(str::to_string),
        }
    }
}

/// One key's share of a candidate's distance: the key, its penalty and its weight.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BreakdownPart {
    pub key: String,
    pub penalty: f64,
    pub weight: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScoredCandidate {
    pub candidate: ReleaseCandidate,
    pub distance: f64,
    pub breakdown: Vec<BreakdownPart>,
}

impl ScoredCandidate {
    pub fn new(candidate: ReleaseCandidate, distance: f64, breakdown: Vec<BreakdownPart>) -> Self {
        Self {
            candidate,
            distance,
            breakdown,
        }
    }

    pub fn penalty_of(&self, key: &str) -> Option<f64> {
        self.breakdown.iter().find(|b| b.key == key).map(|b| b.penalty)
    }
}

/// What the chooser decided for one download: the release that won, how sure it is, every
/// candidate it weighed, and what each field is set to and from. Applied to the Song before the
/// file is placed; kept as a report in the fetched-songs log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagPlan {
    pub confidence: TagConfidence,
    pub chosen: Option<ScoredCandidate>,
    pub ranked: Vec<ScoredCandidate>,
    pub evidence: Option<TagEvidence>,
    pub settings: MatchingSettings,
    /// What the release lookup said, once [`with`](Self::with) added it (public so a plan can
    /// be built as a literal).
    pub details: Option<ReleaseDetails>,

    /// The catalog's best hit, for the fill-the-blanks rules that run after the plan.
    pub catalog_best: Option<FullTrackMeta>,

    pub rehearsed: bool,
    pub details_prefetch_hit: bool,
    pub fields: IndexMap<String, FieldDecision>,
    pub stage_seconds: IndexMap<String, f64>,
    pub notes: Vec<String>,
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbfs: Option<f64>,
}

impl TagPlan {
    pub fn empty(evidence: Option<TagEvidence>, settings: MatchingSettings) -> Self {
        Self {
            confidence: TagConfidence::None,
            evidence,
            settings,
            ..Default::default()
        }
    }

    /// What the release lookup said, once [`with`](Self::with) added it.
    pub fn details(&self) -> Option<&ReleaseDetails> {
        self.details.as_ref()
    }

    /// Whether the chosen release may set the album-level fields: a Strong match of any
    /// source, or a Medium one backed by the fingerprint service or the music database.
    pub fn album_from_candidate(&self) -> bool {
        self.chosen.as_ref().is_some_and(|chosen| {
            self.confidence == TagConfidence::Strong
                || (self.confidence == TagConfidence::Medium
                    && matches!(
                        chosen.candidate.source,
                        TagSource::Fingerprint | TagSource::Database
                    ))
        })
    }

    /// Whether the recording is settled, so its ids may be written: the fingerprint named
    /// it, or the match is Strong.
    pub fn recording_confirmed(&self) -> bool {
        self.chosen.as_ref().is_some_and(|chosen| {
            chosen
                .candidate
                .recording_id
                .as_deref()
                .is_some_and(|id| !id.is_empty())
                && (self.confidence == TagConfidence::Strong || self.fingerprint_backed(&chosen.candidate))
        })
    }

    pub fn fingerprint_backed(&self, candidate: &ReleaseCandidate) -> bool {
        candidate.recording_id.as_deref().is_some_and(|id| {
            !id.is_empty()
                && self
                    .evidence
                    .as_ref()
                    .is_some_and(|evidence| evidence.fingerprinted_recording_ids.contains(id))
        })
    }

    /// Add what the release lookup said to the chosen candidate.
    pub fn with(&mut self, details: ReleaseDetails) {
        let Some(chosen) = self.chosen.as_mut() else {
            return;
        };
        let c = &mut chosen.candidate;
        let track = details.track_for(c.recording_id.as_deref()).cloned();

        if c.release_title.is_none() {
            c.release_title = details.title.clone();
        }
        if c.group_title.is_none() {
            c.group_title = details.group_title.clone();
        }
        if c.release_group_id.is_none() {
            c.release_group_id = details.group_id.clone();
        }
        if details.status.is_some() {
            c.status = details.status.clone();
        }
        if details.country.is_some() {
            c.country = details.country.clone();
        }
        if details.date.is_some() {
            c.release_date = details.date.clone();
        }
        if details.group_first_release_date.is_some() {
            c.group_first_release_date = details.group_first_release_date.clone();
        }
        if details.barcode.is_some() {
            c.barcode = details.barcode.clone();
        }
        if details.label.is_some() {
            c.label = details.label.clone();
        }
        if details.catalog_number.is_some() {
            c.catalog_number = details.catalog_number.clone();
        }
        if c.primary_type.is_none() {
            c.primary_type = details.primary_type.clone();
        }
        if c.secondary_types.is_empty() {
            c.secondary_types = details.secondary_types.clone();
        }
        if c.album_artist.is_none() {
            c.album_artist = details.album_artist.clone();
        }
        if c.album_artist_ids.is_empty() {
            c.album_artist_ids = details.album_artist_ids.clone();
        }
        if c.disc_count.is_none() && details.disc_count > 0 {
            c.disc_count = Some(details.disc_count);
        }
        c.is_compilation = c.is_compilation || details.is_compilation();
        if c.release_track_id.is_none() {
            c.release_track_id = track.as_ref().and_then(|t| t.release_track_id.clone());
        }
        if c.track_number.is_none() {
            c.track_number = track.as_ref().and_then(|t| t.position);
        }
        if c.disc_number.is_none() {
            c.disc_number = track.as_ref().map(|t| t.disc_number);
        }
        if c.track_count.is_none() {
            c.track_count = track.as_ref().map(|t| t.track_count).filter(|&count| count > 0);
        }
        if c.isrcs.is_empty() {
            c.isrcs = track.map(|t| t.isrcs).unwrap_or_default();
        }
        self.details = Some(details);
    }

    /// Set the Song from the decision. Album-level fields come from the chosen release only when
    /// [`album_from_candidate`](Self::album_from_candidate), and never the album, track or disc a
    /// request owns; ids when [`recording_confirmed`](Self::recording_confirmed); the code from
    /// the request, else the match, else the file. Anything weaker is left for the old
    /// fill-the-blanks rules that run after this.
    pub fn apply_to(&mut self, song: &mut Song) {
        let request = self.evidence.as_ref().map(|e| e.request.clone());
        let file = self.evidence.as_ref().map(|e| e.file.clone());

        let Some(chosen) = self.chosen.clone() else {
            self.apply_code(song, request.as_ref(), file.as_ref(), None);
            return;
        };
        let c = &chosen.candidate;
        let from = c.source.name();

        if self.recording_confirmed() {
            if let Some(id) = c.recording_id.as_deref().filter(|id| !id.is_empty()) {
                song.music_brainz_recording_id = Some(id.to_string());
                self.set("recordingId", Some(id), from);
            }
            if !c.artist_ids.is_empty() {
                song.music_brainz_artist_ids = c.artist_ids.clone();
            }
            if c.artists.len() > 1 {
                song.artists = c.artists.clone();
            }
            if let Some(primary) = c.primary_artist.as_deref().filter(|p| !p.is_empty()) {
                song.primary_artist = Some(primary.to_string());
            }
            if let Some(acoust_id) = c.fingerprint_id.as_deref().filter(|id| !id.is_empty()) {
                song.acoust_id = Some(acoust_id.to_string());
                self.set("acoustId", Some(acoust_id), from);
            }
        }

        if self.confidence == TagConfidence::Strong && self.settings.tag_from_match {
            if !is_blank(&c.recording_title) {
                song.title = c.recording_title.clone();
                self.set("title", Some(&c.recording_title), from);
            }
            if !is_blank(&c.artist_credit) {
                song.artist = c.artist_credit.clone();
                self.set("artist", Some(&c.artist_credit), from);
            }
        }

        if self.album_from_candidate() {
            // A request that named its album keeps it; the release only confirms it, and lends its
            // facts when it is the same album. One track of a whole album takes them only from
            // that album by name: another edition's date, label and ids would file the track
            // apart from the rest of the album.
            let request_owns = request.as_ref().is_some_and(TagRequest::owns_album);
            let same_album = match request.as_ref().filter(|r| r.whole_album && request_owns) {
                Some(request) => c.album_title().is_some_and(|album| {
                    ReleaseDistance::names_same_album(request.album.as_deref().unwrap_or(""), album)
                }),
                None => !request_owns || chosen.penalty_of("album").is_some_and(|p| p < 1.0),
            };
            if !request_owns {
                if let Some(album) = c.album_title().filter(|a| !a.is_empty()) {
                    song.album = album.to_string();
                    self.set("album", Some(album), from);
                }
                if let Some(album_artist) = c.album_artist.as_deref().filter(|a| !a.is_empty()) {
                    song.album_artist = Some(album_artist.to_string());
                    self.set("albumArtist", Some(album_artist), from);
                }
                song.is_compilation = c.is_compilation;
                if let Some(track) = c.track_number.filter(|&n| n > 0) {
                    song.track = Some(track);
                    self.set("track", Some(&track.to_string()), from);
                }
                if let Some(count) = c.track_count.filter(|&n| n > 0) {
                    song.total_tracks = Some(count);
                }
                if let Some(disc) = c.disc_number.filter(|&n| n > 0) {
                    song.disc_number = Some(disc);
                }
            }
            if same_album {
                // A request that kept its album takes the album's own artist from the release
                // when nothing named one, so one track of a multi-artist album is filed under
                // the album's artist, not its performer.
                if request_owns
                    && song.album_artist.as_deref().is_none_or(is_blank)
                    && let Some(album_artist) = c.album_artist.as_deref().filter(|a| !is_blank(a))
                {
                    song.album_artist = Some(album_artist.to_string());
                    self.set("albumArtist", Some(album_artist), from);
                    if c.is_compilation {
                        song.is_compilation = true;
                    }
                }
                self.apply_dates(song, c, from);
                if let Some(label) = c.label.as_deref().filter(|v| !v.is_empty()) {
                    song.label = Some(label.to_string());
                    self.set("label", Some(label), from);
                }
                if let Some(number) = c.catalog_number.as_deref().filter(|v| !v.is_empty()) {
                    song.catalog_number = Some(number.to_string());
                    self.set("catalogNumber", Some(number), from);
                }
                if let Some(barcode) = c.barcode.as_deref().filter(|v| !v.is_empty()) {
                    song.barcode = Some(barcode.to_string());
                    self.set("barcode", Some(barcode), from);
                }
                if let Some(kind) = c.kind_text().filter(|v| !v.is_empty()) {
                    self.set("releaseType", Some(&kind), from);
                    song.release_type = Some(kind);
                }
                if let Some(status) = c.status.as_deref().filter(|v| !v.is_empty()) {
                    let status = to_lower_invariant(status);
                    self.set("releaseStatus", Some(&status), from);
                    song.release_status = Some(status);
                }
                if let Some(country) = c.country.as_deref().filter(|v| !v.is_empty()) {
                    song.release_country = Some(country.to_string());
                    self.set("releaseCountry", Some(country), from);
                }
                if let Some(discs) = c.disc_count.filter(|&n| n > 0) {
                    song.total_discs = Some(discs);
                    self.set("discCount", Some(&discs.to_string()), from);
                }
                if matches!(c.source, TagSource::Fingerprint | TagSource::Database) {
                    song.music_brainz_release_id = c.release_id.clone();
                    song.music_brainz_release_group_id = c.release_group_id.clone();
                    song.music_brainz_album_title = c.album_title().map(str::to_string);
                    if let Some(track_id) = c.release_track_id.as_deref().filter(|v| !v.is_empty()) {
                        song.music_brainz_release_track_id = Some(track_id.to_string());
                        self.set("releaseTrackId", Some(track_id), from);
                    }
                    if !c.album_artist_ids.is_empty() {
                        song.music_brainz_album_artist_ids = c.album_artist_ids.clone();
                    }
                }
                // Only the cover of the album itself, by its own artist: a release of a near name
                // ("I'm On One" for "I'm On One 3") lends its facts, never its picture.
                let cover_fits = {
                    let release = c.album_title().unwrap_or("");
                    let by = c
                        .album_artist
                        .as_deref()
                        .or(c.primary_artist.as_deref())
                        .unwrap_or(&c.artist_credit);
                    let wanted: Vec<&str> = [
                        song.album_artist.as_deref(),
                        song.primary_artist.as_deref(),
                        Some(song.artist.as_str()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    crate::common::cover_names::release_fits(&song.album, &wanted, release, by)
                };
                if let Some(cover) = c.cover_url.as_deref().filter(|v| !v.is_empty())
                    && cover_fits
                    && song.cover_art_url_large.as_deref().is_none_or(str::is_empty)
                {
                    song.cover_art_url_large = Some(cover.to_string());
                }
                if let Some(genre) = c.genre.as_deref().filter(|v| !v.is_empty())
                    && song.genre.as_deref().is_none_or(str::is_empty)
                {
                    song.genre = Some(genre.to_string());
                }
            }
        } else if self.confidence != TagConfidence::None {
            self.notes.push(format!(
                "the album-level tags were not taken from {}: {} from {from}",
                Self::describe(c),
                self.confidence
            ));
        }

        let matched = matches!(self.confidence, TagConfidence::Strong | TagConfidence::Medium).then_some(c);
        self.apply_code(song, request.as_ref(), file.as_ref(), matched);
    }

    /// What a rehearsal still writes: the facts that do not depend on the match. The
    /// code from the request, else the file, and the fingerprint service's id when verification
    /// already confirmed the recording.
    pub fn apply_rehearsal_to(&mut self, song: &mut Song) {
        self.rehearsed = true;
        let request = self.evidence.as_ref().map(|e| e.request.clone());
        let file = self.evidence.as_ref().map(|e| e.file.clone());
        self.apply_code(song, request.as_ref(), file.as_ref(), None);
        let confirmed = song
            .verification
            .as_ref()
            .filter(|v| v.verdict == VerificationVerdict::Confirmed)
            .and_then(|v| v.acoust_id.clone())
            .filter(|id| !id.is_empty());
        if let Some(id) = confirmed {
            self.set("acoustId", Some(&id), TagSource::Fingerprint.name());
            song.acoust_id = Some(id);
        }
    }

    fn apply_dates(&mut self, song: &mut Song, c: &ReleaseCandidate, from: &str) {
        let year = if self.settings.year_from_original_release {
            c.original_year().or_else(|| c.year())
        } else {
            c.year().or_else(|| c.original_year())
        };
        if let Some(year) = year.filter(|&y| y > 0) {
            song.year = Some(year);
            self.set("year", Some(&year.to_string()), from);
        }
        let original = c
            .group_first_release_date
            .clone()
            .or_else(|| c.original_year().map(|y| y.to_string()));
        if let Some(original) = original.filter(|v| !v.is_empty()) {
            self.set("originalDate", Some(&original), from);
            song.original_date = Some(original);
        }
        if let Some(date) = c.release_date.as_deref().filter(|v| !v.is_empty()) {
            song.release_date = Some(date.to_string());
            self.set("releaseDate", Some(date), from);
        }
    }

    /// The request's code wins, then the match's, then the file's own.
    fn apply_code(
        &mut self,
        song: &mut Song,
        request: Option<&TagRequest>,
        file: Option<&FileFacts>,
        candidate: Option<&ReleaseCandidate>,
    ) {
        let requested = request
            .and_then(|r| r.isrc.as_deref())
            .and_then(SongIdentity::normalize_isrc);
        if let Some(requested) = requested {
            self.set("isrc", Some(&requested), TagSource::Request.name());
            song.isrc = Some(requested);
        } else if let Some(c) = candidate.filter(|c| !c.isrcs.is_empty()) {
            song.isrc = Some(c.isrcs[0].clone());
            self.set("isrc", Some(&c.isrcs[0]), c.source.name());
        } else if let Some(f) = file.filter(|f| !f.isrcs.is_empty()) {
            song.isrc = Some(f.isrcs[0].clone());
            self.set("isrc", Some(&f.isrcs[0]), TagSource::FileTags.name());
        } else if let Some(own) = song.isrc.as_deref().and_then(SongIdentity::normalize_isrc) {
            song.isrc = Some(own);
        }
    }

    fn set(&mut self, field: &str, value: Option<&str>, source: &str) {
        self.fields
            .insert(field.to_string(), FieldDecision::new(value, Some(source)));
    }

    /// `'Album' (album; compilation) 1998` for the notes and the log.
    pub fn describe(c: &ReleaseCandidate) -> String {
        let kind = c.kind_text().map(|kind| format!(" ({kind})")).unwrap_or_default();
        let year = c.year().map(|year| format!(" {year}")).unwrap_or_default();
        format!("'{}'{kind}{year}", c.album_title().unwrap_or("?"))
    }

    /// One line for the log, about the plan applied to `song`.
    pub fn describe_song(&self, song: &Song) -> String {
        let gain = match song.replay_gain_track_gain_db {
            Some(g) => format!("{} dB", signed_fixed(g, 2)),
            None => "none".to_string(),
        };
        let seconds = self
            .stage_seconds
            .get("total")
            .copied()
            .unwrap_or_else(|| self.stage_seconds.values().sum());
        let label = [song.label.as_deref(), song.catalog_number.as_deref()]
            .into_iter()
            .flatten()
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let from = self
            .chosen
            .as_ref()
            .map_or("nothing", |c| c.candidate.source.name());
        let year = song.year.map_or_else(|| "no year".to_string(), |y| y.to_string());
        let label = if label.is_empty() {
            String::new()
        } else {
            format!(", {label}")
        };
        format!(
            "Tagged '{} - {}' as '{}' ({year}{label}) from {from}, {} ({}), {} candidates, gain {gain}, {}s",
            song.artist,
            song.title,
            song.album,
            self.confidence,
            fixed(self.chosen.as_ref().map_or(1.0, |c| c.distance), 3),
            self.ranked.len(),
            fixed(seconds, 1),
        )
    }

    /// What goes into the fetched-songs log and the dashboard: the top candidates and
    /// their biggest penalties, the field table, the notes and the timings.
    pub fn to_report(&self) -> TagReport {
        let candidates = self
            .ranked
            .iter()
            .take(5)
            .map(|scored| {
                let mut penalties: Vec<&BreakdownPart> =
                    scored.breakdown.iter().filter(|b| b.penalty > 0.0).collect();
                // A stable sort, heaviest first: equal shares keep their order.
                penalties.sort_by(|a, b| (b.penalty * b.weight).total_cmp(&(a.penalty * a.weight)));
                let c = &scored.candidate;
                TagReportCandidate {
                    source: c.source.name().to_string(),
                    title: c.recording_title.clone(),
                    album: c.album_title().unwrap_or("").to_string(),
                    kind: c.kind_text(),
                    date: c
                        .release_date
                        .clone()
                        .or_else(|| c.group_first_release_date.clone()),
                    distance: round(scored.distance, 3),
                    biggest_penalties: penalties
                        .into_iter()
                        .take(3)
                        .map(|b| format!("{} {}", b.key, fixed(b.penalty, 2)))
                        .collect(),
                }
            })
            .collect();
        let chosen = self.chosen.as_ref().map(|c| &c.candidate);
        TagReport {
            confidence: self.confidence.name().to_string(),
            distance: self.chosen.as_ref().map(|c| round(c.distance, 3)),
            release_title: chosen.and_then(|c| c.album_title()).map(str::to_string),
            release_id: chosen.and_then(|c| c.release_id.clone()),
            source: chosen.map(|c| c.source.name().to_string()),
            release_date: chosen.and_then(|c| c.release_date.clone()),
            candidates,
            fields: self.fields.clone(),
            notes: self.notes.clone(),
            stage_seconds: self
                .stage_seconds
                .iter()
                .map(|(key, seconds)| (key.clone(), round(*seconds, 2)))
                .collect(),
            rehearsed: self.rehearsed,
            details_prefetch_hit: self.details_prefetch_hit,
            integrated_lufs: self.integrated_lufs,
            true_peak_dbfs: self.true_peak_dbfs,
        }
    }
}

/// What goes into the fetched-songs log and the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TagReport {
    /// The `TagConfidence` name.
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub confidence: String,
    pub distance: Option<f64>,
    pub release_title: Option<String>,
    pub release_id: Option<String>,
    /// The `TagSource` name.
    pub source: Option<String>,
    pub release_date: Option<String>,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub candidates: Vec<TagReportCandidate>,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub fields: IndexMap<String, FieldDecision>,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub notes: Vec<String>,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub stage_seconds: IndexMap<String, f64>,
    pub rehearsed: bool,
    pub details_prefetch_hit: bool,
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbfs: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TagReportCandidate {
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub source: String,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub title: String,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub album: String,
    #[serde(rename = "Type")]
    pub kind: Option<String>,
    pub date: Option<String>,
    pub distance: f64,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub biggest_penalties: Vec<String>,
}
