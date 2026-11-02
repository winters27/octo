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
        users: vec!["alice".into()],
        own_copies: Vec::new(),
    };
    f.pending.hold_placement(held.clone());
    let (provider, id) = teardrop(f);
    f.tracker.wait_aside(
        &provider,
        &id,
        &held.users,
        crate::services::lidarr::HELD_FOR_LIDARR,
    );
    held
}

/// The line the song's row shows now.
fn row_line(f: &Fixture) -> Option<String> {
    f.tracker.detail(&teardrop_key(f), None).and_then(|row| row.note)
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

/// The same, when the one person whose heart was on it takes it back: un-hearting ends the follow,
/// but a download Octo cannot see is never settled by it. It stays watched, the Lidarr card lists
/// the follow, and only the admin's "Put Lidarr back" settles it and lets the pick end.
#[tokio::test]
async fn a_removal_of_the_clients_only_item_stays_watched_after_every_holder_unhearts() {
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

    until(|| entry(&f).ended == Some(FollowEnd::Unhearted) && !service.unsettled_follows().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let still = entry(&f);
    assert_eq!(still.watched().count(), 1, "never settled by the un-heart");
    assert_eq!(still.unseen, ["dl-1"]);

    assert!(service.put_lidarr_back(ALBUM_ID));
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

/// The late import of the hearted song fails Octo's checks: it is deleted through Lidarr, the pick
/// is put back, and the kept copy is placed in the library.
#[tokio::test]
async fn a_late_import_that_fails_is_put_back_and_the_kept_copy_placed() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);
    f.refused.lock().insert("Teardrop".into());

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    over(&f).await;
    assert!(f.taken().is_empty());
    assert_eq!(
        f.lidarr.deleted(),
        [100],
        "the refused import is deleted through Lidarr"
    );
    assert!(album_deleted(&f), "the album Octo's add created is put back");
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
    let key = teardrop_key(&f);
    let (provider, id) = teardrop(&f);
    f.tracker
        .begin(&provider, &id, None, Some("alice"), None, None, None);
    let held = hold_kept_copy(&f);
    assert_eq!(
        row_line(&f).as_deref(),
        Some(crate::services::lidarr::HELD_FOR_LIDARR)
    );

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
        users: Vec::new(),
        own_copies: Vec::new(),
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
    assert_eq!(
        row_line(&f).as_deref(),
        Some(PROVEN_ELSEWHERE),
        "the row's last line says why Octo stopped waiting"
    );
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
    assert_eq!(service.stop_waiters(&provider, &id), ["alice"]);

    assert_eq!(
        service.stop_waiting_for_lidarr(&provider, &id, "alice").await,
        StopWaiting::Stopped
    );

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    // Nobody else waits on the follow, so it ends and removes Octo's download.
    until(|| removed(&f, 1)).await;
    assert!(!service.waits_for_lidarr(&provider, &id));
    assert!(service.stop_waiters(&provider, &id).is_empty());
    assert_ne!(
        row_line(&f).as_deref(),
        Some(crate::services::lidarr::HELD_FOR_LIDARR),
        "the row no longer says it waits"
    );
    assert_eq!(
        service.stop_waiting_for_lidarr(&provider, &id, "alice").await,
        StopWaiting::NothingWaits
    );
    over(&f).await;
}

/// Two hearts on one follow: alice's kept Teardrop waits, and bob waits on Octo's download of
/// Angel. Alice stops waiting, so her copy is placed while the follow stays open for bob, and a
/// rescan links it into the album. It is Octo's own copy, never a file of the user's: the follow
/// does not step aside, the artist still has no music of the user's, and bob's Angel still lands.
/// Lidarr's import of Angel replaces every file the album had, alice's copy among them: Octo puts
/// it back from the link it kept, and tells nobody a file of theirs was replaced.
#[tokio::test]
async fn a_rescan_linking_a_copy_octo_placed_never_makes_the_follow_step_aside() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(7, Grab::of("dl-1", &[(1, "Angel", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let angel = format!("soulseek:{}", f.id("Angel"));
    f.pending.update(ALBUM_ID, |entry| {
        entry.songs.push(angel.clone());
        entry.holders.push("bob".into());
    });
    let held = hold_kept_copy(&f);
    let (provider, id) = teardrop(&f);

    assert_eq!(
        service.stop_waiting_for_lidarr(&provider, &id, "alice").await,
        StopWaiting::Stopped
    );
    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    assert!(entry(&f).following(), "bob still waits on the follow");

    f.lidarr.queue_command(
        "RescanFolders",
        serde_json::json!({ "folders": ["/data/music/Massive Attack"], "artistIds": [5] }),
        "manual",
    );
    let linked = f.client().artist_track_files(5).await.expect("Lidarr answers");
    assert!(
        linked.iter().any(|file| file
            .path
            .as_deref()
            .is_some_and(|path| path.ends_with("03 - Teardrop.m4a"))),
        "the rescan linked alice's copy: {linked:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let watching = entry(&f);
    assert!(watching.following(), "Octo's own copy never makes it step aside");
    assert!(watching.users_files.is_empty());
    let music = service
        .artist_music()
        .read(ARTIST_ID)
        .await
        .expect("Lidarr answers");
    assert!(
        !music.has_music,
        "a copy Octo placed is not the user's music for the artist"
    );

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    until(|| f.taken() == ["Angel"]).await;
    over(&f).await;
    assert!(
        Path::new(&held.target).is_file(),
        "alice's copy is back where it was placed"
    );
    assert!(
        !notes(&f, &teardrop_key(&f))
            .iter()
            .any(|(text, _)| text == REPLACED_YOURS),
        "nobody is told a file of theirs was replaced"
    );
    let links = f.root.path().join(".octo-incoming/held/lidarr-placed");
    assert!(
        !std::fs::read_dir(&links).is_ok_and(|mut entries| entries.next().is_some()),
        "the link goes once the follow is over"
    );
}

/// The only music of an artist in Lidarr is a copy Octo placed while a follow of the artist is
/// open, linked by a rescan and lying in the artist's folder: the artist has no music of the
/// user's, so a pick may still use Lidarr for it. Once that follow is over, the copy is music of
/// the library like any other, and Lidarr is left alone.
#[tokio::test]
async fn a_copy_octo_placed_during_an_open_follow_is_not_the_artists_music() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    let service = f.service(None, 1);
    f.pending.remember(PendingAlbum {
        foreign_album_id: ALBUM_ID.into(),
        artist: "Massive Attack".into(),
        title: "Mezzanine".into(),
        search_command: 3,
        ..Default::default()
    });
    let copy = f.lidarr.place_file(
        "/data/music/Massive Attack/Mezzanine/03 - Teardrop.m4a",
        b"the version alice heard",
    );
    f.lidarr
        .queue_command("RescanFolders", serde_json::json!({}), "manual");
    let kept = HeldPlacement {
        song: teardrop_key(&f),
        kept: true,
        follows: vec![ALBUM_ID.to_string()],
        users: vec!["alice".into()],
        ..Default::default()
    };
    f.pending
        .placed_copy(&kept, &copy.to_string_lossy(), None, Vec::new());
    let artist = service
        .artist_music()
        .read(ARTIST_ID)
        .await
        .expect("Lidarr answers");
    let artist_id = artist.artist.as_ref().expect("Lidarr has the artist").id;
    assert_eq!(
        f.client()
            .artist_track_files(artist_id)
            .await
            .expect("Lidarr answers")
            .len(),
        1,
        "the rescan linked the copy"
    );
    assert!(!artist.has_music, "Octo's own copy is not the user's music");

    f.pending.forget(ALBUM_ID);
    assert!(
        service
            .artist_music()
            .read(ARTIST_ID)
            .await
            .expect("Lidarr answers")
            .has_music,
        "once the follow is over it is music of the library"
    );
}

/// Octo's download was its client's only item and is gone from the client: Lidarr gives no proof
/// it is gone, so the follow keeps reading, nothing is put back or deleted, and the Lidarr card
/// lists it as a follow Octo cannot settle. The admin's "Put Lidarr back" counts the download
/// gone: the follow ends and the pick is put back.
#[tokio::test]
async fn a_download_octo_cannot_see_is_put_back_only_by_the_admin() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    assert!(service.unsettled_follows().is_empty(), "its client lists it");
    assert!(
        !service.put_lidarr_back(ALBUM_ID),
        "nothing to put back while Octo sees it"
    );

    f.lidarr.remove_in_client("dl-1");
    f.lidarr.refresh();
    until(|| !service.unsettled_follows().is_empty()).await;
    let listed = service.unsettled_follows();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].follow, ALBUM_ID);
    assert_eq!(listed[0].artist, "Massive Attack");
    assert_eq!(listed[0].album, "Mezzanine");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let watching = entry(&f);
    assert!(watching.following(), "the follow keeps reading");
    assert_eq!(watching.watched().count(), 1);
    assert!(!album_deleted(&f), "nothing is put back without the admin");
    assert!(
        f.lidarr.deleted().is_empty(),
        "nothing is deleted without the admin"
    );

    assert!(service.put_lidarr_back(ALBUM_ID));
    assert!(
        service.unsettled_follows().is_empty(),
        "the card lists it no more"
    );

    over(&f).await;
    assert!(album_deleted(&f), "the album Octo's add created is put back");
    assert!(!service.put_lidarr_back(ALBUM_ID), "nothing is left to put back");
}

/// A restart forgets every downloads row, but a copy held for an open follow still waits: its row
/// comes back saying so, for the people it was kept for, and offers them Stop waiting.
#[tokio::test]
async fn a_copy_held_for_an_open_follow_has_its_row_back_after_a_restart() {
    let f = Fixture::new().await;
    f.pending.remember(PendingAlbum {
        foreign_album_id: ALBUM_ID.into(),
        lidarr_album_id: 7,
        artist: "Massive Attack".into(),
        title: "Mezzanine".into(),
        search_command: 3,
        searched_at: Some(chrono::Utc::now()),
        songs: vec![teardrop_key(&f)],
        holders: vec!["alice".into()],
        ..Default::default()
    });
    let folder = f.root.path().join(".octo-incoming/held/lidarr-follow-teardrop");
    std::fs::create_dir_all(&folder).expect("made");
    let held_path = folder.join("03 - Teardrop.m4a");
    std::fs::write(&held_path, b"the version alice heard").expect("written");
    f.pending.hold_placement(HeldPlacement {
        song: teardrop_key(&f),
        held_path: held_path.to_string_lossy().into_owned(),
        target: "/music/Massive Attack/Mezzanine/03 - Teardrop.m4a".into(),
        kept: true,
        follows: vec![ALBUM_ID.to_string()],
        users: vec!["alice".into()],
        own_copies: Vec::new(),
    });
    let service = f.service(None, 1);
    assert!(f.tracker.for_user("alice").is_empty(), "nothing listed yet");

    service.put_back_after_restart().await;

    let rows = f.tracker.for_user("alice");
    let row = rows
        .iter()
        .find(|row| row.key == teardrop_key(&f))
        .expect("the row is back");
    assert_eq!(
        row.note.as_deref(),
        Some(crate::services::lidarr::HELD_FOR_LIDARR)
    );
    let (provider, id) = teardrop(&f);
    assert_eq!(service.stop_waiters(&provider, &id), ["alice"]);
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
        service.stop_waiting_for_lidarr(&provider, &id, "alice").await,
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

/// A release picked in Find songs is grabbed by Lidarr within the call, with no command and so no
/// window: the grab is Octo's by the picked release's guid, and its import is taken.
#[tokio::test]
async fn a_release_picked_in_find_songs_is_octos_by_its_guid() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr
        .offer("guid-picked", 1, "Massive Attack - Mezzanine", 300_000_000);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);
    let before = Utc::now() - TimeDelta::seconds(1);
    let guid = f
        .client()
        .grab_picked(
            7,
            &octo_core::lidarr::LidarrReleasePick {
                guid: "guid-picked".into(),
                indexer_id: 1,
                title: Some("Massive Attack - Mezzanine".into()),
                size: Some(300_000_000),
            },
        )
        .await
        .expect("grabbed");
    f.pending.remember(PendingAlbum {
        foreign_album_id: ALBUM_ID.into(),
        lidarr_album_id: 7,
        lidarr_artist_id: 5,
        artist: "Massive Attack".into(),
        title: "Mezzanine".into(),
        searched_at: Some(before),
        picked_guid: Some(guid),
        holders: vec!["alice".into()],
        ..Default::default()
    });

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
        Some(LidarrWaits::Answer(HeartBusy::MusicBrainz(_)))
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

// ---- Ownership bounded by each grab's own span and Lidarr's own clock ---------------------------

/// Mezzanine as a saved entry of Octo's on the album the user's Lidarr holds (Lidarr's ids 7 and
/// 5), searched.
fn mezzanine_entry() -> PendingAlbum {
    PendingAlbum {
        foreign_album_id: ALBUM_ID.into(),
        lidarr_album_id: 7,
        lidarr_artist_id: 5,
        foreign_artist_id: ARTIST_ID.into(),
        artist: "Massive Attack".into(),
        title: "Mezzanine".into(),
        search_command: 3,
        ..Default::default()
    }
}

async fn put_back(f: &Fixture, entry: &PendingAlbum) {
    let service = f.service(None, 30);
    crate::services::lidarr::lidarr_pick::put_back_searched(
        &f.client(),
        Some(f.pending.as_ref()),
        &service.artist_music(),
        entry,
        Duration::from_millis(20),
    )
    .await
    .expect("put back");
}

/// A torrent keeps its hash, so a download id is not unique over time: the user grabbing again a
/// release Octo was done with brings a download of the same id. Its files are the user's, and
/// Octo's put back never deletes them.
#[tokio::test]
async fn a_put_back_never_deletes_the_files_of_a_later_grab_of_octos_download_id() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    let client = f.client();
    let before = client.now().await.expect("Lidarr's time") - TimeDelta::hours(2);
    let mut entry = mezzanine_entry();
    entry.searched_at = Some(before);
    entry.record_grab_at("dl-1", Some("qBittorrent"), Some(before));
    entry
        .grab_mut("dl-1")
        .expect("recorded")
        .account(before + TimeDelta::minutes(10));
    f.pending.remember(entry.clone());
    f.lidarr.user_grab(
        7,
        Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished(),
        "InteractiveSearch",
    );
    assert_eq!(client.artist_track_files(5).await.expect("read").len(), 1);

    put_back(&f, &entry).await;

    assert!(f.lidarr.deleted().is_empty(), "the user's file stays");
    assert_eq!(client.artist_track_files(5).await.expect("read").len(), 1);
    assert!(f.pending.get(ALBUM_ID).is_none());
}

