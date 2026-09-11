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
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::credential::{redact_secrets_in_text, Secret};

/// Default request timeout. Providers override it per call (OpenRouter's `/key`
/// probe uses 1 s so a slow endpoint cannot hold up the 60 s refresh tick).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// `User-Agent` sent when a provider does not set its own (mirrors the Swift
/// `CodexBar/<version>` header so upstream logs line up).
pub const USER_AGENT: &str = concat!("CodexBar/", env!("CARGO_PKG_VERSION"));

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
        }
    }

    pub const fn status_code(&self) -> Option<u16> {
        self.status
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
    pub fn new() -> Result<Self, HttpError> {
        let inner = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
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
            .timeout(request.timeout);

        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }

        let response = builder.send().map_err(|e| {
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

/// Same as [`default_client`] but reusing one `reqwest` client (connection pool)
/// across providers is not possible through a `dyn` box; kept explicit so the
/// intent is obvious to the next worker.
pub fn shared_client() -> std::sync::Arc<dyn HttpClient> {
    std::sync::Arc::from(default_client())
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
