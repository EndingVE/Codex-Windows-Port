//! **Codex** — the ChatGPT/Codex OAuth provider (flagship, `SPEC-flagship.md` §2).
//!
//! CodexBar does not own this credential: the Codex CLI owns `auth.json` and the
//! refresh grant. The port therefore **reads** the file, **never** writes it, and
//! treats a stale native credential as an honest, actionable state instead of
//! silently refreshing (`SPEC-flagship.md` §2.2, `docs/codex.md`).
//!
//! | Spec | Where |
//! | --- | --- |
//! | Credential file (`%USERPROFILE%\.codex\auth.json` or `%CODEX_HOME%\auth.json`) | [`Codex::load_auth`] |
//! | `chatgpt_base_url` from `config.toml`, HTTPS-only, fail closed | [`Codex::base_url`] |
//! | `GET {base}/wham/usage` (or `/api/codex/usage` when the base lacks `/backend-api`) | [`Codex::fetch_usage`] |
//! | Best-effort `GET {base}/wham/rate-limit-reset-credits` | [`Codex::fetch_reset_credits`] |
//! | `rate_limit.primary_window` → session, `secondary_window` → weekly | [`normalize`] |
//! | `additional_rate_limits[]` → named extra windows | [`additional_windows`] |
//! | Tolerant, per-window decoding: one malformed window never discards its siblings | [`parse_window`], [`parse_usage`] |
//! | JWT `exp` as an integer in the Chrono range, else the 8-day `last_refresh` age rule | [`jwt_expiration`], [`Credentials::needs_refresh`] |
//! | Stale native credential → honest `nativeRefreshRequired` error, **no refresh, no write** | [`Codex::fetch`] |
//!
//! Deliberately **not** implemented (documented, not guessed): the CLI `app-server`
//! RPC path of §2.5 (it needs Windows child-process lifecycle management and is a
//! diagnostic source, not the default), and the monthly spend-controls endpoint of
//! §2.3. The frozen `codexbar-core` contract has no lane for reset-credit
//! *inventory*, so the best-effort fetch is used for its request/account context
//! and for a soft-degrade note only; it publishes nothing.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use codexbar_core::{
    DataSource, FetchStatus, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    WindowKind,
};
use serde_json::Value;

use crate::credential::{home_dir, read_json, CredentialError, Env, Secret};
use crate::http::{self, secure_base_url, HttpClient, HttpError, HttpRequest};

/// Alternate Codex home. An explicit `CODEX_HOME` is isolated: it never falls
/// back to the ambient `~/.codex` (`SPEC-flagship.md` §2.1).
pub const ENV_CODEX_HOME: &str = "CODEX_HOME";
/// Default ChatGPT base used when `config.toml` does not override it.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

const USAGE_PATH_BACKEND: &str = "/wham/usage";
const USAGE_PATH_CODEX: &str = "/api/codex/usage";
const RESET_CREDITS_PATH: &str = "/wham/rate-limit-reset-credits";

/// The CLI's own User-Agent value (`SPEC-flagship.md` §2.3).
const CODEX_USER_AGENT: &str = "CodexBar";

/// Native credentials are considered stale five minutes before `exp`.
const NATIVE_REFRESH_WINDOW_SECS: i64 = 5 * 60;
/// Age fallback when the access token carries no usable `exp`.
const AGE_FALLBACK_SECS: i64 = 8 * 24 * 60 * 60;

/// `wham/usage` is the primary call; give it the CLI's own 30 s budget.
const USAGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Reset-credit inventory is best-effort and must not hold up the refresh tick.
const RESET_CREDITS_TIMEOUT: Duration = Duration::from_secs(4);

const ACCOUNT_HEADER_USAGE: &str = "ChatGPT-Account-Id";
/// The reset-credits endpoint spells the header in caps.
const ACCOUNT_HEADER_RESET: &str = "ChatGPT-Account-ID";

/// How many leading characters of the account id stay visible when it is masked
/// for display (`abcd1234-…` → `abcd1234…`). Enough to tell two accounts apart in
/// a screenshot, never enough to reconstruct the UUID (which is a stable account
/// identifier the port should not publish — `evidence/v2-live-report.json`).
const ACCOUNT_ID_VISIBLE: usize = 8;

/// The `exp` values Chrono 0.4 can represent as UTC seconds (`SPEC-flagship.md`
/// §2.2). Anything outside this range falls back to the `last_refresh` age rule.
const JWT_EXP_SECONDS_RANGE: std::ops::RangeInclusive<i64> = -8_334_601_228_800..=8_210_266_876_799;

/// Where a fetch reads `auth.json` / `config.toml` from.
///
/// Production reads the filesystem; tests inject the exact file contents so the
/// suite never touches the user's real Codex home and never needs a socket.
enum AuthSource {
    Disk,
    Injected {
        auth_json: Option<String>,
        config_toml: Option<String>,
    },
}

pub struct Codex {
    client: Arc<dyn HttpClient>,
    env: Env,
    auth: AuthSource,
}

impl Codex {
    /// Production constructor: real HTTPS client, process environment, the real
    /// Codex home.
    pub fn new() -> Self {
        Self {
            client: http::shared_client(),
            env: Env::from_process(),
            auth: AuthSource::Disk,
        }
    }

    /// Test/embedding constructor: an injected client and environment, still
    /// reading the real Codex home (used by `live_registry`).
    pub fn with_client(client: Arc<dyn HttpClient>, env: Env) -> Self {
        Self {
            client,
            env,
            auth: AuthSource::Disk,
        }
    }

