//! **DeepSeek** — prepaid balance plus optional Platform-session usage detail.
//!
//! Structured exactly like [`crate::OpenRouter`] (the reference provider):
//!
//! ```text
//! pub struct DeepSeek { client: Arc<dyn HttpClient>, env: Env, config: Option<PortConfig> }
//! impl DeepSeek {
//!     pub fn new()            -> Self   // real client, real env, port config
//!     pub fn with_client(...) -> Self   // tests: fixture client, no disk, no network
//! }
//! impl Provider for DeepSeek { fn id(); fn fetch(&self, now) }
//! ```
//!
//! Behaviour implemented here, from `SPEC-apikey.md` §7 and `repo/docs/deepseek.md`:
//!
//! | Spec | Where |
//! | --- | --- |
//! | API key `DEEPSEEK_API_KEY` / `DEEPSEEK_KEY` (`providers[].apiKey`, token account) | [`DeepSeek::api_key`] |
//! | Platform session `DEEPSEEK_PLATFORM_TOKEN` / `DEEPSEEK_USER_TOKEN` (+ legacy `cookieHeader`) | [`DeepSeek::platform_token`] |
//! | `GET https://api.deepseek.com/user/balance` bearer + `Accept: application/json` | [`DeepSeek::api_balance`] |
//! | `is_available` + `balance_infos[]`; prefer a **funded USD** row | [`Balance::select`] |
//! | Platform wallet profiles `get_user_summary` (`normal_wallets` = paid, `bonus_wallets` = granted) | [`DeepSeek::platform_balance`] |
//! | `40002` / `40003` (top-level or `biz_code`) = expired session | [`is_platform_auth_error`] |
//! | Optional amount/cost detail, soft-degraded so it never erases the balance | [`DeepSeek::usage_window`] |
//!
//! Deliberately **not** implemented (documented, not guessed): Chrome
//! `localStorage` `userToken` import and the SHA-256 anti-leak profile scope of
//! §7.1. Neither is reachable from this crate (no browser reader, no credential
//! write path), so the Platform session is env/config only here and the code is
//! honest about which source it used.
//!
//! DeepSeek exposes **no** session/weekly quota. The card is balance-first; the
//! optional amount/cost detail becomes a single `usage-month` extra lane whose
//! number is reported as metadata (`usage_known: false`), never as a fake limit.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use codexbar_core::{
    DataSource, FetchStatus, MoneyBalance, NamedRateWindow, Provider, ProviderId, ProviderSnapshot,
    RateWindow, WindowKind,
};

use crate::credential::{Env, PortConfig, Secret};
use crate::http::{HttpClient, HttpError, HttpRequest};

/// Primary API key env var.
pub const ENV_API_KEY: &str = "DEEPSEEK_API_KEY";
/// Secondary API key env var.
pub const ENV_KEY: &str = "DEEPSEEK_KEY";
/// Platform session token env var (detailed usage / platform balance).
pub const ENV_PLATFORM_TOKEN: &str = "DEEPSEEK_PLATFORM_TOKEN";
/// Platform session token env var alias.
pub const ENV_USER_TOKEN: &str = "DEEPSEEK_USER_TOKEN";

/// Public API base (fixed, documented endpoint).
pub const API_BASE_URL: &str = "https://api.deepseek.com";
/// Platform (private dashboard) base. Fixed: it is **not** an API-key endpoint.
pub const PLATFORM_BASE_URL: &str = "https://platform.deepseek.com";

const ENV_ALIASES: [&str; 2] = [ENV_API_KEY, ENV_KEY];
const PLATFORM_ALIASES: [&str; 2] = [ENV_PLATFORM_TOKEN, ENV_USER_TOKEN];

/// Balance is the important call; give it room but keep the refresh bounded.
const BALANCE_TIMEOUT: Duration = Duration::from_secs(15);
/// Optional detail calls; a slow dashboard must not hold up the refresh tick.
const DETAIL_TIMEOUT: Duration = Duration::from_secs(5);

pub struct DeepSeek {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl DeepSeek {
    /// Production constructor: real HTTPS client, process environment, the port's
    /// own config file if the user has one.
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

    fn api_key(&self) -> Option<Secret> {
        match &self.config {
            Some(config) => config.resolve_api_key(ProviderId::DeepSeek, &self.env, &ENV_ALIASES),
            None => self.env.first_of(&ENV_ALIASES),
        }
    }

