//! GitHub Copilot fixture tests — everything here is offline.
//!
//! Two halves:
//!
//! * the **usage provider** ([`Copilot`]) — token precedence, VS Code headers,
//!   window mapping, placeholder/unlimited handling, honest failure modes;
//! * the **device flow** ([`DeviceFlow`]) — the interactive login state machine
//!   (`authorization_pending` → `slow_down` → token, denied, expired), driven
//!   over a scripted [`FixtureClient`] with an injected, no-op sleeper so nothing
//!   waits and nothing reaches the network.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::providers::copilot::{
    DeviceCode, DeviceFlow, DeviceFlowError, PollOutcome,
};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{redact, Copilot, Env, HttpClient, HttpErrorKind};

/// A fake GitHub OAuth token. Never a real credential.
const TOKEN: &str = "gho_fixture_not_a_real_token";

const USAGE_PREMIUM_CHAT: &str = include_str!("fixtures/copilot/usage-premium-chat.json");
const USAGE_CHAT_ONLY: &str = include_str!("fixtures/copilot/usage-chat-only.json");
const USAGE_UNLIMITED: &str = include_str!("fixtures/copilot/usage-unlimited.json");
const USAGE_MONTHLY: &str = include_str!("fixtures/copilot/usage-monthly-quotas.json");
const USAGE_TOKEN_BILLING: &str = include_str!("fixtures/copilot/usage-token-billing.json");
const USAGE_EMPTY: &str = include_str!("fixtures/copilot/usage-empty.json");
const USER: &str = include_str!("fixtures/copilot/user.json");
const DEVICE_CODE: &str = include_str!("fixtures/copilot/device-code.json");
const TOKEN_PENDING: &str = include_str!("fixtures/copilot/token-pending.json");
const TOKEN_SLOWDOWN: &str = include_str!("fixtures/copilot/token-slowdown.json");
const TOKEN_SUCCESS: &str = include_str!("fixtures/copilot/token-success.json");
const TOKEN_DENIED: &str = include_str!("fixtures/copilot/token-denied.json");
const TOKEN_EXPIRED: &str = include_str!("fixtures/copilot/token-expired.json");
const ERROR_401: &str = include_str!("fixtures/copilot/error-401.json");

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> Copilot {
    Copilot::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn env_with_token() -> Env {
    Env::empty().with("COPILOT_API_TOKEN", TOKEN)
}

fn device_flow(client: &Arc<FixtureClient>) -> DeviceFlow {
    DeviceFlow::with_client(Arc::clone(client) as Arc<dyn HttpClient>, None)
}

/// The parsed device-code fixture, as if `request_device_code` had just run.
fn device_code() -> DeviceCode {
    let body: serde_json::Value = serde_json::from_str(DEVICE_CODE).unwrap();
    DeviceCode::from_json(&body).unwrap()
}

// ---------------------------------------------------------------------------
// Usage provider
// ---------------------------------------------------------------------------

#[test]
fn premium_and_chat_map_to_windows_and_the_request_is_vs_code_authenticated() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_PREMIUM_CHAT),
        FixtureResponse::json(200, USER),
    ]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::Copilot);
    assert_eq!(snapshot.title, "Copilot");
    assert_eq!(snapshot.source, DataSource::OAuth);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.plan.as_deref(), Some("Individual"));
    assert_eq!(snapshot.account.as_deref(), Some("octocat"));

    assert_eq!(snapshot.windows.len(), 2);
    let premium = &snapshot.windows[0];
    assert_eq!(premium.id, "premium");
    assert_eq!(premium.title, "Premium");
    assert_eq!(premium.kind, WindowKind::Session);
    assert_eq!(premium.window.used_percent, 30.0); // 100 - 70
    assert_eq!(premium.window.window_minutes, None);
    assert_eq!(
        premium.window.resets_at.unwrap().to_rfc3339(),
        "2026-10-01T00:00:00+00:00"
    );

    let chat = &snapshot.windows[1];
    assert_eq!(chat.id, "chat");
    assert_eq!(chat.title, "Chat");
    assert_eq!(chat.kind, WindowKind::Weekly);
    assert_eq!(chat.window.used_percent, 0.0);

    assert_eq!(snapshot.max_used_percent(), 30.0);

    // The GET carries the GitHub OAuth token under the `token` scheme, plus the
    // exact VS Code identity headers the Copilot backend expects.
    let requests = client.captured();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].path_ends_with("/copilot_internal/user"));
    assert!(requests[0].url.starts_with("https://api.github.com/"));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("token {TOKEN}").as_str())
    );
    assert_eq!(requests[0].header("Accept"), Some("application/json"));
    assert_eq!(requests[0].header("Editor-Version"), Some("vscode/1.96.2"));
    assert_eq!(
        requests[0].header("Editor-Plugin-Version"),
        Some("copilot-chat/0.26.7")
    );
    assert_eq!(
        requests[0].header("User-Agent"),
        Some("GitHubCopilotChat/0.26.7")
    );
    assert_eq!(
        requests[0].header("X-Github-Api-Version"),
        Some("2025-04-01")
    );

    assert!(requests[1].path_ends_with("/user"), "identity probe");
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(TOKEN));
}

