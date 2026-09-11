//! **xAI** — developer-platform prepaid balance and daily USD spend.
//!
//! This is the **Management API** surface (`SPEC-apikey.md` §10), deliberately
//! separate from the consumer Grok subscription (out of scope here). Only a
//! *Management API key* works; inference keys are rejected upstream.
//!
//! | Behaviour | Where |
//! | --- | --- |
//! | `XAI_MANAGEMENT_API_KEY` + `XAI_TEAM_ID` (or config `apiKey` + `workspaceID`) | [`Xai::api_key`] / [`Xai::team_id`] |
//! | Team id must not contain `/` and must not be `.` or `..` | [`valid_team_id`] |
//! | `GET {root}/prepaid/balance` — **inverted** ledger in string USD cents | [`parse_balance`] |
//! | `POST {root}/usage` — best-effort 30-day daily spend enrichment | [`Xai::spend_history`] |
//!
//! The balance endpoint reports an inverted ledger in string cents: a $10 top-up
//! arrives as `"-1000"`, so the remaining balance is the **negated** cent value. A
//! response without a parseable total is an error, never a `$0.00` balance.
//!
//! Prepaid money is remaining credit, not a quota, so no session/weekly meters
//! are synthesised. The `/usage` history is validated and surfaced only as a
//! diagnostic note when xAI caps its analytics cardinality (`limitReached`); it
//! never turns money into a percentage.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use codexbar_core::{
    DataSource, FetchStatus, MoneyBalance, Provider, ProviderId, ProviderSnapshot,
};

use crate::credential::{Env, PortConfig, Secret};
use crate::http::{HttpClient, HttpRequest};

/// Management API key env var.
pub const ENV_API_KEY: &str = "XAI_MANAGEMENT_API_KEY";
/// Team id env var.
pub const ENV_TEAM_ID: &str = "XAI_TEAM_ID";

/// Fixed Management API root — not user-overridable (`docs/xai.md`).
pub const MANAGEMENT_API_BASE: &str = "https://management-api.x.ai/v1";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// History is enrichment: a slow analytics query must not hold up the balance.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(8);
const HISTORY_DAYS: i64 = 30;

const ENV_ALIASES: [&str; 1] = [ENV_API_KEY];

pub struct Xai {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl Xai {
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
            Some(config) => config.resolve_api_key(ProviderId::Xai, &self.env, &ENV_ALIASES),
            None => self.env.first_of(&ENV_ALIASES),
        }
    }

    fn team_id(&self) -> Option<String> {
        match &self.config {
            Some(config) => config.field(ProviderId::Xai, "workspaceID"),
            None => None,
        }
        .or_else(|| self.env.get_str(ENV_TEAM_ID))
    }
}

impl Default for Xai {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Xai {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Xai")
            .field("has_credentials", &self.api_key().is_some())
            .field("config", &self.config.as_ref().map(PortConfig::source_path))
            .finish()
    }
}

impl Provider for Xai {
    fn id(&self) -> ProviderId {
        ProviderId::Xai
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::Xai,
            title: ProviderId::Xai.title().to_string(),
            account: None,
            plan: Some("Management API".to_string()),
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::ApiKey,
            fetched_at: now,
        };

        let Some(api_key) = self.api_key() else {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(format!(
                "No xAI Management API key found — set {ENV_API_KEY} or add \
                 providers[].apiKey for \"xai\" to %APPDATA%\\CodexBar\\config.json. \
                 Inference API keys are not accepted."
            ));
            return snapshot;
        };

        let Some(team_id) = self.team_id() else {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(format!(
                "Missing xAI team ID — set {ENV_TEAM_ID} or providers[].workspaceID \
                 for \"xai\" (shown in the xAI Console URL and team settings)."
            ));
            return snapshot;
        };

        if !valid_team_id(&team_id) {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(
                "The xAI team ID must be a single identifier without path separators.".to_string(),
            );
            return snapshot;
        }

        let root = format!("{MANAGEMENT_API_BASE}/billing/teams/{}/prepaid", team_id);

