//! **z.ai / GLM** — Coding Plan quota, model usage and the BigModel CN account
//! balance, from `SPEC-apikey.md` §5.
//!
//! Same shape as [`crate::providers::openrouter`]: a constructor pair, a
//! no-panic `fetch`, an HTTPS endpoint policy that fails closed *before* the
//! bearer is attached, and fixture tests that never touch the network.
//!
//! | Spec | Where |
//! | --- | --- |
//! | Region (Global `api.z.ai` / BigModel CN `open.bigmodel.cn`) + inference from an override | [`Zai::region`] |
//! | Token precedence: config → `Z_AI_API_KEY` → CN-only aliases → CN-only relay files | [`Zai::api_key`] |
//! | `Z_AI_QUOTA_URL` / `Z_AI_QUOTA_ENDPOINT` / `Z_AI_API_HOST` (both alias sets) | [`Zai::quota_url`] |
//! | `Z_AI_MODEL_USAGE_ENDPOINT` (`&type=3` for team) | [`Zai::model_usage_url`] |
//! | `Z_AI_BALANCE_URL` / `Z_AI_BALANCE_ENDPOINT` (CN-only, best-effort, 5 s) | [`Zai::balance_url`], [`Zai::balance`] |
//! | Region-mismatch guard on the canonical hosts (`endpointRegionMismatch`) | [`check_region`] |
//! | `success == true && code == 200`, `data.limits[]`, `TOKENS_LIMIT`/`CREDIT_LIMIT`/`TIME_LIMIT` | [`ZaiQuota::from_json`] |
//! | `unit`+`number` → minutes (`{1:1440, 3:60, 5:1, 6:10080}`), `percentage`, count-based recalc | [`ZaiLimit::from_json`] |
//! | 5-hour reset plausibility (never guess a timezone correction) | [`reset_is_plausible`] |
//! | `data.planName` / `plan` / `plan_type` / `packageName` / `level` | [`ZaiQuota::from_json`] |
//!
//! **Model usage (the 24 h / 30 d token charts):** the endpoints, the URL
//! resolution (both alias sets), the `type=3` team selector and the parser are
//! implemented and tested ([`Zai::model_usage_url`], [`ModelUsage`]), but
//! `fetch` does not call them. `schemaVersion 1` has no chart/detail slot — a
//! token total is not a quota — and the reference provider documents its own
//! 30-day chart the same way. They are here for the worker who adds that field.
//!
//! Nothing in this module ever prints a credential: the token goes out through
//! [`Secret`] + [`HttpRequest::bearer`], and every error message is built from
//! already-redacted HTTP errors.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use codexbar_core::{
    DataSource, FetchStatus, MoneyBalance, NamedRateWindow, Provider, ProviderId, ProviderSnapshot,
    RateWindow, WindowKind,
};
use serde_json::Value;

use crate::credential::{cleaned, home_dir, redact_secrets_in_text, Env, PortConfig, Secret};
use crate::http::{secure_base_url, HttpClient, HttpRequest};

/// Region-selected credential (`ZaiSettingsReader.apiTokenKey`).
pub const ENV_API_KEY: &str = "Z_AI_API_KEY";
/// BigModel CN-only credential aliases, in the Swift order.
pub const CN_ENV_ALIASES: [&str; 4] = [
    "BIGMODEL_API_KEY",
    "ZHIPU_API_KEY",
    "ZHIPUAI_API_KEY",
    "GLM_API_KEY",
];
/// Bare-host / base-URL override (Swift `Z_AI_API_HOST`).
pub const ENV_API_HOST: &str = "Z_AI_API_HOST";
/// Full quota-URL override (Swift `Z_AI_QUOTA_URL`).
pub const ENV_QUOTA_URL: &str = "Z_AI_QUOTA_URL";
/// Full balance-URL override (Swift `Z_AI_BALANCE_URL`).
pub const ENV_BALANCE_URL: &str = "Z_AI_BALANCE_URL";
/// Full quota-URL override (plugin JS `Z_AI_QUOTA_ENDPOINT`).
pub const ENV_QUOTA_ENDPOINT: &str = "Z_AI_QUOTA_ENDPOINT";
/// Full model-usage-URL override (plugin JS `Z_AI_MODEL_USAGE_ENDPOINT`).
pub const ENV_MODEL_USAGE_ENDPOINT: &str = "Z_AI_MODEL_USAGE_ENDPOINT";
/// Full balance-URL override (plugin JS `Z_AI_BALANCE_ENDPOINT`).
pub const ENV_BALANCE_ENDPOINT: &str = "Z_AI_BALANCE_ENDPOINT";
/// Explicit region (plugin JS `Z_AI_REGION`): `global` / `bigmodel-cn`.
pub const ENV_REGION: &str = "Z_AI_REGION";
/// `personal` (default) / `team` (plugin JS `Z_AI_USAGE_SCOPE`).
pub const ENV_USAGE_SCOPE: &str = "Z_AI_USAGE_SCOPE";
/// Team selector (plugin JS `Z_AI_ORGANIZATION`).
pub const ENV_ORGANIZATION: &str = "Z_AI_ORGANIZATION";
/// Team selector (plugin JS `Z_AI_PROJECT`).
pub const ENV_PROJECT: &str = "Z_AI_PROJECT";
/// Team selector (Swift `Z_AI_BIGMODEL_ORGANIZATION`).
pub const ENV_ORGANIZATION_SWIFT: &str = "Z_AI_BIGMODEL_ORGANIZATION";
/// Team selector (Swift `Z_AI_BIGMODEL_PROJECT`).
pub const ENV_PROJECT_SWIFT: &str = "Z_AI_BIGMODEL_PROJECT";

