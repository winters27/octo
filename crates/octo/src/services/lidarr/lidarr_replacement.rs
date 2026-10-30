//! A replacement's song through Lidarr: the pick and its follow a heart's search has.
//!
//! - **The pick.** The album is picked as a heart's is (`prepare`): Lidarr only for an artist with
//!   no music in the user's Lidarr, only an album that is unmonitored with no queue item, the
//!   entry saved before anything is sent, monitoring never turned on, the search pinned to a
//!   release that carries the original's recording when that is known, a release picked in Find
//!   songs grabbed instead of the search.
//! - **The follow.** Everything after the search is the heart's own follow. A file of Octo's
//!   import that a fetch waits for (its song, lossless when it asked for lossless) is moved out to
//!   the fetch at once; every other file of Octo's is held out of the library and deleted when the
//!   follow ends, and the album is put back once Lidarr is done with Octo's downloads.
//! - **The wait.** A fetch waits up to Lidarr's import timeout; the follow goes on after it, so a
//!   late import is still Octo's, taken out of the library and put back with the pick.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use octo_core::common::dotnet;
use octo_core::fingerprint::heart_gate::AcquisitionOrigin;
use octo_core::fingerprint::heart_release::{HeardIdentity, HeartSong};
use octo_core::lidarr::lidarr_track_fetcher::{is_lossless, match_track};
use octo_core::lidarr::{LidarrAlbumCandidate, LidarrError, LidarrImportedTrack, LidarrTrackRequest};
use octo_core::models::domain::{Album, Song};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{Ask, Inner, LidarrHeartAcquisitionService, PinHint};
use crate::services::fingerprint::music_brainz_client::{HeartQuestions, Lane};
use crate::services::lidarr::lidarr_pick::delete_moved_record;
use crate::services::lidarr::{LidarrFetched, LidarrReplacements};

/// What a fetch answers when Lidarr was not used for the album.
const NOT_USED: &str = "Lidarr did not bring this song.";

/// What a fetch answers when a heart's follow runs on the album.
const HEART_BUSY: &str = "Lidarr is fetching this album for a heart right now; try again once it lands.";

/// A replacement's fetch waiting for its song of an album.
pub(super) struct FetchWaiter {
    id: u64,
    request: LidarrTrackRequest,
    destination: String,
    sender: Option<oneshot::Sender<LidarrFetched>>,
}

static NEXT_WAITER: AtomicU64 = AtomicU64::new(1);

/// Takes a fetch's waiter off its album when the fetch stops waiting, however it stops.
struct Waiting<'a> {
    inner: &'a Inner,
    album: String,
    id: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut fetches = self.inner.fetches.lock();
        if let Some(waiters) = fetches.get_mut(&self.album) {
            waiters.retain(|waiter| waiter.id != self.id);
            if waiters.is_empty() {
                fetches.remove(&self.album);
            }
        }
    }
}

/// Whether a track of Octo's import is what a fetch asked for: its song by title (the nearest
/// length of several), and lossless when it asked for lossless.
fn fetched_by(request: &LidarrTrackRequest, track: &LidarrImportedTrack) -> bool {
    match_track(std::iter::once(track), request).is_some() && (!request.lossless_only || is_lossless(track))
}

/// A pick's failure as Lidarr's own error, its words kept.
fn lidarr_error(error: anyhow::Error) -> LidarrError {
    match error.downcast::<LidarrError>() {
        Ok(error) => error,
        Err(error) => LidarrError::InvalidOperation(error.to_string()),
    }
}

fn canceled() -> LidarrError {
    LidarrError::Canceled("A task was canceled.".into())
}

impl LidarrHeartAcquisitionService {
    /// How many replacements' fetches wait for a song of the album, for tests that start two.
    #[cfg(test)]
    pub(crate) fn fetches_waiting(&self, foreign_album_id: &str) -> usize {
        self.inner
            .fetches
            .lock()
            .get(&dotnet::ordinal_ignore_case_key(foreign_album_id))
            .map_or(0, Vec::len)
    }
}

#[async_trait]
impl LidarrReplacements for LidarrHeartAcquisitionService {
    async fn fetch_from(
        &self,
        album: LidarrAlbumCandidate,
        request: &LidarrTrackRequest,
        destination: &str,
        ct: &CancellationToken,
    ) -> Result<LidarrFetched, LidarrError> {
        self.inner
            .clone()
            .fetch_from(album, request, destination, ct)
            .await
    }

