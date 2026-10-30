//! Download matching, from two reported problems. "Songs with (Radio edit) only download
//! radio edit songs": the title's version tag was read as words the file name had to carry, so
//! "Too Close (Radio Edit)" took only files that spelled out "radio edit", and the same rule made
//! "Take Care (feat. Rihanna)" refuse every file not naming Rihanna. And "Take Care downloaded
//! MP3, Find better quality found FLAC": the star's search stopped at 500 files after one second,
//! while Better quality read 2,000. Every file name below is shaped like a real Soulseek share.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use octo_core::common::SongIdentity;
use octo_core::common::version_variant;
use octo_core::fingerprint::recording_identity::Takes;
use octo_core::fingerprint::{AcoustIdLookup, AcoustIdRecording, AcoustIdResult, VerificationVerdict};
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, DownloadSource, SettingsStore, SoulseekSettings, SubsonicSettings};
use octo_core::soulseek::search_profile::SONG_FILE_LIMIT;
use octo_core::soulseek::soulseek_client::hit_limit;
use octo_core::soulseek::soulseek_download_service::{
    CandidateWant, adds_version, filename_plausibly_matches_title, from_live_folder, from_version_folder,
    name_title, rank,
};
use octo_core::soulseek::{
    AlbumFolderPicker, AlbumTrack, RoutingKind, SearchProfile, SoulseekFileHit, SoulseekRouting,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::soulseek_candidates::{self, Judged};
use super::*;
use crate::services::common::song_finder::{FindTarget, FoundCopy, SongFinder};
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::common::{DownloadConcurrency, DownloadCore, DownloadServices};
use crate::services::common::{DownloadPicks, PickedCopy};
use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
use crate::services::i_download_service::IDownloadService;
use crate::services::library::ReplacementHandoff;
use crate::services::local::DownloadHistoryService;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::notifications::NotificationService;
use crate::services::soulseek::soulseek_client::Timings;
use crate::services::subsonic::NavidromeIdentityService;
use futures::FutureExt;
use octo_media::audio::AudioFingerprinter;
use octo_media::tags::KeptIdentity;

fn plausible(file: &str, title: &str) -> bool {
    filename_plausibly_matches_title(file, title, false)
}

// ---- The name rule reads the title, not its tags ------------------------------------------

#[test]
fn a_guest_credit_is_not_words_the_file_must_carry() {
    for file in [
        r"Music\Drake\Take Care (Deluxe) (2011)\04 - Take Care.flac",
        r"Music\Drake\Take Care\04 - Drake ft. Rihanna - Take Care.flac",
        r"VA - Now That's What I Call Music! 81\CD1\17. Drake Feat. Rihanna - Take Care.flac",
    ] {
        assert!(plausible(file, "Take Care (feat. Rihanna)"), "{file}");
        assert!(
            filename_plausibly_matches_title(file, "Take Care (feat. Rihanna)", true),
            "{file} (phrase)"
        );
    }
}

#[test]
fn the_name_title_drops_only_guests_and_version_tags() {
    for (title, expected) in [
        ("Too Close (Radio Edit)", "Too Close"),
        ("Too Close - Radio Edit", "Too Close"),
        ("Teardrop - Remastered 2011", "Teardrop"),
        ("Take Care (feat. Rihanna)", "Take Care"),
        ("Blue (Da Ba Dee)", "Blue (Da Ba Dee)"),
        ("Song (Pt. 2)", "Song (Pt. 2)"),
        ("(Exchange)", "(Exchange)"),
    ] {
        assert_eq!(name_title(title), expected, "{title}");
    }
}

#[test]
fn a_guest_credit_inside_a_version_tag_leaves_the_bare_title() {
    for title in [
        "Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)",
        "Get Lucky [Radio Edit - feat. Pharrell Williams and Nile Rodgers]",
        "Get Lucky (Radio Edit, feat. Pharrell Williams & Nile Rodgers)",
        "Get Lucky - Radio Edit - feat. Pharrell Williams and Nile Rodgers",
        "Get Lucky (feat. Pharrell Williams and Nile Rodgers) - Radio Edit",
        "Get Lucky feat. Pharrell Williams and Nile Rodgers (Radio Edit)",
    ] {
        assert_eq!(name_title(title), "Get Lucky", "{title}");
        assert!(
            plausible(r"Daft Punk - Get Lucky (2013) [FLAC]\01. Get Lucky.flac", title),
            "{title}"
        );
    }
}

#[test]
fn a_guest_credit_inside_a_version_tag_keeps_the_version() {
    assert_eq!(
        SongIdentity::strip_features("Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)"),
        "Get Lucky (Radio Edit)"
    );
    assert_eq!(
        SongIdentity::strip_features("Get Lucky feat. Pharrell Williams (Radio Edit)"),
        "Get Lucky (Radio Edit)"
    );
    assert_eq!(
        SongIdentity::strip_features("Get Lucky - Radio Edit - feat. Pharrell Williams"),
        "Get Lucky - Radio Edit"
    );
}

fn file_hit(user: &str, file: &str, seconds: i32, extension: &str) -> SoulseekFileHit {
    SoulseekFileHit {
        username: user.into(),
        filename: file.into(),
        size: 30_000_000,
        length: Some(seconds),
        extension: extension.into(),
        ..Default::default()
    }
}

fn want(title: &str, seconds: Option<i32>, album: Option<&str>, artist: Option<&str>) -> CandidateWant {
    CandidateWant::new(title, seconds).album(album).artist(artist)
}

fn rank_all(hits: &[SoulseekFileHit], want: &CandidateWant) -> Vec<SoulseekFileHit> {
    rank(hits, want, "flac", 0, |_| true)
}

#[test]
fn the_plain_single_is_taken_for_the_radio_edit_with_a_guest_credit() {
    // The live refusal: a plainly named single of 4:07 for the 4:08 radio edit.
    let single = file_hit(
        "peer",
        r"Music\Daft Punk - Get Lucky (2013) [FLAC]\01. Get Lucky.flac",
        247,
        "flac",
    );

    let ranked = rank_all(
        std::slice::from_ref(&single),
        &want(
            "Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)",
            Some(248),
            Some("Get Lucky"),
            Some("Daft Punk"),
        ),
    );

    assert_eq!(ranked, vec![single]);
}

// ---- Mash-ups never pass as the song, nor as its radio edit ------------------------------

pub(crate) const LUCKY_RADIO_EDIT: &str = "Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)";

pub(crate) fn lucky(file: &str, seconds: i32, extension: &str) -> SoulseekFileHit {
    file_hit("peer", file, seconds, extension)
}

#[test]
fn a_mash_up_named_after_the_song_is_not_taken_for_its_radio_edit() {
    // The live miss: 4:13 against the edit's 4:08, kept as fitting.
    let mash_up = lucky(
        r"Music\dk darkly\Bootlegs 2013\04 Get Lucky x Alakazam!.flac",
        253,
        "flac",
    );
    let single = lucky(
        r"Music\Daft Punk - Get Lucky (2013) [FLAC]\01. Get Lucky.flac",
        247,
        "flac",
    );

    let ranked = rank_all(
        &[mash_up.clone(), single.clone()],
        &want(LUCKY_RADIO_EDIT, Some(248), Some("Get Lucky"), Some("Daft Punk")),
    );

    assert_eq!(ranked, vec![single]);
    let settings = SoulseekSettings::default();
    assert_eq!(
        soulseek_candidates::why_not(
            &mash_up,
            judged(
                LUCKY_RADIO_EDIT,
                Some("Get Lucky"),
                Some(248),
                &settings,
                Some("Daft Punk")
            )
        )
        .as_deref(),
        Some("A mash-up or medley of this song with another")
    );
}

fn judged<'a>(
    title: &'a str,
    album: Option<&'a str>,
    duration: Option<i32>,
    settings: &'a SoulseekSettings,
    artist: Option<&'a str>,
) -> Judged<'a> {
    Judged {
        title,
        album,
        duration,
        settings,
        rejected: None,
        remembers_rejections: false,
        artist,
    }
}

