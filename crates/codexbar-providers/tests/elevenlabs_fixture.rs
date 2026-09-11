//! ElevenLabs fixture tests — offline, deterministic.
//!
//! `FixtureClient` replays `tests/fixtures/elevenlabs/` and records what the
//! provider sent. The `xi-api-key` header is the only authentication path; the
//! `Authorization` header must stay unused. Run with:
//! `cargo test -p codexbar-providers elevenlabs`

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{ElevenLabs, Env, HttpClient};

const API_KEY: &str = "elevenlabs-fixture-key-0001";
const SUBSCRIPTION: &str = include_str!("fixtures/elevenlabs/subscription.json");
const ERROR_401: &str = include_str!("fixtures/elevenlabs/error-401-invalid-key.json");
const ERROR_403: &str = include_str!("fixtures/elevenlabs/error-403-permissions.json");

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> ElevenLabs {
    ElevenLabs::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn env_with_key() -> Env {
    Env::empty().with("ELEVENLABS_API_KEY", API_KEY)
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < 1e-9
}

#[test]
fn happy_path_maps_characters_and_voice_slots_to_windows() {
    let client = fixture(vec![FixtureResponse::json(200, SUBSCRIPTION)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::ElevenLabs);
    assert_eq!(snapshot.title, "ElevenLabs");
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.plan.as_deref(), Some("Creator"));
    assert_eq!(
        snapshot.account.as_deref(),
        Some(codexbar_providers::redact(API_KEY).as_str())
    );

    assert_eq!(snapshot.windows.len(), 3);

    let characters = &snapshot.windows[0];
    assert_eq!(characters.id, "characters");
    assert_eq!(characters.kind, WindowKind::Extra);
    assert!(close(characters.window.used_percent, 25.0));
    assert_eq!(
        characters.window.reset_description.as_deref(),
        Some("25,000 / 100,000 credits")
    );
    assert_eq!(
        characters.window.resets_at,
        Utc.timestamp_opt(1_780_000_000, 0).single()
    );

    let voices = &snapshot.windows[1];
    assert_eq!(voices.id, "voice-slots");
    assert!(close(voices.window.used_percent, 30.0));
    assert_eq!(voices.window.reset_description.as_deref(), Some("3 / 10"));

    let professional = &snapshot.windows[2];
    assert_eq!(professional.id, "professional-voices");
    assert!(close(professional.window.used_percent, 20.0));

    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains(API_KEY));
}

#[test]
fn requests_use_the_xi_api_key_header_and_never_authorization() {
    let client = fixture(vec![FixtureResponse::json(200, SUBSCRIPTION)]);
    provider(&client, env_with_key()).fetch(now());

    let requests = client.captured();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].path_ends_with("/v1/user/subscription"));
    assert_eq!(requests[0].header("xi-api-key"), Some(API_KEY));
    assert_eq!(requests[0].header("Accept"), Some("application/json"));
    assert!(
        requests[0].header("Authorization").is_none(),
        "ElevenLabs uses xi-api-key, not bearer auth"
    );
    assert!(requests[0].url.starts_with("https://api.elevenlabs.io/"));
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
    assert!(hint.contains("ELEVENLABS_API_KEY"), "{hint}");
    assert!(hint.contains("XI_API_KEY"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn the_xi_api_key_alias_is_cleaned_and_used() {
    let client = fixture(vec![FixtureResponse::json(200, SUBSCRIPTION)]);
    let env = Env::empty().with("XI_API_KEY", format!("  \"{API_KEY}\"  "));
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(client.captured()[0].header("xi-api-key"), Some(API_KEY));
}

#[test]
fn a_401_identifies_an_invalid_key() {
    let client = fixture(vec![FixtureResponse::json(401, ERROR_401)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("not been revoked"), "{message}");
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(API_KEY));
}

#[test]
fn a_403_identifies_a_missing_permission() {
    let client = fixture(vec![FixtureResponse::json(403, ERROR_403)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("user_read permission"), "{message}");
}

#[test]
fn a_plaintext_override_fails_closed_before_the_key_is_attached() {
    let client = fixture(vec![]);
    let env = env_with_key().with("ELEVENLABS_API_URL", "http://api.elevenlabs.io");
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("ELEVENLABS_API_URL"), "{message}");
    assert!(message.contains("HTTPS"), "{message}");
    assert_eq!(client.request_count(), 0);
}

#[test]
fn a_v1_suffixed_https_override_is_followed() {
    let client = fixture(vec![FixtureResponse::json(200, SUBSCRIPTION)]);
    let env = env_with_key().with("ELEVENLABS_API_URL", "https://proxy.example.com/v1");
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(
        client.captured()[0].url,
        "https://proxy.example.com/v1/user/subscription"
    );
}

#[test]
fn a_body_without_the_character_fields_is_an_error_not_a_zero_account() {
    let client = fixture(vec![FixtureResponse::json(200, r#"{"tier":"creator"}"#)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(!snapshot.error.as_deref().unwrap_or_default().is_empty());
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![FixtureResponse::json(200, SUBSCRIPTION)]);
    let instant = now();
    let snapshot = provider(&client, env_with_key()).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
