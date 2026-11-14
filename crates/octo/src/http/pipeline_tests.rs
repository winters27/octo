//! The assembled pipeline: the headers, statuses and bodies Octo's clients and dashboard
//! depend on, static files included.

use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use http_body_util::BodyExt;
use octo_core::settings::AppSettings;
use tower::ServiceExt;

use super::*;
use crate::http::error::{AppError, json_ok};
use crate::http::static_files::{StaticAsset, StaticRoots};

const CSS: &str = "body { color: #123456; margin: 0 auto; }\n";

/// A fixed modification time, so Last-Modified is known.
fn modified() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000)
}

fn assets() -> StaticAssets {
    let mut a = StaticAssets::default();
    a.push(StaticAsset::new(
        "/admin/index.html".into(),
        "text/html",
        b"<!doctype html><p>hi</p>".to_vec(),
        modified(),
    ));
    a.push(StaticAsset::new(
        "/admin/admin.css".into(),
        "text/css",
        CSS.repeat(100).into_bytes(),
        modified(),
    ));
    a.push(StaticAsset::new(
        "/Assets/octo_logo.png".into(),
        "image/png",
        vec![0x89, b'P', b'N', b'G', 1, 2, 3],
        modified(),
    ));
    // As the host does once it is listening.
    a.warm_all();
    a
}

fn boom() -> &'static str {
    panic!("handler blew up")
}

/// Routes standing in for controllers (`testPing` for ping, which the controller now answers).
fn dummy_routes() -> RouteSet {
    RouteSet::new()
        .subsonic("testPing", get(|| async { "pong" }).post(|| async { "pong" }))
        .route("/test/panic", get(|| async { boom() }))
        .route(
            "/test/not-configured",
            get(|| async {
                AppError::NotConfigured("Set SUBSONIC_URL: an absolute URL, isn't it".into()).into_response()
            }),
        )
        .route(
            "/api/admin/test-dummy",
            get(|| async { json_ok(&serde_json::json!({"ok": true})) })
                .post(|| async { json_ok(&serde_json::json!({"saved": true})) }),
        )
}

fn app() -> App {
    let a = assets();
    build_with(
        AppState::for_tests(AppSettings::default()),
        app_routes(&a).merge(dummy_routes()),
    )
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn has_cors(&self) -> bool {
        self.headers
            .keys()
            .any(|k| k.as_str().starts_with("access-control-"))
    }
}

async fn send_to(app: &App, method: Method, uri: &str, headers: &[(&str, &str)]) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("infallible");
    let (parts, body) = res.into_parts();
    let body = body.collect().await.expect("body").to_bytes().to_vec();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body,
    }
}

async fn send(method: Method, uri: &str, headers: &[(&str, &str)]) -> Reply {
    send_to(&app(), method, uri, headers).await
}

async fn get_(uri: &str, headers: &[(&str, &str)]) -> Reply {
    send(Method::GET, uri, headers).await
}

fn assert_problem_404(r: &Reply, what: &str) {
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{what}");
    assert_eq!(
        r.header("content-type"),
        Some("application/problem+json; charset=utf-8"),
        "{what}"
    );
    let body = r.text();
    assert!(
        body.starts_with(
            r#"{"type":"https://tools.ietf.org/html/rfc9110#section-15.5.5","title":"Not Found","status":404,"traceId":"00-"#
        ),
        "{what}: {body}"
    );
}

// ---- CORS ----

