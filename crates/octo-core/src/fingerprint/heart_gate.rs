//! What a heart or a play heard, and the shapes its gate works in: the source that played, the
//! album a person listened from, the recording they heard (R), why an acquisition runs, what a held
//! acquisition waits for, the readings the gate takes (the heard sample's AcoustID answer, the
//! catalog's clip, the heard master) and its verdict on one downloaded file.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::common::{SongIdentity, SongMatchOptions, SongTitle};
use crate::fingerprint::acoust_id_client::{AcoustIdAnswer, AcoustIdOutcome, AnswerRecording};
use crate::fingerprint::clip_check;
use crate::fingerprint::heart_release::MOST_HEARD_RECORDINGS;
use crate::fingerprint::verification::{
    AnswerReading, MIN_SHARE, MIN_SOURCES_TO_CONFIRM, Reading, SongName, WeighedRecording,
    believed_other_song, is_same_song, length_fits, one_recording, r_song, read_answer, same_song,
};
use crate::models::domain::explicit_status;

/// The client an album context opened through Navidrome's own API is filed under: that API sends
/// no client name.
pub const NATIVE_CLIENT: &str = "navidrome-native";

/// At or under this share of differing bits from the heard print, a file can be the take that
/// played. Measured: the same recording 0.052 to 0.185, another mix or master of it 0.096 to
/// 0.200, a re-recording 0.272 and up, a live take 0.40 and up, another song 0.425 and up.
pub const SAME_TAKE_AT_MOST: f64 = 0.22;
/// A same-take file's length may be this many seconds from the song's, or
/// [`SAME_TAKE_LENGTH_SHARE`] of the song's length when that is more.
pub const SAME_TAKE_LENGTH_SECONDS: f64 = 3.0;
pub const SAME_TAKE_LENGTH_SHARE: f64 = 0.02;
/// Version words that say nothing of another performance: another mix of the take that played,
/// and a bare "version".
const SAME_TAKE_NEUTRAL: [&str; 2] = ["mix", "version"];

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
    /// AcoustID is off (or could not settle what played) and the catalog's clip could not settle
    /// it either; the person may keep the heard audio anyway.
    AcoustIdOff,
    /// Deezer has not answered for the catalog's clip.
    Deezer,
    /// The yt-dlp shim has not answered for the heard video.
    Shim,
    /// Nothing could be proven yet; a play of the song, a proof or un-hearting ends it.
    NotYetProven,
    /// `fpcalc` could not print the sound the catalog's clip check reads; Octo's next start or the
    /// holder's next play of the song runs the check again.
    SoundUnread,
}

