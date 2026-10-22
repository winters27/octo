//! The download verification service. Its types (the verdict, the reason, the result) are
//! `octo_core::fingerprint::verification`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::common::song_versions::strip_versions;
use octo_core::common::version_gap::{self, Against};
use octo_core::fingerprint::recording_identity::{self, RecordingFacts, RecordingKind, Takes};
use octo_core::fingerprint::{
    AcoustIdLookup, AcoustIdRecording, AcoustIdResult, InconclusiveReason, TrackMatchComparer,
    VerificationResult, VerificationVerdict,
};
use octo_core::settings::SettingsStore;
use octo_core::tagging::FingerprintVerifier;
use octo_media::audio::{AudioFingerprinter, FingerprintOutcome, SpectrumAnalyzer, SpectrumReport};
use octo_media::tags::{TagFile, tag_writer_extras};
use tracing::{debug, info, warn};

use super::acoust_id_client::AcoustIdClient;
use super::acoust_id_rate_limiter::AcoustIdRateLimiter;
use super::music_brainz_client::{MusicBrainzClient, MusicBrainzError};
use super::preview_check::{self, PreviewOutcome, PreviewRequest, PreviewSource};

/// The ISRCs MusicBrainz listed per recording id, with ids compared ignoring case.
pub type RecordingIsrcs = HashMap<String, Vec<String>>;

/// What MusicBrainz says each recording is, by lowercased recording id.
pub type FactsById = HashMap<String, RecordingFacts>;

/// Asks what a finished download actually contains, rather than what its name and advertised
/// length claim.
///
/// Every failure path accepts the file. That is the correct trade and also the dominant risk:
/// a broken key, a missing binary, a mishandled gzip and a parse error all look exactly like
/// "everything is fine", which is why the pieces below log refusals at Warning.
pub struct DownloadVerificationService {
    fingerprinter: Arc<AudioFingerprinter>,
    client: Arc<AcoustIdClient>,
    /// The Soulseek settings, read at every use.
    settings: Arc<SettingsStore>,
    music_brainz: Option<Arc<MusicBrainzClient>>,
    spectrum: Option<Arc<SpectrumAnalyzer>>,
}

impl DownloadVerificationService {
    /// How many of the recordings a fingerprint named are asked for their ISRCs. Each is a
    /// MusicBrainz call a second apart, spent only when the request carried an ISRC and the
    /// recordings' names disagreed with it.
    pub const MAX_ISRC_LOOKUPS: usize = 3;

    /// How many recordings MusicBrainz is asked about when AcoustID's answer cannot tell their
    /// kinds apart. Each is a call a second apart.
    pub const MAX_FACT_LOOKUPS: usize = 3;

    pub fn new(
        fingerprinter: Arc<AudioFingerprinter>,
        client: Arc<AcoustIdClient>,
        settings: Arc<SettingsStore>,
        music_brainz: Option<Arc<MusicBrainzClient>>,
        spectrum: Option<Arc<SpectrumAnalyzer>>,
    ) -> Self {
        Self {
            fingerprinter,
            client,
            settings,
            music_brainz,
            spectrum,
        }
    }

    /// Whether a file that claims to be lossless really is, by its spectrum. Unknown, and no
    /// work at all, when the check is off, the file does not claim to be lossless, or ffmpeg
    /// cannot say. Separate from [`Self::verify`] because the answer never rejects a file: it
    /// only decides which of two right songs is kept.
    pub async fn check_lossless(
        &self,
        path: &str,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
    ) -> SpectrumReport {
        let settings = self.settings.current().soulseek.clone();
        let spectrum = match &self.spectrum {
            Some(spectrum) if settings.detect_transcodes && SpectrumAnalyzer::claims_lossless(path) => {
                spectrum
            }
            _ => return SpectrumReport::unknown("not checked", 0),
        };

        let report = spectrum
            .analyze(
                Path::new(path),
                settings.effective_transcode_check_timeout_seconds(),
            )
            .await;
        let (artist, title) = (requested_artist.unwrap_or(""), requested_title.unwrap_or(""));
        if report.is_likely_lossy() {
            let format = dotnet::to_upper_invariant(get_extension(path).trim_start_matches('.'));
            warn!("the {format} for '{artist} - {title}' is {}", report.describe());
        } else {
            // Logged when it passes too, for the same reason a confirmation is: silence on
            // success looks exactly like a check that never ran.
            info!("spectrum of '{artist} - {title}': {}", report.describe());
        }
        report
    }

    /// The takes a download asking for this title and album may be: the ones they name, never
    /// a cover, a karaoke backing or an instrumental while Skip covers and instrumentals is on.
    pub fn takes_for(&self, title: Option<&str>, album: Option<&str>) -> Takes {
        let never = if self.settings.current().soulseek.skip_covers_and_instrumentals {
            std::collections::BTreeSet::from(octo_core::common::skipped_takes::SKIPPED)
        } else {
            std::collections::BTreeSet::new()
        };
        Takes::for_request(title, album, &never)
    }

    /// [`Self::takes_for`] a download request: one replacing a library file keeps the take its
    /// title names, since that file is the person's own (Skip covers, karaoke and instrumentals
    /// is for songs Octo brings in).
    pub fn takes_for_routing(&self, routing: &octo_core::soulseek::SoulseekRouting) -> Takes {
        let (title, album) = (routing.title.as_deref(), routing.album.as_deref());
        if routing.replaces_library_file {
            Takes::for_request(title, album, &std::collections::BTreeSet::new())
        } else {
            self.takes_for(title, album)
        }
    }

    /// Whether a download sounds like the song's YouTube preview, for a verdict that leaves the
    /// question to it ([`preview_check::wanted`]). Unavailable, and no work, while Octo is not
    /// policing downloads or the verdict settled the file.
    pub async fn hear_against_preview(
        &self,
        source: &dyn PreviewSource,
        path: &str,
        request: &PreviewRequest<'_>,
        verdict: &VerificationResult,
        length_known: bool,
    ) -> PreviewOutcome {
        if !self.remembers_rejections() || !preview_check::wanted(verdict, length_known) {
            return PreviewOutcome::NotAsked;
        }
        let timeout = self
            .settings
            .current()
            .soulseek
            .effective_fingerprint_timeout_seconds();
        preview_check::check(source, &self.fingerprinter, Path::new(path), request, timeout).await
    }

    /// AcoustID can answer at all.
    pub fn has_api_key(&self) -> bool {
        !dotnet::is_blank(&self.settings.current().soulseek.acoust_id_api_key)
    }

