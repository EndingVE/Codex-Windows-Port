//! **Claude** — the OAuth usage/profile provider for the CodexBar Windows port.
//!
//! It reads the credential file Claude Code owns (`%USERPROFILE%\.claude\.credentials.json`,
//! or `<CLAUDE_CONFIG_DIR>\.credentials.json`) **read-only** and calls Anthropic's OAuth
//! usage API. It never refreshes and never writes: on this platform there is no Keychain
//! rescue path (that is macOS-only), so the honest answer to an expired token is a clear
//! "run `claude` to re-authenticate" error, not a silent token rewrite.
//!
//! ```text
//! pub struct ClaudeOAuth { client: Arc<dyn HttpClient>, env: Env }
//! ```
//!
//! Behaviour, from `docs-win/SPEC-flagship.md` §3 and `repo/docs/claude.md`:
//!
//! | Spec | Where |
//! | --- | --- |
//! | Credentials at `<CLAUDE_SECURESTORAGE_CONFIG_DIR / CLAUDE_CONFIG_DIR / ~/.claude>\.credentials.json` | [`ClaudeOAuth::credentials_path`] |
//! | `claudeAiOauth.{accessToken, refreshToken, expiresAt, scopes, subscriptionType, rateLimitTier}` | [`ClaudeCredentials::from_json`] |
//! | Required scope `user:profile` | [`ClaudeOAuth::fetch`] |
//! | `GET /api/oauth/usage` + `GET /api/oauth/profile`, `anthropic-beta: oauth-2025-04-20` | [`ClaudeOAuth::usage_request`] |
//! | `five_hour` → session, `seven_day` → weekly, `seven_day_sonnet`/`seven_day_opus` → weekly-scoped | [`map_usage`] |
//! | `limits[].weekly_scoped` → model-scoped weekly windows | [`scoped_limit_windows`] |
//! | `extra_usage` / `seven_day_routines` → extra lanes | [`extra_usage_window`] |
//! | A `null` `five_hour` becomes a **synthetic placeholder** (lane absent, not 0 %) | [`synthetic_session_window`] |
//! | Plan label from `subscriptionType` → `rate_limit_tier` (`Max 20x`) | [`plan_label`] |
//! | Expired token → honest `tokenExpired` error, no request, no write | [`expired_message`] |

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use codexbar_core::{
    DataSource, FetchStatus, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    WindowKind,
};

use crate::credential::{display_path, read_json, CredentialError, Env, Secret};
use crate::http::{self, HttpClient, HttpError, HttpRequest};

/// Claude Code configuration root (one literal directory; `~/…` stays literal).
pub const ENV_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";
/// Claude Code secure-storage root — takes precedence over [`ENV_CONFIG_DIR`].
pub const ENV_SECURE_STORAGE_DIR: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";
/// Optional override for the `claude-code/<version>` User-Agent fragment.
pub const ENV_CODE_VERSION: &str = "CODEXBAR_CLAUDE_CODE_VERSION";

/// Fixed API host (`SPEC-flagship.md` §3.2). Not user-configurable, so it cannot be
/// downgraded to `http://`.
pub const API_BASE_URL: &str = "https://api.anthropic.com";
/// Usage endpoint.
pub const USAGE_PATH: &str = "/api/oauth/usage";
/// Profile endpoint (identity, best-effort enrichment).
pub const PROFILE_PATH: &str = "/api/oauth/profile";
/// The beta flag the OAuth usage endpoint requires.
pub const BETA_HEADER: &str = "oauth-2025-04-20";
/// Fallback Claude Code version when no version is available.
pub const FALLBACK_CLAUDE_CODE_VERSION: &str = "2.1.0";
/// The scope a token must carry to read usage.
pub const USER_PROFILE_SCOPE: &str = "user:profile";

const SESSION_MINUTES: i64 = 5 * 60;
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;
const MONTHLY_MINUTES: i64 = 30 * 24 * 60;