impl WaitingFor {
    /// Every wait, in declaration order.
    pub const ALL: [WaitingFor; 8] = [
        Self::MusicBrainz,
        Self::LidarrIndexers,
        Self::AcoustId,
        Self::AcoustIdOff,
        Self::Deezer,
        Self::Shim,
        Self::NotYetProven,
        Self::SoundUnread,
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
            Self::SoundUnread => "SoundUnread",
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
    /// The ISRCs a file kept as the very audio heard, or as its master, may carry with the heard
    /// answer's recording tag: that recording's own codes that no other recording in the heard
    /// answer carries ([`heard_codes`]). None claims no ISRC.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tag_isrcs: Vec<String>,
    /// The song's words as the catalog marks them where the person listened from: explicit (1),
    /// the clean edit (3) or neither (0), as `explicit_status::from_catalog` reads them. None when
    /// the catalog does not say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit: Option<i32>,
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
    /// `fpcalc` could not print the clip or the heard audio, so nothing is known of what played:
    /// the heart holds under [`ClipReading::UNPRINTED_WAITS_FOR`], whose wake-ups (Octo's next
    /// start, the song's next play) run the check again.
    Unprinted,
}

impl ClipReading {
    /// What a heart holds under when `fpcalc` could not print for its clip check: a wait of its
    /// own, whose holds run again at Octo's next start and at the holder's next play of the song,
    /// never because Deezer answered.
    pub const UNPRINTED_WAITS_FOR: WaitingFor = WaitingFor::SoundUnread;
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
    /// The take that played, perhaps in another master or mix of it: its print within
    /// [`SAME_TAKE_AT_MOST`] of the heard print, as long as the song, and nothing about it naming
    /// another song or another version.
    SameTake,
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

/// A file's own names, which can only refuse it as the same take: its title and album as its
/// labels give them, and its file name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileNames {
    pub title: Option<String>,
    pub album: Option<String>,
    /// The file's name without its extension.
    pub file: Option<String>,
}

/// What MusicBrainz said about one question the gate needed.
#[derive(Debug, Clone, PartialEq)]
pub enum MusicBrainzFact<T> {
    Known(T),
    NotKnown,
    Busy,
    /// MusicBrainz answered other questions but never this one, so nothing is learned from it: a
    /// label it was asked for neither names another recording nor clears one.
    GaveUp,
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
    /// The file's length in seconds, when its container gives one.
    pub seconds: Option<f64>,
    pub names: FileNames,
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
            seconds: None,
            names: FileNames::default(),
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

/// Reads the heard sample's AcoustID outcome against the catalog R, the first match winning:
///
/// - AcoustID has not answered: [`HeardAnswerReading::Busy`];
/// - AcoustID is off or could not read the sample: [`HeardAnswerReading::NeedsClip`], since only
///   the catalog's clip can then say what played;
/// - the answer's dominant recording is a believed different song (another song at most of the
///   weight wins over R's sliver of it): [`HeardAnswerReading::AnotherSong`];
/// - the reading is clear or ambiguous (with R's recording ids known), or R's recordings hold at
///   least [`MIN_SHARE`] of the weight: R stays, tagged with the dominant recording only when the
///   reading is clear;
/// - R holds less and the dominant recording is R's song at half the weight or more: the stream
///   stood in with another version of the song, and R becomes it;
/// - the dominant recording is another song, at any weight too slight to believe: only the
///   catalog's clip can say whether another song played, so it decides
///   ([`HeardAnswerReading::NeedsClip`]) before the heard audio or its master may be kept;
/// - anything else, an empty or unknown answer included: nothing proves which recording played.
pub fn read_heard_answer(
    outcome: &AcoustIdOutcome,
    r: &HeardRecording,
    min_score: f64,
) -> HeardAnswerReading {
    let answer = match outcome {
        AcoustIdOutcome::Busy => return HeardAnswerReading::Busy,
        AcoustIdOutcome::Off | AcoustIdOutcome::Unreadable { .. } => return HeardAnswerReading::NeedsClip,
        AcoustIdOutcome::Answered(answer) => answer,
    };
    let reading = read_answer(answer, r, min_score);
    let Some(dominant) = reading.dominant() else {
        return HeardAnswerReading::OnlyHeardOrMaster;
    };
    if believed_other_song(&reading, r, dominant) {
        return HeardAnswerReading::AnotherSong;
    }
    // With no recording ids, R's song read alone is ambiguous: what stood in is read as the old
    // "another" was, so R can still become the recording that played.
    let ids_known = !r.ids.is_empty();
    match reading.reading {
        Reading::Clear => HeardAnswerReading::RStays {
            tag: Some(dominant.id.clone()),
        },
        Reading::Ambiguous if ids_known => HeardAnswerReading::RStays { tag: None },
        Reading::Another if reading.r_share >= MIN_SHARE => HeardAnswerReading::RStays { tag: None },
        Reading::Another | Reading::Ambiguous
            if same_song(&reading, r, dominant) && 2 * dominant.weight >= reading.total_weight() =>
        {
            HeardAnswerReading::Becomes(dominant.clone())
        }
        _ if !same_song(&reading, r, dominant) => HeardAnswerReading::NeedsClip,
        _ => HeardAnswerReading::OnlyHeardOrMaster,
    }
}

/// The R a heard answer's dominant recording D makes: D first, then the recordings MusicBrainz
/// files under D's codes with D's note (`duplicates`, as the caller fetched them), at most
/// [`MOST_HEARD_RECORDINGS`] ids, with every one of them kept as R's duplicates. R's ISRCs and
/// artist ids become D's; its title and artist stay the catalog's, which say what the same song is.
/// The answer is read again against that R, and D's id is the tag a file kept as heard may carry
/// only when that reading is clear and D carries at least [`MIN_SOURCES_TO_CONFIRM`] submissions
/// of its own. Otherwise R may be another mix than the one heard, so it is marked `from_unclear`
/// and no clear reading against it ever accepts a file.
pub fn became(
    answer: &AcoustIdAnswer,
    dominant: &WeighedRecording,
    duplicates: &[String],
    catalog: &HeardRecording,
    min_score: f64,
) -> (HeardRecording, Option<String>) {
    let mut every = vec![dominant.id.clone()];
    for id in duplicates.iter().map(|id| id.trim()).filter(|id| !id.is_empty()) {
        if !every.iter().any(|known| known.eq_ignore_ascii_case(id)) {
            every.push(id.to_string());
        }
    }
    let mut artist_ids: Vec<String> = Vec::new();
    for id in dominant.artists.iter().filter_map(|artist| artist.id.as_ref()) {
        if !artist_ids.contains(id) {
            artist_ids.push(id.clone());
        }
    }
    let mut r = HeardRecording {
        ids: every.iter().take(MOST_HEARD_RECORDINGS).cloned().collect(),
        duplicates: every,
        isrcs: dominant.isrcs.clone(),
        title: catalog.title.clone(),
        artist: catalog.artist.clone(),
        artist_ids,
        from_unclear: false,
        tag_isrcs: Vec::new(),
        explicit: catalog.explicit,
    };
    let again = read_answer(answer, &r, min_score);
    let tag = (again.reading == Reading::Clear && dominant.weight >= i64::from(MIN_SOURCES_TO_CONFIRM))
        .then(|| dominant.id.clone());
    r.from_unclear = tag.is_none();
    if let Some(id) = &tag {
        r.tag_isrcs = heard_codes(answer, id);
    }
    (r, tag)
}

/// The ISRCs a file kept as heard may carry beside the heard answer's recording `recording_id`:
/// that recording's own codes in the answer that no other recording of the answer carries, since
/// MusicBrainz files one code under more than one mix and the sound proves only the recording.
pub fn heard_codes(answer: &AcoustIdAnswer, recording_id: &str) -> Vec<String> {
    let recordings = || answer.results.iter().flat_map(|result| &result.recordings);
    let carried_elsewhere = |code: &str| {
        recordings().any(|other| {
            !other.id.eq_ignore_ascii_case(recording_id)
                && other
                    .isrcs
                    .iter()
                    .any(|isrc| isrc.trim().eq_ignore_ascii_case(code))
        })
    };
    let mut codes: Vec<String> = Vec::new();
    for code in recordings()
        .filter(|recording| recording.id.eq_ignore_ascii_case(recording_id))
        .flat_map(|recording| &recording.isrcs)
        .map(|code| code.trim())
        .filter(|code| !code.is_empty())
    {
        if !carried_elsewhere(code) && !codes.iter().any(|known| known.eq_ignore_ascii_case(code)) {
            codes.push(code.to_string());
        }
    }
    codes
}

/// What a never-played heart's stand-in (the pinned video, fetched only to help find R) says.
/// With a catalog R, the stand-in may only narrow R to the one recording its answer gives, and
/// only when that recording is already R's; its print is the reference a download is compared
/// with only when its answer reads clear or ambiguous against R. With no catalog R (no ids), the
/// one recording its answer gives becomes R when its length fits the song's, and the stand-in is
/// then the reference.
pub fn read_stand_in(
    answer: &AcoustIdAnswer,
    r: &HeardRecording,
    heard_seconds: Option<i32>,
    min_score: f64,
) -> StandInReading {
    let reading = read_answer(answer, r, min_score);
    let one = one_recording(&reading, r);
    if !r.ids.is_empty() {
        return StandInReading {
            recording: one.filter(|found| r.holds(&found.id)).cloned(),
            is_reference: matches!(reading.reading, Reading::Clear | Reading::Ambiguous),
        };
    }
    let fits = one.filter(|found| match (found.duration_seconds, heard_seconds) {
        (Some(length), Some(heard)) if heard > 0 => length_fits(length, f64::from(heard)),
        _ => true,
    });
    StandInReading {
        is_reference: fits.is_some(),
        recording: fits.cloned(),
    }
}

/// Whether a candidate is the very audio that played, by identity alone: a prefetched copy under
/// the heard key that is still the copy that played (the same ready time and source file, so a
/// later fetch under the key is another copy); a copy of the heard video's own stream with the
/// confirmed byte size and content type that played, and its format when both sides know it; or
/// the library or acquired file that played, by its path.
pub fn is_heard_audio(heard: &HeardSource, candidate: &HeardSource) -> bool {
    match (heard, candidate) {
        (
            HeardSource::Copy {
                key,
                ready_at,
                source_file,
            },
            HeardSource::Copy {
                key: other_key,
                ready_at: other_ready_at,
                source_file: other_source_file,
            },
        ) => {
            key == other_key
                && ready_at.is_some()
                && ready_at == other_ready_at
                && source_file == other_source_file
        }
        (
            HeardSource::Video {
                id,
                size,
                content_type,
                itag,
            },
            HeardSource::Video {
                id: other_id,
                size: other_size,
                content_type: other_content_type,
                itag: other_itag,
            },
        ) => {
            id == other_id
                && size.is_some()
                && size == other_size
                && content_type.eq_ignore_ascii_case(other_content_type)
                && match (itag, other_itag) {
                    (Some(itag), Some(other_itag)) => itag == other_itag,
                    _ => true,
                }
        }
        (
            HeardSource::Library { path } | HeardSource::Acquired { path },
            HeardSource::Library { path: other } | HeardSource::Acquired { path: other },
        ) => !path.is_empty() && path == other,
        _ => false,
    }
}

/// What a file's own labels say against R. Labels can only refuse a file, never accept one.
///
/// - An ISRC label names R when it is one of R's ISRCs, or when in the file's answer R's
///   recordings carry it (one carried by R's and another recording too is neutral, since
///   MusicBrainz files one code under more than one mix); otherwise it names another recording,
///   another song when no recording of R's song in the answer carries it. With no answer that
///   links anything, a code other than R's is asked of MusicBrainz, and names another recording
///   unless one it lists is R's, another song when every one it lists is.
/// - A `MUSICBRAINZ_TRACKID` label is asked of MusicBrainz first, unless it is already R's or one
///   of R's duplicates (MusicBrainz answers a merged id with the recording it became), and names
///   another recording when the recording it now is is neither; another song when the answer
///   shows that recording as another song.
///
/// A label whose question MusicBrainz gave up on says nothing either way. A label that names
/// another song wins; then the first fact still to be asked ([`LabelCheck::Needs`]); then a
/// MusicBrainz that did not answer ([`LabelCheck::Busy`]).
pub fn check_labels(reading: Option<&AnswerReading>, facts: &FileFacts, r: &HeardRecording) -> LabelCheck {
    combine(&label_findings(reading, facts, r))
}

/// How a file of an origin that keeps the shipped checks fares against its AcoustID reading:
/// refused when the reading names another recording or one of its labels does, the shipped
/// verdict standing otherwise, proven only by a clear reading. A label MusicBrainz has not
/// settled refuses nothing.
pub fn import_judgement(reading: &AnswerReading, facts: &FileFacts, r: &HeardRecording) -> ImportJudgement {
    let refused_by_label = label_findings(Some(reading), facts, r)
        .iter()
        .any(|finding| matches!(finding, LabelCheck::NamesAnother { .. }));
    if reading.reading == Reading::Another || refused_by_label {
        ImportJudgement::Refuse
    } else {
        ImportJudgement::Stands {
            proven: reading.reading == Reading::Clear,
        }
    }
}

impl HeartGate {
    /// Judges one downloaded file, or names the next fact it needs before it can. The first match
    /// wins:
    ///
    /// 1. Proof 2: a file whose print is over the match line against the heard audio's is refused
    ///    (never when the stream stood in with another song, and for a never-played heart only
    ///    against a stand-in that is its reference).
    /// 2. What played is not settled yet: the heard lookup is busy, or with AcoustID off the
    ///    catalog's clip did not read "this song". The file waits for that, whatever else it is.
    /// 3. A0: the file is the very audio that played.
    /// 4. A2: the file is the master that played, unless its own reading names another recording
    ///    or a label names another song. Only then is the heard master asked for.
    /// 5. A label naming another recording refuses it.
    /// 6. When nothing proves which recording played, only A0 or A2 could have kept it.
    /// 7. A1, the file's own reading: clear accepts (never against an R built on a reading that was
    ///    not clear), ambiguous leaves it undecided, another recording, no entry and an unlinked
    ///    fingerprint refuse it. A busy lookup holds; a file AcoustID cannot read is refused; with
    ///    AcoustID off the file is undecided.
    /// 8. The same take: a file steps 5 to 7 would keep the heard copy for, or refuse as another
    ///    recording of R's song, no entry or an unlinked fingerprint, is accepted when it is the
    ///    take that played; and a file of a heart never played they leave "not yet proven" when
    ///    it is the take of the catalog's track ([`HeartGate::same_take`]). Ahead of it, a heart
    ///    never played whose stand-in is the reference checks the file against the stand-in's
    ///    master ([`HeartGate::stand_in_master`]).
    ///
    /// Before the file's reading names another recording, a dominant recording of R's song
    /// outside R is asked of MusicBrainz, since AcoustID never follows MusicBrainz's merges.
    /// Undecided keeps the heard copy when something was heard, else holds "not yet proven".
    pub fn judge(&self, facts: &FileFacts, min_score: f64) -> Judgement {
        let catalog = self.r.clone().unwrap_or_default();
        if self.proof_two_applies()
            && facts
                .heard_distance
                .is_some_and(|distance| distance > clip_check::MATCH_AT_MOST)
        {
            return refuse(Refusal::DiffersFromHeard);
        }
        match &self.reading {
            HeardReading::Busy => return hold(WaitingFor::AcoustId),
            HeardReading::Clip(ClipReading::Busy(wait)) => return hold(*wait),
            HeardReading::Clip(ClipReading::Unprinted) => return hold(ClipReading::UNPRINTED_WAITS_FOR),
            HeardReading::Clip(
                ClipReading::AnotherSongOrTake | ClipReading::Unclear | ClipReading::NoClip,
            ) => {
                return hold(WaitingFor::AcoustIdOff);
            }
            _ => {}
        }
        if self.heard_usable() && facts.identical {
            return accept(Acceptance::HeardAudio, self.heard_tag(&catalog));
        }

        let (reading, r) = match &facts.answer {
            AcoustIdOutcome::Answered(answer) => match file_reading(answer, &catalog, facts, min_score) {
                Ok((reading, r)) => (Some(reading), r),
                Err(judgement) => return judgement,
            },
            _ => (None, catalog),
        };
        let findings = label_findings(reading.as_ref(), facts, &r);
        let exact = self.exact_verdict(facts, reading.as_ref(), &r, &findings);
        if let Some(judgement) = self.stand_in_master(&exact, facts, reading.as_ref(), &r, &findings) {
            return judgement;
        }
        if self.same_take(&exact, facts, reading.as_ref(), &r, &findings) {
            return accept(Acceptance::SameTake, self.heard_tag(&r));
        }
        exact
    }

