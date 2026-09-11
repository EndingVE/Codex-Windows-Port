//! **OpenRouter** — the reference provider.
//!
//! Every one of the other 13 providers should be structured exactly like this:
//!
//! ```text
//! pub struct X { client: Arc<dyn HttpClient>, env: Env, config: Option<PortConfig> }
//! impl X {
//!     pub fn new()             -> Self                 // real client, real env, port config
//!     pub fn with_client(...)  -> Self                 // tests: fixture client, no disk, no network
//! }
//! impl Provider for X {
//!     fn id(&self) -> ProviderId
//!     fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot   // never panics, never blocks on input
//! }
//! ```
//!
//! Behaviour implemented here, from `SPEC-apikey.md` §6:
//!
//! | Spec | Where |
//! | --- | --- |
//! | Credential precedence (token account → `providers[].apiKey` → env) | [`PortConfig::resolve_api_key`] |
//! | `OPENROUTER_API_URL` must be HTTPS, fail closed | [`secure_base_url`] |
//! | `GET {base}/credits` required, `GET {base}/key` optional with a 1 s deadline | [`OpenRouter::fetch`] |
//! | `balance = max(0, total_credits - total_usage)` | [`OpenRouter::credits_snapshot`] |
//! | `/key` percentage precedence: `limit - clamp(limit_remaining)` → `usage_{daily,weekly,monthly}` → `usage` | [`KeyInfo::used_percent`] |
//! | Soft degradation: `/key` failure keeps the credits and adds a diagnostic note | [`OpenRouter::key_window`] |
//!
//! Not implemented here (documented, not guessed): the 30-day Activity chart of
//! §6.5. It needs `OPENROUTER_MANAGEMENT_API_KEY`, a fixed host
//! ([`ACTIVITY_URL_FIXED`] — it must **not** follow `OPENROUTER_API_URL`) and a
//! strict row/dedupe validator. It is a chart, not a limit, so it does not gate
//! the provider.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use codexbar_core::{
    DataSource, FetchStatus, MoneyBalance, NamedRateWindow, Provider, ProviderId, ProviderSnapshot,
    RateWindow, WindowKind,
};

use crate::credential::{Env, PortConfig, Secret};
use crate::http::{self, secure_base_url, HttpClient, HttpError, HttpErrorKind, HttpRequest};

/// Primary credential env var.
pub const ENV_API_KEY: &str = "OPENROUTER_API_KEY";
/// Optional management key (30-day Activity only).
pub const ENV_MANAGEMENT_API_KEY: &str = "OPENROUTER_MANAGEMENT_API_KEY";
/// Base URL override — HTTPS only.
pub const ENV_API_URL: &str = "OPENROUTER_API_URL";
/// Optional attribution headers.
pub const ENV_HTTP_REFERER: &str = "OPENROUTER_HTTP_REFERER";
/// Optional attribution header.
pub const ENV_X_TITLE: &str = "OPENROUTER_X_TITLE";

/// Default API base.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// Activity lives on the **fixed** public host and never follows
/// `OPENROUTER_API_URL` (`SPEC-apikey.md` §6.3). Declared for the worker who
/// picks up the 30-day chart.
pub const ACTIVITY_URL_FIXED: &str = "https://openrouter.ai/api/v1/activity";

/// `/key` is a best-effort probe: a slow endpoint must not hold up the refresh
/// tick, and a failure only costs the second meter.
const KEY_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const CREDITS_TIMEOUT: Duration = Duration::from_secs(10);

const ENV_ALIASES: [&str; 1] = [ENV_API_KEY];

pub struct OpenRouter {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl OpenRouter {
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

    /// Constructor for tests and for embedding: no disk reads, no network.
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

    fn api_key(&self) -> Option<Secret> {
        match &self.config {
            Some(config) => config.resolve_api_key(ProviderId::OpenRouter, &self.env, &ENV_ALIASES),
            None => self.env.first_of(&ENV_ALIASES),
        }
    }

    fn base_url(&self) -> Result<String, String> {
        secure_base_url(self.env.get_str(ENV_API_URL).as_deref(), DEFAULT_BASE_URL)
    }
}

impl Default for OpenRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OpenRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouter")
            .field("has_credentials", &self.api_key().is_some())
            .field("config", &self.config.as_ref().map(PortConfig::source_path))
            .finish()
    }
}