    /// The user asked Octo to police what peers deliver. Governs the deny-list on its own,
    /// because the duration check needs no API key and its verdicts are just as durable.
    pub fn remembers_rejections(&self) -> bool {
        self.settings.current().soulseek.verify_downloads
    }

    /// A lookup can actually happen. Split from the above deliberately, in the shape
    /// LastFmService uses for HasApiKey/IsRadioEnabled: collapsing them would mean a user who
    /// switches verification on without a key gets no bad-peer memory either, and the
    /// duration check quietly keeps re-downloading the same wrong file.
    pub fn is_fingerprinting_enabled(&self) -> bool {
        self.remembers_rejections() && self.has_api_key()
    }

    /// No cancellation token, deliberately. There must be no way for a caller who has already
    /// given up to skip verification on a file that is about to enter the library.
    ///
    /// `takes`: which takes of the song the file may be. A download that did not ask for a live
    /// take, an instrumental or a karaoke backing refuses a file that is one; [`Takes::Any`] for
    /// songs already in the library, which may be such a take on purpose.
    pub async fn verify(
        &self,
        path: &str,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        requested_isrc: Option<&str>,
        takes: &Takes,
    ) -> VerificationResult {
        self.verify_on(
            path,
            requested_artist,
            requested_title,
            requested_isrc,
            takes,
            false,
        )
        .await
    }

    /// [`Self::verify`] for work nobody waits on (the library sweep, a song's own file heard
    /// before its search): its MusicBrainz questions go in the background lane, behind every
    /// download's.
    pub async fn verify_in_background(
        &self,
        path: &str,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
    ) -> VerificationResult {
        self.verify_on(path, requested_artist, requested_title, None, &Takes::Any, true)
            .await
    }

    async fn verify_on(
        &self,
        path: &str,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        requested_isrc: Option<&str>,
        takes: &Takes,
        background: bool,
    ) -> VerificationResult {
        if !self.remembers_rejections() {
            return VerificationResult::inconclusive();
        }

        // What the file's own tags say it is, when the request named an ISRC to hold them to.
        // A header read, and it needs no API key: with no key it is the only question asked.
        let isrc = requested_isrc.and_then(SongIdentity::normalize_isrc);
        let tagged = if isrc.is_none() {
            Vec::new()
        } else {
            Self::read_isrcs(path)
        };
        let tagged_match = isrc.as_ref().is_some_and(|isrc| tagged.contains(isrc));

        let verdict = if self.has_api_key() {
            self.identify(
                path,
                requested_artist,
                requested_title,
                isrc.as_deref(),
                tagged_match,
                takes,
                background,
            )
            .await
        } else {
            VerificationResult::inconclusive()
        };

        let verdict = Self::with_tagged_isrc(verdict, isrc.as_deref(), tagged_match);
        let verdict =
            if verdict.verdict == VerificationVerdict::Inconclusive && matches!(takes, Takes::Only(_)) {
                self.with_tagged_recording(verdict, path, requested_title, background)
                    .await
            } else {
                verdict
            };
        let (artist, title) = (requested_artist.unwrap_or(""), requested_title.unwrap_or(""));
        let isrc_text = isrc.as_deref().unwrap_or("");
        match (&verdict.verdict, &verdict.evidence) {
            (VerificationVerdict::Confirmed, Some(evidence)) => {
                info!("confirmed '{artist} - {title}' by ISRC {isrc_text}: {evidence}")
            }
            _ if isrc.is_some() && !tagged.is_empty() && !tagged_match => {
                // Not a rejection: a re-release or a remaster is often given a new code.
                info!(
                    "the file for '{artist} - {title}' is tagged ISRC {}, not the {isrc_text} asked for; \
                     that alone decides nothing",
                    tagged.join(", ")
                )
            }
            _ => {}
        }
        verdict
    }

    /// A file whose own tags carry the ISRC that was asked for is that recording, when nothing
    /// better could be established: AcoustID off, down, without an entry or below the threshold.
    /// A confident fingerprint of something else is not overruled by a tag, since tags are
    /// copied and audio is not, and neither is a fingerprint that named a recording whose ISRCs
    /// were not found, or the right recording at a length far from the file's (those go to a
    /// person).
    pub fn with_tagged_isrc(
        verdict: VerificationResult,
        isrc: Option<&str>,
        tagged_match: bool,
    ) -> VerificationResult {
        if !tagged_match
            || verdict.verdict != VerificationVerdict::Inconclusive
            || verdict.reason == InconclusiveReason::SourceDisagreed
            || verdict.reason == InconclusiveReason::LengthOff
        {
            return verdict;
        }
        VerificationResult {
            verdict: VerificationVerdict::Confirmed,
            reason: InconclusiveReason::None,
            evidence: Some(format!(
                "the file's own tags carry the requested ISRC {}",
                isrc.unwrap_or("")
            )),
            ..verdict
        }
    }

    /// A download AcoustID could not settle whose own tags name a MusicBrainz recording of the
    /// same song in another version ("Let's Get Lost (Teklife remix)" in a file asked for as the
    /// Bear//Face remix) is that other version: a peer's tagger read the audio once, and nothing
    /// heard since says otherwise. A recording of another song, or one the database does not
    /// answer for, changes nothing.
    async fn with_tagged_recording(
        &self,
        verdict: VerificationResult,
        path: &str,
        requested_title: Option<&str>,
        background: bool,
    ) -> VerificationResult {
        let (Some(music_brainz), Some(requested_title)) = (&self.music_brainz, requested_title) else {
            return verdict;
        };
        let Some(tagged) = tag_writer_extras::read_facts(path, true).recording_id else {
            return verdict;
        };
        let timeout = Duration::from_secs(
            self.settings
                .current()
                .soulseek
                .effective_acoust_id_timeout_seconds()
                .max(1) as u64
                + 2,
        );
        let facts =
            match tokio::time::timeout(timeout, music_brainz.recording_facts(&tagged, background)).await {
                Ok(Ok(Some(facts))) => facts,
                _ => return verdict,
            };
        Self::settle_by_tagged_recording(verdict, requested_title, &facts)
    }