    /// A heart never played whose stand-in is the reference: where the exact rule leaves its
    /// file not yet proven, or refuses it for no entry or an unlinked fingerprint, the heard
    /// master's check runs against the stand-in's audio, as a played heart's does against the
    /// heard audio, and the same master accepts the file, explicit song or not. Only a file
    /// AcoustID read, whose labels name nothing else, as long as the song, and whose own names
    /// add no version is checked; another song never is. The stand-in's busy shim holds, and any
    /// other answer leaves the file to the same take and the exact rule. None when the check does
    /// not apply or did not accept.
    fn stand_in_master(
        &self,
        exact: &Judgement,
        facts: &FileFacts,
        reading: Option<&AnswerReading>,
        r: &HeardRecording,
        findings: &[LabelCheck],
    ) -> Option<Judgement> {
        let reading = reading?;
        let checked = matches!(
            exact,
            Judgement::Verdict(
                GateVerdict::Undecided(Undecided::NotYetProven)
                    | GateVerdict::Refuse(Refusal::NoEntry | Refusal::Unknown)
            )
        ) && self.never_played_heart()
            && self
                .stand_in
                .as_ref()
                .is_some_and(|stand_in| stand_in.is_reference)
            && findings.iter().all(|finding| *finding == LabelCheck::Clean)
            && reading
                .dominant()
                .is_none_or(|dominant| same_song(reading, r, dominant))
            && self.same_length(facts.seconds)
            && !names_another_version(&facts.names, r);
        if !checked {
            return None;
        }
        match facts.heard_master {
            None => Some(Judgement::Needs(Need::HeardMaster)),
            Some(HeardMaster::Same) => Some(accept(Acceptance::HeardMaster, self.heard_tag(r))),
            Some(HeardMaster::Busy) => Some(hold(WaitingFor::Shim)),
            Some(HeardMaster::Differs | HeardMaster::Undecided | HeardMaster::HeardGone) => None,
        }
    }

