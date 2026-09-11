//! OpenRouter fixture tests — the template a provider worker copies.
//!
//! Everything here is offline: `FixtureClient` replays recorded bodies (checked
//! into `tests/fixtures/openrouter/` as reviewable JSON) and records what the
//! provider sent, so each test can assert on **the request** and **the mapped
//! `RateWindow`s** in the same place.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::sync::Arc;

use chrono::{Duration, TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Env, HttpClient, HttpErrorKind, OpenRouter};

const API_KEY: &str = "sk-or-v1-0123456789abcdefghijklmnop";
const CREDITS: &str = include_str!("fixtures/openrouter/credits.json");
const KEY_LIMIT: &str = include_str!("fixtures/openrouter/key-limit.json");
const KEY_NO_LIMIT: &str = include_str!("fixtures/openrouter/key-no-limit.json");
const ERROR_401: &str = include_str!("fixtures/openrouter/error-401.json");
const ERROR_403_LEAKY: &str = include_str!("fixtures/openrouter/error-403-leaky.json");

/// A fixed instant so every assertion is deterministic (`fetch` takes `now`).
fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

/// Build a provider over a scripted client. The `Arc` is kept by the caller so
/// the test can still inspect what was sent after `fetch` consumed the provider.
fn provider(client: &Arc<FixtureClient>, env: Env) -> OpenRouter {
    OpenRouter::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn env_with_key() -> Env {
    Env::empty().with("OPENROUTER_API_KEY", API_KEY)
}

#[test]
fn happy_path_maps_credits_and_the_key_limit_to_windows() {
    let client = fixture(vec![
        FixtureResponse::json(200, CREDITS),
        FixtureResponse::json(200, KEY_LIMIT),
    ]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::OpenRouter);
    assert_eq!(snapshot.title, "OpenRouter");
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);

    // Balance = max(0, 100.0 - 37.5).
    let balance = snapshot.balance.as_ref().expect("credits balance");
    assert_eq!(balance.amount, 62.5);
    assert_eq!(balance.currency, "USD");
    assert_eq!(balance.label.as_deref(), Some("Credits remaining"));

    assert_eq!(snapshot.windows.len(), 2);
    let credits = &snapshot.windows[0];
    assert_eq!(credits.id, "credits");
    assert_eq!(credits.kind, WindowKind::Extra);
    assert_eq!(credits.window.used_percent, 37.5); // 37.5 / 100
    assert_eq!(credits.window.window_minutes, Some(43_200));

    let key = &snapshot.windows[1];
    assert_eq!(key.id, "key-limit-monthly");
    assert_eq!(key.kind, WindowKind::Extra);
    // (50 - 12.5) / 50 = 75 %
    assert_eq!(key.window.used_percent, 75.0);
    assert_eq!(key.window.window_minutes, Some(43_200));
    assert!(key.usage_known);

    // The account is the key, masked — never the key itself.
    assert_eq!(
        snapshot.account.as_deref(),
        Some(codexbar_providers::redact(API_KEY).as_str())
    );
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(API_KEY));

    // …and the tray/headline projections work on a balance-only card.
    assert_eq!(snapshot.max_used_percent(), 75.0);
}

#[test]
fn requests_are_bearer_authenticated_against_the_documented_endpoints() {
    let client = fixture(vec![
        FixtureResponse::json(200, CREDITS),
        FixtureResponse::json(200, KEY_NO_LIMIT),
    ]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);

    let requests = client.captured();
    assert_eq!(requests.len(), 2);

    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].path_ends_with("/api/v1/credits"));
    assert!(requests[0].sends_authorization_with("Bearer "));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {API_KEY}").as_str())
    );
    assert_eq!(requests[0].header("Accept"), Some("application/json"));
    // Attribution header defaults to the product name.
    assert_eq!(requests[0].header("X-Title"), Some("CodexBar"));

    assert!(requests[1].path_ends_with("/api/v1/key"));
    // The `/key` probe is capped at 1 s so it cannot hold up the refresh tick.
    assert!(requests[1].url.starts_with("https://"));
}

#[test]
fn a_slow_key_probe_degrades_softly_and_keeps_the_credits() {
    let client = fixture(vec![
        FixtureResponse::json(200, CREDITS),
        FixtureResponse::timeout(),
    ]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok, "credits are still real");
    assert_eq!(snapshot.balance.as_ref().unwrap().amount, 62.5);
    assert_eq!(
        snapshot.windows.len(),
        1,
        "only the credits lane is published"
    );
    assert_eq!(snapshot.windows[0].id, "credits");
    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(
        note.contains("API key limit unavailable right now (timeout)"),
        "{note}"
    );
    assert_eq!(client.request_count(), 2);
}

