//! Frozen data contract for the CodexBar Windows port.
//!
//! # Contract rules
//!
//! This module is the **single source of truth** shared by the tray app, the CLI
//! and every provider implementation. Provider workers adding a real provider
//! (OAuth, API key, CLI scrape, ...) MUST NOT change anything in this file.
//! They only implement [`crate::Provider`] in their own crate/module and return
//! [`ProviderSnapshot`] values built from these types.
//!
//! If a field is genuinely needed, add it as `Option<T>` with
//! `#[serde(default, skip_serializing_if = "Option::is_none")]` so older payloads
//! keep decoding, and bump [`SCHEMA_VERSION`].
//!
//! Wire format: `camelCase` JSON keys, mirroring the Swift original
//! (`Sources/CodexBarCore/UsageFetcher.swift`), so both ports can share fixtures.
//! Timestamps are RFC 3339 / ISO 8601 UTC strings (`2026-09-10T18:04:00Z`).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Serde helpers that pin timestamps to a stable, human-readable wire format.
///
/// The Swift port encodes `Date` as `timeIntervalSinceReferenceDate`; we use
/// RFC 3339 with millisecond precision and a `Z` suffix instead, which is what
/// every Windows-side consumer (PowerShell, jq, JS `Date`) parses natively.
pub mod rfc3339 {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialise a UTC instant as `2026-09-10T20:58:33.577Z`.
    pub fn serialize<S: Serializer>(value: &DateTime<Utc>, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&value.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    /// Parse an RFC 3339 / ISO 8601 instant, accepting any offset.
    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<DateTime<Utc>, D::Error> {
        let raw = String::deserialize(de)?;
        DateTime::parse_from_rfc3339(&raw)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }

    /// `Option<DateTime<Utc>>` variant, for `#[serde(with = "rfc3339::option")]`.
    pub mod option {
        use chrono::{DateTime, Utc};
        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S: Serializer>(
            value: &Option<DateTime<Utc>>,
            ser: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(dt) => super::serialize(dt, ser),
                None => ser.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            de: D,
        ) -> Result<Option<DateTime<Utc>>, D::Error> {
            let raw = Option::<String>::deserialize(de)?;
            raw.map(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
        }
    }
}

/// Version of the JSON contract emitted by [`UsageReport`].
/// Consumers (UI, CLI, external scripts) read this to decide how to parse.
pub const SCHEMA_VERSION: u32 = 1;

/// Stable provider identity. Serde-renamed to the same ids the macOS app uses.
///
/// 14 providers: the six "flagship" ones (`SPEC-flagship.md`: Codex, Claude,
/// Cursor, Gemini, Copilot, plus OpenRouter as the first API-key one) followed by
/// the remaining API-key / local-token providers (`SPEC-apikey.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Codex,
    Claude,
    Cursor,
    OpenRouter,
    Copilot,
    Gemini,
    DeepSeek,
    Groq,
    Zai,
    MiniMax,
    Kimi,
    ElevenLabs,
    Xai,
    OpenCodeGo,
}

impl ProviderId {
    /// Every provider this build knows about, in canonical display order.
    ///
    /// The first six keep the order they had before the contract grew (existing
    /// fixtures, evidence files and UI ordering stay byte-identical); the eight
    /// API-key providers are appended after them.
    pub const ALL: [ProviderId; 14] = [
        ProviderId::Codex,
        ProviderId::Claude,
        ProviderId::Cursor,
        ProviderId::OpenRouter,
        ProviderId::Copilot,
        ProviderId::Gemini,
        ProviderId::DeepSeek,
        ProviderId::Groq,
        ProviderId::Zai,
        ProviderId::MiniMax,
        ProviderId::Kimi,
        ProviderId::ElevenLabs,
        ProviderId::Xai,
        ProviderId::OpenCodeGo,
    ];