    /// [`Self::with_tagged_recording`]'s decision, apart from the I/O.
    pub fn settle_by_tagged_recording(
        verdict: VerificationResult,
        requested_title: &str,
        tagged: &RecordingFacts,
    ) -> VerificationResult {
        if !same_song_any_version(requested_title, &tagged.title) {
            return verdict;
        }
        let Some(gap) = version_gap::version_gap(requested_title, &tagged.title, Against::Recording) else {
            return verdict;
        };
        let kind = recording_identity::kind_of(
            &AcoustIdRecording::new(
                &tagged.recording_id,
                &tagged.title,
                Vec::<String>::new(),
                None,
                None,
            ),
            Some(tagged),
        );
        VerificationResult {
            verdict: VerificationVerdict::Mismatch,
            matched_title: Some(tagged.title.clone()),
            recording_id: Some(tagged.recording_id.clone()),
            deny_reason: format!(
                "is another version by its own tags, '{}': {}",
                tagged.title,
                gap.words()
            ),
            evidence: Some(format!(
                "the file's own tags name the MusicBrainz recording '{}'",
                tagged.title
            )),
            kind: Some(kind),
            ..verdict
        }
    }

    /// A file AcoustID names as another artist's song of the same title, at a length well apart
    /// from the file's, while the file runs exactly the length the request's catalog entry gives:
    /// AcoustID has tied this sound to the wrong song ("Superstar - Let's Get Lost", 3:55, for
    /// G-Eazy's Bear//Face remix, 4:02). Kept and asked about rather than thrown away. Any other
    /// verdict, a recording of another version, or a request of unknown length stays as it was.
    pub fn settle_stray_link(
        verdict: VerificationResult,
        requested_title: &str,
        expected_seconds: Option<i32>,
        file_ms: Option<i64>,
    ) -> VerificationResult {
        if verdict.verdict != VerificationVerdict::Mismatch
            || verdict.kind.is_some_and(|kind| kind != RecordingKind::Studio)
        {
            return verdict;
        }
        let (Some(expected), Some(file_ms)) =
            (expected_seconds.filter(|s| *s > 0), file_ms.filter(|ms| *ms > 0))
        else {
            return verdict;
        };
        let Some(named) = verdict.lookup.as_ref().and_then(|lookup| {
            lookup
                .results
                .iter()
                .flat_map(|result| &result.recordings)
                .find(|r| {
                    verdict
                        .recording_id
                        .as_deref()
                        .is_some_and(|id| dotnet::eq_ignore_case(id, &r.recording_id))
                })
        }) else {
            return verdict;
        };
        let same_title = SongIdentity::same_title(&strip_versions(requested_title), &named.title, None)
            .is_same()
            || SongIdentity::same_title(requested_title, &named.title, None).verdict
                != octo_core::common::SongVerdict::Different;
        let Some(named_ms) = named
            .duration_ms
            .or_else(|| named.duration_seconds.map(|s| i64::from(s) * 1000))
            .filter(|ms| *ms > 0)
        else {
            return verdict;
        };
        let fits_request = (file_ms - i64::from(expected) * 1000).abs() <= STRAY_FITS_MS;
        let named_apart = (named_ms - file_ms).abs() >= STRAY_APART_MS;
        if !same_title || !fits_request || !named_apart {
            return verdict;
        }
        VerificationResult {
            verdict: VerificationVerdict::Inconclusive,
            reason: InconclusiveReason::SourceDisagreed,
            deny_reason: String::new(),
            evidence: Some(format!(
                "that recording runs {}:{:02} and this file runs the song's own {}:{:02}",
                named_ms / 60_000,
                (named_ms / 1000) % 60,
                expected / 60,
                expected % 60
            )),
            ..verdict
        }
    }

    /// The fingerprint and the AcoustID lookup, and the ISRC check of the recordings it named.
    /// The question `verify` asked alone before ISRCs were evidence.
    async fn identify(
        &self,
        path: &str,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        isrc: Option<&str>,
        tagged_match: bool,
        takes: &Takes,
        background: bool,
    ) -> VerificationResult {
        let settings = self.settings.current().soulseek.clone();
        let fingerprint = self
            .fingerprinter
            .fingerprint(
                Path::new(path),
                settings.effective_fingerprint_seconds(),
                settings.effective_fingerprint_timeout_seconds(),
            )
            .await;

        if fingerprint.outcome == FingerprintOutcome::Undecodable {
            return VerificationResult {
                verdict: VerificationVerdict::Mismatch,
                deny_reason: "delivered a file with no decodable audio".to_string(),
                ..Default::default()
            };
        }

        let Some(print) = fingerprint
            .fingerprint
            .clone()
            .filter(|f| fingerprint.outcome == FingerprintOutcome::Ok && !f.is_empty())
        else {
            return reason(InconclusiveReason::NotFingerprinted);
        };

        // TagLib's duration, not fpcalc's. -length pins how much audio is fingerprinted, and
        // staking a rejection on whether that also truncates the reported duration would reject
        // every track over two minutes if it does.
        let (mut seconds, file_ms) = read_duration(path);
        if seconds <= 0 {
            seconds = fingerprint.decoded_seconds;
        }
        if seconds <= 0 {
            return reason(InconclusiveReason::NotFingerprinted);
        }

        let Some(lookup) = self
            .client
            .lookup(
                &settings.acoust_id_api_key,
                &print,
                seconds,
                settings.effective_acoust_id_timeout_seconds(),
            )
            .await
        else {
            return reason(InconclusiveReason::LookupFailed);
        };
        if !lookup.is_ok {
            warn!(
                "acoustid refused the lookup for {path}: {}",
                lookup.error.as_deref().unwrap_or("")
            );
            return reason(InconclusiveReason::LookupFailed);
        }
        let (artist, title) = (requested_artist.unwrap_or(""), requested_title.unwrap_or(""));
        if lookup.results.is_empty() {
            info!(
                "acoustid has no entry for '{artist} - {title}'; keeping the file. Obscure music is \
                 exactly what Soulseek is for, so an absent match is never treated as a mismatch."
            );
            // Kept with the fingerprint: this is the one case a person listening can settle, and
            // the one where their answer is worth sending back to AcoustID.
            return VerificationResult {
                reason: InconclusiveReason::NoEntry,
                fingerprint: Some(print),
                duration_seconds: seconds,
                lookup: Some(lookup),
                threshold: settings.effective_min_score_fraction(),
                ..Default::default()
            };
        }

        // NameFromMatch implies authoritative tags: a path from MusicBrainz beside tags from the
        // source is exactly the split it exists to remove (#48).
        let threshold = settings.effective_min_score_fraction();
        let authoritative = settings.tag_from_music_brainz || settings.name_from_match;
        let facts = self
            .recording_facts(
                &lookup,
                (requested_artist, requested_title),
                threshold,
                (seconds, file_ms),
                takes,
                (settings.effective_acoust_id_timeout_seconds(), background),
            )
            .await;
        let mut verdict = VerificationResult {
            fingerprint: Some(print),
            duration_seconds: seconds,
            ..Self::decide_with(
                &lookup,
                requested_artist,
                requested_title,
                threshold,
                authoritative,
                (seconds, file_ms),
                takes,
                &facts,
            )
        };

        // A recording whose name reads differently from the request may still be it: a title in
        // its own script, or translated. Its ISRCs settle that when the request carried one.
        if let (VerificationVerdict::Mismatch, Some(isrc), Some(music_brainz)) =
            (verdict.verdict, isrc, &self.music_brainz)
        {
            let recording_isrcs = Self::recording_isrcs(
                music_brainz,
                &lookup,
                threshold,
                settings.effective_acoust_id_timeout_seconds(),
            )
            .await;
            verdict = Self::settle_by_isrc(
                verdict,
                &lookup,
                threshold,
                authoritative,
                isrc,
                &recording_isrcs,
                tagged_match,
            );
        }
        verdict.threshold = threshold;

        // A confirmation is logged too, not just a refusal. The dominant risk in this feature is
        // that a broken key, a missing binary or a mangled request makes it accept everything
        // while looking healthy, and silence on success is indistinguishable from never running.
        if verdict.verdict == VerificationVerdict::Confirmed && verdict.evidence.is_none() {
            let album = match verdict.matched_album.as_deref().filter(|a| !a.is_empty()) {
                Some(album) => format!(" from '{album}'"),
                None => String::new(),
            };
            info!(
                "acoustid confirmed '{artist} - {title}' at {}{album}",
                percent(verdict.score)
            );
        }

        if verdict.verdict == VerificationVerdict::Inconclusive
            && verdict.reason == InconclusiveReason::SourceDisagreed
        {
            info!(
                "acoustid names {} for '{artist} - {title}', but {}; keeping the file and asking about it",
                verdict.describe(),
                verdict.evidence.as_deref().unwrap_or("")
            );
        } else if verdict.verdict == VerificationVerdict::Inconclusive {
            let best = lookup
                .results
                .iter()
                .map(|r| r.score)
                .fold(f64::NEG_INFINITY, f64::max);
            info!(
                "acoustid's best match for '{artist} - {title}' scored {} against a {} \
                 threshold, so it decides nothing and the file is kept",
                percent(best),
                percent(threshold)
            );
        }

        verdict
    }

