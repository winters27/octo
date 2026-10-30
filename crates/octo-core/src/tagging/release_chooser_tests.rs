//! Release chooser tests: the calibration table. Ten downloads as they arrive, each with the
//! candidates its sources would offer, and the release, confidence and album-level outcome each
//! must land on. The fixtures pin outcomes, not numbers, so a constant may be tuned until every
//! row holds.

use super::*;
use crate::common::dotnet::round;
use crate::fingerprint::verification::{VerificationResult, VerificationVerdict};
use crate::metadata::deezer_metadata_service::FullTrackMeta;
use crate::models::domain::song::Song;
use crate::tagging::candidate_sources::CandidateSources;
use crate::tagging::release_details::ReleaseDetails;
use crate::tagging::release_identifier::ReleaseIdentifier;
use crate::tagging::tag_evidence::{FileFacts, IgnoreCaseSet, TagRequest, TagSource};
use crate::tagging::tag_plan::TagReport;

const THIS_YEAR: Option<i32> = Some(2026);

fn request(
    artist: &str,
    title: &str,
    album: Option<&str>,
    track: Option<i32>,
    isrc: Option<&str>,
    duration: Option<i32>,
) -> TagRequest {
    TagRequest {
        artist: artist.into(),
        title: title.into(),
        album: album.map(str::to_string),
        track,
        disc: None,
        duration_seconds: duration,
        isrc: isrc.map(str::to_string),
        catalog_album_id: None,
        catalog_track_id: None,
        version_markers: SongIdentity::distinct_versions(&SongIdentity::parse_title(title, None), None),
        whole_album: false,
        album_group_id: None,
        album_group_title: None,
    }
}

fn plain(artist: &str, title: &str, duration: i32) -> TagRequest {
    request(artist, title, None, None, None, Some(duration))
}

fn peer(seconds: i32, title: &str, artist: &str) -> FileFacts {
    FileFacts {
        duration_seconds: seconds,
        extension: ".flac".into(),
        sample_rate: 44100,
        title: Some(title.into()),
        artist: Some(artist.into()),
        tags_are_evidence: true,
        ..Default::default()
    }
}

fn upload(seconds: i32) -> FileFacts {
    FileFacts {
        duration_seconds: seconds,
        extension: ".mp3".into(),
        sample_rate: 44100,
        title: Some("Uploader Name - Song".into()),
        artist: Some("Some Channel".into()),
        album: Some("Some Channel".into()),
        tags_are_evidence: false,
        ..Default::default()
    }
}

fn evidence(request: TagRequest, file: FileFacts, fingerprinted: &[&str]) -> TagEvidence {
    TagEvidence {
        request,
        file,
        fingerprint_threshold: 0.85,
        fingerprinted_recording_ids: fingerprinted.iter().copied().collect::<IgnoreCaseSet>(),
        identity: None,
    }
}

/// A fingerprinted candidate to build, with its optional parts as fields.
struct Fp<'a> {
    recording_id: &'a str,
    title: &'a str,
    credit: &'a str,
    album: &'a str,
    group_id: &'a str,
    primary_type: &'a str,
    date: &'a str,
    group_first: Option<&'a str>,
    length: i32,
    status: Option<&'a str>,
    secondary: &'a [&'a str],
}

impl<'a> Fp<'a> {
    fn new(
        recording_id: &'a str,
        title: &'a str,
        credit: &'a str,
        album: &'a str,
        group_id: &'a str,
        primary_type: &'a str,
        date: &'a str,
    ) -> Self {
        Self {
            recording_id,
            title,
            credit,
            album,
            group_id,
            primary_type,
            date,
            group_first: None,
            length: 330,
            status: None,
            secondary: &[],
        }
    }

    fn length(mut self, length: i32) -> Self {
        self.length = length;
        self
    }

    fn group_first(mut self, date: &'a str) -> Self {
        self.group_first = Some(date);
        self
    }

    fn secondary(mut self, secondary: &'a [&'a str]) -> Self {
        self.secondary = secondary;
        self
    }

    fn build(self) -> ReleaseCandidate {
        ReleaseCandidate {
            recording_id: Some(self.recording_id.into()),
            primary_artist: Some(SongIdentity::primary_artist(self.credit)),
            length_seconds: Some(self.length),
            release_id: Some(format!("rel-{}-{}", self.album, self.date)),
            release_group_id: Some(self.group_id.into()),
            release_title: Some(self.album.into()),
            group_title: Some(self.album.into()),
            primary_type: Some(self.primary_type.into()),
            secondary_types: self.secondary.iter().map(|s| s.to_string()).collect(),
            status: self.status.map(str::to_string),
            release_date: Some(self.date.into()),
            group_first_release_date: Some(self.group_first.unwrap_or(self.date).into()),
            fingerprint_id: Some("acoustid-1".into()),
            sources: 10,
            ..ReleaseCandidate::new(TagSource::Fingerprint, self.title, self.credit)
        }
    }
}

fn song_for(request: &TagRequest) -> Song {
    Song {
        artist: request.artist.clone(),
        title: request.title.clone(),
        album: request.album.clone().unwrap_or_default(),
        track: request.track,
        isrc: request.isrc.clone(),
        ..Default::default()
    }
}

