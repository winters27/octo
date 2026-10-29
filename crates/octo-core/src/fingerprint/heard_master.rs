//! Whether a file is the very master that was heard, judged by sound in both directions.
//!
//! The heard audio and the file are decoded to [`RATE`] Hz stereo, lined up to the sample and
//! compared block by block on the heard audio's timeline. After one gain per channel for the
//! whole song, every second and every quarter second has to match: nothing of the heard audio
//! missing from the file and nothing added to it. A remix, a remaster, a mono fold, another edit
//! or one muted word leaves some window about as loud as the music; two lossy copies of one master
//! stay far under it everywhere.
//!
//! Neither song is ever held whole. Each file is decoded three times, each pass keeping only what
//! the next step needs:
//!
//! 1. [`FirstPass`]: the onset envelope, each second's energy and the mono at an eighth of the
//!    rate, from which [`RefinePlan`] picks up to [`TOP_COARSE`] candidate shifts;
//! 2. [`StretchPass`]: the full-rate mono over the stretches the plan asks for, from which
//!    [`RefinePlan::pick_shift`] picks the one shift;
//! 3. [`BlockSums`]: both files in lockstep at that shift, summed per block of [`BLOCK`] samples.
//!
//! [`verdict`] reads the block sums. [`compare`] runs the three passes over two songs held in
//! memory, and [`F32leFrames`] turns a decoder's `f32le` bytes into the frames every pass takes.

use std::collections::VecDeque;
use std::ops::{Range, RangeInclusive};

/// The rate both files are decoded to, in Hz, as stereo 32-bit floats. The decoder's anti-alias
/// filter keeps the content under about 5 kHz, where the music is and most coding noise is not.
pub const RATE: usize = 11_025;
/// Samples in one onset envelope frame.
pub const HOP: usize = 1024;
/// How far the coarse search looks either way, in envelope frames: 15 seconds.
pub const COARSE_FRAMES: i64 = (15 * RATE / HOP) as i64;
/// How many coarse shifts are refined, each at least 2 frames from a better one.
pub const TOP_COARSE: usize = 3;
/// Seconds of the heard audio each candidate is refined over: the loudest that both files cover.
pub const REFINE_SECONDS: usize = 20;
/// The coarse refine runs on the mono averaged over runs of this many samples.
pub const DECIMATE: usize = 8;
/// How far either way of the decimated lag the full-rate refine looks, in samples.
pub const FINE_SAMPLES: i64 = 16;
/// Samples in one block, about an eighth of a second. A block is in a file when more than half
/// of it lies inside that file, which absorbs an encoder's few milliseconds of padding.
pub const BLOCK: usize = 1378;
/// Blocks in a short window, a quarter second, stepped one block at a time.
pub const SHORT_BLOCKS: usize = 2;
/// Blocks in a long window, one second, stepped half a window at a time.
pub const LONG_BLOCKS: usize = 8;
/// A block is loud when its mean square reaches the louder file's loudest long window this many
/// dB down; a window loud in neither file is skipped.
pub const QUIET_DB: f64 = -35.0;
/// Each file's loudest long window has to reach this, in dB under full scale: a silent side
/// proves nothing.
pub const SILENT_DBFS: f64 = -60.0;
/// Each channel's gain has to lie within `1 / MAX_GAIN` to `MAX_GAIN`.
pub const MAX_GAIN: f64 = 8.0;
/// The fewest long windows that have to count.
pub const MIN_LONG_WINDOWS: usize = 40;
/// Every counted long window has to be at or under this, in dB.
pub const LONG_WORST_DB: f64 = -14.0;
/// The median counted long window has to be at or under this, in dB.
pub const LONG_MEDIAN_DB: f64 = -16.0;
/// Every counted short window has to be at or under this, in dB: a muted or replaced word fails
/// here.
pub const SHORT_WORST_DB: f64 = -9.0;
/// A window's residual is held at or under this, in dB, so a stretch silent on one side reads a
/// fixed +60 dB.
pub const CAP_DB: f64 = 60.0;

