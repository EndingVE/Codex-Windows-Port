//! **Cursor** — reads Cursor's own local session and cursor.com's usage API.
//!
//! Data sources, in the order the port uses them (`SPEC-flagship.md` §4):
//!
//! 1. **Cursor's VS Code global-state DB** (`state.vscdb`) — the `ItemTable` row
//!    `cursorAuth/accessToken`. On Windows that file lives at
//!    `%APPDATA%\Cursor\User\globalStorage\state.vscdb`.
//! 2. **A cookie header from the port's own config** (`providers[].cookieHeader`
//!    for `cursor`, or `CURSOR_COOKIE_HEADER`). Covers the case where Cursor is
//!    not installed but the user pasted a `WorkosCursorSessionToken`.
//!
//! Nothing here ever writes. The DB is read through [`sqlite::Database`], a
//! deliberately tiny **read-only** SQLite page reader that never opens SQLite,
//! never takes a lock and **never creates a `-wal`/`-shm` file** in Cursor's
//! directory — the reason the Swift original falls back to `immutable=1` at all.
//! We always read the main file plus, when present, the committed frames of its
//! `-wal` sidecar; that is the strictest form of "read normally, but leave the
//! directory byte-for-byte as we found it".
//!
//! Mapping (from the Swift `CursorStatusSnapshot.toUsageSnapshot`):
//!
//! | Lane | Source |
//! | --- | --- |
//! | plan (primary) | `individualUsage.plan.totalPercentUsed` → avg(`autoPercentUsed`,`apiPercentUsed`) → either lane → `plan.used/limit` → `overall` → `teamUsage.pooled` |
//! | cursor models | `individualUsage.plan.autoPercentUsed` |
//! | third-party API | `individualUsage.plan.apiPercentUsed` |
//! | extra | `get-sand-usage-status` (`usagePercent`) when the account has a non-zero Bot allowance |
//!
//! A legacy request-based plan (`/api/usage?user=…` with a positive
//! `maxRequestUsage`) replaces the plan lane with the request ratio and hides the
//! token-based model lanes, exactly as the Swift does.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use codexbar_core::{
    DataSource, FetchStatus, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    WindowKind,
};
use serde_json::Value;

use crate::credential::{appdata_dir, cleaned, display_path, Env, PortConfig, Secret};
use crate::http::{HttpClient, HttpError, HttpRequest};

/// Optional DB-path override. Documented port affordance: lets a test (or a user
/// with a non-standard install) point at a `state.vscdb` without faking `%APPDATA%`.
pub const ENV_STATE_DB: &str = "CURSOR_STATE_DB";
/// Cookie-header fallback when Cursor is not installed locally.
pub const ENV_COOKIE_HEADER: &str = "CURSOR_COOKIE_HEADER";

/// Fixed host: the quota API is always on cursor.com and is not user-overridable
/// (a cookie must never travel to a host the user typed into an env var).
pub const BASE_URL: &str = "https://cursor.com";
const USAGE_SUMMARY_PATH: &str = "/api/usage-summary";
const AUTH_ME_PATH: &str = "/api/auth/me";
const USAGE_PATH: &str = "/api/usage";
const SAND_USAGE_PATH: &str = "/api/dashboard/get-sand-usage-status";

/// Required call: the plan summary.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(12);
/// Best-effort probes must not hold up the 60 s refresh tick.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// The JWT is treated as unusable this long before it actually expires, so a
/// refresh cycle never races the server (`SPEC-flagship.md` §4.1: > 60 s).
const JWT_SKEW_SECONDS: i64 = 60;

pub struct Cursor {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl Cursor {
    /// Production constructor: real HTTPS client, process environment, the port's
    /// own config file when the user has one.
    pub fn new() -> Self {
        let env = Env::from_process();
        let config = PortConfig::load(&env).unwrap_or(None);
        Self {
            client: crate::http::shared_client(),
            env,
            config,
        }
    }

    /// Tests: fixture client, injected environment, no disk discovery beyond the
    /// paths the injected environment names.
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

    /// `%APPDATA%\Cursor\User\globalStorage\state.vscdb`, or [`ENV_STATE_DB`].
    fn db_path(&self) -> Option<PathBuf> {
        if let Some(override_path) = self.env.get_str(ENV_STATE_DB) {
            return Some(PathBuf::from(override_path));
        }
        appdata_dir().map(|dir| {
            dir.join("Cursor")
                .join("User")
                .join("globalStorage")
                .join("state.vscdb")
        })
    }

    /// Resolve a usable session: local Cursor DB first, then a configured cookie.
    ///
    /// The Swift treats the app session as authoritative *when it is usable*; a
    /// configured cookie is the documented manual override, so it is consulted
    /// when Cursor is absent or its stored token cannot be used.
    fn session(&self, now: DateTime<Utc>) -> Result<Session, SessionError> {
        let mut local_problem: Option<SessionError> = None;

        if let Some(db_path) = self.db_path().filter(|path| path.is_file()) {
            match sqlite::Database::open_readonly(&db_path)
                .and_then(|db| db.value("ItemTable", "cursorAuth/accessToken"))
            {
                Ok(Some(bytes)) => {
                    match Session::from_access_token(&decode_sqlite_value(&bytes), now) {
                        Ok(session) => return Ok(session),
                        Err(err) => local_problem = Some(err),
                    }
                }
                Ok(None) => local_problem = Some(SessionError::NoToken(db_path)),
                Err(err) => local_problem = Some(SessionError::Database(err.to_string())),
            }
        }

        if let Some(cookie) = self.configured_cookie() {
            return Ok(Session::from_cookie(&cookie));
        }

        Err(local_problem.unwrap_or(SessionError::NotInstalled))
    }

