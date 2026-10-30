//! The Soulseek metadata service's routing model: [`RoutingKind`] and [`SoulseekRouting`],
//! which `external-ids.json` carries (checked against the `external-ids.json` fixture).
//!
//! The service itself lives in `octo::services::soulseek::soulseek_metadata_service`.

use chrono::{DateTime, Utc};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::common::dotnet::eq_ignore_case;
use crate::fingerprint::heart_gate::{AlbumContext, HeardBy, HeardRecord, PlayedMark};
use crate::soulseek::song_length::LengthSource;
use crate::soulseek::stream_facts::StreamFacts;

/// What a short external id stands for. Written as its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum RoutingKind {
    #[default]
    Song = 0,
    Album = 1,
    Artist = 2,
}

/// Where an outside song, album or artist came from, enough to find it again: what the short
/// external ids Octo hands to Subsonic clients stand for.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(remote = "Self", rename_all = "PascalCase", default)]
pub struct SoulseekRouting {
    pub kind: RoutingKind,
    pub you_tube_id: Option<String>,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub duration: Option<i32>,

    /// Deezer album id, when an album search resolved one. Absent on album
    /// routings minted from a song row, which fall back to a name lookup.
    pub external_album_id: Option<String>,

    /// Deezer artist id behind an artist routing, once an artist search or an artist
    /// page settled on one. The id itself is minted from the name alone, and two artists can
    /// share a name, so this is what says which of them the page lists. Not part of the id.
    pub external_artist_id: Option<String>,

    /// Position within its album. Carried so a track downloaded as part of an
    /// album keeps its ordering: the download path rebuilds the song from its id alone,
    /// and without this every track lands untracked and sorts alphabetically.
    pub track: Option<i32>,

    /// Disc within a multi-disc release. Same reasoning as [`SoulseekRouting::track`].
    pub disc_number: Option<i32>,

    /// Track count of the album this came from. Without it the tagger fills the
    /// "x of y" denominator from a per-track Deezer search that can match a different
    /// release, producing nonsense like 5/10 on an 8-track album.
    pub total_tracks: Option<i32>,

    /// The track's ISRC, when the album listing that minted this routing named one.
    /// Carried for the same reason as [`SoulseekRouting::track`], and because it is the
    /// strongest evidence download verification can be given about which recording was asked
    /// for. Not part of the id, so routings minted before it existed keep their ids.
    pub isrc: Option<String>,

    /// The length shown for this song once a lookup found one, kept here so every
    /// later response carries it, across restarts too. Display only: see
    /// [`crate::soulseek::SongLength`] for why this is not [`SoulseekRouting::duration`].
    pub shown_duration: Option<i32>,

    /// Where [`SoulseekRouting::shown_duration`] came from, so a weaker source never
    /// replaces a stronger one.
    pub shown_duration_source: LengthSource,

    /// Whether the words are explicit (1), the clean edit (3) or neither (0), as the catalog
    /// said, so every later response for this id carries it. Display only, not part of the id
    /// (`Octo.Models.Domain.ExplicitStatus`).
    pub explicit_content: Option<i32>,

    /// "clean" for a clean edit, which gets an id of its own so it can be listed beside the
    /// explicit original. None for every other song or album, so every id minted before this
    /// existed (stars, playlists, pins) stays the same.
    pub version: Option<String>,

    /// The exact byte count of the pinned video's audio stream, once an answer named it. It
    /// belongs to [`SoulseekRouting::you_tube_id`]: pinning another video clears it. Not part of
    /// the id.
    pub stream_size: Option<u64>,

    /// Whether [`SoulseekRouting::stream_size`] came from bytes that were played. Written only
    /// when true, so a routing is written exactly as before until a play confirms its size.
    pub stream_size_confirmed: bool,

    /// The download replaces a file already in the library, whose take (an instrumental, a
    /// karaoke track, a cover) is the person's own: such a take named in the title is then
    /// fetched even while Skip covers, karaoke and instrumentals is on. Written only when true,
    /// and not part of the id.
    pub replaces_library_file: bool,

    /// The pinned video's audio format id (140 is AAC in mp4, 251 Opus in webm).
    pub stream_itag: Option<String>,

    /// The pinned video's audio container, as yt-dlp names it ("m4a", "webm").
    pub stream_ext: Option<String>,

