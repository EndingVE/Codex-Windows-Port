//! **MiniMax** — Coding Plan quota over the API-token path, plus the
//! web-session billing history, from `SPEC-apikey.md` §4.
//!
//! Same construction as [`crate::providers::openrouter`]: a constructor pair, a
//! no-panic `fetch`, an endpoint policy that fails closed *before* the bearer is
//! attached, and fixture tests that never touch the network.
//!
//! | Spec | Where |
//! | --- | --- |
//! | Token precedence `MINIMAX_CODING_API_KEY` > `MINIMAX_API_KEY`, then config `apiKey` | [`MiniMax::api_token`] |
//! | Global (`platform/api.minimax.io`) vs China (`platform/api.minimaxi.com`) | [`MiniMaxRegion`] |
//! | `MINIMAX_HOST`, `MINIMAX_CODING_PLAN_URL`, `MINIMAX_REMAINS_URL`, `MINIMAX_BILLING_HISTORY_URL` | [`MiniMax::remains_urls`], [`MiniMax::billing_url`] |
//! | `MINIMAX_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES` → only `minimax.io` / `minimaxi.com` | [`MiniMax::strict_hosts`], [`host_is_allowed`] |
//! | `{api}/v1/token_plan/remains` then `{api}/v1/api/openplatform/coding_plan/remains`, global → China retry | [`MiniMax::fetch_remains`] |
//! | `model_remains[]` → interval + weekly lanes, `status 3` placeholders skipped | [`MiniMaxPlan::from_json`] |
//! | Percentage from `current_interval_remaining_percent`, else from the counts | [`MiniMaxService::from_interval`] |
//!
//! **Billing history** (`{platform}/account/amount?page=1&limit=100&aggregate=false`)
//! is a web-session chart: it needs a `Cookie` header (`MINIMAX_COOKIE` /
//! `MINIMAX_COOKIE_HEADER` or config `cookieHeader`), and `schemaVersion 1` has
//! no chart/detail slot. The URL builder and the record aggregation are
//! implemented and tested ([`MiniMax::billing_url`], [`BillingSummary`]) but
//! `fetch` does not call them — the same call the reference provider makes about
//! its own 30-day Activity chart. `fetch` uses the API-token path only, and a
//! billing failure can never discard the quota.
//!
//! Nothing here ever prints a credential: the token goes out through [`Secret`]
//! and every error message is built from already-redacted HTTP errors.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use codexbar_core::{
    DataSource, FetchStatus, MoneyBalance, NamedRateWindow, Provider, ProviderId, ProviderSnapshot,
    RateWindow, WindowKind,
};
use serde_json::Value;

use crate::credential::{redact_secrets_in_text, Env, PortConfig, Secret};
use crate::http::{host_is_allowed, secure_base_url, HttpClient, HttpRequest};

/// Coding-plan token (highest priority).
pub const ENV_CODING_API_KEY: &str = "MINIMAX_CODING_API_KEY";
/// Standard API token.
pub const ENV_API_KEY: &str = "MINIMAX_API_KEY";
/// Manual `Cookie:` header / "Copy as cURL" string.
pub const ENV_COOKIE: &str = "MINIMAX_COOKIE";
/// Manual `Cookie:` header (alias).
pub const ENV_COOKIE_HEADER: &str = "MINIMAX_COOKIE_HEADER";
/// Bare host override (`platform.minimaxi.com`).
pub const ENV_HOST: &str = "MINIMAX_HOST";
/// Full coding-plan page URL override.
pub const ENV_CODING_PLAN_URL: &str = "MINIMAX_CODING_PLAN_URL";
/// Full remains URL override.
pub const ENV_REMAINS_URL: &str = "MINIMAX_REMAINS_URL";
/// Full billing-history URL override.
pub const ENV_BILLING_HISTORY_URL: &str = "MINIMAX_BILLING_HISTORY_URL";
/// Strict override mode: only MiniMax-owned hosts are accepted.
pub const ENV_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES: &str =
    "MINIMAX_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES";

/// Coding-plan HTML page path.
pub const CODING_PLAN_PATH: &str = "user-center/payment/coding-plan";
/// `cycle_type=3` query used by the coding-plan page.
pub const CODING_PLAN_QUERY: &str = "cycle_type=3";
/// Legacy coding-plan remains path.
pub const CODING_PLAN_REMAINS_PATH: &str = "v1/api/openplatform/coding_plan/remains";
/// Token-plan remains path (tried first).
pub const TOKEN_PLAN_REMAINS_PATH: &str = "v1/token_plan/remains";
/// Billing-history path on the platform host.
pub const BILLING_HISTORY_PATH: &str = "account/amount";

/// Hosts a strict override is allowed to point at.
pub const ALLOWED_HOST_SUFFIXES: [&str; 2] = ["minimax.io", "minimaxi.com"];

const REMAINS_TIMEOUT: Duration = Duration::from_secs(15);
const BILLING_TIMEOUT: Duration = Duration::from_secs(15);

/// Billing history is paged; the plugin requests 100 records per page.
pub const BILLING_HISTORY_LIMIT: u32 = 100;

/// Which MiniMax region a fetch targets (`MiniMaxAPIRegion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiniMaxRegion {
    Global,
    ChinaMainland,
}

impl MiniMaxRegion {
    /// Web platform host.
    pub const fn platform_base(self) -> &'static str {
        match self {
            MiniMaxRegion::Global => "https://platform.minimax.io",
            MiniMaxRegion::ChinaMainland => "https://platform.minimaxi.com",
        }
    }

    /// API host.
    pub const fn api_base(self) -> &'static str {
        match self {
            MiniMaxRegion::Global => "https://api.minimax.io",
            MiniMaxRegion::ChinaMainland => "https://api.minimaxi.com",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            MiniMaxRegion::Global => "global",
            MiniMaxRegion::ChinaMainland => "cn",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "global" => Some(MiniMaxRegion::Global),
            "cn" | "china" | "china-mainland" | "china_mainland" | "chinamainland" => {
                Some(MiniMaxRegion::ChinaMainland)
            }
            _ => None,
        }
    }
}