    /// The Platform session `userToken`: env first, then the legacy
    /// `providers[].cookieHeader` value the macOS app preserves for upgrades.
    fn platform_token(&self) -> Option<Secret> {
        self.env
            .first_of(&PLATFORM_ALIASES)
            .or_else(|| {
                self.config
                    .as_ref()
                    .and_then(|c| c.field(ProviderId::DeepSeek, "cookieHeader"))
                    .map(Secret::new)
            })
            .filter(|secret| !secret.is_empty())
    }

    fn api_balance(&self, api_key: &Secret) -> Result<Balance, HttpError> {
        let request = HttpRequest::get(format!("{API_BASE_URL}/user/balance"))
            .bearer(api_key)
            .accept_json()
            .timeout(BALANCE_TIMEOUT);
        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        Balance::from_api_json(&body)
            .ok_or_else(|| HttpError::decode("DeepSeek /user/balance had no usable balance_infos"))
    }

    fn platform_balance(&self, token: &Secret) -> Result<Balance, HttpError> {
        let request =
            HttpRequest::get(format!("{PLATFORM_BASE_URL}/api/v0/users/get_user_summary"))
                .bearer(token)
                .accept_json()
                .timeout(BALANCE_TIMEOUT);
        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        platform_summary(&body)
    }

    /// `(amount, cost)` detail for the month containing `now`, soft-degradable.
    fn platform_usage(&self, token: &Secret, now: DateTime<Utc>) -> Result<MonthUsage, HttpError> {
        let month = now.format("%-m").to_string();
        let year = now.format("%Y").to_string();

        let amount = self.platform_usage_call(
            &format!("{PLATFORM_BASE_URL}/api/v0/usage/amount?month={month}&year={year}"),
            token,
        )?;
        let cost = self.platform_usage_call(
            &format!("{PLATFORM_BASE_URL}/api/v0/usage/cost?month={month}&year={year}"),
            token,
        )?;

        let amount_body = check_platform_envelope(&amount)?;
        let cost_body = check_platform_envelope(&cost)?;
        Ok(MonthUsage::from_json(
            &amount_body,
            &cost_body,
            &now.format("%Y-%m-%d").to_string(),
        ))
    }

    fn platform_usage_call(
        &self,
        url: &str,
        token: &Secret,
    ) -> Result<serde_json::Value, HttpError> {
        let request = HttpRequest::get(url)
            .bearer(token)
            .accept_json()
            .timeout(DETAIL_TIMEOUT);
        let response = self.client.send_ok(&request)?;
        response.json::<serde_json::Value>()
    }
}

impl Default for DeepSeek {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DeepSeek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepSeek")
            .field("has_api_key", &self.api_key().is_some())
            .field("has_platform_token", &self.platform_token().is_some())
            .field("config", &self.config.as_ref().map(PortConfig::source_path))
            .finish()
    }
}

/// Normalised prepaid balance, whichever endpoint produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    pub is_available: bool,
    pub currency: String,
    pub total: f64,
    pub granted: f64,
    pub topped_up: f64,
}

impl Balance {
    /// `GET /user/balance`: `is_available` + `balance_infos[]`.
    pub fn from_api_json(body: &serde_json::Value) -> Option<Self> {
        let is_available = body.get("is_available").and_then(|v| v.as_bool())?;
        let infos = body.get("balance_infos")?.as_array()?;
        let rows: Vec<Balance> = infos
            .iter()
            .filter_map(|info| {
                let currency = info.get("currency")?.as_str()?.trim().to_string();
                if currency.is_empty() {
                    return None;
                }
                Some(Balance {
                    is_available,
                    currency,
                    total: number_or_numeric_string(info, "total_balance")?,
                    granted: number_or_numeric_string(info, "granted_balance").unwrap_or(0.0),
                    topped_up: number_or_numeric_string(info, "topped_up_balance").unwrap_or(0.0),
                })
            })
            .collect();
        // An empty `balance_infos` array is a valid "nothing funded" answer.
        Some(Self::select(rows)?.with_is_available(is_available))
    }

