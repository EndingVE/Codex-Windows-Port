//! **OpenCode Go** — the API-key provider with a device-local fallback.
//!
//! OpenCode Go is the one provider in `SPEC-apikey.md` §2 with data the port can
//! verify on the user's own machine, so it has three sources, in the spec's `auto`
//! order:
//!
//! 1. **Authoritative quota** — `GET https://opencode.ai/zen/go/v1/usage` with a
//!    bearer token. Response: `usage.{rolling,weekly,monthly}` each carrying a
//!    `usagePercent` (0–100) and a `resetInSec` (`SPEC-apikey.md` §2.2).
//! 2. **Local estimate** — an aggregate of the last 5 h / 7 d / 30 d of
//!    `opencode-go` cost rows in `%USERPROFILE%\.local\share\opencode\opencode.db`
//!    against the spec's session/weekly/monthly USD limits (§2.4).
//! 3. **Auth signal** — `%USERPROFILE%\.local\share\opencode\auth.json` with an
//!    `opencode-go.key`. The port reads it as the fallback bearer when no
//!    `OPENCODE_API_KEY` / config key exists, and as the "has the user ever run
//!    OpenCode Go locally?" signal.
//!
//! The database is opened **read-only**. The port never creates a file next to it
//! and never writes to it: a database with no active `-wal`/`-shm` sidecars is
//! opened with `?immutable=1` (which reads the idle main file without recreating
//! them), and only a database that still has its sidecars is opened the ordinary
//! way. This is stricter than `OpenCodeGoLocalUsageReader.swift` on purpose: a
//! normal read-only open of a WAL database silently recreates the sidecars, which
//! is a write beside the user's OpenCode state and is never acceptable here.
//!
//! Windows path note (`SPEC-apikey.md` §2.1): OpenCode uses the literal XDG form
//! `%USERPROFILE%\.local\share\opencode`, **not** `%APPDATA%` — [`xdg_data_home`]
//! already encodes that convention.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, TimeZone, Timelike, Utc};
use codexbar_core::{
    DataSource, FetchStatus, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    WindowKind,
};

use crate::credential::{cleaned, read_json, xdg_data_home, Env, PortConfig, Secret};
use crate::http::{secure_base_url, HttpClient, HttpError, HttpRequest};

/// Bearer credential, the same name the macOS provider reads
/// (`OpenCodeGoSettingsReader.apiKeyEnvironmentKey`).
pub const ENV_API_KEY: &str = "OPENCODE_API_KEY";
/// Optional endpoint override — HTTPS only, fail-closed.
pub const ENV_USAGE_URL: &str = "OPENCODE_GO_USAGE_URL";

const ENV_ALIASES: [&str; 1] = [ENV_API_KEY];

/// Authoritative quota endpoint (`SPEC-apikey.md` §2.2).
pub const DEFAULT_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

/// Local cost limits in USD (`SPEC-apikey.md` §2.4).
pub const SESSION_LIMIT_USD: f64 = 12.0;
pub const WEEKLY_LIMIT_USD: f64 = 30.0;
pub const MONTHLY_LIMIT_USD: f64 = 60.0;

/// The rolling session window is a fixed 5 hours; weekly is 7 days; monthly is a
/// nominal 30 days (the real anchor is the billing cycle).
const SESSION_WINDOW_MINUTES: i64 = 300;
const WEEKLY_WINDOW_MINUTES: i64 = 10_080;
const MONTHLY_WINDOW_MINUTES: i64 = 43_200;
const FIVE_HOURS_MS: i64 = 5 * 60 * 60 * 1000;
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The quota call is a foreground refresh; it must not hold the 60 s tick.
const USAGE_TIMEOUT: Duration = Duration::from_secs(10);
/// SQLite busy timeout, matching the Swift reader.
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// Where OpenCode keeps its local state. Injectable so tests never touch the
/// real home directory.
#[derive(Debug, Clone)]
pub struct OpenCodePaths {
    /// `…/opencode/auth.json`.
    pub auth: PathBuf,
    /// `…/opencode/opencode.db`.
    pub database: PathBuf,
}