fn chosen(plan: &TagPlan) -> &ReleaseCandidate {
    &plan.chosen.as_ref().expect("a chosen candidate").candidate
}

fn strs(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

// ---- A: a lone star finds its studio album -------------------------------------------

fn fixture_a() -> (TagEvidence, Vec<ReleaseCandidate>) {
    let request = plain("Massive Attack", "Teardrop", 330);
    let evidence = evidence(
        request,
        peer(330, "Teardrop", "Massive Attack"),
        &["rec-teardrop"],
    );
    let credit = "Massive Attack feat. Elizabeth Fraser";
    let candidates = vec![
        Fp::new(
            "rec-teardrop",
            "Teardrop",
            credit,
            "Collected",
            "g-collected",
            "Album",
            "2006-03-27",
        )
        .secondary(&["Compilation"])
        .build(),
        Fp::new(
            "rec-teardrop",
            "Teardrop",
            credit,
            "Teardrop",
            "g-single",
            "Single",
            "1998-04-27",
        )
        .build(),
        ReleaseCandidate {
            label: Some("Virgin".into()),
            catalog_number: Some("CDV 2851".into()),
            barcode: Some("724384559922".into()),
            track_number: Some(3),
            track_count: Some(11),
            disc_number: Some(1),
            ..Fp::new(
                "rec-teardrop",
                "Teardrop",
                credit,
                "Mezzanine",
                "g-mezzanine",
                "Album",
                "1998-04-20",
            )
            .build()
        },
    ];
    (evidence, candidates)
}

#[test]
fn a_lone_star_files_under_the_studio_album_the_single_second_and_not_ambiguous() {
    let (evidence, candidates) = fixture_a();

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);

    assert_eq!(plan.confidence, TagConfidence::Strong);
    assert_eq!(chosen(&plan).album_title(), Some("Mezzanine"));
    assert_eq!(round(plan.chosen.as_ref().unwrap().distance, 3), 0.0);
    assert_eq!(plan.ranked[1].candidate.album_title(), Some("Teardrop"));
    assert_eq!(plan.ranked[2].candidate.album_title(), Some("Collected"));

    let mut song = song_for(&evidence.request);
    plan.apply_to(&mut song);
    assert_eq!(song.album, "Mezzanine");
    assert_eq!(song.year, Some(1998));
    assert_eq!(song.original_date.as_deref(), Some("1998-04-20"));
    assert_eq!(song.label.as_deref(), Some("Virgin"));
    assert_eq!(song.catalog_number.as_deref(), Some("CDV 2851"));
    assert_eq!(song.barcode.as_deref(), Some("724384559922"));
    assert_eq!(song.release_type.as_deref(), Some("album"));
    assert_eq!(song.track, Some(3));
    assert_eq!(song.total_tracks, Some(11));
    assert_eq!(song.music_brainz_recording_id.as_deref(), Some("rec-teardrop"));
    assert_eq!(song.acoust_id.as_deref(), Some("acoustid-1"));
    assert_eq!(song.music_brainz_release_group_id.as_deref(), Some("g-mezzanine"));
    assert_eq!(plan.fields["album"].source.as_deref(), Some("Fingerprint"));
}

/// The same answer when the candidates come from a parsed lookup
/// (`acoust_id_client::parse_lookup`) rather than by hand.
#[test]
fn a_from_a_parsed_lookup_gives_the_same_answer() {
    let doc: serde_json::Value = serde_json::from_str(
        r#"
        {"status": "ok", "results": [{"id": "acoustid-1", "score": 0.97, "recordings": [{
          "id": "rec-teardrop", "title": "Teardrop", "duration": 330.2, "sources": 40, "isrcs": ["GBAAA9800001"],
          "artists": [{"id": "a-ma", "name": "Massive Attack", "joinphrase": " feat. "}, {"id": "a-ef", "name": "Elizabeth Fraser"}],
          "releasegroups": [
            {"id": "g-collected", "title": "Collected", "type": "Album", "secondarytypes": ["Compilation"],
             "releases": [{"id": "r-col", "date": {"year": 2006, "month": 3, "day": 27}, "country": "GB",
               "mediums": [{"position": 1, "track_count": 14, "tracks": [{"id": "t-col", "position": 4}]}]}]},
            {"id": "g-single", "title": "Teardrop", "type": "Single",
             "releases": [{"id": "r-single", "date": {"year": 1998, "month": 4, "day": 27}, "country": "GB",
               "mediums": [{"position": 1, "track_count": 4, "tracks": [{"id": "t-s", "position": 1}]}]}]},
            {"id": "g-mezzanine", "title": "Mezzanine", "type": "Album",
             "releases": [
               {"id": "r-mezz-2019", "date": {"year": 2019, "month": 8, "day": 23}, "country": "XE",
                "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m19", "position": 3}]}]},
               {"id": "r-mezz", "date": {"year": 1998, "month": 4, "day": 20}, "country": "GB",
                "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m", "position": 3}]}]}]}
          ]}]}]}
        "#,
    )
    .expect("test JSON parses");
    let lookup = crate::fingerprint::acoust_id_client::parse_lookup(&doc).expect("reads");
    let candidates = CandidateSources::from_fingerprint(Some(&lookup), 0.85);
    let request = plain("Massive Attack", "Teardrop", 330);
    let evidence = evidence(
        request,
        peer(330, "Teardrop", "Massive Attack"),
        &["rec-teardrop"],
    );

    let plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);

    assert_eq!(candidates.len(), 4);
    assert_eq!(plan.confidence, TagConfidence::Strong);
    let chosen = chosen(&plan);
    assert_eq!(chosen.release_id.as_deref(), Some("r-mezz"));
    assert_eq!(chosen.group_first_release_date.as_deref(), Some("1998-04-20"));
    assert_eq!(chosen.isrcs, ["GBAAA9800001"]);
    assert_eq!(chosen.sources, 40);
    assert_eq!(chosen.release_track_id.as_deref(), Some("t-m"));
    // The 2019 pressing of the same group sits behind the first one, not ahead of it.
    assert_eq!(
        plan.ranked[1].candidate.release_id.as_deref(),
        Some("r-mezz-2019")
    );
}