    /// A manual cookie header from config or env (`cookieHeader` field).
    fn configured_cookie(&self) -> Option<String> {
        let from_config = self
            .config
            .as_ref()
            .and_then(|config| config.field(ProviderId::Cursor, "cookieHeader"));
        let raw = from_config.or_else(|| self.env.get_str(ENV_COOKIE_HEADER))?;
        cleaned(&raw).filter(|value| !value.is_empty())
    }

    fn cookie_request(&self, path: &str, session: &Session) -> HttpRequest {
        let mut request = HttpRequest::get(format!("{BASE_URL}{path}"))
            .accept_json()
            .header("Cookie", session.cookie.clone());
        // cursor.com's API rejects requests with a mismatched Origin (CSRF).
        request = request.header("Origin", BASE_URL);
        request
    }
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cursor")
            .field("db_path", &self.db_path().map(|p| display_path(&p)))
            .field("has_config_cookie", &self.configured_cookie().is_some())
            .finish()
    }
}

/// Why no session could be resolved. Every message is safe to show to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// Neither `%APPDATA%\Cursor` nor a configured cookie exists.
    NotInstalled,
    /// The DB exists but has no `cursorAuth/accessToken` row.
    NoToken(PathBuf),
    /// The row exists but the token is absent/blank/expired/malformed.
    UnusableToken(String),
    /// The DB could not be read.
    Database(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::NotInstalled => write!(
                f,
                "No Cursor session found — install and sign in to Cursor, or set \
                 providers[].cookieHeader for \"cursor\" (or CURSOR_COOKIE_HEADER)."
            ),
            SessionError::NoToken(path) => write!(
                f,
                "Found {} but it has no cursorAuth/accessToken row — sign in to Cursor.",
                display_path(path)
            ),
            SessionError::UnusableToken(reason) => {
                write!(f, "Cursor session is not usable: {reason}")
            }
            SessionError::Database(reason) => {
                write!(f, "Could not read Cursor's state database: {reason}")
            }
        }
    }
}

/// A resolved Cursor session: the cookie header to send plus whatever identity we
/// could extract from it.
#[derive(Debug, Clone)]
pub struct Session {
    pub cookie: String,
    pub subject: Option<String>,
    pub email: Option<String>,
    pub source: DataSource,
}

impl Session {
    /// Local Cursor.app session: a JWT that must be more than 60 s from expiry.
    fn from_access_token(token: &str, now: DateTime<Utc>) -> Result<Self, SessionError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(SessionError::UnusableToken(
                "the stored token is empty".into(),
            ));
        }
        let claims = JwtClaims::parse(token)
            .ok_or_else(|| SessionError::UnusableToken("the stored token is not a JWT".into()))?;
        let user_id = claims
            .user_id()
            .ok_or_else(|| SessionError::UnusableToken("the JWT has no usable user id".into()))?;
        let exp = claims
            .exp
            .ok_or_else(|| SessionError::UnusableToken("the JWT has no expiry".into()))?;
        if exp <= now.timestamp() + JWT_SKEW_SECONDS {
            return Err(SessionError::UnusableToken(
                "the stored token has expired — reopen Cursor to refresh it".into(),
            ));
        }
        Ok(Self {
            // Mirrors the Swift `cookieHeader()`: `WorkosCursorSessionToken=<id>%3A%3A<token>`.
            cookie: format!("WorkosCursorSessionToken={user_id}%3A%3A{token}"),
            subject: Some(user_id),
            email: claims.email,
            source: DataSource::OAuth,
        })
    }

    /// A user-supplied cookie header: forwarded as-is, never parsed for a token.
    fn from_cookie(cookie: &str) -> Self {
        Self {
            cookie: cookie.to_string(),
            subject: None,
            email: None,
            source: DataSource::Web,
        }
    }
}

impl Provider for Cursor {
    fn id(&self) -> ProviderId {
        ProviderId::Cursor
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::Cursor,
            title: ProviderId::Cursor.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::OAuth,
            fetched_at: now,
        };

        let session = match self.session(now) {
            Ok(session) => session,
            Err(err) => {
                snapshot.status = FetchStatus::NotConfigured;
                snapshot.error = Some(err.to_string());
                return snapshot;
            }
        };
        snapshot.source = session.source;
        snapshot.account = session.email.clone().or_else(|| session.subject.clone());