const USAGE_TIMEOUT: Duration = Duration::from_secs(30);
const PROFILE_TIMEOUT: Duration = Duration::from_secs(15);

const SUBSCRIPTION_KEYS: [&str; 2] = ["subscriptionType", "subscription_type"];
const RATE_LIMIT_TIER_KEYS: [&str; 2] = ["rateLimitTier", "rate_limit_tier"];
const ROUTINE_KEYS: [&str; 4] = [
    "seven_day_routines",
    "seven_day_claude_routines",
    "seven_day_cowork",
    "cowork",
];

const MCP_ONLY_MESSAGE: &str =
    "Claude credentials contain MCP OAuth state only (no `claudeAiOauth`) — \
this is an OAuth configuration error, not a network failure. Run `claude` to re-authenticate, or \
switch the Claude usage source to Web/CLI.";

pub struct ClaudeOAuth {
    client: Arc<dyn HttpClient>,
    env: Env,
}

impl ClaudeOAuth {
    /// Production constructor: real HTTPS client, process environment.
    pub fn new() -> Self {
        Self {
            client: http::shared_client(),
            env: Env::from_process(),
        }
    }

    /// Constructor for tests and embedding: no disk reads beyond the injected
    /// `env`, no network beyond the injected `client`.
    pub fn with_client(client: Arc<dyn HttpClient>, env: Env) -> Self {
        Self { client, env }
    }

    /// `<config root>\.credentials.json`, or `None` when no home can be resolved.
    fn credentials_path(&self) -> Option<PathBuf> {
        Some(self.config_root()?.join(".credentials.json"))
    }

    /// Secure-storage root → config root → `<home>\.claude`.
    fn config_root(&self) -> Option<PathBuf> {
        // An explicitly set secure-storage root wins; an empty value means "default".
        for key in [ENV_SECURE_STORAGE_DIR, ENV_CONFIG_DIR] {
            if let Some(raw) = self.env.get_str(key) {
                return resolve_profile_dir(&raw, self.home_dir().as_deref());
            }
        }
        Some(self.home_dir()?.join(".claude"))
    }

    /// Home directory from the injected environment (so tests never fall back to
    /// the real machine's home).
    fn home_dir(&self) -> Option<PathBuf> {
        self.env
            .get_str("USERPROFILE")
            .or_else(|| self.env.get_str("HOME"))
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    }

    fn load_credentials(&self, path: &Path) -> Result<ClaudeCredentials, (FetchStatus, String)> {
        match read_json(path) {
            Ok(root) => ClaudeCredentials::from_json(&root).map_err(|reason| {
                (
                    FetchStatus::Error,
                    format!("{}: {reason}", display_path(path)),
                )
            }),
            Err(CredentialError::Missing(_)) => Err((
                FetchStatus::NotConfigured,
                format!(
                    "No Claude Code credentials at {}. Run `claude` to sign in.",
                    display_path(path)
                ),
            )),
            Err(other) => Err((
                FetchStatus::Error,
                format!(
                    "Claude credentials at {} could not be read: {other}",
                    display_path(path)
                ),
            )),
        }
    }

    fn usage_request(&self, token: &Secret) -> HttpRequest {
        self.authenticated(
            HttpRequest::get(format!("{API_BASE_URL}{USAGE_PATH}")),
            token,
        )
        .timeout(USAGE_TIMEOUT)
    }

    fn profile_request(&self, token: &Secret) -> HttpRequest {
        self.authenticated(
            HttpRequest::get(format!("{API_BASE_URL}{PROFILE_PATH}")),
            token,
        )
        .timeout(PROFILE_TIMEOUT)
    }

    /// Bearer + the OAuth beta header + the `claude-code/<version>` User-Agent.
    fn authenticated(&self, request: HttpRequest, token: &Secret) -> HttpRequest {
        request
            .bearer(token)
            .header("anthropic-beta", BETA_HEADER)
            .accept_json()
            .header("Content-Type", "application/json")
            .header("User-Agent", self.claude_code_user_agent())
    }

