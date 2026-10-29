//! What a heart or a play heard, and the shapes its gate works in: the source that played, the
//! album a person listened from, the recording they heard (R), why an acquisition runs, what a held
//! acquisition waits for, the readings the gate takes (the heard sample's AcoustID answer, the
//! catalog's clip, the heard master) and its verdict on one downloaded file.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fingerprint::acoust_id_client::{AcoustIdOutcome, AnswerRecording};
use crate::fingerprint::verification::WeighedRecording;

/// The client an album context opened through Navidrome's own API is filed under: that API sends
/// no client name.
pub const NATIVE_CLIENT: &str = "navidrome-native";

/// Why an acquisition runs, which decides how its downloaded file is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AcquisitionOrigin {
    /// A person hearted the song.
    Heart,
    /// A person hearted an album and this is one of its tracks, or a member's song heart fetches
    /// the rest of its album.
    AlbumHeart,
    /// A play asked for a permanent copy: Download on play, Lidarr album on play or Wait for
    /// lossless.
    Play,
    /// The import trickle.
    Import,
    /// A family member's request.
    Request,
    /// A library action's replacement: Better quality, Wrong song, Wrong version or an upgrade.
    Replacement,
    /// A copy the person picked in Find songs.
    SongFinderPick,
}

impl AcquisitionOrigin {
    /// Every origin, in declaration order.
    pub const ALL: [AcquisitionOrigin; 7] = [
        Self::Heart,
        Self::AlbumHeart,
        Self::Play,
        Self::Import,
        Self::Request,
        Self::Replacement,
        Self::SongFinderPick,
    ];

    /// The origin's name, as the hold file stores it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Heart => "Heart",
            Self::AlbumHeart => "AlbumHeart",
            Self::Play => "Play",
            Self::Import => "Import",
            Self::Request => "Request",
            Self::Replacement => "Replacement",
            Self::SongFinderPick => "SongFinderPick",
        }
    }
}

/// What a held acquisition waits for. Each wait ends on work (an answer, an event, a proof or the
/// person's own action), never on a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WaitingFor {
    /// MusicBrainz has not answered a question the heart needs.
    MusicBrainz,
    /// Lidarr's indexers are failing.
    LidarrIndexers,
    /// AcoustID has not answered for the heard sample or for a held file.
    AcoustId,
    /// AcoustID is off and the catalog's clip could not settle what played; the person may keep
    /// the heard audio anyway.
    AcoustIdOff,
    /// Deezer has not answered for the catalog's clip.
    Deezer,
    /// The yt-dlp shim has not answered for the heard video.
    Shim,
    /// Nothing could be proven yet; a play of the song, a proof or un-hearting ends it.
    NotYetProven,
}

impl WaitingFor {
    /// Every wait, in declaration order.
    pub const ALL: [WaitingFor; 7] = [
        Self::MusicBrainz,
        Self::LidarrIndexers,
        Self::AcoustId,
        Self::AcoustIdOff,
        Self::Deezer,
        Self::Shim,
        Self::NotYetProven,
    ];

    /// The wait's name, as a hold's key and the hold file carry it.
    pub fn name(self) -> &'static str {
        match self {
            Self::MusicBrainz => "MusicBrainz",
            Self::LidarrIndexers => "LidarrIndexers",
            Self::AcoustId => "AcoustId",
            Self::AcoustIdOff => "AcoustIdOff",
            Self::Deezer => "Deezer",
            Self::Shim => "Shim",
            Self::NotYetProven => "NotYetProven",
        }
    }
}

/// The exact source that played for one person, which a file must be to count as the very audio
/// they heard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all_fields = "PascalCase")]
pub enum HeardSource {
    /// A prefetched copy: the prefetch store's entry under `key` as it was when it played. A later
    /// fetch under the same key is another copy.
    Copy {
        key: String,
        #[serde(
            default,
            with = "crate::json::datetime::utc_option",
            skip_serializing_if = "Option::is_none"
        )]
        ready_at: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_file: Option<String>,
    },
    /// A direct stream of one video: the video that played, the byte size the played
    /// Content-Length or Content-Range confirmed, its content type, and its format when known.
    Video {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        content_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        itag: Option<u32>,
    },
    /// A library file served for an outside id.
    Library { path: String },
    /// The lossless file a play of an acquired song served.
    Acquired { path: String },
}

