//! **GitHub Copilot** — the device-flow provider.
//!
//! Copilot is the one provider whose credential the port **owns**: it never reads
//! `~/.config/github-copilot` or `%APPDATA%\github-copilot` (the original macOS
//! app does not either). Authentication is a user-initiated GitHub **device code
//! flow** ([`DeviceFlow`]), and the resulting OAuth token is stored in the port's
//! own `config.json` (`providers[id="copilot"].tokenAccounts`, or the
//! `COPILOT_API_TOKEN` env var), exactly like an API key.
//!
//! ```text
//! DeviceFlow::request_device_code()   -> show user_code + verification URL
//! DeviceFlow::await_token(...)         -> poll POST /login/oauth/access_token
//!                                         until authorization_pending clears
//! ```
//!
//! Once a token exists, [`Copilot::fetch`] is a plain read-only usage probe:
//!
//! | Spec (`SPEC-flagship.md` §6) | Where |
//! | --- | --- |
//! | Device code `POST /login/device/code`, poll `POST /login/oauth/access_token` | [`DeviceFlow`] |
//! | `client_id` / `scope` / `grant_type` | [`CLIENT_ID`], [`SCOPE`] |
//! | `authorization_pending` → wait, `slow_down` → +5 s, `expired_token` → timeout, other → deny | [`PollOutcome`] |
//! | Usage `GET {api_host}/copilot_internal/user` | [`Copilot::fetch_usage`] |
//! | `Authorization: token …` (the GitHub OAuth token, **not** the Copilot token) | [`Copilot::usage_request`] |
//! | VS Code `Editor-Version` / `Editor-Plugin-Version` / `User-Agent` / `X-Github-Api-Version` | [`Copilot::usage_request`] |
//! | 401/403 → re-login hint | [`Copilot::describe_error`] |
//! | Primary `premium_interactions`, secondary `chat`; placeholders/unlimited → no fake bar | [`make_window`], [`QuotaSnapshots`] |
//! | `monthly_quotas` / `limited_user_quotas` fallback (included / monthly quotas) | [`make_quota_snapshots`] |
//! | Enterprise host (`api.<host>`) | [`normalized_host`], [`api_host`] |
//!
//! The API does not promise a reset date; when `quota_reset_date` is present it is
//! carried onto both windows, otherwise they simply have no `resetsAt`.
//!
//! `fetch` never panics and never starts the interactive flow: with no stored
//! token it returns [`FetchStatus::NotConfigured`] with a setup hint, and the tray
//! keeps refreshing uninterrupted.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use codexbar_core::{
    DataSource, FetchStatus, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    WindowKind,
};

use crate::credential::{Env, PortConfig, Secret};
use crate::http::{self, ensure_https, HttpClient, HttpError, HttpRequest};

/// Primary credential env var (mirrors the macOS `COPILOT_API_TOKEN`).
pub const ENV_TOKEN: &str = "COPILOT_API_TOKEN";
/// Optional GitHub Enterprise host override (`ghe.com`, `github.mycorp.com`).
pub const ENV_ENTERPRISE_HOST: &str = "COPILOT_ENTERPRISE_HOST";

const ENV_ALIASES: [&str; 1] = [ENV_TOKEN];

/// The public GitHub host.
pub const DEFAULT_HOST: &str = "github.com";
/// The public GitHub REST API host.
pub const DEFAULT_API_HOST: &str = "api.github.com";
/// The VS Code OAuth application id (`SPEC-flagship.md` §6.1).
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
/// The only scope the device flow asks for.
pub const SCOPE: &str = "read:user";

/// VS Code identity headers the Copilot backend expects (§6.2).
pub const EDITOR_VERSION: &str = "vscode/1.96.2";
/// Editor plugin version header.
pub const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
/// User agent header.
pub const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
/// Pinned GitHub API version header.
pub const GITHUB_API_VERSION: &str = "2025-04-01";

/// `slow_down` adds this to the polling interval (§6.1).
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

const USAGE_TIMEOUT: Duration = Duration::from_secs(15);
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15);

/// Shown when no token is stored. Actionable, and names every place the user can
/// put one.
const NOT_CONFIGURED_HINT: &str = "Not signed in to GitHub Copilot — use \"Sign in with GitHub\" \
     in Settings (GitHub device code flow). The token is stored under \
     providers[id=\"copilot\"].tokenAccounts in %APPDATA%\\CodexBar\\config.json, or set \
     COPILOT_API_TOKEN.";

// ---------------------------------------------------------------------------
// Device flow (user-initiated; never run inside `fetch`)
// ---------------------------------------------------------------------------

/// One `POST /login/device/code` response: what the UI shows the user and what
/// the poller needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    /// Opaque code exchanged for a token. Redacted in `Debug`.
    pub device_code: Secret,
    /// Short code the user types on the verification page (`ABCD-1234`).
    pub user_code: String,
    /// Verification page URL.
    pub verification_uri: String,
    /// Pre-filled verification URL, when GitHub supplies one.
    pub verification_uri_complete: Option<String>,
    /// Lifetime of the code in seconds.
    pub expires_in: i64,
    /// Server-suggested poll interval in seconds.
    pub interval: i64,
}

