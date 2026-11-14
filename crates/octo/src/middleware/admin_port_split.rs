//! The admin port split: with `Admin:Port` set, the dashboard and the admin API answer
//! only on that port and everything else only on Octo's main port. A proxy or a port-forward can
//! then expose the main port for music apps while the admin port stays on this machine or the
//! home network.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use super::admin_guard::is_admin_path;
use super::admin_headers::is_dashboard_path;

/// The port a request came in on, put on each request by the listener that took it. A request
/// without one (a test calling the app directly) came in on the main port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPort(pub u16);

/// Whether a path is the logo under `/Assets` or the pages' font under `/fonts`, which both
/// ports serve: the dashboard and the Family pages draw with them.
fn is_shared_path(path: &str) -> bool {
    ["/Assets", "/fonts"].iter().any(|prefix| {
        path.len() >= prefix.len()
            && path[..prefix.len()].eq_ignore_ascii_case(prefix)
            && (path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/')
    })
}

/// The answer when the request is on the wrong port for its path: a redirect to the dashboard,
/// or a bare 404. None when it may go on.
pub fn refuse(path: &str, local_port: Option<u16>, admin_port: u16) -> Option<Response> {
    let admin = is_dashboard_path(path) || is_admin_path(path);
    let on_admin_port = local_port == Some(admin_port);
    if on_admin_port && (path.is_empty() || path == "/") {
        let mut res = Response::new(Body::empty());
        *res.status_mut() = StatusCode::FOUND;
        res.headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static("/admin/index.html"));
        return Some(res);
    }
    let allowed = if on_admin_port {
        admin || is_shared_path(path)
    } else {
        !admin
    };
    if allowed {
        return None;
    }
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::NOT_FOUND;
    Some(res)
}

/// The layer, over the admin port the process listens on (0: no split, every request goes on).
pub async fn admin_port_split(State(admin_port): State<u16>, req: Request, next: Next) -> Response {
    if admin_port == 0 {
        return next.run(req).await;
    }
    let local = req.extensions().get::<LocalPort>().map(|port| port.0);
    match refuse(req.uri().path(), local, admin_port) {
        Some(res) => res,
        None => next.run(req).await,
    }
}

/// The addresses to listen on when the dashboard gets a port of its own: the ones Octo already
/// uses (ASPNETCORE_URLS as `urls`, else ASPNETCORE_HTTP_PORTS as `http_ports`, else
/// `http://localhost:5000`) plus the admin port. None when the admin port is off or is already a
/// main port.
pub fn listen_urls(urls: Option<&str>, http_ports: Option<&str>, admin_port: i32) -> Option<String> {
    if admin_port <= 0 {
        return None;
    }
    let mut urls = urls.unwrap_or("").to_string();
    if urls.trim().is_empty()
        && let Some(ports) = http_ports.filter(|p| !p.trim().is_empty())
    {
        urls = ports
            .split(';')
            .map(str::trim)
            .filter(|port| !port.is_empty())
            .map(|port| format!("http://+:{port}"))
            .collect::<Vec<_>>()
            .join(";");
    }
    if urls.trim().is_empty() {
        urls = "http://localhost:5000".to_string();
    }
    let suffix = format!(":{admin_port}");
    if urls
        .split(';')
        .any(|url| url.trim().trim_end_matches('/').ends_with(&suffix))
    {
        return None;
    }
    Some(format!("{urls};http://+:{admin_port}"))
}

/// The admin port this process splits on: `Admin:Port` when [`listen_urls`] adds it, otherwise
/// 0 (off, or already one of Octo's own ports, which then keeps sharing).
pub fn effective_admin_port(urls: Option<&str>, http_ports: Option<&str>, configured: i32) -> u16 {
    match listen_urls(urls, http_ports, configured) {
        Some(_) => u16::try_from(configured).unwrap_or(0),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_admin_port_serves_only_the_dashboard() {
        let cases: [(u16, &str, u16); 12] = [
            (8080, "/admin/index.html", 404),
            (8080, "/api/admin/status", 404),
            (8080, "/rest/ping", 0),
            (5275, "/rest/ping", 404),
            (5275, "/", 302),
            (5275, "/api/admin/status", 0),
            (5275, "/admin/index.html", 0),
            (5275, "/Assets/octo_logo.png", 0),
            (5275, "/fonts/inter.css", 0),
            (8080, "/fonts/InterVariable.woff2", 0),
            (5275, "/fontsx/inter.css", 404),
            (5275, "/family/index.html", 404),
        ];
        for (local_port, path, answered) in cases {
            let refused = refuse(path, Some(local_port), 5275);
            assert_eq!(refused.is_some(), answered != 0, "{local_port} {path}");
            if let Some(res) = refused {
                assert_eq!(res.status().as_u16(), answered, "{local_port} {path}");
                if answered == 302 {
                    assert_eq!(res.headers()[header::LOCATION], "/admin/index.html");
                }
            }
        }
    }

    #[test]
    fn the_admin_port_is_added_to_octos_own_addresses() {
        let cases: [(Option<&str>, Option<&str>, i32, Option<&str>); 6] = [
            (
                Some("http://+:8080"),
                None,
                5275,
                Some("http://+:8080;http://+:5275"),
            ),
            (None, Some("8080"), 5275, Some("http://+:8080;http://+:5275")),
            (None, None, 5275, Some("http://localhost:5000;http://+:5275")),
            (
                Some("https://localhost:7248;http://localhost:5274"),
                None,
                5275,
                Some("https://localhost:7248;http://localhost:5274;http://+:5275"),
            ),
            (Some("http://+:8080"), None, 8080, None),
            (Some("http://+:8080"), None, 0, None),
        ];
        for (urls, ports, admin_port, expected) in cases {
            assert_eq!(
                listen_urls(urls, ports, admin_port).as_deref(),
                expected,
                "{urls:?} {ports:?}"
            );
        }
        assert_eq!(effective_admin_port(Some("http://+:8080"), None, 8081), 8081);
        assert_eq!(effective_admin_port(Some("http://+:8080"), None, 8080), 0);
        assert_eq!(effective_admin_port(None, None, 0), 0);
    }
}