/// A window's residual is held at or over this, -120 dB.
const RESIDUAL_FLOOR: f64 = 1e-12;
/// Keeps a division by an energy of zero finite.
const TINY: f64 = 1e-20;
/// Keeps the onset envelope's normalization finite for a flat envelope.
const SPREAD_GUARD: f64 = 1e-12;
/// Scales a frame's mean square before its logarithm.
const LEVEL_SCALE: f64 = 1e4;
/// The fewest envelope frames two envelopes must share for a coarse shift to be scored.
const MIN_OVERLAP_FRAMES: i64 = 10;
/// Frames [`compare`] hands each pass at a time.
const CHUNK_FRAMES: usize = 4096;

/// Why a check passed or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeardMasterReason {
    /// No shift lines the two up: they share too little, or nothing at all.
    NoShift,
    /// One file's loudest long window is under [`SILENT_DBFS`].
    Silent,
    /// A channel's gain lies outside `1 / MAX_GAIN` to `MAX_GAIN`.
    GainOutOfRange,
    /// Fewer than [`MIN_LONG_WINDOWS`] long windows count.
    TooShort,
    /// A long window is over [`LONG_WORST_DB`], or their median is over [`LONG_MEDIAN_DB`].
    LongWindow,
    /// A short window is over [`SHORT_WORST_DB`].
    ShortWindow,
    /// Every check passed: the file is the master that was heard.
    HeardMaster,
}

impl HeardMasterReason {
    /// The reason in a few words, for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            HeardMasterReason::NoShift => "no shift",
            HeardMasterReason::Silent => "a side is silent",
            HeardMasterReason::GainOutOfRange => "gain out of range",
            HeardMasterReason::TooShort => "too short",
            HeardMasterReason::LongWindow => "long window",
            HeardMasterReason::ShortWindow => "short window",
            HeardMasterReason::HeardMaster => "the heard master",
        }
    }
}

/// What the check found.
#[derive(Debug, Clone, PartialEq)]
pub struct HeardMasterVerdict {
    /// The file's sample `i + shift` lines up with the heard sample `i`. None when no shift was
    /// found.
    pub shift: Option<i64>,
    /// Each counted long window's residual in dB, in timeline order.
    pub long_windows: Vec<f64>,
    /// Each counted short window's residual in dB, in timeline order.
    pub short_windows: Vec<f64>,
    pub reason: HeardMasterReason,
}

impl HeardMasterVerdict {
    /// The verdict when no shift lines the two up.
    pub fn no_shift() -> Self {
        Self {
            shift: None,
            long_windows: Vec::new(),
            short_windows: Vec::new(),
            reason: HeardMasterReason::NoShift,
        }
    }

    /// Whether the file is the master that was heard.
    pub fn passed(&self) -> bool {
        self.reason == HeardMasterReason::HeardMaster
    }

    /// The loudest counted long window, in dB.
    pub fn long_worst(&self) -> Option<f64> {
        self.long_windows.iter().copied().reduce(f64::max)
    }

    /// The median counted long window, in dB.
    pub fn long_median(&self) -> Option<f64> {
        (!self.long_windows.is_empty()).then(|| median(&self.long_windows))
    }

    /// The loudest counted short window, in dB.
    pub fn short_worst(&self) -> Option<f64> {
        self.short_windows.iter().copied().reduce(f64::max)
    }
}

/// Turns a decoder's `-f f32le -ac 2` output, read in pieces of any size, into stereo frames.
#[derive(Debug, Clone, Default)]
pub struct F32leFrames {
    carry: Vec<u8>,
}

impl F32leFrames {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends every frame `bytes` completes to `out`; a partial frame waits for the next piece.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<[f32; 2]>) {
        self.carry.extend_from_slice(bytes);
        let (frames, _) = self.carry.as_chunks::<8>();
        out.extend(frames.iter().map(|frame| {
            [
                f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]),
                f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]),
            ]
        }));
        let used = frames.len() * 8;
        self.carry.drain(..used);
    }
}

/// The first pass over one decode: what the shift search needs of the whole file, about 5.5 KB
/// a second.
#[derive(Debug, Clone, Default)]
pub struct FirstPass {
    frames: usize,
    /// `ln(1 + 1e4 * mean square)` of the mono over each whole envelope frame of [`HOP`] samples.
    frame_levels: Vec<f64>,
    /// The mean square over both channels of each whole second.
    second_energy: Vec<f64>,
    /// The mono averaged over each whole run of [`DECIMATE`] samples.
    decimated: Vec<f32>,
    frame_sum: f64,
    frame_count: usize,
    second_sum: f64,
    second_count: usize,
    decimated_sum: f64,
    decimated_count: usize,
}

