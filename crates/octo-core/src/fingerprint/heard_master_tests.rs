//! Tests for the heard-master check: 17 synthetic songs against the readings in
//! `tests/fixtures/heard_master/expected.tsv`, the passes fed in any pieces and order, and the
//! frame reader.
//!
//! No recorded audio: every signal comes from the generator below, made the same way as the
//! songs the expected readings were measured on (the same constants and order of operations, in
//! f64, the song itself rounded to f32 like decoded PCM).

use std::f64::consts::PI;
use std::path::Path;
use std::sync::LazyLock;

use super::*;

// ---- the generator ----------------------------------------------------------------------

const RATE_F: f64 = RATE as f64;

/// Each voice's pitch and the rate its level beats at, in Hz.
const VOICES: [(f64, f64); 6] = [
    (110.0, 0.5),
    (165.0, 0.75),
    (220.0, 1.0),
    (330.0, 1.25),
    (440.0, 1.5),
    (660.0, 2.0),
];
const PANS: [f64; 6] = [-0.8, -0.4, 0.0, 0.3, 0.6, 0.9];
const LEVELS: [f64; 6] = [1.0; 6];
/// The remix: two voices panned elsewhere, two at other levels.
const REMIX_PANS: [f64; 6] = [-0.8, 0.4, 0.0, -0.3, 0.6, 0.9];
const REMIX_LEVELS: [f64; 6] = [1.0, 1.0, 1.41, 1.0, 0.71, 1.0];

/// `n` values in -1 to 1 from the linear congruential generator `x = 1664525 x + 1013904223`
/// mod 2^32, starting from `seed`.
fn noise(seed: u32, n: usize) -> Vec<f64> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            f64::from(x) / 4_294_967_296.0 * 2.0 - 1.0
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum Edit {
    Mute,
    Reverse,
}

/// The synthetic song: six beating sine voices panned across the field, and drum hits of
/// decaying noise at irregular times, rounded to f32. `edit` silences or reverses one voice from
/// `from` to `to` seconds.
fn synth(
    seconds: f64,
    pans: &[f64; 6],
    levels: &[f64; 6],
    edit: Option<(usize, f64, f64, Edit)>,
) -> Vec<[f64; 2]> {
    let n = (seconds * RATE_F) as usize;
    let mut out = vec![[0.0f64; 2]; n];
    for (index, ((&(frequency, beat), &pan), &level)) in VOICES.iter().zip(pans).zip(levels).enumerate() {
        let mut signal: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE_F;
                level * 0.15 * (0.5 + 0.5 * (2.0 * PI * beat * t).cos()) * (2.0 * PI * frequency * t).sin()
            })
            .collect();
        if let Some((target, from, to, kind)) = edit
            && target == index
        {
            let stretch = &mut signal[(from * RATE_F) as usize..(to * RATE_F) as usize];
            match kind {
                Edit::Mute => stretch.fill(0.0),
                Edit::Reverse => stretch.reverse(),
            }
        }
        let (left, right) = (((pan + 1.0) * PI / 4.0).cos(), ((pan + 1.0) * PI / 4.0).sin());
        for (frame, &value) in out.iter_mut().zip(&signal) {
            frame[0] += value * left;
            frame[1] += value * right;
        }
    }
    let mut hits = vec![0.2];
    for gap in noise(5, (seconds / 0.3) as usize + 2) {
        let next = hits[hits.len() - 1] + 0.3 + 0.4 * (gap + 1.0) / 2.0;
        if next >= seconds {
            break;
        }
        hits.push(next);
    }
    let drum_noise = noise(99, n);
    let mut last = 0;
    for (i, frame) in out.iter_mut().enumerate() {
        let t = i as f64 / RATE_F;
        if t < hits[0] {
            continue;
        }
        while last + 1 < hits.len() && hits[last + 1] <= t {
            last += 1;
        }
        let drum = drum_noise[i] * 0.3 * (-(t - hits[last]) / 0.05).exp();
        frame[0] += drum;
        frame[1] += drum;
    }
    out.iter()
        .map(|&[left, right]| [f64::from(left as f32), f64::from(right as f32)])
        .collect()
}

