//! Groq fixture tests — offline, deterministic.
//!
//! `FixtureClient` replays the recorded bodies in `tests/fixtures/groq/` and
//! records what the provider sent, so each test asserts on **the request** and
//! **the mapped snapshot** in the same place.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Env, Groq, HttpClient};

const API_KEY: &str = "gsk_0123456789abcdefghijklmnop";
const SESSION_TOKEN: &str = "stytch-session-opaque-token-value";

const STYTCH_AUTH: &str = include_str!("fixtures/groq/stytch-authenticate.json");
const ACTIVITY: &str = include_str!("fixtures/groq/activity.json");
const ACTIVITY_EMPTY: &str = include_str!("fixtures/groq/activity-empty.json");
const PROM_REQUESTS: &str = include_str!("fixtures/groq/prometheus-requests.json");
const PROM_TOKENS_IN: &str = include_str!("fixtures/groq/prometheus-tokens-in.json");
const PROM_TOKENS_OUT: &str = include_str!("fixtures/groq/prometheus-tokens-out.json");
const PROM_CACHE: &str = include_str!("fixtures/groq/prometheus-cache-hits.json");
const ERROR_404: &str = include_str!("fixtures/groq/error-404.json");
const ERROR_403: &str = include_str!("fixtures/groq/error-403.json");

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> Groq {
    Groq::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

/// The JWT the Stytch fixture hands back (carries the `org_test123` claim).
fn fixture_jwt() -> String {
    serde_json::from_str::<serde_json::Value>(STYTCH_AUTH)
        .unwrap()
        .pointer("/data/session_jwt")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string()
}

fn env_with_token() -> Env {
    Env::empty().with("GROQ_SESSION_TOKEN", SESSION_TOKEN)
}

fn env_with_key() -> Env {
    Env::empty().with("GROQ_API_KEY", API_KEY)
}

#[test]
fn a_console_session_refreshes_the_jwt_then_maps_daily_activity() {
    let client = fixture(vec![
        FixtureResponse::json(200, STYTCH_AUTH),
        FixtureResponse::json(200, ACTIVITY),
    ]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::Groq);
    assert_eq!(snapshot.title, "Groq");
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.source, DataSource::Web);
    assert_eq!(snapshot.account.as_deref(), Some("Acme Labs"));

    // The two lanes carry the numbers as metadata, never a fabricated percentage.
    assert_eq!(snapshot.windows.len(), 2);
    let daily = &snapshot.windows[0];
    assert_eq!(daily.id, "daily");
    assert_eq!(daily.kind, WindowKind::Extra);
    assert!(!daily.usage_known);
    assert_eq!(daily.window.used_percent, 0.0);
    assert_eq!(daily.window.window_minutes, Some(1_440));
    assert_eq!(
        daily.window.reset_description.as_deref(),
        Some("Today: 7,000 tokens · $0.10 · 90 requests")
    );

    let activity = &snapshot.windows[1];
    assert_eq!(activity.id, "activity");
    assert!(!activity.usage_known);
    assert_eq!(activity.window.window_minutes, Some(43_200));
    assert_eq!(
        activity.window.reset_description.as_deref(),
        Some("Last 30 days: 21,000 tokens · $0.22 · 210 requests")
    );

    // The headline projections never invent a quota.
    assert_eq!(snapshot.max_used_percent(), 0.0);
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains(SESSION_TOKEN));
}

#[test]
fn requests_match_the_documented_stytch_then_activity_shape() {
    let client = fixture(vec![
        FixtureResponse::json(200, STYTCH_AUTH),
        FixtureResponse::json(200, ACTIVITY),
    ]);
    let snapshot = provider(&client, env_with_token()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);

    let requests = client.captured();
    assert_eq!(requests.len(), 2);

    // 1. Stytch B2B exchange: POST with Basic base64(publicToken:sessionToken).
    let stytch = &requests[0];
    assert_eq!(stytch.method.as_str(), "POST");
    assert_eq!(
        stytch.url,
        "https://api.stytchb2b.groq.com/sdk/v1/b2b/sessions/authenticate"
    );
    assert!(stytch.sends_authorization_with("Basic "));
    assert_eq!(stytch.header("Content-Type"), Some("application/json"));
    assert_eq!(stytch.header("Origin"), Some("https://console.groq.com"));
    assert_eq!(
        stytch.header("X-SDK-Parent-Host"),
        Some("https://console.groq.com")
    );
    assert!(stytch.header("X-SDK-Client").is_some());
    let body: serde_json::Value = stytch.json_body().unwrap();
    assert_eq!(
        body.get("session_token").and_then(|v| v.as_str()),
        Some(SESSION_TOKEN)
    );

    // 2. Activity: GET the org route on the host root, bearer-authenticated.
    let activity = &requests[1];
    assert_eq!(activity.method.as_str(), "GET");
    assert!(activity
        .url
        .starts_with("https://api.groq.com/platform/v1/organizations/org_test123/activity?"));
    assert!(activity.url.contains("start_date="));
    assert!(activity.url.contains("end_date="));
    assert_eq!(
        activity.authorization().as_deref(),
        Some(format!("Bearer {}", fixture_jwt()).as_str())
    );
    assert_eq!(activity.header("Accept"), Some("application/json"));

    // The secret never reaches the rendered snapshot.
    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains(SESSION_TOKEN));
}