pub struct MiniMax {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl MiniMax {
    /// Production constructor: real HTTPS client, process environment, the port's
    /// own config file.
    pub fn new() -> Self {
        let env = Env::from_process();
        let config = PortConfig::load(&env).unwrap_or(None);
        Self {
            client: crate::http::shared_client(),
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

    /// `MINIMAX_CODING_API_KEY` > `MINIMAX_API_KEY`, then the config `apiKey`.
    ///
    /// The coding-plan key wins so a standard `sk-api-*` key cannot mask a
    /// coding-plan `sk-cp-*` key.
    pub fn api_token(&self) -> Option<Secret> {
        self.env
            .first_of(&[ENV_CODING_API_KEY, ENV_API_KEY])
            .or_else(|| {
                self.config.as_ref().and_then(|config| {
                    config
                        .active_token_account(ProviderId::MiniMax)
                        .or_else(|| config.api_key(ProviderId::MiniMax))
                })
            })
    }

    /// True when the token looks like a coding-plan key (diagnostic only).
    pub fn api_key_kind(&self) -> Option<&'static str> {
        let token = self.api_token()?;
        if token.expose().starts_with("sk-cp-") {
            Some("codingPlan")
        } else if token.expose().starts_with("sk-api-") {
            Some("standard")
        } else {
            Some("unknown")
        }
    }

    /// The manual cookie header, if the user configured one. Only used by the
    /// billing-history capability; `fetch` is API-token only.
    pub fn cookie_header(&self) -> Option<Secret> {
        self.env
            .first_of(&[ENV_COOKIE, ENV_COOKIE_HEADER])
            .or_else(|| {
                self.config
                    .as_ref()
                    .and_then(|config| config.field(ProviderId::MiniMax, "cookieHeader"))
                    .map(Secret::new)
            })
    }

    /// Selected region: an explicit config value, then the `MINIMAX_HOST` host
    /// suffix, then Global.
    pub fn region(&self) -> MiniMaxRegion {
        if let Some(raw) = self
            .config
            .as_ref()
            .and_then(|config| config.field(ProviderId::MiniMax, "region"))
            .and_then(|raw| MiniMaxRegion::parse(&raw))
        {
            return raw;
        }
        if let Some(host) = self.env.get_str(ENV_HOST) {
            if host.to_ascii_lowercase().contains("minimaxi.com") {
                return MiniMaxRegion::ChinaMainland;
            }
        }
        MiniMaxRegion::Global
    }

    /// `MINIMAX_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES` truthiness.
    pub fn strict_hosts(&self) -> bool {
        self.env
            .get_str(ENV_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES)
            .map(|raw| {
                matches!(
                    raw.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    }

    /// Remains URLs, in the order the fetcher tries them. `MINIMAX_REMAINS_URL`
    /// replaces the list; `MINIMAX_HOST` replaces the host.
    pub fn remains_urls(&self, region: MiniMaxRegion) -> Result<Vec<String>, String> {
        if let Some(raw) = self.env.get_str(ENV_REMAINS_URL) {
            return Ok(vec![self.override_url(ENV_REMAINS_URL, &raw)?]);
        }
        if let Some(raw) = self.env.get_str(ENV_HOST) {
            let host = self.override_url(ENV_HOST, &raw)?;
            return Ok(vec![self.join_path(&host, CODING_PLAN_REMAINS_PATH)]);
        }
        let api = region.api_base();
        Ok(vec![
            self.join_path(api, TOKEN_PLAN_REMAINS_PATH),
            self.join_path(api, CODING_PLAN_REMAINS_PATH),
        ])
    }

    /// Coding-plan page URL (`{platform}/user-center/payment/coding-plan?cycle_type=3`).
    pub fn coding_plan_url(&self, region: MiniMaxRegion) -> Result<String, String> {
        if let Some(raw) = self.env.get_str(ENV_CODING_PLAN_URL) {
            let url = self.override_url(ENV_CODING_PLAN_URL, &raw)?;
            return Ok(append_query(&url, CODING_PLAN_QUERY));
        }
        if let Some(raw) = self.env.get_str(ENV_HOST) {
            let host = self.override_url(ENV_HOST, &raw)?;
            return Ok(append_query(
                &self.join_path(&host, CODING_PLAN_PATH),
                CODING_PLAN_QUERY,
            ));
        }
        Ok(append_query(
            &self.join_path(region.platform_base(), CODING_PLAN_PATH),
            CODING_PLAN_QUERY,
        ))
    }

    /// Billing-history URL (`{platform}/account/amount?page=…&limit=…&aggregate=false`).
    pub fn billing_url(
        &self,
        region: MiniMaxRegion,
        page: u32,
        limit: u32,
    ) -> Result<String, String> {
        let base = if let Some(raw) = self.env.get_str(ENV_BILLING_HISTORY_URL) {
            self.override_url(ENV_BILLING_HISTORY_URL, &raw)?
        } else if let Some(raw) = self.env.get_str(ENV_HOST) {
            let host = self.override_url(ENV_HOST, &raw)?;
            self.join_path(&host, BILLING_HISTORY_PATH)
        } else {
            self.join_path(region.platform_base(), BILLING_HISTORY_PATH)
        };
        let base = strip_query(&base);
        Ok(format!("{base}?page={page}&limit={limit}&aggregate=false"))
    }

    /// `secure_base_url` + the strict allowlist, when strict mode is on.
    fn override_url(&self, key: &str, raw: &str) -> Result<String, String> {
        let url = secure_base_url(Some(raw), "")
            .map_err(|reason| format!("{key} is not usable: {reason}"))?;
        if self.strict_hosts() && !host_is_allowed(&url, &ALLOWED_HOST_SUFFIXES) {
            return Err(format!(
                "{key} must point at a MiniMax-owned host (minimax.io / minimaxi.com) \
                 while MINIMAX_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES is set."
            ));
        }
        Ok(url)
    }

    /// Append `path` when `base` carries no path of its own.
    fn join_path(&self, base: &str, path: &str) -> String {
        let rest = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
        let has_path = rest
            .split(['/', '?', '#'])
            .nth(1)
            .is_some_and(|segment| !segment.is_empty());
        if has_path {
            base.to_string()
        } else {
            format!(
                "{}/{}",
                base.trim_end_matches('/'),
                path.trim_start_matches('/')
            )
        }
    }

    /// One remains request + parse.
    fn remains_once(
        &self,
        url: &str,
        token: &Secret,
        now: DateTime<Utc>,
    ) -> Result<MiniMaxPlan, MiniMaxError> {
        let request = HttpRequest::get(url)
            .bearer(token)
            .accept_json()
            .header("Content-Type", "application/json")
            .header("MM-API-Source", "CodexBar")
            .timeout(REMAINS_TIMEOUT);

        let response = self
            .client
            .execute(&request)
            .map_err(|err| MiniMaxError::Http(err.message))?;
        match response.status {
            200 => {}
            401 | 403 => return Err(MiniMaxError::InvalidCredentials),
            other => return Err(MiniMaxError::Api(format!("HTTP {other}"))),
        }
        let body: Value = response
            .json()
            .map_err(|err| MiniMaxError::Parse(err.message))?;
        MiniMaxPlan::from_json(&body, now)
    }

    /// Try every remains URL, then fall back from Global to China mainland when
    /// the token was rejected (`SPEC-apikey.md` §4.3, "Selección (Auto)").
    pub fn fetch_remains(
        &self,
        region: MiniMaxRegion,
        token: &Secret,
        now: DateTime<Utc>,
    ) -> Result<MiniMaxPlan, MiniMaxError> {
        let result = self.try_region(region, token, now);
        if region == MiniMaxRegion::Global {
            if let Err(MiniMaxError::InvalidCredentials) = result {
                return self.try_region(MiniMaxRegion::ChinaMainland, token, now);
            }
        }
        result
    }

    fn try_region(
        &self,
        region: MiniMaxRegion,
        token: &Secret,
        now: DateTime<Utc>,
    ) -> Result<MiniMaxPlan, MiniMaxError> {
        let urls = self.remains_urls(region).map_err(MiniMaxError::Config)?;
        let mut last: Option<MiniMaxError> = None;
        for url in urls {
            match self.remains_once(&url, token, now) {
                Ok(plan) => return Ok(plan),
                Err(error) => {
                    let retryable = error.retryable();
                    last = Some(error);
                    if !retryable {
                        break;
                    }
                }
            }
        }
        Err(last
            .unwrap_or_else(|| MiniMaxError::Parse("Missing MiniMax API remains URL.".to_string())))
    }

    /// Billing history for a web session. Not called by `fetch` — see the module
    /// header. `cookie` is the raw `Cookie:` header value.
    pub fn billing_summary(
        &self,
        region: MiniMaxRegion,
        page: u32,
        now: DateTime<Utc>,
    ) -> Result<BillingSummary, String> {
        let Some(cookie) = self.cookie_header() else {
            return Err(
                "MiniMax billing history needs a web session (MINIMAX_COOKIE / \
                 MINIMAX_COOKIE_HEADER)."
                    .to_string(),
            );
        };
        let url = self.billing_url(region, page, BILLING_HISTORY_LIMIT)?;
        let mut request = HttpRequest::get(&url)
            .accept_json()
            .header("Cookie", cookie.expose())
            .header("X-Requested-With", "XMLHttpRequest")
            .timeout(BILLING_TIMEOUT);
        if let Some(token) = self.api_token() {
            request = request.bearer(&token);
        }
        let response = self.client.execute(&request).map_err(|err| err.message)?;
        if response.status != 200 {
            return Err(format!(
                "MiniMax billing history returned HTTP {}",
                response.status
            ));
        }
        let body: Value = response.json().map_err(|err| err.message)?;
        BillingSummary::from_json(&body, now)
    }
}

impl Default for MiniMax {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MiniMax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiniMax")
            .field("has_credentials", &self.api_token().is_some())
            .field("config", &self.config.as_ref().map(PortConfig::source_path))
            .finish()
    }
}

impl Provider for MiniMax {
    fn id(&self) -> ProviderId {
        ProviderId::MiniMax
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::MiniMax,
            title: ProviderId::MiniMax.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::ApiKey,
            fetched_at: now,
        };

        // Missing credentials are `notConfigured`, not `error`.
        let Some(token) = self.api_token() else {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(
                "No MiniMax API token found — set MINIMAX_CODING_API_KEY or MINIMAX_API_KEY, or \
                 add providers[].apiKey for \"minimax\" to %APPDATA%\\CodexBar\\config.json."
                    .to_string(),
            );
            return snapshot;
        };
        // Masked, never the value: this string reaches the UI, logs and screenshots.
        snapshot.account = Some(token.redacted());

        let region = self.region();
        match self.fetch_remains(region, &token, now) {
            Ok(plan) => {
                snapshot.plan = plan.plan.clone();
                snapshot.windows = plan.windows();
                if let Some(points) = plan.points_balance {
                    snapshot.balance = Some(MoneyBalance {
                        amount: round2(points),
                        currency: "Points".to_string(),
                        label: Some("MiniMax points balance".to_string()),
                    });
                }
                if snapshot.windows.is_empty() {
                    snapshot.status = FetchStatus::Error;
                    snapshot.error = Some(
                        "MiniMax returned a coding plan with no usable quota windows.".to_string(),
                    );
                }
            }
            Err(error) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(error.message());
            }
        }

        snapshot
    }
}

/// Why a MiniMax request failed. `message()` is always safe to display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiniMaxError {
    /// The API token was rejected (401/403, `status_code` 1004, or a login page).
    InvalidCredentials,
    /// A MiniMax `status_code` / non-2xx answer.
    Api(String),
    /// A non-2xx answer that is worth retrying on another endpoint.
    Parse(String),
    /// A transport failure.
    Http(String),
    /// An endpoint override that could not be used.
    Config(String),
}

impl MiniMaxError {
    pub fn message(&self) -> String {
        match self {
            MiniMaxError::InvalidCredentials => "MiniMax rejected the API token — check \
                 MINIMAX_CODING_API_KEY / MINIMAX_API_KEY (a `sk-cp-*` coding-plan key wins over a \
                 standard `sk-api-*` key)."
                .to_string(),
            MiniMaxError::Api(message) => {
                format!("MiniMax API error: {}", redact_secrets_in_text(message))
            }
            MiniMaxError::Parse(message) => {
                format!(
                    "MiniMax response could not be parsed: {}",
                    redact_secrets_in_text(message)
                )
            }
            MiniMaxError::Http(message) => message.clone(),
            MiniMaxError::Config(message) => message.clone(),
        }
    }