    fn claude_code_user_agent(&self) -> String {
        let version = self
            .env
            .get_str(ENV_CODE_VERSION)
            .and_then(|raw| raw.split_whitespace().next().map(str::to_string))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| FALLBACK_CLAUDE_CODE_VERSION.to_string());
        format!("claude-code/{version}")
    }

    fn fetch_json(&self, request: HttpRequest) -> Result<serde_json::Value, HttpError> {
        let response = self.client.send_ok(&request)?;
        response.json()
    }
}

impl Default for ClaudeOAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ClaudeOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeOAuth")
            .field("credentials_path", &self.credentials_path())
            .finish()
    }
}

/// The `claudeAiOauth` block of Claude Code's credential file.
///
/// The refresh token itself is deliberately not retained: this provider never
/// refreshes, so only its *presence* and expiry are useful for diagnostics.
#[derive(Clone, PartialEq)]
pub struct ClaudeCredentials {
    access_token: Secret,
    has_refresh_token: bool,
    expires_at: Option<DateTime<Utc>>,
    refresh_expires_at: Option<DateTime<Utc>>,
    scopes: Vec<String>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

impl ClaudeCredentials {
    pub fn from_json(root: &serde_json::Value) -> Result<Self, String> {
        let Some(oauth) = root.get("claudeAiOauth") else {
            if root.get("mcpOAuth").is_some() {
                return Err(MCP_ONLY_MESSAGE.to_string());
            }
            return Err("Claude credentials file has no `claudeAiOauth` block".to_string());
        };
        if oauth.is_null() {
            return Err("Claude credentials file has a null `claudeAiOauth` block".to_string());
        }

        let access_token = oauth
            .get("accessToken")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                "Claude credentials are missing `claudeAiOauth.accessToken`".to_string()
            })?;

        let refresh_token = oauth
            .get("refreshToken")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty());

        let scopes = oauth
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            access_token: Secret::new(access_token),
            has_refresh_token: refresh_token.is_some(),
            expires_at: millis_to_datetime(oauth.get("expiresAt")),
            refresh_expires_at: millis_to_datetime(oauth.get("refreshTokenExpiresAt")),
            scopes,
            subscription_type: str_field(oauth, &SUBSCRIPTION_KEYS),
            rate_limit_tier: str_field(oauth, &RATE_LIMIT_TIER_KEYS),
        })
    }

    /// Claude's own rule: an absent `expiresAt` counts as expired (`Date() >= expiresAt`).
    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        match self.expires_at {
            None => true,
            Some(expires_at) => now >= expires_at,
        }
    }

    fn has_user_profile_scope(&self) -> bool {
        self.scopes.iter().any(|s| s == USER_PROFILE_SCOPE)
    }
}

impl std::fmt::Debug for ClaudeCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCredentials")
            .field("access_token", &"<redacted>")
            .field("has_refresh_token", &self.has_refresh_token)
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .field("subscription_type", &self.subscription_type)
            .field("rate_limit_tier", &self.rate_limit_tier)
            .finish()
    }
}

