//! The Deezer HTTP client and its rate limiting, as one type: the only way to reach Deezer is
//! through [`DeezerRateLimitHandler::send`] (or [`get`](DeezerRateLimitHandler::get), which adds
//! the callers' 8 s timeout over the permit's wait and the call, or
//! [`get_after_permit`](DeezerRateLimitHandler::get_after_permit), whose 8 s start once the
//! permit is held), which waits on the limiter first.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use reqwest::StatusCode;
use reqwest::header::ACCEPT_LANGUAGE;
use tracing::warn;

use super::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::client_builder;
use crate::services::http_client_factory::timeout_message;

/// Spends a [`DeezerRateLimiter`] permit before each Deezer API call. The limiter is the
/// singleton; this is the seam that consults it.
pub struct DeezerRateLimitHandler {
    client: reqwest::Client,
    limiter: Arc<DeezerRateLimiter>,
    api_host: String,
    /// How long a call may take: [`Self::TIMEOUT`], shorter in tests.
    timeout: Duration,
}

impl DeezerRateLimitHandler {
    pub const API_HOST: &'static str = "api.deezer.com";

    /// The callers' timeout, 8 seconds. With [`Self::get`] it covers waiting for a permit as well
    /// as the call and reading the answer; with [`Self::get_after_permit`] only the call and
    /// reading the answer.
    pub const TIMEOUT: Duration = Duration::from_secs(8);

    pub fn new(limiter: Arc<DeezerRateLimiter>) -> Self {
        Self::with_api_host(limiter, Self::API_HOST)
    }

    /// A handler that meters another host as the API: a test's mock server.
    pub fn with_api_host(limiter: Arc<DeezerRateLimiter>, api_host: impl Into<String>) -> Self {
        let client = client_builder().build().expect("the Deezer HTTP client builds");
        Self {
            client,
            limiter,
            api_host: api_host.into(),
            timeout: Self::TIMEOUT,
        }
    }

    /// The same handler with another timeout, so a test never waits out 8 seconds.
    #[cfg(test)]
    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn limiter(&self) -> &Arc<DeezerRateLimiter> {
        &self.limiter
    }

    /// Sends a request. `background` marks it as background work, so cache warming yields to
    /// anything a user is actually waiting on.
    pub async fn send(
        &self,
        request: reqwest::Request,
        background: bool,
    ) -> reqwest::Result<reqwest::Response> {
        if !self.permit(&request, background).await {
            return Ok(too_many_requests());
        }
        self.client.execute(request).await
    }

    /// A GET with the given Accept-Language, the body read, and all of it (the permit included)
    /// inside [`Self::TIMEOUT`].
    pub async fn get(
        &self,
        url: &str,
        background: bool,
        accept_language: Option<&str>,
    ) -> anyhow::Result<HttpAnswer> {
        let request = self.request(url, accept_language)?;
        let call = async {
            let response = self.send(request, background).await?;
            HttpAnswer::read(response).await
        };
        match tokio::time::timeout(self.timeout, call).await {
            Ok(answer) => Ok(answer?),
            Err(_) => Err(anyhow!(timeout_message(self.timeout))),
        }
    }

    /// [`Self::get`] for a lookup whose busy queue must never read as no answer: it waits for its
    /// permit first, as long as the limiter's queue takes, and only then starts [`Self::TIMEOUT`]
    /// for the call and reading the answer. A full queue still answers 429 at once.
    pub async fn get_after_permit(
        &self,
        url: &str,
        background: bool,
        accept_language: Option<&str>,
    ) -> anyhow::Result<HttpAnswer> {
        let request = self.request(url, accept_language)?;
        if !self.permit(&request, background).await {
            return Ok(HttpAnswer::empty(StatusCode::TOO_MANY_REQUESTS));
        }
        let call = async {
            let response = self.client.execute(request).await?;
            HttpAnswer::read(response).await
        };
        match tokio::time::timeout(self.timeout, call).await {
            Ok(answer) => Ok(answer?),
            Err(_) => Err(anyhow!(timeout_message(self.timeout))),
        }
    }

    /// A GET of `url` with the given Accept-Language.
    fn request(&self, url: &str, accept_language: Option<&str>) -> reqwest::Result<reqwest::Request> {
        let mut builder = self.client.get(url);
        if let Some(language) = accept_language {
            builder = builder.header(ACCEPT_LANGUAGE, language);
        }
        builder.build()
    }

    /// Whether `request` may go: at once for a host that is not the API, else once a permit is
    /// held, however long the limiter's queue takes. False when the queue is full.
    async fn permit(&self, request: &reqwest::Request, background: bool) -> bool {
        // Only the API is metered. The cover-art lookup pulls actual image bytes from
        // cdn-images.dzcdn.net through this same client, and that host has no quota:
        // metering it would spend an API permit per rendered row and throttle a CDN for
        // nothing.
        let metered = request
            .url()
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(&self.api_host));
        if !metered {
            return true;
        }

        let lease = self.limiter.acquire(background).await;
        if !lease.is_acquired() {
            // The queue is full. Answering 429 rather than throwing means callers take
            // their existing "Deezer refused this" path, which never caches the result:
            // so back-pressure can slow us down but can never poison the cache.
            let lane = if background { "background" } else { "interactive" };
            warn!(
                "deezer rate limiter rejected a {lane} request to {}",
                request.url()
            );
            return false;
        }
        true
    }
}

fn too_many_requests() -> reqwest::Response {
    let response = http::Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .body(Vec::<u8>::new())
        .expect("a bodiless 429 builds");
    reqwest::Response::from(response)
}

#[cfg(test)]
#[path = "deezer_rate_limit_handler_tests.rs"]
mod tests;