impl FirstPass {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the decode's next frames, in order, in pieces of any size.
    pub fn push(&mut self, frames: &[[f32; 2]]) {
        for &[left, right] in frames {
            let (left, right) = (f64::from(left), f64::from(right));
            let mono = (left + right) / 2.0;
            self.frames += 1;
            self.frame_sum += mono * mono;
            self.frame_count += 1;
            if self.frame_count == HOP {
                self.frame_levels
                    .push((LEVEL_SCALE * (self.frame_sum / HOP as f64)).ln_1p());
                self.frame_sum = 0.0;
                self.frame_count = 0;
            }
            self.second_sum += left * left;
            self.second_sum += right * right;
            self.second_count += 1;
            if self.second_count == RATE {
                self.second_energy.push(self.second_sum / (2 * RATE) as f64);
                self.second_sum = 0.0;
                self.second_count = 0;
            }
            self.decimated_sum += mono;
            self.decimated_count += 1;
            if self.decimated_count == DECIMATE {
                self.decimated.push((self.decimated_sum / DECIMATE as f64) as f32);
                self.decimated_sum = 0.0;
                self.decimated_count = 0;
            }
        }
    }

    /// How many frames the decode has given so far.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// The onset envelope: each frame's rise in level over the frame before (falls count as
    /// none), normalized to a mean of 0 and a spread of 1.
    fn onsets(&self) -> Vec<f64> {
        let rises: Vec<f64> = self
            .frame_levels
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).max(0.0))
            .collect();
        if rises.is_empty() {
            return rises;
        }
        let count = rises.len() as f64;
        let mean = rises.iter().sum::<f64>() / count;
        let spread = (rises
            .iter()
            .map(|rise| (rise - mean) * (rise - mean))
            .sum::<f64>()
            / count)
            .sqrt();
        rises
            .iter()
            .map(|rise| (rise - mean) / (spread + SPREAD_GUARD))
            .collect()
    }
}

/// The coarse shifts, in envelope frames, best first. Shift `k` scores how well the heard
/// envelope's frame `i + k` matches the file's frame `i` over every frame both have; a shift is
/// kept only when it is at least 2 frames from every better one kept.
fn coarse_candidates(heard: &[f64], file: &[f64]) -> Vec<i64> {
    let (heard_len, file_len) = (heard.len() as i64, file.len() as i64);
    let mut scored: Vec<(f64, i64)> = Vec::new();
    for k in -COARSE_FRAMES..=COARSE_FRAMES {
        let (from, to) = ((-k).max(0), file_len.min(heard_len - k));
        if to - from < MIN_OVERLAP_FRAMES {
            continue;
        }
        let score = dot(
            &heard[(from + k) as usize..(to + k) as usize],
            &file[from as usize..to as usize],
        );
        // A score of zero ranks the same whatever its sign.
        scored.push((if score == 0.0 { 0.0 } else { score }, k));
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut kept: Vec<i64> = Vec::new();
    for (_, k) in scored {
        if kept.iter().all(|other| (k - other).abs() >= 2) {
            kept.push(k);
        }
        if kept.len() == TOP_COARSE {
            break;
        }
    }
    kept
}

/// One candidate shift's refine: the heard stretch it is judged over and the full-rate lags it
/// tries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    /// The heard stretch's first sample.
    heard_start: usize,
    /// The stretch's length in samples.
    len: usize,
    /// The first full-rate lag tried; the last is `lag_low + 2 * FINE_SAMPLES`.
    lag_low: i64,
}

/// The candidate shifts both files' first passes give, and the stretches the second pass has to
/// keep to refine them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinePlan {
    candidates: Vec<Candidate>,
    file_frames: usize,
}

