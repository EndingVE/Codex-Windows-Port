//! HTTP plumbing shared by every real provider.
//!
//! Providers never call `reqwest` directly. They build an [`HttpRequest`], hand it
//! to an [`HttpClient`], and read back an [`HttpResponse`]. That indirection is
//! what makes providers testable **without network access**: tests inject a
//! [`crate::testing::FixtureClient`] and replay recorded bodies.
//!
//! Two rules this module exists to enforce:
//!
//! 1. **No secret ever reaches an error message or a debug print.** Bearer tokens
//!    are attached through [`crate::credential::Secret`], and URL/body text is
//!    passed through [`crate::credential::redact_secrets_in_text`] before it is
//!    embedded in an error. [`HttpRequest::safe_url`] drops the query string,
//!    because several providers put tokens there.
//! 2. **`http://` never receives a credential.** [`secure_base_url`] normalises a
//!    bare host to HTTPS and *fails closed* on an explicit `http://` override or
//!    on `userinfo` in the URL.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use codexbar_core::refresh::FailureClass;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::credential::{redact_secrets_in_text, Secret};

/// Default request timeout. Providers override it per call (OpenRouter's `/key`
/// probe uses 1 s so a slow endpoint cannot hold up the 60 s refresh tick).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// `User-Agent` sent when a provider does not set its own (mirrors the Swift
/// `CodexBar/<version>` header so upstream logs line up).
pub const USER_AGENT: &str = concat!("CodexBar/", env!("CARGO_PKG_VERSION"));

/// TCP/TLS connect budget of the shared client. A dead host fails in seconds
/// instead of eating the whole request timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Requests with a shorter timeout than this are best-effort probes and are not
/// retried after a timeout.
pub const MIN_RETRYABLE_TIMEOUT: Duration = Duration::from_secs(3);

/// Upper bound for any single request on the shared client, whatever the
/// request itself asks for (a provider's own `timeout()` is usually lower).
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Body size cap for error messages, per `SPEC-flagship.md` §9.8.
const ERROR_BODY_CHARS: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A fully-described outbound request.
///
/// `Debug` is implemented by hand: the derived one would print header values,
/// and header values are where bearer tokens live.
#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.safe_url())
            .field("headers", &self.header_names())
            .field("bodyBytes", &self.body.as_ref().map(Vec::len))
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self::new(Method::Get, url)
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self::new(Method::Post, url)
    }

    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: vec![("User-Agent".into(), USER_AGENT.into())],
            body: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Add or replace a header. Names are matched case-insensitively.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        let value = value.into();
        if let Some(slot) = self
            .headers
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(&name))
        {
            slot.1 = value;
        } else {
            self.headers.push((name, value));
        }
        self
    }

    /// `Authorization: Bearer <token>`. Takes a [`Secret`] so a raw `String`
    /// cannot be leaked into a request by accident.
    pub fn bearer(self, token: &Secret) -> Self {
        self.header("Authorization", format!("Bearer {}", token.expose()))
    }

    /// Raw authorization header, for providers that use a non-bearer scheme
    /// (GitHub Copilot's `token …`, Groq's Stytch `Basic …`).
    pub fn authorization(self, value: impl Into<String>) -> Self {
        self.header("Authorization", value)
    }

    pub fn accept_json(self) -> Self {
        self.header("Accept", "application/json")
    }

    pub fn json_body<T: Serialize>(mut self, value: &T) -> Result<Self, HttpError> {
        let encoded = serde_json::to_vec(value)
            .map_err(|e| HttpError::invalid_request(format!("cannot encode request body: {e}")))?;
        self.body = Some(encoded);
        Ok(self.header("Content-Type", "application/json"))
    }

    /// `application/x-www-form-urlencoded` body (OAuth token exchanges).
    pub fn form_body(mut self, fields: &[(&str, &str)]) -> Self {
        let encoded = fields
            .iter()
            .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        self.body = Some(encoded.into_bytes());
        self.header("Content-Type", "application/x-www-form-urlencoded")
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `METHOD host/path` with the query string removed.
    ///
    /// Safe to put in logs and error messages: several providers (OpenCode Go,
    /// z.ai) carry identifiers in query parameters.
    pub fn safe_url(&self) -> String {
        let (scheme, rest) = match self.url.split_once("://") {
            Some((scheme, rest)) => (format!("{scheme}://"), rest),
            None => (String::new(), self.url.as_str()),
        };
        let without_path = rest.split(['?', '#']).next().unwrap_or("");
        let without_userinfo = match without_path.split_once('@') {
            Some((_, host)) => host,
            None => without_path,
        };
        format!("{}{}", scheme, without_userinfo)
    }

    /// Header names only — values are never rendered.
    fn header_names(&self) -> String {
        self.headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A response, as returned by an [`HttpClient`].
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `Retry-After`, as delta-seconds or an HTTP-date (RFC 7231 §7.1.3).
    pub fn retry_after(&self) -> Option<Duration> {
        parse_retry_after(self.header("Retry-After")?, chrono::Utc::now())
    }

    /// Lossy UTF-8 view of the body (upstream error pages are not always UTF-8).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Parse the body as JSON. Never includes body contents in the error.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, HttpError> {
        serde_json::from_slice(&self.body).map_err(|e| {
            HttpError::decode(format!(
                "HTTP {} returned a body that is not the expected JSON ({e})",
                self.status
            ))
        })
    }

    /// Up to [`ERROR_BODY_CHARS`] characters of the body, with anything that
    /// looks like a credential masked. For diagnostics only.
    pub fn error_excerpt(&self) -> String {
        let text = self.text();
        let clipped: String = text.chars().take(ERROR_BODY_CHARS).collect();
        redact_secrets_in_text(&clipped)
    }
}