// ---- B: a reissue of the same album loses to the first -------------------------------

#[test]
fn b_video_upload_prefers_the_original_album_over_its_reissue_not_ambiguous() {
    let request = plain("Radiohead", "No Surprises (Official Video)", 228);
    let evidence = evidence(request, upload(228), &["rec-ns"]);
    let candidates = vec![
        Fp::new(
            "rec-ns",
            "No Surprises",
            "Radiohead",
            "OKNOTOK 1997 2017",
            "g-oknotok",
            "Album",
            "2017-06-23",
        )
        .length(228)
        .build(),
        Fp::new(
            "rec-ns",
            "No Surprises",
            "Radiohead",
            "OK Computer",
            "g-okc",
            "Album",
            "1997-05-21",
        )
        .length(228)
        .build(),
    ];

    let plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);

    assert_eq!(plan.confidence, TagConfidence::Strong);
    assert_eq!(chosen(&plan).album_title(), Some("OK Computer"));
    assert!(
        plan.ranked[1].distance - plan.chosen.as_ref().unwrap().distance > ReleaseChooser::AMBIGUITY_MARGIN
    );
}

// ---- C: an album walk keeps its album and takes the release's facts -------------------

#[test]
fn c_album_walk_request_album_confirmed_release_facts_taken() {
    let request = request(
        "Daft Punk",
        "One More Time",
        Some("Discovery"),
        Some(1),
        Some("GBDUW0000059"),
        Some(320),
    );
    let file = FileFacts {
        album: Some("Discovery".into()),
        year: Some(2001),
        isrcs: strs(&["GBDUW0000059"]),
        ..peer(320, "One More Time", "Daft Punk")
    };
    let evidence = evidence(request.clone(), file, &["rec-omt"]);
    let candidates = vec![
        ReleaseCandidate {
            track_number: Some(7),
            isrcs: strs(&["GBDUW0000059"]),
            label: Some("Virgin".into()),
            catalog_number: Some("COMP-1".into()),
            ..Fp::new(
                "rec-omt",
                "One More Time",
                "Daft Punk",
                "Musique Vol. 1 (1993-2005)",
                "g-musique",
                "Album",
                "2006-03-29",
            )
            .secondary(&["Compilation"])
            .build()
        },
        ReleaseCandidate {
            track_number: Some(1),
            track_count: Some(14),
            isrcs: strs(&["GBDUW0000059"]),
            label: Some("Virgin".into()),
            catalog_number: Some("7243 8 49606 2 2".into()),
            barcode: Some("724384960629".into()),
            ..Fp::new(
                "rec-omt",
                "One More Time",
                "Daft Punk",
                "Discovery",
                "g-discovery",
                "Album",
                "2001-03-12",
            )
            .build()
        },
    ];

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    let mut song = song_for(&request);
    song.total_tracks = Some(14);
    plan.apply_to(&mut song);

    assert_eq!(plan.confidence, TagConfidence::Strong);
    assert_eq!(chosen(&plan).album_title(), Some("Discovery"));
    let second = plan.ranked[1].distance;
    assert!((0.1..=0.25).contains(&second), "{second}");
    assert_eq!(song.album, "Discovery");
    assert_eq!(song.track, Some(1));
    assert_eq!(song.catalog_number.as_deref(), Some("7243 8 49606 2 2"));
    assert_eq!(song.barcode.as_deref(), Some("724384960629"));
    assert_eq!(song.isrc.as_deref(), Some("GBDUW0000059"));
    assert_eq!(plan.fields["isrc"].source.as_deref(), Some("Request"));
}

#[test]
fn c_request_album_is_never_overwritten_even_by_a_strong_other_album() {
    let request = request(
        "Daft Punk",
        "One More Time",
        Some("Discovery"),
        Some(1),
        None,
        Some(320),
    );
    let evidence = evidence(request.clone(), upload(320), &["rec-omt"]);
    let candidates = vec![ReleaseCandidate {
        track_number: Some(7),
        label: Some("Virgin".into()),
        catalog_number: Some("COMP-1".into()),
        ..Fp::new(
            "rec-omt",
            "One More Time",
            "Daft Punk",
            "Musique Vol. 1 (1993-2005)",
            "g-musique",
            "Album",
            "2006-03-29",
        )
        .secondary(&["Compilation"])
        .build()
    }];

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    let mut song = song_for(&request);
    plan.apply_to(&mut song);

    assert_eq!(song.album, "Discovery");
    assert_eq!(song.track, Some(1));
    assert_eq!(song.catalog_number, None);
    assert_eq!(song.label, None);
}

