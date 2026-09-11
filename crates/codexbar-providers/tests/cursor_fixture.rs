//! Cursor fixture tests — offline, deterministic, no cursor.com and no Cursor.
//!
//! The provider reads a real SQLite `state.vscdb` from disk. These tests ship
//! small generated databases under `tests/fixtures/cursor/` (an `ItemTable` with
//! a `cursorAuth/accessToken` row) and point the provider at them with
//! `CURSOR_STATE_DB`, so the actual DB path — including the WAL overlay and the
//! "never create a file" guarantee — is exercised, not mocked out.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Cursor, Env, HttpClient};

const DB_VALID: &[u8] = include_bytes!("fixtures/cursor/state.vscdb");
const DB_BLOB: &[u8] = include_bytes!("fixtures/cursor/state-blob.vscdb");
const DB_EMPTY: &[u8] = include_bytes!("fixtures/cursor/state-empty.vscdb");
const DB_EXPIRED: &[u8] = include_bytes!("fixtures/cursor/state-expired.vscdb");
const DB_WAL_MAIN: &[u8] = include_bytes!("fixtures/cursor/state-wal-main.vscdb");
const DB_WAL_SIDECAR: &[u8] = include_bytes!("fixtures/cursor/state-wal-main.vscdb-wal");

const USAGE_SUMMARY: &str = include_str!("fixtures/cursor/usage-summary.json");
const USAGE_SUMMARY_LEGACY: &str = include_str!("fixtures/cursor/usage-summary-legacy.json");
const AUTH_ME: &str = include_str!("fixtures/cursor/auth-me.json");
const USAGE_REQUESTS: &str = include_str!("fixtures/cursor/usage-requests.json");
const USAGE_REQUESTS_NONE: &str = include_str!("fixtures/cursor/usage-requests-none.json");
const SAND_USAGE: &str = include_str!("fixtures/cursor/sand-usage.json");
const SAND_USAGE_ZERO: &str = include_str!("fixtures/cursor/sand-usage-zero.json");
const ERROR_401: &str = include_str!("fixtures/cursor/error-401.json");

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

/// A private scratch directory per test name, so parallel tests never collide.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("codexbar-cursor-{}-{}", std::process::id(), name));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, bytes).unwrap();
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

fn env_for(db: &Path) -> Env {
    Env::empty().with("CURSOR_STATE_DB", db.to_string_lossy().to_string())
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> Cursor {
    Cursor::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

#[test]
fn happy_path_reads_the_local_session_and_maps_every_lane() {
    let scratch = Scratch::new("happy");
    let db = scratch.write("state.vscdb", DB_VALID);
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_SUMMARY),
        FixtureResponse::json(200, AUTH_ME),
        FixtureResponse::json(200, USAGE_REQUESTS_NONE),
        FixtureResponse::json(200, SAND_USAGE),
    ]);

    let snapshot = provider(&client, env_for(&db)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok, "{:?}", snapshot.error);
    assert_eq!(snapshot.provider, ProviderId::Cursor);
    assert_eq!(snapshot.source, DataSource::OAuth);
    // The domain never reaches the card: the identity is masked.
    assert_eq!(snapshot.account.as_deref(), Some("user@…"));
    assert_eq!(snapshot.plan.as_deref(), Some("Cursor Pro"));

    // Plan 58.25, Cursor models 61.5, third-party 12, on-demand 35, Grok 42.5.
    let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "cursor-plan",
            "cursor-auto",
            "cursor-api",
            "cursor-on-demand",
            "cursor-grok-bot"
        ]
    );
    assert_eq!(snapshot.windows[0].window.used_percent, 58.25);
    assert_eq!(snapshot.windows[0].kind, WindowKind::Weekly);
    assert_eq!(snapshot.windows[0].window.window_minutes, Some(43_200));
    assert_eq!(snapshot.windows[1].window.used_percent, 61.5);
    assert_eq!(snapshot.windows[1].kind, WindowKind::WeeklyScoped);
    assert_eq!(snapshot.windows[2].window.used_percent, 12.0);
    assert_eq!(snapshot.windows[3].window.used_percent, 35.0);
    assert_eq!(snapshot.windows[4].window.used_percent, 42.5);

    // The token must never survive into the payload.
    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains("eyJ"));
    assert!(!rendered.contains("WorkosCursorSessionToken"));
}

