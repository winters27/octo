//! One downloaded file judged at a source's check point. A heart's or a play's file is judged by
//! each asker's own gate ([`DownloadVerificationService::verify_for_heart`]) and never by the
//! shipped `verify` or the song's YouTube preview; every other origin keeps the shipped checks,
//! with the AcoustID reading's refusals added when its R is known, and an import or replacement
//! whose lookup would have to wait for its bulk turn moves its file to the one hold path instead.

use octo_core::fingerprint::heart_gate::{
    AcquisitionOrigin, FileFacts, GateVerdict, HeardReading, HeardSource, HeartGate, ImportJudgement,
    Refusal, TagClaim, Undecided, WaitingFor, import_judgement,
};
use octo_core::fingerprint::verification::{RecordingProof, read_answer};

use super::*;
use crate::services::common::album_gates::{self, AlbumGateTrack, TrackCheck};
use crate::services::common::heart_acquisition_coordinator::waiting_line;
use crate::services::common::song_finder::ACOUST_ID_BUSY;
use crate::services::common::soulseek_hold_store::held_root;
use crate::services::common::{AcquisitionHeld, GateOutcome, HeldAcquisition, HeldKind, PlayOutcome};
use crate::services::fingerprint::download_verification_service::{HeartCandidate, HeartCheck, LabelSource};

/// What a source's check point did with one downloaded file.
#[derive(Debug)]
pub(crate) enum FileJudged {
    /// Keep it, tagged by this verdict.
    Keep(VerificationResult),
    /// It is not kept. `shared` when the file is not the song at all, so the peer's file is
    /// remembered for every download; otherwise only the gates that refused it remember it.
    /// `verdict` is the shipped reading of its sound, for the words and the YouTube loop.
    Refuse {
        reason: String,
        shared: bool,
        verdict: VerificationResult,
    },
    /// It waits in the one hold path, where it was moved, and the acquisition holds.
    Hold(AcquisitionHeld),
    /// A play's file nothing could prove: thrown away, and the play keeps none.
    NotProven(String),
}

/// Whether a gate is a heart's or a play's, judged by its asker's own gate wherever it runs.
fn personal(gate: &HeartGate) -> bool {
    matches!(
        gate.origin,
        AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Play
    )
}

/// Whether a download is judged by its askers' own gates: a heart's or a play's with a gate.
pub(crate) fn gated(download: &TrackDownload) -> bool {
    matches!(
        download.origin,
        Some(AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Play)
    ) && !download.gates.is_empty()
}

/// Whether every gate of a download reads that nothing proves which recording played, so only the
/// heard audio itself or its master can be kept: YouTube's copies are never tried for it.
pub(crate) fn only_heard_or_master(download: &TrackDownload) -> bool {
    gated(download)
        && download
            .gates
            .iter()
            .all(|gate| gate.reading == HeardReading::OnlyHeardOrMaster)
}

/// The words a gate's verdict leaves in the downloads log.
fn verdict_words(verdict: &GateVerdict) -> (&'static str, &'static str) {
    match verdict {
        GateVerdict::Accept { by, .. } => match by {
            octo_core::fingerprint::heart_gate::Acceptance::HeardAudio => {
                ("The very audio you heard", "Kept as the copy that played")
            }
            octo_core::fingerprint::heart_gate::Acceptance::Sound => (
                "AcoustID: the recording you heard",
                "Its sound proves the version",
            ),
            octo_core::fingerprint::heart_gate::Acceptance::HeardMaster => {
                ("The master you heard", "It matches what played, window by window")
            }
        },
        GateVerdict::Refuse(refusal) => (
            "Not the version you heard; thrown away",
            match refusal {
                Refusal::DiffersFromHeard => "It does not sound like what played",
                Refusal::Label => "Its own tags name another recording",
                Refusal::AnotherRecording => "AcoustID names another recording",
                Refusal::NoEntry => "AcoustID has no entry for its sound, and it is not the master you heard",
                Refusal::Unknown => "AcoustID cannot say what it is, and it is not the master you heard",
                Refusal::Unreadable => "AcoustID could not check it by its sound",
            },
        ),
        GateVerdict::Undecided(Undecided::KeepHeardCopy) => (
            "Not provably the version you heard; thrown away",
            "AcoustID cannot tell this version apart, and it is not the master you heard",
        ),
        GateVerdict::Undecided(Undecided::NotYetProven) => (
            "Not provably the version you want; thrown away",
            "Nothing was heard yet to prove it by",
        ),
        GateVerdict::Hold(_) => ("Waiting before it is kept", "Moved aside until it can be checked"),
    }
}

