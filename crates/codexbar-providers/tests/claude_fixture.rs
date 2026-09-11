//! Claude fixture tests — offline, deterministic, and read-only.
//!
//! `FixtureClient` replays recorded bodies (checked into `tests/fixtures/claude/`)
//! and records what the provider sent, so each test asserts on **the request** and
//! on **the mapped `RateWindow`s** in one place. The credential file is a fixture on
//! disk too, so the real `%USERPROFILE%\.claude\.credentials.json` is never read.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{ClaudeOAuth, Env, HttpClient};

const USAGE: &str = include_str!("fixtures/claude/usage.json");
const USAGE_NULL_SESSION: &str = include_str!("fixtures/claude/usage-five-hour-null.json");
const PROFILE: &str = include_str!("fixtures/claude/profile.json");
const ERROR_401: &str = include_str!("fixtures/claude/error-401.json");
const ERROR_403_SCOPE: &str = include_str!("fixtures/claude/error-403-scope.json");

const ACCESS_TOKEN: &str = "sk-ant-oat01-FIXTURE-ACCESS-TOKEN-000000000000";

/// A fixed instant so every assertion is deterministic (`fetch` takes `now`).
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn fixture_dir(name: &str) -> String {
    format!(
        "{}/tests/fixtures/claude/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// An environment whose Claude config root points at a fixture directory — no
/// real credential file, no real home.
fn env_at(name: &str) -> Env {
    Env::empty()
        .with("CLAUDE_SECURESTORAGE_CONFIG_DIR", fixture_dir(name))
        .with("CODEXBAR_CLAUDE_CODE_VERSION", "2.1.258")
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> ClaudeOAuth {
    ClaudeOAuth::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn window<'a>(
    snapshot: &'a codexbar_core::ProviderSnapshot,
    id: &str,
) -> &'a codexbar_core::NamedRateWindow {
    snapshot
        .windows
        .iter()
        .find(|w| w.id == id)
        .unwrap_or_else(|| panic!("window {id} missing; have {:?}", ids(snapshot)))
}

fn ids(snapshot: &codexbar_core::ProviderSnapshot) -> Vec<&str> {
    snapshot.windows.iter().map(|w| w.id.as_str()).collect()
}

#[test]
fn happy_path_maps_session_weekly_and_scoped_windows() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE),
        FixtureResponse::json(200, PROFILE),
    ]);
    let snapshot = provider(&client, env_at("valid")).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::Claude);
    assert_eq!(snapshot.title, "Claude");
    assert_eq!(snapshot.source, DataSource::OAuth);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);

    // Identity comes from `/profile` (masked: the domain never reaches the
    // card); the plan label from the credential file.
    assert_eq!(snapshot.account.as_deref(), Some("user@…"));
    assert_eq!(snapshot.plan.as_deref(), Some("Max 20x"));

    assert_eq!(
        ids(&snapshot),
        vec![
            "session",
            "weekly",
            "weekly-sonnet",
            "claude-weekly-scoped-claude-fable",
            "extra-usage"
        ]
    );

    let session = window(&snapshot, "session");
    assert_eq!(session.kind, WindowKind::Session);
    assert_eq!(session.window.used_percent, 42.0);
    assert_eq!(session.window.window_minutes, Some(300));
    assert!(!session.window.is_synthetic_placeholder);
    assert!(session.window.resets_at.is_some());

    let weekly = window(&snapshot, "weekly");
    assert_eq!(weekly.kind, WindowKind::Weekly);
    assert_eq!(weekly.window.used_percent, 63.5);
    assert_eq!(weekly.window.window_minutes, Some(10_080));

    let sonnet = window(&snapshot, "weekly-sonnet");
    assert_eq!(sonnet.kind, WindowKind::WeeklyScoped);
    assert_eq!(sonnet.window.used_percent, 12.5);

    // `limits[].weekly_scoped` names its model; the generic "All models" scope
    // stays in the main weekly row and must not become a duplicate lane.
    let fable = window(&snapshot, "claude-weekly-scoped-claude-fable");
    assert_eq!(fable.kind, WindowKind::WeeklyScoped);
    assert_eq!(fable.window.used_percent, 8.0);
    assert_eq!(fable.title, "Weekly · Fable");
    assert!(!snapshot.windows.iter().any(|w| w.id.contains("all-models")));

    assert_eq!(window(&snapshot, "extra-usage").kind, WindowKind::Extra);

    assert_eq!(snapshot.headline_window().unwrap().id, "session");
    assert_eq!(snapshot.max_used_percent(), 63.5);

    // No credential value may reach the payload.
    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains(ACCESS_TOKEN));
    assert!(!rendered.contains("FIXTURE-REFRESH"));
}

#[test]
fn requests_carry_the_bearer_beta_header_and_claude_code_user_agent() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE),
        FixtureResponse::json(200, PROFILE),
    ]);
    let snapshot = provider(&client, env_at("valid")).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);

    let requests = client.captured();
    assert_eq!(requests.len(), 2);

    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].path_ends_with("/api/oauth/usage"));
    assert!(requests[0].url.starts_with("https://api.anthropic.com/"));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {ACCESS_TOKEN}").as_str())
    );
    assert_eq!(
        requests[0].header("anthropic-beta"),
        Some("oauth-2025-04-20")
    );
    assert_eq!(requests[0].header("Accept"), Some("application/json"));
    assert_eq!(
        requests[0].header("User-Agent"),
        Some("claude-code/2.1.258")
    );

    assert!(requests[1].path_ends_with("/api/oauth/profile"));
    assert_eq!(
        requests[1].header("anthropic-beta"),
        Some("oauth-2025-04-20")
    );
    assert!(requests[1].sends_authorization_with("Bearer "));
}

