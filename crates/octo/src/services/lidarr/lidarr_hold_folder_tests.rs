//! The album's tracks Octo holds while a hearted song's file is proven, and the put-back after a
//! search, against the fake Lidarr: Octo's own imports wait in the album's hold folder until the
//! hearted song passes its gate, a refused candidate is a track of the album in its own right,
//! every held file goes when nothing passes, a late import is judged by every person's saved gate,
//! a kept copy gives way to the song's proven import, and a pick that ends without a gated import
//! is put back step by step.

use octo_core::fingerprint::heart_gate::{HeardReading, HeardRecording};
use octo_core::lidarr::lidarr_client::LidarrReleaseChoice;
use tokio::task::JoinHandle;

use super::*;
use crate::services::lidarr::fake_lidarr::GrabFile;
use crate::services::lidarr::lidarr_pick::put_back_searched;

/// `user`'s gate for Teardrop: what they heard is Lidarr's Teardrop recording, `rec-3`.
fn teardrop_gate(user: &str) -> HeartGate {
    HeartGate {
        user: user.into(),
        origin: AcquisitionOrigin::Heart,
        r: Some(HeardRecording {
            ids: vec!["rec-3".into()],
            title: "Teardrop".into(),
            artist: "Massive Attack".into(),
            ..Default::default()
        }),
        heard: None,
        heard_seconds: None,
        reading: HeardReading::NotHeard,
        stand_in: None,
    }
}

/// `user`'s heart of Teardrop with their own gate, in the background; `last` when Lidarr is the
/// chain's last source.
fn gated_heart(
    f: &Fixture,
    service: &LidarrHeartAcquisitionService,
    user: &str,
    last: bool,
) -> JoinHandle<bool> {
    f.tracker.begin(
        "soulseek",
        "teardrop-heart",
        None,
        Some(user),
        Some("Massive Attack"),
        Some("Teardrop"),
        Some("Mezzanine"),
    );
    let (service, user) = (service.clone(), user.to_string());
    tokio::spawn(async move {
        within(service.try_acquire_track(
            "soulseek",
            "teardrop-heart",
            last,
            Some(&user),
            AcquisitionOrigin::Heart,
            &[teardrop_gate(&user)],
        ))
        .await
    })
}

/// Mezzanine's hold folder.
fn hold_folder(f: &Fixture) -> PathBuf {
    f.root.path().join(".octo-incoming/lidarr-held/rg-mezzanine")
}

/// The files Mezzanine's hold folder holds, by name.
fn held_names(f: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(hold_folder(f))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Every audio file under the octo root but the fake Lidarr's own folder, by its path there.
fn audio_files(f: &Fixture) -> Vec<String> {
    fn walk(folder: &Path, root: &Path, found: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(folder) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if entry.file_name() != ".fake-lidarr" {
                    walk(&path, root, found);
                }
            } else if path
                .extension()
                .is_some_and(|extension| extension == "flac" || extension == "m4a")
            {
                found.push(
                    path.strip_prefix(root)
                        .expect("under the root")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut found = Vec::new();
    walk(f.root.path(), f.root.path(), &mut found);
    found.sort();
    found
}

async fn watching(f: &Fixture, download_id: &str) {
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| entry.owns_grab(download_id))
    })
    .await;
}

async fn over(f: &Fixture) {
    until(|| f.pending.get(ALBUM_ID).is_none() && !f.claims.heart_busy(ALBUM_ID)).await;
}

fn asked_as_heart(f: &Fixture) -> Vec<(String, Vec<String>)> {
    f.asked
        .lock()
        .iter()
        .filter(|asked| asked.origin == Some(AcquisitionOrigin::Heart))
        .map(|asked| (asked.file.clone(), asked.users.clone()))
        .collect()
}

