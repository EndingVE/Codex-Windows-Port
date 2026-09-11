//! z.ai fixture tests — the whole provider, offline.
//!
//! `FixtureClient` replays recorded bodies (checked into `tests/fixtures/zai/`)
//! and records what the provider sent, so every test asserts on **the request**
//! and **the mapped windows** in the same place.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::providers::zai::{ZaiRegion, ZaiScope};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Env, HttpClient, Zai};

const API_KEY: &str = "«redacted:zai-token-…»";
const QUOTA: &str = include_str!("fixtures/zai/quota.json");
const QUOTA_RECALC: &str = include_str!("fixtures/zai/quota-recalc.json");
const BALANCE: &str = include_str!("fixtures/zai/balance.json");
const ERROR_401: &str = include_str!("fixtures/zai/error-401.json");

/// A fixed instant so every assertion is deterministic (`fetch` takes `now`).
fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> Zai {
    Zai::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn env_with_key() -> Env {
    Env::empty().with("Z_AI_API_KEY", API_KEY)
}

fn env_cn() -> Env {
    Env::empty()
        .with("Z_AI_API_KEY", API_KEY)
        .with("Z_AI_REGION", "bigmodel-cn")
}

#[test]
fn happy_path_maps_quota_limits_to_windows() {
    let client = fixture(vec![FixtureResponse::json(200, QUOTA)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::Zai);
    assert_eq!(snapshot.title, "z.ai");
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.plan.as_deref(), Some("Coding Plan Pro"));
    assert!(snapshot.balance.is_none(), "Global has no balance endpoint");

    // The account is the key, masked — never the key itself.
    assert_eq!(
        snapshot.account.as_deref(),
        Some(codexbar_providers::redact(API_KEY).as_str())
    );
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(API_KEY));

    assert_eq!(snapshot.windows.len(), 3);

    // Sorted by duration: the 5-hour lane is primary, the weekly lane secondary.
    let session = &snapshot.windows[0];
    assert_eq!(session.id, "session");
    assert_eq!(session.title, "Session · 5h");
    assert_eq!(session.kind, WindowKind::Session);
    assert_eq!(session.window.used_percent, 12.0);
    assert_eq!(session.window.window_minutes, Some(300));
    assert_eq!(
        session.window.reset_description.as_deref(),
        Some("5-hour"),
        "a five-hour plan reset inside the window is published"
    );
    assert_eq!(
        session.window.resets_at,
        Some(Utc.timestamp_millis_opt(1_789_077_600_000).unwrap())
    );

    let weekly = &snapshot.windows[1];
    assert_eq!(weekly.id, "weekly");
    assert_eq!(weekly.kind, WindowKind::Weekly);
    assert_eq!(weekly.window.used_percent, 40.0);
    assert_eq!(weekly.window.window_minutes, Some(10_080));
    assert_eq!(
        weekly.window.reset_description.as_deref(),
        Some("1 week window")
    );

    // A `TIME_LIMIT` unit 5 / number 1 is the monthly MCP marker, not a minute.
    let mcp = &snapshot.windows[2];
    assert_eq!(mcp.id, "zai-mcp");
    assert_eq!(mcp.title, "MCP");
    assert_eq!(mcp.kind, WindowKind::Extra);
    assert_eq!(mcp.window.used_percent, 5.0);
    assert_eq!(mcp.window.window_minutes, Some(43_200));
    assert_eq!(mcp.window.reset_description.as_deref(), Some("MCP"));

    assert_eq!(snapshot.max_used_percent(), 40.0);
    // The headline prefers the session lane, not the highest number.
    assert_eq!(snapshot.headline_used_percent(), Some(12.0));

    let requests = client.captured();
    assert_eq!(requests.len(), 1, "Global personal sends one quota request");
    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].path_ends_with("/api/monitor/usage/quota/limit"));
    assert!(requests[0].url.starts_with("https://api.z.ai/"));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {API_KEY}").as_str())
    );
    assert_eq!(requests[0].header("Accept"), Some("application/json"));
    assert_eq!(requests[0].header("Bigmodel-Organization"), None);
}