    /// A heart (or a hearted album's track) nobody played: nothing was heard of it.
    fn never_played_heart(&self) -> bool {
        self.reading == HeardReading::NotHeard
            && matches!(
                self.origin,
                AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart
            )
    }

    /// Steps 4 to 7 of [`HeartGate::judge`], once the file's reading and its labels' findings are
    /// known.
    fn exact_verdict(
        &self,
        facts: &FileFacts,
        reading: Option<&AnswerReading>,
        r: &HeardRecording,
        findings: &[LabelCheck],
    ) -> Judgement {
        let heard = self.heard_usable();
        let master_may_accept = heard && reading.is_none_or(|f| f.reading != Reading::Another);
        let names_other_song = findings.contains(&LabelCheck::NamesAnother { other_song: true });
        if master_may_accept && !names_other_song {
            if let Some(waiting) = unsettled(findings) {
                return waiting;
            }
            match facts.heard_master {
                None => return Judgement::Needs(Need::HeardMaster),
                Some(HeardMaster::Same) => return accept(Acceptance::HeardMaster, self.heard_tag(r)),
                Some(HeardMaster::Busy) => return hold(WaitingFor::Shim),
                Some(HeardMaster::Differs | HeardMaster::Undecided | HeardMaster::HeardGone) => {}
            }
        }

        if findings
            .iter()
            .any(|finding| matches!(finding, LabelCheck::NamesAnother { .. }))
        {
            return refuse(Refusal::Label);
        }
        if let Some(waiting) = unsettled(findings) {
            return waiting;
        }
        let undecided = Judgement::Verdict(GateVerdict::Undecided(if heard {
            Undecided::KeepHeardCopy
        } else {
            Undecided::NotYetProven
        }));
        if self.reading == HeardReading::OnlyHeardOrMaster {
            return Judgement::Verdict(GateVerdict::Undecided(Undecided::KeepHeardCopy));
        }
        let Some(reading) = reading else {
            return match facts.answer {
                AcoustIdOutcome::Busy => hold(WaitingFor::AcoustId),
                AcoustIdOutcome::Unreadable { .. } => refuse(Refusal::Unreadable),
                AcoustIdOutcome::Off | AcoustIdOutcome::Answered(_) => undecided,
            };
        };
        match reading.reading {
            Reading::Clear if r.from_unclear => undecided,
            Reading::Clear => match reading.dominant() {
                Some(dominant) => accept(
                    Acceptance::Sound,
                    TagClaim::Recording {
                        id: current_id(facts, &dominant.id),
                        isrcs: r.isrcs.clone(),
                    },
                ),
                None => undecided,
            },
            Reading::Ambiguous => undecided,
            Reading::Another => refuse(Refusal::AnotherRecording),
            Reading::Empty => refuse(Refusal::NoEntry),
            Reading::Unknown => refuse(Refusal::Unknown),
        }
    }

