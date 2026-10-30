//! `lidarr-pending.json`: the albums Octo asked Lidarr for, each with the follow of its search.
//! An entry is saved before anything is sent to Lidarr and holds everything a restart needs: the
//! put-back facts, every person's gate for each song, Octo's own downloads on their watch list,
//! the album's tracks Octo holds in its hold folder until the hearted song passes, and the files
//! Octo holds back from the library while the follow is open. An entry ends only on work: its
//! follow or its put-back ends, never its age. An album nobody listed here is the Lidarr owner's
//! own and is left alone.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use octo_core::common::dotnet;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeartGate};
use octo_core::lidarr::LidarrImportedTrack;
use octo_core::lidarr::lidarr_client::LidarrReleaseChoice;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::warn;

use crate::services::library::navidrome_song_path_resolver::{get_full_path, is_inside};
use crate::services::state_file;

/// The downloads line of a file held back from the library while a follow is open.
pub const HELD_FOR_LIDARR: &str =
    "Plays now; joins your library when Lidarr's download for this artist finishes";
/// The downloads line of a held file whose follow a proven file of another source ended.
pub const PROVEN_ELSEWHERE: &str = "Octo stopped waiting for Lidarr: another copy was proven";
/// The follow's button.
pub const STOP_WAITING: &str = "Stop waiting for Lidarr";
/// The downloads line of a file of the user's that Lidarr's import of Octo's download replaced.
pub const REPLACED_YOURS: &str = "Lidarr replaced a file of yours while importing Octo's download";

/// One album Octo asked Lidarr for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PendingAlbum {
    /// The MusicBrainz release group id Lidarr knows the album by.
    pub foreign_album_id: String,
    /// Lidarr's own id for the album.
    pub lidarr_album_id: i32,
    pub artist: String,
    pub title: String,
    pub year: Option<i32>,
    /// The track files Lidarr had before Octo's search; they are the owner's, never taken.
    pub files_before: Vec<i32>,
    /// The tracks that already had a file before Octo's search. Their files stay the owner's
    /// even when Lidarr later replaces them with a new file id.
    pub tracks_before: Vec<i32>,
    /// Who hearted it.
    pub requested_by: Vec<String>,
    pub asked_at: DateTime<Utc>,
    /// Lidarr's own id for the album's artist; 0 while it is not known.
    pub lidarr_artist_id: i32,
    /// The add Octo was about to send, saved before the request went out; None when Lidarr had
    /// the album already.
    pub add: Option<PendingAdd>,
    /// Octo's add created the album.
    pub album_added: bool,
    /// The artist Octo's add created, by Lidarr's id; 0 when Lidarr had the artist already.
    pub artist_added: i32,
    /// Every album Lidarr listed for an artist Octo's add created, right after the add's refresh.
    pub albums_after_add: Vec<i32>,
    /// The album's release choice before Octo pinned it, put back exactly afterwards.
    pub release_choice: Option<LidarrReleaseChoice>,
    /// The release choice Octo's pin set; a choice that is no longer this one was changed by the
    /// user, and stays as the user set it.
    pub pin: Option<LidarrReleaseChoice>,
    /// The AlbumSearch Octo posted, saved the moment Lidarr answered; 0 before the search.
    pub search_command: i32,
    /// When the search was posted.
    pub searched_at: Option<DateTime<Utc>>,
    /// Every download id of Octo's own grabs for this album, accounted for or not.
    pub grab_ids: Vec<String>,
    /// Track file records of Octo's moved imports that Lidarr would not delete yet, asked again
    /// later.
    pub records_to_delete: Vec<i32>,
    /// The tracks whose late imports Octo dealt with: their records may be gone since Octo moved
    /// the files, and they count as in.
    pub tracks_taken: Vec<i32>,
    /// The songs the album was searched for, as `provider:id`: every heart and play on it.
    pub songs: Vec<String>,
    /// The people whose heart is on the album: once every one of them took it back, the follow
    /// ends.
    pub holders: Vec<String>,
    /// Each person's gate for each song, one per song and person; a heart that joins later keeps
    /// the first one's.
    pub gates: Vec<SongGate>,
    /// The album's tracks of Octo's own import, moved into its hold folder and their Lidarr
    /// records deleted, waiting until a hearted song passes its gate.
    pub held_tracks: Vec<HeldTrack>,
    /// The hearted songs whose file passed its gate, as `provider:id`.
    pub passed: Vec<String>,
    /// Whether Lidarr wrote its own tags into what it imported, read once for the follow.
    pub writes_audio_tags: Option<bool>,
    /// The track files the follow handed to the download pipeline.
    pub files_handed: Vec<i32>,
    /// Octo's own downloads, each on the watch list until it is accounted for.
    pub grabs: Vec<FollowedGrab>,
    /// Lidarr's own new searches of the album after a failed grab of Octo's: their grabs are
    /// Octo's too.
    pub research_commands: Vec<i32>,
    /// Releases Lidarr holds back for the album, which have no download id and are followed by
    /// their queue id.
    pub pending_items: Vec<FollowedItem>,
    /// The track files the follow judged Octo's own, by their own import row.
    pub octo_files: Vec<i32>,
    /// The track files the follow judged the user's: a row with no client or another download,
    /// or no row once Lidarr's import commands were idle.
    pub users_files: Vec<i32>,
    /// Lidarr's `trackFileDeleted` rows already told as a file of the user's that Octo's import
    /// replaced.
    pub replaced_told: Vec<i32>,
    /// Whether Lidarr imported finished downloads itself when the follow started.
    pub completed_download_handling: Option<bool>,
    /// Lidarr's root folders as Octo sees them: a file Octo would place under one of them for
    /// this artist waits while the follow is open.
    pub roots: Vec<String>,
    /// The album is filed under Various Artists, so a file is the follow's by its album title.
    pub various_artists: bool,
    /// When the follow first read Lidarr: where a search window ends that Lidarr no longer lists.
    pub first_poll: Option<DateTime<Utc>>,
    /// Why the follow ended, once it has; the pick stays listed until every download of Octo's
    /// is accounted for.
    pub ended: Option<FollowEnd>,
    /// Octo's items were removed and the files the follow held were let go.
    pub released: bool,
    /// The release picked in Find songs: its grab is Octo's by its guid.
    pub picked_guid: Option<String>,
    /// The hearted albums this album was searched for, as `provider:id`: their gates file each
    /// track of Octo's import by its own sound.
    pub album_hearts: Vec<String>,
    /// The songs of those albums a file of Octo's import was filed as, landed or held, as
    /// `provider:id`: no later file is filed as one of them.
    pub filed: Vec<String>,
    /// A replacement's fetch searched the album: nothing of its import is the library's but the
    /// song a fetch waits for, and a file nobody waits for is held out of the library and
    /// deleted with the pick.
    pub replacement: bool,
}

