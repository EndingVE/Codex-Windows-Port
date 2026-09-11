//! Test doubles and fixture loading.
//!
//! Every provider test in this crate (and in the 13 provider workers that follow)
//! must run **without network**. [`FixtureClient`] is how: it replays canned
//! responses in order and records what the provider actually sent, so a test can
//! assert on the request *and* on the mapping to `RateWindow` in one place.
//!
//! ```ignore
//! let client = FixtureClient::new(vec![
//!     FixtureResponse::json(200, include_str!("fixtures/openrouter/credits.json")),
//!     FixtureResponse::failure(HttpErrorKind::Timeout, "1s deadline"),
//! ]);
//! let provider = OpenRouter::with_client(Box::new(client), Env::empty().with("OPENROUTER_API_KEY", "sk-…"));
//! let snapshot = provider.fetch(now);
//! ```
//!
//! `FixtureResponse` variants keep the raw body as a `String`, so fixtures can be
//! `include_str!`-ed straight out of `tests/fixtures/…` and reviewed as normal
//! JSON files.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;

use serde::de::DeserializeOwned;

use crate::credential::redact_secrets_in_text;
use crate::http::{HttpClient, HttpError, HttpErrorKind, HttpRequest, HttpResponse, Method};

/// One canned response.
#[derive(Debug, Clone)]
pub enum FixtureResponse {
    /// A JSON body with the given status.
    Json { status: u16, body: String },
    /// A non-JSON body (HTML error page, empty 204, …).
    Text { status: u16, body: String },
    /// The request never completed.
    Failure {
        kind: HttpErrorKind,
        message: String,
    },
}

impl FixtureResponse {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        FixtureResponse::Json {
            status,
            body: body.into(),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        FixtureResponse::Text {
            status,
            body: body.into(),
        }
    }

    pub fn failure(kind: HttpErrorKind, message: impl Into<String>) -> Self {
        FixtureResponse::Failure {
            kind,
            message: message.into(),
        }
    }

    /// A request that exceeded its deadline — the OpenRouter `/key` degradation
    /// path is exercised with this.
    pub fn timeout() -> Self {
        Self::failure(HttpErrorKind::Timeout, "request timed out (fixture)")
    }
}

impl From<FixtureResponse> for HttpResponse {
    fn from(value: FixtureResponse) -> Self {
        match value {
            FixtureResponse::Json { status, body } | FixtureResponse::Text { status, body } => {
                HttpResponse::new(status, body.into_bytes())
                    .with_header("Content-Type", "application/json")
            }
            FixtureResponse::Failure { .. } => HttpResponse::new(0, Vec::new()),
        }
    }
}

/// What a provider actually sent. Values of credential-bearing headers are
/// replaced with `<redacted>` by `Debug` and by [`CapturedRequest::redacted`];
/// [`CapturedRequest::header`] still returns the real value, because tests need
/// to prove the bearer was attached.
#[derive(Clone)]
pub struct CapturedRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl CapturedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `Authorization: Bearer <token>` returns `Some("Bearer <token>")`.
    pub fn authorization(&self) -> Option<String> {
        self.header("Authorization").map(str::to_string)
    }

    /// `true` when the `Authorization` header contains `prefix` (e.g. `"Bearer "`).
    pub fn sends_authorization_with(&self, prefix: &str) -> bool {
        self.authorization()
            .is_some_and(|value| value.starts_with(prefix))
    }

    pub fn json_body<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(self.body.as_deref().unwrap_or(""))
    }

    /// `true` when the request path (query stripped) ends with `suffix`.
    pub fn path_ends_with(&self, suffix: &str) -> bool {
        let without_query = self.url.split(['?', '#']).next().unwrap_or("");
        without_query.ends_with(suffix)
    }

    /// The headers with credential-bearing values masked — safe to print.
    pub fn redacted(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(name, value)| {
                if is_sensitive_header(name) {
                    (name.clone(), "<redacted>".to_string())
                } else {
                    (name.clone(), value.clone())
                }
            })
            .collect()
    }
}

impl fmt::Debug for CapturedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &self.redacted())
            .field("body", &self.body.as_deref().map(redact_secrets_in_text))
            .finish()
    }
}

