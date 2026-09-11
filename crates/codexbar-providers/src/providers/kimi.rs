//! **Kimi (Kimi Code)** — subscription quota from the Kimi Code API plus the
//! Kimi web billing service.
//!
//! `SPEC-apikey.md` §3 covers **two** billing surfaces that must never be
//! conflated, and this module sits on 3A:
//!
//! * **3A Kimi Code** (this module) — the `kimi.com/code` subscription quota.
//!   Read from `GET https://api.kimi.com/coding/v1/usages` with an API key, or
//!   from the web billing service (`GetUsages`) with a `kimi-auth` token. The
//!   `GetSubscriptionStats` membership call only **enriches** the Code quota: it
//!   adds the shared "Total usage" pool and a distinct Code 7-day lane, and a
//!   failure there never discards the Code usage.
//! * **3B Moonshot / Kimi Open Platform** — a **separate** prepaid-balance
//!   surface (`GET {base}/v1/users/me/balance`, `MOONSHOT_API_KEY`, region-bound).
//!   The frozen contract has no Moonshot provider id, so this module documents
//!   the surface but prioritises Kimi Code. `MOONSHOT_API_KEY` is accepted only
//!   as a last-resort alias for the Code credential; a rejection is surfaced
//!   honestly rather than silently ignored.
//!
//! | Behaviour | Where |
//! | --- | --- |
//! | Credential precedence (token account → `providers[].apiKey` → env) | [`PortConfig::resolve_api_key`] |
//! | `KIMI_CODE_API_KEY` > `MOONSHOT_API_KEY`; `KIMI_AUTH_TOKEN` is the web bearer | [`ENV_ALIASES`] / [`ENV_AUTH_TOKEN`] |
//! | `KIMI_CODE_BASE_URL` must be HTTPS, fail closed | [`secure_base_url`] |
//! | Code / web quota → weekly + 5 h windows | [`Kimi::fetch`] |
//! | Membership stats → "Total usage" + "Code 7-day", soft-degrading | [`Kimi::fetch_subscription_stats`] |
//!
//! Not implemented here (documented, not guessed): the read-only Kimi Code CLI
//! credential file (`~/.kimi-code/credentials/kimi-code.json`) and browser-cookie
//! import. The port is read-only and those sources are owner-managed; the module
//! never writes `device_id` or any credential file.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use codexbar_core::{
    DataSource, FetchStatus, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    WindowKind,
};

use crate::credential::{home_dir, Env, PortConfig, Secret};
use crate::http::{secure_base_url, HttpClient, HttpRequest};

/// Primary credential env var (Kimi Code API key).
pub const ENV_API_KEY: &str = "KIMI_CODE_API_KEY";
/// Separate Moonshot Open Platform key — see the module note above.
pub const ENV_MOONSHOT_API_KEY: &str = "MOONSHOT_API_KEY";
/// Web session token (`kimi-auth` cookie value) for the billing endpoints.
pub const ENV_AUTH_TOKEN: &str = "KIMI_AUTH_TOKEN";
/// Base URL override — HTTPS only, and only with an explicit API key.
pub const ENV_BASE_URL: &str = "KIMI_CODE_BASE_URL";
/// Alternate Kimi Code home for the optional device-id header.
pub const ENV_HOME: &str = "KIMI_CODE_HOME";

/// Default Code API base.
pub const DEFAULT_BASE_URL: &str = "https://api.kimi.com";
/// The web billing service lives on a fixed host.
pub const WEB_BASE_URL: &str = "https://www.kimi.com";

const CODE_USAGES_PATH: &str = "/coding/v1/usages";
const WEB_USAGES_PATH: &str = "/apiv2/kimi.gateway.billing.v1.BillingService/GetUsages";
const WEB_STATS_PATH: &str =
    "/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscriptionStats";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Membership enrichment shares a short deadline so it cannot hold up the tick.
const ENRICH_TIMEOUT: Duration = Duration::from_secs(3);

const WEEKLY_MINUTES: i64 = 10_080;
const MONTHLY_MINUTES: i64 = 43_200;
const SESSION_MINUTES: i64 = 300;

const ENV_ALIASES: [&str; 2] = [ENV_API_KEY, ENV_MOONSHOT_API_KEY];