    /// What MusicBrainz says the recordings that could be this file are, when AcoustID's answer
    /// may not tell their kinds apart: every agreeing recording (up to
    /// [`Self::MAX_FACT_LOOKUPS`]) when there are several and the exact length does not decide,
    /// and otherwise, for a download that refuses some takes, the one recording it would keep,
    /// so a take only the database's note calls an instrumental or a live one is seen. Bounded by
    /// one timeout for all of them; a recording that could not be asked is judged by what
    /// AcoustID said. Work in AcoustID's background lane asks in MusicBrainz's background lane
    /// too, so a library sweep never holds up a download's lookup.
    async fn recording_facts(
        &self,
        lookup: &AcoustIdLookup,
        (requested_artist, requested_title): (Option<&str>, Option<&str>),
        threshold: f64,
        (seconds, file_ms): (i32, Option<i64>),
        takes: &Takes,
        (timeout_seconds, background): (i32, bool),
    ) -> FactsById {
        let mut found = FactsById::new();
        let Some(music_brainz) = &self.music_brainz else {
            return found;
        };
        let Some(best) = best_qualifying(lookup, threshold) else {
            return found;
        };
        let agreeing = agreeing_in(
            best,
            requested_artist.unwrap_or(""),
            requested_title.unwrap_or(""),
            seconds,
        );
        let mut ids: Vec<String> = Vec::new();
        for recording in &agreeing {
            if !ids
                .iter()
                .any(|id| dotnet::eq_ignore_case(id, &recording.recording_id))
            {
                ids.push(recording.recording_id.clone());
            }
        }
        let decided = recording_identity::decided_by_length(&agreeing, file_ms).is_some();
        let wanted: Vec<String> = if ids.len() > 1 && !decided {
            ids.into_iter().take(Self::MAX_FACT_LOOKUPS).collect()
        } else if matches!(takes, Takes::Only(_)) {
            let kind = |r: &AcoustIdRecording| recording_identity::kind_of(r, None);
            let asked = RecordingKind::requested_by(requested_title.unwrap_or(""));
            recording_identity::choose(&agreeing, file_ms, &asked, |k| takes.refuses(k), kind)
                .map(|chosen| vec![chosen.recording.recording_id.clone()])
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if wanted.is_empty() {
            return found;
        }

        // The background lane waits behind every download's questions, so it is given longer.
        let per_lookup = if background { 30 } else { 2 };
        let budget = Duration::from_secs((timeout_seconds.max(0) as u64) + per_lookup * wanted.len() as u64);
        let deadline = tokio::time::Instant::now() + budget;
        for id in &wanted {
            let asked = tokio::time::timeout_at(
                deadline,
                music_brainz.recording_facts(id, background || AcoustIdRateLimiter::in_background_now()),
            )
            .await;
            match asked {
                Ok(Ok(Some(facts))) => {
                    found.insert(dotnet::to_lower_invariant(id), facts);
                }
                Ok(Ok(None)) => {}
                Ok(Err(MusicBrainzError::TimedOut)) | Err(_) => {
                    info!(
                        "musicbrainz took too long to say what the recordings are; deciding on what AcoustID said"
                    );
                    break;
                }
                Ok(Err(error)) => {
                    warn!("musicbrainz answered the recording lookup with something unreadable: {error}");
                    break;
                }
            }
        }
        found
    }

    /// The ISRCs MusicBrainz lists for the first few recordings of the best qualifying result, by
    /// recording id. A recording that could not be asked is left out; one asked that lists none
    /// is present with an empty list. Bounded by one timeout for all of them.
    async fn recording_isrcs(
        music_brainz: &MusicBrainzClient,
        lookup: &AcoustIdLookup,
        threshold: f64,
        timeout_seconds: i32,
    ) -> RecordingIsrcs {
        let mut found = RecordingIsrcs::new();
        let Some(best) = best_qualifying(lookup, threshold) else {
            return found;
        };

        let budget = Duration::from_secs((timeout_seconds.max(0) as u64) + 2 * Self::MAX_ISRC_LOOKUPS as u64);
        let mut asked: Vec<&str> = Vec::new();
        let recordings = best
            .recordings
            .iter()
            .filter(|r| !r.recording_id.is_empty())
            .filter(|r| {
                // Each recording id once, compared exactly; the first one wins.
                if asked.contains(&r.recording_id.as_str()) {
                    false
                } else {
                    asked.push(&r.recording_id);
                    true
                }
            })
            .take(Self::MAX_ISRC_LOOKUPS)
            .collect::<Vec<_>>();
        let ask_all = async {
            for recording in recordings {
                match music_brainz.fetch_isrcs(&recording.recording_id).await {
                    Ok(Some(isrcs)) => {
                        insert_ignore_case(&mut found, &recording.recording_id, isrcs);
                    }
                    Ok(None) => {}
                    Err(MusicBrainzError::TimedOut) => return Err(()),
                    Err(error) => {
                        // A malformed answer ends the lookup the way a timeout does, logged.
                        warn!("musicbrainz answered the ISRC lookup with something unreadable: {error}");
                        return Err(());
                    }
                }
            }
            Ok(())
        };
        let done = tokio::time::timeout(budget, ask_all).await;
        if !matches!(done, Ok(Ok(()))) {
            info!("musicbrainz took too long to list ISRCs; deciding on what it answered");
        }
        found
    }

    /// A fingerprint named recordings whose titles and artists read differently from the
    /// request, and the request carried an ISRC. One of those recordings listing that ISRC makes
    /// it the recording asked for, whatever its name. None of them listing any ISRC at all,
    /// while the file's own tags carry the requested one, is a disagreement between two sources
    /// a person can settle, so the file is kept and asked about rather than deleted. Recordings
    /// that list other ISRCs change nothing: the verdict stays as it was.
    pub fn settle_by_isrc(
        verdict: VerificationResult,
        lookup: &AcoustIdLookup,
        threshold: f64,
        tags_authoritative: bool,
        isrc: &str,
        recording_isrcs: &RecordingIsrcs,
        tagged_match: bool,
    ) -> VerificationResult {
        if verdict.verdict != VerificationVerdict::Mismatch {
            return verdict;
        }
        let Some(best) = best_qualifying(lookup, threshold) else {
            return verdict;
        };

        // A stray link is not believed on its ISRC either: the request's ISRC belongs to the
        // song asked for, so it names the stray as surely as its title did.
        let agreed = best.recordings.iter().find(|recording| {
            get_ignore_case(recording_isrcs, &recording.recording_id)
                .is_some_and(|isrcs| isrcs.iter().any(|code| code == isrc))
                && trustworthy(recording, best, verdict.duration_seconds)
        });
        if let Some(agreed) = agreed {
            return VerificationResult {
                fingerprint: verdict.fingerprint,
                duration_seconds: verdict.duration_seconds,
                lookup: Some(lookup.clone()),
                threshold: verdict.threshold,
                evidence: Some(format!(
                    "MusicBrainz lists the requested ISRC {isrc} on '{} - {}'",
                    agreed.artist_credit(),
                    agreed.title
                )),
                ..confirm(best, agreed, tags_authoritative)
            };
        }

        if tagged_match && !recording_isrcs.is_empty() && recording_isrcs.values().all(Vec::is_empty) {
            return VerificationResult {
                verdict: VerificationVerdict::Inconclusive,
                reason: InconclusiveReason::SourceDisagreed,
                deny_reason: String::new(),
                evidence: Some(format!(
                    "the file's own tags carry the requested ISRC {isrc} and MusicBrainz lists none to contradict it"
                )),
                ..verdict
            };
        }

        verdict
    }

    /// The whole decision, separated from the I/O so it can be driven directly. Every branch
    /// here either keeps a file or deletes one, and the fingerprinter and HTTP client above make
    /// the orchestration awkward to fake for no benefit.
    pub fn decide(
        lookup: &AcoustIdLookup,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        threshold: f64,
        tags_authoritative: bool,
        duration_seconds: i32,
        takes: &Takes,
    ) -> VerificationResult {
        Self::decide_with(
            lookup,
            requested_artist,
            requested_title,
            threshold,
            tags_authoritative,
            (duration_seconds, None),
            takes,
            &FactsById::new(),
        )
    }

    /// [`Self::decide`] with the file's length to the millisecond and what MusicBrainz said
    /// about the recordings, which decide between takes that share the fingerprint.
    pub fn decide_with(
        lookup: &AcoustIdLookup,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        threshold: f64,
        tags_authoritative: bool,
        (duration_seconds, file_ms): (i32, Option<i64>),
        takes: &Takes,
        facts: &FactsById,
    ) -> VerificationResult {
        VerificationResult {
            lookup: Some(lookup.clone()),
            threshold,
            ..decide_core(
                lookup,
                (requested_artist.unwrap_or(""), requested_title.unwrap_or("")),
                threshold,
                tags_authoritative,
                (duration_seconds, file_ms),
                takes,
                facts,
            )
        }
    }

    /// Whether MusicBrainz lists this recording only on live albums: every release group it is
    /// on is typed Live. A studio recording that also appears on a live album is not one; a
    /// recording with no albums listed is not judged.
    pub fn only_on_live_albums(recording: &AcoustIdRecording) -> bool {
        // One release per group, keyed by the group id, else the group title, else the title;
        // the first release of each group stands for it.
        let mut keys: Vec<Option<&str>> = Vec::new();
        let mut groups = Vec::new();
        for release in &recording.releases {
            let key = release
                .release_group_id
                .as_deref()
                .or(release.group_title.as_deref())
                .or(release.title.as_deref());
            if !keys.contains(&key) {
                keys.push(key);
                groups.push(release);
            }
        }
        !groups.is_empty()
            && groups.iter().all(|release| {
                release
                    .secondary_types
                    .iter()
                    .any(|kind| dotnet::eq_ignore_case(kind, "Live"))
            })
    }

    /// Every valid ISRC the file's tags carry: ID3's TSRC, a Vorbis comment's ISRC and the MP4
    /// iTunes ISRC atom all come through one TagLib property. ffmpeg, and whatever converted a
    /// file with it, writes an MP3's ISRC as a user text frame named ISRC instead, so that is
    /// read too. A tag that holds several joins them, so it is split. Empty when there is none or
    /// the tags cannot be read.
    ///
    /// The same reading the release identifier gets from `tag_writer_extras::read_facts`, which
    /// gathers the codes this way.
    pub fn read_isrcs(path: &str) -> Vec<String> {
        tag_writer_extras::read_facts(path, true).isrcs
    }

    /// The single recording, at any score, that agrees on title, artist and length (7 seconds
    /// either way). Two or more is ambiguity, and ambiguity submits nothing.
    pub fn agreeing_candidate(
        lookup: &AcoustIdLookup,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        duration_seconds: i32,
    ) -> Option<String> {
        let (artist, title) = (requested_artist.unwrap_or(""), requested_title.unwrap_or(""));
        let mut ids: Vec<&str> = Vec::new();
        for recording in lookup.results.iter().flat_map(|r| &r.recordings) {
            let agrees = !recording.recording_id.is_empty()
                && TrackMatchComparer::title_matches(title, &recording.title)
                && TrackMatchComparer::artist_matches(artist, &recording.artist_credit(), &recording.artists)
                && (duration_seconds <= 0
                    || recording
                        .duration_seconds
                        .is_none_or(|d| (d - duration_seconds).abs() <= 7));
            if agrees
                && !ids
                    .iter()
                    .any(|id| dotnet::eq_ignore_case(id, &recording.recording_id))
            {
                ids.push(&recording.recording_id);
            }
        }
        (ids.len() == 1).then(|| ids[0].to_string())
    }
}

#[async_trait]
impl FingerprintVerifier for DownloadVerificationService {
    fn is_fingerprinting_enabled(&self) -> bool {
        DownloadVerificationService::is_fingerprinting_enabled(self)
    }

