//! The Subsonic relay, with Navidrome stood in for by wiremock.

use super::*;
use http_body_util::BodyExt;
use octo_core::settings::{AppSettings, SubsonicSettings};
use wiremock::matchers::{header as header_is, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn settings_for(url: &str) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(url.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }))
}

fn service(url: &str) -> SubsonicProxyService {
    SubsonicProxyService::new(settings_for(url))
}

/// A request-scoped service, as for a client request with these headers and no body.
fn scoped(url: &str, headers: &[(&str, &str)]) -> SubsonicProxyService {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        map.append(
            axum::http::HeaderName::from_bytes(k.as_bytes()).expect("header name"),
            HeaderValue::from_str(v).expect("header value"),
        );
    }
    service(url).with_request(Arc::new(IncomingRequest::new(
        Method::GET,
        map,
        None,
        Bytes::new(),
    )))
}

fn params(pairs: &[(&str, &str)]) -> Parameters {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn received(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.expect("recording is on")
}

async fn body_of(response: Response) -> (StatusCode, HeaderMap, Bytes) {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.expect("body").to_bytes();
    (status, headers, body)
}

#[tokio::test]
async fn relay_successful_request_returns_body_and_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1u8, 2, 3, 4, 5], "application/json"))
        .mount(&server)
        .await;

    let result = service(&server.uri())
        .relay(
            "rest/ping",
            &params(&[("u", "admin"), ("p", "password"), ("v", "1.16.0")]),
        )
        .await
        .expect("relayed");

    assert_eq!(result.body.as_ref(), [1, 2, 3, 4, 5]);
    assert_eq!(result.content_type.as_deref(), Some("application/json"));
}

#[tokio::test]
async fn relay_builds_correct_url() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    service(&server.uri())
        .relay("rest/ping", &params(&[("u", "admin"), ("p", "secret")]))
        .await
        .expect("relayed");

    // The mock server saw it, so the host and port were the configured ones.
    let requests = received(&server).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/rest/ping");
    assert_eq!(requests[0].url.query(), Some("u=admin&p=secret"));
}

/// Regression for the admin UI applying nothing until a restart. These services are
/// singletons, so capturing the settings in the constructor froze them at startup while the
/// admin UI, reading the live settings, happily showed the new value as if it had taken effect.
#[tokio::test]
async fn relay_picks_up_a_settings_change_without_being_rebuilt() {
    let first = MockServer::builder().start().await;
    let moved = MockServer::builder().start().await;
    for server in [&first, &moved] {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }
    let settings = settings_for(&first.uri());
    let service = SubsonicProxyService::new(Arc::clone(&settings));

    // Stand in for settings.json being rewritten by the admin UI and reloaded.
    settings.set(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(moved.uri()),
            ..Default::default()
        },
        ..Default::default()
    });

    service
        .relay("rest/ping", &params(&[("u", "admin")]))
        .await
        .expect("relayed");

    // The host is the point: the relay followed the new value with no rebuild.
    assert_eq!(received(&moved).await.len(), 1);
    assert!(received(&first).await.is_empty());
}

#[tokio::test]
async fn relay_encodes_special_characters() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    service(&server.uri())
        .relay(
            "rest/search3",
            &params(&[("query", "rock & roll"), ("artist", "AC/DC")]),
        )
        .await
        .expect("relayed");

    let url = received(&server).await[0].url.clone();
    // Every reserved character is percent-encoded: a space is %20, & is %26 and / is %2F.
    assert_eq!(url.path(), "/rest/search3");
    assert_eq!(url.query(), Some("query=rock%20%26%20roll&artist=AC%2FDC"));
}

#[tokio::test]
async fn relay_http_error_throws_exception() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let error = service(&server.uri())
        .relay("rest/ping", &params(&[("u", "admin")]))
        .await
        .expect_err("a 404 is an HttpRequestException");

    assert_eq!(
        error,
        RelayError::Http("The server answered 404 (Not Found).".into())
    );
}

