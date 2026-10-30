//! The follow of Octo's search, against the fake Lidarr: whose grabs are Octo's, file by file
//! which imports are Octo's, the watch list and when a download is accounted for, the end states,
//! the files held back while a follow is open, and what a heart waits on when Lidarr is last and
//! MusicBrainz does not answer.

use octo_core::lidarr::lidarr_client::LidarrHistoryEvent;
use tokio::task::JoinHandle;

use super::*;
use crate::services::lidarr::fake_lidarr::GrabFile;
use crate::services::lidarr::{FollowEnd, PROVEN_ELSEWHERE, REPLACED_YOURS, StopWaiting};

/// The song a Teardrop heart's follow lists: the metadata's own id for it.
fn teardrop(f: &Fixture) -> (String, String) {
    ("soulseek".to_string(), f.id("Teardrop"))
}

fn teardrop_key(f: &Fixture) -> String {
    let (provider, id) = teardrop(f);
    format!("{provider}:{id}")
}

/// A heart of Teardrop in the background, its row begun under the song's own id.
fn heart_later(f: &Fixture, service: &LidarrHeartAcquisitionService) -> JoinHandle<bool> {
    let (provider, id) = teardrop(f);
    f.tracker.begin(
        &provider,
        &id,
        None,
        Some("alice"),
        Some("Massive Attack"),
        Some("Teardrop"),
        Some("Mezzanine"),
    );
    let service = service.clone();
    tokio::spawn(async move {
        within(service.try_acquire_track(
            "soulseek",
            "teardrop-heart",
            false,
            Some("alice"),
            AcquisitionOrigin::Heart,
            &[],
        ))
        .await
    })
}

/// The entry of Mezzanine as saved now.
fn entry(f: &Fixture) -> PendingAlbum {
    f.pending.get(ALBUM_ID).expect("Mezzanine is listed")
}

/// Waits until the follow has Octo's grab on its watch list.
async fn followed(f: &Fixture, download_id: &str) {
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| entry.owns_grab(download_id))
    })
    .await;
}

/// Waits until the pick is over: the entry forgotten and the album let go.
async fn over(f: &Fixture) {
    until(|| f.pending.get(ALBUM_ID).is_none() && !f.claims.heart_busy(ALBUM_ID)).await;
}

fn removed(f: &Fixture, queue_id: i32) -> bool {
    f.changes()
        .iter()
        .any(|change| change.starts_with(&format!("DELETE /api/v1/queue/{queue_id}?")))
}

fn album_deleted(f: &Fixture) -> bool {
    f.changes()
        .iter()
        .any(|change| change.starts_with("DELETE /api/v1/album/"))
}

/// Another artist Lidarr holds, with an album: Portishead's Dummy, track 310 "Roads".
fn portishead(f: &Fixture) {
    f.lidarr.catalog_artist("portishead", "Portishead", 30);
    f.lidarr.catalog_album(CatalogAlbum::new(
        "rg-dummy",
        "Dummy",
        "portishead",
        31,
        &[(310, "Roads", 1)],
    ));
    f.lidarr.user_album("rg-dummy", false);
}

/// A download of the user's on another artist, by the same client as Octo's: it stays in the
/// queue, so a queue read proves that client answered.
fn another_artists_download(f: &Fixture) {
    portishead(f);
    f.lidarr.user_grab(
        31,
        Grab::of("other-1", &[(310, "Roads", "FLAC")]).into_album(31),
        "Rss",
    );
}

/// The note lines of a row, with their details.
fn notes(f: &Fixture, key: &str) -> Vec<(String, Option<String>)> {
    f.tracker
        .detail(key, None)
        .and_then(|row| row.events)
        .unwrap_or_default()
        .into_iter()
        .filter(|event| {
            event.kind == crate::services::common::acquisition_tracker::acquisition_event_kinds::NOTE
        })
        .map(|event| (event.text, event.detail))
        .collect()
}

/// A file kept as heard for Teardrop, held for the follow of Mezzanine: what the placement does
/// for a kept copy made while the follow is open.
fn hold_kept_copy(f: &Fixture) -> HeldPlacement {
    let folder = f.root.path().join(".octo-incoming/held/lidarr-follow-teardrop");
    std::fs::create_dir_all(&folder).expect("made");
    let held_path = folder.join("03 - Teardrop.m4a");
    std::fs::write(&held_path, b"the version alice heard").expect("written");
    let held = HeldPlacement {
        song: teardrop_key(f),
        held_path: held_path.to_string_lossy().into_owned(),
        target: f
            .root
            .path()
            .join("Massive Attack/Mezzanine/03 - Teardrop.m4a")
            .to_string_lossy()
            .into_owned(),
        kept: true,
        follows: vec![ALBUM_ID.to_string()],
    };
    f.pending.hold_placement(held.clone());
    held
}