    /// Whether a file is the same take, in place of the exact rule's verdict `exact`. What it
    /// sounds like is judged first:
    ///
    /// - where the exact rule keeps the heard copy or refuses the file as another recording of
    ///   R's song, no entry or an unlinked fingerprint ([`same_take_may_replace`]), something was
    ///   heard: the file's distance from it can refuse it (proof 2) and is at most
    ///   [`SAME_TAKE_AT_MOST`];
    /// - where the exact rule leaves a heart never played "not yet proven", the catalog's track
    ///   is what the person chose: with a stand-in that is the reference, the file's distance from
    ///   its print is at most [`SAME_TAKE_AT_MOST`]; with none, the file's own reading is
    ///   ambiguous with R's recordings among it carrying at least [`MIN_SOURCES_TO_CONFIRM`]
    ///   submissions, or clear against an R built on a reading that was not clear.
    ///
    /// Then all of these hold:
    ///
    /// - AcoustID read the file: with AcoustID off, busy or unable to read it, nothing changes;
    /// - the file's length is within the larger of [`SAME_TAKE_LENGTH_SECONDS`] and
    ///   [`SAME_TAKE_LENGTH_SHARE`] of the song's ([`HeartGate::heard_seconds`]);
    /// - the song is not marked explicit or the clean edit: the two sound alike, so either keeps
    ///   the exact rule;
    /// - no label and no reading of the file names another song;
    /// - the file's own names add no version the heard title does not carry
    ///   ([`names_another_version`]).
    fn same_take(
        &self,
        exact: &Judgement,
        facts: &FileFacts,
        reading: Option<&AnswerReading>,
        r: &HeardRecording,
        findings: &[LabelCheck],
    ) -> bool {
        let Some(reading) = reading else {
            return false;
        };
        let near_reference = || {
            self.proof_two_applies()
                && facts
                    .heard_distance
                    .is_some_and(|distance| distance <= SAME_TAKE_AT_MOST)
        };
        let sounds_like = if same_take_may_replace(exact) {
            self.heard_usable() && near_reference()
        } else if *exact == Judgement::Verdict(GateVerdict::Undecided(Undecided::NotYetProven))
            && self.never_played_heart()
        {
            match &self.stand_in {
                Some(stand_in) if stand_in.is_reference => near_reference(),
                // R's recordings among the candidates carry at least two submissions: a lone
                // submission is one person's word, and proves nothing.
                _ => match reading.reading {
                    Reading::Ambiguous => {
                        reading
                            .recordings
                            .iter()
                            .filter(|found| r.holds(&found.id))
                            .map(|found| found.weight)
                            .sum::<i64>()
                            >= i64::from(MIN_SOURCES_TO_CONFIRM)
                    }
                    Reading::Clear => r.from_unclear,
                    _ => false,
                },
            }
        } else {
            false
        };
        sounds_like
            && self.same_length(facts.seconds)
            && !matches!(
                r.explicit,
                Some(explicit_status::EXPLICIT | explicit_status::CLEAN)
            )
            && !findings.contains(&LabelCheck::NamesAnother { other_song: true })
            && reading
                .dominant()
                .is_none_or(|dominant| same_song(reading, r, dominant))
            && !names_another_version(&facts.names, r)
    }