/// A file kept as heard for Teardrop, held for the follow of Mezzanine.
fn hold_kept_copy(f: &Fixture) -> HeldPlacement {
    let folder = f.root.path().join(".octo-incoming/held/lidarr-follow-teardrop");
    std::fs::create_dir_all(&folder).expect("made");
    let held_path = folder.join("03 - Teardrop.m4a");
    std::fs::write(&held_path, b"the version alice heard").expect("written");
    let held = HeldPlacement {
        song: format!("soulseek:{}", f.id("Teardrop")),
        held_path: held_path.to_string_lossy().into_owned(),
        target: f
            .root
            .path()
            .join("Massive Attack/Mezzanine/03 - Teardrop.m4a")
            .to_string_lossy()
            .into_owned(),
        kept: true,
        follows: vec![ALBUM_ID.to_string()],
        users: vec!["alice".into()],
    };
    f.pending.hold_placement(held.clone());
    held
}

/// Octo's import of Mezzanine for a heart of Teardrop, its rows one per step: Angel's file is
/// Octo's first, so it is moved into the hold folder at once and its Lidarr record deleted, and it
/// stays there, untaken, until Teardrop's file passes the heart's gate. Then Angel goes on to the
/// pipeline quietly, as one of the album's tracks under an id of its own, and the hold folder goes.
#[tokio::test]
async fn album_tracks_wait_in_the_hold_folder_until_the_hearted_song_passes() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let service = f.service(None, 30);
    let heart = gated_heart(&f, &service, "alice", false);
    watching(&f, "dl-1").await;

    f.lidarr.set_rows_one_per_step(true);
    f.lidarr.set_command_mode(CommandMode::Held);
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
    // The refresh, the import's files, then Angel's row alone.
    f.lidarr.step();
    f.lidarr.step();
    f.lidarr.step();

    until(|| held_names(&f) == ["01 - Angel.flac"]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        f.taken().is_empty(),
        "nothing lands before the hearted song passes"
    );
    assert!(
        !f.root
            .path()
            .join("Massive Attack/Mezzanine/01 - Angel.flac")
            .exists()
    );
    assert_eq!(f.lidarr.deleted(), [100], "its record went once it was moved");
    assert_eq!(f.pending.get(ALBUM_ID).expect("listed").held_tracks.len(), 1);

    f.lidarr.run_commands();

    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
    assert!(!f.quiet("Teardrop"));
    assert!(f.quiet("Angel"));
    let id_of = |title: &str| {
        f.taken
            .lock()
            .iter()
            .find(|t| t.1 == title)
            .map(|t| t.0.clone())
            .expect("taken")
    };
    assert_eq!(id_of("Teardrop"), f.id("Teardrop"));
    assert_ne!(id_of("Angel"), f.id("Teardrop"));
    assert_eq!(
        asked_as_heart(&f),
        [("03 - Teardrop".to_string(), vec!["alice".to_string()])],
        "only the hearted song is judged by the heart's gate"
    );
    // Lidarr writes no tags in the fake, so the file's own tags are its labels.
    assert!(
        f.asked
            .lock()
            .iter()
            .all(|asked| asked.labels == Some(LabelSource::TheFile))
    );
    over(&f).await;
    assert!(!hold_folder(&f).exists(), "the hold folder goes with its entry");
}

