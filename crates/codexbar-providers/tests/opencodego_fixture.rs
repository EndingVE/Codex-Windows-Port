//! OpenCode Go fixture tests — offline.
//!
//! Two families live here:
//!
//! * **API** tests replay recorded `/zen/go/v1/usage` bodies through a
//!   [`FixtureClient`] (checked into `tests/fixtures/opencodego/`), so they assert
//!   on the request *and* the mapped windows without a socket.
//! * **Local** tests build a throwaway `opencode.db` in the temp directory with
//!   the observed schema, point [`OpenCodePaths`] at it, and check the aggregate.
//!   The provider is read-only: the fixture database must come back with no new
//!   `-wal`/`-shm`/`-journal` sidecar.
//!
//! Run with: `cargo test -p codexbar-providers`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use codexbar_core::{DataSource, FetchStatus, Provider, ProviderId, WindowKind};
use codexbar_providers::testing::{FixtureClient, FixtureResponse};
use codexbar_providers::{Env, HttpClient, OpenCodeGo, OpenCodePaths};
use rusqlite::Connection;

const API_KEY: &str = "opencode-go-test-token-0123456789abcdef";
const USAGE: &str = include_str!("fixtures/opencodego/usage.json");
const USAGE_LIVE_SHAPE: &str = include_str!("fixtures/opencodego/usage-live-shape.json");
const USAGE_ROLLING_ONLY: &str = include_str!("fixtures/opencodego/usage-rolling-only.json");
const USAGE_EMPTY: &str = include_str!("fixtures/opencodego/usage-empty.json");
const ERROR_401: &str = include_str!("fixtures/opencodego/error-401.json");
const ERROR_403_LEAKY: &str = include_str!("fixtures/opencodego/error-403-leaky.json");

/// Thursday, 2026-09-10 20:00 UTC — the ISO week starts Monday 2026-09-07.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap()
}

fn instant(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap()
}

fn provider(client: &Arc<FixtureClient>, env: Env) -> OpenCodeGo {
    OpenCodeGo::with_client(Arc::clone(client) as Arc<dyn HttpClient>, env)
}

fn fixture(script: Vec<FixtureResponse>) -> Arc<FixtureClient> {
    Arc::new(FixtureClient::new(script))
}

fn env_with_key() -> Env {
    Env::empty().with("OPENCODE_API_KEY", API_KEY)
}

/// A unique scratch directory per test, cleaned before and after use.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("codexbar-opencodego-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_auth(dir: &Path, key: &str) -> PathBuf {
    let path = dir.join("auth.json");
    let body = serde_json::json!({ "opencode-go": { "type": "api", "key": key } });
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    path
}

/// Create `opencode.db` with only the `message` table (the message-only variant).
fn seed_message_db(path: &Path, rows: &[(&str, i64, f64, &str)]) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT,
                 time_created INTEGER,
                 time_updated INTEGER,
                 data TEXT
             );",
        )
        .unwrap();
    for (index, (id, created_ms, cost, model)) in rows.iter().enumerate() {
        insert_message(
            &connection,
            id,
            &format!("s{index}"),
            *created_ms,
            *cost,
            Some(*model),
        );
    }
    // A different provider and a user turn must both be filtered out.
    connection
        .execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES ('other-provider', 's', ?1, ?1, ?2)",
            rusqlite::params![
                now().timestamp_millis(),
                r#"{"providerID":"anthropic","role":"assistant","cost":99.0}"#
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES (?1, 's', ?2, ?2, ?3)",
            rusqlite::params![
                "user-turn",
                now().timestamp_millis(),
                r#"{"providerID":"opencode-go","role":"user","cost":50.0}"#
            ],
        )
        .unwrap();
}

/// Variant that also has a `part` table, so the reader takes the step-finish path.
fn seed_part_db(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE message (
                 id TEXT PRIMARY KEY, session_id TEXT,
                 time_created INTEGER, time_updated INTEGER, data TEXT
             );
             CREATE TABLE part (
                 id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                 time_created INTEGER, time_updated INTEGER, data TEXT
             );",
        )
        .unwrap();

    // m1: cost lives on a `step-finish` part (the message cost must not double it).
    insert_message(
        &connection,
        "m1",
        "s",
        instant(2026, 9, 10, 17).timestamp_millis(),
        2.0,
        Some("a"),
    );
    insert_part(
        &connection,
        "p1",
        "m1",
        instant(2026, 9, 10, 17).timestamp_millis(),
        Some(2.0),
    );
    // m2: has cost but no `step-finish` part → counted once from the message.
    insert_message(
        &connection,
        "m2",
        "s",
        instant(2026, 9, 10, 18).timestamp_millis(),
        2.0,
        Some("b"),
    );
    // m3: a non-step part exists, but no cost on it → the message cost is used.
    insert_message(
        &connection,
        "m3",
        "s",
        instant(2026, 9, 10, 19).timestamp_millis(),
        2.0,
        Some("c"),
    );
    insert_part(
        &connection,
        "p3",
        "m3",
        instant(2026, 9, 10, 19).timestamp_millis(),
        None,
    );
}

