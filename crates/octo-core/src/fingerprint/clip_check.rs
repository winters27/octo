//! Whether the audio that played is the catalog's song, from the catalog track's own 30 s clip:
//! for when no AcoustID answer can say what played.
//!
//! Both are raw chromaprints (`fpcalc -raw`). The clip's print, less a second at each end, slides
//! over every place in the heard print, and the lowest mean share of differing bits over the
//! whole clip decides. Measured over 13 clips against 12 heard prints: the same recording 0.052
//! to 0.185, the same song in another mix 0.096 to 0.200, a re-recording of the song 0.272 to
//! 0.276, a live take against the studio one 0.402 and 0.418, every other song 0.425 and up. So a
//! low score is this song, and a high one is another song or another take of this one, which the
//! clip cannot tell apart.

use crate::common::SongIdentity;

/// Seconds one chromaprint item covers: a frame every 1365 samples at 11025 Hz.
pub const ITEM_SECONDS: f64 = 4096.0 / 3.0 / 11025.0;
/// Items dropped at each end of the clip's print, about a second, where an encoder ramps in and
/// out.
pub const EDGE_ITEMS: usize = 8;
/// The fewest items the clip has to keep once its ends are dropped, about 8 seconds.
pub const MIN_ITEMS: usize = 65;
/// At least this share of the kept items have to differ from the item before them. Silence, a
/// pure tone or a steady hum prints one number over and over, and two such stretches look alike
/// whatever they are.
pub const MIN_CHANGING_SHARE: f64 = 0.7;
/// At or under this share of differing bits, the heard audio is this song.
pub const MATCH_AT_MOST: f64 = 0.25;
/// At or over it, the heard audio is another song or another take of this one.
pub const MISMATCH_FROM: f64 = 0.38;

/// What the clip says of the heard audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipReading {
    /// The heard audio is this song: this recording, or another mix or master of it.
    ThisSong,
    /// Another song, or another take of this one such as a live one: the clip cannot tell the
    /// two apart.
    OtherSongOrTake,
    /// Between the two lines, or no usable clip to judge by.
    Unclear,
}

impl ClipReading {
    /// The reading of a score; no score is unclear.
    pub fn of(score: Option<&ClipScore>) -> Self {
        match score {
            Some(score) if score.bit_error <= MATCH_AT_MOST => ClipReading::ThisSong,
            Some(score) if score.bit_error >= MISMATCH_FROM => ClipReading::OtherSongOrTake,
            _ => ClipReading::Unclear,
        }
    }
}

/// Where the clip matches the heard print best.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipScore {
    /// The mean share of differing bits over the kept clip at its best place, 0 to 1. Two
    /// unrelated sounds sit near a half.
    pub bit_error: f64,
    /// The heard print's item that the kept clip's first item lines up with there.
    pub heard_item: usize,
}

impl ClipScore {
    /// Where the clip's own start sits in the heard audio, in seconds; negative when the clip
    /// starts before the heard print does.
    pub fn clip_start_seconds(&self) -> f64 {
        (self.heard_item as f64 - EDGE_ITEMS as f64) * ITEM_SECONDS
    }
}

/// The clip check's answer: the reading and, when the clip was usable, its score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipCheck {
    pub reading: ClipReading,
    pub score: Option<ClipScore>,
}

/// Reads the heard print by the clip's print.
pub fn check(clip: &[u32], heard: &[u32]) -> ClipCheck {
    let score = slide(clip, heard);
    ClipCheck {
        reading: ClipReading::of(score.as_ref()),
        score,
    }
}

/// The clip's best place in the heard print: its ends dropped, then slid over every place the
/// heard print holds it whole, the lowest mean share of differing bits winning (the earliest on
/// a tie). None when the kept clip is shorter than [`MIN_ITEMS`], changes at under
/// [`MIN_CHANGING_SHARE`] of its items, or is longer than the heard print.
pub fn slide(clip: &[u32], heard: &[u32]) -> Option<ClipScore> {
    let kept = clip
        .get(EDGE_ITEMS..clip.len().saturating_sub(EDGE_ITEMS))
        .unwrap_or(&[]);
    let len = kept.len();
    if len < MIN_ITEMS || heard.len() < len {
        return None;
    }
    let changed = kept.windows(2).filter(|pair| pair[0] != pair[1]).count();
    if (changed as f64) < MIN_CHANGING_SHARE * (len - 1) as f64 {
        return None;
    }
    let mut best: Option<(u64, usize)> = None;
    for start in 0..=heard.len() - len {
        let bits = differing_bits(kept, &heard[start..start + len]);
        if best.is_none_or(|(lowest, _)| bits < lowest) {
            best = Some((bits, start));
        }
    }
    best.map(|(bits, start)| ClipScore {
        bit_error: bits as f64 / (32 * len) as f64,
        heard_item: start,
    })
}

/// Whether a YouTube channel is the artist's own by its name: an auto-made "<artist> - Topic"
/// channel, or one named for the artist as [`SongIdentity::same_artist_name`] reads names. A
/// channel under any other name, a label's included, is not.
pub fn channel_names_artist(channel: &str, artist: &str) -> bool {
    let channel = channel.trim();
    let name = channel.strip_suffix(" - Topic").unwrap_or(channel);
    SongIdentity::same_artist_name(name, artist)
}

/// The bits that differ between two runs of items, item by item. An index loop, which stays
/// quick in unoptimized test builds too.
fn differing_bits(a: &[u32], b: &[u32]) -> u64 {
    let len = a.len().min(b.len());
    let mut bits = 0;
    let mut i = 0;
    while i < len {
        bits += u64::from((a[i] ^ b[i]).count_ones());
        i += 1;
    }
    bits
}

#[cfg(test)]
#[path = "clip_check_tests.rs"]
mod tests;