/// The hearted song's file is refused: every held track is deleted by Octo itself (Lidarr's
/// records of them went when they were moved, so Lidarr deletes nothing of Octo's), no file is
/// left in the hold folder or the library, and the pick is put back with every delete leaving the
/// disk alone.
#[tokio::test]
async fn album_tracks_are_deleted_when_the_hearted_song_fails_and_no_file_is_left_in_the_hold_folder() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]).finished(),
    );
    f.refused_for_hearts.lock().insert("Teardrop".into());
    let service = f.service(None, 30);

    assert!(!gated_heart(&f, &service, "alice", true).await.expect("the task"));
    over(&f).await;

    assert!(f.taken().is_empty());
    assert!(!hold_folder(&f).exists());
    assert_eq!(
        audio_files(&f),
        Vec::<String>::new(),
        "no file of Octo's is left anywhere"
    );
    let mut deleted = f.lidarr.deleted();
    deleted.sort_unstable();
    assert_eq!(deleted, [100, 101], "the records went when the files were moved");
    assert_eq!(
        asked_as_heart(&f),
        [("03 - Teardrop".to_string(), vec!["alice".to_string()])]
    );
    let deletes: Vec<String> = f
        .changes()
        .into_iter()
        .filter(|change| {
            change.starts_with("DELETE /api/v1/album") || change.starts_with("DELETE /api/v1/artist")
        })
        .collect();
    assert_eq!(
        deletes,
        [
            "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false",
            "DELETE /api/v1/artist/5?deleteFiles=false&addImportListExclusion=false",
        ]
    );
}

/// Lidarr matched a take of the song to the heard recording, so it is judged first: its sound
/// refuses it, and it goes back to the hold folder, a track of the album in its own right. The
/// next candidate passes, and the take then goes on to the pipeline as one of the album's tracks,
/// quietly, never as the hearted song.
#[tokio::test]
async fn a_refused_candidate_goes_back_and_lands_as_a_track_of_the_album_once_the_song_passes() {
    let f = Fixture::new().await;
    f.lidarr
        .catalog_track(ALBUM_ID, (4, "Teardrop (Take 1)", 4), "rec-3");
    f.lidarr.plan_grab(
        7,
        Grab::of(
            "dl-1",
            &[(4, "Teardrop (Take 1)", "FLAC"), (3, "Teardrop", "FLAC")],
        )
        .finished(),
    );
    f.refused_for_hearts.lock().insert("Teardrop (Take 1)".into());
    let service = f.service(None, 30);

    assert!(gated_heart(&f, &service, "alice", true).await.expect("the task"));
    over(&f).await;

    assert_eq!(
        asked_as_heart(&f),
        [
            ("04 - Teardrop (Take 1)".to_string(), vec!["alice".to_string()]),
            ("03 - Teardrop".to_string(), vec!["alice".to_string()]),
        ]
    );
    assert_eq!(f.taken(), ["Teardrop", "Teardrop (Take 1)"]);
    assert!(f.quiet("Teardrop (Take 1)"));
    let take = f
        .taken
        .lock()
        .iter()
        .find(|t| t.1 == "Teardrop (Take 1)")
        .map(|t| t.0.clone())
        .expect("taken");
    assert_ne!(take, f.id("Teardrop"), "never as the hearted song");
    assert!(!hold_folder(&f).exists());
}

/// A heart times out while Octo's download moves, a second person's heart joins with a gate of
/// their own, and Octo restarts: the late import is judged by every person's gate the pending
/// album saved, never by the shipped checks.
#[tokio::test]
async fn a_late_import_is_gated_from_the_pending_albums_per_person_gates() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let service = f.service(None, 1);
    let alice = gated_heart(&f, &service, "alice", false);
    watching(&f, "dl-1").await;
    let bob = gated_heart(&f, &service, "bob", false);
    assert!(!alice.await.expect("the task"));
    assert!(!bob.await.expect("the task"));
    let saved: Vec<String> = f
        .pending
        .get(ALBUM_ID)
        .expect("listed")
        .gates
        .iter()
        .map(|gate| format!("{}:{}", gate.song, gate.gate.user))
        .collect();
    let song = format!("soulseek:{}", f.id("Teardrop"));
    assert_eq!(saved, [format!("{song}:alice"), format!("{song}:bob")]);
    stopped(&f, &service).await;

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
    let restarted = f.service(None, 1);
    assert_eq!(
        restarted.lidarr_imported(imported(&[TEARDROP])).await,
        LidarrImportOutcome::Late
    );

    until(|| f.taken.lock().len() == 2).await;
    assert_eq!(
        asked_as_heart(&f),
        [(
            "03 - Teardrop".to_string(),
            vec!["alice".to_string(), "bob".to_string()]
        )]
    );
    over(&f).await;
}