impl DeviceCode {
    /// Build from the JSON body. `None` when a required field is missing — never
    /// panics on an unexpected shape.
    pub fn from_json(body: &serde_json::Value) -> Option<Self> {
        let device_code = non_empty(body, "device_code")?;
        let user_code = non_empty(body, "user_code")?;
        let verification_uri = non_empty(body, "verification_uri")?;
        Some(Self {
            device_code: Secret::new(device_code),
            user_code,
            verification_uri,
            verification_uri_complete: non_empty(body, "verification_uri_complete"),
            expires_in: integer(body, "expires_in").unwrap_or(900),
            interval: integer(body, "interval").unwrap_or(5).max(1),
        })
    }

    /// The URL to open: the pre-filled one when present.
    pub fn verification_url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

/// The state machine of a single poll response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// `authorization_pending` — the user has not finished yet; wait and retry.
    Pending,
    /// `slow_down` — back off by [`SLOW_DOWN_STEP`] and retry.
    SlowDown,
    /// A token was issued.
    Authorized(Secret),
    /// `expired_token` — the device code is dead; start over.
    Expired,
    /// `access_denied` — the user declined.
    Denied,
    /// Any other error code (or a body with neither token nor error).
    Failed(String),
}

impl PollOutcome {
    /// Classify a token-endpoint body. GitHub answers with `error` while pending
    /// and with `access_token` once approved.
    pub fn from_json(body: &serde_json::Value) -> Self {
        if let Some(token) = body
            .get("access_token")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Self::Authorized(Secret::new(token));
        }
        match body.get("error").and_then(|value| value.as_str()) {
            Some("authorization_pending") => Self::Pending,
            Some("slow_down") => Self::SlowDown,
            Some("expired_token") => Self::Expired,
            Some("access_denied") => Self::Denied,
            Some("") | None => {
                Self::Failed("GitHub returned neither a token nor an error code".to_string())
            }
            Some(other) => Self::Failed(format!("GitHub device flow failed: {other}")),
        }
    }
}

/// Why a device flow could not complete. Messages never contain the token or the
/// device code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceFlowError {
    /// The request itself failed.
    Http(HttpError),
    /// `expired_token`.
    Expired,
    /// `access_denied`.
    Denied,
    /// A generic GitHub error code.
    Failed(String),
    /// The bounded poll loop ran out of attempts.
    TooManyPolls,
}

impl std::fmt::Display for DeviceFlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceFlowError::Http(err) => write!(f, "{err}"),
            DeviceFlowError::Expired => f.write_str("the GitHub device code expired; start again"),
            DeviceFlowError::Denied => f.write_str("GitHub sign-in was denied"),
            DeviceFlowError::Failed(message) => f.write_str(message),
            DeviceFlowError::TooManyPolls => {
                f.write_str("GitHub sign-in was not completed in time; start again")
            }
        }
    }
}

impl std::error::Error for DeviceFlowError {}

/// The GitHub device flow, scoped to one host (public GitHub by default).
pub struct DeviceFlow {
    client: Arc<dyn HttpClient>,
    host: String,
}

impl DeviceFlow {
    /// Production: real HTTPS client, host from the port config / environment.
    pub fn new() -> Self {
        let env = Env::from_process();
        let config = PortConfig::load(&env).unwrap_or(None);
        let raw = config
            .as_ref()
            .and_then(|config| config.field(ProviderId::Copilot, "enterpriseHost"))
            .or_else(|| env.get_str(ENV_ENTERPRISE_HOST));
        Self {
            client: http::shared_client(),
            host: normalized_host(raw.as_deref()),
        }
    }

    /// Tests and embedders: injected client and host, no disk, no network.
    pub fn with_client(client: Arc<dyn HttpClient>, host: Option<&str>) -> Self {
        Self {
            client,
            host: normalized_host(host),
        }
    }

    /// The normalized GitHub host this flow talks to.
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn device_code_url(&self) -> String {
        format!("https://{}/login/device/code", self.host)
    }

    pub fn access_token_url(&self) -> String {
        format!("https://{}/login/oauth/access_token", self.host)
    }

