//! The data half of download verification: the verdict, why it was inconclusive, and the
//! result a download carries. The service (fingerprinting, the AcoustID and MusicBrainz calls,
//! the decision and its helpers) is `octo::services::fingerprint::download_verification_service`.

use std::fmt;

use crate::common::song_identity::SongIdentity;
use crate::fingerprint::acoust_id_client::{AcoustIdLookup, AcoustIdRecording, AcoustIdResult};
use crate::models::domain::song::Song;
use crate::models::download::acquisition_event::AcquisitionHeard;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum VerificationVerdict {
    /// AcoustID identified the file as the track that was asked for, or the ISRC that
    /// was asked for is on the file's tags or on the recording AcoustID named.
    Confirmed,

    /// Nothing could be established: verification off, no key, no fpcalc, AcoustID down or
    /// rate limited, no result above the score threshold, or no AcoustID entry at all. The
    /// file is kept and nothing is remembered.
    ///
    /// The last of those is the one that matters. A legitimately obscure track, which is the
    /// music Soulseek is best at and the reason Octo uses it, has no AcoustID entry. Treating
    /// absence as evidence would make this feature worst exactly where the library is rarest.
    #[default]
    Inconclusive,

    /// The file was identified with confidence and it is a different recording, or it holds
    /// no decodable audio at all. The only verdict that discards a file or writes a denial.
    Mismatch,
}

impl VerificationVerdict {
    /// The verdict's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Confirmed => "Confirmed",
            Self::Inconclusive => "Inconclusive",
            Self::Mismatch => "Mismatch",
        }
    }
}

impl fmt::Display for VerificationVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a verdict was Inconclusive, or, for the last two, why the library sweep asks about a file
/// whose verdict was not. NoEntry, BelowThreshold and SourceDisagreed are questions a person can
/// settle by listening, so only those reach the Review playlist from a download (#47). The sweep
/// also asks about a confident answer, because it never acts on one (#72). Appended, never
/// inserted: the notice queue stores these by name, but order is still what the code reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InconclusiveReason {
    #[default]
    None,
    Disabled,
    NotFingerprinted,
    LookupFailed,
    NoEntry,
    BelowThreshold,
    SourceDisagreed,
    SoundsLikeAnother,
    LengthOff,
    /// AcoustID had never heard the sound, and the song's YouTube preview, one that can be
    /// believed, sounds nothing like the file.
    DiffersFromYouTube,
}

impl InconclusiveReason {
    /// Every member, in declaration order.
    pub const ALL: [InconclusiveReason; 10] = [
        Self::None,
        Self::Disabled,
        Self::NotFingerprinted,
        Self::LookupFailed,
        Self::NoEntry,
        Self::BelowThreshold,
        Self::SourceDisagreed,
        Self::SoundsLikeAnother,
        Self::LengthOff,
        Self::DiffersFromYouTube,
    ];

    /// The reason's name, which the notice queue stores.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Disabled => "Disabled",
            Self::NotFingerprinted => "NotFingerprinted",
            Self::LookupFailed => "LookupFailed",
            Self::NoEntry => "NoEntry",
            Self::BelowThreshold => "BelowThreshold",
            Self::SourceDisagreed => "SourceDisagreed",
            Self::SoundsLikeAnother => "SoundsLikeAnother",
            Self::LengthOff => "LengthOff",
            Self::DiffersFromYouTube => "DiffersFromYouTube",
        }
    }
}

impl fmt::Display for InconclusiveReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What AcoustID said about a downloaded file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VerificationResult {
    pub verdict: VerificationVerdict,
    pub score: f64,
    pub matched_title: Option<String>,
    pub matched_artist: Option<String>,
    pub matched_album: Option<String>,
    pub matched_year: Option<i32>,
    pub recording_id: Option<String>,
    pub deny_reason: String,
    pub tags_authoritative: bool,

    pub reason: InconclusiveReason,

    /// The recording that agreed with the request. Set when Confirmed, and when the right song
    /// and artist were doubted only for their length (LengthOff), to say how long it runs.
    pub r#match: Option<AcoustIdRecording>,