    /// Machine id (matches the JSON `provider` field and the CLI `--provider` flag).
    pub const fn as_str(self) -> &'static str {
        match self {
            ProviderId::Codex => "codex",
            ProviderId::Claude => "claude",
            ProviderId::Cursor => "cursor",
            ProviderId::OpenRouter => "openrouter",
            ProviderId::Copilot => "copilot",
            ProviderId::Gemini => "gemini",
            ProviderId::DeepSeek => "deepseek",
            ProviderId::Groq => "groq",
            ProviderId::Zai => "zai",
            ProviderId::MiniMax => "minimax",
            ProviderId::Kimi => "kimi",
            ProviderId::ElevenLabs => "elevenlabs",
            ProviderId::Xai => "xai",
            ProviderId::OpenCodeGo => "opencodego",
        }
    }

    /// Human title used in the tray menu, the popover cards and CLI text output.
    pub const fn title(self) -> &'static str {
        match self {
            ProviderId::Codex => "Codex",
            ProviderId::Claude => "Claude",
            ProviderId::Cursor => "Cursor",
            ProviderId::OpenRouter => "OpenRouter",
            ProviderId::Copilot => "Copilot",
            ProviderId::Gemini => "Gemini",
            ProviderId::DeepSeek => "DeepSeek",
            ProviderId::Groq => "Groq",
            ProviderId::Zai => "z.ai",
            ProviderId::MiniMax => "MiniMax",
            ProviderId::Kimi => "Kimi",
            ProviderId::ElevenLabs => "ElevenLabs",
            ProviderId::Xai => "xAI",
            ProviderId::OpenCodeGo => "OpenCode Go",
        }
    }

    /// Logo file name inside `src-tauri/ui/assets/` (sourced from the macOS repo `docs/logos/`).
    ///
    /// The UI falls back to a monogram chip when the file is missing, so adding a
    /// provider does not require a brand asset.
    pub const fn logo(self) -> &'static str {
        match self {
            ProviderId::Codex => "codex.svg",
            ProviderId::Claude => "claude.svg",
            ProviderId::Cursor => "cursor.svg",
            ProviderId::OpenRouter => "openrouter.svg",
            ProviderId::Copilot => "copilot.svg",
            ProviderId::Gemini => "gemini.svg",
            ProviderId::DeepSeek => "deepseek.svg",
            ProviderId::Groq => "groq.svg",
            ProviderId::Zai => "zai.svg",
            ProviderId::MiniMax => "minimax.svg",
            ProviderId::Kimi => "kimi.svg",
            ProviderId::ElevenLabs => "elevenlabs.svg",
            ProviderId::Xai => "xai.svg",
            ProviderId::OpenCodeGo => "opencodego.svg",
        }
    }

    /// Which credential model this provider uses. Workers use it to decide whether
    /// a missing credential is `notConfigured` (API key / OAuth file) or needs a
    /// user-initiated flow (Copilot device code).
    pub const fn auth_kind(self) -> AuthKind {
        match self {
            ProviderId::Codex => AuthKind::LocalOAuthFile,
            ProviderId::Claude => AuthKind::LocalOAuthFile,
            ProviderId::Cursor => AuthKind::LocalOAuthFile,
            ProviderId::Gemini => AuthKind::LocalOAuthFile,
            ProviderId::Copilot => AuthKind::DeviceFlow,
            ProviderId::OpenRouter
            | ProviderId::DeepSeek
            | ProviderId::Groq
            | ProviderId::Zai
            | ProviderId::MiniMax
            | ProviderId::Kimi
            | ProviderId::ElevenLabs
            | ProviderId::Xai
            | ProviderId::OpenCodeGo => AuthKind::ApiKey,
        }
    }

    /// Parse a provider id, tolerating the shapes humans actually type:
    /// case, spaces, dots and hyphens are stripped before matching, so
    /// `OpenCodeGo`, `opencode-go`, `OpenCode Go` and `opencodego` are the same
    /// provider, as are `z.ai` / `zai` and `xAI` / `xai`.
    pub fn from_str_lossy(s: &str) -> Option<ProviderId> {
        let normalized: String = s
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        if normalized.is_empty() {
            return None;
        }
        ProviderId::ALL
            .into_iter()
            .find(|p| p.as_str() == normalized)
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a provider authenticates. Declared on [`ProviderId`] so the UI can decide
/// between a "paste your key" hint and a "run the owner CLI" hint without asking
/// the provider (the provider may fail before it can say anything).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthKind {
    /// API key or bearer token from an env var / the port's own config file.
    ApiKey,
    /// OAuth material owned by another tool (`~/.codex/auth.json`, ...), read-only.
    LocalOAuthFile,
    /// User-initiated device-code flow (GitHub Copilot).
    DeviceFlow,
}

