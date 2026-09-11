//! **Gemini** — Gemini CLI's OAuth session and Google's private Cloud Code quota API.
//!
//! Sources (`SPEC-flagship.md` §5, `docs/gemini.md`):
//!
//! * Credentials: `~/.gemini/oauth_creds.json` (Windows:
//!   `%USERPROFILE%\.gemini\oauth_creds.json`), read-only.
//! * Auth type: `~/.gemini/settings.json` → `security.auth.selectedType`.
//!   `oauth-personal` (or unknown) proceeds; API-key and Vertex AI are hard
//!   errors and say so.
//! * OAuth client id/secret: `GEMINI_OAUTH_CLIENT_ID`/`_SECRET`, then
//!   `GEMINI_OAUTH2_JS_PATH`, then the `oauth2.js` of the installed npm package
//!   (Windows: the npm global prefix, `%APPDATA%\npm\node_modules\@google\...`).
//! * Quota + tier: `cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota`
//!   and `:loadCodeAssist`.
//!
//! The port never writes: a token that has expired is refreshed **in memory**
//! through [`crate::oauth::refresh`] and used for this fetch only, exactly
//! because a background tick must not overwrite another tool's credential file.
//!
//! ## Consumer-tier shutdown (2026-06-18)
//!
//! Google retired Gemini CLI OAuth for individual / AI Pro / Ultra accounts. The
//! signals (`UNSUPPORTED_CLIENT`, `IneligibleTierError`, migration copy, or a 403
//! `SUBSCRIPTION_REQUIRED` that follows an unsupported-client `loadCodeAssist`)
//! become an honest [`FetchStatus::Error`] whose message carries the stable token
//! `consumerTierDeprecated` and points at the Antigravity provider — never a
//! generic "HTTP 403". A licensed `standard-tier` account's 403 stays generic.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use codexbar_core::{
    DataSource, FetchStatus, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    WindowKind,
};
use serde_json::{json, Value};

use crate::credential::{cleaned, display_path, home_dir, read_json, Env, PortConfig, Secret};
use crate::http::{HttpClient, HttpRequest};
use crate::oauth::{refresh, RefreshRequest};

/// Client id override.
pub const ENV_CLIENT_ID: &str = "GEMINI_OAUTH_CLIENT_ID";
/// Client secret override.
pub const ENV_CLIENT_SECRET: &str = "GEMINI_OAUTH_CLIENT_SECRET";
/// Explicit `oauth2.js` path.
pub const ENV_OAUTH2_JS_PATH: &str = "GEMINI_OAUTH2_JS_PATH";
/// Credential-file override (port affordance for tests and non-standard homes).
pub const ENV_CREDS_PATH: &str = "GEMINI_OAUTH_CREDS_PATH";
/// Settings-file override.
pub const ENV_SETTINGS_PATH: &str = "GEMINI_SETTINGS_PATH";

pub const QUOTA_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
pub const LOAD_CODE_ASSIST_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
pub const PROJECTS_URL: &str = "https://cloudresourcemanager.googleapis.com/v1/projects";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The stable marker a consumer-tier shutdown always carries in `error`.
pub const CONSUMER_TIER_DEPRECATED: &str = "consumerTierDeprecated";

const QUOTA_TIMEOUT: Duration = Duration::from_secs(12);
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

/// The npm-relative location of the OAuth client constants inside the CLI.
const OAUTH2_JS_SUBPATH: &str =
    "node_modules/@google/gemini-cli-core/dist/src/code_assist/oauth2.js";

pub struct Gemini {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl Gemini {
    pub fn new() -> Self {
        let env = Env::from_process();
        let config = PortConfig::load(&env).unwrap_or(None);
        Self {
            client: crate::http::shared_client(),
            env,
            config,
        }
    }

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

    fn gemini_dir(&self) -> Option<PathBuf> {
        home_dir().map(|home| home.join(".gemini"))
    }