/// Why a gate did not keep a file, as the rest of a sentence about it ("the file is not kept:
/// it does not sound like what played").
fn not_kept(detail: &str) -> String {
    let mut letters = detail.chars();
    match letters.next() {
        Some(first) => format!("is not kept: {}{}", first.to_lowercase(), letters.as_str()),
        None => "is not kept".to_string(),
    }
}

/// A hearted album's track gate's verdict on its file, under the album's rule: the catalog track
/// is the song being downloaded, and the file fits it by its own title tag and length.
fn album_track_verdict(
    gate: &HeartGate,
    check: &HeartCheck,
    song: &Song,
    title: Option<&str>,
    seconds: Option<i32>,
) -> GateVerdict {
    let r = gate.r.clone().unwrap_or_default();
    let track = AlbumGateTrack {
        title: song.title.clone(),
        duration_seconds: song.duration.or(gate.heard_seconds),
        ..Default::default()
    };
    album_gates::album_verdict(TrackCheck {
        track: 0,
        verdict: check.verdict.clone(),
        reading: check.reading.clone(),
        no_recording: r.ids.is_empty(),
        fits: album_gates::fits(&track, title, seconds),
    })
}

/// The line a row holds with while a file waits for `waiting_for`.
fn file_line(waiting_for: WaitingFor) -> &'static str {
    match waiting_for {
        WaitingFor::AcoustId => "Waiting for AcoustID",
        other => waiting_line(other),
    }
}

impl SoulseekDownloadService {
    /// Judges one downloaded file at a source's check point. `peer` is the Soulseek peer and file
    /// it came from, when it did; `identity` is the file's own identity when it is a copy of the
    /// kind heard audio is.
    pub(crate) async fn judge_file(
        &self,
        base: &BaseDownloadService,
        routing: &SoulseekRouting,
        download: &mut TrackDownload,
        path: &str,
        identity: Option<HeardSource>,
        length_known: bool,
    ) -> FileJudged {
        if gated(download) {
            return self.judge_by_gates(base, download, path, identity).await;
        }
        let judged = self
            .judge_shipped(base, routing, download, path, length_known)
            .await;
        if matches!(judged, FileJudged::Keep(_)) {
            // What the checks said of an earlier candidate they refused is not what they said of
            // the file kept: the kept file is everyone's who asked, as the shipped checks keep it.
            download.ledger.clear();
            self.judge_joiners(download, path, identity).await;
        }
        judged
    }