impl AuthKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            AuthKind::ApiKey => "apiKey",
            AuthKind::LocalOAuthFile => "localOAuthFile",
            AuthKind::DeviceFlow => "deviceFlow",
        }
    }
}

/// Which quota lane a window belongs to. Drives ordering, bar labels and the
/// "Automatic" metric resolution in the tray/UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowKind {
    /// Rolling short window (e.g. Claude's 5-hour session).
    Session,
    /// Long window (7 days for most providers).
    Weekly,
    /// Model-scoped carve-out of the weekly quota.
    WeeklyScoped,
    /// Anything else the provider reports (extra usage, credits reset, ...).
    Extra,
}

impl WindowKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            WindowKind::Session => "session",
            WindowKind::Weekly => "weekly",
            WindowKind::WeeklyScoped => "weeklyScoped",
            WindowKind::Extra => "extra",
        }
    }
}

/// One quota window. Direct analogue of the Swift `RateWindow`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateWindow {
    /// Percentage of the window already consumed. Intentionally **not** clamped:
    /// providers can exceed 100 (over-quota) and display code clamps for rendering.
    pub used_percent: f64,
    /// Length of the window in minutes when known (300 = 5h, 10080 = 7d).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<i64>,
    /// Absolute UTC instant the window rolls over.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "rfc3339::option"
    )]
    pub resets_at: Option<DateTime<Utc>>,
    /// Free-text reset description for providers that only expose prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_description: Option<String>,
    /// Percent restored on the next rolling regeneration tick, if the provider says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_regen_percent: Option<f64>,
    /// True when the provider did not report this lane and we synthesised a
    /// placeholder (e.g. Claude web returning a `null` five_hour). Lane
    /// classifiers must treat such a window as "lane absent", not as "0%".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_synthetic_placeholder: bool,
}

impl RateWindow {
    pub fn new(
        used_percent: f64,
        window_minutes: Option<i64>,
        resets_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            used_percent,
            window_minutes,
            resets_at,
            reset_description: None,
            next_regen_percent: None,
            is_synthetic_placeholder: false,
        }
    }

    /// Percent still available. Clamped at 0 so over-quota providers cannot show negative headroom.
    pub fn remaining_percent(&self) -> f64 {
        (100.0 - self.used_percent).max(0.0)
    }

    /// Usage percent clamped into the displayable `0..=100` range.
    pub fn display_clamped(&self) -> f64 {
        self.used_percent.clamp(0.0, 100.0)
    }

    /// Time left until the window resets, or `None` when unknown / already elapsed.
    pub fn time_until_reset(&self, now: DateTime<Utc>) -> Option<Duration> {
        self.resets_at
            .map(|t| t - now)
            .filter(|d| *d > Duration::zero())
    }

    /// Re-attach a reset instant from a previously cached window when the fresh
    /// fetch lost it (upstream glitch). Mirrors the Swift `backfillingResetTime`.
    pub fn backfilling_reset_time(
        &self,
        cached: Option<&RateWindow>,
        now: DateTime<Utc>,
    ) -> RateWindow {
        if self.resets_at.is_some() {
            return self.clone();
        }
        let Some(cached_reset) = cached.and_then(|c| c.resets_at).filter(|t| *t > now) else {
            return self.clone();
        };
        RateWindow {
            window_minutes: match self.window_minutes {
                Some(m) if m > 0 => self.window_minutes,
                _ => cached.and_then(|c| c.window_minutes),
            },
            resets_at: Some(cached_reset),
            reset_description: self
                .reset_description
                .clone()
                .or_else(|| cached.and_then(|c| c.reset_description.clone())),
            is_synthetic_placeholder: self.is_synthetic_placeholder,
            ..self.clone()
        }
    }
}

