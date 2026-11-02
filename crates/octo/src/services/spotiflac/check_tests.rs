use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::TimeZone;
use octo_core::spotiflac::check::SessionRow;

use super::*;

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 30, hour, minute, 0).unwrap()
}

fn row(key: &str, label: &str, ext: &str, state: &str, paused_s: f64, read: DateTime<Utc>) -> CheckRow {
    CheckRow::from_read(
        &SessionRow {
            key: key.into(),
            kind: "gateway".into(),
            label: label.into(),
            extensions: vec![ext.into()],
            state: state.into(),
            auth_paused_s: paused_s,
        },
        read,
    )
}

fn song(link: &str) -> CheckSong {
    CheckSong {
        link: link.into(),
        artist: "Artist".into(),
        title: "Title".into(),
        utc: at(12, 0),
    }
}

fn connection(url: &str) -> Connection {
    Connection {
        url: url.into(),
        token: "t".into(),
    }
}

fn view(url: &str, started: Instant, rows: Vec<CheckRow>) -> Arc<CheckView> {
    Arc::new(CheckView::new(
        connection(url),
        started,
        at(12, 0),
        Reading::Known(rows),
        Reading::Known(vec![]),
    ))
}

#[test]
fn an_older_state_never_lands_over_a_newer_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mornye-check.json");
    let store = MornyeCheckStore::new(Some(path.clone()));
    store.remember(song("https://open.spotify.com/track/a"));
    store.remember(song("https://open.spotify.com/track/b"));
    // The first change's write arriving after the second's is dropped.
    store.write(&path, 1, Ok("{}".into()));
    let kept: CheckFile = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(kept.check_songs.len(), 2);
}

#[test]
fn the_file_keeps_episodes_and_check_songs_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mornye-check.json");
    let store = MornyeCheckStore::new(Some(path.clone()));
    let opened = store.note(
        "http://spotiflac:8000",
        &[row("k", "g", "one", "unverified", 300.0, at(12, 0))],
        at(12, 0),
    );
    assert_eq!(opened, vec!["g"]);
    store.remember(song("https://open.spotify.com/track/a"));
    store.remember(song("https://open.spotify.com/track/b"));
    store.remember(song("https://open.spotify.com/track/a"));

    let again = MornyeCheckStore::new(Some(path));
    let file = again.snapshot();
    assert_eq!(file.episodes.episodes.len(), 1);
    let links: Vec<&str> = file.check_songs.iter().map(|s| s.link.as_str()).collect();
    assert_eq!(
        links,
        vec![
            "https://open.spotify.com/track/a",
            "https://open.spotify.com/track/b"
        ]
    );
    let reopened = again.note(
        "http://spotiflac:8000",
        &[row("k", "g", "one", "unverified", 900.0, at(12, 6))],
        at(12, 6),
    );
    assert!(reopened.is_empty(), "a restart inside an episode opens nothing");
}

#[test]
fn a_damaged_file_reads_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mornye-check.json");
    std::fs::write(&path, "{ not json").unwrap();
    let store = MornyeCheckStore::new(Some(path));
    assert_eq!(store.snapshot(), CheckFile::default());
    store.remember(song("x"));
    assert_eq!(store.snapshot().check_songs.len(), 1);
}

#[test]
fn at_most_five_check_songs_newest_first_and_a_brought_song_leaves() {
    let store = MornyeCheckStore::new(None);
    for n in 0..7 {
        store.remember(song(&format!("link-{n}")));
    }
    let links: Vec<String> = store.snapshot().check_songs.into_iter().map(|s| s.link).collect();
    assert_eq!(links, vec!["link-6", "link-5", "link-4", "link-3", "link-2"]);
    store.note(
        "u",
        &[row("k", "g", "one", "unverified", 300.0, at(12, 0))],
        at(12, 0),
    );
    store.brought(Some("one-web"), "link-5");
    let file = store.snapshot();
    assert!(file.episodes.episodes.is_empty());
    assert!(!file.check_songs.iter().any(|s| s.link == "link-5"));
}

#[test]
fn another_mornye_drops_the_check_songs() {
    let store = MornyeCheckStore::new(None);
    store.note("http://a:8000", &[], at(12, 0));
    store.remember(song("x"));
    store.note("http://b:8000", &[], at(12, 1));
    assert!(store.snapshot().check_songs.is_empty());
}

#[test]
fn an_older_read_never_replaces_a_newer_one() {
    let check = MornyeCheck::in_memory();
    let early = Instant::now();
    let late = early + Duration::from_millis(10);
    assert!(check.keep(&view("u", late, vec![])));
    assert!(!check.keep(&view("u", early, vec![])));
    assert_eq!(
        check.cached(&connection("u"), Some(late)).unwrap().read_started,
        late
    );
    assert!(
        check
            .cached(&connection("u"), Some(late + Duration::from_millis(1)))
            .is_none()
    );
    assert!(check.cached(&connection("other"), None).is_none());
}

#[test]
fn a_read_goes_stale_after_its_time() {
    let check = MornyeCheck::new(
        Arc::new(MornyeCheckStore::new(None)),
        CheckTimes {
            fresh_for: Duration::from_millis(0),
            call_bound: Duration::from_secs(5),
        },
        None,
    );
    check.keep(&view("u", Instant::now(), vec![]));
    assert!(check.cached(&connection("u"), None).is_none());
}

