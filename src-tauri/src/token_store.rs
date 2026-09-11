//! Persisting the credential the GitHub **device flow** issues.
//!
//! The device flow itself lives with the fetcher
//! (`codexbar_providers::providers::copilot::DeviceFlow`), but the token it
//! returns belongs to the port's own `config.json` — the file
//! `codexbar_providers::credential::PortConfig` already reads. This module is the
//! one place that writes it.
//!
//! **Schema.** The provider resolves its token with
//! [`codexbar_providers::PortConfig::resolve_api_key`], which reads
//! `providers[id="copilot"].tokenAccounts.accounts[activeIndex].token` first and
//! `providers[].apiKey` second. We write the token-account form:
//!
//! ```json
//! { "providers": [ { "id": "copilot", "enabled": true,
//!   "tokenAccounts": { "activeIndex": 0,
//!     "accounts": [ { "token": "<issued>", "label": "GitHub device flow (Copilot)" } ] } } ] }
//! ```
//!
//! **Rules this module keeps:**
//!  * the token is written **only** through
//!    [`codexbar_providers::write_json_atomic`] (temp file + rename, with a
//!    `.bak` of the previous contents) — a crash can never truncate the config;
//!  * the whole file is read-modify-written as raw JSON, so a provider entry's
//!    `apiKey`, a hand-edited `providers` order, unknown top-level keys and the
//!    rest of the config are preserved byte-for-byte apart from the one field we
//!    touch;
//!  * a config that cannot be parsed is left untouched and the write fails
//!    loudly instead of clobbering something we do not understand;
//!  * the token value never appears in a [`StoredToken`], in a log line, or in
//!    any error message — the only public trace is [`Secret::redacted`].

use std::path::{Path, PathBuf};

use codexbar_providers::{write_json_atomic, Secret};
use serde::Serialize;
use serde_json::{Map, Value};

/// Field name inside one `providers[]` entry.
pub const TOKEN_ACCOUNTS_FIELD: &str = "tokenAccounts";

/// What a successful store can tell the caller. Safe to print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredToken {
    /// Provider the account is filed under (`copilot`).
    pub provider: String,
    /// Human label of the account, shown in Settings.
    pub label: String,
    /// `gho_ab…9f2c` — a stub for the UI, never the token.
    pub redacted: String,
    /// File the token was written to.
    pub path: String,
    /// True when an existing copilot account was overwritten (a re-login).
    pub replaced: bool,
}

/// Where the port's config lives: `%APPDATA%\CodexBar\config.json`.
pub fn config_path() -> PathBuf {
    crate::settings::config_path()
}

/// Store (or replace) the single active Copilot token account.
///
/// `provider` is the machine id (`copilot`). Empty tokens are rejected: an
/// empty credential must never be written over a good one.
pub fn store_device_token(
    path: &Path,
    provider: &str,
    token: &Secret,
    label: &str,
) -> Result<StoredToken, String> {
    let cleaned = token.expose().trim();
    if cleaned.is_empty() {
        return Err("refusing to store an empty credential".to_string());
    }

    let mut root = read_root(path)?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))?;

    let providers = object
        .entry("providers")
        .or_insert_with(|| Value::Array(Vec::new()));
    let providers = providers
        .as_array_mut()
        .ok_or_else(|| format!("{}: \"providers\" is not an array", path.display()))?;

    let entry = match providers
        .iter_mut()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(provider))
    {
        Some(entry) => entry,
        None => {
            let mut fresh = Map::new();
            fresh.insert("id".to_string(), Value::String(provider.to_string()));
            fresh.insert("enabled".to_string(), Value::Bool(true));
            providers.push(Value::Object(fresh));
            providers.last_mut().expect("just pushed")
        }
    };

    let entry = entry
        .as_object_mut()
        .ok_or_else(|| format!("{provider} entry is not a JSON object"))?;

    let replaced = entry
        .get(TOKEN_ACCOUNTS_FIELD)
        .and_then(|accounts| accounts.get("accounts"))
        .and_then(Value::as_array)
        .is_some_and(|accounts| !accounts.is_empty());

    let mut account = Map::new();
    account.insert("token".to_string(), Value::String(cleaned.to_string()));
    account.insert("label".to_string(), Value::String(label.to_string()));

    let mut accounts = Map::new();
    accounts.insert("activeIndex".to_string(), Value::Number(0.into()));
    accounts.insert(
        "accounts".to_string(),
        Value::Array(vec![Value::Object(account)]),
    );
    entry.insert(TOKEN_ACCOUNTS_FIELD.to_string(), Value::Object(accounts));

    write_json_atomic(path, &root)
        .map_err(|e| format!("could not write the credential to {}: {e}", path.display()))?;

    Ok(StoredToken {
        provider: provider.to_string(),
        label: label.to_string(),
        redacted: token.redacted(),
        path: path.to_string_lossy().into_owned(),
        replaced,
    })
}