        // 1. Required: the plan summary.
        let summary = match self.usage_summary(&session) {
            Ok(summary) => summary,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(err.message);
                return snapshot;
            }
        };

        // 2. Best-effort identity; a failure keeps the JWT-derived identity.
        if let Ok(user) = self.user_info(&session) {
            if let Some(email) = user.email {
                // The address is contactable PII: the card shows it masked.
                snapshot.account = Some(crate::credential::mask_email(&email));
            }
            if let Some(name) = user.name {
                snapshot.plan.get_or_insert(name);
            }
        }
        snapshot.plan = summary
            .membership_type
            .as_deref()
            .map(format_membership_type)
            .or(snapshot.plan);

        let mut notes: Vec<String> = Vec::new();

        // 3. Best-effort legacy request quota; only meaningful with a positive limit.
        let requests = match self.request_usage(&session) {
            Ok(requests) => requests,
            Err(err) => {
                notes.push(format!(
                    "request quota unavailable right now ({})",
                    err.kind.as_str()
                ));
                None
            }
        };

        // 4. Best-effort Grok Bot weekly allowance; hidden when the account has none.
        let sand = match self.sand_usage(&session) {
            Ok(sand) => sand,
            Err(err) => {
                notes.push(format!(
                    "Grok Bot allowance unavailable right now ({})",
                    err.kind.as_str()
                ));
                None
            }
        };

        map_summary(&mut snapshot, &summary, requests, sand, now);

        if snapshot.windows.is_empty() {
            notes.push(
                "Cursor reported no plan usage for this account — nothing to meter yet".to_string(),
            );
        }
        if !notes.is_empty() {
            // Status stays `ok` when we did get a summary; the notes are diagnostic.
            snapshot.error = Some(notes.join("; "));
        }

        snapshot
    }
}

/// Everything the mapping step needs, parsed with `Option` at every level so a
/// schema change upstream degrades instead of failing the whole card.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSummary {
    pub membership_type: Option<String>,
    pub billing_cycle_start: Option<String>,
    pub billing_cycle_end: Option<String>,
    pub plan: Option<PlanUsage>,
    pub on_demand: Option<Cents>,
    pub team_on_demand: Option<Cents>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanUsage {
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub auto_percent_used: Option<f64>,
    pub api_percent_used: Option<f64>,
    pub total_percent_used: Option<f64>,
    pub overall: Option<Cents>,
    pub pooled: Option<Cents>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cents {
    pub used: f64,
    pub limit: Option<f64>,
}

impl Cents {
    fn from_json(value: &Value) -> Option<Self> {
        let used = number(value, "used")?;
        Some(Self {
            used,
            limit: number(value, "limit"),
        })
    }
}

impl UsageSummary {
    pub fn from_json(body: &Value) -> Self {
        let individual = body.get("individualUsage");
        let plan = individual.and_then(|i| i.get("plan"));
        // `plan` is always present so the `overall`/`pooled` fallbacks below run
        // even for enterprise accounts that omit the `plan` block entirely.
        let plan = PlanUsage {
            used: plan.and_then(|p| number(p, "used")),
            limit: plan.and_then(|p| number(p, "limit")),
            auto_percent_used: plan.and_then(|p| number(p, "autoPercentUsed")),
            api_percent_used: plan.and_then(|p| number(p, "apiPercentUsed")),
            total_percent_used: plan.and_then(|p| number(p, "totalPercentUsed")),
            overall: individual
                .and_then(|i| i.get("overall"))
                .and_then(Cents::from_json),
            pooled: body
                .get("teamUsage")
                .and_then(|t| t.get("pooled"))
                .and_then(Cents::from_json),
        };
        Self {
            membership_type: string(body, "membershipType"),
            billing_cycle_start: string(body, "billingCycleStart"),
            billing_cycle_end: string(body, "billingCycleEnd"),
            plan: Some(plan),
            on_demand: individual
                .and_then(|i| i.get("onDemand"))
                .and_then(Cents::from_json),
            team_on_demand: body
                .get("teamUsage")
                .and_then(|t| t.get("onDemand"))
                .and_then(Cents::from_json),
        }
    }

    /// Headline percent, following the Swift precedence exactly. `None` means the
    /// account reported nothing we can honestly turn into a percentage.
    pub fn headline_percent(&self) -> Option<f64> {
        let plan = self.plan.as_ref()?;
        if let Some(total) = plan.total_percent_used {
            return Some(clamp_pct(total));
        }
        let auto = plan.auto_percent_used.map(clamp_pct);
        let api = plan.api_percent_used.map(clamp_pct);
        match (auto, api) {
            (Some(a), Some(b)) => Some(clamp_pct((a + b) / 2.0)),
            (None, Some(b)) => Some(b),
            (Some(a), None) => Some(a),
            (None, None) => ratio(plan.used, plan.limit)
                .or_else(|| plan.overall.and_then(|o| ratio(Some(o.used), o.limit)))
                .or_else(|| plan.pooled.and_then(|p| ratio(Some(p.used), p.limit))),
        }
    }
}

/// A legacy `/api/usage?user=` response, reduced to the two numbers we meter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RequestUsage {
    pub used: f64,
    pub limit: f64,
}

impl RequestUsage {
    pub fn from_json(body: &Value) -> Option<Self> {
        let gpt4 = body.get("gpt-4")?;
        let limit = number(gpt4, "maxRequestUsage").filter(|l| *l > 0.0)?;
        let used = number(gpt4, "numRequestsTotal").or_else(|| number(gpt4, "numRequests"))?;
        Some(Self { used, limit })
    }

    fn used_percent(self) -> f64 {
        clamp_pct(self.used / self.limit * 100.0)
    }
}

/// The Grok Bot weekly allowance (`get-sand-usage-status`). This is the only
/// field set that is not part of the Swift struct name we mirror, so it is parsed
/// generically.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SandUsage {
    pub usage_percent: f64,
    pub has_non_zero_included_limit: bool,
}