fn mean_square_of(frames: &[[f64; 2]]) -> f64 {
    frames
        .iter()
        .map(|&[left, right]| left * left + right * right)
        .sum::<f64>()
        / (2 * frames.len()) as f64
}

/// Stereo noise `level_db` under `like`'s level (a uniform noise's mean square is a third).
fn stereo_noise(seed: u32, n: usize, level_db: f64, like: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let rms = mean_square_of(like).sqrt();
    let scale = 10f64.powf(level_db / 20.0);
    let values = noise(seed, 2 * n);
    let (pairs, _) = values.as_chunks::<2>();
    pairs
        .iter()
        .map(|&[left, right]| {
            [
                left * rms * scale * 3f64.sqrt(),
                right * rms * scale * 3f64.sqrt(),
            ]
        })
        .collect()
}

/// A 1 kHz and 1.5 kHz part from `from` to `to` seconds, `level_db` under `like` there, the same
/// in both channels.
fn part(from: f64, to: f64, level_db: f64, like: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let tone: Vec<f64> = (0..like.len())
        .map(|i| {
            let t = i as f64 / RATE_F;
            0.1 * ((2.0 * PI * 1000.0 * t).sin() + (2.0 * PI * 1500.0 * t).sin())
        })
        .collect();
    let (low, high) = ((from * RATE_F) as usize, (to * RATE_F) as usize);
    let window = mean_square_of(&like[low..high]).sqrt();
    let tone_rms = (tone[low..high].iter().map(|x| x * x).sum::<f64>() / (high - low) as f64).sqrt();
    let scale = window * 10f64.powf(level_db / 20.0) / tone_rms;
    tone.iter()
        .enumerate()
        .map(|(i, &value)| {
            let t = i as f64 / RATE_F;
            let gate = if t >= from && t < to { 1.0 } else { 0.0 };
            let sample = value * gate * scale;
            [sample, sample]
        })
        .collect()
}

fn add(a: &[[f64; 2]], b: &[[f64; 2]]) -> Vec<[f64; 2]> {
    a.iter().zip(b).map(|(x, y)| [x[0] + y[0], x[1] + y[1]]).collect()
}

fn decoded(frames: &[[f64; 2]]) -> Vec<[f32; 2]> {
    frames
        .iter()
        .map(|&[left, right]| [left as f32, right as f32])
        .collect()
}

/// The songs every case is built from, made once for all the tests.
struct Songs {
    heard: Vec<[f64; 2]>,
    /// Stereo noise 30 dB under the heard song, standing in for a lossy copy's coding noise.
    quiet: Vec<[f64; 2]>,
    remix: Vec<[f64; 2]>,
    /// 12 s of the remix's arrangement, as other music.
    other: Vec<[f64; 2]>,
}

static SONGS: LazyLock<Songs> = LazyLock::new(|| {
    let heard = synth(40.0, &PANS, &LEVELS, None);
    let quiet = stereo_noise(7, heard.len(), -30.0, &heard);
    Songs {
        remix: synth(40.0, &REMIX_PANS, &REMIX_LEVELS, None),
        other: synth(12.0, &REMIX_PANS, &REMIX_LEVELS, None),
        quiet,
        heard,
    }
});

/// Every case, named as in the expected readings, in their order.
const CASES: [&str; 17] = [
    "same master with codec-like noise at -25 dB",
    "half the level",
    "starts 27183 samples later",
    "left and right swapped",
    "0.6 s muted at 12 s",
    "an extra part 12 dB under, 20-30 s",
    "an extra part 30 dB under, 20-30 s",
    "ends at 30 s",
    "a remix: two voices re-panned, two re-leveled",
    "one voice of six muted 0.15 s across 12 s",
    "one voice of six muted 0.2 s across 12 s",
    "one voice of six muted 0.3 s across 12 s",
    "one voice of six reversed 0.4 s at 12 s",
    "8 s of other music, then the heard audio",
    "the heard audio, then 12 s of other music",
    "silent heard audio",
    "the heard audio folded to mono",
];