pub struct Kimi {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl Kimi {
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
            Some(config) => config.resolve_api_key(ProviderId::Kimi, &self.env, &ENV_ALIASES),
            None => self.env.first_of(&ENV_ALIASES),
        }
    }

    fn auth_token(&self) -> Option<Secret> {
        self.env.get(ENV_AUTH_TOKEN)
    }

    fn base_url(&self) -> Result<String, String> {
        secure_base_url(self.env.get_str(ENV_BASE_URL).as_deref(), DEFAULT_BASE_URL)
    }

    /// Best-effort device id for the CLI identity header. **Read-only**: unlike
    /// the macOS app, the port never creates the file.
    fn device_id(&self) -> Option<String> {
        let home = match self.env.get_str(ENV_HOME) {
            Some(path) => PathBuf::from(path),
            None => home_dir()?.join(".kimi-code"),
        };
        let raw = std::fs::read_to_string(home.join("device_id")).ok()?;
        let ascii: String = raw
            .trim()
            .chars()
            .filter(|c| c.is_ascii() && !c.is_control())
            .collect();
        if ascii.is_empty() {
            None
        } else {
            Some(ascii)
        }
    }

    /// Device identity headers the official CLI sends (`SPEC-apikey.md` §3A).
    fn identity(&self, request: HttpRequest) -> HttpRequest {
        let mut request = request
            .header("X-Msh-Platform", "kimi_code_cli")
            .header("X-Msh-Version", env!("CARGO_PKG_VERSION"));
        if let Some(device) = self.device_id() {
            request = request.header("X-Msh-Device-Id", device);
        }
        request
    }

    fn fetch_code_usages(&self, base: &str, key: &Secret) -> Result<CodingUsage, String> {
        let request = self
            .identity(HttpRequest::get(format!("{base}{CODE_USAGES_PATH}")))
            .bearer(key)
            .accept_json()
            .timeout(REQUEST_TIMEOUT);

        let response = self.client.execute(&request).map_err(|err| err.message)?;
        if response.status != 200 {
            return Err(code_api_error(response.status));
        }
        let body: serde_json::Value = response.json().map_err(|err| err.message)?;
        let detail = body
            .get("usage")
            .ok_or_else(|| "Kimi /coding/v1/usages response had no usage object".to_string())?;
        let limits = body.get("limits").and_then(|v| v.as_array());
        coding_usage(detail, limits, plan_name(&body))
            .ok_or_else(|| "Kimi /coding/v1/usages response had no usable quota".to_string())
    }

    fn fetch_web_usages(&self, token: &Secret) -> Result<CodingUsage, String> {
        let request = self
            .web_request(
                WEB_USAGES_PATH,
                token,
                &serde_json::json!({ "scope": ["FEATURE_CODING"] }),
            )
            .timeout(REQUEST_TIMEOUT);

        let response = self.client.execute(&request).map_err(|err| err.message)?;
        if response.status != 200 {
            return Err(web_api_error(response.status));
        }
        let body: serde_json::Value = response.json().map_err(|err| err.message)?;
        let usages = body
            .get("usages")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "Kimi GetUsages response had no usages array".to_string())?;
        let coding = usages
            .iter()
            .find(|entry| entry.get("scope").and_then(|v| v.as_str()) == Some("FEATURE_CODING"))
            .ok_or_else(|| "Kimi GetUsages response had no FEATURE_CODING scope".to_string())?;
        let detail = coding
            .get("detail")
            .ok_or_else(|| "Kimi GetUsages response had no detail object".to_string())?;
        let limits = coding.get("limits").and_then(|v| v.as_array());
        coding_usage(detail, limits, None)
            .ok_or_else(|| "Kimi GetUsages response had no usable quota".to_string())
    }

    /// Membership stats. `Ok(None)` when the endpoint answers non-200; an error
    /// is a soft-degradable string for the caller's diagnostic note.
    fn fetch_subscription_stats(&self, token: &Secret) -> Result<Option<Stats>, String> {
        let request = self
            .web_request(WEB_STATS_PATH, token, &serde_json::json!({}))
            .timeout(ENRICH_TIMEOUT);

        let response = self
            .client
            .execute(&request)
            .map_err(|err| err.kind.as_str().to_string())?;
        if response.status != 200 {
            return Err(format!("HTTP {}", response.status));
        }
        let body: serde_json::Value = response.json().map_err(|err| err.message)?;
        Ok(Some(parse_stats(&body)))
    }

    fn web_request(&self, path: &str, token: &Secret, body: &serde_json::Value) -> HttpRequest {
        let mut request = HttpRequest::post(format!("{WEB_BASE_URL}{path}"))
            .bearer(token)
            .header("Cookie", format!("kimi-auth={}", token.expose()))
            .header("Origin", WEB_BASE_URL)
            .header("Referer", format!("{WEB_BASE_URL}/code/console"))
            .header("Accept", "*/*")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("x-msh-platform", "web");
        if let Ok(encoded) = serde_json::to_vec(body) {
            request.body = Some(encoded);
            request = request.header("Content-Type", "application/json");
        }
        request
    }
}

