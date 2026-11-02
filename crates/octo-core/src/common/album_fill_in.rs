//! Which catalog album may fill in a library album's missing songs on getAlbum. A name is not
//! an identity: "Nightcore" by "Nightcore" names one 3-song album on Deezer and a listener's own
//! 388-song collection in the library, with no song in common (octo-player#1). Navidrome's
//! getAlbum carries no album id the catalog shares (no barcode, and a MusicBrainz id only for
//! files tagged that way), so the albums are compared by what is on them, as Roon does: the
//! catalog album must hold most of the library album's songs, each known by its ISRC or by its
//! title and length.

use serde_json::Value;

use crate::common::dotnet;
use crate::common::song_identity::SongIdentity;
use crate::models::domain::{Album, Song};

/// A library song as Navidrome's getAlbum lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryTrack {
    pub title: Option<String>,
    pub duration: Option<i32>,
    /// Every ISRC the song carries; a null entry is kept as `None`.
    pub isrcs: Vec<Option<String>>,
}

impl LibraryTrack {
    pub fn new<S: Into<String>>(
        title: Option<&str>,
        duration: Option<i32>,
        isrcs: impl IntoIterator<Item = S>,
    ) -> Self {
        Self {
            title: title.map(str::to_string),
            duration,
            isrcs: isrcs.into_iter().map(|isrc| Some(isrc.into())).collect(),
        }
    }
}

/// The album fill-in rules, as associated functions on an empty struct.
pub struct AlbumFillIn;

impl AlbumFillIn {
    /// Two lengths further apart than this, in seconds, are two recordings.
    pub const LENGTH_SLACK_SECONDS: i32 = 5;

    /// A song from Navidrome's JSON, as the merge holds it: title, duration and isrc.
    ///
    /// A JSON number that fits an `int` is taken as the duration as it is, any other number is
    /// rounded half to even, and anything else reads as no duration.
    pub fn from_subsonic(song: Option<&Value>) -> LibraryTrack {
        let Some(Value::Object(dict)) = song else {
            return LibraryTrack::default();
        };
        let title = dict.get("title").and_then(to_text);
        let duration = dict.get("duration").and_then(|d| match d {
            Value::Number(n) => match n.as_i64() {
                Some(i) => i32::try_from(i).ok(),
                None => n.as_f64().map(|x| dotnet::round(x, 0) as i32),
            },
            _ => None,
        });
        // OpenSubsonic sends a list; an older server may send one string.
        let isrcs = match dict.get("isrc") {
            Some(Value::String(one)) => vec![Some(one.clone())],
            Some(Value::Array(many)) => many.iter().map(to_text).collect(),
            _ => Vec::new(),
        };
        LibraryTrack {
            title,
            duration,
            isrcs,
        }
    }