impl RefinePlan {
    /// Each coarse candidate's refine stretch: the heard audio's loudest [`REFINE_SECONDS`]
    /// whole seconds (the earliest on a tie) among those the file covers at that shift with
    /// [`HOP`] and [`FINE_SAMPLES`] to spare on both sides, or all of them when fewer are
    /// covered. Its lag is first found on the decimated mono, then left to the second pass at
    /// full rate. A candidate with no covered second, or no lag inside the file, is dropped.
    pub fn new(heard: &FirstPass, file: &FirstPass) -> Self {
        let file_frames = file.frames as i64;
        let reach = (HOP / DECIMATE) as i64 + 2;
        let spare = HOP as i64 + FINE_SAMPLES;
        let mut candidates = Vec::new();
        for k in coarse_candidates(&heard.onsets(), &file.onsets()) {
            let shift = -k * HOP as i64;
            let covered: Vec<usize> = (0..heard.second_energy.len())
                .filter(|&second| {
                    let start = (second * RATE) as i64 + shift;
                    start - spare >= 0 && start + RATE as i64 + spare <= file_frames
                })
                .collect();
            if covered.is_empty() {
                continue;
            }
            let span = REFINE_SECONDS.min(covered.len());
            let mut loudest: Option<(f64, usize)> = None;
            for run in covered.windows(span) {
                if run[span - 1] - run[0] != span - 1 {
                    continue;
                }
                let energy: f64 = run.iter().map(|&second| heard.second_energy[second]).sum();
                if loudest.is_none_or(|(best, _)| energy > best) {
                    loudest = Some((energy, run[0]));
                }
            }
            let Some((_, first)) = loudest else {
                continue;
            };
            let (start, end) = (first * RATE, (first + span) * RATE);
            let center = -k * (HOP / DECIMATE) as i64;
            let Some(lag) = best_lag_decimated(
                &heard.decimated[start / DECIMATE..end / DECIMATE],
                &file.decimated,
                (start / DECIMATE) as i64,
                center - reach..=center + reach,
            ) else {
                continue;
            };
            candidates.push(Candidate {
                heard_start: start,
                len: end - start,
                lag_low: lag * DECIMATE as i64 - FINE_SAMPLES,
            });
        }
        Self {
            candidates,
            file_frames: file.frames,
        }
    }

    /// Whether no candidate is left: the two never line up, and no second pass is needed.
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// What the second pass keeps of the heard audio, one stretch per candidate, by sample.
    pub fn heard_ranges(&self) -> Vec<Range<usize>> {
        self.candidates
            .iter()
            .map(|c| c.heard_start..c.heard_start + c.len)
            .collect()
    }

    /// What the second pass keeps of the file, one stretch per candidate, by sample: the heard
    /// stretch moved by every lag the candidate tries, within the file.
    pub fn file_ranges(&self) -> Vec<Range<usize>> {
        let frames = self.file_frames as i64;
        self.candidates
            .iter()
            .map(|c| {
                let low = c.heard_start as i64 + c.lag_low;
                let start = low.clamp(0, frames);
                let end = (low + c.len as i64 + 2 * FINE_SAMPLES).clamp(start, frames);
                start as usize..end as usize
            })
            .collect()
    }

    /// The one shift: each candidate's full-rate lag with the highest plain dot product over its
    /// stretch, then the candidate whose lag matches best by normalized dot product (the first
    /// on a tie). `heard` and `file` are the second pass's stretches over
    /// [`RefinePlan::heard_ranges`] and [`RefinePlan::file_ranges`]. The file's sample
    /// `i + shift` lines up with the heard sample `i`.
    pub fn pick_shift(&self, heard: &[Stretch], file: &[Stretch]) -> Option<i64> {
        let mut best = f64::NEG_INFINITY;
        let mut best_shift = None;
        for (index, c) in self.candidates.iter().enumerate() {
            let (Some(x), Some(y)) = (heard.get(index), file.get(index)) else {
                continue;
            };
            if x.mono.len() != c.len {
                continue;
            }
            // Where the file's window for `lag` starts inside its kept stretch.
            let offset = |lag: i64| c.heard_start as i64 + lag - y.start as i64;
            let mut top = f64::NEG_INFINITY;
            let mut fine = None;
            for lag in c.lag_low..=c.lag_low + 2 * FINE_SAMPLES {
                let at = offset(lag);
                if at < 0 || at as usize + c.len > y.mono.len() {
                    continue;
                }
                let value = dot(&x.mono, &y.mono[at as usize..at as usize + c.len]);
                if value > top {
                    top = value;
                    fine = Some(lag);
                }
            }
            let Some(fine) = fine else {
                continue;
            };
            let at = offset(fine) as usize;
            let window = &y.mono[at..at + c.len];
            let score = dot(&x.mono, window) / ((dot(&x.mono, &x.mono) * dot(window, window)).sqrt() + TINY);
            if score > best {
                best = score;
                best_shift = Some(fine);
            }
        }
        best_shift
    }
}