    async fn verify(&self, path: &str, artist: &str, title: &str) -> VerificationResult {
        DownloadVerificationService::verify(self, path, Some(artist), Some(title), None, &Takes::Any).await
    }
}

fn reason(reason: InconclusiveReason) -> VerificationResult {
    VerificationResult {
        reason,
        ..Default::default()
    }
}

fn best_qualifying(lookup: &AcoustIdLookup, threshold: f64) -> Option<&AcoustIdResult> {
    qualifying(lookup, threshold).into_iter().next()
}

/// The results at or above the threshold that name a recording, best first (a stable sort, so
/// equal scores keep their order).
fn qualifying(lookup: &AcoustIdLookup, threshold: f64) -> Vec<&AcoustIdResult> {
    let mut qualifying: Vec<&AcoustIdResult> = lookup
        .results
        .iter()
        .filter(|r| r.score >= threshold && !r.recordings.is_empty())
        .collect();
    qualifying.sort_by(|a, b| b.score.total_cmp(&a.score));
    qualifying
}

/// How close a file must run to the request's catalog length for a stray link to be doubted:
/// whole seconds on the catalog side, so a little over half a second.
const STRAY_FITS_MS: i64 = 1_500;
/// How far the linked recording must run from the file for the link to be doubted.
const STRAY_APART_MS: i64 = 4_000;

/// A link this much weaker than the strongest recording of another song is taken as a stray.
const STRAY_RATIO: i32 = 20;
/// The least length gap, in seconds, that counts as a different recording.
const MIN_LENGTH_SLACK: i32 = 10;
/// The share of the file's length that counts as a different recording on long songs.
const LENGTH_SLACK: f64 = 0.07;

/// Whether an agreeing recording can be believed: not dwarfed by a recording of another song
/// in the same result, and not far from the file's length. The comparison is only with other
/// songs, since the same audio is often tied to several versions of one song (album, single,
/// radio edit) with very different counts. A lone weak link still counts, and so does a
/// length nobody knows.
pub(crate) fn trustworthy(agreed: &AcoustIdRecording, result: &AcoustIdResult, file_seconds: i32) -> bool {
    !is_stray(agreed, result) && !length_off(agreed, file_seconds)
}

/// Whether a recording is dwarfed by a recording of another song in the same result.
fn is_stray(agreed: &AcoustIdRecording, result: &AcoustIdResult) -> bool {
    let song = SongIdentity::key(&strip_versions(&agreed.title));
    let strongest_other = result
        .recordings
        .iter()
        .filter(|r| SongIdentity::key(&strip_versions(&r.title)) != song)
        .map(|r| r.sources)
        .max()
        .unwrap_or(0);
    strongest_other > 0 && agreed.sources.saturating_mul(STRAY_RATIO) < strongest_other
}

/// Whether a recording's known length is far from the file's known length.
fn length_off(agreed: &AcoustIdRecording, file_seconds: i32) -> bool {
    let Some(length) = agreed.duration_seconds.filter(|l| *l > 0) else {
        return false;
    };
    if file_seconds <= 0 {
        return false;
    }
    let slack = MIN_LENGTH_SLACK.max((f64::from(file_seconds) * LENGTH_SLACK).round() as i32);
    (length - file_seconds).abs() > slack
}

/// The recordings of a result that agree with the request on title, artist and version and can
/// be believed (see [`trustworthy`]). A recording of the same song in another version (a remix by
/// someone else, the plain song for a remix, a sped-up copy) never agrees.
fn agreeing_in<'a>(
    result: &'a AcoustIdResult,
    requested_artist: &str,
    requested_title: &str,
    duration_seconds: i32,
) -> Vec<&'a AcoustIdRecording> {
    result
        .recordings
        .iter()
        .filter(|r| {
            TrackMatchComparer::title_matches(requested_title, &r.title)
                && !version_gap::other_version(requested_title, &r.title, Against::Recording)
                && TrackMatchComparer::artist_matches(requested_artist, &r.artist_credit(), &r.artists)
                && trustworthy(r, result, duration_seconds)
        })
        .collect()
}

