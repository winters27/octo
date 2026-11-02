//! The Deezer HTTP client and its rate limiting, as one type: the only way to reach Deezer's API is
//! through [`get_for`](DeezerRateLimitHandler::get_for) (one 8 s budget over the permit's wait and
//! the call) or [`get_after_permit_for`](DeezerRateLimitHandler::get_after_permit_for) (its 8 s
//! start once the permit is held), which wait on the limiter first. [`get`] and
//! [`get_after_permit`] are the same for a caller that only knows its lane.
//!
//! When a [`DeezerStandIn`] is wired, every request to the API host is routed by it first: a
//! question Deezer hides from this server is answered from Apple's catalog in Deezer's shape. What
//! happened to each request sent to the API host is told to the reach.
//!
//! [`get`]: DeezerRateLimitHandler::get
//! [`get_after_permit`]: DeezerRateLimitHandler::get_after_permit

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use reqwest::StatusCode;
use reqwest::header::ACCEPT_LANGUAGE;
use tokio::time::Instant;
use tracing::warn;

use super::deezer_rate_limiter::DeezerRateLimiter;
use super::deezer_reach::{DeezerReach, Outcome};
use super::deezer_stand_in::{DeezerStandIn, Route};
use crate::services::apple::AppleClass;
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::client_builder;
use crate::services::http_client_factory::timeout_message;

/// Who a request to Deezer is for: the Deezer lane it takes (`background`), and the Apple class it
/// takes when Apple answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskFor {
    pub class: AppleClass,
    pub background: bool,
}

impl AskFor {
    /// Someone is waiting on this answer on a page.
    pub fn page() -> Self {
        Self {
            class: AppleClass::Page,
            background: false,
        }
    }

    /// A search row's details while the search waits.
    pub fn row() -> Self {
        Self {
            class: AppleClass::Row,
            background: false,
        }
    }

    /// A cover lookup, on the lane its caller chose.
    pub fn cover(background: bool) -> Self {
        Self {
            class: AppleClass::Cover,
            background,
        }
    }

    /// Work no one is waiting for.
    pub fn background() -> Self {
        Self {
            class: AppleClass::Background,
            background: true,
        }
    }

    /// The purpose of a caller that only knows its lane.
    pub fn lane(background: bool) -> Self {
        if background {
            Self::background()
        } else {
            Self::page()
        }
    }
}

/// Spends a [`DeezerRateLimiter`] permit before each Deezer API call. The limiter is the
/// singleton; this is the seam that consults it.
pub struct DeezerRateLimitHandler {
    client: reqwest::Client,
    limiter: Arc<DeezerRateLimiter>,
    api_host: String,
    /// How long a call may take: [`Self::TIMEOUT`], shorter in tests.
    timeout: Duration,
    stand_in: OnceLock<Arc<DeezerStandIn>>,
}

impl DeezerRateLimitHandler {
    pub const API_HOST: &'static str = "api.deezer.com";

    /// The callers' timeout, 8 seconds. With [`Self::get`] it covers waiting for a permit as well
    /// as the call and reading the answer; with [`Self::get_after_permit`] only the call and
    /// reading the answer.
    pub const TIMEOUT: Duration = Duration::from_secs(8);

    /// How long connecting may take: a host that never answers is told from a slow one.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

    pub fn new(limiter: Arc<DeezerRateLimiter>) -> Self {
        Self::with_api_host(limiter, Self::API_HOST)
    }