/// The lag in `lags` at which `x` best matches `y` from `start + lag` on, by the plain dot
/// product; a lag where `y` runs out is skipped, and the first wins a tie.
fn best_lag_decimated(x: &[f32], y: &[f32], start: i64, lags: RangeInclusive<i64>) -> Option<i64> {
    let mut best = f64::NEG_INFINITY;
    let mut best_lag = None;
    for lag in lags {
        let at = start + lag;
        if at < 0 || at as usize + x.len() > y.len() {
            continue;
        }
        let value = dot_f32(x, &y[at as usize..at as usize + x.len()]);
        if value > best {
            best = value;
            best_lag = Some(lag);
        }
    }
    best_lag
}

/// One stretch of a file's full-rate mono, from the second pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stretch {
    /// The stretch's first sample in the file.
    pub start: usize,
    /// `(left + right) / 2` of each frame from `start` on.
    pub mono: Vec<f64>,
}

/// The second pass over one decode: the full-rate mono over the ranges a [`RefinePlan`] asks
/// for, at most a few MB whatever the song's length.
#[derive(Debug, Clone)]
pub struct StretchPass {
    frames: usize,
    stretches: Vec<(Range<usize>, Stretch)>,
}

impl StretchPass {
    pub fn new(ranges: &[Range<usize>]) -> Self {
        Self {
            frames: 0,
            stretches: ranges
                .iter()
                .map(|range| {
                    let stretch = Stretch {
                        start: range.start,
                        mono: Vec::with_capacity(range.len()),
                    };
                    (range.clone(), stretch)
                })
                .collect(),
        }
    }

    /// Takes the decode's next frames, in order, in pieces of any size.
    pub fn push(&mut self, frames: &[[f32; 2]]) {
        let (from, to) = (self.frames, self.frames + frames.len());
        for (range, stretch) in &mut self.stretches {
            let (low, high) = (range.start.max(from), range.end.min(to));
            if low < high {
                stretch.mono.extend(
                    frames[low - from..high - from]
                        .iter()
                        .map(|&[left, right]| mono(left, right)),
                );
            }
        }
        self.frames = to;
    }

    /// Whether every range has been read, so the decode can stop early.
    pub fn is_done(&self) -> bool {
        self.stretches.iter().all(|(range, _)| self.frames >= range.end)
    }

    /// The stretches, in the order of the ranges asked for.
    pub fn finish(self) -> Vec<Stretch> {
        self.stretches.into_iter().map(|(_, stretch)| stretch).collect()
    }
}

/// One block's sums on the heard timeline, each pair per channel.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Block {
    /// The heard audio's frames inside the block.
    pub heard_frames: usize,
    /// The file's frames inside the block.
    pub file_frames: usize,
    /// The sum of squares of the heard frames inside the block.
    pub heard_energy: [f64; 2],
    /// The sum of squares of the file's frames inside the block.
    pub file_energy: [f64; 2],
    /// The heard sum of squares over the frames both files have in the block.
    pub shared_heard: [f64; 2],
    /// The file's sum of squares over those frames.
    pub shared_file: [f64; 2],
    /// The sum of heard sample times file sample over those frames.
    pub shared_cross: [f64; 2],
}

impl Block {
    /// Whether more than half of the block lies inside the heard audio.
    pub fn in_heard(&self) -> bool {
        self.heard_frames * 2 > BLOCK
    }

    /// Whether more than half of the block lies inside the file.
    pub fn in_file(&self) -> bool {
        self.file_frames * 2 > BLOCK
    }

    fn heard_mean_square(&self) -> f64 {
        mean_square(self.heard_energy, self.heard_frames)
    }

    fn file_mean_square(&self) -> f64 {
        mean_square(self.file_energy, self.file_frames)
    }
}

/// Which decode to read next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Heard,
    File,
}

/// The third pass: both decodes at the chosen shift, summed per block of [`BLOCK`] samples on
/// the heard timeline. Frames of whichever side is ahead wait for the other side to reach them,
/// so reading the side [`BlockSums::behind`] names keeps that wait to one piece.
#[derive(Debug, Clone)]
pub struct BlockSums {
    shift: i64,
    /// The index on the heard timeline of `blocks[0]`.
    first_block: i64,
    blocks: Vec<Block>,
    /// Where the next heard frame and the next file frame sit on the heard timeline.
    heard_next: i64,
    file_next: i64,
    heard_ended: bool,
    file_ended: bool,
    heard_waiting: VecDeque<[f32; 2]>,
    file_waiting: VecDeque<[f32; 2]>,
}

