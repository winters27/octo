//! The pure half of the Lidarr heart acquisition service: which of a heart's songs an imported
//! track is, the order a hearted song's candidates are judged in, and what Lidarr's health says
//! of its indexers. The service is `octo::services::lidarr::lidarr_heart_acquisition_service`.

use super::LidarrImportedTrack;
use super::lidarr_client::LidarrHealthCheck;
use crate::common::SongIdentity;
use crate::common::dotnet;
use crate::fingerprint::verification::core_title;
use crate::models::domain::{Album, Song};

/// The order a hearted song's candidates are judged in, as indexes into `tracks` (each an imported
/// track's title and the recording Lidarr matched it to): only the tracks whose core title is the
/// song's ("Help! (Take 1)" is one of "Help!"'s), those Lidarr matched to one of `recordings`
/// first, then those whose whole title reads as the song's, then the rest, each group in the order
/// given. Lidarr's own match only orders them: every candidate is still judged by its sound.
pub fn candidate_order(
    song_title: &str,
    recordings: &[String],
    tracks: &[(&str, Option<&str>)],
) -> Vec<usize> {
    let core = core_title(song_title);
    if core.is_empty() {
        return Vec::new();
    }
    let strict = SongIdentity::strict_titles();
    let mut ranked: Vec<(u8, usize)> = tracks
        .iter()
        .enumerate()
        .filter(|(_, (title, _))| core_title(title) == core)
        .map(|(index, (title, recording))| {
            let matched =
                recording.is_some_and(|id| recordings.iter().any(|r| dotnet::eq_ignore_case(r, id)));
            let rank = if matched {
                0
            } else if SongIdentity::same_title(title, song_title, Some(&strict)).is_same() {
                1
            } else {
                2
            };
            (rank, index)
        })
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, index)| index).collect()
}

/// What Lidarr's health says of its indexers once a search of Octo's grabbed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexerHealth {
    /// Nothing is wrong with searching: the search just found nothing.
    Healthy,
    /// The indexers that search are failing: a heart waits for them.
    Failing,
    /// No indexer has automatic search on, so Lidarr cannot search for anything.
    NoneCanSearch,
}

/// `IndexerSearchCheck`'s wiki fragment when every indexer that searches is failing.
const UNAVAILABLE: &str = "indexers-are-unavailable-due-to-failures";
/// `IndexerSearchCheck`'s wiki fragment when no indexer has automatic search on.
const NO_AUTOMATIC_SEARCH: &str = "no-indexers-available-with-automatic-search-enabled-lidarr-will-not-provide-any-automatic-search-results";

/// Reads Lidarr's health checks for its indexers: failing on `IndexerStatusCheck` or
/// `IndexerLongTermStatusCheck` at error, or `IndexerSearchCheck` naming indexers unavailable
/// through failures; unable to search when `IndexerSearchCheck` says no indexer searches
/// automatically (which outweighs failures, since no recovery makes such a search run); healthy
/// otherwise, an indexer that only searches interactively included.
pub fn indexer_health(checks: &[LidarrHealthCheck]) -> IndexerHealth {
    let fragment = |check: &LidarrHealthCheck, wanted: &str| {
        check
            .wiki_url
            .as_deref()
            .is_some_and(|url| url.to_ascii_lowercase().contains(wanted))
    };
    let source = |check: &LidarrHealthCheck, name: &str| dotnet::eq_ignore_case(check.source.trim(), name);
    if checks
        .iter()
        .any(|check| source(check, "IndexerSearchCheck") && fragment(check, NO_AUTOMATIC_SEARCH))
    {
        return IndexerHealth::NoneCanSearch;
    }
    let failing = checks.iter().any(|check| {
        ((source(check, "IndexerStatusCheck") || source(check, "IndexerLongTermStatusCheck"))
            && dotnet::eq_ignore_case(check.kind.trim(), "error"))
            || (source(check, "IndexerSearchCheck") && fragment(check, UNAVAILABLE))
    });
    if failing {
        IndexerHealth::Failing
    } else {
        IndexerHealth::Healthy
    }
}

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

    /// Every track whose core title is the song's is a candidate: the one Lidarr matched to a
    /// heard recording first, then the one whose whole title reads as the song's, then a take of
    /// it. Another song is never one.
    #[test]
    fn a_hearted_songs_candidates_are_its_core_titles_by_hint_then_title() {
        let tracks = [
            ("Help! (Take 1)", Some("rec-take-1")),
            ("Yesterday", Some("rec-2009")),
            ("Help!", Some("rec-2015")),
            ("Help!", Some("REC-2009")),
        ];

        assert_eq!(
            candidate_order("Help!", &["rec-2009".to_string()], &tracks),
            [3, 2, 0]
        );
        assert_eq!(candidate_order("Help!", &[], &tracks), [2, 3, 0]);
        assert!(candidate_order("Something", &[], &tracks).is_empty());
    }

    fn check(source: &str, kind: &str, fragment: &str) -> LidarrHealthCheck {
        LidarrHealthCheck {
            source: source.into(),
            kind: kind.into(),
            message: String::new(),
            wiki_url: Some(format!("https://wiki.servarr.com/lidarr/system#{fragment}")),
        }
    }

    /// One reading per case Lidarr's health checks give.
    #[test]
    fn lidarrs_health_reads_as_failing_unable_to_search_or_healthy() {
        assert_eq!(
            indexer_health(&[check(
                "IndexerStatusCheck",
                "error",
                "indexers-are-unavailable-due-to-failures"
            )]),
            IndexerHealth::Failing
        );
        assert_eq!(
            indexer_health(&[check("IndexerLongTermStatusCheck", "error", "x")]),
            IndexerHealth::Failing
        );
        assert_eq!(
            indexer_health(&[check(
                "IndexerSearchCheck",
                "warning",
                "indexers-are-unavailable-due-to-failures"
            )]),
            IndexerHealth::Failing
        );
        assert_eq!(
            indexer_health(&[
                check("IndexerStatusCheck", "error", "x"),
                check("IndexerSearchCheck", "warning", NO_AUTOMATIC_SEARCH),
            ]),
            IndexerHealth::NoneCanSearch
        );
        assert_eq!(
            indexer_health(&[check(
                "IndexerSearchCheck",
                "warning",
                "no-indexers-available-with-interactive-search-enabled"
            )]),
            IndexerHealth::Healthy
        );
        // A warning is not a failure of every indexer.
        assert_eq!(
            indexer_health(&[check("IndexerStatusCheck", "warning", "x")]),
            IndexerHealth::Healthy
        );
        assert_eq!(indexer_health(&[]), IndexerHealth::Healthy);
    }
}