/// The `/credits` payload (`data.total_credits`, `data.total_usage`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Credits {
    pub total_credits: f64,
    pub total_usage: f64,
}

impl Credits {
    pub fn from_json(body: &serde_json::Value) -> Option<Self> {
        let data = body.get("data")?;
        let total_credits = number(data, "total_credits")?;
        let total_usage = number(data, "total_usage")?;
        Some(Self {
            total_credits,
            total_usage,
        })
    }

    /// `max(0, total_credits - total_usage)` — never a negative balance.
    pub fn balance(&self) -> f64 {
        (self.total_credits - self.total_usage).max(0.0)
    }

    /// Consumed share of the purchased credits, for the extra lane.
    pub fn used_percent(&self) -> Option<f64> {
        if self.total_credits <= 0.0 {
            return None;
        }
        Some(round1(
            (self.total_usage / self.total_credits * 100.0).clamp(0.0, 100.0),
        ))
    }
}

/// The `/key` payload (`SPEC-apikey.md` §6.4).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyInfo {
    pub limit: Option<f64>,
    pub limit_remaining: Option<f64>,
    pub usage: Option<f64>,
    pub usage_daily: Option<f64>,
    pub usage_weekly: Option<f64>,
    pub usage_monthly: Option<f64>,
    /// `"daily"` / `"weekly"` / `"monthly"` — which usage figure is current.
    pub limit_reset: Option<String>,
    pub rate_limit_requests: Option<f64>,
    pub rate_limit_interval: Option<String>,
}

impl KeyInfo {
    pub fn from_json(body: &serde_json::Value) -> Option<Self> {
        let data = body.get("data")?;
        let rate_limit = data.get("rate_limit");
        Some(Self {
            limit: number(data, "limit"),
            limit_remaining: number(data, "limit_remaining"),
            usage: number(data, "usage"),
            usage_daily: number(data, "usage_daily"),
            usage_weekly: number(data, "usage_weekly"),
            usage_monthly: number(data, "usage_monthly"),
            limit_reset: non_empty_string(data, "limit_reset"),
            rate_limit_requests: rate_limit.and_then(|r| number(r, "requests")),
            rate_limit_interval: rate_limit.and_then(|r| non_empty_string(r, "interval")),
        })
    }

    /// Percentage of the key's budget consumed, following the spec's precedence.
    ///
    /// `None` means "do not publish a meter": no positive limit, or no
    /// non-negative usage figure to compare against it.
    pub fn used_percent(&self) -> Option<f64> {
        let limit = self.limit.filter(|l| l.is_finite() && *l > 0.0)?;

        if let Some(remaining) = self.limit_remaining.filter(|r| r.is_finite()) {
            let used = limit - remaining.clamp(0.0, limit);
            if used >= 0.0 {
                return Some(round1((used / limit * 100.0).clamp(0.0, 100.0)));
            }
        }

        let current = match self.limit_reset.as_deref().map(str::to_ascii_lowercase) {
            Some(reset) if reset == "daily" => self.usage_daily,
            Some(reset) if reset == "weekly" => self.usage_weekly,
            Some(reset) if reset == "monthly" => self.usage_monthly,
            _ => None,
        }
        .or(self.usage);

        let used = current.filter(|u| u.is_finite() && *u >= 0.0)?;
        Some(round1((used / limit * 100.0).clamp(0.0, 100.0)))
    }

    /// `(id, title, window_minutes)` for the key meter, based on `limit_reset`.
    pub fn lane(&self) -> (&'static str, &'static str, Option<i64>) {
        match self.limit_reset.as_deref().map(str::to_ascii_lowercase) {
            Some(reset) if reset == "daily" => {
                ("key-limit-daily", "API key limit · daily", Some(1_440))
            }
            Some(reset) if reset == "weekly" => {
                ("key-limit-weekly", "API key limit · weekly", Some(10_080))
            }
            Some(reset) if reset == "monthly" => {
                ("key-limit-monthly", "API key limit · monthly", Some(43_200))
            }
            _ => ("key-limit", "API key limit", None),
        }
    }
}