    fn creds_path(&self) -> Option<PathBuf> {
        self.env
            .get_str(ENV_CREDS_PATH)
            .map(PathBuf::from)
            .or_else(|| self.gemini_dir().map(|dir| dir.join("oauth_creds.json")))
    }

    fn settings_path(&self) -> Option<PathBuf> {
        self.env
            .get_str(ENV_SETTINGS_PATH)
            .map(PathBuf::from)
            .or_else(|| self.gemini_dir().map(|dir| dir.join("settings.json")))
    }

    /// Candidate `oauth2.js` locations under the npm global prefix.
    fn oauth2_candidates(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(prefix) = self
            .env
            .get_str("NPM_CONFIG_PREFIX")
            .filter(|value| !value.is_empty())
        {
            roots.push(PathBuf::from(prefix));
        }
        for var in ["APPDATA", "LOCALAPPDATA"] {
            if let Some(dir) = self.env.get_str(var) {
                roots.push(PathBuf::from(dir).join("npm"));
            }
        }
        if let Some(user_profile) = self.env.get_str("USERPROFILE") {
            roots.push(PathBuf::from(user_profile).join("AppData/Roaming/npm"));
        }
        // npm's per-user prefix on POSIX, for parity when this crate is embedded.
        if let Some(home) = home_dir() {
            roots.push(home.join(".npm-global"));
            roots.push(home.join(".local"));
        }

        roots
            .into_iter()
            .map(|root| root.join(OAUTH2_JS_SUBPATH))
            .collect()
    }

    /// Resolve the OAuth client credentials, or `None` when the CLI's install
    /// could not be located. Every source is read-only.
    fn client_credentials(&self) -> Option<ClientCredentials> {
        if let (Some(id), Some(secret)) = (
            self.env.first_of(&[ENV_CLIENT_ID]),
            self.env.first_of(&[ENV_CLIENT_SECRET]),
        ) {
            return Some(ClientCredentials {
                client_id: id.expose().to_string(),
                client_secret: secret.expose().to_string(),
            });
        }

        let mut candidates: Vec<PathBuf> = self
            .env
            .get_str(ENV_OAUTH2_JS_PATH)
            .map(PathBuf::from)
            .into_iter()
            .collect();
        candidates.extend(self.oauth2_candidates());

        for path in candidates {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Some(creds) = ClientCredentials::from_oauth2_js(&content) {
                    return Some(creds);
                }
            }
        }
        None
    }
}

impl Default for Gemini {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Gemini {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gemini")
            .field("creds", &self.creds_path().map(|p| display_path(&p)))
            .field("client_credentials", &self.client_credentials().is_some())
            .finish()
    }
}

/// The Gemini CLI's OAuth client constants, extracted from `oauth2.js`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl ClientCredentials {
    /// `const OAUTH_CLIENT_ID = '…'; const OAUTH_CLIENT_SECRET = '…';`
    pub fn from_oauth2_js(content: &str) -> Option<Self> {
        let client_id = extract_assignment(content, "OAUTH_CLIENT_ID")?;
        let client_secret = extract_assignment(content, "OAUTH_CLIENT_SECRET")?;
        Some(Self {
            client_id,
            client_secret,
        })
    }
}

/// The subset of `oauth_creds.json` this provider needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OAuthCredentials {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expiry: Option<DateTime<Utc>>,
}

impl OAuthCredentials {
    pub fn from_json(body: &Value) -> Self {
        Self {
            access_token: string(body, "access_token"),
            refresh_token: string(body, "refresh_token"),
            id_token: string(body, "id_token"),
            expiry: body
                .get("expiry_date")
                .and_then(Value::as_f64)
                .filter(|ms| ms.is_finite())
                .and_then(|ms| DateTime::from_timestamp_millis(ms as i64)),
        }
    }

    fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        match self.expiry {
            Some(expiry) => expiry <= now,
            None => self.access_token.is_none(),
        }
    }
}