/// The named case's heard audio and file, as decoded frames.
fn case(label: &str) -> (Vec<[f32; 2]>, Vec<[f32; 2]>) {
    let songs = &*SONGS;
    let heard = &songs.heard;
    let quiet = &songs.quiet;
    let voice_muted = |length: f64| {
        let edit = (3, 12.0 - length / 2.0, 12.0 + length / 2.0, Edit::Mute);
        add(&synth(40.0, &PANS, &LEVELS, Some(edit)), quiet)
    };
    let file: Vec<[f64; 2]> = match label {
        "same master with codec-like noise at -25 dB" => {
            add(heard, &stereo_noise(7, heard.len(), -25.0, heard))
        }
        "half the level" => heard
            .iter()
            .zip(quiet)
            .map(|(x, q)| [0.5 * x[0] + q[0], 0.5 * x[1] + q[1]])
            .collect(),
        "starts 27183 samples later" => [vec![[0.0; 2]; 27183], add(heard, quiet)].concat(),
        "left and right swapped" => heard.iter().map(|&[left, right]| [right, left]).collect(),
        "0.6 s muted at 12 s" => {
            let mut muted = add(heard, quiet);
            muted[(12.0 * RATE_F) as usize..(12.6 * RATE_F) as usize].fill([0.0; 2]);
            muted
        }
        "an extra part 12 dB under, 20-30 s" => add(&add(heard, quiet), &part(20.0, 30.0, -12.0, heard)),
        "an extra part 30 dB under, 20-30 s" => add(&add(heard, quiet), &part(20.0, 30.0, -30.0, heard)),
        "ends at 30 s" => add(heard, quiet)[..30 * RATE].to_vec(),
        "a remix: two voices re-panned, two re-leveled" => add(&songs.remix, quiet),
        "one voice of six muted 0.15 s across 12 s" => voice_muted(0.15),
        "one voice of six muted 0.2 s across 12 s" => voice_muted(0.2),
        "one voice of six muted 0.3 s across 12 s" => voice_muted(0.3),
        "one voice of six reversed 0.4 s at 12 s" => add(
            &synth(40.0, &PANS, &LEVELS, Some((3, 11.8, 12.2, Edit::Reverse))),
            quiet,
        ),
        "8 s of other music, then the heard audio" => {
            [songs.other[..8 * RATE].to_vec(), add(heard, quiet)].concat()
        }
        "the heard audio, then 12 s of other music" => [add(heard, quiet), songs.other.clone()].concat(),
        "silent heard audio" => return (vec![[0.0; 2]; heard.len()], decoded(&add(heard, quiet))),
        "the heard audio folded to mono" => heard
            .iter()
            .zip(quiet)
            .map(|(x, q)| {
                let fold = (x[0] + x[1]) / 2.0;
                [fold + q[0], fold + q[1]]
            })
            .collect(),
        other => panic!("no synthetic case is named {other:?}"),
    };
    (decoded(heard), decoded(&file))
}

// ---- the expected readings ----------------------------------------------------------------

/// One row of the expected readings, the dB values rounded to 0.1.
struct Expected {
    label: String,
    shift: Option<i64>,
    long_windows: usize,
    long_worst: Option<f64>,
    long_median: Option<f64>,
    short_worst: Option<f64>,
    passed: bool,
    reason: String,
}

fn expected_rows() -> Vec<Expected> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heard_master/expected.tsv");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let number = |field: &str| (field != "-").then(|| field.parse::<f64>().expect("a dB value"));
    text.lines()
        .skip(1)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 8, "{line:?}");
            Expected {
                label: fields[0].to_string(),
                shift: (fields[1] != "None").then(|| fields[1].parse().expect("a shift")),
                long_windows: fields[2].parse().expect("a window count"),
                long_worst: number(fields[3]),
                long_median: number(fields[4]),
                short_worst: number(fields[5]),
                passed: fields[6] == "yes",
                reason: fields[7].to_string(),
            }
        })
        .collect()
}

fn assert_db(label: &str, what: &str, got: Option<f64>, want: Option<f64>) {
    match (got, want) {
        (Some(got), Some(want)) => {
            assert!(
                (got - want).abs() <= 0.05 + 1e-9,
                "{label}: {what} is {got:.3} dB, expected {want:.1}"
            );
        }
        (None, None) => {}
        _ => panic!("{label}: {what} is {got:?}, expected {want:?}"),
    }
}