// ---- D: the pressing the file came from, the year the album first came out ----------

fn nevermind_peer() -> FileFacts {
    FileFacts {
        album: Some("Nevermind".into()),
        year: Some(2011),
        ..peer(301, "Smells Like Teen Spirit", "Nirvana")
    }
}

#[test]
fn d_peer_tagged_with_the_remaster_year_takes_that_pressing_shows_the_original_year() {
    let request = plain("Nirvana", "Smells Like Teen Spirit", 301);
    let evidence = evidence(request.clone(), nevermind_peer(), &["rec-slts"]);
    let fp = |date: &'static str| {
        Fp::new(
            "rec-slts",
            "Smells Like Teen Spirit",
            "Nirvana",
            "Nevermind",
            "g-nevermind",
            "Album",
            date,
        )
        .length(301)
    };
    let candidates = vec![
        ReleaseCandidate {
            label: Some("DGC".into()),
            catalog_number: Some("DGCD-24425".into()),
            ..fp("1991-09-24").build()
        },
        ReleaseCandidate {
            label: Some("DGC".into()),
            catalog_number: Some("B0015884-02".into()),
            ..fp("2011-09-19").group_first("1991-09-24").build()
        },
    ];

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    let mut song = song_for(&request);
    plan.apply_to(&mut song);

    assert_eq!(plan.confidence, TagConfidence::Strong);
    assert_eq!(chosen(&plan).release_date.as_deref(), Some("2011-09-19"));
    assert_eq!(song.catalog_number.as_deref(), Some("B0015884-02"));
    assert_eq!(song.year, Some(1991));
    assert_eq!(song.original_date.as_deref(), Some("1991-09-24"));
    assert_eq!(plan.to_report().release_date.as_deref(), Some("2011-09-19"));
}

#[test]
fn d_year_from_original_release_off_writes_the_pressings_year() {
    let request = plain("Nirvana", "Smells Like Teen Spirit", 301);
    let evidence = evidence(request.clone(), nevermind_peer(), &["rec-slts"]);
    let candidates = vec![
        Fp::new(
            "rec-slts",
            "Smells Like Teen Spirit",
            "Nirvana",
            "Nevermind",
            "g-nevermind",
            "Album",
            "2011-09-19",
        )
        .group_first("1991-09-24")
        .length(301)
        .build(),
    ];
    let settings = MatchingSettings {
        year_from_original_release: false,
        ..Default::default()
    };

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &settings, THIS_YEAR);
    let mut song = song_for(&request);
    plan.apply_to(&mut song);

    assert_eq!(song.year, Some(2011));
    assert_eq!(song.original_date.as_deref(), Some("1991-09-24"));
}

// ---- E: a live request with no fingerprint finds the live release in the database ----

fn fixture_e() -> (TagEvidence, Vec<ReleaseCandidate>) {
    let request = plain("Portishead", "Glory Box (Live)", 300);
    let evidence = evidence(request, upload(300), &[]);
    let candidates = vec![
        ReleaseCandidate {
            recording_id: Some("rec-studio".into()),
            length_seconds: Some(300),
            release_id: Some("r-dummy".into()),
            release_group_id: Some("g-dummy".into()),
            release_title: Some("Dummy".into()),
            group_title: Some("Dummy".into()),
            primary_type: Some("Album".into()),
            status: Some("Official".into()),
            release_date: Some("1994-08-22".into()),
            group_first_release_date: Some("1994-08-22".into()),
            ..ReleaseCandidate::new(TagSource::Database, "Glory Box", "Portishead")
        },
        ReleaseCandidate {
            recording_id: Some("rec-live".into()),
            length_seconds: Some(300),
            release_id: Some("r-roseland".into()),
            release_group_id: Some("g-roseland".into()),
            release_title: Some("Roseland NYC Live".into()),
            group_title: Some("Roseland NYC Live".into()),
            primary_type: Some("Album".into()),
            secondary_types: strs(&["Live"]),
            status: Some("Official".into()),
            release_date: Some("1998-11-09".into()),
            group_first_release_date: Some("1998-11-09".into()),
            ..ReleaseCandidate::new(
                TagSource::Database,
                "Glory Box (live, 1997-07-24: Roseland Ballroom, New York)",
                "Portishead",
            )
        },
    ];
    (evidence, candidates)
}

#[test]
fn e_live_request_database_search_picks_the_live_release_studio_never_reaches_medium() {
    let (evidence, candidates) = fixture_e();

    let plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);

    assert_eq!(plan.confidence, TagConfidence::Strong);
    assert_eq!(chosen(&plan).album_title(), Some("Roseland NYC Live"));
    assert!(
        plan.ranked[1].distance > ReleaseChooser::MEDIUM_THRESHOLD,
        "studio album at {}",
        plan.ranked[1].distance
    );
}

