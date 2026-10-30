//! AcoustID's response decides whether a finished download is kept or deleted, so the parser
//! has to tell three things apart that all look like "no match" from a distance: a refusal, a
//! track AcoustID has never heard of, and a confident identification of something else.

use super::*;

fn parse(json: &str) -> AcoustIdLookup {
    let doc: Value = serde_json::from_str(json).expect("test JSON parses");
    parse_lookup(&doc).expect("the lookup reads")
}

/// The separator is load-bearing. The form encoding writes a literal '+' as %2B, so a
/// '+'-joined meta reaches AcoustID as ONE unknown token; it answers 200 with a real score
/// and no metadata, every result has zero recordings, and the verdict is permanently
/// Inconclusive. The feature then accepts every file forever while looking healthy, which
/// is exactly the silent no-op this whole design is meant to avoid.
#[test]
fn meta_fields_are_space_separated() {
    assert!(!META_FIELDS.contains('+'));
    assert_eq!(
        META_FIELDS.split(' ').collect::<Vec<_>>(),
        [
            "recordings",
            "releasegroups",
            "releases",
            "tracks",
            "compress",
            "isrcs",
            "sources"
        ]
    );
    // And on the wire the spaces become '+', never %2B.
    let body = encode_form(&build_lookup_form("key", "AQAD", 200));
    assert!(body.ends_with("meta=recordings+releasegroups+releases+tracks+compress+isrcs+sources"));
}

/// Every release of every group is kept for the chooser, each with its own id, date,
/// country, medium and track position, while the one pick the old fields read from is
/// unchanged. The shape is the live answer of 2026-10-01: a date object, a country string,
/// medium_count, and the track's own id.
#[test]
fn parse_lookup_keeps_every_release_and_the_old_pick_is_unchanged() {
    let lookup = parse(
        r#"
        {"status": "ok", "results": [{"id": "5745be34-ef80-4c5d-a99b-022b1c3ce567", "score": 0.99, "recordings": [{
          "id": "rec-1", "title": "Human", "duration": 172.253, "sources": 5, "isrcs": ["QMCE32000213", "qm-ce3-20-00213", "bad"],
          "artists": [{"id": "a1", "name": "$NOT", "joinphrase": " feat. "}, {"id": "a2", "name": "Night Lovell"}],
          "releasegroups": [
            {"id": "g-comp", "title": "Trap Hits", "type": "Album", "secondarytypes": ["Compilation"],
             "artists": [{"id": "va", "name": "Various Artists"}],
             "releases": [{"id": "r-comp", "country": "US", "date": {"year": 2021, "month": 1}, "medium_count": 2,
               "mediums": [{"position": 2, "track_count": 16, "tracks": [{"id": "t-comp", "position": 9, "title": "Human"}]}]}]},
            {"id": "g-single", "title": "Human", "type": "Single",
             "artists": [{"id": "a1", "name": "$NOT", "joinphrase": " feat. "}, {"id": "a2", "name": "Night Lovell"}],
             "releases": [
               {"id": "r-single", "country": "XW", "date": {"day": 22, "month": 5, "year": 2020}, "medium_count": 1,
                "mediums": [{"position": 1, "track_count": 1, "tracks": [{"id": "t-single", "position": 1, "title": "Human"}]}]},
               {"id": "r-single-2", "title": "Human (Explicit)", "country": "GB", "date": {"year": 2020}, "medium_count": 1,
                "mediums": [{"position": 1, "track_count": 1, "tracks": [{"id": "t-single-2", "position": 1}]}]}]}
          ]}]}]}
        "#,
    );

    assert_eq!(lookup.results.len(), 1);
    let result = &lookup.results[0];
    assert_eq!(result.id.as_deref(), Some("5745be34-ef80-4c5d-a99b-022b1c3ce567"));
    assert_eq!(result.recordings.len(), 1);
    let recording = &result.recordings[0];
    assert_eq!(recording.sources, 5);
    assert_eq!(recording.isrcs, ["QMCE32000213"]);

    assert_eq!(recording.releases.len(), 3);
    let comp = &recording.releases[0];
    assert_eq!(comp.release_id.as_deref(), Some("r-comp"));
    assert_eq!(comp.release_group_id.as_deref(), Some("g-comp"));
    assert_eq!(comp.group_title.as_deref(), Some("Trap Hits"));
    assert_eq!(comp.primary_type.as_deref(), Some("Album"));
    assert_eq!(comp.secondary_types, ["Compilation"]);
    assert!(comp.is_compilation);
    assert_eq!(comp.country.as_deref(), Some("US"));
    assert_eq!(comp.date.as_deref(), Some("2021-01"));
    assert_eq!(comp.year, Some(2021));
    assert_eq!(comp.disc_number, Some(2));
    assert_eq!(comp.disc_count, Some(2));
    assert_eq!(comp.track_number, Some(9));
    assert_eq!(comp.track_count, Some(16));
    assert_eq!(comp.release_track_id.as_deref(), Some("t-comp"));
    assert_eq!(comp.album_artist_ids, ["va"]);

    let single = &recording.releases[1];
    assert_eq!(single.release_id.as_deref(), Some("r-single"));
    assert_eq!(single.primary_type.as_deref(), Some("Single"));
    assert_eq!(single.date.as_deref(), Some("2020-05-22"));
    assert_eq!(single.country.as_deref(), Some("XW"));
    assert_eq!(single.title.as_deref(), Some("Human"));
    assert_eq!(single.album_artist.as_deref(), Some("$NOT feat. Night Lovell"));
    assert_eq!(single.album_artist_ids, ["a1", "a2"]);
    assert_eq!(recording.releases[2].title.as_deref(), Some("Human (Explicit)"));
    assert_eq!(recording.releases[2].date.as_deref(), Some("2020"));

    // The old pick: the first plain album group, else the first group; here the compilation.
    assert_eq!(
        recording.release.as_ref().and_then(|r| r.release_id.as_deref()),
        Some("r-comp")
    );
    assert_eq!(recording.album_title.as_deref(), Some("Trap Hits"));
    assert_eq!(recording.year, Some(2021));
}

