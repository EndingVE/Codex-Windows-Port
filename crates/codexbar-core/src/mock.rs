//! Deterministic sample data.
//!
//! Used by the tray app, the CLI (`--mock`, the default) and the static UI so the
//! whole product can be exercised with **no credentials and no network access**.
//!
//! Values are derived from a fixed seed plus the current time, so two runs a minute
//! apart look like a live refresh while remaining reproducible in tests.

use chrono::{DateTime, Duration, Utc};

use crate::types::{
    DataSource, FetchStatus, MoneyBalance, NamedRateWindow, Provider, ProviderId, ProviderSnapshot,
    RateWindow, UsageReport, WindowKind,
};

/// A mock provider: returns a synthesised snapshot for one [`ProviderId`].
pub struct MockProvider {
    id: ProviderId,
    /// Scenario knob, useful for screenshots and for testing degraded UI states.
    scenario: Scenario,
}

/// Which shape of sample data a [`MockProvider`] produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Everything healthy, mid-range usage.
    Normal,
    /// One provider close to its limit (critical tray colour).
    NearlyExhausted,
    /// Fetch failed — exercises the error card.
    Error,
    /// No credentials configured — exercises the setup hint.
    NotConfigured,
}

impl MockProvider {
    pub fn new(id: ProviderId) -> Self {
        Self {
            id,
            scenario: default_scenario(id),
        }
    }

    pub fn with_scenario(id: ProviderId, scenario: Scenario) -> Self {
        Self { id, scenario }
    }
}

/// Scenario each provider uses in the default demo report.
///
/// Deliberately covers every card state the UI can render — healthy, near-limit,
/// fetch failure and unconfigured — so screenshots and manual runs exercise the
/// real code paths instead of only the happy one.
pub const fn default_scenario(id: ProviderId) -> Scenario {
    match id {
        ProviderId::Codex => Scenario::Normal,
        ProviderId::Claude => Scenario::Normal,
        ProviderId::Cursor => Scenario::NearlyExhausted,
        ProviderId::OpenRouter => Scenario::Normal,
        ProviderId::Copilot => Scenario::NotConfigured,
        ProviderId::Gemini => Scenario::Error,
        // API-key providers: one of them is left unconfigured so the demo report
        // also exercises `apiKey` setup hints, and the rest are healthy.
        ProviderId::DeepSeek => Scenario::Normal,
        ProviderId::Groq => Scenario::Normal,
        ProviderId::Zai => Scenario::Normal,
        ProviderId::MiniMax => Scenario::NotConfigured,
        ProviderId::Kimi => Scenario::Normal,
        ProviderId::ElevenLabs => Scenario::Normal,
        ProviderId::Xai => Scenario::Normal,
        ProviderId::OpenCodeGo => Scenario::Normal,
    }
}

impl Provider for MockProvider {
    fn id(&self) -> ProviderId {
        self.id
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        mock_snapshot(self.id, self.scenario, now)
    }
}

/// Build the full mock report: every known provider, deterministic per `now`.
pub fn mock_report(now: DateTime<Utc>) -> UsageReport {
    let providers = ProviderId::ALL
        .into_iter()
        .map(|id| mock_snapshot(id, default_scenario(id), now))
        .collect();
    UsageReport {
        schema_version: crate::types::SCHEMA_VERSION,
        generated_at: now,
        providers,
    }
}

/// Mock registry, ready to hand to [`crate::types::collect`].
///
/// Uses [`default_scenario`] per provider, so the registry (and therefore the CLI
/// and the tray app) shows the same mix of states as [`mock_report`].
pub fn mock_registry() -> Vec<Box<dyn Provider>> {
    ProviderId::ALL
        .into_iter()
        .map(|id| Box::new(MockProvider::new(id)) as Box<dyn Provider>)
        .collect()
}

