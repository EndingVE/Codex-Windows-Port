//! **Groq** — console spend/usage through a browser session (Stytch), with an
//! optional Enterprise Prometheus fallback.
//!
//! Structured exactly like [`crate::OpenRouter`] (the reference provider):
//!
//! ```text
//! pub struct Groq { client: Arc<dyn HttpClient>, env: Env, config: Option<PortConfig> }
//! ```
//!
//! Behaviour implemented here, from `SPEC-apikey.md` §8 and `repo/docs/groq.md`:
//!
//! | Spec | Where |
//! | --- | --- |
//! | Session from `GROQ_SESSION_TOKEN` (opaque) / `GROQ_SESSION_JWT` (direct) / `cookieHeader` | [`Groq::session`] |
//! | Stytch B2B exchange `POST {stytch}/sdk/v1/b2b/sessions/authenticate` | [`Groq::refresh_session_jwt`] |
//! | `GROQ_STYTCH_PUBLIC_TOKEN` / `GROQ_STYTCH_URL` overrides | [`Groq::stytch_url`] |
//! | Activity `GET {host}/platform/v1/organizations/{org}/activity?start_date&end_date` | [`Groq::console_windows`] |
//! | `orgId` from the JWT `https://groq.com/organization` claim (routing only) | [`organization_id_from_jwt`] |
//! | Prometheus `GET {base}/metrics/prometheus/api/v1/query` (`GROQ_API_KEY`, Enterprise) | [`Groq::prometheus_windows`] |
//! | `GROQ_API_URL` must be HTTPS, fail closed | [`secure_base_url`] |
//!
//! Groq reports **daily spend/tokens**, not a quota with a limit. The lanes this
//! provider publishes therefore carry the numbers in their `reset_description`
//! and set `usage_known: false`, so the tray/UI never renders a fabricated
//! percentage (the same honesty rule the frozen contract states for a lane whose
//! usage number is not real).
//!
//! Deliberately **not** implemented (documented, not guessed): Chrome/Edge cookie
//! import (`groq.com` / `console.groq.com`). This crate has no browser reader, so
//! the session arrives through the documented env overrides or a manual
//! `cookieHeader`; the code is explicit about which source it used.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use codexbar_core::{
    DataSource, FetchStatus, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    WindowKind,
};

use crate::credential::{Env, PortConfig, Secret};
use crate::http::{secure_base_url, HttpClient, HttpError, HttpErrorKind, HttpRequest};

/// Enterprise API key env var (Prometheus fallback).
pub const ENV_API_KEY: &str = "GROQ_API_KEY";
/// Opaque console session cookie, exercises the Stytch refresh.
pub const ENV_SESSION_TOKEN: &str = "GROQ_SESSION_TOKEN";
/// Console session JWT used directly (skips the refresh).
pub const ENV_SESSION_JWT: &str = "GROQ_SESSION_JWT";
/// Stytch publishable-token override (Groq rotation).
pub const ENV_STYTCH_PUBLIC_TOKEN: &str = "GROQ_STYTCH_PUBLIC_TOKEN";
/// Stytch base URL override (HTTPS only).
pub const ENV_STYTCH_URL: &str = "GROQ_STYTCH_URL";
/// API base URL override (HTTPS only).
pub const ENV_API_URL: &str = "GROQ_API_URL";

/// Default API base.
pub const DEFAULT_API_URL: &str = "https://api.groq.com/v1";
/// Default Stytch B2B base.
pub const DEFAULT_STYTCH_URL: &str = "https://api.stytchb2b.groq.com";
/// Groq's Stytch **publishable** token (public by design; overridable).
pub const DEFAULT_STYTCH_PUBLIC_TOKEN: &str =
    "public-token-live-58df57a9-a1f5-4066-bc0c-2ff942db684f";

const ENV_ALIASES: [&str; 1] = [ENV_API_KEY];