/// Lidarr fails Octo's grab and searches again on its own: the new grab, inside that search's
/// window, is Octo's too, so its import is taken and gated, never read as the user's.
#[tokio::test]
async fn the_regrab_lidarr_makes_in_its_own_window_after_octos_failed_grab_is_gated() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    f.lidarr
        .plan_grab(7, Grab::of("dl-2", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.fail("dl-1");

    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken(), ["Teardrop"]);
    assert_eq!(f.lidarr.searches(), 2, "Octo's search and Lidarr's own");
    over(&f).await;
}

/// A grab of the album from outside every search window of Octo's (the user's interactive
/// search): the album is the user's now. Octo removes its own item and steps aside; the user's
/// grab is never touched and the album stays.
#[tokio::test]
async fn a_grab_outside_every_window_of_octos_makes_octo_step_aside_untouched() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.user_grab(
        7,
        Grab::of("user-1", &[(1, "Angel", "FLAC")]),
        "InteractiveSearch",
    );

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    assert!(removed(&f, 1), "Octo's own item is removed");
    assert!(!removed(&f, 2), "the user's grab is never touched");
    assert!(!album_deleted(&f), "the album holding the user's grab stays");
}

/// A search of the user's for the album that runs in Octo's window: the window's grab cannot be
/// told apart, so it is the user's. Octo takes nothing, removes nothing, and steps aside.
#[tokio::test]
async fn a_users_album_search_overlapping_octos_window_makes_its_grab_the_users() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    f.lidarr.overlap_next_search(7);
    let service = f.service(None, 30);

    let heart = heart_later(&f, &service);

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    assert!(f.taken().is_empty());
    assert!(!removed(&f, 1));
    assert!(!album_deleted(&f));
}

/// A release Lidarr held back for the album (its client was away) is followed by its queue id;
/// when Lidarr's RSS sync drops it, it is accounted for and the pick ends, put back.
#[tokio::test]
async fn a_held_back_release_rss_drops_is_accounted_for_and_the_entry_ends() {
    let f = Fixture::new().await;
    f.lidarr.plan_held_back(7, "downloadClientUnavailable");
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| !entry.pending_items.is_empty())
    })
    .await;
    let queue_id = entry(&f).pending_items[0].queue_id;

    f.lidarr.drop_pending(queue_id);

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    assert!(
        !removed(&f, queue_id),
        "RSS dropped it; Octo had nothing to remove"
    );
}

/// A held-back release that Lidarr grabs after all is followed as Octo's grab.
#[tokio::test]
async fn a_held_back_release_that_becomes_a_grab_is_followed() {
    let f = Fixture::new().await;
    f.lidarr.plan_held_back(7, "delay");
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| !entry.pending_items.is_empty())
    })
    .await;
    let queue_id = entry(&f).pending_items[0].queue_id;

    f.lidarr
        .grab_pending(queue_id, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());

    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken(), ["Teardrop"]);
}

/// Twelve tracks imported at once, their rows written one per read: every file waits for its own
/// row, none is ever the user's, and each is taken.
#[tokio::test]
async fn all_twelve_files_visible_at_once_with_rows_one_per_read_are_each_taken() {
    let f = Fixture::new().await;
    let tracks: Vec<(i32, String, i32)> = (1..=12).map(|n| (800 + n, format!("Part {n}"), n)).collect();
    let catalog: Vec<(i32, &str, i32)> = tracks.iter().map(|(id, t, n)| (*id, t.as_str(), *n)).collect();
    f.lidarr
        .catalog_album(CatalogAlbum::new("rg-twelve", "Twelve", ARTIST_ID, 8, &catalog));
    f.metadata.albums.lock().insert(
        ("soulseek".into(), "twelve".into()),
        Album {
            title: "Twelve".into(),
            artist: "Massive Attack".into(),
            songs: tracks
                .iter()
                .map(|(_, title, n)| Song {
                    title: title.clone(),
                    artist: "Massive Attack".into(),
                    album: "Twelve".into(),
                    track: Some(*n),
                    external_provider: Some("soulseek".into()),
                    external_id: Some(f.id(title)),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
    );
    let files: Vec<(i32, &str, &str)> = tracks
        .iter()
        .map(|(id, t, _)| (*id, t.as_str(), "FLAC"))
        .collect();
    f.lidarr.plan_grab(8, Grab::of("dl-1", &files).into_album(8));
    let service = f.service(None, 30);
    let heart = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_album(
                "soulseek",
                "twelve",
                false,
                Some("alice"),
                AcquisitionOrigin::AlbumHeart,
                &[],
            ))
            .await
        })
    };
    until(|| {
        f.pending
            .get("rg-twelve")
            .is_some_and(|entry| entry.owns_grab("dl-1"))
    })
    .await;

    f.lidarr.set_rows_one_per_step(true);
    f.lidarr.set_command_mode(CommandMode::PerPoll);
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken.lock().len(), 12);
    until(|| f.pending.get("rg-twelve").is_none()).await;
    assert!(!removed(&f, 1), "no file was ever the user's");
}