/// A pick of Octo's that is over left its download id known as Octo's for the span of that grab.
/// The user later grabs the same release again, keeping its id, while another pick of Octo's on the
/// artist is open: that grab is the user's, so Octo steps aside before its own import could replace
/// the user's files, and never takes the user's import.
#[tokio::test]
async fn a_users_grab_of_a_download_id_octo_was_done_with_makes_octo_step_aside() {
    let f = Fixture::new().await;
    let earlier = f.client().now().await.expect("Lidarr's time") - TimeDelta::hours(2);
    let mut over_now = PendingAlbum {
        foreign_album_id: "rg-earlier".into(),
        ..Default::default()
    };
    over_now.record_grab_at("dl-1", Some("qBittorrent"), Some(earlier));
    over_now
        .grab_mut("dl-1")
        .expect("recorded")
        .account(earlier + TimeDelta::minutes(10));
    f.pending.remember(over_now);
    f.pending.forget("rg-earlier");
    f.lidarr
        .plan_grab(7, Grab::of("dl-2", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-2").await;

    f.lidarr.user_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC")]).finished(),
        "InteractiveSearch",
    );

    until(|| f.pending.get(ALBUM_ID).is_none_or(|entry| entry.ended.is_some())).await;
    over(&f).await;
    assert!(removed(&f, 1), "Octo's own item is removed");
    assert!(
        !f.taken().contains(&"Angel".to_string()),
        "the user's import is never Octo's"
    );
    assert!(f.lidarr.deleted().is_empty(), "the user's file stays");
}

/// One open pick's put back never deletes what another open pick of Octo's imported on the same
/// artist.
#[tokio::test]
async fn a_put_back_never_deletes_another_open_picks_imports_of_the_artist() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr.catalog_album(CatalogAlbum::new(
        "rg-protection",
        "Protection",
        ARTIST_ID,
        8,
        &[(80, "Protection", 1)],
    ));
    f.lidarr.user_album("rg-protection", false);
    let mut other = PendingAlbum {
        foreign_album_id: "rg-protection".into(),
        lidarr_album_id: 8,
        lidarr_artist_id: 5,
        foreign_artist_id: ARTIST_ID.into(),
        search_command: 9,
        ..Default::default()
    };
    other.record_grab("dl-2", Some("qBittorrent"));
    f.pending.remember(other);
    f.lidarr.user_grab(
        8,
        Grab::of("dl-2", &[(80, "Protection", "FLAC")])
            .into_album(8)
            .finished(),
        "Search",
    );
    let client = f.client();
    assert_eq!(client.artist_track_files(5).await.expect("read").len(), 1);
    let mezzanine = mezzanine_entry();
    f.pending.remember(mezzanine.clone());

    put_back(&f, &mezzanine).await;

    assert!(
        f.lidarr.deleted().is_empty(),
        "Protection's file is the other pick's"
    );
    assert_eq!(client.artist_track_files(5).await.expect("read").len(), 1);
    assert!(f.pending.get("rg-protection").is_some());
}