impl Default for Kimi {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Kimi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kimi")
            .field("has_credentials", &self.api_key().is_some())
            .field("has_web_token", &self.auth_token().is_some())
            .field("config", &self.config.as_ref().map(PortConfig::source_path))
            .finish()
    }
}

/// Extracted counters for one quota detail. `reliable == false` means a positive
/// limit existed but neither `used` nor a valid `remaining` did, so display code
/// must not derive pace from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counts {
    used: i64,
    limit: i64,
    reliable: bool,
}

#[derive(Debug, Clone)]
struct CodingUsage {
    weekly: Counts,
    weekly_reset: Option<DateTime<Utc>>,
    rate: Option<(Counts, Option<DateTime<Utc>>, Option<i64>)>,
    plan: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    monthly: Option<(f64, Option<DateTime<Utc>>)>,
    code7d: Option<(f64, Option<DateTime<Utc>>)>,
}

impl Provider for Kimi {
    fn id(&self) -> ProviderId {
        ProviderId::Kimi
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::Kimi,
            title: ProviderId::Kimi.title().to_string(),
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
        let web_token = self.auth_token();
        if api_key.is_none() && web_token.is_none() {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(format!(
                "No Kimi credentials found — set {ENV_API_KEY} (Kimi Code API key) or \
                 {ENV_AUTH_TOKEN} (web kimi-auth token), or add providers[].apiKey for \
                 \"kimi\" to %APPDATA%\\CodexBar\\config.json."
            ));
            return snapshot;
        }

        let base = match self.base_url() {
            Ok(base) => base,
            Err(reason) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!("{ENV_BASE_URL} is not usable: {reason}"));
                return snapshot;
            }
        };

        let coding = if let Some(key) = &api_key {
            snapshot.account = Some(key.redacted());
            match self.fetch_code_usages(&base, key) {
                Ok(usage) => usage,
                Err(message) => {
                    snapshot.status = FetchStatus::Error;
                    snapshot.error = Some(message);
                    return snapshot;
                }
            }
        } else if let Some(token) = &web_token {
            snapshot.account = Some(token.redacted());
            match self.fetch_web_usages(token) {
                Ok(usage) => usage,
                Err(message) => {
                    snapshot.status = FetchStatus::Error;
                    snapshot.error = Some(message);
                    return snapshot;
                }
            }
        } else {
            // Unreachable: the guard above proved one credential exists.
            return snapshot;
        };

        let weekly_percent = percent(coding.weekly.used, coding.weekly.limit);
        let mut weekly = RateWindow::new(
            weekly_percent,
            coding.weekly.reliable.then_some(WEEKLY_MINUTES),
            coding.weekly_reset,
        );
        weekly.reset_description = Some(format!(
            "{}/{} requests",
            coding.weekly.used, coding.weekly.limit
        ));
        snapshot.windows.push(NamedRateWindow::new(
            "kimi-weekly",
            "Weekly",
            WindowKind::Weekly,
            weekly,
        ));

        if let Some((counts, reset, minutes)) = coding.rate {
            let window_minutes = if counts.reliable {
                minutes.or(Some(SESSION_MINUTES))
            } else {
                None
            };
            let mut window =
                RateWindow::new(percent(counts.used, counts.limit), window_minutes, reset);
            window.reset_description =
                Some(rate_description(counts.used, counts.limit, window_minutes));
            snapshot.windows.push(NamedRateWindow::new(
                "kimi-rate",
                "Rate limit",
                WindowKind::Session,
                window,
            ));
        }

        snapshot.plan = coding.plan;

        // Membership enrichment only when a web session exists.
        if let Some(token) = &web_token {
            match self.fetch_subscription_stats(token) {
                Ok(Some(stats)) => {
                    if let Some((used, reset)) = stats.monthly {
                        snapshot.windows.push(NamedRateWindow::new(
                            "kimi-monthly",
                            "Total usage",
                            WindowKind::Extra,
                            RateWindow::new(used, Some(MONTHLY_MINUTES), reset),
                        ));
                    }
                    if let Some((used, reset)) = stats.code7d {
                        let equivalent = is_equivalent_to_weekly(
                            used,
                            reset,
                            weekly_percent,
                            coding.weekly_reset,
                            coding.weekly.reliable,
                        );
                        if !equivalent {
                            snapshot.windows.push(NamedRateWindow::new(
                                "kimi-code-7d",
                                "Code 7-day",
                                WindowKind::Weekly,
                                RateWindow::new(used, Some(WEEKLY_MINUTES), reset),
                            ));
                        }
                    }
                }
                Ok(None) => {}
                Err(message) => {
                    snapshot.error = Some(format!(
                        "Kimi subscription stats unavailable right now ({message})"
                    ));
                }
            }
        }

        snapshot
    }
}

