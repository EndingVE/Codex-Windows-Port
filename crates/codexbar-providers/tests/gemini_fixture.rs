//! Gemini fixture tests — offline, deterministic, no Google endpoints.
//!
//! Credentials, settings and `oauth2.js` are written from reviewed fixtures into
//! a private temp directory and injected through `GEMINI_OAUTH_CREDS_PATH` /
//! `GEMINI_SETTINGS_PATH` / `GEMINI_OAUTH2_JS_PATH`, so the real file-reading
//! paths run without ever touching the user's `~/.gemini`.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Env, Gemini, HttpClient};

const CREDS_VALID: &str = include_str!("fixtures/gemini/oauth-creds.json");
const CREDS_WORKSPACE: &str = include_str!("fixtures/gemini/oauth-creds-workspace.json");
const CREDS_EXPIRED: &str = include_str!("fixtures/gemini/oauth-creds-expired.json");
const CREDS_NO_REFRESH: &str = include_str!("fixtures/gemini/oauth-creds-no-refresh.json");
const SETTINGS_OAUTH: &str = include_str!("fixtures/gemini/settings-oauth.json");
const SETTINGS_API_KEY: &str = include_str!("fixtures/gemini/settings-api-key.json");
const SETTINGS_VERTEX: &str = include_str!("fixtures/gemini/settings-vertex.json");
const OAUTH2_JS: &str = include_str!("fixtures/gemini/oauth2.js");
const LOAD_CODE_ASSIST: &str = include_str!("fixtures/gemini/load-code-assist.json");
const LOAD_CODE_ASSIST_PAID: &str =
    include_str!("fixtures/gemini/load-code-assist-paid-named.json");
const LOAD_CODE_ASSIST_FREE: &str = include_str!("fixtures/gemini/load-code-assist-free.json");
const LOAD_CODE_ASSIST_UNSUPPORTED: &str =
    include_str!("fixtures/gemini/load-code-assist-unsupported.json");
const LOAD_CODE_ASSIST_UNSUPPORTED_TIER: &str =
    include_str!("fixtures/gemini/load-code-assist-unsupported-with-tier.json");
const QUOTA: &str = include_str!("fixtures/gemini/retrieve-user-quota.json");
const QUOTA_EMPTY: &str = include_str!("fixtures/gemini/retrieve-user-quota-empty.json");
const REFRESH: &str = include_str!("fixtures/gemini/refresh-token.json");
const ERROR_401: &str = include_str!("fixtures/gemini/error-401.json");
const ERROR_403_SUBSCRIPTION: &str = include_str!("fixtures/gemini/error-403-subscription.json");

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("codexbar-gemini-{}-{}", std::process::id(), name));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

/// Env wired to a scratch directory, with client credentials so no `oauth2.js`
/// discovery is ever attempted for tests that do not test it.
fn env_for(creds: &Path, settings: &Path) -> Env {
    Env::empty()
        .with(
            "GEMINI_OAUTH_CREDS_PATH",
            creds.to_string_lossy().to_string(),
        )
        .with(
            "GEMINI_SETTINGS_PATH",
            settings.to_string_lossy().to_string(),
        )
        .with(
            "GEMINI_OAUTH_CLIENT_ID",
            "fixture-client-id.apps.googleusercontent.com",
        )
        .with("GEMINI_OAUTH_CLIENT_SECRET", "fixture-client-secret")
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> Gemini {
    Gemini::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

#[test]
fn missing_credentials_are_not_configured_and_send_nothing() {
    let scratch = Scratch::new("missing");
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let creds = scratch.dir.join("absent-oauth_creds.json");
    let client = fixture(vec![]);

    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("gemini"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn happy_path_maps_the_pro_and_flash_lanes() {
    let scratch = Scratch::new("happy");
    let creds = scratch.write("oauth_creds.json", CREDS_VALID);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![
        FixtureResponse::json(200, LOAD_CODE_ASSIST),
        FixtureResponse::json(200, QUOTA),
    ]);

    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok, "{:?}", snapshot.error);
    assert_eq!(snapshot.provider, ProviderId::Gemini);
    assert_eq!(snapshot.source, DataSource::OAuth);
    // The domain never reaches the card: the identity is masked.
    assert_eq!(snapshot.account.as_deref(), Some("user@…"));
    assert_eq!(snapshot.plan.as_deref(), Some("Code Assist Standard"));

    // Pro 0.4 left -> 60% used; Flash 0.85 -> 15%; Flash Lite 0.95 -> 5%.
    let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, vec!["gemini-pro", "gemini-flash", "gemini-flash-lite"]);
    assert_eq!(snapshot.windows[0].window.used_percent, 60.0);
    assert_eq!(snapshot.windows[0].kind, WindowKind::Session);
    assert_eq!(snapshot.windows[0].window.window_minutes, Some(1_440));
    assert!(snapshot.windows[0].window.resets_at.is_some());
    assert_eq!(snapshot.windows[1].window.used_percent, 15.0);
    assert_eq!(snapshot.windows[2].window.used_percent, 5.0);

    // The access token must never reach the payload.
    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains("ya29."));
}