/// With Lidarr writing its own tags into what it imports, a file's tags are Lidarr's matching,
/// never a label: the hearted song's file is judged with no labels at all.
#[tokio::test]
async fn with_lidarr_writing_tags_a_hearted_songs_file_has_no_labels() {
    let f = Fixture::new().await;
    f.lidarr.set_write_audio_tags("newFiles");
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);

    assert!(gated_heart(&f, &service, "alice", true).await.expect("the task"));

    let asked = f.asked.lock().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].labels, Some(LabelSource::None));
    let reads = f
        .lidarr
        .requests()
        .iter()
        .filter(|request| request.path.starts_with("/api/v1/config/metadataprovider"))
        .count();
    assert_eq!(reads, 1, "read once for the follow");
}

/// An import of a song Lidarr fetched carries its own gate to the pipeline's check, which applies
/// the import rule by it; the album's other tracks keep the shipped checks. Nothing is held: an
/// import is no heart.
#[tokio::test]
async fn an_imports_file_through_lidarr_carries_its_gate_to_the_check() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]).finished(),
    );
    let service = f.service(None, 30);
    let gate = HeartGate {
        origin: AcquisitionOrigin::Import,
        ..teardrop_gate("alice")
    };

    assert!(
        within(service.try_acquire_track(
            "soulseek",
            "teardrop-heart",
            true,
            Some("alice"),
            AcquisitionOrigin::Import,
            &[gate],
        ))
        .await
    );

    let mut asked: Vec<(String, Option<AcquisitionOrigin>, Vec<String>)> = f
        .asked
        .lock()
        .iter()
        .map(|asked| (asked.title.clone(), asked.origin, asked.users.clone()))
        .collect();
    asked.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        asked,
        [
            ("Angel".to_string(), None, Vec::new()),
            (
                "Teardrop".to_string(),
                Some(AcquisitionOrigin::Import),
                vec!["alice".to_string()]
            ),
        ]
    );
    assert!(!hold_folder(&f).exists(), "nothing was held");
}

/// Lidarr's file of the hearted song waits in the one hold path for AcoustID: the heart's step
/// ends with that file, which the heart holds with, as any source's held file. Read once.
#[tokio::test]
async fn a_hearted_songs_file_that_waits_for_acoust_id_holds_the_heart_with_it() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    f.holding.lock().insert("Teardrop".into());
    let service = f.service(None, 30);

    assert!(!gated_heart(&f, &service, "alice", false).await.expect("the task"));

    let held = service
        .held_file("soulseek", "teardrop-heart")
        .expect("the heart holds with its file");
    assert_eq!(held.files.len(), 1);
    assert!(Path::new(&held.files[0]).is_file());
    assert_eq!(held.users, ["alice"]);
    assert!(
        service.held_file("soulseek", "teardrop-heart").is_none(),
        "read once"
    );
}

/// The heart kept the heard audio while Octo's download still moved (Lidarr last, its timeout
/// passed), and the copy waits for the follow. The song's late import passes Alice's gate: the kept
/// copy is dropped for it, never placed, and Alice ends with the proven file alone.
#[tokio::test]
async fn a_kept_copy_gives_way_to_the_songs_proven_lidarr_import() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!gated_heart(&f, &service, "alice", true).await.expect("the task"));
    watching(&f, "dl-1").await;
    let held = hold_kept_copy(&f);

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    until(|| f.taken.lock().len() == 1).await;
    over(&f).await;
    assert_eq!(f.taken(), ["Teardrop"]);
    assert_eq!(*f.dropped.lock(), [(held.clone(), vec!["alice".to_string()])]);
    assert!(f.placed.lock().is_empty(), "the kept copy is never placed");
    assert!(!Path::new(&held.held_path).exists());
    assert!(f.pending.held().is_empty());
}