    /// Test constructor: inject the exact contents of `auth.json` and
    /// `config.toml`. `None` means "that file does not exist" — no disk at all.
    pub fn with_injected(
        client: Arc<dyn HttpClient>,
        env: Env,
        auth_json: Option<String>,
        config_toml: Option<String>,
    ) -> Self {
        Self {
            client,
            env,
            auth: AuthSource::Injected {
                auth_json,
                config_toml,
            },
        }
    }

    /// The Codex home this fetch would read from.
    fn codex_home(&self) -> Option<PathBuf> {
        if let Some(configured) = self.env.get_str(ENV_CODEX_HOME) {
            let path = PathBuf::from(configured);
            if !path.as_os_str().is_empty() {
                return Some(path);
            }
        }
        home_dir().map(|home| home.join(".codex"))
    }

    /// Path shown in error messages. Never a credential, just a hint.
    fn auth_path_display(&self) -> String {
        match self.codex_home() {
            Some(home) => home.join("auth.json").to_string_lossy().into_owned(),
            None => "%USERPROFILE%\\.codex\\auth.json".to_string(),
        }
    }

    /// Read (never write) `auth.json`.
    fn load_auth(&self) -> Result<Value, AuthError> {
        match &self.auth {
            AuthSource::Disk => match self.codex_home() {
                None => Err(AuthError::Missing),
                Some(home) => match read_json(&home.join("auth.json")) {
                    Ok(value) => Ok(value),
                    Err(CredentialError::Missing(_)) => Err(AuthError::Missing),
                    Err(CredentialError::Io(message))
                    | Err(CredentialError::Parse(message))
                    | Err(CredentialError::Schema(message)) => Err(AuthError::Unreadable(message)),
                },
            },
            AuthSource::Injected { auth_json, .. } => match auth_json {
                None => Err(AuthError::Missing),
                Some(text) => serde_json::from_str(text).map_err(|err| {
                    AuthError::Unreadable(format!("Codex auth.json is not valid JSON ({err})"))
                }),
            },
        }
    }

    /// Read (never write) `config.toml`, when the Codex home has one.
    fn config_toml(&self) -> Option<String> {
        match &self.auth {
            AuthSource::Injected { config_toml, .. } => config_toml.clone(),
            AuthSource::Disk => {
                let home = self.codex_home()?;
                std::fs::read_to_string(home.join("config.toml")).ok()
            }
        }
    }

    /// The ChatGPT base URL, after `config.toml` and the HTTPS-only policy.
    fn base_url(&self) -> Result<String, String> {
        let configured = self
            .config_toml()
            .and_then(|contents| parse_chatgpt_base_url(&contents));
        let secure = secure_base_url(configured.as_deref(), DEFAULT_BASE_URL)?;
        Ok(normalize_base_url(&secure))
    }

    /// `GET {base}/wham/usage`, with the Codex CLI's own headers.
    fn fetch_usage(&self, base: &str, credentials: &Credentials) -> Result<Usage, HttpError> {
        let mut request = HttpRequest::get(usage_url(base))
            .bearer(&credentials.access_token)
            .accept_json()
            .header("User-Agent", CODEX_USER_AGENT)
            .timeout(USAGE_TIMEOUT);
        if let Some(account_id) = credentials.account_id.as_deref() {
            request = request.header(ACCOUNT_HEADER_USAGE, account_id);
        }

        let response = self.client.send_ok(&request)?;
        let body: Value = response.json()?;
        parse_usage(&body).map_err(HttpError::decode)
    }

    /// Best-effort reset-credit inventory. Its failure is diagnostic only.
    fn fetch_reset_credits(
        &self,
        base: &str,
        credentials: &Credentials,
    ) -> Result<Option<i64>, HttpError> {
        let mut request = HttpRequest::get(format!("{base}{RESET_CREDITS_PATH}"))
            .bearer(&credentials.access_token)
            .accept_json()
            .header("User-Agent", CODEX_USER_AGENT)
            .header("OpenAI-Beta", "codex-1")
            .header("originator", "Codex Desktop")
            .timeout(RESET_CREDITS_TIMEOUT);
        if let Some(account_id) = credentials.account_id.as_deref() {
            request = request.header(ACCOUNT_HEADER_RESET, account_id);
        }

        let response = self.client.send_ok(&request)?;
        let body: Value = response.json()?;
        Ok(available_reset_credits(&body))
    }
}

impl Default for Codex {
    fn default() -> Self {
        Self::new()
    }
}

/// Never print the credential material the struct holds.
impl std::fmt::Debug for Codex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Codex")
            .field("codex_home", &self.codex_home())
            .field(
                "injected",
                &matches!(self.auth, AuthSource::Injected { .. }),
            )
            .finish()
    }
}

