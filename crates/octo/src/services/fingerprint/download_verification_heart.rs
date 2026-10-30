//! The heart gate in Octo: one heart's or play's gate built from the album the person listened
//! from and what they heard, and one downloaded file judged with it.

use std::collections::HashSet;

use chrono::Utc;
use octo_core::fingerprint::acoust_id_client::{AcoustIdAnswer, AcoustIdOutcome};
use octo_core::fingerprint::heart_gate::{
    AcquisitionOrigin, AlbumContext, ClipReading, FileFacts, FileLabels, HeardAnswerReading, HeardMaster,
    HeardReading, HeardRecord, HeardRecording, Judgement, MusicBrainzFact, Need, StandIn, TagClaim, became,
    is_heard_audio, read_heard_answer, read_stand_in,
};
use octo_core::fingerprint::heart_release::{GroupFacts, HeardIdentity, HeartSong, listened_release};
use octo_core::fingerprint::verification::{
    Reading, RecordingProof, WeighedRecording, length_fits, read_answer,
};
use octo_core::fingerprint::{InconclusiveReason, VerificationResult, VerificationVerdict};
use octo_core::soulseek::SoulseekRouting;
use octo_media::audio::FingerprintOutcome;
use octo_media::tags::tag_writer_extras;
use tracing::{debug, info};

use super::*;
use crate::services::common::heart_acquisition_coordinator::{GateStart, HeartGates, waiting_line};
use crate::services::common::{HeldAcquisition, Holder};
use crate::services::fingerprint::heard_master_check::{HeardAudio, RoundGuard};
use crate::services::fingerprint::music_brainz_client::{
    HeartBusy, HeartQuestions, Lane, code_question, heart_song, recording_question,
};
use crate::services::prefetch::heard_samples::{FpcalcClass, Printer, TopUp, sample_id_of};

/// The downloads line of a heart whose heard lookup has not answered yet.
pub const HEARD_LOOKUP_BUSY: &str = "Waiting for AcoustID to check what played";
/// The downloads line of a heart whose stream stood in with another song.
pub const ANOTHER_SONG_PLAYED: &str =
    "Octo played another song for this one. Play it once more so Octo can keep the version you hear";
/// The downloads line of a heart the catalog's clip says stood in with another song or take.
pub const ANOTHER_SONG_OR_TAKE: &str = "Octo played another song or another take for this one";
/// The downloads line of a heart nobody played.
pub const NEVER_PLAYED: &str = "Play it once so Octo can keep the version you hear";

/// The most facts one file's judgement asks for before it settles: a bound, never reached by a
/// gate that asks each fact once.
const MOST_NEEDS: usize = 32;

/// What reading the heard audio's own AcoustID answer came to.
enum HeardLookup {
    /// AcoustID's outcome for the heard print, and the song's played length sent with it.
    Read(AcoustIdOutcome, Option<i32>),
    /// The shim did not answer for the heard video.
    ShimBusy,
    /// The heard audio is gone and no whole print of it is kept.
    Gone,
}

impl DownloadVerificationService {
    /// Gives the heart gate what it reads. Once only; a second call changes nothing.
    pub fn use_heart(&self, parts: HeartParts) {
        let _ = self.heart.set(parts);
    }

    /// Whether the heart gate has what it reads.
    pub fn heart_ready(&self) -> bool {
        self.heart.get().is_some()
    }

    /// Resumes a hold by its key once what it waits for answers.
    pub fn on_resume(&self, resume: ResumeHold) {
        *self.resume.lock() = Some(resume);
    }

    fn min_score(&self) -> f64 {
        self.settings.current().soulseek.effective_min_score_fraction()
    }

    /// What prints a downloaded file: the heard samples' printer, which every heart's sound work
    /// shares, once the heart gate has its parts; else `fpcalc` as the service was built with.
    pub(crate) fn file_printer(&self) -> Arc<dyn Printer> {
        match self.heart.get() {
            Some(parts) => parts.samples.printer(),
            None => self.fingerprinter.clone(),
        }
    }

    /// Pauses each gate's open heard-master round of the acquisition: its heard copy goes and its
    /// place is freed while the acquisition waits on something outside it. The next check makes
    /// the copy again.
    pub fn pause_rounds(&self, acquisition_key: &str, gates: &[HeartGate]) {
        let Some(parts) = self.heart.get() else {
            return;
        };
        for gate in gates {
            if let Some(round) = parts.master.round(acquisition_key, &gate.user) {
                round.pause();
            }
        }
    }