#[test]
fn a_direct_jwt_skips_the_stytch_refresh() {
    let client = fixture(vec![FixtureResponse::json(200, ACTIVITY)]);
    let env = Env::empty().with("GROQ_SESSION_JWT", fixture_jwt());
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.source, DataSource::Web);
    assert_eq!(client.request_count(), 1, "no refresh for a direct JWT");
    assert!(client.captured()[0]
        .url
        .contains("/platform/v1/organizations/org_test123/activity"));
}

#[test]
fn an_empty_activity_window_is_ok_and_publishes_no_lane() {
    let client = fixture(vec![
        FixtureResponse::json(200, STYTCH_AUTH),
        FixtureResponse::json(200, ACTIVITY_EMPTY),
    ]);
    let snapshot = provider(&client, env_with_token()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(snapshot.windows.is_empty());
}

#[test]
fn an_enterprise_api_key_uses_prometheus() {
    let client = fixture(vec![
        FixtureResponse::json(200, PROM_REQUESTS),
        FixtureResponse::json(200, PROM_TOKENS_IN),
        FixtureResponse::json(200, PROM_TOKENS_OUT),
        FixtureResponse::json(200, PROM_CACHE),
    ]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(
        snapshot.account.as_deref(),
        Some(codexbar_providers::redact(API_KEY).as_str())
    );

    assert_eq!(snapshot.windows.len(), 3);
    assert_eq!(snapshot.windows[0].id, "requests");
    assert!(!snapshot.windows[0].usage_known);
    assert_eq!(
        snapshot.windows[0].window.reset_description.as_deref(),
        Some("2550 req/min")
    );
    assert_eq!(snapshot.windows[1].id, "tokens");
    assert_eq!(
        snapshot.windows[1].window.reset_description.as_deref(),
        Some("120045 tok/min")
    );
    assert_eq!(snapshot.windows[2].id, "cache");
    assert_eq!(
        snapshot.windows[2].window.reset_description.as_deref(),
        Some("720 cache/min")
    );

    let requests = client.captured();
    assert_eq!(requests.len(), 4);
    assert!(requests[0]
        .url
        .starts_with("https://api.groq.com/v1/metrics/prometheus/api/v1/query?query="));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {API_KEY}").as_str())
    );
    assert!(requests[0].url.contains("requests:rate5m"));

    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains(API_KEY));
}

#[test]
fn a_standard_key_gets_404_from_prometheus_and_reports_it() {
    let client = fixture(vec![FixtureResponse::json(404, ERROR_404)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("404"), "{message}");
}

#[test]
fn an_https_override_is_followed_for_the_api_base() {
    let client = fixture(vec![
        FixtureResponse::json(200, PROM_REQUESTS),
        FixtureResponse::json(200, PROM_TOKENS_IN),
        FixtureResponse::json(200, PROM_TOKENS_OUT),
        FixtureResponse::json(200, PROM_CACHE),
    ]);
    let env = env_with_key().with("GROQ_API_URL", "https://proxy.example.com/v1/");
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(client.captured()[0]
        .url
        .starts_with("https://proxy.example.com/v1/metrics/prometheus/api/v1/query"));
}

#[test]
fn a_plaintext_override_fails_closed_before_the_bearer_is_attached() {
    let client = fixture(vec![]);
    let env = env_with_key().with("GROQ_API_URL", "http://api.groq.com/v1");
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("GROQ_API_URL"), "{message}");
    assert!(message.contains("HTTPS"), "{message}");
    assert_eq!(client.request_count(), 0);
}

#[test]
fn a_session_without_an_organization_claim_is_an_error() {
    // A well-formed JWT whose payload has no Groq organization claim.
    let jwt = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyIn0.sig";
    let client = fixture(vec![]);
    let env = Env::empty().with("GROQ_SESSION_JWT", jwt);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("organization"));
    assert_eq!(client.request_count(), 0, "no org, no activity call");
}

#[test]
fn missing_credentials_are_not_configured_and_send_nothing() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, Env::empty()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.account.is_none());
    let hint = snapshot.error.as_deref().unwrap_or_default();
    assert!(hint.contains("GROQ_SESSION_TOKEN"), "{hint}");
    assert!(hint.contains("GROQ_SESSION_JWT"), "{hint}");
    assert!(hint.contains("GROQ_API_KEY"), "{hint}");
    assert!(hint.contains("%APPDATA%\\CodexBar\\config.json"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn a_rejected_console_session_is_an_error_without_leaking_it() {
    let client = fixture(vec![FixtureResponse::json(403, ERROR_403)]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains(SESSION_TOKEN));
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![
        FixtureResponse::json(200, STYTCH_AUTH),
        FixtureResponse::json(200, ACTIVITY),
    ]);
    let instant = now();
    let snapshot = provider(&client, env_with_token()).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