impl Provider for Codex {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::Codex,
            title: ProviderId::Codex.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            // Credential-file provider: the numbers come from an OAuth bearer.
            source: DataSource::OAuth,
            fetched_at: now,
        };

        // 1. Credentials. A missing file is `notConfigured` (an actionable setup
        //    hint); a present-but-unusable file is an error.
        let json = match self.load_auth() {
            Ok(json) => json,
            Err(AuthError::Missing) => {
                snapshot.status = FetchStatus::NotConfigured;
                snapshot.error = Some(format!(
                    "No Codex credentials at {}. Run `codex login` to sign in.",
                    self.auth_path_display()
                ));
                return snapshot;
            }
            Err(AuthError::Unreadable(message)) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!("{message}. Run `codex login` to sign in again."));
                return snapshot;
            }
            // `load_auth` walks the file's shape, not its tokens, so this arm is
            // unreachable — kept explicit rather than a wildcard so a new variant
            // is a compile error, not a silent success.
            Err(AuthError::MissingTokens) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!(
                    "{} contains no Codex OAuth tokens. Run `codex login` to sign in.",
                    self.auth_path_display()
                ));
                return snapshot;
            }
        };

        let credentials = match parse_credentials(&json) {
            Ok(credentials) => credentials,
            Err(AuthError::MissingTokens) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!(
                    "{} exists but contains no Codex OAuth tokens. Run `codex login` to sign in.",
                    self.auth_path_display()
                ));
                return snapshot;
            }
            Err(AuthError::Missing) => {
                snapshot.status = FetchStatus::NotConfigured;
                snapshot.error = Some(format!(
                    "No Codex credentials at {}. Run `codex login` to sign in.",
                    self.auth_path_display()
                ));
                return snapshot;
            }
            Err(AuthError::Unreadable(message)) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(message);
                return snapshot;
            }
        };

        // Identity comes from the same credential file as the numbers and is
        // never a token: `account_id` is the UUID the CLI sends in a header. It
        // is a stable account identifier, so it is **masked at this boundary** —
        // the raw UUID must never reach a snapshot, a log or a screenshot (it
        // did leak into `evidence/v2-live-report.json`). An email, when the
        // credential carries one, is preferred over the opaque UUID and is
        // masked the same way (`user@…`): the full address is PII.
        snapshot.account = account_label(&credentials);

        // 2. Freshness. The CLI owns the refresh grant; the port is read-only.
        //    A stale native credential is an honest, actionable state.
        if credentials.needs_refresh(now) {
            snapshot.status = FetchStatus::Error;
            snapshot.error = Some(format!(
                "Codex OAuth token expired or stale — this port never refreshes or rewrites {}. \
                 Run `codex login` in the same Codex home to re-authenticate.",
                self.auth_path_display()
            ));
            return snapshot;
        }

        // 3. Endpoint policy — fail closed *before* the bearer is attached.
        let base = match self.base_url() {
            Ok(base) => base,
            Err(reason) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!("chatgpt_base_url is not usable: {reason}"));
                return snapshot;
            }
        };

        // 4. Usage. A failure here is terminal for the card.
        let usage = match self.fetch_usage(&base, &credentials) {
            Ok(usage) => usage,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(err.message);
                return snapshot;
            }
        };

        // The usage body repeats the account id; mask it with the same rule so a
        // response-only identity can never smuggle the raw UUID into the card.
        snapshot.account = snapshot
            .account
            .take()
            .or_else(|| usage.account_id.as_deref().map(masked_account_id));
        snapshot.plan = usage.plan.clone();
        snapshot.windows = usage.windows;

        // 5. Optional: reset-credit inventory, best-effort. The frozen contract
        //    has no lane for it yet, so its only effect is a diagnostic note.
        let mut notes: Vec<String> = Vec::new();
        if let Err(err) = self.fetch_reset_credits(&base, &credentials) {
            notes.push(format!(
                "Rate-limit reset credits unavailable right now ({})",
                err.kind.as_str()
            ));
        }
        if !notes.is_empty() {
            // Status stays `ok`: the quots above are real; the note is diagnostic.
            snapshot.error = Some(notes.join("; "));
        }

        snapshot
    }
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthError {
    /// No `auth.json` at all — the user simply has not run `codex login`.
    Missing,
    /// Present but unreadable or not the expected JSON.
    Unreadable(String),
    /// Parsed, but carries no OAuth token.
    MissingTokens,
}

/// The read-only view of `auth.json` the fetcher needs.
///
/// The refresh and id tokens are deliberately **not** kept: the port never
/// refreshes, `account_id` has already been recovered from the JWT claims, and a
/// credential value that is never retained cannot leak.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Credentials {
    access_token: Secret,
    account_id: Option<String>,
    /// A human-meaningful identity, when the token carries one (`email` claim).
    /// Preferred over the opaque `account_id` for the snapshot's `account`.
    email: Option<String>,
    last_refresh: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
}

impl Credentials {
    /// `SPEC-flagship.md` §2.2: within five minutes of `exp` for native
    /// credentials; the 8-day `last_refresh` age rule when `exp` is unusable.
    fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        if let Some(expires_at) = self.expires_at {
            return expires_at <= now + ChronoDuration::seconds(NATIVE_REFRESH_WINDOW_SECS);
        }
        match self.last_refresh {
            Some(last_refresh) => {
                now.signed_duration_since(last_refresh) > ChronoDuration::seconds(AGE_FALLBACK_SECS)
            }
            None => true,
        }
    }
}

/// Parse `auth.json`. `tokens.access_token` is required; `account_id` falls back
/// to the JWT claims the way the CLI does.
fn parse_credentials(json: &Value) -> Result<Credentials, AuthError> {
    let Some(tokens) = json.get("tokens").and_then(Value::as_object) else {
        return Err(AuthError::MissingTokens);
    };
    let Some(access_token) = string_value(tokens, "access_token", "accessToken") else {
        return Err(AuthError::MissingTokens);
    };
    let id_token = string_value(tokens, "id_token", "idToken");

    let account_id = string_value(tokens, "account_id", "accountId")
        .or_else(|| account_id_from_jwt(id_token.as_deref(), &access_token));
    let email = string_value(tokens, "email", "email")
        .or_else(|| email_from_jwt(id_token.as_deref(), &access_token));

    Ok(Credentials {
        expires_at: jwt_expiration(&access_token),
        access_token: Secret::new(access_token),
        account_id,
        email,
        last_refresh: parse_last_refresh(&json["last_refresh"]),
    })
}

