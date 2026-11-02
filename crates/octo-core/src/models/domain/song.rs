use serde::{Deserialize, Serialize};

use crate::common::song_identity::SongIdentity;
use crate::fingerprint::verification::VerificationResult;
use crate::tagging::release_fact_tags::AlbumGrouping;
use crate::tagging::tag_plan::TagPlan;

/// Represents a song (local or external)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Song {
    /// Unique ID. For external songs, prefixed with "ext-" + provider + "-" + external id
    /// Example: "ext-deezer-123456" or "local-789"
    pub id: String,

    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub album: String,
    pub album_id: Option<String>,
    /// In seconds
    pub duration: Option<i32>,
    pub track: Option<i32>,
    pub disc_number: Option<i32>,
    pub total_tracks: Option<i32>,
    pub year: Option<i32>,
    pub genre: Option<String>,

    /// The file's real format and bitrate, for a library song Navidrome described. Without them a
    /// library song was declared as FLAC at 1411 kbps whatever it was, and a strict client
    /// prepared the wrong decoder for an MP3.
    pub suffix: Option<String>,
    pub bit_rate: Option<i32>,

    /// The Soulseek peer and remote filename this came from, when it came from Soulseek.
    /// Carried so it can be written down at registration: "Wrong song" needs to know who
    /// delivered the file, and nothing else in Octo records that after the transfer ends.
    pub source_peer: Option<String>,
    pub source_file: Option<String>,

    /// For a download kept although its spectrum says it was made from a lossy file, what it
    /// was likely made from ("about 128 kbps MP3"). Written down beside SourcePeer, so a file that
    /// claims to be lossless and is not can be found again. Null for everything else.
    pub transcoded_from: Option<String>,
    pub cover_art_url: Option<String>,

    /// High-resolution cover art URL (for embedding)
    pub cover_art_url_large: Option<String>,

    /// BPM (beats per minute) if available
    pub bpm: Option<i32>,

    /// ISRC (International Standard Recording Code)
    pub isrc: Option<String>,

    /// Every ISRC the library's own entry listed, exactly as Navidrome sent them, for a library
    /// song Octo rebuilt from Navidrome's answer (radio, the Discovery blend). Carried whole so the
    /// song goes back out to the client with the codes it came in with, not with none.
    pub isrcs: Vec<String>,

    /// Full release date (format: YYYY-MM-DD)
    pub release_date: Option<String>,

    /// Album artist name (may differ from track artist)
    pub album_artist: Option<String>,

    /// Composer(s)
    pub composer: Option<String>,

    /// Album label
    pub label: Option<String>,

    /// Copyright
    pub copyright: Option<String>,

    /// Contributing artists (features, etc.)
    pub contributors: Vec<String>,

    /// Indicates whether the song is available locally or needs to be downloaded
    pub is_local: bool,

    /// External provider (deezer, spotify, etc.) - null if local
    pub external_provider: Option<String>,

    /// ID on the external provider (for downloading)
    pub external_id: Option<String>,

    /// Local file path (if available)
    pub local_path: Option<String>,

    /// Deezer explicit content lyrics value
    /// 0 = Naturally clean, 1 = Explicit, 2 = Not applicable, 3 = Clean/edited version, 6/7 = Unknown
    pub explicit_content_lyrics: Option<i32>,

    /// MusicBrainz recording id of a fingerprint-confirmed download. Written as
    /// MUSICBRAINZ_TRACKID (UFID on ID3), which is where Picard and Navidrome both keep the
    /// RECORDING id, despite the name.
    pub music_brainz_recording_id: Option<String>,

    /// The release and release group the match came from, and that group's title. Used to find
    /// the right cover; the release id is never written, because Navidrome groups albums by
    /// MUSICBRAINZ_ALBUMID before the album name.
    pub music_brainz_release_id: Option<String>,
    pub music_brainz_release_group_id: Option<String>,
    pub music_brainz_album_title: Option<String>,
    pub music_brainz_artist_ids: Vec<String>,

    /// Every credited artist, one per entry, for the multi-value ARTISTS tag. Empty when the
    /// credit is a single name. Navidrome reads it, so a collaboration is filed under each artist
    /// instead of under a new artist named after all of them (#49).
    pub artists: Vec<String>,

    /// The first credited artist, when a structured source said which one that is. Names the
    /// artist folder; see the download base's `placement::primary_credit`.
    pub primary_artist: Option<String>,

    pub is_compilation: bool,

    /// The catalog number the label gave the release.
    pub catalog_number: Option<String>,

    /// The release's barcode (UPC or EAN).
    pub barcode: Option<String>,

    /// The kind of release, lowercase, as the music database says it: "album", "single", "album; compilation".
    pub release_type: Option<String>,

    /// The release's status, lowercase: "official", "promotion", "bootleg".
    pub release_status: Option<String>,

    /// The two-letter country the release came out in ("XW" for worldwide).
    pub release_country: Option<String>,

    /// When the recording first came out, as a full date when known (YYYY-MM-DD), else a year.
    pub original_date: Option<String>,

    /// The id of this track on the chosen release, which taggers and the library server both read.
    pub music_brainz_release_track_id: Option<String>,

    /// The ids of the release's album artists, one per credit.
    pub music_brainz_album_artist_ids: Vec<String>,

    /// How many discs the release has, when the chosen release said.
    pub total_discs: Option<i32>,

    /// What the downloaded version's words are, as the advisory tag holds it: 1 explicit, 2 the
    /// clean edit, 0 neither. None when nothing that matched this exact version said.
    pub advisory: Option<i32>,

    /// The album grouping values the album this track joins already carries in the library (its
    /// release id, release date and version), copied exactly so the library server keeps the two
    /// as one album. None when there is no such album to join. Never serialized.
    #[serde(skip)]
    pub joins_album: Option<AlbumGrouping>,

    /// The fingerprint service's id for the audio, once it confirmed the recording.
    pub acoust_id: Option<String>,

    /// ReplayGain from the measured loudness: the gain in dB that brings the track to the
    /// reference level, and its peak as a fraction of full scale. Album values come from an album walk.
    pub replay_gain_track_gain_db: Option<f64>,
    pub replay_gain_track_peak: Option<f64>,
    pub replay_gain_album_gain_db: Option<f64>,
    pub replay_gain_album_peak: Option<f64>,

    /// How the download was identified and what the tags came from, carried through the download
    /// so the fetched-songs log can show it. Never serialized.
    #[serde(skip)]
    pub tag_plan: Option<Box<TagPlan>>,

    /// What AcoustID said about the downloaded file. Carried on the Song because
    /// `download_song_internal` threads ONE instance through download, tagging and placement.
    /// Never serialized.
    #[serde(skip)]
    pub verification: Option<Box<VerificationResult>>,

    /// Which radio source suggested this song, as the apps show it ("YouTube Music"). Only radio
    /// answers set it; never stored.
    #[serde(skip)]
    pub suggested_by: Option<String>,

    /// What the file layout's names were filled from when the download was placed, carried to
    /// the library's record of it. Never serialized here.
    #[serde(skip)]
    pub layout_values: Option<Box<crate::common::LayoutValues>>,
}

