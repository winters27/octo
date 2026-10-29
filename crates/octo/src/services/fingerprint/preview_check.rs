//! The check behind AcoustID for a sound it has never heard: whether a download sounds like the
//! song's YouTube preview. Both are fingerprinted with `fpcalc -raw` and lined up wherever they
//! agree best, so an intro on a music video, or a longer one on the album take, does not matter.
//!
//! Only a preview that can be believed refuses a file: one whose title and artist are the song
//! asked for, at a length within [`LENGTH_SLACK_SECONDS`] of it. Any other disagreement leaves
//! the file to a person, like any sound AcoustID could not place.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use octo_core::common::{SongIdentity, SongMatchOptions, SongRef};
use octo_core::fingerprint::{InconclusiveReason, VerificationResult, VerificationVerdict};
use octo_media::audio::{AudioFingerprinter, RawFingerprint};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use crate::services::you_tube::you_tube_resolver::{
    Failure, Lookup, ShimRefused, ShimStream, YouTubeHit, YouTubeResolver,
};

/// Seconds one chromaprint item covers: a frame every 1365 samples at 11025 Hz.
pub const ITEM_SECONDS: f64 = 4096.0 / 3.0 / 11025.0;
/// How much of the download is fingerprinted.
pub const FILE_SECONDS: i32 = 120;
/// How much of the preview is fingerprinted: the download's stretch and room for an intro.
pub const PREVIEW_SECONDS: i32 = 240;
/// How far into the preview the download's start may line up: a music video's intro.
pub const MAX_LEAD_SECONDS: f64 = 120.0;
/// How far into the download the preview's start may line up: the album take's longer intro.
pub const MAX_LAG_SECONDS: f64 = 60.0;
/// The stretch the two are judged by. The best one anywhere decides, so a cut verse or a skit
/// in the video does not count against the file.
pub const WINDOW_SECONDS: f64 = 30.0;
/// How far a preview's length may be from the song's for it to be believed.
pub const LENGTH_SLACK_SECONDS: i32 = 8;
/// At or below this share of differing bits, the download is what the preview plays. The right
/// recording against its YouTube Music row, a yt-dlp pick or its music video measured 0.001 to
/// 0.19; another song, or noise, never under 0.37.
pub const MATCH_AT_MOST: f64 = 0.25;
/// At or above it, no stretch of the download lines up with the preview anywhere. Above every
/// right file against a believed preview (at most 0.11) and against another recording of the
/// same song (a re-recording, a cover, up to 0.37), and below every wrong file against a
/// believed preview (from 0.39).
pub const MISMATCH_FROM: f64 = 0.38;
/// At least this share of a stretch's items have to differ from the item before them for it to
/// say anything. Silence, a pure tone or a steady hum fingerprints to one number over and over,
/// and two such stretches look alike whatever they are (silence against a 440 Hz tone, 94%
/// alike). Real music changes at nearly every item, loops and spoken skits too: 0.92 at the
/// lowest over 125 library songs (median 0.98), where a song built on one loop has only a few
/// hundred different numbers. A stretch with up to 9 s of silence in its 30 s still counts.
pub const MIN_CHANGING_SHARE: f64 = 0.7;
/// The most of a preview's audio that is read.
pub const MAX_PREVIEW_BYTES: usize = 40 * 1024 * 1024;
/// How long reading the preview may take.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(90);

/// Numbers each preview's temporary file, so two checks at once never share one.
static PREVIEW_RUN: AtomicU64 = AtomicU64::new(0);

/// How the two fingerprints lined up at their best.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Comparison {
    /// The share of bits that differ over the best [`WINDOW_SECONDS`] stretch, 0 to 1. Two
    /// unrelated sounds sit near a half.
    pub bit_error: f64,
    /// Where the download's start sits in the preview, in seconds; negative when the preview
    /// starts later in the song than the download does.
    pub offset_seconds: f64,
}

/// What the preview says of the download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// The download sounds like the preview.
    Match,
    /// Neither: kept, and asked about as before.
    Unclear,
    /// Nothing in the download sounds like a preview that can be believed: refused.
    Mismatch,
}

/// The preview a download is compared with: a YouTube video and what is known about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewFacts {
    pub video_id: String,
    /// The video's or the YouTube Music row's title.
    pub title: Option<String>,
    /// Who the row credits, or the channel that posted the video.
    pub artist: Option<String>,
    pub seconds: Option<i32>,
    /// The album of the YouTube Music row the preview is. None for any other video.
    pub album: Option<String>,
}