impl HeardSource {
    /// The video that played, for a direct stream.
    pub fn video_id(&self) -> Option<&str> {
        match self {
            Self::Video { id, .. } => Some(id.as_str()),
            _ => None,
        }
    }
}

/// One person's heard source of a song: when it was first served to them, what it was, and the
/// heard-sample store's id for its prints once they were taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HeardRecord {
    #[serde(with = "crate::json::datetime::utc")]
    pub heard_at: DateTime<Utc>,
    pub source: HeardSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_id: Option<String>,
}

/// A heard record with the person it belongs to, as a routing keeps one per person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HeardBy {
    pub user: String,
    pub record: HeardRecord,
}

/// Which of a person's album contexts a stream marks played: the most recent one listed by the
/// same client, or the one whose catalog album id is the stream's album.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayedMark {
    pub client: Option<String>,
    pub catalog_album_id: Option<String>,
}

/// One album a person listed or played a song from: which catalog album, where the song sits on
/// it, and when they listed and played it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct AlbumContext {
    pub user: String,
    /// The client that listed it (Subsonic's `c`), or [`NATIVE_CLIENT`].
    pub client: Option<String>,
    /// The catalog's own album id, never sent to a client.
    pub catalog_album_id: String,
    pub album: Option<String>,
    pub disc: Option<i32>,
    pub track: Option<i32>,
    pub total: Option<i32>,
    pub isrc: Option<String>,
    /// Explicit (1), the clean edit (3) or neither (0), as the routing stores it.
    pub explicit: Option<i32>,
    #[serde(with = "crate::json::datetime::utc")]
    pub listed_at: DateTime<Utc>,
    #[serde(
        with = "crate::json::datetime::utc_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub played_at: Option<DateTime<Utc>>,
}

/// R, the recording a person heard: a set of MusicBrainz recording ids, and what reading an
/// AcoustID answer against it needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HeardRecording {
    /// The recording ids, the heard track's first, at most six.
    pub ids: Vec<String>,
    /// Every recording MusicBrainz files under R's code with R's note, uncapped: what a
    /// `MUSICBRAINZ_TRACKID` label is held to.
    pub duplicates: Vec<String>,
    /// The ISRCs that name R: the heard catalog track's own, or the heard recording's once R
    /// became it.
    pub isrcs: Vec<String>,
    /// The song's title and artist as the routing stores them, which say what "the same song" is
    /// when an answer names none of R's recordings.
    pub title: String,
    pub artist: String,
    /// The MusicBrainz artist ids R's recordings carry.
    pub artist_ids: Vec<String>,
    /// R was built from a dominant recording whose reading was not clear, so it may be another mix
    /// than the one heard, and a clear reading against it never accepts a file.
    pub from_unclear: bool,
}

impl HeardRecording {
    /// Whether `id` is one of R's recordings, ignoring case.
    pub fn holds(&self, id: &str) -> bool {
        self.ids.iter().any(|known| known.eq_ignore_ascii_case(id))
    }
}

/// A heart's stand-in when it was never played: the pinned video, fetched only to help find R and
/// never counted as heard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct StandIn {
    pub video_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_id: Option<String>,
    /// Its print is what a download's print is compared with.
    pub is_reference: bool,
}

/// What the catalog's clip check read of the heard audio, with AcoustID off or unable to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipReading {
    /// The catalog's clip sounds like the heard audio, and the heard video's channel names the
    /// song's artist.
    ThisSong,
    /// The clip reads as another song or another take of this one.
    AnotherSongOrTake,
    /// Between the two lines, a heard print that does not reach the clip, or a channel that does
    /// not name the artist.
    Unclear,
    /// Deezer holds no usable clip of this song.
    NoClip,
    /// Deezer or the shim did not answer; the heart holds for it.
    Busy(WaitingFor),
}