impl OpenCodePaths {
    pub fn new(auth: impl Into<PathBuf>, database: impl Into<PathBuf>) -> Self {
        Self {
            auth: auth.into(),
            database: database.into(),
        }
    }

    /// `%USERPROFILE%\.local\share\opencode\{auth.json,opencode.db}` (Windows uses
    /// the literal XDG layout for OpenCode, not `%APPDATA%`).
    pub fn from_env(env: &Env) -> Option<Self> {
        let directory = xdg_data_home(env)?.join("opencode");
        Some(Self {
            auth: directory.join("auth.json"),
            database: directory.join("opencode.db"),
        })
    }
}

pub struct OpenCodeGo {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
    paths: Option<OpenCodePaths>,
}

impl OpenCodeGo {
    /// Production constructor: real HTTPS client, process environment, the port's
    /// config file, and the machine's OpenCode state directory.
    pub fn new() -> Self {
        let env = Env::from_process();
        let config = PortConfig::load(&env).unwrap_or(None);
        let paths = OpenCodePaths::from_env(&env);
        Self {
            client: crate::http::shared_client(),
            env,
            config,
            paths,
        }
    }

    /// Test/embed constructor: no disk reads, no network.
    pub fn with_client(client: Arc<dyn HttpClient>, env: Env) -> Self {
        Self {
            client,
            env,
            config: None,
            paths: None,
        }
    }

    pub fn with_config(mut self, config: Option<PortConfig>) -> Self {
        self.config = config;
        self
    }

    pub fn with_paths(mut self, paths: Option<OpenCodePaths>) -> Self {
        self.paths = paths;
        self
    }

    /// `config[].apiKey` → `OPENCODE_API_KEY` → `auth.json` `opencode-go.key`.
    fn api_key(&self) -> Option<Secret> {
        let configured = match &self.config {
            Some(config) => config.resolve_api_key(ProviderId::OpenCodeGo, &self.env, &ENV_ALIASES),
            None => self.env.first_of(&ENV_ALIASES),
        };
        configured.or_else(|| self.auth_key())
    }

    /// `auth.json` → `{"opencode-go":{"key":"…"}}`. Read-only, best-effort: a
    /// missing or malformed file simply means "no local auth signal".
    fn auth_key(&self) -> Option<Secret> {
        let auth = self.paths.as_ref()?.auth.as_path();
        let root = read_json(auth).ok()?;
        let raw = root
            .get("opencode-go")
            .and_then(|entry| entry.get("key"))
            .and_then(|value| value.as_str())?;
        cleaned(raw).map(Secret::new)
    }

    fn usage_url(&self) -> Result<String, String> {
        secure_base_url(
            self.env.get_str(ENV_USAGE_URL).as_deref(),
            DEFAULT_USAGE_URL,
        )
    }

    fn fetch_api(
        &self,
        url: &str,
        api_key: &Secret,
        now: DateTime<Utc>,
    ) -> Result<ApiUsage, HttpError> {
        let request = HttpRequest::get(url)
            .bearer(api_key)
            .accept_json()
            .timeout(USAGE_TIMEOUT);
        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        ApiUsage::from_json(&body, now).ok_or_else(|| {
            HttpError::decode("OpenCode Go usage response had no rolling/weekly/monthly windows")
        })
    }

    /// Best-effort device-local aggregate. `Ok(None)` means "nothing local to
    /// report" (no database, or no `opencode-go` rows).
    fn local_usage(&self, now: DateTime<Utc>) -> Result<Option<LocalUsage>, String> {
        let Some(paths) = &self.paths else {
            return Ok(None);
        };
        if !paths.database.is_file() {
            return Ok(None);
        }
        let rows = read_rows(&paths.database)?;
        if rows.is_empty() {
            return Ok(None);
        }
        Ok(Some(LocalUsage::aggregate(&rows, now)))
    }
}

impl Default for OpenCodeGo {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OpenCodeGo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenCodeGo")
            .field("has_credentials", &self.api_key().is_some())
            .field("database", &self.paths.as_ref().map(|p| p.database.clone()))
            .finish()
    }
}