    /// One heart's or play's gate, built before any source runs: R from the album the person
    /// listened from (their most recent played context, else their most recent listed one, else
    /// every context, ranked by the kind of release; contexts that disagree on the ISRC picked by
    /// the heard answer alone, else their overlap), then what the heard audio's own AcoustID answer
    /// says of it (R stays, R becomes the recording that played, nothing proves which recording
    /// played, or another song played, whose video is refused for the song), the catalog's clip in
    /// its place with AcoustID off or unable to read the sample, and for a heart never played the
    /// pinned video as a stand-in that may only narrow R. Imports, family requests, replacements
    /// and Find songs picks have no heard audio: their gate carries R alone. A busy answer, an
    /// unsettled clip check, or nothing left to try holds before any source runs; a family request
    /// never holds, and goes on with what is known.
    pub async fn build_heart_gate(
        &self,
        provider: &str,
        external_id: &str,
        user: &str,
        origin: AcquisitionOrigin,
        heard: Option<HeardRecord>,
        prints: &mut ClipPrints,
    ) -> GateBuild {
        let _ = provider;
        let personal = matches!(
            origin,
            AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Play
        );
        let heard = heard.filter(|_| personal);
        let mut gate = HeartGate {
            user: user.trim().to_string(),
            origin,
            r: None,
            heard: heard.clone(),
            heard_seconds: None,
            reading: HeardReading::NotHeard,
            stand_in: None,
        };
        let Some(parts) = self.heart.get().cloned() else {
            return GateBuild::Ready(gate);
        };
        let Some(routing) = parts
            .registry
            .resolve_song(external_id, personal)
            .map(|shared| shared.snapshot())
        else {
            return GateBuild::Ready(gate);
        };
        let song = heart_song(&routing);
        gate.heard_seconds = routing.duration.filter(|seconds| *seconds > 0);
        let min = self.min_score();

        // What was heard is read first: its answer is what picks between album contexts that
        // disagree on the ISRC.
        let mut heard_answer = None;
        if let Some(record) = &heard {
            match self.heard_lookup(&parts, record, &routing).await {
                HeardLookup::Read(outcome, seconds) => {
                    gate.heard_seconds = gate.heard_seconds.or(seconds);
                    heard_answer = Some(outcome);
                }
                HeardLookup::ShimBusy => {
                    return hold(gate, WaitingFor::Shim, waiting_line(WaitingFor::Shim), None);
                }
                HeardLookup::Gone => {}
            }
        }
        if heard_answer == Some(AcoustIdOutcome::Busy) {
            gate.reading = HeardReading::Busy;
            return hold(gate, WaitingFor::AcoustId, HEARD_LOOKUP_BUSY, None);
        }

        let catalog = match self
            .catalog_r(&parts, external_id, user, &song, heard_answer.as_ref(), min)
            .await
        {
            Ok(catalog) => catalog,
            // A family request never holds: it goes on with the routing's names alone.
            Err(_) if origin == AcquisitionOrigin::Request => named_r(&song),
            Err(HeartBusy::MusicBrainz(question)) => {
                return hold(
                    gate,
                    WaitingFor::MusicBrainz,
                    waiting_line(WaitingFor::MusicBrainz),
                    Some(question),
                );
            }
            Err(HeartBusy::Deezer) => {
                return hold(gate, WaitingFor::Deezer, waiting_line(WaitingFor::Deezer), None);
            }
        };
        gate.r = Some(catalog.clone());
        if !personal {
            return GateBuild::Ready(gate);
        }

        let (Some(record), Some(outcome)) = (heard, heard_answer) else {
            if gate.heard.is_some() {
                // The heard audio is gone with no whole print kept: nothing can prove which
                // recording played.
                gate.reading = HeardReading::OnlyHeardOrMaster;
                return GateBuild::Ready(gate);
            }
            if origin == AcquisitionOrigin::Play {
                return GateBuild::Ready(gate);
            }
            self.stand_in(&parts, external_id, &routing, &mut gate, &catalog, min)
                .await;
            if gate.r.as_ref().is_none_or(|r| r.ids.is_empty()) {
                return hold(gate, WaitingFor::NotYetProven, NEVER_PLAYED, None);
            }
            return GateBuild::Ready(gate);
        };

        match read_heard_answer(&outcome, &catalog, min) {
            HeardAnswerReading::Busy => {
                gate.reading = HeardReading::Busy;
                hold(gate, WaitingFor::AcoustId, HEARD_LOOKUP_BUSY, None)
            }
            HeardAnswerReading::NeedsClip => {
                let reading = parts.clip.read(&routing, &record, prints).await;
                gate.reading = HeardReading::Clip(reading);
                match reading {
                    ClipReading::ThisSong => GateBuild::Ready(gate),
                    ClipReading::AnotherSongOrTake => {
                        // Never for every user: a live take of this song reads the same.
                        if let Some(video) = record.source.video_id() {
                            parts.rejected.deny_for_heart(
                                external_id,
                                &catalog,
                                Some(YOUTUBE_PEER),
                                Some(video),
                                ANOTHER_SONG_OR_TAKE,
                            );
                        }
                        hold(gate, WaitingFor::AcoustIdOff, ANOTHER_SONG_OR_TAKE, None)
                    }
                    ClipReading::Unclear | ClipReading::NoClip => hold(
                        gate,
                        WaitingFor::AcoustIdOff,
                        waiting_line(WaitingFor::AcoustIdOff),
                        None,
                    ),
                    ClipReading::Busy(waiting) => hold(gate, waiting, waiting_line(waiting), None),
                }
            }
            HeardAnswerReading::AnotherSong => {
                if let Some(video) = record.source.video_id() {
                    info!(
                        "What played for {external_id} was another song; its video {video} is refused for the song"
                    );
                    parts.registry.refuse_video(external_id, video);
                    parts.registry.refuse_song_video(&song.artist, &song.title, video);
                }
                gate.reading = HeardReading::AnotherSong;
                if catalog.ids.is_empty() {
                    return hold(gate, WaitingFor::NotYetProven, ANOTHER_SONG_PLAYED, None);
                }
                GateBuild::Ready(gate)
            }
            HeardAnswerReading::RStays { tag } => {
                gate.reading = HeardReading::RStays { tag };
                GateBuild::Ready(gate)
            }
            HeardAnswerReading::Becomes(dominant) => {
                // With no catalog identity the recording that played becomes R only when its
                // length fits the song's.
                let fits = match (dominant.duration_seconds, gate.heard_seconds) {
                    (Some(length), Some(heard)) if catalog.ids.is_empty() => {
                        length_fits(length, f64::from(heard))
                    }
                    _ => true,
                };
                let AcoustIdOutcome::Answered(answer) = &outcome else {
                    gate.reading = HeardReading::OnlyHeardOrMaster;
                    return GateBuild::Ready(gate);
                };
                if !fits {
                    gate.reading = HeardReading::OnlyHeardOrMaster;
                    return GateBuild::Ready(gate);
                }
                match self.became_r(&parts, answer, &dominant, &catalog, min).await {
                    Ok((r, tag)) => {
                        info!(
                            "What played for {external_id} was the recording {}; it is what the heart keeps",
                            dominant.id
                        );
                        gate.r = Some(r);
                        gate.reading = HeardReading::RBecame { tag };
                        GateBuild::Ready(gate)
                    }
                    Err(HeartBusy::MusicBrainz(question)) => hold(
                        gate,
                        WaitingFor::MusicBrainz,
                        waiting_line(WaitingFor::MusicBrainz),
                        Some(question),
                    ),
                    Err(HeartBusy::Deezer) => {
                        hold(gate, WaitingFor::Deezer, waiting_line(WaitingFor::Deezer), None)
                    }
                }
            }
            HeardAnswerReading::OnlyHeardOrMaster => {
                gate.reading = HeardReading::OnlyHeardOrMaster;
                GateBuild::Ready(gate)
            }
        }
    }