impl Default for PendingAlbum {
    fn default() -> Self {
        PendingAlbum {
            foreign_album_id: String::new(),
            lidarr_album_id: 0,
            artist: String::new(),
            title: String::new(),
            year: None,
            files_before: Vec::new(),
            tracks_before: Vec::new(),
            requested_by: Vec::new(),
            asked_at: Utc::now(),
            lidarr_artist_id: 0,
            add: None,
            album_added: false,
            artist_added: 0,
            albums_after_add: Vec::new(),
            release_choice: None,
            pin: None,
            search_command: 0,
            searched_at: None,
            grab_ids: Vec::new(),
            records_to_delete: Vec::new(),
            tracks_taken: Vec::new(),
            songs: Vec::new(),
            holders: Vec::new(),
            gates: Vec::new(),
            held_tracks: Vec::new(),
            passed: Vec::new(),
            writes_audio_tags: None,
            files_handed: Vec::new(),
            grabs: Vec::new(),
            research_commands: Vec::new(),
            pending_items: Vec::new(),
            octo_files: Vec::new(),
            users_files: Vec::new(),
            replaced_told: Vec::new(),
            completed_download_handling: None,
            roots: Vec::new(),
            various_artists: false,
            first_poll: None,
            ended: None,
            released: false,
            picked_guid: None,
            album_hearts: Vec::new(),
            filed: Vec::new(),
            replacement: false,
        }
    }
}

impl PendingAlbum {
    /// Whether Octo posted the album's search.
    pub fn searched(&self) -> bool {
        self.search_command > 0 || self.searched_at.is_some()
    }

    /// Whether the follow of the search is open: searched, and not ended.
    pub fn following(&self) -> bool {
        self.searched() && self.ended.is_none()
    }

    /// Whether a download id is one of this entry's own.
    pub fn owns_grab(&self, download_id: &str) -> bool {
        self.grab_ids
            .iter()
            .any(|id| dotnet::eq_ignore_case(id, download_id))
    }

    /// The downloads still on the watch list.
    pub fn watched(&self) -> impl Iterator<Item = &FollowedGrab> {
        self.grabs.iter().filter(|grab| !grab.accounted)
    }

    /// Records a download of Octo's own on the watch list; false when it was there already.
    pub fn record_grab(&mut self, download_id: &str, client: Option<&str>) -> bool {
        if self.owns_grab(download_id) {
            return false;
        }
        self.grab_ids.push(download_id.to_string());
        self.grabs.push(FollowedGrab {
            download_id: download_id.to_string(),
            client: client.map(str::to_string),
            ..Default::default()
        });
        true
    }

    /// One of Octo's downloads by its id.
    pub fn grab_mut(&mut self, download_id: &str) -> Option<&mut FollowedGrab> {
        self.grabs
            .iter_mut()
            .find(|grab| dotnet::eq_ignore_case(&grab.download_id, download_id))
    }

    /// The gates of one song, as `provider:id`, one per person.
    pub fn gates_of(&self, song: &str) -> Vec<HeartGate> {
        self.gates
            .iter()
            .filter(|gate| gate.song == song)
            .map(|gate| gate.gate.clone())
            .collect()
    }