/// A folder import of the user's that Lidarr gave a guessed id of Octo's names no client: it is
/// not Octo's, so the webhook's word about it wakes nothing and answers NotOurs, while an import of
/// Octo's own download wakes its follow.
#[tokio::test]
async fn a_folder_imports_webhook_with_a_guessed_id_of_octos_is_not_ours() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let _heart = gated_heart(&f, &service, "alice", false);
    watching(&f, "dl-1").await;

    let mut folder = imported(&[ANGEL]);
    folder.download_client = None;
    assert_eq!(
        service.lidarr_imported(folder).await,
        LidarrImportOutcome::NotOurs
    );
    let mut users_own = imported(&[ANGEL]);
    users_own.download_id = Some("users-own".into());
    assert_eq!(
        service.lidarr_imported(users_own).await,
        LidarrImportOutcome::NotOurs
    );
    // Octo's own, upgrade or not.
    let mut own = imported(&[TEARDROP]);
    own.is_upgrade = true;
    assert_eq!(service.lidarr_imported(own).await, LidarrImportOutcome::Woke);
}

/// Two people who heard different versions of Teardrop wait on one Lidarr import: it is judged by
/// both their gates at once, and each is answered by their own: Alice's gate passed it, so it is
/// hers; Bob's refused it, so his heart is not answered with her file and goes on once the follow
/// ends.
#[tokio::test]
async fn two_people_on_one_import_each_get_only_what_their_own_gate_passed() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    f.refused_users.lock().insert("bob".into());
    let service = f.service(None, 30);
    let alice = gated_heart(&f, &service, "alice", false);
    watching(&f, "dl-1").await;
    let bob = gated_heart(&f, &service, "bob", false);
    until(|| {
        service
            .askers_of(ALBUM_ID)
            .is_some_and(|askers| askers.len() == 2)
    })
    .await;

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    assert!(alice.await.expect("the task"), "hers by her own gate");
    assert!(!bob.await.expect("the task"), "never his by hers");
    assert_eq!(
        asked_as_heart(&f),
        [(
            "03 - Teardrop".to_string(),
            vec!["alice".to_string(), "bob".to_string()]
        )]
    );
    over(&f).await;
}

/// After a restart, a hold folder no pending entry names is left from a pick that is over: it goes
/// with what it holds. One a listed entry names stays.
#[tokio::test]
async fn a_restart_removes_a_hold_folder_no_entry_names() {
    let f = Fixture::new().await;
    let held = f.root.path().join(".octo-incoming/lidarr-held");
    for (folder, file) in [("rg-gone", "01 - Gone.flac"), ("rg-listed", "01 - Kept.flac")] {
        std::fs::create_dir_all(held.join(folder)).expect("made");
        std::fs::write(held.join(folder).join(file), b"a held track").expect("written");
    }
    f.pending.remember(PendingAlbum {
        foreign_album_id: "rg-listed".into(),
        search_command: 3,
        ..Default::default()
    });

    f.service(None, 30).put_back_after_restart().await;

    assert!(!held.join("rg-gone").exists());
    assert!(held.join("rg-listed/01 - Kept.flac").is_file());
}

// ---- Put back ------------------------------------------------------------------------------------