    /// The pinned video's average audio bit rate in kbps.
    pub stream_bit_rate: Option<i32>,

    /// ReplayGain track gain in dB of the pinned video's audio, once a play measured it.
    pub track_gain: Option<f64>,

    /// ReplayGain track peak (linear) of the pinned video's audio, measured with the gain.
    pub track_peak: Option<f64>,

    /// Videos found to be another song than this one, newest last, so neither a play nor a
    /// download takes them again. At most [`Self::MAX_REFUSED_VIDEOS`].
    pub refused_videos: Vec<String>,

    /// Which ranking picked `you_tube_id`: [`Self::VIDEO_PICK`] for the current one,
    /// [`Self::VIDEO_NAMED`] for a video the song was named by, 0 for one picked before
    /// rankings were versioned.
    pub video_pick: u8,

    /// A video the current ranking prefers to `you_tube_id`, applied when a play of the song
    /// next starts at its first byte. Not persisted.
    #[serde(skip)]
    pub pending_video: Option<String>,

    /// The albums people listed or played this song from, at most 8, the least recently listed
    /// first out. Never sent to a client.
    pub contexts: Vec<AlbumContext>,

    /// What each person heard of this song, one per person, at most 8, the least recently heard
    /// first out.
    pub heard: Vec<HeardBy>,
}

/// The gain and peak are plain measured numbers, never NaN, so equality is total.
impl Eq for SoulseekRouting {}

impl SoulseekRouting {
    /// How many refused videos a song remembers; the oldest goes first.
    pub const MAX_REFUSED_VIDEOS: usize = 5;

    /// The version of the ranking that picks a song's video today.
    pub const VIDEO_PICK: u8 = 3;

    /// The mark of a video the song was named by, which no ranking picked.
    pub const VIDEO_NAMED: u8 = u8::MAX;

    /// How many album contexts a song keeps; the least recently listed goes first.
    pub const MAX_CONTEXTS: usize = 8;

    /// How many people's heard records a song keeps; the least recently heard goes first.
    pub const MAX_HEARD: usize = 8;

    /// Adds `context`, or moves the person's context of the same catalog album to it: one per
    /// person and album, the most recently listed last. A listing older than the one kept never
    /// replaces it, and a played time already known stays. True when anything changed.
    pub fn remember_context(&mut self, context: AlbumContext) -> bool {
        if context.user.trim().is_empty() || context.catalog_album_id.trim().is_empty() {
            return false;
        }
        let before = self.contexts.clone();
        let mut context = context;
        if let Some(at) = self.contexts.iter().position(|known| {
            eq_ignore_case(&known.user, &context.user) && known.catalog_album_id == context.catalog_album_id
        }) {
            let known = &self.contexts[at];
            if known.listed_at > context.listed_at {
                return false;
            }
            context.played_at = known.played_at.max(context.played_at);
            self.contexts.remove(at);
        }
        self.contexts.push(context);
        self.contexts.sort_by_key(|known| known.listed_at);
        let excess = self.contexts.len().saturating_sub(Self::MAX_CONTEXTS);
        self.contexts.drain(..excess);
        self.contexts != before
    }

    /// Marks played, at `at`, the context of `user`'s that a play names: the one whose catalog
    /// album id is the mark's, else the one most recently listed by the mark's client. True when
    /// one was marked.
    pub fn mark_played(&mut self, user: &str, mark: &PlayedMark, at: DateTime<Utc>) -> bool {
        let by_album = mark
            .catalog_album_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .and_then(|id| {
                self.contexts
                    .iter()
                    .rposition(|known| eq_ignore_case(&known.user, user) && known.catalog_album_id == id)
            });
        let same_client = |known: &AlbumContext| match (&known.client, &mark.client) {
            (Some(listed), Some(played)) => eq_ignore_case(listed, played),
            (None, None) => true,
            _ => false,
        };
        let found = by_album.or_else(|| {
            self.contexts
                .iter()
                .rposition(|known| eq_ignore_case(&known.user, user) && same_client(known))
        });
        let Some(found) = found else {
            return false;
        };
        let context = &mut self.contexts[found];
        if context.played_at.is_some_and(|played| played >= at) {
            return false;
        }
        context.played_at = Some(at);
        true
    }

