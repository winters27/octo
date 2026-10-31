//! The verification decision (whether a file needs review is tested in
//! `octo_core::fingerprint::verification`), the ISRC check, live takes only on live albums, and
//! an agreeing AcoustID candidate that skips another version.

use octo_core::fingerprint::{AcoustIdCredit, AcoustIdRelease};
use octo_core::models::domain::Song;

use super::*;

const THRESHOLD: f64 = 0.85;

fn ok(results: Vec<AcoustIdResult>) -> AcoustIdLookup {
    AcoustIdLookup::new(true, None, results)
}

fn result(score: f64, recordings: Vec<AcoustIdRecording>) -> AcoustIdResult {
    AcoustIdResult::new(score, recordings)
}

/// A recording two submissions tie the sound to, enough to confirm on its own.
fn recording(title: &str, artist: &str, album: Option<&str>, year: Option<i32>) -> AcoustIdRecording {
    AcoustIdRecording {
        sources: 2,
        ..AcoustIdRecording::new(format!("mbid-{title}"), title, [artist], album, year)
    }
}

fn decide(lookup: &AcoustIdLookup, artist: &str, title: &str, authoritative: bool) -> VerificationResult {
    DownloadVerificationService::decide(
        lookup,
        Some(artist),
        Some(title),
        THRESHOLD,
        authoritative,
        0,
        &Takes::Any,
    )
}

// ---- the decision ---------------------------------------------------------------------------

#[test]
fn decide_confident_agreeing_match_is_confirmed() {
    let verdict = decide(
        &ok(vec![result(
            0.97,
            vec![recording(
                "Teardrop",
                "Massive Attack",
                Some("Mezzanine"),
                Some(1998),
            )],
        )]),
        "Massive Attack",
        "Teardrop",
        true,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.matched_album.as_deref(), Some("Mezzanine"));
    assert_eq!(verdict.matched_year, Some(1998));
}

/// The whole answer rides along on every verdict, so the chooser can weigh every release the
/// service named, and a confirmation carries the service's own id.
#[test]
fn decide_carries_the_lookup_on_every_branch_and_the_result_id_when_confirmed() {
    let mut confirmed_result = result(0.97, vec![recording("Teardrop", "Massive Attack", None, None)]);
    confirmed_result.id = Some("acoustid-1".into());
    let confirmed = ok(vec![confirmed_result]);
    let confirmed_verdict = decide(&confirmed, "Massive Attack", "Teardrop", false);
    assert_eq!(confirmed_verdict.lookup.as_ref(), Some(&confirmed));
    assert_eq!(confirmed_verdict.acoust_id.as_deref(), Some("acoustid-1"));

    let weak = ok(vec![result(
        0.60,
        vec![recording("Teardrop", "Massive Attack", None, None)],
    )]);
    let weak_verdict = decide(&weak, "Massive Attack", "Teardrop", false);
    assert_eq!(weak_verdict.lookup.as_ref(), Some(&weak));
    assert_eq!(weak_verdict.acoust_id, None);

    let mismatch = ok(vec![result(
        0.97,
        vec![recording("Angel", "Massive Attack", None, None)],
    )]);
    assert_eq!(
        decide(&mismatch, "Massive Attack", "Teardrop", false)
            .lookup
            .as_ref(),
        Some(&mismatch)
    );
}

/// The most important test in the feature. A weak match must never delete a file, which is also
/// why raising MinMatchScore makes Octo more permissive rather than stricter.
#[test]
fn decide_score_below_threshold_is_inconclusive_not_a_mismatch() {
    let verdict = decide(
        &ok(vec![result(
            0.60,
            vec![recording("Something Else Entirely", "Another Artist", None, None)],
        )]),
        "Massive Attack",
        "Teardrop",
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.deny_reason, "");
}

/// An obscure track has no AcoustID entry, and that is the music Soulseek exists to find.
/// Rejecting on absence would make verification worst where the library is rarest.
#[test]
fn decide_no_results_at_all_is_inconclusive() {
    let verdict = decide(&ok(vec![]), "Some Obscure Band", "A Song", false);
    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
}