    /// Whether a file of `seconds` is as long as the song: within the larger of
    /// [`SAME_TAKE_LENGTH_SECONDS`] and [`SAME_TAKE_LENGTH_SHARE`] of its length. Never when
    /// either length is unknown.
    fn same_length(&self, seconds: Option<f64>) -> bool {
        let (Some(file), Some(song)) = (
            seconds.filter(|seconds| *seconds > 0.0),
            self.heard_seconds.filter(|seconds| *seconds > 0),
        ) else {
            return false;
        };
        let song = f64::from(song);
        (file - song).abs() <= (SAME_TAKE_LENGTH_SHARE * song).max(SAME_TAKE_LENGTH_SECONDS)
    }

    /// Something was heard that may be the song: not a heart never played, and not a stream that
    /// stood in with another song.
    fn heard_usable(&self) -> bool {
        !matches!(self.reading, HeardReading::NotHeard | HeardReading::AnotherSong)
    }

    /// Whether a file's distance from the reference print can refuse it: not when what played was
    /// another song (or, with AcoustID off, may have been), and for a heart never played only when
    /// its stand-in is the reference.
    fn proof_two_applies(&self) -> bool {
        match &self.reading {
            HeardReading::AnotherSong | HeardReading::Clip(ClipReading::AnotherSongOrTake) => false,
            HeardReading::NotHeard => self
                .stand_in
                .as_ref()
                .is_some_and(|stand_in| stand_in.is_reference),
            _ => true,
        }
    }

