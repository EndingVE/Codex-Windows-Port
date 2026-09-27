//! OAuth refresh + the crate's only credential write.
//!
//! `SPEC-flagship.md` §2.2/§3.1 and `SPEC-apikey.md` §1 are explicit: the port
//! **reads** other tools' credential files and must not refresh or rewrite them.
//! This module is the single, deliberately-narrow exception:
//!
//! * [`refresh`] performs an RFC 6749 refresh-token grant against a provider's
//!   own token endpoint. It is called only from an explicit, user-initiated
//!   action, never from the background 60 s refresh tick.
//! * [`write_json_atomic`] persists the result with a `.bak` copy of the previous
//!   file and a temp-file + rename, so an interrupted write can never leave a
//!   half-written credential behind.
//!
//! Everything else in the crate is read-only. Keep it that way.
//!
//! [`TokenCache`] keeps a refreshed access token **in memory** until shortly
//! before it expires (RECON D6), so a provider that reads another CLI's expired
//! session does not hit the token endpoint on every tick. Nothing it holds is
//! ever written to disk: the owning CLI's credential file stays untouched.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::credential::Secret;
use crate::http::{HttpClient, HttpError, HttpRequest};

/// Default deadline for a token exchange.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(20);

/// An RFC 6749 §6 refresh-token request.
#[derive(Debug, Clone)]
pub struct RefreshRequest {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<Secret>,
    pub refresh_token: Secret,
    pub scopes: Vec<String>,
    /// Provider-specific extras (Google's `grant_type` is always added).
    pub extra_params: Vec<(String, String)>,
}

impl RefreshRequest {
    pub fn new(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        refresh_token: Secret,
    ) -> Self {
        Self {
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: None,
            refresh_token,
            scopes: Vec::new(),
            extra_params: Vec::new(),
        }
    }

    pub fn with_client_secret(mut self, secret: Secret) -> Self {
        self.client_secret = Some(secret);
        self
    }

    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scopes.push(scope.into());
        self
    }
}

/// The provider's answer, already normalised.
#[derive(Debug, Clone)]
pub struct RefreshedToken {
    pub access_token: Secret,
    /// `None` when the provider does not rotate refresh tokens.
    pub refresh_token: Option<Secret>,
    /// Absolute expiry, computed from `expires_in` when present.
    pub expires_at: Option<DateTime<Utc>>,
    pub token_type: Option<String>,
    pub scope: Option<String>,
}