#[test]
fn requests_carry_the_derived_cookie_and_the_csrf_origin() {
    let scratch = Scratch::new("requests");
    let db = scratch.write("state.vscdb", DB_VALID);
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_SUMMARY),
        FixtureResponse::json(200, AUTH_ME),
        FixtureResponse::json(200, USAGE_REQUESTS_NONE),
        FixtureResponse::json(200, SAND_USAGE),
    ]);
    provider(&client, env_for(&db)).fetch(now());

    let requests = client.captured();
    assert_eq!(requests.len(), 4);

    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].path_ends_with("/api/usage-summary"));
    let cookie = requests[0].header("Cookie").unwrap_or_default();
    assert!(
        cookie.starts_with("WorkosCursorSessionToken=user_abc123%3A%3A"),
        "{cookie}"
    );
    assert_eq!(requests[0].header("Origin"), Some("https://cursor.com"));

    assert!(requests[1].path_ends_with("/api/auth/me"));
    // The legacy probe carries the user id derived from the JWT subject.
    assert!(requests[2].url.contains("/api/usage?user=user_abc123"));
    assert_eq!(requests[3].method.as_str(), "POST");
    assert!(requests[3].path_ends_with("/api/dashboard/get-sand-usage-status"));
    assert_eq!(requests[3].header("Origin"), Some("https://cursor.com"));
}

#[test]
fn a_legacy_request_plan_replaces_the_plan_lane_and_hides_the_model_lanes() {
    let scratch = Scratch::new("legacy");
    let db = scratch.write("state.vscdb", DB_VALID);
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_SUMMARY_LEGACY),
        FixtureResponse::json(200, AUTH_ME),
        FixtureResponse::json(200, USAGE_REQUESTS),
        FixtureResponse::json(200, SAND_USAGE_ZERO),
    ]);

    let snapshot = provider(&client, env_for(&db)).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);

    let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, vec!["cursor-requests", "cursor-on-demand"]);
    // 120 / 500 requests.
    assert_eq!(snapshot.windows[0].window.used_percent, 24.0);
    // plan.used/limit fallback (250 / 1000) never runs while a request quota exists.
    assert!(!ids.contains(&"cursor-plan"));
    // Zero Grok allowance means no Bot lane.
    assert!(!ids.contains(&"cursor-grok-bot"));
}

#[test]
fn a_missing_database_is_not_configured_and_sends_nothing() {
    let scratch = Scratch::new("missing");
    let missing = scratch.dir.join("does-not-exist.vscdb");
    let client = fixture(vec![]);
    let snapshot = provider(&client, env_for(&missing)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot.account.is_none());
    assert!(snapshot.windows.is_empty());
    assert_eq!(client.request_count(), 0, "no session, no request");
    assert_eq!(scratch.names(), Vec::<String>::new(), "nothing was created");
}