    /// The tags a file kept as the heard audio or the heard master may carry: the recording the
    /// heard answer read clear, with only the codes its sound proved (R's `tag_isrcs`), else none
    /// ("as heard").
    fn heard_tag(&self, r: &HeardRecording) -> TagClaim {
        match &self.reading {
            HeardReading::RStays { tag: Some(id) } | HeardReading::RBecame { tag: Some(id) } => {
                TagClaim::Recording {
                    id: id.clone(),
                    isrcs: r.tag_isrcs.clone(),
                }
            }
            _ => TagClaim::AsHeard,
        }
    }
}

/// Whether the exact rule's verdict is one a same-take file of something heard is accepted in
/// place of: the heard copy kept, or a refusal for another recording (by AcoustID or a label), no
/// entry or an unlinked fingerprint. Another song is left to [`HeartGate::same_take`], which
/// refuses it.
fn same_take_may_replace(judgement: &Judgement) -> bool {
    matches!(
        judgement,
        Judgement::Verdict(
            GateVerdict::Undecided(Undecided::KeepHeardCopy)
                | GateVerdict::Refuse(
                    Refusal::Label | Refusal::AnotherRecording | Refusal::NoEntry | Refusal::Unknown
                )
        )
    )
}

/// Whether a file's own names add a version the heard title (R's) does not carry: live,
/// acoustic, demo, instrumental, karaoke, cover, sped up, slowed, nightcore, remix, edit,
/// extended, or any other marker [`SongIdentity`] reads as a different recording, and the
/// explicit word. Only another mix of the take and a bare "version" say nothing. The title and
/// the file name are read as titles; the album is read whole, since a release's name says what
/// kind it is anywhere in it ("Live at the BBC").
fn names_another_version(names: &FileNames, r: &HeardRecording) -> bool {
    let options = SongMatchOptions {
        also_neutral: SAME_TAKE_NEUTRAL.iter().map(|word| word.to_string()).collect(),
        ..Default::default()
    };
    let versions = |title: &SongTitle| -> BTreeSet<String> {
        let mut versions = SongIdentity::distinct_versions(title, Some(&options));
        if title
            .versions
            .iter()
            .any(|version| version == explicit_status::EXPLICIT_WORD)
        {
            versions.insert(explicit_status::EXPLICIT_WORD.to_string());
        }
        versions
    };
    let artist = Some(r.artist.as_str()).filter(|artist| !artist.trim().is_empty());
    let heard = versions(&SongIdentity::parse_title(&r.title, artist));
    let mut read: Vec<SongTitle> = [&names.title, &names.file]
        .into_iter()
        .flatten()
        .filter(|name| !name.trim().is_empty())
        .map(|name| SongIdentity::parse_title(name, artist))
        .collect();
    if let Some(album) = names.album.as_deref().filter(|album| !album.trim().is_empty()) {
        read.push(SongIdentity::parse_title(&format!("({album})"), None));
    }
    read.iter()
        .any(|title| versions(title).iter().any(|version| !heard.contains(version)))
}

/// The file's own reading against R. When it names another recording because a recording of R's
/// song outside R dominates, MusicBrainz is asked what that recording is now: a merged-away id of
/// one of R's recordings counts as R's (the R returned holds it), and a question MusicBrainz gave
/// up on leaves the reading ambiguous, since that recording may still be R's.
fn file_reading(
    answer: &AcoustIdAnswer,
    r: &HeardRecording,
    facts: &FileFacts,
    min_score: f64,
) -> Result<(AnswerReading, HeardRecording), Judgement> {
    let reading = read_answer(answer, r, min_score);
    let Some(dominant) = reading
        .dominant()
        .filter(|_| reading.reading == Reading::Another)
        .filter(|dominant| !r.holds(&dominant.id) && same_song(&reading, r, dominant))
    else {
        return Ok((reading, r.clone()));
    };
    match current_recording(facts, &dominant.id) {
        None => Err(Judgement::Needs(Need::CurrentRecording(dominant.id.clone()))),
        Some(MusicBrainzFact::Busy) => Err(hold(WaitingFor::MusicBrainz)),
        Some(MusicBrainzFact::Known(current)) if r.holds(current) => {
            let mut merged = r.clone();
            merged.ids.push(dominant.id.clone());
            Ok((read_answer(answer, &merged, min_score), merged))
        }
        Some(MusicBrainzFact::GaveUp) => Ok((
            AnswerReading {
                reading: Reading::Ambiguous,
                ..reading
            },
            r.clone(),
        )),
        Some(_) => Ok((reading, r.clone())),
    }
}

/// What each of a file's labels says, the recording id first, then each ISRC.
fn label_findings(reading: Option<&AnswerReading>, facts: &FileFacts, r: &HeardRecording) -> Vec<LabelCheck> {
    let reading = reading.filter(|reading| !reading.recordings.is_empty());
    let mut findings = Vec::new();
    if let Some(id) = facts
        .labels
        .recording_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        findings.push(recording_label(id, reading, facts, r));
    }
    for code in facts
        .labels
        .isrcs
        .iter()
        .map(|code| code.trim())
        .filter(|code| !code.is_empty())
    {
        findings.push(isrc_label(code, reading, facts, r));
    }
    findings
}

fn recording_label(
    id: &str,
    reading: Option<&AnswerReading>,
    facts: &FileFacts,
    r: &HeardRecording,
) -> LabelCheck {
    if r.holds(id) || is_duplicate(r, id) {
        return LabelCheck::Clean;
    }
    match current_recording(facts, id) {
        None => LabelCheck::Needs(Need::CurrentRecording(id.to_string())),
        Some(MusicBrainzFact::Busy) => LabelCheck::Busy,
        // A question MusicBrainz could not answer says nothing of the label.
        Some(MusicBrainzFact::GaveUp) => LabelCheck::Clean,
        Some(MusicBrainzFact::NotKnown) => LabelCheck::NamesAnother { other_song: false },
        Some(MusicBrainzFact::Known(current)) if r.holds(current) || is_duplicate(r, current) => {
            LabelCheck::Clean
        }
        Some(MusicBrainzFact::Known(current)) => LabelCheck::NamesAnother {
            other_song: reading.is_some_and(|reading| {
                reading
                    .recordings
                    .iter()
                    .find(|found| found.id.eq_ignore_ascii_case(current) || found.id.eq_ignore_ascii_case(id))
                    .is_some_and(|found| !same_song(reading, r, found))
            }),
        },
    }
}

fn isrc_label(
    code: &str,
    reading: Option<&AnswerReading>,
    facts: &FileFacts,
    r: &HeardRecording,
) -> LabelCheck {
    if r.isrcs.iter().any(|own| own.eq_ignore_ascii_case(code)) {
        return LabelCheck::Clean;
    }
    if let Some(reading) = reading {
        let carriers: Vec<&WeighedRecording> = reading
            .recordings
            .iter()
            .filter(|found| found.isrcs.iter().any(|isrc| isrc.eq_ignore_ascii_case(code)))
            .collect();
        if carriers.iter().any(|found| r.holds(&found.id)) {
            return LabelCheck::Clean;
        }
        return LabelCheck::NamesAnother {
            other_song: !carriers.is_empty() && !carriers.iter().any(|found| same_song(reading, r, found)),
        };
    }
    match facts
        .isrc_recordings
        .iter()
        .find(|(asked, _)| asked.eq_ignore_ascii_case(code))
        .map(|(_, fact)| fact)
    {
        None => LabelCheck::Needs(Need::IsrcRecordings(code.to_string())),
        Some(MusicBrainzFact::Busy) => LabelCheck::Busy,
        Some(MusicBrainzFact::GaveUp) => LabelCheck::Clean,
        Some(MusicBrainzFact::NotKnown) => LabelCheck::NamesAnother { other_song: false },
        Some(MusicBrainzFact::Known(listed)) => {
            if listed
                .iter()
                .any(|found| r.holds(&found.id) || is_duplicate(r, &found.id))
            {
                return LabelCheck::Clean;
            }
            let nothing_read = AnswerReading {
                reading: Reading::Empty,
                recordings: Vec::new(),
                r_share: 0.0,
            };
            let song = r_song(&nothing_read, r);
            LabelCheck::NamesAnother {
                other_song: !listed.is_empty()
                    && song
                        .is_some_and(|song| listed.iter().all(|found| !is_same_song(&song_of(found), &song))),
            }
        }
    }
}

/// Every label's findings as one: another song first, then a fact still to ask, then a busy
/// MusicBrainz, then another recording.
fn combine(findings: &[LabelCheck]) -> LabelCheck {
    if findings.contains(&LabelCheck::NamesAnother { other_song: true }) {
        return LabelCheck::NamesAnother { other_song: true };
    }
    if let Some(need) = findings.iter().find_map(|finding| match finding {
        LabelCheck::Needs(need) => Some(need.clone()),
        _ => None,
    }) {
        return LabelCheck::Needs(need);
    }
    if findings.contains(&LabelCheck::Busy) {
        return LabelCheck::Busy;
    }
    if findings.contains(&LabelCheck::NamesAnother { other_song: false }) {
        return LabelCheck::NamesAnother { other_song: false };
    }
    LabelCheck::Clean
}

/// The label fact still to ask, or MusicBrainz's busy answer, when one label has either.
fn unsettled(findings: &[LabelCheck]) -> Option<Judgement> {
    if let Some(need) = findings.iter().find_map(|finding| match finding {
        LabelCheck::Needs(need) => Some(need.clone()),
        _ => None,
    }) {
        return Some(Judgement::Needs(need));
    }
    findings
        .contains(&LabelCheck::Busy)
        .then(|| hold(WaitingFor::MusicBrainz))
}

fn current_recording<'a>(facts: &'a FileFacts, id: &str) -> Option<&'a MusicBrainzFact<String>> {
    facts
        .current_recordings
        .iter()
        .find(|(asked, _)| asked.eq_ignore_ascii_case(id))
        .map(|(_, fact)| fact)
}