        let balance_request = HttpRequest::get(format!("{root}/balance"))
            .bearer(&api_key)
            .accept_json()
            .timeout(REQUEST_TIMEOUT);

        let response = match self.client.execute(&balance_request) {
            Ok(response) => response,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(err.message);
                return snapshot;
            }
        };

        match response.status {
            200 => {}
            401 | 403 => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(
                    "xAI rejected the Management API key. Create one in the xAI Console under \
                     Settings > Management Keys; inference API keys are not accepted."
                        .to_string(),
                );
                return snapshot;
            }
            404 => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(
                    "xAI returned 404 for this team. Check the team ID, and that the Management \
                     key belongs to the same team."
                        .to_string(),
                );
                return snapshot;
            }
            429 => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(
                    "xAI Management API rate limit exceeded. Usage will refresh on the next \
                     cycle."
                        .to_string(),
                );
                return snapshot;
            }
            other => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!("xAI Management API returned HTTP {other}."));
                return snapshot;
            }
        }

        let balance_body: serde_json::Value = match response.json() {
            Ok(body) => body,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(err.message);
                return snapshot;
            }
        };

        let Some(balance) = parse_balance(&balance_body) else {
            snapshot.status = FetchStatus::Error;
            snapshot.error = Some(
                "Could not parse xAI billing data: balance total.val is not a cent amount."
                    .to_string(),
            );
            return snapshot;
        };

        snapshot.balance = Some(MoneyBalance {
            amount: round2(balance),
            currency: "USD".to_string(),
            label: Some("Prepaid credits".to_string()),
        });

        // Best-effort: a history failure keeps the balance and adds a note.
        let mut notes: Vec<String> = Vec::new();
        match self.spend_history(&root, &api_key, now) {
            Ok(Some(partial)) => {
                if partial {
                    notes.push(
                        "xAI 30-day spend history is partial (analytics cardinality cap reached)"
                            .to_string(),
                    );
                }
            }
            Ok(None) => {}
            Err(err) => {
                if err.status == Some(401) || err.status == Some(403) {
                    snapshot.status = FetchStatus::Error;
                    snapshot.balance = None;
                    snapshot.error = Some(
                        "xAI rejected the Management API key. Create one in the xAI Console under \
                         Settings > Management Keys; inference API keys are not accepted."
                            .to_string(),
                    );
                    return snapshot;
                }
                notes.push(format!(
                    "xAI 30-day spend history unavailable right now ({})",
                    err.kind.as_str()
                ));
            }
        }

        if !notes.is_empty() {
            snapshot.error = Some(notes.join("; "));
        }

        snapshot
    }
}

impl Xai {
    /// `POST {root}/usage` — a UTC daily, USD-summed analytics query.
    ///
    /// `Ok(Some(true))` means history was returned but flagged `limitReached`.
    /// `Ok(None)` means history was unavailable for a non-auth reason.
    fn spend_history(
        &self,
        root: &str,
        api_key: &Secret,
        now: DateTime<Utc>,
    ) -> Result<Option<bool>, crate::http::HttpError> {
        let body = analytics_body(now);
        let request = HttpRequest::post(format!("{root}/usage"))
            .bearer(api_key)
            .accept_json()
            .timeout(HISTORY_TIMEOUT)
            .json_body(&body)?;

        let response = self.client.execute(&request)?;
        if !response.is_success() {
            return Err(crate::http::HttpError::status(&request, &response));
        }
        let body: serde_json::Value = response.json()?;
        match parse_history(&body) {
            Some(partial) => Ok(Some(partial)),
            None => Ok(None),
        }
    }
}

fn valid_team_id(team_id: &str) -> bool {
    !team_id.is_empty()
        && !team_id.contains('/')
        && team_id != "."
        && team_id != ".."
        && !team_id
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '?' || c == '#')
}

/// The Management API balance payload: `total.val` is a string cent amount.
fn parse_balance(body: &serde_json::Value) -> Option<f64> {
    let raw = body.get("total")?.get("val")?;
    let text = match raw {
        serde_json::Value::String(value) => value.trim().to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        _ => return None,
    };
    if !is_cent_amount(&text) {
        return None;
    }
    let cents: f64 = text.parse().ok()?;
    if !cents.is_finite() {
        return None;
    }
    // The ledger is inverted on the wire; the balance is the negated cent value.
    Some(-cents / 100.0)
}