    /// A heart or a play that joined another origin's download is judged by its own gate once the
    /// download's own checks keep the file: the file lands for the download's own askers, and each
    /// joiner counts it as theirs only when their own gate passes it.
    pub(crate) async fn judge_joiners(
        &self,
        download: &TrackDownload,
        path: &str,
        identity: Option<HeardSource>,
    ) {
        let joiners: Vec<&HeartGate> = download.gates.iter().filter(|gate| personal(gate)).collect();
        if gated(download) || joiners.is_empty() {
            return;
        }
        let ledger = &download.ledger;
        if let Some(origin) = download.origin {
            let own = download
                .gates
                .iter()
                .filter(|gate| gate.origin == origin)
                .map(|gate| gate.user.clone())
                .chain(download.requested_by.iter().cloned());
            for user in own {
                if ledger.outcome_for(&user, origin).is_none() {
                    ledger.record(&user, origin, GateOutcome::Accepted);
                }
            }
        }
        let candidate = HeartCandidate {
            labels: LabelSource::TheFile,
            identity,
            acquisition_key: download.acquisition_key.clone(),
        };
        for gate in joiners {
            let check = self
                .verification
                .verify_for_heart(Path::new(path), &candidate, gate)
                .await;
            info!(
                "The gate of {} ({}), who joined {}, for {path}: {:?}",
                gate.user,
                gate.origin.name(),
                download.acquisition_key,
                check.verdict
            );
            let outcome = match &check.verdict {
                GateVerdict::Accept { .. } => GateOutcome::Accepted,
                GateVerdict::Hold(waiting) => GateOutcome::Held(*waiting),
                other => GateOutcome::NotAccepted(not_kept(verdict_words(other).1)),
            };
            ledger.record(&gate.user, gate.origin, outcome);
            if let GateVerdict::Accept { by, .. } = check.verdict {
                ledger.record_accepted(&gate.user, gate.origin, by);
            }
        }
    }