#[test]
fn parse_lookup_no_isrcs_or_sources_reads_as_none() {
    let lookup = parse(
        r#"{"status": "ok", "results": [{"score": 0.99, "recordings": [{"id": "r1", "title": "Song", "artists": [{"name": "A"}]}]}]}"#,
    );
    let recording = &lookup.results[0].recordings[0];
    assert_eq!(lookup.results[0].id, None);
    assert!(recording.isrcs.is_empty());
    assert_eq!(recording.sources, 0);
    assert!(recording.releases.is_empty());
}

#[test]
fn parse_lookup_real_response_reads_score_title_artist_album_and_year() {
    let lookup = parse(
        r#"
        {
          "status": "ok",
          "results": [{
            "id": "9ff43b6a-4f16-427c-93c2-92307ca505e0",
            "score": 0.97,
            "recordings": [{
              "id": "0a1b2c3d-0000-0000-0000-000000000001",
              "title": "Teardrop",
              "artists": [{ "name": "Massive Attack" }, { "name": "Elizabeth Fraser" }],
              "releasegroups": [{
                "id": "rg-1", "title": "Mezzanine", "type": "Album",
                "releases": [{ "date": { "year": 2011 } }, { "date": { "year": 1998 } }]
              }]
            }]
          }]
        }
        "#,
    );

    assert!(lookup.is_ok);
    assert_eq!(lookup.results.len(), 1);
    let result = &lookup.results[0];
    assert!((result.score - 0.97).abs() < 0.0005);

    assert_eq!(result.recordings.len(), 1);
    let recording = &result.recordings[0];
    assert_eq!(recording.title, "Teardrop");
    // Not "Massive Attack, Elizabeth Fraser": Navidrome never splits an artist on a comma, so
    // that credit became one artist, and one folder, that neither of them owns (#49).
    assert_eq!(recording.artist_credit(), "Massive Attack & Elizabeth Fraser");
    assert_eq!(recording.album_title.as_deref(), Some("Mezzanine"));
    // The earliest release, because a 2011 reissue is not the track's year.
    assert_eq!(recording.year, Some(1998));
}