    async fn first_pick(&self, request: &LidarrTrackRequest) -> Option<String> {
        self.inner.first_pick(request).await
    }
}

impl Inner {
    /// One song of `candidate` for a replacement: picked and followed as a heart's search is, and
    /// answered once its own file of Octo's import is moved into `destination`.
    async fn fetch_from(
        self: Arc<Self>,
        candidate: LidarrAlbumCandidate,
        request: &LidarrTrackRequest,
        destination: &str,
        ct: &CancellationToken,
    ) -> Result<LidarrFetched, LidarrError> {
        let key = candidate.foreign_album_id.clone();
        let album_key = dotnet::ordinal_ignore_case_key(&key);
        let running = self.jobs.lock().get(&album_key).cloned();
        if running.as_ref().is_some_and(|job| !job.replacement_only()) {
            return Err(LidarrError::InvalidOperation(HEART_BUSY.into()));
        }
        let joined = running.is_some() || self.pending.get(&key).is_some_and(|entry| entry.searched());
        if let (Some(pick), Some(step)) = (&request.release, &request.step) {
            if joined {
                // One album, one search: the song comes from the one already running.
                info!(
                    "Lidarr is already fetching '{} - {}', so the release picked for '{}' ({}) is not grabbed",
                    candidate.artist,
                    candidate.title,
                    request.title,
                    pick.title.as_deref().unwrap_or_default()
                );
                step.tell(
                    "Lidarr is already fetching this album, so the release you picked was not grabbed",
                    Some("The song comes from the album's search already running"),
                );
            } else {
                step.tell("Grabbing the release you picked", pick.title.as_deref());
            }
        }

        // Waiting from before anything is searched, so no file of the song goes by unseen.
        let (sender, mut receiver) = oneshot::channel();
        let id = NEXT_WAITER.fetch_add(1, Ordering::Relaxed);
        self.fetches
            .lock()
            .entry(album_key.clone())
            .or_default()
            .push(FetchWaiter {
                id,
                request: request.clone(),
                destination: destination.to_string(),
                sender: Some(sender),
            });
        let _waiting = Waiting {
            inner: &self,
            album: album_key,
            id,
        };
        // The album's search may have brought the song already.
        self.deliver_held(&key).await;

        let (provider, external_id) = match request.song.as_deref().and_then(|song| song.split_once(':')) {
            Some((provider, id)) => (Some(provider.to_string()), Some(id.to_string())),
            None => (None, None),
        };
        let album = Album {
            title: candidate.title.clone(),
            artist: candidate.artist.clone(),
            year: candidate.year,
            songs: vec![Song {
                title: request.title.clone(),
                artist: request.artist.clone(),
                album: request.album.clone().unwrap_or_else(|| candidate.title.clone()),
                duration: request.duration_seconds,
                external_provider: provider,
                external_id,
                ..Default::default()
            }],
            ..Default::default()
        };
        let ask = Ask {
            hint: (!request.recordings.is_empty()).then(|| PinHint {
                listened_release: None,
                recordings: request.recordings.clone(),
                lane: Some(Lane::Background),
            }),
            release: request.release.clone(),
            album_heart: None,
        };
        let queued = tokio::select! {
            biased;
            _ = ct.cancelled() => return Err(canceled()),
            queued = self.queue_resolved_album(
                &album,
                None,
                Some(candidate),
                false,
                false,
                ask,
                AcquisitionOrigin::Replacement,
                &[],
            ) => queued,
        };
        let job = queued.map_err(lidarr_error)?;
        let skipped = *job.skipped.lock();
        if let Some(line) = skipped {
            return Err(LidarrError::InvalidOperation(
                line.unwrap_or(NOT_USED).to_string(),
            ));
        }

        let seconds = self.settings.current().lidarr.import_timeout_seconds.max(1);
        let deadline = Instant::now() + Duration::from_secs(seconds as u64);
        let mut changes = job.progress.subscribe();
        loop {
            if let Ok(fetched) = receiver.try_recv() {
                return Ok(fetched);
            }
            if job.is_ended() {
                return Err(LidarrError::FileNotFound(format!(
                    "Lidarr finished the album with no {}copy of '{} - {}'.",
                    if request.lossless_only { "lossless " } else { "" },
                    request.artist,
                    request.title
                )));
            }
            tokio::select! {
                biased;
                _ = ct.cancelled() => return Err(canceled()),
                fetched = &mut receiver => {
                    return fetched.map_err(|_| LidarrError::FileNotFound(NOT_USED.into()));
                }
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Err(LidarrError::FileNotFound(NOT_USED.into()));
                    }
                }
                () = tokio::time::sleep_until(deadline.into()) => {
                    return Err(LidarrError::FileNotFound(format!(
                        "Lidarr found no {}copy of '{} - {}' within {} minutes.",
                        if request.lossless_only { "lossless " } else { "" },
                        request.artist,
                        request.title,
                        (seconds / 60).max(1)
                    )));
                }
            }
        }
    }

    /// A file of Octo's import that a replacement's fetch waits for: it is moved into the
    /// fetch's folder (Lidarr's record of it deleted, for a file Lidarr still lists) and handed
    /// over with where its labels are read from. False when no fetch waits for it.
    pub(super) async fn deliver_fetched(
        &self,
        key: &str,
        track: &LidarrImportedTrack,
        path: &str,
        dropped: Option<String>,
    ) -> bool {
        let taken = {
            let mut fetches = self.fetches.lock();
            let Some(waiters) = fetches.get_mut(&dotnet::ordinal_ignore_case_key(key)) else {
                return false;
            };
            waiters
                .iter_mut()
                .filter(|waiter| waiter.sender.is_some())
                .find(|waiter| fetched_by(&waiter.request, track))
                .and_then(|waiter| Some((waiter.sender.take()?, waiter.destination.clone())))
        };
        let Some((sender, destination)) = taken else {
            return false;
        };
        let extension = Path::new(path)
            .extension()
            .map(|extension| format!(".{}", extension.to_string_lossy()))
            .unwrap_or_default();
        let copy =
            Path::new(&destination).join(format!("lidarr-{}{extension}", uuid::Uuid::new_v4().simple()));
        let moved = std::fs::create_dir_all(&destination).is_ok()
            && (std::fs::rename(path, &copy).is_ok()
                || (std::fs::copy(path, &copy).is_ok() && std::fs::remove_file(path).is_ok()));
        if !moved {
            warn!("Could not move Lidarr's {path} out for its replacement");
            return false;
        }
        if track.track_file_id > 0 {
            delete_moved_record(
                &self.client,
                Some(self.pending.as_ref()),
                key,
                track.track_file_id,
                path,
            )
            .await;
        }
        let labels = self.labels(key, dropped).await;
        let copy = copy.to_string_lossy().into_owned();
        info!(
            "Lidarr brought '{}' ({}) for a replacement; moved it to {copy}",
            track.title,
            track.quality.as_deref().unwrap_or("its own quality")
        );
        if let Err(fetched) = sender.send(LidarrFetched { path: copy, labels }) {
            // The fetch stopped waiting in the meantime: nothing takes the copy.
            let _ = std::fs::remove_file(&fetched.path);
        }
        true
    }

    /// Hands a fetch the files of the album Octo holds already, its song among them.
    async fn deliver_held(&self, key: &str) {
        let Some(entry) = self.pending.get(key) else {
            return;
        };
        for held in entry.held_tracks {
            if self
                .deliver_fetched(key, &held.imported(), &held.path, held.dropped.clone())
                .await
            {
                let file_id = held.track_file_id;
                self.pending.update(key, |entry| {
                    entry.held_tracks.retain(|track| track.track_file_id != file_id);
                });
            }
        }
    }

    /// The walk's first pick for the request's recordings, on the background lane: None with no
    /// recording, no MusicBrainz, or no answer.
    async fn first_pick(&self, request: &LidarrTrackRequest) -> Option<String> {
        if request.recordings.is_empty() {
            return None;
        }
        let music_brainz = self.music_brainz.clone()?;
        let questions = HeartQuestions::new(music_brainz, self.catalog.clone(), Lane::Background);
        let song = HeartSong {
            artist: request.artist.clone(),
            title: request.title.clone(),
            album: request.album.clone().unwrap_or_default(),
            seconds: request
                .duration_seconds
                .and_then(|seconds| u32::try_from(seconds).ok())
                .unwrap_or(0),
            ..Default::default()
        };
        let identity = HeardIdentity {
            recordings: request.recordings.clone(),
            recordings_uncapped: request.recordings.clone(),
            ..Default::default()
        };
        match questions.walk(&song, &identity).await {
            Ok(picks) => picks.into_iter().next(),
            Err(busy) => {
                warn!(
                    "{busy:?} did not answer for the walk of '{} - {}'; no pick",
                    request.artist, request.title
                );
                None
            }
        }
    }
}