/// Why a request failed. `message` is always safe to show to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpError {
    pub kind: HttpErrorKind,
    pub message: String,
    /// Present for [`HttpErrorKind::Status`].
    pub status: Option<u16>,
    /// Parsed `Retry-After` of a 429/503 response, when the server sent one.
    pub retry_after: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpErrorKind {
    /// Connection failed (DNS, TLS, refused).
    Connect,
    /// Request exceeded its timeout.
    Timeout,
    /// Non-2xx response.
    Status,
    /// 2xx but the body did not parse.
    Decode,
    /// We refused to build or send the request (bad URL, `http://` override).
    InvalidRequest,
    /// The client itself could not be constructed — every request fails.
    Client,
}

impl HttpErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            HttpErrorKind::Connect => "connect",
            HttpErrorKind::Timeout => "timeout",
            HttpErrorKind::Status => "status",
            HttpErrorKind::Decode => "decode",
            HttpErrorKind::InvalidRequest => "invalidRequest",
            HttpErrorKind::Client => "client",
        }
    }
}

impl HttpError {
    pub fn new(kind: HttpErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retry_after: None,
        }
    }

    pub fn connect(message: impl Into<String>) -> Self {
        Self::new(HttpErrorKind::Connect, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(HttpErrorKind::Timeout, message)
    }

    pub fn decode(message: impl Into<String>) -> Self {
        Self::new(HttpErrorKind::Decode, message)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(HttpErrorKind::InvalidRequest, message)
    }

    pub fn client(message: impl Into<String>) -> Self {
        Self::new(HttpErrorKind::Client, message)
    }

    /// Non-2xx. `request` supplies the redacted URL, `response` the excerpt.
    pub fn status(request: &HttpRequest, response: &HttpResponse) -> Self {
        Self {
            kind: HttpErrorKind::Status,
            message: format!(
                "HTTP {} from {} — {}",
                response.status,
                request.safe_url(),
                response.error_excerpt()
            ),
            status: Some(response.status),
            retry_after: response.retry_after(),
        }
    }

    pub const fn status_code(&self) -> Option<u16> {
        self.status
    }

    /// Server-requested wait before the next attempt (`Retry-After` on a
    /// 429/503), if any. The UI can turn this into "retry at HH:MM".
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Coarse, user-actionable class of this error (shared with
    /// `codexbar_core::refresh`).
    pub fn class(&self) -> FailureClass {
        match self.kind {
            HttpErrorKind::Connect | HttpErrorKind::Client => FailureClass::Network,
            HttpErrorKind::Timeout => FailureClass::Timeout,
            HttpErrorKind::Decode | HttpErrorKind::InvalidRequest => FailureClass::Other,
            HttpErrorKind::Status => match self.status {
                Some(401) | Some(403) => FailureClass::Auth,
                Some(429) => FailureClass::RateLimited,
                Some(code) if (500..600).contains(&code) => FailureClass::Server,
                // A `Status` error without a code is a provider-level mapping
                // (e.g. DeepSeek's platform codes): classify its prose.
                None => codexbar_core::refresh::classify_error(&self.message),
                Some(_) => FailureClass::Other,
            },
        }
    }

    /// Credentials rejected — the user has to sign in again. Never retried.
    pub fn is_auth(&self) -> bool {
        self.class() == FailureClass::Auth
    }

    /// Worth retrying later (network, timeout, 5xx, 429).
    pub fn is_transient(&self) -> bool {
        self.class().is_transient()
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for HttpError {}

/// The seam every provider goes through. Implemented by [`ReqwestClient`] in
/// production and by `codexbar_providers::testing::FixtureClient` in tests.
pub trait HttpClient: Send + Sync {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError>;

    /// Convenience: send and require a 2xx, turning anything else into a `Status`
    /// error whose message is already redacted and truncated.
    fn send_ok(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let response = self.execute(request)?;
        if response.is_success() {
            Ok(response)
        } else {
            Err(HttpError::status(request, &response))
        }
    }
}

/// The real client: `reqwest::blocking` with rustls.
pub struct ReqwestClient {
    inner: reqwest::blocking::Client,
}

impl ReqwestClient {
    /// One pooled client: connect + total timeouts, keep-alive, no redirects.
    ///
    /// TLS trusts both the bundled webpki roots and the Windows certificate
    /// store (`rustls-tls-native-roots`), and the system proxy settings are
    /// honoured (`system-proxy`), so a corporate proxy or a TLS-inspecting VPN
    /// works like it does for the browser.
    pub fn new() -> Result<Self, HttpError> {
        let inner = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(MAX_REQUEST_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            // A redirect could carry the bearer to another host; providers that
            // legitimately redirect handle it themselves.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| {
                HttpError::client(format!("could not initialise the HTTPS client: {e}"))
            })?;
        Ok(Self { inner })
    }
}

impl fmt::Debug for ReqwestClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReqwestClient")
    }
}