/// A named quota lane, e.g. `session` / `weekly` / `weekly (Sonnet)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedRateWindow {
    /// Stable slug used as a DOM id / CLI key: `session`, `weekly`, `weekly-sonnet`, ...
    pub id: String,
    /// Display label.
    pub title: String,
    /// Which lane family this is, for ordering and "auto" resolution.
    pub kind: WindowKind,
    pub window: RateWindow,
    /// False when the provider returned reset metadata but no usable usage number.
    /// Clients must not render `usedPercent` as a real quota in that case.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub usage_known: bool,
}

fn default_true() -> bool {
    true
}
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(v: &bool) -> bool {
    *v
}

impl NamedRateWindow {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: WindowKind,
        window: RateWindow,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            kind,
            window,
            usage_known: true,
        }
    }

    pub fn with_usage_known(mut self, known: bool) -> Self {
        self.usage_known = known;
        self
    }
}

/// Credits / prepaid balance a provider exposes next to its quotas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoneyBalance {
    pub amount: f64,
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// How the numbers were obtained. Displayed as a small badge on each card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DataSource {
    /// Plain API key (env var or config file).
    ApiKey,
    /// OAuth token from the provider's own credential store.
    OAuth,
    /// Scraped from the provider's own CLI.
    Cli,
    /// Scraped from the provider's web dashboard using browser cookies.
    Web,
    /// Hard-coded sample data — nothing left the machine.
    Mock,
}

impl DataSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            DataSource::ApiKey => "apiKey",
            DataSource::OAuth => "oauth",
            DataSource::Cli => "cli",
            DataSource::Web => "web",
            DataSource::Mock => "mock",
        }
    }
}

/// Health of the most recent fetch for a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FetchStatus {
    /// Fresh numbers this cycle.
    Ok,
    /// Last known good numbers, older than the refresh interval.
    Stale,
    /// Fetch failed; `error` explains why.
    Error,
    /// Not configured yet (no credentials) — the card renders a setup hint.
    NotConfigured,
}

impl FetchStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            FetchStatus::Ok => "ok",
            FetchStatus::Stale => "stale",
            FetchStatus::Error => "error",
            FetchStatus::NotConfigured => "notConfigured",
        }
    }
}

/// Everything the UI needs to render one provider card and its tray entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSnapshot {
    pub provider: ProviderId,
    /// Display title (lets a provider override its own branding without touching the enum).
    pub title: String,
    /// Account identity for the card subtitle. Never mix providers here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Plan name, e.g. `Max 20x`, `Pro`, `Team`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Quota lanes, in display order.
    pub windows: Vec<NamedRateWindow>,
    /// Optional credit balance (OpenRouter-style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<MoneyBalance>,
    pub status: FetchStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub source: DataSource,
    /// When this snapshot was produced.
    #[serde(with = "rfc3339")]
    pub fetched_at: DateTime<Utc>,
}