/// AcoustID knows the audio but has no MusicBrainz link, so nothing can be decided.
#[test]
fn decide_qualifying_result_with_no_recordings_is_inconclusive() {
    let verdict = decide(
        &ok(vec![result(0.99, vec![])]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
}

#[test]
fn decide_confident_different_recording_is_a_mismatch_and_names_what_it_got() {
    let verdict = decide(
        &ok(vec![result(
            0.96,
            vec![recording("Picture to Burn", "Taylor Swift", None, None)],
        )]),
        "Taylor Swift",
        "Cruel Summer",
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert!(verdict.deny_reason.contains("Picture to Burn"));
    assert_eq!(verdict.describe(), "'Taylor Swift - Picture to Burn'");
}

/// One AcoustID id maps to several MusicBrainz recordings when the same audio ships on an album
/// and a compilation. Demanding the first would reject correct files.
#[test]
fn decide_second_recording_agrees_is_confirmed() {
    let verdict = decide(
        &ok(vec![result(
            0.98,
            vec![
                recording(
                    "Teardrop",
                    "Massive Attack",
                    Some("Now That's What I Call Music! 42"),
                    None,
                ),
                recording("Teardrop", "Massive Attack", Some("Mezzanine"), Some(1998)),
            ],
        )]),
        "Massive Attack",
        "Teardrop",
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
}

/// The highest-scoring qualifying result wins, not the first one listed.
#[test]
fn decide_prefers_the_highest_scoring_qualifying_result() {
    let verdict = decide(
        &ok(vec![
            result(0.86, vec![recording("Wrong Song", "Wrong Artist", None, None)]),
            result(0.99, vec![recording("Teardrop", "Massive Attack", None, None)]),
        ]),
        "Massive Attack",
        "Teardrop",
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert!((verdict.score - 0.99).abs() < 0.0005);
}

fn peer_song() -> Song {
    Song {
        title: "peer title".into(),
        artist: "peer artist".into(),
        album: "peer album".into(),
        ..Default::default()
    }
}

#[test]
fn apply_tags_to_with_tagging_off_leaves_the_song_alone() {
    let mut song = peer_song();
    decide(
        &ok(vec![result(
            0.99,
            vec![recording(
                "Teardrop",
                "Massive Attack",
                Some("Mezzanine"),
                Some(1998),
            )],
        )]),
        "Massive Attack",
        "Teardrop",
        false,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.title, "peer title");
    assert_eq!(song.artist, "peer artist");
    assert_eq!(song.year, None);
}

#[test]
fn apply_tags_to_confirmed_and_tagging_on_overwrites_the_peers_tags() {
    let mut song = peer_song();
    decide(
        &ok(vec![result(
            0.99,
            vec![recording(
                "Teardrop",
                "Massive Attack",
                Some("Mezzanine"),
                Some(1998),
            )],
        )]),
        "Massive Attack",
        "Teardrop",
        true,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.title, "Teardrop");
    assert_eq!(song.artist, "Massive Attack");
    assert_eq!(song.album, "Mezzanine");
    assert_eq!(song.year, Some(1998));
}

/// A mismatch must never retag; the file is about to be deleted.
#[test]
fn apply_tags_to_mismatch_leaves_the_song_alone() {
    let mut song = Song {
        title: "peer title".into(),
        artist: "peer artist".into(),
        ..Default::default()
    };
    decide(
        &ok(vec![result(
            0.96,
            vec![recording("Picture to Burn", "Taylor Swift", None, None)],
        )]),
        "Taylor Swift",
        "Cruel Summer",
        true,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.title, "peer title");
    assert_eq!(song.music_brainz_recording_id, None);
}

/// Below the threshold nothing is decided, but the one recording that agrees on title, artist
/// and length is remembered: it is the MusicBrainz id a person's Keep may send back (#47).
#[test]
fn decide_below_threshold_says_why_and_names_the_one_agreeing_candidate() {
    let mut teardrop = recording("Teardrop", "Massive Attack", None, None);
    teardrop.duration_seconds = Some(331);
    let verdict = DownloadVerificationService::decide(
        &ok(vec![result(
            0.60,
            vec![teardrop, recording("Something Else", "Someone Else", None, None)],
        )]),
        Some("Massive Attack"),
        Some("Teardrop"),
        THRESHOLD,
        false,
        330,
        &Takes::Any,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::BelowThreshold);
    assert_eq!(verdict.candidate_recording_id.as_deref(), Some("mbid-Teardrop"));
    assert!(verdict.needs_review());
}

/// MusicBrainz holds near-duplicate recordings; two agreeing ids is a guess, not an answer.
#[test]
fn decide_two_agreeing_candidates_names_none() {
    let mut first = AcoustIdRecording::new("id-1", "Teardrop", ["Massive Attack"], None, None);
    first.duration_seconds = Some(315);
    let mut second = AcoustIdRecording::new("id-2", "Teardrop", ["Massive Attack"], None, None);
    second.duration_seconds = Some(315);
    let verdict = DownloadVerificationService::decide(
        &ok(vec![result(0.60, vec![first, second])]),
        Some("Massive Attack"),
        Some("Teardrop"),
        THRESHOLD,
        false,
        315,
        &Takes::Any,
    );

    assert_eq!(verdict.candidate_recording_id, None);
}

#[test]
fn decide_candidate_of_the_wrong_length_is_not_named() {
    let mut live = recording("Teardrop", "Massive Attack", None, None);
    live.duration_seconds = Some(400);
    let verdict = DownloadVerificationService::decide(
        &ok(vec![result(0.60, vec![live])]),
        Some("Massive Attack"),
        Some("Teardrop"),
        THRESHOLD,
        false,
        330,
        &Takes::Any,
    );

    assert_eq!(verdict.candidate_recording_id, None);
}

/// A fingerprint AcoustID knows but MusicBrainz does not is exactly what a person can settle.
#[test]
fn decide_high_score_with_no_recordings_is_no_entry() {
    let verdict = decide(
        &ok(vec![result(0.99, vec![])]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    assert_eq!(verdict.reason, InconclusiveReason::NoEntry);
}

/// An id says what the file IS and changes nothing a person reads, so a confirmed match records
/// it even with tagging from MusicBrainz off (#48).
#[test]
fn apply_tags_to_confirmed_with_tagging_off_still_records_the_recording_id() {
    let mut song = Song {
        title: "peer title".into(),
        artist: "Bizarrap, Rauw Alejandro".into(),
        ..Default::default()
    };
    let title = "Rauw Alejandro: Bzrp Music Sessions, Vol. 56";
    let mut found = AcoustIdRecording::new(
        "rec-1",
        title,
        ["Bizarrap", "Rauw Alejandro"],
        Some(title),
        Some(2023),
    );
    found.sources = 2;
    found.credits = vec![
        AcoustIdCredit::new("Bizarrap", Some("a1"), " & "),
        AcoustIdCredit::new("Rauw Alejandro", Some("a2"), ""),
    ];
    found.release = Some(AcoustIdRelease::new(
        Some("rel-1".into()),
        Some("rg-1".into()),
        Some(title.into()),
        Some(2023),
        Some(1),
        Some(1),
        Some(1),
        Some("Bizarrap & Rauw Alejandro".into()),
        false,
    ));
    decide(
        &ok(vec![result(0.99, vec![found])]),
        "Bizarrap, Rauw Alejandro",
        title,
        false,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.title, "peer title");
    assert_eq!(song.music_brainz_recording_id.as_deref(), Some("rec-1"));
    assert_eq!(song.primary_artist.as_deref(), Some("Bizarrap"));
    assert_eq!(song.artists, ["Bizarrap", "Rauw Alejandro"]);
    assert_eq!(song.music_brainz_release_id.as_deref(), Some("rel-1"));
    assert_eq!(song.music_brainz_release_group_id.as_deref(), Some("rg-1"));
    // Only with tags authoritative does the track number and album artist come across.
    assert_eq!(song.track, None);
    assert_eq!(song.album_artist, None);
}

#[test]
fn apply_tags_to_confirmed_and_tagging_on_takes_the_releases_track_and_album_artist() {
    let mut song = Song {
        title: "t".into(),
        artist: "a".into(),
        ..Default::default()
    };
    let mut found = AcoustIdRecording::new(
        "rec-1",
        "Teardrop",
        ["Massive Attack"],
        Some("Mezzanine"),
        Some(1998),
    );
    found.sources = 2;
    found.release = Some(AcoustIdRelease::new(
        Some("rel-1".into()),
        Some("rg-1".into()),
        Some("Mezzanine".into()),
        Some(1998),
        Some(3),
        Some(11),
        Some(1),
        Some("Massive Attack".into()),
        false,
    ));
    decide(
        &ok(vec![result(0.99, vec![found])]),
        "Massive Attack",
        "Teardrop",
        true,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.track, Some(3));
    assert_eq!(song.total_tracks, Some(11));
    assert_eq!(song.disc_number, Some(1));
    assert_eq!(song.album_artist.as_deref(), Some("Massive Attack"));
}

// ---- the ISRC check --------------------------------------------------------------------------

const ISRC: &str = "JPU901901234";

/// The fingerprint confidently names the song in its own script, and the request was made in its
/// romanised name: by the text alone, a mismatch.
fn native_script() -> AcoustIdLookup {
    native_script_with(2)
}

/// [`native_script`] with this many submissions behind each recording.
fn native_script_with(sources: i32) -> AcoustIdLookup {
    ok(vec![result(
        0.97,
        vec![
            AcoustIdRecording {
                sources,
                ..AcoustIdRecording::new("mbid-gurenge", "紅蓮華", ["LiSA"], None, None)
            },
            AcoustIdRecording {
                sources,
                ..AcoustIdRecording::new("mbid-other", "紅蓮華 (TV Size)", ["LiSA"], None, None)
            },
        ],
    )])
}

fn text_verdict() -> VerificationResult {
    VerificationResult {
        fingerprint: Some("AQAD".into()),
        duration_seconds: 239,
        ..decide(&native_script(), "LiSA", "Gurenge", false)
    }
}

fn isrcs(entries: &[(&str, &[&str])]) -> RecordingIsrcs {
    entries
        .iter()
        .map(|(id, codes)| (id.to_string(), codes.iter().map(|c| c.to_string()).collect()))
        .collect()
}

#[test]
fn the_text_alone_calls_the_native_title_a_mismatch() {
    assert_eq!(text_verdict().verdict, VerificationVerdict::Mismatch);
}

#[test]
fn a_recording_listing_the_requested_isrc_confirms() {
    let isrcs = isrcs(&[("mbid-gurenge", &[ISRC]), ("mbid-other", &[])]);

    let verdict = DownloadVerificationService::settle_by_isrc(
        text_verdict(),
        &native_script(),
        THRESHOLD,
        true,
        ISRC,
        &isrcs,
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.recording_id.as_deref(), Some("mbid-gurenge"));
    assert_eq!(
        verdict.r#match.as_ref().map(|m| m.recording_id.as_str()),
        Some("mbid-gurenge")
    );
    assert!(verdict.tags_authoritative);
    assert_eq!(verdict.fingerprint.as_deref(), Some("AQAD"));
    assert!(
        verdict
            .evidence
            .as_deref()
            .is_some_and(|e| e.contains("MusicBrainz lists the requested ISRC"))
    );
    assert_eq!(verdict.deny_reason, "");
}

/// Different codes are not a rejection by themselves, and not a rescue either: the verdict the
/// text gave stands.
#[test]
fn recordings_listing_other_isrcs_change_nothing() {
    let isrcs = isrcs(&[("mbid-gurenge", &["USRC17607839"])]);
    let before = text_verdict();

    let verdict = DownloadVerificationService::settle_by_isrc(
        before.clone(),
        &native_script(),
        THRESHOLD,
        false,
        ISRC,
        &isrcs,
        true,
    );

    assert_eq!(verdict, before);
}

/// The tags carry the code, MusicBrainz lists none to contradict them: two sources disagree
/// about names only, and the file is kept and asked about instead of deleted.
#[test]
fn tagged_isrc_and_a_recording_with_none_keeps_the_file_for_review() {
    let isrcs = isrcs(&[("mbid-gurenge", &[]), ("mbid-other", &[])]);

    let verdict = DownloadVerificationService::settle_by_isrc(
        text_verdict(),
        &native_script(),
        THRESHOLD,
        false,
        ISRC,
        &isrcs,
        true,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::SourceDisagreed);
    assert!(verdict.needs_review());
    assert_eq!(verdict.deny_reason, "");
    // A tag does not then promote it to Confirmed: the fingerprint did name something else.
    assert_eq!(
        DownloadVerificationService::with_tagged_isrc(verdict, Some(ISRC), true).verdict,
        VerificationVerdict::Inconclusive
    );
}

#[test]
fn no_tagged_isrc_and_a_recording_with_none_stays_a_mismatch() {
    let isrcs = isrcs(&[("mbid-gurenge", &[])]);

    let verdict = DownloadVerificationService::settle_by_isrc(
        text_verdict(),
        &native_script(),
        THRESHOLD,
        false,
        ISRC,
        &isrcs,
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
}

/// MusicBrainz could not be asked at all: nothing is known, so nothing changes.
#[test]
fn music_brainz_unreachable_changes_nothing() {
    let before = text_verdict();
    let verdict = DownloadVerificationService::settle_by_isrc(
        before.clone(),
        &native_script(),
        THRESHOLD,
        false,
        ISRC,
        &RecordingIsrcs::new(),
        true,
    );

    assert_eq!(verdict, before);
}

#[test]
fn a_tagged_isrc_confirms_when_acoust_id_could_not_say() {
    for reason in [
        InconclusiveReason::Disabled,
        InconclusiveReason::NotFingerprinted,
        InconclusiveReason::LookupFailed,
        InconclusiveReason::NoEntry,
        InconclusiveReason::BelowThreshold,
    ] {
        let verdict = DownloadVerificationService::with_tagged_isrc(
            VerificationResult {
                reason,
                fingerprint: Some("AQAD".into()),
                ..Default::default()
            },
            Some(ISRC),
            true,
        );

        assert_eq!(verdict.verdict, VerificationVerdict::Confirmed, "{reason}");
        assert_eq!(verdict.reason, InconclusiveReason::None, "{reason}");
        assert!(!verdict.needs_review(), "{reason}");
        assert_eq!(verdict.fingerprint.as_deref(), Some("AQAD"), "{reason}");
        assert_eq!(
            verdict.evidence.as_deref(),
            Some(format!("the file's own tags carry the requested ISRC {ISRC}").as_str()),
            "{reason}"
        );
    }
}

#[test]
fn a_tagged_isrc_never_overrules_a_confident_fingerprint() {
    let mismatch = text_verdict();
    assert_eq!(
        DownloadVerificationService::with_tagged_isrc(mismatch.clone(), Some(ISRC), true),
        mismatch
    );
}

#[test]
fn no_tagged_match_changes_nothing() {
    let inconclusive = VerificationResult {
        reason: InconclusiveReason::NoEntry,
        ..Default::default()
    };
    assert_eq!(
        DownloadVerificationService::with_tagged_isrc(inconclusive.clone(), Some(ISRC), false),
        inconclusive
    );
}

#[test]
fn read_isrcs_a_file_that_is_not_there_is_empty() {
    let missing = std::env::temp_dir().join(format!("missing-{}.flac", uuid::Uuid::new_v4().simple()));
    assert!(DownloadVerificationService::read_isrcs(&missing.to_string_lossy()).is_empty());
}

/// The tags as another program wrote them, read through TagLib's one ISRC property: a FLAC's
/// Vorbis ISRC and an MP3's ID3 TSRC and user text ISRC written by ffmpeg, and an MP4's iTunes
/// ISRC atom (which ffmpeg does not write) written by the tag writer into a file ffmpeg made.
#[test]
fn read_isrcs_reads_every_containers_isrc_tag() {
    if !crate::services::test_support::ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on the PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let ffmpeg = |arguments: &str| crate::services::test_support::run_ffmpeg(dir.path(), arguments);
    const TONE: &str = "-f lavfi -i sine=frequency=440:duration=1";
    ffmpeg(&format!(
        "{TONE} -metadata ISRC=JP-U90-19-01234 -c:a flac tagged.flac"
    ));
    // TSRC is ID3's own ISRC frame; ffmpeg writes a key named ISRC as a user text frame.
    ffmpeg(&format!(
        "{TONE} -metadata TSRC=JPU901901234 -c:a libmp3lame -b:a 128k -id3v2_version 4 tagged.mp3"
    ));
    ffmpeg(&format!(
        "{TONE} -metadata ISRC=JPU901901234 -c:a libmp3lame -b:a 128k -id3v2_version 4 user_frame.mp3"
    ));
    ffmpeg(&format!("{TONE} -c:a aac -b:a 128k tagged.m4a"));
    {
        let m4a = dir.path().join("tagged.m4a");
        let mut file = TagFile::open(&m4a).expect("ffmpeg's m4a opens");
        tag_writer_extras::set_text(&mut file, octo_media::tags::TagFields::ISRC, Some("JPU901901234"));
        file.save().expect("saved");
    }
    ffmpeg(&format!("{TONE} -c:a flac untagged.flac"));

    let read = |name: &str| DownloadVerificationService::read_isrcs(&dir.path().join(name).to_string_lossy());
    for name in ["tagged.flac", "tagged.mp3", "user_frame.mp3", "tagged.m4a"] {
        assert_eq!(read(name), [ISRC], "{name}");
    }
    assert!(read("untagged.flac").is_empty());
}

// ---- live versions ---------------------------------------------------------------------------

fn live_recording(id: &str, albums: &[(&str, &[&str])]) -> AcoustIdRecording {
    let mut found = AcoustIdRecording::new(
        id,
        "Smile in Your Sleep",
        ["Silverstein"],
        albums.first().map(|(group, _)| *group),
        Some(2010),
    );
    found.sources = 2;
    found.releases = albums
        .iter()
        .map(|(group, types)| {
            let mut release = AcoustIdRelease::new(
                None,
                Some(format!("rg-{group}")),
                Some(group.to_string()),
                Some(2010),
                None,
                None,
                None,
                Some("Silverstein".into()),
                false,
            );
            release.group_title = Some(group.to_string());
            release.primary_type = Some("Album".into());
            release.secondary_types = types.iter().map(|t| t.to_string()).collect();
            release
        })
        .collect();
    found
}

fn live_lookup(recordings: Vec<AcoustIdRecording>) -> AcoustIdLookup {
    ok(vec![result(1.0, recordings)])
}

fn live_only() -> AcoustIdRecording {
    live_recording("live", &[("Decade (live at the El Mocambo)", &["Live"])])
}

fn studio() -> AcoustIdRecording {
    live_recording(
        "studio",
        &[
            ("Discovering the Waterfront", &[]),
            ("Decade (live at the El Mocambo)", &["Live"]),
        ],
    )
}

fn decide_live(lookup: &AcoustIdLookup, refuse_live: bool) -> VerificationResult {
    DownloadVerificationService::decide(
        lookup,
        Some("Silverstein"),
        Some("Smile in Your Sleep"),
        0.85,
        true,
        0,
        &(if refuse_live {
            Takes::Only(Default::default())
        } else {
            Takes::Any
        }),
    )
}

#[test]
fn a_download_refuses_a_recording_only_live_albums_list() {
    let verdict = decide_live(&live_lookup(vec![live_only()]), true);

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        verdict.deny_reason,
        "is a live recording, from 'Decade (live at the El Mocambo)'"
    );
}

#[test]
fn a_song_already_in_the_library_is_never_refused_for_being_live() {
    // The Review sweep and the tag preview ask without refuseLive: a live album may be yours on purpose.
    assert_eq!(
        decide_live(&live_lookup(vec![live_only()]), false).verdict,
        VerificationVerdict::Confirmed
    );
}

#[test]
fn a_studio_recording_that_is_also_on_a_live_album_is_fine() {
    assert_eq!(
        decide_live(&live_lookup(vec![studio()]), true).verdict,
        VerificationVerdict::Confirmed
    );
}

#[test]
fn the_same_audio_listed_both_ways_confirms_as_the_studio_recording() {
    let verdict = decide_live(&live_lookup(vec![live_only(), studio()]), true);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.recording_id.as_deref(), Some("studio"));
}

#[test]
fn a_recording_with_no_albums_listed_is_not_judged() {
    assert!(!DownloadVerificationService::only_on_live_albums(
        &AcoustIdRecording::new("x", "Song", ["A"], None, None)
    ));
}

// ---- the AcoustID candidate case -------------------------------------------------------------

#[test]
fn acoust_id_agreeing_candidate_skips_another_version() {
    let mut live = AcoustIdRecording::new("live", "Creep (Live)", ["Radiohead"], None, None);
    live.duration_seconds = Some(238);
    let mut studio = AcoustIdRecording::new("studio", "Creep", ["Radiohead"], None, None);
    studio.duration_seconds = Some(238);
    let lookup = AcoustIdLookup::new(true, None, vec![result(0.95, vec![live, studio])]);

    assert_eq!(
        DownloadVerificationService::agreeing_candidate(&lookup, Some("Radiohead"), Some("Creep"), 238)
            .as_deref(),
        Some("studio")
    );
}

/// The verifier the tag preview asks reports the switches, and with verification off
/// a file is never even fingerprinted.
#[tokio::test]
async fn the_preview_verifier_answers_through_the_service() {
    use octo_core::settings::{AppSettings, SoulseekSettings};

    use crate::services::fingerprint::{AcoustIdRateLimitHandler, AcoustIdRateLimiter};

    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: SoulseekSettings {
            verify_downloads: true,
            ..Default::default()
        },
        ..Default::default()
    }));
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::new(
            Arc::new(AcoustIdRateLimiter::new()),
        )),
        Arc::clone(&settings),
        None,
    ));
    let service = DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        client,
        Arc::clone(&settings),
        None,
        None,
    );
    let verifier: &dyn FingerprintVerifier = &service;
    // No key, not even Octo's own: nothing to ask AcoustID with.
    assert!(!verifier.is_fingerprinting_enabled());
    assert!(service.remembers_rejections());
    assert_eq!(
        verifier.verify("/nowhere.flac", "A", "T").await.reason,
        InconclusiveReason::Disabled
    );

    settings.set(AppSettings {
        soulseek: SoulseekSettings {
            verify_downloads: false,
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(
        service
            .verify(
                "/nowhere.flac",
                Some("A"),
                Some("T"),
                Some(ISRC),
                &Takes::Only(Default::default())
            )
            .await,
        VerificationResult::inconclusive()
    );
    assert_eq!(
        service.check_lossless("/nowhere.flac", None, None).await,
        SpectrumReport::unknown("not checked", 0)
    );
}

/// Verification is on for a default install, so a file refused once is remembered and the same
/// peer's same file is never offered for download again.
#[test]
fn a_default_install_never_fetches_a_refused_file_again() {
    use octo_core::settings::AppSettings;
    use octo_core::soulseek::SoulseekFileHit;

    use crate::services::fingerprint::{AcoustIdRateLimitHandler, AcoustIdRateLimiter};
    use crate::services::soulseek::rejected_peer_registry::RejectedPeerRegistry;
    use crate::services::soulseek::soulseek_download_service::candidate_allowed;

    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let service = DownloadVerificationService::new(
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
    );
    assert!(service.remembers_rejections());
    let registry = RejectedPeerRegistry::in_memory();
    registry.deny(
        Some("peer1"),
        Some("Help!.flac"),
        "wrong recording",
        "The Beatles - Help!",
    );
    let hit = |file: &str| SoulseekFileHit {
        username: "peer1".into(),
        filename: file.into(),
        extension: "flac".into(),
        size: 40_000_000,
        ..Default::default()
    };
    assert!(!candidate_allowed(
        &hit("Help!.flac"),
        Some(&registry),
        service.remembers_rejections()
    ));
    assert!(candidate_allowed(
        &hit("Yesterday.flac"),
        Some(&registry),
        service.remembers_rejections()
    ));
}

// ---- Stray links: a weak recording beside a much stronger one of another song ---------------

fn sized(title: &str, artist: &str, sources: i32, seconds: Option<i32>) -> AcoustIdRecording {
    let mut r = recording(title, artist, None, None);
    r.sources = sources;
    r.duration_seconds = seconds;
    r
}

fn decide_at(lookup: &AcoustIdLookup, artist: &str, title: &str, seconds: i32) -> VerificationResult {
    DownloadVerificationService::decide(
        lookup,
        Some(artist),
        Some(title),
        THRESHOLD,
        false,
        seconds,
        &Takes::Any,
    )
}

/// AcoustID ties this audio to "bury a friend" with 1,865 submissions and to "my boy" with one.
fn bury_a_friend() -> AcoustIdLookup {
    let mut stray = sized("my boy", "Billie Eilish", 1, Some(171));
    stray.recording_id = "mbid-my-boy".into();
    let mut acoustic = sized("Bury a Friend (acoustic)", "Billie Eilish", 1, Some(190));
    acoustic.recording_id = "mbid-acoustic".into();
    let mut single = sized("bury a friend", "Billie Eilish", 2, Some(193));
    single.recording_id = "mbid-single".into();
    ok(vec![result(
        0.97,
        vec![
            stray,
            sized("bury a friend", "Billie Eilish", 1865, Some(193)),
            acoustic,
            single,
        ],
    )])
}

#[test]
fn a_stray_link_beside_a_strong_one_does_not_confirm() {
    let verdict = decide_at(&bury_a_friend(), "Billie Eilish", "my boy", 193);

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(verdict.matched_title.as_deref(), Some("bury a friend"));
    assert_eq!(verdict.recording_id.as_deref(), Some("mbid-bury a friend"));
}

#[test]
fn a_stray_link_with_the_right_length_still_does_not_confirm() {
    // Same lengths everywhere: only the submissions tell the stray apart.
    let mut stray = sized("my boy", "Billie Eilish", 1, Some(193));
    stray.recording_id = "mbid-my-boy".into();
    let lookup = ok(vec![result(
        0.97,
        vec![stray, sized("bury a friend", "Billie Eilish", 1865, Some(193))],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 193);

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(verdict.matched_title.as_deref(), Some("bury a friend"));
}

#[test]
fn the_strong_link_still_confirms() {
    let verdict = decide_at(&bury_a_friend(), "Billie Eilish", "bury a friend", 193);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
}

#[test]
fn a_far_off_length_does_not_confirm() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 5, Some(171))],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 193);

    assert_ne!(verdict.verdict, VerificationVerdict::Confirmed);
}

