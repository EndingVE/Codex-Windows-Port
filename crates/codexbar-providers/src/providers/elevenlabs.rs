//! **ElevenLabs** — subscription character credits and voice slots.
//!
//! One endpoint, one header (`SPEC-apikey.md` §9):
//!
//! | Behaviour | Where |
//! | --- | --- |
//! | Credential precedence (token account → `providers[].apiKey` → env) | [`PortConfig::resolve_api_key`] |
//! | `ELEVENLABS_API_KEY` > `XI_API_KEY` alias chain | [`ENV_ALIASES`] |
//! | `ELEVENLABS_API_URL` must be HTTPS, fail closed | [`secure_base_url`] |
//! | `GET {base}/v1/user/subscription` with the **`xi-api-key`** header (never `Authorization`) | [`ElevenLabs::fetch`] |
//! | Character window + optional voice-slot windows | [`ElevenLabs::fetch`] |
//! | `detail.code` / `detail.status` → the documented 401/403 messages | [`auth_error`] |
//!
//! Missing credentials are `notConfigured`, never `error`, so the card renders a
//! setup hint. The API key is only ever shown masked (`account`).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use codexbar_core::{
    DataSource, FetchStatus, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    WindowKind,
};
use serde::Deserialize;

use crate::credential::{Env, PortConfig, Secret};
use crate::http::{secure_base_url, HttpClient, HttpRequest};

/// Primary credential env var.
pub const ENV_API_KEY: &str = "ELEVENLABS_API_KEY";
/// Alias accepted by the macOS app and the API documentation.
pub const ENV_API_KEY_ALT: &str = "XI_API_KEY";
/// Base URL override — HTTPS only.
pub const ENV_API_URL: &str = "ELEVENLABS_API_URL";

/// Default API base.
pub const DEFAULT_BASE_URL: &str = "https://api.elevenlabs.io";

/// `SPEC-apikey.md` §9 fixes a 15 s timeout for this provider.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

const ENV_ALIASES: [&str; 2] = [ENV_API_KEY, ENV_API_KEY_ALT];

pub struct ElevenLabs {
    client: Arc<dyn HttpClient>,
    env: Env,
    config: Option<PortConfig>,
}

impl ElevenLabs {
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
            Some(config) => config.resolve_api_key(ProviderId::ElevenLabs, &self.env, &ENV_ALIASES),
            None => self.env.first_of(&ENV_ALIASES),
        }
    }

    fn base_url(&self) -> Result<String, String> {
        secure_base_url(self.env.get_str(ENV_API_URL).as_deref(), DEFAULT_BASE_URL)
    }
}

impl Default for ElevenLabs {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ElevenLabs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElevenLabs")
            .field("has_credentials", &self.api_key().is_some())
            .field("config", &self.config.as_ref().map(PortConfig::source_path))
            .finish()
    }
}

/// The `/v1/user/subscription` payload (`SPEC-apikey.md` §9).
///
/// `character_count` / `character_limit` are required: a body without them is a
/// parse failure, not a $0 account.
#[derive(Debug, Clone, Deserialize)]
struct Subscription {
    #[serde(default)]
    tier: Option<String>,
    character_count: i64,
    character_limit: i64,
    #[serde(default)]
    voice_slots_used: Option<i64>,
    #[serde(default)]
    professional_voice_slots_used: Option<i64>,
    #[serde(default)]
    voice_limit: Option<i64>,
    #[serde(default)]
    professional_voice_limit: Option<i64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    next_character_count_reset_unix: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorResponse {
    #[serde(default)]
    detail: Option<ApiErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

impl Provider for ElevenLabs {
    fn id(&self) -> ProviderId {
        ProviderId::ElevenLabs
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot {
            provider: ProviderId::ElevenLabs,
            title: ProviderId::ElevenLabs.title().to_string(),
            account: None,
            plan: None,
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
                "No ElevenLabs API key found — set {ENV_API_KEY} or {ENV_API_KEY_ALT}, or add \
                 providers[].apiKey for \"elevenlabs\" to %APPDATA%\\CodexBar\\config.json."
            ));
            return snapshot;
        };
        snapshot.account = Some(api_key.redacted());

        let base = match self.base_url() {
            Ok(base) => base,
            Err(reason) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!("{ENV_API_URL} is not usable: {reason}"));
                return snapshot;
            }
        };

        let request = HttpRequest::get(subscription_url(&base))
            .header("xi-api-key", api_key.expose())
            .accept_json()
            .timeout(REQUEST_TIMEOUT);

        let response = match self.client.execute(&request) {
            Ok(response) => response,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(err.message);
                return snapshot;
            }
        };

        match response.status {
            200 => {}
            401 => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(auth_error(
                    &response.body,
                    "ElevenLabs could not authenticate the selected API key. Check the key and \
                     its permissions.",
                ));
                return snapshot;
            }
            403 => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(auth_error(
                    &response.body,
                    "ElevenLabs denied access for the selected API key. Check its endpoint \
                     permissions and IP allowlist.",
                ));
                return snapshot;
            }
            other => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!("ElevenLabs API error: HTTP {other}."));
                return snapshot;
            }
        }

        let subscription: Subscription = match response.json() {
            Ok(subscription) => subscription,
            Err(err) => {
                snapshot.status = FetchStatus::Error;
                snapshot.error = Some(format!(
                    "Failed to parse ElevenLabs response: {}",
                    err.message
                ));
                return snapshot;
            }
        };

        let resets_at = subscription
            .next_character_count_reset_unix
            .and_then(|unix| Utc.timestamp_opt(unix, 0).single());

        let mut characters = RateWindow::new(
            percent(subscription.character_count, subscription.character_limit),
            None,
            resets_at,
        );
        characters.reset_description = Some(format!(
            "{} / {} credits",
            format_count(subscription.character_count),
            format_count(subscription.character_limit)
        ));
        snapshot.windows.push(NamedRateWindow::new(
            "characters",
            "Characters",
            WindowKind::Extra,
            characters,
        ));

        if let (Some(used), Some(limit)) = (subscription.voice_slots_used, subscription.voice_limit)
        {
            if limit > 0 {
                snapshot
                    .windows
                    .push(voice_window("voice-slots", "Voice slots", used, limit));
            }
        }
        if let (Some(used), Some(limit)) = (
            subscription.professional_voice_slots_used,
            subscription.professional_voice_limit,
        ) {
            if limit > 0 {
                snapshot.windows.push(voice_window(
                    "professional-voices",
                    "Professional voices",
                    used,
                    limit,
                ));
            }
        }

        snapshot.plan = display_tier(subscription.tier.as_deref(), subscription.status.as_deref());

        snapshot
    }
}