fn is_sensitive_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "authorization"
        || lower == "cookie"
        || lower == "x-api-key"
        || lower == "xi-api-key"
        || lower.contains("token")
        || lower.contains("secret")
}

/// An [`HttpClient`] that replays a fixed script and records every request.
///
/// Responses are consumed in order. Running past the end of the script is a
/// programming error in a test, so it returns a distinctive error rather than
/// panicking (the no-panic rule applies to test doubles too — a panic inside a
/// provider would take down the tray app, and we want the surface identical).
pub struct FixtureClient {
    responses: Mutex<VecDeque<FixtureResponse>>,
    captured: Mutex<Vec<CapturedRequest>>,
}

impl FixtureClient {
    pub fn new(responses: Vec<FixtureResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            captured: Mutex::new(Vec::new()),
        }
    }

    /// A client that describes what it would have been sent, without a script —
    /// useful for `notConfigured` tests, where no request may happen at all.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn captured(&self) -> Vec<CapturedRequest> {
        self.captured.lock().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn request_count(&self) -> usize {
        self.captured.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// `true` when the script was fully consumed.
    pub fn exhausted(&self) -> bool {
        self.responses.lock().map(|r| r.is_empty()).unwrap_or(true)
    }
}

impl fmt::Debug for FixtureClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FixtureClient")
            .field("captured", &self.captured())
            .finish()
    }
}

impl HttpClient for FixtureClient {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let captured = CapturedRequest {
            method: request.method,
            url: request.url.clone(),
            headers: request.headers.clone(),
            body: request
                .body
                .as_ref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
        };
        if let Ok(mut recorded) = self.captured.lock() {
            recorded.push(captured);
        }

        let next = self
            .responses
            .lock()
            .ok()
            .and_then(|mut queue| queue.pop_front());

        match next {
            Some(FixtureResponse::Failure { kind, message }) => Err(HttpError::new(kind, message)),
            Some(response) => Ok(response.into()),
            None => Err(HttpError::new(
                HttpErrorKind::InvalidRequest,
                format!(
                    "fixture script exhausted: {} was not scripted (add a FixtureResponse)",
                    request.safe_url()
                ),
            )),
        }
    }
}

/// Build a client that answers the same request repeatedly — handy when a
/// provider retries (MiniMax global → China).
pub fn repeating(responses: Vec<FixtureResponse>, times: usize) -> FixtureClient {
    let mut script = Vec::new();
    for _ in 0..times {
        script.extend(responses.iter().cloned());
    }
    FixtureClient::new(script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::Secret;

    #[test]
    fn fixtures_replay_in_order_and_record_requests() {
        let client = FixtureClient::new(vec![
            FixtureResponse::json(200, r#"{"a":1}"#),
            FixtureResponse::failure(HttpErrorKind::Status, "HTTP 500"),
        ]);

        let first = client
            .execute(&HttpRequest::get("https://example.com/a").bearer(&Secret::new("token-value")))
            .unwrap();
        assert_eq!(first.status, 200);

        let second = client.execute(&HttpRequest::get("https://example.com/b"));
        assert!(second.is_err());

        assert_eq!(client.request_count(), 2);
        assert!(client.exhausted());
        let captured = client.captured();
        assert!(captured[0].sends_authorization_with("Bearer "));
        assert!(captured[0].path_ends_with("/a"));
    }

    #[test]
    fn running_past_the_script_is_an_error_not_a_panic() {
        let client = FixtureClient::empty();
        let err = client
            .execute(&HttpRequest::get("https://example.com"))
            .unwrap_err();
        assert_eq!(err.kind, HttpErrorKind::InvalidRequest);
        assert!(err.message.contains("fixture script exhausted"));
    }

    #[test]
    fn debug_output_never_shows_credentials() {
        let client = FixtureClient::new(vec![FixtureResponse::json(200, "{}")]);
        client
            .execute(
                &HttpRequest::post("https://example.com/token?api_key=leaky")
                    .bearer(&Secret::new("super-secret-token-value")),
            )
            .unwrap();
        let rendered = format!("{:?}", client.captured());
        assert!(!rendered.contains("super-secret-token-value"));
        assert!(rendered.contains("<redacted>"));
    }
}