/// The right song and artist whose only doubt is the length is a question for a person, never
/// a mismatch that calls the requested song another one.
#[test]
fn a_right_song_off_only_in_length_needs_a_look_and_is_never_called_another_song() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 5, Some(171))],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 193);

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::LengthOff);
    assert!(verdict.needs_review());
    assert_eq!(verdict.deny_reason, "");
    assert_eq!(verdict.matched_title.as_deref(), Some("my boy"));
    assert_eq!(
        verdict.r#match.as_ref().and_then(|m| m.duration_seconds),
        Some(171)
    );
}

#[test]
fn a_tagged_isrc_never_settles_a_length_doubt() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 5, Some(171))],
    )]);
    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 193);

    let kept = DownloadVerificationService::with_tagged_isrc(verdict, Some("USUM71900764"), true);

    assert_eq!(kept.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(kept.reason, InconclusiveReason::LengthOff);
}

/// A stray link stays a mismatch even when its length is off too: the stronger song is named.
#[test]
fn a_stray_link_with_an_off_length_is_still_a_mismatch() {
    let verdict = decide_at(&bury_a_friend(), "Billie Eilish", "my boy", 240);

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(verdict.matched_title.as_deref(), Some("bury a friend"));
}

#[test]
fn a_lone_small_link_needs_the_preview() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 1, Some(171))],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 172);

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::LoneLink);
    assert!(verdict.needs_review());
    assert_eq!(verdict.recording_id.as_deref(), Some("mbid-my boy"));
    assert_eq!(verdict.candidate_recording_id.as_deref(), Some("mbid-my boy"));
    assert_eq!(
        verdict.r#match.as_ref().map(|m| m.recording_id.as_str()),
        Some("mbid-my boy")
    );
    assert_eq!(verdict.matched_title.as_deref(), Some("my boy"));
    assert!((verdict.score - 0.97).abs() < 0.0005);
    assert_eq!(verdict.deny_reason, "");
}