impl PreviewFacts {
    /// A video nothing is known about but its id.
    pub fn bare(video_id: &str) -> Self {
        Self {
            video_id: video_id.to_string(),
            ..Default::default()
        }
    }

    fn from_hit(hit: YouTubeHit, artist: Option<String>) -> Self {
        Self {
            video_id: hit.video_id,
            title: hit.title,
            artist,
            seconds: hit.duration,
            album: hit.album,
        }
    }
}

/// The song a download was asked for, as the preview check reads it.
#[derive(Debug, Clone, Copy)]
pub struct PreviewRequest<'a> {
    pub artist: &'a str,
    pub title: &'a str,
    /// The song's length, when the catalog or a lookup gave one.
    pub seconds: Option<i32>,
    /// The video the song plays as, when one is pinned on it.
    pub video_id: Option<&'a str>,
    /// Work nobody waits on: its YouTube Music lookup goes in the shim's background lane.
    pub background: bool,
}

/// What the check came to.
#[derive(Debug, Clone, PartialEq)]
pub enum PreviewOutcome {
    /// The verdict did not leave the question to the preview, or Octo is not checking downloads.
    NotAsked,
    /// Nothing to compare with: no video, or fpcalc could not read one of the two. The download is judged
    /// as it was before.
    Unavailable(String),
    /// The song's video was found but the shim did not send it. Nothing is said about the file;
    /// asking again later may get it.
    NotSent(String),
    /// The shim's background lane was full, so the preview was not fetched now. Asked again later.
    Busy,
    /// The shim could not be asked for the song's video at all. Asked again later, and never the
    /// song's fault.
    Unreachable,
    /// fpcalc could not read the file being checked.
    FileUnreadable,
    Compared {
        heard: Heard,
        comparison: Comparison,
        /// The preview is the song asked for by its title, artist and length.
        trusted: bool,
        preview: PreviewFacts,
    },
}

/// Why a preview's fingerprint could not be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewFailure {
    /// The shim did not send the video's audio.
    NotSent,
    /// The audio arrived but fpcalc could not read it.
    Unreadable,
    /// The shim's background lane was full, so the video was not fetched now.
    Busy,
}

/// Where previews come from: the yt-dlp shim.
#[async_trait]
pub trait PreviewSource: Send + Sync {
    /// The preview to compare with: the pinned video, with what a lookup says it is when one
    /// names it, or else the video a play would pick. `Ok(None)` only when YouTube Music and
    /// yt-dlp both answered and neither had the song; `Err` when either could not answer and the
    /// other did not find it.
    async fn find(&self, request: &PreviewRequest<'_>) -> Result<Option<PreviewFacts>, Failure>;

    /// Saves the video's audio at `dest`. False when the shim could not send it.
    async fn fetch(&self, video_id: &str, dest: &Path) -> bool;

    /// [`Self::fetch`] for work nobody waits on, in the shim's background lane where it has one.
    async fn fetch_in_background(&self, video_id: &str, dest: &Path) -> bool {
        self.fetch(video_id, dest).await
    }

    /// Saves the video's audio at `dest`, in the shim's background lane when `background` is
    /// true. [`PreviewFailure::Busy`] when the shim said its lane is full, else
    /// [`PreviewFailure::NotSent`].
    async fn fetch_preview(
        &self,
        video_id: &str,
        dest: &Path,
        background: bool,
    ) -> Result<(), PreviewFailure> {
        let fetched = if background {
            self.fetch_in_background(video_id, dest).await
        } else {
            self.fetch(video_id, dest).await
        };
        if fetched {
            Ok(())
        } else {
            Err(PreviewFailure::NotSent)
        }
    }
}

#[async_trait]
impl PreviewSource for YouTubeResolver {
    async fn find(&self, request: &PreviewRequest<'_>) -> Result<Option<PreviewFacts>, Failure> {
        let pinned = request.video_id.filter(|id| !id.trim().is_empty());
        let is_pinned = |hit: &YouTubeHit| pinned.is_none_or(|id| id == hit.video_id);
        // The order a play picks in: YouTube Music's row for the song, then yt-dlp's search.
        // A row is the song by its title and artists, so it credits the artist asked for.
        let music = self
            .music_song_lookup(request.artist, request.title, request.seconds, request.background)
            .await;
        let mut failure: Option<Failure> = None;
        match music {
            Lookup::Found(row) if is_pinned(&row) => {
                return Ok(Some(PreviewFacts::from_hit(
                    row,
                    Some(request.artist.to_string()),
                )));
            }
            Lookup::Failed(why) => failure = Some(why),
            _ => {}
        }
        let query = format!("{} {}", request.artist, request.title);
        match self.search_lookup(&query, request.seconds, true, &[]).await {
            Lookup::Found(hit) if !hit.video_id.is_empty() && is_pinned(&hit) => {
                let channel = hit.channel.clone();
                return Ok(Some(PreviewFacts::from_hit(hit, channel)));
            }
            Lookup::Failed(why) => {
                // A busy answer from either lookup is the one reported.
                failure = Some(if failure == Some(Failure::Busy) {
                    Failure::Busy
                } else {
                    why
                });
            }
            _ => {}
        }
        // A pinned video is the preview, known by its id alone, whatever the lookups said.
        if let Some(id) = pinned {
            return Ok(Some(PreviewFacts::bare(id)));
        }
        failure.map_or(Ok(None), Err)
    }