    /// The heard audio's own AcoustID answer, from its whole print (topped up from the heard audio
    /// when short or missing), sent with the song's played length: the routing's, else the
    /// print's when it reached the song's end. Never sent with a guessed or zero length: with none
    /// the sample reads as one AcoustID could not check, and the catalog's clip decides.
    async fn heard_lookup(
        &self,
        parts: &HeartParts,
        record: &HeardRecord,
        routing: &SoulseekRouting,
    ) -> HeardLookup {
        let song_seconds = routing.duration.filter(|seconds| *seconds > 0);
        let sample_id = record
            .sample_id
            .clone()
            .unwrap_or_else(|| sample_id_of(&record.source));
        let mut prints = parts.samples.prints(&sample_id).filter(|prints| prints.whole);
        if prints.is_none() {
            match parts.samples.top_up(record, song_seconds).await {
                TopUp::Ready(id) => prints = parts.samples.prints(&id),
                TopUp::Busy => return HeardLookup::ShimBusy,
                TopUp::Gone => return HeardLookup::Gone,
            }
        }
        let Some(prints) = prints else {
            return HeardLookup::Gone;
        };
        let seconds =
            song_seconds.or_else(|| prints.reaches_end.then_some(prints.seconds).filter(|s| *s > 0));
        let Some(seconds) = seconds else {
            debug!("The heard audio's length is not known; its sample is read by the catalog's clip");
            return HeardLookup::Read(AcoustIdOutcome::Unreadable { code: None }, None);
        };
        let outcome = self
            .client
            .lookup_answer(LookupClass::Heart, &prints.compressed, seconds)
            .await;
        HeardLookup::Read(outcome, Some(seconds))
    }

    /// R from the album contexts: the person's most recent played one, else their most recent
    /// listed one, else every context, the first by the kind of its release. Contexts that disagree
    /// on the ISRC give the one the heard answer reads clear against alone, else the overlap of
    /// their R sets. With no context, the catalog album is found from the album title.
    async fn catalog_r(
        &self,
        parts: &HeartParts,
        external_id: &str,
        user: &str,
        song: &HeartSong,
        heard_answer: Option<&AcoustIdOutcome>,
        min: f64,
    ) -> Result<HeardRecording, HeartBusy> {
        let Some(music_brainz) = self.music_brainz.clone() else {
            return Ok(named_r(song));
        };
        let questions = HeartQuestions::new(music_brainz, Arc::clone(&parts.catalog), Lane::Heart);
        let contexts = parts.registry.contexts(external_id);
        let candidates = pick_contexts(&contexts, user);
        if candidates.is_empty() {
            let identity = questions.identity(song, None).await?;
            return Ok(r_of(song, &identity));
        }
        let mut ranked = Vec::with_capacity(candidates.len());
        for context in &candidates {
            let listed = song_in(song, context);
            let identity = questions
                .identity(&listed, Some(&context.catalog_album_id))
                .await?;
            let kind = if candidates.len() > 1 {
                group_kind(&questions, &listed, &context.catalog_album_id).await?
            } else {
                0
            };
            ranked.push((kind, r_of(&listed, &identity)));
        }
        // A stable sort: contexts of one kind keep their order.
        ranked.sort_by_key(|(kind, _)| *kind);
        let rs: Vec<HeardRecording> = ranked.into_iter().map(|(_, r)| r).collect();
        Ok(settle_candidates(song, rs, heard_answer, min))
    }