/// [`mock_registry`] as shared providers, for
/// [`crate::refresh::collect_with`] (which can abandon a wedged fetch and so
/// needs `Arc` rather than borrowed providers).
pub fn mock_registry_shared() -> Vec<std::sync::Arc<dyn Provider>> {
    ProviderId::ALL
        .into_iter()
        .map(|id| std::sync::Arc::new(MockProvider::new(id)) as std::sync::Arc<dyn Provider>)
        .collect()
}

fn hourly_seed(now: DateTime<Utc>) -> f64 {
    // Stable within a minute bucket: screenshots taken back-to-back match.
    ((now.timestamp() / 60) % 360) as f64
}

fn jitter(seed: f64, salt: f64, span: f64) -> f64 {
    let x = (seed * 12.9898 + salt * 78.233).sin() * 43758.5453;
    (x - x.floor()) * span
}

/// Stable per-provider offset, so each provider shows its own numbers instead of
/// every provider mirroring the same synthetic values.
fn provider_salt(id: ProviderId) -> f64 {
    ProviderId::ALL.iter().position(|p| *p == id).unwrap_or(0) as f64 * 13.37
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// One synthesised snapshot. Public so providers/tests can build fixtures.
pub fn mock_snapshot(id: ProviderId, scenario: Scenario, now: DateTime<Utc>) -> ProviderSnapshot {
    let seed = hourly_seed(now);
    let salt = provider_salt(id);
    let session_reset = now + Duration::minutes((jitter(seed, salt + 1.0, 240.0) + 20.0) as i64);
    let weekly_reset =
        now + Duration::days(2) + Duration::hours(jitter(seed, salt + 2.0, 96.0) as i64);

    let (account, plan) = match id {
        ProviderId::Codex => (Some("user@example.com"), Some("Plus")),
        ProviderId::Claude => (Some("user@example.com"), Some("Max 20x")),
        ProviderId::Cursor => (Some("user@example.com"), Some("Pro")),
        ProviderId::OpenRouter => (Some("sk-or-v1…9f2c"), None),
        ProviderId::Copilot => (None, Some("Individual")),
        ProviderId::Gemini => (Some("user@example.com"), Some("AI Pro")),
        ProviderId::DeepSeek => (None, None),
        ProviderId::Groq => (Some("user@example.com"), Some("Enterprise")),
        ProviderId::Zai => (Some("user@example.com"), Some("Coding Plan Pro")),
        ProviderId::MiniMax => (None, Some("Coding Plan")),
        ProviderId::Kimi => (Some("user@example.com"), Some("Moderato")),
        ProviderId::ElevenLabs => (None, Some("Creator")),
        ProviderId::Xai => (None, None),
        ProviderId::OpenCodeGo => (Some("wrk_example…0001"), None),
    };

    let mut snapshot = ProviderSnapshot {
        provider: id,
        title: id.title().to_string(),
        account: account.map(str::to_string),
        plan: plan.map(str::to_string),
        windows: Vec::new(),
        balance: None,
        status: FetchStatus::Ok,
        error: None,
        source: DataSource::Mock,
        fetched_at: now,
    };

    match scenario {
        Scenario::NotConfigured => {
            snapshot.status = FetchStatus::NotConfigured;
            snapshot.source = DataSource::Mock;
            snapshot.error =
                Some("No credentials found — open Settings to connect this provider.".into());
            return snapshot;
        }
        Scenario::Error => {
            snapshot.status = FetchStatus::Error;
            snapshot.error = Some("HTTP 503 from upstream usage endpoint (mock failure).".into());
            return snapshot;
        }
        Scenario::Normal | Scenario::NearlyExhausted => {}
    }

    let (session_used, weekly_used) = match scenario {
        Scenario::NearlyExhausted => (
            round1(72.0 + jitter(seed, salt + 3.0, 20.0)),
            round1(93.0 + jitter(seed, salt + 4.0, 5.0)),
        ),
        _ => (
            round1(8.0 + jitter(seed, salt + 3.0, 55.0)),
            round1(15.0 + jitter(seed, salt + 4.0, 60.0)),
        ),
    };

    match id {
        ProviderId::Codex | ProviderId::Claude | ProviderId::Cursor | ProviderId::Gemini => {
            snapshot.windows.push(NamedRateWindow::new(
                "session",
                "Session · 5h",
                WindowKind::Session,
                RateWindow {
                    used_percent: session_used,
                    window_minutes: Some(300),
                    resets_at: Some(session_reset),
                    reset_description: None,
                    next_regen_percent: None,
                    is_synthetic_placeholder: false,
                },
            ));
            snapshot.windows.push(NamedRateWindow::new(
                "weekly",
                "Weekly · 7d",
                WindowKind::Weekly,
                RateWindow {
                    used_percent: weekly_used,
                    window_minutes: Some(10_080),
                    resets_at: Some(weekly_reset),
                    reset_description: None,
                    next_regen_percent: None,
                    is_synthetic_placeholder: false,
                },
            ));
        }
        ProviderId::OpenRouter => {
            // Balance-only provider: no quota lanes, but a credit balance.
            snapshot.balance = Some(MoneyBalance {
                amount: round1(42.5 - jitter(seed, 5.0, 12.0)),
                currency: "USD".into(),
                label: Some("Credits remaining".into()),
            });
            snapshot.windows.push(NamedRateWindow::new(
                "credits",
                "Credits · 30d",
                WindowKind::Extra,
                RateWindow::new(
                    round1(20.0 + jitter(seed, salt + 6.0, 40.0)),
                    Some(43_200),
                    Some(weekly_reset),
                ),
            ));
        }
        ProviderId::Copilot => {
            snapshot.windows.push(NamedRateWindow::new(
                "monthly",
                "Monthly",
                WindowKind::Extra,
                RateWindow::new(
                    round1(33.0 + jitter(seed, salt + 7.0, 20.0)),
                    Some(43_200),
                    Some(weekly_reset),
                ),
            ));
        }
        ProviderId::DeepSeek => {
            // Balance-only provider: no quota lanes at all. Exercises the
            // "snapshot with a balance and zero windows" render path.
            snapshot.balance = Some(MoneyBalance {
                amount: round1(18.4 - jitter(seed, 8.0, 6.0)),
                currency: "USD".into(),
                label: Some("Balance".into()),
            });
        }
        ProviderId::Groq => {
            // Console activity is bucketed per day.
            snapshot.windows.push(NamedRateWindow::new(
                "daily",
                "Daily · tokens",
                WindowKind::Extra,
                RateWindow::new(
                    round1(12.0 + jitter(seed, salt + 8.0, 45.0)),
                    Some(1_440),
                    Some(weekly_reset),
                ),
            ));
        }
        ProviderId::Zai => {
            for (id_, title, kind, minutes) in [
                ("session", "Session · 5h", WindowKind::Session, 300),
                ("weekly", "Weekly · 7d", WindowKind::Weekly, 10_080),
            ] {
                snapshot.windows.push(NamedRateWindow::new(
                    id_,
                    title,
                    kind,
                    RateWindow::new(
                        if kind == WindowKind::Session {
                            session_used
                        } else {
                            weekly_used
                        },
                        Some(minutes),
                        Some(if kind == WindowKind::Session {
                            session_reset
                        } else {
                            weekly_reset
                        }),
                    ),
                ));
            }
        }
        ProviderId::MiniMax => {
            snapshot.windows.push(NamedRateWindow::new(
                "daily",
                "Daily",
                WindowKind::Extra,
                RateWindow::new(
                    round1(24.0 + jitter(seed, salt + 9.0, 40.0)),
                    Some(1_440),
                    Some(weekly_reset),
                ),
            ));
        }
        ProviderId::Kimi => {
            // Kimi Code: a 5 h rate limit plus the weekly plan window.
            snapshot.windows.push(NamedRateWindow::new(
                "rate-limit",
                "Rate limit · 5h",
                WindowKind::Session,
                RateWindow::new(session_used, Some(300), Some(session_reset)),
            ));
            snapshot.windows.push(NamedRateWindow::new(
                "weekly",
                "Weekly · plan",
                WindowKind::Weekly,
                RateWindow::new(weekly_used, Some(10_080), Some(weekly_reset)),
            ));
        }
        ProviderId::ElevenLabs => {
            // Character credits are the primary meter; voice slots are extras.
            snapshot.windows.push(NamedRateWindow::new(
                "characters",
                "Characters · monthly",
                WindowKind::Weekly,
                RateWindow::new(weekly_used, Some(43_200), Some(weekly_reset)),
            ));
            for (id_, title, used) in [
                (
                    "voice-slots",
                    "Voice slots",
                    round1(30.0 + jitter(seed, salt + 10.0, 40.0)),
                ),
                (
                    "professional-voices",
                    "Professional voices",
                    round1(50.0 + jitter(seed, salt + 11.0, 40.0)),
                ),
            ] {
                snapshot.windows.push(NamedRateWindow::new(
                    id_,
                    title,
                    WindowKind::Extra,
                    RateWindow::new(used, None, None),
                ));
            }
        }
        ProviderId::Xai => {
            // Prepaid balance: a money figure plus the matching Extra lane.
            snapshot.balance = Some(MoneyBalance {
                amount: round1(120.0 - jitter(seed, 12.0, 90.0)),
                currency: "USD".into(),
                label: Some("Prepaid balance".into()),
            });
            snapshot.windows.push(NamedRateWindow::new(
                "prepaid",
                "Prepaid · 30d",
                WindowKind::Extra,
                RateWindow::new(
                    round1(15.0 + jitter(seed, salt + 12.0, 60.0)),
                    Some(43_200),
                    Some(weekly_reset),
                ),
            ));
        }
        ProviderId::OpenCodeGo => {
            for (id_, title, kind, minutes, used) in [
                (
                    "session",
                    "Session · 5h",
                    WindowKind::Session,
                    300,
                    session_used,
                ),
                (
                    "weekly",
                    "Weekly · 7d",
                    WindowKind::Weekly,
                    10_080,
                    weekly_used,
                ),
                (
                    "monthly",
                    "Monthly · 30d",
                    WindowKind::Extra,
                    43_200,
                    round1(10.0 + jitter(seed, salt + 13.0, 35.0)),
                ),
            ] {
                snapshot.windows.push(NamedRateWindow::new(
                    id_,
                    title,
                    kind,
                    RateWindow::new(used, Some(minutes), Some(weekly_reset)),
                ));
            }
        }
    }

    // Claude model-scoped weekly carve-out (mirrors `seven_day_sonnet`).
    if id == ProviderId::Claude {
        snapshot.windows.push(NamedRateWindow::new(
            "weekly-sonnet",
            "Weekly · Sonnet",
            WindowKind::WeeklyScoped,
            RateWindow {
                used_percent: round1(weekly_used * 0.7),
                window_minutes: Some(10_080),
                resets_at: Some(weekly_reset),
                reset_description: None,
                next_regen_percent: None,
                is_synthetic_placeholder: false,
            },
        ));
    }

    snapshot
}

/// Pretty JSON for the mock report, matching `codexbar usage --format json`.
pub fn mock_json(now: DateTime<Utc>) -> String {
    mock_report(now).to_json_pretty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_covers_every_provider_in_canonical_order() {
        let report = mock_report(Utc::now());
        let ids: Vec<ProviderId> = report.providers.iter().map(|p| p.provider).collect();
        assert_eq!(ids, ProviderId::ALL.to_vec());
    }

    /// `collect` fetches in parallel now; the report must still come back in
    /// canonical order and identical to the sequential mock report.
    #[test]
    fn parallel_collect_matches_the_sequential_report() {
        for limit in [1, 3, 64] {
            let report = crate::types::collect_parallel(&mock_registry(), limit);
            let ids: Vec<ProviderId> = report.providers.iter().map(|p| p.provider).collect();
            assert_eq!(ids, ProviderId::ALL.to_vec(), "limit {limit}");
        }
        let shared = crate::refresh::collect_with(
            &mock_registry_shared(),
            crate::refresh::CollectOptions::default(),
            Utc::now(),
            &|_| None,
        );
        let ids: Vec<ProviderId> = shared.report.providers.iter().map(|p| p.provider).collect();
        assert_eq!(ids, ProviderId::ALL.to_vec());
    }

    #[test]
    fn report_is_deterministic_within_the_same_minute() {
        let t = Utc::now();
        let a = serde_json::to_value(mock_report(t)).unwrap();
        let b = serde_json::to_value(mock_report(t)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn json_round_trips() {
        let report = mock_report(Utc::now());
        let json = report.to_json_pretty();
        let back: UsageReport = serde_json::from_str(&json).unwrap();
        // Timestamps are pinned to millisecond precision on the wire, so the
        // canonical re-serialisation is what has to match, not the in-memory value.
        assert_eq!(back.to_json_pretty(), json);
    }

    #[test]
    fn json_uses_the_documented_camel_case_contract() {
        let value: serde_json::Value = serde_json::from_str(&mock_json(Utc::now())).unwrap();
        let obj = value.as_object().unwrap();
        assert!(
            obj.contains_key("schemaVersion"),
            "top-level key must be schemaVersion"
        );
        assert!(obj.contains_key("generatedAt"));
        assert!(obj.contains_key("providers"));

        let codex = &value["providers"][0];
        assert_eq!(codex["provider"], "codex");
        assert_eq!(codex["windows"][0]["id"], "session");
        assert!(codex["windows"][0]["window"].get("usedPercent").is_some());
        assert!(codex["windows"][0]["window"].get("resetsAt").is_some());
        // Optional fields must be absent rather than null, so consumers can use
        // `Option` semantics on both sides.
        assert!(codex["windows"][0]["window"]
            .get("nextRegenPercent")
            .is_none());
        assert!(codex["windows"][0].get("usageKnown").is_none());
    }

    #[test]
    fn headline_prefers_the_most_constrained_lane() {
        let snap = mock_snapshot(ProviderId::Cursor, Scenario::NearlyExhausted, Utc::now());
        assert!(snap.headline_used_percent().unwrap() >= 72.0);
        assert!(snap.tray_label().starts_with("Cursor "));
    }

    #[test]
    fn demo_report_exercises_every_card_state() {
        // The UI has four card states; the default demo data must cover all of
        // them, otherwise screenshots only ever show the happy path.
        let report = mock_report(Utc::now());
        let states: std::collections::BTreeSet<&str> =
            report.providers.iter().map(|p| p.status.as_str()).collect();
        assert!(states.contains("ok"));
        assert!(states.contains("error"));
        assert!(states.contains("notConfigured"));

        let cursor = report.get(ProviderId::Cursor).unwrap();
        assert!(
            cursor.max_used_percent() >= 90.0,
            "Cursor is the near-limit sample"
        );

        let gemini = report.get(ProviderId::Gemini).unwrap();
        assert!(gemini.error.is_some(), "error cards must carry a message");

        let copilot = report.get(ProviderId::Copilot).unwrap();
        assert!(
            copilot.error.is_some(),
            "unconfigured cards must carry a setup hint"
        );
    }

    #[test]
    fn severity_thresholds_are_shared() {
        use crate::types::{severity_for_used_percent, Severity};
        assert_eq!(severity_for_used_percent(0.0), Severity::Healthy);
        assert_eq!(severity_for_used_percent(69.9), Severity::Healthy);
        assert_eq!(severity_for_used_percent(70.0), Severity::Warning);
        assert_eq!(severity_for_used_percent(89.9), Severity::Warning);
        assert_eq!(severity_for_used_percent(90.0), Severity::Critical);
        assert_eq!(severity_for_used_percent(140.0), Severity::Critical);
    }
}
