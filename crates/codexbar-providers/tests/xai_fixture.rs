//! xAI fixture tests — offline, deterministic.
//!
//! `FixtureClient` replays `tests/fixtures/xai/` and records what the provider
//! sent. The balance endpoint's **inverted** string-cent ledger and the team-id
//! validation are the two behaviours that most deserve a test. Run with:
//! `cargo test -p codexbar-providers xai`

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Env, HttpClient, Xai};

const KEY: &str = "xai-management-fixture-key-0001";
const TEAM: &str = "team_fixture_0001";
const BALANCE: &str = include_str!("fixtures/xai/balance.json");
const USAGE: &str = include_str!("fixtures/xai/usage.json");
const ERROR_403: &str = include_str!("fixtures/xai/error-403.json");

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> Xai {
    Xai::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn env_with_credentials() -> Env {
    Env::empty()
        .with("XAI_MANAGEMENT_API_KEY", KEY)
        .with("XAI_TEAM_ID", TEAM)
}

#[test]
fn happy_path_maps_the_inverted_ledger_to_a_prepaid_balance() {
    let client = fixture(vec![
        FixtureResponse::json(200, BALANCE),
        FixtureResponse::json(200, USAGE),
    ]);
    let snapshot = provider(&client, env_with_credentials()).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::Xai);
    assert_eq!(snapshot.title, "xAI");
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.plan.as_deref(), Some("Management API"));

    // "-1234" cents is a $12.34 top-up; the balance is the negated value.
    let balance = snapshot.balance.as_ref().expect("prepaid balance");
    assert_eq!(balance.amount, 12.34);
    assert_eq!(balance.currency, "USD");
    assert_eq!(balance.label.as_deref(), Some("Prepaid credits"));

    // Prepaid money is not a quota: no session/weekly meters are synthesised.
    assert!(snapshot.windows.is_empty());

    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains(KEY));
}

#[test]
fn requests_are_bearer_authenticated_with_a_daily_usd_analytics_query() {
    let client = fixture(vec![
        FixtureResponse::json(200, BALANCE),
        FixtureResponse::json(200, USAGE),
    ]);
    provider(&client, env_with_credentials()).fetch(now());

    let requests = client.captured();
    assert_eq!(requests.len(), 2);

    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].path_ends_with(&format!("/v1/billing/teams/{TEAM}/prepaid/balance")));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(requests[0].url.starts_with("https://management-api.x.ai/"));

    assert_eq!(requests[1].method.as_str(), "POST");
    assert!(requests[1].path_ends_with(&format!("/v1/billing/teams/{TEAM}/prepaid/usage")));
    assert_eq!(requests[1].header("Content-Type"), Some("application/json"));
    let body = requests[1].body.as_deref().unwrap_or_default();
    assert!(body.contains("analyticsRequest"), "{body}");
    assert!(body.contains("AGGREGATION_SUM"), "{body}");
    assert!(body.contains("TIME_UNIT_DAY"), "{body}");
    assert!(body.contains("Etc/GMT"), "{body}");
}

#[test]
fn an_unparseable_balance_is_an_error_never_zero_dollars() {
    let client = fixture(vec![FixtureResponse::json(
        200,
        r#"{"total":{"val":"not-a-number"}}"#,
    )]);
    let snapshot = provider(&client, env_with_credentials()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot.balance.is_none());
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("cent amount"));
    // A failed balance stops before the optional history probe.
    assert_eq!(client.request_count(), 1);
}

#[test]
fn a_usage_failure_keeps_the_balance_and_adds_a_note() {
    let client = fixture(vec![
        FixtureResponse::json(200, BALANCE),
        FixtureResponse::timeout(),
    ]);
    let snapshot = provider(&client, env_with_credentials()).fetch(now());

    assert_eq!(
        snapshot.status,
        FetchStatus::Ok,
        "the balance is still real"
    );
    assert_eq!(snapshot.balance.as_ref().unwrap().amount, 12.34);
    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(
        note.contains("spend history unavailable right now (timeout)"),
        "{note}"
    );
    assert_eq!(client.request_count(), 2);
}

#[test]
fn a_partial_analytics_result_is_labelled() {
    let partial = r#"{"timeSeries":[],"limitReached":true}"#;
    let client = fixture(vec![
        FixtureResponse::json(200, BALANCE),
        FixtureResponse::json(200, partial),
    ]);
    let snapshot = provider(&client, env_with_credentials()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(note.contains("partial"), "{note}");
}

#[test]
fn a_rejected_usage_key_errors_without_publishing_a_balance() {
    let client = fixture(vec![
        FixtureResponse::json(200, BALANCE),
        FixtureResponse::json(403, ERROR_403),
    ]);
    let snapshot = provider(&client, env_with_credentials()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot.balance.is_none());
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("rejected the Management API key"));
}

#[test]
fn a_404_names_the_team_id_as_the_likely_cause() {
    let client = fixture(vec![FixtureResponse::json(404, "{}")]);
    let snapshot = provider(&client, env_with_credentials()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("team ID"));
}

#[test]
fn a_429_is_reported_as_rate_limited() {
    let client = fixture(vec![FixtureResponse::json(429, "{}")]);
    let snapshot = provider(&client, env_with_credentials()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("rate limit"));
}

#[test]
fn a_403_on_balance_names_the_management_key() {
    let client = fixture(vec![FixtureResponse::json(403, ERROR_403)]);
    let snapshot = provider(&client, env_with_credentials()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("Management API key"), "{message}");
    assert!(
        message.contains("inference API keys are not accepted"),
        "{message}"
    );
}

#[test]
fn missing_credentials_are_not_configured_and_send_nothing() {
    // No key at all.
    let client = fixture(vec![]);
    let snapshot = provider(&client, Env::empty()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.balance.is_none());
    let hint = snapshot.error.as_deref().unwrap_or_default();
    assert!(hint.contains("XAI_MANAGEMENT_API_KEY"), "{hint}");
    assert_eq!(client.request_count(), 0);

    // Key present, team missing.
    let client = fixture(vec![]);
    let env = Env::empty().with("XAI_MANAGEMENT_API_KEY", KEY);
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("XAI_TEAM_ID"));
    assert_eq!(client.request_count(), 0);

    // Team id with a path separator is refused before any request.
    let client = fixture(vec![]);
    let env = env_with_credentials().with("XAI_TEAM_ID", "a/b");
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("path separators"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![
        FixtureResponse::json(200, BALANCE),
        FixtureResponse::json(200, USAGE),
    ]);
    let instant = now();
    let snapshot = provider(&client, env_with_credentials()).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