#[test]
fn a_mash_up_is_another_version() {
    for file in [
        r"Music\Mashups\Daft Punk VS Michael Jackson - Get Lucky-Billie Jean.mp3",
        r"Music\dk darkly\04 Get Lucky x Alakazam!.flac",
        r"Music\Mashups\Get Lucky vs. Blurred Lines.flac",
        r"Music\Mashups\Get Lucky versus Lose Yourself to Dance.flac",
        r"Music\Mashups\Daft Punk - Get Lucky (Mashup).flac",
        r"Music\Mashups\Daft Punk - Get Lucky (Mash-Up with Uptown Funk).flac",
        r"Music\Pentatonix\Daft Punk Medley.flac",
        r"Music\Mixes\2013 Summer Megamix\05 - Get Lucky.flac",
        r"Music\Blends\Get Lucky (Blend).flac",
        r"Music\Bootlegs\Get Lucky (Alakazam Bootleg).flac",
    ] {
        let title = if file.contains("Medley") {
            "Daft Punk"
        } else {
            "Get Lucky"
        };
        assert!(
            adds_version(file, title, Some("Daft Punk"))
                || from_version_folder(file, title, Some("Random Access Memories"), Some("Daft Punk")),
            "{file}"
        );
        if file.ends_with(".mp3") {
            let settings = SoulseekSettings::default();
            assert_eq!(
                soulseek_candidates::why_not(
                    &lucky(file, 248, "mp3"),
                    judged(
                        title,
                        Some("Random Access Memories"),
                        Some(248),
                        &settings,
                        Some("Daft Punk")
                    )
                )
                .as_deref(),
                Some("A mash-up or medley of this song with another"),
                "{file}"
            );
        }
    }
}

#[test]
fn a_collab_credit_or_a_title_with_the_word_is_no_mash_up() {
    for (file, title, artist) in [
        (
            r"Music\Skrillex x Diplo\Skrillex and Diplo Present Jack U\Skrillex x Diplo - Where Are U Now (with Justin Bieber).flac",
            "Where Are \u{dc} Now",
            "Jack \u{dc}",
        ),
        (
            r"Music\Jack U\05 - Skrillex vs Diplo - Where Are U Now.flac",
            "Where Are \u{dc} Now",
            "Jack \u{dc}",
        ),
        (
            r"Music\Silk Sonic\An Evening With Silk Sonic\Bruno Mars x Anderson .Paak - Leave the Door Open.flac",
            "Leave the Door Open",
            "Silk Sonic",
        ),
        (
            r"Music\Daft Punk\RAM\08 - Get Lucky x Pharrell Williams.flac",
            "Get Lucky (feat. Pharrell Williams)",
            "Daft Punk",
        ),
        (
            r"Music\Next\Singles\09 Next-Too Close-radio edit.flac",
            "Too Close (Radio Edit)",
            "Next",
        ),
        (
            r"Music\Soundtracks\Alien vs. Predator - Main Title.flac",
            "Alien vs. Predator - Main Title",
            "Harald Kloser",
        ),
    ] {
        assert!(
            !version_variant::mash_up(file, Some(title), Some(artist)),
            "{file}"
        );
    }
}

#[test]
fn the_plain_single_is_still_taken_for_the_radio_edit() {
    for file in [
        r"Music\Daft Punk - Get Lucky (2013) [FLAC]\01. Get Lucky.flac",
        r"Music\Daft Punk\Singles\Daft Punk - Get Lucky.flac",
        r"Music\Daft Punk\Singles\Daft Punk feat. Pharrell Williams - Get Lucky.flac",
        r"Music\Daft Punk\Singles\01 - Daft Punk - Get Lucky (feat. Pharrell Williams & Nile Rodgers) [16-44].flac",
        r"Music\Daft Punk\Singles\Get_Lucky_(2013).flac",
    ] {
        assert!(
            !version_variant::cut_not_plain(file, Some(LUCKY_RADIO_EDIT), Some("Daft Punk")),
            "{file}"
        );
    }
}

#[test]
fn a_name_with_more_than_the_title_is_not_taken_for_the_radio_edit() {
    for file in [
        r"Music\Covers\04 Get Lucky Piano Tribute.flac",
        r"Music\Daft Punk\Live\Get Lucky - Grammys Performance.flac",
        r"Music\Various\01 - Get Lucky (Ultra Music Festival 2014 Intro).flac",
    ] {
        assert!(
            version_variant::cut_not_plain(file, Some(LUCKY_RADIO_EDIT), Some("Daft Punk")),
            "{file}"
        );
        // Asked for the song itself, not an edit, that rule does not apply.
        assert!(
            !version_variant::cut_not_plain(file, Some("Get Lucky"), Some("Daft Punk")),
            "{file} (the song itself)"
        );
    }
}

// ---- An edition that changes what is played is a version ---------------------------------