impl SandUsage {
    pub fn from_json(body: &Value) -> Option<Self> {
        Some(Self {
            usage_percent: number(body, "usagePercent")?,
            has_non_zero_included_limit: body
                .get("hasNonZeroIncludedLimit")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Identity from `/api/auth/me`.
#[derive(Debug, Clone, Default)]
pub struct UserInfo {
    pub email: Option<String>,
    pub name: Option<String>,
}

impl Cursor {
    fn usage_summary(&self, session: &Session) -> Result<UsageSummary, HttpError> {
        let request = self
            .cookie_request(USAGE_SUMMARY_PATH, session)
            .timeout(SUMMARY_TIMEOUT);
        let response = self.client.send_ok(&request)?;
        let body: Value = response.json()?;
        Ok(UsageSummary::from_json(&body))
    }

    fn user_info(&self, session: &Session) -> Result<UserInfo, HttpError> {
        let request = self
            .cookie_request(AUTH_ME_PATH, session)
            .timeout(PROBE_TIMEOUT);
        let response = self.client.send_ok(&request)?;
        let body: Value = response.json()?;
        Ok(UserInfo {
            email: string(&body, "email"),
            name: string(&body, "name"),
        })
    }

    /// `GET /api/usage?user=<id>` — only reachable when the session carries an id.
    fn request_usage(&self, session: &Session) -> Result<Option<RequestUsage>, HttpError> {
        let Some(user_id) = session.subject.as_deref() else {
            return Ok(None);
        };
        let mut request = self.cookie_request(USAGE_PATH, session);
        request.url = format!("{BASE_URL}{USAGE_PATH}?user={user_id}");
        let request = request.timeout(PROBE_TIMEOUT);
        let response = self.client.send_ok(&request)?;
        let body: Value = response.json()?;
        Ok(RequestUsage::from_json(&body))
    }

    fn sand_usage(&self, session: &Session) -> Result<Option<SandUsage>, HttpError> {
        let request = HttpRequest::post(format!("{BASE_URL}{SAND_USAGE_PATH}"))
            .accept_json()
            .header("Cookie", session.cookie.clone())
            .header("Origin", BASE_URL)
            .timeout(PROBE_TIMEOUT);
        let request = request.json_body(&serde_json::json!({}))?;
        let response = self.client.send_ok(&request)?;
        let body: Value = response.json()?;
        Ok(SandUsage::from_json(&body))
    }
}

/// Map a parsed summary onto the snapshot's lanes.
pub fn map_summary(
    snapshot: &mut ProviderSnapshot,
    summary: &UsageSummary,
    requests: Option<RequestUsage>,
    sand: Option<SandUsage>,
    _now: DateTime<Utc>,
) {
    let window_minutes = cycle_minutes(
        summary.billing_cycle_start.as_deref(),
        summary.billing_cycle_end.as_deref(),
    );
    let resets_at = summary
        .billing_cycle_end
        .as_deref()
        .and_then(parse_timestamp);

    // A legacy request plan replaces the plan lane and hides the token lanes.
    if let Some(requests) = requests {
        snapshot.windows.push(NamedRateWindow::new(
            "cursor-requests",
            "Requests · monthly",
            WindowKind::Weekly,
            RateWindow::new(requests.used_percent(), window_minutes, resets_at),
        ));
    } else {
        if let Some(percent) = summary.headline_percent() {
            snapshot.windows.push(NamedRateWindow::new(
                "cursor-plan",
                "Plan · monthly",
                WindowKind::Weekly,
                RateWindow::new(percent, window_minutes, resets_at),
            ));
        }
        if let Some(plan) = &summary.plan {
            if let Some(auto) = plan.auto_percent_used {
                snapshot.windows.push(NamedRateWindow::new(
                    "cursor-auto",
                    "Cursor models · monthly",
                    WindowKind::WeeklyScoped,
                    RateWindow::new(clamp_pct(auto), window_minutes, resets_at),
                ));
            }
            if let Some(api) = plan.api_percent_used {
                snapshot.windows.push(NamedRateWindow::new(
                    "cursor-api",
                    "Third-party API · monthly",
                    WindowKind::WeeklyScoped,
                    RateWindow::new(clamp_pct(api), window_minutes, resets_at),
                ));
            }
        }
    }

    // On-demand spend only gets a meter when there is a real cap.
    let on_demand = summary.on_demand.or(summary.team_on_demand);
    if let Some(cap) = on_demand {
        if let Some(percent) = ratio(Some(cap.used), cap.limit) {
            snapshot.windows.push(NamedRateWindow::new(
                "cursor-on-demand",
                "On-demand · monthly",
                WindowKind::Extra,
                RateWindow::new(percent, window_minutes, resets_at),
            ));
        }
    }

    // Grok Bot weekly allowance, only for accounts that have one.
    if let Some(sand) = sand {
        if sand.has_non_zero_included_limit {
            snapshot.windows.push(NamedRateWindow::new(
                "cursor-grok-bot",
                "Grok Bot · weekly",
                WindowKind::Extra,
                RateWindow::new(clamp_pct(sand.usage_percent), Some(10_080), None),
            ));
        }
    }
}

/// `Cursor Pro`, `Cursor Ultra`, … — mirrors the Swift `formatMembershipType`.
pub fn format_membership_type(raw: &str) -> String {
    let name = match raw.to_ascii_lowercase().as_str() {
        "enterprise" => "Enterprise",
        "express" => "Start",
        "free" => "Free",
        "free_trial" => "Pro Trial",
        "hobby" => "Hobby",
        "pro" | "pro_student" => "Pro",
        "pro_plus" => "Pro+",
        "team" => "Team",
        "ultra" => "Ultra",
        _ => raw,
    };
    format!("Cursor {name}")
}

/// JWT payload claims we use. The signature is never verified (see the Swift).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JwtClaims {
    pub sub: Option<String>,
    pub email: Option<String>,
    pub exp: Option<i64>,
}

impl JwtClaims {
    pub fn parse(jwt: &str) -> Option<Self> {
        let mut parts = jwt.split('.');
        parts.next()?;
        let payload = parts.next()?;
        let json: Value = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
        Some(Self {
            sub: string(&json, "sub"),
            email: string(&json, "email"),
            exp: json.get("exp").and_then(Value::as_i64),
        })
    }

    /// The trailing segment of `sub` (`auth0|user_abc123` → `user_abc123`),
    /// validated to the same charset the Swift allows.
    pub fn user_id(&self) -> Option<String> {
        let sub = self.sub.as_deref()?;
        let last = sub.split('|').rfind(|s| !s.is_empty())?;
        if last.is_empty()
            || !last
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return None;
        }
        Some(last.to_string())
    }
}

/// The Swift BLOB rule: recognize BOM-less ASCII UTF-16LE before trying UTF-8,
/// which would otherwise keep interleaved NUL bytes.
pub fn decode_sqlite_value(bytes: &[u8]) -> String {
    if bytes.len() % 2 == 0
        && !bytes.is_empty()
        && bytes
            .chunks_exact(2)
            .all(|pair| (1..128).contains(&pair[0]) && pair[1] == 0)
    {
        if let Some(decoded) = decode_utf16le(bytes) {
            if !decoded.trim().is_empty() {
                return decoded;
            }
        }
    }
    match String::from_utf8(bytes.to_vec()) {
        Ok(text) => text,
        Err(_) => decode_utf16le(bytes).unwrap_or_default(),
    }
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// base64url decoder (`-`/`_` alphabet, optional padding), no dependency.
pub fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    const VALUES: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = VALUES.iter().position(|c| *c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

fn clamp_pct(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 100.0)
}

/// `used / limit * 100` as a percentage, or `None` when there is no positive limit.
fn ratio(used: Option<f64>, limit: Option<f64>) -> Option<f64> {
    let limit = limit.filter(|l| l.is_finite() && *l > 0.0)?;
    let used = used.filter(|u| u.is_finite() && *u >= 0.0)?;
    Some(clamp_pct(used / limit * 100.0))
}

fn number(object: &Value, key: &str) -> Option<f64> {
    object
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn string(object: &Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn cycle_minutes(start: Option<&str>, end: Option<&str>) -> Option<i64> {
    let start = parse_timestamp(start?)?;
    let end = parse_timestamp(end?)?;
    let minutes = (end - start).num_minutes();
    (minutes > 0).then_some(minutes)
}

/// Expose a `Secret` as a cookie header value only where a request is built.
#[allow(dead_code)]
fn secret_header(secret: &Secret) -> String {
    secret.expose().to_string()
}

// ---------------------------------------------------------------------------
// A read-only SQLite page reader.
//
// This is deliberately not a SQL engine. It understands exactly what Cursor's
// `state.vscdb` needs: the `ItemTable` (key, value) b-tree, table-leaf and
// interior pages, the record format, and committed WAL frames. It opens no
// connection, takes no lock and creates no file — so reading Cursor's global
// state can never mutate Cursor's directory.
// ---------------------------------------------------------------------------
pub mod sqlite {
    use std::collections::HashMap;
    use std::fmt;
    use std::path::Path;

    /// Why a read failed. No path or value ever appears in the message.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum SqliteError {
        /// The file is missing or empty.
        Missing,
        /// The header is not a SQLite database.
        NotADatabase,
        /// The page size in the header is impossible.
        BadPageSize(usize),
        /// An I/O error while reading the file.
        Io(String),
        /// The b-tree structure could not be walked.
        Corrupt(String),
    }

    impl fmt::Display for SqliteError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                SqliteError::Missing => write!(f, "the file is missing"),
                SqliteError::NotADatabase => write!(f, "not a SQLite database"),
                SqliteError::BadPageSize(size) => write!(f, "invalid page size {size}"),
                SqliteError::Io(reason) => write!(f, "{reason}"),
                SqliteError::Corrupt(reason) => write!(f, "corrupt b-tree ({reason})"),
            }
        }
    }

    const HEADER: &[u8; 16] = b"SQLite format 3\0";
    const TABLE_LEAF: u8 = 0x0d;
    const TABLE_INTERIOR: u8 = 0x05;

    /// A parsed cell value.
    #[derive(Debug, Clone, PartialEq)]
    enum Value {
        Null,
        Integer(i64),
        Float(f64),
        Text(String),
        Blob(Vec<u8>),
    }

    /// An in-memory, read-only view of one database file plus its WAL overlay.
    pub struct Database {
        main: Vec<u8>,
        overlay: HashMap<u32, Vec<u8>>,
        page_size: usize,
        usable_size: usize,
        wal_active: bool,
    }

    impl fmt::Debug for Database {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Database")
                .field("page_size", &self.page_size)
                .field("wal_active", &self.wal_active)
                .field("wal_frames", &self.overlay.len())
                .finish()
        }
    }