/// A parsed quota window from the API.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ApiWindow {
    pub percent: f64,
    /// Absolute reset instant, resolved from `resetInSec` (relative) or an
    /// absolute `resetsAt`/`resetAt` field.
    pub resets_at: Option<DateTime<Utc>>,
}

/// The `/zen/go/v1/usage` payload (`SPEC-apikey.md` §2.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ApiUsage {
    pub rolling: ApiWindow,
    pub weekly: Option<ApiWindow>,
    pub monthly: Option<ApiWindow>,
}

impl ApiUsage {
    pub fn from_json(body: &serde_json::Value, now: DateTime<Utc>) -> Option<Self> {
        // The API nestles the windows under `usage`; tolerate a top-level shape too.
        let scope = match body.get("usage") {
            Some(usage) if usage.is_object() => usage,
            _ => body,
        };
        let rolling = window(
            scope,
            &["rolling", "rollingUsage", "rolling_usage", "rollingWindow"],
            now,
        )?;
        Some(Self {
            rolling,
            weekly: window(
                scope,
                &["weekly", "weeklyUsage", "weekly_usage", "weeklyWindow"],
                now,
            ),
            monthly: window(
                scope,
                &["monthly", "monthlyUsage", "monthly_usage", "monthlyWindow"],
                now,
            ),
        })
    }
}

/// A device-local cost aggregate over the three spec windows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalUsage {
    pub session_percent: f64,
    pub weekly_percent: f64,
    pub monthly_percent: f64,
    pub session_reset_in_sec: i64,
    pub weekly_reset_in_sec: i64,
    pub monthly_reset_in_sec: i64,
}

impl LocalUsage {
    fn aggregate(rows: &[UsageRow], now: DateTime<Utc>) -> Self {
        let now_ms = now.timestamp_millis();
        let session_start = now_ms - FIVE_HOURS_MS;
        let week_start = start_of_utc_week(now).timestamp_millis();
        let week_end = week_start + WEEK_MS;
        let earliest = rows.iter().map(|row| row.created_ms).min();
        let (month_start, month_end) = month_bounds(now, earliest);

        let mut session_cost = 0.0_f64;
        let mut weekly_cost = 0.0_f64;
        let mut monthly_cost = 0.0_f64;
        let mut oldest_session = None;
        for row in rows {
            if row.created_ms >= session_start && row.created_ms < now_ms {
                session_cost += row.cost;
                if oldest_session.map_or(true, |oldest| row.created_ms < oldest) {
                    oldest_session = Some(row.created_ms);
                }
            }
            if row.created_ms >= week_start && row.created_ms < week_end {
                weekly_cost += row.cost;
            }
            if row.created_ms >= month_start && row.created_ms < month_end {
                monthly_cost += row.cost;
            }
        }

        let oldest_session = oldest_session.unwrap_or(now_ms);
        Self {
            session_percent: percent(session_cost, SESSION_LIMIT_USD),
            weekly_percent: percent(weekly_cost, WEEKLY_LIMIT_USD),
            monthly_percent: percent(monthly_cost, MONTHLY_LIMIT_USD),
            session_reset_in_sec: max_zero((oldest_session + FIVE_HOURS_MS - now_ms) / 1000),
            weekly_reset_in_sec: max_zero((week_end - now_ms) / 1000),
            monthly_reset_in_sec: max_zero((month_end - now_ms) / 1000),
        }
    }
}

impl Provider for OpenCodeGo {
    fn id(&self) -> ProviderId {
        ProviderId::OpenCodeGo
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::OpenCodeGo,
            title: ProviderId::OpenCodeGo.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::ApiKey,
            fetched_at: now,
        };