    /// Takes in what another save of the same album says: the union of the people, their gates
    /// (a person already listed for a song keeps their first gate for it), the songs, the files
    /// and tracks Lidarr had before, the tracks held and the songs passed, and every recorded
    /// download; the put-back facts as the earliest save had them, filled in where it had none.
    /// So a second heart never makes Octo's album look like the user's, and never drops the first
    /// heart's gate.
    pub fn merge(&mut self, other: PendingAlbum) {
        fn union<T: PartialEq>(into: &mut Vec<T>, from: Vec<T>) {
            for item in from {
                if !into.contains(&item) {
                    into.push(item);
                }
            }
        }
        if self.lidarr_album_id <= 0 {
            self.lidarr_album_id = other.lidarr_album_id;
        }
        if self.lidarr_artist_id <= 0 {
            self.lidarr_artist_id = other.lidarr_artist_id;
        }
        if dotnet::is_blank(&self.artist) {
            self.artist = other.artist;
        }
        if dotnet::is_blank(&self.title) {
            self.title = other.title;
        }
        self.year = self.year.or(other.year);
        union(&mut self.files_before, other.files_before);
        self.files_before.sort_unstable();
        union(&mut self.tracks_before, other.tracks_before);
        self.tracks_before.sort_unstable();
        for name in other.requested_by {
            if !self
                .requested_by
                .iter()
                .any(|known| dotnet::eq_ignore_case(known.trim(), name.trim()))
            {
                self.requested_by.push(name);
            }
        }
        self.asked_at = self.asked_at.min(other.asked_at);
        if self.add.is_none() {
            self.add = other.add;
        }
        self.album_added |= other.album_added;
        if self.artist_added <= 0 {
            self.artist_added = other.artist_added;
        }
        if self.albums_after_add.is_empty() {
            self.albums_after_add = other.albums_after_add;
        }
        if self.release_choice.is_none() {
            self.release_choice = other.release_choice;
        }
        if self.pin.is_none() {
            self.pin = other.pin;
        }
        if self.search_command <= 0 {
            self.search_command = other.search_command;
        }
        self.searched_at = self.searched_at.or(other.searched_at);
        for grab in other.grabs {
            match self.grab_mut(&grab.download_id) {
                Some(known) => {
                    known.accounted |= grab.accounted;
                    known.client = known.client.take().or(grab.client);
                }
                None => {
                    self.grab_ids.push(grab.download_id.clone());
                    self.grabs.push(grab);
                }
            }
        }
        for id in other.grab_ids {
            if !self.owns_grab(&id) {
                self.grab_ids.push(id);
            }
        }
        union(&mut self.records_to_delete, other.records_to_delete);
        union(&mut self.tracks_taken, other.tracks_taken);
        union(&mut self.songs, other.songs);
        for name in other.holders {
            if !self
                .holders
                .iter()
                .any(|known| dotnet::eq_ignore_case(known.trim(), name.trim()))
            {
                self.holders.push(name);
            }
        }
        union(&mut self.files_handed, other.files_handed);
        for gate in other.gates {
            if !self.gates.iter().any(|known| known.same_person(&gate)) {
                self.gates.push(gate);
            }
        }
        for track in other.held_tracks {
            if !self
                .held_tracks
                .iter()
                .any(|known| known.track_file_id == track.track_file_id)
            {
                self.held_tracks.push(track);
            }
        }
        union(&mut self.passed, other.passed);
        self.writes_audio_tags = self.writes_audio_tags.or(other.writes_audio_tags);
        union(&mut self.research_commands, other.research_commands);
        for item in other.pending_items {
            if !self
                .pending_items
                .iter()
                .any(|known| known.queue_id == item.queue_id)
            {
                self.pending_items.push(item);
            }
        }
        union(&mut self.octo_files, other.octo_files);
        union(&mut self.users_files, other.users_files);
        union(&mut self.replaced_told, other.replaced_told);
        self.completed_download_handling = self
            .completed_download_handling
            .or(other.completed_download_handling);
        union(&mut self.roots, other.roots);
        self.various_artists |= other.various_artists;
        self.first_poll = self.first_poll.or(other.first_poll);
        if self.ended.is_none() {
            self.ended = other.ended;
        }
        self.released |= other.released;
        if self.picked_guid.is_none() {
            self.picked_guid = other.picked_guid;
        }
        union(&mut self.album_hearts, other.album_hearts);
        union(&mut self.filed, other.filed);
        self.replacement |= other.replacement;
    }
}

/// An add Octo was about to send to Lidarr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PendingAdd {
    /// The MusicBrainz artist id the album is added under.
    pub foreign_artist_id: String,
    /// When the add was sent: an artist Lidarr added at or after it is the add's.
    pub posted_at: DateTime<Utc>,
    /// The artists Lidarr had right before the add, by Lidarr's id.
    pub artists_before: Vec<i32>,
}

impl Default for PendingAdd {
    fn default() -> Self {
        PendingAdd {
            foreign_artist_id: String::new(),
            posted_at: Utc::now(),
            artists_before: Vec::new(),
        }
    }
}

/// One person's gate for one song of the album: every file Lidarr imports as that song is judged
/// against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SongGate {
    /// The song, as `provider:id`.
    pub song: String,
    /// The song's catalog title and artist: which imported tracks may be it.
    pub title: String,
    pub artist: String,
    pub gate: HeartGate,
}

impl SongGate {
    /// Whether two gates are one person's for one song.
    pub fn same_person(&self, other: &SongGate) -> bool {
        self.song == other.song && dotnet::eq_ignore_case(self.gate.user.trim(), other.gate.user.trim())
    }

    /// Whether the gate is a heart's or a play's: such a song's file is proven by its own sound
    /// before anything of the album lands.
    pub fn personal(&self) -> bool {
        matches!(
            self.gate.origin,
            AcquisitionOrigin::Heart | AcquisitionOrigin::Play
        )
    }
}

/// One track of Octo's own import, moved into the album's hold folder (its Lidarr record deleted)
/// until a hearted song passes its gate: then it goes on to the library as one of the album's
/// tracks, or is deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HeldTrack {
    pub track_file_id: i32,
    pub track_id: i32,
    /// The album Lidarr imported it into, which may be another album of the artist.
    pub album_id: i32,
    pub title: String,
    pub artist: Option<String>,
    pub number: Option<i32>,
    pub seconds: Option<i32>,
    pub quality: Option<String>,
    /// The recording Lidarr matched it to: it only orders candidates, it never proves one.
    pub recording: Option<String>,
    /// Where it waits, in the hold folder.
    pub path: String,
    /// The download's own file as delivered, when Octo can read it: where its labels come from.
    pub dropped: Option<String>,
    /// The hearted songs whose gate refused it.
    pub refused_for: Vec<String>,
}

impl HeldTrack {
    /// The track as Lidarr listed it, at the path it waits at, with no track file record: Octo
    /// deleted that record when it moved the file.
    pub fn imported(&self) -> LidarrImportedTrack {
        LidarrImportedTrack {
            id: self.track_id,
            title: self.title.clone(),
            track_number: self.number,
            duration_seconds: self.seconds,
            has_file: true,
            path: Some(self.path.clone()),
            size_bytes: 0,
            artist: self.artist.clone(),
            track_file_id: 0,
            quality: self.quality.clone(),
            foreign_recording_id: self.recording.clone(),
        }
    }
}

/// One download of Octo's own grabs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FollowedGrab {
    /// The download client's id for it.
    pub download_id: String,
    /// The name its download client has in Lidarr, recorded from the grab and every queue read.
    pub client: Option<String>,
    /// Lidarr's queue id for it, from the last read that listed it.
    pub queue_id: i32,
    /// Accounted for: Lidarr wrote its download-level row, Octo removed it, its absence was
    /// settled, or Octo's own manual import of it ended.
    pub accounted: bool,
    /// What was left to download at the last play or heart of a song the follow is for.
    pub size_left_at_event: Option<i64>,
    /// The ManualImport Octo posted for it while Lidarr's own imports are off; 0 for none.
    pub manual_import: i32,
}

