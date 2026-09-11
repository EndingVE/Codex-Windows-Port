//! Kimi fixture tests — offline, deterministic.
//!
//! `FixtureClient` replays the recorded bodies in `tests/fixtures/kimi/` and
//! records what the provider sent, so each test asserts on **the request** and
//! **the mapped windows** in the same place. Run with:
//! `cargo test -p codexbar-providers kimi`

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Env, HttpClient, HttpErrorKind, Kimi};

const API_KEY: &str = "kimi-code-fixture-key-0001";
const WEB_TOKEN: &str = "kimi-web-fixture-token-0002";
const CODE_USAGES: &str = include_str!("fixtures/kimi/code-usages.json");
const WEB_USAGES: &str = include_str!("fixtures/kimi/web-usages.json");
const STATS: &str = include_str!("fixtures/kimi/subscription-stats.json");
const ERROR_401: &str = include_str!("fixtures/kimi/error-401.json");

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> Kimi {
    Kimi::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn env_with_key() -> Env {
    Env::empty().with("KIMI_CODE_API_KEY", API_KEY)
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < 1e-9
}

#[test]
fn happy_path_maps_code_quota_and_membership_stats_to_windows() {
    let client = fixture(vec![
        FixtureResponse::json(200, CODE_USAGES),
        FixtureResponse::json(200, STATS),
    ]);
    let env = env_with_key().with("KIMI_AUTH_TOKEN", WEB_TOKEN);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::Kimi);
    assert_eq!(snapshot.title, "Kimi");
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.plan.as_deref(), Some("Moderato"));
    assert_eq!(
        snapshot.account.as_deref(),
        Some(codexbar_providers::redact(API_KEY).as_str())
    );

    assert_eq!(snapshot.windows.len(), 4);

    let weekly = &snapshot.windows[0];
    assert_eq!(weekly.id, "kimi-weekly");
    assert_eq!(weekly.kind, WindowKind::Weekly);
    assert!(close(weekly.window.used_percent, 214.0 / 2048.0 * 100.0));
    assert_eq!(weekly.window.window_minutes, Some(10_080));
    assert!(weekly.window.resets_at.is_some());
    assert_eq!(
        weekly.window.reset_description.as_deref(),
        Some("214/2048 requests")
    );

    let rate = &snapshot.windows[1];
    assert_eq!(rate.id, "kimi-rate");
    assert_eq!(rate.kind, WindowKind::Session);
    assert!(close(rate.window.used_percent, 69.5));
    assert_eq!(rate.window.window_minutes, Some(300));
    assert_eq!(
        rate.window.reset_description.as_deref(),
        Some("Rate: 139/200 per 5 hours")
    );

    let monthly = &snapshot.windows[2];
    assert_eq!(monthly.id, "kimi-monthly");
    assert_eq!(monthly.kind, WindowKind::Extra);
    assert!(close(monthly.window.used_percent, 42.0));
    assert_eq!(monthly.window.window_minutes, Some(43_200));

    let code7d = &snapshot.windows[3];
    assert_eq!(code7d.id, "kimi-code-7d");
    assert_eq!(code7d.kind, WindowKind::Weekly);
    assert!(close(code7d.window.used_percent, 30.0));

    // Neither credential may reach the serialised payload.
    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains(API_KEY));
    assert!(!rendered.contains(WEB_TOKEN));
}

#[test]
fn requests_are_bearer_authenticated_against_the_documented_endpoints() {
    let client = fixture(vec![
        FixtureResponse::json(200, CODE_USAGES),
        FixtureResponse::json(200, STATS),
    ]);
    let env = env_with_key().with("KIMI_AUTH_TOKEN", WEB_TOKEN);
    provider(&client, env).fetch(now());

    let requests = client.captured();
    assert_eq!(requests.len(), 2);

    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].path_ends_with("/coding/v1/usages"));
    assert!(requests[0].url.starts_with("https://api.kimi.com/"));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {API_KEY}").as_str())
    );
    assert_eq!(requests[0].header("X-Msh-Platform"), Some("kimi_code_cli"));
    assert_eq!(requests[0].header("Accept"), Some("application/json"));

    assert_eq!(requests[1].method.as_str(), "POST");
    assert!(requests[1].path_ends_with("/GetSubscriptionStats"));
    assert!(requests[1].sends_authorization_with("Bearer "));
    assert_eq!(requests[1].header("Content-Type"), Some("application/json"));
    assert_eq!(requests[1].header("Origin"), Some("https://www.kimi.com"));
    assert!(requests[1]
        .header("Cookie")
        .unwrap_or_default()
        .contains("kimi-auth="));
    assert_eq!(requests[1].body.as_deref(), Some("{}"));
}