/// The heard-master check's verdict on one file: whether it is the very master that played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeardMaster {
    /// Window by window, both ways, the file is the master that played.
    Same,
    /// The file is not provably that master: another master, another mix or an edit.
    Differs,
    /// A decode failed or ran out of time: undecided, never accepted.
    Undecided,
    /// The shim did not answer for the heard audio; the heart holds for it.
    Busy,
    /// The heard video is gone, so nothing can be compared with it.
    HeardGone,
}

/// What the heard audio says about R, read once before any download is judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all_fields = "PascalCase")]
pub enum HeardReading {
    /// Nothing was heard: a heart never played, or a play with no playable source.
    NotHeard,
    /// AcoustID has not answered for the heard sample yet.
    Busy,
    /// The stream stood in with another song: nothing it played is kept, and its video is refused
    /// for the song.
    AnotherSong,
    /// R stands as the catalog gave it. `tag` is the recording a file kept as heard may be tagged
    /// with: the heard answer's dominant recording when the answer reads clear against R.
    RStays {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },
    /// R became the heard answer's dominant recording and its same-ISRC duplicates. `tag` is that
    /// recording when its own reading allows its recording tags.
    RBecame {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },
    /// Nothing proves which recording played: only the heard audio itself or the heard master may
    /// be kept.
    OnlyHeardOrMaster,
    /// AcoustID is off or could not read the heard sample, so the catalog's clip decided.
    Clip(ClipReading),
}

/// One heart's or play's gate: whose it is, why it runs, R, what the person heard and what that
/// audio said. Every file a heart or a play downloads is judged against its own gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HeartGate {
    pub user: String,
    pub origin: AcquisitionOrigin,
    /// R. None when neither the catalog nor an answer gave an identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r: Option<HeardRecording>,
    /// What this person heard of the song.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heard: Option<HeardRecord>,
    /// The song's played length in whole seconds, which a lookup of the heard audio sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heard_seconds: Option<i32>,
    pub reading: HeardReading,
    /// A never-played heart's stand-in, when one was fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stand_in: Option<StandIn>,
}

/// How a file was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    /// The very audio that played, by identity.
    HeardAudio,
    /// AcoustID's clear reading alone.
    Sound,
    /// The master that played, window by window, both ways.
    HeardMaster,
}

/// Which recording-level tags (MusicBrainz recording id, ISRC) an accepted file may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagClaim {
    /// The sound proved this recording.
    Recording { id: String, isrcs: Vec<String> },
    /// Kept as heard: names from the routing, marked unconfirmed, and no recording id or ISRC.
    AsHeard,
}

/// Why a file was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Its print is over the match line against the heard audio's.
    DiffersFromHeard,
    /// One of its own labels names another recording, or another song once it passed as the heard
    /// master.
    Label,
    /// AcoustID names another recording.
    AnotherRecording,
    /// AcoustID has no entry for its sound, and it is not the heard master.
    NoEntry,
    /// A fingerprint AcoustID has not linked scores above every link, and it is not the heard
    /// master.
    Unknown,
    /// AcoustID could not check this one file by its sound.
    Unreadable,
}

/// What the heart does with a file the gate can neither accept nor refuse; the file is never kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undecided {
    /// The heart keeps the heard audio and stays on the re-check path.
    KeepHeardCopy,
    /// Nothing was heard: the heart holds "not yet proven" until a play.
    NotYetProven,
}

/// The gate's verdict on one downloaded file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// The file is the recording heard; `tags` says which recording tags it may carry.
    Accept {
        by: Acceptance,
        tags: TagClaim,
    },
    Refuse(Refusal),
    Undecided(Undecided),
    /// The file waits in the one hold path for this.
    Hold(WaitingFor),
}

/// A file's own labels, as its source delivered it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileLabels {
    /// Its ISRC tags (`TSRC`, `ISRC`), normalized.
    pub isrcs: Vec<String>,
    /// Its `MUSICBRAINZ_TRACKID` tag.
    pub recording_id: Option<String>,
}