/// A download client named "SAB Music", whose queue items name no folder: Octo follows its own
/// grab by its id and takes the import.
#[tokio::test]
async fn a_sab_music_client_with_no_output_path_is_followed_and_taken() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("SABnzbd_nzo_1", &[(3, "Teardrop", "FLAC")])
            .by_client("Sabnzbd", "SAB Music", "usenet", false)
            .without_output_path()
            .finished(),
    );
    let service = f.service(None, 30);

    assert!(f.heart_teardrop(&service).await);
    assert_eq!(f.taken(), ["Teardrop"]);
}

/// A Deluge-shaped download that moves its files on completion: Octo knows its files by their
/// rows, not by where the client put them.
#[tokio::test]
async fn a_deluge_shaped_move_on_completion_is_followed_and_taken() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("deluge-1", &[(3, "Teardrop", "FLAC")])
            .by_client("Deluge", "Deluge", "torrent", false)
            .finished(),
    );
    let service = f.service(None, 30);

    assert!(f.heart_teardrop(&service).await);
    assert_eq!(f.taken(), ["Teardrop"]);
}

/// With Completed Download Handling off, Lidarr never imports Octo's download itself: its absence
/// from the queue, proven by another item of its client, is the download finishing. Octo imports
/// its own files as that download, replacing nothing, leaves out a file Lidarr matched to another
/// album, takes what came in, and the entry ends when that import ends.
#[tokio::test]
async fn with_completed_download_handling_off_octo_imports_its_own_download() {
    let f = Fixture::new().await;
    f.lidarr.set_completed_download_handling(false);
    another_artists_download(&f);
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(3, "Teardrop", "FLAC"), (40, "Karmacoma", "FLAC")]).file_into_album(40, 9),
    );
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken(), ["Teardrop"]);
    over(&f).await;
    let imports: Vec<Value> = f
        .lidarr
        .changes()
        .into_iter()
        .filter_map(|change| change.body)
        .filter(|body| body["name"] == "ManualImport")
        .collect();
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0]["replaceExistingFiles"], Value::Bool(false));
    assert_eq!(imports[0]["importMode"], "auto");
    let files = imports[0]["files"].as_array().expect("files");
    assert_eq!(files.len(), 1, "only the file of the pick's album");
    assert_eq!(files[0]["downloadId"], "dl-1");
    assert_eq!(files[0]["albumId"], 7);
}

/// With Completed Download Handling off, a client that fails one refresh drops its items from it:
/// with no other item of that client listed, the missing item proves nothing, and nothing is
/// imported.
#[tokio::test]
async fn a_client_hiccup_with_completed_download_handling_off_imports_nothing() {
    let f = Fixture::new().await;
    f.lidarr.set_completed_download_handling(false);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let _heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.client_fails_next_refresh("qBittorrent");
    f.lidarr.refresh();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        !f.lidarr
            .changes()
            .iter()
            .filter_map(|change| change.body.clone())
            .any(|body| body["name"] == "ManualImport")
    );
    let entry = entry(&f);
    assert!(entry.following());
    assert_eq!(entry.watched().count(), 1);
}

/// The user's folder import onto the album, its rows naming no client though Lidarr guessed
/// Octo's download id: the files are the user's, untouched, Octo steps aside, and the album
/// that holds them stays.
#[tokio::test]
async fn a_folder_import_with_a_guessed_id_of_octos_is_the_users_untouched_and_its_album_kept() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.user_folder_import(
        &[GrabFile {
            album_id: 7,
            track_id: 1,
            title: "Angel".into(),
            quality: "FLAC".into(),
            rejection: None,
        }],
        Some("dl-1"),
    );

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    assert!(f.taken().is_empty());
    assert!(f.lidarr.deleted().is_empty());
    assert!(
        f.root
            .path()
            .join("Massive Attack/Mezzanine/01 - Angel.flac")
            .is_file()
    );
    assert!(removed(&f, 1));
    assert!(!album_deleted(&f));
}