#[test]
fn requests_match_the_documented_endpoints_and_bodies() {
    let scratch = Scratch::new("requests");
    let creds = scratch.write("oauth_creds.json", CREDS_VALID);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![
        FixtureResponse::json(200, LOAD_CODE_ASSIST),
        FixtureResponse::json(200, QUOTA),
    ]);
    provider(&client, env_for(&creds, &settings)).fetch(now());

    let requests = client.captured();
    assert_eq!(requests.len(), 2);

    assert_eq!(requests[0].method.as_str(), "POST");
    assert!(
        requests[0].url.ends_with("/v1internal:loadCodeAssist"),
        "{}",
        requests[0].url
    );
    assert!(requests[0].sends_authorization_with("Bearer "));
    let body: serde_json::Value = requests[0].json_body().unwrap();
    assert_eq!(body["metadata"]["ideType"], "GEMINI_CLI");

    assert!(requests[1].url.ends_with("/v1internal:retrieveUserQuota"));
    let body: serde_json::Value = requests[1].json_body().unwrap();
    assert_eq!(body["project"], "gen-lang-client-0123456789");
}

#[test]
fn an_expired_token_is_refreshed_in_memory_and_never_written_back() {
    let scratch = Scratch::new("refresh");
    let creds_path = scratch.write("oauth_creds.json", CREDS_EXPIRED);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let before = std::fs::read(&creds_path).unwrap();
    let before_names = scratch.names();

    let client = fixture(vec![
        FixtureResponse::json(200, REFRESH),
        FixtureResponse::json(200, LOAD_CODE_ASSIST),
        FixtureResponse::json(200, QUOTA),
    ]);
    let snapshot = provider(&client, env_for(&creds_path, &settings)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok, "{:?}", snapshot.error);
    assert_eq!(client.request_count(), 3);

    let refresh = &client.captured()[0];
    assert!(refresh.url.contains("oauth2.googleapis.com/token"));
    let form = refresh.body.clone().unwrap_or_default();
    assert!(form.contains("grant_type=refresh_token"));
    assert!(form.contains("client_id=fixture-client-id.apps.googleusercontent.com"));
    assert!(form.contains("refresh_token="));

    // Read-only: the credential file and its directory are untouched.
    assert_eq!(std::fs::read(&creds_path).unwrap(), before);
    assert_eq!(scratch.names(), before_names);
}

#[test]
fn the_client_id_is_extracted_from_the_installed_package_when_env_is_absent() {
    let scratch = Scratch::new("oauth2js");
    let creds = scratch.write("oauth_creds.json", CREDS_EXPIRED);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let oauth2_js = scratch.write("oauth2.js", OAUTH2_JS);

    let env = Env::empty()
        .with(
            "GEMINI_OAUTH_CREDS_PATH",
            creds.to_string_lossy().to_string(),
        )
        .with(
            "GEMINI_SETTINGS_PATH",
            settings.to_string_lossy().to_string(),
        )
        .with(
            "GEMINI_OAUTH2_JS_PATH",
            oauth2_js.to_string_lossy().to_string(),
        );

    let client = fixture(vec![
        FixtureResponse::json(200, REFRESH),
        FixtureResponse::json(200, LOAD_CODE_ASSIST),
        FixtureResponse::json(200, QUOTA),
    ]);
    let snapshot = provider(&client, env).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok, "{:?}", snapshot.error);

    let form = client.captured()[0].body.clone().unwrap_or_default();
    assert!(
        form.contains(
            "client_id=681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
        ),
        "{form}"
    );
    assert!(form.contains("client_secret=fixture-client-secret-value-not-real"));
}