/// The user grabs the album while Lidarr holds a release of Octo's search back: Lidarr clears what
/// it held back, and the user's grab, even of the same release, is never Octo's.
#[tokio::test]
async fn a_users_grab_while_lidarr_holds_a_release_back_is_never_octos() {
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

    f.lidarr.take_pending(
        queue_id,
        Grab::of("user-1", &[(3, "Teardrop", "FLAC")]).finished(),
        "InteractiveSearch",
    );

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    assert!(f.taken().is_empty(), "the user's grab is never taken");
}

/// Lidarr's clock runs an hour behind Octo's: the release it held back and grabbed later is still
/// known as Octo's by Lidarr's own times.
#[tokio::test]
async fn a_held_back_release_becomes_octos_grab_by_lidarrs_own_clock() {
    let f = Fixture::new().await;
    f.lidarr.set_clock_skew(-TimeDelta::hours(1));
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

/// A release picked in Find songs is Octo's by its guid alone, whatever Lidarr's clock says next
/// to Octo's.
#[tokio::test]
async fn a_picked_release_is_octos_by_its_guid_whatever_lidarrs_clock_says() {
    let f = Fixture::new().await;
    f.lidarr.set_clock_skew(-TimeDelta::hours(1));
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr
        .offer("guid-picked", 1, "Massive Attack - Mezzanine", 300_000_000);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);
    let client = f.client();
    let pending_at = client.now().await.expect("Lidarr's time");
    let guid = client
        .grab_picked(
            7,
            &octo_core::lidarr::LidarrReleasePick {
                guid: "guid-picked".into(),
                indexer_id: 1,
                title: Some("Massive Attack - Mezzanine".into()),
                size: Some(300_000_000),
            },
        )
        .await
        .expect("grabbed");
    f.pending.remember(PendingAlbum {
        search_command: 0,
        searched_at: Some(Utc::now()),
        search_pending: Some(pending_at),
        picked_guid: Some(guid),
        holders: vec!["alice".into()],
        ..mezzanine_entry()
    });

    service.put_back_after_restart().await;

    until(|| f.taken.lock().len() == 1).await;
    assert_eq!(f.taken(), ["Teardrop"]);
    over(&f).await;
}