impl ProviderSnapshot {
    /// The lane that best represents the provider right now: the most constrained
    /// non-placeholder window, preferring `Session` then `Weekly`.
    pub fn headline_window(&self) -> Option<&NamedRateWindow> {
        let usable = |w: &&NamedRateWindow| w.usage_known && !w.window.is_synthetic_placeholder;

        let by_kind = |kinds: &[WindowKind]| {
            self.windows
                .iter()
                .filter(usable)
                .filter(|w| kinds.contains(&w.kind))
                .max_by(|a, b| {
                    a.window
                        .used_percent
                        .partial_cmp(&b.window.used_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        };

        by_kind(&[WindowKind::Session])
            .or_else(|| by_kind(&[WindowKind::Weekly]))
            .or_else(|| by_kind(&[WindowKind::WeeklyScoped]))
            .or_else(|| by_kind(&[WindowKind::Extra]))
    }

    /// Headline usage percent, clamped for display.
    pub fn headline_used_percent(&self) -> Option<f64> {
        self.headline_window().map(|w| w.window.display_clamped())
    }

    /// Highest usage percent across all real lanes — used to pick the tray icon colour.
    pub fn max_used_percent(&self) -> f64 {
        self.windows
            .iter()
            .filter(|w| w.usage_known && !w.window.is_synthetic_placeholder)
            .map(|w| w.window.display_clamped())
            .fold(0.0_f64, f64::max)
    }

    /// Compact tray label, e.g. `Codex 42%`.
    pub fn tray_label(&self) -> String {
        match self.headline_used_percent() {
            Some(p) => format!("{} {}%", self.title, p.round() as i64),
            None => format!("{} —", self.title),
        }
    }
}

/// Top-level payload of `codexbar usage --format json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub schema_version: u32,
    #[serde(with = "rfc3339")]
    pub generated_at: DateTime<Utc>,
    /// Ordered exactly like [`ProviderId::ALL`] (canonical display order).
    pub providers: Vec<ProviderSnapshot>,
}

impl UsageReport {
    pub fn new(providers: Vec<ProviderSnapshot>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            generated_at: Utc::now(),
            providers,
        }
    }

    pub fn get(&self, id: ProviderId) -> Option<&ProviderSnapshot> {
        self.providers.iter().find(|p| p.provider == id)
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("UsageReport is always serialisable")
    }
}

impl std::fmt::Display for UsageReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let now = Utc::now();
        for p in &self.providers {
            writeln!(f, "{}", p.tray_label())?;
            for w in &p.windows {
                // Countdown, matching the app's default "resets in …" presentation.
                let reset = match w.window.time_until_reset(now) {
                    Some(delta) => format!("  resets in {}", humanize(delta)),
                    None => String::new(),
                };
                writeln!(
                    f,
                    "  {:<16} {:>5.1}% used{}",
                    w.title, w.window.used_percent, reset
                )?;
            }
            if let Some(err) = &p.error {
                writeln!(f, "  note: {err}")?;
            }
        }
        Ok(())
    }
}

/// Compact human duration: `3h 12m`, `2d 4h`, `42s`.
pub fn humanize(delta: Duration) -> String {
    let secs = delta.num_seconds().max(0);
    let (days, hours, minutes) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

/// Severity buckets shared by the tray icon, the UI accent colours and alerts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Healthy,
    Warning,
    Critical,
}

/// Map a used-percent value onto the shared severity scale.
///
/// Thresholds (used %, not remaining): `< 70` healthy, `< 90` warning, else critical.
pub const fn severity_for_used_percent(used_percent: f64) -> Severity {
    if used_percent >= 90.0 {
        Severity::Critical
    } else if used_percent >= 70.0 {
        Severity::Warning
    } else {
        Severity::Healthy
    }
}

/// The only trait a provider worker has to implement.
///
/// Implementations live in their own modules (`providers::claude`, ...) and are
/// registered in `build_registry()`. They may use any dependency they like; the
/// contract with the rest of the app is exactly this method.
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;

    /// Fetch the current snapshot. Implementations must never panic: return a
    /// snapshot with [`FetchStatus::Error`] and an `error` message instead.
    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot;
}

/// Sequential fetch of every registered provider, in canonical order.
///
/// Providers with no credentials should return [`FetchStatus::NotConfigured`];
/// they stay in the report so the UI can show a setup hint.
pub fn collect(providers: &[Box<dyn Provider>]) -> UsageReport {
    let now = Utc::now();
    let mut snapshots: Vec<ProviderSnapshot> = providers.iter().map(|p| p.fetch(now)).collect();
    snapshots.sort_by_key(|s| {
        ProviderId::ALL
            .iter()
            .position(|p| *p == s.provider)
            .unwrap_or(usize::MAX)
    });
    UsageReport::new(snapshots)
}