/// `security.auth.selectedType`, normalised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    OAuthPersonal,
    ApiKey,
    VertexAi,
}

impl AuthType {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value) if value == "gemini-api-key" || value == "api-key" => Self::ApiKey,
            Some(value) if value == "vertex-ai" || value == "vertex" => Self::VertexAi,
            // `oauth-personal` and anything we do not recognize both attempt OAuth.
            _ => Self::OAuthPersonal,
        }
    }

    fn from_settings(body: &Value) -> Self {
        Self::parse(
            body.get("security")
                .and_then(|security| security.get("auth"))
                .and_then(|auth| auth.get("selectedType"))
                .and_then(Value::as_str),
        )
    }
}

/// The quota API's answer, reduced to one entry per model (lowest fraction wins).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelQuota {
    pub model_id: String,
    pub percent_left: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

impl ModelQuota {
    fn used_percent(&self) -> f64 {
        clamp_pct(100.0 - self.percent_left)
    }
}

/// Parse `retrieveUserQuota`'s `buckets`, keeping the lowest `remainingFraction`
/// per model (input tokens usually report the constrained number).
pub fn parse_quotas(body: &Value) -> Vec<ModelQuota> {
    let mut per_model: Vec<(String, f64, Option<DateTime<Utc>>)> = Vec::new();
    let Some(buckets) = body.get("buckets").and_then(Value::as_array) else {
        return Vec::new();
    };
    for bucket in buckets {
        let Some(model_id) = string(bucket, "modelId") else {
            continue;
        };
        let Some(fraction) = bucket
            .get("remainingFraction")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        let reset = string(bucket, "resetTime")
            .as_deref()
            .and_then(parse_timestamp);
        match per_model.iter_mut().find(|(id, _, _)| *id == model_id) {
            Some(entry) => {
                if fraction < entry.1 {
                    entry.1 = fraction;
                    entry.2 = reset;
                }
            }
            None => per_model.push((model_id, fraction, reset)),
        }
    }
    per_model.sort_by(|a, b| a.0.cmp(&b.0));
    per_model
        .into_iter()
        .map(|(model_id, fraction, resets_at)| ModelQuota {
            model_id,
            percent_left: clamp_pct(fraction * 100.0),
            resets_at,
        })
        .collect()
}

fn is_flash_lite(model_id: &str) -> bool {
    model_id.to_ascii_lowercase().contains("flash-lite")
}

fn is_flash(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.contains("flash") && !is_flash_lite(model_id)
}

fn is_pro(model_id: &str) -> bool {
    model_id.to_ascii_lowercase().contains("pro")
}

/// `paidTier.name` → `standard-tier` → `free-tier`+`hd` → `free-tier` → `legacy-tier`.
pub fn resolve_plan(
    tier: Option<&str>,
    hosted_domain: Option<&str>,
    paid_tier_name: Option<&str>,
) -> Option<String> {
    if let Some(name) = paid_tier_name.map(str::trim).filter(|n| !n.is_empty()) {
        return Some(name.to_string());
    }
    match (tier, hosted_domain) {
        (Some("standard-tier"), _) => Some("Paid".to_string()),
        (Some("free-tier"), Some(_)) => Some("Workspace".to_string()),
        (Some("free-tier"), None) => Some("Free".to_string()),
        (Some("legacy-tier"), _) => Some("Legacy".to_string()),
        _ => None,
    }
}

/// Google's consumer-shutdown wording, matching the Swift's signal test.
pub fn is_deprecation_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("unsupported_client") || lower.contains("ineligibletiererror") {
        return true;
    }
    if lower.contains("no longer supported") && lower.contains("gemini code assist") {
        return true;
    }
    lower.contains("migrate") && lower.contains("antigravity") && lower.contains("gemini")
}