    /// Kept so a person's confirmation can be sent back to AcoustID (#47).
    pub fingerprint: Option<String>,
    pub duration_seconds: i32,

    /// The whole answer, every result and release, so the chooser can weigh them all.
    /// Set whenever the service answered, even below the threshold.
    pub lookup: Option<AcoustIdLookup>,

    /// The service's id for the fingerprint that confirmed the recording.
    pub acoust_id: Option<String>,

    /// The one recording AcoustID proposed below the threshold that agrees with the request on
    /// title, artist and length. The first MusicBrainz id a person's Keep may submit, and None
    /// when there were none or more than one.
    pub candidate_recording_id: Option<String>,

    /// What confirmed or kept the file when it was not the fingerprint's title and artist alone,
    /// in words for the log: an ISRC in the file's tags, or one on the MusicBrainz recording the
    /// fingerprint named, or how far a file is from its YouTube preview. None otherwise.
    pub evidence: Option<String>,

    /// The kind of take the recording named in `recording_id` is, when one was named.
    pub kind: Option<crate::fingerprint::recording_identity::RecordingKind>,

    /// The file's exact length is what chose `recording_id` among takes that share the
    /// fingerprint. The tagger then tags from that recording and no other.
    pub by_length: bool,

    /// The take in `kind` is known: the exact length decided it, or every recording that agreed
    /// with the request is that take.
    pub kind_certain: bool,

    /// `recording_id` was the only recording of the fingerprint that agreed with the song, so
    /// no other take could be the file.
    pub sole: bool,

    /// The score an AcoustID answer had to reach to count, in 0..1, as the decision used it.
    /// 0 when no answer was weighed.
    pub threshold: f64,
}

/// How many of the other recordings AcoustID heard a check line keeps.
pub const MAX_HEARD: usize = 3;

/// A score in 0..1 as a whole percent, rounded down so it never reads as more than it was.
pub fn whole_percent(fraction: f64) -> i32 {
    (fraction * 100.0 + 1e-9).floor().clamp(0.0, 100.0) as i32
}

/// A threshold in 0..1 as the whole percent it was set as.
pub fn threshold_percent(fraction: f64) -> i32 {
    (fraction * 100.0).round().clamp(0.0, 100.0) as i32
}

impl VerificationResult {
    /// The best score among AcoustID's results; None when it did not answer or listed none.
    pub fn best_score(&self) -> Option<f64> {
        self.lookup
            .as_ref()?
            .results
            .iter()
            .map(|r| r.score)
            .reduce(f64::max)
    }

    /// Whether the verdict names a recording (a title or an artist).
    pub fn names_recording(&self) -> bool {
        self.matched_title.as_deref().is_some_and(|t| !t.is_empty())
            || self.matched_artist.as_deref().is_some_and(|a| !a.is_empty())
    }

    /// The score to show beside the verdict: its own when it names a recording, otherwise the
    /// best of AcoustID's results. None when AcoustID gave no score at all.
    pub fn shown_score(&self) -> Option<f64> {
        if self.names_recording() && self.score > 0.0 {
            Some(self.score)
        } else {
            self.best_score()
        }
    }

    /// What AcoustID matched, "Artist - Title": the recording the verdict names. None when it
    /// named none, even when it heard something below the threshold.
    pub fn matched_text(&self) -> Option<String> {
        self.names_recording().then(|| {
            format!(
                "{} - {}",
                self.matched_artist.as_deref().unwrap_or(""),
                self.matched_title.as_deref().unwrap_or("")
            )
        })
    }

    /// The best recording AcoustID heard, "Artist - Title", when the verdict names none: what
    /// came closest without counting as a match. None when the verdict names a recording or
    /// AcoustID heard nothing.
    pub fn closest_text(&self) -> Option<String> {
        if self.names_recording() {
            return None;
        }
        self.ranked_heard(|_| false)
            .into_iter()
            .next()
            .map(|heard| format!("{} - {}", heard.artist, heard.title))
    }