    async fn fetch(&self, video_id: &str, dest: &Path) -> bool {
        self.fetch_preview(video_id, dest, false).await.is_ok()
    }

    async fn fetch_in_background(&self, video_id: &str, dest: &Path) -> bool {
        self.fetch_preview(video_id, dest, true).await.is_ok()
    }

    async fn fetch_preview(
        &self,
        video_id: &str,
        dest: &Path,
        background: bool,
    ) -> Result<(), PreviewFailure> {
        let opened = if background {
            self.try_open_stream_in_background(video_id).await
        } else {
            self.try_open_stream(video_id).await
        };
        save_preview(opened, video_id, dest).await
    }
}

/// Writes an opened preview to `dest`, at most [`MAX_PREVIEW_BYTES`] within [`FETCH_TIMEOUT`].
/// [`PreviewFailure::Busy`] when the shim said its lane is full; any other failure to open,
/// receive or finish is [`PreviewFailure::NotSent`].
async fn save_preview(
    opened: Result<ShimStream, ShimRefused>,
    video_id: &str,
    dest: &Path,
) -> Result<(), PreviewFailure> {
    let opened = match opened {
        Ok(opened) => opened,
        Err(ShimRefused { busy: true }) => return Err(PreviewFailure::Busy),
        Err(_) => return Err(PreviewFailure::NotSent),
    };
    if opened.status_code != 200 {
        debug!(
            "shim /stream answered {} for preview {video_id}",
            opened.status_code
        );
        return Err(PreviewFailure::NotSent);
    }
    let read = async {
        let mut file = tokio::fs::File::create(dest).await?;
        let mut body = opened.response.bytes_stream();
        let mut written = 0usize;
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            let room = MAX_PREVIEW_BYTES.saturating_sub(written);
            file.write_all(&chunk[..chunk.len().min(room)]).await?;
            written += chunk.len().min(room);
            if written >= MAX_PREVIEW_BYTES {
                break;
            }
        }
        file.flush().await?;
        anyhow::Ok(written)
    };
    match tokio::time::timeout(FETCH_TIMEOUT, read).await {
        Ok(Ok(written)) if written > 0 => Ok(()),
        Ok(Ok(_)) => Err(PreviewFailure::NotSent),
        Ok(Err(error)) => {
            warn!("could not read preview {video_id} from the shim: {error}");
            Err(PreviewFailure::NotSent)
        }
        Err(_) => {
            warn!("preview {video_id} took longer than {FETCH_TIMEOUT:?} to read from the shim");
            Err(PreviewFailure::NotSent)
        }
    }
}

/// Whether a verdict leaves the question to the preview: AcoustID has no entry for the sound, or
/// ties it to a song through one person's submissions only, or
/// the length that would have caught a wrong file is unknown and AcoustID named nothing that
/// settles it. A verdict that names the song (a confirmation, a doubted length, a source that
/// disagreed) is never second-guessed.
pub fn wanted(verdict: &VerificationResult, length_known: bool) -> bool {
    verdict.verdict == VerificationVerdict::Inconclusive
        && match verdict.reason {
            InconclusiveReason::NoEntry | InconclusiveReason::LoneLink => true,
            InconclusiveReason::SourceDisagreed | InconclusiveReason::LengthOff => false,
            _ => !length_known,
        }
}

