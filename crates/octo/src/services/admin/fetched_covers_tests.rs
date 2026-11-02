use std::sync::Arc;
use std::time::{Duration, SystemTime};

use octo_core::models::download::DownloadHistoryEntry;
use octo_media::cover::cover_image;
use octo_media::tags::{TagFile, TagPicture};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::test_support::flac;

fn sample_cover() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/covers/sample-cover.jpg"
    ))
    .expect("the sample cover")
}

/// The sample cover shrunk to `side`, a picture told apart from the sample by its size.
fn cover_at(side: u32) -> Vec<u8> {
    cover_image::fit_within(&sample_cover(), side)
}

fn side_of(picture: &[u8]) -> u32 {
    let (width, height) = cover_image::measure(picture).expect("a picture");
    width.max(height)
}

fn song(dir: &std::path::Path, name: &str, cover: Option<&[u8]>) -> String {
    let path = dir.join(name).to_string_lossy().into_owned();
    std::fs::write(&path, flac()).expect("written");
    if let Some(cover) = cover {
        let mut file = TagFile::open(&path).expect("opens");
        file.set_pictures(&[TagPicture::front_cover(cover.to_vec(), "image/jpeg")]);
        file.save().expect("saved");
    }
    path
}

fn entry(path: &str, cover: Option<String>) -> DownloadHistoryEntry {
    DownloadHistoryEntry {
        artist: "Zephyr Echo".into(),
        title: "Glass".into(),
        path: path.into(),
        cover_art_url: cover,
        downloaded_at: "2021-06-03T12:00:00.0000000Z".into(),
        ..Default::default()
    }
}

fn covers(budget: Duration) -> FetchedCovers {
    FetchedCovers::with_client(reqwest::Client::new(), budget)
}

/// A server of its own (never a pooled one), so the count of asks is this test's alone.
async fn serving(route: &str, picture: Vec<u8>, times: u64) -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(picture))
        .expect(times)
        .mount(&server)
        .await;
    server
}

#[test]
fn a_page_asks_small_unless_it_asks_for_more_than_200_px() {
    assert_eq!(side_for(None), SMALL_SIDE);
    assert_eq!(side_for(Some("")), SMALL_SIDE);
    assert_eq!(side_for(Some("96")), SMALL_SIDE);
    assert_eq!(side_for(Some("264")), LARGE_SIDE);
    assert_eq!(side_for(Some("lots")), SMALL_SIDE);
}

#[test]
fn the_cover_address_names_the_entry_by_when_it_was_written() {
    assert_eq!(
        entry_cover_path("2026-10-30T01:30:00.9766532Z"),
        "/api/admin/downloads/cover?at=2026-10-30T01%3A30%3A00.9766532Z"
    );
}

/// The song's own picture, made small, and the saved address never asked.
#[tokio::test]
async fn a_song_file_answers_with_its_own_picture() {
    let dir = tempfile::tempdir().expect("a folder");
    let path = song(dir.path(), "song.flac", Some(&sample_cover()));
    let server = serving("/catalog.jpg", sample_cover(), 0).await;

    let picture = covers(FetchedCovers::FILE_BUDGET)
        .entry_cover(
            &entry(&path, Some(format!("{}/catalog.jpg", server.uri()))),
            SMALL_SIDE,
        )
        .await
        .expect("a cover");
    assert_eq!(side_of(&picture), SMALL_SIDE);
    server.verify().await;
}

/// A file with no picture of its own shows its folder's cover, named in any case.
#[tokio::test]
async fn a_file_without_a_picture_shows_its_folders_cover() {
    let dir = tempfile::tempdir().expect("a folder");
    let path = song(dir.path(), "song.flac", None);
    std::fs::write(dir.path().join("notes.txt"), b"not a picture").expect("written");
    std::fs::write(dir.path().join("Folder.JPG"), cover_at(120)).expect("written");

    let picture = covers(FetchedCovers::FILE_BUDGET)
        .entry_cover(&entry(&path, None), SMALL_SIDE)
        .await
        .expect("a cover");
    assert_eq!(side_of(&picture), 120);
    assert_eq!(side_of(&song_picture(&path).expect("a picture")), 120);
}