/// Octo's grab fails and Lidarr queues its own new search of the album: nothing is put back while
/// that search waits. Once it ran, its grab (in Octo's window, so Octo's) is removed from the queue
/// first, and only then is the album Octo's add created deleted.
#[tokio::test]
async fn a_re_search_queued_during_the_put_back_finishes_before_the_delete() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = gated_heart(&f, &service, "alice", false);
    watching(&f, "dl-1").await;

    f.lidarr.set_command_mode(CommandMode::Held);
    f.lidarr.fail("dl-1");
    until(|| f.pending.get(ALBUM_ID).is_some_and(|entry| entry.ended.is_some())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let album_deleted = |f: &Fixture| f.changes().iter().any(|c| c.starts_with("DELETE /api/v1/album/"));
    assert!(
        !album_deleted(&f),
        "never while Lidarr's search of the album waits"
    );
    assert!(f.pending.get(ALBUM_ID).is_some());

    f.lidarr
        .plan_grab(7, Grab::of("dl-2", &[(3, "Teardrop", "FLAC")]));
    f.lidarr.set_command_mode(CommandMode::Instant);
    f.lidarr.run_commands();
    f.lidarr.refresh();

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    let changes = f.changes();
    let queue_delete = changes
        .iter()
        .rposition(|c| c.starts_with("DELETE /api/v1/queue/"))
        .expect("the re-search's grab is removed");
    let album_delete = changes
        .iter()
        .position(|c| c.starts_with("DELETE /api/v1/album/7?deleteFiles=false"))
        .expect("the album Octo's add created is deleted");
    assert!(queue_delete < album_delete, "{changes:#?}");
    assert_eq!(
        f.lidarr.searches(),
        2,
        "Octo's search and Lidarr's own, nothing more"
    );
}

/// The put-back after a search, step by step: Octo's own files are deleted, the album's release
/// choice is given back and its rescan waited out, and an import of Octo's that lands meanwhile is
/// found and deleted too, by Octo, from disk, before the album Octo's add created is deleted with
/// its files left alone. A library file the restore's rescan adopted (no import row) is never
/// deleted.
#[tokio::test]
async fn an_import_landing_between_the_deletes_and_the_album_read_is_removed() {
    let f = Fixture::new().await;
    f.lidarr.set_recycle_bin(true);
    f.lidarr.catalog_album(protection());
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    watching(&f, "dl-1").await;
    stopped(&f, &service).await;
    let client = f.client();
    let pinned = client
        .album(7)
        .await
        .expect("read")
        .expect("Lidarr has it")
        .release_choice();
    f.pending.update(ALBUM_ID, |entry| {
        entry.pin = Some(pinned.clone());
        entry.release_choice = Some(LidarrReleaseChoice {
            any_release_ok: true,
            ..pinned.clone()
        });
    });
    // A library file lying in another album's folder of the artist, which the restore's rescan
    // adopts with no import row.
    let adopted = f.lidarr.place_file(
        "/data/music/Massive Attack/Protection/01 - Karmacoma.flac",
        b"the user's own Karmacoma",
    );
    // Octo's download finishes while Octo is down; its import runs only as Lidarr's commands move,
    // which the restore's rescan wait does.
    f.lidarr.set_command_mode(CommandMode::PerPoll);
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
    let entry = f.pending.get(ALBUM_ID).expect("listed");

    put_back_searched(
        &client,
        Some(&f.pending),
        &service.artist_music(),
        &entry,
        Duration::from_millis(20),
    )
    .await
    .expect("put back");

    assert!(
        !f.root.path().join(TEARDROP_FILE).exists(),
        "the import of Octo's that landed meanwhile is gone"
    );
    assert!(
        !f.root
            .path()
            .join(".fake-lidarr/recycle/03 - Teardrop.flac")
            .exists(),
        "Octo deleted it itself, so Lidarr's recycle bin never kept it"
    );
    assert!(adopted.is_file(), "the adopted library file survives");
    assert_eq!(
        f.lidarr.deleted().len(),
        1,
        "only the record of Octo's own import"
    );
    assert!(
        f.changes()
            .iter()
            .any(|c| c == "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false")
    );
    assert!(f.pending.get(ALBUM_ID).is_none(), "forgotten last");
}