/// Whether the preview is the song asked for: its title and artist name it, in the same version
/// and with no subtitle of its own ("(AMAs Performance)"), and its length is within
/// [`LENGTH_SLACK_SECONDS`] of the song's. The artist is who a YouTube Music row credits or the
/// channel that posted the video, so an upload by anyone else ("Artist - Song (Lyrics)" on a
/// lyrics channel, a fan's live recording) is never believed, however its title reads.
pub fn trustworthy(request: &PreviewRequest<'_>, preview: &PreviewFacts) -> bool {
    let (Some(want), Some(got)) = (
        request.seconds.filter(|s| *s > 0),
        preview.seconds.filter(|s| *s > 0),
    ) else {
        return false;
    };
    if (want - got).abs() > LENGTH_SLACK_SECONDS {
        return false;
    }
    let Some(title) = preview.title.as_deref().filter(|t| !t.trim().is_empty()) else {
        return false;
    };
    let options = SongMatchOptions {
        length_tolerance_seconds: Some(LENGTH_SLACK_SECONDS),
        extras_must_agree: true,
        ..Default::default()
    };
    let Some(artist) = preview.artist.as_deref().filter(|a| !a.trim().is_empty()) else {
        return false;
    };
    SongIdentity::same(
        &SongRef::new(request.title, request.artist).with_seconds(f64::from(want)),
        &SongRef::new(title, artist).with_seconds(f64::from(got)),
        Some(&options),
    )
    .is_same()
}

/// For each position, whether the `window` items that start there have enough in them to be
/// compared (see [`MIN_CHANGING_SHARE`]).
fn informative_windows(items: &[u32], window: usize) -> Vec<bool> {
    if window == 0 || items.len() < window {
        return Vec::new();
    }
    let need = ((window - 1) as f64 * MIN_CHANGING_SHARE).ceil() as u32;
    // changed[j]: how many items up to j differ from the one before them.
    let mut changed = vec![0u32; items.len()];
    for j in 1..items.len() {
        changed[j] = changed[j - 1] + u32::from(items[j] != items[j - 1]);
    }
    (0..=items.len() - window)
        .map(|start| changed[start + window - 1] - changed[start] >= need)
        .collect()
}

/// The best [`WINDOW_SECONDS`] stretch at which the download's fingerprint lines up with the
/// preview's, over every offset from the preview [`MAX_LAG_SECONDS`] behind to
/// [`MAX_LEAD_SECONDS`] ahead. None when either is shorter than the stretch, or when the preview
/// has no stretch with sound in it to say what the download should be. Only stretches with sound
/// in both count, so a silent gap or a held tone in each never passes for the same song. A
/// download with no such stretch is as different as it gets from a preview with sound at every
/// place the two could line up, and says nothing against one as quiet as it somewhere.
pub fn compare(file: &[u32], preview: &[u32]) -> Option<Comparison> {
    let window = (WINDOW_SECONDS / ITEM_SECONDS).round() as usize;
    if file.len() < window || preview.len() < window {
        return None;
    }
    let lead = (MAX_LEAD_SECONDS / ITEM_SECONDS).round() as isize;
    let lag = (MAX_LAG_SECONDS / ITEM_SECONDS).round() as isize;
    let (file_len, preview_len) = (file.len() as isize, preview.len() as isize);
    let file_ok = informative_windows(file, window);
    let preview_ok = informative_windows(preview, window);
    if !preview_ok.contains(&true) {
        return None;
    }
    if !file_ok.contains(&true) {
        // A preview just as quiet where they line up (a long held note, a silent intro) may be
        // the same recording.
        let sound_everywhere = (-lag..=lead).all(|shift| {
            let start = (-shift).max(0);
            let end = file_len.min(preview_len - shift);
            end - start < window as isize
                || (start + shift..=end - window as isize + shift).all(|at| preview_ok[at as usize])
        });
        return sound_everywhere.then_some(Comparison {
            bit_error: 0.5,
            offset_seconds: 0.0,
        });
    }
    let mut best: Option<(u32, isize)> = None;
    let mut errors: Vec<u32> = Vec::with_capacity(file.len());
    for shift in -lag..=lead {
        // File item i lines up with preview item i + shift.
        let start = (-shift).max(0);
        let end = file_len.min(preview_len - shift);
        if end - start < window as isize {
            continue;
        }
        errors.clear();
        errors.extend((start..end).map(|i| (file[i as usize] ^ preview[(i + shift) as usize]).count_ones()));
        let mut run: u32 = errors[..window].iter().sum();
        let mut lowest = if file_ok[start as usize] && preview_ok[(start + shift) as usize] {
            run
        } else {
            u32::MAX
        };
        for i in window..errors.len() {
            run = run + errors[i] - errors[i - window];
            let from = start as usize + i + 1 - window;
            if file_ok[from] && preview_ok[(from as isize + shift) as usize] {
                lowest = lowest.min(run);
            }
        }
        if lowest != u32::MAX && best.is_none_or(|(errors, _)| lowest < errors) {
            best = Some((lowest, shift));
        }
    }
    best.map(|(errors, shift)| Comparison {
        bit_error: f64::from(errors) / (32 * window) as f64,
        offset_seconds: shift as f64 * ITEM_SECONDS,
    })
}