    /// Step 1 — ask GitHub for a device code. The caller shows `user_code` and
    /// [`DeviceCode::verification_url`] to the user, then polls.
    pub fn request_device_code(&self) -> Result<DeviceCode, DeviceFlowError> {
        let request = HttpRequest::post(self.device_code_url())
            .form_body(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
            .accept_json()
            .timeout(DEVICE_TIMEOUT);

        let response = self
            .client
            .send_ok(&request)
            .map_err(DeviceFlowError::Http)?;
        let body: serde_json::Value = response.json().map_err(DeviceFlowError::Http)?;
        DeviceCode::from_json(&body).ok_or_else(|| {
            DeviceFlowError::Failed(
                "GitHub device-code response was missing device_code/user_code/verification_uri"
                    .to_string(),
            )
        })
    }

    /// One poll. The caller owns the waiting policy so this stays testable.
    pub fn poll_once(&self, device_code: &Secret) -> Result<PollOutcome, DeviceFlowError> {
        let request = HttpRequest::post(self.access_token_url())
            .form_body(&[
                ("client_id", CLIENT_ID),
                ("device_code", device_code.expose()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .accept_json()
            .timeout(DEVICE_TIMEOUT);

        let response = self
            .client
            .send_ok(&request)
            .map_err(DeviceFlowError::Http)?;
        let body: serde_json::Value = response.json().map_err(DeviceFlowError::Http)?;
        Ok(PollOutcome::from_json(&body))
    }

    /// Step 2 — poll until a token arrives, the code expires, or the user denies.
    ///
    /// `sleeper` is injected so tests never sleep; production passes
    /// [`DeviceFlow::await_token_blocking`]. `max_attempts` bounds the loop so a
    /// misbehaving server can never spin forever.
    pub fn await_token(
        &self,
        device_code: &DeviceCode,
        sleeper: &dyn Fn(Duration),
        max_attempts: usize,
    ) -> Result<Secret, DeviceFlowError> {
        let mut interval = Duration::from_secs(device_code.interval.max(1) as u64);
        for _ in 0..max_attempts.max(1) {
            sleeper(interval);
            match self.poll_once(&device_code.device_code)? {
                PollOutcome::Pending => {}
                PollOutcome::SlowDown => interval += SLOW_DOWN_STEP,
                PollOutcome::Authorized(token) => return Ok(token),
                PollOutcome::Expired => return Err(DeviceFlowError::Expired),
                PollOutcome::Denied => return Err(DeviceFlowError::Denied),
                PollOutcome::Failed(message) => return Err(DeviceFlowError::Failed(message)),
            }
        }
        Err(DeviceFlowError::TooManyPolls)
    }

    /// The interactive driver: [`DeviceFlow::await_token`] with a real sleep.
    /// Only the settings UI calls this — `fetch` never does.
    pub fn await_token_blocking(
        &self,
        device_code: &DeviceCode,
        max_attempts: usize,
    ) -> Result<Secret, DeviceFlowError> {
        self.await_token(device_code, &std::thread::sleep, max_attempts)
    }
}

impl Default for DeviceFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DeviceFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceFlow")
            .field("host", &self.host)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Usage payload (`GET /copilot_internal/user`)
// ---------------------------------------------------------------------------

/// `{ chat, completions }` absolute counts (`monthly_quotas` /
/// `limited_user_quotas`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaCounts {
    pub chat: Option<f64>,
    pub completions: Option<f64>,
}

impl QuotaCounts {
    pub fn from_json(value: &serde_json::Value) -> Self {
        Self {
            chat: number(value, "chat"),
            completions: number(value, "completions"),
        }
    }
}

/// One `quota_snapshots.*` entry. Mirrors the tolerant Swift decode: anything can
/// be a number, a numeric string, or absent.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaSnapshot {
    pub entitlement: f64,
    pub remaining: f64,
    pub credits_used: Option<f64>,
    pub percent_remaining: f64,
    pub quota_id: String,
    pub has_percent_remaining: bool,
    pub unlimited: bool,
    /// Whether `entitlement` was actually present in the payload (placeholder rule).
    entitlement_present: bool,
    /// Whether `remaining` was actually present in the payload (placeholder rule).
    remaining_present: bool,
}

impl QuotaSnapshot {
    /// Decode with the documented fallbacks: an explicit `percent_remaining`
    /// wins, else it is derived from `remaining / entitlement`.
    pub fn from_json(value: &serde_json::Value) -> Self {
        let entitlement = number(value, "entitlement");
        let remaining = number(value, "remaining");
        let unlimited = value
            .get("unlimited")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let decoded_percent = number(value, "percent_remaining");

        let (percent_remaining, has_percent_remaining) = if unlimited {
            (100.0, true)
        } else if let Some(percent) = decoded_percent {
            (percent, true)
        } else if let (Some(entitlement), Some(remaining)) = (entitlement, remaining) {
            if entitlement > 0.0 {
                ((remaining / entitlement) * 100.0, true)
            } else {
                (0.0, false)
            }
        } else {
            (0.0, false)
        };

        Self {
            entitlement: entitlement.unwrap_or(0.0),
            remaining: remaining.unwrap_or(0.0),
            credits_used: number(value, "credits_used"),
            percent_remaining,
            quota_id: non_empty(value, "quota_id").unwrap_or_default(),
            has_percent_remaining,
            unlimited,
            entitlement_present: entitlement.is_some(),
            remaining_present: remaining.is_some(),
        }
    }

    /// Used percent. **Not clamped** — over-quota providers exceed 100.
    pub fn used_percent(&self) -> f64 {
        (100.0 - self.percent_remaining).max(0.0)
    }

    /// Set only when the account is over quota (`SPEC` over-quota description).
    pub fn over_quota_used_percent(&self) -> Option<f64> {
        let used = self.used_percent();
        (used > 100.0).then_some(used)
    }

    /// A snapshot that carries no usable quota signal — a zero/zero entitlement
    /// GitHub returns for token-based billing seats, or a bare `{}`.
    pub fn is_placeholder(&self) -> bool {
        if self.unlimited {
            return false;
        }
        if self.entitlement == 0.0
            && self.remaining == 0.0
            && self.percent_remaining == 0.0
            && !self.has_percent_remaining
        {
            return true;
        }
        self.entitlement_present
            && self.remaining_present
            && self.entitlement == 0.0
            && self.remaining == 0.0
    }

    /// Whether the snapshot carries a real absolute credit counter, even when it
    /// cannot produce a percentage window.
    pub fn carries_credits_counter(&self) -> bool {
        self.credits_used.is_some()
    }

    /// Can this snapshot become a percentage bar?
    pub fn is_usable(&self) -> bool {
        !self.is_placeholder() && self.has_percent_remaining
    }

    fn with_credits_used(mut self, credits_used: Option<f64>) -> Self {
        self.credits_used = credits_used;
        self
    }
}

/// The `quota_snapshots` object, with the dynamic-key fallback the original uses
/// when GitHub renames the slots.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaSnapshots {
    pub premium_interactions: Option<QuotaSnapshot>,
    pub chat: Option<QuotaSnapshot>,
}

impl QuotaSnapshots {
    pub fn from_json(value: &serde_json::Value) -> Self {
        let mut premium = selected_snapshot(value.get("premium_interactions"));
        let mut chat = selected_snapshot(value.get("chat"));

        if premium.is_none() || chat.is_none() {
            let mut fallback_premium: Option<QuotaSnapshot> = None;
            let mut fallback_chat: Option<QuotaSnapshot> = None;
            let mut first_usable: Option<QuotaSnapshot> = None;

            if let Some(object) = value.as_object() {
                for (name, raw) in object {
                    let snapshot = QuotaSnapshot::from_json(raw);
                    if !snapshot.is_usable() && !snapshot.carries_credits_counter() {
                        continue;
                    }
                    let lowered = name.to_ascii_lowercase();
                    if first_usable.is_none() {
                        first_usable = Some(snapshot.clone());
                    }
                    if fallback_chat.is_none() && lowered.contains("chat") {
                        fallback_chat = Some(snapshot);
                        continue;
                    }
                    if fallback_premium.is_none()
                        && (lowered.contains("premium")
                            || lowered.contains("completion")
                            || lowered.contains("code"))
                    {
                        fallback_premium = Some(snapshot);
                    }
                }
            }

            if premium.is_none() {
                premium = fallback_premium;
            }
            if chat.is_none() {
                chat = fallback_chat;
            }
            if premium.is_none() && chat.is_none() {
                chat = first_usable;
            }
        }

        Self {
            premium_interactions: premium,
            chat,
        }
    }
}

/// A decoded `GET /copilot_internal/user` body.
#[derive(Debug, Clone, PartialEq)]
pub struct CopilotUsage {
    /// The two quota slots, already de-placeholdered.
    pub quota_snapshots: QuotaSnapshots,
    /// `copilot_plan` (`free`, `individual`, `business`, ...).
    pub copilot_plan: String,
    /// Token-based billing seats have no percentage windows.
    pub token_based_billing: bool,
    /// `quota_reset_date`, if present (the API usually omits it).
    pub quota_reset_date: Option<String>,
}

impl CopilotUsage {
    /// Decode the body with the documented fallbacks. Never panics.
    pub fn from_json(body: &serde_json::Value) -> Self {
        let direct = body.get("quota_snapshots").map(QuotaSnapshots::from_json);
        let monthly = body.get("monthly_quotas").map(QuotaCounts::from_json);
        let limited = body.get("limited_user_quotas").map(QuotaCounts::from_json);
        let from_counts = make_quota_snapshots(monthly.as_ref(), limited.as_ref());

        let premium = preferred_quota_snapshot(
            direct.as_ref().and_then(|d| d.premium_interactions.clone()),
            from_counts
                .as_ref()
                .and_then(|d| d.premium_interactions.clone()),
        );
        let chat = preferred_quota_snapshot(
            direct.as_ref().and_then(|d| d.chat.clone()),
            from_counts.as_ref().and_then(|d| d.chat.clone()),
        );

        let quota_snapshots = if premium.is_some() || chat.is_some() {
            QuotaSnapshots {
                premium_interactions: premium,
                chat,
            }
        } else {
            direct.unwrap_or_default()
        };

        Self {
            quota_snapshots,
            copilot_plan: non_empty(body, "copilot_plan").unwrap_or_else(|| "unknown".to_string()),
            token_based_billing: body
                .get("token_based_billing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            quota_reset_date: non_empty(body, "quota_reset_date"),
        }
    }
}

/// A snapshot that survives the placeholder filter, or `None`.
fn selected_snapshot(raw: Option<&serde_json::Value>) -> Option<QuotaSnapshot> {
    let snapshot = QuotaSnapshot::from_json(raw?);
    if snapshot.is_placeholder() && !snapshot.carries_credits_counter() {
        None
    } else {
        Some(snapshot)
    }
}

/// Derive quota slots from the `monthly_quotas` / `limited_user_quotas`
/// fallback (`remaining / entitlement`, clamped to `0..=100`).
fn make_quota_snapshots(
    monthly: Option<&QuotaCounts>,
    limited: Option<&QuotaCounts>,
) -> Option<QuotaSnapshots> {
    let premium = make_quota_snapshot(
        monthly.and_then(|m| m.completions),
        limited.and_then(|l| l.completions),
        "completions",
    );
    let chat = make_quota_snapshot(
        monthly.and_then(|m| m.chat),
        limited.and_then(|l| l.chat),
        "chat",
    );
    (premium.is_some() || chat.is_some()).then_some(QuotaSnapshots {
        premium_interactions: premium,
        chat,
    })
}

fn make_quota_snapshot(
    monthly: Option<f64>,
    limited: Option<f64>,
    quota_id: &str,
) -> Option<QuotaSnapshot> {
    if monthly.is_none() && limited.is_none() {
        return None;
    }
    // Without a denominator, fabricating a percentage would be a lie.
    let monthly = monthly?;
    let limited = limited?;
    let entitlement = monthly.max(0.0);
    if entitlement <= 0.0 {
        return None;
    }
    let remaining = limited.max(0.0);
    let percent_remaining = (remaining / entitlement * 100.0).clamp(0.0, 100.0);
    Some(QuotaSnapshot {
        entitlement,
        remaining,
        credits_used: None,
        percent_remaining,
        quota_id: quota_id.to_string(),
        has_percent_remaining: true,
        unlimited: false,
        entitlement_present: true,
        remaining_present: true,
    })
}

/// Prefer a usable direct snapshot; fall back to the derived one, carrying a real
/// credit counter across when the direct slot was unlimited/placeholder.
fn preferred_quota_snapshot(
    direct: Option<QuotaSnapshot>,
    fallback: Option<QuotaSnapshot>,
) -> Option<QuotaSnapshot> {
    let direct_credits = direct.as_ref().and_then(|d| d.credits_used);

    if direct.as_ref().is_some_and(|d| d.unlimited) {
        if let Some(fallback) = fallback.clone().filter(QuotaSnapshot::is_usable) {
            return Some(fallback.with_credits_used(direct_credits));
        }
    }
    if let Some(direct) = direct.filter(QuotaSnapshot::is_usable) {
        return Some(direct);
    }
    let fallback = fallback.filter(QuotaSnapshot::is_usable)?;
    if direct_credits.is_some() {
        return Some(fallback.with_credits_used(direct_credits));
    }
    Some(fallback)
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct Copilot {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl Copilot {
    /// Production constructor: real HTTPS client, process environment, the port's
    /// own config file if the user has one.
    pub fn new() -> Self {
        let env = Env::from_process();
        let config = PortConfig::load(&env).unwrap_or(None);
        Self {
            client: http::shared_client(),
            env,
            config,
        }
    }

    /// Constructor for tests and embedding: no disk, no network.
    pub fn with_client(client: Arc<dyn HttpClient>, env: Env) -> Self {
        Self {
            client,
            env,
            config: None,
        }
    }

    pub fn with_config(mut self, config: Option<PortConfig>) -> Self {
        self.config = config;
        self
    }

    /// The stored GitHub OAuth token: active token account → `providers[].apiKey`
    /// → `COPILOT_API_TOKEN`.
    fn token(&self) -> Option<Secret> {
        match &self.config {
            Some(config) => config.resolve_api_key(ProviderId::Copilot, &self.env, &ENV_ALIASES),
            None => self.env.first_of(&ENV_ALIASES),
        }
    }

    /// The GitHub host (public or enterprise), validated before any request.
    fn endpoint_host(&self) -> Result<String, String> {
        let raw = self
            .config
            .as_ref()
            .and_then(|config| config.field(ProviderId::Copilot, "enterpriseHost"))
            .or_else(|| self.env.get_str(ENV_ENTERPRISE_HOST));
        let host = normalized_host(raw.as_deref());
        if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(
                "the configured Copilot enterprise host contains whitespace or control characters"
                    .to_string(),
            );
        }
        // Fail closed before a token is attached: only HTTPS endpoints are used.
        ensure_https(&format!("https://{host}/copilot_internal/user"))?;
        Ok(host)
    }

    fn usage_request(&self, url: String, token: &Secret) -> HttpRequest {
        HttpRequest::get(url)
            // The GitHub OAuth token, NOT the Copilot token (§6.2).
            .authorization(format!("token {}", token.expose()))
            .accept_json()
            .header("Editor-Version", EDITOR_VERSION)
            .header("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION)
            .header("User-Agent", COPILOT_USER_AGENT)
            .header("X-Github-Api-Version", GITHUB_API_VERSION)
            .timeout(USAGE_TIMEOUT)
    }

    fn fetch_usage(&self, api_host: &str, token: &Secret) -> Result<CopilotUsage, HttpError> {
        let request =
            self.usage_request(format!("https://{api_host}/copilot_internal/user"), token);
        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        Ok(CopilotUsage::from_json(&body))
    }

    /// Best-effort GitHub login for the card subtitle.
    fn fetch_login(&self, api_host: &str, token: &Secret) -> Result<String, HttpError> {
        let request = self.usage_request(format!("https://{api_host}/user"), token);
        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        non_empty(&body, "login")
            .ok_or_else(|| HttpError::decode("GitHub /user response had no login field"))
    }

    /// Turn an HTTP failure into a message safe to show.
    fn describe_error(err: &HttpError) -> String {
        match err.status {
            Some(status @ (401 | 403)) => format!(
                "GitHub rejected the Copilot token (HTTP {status}). Sign in with GitHub again in Settings."
            ),
            _ => err.message.clone(),
        }
    }
}

impl Default for Copilot {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Copilot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Copilot")
            .field("has_credentials", &self.token().is_some())
            .field("config", &self.config.as_ref().map(PortConfig::source_path))
            .finish()
    }
}

impl Provider for Copilot {
    fn id(&self) -> ProviderId {
        ProviderId::Copilot
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::Copilot,
            title: ProviderId::Copilot.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::OAuth,
            fetched_at: now,
        };

        // 1. Credential. There is no third-party file to scrape: missing token is
        //    `notConfigured` with the device-flow hint, never an error.
        let Some(token) = self.token() else {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(NOT_CONFIGURED_HINT.to_string());
            return snapshot;
        };

        // 2. Endpoint policy — fail closed *before* the token is attached.
        let host = match self.endpoint_host() {
            Ok(host) => host,
            Err(reason) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(reason);
                return snapshot;
            }
        };
        let api_host = api_host(&host);

        // 3. Usage. 401/403 keeps the token out of the message.
        let usage = match self.fetch_usage(&api_host, &token) {
            Ok(usage) => usage,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(Self::describe_error(&err));
                return snapshot;
            }
        };

