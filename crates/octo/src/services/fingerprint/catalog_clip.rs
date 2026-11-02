//! The catalog clip check as Octo runs it: with no AcoustID answer for what played, the catalog
//! track's own 30 s Deezer clip is compared with the heard print, locally
//! (`octo_core::fingerprint::clip_check`).
//!
//! The Deezer track is the song's album at its disc and track, else its ISRC's track, else the
//! track search's, each used only when it is this song. Its clip is fetched at once (the URL is
//! signed and short-lived) over https from Deezer's CDN alone, redirects followed by hand, into a
//! scratch file, printed in one `fpcalc` place and deleted. A heard print that stops short of the
//! song is first made whole from the heard audio, and a "this song" from a video must come from a
//! channel that names the artist. A held heart keeps both prints, so a resume never fetches twice.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::SongIdentity;
use octo_core::fingerprint::clip_check::{self, EDGE_ITEMS, MIN_CHANGING_SHARE, MIN_ITEMS};
use octo_core::fingerprint::heart_gate::{ClipReading, HeardRecord, HeardSource, WaitingFor};
use octo_core::fingerprint::verification::{core_title, length_fits};
use octo_core::soulseek::soulseek_metadata_service::SoulseekRouting;
use parking_lot::Mutex;
use reqwest::Url;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info};

use crate::services::common::soulseek_hold_store::{HeldAcquisition, SoulseekHoldStore};
use crate::services::metadata::deezer_metadata_service::{DeezerAsked, DeezerMetadataService, DeezerTrack};
use crate::services::prefetch::heard_samples::{
    FpcalcClass, HeardSamples, ScratchFile, WholePrint, sample_id_of,
};
use crate::services::you_tube::YouTubeResolver;
use crate::services::you_tube::you_tube_resolver::{Failure, Lookup};

/// The most bytes a clip may take.
pub const MAX_CLIP_BYTES: u64 = 2 * 1024 * 1024;
/// The most redirects a clip's fetch follows.
pub const MAX_REDIRECTS: usize = 3;
/// How long one hop of a clip's fetch may take.
const CLIP_TIMEOUT: Duration = Duration::from_secs(20);
/// The seconds of the clip `fpcalc` prints.
const CLIP_SECONDS: i32 = 30;
/// How far short of the song a print may stop and still cover it.
const COVER_SLACK_SECONDS: f64 = 3.0;

/// The prints a clip check took, kept by a held heart so a resume never takes them twice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipPrints {
    pub clip: Option<Vec<u32>>,
    pub whole_heard: Option<Vec<u32>>,
}

impl ClipPrints {
    /// The prints a held record keeps for `user`: the record's clip print and the holder's whole
    /// heard print.
    pub fn of_hold(record: &HeldAcquisition, user: &str) -> Self {
        ClipPrints {
            clip: record.clip_print.clone(),
            whole_heard: record
                .holders
                .iter()
                .find(|holder| holder.user.trim().eq_ignore_ascii_case(user.trim()))
                .and_then(|holder| holder.whole_heard_print.clone()),
        }
    }

    /// Saves what a check took into the hold `key`: the clip print on the record, the whole heard
    /// print on `user`'s holder.
    pub fn save_to(&self, holds: &SoulseekHoldStore, key: &str, user: &str) {
        holds.update(key, |record| {
            if self.clip.is_some() {
                record.clip_print = self.clip.clone();
            }
            if self.whole_heard.is_some()
                && let Some(holder) = record
                    .holders
                    .iter_mut()
                    .find(|holder| holder.user.trim().eq_ignore_ascii_case(user.trim()))
            {
                holder.whole_heard_print = self.whole_heard.clone();
            }
        });
    }
}

/// Whether a clip's URL may be fetched: https, from a host under Deezer's CDN, `dzcdn.net`.
pub fn clip_url_allowed(url: &Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host.to_ascii_lowercase().ends_with(".dzcdn.net"))
}

/// Which hosts a clip may come from.
pub type ClipHosts = Arc<dyn Fn(&Url) -> bool + Send + Sync>;

/// What fetching a clip came to.
enum Fetched {
    Clip(ScratchFile),
    /// The CDN or the network did not answer.
    Busy,
    /// No usable clip: a refused URL, a refusal, an empty or oversized answer.
    Unusable,
}

/// The catalog clip runner.
pub struct CatalogClip {
    deezer: Arc<DeezerMetadataService>,
    samples: Arc<HeardSamples>,
    youtube: Arc<YouTubeResolver>,
    http: reqwest::Client,
    hosts: Mutex<ClipHosts>,
}