impl HttpClient for ReqwestClient {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
        };

        let mut builder = self
            .inner
            .request(method, &request.url)
            .timeout(request.timeout.min(MAX_REQUEST_TIMEOUT));

        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }

        let response = builder.send().map_err(|e| {
            NETWORK_FAILURES.fetch_add(1, Ordering::Relaxed);
            if e.is_timeout() {
                HttpError::timeout(format!(
                    "{} timed out after {}s",
                    request.safe_url(),
                    request.timeout.as_secs()
                ))
            } else {
                HttpError::connect(format!("{} could not be reached: {e}", request.safe_url()))
            }
        })?;

        RESPONSES.fetch_add(1, Ordering::Relaxed);
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = response.bytes().map_err(|e| {
            HttpError::connect(format!(
                "reading the body of {} failed: {e}",
                request.safe_url()
            ))
        })?;

        Ok(HttpResponse {
            status,
            headers,
            body: body.to_vec(),
        })
    }
}

impl fmt::Debug for dyn HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HttpClient")
    }
}

/// A client whose construction failed. It answers every request with a
/// [`HttpErrorKind::Client`] error instead of panicking — a provider must be able
/// to run even if the TLS backend is unavailable.
#[derive(Debug)]
pub struct FailingClient {
    reason: String,
}

impl FailingClient {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl HttpClient for FailingClient {
    fn execute(&self, _request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::client(self.reason.clone()))
    }
}

/// The shared production client used by [`crate::live_registry`].
///
/// Never panics: if the TLS stack cannot start, providers get a `FailingClient`
/// and report a normal `FetchStatus::Error`.
pub fn default_client() -> Box<dyn HttpClient> {
    match ReqwestClient::new() {
        Ok(client) => Box::new(client),
        Err(err) => Box::new(FailingClient::new(err.message)),
    }
}

/// The process-wide client every production provider uses.
///
/// Built **once** (one connection pool, one TLS config) and wrapped in a
/// [`RetryingClient`] with [`RetryPolicy::default`]. Every call returns the same
/// `Arc`, so `live_registry()` and each provider's `new()` share the pool.
pub fn shared_client() -> Arc<dyn HttpClient> {
    static SHARED: OnceLock<Arc<dyn HttpClient>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| {
        let inner: Arc<dyn HttpClient> = Arc::from(default_client());
        Arc::new(RetryingClient::new(inner, RetryPolicy::default()))
    }))
}

// ---------------------------------------------------------------------------
// Connectivity counters + offline hint
// ---------------------------------------------------------------------------