#[test]
fn a_chat_only_plan_publishes_the_chat_window() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_CHAT_ONLY),
        FixtureResponse::json(200, USER),
    ]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.plan.as_deref(), Some("Free"));
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].id, "chat");
    assert_eq!(snapshot.windows[0].kind, WindowKind::Weekly);
    assert_eq!(snapshot.windows[0].window.used_percent, 60.0); // 100 - 40
}

#[test]
fn monthly_and_limited_quotas_become_windows() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_MONTHLY),
        FixtureResponse::json(200, USER),
    ]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.windows.len(), 2);
    assert_eq!(snapshot.windows[0].id, "premium");
    assert_eq!(snapshot.windows[0].window.used_percent, 50.0); // (300-150)/300
    assert_eq!(snapshot.windows[1].id, "chat");
    assert_eq!(snapshot.windows[1].window.used_percent, 20.0); // (500-400)/500
}

#[test]
fn an_unlimited_quota_publishes_no_fake_bar() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_UNLIMITED),
        FixtureResponse::json(200, USER),
    ]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(
        snapshot.windows.is_empty(),
        "unlimited is not a 0%-used window"
    );
    assert_eq!(snapshot.plan.as_deref(), Some("Business"));
}

#[test]
fn token_based_billing_publishes_no_fake_bar() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_TOKEN_BILLING),
        FixtureResponse::json(200, USER),
    ]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(snapshot.windows.is_empty());
    assert_eq!(snapshot.error, None);
}

#[test]
fn a_body_with_no_usable_quota_is_an_error_not_a_panic() {
    let client = fixture(vec![FixtureResponse::json(200, USAGE_EMPTY)]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("no usable Copilot quota"));
    // No point probing identity once the usage shape is unusable.
    assert_eq!(client.request_count(), 1);
}

#[test]
fn a_missing_token_is_not_configured_and_sends_nothing() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, Env::empty()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert_eq!(snapshot.source, DataSource::OAuth);
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.account.is_none());
    // The hint tells the user to sign in with GitHub (device flow).
    let hint = snapshot.error.as_deref().unwrap_or_default();
    assert!(hint.contains("GitHub"), "{hint}");
    assert!(hint.contains("COPILOT_API_TOKEN"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn a_rejected_token_is_an_error_that_never_leaks_it() {
    let client = fixture(vec![FixtureResponse::json(401, ERROR_401)]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("Settings"), "{message}");
    assert!(!message.contains(TOKEN));
    assert_eq!(client.request_count(), 1);
}

#[test]
fn a_failed_identity_probe_degrades_softly() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_PREMIUM_CHAT),
        FixtureResponse::json(401, ERROR_401),
    ]);
    let snapshot = provider(&client, env_with_token()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok, "usage is still real");
    assert_eq!(snapshot.windows.len(), 2);
    // Falls back to the masked token, never the raw value.
    assert_eq!(snapshot.account.as_deref(), Some(redact(TOKEN).as_str()));
    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(note.contains("identity unavailable"), "{note}");
    assert!(!note.contains(TOKEN));
}

#[test]
fn an_enterprise_host_rewrites_both_endpoints() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_PREMIUM_CHAT),
        FixtureResponse::json(200, USER),
    ]);
    let env = env_with_token().with(
        "COPILOT_ENTERPRISE_HOST",
        "https://contoso.ghe.com/some/path",
    );
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    let requests = client.captured();
    assert!(requests[0]
        .url
        .starts_with("https://api.contoso.ghe.com/copilot_internal/user"));
    assert!(requests[1]
        .url
        .starts_with("https://api.contoso.ghe.com/user"));
}

#[test]
fn a_bad_enterprise_host_fails_closed_before_the_token_is_attached() {
    let client = fixture(vec![]);
    let env = env_with_token().with("COPILOT_ENTERPRISE_HOST", "evil host");
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("whitespace"));
    assert_eq!(client.request_count(), 0);
}