        // Masked credential until a real login is known.
        snapshot.account = Some(token.redacted());
        snapshot.plan = plan_label(&usage.copilot_plan);

        let resets_at = parse_quota_reset_date(usage.quota_reset_date.as_deref());
        let premium = make_window(
            usage.quota_snapshots.premium_interactions.as_ref(),
            resets_at,
        );
        let chat = make_window(usage.quota_snapshots.chat.as_ref(), resets_at);
        let has_unlimited = usage
            .quota_snapshots
            .premium_interactions
            .as_ref()
            .is_some_and(|s| s.unlimited)
            || usage
                .quota_snapshots
                .chat
                .as_ref()
                .is_some_and(|s| s.unlimited);

        // 4. Map onto windows. Premium is the primary (session) lane, chat the
        //    secondary (weekly) lane; placeholders never become fake bars.
        if let Some(premium) = premium {
            snapshot.windows.push(NamedRateWindow::new(
                "premium",
                "Premium",
                WindowKind::Session,
                premium,
            ));
            if let Some(chat) = chat {
                snapshot.windows.push(NamedRateWindow::new(
                    "chat",
                    "Chat",
                    WindowKind::Weekly,
                    chat,
                ));
            }
        } else if let Some(chat) = chat {
            // Chat-only plans keep the "Chat" label in the secondary slot.
            snapshot.windows.push(NamedRateWindow::new(
                "chat",
                "Chat",
                WindowKind::Weekly,
                chat,
            ));
        } else if !usage.token_based_billing && !has_unlimited {
            snapshot.status = FetchStatus::Error;
            snapshot.error = Some(
                "GitHub returned no usable Copilot quota (no premium_interactions or chat \
                 snapshot); the account may be billed per token."
                    .to_string(),
            );
            return snapshot;
        }