    /// A heart's or a play's file, judged by each asker's own gate. Kept when one gate accepts it
    /// (each gate's verdict written for its asker); held in the one hold path when none accepts and
    /// one holds (a play never holds: its file is thrown away); otherwise not kept.
    async fn judge_by_gates(
        &self,
        base: &BaseDownloadService,
        download: &mut TrackDownload,
        path: &str,
        identity: Option<HeardSource>,
    ) -> FileJudged {
        let track_key = download.song.external_id.clone().unwrap_or_default();
        let candidate = HeartCandidate {
            labels: LabelSource::TheFile,
            identity,
            acquisition_key: download.acquisition_key.clone(),
        };
        let mut checks = Vec::with_capacity(download.gates.len());
        // A hearted album's track is filed by the album's rule, which reads the file's own title
        // tag and length beside its sound.
        let album_file = download
            .gates
            .iter()
            .any(|gate| gate.origin == AcquisitionOrigin::AlbumHeart)
            .then(|| album_gates::title_and_length(path));
        for gate in &download.gates {
            let mut check = self
                .verification
                .verify_for_heart(Path::new(path), &candidate, gate)
                .await;
            if let (AcquisitionOrigin::AlbumHeart, Some((title, seconds))) = (gate.origin, &album_file) {
                check.verdict = album_track_verdict(gate, &check, &download.song, title.as_deref(), *seconds);
            }
            let (text, detail) = verdict_words(&check.verdict);
            base.log_step_heard(
                &track_key,
                acquisition_event_kinds::CHECK,
                text,
                Some(detail),
                check.verification.heard(),
            );
            info!(
                "The gate of {} ({}) for {track_key}: {:?}",
                gate.user,
                gate.origin.name(),
                check.verdict
            );
            checks.push((gate.clone(), check));
        }
        let ledger = &download.ledger;
        let outcome_of = |verdict: &GateVerdict| match verdict {
            GateVerdict::Accept { .. } => GateOutcome::Accepted,
            GateVerdict::Hold(waiting) => GateOutcome::Held(*waiting),
            other => GateOutcome::NotAccepted(verdict_words(other).1.to_string()),
        };

        if let Some((gate, check)) = checks
            .iter()
            .find(|(_, check)| matches!(check.verdict, GateVerdict::Accept { .. }))
        {
            for (other, its) in &checks {
                ledger.record(&other.user, other.origin, outcome_of(&its.verdict));
                if let GateVerdict::Accept { by, .. } = its.verdict {
                    ledger.record_accepted(&other.user, other.origin, by);
                }
            }
            let kept = DownloadVerificationService::kept_verification(check, gate);
            if let GateVerdict::Accept {
                tags: TagClaim::Recording { isrcs, .. },
                ..
            } = &check.verdict
                && !isrcs.is_empty()
                && !download
                    .song
                    .isrc
                    .as_deref()
                    .is_some_and(|own| isrcs.iter().any(|code| code.eq_ignore_ascii_case(own)))
            {
                download.song.isrc = isrcs.first().cloned();
            }
            if matches!(
                check.verdict,
                GateVerdict::Accept {
                    tags: TagClaim::AsHeard,
                    ..
                }
            ) {
                // Kept as heard: no tag may claim a catalog recording the sound did not prove.
                download.song.isrc = None;
                download.song.music_brainz_recording_id = None;
            }
            return FileJudged::Keep(kept);
        }

        let holding: Vec<&(HeartGate, _)> = checks
            .iter()
            .filter(|(gate, check)| {
                matches!(check.verdict, GateVerdict::Hold(_)) && gate.origin != AcquisitionOrigin::Play
            })
            .collect();
        if let Some((gate, first)) = holding.first() {
            let GateVerdict::Hold(waiting_for) = first.verdict else {
                unreachable!("only holds were kept");
            };
            let line =
                if gate.origin == AcquisitionOrigin::AlbumHeart && waiting_for == WaitingFor::NotYetProven {
                    album_gates::tracks_wait_line(1)
                } else {
                    file_line(waiting_for).to_string()
                };
            let Some(moved) = move_to_hold(base, &download.song, path) else {
                return FileJudged::Refuse {
                    reason: "the file could not be moved aside to wait".to_string(),
                    shared: false,
                    verdict: first.verification.clone(),
                };
            };
            for (gate, check) in &checks {
                ledger.record(&gate.user, gate.origin, outcome_of(&check.verdict));
            }
            return FileJudged::Hold(AcquisitionHeld {
                waiting_for,
                line,
                files: vec![moved],
                users: holding.iter().map(|(gate, _)| gate.user.clone()).collect(),
            });
        }

        let plays_only = download
            .gates
            .iter()
            .all(|gate| gate.origin == AcquisitionOrigin::Play);
        let (_, first) = &checks[0];
        let reason = not_kept(verdict_words(&first.verdict).1);
        if plays_only
            && checks
                .iter()
                .any(|(_, check)| matches!(check.verdict, GateVerdict::Hold(_)))
        {
            // A play never holds: what its gate cannot judge now is thrown away.
            let _ = std::fs::remove_file(path);
            return FileJudged::NotProven(reason);
        }
        // Whoever's gate could not tell this version apart keeps what they heard if nothing better
        // is proven: their chain reads it here.
        for (gate, check) in &checks {
            if check.verdict == GateVerdict::Undecided(Undecided::KeepHeardCopy) {
                ledger.record(&gate.user, gate.origin, GateOutcome::Undecided);
            }
        }
        // Not the song at all only when its audio cannot even be decoded; every other refusal is
        // one person's version, remembered for them alone.
        let shared = checks.iter().all(|(_, check)| {
            check.verdict == GateVerdict::Refuse(Refusal::Unreadable)
                && check.verification.verdict == VerificationVerdict::Mismatch
        });
        FileJudged::Refuse {
            reason,
            shared,
            verdict: first.verification.clone(),
        }
    }