fn is_cent_amount(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    if digits.is_empty() {
        return false;
    }
    let mut parts = digits.splitn(2, '.');
    let integer = parts.next().unwrap_or("");
    if integer.is_empty() || !integer.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if let Some(fraction) = parts.next() {
        if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    true
}

/// `analyticsRequest` for the last 30 UTC days, daily SUM of `usd`.
fn analytics_body(now: DateTime<Utc>) -> serde_json::Value {
    let start = (now - chrono::Duration::days(HISTORY_DAYS - 1))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| Utc.from_utc_datetime(&naive))
        .unwrap_or(now);
    serde_json::json!({
        "analyticsRequest": {
            "timeRange": {
                "startTime": timestamp(start),
                "endTime": timestamp(now),
                "timezone": "Etc/GMT"
            },
            "timeUnit": "TIME_UNIT_DAY",
            "values": [{ "name": "usd", "aggregation": "AGGREGATION_SUM" }],
            "groupBy": [],
            "filters": []
        }
    })
}

fn timestamp(instant: DateTime<Utc>) -> String {
    instant.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// `Some(limitReached)` when the time series is well-formed, `None` otherwise.
fn parse_history(body: &serde_json::Value) -> Option<bool> {
    let series = body.get("timeSeries")?.as_array()?;
    for entry in series {
        let points = entry.get("dataPoints")?.as_array()?;
        for point in points {
            let value = point
                .get("values")
                .and_then(|values| values.as_array())
                .and_then(|values| values.first())
                .and_then(serde_json::Value::as_f64)?;
            if !value.is_finite() || value < 0.0 {
                return None;
            }
        }
    }
    Some(
        body.get("limitReached")
            .and_then(serde_json::Value::as_bool)
            == Some(true),
    )
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_is_the_negated_cent_value() {
        let body = serde_json::json!({ "total": { "val": "-1000" } });
        assert_eq!(parse_balance(&body), Some(10.0));
        let body = serde_json::json!({ "total": { "val": "-1234.56" } });
        assert_eq!(parse_balance(&body), Some(12.3456));
        // A credit (positive ledger) is negative spend headroom, never clamped here.
        let body = serde_json::json!({ "total": { "val": "250" } });
        assert_eq!(parse_balance(&body), Some(-2.5));
    }

    #[test]
    fn an_unparseable_total_is_none_not_zero() {
        assert_eq!(parse_balance(&serde_json::json!({})), None);
        assert_eq!(parse_balance(&serde_json::json!({ "total": {} })), None);
        assert_eq!(
            parse_balance(&serde_json::json!({ "total": { "val": "not-a-number" } })),
            None
        );
        assert_eq!(
            parse_balance(&serde_json::json!({ "total": { "val": "" } })),
            None
        );
        assert_eq!(
            parse_balance(&serde_json::json!({ "total": { "val": "-1.2.3" } })),
            None
        );
    }

    #[test]
    fn team_ids_reject_path_separators_and_dot_segments() {
        assert!(valid_team_id("team_abc123"));
        assert!(!valid_team_id(""));
        assert!(!valid_team_id("a/b"));
        assert!(!valid_team_id("."));
        assert!(!valid_team_id(".."));
        assert!(!valid_team_id("a b"));
    }

    #[test]
    fn history_requires_well_formed_series() {
        let good = serde_json::json!({
            "timeSeries": [{ "dataPoints": [{ "timestamp": "2026-09-09T00:00:00Z", "values": [1.5] }] }],
            "limitReached": true
        });
        assert_eq!(parse_history(&good), Some(true));
        let bad = serde_json::json!({ "timeSeries": "nope" });
        assert_eq!(parse_history(&bad), None);
        let negative = serde_json::json!({
            "timeSeries": [{ "dataPoints": [{ "values": [-1.0] }] }]
        });
        assert_eq!(parse_history(&negative), None);
    }
}
