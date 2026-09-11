//! DeepSeek fixture tests — offline, deterministic.
//!
//! `FixtureClient` replays the recorded bodies in `tests/fixtures/deepseek/` and
//! records what the provider sent, so each test asserts on **the request** and
//! **the mapped snapshot** in the same place.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{DeepSeek, Env, HttpClient};

const API_KEY: &str = "sk-deepseek-0123456789abcdef";
const PLATFORM_TOKEN: &str = "platform-token-abcdefghij";

const BALANCE: &str = include_str!("fixtures/deepseek/balance.json");
const BALANCE_ZERO: &str = include_str!("fixtures/deepseek/balance-zero.json");
const BALANCE_UNAVAILABLE: &str = include_str!("fixtures/deepseek/balance-unavailable.json");
const PLATFORM_SUMMARY: &str = include_str!("fixtures/deepseek/platform-summary.json");
const PLATFORM_EXPIRED: &str = include_str!("fixtures/deepseek/platform-summary-expired.json");
const USAGE_AMOUNT: &str = include_str!("fixtures/deepseek/usage-amount.json");
const USAGE_COST: &str = include_str!("fixtures/deepseek/usage-cost.json");
const ERROR_401: &str = include_str!("fixtures/deepseek/error-401.json");

/// A fixed instant so every assertion is deterministic (`fetch` takes `now`).
fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> DeepSeek {
    DeepSeek::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn env_with_key() -> Env {
    Env::empty().with("DEEPSEEK_API_KEY", API_KEY)
}

#[test]
fn an_api_key_maps_the_balance_and_sends_nothing_else() {
    let client = fixture(vec![FixtureResponse::json(200, BALANCE)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::DeepSeek);
    assert_eq!(snapshot.title, "DeepSeek");
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(snapshot.status, FetchStatus::Ok);

    // Funded USD row wins even though CNY comes first.
    let balance = snapshot.balance.as_ref().expect("balance");
    assert_eq!(balance.amount, 18.4);
    assert_eq!(balance.currency, "USD");
    assert_eq!(
        balance.label.as_deref(),
        Some("$18.40 (Paid: $15.00 / Granted: $3.40)")
    );

    // Balance-only: no quota lane is synthesized.
    assert!(snapshot.windows.is_empty());

    // The account is the key, masked — never the key itself.
    assert_eq!(
        snapshot.account.as_deref(),
        Some(codexbar_providers::redact(API_KEY).as_str())
    );
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(API_KEY));

    // Without a Platform session the card says so, but stays `ok`.
    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(note.contains("DEEPSEEK_PLATFORM_TOKEN"), "{note}");

    assert_eq!(client.request_count(), 1);
    let requests = client.captured();
    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].path_ends_with("/user/balance"));
    assert_eq!(requests[0].url, "https://api.deepseek.com/user/balance");
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {API_KEY}").as_str())
    );
    assert_eq!(requests[0].header("Accept"), Some("application/json"));
}

#[test]
fn a_platform_session_adds_the_month_usage_lane() {
    let client = fixture(vec![
        FixtureResponse::json(200, BALANCE),
        FixtureResponse::json(200, USAGE_AMOUNT),
        FixtureResponse::json(200, USAGE_COST),
    ]);
    let env = env_with_key().with("DEEPSEEK_PLATFORM_TOKEN", PLATFORM_TOKEN);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.balance.as_ref().unwrap().amount, 18.4);

    assert_eq!(snapshot.windows.len(), 1);
    let usage = &snapshot.windows[0];
    assert_eq!(usage.id, "usage-month");
    assert_eq!(usage.window.window_minutes, Some(43_200));
    // Not a quota lane: the number is metadata, never a fabricated percentage.
    assert!(!usage.usage_known);
    assert_eq!(usage.window.used_percent, 0.0);
    assert_eq!(
        usage.window.reset_description.as_deref(),
        Some("This month: $0.68 · 10,000 tokens · 90 requests")
    );
    assert!(snapshot.error.is_none());

    // Balance, then amount, then cost.
    assert_eq!(client.request_count(), 3);
    let requests = client.captured();
    assert!(requests[1].path_ends_with("/api/v0/usage/amount"));
    assert!(requests[1].url.contains("month=9"));
    assert!(requests[1].url.contains("year=2026"));
    assert_eq!(
        requests[1].authorization().as_deref(),
        Some(format!("Bearer {PLATFORM_TOKEN}").as_str())
    );
    assert!(requests[2].path_ends_with("/api/v0/usage/cost"));
}