/// A read between an import's track files and their rows, with the import still running: the
/// files are neither taken nor judged the user's; each is decided by its row once written.
#[tokio::test]
async fn a_track_file_before_its_row_is_neither_taken_nor_the_users() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.set_rows_one_per_step(true);
    f.lidarr.set_command_mode(CommandMode::Held);
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
    // The refresh, then the import Lidarr starts as the refresh ends: the files are in, no row
    // yet.
    f.lidarr.step();
    f.lidarr.step();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let waiting = entry(&f);
    assert!(f.taken().is_empty());
    assert!(waiting.users_files.is_empty());
    assert!(waiting.following());

    f.lidarr.run_commands();

    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
}

/// Lidarr restarts mid-follow: its queue is empty until its first refresh, which proves nothing.
/// Octo's download stays watched, and its later import is taken.
#[tokio::test]
async fn a_lidarr_restart_mid_follow_keeps_the_download_watched_and_its_import_taken() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.restart();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let watching = entry(&f);
    assert!(watching.following());
    assert_eq!(watching.watched().count(), 1);

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken(), ["Teardrop"]);
}

/// The client throws for one refresh while Lidarr's health shows nothing wrong: the download stays
/// watched, and its import is taken when it comes.
#[tokio::test]
async fn a_client_throwing_for_one_refresh_keeps_the_download_watched_and_its_import_taken() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.client_fails_next_refresh("qBittorrent");
    f.lidarr.refresh();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let watching = entry(&f);
    assert!(watching.following());
    assert_eq!(watching.watched().count(), 1);

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken(), ["Teardrop"]);
}

/// Octo's download removed in the client while another item of that client stays listed: the
/// absence is settled, the download accounted for, and the pick ends, put back.
#[tokio::test]
async fn a_removal_in_the_client_with_another_item_of_that_client_is_accounted_and_ends() {
    let f = Fixture::new().await;
    another_artists_download(&f);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.remove_in_client("dl-1");
    f.lidarr.refresh();

    assert!(!heart.await.expect("the task"));
    over(&f).await;
}

/// Octo's download was its client's only item, and is removed in the client: the empty refresh
/// proves nothing, so it stays watched until a later refresh lists any item of that client.
#[tokio::test]
async fn a_removal_of_the_clients_only_item_stays_watched_until_that_client_lists_another() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.remove_in_client("dl-1");
    f.lidarr.refresh();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let watching = entry(&f);
    assert!(watching.following());
    assert_eq!(watching.watched().count(), 1);

    another_artists_download(&f);

    assert!(!heart.await.expect("the task"));
    over(&f).await;
}

/// The same, ended instead by the one person whose heart was on it taking it back.
#[tokio::test]
async fn a_removal_of_the_clients_only_item_ends_when_every_holder_unhearts() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    f.lidarr.remove_in_client("dl-1");
    f.lidarr.refresh();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(entry(&f).following());

    let (provider, id) = teardrop(&f);
    service.unhearted(&provider, &id, "Alice");

    over(&f).await;
}

/// Lidarr's import picks the album from the files, so Octo's grab can land on another album of
/// the artist: the file is still Octo's by its row, taken, and never left in Lidarr's library;
/// that album, the user's, stays.
#[tokio::test]
async fn octos_grab_imported_into_another_album_of_the_artist_is_taken_and_never_left() {
    let f = Fixture::new().await;
    f.lidarr.catalog_album(protection());
    f.lidarr.user_album("rg-protection", false);
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(40, "Karmacoma", "FLAC")])
            .into_album(9)
            .finished(),
    );
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);
    over(&f).await;
    assert_eq!(f.taken(), ["Karmacoma"]);
    assert_eq!(
        f.lidarr.deleted(),
        [100],
        "its record went with the file Octo moved"
    );
    assert!(
        !f.root
            .path()
            .join("Massive Attack/Protection/01 - Karmacoma.flac")
            .exists()
    );
    assert!(
        !f.changes()
            .iter()
            .any(|change| change.starts_with("DELETE /api/v1/album/9")),
        "the user's album stays"
    );
}