#[test]
fn a_null_five_hour_is_a_synthetic_placeholder_not_a_real_session() {
    let client = fixture(vec![FixtureResponse::json(200, USAGE_NULL_SESSION)]);
    let snapshot = provider(&client, env_at("valid")).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);

    let session = window(&snapshot, "session");
    assert!(
        session.window.is_synthetic_placeholder,
        "a null five_hour is a lane that does not exist, not a real 0 % session"
    );
    assert_eq!(session.window.used_percent, 0.0);

    let weekly = window(&snapshot, "weekly");
    assert_eq!(weekly.window.used_percent, 71.0);

    // The phantom session must not drive the headline or the tray metric.
    assert_eq!(snapshot.headline_window().unwrap().id, "weekly");
    assert_eq!(snapshot.headline_used_percent(), Some(71.0));
    assert_eq!(snapshot.max_used_percent(), 71.0);
}

#[test]
fn an_expired_token_is_reported_honestly_without_a_request_or_a_write() {
    let path = format!("{}/.credentials.json", fixture_dir("expired"));
    let before = std::fs::read(&path).expect("expired fixture");

    let client = fixture(vec![]);
    let snapshot = provider(&client, env_at("expired")).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("tokenExpired"), "{message}");
    assert!(message.contains("expired"), "{message}");
    assert!(message.contains("re-authenticate"), "{message}");
    // The plan is still provable from the credential file itself.
    assert_eq!(snapshot.plan.as_deref(), Some("Max 20x"));
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.account.is_none());
    assert_eq!(
        client.request_count(),
        0,
        "an expired or revoked token must never be spent on a doomed request"
    );

    // Read-only means byte-identical: no refresh, no rewrite, no sidecar.
    let after = std::fs::read(&path).expect("expired fixture still present");
    assert_eq!(before, after, "the provider must never rewrite credentials");

    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains("EXPIRED-ACCESS-TOKEN"));
    assert!(!rendered.contains("sk-ant-ort01"));
}

#[test]
fn a_missing_credential_file_is_not_configured_and_sends_nothing() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, env_at("does-not-exist")).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert_eq!(snapshot.source, DataSource::OAuth);
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.account.is_none());
    let hint = snapshot.error.as_deref().unwrap_or_default();
    assert!(hint.contains("claude"), "{hint}");
    assert!(hint.contains("Run `claude`"), "{hint}");
    assert_eq!(client.request_count(), 0);
}

#[test]
fn no_resolvable_home_is_not_configured_without_touching_disk() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, Env::empty()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot.windows.is_empty());
    assert_eq!(client.request_count(), 0);
}

#[test]
fn claude_config_dir_is_used_when_no_secure_storage_root_is_set() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE),
        FixtureResponse::json(200, PROFILE),
    ]);
    let env = Env::empty().with("CLAUDE_CONFIG_DIR", fixture_dir("valid"));
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    // The domain never reaches the card: the identity is masked.
    assert_eq!(snapshot.account.as_deref(), Some("user@…"));
}

#[test]
fn an_mcp_only_credential_file_is_a_configuration_error_not_missing_credentials() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, env_at("mcp-only")).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("claudeAiOauth"), "{message}");
    assert_eq!(client.request_count(), 0);
}

#[test]
fn a_revoked_token_becomes_an_actionable_error_without_leaking_it() {
    let client = fixture(vec![FixtureResponse::json(401, ERROR_401)]);
    let snapshot = provider(&client, env_at("valid")).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("re-authenticate"), "{message}");
    assert!(!message.contains(ACCESS_TOKEN));
    // A failed usage call stops there; the optional profile call is not attempted.
    assert_eq!(client.request_count(), 1);
}

#[test]
fn a_missing_user_profile_scope_is_named_explicitly() {
    let client = fixture(vec![FixtureResponse::json(403, ERROR_403_SCOPE)]);
    let snapshot = provider(&client, env_at("valid")).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("user:profile"), "{message}");
    assert_eq!(client.request_count(), 1);
}

#[test]
fn a_failed_profile_call_keeps_the_real_usage() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE),
        FixtureResponse::timeout(),
    ]);
    let snapshot = provider(&client, env_at("valid")).fetch(now());

    // Identity is enrichment: losing it must not blank the real quota windows.
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(!snapshot.windows.is_empty());
    assert!(snapshot.account.is_none());
    assert_eq!(snapshot.plan.as_deref(), Some("Max 20x"));
    assert_eq!(client.request_count(), 2);
}

#[test]
fn an_empty_usage_body_is_an_error_not_a_panic() {
    let client = fixture(vec![FixtureResponse::json(200, "{}")]);
    let snapshot = provider(&client, env_at("valid")).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot.windows.is_empty());
    assert_eq!(client.request_count(), 1);
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE),
        FixtureResponse::json(200, PROFILE),
    ]);
    let instant = now();
    let snapshot = provider(&client, env_at("valid")).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