/// What a comparison says: a match whatever the preview is, a mismatch only against a preview
/// that can be believed, and anything between unclear.
pub fn judge(comparison: &Comparison, trusted: bool) -> Heard {
    if comparison.bit_error <= MATCH_AT_MOST {
        Heard::Match
    } else if trusted && comparison.bit_error >= MISMATCH_FROM {
        Heard::Mismatch
    } else {
        Heard::Unclear
    }
}

/// What a lookup that could not answer comes to: a busy shim is waited out, any other failure
/// says the shim could not be asked.
fn outcome_of(failure: Failure) -> PreviewOutcome {
    match failure {
        Failure::Busy => PreviewOutcome::Busy,
        Failure::Unreachable | Failure::Other => PreviewOutcome::Unreachable,
        Failure::Gone | Failure::Refused => PreviewOutcome::Unreachable,
    }
}

/// Compares the download at `path` with the song's preview, fetched in the shim's background lane
/// when the request is `background`. Every failure is [`PreviewOutcome::Unavailable`],
/// [`PreviewOutcome::NotSent`], [`PreviewOutcome::Busy`], [`PreviewOutcome::Unreachable`] or
/// [`PreviewOutcome::FileUnreadable`], which change nothing about the file.
pub async fn check(
    source: &dyn PreviewSource,
    fingerprinter: &AudioFingerprinter,
    path: &Path,
    request: &PreviewRequest<'_>,
    timeout_seconds: i32,
) -> PreviewOutcome {
    let background = request.background;
    let mut preview = match source.find(request).await {
        Ok(Some(preview)) => preview,
        Ok(None) => return PreviewOutcome::Unavailable("no YouTube video for the song".into()),
        Err(why) => return outcome_of(why),
    };
    let Some(file) = fingerprinter
        .raw_fingerprint(path, FILE_SECONDS, timeout_seconds)
        .await
    else {
        return PreviewOutcome::FileUnreadable;
    };
    let heard = match fingerprint_preview(
        source,
        fingerprinter,
        &preview.video_id,
        timeout_seconds,
        background,
    )
    .await
    {
        Ok(heard) => heard,
        Err(PreviewFailure::NotSent) => {
            return PreviewOutcome::NotSent(format!("preview {} was not sent", preview.video_id));
        }
        Err(PreviewFailure::Unreadable) => {
            return PreviewOutcome::Unavailable(format!("preview {} could not be read", preview.video_id));
        }
        Err(PreviewFailure::Busy) => return PreviewOutcome::Busy,
    };
    if preview.seconds.is_none_or(|s| s <= 0) {
        preview.seconds = Some(heard.seconds.round() as i32);
    }
    let Some(comparison) = compare(&file.items, &heard.items) else {
        return PreviewOutcome::Unavailable("too little audio to compare".into());
    };
    let trusted = trustworthy(request, &preview);
    PreviewOutcome::Compared {
        heard: judge(&comparison, trusted),
        comparison,
        trusted,
        preview,
    }
}

/// The preview's raw fingerprint, read through a temporary file that is removed afterwards.
/// `background` fetches it in the shim's background lane. [`PreviewFailure::NotSent`] and
/// [`PreviewFailure::Busy`] when the fetch failed, [`PreviewFailure::Unreadable`] when fpcalc could
/// not read what was fetched.
pub(crate) async fn fingerprint_preview(
    source: &dyn PreviewSource,
    fingerprinter: &AudioFingerprinter,
    video_id: &str,
    timeout_seconds: i32,
    background: bool,
) -> Result<RawFingerprint, PreviewFailure> {
    let dest = preview_path();
    let print = match source.fetch_preview(video_id, &dest, background).await {
        Ok(()) => fingerprinter
            .raw_fingerprint(&dest, PREVIEW_SECONDS, timeout_seconds)
            .await
            .ok_or(PreviewFailure::Unreadable),
        Err(failure) => Err(failure),
    };
    let _ = std::fs::remove_file(&dest);
    print
}

fn preview_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "octo-preview-{}-{}",
        std::process::id(),
        PREVIEW_RUN.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
#[path = "preview_check_tests.rs"]
mod tests;
