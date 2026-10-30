//! A person's keep, through the real download service: a hearted album's held file lands as it is,
//! nothing looked up or searched, marked "kept by you, version not proven" with no recording id or
//! ISRC of its own or its peer's, named from the catalog, kept for its holder with its own print;
//! a keep of the heard audio lands only the very stream that played, and waits for YouTube when the
//! heard video does not answer.

use octo_core::fingerprint::heart_gate::{HeardRecord, HeardSource, WaitingFor};

use super::*;
use crate::services::common::AcquisitionHeld;
use crate::services::common::heart_acquisition_coordinator::KEEP_WAITS_FOR_YOUTUBE;
use crate::services::local::{ILocalLibraryService, LocalLibraryService};

const KEPT_MARK: &str = "kept by you, version not proven";

/// A library that keeps its mappings in `root`, as Octo's own does.
fn library_in(root: &Root) -> Arc<LocalLibraryService> {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    settings.set_raw("Library:DownloadPath", Some(&root.path()));
    let http = reqwest::Client::new();
    Arc::new(LocalLibraryService::new(
        settings.clone(),
        http.clone(),
        Arc::new(ExternalIdRegistry::in_memory()),
        NavidromeIdentityService::new(settings, http),
    ))
}

/// The gated walk with `library` as the library and AcoustID on `key`.
async fn gated_into(
    root: &Root,
    source: DownloadSource,
    key: &'static str,
    library: &Arc<LocalLibraryService>,
) -> Gated {
    gated_with(
        root,
        source,
        Vec::new(),
        Options {
            key,
            library: Some(library.clone() as Arc<dyn ILocalLibraryService>),
            ..Default::default()
        },
    )
    .await
}

/// brandon's gate for the held track "Started" of a hearted album: its own recording, nothing heard.
fn album_track_gate() -> HeartGate {
    HeartGate {
        origin: AcquisitionOrigin::AlbumHeart,
        heard: None,
        reading: HeardReading::NotHeard,
        ..started_gate()
    }
}

/// A keep of "Started" run for `origin`, for these gates, with these held files.
fn keeping(gates: Vec<HeartGate>, held_files: Vec<String>, origin: AcquisitionOrigin) -> AcquisitionPlace {
    AcquisitionPlace {
        origin: Some(origin),
        gates,
        held_files,
        keep_anyway: true,
        ..Default::default()
    }
}

/// "Keep these anyway" lands a held track's file as it is: nothing looked up, nothing searched,
/// marked "kept by you, version not proven", with no recording id or ISRC tag (not even the ones
/// its peer tagged it with), named from the catalog, and kept for its holder with its own print, so
/// a later check can read its sound. Each other held file of the track goes.
#[tokio::test]
async fn a_kept_held_file_lands_as_it_is_kept_by_you_with_no_recording_id_or_isrc() {
    let root = Root::new("octo-keep-held-");
    let library = library_in(&root);
    let g = gated_into(&root, DownloadSource::Soulseek, "key", &library).await;
    let held = tempfile::tempdir().expect("the hold folder");
    let file = held.path().join("held.flac");
    sound_flac(
        &file,
        180,
        &sound_bytes("started-held", "held-master", "started-held"),
        Some("USCM51400004"),
    );
    {
        let mut tags = octo_media::tags::TagFile::open(&file).expect("the held file opens");
        octo_media::tags::tag_writer_extras::set_recording_id(&mut tags, "rec-started");
        tags.set_title(Some("Started (Its Peer's Name)"));
        tags.save().expect("tagged as its peer tagged it");
    }
    let second = held.path().join("second.flac");
    std::fs::write(&second, flac_of(180, "other", "other", "other", None)).expect("written");
    let gate = album_track_gate();

    let kept = acquire_as(
        &g,
        DownloadSource::Soulseek,
        keeping(
            vec![gate.clone()],
            vec![
                file.to_string_lossy().to_string(),
                second.to_string_lossy().to_string(),
            ],
            AcquisitionOrigin::AlbumHeart,
        ),
    )
    .await
    .expect("kept");

    assert!(g.answers.asked().is_empty(), "nothing was looked up");
    assert!(
        g.walk.slskd.state.lock().searches.is_empty(),
        "nothing was searched"
    );
    assert!(!second.exists(), "the track's other held file went");
    let mut tags = octo_media::tags::TagFile::open(&kept).expect("the kept file opens");
    assert_eq!(tags.isrc(), None, "no ISRC tag");
    assert_eq!(
        octo_media::tags::tag_writer_extras::read_text(&tags, octo_media::tags::TagFields::ISRC),
        None
    );
    assert_eq!(
        octo_media::tags::tag_writer_extras::read_recording_id(&mut tags),
        None,
        "no recording id"
    );
    assert_eq!(tags.title().as_deref(), Some("Started"), "named from the catalog");
    let mapping = library
        .get_mappings()
        .await
        .into_iter()
        .find(|mapping| mapping.external_id == g.started())
        .expect("registered");
    assert_eq!(mapping.proof_mark.as_deref(), Some(KEPT_MARK));
    assert_eq!(mapping.music_brainz_recording_id, None);
    assert!(mapping.kept_unproven());
    let heart = mapping.heart.expect("kept for its holder");
    assert_eq!(heart.users, ["brandon"]);
    assert_eq!(heart.recording, gate.r, "the track's own recording");
    assert_eq!(heart.heard, None);
    assert!(!heart.as_heard, "nobody heard it");
    assert!(heart.print.is_some(), "its own print, for a later check");
}