#[test]
fn an_edition_that_changes_the_performance_is_another_version() {
    for file in [
        r"Daft Punk\Random Access Memories (Drumless Edition)\08 - Get Lucky (Drumless Edition).flac",
        r"Daft Punk\Singles\Daft Punk - Get Lucky Drumless.flac",
        r"Daft Punk\Singles\Get Lucky (Piano Version).flac",
        r"Daft Punk\Singles\Get Lucky (8D Audio).flac",
        r"Daft Punk\Singles\Get Lucky (Orchestral Version).flac",
        r"Daft Punk\Singles\Get Lucky (Lo-Fi).flac",
        r"Daft Punk\Singles\Get Lucky (Bass Boosted).flac",
        r"Daft Punk\Singles\Get Lucky (Instrumental Edition).flac",
        r"Daft Punk\Singles\Get Lucky (A Cappella).flac",
        r"Daft Punk\Singles\Get Lucky (Karaoke Edition).flac",
        r"Daft Punk\Singles\Get Lucky (Slowed + Reverb).flac",
        r"Daft Punk\Singles\Get Lucky (Nightcore).flac",
        r"Daft Punk\Singles\Get Lucky (Unplugged).flac",
    ] {
        assert!(
            adds_version(file, "Get Lucky", None)
                || from_version_folder(
                    file,
                    "Get Lucky",
                    Some("Random Access Memories"),
                    Some("Daft Punk")
                ),
            "{file}"
        );
    }
}

#[test]
fn a_drumless_albums_plain_file_is_the_drumless_one() {
    assert!(from_version_folder(
        r"Daft Punk\Random Access Memories (Drumless Edition) (2023)\08 - Get Lucky.flac",
        "Get Lucky",
        Some("Random Access Memories"),
        Some("Daft Punk")
    ));
}

#[test]
fn a_release_edition_is_the_same_recording() {
    for file in [
        r"Daft Punk\Random Access Memories (10th Anniversary Edition)\08 - Get Lucky.flac",
        r"Daft Punk\Random Access Memories (Deluxe Edition)\08 - Get Lucky.flac",
        r"Daft Punk\Random Access Memories (Super Deluxe Edition)\08 - Get Lucky.flac",
        r"Daft Punk\Random Access Memories (Collector's Edition)\08 - Get Lucky.flac",
        r"Daft Punk\Random Access Memories (Bonus Track Version)\08 - Get Lucky.flac",
        r"Daft Punk\Random Access Memories (Limited Edition)\08 - Get Lucky (Remastered Edition).flac",
    ] {
        assert!(!adds_version(file, "Get Lucky", None), "{file}");
        assert!(
            !from_version_folder(
                file,
                "Get Lucky",
                Some("Random Access Memories"),
                Some("Daft Punk")
            ),
            "{file} (folder)"
        );
    }
}

#[test]
fn a_drumless_request_still_takes_the_drumless_copy() {
    assert!(!adds_version(
        r"Daft Punk\Random Access Memories (Drumless Edition)\08 - Get Lucky (Drumless Edition).flac",
        "Get Lucky (Drumless Edition)",
        None
    ));
}

#[test]
fn a_radio_edit_request_hears_a_plainly_named_single() {
    assert!(plausible(
        r"music\Next\Too Close (CDS) (1998)\01 - Too Close.flac",
        "Too Close (Radio Edit)"
    ));
}

// ---- Wanting the song never takes another version of it ------------------------------------

#[test]
fn the_song_itself_never_takes_an_edit_or_a_remix() {
    for file in [
        r"music\Ministry Of Sound - One R&B [UK] [3CD]\CD3\09 - Next - Too Close [Radio Edit].flac",
        r"music\Next\Singles\Next - Too Close - Radio Edit.flac",
        r"music\Next\Singles\09 Next-Too Close-radio edit.flac",
        r"music\Next\Singles\09_next_-_too_close_(radio_edit).flac",
        r"music\Next\Rated Next\03 - Too Close (Clean).flac",
        r"tiktok\Next - Too Close (Sped Up).flac",
        r"Club\Next - Too Close (Extended Mix).flac",
        r"music\Next\Singles\Too Close (Single Edit).flac",
        r"music\Next\Singles\Too Close (Kenny Dope Remix).flac",
    ] {
        assert!(adds_version(file, "Too Close", None), "{file}");
    }
}

#[test]
fn the_same_recording_under_another_label_is_fine() {
    for file in [
        r"music\Next\Rated Next (1997)\03 - Too Close.flac",
        r"music\Next\Rated Next (1997)\03 - Too Close (Album Version).flac",
        r"music\Next\Rated Next (1997)\03 - Too Close (Explicit).flac",
        r"music\Next\Rated Next (Remastered 2017)\03 - Too Close (Remastered).flac",
    ] {
        assert!(!adds_version(file, "Too Close", None), "{file}");
        assert!(
            !from_version_folder(file, "Too Close", Some("Rated Next"), Some("Next")),
            "{file} (folder)"
        );
    }
}

#[test]
fn a_folder_that_names_a_version_makes_its_plain_files_that_version() {
    for file in [
        r"music\Next\Too Close (Radio Edit) - Single\01 - Too Close.flac",
        r"Share\VA - Club Hits 1998 (Extended Mixes)\CD2\04 - Next - Too Close.flac",
        r"Share\Next - Too Close (The Remixes)\01 - Too Close.flac",
        r"Share\Next - Rated Next [Clean]\03 - Too Close.flac",
        r"Share\Next - Rated Next (Instrumentals)\03 - Too Close.flac",
        r"Share\Ministry of Sound - The Annual 1999 (DJ Mix)\CD1\05 - Next - Too Close.flac",
    ] {
        assert!(
            from_version_folder(file, "Too Close", Some("Rated Next"), Some("Next")),
            "{file}"
        );
    }
}

#[test]
fn a_remix_albums_folder_is_read_like_a_live_one() {
    // The real folder that pulled four Mezzanine tracks off a Mad Professor remix album.
    assert!(from_version_folder(
        r"Music\Massive Attack\Massive Attack V Mad Professor Part II (Mezzanine Remix Tapes '98)\03 - Angel.flac",
        "Angel",
        Some("Mezzanine"),
        Some("Massive Attack")
    ));
}

#[test]
fn an_edition_or_a_name_is_not_a_version() {
    for file in [
        r"Music\Drake\Take Care (Deluxe Edition) [2011] [FLAC 16-44]\04 - Take Care.flac",
        r"Music\Drake\Take Care (Extended Edition)\04 - Take Care.flac",
        r"Music\Clean Bandit\New Eyes (2014)\04 - Take Care.flac",
        r"Music\Reprise Records\Take Care\04 - Take Care.flac",
        r"Music\Drake\Take Care (Japanese Version)\04 - Take Care.flac",
    ] {
        assert!(
            !from_version_folder(file, "Take Care", Some("Take Care"), Some("Drake")),
            "{file}"
        );
    }
}

#[test]
fn a_band_called_live_is_not_a_live_take() {
    let file = r"Music\Live\Throwing Copper (1994)\05 - Lightning Crashes.flac";
    assert!(!from_version_folder(
        file,
        "Lightning Crashes",
        Some("Throwing Copper"),
        Some("Live")
    ));
    // Their concert record still reads as one by its other words; AcoustID's live check backs it up.
    assert!(from_live_folder(
        r"Music\Live\Live - Paradiso (In Concert 1995)\05 - Lightning Crashes.flac",
        "Lightning Crashes",
        Some("Throwing Copper"),
        Some("Live")
    ));
}