impl Provider for OpenRouter {
    fn id(&self) -> ProviderId {
        ProviderId::OpenRouter
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::OpenRouter,
            title: ProviderId::OpenRouter.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            // Credential-scraped providers advertise how they got the numbers.
            source: DataSource::ApiKey,
            fetched_at: now,
        };

        // 1. Credentials. Missing credentials are `notConfigured`, not `error`:
        //    the card turns that into an actionable setup hint.
        let Some(api_key) = self.api_key() else {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(
                "No OpenRouter API key found — set OPENROUTER_API_KEY or add \
                 providers[].apiKey for \"openrouter\" to %APPDATA%\\CodexBar\\config.json."
                    .to_string(),
            );
            return snapshot;
        };
        // Masked, never the value: this string reaches the UI, logs and screenshots.
        snapshot.account = Some(api_key.redacted());

        // 2. Base URL policy — fail closed *before* the bearer is attached.
        let base = match self.base_url() {
            Ok(base) => base,
            Err(reason) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!("OPENROUTER_API_URL is not usable: {reason}"));
                return snapshot;
            }
        };

        // 3. Required: credits.
        let credits = match self.credits(&base, &api_key) {
            Ok(credits) => credits,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(err.message);
                return snapshot;
            }
        };

        snapshot.balance = Some(MoneyBalance {
            amount: round2(credits.balance()),
            currency: "USD".into(),
            label: Some("Credits remaining".into()),
        });
        if let Some(used) = credits.used_percent() {
            snapshot.windows.push(NamedRateWindow::new(
                "credits",
                "Credits · 30d",
                WindowKind::Extra,
                RateWindow::new(used, Some(43_200), None),
            ));
        }

        // 4. Optional: key limit, with a 1 s deadline and soft degradation.
        let mut notes: Vec<String> = Vec::new();
        match self.key_window(&base, &api_key) {
            Ok(Some(window)) => snapshot.windows.push(window),
            Ok(None) => { /* the key has no budget limit: no meter, no noise */ }
            Err(err) => notes.push(format!(
                "API key limit unavailable right now ({})",
                err.kind.as_str()
            )),
        }

        if !notes.is_empty() {
            // Status stays `ok`: the credits are real. The note is diagnostic.
            snapshot.error = Some(notes.join("; "));
        }

        snapshot
    }
}

impl OpenRouter {
    fn credits(&self, base: &str, api_key: &Secret) -> Result<Credits, HttpError> {
        let request = self
            .attributed(HttpRequest::get(format!("{base}/credits")))
            .bearer(api_key)
            .accept_json()
            .timeout(CREDITS_TIMEOUT);

        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        Credits::from_json(&body).ok_or_else(|| {
            HttpError::decode("OpenRouter /credits response had no data.total_credits/total_usage")
        })
    }

    /// `Ok(None)` when the key simply has no limit configured.
    fn key_window(
        &self,
        base: &str,
        api_key: &Secret,
    ) -> Result<Option<NamedRateWindow>, HttpError> {
        let request = self
            .attributed(HttpRequest::get(format!("{base}/key")))
            .bearer(api_key)
            .accept_json()
            .timeout(KEY_PROBE_TIMEOUT);

        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        let Some(info) = KeyInfo::from_json(&body) else {
            return Ok(None);
        };
        let Some(used) = info.used_percent() else {
            return Ok(None);
        };
        let (id, title, minutes) = info.lane();
        Ok(Some(NamedRateWindow::new(
            id,
            title,
            WindowKind::Extra,
            RateWindow::new(used, minutes, None),
        )))
    }

    /// Attribution headers (`OPENROUTER_HTTP_REFERER`, `OPENROUTER_X_TITLE`).
    fn attributed(&self, request: HttpRequest) -> HttpRequest {
        let mut request = request;
        if let Some(referer) = self.env.get_str(ENV_HTTP_REFERER) {
            request = request.header("HTTP-Referer", referer);
        }
        let title = self
            .env
            .get_str(ENV_X_TITLE)
            .unwrap_or_else(|| "CodexBar".to_string());
        request.header("X-Title", title)
    }
}

