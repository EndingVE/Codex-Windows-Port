//! MiniMax fixture tests — the whole provider, offline.
//!
//! `FixtureClient` replays recorded bodies (checked into `tests/fixtures/minimax/`)
//! and records what the provider sent, so every test asserts on **the request**
//! and **the mapped windows** in the same place. `MINIMAX_API_KEY` is set on the
//! machine these run on; it is never read here — every `Env` is injected, and no
//! test touches the network.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::providers::minimax::MiniMaxRegion;
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Env, HttpClient, MiniMax};

const CODING_KEY: &str = "«redacted:sk-cp-…»";
const STANDARD_KEY: &str = "«redacted:sk-api-…»";
const REMAINS: &str = include_str!("fixtures/minimax/remains.json");
const SIGNED_OUT: &str = include_str!("fixtures/minimax/signed-out.json");
const BILLING: &str = include_str!("fixtures/minimax/billing.json");

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> MiniMax {
    MiniMax::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn env_with_key() -> Env {
    Env::empty().with("MINIMAX_CODING_API_KEY", CODING_KEY)
}

#[test]
fn happy_path_maps_model_remains_to_windows() {
    let client = fixture(vec![FixtureResponse::json(200, REMAINS)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::MiniMax);
    assert_eq!(snapshot.title, "MiniMax");
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.plan.as_deref(), Some("Coding Plan Pro"));

    assert_eq!(
        snapshot.account.as_deref(),
        Some(codexbar_providers::redact(CODING_KEY).as_str())
    );
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains(CODING_KEY));

    // Three lanes: the primary text lane, its weekly companion, then video.
    assert_eq!(snapshot.windows.len(), 3);

    let general = &snapshot.windows[0];
    assert_eq!(general.id, "general");
    assert_eq!(general.title, "General");
    assert_eq!(general.kind, WindowKind::Session);
    // 100 - 37.5 remaining percent.
    assert_eq!(general.window.used_percent, 62.5);
    assert_eq!(general.window.window_minutes, Some(300));
    assert_eq!(
        general.window.resets_at,
        Some(Utc.timestamp_opt(1_789_084_800, 0).unwrap())
    );
    assert!(general
        .window
        .reset_description
        .as_deref()
        .unwrap_or_default()
        .starts_with("Resets in"));

    let weekly = &snapshot.windows[1];
    assert_eq!(weekly.id, "general-weekly");
    assert_eq!(weekly.title, "General · weekly");
    assert_eq!(weekly.kind, WindowKind::Weekly);
    assert_eq!(weekly.window.used_percent, 20.0);
    assert_eq!(weekly.window.window_minutes, Some(10_080));

    // A lane with counts but no remaining percent derives the percentage.
    let video = &snapshot.windows[2];
    assert_eq!(video.id, "video");
    assert_eq!(video.kind, WindowKind::Session);
    assert_eq!(video.window.used_percent, 75.0);

    let balance = snapshot.balance.as_ref().expect("points balance");
    assert_eq!(balance.amount, 1234.5);
    assert_eq!(balance.currency, "Points");
    assert_eq!(balance.label.as_deref(), Some("MiniMax points balance"));

    assert_eq!(snapshot.max_used_percent(), 75.0);

    let requests = client.captured();
    assert_eq!(requests.len(), 1, "the token-plan endpoint answers first");
    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].url.starts_with("https://api.minimax.io/"));
    assert!(requests[0].path_ends_with("/v1/token_plan/remains"));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {CODING_KEY}").as_str())
    );
    assert_eq!(requests[0].header("Accept"), Some("application/json"));
    assert_eq!(requests[0].header("MM-API-Source"), Some("CodexBar"));
}

#[test]
fn the_coding_plan_key_wins_over_the_standard_key() {
    let client = fixture(vec![FixtureResponse::json(200, REMAINS)]);
    let env = Env::empty()
        .with("MINIMAX_API_KEY", STANDARD_KEY)
        .with("MINIMAX_CODING_API_KEY", CODING_KEY);
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(
        client.captured()[0].authorization().as_deref(),
        Some(format!("Bearer {CODING_KEY}").as_str())
    );

    // …and a standard key alone is still used.
    let client = fixture(vec![FixtureResponse::json(200, REMAINS)]);
    let env = Env::empty().with("MINIMAX_API_KEY", STANDARD_KEY);
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(
        client.captured()[0].authorization().as_deref(),
        Some(format!("Bearer {STANDARD_KEY}").as_str())
    );
}

#[test]
fn a_404_on_the_token_plan_falls_back_to_the_legacy_endpoint() {
    let client = fixture(vec![
        FixtureResponse::json(404, "{}"),
        FixtureResponse::json(200, REMAINS),
    ]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    let requests = client.captured();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].path_ends_with("/v1/token_plan/remains"));
    assert!(requests[1].path_ends_with("/v1/api/openplatform/coding_plan/remains"));
    assert!(requests[1].url.starts_with("https://api.minimax.io/"));
}

#[test]
fn a_rejected_token_retries_the_china_host() {
    let client = fixture(vec![
        FixtureResponse::json(401, "{}"),
        FixtureResponse::json(401, "{}"),
        FixtureResponse::json(200, REMAINS),
    ]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    let requests = client.captured();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].url.starts_with("https://api.minimax.io/"));
    assert!(requests[1].url.starts_with("https://api.minimax.io/"));
    assert!(requests[2].url.starts_with("https://api.minimaxi.com/"));
}