/// A release Lidarr holds back for the album, followed by its queue id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FollowedItem {
    pub queue_id: i32,
    /// The last read that listed it: a grab of the album after it is the release it became.
    pub last_seen: Option<DateTime<Utc>>,
}

/// Why a follow ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FollowEnd {
    /// Every download of Octo's was accounted for.
    Done,
    /// The search grabbed nothing of Octo's.
    NoGrab,
    /// Lidarr's download failed, was blocked, or left without an import.
    Failed,
    /// The user's own grab or file came into the artist, or another search overlapped Octo's:
    /// Octo stepped aside.
    SteppedAside,
    /// Another source's file for a song of the artist was proven.
    ProvenElsewhere,
    /// A heart on it has a kept copy and Lidarr's download makes no progress.
    DeadDownload,
    /// The person stopped waiting for Lidarr.
    Stopped,
    /// Everyone who asked took their heart back.
    Unhearted,
}

/// A file Octo would have placed under one of Lidarr's root folders for a followed artist,
/// waiting in the one hold path until no follow on the artist is open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HeldPlacement {
    /// The song whose file it is, as `provider:id`.
    pub song: String,
    /// Where the file waits and plays from.
    pub held_path: String,
    /// Where the library's layout files it.
    pub target: String,
    /// A copy kept as heard or kept by the person, which no sound proved.
    pub kept: bool,
    /// The follows it waits for, by foreign album id.
    pub follows: Vec<String>,
    /// The people whose heart or play it was fetched or kept for: who may stop waiting for it,
    /// and whose downloads list says it waits.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<String>,
}

/// What asking to stop waiting for Lidarr came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopWaiting {
    /// The follows holding the song's kept copy end, and the copy is placed.
    Stopped,
    /// No kept copy of the song waits for Lidarr.
    NothingWaits,
}

/// How many download ids of forgotten entries are still known as Octo's.
const FINISHED_KEPT: usize = 500;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PendingFile {
    albums: Vec<PendingAlbum>,
    held: Vec<HeldPlacement>,
    finished_grabs: Vec<String>,
}

#[derive(Default)]
struct Listed {
    albums: IndexMap<String, PendingAlbum>,
    held: Vec<HeldPlacement>,
    /// The download ids of entries Octo forgot, newest last: Lidarr's queue lists a finished
    /// download until its next refresh, and it is still Octo's own, never a grab of the user's.
    finished_grabs: Vec<String>,
}

/// The listed albums, by foreign album id ignoring case, saved on every change; the files held
/// for their follows; and which follows run now, each with what wakes it.
pub struct LidarrPendingAlbums {
    path: Option<PathBuf>,
    listed: Mutex<Listed>,
    running: Mutex<HashMap<String, Arc<Notify>>>,
}

fn key(foreign_album_id: &str) -> String {
    dotnet::ordinal_ignore_case_key(foreign_album_id)
}

/// The name a song's artist and a followed artist are compared by.
fn artist_key(name: &str) -> String {
    dotnet::to_lower_invariant(name.trim())
}