        // 5. Best-effort identity for the card subtitle; never fails the fetch.
        let mut notes: Vec<String> = Vec::new();
        match self.fetch_login(&api_host, &token) {
            Ok(login) => snapshot.account = Some(login),
            Err(err) => notes.push(format!(
                "GitHub identity unavailable ({})",
                err.kind.as_str()
            )),
        }
        if !notes.is_empty() {
            snapshot.error = Some(notes.join("; "));
        }

        snapshot
    }
}

/// Turn a snapshot into a window, honouring the placeholder/unlimited rules.
fn make_window(
    snapshot: Option<&QuotaSnapshot>,
    resets_at: Option<DateTime<Utc>>,
) -> Option<RateWindow> {
    let snapshot = snapshot?;
    if snapshot.unlimited || snapshot.is_placeholder() || !snapshot.has_percent_remaining {
        return None;
    }
    let mut window = RateWindow::new(snapshot.used_percent(), None, resets_at);
    window.reset_description = snapshot
        .over_quota_used_percent()
        .map(|used| format!("{used:.0}% used"));
    Some(window)
}

/// The API does not promise a reset instant; parse `quota_reset_date` when GitHub
/// sends one (RFC 3339, with or without fractional seconds, or `YYYY-MM-DD`).
fn parse_quota_reset_date(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|naive| Utc.from_utc_datetime(&naive))
}