    /// R becomes the recording that played and the recordings MusicBrainz files under its own
    /// codes with its note, and the tag a file kept as heard may carry.
    async fn became_r(
        &self,
        parts: &HeartParts,
        answer: &AcoustIdAnswer,
        dominant: &WeighedRecording,
        catalog: &HeardRecording,
        min: f64,
    ) -> Result<(HeardRecording, Option<String>), HeartBusy> {
        let mut duplicates = Vec::new();
        if let Some(music_brainz) = self.music_brainz.clone() {
            let questions = HeartQuestions::new(music_brainz, Arc::clone(&parts.catalog), Lane::Heart);
            for code in &dominant.isrcs {
                let listed = questions.code_recordings(code).await?;
                let Some(note) = listed
                    .iter()
                    .find(|recording| recording.id.eq_ignore_ascii_case(&dominant.id))
                    .map(|recording| recording.note.clone())
                else {
                    continue;
                };
                duplicates.extend(
                    listed
                        .iter()
                        .filter(|recording| recording.note == note)
                        .filter(|recording| !recording.id.eq_ignore_ascii_case(&dominant.id))
                        .map(|recording| recording.id.clone()),
                );
            }
        }
        Ok(became(answer, dominant, &duplicates, catalog, min))
    }

    /// A heart never played: the song's pinned video, fetched and printed only to help find R. With
    /// a catalog R it may only narrow R to the one recording its answer gives that is already R's;
    /// with none, the one recording its answer gives that fits the song's length becomes R. Its
    /// print is what downloads are compared with only when its answer reads clear or ambiguous
    /// against R (or, with no catalog R, when it gave R). A busy shim or AcoustID leaves it out.
    async fn stand_in(
        &self,
        parts: &HeartParts,
        external_id: &str,
        routing: &SoulseekRouting,
        gate: &mut HeartGate,
        catalog: &HeardRecording,
        min: f64,
    ) {
        let Some(video) = routing
            .you_tube_id
            .clone()
            .filter(|video| !video.trim().is_empty())
        else {
            return;
        };
        let facts = parts.registry.stream_facts(external_id).unwrap_or_default();
        let record = HeardRecord {
            heard_at: Utc::now(),
            source: HeardSource::Video {
                id: video.clone(),
                size: facts.size.filter(|_| facts.size_confirmed),
                content_type: match facts.ext.as_deref() {
                    Some("webm") => "audio/webm".to_string(),
                    _ => "audio/mp4".to_string(),
                },
                itag: facts.itag.as_deref().and_then(|itag| itag.trim().parse().ok()),
            },
            sample_id: None,
        };
        let HeardLookup::Read(AcoustIdOutcome::Answered(answer), _) =
            self.heard_lookup(parts, &record, routing).await
        else {
            return;
        };
        let reading = read_stand_in(&answer, catalog, gate.heard_seconds, min);
        if let Some(recording) = &reading.recording {
            if catalog.ids.is_empty() {
                if let Ok((r, _)) = self.became_r(parts, &answer, recording, catalog, min).await {
                    gate.r = Some(r);
                }
            } else {
                let mut narrowed = catalog.clone();
                narrowed.ids = vec![recording.id.clone()];
                gate.r = Some(narrowed);
            }
        }
        gate.stand_in = Some(StandIn {
            video_id: video,
            sample_id: Some(sample_id_of(&record.source)),
            is_reference: reading.is_reference,
        });
    }