/// A song whose file is gone is drawn from the saved address, fetched once and then kept; a
/// folder cover beside a gone file says nothing about the song.
#[tokio::test]
async fn a_gone_file_shows_the_saved_cover_fetched_once() {
    let dir = tempfile::tempdir().expect("a folder");
    std::fs::write(dir.path().join("cover.jpg"), cover_at(90)).expect("written");
    let gone = dir.path().join("gone.flac").to_string_lossy().into_owned();
    let server = serving("/catalog.jpg", sample_cover(), 1).await;
    let covers = covers(FetchedCovers::FILE_BUDGET);
    let gone_song = entry(&gone, Some(format!("{}/catalog.jpg", server.uri())));

    for _ in 0..2 {
        let picture = covers.entry_cover(&gone_song, LARGE_SIDE).await.expect("a cover");
        // The sample is 300 px, under the large side, so it comes back whole.
        assert_eq!(side_of(&picture), 300);
    }
    server.verify().await;
    assert!(
        covers
            .entry_cover(&entry(&gone, None), SMALL_SIDE)
            .await
            .is_none()
    );
}

/// A file slower than the budget gives way to the saved address this time, and its own
/// picture answers the next ask.
#[tokio::test]
async fn a_slow_file_gives_way_to_the_saved_cover_and_answers_next_time() {
    let server = serving("/catalog.jpg", sample_cover(), 1).await;
    let covers = covers(Duration::from_millis(50)).reading_songs_with(|_| {
        std::thread::sleep(Duration::from_millis(300));
        Some(cover_at(100))
    });
    let slow = entry("/music/slow.flac", Some(format!("{}/catalog.jpg", server.uri())));

    let first = covers.entry_cover(&slow, SMALL_SIDE).await.expect("a cover");
    assert_eq!(side_of(&first), SMALL_SIDE, "the saved cover, made small");

    let mut next = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let picture = covers.entry_cover(&slow, SMALL_SIDE).await.expect("a cover");
        if side_of(&picture) == 100 {
            next = Some(picture);
            break;
        }
    }
    assert!(next.is_some(), "the file's own picture answers once it is read");
    server.verify().await;
}

/// A file whose picture changes is read again; the same file is not.
#[tokio::test]
async fn a_changed_file_is_read_again() {
    let dir = tempfile::tempdir().expect("a folder");
    let path = song(dir.path(), "song.flac", Some(&cover_at(100)));
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    let covers = covers(FetchedCovers::FILE_BUDGET).reading_songs_with(move |path| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        song_picture(path)
    });

    let before = covers
        .entry_cover(&entry(&path, None), SMALL_SIDE)
        .await
        .expect("a cover");
    assert_eq!(side_of(&before), 100);
    covers
        .entry_cover(&entry(&path, None), SMALL_SIDE)
        .await
        .expect("a cover");
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);

    let mut file = TagFile::open(&path).expect("opens");
    file.set_pictures(&[TagPicture::front_cover(cover_at(80), "image/jpeg")]);
    file.save().expect("saved");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("opens")
        .set_modified(SystemTime::now() + Duration::from_secs(5))
        .expect("a later time");

    let after = covers
        .entry_cover(&entry(&path, None), SMALL_SIDE)
        .await
        .expect("a cover");
    assert_eq!(side_of(&after), 80);
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// An address that is not http, does not answer with a success, or answers with no picture
/// gives no cover.
#[tokio::test]
async fn an_address_without_a_picture_gives_no_cover() {
    let server = MockServer::start().await;
    Mock::given(path("/page.html"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>no cover</html>"))
        .mount(&server)
        .await;
    Mock::given(path("/gone.jpg"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let covers = covers(FetchedCovers::FILE_BUDGET);
    for url in [
        "ftp://example.test/cover.jpg".to_string(),
        "not an address".to_string(),
        format!("{}/page.html", server.uri()),
        format!("{}/gone.jpg", server.uri()),
    ] {
        assert!(covers.outside_cover(&url, SMALL_SIDE).await.is_none(), "{url}");
    }
}

/// A host that does not answer in time is asked again next time, never remembered as having no
/// cover.
#[tokio::test]
async fn an_address_that_does_not_answer_is_asked_again() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/cover.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(sample_cover())
                .set_delay(Duration::from_secs(1)),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/cover.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(sample_cover()))
        .with_priority(2)
        .mount(&server)
        .await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .expect("a client");
    let covers = FetchedCovers::with_client(client, FetchedCovers::FILE_BUDGET);
    let url = format!("{}/cover.jpg", server.uri());

    assert!(covers.outside_cover(&url, SMALL_SIDE).await.is_none());
    assert!(covers.outside_cover(&url, SMALL_SIDE).await.is_some());
}