        // 1. Endpoint policy — fail closed before the bearer is attached.
        let url = match self.usage_url() {
            Ok(url) => url,
            Err(reason) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!("{ENV_USAGE_URL} is not usable: {reason}"));
                return snapshot;
            }
        };

        // 2. Credentials and the local history, both read up front (no writes).
        let api_key = self.api_key();
        let local = self.local_usage(now);

        let mut notes: Vec<String> = Vec::new();
        let mut api: Option<ApiUsage> = None;
        if let Some(key) = api_key.as_ref() {
            match self.fetch_api(&url, key, now) {
                Ok(usage) => {
                    // Masked, never the value — this string reaches the UI and
                    // screenshots. Only claimed when the numbers are the account's.
                    snapshot.account = Some(key.redacted());
                    api = Some(usage);
                }
                Err(err) => notes.push(format!(
                    "OpenCode Go usage API unavailable right now ({})",
                    err.kind.as_str()
                )),
            }
        }

        // 3a. Authoritative quota wins when the API answered.
        if let Some(usage) = api {
            snapshot.source = DataSource::ApiKey;
            push_api_windows(&mut snapshot.windows, &usage);
            snapshot.status = FetchStatus::Ok;
            if !notes.is_empty() {
                snapshot.error = Some(notes.join("; "));
            }
            return snapshot;
        }

        // 3b. Otherwise the device-local aggregate is the honest fallback.
        match local {
            Ok(Some(local)) => {
                snapshot.source = DataSource::Cli;
                push_local_windows(&mut snapshot.windows, &local, now);
                notes.push(
                    "Estimated from local opencode.db device history — not the account quota"
                        .to_string(),
                );
                snapshot.status = FetchStatus::Ok;
                snapshot.error = Some(notes.join("; "));
            }
            Ok(None) => {
                if api_key.is_some() {
                    snapshot.status = FetchStatus::Error;
                    notes.push("no local OpenCode Go usage history was found".to_string());
                } else {
                    snapshot.status = FetchStatus::NotConfigured;
                    notes.push(no_credentials_hint(self.paths.as_ref()));
                }
                snapshot.error = Some(notes.join("; "));
            }
            Err(message) => {
                snapshot.status = FetchStatus::Error;
                notes.push(format!(
                    "OpenCode Go local usage history is unavailable: {message}"
                ));
                snapshot.error = Some(notes.join("; "));
            }
        }

        snapshot
    }
}

fn no_credentials_hint(paths: Option<&OpenCodePaths>) -> String {
    let database = paths
        .map(|p| p.database.display().to_string())
        .unwrap_or_else(|| "%USERPROFILE%\\.local\\share\\opencode\\opencode.db".to_string());
    format!(
        "No OpenCode Go credentials found — set {ENV_API_KEY}, add providers[].apiKey for \
         \"opencodego\", or run OpenCode Go locally so {database} exists."
    )
}

fn push_api_windows(windows: &mut Vec<NamedRateWindow>, usage: &ApiUsage) {
    windows.push(api_window(
        "session",
        "Session · 5h",
        WindowKind::Session,
        SESSION_WINDOW_MINUTES,
        usage.rolling,
    ));
    if let Some(weekly) = usage.weekly {
        windows.push(api_window(
            "weekly",
            "Weekly · 7d",
            WindowKind::Weekly,
            WEEKLY_WINDOW_MINUTES,
            weekly,
        ));
    }
    if let Some(monthly) = usage.monthly {
        windows.push(api_window(
            "monthly",
            "Monthly · 30d",
            WindowKind::Extra,
            MONTHLY_WINDOW_MINUTES,
            monthly,
        ));
    }
}

fn api_window(
    id: &str,
    title: &str,
    kind: WindowKind,
    minutes: i64,
    window: ApiWindow,
) -> NamedRateWindow {
    NamedRateWindow::new(
        id,
        title,
        kind,
        RateWindow::new(window.percent, Some(minutes), window.resets_at),
    )
}

fn push_local_windows(windows: &mut Vec<NamedRateWindow>, local: &LocalUsage, now: DateTime<Utc>) {
    for (id, title, kind, minutes, used, reset) in [
        (
            "session",
            "Session · 5h",
            WindowKind::Session,
            SESSION_WINDOW_MINUTES,
            local.session_percent,
            local.session_reset_in_sec,
        ),
        (
            "weekly",
            "Weekly · 7d",
            WindowKind::Weekly,
            WEEKLY_WINDOW_MINUTES,
            local.weekly_percent,
            local.weekly_reset_in_sec,
        ),
        (
            "monthly",
            "Monthly · 30d",
            WindowKind::Extra,
            MONTHLY_WINDOW_MINUTES,
            local.monthly_percent,
            local.monthly_reset_in_sec,
        ),
    ] {
        windows.push(NamedRateWindow::new(
            id,
            title,
            kind,
            RateWindow::new(used, Some(minutes), reset_at(now, Some(reset))),
        ));
    }
}