#[test]
fn counts_recalculate_the_percentage_and_clamp_it() {
    let client = fixture(vec![FixtureResponse::json(200, QUOTA_RECALC)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.windows.len(), 1);
    let lane = &snapshot.windows[0];
    // max(1000 - 700, 250) = 300 of 1000 → 30 %, not the reported 1 %.
    assert_eq!(lane.window.used_percent, 30.0);
    assert_eq!(lane.id, "session");
    assert_eq!(lane.window.window_minutes, Some(300));
}

#[test]
fn the_cn_team_scope_adds_type_2_and_the_bigmodel_headers() {
    let client = fixture(vec![
        FixtureResponse::json(200, QUOTA),
        FixtureResponse::json(200, BALANCE),
    ]);
    let env = env_cn()
        .with("Z_AI_USAGE_SCOPE", "team")
        .with("Z_AI_ORGANIZATION", "org_123")
        .with("Z_AI_PROJECT", "proj_456");
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    let requests = client.captured();
    assert_eq!(requests.len(), 2, "quota + CN balance");

    assert!(requests[0].url.starts_with("https://open.bigmodel.cn/"));
    assert!(requests[0].url.contains("type=2"), "{}", requests[0].url);
    assert_eq!(requests[0].header("Bigmodel-Organization"), Some("org_123"));
    assert_eq!(requests[0].header("Bigmodel-Project"), Some("proj_456"));

    // Balance is CN-only: `availableBalance` wins over `balance`.
    assert!(requests[1].path_ends_with("/api/biz/account/query-customer-account-report"));
    let balance = snapshot.balance.as_ref().expect("CN balance");
    assert_eq!(balance.amount, 1234.56);
    assert_eq!(balance.currency, "CNY");
    assert_eq!(balance.label.as_deref(), Some("Account balance"));
}

#[test]
fn team_scope_without_both_ids_fails_closed_before_the_bearer() {
    let client = fixture(vec![]);
    let env = env_with_key()
        .with("Z_AI_USAGE_SCOPE", "team")
        .with("Z_AI_ORGANIZATION", "org_123");
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("team scope"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn both_override_naming_sets_resolve_to_the_same_endpoint() {
    // Swift names.
    let env = env_with_key().with("Z_AI_QUOTA_URL", "https://proxy.example.com/z/quota");
    assert_eq!(
        provider(&fixture(vec![]), env)
            .quota_url(ZaiRegion::Global, ZaiScope::Personal)
            .unwrap(),
        "https://proxy.example.com/z/quota"
    );

    // Plugin-JS names.
    let env = env_with_key().with("Z_AI_QUOTA_ENDPOINT", "https://proxy.example.com/z/quota");
    assert_eq!(
        provider(&fixture(vec![]), env)
            .quota_url(ZaiRegion::Global, ZaiScope::Personal)
            .unwrap(),
        "https://proxy.example.com/z/quota"
    );

    // A bare host override gets the canonical quota path appended.
    let env = env_with_key().with("Z_AI_API_HOST", "api.z.ai");
    assert_eq!(
        provider(&fixture(vec![]), env)
            .quota_url(ZaiRegion::Global, ZaiScope::Personal)
            .unwrap(),
        "https://api.z.ai/api/monitor/usage/quota/limit"
    );

    // Balance: Swift `Z_AI_BALANCE_URL` and JS `Z_AI_BALANCE_ENDPOINT`.
    let env = env_with_key().with("Z_AI_BALANCE_URL", "https://proxy.example.com/balance");
    let zai = provider(&fixture(vec![]), env);
    assert_eq!(
        zai.balance_url(ZaiRegion::Global).unwrap().as_deref(),
        Some("https://proxy.example.com/balance")
    );
    // …and Global with no override has no balance endpoint at all.
    let zai = provider(&fixture(vec![]), env_with_key());
    assert_eq!(zai.balance_url(ZaiRegion::Global).unwrap(), None);
}

#[test]
fn the_model_usage_endpoint_is_honoured_for_both_alias_sets() {
    let zai = provider(&fixture(vec![]), env_with_key());
    let url = zai
        .model_usage_url(
            ZaiRegion::Global,
            ZaiScope::Personal,
            "2026-09-04 00:00:00",
            "2026-09-10 20:59:00",
        )
        .unwrap();
    assert!(url.starts_with("https://api.z.ai/api/monitor/usage/model-usage?startTime="));
    assert!(url.contains("&endTime=2026-09-10%2020%3A59%3A00"), "{url}");
    assert!(!url.contains("type=3"));

    let team = zai
        .model_usage_url(ZaiRegion::Global, ZaiScope::Team, "a", "b")
        .unwrap();
    assert!(team.ends_with("&type=3"));

    let env = env_with_key().with("Z_AI_MODEL_USAGE_ENDPOINT", "https://proxy.example.com/mu");
    let url = provider(&fixture(vec![]), env)
        .model_usage_url(ZaiRegion::Global, ZaiScope::Personal, "a", "b")
        .unwrap();
    assert!(url.starts_with("https://proxy.example.com/mu?startTime=a&endTime=b"));
}

#[test]
fn a_region_mismatch_override_fails_closed_before_the_bearer() {
    let client = fixture(vec![]);
    // The region is inferred from a canonical override host, so a mismatch needs
    // an *explicit* region selection (Swift's `ZaiSettingsReader.inferredRegion`
    // does the same).
    let env = env_with_key().with("Z_AI_REGION", "global").with(
        "Z_AI_QUOTA_URL",
        "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
    );
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("does not match"), "{message}");
    assert_eq!(client.request_count(), 0);

    // …and the other way round: a CN selection cannot point at api.z.ai.
    let env = env_cn().with(
        "Z_AI_QUOTA_URL",
        "https://api.z.ai/api/monitor/usage/quota/limit",
    );
    assert_eq!(
        provider(&fixture(vec![]), env).fetch(now()).status,
        FetchStatus::Error
    );
}

#[test]
fn an_http_override_fails_closed_before_the_bearer_is_attached() {
    let client = fixture(vec![]);
    let env = env_with_key().with(
        "Z_AI_QUOTA_URL",
        "http://api.z.ai/api/monitor/usage/quota/limit",
    );
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("Z_AI_QUOTA_URL"), "{message}");
    assert!(message.contains("HTTPS"), "{message}");
    assert_eq!(client.request_count(), 0);
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
    assert!(hint.contains("Z_AI_API_KEY"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn a_bigmodel_alias_alone_only_works_for_the_cn_region() {
    // Global ignores the BigModel aliases entirely (`SPEC-apikey.md` §5.1).
    let global = Env::empty().with("BIGMODEL_API_KEY", API_KEY);
    assert_eq!(
        provider(&fixture(vec![]), global).fetch(now()).status,
        FetchStatus::NotConfigured
    );

    // BigModel CN accepts them.
    let client = fixture(vec![FixtureResponse::json(200, QUOTA)]);
    let cn = Env::empty()
        .with("Z_AI_REGION", "bigmodel-cn")
        .with("BIGMODEL_API_KEY", API_KEY);
    let snapshot = provider(&client, cn).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(
        client.captured()[0].authorization().as_deref(),
        Some(format!("Bearer {API_KEY}").as_str())
    );
}

#[test]
fn a_rejected_token_is_an_error_and_never_leaks_the_key() {
    let client = fixture(vec![FixtureResponse::json(200, ERROR_401)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("z.ai quota API error"), "{message}");
    assert!(message.contains("Unauthorized"), "{message}");
    assert!(!message.contains(API_KEY));
    // A failed quota call stops there: the optional balance probe is not attempted.
    assert_eq!(client.request_count(), 1);
}

#[test]
fn an_http_401_stops_the_fetch_with_a_safe_message() {
    let client = fixture(vec![FixtureResponse::json(401, "{\"message\":\"nope\"}")]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("401"), "{message}");
    assert!(!message.contains(API_KEY));
}

#[test]
fn a_cn_balance_failure_is_soft_and_keeps_the_quota() {
    let client = fixture(vec![
        FixtureResponse::json(200, QUOTA),
        FixtureResponse::timeout(),
    ]);
    let snapshot = provider(&client, env_cn()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok, "the quota is still real");
    assert!(snapshot.balance.is_none());
    assert_eq!(snapshot.windows.len(), 3);
    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(note.contains("account balance unavailable"), "{note}");
    assert_eq!(client.request_count(), 2);
}

#[test]
fn a_malformed_body_is_an_error_not_a_panic() {
    let client = fixture(vec![FixtureResponse::json(
        200,
        "{\"success\":true,\"code\":200}",
    )]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("z.ai quota data"));

    let client = fixture(vec![FixtureResponse::text(200, "<html>nope</html>")]);
    assert_eq!(
        provider(&client, env_with_key()).fetch(now()).status,
        FetchStatus::Error
    );
}

#[test]
fn an_unsupported_region_or_scope_is_rejected_without_a_request() {
    let client = fixture(vec![]);
    let env = env_with_key().with("Z_AI_REGION", "mars");
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("Unsupported z.ai region"));

    let env = env_with_key().with("Z_AI_USAGE_SCOPE", "enterprise");
    let snapshot = provider(&fixture(vec![]), env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("Unsupported z.ai usage scope"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![FixtureResponse::json(200, QUOTA)]);
    let instant = now();
    let snapshot = provider(&client, env_with_key()).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
