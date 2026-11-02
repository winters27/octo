//! Album hearts through Lidarr.
//!
//! - **The album.** The album a heart names is found by its catalog album's barcode: the release
//!   the barcode names, and that release's group, which Lidarr is asked for by its id. With no
//!   catalog id, the catalog album is first found from the album's title; only with neither is
//!   Lidarr asked by the album's name. The search is pinned to the barcode release.
//! - **Each track by its own sound.** Lidarr may import another edition, whose track 5 is another
//!   recording than the barcode release's track 5. So each file of Octo's import is filed among
//!   the album's tracks no earlier file was filed as, by its own sound against each one's own
//!   recording, never by where Lidarr put it: a proven track lands as that track, one AcoustID
//!   cannot tell apart (or does not know, when its own tags fit a track) waits in the one hold
//!   path as a held album track of the heart, and one that is none of them is deleted.

use std::path::Path;
use std::sync::Arc;

use octo_core::common::dotnet;
use octo_core::fingerprint::VerificationResult;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, WaitingFor};
use octo_core::fingerprint::heart_release::{BarcodeRelease, HeartSong};
use octo_core::lidarr::LidarrImportedTrack;
use octo_core::models::domain::{Album, Song};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{AlbumJob, Inner, lane_for, song_entry_key};
use crate::services::common::album_gates::{AlbumGate, AlbumGateTrack, TrackFiling, tracks_wait_line};
use crate::services::common::heart_acquisition_coordinator::waiting_line;
use crate::services::common::soulseek_hold_store::HeldLidarrFile;
use crate::services::common::{AcquisitionHeld, HeldAcquisition, HeldKind};
use crate::services::fingerprint::download_verification_service::LabelSource;
use crate::services::fingerprint::music_brainz_client::{HeartBusy, HeartQuestions};
use crate::services::lidarr::lidarr_pending_albums::SongGate;
use crate::services::lidarr::lidarr_pick::delete_moved_record;

/// Where an album heart's album is found.
pub(super) enum AlbumFind {
    /// The release group its catalog album's barcode names, and that barcode release.
    Group { group: String, release: String },
    /// No catalog album: by the album's name.
    ByName,
    /// MusicBrainz or Deezer did not answer.
    Busy(HeartBusy),
}

/// Each track's gate of an album gate as a job's per-song gate: the track's song (the album
/// listing's provider and id), its catalog title and artist.
pub(super) fn album_song_gates(provider: &str, album: &Album, gate: &AlbumGate) -> Vec<SongGate> {
    gate.tracks
        .iter()
        .map(|track| {
            let track_provider = album
                .songs
                .iter()
                .find(|song| song.external_id.as_deref() == Some(track.catalog_track_id.as_str()))
                .and_then(|song| song.external_provider.clone())
                .filter(|known| !dotnet::is_blank(known))
                .unwrap_or_else(|| provider.to_string());
            SongGate {
                song: format!("{track_provider}:{}", track.catalog_track_id),
                title: track.title.clone(),
                artist: track.artist.clone(),
                gate: gate.gate_for(track),
            }
        })
        .collect()
}

/// One person's album gate rebuilt from a job's per-song gates of album hearts, with each track's
/// song key, in the order the gates list them.
struct FiledAlbum {
    gate: AlbumGate,
    keys: Vec<String>,
}

/// Every person's album gate in a job's per-song gates, the first person first.
fn album_gates_of(gates: &[SongGate]) -> Vec<FiledAlbum> {
    let mut albums: Vec<FiledAlbum> = Vec::new();
    for gate in gates
        .iter()
        .filter(|gate| gate.gate.origin == AcquisitionOrigin::AlbumHeart)
    {
        let user = gate.gate.user.trim();
        let track = AlbumGateTrack {
            catalog_track_id: gate
                .song
                .split_once(':')
                .map_or_else(|| gate.song.clone(), |(_, id)| id.to_string()),
            title: gate.title.clone(),
            artist: gate.artist.clone(),
            duration_seconds: gate.gate.heard_seconds,
            r: gate.gate.r.clone().unwrap_or_default(),
            ..Default::default()
        };
        match albums
            .iter_mut()
            .find(|album| dotnet::eq_ignore_case(&album.gate.user, user))
        {
            Some(album) => {
                album.gate.tracks.push(track);
                album.keys.push(gate.song.clone());
            }
            None => albums.push(FiledAlbum {
                gate: AlbumGate {
                    user: user.to_string(),
                    origin: AcquisitionOrigin::AlbumHeart,
                    tracks: vec![track],
                },
                keys: vec![gate.song.clone()],
            }),
        }
    }
    albums
}

