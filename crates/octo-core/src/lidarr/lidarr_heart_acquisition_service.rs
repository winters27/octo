//! The pure half of the Lidarr heart acquisition service: which of a heart's songs an imported
//! track is. The service is `octo::services::lidarr::lidarr_heart_acquisition_service`.

use super::LidarrImportedTrack;
use crate::common::SongIdentity;
use crate::models::domain::{Album, Song};

/// The album's song Lidarr imported: by title, read by [`SongIdentity`] so Deezer's
/// "Song (feat. X)" is MusicBrainz's "Song" but never its "Song (Live)", then by track number.
///
/// `match_by_number`: whether an imported file may be matched to a song by its track number. Not
/// for a track heart: its one song carries the number from whatever release it was found on,
/// often the single, where it is track 1, which on the album is another song.
pub fn match_song<'a>(
    album: &'a Album,
    track: &LidarrImportedTrack,
    match_by_number: bool,
) -> Option<&'a Song> {
    let strict = SongIdentity::strict_titles();
    let by_title: Vec<&Song> = album
        .songs
        .iter()
        .filter(|s| SongIdentity::same_title(&track.title, &s.title, Some(&strict)).is_same())
        .collect();
    if by_title.len() == 1 {
        return Some(by_title[0]);
    }
    if match_by_number && let Some(number) = track.track_number {
        let by_number: Vec<&Song> = album.songs.iter().filter(|s| s.track == Some(number)).collect();
        if by_number.len() == 1 {
            return Some(by_number[0]);
        }
    }
    by_title.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_heart_never_matches_by_track_number() {
        // Deezer found the song on its single, where it is track 1. On the album, 1 is Angel.
        let heart = Album {
            title: "Mezzanine".into(),
            artist: "Massive Attack".into(),
            songs: vec![Song {
                title: "Teardrop".into(),
                track: Some(1),
                ..Default::default()
            }],
            ..Default::default()
        };
        let angel = LidarrImportedTrack {
            id: 1,
            title: "Angel".into(),
            track_number: Some(1),
            duration_seconds: Some(380),
            has_file: true,
            path: Some("/data/music/a.flac".into()),
            size_bytes: 1,
            artist: Some("Massive Attack".into()),
            track_file_id: 101,
            quality: None,
            foreign_recording_id: None,
        };

        assert!(match_song(&heart, &angel, false).is_none());
        assert!(std::ptr::eq(
            match_song(&heart, &angel, true).expect("by number"),
            &heart.songs[0]
        ));
    }
}