#[tokio::test]
async fn relay_safe_successful_request_returns_success_true() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1u8, 2, 3], "application/xml"))
        .mount(&server)
        .await;

    let result = service(&server.uri())
        .relay_safe("rest/ping", &params(&[("u", "admin")]))
        .await
        .expect("success");

    assert_eq!(result.body.as_ref(), [1, 2, 3]);
    assert_eq!(result.content_type.as_deref(), Some("application/xml"));
}

#[tokio::test]
async fn relay_safe_http_error_returns_success_false() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    assert!(
        service(&server.uri())
            .relay_safe("rest/ping", &params(&[("u", "admin")]))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn relay_safe_network_exception_returns_success_false() {
    // Nothing listens on port 1.
    assert!(
        service("http://127.0.0.1:1")
            .relay_safe("rest/ping", &params(&[("u", "admin")]))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn relay_stream_successful_request_returns_file_stream_result() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/stream"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1u8, 2, 3, 4, 5], "audio/mpeg"))
        .mount(&server)
        .await;

    let response = scoped(&server.uri(), &[])
        .relay_stream(&params(&[("id", "song123"), ("u", "admin")]))
        .await;

    let (status, headers, body) = body_of(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "audio/mpeg");
    assert_eq!(headers[header::CONTENT_LENGTH], "5");
    assert_eq!(body.as_ref(), [1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn relay_stream_http_error_returns_status_code_result() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let response = scoped(&server.uri(), &[])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    // A bare 404 goes out as a problem details body.
    let (status, headers, body) = body_of(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/problem+json; charset=utf-8"
    );
    assert!(String::from_utf8_lossy(&body).contains("\"status\":404"));
}

#[tokio::test]
async fn relay_stream_exception_returns_object_result_with_500() {
    let address = crate::services::test_support::refused_address();
    let response = scoped(&format!("http://{address}"), &[])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    let (status, _, body) = body_of(response).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        String::from_utf8_lossy(&body),
        format!(
            r#"{{"error":"Error streaming from Subsonic: Could not connect to {address}: the connection was refused."}}"#
        )
    );
}

#[tokio::test]
async fn relay_stream_default_content_type_uses_audio_mpeg() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
        .mount(&server)
        .await;

    let response = scoped(&server.uri(), &[])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    assert_eq!(response.headers()[header::CONTENT_TYPE], "audio/mpeg");
}

#[tokio::test]
async fn relay_stream_with_range_header_forwards_range_to_upstream() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(header_is("Range", "bytes=0-1023"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 0-4/10")
                .insert_header("Accept-Ranges", "bytes")
                .set_body_raw(vec![1u8, 2, 3, 4, 5], "audio/mpeg"),
        )
        .mount(&server)
        .await;

    let response = scoped(&server.uri(), &[("Range", "bytes=0-1023")])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    let (status, headers, _) = body_of(response).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["Content-Range"], "bytes 0-4/10");
    assert_eq!(headers["Accept-Ranges"], "bytes");
    assert_eq!(received(&server).await[0].headers["range"], "bytes=0-1023");
}

#[tokio::test]
async fn relay_stream_with_if_range_header_forwards_if_range_to_upstream() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
        .mount(&server)
        .await;

    scoped(&server.uri(), &[("If-Range", "\"etag123\"")])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    assert_eq!(received(&server).await[0].headers["if-range"], "\"etag123\"");
}

#[tokio::test]
async fn relay_stream_null_http_context_returns_error() {
    let response = service("http://localhost:4533")
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    let (status, _, body) = body_of(response).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        String::from_utf8_lossy(&body),
        r#"{"error":"HTTP context not available"}"#
    );
}

// The relay's rules for URLs, parameters and bodies.

#[tokio::test]
async fn a_missing_or_relative_url_is_not_configured() {
    for url in ["", "   ", "navidrome", "http//navidrome:4533"] {
        let error = service(url)
            .relay("rest/ping", &params(&[]))
            .await
            .expect_err("not configured");
        assert_eq!(
            error,
            RelayError::NotConfigured(NOT_CONFIGURED_MESSAGE.into()),
            "{url:?}"
        );
    }
    // An absolute URI, but not something the HTTP client can send to.
    let error = service("localhost:4533")
        .relay("rest/ping", &params(&[]))
        .await
        .expect_err("a scheme");
    assert_eq!(
        error,
        RelayError::NotSupported("The 'localhost' scheme is not supported.".into())
    );
    let error = service("/music")
        .relay("rest/ping", &params(&[]))
        .await
        .expect_err("a file path");
    assert_eq!(error, RelayError::InvalidOperation(INVALID_REQUEST_URI.into()));
}