    /// The search hits that could be the library album, best first: the same artist and title,
    /// then a looser match where one title or artist name holds the other. A hit with too few
    /// tracks to hold half of the library album's `library_songs` is left out before its
    /// tracklist is ever fetched.
    ///
    /// An album's artist is never null, and an empty one matches neither rule below.
    pub fn candidates<'a>(
        hits: &'a [Album],
        artist_name: &str,
        album_name: &str,
        library_songs: i32,
    ) -> Vec<&'a Album> {
        let list: Vec<(usize, &Album)> = hits
            .iter()
            .enumerate()
            .filter(|(_, h)| !too_small(h.song_count, library_songs))
            .collect();
        let wanted_title = SongIdentity::key(album_name);
        let wanted_artist = SongIdentity::key(artist_name);

        let exact = list.iter().filter(|(_, h)| {
            SongIdentity::same_artist_name(&h.artist, artist_name)
                && SongIdentity::key(&h.title) == wanted_title
        });
        let loose = list.iter().filter(|(_, h)| {
            let title = SongIdentity::key(&h.title);
            SongIdentity::key(&h.artist).contains(&wanted_artist)
                && !title.is_empty()
                && !wanted_title.is_empty()
                && (title.contains(&wanted_title) || wanted_title.contains(&title))
        });
        // The same hit once, wherever it matched first.
        let mut seen = Vec::new();
        let mut answer = Vec::new();
        for (index, hit) in exact.chain(loose) {
            if !seen.contains(index) {
                seen.push(*index);
                answer.push(*hit);
            }
        }
        answer
    }

    /// Whether a catalog album holds the library album: at least one song in common, and at
    /// least half of the library album's songs on it. A library album with more of its own
    /// songs than the catalog album shares is another record that happens to have the name.
    pub fn holds(library: &[LibraryTrack], catalog: &[Song]) -> bool {
        let songs = distinct(library);
        if songs.is_empty() {
            return false;
        }
        let shared = songs
            .iter()
            .filter(|l| catalog.iter().any(|c| Self::same_recording(l, c)))
            .count();
        shared > 0 && shared * 2 >= songs.len()
    }

    /// Which catalog rows the library already has, so they are not offered as missing. A library
    /// song owns every row that is its recording ([`Self::same_recording`]), so a recording the
    /// album lists twice stays out twice. A library song that is no row's recording owns the
    /// first row not yet owned with its title ("Song (feat. X)" and "Song" alike, never "Song
    /// (Live)"), so one file never hides another recording of its title on the album.
    pub fn owned_rows(library: &[LibraryTrack], catalog: &[Song]) -> Vec<bool> {
        let mut owned = vec![false; catalog.len()];
        let mut by_title = Vec::new();
        for track in library {
            let mut any = false;
            for (at, row) in catalog.iter().enumerate() {
                if Self::same_recording(track, row) {
                    owned[at] = true;
                    any = true;
                }
            }
            if !any {
                by_title.push(track);
            }
        }
        for track in by_title {
            let key = SongIdentity::title_key(track.title.as_deref().unwrap_or(""));
            if key.is_empty() {
                continue;
            }
            if let Some(at) = (0..catalog.len())
                .find(|&at| !owned[at] && SongIdentity::title_key(&catalog[at].title) == key)
            {
                owned[at] = true;
            }
        }
        owned
    }

    /// Up to `max` of the library album's ISRCs, each once, in the album's order.
    pub fn isrcs(library: &[LibraryTrack], max: usize) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        for isrc in library.iter().flat_map(|track| present(&track.isrcs)) {
            let Some(code) = SongIdentity::normalize_isrc(isrc) else {
                continue;
            };
            if found.len() >= max {
                break;
            }
            if !found.contains(&code) {
                found.push(code);
            }
        }
        found
    }

    /// One recording on both sides: the same ISRC, or the same title at nearly the same length.
    /// An ISRC that differs proves nothing, as a reissue can be given a new one.
    pub fn same_recording(library: &LibraryTrack, catalog: &Song) -> bool {
        if SongIdentity::shares_isrc(present(&library.isrcs), catalog_isrcs(catalog)) {
            return true;
        }
        let key = SongIdentity::title_key(library.title.as_deref().unwrap_or(""));
        !key.is_empty()
            && key == SongIdentity::title_key(&catalog.title)
            && lengths_agree(library.duration, catalog.duration)
    }

    /// How many songs the library album holds, each counted once.
    pub fn count_songs(library: &[LibraryTrack]) -> usize {
        distinct(library).len()
    }
}

/// A JSON value as text: a string as it is, a null as nothing, anything else as its JSON text.
fn to_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn present(isrcs: &[Option<String>]) -> impl Iterator<Item = &str> {
    isrcs.iter().flatten().map(String::as_str)
}

fn too_small(catalog_songs: Option<i32>, library_songs: i32) -> bool {
    matches!(catalog_songs, Some(songs) if songs > 0 && songs * 2 < library_songs)
}

fn lengths_agree(a: Option<i32>, b: Option<i32>) -> bool {
    match (a.filter(|a| *a > 0), b.filter(|b| *b > 0)) {
        (Some(a), Some(b)) => (a - b).abs() <= AlbumFillIn::LENGTH_SLACK_SECONDS,
        _ => true,
    }
}

fn catalog_isrcs(song: &Song) -> impl Iterator<Item = &str> {
    song.isrc
        .as_deref()
        .into_iter()
        .chain(song.isrcs.iter().map(String::as_str))
}