/// A display label for `copilot_plan` (`pro` and `individual` are the same plan).
pub fn plan_label(raw: &str) -> Option<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "unknown" => None,
        "free" => Some("Free".to_string()),
        "individual" | "pro" => Some("Individual".to_string()),
        "business" => Some("Business".to_string()),
        "enterprise" => Some("Enterprise".to_string()),
        _ => Some(capitalize_words(raw.trim())),
    }
}

fn capitalize_words(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str().to_lowercase()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize a user-supplied GitHub host: strip scheme/userinfo/path, lower-case,
/// drop the default port. Mirrors the Swift `normalizedHost`.
pub fn normalized_host(raw: Option<&str>) -> String {
    let raw = raw.map(str::trim).unwrap_or("");
    if raw.is_empty() {
        return DEFAULT_HOST.to_string();
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let after_scheme = with_scheme
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(with_scheme.as_str());
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host, Some(port))
        }
        _ => (authority, None),
    };
    let host = host.trim_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return DEFAULT_HOST.to_string();
    }
    match port {
        Some(port) if port != "443" => format!("{host}:{port}"),
        _ => host,
    }
}

/// The REST API host for a GitHub host: `api.github.com`, or `api.<enterprise>`.
pub fn api_host(host: &str) -> String {
    if host == DEFAULT_HOST {
        return DEFAULT_API_HOST.to_string();
    }
    if host.starts_with("api.") {
        return host.to_string();
    }
    format!("api.{host}")
}