/// Read the config as raw JSON, or an empty object when the file does not exist.
///
/// A parse failure is an error, not a silent reset: this module must not replace
/// a config it cannot understand.
fn read_root(path: &Path) -> Result<Value, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} is not valid JSON: {e}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(err) => Err(format!("could not read {}: {err}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codexbar_core::{FetchStatus, Provider, ProviderId};
    use codexbar_providers::providers::copilot::Copilot;
    use codexbar_providers::testing::{FixtureClient, FixtureResponse};
    use codexbar_providers::{Env, HttpClient, PortConfig};
    use std::sync::Arc;

    /// A fake GitHub OAuth token — never a real credential.
    const TOKEN: &str = "gho_fixture_device_flow_token";

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codexbar-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    /// The provider reads back exactly what the store wrote — the two halves of
    /// the round-trip are the same schema.
    #[test]
    fn round_trips_through_the_provider_resolver() {
        let path = scratch("round-trip");
        let stored = store_device_token(
            &path,
            ProviderId::Copilot.as_str(),
            &Secret::new(TOKEN),
            "GitHub device flow (Copilot)",
        )
        .unwrap();
        assert_eq!(stored.provider, "copilot");
        assert!(!stored.replaced);

        let env = Env::empty().with("CODEXBAR_CONFIG", path.to_string_lossy().to_string());
        let config = PortConfig::load(&env).unwrap().unwrap();
        assert_eq!(
            config
                .resolve_api_key(ProviderId::Copilot, &env, &["COPILOT_API_TOKEN"])
                .unwrap()
                .expose(),
            TOKEN
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// The rest of the config survives: other providers, their API keys, extra
    /// fields and unknown top-level keys.
    #[test]
    fn preserves_every_other_field_and_provider() {
        let path = scratch("preserve");
        std::fs::write(
            &path,
            r#"{"providers":[
                 {"id":"codex","enabled":true},
                 {"id":"openrouter","apiKey":"sk-or-v1-keepme","customField":42}
               ],"refreshIntervalSecs":300,"unknownTopLevel":{"a":1}}"#,
        )
        .unwrap();

        store_device_token(&path, "copilot", &Secret::new(TOKEN), "lbl").unwrap();

        let env = Env::empty().with("CODEXBAR_CONFIG", path.to_string_lossy().to_string());
        let config = PortConfig::load(&env).unwrap().unwrap();
        assert_eq!(
            config.api_key(ProviderId::OpenRouter).unwrap().expose(),
            "sk-or-v1-keepme"
        );
        assert_eq!(
            config
                .field(ProviderId::OpenRouter, "customField")
                .as_deref(),
            None
        );
        let root: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(root["refreshIntervalSecs"], 300);
        assert_eq!(root["unknownTopLevel"]["a"], 1);
        assert_eq!(root["providers"].as_array().unwrap().len(), 3);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Writing twice leaves one account and reports the replacement — a re-login
    /// does not stack up dead tokens.
    #[test]
    fn a_second_store_replaces_the_single_account() {
        let path = scratch("replace");
        store_device_token(&path, "copilot", &Secret::new("first-token-value"), "lbl").unwrap();
        let second =
            store_device_token(&path, "copilot", &Secret::new("second-token-value"), "lbl")
                .unwrap();
        assert!(second.replaced);

        let root: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let entry = root["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "copilot")
            .unwrap();
        let accounts = entry["tokenAccounts"]["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["token"], "second-token-value");

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Atomicity: the temp file never survives a successful write, and a `.bak`
    /// of the previous contents is kept for recovery.
    #[test]
    fn the_write_is_atomic_and_never_leaves_a_temp_file() {
        let path = scratch("atomic");
        store_device_token(&path, "copilot", &Secret::new(TOKEN), "lbl").unwrap();

        let dir = path.parent().unwrap();
        let names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n.contains(".tmp")),
            "temp file left behind: {names:?}"
        );
        assert!(path.is_file());

        // A second write keeps the previous revision at config.json.bak.
        store_device_token(&path, "copilot", &Secret::new("second-token-value"), "lbl").unwrap();
        assert!(dir.join("config.json.bak").is_file());

        std::fs::remove_dir_all(dir).ok();
    }

    /// The token never appears in anything this module renders.
    #[test]
    fn the_token_never_reaches_a_rendered_value() {
        let path = scratch("redaction");
        let stored = store_device_token(&path, "copilot", &Secret::new(TOKEN), "lbl").unwrap();
        let rendered = format!("{stored:?} {}", serde_json::to_string(&stored).unwrap());
        assert!(!rendered.contains("fixture_device_flow"));
        assert!(rendered.contains("gho_fi…oken") || rendered.contains('…'));

        // The on-disk file does contain it (that is the point), but nothing the
        // module returns does.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains(TOKEN));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// An empty credential is refused rather than written.
    #[test]
    fn an_empty_token_is_refused() {
        let path = scratch("empty");
        assert!(store_device_token(&path, "copilot", &Secret::new("   "), "lbl").is_err());
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A config we cannot parse is left exactly as it was.
    #[test]
    fn an_unparseable_config_is_never_clobbered() {
        let path = scratch("corrupt");
        std::fs::write(&path, "{ this is not json").unwrap();
        assert!(store_device_token(&path, "copilot", &Secret::new(TOKEN), "lbl").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ this is not json"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// End to end, offline: before the store the provider returns
    /// `notConfigured` and opens nothing; after it, the *same* provider issues a
    /// real usage probe against the fixture script.
    #[test]
    fn a_stored_token_moves_copilot_from_not_configured_to_a_usage_probe() {
        const USAGE: &str = include_str!(
            "../../crates/codexbar-providers/tests/fixtures/copilot/usage-premium-chat.json"
        );
        const USER: &str =
            include_str!("../../crates/codexbar-providers/tests/fixtures/copilot/user.json");

        let path = scratch("provider");
        let now = chrono::Utc::now();

        // No config, no env token: Copilot is honestly "not configured" and the
        // fixture client is never touched.
        let client = Arc::new(FixtureClient::new(vec![
            FixtureResponse::json(200, USAGE),
            FixtureResponse::json(200, USER),
        ]));
        let before = Copilot::with_client(Arc::clone(&client) as Arc<dyn HttpClient>, Env::empty())
            .fetch(now);
        assert_eq!(before.status, FetchStatus::NotConfigured);
        assert_eq!(client.request_count(), 0, "no credential → no request");

        // After the device flow's token is stored, the same provider now probes
        // the usage API and maps the fixture onto windows.
        store_device_token(&path, "copilot", &Secret::new(TOKEN), "lbl").unwrap();
        let env = Env::empty().with("CODEXBAR_CONFIG", path.to_string_lossy().to_string());
        let config = PortConfig::load(&env).unwrap().unwrap();
        let after = Copilot::with_client(Arc::clone(&client) as Arc<dyn HttpClient>, env.clone())
            .with_config(Some(config))
            .fetch(now);

        assert_eq!(after.status, FetchStatus::Ok, "{:?}", after.error);
        assert!(client.request_count() >= 1);
        // The token was attached to the probe — and the fixture recorded it in
        // the Authorization header.
        let captured = client.captured();
        assert!(captured[0].sends_authorization_with("token "));
        assert!(
            !after.windows.is_empty(),
            "the fixture maps to real windows"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