fn insert_message(
    connection: &Connection,
    id: &str,
    session: &str,
    created_ms: i64,
    cost: f64,
    model: Option<&str>,
) {
    let data = serde_json::json!({
        "providerID": "opencode-go",
        "role": "assistant",
        "cost": cost,
        "modelID": model.unwrap_or(""),
        "time": { "created": created_ms }
    })
    .to_string();
    connection
        .execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            rusqlite::params![id, session, created_ms, data],
        )
        .unwrap();
}

fn insert_part(
    connection: &Connection,
    id: &str,
    message: &str,
    created_ms: i64,
    cost: Option<f64>,
) {
    let mut data = serde_json::json!({
        "type": if cost.is_some() { "step-finish" } else { "text" },
        "time": { "created": created_ms }
    });
    if let Some(cost) = cost {
        data["cost"] = serde_json::json!(cost);
    }
    connection
        .execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, 's', ?3, ?3, ?4)",
            rusqlite::params![id, message, created_ms, data.to_string()],
        )
        .unwrap();
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

#[test]
fn happy_path_maps_the_usage_endpoint_to_three_windows() {
    let client = fixture(vec![FixtureResponse::json(200, USAGE)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.provider, ProviderId::OpenCodeGo);
    assert_eq!(snapshot.title, "OpenCode Go");
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.error, None);
    assert_eq!(
        snapshot.account.as_deref(),
        Some(codexbar_providers::redact(API_KEY).as_str())
    );

    assert_eq!(snapshot.windows.len(), 3);
    let session = &snapshot.windows[0];
    assert_eq!(session.id, "session");
    assert_eq!(session.title, "Session · 5h");
    assert_eq!(session.kind, WindowKind::Session);
    assert_eq!(session.window.used_percent, 35.5);
    assert_eq!(session.window.window_minutes, Some(300));
    assert_eq!(
        session.window.resets_at,
        Some(
            now()
                .checked_add_signed(chrono::Duration::seconds(3600))
                .unwrap()
        )
    );

    let weekly = &snapshot.windows[1];
    assert_eq!(weekly.id, "weekly");
    assert_eq!(weekly.kind, WindowKind::Weekly);
    assert_eq!(weekly.window.used_percent, 20.0);
    assert_eq!(weekly.window.window_minutes, Some(10_080));

    let monthly = &snapshot.windows[2];
    assert_eq!(monthly.id, "monthly");
    assert_eq!(monthly.kind, WindowKind::Extra);
    assert_eq!(monthly.window.used_percent, 5.0);
    assert_eq!(monthly.window.window_minutes, Some(43_200));

    // The credential is never in the payload.
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(API_KEY));
}

#[test]
fn the_live_percent_and_resets_at_shape_is_mapped() {
    // Recorded shape of `GET /zen/go/v1/usage` on this machine: each window is
    // `{ percent, resetsAt, status }`, not `{ usagePercent, resetInSec }`.
    let client = fixture(vec![FixtureResponse::json(200, USAGE_LIVE_SHAPE)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.windows.len(), 3);
    assert_eq!(snapshot.windows[0].window.used_percent, 19.0);
    assert_eq!(snapshot.windows[1].window.used_percent, 29.0);
    assert_eq!(snapshot.windows[2].window.used_percent, 67.0);
    assert_eq!(
        snapshot.windows[0].window.resets_at,
        Some(instant(2026, 9, 10, 22))
    );
    assert_eq!(
        snapshot.windows[2].window.resets_at,
        Some(Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap())
    );
}

#[test]
fn requests_are_bearer_authenticated_against_the_documented_endpoint() {
    let client = fixture(vec![FixtureResponse::json(200, USAGE)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Ok);

    let requests = client.captured();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0]
        .url
        .starts_with("https://opencode.ai/zen/go/v1/usage"));
    assert!(requests[0].path_ends_with("/zen/go/v1/usage"));
    assert_eq!(
        requests[0].authorization().as_deref(),
        Some(format!("Bearer {API_KEY}").as_str())
    );
    assert_eq!(requests[0].header("Accept"), Some("application/json"));
}