#[test]
fn repeated_parameters_go_upstream_as_the_client_sent_them() {
    let query = parser::parse_query("id=A&id=B&u=x&f=json&songId=1&songId=2");
    let parameters = params(&[
        ("id", "A,B"),
        ("u", "x"),
        ("f", "xml"),
        ("songId", "1,2,3"),
        ("c", "Octo"),
    ]);

    let restored = restore_repeated_parameters(&parameters, Some(&query), None, false);

    let expected: Vec<(String, String)> = [
        ("id", "A"),
        ("id", "B"),
        ("u", "x"),
        // Changed by a handler: sent once, as it now reads.
        ("f", "xml"),
        ("songId", "1,2,3"),
        // Added by a handler.
        ("c", "Octo"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(restored, expected);
}

#[test]
fn empty_repeats_are_restored_too() {
    // "id=&id=B&id=" reads "B", which is unchanged, so every value goes back, empty ones included.
    let query = parser::parse_query("id=&id=B&id=");
    let restored = restore_repeated_parameters(&params(&[("id", "B")]), Some(&query), None, false);
    let ids: Vec<&str> = restored.iter().map(|(_, v)| v.as_str()).collect();
    assert_eq!(ids, ["", "B", ""]);
}

#[test]
fn unchanged_form_fields_stay_in_a_forwarded_body() {
    let form = parser::parse_query("id=A&id=B&u=x");
    let query = parser::parse_query("f=json");
    let parameters = params(&[("f", "json"), ("id", "A,B"), ("u", "y")]);

    let in_body = restore_repeated_parameters(&parameters, Some(&query), Some(&form), true);
    assert_eq!(
        in_body,
        [
            ("f".to_string(), "json".to_string()),
            ("u".to_string(), "y".to_string())
        ]
    );

    let not_in_body = restore_repeated_parameters(&parameters, Some(&query), Some(&form), false);
    let keys: Vec<&str> = not_in_body.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["f", "id", "id", "u"]);
}

#[tokio::test]
async fn the_faithful_relay_keeps_method_body_status_and_allowlisted_headers() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("X-Nd-Authorization", "token.two")
                .insert_header("X-Not-Forwarded", "x")
                .insert_header("Vary", "Origin, Accept-Encoding")
                .insert_header("Cache-Control", "max-age=60, public")
                .insert_header("Last-Modified", "Sun, 4 Oct 2026 08:07:56 GMT")
                .set_body_raw(
                    r#"{"error":"Invalid username or password"}"#,
                    "application/json;charset=UTF-8",
                ),
        )
        .mount(&server)
        .await;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("X-ND-Client-Unique-Id", HeaderValue::from_static("abc"));
    headers.insert("X-Other", HeaderValue::from_static("dropped"));
    headers.append(header::ACCEPT, HeaderValue::from_static("a/b"));
    headers.append(header::ACCEPT, HeaderValue::from_static("c/d"));
    let body = Bytes::from_static(br#"{"username":"admin","password":"nope"}"#);
    let incoming = Arc::new(IncomingRequest::new(
        Method::POST,
        headers,
        Some("x=1".into()),
        body.clone(),
    ));
    let relay = service(&server.uri()).with_request(Arc::clone(&incoming));

    let raw = relay
        .relay_raw("auth/login", &incoming.parameters())
        .await
        .expect("relayed");

    assert_eq!(raw.status, 401);
    assert_eq!(raw.body.as_ref(), br#"{"error":"Invalid username or password"}"#);
    assert_eq!(
        raw.content_type.as_deref(),
        Some("application/json; charset=UTF-8")
    );
    let pairs: Vec<(&str, &str)> = raw
        .response_headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("X-Nd-Authorization", "token.two"),
            ("Last-Modified", "Sun, 04 Oct 2026 08:07:56 GMT"),
            ("Cache-Control", "public, max-age=60"),
            ("Vary", "Origin"),
            ("Vary", "Accept-Encoding"),
        ]
    );

    let sent = &received(&server).await[0];
    assert_eq!(sent.body, body.to_vec());
    // A JSON body's members are parameters, and only a url-encoded body keeps them out of the
    // query, so they go upstream twice.
    assert_eq!(sent.url.query(), Some("x=1&username=admin&password=nope"));
    assert_eq!(sent.headers["content-type"], "application/json");
    assert_eq!(sent.headers["x-nd-client-unique-id"], "abc");
    assert_eq!(sent.headers["accept"], "a/b, c/d");
    assert!(!sent.headers.contains_key("x-other"));
    assert!(!sent.headers.contains_key("accept-encoding"));
}