    /// The recordings AcoustID heard other than the one the verdict names, best first, at most
    /// [`MAX_HEARD`]. A result holding the named recording is left out whole.
    pub fn heard(&self) -> Vec<AcquisitionHeard> {
        let named_id = self.recording_id.as_deref().filter(|id| !id.is_empty());
        let named_key = self.names_recording().then(|| {
            heard_key(
                self.matched_artist.as_deref().unwrap_or(""),
                self.matched_title.as_deref().unwrap_or(""),
            )
        });
        let mut heard = self.ranked_heard(|result| {
            result.recordings.iter().any(|recording| {
                named_id.is_some_and(|id| crate::common::dotnet::eq_ignore_case(id, &recording.recording_id))
                    || named_key
                        .as_ref()
                        .is_some_and(|key| *key == heard_key(&recording.artist_credit(), &recording.title))
            })
        });
        heard.truncate(MAX_HEARD);
        heard
    }

    /// One recording per result, best score first (equal scores keep their order): the one
    /// most submissions tie the audio to, the first listed on a tie. Results that name no
    /// recording, and those `skip` refuses, are left out, and an artist and title already
    /// listed is not listed again.
    fn ranked_heard(&self, skip: impl Fn(&AcoustIdResult) -> bool) -> Vec<AcquisitionHeard> {
        let Some(lookup) = &self.lookup else {
            return Vec::new();
        };
        let mut results: Vec<&AcoustIdResult> = lookup
            .results
            .iter()
            .filter(|r| !r.recordings.is_empty() && !skip(r))
            .collect();
        results.sort_by(|a, b| b.score.total_cmp(&a.score));
        let mut keys: Vec<String> = Vec::new();
        let mut heard = Vec::new();
        for result in results {
            let Some(recording) = result.recordings.iter().rev().max_by_key(|r| r.sources) else {
                continue;
            };
            let artist = recording.artist_credit();
            let key = heard_key(&artist, &recording.title);
            if keys.contains(&key) {
                continue;
            }
            keys.push(key);
            heard.push(AcquisitionHeard {
                title: recording.title.clone(),
                artist,
                score: whole_percent(result.score),
            });
        }
        heard
    }

    /// What AcoustID said about the file, in plain words with its numbers: "AcoustID: the same
    /// recording, 92% sure (needs 85%)", "AcoustID says this is 'Artist - Title', 97% sure
    /// (needs 85%)", "AcoustID: no match above 85% (best 61%)", "AcoustID has no entry for this
    /// sound". What else stood behind the verdict (an ISRC) follows it. None when AcoustID was
    /// not asked and nothing else decided, and for a file with no audio in it.
    pub fn acoust_id_text(&self) -> Option<String> {
        let needs = threshold_percent(self.threshold);
        let sure = |score: f64| format!("{}% sure (needs {needs}%)", whole_percent(score));
        let by_acoust_id = self.verdict == VerificationVerdict::Confirmed && self.r#match.is_some();
        let said = match (self.verdict, self.reason) {
            (VerificationVerdict::Mismatch, _) if !self.names_recording() => return None,
            // A take `describe` already names is not named twice.
            (VerificationVerdict::Mismatch, _)
                if self.deny_reason.starts_with("is a live recording")
                    && self.kind.is_none_or(|kind| {
                        kind == crate::fingerprint::recording_identity::RecordingKind::Studio
                    }) =>
            {
                Some(format!(
                    "AcoustID says this is a live recording of {}, {}",
                    self.describe(),
                    sure(self.score)
                ))
            }
            (VerificationVerdict::Mismatch, _) => Some(format!(
                "AcoustID says this is {}, {}",
                self.describe(),
                sure(self.score)
            )),
            _ if by_acoust_id => Some(format!("AcoustID: the same recording, {}", sure(self.score))),
            (VerificationVerdict::Inconclusive, InconclusiveReason::LengthOff) => {
                let runs = self
                    .r#match
                    .as_ref()
                    .and_then(|m| m.duration_seconds)
                    .filter(|seconds| *seconds > 0);
                Some(match runs {
                    Some(seconds) => format!(
                        "AcoustID: the same song, {}, but that recording runs {}:{:02}",
                        sure(self.score),
                        seconds / 60,
                        seconds % 60
                    ),
                    None => format!(
                        "AcoustID: the same song, {}, but not at this length",
                        sure(self.score)
                    ),
                })
            }
            (VerificationVerdict::Inconclusive, _) if self.names_recording() => Some(format!(
                "AcoustID says this is {}, {}",
                self.describe(),
                sure(self.score)
            )),
            _ => self.unsettled_text(needs),
        };
        match (said, self.evidence.as_deref().filter(|e| !e.is_empty())) {
            (Some(said), Some(evidence)) if by_acoust_id => Some(format!("{said}, and {evidence}")),
            (Some(said), Some(evidence)) => Some(format!("{said}, but {evidence}")),
            (said, evidence) => said.or_else(|| evidence.map(str::to_string)),
        }
    }