    /// Prefer a **funded** USD row, then any funded row, then USD, then the
    /// first row — an empty USD row must not hide a positive CNY balance.
    pub fn select(rows: Vec<Balance>) -> Option<Self> {
        if rows.is_empty() {
            return None;
        }
        if let Some(row) = rows.iter().find(|r| r.currency == "USD" && r.total > 0.0) {
            return Some(row.clone());
        }
        if let Some(row) = rows.iter().find(|r| r.total > 0.0) {
            return Some(row.clone());
        }
        if let Some(row) = rows.iter().find(|r| r.currency == "USD") {
            return Some(row.clone());
        }
        rows.first().cloned()
    }

    fn with_is_available(mut self, is_available: bool) -> Self {
        self.is_available = is_available;
        self
    }

    /// The human-readable detail the macOS card shows next to the number.
    pub fn detail(&self) -> String {
        let symbol = currency_symbol(&self.currency);
        if self.total <= 0.0 {
            format!("{symbol}0.00 — add credits at platform.deepseek.com")
        } else if !self.is_available {
            "Balance unavailable for API calls".to_string()
        } else {
            format!(
                "{}{:.2} (Paid: {}{:.2} / Granted: {}{:.2})",
                symbol, self.total, symbol, self.topped_up, symbol, self.granted
            )
        }
    }
}

/// The month-to-date amount/cost detail (`usage/amount` + `usage/cost`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonthUsage {
    pub currency: String,
    pub tokens: i64,
    pub requests: i64,
    pub cost: f64,
    pub today_tokens: i64,
    pub today_requests: i64,
    pub today_cost: f64,
}

impl MonthUsage {
    pub fn from_json(amount: &serde_json::Value, cost: &serde_json::Value, today: &str) -> Self {
        let currency = cost
            .pointer("/data/biz_data/0/currency")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("USD")
            .to_string();

        let (tokens, requests) = totals_by_category(amount.pointer("/data/biz_data/total"));
        let cost_total = cost_scalar_total(cost.pointer("/data/biz_data/0/total"));

        let (today_tokens, today_requests) = day_by_category(amount, "/data/biz_data/days", today);
        let today_cost = day_cost(cost, "/data/biz_data/0/days", today);

        Self {
            currency,
            tokens,
            requests,
            cost: cost_total,
            today_tokens,
            today_requests,
            today_cost,
        }
    }

    /// `usage-month` lane description, e.g. `This month: $0.68 · 10,000 tokens · 90 req`.
    pub fn description(&self) -> String {
        format!(
            "This month: {}{:.2} · {} tokens · {} requests",
            currency_symbol(&self.currency),
            self.cost,
            group_digits(self.tokens),
            group_digits(self.requests)
        )
    }
}

impl Provider for DeepSeek {
    fn id(&self) -> ProviderId {
        ProviderId::DeepSeek
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::DeepSeek,
            title: ProviderId::DeepSeek.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::ApiKey,
            fetched_at: now,
        };

        let api_key = self.api_key();
        let platform = self.platform_token();

        // 1. No credentials at all is `notConfigured`, not `error`: the UI turns
        //    that into an actionable setup hint.
        if api_key.is_none() && platform.is_none() {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(
                "No DeepSeek credential found — set DEEPSEEK_API_KEY (or DEEPSEEK_KEY) for \
                 the balance, and optionally DEEPSEEK_PLATFORM_TOKEN (or DEEPSEEK_USER_TOKEN) \
                 for detailed usage, or add providers[].apiKey for \"deepseek\" to \
                 %APPDATA%\\CodexBar\\config.json."
                    .to_string(),
            );
            return snapshot;
        }

        let mut notes: Vec<String> = Vec::new();

        // 2. Balance. API key wins (it is the documented public endpoint); a
        //    Platform-only install falls back to the dashboard summary.
        let (balance, source) = match &api_key {
            Some(key) => {
                snapshot.account = Some(key.redacted());
                match self.api_balance(key) {
                    Ok(balance) => (balance, DataSource::ApiKey),
                    Err(err) => {
                        snapshot.status = FetchStatus::Error;
                        snapshot.error = Some(err.message);
                        return snapshot;
                    }
                }
            }
            None => {
                let token = platform
                    .as_ref()
                    .expect("a credential exists, so a platform token does");
                snapshot.account = Some(token.redacted());
                match self.platform_balance(token) {
                    Ok(balance) => (balance, DataSource::Web),
                    Err(err) => {
                        snapshot.status = FetchStatus::Error;
                        snapshot.error = Some(err.message);
                        return snapshot;
                    }
                }
            }
        };
        snapshot.source = source;
        snapshot.balance = Some(MoneyBalance {
            amount: round2(balance.total),
            currency: balance.currency.clone(),
            label: Some(balance.detail()),
        });