/// A recording of the same song by the same artist in another version than the one asked for,
/// believed by its links and its length: what the file is when no recording of the asked
/// version agreed. The one most submissions tie the audio to, the first listed on a tie.
fn other_version_in<'a>(
    result: &'a AcoustIdResult,
    requested_artist: &str,
    requested_title: &str,
    duration_seconds: i32,
) -> Option<&'a AcoustIdRecording> {
    result
        .recordings
        .iter()
        .rev()
        .filter(|r| {
            same_song_any_version(requested_title, &r.title)
                && version_gap::other_version(requested_title, &r.title, Against::Recording)
                && TrackMatchComparer::artist_matches(requested_artist, &r.artist_credit(), &r.artists)
                && trustworthy(r, result, duration_seconds)
        })
        .max_by_key(|r| r.sources)
}

/// Whether two titles name one song, whatever version each is.
fn same_song_any_version(requested: &str, matched: &str) -> bool {
    SongIdentity::same_title(requested, matched, None).verdict != octo_core::common::SongVerdict::Different
}

/// A take the request's title asks for that the chosen recording is not, when MusicBrainz was
/// asked what that recording is: "Song (Live)" against a recording the database knows only as
/// the studio take. Without the database's word nothing is said, since a live take's title often
/// leaves the word out.
fn asked_take_missing(
    asked: &std::collections::BTreeSet<RecordingKind>,
    kind: RecordingKind,
    recording: &AcoustIdRecording,
    facts: &FactsById,
) -> Option<RecordingKind> {
    if asked.contains(&kind) || !facts.contains_key(&dotnet::to_lower_invariant(&recording.recording_id)) {
        return None;
    }
    asked.iter().copied().find(|wanted| {
        matches!(
            wanted,
            RecordingKind::Live | RecordingKind::Demo | RecordingKind::Cover | RecordingKind::Acapella
        ) && *wanted != kind
    })
}