    /// Every other origin: the shipped checks exactly as they are, run on the origin's class (an
    /// import's and a replacement's only when their bulk turn is free at once, else the file
    /// waits in the one hold path), with the AcoustID reading's refusals added when the gate
    /// carries R: a reading of another recording, or a label naming another, refuses a file the
    /// shipped check would confirm. A confirmed file the reading cannot settle lands as "version
    /// not proven", with no recording tags.
    async fn judge_shipped(
        &self,
        base: &BaseDownloadService,
        routing: &SoulseekRouting,
        download: &mut TrackDownload,
        path: &str,
        length_known: bool,
    ) -> FileJudged {
        let (_, title) = names(routing);
        let track_key = download.song.external_id.clone().unwrap_or_default();
        let class = download
            .origin
            .map_or_else(AcoustIdRateLimiter::class_now, LookupClass::for_origin);
        let bulk = matches!(
            download.origin,
            Some(AcquisitionOrigin::Import | AcquisitionOrigin::Replacement)
        );
        let isrc = download.song.isrc.clone().or(routing.isrc.clone());
        let takes = self.verification.takes_for_routing(routing);
        let asked = AcoustIdRateLimiter::with_class(
            class,
            self.verification.verify(
                path,
                routing.artist.as_deref(),
                routing.title.as_deref(),
                isrc.as_deref(),
                &takes,
            ),
        );
        let (mut verdict, turned_away) = if bulk {
            AcoustIdRateLimiter::if_free(asked).await
        } else {
            (asked.await, false)
        };
        if turned_away {
            // A bulk wait never keeps the download worker: the file waits for its turn aside.
            return match move_to_hold(base, &download.song, path) {
                Some(moved) => {
                    info!("AcoustID's bulk turn is not free for {track_key}; its file waits for it: {moved}");
                    FileJudged::Hold(AcquisitionHeld {
                        waiting_for: WaitingFor::AcoustId,
                        line: file_line(WaitingFor::AcoustId).to_string(),
                        files: vec![moved],
                        users: Vec::new(),
                    })
                }
                None => FileJudged::Keep(verdict),
            };
        }
        // A Find songs pick is never held: with AcoustID not answering, its file is thrown away and
        // the pick ends with words the person can act on.
        if download.origin == Some(AcquisitionOrigin::SongFinderPick)
            && verdict.reason == octo_core::fingerprint::InconclusiveReason::LookupFailed
            && self.verification.is_fingerprinting_enabled()
        {
            info!("AcoustID did not answer for the pick of {track_key}; its file is thrown away: {path}");
            let _ = std::fs::remove_file(path);
            return FileJudged::Hold(AcquisitionHeld {
                waiting_for: WaitingFor::AcoustId,
                line: ACOUST_ID_BUSY.to_string(),
                files: Vec::new(),
                users: Vec::new(),
            });
        }
        if verdict.verdict == VerificationVerdict::Mismatch && file_exists(path) {
            let file_ms = TagFile::open(path)
                .ok()
                .map(|file| file.duration().as_millis() as i64);
            verdict =
                DownloadVerificationService::settle_stray_link(verdict, &title, routing.duration, file_ms);
        }
        if verdict.verdict == VerificationVerdict::Mismatch {
            return FileJudged::Refuse {
                reason: verdict.deny_reason.clone(),
                shared: true,
                verdict,
            };
        }

        if self.import_refuses(base, download, path, &mut verdict) {
            return FileJudged::Refuse {
                reason: "is another recording by AcoustID's reading".to_string(),
                shared: false,
                verdict,
            };
        }
        if let Some(why) = self.denied_by_sound(base, download, &verdict) {
            return FileJudged::Refuse {
                reason: why.to_string(),
                shared: false,
                verdict,
            };
        }

        match self
            .refused_by_preview_on(base, routing, &track_key, path, verdict, length_known, bulk)
            .await
        {
            PreviewSays::Keep(kept) => FileJudged::Keep(kept),
            PreviewSays::Refuse(reason) => FileJudged::Refuse {
                verdict: VerificationResult {
                    deny_reason: reason.clone(),
                    ..Default::default()
                },
                reason,
                shared: true,
            },
        }
    }