#[test]
fn e_from_a_parsed_database_search_gives_the_same_answer() {
    let doc: serde_json::Value = serde_json::from_str(
        r#"
    {"created": "2026-10-01T00:00:00.000Z", "count": 2, "offset": 0, "recordings": [
      {"id": "rec-studio", "score": 100, "title": "Glory Box", "length": 300000, "video": false,
       "artist-credit": [{"name": "Portishead", "artist": {"id": "a-p", "name": "Portishead"}}],
       "isrcs": ["GBAAA9400001"],
       "releases": [{"id": "r-dummy", "title": "Dummy", "status": "Official", "date": "1994-08-22", "country": "GB",
         "release-group": {"id": "g-dummy", "title": "Dummy", "primary-type": "Album", "first-release-date": "1994-08-22"},
         "media": [{"position": 1, "format": "CD", "track-count": 11, "track-offset": 10,
           "track": [{"id": "t-gb", "number": "11", "title": "Glory Box", "length": 300000}]}]}]},
      {"id": "rec-live", "score": 95, "title": "Glory Box", "length": 300000, "video": false,
       "disambiguation": "live, 1997-07-24: Roseland Ballroom, New York",
       "artist-credit": [{"name": "Portishead", "artist": {"id": "a-p", "name": "Portishead"}}],
       "releases": [{"id": "r-roseland", "title": "Roseland NYC Live", "status": "Official", "date": "1998-11-09", "country": "GB",
         "release-group": {"id": "g-roseland", "title": "Roseland NYC Live", "primary-type": "Album", "secondary-types": ["Live"], "first-release-date": "1998-11-09"},
         "media": [{"position": 1, "format": "CD", "track-count": 11, "track-offset": 7,
           "track": [{"id": "t-gbl", "number": "8", "title": "Glory Box", "length": 300000}]}]}]}
    ]}
    "#,
    )
    .expect("valid JSON");
    let candidates = CandidateSources::from_database_search(&doc);
    let (evidence, _) = fixture_e();

    let plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);

    assert_eq!(candidates.len(), 2);
    assert_eq!(plan.confidence, TagConfidence::Strong);
    assert_eq!(chosen(&plan).release_id.as_deref(), Some("r-roseland"));
    assert_eq!(chosen(&plan).track_number, Some(8));
    assert_eq!(chosen(&plan).release_track_id.as_deref(), Some("t-gbl"));
    assert_eq!(chosen(&plan).artist_ids, ["a-p"]);
    assert_eq!(plan.ranked[1].candidate.isrcs, ["GBAAA9400001"]);
}

// ---- F: every credit gets its id ----------------------------------------------------

#[test]
fn f_list_of_credits_matches_the_joined_credit_writes_every_artist_id() {
    let request = plain("Bizarrap, Rauw Alejandro", "Bzrp Music Sessions, Vol. 56", 200);
    let evidence = evidence(request.clone(), upload(200), &["rec-bzrp"]);
    let candidates = vec![ReleaseCandidate {
        artists: strs(&["Bizarrap", "Rauw Alejandro"]),
        artist_ids: strs(&["a-bzrp", "a-rauw"]),
        primary_artist: Some("Bizarrap".into()),
        ..Fp::new(
            "rec-bzrp",
            "Bzrp Music Sessions, Vol. 56",
            "Bizarrap & Rauw Alejandro",
            "Bzrp Music Sessions, Vol. 56",
            "g-bzrp",
            "Single",
            "2023-06-29",
        )
        .length(200)
        .build()
    }];

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    let mut song = song_for(&request);
    plan.apply_to(&mut song);

    assert_eq!(plan.confidence, TagConfidence::Strong);
    assert_eq!(song.music_brainz_artist_ids, ["a-bzrp", "a-rauw"]);
    assert_eq!(song.artists, ["Bizarrap", "Rauw Alejandro"]);
    assert_eq!(song.primary_artist.as_deref(), Some("Bizarrap"));
    assert_eq!(song.artist, "Bizarrap, Rauw Alejandro");
}

// ---- G: a catalog-only single is Medium and only fills blanks -----------------------

#[test]
fn g_catalog_only_single_is_medium_and_does_not_set_the_album() {
    let request = plain("Artist", "Song", 200);
    let evidence = evidence(request.clone(), upload(200), &[]);
    let meta = FullTrackMeta {
        album_title: Some("Song".into()),
        album_cover_url: Some("https://cdn/xl.jpg".into()),
        year: Some(2021),
        duration: Some(200),
        artist_name: Some("Artist".into()),
        track_number: Some(1),
        disc_number: Some(1),
        isrc: Some("USAAA2100001".into()),
        total_tracks: Some(1),
        genre: Some("Pop".into()),
        label: Some("Label".into()),
        release_date: Some("2021-05-01".into()),
        contributors: Some(strs(&["Artist"])),
        album_artist_name: Some("Artist".into()),
        record_type: Some("single".into()),
        title: Some("Song".into()),
        track_id: Some("1".into()),
        album_id: Some("2".into()),
        ..Default::default()
    };

    let mut plan = ReleaseChooser::choose(
        &evidence,
        &[CandidateSources::from_catalog(&meta, None)],
        &MatchingSettings::default(),
        THIS_YEAR,
    );
    let mut song = song_for(&request);
    plan.apply_to(&mut song);

    assert_eq!(plan.confidence, TagConfidence::Medium);
    assert!(!plan.album_from_candidate());
    assert_eq!(song.album, "");
    assert_eq!(song.label, None);
    assert_eq!(song.isrc.as_deref(), Some("USAAA2100001"));
    assert!(
        plan.notes.iter().any(|note| note.contains("not taken")),
        "{:?}",
        plan.notes
    );
}