/// The display identity for the card: the credential's email when it carries one
/// (`user@…`), otherwise the account UUID masked to its 8-character prefix
/// (`abcd1234…`) — never the raw value of either. Both are stable account
/// identifiers, so both follow the masking policy: the raw UUID leaked once
/// (`evidence/v2-live-report.json`) and a full address is contactable PII.
fn account_label(credentials: &Credentials) -> Option<String> {
    credentials
        .email
        .as_deref()
        .map(crate::credential::mask_email)
        .or_else(|| credentials.account_id.as_deref().map(masked_account_id))
}

/// `abcd1234-5678-4abc-9def-0123456789ab` → `abcd1234…`.
///
/// Follows the same policy as `Secret::redacted` (and the redaction used by the
/// other providers): a stable, recognisable stub, never the whole value. A value
/// short enough to be fully exposed by its prefix collapses to `…`, matching
/// `credential::redact`.
fn masked_account_id(value: &str) -> String {
    let trimmed = value.trim();
    let mut chars = trimmed.chars();
    let head: String = chars.by_ref().take(ACCOUNT_ID_VISIBLE).collect();
    if chars.next().is_none() {
        return "…".to_string();
    }
    format!("{head}…")
}

/// `snake_case` first, then `camelCase`; empty is absent, like the CLI.
fn string_value(
    tokens: &serde_json::Map<String, Value>,
    snake_case: &str,
    camel_case: &str,
) -> Option<String> {
    for key in [snake_case, camel_case] {
        if let Some(value) = tokens.get(key).and_then(Value::as_str).and_then(non_empty) {
            return Some(value.to_string());
        }
    }
    None
}

/// `last_refresh`, accepting ISO-8601 with or without fractional seconds.
fn parse_last_refresh(raw: &Value) -> Option<DateTime<Utc>> {
    let value = raw.as_str().and_then(non_empty)?;
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

// ---------------------------------------------------------------------------
// JWT — best-effort scheduling hints only
// ---------------------------------------------------------------------------

/// `exp` as an integer in the Chrono range. Booleans, strings, fractions,
/// exponent spellings and out-of-range integers all fall back to the age rule.
fn jwt_expiration(access_token: &str) -> Option<DateTime<Utc>> {
    let payload = jwt_payload(access_token)?;
    let seconds = payload.get("exp")?.as_i64()?;
    if !JWT_EXP_SECONDS_RANGE.contains(&seconds) {
        return None;
    }
    DateTime::from_timestamp(seconds, 0)
}

/// OpenAI also carries the account identity in JWT claims; recover it without
/// treating a malformed or opaque token as a credential-read failure.
fn account_id_from_jwt(id_token: Option<&str>, access_token: &str) -> Option<String> {
    for token in id_token.into_iter().chain(std::iter::once(access_token)) {
        if let Some(account_id) = jwt_account_id(token) {
            return Some(account_id);
        }
    }
    None
}

/// The `email` claim, when the token carries one. Same scan order as the account
/// id; a credential without one is not an error, the UUID mask is used instead.
fn email_from_jwt(id_token: Option<&str>, access_token: &str) -> Option<String> {
    for token in id_token.into_iter().chain(std::iter::once(access_token)) {
        if let Some(email) = jwt_email(token) {
            return Some(email);
        }
    }
    None
}

fn jwt_email(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    if let Some(email) = payload.get("email").and_then(Value::as_str) {
        return non_empty(email).map(str::to_string);
    }
    payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("email"))
        .and_then(Value::as_str)
        .and_then(non_empty)
        .map(str::to_string)
}