fn code_api_error(status: u16) -> String {
    match status {
        400 => "Kimi Code API rejected the request (HTTP 400).".to_string(),
        401 => "Kimi Code API key is invalid or expired. Create a new key in the Kimi Code \
                Console or set KIMI_CODE_API_KEY."
            .to_string(),
        403 => "Kimi Code API returned HTTP 403 (permission or quota denied).".to_string(),
        other => format!("Kimi Code API returned HTTP {other}."),
    }
}

fn web_api_error(status: u16) -> String {
    match status {
        400 => "Kimi GetUsages request was rejected (HTTP 400).".to_string(),
        401 | 403 => "Kimi auth token is invalid or expired. Refresh the kimi-auth token or set \
                      KIMI_AUTH_TOKEN."
            .to_string(),
        other => format!("Kimi web billing API returned HTTP {other}."),
    }
}

fn coding_usage(
    detail: &serde_json::Value,
    limits: Option<&Vec<serde_json::Value>>,
    plan: Option<String>,
) -> Option<CodingUsage> {
    let weekly = usage_counts(detail)?;
    let weekly_reset = reset_at(detail);
    let rate = limits.and_then(|list| list.first()).and_then(|entry| {
        let detail = entry.get("detail")?;
        let counts = usage_counts(detail)?;
        let minutes = entry.get("window").and_then(window_minutes);
        Some((counts, reset_at(detail), minutes))
    });
    Some(CodingUsage {
        weekly,
        weekly_reset,
        rate,
        plan,
    })
}

/// `used` is authoritative and may exceed the limit during overage; `remaining`
/// must describe a valid balance; otherwise a valid limit still publishes a 0 %
/// lane but withholds duration so invalid counters cannot create pace.
fn usage_counts(detail: &serde_json::Value) -> Option<Counts> {
    let limit = int_field(detail, "limit")?;
    if limit <= 0 {
        return None;
    }
    if let Some(used) = int_field(detail, "used") {
        if used >= 0 {
            return Some(Counts {
                used,
                limit,
                reliable: true,
            });
        }
    }
    if let Some(remaining) = int_field(detail, "remaining") {
        if (0..=limit).contains(&remaining) {
            return Some(Counts {
                used: limit - remaining,
                limit,
                reliable: true,
            });
        }
    }
    Some(Counts {
        used: 0,
        limit,
        reliable: false,
    })
}

/// Accept a JSON integer, a numeric string, or a whole float — the API mixes all
/// three across versions.
fn int_field(object: &serde_json::Value, key: &str) -> Option<i64> {
    let value = object.get(key)?;
    if let Some(text) = value.as_str() {
        return text.trim().parse().ok();
    }
    if let Some(integer) = value.as_i64() {
        return Some(integer);
    }
    let float = value.as_f64()?;
    if float.is_finite() && float.fract() == 0.0 {
        Some(float as i64)
    } else {
        None
    }
}