    /// Judges one downloaded file with one gate: its print at the length downloads are
    /// fingerprinted at, in one heart's `fpcalc` place; AcoustID's answer for it on the gate's
    /// class; its own labels from where its source delivered them; its distance from the heard
    /// print (proof 2); whether it is the very audio that played (A0); and every fact the gate asks
    /// for until it gives a verdict: the heard master from the person's open round (else a round of
    /// this file alone), and MusicBrainz's word on a merged id or a code. Never the shipped
    /// `verify` and never the song's YouTube preview: a preview cannot tell two mixes apart.
    pub async fn verify_for_heart(
        &self,
        path: &Path,
        candidate: &HeartCandidate,
        gate: &HeartGate,
    ) -> HeartCheck {
        let parts = self.heart.get().cloned();
        let settings = self.settings.current().soulseek.clone();
        let file = path.to_string_lossy().to_string();
        let r = gate.r.clone().unwrap_or_default();
        let (title, artist) = (r.title.clone(), r.artist.clone());

        let (fingerprint, raw) = {
            let _slot = match &parts {
                Some(parts) => Some(parts.samples.slot(FpcalcClass::Heart).await),
                None => None,
            };
            let printer = self.file_printer();
            let seconds = settings.effective_fingerprint_seconds();
            let timeout = settings.effective_fingerprint_timeout_seconds();
            let fingerprint = printer.fingerprint(path, seconds, timeout).await;
            let raw = if fingerprint.outcome == FingerprintOutcome::Ok {
                printer.raw(path, seconds, timeout).await
            } else {
                None
            };
            (fingerprint, raw)
        };
        if fingerprint.outcome == FingerprintOutcome::Undecodable {
            return HeartCheck {
                verdict: GateVerdict::Refuse(octo_core::fingerprint::heart_gate::Refusal::Unreadable),
                reading: None,
                verification: VerificationResult {
                    verdict: VerificationVerdict::Mismatch,
                    deny_reason: "delivered a file with no decodable audio".to_string(),
                    ..Default::default()
                },
            };
        }
        let (mut seconds, file_ms) = read_duration(&file);
        if seconds <= 0 {
            seconds = fingerprint.decoded_seconds;
        }
        let print = fingerprint.fingerprint.clone().filter(|print| !print.is_empty());
        let (outcome, lookup) = match (&print, seconds > 0) {
            (Some(print), true) => {
                self.client
                    .lookup_answer_read(LookupClass::for_origin(gate.origin), print, seconds)
                    .await
            }
            // A file nothing could fingerprint is one AcoustID cannot check.
            _ => (AcoustIdOutcome::Unreadable { code: None }, None),
        };

        let labels = match &candidate.labels {
            LabelSource::TheFile => labels_of(&file),
            LabelSource::Delivered(delivered) => labels_of(&delivered.to_string_lossy()),
            LabelSource::None => FileLabels::default(),
        };
        let reading = match &outcome {
            AcoustIdOutcome::Answered(answer) => Some(read_answer(answer, &r, self.min_score())),
            _ => None,
        };
        let mut facts = FileFacts::new(outcome, labels);
        if let (Some(parts), Some(raw)) = (&parts, &raw) {
            facts.heard_distance = reference_print(parts, gate)
                .and_then(|heard| preview_check::compare(&raw.items, &heard))
                .map(|comparison| comparison.bit_error);
        }
        facts.identical = match (&gate.heard, &candidate.identity) {
            (Some(heard), Some(identity)) => is_heard_audio(&heard.source, identity),
            _ => false,
        };
        let verdict = self.settle(&parts, &mut facts, gate, candidate, path).await;

        let verification = match lookup {
            Some(lookup) => {
                let threshold = settings.effective_min_score_fraction();
                let authoritative = settings.tag_from_music_brainz || settings.name_from_match;
                let takes = self.takes_for(Some(&title), None);
                VerificationResult {
                    fingerprint: print,
                    duration_seconds: seconds,
                    ..Self::decide_with(
                        &lookup,
                        Some(&artist),
                        Some(&title),
                        threshold,
                        authoritative,
                        (seconds, file_ms),
                        &takes,
                        &FactsById::new(),
                    )
                }
            }
            None if raw.is_none() && print.is_none() => reason(InconclusiveReason::NotFingerprinted),
            None => reason(InconclusiveReason::LookupFailed),
        };
        HeartCheck {
            verdict,
            reading,
            verification,
        }
    }

    /// Asks the gate until it gives a verdict, answering each fact it needs.
    async fn settle(
        &self,
        parts: &Option<HeartParts>,
        facts: &mut FileFacts,
        gate: &HeartGate,
        candidate: &HeartCandidate,
        path: &Path,
    ) -> GateVerdict {
        let min = self.min_score();
        for _ in 0..MOST_NEEDS {
            match gate.judge(facts, min) {
                Judgement::Verdict(verdict) => return verdict,
                Judgement::Needs(Need::HeardMaster) => {
                    facts.heard_master = Some(heard_master(parts, gate, candidate, path).await);
                }
                Judgement::Needs(Need::CurrentRecording(id)) => {
                    let fact = match &self.music_brainz {
                        Some(music_brainz) => music_brainz.current_recording(&id).await,
                        None => MusicBrainzFact::NotKnown,
                    };
                    if matches!(fact, MusicBrainzFact::Busy) {
                        self.remember_question(candidate, gate, recording_question(&id));
                    }
                    facts.current_recordings.push((id, fact));
                }
                Judgement::Needs(Need::IsrcRecordings(code)) => {
                    let fact = match &self.music_brainz {
                        Some(music_brainz) => music_brainz.isrc_songs_for_heart(&code).await,
                        None => MusicBrainzFact::NotKnown,
                    };
                    if matches!(fact, MusicBrainzFact::Busy) {
                        self.remember_question(candidate, gate, code_question(&code));
                    }
                    facts.isrc_recordings.push((code, fact));
                }
            }
        }
        GateVerdict::Undecided(octo_core::fingerprint::heart_gate::Undecided::NotYetProven)
    }

    /// Keeps the MusicBrainz question a file's label check met busy, so the hold the file leaves
    /// for this person asks it again once filed.
    fn remember_question(&self, candidate: &HeartCandidate, gate: &HeartGate, question: Option<String>) {
        let Some(question) = question else {
            return;
        };
        let song = candidate
            .acquisition_key
            .strip_suffix(":flac")
            .unwrap_or(&candidate.acquisition_key)
            .to_string();
        let mut kept = self.file_questions.lock();
        // Bounded: a question waits here only between a file's check and its hold.
        if kept.len() >= MOST_KEPT_PRINTS {
            kept.clear();
        }
        kept.insert((song, gate.user.trim().to_lowercase()), question);
    }