/// `GET {base}/api/monitor/usage/quota/limit`.
pub const QUOTA_PATH: &str = "api/monitor/usage/quota/limit";
/// `GET {base}/api/monitor/usage/model-usage`.
pub const MODEL_USAGE_PATH: &str = "api/monitor/usage/model-usage";
/// BigModel CN pay-as-you-go balance lives on the console host, not the API host.
pub const CN_BALANCE_URL_FIXED: &str =
    "https://www.bigmodel.cn/api/biz/account/query-customer-account-report";
/// Global (api.z.ai) base.
pub const GLOBAL_BASE_URL: &str = "https://api.z.ai";
/// BigModel CN (open.bigmodel.cn) base.
pub const CN_BASE_URL: &str = "https://open.bigmodel.cn";
/// Canonical global host — a CN selection may not override it and vice versa.
pub const GLOBAL_HOST: &str = "api.z.ai";
/// Canonical BigModel CN host.
pub const CN_HOST: &str = "open.bigmodel.cn";

/// BigModel CN relay files, tried in order (`ZaiSettingsReader`).
const CN_RELAY_FILES: [&str; 3] = [
    ".coding-relay/glm-api-key",
    ".config/bigmodel/api_key",
    ".config/zhipu/api_key",
];

const QUOTA_TIMEOUT: Duration = Duration::from_secs(15);
/// Balance is best-effort: a stalling console must not delay the quota display.
const BALANCE_TIMEOUT: Duration = Duration::from_secs(5);

/// Which z.ai region a fetch targets (`ZaiAPIRegion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZaiRegion {
    Global,
    BigModelCn,
}

impl ZaiRegion {
    pub const fn base(self) -> &'static str {
        match self {
            ZaiRegion::Global => GLOBAL_BASE_URL,
            ZaiRegion::BigModelCn => CN_BASE_URL,
        }
    }

    pub const fn host(self) -> &'static str {
        match self {
            ZaiRegion::Global => GLOBAL_HOST,
            ZaiRegion::BigModelCn => CN_HOST,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            ZaiRegion::Global => "global",
            ZaiRegion::BigModelCn => "bigmodel-cn",
        }
    }

    /// Tolerant region parse; `None` for anything the plugin would reject.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "global" => Some(ZaiRegion::Global),
            "bigmodel-cn" | "bigmodel_cn" | "bigmodelcn" | "cn" | "china" | "china-mainland" => {
                Some(ZaiRegion::BigModelCn)
            }
            _ => None,
        }
    }
}

/// `personal` (default) or `team` (`ZaiUsageScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZaiScope {
    Personal,
    Team,
}

impl ZaiScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            ZaiScope::Personal => "personal",
            ZaiScope::Team => "team",
        }
    }
}

pub struct Zai {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
    /// `Some` only in production: the CN relay files live under the user's home.
    /// Tests pass `None` so nothing is read from disk.
    home: Option<PathBuf>,
}

impl Zai {
    /// Production constructor: real HTTPS client, process environment, the port's
    /// own config file and the user's home directory.
    pub fn new() -> Self {
        let env = Env::from_process();
        let config = PortConfig::load(&env).unwrap_or(None);
        Self {
            client: crate::http::shared_client(),
            env,
            config,
            home: home_dir(),
        }
    }

    /// Constructor for tests and for embedding: no disk reads, no network.
    pub fn with_client(client: Arc<dyn HttpClient>, env: Env) -> Self {
        Self {
            client,
            env,
            config: None,
            home: None,
        }
    }

    pub fn with_config(mut self, config: Option<PortConfig>) -> Self {
        self.config = config;
        self
    }

    /// The selected region: an explicit config/`Z_AI_REGION` value wins, then the
    /// host of a quota/API override, then Global.
    pub fn region(&self) -> Result<ZaiRegion, String> {
        let explicit = self
            .config
            .as_ref()
            .and_then(|c| c.field(ProviderId::Zai, "region"))
            .or_else(|| self.env.get_str(ENV_REGION));
        if let Some(raw) = explicit {
            return ZaiRegion::parse(&raw).ok_or_else(|| {
                format!(
                    "Unsupported z.ai region \"{}\" — expected \"global\" or \"bigmodel-cn\".",
                    raw.trim()
                )
            });
        }

        let host = self
            .env
            .get_str(ENV_QUOTA_URL)
            .or_else(|| self.env.get_str(ENV_QUOTA_ENDPOINT))
            .or_else(|| self.env.get_str(ENV_API_HOST))
            .and_then(|raw| secure_base_url(Some(&raw), "").ok())
            .and_then(|url| host_of(&url));
        if host.as_deref() == Some(CN_HOST) {
            Ok(ZaiRegion::BigModelCn)
        } else {
            Ok(ZaiRegion::Global)
        }
    }

