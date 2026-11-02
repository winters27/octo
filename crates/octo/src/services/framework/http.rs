//! A buffered HTTP call: the whole body is read before the call returns, inside the client's
//! timeout. [`HttpAnswer`] is that buffered response, and the helpers here read it as text or
//! JSON, and read a `Retry-After`.

use std::time::Duration;

use bytes::Bytes;
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde_json::Value;

/// A response with its body read.
#[derive(Debug, Clone)]
pub struct HttpAnswer {
    pub status: StatusCode,
    pub body: Bytes,
    /// The response's `Retry-After`, when it gave one in seconds.
    pub retry_after: Option<Duration>,
}

impl HttpAnswer {
    /// Reads the whole body of a response.
    pub async fn read(response: reqwest::Response) -> reqwest::Result<HttpAnswer> {
        let status = response.status();
        let retry_after = retry_after_delta(response.headers());
        let body = response.bytes().await?;
        Ok(HttpAnswer {
            status,
            body,
            retry_after,
        })
    }

    /// An answer the client made up itself, with no body (the limiters' 429).
    pub fn empty(status: StatusCode) -> HttpAnswer {
        HttpAnswer {
            status,
            body: Bytes::new(),
            retry_after: None,
        }
    }

    /// An answer made up from a JSON document (an answer from another catalog in this one's shape).
    pub fn from_json(status: StatusCode, document: &serde_json::Value) -> HttpAnswer {
        HttpAnswer {
            status,
            body: Bytes::from(serde_json::to_vec(document).unwrap_or_default()),
            retry_after: None,
        }
    }

    /// A success status: 200-299.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// The body as UTF-8 text, a byte order mark dropped, bad bytes replaced.
    pub fn text(&self) -> String {
        let body = self.body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&self.body);
        String::from_utf8_lossy(body).into_owned()
    }

    /// The body parsed as JSON.
    pub fn json(&self) -> serde_json::Result<Value> {
        parse_json(&self.body)
    }
}

/// JSON parsed from bytes: a UTF-8 byte order mark is skipped.
pub fn parse_json(bytes: &[u8]) -> serde_json::Result<Value> {
    serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes))
}

/// A client builder with no automatic decompression (so no Accept-Encoding header); only the
/// AcoustID client turns it on.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().no_gzip().no_deflate()
}

/// A relative request resolved against a client's base address.
pub fn resolve(base: &url::Url, relative: &str) -> Result<url::Url, url::ParseError> {
    base.join(relative)
}

/// A `Retry-After` given in seconds. A date, or nothing readable, is None, and the caller's
/// default cool-down applies.
pub fn retry_after_delta(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<i32>()
        .ok()
        .map(|seconds| Duration::from_secs(seconds as u64))
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    #[test]
    fn retry_after_is_read_only_in_seconds() {
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after_delta(&headers), None);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("300"));
        assert_eq!(retry_after_delta(&headers), Some(Duration::from_secs(300)));
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(retry_after_delta(&headers), None);
    }
}