    /// What AcoustID's answer was when it named nothing that decided: no answer, no entry, or
    /// nothing at the threshold.
    fn unsettled_text(&self, needs: i32) -> Option<String> {
        let Some(lookup) = &self.lookup else {
            return match self.reason {
                InconclusiveReason::LookupFailed => Some("AcoustID did not answer".to_string()),
                InconclusiveReason::NotFingerprinted => {
                    Some("AcoustID was not asked: the file could not be fingerprinted".to_string())
                }
                _ => None,
            };
        };
        let Some(best) = lookup.results.iter().map(|r| r.score).reduce(f64::max) else {
            return Some("AcoustID has no entry for this sound".to_string());
        };
        if best >= self.threshold {
            return Some(format!(
                "AcoustID knows this sound, {}% sure, but no recording is linked to it",
                whole_percent(best)
            ));
        }
        Some(format!(
            "AcoustID: no match above {needs}% (best {}%)",
            whole_percent(best)
        ))
    }

    /// `VerificationResult.Inconclusive`: verification is off.
    pub fn inconclusive() -> Self {
        Self {
            reason: InconclusiveReason::Disabled,
            ..Default::default()
        }
    }

    pub fn needs_review(&self) -> bool {
        self.verdict == VerificationVerdict::Inconclusive
            && matches!(
                self.reason,
                InconclusiveReason::NoEntry
                    | InconclusiveReason::BelowThreshold
                    | InconclusiveReason::SourceDisagreed
                    | InconclusiveReason::LengthOff
            )
    }

    /// Whether a song's album IS the MusicBrainz release its fingerprint matched, so the ids and
    /// artwork that belong to that release describe the album the tags name. A download tagged
    /// with a compilation's name must not get the original album's cover or group id.
    pub fn album_is_from_release(song: &Song) -> bool {
        match song.music_brainz_album_title.as_deref() {
            Some(title) if !crate::common::dotnet::is_blank(title) => {
                SongIdentity::key(&song.album) == SongIdentity::key(title)
            }
            _ => false,
        }
    }

    pub fn describe(&self) -> String {
        let artist = self.matched_artist.as_deref().unwrap_or("");
        let title = self.matched_title.as_deref().unwrap_or("");
        let take = self
            .kind
            .filter(|kind| *kind != crate::fingerprint::recording_identity::RecordingKind::Studio);
        match take {
            _ if artist.is_empty() && title.is_empty() => "a different recording".to_string(),
            Some(kind) => format!("{} of '{artist} - {title}'", kind.described()),
            None => format!("'{artist} - {title}'"),
        }
    }