#[tokio::test]
async fn a_forwarded_form_body_is_not_repeated_in_the_query() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    let body = Bytes::from_static(b"u=admin&p=pw&id=A&id=B");
    let incoming = Arc::new(IncomingRequest::new(
        Method::POST,
        headers,
        Some("f=json".into()),
        body.clone(),
    ));

    service(&server.uri())
        .with_request(Arc::clone(&incoming))
        .relay_raw("rest/getGenres.view", &incoming.parameters())
        .await
        .expect("relayed");

    let sent = &received(&server).await[0];
    assert_eq!(sent.url.path(), "/rest/getGenres.view");
    assert_eq!(sent.url.query(), Some("f=json"));
    assert_eq!(sent.body, body.to_vec());
}

#[test]
fn content_types_are_rewritten_as_media_type_header_value_wrote_them() {
    for (raw, expected) in [
        ("text/plain; charset=utf-8", Some("text/plain; charset=utf-8")),
        ("application/json", Some("application/json")),
        ("text/xml;charset=UTF-8", Some("text/xml; charset=UTF-8")),
        (
            "application/json ; charset = \"utf-8\"",
            Some("application/json; charset=\"utf-8\""),
        ),
        ("bad type", None),
        ("text/html; foo", Some("text/html; foo")),
    ] {
        assert_eq!(normalize_media_type(raw).as_deref(), expected, "{raw}");
    }
}

#[test]
fn cache_control_is_merged_into_one_ordered_line_or_none_when_invalid() {
    let lines = ["max-age=60, public".to_string(), "no-cache".to_string()];
    assert_eq!(
        normalize_cache_control(&lines).as_deref(),
        Some("public, no-cache, max-age=60")
    );
    let lines = ["public, max-age=315360000".to_string()];
    assert_eq!(
        normalize_cache_control(&lines).as_deref(),
        Some("public, max-age=315360000")
    );
    let lines = ["immutable, max-age=x".to_string()];
    assert_eq!(normalize_cache_control(&lines), None);
}

// ---- Repeated parameters restored --------------------------------------------------------

const REPEAT_A: &str = "3vXkQ9mTz2LbW8rYcN1pDf";
const REPEAT_B: &str = "7HqRs4uVw0XyZaBcDeFgHi";

fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn restore_a_value_a_handler_changed_goes_out_as_it_is() {
    let query = parser::parse_query(&format!("id={REPEAT_A}&id={REPEAT_B}&size=50"));
    let parameters = params(&[("id", "rewritten"), ("size", "50"), ("extra", "1")]);

    let sent = restore_repeated_parameters(&parameters, Some(&query), None, false);

    assert_eq!(
        sent,
        pairs(&[("id", "rewritten"), ("size", "50"), ("extra", "1")])
    );
}

#[test]
fn restore_a_single_value_with_a_comma_stays_one_value() {
    let query = parser::parse_query("query=daft%20punk%2C%20justice");
    let parameters = params(&[("query", "daft punk, justice")]);

    let sent = restore_repeated_parameters(&parameters, Some(&query), None, false);

    assert_eq!(sent, pairs(&[("query", "daft punk, justice")]));
}