    /// Whether another endpoint (or the China host) is worth trying.
    fn retryable(&self) -> bool {
        match self {
            MiniMaxError::InvalidCredentials | MiniMaxError::Parse(_) | MiniMaxError::Http(_) => {
                true
            }
            MiniMaxError::Api(message) => {
                message.contains("HTTP 404") || message.contains("HTTP 405")
            }
            MiniMaxError::Config(_) => false,
        }
    }
}

/// The parsed coding-plan response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MiniMaxPlan {
    pub plan: Option<String>,
    pub points_balance: Option<f64>,
    /// Percentage of the first model's interval window, for the fallback lane.
    pub used_percent: Option<f64>,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub services: Vec<MiniMaxService>,
}

impl MiniMaxPlan {
    /// `base_resp.status_code != 0` is an error; `data.model_remains[]` carries
    /// the quota lanes.
    pub fn from_json(body: &Value, now: DateTime<Utc>) -> Result<Self, MiniMaxError> {
        let data = body.get("data").unwrap_or(body);
        let base_resp = data.get("base_resp").or_else(|| body.get("base_resp"));
        if let Some(status) = base_resp
            .and_then(|value| value.get("status_code"))
            .and_then(as_integer)
        {
            if status != 0 {
                let message = base_resp
                    .and_then(|value| value.get("status_msg"))
                    .and_then(Value::as_str)
                    .unwrap_or("status_code")
                    .to_string();
                let lower = message.to_ascii_lowercase();
                if status == 1004
                    || lower.contains("cookie")
                    || lower.contains("log in")
                    || lower.contains("login")
                {
                    return Err(MiniMaxError::InvalidCredentials);
                }
                return Err(MiniMaxError::Api(message));
            }
        }

        let model_remains = data
            .get("model_remains")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if model_remains.is_empty() {
            return Err(MiniMaxError::Parse(
                "Missing MiniMax coding plan data (model_remains)".to_string(),
            ));
        }

        let mut services: Vec<MiniMaxService> = Vec::new();
        for item in &model_remains {
            let Some(model_name) = item.get("model_name").and_then(Value::as_str) else {
                continue;
            };
            let service_type = map_model_name(model_name);
            if let Some(interval) = MiniMaxService::from_interval(item, &service_type, now) {
                services.push(interval);
            }
            if should_render_weekly(model_name) {
                if let Some(weekly) = MiniMaxService::from_weekly(item, &service_type, now) {
                    services.push(weekly);
                }
            }
        }

        // Backward-compatible single-lane fields, from the first model.
        let first = model_remains.first();
        let remaining_percent =
            first.and_then(|item| number(item, "current_interval_remaining_percent"));
        let has_percent = remaining_percent.is_some();
        let total = first
            .and_then(|item| int(item, "current_interval_total_count"))
            .filter(|_| {
                !has_percent
                    || first.and_then(|item| int(item, "current_interval_total_count")) != Some(0)
            });
        let remaining = first
            .and_then(|item| int(item, "current_interval_usage_count"))
            .filter(|_| {
                !has_percent
                    || first.and_then(|item| int(item, "current_interval_usage_count")) != Some(0)
            });
        let used_percent = remaining_percent
            .map(|percent| (100.0 - percent).clamp(0.0, 100.0))
            .or_else(|| {
                let total = total?;
                if total <= 0 {
                    return None;
                }
                let remaining = remaining?;
                Some(((total - remaining).max(0) as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
            });

        let window_minutes = first
            .map(|item| {
                window_minutes_from(
                    epoch(item, "start_time").and_then(epoch_to_datetime),
                    epoch(item, "end_time").and_then(epoch_to_datetime),
                )
            })
            .unwrap_or(None);
        let resets_at = first.and_then(|item| resets_at(item, "end_time", "remains_time", now));

        Ok(Self {
            plan: plan_name(data),
            points_balance: points_balance(data),
            used_percent,
            window_minutes,
            resets_at,
            services,
        })
    }

    /// The contract lanes, primary text lane first, then weekly, then the rest.
    pub fn windows(&self) -> Vec<NamedRateWindow> {
        let mut ordered: Vec<(usize, &MiniMaxService)> = self.services.iter().enumerate().collect();
        ordered.sort_by_key(|(index, service)| {
            (
                if service.is_primary_text_lane() { 0 } else { 1 },
                if service.is_weekly() { 1 } else { 0 },
                *index,
            )
        });
        let mut windows: Vec<NamedRateWindow> = ordered
            .into_iter()
            .map(|(_, service)| service.window())
            .collect();

        // A response whose only model is an unavailable placeholder still carries
        // a usable first-lane percentage; publish it as a single lane.
        if windows.is_empty() {
            if let Some(percent) = self.used_percent {
                let mut window = RateWindow::new(percent, self.window_minutes, self.resets_at);
                window.reset_description = Some("Coding Plan".to_string());
                windows.push(NamedRateWindow::new(
                    "quota",
                    "Coding Plan",
                    WindowKind::Extra,
                    window,
                ));
            }
        }
        dedupe_ids(windows)
    }
}

/// One quota lane of a coding-plan response.
#[derive(Debug, Clone, PartialEq)]
pub struct MiniMaxService {
    pub service_type: String,
    pub window_type: String,
    pub usage: i64,
    pub limit: i64,
    pub percent: f64,
    pub unlimited: bool,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub reset_description: String,
}

impl MiniMaxService {
    /// `current_interval_*`: the interval lane (5 hours / today).
    pub fn from_interval(item: &Value, service_type: &str, now: DateTime<Utc>) -> Option<Self> {
        Self::from_fields(
            FieldSet {
                service_type,
                window_type: None,
                total: int(item, "current_interval_total_count"),
                remaining: int(item, "current_interval_usage_count"),
                remaining_percent: number(item, "current_interval_remaining_percent"),
                status: int(item, "current_interval_status"),
                start: epoch(item, "start_time"),
                end: epoch(item, "end_time"),
                remains: int(item, "remains_time"),
                boost_permille: boost_permille(item, "interval"),
            },
            now,
        )
    }

    /// `current_weekly_*`: the weekly lane (text-generation models only).
    pub fn from_weekly(item: &Value, service_type: &str, now: DateTime<Utc>) -> Option<Self> {
        Self::from_fields(
            FieldSet {
                service_type,
                window_type: Some("Weekly"),
                total: int(item, "current_weekly_total_count"),
                remaining: int(item, "current_weekly_usage_count"),
                remaining_percent: number(item, "current_weekly_remaining_percent"),
                status: int(item, "current_weekly_status"),
                start: epoch(item, "weekly_start_time"),
                end: epoch(item, "weekly_end_time"),
                remains: int(item, "weekly_remains_time"),
                boost_permille: boost_permille(item, "weekly"),
            },
            now,
        )
    }

    fn from_fields(fields: FieldSet<'_>, now: DateTime<Utc>) -> Option<Self> {
        let start = fields.start.and_then(epoch_to_datetime);
        let end = fields.end.and_then(epoch_to_datetime);
        let window_type = fields
            .window_type
            .map(str::to_string)
            .unwrap_or_else(|| window_type_from(start, end));

        if is_unavailable_placeholder(&fields) {
            return None;
        }

        let resets_at = resets_at_from(end, fields.remains, now);
        let unlimited = is_unlimited(&fields, &window_type);
        let (usage, limit, percent) = if unlimited {
            (0, 0, 0.0)
        } else if let Some(remaining_percent) = fields.remaining_percent {
            let quota_limit = quota_limit(fields.boost_permille);
            let percent = (100.0 - remaining_percent).clamp(0.0, 100.0);
            let usage = (percent * quota_limit as f64 / 100.0).round() as i64;
            (usage, quota_limit, percent)
        } else {
            let total = fields.total?;
            let remaining = fields.remaining?;
            if total <= 0 {
                return None;
            }
            let used = (total - remaining).max(0);
            let percent = (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
            (used, total, percent)
        };

        let window_minutes = if fields.window_type == Some("Weekly") {
            Some(7 * 24 * 60)
        } else {
            window_minutes_for(&window_type, start, end)
        };

        let reset_description = if unlimited {
            "Unlimited".to_string()
        } else {
            reset_description(resets_at, now)
        };

        Some(Self {
            service_type: fields.service_type.to_string(),
            window_type,
            usage,
            limit,
            percent: percent.clamp(0.0, 100.0),
            unlimited,
            window_minutes,
            resets_at,
            reset_description,
        })
    }

    pub fn is_weekly(&self) -> bool {
        self.window_type.eq_ignore_ascii_case("weekly")
    }

    pub fn is_primary_text_lane(&self) -> bool {
        matches!(self.service_type.as_str(), "General" | "Text Generation")
    }

    fn window(&self) -> NamedRateWindow {
        let kind = if self.is_weekly() {
            WindowKind::Weekly
        } else if self.window_minutes == Some(300) {
            WindowKind::Session
        } else {
            WindowKind::Extra
        };
        let slug = slugify(&self.service_type);
        let (id, title) = if self.is_weekly() {
            (
                format!("{slug}-weekly"),
                format!("{} · weekly", self.service_type),
            )
        } else {
            (slug, self.service_type.clone())
        };
        let mut window = RateWindow::new(self.percent, self.window_minutes, self.resets_at);
        window.reset_description = Some(self.reset_description.clone());
        NamedRateWindow::new(id, title, kind, window)
    }
}

struct FieldSet<'a> {
    service_type: &'a str,
    window_type: Option<&'a str>,
    total: Option<i64>,
    remaining: Option<i64>,
    remaining_percent: Option<f64>,
    status: Option<i64>,
    start: Option<i64>,
    end: Option<i64>,
    remains: Option<i64>,
    boost_permille: Option<i64>,
}

/// A lane the subscription does not include: status 3, no counts, 100 % left.
fn is_unavailable_placeholder(fields: &FieldSet<'_>) -> bool {
    let unlimited_candidate = fields.window_type == Some("Weekly")
        && matches!(fields.service_type, "General" | "Text Generation")
        && fields
            .remaining_percent
            .is_some_and(|percent| percent >= 100.0);
    if unlimited_candidate && fields.status == Some(3) {
        // An unlimited weekly lane is real; it is not a placeholder.
        return false;
    }
    fields.status == Some(3)
        && fields.total.unwrap_or(0) == 0
        && fields.remaining.unwrap_or(0) == 0
        && fields
            .remaining_percent
            .is_some_and(|percent| percent >= 100.0)
}

fn is_unlimited(fields: &FieldSet<'_>, window_type: &str) -> bool {
    fields.status == Some(3)
        && matches!(fields.service_type, "General" | "Text Generation")
        && window_type.eq_ignore_ascii_case("weekly")
        && fields
            .remaining_percent
            .is_some_and(|percent| percent >= 100.0)
}

/// Boosted plans report their ceiling in permille (1000 = 100).
fn quota_limit(boost_permille: Option<i64>) -> i64 {
    match boost_permille {
        Some(boost) if boost > 0 => ((boost as f64) / 10.0).round().max(1.0) as i64,
        _ => 100,
    }
}

fn window_type_from(start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> String {
    let (Some(start), Some(end)) = (start, end) else {
        return "Unknown".to_string();
    };
    let hours = (end - start).num_seconds() as f64 / 3_600.0;
    if (23.0..=25.0).contains(&hours) {
        "Today".to_string()
    } else if (4.0..=6.0).contains(&hours) {
        "5 hours".to_string()
    } else if (1.0..23.0).contains(&hours) {
        format!("{} hours", hours as i64)
    } else {
        "Custom".to_string()
    }
}

fn window_minutes_for(
    window_type: &str,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
) -> Option<i64> {
    match window_type.trim().to_ascii_lowercase().as_str() {
        "today" => return Some(24 * 60),
        "weekly" => return Some(7 * 24 * 60),
        other => {
            let mut parts = other.split_whitespace();
            if let (Some(value), Some(unit)) = (parts.next(), parts.next()) {
                if let Ok(value) = value.parse::<i64>() {
                    let minutes = match unit {
                        "hour" | "hours" | "h" | "hr" | "hrs" => Some(value * 60),
                        "minute" | "minutes" | "min" | "mins" | "m" => Some(value),
                        "day" | "days" | "d" => Some(value * 24 * 60),
                        _ => None,
                    };
                    if minutes.is_some() {
                        return minutes;
                    }
                }
            }
        }
    }
    window_minutes_from(start, end)
}

/// Elapsed minutes between two instants (used when the window type is unknown).
fn window_minutes_from(start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> Option<i64> {
    let (Some(start), Some(end)) = (start, end) else {
        return None;
    };
    let minutes = (end - start).num_minutes();
    (minutes > 0).then_some(minutes)
}

fn reset_description(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(resets_at) = resets_at.filter(|instant| *instant > now) else {
        return "Resets soon".to_string();
    };
    let seconds = (resets_at - now).num_seconds();
    if seconds < 60 {
        format!("Resets in {seconds} seconds")
    } else if seconds < 3_600 {
        let minutes = seconds / 60;
        format!("Resets in {minutes} minute{}", plural(minutes))
    } else if seconds < 86_400 {
        let hours = seconds / 3_600;
        format!("Resets in {hours} hour{}", plural(hours))
    } else {
        let days = seconds / 86_400;
        format!("Resets in {days} day{}", plural(days))
    }
}

fn plural(value: i64) -> &'static str {
    if value == 1 {
        ""
    } else {
        "s"
    }
}

/// `model_name` → the display service type (`MiniMaxUsageFetcher+ModelMapping`).
pub fn map_model_name(model_name: &str) -> String {
    let lower = model_name.trim().to_ascii_lowercase();
    if lower == "general" || lower == "video" {
        return capitalize(&lower);
    }
    if is_text_generation_model_name(model_name) {
        return "Text Generation".to_string();
    }
    if lower.contains("speech") {
        return "Text to Speech".to_string();
    }
    if lower.contains("hailuo") && lower.contains("fast") {
        return "Image to Video".to_string();
    }
    if lower.contains("hailuo") {
        return "Text to Video".to_string();
    }
    if lower.starts_with("image-") {
        return "Image Generation".to_string();
    }
    if lower.contains("music") {
        return "Music Generation".to_string();
    }
    model_name.trim().to_string()
}

fn is_text_generation_model_name(model_name: &str) -> bool {
    let lower = model_name.to_ascii_lowercase();
    lower == "general" || lower.contains("minimax-m") || lower.starts_with("m2.")
}

/// Weekly quota is a text-model concept; media lanes have no weekly bucket.
pub fn should_render_weekly(model_name: &str) -> bool {
    is_text_generation_model_name(model_name)
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn plan_name(data: &Value) -> Option<String> {
    [
        "current_subscribe_title",
        "plan_name",
        "combo_title",
        "current_plan_title",
    ]
    .iter()
    .filter_map(|key| data.get(*key).and_then(Value::as_str))
    .map(str::trim)
    .find(|value| !value.is_empty())
    .map(str::to_string)
    .or_else(|| {
        data.get("current_combo_card")
            .and_then(|card| card.get("title"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn points_balance(data: &Value) -> Option<f64> {
    [
        "points_balance",
        "point_balance",
        "credits_balance",
        "credit_balance",
        "balance",
    ]
    .iter()
    .find_map(|key| number(data, key))
}

/// `end_time` when it is in the future, else `now + remains`.
fn resets_at_from(
    end: Option<DateTime<Utc>>,
    remains: Option<i64>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if let Some(end) = end.filter(|instant| *instant > now) {
        return Some(end);
    }
    let remains = remains.filter(|value| *value > 0)?;
    let seconds = if remains > 1_000_000 {
        remains / 1_000
    } else {
        remains
    };
    Some(now + chrono::Duration::seconds(seconds))
}

fn resets_at(
    item: &Value,
    end_key: &str,
    remains_key: &str,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    resets_at_from(
        epoch(item, end_key).and_then(epoch_to_datetime),
        int(item, remains_key),
        now,
    )
}

fn boost_permille(item: &Value, prefix: &str) -> Option<i64> {
    let keys = [
        format!("{prefix}_boost_permill"),
        format!("{prefix}_boost_permille"),
    ];
    keys.iter().find_map(|key| int(item, key))
}

fn dedupe_ids(windows: Vec<NamedRateWindow>) -> Vec<NamedRateWindow> {
    let mut seen: Vec<String> = Vec::new();
    windows
        .into_iter()
        .map(|mut window| {
            if seen.contains(&window.id) {
                let mut suffix = 2;
                let mut candidate = format!("{}-{suffix}", window.id);
                while seen.contains(&candidate) {
                    suffix += 1;
                    candidate = format!("{}-{suffix}", window.id);
                }
                window.id = candidate;
            }
            seen.push(window.id.clone());
            window
        })
        .collect()
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(character.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        "quota".to_string()
    } else {
        slug
    }
}

fn append_query(url: &str, query: &str) -> String {
    if query.is_empty() {
        return url.to_string();
    }
    if url.contains('?') {
        format!("{url}&{query}")
    } else {
        format!("{url}?{query}")
    }
}

fn strip_query(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or(url).to_string()
}

/// `created_at` epoch: milliseconds when it looks like one, seconds otherwise.
fn epoch_to_datetime(epoch: i64) -> Option<DateTime<Utc>> {
    if epoch > 1_000_000_000_000 {
        DateTime::from_timestamp_millis(epoch)
    } else if epoch > 1_000_000_000 {
        DateTime::from_timestamp(epoch, 0)
    } else {
        None
    }
}

fn epoch(item: &Value, key: &str) -> Option<i64> {
    int(item, key)
}

fn int(item: &Value, key: &str) -> Option<i64> {
    item.get(key).and_then(as_integer)
}

fn number(item: &Value, key: &str) -> Option<f64> {
    item.get(key).and_then(|value| {
        value
            .as_f64()
            .filter(|float| float.is_finite())
            .or_else(|| {
                value
                    .as_str()
                    .and_then(|raw| raw.trim().parse::<f64>().ok())
                    .filter(|float| float.is_finite())
            })
    })
}

/// JSON integer accepting `12` and `"12"` but not `12.5`.
fn as_integer(value: &Value) -> Option<i64> {
    if let Some(int) = value.as_i64() {
        return Some(int);
    }
    if let Some(float) = value.as_f64() {
        if float.fract() == 0.0 {
            return Some(float as i64);
        }
        return None;
    }
    value
        .as_str()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
}

// ---------------------------------------------------------------------------
// Billing history (`SPEC-apikey.md` §4.3) — web-session only, not wired to fetch.
// ---------------------------------------------------------------------------

/// One day of billing history.
#[derive(Debug, Clone, PartialEq)]
pub struct BillingDay {
    pub day: String,
    pub tokens: i64,
}

/// A per-model / per-method billing roll-up.
#[derive(Debug, Clone, PartialEq)]
pub struct BillingBreakdown {
    pub name: String,
    pub tokens: i64,
}

/// The aggregated `charge_records[]` of a billing-history page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BillingSummary {
    pub today_tokens: i64,
    pub last_30_days_tokens: i64,
    pub today_cash: Option<f64>,
    pub last_30_days_cash: Option<f64>,
    pub daily: Vec<BillingDay>,
    pub top_models: Vec<BillingBreakdown>,
    pub top_methods: Vec<BillingBreakdown>,
}

impl BillingSummary {
    /// A `base_resp.status_code != 0` answer is an error; otherwise the records
    /// are aggregated over the last 30 days (UTC days, `now` supplied by the
    /// caller so the result is deterministic in tests).
    pub fn from_json(body: &Value, now: DateTime<Utc>) -> Result<Self, String> {
        let status = body
            .get("base_resp")
            .and_then(|base| base.get("status_code"))
            .and_then(as_integer)
            .unwrap_or(0);
        if status != 0 {
            let message = body
                .get("base_resp")
                .and_then(|base| base.get("status_msg"))
                .and_then(Value::as_str)
                .unwrap_or("status_code");
            return Err(format!(
                "MiniMax billing error: {}",
                redact_secrets_in_text(message)
            ));
        }
        let records = body
            .get("charge_records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(Self::aggregate(&records, now))
    }

    pub fn aggregate(records: &[Value], now: DateTime<Utc>) -> Self {
        let start_of_today = now.date_naive();
        let start_of_window = start_of_today - chrono::Duration::days(29);

        let mut daily: std::collections::BTreeMap<String, (i64, f64, bool)> =
            std::collections::BTreeMap::new();
        let mut methods: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        let mut models: std::collections::HashMap<String, i64> = std::collections::HashMap::new();

        for record in records {
            let result = record
                .get("result")
                .and_then(Value::as_str)
                .or_else(|| record.get("status").and_then(Value::as_str));
            if let Some(result) = result {
                if !result.trim().eq_ignore_ascii_case("SUCCESS") {
                    continue;
                }
            }
            let Some(date) = record_date(record) else {
                continue;
            };
            if date < start_of_window || date > start_of_today {
                continue;
            }
            let day = date.format("%Y-%m-%d").to_string();
            let tokens = token_count(record);
            let cash = cash_value(record);
            let bucket = daily.entry(day).or_insert((0, 0.0, false));
            bucket.0 += tokens;
            if let Some(cash) = cash {
                bucket.1 += cash;
                bucket.2 = true;
            }
            if let Some(name) = record.get("method").and_then(Value::as_str) {
                *methods.entry(name.to_string()).or_insert(0) += tokens;
            }
            if let Some(name) = record.get("model").and_then(Value::as_str) {
                *models.entry(name.to_string()).or_insert(0) += tokens;
            }
        }

        let today_key = start_of_today.format("%Y-%m-%d").to_string();
        let today = daily.get(&today_key).copied();
        let daily_rows: Vec<BillingDay> = daily
            .into_iter()
            .map(|(day, (tokens, _, _))| BillingDay { day, tokens })
            .collect();
        let last_30_days_tokens = daily_rows.iter().map(|row| row.tokens).sum();
        let last_30_days_cash = {
            let values: Vec<f64> = daily_rows
                .iter()
                .filter_map(|row| records_cash(records, &row.day))
                .collect();
            if values.is_empty() {
                None
            } else {
                Some(values.into_iter().sum())
            }
        };

        Self {
            today_tokens: today.map(|bucket| bucket.0).unwrap_or(0),
            last_30_days_tokens,
            today_cash: today.filter(|bucket| bucket.2).map(|bucket| bucket.1),
            last_30_days_cash,
            daily: daily_rows,
            top_models: breakdowns(models),
            top_methods: breakdowns(methods),
        }
    }
}

/// Sum of the daily cash buckets — kept separate so `last_30_days_cash` stays
/// `None` when no record carried a cash value.
fn records_cash(records: &[Value], day: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut seen = false;
    for record in records {
        let Some(date) = record_date(record) else {
            continue;
        };
        if date.format("%Y-%m-%d").to_string() != day {
            continue;
        }
        if let Some(cash) = cash_value(record) {
            total += cash;
            seen = true;
        }
    }
    seen.then_some(total)
}

fn breakdowns(totals: std::collections::HashMap<String, i64>) -> Vec<BillingBreakdown> {
    let mut rows: Vec<BillingBreakdown> = totals
        .into_iter()
        .map(|(name, tokens)| BillingBreakdown { name, tokens })
        .collect();
    rows.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.name.cmp(&b.name)));
    rows.truncate(20);
    rows
}

fn token_count(record: &Value) -> i64 {
    if let Some(consume) = int(record, "consume_token") {
        if consume > 0 {
            return consume;
        }
    }
    let input = int(record, "consume_input_token").unwrap_or(0);
    let output = int(record, "consume_output_token").unwrap_or(0);
    (input + output).max(0)
}

fn cash_value(record: &Value) -> Option<f64> {
    number(record, "consume_cash_after_voucher").or_else(|| number(record, "consume_cash"))
}

fn record_date(record: &Value) -> Option<NaiveDate> {
    if let Some(created) = int(record, "created_at").and_then(epoch_to_datetime) {
        return Some(created.date_naive());
    }
    if let Some(ymd) = record.get("ymd").and_then(Value::as_str) {
        for format in ["%Y-%m-%d", "%Y%m%d", "%Y/%m/%d"] {
            if let Ok(date) = NaiveDate::parse_from_str(ymd.trim(), format) {
                return Some(date);
            }
        }
    }
    if let Some(consume_time) = record.get("consume_time").and_then(Value::as_str) {
        for format in ["%Y-%m-%d %H:%M:%S", "%Y/%m/%d %H:%M:%S"] {
            if let Ok(stamp) = NaiveDateTime::parse_from_str(consume_time.trim(), format) {
                return Some(stamp.date());
            }
        }
        if let Ok(stamp) = chrono::DateTime::parse_from_rfc3339(consume_time.trim()) {
            return Some(stamp.date_naive());
        }
    }
    None
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_757_540_800_000).unwrap()
    }

    #[test]
    fn model_names_map_to_the_shared_service_types() {
        assert_eq!(map_model_name("general"), "General");
        assert_eq!(map_model_name("video"), "Video");
        assert_eq!(map_model_name("MiniMax-M2"), "Text Generation");
        assert_eq!(map_model_name("m2.1"), "Text Generation");
        assert_eq!(map_model_name("speech-hd"), "Text to Speech");
        assert_eq!(map_model_name("Hailuo-2.3-Fast"), "Image to Video");
        assert_eq!(map_model_name("Hailuo-2.3"), "Text to Video");
        assert_eq!(map_model_name("image-01"), "Image Generation");
        assert_eq!(map_model_name("music-2.5"), "Music Generation");
        assert!(should_render_weekly("general"));
        assert!(!should_render_weekly("video"));
    }

    #[test]
    fn a_percentage_quota_uses_remaining_percent_and_the_boost_ceiling() {
        let item = serde_json::json!({
            "model_name": "general",
            "current_interval_total_count": 0,
            "current_interval_usage_count": 0,
            "current_interval_remaining_percent": 62.5,
            "current_interval_status": 1,
            "interval_boost_permille": 1000,
            "start_time": 1_757_540_800_i64,
            "end_time": 1_757_540_800_i64 + 5 * 3600
        });
        let service = MiniMaxService::from_interval(&item, "General", now()).unwrap();
        assert_eq!(service.percent, 37.5);
        assert_eq!(service.limit, 100);
        assert_eq!(service.usage, 38);
        assert_eq!(service.window_minutes, Some(300));
        assert_eq!(service.window_type, "5 hours");
    }

    #[test]
    fn an_unavailable_placeholder_lane_is_skipped() {
        let item = serde_json::json!({
            "model_name": "video",
            "current_interval_total_count": 0,
            "current_interval_usage_count": 0,
            "current_interval_remaining_percent": 100,
            "current_interval_status": 3
        });
        assert!(MiniMaxService::from_interval(&item, "Video", now()).is_none());
    }

    #[test]
    fn an_unlimited_text_weekly_lane_stays_visible() {
        let item = serde_json::json!({
            "model_name": "general",
            "current_weekly_total_count": 0,
            "current_weekly_usage_count": 0,
            "current_weekly_remaining_percent": 100,
            "current_weekly_status": 3
        });
        let service = MiniMaxService::from_weekly(&item, "General", now()).unwrap();
        assert!(service.unlimited);
        assert_eq!(service.window_minutes, Some(10_080));
        assert_eq!(service.reset_description, "Unlimited");
    }

    #[test]
    fn counts_drive_the_percentage_when_no_remaining_percent_is_reported() {
        let item = serde_json::json!({
            "model_name": "general",
            "current_interval_total_count": 200,
            "current_interval_usage_count": 50,
            "current_interval_status": 1
        });
        let service = MiniMaxService::from_interval(&item, "General", now()).unwrap();
        assert_eq!(service.percent, 75.0);
        assert_eq!(service.usage, 150);
        assert_eq!(service.limit, 200);
    }

    #[test]
    fn billing_records_aggregate_over_the_last_thirty_utc_days() {
        let body = serde_json::json!({
            "base_resp": {"status_code": 0},
            "charge_records": [
                {"result": "SUCCESS", "created_at": 1_757_540_800_i64, "consume_token": 100,
                 "consume_cash_after_voucher": 1.5, "model": "MiniMax-M2", "method": "api"},
                {"result": "FAIL", "created_at": 1_757_540_800_i64, "consume_token": 999},
                {"status": "SUCCESS", "ymd": "2026-08-01", "consume_input_token": 5,
                 "consume_output_token": 7, "model": "abab"}
            ]
        });
        let summary = BillingSummary::from_json(&body, now()).unwrap();
        assert_eq!(summary.last_30_days_tokens, 100);
        assert_eq!(summary.today_cash, Some(1.5));
        assert_eq!(summary.top_models[0].name, "MiniMax-M2");
    }

    #[test]
    fn strict_mode_only_accepts_provider_owned_hosts() {
        let env = Env::empty()
            .with(ENV_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES, "true")
            .with(
                ENV_REMAINS_URL,
                "https://proxy.example.com/coding_plan/remains",
            );
        let provider = MiniMax::with_client(Arc::new(crate::testing::FixtureClient::empty()), env);
        assert!(provider.strict_hosts());
        assert!(provider.remains_urls(MiniMaxRegion::Global).is_err());

        let env = Env::empty()
            .with(ENV_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES, "1")
            .with(
                ENV_REMAINS_URL,
                "https://platform.minimaxi.com/v1/token_plan/remains",
            );
        let provider = MiniMax::with_client(Arc::new(crate::testing::FixtureClient::empty()), env);
        assert!(provider.remains_urls(MiniMaxRegion::Global).is_ok());
    }
}