impl Song {
    /// What a Subsonic response lists under OpenSubsonic's `isrc`: the library's own list
    /// untouched when there is one, otherwise the song's ISRC when it is a valid one.
    pub fn isrcs_for_clients(&self) -> Vec<String> {
        if !self.isrcs.is_empty() {
            return self.isrcs.clone();
        }
        SongIdentity::normalize_isrc(self.isrc.as_deref().unwrap_or(""))
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isrcs_for_clients_prefers_the_library_list_then_a_valid_own_code() {
        let listed = Song {
            isrcs: vec!["us-rc1-76-07839".into(), "junk".into()],
            isrc: Some("GBAHT1600302".into()),
            ..Default::default()
        };
        assert_eq!(listed.isrcs_for_clients(), ["us-rc1-76-07839", "junk"]);

        let own = Song {
            isrc: Some("us-rc1-76-07839".into()),
            ..Default::default()
        };
        assert_eq!(own.isrcs_for_clients(), ["USRC17607839"]);

        let invalid = Song {
            isrc: Some("junk".into()),
            ..Default::default()
        };
        assert!(invalid.isrcs_for_clients().is_empty());
        assert!(Song::default().isrcs_for_clients().is_empty());
    }

    #[test]
    fn serializes_every_field_in_declaration_order_but_the_ignored_two() {
        let song = Song {
            id: "ext-deezer-1".into(),
            title: "Hoppípolla".into(),
            tag_plan: Some(Box::default()),
            verification: Some(Box::default()),
            ..Default::default()
        };
        let json = crate::json::to_string(&song);
        assert!(
            json.starts_with(
                r#"{"Id":"ext-deezer-1","Title":"Hopp\u00EDpolla","Artist":"","ArtistId":null,"#
            )
        );
        assert!(
            json.ends_with(r#""ReplayGainAlbumGainDb":null,"ReplayGainAlbumPeak":null}"#),
            "{json}"
        );
        assert!(!json.contains("TagPlan") && !json.contains("Verification") && !json.contains("JoinsAlbum"));
        for name in [
            "\"BitRate\"",
            "\"CoverArtUrlLarge\"",
            "\"MusicBrainzReleaseTrackId\"",
            "\"AcoustId\"",
            "\"IsCompilation\"",
            "\"TotalDiscs\"",
            "\"Advisory\"",
        ] {
            assert!(json.contains(name), "{name}");
        }

        let back: Song = serde_json::from_str(&json).expect("reads back");
        assert_eq!(
            crate::json::to_string(&back),
            crate::json::to_string(&Song {
                tag_plan: None,
                verification: None,
                ..song
            })
        );
    }
}
