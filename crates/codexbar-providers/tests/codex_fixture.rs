//! Codex fixture tests — the whole provider, offline.
//!
//! Everything here replays recorded bodies (`tests/fixtures/codex/`) through a
//! [`FixtureClient`] and injects the exact `auth.json` / `config.toml` contents,
//! so the suite never touches the user's real Codex home, never writes a file and
//! never opens a socket. Each test asserts on **the request** and on **the mapped
//! `RateWindow`s** in the same place, the way `openrouter_fixture.rs` does.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Codex, Env, HttpClient, HttpErrorKind};

const AUTH: &str = include_str!("fixtures/codex/native-auth.json");
const AUTH_EXPIRED: &str = include_str!("fixtures/codex/native-auth-expired.json");
const AUTH_STALE: &str = include_str!("fixtures/codex/native-auth-stale.json");
const AUTH_FRESH_NO_EXP: &str = include_str!("fixtures/codex/native-auth-fresh-no-exp.json");
const AUTH_MALFORMED_EXP: &str = include_str!("fixtures/codex/native-auth-malformed-exp.json");
const AUTH_STRING_EXP: &str = include_str!("fixtures/codex/native-auth-string-exp.json");
const AUTH_OUT_OF_RANGE_EXP: &str =
    include_str!("fixtures/codex/native-auth-out-of-range-exp.json");
const AUTH_NO_TOKENS: &str = include_str!("fixtures/codex/native-auth-no-tokens.json");

const USAGE: &str = include_str!("fixtures/codex/usage.json");
const USAGE_MALFORMED_SECONDARY: &str =
    include_str!("fixtures/codex/usage-malformed-secondary.json");
const USAGE_MALFORMED_PRIMARY: &str = include_str!("fixtures/codex/usage-malformed-primary.json");
const USAGE_ADDITIONAL: &str = include_str!("fixtures/codex/usage-additional-limits.json");
const USAGE_EMPTY: &str = include_str!("fixtures/codex/usage-empty.json");
const RESET_CREDITS: &str = include_str!("fixtures/codex/rate-limit-reset-credits.json");
const ERROR_401: &str = include_str!("fixtures/codex/error-401.json");

const CONFIG: &str = include_str!("fixtures/codex/config.toml");
const CONFIG_BARE_HOST: &str = include_str!("fixtures/codex/config-bare-host.toml");
const CONFIG_CUSTOM: &str = include_str!("fixtures/codex/config-custom.toml");

/// A fixed instant so every assertion is deterministic (`fetch` takes `now`).
fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

/// Build a provider over a scripted client with injected credential files.
fn provider(client: &Arc<FixtureClient>, auth: Option<&str>, config: Option<&str>) -> Codex {
    Codex::with_injected(
        Arc::clone(client) as Arc<dyn HttpClient>,
        Env::empty(),
        auth.map(str::to_string),
        config.map(str::to_string),
    )
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

/// The happy path: usage + best-effort reset credits.
fn healthy_script() -> Arc<FixtureClient> {
    fixture(vec![
        FixtureResponse::json(200, USAGE),
        FixtureResponse::json(200, RESET_CREDITS),
    ])
}

#[test]
fn happy_path_maps_the_session_and_weekly_windows() {
    let client = healthy_script();
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG)).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::Codex);
    assert_eq!(snapshot.title, "Codex");
    assert_eq!(snapshot.source, DataSource::OAuth);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.fetched_at, now());

    // Identity comes from the same credential file as the numbers, masked to a
    // recognisable stub so the raw account id never reaches the card.
    assert_eq!(snapshot.account.as_deref(), Some("acct-EXA…"));
    assert_eq!(snapshot.plan.as_deref(), Some("Plus"));

    assert_eq!(snapshot.windows.len(), 2);
    let session = &snapshot.windows[0];
    assert_eq!(session.id, "session");
    assert_eq!(session.title, "Session · 5h");
    assert_eq!(session.kind, WindowKind::Session);
    assert_eq!(session.window.used_percent, 42.0);
    assert_eq!(session.window.window_minutes, Some(300));
    assert_eq!(
        session.window.resets_at,
        chrono::DateTime::from_timestamp(1_790_000_000, 0)
    );
    assert!(session.usage_known);

    let weekly = &snapshot.windows[1];
    assert_eq!(weekly.id, "weekly");
    assert_eq!(weekly.kind, WindowKind::Weekly);
    assert_eq!(weekly.window.used_percent, 13.0);
    assert_eq!(weekly.window.window_minutes, Some(10_080));

    // …and the tray/headline projections agree.
    assert_eq!(snapshot.headline_used_percent(), Some(42.0));
    assert_eq!(snapshot.max_used_percent(), 42.0);
}