/// The id MusicBrainz files a recording under now, when it was asked; the id itself otherwise.
fn current_id(facts: &FileFacts, id: &str) -> String {
    match current_recording(facts, id) {
        Some(MusicBrainzFact::Known(current)) => current.clone(),
        _ => id.to_string(),
    }
}

fn is_duplicate(r: &HeardRecording, id: &str) -> bool {
    r.duplicates.iter().any(|known| known.eq_ignore_ascii_case(id))
}

/// A recording MusicBrainz lists, as two songs are compared.
fn song_of(recording: &AnswerRecording) -> SongName {
    SongName {
        title: recording.title.clone(),
        artists: recording
            .artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
        artist_ids: recording
            .artists
            .iter()
            .filter_map(|artist| artist.id.clone())
            .collect(),
    }
}

fn accept(by: Acceptance, tags: TagClaim) -> Judgement {
    Judgement::Verdict(GateVerdict::Accept { by, tags })
}

fn refuse(why: Refusal) -> Judgement {
    Judgement::Verdict(GateVerdict::Refuse(why))
}

fn hold(wait: WaitingFor) -> Judgement {
    Judgement::Verdict(GateVerdict::Hold(wait))
}

#[cfg(test)]
#[path = "heart_gate_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "heart_gate_judge_tests.rs"]
mod judge_tests;