// ---------------------------------------------------------------------------
// Device flow (interactive login, exercised fully offline)
// ---------------------------------------------------------------------------

#[test]
fn the_device_flow_requests_a_code_and_describes_it_to_the_user() {
    let client = fixture(vec![FixtureResponse::json(200, DEVICE_CODE)]);
    let flow = device_flow(&client);

    let code = flow.request_device_code().unwrap();
    assert_eq!(code.user_code, "ABCD-1234");
    assert_eq!(code.interval, 5);
    assert_eq!(code.expires_in, 900);
    assert!(code.verification_url().contains("user_code=ABCD-1234"));
    // The device code never prints itself.
    assert!(!format!("{code:?}").contains("device-code-fixture-value"));

    let requests = client.captured();
    assert_eq!(requests[0].method.as_str(), "POST");
    assert!(requests[0].path_ends_with("/login/device/code"));
    assert!(requests[0].url.starts_with("https://github.com/"));
    let body = requests[0].body.as_deref().unwrap_or_default();
    assert!(body.contains("client_id=Iv1.b507a08c87ecfe98"), "{body}");
    assert!(body.contains("scope=read%3Auser"), "{body}");
}

#[test]
fn polling_waits_through_pending_and_slow_down_then_returns_the_token() {
    let client = fixture(vec![
        FixtureResponse::json(200, TOKEN_PENDING),
        FixtureResponse::json(200, TOKEN_SLOWDOWN),
        FixtureResponse::json(200, TOKEN_SUCCESS),
    ]);
    let flow = device_flow(&client);
    let code = device_code();

    let sleeps: RefCell<Vec<u64>> = RefCell::new(Vec::new());
    let token = {
        let sleeper = |d: Duration| sleeps.borrow_mut().push(d.as_secs());
        flow.await_token(&code, &sleeper, 10).unwrap()
    };

    assert_eq!(token.expose(), "gho_device_flow_fixture_token");
    // interval starts at 5s; `slow_down` adds 5s for the next attempt.
    assert_eq!(*sleeps.borrow(), vec![5, 5, 10]);

    let requests = client.captured();
    // Three polls, one response each.
    assert_eq!(requests.len(), 3);
    let poll = requests[0].clone();
    assert_eq!(poll.method.as_str(), "POST");
    assert!(poll.path_ends_with("/login/oauth/access_token"));
    // The poll carries no Authorization header — the device code is in the body.
    assert_eq!(poll.header("Authorization"), None);
    let body = poll.body.as_deref().unwrap_or_default();
    assert!(
        body.contains("device_code=device-code-fixture-value"),
        "{body}"
    );
    assert!(
        body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"),
        "{body}"
    );
}

#[test]
fn a_single_poll_classifies_pending_without_waiting() {
    let client = fixture(vec![FixtureResponse::json(200, TOKEN_PENDING)]);
    let flow = device_flow(&client);
    let outcome = flow.poll_once(&device_code().device_code).unwrap();
    assert_eq!(outcome, PollOutcome::Pending);
}

#[test]
fn a_denied_device_flow_is_reported_as_denied() {
    let client = fixture(vec![FixtureResponse::json(200, TOKEN_DENIED)]);
    let flow = device_flow(&client);
    let code = device_code();

    let error = flow.await_token(&code, &|_| {}, 10).unwrap_err();
    assert_eq!(error, DeviceFlowError::Denied);
}

#[test]
fn an_expired_device_flow_is_reported_as_expired() {
    let client = fixture(vec![FixtureResponse::json(200, TOKEN_EXPIRED)]);
    let flow = device_flow(&client);
    let code = device_code();

    let error = flow.await_token(&code, &|_| {}, 10).unwrap_err();
    assert_eq!(error, DeviceFlowError::Expired);
}

#[test]
fn the_poll_loop_is_bounded() {
    let client = fixture(vec![
        FixtureResponse::json(200, TOKEN_PENDING),
        FixtureResponse::json(200, TOKEN_PENDING),
    ]);
    let flow = device_flow(&client);
    let code = device_code();

    let error = flow.await_token(&code, &|_| {}, 2).unwrap_err();
    assert_eq!(error, DeviceFlowError::TooManyPolls);
}

#[test]
fn a_dead_transport_is_reported_without_panicking() {
    let client = fixture(vec![FixtureResponse::failure(
        HttpErrorKind::Timeout,
        "the device-code request timed out (fixture)",
    )]);
    let flow = device_flow(&client);
    let error = flow.request_device_code().unwrap_err();
    assert!(matches!(error, DeviceFlowError::Http(_)));
}