// ---- I: a rip from a compilation, filed under the studio album, or not -----------------

fn fixture_i() -> (TagEvidence, Vec<ReleaseCandidate>) {
    let request = plain("Massive Attack", "Teardrop", 330);
    let file = FileFacts {
        album: Some("Now That's What I Call Music! 42".into()),
        is_compilation: true,
        ..peer(330, "Teardrop", "Massive Attack")
    };
    let evidence = evidence(request.clone(), file.clone(), &["rec-teardrop"]);
    let mut candidates = vec![
        ReleaseCandidate {
            album_artist: Some("Various Artists".into()),
            is_compilation: true,
            ..Fp::new(
                "rec-teardrop",
                "Teardrop",
                "Massive Attack",
                "Now That's What I Call Music! 42",
                "g-now42",
                "Album",
                "1999-04-12",
            )
            .secondary(&["Compilation"])
            .build()
        },
        Fp::new(
            "rec-teardrop",
            "Teardrop",
            "Massive Attack",
            "Mezzanine",
            "g-mezzanine",
            "Album",
            "1998-04-20",
        )
        .build(),
    ];
    candidates.push(CandidateSources::from_file_tags(&file, Some(&request)).expect("the file's candidate"));
    (evidence, candidates)
}

#[test]
fn i_prefer_original_album_on_the_studio_album_replaces_the_compilation() {
    let (evidence, candidates) = fixture_i();

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    let mut song = song_for(&evidence.request);
    plan.apply_to(&mut song);

    assert_eq!(plan.confidence, TagConfidence::Medium);
    assert_eq!(chosen(&plan).album_title(), Some("Mezzanine"));
    assert!(plan.album_from_candidate());
    assert_eq!(song.album, "Mezzanine");
    assert!(!song.is_compilation);
}

#[test]
fn i_prefer_original_album_off_the_compilation_stays() {
    let (evidence, candidates) = fixture_i();
    let settings = MatchingSettings {
        prefer_original_album: false,
        ..Default::default()
    };

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &settings, THIS_YEAR);
    let mut song = song_for(&evidence.request);
    plan.apply_to(&mut song);

    assert_eq!(plan.confidence, TagConfidence::Medium);
    assert_eq!(
        chosen(&plan).album_title(),
        Some("Now That's What I Call Music! 42")
    );
    assert_eq!(chosen(&plan).source, TagSource::Fingerprint);
    assert_eq!(song.album, "Now That's What I Call Music! 42");
    assert!(song.is_compilation);
}

// ---- J: a version the fingerprint does not name ---------------------------------------

#[test]
fn j_remix_requested_original_recording_is_medium_title_never_overwritten() {
    let request = plain("Artist", "Song (Remix)", 240);
    let evidence = evidence(request.clone(), upload(240), &["rec-song"]);
    let candidates = vec![
        Fp::new(
            "rec-song",
            "Song",
            "Artist",
            "The Album",
            "g-album",
            "Album",
            "2015-01-01",
        )
        .length(240)
        .build(),
    ];
    let settings = MatchingSettings {
        tag_from_match: true,
        ..Default::default()
    };

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &settings, THIS_YEAR);
    let mut song = song_for(&request);
    plan.apply_to(&mut song);

    assert_eq!(plan.confidence, TagConfidence::Medium);
    assert_eq!(song.title, "Song (Remix)");
    assert_eq!(song.album, "The Album");
    assert_eq!(song.music_brainz_recording_id.as_deref(), Some("rec-song"));
}

// ---- the rules around the table ------------------------------------------------------

#[test]
fn no_candidates_gives_none_and_leaves_the_song_untouched() {
    let request = request("Artist", "Song", None, None, None, None);
    let mut plan = ReleaseChooser::choose(
        &evidence(request.clone(), upload(0), &[]),
        &[],
        &MatchingSettings::default(),
        THIS_YEAR,
    );
    let mut song = song_for(&request);
    song.album = "Whatever the catalog said".into();

    plan.apply_to(&mut song);

    assert_eq!(plan.confidence, TagConfidence::None);
    assert!(plan.chosen.is_none());
    assert_eq!(song.album, "Whatever the catalog said");
    assert!(plan.fields.is_empty());
}