/// A restart after Octo's search went out, before Lidarr's answer to it was saved: the pick is
/// followed, never put back as unsearched, and its grab is Octo's.
#[tokio::test]
async fn a_restart_after_the_search_went_out_before_its_answer_was_saved_keeps_the_grab_octos() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);
    let client = f.client();
    f.pending.remember(PendingAlbum {
        search_command: 0,
        search_pending: Some(client.now().await.expect("Lidarr's time")),
        holders: vec!["alice".into()],
        ..mezzanine_entry()
    });
    client.album_search(7).await.expect("searched");

    service.put_back_after_restart().await;

    until(|| f.taken.lock().len() == 1).await;
    assert_eq!(f.taken(), ["Teardrop"]);
    over(&f).await;
}

/// The same for a release picked in Find songs: its guid was saved before the grab went out.
#[tokio::test]
async fn a_restart_after_a_picked_grab_went_out_before_its_answer_was_saved_keeps_it_octos() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr
        .offer("guid-picked", 1, "Massive Attack - Mezzanine", 300_000_000);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);
    let client = f.client();
    let pick = octo_core::lidarr::LidarrReleasePick {
        guid: "guid-picked".into(),
        indexer_id: 1,
        title: Some("Massive Attack - Mezzanine".into()),
        size: Some(300_000_000),
    };
    f.pending.remember(PendingAlbum {
        search_command: 0,
        search_pending: Some(client.now().await.expect("Lidarr's time")),
        picked_guid: Some(pick.guid.clone()),
        holders: vec!["alice".into()],
        ..mezzanine_entry()
    });
    client.grab_release(&pick).await.expect("grabbed");

    service.put_back_after_restart().await;

    until(|| f.taken.lock().len() == 1).await;
    assert_eq!(f.taken(), ["Teardrop"]);
    over(&f).await;
}

/// Taking the last heart back while Octo's download is still listed keeps that download watched:
/// its removal, which Lidarr failed, is asked again until it goes through, and only then is the
/// pick over. Nothing of Octo's is left in Lidarr's queue.
#[tokio::test]
async fn unhearting_keeps_a_listed_download_watched_until_its_removal_goes_through() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    f.lidarr.fail_next("DELETE", "queue", 2);
    let (provider, id) = teardrop(&f);

    service.unhearted(&provider, &id, "alice");

    over(&f).await;
    let queue = f.client().queue().await.expect("read");
    assert!(
        queue
            .iter()
            .all(|item| item.download_id.as_deref() != Some("dl-1")),
        "Octo's download is gone from Lidarr's queue"
    );
}

/// Lidarr's import of Octo's download switched the album to the release it matched: the choice is
/// still Octo's pin with only its release moved, so the put back gives the saved choice back.
#[tokio::test]
async fn the_release_choice_comes_back_after_lidarrs_import_switched_the_release() {
    use octo_core::lidarr::lidarr_client::{LidarrReleaseChoice, LidarrReleaseMonitored};

    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    let choice = |any: bool, releases: &[(i32, bool)]| LidarrReleaseChoice {
        any_release_ok: any,
        releases: releases
            .iter()
            .map(|(id, monitored)| LidarrReleaseMonitored {
                id: *id,
                monitored: *monitored,
            })
            .collect(),
    };
    let saved = choice(true, &[(70, true), (71, true)]);
    f.lidarr.set_releases(7, &[(70, false), (71, true)], false);
    let entry = PendingAlbum {
        release_choice: Some(saved.clone()),
        pin: Some(choice(false, &[(70, true), (71, false)])),
        imported_releases: vec![71],
        ..mezzanine_entry()
    };
    f.pending.remember(entry.clone());

    put_back(&f, &entry).await;

    let album = f.client().album(7).await.expect("read").expect("the album");
    assert_eq!(album.release_choice(), saved);
}