    /// Keeps `record` as what `user` heard of this song, one record per person: a record heard
    /// at the same time or later replaces the one kept, an earlier one never does. A record of the
    /// same source and time with no sample keeps the sample already known. True when anything
    /// changed.
    pub fn remember_heard(&mut self, user: &str, record: HeardRecord) -> bool {
        let user = user.trim();
        if user.is_empty() {
            return false;
        }
        let mut record = record;
        if let Some(at) = self
            .heard
            .iter()
            .position(|known| eq_ignore_case(&known.user, user))
        {
            let known = &self.heard[at].record;
            if known.heard_at > record.heard_at {
                return false;
            }
            if record.sample_id.is_none()
                && known.heard_at == record.heard_at
                && known.source == record.source
            {
                record.sample_id = known.sample_id.clone();
            }
            if *known == record {
                return false;
            }
            self.heard.remove(at);
        }
        self.heard.push(HeardBy {
            user: user.to_string(),
            record,
        });
        self.heard.sort_by_key(|known| known.record.heard_at);
        let excess = self.heard.len().saturating_sub(Self::MAX_HEARD);
        self.heard.drain(..excess);
        true
    }

    /// What `user` heard of this song, when it is kept.
    pub fn heard_by(&self, user: &str) -> Option<&HeardRecord> {
        self.heard
            .iter()
            .find(|known| eq_ignore_case(&known.user, user))
            .map(|known| &known.record)
    }

    /// Takes another copy's album contexts and heard records, keeping for each person (and
    /// album) the later one, so an older copy never writes back over a newer one.
    pub fn merge_heard(&mut self, contexts: &[AlbumContext], heard: &[HeardBy]) {
        for context in contexts {
            self.remember_context(context.clone());
        }
        for known in heard {
            self.remember_heard(&known.user, known.record.clone());
        }
    }

    pub fn has_you_tube(&self) -> bool {
        self.you_tube_id.as_deref().is_some_and(|id| !id.is_empty())
    }

    /// What is known of the pinned video's stream.
    pub fn stream_facts(&self) -> StreamFacts {
        StreamFacts {
            size: self.stream_size,
            size_confirmed: self.stream_size_confirmed,
            itag: self.stream_itag.clone(),
            ext: self.stream_ext.clone(),
            bit_rate: self.stream_bit_rate,
            track_gain: self.track_gain,
            track_peak: self.track_peak,
        }
    }

    /// Takes what `facts` knows of the pinned video's stream, keeping what it does not.
    pub fn merge_stream_facts(&mut self, facts: &StreamFacts) {
        let mut known = self.stream_facts();
        known.merge(facts);
        self.stream_size = known.size;
        self.stream_size_confirmed = known.size_confirmed;
        self.stream_itag = known.itag;
        self.stream_ext = known.ext;
        self.stream_bit_rate = known.bit_rate;
        self.track_gain = known.track_gain;
        self.track_peak = known.track_peak;
    }

    /// Pins `video` for playback. A video other than the pinned one forgets the old one's
    /// stream facts, which describe a different stream. A refused video is never pinned. True
    /// when the pin changed.
    pub fn pin_video(&mut self, video: &str) -> bool {
        if self.you_tube_id.as_deref() == Some(video) || self.refuses_video(video) {
            return false;
        }
        self.you_tube_id = Some(video.to_string());
        self.video_pick = 0;
        self.stream_size = None;
        self.stream_size_confirmed = false;
        self.stream_itag = None;
        self.stream_ext = None;
        self.stream_bit_rate = None;
        self.track_gain = None;
        self.track_peak = None;
        true
    }

    /// Forgets `video` as the song's video, what was known of its stream, and a length learned
    /// from it. False when the song plays another video, or none.
    pub fn unpin_video(&mut self, video: &str) -> bool {
        if self.you_tube_id.as_deref() != Some(video) {
            return false;
        }
        // A length learned from this video goes with it; the song's own length (catalog or
        // file) takes its place.
        if self.shown_duration_source == LengthSource::Video {
            if self.duration.is_some() && self.duration == self.shown_duration {
                self.duration = None;
            }
            self.shown_duration = None;
            self.shown_duration_source = LengthSource::None;
        } else if self.shown_duration_source >= LengthSource::LastFm
            && let Some(known) = self.shown_duration.filter(|&s| s > 0)
        {
            // The catalog's or the file's length stands for the song again.
            self.duration = Some(known);
        }
        self.you_tube_id = None;
        self.video_pick = 0;
        self.stream_size = None;
        self.stream_size_confirmed = false;
        self.stream_itag = None;
        self.stream_ext = None;
        self.stream_bit_rate = None;
        self.track_gain = None;
        self.track_peak = None;
        true
    }

