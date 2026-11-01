use chrono::TimeZone;

use super::*;

fn at(hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 30, hour, minute, second).unwrap()
}

fn session(key: &str, label: &str, extensions: &[&str], state: &str, paused_s: f64) -> SessionRow {
    SessionRow {
        key: key.into(),
        kind: "gateway".into(),
        label: label.into(),
        extensions: extensions.iter().map(|e| e.to_string()).collect(),
        state: state.into(),
        auth_paused_s: paused_s,
    }
}

fn rows(sessions: &[SessionRow], read: DateTime<Utc>) -> Reading<Vec<CheckRow>> {
    check_rows(
        &SessionsAnswer {
            sessions: sessions.to_vec(),
            error: None,
        },
        read,
    )
}

fn ids(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

#[test]
fn an_extension_id_follows_mornyes_rule_and_keeps_case() {
    assert_eq!(extension_id(" ext:Example-Ext-web "), "Example-Ext");
    assert_eq!(extension_id("example-ext-py"), "example-ext");
    assert_eq!(extension_id("example-ext-py-web"), "example-ext");
    assert_eq!(extension_id("example-ext"), "example-ext");
    assert_ne!(extension_id("Example-Ext"), extension_id("example-ext"));
}

#[test]
fn every_usable_extension_paused_gives_the_earliest_freeing_moment() {
    let read = at(12, 0, 0);
    let rows = rows(
        &[
            session("a", "gateway-v2-abc", &["one-web"], "unverified", 300.0),
            session("b", "gateway-v2-def", &["two"], "expired", 900.0),
        ],
        read,
    );
    let pause = paused_for(&rows, &ids(&["one", "two"]), at(12, 1, 0)).expect("paused");
    assert_eq!(pause.until_utc, at(12, 5, 0));
    assert_eq!(pause.labels, vec!["gateway-v2-abc", "gateway-v2-def"]);
}

#[test]
fn one_free_usable_extension_means_mornye_is_asked() {
    let read = at(12, 0, 0);
    let rows = rows(
        &[session("a", "gateway-v2-abc", &["one"], "unverified", 300.0)],
        read,
    );
    assert_eq!(paused_for(&rows, &ids(&["one", "two"]), read), None);
}

#[test]
fn a_working_session_on_a_paused_gateway_is_never_kept_back() {
    // Mornye reads its gateway's pause only for a session that is not authenticated, so a row
    // whose own session works downloads although its gateway shows a pause.
    let read = at(12, 0, 0);
    let rows = rows(
        &[
            session("a", "gateway-v2-abc", &["example-ext"], "unverified", 900.0),
            session("b", "gateway-v2-abc", &["example-ext-two"], "active", 900.0),
        ],
        read,
    );
    assert_eq!(paused_for(&rows, &ids(&["example-ext-two"]), read), None);
    assert!(paused_for(&rows, &ids(&["example-ext"]), read).is_some());
}

#[test]
fn a_pause_is_fixed_when_it_is_read_and_ends_on_time() {
    let read = at(12, 0, 0);
    let rows = rows(&[session("a", "g", &["one"], "unverified", 30.0)], read);
    assert!(paused_for(&rows, &ids(&["one"]), at(12, 0, 29)).is_some());
    assert_eq!(paused_for(&rows, &ids(&["one"]), at(12, 0, 45)), None);
    let a = paused_for(&rows, &ids(&["one"]), at(12, 0, 10)).unwrap();
    let b = paused_for(&rows, &ids(&["one"]), at(12, 0, 11)).unwrap();
    assert_eq!(a.until_utc, b.until_utc);
}

#[test]
fn octo_asks_mornye_when_it_cannot_tell() {
    let read = at(12, 0, 0);
    let unknown: Reading<Vec<CheckRow>> = Reading::CannotTell("no answer".into());
    assert_eq!(paused_for(&unknown, &ids(&["one"]), read), None);
    let known = rows(&[session("a", "g", &["one"], "unverified", 300.0)], read);
    assert_eq!(paused_for(&known, &BTreeSet::new(), read), None);
    let errored = check_rows(
        &SessionsAnswer {
            sessions: vec![],
            error: Some("The utilities extension is not installed. Configure ...".into()),
        },
        read,
    );
    assert!(matches!(errored, Reading::CannotTell(_)));
}

#[test]
fn the_usable_extensions_come_from_the_setting_within_what_mornye_offers() {
    let services = Reading::Known(vec![
        ServiceRow {
            id: "ext:one-web".into(),
            label: "One".into(),
            extensions: vec!["one-web".into()],
        },
        ServiceRow {
            id: "ext:two".into(),
            label: "Two".into(),
            extensions: vec!["two".into(), "two-py".into()],
        },
    ]);
    assert_eq!(usable(&[], &services), ids(&["one", "two"]));
    assert_eq!(usable(&["ext:two-web".into()], &services), ids(&["two"]));
    assert_eq!(usable(&["missing".into()], &services), BTreeSet::new());
    assert_eq!(usable(&["One".into()], &services), BTreeSet::new());
    let unknown = Reading::CannotTell("timed out".into());
    assert_eq!(usable(&["ext:one".into()], &unknown), ids(&["one"]));
    assert_eq!(usable(&[], &unknown), BTreeSet::new());
}

#[test]
fn mornyes_dashes_become_plain_punctuation() {
    assert_eq!(
        plain_dashes("verification paused after a failed attempt \u{2014} next try in 4m"),
        "verification paused after a failed attempt, next try in 4m"
    );
    assert_eq!(plain_dashes("tracks 1\u{2013}2"), "tracks 1 to 2");
    assert_eq!(plain_dashes("A \u{2013} B"), "A - B");
}

#[test]
fn a_moment_reads_as_a_time_today_and_with_its_date_otherwise() {
    let now = at(12, 0, 0);
    assert_eq!(until_words(at(14, 4, 1), now), "14:05 UTC");
    assert_eq!(until_words(at(14, 5, 0), now), "14:05 UTC");
    let tomorrow = Utc.with_ymd_and_hms(2026, 10, 31, 0, 30, 0).unwrap();
    assert_eq!(until_words(tomorrow, now), "Oct 31, 00:30 UTC");
}

fn health(provider: &str, outcome: &str, error: &str) -> ExtensionRow {
    ExtensionRow {
        installed: true,
        provider: provider.into(),
        attempts: 4,
        failures: 3,
        last_outcome: outcome.into(),
        last_error: error.into(),
    }
}

const THEIR_DIR: &str = "/music/.octo-incoming/spotiflac/0123456789abcdef0123456789abcdef";

#[test]
fn a_missing_utilities_extension_is_named_first() {
    let row = health("example-ext", "failure", "anything");
    let words = failure_words(&FailureFacts {
        reason: "The utilities extension is not installed. Configure MORNYE_REGISTRIES ...",
        provider: Some("example-ext"),
        health: Some(&row),
        own_job: true,
        their_dir: THEIR_DIR,
        utilities_missing: false,
    });
    assert_eq!(
        words.shown,
        "Mornye needs its utilities extension, which your registry does not list."
    );
}

#[test]
fn the_utilities_fix_reaches_the_download_log_whole() {
    let detail = utilities_words().detail.expect("a fix");
    assert_eq!(
        crate::common::acquisition_tracker::user_safe(Some(&detail)).as_deref(),
        Some(detail.trim_end_matches('.')),
        "the log keeps the first sentence only, so the fix is one sentence under the cap"
    );
}

#[test]
fn a_refused_write_into_the_job_folder_is_named_only_for_this_job() {
    let row = health(
        "example-ext",
        "failure",
        &format!("EACCES: permission denied, open '{THEIR_DIR}/x.flac'"),
    );
    let facts = FailureFacts {
        reason: "download_failed",
        provider: Some("example-ext"),
        health: Some(&row),
        own_job: true,
        their_dir: THEIR_DIR,
        utilities_missing: false,
    };
    let words = failure_words(&facts);
    assert_eq!(
        words.shown,
        "Mornye could not write the song into Octo's music folder; the Test on the Mornye page checks that folder."
    );
    assert_eq!(
        words.detail.as_deref(),
        Some(format!("EACCES: permission denied, open '{THEIR_DIR}/x.flac'").as_str()),
        "the system's own words go to the log's detail"
    );
    let python = health(
        "example-ext",
        "failure",
        &format!("[Errno 13] Permission denied: '{THEIR_DIR}/x.flac'"),
    );
    assert_eq!(
        failure_words(&FailureFacts {
            health: Some(&python),
            ..facts
        })
        .shown,
        "Mornye could not write the song into Octo's music folder; the Test on the Mornye page checks that folder."
    );
    let other = failure_words(&FailureFacts {
        own_job: false,
        ..facts
    });
    assert!(
        other
            .shown
            .starts_with("Mornye's example-ext extension failed; its last try said: EACCES"),
        "{}",
        other.shown
    );
}

#[test]
fn what_the_extension_said_is_quoted_with_its_name() {
    let row = health("example-ext", "failure", "Track not available: not found");
    let own = failure_words(&FailureFacts {
        reason: "download_failed",
        provider: Some("example-ext"),
        health: Some(&row),
        own_job: true,
        their_dir: THEIR_DIR,
        utilities_missing: false,
    });
    assert_eq!(
        own.shown,
        "Mornye's example-ext extension could not get the song: Track not available: not found"
    );
}

#[test]
fn mornyes_codes_read_as_words_and_none_is_no_extension() {
    let facts = FailureFacts {
        reason: "download_failed",
        provider: named_provider("none"),
        health: None,
        own_job: false,
        their_dir: THEIR_DIR,
        utilities_missing: false,
    };
    assert_eq!(failure_words(&facts).shown, "Mornye failed and gave no reason.");
    assert_eq!(
        failure_words(&FailureFacts {
            provider: Some("example-ext"),
            ..facts
        })
        .shown,
        "Mornye's example-ext extension failed and gave no reason."
    );
    assert_eq!(
        failure_words(&FailureFacts {
            reason: "source_not_supported",
            ..facts
        })
        .shown,
        "Mornye's extensions cannot download this link from the catalog."
    );
    assert_eq!(
        failure_words(&FailureFacts {
            reason: "Track not available \u{2014} region",
            ..facts
        })
        .shown,
        "Track not available, region"
    );
    let succeeded = health("example-ext", "success", "");
    assert_eq!(
        failure_words(&FailureFacts {
            provider: Some("example-ext"),
            health: Some(&succeeded),
            ..facts
        })
        .shown,
        "Mornye's example-ext extension failed and gave no reason."
    );
}

#[test]
fn the_gate_says_who_and_when() {
    let pause = CheckPause {
        until_utc: at(14, 4, 30),
        labels: vec!["gateway-v2-abc".into(), "gateway-v2-def".into()],
    };
    let (shown, detail) = gate_words(&pause, at(12, 0, 0));
    assert_eq!(
        shown,
        "Mornye could not pass its check for gateway-v2-abc and gateway-v2-def, so Octo did not ask it for this song."
    );
    assert_eq!(
        detail,
        "It allows its next try at 14:05 UTC; the next song after that lets it try."
    );
}

fn paused_row(
    key: &str,
    label: &str,
    ext: &str,
    state: &str,
    paused_s: f64,
    read: DateTime<Utc>,
) -> CheckRow {
    CheckRow::from_read(&session(key, label, &[ext], state, paused_s), read)
}

#[test]
fn one_episode_for_a_chain_of_failed_tries_however_quiet_the_server() {
    let mut episodes = Episodes::default();
    let url = "http://spotiflac:8000";
    // 5 min pause, read at 12:00; a song at 12:30 tries and fails into 15 min; one at 13:00 into 45.
    let (opened, _) = episodes.note(
        url,
        &[paused_row("k", "g", "one", "unverified", 300.0, at(12, 0, 0))],
        at(12, 0, 0),
    );
    assert_eq!(opened, vec!["g"]);
    let (opened, _) = episodes.note(
        url,
        &[paused_row("k", "g", "one", "unverified", 900.0, at(12, 30, 0))],
        at(12, 30, 0),
    );
    assert!(opened.is_empty());
    let (opened, _) = episodes.note(
        url,
        &[paused_row("k", "g", "one", "unverified", 2700.0, at(13, 0, 0))],
        at(13, 0, 0),
    );
    assert!(opened.is_empty());
    // An unverified row between tries keeps the episode open.
    let (opened, changed) = episodes.note(
        url,
        &[paused_row("k", "g", "one", "unverified", 0.0, at(14, 0, 0))],
        at(14, 0, 0),
    );
    assert!(opened.is_empty() && !changed);
}

#[test]
fn a_working_session_ends_the_episode_and_the_next_pause_opens_a_new_one() {
    let mut episodes = Episodes::default();
    let url = "http://spotiflac:8000";
    episodes.note(
        url,
        &[paused_row("k", "g", "one", "unverified", 300.0, at(12, 0, 0))],
        at(12, 0, 0),
    );
    let (_, changed) = episodes.note(
        url,
        &[paused_row("k", "g", "one", "active", 0.0, at(12, 10, 0))],
        at(12, 10, 0),
    );
    assert!(changed);
    let (opened, _) = episodes.note(
        url,
        &[paused_row("k", "g", "one", "expired", 300.0, at(15, 0, 0))],
        at(15, 0, 0),
    );
    assert_eq!(opened, vec!["g"]);
}

#[test]
fn a_song_brought_through_an_extension_ends_its_episode() {
    let mut episodes = Episodes::default();
    let url = "http://spotiflac:8000";
    episodes.note(
        url,
        &[paused_row("k", "g", "one", "unverified", 300.0, at(12, 0, 0))],
        at(12, 0, 0),
    );
    assert!(!episodes.brought_through("two"));
    assert!(episodes.brought_through("one"));
    assert!(episodes.episodes.is_empty());
}

#[test]
fn two_gateways_open_two_episodes_and_mornye_forgetting_opens_a_new_one() {
    let mut episodes = Episodes::default();
    let url = "http://spotiflac:8000";
    let read = at(12, 0, 0);
    let (opened, _) = episodes.note(
        url,
        &[
            paused_row("k1", "g1", "one", "unverified", 300.0, read),
            paused_row("k2", "g2", "two", "unverified", 300.0, read),
        ],
        read,
    );
    assert_eq!(opened, vec!["g1", "g2"]);
    let later = Utc.with_ymd_and_hms(2026, 10, 31, 13, 0, 0).unwrap();
    let (opened, _) = episodes.note(
        url,
        &[
            paused_row("k1", "g1", "one", "unverified", 300.0, later),
            paused_row("k2", "g2", "two", "unverified", 0.0, later),
        ],
        later,
    );
    assert_eq!(opened, vec!["g1"]);
}

#[test]
fn another_mornye_starts_the_episodes_over_and_they_survive_a_round_trip() {
    let mut episodes = Episodes::default();
    episodes.note(
        "http://a:8000",
        &[paused_row("k", "g", "one", "unverified", 300.0, at(12, 0, 0))],
        at(12, 0, 0),
    );
    let text = serde_json::to_string(&episodes).unwrap();
    let mut back: Episodes = serde_json::from_str(&text).unwrap();
    assert_eq!(back, episodes);
    let (opened, _) = back.note(
        "http://a:8000",
        &[paused_row("k", "g", "one", "unverified", 900.0, at(12, 6, 0))],
        at(12, 6, 0),
    );
    assert!(opened.is_empty(), "a restart inside an episode pushes nothing");
    let (opened, changed) = back.note("http://b:8000", &[], at(12, 7, 0));
    assert!(opened.is_empty() && changed && back.episodes.is_empty());
}

#[test]
fn only_a_mornye_before_1_0_4_cannot_pass_its_check() {
    let info = |name: &str, version: &str| ServerInfo {
        name: name.into(),
        version: version.into(),
        ..Default::default()
    };
    assert!(info("Mornye", "1.0.3").cannot_pass_check());
    assert!(!info("Mornye", "1.0.4").cannot_pass_check());
    assert!(!info("Mornye", "1.1.0").cannot_pass_check());
    assert!(!info("Mornye", "").cannot_pass_check());
    assert!(!info("SpotiFLAC", "0.9.0").cannot_pass_check());
}

#[test]
fn mornyes_own_pause_words_give_its_next_try() {
    let now = at(12, 0, 0);
    assert_eq!(
        next_try_from_words(
            "verification paused after a failed attempt \u{2014} next try in 4 min",
            now
        ),
        Some(at(12, 4, 0))
    );
    assert_eq!(
        next_try_from_words(
            "RuntimeError: verification paused after a failed attempt, next try in 1 h 05 min",
            now
        ),
        Some(at(13, 5, 0))
    );
    assert_eq!(next_try_from_words("download_failed", now), None);
}

#[test]
fn a_jobs_extension_reads_from_its_item_whatever_mornye_puts_there() {
    let job = |provider: &str| -> crate::spotiflac::Job {
        serde_json::from_str(&format!(
            r#"{{"id": "j", "status": "failed", "payload": {{"items_detail": [{{"status": "FAILED", "provider": {provider}}}]}}}}"#
        ))
        .expect("a job")
    };
    assert_eq!(job("null").provider(), None);
    assert_eq!(job(r#""none""#).provider(), None);
    assert_eq!(job(r#""example-ext""#).provider(), Some("example-ext"));
}

#[test]
fn an_extensions_tries_are_read_from_the_row_that_counts_them() {
    // Mornye 1.0.5 and 1.1.0: the tries sit on the bare name, the installed row has none.
    let answer: crate::spotiflac::ExtensionsAnswer = serde_json::from_str(
        r#"{"providers": [
            {"provider": "example-ext", "attempts": 4, "failures": 3, "last_outcome": "failure", "last_error": "EACCES", "installed": false},
            {"provider": "ext:example-ext", "attempts": 0, "failures": 0, "last_outcome": "", "last_error": "", "installed": true}
        ]}"#,
    )
    .unwrap();
    for name in ["example-ext", "ext:example-ext"] {
        let row = answer.row(name).expect("a row");
        assert_eq!(
            (row.provider.as_str(), row.failures),
            ("example-ext", 3),
            "{name}"
        );
    }
    assert!(answer.row("other").is_none());
}

#[test]
fn an_answer_without_its_result_does_not_parse() {
    assert!(serde_json::from_str::<RpcAnswer<SessionsAnswer>>(r#"{"sessions": []}"#).is_err());
    let answer: RpcAnswer<SessionsAnswer> =
        serde_json::from_str(r#"{"result": {"sessions": [{"key": "k", "auth_paused_s": 12.5}]}}"#).unwrap();
    assert_eq!(answer.result.sessions[0].auth_paused_s, 12.5);
}