#[test]
fn requests_follow_the_documented_endpoints_and_headers() {
    let client = healthy_script();
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);

    let requests = client.captured();
    assert_eq!(requests.len(), 2, "usage + reset credits");

    let usage = &requests[0];
    assert_eq!(usage.method.as_str(), "GET");
    assert!(usage.path_ends_with("/backend-api/wham/usage"));
    assert!(usage.sends_authorization_with("Bearer "));
    assert_eq!(usage.header("Accept"), Some("application/json"));
    assert_eq!(usage.header("User-Agent"), Some("CodexBar"));
    assert_eq!(
        usage.header("ChatGPT-Account-Id"),
        Some("acct-EXAMPLE-0001")
    );
    assert!(usage.url.starts_with("https://chatgpt.com/"));

    let reset = &requests[1];
    assert_eq!(reset.method.as_str(), "GET");
    assert!(reset.path_ends_with("/backend-api/wham/rate-limit-reset-credits"));
    assert!(reset.sends_authorization_with("Bearer "));
    assert_eq!(reset.header("OpenAI-Beta"), Some("codex-1"));
    assert_eq!(reset.header("originator"), Some("Codex Desktop"));
    // The reset endpoint spells the account header in caps.
    assert_eq!(
        reset.header("ChatGPT-Account-ID"),
        Some("acct-EXAMPLE-0001")
    );
}

#[test]
fn a_malformed_secondary_window_never_discards_the_session_window() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_MALFORMED_SECONDARY),
        FixtureResponse::json(200, RESET_CREDITS),
    ]);
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);
    assert_eq!(
        snapshot.windows.len(),
        1,
        "only the readable window survives"
    );
    assert_eq!(snapshot.windows[0].id, "session");
    assert_eq!(snapshot.windows[0].window.used_percent, 42.0);
}

#[test]
fn a_malformed_primary_window_leaves_the_weekly_window_intact() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_MALFORMED_PRIMARY),
        FixtureResponse::json(200, RESET_CREDITS),
    ]);
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.windows.len(), 1);
    // The lone survivor is 7 days long, so it lands in the weekly lane even
    // though the API called it `secondary_window`.
    assert_eq!(snapshot.windows[0].id, "weekly");
    assert_eq!(snapshot.windows[0].kind, WindowKind::Weekly);
    assert_eq!(snapshot.windows[0].window.used_percent, 88.0);
}

#[test]
fn additional_rate_limits_become_named_extra_windows() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_ADDITIONAL),
        FixtureResponse::json(200, RESET_CREDITS),
    ]);
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "session",
            "codex-spark",
            "codex-spark-weekly",
            "codex-code-review"
        ]
    );
    assert_eq!(snapshot.plan.as_deref(), Some("Pro 20x"));
    let spark = &snapshot.windows[1];
    assert_eq!(spark.title, "Codex Spark 5-hour");
    assert_eq!(spark.kind, WindowKind::Extra);
    assert_eq!(spark.window.used_percent, 5.0);
    assert_eq!(snapshot.windows[3].title, "Code Reviews");
}

#[test]
fn an_expired_token_is_an_honest_error_and_sends_nothing() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, Some(AUTH_EXPIRED), Some(CONFIG)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert_eq!(snapshot.source, DataSource::OAuth);
    assert!(snapshot.windows.is_empty());
    // The identity is still reported (masked) so the user knows which account
    // to fix — but never the raw account id.
    assert_eq!(snapshot.account.as_deref(), Some("acct-EXA…"));
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("token expired"), "{message}");
    assert!(message.contains("codex login"), "{message}");
    assert_eq!(
        client.request_count(),
        0,
        "a token we know is stale must not be spent on a doomed request"
    );
}