#[test]
fn a_request_on_the_versions_album_takes_its_folder() {
    assert!(!from_version_folder(
        r"Share\Next - Too Close (The Remixes)\01 - Too Close.flac",
        "Too Close",
        Some("Too Close (The Remixes)"),
        Some("Next")
    ));
}

// ---- Wanting a version gets that version ----------------------------------------------------

fn hit(user: &str, file: &str, seconds: i32, extension: &str, queue: i32) -> SoulseekFileHit {
    SoulseekFileHit {
        username: user.into(),
        filename: file.into(),
        size: 30_000_000,
        length: Some(seconds),
        extension: extension.into(),
        queue_length: Some(queue),
        upload_speed: Some(1_000_000),
        ..Default::default()
    }
}

fn users(ranked: &[SoulseekFileHit]) -> Vec<&str> {
    ranked.iter().map(|h| h.username.as_str()).collect()
}

#[test]
fn a_radio_edit_request_takes_the_named_edit_first_and_a_plain_copy_of_its_length() {
    let ranked = rank_all(
        &[
            // The plain one first in every other way: no queue.
            hit(
                "plain",
                r"music\Next\Too Close (CDS)\01 - Too Close.flac",
                226,
                "flac",
                0,
            ),
            hit(
                "named",
                r"music\VA - One R&B\CD3\09 - Next - Too Close [Radio Edit].flac",
                227,
                "flac",
                5,
            ),
            hit(
                "album",
                r"music\Next\Rated Next (1997)\03 - Too Close.flac",
                261,
                "flac",
                0,
            ),
            hit(
                "clean",
                r"music\Next\Singles\Too Close (Radio Edit) (Clean).flac",
                226,
                "flac",
                0,
            ),
        ],
        &want("Too Close (Radio Edit)", Some(226), None, Some("Next")),
    );

    assert_eq!(users(&ranked), ["named", "plain"]);
}

#[test]
fn the_song_itself_never_ranks_an_edit() {
    let ranked = rank_all(
        &[
            hit(
                "edit",
                r"music\VA - One R&B\CD3\09 - Next - Too Close [Radio Edit].flac",
                262,
                "flac",
                0,
            ),
            hit(
                "single",
                r"music\Next\Too Close (Radio Edit) - Single\01 - Too Close.flac",
                262,
                "flac",
                0,
            ),
            hit(
                "album",
                r"music\Next\Rated Next (1997)\03 - Too Close.flac",
                261,
                "flac",
                9,
            ),
        ],
        &want("Too Close", Some(261), Some("Rated Next"), Some("Next")),
    );

    assert_eq!(users(&ranked), ["album"]);
}

#[test]
fn a_version_that_is_not_a_cut_is_never_answered_by_the_plain_song() {
    for (title, file) in [
        (
            "Too Close (Kenny Dope Remix)",
            r"music\Next\Rated Next (1997)\03 - Too Close.flac",
        ),
        (
            "Heat Waves (Sped Up)",
            r"music\Glass Animals\Dreamland\08 - Heat Waves.flac",
        ),
        ("Creep (Acoustic)", r"music\Radiohead\Pablo Honey\02 - Creep.flac"),
        (
            "Love Story (Taylor's Version)",
            r"music\Taylor Swift\Fearless (2008)\03 - Love Story.flac",
        ),
        (
            "Smile in Your Sleep (Live)",
            r"Music\Silverstein\Discovering the Waterfront (2005)\05 - Smile in Your Sleep.flac",
        ),
    ] {
        assert!(
            version_variant::lacks_requested(file, Some(title), None),
            "{title}"
        );
    }
}

#[test]
fn the_version_asked_for_passes_wherever_it_is_named() {
    for (title, file) in [
        (
            "Too Close (Kenny Dope Remix)",
            r"Share\Next - Too Close (The Remixes)\02 - Too Close (Kenny Dope Remix).flac",
        ),
        (
            "Heat Waves (Sped Up)",
            r"tiktok\Glass Animals - Heat Waves (Sped Up).flac",
        ),
        (
            "Love Story (Taylor's Version)",
            r"music\Taylor Swift\Fearless (Taylor's Version)\03 - Love Story (Taylor's Version).flac",
        ),
        (
            "Smile in Your Sleep (Live)",
            r"Music\Silverstein\Decade (live at the El Mocambo) (2010)\17 - Smile in Your Sleep.flac",
        ),
        (
            "Too Close (Radio Edit)",
            r"music\Next\Too Close (CDS)\01 - Too Close.flac",
        ),
    ] {
        assert!(
            !version_variant::lacks_requested(file, Some(title), None),
            "{title}"
        );
        assert!(!adds_version(file, title, None), "{title} (adds)");
    }
}

#[test]
fn an_album_folder_never_gives_a_version_track_the_plain_file() {
    let track = AlbumTrack::new("t1", "Too Close (Kenny Dope Remix)", Some(330), Some(2));
    let files = [hit(
        "p",
        r"Share\Next - Rated Next\02 - Too Close.flac",
        330,
        "flac",
        0,
    )];
    let files: Vec<&SoulseekFileHit> = files.iter().collect();
    assert!(AlbumFolderPicker::match_files(&[&track], &files).is_empty());
}

// ---- AcoustID says which recording arrived ---------------------------------------------------

fn lookup(title: &str) -> AcoustIdLookup {
    AcoustIdLookup::new(
        true,
        None,
        vec![AcoustIdResult::new(
            0.98,
            vec![AcoustIdRecording {
                sources: 2,
                ..AcoustIdRecording::new("rec", title, ["Next"], Some("Rated Next"), Some(1997))
            }],
        )],
    )
}

#[test]
fn the_song_itself_refuses_a_recording_music_brainz_calls_the_radio_edit() {
    assert_eq!(
        DownloadVerificationService::decide(
            &lookup("Too Close (radio edit)"),
            Some("Next"),
            Some("Too Close"),
            0.85,
            true,
            0,
            &Takes::Any
        )
        .verdict,
        VerificationVerdict::Mismatch
    );
}

#[test]
fn a_radio_edit_request_keeps_the_recording_music_brainz_lists_plainly() {
    assert_eq!(
        DownloadVerificationService::decide(
            &lookup("Too Close"),
            Some("Next"),
            Some("Too Close (Radio Edit)"),
            0.85,
            true,
            0,
            &Takes::Any
        )
        .verdict,
        VerificationVerdict::Confirmed
    );
}

// ---- One search width for a star and Better quality -----------------------------------------