#[test]
fn one_push_when_a_read_opens_episodes() {
    let pushes = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&pushes);
    let check = MornyeCheck::new(
        Arc::new(MornyeCheckStore::new(None)),
        CheckTimes::default(),
        Some(Arc::new(move |_words: String| {
            seen.fetch_add(1, Ordering::SeqCst);
        })),
    );
    let paused = view(
        "u",
        Instant::now(),
        vec![row("k", "g", "one", "unverified", 300.0, at(12, 0))],
    );
    check.note(&paused, |labels, _| labels.join(","));
    check.note(&paused, |labels, _| labels.join(","));
    assert_eq!(pushes.load(Ordering::SeqCst), 1);
    let cannot = Arc::new(CheckView::new(
        connection("u"),
        Instant::now(),
        at(12, 0),
        Reading::CannotTell("timed out".into()),
        Reading::CannotTell("timed out".into()),
    ));
    check.note(&cannot, |labels, _| labels.join(","));
    assert_eq!(pushes.load(Ordering::SeqCst), 1);
}

#[test]
fn the_utilities_extension_is_named_missing_from_any_call() {
    let missing = CheckView::new(
        connection("u"),
        Instant::now(),
        at(12, 0),
        Reading::CannotTell(
            "The optional utilities extension is not installed; 'mornye.core.signed_session_status' requires 'utilities.sflx' to be available.".into(),
        ),
        Reading::Known(vec![]),
    );
    assert!(missing.utilities_missing);
}

#[test]
fn the_push_says_what_happens_meanwhile_and_where_to_pass() {
    let labels = vec!["example-ext".to_string()];
    let meanwhile = |soulseek_on, then_mp3| Meanwhile {
        soulseek_on,
        then_mp3,
    };
    assert_eq!(
        push_words(
            &labels,
            Some(at(14, 5)),
            meanwhile(true, false),
            "https://octo.example/",
            at(12, 0)
        ),
        "Mornye could not pass the check for example-ext, so Octo asks Soulseek in the meantime. Mornye allows its next try at 14:05 UTC. Pass it on the dashboard: https://octo.example/admin/#spotiflac"
    );
    assert_eq!(
        push_words(&labels, None, meanwhile(false, false), "", at(12, 0)),
        "Mornye could not pass the check for example-ext, so songs only Mornye can bring fail until it passes. Pass it on the Mornye page of Octo's dashboard."
    );
    // Soulseek then YouTube: songs come as MP3s, they do not fail.
    assert!(
        push_words(&labels, None, meanwhile(true, true), "", at(12, 0))
            .contains(", so Octo asks Soulseek, then YouTube, in the meantime.")
    );
    assert!(
        push_words(&labels, None, meanwhile(false, true), "", at(12, 0))
            .contains(", so Octo gets MP3s from YouTube in the meantime.")
    );
}

#[test]
fn the_meanwhile_follows_the_download_source() {
    use octo_core::settings::DownloadSource;
    let all = |source, slskd_on: bool| {
        let mut all = octo_core::settings::AppSettings::default();
        all.subsonic.download_source = source;
        all.soulseek.enabled = slskd_on;
        all.soulseek.base_url = slskd_on.then(|| "http://slskd:5030".to_string());
        all
    };
    assert_eq!(
        Meanwhile::of(&all(DownloadSource::SoulseekThenYouTube, false), false),
        Meanwhile {
            soulseek_on: false,
            then_mp3: true
        }
    );
    assert_eq!(
        Meanwhile::of(&all(DownloadSource::Soulseek, true), true),
        Meanwhile {
            soulseek_on: true,
            then_mp3: false
        }
    );
    // Whether Soulseek is asked is the downloads' rule, not the live switch.
    assert!(Meanwhile::of(&all(DownloadSource::Soulseek, false), true).soulseek_on);
    assert!(!Meanwhile::of(&all(DownloadSource::Soulseek, true), false).soulseek_on);
}

#[test]
fn mornye_s_client_reads_slskd_by_its_link() {
    use std::sync::Arc;

    use crate::services::common::test_fakes::LoggedInLink;
    use crate::services::spotiflac::SpotiflacClient;
    let settings = Arc::new(octo_core::settings::SettingsStore::for_tests(
        octo_core::settings::AppSettings::default(),
    ));
    let client = SpotiflacClient::new(settings);
    // No link: the live settings decide.
    assert!(!client.slskd_in_use());
    // Switched off in settings, but the running slskd link is on.
    let client = client.with_slskd(Arc::new(LoggedInLink));
    assert!(client.slskd_in_use());
}

#[test]
fn a_provider_s_own_pause_comes_from_its_covering_rows() {
    let rows = vec![
        row("a", "example-ext", "example-ext", "unverified", 900.0, at(12, 0)),
        row(
            "b",
            "example-ext-two",
            "example-ext-two",
            "active",
            900.0,
            at(12, 0),
        ),
    ];
    let view = view("u", Instant::now(), rows);
    let pause = view.pause_of("ext:example-ext-web", at(12, 1)).unwrap();
    assert_eq!(pause.labels, vec!["example-ext"]);
    assert_eq!(pause.until_utc, at(12, 15));
    assert!(view.pause_of("example-ext-two", at(12, 1)).is_none());
}