    /// The verdict a kept file is tagged by: a recording the sound proved, with its id and the
    /// recording AcoustID names for it; or "as heard", with no recording id, no ISRC and no name
    /// from the match. Anything the gate did not accept keeps the shipped reading.
    pub fn kept_verification(check: &HeartCheck, gate: &HeartGate) -> VerificationResult {
        let mut kept = check.verification.clone();
        let GateVerdict::Accept { tags, .. } = &check.verdict else {
            return kept;
        };
        kept.verdict = VerificationVerdict::Confirmed;
        kept.reason = InconclusiveReason::None;
        kept.deny_reason = String::new();
        match tags {
            TagClaim::Recording { id, .. } => {
                if !kept
                    .recording_id
                    .as_deref()
                    .is_some_and(|named| named.eq_ignore_ascii_case(id))
                {
                    let named = kept.lookup.as_ref().and_then(|lookup| {
                        lookup
                            .results
                            .iter()
                            .flat_map(|result| &result.recordings)
                            .find(|recording| recording.recording_id.eq_ignore_ascii_case(id))
                            .cloned()
                    });
                    kept.matched_title = named.as_ref().map(|r| r.title.clone()).or(kept.matched_title);
                    kept.matched_artist = named.as_ref().map(|r| r.artist_credit()).or(kept.matched_artist);
                    kept.r#match = named;
                }
                kept.recording_id = Some(id.clone());
                kept.proof = RecordingProof::Proven;
            }
            TagClaim::AsHeard => {
                kept.recording_id = None;
                kept.candidate_recording_id = None;
                kept.r#match = None;
                kept.tags_authoritative = false;
                kept.proof = RecordingProof::AsHeard {
                    video_id: gate
                        .heard
                        .as_ref()
                        .and_then(|heard| heard.source.video_id())
                        .map(str::to_string),
                };
            }
        }
        kept
    }
}

/// The peer name a refused video is remembered under.
pub const YOUTUBE_PEER: &str = "youtube";

/// The print a file is compared with for proof 2: what the person heard, or for a heart never
/// played its stand-in when that is the reference.
fn reference_print(parts: &HeartParts, gate: &HeartGate) -> Option<Vec<u32>> {
    let sample = match (&gate.heard, &gate.stand_in) {
        (Some(heard), _) => heard
            .sample_id
            .clone()
            .unwrap_or_else(|| sample_id_of(&heard.source)),
        (None, Some(stand_in)) if stand_in.is_reference => stand_in.sample_id.clone()?,
        _ => return None,
    };
    parts
        .samples
        .prints(&sample)
        .map(|prints| prints.raw)
        .filter(|raw| !raw.is_empty())
}

/// The heard master's verdict on one file: from the person's open round of this acquisition,
/// else a round of this file alone. Undecided with nothing heard, or nothing to compare with.
async fn heard_master(
    parts: &Option<HeartParts>,
    gate: &HeartGate,
    candidate: &HeartCandidate,
    path: &Path,
) -> HeardMaster {
    let (Some(parts), Some(heard)) = (parts, &gate.heard) else {
        return HeardMaster::Undecided;
    };
    match parts.master.round(&candidate.acquisition_key, &gate.user) {
        Some(round) => round.check(path).await,
        None => {
            parts
                .master
                .check_once(HeardAudio::Heard(heard.clone()), path)
                .await
        }
    }
}

/// A file's own ISRC tags and `MUSICBRAINZ_TRACKID`, as read from it.
fn labels_of(path: &str) -> FileLabels {
    let facts = tag_writer_extras::read_facts(path, true);
    FileLabels {
        isrcs: facts.isrcs,
        recording_id: facts.recording_id.filter(|id| !id.trim().is_empty()),
    }
}

fn hold(gate: HeartGate, waiting_for: WaitingFor, line: &str, question: Option<String>) -> GateBuild {
    GateBuild::Hold {
        gate,
        waiting_for,
        line: line.to_string(),
        question,
    }
}

/// R with the routing's names and nothing more: what "the same song" is when nothing named a
/// recording.
fn named_r(song: &HeartSong) -> HeardRecording {
    HeardRecording {
        isrcs: octo_core::common::SongIdentity::normalize_isrc(&song.isrc)
            .into_iter()
            .collect(),
        title: song.title.clone(),
        artist: song.artist.clone(),
        ..Default::default()
    }
}

/// R as an identity gives it: the heard recordings, every one of them as R's duplicates, the
/// song's own ISRC and the credited artists.
fn r_of(song: &HeartSong, identity: &HeardIdentity) -> HeardRecording {
    HeardRecording {
        ids: identity.recordings.clone(),
        duplicates: identity.recordings_uncapped.clone(),
        artist_ids: identity.artist_ids.clone(),
        ..named_r(song)
    }
}

/// R from the candidate contexts' R sets, the first by kind first: the first when they agree on
/// the ISRC; when they disagree, the one the heard answer reads clear against alone (a dominant
/// recording alone never picks one), else their overlap.
fn settle_candidates(
    song: &HeartSong,
    rs: Vec<HeardRecording>,
    heard_answer: Option<&AcoustIdOutcome>,
    min: f64,
) -> HeardRecording {
    let codes: HashSet<String> = rs
        .iter()
        .flat_map(|r| r.isrcs.iter().map(|code| code.to_uppercase()))
        .collect();
    if rs.len() <= 1 || codes.len() <= 1 {
        return rs.into_iter().next().unwrap_or_else(|| named_r(song));
    }
    if let Some(AcoustIdOutcome::Answered(answer)) = heard_answer {
        let clear: Vec<&HeardRecording> = rs
            .iter()
            .filter(|r| read_answer(answer, r, min).reading == Reading::Clear)
            .collect();
        if let [only] = clear.as_slice() {
            return (*only).clone();
        }
    }
    overlap(song, &rs)
}