#[test]
fn a_star_searches_as_wide_as_better_quality() {
    let s = SoulseekSettings::default();
    let star = SearchProfile::interactive(&s);
    let upgrade = SearchProfile::upgrade(&s);
    assert_eq!(
        (upgrade.response_limit, upgrade.file_limit),
        (star.response_limit, star.file_limit)
    );
    assert_eq!(
        (star.wider().response_limit, star.wider().file_limit),
        (1_000, 8_000)
    );
    assert_eq!(star.ceiling_seconds, star.wider().ceiling_seconds);
}

#[test]
fn a_search_stopped_at_its_limit_is_told() {
    for (state, limited) in [
        (Some("Completed, FileLimitReached"), true),
        (Some("Completed, ResponseLimitReached"), true),
        (Some("Completed, TimedOut"), false),
        (Some("Completed, Cancelled"), false),
        (None, false),
    ] {
        assert_eq!(hit_limit(state), limited, "{state:?}");
    }
}

#[test]
fn the_take_care_search_finds_the_plain_flac_among_its_mp3s() {
    // 'Drake Take Care' answers as the canary saw them: a wall of MP3s, the FLACs named plainly.
    let mut hits: Vec<SoulseekFileHit> = (1..=40)
        .map(|i| {
            hit(
                &format!("mp3peer{i}"),
                r"Music\Drake - Take Care\04 - Take Care (feat. Rihanna).mp3",
                277,
                "mp3",
                0,
            )
        })
        .collect();
    hits.push(hit(
        "flacpeer",
        r"Music\Drake\Take Care (Deluxe) (2011)\04 - Take Care.flac",
        277,
        "flac",
        3,
    ));

    let ranked = rank_all(
        &hits,
        &want(
            "Take Care (feat. Rihanna)",
            Some(277),
            Some("Take Care"),
            Some("Drake"),
        ),
    );

    assert_eq!(users(&ranked), ["flacpeer"]);
}

// ---- A star's search, against slskd ------------------------------------------------------

/// A FLAC whose STREAMINFO says it lasts this long, so the length check passes.
fn flac(seconds: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"fLaC");
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    let (sample_rate, channels_minus_one, bits_minus_one): (u64, u64, u64) = (44100, 1, 15);
    let total_samples = 44100u64 * seconds;
    let packed = (sample_rate << 44) | (channels_minus_one << 41) | (bits_minus_one << 36) | total_samples;
    bytes.extend_from_slice(&packed.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes
}

/// What a search with these words and this file limit answers, and the state slskd ended it in.
pub(crate) type Answer = Arc<dyn Fn(&str, i32) -> (Vec<Value>, &'static str) + Send + Sync>;

#[derive(Default)]
pub(crate) struct MatchingState {
    pub searches: Vec<(String, i32)>,
    pub batches: Vec<(String, Vec<String>)>,
    search: HashMap<String, (String, i32)>,
    transfers: HashMap<String, Vec<Value>>,
    lengths: HashMap<String, u64>,
    ids: i32,
}

/// slskd as far as one star needs: logged in, each search answered by its words and its file
/// limit with the state slskd ended it in, and a queued file written straight into its folder.
#[derive(Clone)]
pub(crate) struct MatchingSlskd {
    root: String,
    answer: Answer,
    pub state: Arc<Mutex<MatchingState>>,
}

impl MatchingSlskd {
    pub fn length(&self, remote: &str, seconds: i32) {
        self.state
            .lock()
            .lengths
            .insert(remote.to_string(), seconds as u64);
    }

    pub fn searches(&self) -> Vec<(String, i32)> {
        self.state.lock().searches.clone()
    }

    pub fn batches(&self) -> Vec<(String, Vec<String>)> {
        self.state.lock().batches.clone()
    }
}

fn json_answer(body: &Value, status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.to_string().into_bytes(), "application/json")
}

impl Respond for MatchingSlskd {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let segments: Vec<&str> = path.split('/').collect();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let method = request.method.as_str();
        if path == "/api/v0/session" {
            return json_answer(&json!({"token": "jwt", "expires": 4102444800i64}), 200);
        }
        if path == "/api/v0/options" {
            return json_answer(
                &json!({"directories": {"downloads": self.root, "incomplete": "/app/incomplete"}}),
                200,
            );
        }
        if path == "/api/v0/searches" && method == "POST" {
            let search = (
                body["searchText"].as_str().unwrap_or_default().to_string(),
                body["fileLimit"].as_i64().unwrap_or_default() as i32,
            );
            let id = body["id"].as_str().unwrap_or_default().to_string();
            let mut state = self.state.lock();
            state.search.insert(id, search.clone());
            state.searches.push(search);
            return json_answer(&json!({}), 200);
        }
        if path.starts_with("/api/v0/searches/") && method == "GET" {
            let (text, file_limit) = self
                .state
                .lock()
                .search
                .get(segments[4])
                .cloned()
                .unwrap_or_default();
            let (responses, state) = (self.answer)(&text, file_limit);
            return if path.ends_with("/responses") {
                json_answer(&Value::Array(responses), 200)
            } else {
                json_answer(
                    &json!({"state": state, "endedAt": "2026-10-04T12:00:00Z", "responseCount": responses.len()}),
                    200,
                )
            };
        }
        if path.starts_with("/api/v0/users/") && path.ends_with("/directory") {
            return ResponseTemplate::new(404);
        }
        if path == "/api/v0/transfers/downloads/batches" {
            let user = body["username"].as_str().unwrap_or_default().to_string();
            let destination = body["options"]["destination"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let files: Vec<String> = body["files"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .map(|f| f["filename"].as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();
            let mut state = self.state.lock();
            state.batches.push((user.clone(), files.clone()));
            let mut queued = Vec::new();
            for file in &files {
                let leaf = file
                    .replace('\\', "/")
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let local = std::path::Path::new(&self.root).join(&destination).join(&leaf);
                std::fs::create_dir_all(local.parent().expect("a parent")).expect("made");
                let bytes = flac(state.lengths.get(file).copied().unwrap_or(200));
                std::fs::write(&local, &bytes).expect("written");
                state.ids += 1;
                let transfer = json!({
                    "id": format!("t-{}", state.ids), "filename": file, "state": "Completed, Succeeded",
                    "size": bytes.len() as i64, "bytesTransferred": bytes.len() as i64, "percentComplete": 100.0,
                });
                state
                    .transfers
                    .entry(user.clone())
                    .or_default()
                    .push(transfer.clone());
                queued.push(transfer);
            }
            return json_answer(
                &json!({"batch": {"username": user, "transfers": queued}, "failures": []}),
                201,
            );
        }
        if path.starts_with("/api/v0/transfers/downloads/") && method == "GET" {
            let user = dotnet::unescape_data_string(segments[5]);
            let files = self
                .state
                .lock()
                .transfers
                .get(&user)
                .cloned()
                .unwrap_or_default();
            return json_answer(
                &json!({"username": user, "directories": [{"directory": "x", "files": files}]}),
                200,
            );
        }
        json_answer(&json!({}), 200)
    }
}

/// The peers who answer "Drake Take Care" first: thirty MP3s.
fn mp3_wall() -> Vec<Value> {
    (1..=30)
        .map(|i| {
            json!({
                "username": format!("mp3peer{i}"), "uploadSpeed": 5_000_000, "queueLength": 0, "hasFreeUploadSlot": true,
                "files": [{"filename": r"Music\Drake - Take Care\04 - Take Care (feat. Rihanna).mp3", "size": 11_000_000i64,
                    "length": 277, "extension": "mp3", "bitRate": 320}],
            })
        })
        .collect()
}

fn flac_response(user: &str, files: &[(&str, i32)]) -> Value {
    json!({
        "username": user, "uploadSpeed": 1_000_000, "queueLength": 2, "hasFreeUploadSlot": false,
        "files": files.iter().map(|(file, seconds)| json!({
            "filename": file, "size": flac(*seconds as u64).len() as i64, "length": seconds, "extension": "flac",
        })).collect::<Vec<Value>>(),
    })
}

/// One song, "Drake - Take Care (feat. Rihanna)" from the album Take Care, ready to star.
pub(crate) struct Star {
    pub service: Arc<BaseDownloadService>,
    pub slskd: MatchingSlskd,
    pub song: Song,
    pub _root: tempfile::TempDir,
    _server: MockServer,
}

pub(crate) async fn star(answer: Answer, services: DownloadServices) -> Star {
    let root = tempfile::Builder::new()
        .prefix("octo-dl-matching-")
        .tempdir()
        .expect("a temp folder");
    let root_path = root.path().to_string_lossy().into_owned();
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let id = registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Drake".into()),
        title: Some("Take Care (feat. Rihanna)".into()),
        album: Some("Take Care".into()),
        duration: Some(277),
        ..Default::default()
    });
    let song = Song {
        id: format!("ext-soulseek-{id}"),
        external_provider: Some("soulseek".into()),
        external_id: Some(id.clone()),
        title: "Take Care (feat. Rihanna)".into(),
        artist: "Drake".into(),
        album: "Take Care".into(),
        duration: Some(277),
        ..Default::default()
    };