fn subscription_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/user/subscription")
    } else {
        format!("{base}/v1/user/subscription")
    }
}

fn voice_window(id: &str, title: &str, used: i64, limit: i64) -> NamedRateWindow {
    let mut window = RateWindow::new(percent(used, limit), None, None);
    window.reset_description = Some(format!("{used} / {limit}"));
    NamedRateWindow::new(id, title, WindowKind::Extra, window)
}

/// Percentage consumed, clamped for display. `limit <= 0` publishes no pressure.
fn percent(used: i64, limit: i64) -> f64 {
    if limit <= 0 {
        return 0.0;
    }
    ((used as f64 / limit as f64) * 100.0).clamp(0.0, 100.0)
}

/// The documented `detail.code` / `detail.status` diagnosis (`docs/elevenlabs.md`).
fn auth_error(body: &[u8], fallback: &str) -> String {
    let detail = serde_json::from_slice::<ApiErrorResponse>(body)
        .ok()
        .and_then(|parsed| parsed.detail);
    if let Some(detail) = detail {
        for value in [detail.code.as_deref(), detail.status.as_deref()]
            .into_iter()
            .flatten()
        {
            match value.trim().to_ascii_lowercase().as_str() {
                "invalid_api_key" => {
                    return "ElevenLabs rejected the selected API key. Check that it is valid and \
                            has not been revoked."
                        .to_string();
                }
                "missing_permissions" | "insufficient_permissions" => {
                    return "ElevenLabs API key is missing the user_read permission required to \
                            fetch subscription usage."
                        .to_string();
                }
                _ => {}
            }
        }
    }
    fallback.to_string()
}

/// `creator` → `Creator`, `creator_pro` → `Creator Pro`, plus a ` · status`
/// suffix when the subscription is not simply `active`.
fn display_tier(tier: Option<&str>, status: Option<&str>) -> Option<String> {
    let tier = tier.map(str::trim).filter(|value| !value.is_empty());
    let status = status.map(str::trim).filter(|value| !value.is_empty());
    match tier {
        Some(tier) => {
            let pretty = title_case(&tier.replace('_', " "));
            match status {
                Some(status) if !status.eq_ignore_ascii_case("active") => {
                    Some(format!("{pretty} · {status}"))
                }
                _ => Some(pretty),
            }
        }
        None => status.map(str::to_string),
    }
}

fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `12345` → `12,345` (matches the macOS reset description).
fn format_count(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_is_clamped_and_guards_a_zero_limit() {
        assert_eq!(percent(25_000, 100_000), 25.0);
        assert_eq!(percent(150, 100), 100.0);
        assert_eq!(percent(5, 0), 0.0);
    }

    #[test]
    fn tier_is_pretty_printed_with_a_status_suffix() {
        assert_eq!(
            display_tier(Some("creator"), Some("active")).as_deref(),
            Some("Creator")
        );
        assert_eq!(
            display_tier(Some("creator_pro"), Some("trialing")).as_deref(),
            Some("Creator Pro · trialing")
        );
        assert_eq!(
            display_tier(None, Some("past_due")).as_deref(),
            Some("past_due")
        );
        assert_eq!(display_tier(None, None), None);
    }

    #[test]
    fn counts_are_grouped() {
        assert_eq!(format_count(12_345), "12,345");
        assert_eq!(format_count(1_000_000), "1,000,000");
        assert_eq!(format_count(0), "0");
    }

    #[test]
    fn subscription_url_respects_a_v1_suffixed_override() {
        assert_eq!(
            subscription_url("https://api.elevenlabs.io"),
            "https://api.elevenlabs.io/v1/user/subscription"
        );
        assert_eq!(
            subscription_url("https://proxy.example.com/v1/"),
            "https://proxy.example.com/v1/user/subscription"
        );
    }

    #[test]
    fn auth_errors_follow_the_documented_code_then_status_order() {
        let body = br#"{"detail":{"code":"invalid_api_key","status":"missing_permissions"}}"#;
        assert!(auth_error(body, "fallback").contains("not been revoked"));
        let body = br#"{"detail":{"status":"missing_permissions"}}"#;
        assert!(auth_error(body, "fallback").contains("user_read permission"));
        assert_eq!(auth_error(b"{}", "fallback"), "fallback");
    }
}