fn jwt_account_id(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    if let Some(id) = payload.get("chatgpt_account_id").and_then(Value::as_str) {
        return non_empty(id).map(str::to_string);
    }
    if let Some(id) = payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
    {
        return non_empty(id).map(str::to_string);
    }
    if let Some(organizations) = payload.get("organizations").and_then(Value::as_array) {
        for organization in organizations {
            if let Some(id) = organization.get("id").and_then(Value::as_str) {
                if let Some(id) = non_empty(id) {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

/// The JSON payload of a three-part JWT, or `None` for anything else.
fn jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let signature = parts.next()?;
    if parts.next().is_some() || header.is_empty() || payload.is_empty() || signature.is_empty() {
        return None;
    }
    let decoded = base64url_decode(payload)?;
    serde_json::from_slice(&decoded).ok()
}

/// Minimal unpadded base64url decoder (no dependency; JWT payloads only).
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// config.toml / endpoint policy
// ---------------------------------------------------------------------------

/// `chatgpt_base_url = "..."` (quotes optional) from a `config.toml`, comments
/// and all. The first assignment wins, matching the CLI.
fn parse_chatgpt_base_url(contents: &str) -> Option<String> {
    for raw_line in contents.lines() {
        let line = raw_line.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() != "chatgpt_base_url" {
            continue;
        }
        return Some(strip_quotes(value.trim()).trim().to_string());
    }
    None
}

fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// The CLI appends `/backend-api` to a bare `chatgpt.com` / `chat.openai.com`
/// base, and strips trailing slashes.
fn normalize_base_url(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return DEFAULT_BASE_URL.to_string();
    }
    if (trimmed.starts_with("https://chatgpt.com")
        || trimmed.starts_with("https://chat.openai.com"))
        && !trimmed.contains("/backend-api")
    {
        return format!("{trimmed}/backend-api");
    }
    trimmed.to_string()
}

/// A base that mentions `/backend-api` uses the `wham` path; anything else uses
/// the Codex-style `/api/codex/usage`.
fn usage_url(base: &str) -> String {
    let path = if base.contains("/backend-api") {
        USAGE_PATH_BACKEND
    } else {
        USAGE_PATH_CODEX
    };
    format!("{base}{path}")
}

// ---------------------------------------------------------------------------
// Usage decoding — tolerant, per window
// ---------------------------------------------------------------------------

/// One decoded `*_window`. `used_percent` is the only required field, so a
/// sibling's malformed value cannot discard a valid window.
#[derive(Debug, Clone, PartialEq)]
struct WindowSnapshot {
    used_percent: f64,
    window_minutes: Option<i64>,
    resets_at: Option<DateTime<Utc>>,
}

/// The decoded `wham/usage` body, already mapped to the contract's lanes.
#[derive(Debug, Clone, Default)]
struct Usage {
    account_id: Option<String>,
    plan: Option<String>,
    windows: Vec<NamedRateWindow>,
}

fn parse_usage(body: &Value) -> Result<Usage, String> {
    let rate_limit = body.get("rate_limit");
    let primary = rate_limit
        .and_then(|details| details.get("primary_window"))
        .and_then(parse_window);
    let secondary = rate_limit
        .and_then(|details| details.get("secondary_window"))
        .and_then(parse_window);
    let (session, weekly) = normalize(primary, secondary);

    let mut windows = Vec::new();
    if let Some(session) = session.as_ref() {
        windows.push(session_window(session));
    }
    if let Some(weekly) = weekly.as_ref() {
        windows.push(weekly_window(weekly));
    }
    windows.extend(additional_windows(body));

    let plan = body
        .get("plan_type")
        .and_then(Value::as_str)
        .and_then(non_empty)
        .map(plan_label);
    let account_id = body
        .get("account_id")
        .or_else(|| body.get("accountId"))
        .and_then(Value::as_str)
        .and_then(non_empty)
        .map(str::to_string);

    if windows.is_empty() && plan.is_none() {
        return Err("Codex usage response had no rate_limit windows or plan_type".to_string());
    }

    Ok(Usage {
        account_id,
        plan,
        windows,
    })
}

/// Decode one `*_window`. Never throws: a malformed window is simply absent.
fn parse_window(value: &Value) -> Option<WindowSnapshot> {
    let used_percent = value.get("used_percent").and_then(finite_number)?;
    let resets_at = value
        .get("reset_at")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
    let window_minutes = value
        .get("limit_window_seconds")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .map(|seconds| seconds / 60);
    Some(WindowSnapshot {
        used_percent,
        window_minutes,
        resets_at,
    })
}

fn finite_number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

/// Port of `CodexRateWindowNormalizer`: the API's `primary`/`secondary` slots are
/// not guaranteed to be the session/weekly lanes, so classify by `window_minutes`
/// (300 = 5 h, 10080 = 7 d) and swap when needed.
fn normalize(
    primary: Option<WindowSnapshot>,
    secondary: Option<WindowSnapshot>,
) -> (Option<WindowSnapshot>, Option<WindowSnapshot>) {
    match (primary, secondary) {
        (Some(primary), Some(secondary)) => match (role(&primary), role(&secondary)) {
            (Role::Weekly, Role::Session) | (Role::Weekly, Role::Unknown) => {
                (Some(secondary), Some(primary))
            }
            _ => (Some(primary), Some(secondary)),
        },
        (Some(primary), None) => match role(&primary) {
            Role::Weekly => (None, Some(primary)),
            Role::Session | Role::Unknown => (Some(primary), None),
        },
        (None, Some(secondary)) => match role(&secondary) {
            Role::Session | Role::Unknown => (Some(secondary), None),
            Role::Weekly => (None, Some(secondary)),
        },
        (None, None) => (None, None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Session,
    Weekly,
    Unknown,
}

fn role(window: &WindowSnapshot) -> Role {
    match window.window_minutes {
        Some(300) => Role::Session,
        Some(10_080) => Role::Weekly,
        _ => Role::Unknown,
    }
}

fn session_window(snapshot: &WindowSnapshot) -> NamedRateWindow {
    NamedRateWindow::new(
        "session",
        "Session · 5h",
        WindowKind::Session,
        RateWindow::new(
            snapshot.used_percent,
            snapshot.window_minutes.or(Some(300)),
            snapshot.resets_at,
        ),
    )
}

fn weekly_window(snapshot: &WindowSnapshot) -> NamedRateWindow {
    NamedRateWindow::new(
        "weekly",
        "Weekly · 7d",
        WindowKind::Weekly,
        RateWindow::new(
            snapshot.used_percent,
            snapshot.window_minutes.or(Some(10_080)),
            snapshot.resets_at,
        ),
    )
}

/// `additional_rate_limits[]` → named extra lanes. Each element is decoded on its
/// own, so one malformed entry cannot discard its valid siblings.
fn additional_windows(body: &Value) -> Vec<NamedRateWindow> {
    let Some(entries) = body.get("additional_rate_limits").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut used_ids: BTreeSet<String> = BTreeSet::new();
    let mut windows = Vec::new();
    for entry in entries {
        windows.extend(entry_windows(entry, &mut used_ids));
    }
    windows
}

fn entry_windows(entry: &Value, used_ids: &mut BTreeSet<String>) -> Vec<NamedRateWindow> {
    let rate_limit = entry.get("rate_limit");
    let primary = rate_limit
        .and_then(|details| details.get("primary_window"))
        .and_then(parse_window);
    let secondary = rate_limit
        .and_then(|details| details.get("secondary_window"))
        .and_then(parse_window);
    let limit_name = entry
        .get("limit_name")
        .and_then(Value::as_str)
        .and_then(non_empty);
    let metered_feature = entry
        .get("metered_feature")
        .and_then(Value::as_str)
        .and_then(non_empty);

    let is_spark = [limit_name, metered_feature]
        .into_iter()
        .flatten()
        .any(|value| value.to_ascii_lowercase().contains("spark"));

    if is_spark {
        let mut windows = Vec::new();
        for (snapshot, fallback) in [
            (primary, SparkWindow::FiveHour),
            (secondary, SparkWindow::Weekly),
        ] {
            let Some(snapshot) = snapshot else { continue };
            let kind = spark_window(&snapshot, fallback);
            if used_ids.insert(kind.id().to_string()) {
                windows.push(named_extra(kind.id(), kind.title(), &snapshot));
            }
        }
        return windows;
    }

    let Some(snapshot) = primary.or(secondary) else {
        return Vec::new();
    };
    let Some(source) = metered_feature.or(limit_name) else {
        return Vec::new();
    };
    let slug = slugify(source);
    if slug.is_empty() {
        return Vec::new();
    }
    let id = format!("codex-{slug}");
    if !used_ids.insert(id.clone()) {
        return Vec::new();
    }
    let title = limit_name
        .or(metered_feature)
        .unwrap_or("Codex extra limit");
    vec![named_extra(id, title, &snapshot)]
}

fn named_extra(
    id: impl Into<String>,
    title: impl Into<String>,
    snapshot: &WindowSnapshot,
) -> NamedRateWindow {
    NamedRateWindow::new(
        id,
        title,
        WindowKind::Extra,
        RateWindow::new(
            snapshot.used_percent,
            snapshot.window_minutes,
            snapshot.resets_at,
        ),
    )
}

#[derive(Debug, Clone, Copy)]
enum SparkWindow {
    FiveHour,
    Weekly,
}

impl SparkWindow {
    const fn id(self) -> &'static str {
        match self {
            SparkWindow::FiveHour => "codex-spark",
            SparkWindow::Weekly => "codex-spark-weekly",
        }
    }

    const fn title(self) -> &'static str {
        match self {
            SparkWindow::FiveHour => "Codex Spark 5-hour",
            SparkWindow::Weekly => "Codex Spark Weekly",
        }
    }
}

fn spark_window(snapshot: &WindowSnapshot, fallback: SparkWindow) -> SparkWindow {
    match snapshot.window_minutes {
        Some(minutes) if minutes > 0 && minutes <= 360 => SparkWindow::FiveHour,
        Some(minutes) if minutes >= 6 * 24 * 60 => SparkWindow::Weekly,
        _ => fallback,
    }
}

/// Stable id slug: lowercase, non-alphanumerics collapse to a single dash.
fn slugify(value: &str) -> String {
    let mut result = String::new();
    let mut last_was_dash = false;
    for character in value.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character);
            last_was_dash = false;
        } else if !last_was_dash {
            result.push('-');
            last_was_dash = true;
        }
    }
    result.trim_matches('-').to_string()
}