#[tokio::test]
async fn cors_a_request_with_origin_gets_allow_origin_and_the_exposed_headers() {
    let r = get_("/rest/testPing", &[("Origin", "http://player.example")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    assert_eq!(
        r.header("access-control-expose-headers"),
        Some("X-Content-Duration,X-Total-Count,X-Nd-Authorization")
    );
    assert!(r.header("vary").is_none(), "no Vary");

    let r = get_("/rest/testPing", &[]).await;
    assert!(!r.has_cors(), "no Origin, no CORS headers");
}

#[tokio::test]
async fn cors_headers_are_added_to_errors_and_static_files_too() {
    let origin = [("Origin", "http://x")];
    for uri in [
        "/favicon.ico",
        "/admin/admin.css",
        "/admin",
        "/test/not-configured",
    ] {
        let r = get_(uri, &origin).await;
        assert_eq!(r.header("access-control-allow-origin"), Some("*"), "{uri}");
    }
}

#[tokio::test]
async fn cors_preflight_echoes_the_method_and_headers_with_204() {
    let r = send(
        Method::OPTIONS,
        "/admin/admin.css",
        &[
            ("Origin", "http://x"),
            ("Access-Control-Request-Method", "PUT"),
            ("Access-Control-Request-Headers", "X-Foo, Content-Type"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        r.header("access-control-allow-headers"),
        Some("X-Foo,Content-Type")
    );
    assert_eq!(r.header("access-control-allow-methods"), Some("PUT"));
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    assert!(r.header("content-type").is_none());
    assert!(r.header("access-control-expose-headers").is_none());
    assert!(r.body.is_empty());

    let r = send(
        Method::OPTIONS,
        "/rest/testPing",
        &[("Origin", "http://x"), ("Access-Control-Request-Method", "GET")],
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(
        r.header("access-control-allow-headers").is_none(),
        "nothing asked, nothing allowed"
    );
    assert_eq!(r.header("access-control-allow-methods"), Some("GET"));
}

#[tokio::test]
async fn an_options_request_that_is_not_a_preflight_reaches_the_catch_all() {
    // No Access-Control-Request-Method.
    let r = send(Method::OPTIONS, "/admin/admin.css", &[("Origin", "http://x")]).await;
    assert_problem_404(&r, "OPTIONS static");
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    // No Origin.
    let r = send(
        Method::OPTIONS,
        "/admin/admin.css",
        &[("Access-Control-Request-Method", "GET")],
    )
    .await;
    assert_problem_404(&r, "OPTIONS without Origin");
    assert!(!r.has_cors());
}

// ---- Admin guard ----

#[tokio::test]
async fn admin_guard_strips_cors_from_admin_reads() {
    let r = get_("/api/admin/test-dummy", &[("Origin", "http://evil.example")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text(), r#"{"ok":true}"#);
    assert!(!r.has_cors());
}

#[tokio::test]
async fn admin_guard_answers_options_with_a_bare_204() {
    let r = send(
        Method::OPTIONS,
        "/api/admin/test-dummy",
        &[
            ("Origin", "http://evil.example"),
            ("Access-Control-Request-Method", "POST"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(!r.has_cors());
    assert!(r.body.is_empty());
}

#[tokio::test]
async fn admin_guard_refuses_writes_without_the_header() {
    let r = send(
        Method::POST,
        "/api/admin/test-dummy",
        &[("Origin", "http://evil.example")],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.header("content-type"), Some("application/json; charset=utf-8"));
    assert_eq!(
        r.text(),
        r#"{"error":"Admin changes must come from Octo's dashboard. A script can send the X-Octo-Admin header to opt in."}"#
    );
    assert!(!r.has_cors());

    let r = send(Method::POST, "/API/Admin/Test-Dummy", &[("X-Octo-Admin", "1")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text(), r#"{"saved":true}"#);

    let r = send(
        Method::DELETE,
        "/api/admin/nope",
        &[("X-Octo-Admin", "1"), ("Origin", "http://x")],
    )
    .await;
    assert_problem_404(&r, "unknown admin route");
    assert!(!r.has_cors());
}

#[tokio::test]
async fn head_on_an_admin_get_route_reaches_the_catch_all() {
    let r = send(Method::HEAD, "/api/admin/test-dummy", &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

// ---- Static files ----

#[tokio::test]
async fn static_files_carry_their_type_cache_and_etag_headers() {
    let a = assets();
    let css = a.get("/admin/admin.css").expect("css");
    let r = get_("/admin/admin.css", &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), Some("text/css"));
    assert_eq!(
        r.header("content-length"),
        Some(CSS.len() * 100).map(|n| n.to_string()).as_deref()
    );
    assert_eq!(r.all("accept-ranges"), vec!["bytes"]);
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    assert_eq!(r.all("etag"), vec![css.etag()]);
    assert_eq!(r.header("last-modified"), Some("Mon, 21 Sep 2026 14:13:20 GMT"));
    assert!(r.header("vary").is_none(), "the identity variant has no Vary");
    assert!(r.header("content-encoding").is_none());
    assert_eq!(r.body, CSS.repeat(100).into_bytes());

    for (uri, ty) in [
        ("/admin/index.html", "text/html"),
        ("/Assets/octo_logo.png", "image/png"),
    ] {
        let r = get_(uri, &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}");
        assert_eq!(r.header("content-type"), Some(ty), "{uri}");
    }
}

#[tokio::test]
async fn static_etag_is_the_base64_sha256_of_the_body() {
    use base64::Engine as _;
    use sha2::Digest as _;
    let r = get_("/admin/admin.css", &[]).await;
    let expected = format!(
        "\"{}\"",
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&r.body))
    );
    assert_eq!(r.header("etag"), Some(expected.as_str()));
}

#[tokio::test]
async fn static_if_none_match_gives_304_with_the_same_headers_and_no_body() {
    let a = assets();
    let etag = a.get("/admin/admin.css").expect("css").etag().to_string();
    for inm in [
        etag.clone(),
        format!("W/{etag}"),
        format!("\"zzz\", {etag}"),
        "*".to_string(),
    ] {
        let r = get_("/admin/admin.css", &[("If-None-Match", &inm)]).await;
        assert_eq!(r.status, StatusCode::NOT_MODIFIED, "{inm}");
        assert!(r.body.is_empty());
        assert!(
            r.header("content-length").is_none(),
            "{inm}: no Content-Length on a 304"
        );
        assert_eq!(r.header("content-type"), Some("text/css"));
        assert_eq!(r.header("cache-control"), Some("no-cache"));
        assert_eq!(r.all("etag"), vec![etag.as_str()]);
        assert!(r.header("last-modified").is_some());
    }
    // A non-matching If-None-Match wins over a matching If-Modified-Since.
    let r = get_(
        "/admin/admin.css",
        &[
            ("If-None-Match", "\"zzz\""),
            ("If-Modified-Since", "Mon, 21 Sep 2026 14:13:20 GMT"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn static_if_modified_since_compares_dates_and_ignores_the_future() {
    let cases = [
        ("Mon, 21 Sep 2026 14:13:20 GMT", StatusCode::NOT_MODIFIED),
        ("Mon, 21 Sep 2026 14:00:00 GMT", StatusCode::OK),
        ("Fri, 01 Jan 2100 00:00:00 GMT", StatusCode::OK),
        ("not a date", StatusCode::OK),
    ];
    for (ims, expected) in cases {
        let r = get_("/admin/admin.css", &[("If-Modified-Since", ims)]).await;
        assert_eq!(r.status, expected, "{ims}");
    }
}

#[tokio::test]
async fn static_failed_preconditions_give_a_bare_412() {
    for (name, value) in [
        ("If-Match", "\"nope\""),
        ("If-Unmodified-Since", "Sun, 04 Oct 2020 06:00:00 GMT"),
    ] {
        let r = get_("/admin/admin.css", &[(name, value)]).await;
        assert_eq!(r.status, StatusCode::PRECONDITION_FAILED, "{name}");
        assert_eq!(r.header("content-length"), Some("0"));
        assert!(r.header("content-type").is_none());
        assert!(r.header("etag").is_none());
    }
    let etag = assets().get("/admin/admin.css").expect("css").etag().to_string();
    let r = get_("/admin/admin.css", &[("If-Match", &etag)]).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn static_range_gives_206_with_content_range() {
    let len = CSS.len() * 100;
    let r = get_("/admin/admin.css", &[("Range", "bytes=0-9")]).await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        r.header("content-range"),
        Some(format!("bytes 0-9/{len}").as_str())
    );
    assert_eq!(r.header("content-length"), Some("10"));
    assert_eq!(r.body, CSS.as_bytes()[..10].to_vec());
    assert_eq!(r.header("content-type"), Some("text/css"));
    assert!(r.header("etag").is_some());

    let r = get_("/admin/admin.css", &[("Range", "bytes=-5")]).await;
    assert_eq!(
        r.header("content-range"),
        Some(format!("bytes {}-{}/{len}", len - 5, len - 1).as_str())
    );

    // Several ranges are not supported: the whole file.
    let r = get_("/admin/admin.css", &[("Range", "bytes=0-1,5-6")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body.len(), len);

    // Past the end: 416 with only Content-Range.
    let r = get_("/admin/admin.css", &[("Range", "bytes=999999-")]).await;
    assert_eq!(r.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.header("content-range"), Some(format!("bytes */{len}").as_str()));
    assert_eq!(r.header("content-length"), Some("0"));
    assert!(r.header("content-type").is_none());

    // If-Range with another ETag: the whole file.
    let r = get_(
        "/admin/admin.css",
        &[("Range", "bytes=0-9"), ("If-Range", "\"nope\"")],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let etag = assets().get("/admin/admin.css").expect("css").etag().to_string();
    let r = get_("/admin/admin.css", &[("Range", "bytes=0-9"), ("If-Range", &etag)]).await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
}

#[tokio::test]
async fn static_brotli_and_gzip_variants_follow_accept_encoding() {
    let a = assets();
    let css = a.get("/admin/admin.css").expect("css");
    let br_etag = css
        .compressed_etag(crate::http::static_files::Encoding::Br)
        .expect("br");
    let gz_etag = css
        .compressed_etag(crate::http::static_files::Encoding::Gzip)
        .expect("gzip");
    let weak = format!("W/{}", css.etag());

    let r = get_("/admin/admin.css", &[("Accept-Encoding", "gzip, br")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-encoding"), Some("br"));
    assert_eq!(r.header("vary"), Some("Content-Encoding"));
    assert_eq!(r.all("etag"), vec![br_etag, weak.as_str()]);
    assert_eq!(
        r.header("content-length"),
        Some(r.body.len().to_string().as_str())
    );
    assert!(r.body.len() < CSS.len() * 100);

    let cases = [
        ("gzip", Some(gz_etag)),
        ("br;q=0, gzip", Some(gz_etag)),
        ("gzip;q=0.5, br;q=0.4", Some(gz_etag)),
        ("gzip;q=1, br;q=1", Some(br_etag)),
        ("BR", Some(br_etag)),
        ("*", None),
        ("identity", None),
        ("deflate", None),
    ];
    for (ae, expected) in cases {
        let r = get_("/admin/admin.css", &[("Accept-Encoding", ae)]).await;
        assert_eq!(
            r.headers
                .get_all("etag")
                .iter()
                .next()
                .and_then(|v| v.to_str().ok()),
            Some(expected.unwrap_or(css.etag())),
            "{ae}"
        );
        assert_eq!(r.header("vary").is_some(), expected.is_some(), "{ae}");
    }

    // The precompressed variant's own ETag is the one If-None-Match is checked against.
    let r = get_(
        "/admin/admin.css",
        &[("Accept-Encoding", "br"), ("If-None-Match", br_etag)],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    assert_eq!(r.header("content-encoding"), Some("br"));
    let r = get_(
        "/admin/admin.css",
        &[("Accept-Encoding", "br"), ("If-None-Match", css.etag())],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);

    // A range applies to the compressed body.
    let r = get_(
        "/admin/admin.css",
        &[("Accept-Encoding", "br"), ("Range", "bytes=0-9")],
    )
    .await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.header("content-encoding"), Some("br"));
    assert!(
        r.header("content-range")
            .expect("range")
            .starts_with("bytes 0-9/")
    );

    // Images are not precompressed.
    let r = get_("/Assets/octo_logo.png", &[("Accept-Encoding", "gzip, br")]).await;
    assert!(r.header("content-encoding").is_none());
    assert!(r.header("vary").is_none());
}

#[tokio::test]
async fn static_head_has_the_headers_and_no_body() {
    let r = send(Method::HEAD, "/admin/admin.css", &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.is_empty());
    assert_eq!(r.header("content-type"), Some("text/css"));
    assert!(r.header("etag").is_some());
    assert!(
        r.header("content-length").is_none(),
        "ASP.NET sent no Content-Length on HEAD"
    );
}

#[tokio::test]
async fn static_paths_match_ignoring_case_and_a_trailing_slash() {
    for uri in [
        "/ADMIN/Admin.CSS",
        "/admin/admin.css/",
        "/assets/OCTO_LOGO.png",
        "/admin/admin.css?v=3",
    ] {
        let r = get_(uri, &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn static_files_answer_only_get_and_head() {
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let r = send(method.clone(), "/admin/admin.css", &[]).await;
        assert_problem_404(&r, &format!("{method} static"));
    }
}

#[tokio::test]
async fn the_real_admin_ui_is_served_with_content_hash_etags() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../octo");
    let roots = StaticRoots::resolve(None, None, None, Some(&repo));
    let assets = StaticAssets::load(&roots);
    assets.warm_all();
    for url in [
        "/admin/index.html",
        "/admin/admin.css",
        "/admin/admin.js",
        "/admin/icons.svg",
        "/Assets/octo_logo.png",
        "/fonts/inter.css",
        "/fonts/InterVariable.woff2",
        "/fonts/InterVariable-Italic.woff2",
        "/fonts/OFL.txt",
    ] {
        assert!(assets.get(url).is_some(), "{url} missing");
    }
    // The ETag is the quoted base64 SHA-256 of the file. The dashboard files change with every
    // release, so their ETags are worked out from the files; the logo does not, so its ETag
    // stays pinned.
    let c_sharp_etag = |file: &str| {
        use base64::Engine as _;
        use sha2::Digest as _;
        let bytes = std::fs::read(repo.join(file)).expect("the file reads");
        let hash = sha2::Sha256::digest(&bytes);
        format!("\"{}\"", base64::engine::general_purpose::STANDARD.encode(hash))
    };
    for (url, etag) in [
        ("/admin/index.html", c_sharp_etag("wwwroot/admin/index.html")),
        ("/admin/admin.css", c_sharp_etag("wwwroot/admin/admin.css")),
        (
            "/Assets/octo_logo.png",
            "\"Qptg3+qU6TXlwlGbzo6M68djiy8XJUt94wGNPAXBs14=\"".to_string(),
        ),
    ] {
        assert_eq!(assets.get(url).map(|a| a.etag()), Some(etag.as_str()), "{url}");
    }
    let app = build(AppState::for_tests(AppSettings::default()), &assets, 0);
    for (url, ty) in [
        ("/admin/index.html", "text/html"),
        ("/admin/admin.js", "text/javascript"),
        ("/admin/icons.svg", "image/svg+xml"),
        ("/Assets/octo_logo.png", "image/png"),
        ("/fonts/inter.css", "text/css"),
        ("/fonts/InterVariable.woff2", "font/woff2"),
        ("/fonts/InterVariable-Italic.woff2", "font/woff2"),
    ] {
        let r = send_to(&app, Method::GET, url, &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{url}");
        assert_eq!(r.header("content-type"), Some(ty), "{url}");
    }
    let r = send_to(
        &app,
        Method::GET,
        "/admin/icons.svg",
        &[("Accept-Encoding", "br")],
    )
    .await;
    assert_eq!(r.header("content-encoding"), Some("br"));
}

/// Every place in a web file that pulls in a stylesheet, a font or a script from another host:
/// a `<link>` or `<script>` tag's `href` or `src`, a CSS `url(...)` or `@import` (in a
/// stylesheet, a page's own style or a script that sets one), and a JavaScript `import`.
fn outside_loads(text: &str, script: bool) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let is_outside = |rest: &str| {
        let rest = rest.trim_start().trim_start_matches(['"', '\'']).trim_start();
        rest.starts_with("http://") || rest.starts_with("https://") || rest.starts_with("//")
    };
    let mut found = Vec::new();
    for tag in ["<link", "<script"] {
        let mut from = 0;
        while let Some(at) = lower[from..].find(tag) {
            let start = from + at;
            let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
            let element = &lower[start..end];
            if ["href=", "src="].iter().any(|attr| {
                element
                    .find(attr)
                    .is_some_and(|a| is_outside(&element[a + attr.len()..]))
            }) {
                found.push(text[start..end].to_string());
            }
            from = end;
        }
    }
    let mut markers = vec!["url(", "@import"];
    if script {
        markers.extend(["from ", "import(", "importscripts("]);
    }
    for marker in markers {
        for (at, _) in lower.match_indices(marker) {
            let rest = &lower[at + marker.len()..];
            if is_outside(rest) {
                let end = rest.find(['\n', ';', ')']).unwrap_or(rest.len());
                found.push(text[at..at + marker.len() + end].to_string());
            }
        }
    }
    found
}

#[test]
fn the_web_pages_load_nothing_from_another_host() {
    // The check itself finds each kind of outside load.
    for (sample, script) in [
        (
            r#"<link rel="stylesheet" href="https://rsms.me/inter/inter.css" />"#,
            false,
        ),
        (r#"<link rel="preconnect" href="//rsms.me" />"#, false),
        (r#"<script defer src='http://cdn.example/x.js'></script>"#, false),
        (
            r#"@font-face { src: url( "https://x.example/a.woff2" ) format('woff2'); }"#,
            false,
        ),
        (r#"@import "https://x.example/a.css";"#, false),
        (r#"import { a } from "https://x.example/a.js";"#, true),
        (r#"const m = await import('https://x.example/m.js');"#, true),
    ] {
        assert_eq!(outside_loads(sample, script).len(), 1, "{sample}");
    }
    for (sample, script) in [
        (r#"<link rel="stylesheet" href="/fonts/inter.css" />"#, false),
        (
            r#"<a href="https://www.last.fm/api/account/create">key</a>"#,
            false,
        ),
        (r#"<input placeholder="https://music.example.com" />"#, false),
        (r#"src: url('InterVariable.woff2') format('woff2');"#, false),
        (
            r#"Free from <a href="https://acoustid.org/">acoustid.org</a>."#,
            false,
        ),
        (r#"import test from 'node:test';"#, true),
    ] {
        assert!(outside_loads(sample, script).is_empty(), "{sample}");
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../octo/wwwroot");
    let mut dirs = vec![root.clone()];
    let mut checked = 0;
    let mut found = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("the web root reads") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !["html", "htm", "css", "js", "mjs"].contains(&ext.as_str()) {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a web file reads as text");
            checked += 1;
            for load in outside_loads(&text, ext == "js" || ext == "mjs") {
                found.push(format!("{}: {load}", path.display()));
            }
        }
    }
    assert!(
        checked >= 10,
        "only {checked} web files were found under {}",
        root.display()
    );
    assert!(
        found.is_empty(),
        "loaded from another host:\n{}",
        found.join("\n")
    );
}

// ---- /admin, /, the catch-all ----

#[tokio::test]
async fn admin_redirects_to_the_dashboard_page() {
    for uri in ["/admin", "/admin/", "/ADMIN/"] {
        let r = get_(uri, &[]).await;
        assert_eq!(r.status, StatusCode::FOUND, "{uri}");
        assert_eq!(r.header("location"), Some("/admin/index.html"));
        assert_eq!(r.header("content-length"), Some("0"));
        assert!(r.body.is_empty());
    }
    for method in [Method::HEAD, Method::POST] {
        let r = send(method.clone(), "/admin", &[]).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{method} /admin");
    }
}

/// #80: the bare address sends to Navidrome's web app, relative, so a
/// reverse proxy's path prefix is kept. Amperfy checks the address before signing in.
#[tokio::test]
async fn root_redirects_to_the_web_app() {
    for uri in ["/", "/?u=admin&p=secret&v=1.16.1&c=Amperfy&f=json"] {
        let r = get_(uri, &[]).await;
        assert_eq!(r.status, StatusCode::FOUND, "{uri}");
        assert_eq!(r.header("location"), Some("app/"), "{uri}");
        assert!(r.body.is_empty(), "{uri}");
    }
    let r = send(Method::HEAD, "/", &[]).await;
    assert_eq!(r.status, StatusCode::FOUND);
    assert_eq!(r.header("location"), Some("app/"));
}

#[tokio::test]
async fn octo_owned_paths_are_problem_404s() {
    for uri in [
        "/favicon.ico",
        "/admin/nope.js",
        "/administrator",
        "/api/admin/unknown",
        "/assets/nope.png",
        "/Assets/nope.png",
    ] {
        assert_problem_404(&get_(uri, &[]).await, uri);
    }
}

/// With no Navidrome URL configured, a relayed call answers 200 with the Subsonic error envelope
/// naming the missing setting, in XML unless `f=json`.
#[tokio::test]
async fn everything_else_is_relayed_and_without_a_url_says_so() {
    let r = get_("/rest/getArtists.view?u=a", &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), Some("application/xml"));
    assert_eq!(
        r.text(),
        "<subsonic-response status=\"failed\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n  \
         <error code=\"0\" message=\"Error connecting to Subsonic server: Octo has no valid Navidrome URL. \
         Set SUBSONIC_URL (Subsonic__Url) to your Navidrome server, e.g. http://192.168.1.10:4533. It must be an absolute \
         URL the Octo container can reach, not localhost.\" />\n</subsonic-response>"
    );
    let r = get_("/rest/getArtists.view?u=a&f=json", &[]).await;
    assert_eq!(r.header("content-type"), Some("application/json; charset=utf-8"));
    assert_eq!(
        r.text(),
        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":0,"message":"Error connecting to Subsonic server: Octo has no valid Navidrome URL. Set SUBSONIC_URL (Subsonic__Url) to your Navidrome server, e.g. http://192.168.1.10:4533. It must be an absolute URL the Octo container can reach, not localhost."}}}"#
    );
}

#[tokio::test]
async fn paths_are_canonicalised_before_routing() {
    for uri in ["/REST/TestPing.VIEW", "/rest/testping/", "/Rest/TESTPING?f=json"] {
        let r = get_(uri, &[]).await;
        assert_eq!((r.status, r.text().as_str()), (StatusCode::OK, "pong"), "{uri}");
    }
    // A method the route does not take reaches the catch-all, never a 405; so does HEAD.
    // (No Navidrome URL here, so the relay answers its 200 error envelope.)
    for method in [Method::PUT, Method::DELETE, Method::HEAD] {
        let r = send(method.clone(), "/rest/testPing", &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{method}");
        if method != Method::HEAD {
            assert!(
                r.text().contains("Error connecting to Subsonic server"),
                "{method}"
            );
        }
    }
}

// ---- Errors ----

#[tokio::test]
async fn a_panicking_handler_becomes_the_500_envelope() {
    let r = get_("/test/panic", &[("Origin", "http://x")]).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.header("content-type"), Some("application/json; charset=utf-8"));
    assert_eq!(
        r.text(),
        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":0,"message":"An internal server error occurred"}}}"#
    );
    assert_eq!(r.header("cache-control"), Some("no-cache,no-store"));
    assert_eq!(r.header("expires"), Some("-1"));
    assert_eq!(r.header("pragma"), Some("no-cache"));
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("*"),
        "CORS still applies"
    );
}

#[tokio::test]
async fn exception_envelopes_use_the_relaxed_encoder() {
    let r = get_("/test/not-configured", &[]).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        r.text(),
        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":0,"message":"Set SUBSONIC_URL: an absolute URL, isn't it"}}}"#
    );
}

fn outside_song(title: &str) -> octo_core::soulseek::SoulseekRouting {
    octo_core::soulseek::SoulseekRouting {
        kind: octo_core::soulseek::RoutingKind::Song,
        artist: Some("Artist".into()),
        title: Some(title.into()),
        ..Default::default()
    }
}

/// A route standing in for an answer that lists `rows` outside songs, none pinned or sized.
fn listing_route(rows: usize) -> axum::routing::MethodRouter<AppState> {
    get(
        move |axum::extract::State(state): axum::extract::State<AppState>| async move {
            use octo_subsonic::subsonic_response_builder::IdRegistry;
            for i in 0..rows {
                let id = state
                    .external_id_registry
                    .register(outside_song(&format!("{rows} {i}")));
                state.stream_size_warmer.outside_listed(&id);
                state.stream_size_warmer.size_wanted(&id);
            }
            "listed"
        },
    )
}

/// Each request is one listing for the stream size warmer: a short answer may have its songs
/// searched for, a long one never.
#[tokio::test]
async fn each_answer_is_one_listing_for_the_stream_size_warmer() {
    let state = AppState::for_tests(AppSettings::default());
    let app = build_with(
        state.clone(),
        RouteSet::new()
            .route("/test/few", listing_route(3))
            .route("/test/many", listing_route(60)),
    );
    assert_eq!(
        send_to(&app, Method::GET, "/test/few", &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(
        send_to(&app, Method::GET, "/test/many", &[]).await.status,
        StatusCode::OK
    );

    let queued = |title: &str| {
        let id = state.external_id_registry.register(outside_song(title));
        // Queued already when it cannot be queued again.
        !state.stream_size_warmer.want(&id, true)
    };
    assert!(
        queued("3 0") && queued("3 2"),
        "a short answer's songs are searched for"
    );
    assert!(
        !queued("60 0") && !queued("60 59"),
        "a long answer's songs are not"
    );
}

// ---- Family ----

/// Octo Family refuses a `/rest` endpoint it has no class for to capped and kid members:
/// every `/rest` route Octo answers itself must have one.
#[test]
fn every_rest_route_has_a_family_class() {
    let names: Vec<String> = app_routes(&StaticAssets::default())
        .templates()
        .into_iter()
        .filter_map(|t| octo_core::family::endpoints::rest_endpoint_name(&t).map(str::to_string))
        .collect();
    assert!(names.len() > 40, "{names:?}");
    let missing: Vec<&String> = names
        .iter()
        .filter(|name| octo_core::family::endpoints::rest_class(name).is_none())
        .collect();
    assert!(missing.is_empty(), "no family class: {missing:?}");
}