const CONSOLE_ORIGIN: &str = "https://console.groq.com";
const SDK_VERSION: &str = "5.43.0";
/// Console activity rows are per-model, per-day; the chart spans 30 days.
const HISTORY_DAYS: i64 = 30;

const STYTCH_TIMEOUT: Duration = Duration::from_secs(20);
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(20);
const METRICS_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Groq {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl Groq {
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
            Some(config) => config.resolve_api_key(ProviderId::Groq, &self.env, &ENV_ALIASES),
            None => self.env.first_of(&ENV_ALIASES),
        }
    }

    /// Resolve a console session from the env overrides, then a manual
    /// `providers[].cookieHeader`.
    fn session(&self) -> Option<Session> {
        let env_token = self.env.get_str(ENV_SESSION_TOKEN);
        let env_jwt = self.env.get_str(ENV_SESSION_JWT);
        let from_env = Session::from_parts(env_token, env_jwt, "env");
        if from_env.is_some() {
            return from_env;
        }
        let header = self
            .config
            .as_ref()
            .and_then(|c| c.field(ProviderId::Groq, "cookieHeader"))?;
        Session::from_parts(
            cookie_value(&header, "stytch_session"),
            cookie_value(&header, "stytch_session_jwt"),
            "manual",
        )
    }

    fn stytch_url(&self) -> Result<String, String> {
        secure_base_url(
            self.env.get_str(ENV_STYTCH_URL).as_deref(),
            DEFAULT_STYTCH_URL,
        )
    }

    fn stytch_public_token(&self) -> String {
        self.env
            .get_str(ENV_STYTCH_PUBLIC_TOKEN)
            .filter(|token| !token.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_STYTCH_PUBLIC_TOKEN.to_string())
    }

    fn api_base(&self) -> Result<String, String> {
        secure_base_url(self.env.get_str(ENV_API_URL).as_deref(), DEFAULT_API_URL)
    }

    /// Exchange the opaque session cookie for a fresh short-lived JWT.
    fn refresh_session_jwt(&self, session_token: &str) -> Result<String, HttpError> {
        let base = self.stytch_url().map_err(|reason| {
            HttpError::invalid_request(format!("GROQ_STYTCH_URL is not usable: {reason}"))
        })?;
        let public_token = self.stytch_public_token();
        // Stytch SDK auth: Basic base64(publicToken:sessionToken).
        let credential = base64_encode(format!("{public_token}:{session_token}").as_bytes());

        let body = serde_json::json!({
            "session_token": session_token,
            "session_duration_minutes": 30,
        });
        let request = HttpRequest::post(format!("{base}/sdk/v1/b2b/sessions/authenticate"))
            .authorization(format!("Basic {credential}"))
            .header("Content-Type", "application/json")
            .header("Origin", CONSOLE_ORIGIN)
            .header("X-SDK-Parent-Host", CONSOLE_ORIGIN)
            .header("X-SDK-Client", sdk_client_header())
            .json_body(&body)?
            .timeout(STYTCH_TIMEOUT);

        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        body.pointer("/data/session_jwt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|jwt| !jwt.is_empty())
            .map(str::to_string)
            .ok_or_else(|| HttpError::decode("Stytch response had no data.session_jwt"))
    }

    /// Console activity → daily buckets → one `daily` and one `activity` lane.
    fn console_windows(
        &self,
        jwt: &str,
        now: DateTime<Utc>,
    ) -> Result<(Option<String>, Vec<NamedRateWindow>, String), HttpError> {
        let org = organization_id_from_jwt(jwt).ok_or_else(|| {
            HttpError::decode("Groq session token is missing the organization claim")
        })?;

        let base = self.api_base().map_err(|reason| {
            HttpError::invalid_request(format!("GROQ_API_URL is not usable: {reason}"))
        })?;
        let host = scheme_and_authority(&base);
        let (start, end) = activity_window(now);
        let url = format!(
            "{host}/platform/v1/organizations/{org}/activity?start_date={start}&end_date={end}"
        );

        let request = HttpRequest::get(url)
            .bearer(&Secret::new(jwt.to_string()))
            .accept_json()
            .timeout(ACTIVITY_TIMEOUT);

        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        let rows = body
            .get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| HttpError::decode("Groq activity response had no data[] array"))?;

        let activity = Activity::aggregate(rows, now);
        let mut windows = Vec::new();
        if activity.rows > 0 {
            if activity.today.requests > 0 || activity.today.tokens > 0 || activity.today.cost > 0.0
            {
                windows.push(lane(
                    "daily",
                    "Daily · tokens",
                    Some(1_440),
                    format!(
                        "Today: {} tokens · {} · {} requests",
                        group_digits(activity.today.tokens),
                        usd(activity.today.cost),
                        group_digits(activity.today.requests)
                    ),
                ));
            }
            windows.push(lane(
                "activity",
                "Activity · 30d",
                Some(43_200),
                format!(
                    "Last {HISTORY_DAYS} days: {} tokens · {} · {} requests",
                    group_digits(activity.total.tokens),
                    usd(activity.total.cost),
                    group_digits(activity.total.requests)
                ),
            ));
        }

        Ok((activity.organization_name, windows, String::new()))
    }

    /// Enterprise Prometheus fallback: `rate5m` request/token/cache series.
    fn prometheus_windows(&self, api_key: &Secret) -> Result<Vec<NamedRateWindow>, HttpError> {
        let base = self.api_base().map_err(|reason| {
            HttpError::invalid_request(format!("GROQ_API_URL is not usable: {reason}"))
        })?;
        let url = format!("{base}/metrics/prometheus/api/v1/query");

        let requests = self.prometheus_query(
            &url,
            "sum(model_project_id_status_code:requests:rate5m)",
            api_key,
        )?;
        let tokens_in =
            self.prometheus_query(&url, "sum(model_project_id:tokens_in:rate5m)", api_key)?;
        let tokens_out =
            self.prometheus_query(&url, "sum(model_project_id:tokens_out:rate5m)", api_key)?;
        let cache_hits = self.prometheus_query(
            &url,
            "sum(model_project_id:prompt_cache_hits:rate5m)",
            api_key,
        )?;

        let mut windows = vec![
            lane(
                "requests",
                "Requests · 5m",
                Some(5),
                format!("{} req/min", decimal(requests * 60.0)),
            ),
            lane(
                "tokens",
                "Tokens · 5m",
                Some(5),
                format!("{} tok/min", decimal((tokens_in + tokens_out) * 60.0)),
            ),
        ];
        if cache_hits > 0.0 {
            windows.push(lane(
                "cache",
                "Cache hits · 5m",
                Some(5),
                format!("{} cache/min", decimal(cache_hits * 60.0)),
            ));
        }
        Ok(windows)
    }

    fn prometheus_query(&self, url: &str, query: &str, api_key: &Secret) -> Result<f64, HttpError> {
        let request = HttpRequest::get(format!("{url}?query={query}"))
            .bearer(api_key)
            .accept_json()
            .timeout(METRICS_TIMEOUT);
        let response = self.client.send_ok(&request)?;
        let body: serde_json::Value = response.json()?;
        parse_prometheus_scalar(&body)
    }
}