impl BlockSums {
    /// Sums for `shift`: the file's frame `i + shift` lines up with the heard frame `i`.
    pub fn new(shift: i64) -> Self {
        Self {
            shift,
            first_block: 0.min(-shift).div_euclid(BLOCK as i64),
            blocks: Vec::new(),
            heard_next: 0,
            file_next: -shift,
            heard_ended: false,
            file_ended: false,
            heard_waiting: VecDeque::new(),
            file_waiting: VecDeque::new(),
        }
    }

    /// The side to read next: the one further behind on the heard timeline, or the one still
    /// going. None once both have ended.
    pub fn behind(&self) -> Option<Side> {
        match (self.heard_ended, self.file_ended) {
            (true, true) => None,
            (true, false) => Some(Side::File),
            (false, true) => Some(Side::Heard),
            (false, false) if self.heard_next <= self.file_next => Some(Side::Heard),
            (false, false) => Some(Side::File),
        }
    }

    /// Takes the heard decode's next frames, in order, in pieces of any size.
    pub fn push_heard(&mut self, frames: &[[f32; 2]]) {
        for &frame in frames {
            let at = self.heard_next;
            self.heard_next += 1;
            if at < -self.shift {
                self.add_heard(at, frame);
            } else if let Some(other) = self.file_waiting.pop_front() {
                self.add_both(at, frame, other);
            } else if self.file_ended {
                self.add_heard(at, frame);
            } else {
                self.heard_waiting.push_back(frame);
            }
        }
    }

    /// Takes the file decode's next frames, in order, in pieces of any size.
    pub fn push_file(&mut self, frames: &[[f32; 2]]) {
        for &frame in frames {
            let at = self.file_next;
            self.file_next += 1;
            if at < 0 {
                self.add_file(at, frame);
            } else if let Some(other) = self.heard_waiting.pop_front() {
                self.add_both(at, other, frame);
            } else if self.heard_ended {
                self.add_file(at, frame);
            } else {
                self.file_waiting.push_back(frame);
            }
        }
    }

    /// The heard decode has ended: the file's frames waiting for it lie past its end.
    pub fn end_heard(&mut self) {
        self.heard_ended = true;
        let mut at = self.file_next - self.file_waiting.len() as i64;
        while let Some(frame) = self.file_waiting.pop_front() {
            self.add_file(at, frame);
            at += 1;
        }
    }

    /// The file decode has ended: the heard frames waiting for it lie past its end.
    pub fn end_file(&mut self) {
        self.file_ended = true;
        let mut at = self.heard_next - self.heard_waiting.len() as i64;
        while let Some(frame) = self.heard_waiting.pop_front() {
            self.add_heard(at, frame);
            at += 1;
        }
    }

    /// Every block from the first either file reaches to the last, in timeline order.
    pub fn finish(mut self) -> Vec<Block> {
        self.end_heard();
        self.end_file();
        self.blocks
    }

    fn block(&mut self, at: i64) -> &mut Block {
        let index = (at.div_euclid(BLOCK as i64) - self.first_block) as usize;
        if index >= self.blocks.len() {
            self.blocks.resize(index + 1, Block::default());
        }
        &mut self.blocks[index]
    }

    fn add_heard(&mut self, at: i64, [left, right]: [f32; 2]) {
        let (left, right) = (f64::from(left), f64::from(right));
        let block = self.block(at);
        block.heard_frames += 1;
        block.heard_energy[0] += left * left;
        block.heard_energy[1] += right * right;
    }

    fn add_file(&mut self, at: i64, [left, right]: [f32; 2]) {
        let (left, right) = (f64::from(left), f64::from(right));
        let block = self.block(at);
        block.file_frames += 1;
        block.file_energy[0] += left * left;
        block.file_energy[1] += right * right;
    }