/// Extract a `NAME = 'value';` (or `"value"`) assignment from JS source.
pub fn extract_assignment(content: &str, name: &str) -> Option<String> {
    let mut search_from = 0usize;
    while let Some(relative) = content[search_from..].find(name) {
        let start = search_from + relative;
        search_from = start + name.len();
        // Word boundary: the byte before must not continue an identifier.
        if content[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let rest = &content[search_from..];
        // Only whitespace may sit between the name and `=`; this rejects
        // `OAUTH_CLIENT_IDX = …` without a fragile substring match.
        let after_name = rest.trim_start();
        let Some(after_equals) = after_name.strip_prefix('=') else {
            continue;
        };
        let after_equals = after_equals.trim_start();
        let mut chars = after_equals.chars();
        let Some(quote @ ('\'' | '"')) = chars.next() else {
            continue;
        };
        let value: String = chars.take_while(|c| *c != quote).collect();
        if !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Some(value);
        }
    }
    None
}

/// `loadCodeAssist` reduced to what the port needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodeAssistStatus {
    pub tier: Option<String>,
    pub project_id: Option<String>,
    pub paid_tier_name: Option<String>,
    pub consumer_client_unsupported: bool,
}

impl CodeAssistStatus {
    pub fn from_json(body: &Value, hosted_domain: Option<&str>) -> Self {
        let project_id = match body.get("cloudaicompanionProject") {
            Some(Value::String(value)) => cleaned(value),
            Some(Value::Object(object)) => object
                .get("id")
                .or_else(|| object.get("projectId"))
                .and_then(Value::as_str)
                .and_then(cleaned),
            _ => None,
        };
        let tier = body
            .get("currentTier")
            .and_then(|tier| tier.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let paid_tier_name = body
            .get("paidTier")
            .and_then(|tier| tier.get("name"))
            .and_then(Value::as_str)
            .and_then(cleaned);
        let listed_unsupported = body
            .get("ineligibleTiers")
            .and_then(Value::as_array)
            .is_some_and(|tiers| {
                tiers.iter().any(|entry| {
                    ["reasonCode", "reasonMessage"]
                        .iter()
                        .filter_map(|key| entry.get(key).and_then(Value::as_str))
                        .any(is_deprecation_signal)
                })
            });
        // A named paid tier or an `hd` claim keeps the account outside the shutdown.
        let consumer_client_unsupported =
            paid_tier_name.is_none() && hosted_domain.is_none() && listed_unsupported;
        Self {
            tier,
            project_id,
            paid_tier_name,
            consumer_client_unsupported,
        }
    }
}

impl Provider for Gemini {
    fn id(&self) -> ProviderId {
        ProviderId::Gemini
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::Gemini,
            title: ProviderId::Gemini.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: DataSource::OAuth,
            fetched_at: now,
        };

        // 1. Auth type is checked first: an unsupported mode is a hard error even
        //    with no credential file present.
        if let Some(settings_path) = self.settings_path() {
            if let Ok(settings) = read_json(&settings_path) {
                match AuthType::from_settings(&settings) {
                    AuthType::ApiKey => {
                        snapshot.status = FetchStatus::Error;
                        snapshot.error = Some(
                            "Gemini API-key auth is not supported — use a Google account (OAuth)."
                                .to_string(),
                        );
                        return snapshot;
                    }
                    AuthType::VertexAi => {
                        snapshot.status = FetchStatus::Error;
                        snapshot.error = Some(
                            "Gemini Vertex AI auth is not supported — use a Google account (OAuth)."
                                .to_string(),
                        );
                        return snapshot;
                    }
                    AuthType::OAuthPersonal => {}
                }
            }
        }