/// The R every candidate shares: the recordings in all of them, and every candidate's ISRC, since
/// any of them may be the one heard. None shared gives no identity.
fn overlap(song: &HeartSong, rs: &[HeardRecording]) -> HeardRecording {
    let Some((first, rest)) = rs.split_first() else {
        return named_r(song);
    };
    let shared = |ids: &[String], of: fn(&HeardRecording) -> &Vec<String>| -> Vec<String> {
        ids.iter()
            .filter(|id| {
                rest.iter()
                    .all(|r| of(r).iter().any(|other| other.eq_ignore_ascii_case(id)))
            })
            .cloned()
            .collect()
    };
    let mut isrcs: Vec<String> = Vec::new();
    for code in rs.iter().flat_map(|r| &r.isrcs) {
        if !isrcs.iter().any(|known| known.eq_ignore_ascii_case(code)) {
            isrcs.push(code.clone());
        }
    }
    let mut artist_ids: Vec<String> = Vec::new();
    for id in rs.iter().flat_map(|r| &r.artist_ids) {
        if !artist_ids.contains(id) {
            artist_ids.push(id.clone());
        }
    }
    HeardRecording {
        ids: shared(&first.ids, |r| &r.ids),
        duplicates: shared(&first.duplicates, |r| &r.duplicates),
        isrcs,
        artist_ids,
        ..named_r(song)
    }
}

/// The contexts a heart reads, in order: the person's most recent played one; else their most
/// recent listed one; else every context, each catalog album once.
fn pick_contexts(contexts: &[AlbumContext], user: &str) -> Vec<AlbumContext> {
    let user = user.trim();
    let mine: Vec<&AlbumContext> = contexts
        .iter()
        .filter(|context| context.user.trim().eq_ignore_ascii_case(user))
        .collect();
    if let Some(played) = mine
        .iter()
        .filter(|context| context.played_at.is_some())
        .max_by_key(|context| context.played_at)
    {
        return vec![(*played).clone()];
    }
    if let Some(listed) = mine.iter().max_by_key(|context| context.listed_at) {
        return vec![(*listed).clone()];
    }
    let mut every: Vec<AlbumContext> = Vec::new();
    for context in contexts {
        if !every
            .iter()
            .any(|known| known.catalog_album_id == context.catalog_album_id)
        {
            every.push(context.clone());
        }
    }
    every
}

/// The song as one album context lists it: its album, its place, its ISRC and its explicit value
/// there, where the context knows them.
fn song_in(song: &HeartSong, context: &AlbumContext) -> HeartSong {
    let count = |value: Option<i32>| value.and_then(|v| u32::try_from(v).ok()).filter(|v| *v > 0);
    HeartSong {
        album: context
            .album
            .clone()
            .filter(|album| !album.trim().is_empty())
            .unwrap_or_else(|| song.album.clone()),
        disc: count(context.disc).or(song.disc),
        position: count(context.track).or(song.position),
        total_tracks: count(context.total).or(song.total_tracks),
        isrc: context
            .isrc
            .clone()
            .filter(|isrc| !isrc.trim().is_empty())
            .unwrap_or_else(|| song.isrc.clone()),
        explicit: context.explicit.or(song.explicit),
        ..song.clone()
    }
}

/// How a context's release ranks, by its group's kind alone (no carriers): an album (or a
/// soundtrack or a live album) first, then a single or an EP, then a compilation, then anything
/// else, then a release that could not be found.
async fn group_kind(questions: &HeartQuestions, song: &HeartSong, album_id: &str) -> Result<u8, HeartBusy> {
    let Some((album, _)) = questions.catalog_album(album_id).await? else {
        return Ok(4);
    };
    let releases = questions.barcode_releases(&album.upc).await?;
    let codes = questions.code_recordings(&song.isrc).await?;
    Ok(listened_release(song, &releases, &codes).map_or(4, |release| kind_of_group(&release.group)))
}

fn kind_of_group(group: &GroupFacts) -> u8 {
    let has = |kind: &str| {
        group
            .secondary_types
            .iter()
            .any(|secondary| secondary.eq_ignore_ascii_case(kind))
    };
    let primary = group.primary_type.as_str();
    if has("Compilation") {
        2
    } else if primary.eq_ignore_ascii_case("Album")
        && group.secondary_types.iter().all(|secondary| {
            secondary.eq_ignore_ascii_case("Soundtrack") || secondary.eq_ignore_ascii_case("Live")
        })
    {
        0
    } else if (primary.eq_ignore_ascii_case("Single") || primary.eq_ignore_ascii_case("EP"))
        && group.secondary_types.is_empty()
    {
        1
    } else {
        3
    }
}

/// The gates the heart chain builds, over this service: each holder's gate from what they heard,
/// their clip prints kept with their hold, what a hold waits for asked again, and one heard-master
/// round per person per pass of the chain.
#[async_trait]
impl HeartGates for DownloadVerificationService {
    async fn build(
        &self,
        provider: &str,
        external_id: &str,
        holder: &Holder,
        origin: AcquisitionOrigin,
    ) -> GateStart {
        let mut prints = self.prints_for(provider, external_id, holder);
        let built = AcoustIdRateLimiter::with_class(
            LookupClass::for_origin(origin),
            self.build_heart_gate(
                provider,
                external_id,
                &holder.user,
                origin,
                holder.heard.clone(),
                &mut prints,
            ),
        )
        .await;
        self.keep_prints(provider, external_id, &holder.user, prints);
        match built {
            GateBuild::Ready(gate) => GateStart::Ready(gate),
            GateBuild::Hold {
                gate,
                waiting_for,
                line,
                question,
            } => GateStart::Hold {
                gate,
                waiting_for,
                line,
                question,
            },
        }
    }