    let slskd = MatchingSlskd {
        root: root_path.clone(),
        answer,
        state: Arc::new(Mutex::new(MatchingState::default())),
    };
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(slskd.clone())
        .mount(&server)
        .await;

    let soulseek = SoulseekSettings {
        base_url: Some(server.uri()),
        username: Some("u".into()),
        password: Some("p".into()),
        min_file_size_bytes: 0,
        album_folders: false,
        parallel_downloads: 1,
        detect_transcodes: false,
        verify_downloads: false,
        download_timeout_seconds: 30,
        ..Default::default()
    };
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: soulseek.clone(),
        subsonic: SubsonicSettings {
            auto_detect_download_path: false,
            download_source: DownloadSource::Soulseek,
            ..Default::default()
        },
        ..Default::default()
    }));
    settings.set_raw("Library:DownloadPath", Some(&root_path));
    let client = SoulseekClient::with_timings(
        &soulseek,
        Timings {
            search_poll_interval: Duration::from_millis(5),
            poll_interval: Duration::from_millis(5),
            min_search_spacing: Duration::ZERO,
            ..Default::default()
        },
    );

    let metadata = FakeMetadata::default();
    metadata
        .songs
        .lock()
        .insert(("soulseek".into(), id), song.clone());
    let core = DownloadCore {
        settings: Arc::clone(&settings),
        local_library: Arc::new(FakeLocalLibrary::default()),
        metadata: Arc::new(metadata),
        navidrome_identity: NavidromeIdentityService::new(Arc::clone(&settings), reqwest::Client::new()),
        history: Arc::new(DownloadHistoryService::new(root.path().join("history.json"))),
        notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
    };
    let services = DownloadServices {
        concurrency: Some(Arc::new(DownloadConcurrency::new(Arc::clone(&settings)))),
        ..services
    };
    let verification = Arc::new(DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        Arc::new(AcoustIdClient::new(
            Arc::new(AcoustIdRateLimitHandler::new(
                Arc::new(AcoustIdRateLimiter::new()),
            )),
            Arc::clone(&settings),
        )),
        Arc::clone(&settings),
        None,
        None,
    ));
    let parts = SoulseekDownloadParts {
        slskd: client,
        rejected_peers: Arc::new(RejectedPeerRegistry::in_memory()),
        verification,
        youtube: Arc::new(YouTubeResolver::with_base_url(Some(&server.uri()))),
        id_registry: registry,
        soulseek_link: None,
        lidarr_imports: None,
        lidarr_fetcher: None,
        spotiflac: None,
        preview_copies: None,
    };
    let (service, _) = SoulseekDownloadService::build(core, services, parts);
    Star {
        service,
        slskd,
        song,
        _root: root,
        _server: server,
    }
}

impl Star {
    pub async fn acquire(&self) -> anyhow::Result<String> {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.service.execute_acquisition(
                "soulseek",
                self.song.external_id.as_deref().expect("an id"),
                false,
                true,
                Some(DownloadSource::Soulseek),
                &CancellationToken::new(),
                None,
                false,
                None,
            ),
        )
        .await
        .expect("within a minute")
    }
}

#[tokio::test]
async fn a_search_filled_with_mp3s_is_asked_again_wider_and_the_flac_it_then_hears_is_taken() {
    const FLAC: &str = r"Music\Drake\Take Care (Deluxe) (2011)\04 - Take Care.flac";
    let star = star(
        Arc::new(|text: &str, file_limit: i32| {
            if text != "Drake Take Care" {
                (Vec::new(), "Completed, TimedOut")
            } else if file_limit <= SONG_FILE_LIMIT {
                (mp3_wall(), "Completed, FileLimitReached")
            } else {
                let mut answers = mp3_wall();
                answers.push(flac_response("flacpeer", &[(FLAC, 277)]));
                (answers, "Completed, FileLimitReached")
            }
        }),
        DownloadServices::default(),
    )
    .await;
    star.slskd.length(FLAC, 277);

    let path = star.acquire().await.expect("the FLAC is taken");

    assert_eq!(
        star.slskd.searches(),
        [
            ("Drake Take Care".to_string(), 2_000),
            ("Drake Take Care".to_string(), 8_000)
        ]
    );
    let batches = star.slskd.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].0, "flacpeer");
    assert!(path.ends_with(".flac"), "{path}");
}