        // 2. Credentials. Missing => `notConfigured` with a setup hint.
        let Some(creds_path) = self.creds_path() else {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.error =
                Some("No Gemini credentials found — run `gemini` once to sign in.".to_string());
            return snapshot;
        };
        let creds = match read_json(&creds_path) {
            Ok(body) => OAuthCredentials::from_json(&body),
            Err(crate::credential::CredentialError::Missing(_)) => {
                snapshot.status = FetchStatus::NotConfigured;
                snapshot.error = Some(format!(
                    "No Gemini credentials at {} — run `gemini` once to sign in.",
                    display_path(&creds_path)
                ));
                return snapshot;
            }
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!("Could not read Gemini credentials: {err}"));
                return snapshot;
            }
        };

        let claims = TokenClaims::from_id_token(creds.id_token.as_deref());
        // The address is contactable PII: the card shows it masked (`user@…`).
        snapshot.account = claims.email.as_deref().map(crate::credential::mask_email);

        // 3. Access token, refreshed in memory when expired (never written back).
        let access_token = match self.access_token(&creds, now) {
            Ok(token) => token,
            Err(FetchFailure::NotConfigured(message)) => {
                snapshot.status = FetchStatus::NotConfigured;
                snapshot.error = Some(message);
                return snapshot;
            }
            Err(FetchFailure::Error(message)) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(message);
                return snapshot;
            }
            Err(FetchFailure::Deprecated) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(deprecation_message());
                return snapshot;
            }
        };

        // 4. Tier + project.
        let status = match self.load_code_assist(&access_token, claims.hosted_domain.as_deref()) {
            Ok(status) => status,
            Err(FetchFailure::Deprecated) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(deprecation_message());
                return snapshot;
            }
            // A `loadCodeAssist` that merely failed is soft: the quota call can
            // still succeed without a project id.
            Err(_) => CodeAssistStatus::default(),
        };

        let project_id = status
            .project_id
            .clone()
            .or_else(|| self.discover_project(&access_token));

        // 5. Quota (required).
        let quotas = match self.user_quota(&access_token, project_id.as_deref()) {
            Ok(quotas) => quotas,
            Err(FetchFailure::NotConfigured(message)) => {
                snapshot.status = FetchStatus::NotConfigured;
                snapshot.error = Some(message);
                return snapshot;
            }
            Err(FetchFailure::Deprecated) if status.consumer_client_unsupported => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(deprecation_message());
                return snapshot;
            }
            Err(FetchFailure::Deprecated) | Err(FetchFailure::Error(_))
                if status.tier.as_deref() == Some("standard-tier") =>
            {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(
                    "Gemini quota request failed (HTTP 403) on a licensed account.".to_string(),
                );
                return snapshot;
            }
            Err(FetchFailure::Error(message)) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(message);
                return snapshot;
            }
            Err(FetchFailure::Deprecated) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(deprecation_message());
                return snapshot;
            }
        };

        snapshot.plan = resolve_plan(
            status.tier.as_deref(),
            claims.hosted_domain.as_deref(),
            status.paid_tier_name.as_deref(),
        );

        map_quotas(&mut snapshot, &quotas, now);

        if snapshot.windows.is_empty() {
            snapshot.status = FetchStatus::Error;
            snapshot.error = Some(
                "Gemini returned no quota buckets — the account has no measurable quota."
                    .to_string(),
            );
        }

        snapshot
    }
}

impl Gemini {
    fn access_token(
        &self,
        creds: &OAuthCredentials,
        now: DateTime<Utc>,
    ) -> Result<Secret, FetchFailure> {
        if let Some(token) = creds.access_token.as_deref() {
            if !creds.is_expired_at(now) {
                return Ok(Secret::new(token));
            }
        }
        let Some(refresh_token) = creds.refresh_token.as_deref() else {
            return Err(FetchFailure::NotConfigured(
                "Gemini access token has expired and there is no refresh token — run `gemini` \
                 again to sign in."
                    .to_string(),
            ));
        };
        let Some(client) = self.client_credentials() else {
            return Err(FetchFailure::NotConfigured(
                "Gemini access token has expired and the CLI's OAuth client could not be located \
                 (set GEMINI_OAUTH_CLIENT_ID/GEMINI_OAUTH_CLIENT_SECRET) — run `gemini` again."
                    .to_string(),
            ));
        };

        let request = RefreshRequest::new(TOKEN_URL, client.client_id, Secret::new(refresh_token))
            .with_client_secret(Secret::new(client.client_secret));
        let refreshed = refresh(self.client.as_ref(), &request, now)
            .map_err(|err| FetchFailure::Error(err.user_message()))?;
        Ok(refreshed.access_token)
    }