impl RefreshedToken {
    /// `true` when the token is missing or inside `skew` of its expiry.
    pub fn is_expired_at(&self, now: DateTime<Utc>, skew: ChronoDuration) -> bool {
        match self.expires_at {
            Some(expiry) => expiry <= now + skew,
            None => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthError {
    /// The provider rejected the grant (`invalid_grant`, revoked, …).
    Rejected { code: String, message: String },
    /// Could not reach or parse the token endpoint.
    Transport(String),
    /// 2xx but no `access_token`.
    Malformed(String),
}

impl OAuthError {
    /// `true` when the refresh token itself is dead (revoked, expired, wrong
    /// client) — retrying cannot help and the user must sign in again with the
    /// CLI that owns the session. Transport failures are *not* re-auth cases.
    pub fn needs_reauth(&self) -> bool {
        match self {
            OAuthError::Rejected { code, .. } => {
                let code = code.to_ascii_lowercase();
                matches!(
                    code.as_str(),
                    "invalid_grant" | "unauthorized_client" | "invalid_client" | "http401"
                ) || code.contains("401")
            }
            _ => false,
        }
    }

    /// Short, actionable text with no secret material in it.
    pub fn user_message(&self) -> String {
        match self {
            OAuthError::Rejected { code, message } => {
                format!("OAuth refresh rejected ({code}): {message}")
            }
            OAuthError::Transport(m) => format!("OAuth refresh could not complete: {m}"),
            OAuthError::Malformed(m) => format!("OAuth refresh returned an unusable token: {m}"),
        }
    }
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for OAuthError {}

/// Exchange a refresh token for a fresh access token.
///
/// Never panics: any failure becomes an [`OAuthError`]. The response body is
/// never embedded in an error — only the provider's `error` /
/// `error_description` fields, which are masked anyway.
pub fn refresh(
    client: &dyn HttpClient,
    request: &RefreshRequest,
    now: DateTime<Utc>,
) -> Result<RefreshedToken, OAuthError> {
    crate::http::ensure_https(&request.token_url).map_err(OAuthError::Transport)?;

    let mut fields: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("client_id", request.client_id.as_str()),
        ("refresh_token", request.refresh_token.expose()),
    ];
    if let Some(secret) = &request.client_secret {
        fields.push(("client_secret", secret.expose()));
    }
    let scope = request.scopes.join(" ");
    if !scope.is_empty() {
        fields.push(("scope", scope.as_str()));
    }
    for (name, value) in &request.extra_params {
        fields.push((name.as_str(), value.as_str()));
    }

    let http_request = HttpRequest::post(request.token_url.clone())
        .accept_json()
        .timeout(REFRESH_TIMEOUT)
        .form_body(&fields);

    let response = client
        .execute(&http_request)
        .map_err(|err| OAuthError::Transport(err.message))?;

    let body: serde_json::Value = match response.json() {
        Ok(value) => value,
        Err(HttpError { message, .. }) => {
            if response.is_success() {
                return Err(OAuthError::Malformed(message));
            }
            return Err(OAuthError::Rejected {
                code: format!("http{}", response.status),
                message,
            });
        }
    };

    if !response.is_success() {
        return Err(OAuthError::Rejected {
            code: body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown_error")
                .to_string(),
            message: crate::credential::redact_secrets_in_text(
                body.get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("no description"),
            ),
        });
    }

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .and_then(crate::credential::cleaned)
        .ok_or_else(|| OAuthError::Malformed("response has no access_token".into()))?;

    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .and_then(crate::credential::cleaned)
        .map(Secret::new);

    let expires_at = body
        .get("expires_in")
        .and_then(|v| v.as_f64())
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map(|secs| now + ChronoDuration::milliseconds((secs * 1000.0) as i64));

    Ok(RefreshedToken {
        access_token: Secret::new(access_token),
        refresh_token,
        expires_at,
        token_type: body
            .get("token_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        scope: body
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

// ---------------------------------------------------------------------------
// In-memory access-token cache (RECON D6)
// ---------------------------------------------------------------------------

/// Safety margin: a cached token is not handed out in its last minute.
pub const TOKEN_CACHE_SKEW: ChronoDuration = ChronoDuration::seconds(60);
/// Lifetime assumed when the token endpoint omits `expires_in`.
pub const TOKEN_CACHE_DEFAULT_TTL: ChronoDuration = ChronoDuration::minutes(5);

enum CacheEntry {
    Token {
        access_token: Secret,
        expires_at: DateTime<Utc>,
    },
    /// The refresh token was rejected (`invalid_grant`, 401, …). Remembered so
    /// the tick does not keep posting a dead grant; a new sign-in changes the
    /// refresh token and therefore the cache key.
    Rejected(String),
}

/// What a cache lookup found.
#[derive(Debug, Clone)]
pub enum CachedToken {
    Fresh(Secret),
    /// The last refresh with this grant needed re-authentication; carries the
    /// user-facing message.
    Rejected(String),
}

/// Process-wide cache of refreshed access tokens, keyed by provider + a hash
/// of the refresh token (the refresh token itself is never stored).
#[derive(Default)]
pub struct TokenCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
}

impl fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenCache(<redacted>)")
    }
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cache key: `<provider>:<hash(refresh_token)>`.
    pub fn key(provider: &str, refresh_token: &Secret) -> String {
        let mut hasher = DefaultHasher::new();
        refresh_token.expose().hash(&mut hasher);
        format!("{provider}:{:016x}", hasher.finish())
    }

    /// A still-valid token (outside [`TOKEN_CACHE_SKEW`] of expiry), or a
    /// remembered rejection.
    pub fn get(&self, key: &str, now: DateTime<Utc>) -> Option<CachedToken> {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        match entries.get(key) {
            Some(CacheEntry::Token {
                access_token,
                expires_at,
            }) => {
                if *expires_at > now + TOKEN_CACHE_SKEW {
                    Some(CachedToken::Fresh(access_token.clone()))
                } else {
                    entries.remove(key);
                    None
                }
            }
            Some(CacheEntry::Rejected(message)) => Some(CachedToken::Rejected(message.clone())),
            None => None,
        }
    }

    /// Remember a successful refresh.
    pub fn store(&self, key: &str, token: &RefreshedToken, now: DateTime<Utc>) {
        let expires_at = token.expires_at.unwrap_or(now + TOKEN_CACHE_DEFAULT_TTL);
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                key.to_string(),
                CacheEntry::Token {
                    access_token: token.access_token.clone(),
                    expires_at,
                },
            );
    }

    /// Remember that this grant needs re-authentication.
    pub fn mark_rejected(&self, key: &str, message: impl Into<String>) {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.to_string(), CacheEntry::Rejected(message.into()));
    }

    /// Drop an entry — e.g. the API answered 401 to a cached token.
    pub fn invalidate(&self, key: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(key);
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The cache every production provider shares (the registry is rebuilt on each
/// tick, so a per-instance cache would never be hit).
pub fn shared_token_cache() -> Arc<TokenCache> {
    static CACHE: OnceLock<Arc<TokenCache>> = OnceLock::new();
    Arc::clone(CACHE.get_or_init(|| Arc::new(TokenCache::new())))
}

/// Atomically write `value` as JSON to `path`, keeping the previous contents at
/// `<path>.bak`.
///
/// Sequence: write `<path>.tmp<pid>` → copy the current file to `<path>.bak` →
/// remove the current file → rename the temp file into place. On Windows a plain
/// rename over an existing file fails, hence the remove; the `.bak` copy is what
/// makes that window recoverable.
///
/// Returns the backup path when a previous file existed.
pub fn write_json_atomic(path: &Path, value: &serde_json::Value) -> io::Result<Option<PathBuf>> {
    let encoded = serde_json::to_vec_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temp = temp_path(path);
    fs::write(&temp, &encoded)?;

    let backup = if path.is_file() {
        let backup = backup_path(path);
        // Best effort: a backup failure must not lose the new token, but the
        // caller is told about it through the returned path + the log line.
        match fs::copy(path, &backup) {
            Ok(_) => Some(backup),
            Err(err) => {
                // Do not abort the refresh because the backup failed; the caller
                // decides whether to surface this.
                eprintln!(
                    "codexbar-providers: could not write backup {}: {err}",
                    backup.display()
                );
                None
            }
        }
    } else {
        None
    };

    if path.is_file() {
        fs::remove_file(path)?;
    }
    fs::rename(&temp, path)?;
    Ok(backup)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}", std::process::id()));
    path.with_file_name(name)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FixtureClient, FixtureResponse};

    fn request() -> RefreshRequest {
        RefreshRequest::new(
            "https://oauth2.example.com/token",
            "client-123",
            Secret::new("old-refresh-token-value"),
        )
        .with_client_secret(Secret::new("client-secret-value"))
    }

    #[test]
    fn refresh_parses_a_successful_grant_without_leaking_it() {
        let client = FixtureClient::new(vec![FixtureResponse::json(
            200,
            r#"{"access_token":"ya29.new-access-token","refresh_token":"new-refresh",
                "expires_in":3600,"token_type":"Bearer","scope":"user:profile"}"#,
        )]);
        let now = Utc::now();
        let token = refresh(&client, &request(), now).unwrap();

        assert_eq!(token.access_token.expose(), "ya29.new-access-token");
        assert_eq!(
            token.refresh_token.as_ref().map(Secret::expose),
            Some("new-refresh")
        );
        assert_eq!(token.scope.as_deref(), Some("user:profile"));
        let expires = token.expires_at.unwrap();
        assert!(
            (expires - (now + ChronoDuration::seconds(3600)))
                .num_seconds()
                .abs()
                <= 1
        );
        assert!(!token.is_expired_at(now, ChronoDuration::minutes(5)));

        // The wire request carries the grant as a form body, and the fixture
        // captures it so the assertion is about the actual bytes sent.
        let captured = client.captured();
        assert_eq!(captured.len(), 1);
        let body = captured[0].body.clone().unwrap_or_default();
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("client_id=client-123"));
        assert!(captured[0].method.as_str() == "POST");
    }

    #[test]
    fn refresh_reports_a_rejection_in_provider_terms() {
        let client = FixtureClient::new(vec![FixtureResponse::json(
            400,
            r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#,
        )]);
        let err = refresh(&client, &request(), Utc::now()).unwrap_err();
        match err {
            OAuthError::Rejected { code, message } => {
                assert_eq!(code, "invalid_grant");
                assert!(message.contains("revoked"));
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn refresh_reports_transport_failures() {
        let client = FixtureClient::new(vec![FixtureResponse::failure(
            crate::http::HttpErrorKind::Timeout,
            "token endpoint timed out",
        )]);
        let err = refresh(&client, &request(), Utc::now()).unwrap_err();
        assert!(matches!(err, OAuthError::Transport(_)));
    }

    #[test]
    fn refresh_refuses_a_plain_http_token_url() {
        let client = FixtureClient::new(vec![]);
        let req = RefreshRequest::new(
            "http://oauth2.example.com/token",
            "client-123",
            Secret::new("refresh"),
        );
        assert!(matches!(
            refresh(&client, &req, Utc::now()).unwrap_err(),
            OAuthError::Transport(_)
        ));
        assert!(client.captured().is_empty(), "no request may be sent");
    }

    #[test]
    fn atomic_write_keeps_a_backup_and_replaces_in_place() {
        let dir = std::env::temp_dir().join(format!("codexbar-oauth-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.json");

        fs::write(&path, r#"{"access_token":"first-token-value"}"#).unwrap();
        let backup = write_json_atomic(&path, &serde_json::json!({"access_token": "second"}))
            .unwrap()
            .expect("a previous file existed, so a backup is expected");

        assert!(backup.is_file());
        let backup_text = fs::read_to_string(&backup).unwrap();
        assert!(backup_text.contains("first-token-value"));

        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["access_token"], "second");

        // No temp file survives a successful write.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must not be left behind");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_creates_a_new_file_without_backup() {
        let dir = std::env::temp_dir().join(format!("codexbar-oauth-new-{}", std::process::id()));
        let path = dir.join("nested/creds.json");
        let backup =
            write_json_atomic(&path, &serde_json::json!({"access_token": "only"})).unwrap();
        assert!(backup.is_none());
        assert!(path.is_file());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reauth_is_detected_for_dead_grants_only() {
        let rejected = |code: &str| OAuthError::Rejected {
            code: code.into(),
            message: "m".into(),
        };
        assert!(rejected("invalid_grant").needs_reauth());
        assert!(rejected("http401").needs_reauth());
        assert!(rejected("unauthorized_client").needs_reauth());
        assert!(!rejected("temporarily_unavailable").needs_reauth());
        assert!(!OAuthError::Transport("timeout".into()).needs_reauth());
    }

    #[test]
    fn the_token_cache_serves_until_the_skew_then_expires() {
        let now = Utc::now();
        let cache = TokenCache::new();
        let key = TokenCache::key("gemini", &Secret::new("refresh-a"));
        assert_ne!(key, TokenCache::key("gemini", &Secret::new("refresh-b")));
        assert!(!key.contains("refresh-a"));
        let token = RefreshedToken {
            access_token: Secret::new("cached-access"),
            refresh_token: None,
            expires_at: Some(now + ChronoDuration::minutes(10)),
            token_type: None,
            scope: None,
        };
        cache.store(&key, &token, now);
        match cache.get(&key, now + ChronoDuration::minutes(5)) {
            Some(CachedToken::Fresh(t)) => assert_eq!(t.expose(), "cached-access"),
            other => panic!("expected a fresh token, got {other:?}"),
        }
        // Inside the last minute the token is no longer handed out.
        assert!(cache
            .get(&key, now + ChronoDuration::seconds(550))
            .is_none());
        assert!(cache.is_empty());

        cache.mark_rejected(&key, "sign in again");
        assert!(matches!(
            cache.get(&key, now),
            Some(CachedToken::Rejected(_))
        ));
        cache.invalidate(&key);
        assert!(cache.get(&key, now).is_none());
        assert!(!format!("{cache:?}").contains("cached-access"));
    }
}