/// The library's songs once each, a FLAC and an MP3 of one song counting once, with the ISRCs of
/// every copy.
fn distinct(library: &[LibraryTrack]) -> Vec<LibraryTrack> {
    let mut groups: Vec<(String, Vec<&LibraryTrack>)> = Vec::new();
    for track in library {
        let key = SongIdentity::title_key(track.title.as_deref().unwrap_or(""));
        if key.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(track),
            None => groups.push((key, vec![track])),
        }
    }
    groups
        .into_iter()
        .map(|(_, members)| LibraryTrack {
            title: members[0].title.clone(),
            // The first positive length, else none.
            duration: members.iter().filter_map(|l| l.duration).find(|d| *d > 0),
            isrcs: members.iter().flat_map(|l| l.isrcs.iter().cloned()).collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn lib(title: &str, duration: Option<i32>, isrcs: &[&str]) -> LibraryTrack {
        LibraryTrack::new(Some(title), duration, isrcs.iter().copied())
    }

    fn cat(title: &str, duration: Option<i32>, isrc: Option<&str>) -> Song {
        Song {
            title: title.into(),
            duration,
            isrc: isrc.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn holds_one_owned_song_of_a_full_album() {
        assert!(AlbumFillIn::holds(
            &[lib("My Bad", Some(180), &[])],
            &[
                cat("Intro", Some(60), None),
                cat("My Bad", Some(181), None),
                cat("Outro", Some(90), None)
            ]
        ));
    }

    #[test]
    fn holds_not_an_album_that_only_shares_the_name() {
        assert!(!AlbumFillIn::holds(
            &[
                lib("Believer (Rock Version)", Some(216), &[]),
                lib("Mi Mi Mi (Rock Version)", Some(168), &[]),
                lib("MAGIC", Some(134), &[])
            ],
            &[
                cat("Love Tonight (Nightcore Remix)", Some(142), None),
                cat("Angel (Nightcore Remix)", Some(159), None),
                cat("Sad Songs & Depression (Nightcore Remix)", Some(280), None)
            ]
        ));
    }

    #[test]
    fn holds_not_when_most_of_the_library_album_is_elsewhere() {
        // One title in common by chance, out of many: another record.
        let library: Vec<LibraryTrack> = (1..=20).map(|i| lib(&format!("Song {i}"), None, &[])).collect();
        assert!(!AlbumFillIn::holds(
            &library,
            &[
                cat("Song 1", None, None),
                cat("Other A", None, None),
                cat("Other B", None, None)
            ]
        ));
    }

    #[test]
    fn holds_a_deluxe_library_album_against_the_standard_edition() {
        // Twelve songs, ten on the standard edition: the same record with two bonus songs.
        let library: Vec<LibraryTrack> = (1..=12)
            .map(|i| lib(&format!("Song {i}"), Some(200 + i), &[]))
            .collect();
        let catalog: Vec<Song> = (1..=10)
            .map(|i| cat(&format!("Song {i}"), Some(200 + i), None))
            .collect();
        assert!(AlbumFillIn::holds(&library, &catalog));
    }

    #[test]
    fn holds_the_same_title_at_another_length_is_another_recording() {
        // "Intro" by name only: a 40 s intro is not the 4 minute one.
        assert!(!AlbumFillIn::holds(
            &[lib("Intro", Some(40), &[])],
            &[cat("Intro", Some(240), None), cat("Other", Some(200), None)]
        ));
        assert!(AlbumFillIn::holds(
            &[lib("Intro", Some(40), &[])],
            &[cat("Intro", Some(43), None), cat("Other", Some(200), None)]
        ));
    }

    #[test]
    fn holds_an_isrc_matches_whatever_the_title_says() {
        assert!(AlbumFillIn::holds(
            &[lib("Song - 2011 Remaster", Some(200), &["GB-AAA-00-00001"])],
            &[
                cat("Song", Some(230), Some("GBAAA0000001")),
                cat("Other", Some(200), None)
            ]
        ));
    }

    #[test]
    fn holds_a_length_unknown_on_one_side_does_not_count_against() {
        assert!(AlbumFillIn::holds(
            &[lib("Song", None, &[])],
            &[cat("Song", Some(200), None), cat("Other", Some(200), None)]
        ));
    }

    #[test]
    fn holds_counts_a_guest_credit_as_the_same_song_but_not_a_live_take() {
        assert!(AlbumFillIn::holds(
            &[lib("Song (feat. Guest)", None, &[])],
            &[cat("Song", None, None), cat("Other", None, None)]
        ));
        assert!(!AlbumFillIn::holds(
            &[lib("Song (Live)", None, &[])],
            &[cat("Song", None, None), cat("Other", None, None)]
        ));
    }

    #[test]
    fn holds_counts_two_copies_of_one_song_once() {
        // A FLAC and an MP3 of "Song", and "Extra": one of two songs shared, which is half.
        assert!(AlbumFillIn::holds(
            &[
                lib("Song", None, &[]),
                lib("Song", None, &[]),
                lib("Extra", None, &[])
            ],
            &[cat("Song", None, None), cat("Other", None, None)]
        ));
    }

    #[test]
    fn holds_nothing_for_an_empty_library_album() {
        assert!(!AlbumFillIn::holds(&[], &[cat("Song", None, None)]));
        assert!(!AlbumFillIn::holds(
            &[LibraryTrack::default(), lib("", None, &[])],
            &[cat("Song", None, None)]
        ));
    }

    #[test]
    fn owned_by_isrc_or_title_so_no_song_is_offered_twice() {
        let library = [
            lib("Song - 2011 Remaster", Some(200), &["GBAAA0000001"]),
            lib("Other", Some(100), &[]),
        ];

        let owned = AlbumFillIn::owned_rows(
            &library,
            &[
                cat("Song", Some(230), Some("GBAAA0000001")),
                cat("Other", Some(300), None),
                cat("New Song", Some(200), None),
            ],
        );
        assert_eq!(owned, [true, true, false]);
    }

    #[test]
    fn owned_rows_a_file_owns_its_own_recording_and_never_hides_another_of_its_title() {
        // The library's "Stay" is 382 s; the album lists it, and another "Stay" at 412 s.
        let owned = AlbumFillIn::owned_rows(
            &[lib("Stay", Some(382), &[])],
            &[
                cat("Stay", Some(382), None),
                cat("Other", Some(200), None),
                cat("Stay", Some(412), None),
            ],
        );
        assert_eq!(owned, [true, false, false]);
        // Only "Stay" 412 s on the album: the file owns it by its title, as before.
        let owned = AlbumFillIn::owned_rows(
            &[lib("Stay", Some(382), &[])],
            &[cat("Stay", Some(412), None), cat("Other", Some(200), None)],
        );
        assert_eq!(owned, [true, false]);
    }

    #[test]
    fn owned_rows_a_recording_the_album_lists_twice_stays_out_twice() {
        let owned = AlbumFillIn::owned_rows(
            &[lib("Theme", Some(200), &["GBAAA0000001"])],
            &[
                cat("Theme", Some(200), Some("GBAAA0000001")),
                cat("Theme (Reprise)", Some(90), None),
                cat("Theme", Some(200), Some("GBAAA0000001")),
            ],
        );
        assert_eq!(owned, [true, false, true]);
    }

    #[test]
    fn isrcs_are_the_albums_own_each_once_and_bounded() {
        let library = [
            lib("A", None, &["GB-AAA-00-00001", "GBAAA0000001"]),
            lib("B", None, &["GBAAA0000002"]),
            lib("C", None, &["GBAAA0000003"]),
        ];
        assert_eq!(AlbumFillIn::isrcs(&library, 2), ["GBAAA0000001", "GBAAA0000002"]);
        assert!(AlbumFillIn::isrcs(&[lib("A", None, &[])], 3).is_empty());
    }

    #[test]
    fn from_subsonic_reads_navidromes_song() {
        let song = json!({"title": "One", "duration": 100, "isrc": ["GBAAA0000001"]});

        let track = AlbumFillIn::from_subsonic(Some(&song));

        assert_eq!(track.title.as_deref(), Some("One"));
        assert_eq!(track.duration, Some(100));
        assert_eq!(track.isrcs, [Some("GBAAA0000001".to_string())]);
        assert_eq!(
            AlbumFillIn::from_subsonic(Some(&json!({"isrc": "GBAAA0000001"}))).isrcs,
            [Some("GBAAA0000001".to_string())]
        );
        assert!(AlbumFillIn::from_subsonic(None).isrcs.is_empty());
    }

    fn album(id: &str, title: &str, artist: &str, song_count: Option<i32>) -> Album {
        Album {
            id: id.into(),
            title: title.into(),
            artist: artist.into(),
            song_count,
            ..Default::default()
        }
    }

    #[test]
    fn candidates_same_artist_and_title_first_then_looser_matches() {
        let hits = [
            album("deluxe", "Test Album (Deluxe)", "Test Artist", None),
            album("other", "Test Album", "Someone Else", None),
            album("exact", "Test Album", "Test Artist", None),
            // A hit with no artist is left out.
            album("unknown", "Test Album", "", None),
        ];

        let ids: Vec<&str> = AlbumFillIn::candidates(&hits, "Test Artist", "Test Album", 2)
            .iter()
            .map(|a| a.id.as_str())
            .collect();

        assert_eq!(ids, ["exact", "deluxe"]);
    }

    #[test]
    fn candidates_leaves_out_an_album_too_small_to_hold_the_librarys_songs() {
        // 388 songs cannot be half on a 3-track album: turned down before its tracks are fetched.
        let hits = [
            album("ep", "Nightcore", "Nightcore", Some(3)),
            album("unknown", "Nightcore", "Nightcore", None),
        ];
        let ids = |library_songs| -> Vec<String> {
            AlbumFillIn::candidates(&hits, "Nightcore", "Nightcore", library_songs)
                .iter()
                .map(|a| a.id.clone())
                .collect()
        };

        assert_eq!(ids(388), ["unknown"]);
        assert_eq!(ids(6), ["ep", "unknown"]);
    }

    /// A fractional duration rounds half to even, and the song count ignores blanks.
    #[test]
    fn a_fractional_duration_rounds_and_blank_titles_are_not_songs() {
        assert_eq!(
            AlbumFillIn::from_subsonic(Some(&json!({"duration": 100.5}))).duration,
            Some(100)
        );
        assert_eq!(
            AlbumFillIn::count_songs(&[lib("A", None, &[]), lib("a", None, &[]), lib(" ", None, &[])]),
            1
        );
    }
}
