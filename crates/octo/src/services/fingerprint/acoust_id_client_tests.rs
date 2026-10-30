//! Tests for AcoustID by class against a stub: which key each call uses, Octo's own key and
//! `off`, AcoustID's error codes, rejected keys set aside and tried again, the turn taken before
//! the timer, held lookups resumed, and a refused submission key.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use octo_core::settings::{AppSettings, SoulseekSettings};
use wiremock::matchers::{body_string_contains, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::fingerprint::acoust_id_rate_limiter::{KeyLane, LookupClass};

const OK: &str = r#"{"status":"ok","results":[{"id":"fp","score":0.97,"recordings":[{"id":"rec-1","title":"Help!","sources":3}]}]}"#;
const OCTO_KEY: &str = "octo-key";

fn error(code: i64) -> String {
    format!(r#"{{"status":"error","error":{{"code":{code},"message":"error {code}"}}}}"#)
}

fn settings_with(key: &str) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: SoulseekSettings {
            acoust_id_api_key: key.into(),
            acoust_id_timeout_seconds: 2,
            ..Default::default()
        },
        ..Default::default()
    }))
}

/// A limiter whose gaps are short enough for a test in real time, with no random spread.
fn quick() -> AcoustIdRateLimiter {
    AcoustIdRateLimiter::with_gaps(Duration::from_millis(10), Duration::from_millis(100)).with_jitter(|| 1.0)
}

fn client_on(
    server: &MockServer,
    settings: &Arc<SettingsStore>,
    limiter: AcoustIdRateLimiter,
    built_in: Option<&str>,
) -> AcoustIdClient {
    let base = url::Url::parse(&format!("{}/", server.uri())).expect("parses");
    let handler = AcoustIdRateLimitHandler::with_base_url(Arc::new(limiter), base);
    AcoustIdClient::with_built_in_key(Arc::new(handler), Arc::clone(settings), built_in)
}

async fn answering(body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/v2/lookup"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    server
}

async fn sent_with(server: &MockServer, key: &str) -> usize {
    let wanted = format!("client={key}&");
    server
        .received_requests()
        .await
        .expect("recorded")
        .iter()
        .filter(|request| String::from_utf8_lossy(&request.body).starts_with(&wanted))
        .count()
}

/// A blank key looks up with Octo's own key; `off` turns AcoustID off with nothing sent.
#[tokio::test]
async fn a_blank_key_uses_octos_own_and_off_turns_acoustid_off() {
    let server = answering(OK).await;
    let blank = settings_with("");
    let client = client_on(&server, &blank, quick(), Some(OCTO_KEY));
    assert!(client.is_on());
    let outcome = client.lookup_answer(LookupClass::Heart, "AQAD", 200).await;
    assert!(matches!(outcome, AcoustIdOutcome::Answered(_)), "{outcome:?}");
    assert_eq!(sent_with(&server, OCTO_KEY).await, 1);

    let off = settings_with("off");
    let client = client_on(&server, &off, quick(), Some(OCTO_KEY));
    assert!(!client.is_on());
    assert_eq!(
        client.lookup_answer(LookupClass::Heart, "AQAD", 200).await,
        AcoustIdOutcome::Off
    );
    assert_eq!(client.key_for(LookupClass::Bulk), None);
    assert_eq!(server.received_requests().await.expect("recorded").len(), 1);
}

/// Octo's own key counts only once it is set: until then an install without a key of its own has
/// AcoustID off.
#[tokio::test]
async fn octos_own_key_counts_as_off_until_it_is_set() {
    let server = answering(OK).await;
    let keyless = settings_with("");
    let unset = client_on(&server, &keyless, quick(), None);
    assert!(!unset.is_on());
    assert_eq!(
        unset.lookup_answer(LookupClass::Heart, "AQAD", 200).await,
        AcoustIdOutcome::Off
    );
    assert!(server.received_requests().await.expect("recorded").is_empty());
    let built = AcoustIdClient::new(
        Arc::new(AcoustIdRateLimitHandler::new(Arc::new(quick()))),
        Arc::clone(&keyless),
    );
    assert_eq!(
        built.is_on(),
        !octo_core::settings::soulseek::OCTO_ACOUSTID_APPLICATION_KEY
            .trim()
            .is_empty()
    );
}