/// What MusicBrainz said about one question the gate needed.
#[derive(Debug, Clone, PartialEq)]
pub enum MusicBrainzFact<T> {
    Known(T),
    NotKnown,
    Busy,
}

/// Everything known about one downloaded file when the gate judges it.
#[derive(Debug, Clone, PartialEq)]
pub struct FileFacts {
    /// AcoustID's answer for the file's own print.
    pub answer: AcoustIdOutcome,
    pub labels: FileLabels,
    /// The bit error of the file's print against the heard audio's print, when both exist.
    pub heard_distance: Option<f64>,
    /// The file is the very audio that played, by identity.
    pub identical: bool,
    /// The heard-master check's verdict, once it ran.
    pub heard_master: Option<HeardMaster>,
    /// The recording MusicBrainz now files each asked id under.
    pub current_recordings: Vec<(String, MusicBrainzFact<String>)>,
    /// The recordings MusicBrainz lists for each asked code.
    pub isrc_recordings: Vec<(String, MusicBrainzFact<Vec<AnswerRecording>>)>,
}

impl FileFacts {
    /// A file's facts before anything but its answer and its labels is known.
    pub fn new(answer: AcoustIdOutcome, labels: FileLabels) -> Self {
        Self {
            answer,
            labels,
            heard_distance: None,
            identical: false,
            heard_master: None,
            current_recordings: Vec::new(),
            isrc_recordings: Vec::new(),
        }
    }
}

/// One more fact the gate needs before it can decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// The heard-master check of the file against the heard audio.
    HeardMaster,
    /// The recording MusicBrainz now files this id under (`recording/<id>?fmt=json`, which answers
    /// a merged id with the recording it became).
    CurrentRecording(String),
    /// The recordings MusicBrainz lists for this code (`isrc/<code>?inc=artist-credits&fmt=json`).
    IsrcRecordings(String),
}

/// The gate's answer for one file: its verdict, or the next fact it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    Needs(Need),
    Verdict(GateVerdict),
}

/// The heard sample's AcoustID outcome read against the catalog R, before R becomes anything else.
#[derive(Debug, Clone, PartialEq)]
pub enum HeardAnswerReading {
    /// AcoustID has not answered yet.
    Busy,
    /// AcoustID is off, or could not read the sample: the catalog's clip check decides instead.
    NeedsClip,
    /// A believed different song played.
    AnotherSong,
    /// R stands. `tag` is the dominant recording when the answer reads clear against R.
    RStays { tag: Option<String> },
    /// The stream stood in with another version: R becomes this recording and its same-ISRC
    /// duplicates.
    Becomes(WeighedRecording),
    /// Nothing proves which recording played.
    OnlyHeardOrMaster,
}

/// What a never-played heart's stand-in says about R.
#[derive(Debug, Clone, PartialEq)]
pub struct StandInReading {
    /// The recording R narrows to (with a catalog R) or becomes (with none), when one recording
    /// from the stand-in's answer qualifies.
    pub recording: Option<WeighedRecording>,
    /// The stand-in's print is what a download's print is compared with.
    pub is_reference: bool,
}

/// What a file's own labels say against R.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelCheck {
    /// No label names another recording.
    Clean,
    /// A label names another recording; `other_song` when that recording is another song.
    NamesAnother { other_song: bool },
    /// A label needs one more MusicBrainz answer before it can be read.
    Needs(Need),
    /// MusicBrainz did not answer for a label.
    Busy,
}

/// How a file of an origin that keeps the shipped checks fares against the AcoustID reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportJudgement {
    /// The reading or a label names another recording: refused, and the next candidate is tried.
    Refuse,
    /// The shipped verdict stands. `proven` when the reading is clear, so a confirmed file may
    /// carry its recording tags; otherwise a confirmed file lands marked "version not proven"
    /// with none.
    Stands { proven: bool },
}

#[cfg(test)]
#[path = "heart_gate_tests.rs"]
mod tests;