        // 3. Optional detail. A failure here never erases the balance; it becomes
        //    a diagnostic note and the status stays `ok`.
        match &platform {
            Some(token) => match self.platform_usage(token, now) {
                Ok(usage) => snapshot.windows.push(
                    NamedRateWindow::new(
                        "usage-month",
                        "Usage · this month",
                        WindowKind::Extra,
                        RateWindow {
                            // Not a quota lane: the description carries the number.
                            reset_description: Some(usage.description()),
                            ..RateWindow::new(0.0, Some(43_200), None)
                        },
                    )
                    .with_usage_known(false),
                ),
                Err(err) => notes.push(format!(
                    "Detailed usage unavailable right now ({})",
                    err.kind.as_str()
                )),
            },
            None => notes.push(
                "Detailed usage needs a DeepSeek Platform session — set \
                 DEEPSEEK_PLATFORM_TOKEN (or DEEPSEEK_USER_TOKEN)."
                    .to_string(),
            ),
        }

        if !notes.is_empty() {
            snapshot.error = Some(notes.join("; "));
        }

        snapshot
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Validate a Platform envelope: `code` / `data.biz_code` must be 0.
fn check_platform_envelope(body: &serde_json::Value) -> Result<serde_json::Value, HttpError> {
    if let Some(code) = body.get("code").and_then(|v| v.as_i64()) {
        if code != 0 {
            return Err(platform_code_error(code));
        }
    }
    if let Some(code) = body.pointer("/data/biz_code").and_then(|v| v.as_i64()) {
        if code != 0 {
            return Err(platform_code_error(code));
        }
    }
    Ok(body.clone())
}

fn platform_code_error(code: i64) -> HttpError {
    if is_platform_auth_error(code) {
        HttpError::new(
            crate::http::HttpErrorKind::Status,
            "DeepSeek Platform session is missing or expired",
        )
    } else {
        HttpError::new(
            crate::http::HttpErrorKind::Status,
            format!("DeepSeek Platform returned code {code}"),
        )
    }
}

/// `40002` / `40003` (top-level or nested) mean the session is expired.
pub const fn is_platform_auth_error(code: i64) -> bool {
    code == 40002 || code == 40003
}

/// Platform `get_user_summary`: `normal_wallets` = paid, `bonus_wallets` = granted.
fn platform_summary(body: &serde_json::Value) -> Result<Balance, HttpError> {
    check_platform_envelope(body)?;
    let data = body
        .pointer("/data/biz_data")
        .ok_or_else(|| HttpError::decode("DeepSeek user summary had no biz_data"))?;

    let paid = wallet_totals(data.get("normal_wallets"));
    let granted = wallet_totals(data.get("bonus_wallets"));

    let mut currencies: Vec<String> = paid.keys().chain(granted.keys()).cloned().collect();
    currencies.sort();
    currencies.dedup();
    if currencies.is_empty() {
        return Ok(Balance {
            is_available: false,
            currency: "USD".to_string(),
            total: 0.0,
            granted: 0.0,
            topped_up: 0.0,
        });
    }

    let rows: Vec<Balance> = currencies
        .into_iter()
        .map(|currency| {
            let topped_up = paid.get(&currency).copied().unwrap_or(0.0);
            let granted = granted.get(&currency).copied().unwrap_or(0.0);
            Balance {
                is_available: topped_up + granted > 0.0,
                currency,
                total: topped_up + granted,
                granted,
                topped_up,
            }
        })
        .collect();

    Ok(Balance::select(rows).unwrap_or(Balance {
        is_available: false,
        currency: "USD".to_string(),
        total: 0.0,
        granted: 0.0,
        topped_up: 0.0,
    }))
}

/// Sum `[{balance, currency}]` rows per currency (balance may be a number or a
/// numeric string).
fn wallet_totals(wallets: Option<&serde_json::Value>) -> std::collections::BTreeMap<String, f64> {
    let mut totals = std::collections::BTreeMap::new();
    let Some(rows) = wallets.and_then(|v| v.as_array()) else {
        return totals;
    };
    for row in rows {
        let Some(currency) = row.get("currency").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(balance) = number_or_numeric_string(row, "balance") else {
            continue;
        };
        *totals.entry(currency.trim().to_string()).or_insert(0.0) += balance;
    }
    totals
}

/// `total[]` rows: `REQUEST` counts as requests, everything else as tokens.
fn totals_by_category(total: Option<&serde_json::Value>) -> (i64, i64) {
    let mut tokens = 0_i64;
    let mut requests = 0_i64;
    let Some(models) = total.and_then(|v| v.as_array()) else {
        return (0, 0);
    };
    for model in models {
        let Some(items) = model.get("usage").and_then(|v| v.as_array()) else {
            continue;
        };
        for item in items {
            let kind = item
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_ascii_uppercase();
            let amount = item.get("amount").and_then(value_as_i64).unwrap_or(0);
            if kind == "REQUEST" {
                requests += amount;
            } else {
                tokens += amount;
            }
        }
    }
    (tokens, requests)
}

fn cost_scalar_total(total: Option<&serde_json::Value>) -> f64 {
    let mut sum = 0.0;
    let Some(models) = total.and_then(|v| v.as_array()) else {
        return 0.0;
    };
    for model in models {
        let Some(items) = model.get("usage").and_then(|v| v.as_array()) else {
            continue;
        };
        for item in items {
            let kind = item
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_ascii_uppercase();
            if kind != "REQUEST" {
                sum += item.get("amount").and_then(value_as_f64).unwrap_or(0.0);
            }
        }
    }
    sum
}

fn day_by_category(body: &serde_json::Value, days_pointer: &str, today: &str) -> (i64, i64) {
    let Some(days) = body.pointer(days_pointer).and_then(|v| v.as_array()) else {
        return (0, 0);
    };
    for day in days {
        if day.get("date").and_then(|v| v.as_str()) != Some(today) {
            continue;
        }
        return totals_by_category(day.get("data"));
    }
    (0, 0)
}

fn day_cost(body: &serde_json::Value, days_pointer: &str, today: &str) -> f64 {
    let Some(days) = body.pointer(days_pointer).and_then(|v| v.as_array()) else {
        return 0.0;
    };
    for day in days {
        if day.get("date").and_then(|v| v.as_str()) != Some(today) {
            continue;
        }
        return cost_scalar_total(day.get("data"));
    }
    0.0
}

fn number_or_numeric_string(object: &serde_json::Value, key: &str) -> Option<f64> {
    object.get(key).and_then(value_as_f64)
}

fn value_as_f64(value: &serde_json::Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return number.is_finite().then_some(number);
    }
    let parsed = value.as_str()?.trim().parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

fn value_as_i64(value: &serde_json::Value) -> Option<i64> {
    if let Some(int) = value.as_i64() {
        return Some(int);
    }
    if let Some(number) = value.as_f64() {
        return number.is_finite().then_some(number as i64);
    }
    value.as_str()?.trim().parse::<i64>().ok()
}

fn currency_symbol(currency: &str) -> &'static str {
    if currency.eq_ignore_ascii_case("CNY") {
        "¥"
    } else {
        "$"
    }
}