impl CatalogClip {
    pub fn new(
        deezer: Arc<DeezerMetadataService>,
        samples: Arc<HeardSamples>,
        youtube: Arc<YouTubeResolver>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(CLIP_TIMEOUT)
            .build()
            .unwrap_or_default();
        CatalogClip {
            deezer,
            samples,
            youtube,
            http,
            hosts: Mutex::new(Arc::new(clip_url_allowed)),
        }
    }

    /// Takes clips from the hosts `hosts` allows from now on, in place of Deezer's CDN.
    pub fn use_clip_hosts(&self, hosts: ClipHosts) {
        *self.hosts.lock() = hosts;
    }

    /// What the catalog's clip says of what `heard` played of the song `routing` is. `prints`
    /// holds what an earlier check took and gets what this one takes.
    pub async fn read(
        &self,
        routing: &SoulseekRouting,
        heard: &HeardRecord,
        prints: &mut ClipPrints,
    ) -> ClipReading {
        let song_seconds = routing.duration.filter(|s| *s > 0);
        let mut clip_seconds = None;
        if prints.clip.is_none() {
            let track = match self.track(routing).await {
                Ok(Some(track)) => track,
                Ok(None) => return ClipReading::NoClip,
                Err(busy) => return busy,
            };
            clip_seconds = track.duration_seconds;
            let Some(url) = track.clip_url.as_deref().filter(|url| !url.trim().is_empty()) else {
                return ClipReading::NoClip;
            };
            let file = match self.fetch(url).await {
                Fetched::Clip(file) => file,
                Fetched::Busy => return ClipReading::Busy(WaitingFor::Deezer),
                Fetched::Unusable => return ClipReading::NoClip,
            };
            let printed = self
                .samples
                .raw_print(file.path(), CLIP_SECONDS, FpcalcClass::Heart)
                .await;
            // The clip's audio goes as soon as it is printed.
            drop(file);
            // fpcalc failing here says nothing of the clip: the heart holds, and the check runs
            // again on Octo's next start, the next play or heart of the song, or Deezer's next
            // answer.
            let Some(items) = printed else {
                return ClipReading::Busy(WaitingFor::Deezer);
            };
            prints.clip = Some(items);
        }
        let clip = prints.clip.clone().unwrap_or_default();
        if !usable(&clip) {
            return ClipReading::NoClip;
        }
        let song = song_seconds.or(clip_seconds);
        let (heard_items, whole) = match self.heard_print(heard, song, prints).await {
            Ok(found) => found,
            Err(reading) => return reading,
        };
        let check = clip_check::check(&clip, &heard_items);
        match check.reading {
            clip_check::ClipReading::ThisSong => self.channel_guard(routing, heard).await,
            // A print that stops short of the song never says another song: the clip may lie
            // past it.
            clip_check::ClipReading::OtherSongOrTake if whole => ClipReading::AnotherSongOrTake,
            clip_check::ClipReading::OtherSongOrTake | clip_check::ClipReading::Unclear => {
                ClipReading::Unclear
            }
        }
    }