/// The song an album's catalog album is found from by its title: its first track with an ISRC
/// and a place on the album, else its first with a place.
fn album_song(album: &Album) -> Option<HeartSong> {
    let song = album
        .songs
        .iter()
        .find(|song| song.isrc.as_deref().is_some_and(|isrc| !isrc.trim().is_empty()) && song.track.is_some())
        .or_else(|| album.songs.iter().find(|song| song.track.is_some()))?;
    Some(HeartSong {
        artist: album.artist.clone(),
        title: song.title.clone(),
        album: album.title.clone(),
        disc: Some(
            song.disc_number
                .and_then(|disc| u32::try_from(disc).ok())
                .unwrap_or(1),
        ),
        position: song.track.and_then(|track| u32::try_from(track).ok()),
        total_tracks: u32::try_from(album.songs.len()).ok().filter(|count| *count > 0),
        isrc: song.isrc.clone().unwrap_or_default(),
        seconds: song
            .duration
            .and_then(|seconds| u32::try_from(seconds).ok())
            .unwrap_or(0),
        explicit: None,
    })
}

/// The release a barcode names that is the catalog album: one with its number of tracks first,
/// then the earliest date, then the id.
fn barcode_release(releases: &[BarcodeRelease], tracks: Option<u32>) -> Option<&BarcodeRelease> {
    releases
        .iter()
        .filter(|release| !release.group.id.trim().is_empty())
        .min_by_key(|release| {
            let count: usize = release.media.iter().map(|medium| medium.tracks.len()).sum();
            let differs = tracks.is_some_and(|wanted| wanted as usize != count);
            let date = if release.date.is_empty() {
                "9999"
            } else {
                release.date.as_str()
            };
            (differs, date, release.id.as_str())
        })
}

/// The line a held album track's downloads row shows while it waits for `waiting_for`.
fn held_line(waiting_for: WaitingFor) -> String {
    match waiting_for {
        WaitingFor::NotYetProven => tracks_wait_line(1),
        WaitingFor::AcoustId => "Waiting for AcoustID".to_string(),
        other => waiting_line(other).to_string(),
    }
}

impl Inner {
    /// Where an album heart's album is: the group of the release its catalog album's barcode
    /// names (the catalog album found from its title when the heart has no catalog id), else by
    /// name.
    pub(super) async fn album_find(
        &self,
        external_id: &str,
        album: &Album,
        origin: AcquisitionOrigin,
    ) -> AlbumFind {
        let Some(music_brainz) = self.music_brainz.clone() else {
            return AlbumFind::ByName;
        };
        let questions = HeartQuestions::new(music_brainz, self.catalog.clone(), lane_for(origin));
        let catalog_id = self
            .ids
            .as_ref()
            .and_then(|ids| ids.lookup(external_id))
            .and_then(|routing| routing.snapshot().external_album_id)
            .filter(|id| !dotnet::is_blank(id));
        let found = match &catalog_id {
            Some(id) => questions.catalog_album(id).await,
            None => match album_song(album) {
                Some(song) => questions.catalog_album_by_title(&song).await,
                None => Ok(None),
            },
        };
        let (catalog, tracks) = match found {
            Ok(Some(found)) => found,
            Ok(None) => return AlbumFind::ByName,
            Err(busy) => return AlbumFind::Busy(busy),
        };
        let releases = match questions.barcode_releases(&catalog.upc).await {
            Ok(releases) => releases,
            Err(busy) => return AlbumFind::Busy(busy),
        };
        let count = catalog
            .track_count
            .or_else(|| u32::try_from(tracks.len()).ok().filter(|count| *count > 0));
        match barcode_release(&releases, count) {
            Some(release) => AlbumFind::Group {
                group: release.group.id.clone(),
                release: release.id.clone(),
            },
            None => {
                info!(
                    "MusicBrainz names no release by the barcode {} of '{} - {}'; Lidarr is asked by name",
                    catalog.upc, album.artist, album.title
                );
                AlbumFind::ByName
            }
        }
    }