    fn load_code_assist(
        &self,
        token: &Secret,
        hosted_domain: Option<&str>,
    ) -> Result<CodeAssistStatus, FetchFailure> {
        let body = json!({"metadata": {"ideType": "GEMINI_CLI", "pluginType": "GEMINI"}});
        let request = HttpRequest::post(LOAD_CODE_ASSIST_URL)
            .bearer(token)
            .accept_json()
            .timeout(PROBE_TIMEOUT);
        let request = request
            .json_body(&body)
            .map_err(|err| FetchFailure::Error(err.message))?;

        let response = match self.client.execute(&request) {
            Ok(response) => response,
            Err(_) => return Ok(CodeAssistStatus::default()),
        };
        if !response.is_success() {
            // A 4xx/5xx body can still carry the shutdown wording.
            if is_deprecation_signal(&response.text()) {
                return Err(FetchFailure::Deprecated);
            }
            return Ok(CodeAssistStatus::default());
        }
        let Ok(json) = response.json::<Value>() else {
            return Ok(CodeAssistStatus::default());
        };
        let status = CodeAssistStatus::from_json(&json, hosted_domain);
        // Google answers the shutdown with HTTP 200: no `currentTier`, and the
        // consumer tier listed under `ineligibleTiers`. The raw body wording is
        // deliberately not consulted here — only the structured listing is, so a
        // licensed account whose `reasonMessage` happens to mention Antigravity is
        // not misread.
        if status.tier.is_none() && status.consumer_client_unsupported {
            return Err(FetchFailure::Deprecated);
        }
        Ok(status)
    }

    fn discover_project(&self, token: &Secret) -> Option<String> {
        let request = HttpRequest::get(PROJECTS_URL)
            .bearer(token)
            .accept_json()
            .timeout(PROBE_TIMEOUT);
        let response = self.client.execute(&request).ok()?;
        if !response.is_success() {
            return None;
        }
        let body: Value = response.json().ok()?;
        for project in body.get("projects")?.as_array()? {
            let project_id = string(project, "projectId")?;
            if project_id.starts_with("gen-lang-client") {
                return Some(project_id);
            }
            if project
                .get("labels")
                .and_then(|labels| labels.get("generative-language"))
                .is_some()
            {
                return Some(project_id);
            }
        }
        None
    }

    fn user_quota(
        &self,
        token: &Secret,
        project_id: Option<&str>,
    ) -> Result<Vec<ModelQuota>, FetchFailure> {
        let body = match project_id {
            Some(project) => json!({ "project": project }),
            None => json!({}),
        };
        let request = HttpRequest::post(QUOTA_URL)
            .bearer(token)
            .accept_json()
            .timeout(QUOTA_TIMEOUT);
        let request = request
            .json_body(&body)
            .map_err(|err| FetchFailure::Error(err.message))?;

        let response = self
            .client
            .execute(&request)
            .map_err(|err| FetchFailure::Error(err.message))?;

        if response.status == 401 {
            return Err(FetchFailure::NotConfigured(
                "Gemini rejected the session (HTTP 401) — run `gemini` again to sign in."
                    .to_string(),
            ));
        }
        if !response.is_success() {
            let text = response.text();
            if is_deprecation_signal(&text) || text.contains("SUBSCRIPTION_REQUIRED") {
                return Err(FetchFailure::Deprecated);
            }
            return Err(FetchFailure::Error(format!(
                "Gemini quota request failed (HTTP {}).",
                response.status
            )));
        }
        let json: Value = response
            .json()
            .map_err(|err| FetchFailure::Error(err.message))?;
        Ok(parse_quotas(&json))
    }
}