/// Whether a release choice is still Octo's pin: exactly the pin, or the pin with only its release
/// moved to one Octo's import switched it to; any other change is the user's.
#[test]
fn a_choice_is_octos_pin_only_with_nothing_but_its_release_moved_by_octos_import() {
    use octo_core::lidarr::lidarr_client::{LidarrReleaseChoice, LidarrReleaseMonitored};

    use crate::services::lidarr::lidarr_pick::still_octos_pin;

    let choice = |any: bool, releases: &[(i32, bool)]| LidarrReleaseChoice {
        any_release_ok: any,
        releases: releases
            .iter()
            .map(|(id, monitored)| LidarrReleaseMonitored {
                id: *id,
                monitored: *monitored,
            })
            .collect(),
    };
    let pin = choice(false, &[(70, true), (71, false), (72, false)]);
    assert!(still_octos_pin(&pin, &pin, &[]));
    let switched = choice(false, &[(70, false), (71, true), (72, false)]);
    assert!(still_octos_pin(&switched, &pin, &[71]));
    assert!(
        !still_octos_pin(&switched, &pin, &[72]),
        "not a release Octo's import brought"
    );
    assert!(!still_octos_pin(
        &choice(true, &[(70, false), (71, true), (72, false)]),
        &pin,
        &[71]
    ));
    assert!(!still_octos_pin(
        &choice(false, &[(70, true), (71, true), (72, false)]),
        &pin,
        &[71]
    ));
}

/// A pick whose import landed gives back what Octo's add created as well as the release choice:
/// the album and the artist Octo added go from Lidarr, the files Octo moved out stay Octo's.
#[tokio::test]
async fn a_landed_pick_also_removes_the_album_and_artist_octos_add_created() {
    let f = Fixture::new().await;
    f.grabs_at_once("dl-1", &[(3, "Teardrop", "FLAC")]);
    let service = f.service(None, 30);

    assert!(f.heart_teardrop(&service).await);

    over(&f).await;
    assert_eq!(f.taken(), ["Teardrop"]);
    assert!(album_deleted(&f));
    assert!(
        f.changes()
            .iter()
            .any(|change| change.starts_with("DELETE /api/v1/artist/5?"))
    );
}

/// A put back Lidarr fails is tried again until it is done, not left for the next start.
#[tokio::test]
async fn a_put_back_lidarr_failed_is_tried_again_until_it_is_done() {
    let f = Fixture::new().await;
    let service = f.service(None, 30);
    f.lidarr.fail_next("DELETE", "album", 1);

    assert!(!f.heart_teardrop(&service).await, "the search grabbed nothing");

    over(&f).await;
    let deletes = f
        .changes()
        .iter()
        .filter(|change| change.starts_with("DELETE /api/v1/album/"))
        .count();
    assert_eq!(deletes, 2, "failed once, then done");
}

/// The album and artist a put back would remove are read first: under the ids the entry saved,
/// Lidarr now has an album and an artist that are not the entry's (another Lidarr), and nothing of
/// them is changed. The entry is let go.
#[tokio::test]
async fn a_put_back_changes_nothing_that_is_not_the_entrys_own() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    let album = f.client().album(7).await.expect("read").expect("the album");
    let entry = PendingAlbum {
        foreign_album_id: "rg-of-another-lidarr".into(),
        foreign_artist_id: "artist-of-another-lidarr".into(),
        album_added: true,
        artist_added: 5,
        release_choice: Some(album.release_choice()),
        pin: Some(album.release_choice()),
        ..mezzanine_entry()
    };
    f.pending.remember(entry.clone());

    put_back(&f, &entry).await;

    assert!(!album_deleted(&f));
    assert!(!f.changes().iter().any(
        |change| change.starts_with("DELETE /api/v1/artist/") || change.starts_with("PUT /api/v1/album/")
    ));
    assert!(f.pending.get("rg-of-another-lidarr").is_none());
}

/// Lidarr taken out of Octo's settings mid-follow: the kept copy waiting for that follow is placed,
/// since nobody's copy waits on a Lidarr Octo no longer has.
#[tokio::test]
async fn clearing_lidarr_in_the_settings_lets_the_held_copies_go() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);
    let settings = f.settings();
    let mut cleared = (*settings.current()).clone();
    cleared.lidarr.base_url = None;

    settings.set(cleared);

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    service.stop_following();
}

/// Another Lidarr set in Octo's settings mid-follow: the follow ends without touching either one
/// (the old Lidarr is no longer Octo's, and the ids name nothing of the new one), the kept copy is
/// placed, and the entry is let go.
#[tokio::test]
async fn another_lidarr_set_mid_follow_ends_the_follow_and_touches_nothing() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);
    let changes_before = f.changes().len();
    let another = MockServer::start().await;
    Mock::given(any())
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&another)
        .await;
    let settings = f.settings();
    let mut swapped = (*settings.current()).clone();
    swapped.lidarr.base_url = Some(another.uri());

    settings.set(swapped);

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    until(|| f.pending.get(ALBUM_ID).is_none()).await;
    assert_eq!(
        f.changes().len(),
        changes_before,
        "nothing more was asked of the old Lidarr"
    );
    let asked: Vec<String> = another
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| request.method.to_string())
        .collect();
    assert!(
        asked.iter().all(|method| method == "GET"),
        "nothing of the new one is changed: {asked:?}"
    );
}

/// Lidarr's address edited mid-follow to another way of reaching the same Lidarr: its album and
/// artist under the pick's ids are the pick's, so the new address is saved and the follow goes
/// on there, its import landing.
#[tokio::test]
async fn the_same_lidarr_at_a_new_address_keeps_the_follow() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let settings = f.settings();
    let mut edited = (*settings.current()).clone();
    let address = f.server.uri().replace("127.0.0.1", "localhost");
    edited.lidarr.base_url = Some(address.clone());

    settings.set(edited);
    until(|| entry(&f).lidarr_url.as_deref() == Some(address.to_lowercase().as_str())).await;
    assert!(entry(&f).following());

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
    until(|| f.taken.lock().len() == 1).await;
    assert_eq!(f.taken(), ["Teardrop"]);
    over(&f).await;
}