#[test]
fn a_key_without_a_budget_publishes_no_meter_and_no_note() {
    let client = fixture(vec![
        FixtureResponse::json(200, CREDITS),
        FixtureResponse::json(200, KEY_NO_LIMIT),
    ]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.error, None, "no limit configured is not a problem");
    assert_eq!(client.request_count(), 2);
}

#[test]
fn missing_credentials_are_not_configured_and_send_nothing() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, Env::empty()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.balance.is_none());
    assert!(snapshot.account.is_none());
    let hint = snapshot.error.as_deref().unwrap_or_default();
    assert!(hint.contains("OPENROUTER_API_KEY"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn a_blank_or_quoted_env_value_is_treated_as_missing_or_cleaned() {
    // `setx FOO "\"sk-...\""` is the common Windows footgun.
    let client = fixture(vec![
        FixtureResponse::json(200, CREDITS),
        FixtureResponse::json(200, KEY_NO_LIMIT),
    ]);
    let env = Env::empty().with("openrouter_api_key", "  \"sk-or-v1-quotedkeyvalue\"  ");
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(
        client.captured()[0].authorization().as_deref(),
        Some("Bearer sk-or-v1-quotedkeyvalue")
    );

    let blank = provider(
        &fixture(vec![]),
        Env::empty().with("OPENROUTER_API_KEY", "   "),
    );
    assert_eq!(blank.fetch(now()).status, FetchStatus::NotConfigured);
}

#[test]
fn an_http_override_fails_closed_before_the_bearer_is_attached() {
    let client = fixture(vec![]);
    let env = env_with_key().with("OPENROUTER_API_URL", "http://openrouter.ai/api/v1");
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("OPENROUTER_API_URL"), "{message}");
    assert!(message.contains("HTTPS"), "{message}");
    assert_eq!(
        client.request_count(),
        0,
        "a plaintext override must never receive the key"
    );
}

#[test]
fn an_https_override_is_followed() {
    let client = fixture(vec![
        FixtureResponse::json(200, CREDITS),
        FixtureResponse::json(200, KEY_NO_LIMIT),
    ]);
    let env = env_with_key().with("OPENROUTER_API_URL", "https://proxy.example.com/v1/");
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(client.captured()[0]
        .url
        .starts_with("https://proxy.example.com/v1/credits"));
}

#[test]
fn a_rejected_key_becomes_an_error_snapshot() {
    let client = fixture(vec![FixtureResponse::json(401, ERROR_401)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("401"), "{message}");
    assert!(!message.contains(API_KEY));
    // A failed credits call stops there: the optional probe is not attempted.
    assert_eq!(client.request_count(), 1);
}

#[test]
fn an_upstream_error_body_never_leaks_a_key() {
    // The 403 body itself contains another key; the error message must mask it.
    let client = fixture(vec![FixtureResponse::json(403, ERROR_403_LEAKY)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains("sk-or-v1-0123456789abcdefghijklmnop"));
    assert!(!rendered.contains("0123456789abcdefghijklmnop"));
    assert!(rendered.contains("[redacted]"));
    // The human-readable part of the message survives.
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("disabled"));
}

#[test]
fn an_unexpected_body_shape_is_an_error_not_a_panic() {
    let client = fixture(vec![FixtureResponse::json(200, r#"{"data":{}}"#)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("total_credits"));

    // A 200 with an HTML body (captive portal, proxy) is also handled.
    let client = fixture(vec![FixtureResponse::text(200, "<html>nope</html>")]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
}

#[test]
fn a_dead_client_is_reported_without_panicking() {
    let client = fixture(vec![FixtureResponse::failure(
        HttpErrorKind::Client,
        "could not initialise the HTTPS client",
    )]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("HTTPS client"));
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![
        FixtureResponse::json(200, CREDITS),
        FixtureResponse::json(200, KEY_NO_LIMIT),
    ]);
    let instant = now();
    let snapshot = provider(&client, env_with_key()).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
    // Nothing in the snapshot may be relative to the wall clock.
    assert!(instant + Duration::days(3650) > snapshot.fetched_at);
}