/// A rescan covering the artist queued while Octo's finished download waits for its import:
/// Octo removes its item before the import could run, so the file the rescan links is never
/// replaced.
async fn a_command_while_octos_item_waits_removes_octos_items_first(body: Value, trigger: &str) {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(7, Grab::of("dl-1", &[(1, "Angel", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.set_command_mode(CommandMode::Held);
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
    f.lidarr.step();
    let lying = f.lidarr.place_file(
        "/data/music/Massive Attack/Mezzanine/01 - Angel.flac",
        b"the user's own",
    );
    f.lidarr.queue_command("RescanFolders", body, trigger);

    until(|| removed(&f, 1)).await;
    f.lidarr.run_commands();

    assert!(!heart.await.expect("the task"));
    assert_eq!(std::fs::read(&lying).expect("still there"), b"the user's own");
    assert!(f.taken().is_empty());
    assert!(
        !f.lidarr
            .requests()
            .iter()
            .any(|request| request.path.starts_with("/api/v1/manualimport")),
    );
}

#[tokio::test]
async fn a_rescan_queued_while_octos_item_waits_for_its_import_removes_octos_items_first() {
    a_command_while_octos_item_waits_removes_octos_items_first(
        serde_json::json!({ "folders": ["/data/music/Massive Attack"], "artistIds": [5] }),
        "manual",
    )
    .await;
}

#[tokio::test]
async fn lidarrs_daily_root_scan_while_octos_item_waits_for_its_import_removes_octos_items() {
    a_command_while_octos_item_waits_removes_octos_items_first(
        serde_json::json!({ "folders": null }),
        "scheduled",
    )
    .await;
}

/// A manual import of the user's that Lidarr runs right before Octo's own import, with nothing
/// queued for Octo to see: Lidarr's import of Octo's download replaces the user's file. Octo
/// cannot prevent that, and says so on the downloads entry, naming the file and the recycle bin.
#[tokio::test]
async fn a_file_of_the_users_that_octos_import_replaced_is_told_on_the_downloads_entry() {
    let f = Fixture::new().await;
    f.lidarr.set_recycle_bin(true);
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.set_command_mode(CommandMode::Held);
    f.lidarr.complete("dl-1");
    f.lidarr.user_folder_import(
        &[GrabFile {
            album_id: 7,
            track_id: 3,
            title: "Teardrop".into(),
            quality: "MP3-320".into(),
            rejection: None,
        }],
        None,
    );
    f.lidarr.refresh();
    f.lidarr.run_commands();

    assert!(heart.await.expect("the task"));
    let key = teardrop_key(&f);
    until(|| !notes(&f, &key).is_empty()).await;
    let told = notes(&f, &key);
    let (text, detail) = told
        .iter()
        .find(|(text, _)| text == REPLACED_YOURS)
        .expect("the replaced file is told");
    assert_eq!(text, REPLACED_YOURS);
    let detail = detail.as_deref().expect("the file named");
    assert!(detail.contains("03 - Teardrop.mp3"), "{detail}");
    assert!(detail.contains("/data/recycle"), "{detail}");
}

/// Lidarr is last, the timeout passed and the heard audio was kept while Octo's download still
/// moves: the follow stays (a kept copy alone never ends it), the copy waits in the hold path,
/// and it is placed once Lidarr is done with Octo's download.
#[tokio::test]
async fn a_kept_copy_held_while_a_follow_is_open_waits_and_is_placed_when_it_ends() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(7, Grab::of("dl-1", &[(1, "Angel", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);
    assert!(service.waits_for_lidarr("soulseek", &f.id("Teardrop")));

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(entry(&f).following(), "a kept copy alone never ends the follow");
    assert!(f.placed.lock().is_empty());
    assert!(
        Path::new(&held.held_path).is_file(),
        "it plays from the hold path"
    );

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    assert_eq!(f.taken(), ["Angel"]);
    over(&f).await;
}

/// A stalled torrent (qBittorrent's own warning) with a kept copy waiting: the follow ends, Octo's
/// download is removed, and the kept copy is placed at once.
#[tokio::test]
async fn a_stalled_qbittorrent_download_with_a_kept_copy_ends_the_follow_and_places_it() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);

    f.lidarr.stall("dl-1");
    f.lidarr.refresh();

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    assert!(
        f.changes().iter().any(|change| change
            == "DELETE /api/v1/queue/1?removeFromClient=true&blocklist=false&skipRedownload=true")
    );
    over(&f).await;
}

/// A Transmission torrent never warns when it is dead: with a kept copy waiting, a play that finds
/// it made no progress since the last play or heart ends the follow. One that moved does not.
#[tokio::test]
async fn a_transmission_download_with_no_progress_between_two_plays_ends_the_follow() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).by_client(
            "Transmission",
            "Transmission",
            "torrent",
            true,
        ),
    );
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    hold_kept_copy(&f);
    let (provider, id) = teardrop(&f);
    let marked = |left: i64| {
        f.pending.get(ALBUM_ID).is_some_and(|entry| {
            entry
                .grabs
                .iter()
                .any(|grab| grab.size_left_at_event == Some(left))
        })
    };

    f.lidarr.progress("dl-1", 5_000);
    f.lidarr.refresh();
    service.song_event(&provider, &id);
    until(|| marked(5_000)).await;
    f.lidarr.progress("dl-1", 4_000);
    f.lidarr.refresh();
    service.song_event(&provider, &id);
    until(|| marked(4_000)).await;
    assert!(entry(&f).following(), "it moved between the two plays");

    service.song_event(&provider, &id);

    until(|| f.placed.lock().len() == 1).await;
    assert!(removed(&f, 1));
    over(&f).await;
}

/// Another heart's file proven by its sound for another album of the artist: the follow ends at
/// once, Octo's download is removed, the proven file is placed, and the held entries say why.
#[tokio::test]
async fn another_hearts_proven_soulseek_file_ends_the_follow_and_lands() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);
    let key = teardrop_key(&f);
    let (provider, id) = teardrop(&f);
    f.tracker
        .begin(&provider, &id, None, Some("alice"), None, None, None);

    // What the placement does for the proven Karmacoma: holds it for the follow and ends it.
    let folder = f.root.path().join(".octo-incoming/held/lidarr-follow-karmacoma");
    std::fs::create_dir_all(&folder).expect("made");
    let proven = folder.join("01 - Karmacoma.flac");
    std::fs::write(&proven, b"proven").expect("written");
    f.pending.hold_placement(HeldPlacement {
        song: "soulseek:karmacoma".into(),
        held_path: proven.to_string_lossy().into_owned(),
        target: f
            .root
            .path()
            .join("Massive Attack/Protection/01 - Karmacoma.flac")
            .to_string_lossy()
            .into_owned(),
        kept: false,
        follows: vec![ALBUM_ID.into()],
    });
    assert!(f.pending.request_end(ALBUM_ID, FollowEnd::ProvenElsewhere));

    until(|| f.placed.lock().len() == 2).await;
    assert!(Path::new(&held.target).is_file());
    assert!(
        f.root
            .path()
            .join("Massive Attack/Protection/01 - Karmacoma.flac")
            .is_file()
    );
    assert!(removed(&f, 1));
    assert!(notes(&f, &key).iter().any(|(text, _)| text == PROVEN_ELSEWHERE));
    over(&f).await;
}

