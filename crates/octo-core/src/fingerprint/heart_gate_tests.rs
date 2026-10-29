use super::*;
use chrono::{DateTime, TimeZone, Utc};

fn at(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 28, 12, minute, 0).unwrap()
}

/// A video source is written by its field names, and reads back the same.
#[test]
fn a_video_source_is_written_by_its_field_names() {
    let video = HeardSource::Video {
        id: "abc".into(),
        size: Some(4_000_000),
        content_type: "audio/mp4".into(),
        itag: Some(140),
    };
    let text = serde_json::to_string(&video).unwrap();
    assert_eq!(
        text,
        r#"{"Video":{"Id":"abc","Size":4000000,"ContentType":"audio/mp4","Itag":140}}"#
    );
    assert_eq!(serde_json::from_str::<HeardSource>(&text).unwrap(), video);
    assert_eq!(video.video_id(), Some("abc"));
}

/// A copy source writes only what it knows, and its ready time in Octo's date form.
#[test]
fn a_copy_source_writes_only_what_it_knows() {
    let copy = HeardSource::Copy {
        key: "soulseek_a".into(),
        ready_at: None,
        source_file: None,
    };
    let text = serde_json::to_string(&copy).unwrap();
    assert_eq!(text, r#"{"Copy":{"Key":"soulseek_a"}}"#);
    assert_eq!(serde_json::from_str::<HeardSource>(&text).unwrap(), copy);
    let dated = HeardSource::Copy {
        key: "soulseek_a".into(),
        ready_at: Some(at(5)),
        source_file: Some("Help!.flac".into()),
    };
    let text = serde_json::to_string(&dated).unwrap();
    assert!(text.contains(r#""ReadyAt":"2026-10-28T12:05:00Z""#), "{text}");
    assert_eq!(serde_json::from_str::<HeardSource>(&text).unwrap(), dated);
    assert_eq!(dated.video_id(), None);
}

/// Waits and origins are written by their names, which `name()` gives too.
#[test]
fn waits_and_origins_are_written_by_their_names() {
    for waiting in WaitingFor::ALL {
        let text = serde_json::to_string(&waiting).unwrap();
        assert_eq!(text, format!("\"{}\"", waiting.name()));
        let back: WaitingFor = serde_json::from_str(&text).unwrap();
        assert_eq!(back, waiting);
    }
    for origin in AcquisitionOrigin::ALL {
        let text = serde_json::to_string(&origin).unwrap();
        assert_eq!(text, format!("\"{}\"", origin.name()));
        let back: AcquisitionOrigin = serde_json::from_str(&text).unwrap();
        assert_eq!(back, origin);
    }
}

/// A gate round-trips through its saved form.
#[test]
fn a_heart_gate_round_trips() {
    let gate = HeartGate {
        user: "brandon".into(),
        origin: AcquisitionOrigin::Heart,
        r: Some(HeardRecording {
            ids: vec!["a1".into()],
            isrcs: vec!["GBAYE0601465".into()],
            title: "Help!".into(),
            artist: "The Beatles".into(),
            ..Default::default()
        }),
        heard: Some(HeardRecord {
            heard_at: at(1),
            source: HeardSource::Library {
                path: "/music/help.flac".into(),
            },
            sample_id: Some("s1".into()),
        }),
        heard_seconds: Some(139),
        reading: HeardReading::RStays {
            tag: Some("a1".into()),
        },
        stand_in: None,
    };
    let text = serde_json::to_string(&gate).unwrap();
    let back: HeartGate = serde_json::from_str(&text).unwrap();
    assert_eq!(back, gate);
    assert!(text.contains(r#""Reading":{"RStays":{"Tag":"a1"}}"#), "{text}");
}

/// An album context round-trips with its times, and an unplayed one writes no played time.
#[test]
fn an_album_context_round_trips_with_its_times() {
    let context = AlbumContext {
        user: "brandon".into(),
        client: Some(NATIVE_CLIENT.into()),
        catalog_album_id: "302127".into(),
        album: Some("Help!".into()),
        disc: Some(1),
        track: Some(13),
        total: Some(14),
        isrc: Some("GBAYE0601477".into()),
        explicit: Some(0),
        listed_at: at(2),
        played_at: Some(at(3)),
    };
    let text = serde_json::to_string(&context).unwrap();
    let back: AlbumContext = serde_json::from_str(&text).unwrap();
    assert_eq!(back, context);
    let unplayed = AlbumContext {
        played_at: None,
        ..context
    };
    let text = serde_json::to_string(&unplayed).unwrap();
    assert!(!text.contains("PlayedAt"), "{text}");
}

/// R holds its ids whatever their case.
#[test]
fn r_holds_its_ids_ignoring_case() {
    let r = HeardRecording {
        ids: vec!["ab12".into()],
        ..Default::default()
    };
    assert!(r.holds("AB12"));
    assert!(!r.holds("cd34"));
}

/// A busy clip reading says what it waits for.
#[test]
fn a_busy_clip_reading_names_what_it_waits_for() {
    let reading = HeardReading::Clip(ClipReading::Busy(WaitingFor::Deezer));
    let text = serde_json::to_string(&reading).unwrap();
    assert_eq!(text, r#"{"Clip":{"Busy":"Deezer"}}"#);
    let back: HeardReading = serde_json::from_str(&text).unwrap();
    assert_eq!(back, reading);
}

/// A file's facts start with only its answer and its labels.
#[test]
fn a_file_starts_with_only_its_answer_and_labels() {
    let facts = FileFacts::new(AcoustIdOutcome::Busy, FileLabels::default());
    assert_eq!(facts.answer, AcoustIdOutcome::Busy);
    assert_eq!(facts.heard_master, None);
    assert!(!facts.identical);
    assert!(facts.current_recordings.is_empty());
    assert!(facts.isrc_recordings.is_empty());
}