#[test]
fn ambiguous_pressings_keep_the_recording_id_but_not_the_album() {
    let request = plain("Artist", "Song", 240);
    let evidence = evidence(request.clone(), upload(240), &["rec-song"]);
    let candidates = vec![
        Fp::new(
            "rec-song",
            "Song",
            "Artist",
            "Album A",
            "g-a",
            "Album",
            "1998-01-01",
        )
        .length(240)
        .build(),
        Fp::new(
            "rec-song",
            "Song",
            "Artist",
            "Album B",
            "g-b",
            "Album",
            "1998-01-01",
        )
        .length(240)
        .build(),
    ];

    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    let mut song = song_for(&request);
    plan.apply_to(&mut song);

    assert_eq!(plan.confidence, TagConfidence::Ambiguous);
    assert_eq!(song.album, "");
    assert_eq!(song.music_brainz_recording_id.as_deref(), Some("rec-song"));
    assert!(
        plan.notes.iter().any(|note| note.contains("too close to call")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn two_pressings_of_one_group_are_never_ambiguous() {
    let request = plain("Artist", "Song", 240);
    let evidence = evidence(request, upload(240), &["rec-song"]);
    let candidates = vec![
        Fp::new(
            "rec-song",
            "Song",
            "Artist",
            "Album A",
            "g-a",
            "Album",
            "1998-01-01",
        )
        .length(240)
        .build(),
        Fp::new(
            "rec-song",
            "Song",
            "Artist",
            "Album A",
            "g-a",
            "Album",
            "1998-01-02",
        )
        .group_first("1998-01-01")
        .length(240)
        .build(),
    ];

    let plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);

    assert_eq!(plan.confidence, TagConfidence::Strong);
    assert_eq!(chosen(&plan).release_date.as_deref(), Some("1998-01-01"));
}

#[test]
fn rehearsal_writes_only_the_code_and_the_confirmed_fingerprint_id() {
    let (evidence, candidates) = fixture_a();
    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    let mut song = song_for(&evidence.request);
    song.verification = Some(Box::new(VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        acoust_id: Some("acoustid-1".into()),
        ..Default::default()
    }));

    plan.apply_rehearsal_to(&mut song);

    assert!(plan.rehearsed);
    assert_eq!(song.album, "");
    assert_eq!(song.label, None);
    assert_eq!(song.year, None);
    assert_eq!(song.acoust_id.as_deref(), Some("acoustid-1"));
    assert_eq!(plan.to_report().confidence, "Strong");
}

#[test]
fn report_carries_the_top_candidates_their_penalties_and_the_fields() {
    let (evidence, candidates) = fixture_a();
    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    plan.apply_to(&mut song_for(&evidence.request));
    plan.stage_seconds.insert("catalog".into(), 1.234);
    plan.notes.push("a note".into());

    let report = plan.to_report();

    assert_eq!(report.confidence, "Strong");
    assert_eq!(report.release_title.as_deref(), Some("Mezzanine"));
    assert_eq!(report.candidates.len(), 3);
    assert_eq!(report.candidates[0].album, "Mezzanine");
    assert!(
        report.candidates[2]
            .biggest_penalties
            .iter()
            .any(|p| p.starts_with("type")),
        "{:?}",
        report.candidates[2].biggest_penalties
    );
    assert_eq!(report.fields["album"].value.as_deref(), Some("Mezzanine"));
    assert_eq!(report.stage_seconds["catalog"], 1.23);
    assert!(report.notes.iter().any(|n| n == "a note"));
    let json = crate::json::to_string(&report);
    let back: TagReport = serde_json::from_str(&json).expect("the report reads back");
    assert_eq!(back.fields["album"].value.as_deref(), Some("Mezzanine"));
}

// ---- the tag plan's own rules -----------------------------------------------------------

#[test]
fn report_penalties_are_formatted_with_two_decimals_and_the_log_line_keeps_its_wording() {
    let (evidence, candidates) = fixture_a();
    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    let mut song = song_for(&evidence.request);
    plan.apply_to(&mut song);
    let report = plan.to_report();
    // Collected: a compilation (type 1.00) and a later first release (original 0.32).
    assert_eq!(
        report.candidates[2].biggest_penalties,
        ["type 1.00", "original 0.32"]
    );
    assert_eq!(report.candidates[1].biggest_penalties, ["type 0.20"]);
    assert_eq!(report.candidates[0].kind.as_deref(), Some("album"));
    assert_eq!(report.distance, Some(0.0));
    assert_eq!(report.source.as_deref(), Some("Fingerprint"));

    song.replay_gain_track_gain_db = Some(-6.5);
    plan.stage_seconds.insert("identify".into(), 1.25);
    plan.stage_seconds.insert("details".into(), 0.5);
    assert_eq!(
        plan.describe_song(&song),
        "Tagged 'Massive Attack - Teardrop' as 'Mezzanine' (1998, Virgin CDV 2851) from Fingerprint, Strong (0.000), \
         3 candidates, gain -6.50 dB, 1.8s"
    );
    assert_eq!(TagPlan::describe(chosen(&plan)), "'Mezzanine' (album) 1998");
}

#[test]
fn with_details_fills_what_the_candidate_left_unsaid() {
    let (evidence, candidates) = fixture_a();
    let mut plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
    let details = ReleaseDetails::parse(&serde_json::json!({
        "id": "rel-Mezzanine-1998-04-20", "status": "Official", "country": "GB", "barcode": "000",
        "label-info": [{"catalog-number": "WBRCD4", "label": {"name": "Circa"}}],
        "release-group": {"id": "other-group", "first-release-date": "1998-04-20"},
        "artist-credit": [{"name": "Massive Attack", "artist": {"id": "a-ma"}}],
        "media": [{"position": 1, "track-count": 11, "tracks": [
            {"id": "t-3", "position": 3, "recording": {"id": "REC-TEARDROP", "isrcs": ["GBAAA9800001"]}}]}]
    }))
    .expect("a release");

    plan.with(details);

    let c = chosen(&plan);
    assert_eq!(c.status.as_deref(), Some("Official"));
    assert_eq!(c.country.as_deref(), Some("GB"));
    assert_eq!(c.label.as_deref(), Some("Circa"));
    assert_eq!(c.catalog_number.as_deref(), Some("WBRCD4"));
    // Kept: the candidate already said these.
    assert_eq!(c.release_group_id.as_deref(), Some("g-mezzanine"));
    assert_eq!(c.track_number, Some(3));
    // Taken: the candidate did not say.
    assert_eq!(c.album_artist.as_deref(), Some("Massive Attack"));
    assert_eq!(c.album_artist_ids, ["a-ma"]);
    assert_eq!(c.release_track_id.as_deref(), Some("t-3"));
    assert_eq!(c.disc_count, Some(1));
    assert_eq!(c.isrcs, ["GBAAA9800001"]);
    assert!(plan.details().is_some());

    // Nothing chosen: nothing to add to.
    let mut empty = TagPlan::empty(None, MatchingSettings::default());
    empty.with(ReleaseDetails::default());
    assert!(empty.details().is_none());
}

// ---- a track of a whole album takes facts only from that album ----------------------------

#[test]
fn a_track_of_a_whole_album_takes_no_facts_from_a_larger_edition_of_it() {
    for whole_album in [false, true] {
        let request = TagRequest {
            whole_album,
            ..request(
                "Dua Lipa",
                "Levitating",
                Some("Future Nostalgia"),
                Some(5),
                None,
                Some(203),
            )
        };
        let evidence = evidence(request.clone(), peer(203, "Levitating", "Dua Lipa"), &["rec-lev"]);
        let candidates = vec![ReleaseCandidate {
            label: Some("Warner Moon".into()),
            ..Fp::new(
                "rec-lev",
                "Levitating",
                "Dua Lipa",
                "Future Nostalgia (The Moonlight Edition)",
                "g-moon",
                "Album",
                "2021-02-11",
            )
            .length(203)
            .build()
        }];

        let mut plan =
            ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);
        let mut song = song_for(&request);
        plan.apply_to(&mut song);

        assert!(plan.album_from_candidate(), "{whole_album}");
        assert_eq!(song.album, "Future Nostalgia", "{whole_album}");
        assert_eq!(song.track, Some(5), "{whole_album}");
        assert_eq!(song.music_brainz_recording_id.as_deref(), Some("rec-lev"));
        // Any other request lends the larger edition its facts, as before.
        assert_eq!(
            song.label.as_deref(),
            (!whole_album).then_some("Warner Moon"),
            "{whole_album}"
        );
        assert_eq!(song.year, (!whole_album).then_some(2021), "{whole_album}");
        assert_eq!(
            song.music_brainz_release_group_id.as_deref(),
            (!whole_album).then_some("g-moon"),
            "{whole_album}"
        );
    }
}