impl Provider for ClaudeOAuth {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::Claude,
            title: ProviderId::Claude.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::OAuth,
            fetched_at: now,
        };

        // 1. Credentials — read-only, never created, never rewritten.
        let Some(path) = self.credentials_path() else {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(
                "Claude Code credentials could not be located: neither USERPROFILE/HOME nor \
                 CLAUDE_CONFIG_DIR is set."
                    .to_string(),
            );
            return snapshot;
        };
        let credentials = match self.load_credentials(&path) {
            Ok(credentials) => credentials,
            Err((status, message)) => {
                snapshot.status = status;
                snapshot.error = Some(message);
                return snapshot;
            }
        };

        // 2. Plan label is provable from the credential file itself.
        snapshot.plan = plan_label(
            credentials.subscription_type.as_deref(),
            credentials.rate_limit_tier.as_deref(),
        );

        // 3. Expired token: report it honestly. Do **not** refresh, do **not** write,
        //    and do **not** spend the revoked token on a doomed request.
        if credentials.is_expired(now) {
            snapshot.status = FetchStatus::Error;
            snapshot.error = Some(expired_message(&credentials, &path, now));
            return snapshot;
        }

        // 4. A token without `user:profile` cannot read usage at all.
        if !credentials.scopes.is_empty() && !credentials.has_user_profile_scope() {
            snapshot.status = FetchStatus::Error;
            snapshot.error = Some(scope_message(&credentials.scopes));
            return snapshot;
        }

        // 5. Usage (required).
        let usage = match self.fetch_json(self.usage_request(&credentials.access_token)) {
            Ok(body) => body,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(oauth_error_message(&err));
                return snapshot;
            }
        };
        snapshot.windows = map_usage(&usage);
        if snapshot.windows.is_empty() {
            snapshot.status = FetchStatus::Error;
            snapshot.error =
                Some("Claude usage response contained no usable quota windows".to_string());
            return snapshot;
        }

        // 6. Profile (best-effort identity/plan enrichment; a failure never blanks usage).
        if let Ok(profile) = self.fetch_json(self.profile_request(&credentials.access_token)) {
            if let Some(email) = profile_email(&profile) {
                // The address is contactable PII: it is the card's identity, so it
                // is shown masked (`user@…`), like a masked credential.
                snapshot.account = Some(crate::credential::mask_email(&email));
            }
            if let Some(plan) = profile_plan_label(&profile) {
                snapshot.plan = Some(plan);
            }
        }

        snapshot
    }
}

// ---------------------------------------------------------------------------
// Usage mapping
// ---------------------------------------------------------------------------

/// A decoded window (`utilization` + `resets_at`).
#[derive(Debug, Clone, PartialEq)]
struct UsageWindow {
    used_percent: f64,
    resets_at: Option<DateTime<Utc>>,
}

/// Map the OAuth usage payload onto named lanes (`SPEC-flagship.md` §3.3).
fn map_usage(body: &serde_json::Value) -> Vec<NamedRateWindow> {
    let five_hour = decode_window(body.get("five_hour"));
    let seven_day = decode_window(body.get("seven_day"));
    let sonnet = decode_window(body.get("seven_day_sonnet"));
    let opus = decode_window(body.get("seven_day_opus"));

    let mut windows: Vec<NamedRateWindow> = Vec::new();

    if let Some(window) = &five_hour {
        windows.push(NamedRateWindow::new(
            "session",
            "Session · 5h",
            WindowKind::Session,
            rate_window(window, SESSION_MINUTES),
        ));
    } else if seven_day.is_some() {
        // No live 5-hour window: publish the placeholder so lane classifiers skip
        // it instead of rendering a phantom "5h · 0 %".
        windows.push(synthetic_session_window());
    }

    if let Some(window) = &seven_day {
        windows.push(NamedRateWindow::new(
            "weekly",
            "Weekly · 7d",
            WindowKind::Weekly,
            rate_window(window, WEEKLY_MINUTES),
        ));
    }
    if let Some(window) = &sonnet {
        windows.push(NamedRateWindow::new(
            "weekly-sonnet",
            "Weekly · Sonnet",
            WindowKind::WeeklyScoped,
            rate_window(window, WEEKLY_MINUTES),
        ));
    }
    if let Some(window) = &opus {
        windows.push(NamedRateWindow::new(
            "weekly-opus",
            "Weekly · Opus",
            WindowKind::WeeklyScoped,
            rate_window(window, WEEKLY_MINUTES),
        ));
    }

    windows.extend(scoped_limit_windows(body));

    if let Some(window) = routine_window(body) {
        windows.push(window);
    }
    if let Some(window) = extra_usage_window(body) {
        windows.push(window);
    }

    windows
}