/// The named case reads as expected: the same shift, window count and verdict, and every dB
/// value the same to 0.1 dB.
fn assert_case(label: &str) {
    let want = expected_rows()
        .into_iter()
        .find(|row| row.label == label)
        .expect("the case has an expected row");
    let (heard, file) = case(label);
    let got = compare(&heard, &file);
    assert_eq!(got.shift, want.shift, "{label}: shift");
    assert_eq!(got.long_windows.len(), want.long_windows, "{label}: long windows");
    assert_db(label, "the worst long window", got.long_worst(), want.long_worst);
    assert_db(
        label,
        "the median long window",
        got.long_median(),
        want.long_median,
    );
    assert_db(
        label,
        "the worst short window",
        got.short_worst(),
        want.short_worst,
    );
    assert_eq!(got.passed(), want.passed, "{label}: passed");
    assert_eq!(got.reason.as_str(), want.reason, "{label}: reason");
}

#[test]
fn every_expected_reading_has_its_case() {
    let labels: Vec<String> = expected_rows().into_iter().map(|row| row.label).collect();
    assert_eq!(labels, CASES);
}

#[test]
fn a_copy_with_codec_like_noise_is_the_heard_master() {
    assert_case(CASES[0]);
}

#[test]
fn half_the_level_is_the_heard_master() {
    assert_case(CASES[1]);
}

#[test]
fn a_late_start_is_found_to_the_sample_and_passes() {
    assert_case(CASES[2]);
}

#[test]
fn swapped_channels_fail() {
    assert_case(CASES[3]);
}

#[test]
fn a_muted_stretch_fails() {
    assert_case(CASES[4]);
}

#[test]
fn an_added_part_12_db_under_fails() {
    assert_case(CASES[5]);
}

#[test]
fn an_added_part_30_db_under_passes() {
    assert_case(CASES[6]);
}

#[test]
fn a_file_that_ends_early_fails() {
    assert_case(CASES[7]);
}

#[test]
fn a_remix_fails() {
    assert_case(CASES[8]);
}

#[test]
fn one_voice_muted_for_0_15_s_fails() {
    assert_case(CASES[9]);
}

#[test]
fn one_voice_muted_for_0_2_s_fails() {
    assert_case(CASES[10]);
}

#[test]
fn one_voice_muted_for_0_3_s_fails() {
    assert_case(CASES[11]);
}

#[test]
fn one_voice_reversed_fails() {
    assert_case(CASES[12]);
}

#[test]
fn other_music_before_the_heard_audio_fails() {
    assert_case(CASES[13]);
}

#[test]
fn other_music_after_the_heard_audio_fails() {
    assert_case(CASES[14]);
}

#[test]
fn a_silent_heard_side_fails() {
    assert_case(CASES[15]);
}

#[test]
fn a_mono_fold_fails() {
    assert_case(CASES[16]);
}

// ---- streaming ----------------------------------------------------------------------------

/// `frames` in pieces whose sizes cycle through `sizes`.
fn pieces<'a>(frames: &'a [[f32; 2]], sizes: &'a [usize]) -> impl Iterator<Item = &'a [[f32; 2]]> + 'a {
    let mut rest = frames;
    let mut turn = 0;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (now, later) = rest.split_at(sizes[turn % sizes.len()].min(rest.len()));
        rest = later;
        turn += 1;
        Some(now)
    })
}