/// The kind of take a recording is, with what MusicBrainz said about it when it was asked.
fn kind_with(recording: &AcoustIdRecording, facts: &FactsById) -> RecordingKind {
    recording_identity::kind_of(
        recording,
        facts.get(&dotnet::to_lower_invariant(&recording.recording_id)),
    )
}

fn decide_core(
    lookup: &AcoustIdLookup,
    (requested_artist, requested_title): (&str, &str),
    threshold: f64,
    tags_authoritative: bool,
    (duration_seconds, file_ms): (i32, Option<i64>),
    takes: &Takes,
    facts: &FactsById,
) -> VerificationResult {
    let qualifying = qualifying(lookup, threshold);

    // Below the threshold an answer is ignored, never acted on. That is why raising
    // MinMatchScore makes Octo MORE permissive rather than less.
    let Some(best) = qualifying.first().copied() else {
        // A result above the threshold with no recordings is a fingerprint AcoustID knows and
        // MusicBrainz does not: exactly the gap a person's confirmation can fill.
        let known = lookup.results.iter().any(|r| r.score >= threshold);
        return VerificationResult {
            reason: if known {
                InconclusiveReason::NoEntry
            } else {
                InconclusiveReason::BelowThreshold
            },
            candidate_recording_id: DownloadVerificationService::agreeing_candidate(
                lookup,
                Some(requested_artist),
                Some(requested_title),
                duration_seconds,
            ),
            ..Default::default()
        };
    };

    // Any, not first: one AcoustID id maps to several MusicBrainz recordings when the same audio
    // ships on an album and a compilation, and demanding the first would reject correct files.
    let agreeing = agreeing_in(best, requested_artist, requested_title, duration_seconds);
    // One fingerprint is often tied to the studio take, a live take and an instrumental alike.
    // The file's exact length decides between them when it can; otherwise a take the title asks
    // for goes first, then the studio take.
    let asked = RecordingKind::requested_by(requested_title);
    let chosen = recording_identity::choose(
        &agreeing,
        file_ms,
        &asked,
        |k| takes.refuses(k),
        |r| kind_with(r, facts),
    );
    let sole = agreeing.first().is_some_and(|first| {
        agreeing
            .iter()
            .all(|r| dotnet::eq_ignore_case(&first.recording_id, &r.recording_id))
    });

    if let Some(recording_identity::Chosen {
        recording: agreed,
        kind,
        by_length,
        kind_certain,
    }) = chosen
    {
        // The right song and artist, but a take the request never asked for: the same title and
        // nearly the same length, so only its exact length and what it is on tell it apart.
        if let Some(missing) = asked_take_missing(&asked, kind, agreed, facts) {
            return VerificationResult {
                verdict: VerificationVerdict::Mismatch,
                score: best.score,
                matched_title: Some(agreed.title.clone()),
                matched_artist: Some(agreed.artist_credit()),
                matched_album: recording_album(agreed, kind),
                matched_year: agreed.year,
                recording_id: Some(agreed.recording_id.clone()),
                deny_reason: format!("is {}, not {} asked for", kind.described(), missing.described()),
                kind: Some(kind),
                by_length,
                kind_certain,
                sole,
                ..Default::default()
            };
        }
        if takes.refuses(kind) {
            let album = recording_album(agreed, kind);
            return VerificationResult {
                verdict: VerificationVerdict::Mismatch,
                score: best.score,
                matched_title: Some(agreed.title.clone()),
                matched_artist: Some(agreed.artist_credit()),
                matched_album: album.clone(),
                matched_year: agreed.year,
                recording_id: Some(agreed.recording_id.clone()),
                deny_reason: match album.filter(|a| !a.is_empty()) {
                    None => format!("is {}", kind.described()),
                    Some(album) => format!("is {}, from '{album}'", kind.described()),
                },
                kind: Some(kind),
                by_length,
                kind_certain,
                sole,
                ..Default::default()
            };
        }
        return VerificationResult {
            kind: Some(kind),
            by_length,
            kind_certain,
            sole,
            ..confirm(best, agreed, tags_authoritative)
        };
    }

    // The right song and artist, believed but for its length: a person can tell whether it is a
    // different cut, and the requested song is never named as another one.
    if let Some(doubted) = best.recordings.iter().find(|r| {
        TrackMatchComparer::title_matches(requested_title, &r.title)
            && !version_gap::other_version(requested_title, &r.title, Against::Recording)
            && TrackMatchComparer::artist_matches(requested_artist, &r.artist_credit(), &r.artists)
            && !is_stray(r, best)
    }) {
        return VerificationResult {
            verdict: VerificationVerdict::Inconclusive,
            reason: InconclusiveReason::LengthOff,
            score: best.score,
            matched_title: Some(doubted.title.clone()),
            matched_artist: Some(doubted.artist_credit()),
            matched_album: doubted.album_title.clone(),
            matched_year: doubted.year,
            recording_id: Some(doubted.recording_id.clone()),
            r#match: Some(doubted.clone()),
            ..Default::default()
        };
    }

    // The same song in another version: never the file asked for, however alike the rest reads.
    if let Some(other) = other_version_in(best, requested_artist, requested_title, duration_seconds) {
        let kind = kind_with(other, facts);
        let gap = version_gap::version_gap(requested_title, &other.title, Against::Recording)
            .map(|gap| gap.words())
            .unwrap_or_default();
        return VerificationResult {
            verdict: VerificationVerdict::Mismatch,
            score: best.score,
            matched_title: Some(other.title.clone()),
            matched_artist: Some(other.artist_credit()),
            matched_album: recording_album(other, kind),
            matched_year: other.year,
            recording_id: Some(other.recording_id.clone()),
            deny_reason: format!(
                "is another version, '{} - {}': {gap}",
                other.artist_credit(),
                other.title
            ),
            kind: Some(kind),
            ..Default::default()
        };
    }

    // The recording most submissions tie this audio to, the first listed on a tie.
    let actual = best
        .recordings
        .iter()
        .rev()
        .max_by_key(|r| r.sources)
        .unwrap_or(&best.recordings[0]);
    VerificationResult {
        verdict: VerificationVerdict::Mismatch,
        score: best.score,
        matched_title: Some(actual.title.clone()),
        matched_artist: Some(actual.artist_credit()),
        matched_album: actual.album_title.clone(),
        matched_year: actual.year,
        recording_id: Some(actual.recording_id.clone()),
        deny_reason: format!("is '{} - {}'", actual.artist_credit(), actual.title),
        ..Default::default()
    }
}