/// Lidarr's address edited to one Octo cannot read: whether it is the same Lidarr waits until it
/// answers, so the follow is neither ended nor let go; the kept copy that waited on it is placed
/// meanwhile.
#[tokio::test]
async fn a_new_address_that_cannot_be_read_keeps_the_entry_and_places_the_copy() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);
    let settings = f.settings();
    let mut unreadable = (*settings.current()).clone();
    unreadable.lidarr.base_url = Some("http://127.0.0.1:1".into());

    settings.set(unreadable);

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let still = entry(&f);
    assert!(still.following(), "nothing is decided until it answers");
    assert_ne!(still.lidarr_url.as_deref(), Some("http://127.0.0.1:1"));
    service.stop_following();
}

/// A record Lidarr kept is read again before it is deleted: one that names another file now is
/// left alone and let go; one that still names the moved file, with nothing there, is deleted.
#[tokio::test]
async fn a_kept_record_is_deleted_only_while_it_names_the_file_octo_moved() {
    use crate::services::lidarr::MovedRecord;
    use crate::services::lidarr::lidarr_pick::retry_moved_records;

    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr.user_grab(
        7,
        Grab::of("user-1", &[(3, "Teardrop", "FLAC")]).finished(),
        "InteractiveSearch",
    );
    let client = f.client();
    let file = client.artist_track_files(5).await.expect("read")[0].clone();
    let service = f.service(None, 30);
    let music = service.artist_music();
    let local = music
        .local(file.path.as_deref().expect("a path"))
        .expect("Octo reaches it");
    f.pending.remember(PendingAlbum {
        records_to_delete: vec![MovedRecord {
            id: file.id,
            path: Some(
                f.root
                    .path()
                    .join("Massive Attack/Mezzanine/elsewhere.flac")
                    .to_string_lossy()
                    .into_owned(),
            ),
        }],
        ..mezzanine_entry()
    });

    retry_moved_records(&client, &f.pending, &music, ALBUM_ID).await;

    assert!(f.lidarr.deleted().is_empty(), "the record names another file now");
    assert!(entry(&f).records_to_delete.is_empty());

    std::fs::remove_file(&local).expect("moved away");
    f.pending.update(ALBUM_ID, |entry| {
        entry.records_to_delete = vec![MovedRecord {
            id: file.id,
            path: Some(local.to_string_lossy().into_owned()),
        }];
    });

    retry_moved_records(&client, &f.pending, &music, ALBUM_ID).await;

    assert_eq!(f.lidarr.deleted(), [file.id]);
    assert!(entry(&f).records_to_delete.is_empty());
}

/// A kept copy that could not be placed when its follow ended stays on the list, waiting for no
/// follow, and is placed at the next start.
#[tokio::test]
async fn a_held_copy_that_could_not_be_placed_is_placed_at_the_next_start() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 1);
    assert!(!f.heart_teardrop(&service).await);
    followed(&f, "dl-1").await;
    let held = hold_kept_copy(&f);
    // A file lies where the copy's folder goes, so placing it fails.
    let folder = Path::new(&held.target).parent().expect("a folder").to_path_buf();
    std::fs::create_dir_all(folder.parent().expect("its parent")).expect("made");
    std::fs::write(&folder, b"in the way").expect("written");
    let (provider, id) = teardrop(&f);

    assert_eq!(
        service.stop_waiting_for_lidarr(&provider, &id, "alice").await,
        StopWaiting::Stopped
    );
    until(|| {
        f.pending
            .held()
            .iter()
            .any(|known| known.held_path == held.held_path && known.follows.is_empty())
    })
    .await;
    assert!(f.placed.lock().is_empty());
    over(&f).await;
    std::fs::remove_file(&folder).expect("cleared");

    service.put_back_after_restart().await;

    until(|| f.placed.lock().len() == 1).await;
    assert!(Path::new(&held.target).is_file());
    assert!(f.pending.held().is_empty());
}

/// A fresh pick of an album waits for the restart's put back of the pick a restart cut short on
/// it: the album Octo's earlier add created is deleted first, then added again for the fresh pick,
/// never searched while it is being put back.
#[tokio::test]
async fn a_fresh_pick_waits_for_the_restarts_put_back_of_its_album() {
    let f = Fixture::new().await;
    f.lidarr.add_album(false);
    f.pending.remember(PendingAlbum {
        search_command: 0,
        album_added: true,
        ..mezzanine_entry()
    });
    f.grabs_at_once("dl-1", &[(3, "Teardrop", "FLAC")]);
    f.lidarr.slow_next("GET", "album/7", Duration::from_millis(600));
    let service = f.service(None, 30);

    service.resume_after_restart();
    assert!(f.heart_teardrop(&service).await);

    let changes = f.changes();
    let deleted = changes
        .iter()
        .position(|change| change.starts_with("DELETE /api/v1/album/7"))
        .expect("the cut-short pick was put back");
    let added = changes
        .iter()
        .position(|change| change == "POST /api/v1/album")
        .expect("the fresh pick added the album again");
    assert!(deleted < added, "{changes:?}");
}

/// Lidarr's settings asked of a release choice: one the user changed since Octo read the album is
/// never written over.
#[tokio::test]
async fn a_release_choice_changed_since_octo_read_it_is_never_written_over() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    let client = f.client();
    let album = client.album(7).await.expect("read").expect("the album");
    let before = album.release_choice();
    let pin = album.pinned_to(70).expect("a release");
    f.lidarr.set_releases(7, &[(70, false), (71, true)], true);

    let written = client
        .set_release_choice_if(7, |now| now.release_choice() == before, &pin)
        .await
        .expect("asked");

    assert!(written.is_none());
    assert!(
        !f.changes()
            .iter()
            .any(|change| change.starts_with("PUT /api/v1/album/7"))
    );
}