#[test]
fn a_link_two_people_made_confirms_on_its_own() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 2, Some(171))],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 172);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.reason, InconclusiveReason::None);
}

/// The same audio on an album and a compilation, one submission each: two people in all.
#[test]
fn two_recordings_of_one_submission_each_confirm() {
    let mut album = sized("my boy", "Billie Eilish", 1, Some(171));
    album.recording_id = "mbid-album".into();
    let mut compilation = sized("my boy", "Billie Eilish", 1, Some(171));
    compilation.recording_id = "mbid-compilation".into();
    let lookup = ok(vec![result(0.97, vec![album, compilation])]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 172);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
}

/// One submission naming another song is one person's word: never a refusal on its own, and it
/// names no recording to tag the file from or to send AcoustID.
#[test]
fn a_lone_link_to_another_song_is_a_question_not_a_mismatch() {
    let lookup = ok(vec![result(
        0.96,
        vec![sized("Picture to Burn", "Taylor Swift", 1, None)],
    )]);

    let verdict = decide_at(&lookup, "Taylor Swift", "Cruel Summer", 0);

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::LoneLink);
    assert_eq!(verdict.deny_reason, "");
    assert_eq!(verdict.recording_id, None);
    assert_eq!(verdict.r#match, None);
    assert_eq!(verdict.candidate_recording_id, None);
    assert_eq!(verdict.describe(), "'Taylor Swift - Picture to Burn'");
    assert!((verdict.score - 0.96).abs() < 0.0005);
}