#[tokio::test]
async fn a_search_that_ran_out_of_answers_is_not_asked_again() {
    let star = star(
        Arc::new(|_: &str, _: i32| (mp3_wall(), "Completed, TimedOut")),
        DownloadServices::default(),
    )
    .await;

    assert!(star.acquire().await.is_err());

    let searches = star.slskd.searches();
    assert_eq!(
        searches.iter().map(|(text, _)| text.as_str()).collect::<Vec<_>>(),
        ["Drake Take Care", "Take Care"]
    );
    assert!(searches.iter().all(|(_, limit)| *limit == 2_000), "{searches:?}");
}

// ---- A picked copy meets the same filters --------------------------------------------------

const LIVE_TAKE: &str = r"Music\Drake\Take Care (Live at the O2 Arena 2012)\04 - Take Care.flac";

fn picked(file: &str) -> PickedCopy {
    PickedCopy {
        source: "Soulseek".into(),
        peer: Some("livepeer".into()),
        file: Some(file.into()),
        size: flac(277).len() as i64,
        format: Some("flac".into()),
        length: Some(277),
        ..Default::default()
    }
}

fn timed_out() -> Answer {
    Arc::new(|_: &str, _: i32| (Vec::new(), "Completed, TimedOut"))
}

#[tokio::test]
async fn a_pick_from_a_live_album_never_takes_the_library_files_place() {
    let picks = Arc::new(DownloadPicks::default());
    let star = star(
        timed_out(),
        DownloadServices {
            picks: Some(picks.clone()),
            ..Default::default()
        },
    )
    .await;
    let external_id = star.song.external_id.clone().expect("an id");
    picks.pin(&external_id, Arc::new(picked(LIVE_TAKE)));
    let original = star._root.path().join("library").join("04 - Take Care.mp3");
    std::fs::create_dir_all(original.parent().expect("a folder")).expect("made");
    std::fs::write(&original, [1, 2, 3]).expect("written");
    let handoff = Arc::new(ReplacementHandoff::new(
        original.to_string_lossy().into_owned(),
        KeptIdentity {
            title: "Take Care".into(),
            album: Some("Take Care".into()),
            album_artist: vec!["Drake".into()],
            album_artists: vec!["Drake".into()],
            track: 4,
            disc: 1,
            ..Default::default()
        },
        Arc::new(|_| async { None }.boxed()),
        None,
    ));

    let refused = tokio::time::timeout(
        Duration::from_secs(60),
        star.service.execute_acquisition(
            "soulseek",
            &external_id,
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            None,
            true,
            Some(handoff),
        ),
    )
    .await
    .expect("within a minute")
    .expect_err("refused");

    assert!(refused.to_string().contains("From a live album"), "{refused}");
    assert!(star.slskd.batches().is_empty());
    assert!(star.slskd.searches().is_empty());
}

#[tokio::test]
async fn a_pick_for_a_song_not_in_the_library_is_fetched_as_picked() {
    let picks = Arc::new(DownloadPicks::default());
    let star = star(
        timed_out(),
        DownloadServices {
            picks: Some(picks.clone()),
            ..Default::default()
        },
    )
    .await;
    picks.pin(
        star.song.external_id.as_deref().expect("an id"),
        Arc::new(picked(LIVE_TAKE)),
    );
    star.slskd.length(LIVE_TAKE, 277);

    let path = star.acquire().await.expect("fetched");

    let batches = star.slskd.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].0, "livepeer");
    assert!(star.slskd.searches().is_empty());
    assert!(path.ends_with(".flac"), "{path}");
}

#[test]
fn a_picked_copy_meets_the_deny_list_always_and_the_searchs_checks_when_it_replaces_a_file() {
    let settings = SoulseekSettings::default();
    let file = |user: &str, name: &str, size: i64, extension: &str| SoulseekFileHit {
        username: user.into(),
        filename: name.into(),
        size,
        length: Some(277),
        extension: extension.into(),
        ..Default::default()
    };
    let live = file("livepeer", LIVE_TAKE, 30_000_000, "flac");
    let plain = file(
        "peer",
        r"Music\Drake\Take Care (2011)\04 - Take Care.flac",
        30_000_000,
        "flac",
    );
    let mp3 = file(
        "peer",
        r"Music\Drake\Take Care (2011)\04 - Take Care.mp3",
        11_000_000,
        "mp3",
    );
    let denied = RejectedPeerRegistry::in_memory();
    denied.deny(
        Some(&plain.username),
        Some(&plain.filename),
        "AcoustID: another recording",
        "Drake - Take Care",
    );

    let refusal = |hit: &SoulseekFileHit, replacing: bool, rejected: Option<&RejectedPeerRegistry>| {
        soulseek_candidates::pick_refusal(
            hit,
            Judged {
                rejected,
                remembers_rejections: rejected.is_some(),
                ..judged(
                    "Take Care (feat. Rihanna)",
                    Some("Take Care"),
                    Some(277),
                    &settings,
                    Some("Drake"),
                )
            },
            replacing,
        )
    };

    assert_eq!(refusal(&live, false, None), None);
    assert_eq!(refusal(&live, true, None).as_deref(), Some("From a live album"));
    assert_eq!(refusal(&plain, true, None), None);
    // The format is the one check a person can see for themselves.
    assert_eq!(refusal(&mp3, true, None), None);
    assert_eq!(
        refusal(&plain, false, Some(&denied)).as_deref(),
        Some("Octo downloaded this file before and it was the wrong recording")
    );
}

#[test]
fn find_songs_refuses_a_live_take_in_place_of_a_library_song() {
    let target = FindTarget {
        library_id: Some("nd-1".into()),
        owned_format: Some("mp3".into()),
        ..FindTarget::new(
            "Drake",
            "Take Care (feat. Rihanna)",
            Some("Take Care"),
            Some(277),
            "nd-1",
        )
    };
    let pick = PickedCopy {
        size: 30_000_000,
        ..picked(LIVE_TAKE)
    };
    let copy = FoundCopy::new(
        soulseek_candidates::of(&pick.to_hit().expect("a hit"), None, None),
        pick,
    );

    assert_eq!(
        SongFinder::replace_refusal(&target, &copy, None, None, false).as_deref(),
        Some("From a live album, so it cannot take the place of the one in your library.")
    );
}

#[test]
fn a_log_says_a_picked_copy_was_kept_on_the_word_of_the_pick_not_of_its_name() {
    let unasked = VerificationResult::inconclusive();
    assert_eq!(
        checks_passed_text(&unasked, false, false),
        "the right length; AcoustID could not say, so the name and length decide"
    );
    assert_eq!(
        checks_passed_text(&unasked, false, true),
        "the right length; AcoustID could not say, and its name or folder did not fit the song: kept because you picked it"
    );
    let confirmed = VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        ..Default::default()
    };
    assert!(
        checks_passed_text(&confirmed, true, true)
            .starts_with("the right length; AcoustID: the same recording")
    );
}

