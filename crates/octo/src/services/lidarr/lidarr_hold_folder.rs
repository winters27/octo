//! The album's tracks Octo holds while a hearted song's file is proven.
//!
//! - **Held at once.** When a heart's or a play's gate is on a song of the album, every track of
//!   Octo's own import is moved, the moment the follow judges it Octo's, into the album's hold
//!   folder under the download path's incoming dot folder, which no library scan and no sweep
//!   reads. Its Lidarr track file record is deleted once the file is moved, and the track is saved
//!   on the pending entry, so a restart finds it.
//! - **The hearted song first.** Each hearted song's candidates are the held tracks whose core
//!   title is the song's, those Lidarr matched to the song's own recording first: Lidarr's match
//!   only orders them. Each is handed to the download pipeline as the song, with every person's
//!   own gate for it, until one passes: that one is the hearted song. A refused candidate comes
//!   back to the hold folder, a track of the album in its own right; one whose gate waits (for
//!   AcoustID) waits in the one hold path, and the heart holds with it.
//! - **The rest.** Once a hearted song passed, the album's other tracks (refused candidates
//!   included) go on to the pipeline as the album's own tracks, under ids of their own; on a pick
//!   another artist leads they are deleted instead. When the follow ends and no hearted song
//!   passed, every held track is deleted. Octo deletes held files itself, and the hold folder goes
//!   with its pending entry.
//! - **One file.** A copy kept as heard (or kept by the person) that waited for the follow is
//!   dropped once the song's own Lidarr file passes the gates of everyone it was kept for; one
//!   Octo placed during it when a person stopped waiting, once that file passes the gate of
//!   anyone it was kept for (the library holds one file of the song). They end with the proven
//!   file alone. A file refused for them is never kept as one of the album's other tracks, and a
//!   placed copy Lidarr's import replaced comes back from its link.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::fingerprint::heart_gate::HeartGate;
use octo_core::fingerprint::verification::core_title;
use octo_core::lidarr::LidarrImportedTrack;
use octo_core::lidarr::lidarr_heart_acquisition_service::candidate_order;
use octo_core::models::domain::Song;
use octo_core::settings::{DownloadSource, LidarrSettings};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{AlbumJob, Inner, key_of, match_song, song_entry_key, translate_imported_path};
use crate::services::common::{AcquisitionHeld, AcquisitionPlace, BaseDownloadService, GateLedger};
use crate::services::fingerprint::download_verification_service::LabelSource;
use crate::services::lidarr::LidarrImport;
use crate::services::lidarr::lidarr_pending_albums::{HELD_FOR_LIDARR, HeldTrack, SongGate};
use crate::services::lidarr::lidarr_pick::delete_moved_record;
use crate::services::soulseek::SoulseekMetadataService;
use crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME;

/// The folder under the incoming dot folder that holds each album's held tracks.
pub(crate) const HOLD_FOLDER_NAME: &str = "lidarr-held";

/// How one candidate of a hearted song came out of the pipeline.
enum Candidate {
    /// Its file passed the gate: it is the hearted song.
    Passed,
    /// It is not the hearted song; `back` when its file is back in the hold folder.
    Refused { back: bool },
    /// Its file waits in the one hold path for what its gate needs.
    Held(AcquisitionHeld),
}