#[test]
fn restore_a_changed_form_field_still_reaches_the_query_when_the_body_is_forwarded() {
    let form = parser::parse_query(&format!("id={REPEAT_A}&id={REPEAT_B}&u=alice"));
    let parameters = params(&[("id", &format!("{REPEAT_A},{REPEAT_B}")), ("u", "octo")]);

    let sent = restore_repeated_parameters(&parameters, None, Some(&form), true);

    // The unchanged ids ride in the body only; a changed value is never dropped.
    assert_eq!(sent, pairs(&[("u", "octo")]));
}

// ---- A transcode's estimated length ------------------------------------------------------

/// Navidrome as a socket answering every connection with one canned answer and closing it, so a
/// body shorter than its announced Content-Length fails in the HTTP client exactly as it does
/// against the real one.
async fn canned_upstream(status: &str, headers: &[&str], body: Vec<u8>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    let head = format!(
        "HTTP/1.1 {status}\r\n{}Connection: close\r\n\r\n",
        headers.iter().map(|h| format!("{h}\r\n")).collect::<String>()
    );
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (head, body) = (head.clone(), body.clone());
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut one = [0u8; 1];
                while stream.read(&mut one).await.unwrap_or(0) == 1 {
                    seen.push(one[0]);
                    if seen.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
                let _ = stream.flush().await;
            });
        }
    });
    url
}

fn estimated_transcode() -> Parameters {
    params(&[
        ("id", "song123"),
        ("format", "mp3"),
        ("maxBitRate", "192"),
        ("estimateContentLength", "true"),
    ])
}

fn ninety_seven() -> Vec<u8> {
    (0..97u8).collect()
}