fn reset_at(detail: &serde_json::Value) -> Option<DateTime<Utc>> {
    ["resetTime", "resetAt", "reset_time", "reset_at"]
        .iter()
        .find_map(|key| detail.get(key).and_then(|value| value.as_str()))
        .and_then(parse_time)
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn window_minutes(window: &serde_json::Value) -> Option<i64> {
    let duration = window.get("duration").and_then(serde_json::Value::as_i64)?;
    if duration <= 0 {
        return None;
    }
    let multiplier = match window.get("timeUnit").and_then(|value| value.as_str())? {
        "TIME_UNIT_MINUTE" => 1,
        "TIME_UNIT_HOUR" => 60,
        "TIME_UNIT_DAY" => 24 * 60,
        _ => return None,
    };
    duration.checked_mul(multiplier)
}

fn plan_name(body: &serde_json::Value) -> Option<String> {
    let level = body
        .get("user")?
        .get("membership")?
        .get("level")?
        .as_str()?
        .trim();
    if level.is_empty() || level == "LEVEL_UNSPECIFIED" {
        return None;
    }
    let version = body.get("version").and_then(|value| value.as_str());
    // Unknown schema versions keep the raw enum; V1 maps to the goods catalog.
    if version.is_some() && version != Some("GOODS_VERSION_V1") {
        return Some(level.to_string());
    }
    Some(
        match level {
            "LEVEL_FREE" => "Adagio",
            "LEVEL_TRIAL" => "Andante",
            "LEVEL_BASIC" => "Moderato",
            "LEVEL_INTERMEDIATE" => "Allegretto",
            "LEVEL_ADVANCED" => "Allegro",
            other => other,
        }
        .to_string(),
    )
}

fn parse_stats(body: &serde_json::Value) -> Stats {
    let monthly = body.get("subscriptionBalance").and_then(|balance| {
        let feature_ok = balance
            .get("feature")
            .and_then(|value| value.as_str())
            .map_or(true, |feature| feature == "FEATURE_OMNI");
        let type_ok = balance
            .get("type")
            .and_then(|value| value.as_str())
            .map_or(true, |kind| kind == "SUBSCRIPTION");
        if !feature_ok || !type_ok {
            return None;
        }
        // The shared pool is `amountUsedRatio`; `kimiCodeUsedRatio` is Code-only.
        let ratio = ratio_value(balance, "amountUsedRatio")?;
        Some((ratio * 100.0, balance_time(balance, "expireTime")))
    });

    let code7d = body.get("ratelimitCode7d").and_then(|limit| {
        if limit.get("enabled").and_then(|value| value.as_bool()) == Some(false) {
            return None;
        }
        let ratio = ratio_value(limit, "ratio")?;
        Some((ratio * 100.0, balance_time(limit, "resetTime")))
    });

    Stats { monthly, code7d }
}

fn ratio_value(object: &serde_json::Value, key: &str) -> Option<f64> {
    object
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .filter(|ratio| ratio.is_finite())
        .map(|ratio| ratio.clamp(0.0, 1.0))
}

fn balance_time(object: &serde_json::Value, key: &str) -> Option<DateTime<Utc>> {
    object
        .get(key)
        .and_then(|value| value.as_str())
        .and_then(parse_time)
}

/// The membership 7-day ratio and the FEATURE_CODING weekly detail report the
/// same quota through two endpoints; suppress the duplicate lane only on positive
/// evidence that they agree.
fn is_equivalent_to_weekly(
    code_percent: f64,
    code_reset: Option<DateTime<Utc>>,
    weekly_percent: f64,
    weekly_reset: Option<DateTime<Utc>>,
    weekly_has_minutes: bool,
) -> bool {
    if !weekly_has_minutes {
        return false;
    }
    if (code_percent - weekly_percent).abs() > 1.0 {
        return false;
    }
    match (code_reset, weekly_reset) {
        (Some(code), Some(weekly)) => (code - weekly).num_seconds().abs() <= 300,
        _ => false,
    }
}

fn percent(used: i64, limit: i64) -> f64 {
    if limit <= 0 {
        return 0.0;
    }
    ((used as f64 / limit as f64) * 100.0).clamp(0.0, 100.0)
}

fn rate_description(used: i64, limit: i64, window_minutes: Option<i64>) -> String {
    match window_minutes {
        Some(minutes) if minutes % 60 == 0 => {
            let hours = minutes / 60;
            format!(
                "Rate: {used}/{limit} per {hours} {}",
                if hours == 1 { "hour" } else { "hours" }
            )
        }
        Some(minutes) => format!(
            "Rate: {used}/{limit} per {minutes} {}",
            if minutes == 1 { "minute" } else { "minutes" }
        ),
        None => format!("Rate: {used}/{limit}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_prefer_used_then_remaining_then_zero() {
        let body = serde_json::json!({ "limit": "2048", "used": "214", "remaining": "1834" });
        assert_eq!(
            usage_counts(&body),
            Some(Counts {
                used: 214,
                limit: 2048,
                reliable: true
            })
        );
        let body = serde_json::json!({ "limit": "200", "remaining": "61" });
        assert_eq!(
            usage_counts(&body),
            Some(Counts {
                used: 139,
                limit: 200,
                reliable: true
            })
        );
        // No usable counter at all: publish a 0 % lane but mark it unreliable.
        let body = serde_json::json!({ "limit": "200" });
        assert_eq!(
            usage_counts(&body),
            Some(Counts {
                used: 0,
                limit: 200,
                reliable: false
            })
        );
        assert_eq!(usage_counts(&serde_json::json!({ "limit": "0" })), None);
        assert_eq!(usage_counts(&serde_json::json!({})), None);
    }

    #[test]
    fn window_minutes_map_the_time_units() {
        assert_eq!(
            window_minutes(&serde_json::json!({"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"})),
            Some(300)
        );
        assert_eq!(
            window_minutes(&serde_json::json!({"duration": 5, "timeUnit": "TIME_UNIT_HOUR"})),
            Some(300)
        );
        assert_eq!(
            window_minutes(&serde_json::json!({"duration": 1, "timeUnit": "TIME_UNIT_DAY"})),
            Some(1440)
        );
        assert_eq!(
            window_minutes(&serde_json::json!({"duration": 5, "timeUnit": "TIME_UNIT_WEEK"})),
            None
        );
    }

    #[test]
    fn plan_names_follow_the_goods_catalog_and_versions() {
        let body = serde_json::json!({
            "user": {"membership": {"level": "LEVEL_BASIC"}},
            "version": "GOODS_VERSION_V1"
        });
        assert_eq!(plan_name(&body).as_deref(), Some("Moderato"));
        let body = serde_json::json!({"user": {"membership": {"level": "LEVEL_MYSTERY"}}});
        assert_eq!(plan_name(&body).as_deref(), Some("LEVEL_MYSTERY"));
        let body = serde_json::json!({"user": {"membership": {"level": "LEVEL_UNSPECIFIED"}}});
        assert_eq!(plan_name(&body), None);
    }

    #[test]
    fn stats_use_the_shared_pool_ratio_and_respect_feature_type() {
        let body = serde_json::json!({
            "subscriptionBalance": {"feature": "FEATURE_OMNI", "type": "SUBSCRIPTION", "amountUsedRatio": 0.42},
            "ratelimitCode7d": {"ratio": 0.3, "enabled": true}
        });
        let stats = parse_stats(&body);
        assert_eq!(stats.monthly.map(|(p, _)| p), Some(42.0));
        assert_eq!(stats.code7d.map(|(p, _)| p), Some(30.0));

        let body = serde_json::json!({
            "subscriptionBalance": {"feature": "FEATURE_OTHER", "amountUsedRatio": 0.42},
            "ratelimitCode7d": {"ratio": 0.3, "enabled": false}
        });
        let stats = parse_stats(&body);
        assert_eq!(stats.monthly, None);
        assert_eq!(stats.code7d, None);
    }

    #[test]
    fn rate_descriptions_are_human_readable() {
        assert_eq!(
            rate_description(139, 200, Some(300)),
            "Rate: 139/200 per 5 hours"
        );
        assert_eq!(rate_description(1, 2, Some(60)), "Rate: 1/2 per 1 hour");
        assert_eq!(rate_description(1, 2, None), "Rate: 1/2");
    }
}