#[test]
fn the_eight_day_age_rule_stands_in_when_exp_is_missing() {
    // No `exp`: `last_refresh` is nine days before `now`.
    let client = fixture(vec![]);
    let snapshot = provider(&client, Some(AUTH_STALE), Some(CONFIG)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("token expired"));
    assert_eq!(client.request_count(), 0);

    // …and a recent `last_refresh` lets the fetch proceed.
    let client = healthy_script();
    let snapshot = provider(&client, Some(AUTH_FRESH_NO_EXP), Some(CONFIG)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(client.request_count(), 2);
}

#[test]
fn malformed_or_out_of_range_exp_falls_back_to_the_age_rule() {
    for auth in [AUTH_MALFORMED_EXP, AUTH_STRING_EXP, AUTH_OUT_OF_RANGE_EXP] {
        let client = healthy_script();
        let snapshot = provider(&client, Some(auth), Some(CONFIG)).fetch(now());
        assert_eq!(
            snapshot.status,
            FetchStatus::Ok,
            "a bad exp must not fabricate an expiry"
        );
        assert_eq!(client.request_count(), 2);
    }
}

#[test]
fn missing_credentials_are_not_configured_and_send_nothing() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, None, Some(CONFIG)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert_eq!(snapshot.source, DataSource::OAuth);
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.account.is_none());
    let hint = snapshot.error.as_deref().unwrap_or_default();
    assert!(hint.contains("codex login"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn a_file_without_tokens_is_an_error_not_a_setup_hint() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, Some(AUTH_NO_TOKENS), Some(CONFIG)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("no Codex OAuth tokens"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn a_plaintext_base_url_override_fails_closed_before_the_bearer() {
    let client = fixture(vec![]);
    let config = "chatgpt_base_url = \"http://evil.example/v1\"\n";
    let snapshot = provider(&client, Some(AUTH), Some(config)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("chatgpt_base_url"), "{message}");
    assert!(message.contains("HTTPS"), "{message}");
    assert_eq!(
        client.request_count(),
        0,
        "a plaintext override must never receive the token"
    );
}

#[test]
fn a_bare_host_override_is_normalised_to_the_backend_api() {
    let client = healthy_script();
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG_BARE_HOST)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(client.captured()[0]
        .url
        .starts_with("https://chatgpt.com/backend-api/wham/usage"));
}

#[test]
fn a_base_without_backend_api_uses_the_codex_usage_path() {
    let client = healthy_script();
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG_CUSTOM)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert!(client.captured()[0]
        .url
        .starts_with("https://proxy.example.com/wham/api/codex/usage"));
}

#[test]
fn an_unavailable_reset_credit_probe_degrades_softly() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE),
        FixtureResponse::failure(HttpErrorKind::Timeout, "4s deadline"),
    ]);
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok, "usage is still real");
    assert_eq!(snapshot.windows.len(), 2);
    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(
        note.contains("Rate-limit reset credits unavailable right now (timeout)"),
        "{note}"
    );
    assert_eq!(client.request_count(), 2);
}

#[test]
fn a_rejected_token_becomes_an_error_naming_the_status() {
    let client = fixture(vec![FixtureResponse::json(401, ERROR_401)]);
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("401"), "{message}");
    // A failed usage call stops there; the optional probe is not attempted.
    assert_eq!(client.request_count(), 1);
    // The credential itself is never echoed back.
    assert!(!message.contains("Bearer "));
}

#[test]
fn an_unexpected_body_shape_is_an_error_not_a_panic() {
    let client = fixture(vec![FixtureResponse::json(200, USAGE_EMPTY)]);
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("rate_limit"));
    assert_eq!(client.request_count(), 1);
}