/// Finite `f64` at `key`, accepting JSON integers, `null` → `None`.
fn number(object: &serde_json::Value, key: &str) -> Option<f64> {
    object
        .get(key)
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite())
}

fn integer(object: &serde_json::Value, key: &str) -> Option<i64> {
    object.get(key).and_then(|value| value.as_i64())
}

fn non_empty(object: &serde_json::Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_code_parsing_tolerates_missing_optionals() {
        let code = DeviceCode::from_json(&serde_json::json!({
            "device_code": "dc",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device"
        }))
        .unwrap();
        assert_eq!(code.user_code, "ABCD-1234");
        assert_eq!(code.interval, 5);
        assert_eq!(code.expires_in, 900);
        assert_eq!(code.verification_url(), "https://github.com/login/device");
        // Never prints the device code.
        assert!(!format!("{code:?}").contains("dc"));

        assert!(DeviceCode::from_json(&serde_json::json!({})).is_none());
    }

    #[test]
    fn poll_outcomes_classify_every_github_code() {
        let token = "«redacted:gho_…»";
        assert_eq!(
            PollOutcome::from_json(&serde_json::json!({"error": "authorization_pending"})),
            PollOutcome::Pending
        );
        assert_eq!(
            PollOutcome::from_json(&serde_json::json!({"error": "slow_down"})),
            PollOutcome::SlowDown
        );
        assert_eq!(
            PollOutcome::from_json(&serde_json::json!({"error": "expired_token"})),
            PollOutcome::Expired
        );
        assert_eq!(
            PollOutcome::from_json(&serde_json::json!({"error": "access_denied"})),
            PollOutcome::Denied
        );
        assert_eq!(
            PollOutcome::from_json(
                &serde_json::json!({"access_token": token, "token_type": "bearer"})
            ),
            PollOutcome::Authorized(Secret::new(token))
        );
        match PollOutcome::from_json(&serde_json::json!({"error": "incorrect_client_credentials"}))
        {
            PollOutcome::Failed(message) => {
                assert!(message.contains("incorrect_client_credentials"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        match PollOutcome::from_json(&serde_json::json!({})) {
            PollOutcome::Failed(message) => assert!(message.contains("neither a token")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn used_percent_is_not_clamped_and_marks_over_quota() {
        let over = QuotaSnapshot::from_json(&serde_json::json!({
            "entitlement": 100, "remaining": 0, "percent_remaining": -25.0
        }));
        assert_eq!(over.used_percent(), 125.0);
        assert_eq!(over.over_quota_used_percent(), Some(125.0));

        let normal = QuotaSnapshot::from_json(&serde_json::json!({
            "entitlement": 100, "remaining": 30, "percent_remaining": 30.0
        }));
        assert_eq!(normal.used_percent(), 70.0);
        assert_eq!(normal.over_quota_used_percent(), None);
    }

    #[test]
    fn percent_remaining_is_derived_when_absent() {
        let derived = QuotaSnapshot::from_json(&serde_json::json!({
            "entitlement": 200, "remaining": 50
        }));
        assert!(derived.has_percent_remaining);
        assert_eq!(derived.percent_remaining, 25.0);
        assert_eq!(derived.used_percent(), 75.0);

        let unknown = QuotaSnapshot::from_json(&serde_json::json!({ "entitlement": 200 }));
        assert!(!unknown.has_percent_remaining);
        assert!(!unknown.is_usable());
    }

    #[test]
    fn placeholder_and_unlimited_snapshots_are_classified() {
        let bare = QuotaSnapshot::from_json(&serde_json::json!({}));
        assert!(bare.is_placeholder());

        let zeroed = QuotaSnapshot::from_json(&serde_json::json!({
            "entitlement": 0, "remaining": 0, "percent_remaining": 100
        }));
        assert!(zeroed.is_placeholder());

        let unlimited = QuotaSnapshot::from_json(&serde_json::json!({ "unlimited": true }));
        assert!(!unlimited.is_placeholder());
        assert!(unlimited.has_percent_remaining);
        assert_eq!(unlimited.percent_remaining, 100.0);
    }

    #[test]
    fn quota_snapshots_fall_back_to_unfamiliar_keys() {
        let snapshots = QuotaSnapshots::from_json(&serde_json::json!({
            "premium_requests": {"percent_remaining": 40.0},
            "chat_quota": {"percent_remaining": 80.0}
        }));
        assert_eq!(
            snapshots
                .premium_interactions
                .as_ref()
                .unwrap()
                .used_percent(),
            60.0
        );
        assert_eq!(snapshots.chat.as_ref().unwrap().used_percent(), 20.0);
    }

    #[test]
    fn monthly_quotas_become_windows() {
        let usage = CopilotUsage::from_json(&serde_json::json!({
            "copilot_plan": "business",
            "monthly_quotas": {"chat": 500, "completions": 300},
            "limited_user_quotas": {"chat": 400, "completions": 150}
        }));
        assert_eq!(
            usage
                .quota_snapshots
                .premium_interactions
                .as_ref()
                .unwrap()
                .used_percent(),
            50.0
        );
        assert_eq!(
            usage.quota_snapshots.chat.as_ref().unwrap().used_percent(),
            20.0
        );
    }

    #[test]
    fn plan_labels_follow_the_descriptor() {
        assert_eq!(plan_label("pro").as_deref(), Some("Individual"));
        assert_eq!(plan_label("individual").as_deref(), Some("Individual"));
        assert_eq!(plan_label("business").as_deref(), Some("Business"));
        assert_eq!(plan_label("unknown"), None);
        assert_eq!(plan_label("").as_deref(), None);
        assert_eq!(
            plan_label("something new").as_deref(),
            Some("Something New")
        );
    }

    #[test]
    fn hosts_are_normalized_and_mapped_to_the_api() {
        assert_eq!(normalized_host(None), "github.com");
        assert_eq!(
            normalized_host(Some("https://octocorp.ghe.com/login")),
            "octocorp.ghe.com"
        );
        assert_eq!(
            normalized_host(Some("github.mycorp.com/")),
            "github.mycorp.com"
        );
        assert_eq!(
            normalized_host(Some("https://user@host:8443/x")),
            "host:8443"
        );
        assert_eq!(normalized_host(Some("HTTPS://GitHub.COM.")), "github.com");
        assert_eq!(api_host("github.com"), "api.github.com");
        assert_eq!(api_host("octocorp.ghe.com"), "api.octocorp.ghe.com");
        assert_eq!(api_host("api.github.com"), "api.github.com");
    }

    #[test]
    fn reset_dates_accept_all_documented_shapes() {
        let instant = parse_quota_reset_date(Some("2026-10-01T00:00:00.000Z")).unwrap();
        assert_eq!(instant.to_rfc3339(), "2026-10-01T00:00:00+00:00");
        let day = parse_quota_reset_date(Some("2026-10-01")).unwrap();
        assert_eq!(day.to_rfc3339(), "2026-10-01T00:00:00+00:00");
        assert!(parse_quota_reset_date(Some("not a date")).is_none());
        assert!(parse_quota_reset_date(None).is_none());
    }

    #[test]
    fn a_snapshot_with_no_quota_at_all_is_an_error() {
        let usage = CopilotUsage::from_json(&serde_json::json!({}));
        assert!(usage.quota_snapshots.premium_interactions.is_none());
        assert!(usage.quota_snapshots.chat.is_none());
        assert!(!usage.token_based_billing);
    }
}