/// "Stop waiting for Lidarr": the follow ends, Octo's download is removed, and the kept copy is
/// placed at once. Nothing waits for a second tap.
#[tokio::test]
async fn stop_waiting_for_lidarr_ends_the_follow_and_places_the_kept_copy() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);
    let (provider, id) = teardrop(&f);
    assert!(service.waits_for_lidarr(&provider, &id));

    assert_eq!(
        service.stop_waiting_for_lidarr(&provider, &id).await,
        StopWaiting::Stopped
    );

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    assert!(removed(&f, 1));
    assert!(!service.waits_for_lidarr(&provider, &id));
    assert_eq!(
        service.stop_waiting_for_lidarr(&provider, &id).await,
        StopWaiting::NothingWaits
    );
    over(&f).await;
}

/// "Stop waiting for Lidarr" while Lidarr does not answer: the kept copy is still placed at once;
/// Octo's download is removed once Lidarr answers again.
#[tokio::test]
async fn stop_waiting_places_the_kept_copy_even_while_lidarr_does_not_answer() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);
    let (provider, id) = teardrop(&f);
    f.lidarr.set_down(true);

    assert_eq!(
        service.stop_waiting_for_lidarr(&provider, &id).await,
        StopWaiting::Stopped
    );

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    assert_eq!(
        entry(&f).watched().count(),
        1,
        "Octo's download waits to be removed"
    );

    f.lidarr.set_down(false);

    over(&f).await;
    assert!(removed(&f, 1));
}

/// The import timeout passed while Octo's download still moves: the heart's step ended, but the
/// entry and its watch list stay, and the later import is still Octo's, taken and gated.
#[tokio::test]
async fn a_moving_download_past_the_timeout_keeps_its_entry_and_is_still_taken() {
    let f = Fixture::new().await;
    let _service = heart_gave_up_on(&f, "dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]).await;
    followed(&f, "dl-1").await;
    assert!(f.claims.heart_busy(ALBUM_ID), "the follow still holds the album");

    f.lidarr.progress("dl-1", 1_000);
    f.lidarr.refresh();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(entry(&f).following());

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    until(|| f.taken.lock().len() == 2).await;
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
    over(&f).await;
}

/// A download that never moves ends when the one person whose heart is on it takes it back:
/// Octo's item is removed and the pick put back.
#[tokio::test]
async fn a_stuck_download_ends_when_every_holder_takes_their_heart_back() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let (provider, id) = teardrop(&f);

    service.unhearted(&provider, &id, "bob");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(entry(&f).following(), "bob's heart was never on it");

    service.unhearted(&provider, &id, "alice");

    over(&f).await;
    assert!(removed(&f, 1));
}