#[test]
fn rolling_only_publishes_a_single_session_lane() {
    let client = fixture(vec![FixtureResponse::json(200, USAGE_ROLLING_ONLY)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].id, "session");
    assert_eq!(snapshot.windows[0].window.used_percent, 12.5);
}

#[test]
fn missing_credentials_are_not_configured_and_send_nothing() {
    let client = fixture(vec![]);
    let snapshot = provider(&client, Env::empty()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::NotConfigured);
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.account.is_none());
    let hint = snapshot.error.as_deref().unwrap_or_default();
    assert!(hint.contains("OPENCODE_API_KEY"), "{hint}");
    assert_eq!(client.request_count(), 0, "no credentials, no request");
}

#[test]
fn auth_json_token_is_used_as_the_fallback_bearer() {
    let scratch = Scratch::new("auth");
    let auth = write_auth(scratch.path(), "auth-json-token-0987654321");
    let paths = OpenCodePaths::new(&auth, scratch.path().join("does-not-exist.db"));

    let client = fixture(vec![FixtureResponse::json(200, USAGE)]);
    let snapshot =
        OpenCodeGo::with_client(Arc::clone(&client) as Arc<dyn HttpClient>, Env::empty())
            .with_paths(Some(paths))
            .fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(
        client.captured()[0].authorization().as_deref(),
        Some("Bearer auth-json-token-0987654321")
    );
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains("auth-json-token-0987654321"));
}

#[test]
fn api_failure_without_local_history_is_an_error() {
    let client = fixture(vec![FixtureResponse::json(401, ERROR_401)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("status"), "{message}");
    assert!(!message.contains(API_KEY));
}

#[test]
fn an_http_override_fails_closed_before_the_bearer_is_attached() {
    let client = fixture(vec![]);
    let env = env_with_key().with(
        "OPENCODE_GO_USAGE_URL",
        "http://opencode.ai/zen/go/v1/usage",
    );
    let snapshot = provider(&client, env).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let message = snapshot.error.as_deref().unwrap_or_default();
    assert!(message.contains("OPENCODE_GO_USAGE_URL"), "{message}");
    assert!(message.contains("HTTPS"), "{message}");
    assert_eq!(
        client.request_count(),
        0,
        "a plaintext override must never receive the token"
    );
}

#[test]
fn an_unexpected_body_shape_is_an_error_not_a_panic() {
    let client = fixture(vec![FixtureResponse::json(200, USAGE_EMPTY)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);

    // A 200 with an HTML body (captive portal, proxy) is handled too.
    let client = fixture(vec![FixtureResponse::text(200, "<html>nope</html>")]);
    let snapshot = provider(&client, env_with_key()).fetch(now());
    assert_eq!(snapshot.status, FetchStatus::Error);
}

#[test]
fn an_upstream_error_body_never_leaks_a_credential() {
    let client = fixture(vec![FixtureResponse::json(403, ERROR_403_LEAKY)]);
    let snapshot = provider(&client, env_with_key()).fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains("sk-opencode-leak-0123456789abcdef"));
}

#[test]
fn fetch_uses_the_supplied_now_for_fetched_at() {
    let client = fixture(vec![FixtureResponse::json(200, USAGE)]);
    let instant = now();
    let snapshot = provider(&client, env_with_key()).fetch(instant);
    assert_eq!(snapshot.fetched_at, instant);
}

// ---------------------------------------------------------------------------
// Local database (read-only)
// ---------------------------------------------------------------------------

#[test]
fn local_opencode_db_is_aggregated_into_three_estimated_windows() {
    let scratch = Scratch::new("local");
    let database = scratch.path().join("opencode.db");
    // earliest row 2026-09-01 anchors the monthly window; nothing in the fixture
    // touches the 5 h / week boundaries except where intended.
    seed_message_db(
        &database,
        &[
            (
                "anchor",
                instant(2026, 9, 1, 0).timestamp_millis(),
                0.5,
                "m",
            ),
            (
                "session",
                instant(2026, 9, 10, 19).timestamp_millis(),
                6.0,
                "m",
            ),
            ("week", instant(2026, 9, 8, 10).timestamp_millis(), 3.0, "m"),
            (
                "month",
                instant(2026, 9, 3, 10).timestamp_millis(),
                17.5,
                "m",
            ),
        ],
    );

    let paths = OpenCodePaths::new(scratch.path().join("auth.json"), &database);
    let client = fixture(vec![]);
    let snapshot =
        OpenCodeGo::with_client(Arc::clone(&client) as Arc<dyn HttpClient>, Env::empty())
            .with_paths(Some(paths))
            .fetch(now());

    assert_eq!(client.request_count(), 0, "no credentials means no network");
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.source, DataSource::Cli);
    assert!(snapshot.account.is_none());

    assert_eq!(snapshot.windows.len(), 3);
    // session 6.0/12 = 50 %, weekly 9.0/30 = 30 %, monthly 27.0/60 = 45 %.
    assert_eq!(snapshot.windows[0].id, "session");
    assert_eq!(snapshot.windows[0].window.used_percent, 50.0);
    assert_eq!(snapshot.windows[0].window.window_minutes, Some(300));
    assert_eq!(snapshot.windows[1].id, "weekly");
    assert_eq!(snapshot.windows[1].window.used_percent, 30.0);
    assert_eq!(snapshot.windows[2].id, "monthly");
    assert_eq!(snapshot.windows[2].window.window_minutes, Some(43_200));
    assert_eq!(snapshot.windows[2].window.used_percent, 45.0);

    // The oldest in-session row (19:00) resets 4 h later.
    assert_eq!(
        snapshot.windows[0].window.resets_at,
        Some(instant(2026, 9, 11, 0))
    );

    let note = snapshot.error.as_deref().unwrap_or_default();
    assert!(note.contains("Estimated"), "{note}");

    // Read-only: the reader must not have created any sidecar next to the DB.
    assert!(
        !scratch.path().join("opencode.db-wal").exists()
            && !scratch.path().join("opencode.db-shm").exists()
            && !scratch.path().join("opencode.db-journal").exists(),
        "the provider recreated SQLite sidecars beside the user's database"
    );
}