/// Map the parsed model quotas onto the provider's lanes.
pub fn map_quotas(snapshot: &mut ProviderSnapshot, quotas: &[ModelQuota], now: DateTime<Utc>) {
    let pick = |predicate: fn(&str) -> bool| -> Option<&ModelQuota> {
        quotas
            .iter()
            .filter(|quota| predicate(&quota.model_id))
            .min_by(|a, b| a.percent_left.total_cmp(&b.percent_left))
    };

    let lanes = [
        (pick(is_pro), "gemini-pro", "Pro", WindowKind::Session),
        (pick(is_flash), "gemini-flash", "Flash", WindowKind::Weekly),
        (
            pick(is_flash_lite),
            "gemini-flash-lite",
            "Flash Lite",
            WindowKind::WeeklyScoped,
        ),
    ];

    for (quota, id, label, kind) in lanes {
        let Some(quota) = quota else {
            continue;
        };
        let mut window = RateWindow::new(quota.used_percent(), Some(1_440), quota.resets_at);
        window.reset_description = quota.resets_at.map(|reset| format_reset(now, reset));
        snapshot
            .windows
            .push(NamedRateWindow::new(id, label, kind, window));
    }
}

fn format_reset(now: DateTime<Utc>, reset: DateTime<Utc>) -> String {
    let minutes = (reset - now).num_minutes();
    if minutes <= 0 {
        return "Resets soon".to_string();
    }
    let hours = minutes / 60;
    if hours > 0 {
        format!("Resets in {}h {}m", hours, minutes % 60)
    } else {
        format!("Resets in {}m", minutes)
    }
}

fn deprecation_message() -> String {
    format!(
        "{CONSUMER_TIER_DEPRECATED}: Google retired Gemini CLI OAuth for individual, AI Pro and \
         Ultra accounts on 2026-06-18. Switch to the Antigravity provider, or use a Workspace / \
         Code Assist Standard account."
    )
}

/// Why a step failed, before it becomes a `FetchStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FetchFailure {
    /// The user simply needs to sign in / configure something.
    NotConfigured(String),
    /// A real failure, with a message safe to display.
    Error(String),
    /// Google's consumer-tier shutdown.
    Deprecated,
}

/// `id_token` claims (email + `hd`). The signature is never verified.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenClaims {
    pub email: Option<String>,
    pub hosted_domain: Option<String>,
}

impl TokenClaims {
    pub fn from_id_token(token: Option<&str>) -> Self {
        let Some(payload) = token.and_then(|token| token.split('.').nth(1)) else {
            return Self::default();
        };
        let Some(bytes) = crate::providers::cursor::base64url_decode(payload) else {
            return Self::default();
        };
        let Ok(json) = serde_json::from_slice::<Value>(&bytes) else {
            return Self::default();
        };
        Self {
            email: string(&json, "email"),
            hosted_domain: string(&json, "hd"),
        }
    }
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn clamp_pct(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 100.0)
}