static RESPONSES: AtomicU64 = AtomicU64::new(0);
static NETWORK_FAILURES: AtomicU64 = AtomicU64::new(0);
static OFFLINE_HINT: AtomicBool = AtomicBool::new(false);

/// Monotonic transport counters of the real client ([`ReqwestClient`]).
///
/// Take one before and one after a refresh cycle; [`ConnectivityCounters::since`]
/// gives the cycle's delta and [`ConnectivityCounters::looks_offline`] the verdict
/// (network failures and not a single HTTP response of any status).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectivityCounters {
    /// HTTP responses received (any status).
    pub responses: u64,
    /// Requests that failed before a response (connect / timeout / TLS).
    pub network_failures: u64,
}

impl ConnectivityCounters {
    pub fn since(self, earlier: ConnectivityCounters) -> ConnectivityCounters {
        ConnectivityCounters {
            responses: self.responses.saturating_sub(earlier.responses),
            network_failures: self
                .network_failures
                .saturating_sub(earlier.network_failures),
        }
    }

    pub const fn looks_offline(self) -> bool {
        codexbar_core::refresh::looks_offline(self.responses, self.network_failures)
    }
}

/// Current transport counters.
pub fn connectivity() -> ConnectivityCounters {
    ConnectivityCounters {
        responses: RESPONSES.load(Ordering::Relaxed),
        network_failures: NETWORK_FAILURES.load(Ordering::Relaxed),
    }
}

/// Tell the retry layer the machine looked offline last cycle. While set,
/// [`RetryingClient`] makes a single attempt per request (no retry loop against
/// a dead network); the refresh loop clears it as soon as anything answers.
pub fn set_offline_hint(offline: bool) {
    OFFLINE_HINT.store(offline, Ordering::Relaxed);
}

pub fn offline_hint() -> bool {
    OFFLINE_HINT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Retry with exponential backoff + jitter
// ---------------------------------------------------------------------------

/// When and how long [`RetryingClient`] waits between attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Extra attempts after the first (0 = never retry).
    pub max_retries: u32,
    /// Delay before the first retry; doubles each time.
    pub base_delay: Duration,
    /// Cap for the computed backoff delay.
    pub max_delay: Duration,
    /// A `Retry-After` longer than this is not waited for: the error is
    /// returned (with [`HttpError::retry_after`] set) and the next tick retries.
    pub max_retry_after: Duration,
    /// No new attempt starts once this much time has passed since the first
    /// one, so retries stay inside the refresh pipeline's per-provider deadline.
    pub max_elapsed: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(400),
            max_delay: Duration::from_secs(4),
            max_retry_after: Duration::from_secs(10),
            max_elapsed: Duration::from_secs(25),
        }
    }
}

impl RetryPolicy {
    /// No retries at all.
    pub const fn none() -> Self {
        Self {
            max_retries: 0,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            max_retry_after: Duration::ZERO,
            max_elapsed: Duration::ZERO,
        }
    }

    /// Backoff before retry number `attempt` (1-based), with "equal jitter":
    /// half the exponential delay fixed, half random. `jitter` is in `[0, 1)`.
    pub fn backoff(&self, attempt: u32, jitter: f64) -> Duration {
        let exp = attempt.saturating_sub(1).min(16);
        let full = self
            .base_delay
            .saturating_mul(1u32 << exp)
            .min(self.max_delay);
        let half = full / 2;
        half + half.mul_f64(jitter.clamp(0.0, 1.0))
    }
}