    /// Record what a confirmed match proved, and overwrite the song's name from it when
    /// tagging from MusicBrainz is on.
    ///
    /// The ids are written whenever the match is confirmed. An id says what the file IS and
    /// changes nothing a person reads, and it is what lets every later pass skip identifying
    /// the same file again (#48). The release id stays in memory for the cover lookup and is
    /// never written: Navidrome groups albums by MUSICBRAINZ_ALBUMID before the album name, so
    /// one track carrying it beside another without it would split an album in two.
    ///
    /// The name overwrite is unconditional where the Deezer fill is conditional. That one fills
    /// only what is missing, because a peer's own tags beat nothing. A confirmed fingerprint
    /// match beats the peer, which is the entire point of the setting.
    pub fn apply_tags_to(&self, song: &mut Song) {
        if self.verdict != VerificationVerdict::Confirmed {
            return;
        }

        if let Some(id) = self.recording_id.as_deref().filter(|id| !id.is_empty()) {
            song.music_brainz_recording_id = Some(id.to_string());
        }
        if let Some(found) = &self.r#match {
            if found.credits.len() > 1 {
                song.artists = found.credits.iter().map(|credit| credit.name.clone()).collect();
            }
            if found.credits.len() == 1
                && let Some(artist_id) = found.credits[0].artist_id.as_deref().filter(|id| !id.is_empty())
            {
                song.music_brainz_artist_ids = vec![artist_id.to_string()];
            }
            if let Some(primary) = found.primary_artist().filter(|primary| !primary.is_empty()) {
                song.primary_artist = Some(primary.to_string());
            }
            song.music_brainz_release_id = found.release.as_ref().and_then(|r| r.release_id.clone());
            song.music_brainz_release_group_id =
                found.release.as_ref().and_then(|r| r.release_group_id.clone());
            song.music_brainz_album_title = found.album_title.clone();
        }

        if !self.tags_authoritative {
            return;
        }
        if let Some(title) = self.matched_title.as_deref().filter(|v| !v.is_empty()) {
            song.title = title.to_string();
        }
        if let Some(artist) = self.matched_artist.as_deref().filter(|v| !v.is_empty()) {
            song.artist = artist.to_string();
        }
        let matched_album = self.matched_album.as_deref().filter(|v| !v.is_empty());
        if let Some(album) = matched_album {
            song.album = album.to_string();
        }
        if let Some(year) = self.matched_year.filter(|&year| year > 0) {
            song.year = Some(year);
        }

        if let (Some(release), Some(_)) = (
            self.r#match.as_ref().and_then(|m| m.release.as_ref()),
            matched_album,
        ) {
            if let Some(track) = release.track_number.filter(|&n| n > 0) {
                song.track = Some(track);
            }
            if let Some(count) = release.track_count.filter(|&n| n > 0) {
                song.total_tracks = Some(count);
            }
            if let Some(disc) = release.disc_number.filter(|&n| n > 0) {
                song.disc_number = Some(disc);
            }
            if let Some(album_artist) = release.album_artist.as_deref().filter(|v| !v.is_empty()) {
                song.album_artist = Some(album_artist.to_string());
            }
            song.is_compilation = release.is_compilation;
        }
    }
}