#[test]
fn local_opencode_db_with_parts_counts_step_finish_rows_once() {
    let scratch = Scratch::new("parts");
    let database = scratch.path().join("opencode.db");
    seed_part_db(&database);

    let paths = OpenCodePaths::new(scratch.path().join("auth.json"), &database);
    let client = fixture(vec![]);
    let snapshot =
        OpenCodeGo::with_client(Arc::clone(&client) as Arc<dyn HttpClient>, Env::empty())
            .with_paths(Some(paths))
            .fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.windows.len(), 3);
    // 2 + 2 + 2 = 6.0 over the 12 USD session limit → 50 %, and no double count.
    assert_eq!(snapshot.windows[0].window.used_percent, 50.0);
    assert!(!scratch.path().join("opencode.db-wal").exists());
}

#[test]
fn api_overlays_the_local_estimate_with_authoritative_windows() {
    let scratch = Scratch::new("overlay");
    let database = scratch.path().join("opencode.db");
    seed_message_db(
        &database,
        &[(
            "session",
            instant(2026, 9, 10, 19).timestamp_millis(),
            6.0,
            "m",
        )],
    );

    let paths = OpenCodePaths::new(scratch.path().join("auth.json"), &database);
    let client = fixture(vec![FixtureResponse::json(200, USAGE)]);
    let snapshot =
        OpenCodeGo::with_client(Arc::clone(&client) as Arc<dyn HttpClient>, env_with_key())
            .with_paths(Some(paths))
            .fetch(now());

    // The API answered, so its numbers win over the local 50 %.
    assert_eq!(snapshot.status, FetchStatus::Ok);
    assert_eq!(snapshot.source, DataSource::ApiKey);
    assert_eq!(snapshot.windows[0].window.used_percent, 35.5);
    assert_eq!(snapshot.error, None);
    assert_eq!(client.request_count(), 1);
}

#[test]
fn an_unreadable_local_database_is_reported_without_panicking() {
    let scratch = Scratch::new("corrupt");
    let database = scratch.path().join("opencode.db");
    std::fs::write(&database, b"this is definitely not a SQLite file").unwrap();

    let paths = OpenCodePaths::new(scratch.path().join("auth.json"), &database);
    let client = fixture(vec![]);
    let snapshot =
        OpenCodeGo::with_client(Arc::clone(&client) as Arc<dyn HttpClient>, Env::empty())
            .with_paths(Some(paths))
            .fetch(now());

    assert_eq!(snapshot.status, FetchStatus::Error);
    assert!(snapshot
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("unavailable"));
}