/// Why an attempt may (or may not) be retried.
///
/// Returns `None` for "do not retry", `Some(None)` for "retry after the
/// computed backoff" and `Some(Some(wait))` for "retry after the server's
/// `Retry-After`".
///
/// * **401 / 403 and every other 4xx: never.**
/// * 429: only when the server sent a `Retry-After` we are willing to wait for
///   (at most [`RetryPolicy::max_retry_after`]); a longer one is returned to the
///   caller with [`HttpError::retry_after`] set.
/// * `GET` / `PUT` / `DELETE` (idempotent): network failures, timeouts and
///   500/502/503/504 are retried — except a timeout on a request shorter than
///   [`MIN_RETRYABLE_TIMEOUT`], which marks a best-effort probe.
/// * `POST`: only the 429 case above. A POST may already have been acted on
///   (OAuth refresh tokens rotate, device-flow polls are rate-limited), so a
///   blind replay could burn a credential.
pub fn retry_decision(
    request: &HttpRequest,
    outcome: &Result<HttpResponse, HttpError>,
    policy: &RetryPolicy,
) -> Option<Option<Duration>> {
    let idempotent = request.method != Method::Post;
    match outcome {
        Ok(response) => match response.status {
            429 => match response.retry_after() {
                Some(wait) if wait <= policy.max_retry_after => Some(Some(wait)),
                _ => None,
            },
            500 | 502 | 503 | 504 if idempotent => match response.retry_after() {
                Some(wait) if wait > policy.max_retry_after => None,
                other => Some(other),
            },
            _ => None,
        },
        Err(err) if idempotent => match err.kind {
            HttpErrorKind::Connect => Some(None),
            HttpErrorKind::Timeout if request.timeout >= MIN_RETRYABLE_TIMEOUT => Some(None),
            _ => None,
        },
        Err(_) => None,
    }
}

type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

/// [`HttpClient`] decorator that retries transient failures with exponential
/// backoff and jitter. See [`retry_decision`] for exactly what is retried.
pub struct RetryingClient {
    inner: Arc<dyn HttpClient>,
    policy: RetryPolicy,
    sleeper: Sleeper,
}

impl RetryingClient {
    pub fn new(inner: Arc<dyn HttpClient>, policy: RetryPolicy) -> Self {
        Self {
            inner,
            policy,
            sleeper: Arc::new(std::thread::sleep),
        }
    }

    /// Replace the sleep function (tests record the waits instead of sleeping).
    pub fn with_sleeper(mut self, sleeper: impl Fn(Duration) + Send + Sync + 'static) -> Self {
        self.sleeper = Arc::new(sleeper);
        self
    }

    pub fn policy(&self) -> RetryPolicy {
        self.policy
    }
}

impl fmt::Debug for RetryingClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetryingClient")
            .field("policy", &self.policy)
            .finish()
    }
}

impl HttpClient for RetryingClient {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let max_retries = if offline_hint() {
            0
        } else {
            self.policy.max_retries
        };
        let started = std::time::Instant::now();
        let mut attempt = 0;
        loop {
            let outcome = self.inner.execute(request);
            if attempt >= max_retries {
                return outcome;
            }
            let Some(server_wait) = retry_decision(request, &outcome, &self.policy) else {
                return outcome;
            };
            attempt += 1;
            let wait = server_wait.unwrap_or_else(|| self.policy.backoff(attempt, jitter()));
            if started.elapsed() + wait >= self.policy.max_elapsed {
                return outcome;
            }
            (self.sleeper)(wait);
        }
    }
}

/// Cheap `[0, 1)` jitter without a RNG dependency (splitmix64 over the clock
/// and a counter). Not cryptographic; it only has to de-synchronise retries.
fn jitter() -> f64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut z = nanos ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// Parse a `Retry-After` value: delta-seconds (`120`) or an HTTP-date
/// (`Wed, 21 Oct 2015 07:28:00 GMT`). A date in the past is `0 s`.
pub fn parse_retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let delta = when.with_timezone(&chrono::Utc) - now;
    Some(delta.to_std().unwrap_or(Duration::ZERO))
}

// ---------------------------------------------------------------------------
// Endpoint policy (`SPEC-apikey.md` §1.5)
// ---------------------------------------------------------------------------

/// Normalise a user-supplied base URL and refuse anything that would send a
/// bearer over plaintext.
///
/// * `None` / empty → `default`.
/// * bare host (`api.z.ai`, `api.z.ai/v1`) → `https://…`.
/// * explicit `http://` → `Err` (fail closed **before** the token is attached).
/// * `userinfo@host` → `Err` (credential-in-URL phishing).
/// * whitespace or control characters → `Err`.
pub fn secure_base_url(raw: Option<&str>, default: &str) -> Result<String, String> {
    let trimmed = raw.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        return Ok(trim(default));
    }
    if trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("endpoint override contains whitespace or control characters".into());
    }

    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{}", trimmed.trim_start_matches('/'))
    };

    let (scheme, rest) = with_scheme.split_once("://").expect("contains ://");
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(format!(
            "endpoint override must be HTTPS, refusing \"{scheme}://\""
        ));
    }

    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err("endpoint override has no host".into());
    }
    if authority.contains('@') {
        return Err("endpoint override must not contain userinfo".into());
    }
    // Reject encoded host delimiters anywhere in the URL (`%2f`, `%5c`, `%40`,
    // `%3a`): a proxy or DNS layer that decodes them can turn a validated URL
    // into a different host, which is how a bearer ends up somewhere else.
    let lowered = with_scheme.to_ascii_lowercase();
    if ["%2f", "%5c", "%40", "%3a"]
        .iter()
        .any(|d| lowered.contains(d))
    {
        return Err("endpoint override must not contain encoded host delimiters".into());
    }

    Ok(trim(&with_scheme))
}