/// Plays during a follow wait for it: a play of the song joins the open follow and starts no
/// second search.
#[tokio::test]
async fn plays_during_the_follow_start_no_search() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;

    for _ in 0..2 {
        assert!(
            !within(service.try_acquire_track(
                "soulseek",
                "teardrop-heart",
                false,
                Some("bob"),
                AcquisitionOrigin::Play,
                &[],
            ))
            .await
        );
    }

    assert_eq!(f.lidarr.searches(), 1);
    assert!(entry(&f).following());
}

/// Octo stopped between its search and the follow's first read, and Lidarr no longer lists the
/// search: its window runs from the saved post to the first read after the restart, so the grab
/// is still Octo's and its import is taken.
#[tokio::test]
async fn a_restart_between_the_search_and_the_first_read_keeps_the_grab_octos() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);
    let search = f.client().album_search(7).await.expect("searched");
    f.pending.remember(PendingAlbum {
        foreign_album_id: ALBUM_ID.into(),
        lidarr_album_id: 7,
        lidarr_artist_id: 5,
        artist: "Massive Attack".into(),
        title: "Mezzanine".into(),
        search_command: search.id,
        searched_at: search.queued,
        holders: vec!["alice".into()],
        ..Default::default()
    });
    f.lidarr.forget_command(search.id);

    service.put_back_after_restart().await;

    until(|| f.taken.lock().len() == 1).await;
    assert_eq!(f.taken(), ["Teardrop"]);
    over(&f).await;
}

/// The user's own Lidarr grabs another album of the artist (RSS) while Octo follows: that grab
/// will bring files Octo's import could replace, so Octo removes its own item and steps aside,
/// leaving the user's.
#[tokio::test]
async fn a_users_rss_grab_of_another_album_of_the_artist_mid_follow_makes_octo_step_aside() {
    let f = Fixture::new().await;
    f.lidarr.catalog_album(protection());
    f.lidarr.user_album("rg-protection", false);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    f.lidarr.user_grab(
        9,
        Grab::of("rss-1", &[(40, "Karmacoma", "FLAC")]).into_album(9),
        "Rss",
    );

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    assert!(removed(&f, 1));
    assert!(!removed(&f, 2));
}

/// A kept-as-heard file in the artist's folder that a rescan adopts while Octo's download is still
/// downloading: it has no row, so once Lidarr's imports are idle it is the user's, before any file
/// of Octo's. Octo removes its item, and the file survives as it was.
#[tokio::test]
async fn a_library_file_a_rescan_adopts_mid_download_makes_octo_step_aside_and_survives() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    let kept = f.lidarr.place_file(
        "/data/music/Massive Attack/Mezzanine/01 - Angel.flac",
        b"kept as heard",
    );
    f.lidarr.queue_command(
        "RescanFolders",
        serde_json::json!({ "folders": ["/data/music/Massive Attack"], "artistIds": [5] }),
        "manual",
    );

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    assert!(removed(&f, 1));
    assert_eq!(std::fs::read(&kept).expect("still there"), b"kept as heard");
    assert!(!album_deleted(&f));
}

/// Each of Lidarr's failed states of Octo's download ends the pick at once; a download Lidarr will
/// never import is removed, blocklisted only when its download failed.
#[tokio::test]
async fn each_failed_state_ends_the_pick() {
    for (state, blocklist) in [
        ("importBlocked", false),
        ("importFailed", false),
        ("downloadFailedPending", true),
    ] {
        let f = Fixture::new().await;
        f.lidarr
            .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
        let service = f.service(None, 30);
        let heart = heart_later(&f, &service);
        followed(&f, "dl-1").await;

        f.lidarr.block_import("dl-1", state);
        f.lidarr.refresh();

        assert!(!heart.await.expect("the task"), "{state}");
        over(&f).await;
        assert!(
            f.changes().iter().any(|change| change
                == &format!(
                    "DELETE /api/v1/queue/1?removeFromClient=true&blocklist={blocklist}&skipRedownload=true"
                )),
            "{state}: {:?}",
            f.changes()
        );
    }
    for event in [
        LidarrHistoryEvent::AlbumImportIncomplete,
        LidarrHistoryEvent::DownloadIgnored,
    ] {
        let f = Fixture::new().await;
        f.lidarr
            .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
        let service = f.service(None, 30);
        let heart = heart_later(&f, &service);
        followed(&f, "dl-1").await;

        f.lidarr.download_row("dl-1", event);

        assert!(!heart.await.expect("the task"), "{event:?}");
        over(&f).await;
    }
}