/// The entry Octo saves before anything is sent to Lidarr is on disk before the first request
/// goes out: when it cannot be written, nothing at all is sent, and the pick is let go.
#[tokio::test]
async fn a_save_that_cannot_be_written_keeps_every_request_from_going_out() {
    let mut f = Fixture::new().await;
    let blocked = f.root.path().join("not-a-folder");
    std::fs::write(&blocked, b"a file where the state folder would be").expect("written");
    f.pending = Arc::new(LidarrPendingAlbums::new(Some(
        blocked.join("lidarr-pending.json"),
    )));
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);

    let sent: Vec<String> = f
        .changes()
        .into_iter()
        .filter(|change| !change.starts_with("GET"))
        .collect();
    assert!(sent.is_empty(), "{sent:?}");
    assert_eq!(f.lidarr.searches(), 0);
    assert!(f.pending.get(ALBUM_ID).is_none());
}

/// A restart came before Octo's search went out (Lidarr's time was saved, the request never
/// sent), and the user searches the album by hand a minute later: that search is never taken for
/// Octo's, so its grab is the user's, never taken, and Octo steps aside.
#[tokio::test]
async fn a_manual_search_long_after_octos_saved_time_is_the_users() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);
    let client = f.client();
    f.pending.remember(PendingAlbum {
        search_command: 0,
        search_pending: Some(client.now().await.expect("Lidarr's time")),
        holders: vec!["alice".into()],
        ..mezzanine_entry()
    });
    f.lidarr.set_clock_skew(TimeDelta::minutes(1));
    client.album_search(7).await.expect("the user's search");

    service.put_back_after_restart().await;

    over(&f).await;
    assert!(f.taken().is_empty(), "the user's grab is never taken");
}

/// Octo's search reaches Lidarr, which queues it, but the answer is lost on the way back (a proxy
/// in front of Lidarr answers 502): what it started is followed as Octo's, never put back as if
/// nothing were searched, and its import lands.
#[tokio::test]
async fn a_search_whose_answer_is_lost_is_followed_as_octos() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    f.lidarr.lose_next_command_answer("AlbumSearch");
    let service = f.service(None, 30);

    assert!(f.heart_teardrop(&service).await);

    assert_eq!(f.taken(), ["Teardrop"]);
    assert_eq!(f.lidarr.searches(), 1);
    over(&f).await;
}

/// Every guid a grab of the picked release was sent under stays Octo's: the grab's answer is lost,
/// it is never sent again as if Lidarr had refused it, and its guid is saved as Octo's.
#[tokio::test]
async fn a_picked_grab_whose_answer_is_lost_stays_octos_by_its_guid() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr
        .offer("guid-picked", 1, "Massive Attack - Mezzanine", 300_000_000);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    f.lidarr.lose_next_answer("POST", "release");
    let pending = f.pending.clone();
    let client = f.client();
    pending.remember(PendingAlbum {
        holders: vec!["alice".into()],
        search_command: 0,
        ..mezzanine_entry()
    });
    let pick = octo_core::lidarr::LidarrReleasePick {
        guid: "guid-picked".into(),
        indexer_id: 1,
        title: Some("Massive Attack - Mezzanine".into()),
        size: Some(300_000_000),
    };

    let lost = client
        .grab_picked_saving(7, &pick, |guid| {
            let pending = pending.clone();
            async move {
                pending
                    .update_now(ALBUM_ID, |saved| saved.pick_sent(&guid))
                    .await
                    .map(|_| ())
            }
        })
        .await
        .expect_err("its answer was lost");

    assert!(matches!(lost, LidarrError::MaybeSent(_)), "{lost:?}");
    assert_eq!(f.lidarr.grabs().len(), 1, "never sent again as if refused");
    assert!(entry(&f).picked("GUID-PICKED"));
}

/// Lidarr fails Octo's grab and searches again on its own, and holds the release that search
/// found back; Octo's removal of it does not go through, and Lidarr grabs it once its own search
/// is over, as that search's (`Search`): the grab is Octo's, followed to be removed.
#[tokio::test]
async fn a_release_held_back_by_lidarrs_own_new_search_becomes_octos_grab() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 30);
    let heart = heart_later(&f, &service);
    followed(&f, "dl-1").await;
    f.lidarr.plan_held_back(7, "delay");
    f.lidarr.fail_next("DELETE", "queue", 1000);

    f.lidarr.fail("dl-1");
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| !entry.pending_items.is_empty())
    })
    .await;
    let queue_id = entry(&f).pending_items[0].queue_id;
    f.lidarr
        .grab_pending(queue_id, Grab::of("dl-2", &[(3, "Teardrop", "FLAC")]));

    until(|| entry(&f).owns_grab("dl-2")).await;
    heart.abort();
    service.stop_following();
}

/// A release held back while a search of the user's for the album ran beside Octo's: which search
/// held it back cannot be told, so its grab is the user's, never taken.
#[tokio::test]
async fn a_release_held_back_while_a_users_search_ran_beside_octos_is_the_users() {
    let f = Fixture::new().await;
    f.lidarr.plan_held_back(7, "delay");
    f.lidarr.overlap_next_search(7);
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

    assert!(!heart.await.expect("the task"));
    over(&f).await;
    assert!(f.taken().is_empty());
}

/// A restart came before Lidarr's answer to Octo's search was saved, and Lidarr has forgotten the
/// command since: the follow still tells of a file of the user's that Octo's import replaced,
/// counting from Lidarr's time saved before the search went out.
#[tokio::test]
async fn a_replaced_file_is_told_after_a_restart_before_the_searchs_answer_was_saved() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let service = f.service(None, 30);
    let client = f.client();
    let (provider, id) = teardrop(&f);
    f.tracker.begin(
        &provider,
        &id,
        None,
        Some("alice"),
        Some("Massive Attack"),
        Some("Teardrop"),
        Some("Mezzanine"),
    );
    f.pending.remember(PendingAlbum {
        search_command: 0,
        search_pending: Some(client.now().await.expect("Lidarr's time")),
        holders: vec!["alice".into()],
        songs: vec![teardrop_key(&f)],
        ..mezzanine_entry()
    });
    let search = client.album_search(7).await.expect("searched");
    f.lidarr.forget_command(search.id);

    service.put_back_after_restart().await;
    followed(&f, "dl-1").await;
    assert!(entry(&f).searched_at.is_none(), "no answer of Lidarr's was saved");
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

    let key = teardrop_key(&f);
    until(|| notes(&f, &key).iter().any(|(text, _)| text == REPLACED_YOURS)).await;
    service.stop_following();
}