    /// Every song of an album heart Lidarr was not used for is missed with `reason`, when this
    /// is the last source.
    pub(super) fn album_missed(&self, album: &Album, notify_failure: bool, reason: &str) {
        let (true, Some(tracker)) = (notify_failure, &self.tracker) else {
            return;
        };
        for song in &album.songs {
            if let (Some(provider), Some(id)) = (
                song.external_provider.as_deref().filter(|p| !p.is_empty()),
                song.external_id.as_deref().filter(|id| !id.is_empty()),
            ) {
                tracker.fail(provider, id, Some(reason));
            }
        }
    }

    /// The job's own hold on the gates of the hearted albums its saved entry names, rebuilt from
    /// its per-song gates, so a follow a restart started again still files its import.
    pub(super) fn hold_album_gates(&self, job: &AlbumJob, hearts: &[String], gates: &[SongGate]) {
        let Some(store) = &self.album_gates else {
            return;
        };
        let mut holds = job.gate_holds.lock();
        if !holds.is_empty() {
            return;
        }
        for heart in hearts {
            let Some((provider, album_id)) = heart.split_once(':') else {
                continue;
            };
            for album in album_gates_of(gates) {
                holds.push(store.register(provider, album_id, album.gate, Vec::new()));
            }
        }
    }

    /// One file of Octo's import of a hearted album, filed among the album's tracks no earlier
    /// file was filed as, by its own sound against each one's own recording: a track it proves
    /// to be is handed to the pipeline as that track, with the verdict that proved it; one it
    /// waits for proof of is held in the one hold path as a held album track of each heart; one
    /// it is none of is deleted. Answers the pipeline's work, when it was handed on.
    pub(super) async fn file_album_import(
        self: &Arc<Self>,
        job: &Arc<AlbumJob>,
        key: &str,
        track: &LidarrImportedTrack,
        imported_path: String,
        dropped: Option<String>,
    ) -> Option<JoinHandle<()>> {
        let _turn = job.filing.lock().await;
        let labels = self.labels(key, dropped).await;
        let entry = self.pending.get(key)?;
        let albums = album_gates_of(&entry.gates);
        let (Some(filer), Some(album)) = (self.gates.clone(), albums.first()) else {
            return self.take_import(job, track, imported_path, labels, true).await;
        };
        let unfilled: Vec<usize> = album
            .keys
            .iter()
            .enumerate()
            .filter(|(_, song)| !entry.filed.contains(song) && !entry.passed.contains(song))
            .map(|(index, _)| index)
            .collect();
        let acquisition = entry
            .album_hearts
            .first()
            .cloned()
            .unwrap_or_else(|| key.to_string());
        let (filing, verdict) = filer
            .file_album_file(
                Path::new(&imported_path),
                labels.clone(),
                &album.gate,
                &unfilled,
                &acquisition,
            )
            .await;
        let filed = |index: usize| {
            let song = album.keys[index].clone();
            self.pending.update(key, |entry| {
                if !entry.filed.contains(&song) {
                    entry.filed.push(song.clone());
                }
            });
        };
        match filing {
            TrackFiling::Land { track: index, .. } => {
                filed(index);
                // The album's own listing of the track, or, for a follow a restart started again,
                // the track as its gate names it.
                let song = job
                    .view()
                    .songs
                    .into_iter()
                    .find(|song| song_entry_key(song).as_deref() == Some(album.keys[index].as_str()))
                    .or_else(|| {
                        let gate_track = &album.gate.tracks[index];
                        let (provider, id) = album.keys[index].split_once(':')?;
                        Some(Song {
                            title: gate_track.title.clone(),
                            artist: gate_track.artist.clone(),
                            album: job.album.title.clone(),
                            duration: gate_track.duration_seconds,
                            external_provider: Some(provider.to_string()),
                            external_id: Some(id.to_string()),
                            ..Default::default()
                        })
                    });
                info!(
                    "Lidarr's '{}' of {key} is {} by its own sound",
                    track.title, album.keys[index]
                );
                self.take_import_as(job, track, imported_path, labels, song, verdict.map(Box::new))
                    .await
            }
            TrackFiling::Hold {
                track: index,
                waiting_for,
            } => {
                filed(index);
                self.hold_album_track(
                    key,
                    track,
                    &imported_path,
                    &labels,
                    &entry.album_hearts,
                    &albums,
                    index,
                    waiting_for,
                )
                .await;
                None
            }
            TrackFiling::Refuse(refusal) => {
                self.drop_import(
                    key,
                    track,
                    &imported_path,
                    &format!("its sound is none of the hearted album's tracks ({refusal:?})"),
                )
                .await;
                None
            }
        }
    }