fn reset_at(now: DateTime<Utc>, seconds: Option<i64>) -> Option<DateTime<Utc>> {
    let seconds = seconds.filter(|value| *value >= 0)?;
    now.checked_add_signed(ChronoDuration::seconds(seconds))
}

/// Percent consumed, rounded to a tenth and clamped to `0..=100` (§2.4).
fn percent(used: f64, limit: f64) -> f64 {
    if !used.is_finite() || limit <= 0.0 {
        return 0.0;
    }
    let value = (used / limit * 100.0).clamp(0.0, 100.0);
    (value * 10.0).round() / 10.0
}

fn max_zero(value: i64) -> i64 {
    value.max(0)
}

/// `percent` (direct 0–100) or `used/limit`, plus the window's reset instant.
///
/// The live endpoint reports `{ percent, resetsAt, status }`; the documented
/// shape is `{ usagePercent, resetInSec }`. Both are accepted.
fn window(scope: &serde_json::Value, keys: &[&str], now: DateTime<Utc>) -> Option<ApiWindow> {
    let dict = keys
        .iter()
        .find_map(|key| scope.get(*key))
        .filter(|value| value.is_object())?;

    let percent = number_from(
        dict,
        &[
            "usagePercent",
            "usedPercent",
            "percentUsed",
            "percent",
            "usage_percent",
            "used_percent",
            "utilization",
            "utilizationPercent",
        ],
    )
    .or_else(|| {
        let used = number_from(dict, &["used", "usage", "consumed", "count"])?;
        let limit = number_from(dict, &["limit", "total", "quota", "max", "cap"])?;
        (limit > 0.0).then_some(used / limit * 100.0)
    })?;

    let reset_in_sec = [
        "resetInSec",
        "resetInSeconds",
        "resetSeconds",
        "reset_sec",
        "reset_in_sec",
        "resetsInSec",
        "resetsInSeconds",
        "resetSec",
    ]
    .iter()
    .find_map(|key| as_i64(dict.get(*key)));

    // An absolute reset instant wins over nothing; `resetInSec` is the fallback.
    let resets_at = absolute_from(
        dict,
        &[
            "resetsAt",
            "resetAt",
            "reset_at",
            "resets_at",
            "nextReset",
            "next_reset",
            "renewAt",
            "renew_at",
        ],
    )
    .or_else(|| reset_at(now, reset_in_sec));

    Some(ApiWindow {
        percent: percent.clamp(0.0, 100.0),
        resets_at,
    })
}

/// First key that parses as an absolute instant (epoch seconds/millis or RFC 3339).
fn absolute_from(dict: &serde_json::Value, keys: &[&str]) -> Option<DateTime<Utc>> {
    keys.iter()
        .find_map(|key| dict.get(*key))
        .and_then(date_value)
}

fn date_value(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    if let Some(number) = value.as_f64() {
        return epoch_to_datetime(number);
    }
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(text.trim()) {
            return Some(parsed.with_timezone(&Utc));
        }
        if let Ok(number) = text.trim().parse::<f64>() {
            return epoch_to_datetime(number);
        }
    }
    None
}

fn epoch_to_datetime(number: f64) -> Option<DateTime<Utc>> {
    if !number.is_finite() {
        return None;
    }
    let millis = if number > 1_000_000_000_000.0 {
        number as i64
    } else if number > 1_000_000_000.0 {
        (number * 1000.0) as i64
    } else {
        return None;
    };
    Utc.timestamp_millis_opt(millis).single()
}

fn number_from(dict: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| dict.get(*key).and_then(serde_json::Value::as_f64))
        .filter(|value| value.is_finite())
}