#[test]
fn no_credential_value_ever_reaches_the_snapshot() {
    let auth: serde_json::Value = serde_json::from_str(AUTH).unwrap();
    let tokens = auth["tokens"].as_object().unwrap();
    let access_token = tokens["access_token"].as_str().unwrap().to_string();
    let refresh_token = tokens["refresh_token"].as_str().unwrap().to_string();
    let id_token = tokens["id_token"].as_str().unwrap().to_string();

    // Prove it on the healthy payload…
    let client = healthy_script();
    let snapshot = provider(&client, Some(AUTH), Some(CONFIG)).fetch(now());
    let rendered = serde_json::to_string(&snapshot).unwrap();
    for secret in [&access_token, &refresh_token, &id_token] {
        assert!(!rendered.contains(secret.as_str()), "a token leaked");
    }
    // …and on every failure shape, which is what users screenshot.
    for (auth, script) in [
        (AUTH_EXPIRED, vec![]),
        (AUTH_STALE, vec![]),
        (AUTH_NO_TOKENS, vec![]),
        (AUTH, vec![FixtureResponse::json(401, ERROR_401)]),
        (AUTH, vec![FixtureResponse::json(200, USAGE_EMPTY)]),
        (
            AUTH,
            vec![FixtureResponse::failure(HttpErrorKind::Client, "no TLS")],
        ),
    ] {
        let client = fixture(script);
        let snapshot = provider(&client, Some(auth), Some(CONFIG)).fetch(now());
        let rendered = serde_json::to_string(&snapshot).unwrap();
        for secret in [&access_token, &refresh_token, &id_token] {
            assert!(!rendered.contains(secret.as_str()), "a token leaked");
        }
        // The Debug view is safe too.
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains(access_token.as_str()));
    }
}

/// The defect this closes: the raw account UUID must never appear in the
/// serialized snapshot or its `Debug`, only its 8-character masked prefix.
#[test]
fn the_account_uuid_never_reaches_the_serialized_snapshot() {
    const UUID: &str = "abcd1234-5678-4abc-9def-0123456789ab";
    let auth = serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": "not-a-jwt",
            "account_id": UUID,
        },
        "last_refresh": "2026-09-09T00:00:00Z",
    })
    .to_string();

    // Healthy, and on a failure shape (what users screenshot).
    let client = healthy_script();
    let healthy = provider(&client, Some(&auth), Some(CONFIG)).fetch(now());
    assert_eq!(healthy.status, FetchStatus::Ok);
    assert_eq!(healthy.account.as_deref(), Some("abcd1234…"));

    let client = fixture(vec![]);
    let expired = provider(&client, Some(AUTH_EXPIRED), Some(CONFIG)).fetch(now());

    for snapshot in [&healthy, &expired] {
        let rendered = serde_json::to_string(snapshot).unwrap();
        let debug = format!("{snapshot:?}");
        assert!(
            !rendered.contains(UUID),
            "the raw account UUID leaked into the serialized snapshot: {rendered}"
        );
        assert!(
            !debug.contains(UUID),
            "the raw account UUID leaked into Debug: {debug}"
        );
        // Not just the whole UUID — no recognisable fragment beyond the prefix.
        assert!(!rendered.contains("7bd7-4ab3-8a72"));
        assert!(!debug.contains("7bd7-4ab3-8a72"));
    }
}

/// Requirement: a credential that carries an email uses it for `account`,
/// preferring it over the opaque (and masked) account id.
#[test]
fn an_email_identity_is_preferred_over_the_masked_uuid() {
    const UUID: &str = "abcd1234-5678-4abc-9def-0123456789ab";
    let auth = serde_json::json!({
        "tokens": {
            "access_token": "not-a-jwt",
            "account_id": UUID,
            "email": "user@example.com",
        },
        "last_refresh": "2026-09-09T00:00:00Z",
    })
    .to_string();

    let client = healthy_script();
    let snapshot = provider(&client, Some(&auth), Some(CONFIG)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);
    // Masked: the domain never reaches the card.
    assert_eq!(snapshot.account.as_deref(), Some("user@…"));

    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains(UUID), "the raw UUID still leaked");
}