/// Finite `f64` at `key`, accepting JSON integers, `null` → `None`.
fn number(object: &serde_json::Value, key: &str) -> Option<f64> {
    object
        .get(key)
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite())
}

fn non_empty_string(object: &serde_json::Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Exposed for the CLI's `--live` self-check and for the next worker: the HTTP
/// error kinds a provider is expected to degrade on rather than fail hard.
pub fn is_degradable(err: &HttpError) -> bool {
    matches!(
        err.kind,
        HttpErrorKind::Timeout | HttpErrorKind::Connect | HttpErrorKind::Client
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credits_body() -> String {
        r#"{"data":{"total_credits":100.0,"total_usage":37.5}}"#.to_string()
    }

    #[test]
    fn credits_balance_never_goes_negative() {
        let over = Credits {
            total_credits: 10.0,
            total_usage: 12.5,
        };
        assert_eq!(over.balance(), 0.0);
        let normal = Credits {
            total_credits: 100.0,
            total_usage: 37.5,
        };
        assert_eq!(normal.balance(), 62.5);
        assert_eq!(normal.used_percent(), Some(37.5));
        assert_eq!(
            Credits {
                total_credits: 0.0,
                total_usage: 0.0
            }
            .used_percent(),
            None
        );
    }

    #[test]
    fn credits_parsing_tolerates_absent_or_null_fields() {
        assert!(Credits::from_json(&serde_json::json!({"data": {}})).is_none());
        assert!(Credits::from_json(
            &serde_json::json!({"data": {"total_credits": null, "total_usage": 1.0}})
        )
        .is_none());
        assert!(Credits::from_json(&serde_json::json!({})).is_none());
        assert_eq!(
            Credits::from_json(&serde_json::json!({"data":{"total_credits":5,"total_usage":1}})),
            Some(Credits {
                total_credits: 5.0,
                total_usage: 1.0
            })
        );
        let _ = credits_body();
    }

    #[test]
    fn key_percent_prefers_limit_remaining() {
        let info = KeyInfo::from_json(&serde_json::json!({"data":{
            "limit": 50.0, "limit_remaining": 12.5, "usage": 5.0, "limit_reset": "monthly"
        }}))
        .unwrap();
        // (50 - 12.5) / 50 = 75 %
        assert_eq!(info.used_percent(), Some(75.0));
    }

    #[test]
    fn key_percent_clamps_a_negative_remaining() {
        let info = KeyInfo::from_json(&serde_json::json!({"data":{
            "limit": 50.0, "limit_remaining": -10.0
        }}))
        .unwrap();
        assert_eq!(info.used_percent(), Some(100.0));
    }

    #[test]
    fn key_percent_falls_back_to_the_reset_scoped_usage() {
        let info = KeyInfo::from_json(&serde_json::json!({"data":{
            "limit": 100.0, "limit_remaining": null,
            "usage": 40.0, "usage_daily": 2.0, "usage_weekly": 25.0, "usage_monthly": 90.0,
            "limit_reset": "weekly"
        }}))
        .unwrap();
        assert_eq!(info.used_percent(), Some(25.0));
        assert_eq!(info.lane().0, "key-limit-weekly");
        assert_eq!(info.lane().2, Some(10_080));
    }

    #[test]
    fn key_percent_falls_back_to_cumulative_usage() {
        let info = KeyInfo::from_json(&serde_json::json!({"data":{
            "limit": 100.0, "usage": 40.0, "limit_reset": "something-new"
        }}))
        .unwrap();
        assert_eq!(info.used_percent(), Some(40.0));
        assert_eq!(info.lane().0, "key-limit");
        assert_eq!(info.lane().2, None);
    }

    #[test]
    fn key_meter_is_not_published_without_a_positive_limit() {
        let info = KeyInfo::from_json(&serde_json::json!({"data":{
            "limit": 0.0, "usage": 10.0
        }}))
        .unwrap();
        assert_eq!(info.used_percent(), None);
        let info = KeyInfo::from_json(&serde_json::json!({"data":{"usage": 10.0}})).unwrap();
        assert_eq!(info.used_percent(), None);
    }
}