/// One person's calls read the heart key and bulk work the bulk key: with a key of the user's
/// own both use it, without one both use Octo's own, bulk work at its own lowest place, and a
/// family request uses the heart key.
#[tokio::test]
async fn heart_and_bulk_calls_read_their_keys() {
    let server = answering(OK).await;
    let own = settings_with("user-key");
    let client = client_on(&server, &own, quick(), Some(OCTO_KEY));
    for class in [LookupClass::Heart, LookupClass::Family, LookupClass::Bulk] {
        assert_eq!(client.key_for(class).as_deref(), Some("user-key"), "{class:?}");
        client.lookup_answer(class, "AQAD", 200).await;
    }
    assert_eq!(sent_with(&server, "user-key").await, 3);

    let keyless = settings_with("");
    let limiter = quick();
    let client = client_on(&server, &keyless, limiter, Some(OCTO_KEY));
    assert_eq!(
        client.key_for(LookupClass::Family),
        client.key_for(LookupClass::Heart)
    );
    let start = Instant::now();
    client.lookup_answer(LookupClass::Bulk, "AQAD", 200).await;
    client.lookup_answer(LookupClass::Heart, "AQAD", 200).await;
    client.lookup_answer(LookupClass::Bulk, "AQAD", 200).await;
    assert!(start.elapsed() >= Duration::from_millis(100), "the bulk gap");
    assert_eq!(sent_with(&server, OCTO_KEY).await, 3);
}