// ---- a request with no album takes the group its ISRC picked ------------------------------

#[test]
fn a_request_with_no_album_is_filed_under_the_group_its_isrc_picked() {
    let candidates = vec![
        Fp::new(
            "rec-yesterday",
            "Yesterday",
            "The Beatles",
            "Yesterday and Today",
            "g-yesterday-and-today",
            "Album",
            "1966-06-20",
        )
        .length(125)
        .build(),
        ReleaseCandidate {
            release_title: Some("ヘルプ!".into()),
            ..Fp::new(
                "rec-yesterday",
                "Yesterday",
                "The Beatles",
                "Help!",
                "g-help",
                "Album",
                "1965-08-06",
            )
            .secondary(&["Soundtrack"])
            .length(125)
            .build()
        },
    ];
    let picked = ("g-help".to_string(), "Help!".to_string());
    for (pick, group) in [(None, "g-yesterday-and-today"), (Some(&picked), "g-help")] {
        let request = ReleaseIdentifier::with_album_group(plain("The Beatles", "Yesterday", 125), pick);
        let evidence = evidence(
            request.clone(),
            peer(125, "Yesterday", "The Beatles"),
            &["rec-yesterday"],
        );

        let plan = ReleaseChooser::choose(&evidence, &candidates, &MatchingSettings::default(), THIS_YEAR);

        assert_eq!(chosen(&plan).release_group_id.as_deref(), Some(group), "{pick:?}");
    }

    // A request that named its album keeps it whatever was picked.
    let named = ReleaseIdentifier::with_album_group(
        request("The Beatles", "Yesterday", Some("1"), None, None, Some(125)),
        Some(&picked),
    );
    assert_eq!((named.album_group_id, named.album_group_title), (None, None));
}