impl Default for Groq {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Groq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Groq")
            .field("has_api_key", &self.api_key().is_some())
            .field("has_session", &self.session().is_some())
            .field("config", &self.config.as_ref().map(PortConfig::source_path))
            .finish()
    }
}

impl Provider for Groq {
    fn id(&self) -> ProviderId {
        ProviderId::Groq
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::Groq,
            title: ProviderId::Groq.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::Web,
            fetched_at: now,
        };

        let session = self.session();
        let api_key = self.api_key();

        // No credential of either kind is `notConfigured`, not `error`.
        if session.is_none() && api_key.is_none() {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error = Some(
                "No Groq credential found — set GROQ_SESSION_TOKEN (or GROQ_SESSION_JWT) for the \
                 console activity API, or GROQ_API_KEY for Enterprise Prometheus metrics, or add \
                 providers[].apiKey for \"groq\" to %APPDATA%\\CodexBar\\config.json."
                    .to_string(),
            );
            return snapshot;
        }

        // Console (web) is preferred; Prometheus is the fallback.
        if let Some(session) = session {
            snapshot.source = DataSource::Web;
            let jwt = match session.resolve_jwt(self) {
                Ok(jwt) => jwt,
                Err(err) => {
                    if let Some(key) = &api_key {
                        // A dead session still leaves the Enterprise path open.
                        return self.prometheus_snapshot(snapshot, key);
                    }
                    snapshot.status = FetchStatus::Error;
                    snapshot.error = Some(err.message);
                    return snapshot;
                }
            };
            let (organization, windows, _) = match self.console_windows(&jwt, now) {
                Ok(result) => result,
                Err(err) => {
                    snapshot.status = FetchStatus::Error;
                    snapshot.error = Some(err.message);
                    return snapshot;
                }
            };
            snapshot.account = organization;
            snapshot.windows = windows;
            return snapshot;
        }