/// A late import the pipeline refuses is gone from disk, with Lidarr's recycle bin on and off:
/// Octo deletes the file itself before Lidarr's record, so the bin never keeps it.
#[tokio::test]
async fn a_refused_late_import_is_gone_from_disk_with_and_without_a_recycle_bin() {
    for bin in [false, true] {
        let f = Fixture::new().await;
        f.lidarr.set_recycle_bin(bin);
        let _service = heart_gave_up_on(&f, "dl-1", &[(3, "Teardrop", "FLAC")]).await;
        f.refused.lock().insert("Teardrop".into());
        f.lidarr.complete("dl-1");
        f.lidarr.refresh();

        over(&f).await;
        assert!(f.taken().is_empty(), "bin {bin}");
        assert!(!f.root.path().join(TEARDROP_FILE).exists(), "bin {bin}");
        assert_eq!(audio_files(&f), Vec::<String>::new(), "bin {bin}");
        let recycled = std::fs::read_dir(f.root.path().join(".fake-lidarr/recycle"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(recycled, 0, "bin {bin}: nothing in Lidarr's recycle bin");
    }
}

/// The user's folder import onto another album of the artist, which Lidarr gave a guessed id of
/// Octo's, makes Octo step aside: the user's file and its record are untouched, the album Octo's
/// add created is deleted with its files left alone, and the artist, which now holds the user's
/// file, stays.
#[tokio::test]
async fn a_folder_import_of_the_users_survives_a_refused_pick_and_the_album_delete_leaves_files_alone() {
    let f = Fixture::new().await;
    f.lidarr.catalog_album(protection());
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = gated_heart(&f, &service, "alice", false);
    watching(&f, "dl-1").await;

    f.lidarr.user_folder_import(
        &[GrabFile {
            album_id: 9,
            track_id: 40,
            title: "Karmacoma".into(),
            quality: "FLAC".into(),
            rejection: None,
        }],
        Some("dl-1"),
    );

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    assert!(f.lidarr.deleted().is_empty(), "nothing of the user's is deleted");
    assert_eq!(audio_files(&f).len(), 1);
    assert!(audio_files(&f)[0].ends_with("Karmacoma.flac"));
    let changes = f.changes();
    assert!(
        changes
            .iter()
            .any(|c| c == "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false"),
        "{changes:#?}"
    );
    assert!(!changes.iter().any(|c| c.starts_with("DELETE /api/v1/artist/")));
    assert!(!changes.iter().any(|c| c.starts_with("DELETE /api/v1/album/9")));
}

/// Octo stops while Angel waits in the hold folder; Teardrop's import comes in meanwhile. After the
/// restart the follow resumes with what its entry saved: Teardrop is held too and judged, refused,
/// and both held files are deleted as the pick is put back.
#[tokio::test]
async fn a_restart_mid_pick_resumes_its_held_tracks_and_puts_back() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    f.refused_for_hearts.lock().insert("Teardrop".into());
    let service = f.service(None, 30);
    let _heart = gated_heart(&f, &service, "alice", false);
    watching(&f, "dl-1").await;
    f.lidarr.set_rows_one_per_step(true);
    f.lidarr.set_command_mode(CommandMode::Held);
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
    f.lidarr.step();
    f.lidarr.step();
    f.lidarr.step();
    until(|| held_names(&f) == ["01 - Angel.flac"]).await;
    service.stop_following();
    until(|| !f.pending.is_running(ALBUM_ID)).await;

    f.lidarr.set_command_mode(CommandMode::Instant);
    f.lidarr.run_commands();
    let restarted = f.service(None, 30);
    restarted.put_back_after_restart().await;

    over(&f).await;
    assert!(f.taken().is_empty());
    assert!(!hold_folder(&f).exists());
    assert_eq!(audio_files(&f), Vec::<String>::new());
    assert_eq!(
        asked_as_heart(&f),
        [("03 - Teardrop".to_string(), vec!["alice".to_string()])]
    );
    assert!(
        f.changes()
            .iter()
            .any(|c| c == "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false")
    );
}