    pub fn usage_scope(&self) -> Result<ZaiScope, String> {
        let raw = self
            .config
            .as_ref()
            .and_then(|c| c.field(ProviderId::Zai, "usageScope"))
            .or_else(|| self.env.get_str(ENV_USAGE_SCOPE));
        match raw.as_deref().map(str::trim) {
            None | Some("") => Ok(ZaiScope::Personal),
            Some(value) => match value.to_ascii_lowercase().as_str() {
                "personal" => Ok(ZaiScope::Personal),
                "team" => Ok(ZaiScope::Team),
                other => Err(format!(
                    "Unsupported z.ai usage scope \"{other}\" — expected \"personal\" or \"team\"."
                )),
            },
        }
    }

    /// `(organization, project)` for team usage. Both are required for team.
    pub fn team_context(&self) -> (Option<String>, Option<String>) {
        let field = |name: &str| {
            self.config
                .as_ref()
                .and_then(|c| c.field(ProviderId::Zai, name))
        };
        let organization = self
            .env
            .get_str(ENV_ORGANIZATION)
            .or_else(|| self.env.get_str(ENV_ORGANIZATION_SWIFT))
            .or_else(|| field("organizationID"));
        let project = self
            .env
            .get_str(ENV_PROJECT)
            .or_else(|| self.env.get_str(ENV_PROJECT_SWIFT))
            .or_else(|| field("workspaceID"));
        (organization, project)
    }

    /// Token precedence (`SPEC-apikey.md` §5.1): config → `Z_AI_API_KEY` →
    /// CN-only aliases → CN-only relay files.
    pub fn api_key(&self, region: ZaiRegion) -> Option<Secret> {
        if let Some(config) = &self.config {
            if let Some(token) = config
                .active_token_account(ProviderId::Zai)
                .or_else(|| config.api_key(ProviderId::Zai))
            {
                return Some(token);
            }
        }
        if let Some(token) = self.env.get(ENV_API_KEY) {
            return Some(token);
        }
        if region == ZaiRegion::BigModelCn {
            if let Some(token) = self.env.first_of(&CN_ENV_ALIASES) {
                return Some(token);
            }
            return self.cn_relay_token();
        }
        None
    }

    /// First readable one-line CN relay file. Never runs in tests (`home: None`).
    fn cn_relay_token(&self) -> Option<Secret> {
        let home = self.home.as_ref()?;
        for relative in CN_RELAY_FILES {
            if let Some(token) = read_first_line(&home.join(relative)) {
                return Some(token);
            }
        }
        None
    }

    /// Quota URL: `Z_AI_QUOTA_URL` → `Z_AI_QUOTA_ENDPOINT` → `Z_AI_API_HOST` →
    /// region default. Team scope appends `type=2`.
    pub fn quota_url(&self, region: ZaiRegion, scope: ZaiScope) -> Result<String, String> {
        let keyed = self
            .env
            .get_str(ENV_QUOTA_URL)
            .map(|raw| (ENV_QUOTA_URL, raw))
            .or_else(|| {
                self.env
                    .get_str(ENV_QUOTA_ENDPOINT)
                    .map(|raw| (ENV_QUOTA_ENDPOINT, raw))
            });

        let url = if let Some((key, raw)) = keyed {
            let url = override_url(key, &raw, region)?;
            check_region(&url, region, key)?;
            url
        } else if let Some(raw) = self.env.get_str(ENV_API_HOST) {
            let url = override_url(ENV_API_HOST, &raw, region)?;
            check_region(&url, region, ENV_API_HOST)?;
            join_path(&url, QUOTA_PATH)
        } else {
            join_path(region.base(), QUOTA_PATH)
        };

        Ok(if scope == ZaiScope::Team {
            append_type(&url, "2")
        } else {
            url
        })
    }

    /// Model-usage URL with the `startTime`/`endTime` window the plugin uses.
    /// Team scope appends `&type=3` (`Z_AI_MODEL_USAGE_ENDPOINT` is honoured).
    pub fn model_usage_url(
        &self,
        region: ZaiRegion,
        scope: ZaiScope,
        start_time: &str,
        end_time: &str,
    ) -> Result<String, String> {
        let base = if let Some(raw) = self.env.get_str(ENV_MODEL_USAGE_ENDPOINT) {
            let url = override_url(ENV_MODEL_USAGE_ENDPOINT, &raw, region)?;
            check_region(&url, region, ENV_MODEL_USAGE_ENDPOINT)?;
            url
        } else if let Some(raw) = self.env.get_str(ENV_API_HOST) {
            let url = override_url(ENV_API_HOST, &raw, region)?;
            check_region(&url, region, ENV_API_HOST)?;
            join_path(&url, MODEL_USAGE_PATH)
        } else {
            join_path(region.base(), MODEL_USAGE_PATH)
        };
        let base = base.split(['?', '#']).next().unwrap_or(&base).to_string();
        let team = if scope == ZaiScope::Team {
            "&type=3"
        } else {
            ""
        };
        Ok(format!(
            "{base}?startTime={}&endTime={}{team}",
            percent_encode(start_time),
            percent_encode(end_time)
        ))
    }