/// A download id Octo grabbed, accounted for, and grabbed again inside its window keeps its first
/// grab's span too: a row of that first grab is still Octo's, one between the grabs is not.
#[test]
fn a_regrabbed_download_keeps_its_earlier_spans() {
    let start = Utc::now();
    let mut entry = mezzanine_entry();
    assert!(entry.record_grab_at("dl-1", Some("qbit"), Some(start)));
    entry
        .grab_mut("dl-1")
        .expect("watched")
        .account(start + TimeDelta::minutes(10));
    entry.watch_again("dl-1", None, start + TimeDelta::hours(1));

    assert!(
        entry.owns_grab_at("dl-1", start + TimeDelta::minutes(5)),
        "the first grab's row"
    );
    assert!(
        !entry.owns_grab_at("dl-1", start + TimeDelta::minutes(30)),
        "between the grabs"
    );
    assert!(
        entry.owns_grab_at("dl-1", start + TimeDelta::hours(2)),
        "the second grab's row"
    );

    let pending = LidarrPendingAlbums::new(None);
    pending.remember(entry);
    pending.forget(ALBUM_ID);
    assert!(pending.is_octos_grab_at("dl-1", start + TimeDelta::minutes(5)));
    assert!(!pending.is_octos_grab_at("dl-1", start + TimeDelta::minutes(30)));
}

/// After a restart: a copy Octo placed that Lidarr replaced and that could not come back before is
/// put back where it was, though its follow is over; a link in the hold path that no copy names
/// any more goes.
#[tokio::test]
async fn a_restart_puts_back_an_owed_copy_and_sweeps_links_nothing_names() {
    let f = Fixture::new().await;
    let service = f.service(None, 30);
    f.pending.remember(mezzanine_entry());
    let links = f.root.path().join(".octo-incoming/held/lidarr-placed");
    std::fs::create_dir_all(&links).expect("made");
    let link = links.join("named.m4a");
    std::fs::write(&link, b"the version alice heard").expect("written");
    let orphan = links.join("orphan.m4a");
    std::fs::write(&orphan, b"a copy that is over").expect("written");
    let copy = f.root.path().join("Massive Attack/Mezzanine/03 - Teardrop.m4a");
    let placed = f.pending.placed_copy(
        &HeldPlacement {
            song: teardrop_key(&f),
            kept: true,
            follows: vec![ALBUM_ID.to_string()],
            users: vec!["alice".into()],
            ..Default::default()
        },
        &copy.to_string_lossy(),
        Some((link.to_string_lossy().into_owned(), true)),
        Vec::new(),
    );
    assert_eq!(placed, crate::services::lidarr::PlacedAs::Octos);
    assert!(f.pending.mark_replaced(&copy.to_string_lossy()));
    f.pending.forget(ALBUM_ID);
    assert_eq!(f.pending.replaced_copies(None, None).len(), 1, "owed back");

    service.put_back_after_restart().await;

    assert_eq!(
        std::fs::read(&copy).expect("back where it was"),
        b"the version alice heard"
    );
    assert!(f.pending.replaced_copies(None, None).is_empty());
    assert!(!orphan.exists(), "nothing names it");
}

/// A copy Octo placed during another open follow of the artist, linked by a rescan, is the
/// artist's only file in Lidarr: read again right before the search, it is never taken for the
/// user's music, so the album is searched.
#[tokio::test]
async fn a_copy_octo_placed_during_another_follow_never_keeps_the_search_out() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]).finished());
    let service = f.service(None, 30);
    f.pending.remember(PendingAlbum {
        foreign_album_id: "rg-protection".into(),
        artist: "Massive Attack".into(),
        title: "Protection".into(),
        search_command: 4,
        ..Default::default()
    });
    let copy = f.lidarr.place_file(
        "/data/music/Massive Attack/Mezzanine/03 - Teardrop.m4a",
        b"the version alice heard",
    );
    f.pending.placed_copy(
        &HeldPlacement {
            song: "soulseek:teardrop-kept".into(),
            kept: true,
            follows: vec!["rg-protection".into()],
            users: vec!["alice".into()],
            ..Default::default()
        },
        &copy.to_string_lossy(),
        None,
        Vec::new(),
    );
    f.lidarr
        .queue_command("RescanFolders", serde_json::json!({}), "manual");
    assert_eq!(
        f.client()
            .artist_track_files(5)
            .await
            .expect("Lidarr answers")
            .len(),
        1,
        "the rescan linked the copy"
    );

    f.heart_teardrop(&service).await;

    assert_eq!(f.searched_albums(), [7], "searched, never left alone");
    service.stop_following();
}

/// Behind a proxy whose clock lags Lidarr's, the `Date` header is the proxy's: a read's time on
/// Lidarr's clock is never before the newest time Lidarr itself stamped on a command it lists.
#[test]
fn a_reads_time_is_never_before_what_lidarr_itself_stamped() {
    use crate::services::lidarr::lidarr_heart_acquisition_service::follow::reading_now;
    use octo_core::lidarr::lidarr_client::LidarrCommand;

    let lidarrs = Utc::now();
    let proxys = lidarrs - TimeDelta::hours(1);
    let command =
        |queued: Option<chrono::DateTime<Utc>>, ended: Option<chrono::DateTime<Utc>>| LidarrCommand {
            id: 1,
            name: "RefreshMonitoredDownloads".into(),
            status: "completed".into(),
            queued,
            started: queued,
            ended,
            trigger: Some("scheduled".into()),
            body: serde_json::Map::new(),
        };

    let now = reading_now(
        proxys,
        &[
            command(Some(lidarrs - TimeDelta::seconds(30)), Some(lidarrs)),
            command(None, None),
        ],
    );
    assert_eq!(now, lidarrs);
    assert_eq!(
        reading_now(lidarrs, &[]),
        lidarrs + TimeDelta::milliseconds(999),
        "the header's second, to its end"
    );
}