fn group_digits(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if value < 0 {
        format!("-{out}")
    } else {
        out
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_balance_prefers_a_funded_usd_row() {
        let body = serde_json::json!({
            "is_available": true,
            "balance_infos": [
                {"currency": "CNY", "total_balance": "0.00", "granted_balance": "0.00", "topped_up_balance": "0.00"},
                {"currency": "USD", "total_balance": "18.40", "granted_balance": "3.40", "topped_up_balance": "15.00"}
            ]
        });
        let balance = Balance::from_api_json(&body).unwrap();
        assert_eq!(balance.currency, "USD");
        assert_eq!(balance.total, 18.4);
        assert_eq!(balance.granted, 3.4);
        assert_eq!(balance.topped_up, 15.0);
        assert_eq!(balance.detail(), "$18.40 (Paid: $15.00 / Granted: $3.40)");
    }

    #[test]
    fn api_balance_falls_back_to_a_funded_non_usd_row() {
        let body = serde_json::json!({
            "is_available": true,
            "balance_infos": [
                {"currency": "USD", "total_balance": "0.00"},
                {"currency": "CNY", "total_balance": "42.00"}
            ]
        });
        let balance = Balance::from_api_json(&body).unwrap();
        assert_eq!(balance.currency, "CNY");
        assert_eq!(balance.total, 42.0);
        assert!(balance.detail().starts_with('¥'));
    }

    #[test]
    fn zero_balance_says_add_credits_and_unavailable_says_so() {
        let zero = Balance::from_api_json(&serde_json::json!({
            "is_available": true,
            "balance_infos": [{"currency": "USD", "total_balance": "0.00"}]
        }))
        .unwrap();
        assert!(zero.detail().contains("add credits"));

        let unavailable = Balance::from_api_json(&serde_json::json!({
            "is_available": false,
            "balance_infos": [{"currency": "USD", "total_balance": "5.00"}]
        }))
        .unwrap();
        assert_eq!(unavailable.detail(), "Balance unavailable for API calls");
    }

    #[test]
    fn balance_requires_is_available_and_a_valid_schema() {
        assert!(Balance::from_api_json(&serde_json::json!({"balance_infos": []})).is_none());
        assert!(Balance::from_api_json(&serde_json::json!({"is_available": true})).is_none());
        // Empty balance_infos is a valid "nothing funded" answer, not an error.
        let empty = Balance::from_api_json(&serde_json::json!({
            "is_available": false, "balance_infos": []
        }));
        assert!(empty.is_none());
    }

    #[test]
    fn platform_wallets_split_paid_and_granted() {
        let body = serde_json::json!({
            "code": 0,
            "data": {"biz_code": 0, "biz_data": {
                "normal_wallets": [{"balance": 15.0, "currency": "USD"}],
                "bonus_wallets": [{"balance": "3.4", "currency": "USD"}]
            }}
        });
        let balance = platform_summary(&body).unwrap();
        assert_eq!(balance.total, 18.4);
        assert_eq!(balance.topped_up, 15.0);
        assert_eq!(balance.granted, 3.4);
        assert!(balance.is_available);
    }

    #[test]
    fn platform_codes_40002_and_40003_mean_an_expired_session() {
        for code in [40002, 40003] {
            let body = serde_json::json!({"code": code});
            let err = platform_summary(&body).unwrap_err();
            assert!(err.message.contains("expired"));
            assert!(is_platform_auth_error(code));
        }
        assert!(!is_platform_auth_error(0));
        assert!(!is_platform_auth_error(500));
    }

    #[test]
    fn month_usage_sums_totals_and_today() {
        let amount = serde_json::json!({"data": {"biz_data": {
            "total": [
                {"model": "deepseek-chat", "usage": [
                    {"type": "PROMPT_CACHE_HIT_TOKEN", "amount": "2000"},
                    {"type": "PROMPT_CACHE_MISS_TOKEN", "amount": "5000"},
                    {"type": "RESPONSE_TOKEN", "amount": "3000"},
                    {"type": "REQUEST", "amount": "90"}
                ]}
            ],
            "days": [
                {"date": "2026-09-10", "data": [
                    {"model": "deepseek-chat", "usage": [
                        {"type": "PROMPT_CACHE_MISS_TOKEN", "amount": "1000"},
                        {"type": "REQUEST", "amount": "12"}
                    ]}
                ]}
            ]
        }}});
        let cost = serde_json::json!({"data": {"biz_data": [{"currency": "USD",
            "total": [{"model": "deepseek-chat", "usage": [
                {"type": "PROMPT_CACHE_MISS_TOKEN", "amount": "0.35"},
                {"type": "RESPONSE_TOKEN", "amount": "0.31"}
            ]}],
            "days": [{"date": "2026-09-10", "data": [{"model": "deepseek-chat", "usage": [
                {"type": "PROMPT_CACHE_HIT_TOKEN", "amount": "0.002"}
            ]}]}]
        }]}});
        let usage = MonthUsage::from_json(&amount, &cost, "2026-09-10");
        assert_eq!(usage.currency, "USD");
        assert_eq!(usage.tokens, 10_000);
        assert_eq!(usage.requests, 90);
        assert!((usage.cost - 0.66).abs() < 1e-9);
        assert_eq!(usage.today_tokens, 1_000);
        assert_eq!(usage.today_requests, 12);
        assert!((usage.today_cost - 0.002).abs() < 1e-9);
        assert_eq!(
            usage.description(),
            "This month: $0.66 · 10,000 tokens · 90 requests"
        );
    }

    #[test]
    fn grouping_inserts_thousands_separators() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }
}