/// Another artist's download is never Octo's business: it is never removed and never makes Octo
/// step aside.
#[tokio::test]
async fn another_artists_item_never_counts_and_is_never_touched() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;

    another_artists_download(&f);
    tokio::time::sleep(Duration::from_millis(200)).await;
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    assert!(heart.await.expect("the task"));
    over(&f).await;
    assert!(!removed(&f, 2));
}

/// Before the follow waits, the heart's heard-master rounds are paused, so a long wait never
/// keeps a heard copy's place.
#[tokio::test]
async fn the_hearts_rounds_are_paused_while_its_pick_waits_for_the_follow() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let paused: Arc<Mutex<Vec<(String, usize)>>> = Arc::default();
    let seen = paused.clone();
    let service = f.service_with(Options {
        timeout_seconds: 1,
        rounds: Some(Arc::new(move |key: &str, gates: &[HeartGate]| {
            seen.lock().push((key.to_string(), gates.len()));
        })),
        ..Default::default()
    });
    let gate = HeartGate {
        user: "alice".into(),
        origin: AcquisitionOrigin::Heart,
        r: None,
        heard: None,
        heard_seconds: None,
        reading: octo_core::fingerprint::heart_gate::HeardReading::NotHeard,
        stand_in: None,
    };

    assert!(
        !within(service.try_acquire_track(
            "soulseek",
            "teardrop-heart",
            false,
            Some("alice"),
            AcquisitionOrigin::Heart,
            &[gate],
        ))
        .await
    );

    assert_eq!(
        *paused.lock(),
        [(acquisition_key("soulseek", "teardrop-heart", false), 1)]
    );
}

// ---- Lidarr last, and MusicBrainz does not answer -------------------------------------------

/// Eye of the Tiger heard from Rocky IV, with a MusicBrainz that answers 503 to everything.
async fn busy_row(f: &Fixture) -> (String, Arc<MusicBrainzClient>, MockServer) {
    let (id, _, _) = f.recorded_row(&EYE_OF_THE_TIGER).await;
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let music_brainz = Arc::new(quick(&server, Duration::from_secs(5)));
    (id, music_brainz, server)
}

/// With Lidarr last, a heart whose walk MusicBrainz does not answer leaves the question it waits
/// on, so the heart holds for it; nothing is searched and its row is not failed. With a source
/// after Lidarr, and for a play, the step just ends.
#[tokio::test]
async fn a_busy_walk_with_lidarr_last_leaves_what_the_heart_waits_on() {
    let f = Fixture::new().await;
    survivors_albums(&f, None);
    let (id, music_brainz, _server) = busy_row(&f).await;
    let service = f.service_with(Options {
        timeout_seconds: 30,
        music_brainz: Some(music_brainz),
        catalog: Some(RecordedCatalog::open() as Arc<dyn CatalogQuestions>),
        ..Default::default()
    });
    f.tracker
        .begin("soulseek", &id, None, Some("alice"), None, None, None);

    assert!(
        !within(service.try_acquire_track(
            "soulseek",
            &id,
            true,
            Some("alice"),
            AcquisitionOrigin::Heart,
            &[]
        ))
        .await
    );
    assert!(matches!(
        service.waits_on("soulseek", &id),
        Some(HeartBusy::MusicBrainz(_))
    ));
    assert_eq!(service.waits_on("soulseek", &id), None, "read once");
    assert_ne!(
        f.tracker
            .detail(&format!("soulseek:{id}"), None)
            .map(|row| row.state),
        Some(AcquisitionState::Failed)
    );

    // A source after Lidarr: the chain goes on.
    assert!(
        !within(service.try_acquire_track(
            "soulseek",
            &id,
            false,
            Some("alice"),
            AcquisitionOrigin::Heart,
            &[]
        ))
        .await
    );
    assert_eq!(service.waits_on("soulseek", &id), None);
    // A play never waits.
    assert!(
        !within(service.try_acquire_track(
            "soulseek",
            &id,
            true,
            Some("alice"),
            AcquisitionOrigin::Play,
            &[]
        ))
        .await
    );
    assert_eq!(service.waits_on("soulseek", &id), None);
    assert_eq!(f.lidarr.searches(), 0);
}