/// A lookup answering one result for the song asked for, at this score.
fn take_care_lookup(score: f64) -> AcoustIdLookup {
    AcoustIdLookup::new(
        true,
        None,
        vec![AcoustIdResult::new(
            score,
            vec![AcoustIdRecording {
                sources: 2,
                ..AcoustIdRecording::new("mbid-take-care", "Take Care", ["Drake"], None, None)
            }],
        )],
    )
}

fn take_care_verdict(lookup: &AcoustIdLookup) -> VerificationResult {
    DownloadVerificationService::decide(
        lookup,
        Some("Drake"),
        Some("Take Care"),
        0.85,
        false,
        0,
        &Takes::Any,
    )
}

#[test]
fn a_log_puts_acoust_id_numbers_on_the_checks_a_file_passed() {
    assert_eq!(
        checks_passed_text(&take_care_verdict(&take_care_lookup(0.92)), true, false),
        "the right length; AcoustID: the same recording, 92% sure (needs 85%); the spectrum: really lossless"
    );
    assert_eq!(
        checks_passed_text(&take_care_verdict(&take_care_lookup(0.61)), false, false),
        "the right length; AcoustID: no match above 85% (best 61%), so the name and length decide"
    );
    assert_eq!(
        checks_passed_text(&take_care_verdict(&take_care_lookup(0.61)), false, true),
        "the right length; AcoustID: no match above 85% (best 61%), and its name or folder did not fit the song: kept because you picked it"
    );
    let no_entry = VerificationResult {
        reason: InconclusiveReason::NoEntry,
        lookup: Some(AcoustIdLookup::new(true, None, Vec::new())),
        threshold: 0.85,
        ..Default::default()
    };
    assert_eq!(
        checks_passed_text(&no_entry, false, false),
        "the right length; AcoustID has no entry for this sound, so the name and length decide"
    );
}

#[test]
fn a_log_names_what_acoust_id_heard_when_it_throws_a_file_away() {
    let lookup = AcoustIdLookup::new(
        true,
        None,
        vec![AcoustIdResult::new(
            0.97,
            vec![AcoustIdRecording {
                sources: 2,
                ..AcoustIdRecording::new("mbid-marvins", "Marvins Room", ["Drake"], None, None)
            }],
        )],
    );
    let verdict = take_care_verdict(&lookup);
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        mismatch_text(&verdict),
        "AcoustID says this is 'Drake - Marvins Room', 97% sure (needs 85%)"
    );
    // A file with no audio in it reads as it always has.
    let undecodable = VerificationResult {
        verdict: VerificationVerdict::Mismatch,
        ..Default::default()
    };
    assert_eq!(
        mismatch_text(&undecodable),
        "AcoustID says this is a different recording"
    );
}

// ---- Whole words in the file name, the same in Find songs ----------------------------------

#[test]
fn find_songs_gives_the_same_name_verdict_as_the_download() {
    let settings = SoulseekSettings::default();
    let fourteen = lucky(r"Music\Peer\05 - Fourteen.flac", 200, "flac");
    assert_eq!(
        soulseek_candidates::why_not(&fourteen, judged("Four", None, Some(200), &settings, None)).as_deref(),
        Some("The file name does not match the title")
    );
    assert!(
        rank_all(
            std::slice::from_ref(&fourteen),
            &want("Four", Some(200), None, None)
        )
        .is_empty()
    );

    let run_together = lucky(r"Music\Queen\BohemianRhapsody.flac", 354, "flac");
    assert_eq!(
        soulseek_candidates::why_not(
            &run_together,
            judged("Bohemian Rhapsody", None, Some(354), &settings, Some("Queen"))
        ),
        None
    );
    assert_eq!(
        rank_all(
            std::slice::from_ref(&run_together),
            &want("Bohemian Rhapsody", Some(354), None, Some("Queen"))
        ),
        vec![run_together]
    );
}

// ---- A remix by someone else is never the remix asked for -----------------------------------

/// The five copies the 2026-10-13 search for the Bear//Face remix found, as the peers named them.
fn lost_hits() -> Vec<SoulseekFileHit> {
    vec![
        hit(
            "bear-ep",
            "G\u{2010}Eazy/Let's Get Lost Remixes (2015) (EP)/02 - G\u{2010}Eazy - Let's Get Lost (Bear++Face remix).flac",
            242,
            "flac",
            0,
        ),
        hit(
            "teklife",
            "G\u{2010}Eazy/Let's Get Lost Remixes (2015) (EP)/04 - G\u{2010}Eazy - Let's Get Lost (Teklife remix).flac",
            269,
            "flac",
            0,
        ),
        hit(
            "funkin",
            "G\u{2010}Eazy/Let's Get Lost Remixes (2015) (EP)/03 - G\u{2010}Eazy - Let's Get Lost (Funkin Matt remix).flac",
            273,
            "flac",
            0,
        ),
        hit(
            "futon",
            "G\u{2010}Eazy/Let's Get Lost Remixes (2015) (EP)/01 - G\u{2010}Eazy - Let's Get Lost (Louis Futon remix).flac",
            267,
            "flac",
            0,
        ),
        hit(
            "bear",
            "G\u{2010}Eazy/Let's Get Lost Remixes (2015)/02 - Let's Get Lost (Bear++Face remix).flac",
            242,
            "flac",
            0,
        ),
    ]
}

#[test]
fn only_the_bear_face_copies_are_tried_for_the_bear_face_remix_even_with_no_length() {
    let title = "Let's Get Lost (feat. Devon Baldwin) [Bear//Face Remix]";
    let album = Some("Let's Get Lost Remixes (feat. Devon Baldwin) - EP");
    for seconds in [None, Some(242)] {
        let ranked = rank_all(&lost_hits(), &want(title, seconds, album, Some("G-Eazy")));
        assert_eq!(users(&ranked), ["bear-ep", "bear"], "length {seconds:?}");
    }
    assert!(version_variant::other_remixer(
        "04 - G\u{2010}Eazy - Let's Get Lost (Teklife remix).flac",
        Some(title),
        Some("G-Eazy")
    ));
    let settings = SoulseekSettings::default();
    let judged = soulseek_candidates::why_not(
        &lost_hits()[1],
        Judged {
            title,
            album,
            duration: None,
            settings: &settings,
            rejected: None,
            remembers_rejections: false,
            artist: Some("G-Eazy"),
        },
    );
    assert_eq!(judged.as_deref(), Some("A remix by someone else"));
}