/// The three passes as a streaming caller runs them: each decode in pieces, the second pass
/// stopping once its stretches are read, the third reading whichever side is behind.
fn streamed(heard: &[[f32; 2]], file: &[[f32; 2]], sizes: &[usize]) -> HeardMasterVerdict {
    let mut first_heard = FirstPass::new();
    pieces(heard, sizes).for_each(|piece| first_heard.push(piece));
    let mut first_file = FirstPass::new();
    pieces(file, sizes).for_each(|piece| first_file.push(piece));
    assert_eq!(
        (first_heard.frames(), first_file.frames()),
        (heard.len(), file.len())
    );
    let plan = RefinePlan::new(&first_heard, &first_file);
    let mut heard_stretches = StretchPass::new(&plan.heard_ranges());
    for piece in pieces(heard, sizes) {
        if heard_stretches.is_done() {
            break;
        }
        heard_stretches.push(piece);
    }
    let mut file_stretches = StretchPass::new(&plan.file_ranges());
    for piece in pieces(file, sizes) {
        if file_stretches.is_done() {
            break;
        }
        file_stretches.push(piece);
    }
    let Some(shift) = plan.pick_shift(&heard_stretches.finish(), &file_stretches.finish()) else {
        return HeardMasterVerdict::no_shift();
    };
    let mut sums = BlockSums::new(shift);
    let (mut heard_pieces, mut file_pieces) = (pieces(heard, sizes), pieces(file, sizes));
    while let Some(side) = sums.behind() {
        match side {
            Side::Heard => match heard_pieces.next() {
                Some(piece) => sums.push_heard(piece),
                None => sums.end_heard(),
            },
            Side::File => match file_pieces.next() {
                Some(piece) => sums.push_file(piece),
                None => sums.end_file(),
            },
        }
    }
    verdict(shift, &sums.finish())
}

/// The third pass with one whole side pushed before the other.
fn blocks_one_side_first(heard: &[[f32; 2]], file: &[[f32; 2]], shift: i64, heard_first: bool) -> Vec<Block> {
    let mut sums = BlockSums::new(shift);
    if heard_first {
        sums.push_heard(heard);
        sums.push_file(file);
    } else {
        sums.push_file(file);
        sums.push_heard(heard);
    }
    sums.finish()
}

#[test]
fn the_passes_read_the_same_in_any_pieces_and_order() {
    let (heard, late) = case("starts 27183 samples later");
    // Either way round: the file starting late, and the heard audio starting late.
    for (heard, file, shift) in [(&heard, &late, 27_183), (&late, &heard, -27_183)] {
        let whole = compare(heard, file);
        assert_eq!(whole.shift, Some(shift));
        assert_eq!(streamed(heard, file, &[1, 7, 333, 4096, 65_536]), whole);
        assert_eq!(streamed(heard, file, &[1_000_003]), whole);
        let heard_first = blocks_one_side_first(heard, file, shift, true);
        assert_eq!(blocks_one_side_first(heard, file, shift, false), heard_first);
        assert_eq!(verdict(shift, &heard_first), whole);
    }
}

#[test]
fn a_heard_audio_that_starts_late_is_found_to_the_sample() {
    let (heard, late) = case("starts 27183 samples later");
    let got = compare(&late, &heard);
    // The heard audio's blocks start 27183 samples before the music does, so the first block
    // the music reaches holds heard audio the file has too little of to count: a 0 dB window.
    assert_eq!(got.shift, Some(-27_183));
    assert_eq!(got.long_windows.len(), 81);
    assert_eq!(got.reason, HeardMasterReason::LongWindow);
}

#[test]
fn too_little_heard_audio_has_no_shift() {
    let (heard, file) = case("half the level");
    let got = compare(&heard[..RATE / 2], &file);
    assert_eq!(got, HeardMasterVerdict::no_shift());
    assert!(!got.passed());
    assert_eq!(compare(&[], &file), HeardMasterVerdict::no_shift());
}

#[test]
fn frames_come_whole_from_bytes_split_anywhere() {
    let frames = [[0.5f32, -0.25], [1.0, 0.0], [-1.0, 0.125]];
    let bytes: Vec<u8> = frames
        .iter()
        .flat_map(|[left, right]| left.to_le_bytes().into_iter().chain(right.to_le_bytes()))
        .collect();
    for split in 0..=bytes.len() {
        let mut reader = F32leFrames::new();
        let mut out = Vec::new();
        reader.push(&bytes[..split], &mut out);
        reader.push(&bytes[split..], &mut out);
        assert_eq!(out, frames, "split at byte {split}");
    }
    let mut reader = F32leFrames::new();
    let mut out = Vec::new();
    for byte in &bytes {
        reader.push(std::slice::from_ref(byte), &mut out);
    }
    assert_eq!(out, frames);
}