#[test]
fn a_platform_session_alone_supplies_the_balance() {
    let client = fixture(vec![
        FixtureResponse::json(200, PLATFORM_SUMMARY),
        FixtureResponse::json(200, USAGE_AMOUNT),
        FixtureResponse::json(200, USAGE_COST),
    ]);
    let env = Env::empty().with("DEEPSEEK_USER_TOKEN", PLATFORM_TOKEN);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    // Scraped from the dashboard, not an API key.
    assert_eq!(snapshot.source, DataSource::Web);
    let balance = snapshot.balance.as_ref().expect("platform balance");
    assert_eq!(balance.amount, 18.4);
    assert_eq!(balance.currency, "USD");
    assert_eq!(snapshot.windows.len(), 1);

    let requests = client.captured();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].path_ends_with("/api/v0/users/get_user_summary"));
}

#[test]
fn an_expired_platform_session_is_a_soft_note_when_the_api_key_still_works() {
    let client = fixture(vec![
        FixtureResponse::json(200, BALANCE),
        FixtureResponse::json(200, PLATFORM_EXPIRED),
    ]);
    let env = env_with_key().with("DEEPSEEK_PLATFORM_TOKEN", PLATFORM_TOKEN);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(
        snapshot.status,
        FetchStatus::Ok,
        "the balance is still real"
    );
    assert_eq!(snapshot.balance.as_ref().unwrap().amount, 18.4);
    assert!(snapshot.windows.is_empty());
    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(note.contains("Detailed usage unavailable"), "{note}");
}

#[test]
fn an_expired_platform_session_alone_is_an_error() {
    let client = fixture(vec![FixtureResponse::json(200, PLATFORM_EXPIRED)]);
    let env = Env::empty().with("DEEPSEEK_PLATFORM_TOKEN", PLATFORM_TOKEN);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("expired"), "{message}");
}

#[test]
fn a_zero_balance_says_add_credits() {
    let client = fixture(vec![FixtureResponse::json(200, BALANCE_ZERO)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    let balance = snapshot.balance.as_ref().expect("balance");
    assert_eq!(balance.amount, 0.0);
    assert!(balance
        .label
        .as_deref()
        .unwrap_or_default()
        .contains("add credits"));
}

#[test]
fn a_nonzero_but_unavailable_balance_says_so() {
    let client = fixture(vec![FixtureResponse::json(200, BALANCE_UNAVAILABLE)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    let balance = snapshot.balance.as_ref().expect("balance");
    assert_eq!(balance.amount, 5.0);
    assert_eq!(
        balance.label.as_deref(),
        Some("Balance unavailable for API calls")
    );
}

#[test]
fn missing_credentials_are_not_configured_and_send_nothing() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, Env::empty()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.balance.is_none());
    assert!(snapshot.account.is_none());
    let hint = snapshot.error.as_deref().unwrap_or_default();
    assert!(hint.contains("DEEPSEEK_API_KEY"), "{hint}");
    assert!(hint.contains("DEEPSEEK_KEY"), "{hint}");
    assert!(hint.contains("DEEPSEEK_PLATFORM_TOKEN"), "{hint}");
    assert!(hint.contains("%APPDATA%\\CodexBar\\config.json"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn the_secondary_key_alias_and_quoting_are_handled() {
    // `DEEPSEEK_KEY` is the documented alias; Windows users often paste quotes.
    let client = fixture(vec![FixtureResponse::json(200, BALANCE)]);
    let env = Env::empty().with("deepseek_key", format!("  \"{API_KEY}\"  "));
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(
        client.captured()[0].authorization().as_deref(),
        Some(format!("Bearer {API_KEY}").as_str())
    );

    // A blank value is treated as missing, not as a credential.
    let blank = provider(
        &fixture(vec![]),
        Env::empty().with("DEEPSEEK_API_KEY", "   "),
    );
    assert_eq!(blank.fetch(now()).status, FetchStatus::NotConfigured);
}

#[test]
fn a_rejected_key_becomes_an_error_snapshot_without_leaking_it() {
    let client = fixture(vec![FixtureResponse::json(401, ERROR_401)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("401"), "{message}");
    assert!(!message.contains(API_KEY));
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(API_KEY));
    assert_eq!(client.request_count(), 1);
}

#[test]
fn an_unexpected_body_shape_is_an_error_not_a_panic() {
    let client = fixture(vec![FixtureResponse::json(200, r#"{"is_available":true}"#)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);

    let client = fixture(vec![FixtureResponse::text(200, "<html>nope</html>")]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![FixtureResponse::json(200, BALANCE)]);
    let instant = now();
    let snapshot = provider(&client, env_with_key()).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