/// An album's hold folder name: its foreign id, with nothing a path could misread.
fn folder_name(foreign_album_id: &str) -> String {
    foreign_album_id
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Moves a file, copying it when it lies on another volume.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

impl Inner {
    /// Where the album's tracks are held.
    pub(super) fn hold_folder(&self, foreign_album_id: &str) -> PathBuf {
        Path::new(&self.music.octo_root())
            .join(INCOMING_FOLDER_NAME)
            .join(HOLD_FOLDER_NAME)
            .join(folder_name(foreign_album_id))
    }

    /// The download's own file as delivered, when Octo can read it: at the path Lidarr names, or
    /// under Lidarr's root folder as Octo sees it.
    pub(super) fn delivered(&self, dropped: &str, settings: &LidarrSettings) -> Option<String> {
        if Path::new(dropped).is_file() {
            return Some(dropped.to_string());
        }
        translate_imported_path(
            dropped,
            settings.root_folder_path.as_deref(),
            &self.music.octo_root(),
        )
        .ok()
        .filter(|path| Path::new(path).is_file())
    }

    /// Where a file of Octo's import has its own labels: the download as delivered when Octo can
    /// still read it, else the file itself only when Lidarr writes no tags (read once per follow),
    /// else nowhere. Lidarr's own matching is never a label.
    pub(super) async fn labels(&self, foreign_album_id: &str, dropped: Option<String>) -> LabelSource {
        if let Some(dropped) = dropped.filter(|path| Path::new(path).is_file()) {
            return LabelSource::Delivered(PathBuf::from(dropped));
        }
        let known = self
            .pending
            .get(foreign_album_id)
            .and_then(|entry| entry.writes_audio_tags);
        let writes = match known {
            Some(writes) => writes,
            None => match self.client.writes_audio_tags().await {
                Ok(writes) => {
                    self.pending
                        .update(foreign_album_id, |entry| entry.writes_audio_tags = Some(writes));
                    writes
                }
                Err(e) => {
                    warn!("Could not read whether Lidarr writes tags: {e}; its files' own tags are not read");
                    true
                }
            },
        };
        if writes {
            LabelSource::None
        } else {
            LabelSource::TheFile
        }
    }

    /// Moves one file of Octo's import into the album's hold folder at once, saves it on the
    /// entry, and deletes its Lidarr record. False when it could not be moved.
    pub(super) async fn hold_track(
        &self,
        foreign_album_id: &str,
        track: &LidarrImportedTrack,
        album_id: i32,
        imported_path: &str,
        dropped: Option<String>,
    ) -> bool {
        let folder = self.hold_folder(foreign_album_id);
        if let Err(e) = std::fs::create_dir_all(&folder) {
            warn!("Could not make the hold folder {}: {e}", folder.display());
            return false;
        }
        let name = Path::new(imported_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{}.audio", track.track_file_id));
        let mut held_path = folder.join(&name);
        if held_path.exists() {
            held_path = folder.join(format!("{}-{name}", track.track_file_id));
        }
        if let Err(e) = move_file(Path::new(imported_path), &held_path) {
            warn!("Could not hold {imported_path} back for its album: {e}");
            return false;
        }
        let octo_root = self.music.octo_root();
        BaseDownloadService::try_remove_empty_parents(
            Path::new(imported_path).parent().and_then(Path::to_str),
            &octo_root,
        );
        let held = HeldTrack {
            track_file_id: track.track_file_id,
            track_id: track.id,
            album_id,
            title: track.title.clone(),
            artist: track.artist.clone(),
            number: track.track_number,
            seconds: track.duration_seconds,
            quality: track.quality.clone(),
            recording: track.foreign_recording_id.clone(),
            path: held_path.to_string_lossy().into_owned(),
            dropped,
            refused_for: Vec::new(),
        };
        info!(
            "Holding '{}' of {foreign_album_id} in its hold folder until a hearted song passes: {}",
            track.title, held.path
        );
        self.pending.update(foreign_album_id, |entry| {
            if !entry
                .held_tracks
                .iter()
                .any(|known| known.track_file_id == held.track_file_id)
            {
                entry.held_tracks.push(held);
            }
        });
        delete_moved_record(
            &self.client,
            Some(self.pending.as_ref()),
            foreign_album_id,
            track.track_file_id,
            imported_path,
        )
        .await;
        true
    }

    /// Settles the album's hold folder as far as it can now: each hearted song's next candidate is
    /// judged, one at a time, until a song passes or has none left; once one passed, the album's
    /// other tracks go on (or go, on a pick another artist leads). `last`: nothing more comes, so a
    /// candidate waits for nothing, and with no song passed every held track is deleted and the
    /// hold folder goes.
    pub(super) async fn settle(self: Arc<Self>, job: Arc<AlbumJob>, foreign_album_id: String, last: bool) {
        let _turn = job.settling.lock().await;
        while let Some(entry) = self.pending.get(&foreign_album_id) {
            let Some((song, gates, track)) =
                next_candidate(&job, &entry.gates, &entry.held_tracks, &entry.passed)
            else {
                break;
            };
            self.judge_candidate(&job, &foreign_album_id, &song, &gates, &track)
                .await;
        }
        let Some(entry) = self.pending.get(&foreign_album_id) else {
            self.drop_hold_folder(&foreign_album_id);
            return;
        };
        if !entry.passed.is_empty() {
            self.release_held(&job, &foreign_album_id, last).await;
        } else if last {
            self.delete_held(&foreign_album_id, "no hearted song of the album passed");
        }
        if last {
            self.drop_hold_folder(&foreign_album_id);
        }
    }

    /// Hands one candidate to the pipeline as the hearted song, with its gates, and saves what came
    /// of it.
    async fn judge_candidate(
        self: &Arc<Self>,
        job: &Arc<AlbumJob>,
        foreign_album_id: &str,
        song_key: &str,
        gates: &[SongGate],
        track: &HeldTrack,
    ) {
        let Some(first) = gates.first() else {
            return;
        };
        let (provider, id) = song_key.split_once(':').unwrap_or(("", song_key));
        let song = Song {
            title: first.title.clone(),
            artist: first.artist.clone(),
            album: job.album.title.clone(),
            external_provider: Some(provider.to_string()),
            external_id: Some(id.to_string()),
            ..Default::default()
        };
        let labels = self.labels(foreign_album_id, track.dropped.clone()).await;
        let ledger = GateLedger::default();
        let heart_gates: Vec<HeartGate> = gates.iter().map(|gate| gate.gate.clone()).collect();
        info!(
            "Lidarr's '{}' may be {song_key}; judging it by the gate of {}",
            track.title,
            heart_gates
                .iter()
                .map(|gate| gate.user.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let file_id = track.track_file_id;
        match self
            .hand_candidate(job, &song, track, heart_gates, labels, ledger.clone())
            .await
        {
            Candidate::Passed => {
                let users: Vec<String> = gates
                    .iter()
                    .map(|gate| gate.gate.user.clone())
                    .filter(|user| ledger.is_empty() || ledger.accepted(user))
                    .collect();
                info!(
                    "Lidarr's '{}' is {song_key}, proven for {}",
                    track.title,
                    users.join(", ")
                );
                job.accepted
                    .lock()
                    .entry(song_key.to_string())
                    .or_default()
                    .extend(users.iter().cloned());
                let key = song_key.to_string();
                self.pending.update(foreign_album_id, |entry| {
                    if !entry.passed.contains(&key) {
                        entry.passed.push(key);
                    }
                    entry.held_tracks.retain(|held| held.track_file_id != file_id);
                });
                job.add_landed(Some(&song), Some(&track.imported()), &job.view());
                self.drop_kept_copies(song_key, &users).await;
            }
            Candidate::Refused { back } => {
                // Lidarr's file is not the song for anyone a copy of it was kept for: a kept copy
                // Lidarr's import replaced comes back.
                self.restore_replaced(Some(foreign_album_id), Some(song_key))
                    .await;
                let key = song_key.to_string();
                self.pending.update(foreign_album_id, |entry| {
                    if back {
                        for held in entry
                            .held_tracks
                            .iter_mut()
                            .filter(|held| held.track_file_id == file_id)
                        {
                            if !held.refused_for.contains(&key) {
                                held.refused_for.push(key.clone());
                            }
                        }
                    } else {
                        entry.held_tracks.retain(|held| held.track_file_id != file_id);
                    }
                });
            }
            Candidate::Held(held) => {
                self.pending.update(foreign_album_id, |entry| {
                    entry.held_tracks.retain(|known| known.track_file_id != file_id);
                });
                if job.waits_for(&key_of(&song)) {
                    job.hold(&key_of(&song), held);
                } else {
                    // Nobody waits on it any more, so nothing can hold for it: a late import that
                    // cannot be gated now is deleted.
                    for file in &held.files {
                        let _ = std::fs::remove_file(file);
                    }
                    info!(
                        "Lidarr's late '{}' could not be judged now and nobody waits on it; deleted",
                        track.title
                    );
                    self.restore_replaced(Some(foreign_album_id), Some(song_key))
                        .await;
                }
            }
        }
    }

    /// One candidate through the download pipeline as the hearted song: offered from the hold
    /// folder with its labels, judged by `gates`, and given back to the hold folder when refused.
    async fn hand_candidate(
        &self,
        job: &AlbumJob,
        song: &Song,
        track: &HeldTrack,
        gates: Vec<HeartGate>,
        labels: LabelSource,
        ledger: GateLedger,
    ) -> Candidate {
        let Some(downloads) = self.downloads.clone() else {
            warn!(
                "Lidarr brought '{}', but there is no download pipeline to judge it",
                track.title
            );
            return Candidate::Refused {
                back: Path::new(&track.path).is_file(),
            };
        };
        let provider = SoulseekMetadataService::PROVIDER_NAME;
        let own = song
            .external_provider
            .as_deref()
            .is_some_and(|p| dotnet::eq_ignore_case(p, provider));
        let id = match song.external_id.as_deref().filter(|id| own && !id.is_empty()) {
            Some(id) => id.to_string(),
            None => match &self.ids {
                Some(ids) => ids.register(SoulseekRouting {
                    kind: RoutingKind::Song,
                    artist: Some(song.artist.clone()),
                    title: Some(song.title.clone()),
                    album: Some(job.album.title.clone()),
                    duration: track.seconds,
                    track: track.number,
                    ..Default::default()
                }),
                None => {
                    return Candidate::Refused {
                        back: Path::new(&track.path).is_file(),
                    };
                }
            },
        };
        let origin = gates.first().map(|gate| gate.origin);
        self.imports.offer_import(
            &id,
            LidarrImport {
                path: track.path.clone(),
                quiet: job.album_heart(),
                labels,
                refused_to: Some(track.path.clone()),
                filed: None,
            },
        );
        let result = downloads
            .execute_acquisition_to(
                provider,
                &id,
                false,
                true,
                Some(DownloadSource::Lidarr),
                &CancellationToken::new(),
                job.asked_by(),
                false,
                None,
                AcquisitionPlace {
                    origin,
                    gates,
                    ledger,
                    ..Default::default()
                },
            )
            .await;
        let untouched = self.imports.withdraw(&id);
        match result {
            Ok(path) => {
                if untouched && Path::new(&track.path).is_file() {
                    // The library already had the song for these people: this copy is not wanted.
                    let _ = std::fs::remove_file(&track.path);
                    info!(
                        "{} already had a copy for its gates ({path}); Lidarr's is deleted",
                        song.title
                    );
                }
                Candidate::Passed
            }
            Err(e) => {
                if let Some(held) = e.downcast_ref::<AcquisitionHeld>() {
                    return Candidate::Held(held.clone());
                }
                info!("Lidarr's '{}' is not {}: {e}", track.title, song.title);
                job.missed.lock().insert(key_of(song), e.to_string());
                Candidate::Refused {
                    back: Path::new(&track.path).is_file(),
                }
            }
        }
    }

    /// Once a hearted song passed: every held track no undecided hearted song may still be goes on
    /// to the pipeline as one of the album's own tracks, or is deleted on a pick another artist
    /// leads. `last`: none waits any more.
    async fn release_held(self: &Arc<Self>, job: &Arc<AlbumJob>, foreign_album_id: &str, last: bool) {
        let Some(entry) = self.pending.get(foreign_album_id) else {
            return;
        };
        let hearted: Vec<&SongGate> = entry.gates.iter().filter(|gate| gate.personal()).collect();
        let led_by_another = entry.various_artists
            || (!hearted.is_empty()
                && !hearted
                    .iter()
                    .any(|gate| SongIdentity::artists_agree(&job.candidate.artist, &gate.artist)));
        let undecided: Vec<(&str, String)> = hearted
            .iter()
            .filter(|gate| !entry.passed.contains(&gate.song) && !job.held.lock().contains_key(&gate.song))
            .map(|gate| (gate.song.as_str(), core_title(&gate.title)))
            .collect();
        for track in &entry.held_tracks {
            let core = core_title(&track.title);
            let may_be_undecided = undecided.iter().any(|(song, title)| {
                *title == core && !track.refused_for.iter().any(|refused| refused == song)
            });
            if may_be_undecided && !last {
                continue;
            }
            let file_id = track.track_file_id;
            self.pending.update(foreign_album_id, |entry| {
                entry.held_tracks.retain(|held| held.track_file_id != file_id);
            });
            // A song whose file is a person's kept copy takes Lidarr's file only by that person's
            // gate, never as one of the album's other tracks: the library holds one file of it.
            let kept_song = match_song(&job.view(), &track.imported(), job.match_by_number())
                .and_then(song_entry_key)
                .is_some_and(|song| self.pending.kept_copy_for(&song));
            if kept_song {
                let _ = std::fs::remove_file(&track.path);
                info!(
                    "'{}' of {foreign_album_id} is not kept: a kept copy is that song's file",
                    track.title
                );
                continue;
            }
            if led_by_another {
                let _ = std::fs::remove_file(&track.path);
                info!(
                    "'{}' of {foreign_album_id}, a pick another artist leads, is not kept",
                    track.title
                );
                continue;
            }
            let labels = self.labels(foreign_album_id, track.dropped.clone()).await;
            if let Some(handoff) = self
                .take_import(job, &track.imported(), track.path.clone(), labels, false)
                .await
            {
                let _ = handoff.await;
            }
        }
    }

    /// Deletes every track the album's hold folder still holds: Octo deletes each file itself.
    pub(super) fn delete_held(&self, foreign_album_id: &str, why: &str) {
        let Some(entry) = self.pending.get(foreign_album_id) else {
            return;
        };
        for track in &entry.held_tracks {
            match std::fs::remove_file(&track.path) {
                Ok(()) => info!("Deleted the held '{}' of {foreign_album_id}: {why}", track.title),
                Err(e) if Path::new(&track.path).exists() => {
                    warn!("Could not delete the held {}: {e}", track.path);
                }
                Err(_) => {}
            }
        }
        self.pending
            .update(foreign_album_id, |entry| entry.held_tracks.clear());
    }

    /// The album's hold folder goes once it is empty, and the folder of hold folders with its last.
    fn drop_hold_folder(&self, foreign_album_id: &str) {
        let folder = self.hold_folder(foreign_album_id);
        if std::fs::remove_dir(&folder).is_ok()
            && let Some(parent) = folder.parent()
        {
            let _ = std::fs::remove_dir(parent);
        }
    }

    /// After a restart: a hold folder no pending entry names is left from a pick that is over, so
    /// it goes with what it holds.
    pub(super) fn sweep_hold_folders(&self) {
        let root = Path::new(&self.music.octo_root())
            .join(INCOMING_FOLDER_NAME)
            .join(HOLD_FOLDER_NAME);
        let Ok(folders) = std::fs::read_dir(&root) else {
            return;
        };
        let listed: Vec<String> = self
            .pending
            .entries()
            .iter()
            .map(|entry| folder_name(&entry.foreign_album_id))
            .collect();
        for folder in folders.flatten() {
            let name = folder.file_name().to_string_lossy().into_owned();
            if folder.path().is_dir() && !listed.contains(&name) {
                match std::fs::remove_dir_all(folder.path()) {
                    Ok(()) => info!("Removed the hold folder of a Lidarr pick that is over: {name}"),
                    Err(e) => warn!(
                        "Could not remove the hold folder {}: {e}",
                        folder.path().display()
                    ),
                }
            }
        }
        let _ = std::fs::remove_dir(&root);
    }

    /// A copy of the song kept as heard, or kept by its person, waits for the follow, or was placed
    /// during it when its person stopped waiting: once the song's Lidarr file passed the gates of
    /// everyone a waiting copy was kept for (of anyone a placed copy was kept for: the library
    /// holds one file of the song), it is dropped (a placed copy only while it is still the file
    /// Octo placed, one Lidarr's import replaced only let go of), and its link with it, so they
    /// end with the proven file alone.
    async fn drop_kept_copies(&self, song_key: &str, users: &[String]) {
        let Some(downloads) = &self.downloads else {
            return;
        };
        for placed in self.pending.giving_way(song_key, users) {
            self.give_way(&placed).await;
        }
        for held in self
            .pending
            .held()
            .into_iter()
            .filter(|held| held.kept && held.song == song_key)
        {
            if downloads.drop_held(&held, users).await {
                self.pending.unhold(&held.held_path);
                if let Some(tracker) = &self.tracker
                    && let Some((provider, id)) = song_key.split_once(':')
                {
                    let waiting = self.pending.kept_copies_waiting(song_key);
                    if waiting.is_empty() {
                        tracker.end_aside(provider, id, None);
                    } else {
                        let still: Vec<String> = waiting
                            .iter()
                            .flat_map(|copy| copy.users.iter().cloned())
                            .collect();
                        tracker.wait_aside(provider, id, &still, HELD_FOR_LIDARR);
                    }
                }
                info!(
                    "The kept copy of {song_key} gives way to its proven Lidarr file: {}",
                    held.held_path
                );
            }
        }
    }
}

/// The next candidate to judge, with its song and that song's gates: of the hearted songs with a
/// heart's or a play's gate that did not pass and whose file is not held, the first held track
/// in candidate order that the song's gates have not refused.
fn next_candidate(
    job: &AlbumJob,
    gates: &[SongGate],
    held_tracks: &[HeldTrack],
    passed: &[String],
) -> Option<(String, Vec<SongGate>, HeldTrack)> {
    let mut songs: Vec<(String, Vec<SongGate>)> = Vec::new();
    for gate in gates.iter().filter(|gate| gate.personal()) {
        match songs.iter_mut().find(|(song, _)| *song == gate.song) {
            Some((_, known)) => known.push(gate.clone()),
            None => songs.push((gate.song.clone(), vec![gate.clone()])),
        }
    }
    for (song, gates) in songs {
        if passed.contains(&song) || job.held.lock().contains_key(&song) {
            continue;
        }
        let tries: Vec<&HeldTrack> = held_tracks
            .iter()
            .filter(|track| !track.refused_for.contains(&song))
            .collect();
        let recordings: Vec<String> = gates
            .iter()
            .filter_map(|gate| gate.gate.r.as_ref())
            .flat_map(|r| r.ids.iter().cloned())
            .collect();
        let listed: Vec<(&str, Option<&str>)> = tries
            .iter()
            .map(|track| (track.title.as_str(), track.recording.as_deref()))
            .collect();
        let title = gates.first().map(|gate| gate.title.clone()).unwrap_or_default();
        if let Some(first) = candidate_order(&title, &recordings, &listed).first() {
            return Some((song, gates, tries[*first].clone()));
        }
    }
    None
}