/// A take one person's submissions name is not refused on that alone either.
#[test]
fn a_lone_link_to_a_refused_take_is_a_question() {
    let mut live = live_only();
    live.sources = 1;
    let verdict = decide_live(&live_lookup(vec![live]), true);

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::LoneLink);
    assert_eq!(verdict.deny_reason, "");
    assert_eq!(verdict.recording_id, None);
}

#[test]
fn a_tagged_isrc_leaves_a_lone_link_alone() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 1, Some(171))],
    )]);
    let lone = decide_at(&lookup, "Billie Eilish", "my boy", 172);
    assert_eq!(lone.reason, InconclusiveReason::LoneLink);

    assert_eq!(
        DownloadVerificationService::with_tagged_isrc(lone.clone(), Some(ISRC), true),
        lone
    );

    let elsewhere = decide_at(
        &ok(vec![result(
            0.96,
            vec![sized("Picture to Burn", "Taylor Swift", 1, None)],
        )]),
        "Taylor Swift",
        "Cruel Summer",
        0,
    );
    assert_eq!(
        DownloadVerificationService::with_tagged_isrc(elsewhere.clone(), Some(ISRC), true),
        elsewhere
    );
}

/// The ISRC names the song in its own script, but one person's submissions alone tie the sound
/// to it: the recording is carried for the preview to settle, never confirmed yet.
#[test]
fn settle_by_isrc_with_only_lone_recordings_gives_a_lone_link() {
    let isrcs = isrcs(&[("mbid-gurenge", &[ISRC]), ("mbid-other", &[])]);

    // One submission each: the text alone is a lone link to another song.
    let lone = native_script_with(1);
    let before = VerificationResult {
        fingerprint: Some("AQAD".into()),
        duration_seconds: 239,
        ..decide(&lone, "LiSA", "Gurenge", false)
    };
    assert_eq!(before.reason, InconclusiveReason::LoneLink);
    assert_eq!(before.recording_id, None);
    let settled =
        DownloadVerificationService::settle_by_isrc(before, &lone, THRESHOLD, true, ISRC, &isrcs, false);
    assert_eq!(settled.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(settled.reason, InconclusiveReason::LoneLink);
    assert_eq!(settled.recording_id.as_deref(), Some("mbid-gurenge"));
    assert_eq!(settled.candidate_recording_id.as_deref(), Some("mbid-gurenge"));
    assert_eq!(settled.fingerprint.as_deref(), Some("AQAD"));
    assert!(settled.tags_authoritative);

    // A believed mismatch whose ISRC recording only one person tied to the sound.
    let mut gurenge = AcoustIdRecording::new("mbid-gurenge", "紅蓮華", ["LiSA"], None, None);
    gurenge.sources = 1;
    let mut other = AcoustIdRecording::new("mbid-catch", "Catch the Moment", ["LiSA"], None, None);
    other.sources = 5;
    let mixed = ok(vec![result(0.97, vec![gurenge, other])]);
    let mismatch = decide(&mixed, "LiSA", "Gurenge", false);
    assert_eq!(mismatch.verdict, VerificationVerdict::Mismatch);
    let settled =
        DownloadVerificationService::settle_by_isrc(mismatch, &mixed, THRESHOLD, false, ISRC, &isrcs, false);
    assert_eq!(settled.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(settled.reason, InconclusiveReason::LoneLink);
    assert_eq!(settled.recording_id.as_deref(), Some("mbid-gurenge"));
}

/// A lone link to the remix asked for, in a file whose tags name another remix: two single words
/// that disagree, so a question and never a refusal.
#[test]
fn a_lone_link_whose_tags_name_another_version_is_asked_not_refused() {
    use octo_core::fingerprint::recording_identity::RecordingFacts;
    let mut bear_face = g_eazy(
        "3f839c1f-10a7-4640-a9b3-2d100acdae37",
        "Let's Get Lost (Bear//Face remix)",
        242_202,
    );
    bear_face.sources = 1;
    let lone = decide_download(&ok(vec![result(0.99, vec![bear_face])]), BEAR_FACE_ASKED, 242_202);
    assert_eq!(lone.reason, InconclusiveReason::LoneLink);
    let teklife = RecordingFacts {
        recording_id: "5a584600-24a3-47a0-8f9d-d15e58bfa694".to_string(),
        title: "Let's Get Lost (Teklife remix)".to_string(),
        ..Default::default()
    };

    let verdict = DownloadVerificationService::settle_by_tagged_recording(lone, BEAR_FACE_ASKED, &teklife);

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::SourceDisagreed);
    assert!(verdict.needs_review());
    assert_eq!(verdict.deny_reason, "");
    assert_eq!(
        verdict.recording_id.as_deref(),
        Some("3f839c1f-10a7-4640-a9b3-2d100acdae37")
    );
}

#[test]
fn confirmed_by_preview_turns_a_lone_link_into_a_confirmation_and_leaves_the_rest() {
    let lone = decide_at(
        &ok(vec![result(
            0.97,
            vec![sized("my boy", "Billie Eilish", 1, Some(171))],
        )]),
        "Billie Eilish",
        "my boy",
        172,
    );
    let confirmed = DownloadVerificationService::confirmed_by_preview(lone.clone());
    assert_eq!(confirmed.verdict, VerificationVerdict::Confirmed);
    assert_eq!(confirmed.reason, InconclusiveReason::None);
    assert_eq!(confirmed.recording_id, lone.recording_id);
    assert_eq!(confirmed.r#match, lone.r#match);
    assert_eq!(
        confirmed.evidence.as_deref(),
        Some("the song's YouTube stream agrees")
    );
    assert!(!confirmed.needs_review());

    // A lone link to another song names no recording to confirm the file as.
    let elsewhere = decide_at(
        &ok(vec![result(
            0.96,
            vec![sized("Picture to Burn", "Taylor Swift", 1, None)],
        )]),
        "Taylor Swift",
        "Cruel Summer",
        0,
    );
    assert_eq!(
        DownloadVerificationService::confirmed_by_preview(elsewhere.clone()),
        elsewhere
    );

    // Every other verdict is left as it was.
    for other in [
        VerificationResult {
            reason: InconclusiveReason::NoEntry,
            ..Default::default()
        },
        VerificationResult {
            verdict: VerificationVerdict::Mismatch,
            recording_id: Some("rec".into()),
            ..Default::default()
        },
        VerificationResult {
            reason: InconclusiveReason::LengthOff,
            recording_id: Some("rec".into()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            DownloadVerificationService::confirmed_by_preview(other.clone()),
            other
        );
    }
}

#[test]
fn unknown_lengths_are_not_held_against_a_link() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 3, None)],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 0);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
}

/// The same audio on an album and as a radio edit: a weaker recording of the same song is a
/// version of it, not a stray.
#[test]
fn a_weaker_version_of_the_same_song_still_confirms() {
    let lookup = ok(vec![result(
        0.97,
        vec![
            sized("Song (Radio Edit)", "Artist", 5, Some(200)),
            sized("Song", "Artist", 900, Some(200)),
        ],
    )]);

    let verdict = decide_at(&lookup, "Artist", "Song (Radio Edit)", 200);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.recording_id.as_deref(), Some("mbid-Song (Radio Edit)"));
}