    /// The import rule, for an origin that keeps the shipped checks and whose gate carries R: true
    /// when AcoustID's reading names another recording, or one of the file's own labels does, so
    /// the file is refused whatever the shipped check said. A confirmed file the reading cannot
    /// settle is marked "version not proven", and carries no recording tags.
    pub(crate) fn import_refuses(
        &self,
        base: &BaseDownloadService,
        download: &TrackDownload,
        path: &str,
        verdict: &mut VerificationResult,
    ) -> bool {
        let track_key = download.song.external_id.clone().unwrap_or_default();
        let Some(r) = download
            .gates
            .first()
            .and_then(|gate| gate.r.clone())
            .filter(|r| !r.ids.is_empty())
        else {
            return false;
        };
        let Some(answer) = verdict.lookup.as_ref().and_then(|lookup| lookup.answer.clone()) else {
            return false;
        };
        let min = base.current_soulseek_settings().effective_min_score_fraction();
        let reading = read_answer(&answer, &r, min);
        let labels = {
            let facts = octo_media::tags::tag_writer_extras::read_facts(path, true);
            octo_core::fingerprint::heart_gate::FileLabels {
                isrcs: facts.isrcs,
                recording_id: facts.recording_id,
            }
        };
        let facts = FileFacts::new(
            octo_core::fingerprint::acoust_id_client::AcoustIdOutcome::Answered(answer),
            labels,
        );
        match import_judgement(&reading, &facts, &r) {
            ImportJudgement::Refuse => {
                warn!(
                    "AcoustID's reading names another recording than {track_key} asked for in {path}; refusing it"
                );
                base.log_step_heard(
                    &track_key,
                    acquisition_event_kinds::CHECK,
                    "Another recording; thrown away",
                    Some("AcoustID, or the file's own tags, name another recording than the one asked for"),
                    verdict.heard(),
                );
                if let Some(origin) = download.origin {
                    for gate in download.gates.iter().filter(|gate| gate.origin == origin) {
                        download.ledger.record(
                            &gate.user,
                            origin,
                            GateOutcome::NotAccepted("another recording".to_string()),
                        );
                    }
                }
                true
            }
            ImportJudgement::Stands { proven: false }
                if verdict.verdict == VerificationVerdict::Confirmed =>
            {
                info!("{track_key} is confirmed, but its sound cannot settle the version: {path}");
                verdict.proof = RecordingProof::NotProven;
                base.log_step(
                    &track_key,
                    acquisition_event_kinds::CHECK,
                    "version not proven",
                    Some("The checks confirm the song, but its sound cannot tell which version it is"),
                );
                false
            }
            ImportJudgement::Stands { .. } => false,
        }
    }

    /// Why a Wrong song or Wrong version replacement refuses this file by its original's sound (its
    /// dominant recording is the original's again, or its song is), or None.
    pub(crate) fn denied_by_sound(
        &self,
        base: &BaseDownloadService,
        download: &TrackDownload,
        verdict: &VerificationResult,
    ) -> Option<&'static str> {
        let deny = download.deny.as_ref()?;
        let answer = verdict.lookup.as_ref()?.answer.as_ref()?;
        let min = base.current_soulseek_settings().effective_min_score_fraction();
        let why = deny.refuses(answer, min)?;
        let track_key = download.song.external_id.clone().unwrap_or_default();
        info!("A replacement for {track_key} {why}; refusing it");
        base.log_step_heard(
            &track_key,
            acquisition_event_kinds::CHECK,
            "The copy you said was wrong, again; thrown away",
            Some("AcoustID hears what the copy it was to replace is"),
            verdict.heard(),
        );
        Some(why)
    }

    /// Whether every gate of a gated download has refused this peer's file before.
    pub(crate) fn denied_for_gates(&self, download: &TrackDownload, username: &str, filename: &str) -> bool {
        let song = download.song.external_id.clone().unwrap_or_default();
        gated(download)
            && download.gates.iter().all(|gate| {
                self.rejected_peers.is_denied_for_heart(
                    &song,
                    &gate.r.clone().unwrap_or_default(),
                    Some(username),
                    Some(filename),
                )
            })
    }

    /// Remembers a peer's file one heart's or play's gate refused, for that gate's song and R only.
    pub(crate) fn deny_for_gates(
        &self,
        download: &TrackDownload,
        username: &str,
        filename: &str,
        reason: &str,
    ) {
        let song = download.song.external_id.clone().unwrap_or_default();
        for gate in &download.gates {
            self.rejected_peers.deny_for_heart(
                &song,
                &gate.r.clone().unwrap_or_default(),
                Some(username),
                Some(filename),
                reason,
            );
        }
    }
}