/// A keep whose held file is gone searches for nothing and fails.
#[tokio::test]
async fn a_keep_with_no_held_file_left_searches_for_nothing() {
    let root = Root::new("octo-keep-gone-");
    let library = library_in(&root);
    let g = gated_into(&root, DownloadSource::Soulseek, "key", &library).await;

    let kept = acquire_as(
        &g,
        DownloadSource::Soulseek,
        keeping(
            vec![album_track_gate()],
            vec![root.0.path().join("gone.flac").to_string_lossy().to_string()],
            AcquisitionOrigin::AlbumHeart,
        ),
    )
    .await;

    assert!(kept.is_err(), "{kept:?}");
    assert!(g.walk.slskd.state.lock().searches.is_empty());
    assert!(g.answers.asked().is_empty());
    assert_eq!(placed_flacs(&root), 0);
}

/// A song kept anyway keeps only the audio its person heard: the very stream of the heard video,
/// at its heard format and size, kept as the heard audio and marked "kept by you, version not
/// proven". With the shim not answering for that video, the keep waits for YouTube with its line,
/// where a heart's own chain would end; a heart of the same song that is no keep never waits so.
#[tokio::test]
async fn a_kept_song_lands_the_heard_stream_and_waits_when_youtube_does_not_answer() {
    let root = Root::new("octo-keep-heard-");
    let library = library_in(&root);
    let g = gated_into(&root, DownloadSource::YouTube, "off", &library).await;
    let stream = aac_stream();
    let heard = HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Video {
            id: HEARD.into(),
            size: Some(stream.len() as u64),
            content_type: "audio/mp4".into(),
            itag: Some(140),
        },
        sample_id: None,
    };
    g.shim_streams(HEARD, Some(140), stream).await;
    let gate = gate_of("brandon", Some(heard.clone()), HeardReading::OnlyHeardOrMaster);

    let kept = acquire_as(
        &g,
        DownloadSource::YouTube,
        keeping(vec![gate], Vec::new(), AcquisitionOrigin::Heart),
    )
    .await
    .expect("the heard stream is kept");

    assert!(kept.ends_with(".m4a"), "{kept}");
    assert!(g.walk.slskd.state.lock().you_tube_downloads.is_empty());
    let mapping = library
        .get_mappings()
        .await
        .into_iter()
        .find(|mapping| mapping.external_id == g.started())
        .expect("registered");
    assert_eq!(mapping.proof_mark.as_deref(), Some(KEPT_MARK));
    let kept_heart = mapping.heart.expect("kept for brandon");
    assert!(kept_heart.as_heard, "it is the very audio that played");
    assert_eq!(
        kept_heart.heard.map(|kept| kept.source),
        Some(heard.source.clone())
    );

    // The shim does not answer for the heard video.
    let root = Root::new("octo-keep-busy-");
    let library = library_in(&root);
    let g = gated_into(&root, DownloadSource::YouTube, "off", &library).await;
    Mock::given(path("/stream"))
        .and(query_param("id", HEARD))
        .and(query_param("fresh", "1"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("X-Octo-Fresh", "1")
                .set_body_raw(r#"{"error":"busy","fresh":true}"#, "application/json"),
        )
        .with_priority(1)
        .mount(&g.walk._server)
        .await;
    let busy_gate = gate_of("brandon", Some(heard), HeardReading::OnlyHeardOrMaster);

    let waits = acquire_as(
        &g,
        DownloadSource::YouTube,
        keeping(vec![busy_gate.clone()], Vec::new(), AcquisitionOrigin::Heart),
    )
    .await
    .expect_err("it waits");
    let held = waits.downcast_ref::<AcquisitionHeld>().expect("held");
    assert_eq!(held.waiting_for, WaitingFor::Shim);
    assert_eq!(held.line, KEEP_WAITS_FOR_YOUTUBE);
    assert_eq!(held.users, ["brandon"]);

    let heart = acquire_as(
        &g,
        DownloadSource::YouTube,
        heart_place(vec![busy_gate], &GateLedger::default()),
    )
    .await
    .expect_err("nothing to keep");
    assert!(
        heart.downcast_ref::<AcquisitionHeld>().is_none(),
        "a heart's own chain ends here: {heart}"
    );
    assert_eq!(placed_flacs(&root), 0);
}