    /// Balance URL: `Z_AI_BALANCE_URL` → `Z_AI_BALANCE_ENDPOINT` → CN-only
    /// default. `None` for Global, which has no documented equivalent.
    pub fn balance_url(&self, region: ZaiRegion) -> Result<Option<String>, String> {
        let keyed = self
            .env
            .get_str(ENV_BALANCE_URL)
            .map(|raw| (ENV_BALANCE_URL, raw))
            .or_else(|| {
                self.env
                    .get_str(ENV_BALANCE_ENDPOINT)
                    .map(|raw| (ENV_BALANCE_ENDPOINT, raw))
            });
        if let Some((key, raw)) = keyed {
            return Ok(Some(override_url(key, &raw, region)?));
        }
        if region == ZaiRegion::BigModelCn {
            return Ok(Some(CN_BALANCE_URL_FIXED.to_string()));
        }
        Ok(None)
    }

    /// The Coding Plan quota, mapped to windows. Errors are safe to display.
    fn quota(
        &self,
        url: &str,
        token: &Secret,
        scope: ZaiScope,
        team: (&Option<String>, &Option<String>),
        now: DateTime<Utc>,
    ) -> Result<ZaiQuota, String> {
        let mut request = HttpRequest::get(url)
            .bearer(token)
            .accept_json()
            .timeout(QUOTA_TIMEOUT);
        if scope == ZaiScope::Team {
            if let Some(organization) = team.0 {
                request = request.header("Bigmodel-Organization", organization.clone());
            }
            if let Some(project) = team.1 {
                request = request.header("Bigmodel-Project", project.clone());
            }
        }
        let response = self.client.send_ok(&request).map_err(|err| err.message)?;
        let body: Value = response.json().map_err(|err| err.message)?;
        ZaiQuota::from_json(&body, now)
    }

    /// BigModel CN account balance. Best-effort; `Ok(None)` = "no balance row".
    fn balance(&self, url: &str, token: &Secret) -> Result<Option<MoneyBalance>, String> {
        let request = HttpRequest::get(url)
            .bearer(token)
            .accept_json()
            .timeout(BALANCE_TIMEOUT);
        let response = self.client.send_ok(&request).map_err(|err| err.message)?;
        let body: Value = response.json().map_err(|err| err.message)?;
        Ok(balance_from_json(&body))
    }

    /// `GET` + parse a model-usage response. Not called by `fetch` — see the
    /// module header. Exposed so the chart worker can reuse it.
    pub fn model_usage(
        &self,
        url: &str,
        token: &Secret,
        team: (&Option<String>, &Option<String>),
    ) -> Result<ModelUsage, String> {
        let mut request = HttpRequest::get(url)
            .bearer(token)
            .accept_json()
            .timeout(QUOTA_TIMEOUT);
        if let Some(organization) = team.0 {
            request = request.header("Bigmodel-Organization", organization.clone());
        }
        if let Some(project) = team.1 {
            request = request.header("Bigmodel-Project", project.clone());
        }
        let response = self.client.send_ok(&request).map_err(|err| err.message)?;
        let body: Value = response.json().map_err(|err| err.message)?;
        ModelUsage::from_json(&body)
            .ok_or_else(|| "z.ai model-usage response had no data.x_time/modelDataList".to_string())
    }
}

impl Default for Zai {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Zai {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Zai")
            .field("has_credentials", &self.env.has(ENV_API_KEY))
            .field("config", &self.config.as_ref().map(PortConfig::source_path))
            .finish()
    }
}

impl Provider for Zai {
    fn id(&self) -> ProviderId {
        ProviderId::Zai
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::Zai,
            title: ProviderId::Zai.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::ApiKey,
            fetched_at: now,
        };

        let region = match self.region() {
            Ok(region) => region,
            Err(message) => return failed(snapshot, message),
        };
        let scope = match self.usage_scope() {
            Ok(scope) => scope,
            Err(message) => return failed(snapshot, message),
        };

        // Missing credentials are `notConfigured`, not `error`.
        let Some(token) = self.api_key(region) else {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(
                "No z.ai API token found — set Z_AI_API_KEY (the BigModel CN region also accepts \
                 BIGMODEL_API_KEY / ZHIPU_API_KEY / …), or add providers[].apiKey for \"zai\" to \
                 %APPDATA%\\CodexBar\\config.json."
                    .to_string(),
            );
            return snapshot;
        };
        // Masked, never the value: this string reaches the UI, logs and screenshots.
        snapshot.account = Some(token.redacted());