    impl Database {
        /// Read `path` (and its `-wal` sidecar when present) into memory.
        ///
        /// Never creates a file, never locks. When no `-wal`/`-shm` sidecar exists
        /// this behaves like SQLite's `immutable=1`; when a WAL exists its
        /// committed frames are applied on top of the main file, which is what
        /// "read normally under an active WAL" means without side effects.
        pub fn open_readonly(path: &Path) -> Result<Self, SqliteError> {
            let main = std::fs::read(path).map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    SqliteError::Missing
                } else {
                    SqliteError::Io(err.to_string())
                }
            })?;
            if main.len() < 100 {
                return Err(SqliteError::Missing);
            }
            if &main[..16] != HEADER {
                return Err(SqliteError::NotADatabase);
            }
            let page_size = match u16::from_be_bytes([main[16], main[17]]) {
                1 => 65_536,
                size if size.is_power_of_two() && size >= 512 => size as usize,
                other => return Err(SqliteError::BadPageSize(other as usize)),
            };
            let reserved = main[20] as usize;
            if reserved >= page_size {
                return Err(SqliteError::BadPageSize(page_size - reserved));
            }
            let usable_size = page_size - reserved;

            let wal_path = sidecar(path, "-wal");
            let (overlay, wal_active) = match std::fs::read(&wal_path) {
                Ok(bytes) if !bytes.is_empty() => (committed_frames(&bytes, page_size), true),
                _ => (HashMap::new(), false),
            };

            Ok(Self {
                main,
                overlay,
                page_size,
                usable_size,
                wal_active,
            })
        }

        /// `true` when a non-empty `-wal` sidecar contributed frames.
        pub fn wal_active(&self) -> bool {
            self.wal_active
        }

        /// `SELECT value FROM <table> WHERE key = ? LIMIT 1`, returning the raw
        /// bytes of the first matching row.
        pub fn value(&self, table: &str, key: &str) -> Result<Option<Vec<u8>>, SqliteError> {
            let root = self.table_root(table)?;
            let Some(root) = root else {
                return Ok(None);
            };
            let mut found: Option<Vec<u8>> = None;
            self.walk_table(root, &mut |payload| {
                let Some(record) = self.parse_record(payload) else {
                    return false;
                };
                if record.first().and_then(Value::as_text) == Some(key) {
                    found = record.get(1).and_then(Value::as_bytes);
                    return true;
                }
                false
            })?;
            Ok(found)
        }

        fn page(&self, pgno: u32) -> Option<&[u8]> {
            if let Some(page) = self.overlay.get(&pgno) {
                return Some(page.as_slice());
            }
            let index = pgno.checked_sub(1)? as usize;
            let start = index.checked_mul(self.page_size)?;
            let end = start.checked_add(self.page_size)?;
            self.main.get(start..end)
        }

        /// Root page of `name` in `sqlite_schema` (page 1).
        fn table_root(&self, name: &str) -> Result<Option<u32>, SqliteError> {
            let mut root = None;
            self.walk_table(1, &mut |payload| {
                let Some(record) = self.parse_record(payload) else {
                    return false;
                };
                let kind = record.first().and_then(Value::as_text);
                let table = record.get(1).and_then(Value::as_text);
                if kind == Some("table") && table == Some(name) {
                    root = record.get(3).and_then(Value::as_integer).map(|v| v as u32);
                    return true;
                }
                false
            })?;
            Ok(root)
        }

        /// Depth-first walk of a table b-tree, calling `visit` with each leaf
        /// cell's payload. Stops early when `visit` returns `true`.
        fn walk_table(
            &self,
            pgno: u32,
            visit: &mut dyn FnMut(&[u8]) -> bool,
        ) -> Result<(), SqliteError> {
            let page = self
                .page(pgno)
                .ok_or_else(|| SqliteError::Corrupt(format!("page {pgno} out of range")))?;
            let header = if pgno == 1 { 100 } else { 0 };
            let page_type = *page
                .get(header)
                .ok_or_else(|| SqliteError::Corrupt("truncated page header".into()))?;
            let cell_count = read_u16(page, header + 3)
                .ok_or_else(|| SqliteError::Corrupt("no cell count".into()))?
                as usize;
            let pointers = header + 8;

            match page_type {
                TABLE_LEAF => {
                    for index in 0..cell_count {
                        let offset = read_u16(page, pointers + index * 2)
                            .ok_or_else(|| SqliteError::Corrupt("bad cell pointer".into()))?
                            as usize;
                        let Some(cell) = page.get(offset..) else {
                            continue;
                        };
                        let mut cursor = 0usize;
                        let Some(payload_len) = read_varint(cell, &mut cursor) else {
                            continue;
                        };
                        let Some(_rowid) = read_varint(cell, &mut cursor) else {
                            continue;
                        };
                        if payload_len as usize > self.usable_size - 35 {
                            // Overflow chain: not a row this reader needs (tokens
                            // are small), so skip it rather than mis-parse.
                            continue;
                        }
                        let Some(payload) = cell.get(cursor..cursor + payload_len as usize) else {
                            continue;
                        };
                        if visit(payload) {
                            return Ok(());
                        }
                    }
                }
                TABLE_INTERIOR => {
                    for index in 0..cell_count {
                        let offset = read_u16(page, pointers + index * 2)
                            .ok_or_else(|| SqliteError::Corrupt("bad cell pointer".into()))?
                            as usize;
                        let child = read_u32(page, offset)
                            .ok_or_else(|| SqliteError::Corrupt("bad interior cell".into()))?;
                        self.walk_table(child, visit)?;
                    }
                    let right = read_u32(page, pointers)
                        .ok_or_else(|| SqliteError::Corrupt("no right-most pointer".into()))?;
                    self.walk_table(right, visit)?;
                }
                other => {
                    return Err(SqliteError::Corrupt(format!(
                        "unexpected page type {other:#x}"
                    )));
                }
            }
            Ok(())
        }

        fn parse_record(&self, payload: &[u8]) -> Option<Vec<Value>> {
            let mut cursor = 0usize;
            let header_size = read_varint(payload, &mut cursor)? as usize;
            if header_size < cursor || header_size > payload.len() {
                return None;
            }
            let mut serials = Vec::new();
            while cursor < header_size {
                serials.push(read_varint(payload, &mut cursor)?);
            }
            let mut body = header_size;
            let mut values = Vec::with_capacity(serials.len());
            for serial in serials {
                let (value, length) = read_value(payload, body, serial)?;
                body += length;
                values.push(value);
            }
            Some(values)
        }
    }

    impl Value {
        fn as_text(&self) -> Option<&str> {
            match self {
                Value::Text(text) => Some(text.as_str()),
                _ => None,
            }
        }

        fn as_integer(&self) -> Option<i64> {
            match self {
                Value::Integer(value) => Some(*value),
                _ => None,
            }
        }

        fn as_bytes(&self) -> Option<Vec<u8>> {
            match self {
                Value::Blob(bytes) => Some(bytes.clone()),
                Value::Text(text) => Some(text.as_bytes().to_vec()),
                _ => None,
            }
        }
    }

    /// Read one record value, returning it plus its byte length in the payload.
    fn read_value(payload: &[u8], offset: usize, serial: u64) -> Option<(Value, usize)> {
        let slice = payload.get(offset..)?;
        match serial {
            0 => Some((Value::Null, 0)),
            1..=6 => {
                // Serial types 1..=6 are 1, 2, 3, 4, 6 and 8 bytes of big-endian
                // two's-complement integer.
                let length = [0usize, 1, 2, 3, 4, 6, 8][serial as usize];
                let bytes = slice.get(..length)?;
                let mut raw: u64 = 0;
                for byte in bytes {
                    raw = (raw << 8) | u64::from(*byte);
                }
                let value = if length == 8 {
                    raw as i64
                } else {
                    let shift = 64 - length * 8;
                    ((raw << shift) as i64) >> shift
                };
                Some((Value::Integer(value), length))
            }
            7 => {
                let bytes = slice.get(..8)?;
                let mut array = [0u8; 8];
                array.copy_from_slice(bytes);
                Some((Value::Float(f64::from_be_bytes(array)), 8))
            }
            8 => Some((Value::Integer(0), 0)),
            9 => Some((Value::Integer(1), 0)),
            10 | 11 => Some((Value::Null, 0)),
            even if even % 2 == 0 => {
                let length = (even as usize - 12) / 2;
                Some((Value::Blob(slice.get(..length)?.to_vec()), length))
            }
            odd => {
                let length = (odd as usize - 13) / 2;
                let text = String::from_utf8_lossy(slice.get(..length)?).into_owned();
                Some((Value::Text(text), length))
            }
        }
    }

    /// Committed WAL frames, keyed by page number. Frames after the last commit
    /// record are ignored, and frames from a previous WAL generation (different
    /// salt) are rejected.
    fn committed_frames(wal: &[u8], page_size: usize) -> HashMap<u32, Vec<u8>> {
        let mut overlay = HashMap::new();
        if wal.len() < 32 {
            return overlay;
        }
        let magic = read_u32(wal, 0).unwrap_or(0);
        if magic != 0x377f_0682 && magic != 0x377f_0683 {
            return overlay;
        }
        if read_u32(wal, 8).map(|size| size as usize) != Some(page_size) {
            return overlay;
        }
        let (salt1, salt2) = (read_u32(wal, 16), read_u32(wal, 20));
        let frame_size = 24 + page_size;

        let mut committed = HashMap::new();
        let mut pending = HashMap::new();
        let mut offset = 32usize;
        while offset + frame_size <= wal.len() {
            let page_no = read_u32(wal, offset).unwrap_or(0);
            let db_size = read_u32(wal, offset + 4).unwrap_or(0);
            let frame_salt1 = read_u32(wal, offset + 8);
            let frame_salt2 = read_u32(wal, offset + 12);
            if frame_salt1 != salt1 || frame_salt2 != salt2 {
                break; // a later, unrelated WAL generation
            }
            if page_no == 0 {
                break;
            }
            let start = offset + 24;
            if let Some(page) = wal.get(start..start + page_size) {
                pending.insert(page_no, page.to_vec());
            }
            if db_size != 0 {
                committed = pending.clone();
            }
            offset += frame_size;
        }
        overlay = committed;
        overlay
    }

    fn sidecar(path: &Path, suffix: &str) -> std::path::PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        std::path::PathBuf::from(name)
    }

    fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
        let slice = bytes.get(offset..offset + 2)?;
        Some(u16::from_be_bytes([slice[0], slice[1]]))
    }

    fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
        let slice = bytes.get(offset..offset + 4)?;
        Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn read_varint(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
        let mut result: u64 = 0;
        for _ in 0..8 {
            let byte = *bytes.get(*cursor)?;
            *cursor += 1;
            result = (result << 7) | u64::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                return Some(result);
            }
        }
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        Some((result << 8) | u64::from(byte))
    }

    /// Kept private to the module but exercised by the unit tests below.
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn varints_follow_the_sqlite_encoding() {
            let mut cursor = 0usize;
            assert_eq!(read_varint(&[0x00], &mut cursor), Some(0));
            let mut cursor = 0usize;
            assert_eq!(read_varint(&[0x7f], &mut cursor), Some(127));
            let mut cursor = 0usize;
            assert_eq!(read_varint(&[0x81, 0x00], &mut cursor), Some(128));
        }

        #[test]
        fn truncated_input_is_never_a_panic() {
            let mut cursor = 0usize;
            assert_eq!(read_varint(&[], &mut cursor), None);
            assert_eq!(read_u16(&[0x01], 0), None);
            assert_eq!(read_u32(&[0x01, 0x02], 0), None);
        }

        #[test]
        fn a_wal_from_another_generation_is_ignored() {
            let page_size = 512;
            let mut wal = vec![0u8; 32];
            wal[..4].copy_from_slice(&0x377f_0682u32.to_be_bytes());
            wal[8..12].copy_from_slice(&(page_size as u32).to_be_bytes());
            wal[16..20].copy_from_slice(&7u32.to_be_bytes());
            wal[20..24].copy_from_slice(&9u32.to_be_bytes());
            assert!(committed_frames(&wal, page_size).is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only base64url encoder, so the fixtures need no dependency.
    fn base64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[(n >> 6) as usize & 63] as char);
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[n as usize & 63] as char);
            }
        }
        out
    }

    fn jwt(payload: &str) -> String {
        let encoded = base64url(payload.as_bytes());
        format!("header.{encoded}.sig")
    }

    #[test]
    fn access_token_becomes_the_documented_cookie() {
        let token = jwt(r#"{"sub":"auth0|user_abc123","email":"a@b.co","exp":2000000000}"#);
        let now = DateTime::parse_from_rfc3339("2026-09-10T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let session = Session::from_access_token(&token, now).unwrap();
        assert_eq!(
            session.cookie,
            format!("WorkosCursorSessionToken=user_abc123%3A%3A{token}")
        );
        assert_eq!(session.subject.as_deref(), Some("user_abc123"));
        assert_eq!(session.email.as_deref(), Some("a@b.co"));
    }

    #[test]
    fn an_expired_or_shapeless_token_is_refused_with_a_reason() {
        let now = DateTime::parse_from_rfc3339("2026-09-10T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let expired = jwt(r#"{"sub":"auth0|u1","exp":1600000000}"#);
        assert!(matches!(
            Session::from_access_token(&expired, now),
            Err(SessionError::UnusableToken(_))
        ));
        assert!(matches!(
            Session::from_access_token("not-a-jwt", now),
            Err(SessionError::UnusableToken(_))
        ));
        assert!(matches!(
            Session::from_access_token("", now),
            Err(SessionError::UnusableToken(_))
        ));
    }

    #[test]
    fn headline_precedence_matches_the_swift() {
        let total = UsageSummary {
            plan: Some(PlanUsage {
                total_percent_used: Some(58.25),
                auto_percent_used: Some(61.5),
                api_percent_used: Some(12.0),
                ..PlanUsage::default()
            }),
            ..UsageSummary::default()
        };
        assert_eq!(total.headline_percent(), Some(58.25));

        let averaged = UsageSummary {
            plan: Some(PlanUsage {
                auto_percent_used: Some(60.0),
                api_percent_used: Some(20.0),
                ..PlanUsage::default()
            }),
            ..UsageSummary::default()
        };
        assert_eq!(averaged.headline_percent(), Some(40.0));

        let ratio_only = UsageSummary {
            plan: Some(PlanUsage {
                used: Some(250.0),
                limit: Some(1000.0),
                ..PlanUsage::default()
            }),
            ..UsageSummary::default()
        };
        assert_eq!(ratio_only.headline_percent(), Some(25.0));

        let nothing = UsageSummary {
            plan: Some(PlanUsage::default()),
            ..UsageSummary::default()
        };
        assert_eq!(nothing.headline_percent(), None);
    }

    #[test]
    fn membership_types_are_named_like_the_app() {
        assert_eq!(format_membership_type("pro"), "Cursor Pro");
        assert_eq!(format_membership_type("pro_plus"), "Cursor Pro+");
        assert_eq!(format_membership_type("free_trial"), "Cursor Pro Trial");
        assert_eq!(format_membership_type("mystery"), "Cursor mystery");
    }

    #[test]
    fn blob_decoding_recognises_utf16le_before_utf8() {
        let utf16: Vec<u8> = "token".bytes().flat_map(|b| [b, 0]).collect();
        assert_eq!(decode_sqlite_value(&utf16), "token");
        assert_eq!(decode_sqlite_value(b"plain-ascii"), "plain-ascii");
        assert_eq!(decode_sqlite_value(&[]), "");
    }

    #[test]
    fn request_usage_needs_a_positive_limit() {
        let body = serde_json::json!({"gpt-4": {"numRequestsTotal": 120, "maxRequestUsage": 500}});
        let usage = RequestUsage::from_json(&body).unwrap();
        assert_eq!(usage.used_percent(), 24.0);
        assert!(
            RequestUsage::from_json(&serde_json::json!({"gpt-4": {"numRequestsTotal": 120}}))
                .is_none()
        );
    }
}