/// `plan_type` → the app's display label (`CodexProviderDescriptor.sharePlanLabels`).
fn plan_label(raw: &str) -> String {
    let label = match raw.trim().to_ascii_lowercase().as_str() {
        "guest" => "Guest",
        "free" => "Free",
        "go" => "Go",
        "plus" => "Plus",
        "pro" => "Pro 20x",
        "prolite" | "pro_lite" | "pro-lite" | "pro lite" => "Pro 5x",
        "free_workspace" => "Free Workspace",
        "team" => "Team",
        "business" => "Business",
        "education" => "Education",
        "quorum" => "Quorum",
        "k12" => "K12",
        "enterprise" => "Enterprise",
        "edu" => "Edu",
        _ => return raw.trim().to_string(),
    };
    label.to_string()
}

/// The reset-credit inventory count, when the body carries a usable one.
fn available_reset_credits(body: &Value) -> Option<i64> {
    body.get("available_count")
        .and_then(Value::as_i64)
        .filter(|count| *count >= 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
    }

    fn auth_json(access_token: &str, last_refresh: &str) -> String {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access_token,
                "account_id": "acct-EXAMPLE-0001",
            },
            "last_refresh": last_refresh,
        })
        .to_string()
    }

    fn encode(payload: &serde_json::Value) -> String {
        // Only used to build synthetic JWTs in tests.
        let header = base64url_encode(b"{}");
        let body = base64url_encode(payload.to_string().as_bytes());
        format!("{header}.{body}.sig")
    }

    fn base64url_encode(input: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let mut buffer = 0u32;
            for (index, byte) in chunk.iter().enumerate() {
                buffer |= u32::from(*byte) << (16 - 8 * index);
            }
            for index in 0..=chunk.len() {
                if index > 0 && index * 6 > chunk.len() * 8 {
                    break;
                }
                out.push(ALPHABET[((buffer >> (18 - 6 * index)) & 0x3f) as usize] as char);
            }
        }
        out
    }

    #[test]
    fn integer_exp_is_read_in_the_chrono_range() {
        let token = encode(&serde_json::json!({"exp": 4_102_444_800_i64}));
        assert_eq!(
            jwt_expiration(&token),
            DateTime::from_timestamp(4_102_444_800, 0)
        );
        // …and strings, fractions, exponent spellings and out-of-range integers
        // all fall back instead of producing a bogus expiry.
        assert_eq!(
            jwt_expiration(&encode(&serde_json::json!({"exp": "4102444800"}))),
            None
        );
        assert_eq!(
            jwt_expiration(&encode(&serde_json::json!({"exp": 4_102_444_800.0}))),
            None
        );
        assert_eq!(
            jwt_expiration(&encode(
                &serde_json::json!({"exp": 99_999_999_999_999_999_i64})
            )),
            None
        );
        assert_eq!(
            jwt_expiration(&encode(&serde_json::json!({"sub": "x"}))),
            None
        );
        assert_eq!(jwt_expiration("not-a-jwt"), None);
        assert_eq!(jwt_expiration("a.b"), None);
        assert_eq!(jwt_expiration(".."), None);
    }

    #[test]
    fn expiration_falls_back_to_the_eight_day_age_rule() {
        let fresh = parse_credentials(
            &serde_json::from_str(&auth_json("not-a-jwt", "2026-09-09T00:00:00Z")).unwrap(),
        )
        .unwrap();
        assert!(fresh.expires_at.is_none());
        assert!(!fresh.needs_refresh(now()), "within the 8-day window");

        let stale = parse_credentials(
            &serde_json::from_str(&auth_json("not-a-jwt", "2026-08-01T00:00:00Z")).unwrap(),
        )
        .unwrap();
        assert!(stale.needs_refresh(now()), "older than 8 days");

        let never = parse_credentials(
            &serde_json::from_str(r#"{"tokens":{"access_token":"not-a-jwt"}}"#).unwrap(),
        )
        .unwrap();
        assert!(
            never.needs_refresh(now()),
            "no expiry and no last_refresh is not knowable, so refresh is required"
        );
    }

    #[test]
    fn a_native_token_is_stale_five_minutes_before_expiry() {
        // `now` is 2026-09-10T20:00:00Z = 1_789_070_400.
        let token = encode(&serde_json::json!({"exp": 1_789_070_500_i64}));
        let credentials = parse_credentials(
            &serde_json::from_str(&auth_json(&token, "2026-09-10T00:00:00Z")).unwrap(),
        )
        .unwrap();
        // 100 s of headroom → inside the five-minute refresh window.
        assert!(credentials.needs_refresh(now()));
        let healthy = encode(&serde_json::json!({"exp": 1_789_074_000_i64}));
        let credentials = parse_credentials(
            &serde_json::from_str(&auth_json(&healthy, "2026-09-10T00:00:00Z")).unwrap(),
        )
        .unwrap();
        assert!(!credentials.needs_refresh(now()));
    }

    #[test]
    fn account_id_falls_back_to_jwt_claims() {
        let token = encode(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-from-claim"}
        }));
        assert_eq!(
            account_id_from_jwt(None, &token).as_deref(),
            Some("acct-from-claim")
        );
        let organizations = encode(&serde_json::json!({
            "organizations": [{"id": "org-1"}, {"id": "org-2"}]
        }));
        assert_eq!(
            account_id_from_jwt(None, &organizations).as_deref(),
            Some("org-1")
        );
        assert_eq!(account_id_from_jwt(None, "opaque"), None);
    }

    #[test]
    fn the_account_id_is_masked_to_its_visible_prefix() {
        assert_eq!(
            masked_account_id("abcd1234-5678-4abc-9def-0123456789ab"),
            "abcd1234…"
        );
        // A value fully exposed by its prefix (or shorter) collapses to the same
        // placeholder `credential::redact` uses — nothing to hide behind.
        assert_eq!(masked_account_id("short"), "…");
        assert_eq!(masked_account_id("12345678"), "…");
        assert_eq!(masked_account_id(""), "…");
    }

    #[test]
    fn an_email_identity_is_preferred_over_the_masked_uuid() {
        let credentials = Credentials {
            access_token: Secret::new("token"),
            account_id: Some("abcd1234-5678-4abc-9def-0123456789ab".to_string()),
            email: Some("user@example.com".to_string()),
            last_refresh: None,
            expires_at: None,
        };
        assert_eq!(
            account_label(&credentials).as_deref(),
            Some("user@…"),
            "the email identity is masked, not printed whole"
        );

        let credentials = Credentials {
            email: None,
            ..credentials
        };
        assert_eq!(account_label(&credentials).as_deref(), Some("abcd1234…"));
    }

    #[test]
    fn an_email_claim_is_recovered_from_the_jwt() {
        let token = encode(&serde_json::json!({"email": "user@example.com"}));
        assert_eq!(
            email_from_jwt(None, &token).as_deref(),
            Some("user@example.com")
        );
        let nested = encode(&serde_json::json!({
            "https://api.openai.com/auth": {"email": "nested@example.com"}
        }));
        assert_eq!(
            email_from_jwt(None, &nested).as_deref(),
            Some("nested@example.com")
        );
        assert_eq!(email_from_jwt(None, "opaque"), None);
    }

    #[test]
    fn credentials_without_tokens_are_reported() {
        assert_eq!(
            parse_credentials(&serde_json::json!({"tokens": {}})),
            Err(AuthError::MissingTokens)
        );
        assert_eq!(
            parse_credentials(&serde_json::json!({})),
            Err(AuthError::MissingTokens)
        );
        // The refresh token is deliberately never retained.
        let credentials = parse_credentials(
            &serde_json::json!({"tokens": {"access_token": "abc", "refresh_token": "xyz"}}),
        )
        .unwrap();
        assert_eq!(credentials.access_token.expose(), "abc");
        assert!(!format!("{credentials:?}").contains("xyz"));
    }

    #[test]
    fn base64url_round_trips_and_rejects_foreign_characters() {
        assert_eq!(base64url_decode("aGk").as_deref(), Some(&b"hi"[..]));
        assert_eq!(base64url_decode("aGk=").as_deref(), Some(&b"hi"[..]));
        assert!(base64url_decode("a$k").is_none());
        let encoded = base64url_encode(b"\xfb\xff\x00\x01");
        assert_eq!(
            base64url_decode(&encoded).as_deref(),
            Some(&b"\xfb\xff\x00\x01"[..])
        );
    }

    #[test]
    fn config_toml_parsing_follows_the_cli() {
        assert_eq!(
            parse_chatgpt_base_url("chatgpt_base_url = \"https://chatgpt.com/backend-api\""),
            Some("https://chatgpt.com/backend-api".to_string())
        );
        assert_eq!(
            parse_chatgpt_base_url("chatgpt_base_url = https://x.example/v1"),
            Some("https://x.example/v1".to_string())
        );
        assert_eq!(
            parse_chatgpt_base_url("chatgpt_base_url = 'https://x.example'  # note"),
            Some("https://x.example".to_string())
        );
        assert_eq!(parse_chatgpt_base_url("model = \"gpt-5\""), None);
        assert_eq!(parse_chatgpt_base_url(""), None);
        // A commented-out assignment is not configuration.
        assert_eq!(
            parse_chatgpt_base_url("# chatgpt_base_url = \"https://evil.example\""),
            None
        );
    }

    #[test]
    fn base_url_normalisation_and_path_selection() {
        assert_eq!(
            normalize_base_url("https://chatgpt.com"),
            "https://chatgpt.com/backend-api"
        );
        assert_eq!(
            normalize_base_url("https://chat.openai.com/"),
            "https://chat.openai.com/backend-api"
        );
        assert_eq!(
            normalize_base_url("https://proxy.example.com/v1"),
            "https://proxy.example.com/v1"
        );
        assert_eq!(
            usage_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(
            usage_url("https://proxy.example.com/v1"),
            "https://proxy.example.com/v1/api/codex/usage"
        );
    }

    #[test]
    fn window_roles_are_classified_by_length_not_slot() {
        let session = WindowSnapshot {
            used_percent: 10.0,
            window_minutes: Some(300),
            resets_at: None,
        };
        let weekly = WindowSnapshot {
            used_percent: 90.0,
            window_minutes: Some(10_080),
            resets_at: None,
        };
        // Swapped slots are normalised back to (session, weekly).
        let (first, second) = normalize(Some(weekly.clone()), Some(session.clone()));
        assert_eq!(first.unwrap().used_percent, 10.0);
        assert_eq!(second.unwrap().used_percent, 90.0);
        // A lone weekly window stays weekly even though it arrived as `primary`.
        let (first, second) = normalize(Some(weekly.clone()), None);
        assert!(first.is_none());
        assert_eq!(second.unwrap().window_minutes, Some(10_080));
        // An unlabelled lone window is treated as the session lane.
        let unknown = WindowSnapshot {
            used_percent: 5.0,
            window_minutes: None,
            resets_at: None,
        };
        let (first, second) = normalize(Some(unknown), None);
        assert_eq!(first.unwrap().used_percent, 5.0);
        assert!(second.is_none());
    }

    #[test]
    fn a_malformed_window_never_takes_down_its_sibling() {
        let usage = parse_usage(&serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {"used_percent": 42, "limit_window_seconds": 18000},
                "secondary_window": {"used_percent": "oops"},
            }
        }))
        .unwrap();
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].id, "session");
        assert_eq!(usage.windows[0].window.used_percent, 42.0);

        let usage = parse_usage(&serde_json::json!({
            "rate_limit": {"primary_window": 7}
        }));
        assert!(
            usage.is_err(),
            "no windows and no plan is an unexpected shape"
        );
    }

    #[test]
    fn additional_limits_map_to_stable_named_extras() {
        let usage = parse_usage(&serde_json::json!({
            "plan_type": "pro",
            "additional_rate_limits": [
                {"limit_name": "GPT-5.3-Codex-Spark", "metered_feature": "codex-spark",
                 "rate_limit": {
                     "primary_window": {"used_percent": 5, "limit_window_seconds": 18000},
                     "secondary_window": {"used_percent": 2, "limit_window_seconds": 604800}
                 }},
                {"limit_name": "Code Reviews", "metered_feature": "code_review",
                 "rate_limit": {"primary_window": {"used_percent": 30, "limit_window_seconds": 18000}}},
                {"limit_name": "Broken", "rate_limit": {"primary_window": {"used_percent": "nope"}}},
            ]
        }))
        .unwrap();
        let ids: Vec<&str> = usage.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["codex-spark", "codex-spark-weekly", "codex-code-review"]
        );
        assert_eq!(usage.windows[0].title, "Codex Spark 5-hour");
        assert_eq!(usage.windows[2].title, "Code Reviews");
        assert_eq!(usage.windows[2].kind, WindowKind::Extra);
    }

    #[test]
    fn plan_labels_mirror_the_share_labels() {
        assert_eq!(plan_label("plus"), "Plus");
        assert_eq!(plan_label("pro"), "Pro 20x");
        assert_eq!(plan_label("pro_lite"), "Pro 5x");
        assert_eq!(plan_label("free_workspace"), "Free Workspace");
        assert_eq!(plan_label("something-new"), "something-new");
    }

    #[test]
    fn slugify_is_stable_and_trimmed() {
        assert_eq!(slugify("Code Reviews"), "code-reviews");
        assert_eq!(slugify("  gpt-5.3 -- spark "), "gpt-5-3-spark");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn reset_credit_count_is_read_only_when_usable() {
        assert_eq!(
            available_reset_credits(&serde_json::json!({"available_count": 3})),
            Some(3)
        );
        assert_eq!(
            available_reset_credits(&serde_json::json!({"available_count": -1})),
            None
        );
        assert_eq!(available_reset_credits(&serde_json::json!({})), None);
    }
}