#[test]
fn an_empty_item_table_is_not_configured() {
    let scratch = Scratch::new("empty");
    let db = scratch.write("state.vscdb", DB_EMPTY);
    let client = fixture(vec![]);
    let snapshot = provider(&client, env_for(&db)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("cursorAuth/accessToken"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn an_expired_token_is_not_configured_not_a_network_error() {
    let scratch = Scratch::new("expired");
    let db = scratch.write("state.vscdb", DB_EXPIRED);
    let client = fixture(vec![]);
    let snapshot = provider(&client, env_for(&db)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .to_lowercase()
        .contains("expired"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn a_utf16le_blob_token_is_decoded_before_utf8() {
    let scratch = Scratch::new("blob");
    let db = scratch.write("state.vscdb", DB_BLOB);
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_SUMMARY),
        FixtureResponse::json(200, AUTH_ME),
        FixtureResponse::json(200, USAGE_REQUESTS_NONE),
        FixtureResponse::json(200, SAND_USAGE),
    ]);
    let snapshot = provider(&client, env_for(&db)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok, "{:?}", snapshot.error);
    let captured = client.captured();
    let cookie = captured[0].header("Cookie").unwrap_or_default();
    assert!(cookie.starts_with("WorkosCursorSessionToken=user_abc123%3A%3A"));
}

#[test]
fn wal_frames_are_read_and_no_sidecar_file_is_created() {
    let scratch = Scratch::new("wal");
    // The main file predates the insert: the token lives only in the WAL.
    let db = scratch.write("state.vscdb", DB_WAL_MAIN);
    scratch.write("state.vscdb-wal", DB_WAL_SIDECAR);
    let before = scratch.names();

    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_SUMMARY),
        FixtureResponse::json(200, AUTH_ME),
        FixtureResponse::json(200, USAGE_REQUESTS_NONE),
        FixtureResponse::json(200, SAND_USAGE),
    ]);
    let snapshot = provider(&client, env_for(&db)).fetch(now());

    assert_eq!(
        snapshot.status,
        FetchStatus::Ok,
        "the token is only in the WAL: {:?}",
        snapshot.error
    );
    assert_eq!(
        scratch.names(),
        before,
        "reading must never create a -wal/-shm file in Cursor's directory"
    );
    assert!(!scratch.names().iter().any(|name| name.ends_with("-shm")));
}

#[test]
fn a_manual_cookie_header_is_honoured_when_cursor_is_absent() {
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_SUMMARY),
        FixtureResponse::json(200, AUTH_ME),
        FixtureResponse::json(200, USAGE_REQUESTS_NONE),
        FixtureResponse::json(200, SAND_USAGE),
    ]);
    let env = Env::empty().with(
        "CURSOR_COOKIE_HEADER",
        "WorkosCursorSessionToken=pasted%3A%3Atoken",
    );
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.source, DataSource::Web);
    let captured = client.captured();
    assert_eq!(
        captured[0].header("Cookie"),
        Some("WorkosCursorSessionToken=pasted%3A%3Atoken")
    );
    // No local JWT means no subject, so the legacy request probe is skipped.
    assert!(!client
        .captured()
        .iter()
        .any(|request| request.url.contains("/api/usage?")));
}

#[test]
fn a_rejected_session_becomes_an_error_snapshot() {
    let scratch = Scratch::new("401");
    let db = scratch.write("state.vscdb", DB_VALID);
    let client = fixture(vec![FixtureResponse::json(401, ERROR_401)]);
    let snapshot = provider(&client, env_for(&db)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("401"), "{message}");
    assert!(!message.contains("eyJ"));
    assert_eq!(
        client.request_count(),
        1,
        "a failed summary stops the fetch"
    );
}

#[test]
fn a_corrupt_database_is_an_error_not_a_panic() {
    let scratch = Scratch::new("corrupt");
    let db = scratch.write("state.vscdb", b"this is not a sqlite database at all");
    let client = fixture(vec![]);
    let snapshot = provider(&client, env_for(&db)).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("state database"));
    assert_eq!(client.request_count(), 0);
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let scratch = Scratch::new("now");
    let db = scratch.write("state.vscdb", DB_VALID);
    let client = fixture(vec![
        FixtureResponse::json(200, USAGE_SUMMARY),
        FixtureResponse::json(200, AUTH_ME),
        FixtureResponse::json(200, USAGE_REQUESTS_NONE),
        FixtureResponse::json(200, SAND_USAGE),
    ]);
    let instant = now();
    let snapshot = provider(&client, env_for(&db)).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}