fn confirm(
    result: &AcoustIdResult,
    recording: &AcoustIdRecording,
    tags_authoritative: bool,
) -> VerificationResult {
    VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        score: result.score,
        acoust_id: result.id.clone(),
        matched_title: Some(recording.title.clone()),
        matched_artist: Some(recording.artist_credit()),
        matched_album: recording.album_title.clone(),
        matched_year: recording.year,
        recording_id: Some(recording.recording_id.clone()),
        tags_authoritative,
        r#match: Some(recording.clone()),
        ..Default::default()
    }
}

/// The release a take is best named by: one whose title says what the take is (the promo of
/// instrumentals, not the album the studio take is on), else the album AcoustID picked.
fn recording_album(recording: &AcoustIdRecording, kind: RecordingKind) -> Option<String> {
    if kind != RecordingKind::Studio {
        let telling = recording.releases.iter().find_map(|release| {
            let title = release.title.as_deref().or(release.group_title.as_deref())?;
            RecordingKind::named_in(title)
                .contains(&kind)
                .then(|| title.to_string())
        });
        if telling.is_some() {
            return telling;
        }
    }
    recording.album_title.clone()
}

/// The file's length in whole seconds and in milliseconds; 0 and None when it cannot be read.
fn read_duration(path: &str) -> (i32, Option<i64>) {
    match TagFile::open(path) {
        Ok(file) => {
            let ms = file.duration().as_millis() as i64;
            (file.duration_seconds(), (ms > 0).then_some(ms))
        }
        Err(error) => {
            debug!("could not read a duration from {path}: {error}");
            (0, None)
        }
    }
}

fn get_ignore_case<'a>(map: &'a RecordingIsrcs, key: &str) -> Option<&'a Vec<String>> {
    map.get(key).or_else(|| {
        map.iter()
            .find(|(k, _)| dotnet::eq_ignore_case(k, key))
            .map(|(_, v)| v)
    })
}

fn insert_ignore_case(map: &mut RecordingIsrcs, key: &str, value: Vec<String>) {
    let existing = map.keys().find(|k| dotnet::eq_ignore_case(k, key)).cloned();
    map.insert(existing.unwrap_or_else(|| key.to_string()), value);
}

/// A fraction for the log as a whole percentage, such as `85%`.
fn percent(fraction: f64) -> String {
    format!("{}%", dotnet::round(fraction * 100.0, 0))
}

/// The extension of the file name: its last dot and what follows, or empty when the name has
/// no dot or ends with one.
fn get_extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => &name[dot..],
        _ => "",
    }
}

#[cfg(test)]
#[path = "download_verification_service_tests.rs"]
mod tests;

/// The review sweep's check: a verification of the path, artist and title with no ISRC and
/// live takes not refused.
#[async_trait::async_trait]
impl crate::services::library::library_review_sweep_worker::SweepVerification
    for DownloadVerificationService
{
    fn is_fingerprinting_enabled(&self) -> bool {
        DownloadVerificationService::is_fingerprinting_enabled(self)
    }

    async fn verify(&self, path: &str, artist: Option<&str>, title: Option<&str>) -> VerificationResult {
        DownloadVerificationService::verify_in_background(self, path, artist, title).await
    }

    /// The same question a new download is asked of a sound AcoustID has never heard.
    async fn hear_preview(
        &self,
        source: &dyn PreviewSource,
        path: &str,
        request: &PreviewRequest<'_>,
        question: &VerificationResult,
    ) -> PreviewOutcome {
        self.hear_against_preview(source, path, request, question, true)
            .await
    }

    /// The codes AcoustID's own answer carried for the recordings of its best result, else the
    /// ones MusicBrainz lists for them, the lookup a download with an ISRC is settled by.
    async fn isrc_agrees(&self, question: &VerificationResult, isrc: &str) -> Option<bool> {
        let isrc = SongIdentity::normalize_isrc(isrc)?;
        let lookup = question.lookup.as_ref()?;
        let settings = self.settings.current().soulseek.clone();
        let threshold = if question.threshold > 0.0 {
            question.threshold
        } else {
            settings.effective_min_score_fraction()
        };
        let best = best_qualifying(lookup, threshold)?;
        let mut listed = RecordingIsrcs::new();
        for recording in best.recordings.iter().filter(|r| !r.recording_id.is_empty()) {
            if !recording.isrcs.is_empty() {
                insert_ignore_case(&mut listed, &recording.recording_id, recording.isrcs.clone());
            }
        }
        if listed.is_empty() {
            let music_brainz = self.music_brainz.as_ref()?;
            listed = Self::recording_isrcs(
                music_brainz,
                lookup,
                threshold,
                settings.effective_acoust_id_timeout_seconds(),
            )
            .await;
        }
        let settled = Self::settle_by_isrc(question.clone(), lookup, threshold, false, &isrc, &listed, true);
        if settled.verdict == VerificationVerdict::Confirmed {
            return Some(true);
        }
        if listed.values().all(Vec::is_empty) {
            return None;
        }
        Some(
            listed
                .values()
                .flatten()
                .any(|code| SongIdentity::normalize_isrc(code).as_deref() == Some(isrc.as_str())),
        )
    }
}