/// The synthetic 5-hour placeholder: lane absent, not "0 % used".
fn synthetic_session_window() -> NamedRateWindow {
    NamedRateWindow::new(
        "session",
        "Session · 5h",
        WindowKind::Session,
        RateWindow {
            used_percent: 0.0,
            window_minutes: Some(SESSION_MINUTES),
            resets_at: None,
            reset_description: None,
            next_regen_percent: None,
            is_synthetic_placeholder: true,
        },
    )
}

fn rate_window(window: &UsageWindow, minutes: i64) -> RateWindow {
    RateWindow::new(window.used_percent, Some(minutes), window.resets_at)
}

/// `limits[].{kind: weekly_scoped, group: weekly}` → one weekly-scoped lane per model.
fn scoped_limit_windows(body: &serde_json::Value) -> Vec<NamedRateWindow> {
    let Some(limits) = body.get("limits").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut seen: Vec<String> = Vec::new();
    let mut windows = Vec::new();

    for limit in limits {
        if limit.get("kind").and_then(|v| v.as_str()) != Some("weekly_scoped") {
            continue;
        }
        if limit.get("group").and_then(|v| v.as_str()) != Some("weekly") {
            continue;
        }
        let Some(percent) = limit
            .get("percent")
            .and_then(|v| v.as_f64())
            .filter(|p| p.is_finite())
        else {
            continue;
        };
        let model = limit.get("scope").and_then(|scope| scope.get("model"));
        let Some(name) = model
            .and_then(|m| m.get("display_name"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let model_id = model
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if is_all_models_scope(model_id, name) {
            continue;
        }
        let identity = model_id.unwrap_or(name);
        let slug = slugify(identity);
        if slug.is_empty() {
            continue;
        }
        let id = format!("claude-weekly-scoped-{slug}");
        if seen.contains(&id) {
            continue;
        }
        seen.push(id.clone());
        let resets_at = limit.get("resets_at").and_then(parse_iso);
        windows.push(NamedRateWindow::new(
            id,
            format!("Weekly · {name}"),
            WindowKind::WeeklyScoped,
            RateWindow::new(percent, Some(WEEKLY_MINUTES), resets_at),
        ));
    }

    windows
}

fn routine_window(body: &serde_json::Value) -> Option<NamedRateWindow> {
    let window = ROUTINE_KEYS
        .iter()
        .find_map(|key| decode_window(body.get(*key)))?;
    Some(NamedRateWindow::new(
        "claude-routines",
        "Daily Routines",
        WindowKind::Extra,
        rate_window(&window, WEEKLY_MINUTES),
    ))
}

fn extra_usage_window(body: &serde_json::Value) -> Option<NamedRateWindow> {
    let extra = body.get("extra_usage")?;
    if extra.get("is_enabled").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let used_percent = extra
        .get("utilization")
        .and_then(|v| v.as_f64())
        .filter(|p| p.is_finite())?;
    Some(NamedRateWindow::new(
        "extra-usage",
        "Extra usage",
        WindowKind::Extra,
        RateWindow::new(used_percent, Some(MONTHLY_MINUTES), None),
    ))
}

// ---------------------------------------------------------------------------
// Decoding helpers
// ---------------------------------------------------------------------------

fn decode_window(value: Option<&serde_json::Value>) -> Option<UsageWindow> {
    let object = value?.as_object()?;
    let used_percent = object.get("utilization").and_then(|v| v.as_f64())?;
    if !used_percent.is_finite() {
        return None;
    }
    Some(UsageWindow {
        used_percent,
        resets_at: object.get("resets_at").and_then(parse_iso),
    })
}

fn parse_iso(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    let raw = value.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn millis_to_datetime(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    let value = value?;
    let millis = value
        .as_i64()
        .or_else(|| value.as_f64().map(|f| f.round() as i64))?;
    DateTime::from_timestamp_millis(millis)
}

fn str_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn is_all_models_scope(model_id: Option<&str>, name: &str) -> bool {
    if slugify(name) == "all-models" {
        return true;
    }
    match model_id {
        None => false,
        Some(id) => {
            let slug = slugify(id);
            slug == "all-models" || slug.ends_with("-all-models")
        }
    }
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

// ---------------------------------------------------------------------------
// Identity / plan
// ---------------------------------------------------------------------------

fn profile_email(profile: &serde_json::Value) -> Option<String> {
    const KEYS: [&str; 3] = ["emailAddress", "email_address", "email"];
    profile
        .get("account")
        .and_then(|account| str_field(account, &KEYS))
        .or_else(|| str_field(profile, &KEYS))
}

fn profile_plan_label(profile: &serde_json::Value) -> Option<String> {
    let account = profile.get("account");
    let subscription = account
        .and_then(|a| str_field(a, &SUBSCRIPTION_KEYS))
        .or_else(|| str_field(profile, &SUBSCRIPTION_KEYS));
    let tier = account
        .and_then(|a| str_field(a, &RATE_LIMIT_TIER_KEYS))
        .or_else(|| str_field(profile, &RATE_LIMIT_TIER_KEYS));
    plan_label(subscription.as_deref(), tier.as_deref())
}

/// `subscriptionType` first, `rate_limit_tier` as the fallback; a Max tier's
/// multiplier is surfaced as `Max 5x` / `Max 20x` (SPEC §3.3).
pub fn plan_label(
    subscription_type: Option<&str>,
    rate_limit_tier: Option<&str>,
) -> Option<String> {
    let base = subscription_type
        .and_then(plan_from_words)
        .or_else(|| rate_limit_tier.and_then(plan_from_words))?;
    if base == "Max" {
        if let Some(multiplier) = max_multiplier(rate_limit_tier) {
            return Some(format!("Max {multiplier}"));
        }
    }
    Some(base.to_string())
}

fn plan_from_words(raw: &str) -> Option<&'static str> {
    let words = words(raw);
    if words.iter().any(|w| w == "max") {
        Some("Max")
    } else if words.iter().any(|w| w == "pro") {
        Some("Pro")
    } else if words.iter().any(|w| w == "team") {
        Some("Team")
    } else if words.iter().any(|w| w == "enterprise") {
        Some("Enterprise")
    } else if words.iter().any(|w| w == "ultra") {
        Some("Ultra")
    } else {
        None
    }
}

fn max_multiplier(rate_limit_tier: Option<&str>) -> Option<String> {
    let words = words(rate_limit_tier?);
    let index = words.iter().position(|w| w == "max")?;
    let candidate = words.get(index + 1)?;
    let digits = candidate.strip_suffix('x')?;
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        Some(candidate.clone())
    } else {
        None
    }
}

fn words(raw: &str) -> Vec<String> {
    raw.to_ascii_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Paths / messages
// ---------------------------------------------------------------------------

/// Resolve a `CLAUDE_*_CONFIG_DIR` value. Only a leading `/` or `\` or a `X:` drive
/// prefix is absolute; `~/…` stays literal (matching `ClaudeConfigPaths.swift`).
fn resolve_profile_dir(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if is_absolute_profile_path(raw) {
        return Some(PathBuf::from(raw));
    }
    home.map(|home| home.join(raw))
}

fn is_absolute_profile_path(raw: &str) -> bool {
    if raw.starts_with('/') || raw.starts_with('\\') {
        return true;
    }
    let mut chars = raw.chars();
    matches!((chars.next(), chars.next()), (Some(drive), Some(':')) if drive.is_ascii_alphabetic())
}

fn expired_message(credentials: &ClaudeCredentials, path: &Path, now: DateTime<Utc>) -> String {
    let expiry = credentials
        .expires_at
        .map(format_rfc3339)
        .unwrap_or_else(|| "an unknown time".to_string());
    let refresh = match credentials.refresh_expires_at {
        Some(t) if t > now => format!(
            " The stored refresh token is still valid until {}, but CodexBar will not use it.",
            format_rfc3339(t)
        ),
        _ => String::new(),
    };
    format!(
        "Claude OAuth tokenExpired: the access token at {} expired at {expiry}. CodexBar reads \
         Claude Code credentials read-only and never refreshes or rewrites them — run `claude` to \
         re-authenticate.{refresh}",
        display_path(path)
    )
}

fn scope_message(scopes: &[String]) -> String {
    let list = if scopes.is_empty() {
        "none".to_string()
    } else {
        scopes.join(", ")
    };
    format!(
        "Claude OAuth token is missing the required '{USER_PROFILE_SCOPE}' scope (has: {list}). \
         Run `claude setup-token` to regenerate credentials."
    )
}

fn oauth_error_message(err: &HttpError) -> String {
    match err.status {
        Some(401) => "Claude OAuth request was rejected (401). The token may have been revoked — \
                      run `claude` to re-authenticate."
            .to_string(),
        Some(403) => {
            if err.message.contains(USER_PROFILE_SCOPE) {
                format!(
                    "Claude OAuth token does not meet the required '{USER_PROFILE_SCOPE}' scope. \
                     Run `claude setup-token`, or switch the Claude usage source to Web/CLI."
                )
            } else {
                format!("Claude OAuth request was forbidden (403): {}", err.message)
            }
        }
        Some(429) => format!(
            "Claude OAuth usage endpoint is rate limited (429); wait a few minutes and retry. {}",
            err.message
        ),
        _ => err.message.clone(),
    }
}

fn format_rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_prefers_subscription_type_and_surfaces_the_max_multiplier() {
        assert_eq!(
            plan_label(Some("max"), Some("default_claude_max_20x")).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(
            plan_label(None, Some("default_claude_max_5x")).as_deref(),
            Some("Max 5x")
        );
        assert_eq!(plan_label(Some("pro"), None).as_deref(), Some("Pro"));
        assert_eq!(plan_label(Some("team"), None).as_deref(), Some("Team"));
        assert_eq!(
            plan_label(Some("enterprise"), None).as_deref(),
            Some("Enterprise")
        );
        assert_eq!(plan_label(Some("max"), None).as_deref(), Some("Max"));
        assert_eq!(plan_label(Some("something-else"), None), None);
    }

    #[test]
    fn credentials_reject_missing_or_mcp_only_blocks() {
        assert!(ClaudeCredentials::from_json(&serde_json::json!({})).is_err());
        let mcp_only = serde_json::json!({"mcpOAuth": {"server": {}}});
        let err = ClaudeCredentials::from_json(&mcp_only).unwrap_err();
        assert!(err.contains("claudeAiOauth"), "{err}");
        let null_block = serde_json::json!({"claudeAiOauth": null});
        assert!(ClaudeCredentials::from_json(&null_block).is_err());
        let no_token = serde_json::json!({"claudeAiOauth": {"expiresAt": 1}});
        assert!(ClaudeCredentials::from_json(&no_token).is_err());
    }

    #[test]
    fn slugify_matches_the_swift_scoped_id_rule() {
        assert_eq!(slugify("Fable"), "fable");
        assert_eq!(slugify("Claude Sonnet 4.5"), "claude-sonnet-4-5");
        assert_eq!(slugify("--weird--"), "weird");
    }

    #[test]
    fn a_null_five_hour_becomes_a_synthetic_placeholder() {
        let body = serde_json::json!({
            "five_hour": null,
            "seven_day": {"utilization": 10.0, "resets_at": "2026-09-17T20:00:00Z"}
        });
        let windows = map_usage(&body);
        let session = windows.iter().find(|w| w.id == "session").unwrap();
        assert!(session.window.is_synthetic_placeholder);
        assert_eq!(session.window.used_percent, 0.0);
        assert!(windows.iter().any(|w| w.id == "weekly"));
    }
}