fn as_i64(value: Option<&serde_json::Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
}

/// Start of the ISO week (Monday 00:00 UTC), matching the Swift reader's
/// `firstWeekday = 2, minimumDaysInFirstWeek = 4`.
fn start_of_utc_week(now: DateTime<Utc>) -> DateTime<Utc> {
    let date = now.date_naive();
    let iso = date.iso_week();
    let monday = chrono::NaiveDate::from_isoywd_opt(iso.year(), iso.week(), chrono::Weekday::Mon)
        .unwrap_or(date);
    Utc.with_ymd_and_hms(monday.year(), monday.month(), monday.day(), 0, 0, 0)
        .single()
        .unwrap_or_else(|| {
            now - ChronoDuration::seconds(i64::from(now.time().num_seconds_from_midnight()))
        })
}

/// Monthly bounds anchored at the earliest local row's day-of-month/time, in UTC
/// (§2.4 — the estimate can drift from the real billing cycle).
fn month_bounds(now: DateTime<Utc>, anchor_ms: Option<i64>) -> (i64, i64) {
    let anchor = anchor_ms
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .unwrap_or(now);

    let (mut year, mut month) = (now.year(), now.month());
    let mut start = anchored_month(year, month, anchor);
    if start > now {
        let (py, pm) = previous_month(year, month);
        year = py;
        month = pm;
        start = anchored_month(year, month, anchor);
    }
    let (ny, nm) = next_month(year, month);
    let end = anchored_month(ny, nm, anchor);
    (start.timestamp_millis(), end.timestamp_millis())
}

fn anchored_month(year: i32, month: u32, anchor: DateTime<Utc>) -> DateTime<Utc> {
    let day = anchor.day().min(last_day_of_month(year, month)).max(1);
    Utc.with_ymd_and_hms(
        year,
        month,
        day,
        anchor.hour(),
        anchor.minute(),
        anchor.second(),
    )
    .single()
    .unwrap_or(anchor)
}

fn last_day_of_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = next_month(year, month);
    let first_next = chrono::NaiveDate::from_ymd_opt(ny, nm, 1)
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap());
    (first_next - ChronoDuration::days(1)).day()
}

fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

fn previous_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

/// One `opencode-go` cost row read out of `opencode.db`.
#[derive(Debug, Clone, PartialEq)]
struct UsageRow {
    created_ms: i64,
    cost: f64,
}

const MESSAGE_USAGE_SQL: &str = "
    SELECT
      CAST(COALESCE(json_extract(data, '$.time.created'), time_created) AS INTEGER) AS createdMs,
      CAST(json_extract(data, '$.cost') AS REAL) AS cost
    FROM message
    WHERE json_valid(data)
      AND json_extract(data, '$.providerID') = 'opencode-go'
      AND json_extract(data, '$.role') = 'assistant'
      AND json_type(data, '$.cost') IN ('integer', 'real')
";

const MESSAGE_AND_PART_USAGE_SQL: &str = "
    WITH provider_messages AS (
      SELECT
        id AS messageID,
        CAST(COALESCE(json_extract(data, '$.time.created'), time_created) AS INTEGER) AS createdMs,
        CAST(json_extract(data, '$.cost') AS REAL) AS cost,
        json_type(data, '$.cost') IN ('integer', 'real') AS hasCost
      FROM message
      WHERE json_valid(data)
        AND json_extract(data, '$.providerID') = 'opencode-go'
        AND json_extract(data, '$.role') = 'assistant'
    )
    SELECT
      CAST(COALESCE(json_extract(p.data, '$.time.created'), p.time_created, m.createdMs) AS INTEGER)
        AS createdMs,
      CAST(json_extract(p.data, '$.cost') AS REAL) AS cost
    FROM part p
    JOIN provider_messages m ON m.messageID = p.message_id
    WHERE json_valid(p.data)
      AND json_extract(p.data, '$.type') = 'step-finish'
      AND json_type(p.data, '$.cost') IN ('integer', 'real')
    UNION ALL
    SELECT createdMs, cost
    FROM provider_messages m
    WHERE hasCost
      AND NOT EXISTS (
        SELECT 1
        FROM part p
        WHERE p.message_id = m.messageID
          AND json_valid(p.data)
          AND json_extract(p.data, '$.type') = 'step-finish'
          AND json_type(p.data, '$.cost') IN ('integer', 'real')
      )