impl SoulseekDownloadService {
    /// A0 for a heart's or a play's own heard video: a whole copy of that video's own stream at the
    /// heard format and size, remuxed without re-encoding (AAC in mp4 stays `.m4a`, Opus in webm
    /// becomes `.opus`) so the library holds the very audio stream that played, then judged with
    /// its own identity. None when no gate heard a video or no such copy is had now; the error when
    /// the file waits or a play's is not proven. The shim's MP3 transcode is never this.
    pub(crate) async fn heard_stream_copy(
        &self,
        base: &BaseDownloadService,
        routing: &SoulseekRouting,
        download: &mut TrackDownload,
        incoming: &str,
    ) -> Option<anyhow::Result<String>> {
        let copies = self.preview_copies.clone()?;
        let heard: Vec<HeardSource> = download
            .gates
            .iter()
            .filter(|gate| !matches!(gate.reading, HeardReading::NotHeard | HeardReading::AnotherSong))
            .filter_map(|gate| gate.heard.as_ref())
            .map(|heard| heard.source.clone())
            .filter(|source| source.video_id().is_some())
            .collect();
        let length_known = routing.duration.is_some_and(|s| s > 0);
        for source in heard {
            let HeardSource::Video {
                id,
                itag,
                content_type,
                ..
            } = &source
            else {
                continue;
            };
            let crate::services::prefetch::HeardCopy::Ready(copy) = copies.heard_copy(&source).await else {
                continue;
            };
            let Ok(size) = std::fs::metadata(&copy).map(|meta| meta.len()) else {
                continue;
            };
            let kind = copies
                .copy_facts(id)
                .filter(|(known, _)| *known == size)
                .map(|(_, kind)| kind)
                .unwrap_or_else(|| content_type.clone());
            let identity = HeardSource::Video {
                id: id.clone(),
                size: Some(size),
                content_type: kind.clone(),
                itag: *itag,
            };
            let Some(remuxed) = remux_audio(&copy, &kind, incoming, id).await else {
                continue;
            };
            match self
                .judge_file(base, routing, download, &remuxed, Some(identity), length_known)
                .await
            {
                FileJudged::Keep(verdict) => {
                    info!("Kept the stream of video {id} that played, as it is: {remuxed}");
                    download.fetched_from = Some("YouTube".to_string());
                    verdict.apply_tags_to(&mut download.song);
                    download.song.verification = Some(Box::new(verdict));
                    return Some(Ok(remuxed));
                }
                FileJudged::Hold(held) => return Some(Err(held.into())),
                FileJudged::NotProven(_) => {
                    let _ = std::fs::remove_file(&remuxed);
                    return Some(Err(not_proven()));
                }
                FileJudged::Refuse { .. } => {
                    let _ = std::fs::remove_file(&remuxed);
                }
            }
        }
        None
    }
}