/// An artist and title compared the way song names are: case, accents and punctuation aside.
fn heard_key(artist: &str, title: &str) -> String {
    format!("{}|{}", SongIdentity::key(artist), SongIdentity::key(title))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::acoust_id_client::{AcoustIdCredit, AcoustIdRelease};

    #[test]
    fn needs_review_only_for_questions_a_person_can_settle() {
        let cases = [
            (InconclusiveReason::Disabled, false),
            (InconclusiveReason::NotFingerprinted, false),
            (InconclusiveReason::LookupFailed, false),
            (InconclusiveReason::NoEntry, true),
            (InconclusiveReason::BelowThreshold, true),
            (InconclusiveReason::SourceDisagreed, true),
            (InconclusiveReason::LengthOff, true),
            (InconclusiveReason::SoundsLikeAnother, false),
        ];
        for (reason, expected) in cases {
            let result = VerificationResult {
                reason,
                ..Default::default()
            };
            assert_eq!(result.needs_review(), expected, "{reason}");
        }
    }

    #[test]
    fn defaults_are_an_inconclusive_result_with_no_reason() {
        let result = VerificationResult::default();
        assert_eq!(result.verdict, VerificationVerdict::Inconclusive);
        assert_eq!(result.reason, InconclusiveReason::None);
        assert_eq!(result.deny_reason, "");
        assert_eq!(
            VerificationResult::inconclusive().reason,
            InconclusiveReason::Disabled
        );
        assert_eq!(result.describe(), "a different recording");
        let named = VerificationResult {
            matched_artist: Some("Massive Attack".into()),
            matched_title: Some("Teardrop".into()),
            ..Default::default()
        };
        assert_eq!(named.describe(), "'Massive Attack - Teardrop'");
    }

    /// Applying tags from a confirmed result: when its tags are not authoritative it still
    /// records the recording id, and when they are it takes the release's track and album
    /// artist. The result is the one the service builds for a confirmation (the decision itself
    /// is tested with the service).
    #[test]
    fn apply_tags_to_records_ids_and_takes_the_release_only_when_authoritative() {
        let release = AcoustIdRelease {
            release_id: Some("rel-1".into()),
            release_group_id: Some("rg-1".into()),
            title: Some("Mezzanine".into()),
            year: Some(1998),
            track_number: Some(3),
            track_count: Some(11),
            disc_number: Some(1),
            album_artist: Some("Massive Attack".into()),
            ..Default::default()
        };
        let found = AcoustIdRecording {
            credits: vec![
                AcoustIdCredit::new("Bizarrap", Some("a1"), " & "),
                AcoustIdCredit::new("Rauw Alejandro", Some("a2"), ""),
            ],
            release: Some(release),
            ..AcoustIdRecording::new(
                "rec-1",
                "Teardrop",
                ["Bizarrap", "Rauw Alejandro"],
                Some("Mezzanine"),
                Some(1998),
            )
        };
        let confirmed = |authoritative: bool| VerificationResult {
            verdict: VerificationVerdict::Confirmed,
            score: 0.99,
            matched_title: Some(found.title.clone()),
            matched_artist: Some(found.artist_credit()),
            matched_album: found.album_title.clone(),
            matched_year: found.year,
            recording_id: Some(found.recording_id.clone()),
            tags_authoritative: authoritative,
            r#match: Some(found.clone()),
            ..Default::default()
        };

        let mut song = Song {
            title: "peer title".into(),
            artist: "a".into(),
            ..Default::default()
        };
        confirmed(false).apply_tags_to(&mut song);
        assert_eq!(song.title, "peer title");
        assert_eq!(song.music_brainz_recording_id.as_deref(), Some("rec-1"));
        assert_eq!(song.primary_artist.as_deref(), Some("Bizarrap"));
        assert_eq!(song.artists, ["Bizarrap", "Rauw Alejandro"]);
        assert_eq!(song.music_brainz_release_id.as_deref(), Some("rel-1"));
        assert_eq!(song.music_brainz_release_group_id.as_deref(), Some("rg-1"));
        assert_eq!(song.track, None);
        assert_eq!(song.album_artist, None);

        let mut song = Song::default();
        confirmed(true).apply_tags_to(&mut song);
        assert_eq!(song.title, "Teardrop");
        assert_eq!(song.album, "Mezzanine");
        assert_eq!(song.year, Some(1998));
        assert_eq!(song.track, Some(3));
        assert_eq!(song.total_tracks, Some(11));
        assert_eq!(song.disc_number, Some(1));
        assert_eq!(song.album_artist.as_deref(), Some("Massive Attack"));

        let mut song = Song {
            title: "peer title".into(),
            ..Default::default()
        };
        VerificationResult {
            verdict: VerificationVerdict::Mismatch,
            ..confirmed(true)
        }
        .apply_tags_to(&mut song);
        assert_eq!(song.title, "peer title");
        assert_eq!(song.music_brainz_recording_id, None);
    }

    #[test]
    fn album_is_from_release_compares_the_album_keys() {
        let mut song = Song {
            album: "Mezzanine (Deluxe)".into(),
            ..Default::default()
        };
        assert!(!VerificationResult::album_is_from_release(&song));
        song.music_brainz_album_title = Some("  ".into());
        assert!(!VerificationResult::album_is_from_release(&song));
        song.music_brainz_album_title = Some("mezzanine".into());
        song.album = "Mezzanine".into();
        assert!(VerificationResult::album_is_from_release(&song));
    }
}