    fn on_filed(&self, record: &HeldAcquisition, user: &str) {
        let Some(parts) = self.heart.get() else {
            return;
        };
        let key = record.key();
        let song = (record.base_key(), user.trim().to_lowercase());
        if let Some(prints) = self.clip_prints.lock().remove(&song) {
            prints.save_to(&parts.holds, &key, user);
        }
        match record.waiting_for {
            Some(WaitingFor::Shim) => {
                let video = record
                    .holders
                    .iter()
                    .find(|holder| holder.user.trim().eq_ignore_ascii_case(user.trim()))
                    .and_then(|holder| holder.heard.as_ref())
                    .and_then(|heard| heard.source.video_id())
                    .map(str::to_string);
                if let Some(video) = video {
                    parts.samples.wait_for_shim(&key, user, &video);
                }
            }
            Some(WaitingFor::AcoustId) => {
                let resume = self.resume.lock().clone();
                if let Some(resume) = resume {
                    self.client.hold_lookup(Box::new(move || resume(&key)));
                }
            }
            Some(WaitingFor::MusicBrainz) => {
                // A hold a file's label check left: its question is asked again until it
                // answers, as a gate's own question is.
                let song = (
                    format!("{}:{}", record.provider, record.external_id),
                    user.trim().to_lowercase(),
                );
                let question = self.file_questions.lock().remove(&song);
                let resume = self.resume.lock().clone();
                if let (Some(question), Some(resume), Some(music_brainz)) =
                    (question, resume, self.music_brainz.as_ref())
                {
                    music_brainz.re_ask(
                        &question,
                        Box::new(move |ended| {
                            debug!("MusicBrainz {ended:?} the label question {key} waited on");
                            resume(&key);
                        }),
                    );
                }
            }
            _ => {}
        }
    }

    async fn owned_passes(&self, path: &str, gate: &HeartGate) -> bool {
        let candidate = HeartCandidate {
            labels: LabelSource::TheFile,
            identity: Some(octo_core::fingerprint::heart_gate::HeardSource::Library {
                path: path.to_string(),
            }),
            acquisition_key: String::new(),
        };
        let check = self
            .verify_for_heart(std::path::Path::new(path), &candidate, gate)
            .await;
        matches!(
            check.verdict,
            octo_core::fingerprint::heart_gate::GateVerdict::Accept { .. }
        )
    }

    fn open_rounds(&self, acquisition_key: &str, gates: &[HeartGate]) -> Box<dyn Send> {
        let Some(parts) = self.heart.get() else {
            return Box::new(());
        };
        let rounds: Vec<RoundGuard> = gates
            .iter()
            .filter(|gate| !matches!(gate.reading, HeardReading::NotHeard | HeardReading::AnotherSong))
            .filter_map(|gate| {
                let heard = gate.heard.clone()?;
                Some(
                    parts
                        .master
                        .open_round(acquisition_key, &gate.user, HeardAudio::Heard(heard)),
                )
            })
            .collect();
        Box::new(rounds)
    }
}

impl DownloadVerificationService {
    /// The clip prints a holder's hold already keeps, or that their last gate took.
    fn prints_for(&self, provider: &str, external_id: &str, holder: &Holder) -> ClipPrints {
        let base =
            crate::services::common::heart_acquisition_coordinator::track_base_key(provider, external_id);
        if let Some(kept) = self
            .clip_prints
            .lock()
            .get(&(base.clone(), holder.user.trim().to_lowercase()))
        {
            return kept.clone();
        }
        let Some(parts) = self.heart.get() else {
            return ClipPrints::default();
        };
        let mut prints = ClipPrints {
            whole_heard: holder.whole_heard_print.clone(),
            ..Default::default()
        };
        for record in parts.holds.records_for(&base) {
            let held = ClipPrints::of_hold(&record, &holder.user);
            prints.clip = prints.clip.or(held.clip);
            prints.whole_heard = prints.whole_heard.or(held.whole_heard);
        }
        prints
    }

    /// Keeps what a gate's clip check took until the holder's hold keeps it, and saves it at once
    /// into every hold of the song the holder is in.
    fn keep_prints(&self, provider: &str, external_id: &str, user: &str, prints: ClipPrints) {
        if prints == ClipPrints::default() {
            return;
        }
        let base =
            crate::services::common::heart_acquisition_coordinator::track_base_key(provider, external_id);
        if let Some(parts) = self.heart.get() {
            for record in parts.holds.records_for(&base) {
                if record
                    .holders
                    .iter()
                    .any(|holder| holder.user.trim().eq_ignore_ascii_case(user.trim()))
                {
                    prints.save_to(&parts.holds, &record.key(), user);
                }
            }
        }
        let mut kept = self.clip_prints.lock();
        // Bounded: a print waits here only between a gate and its hold.
        if kept.len() >= MOST_KEPT_PRINTS {
            kept.clear();
        }
        kept.insert((base, user.trim().to_lowercase()), prints);
    }
}

/// The most clip prints kept between a gate and its hold.
const MOST_KEPT_PRINTS: usize = 256;

#[cfg(test)]
#[path = "download_verification_heart_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "download_verification_gate_tests.rs"]
mod gate_tests;