    /// Unpins `video` and remembers it as another song than this one, so it is never pinned
    /// again. True when anything changed.
    pub fn refuse_video(&mut self, video: &str) -> bool {
        if video.is_empty() {
            return false;
        }
        let unpinned = self.unpin_video(video);
        if self.refuses_video(video) {
            return unpinned;
        }
        self.refused_videos.push(video.to_string());
        if self.refused_videos.len() > Self::MAX_REFUSED_VIDEOS {
            self.refused_videos.remove(0);
        }
        true
    }

    /// Whether `video` was found to be another song than this one.
    pub fn refuses_video(&self, video: &str) -> bool {
        self.refused_videos.iter().any(|v| v == video)
    }

    /// Takes `others` into the refused videos, oldest first, keeping each once and at most
    /// [`Self::MAX_REFUSED_VIDEOS`].
    pub fn keep_refused_videos(&mut self, others: &[String]) {
        let mut all: Vec<String> = others.to_vec();
        for video in std::mem::take(&mut self.refused_videos) {
            if !all.contains(&video) {
                all.push(video);
            }
        }
        let excess = all.len().saturating_sub(Self::MAX_REFUSED_VIDEOS);
        self.refused_videos = all.split_off(excess);
    }

    pub fn has_artist_title(&self) -> bool {
        self.artist.as_deref().is_some_and(|a| !a.is_empty())
            && self.title.as_deref().is_some_and(|t| !t.is_empty())
    }
}

/// A length read from a downloaded file is written as Deezer's number with `ShownFromFile` beside
/// it, because a reader that knows only the first four sources refuses the whole file over a
/// number it does not know. Both that form and the plain number are read.
const SHOWN_FROM_FILE: &str = "ShownFromFile";

impl<'de> Deserialize<'de> for SoulseekRouting {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let from_file = value.get(SHOWN_FROM_FILE).and_then(serde_json::Value::as_bool) == Some(true);
        let mut routing = SoulseekRouting::deserialize(value).map_err(D::Error::custom)?;
        if from_file && routing.shown_duration_source == LengthSource::Deezer {
            routing.shown_duration_source = LengthSource::File;
        }
        Ok(routing)
    }
}