    fn add_both(&mut self, at: i64, heard: [f32; 2], file: [f32; 2]) {
        let (heard_left, heard_right) = (f64::from(heard[0]), f64::from(heard[1]));
        let (file_left, file_right) = (f64::from(file[0]), f64::from(file[1]));
        let block = self.block(at);
        block.heard_frames += 1;
        block.heard_energy[0] += heard_left * heard_left;
        block.heard_energy[1] += heard_right * heard_right;
        block.file_frames += 1;
        block.file_energy[0] += file_left * file_left;
        block.file_energy[1] += file_right * file_right;
        block.shared_heard[0] += heard_left * heard_left;
        block.shared_heard[1] += heard_right * heard_right;
        block.shared_file[0] += file_left * file_left;
        block.shared_file[1] += file_right * file_right;
        block.shared_cross[0] += heard_left * file_left;
        block.shared_cross[1] += heard_right * file_right;
    }
}

/// The verdict over the third pass's blocks at `shift`.
///
/// One gain per channel over every block in both files fits the file to the heard audio. Each
/// long and short window is then read: skipped when no block in it is loud in either file, 0 dB
/// when a block in it is loud in one file and not inside the other (material missing, or
/// added), else the worse of what is left of the heard audio after taking the file away and of
/// the file after taking the heard audio away, over its blocks in both files.
pub fn verdict(shift: i64, blocks: &[Block]) -> HeardMasterVerdict {
    let mut cross = [0.0; 2];
    let mut file_power = [0.0; 2];
    for block in blocks.iter().filter(|b| b.in_heard() && b.in_file()) {
        cross[0] += block.shared_cross[0];
        cross[1] += block.shared_cross[1];
        file_power[0] += block.shared_file[0];
        file_power[1] += block.shared_file[1];
    }
    let gain = [
        cross[0] / (file_power[0] + TINY),
        cross[1] / (file_power[1] + TINY),
    ];
    let top_heard = loudest(blocks, |b| (b.heard_energy, b.heard_frames));
    let top_file = loudest(blocks, |b| (b.file_energy, b.file_frames));
    let floor = top_heard.max(top_file) * 10f64.powf(QUIET_DB / 10.0);
    let long_windows = windows(blocks, gain, LONG_BLOCKS, LONG_BLOCKS / 2, floor);
    let short_windows = windows(blocks, gain, SHORT_BLOCKS, 1, floor);
    let silent = 10f64.powf(SILENT_DBFS / 10.0);
    let worst = |values: &[f64]| values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let reason = if top_heard < silent || top_file < silent {
        HeardMasterReason::Silent
    } else if !gain.iter().all(|g| (1.0 / MAX_GAIN..=MAX_GAIN).contains(g)) {
        HeardMasterReason::GainOutOfRange
    } else if long_windows.len() < MIN_LONG_WINDOWS {
        HeardMasterReason::TooShort
    } else if worst(&long_windows) > LONG_WORST_DB || median(&long_windows) > LONG_MEDIAN_DB {
        HeardMasterReason::LongWindow
    } else if worst(&short_windows) > SHORT_WORST_DB {
        HeardMasterReason::ShortWindow
    } else {
        HeardMasterReason::HeardMaster
    };
    HeardMasterVerdict {
        shift: Some(shift),
        long_windows,
        short_windows,
        reason,
    }
}

/// Runs the three passes over two songs held in memory, `heard` the audio that played and `file`
/// the candidate, both as decoded stereo frames at [`RATE`] Hz.
pub fn compare(heard: &[[f32; 2]], file: &[[f32; 2]]) -> HeardMasterVerdict {
    let mut first_heard = FirstPass::new();
    first_heard.push(heard);
    let mut first_file = FirstPass::new();
    first_file.push(file);
    let plan = RefinePlan::new(&first_heard, &first_file);
    if plan.is_empty() {
        return HeardMasterVerdict::no_shift();
    }
    let mut heard_stretches = StretchPass::new(&plan.heard_ranges());
    heard_stretches.push(heard);
    let mut file_stretches = StretchPass::new(&plan.file_ranges());
    file_stretches.push(file);
    let Some(shift) = plan.pick_shift(&heard_stretches.finish(), &file_stretches.finish()) else {
        return HeardMasterVerdict::no_shift();
    };
    let mut sums = BlockSums::new(shift);
    let (mut heard_left, mut file_left) = (heard, file);
    while let Some(side) = sums.behind() {
        let left = match side {
            Side::Heard => &mut heard_left,
            Side::File => &mut file_left,
        };
        if left.is_empty() {
            match side {
                Side::Heard => sums.end_heard(),
                Side::File => sums.end_file(),
            }
            continue;
        }
        let (now, later) = left.split_at(left.len().min(CHUNK_FRAMES));
        *left = later;
        match side {
            Side::Heard => sums.push_heard(now),
            Side::File => sums.push_file(now),
        }
    }
    verdict(shift, &sums.finish())
}