#[tokio::test]
async fn relay_stream_async_estimated_transcode_leaves_the_length_out_and_ends_at_the_real_end() {
    let url = canned_upstream(
        "200 OK",
        &[
            "Content-Type: audio/mpeg",
            "Accept-Ranges: none",
            "Content-Length: 100",
        ],
        ninety_seven(),
    )
    .await;

    let response = scoped(&url, &[]).relay_stream(&estimated_transcode()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    assert_eq!(response.headers()["accept-ranges"], "none");
    let (_, _, body) = body_of(response).await;
    assert_eq!(body.as_ref(), ninety_seven().as_slice());
}

/// The tolerant stream stays at its end once it has ended short.
#[tokio::test]
async fn an_estimated_body_stays_ended_after_it_ends_short() {
    let url = canned_upstream(
        "200 OK",
        &[
            "Content-Type: audio/mpeg",
            "Accept-Ranges: none",
            "Content-Length: 100",
        ],
        ninety_seven(),
    )
    .await;
    let response = reqwest::get(&url).await.expect("an answer");

    let mut upstream = Box::pin(tolerate_early_end(response, "rest/stream"));
    let mut read = Vec::new();
    while let Some(chunk) = upstream.next().await {
        read.extend_from_slice(&chunk.expect("no failure"));
    }

    assert_eq!(read, ninety_seven());
    assert!(upstream.next().await.is_none());
}

#[tokio::test]
async fn relay_stream_async_cached_transcode_keeps_its_exact_length() {
    // Navidrome serves a transcode it has finished from its cache, with ranges and the real
    // size, even when the client asked for an estimate.
    let url = canned_upstream(
        "200 OK",
        &[
            "Content-Type: audio/mpeg",
            "Accept-Ranges: bytes",
            "Content-Length: 3",
        ],
        vec![1, 2, 3],
    )
    .await;

    let response = scoped(&url, &[]).relay_stream(&estimated_transcode()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-length"], "3");
}

#[tokio::test]
async fn relay_stream_async_raw_stream_that_ends_early_is_not_passed_off_as_whole() {
    // An exact length that is not met is a real failure, and must not read as a clean end.
    let url = canned_upstream(
        "200 OK",
        &[
            "Content-Type: audio/mpeg",
            "Accept-Ranges: bytes",
            "Content-Length: 6",
        ],
        vec![1, 2, 3],
    )
    .await;

    let response = scoped(&url, &[])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    assert_eq!(response.headers()["content-length"], "6");
    assert!(response.into_body().collect().await.is_err());
}

#[tokio::test]
async fn relay_stream_async_range_not_satisfiable_passes_the_real_size_on() {
    let url = canned_upstream(
        "416 Requested Range Not Satisfiable",
        &["Content-Range: bytes */3974093", "Content-Length: 0"],
        Vec::new(),
    )
    .await;

    let response = scoped(&url, &[]).relay_stream(&estimated_transcode()).await;

    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()["content-range"], "bytes */3974093");
}

#[tokio::test]
async fn relay_raw_async_download_of_an_estimated_transcode_keeps_every_byte() {
    let url = canned_upstream(
        "200 OK",
        &[
            "Content-Type: audio/mpeg",
            "Accept-Ranges: none",
            "Content-Length: 100",
        ],
        ninety_seven(),
    )
    .await;

    let result = scoped(&url, &[])
        .relay_raw("rest/download", &estimated_transcode())
        .await
        .expect("relayed");

    assert_eq!(result.status, 200);
    assert_eq!(result.body.as_ref(), ninety_seven().as_slice());
}

/// An exact length the body falls short of stays a failure in the general relay too.
#[tokio::test]
async fn relay_raw_async_a_raw_body_that_ends_early_is_a_failure() {
    let url = canned_upstream(
        "200 OK",
        &[
            "Content-Type: audio/mpeg",
            "Accept-Ranges: bytes",
            "Content-Length: 6",
        ],
        vec![1, 2, 3],
    )
    .await;

    let result = scoped(&url, &[])
        .relay_raw("rest/download", &params(&[("id", "song123")]))
        .await;

    assert!(matches!(result, Err(RelayError::Http(_))), "{result:?}");
}

#[test]
fn length_may_be_estimate_follows_navidrome_then_the_request() {
    let cases: [(u16, Option<&'static str>, &str, bool); 11] = [
        // Navidrome's own marks win: a live transcode says none, a file or cached transcode bytes.
        (200, Some("none"), "", true),
        (
            200,
            Some("bytes"),
            "format=mp3&maxBitRate=192&estimateContentLength=true",
            false,
        ),
        (
            200,
            Some("none"),
            "format=mp3&maxBitRate=192&estimateContentLength=true",
            true,
        ),
        // A 206 carries an exact Content-Range.
        (206, None, "format=mp3&estimateContentLength=true", false),
        // With no mark, the request decides.
        (200, None, "estimateContentLength=true", true),
        (200, None, "format=mp3", true),
        (200, None, "maxBitRate=128", true),
        (200, None, "format=raw", false),
        (200, None, "maxBitRate=0", false),
        (200, None, "estimateContentLength=false", false),
        (200, None, "", false),
    ];
    for (status, accept_ranges, query, expected) in cases {
        let mut headers = HeaderMap::new();
        if let Some(value) = accept_ranges {
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static(value));
        }
        let parameters: Vec<(String, String)> = query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (k, v) = pair.split_once('=').expect("a pair");
                (k.to_string(), v.to_string())
            })
            .collect();

        assert_eq!(
            length_may_be_estimate(
                StatusCode::from_u16(status).expect("a status"),
                &headers,
                &parameters
            ),
            expected,
            "{status} {accept_ranges:?} {query}"
        );
    }
}

#[tokio::test]
async fn relay_events_passes_each_event_on_while_the_stream_stays_open() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = vec![0u8; 4096];
        let read = socket.read(&mut request).await.expect("read");
        let _ = seen_tx.send(String::from_utf8_lossy(&request[..read]).to_string());
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
                  Transfer-Encoding: chunked\r\n\r\nd\r\ndata: hello\n\n\r\n",
            )
            .await
            .expect("write");
        std::future::pending::<()>().await;
    });

    let response = scoped(
        &format!("http://{address}"),
        &[("X-Nd-Authorization", "Bearer token")],
    )
    .relay_events("api/events", &params(&[("jwt", "token")]))
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    let request = seen_rx.await.expect("the request reached Navidrome");
    assert!(request.starts_with("GET /api/events?jwt=token "), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-nd-authorization: bearer token"),
        "{request}"
    );

    let mut body = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
        .await
        .expect("the first event arrives before the stream ends")
        .expect("a frame")
        .expect("no error");
    assert_eq!(frame.into_data().expect("data").as_ref(), b"data: hello\n\n");
}