        let team = self.team_context();
        if scope == ZaiScope::Team && (team.0.is_none() || team.1.is_none()) {
            return failed(
                snapshot,
                "z.ai team scope needs both an organization id and a project id \
                 (Z_AI_ORGANIZATION / Z_AI_PROJECT)."
                    .to_string(),
            );
        }

        // Endpoint policy — fail closed *before* the bearer is attached.
        let quota_url = match self.quota_url(region, scope) {
            Ok(url) => url,
            Err(message) => return failed(snapshot, message),
        };
        let balance_url = match self.balance_url(region) {
            Ok(url) => url,
            Err(message) => return failed(snapshot, message),
        };

        let quota = match self.quota(&quota_url, &token, scope, (&team.0, &team.1), now) {
            Ok(quota) => quota,
            Err(message) => return failed(snapshot, message),
        };
        snapshot.plan = quota.plan;
        snapshot.windows = quota.windows;

        // Balance is a CN-only, best-effort extra: a failure keeps the quota.
        let mut notes: Vec<String> = Vec::new();
        if let Some(url) = balance_url {
            match self.balance(&url, &token) {
                Ok(Some(balance)) => snapshot.balance = Some(balance),
                Ok(None) => {}
                Err(message) => notes.push(format!(
                    "z.ai account balance unavailable right now ({message})"
                )),
            }
        }
        if !notes.is_empty() {
            snapshot.error = Some(notes.join("; "));
        }

        snapshot
    }
}

/// The parsed `data` block of a quota response.
#[derive(Debug, Clone, PartialEq)]
pub struct ZaiQuota {
    pub plan: Option<String>,
    pub windows: Vec<NamedRateWindow>,
}