/// By hand, because the computed `HasYouTube` and `HasArtistTitle` are written too (and
/// ignored again on read). The stream facts are written only when known, so a routing without
/// them is written exactly as before.
impl Serialize for SoulseekRouting {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let stream_fields = [
            self.stream_size.is_some(),
            self.stream_size_confirmed,
            self.replaces_library_file,
            self.stream_itag.is_some(),
            self.stream_ext.is_some(),
            self.stream_bit_rate.is_some(),
            self.track_gain.is_some(),
            self.track_peak.is_some(),
            !self.refused_videos.is_empty(),
            self.video_pick > 0,
            !self.contexts.is_empty(),
            !self.heard.is_empty(),
        ]
        .into_iter()
        .filter(|known| *known)
        .count();
        let from_file = self.shown_duration_source == LengthSource::File;
        let mut s =
            serializer.serialize_struct("SoulseekRouting", 18 + stream_fields + usize::from(from_file))?;
        s.serialize_field("Kind", &self.kind)?;
        s.serialize_field("YouTubeId", &self.you_tube_id)?;
        s.serialize_field("Artist", &self.artist)?;
        s.serialize_field("Title", &self.title)?;
        s.serialize_field("Album", &self.album)?;
        s.serialize_field("Duration", &self.duration)?;
        s.serialize_field("ExternalAlbumId", &self.external_album_id)?;
        s.serialize_field("ExternalArtistId", &self.external_artist_id)?;
        s.serialize_field("Track", &self.track)?;
        s.serialize_field("DiscNumber", &self.disc_number)?;
        s.serialize_field("TotalTracks", &self.total_tracks)?;
        s.serialize_field("Isrc", &self.isrc)?;
        s.serialize_field("ShownDuration", &self.shown_duration)?;
        if from_file {
            s.serialize_field("ShownDurationSource", &LengthSource::Deezer)?;
        } else {
            s.serialize_field("ShownDurationSource", &self.shown_duration_source)?;
        }
        s.serialize_field("ExplicitContent", &self.explicit_content)?;
        s.serialize_field("Version", &self.version)?;
        if let Some(size) = &self.stream_size {
            s.serialize_field("StreamSize", size)?;
        }
        if self.stream_size_confirmed {
            s.serialize_field("StreamSizeConfirmed", &true)?;
        }
        if self.replaces_library_file {
            s.serialize_field("ReplacesLibraryFile", &true)?;
        }
        if let Some(itag) = &self.stream_itag {
            s.serialize_field("StreamItag", itag)?;
        }
        if let Some(ext) = &self.stream_ext {
            s.serialize_field("StreamExt", ext)?;
        }
        if let Some(bit_rate) = &self.stream_bit_rate {
            s.serialize_field("StreamBitRate", bit_rate)?;
        }
        if let Some(gain) = &self.track_gain {
            s.serialize_field("TrackGain", gain)?;
        }
        if let Some(peak) = &self.track_peak {
            s.serialize_field("TrackPeak", peak)?;
        }
        if !self.refused_videos.is_empty() {
            s.serialize_field("RefusedVideos", &self.refused_videos)?;
        }
        if self.video_pick > 0 {
            s.serialize_field("VideoPick", &self.video_pick)?;
        }
        if !self.contexts.is_empty() {
            s.serialize_field("Contexts", &self.contexts)?;
        }
        if !self.heard.is_empty() {
            s.serialize_field("Heard", &self.heard)?;
        }
        if from_file {
            s.serialize_field(SHOWN_FROM_FILE, &true)?;
        }
        s.serialize_field("HasYouTube", &self.has_you_tube())?;
        s.serialize_field("HasArtistTitle", &self.has_artist_title())?;
        s.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpinning_the_songs_video_forgets_it_and_its_stream() {
        let mut routing = SoulseekRouting {
            you_tube_id: Some("wrong".into()),
            stream_size: Some(2_366_424),
            track_gain: Some(-11.0),
            ..Default::default()
        };

        assert!(!routing.unpin_video("other"));
        assert_eq!(routing.you_tube_id.as_deref(), Some("wrong"));

        assert!(routing.unpin_video("wrong"));
        assert_eq!(routing.you_tube_id, None);
        assert_eq!((routing.stream_size, routing.track_gain), (None, None));
    }

    #[test]
    fn unpinning_forgets_a_length_learned_from_the_video() {
        let mut routing = SoulseekRouting {
            you_tube_id: Some("v".into()),
            shown_duration: Some(153),
            shown_duration_source: LengthSource::Video,
            duration: Some(153),
            ..Default::default()
        };
        assert!(routing.unpin_video("v"));
        assert_eq!(routing.shown_duration, None);
        assert_eq!(routing.shown_duration_source, LengthSource::None);
        assert_eq!(routing.duration, None);

        let mut catalog = SoulseekRouting {
            you_tube_id: Some("v".into()),
            shown_duration: Some(179),
            shown_duration_source: LengthSource::Deezer,
            duration: Some(179),
            ..Default::default()
        };
        assert!(catalog.unpin_video("v"));
        assert_eq!(catalog.shown_duration, Some(179));
        assert_eq!(catalog.shown_duration_source, LengthSource::Deezer);
        assert_eq!(catalog.duration, Some(179));
    }

    #[test]
    fn unpinning_puts_back_the_catalog_length() {
        let mut routing = SoulseekRouting {
            you_tube_id: Some("v".into()),
            duration: Some(171),
            shown_duration: Some(179),
            shown_duration_source: LengthSource::Deezer,
            ..Default::default()
        };
        assert!(routing.unpin_video("v"));
        assert_eq!(routing.duration, Some(179));
        assert_eq!(routing.shown_duration, Some(179));
        assert_eq!(routing.shown_duration_source, LengthSource::Deezer);
    }