/// The loudest long window's mean square over one file's frames, stepped half a window at a
/// time; `side` gives a block's sums of squares and frame count in that file.
fn loudest(blocks: &[Block], side: impl Fn(&Block) -> ([f64; 2], usize)) -> f64 {
    let mut best = 0.0;
    if blocks.len() < LONG_BLOCKS {
        return best;
    }
    for start in (0..=blocks.len() - LONG_BLOCKS).step_by(LONG_BLOCKS / 2) {
        let mut energy = 0.0;
        let mut count = 0;
        for block in &blocks[start..start + LONG_BLOCKS] {
            let (sums, frames) = side(block);
            energy += sums[0] + sums[1];
            count += 2 * frames.max(1);
        }
        let mean = energy / count as f64;
        if mean > best {
            best = mean;
        }
    }
    best
}

/// Each counted window's residual in dB, for windows of `size` blocks stepped `hop` blocks.
fn windows(blocks: &[Block], gain: [f64; 2], size: usize, hop: usize, floor: f64) -> Vec<f64> {
    let mut out = Vec::new();
    if blocks.len() < size {
        return out;
    }
    let cap = 10f64.powf(CAP_DB / 10.0);
    for start in (0..=blocks.len() - size).step_by(hop) {
        let window = &blocks[start..start + size];
        let mut loud = false;
        let mut one_sided = false;
        for block in window {
            let heard = block.in_heard() && block.heard_mean_square() >= floor;
            let file = block.in_file() && block.file_mean_square() >= floor;
            loud |= heard || file;
            one_sided |= (heard && !block.in_file()) || (file && !block.in_heard());
        }
        if !loud {
            continue;
        }
        if one_sided {
            out.push(0.0);
            continue;
        }
        let mut residual = 0.0;
        let mut heard_power = 0.0;
        let mut file_power = 0.0;
        for block in window.iter().filter(|b| b.in_heard() && b.in_file()) {
            residual += channel_residual(block, gain, 0) + channel_residual(block, gain, 1);
            heard_power += block.shared_heard[0] + block.shared_heard[1];
            file_power += gain[0] * gain[0] * block.shared_file[0] + gain[1] * gain[1] * block.shared_file[1];
        }
        let of_heard = residual.max(0.0) / (heard_power + TINY);
        let of_file = residual.max(0.0) / (file_power + TINY);
        out.push(10.0 * of_heard.max(of_file).max(RESIDUAL_FLOOR).min(cap).log10());
    }
    out
}

/// What is left in one channel of a block after taking the gained file away from the heard
/// audio: `sum (a - g b)^2` over the frames both have.
fn channel_residual(block: &Block, gain: [f64; 2], channel: usize) -> f64 {
    block.shared_heard[channel] - 2.0 * gain[channel] * block.shared_cross[channel]
        + gain[channel] * gain[channel] * block.shared_file[channel]
}

fn mean_square(sums: [f64; 2], frames: usize) -> f64 {
    (sums[0] + sums[1]) / (2 * frames.max(1)) as f64
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn mono(left: f32, right: f32) -> f64 {
    (f64::from(left) + f64::from(right)) / 2.0
}

/// The plain dot product. An index loop, which stays quick in unoptimized test builds too.
fn dot(x: &[f64], y: &[f64]) -> f64 {
    let len = x.len().min(y.len());
    let mut sum = 0.0;
    let mut i = 0;
    while i < len {
        sum += x[i] * y[i];
        i += 1;
    }
    sum
}

/// [`dot`] over the decimated mono, summed in f64.
fn dot_f32(x: &[f32], y: &[f32]) -> f64 {
    let len = x.len().min(y.len());
    let mut sum = 0.0;
    let mut i = 0;
    while i < len {
        sum += f64::from(x[i]) * f64::from(y[i]);
        i += 1;
    }
    sum
}

#[cfg(test)]
#[path = "heard_master_tests.rs"]
mod tests;