/// Reject a URL that is not HTTPS. Used for the *fixed* endpoints that are not
/// user-configurable but are passed through config on some providers.
pub fn ensure_https(url: &str) -> Result<(), String> {
    if url.trim().to_ascii_lowercase().starts_with("https://") {
        Ok(())
    } else {
        Err(format!("refusing non-HTTPS endpoint {}", truncate(url, 60)))
    }
}

/// Restrict an override to a set of allowed host suffixes (MiniMax, z.ai).
pub fn host_is_allowed(url: &str, allowed_suffixes: &[&str]) -> bool {
    let Some(authority) = url.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    let host = authority
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    allowed_suffixes.iter().any(|suffix| {
        let suffix = suffix.to_ascii_lowercase();
        host == suffix || host.ends_with(&format!(".{suffix}"))
    })
}

fn trim(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        value.chars().take(max).collect::<String>() + "…"
    }
}

/// Minimal `application/x-www-form-urlencoded` encoder (form bodies only; the
/// crate deliberately avoids a full URL dependency for this).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_url_drops_query_and_userinfo() {
        let req = HttpRequest::get("https://example.com/v1/usage?api_key=abc123#frag");
        assert_eq!(req.safe_url(), "https://example.com/v1/usage");
        let req = HttpRequest::get("https://user:pw@example.com/v1");
        assert_eq!(req.safe_url(), "https://example.com/v1");
    }

    #[test]
    fn bearer_round_trips_through_the_secret() {
        let token = Secret::new("sk-or-v1-abcdefghijklmnop");
        let req = HttpRequest::get("https://example.com").bearer(&token);
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .unwrap();
        assert!(auth.1.contains("abcdefghijklmnop"));
        // …but the Debug of the request must not:
        assert!(!format!("{:?}", req).contains("abcdefghijklmnop"));
    }

    #[test]
    fn secure_base_url_normalises_and_fails_closed() {
        assert_eq!(
            secure_base_url(None, "https://openrouter.ai/api/v1").unwrap(),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            secure_base_url(Some("api.z.ai/"), "https://api.z.ai").unwrap(),
            "https://api.z.ai"
        );
        assert!(secure_base_url(Some("http://api.z.ai"), "x").is_err());
        assert!(secure_base_url(Some("https://user:pw@api.z.ai"), "x").is_err());
        assert!(secure_base_url(Some("https://api.z.ai/%2fevil"), "x").is_err());
        // A stray newline is trimmed (common from a `.env` paste)…
        assert_eq!(
            secure_base_url(Some("https://api.z.ai\n"), "x").unwrap(),
            "https://api.z.ai"
        );
        // …but an embedded one is refused, not silently stripped.
        assert!(secure_base_url(Some("https://api.\nz.ai"), "x").is_err());
    }

    #[test]
    fn host_allowlist_matches_suffixes_not_prefixes() {
        assert!(host_is_allowed(
            "https://api.minimax.io/v1",
            &["minimax.io"]
        ));
        assert!(host_is_allowed(
            "https://platform.minimaxi.com/x",
            &["minimaxi.com"]
        ));
        assert!(!host_is_allowed(
            "https://evilminimax.io/x",
            &["minimax.io"]
        ));
        assert!(!host_is_allowed(
            "https://minimax.io.evil.com/x",
            &["minimax.io"]
        ));
    }

    #[test]
    fn error_excerpt_is_truncated_and_masked() {
        let long = format!("sk-or-v1-{}", "a".repeat(600));
        let response = HttpResponse::new(401, long.into_bytes());
        let excerpt = response.error_excerpt();
        assert!(excerpt.chars().count() <= ERROR_BODY_CHARS);
        assert!(!excerpt.contains(&"a".repeat(64)));
        assert!(excerpt.contains('…') || excerpt.contains("sk-or-v1…"));
    }

    #[test]
    fn failing_client_never_panics() {
        let client = FailingClient::new("TLS unavailable");
        let err = client
            .execute(&HttpRequest::get("https://example.com"))
            .unwrap_err();
        assert_eq!(err.kind, HttpErrorKind::Client);
    }

    // ---- retry / backoff ---------------------------------------------------

    use crate::testing::{FixtureClient, FixtureResponse};
    use std::sync::Mutex;

    fn retrying(
        script: Vec<FixtureResponse>,
    ) -> (
        RetryingClient,
        Arc<FixtureClient>,
        Arc<Mutex<Vec<Duration>>>,
    ) {
        let fixture = Arc::new(FixtureClient::new(script));
        let waits = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&waits);
        let client = RetryingClient::new(
            Arc::clone(&fixture) as Arc<dyn HttpClient>,
            RetryPolicy::default(),
        )
        .with_sleeper(move |d| recorder.lock().unwrap().push(d));
        (client, fixture, waits)
    }

    #[test]
    fn retries_503_then_succeeds() {
        let (client, fixture, waits) = retrying(vec![
            FixtureResponse::text(503, "maintenance"),
            FixtureResponse::json(200, "{}"),
        ]);
        let response = client
            .send_ok(&HttpRequest::get("https://example.com/usage"))
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(fixture.request_count(), 2);
        let waits = waits.lock().unwrap();
        assert_eq!(waits.len(), 1);
        let policy = RetryPolicy::default();
        assert!(waits[0] >= policy.base_delay / 2 && waits[0] <= policy.base_delay);
    }

    #[test]
    fn retries_connect_errors_up_to_the_limit() {
        let (client, fixture, waits) = retrying(vec![
            FixtureResponse::failure(HttpErrorKind::Connect, "refused"),
            FixtureResponse::failure(HttpErrorKind::Connect, "refused"),
            FixtureResponse::failure(HttpErrorKind::Connect, "refused"),
            FixtureResponse::json(200, "{}"),
        ]);
        let err = client
            .execute(&HttpRequest::get("https://example.com"))
            .unwrap_err();
        assert_eq!(err.kind, HttpErrorKind::Connect);
        assert_eq!(fixture.request_count(), 3, "1 attempt + 2 retries");
        assert_eq!(waits.lock().unwrap().len(), 2);
        assert!(!fixture.exhausted());
    }

    #[test]
    fn never_retries_401_or_403() {
        for status in [401, 403] {
            let (client, fixture, waits) = retrying(vec![
                FixtureResponse::text(status, "nope"),
                FixtureResponse::json(200, "{}"),
            ]);
            let err = client
                .send_ok(&HttpRequest::get("https://example.com"))
                .unwrap_err();
            assert_eq!(err.status_code(), Some(status));
            assert!(err.is_auth());
            assert!(!err.is_transient());
            assert_eq!(
                fixture.request_count(),
                1,
                "HTTP {status} must not be retried"
            );
            assert!(waits.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn respects_retry_after_on_429() {
        let (client, fixture, waits) = retrying(vec![
            FixtureResponse::with_headers(429, "slow down", &[("Retry-After", "3")]),
            FixtureResponse::json(200, "{}"),
        ]);
        client
            .send_ok(&HttpRequest::get("https://example.com"))
            .unwrap();
        assert_eq!(fixture.request_count(), 2);
        assert_eq!(*waits.lock().unwrap(), vec![Duration::from_secs(3)]);
    }

    #[test]
    fn does_not_wait_out_a_long_retry_after_and_exposes_it() {
        let (client, fixture, waits) = retrying(vec![FixtureResponse::with_headers(
            429,
            "quota",
            &[("Retry-After", "600")],
        )]);
        let err = client
            .send_ok(&HttpRequest::get("https://example.com"))
            .unwrap_err();
        assert_eq!(fixture.request_count(), 1);
        assert!(waits.lock().unwrap().is_empty());
        assert_eq!(err.retry_after(), Some(Duration::from_secs(600)));
        assert_eq!(err.class(), FailureClass::RateLimited);
    }

    #[test]
    fn plain_429_without_retry_after_is_not_retried() {
        let (client, fixture, _) = retrying(vec![
            FixtureResponse::text(429, "slow down"),
            FixtureResponse::json(200, "{}"),
        ]);
        assert!(client
            .send_ok(&HttpRequest::get("https://example.com"))
            .is_err());
        assert_eq!(fixture.request_count(), 1);
    }

    #[test]
    fn post_is_not_replayed_on_5xx_or_connect_errors() {
        for script in [
            vec![
                FixtureResponse::text(503, "busy"),
                FixtureResponse::json(200, "{}"),
            ],
            vec![
                FixtureResponse::connect_error(),
                FixtureResponse::json(200, "{}"),
            ],
        ] {
            let (client, fixture, _) = retrying(script);
            assert!(client
                .send_ok(&HttpRequest::post("https://example.com/token"))
                .is_err());
            assert_eq!(fixture.request_count(), 1);
        }
    }

    #[test]
    fn timed_out_post_is_not_retried() {
        let (client, fixture, _) = retrying(vec![
            FixtureResponse::timeout(),
            FixtureResponse::json(200, "{}"),
        ]);
        let err = client
            .execute(&HttpRequest::post("https://example.com/token"))
            .unwrap_err();
        assert_eq!(err.kind, HttpErrorKind::Timeout);
        assert_eq!(fixture.request_count(), 1);
    }

    #[test]
    fn short_best_effort_probe_is_not_retried_on_timeout() {
        let (client, fixture, _) = retrying(vec![
            FixtureResponse::timeout(),
            FixtureResponse::json(200, "{}"),
        ]);
        let probe = HttpRequest::get("https://example.com/key").timeout(Duration::from_secs(1));
        assert!(client.execute(&probe).is_err());
        assert_eq!(fixture.request_count(), 1);

        let (client, fixture, _) = retrying(vec![
            FixtureResponse::timeout(),
            FixtureResponse::json(200, "{}"),
        ]);
        assert!(client
            .execute(&HttpRequest::get("https://example.com/usage"))
            .is_ok());
        assert_eq!(fixture.request_count(), 2);
    }

    #[test]
    fn backoff_is_exponential_capped_and_jittered() {
        let policy = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(1000),
            max_retry_after: Duration::from_secs(1),
            max_elapsed: Duration::from_secs(60),
        };
        assert_eq!(policy.backoff(1, 0.0), Duration::from_millis(50));
        assert_eq!(policy.backoff(1, 1.0), Duration::from_millis(100));
        assert_eq!(policy.backoff(2, 1.0), Duration::from_millis(200));
        assert_eq!(policy.backoff(3, 1.0), Duration::from_millis(400));
        assert_eq!(policy.backoff(9, 1.0), Duration::from_millis(1000));
        for _ in 0..100 {
            let j = jitter();
            assert!((0.0..1.0).contains(&j));
        }
    }

    #[test]
    fn retry_after_parses_seconds_and_http_dates() {
        let now = chrono::DateTime::parse_from_rfc3339("2015-10-21T07:27:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            parse_retry_after(" 120 ", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", now),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn shared_client_is_one_instance() {
        let a = shared_client();
        let b = shared_client();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn error_classes() {
        let req = HttpRequest::get("https://example.com");
        let class_of =
            |status: u16| HttpError::status(&req, &HttpResponse::new(status, "")).class();
        assert_eq!(class_of(401), FailureClass::Auth);
        assert_eq!(class_of(403), FailureClass::Auth);
        assert_eq!(class_of(429), FailureClass::RateLimited);
        assert_eq!(class_of(502), FailureClass::Server);
        assert_eq!(class_of(404), FailureClass::Other);
        assert_eq!(HttpError::connect("x").class(), FailureClass::Network);
        assert_eq!(HttpError::timeout("x").class(), FailureClass::Timeout);
        assert_eq!(HttpError::decode("x").class(), FailureClass::Other);
    }

    #[test]
    fn connectivity_delta_and_offline_verdict() {
        let before = ConnectivityCounters {
            responses: 10,
            network_failures: 2,
        };
        let offline = ConnectivityCounters {
            responses: 10,
            network_failures: 7,
        };
        assert!(offline.since(before).looks_offline());
        let online = ConnectivityCounters {
            responses: 11,
            network_failures: 7,
        };
        assert!(!online.since(before).looks_offline());
    }

    #[test]
    fn form_body_is_url_encoded() {
        let req = HttpRequest::post("https://example.com/token")
            .form_body(&[("grant_type", "refresh_token"), ("scope", "a b")]);
        assert_eq!(
            String::from_utf8(req.body.unwrap()).unwrap(),
            "grant_type=refresh_token&scope=a+b"
        );
    }
}