";

/// Open the database, read the rows, close it — read-only throughout.
///
/// **No sidecar is ever created.** Opening a WAL-mode database normally (even
/// read-only) makes SQLite recreate `-wal`/`-shm`; that is a write next to the
/// user's OpenCode state, so a database with no active WAL is read with
/// `?immutable=1` instead and only a database that already has its sidecars is
/// opened the normal way.
fn read_rows(database: &Path) -> Result<Vec<UsageRow>, String> {
    let immutable = sidecars_missing(database);
    open_readonly(database, immutable).and_then(|connection| collect_rows(&connection))
}

fn open_readonly(database: &Path, immutable: bool) -> Result<rusqlite::Connection, String> {
    use rusqlite::OpenFlags;

    let (target, flags) = if immutable {
        (
            format!("file:{}?immutable=1", uri_path(database)),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
    } else {
        (
            database.to_string_lossy().into_owned(),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
    };

    let connection = rusqlite::Connection::open_with_flags(target, flags)
        .map_err(|error| format!("could not open {} read-only: {error}", database.display()))?;
    // Read-only handles still honour a busy timeout for a concurrent writer.
    let _ = connection.busy_timeout(SQLITE_BUSY_TIMEOUT);
    Ok(connection)
}

/// A file URI needs forward slashes; MSYS/native Windows paths both normalise here.
fn uri_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn sidecars_missing(database: &Path) -> bool {
    let wal = PathBuf::from(format!("{}-wal", database.display()));
    let shm = PathBuf::from(format!("{}-shm", database.display()));
    !wal.exists() && !shm.exists()
}

fn collect_rows(connection: &rusqlite::Connection) -> Result<Vec<UsageRow>, String> {
    let sql = if has_table(connection, "part")? {
        MESSAGE_AND_PART_USAGE_SQL
    } else {
        MESSAGE_USAGE_SQL
    };

    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("could not prepare the usage query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(UsageRow {
                created_ms: row.get(0)?,
                cost: row.get(1)?,
            })
        })
        .map_err(|error| format!("could not run the usage query: {error}"))?;

    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(|error| format!("could not read a usage row: {error}"))?;
        if row.created_ms > 0 && row.cost.is_finite() && row.cost >= 0.0 {
            out.push(row);
        }
    }
    Ok(out)
}