#[test]
fn a_token_rejected_everywhere_is_an_error_that_names_the_variables() {
    let client = fixture(vec![
        FixtureResponse::json(200, SIGNED_OUT),
        FixtureResponse::json(200, SIGNED_OUT),
        FixtureResponse::json(200, SIGNED_OUT),
        FixtureResponse::json(200, SIGNED_OUT),
    ]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("MINIMAX_CODING_API_KEY"), "{message}");
    assert!(!message.contains(CODING_KEY));
    assert_eq!(client.request_count(), 4, "both hosts, both endpoints");
}

#[test]
fn an_http_500_is_an_error_and_does_not_retry_another_host() {
    let client = fixture(vec![FixtureResponse::json(500, "{}")]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("HTTP 500"));
    assert_eq!(client.request_count(), 1);
}

#[test]
fn strict_mode_rejects_a_custom_host_before_the_bearer() {
    let client = fixture(vec![]);
    let env = env_with_key()
        .with("MINIMAX_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES", "true")
        .with(
            "MINIMAX_REMAINS_URL",
            "https://proxy.example.com/v1/token_plan/remains",
        );
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("minimax.io"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn a_custom_https_host_is_allowed_without_strict_mode() {
    let client = fixture(vec![FixtureResponse::json(200, REMAINS)]);
    let env = env_with_key().with(
        "MINIMAX_REMAINS_URL",
        "https://proxy.example.com/v1/token_plan/remains",
    );
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    let requests = client.captured();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].url.starts_with("https://proxy.example.com/"));
}

#[test]
fn an_http_override_fails_closed_before_the_bearer_is_attached() {
    let client = fixture(vec![]);
    let env = env_with_key().with(
        "MINIMAX_REMAINS_URL",
        "http://api.minimax.io/v1/token_plan/remains",
    );
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("HTTPS"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn a_minimax_host_override_moves_the_whole_request() {
    let client = fixture(vec![FixtureResponse::json(200, REMAINS)]);
    let env = env_with_key().with("MINIMAX_HOST", "platform.minimaxi.com");
    let snapshot = provider(&client, env.clone()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(client.captured()[0]
        .url
        .starts_with("https://platform.minimaxi.com/"));
    assert_eq!(
        provider(&fixture(vec![]), env.clone()).region(),
        MiniMaxRegion::ChinaMainland
    );
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
    assert!(hint.contains("MINIMAX_CODING_API_KEY"), "{hint}");
    assert!(hint.contains("MINIMAX_API_KEY"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn an_unavailable_placeholder_lane_is_not_published() {
    let body = serde_json::json!({
        "base_resp": {"status_code": 0},
        "data": {
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_total_count": 0,
                    "current_interval_usage_count": 0,
                    "current_interval_remaining_percent": 40.0,
                    "current_interval_status": 1,
                    "start_time": 1_789_066_800_i64,
                    "end_time": 1_789_084_800_i64
                },
                {
                    "model_name": "video",
                    "current_interval_total_count": 0,
                    "current_interval_usage_count": 0,
                    "current_interval_remaining_percent": 100.0,
                    "current_interval_status": 3
                }
            ]
        }
    })
    .to_string();
    let client = fixture(vec![FixtureResponse::json(200, body)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].id, "general");
    assert_eq!(snapshot.windows[0].window.used_percent, 60.0);
}

#[test]
fn a_body_without_model_remains_is_an_error() {
    // Both remains endpoints answer with the same empty body: the parse failure
    // is retryable, so each is tried once before the fetch gives up.
    let body = r#"{"base_resp":{"status_code":0},"data":{}}"#;
    let client = fixture(vec![
        FixtureResponse::json(200, body),
        FixtureResponse::json(200, body),
    ]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(
        snapshot
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("model_remains"),
        "{:?}",
        snapshot.error
    );
    assert_eq!(client.request_count(), 2);
}

#[test]
fn billing_history_is_parsed_over_utc_days_and_uses_the_cookie_session() {
    let client = fixture(vec![FixtureResponse::json(200, BILLING)]);
    let env = env_with_key().with("MINIMAX_COOKIE", "session=«redacted»");
    let provider = provider(&client, env);

    let summary = provider
        .billing_summary(MiniMaxRegion::Global, 1, now())
        .expect("billing summary");

    assert_eq!(summary.today_tokens, 1000);
    assert_eq!(summary.last_30_days_tokens, 1800);
    assert_eq!(summary.today_cash, Some(1.5));
    assert_eq!(summary.daily.len(), 2);
    assert_eq!(summary.top_models[0].name, "MiniMax-M2");
    assert_eq!(summary.top_models[1].name, "MiniMax-M1");

    let requests = client.captured();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].url.starts_with(
        "https://platform.minimax.io/account/amount?page=1&limit=100&aggregate=false"
    ));
    assert_eq!(requests[0].header("Cookie"), Some("session=«redacted»"));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {CODING_KEY}").as_str())
    );
}

#[test]
fn billing_without_a_session_is_refused_before_sending_anything() {
    let client = fixture(vec![]);
    let provider = provider(&client, env_with_key());
    assert!(provider
        .billing_summary(MiniMaxRegion::Global, 1, now())
        .is_err());
    assert_eq!(client.request_count(), 0);
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![FixtureResponse::json(200, REMAINS)]);
    let instant = now();
    let snapshot = provider(&client, env_with_key()).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
