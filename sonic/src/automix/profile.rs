//! A song's transition profile: what the planner needs to know about the
//! start and the end of a song, worked out once from the whole song. A
//! server that holds the file can send it ahead, so a player needs no
//! scouting for that song, and the end of the song is measured against the
//! level of the whole song, which a scout of the end alone cannot know.

use super::envelope::{EnvelopeBuilder, SILENT_DB, SectionEnvelope, db_of};
use super::features::{
    HEAD_BODY_PERCENTILE, SectionAnalysis, SectionFeatures, Tempo, analyze_head, analyze_tail,
};

/// The version of the analysis a profile comes from. It changes whenever
/// the analysis or the profile's form changes, and a player ignores a
/// profile of a version it does not know.
pub const PROFILE_VERSION: u32 = 1;

/// Which pass of the analysis, within one version, a profile comes from. A
/// player reads every revision the same way; a server reads a song again
/// when its profile's revision is lower than this. A profile that does not
/// say comes from revision 1. Revision 2 measures the start against the
/// whole song's level, over 45 s.
pub const PROFILE_REVISION: u32 = 2;

/// How much of the start and of the end of a song a profile covers, as
/// much as a scout reads.
pub const PROFILE_HEAD_MS: i64 = 45_000;
pub const PROFILE_TAIL_MS: i64 = 60_000;

/// A profile keeps one level per this many milliseconds: enough for where
/// a blend starts and how loud each song is through it.
pub const PROFILE_HOP_MS: i32 = 100;

/// Levels go out as whole steps of this many dB above [`SILENT_DB`].
pub const LEVEL_STEP_DB: f32 = 0.5;

/// One end of a song: its level over time, a level per
/// [`PROFILE_HOP_MS`] in dBFS from `start_ms` on, and its features.
#[derive(Clone, Debug, PartialEq)]
pub struct SectionProfile {
    pub start_ms: i64,
    pub hop_ms: i32,
    pub levels: Vec<f32>,
    pub features: SectionFeatures,
}

impl SectionProfile {
    /// The profile of an analyzed section.
    pub fn of(analysis: &SectionAnalysis) -> SectionProfile {
        let envelope = &analysis.envelope;
        let per = (PROFILE_HOP_MS / envelope.hop_ms).max(1) as usize;
        let levels = envelope
            .db
            .chunks(per)
            .map(|hops| {
                let power: f64 = hops.iter().map(|&d| 10f64.powf(d as f64 / 10.0)).sum();
                db_of(power / hops.len() as f64)
            })
            .collect();
        SectionProfile {
            start_ms: envelope.start_ms,
            hop_ms: envelope.hop_ms * per as i32,
            levels,
            features: analysis.features.clone(),
        }
    }

    /// The section as the planner takes it.
    pub fn analysis(&self) -> SectionAnalysis {
        let size = self.levels.len();
        let envelope = SectionEnvelope::new(
            self.start_ms,
            self.hop_ms.max(1),
            self.levels.clone(),
            self.levels.clone(),
            vec![0.0; size],
            vec![0.0; size],
        );
        SectionAnalysis::from_features(envelope, self.features.clone())
    }
}

/// What a player needs to plan the transitions into and out of one song.
/// `body_db` is the level of the whole song (`None` when it is silent) and
/// `tempo` its beat over the whole song, the same as a live tap that heard
/// all of it would give. Both ends of the song are measured against the
/// whole song's level. `revision` is the analysis pass it comes from
/// ([`PROFILE_REVISION`]).
#[derive(Clone, Debug, PartialEq)]
pub struct TransitionProfile {
    pub version: u32,
    pub revision: u32,
    pub duration_ms: i64,
    pub body_db: Option<f64>,
    pub tempo: Option<Tempo>,
    pub head: SectionProfile,
    pub tail: SectionProfile,
}

impl TransitionProfile {
    /// The profile of decoded sound: interleaved samples between -1 and 1.
    pub fn analyze(samples: &[f32], sample_rate: u32, channels: u32) -> TransitionProfile {
        let mut builder = EnvelopeBuilder::new(sample_rate, channels, 0);
        builder.push(samples);
        Self::of_envelope(&builder.into_envelope())
    }

    /// The profile of a whole song's envelope, which starts at 0.
    pub fn of_envelope(song: &SectionEnvelope) -> TransitionProfile {
        let duration_ms = song.end_ms();
        let whole = SectionAnalysis::of(song.clone(), None, None, HEAD_BODY_PERCENTILE);
        let body_db = Some(whole.features.body_db).filter(|&b| b > SILENT_DB as f64);
        let head = analyze_head(song.slice(0.0, PROFILE_HEAD_MS.min(duration_ms) as f64), None, body_db);
        let tail_from = (duration_ms - PROFILE_TAIL_MS).max(0);
        let tail = analyze_tail(song.slice(tail_from as f64, duration_ms as f64), None, body_db);
        TransitionProfile {
            version: PROFILE_VERSION,
            revision: PROFILE_REVISION,
            duration_ms,
            body_db,
            tempo: whole.features.tempo,
            head: SectionProfile::of(&head),
            tail: SectionProfile::of(&tail),
        }
    }

    /// The start of the song as the planner takes it, measured against the
    /// whole song's level from revision 2 on.
    pub fn head_analysis(&self) -> SectionAnalysis {
        self.head.analysis()
    }

    /// The end of the song as the planner takes it, already measured
    /// against the whole song's level.
    pub fn tail_analysis(&self) -> SectionAnalysis {
        self.tail.analysis()
    }

    /// The whole song's tempo when it is trusted, as a live tap gives it.
    pub fn tempo_prior(&self) -> Option<f64> {
        self.tempo.filter(Tempo::confident).map(|t| t.bpm)
    }

    /// The same profile with every level rounded to the step it is sent in.
    pub fn rounded(&self) -> TransitionProfile {
        let round = |section: &SectionProfile| SectionProfile {
            levels: section.levels.iter().map(|&db| level_of_code(level_code(db))).collect(),
            ..section.clone()
        };
        TransitionProfile { head: round(&self.head), tail: round(&self.tail), ..self.clone() }
    }
}

/// A level as the byte it is sent as: whole [`LEVEL_STEP_DB`] steps above
/// [`SILENT_DB`], up to 255.
pub fn level_code(db: f32) -> u8 {
    ((db - SILENT_DB) / LEVEL_STEP_DB).round().clamp(0.0, 255.0) as u8
}

/// The level a sent byte stands for.
pub fn level_of_code(code: u8) -> f32 {
    SILENT_DB + code as f32 * LEVEL_STEP_DB
}