    /// A handler that meters another host as the API: a test's mock server.
    pub fn with_api_host(limiter: Arc<DeezerRateLimiter>, api_host: impl Into<String>) -> Self {
        let client = client_builder()
            .connect_timeout(Self::CONNECT_TIMEOUT)
            .build()
            .expect("the Deezer HTTP client builds");
        Self {
            client,
            limiter,
            api_host: api_host.into(),
            timeout: Self::TIMEOUT,
            stand_in: OnceLock::new(),
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

    /// Wires Apple's catalog in for what Deezer hides; set once, before any request.
    pub fn set_stand_in(&self, stand_in: Arc<DeezerStandIn>) {
        let _ = self.stand_in.set(stand_in);
    }

    /// The reach, when a stand-in is wired.
    pub fn reach(&self) -> Option<&Arc<DeezerReach>> {
        self.stand_in.get().map(|stand_in| stand_in.reach())
    }

    /// Whether Apple answers Deezer's catalog searches now.
    pub fn searches_stand_in(&self) -> bool {
        self.reach().is_some_and(|reach| reach.stands_in())
    }

    /// The catalog generation (0 with no stand-in), for the keys of what a caller keeps.
    pub fn generation(&self) -> u64 {
        self.reach().map_or(0, |reach| reach.generation())
    }

    /// Sends a request to a host that is not the API (an image), unmetered. A request to the API
    /// host is refused: it goes through [`Self::get_for`].
    pub async fn send(
        &self,
        request: reqwest::Request,
        _background: bool,
    ) -> anyhow::Result<reqwest::Response> {
        if self.is_api(&request) {
            return Err(anyhow!("a request to Deezer's API goes through get_for"));
        }
        Ok(self.client.execute(request).await?)
    }

    /// [`Self::get_for`] for a caller that only knows its lane.
    pub async fn get(
        &self,
        url: &str,
        background: bool,
        accept_language: Option<&str>,
    ) -> anyhow::Result<HttpAnswer> {
        self.get_for(url, AskFor::lane(background), accept_language).await
    }

    /// [`Self::get_after_permit_for`] for a caller that only knows its lane.
    pub async fn get_after_permit(
        &self,
        url: &str,
        background: bool,
        accept_language: Option<&str>,
    ) -> anyhow::Result<HttpAnswer> {
        self.get_after_permit_for(url, AskFor::lane(background), accept_language)
            .await
    }

    /// A GET with the given Accept-Language, the body read, and all of it (the permit, and Apple's
    /// turns when Apple answers, included) inside [`Self::TIMEOUT`].
    pub async fn get_for(
        &self,
        url: &str,
        purpose: AskFor,
        accept_language: Option<&str>,
    ) -> anyhow::Result<HttpAnswer> {
        let deadline = Instant::now() + self.timeout;
        let request = self.request(url, accept_language)?;
        let Some((stand_in, origin)) = self.stand_in_for(&request) else {
            return self.call(request, purpose, Some(deadline)).await.0;
        };
        let route = stand_in.route(request.url());
        match route {
            Route::Apple(question) => {
                match tokio::time::timeout_at(deadline, stand_in.answer(&question, &origin, purpose)).await {
                    Ok(answer) => Ok(answer),
                    Err(_) => Err(anyhow!(timeout_message(self.timeout))),
                }
            }
            Route::Refuse => Ok(HttpAnswer::empty(StatusCode::SERVICE_UNAVAILABLE)),
            Route::Deezer => {
                let (answer, outcome) = self.call(request, purpose, Some(deadline)).await;
                stand_in.reach().observe(&outcome);
                answer
            }
            Route::Check(question) => {
                let (answer, outcome) = self.call(request, purpose, Some(deadline)).await;
                stand_in.reach().observe(&outcome);
                match tokio::time::timeout_at(deadline, stand_in.after(&question, &origin, purpose, answer))
                    .await
                {
                    Ok(answer) => answer,
                    Err(_) => Err(anyhow!(timeout_message(self.timeout))),
                }
            }
        }
    }

    /// [`Self::get_for`] for a lookup whose busy queue must never read as no answer: it waits for its
    /// permit (or Apple's turn) first, as long as the queue takes, and only then do the calls start
    /// their time. A full Deezer queue still answers 429 at once.
    pub async fn get_after_permit_for(
        &self,
        url: &str,
        purpose: AskFor,
        accept_language: Option<&str>,
    ) -> anyhow::Result<HttpAnswer> {
        let request = self.request(url, accept_language)?;
        let Some((stand_in, origin)) = self.stand_in_for(&request) else {
            return self.call(request, purpose, None).await.0;
        };
        match stand_in.route(request.url()) {
            // Apple's turns are waited out; each of Apple's calls keeps its own timeout.
            Route::Apple(question) => Ok(stand_in.answer(&question, &origin, purpose).await),
            Route::Refuse => Ok(HttpAnswer::empty(StatusCode::SERVICE_UNAVAILABLE)),
            Route::Deezer => {
                let (answer, outcome) = self.call(request, purpose, None).await;
                stand_in.reach().observe(&outcome);
                answer
            }
            Route::Check(question) => {
                let (answer, outcome) = self.call(request, purpose, None).await;
                stand_in.reach().observe(&outcome);
                stand_in.after(&question, &origin, purpose, answer).await
            }
        }
    }

    /// A GET straight to Deezer, never routed by the stand-in, and what happened to it (told to the
    /// reach like every request to the API host): how `/infos` is read.
    pub(crate) async fn get_with_outcome(
        &self,
        url: &str,
        purpose: AskFor,
    ) -> (anyhow::Result<HttpAnswer>, Outcome) {
        let deadline = Instant::now() + self.timeout;
        let request = match self.request(url, None) {
            Ok(request) => request,
            Err(e) => return (Err(e.into()), Outcome::NotSent),
        };
        let stand_in = self.stand_in_for(&request);
        let (answer, outcome) = self.call(request, purpose, Some(deadline)).await;
        if let Some((stand_in, _)) = stand_in {
            stand_in.reach().observe(&outcome);
        }
        (answer, outcome)
    }

    /// The stand-in and the address's origin, for a request to the API host when one is wired.
    fn stand_in_for(&self, request: &reqwest::Request) -> Option<(Arc<DeezerStandIn>, String)> {
        if !self.is_api(request) {
            return None;
        }
        let stand_in = Arc::clone(self.stand_in.get()?);
        let url = request.url();
        let origin = match url.port() {
            Some(port) => format!("{}://{}:{port}", url.scheme(), url.host_str().unwrap_or("")),
            None => format!("{}://{}", url.scheme(), url.host_str().unwrap_or("")),
        };
        Some((stand_in, origin))
    }

    /// One request to Deezer, and what happened to it. With a `deadline`, the permit's wait and the
    /// call share it; without one, the permit is waited out and the call has [`Self::TIMEOUT`]. A
    /// request to the API host waits for a permit first.
    async fn call(
        &self,
        mut request: reqwest::Request,
        purpose: AskFor,
        deadline: Option<Instant>,
    ) -> (anyhow::Result<HttpAnswer>, Outcome) {
        if self.is_api(&request) {
            let permit = self.permit(&request, purpose.background);
            let allowed = match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, permit).await {
                    Ok(allowed) => allowed,
                    Err(_) => return (Err(anyhow!(timeout_message(self.timeout))), Outcome::NotSent),
                },
                None => permit.await,
            };
            if !allowed {
                return (
                    Ok(HttpAnswer::empty(StatusCode::TOO_MANY_REQUESTS)),
                    Outcome::NotSent,
                );
            }
        }
        let left = match deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => self.timeout,
        };
        if left.is_zero() {
            return (Err(anyhow!(timeout_message(self.timeout))), Outcome::NotSent);
        }
        *request.timeout_mut() = Some(left);
        let response = match self.client.execute(request).await {
            Ok(response) => response,
            Err(e) => {
                let outcome = if network_failed(e.is_connect(), e.is_timeout(), false, left) {
                    Outcome::NetworkFailed(e.to_string())
                } else {
                    Outcome::NotSent
                };
                return (Err(e.into()), outcome);
            }
        };
        // The headers came: Deezer answered, whatever happens to the body.
        let status = response.status().as_u16();
        match HttpAnswer::read(response).await {
            Ok(answer) => (Ok(answer), Outcome::Answered(status)),
            Err(e) => (Err(e.into()), Outcome::Answered(status)),
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

    fn is_api(&self, request: &reqwest::Request) -> bool {
        request
            .url()
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(&self.api_host))
    }

    /// Whether `request` may go: at once for a host that is not the API, else once a permit is
    /// held, however long the limiter's queue takes. False when the queue is full.
    async fn permit(&self, request: &reqwest::Request, background: bool) -> bool {
        // Only the API is metered. The cover-art lookup pulls actual image bytes from
        // cdn-images.dzcdn.net through this same client, and that host has no quota:
        // metering it would spend an API permit per rendered row and throttle a CDN for
        // nothing.
        if !self.is_api(request) {
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

/// Whether a request that failed says Deezer cannot be reached: its headers never came, and either
/// connecting failed or it timed out after being given at least the connect timeout. A request cut
/// short by what was left of its budget says nothing about Deezer.
pub(crate) fn network_failed(
    connect_error: bool,
    timed_out: bool,
    headers_seen: bool,
    time_given: Duration,
) -> bool {
    if headers_seen {
        return false;
    }
    if timed_out {
        return time_given >= DeezerRateLimitHandler::CONNECT_TIMEOUT;
    }
    connect_error
}

#[cfg(test)]
#[path = "deezer_rate_limit_handler_tests.rs"]
mod tests;