    /// The Deezer track of this song: its album at its disc and track, then its ISRC, then the
    /// track search, the first that is this song. Ok(None) when none is; Err when Deezer did not
    /// answer.
    async fn track(&self, routing: &SoulseekRouting) -> Result<Option<DeezerTrack>, ClipReading> {
        let busy = Err(ClipReading::Busy(WaitingFor::Deezer));
        if let Some((album, disc, position)) = album_place(routing) {
            match self.deezer.album_rows(&album, false).await {
                DeezerAsked::Busy => return busy,
                DeezerAsked::NoData => {}
                DeezerAsked::Answer((_, tracks)) => {
                    let at = tracks
                        .iter()
                        .find(|t| t.disc.unwrap_or(1) == disc && t.position == Some(position));
                    if let Some(listed) = at {
                        match self.deezer.clip_track(&listed.id).await {
                            DeezerAsked::Busy => return busy,
                            DeezerAsked::Answer(track) if is_this_song(&track, routing) => {
                                return Ok(Some(track));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if let Some(isrc) = routing.isrc.as_deref().filter(|isrc| !isrc.trim().is_empty()) {
            match self.deezer.clip_track_by_isrc(isrc).await {
                DeezerAsked::Busy => return busy,
                DeezerAsked::Answer(track) if is_this_song(&track, routing) => return Ok(Some(track)),
                DeezerAsked::Answer(track) => debug!(
                    "Deezer's track for {isrc} is '{} - {}', not this song; the title search is asked",
                    track.artist, track.title
                ),
                DeezerAsked::NoData => {}
            }
        }
        let (artist, title) = (
            routing.artist.clone().unwrap_or_default(),
            routing.title.clone().unwrap_or_default(),
        );
        match self.deezer.clip_track_search(&artist, &title).await {
            DeezerAsked::Busy => busy,
            DeezerAsked::Answer(track) if is_this_song(&track, routing) => Ok(Some(track)),
            _ => Ok(None),
        }
    }

    /// Fetches a clip into a scratch file: https from the allowed hosts only, every redirect
    /// checked by hand (at most [`MAX_REDIRECTS`]), at most [`MAX_CLIP_BYTES`].
    async fn fetch(&self, url: &str) -> Fetched {
        let hosts = self.hosts.lock().clone();
        let Ok(mut url) = Url::parse(url) else {
            return Fetched::Unusable;
        };
        for _ in 0..=MAX_REDIRECTS {
            if !hosts(&url) {
                info!(
                    "A clip from {} is not taken: only https from Deezer's CDN is",
                    url.host_str().unwrap_or("")
                );
                return Fetched::Unusable;
            }
            let response = match self.http.get(url.clone()).send().await {
                Ok(response) => response,
                Err(_) => return Fetched::Busy,
            };
            let status = response.status();
            if status.is_redirection() {
                let next = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|location| url.join(location).ok());
                match next {
                    Some(next) => {
                        url = next;
                        continue;
                    }
                    None => return Fetched::Unusable,
                }
            }
            if status.as_u16() == 429 || status.is_server_error() {
                return Fetched::Busy;
            }
            if !status.is_success() {
                return Fetched::Unusable;
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_CLIP_BYTES)
            {
                return Fetched::Unusable;
            }
            let Some(file) = self.samples.scratch(FpcalcClass::Heart, "mp3") else {
                return Fetched::Busy;
            };
            return match save(response, &file).await {
                Saved::Done => Fetched::Clip(file),
                Saved::TooBig | Saved::Empty => Fetched::Unusable,
                Saved::Failed => Fetched::Busy,
            };
        }
        Fetched::Unusable
    }

    /// The heard items to slide the clip over, and whether they cover the whole song: the stored
    /// sample when it does, else a print of the whole song from the heard audio itself.
    async fn heard_print(
        &self,
        heard: &HeardRecord,
        song_seconds: Option<i32>,
        prints: &mut ClipPrints,
    ) -> Result<(Vec<u32>, bool), ClipReading> {
        if let Some(whole) = &prints.whole_heard {
            return Ok((whole.clone(), true));
        }
        let stored = heard
            .sample_id
            .as_deref()
            .and_then(|id| self.samples.prints(id))
            .or_else(|| self.samples.prints(&sample_id_of(&heard.source)));
        if let Some(stored) = &stored {
            let covers = stored.reaches_end
                || song_seconds
                    .is_some_and(|song| stored.covered_seconds() + COVER_SLACK_SECONDS >= f64::from(song));
            if covers {
                return Ok((stored.raw.clone(), true));
            }
        }
        match self.samples.whole_song_print(heard).await {
            WholePrint::Ready(items, _) => {
                prints.whole_heard = Some(items.clone());
                Ok((items, true))
            }
            WholePrint::Busy => Err(ClipReading::Busy(WaitingFor::Shim)),
            // fpcalc could not print the heard audio: the check runs again later, as for a clip.
            WholePrint::NotPrinted => Err(ClipReading::Busy(WaitingFor::Deezer)),
            // The heard audio is gone: what is stored is all there is, and it cannot say
            // another song.
            WholePrint::Gone => match stored {
                Some(stored) => Ok((stored.raw, false)),
                None => Err(ClipReading::Unclear),
            },
        }
    }

    /// "This song" holds only when the heard video's channel, without " - Topic", names the
    /// song's artist: a cover or karaoke upload can sound like the song. A library or acquired
    /// file has no channel to read and needs none. A copy fetched ahead is read by the channel of
    /// the YouTube video it was made from; one from any other source (a peer's upload can be a
    /// cover too) is unclear. A shim that does not answer, or one too old to answer fresh, holds
    /// for the shim; a video gone or refused is unclear.
    async fn channel_guard(&self, routing: &SoulseekRouting, heard: &HeardRecord) -> ClipReading {
        let id = match &heard.source {
            HeardSource::Video { id, .. } => id.clone(),
            HeardSource::Library { .. } | HeardSource::Acquired { .. } => return ClipReading::ThisSong,
            copy @ HeardSource::Copy { .. } => match self.samples.copy_video(copy) {
                Some(id) => id,
                None => return ClipReading::Unclear,
            },
        };
        let artist = routing.artist.clone().unwrap_or_default();
        match self.youtube.facts_fresh(&id).await {
            Lookup::Found(facts) => match facts.channel {
                Some(channel) if clip_check::channel_names_artist(&channel, &artist) => ClipReading::ThisSong,
                Some(channel) => {
                    info!("The heard video's channel '{channel}' does not name {artist}: unclear");
                    ClipReading::Unclear
                }
                None => ClipReading::Unclear,
            },
            Lookup::Failed(Failure::Busy | Failure::Unreachable | Failure::Other) => {
                ClipReading::Busy(WaitingFor::Shim)
            }
            Lookup::Failed(Failure::Gone | Failure::Refused) | Lookup::None => ClipReading::Unclear,
        }
    }
}

/// The album, disc and position the song was listed or played from: the most recently played
/// context, else the most recently listed, else the routing's own album.
fn album_place(routing: &SoulseekRouting) -> Option<(String, u32, u32)> {
    let context = routing
        .contexts
        .iter()
        .filter(|c| c.played_at.is_some())
        .max_by_key(|c| c.played_at)
        .or_else(|| routing.contexts.iter().max_by_key(|c| c.listed_at));
    let (album, disc, track) = match context {
        Some(c) => (Some(c.catalog_album_id.clone()), c.disc, c.track),
        None => (
            routing.external_album_id.clone(),
            routing.disc_number,
            routing.track,
        ),
    };
    let album = album.filter(|a| !a.trim().is_empty())?;
    let position = u32::try_from(track?).ok().filter(|p| *p > 0)?;
    let disc = disc
        .and_then(|d| u32::try_from(d).ok())
        .filter(|d| *d > 0)
        .unwrap_or(1);
    Some((album, disc, position))
}

/// Whether a Deezer track is this song: the same core title, the same artist by name, and
/// lengths that fit when both are known.
pub fn is_this_song(track: &DeezerTrack, routing: &SoulseekRouting) -> bool {
    let title = routing.title.as_deref().unwrap_or("");
    let artist = routing.artist.as_deref().unwrap_or("");
    let core = core_title(title);
    if core.is_empty() || core_title(&track.title) != core {
        return false;
    }
    if !SongIdentity::same_artist_name(&track.artist, artist) {
        return false;
    }
    match (
        track.duration_seconds.filter(|s| *s > 0),
        routing.duration.filter(|s| *s > 0),
    ) {
        (Some(a), Some(b)) => length_fits(f64::from(a), f64::from(b)),
        _ => true,
    }
}

/// Whether a clip's print can be judged: long enough, and changing enough, once its edges go.
fn usable(clip: &[u32]) -> bool {
    let kept = clip
        .get(EDGE_ITEMS..clip.len().saturating_sub(EDGE_ITEMS))
        .unwrap_or(&[]);
    if kept.len() < MIN_ITEMS {
        return false;
    }
    let changed = kept.windows(2).filter(|pair| pair[0] != pair[1]).count();
    changed as f64 >= MIN_CHANGING_SHARE * (kept.len() - 1) as f64
}

enum Saved {
    Done,
    TooBig,
    Empty,
    Failed,
}

/// Writes a clip's body into its scratch file, giving up past [`MAX_CLIP_BYTES`].
async fn save(response: reqwest::Response, file: &ScratchFile) -> Saved {
    use futures::StreamExt;
    let Ok(mut out) = tokio::fs::File::create(file.path()).await else {
        return Saved::Failed;
    };
    let mut body = response.bytes_stream();
    let mut written = 0u64;
    while let Some(chunk) = body.next().await {
        let Ok(chunk) = chunk else {
            return Saved::Failed;
        };
        written += chunk.len() as u64;
        if written > MAX_CLIP_BYTES {
            return Saved::TooBig;
        }
        if !file.reserve(chunk.len() as u64) || out.write_all(&chunk).await.is_err() {
            return Saved::Failed;
        }
    }
    if out.flush().await.is_err() {
        return Saved::Failed;
    }
    if written == 0 { Saved::Empty } else { Saved::Done }
}

#[cfg(test)]
#[path = "catalog_clip_tests.rs"]
mod tests;
