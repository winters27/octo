//! The AcoustID rate limit handler, together with the "acoustid" client it holds (base address
//! https://api.acoustid.org/, a 10 s timeout, gzip and deflate decompression).
//!
//! As with Deezer, the client and the handler are one type, so there is no way to reach
//! AcoustID without spending a permit.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use tracing::{debug, warn};
use url::Url;

use super::acoust_id_rate_limiter::{AcoustIdRateLimiter, CallPermit};
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::resolve;

/// Spends an [`AcoustIdRateLimiter`] permit before each AcoustID call. The limiter is the
/// singleton; this is just the seam that consults it. A form is posted only for a call that holds
/// its turn ([`CallPermit`]).
pub struct AcoustIdRateLimitHandler {
    client: reqwest::Client,
    limiter: Arc<AcoustIdRateLimiter>,
    base: Url,
}

impl AcoustIdRateLimitHandler {
    pub const BASE_ADDRESS: &'static str = "https://api.acoustid.org/";

    /// Verification sits between a finished transfer and the file joining the library.
    /// A slow AcoustID must cost seconds, never the download. It covers the HTTP call alone, never
    /// the wait for a turn, so a lane backed off to a long gap never times its queue out.
    pub const TIMEOUT: Duration = Duration::from_secs(10);

    pub fn new(limiter: Arc<AcoustIdRateLimiter>) -> Self {
        Self::with_base_url(
            limiter,
            Url::parse(Self::BASE_ADDRESS).expect("the AcoustID base address parses"),
        )
    }

    /// A client for another base address (a test's mock server). Its host is the one metered.
    pub fn with_base_url(limiter: Arc<AcoustIdRateLimiter>, base: Url) -> Self {
        // meta=...+compress asks AcoustID to gzip the body. Without decompression it arrives
        // compressed, fails to parse, and reads as "no match", silently accepting everything,
        // which is the worst outcome this feature can have. (The workspace reqwest has gzip and
        // deflate, and they are on unless turned off.)
        let client = reqwest::Client::builder()
            .gzip(true)
            .deflate(true)
            .build()
            .expect("the AcoustID HTTP client builds");
        Self {
            client,
            limiter,
            base,
        }
    }

    pub fn limiter(&self) -> &Arc<AcoustIdRateLimiter> {
        &self.limiter
    }

    /// Sends a request, spending a permit first when it goes to AcoustID.
    pub async fn send(&self, request: reqwest::Request) -> reqwest::Result<reqwest::Response> {
        let metered = match (request.url().host_str(), self.base.host_str()) {
            (Some(host), Some(api)) => host.eq_ignore_ascii_case(api),
            _ => false,
        };
        if !metered {
            return self.client.execute(request).await;
        }

        let lease = self.limiter.acquire().await;
        if !lease.is_acquired() {
            // Answering 429 rather than throwing means the caller takes its ordinary
            // "AcoustID had no verdict" path, which keeps the file and remembers nothing.
            // Back-pressure can make verification less effective; it must never make it
            // reject a good download.
            // The background lane is refused by design while a download waits; that is not news.
            if AcoustIdRateLimiter::in_background_now() {
                debug!("acoustid background request deferred: {}", request.url());
            } else {
                warn!("acoustid rate limiter rejected a request to {}", request.url());
            }
            let response = http::Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Vec::<u8>::new())
                .expect("a bodiless 429 builds");
            return Ok(reqwest::Response::from(response));
        }

        self.client.execute(request).await
    }

    /// Posts a URL-encoded form for a call that holds its turn, and reads the body, all of it
    /// inside [`Self::TIMEOUT`], which starts only now.
    pub async fn post_form(
        &self,
        _turn: &CallPermit,
        relative: &str,
        body: String,
    ) -> anyhow::Result<HttpAnswer> {
        let url = resolve(&self.base, relative)?;
        let request = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .build()?;
        let call = async {
            let response = self.client.execute(request).await?;
            HttpAnswer::read(response).await
        };
        match tokio::time::timeout(Self::TIMEOUT, call).await {
            Ok(answer) => Ok(answer?),
            Err(_) => Err(anyhow!(
                "No answer came within 10 seconds, so Octo stopped waiting."
            )),
        }
    }
}