impl ZaiQuota {
    /// `success == true && code == 200`, `data.limits[]`, plan fields.
    pub fn from_json(body: &Value, now: DateTime<Utc>) -> Result<Self, String> {
        let Some(root) = body.as_object() else {
            return Err("z.ai quota API returned a non-object body".to_string());
        };
        let ok = root.get("success").and_then(Value::as_bool) == Some(true)
            && root.get("code").and_then(as_integer) == Some(200);
        if !ok {
            let message = root
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("invalid response");
            return Err(format!(
                "z.ai quota API error: {}",
                redact_secrets_in_text(message)
            ));
        }

        let Some(data) = root.get("data").and_then(Value::as_object) else {
            return Err("Failed to parse z.ai quota data".to_string());
        };
        let Some(limits) = data.get("limits").and_then(Value::as_array) else {
            return Err("Failed to parse z.ai quota data (data.limits)".to_string());
        };

        let mut parsed: Vec<ZaiLimit> = Vec::new();
        for raw in limits {
            if let Some(limit) = ZaiLimit::from_json(raw)? {
                parsed.push(limit);
            }
        }
        if parsed.is_empty() {
            return Err("z.ai quota response had no usable limits".to_string());
        }

        let plan = ["planName", "plan", "plan_type", "packageName", "level"]
            .iter()
            .find_map(|key| data.get(*key).and_then(non_empty_str));

        Ok(Self {
            plan,
            windows: map_windows(&parsed, now),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZaiLimitType {
    Tokens,
    Credit,
    Time,
}

/// One `data.limits[]` entry, with the percentage already resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ZaiLimit {
    pub kind: ZaiLimitType,
    pub unit: i64,
    pub number: i64,
    pub percent: f64,
    pub window_minutes: Option<i64>,
    pub reset_ms: Option<i64>,
}

impl ZaiLimit {
    /// `Ok(None)` for a limit type we do not publish. `Err` on a malformed entry
    /// (the plugin rejects the whole response rather than guessing).
    pub fn from_json(raw: &Value) -> Result<Option<Self>, String> {
        let Some(object) = raw.as_object() else {
            return Err("Failed to parse z.ai limit entry".to_string());
        };
        let Some(type_) = object.get("type").and_then(Value::as_str) else {
            return Err("Failed to parse z.ai limit entry".to_string());
        };
        let kind = match type_ {
            "TOKENS_LIMIT" => ZaiLimitType::Tokens,
            "TIME_LIMIT" => ZaiLimitType::Time,
            "CREDIT_LIMIT" => ZaiLimitType::Credit,
            _ => return Ok(None),
        };
        let unit = required_int(object, "unit")?;
        let number = required_int(object, "number")?;
        let percentage = required_int(object, "percentage")?;
        let usage = optional_int(object, "usage")?;
        let current = optional_int(object, "currentValue")?;
        let remaining = optional_int(object, "remaining")?;

        let mut percent = percentage as f64;
        if let Some(usage) = usage {
            if usage > 0 {
                let used = match (remaining, current) {
                    (Some(remaining), Some(current)) => Some((usage - remaining).max(current)),
                    (Some(remaining), None) => Some(usage - remaining),
                    (None, Some(current)) => Some(current),
                    (None, None) => None,
                };
                if let Some(used) = used {
                    percent = (used.clamp(0, usage) as f64) / (usage as f64) * 100.0;
                }
            }
        }

        Ok(Some(Self {
            kind,
            unit,
            number,
            percent: percent.clamp(0.0, 100.0),
            window_minutes: minutes_for(unit, number),
            reset_ms: optional_int(object, "nextResetTime")?,
        }))
    }
}

/// `unit`+`number` → minutes (`{1: day, 3: hour, 5: minute, 6: week}`).
pub fn minutes_for(unit: i64, number: i64) -> Option<i64> {
    if number <= 0 {
        return None;
    }
    let multiplier = match unit {
        1 => 1_440,
        3 => 60,
        5 => 1,
        6 => 10_080,
        _ => return None,
    };
    Some(number * multiplier)
}

/// `data.limits[]` → the lanes the contract understands, in plugin order.
fn map_windows(limits: &[ZaiLimit], now: DateTime<Utc>) -> Vec<NamedRateWindow> {
    let mut token_limits: Vec<&ZaiLimit> = limits
        .iter()
        .filter(|limit| limit.kind != ZaiLimitType::Time)
        .collect();
    token_limits.sort_by_key(|limit| limit.window_minutes.unwrap_or(i64::MAX));

    let time_limit = limits
        .iter()
        .rfind(|limit| limit.kind == ZaiLimitType::Time);
    let token_limit = token_limits.last().copied();
    let session_limit = if token_limits.len() >= 2 {
        token_limits.first().copied()
    } else {
        None
    };

    let mut windows: Vec<NamedRateWindow> = Vec::new();
    if let Some(primary) = session_limit.or(token_limit).or(time_limit) {
        windows.push(lane(primary, now));
    }
    if session_limit.is_some() {
        if let Some(secondary) = token_limit {
            windows.push(lane(secondary, now));
        }
    }
    if let Some(mcp) = time_limit {
        if token_limit.is_some() || session_limit.is_some() {
            windows.push(lane(mcp, now));
        }
    }
    dedupe_ids(windows)
}

fn lane(limit: &ZaiLimit, now: DateTime<Utc>) -> NamedRateWindow {
    let window = rate_window(limit, now);
    let (id, title, kind) = lane_identity(limit, window.window_minutes);
    NamedRateWindow::new(id, title, kind, window)
}

fn rate_window(limit: &ZaiLimit, now: DateTime<Utc>) -> RateWindow {
    let minutes = if limit.kind == ZaiLimitType::Time {
        // A `TIME_LIMIT` unit 5 / number 1 is the monthly MCP marker.
        if limit.unit == 5 && limit.number == 1 {
            Some(30 * 24 * 60)
        } else {
            limit.window_minutes
        }
    } else {
        limit.window_minutes
    };
    let mut window = RateWindow::new(limit.percent, minutes, None);
    if let Some(ms) = limit.reset_ms {
        if reset_is_plausible(limit, minutes, ms, now) {
            window.resets_at = DateTime::from_timestamp_millis(ms);
        }
    }
    window.reset_description = reset_description(limit, minutes);
    window
}

/// A five-hour Coding Plan reset cannot be more than five hours plus one minute
/// of clock skew away. This port never guesses a timezone correction.
fn reset_is_plausible(
    limit: &ZaiLimit,
    minutes: Option<i64>,
    reset_ms: i64,
    now: DateTime<Utc>,
) -> bool {
    if limit.kind == ZaiLimitType::Time || minutes != Some(300) {
        return true;
    }
    reset_ms <= now.timestamp_millis() + (5 * 3_600 + 60) * 1_000
}

fn reset_description(limit: &ZaiLimit, minutes: Option<i64>) -> Option<String> {
    if limit.kind == ZaiLimitType::Time {
        return Some("MCP".to_string());
    }
    if minutes == Some(300) {
        return Some("5-hour".to_string());
    }
    // No duration means no window text to describe.
    minutes?;
    let name = match limit.unit {
        1 => "day",
        3 => "hour",
        5 => "minute",
        6 => "week",
        _ => return None,
    };
    Some(format!(
        "{} {name}{} window",
        limit.number,
        if limit.number == 1 { "" } else { "s" }
    ))
}

fn lane_identity(limit: &ZaiLimit, minutes: Option<i64>) -> (String, String, WindowKind) {
    if limit.kind == ZaiLimitType::Time {
        return ("zai-mcp".to_string(), "MCP".to_string(), WindowKind::Extra);
    }
    match minutes {
        Some(300) => (
            "session".to_string(),
            "Session · 5h".to_string(),
            WindowKind::Session,
        ),
        Some(10_080) => (
            "weekly".to_string(),
            "Weekly · 7d".to_string(),
            WindowKind::Weekly,
        ),
        Some(1_440) => ("daily".to_string(), "Daily".to_string(), WindowKind::Extra),
        Some(other) => (
            format!("quota-{other}m"),
            format!("Quota · {}", humanize_minutes(other)),
            WindowKind::Extra,
        ),
        None => ("quota".to_string(), "Quota".to_string(), WindowKind::Extra),
    }
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

fn humanize_minutes(minutes: i64) -> String {
    if minutes % 10_080 == 0 {
        format!("{}d", minutes / 10_080)
    } else if minutes % 1_440 == 0 {
        format!("{}d", minutes / 1_440)
    } else if minutes % 60 == 0 {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

/// BigModel CN balance body → the contract's [`MoneyBalance`].
fn balance_from_json(body: &Value) -> Option<MoneyBalance> {
    let root = body.as_object()?;
    if root.get("success").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let data = root.get("data").and_then(Value::as_object);
    let data = data?;
    // `Number(null)` is 0 in JS, which would silently defeat the fallback.
    let available = data.get("availableBalance").and_then(finite_number);
    let current = data.get("balance").and_then(finite_number);
    let value = available.or(current)?;
    Some(MoneyBalance {
        amount: round2(value),
        currency: "CNY".to_string(),
        label: Some("Account balance".to_string()),
    })
}

/// One point of a model-usage chart.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsagePoint {
    pub label: String,
    pub tokens: u64,
}

/// The parsed `data` block of a model-usage response (`x_time` + `modelDataList`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelUsage {
    pub points: Vec<ModelUsagePoint>,
    pub totals: Vec<ModelUsagePoint>,
}

impl ModelUsage {
    pub fn from_json(body: &Value) -> Option<Self> {
        let root = body.as_object()?;
        if root.get("success").and_then(Value::as_bool) != Some(true)
            || root.get("code").and_then(as_integer) != Some(200)
        {
            return None;
        }
        let empty = serde_json::Map::new();
        let data = root
            .get("data")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let labels = data.get("x_time").and_then(Value::as_array);
        let labels = labels?;
        let models = data
            .get("modelDataList")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .map(|model| {
                        let name = model
                            .get("modelName")
                            .and_then(Value::as_str)
                            .unwrap_or("Unknown")
                            .to_string();
                        let tokens: Vec<u64> = model
                            .get("tokensUsage")
                            .and_then(Value::as_array)
                            .map(|values| {
                                values
                                    .iter()
                                    .map(|value| {
                                        as_integer(value)
                                            .filter(|v| *v > 0)
                                            .map(|v| v as u64)
                                            .unwrap_or(0)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        (name, tokens)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let points = labels
            .iter()
            .enumerate()
            .map(|(index, label)| ModelUsagePoint {
                label: label.as_str().map(str::to_string).unwrap_or_default(),
                tokens: models
                    .iter()
                    .map(|(_, tokens)| tokens.get(index).copied().unwrap_or(0))
                    .sum(),
            })
            .filter(|point| point.tokens > 0)
            .collect();

        let mut totals: Vec<ModelUsagePoint> = models
            .iter()
            .map(|(name, tokens)| ModelUsagePoint {
                label: name.clone(),
                tokens: tokens.iter().sum(),
            })
            .filter(|item| item.tokens > 0)
            .collect();
        totals.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.label.cmp(&b.label)));

        Some(Self { points, totals })
    }
}

fn failed(mut snapshot: ProviderSnapshot, message: String) -> ProviderSnapshot {
    snapshot.status = FetchStatus::Error;
    snapshot.error = Some(message);
    snapshot
}

/// Normalise an override to HTTPS (bare host → `https://…`) at `key`.
fn override_url(key: &str, raw: &str, region: ZaiRegion) -> Result<String, String> {
    secure_base_url(Some(raw), region.base())
        .map_err(|reason| format!("{key} is not usable: {reason}"))
}

/// A canonical host may not contradict the selected region.
fn check_region(url: &str, region: ZaiRegion, key: &str) -> Result<(), String> {
    let Some(host) = host_of(url) else {
        return Ok(());
    };
    if (host == GLOBAL_HOST || host == CN_HOST) && host != region.host() {
        return Err(format!(
            "z.ai endpoint override {key} ({host}) does not match the selected {} region.",
            region.as_str()
        ));
    }
    Ok(())
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, rest)| rest)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Append `path` when `base` carries no path of its own (Swift `endpointURL`).
fn join_path(base: &str, path: &str) -> String {
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

/// Replace any existing `type` query parameter and append `type={value}`.
fn append_type(url: &str, value: &str) -> String {
    let mut parts = url.splitn(2, '?');
    let base = parts.next().unwrap_or("");
    let mut query: Vec<String> = parts
        .next()
        .map(|query| {
            query
                .split('&')
                .filter(|item| !item.is_empty())
                .filter(|item| item.split('=').next() != Some("type"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    query.push(format!("type={value}"));
    format!("{base}?{}", query.join("&"))
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// JSON integer accepting `200` and `200.0` but not `"200"`.
fn as_integer(value: &Value) -> Option<i64> {
    if let Some(int) = value.as_i64() {
        return Some(int);
    }
    value
        .as_f64()
        .filter(|float| float.fract() == 0.0)
        .map(|float| float as i64)
}

fn finite_number(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return number.is_finite().then_some(number);
    }
    value
        .as_str()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|number| number.is_finite())
}

fn optional_int(object: &serde_json::Map<String, Value>, key: &str) -> Result<Option<i64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => as_integer(value)
            .map(Some)
            .ok_or_else(|| format!("z.ai limit.{key} must be an integer")),
    }
}

fn required_int(object: &serde_json::Map<String, Value>, key: &str) -> Result<i64, String> {
    optional_int(object, key)?.ok_or_else(|| format!("Failed to parse z.ai limit entry ({key})"))
}

fn non_empty_str(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(str::to_string)
}

/// First line of a one-line credential file, cleaned.
fn read_first_line(path: &Path) -> Option<Secret> {
    let text = std::fs::read_to_string(path).ok()?;
    cleaned(text.lines().next().unwrap_or("")).map(Secret::new)
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
    fn unit_multipliers_match_the_plugin() {
        assert_eq!(minutes_for(1, 1), Some(1_440));
        assert_eq!(minutes_for(3, 5), Some(300));
        assert_eq!(minutes_for(5, 30), Some(30));
        assert_eq!(minutes_for(6, 1), Some(10_080));
        assert_eq!(minutes_for(9, 1), None);
        assert_eq!(minutes_for(1, 0), None);
    }

    #[test]
    fn percentage_is_recalculated_from_counts_and_clamped() {
        let limit = ZaiLimit::from_json(&serde_json::json!({
            "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 1,
            "usage": 1000, "currentValue": 250, "remaining": 700
        }))
        .unwrap()
        .unwrap();
        // max(1000 - 700, 250) = 300 of 1000 → 30 %.
        assert_eq!(limit.percent, 30.0);
        assert_eq!(limit.window_minutes, Some(300));

        let overflow = ZaiLimit::from_json(&serde_json::json!({
            "type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 10,
            "usage": 10, "currentValue": 40
        }))
        .unwrap()
        .unwrap();
        assert_eq!(overflow.percent, 100.0);
    }

    #[test]
    fn unknown_limit_types_are_skipped_and_malformed_ones_rejected() {
        assert!(ZaiLimit::from_json(&serde_json::json!({
            "type": "OTHER_LIMIT", "unit": 1, "number": 1, "percentage": 0
        }))
        .unwrap()
        .is_none());
        assert!(ZaiLimit::from_json(&serde_json::json!({
            "type": "TOKENS_LIMIT", "unit": "1", "number": 1, "percentage": 0
        }))
        .is_err());
    }

    #[test]
    fn five_hour_resets_further_than_five_hours_are_omitted() {
        let instant = now();
        let plausible = ZaiLimit {
            kind: ZaiLimitType::Tokens,
            unit: 3,
            number: 5,
            percent: 10.0,
            window_minutes: Some(300),
            reset_ms: Some(instant.timestamp_millis() + 60_000),
        };
        assert!(rate_window(&plausible, instant).resets_at.is_some());
        assert_eq!(
            rate_window(&plausible, instant)
                .reset_description
                .as_deref(),
            Some("5-hour")
        );

        let implausible = ZaiLimit {
            reset_ms: Some(instant.timestamp_millis() + 10 * 3_600 * 1_000),
            ..plausible
        };
        assert!(rate_window(&implausible, instant).resets_at.is_none());
    }

    #[test]
    fn balance_falls_back_to_the_current_field_only_when_numeric() {
        assert_eq!(
            balance_from_json(&serde_json::json!({
                "success": true, "data": {"availableBalance": null, "balance": 12.5}
            })),
            Some(MoneyBalance {
                amount: 12.5,
                currency: "CNY".to_string(),
                label: Some("Account balance".to_string()),
            })
        );
        assert!(balance_from_json(&serde_json::json!({
            "success": true, "data": {"availableBalance": null, "balance": null}
        }))
        .is_none());
        assert!(balance_from_json(&serde_json::json!({"success": false})).is_none());
    }

    #[test]
    fn model_usage_sums_models_per_point_and_sorts_totals() {
        let usage = ModelUsage::from_json(&serde_json::json!({
            "success": true, "code": 200,
            "data": {
                "x_time": ["10:00", "11:00", "12:00"],
                "modelDataList": [
                    {"modelName": "glm-4.5", "tokensUsage": [10, 20, 0]},
                    {"modelName": "glm-4-air", "tokensUsage": [5, 0, 0]}
                ]
            }
        }))
        .unwrap();
        assert_eq!(usage.points.len(), 2);
        assert_eq!(usage.points[0].tokens, 15);
        assert_eq!(usage.totals[0].label, "glm-4.5");
        assert_eq!(usage.totals[1].label, "glm-4-air");
    }

    #[test]
    fn append_type_replaces_the_existing_selector() {
        assert_eq!(
            append_type("https://api.z.ai/api/monitor/usage/quota/limit", "2"),
            "https://api.z.ai/api/monitor/usage/quota/limit?type=2"
        );
        assert_eq!(
            append_type("https://api.z.ai/q?type=9&x=1", "2"),
            "https://api.z.ai/q?x=1&type=2"
        );
    }
}