#[test]
fn an_expired_token_without_a_refresh_token_is_not_configured() {
    let scratch = Scratch::new("norefresh");
    let creds = scratch.write("oauth_creds.json", CREDS_NO_REFRESH);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![]);

    // Force expiry by evaluating against a time far past the fixture's expiry.
    let late = Utc.with_ymd_and_hms(2031, 1, 1, 0, 0, 0).unwrap();
    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(late);

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("refresh token"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn the_consumer_shutdown_is_an_honest_state_not_a_generic_error() {
    let scratch = Scratch::new("shutdown");
    let creds = scratch.write("oauth_creds.json", CREDS_VALID);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![FixtureResponse::json(
        200,
        LOAD_CODE_ASSIST_UNSUPPORTED,
    )]);

    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("consumerTierDeprecated"), "{message}");
    assert!(message.contains("Antigravity"), "{message}");
    assert_eq!(
        client.request_count(),
        1,
        "the quota call is never attempted after the shutdown signal"
    );
}

#[test]
fn a_403_on_an_unsupported_consumer_account_maps_to_the_shutdown() {
    let scratch = Scratch::new("403consumer");
    let creds = scratch.write("oauth_creds.json", CREDS_VALID);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![
        FixtureResponse::json(200, LOAD_CODE_ASSIST_UNSUPPORTED_TIER),
        FixtureResponse::json(403, ERROR_403_SUBSCRIPTION),
    ]);

    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("consumerTierDeprecated"));
    assert_eq!(client.request_count(), 2);
}

#[test]
fn a_403_on_a_licensed_account_stays_a_plain_error() {
    let scratch = Scratch::new("403licensed");
    let creds = scratch.write("oauth_creds.json", CREDS_VALID);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![
        FixtureResponse::json(200, LOAD_CODE_ASSIST),
        FixtureResponse::json(403, ERROR_403_SUBSCRIPTION),
    ]);

    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(!message.contains("consumerTierDeprecated"), "{message}");
    assert!(message.contains("403"), "{message}");
}

#[test]
fn a_workspace_account_is_named_workspace() {
    let scratch = Scratch::new("workspace");
    let creds = scratch.write("oauth_creds.json", CREDS_WORKSPACE);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![
        FixtureResponse::json(200, LOAD_CODE_ASSIST_FREE),
        FixtureResponse::json(200, QUOTA),
    ]);

    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok, "{:?}", snapshot.error);
    assert_eq!(snapshot.plan.as_deref(), Some("Workspace"));
    assert_eq!(
        snapshot.account.as_deref(),
        Some("user@…"),
        "the workspace identity is masked"
    );
}

#[test]
fn a_named_paid_tier_wins_over_the_tier_id() {
    let scratch = Scratch::new("paidname");
    let creds = scratch.write("oauth_creds.json", CREDS_VALID);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![
        FixtureResponse::json(200, LOAD_CODE_ASSIST_PAID),
        FixtureResponse::json(200, QUOTA),
    ]);

    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.plan.as_deref(), Some("Google One AI Premium"));
}

#[test]
fn no_quota_buckets_is_an_honest_error() {
    let scratch = Scratch::new("noquota");
    let creds = scratch.write("oauth_creds.json", CREDS_VALID);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![
        FixtureResponse::json(200, LOAD_CODE_ASSIST),
        FixtureResponse::json(200, QUOTA_EMPTY),
    ]);

    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("no quota buckets"));
    assert!(snapshot.windows.is_empty());
}

#[test]
fn a_401_is_not_configured_with_a_re_login_hint() {
    let scratch = Scratch::new("401");
    let creds = scratch.write("oauth_creds.json", CREDS_VALID);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![
        FixtureResponse::json(200, LOAD_CODE_ASSIST),
        FixtureResponse::json(401, ERROR_401),
    ]);

    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("gemini"));
}

#[test]
fn api_key_and_vertex_auth_are_hard_errors() {
    for (name, settings_body) in [("apikey", SETTINGS_API_KEY), ("vertex", SETTINGS_VERTEX)] {
        let scratch = Scratch::new(name);
        let creds = scratch.write("oauth_creds.json", CREDS_VALID);
        let settings = scratch.write("settings.json", settings_body);
        let client = fixture(vec![]);
        let snapshot = provider(&client, env_for(&creds, &settings)).fetch(now());

        assert_eq!(snapshot.status, FetchStatus::Error, "{name}");
        assert!(snapshot
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("not supported"));
        assert_eq!(client.request_count(), 0, "{name}");
    }
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let scratch = Scratch::new("now");
    let creds = scratch.write("oauth_creds.json", CREDS_VALID);
    let settings = scratch.write("settings.json", SETTINGS_OAUTH);
    let client = fixture(vec![
        FixtureResponse::json(200, LOAD_CODE_ASSIST),
        FixtureResponse::json(200, QUOTA),
    ]);
    let instant = now();
    let snapshot = provider(&client, env_for(&creds, &settings)).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