    #[test]
    fn a_refused_video_is_never_pinned_again() {
        let mut routing = SoulseekRouting {
            you_tube_id: Some("v".into()),
            ..Default::default()
        };
        assert!(routing.refuse_video("v"));
        assert_eq!(routing.you_tube_id, None);
        assert!(routing.refuses_video("v"));
        assert!(!routing.pin_video("v"));
        assert_eq!(routing.you_tube_id, None);
        assert!(routing.pin_video("w"));
    }

    #[test]
    fn refused_videos_keep_each_once_and_the_newest_five() {
        let mut routing = SoulseekRouting::default();
        for video in ["a", "a", "b", "c", "d", "e", "f"] {
            routing.refuse_video(video);
        }
        assert_eq!(routing.refused_videos, ["b", "c", "d", "e", "f"]);
    }

    #[test]
    fn keeping_refused_videos_merges_oldest_first() {
        let mut routing = SoulseekRouting {
            refused_videos: vec!["c".into()],
            ..Default::default()
        };
        routing.keep_refused_videos(&["a".into(), "b".into(), "c".into()]);
        assert_eq!(routing.refused_videos, ["a", "b", "c"]);
    }

    #[test]
    fn refused_videos_are_written_only_when_there_are_some() {
        let none = serde_json::to_value(SoulseekRouting::default()).unwrap();
        assert!(none.get("RefusedVideos").is_none());

        let routing = SoulseekRouting {
            refused_videos: vec!["v".into()],
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["RefusedVideos"], serde_json::json!(["v"]));
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back.refused_videos, ["v"]);
    }

    #[test]
    fn the_video_pick_is_written_only_when_set_and_another_video_resets_it() {
        let none = serde_json::to_value(SoulseekRouting::default()).unwrap();
        assert!(none.get("VideoPick").is_none());

        let mut routing = SoulseekRouting {
            you_tube_id: Some("v".into()),
            video_pick: SoulseekRouting::VIDEO_PICK,
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["VideoPick"], serde_json::json!(3));
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back.video_pick, SoulseekRouting::VIDEO_PICK);

        assert!(!routing.pin_video("v"));
        assert_eq!(routing.video_pick, SoulseekRouting::VIDEO_PICK);
        assert!(routing.pin_video("w"));
        assert_eq!(routing.video_pick, 0);

        routing.video_pick = SoulseekRouting::VIDEO_NAMED;
        assert!(routing.unpin_video("w"));
        assert_eq!(routing.video_pick, 0);
    }

    #[test]
    fn the_computed_flags_are_written_and_ignored_on_read() {
        let routing = SoulseekRouting {
            you_tube_id: Some("abc".into()),
            artist: Some("A".into()),
            title: Some(String::new()),
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        assert!(
            json.ends_with(
                r#""ShownDurationSource":0,"ExplicitContent":null,"Version":null,"HasYouTube":true,"HasArtistTitle":false}"#
            ),
            "{json}"
        );
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back, routing);
    }

    #[test]
    fn a_length_from_a_file_is_written_in_numbers_an_older_reader_knows() {
        let routing = SoulseekRouting {
            title: Some("Angel".into()),
            shown_duration: Some(380),
            shown_duration_source: LengthSource::File,
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        assert!(json.contains(r#""ShownDurationSource":3,"#), "{json}");
        assert!(json.contains(r#""ShownFromFile":true"#), "{json}");
        assert!(!json.contains(r#""ShownDurationSource":4"#), "{json}");

        // Read back, the file's length is still the file's.
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back, routing);
        // The number an earlier build wrote reads too.
        let older: SoulseekRouting =
            serde_json::from_str(r#"{"Kind":0,"ShownDuration":380,"ShownDurationSource":4}"#).expect("reads");
        assert_eq!(older.shown_duration_source, LengthSource::File);
        // A Deezer length stays Deezer's, with no marker written.
        let deezer = SoulseekRouting {
            shown_duration_source: LengthSource::Deezer,
            ..Default::default()
        };
        let json = crate::json::to_string(&deezer);
        assert!(!json.contains("ShownFromFile"), "{json}");
        assert_eq!(
            serde_json::from_str::<SoulseekRouting>(&json).expect("reads"),
            deezer
        );
    }

    #[test]
    fn stream_facts_round_trip_and_a_routing_written_before_them_still_reads() {
        let routing = SoulseekRouting {
            you_tube_id: Some("abc".into()),
            stream_size: Some(6_755_585),
            stream_itag: Some("140".into()),
            stream_ext: Some("m4a".into()),
            stream_bit_rate: Some(129),
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        assert!(
            json.contains(r#""StreamSize":6755585,"StreamItag":"140","StreamExt":"m4a","StreamBitRate":129"#),
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<SoulseekRouting>(&json).expect("reads"),
            routing
        );
        let old: SoulseekRouting = serde_json::from_str(r#"{"Kind":0,"YouTubeId":"abc"}"#).expect("reads");
        assert_eq!(old.stream_facts(), StreamFacts::default());
    }

    #[test]
    fn pinning_another_video_forgets_the_old_streams_facts() {
        let mut routing = SoulseekRouting::default();
        assert!(routing.pin_video("a"));
        routing.merge_stream_facts(&StreamFacts {
            size: Some(10),
            itag: Some("140".into()),
            ..Default::default()
        });
        assert!(!routing.pin_video("a"));
        assert_eq!(routing.stream_size, Some(10));
        assert!(routing.pin_video("b"));
        assert_eq!(routing.you_tube_id.as_deref(), Some("b"));
        assert_eq!(routing.stream_facts(), StreamFacts::default());
    }

    #[test]
    fn a_confirmed_stream_size_is_saved_and_a_new_video_forgets_it() {
        let mut routing = SoulseekRouting {
            kind: RoutingKind::Song,
            you_tube_id: Some("vid-a".into()),
            ..Default::default()
        };
        let unconfirmed = serde_json::to_string(&routing).unwrap();
        assert!(!unconfirmed.contains("StreamSizeConfirmed"));
        routing.merge_stream_facts(&StreamFacts {
            size: Some(1000),
            size_confirmed: true,
            ..Default::default()
        });

        let json = serde_json::to_string(&routing).unwrap();
        assert!(
            json.contains(r#""StreamSize":1000,"StreamSizeConfirmed":true"#),
            "{json}"
        );
        let read: SoulseekRouting = serde_json::from_str(&json).unwrap();
        assert!(read.stream_facts().size_confirmed);

        routing.pin_video("vid-b");
        assert_eq!(routing.stream_facts(), StreamFacts::default());
    }

    use crate::fingerprint::heart_gate::HeardSource;
    use chrono::TimeZone;

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 29, 12, minute, 0).unwrap()
    }

    fn context(user: &str, client: &str, album: &str, minute: u32) -> AlbumContext {
        AlbumContext {
            user: user.into(),
            client: Some(client.into()),
            catalog_album_id: album.into(),
            album: Some(format!("Album {album}")),
            listed_at: at(minute),
            ..Default::default()
        }
    }

    fn heard(video: &str, minute: u32) -> HeardRecord {
        HeardRecord {
            heard_at: at(minute),
            source: HeardSource::Video {
                id: video.into(),
                size: Some(1000),
                content_type: "audio/mp4".into(),
                itag: None,
            },
            sample_id: None,
        }
    }

    #[test]
    fn two_users_on_two_clients_keep_a_context_each() {
        let mut routing = SoulseekRouting::default();
        assert!(routing.remember_context(context("brandon", "OctoPlayer", "302127", 1)));
        assert!(routing.remember_context(context("anna", "Feishin", "302127", 2)));
        assert!(routing.remember_context(context("brandon", "OctoPlayer", "6575789", 3)));
        assert_eq!(routing.contexts.len(), 3);

        // Listing the same album again moves it last and keeps its played time.
        assert!(routing.mark_played(
            "brandon",
            &PlayedMark {
                client: Some("octoplayer".into()),
                catalog_album_id: None,
            },
            at(4)
        ));
        assert_eq!(
            routing.contexts[2].played_at,
            Some(at(4)),
            "the client's newest listing"
        );
        assert!(routing.remember_context(context("Brandon", "OctoPlayer", "302127", 5)));
        let albums: Vec<(&str, &str)> = routing
            .contexts
            .iter()
            .map(|c| (c.user.as_str(), c.catalog_album_id.as_str()))
            .collect();
        assert_eq!(
            albums,
            [("anna", "302127"), ("brandon", "6575789"), ("Brandon", "302127")]
        );
        assert_eq!(routing.contexts[1].played_at, Some(at(4)));

        // The stream's album wins over the client's newest listing.
        assert!(routing.mark_played(
            "anna",
            &PlayedMark {
                client: Some("other".into()),
                catalog_album_id: Some("302127".into()),
            },
            at(6)
        ));
        assert_eq!(routing.contexts[0].played_at, Some(at(6)));
        // Nothing of the user's matches: nothing marked.
        assert!(!routing.mark_played("nobody", &PlayedMark::default(), at(7)));
    }

    #[test]
    fn an_older_listing_never_replaces_a_newer_one_and_eight_contexts_are_kept() {
        let mut routing = SoulseekRouting::default();
        assert!(routing.remember_context(context("brandon", "c", "a", 9)));
        let mut older = context("brandon", "c", "a", 3);
        older.album = Some("stale".into());
        assert!(!routing.remember_context(older));
        assert_eq!(routing.contexts[0].album.as_deref(), Some("Album a"));

        for minute in 10..20 {
            routing.remember_context(context("brandon", "c", &format!("x{minute}"), minute));
        }
        assert_eq!(routing.contexts.len(), SoulseekRouting::MAX_CONTEXTS);
        assert_eq!(
            routing.contexts[0].catalog_album_id, "x12",
            "the least recently listed went"
        );
    }

    #[test]
    fn one_heard_record_per_person_the_later_one_kept() {
        let mut routing = SoulseekRouting::default();
        assert!(routing.remember_heard("brandon", heard("v1", 5)));
        assert!(
            !routing.remember_heard("Brandon", heard("v0", 4)),
            "an older record never replaces"
        );
        assert_eq!(
            routing.heard_by("BRANDON").and_then(|r| r.source.video_id()),
            Some("v1")
        );

        // The sample of the same play arrives later and is kept; a write without it keeps it.
        let mut sampled = heard("v1", 5);
        sampled.sample_id = Some("s1".into());
        assert!(routing.remember_heard("brandon", sampled));
        assert!(!routing.remember_heard("brandon", heard("v1", 5)));
        assert_eq!(
            routing.heard_by("brandon").unwrap().sample_id.as_deref(),
            Some("s1")
        );

        assert!(routing.remember_heard("brandon", heard("v2", 6)));
        assert_eq!(routing.heard.len(), 1);
        for (minute, user) in (10..20).zip(["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]) {
            routing.remember_heard(user, heard("v", minute));
        }
        assert_eq!(routing.heard.len(), SoulseekRouting::MAX_HEARD);
        assert!(
            routing.heard_by("brandon").is_none(),
            "the least recently heard went"
        );
    }

    #[test]
    fn merging_keeps_the_newer_record_of_each_person() {
        let mut fresh = SoulseekRouting::default();
        fresh.remember_heard("brandon", heard("old", 1));
        fresh.remember_context(context("brandon", "c", "a", 1));
        let mut registered = SoulseekRouting::default();
        registered.remember_heard("brandon", heard("new", 2));
        registered.remember_heard("anna", heard("hers", 3));
        registered.remember_context(context("brandon", "c", "a", 2));

        fresh.merge_heard(&registered.contexts, &registered.heard);
        assert_eq!(
            fresh.heard_by("brandon").and_then(|r| r.source.video_id()),
            Some("new")
        );
        assert_eq!(
            fresh.heard_by("anna").and_then(|r| r.source.video_id()),
            Some("hers")
        );
        assert_eq!(fresh.contexts.len(), 1);
        assert_eq!(fresh.contexts[0].listed_at, at(2));
    }

    #[test]
    fn contexts_and_heard_records_are_written_only_when_there_are_some() {
        let none = crate::json::to_string(&SoulseekRouting::default());
        assert!(
            !none.contains("Contexts") && !none.contains("\"Heard\""),
            "{none}"
        );

        let mut routing = SoulseekRouting::default();
        routing.remember_context(context("brandon", "c", "302127", 1));
        routing.remember_heard("brandon", heard("v1", 2));
        let json = crate::json::to_string(&routing);
        assert!(json.contains(r#""Contexts":[{"User":"brandon""#), "{json}");
        assert!(
            json.contains(r#""Heard":[{"User":"brandon","Record":{"#),
            "{json}"
        );
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back, routing);
    }
}