/// A request carrying the stray's own ISRC does not bring the stray back.
#[test]
fn the_isrc_of_a_stray_link_does_not_confirm_it() {
    let lookup = bury_a_friend();
    let before = VerificationResult {
        duration_seconds: 193,
        ..decide_at(&lookup, "Billie Eilish", "my boy", 193)
    };
    let isrcs = isrcs(&[("mbid-my-boy", &["USUM71900764"]), ("mbid-bury a friend", &[])]);

    let verdict = DownloadVerificationService::settle_by_isrc(
        before,
        &lookup,
        THRESHOLD,
        false,
        "USUM71900764",
        &isrcs,
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
}

// ---- the numbers in the log -----------------------------------------------------------------

#[test]
fn every_decision_carries_the_threshold_it_was_held_to() {
    let lookup = ok(vec![result(
        0.61,
        vec![recording("Teardrop", "Massive Attack", None, None)],
    )]);
    let verdict = DownloadVerificationService::decide(
        &lookup,
        Some("Massive Attack"),
        Some("Teardrop"),
        0.7,
        false,
        0,
        &Takes::Any,
    );
    assert_eq!(verdict.threshold, 0.7);
    assert_eq!(
        verdict.acoust_id_text().as_deref(),
        Some("AcoustID: no match above 70% (best 61%)")
    );
}

#[test]
fn a_confirmed_match_says_how_sure_acoust_id_was_and_what_it_needed() {
    let verdict = decide(
        &ok(vec![result(
            0.92,
            vec![recording("Teardrop", "Massive Attack", None, None)],
        )]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(
        verdict.acoust_id_text().as_deref(),
        Some("AcoustID: the same recording, 92% sure (needs 85%)")
    );
    // The recording named is not heard again.
    assert!(verdict.heard().is_empty());
}

#[test]
fn a_match_below_the_threshold_names_the_best_score_and_keeps_what_it_heard() {
    let verdict = decide(
        &ok(vec![
            result(0.40, vec![recording("Angel", "Massive Attack", None, None)]),
            result(0.61, vec![recording("Teardrop", "Massive Attack", None, None)]),
        ]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    assert_eq!(verdict.reason, InconclusiveReason::BelowThreshold);
    assert_eq!(
        verdict.acoust_id_text().as_deref(),
        Some("AcoustID: no match above 85% (best 61%)")
    );
    let heard: Vec<String> = verdict.heard().iter().map(|h| h.describe()).collect();
    assert_eq!(
        heard,
        [
            "Massive Attack - Teardrop, 61% sure",
            "Massive Attack - Angel, 40% sure"
        ]
    );
    assert_eq!(verdict.shown_score(), Some(0.61));
    // Nothing counted as a match, so it names no match, only what came closest.
    assert_eq!(verdict.matched_text(), None);
    assert_eq!(
        verdict.closest_text().as_deref(),
        Some("Massive Attack - Teardrop")
    );
}

#[test]
fn a_mismatch_names_the_recording_and_how_sure_acoust_id_was() {
    let verdict = decide(
        &ok(vec![
            result(0.97, vec![recording("Angel", "Massive Attack", None, None)]),
            result(
                0.30,
                vec![recording("Unfinished Sympathy", "Massive Attack", None, None)],
            ),
        ]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        verdict.acoust_id_text().as_deref(),
        Some("AcoustID says this is 'Massive Attack - Angel', 97% sure (needs 85%)")
    );
    let heard: Vec<String> = verdict.heard().iter().map(|h| h.describe()).collect();
    assert_eq!(heard, ["Massive Attack - Unfinished Sympathy, 30% sure"]);
}

#[test]
fn a_live_take_says_it_is_one() {
    let mut live = recording("Teardrop", "Massive Attack", Some("Live at the BBC"), None);
    live.releases = vec![AcoustIdRelease {
        title: Some("Live at the BBC".into()),
        secondary_types: vec!["Live".into()],
        ..Default::default()
    }];
    let verdict = DownloadVerificationService::decide(
        &ok(vec![result(0.95, vec![live])]),
        Some("Massive Attack"),
        Some("Teardrop"),
        THRESHOLD,
        false,
        0,
        &Takes::Only(Default::default()),
    );
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        verdict.acoust_id_text().as_deref(),
        Some("AcoustID says this is a live recording of 'Massive Attack - Teardrop', 95% sure (needs 85%)")
    );
}

#[test]
fn no_entry_says_acoust_id_has_never_heard_the_sound() {
    let verdict = VerificationResult {
        reason: InconclusiveReason::NoEntry,
        lookup: Some(ok(Vec::new())),
        threshold: THRESHOLD,
        ..Default::default()
    };
    assert_eq!(
        verdict.acoust_id_text().as_deref(),
        Some("AcoustID has no entry for this sound")
    );
    assert_eq!(verdict.shown_score(), None);
    assert!(verdict.heard().is_empty());

    // A sound AcoustID knows that no recording is linked to.
    let known = decide(
        &ok(vec![result(0.9, Vec::new())]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    assert_eq!(known.reason, InconclusiveReason::NoEntry);
    assert_eq!(
        known.acoust_id_text().as_deref(),
        Some("AcoustID knows this sound, 90% sure, but no recording is linked to it")
    );
}

#[test]
fn a_length_off_match_says_how_long_the_recording_runs() {
    let mut long = recording("Teardrop", "Massive Attack", None, None);
    long.duration_seconds = Some(400);
    let verdict = DownloadVerificationService::decide(
        &ok(vec![result(0.93, vec![long])]),
        Some("Massive Attack"),
        Some("Teardrop"),
        THRESHOLD,
        false,
        330,
        &Takes::Any,
    );
    assert_eq!(verdict.reason, InconclusiveReason::LengthOff);
    assert_eq!(
        verdict.acoust_id_text().as_deref(),
        Some("AcoustID: the same song, 93% sure (needs 85%), but that recording runs 6:40")
    );
}

#[test]
fn a_file_kept_on_its_isrc_says_what_acoust_id_said_first() {
    let below = decide(
        &ok(vec![result(
            0.61,
            vec![recording("Teardrop", "Massive Attack", None, None)],
        )]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    let kept = DownloadVerificationService::with_tagged_isrc(below, Some("GBAAA9800001"), true);
    assert_eq!(kept.verdict, VerificationVerdict::Confirmed);
    assert_eq!(
        kept.acoust_id_text().as_deref(),
        Some(
            "AcoustID: no match above 85% (best 61%), but the file's own tags carry the requested ISRC GBAAA9800001"
        )
    );

    // Without a lookup at all the ISRC alone speaks.
    let alone = DownloadVerificationService::with_tagged_isrc(
        VerificationResult::default(),
        Some("GBAAA9800001"),
        true,
    );
    assert_eq!(
        alone.acoust_id_text().as_deref(),
        Some("the file's own tags carry the requested ISRC GBAAA9800001")
    );
}

#[test]
fn nothing_is_said_when_acoust_id_was_not_asked() {
    assert_eq!(VerificationResult::inconclusive().acoust_id_text(), None);
    assert_eq!(
        reason(InconclusiveReason::LookupFailed)
            .acoust_id_text()
            .as_deref(),
        Some("AcoustID did not answer")
    );
    // A file with no audio is said elsewhere.
    let undecodable = VerificationResult {
        verdict: VerificationVerdict::Mismatch,
        deny_reason: "delivered a file with no decodable audio".into(),
        ..Default::default()
    };
    assert_eq!(undecodable.acoust_id_text(), None);
}

#[test]
fn heard_keeps_three_best_first_and_each_song_once() {
    let mut popular = recording("Angel", "Massive Attack", None, None);
    popular.sources = 9;
    let mut stray = recording("Teardrop", "Massive Attack", None, None);
    stray.sources = 1;
    let verdict = decide(
        &ok(vec![
            result(0.70, vec![stray, popular]),
            result(0.65, vec![recording("Angel", "massive attack", None, None)]),
            result(
                0.50,
                vec![recording("Safe from Harm", "Massive Attack", None, None)],
            ),
            result(0.45, Vec::new()),
            result(
                0.40,
                vec![recording("Inertia Creeps", "Massive Attack", None, None)],
            ),
            result(0.20, vec![recording("Karmacoma", "Massive Attack", None, None)]),
        ]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    let heard: Vec<String> = verdict.heard().iter().map(|h| h.describe()).collect();
    assert_eq!(
        heard,
        [
            "Massive Attack - Angel, 70% sure",
            "Massive Attack - Safe from Harm, 50% sure",
            "Massive Attack - Inertia Creeps, 40% sure"
        ]
    );
}

// ---- takes that share one fingerprint ----------------------------------------------------------

/// "Welcome to the Black Parade" as AcoustID answered it on 2026-10-12: one fingerprint tied to
/// the studio take, a live take and the instrumental from a promo CD of instrumentals, all titled
/// the same.
fn parade() -> AcoustIdLookup {
    use octo_core::fingerprint::AcoustIdRelease;
    let take = |id: &str, ms: i64, release: Option<&str>, group: &str, secondary: &[&str]| {
        let mut recording = AcoustIdRecording::new(
            id,
            "Welcome to the Black Parade",
            ["My Chemical Romance"],
            Some(group),
            Some(2006),
        );
        recording.duration_seconds = Some(((ms as f64) / 1000.0).round() as i32);
        recording.duration_ms = Some(ms);
        recording.sources = 10;
        recording.releases = vec![AcoustIdRelease {
            title: release.map(str::to_string),
            group_title: Some(group.to_string()),
            secondary_types: secondary.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }];
        recording
    };
    ok(vec![result(
        0.97,
        vec![
            take("studio", 311_106, None, "The Black Parade", &[]),
            take(
                "instrumental",
                309_684,
                Some("The Black Parade (instrumentals)"),
                "The Black Parade",
                &[],
            ),
            take("live", 305_426, None, "The Black Parade Is Dead!", &["Live"]),
        ],
    )])
}

fn parade_verdict(file_ms: i64, takes: &Takes) -> VerificationResult {
    DownloadVerificationService::decide_with(
        &parade(),
        Some("My Chemical Romance"),
        Some("Welcome to the Black Parade"),
        THRESHOLD,
        true,
        ((file_ms as f64 / 1000.0).round() as i32, Some(file_ms)),
        takes,
        &FactsById::new(),
    )
}

fn plain_download() -> Takes {
    Takes::Only(Default::default())
}

#[test]
fn the_exact_length_names_the_live_take_and_a_plain_download_refuses_it() {
    let verdict = parade_verdict(305_427, &plain_download());
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(verdict.recording_id.as_deref(), Some("live"));
    assert_eq!(
        verdict.deny_reason,
        "is a live recording, from 'The Black Parade Is Dead!'"
    );
    assert!(verdict.by_length);
}

#[test]
fn an_instrumental_the_fingerprint_names_alone_is_refused() {
    let mut lookup = parade();
    lookup.results[0]
        .recordings
        .retain(|recording| recording.recording_id == "instrumental");
    let verdict = DownloadVerificationService::decide_with(
        &lookup,
        Some("My Chemical Romance"),
        Some("Welcome to the Black Parade"),
        THRESHOLD,
        true,
        (310, Some(309_690)),
        &plain_download(),
        &FactsById::new(),
    );
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        verdict.deny_reason,
        "is an instrumental, from 'The Black Parade (instrumentals)'"
    );
    assert!(verdict.kind_certain);
}

#[test]
fn takes_a_remaster_apart_are_not_decided_by_length_and_the_studio_take_stands() {
    // 1.4 s between the studio take and the instrumental is within what a remaster drifts.
    for file_ms in [311_107, 309_690] {
        let verdict = parade_verdict(file_ms, &plain_download());
        assert_eq!(verdict.verdict, VerificationVerdict::Confirmed, "{file_ms}");
        assert_eq!(verdict.recording_id.as_deref(), Some("studio"));
        assert_eq!(verdict.kind, Some(RecordingKind::Studio));
        assert!(!verdict.by_length && !verdict.kind_certain);
    }
}

#[test]
fn a_song_already_in_the_library_is_named_for_what_it_is_and_kept() {
    // The Review sweep asks with any take allowed: the live copy is kept, and named live.
    let verdict = parade_verdict(305_427, &Takes::Any);
    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.recording_id.as_deref(), Some("live"));
    assert_eq!(verdict.kind, Some(RecordingKind::Live));
}

#[test]
fn with_no_decisive_length_a_plain_download_is_the_studio_take() {
    let verdict = parade_verdict(309_000, &plain_download());
    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.recording_id.as_deref(), Some("studio"));
    assert!(!verdict.by_length);
}

#[test]
fn a_request_for_the_instrumental_takes_it_unless_covers_and_instrumentals_are_skipped() {
    let asked = Takes::for_request(
        Some("Welcome to the Black Parade (Instrumental)"),
        None,
        &Default::default(),
    );
    let lookup = parade();
    let verdict = DownloadVerificationService::decide_with(
        &lookup,
        Some("My Chemical Romance"),
        Some("Welcome to the Black Parade (Instrumental)"),
        THRESHOLD,
        true,
        (310, Some(309_690)),
        &asked,
        &FactsById::new(),
    );
    // The recording's own title reads plainly, so the title comparer may not call it the
    // instrumental asked for; what matters is that the instrumental is never refused here.
    assert_ne!(
        verdict.deny_reason,
        "is an instrumental, from 'The Black Parade (instrumentals)'"
    );

    let skipped = Takes::for_request(
        Some("Welcome to the Black Parade (Instrumental)"),
        None,
        &std::collections::BTreeSet::from(octo_core::common::skipped_takes::SKIPPED),
    );
    assert!(skipped.refuses(RecordingKind::Instrumental));
}

/// Skip covers and instrumentals, on by default, refuses a cover, a karaoke take and an
/// instrumental even when the title asks for one. Off, only a title that asks gets one.
#[test]
fn takes_follow_skip_covers_and_instrumentals() {
    use octo_core::settings::{AppSettings, SoulseekSettings};

    use crate::services::fingerprint::{AcoustIdRateLimitHandler, AcoustIdRateLimiter};

    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let client = Arc::new(AcoustIdClient::new(
        Arc::new(AcoustIdRateLimitHandler::new(
            Arc::new(AcoustIdRateLimiter::new()),
        )),
        Arc::clone(&settings),
    ));
    let service = DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        client,
        Arc::clone(&settings),
        None,
        None,
    );
    let skipped = [
        RecordingKind::Cover,
        RecordingKind::Karaoke,
        RecordingKind::Instrumental,
    ];

    assert!(
        settings.current().soulseek.skip_covers_and_instrumentals,
        "on by default"
    );
    for title in [
        "Hello (Acoustic Cover)",
        "Hello (Karaoke Version)",
        "Hello (Instrumental)",
        "Hello",
    ] {
        let takes = service.takes_for(Some(title), None);
        for kind in skipped {
            assert!(takes.refuses(kind), "{title}: {kind:?}");
        }
        assert!(!takes.refuses(RecordingKind::Studio), "{title}");
    }

    settings.set(AppSettings {
        soulseek: SoulseekSettings {
            skip_covers_and_instrumentals: false,
            ..Default::default()
        },
        ..Default::default()
    });
    let plain = service.takes_for(Some("Hello"), None);
    for kind in skipped {
        assert!(plain.refuses(kind), "a plain request never gets {kind:?}");
    }
    assert!(
        !service
            .takes_for(Some("Hello (Acoustic Cover)"), None)
            .refuses(RecordingKind::Cover)
    );
    assert!(
        !service
            .takes_for(Some("Hello (Karaoke Version)"), None)
            .refuses(RecordingKind::Karaoke)
    );
    assert!(
        !service
            .takes_for(Some("Hello (Instrumental)"), None)
            .refuses(RecordingKind::Instrumental)
    );
    assert!(
        service
            .takes_for(Some("Hello (Acoustic Cover)"), None)
            .refuses(RecordingKind::Instrumental)
    );
}

#[test]
fn an_instrumental_only_the_database_note_names_is_refused() {
    use octo_core::fingerprint::recording_identity::RecordingFacts;
    let mut recording = recording("So What", "dvsn", Some("A Muse in Her Feelings"), Some(2020));
    recording.recording_id = "inst".to_string();
    let lookup = ok(vec![result(0.95, vec![recording])]);
    let mut facts = FactsById::new();
    facts.insert(
        "inst".to_string(),
        RecordingFacts {
            recording_id: "inst".to_string(),
            disambiguation: Some("instrumental".to_string()),
            ..Default::default()
        },
    );
    let verdict = DownloadVerificationService::decide_with(
        &lookup,
        Some("dvsn"),
        Some("So What"),
        THRESHOLD,
        true,
        (195, Some(194_969)),
        &plain_download(),
        &facts,
    );
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        verdict.deny_reason,
        "is an instrumental, from 'A Muse in Her Feelings'"
    );
}

// ---- the version asked for, the real case of G-Eazy's "Let's Get Lost" ----------------------

const BEAR_FACE_ASKED: &str = "Let's Get Lost (feat. Devon Baldwin) [Bear//Face Remix]";
const ORIGINAL_ASKED: &str = "Let's Get Lost (feat. Devon Baldwin)";

fn g_eazy(id: &str, title: &str, ms: i64) -> AcoustIdRecording {
    AcoustIdRecording {
        duration_ms: Some(ms),
        duration_seconds: Some(((ms + 500) / 1000) as i32),
        sources: 2,
        ..AcoustIdRecording::new(
            id,
            title,
            ["G\u{2010}Eazy"],
            Some("Let's Get Lost Remixes"),
            Some(2015),
        )
    }
}

fn decide_download(lookup: &AcoustIdLookup, title: &str, file_ms: i64) -> VerificationResult {
    DownloadVerificationService::decide_with(
        lookup,
        Some("G-Eazy"),
        Some(title),
        THRESHOLD,
        true,
        (((file_ms + 500) / 1000) as i32, Some(file_ms)),
        &Takes::for_request(Some(title), None, &Default::default()),
        &FactsById::new(),
    )
}

#[test]
fn a_remix_by_someone_else_is_refused_for_the_remix_asked_for() {
    let lookup = ok(vec![result(
        0.99,
        vec![g_eazy(
            "5a584600-24a3-47a0-8f9d-d15e58bfa694",
            "Let's Get Lost (Teklife remix)",
            269_653,
        )],
    )]);
    let verdict = decide_download(&lookup, BEAR_FACE_ASKED, 269_653);
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        verdict.deny_reason,
        "is another version, 'G\u{2010}Eazy - Let's Get Lost (Teklife remix)': a remix by someone else"
    );
}

#[test]
fn the_remix_is_refused_for_the_original_asked_for_and_the_original_for_the_remix() {
    let remix = ok(vec![result(
        0.99,
        vec![g_eazy(
            "3f839c1f-10a7-4640-a9b3-2d100acdae37",
            "Let's Get Lost (Bear//Face remix)",
            242_202,
        )],
    )]);
    let verdict = decide_download(&remix, ORIGINAL_ASKED, 242_202);
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);

    let original = ok(vec![result(
        0.97,
        vec![g_eazy(
            "20cb620c-1b37-4e93-9b4b-c64b0a2faddc",
            "Let\u{2019}s Get Lost",
            240_680,
        )],
    )]);
    let verdict = decide_download(&original, BEAR_FACE_ASKED, 240_680);
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert!(
        verdict.deny_reason.contains("not the remix asked for"),
        "{}",
        verdict.deny_reason
    );
}

#[test]
fn the_remix_asked_for_written_otherwise_is_confirmed() {
    let lookup = ok(vec![result(
        0.99,
        vec![g_eazy(
            "3f839c1f-10a7-4640-a9b3-2d100acdae37",
            "Let's Get Lost (Bear//Face remix)",
            242_202,
        )],
    )]);
    let verdict = decide_download(&lookup, BEAR_FACE_ASKED, 242_202);
    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
}

#[test]
fn every_titled_variant_is_refused_for_the_plain_song() {
    for title in [
        "Heat Waves (Sped Up)",
        "Heat Waves (Slowed + Reverb)",
        "Heat Waves (Nightcore)",
        "Heat Waves (Extended Mix)",
        "Heat Waves (Club Mix)",
        "Heat Waves (Instrumental)",
        "Heat Waves (A Cappella)",
        "Heat Waves (Karaoke Version)",
    ] {
        let lookup = ok(vec![result(
            0.98,
            vec![recording(title, "Glass Animals", Some("Dreamland"), Some(2020))],
        )]);
        let verdict = DownloadVerificationService::decide_with(
            &lookup,
            Some("Glass Animals"),
            Some("Heat Waves"),
            THRESHOLD,
            true,
            (238, None),
            &plain_download(),
            &FactsById::new(),
        );
        assert_eq!(verdict.verdict, VerificationVerdict::Mismatch, "{title}");
    }
}

#[test]
fn a_live_take_asked_for_is_not_answered_by_the_studio_recording_the_database_knows() {
    use octo_core::fingerprint::recording_identity::RecordingFacts;
    let mut studio = recording("Creep", "Radiohead", Some("Pablo Honey"), Some(1993));
    studio.recording_id = "studio".to_string();
    let lookup = ok(vec![result(0.95, vec![studio])]);
    let mut facts = FactsById::new();
    facts.insert(
        "studio".to_string(),
        RecordingFacts {
            recording_id: "studio".to_string(),
            title: "Creep".to_string(),
            ..Default::default()
        },
    );
    let asked = Takes::for_request(Some("Creep (Live)"), None, &Default::default());
    let verdict = DownloadVerificationService::decide_with(
        &lookup,
        Some("Radiohead"),
        Some("Creep (Live)"),
        THRESHOLD,
        true,
        (238, None),
        &asked,
        &facts,
    );
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        verdict.deny_reason,
        "is the studio recording, not a live recording asked for"
    );
    // Without the database's word the studio recording is not held against it.
    let unasked = DownloadVerificationService::decide_with(
        &lookup,
        Some("Radiohead"),
        Some("Creep (Live)"),
        THRESHOLD,
        true,
        (238, None),
        &asked,
        &FactsById::new(),
    );
    assert_eq!(unasked.verdict, VerificationVerdict::Confirmed);
}

#[test]
fn a_file_whose_own_tags_name_another_remix_is_that_remix() {
    use octo_core::fingerprint::recording_identity::RecordingFacts;
    let teklife = RecordingFacts {
        recording_id: "5a584600-24a3-47a0-8f9d-d15e58bfa694".to_string(),
        title: "Let's Get Lost (Teklife remix)".to_string(),
        length_ms: Some(269_653),
        ..Default::default()
    };
    let unsure = VerificationResult {
        reason: InconclusiveReason::NoEntry,
        ..Default::default()
    };
    let verdict =
        DownloadVerificationService::settle_by_tagged_recording(unsure.clone(), BEAR_FACE_ASKED, &teklife);
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        verdict.deny_reason,
        "is another version by its own tags, 'Let's Get Lost (Teklife remix)': a remix by someone else"
    );
    let bear_face = RecordingFacts {
        title: "Let's Get Lost (Bear//Face remix)".to_string(),
        ..teklife.clone()
    };
    let kept =
        DownloadVerificationService::settle_by_tagged_recording(unsure.clone(), BEAR_FACE_ASKED, &bear_face);
    assert_eq!(kept.verdict, VerificationVerdict::Inconclusive);
    let other_song = RecordingFacts {
        title: "Me, Myself & I".to_string(),
        ..teklife
    };
    let kept = DownloadVerificationService::settle_by_tagged_recording(unsure, BEAR_FACE_ASKED, &other_song);
    assert_eq!(kept.verdict, VerificationVerdict::Inconclusive);
}

#[test]
fn another_artists_song_of_the_same_name_and_another_length_is_doubted_not_refused() {
    let superstar = AcoustIdRecording {
        duration_ms: Some(235_800),
        duration_seconds: Some(236),
        sources: 2,
        ..AcoustIdRecording::new(
            "76a71dbe-3b16-4bb3-a7db-cb860d3bbee4",
            "Let's Get Lost",
            ["Superstar"],
            Some("Greatest Hits Vol. One"),
            None,
        )
    };
    let lookup = ok(vec![result(0.998, vec![superstar])]);
    let verdict = decide_download(&lookup, BEAR_FACE_ASKED, 242_202);
    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    let settled = DownloadVerificationService::settle_stray_link(
        verdict.clone(),
        BEAR_FACE_ASKED,
        Some(242),
        Some(242_202),
    );
    assert_eq!(settled.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(settled.reason, InconclusiveReason::SourceDisagreed);
    assert!(settled.needs_review());
    // A file of another length than the song's stays refused, and so does an unknown length.
    let refused = DownloadVerificationService::settle_stray_link(
        verdict.clone(),
        BEAR_FACE_ASKED,
        Some(242),
        Some(269_653),
    );
    assert_eq!(refused.verdict, VerificationVerdict::Mismatch);
    let refused =
        DownloadVerificationService::settle_stray_link(verdict, BEAR_FACE_ASKED, None, Some(242_202));
    assert_eq!(refused.verdict, VerificationVerdict::Mismatch);
}