fn has_table(connection: &rusqlite::Connection, name: &str) -> Result<bool, String> {
    let mut statement = connection
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1")
        .map_err(|error| format!("could not inspect the schema: {error}"))?;
    statement
        .exists([name])
        .map_err(|error| format!("could not inspect the schema: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instant() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
    }

    #[test]
    fn api_usage_reads_rolling_weekly_and_monthly() {
        let body = serde_json::json!({
            "usage": {
                "rolling": { "usagePercent": 35.5, "resetInSec": 3600 },
                "weekly": { "usagePercent": 20.0, "resetInSec": 86400 },
                "monthly": { "usagePercent": 5.0, "resetInSec": 172800 }
            }
        });
        let usage = ApiUsage::from_json(&body, instant()).unwrap();
        assert_eq!(usage.rolling.percent, 35.5);
        assert_eq!(
            usage.rolling.resets_at,
            Some(instant() + ChronoDuration::seconds(3600))
        );
        assert_eq!(usage.weekly.unwrap().percent, 20.0);
        assert_eq!(usage.monthly.unwrap().percent, 5.0);
    }

    #[test]
    fn api_usage_accepts_the_live_percent_and_resets_at_shape() {
        // What `GET /zen/go/v1/usage` actually returns on this machine.
        let body = serde_json::json!({
            "usage": {
                "rolling": { "percent": 19.0, "resetsAt": "2026-09-10T22:00:00.000Z", "status": "ok" },
                "weekly": { "percent": 29.0, "resetsAt": "2026-09-14T00:00:00.000Z", "status": "ok" },
                "monthly": { "percent": 67.0, "resetsAt": 1780000000000i64, "status": "ok" }
            }
        });
        let usage = ApiUsage::from_json(&body, instant()).unwrap();
        assert_eq!(usage.rolling.percent, 19.0);
        assert_eq!(
            usage.rolling.resets_at,
            Some(Utc.with_ymd_and_hms(2026, 9, 10, 22, 0, 0).unwrap())
        );
        assert_eq!(
            usage.monthly.unwrap().resets_at,
            Some(
                Utc.timestamp_millis_opt(1_780_000_000_000)
                    .single()
                    .unwrap()
            )
        );
    }

    #[test]
    fn api_usage_requires_a_rolling_window() {
        assert!(ApiUsage::from_json(&serde_json::json!({"usage": {}}), instant()).is_none());
        assert!(ApiUsage::from_json(&serde_json::json!({}), instant()).is_none());
    }

    #[test]
    fn api_usage_clamps_and_computes_used_over_limit() {
        let body = serde_json::json!({
            "usage": { "rolling": { "used": 25.0, "limit": 50.0, "resetInSec": 10 } }
        });
        let usage = ApiUsage::from_json(&body, instant()).unwrap();
        assert_eq!(usage.rolling.percent, 50.0);
        let over = serde_json::json!({
            "usage": { "rolling": { "usagePercent": 250.0 } }
        });
        assert_eq!(
            ApiUsage::from_json(&over, instant())
                .unwrap()
                .rolling
                .percent,
            100.0
        );
    }

    #[test]
    fn local_percent_is_rounded_and_clamped() {
        assert_eq!(percent(6.0, 12.0), 50.0);
        assert_eq!(percent(0.0, 12.0), 0.0);
        assert_eq!(percent(30.0, 12.0), 100.0);
        assert_eq!(percent(1.0, 3.0), 33.3);
        assert_eq!(percent(1.0, 0.0), 0.0);
    }

    #[test]
    fn monthly_bounds_are_anchored_at_the_oldest_row() {
        // Same day/time as the anchor, previous cycle rolled over.
        let now = Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap();
        let anchor = Utc.with_ymd_and_hms(2026, 8, 5, 9, 30, 0).unwrap();
        let (start, end) = month_bounds(now, Some(anchor.timestamp_millis()));
        assert_eq!(
            start,
            Utc.with_ymd_and_hms(2026, 9, 5, 9, 30, 0)
                .unwrap()
                .timestamp_millis()
        );
        assert_eq!(
            end,
            Utc.with_ymd_and_hms(2026, 10, 5, 9, 30, 0)
                .unwrap()
                .timestamp_millis()
        );
    }

    #[test]
    fn month_bounds_absorb_a_short_target_month() {
        // The anchor day (31) does not exist in September, so the current-month
        // start clamps to the 30th instead of spilling into October.
        let now = Utc.with_ymd_and_hms(2026, 9, 30, 20, 0, 0).unwrap();
        let anchor = Utc.with_ymd_and_hms(2026, 8, 31, 12, 0, 0).unwrap();
        let (start, end) = month_bounds(now, Some(anchor.timestamp_millis()));
        assert_eq!(
            start,
            Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0)
                .unwrap()
                .timestamp_millis()
        );
        assert_eq!(
            end,
            Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0)
                .unwrap()
                .timestamp_millis()
        );
    }

    #[test]
    fn week_starts_on_monday_at_midnight_utc() {
        // 2026-09-10 is a Thursday; the ISO week starts on Monday 2026-09-07.
        let start = start_of_utc_week(instant());
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 9, 7, 0, 0, 0).unwrap());
    }

    #[test]
    fn debug_never_prints_the_token() {
        let env = Env::empty().with(ENV_API_KEY, "opencode-secret-token-value");
        let provider =
            OpenCodeGo::with_client(Arc::new(crate::testing::FixtureClient::empty()), env);
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("opencode-secret-token-value"));
    }
}