/// The Deezer trap, reproduced here: refusal arrives as HTTP 200 with an error envelope.
/// Reading `is_ok` from the status code would turn every over-budget call into "no match",
/// which this feature reads as "accept the file".
#[test]
fn parse_lookup_error_envelope_in_a_200_is_not_ok() {
    let lookup = parse(r#"{"status": "error", "error": {"code": 6, "message": "invalid API key"}}"#);

    assert!(!lookup.is_ok);
    assert_eq!(lookup.error.as_deref(), Some("invalid API key"));
    assert!(lookup.results.is_empty());
}

/// A compilation or a live album must not supply the album name and year for a studio
/// track, so a release group carrying secondarytypes loses to a plain one.
#[test]
fn parse_lookup_compilation_release_group_loses_to_the_plain_album() {
    let lookup = parse(
        r#"
        {
          "status": "ok",
          "results": [{
            "score": 0.99,
            "recordings": [{
              "id": "r1", "title": "Song", "artists": [{ "name": "Artist" }],
              "releasegroups": [
                { "title": "Now That's What I Call Music! 42", "type": "Album",
                  "secondarytypes": ["Compilation"], "releases": [{ "date": { "year": 1999 } }] },
                { "title": "The Real Album", "type": "Album",
                  "releases": [{ "date": { "year": 1997 } }] }
              ]
            }]
          }]
        }
        "#,
    );

    let recording = &lookup.results[0].recordings[0];
    assert_eq!(recording.album_title.as_deref(), Some("The Real Album"));
    assert_eq!(recording.year, Some(1997));
}

/// AcoustID knows the audio but has no MusicBrainz link for it. Nothing can be decided,
/// so nothing is.
#[test]
fn parse_lookup_result_with_no_recordings_yields_no_metadata() {
    let lookup = parse(r#"{"status": "ok", "results": [{"id": "x", "score": 0.99}]}"#);

    assert!(lookup.is_ok);
    assert_eq!(lookup.results.len(), 1);
    assert!(lookup.results[0].recordings.is_empty());
}

/// The most important case in the whole feature. A legitimately obscure track, which is
/// the music Soulseek is best at and the reason Octo uses it, has no AcoustID entry at
/// all. Rejecting on absence would make verification worst exactly where the library is
/// rarest.
#[test]
fn parse_lookup_no_results_at_all_is_an_ok_lookup_with_nothing_to_say() {
    let lookup = parse(r#"{"status": "ok", "results": []}"#);

    assert!(lookup.is_ok);
    assert!(lookup.results.is_empty());
}

#[test]
fn parse_lookup_malformed_recording_fields_are_skipped_not_fatal() {
    let lookup = parse(
        r#"
        {"status": "ok", "results": [{"score": 0.9, "recordings": [
          {"id": "r1", "title": "Song", "artists": [{"nope": "x"}], "releasegroups": []}
        ]}]}
        "#,
    );

    let recording = &lookup.results[0].recordings[0];
    assert!(recording.artists.is_empty());
    assert_eq!(recording.album_title, None);
    assert_eq!(recording.year, None);
}

/// Trimmed from a live answer to meta=recordings releasegroups releases tracks compress
/// (2026-09-25, the docs' example track). The first group is a Various Artists soundtrack
/// compilation and must lose; within the real album the EARLIEST release supplies the ids and
/// the track position, and compress has dropped the release title because it equals the
/// group's.
#[test]
fn parse_lookup_tracks_meta_reads_release_ids_and_track_position() {
    let lookup = parse(
        r#"
        {"status": "ok", "results": [{"id": "9ff43b6a-4f16-427c-93c2-92307ca505e0", "score": 1.0,
          "recordings": [{
            "id": "cd2e7c47-16f5-46c6-a37c-a1eb7bf599ff",
            "title": "Lower Your Eyelids to Die With the Sun",
            "duration": 637.333,
            "artists": [{"id": "6d7b7cd4-254b-4c25-83f6-dd20f98ceacd", "name": "M83"}],
            "releasegroups": [
              {"id": "9e585041-f2c1-3f0d-be40-40c845a3323f", "title": "Donkey Punch", "type": "Album",
               "secondarytypes": ["Compilation", "Soundtrack"],
               "artists": [{"id": "89ad4ac3-39f7-470e-963a-56509c546377", "name": "Various Artists"}],
               "releases": [{"id": "11de51d7-32c0-4f8b-8df8-1dce7e65245f", "date": {"year": 2008, "month": 7, "day": 28},
                 "mediums": [{"position": 1, "track_count": 16, "tracks": [{"id": "t0", "position": 16}]}]}]},
              {"id": "ddaa2d4d-314e-3e7c-b1d0-f6d207f5aa2f", "title": "Before the Dawn Heals Us", "type": "Album",
               "artists": [{"id": "6d7b7cd4-254b-4c25-83f6-dd20f98ceacd", "name": "M83"}],
               "releases": [
                 {"id": "fad5e4b4-13ac-3f6c-9915-1f0267780df7", "date": {"year": 2005, "month": 1, "day": 25},
                  "mediums": [{"position": 1, "track_count": 15, "tracks": [{"id": "t1", "position": 15}]}]},
                 {"id": "db85c244-53e7-441c-bab0-52c9c0d27450", "date": {"year": 2005, "month": 1, "day": 24},
                  "mediums": [{"position": 1, "track_count": 15, "tracks": [{"id": "t2", "position": 15}]}]},
                 {"id": "e719659b-f591-4faf-ae77-f7f9ccc921c0", "date": {"year": 2014, "month": 8, "day": 26},
                  "mediums": [{"position": 2, "track_count": 6, "tracks": [{"id": "t3", "position": 6}]}]}
               ]}
            ]
          }]
        }]}
        "#,
    );

    let recording = &lookup.results[0].recordings[0];
    assert_eq!(recording.album_title.as_deref(), Some("Before the Dawn Heals Us"));
    assert_eq!(recording.year, Some(2005));
    assert_eq!(recording.duration_seconds, Some(637));
    assert_eq!(recording.primary_artist(), Some("M83"));
    assert_eq!(recording.credits.len(), 1);
    assert_eq!(
        recording.credits[0].artist_id.as_deref(),
        Some("6d7b7cd4-254b-4c25-83f6-dd20f98ceacd")
    );

    let release = recording.release.as_ref().expect("a release was picked");
    assert_eq!(
        release.release_id.as_deref(),
        Some("db85c244-53e7-441c-bab0-52c9c0d27450")
    );
    assert_eq!(
        release.release_group_id.as_deref(),
        Some("ddaa2d4d-314e-3e7c-b1d0-f6d207f5aa2f")
    );
    assert_eq!(release.title.as_deref(), Some("Before the Dawn Heals Us"));
    assert_eq!(release.track_number, Some(15));
    assert_eq!(release.track_count, Some(15));
    assert_eq!(release.disc_number, Some(1));
    assert_eq!(release.album_artist.as_deref(), Some("M83"));
    assert!(!release.is_compilation);
}

#[test]
fn parse_lookup_join_phrases_build_the_credit_music_brainz_prints() {
    let lookup = parse(
        r#"
        {"status": "ok", "results": [{"score": 0.99, "recordings": [{
          "id": "r1", "title": "Under Pressure",
          "artists": [{"id": "a1", "name": "Queen", "joinphrase": " & "}, {"id": "a2", "name": "David Bowie"}]
        }]}]}
        "#,
    );

    let recording = &lookup.results[0].recordings[0];
    assert_eq!(recording.artist_credit(), "Queen & David Bowie");
    assert_eq!(recording.primary_artist(), Some("Queen"));
    assert_eq!(recording.artists, ["Queen", "David Bowie"]);
}

#[test]
fn artist_credit_without_join_phrases_never_comma_joins_two_artists() {
    let cases: &[(&[&str], &str)] = &[
        (&["Bizarrap", "Rauw Alejandro"], "Bizarrap & Rauw Alejandro"),
        (&["A", "B", "C"], "A, B & C"),
        (&["Earth, Wind & Fire"], "Earth, Wind & Fire"),
    ];
    for &(names, expected) in cases {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        assert_eq!(
            AcoustIdRecording::new("r", "t", names, None, None).artist_credit(),
            expected,
            "{expected}"
        );
    }
}

/// With only compilations to choose from, the one picked still names the album, and it says
/// so, which is what keeps a single from being filed as a hundred-track various-artists album.
#[test]
fn pick_release_various_artists_group_is_a_compilation() {
    let lookup = parse(
        r#"
        {"status": "ok", "results": [{"score": 0.99, "recordings": [{
          "id": "r1", "title": "Song", "artists": [{"name": "Artist"}],
          "releasegroups": [{"id": "g1", "title": "Summer Hits", "type": "Album",
            "artists": [{"name": "Various Artists"}],
            "releases": [{"id": "rel1", "date": {"year": 2001}}]}]
        }]}]}
        "#,
    );

    let release = lookup.results[0].recordings[0].release.as_ref().expect("picked");
    assert!(release.is_compilation);
    assert_eq!(release.album_artist.as_deref(), Some("Various Artists"));
    assert_eq!(release.release_id.as_deref(), Some("rel1"));
}

/// A field of the wrong kind ends the read with an error, so the client reads it as a failed
/// lookup rather than half an answer.
#[test]
fn a_field_of_the_wrong_kind_fails_the_whole_read() {
    let doc: Value = serde_json::from_str(r#"{"status": "ok", "results": [{"recordings": [{"id": 5}]}]}"#)
        .expect("parses");
    assert!(parse_lookup(&doc).is_err());
}

#[test]
fn the_submit_form_numbers_every_item() {
    let items = [
        AcoustIdSubmission {
            fingerprint: "AQAD".into(),
            duration_seconds: 239,
            recording_id: "mbid-1".into(),
            file_format: Some("FLAC".into()),
        },
        AcoustIdSubmission {
            fingerprint: "AQAE".into(),
            duration_seconds: 100,
            recording_id: "mbid-2".into(),
            file_format: None,
        },
    ];
    let form = build_submit_form("client", "user", &items);
    let names: Vec<&str> = form.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "client",
            "user",
            "format",
            "clientversion",
            "duration.0",
            "fingerprint.0",
            "mbid.0",
            "fileformat.0",
            "duration.1",
            "fingerprint.1",
            "mbid.1"
        ]
    );
    assert_eq!(form[3].1, format!("octo-{}", octo_user_agent::version()));
}

#[test]
fn artist_credit_joins_with_the_phrases_or_the_usual_ones() {
    let credited = AcoustIdRecording {
        credits: vec![
            AcoustIdCredit::new("Massive Attack", Some("a1"), " feat. "),
            AcoustIdCredit::new("Elizabeth Fraser", Some("a2"), ""),
        ],
        ..AcoustIdRecording::new("r", "Teardrop", ["Massive Attack"], None, None)
    };
    assert_eq!(credited.artist_credit(), "Massive Attack feat. Elizabeth Fraser");
    assert_eq!(credited.primary_artist(), Some("Massive Attack"));

    let three = [
        AcoustIdCredit::new("A", None, ""),
        AcoustIdCredit::new("B", None, ""),
        AcoustIdCredit::new("C", None, ""),
    ];
    assert_eq!(AcoustIdRecording::join_credits(&three), "A, B & C");

    let named = AcoustIdRecording::new("r", "t", ["A", "B", "C"], None, None);
    assert_eq!(named.artist_credit(), "A, B & C");
    assert_eq!(named.primary_artist(), Some("A"));
    assert_eq!(AcoustIdRecording::default().artist_credit(), "");
    assert_eq!(AcoustIdRecording::default().primary_artist(), None);
}

fn answer_of(json: &str) -> Result<AcoustIdAnswer, AnswerError> {
    let doc: Value = serde_json::from_str(json).expect("test JSON parses");
    parse_answer(&doc).expect("the answer reads")
}

/// The whole answer keeps every result and every recording, while the shipped reading still
/// keeps 10 results of 25 recordings for its own readers.
#[test]
fn parse_answer_keeps_every_result_and_recording() {
    let recordings: Vec<String> = (0..30)
        .map(|n| format!(r#"{{"id": "rec-{n}", "title": "Help!", "sources": 1}}"#))
        .collect();
    let results: Vec<String> = (0..12)
        .map(|n| {
            format!(
                r#"{{"id": "fp-{n}", "score": 0.9, "recordings": [{}]}}"#,
                recordings.join(", ")
            )
        })
        .collect();
    let json = format!(r#"{{"status": "ok", "results": [{}]}}"#, results.join(", "));
    let whole = answer_of(&json).expect("the status is ok");
    assert_eq!(whole.results.len(), 12);
    assert!(whole.results.iter().all(|result| result.recordings.len() == 30));
    assert_eq!(whole.results[11].id.as_deref(), Some("fp-11"));
    assert_eq!(whole.results[11].recordings[29].id, "rec-29");
    let cut = parse(&json);
    assert_eq!(cut.results.len(), 10);
    assert_eq!(cut.results[0].recordings.len(), 25);
}

/// Each linked recording keeps its id, title, artists with their ids, length, normalized ISRCs
/// and submissions as AcoustID sent them; a recording with no id links nothing.
#[test]
fn parse_answer_reads_each_linked_recording() {
    let whole = answer_of(
        r#"
        {"status": "ok", "results": [
          {"id": "fp-1", "score": 0.97, "recordings": [
            {"id": "rec-1", "title": "Human", "duration": 172.253, "sources": 5,
             "isrcs": ["QMCE32000213", "qm-ce3-20-00213", "bad"],
             "artists": [{"id": "a1", "name": "$NOT"}, {"name": "Night Lovell"}, {"id": "a3"},
                         {"joinphrase": " & "}]},
            {"title": "No id", "sources": 9},
            {"id": "rec-2", "duration": 0, "sources": -2}
          ]},
          {"score": null, "recordings": null},
          {}
        ]}
        "#,
    )
    .expect("the status is ok");
    assert_eq!(whole.results.len(), 3);
    let first = &whole.results[0];
    assert_eq!(first.id.as_deref(), Some("fp-1"));
    assert!((first.score - 0.97).abs() < 1e-12);
    assert_eq!(first.recordings.len(), 2);
    let human = &first.recordings[0];
    assert_eq!(
        (
            human.id.as_str(),
            human.title.as_str(),
            human.duration_seconds,
            human.sources
        ),
        ("rec-1", "Human", Some(172.253), 5)
    );
    assert_eq!(human.isrcs, ["QMCE32000213"]);
    assert_eq!(
        human.artists,
        [
            AnswerArtist {
                id: Some("a1".into()),
                name: "$NOT".into()
            },
            AnswerArtist {
                id: None,
                name: "Night Lovell".into()
            },
            AnswerArtist {
                id: Some("a3".into()),
                name: String::new()
            },
        ]
    );
    let bare = &first.recordings[1];
    assert_eq!(
        (
            bare.id.as_str(),
            bare.title.as_str(),
            bare.duration_seconds,
            bare.sources
        ),
        ("rec-2", "", None, -2)
    );
    assert!(bare.artists.is_empty() && bare.isrcs.is_empty());
    assert_eq!(whole.results[1], AnswerResult::default());
    assert_eq!(whole.results[2], AnswerResult::default());
}

/// A refusal is AcoustID's own error code and message, never an empty answer, so it can never be
/// read as "no entry".
#[test]
fn parse_answer_reads_a_status_error_as_its_code() {
    assert_eq!(
        answer_of(r#"{"status": "error", "error": {"code": 3, "message": "invalid fingerprint"}}"#),
        Err(AnswerError {
            code: Some(3),
            message: Some("invalid fingerprint".into()),
        })
    );
    assert_eq!(answer_of(r#"{"status": "error"}"#), Err(AnswerError::default()));
    assert_eq!(answer_of(r#"{"results": []}"#), Err(AnswerError::default()));
    assert_eq!(answer_of(r#"{"status": "ok"}"#), Ok(AcoustIdAnswer::default()));
}

/// A field of the wrong kind ends the whole read with an error, which the client reads as an
/// answer it could not read rather than half an answer.
#[test]
fn a_field_of_the_wrong_kind_fails_the_whole_answer() {
    for json in [
        "[]",
        r#"{"status": 1}"#,
        r#"{"status": "ok", "results": {}}"#,
        r#"{"status": "ok", "results": [{"score": "high"}]}"#,
        r#"{"status": "ok", "results": [{"recordings": [{"id": 5}]}]}"#,
        r#"{"status": "ok", "results": [{"recordings": [{"id": "r", "sources": 2.5}]}]}"#,
        r#"{"status": "ok", "results": [{"recordings": [{"id": "r", "artists": ["The Beatles"]}]}]}"#,
        r#"{"status": "error", "error": {"code": "3"}}"#,
    ] {
        let doc: Value = serde_json::from_str(json).expect("test JSON parses");
        assert!(parse_answer(&doc).is_err(), "{json}");
    }
}