fn string(object: &Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotas_keep_the_lowest_fraction_per_model() {
        let body = json!({"buckets": [
            {"modelId": "gemini-2.5-pro", "remainingFraction": 0.9, "resetTime": "2026-09-11T20:00:00Z"},
            {"modelId": "gemini-2.5-pro", "remainingFraction": 0.4, "resetTime": "2026-09-11T21:00:00Z"},
            {"modelId": "gemini-2.5-flash", "remainingFraction": 0.85}
        ]});
        let quotas = parse_quotas(&body);
        assert_eq!(quotas.len(), 2);
        assert_eq!(quotas[0].model_id, "gemini-2.5-flash");
        assert_eq!(quotas[1].percent_left, 40.0);
        assert!(quotas[1].resets_at.is_some());
        assert_eq!(quotas[1].used_percent(), 60.0);
    }

    #[test]
    fn a_body_without_buckets_is_empty_not_a_panic() {
        assert!(parse_quotas(&json!({})).is_empty());
        assert!(parse_quotas(&json!({"buckets": null})).is_empty());
        assert!(parse_quotas(&json!({"buckets": [{"modelId": "x"}]})).is_empty());
    }

    #[test]
    fn plan_names_follow_the_documented_precedence() {
        assert_eq!(
            resolve_plan(Some("free-tier"), None, Some("Google One AI Premium")).as_deref(),
            Some("Google One AI Premium")
        );
        assert_eq!(
            resolve_plan(Some("standard-tier"), None, None).as_deref(),
            Some("Paid")
        );
        assert_eq!(
            resolve_plan(Some("free-tier"), Some("corp.example.com"), None).as_deref(),
            Some("Workspace")
        );
        assert_eq!(
            resolve_plan(Some("free-tier"), None, None).as_deref(),
            Some("Free")
        );
        assert_eq!(
            resolve_plan(Some("legacy-tier"), None, None).as_deref(),
            Some("Legacy")
        );
        assert_eq!(resolve_plan(None, None, None), None);
    }

    #[test]
    fn oauth2_js_regex_reads_the_client_constants() {
        let content = "const OAUTH_CLIENT_ID = 'abc-123.apps.googleusercontent.com';\n\
                       const OAUTH_CLIENT_SECRET = 'secret_value-1';\n";
        let creds = ClientCredentials::from_oauth2_js(content).unwrap();
        assert_eq!(creds.client_id, "abc-123.apps.googleusercontent.com");
        assert_eq!(creds.client_secret, "secret_value-1");
        // A missing secret means no credentials, never a half-populated pair.
        assert!(ClientCredentials::from_oauth2_js("const OAUTH_CLIENT_ID = 'abc';").is_none());
        // The suffix must not be mistaken for the constant itself.
        assert!(
            extract_assignment("const OAUTH_CLIENT_IDX = 'nope';", "OAUTH_CLIENT_ID").is_none()
        );
    }

    #[test]
    fn consumer_deprecation_signals_are_recognized() {
        assert!(is_deprecation_signal(
            "{\"reasonCode\":\"UNSUPPORTED_CLIENT\"}"
        ));
        assert!(is_deprecation_signal("IneligibleTierError"));
        assert!(is_deprecation_signal(
            "migrate to Antigravity for Gemini Code Assist"
        ));
        assert!(!is_deprecation_signal("HTTP 429 rate limited"));
    }

    #[test]
    fn unsupported_client_listing_is_suppressed_by_a_paid_tier() {
        let body = json!({"ineligibleTiers": [{"reasonCode": "UNSUPPORTED_CLIENT"}]});
        let personal = CodeAssistStatus::from_json(&body, None);
        assert!(personal.consumer_client_unsupported);
        let paid = CodeAssistStatus::from_json(
            &json!({"paidTier": {"name": "Standard"}, "ineligibleTiers": [{"reasonCode": "UNSUPPORTED_CLIENT"}]}),
            None,
        );
        assert!(!paid.consumer_client_unsupported);
        let workspace = CodeAssistStatus::from_json(&body, Some("corp.example.com"));
        assert!(!workspace.consumer_client_unsupported);
    }

    #[test]
    fn project_id_is_read_from_both_shapes() {
        let string_form = CodeAssistStatus::from_json(
            &json!({"cloudaicompanionProject": "gen-lang-client-1"}),
            None,
        );
        assert_eq!(string_form.project_id.as_deref(), Some("gen-lang-client-1"));
        let object_form = CodeAssistStatus::from_json(
            &json!({"cloudaicompanionProject": {"projectId": "gen-lang-client-2"}}),
            None,
        );
        assert_eq!(object_form.project_id.as_deref(), Some("gen-lang-client-2"));
    }
}