    /// A file of an album heart's import that waits for proof of the track it was filed as: moved
    /// into the one hold path (its Lidarr record deleted), and handed with where its labels were
    /// read from to each person whose heart gate lists the track, as that heart's held album
    /// track.
    #[allow(clippy::too_many_arguments)] // the file, the album's hearts and gates, and the track
    async fn hold_album_track(
        &self,
        key: &str,
        track: &LidarrImportedTrack,
        imported_path: &str,
        labels: &LabelSource,
        hearts: &[String],
        albums: &[FiledAlbum],
        index: usize,
        waiting_for: WaitingFor,
    ) {
        let song = albums[0].keys[index].clone();
        let (provider, track_id) = song.split_once(':').unwrap_or(("", song.as_str()));
        let record = HeldAcquisition {
            kind: HeldKind::Track,
            provider: provider.to_string(),
            external_id: track_id.to_string(),
            ..Default::default()
        };
        let folder = record.hold_folder(Path::new(&self.music.octo_root()));
        let name = Path::new(imported_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{}.audio", track.track_file_id));
        let held_path = folder.join(format!("{}-{name}", uuid::Uuid::new_v4().simple()));
        let moved = std::fs::create_dir_all(&folder).is_ok()
            && (std::fs::rename(imported_path, &held_path).is_ok()
                || (std::fs::copy(imported_path, &held_path).is_ok()
                    && std::fs::remove_file(imported_path).is_ok()));
        if !moved {
            warn!("Could not hold {imported_path} for its album; it is deleted");
            self.drop_import(key, track, imported_path, "it could not be held for its album")
                .await;
            return;
        }
        delete_moved_record(
            &self.client,
            Some(self.pending.as_ref()),
            key,
            track.track_file_id,
            imported_path,
        )
        .await;
        let held_path = held_path.to_string_lossy().into_owned();
        let users: Vec<String> = albums.iter().map(|album| album.gate.user.clone()).collect();
        let held = AcquisitionHeld {
            waiting_for,
            line: held_line(waiting_for),
            files: vec![held_path.clone()],
            lidarr: vec![HeldLidarrFile::of(&held_path, labels)],
            users: users.clone(),
        };
        let hook = self.album_holds.lock().clone();
        let mut handed = false;
        if let Some(store) = &self.album_gates {
            for heart in hearts {
                let Some((album_provider, album_id)) = heart.split_once(':') else {
                    continue;
                };
                for user in &users {
                    let lists_it = store
                        .gate(album_provider, album_id, user)
                        .is_some_and(|gate| gate.track(track_id).is_some());
                    if !lists_it {
                        continue;
                    }
                    store.held(
                        album_provider,
                        album_id,
                        std::slice::from_ref(user),
                        track_id,
                        held.clone(),
                    );
                    handed = true;
                    if let Some(hook) = &hook {
                        hook(
                            album_provider,
                            album_id,
                            user.as_str(),
                            AcquisitionOrigin::AlbumHeart,
                        );
                    }
                }
            }
        }
        if handed {
            info!(
                "Lidarr's '{}' of {key} waits as {song} ({}): {held_path}",
                track.title, held.line
            );
        } else {
            let _ = std::fs::remove_file(&held_path);
            warn!(
                "No album heart's gate lists {song} any more; Lidarr's '{}' is deleted",
                track.title
            );
        }
    }
}

/// What filing a file proved, for the import that hands it on.
pub(super) type Filed = Option<Box<VerificationResult>>;