impl LidarrPendingAlbums {
    /// The list over its file; None keeps it in memory only.
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut listed = Listed::default();
        if let Some(path) = &path {
            match state_file::read_text(path) {
                Ok(Some(text)) => match serde_json::from_str::<PendingFile>(&text) {
                    Ok(file) => {
                        for album in file.albums {
                            if !album.foreign_album_id.trim().is_empty() {
                                listed.albums.insert(key(&album.foreign_album_id), album);
                            }
                        }
                        listed.held = file.held;
                        listed.finished_grabs = file.finished_grabs;
                    }
                    Err(e) => warn!(
                        "Could not read {}: {e}; starting with no pending Lidarr albums",
                        path.display()
                    ),
                },
                Ok(None) => {}
                Err(e) => warn!(
                    "Could not read {}: {e}; starting with no pending Lidarr albums",
                    path.display()
                ),
            }
        }
        LidarrPendingAlbums {
            path,
            listed: Mutex::new(listed),
            running: Mutex::new(HashMap::new()),
        }
    }

    /// Lists an album, merged into what was listed for it (see [`PendingAlbum::merge`]).
    pub fn remember(&self, album: PendingAlbum) {
        let mut listed = self.listed.lock();
        match listed.albums.get_mut(&key(&album.foreign_album_id)) {
            Some(known) => known.merge(album),
            None => {
                listed.albums.insert(key(&album.foreign_album_id), album);
            }
        }
        self.save(&listed);
    }

    /// The listed album, however long ago it was asked for: an entry ends only when its follow or
    /// its put-back forgets it.
    pub fn get(&self, foreign_album_id: &str) -> Option<PendingAlbum> {
        self.listed.lock().albums.get(&key(foreign_album_id)).cloned()
    }

    /// Changes the listed album in place and saves; false when it is not listed.
    pub fn update(&self, foreign_album_id: &str, change: impl FnOnce(&mut PendingAlbum)) -> bool {
        let mut listed = self.listed.lock();
        let Some(album) = listed.albums.get_mut(&key(foreign_album_id)) else {
            return false;
        };
        change(album);
        self.save(&listed);
        true
    }

    /// Every listed album, however long ago it was asked for.
    pub fn entries(&self) -> Vec<PendingAlbum> {
        self.listed.lock().albums.values().cloned().collect()
    }

    /// Whether a download id is one of Octo's own grabs, on any listed album or of an entry Octo
    /// forgot lately.
    pub fn is_octos_grab(&self, download_id: &str) -> bool {
        let listed = self.listed.lock();
        listed.albums.values().any(|album| album.owns_grab(download_id))
            || listed
                .finished_grabs
                .iter()
                .any(|id| dotnet::eq_ignore_case(id, download_id))
    }

    /// Whether a grab row of `album_id` from a search at `date` falls to a follow of Octo's other
    /// than `except` that searched that album and has not read it yet.
    pub fn another_follows(&self, album_id: i32, date: DateTime<Utc>, except: &str) -> bool {
        self.listed.lock().albums.values().any(|album| {
            album.lidarr_album_id == album_id
                && !dotnet::eq_ignore_case(&album.foreign_album_id, except)
                && album.following()
                && album.searched_at.is_some_and(|searched| searched <= date)
        })
    }

    /// Whether any listed album other than `except` is under this Lidarr artist.
    pub fn uses_artist(&self, artist_id: i32, except: &str) -> bool {
        artist_id > 0
            && self.listed.lock().albums.values().any(|album| {
                album.lidarr_artist_id == artist_id
                    && !dotnet::eq_ignore_case(&album.foreign_album_id, except)
            })
    }

    /// Takes an album off the list. Its download ids stay known as Octo's.
    pub fn forget(&self, foreign_album_id: &str) {
        let mut listed = self.listed.lock();
        if let Some(album) = listed.albums.shift_remove(&key(foreign_album_id)) {
            for id in album.grab_ids {
                if !listed
                    .finished_grabs
                    .iter()
                    .any(|known| dotnet::eq_ignore_case(known, &id))
                {
                    listed.finished_grabs.push(id);
                }
            }
            let over = listed.finished_grabs.len().saturating_sub(FINISHED_KEPT);
            listed.finished_grabs.drain(..over);
            self.save(&listed);
        }
    }

    // ---- Follows that run now ----------------------------------------------------------------

    /// A follow of the album starts running: what wakes it. None when one runs already.
    pub fn start_running(&self, foreign_album_id: &str) -> Option<Arc<Notify>> {
        let mut running = self.running.lock();
        if running.contains_key(&key(foreign_album_id)) {
            return None;
        }
        let wake = Arc::new(Notify::new());
        running.insert(key(foreign_album_id), wake.clone());
        Some(wake)
    }

    /// The follow of the album stopped running.
    pub fn stop_running(&self, foreign_album_id: &str) {
        self.running.lock().remove(&key(foreign_album_id));
    }

    /// Whether a follow of the album runs now.
    pub fn is_running(&self, foreign_album_id: &str) -> bool {
        self.running.lock().contains_key(&key(foreign_album_id))
    }

    /// Wakes the running follow of the album, to read Lidarr now.
    pub fn wake(&self, foreign_album_id: &str) {
        if let Some(wake) = self.running.lock().get(&key(foreign_album_id)) {
            wake.notify_one();
        }
    }

    /// Asks the follow of the album to end, for `why`: true when it was open. Its running follow
    /// removes Octo's items and lets the files it held go.
    pub fn request_end(&self, foreign_album_id: &str, why: FollowEnd) -> bool {
        let ended = {
            let mut listed = self.listed.lock();
            let Some(album) = listed.albums.get_mut(&key(foreign_album_id)) else {
                return false;
            };
            if !album.following() {
                return false;
            }
            album.ended = Some(why);
            self.save(&listed);
            true
        };
        self.wake(foreign_album_id);
        ended
    }

    /// `user` took their heart on `song` back: they leave every open follow of it (their gate for
    /// that song goes, and they stop holding the album unless their gate for another of its songs
    /// is still there), and a follow nobody's heart is on any more is asked to end. The follows
    /// asked to end.
    pub fn unhearted(&self, song: &str, user: &str) -> Vec<String> {
        let ending: Vec<String> = {
            let mut listed = self.listed.lock();
            let mut ending = Vec::new();
            for album in listed.albums.values_mut() {
                if !album.following() || !album.songs.iter().any(|known| known == song) {
                    continue;
                }
                let before = album.holders.len();
                let theirs = |gate: &SongGate| dotnet::eq_ignore_case(gate.gate.user.trim(), user.trim());
                album.gates.retain(|gate| !(gate.song == song && theirs(gate)));
                if !album
                    .gates
                    .iter()
                    .any(|gate| theirs(gate) && gate.gate.origin == AcquisitionOrigin::Heart)
                {
                    album
                        .holders
                        .retain(|holder| !dotnet::eq_ignore_case(holder.trim(), user.trim()));
                }
                if before > 0 && album.holders.is_empty() {
                    album.ended = Some(FollowEnd::Unhearted);
                    ending.push(album.foreign_album_id.clone());
                }
            }
            self.save(&listed);
            ending
        };
        for follow in &ending {
            self.wake(follow);
        }
        ending
    }

    /// The open follows a song belongs to, by foreign album id.
    pub fn follows_of_song(&self, song: &str) -> Vec<String> {
        self.listed
            .lock()
            .albums
            .values()
            .filter(|album| album.following())
            .filter(|album| album.songs.iter().any(|known| known == song))
            .map(|album| album.foreign_album_id.clone())
            .collect()
    }

    // ---- Files held while a follow is open -----------------------------------------------------

    /// The open follows a file Octo would place at `target` waits for: those on an artist named in
    /// `artists` (a Various Artists album's by the song's `album` title instead) whose Lidarr root
    /// folders hold `target`.
    pub fn covering(&self, artists: &[String], album: &str, target: &str) -> Vec<String> {
        let full = get_full_path(target);
        let wanted: Vec<String> = artists
            .iter()
            .filter(|artist| !dotnet::is_blank(artist))
            .map(|artist| artist_key(artist))
            .collect();
        self.listed
            .lock()
            .albums
            .values()
            .filter(|entry| entry.following())
            .filter(|entry| {
                if entry.various_artists {
                    dotnet::eq_ignore_case(entry.title.trim(), album.trim())
                } else {
                    wanted.contains(&artist_key(&entry.artist))
                }
            })
            .filter(|entry| {
                entry.roots.iter().any(|root| {
                    get_full_path(root).trim_end_matches('/') == full.trim_end_matches('/')
                        || is_inside(&full, root)
                })
            })
            .map(|entry| entry.foreign_album_id.clone())
            .collect()
    }

    /// Holds a file for its follows, saved, and wakes them.
    pub fn hold_placement(&self, held: HeldPlacement) {
        let follows = held.follows.clone();
        {
            let mut listed = self.listed.lock();
            listed.held.retain(|known| known.held_path != held.held_path);
            listed.held.push(held);
            self.save(&listed);
        }
        for follow in follows {
            self.wake(&follow);
        }
    }

    /// Every file held now.
    pub fn held(&self) -> Vec<HeldPlacement> {
        self.listed.lock().held.clone()
    }

    /// Whether a kept copy of one of the follow's songs waits for it.
    pub fn kept_waits(&self, foreign_album_id: &str) -> bool {
        let listed = self.listed.lock();
        let Some(entry) = listed.albums.get(&key(foreign_album_id)) else {
            return false;
        };
        listed.held.iter().any(|held| {
            held.kept
                && held
                    .follows
                    .iter()
                    .any(|follow| dotnet::eq_ignore_case(follow, foreign_album_id))
                && entry.songs.contains(&held.song)
        })
    }

    /// Whether a kept copy of the song waits for an open follow.
    pub fn kept_copy_waits(&self, song: &str) -> bool {
        !self.kept_copies_waiting(song).is_empty()
    }

    /// The kept copies of the song that wait for an open follow.
    pub fn kept_copies_waiting(&self, song: &str) -> Vec<HeldPlacement> {
        let listed = self.listed.lock();
        listed
            .held
            .iter()
            .filter(|held| {
                held.kept
                    && held.song == song
                    && held.follows.iter().any(|follow| {
                        listed
                            .albums
                            .get(&key(follow))
                            .is_some_and(PendingAlbum::following)
                    })
            })
            .cloned()
            .collect()
    }

    /// The follow lets go of what it held: each file no other open follow holds is taken off the
    /// list and answered, to be placed now.
    pub fn release(&self, foreign_album_id: &str) -> Vec<HeldPlacement> {
        let mut listed = self.listed.lock();
        let open: Vec<String> = listed
            .albums
            .values()
            .filter(|album| album.following())
            .map(|album| key(&album.foreign_album_id))
            .collect();
        let mut free = Vec::new();
        let mut kept = Vec::new();
        for mut held in std::mem::take(&mut listed.held) {
            held.follows
                .retain(|follow| !dotnet::eq_ignore_case(follow, foreign_album_id));
            held.follows.retain(|follow| open.contains(&key(follow)));
            if held.follows.is_empty() {
                free.push(held);
            } else {
                kept.push(held);
            }
        }
        listed.held = kept;
        self.save(&listed);
        free
    }

    /// Puts a held file back on the list, when placing it failed.
    pub fn hold_again(&self, held: HeldPlacement) {
        let mut listed = self.listed.lock();
        listed.held.retain(|known| known.held_path != held.held_path);
        listed.held.push(held);
        self.save(&listed);
    }

    /// Takes the file held at `held_path` off the list, without placing it: a proven copy of its
    /// song replaced it.
    pub fn unhold(&self, held_path: &str) {
        let mut listed = self.listed.lock();
        listed.held.retain(|known| known.held_path != held_path);
        self.save(&listed);
    }

    fn save(&self, listed: &Listed) {
        let Some(path) = &self.path else {
            return;
        };
        let file = PendingFile {
            albums: listed.albums.values().cloned().collect(),
            held: listed.held.clone(),
            finished_grabs: listed.finished_grabs.clone(),
        };
        let written = serde_json::to_string_pretty(&file)
            .map_err(std::io::Error::other)
            .and_then(|text| state_file::save_atomic(path, &text));
        if let Err(e) = written {
            warn!("Could not save {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use octo_core::fingerprint::heart_gate::HeardReading;

    use super::*;

    fn album(id: &str, asked_at: DateTime<Utc>) -> PendingAlbum {
        PendingAlbum {
            foreign_album_id: id.into(),
            lidarr_album_id: 7,
            artist: "Massive Attack".into(),
            title: "Mezzanine".into(),
            files_before: vec![3],
            requested_by: vec!["alice".into()],
            asked_at,
            ..Default::default()
        }
    }

    fn heart_gate(user: &str) -> HeartGate {
        HeartGate {
            user: user.into(),
            origin: AcquisitionOrigin::Heart,
            r: None,
            heard: None,
            heard_seconds: None,
            reading: HeardReading::NotHeard,
            stand_in: None,
        }
    }

    fn song_gate(song: &str, user: &str) -> SongGate {
        SongGate {
            song: song.into(),
            title: "Teardrop".into(),
            artist: "Massive Attack".into(),
            gate: heart_gate(user),
        }
    }

    fn gate(user: &str) -> SongGate {
        song_gate("soulseek:teardrop", user)
    }

    #[test]
    fn a_pending_album_survives_a_restart_and_is_found_ignoring_case() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        LidarrPendingAlbums::new(Some(path.clone())).remember(album("rg-ABC", Utc::now()));

        let reloaded = LidarrPendingAlbums::new(Some(path.clone()));
        let found = reloaded.get("rg-abc").expect("listed");
        assert_eq!(found.lidarr_album_id, 7);
        assert_eq!(found.files_before, [3]);
        assert_eq!(found.requested_by, ["alice"]);

        reloaded.forget("RG-abc");
        assert!(LidarrPendingAlbums::new(Some(path)).get("rg-abc").is_none());
    }

    /// An entry ends only on work, never on its age: one asked for 44 days ago, holding a gate,
    /// put-back facts, a search or nothing at all, is listed across a restart until its follow or
    /// its put-back forgets it.
    #[test]
    fn an_entry_holding_a_gate_or_put_back_facts_outlives_fourteen_days() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        let pending = LidarrPendingAlbums::new(Some(path.clone()));
        let old = Utc::now() - TimeDelta::days(44);
        pending.remember(PendingAlbum {
            gates: vec![gate("alice")],
            ..album("gated", old)
        });
        pending.remember(PendingAlbum {
            album_added: true,
            ..album("added", old)
        });
        pending.remember(PendingAlbum {
            search_command: 12,
            ..album("searched", old)
        });
        pending.remember(album("plain", old));
        // Another save, which used to drop what was old, drops nothing.
        pending.remember(album("new", Utc::now()));

        let reloaded = LidarrPendingAlbums::new(Some(path));
        for id in ["gated", "added", "searched", "plain", "new"] {
            assert!(reloaded.get(id).is_some(), "{id} is still listed");
        }
        reloaded.forget("plain");
        assert!(reloaded.get("plain").is_none(), "work ends it");
    }

    /// One person hearting two songs of one album keeps a gate for each, and taking one heart
    /// back leaves the other's gate and keeps them holding the album.
    #[test]
    fn one_person_keeps_a_gate_per_song_and_unhearting_one_keeps_the_other() {
        let pending = LidarrPendingAlbums::new(None);
        pending.remember(PendingAlbum {
            search_command: 3,
            songs: vec!["soulseek:teardrop".into(), "soulseek:angel".into()],
            holders: vec!["alice".into()],
            gates: vec![gate("alice")],
            ..album("rg-abc", Utc::now())
        });
        pending.remember(PendingAlbum {
            gates: vec![song_gate("soulseek:angel", "ALICE"), gate("alice")],
            ..PendingAlbum {
                foreign_album_id: "rg-abc".into(),
                ..Default::default()
            }
        });
        let entry = pending.get("rg-abc").expect("listed");
        assert_eq!(entry.gates.len(), 2);
        assert_eq!(entry.gates_of("soulseek:angel").len(), 1);

        assert!(pending.unhearted("soulseek:teardrop", "alice").is_empty());
        let entry = pending.get("rg-abc").expect("listed");
        assert!(entry.gates_of("soulseek:teardrop").is_empty());
        assert_eq!(entry.gates_of("soulseek:angel").len(), 1);
        assert_eq!(entry.holders, ["alice"]);
        assert!(entry.following());

        assert_eq!(pending.unhearted("soulseek:angel", "alice"), ["rg-abc"]);
        assert_eq!(
            pending.get("rg-abc").expect("listed").ended,
            Some(FollowEnd::Unhearted)
        );
    }

    /// The tracks held for an album and the songs that passed survive a restart and merge.
    #[test]
    fn held_tracks_and_passed_songs_survive_a_restart_and_merge() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        let pending = LidarrPendingAlbums::new(Some(path.clone()));
        let held = |file: i32, title: &str| HeldTrack {
            track_file_id: file,
            title: title.into(),
            path: format!("/music/.octo-incoming/lidarr-held/rg-abc/{title}.flac"),
            ..Default::default()
        };
        pending.remember(PendingAlbum {
            held_tracks: vec![held(100, "Angel")],
            ..album("rg-abc", Utc::now())
        });
        pending.remember(PendingAlbum {
            held_tracks: vec![held(100, "Angel"), held(101, "Teardrop")],
            passed: vec!["soulseek:teardrop".into()],
            writes_audio_tags: Some(false),
            ..PendingAlbum {
                foreign_album_id: "RG-ABC".into(),
                ..Default::default()
            }
        });

        let entry = LidarrPendingAlbums::new(Some(path))
            .get("rg-abc")
            .expect("listed");
        assert_eq!(
            entry
                .held_tracks
                .iter()
                .map(|track| track.track_file_id)
                .collect::<Vec<_>>(),
            [100, 101]
        );
        assert_eq!(entry.passed, ["soulseek:teardrop"]);
        assert_eq!(entry.writes_audio_tags, Some(false));
        let teardrop = entry.held_tracks[1].imported();
        assert_eq!(teardrop.path.as_deref(), Some(entry.held_tracks[1].path.as_str()));
        assert_eq!(teardrop.track_file_id, 0, "its record went when it was moved");
    }

    /// Everything a pick saves before and around its add, its pin and its search survives a
    /// restart, and a file written before those were saved still reads.
    #[test]
    fn a_picks_add_pin_and_search_survive_a_restart() {
        use octo_core::lidarr::lidarr_client::LidarrReleaseMonitored;

        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        let choice = |monitored: bool| LidarrReleaseChoice {
            any_release_ok: monitored,
            releases: vec![LidarrReleaseMonitored { id: 70, monitored }],
        };
        let posted_at = Utc::now() - TimeDelta::minutes(1);
        let pending = LidarrPendingAlbums::new(Some(path.clone()));
        pending.remember(PendingAlbum {
            add: Some(PendingAdd {
                foreign_artist_id: "ma".into(),
                posted_at,
                artists_before: vec![2, 3],
            }),
            ..album("rg-abc", Utc::now())
        });
        assert!(pending.update("RG-ABC", |entry| {
            entry.album_added = true;
            entry.artist_added = 5;
            entry.lidarr_artist_id = 5;
            entry.release_choice = Some(choice(true));
            entry.pin = Some(choice(false));
            entry.search_command = 12;
            entry.grab_ids.push("DL-1".into());
        }));
        assert!(!pending.update("missing", |_| {}));

        let reloaded = LidarrPendingAlbums::new(Some(path.clone()));
        let found = reloaded.get("rg-abc").expect("listed");
        assert_eq!(
            found.add.as_ref().map(|add| add.artists_before.clone()),
            Some(vec![2, 3])
        );
        assert_eq!(found.add.as_ref().map(|add| add.posted_at), Some(posted_at));
        assert!(found.album_added && found.searched());
        assert_eq!((found.artist_added, found.search_command), (5, 12));
        assert_eq!(found.release_choice, Some(choice(true)));
        assert_eq!(found.pin, Some(choice(false)));
        assert!(reloaded.is_octos_grab("dl-1"));
        assert!(!reloaded.is_octos_grab("DL-2"));
        assert!(reloaded.uses_artist(5, "other"));
        assert!(!reloaded.uses_artist(5, "RG-ABC"));
        assert_eq!(reloaded.entries().len(), 1);

        std::fs::write(
            &path,
            r#"{"Albums":[{"ForeignAlbumId":"rg-old","LidarrAlbumId":7,"FilesBefore":[3]}]}"#,
        )
        .expect("written");
        let old = LidarrPendingAlbums::new(Some(path)).entries();
        assert_eq!(old.len(), 1);
        assert_eq!((old[0].add.clone(), old[0].searched()), (None, false));
    }

    /// A second heart's save of the same album keeps the first one's gate and put-back facts,
    /// adds its own person, gate and song, and loses no download of Octo's.
    #[test]
    fn a_second_hearts_remember_keeps_the_first_ones_gate_and_put_back_facts() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        let pending = LidarrPendingAlbums::new(Some(path.clone()));
        let mut first = PendingAlbum {
            songs: vec!["soulseek:teardrop".into()],
            gates: vec![gate("alice")],
            album_added: true,
            artist_added: 5,
            add: Some(PendingAdd {
                foreign_artist_id: "ma".into(),
                ..Default::default()
            }),
            search_command: 12,
            tracks_before: vec![1],
            ..album("rg-abc", Utc::now() - TimeDelta::minutes(5))
        };
        first.record_grab("dl-1", Some("qBittorrent"));
        pending.remember(first);

        let mut alices_again = gate("alice");
        alices_again.gate.reading = HeardReading::AnotherSong;
        pending.remember(PendingAlbum {
            songs: vec!["soulseek:angel".into(), "soulseek:teardrop".into()],
            gates: vec![alices_again, gate("bob")],
            requested_by: vec!["Bob".into(), "ALICE".into()],
            tracks_before: vec![3],
            ..PendingAlbum {
                foreign_album_id: "RG-ABC".into(),
                ..Default::default()
            }
        });

        let merged = LidarrPendingAlbums::new(Some(path))
            .get("rg-abc")
            .expect("listed");
        assert_eq!(
            merged
                .gates
                .iter()
                .map(|gate| (gate.gate.user.as_str(), gate.gate.reading.clone()))
                .collect::<Vec<_>>(),
            [("alice", HeardReading::NotHeard), ("bob", HeardReading::NotHeard)]
        );
        assert_eq!(merged.songs, ["soulseek:teardrop", "soulseek:angel"]);
        assert_eq!(merged.requested_by, ["alice", "Bob"]);
        assert!(merged.album_added);
        assert_eq!((merged.artist_added, merged.search_command), (5, 12));
        assert!(merged.add.is_some());
        assert_eq!(merged.tracks_before, [1, 3]);
        assert_eq!(merged.files_before, [3]);
        assert!(merged.owns_grab("DL-1"));
        assert_eq!(merged.lidarr_album_id, 7);
    }

    /// A file held for two follows on one artist is let go only once both have ended.
    #[test]
    fn a_held_file_waits_for_every_open_follow_on_its_artist() {
        let pending = LidarrPendingAlbums::new(None);
        for id in ["rg-1", "rg-2"] {
            pending.remember(PendingAlbum {
                search_command: 3,
                songs: vec!["soulseek:angel".into()],
                roots: vec!["/music".into()],
                ..album(id, Utc::now())
            });
        }
        let covering = pending.covering(
            &["massive attack".to_string()],
            "Mezzanine",
            "/music/Massive Attack/Mezzanine/01 - Angel.flac",
        );
        assert_eq!(covering, ["rg-1", "rg-2"]);
        assert!(
            pending
                .covering(&["Portishead".to_string()], "Dummy", "/music/x.flac")
                .is_empty()
        );
        assert!(
            pending
                .covering(&["Massive Attack".to_string()], "Mezzanine", "/elsewhere/x.flac")
                .is_empty()
        );
        pending.hold_placement(HeldPlacement {
            song: "soulseek:angel".into(),
            held_path: "/music/.octo-incoming/held/a.flac".into(),
            target: "/music/Massive Attack/Mezzanine/01 - Angel.flac".into(),
            kept: true,
            follows: covering,
            users: vec!["alice".into()],
        });
        assert!(pending.kept_waits("rg-1"));
        assert!(pending.kept_copy_waits("soulseek:angel"));
        assert_eq!(pending.kept_copies_waiting("soulseek:angel")[0].users, ["alice"]);

        assert!(pending.request_end("rg-1", FollowEnd::Stopped));
        assert!(!pending.request_end("rg-1", FollowEnd::Stopped));
        assert!(pending.release("rg-1").is_empty());
        assert!(pending.request_end("rg-2", FollowEnd::Done));
        let free = pending.release("rg-2");
        assert_eq!(free.len(), 1);
        assert!(pending.held().is_empty());
        assert!(!pending.kept_copy_waits("soulseek:angel"));
    }

    /// A Various Artists pick holds a file by its album title: the song's own artist is another.
    #[test]
    fn a_various_artists_follow_holds_a_file_by_its_album_title() {
        let pending = LidarrPendingAlbums::new(None);
        pending.remember(PendingAlbum {
            artist: "Various Artists".into(),
            title: "Live Aid".into(),
            various_artists: true,
            search_command: 3,
            roots: vec!["/music".into()],
            ..album("rg-live-aid", Utc::now())
        });

        assert_eq!(
            pending.covering(
                &["Phil Collins".to_string()],
                "Live Aid",
                "/music/Phil Collins/Live Aid/01 - Against All Odds.flac",
            ),
            ["rg-live-aid"]
        );
        assert!(
            pending
                .covering(
                    &["Phil Collins".to_string()],
                    "Face Value",
                    "/music/Phil Collins/x.flac"
                )
                .is_empty()
        );
    }

    /// Lidarr's queue lists a finished download until its next refresh: once its entry is
    /// forgotten, its id is still Octo's, across a restart, so the next pick never reads Octo's
    /// own download as a grab of the user's.
    #[test]
    fn a_forgotten_entrys_downloads_stay_octos() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        let pending = LidarrPendingAlbums::new(Some(path.clone()));
        let mut entry = album("rg-abc", Utc::now());
        entry.record_grab("DL-1", Some("qBittorrent"));
        pending.remember(entry);

        pending.forget("rg-abc");

        let reloaded = LidarrPendingAlbums::new(Some(path));
        assert!(reloaded.get("rg-abc").is_none());
        assert!(reloaded.is_octos_grab("dl-1"));
        assert!(!reloaded.is_octos_grab("dl-2"));
    }

    #[test]
    fn an_unreadable_file_starts_empty() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lidarr-pending.json");
        std::fs::write(&path, "{ not json").expect("written");

        assert!(LidarrPendingAlbums::new(Some(path)).get("anything").is_none());
    }
}