#[test]
fn an_api_key_alone_publishes_the_quota_without_enrichment() {
    let client = fixture(vec![FixtureResponse::json(200, CODE_USAGES)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.windows.len(), 2);
    assert_eq!(snapshot.windows[0].id, "kimi-weekly");
    assert_eq!(snapshot.windows[1].id, "kimi-rate");
    assert_eq!(snapshot.plan.as_deref(), Some("Moderato"));
    assert_eq!(client.request_count(), 1, "no web token, no stats call");
}

#[test]
fn a_web_token_drives_the_web_billing_quota() {
    let client = fixture(vec![
        FixtureResponse::json(200, WEB_USAGES),
        FixtureResponse::json(200, STATS),
    ]);
    let env = Env::empty().with("KIMI_AUTH_TOKEN", WEB_TOKEN);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(
        snapshot.account.as_deref(),
        Some(codexbar_providers::redact(WEB_TOKEN).as_str())
    );
    assert_eq!(snapshot.windows.len(), 4);
    assert!(close(snapshot.windows[0].window.used_percent, 25.0));
    assert_eq!(snapshot.windows[0].window.window_minutes, Some(10_080));
    // `duration: 5 TIME_UNIT_HOUR` maps to 300 minutes.
    assert_eq!(snapshot.windows[1].window.window_minutes, Some(300));

    let requests = client.captured();
    assert_eq!(requests[0].method.as_str(), "POST");
    assert!(requests[0].path_ends_with("/GetUsages"));
    let body = requests[0].body.as_deref().unwrap_or_default();
    assert!(body.contains("FEATURE_CODING"), "{body}");
    assert!(requests[0]
        .header("Cookie")
        .unwrap_or_default()
        .contains("kimi-auth="));
    assert_eq!(requests[0].header("Origin"), Some("https://www.kimi.com"));
    assert_eq!(
        requests[0].header("Referer"),
        Some("https://www.kimi.com/code/console")
    );
}

#[test]
fn a_moonshot_key_is_accepted_as_a_last_resort_alias() {
    let client = fixture(vec![FixtureResponse::json(200, CODE_USAGES)]);
    let env = Env::empty().with("MOONSHOT_API_KEY", API_KEY);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(
        client.captured()[0].authorization().as_deref(),
        Some(format!("Bearer {API_KEY}").as_str())
    );
}

#[test]
fn missing_credentials_are_not_configured_and_send_nothing() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, Env::empty()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.account.is_none());
    let hint = snapshot.error.as_deref().unwrap_or_default();
    assert!(hint.contains("KIMI_CODE_API_KEY"), "{hint}");
    assert!(hint.contains("KIMI_AUTH_TOKEN"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn a_rejected_key_becomes_an_error_snapshot() {
    let client = fixture(vec![FixtureResponse::json(401, ERROR_401)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("invalid or expired"), "{message}");
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(API_KEY));
    assert_eq!(client.request_count(), 1);
}

#[test]
fn a_failed_enrichment_keeps_the_code_quota_and_adds_a_note() {
    let client = fixture(vec![
        FixtureResponse::json(200, CODE_USAGES),
        FixtureResponse::timeout(),
    ]);
    let env = env_with_key().with("KIMI_AUTH_TOKEN", WEB_TOKEN);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok, "the Code quota is real");
    assert_eq!(snapshot.windows.len(), 2);
    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(
        note.contains("subscription stats unavailable right now (timeout)"),
        "{note}"
    );
    assert_eq!(client.request_count(), 2);
}

#[test]
fn a_plaintext_override_fails_closed_before_the_bearer_is_attached() {
    let client = fixture(vec![]);
    let env = env_with_key().with("KIMI_CODE_BASE_URL", "http://api.kimi.com");
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("KIMI_CODE_BASE_URL"), "{message}");
    assert!(message.contains("HTTPS"), "{message}");
    assert_eq!(
        client.request_count(),
        0,
        "a plaintext override must never receive the key"
    );
}

#[test]
fn an_https_override_is_followed() {
    let client = fixture(vec![FixtureResponse::json(200, CODE_USAGES)]);
    let env = env_with_key().with("KIMI_CODE_BASE_URL", "https://proxy.example.com");
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(client.captured()[0]
        .url
        .starts_with("https://proxy.example.com/coding/v1/usages"));
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
        .contains("usage"));

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
fn the_device_id_header_is_read_from_disk_but_never_created() {
    let dir = std::env::temp_dir().join(format!("codexbar-kimi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("device_id"), "device-fixture-123\n").unwrap();

    let env = env_with_key().with("KIMI_CODE_HOME", dir.to_string_lossy().to_string());
    let client = fixture(vec![FixtureResponse::json(200, CODE_USAGES)]);
    provider(&client, env).fetch(now());
    assert_eq!(
        client.captured()[0].header("X-Msh-Device-Id"),
        Some("device-fixture-123")
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![FixtureResponse::json(200, CODE_USAGES)]);
    let instant = now();
    let snapshot = provider(&client, env_with_key()).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