impl SoulseekDownloadService {
    /// A song fetched ahead, before a heart or a play keeps it: kept only as the very copy a person
    /// heard (A0), or by its own sound's clear reading (A1) with proof 2's refusal; anything else
    /// passes it over and the chain runs.
    pub(crate) async fn judge_ahead_copy(
        &self,
        download: &mut TrackDownload,
        path: &str,
        identity: HeardSource,
    ) -> bool {
        let candidate = HeartCandidate {
            labels: LabelSource::TheFile,
            identity: Some(identity),
            acquisition_key: download.acquisition_key.clone(),
        };
        let mut checks = Vec::with_capacity(download.gates.len());
        for gate in &download.gates {
            let check = self
                .verification
                .verify_for_heart(Path::new(path), &candidate, gate)
                .await;
            checks.push((gate.clone(), check));
        }
        let kept = checks.iter().find(|(_, check)| {
            matches!(
                check.verdict,
                GateVerdict::Accept {
                    by: octo_core::fingerprint::heart_gate::Acceptance::HeardAudio
                        | octo_core::fingerprint::heart_gate::Acceptance::Sound,
                    ..
                }
            )
        });
        let Some((gate, check)) = kept else {
            info!(
                "The copy of {} fetched ahead is not provably what was heard; the chain runs",
                download.track_id
            );
            return false;
        };
        for (other, its) in &checks {
            let outcome = match &its.verdict {
                GateVerdict::Accept { .. } => GateOutcome::Accepted,
                GateVerdict::Hold(waiting) => GateOutcome::Held(*waiting),
                other => GateOutcome::NotAccepted(not_kept(verdict_words(other).1)),
            };
            download.ledger.record(&other.user, other.origin, outcome);
            if let GateVerdict::Accept { by, .. } = its.verdict {
                download.ledger.record_accepted(&other.user, other.origin, by);
            }
        }
        let verdict = DownloadVerificationService::kept_verification(check, gate);
        verdict.apply_tags_to(&mut download.song);
        download.song.verification = Some(Box::new(verdict));
        true
    }
}

/// The audio stream of a whole copy of a video's stream, remuxed into its own container without
/// re-encoding: AAC in mp4 as `.m4a`, Opus in webm as `.opus`. None when ffmpeg is missing or
/// failed.
pub(crate) async fn remux_audio(
    copy: &Path,
    content_type: &str,
    folder: &str,
    video_id: &str,
) -> Option<String> {
    let extension = if content_type.to_ascii_lowercase().contains("webm") {
        "opus"
    } else {
        "m4a"
    };
    std::fs::create_dir_all(folder).ok()?;
    let out = combine(folder, &format!("{video_id}-{}.{extension}", new_guid()));
    let run = tokio::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(copy)
        .args(["-vn", "-map", "0:a:0", "-c:a", "copy"])
        .arg(&out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    match run {
        Ok(output) if output.status.success() && file_exists(&out) => Some(out),
        Ok(output) => {
            warn!(
                "ffmpeg could not remux the stream of {}: {}",
                copy.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let _ = std::fs::remove_file(&out);
            None
        }
        Err(e) => {
            warn!("ffmpeg could not run to remux a heard stream: {e}");
            None
        }
    }
}

/// Moves a file into the one hold path, `<download>/.octo-incoming/held/<song>/`, which no sweep
/// touches. A file already there stays. None when it cannot be moved.
pub(crate) fn move_to_hold(base: &BaseDownloadService, song: &Song, path: &str) -> Option<String> {
    let download_path = base.download_path();
    if download_path.is_empty() || !file_exists(path) {
        return None;
    }
    let root = held_root(Path::new(&download_path));
    if Path::new(path).starts_with(&root) {
        return Some(path.to_string());
    }
    let record = HeldAcquisition {
        kind: HeldKind::Track,
        provider: song.external_provider.clone().unwrap_or_default(),
        external_id: song.external_id.clone().unwrap_or_default(),
        ..Default::default()
    };
    let folder = record.hold_folder(Path::new(&download_path));
    std::fs::create_dir_all(&folder).ok()?;
    let target = folder.join(format!("{}-{}", new_guid(), file_name(path)));
    if std::fs::rename(path, &target).is_err() {
        std::fs::copy(path, &target).ok()?;
        let _ = std::fs::remove_file(path);
    }
    Some(target.to_string_lossy().to_string())
}

/// A play whose file nothing could prove ends with this error, so a waiting play serves its
/// stand-in.
pub(crate) fn not_proven() -> anyhow::Error {
    PlayOutcome::NotProven.into()
}