/// Codes 4, 12, 16 and 17 set the key aside: the user's key is not asked again and Octo's own is
/// used; once both are rejected AcoustID is off, and any settings change tries them again.
#[tokio::test]
async fn rejected_keys_are_set_aside_until_the_settings_change() {
    for code in [4, 12, 16, 17] {
        let server = MockServer::start().await;
        Mock::given(path("/v2/lookup"))
            .and(body_string_contains("client=user-key&"))
            .respond_with(ResponseTemplate::new(200).set_body_string(error(code)))
            .mount(&server)
            .await;
        Mock::given(path("/v2/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_string(OK))
            .mount(&server)
            .await;
        let settings = settings_with("user-key");
        let client = client_on(&server, &settings, quick(), Some(OCTO_KEY));
        for _ in 0..2 {
            let outcome = client.lookup_answer(LookupClass::Heart, "AQAD", 200).await;
            assert!(
                matches!(outcome, AcoustIdOutcome::Answered(_)),
                "{code}: {outcome:?}"
            );
        }
        assert_eq!(sent_with(&server, "user-key").await, 1, "{code}");
        assert_eq!(sent_with(&server, OCTO_KEY).await, 2, "{code}");
        settings.set((*settings.current()).clone());
        client.lookup_answer(LookupClass::Heart, "AQAD", 200).await;
        assert_eq!(sent_with(&server, "user-key").await, 2, "{code}: tried again");
    }

    let server = answering(&error(4)).await;
    let settings = settings_with("user-key");
    let client = client_on(&server, &settings, quick(), Some(OCTO_KEY));
    let (tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    client.subscribe_on(Arc::new(move |on| {
        let _ = tx.send(on);
    }));
    assert_eq!(
        client.lookup_answer(LookupClass::Heart, "AQAD", 200).await,
        AcoustIdOutcome::Off
    );
    assert!(!client.is_on());
    assert_eq!(heard.recv().await, Some(false));
    assert_eq!(
        client.lookup_answer(LookupClass::Heart, "AQAD", 200).await,
        AcoustIdOutcome::Off
    );
    assert_eq!(server.received_requests().await.expect("recorded").len(), 2);
    settings.set((*settings.current()).clone());
    let back = tokio::time::timeout(Duration::from_secs(5), heard.recv()).await;
    assert_eq!(back.expect("heard in time"), Some(true));
    assert!(client.is_on());
}

/// The `off` setting saved is heard by every listener, with no call made.
#[tokio::test]
async fn turning_acoustid_off_is_heard_without_a_call() {
    let server = answering(OK).await;
    let settings = settings_with("user-key");
    let client = client_on(&server, &settings, quick(), Some(OCTO_KEY));
    let (tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    client.subscribe_on(Arc::new(move |on| {
        let _ = tx.send(on);
    }));
    let mut off = (*settings.current()).clone();
    off.soulseek.acoust_id_api_key = "off".into();
    settings.set(off);
    let heard_off = tokio::time::timeout(Duration::from_secs(5), heard.recv()).await;
    assert_eq!(heard_off.expect("heard in time"), Some(false));
    assert!(server.received_requests().await.expect("recorded").is_empty());
}

/// Codes for one fingerprint AcoustID cannot check, a code it does not list and an error with no
/// code are that fingerprint unreadable: never "no entry", never sent again, the key untouched.
#[tokio::test]
async fn codes_for_one_fingerprint_are_unreadable_and_never_sent_again() {
    let mut cases: Vec<(String, Option<i64>)> = [1, 2, 3, 7, 8, 9, 10, 11, 19, 6, 99]
        .into_iter()
        .map(|code| (error(code), Some(code)))
        .collect();
    cases.push((r#"{"status":"error"}"#.into(), None));
    for (body, code) in cases {
        let server = answering(&body).await;
        let settings = settings_with("user-key");
        let client = client_on(&server, &settings, quick(), Some(OCTO_KEY));
        assert_eq!(
            client.lookup_answer(LookupClass::Heart, "AQAD", 200).await,
            AcoustIdOutcome::Unreadable { code },
            "{body}"
        );
        assert_eq!(
            server.received_requests().await.expect("recorded").len(),
            1,
            "{body}"
        );
        assert_eq!(client.key_for(LookupClass::Heart).as_deref(), Some("user-key"));
        assert_eq!(
            client.http.limiter().gap(KeyLane::User, LookupClass::Heart),
            AcoustIdRateLimiter::USER_GAP,
            "{body}: an answer"
        );
    }
}

/// Codes 5, 13 and 14, a 429, a 500 (with or without code 5 inside it), a 503, a body that cannot
/// be read and a timeout are Busy, and back the lane off: overloads and outages both.
#[tokio::test]
async fn busy_answers_are_busy_and_back_the_lane_off() {
    let cases: Vec<(ResponseTemplate, &str)> = vec![
        (ResponseTemplate::new(200).set_body_string(error(5)), "code 5"),
        (ResponseTemplate::new(200).set_body_string(error(13)), "code 13"),
        (ResponseTemplate::new(200).set_body_string(error(14)), "code 14"),
        (ResponseTemplate::new(429), "429"),
        (ResponseTemplate::new(500), "500"),
        (
            ResponseTemplate::new(500).set_body_string(error(5)),
            "code 5 in a 500",
        ),
        (ResponseTemplate::new(503), "503"),
        (ResponseTemplate::new(200).set_body_string("not json"), "not json"),
        (
            ResponseTemplate::new(200)
                .set_body_string(OK)
                .set_delay(Duration::from_secs(3)),
            "a timeout",
        ),
    ];
    for (response, name) in cases {
        let server = MockServer::start().await;
        Mock::given(path("/v2/lookup"))
            .respond_with(response)
            .mount(&server)
            .await;
        let settings = settings_with("");
        let client = client_on(&server, &settings, quick(), Some(OCTO_KEY));
        assert_eq!(
            client.lookup_answer(LookupClass::Heart, "AQAD", 200).await,
            AcoustIdOutcome::Busy,
            "{name}"
        );
        assert_eq!(
            client.http.limiter().gap(KeyLane::BuiltIn, LookupClass::Heart),
            Duration::from_millis(20),
            "{name}"
        );
        assert!(client.is_on(), "{name}: a busy answer never sets a key aside");
    }
}

/// Answers sent one after another by a stub: each call takes the next.
struct Series {
    answers: Vec<ResponseTemplate>,
    next: AtomicUsize,
}

impl Respond for Series {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let at = self.next.fetch_add(1, Ordering::SeqCst);
        self.answers[at.min(self.answers.len() - 1)].clone()
    }
}

async fn series(answers: Vec<ResponseTemplate>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/v2/lookup"))
        .respond_with(Series {
            answers,
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    server
}

/// Against a stub answering 429, the gap doubles past where an outage stops; against one
/// answering 503 it stops at the outage cap; answers halve it again. On both lanes.
#[tokio::test]
async fn the_429_and_503_stubs_set_the_gap_series_on_both_lanes() {
    let ms = |values: &[u64]| {
        values
            .iter()
            .map(|v| Duration::from_millis(*v))
            .collect::<Vec<_>>()
    };
    let mut answers = vec![ResponseTemplate::new(429); 7];
    answers.push(ResponseTemplate::new(200).set_body_string(OK));
    let server = series(answers).await;
    let settings = settings_with("");
    let client = client_on(&server, &settings, quick(), Some(OCTO_KEY));
    let mut gaps = Vec::new();
    for _ in 0..8 {
        client.lookup_answer(LookupClass::Heart, "AQAD", 200).await;
        gaps.push(client.http.limiter().gap(KeyLane::BuiltIn, LookupClass::Heart));
    }
    assert_eq!(gaps, ms(&[20, 40, 80, 160, 320, 640, 1280, 640]));

    let mut answers = vec![ResponseTemplate::new(503); 7];
    answers.push(ResponseTemplate::new(200).set_body_string(OK));
    let server = series(answers).await;
    let client = client_on(&server, &settings, quick(), Some(OCTO_KEY));
    let mut gaps = Vec::new();
    for _ in 0..8 {
        client.lookup_answer(LookupClass::Heart, "AQAD", 200).await;
        gaps.push(client.http.limiter().gap(KeyLane::BuiltIn, LookupClass::Heart));
    }
    // The person outage cap scaled with the 10 ms floor: 64 s for 2 s.
    assert_eq!(gaps, ms(&[20, 40, 80, 160, 320, 320, 320, 160]));

    let server = series(vec![
        ResponseTemplate::new(503),
        ResponseTemplate::new(429),
        ResponseTemplate::new(200).set_body_string(OK),
    ])
    .await;
    let own = settings_with("user-key");
    let client = client_on(&server, &own, quick(), Some(OCTO_KEY));
    let floor = AcoustIdRateLimiter::USER_GAP;
    let mut gaps = Vec::new();
    for _ in 0..3 {
        client.lookup_answer(LookupClass::Heart, "AQAD", 200).await;
        gaps.push(client.http.limiter().gap(KeyLane::User, LookupClass::Heart));
    }
    assert_eq!(gaps, [floor * 2, floor * 4, floor * 2]);
}

/// The turn comes before the timer: six heart lookups queued at once on a long gap all get their
/// answers, though the last waits longer than the timeout.
#[tokio::test]
async fn queued_lookups_on_a_long_gap_never_time_out_waiting() {
    let server = answering(OK).await;
    let settings = settings_with("");
    let limiter = AcoustIdRateLimiter::with_gaps(Duration::from_millis(600), Duration::from_secs(20))
        .with_jitter(|| 1.0);
    let client = Arc::new(client_on(&server, &settings, limiter, Some(OCTO_KEY)));
    let start = Instant::now();
    let lookups = (0..6).map(|_| {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.lookup_answer(LookupClass::Heart, "AQAD", 200).await })
    });
    for lookup in futures::future::join_all(lookups).await {
        let outcome = lookup.expect("the lookup ran");
        assert!(matches!(outcome, AcoustIdOutcome::Answered(_)), "{outcome:?}");
    }
    assert!(start.elapsed() >= Duration::from_secs(3), "{:?}", start.elapsed());
}

/// A turn that is not free at once is None at once when the caller cannot wait, with nothing sent.
#[tokio::test]
async fn a_lookup_only_if_free_sends_nothing_when_no_turn_is_free() {
    let server = answering(OK).await;
    let settings = settings_with("");
    let limiter = AcoustIdRateLimiter::with_gaps(Duration::from_secs(5), Duration::from_secs(20));
    let client = client_on(&server, &settings, limiter, Some(OCTO_KEY));
    let first = client.lookup_answer_if_free(LookupClass::Bulk, "AQAD", 200).await;
    assert!(matches!(first, Some(AcoustIdOutcome::Answered(_))), "{first:?}");
    let start = Instant::now();
    assert_eq!(
        client.lookup_answer_if_free(LookupClass::Bulk, "AQAD", 200).await,
        None
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(server.received_requests().await.expect("recorded").len(), 1);
}

/// An idle install's held lookups resume on their own once the stub recovers: each held lookup
/// is sent again at the lane's gap, a busy answer backs it off, and the answer that finally comes
/// wakes the next held lookup.
#[tokio::test]
async fn an_idle_installs_held_lookups_resume_when_acoustid_recovers() {
    let server = series(vec![
        ResponseTemplate::new(503),
        ResponseTemplate::new(503),
        ResponseTemplate::new(503),
        ResponseTemplate::new(200).set_body_string(OK),
    ])
    .await;
    let settings = settings_with("");
    let client = Arc::new(client_on(&server, &settings, quick(), Some(OCTO_KEY)));
    let (done, mut answered) = tokio::sync::mpsc::unbounded_channel::<u32>();

    fn ask(client: Arc<AcoustIdClient>, n: u32, done: tokio::sync::mpsc::UnboundedSender<u32>) {
        tokio::spawn(async move {
            match client.lookup_answer(LookupClass::Heart, "AQAD", 200).await {
                AcoustIdOutcome::Busy => {
                    let again = Arc::clone(&client);
                    client.hold_lookup(Box::new(move || ask(again, n, done)));
                }
                _ => {
                    let _ = done.send(n);
                }
            }
        });
    }
    ask(Arc::clone(&client), 1, done.clone());
    // The second waits held from the start, behind the first.
    let (second, second_done) = (Arc::clone(&client), done.clone());
    client.hold_lookup(Box::new(move || ask(second, 2, second_done)));
    let mut finished = Vec::new();
    for _ in 0..2 {
        let n = tokio::time::timeout(Duration::from_secs(5), answered.recv())
            .await
            .expect("answered in time")
            .expect("an answer");
        finished.push(n);
    }
    finished.sort();
    assert_eq!(finished, [1, 2]);
    assert_eq!(server.received_requests().await.expect("recorded").len(), 5);
}

/// Code 6 on a submission sets the user's submission key aside: submissions pause, with nothing
/// sent, until the settings change.
#[tokio::test]
async fn a_refused_submission_key_pauses_submissions_until_the_settings_change() {
    let server = MockServer::start().await;
    Mock::given(path("/v2/submit"))
        .respond_with(ResponseTemplate::new(200).set_body_string(error(6)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/v2/submit"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(&server)
        .await;
    let settings = settings_with("user-key");
    let client = client_on(&server, &settings, quick(), Some(OCTO_KEY));
    let items = [AcoustIdSubmission {
        fingerprint: "AQAD".into(),
        duration_seconds: 200,
        recording_id: "mbid".into(),
        file_format: None,
    }];
    assert!(!client.submit("user-key", "person", &items, 10).await);
    assert!(client.submission_key_refused());
    assert!(!client.submit("user-key", "person", &items, 10).await);
    assert_eq!(server.received_requests().await.expect("recorded").len(), 1);
    assert!(client.is_on(), "lookups go on");
    settings.set((*settings.current()).clone());
    assert!(!client.submission_key_refused());
    assert!(client.submit("user-key", "person", &items, 10).await);
}