        // Only an API key: Enterprise Prometheus.
        match api_key {
            Some(key) => self.prometheus_snapshot(snapshot, &key),
            None => snapshot,
        }
    }
}

impl Groq {
    fn prometheus_snapshot(
        &self,
        mut snapshot: ProviderSnapshot,
        api_key: &Secret,
    ) -> ProviderSnapshot {
        snapshot.source = DataSource::ApiKey;
        snapshot.account = Some(api_key.redacted());
        match self.prometheus_windows(api_key) {
            Ok(windows) => snapshot.windows = windows,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(err.message);
            }
        }
        snapshot
    }
}

/// A console session: an opaque cookie (refreshed per fetch) and/or a direct JWT.
#[derive(Debug, Clone)]
pub struct Session {
    pub session_token: Option<String>,
    pub direct_jwt: Option<String>,
}

impl Session {
    fn from_parts(token: Option<String>, jwt: Option<String>, _source: &str) -> Option<Self> {
        let clean = |value: Option<String>| {
            value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let session_token = clean(token);
        let direct_jwt = clean(jwt);
        if session_token.is_none() && direct_jwt.is_none() {
            return None;
        }
        Some(Self {
            session_token,
            direct_jwt,
        })
    }

    /// Refresh the opaque token when possible; fall back to a direct JWT.
    fn resolve_jwt(&self, provider: &Groq) -> Result<String, HttpError> {
        if let Some(token) = &self.session_token {
            match provider.refresh_session_jwt(token) {
                Ok(jwt) => return Ok(jwt),
                Err(err) => {
                    if let Some(jwt) = &self.direct_jwt {
                        return Ok(jwt.clone());
                    }
                    return Err(err);
                }
            }
        }
        if let Some(jwt) = &self.direct_jwt {
            return Ok(jwt.clone());
        }
        Err(HttpError::invalid_request(
            "Groq console session is missing",
        ))
    }
}

/// Aggregated console activity: totals over the window plus today's bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DayTotals {
    pub tokens: i64,
    pub requests: i64,
    pub cost: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Activity {
    pub rows: usize,
    pub organization_name: Option<String>,
    pub total: DayTotals,
    pub today: DayTotals,
}

impl Activity {
    pub fn aggregate(rows: &[serde_json::Value], now: DateTime<Utc>) -> Self {
        let mut by_day: BTreeMap<String, DayTotals> = BTreeMap::new();
        let mut organization_name = None;

        for row in rows {
            if organization_name.is_none() {
                organization_name = row
                    .get("organization_name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string);
            }
            let Some(timestamp) = row.get("timestamp").and_then(value_as_f64) else {
                continue;
            };
            let Some(instant) = DateTime::<Utc>::from_timestamp(timestamp as i64, 0) else {
                continue;
            };
            let day_key = instant.format("%Y-%m-%d").to_string();

            let context = row
                .get("n_context_tokens_total")
                .and_then(value_as_i64)
                .unwrap_or(0);
            let non_cached = row
                .get("n_non_cached_context_tokens_total")
                .and_then(value_as_i64)
                .unwrap_or(context);
            let _cached = (context - non_cached).max(0);
            let generated = row
                .get("n_generated_tokens_total")
                .and_then(value_as_i64)
                .unwrap_or(0);

            let bucket = by_day.entry(day_key).or_default();
            bucket.tokens += context + generated;
            bucket.requests += row.get("num_requests").and_then(value_as_i64).unwrap_or(0);
            bucket.cost += row.get("cost").and_then(value_as_f64).unwrap_or(0.0);
        }

        let total = by_day
            .values()
            .fold(DayTotals::default(), |acc, day| DayTotals {
                tokens: acc.tokens + day.tokens,
                requests: acc.requests + day.requests,
                cost: acc.cost + day.cost,
            });
        let today = by_day
            .get(&now.format("%Y-%m-%d").to_string())
            .copied()
            .unwrap_or_default();

        Self {
            rows: rows.len(),
            organization_name,
            total,
            today,
        }
    }
}

// ---------------------------------------------------------------------------
// JWT / query helpers
// ---------------------------------------------------------------------------

/// Read the Groq organization id from the session JWT's
/// `https://groq.com/organization` claim, falling back to Stytch's slug.
///
/// No signature verification — the API authenticates the token; this only reads
/// the routing claim.
pub fn organization_id_from_jwt(jwt: &str) -> Option<String> {
    let mut segments = jwt.split('.');
    let _header = segments.next()?;
    let payload = base64_url_decode(segments.next()?)?;
    let object: serde_json::Value = serde_json::from_slice(&payload).ok()?;

    // The claim keys contain `://`, so a JSON Pointer (`/…/…`) cannot address
    // them — the `/` inside the key would be read as a separator. Use `get`.
    if let Some(id) = object
        .get("https://groq.com/organization")
        .and_then(|org| org.get("id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return Some(id.to_string());
    }
    object
        .get("https://stytch.com/organization")
        .and_then(|org| org.get("slug"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
        .map(str::to_string)
}

/// `(start, end)` unix seconds for the activity window: the full day-span of the
/// last [`HISTORY_DAYS`] days, inclusive of today.
fn activity_window(now: DateTime<Utc>) -> (i64, i64) {
    let start_of_today = now
        .with_hour(0)
        .and_then(|d| d.with_minute(0))
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(now);
    let start = start_of_today - chrono::Duration::days(HISTORY_DAYS - 1);
    let end = start_of_today + chrono::Duration::days(1);
    (start.timestamp(), end.timestamp())
}

/// `https://host[:port]` — the console platform API is rooted at the host, not
/// under the public `/v1` base.
fn scheme_and_authority(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            format!("{scheme}://{authority}")
        }
        None => url.split(['/', '?', '#']).next().unwrap_or("").to_string(),
    }
}

/// Pull one cookie value out of a raw `Cookie:` header string.
fn cookie_value(header: &str, name: &str) -> Option<String> {
    header
        .trim()
        .trim_start_matches("Cookie:")
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_prometheus_scalar(body: &serde_json::Value) -> Result<f64, HttpError> {
    if body.get("status").and_then(|v| v.as_str()) != Some("success") {
        let detail = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("query failed");
        return Err(HttpError::new(HttpErrorKind::Status, detail.to_string()));
    }
    let sum = body
        .pointer("/data/result")
        .and_then(|v| v.as_array())
        .map(|series| {
            series
                .iter()
                .filter_map(|entry| entry.get("value").and_then(|v| v.as_array()))
                .filter_map(|pair| pair.last())
                .filter_map(value_as_f64)
                .sum::<f64>()
        })
        .unwrap_or(0.0);
    Ok(sum)
}

/// A `usage_known: false` extra lane: the number lives in the description, not
/// in a percentage.
fn lane(id: &str, title: &str, minutes: Option<i64>, description: String) -> NamedRateWindow {
    NamedRateWindow::new(
        id,
        title,
        WindowKind::Extra,
        RateWindow {
            reset_description: Some(description),
            ..RateWindow::new(0.0, minutes, None)
        },
    )
    .with_usage_known(false)
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

fn usd(value: f64) -> String {
    format!("${value:.2}")
}

/// `12.34` / `42.5` / `187` — matching the macOS `formatDecimal` buckets.
fn decimal(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
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

// ---------------------------------------------------------------------------
// Minimal base64 (no dependency: the crate deliberately stays lean)
// ---------------------------------------------------------------------------

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(BASE64_ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    let mut normalized: String = input
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    while normalized.len() % 4 != 0 {
        normalized.push('=');
    }

    let mut out = Vec::with_capacity(normalized.len() / 4 * 3);
    for chunk in normalized.as_bytes().chunks(4) {
        if chunk.len() != 4 {
            return None;
        }
        let mut values = [0u8; 4];
        for (index, byte) in chunk.iter().enumerate() {
            values[index] = match byte {
                b'=' => 0,
                other => BASE64_ALPHABET.iter().position(|c| c == other)? as u8,
            };
        }
        let triple = ((values[0] as u32) << 18)
            | ((values[1] as u32) << 12)
            | ((values[2] as u32) << 6)
            | (values[3] as u32);
        out.push((triple >> 16) as u8);
        if chunk[2] != b'=' {
            out.push((triple >> 8) as u8);
        }
        if chunk[3] != b'=' {
            out.push(triple as u8);
        }
    }
    Some(out)
}

/// The base64 telemetry blob the Stytch SDK expects.
fn sdk_client_header() -> String {
    let blob = format!(
        "{{\"app\":{{\"identifier\":\"console.groq.com\"}},\
         \"sdk\":{{\"identifier\":\"Stytch.js Javascript SDK\",\"version\":\"{SDK_VERSION}\"}}}}"
    );
    base64_encode(blob.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_jwt() -> String {
        let header = base64_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64_encode(
            br#"{"sub":"user","https://groq.com/organization":{"id":"org_test123","slug":"acme"}}"#,
        );
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn organization_claim_is_read_from_the_jwt_payload() {
        assert_eq!(
            organization_id_from_jwt(&sample_jwt()).as_deref(),
            Some("org_test123")
        );
    }

    #[test]
    fn organization_falls_back_to_the_stytch_slug() {
        let header = base64_encode(br#"{"alg":"none"}"#);
        let payload = base64_encode(br#"{"https://stytch.com/organization":{"slug":"acme-slug"}}"#);
        let jwt = format!("{header}.{payload}.sig");
        assert_eq!(organization_id_from_jwt(&jwt).as_deref(), Some("acme-slug"));
    }

    #[test]
    fn a_jwt_without_an_organization_claim_has_no_id() {
        let header = base64_encode(br#"{"alg":"none"}"#);
        let payload = base64_encode(br#"{"sub":"user"}"#);
        assert!(organization_id_from_jwt(&format!("{header}.{payload}.sig")).is_none());
        assert!(organization_id_from_jwt("not-a-jwt").is_none());
    }

    #[test]
    fn activity_rows_aggregate_into_daily_buckets() {
        let rows = vec![
            serde_json::json!({
                "organization_name": "Acme Labs", "model": "llama-3.3-70b",
                "timestamp": 1_757_462_400.0, "num_requests": 120,
                "n_context_tokens_total": 10_000, "n_non_cached_context_tokens_total": 6_000,
                "n_generated_tokens_total": 4_000, "cost": 0.12
            }),
            serde_json::json!({
                "organization_name": "Acme Labs", "model": "llama-3.3-70b",
                "timestamp": 1_757_548_800.0, "num_requests": 80,
                "n_context_tokens_total": 5_000, "n_non_cached_context_tokens_total": 5_000,
                "n_generated_tokens_total": 2_000, "cost": 0.08
            }),
            // A row with no timestamp is skipped, not fatal.
            serde_json::json!({"model": "broken", "cost": 9.99}),
        ];
        let now = DateTime::<Utc>::from_timestamp(1_757_548_800, 0).unwrap();
        let activity = Activity::aggregate(&rows, now);
        assert_eq!(activity.organization_name.as_deref(), Some("Acme Labs"));
        assert_eq!(activity.rows, 3);
        assert_eq!(activity.total.tokens, 21_000);
        assert_eq!(activity.total.requests, 200);
        assert!((activity.total.cost - 0.20).abs() < 1e-9);
        assert_eq!(activity.today.tokens, 7_000);
        assert_eq!(activity.today.requests, 80);
        assert!((activity.today.cost - 0.08).abs() < 1e-9);
    }

    #[test]
    fn prometheus_scalars_sum_every_series_and_accept_strings() {
        let body = serde_json::json!({
            "status": "success",
            "data": {"result": [
                {"value": [1_757_548_800.0, "42.5"]},
                {"value": [1_757_548_800.0, 7.5]}
            ]}
        });
        assert!((parse_prometheus_scalar(&body).unwrap() - 50.0).abs() < 1e-9);

        let empty = serde_json::json!({"status": "success", "data": {"result": []}});
        assert_eq!(parse_prometheus_scalar(&empty).unwrap(), 0.0);

        let failed = serde_json::json!({"status": "error", "error": "bad query"});
        let err = parse_prometheus_scalar(&failed).unwrap_err();
        assert!(err.message.contains("bad query"));
    }

    #[test]
    fn base64_round_trips_and_recovers_without_padding() {
        let encoded = base64_encode(b"public-token:session-token");
        assert!(encoded.starts_with("cHVibGlj"));
        assert_eq!(
            base64_url_decode(&encoded).unwrap(),
            b"public-token:session-token"
        );
        // URL-safe, unpadded input decodes the same way.
        let urlsafe = encoded
            .replace('+', "-")
            .replace('/', "_")
            .trim_end_matches('=')
            .to_string();
        assert_eq!(
            base64_url_decode(&urlsafe).unwrap(),
            b"public-token:session-token"
        );
    }

    #[test]
    fn cookie_header_values_are_extracted_case_insensitively() {
        let header = "stytch_session=opaque-value; stytch_session_jwt=jwt-value; other=x";
        assert_eq!(
            cookie_value(header, "stytch_session").as_deref(),
            Some("opaque-value")
        );
        assert_eq!(
            cookie_value(header, "stytch_session_jwt").as_deref(),
            Some("jwt-value")
        );
        assert!(cookie_value(header, "missing").is_none());
    }

    #[test]
    fn host_root_drops_the_public_v1_path() {
        assert_eq!(
            scheme_and_authority("https://api.groq.com/v1"),
            "https://api.groq.com"
        );
        assert_eq!(
            scheme_and_authority("https://proxy.example.com:8443/v1/"),
            "https://proxy.example.com:8443"
        );
    }

    #[test]
    fn activity_window_spans_the_full_history_window() {
        let now = DateTime::<Utc>::from_timestamp(1_757_548_800 + 3600, 0).unwrap();
        let (start, end) = activity_window(now);
        // 30 days inclusive of today.
        assert_eq!(end - start, 30 * 86_400);
        assert_eq!(start % 86_400, 0);
    }
}
