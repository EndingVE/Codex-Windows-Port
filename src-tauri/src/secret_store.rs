//! CodexBar's **own** secrets: where they live, how they leave `config.json`,
//! and the single writer of that file (RECON D1-D4).
//!
//! * **Storage.** API keys, cookie headers and the Copilot device-flow token go
//!   to Windows Credential Manager, with a DPAPI-encrypted file under
//!   `%LOCALAPPDATA%\CodexBar` as the fallback
//!   ([`codexbar_providers::credential::default_vault`]). `config.json` keeps
//!   only a `{"$vault": "<key>", "hint": "1234"}` reference, which
//!   [`codexbar_providers::PortConfig`] resolves when a provider needs it.
//!   Credentials that belong to other CLIs are never copied here.
//! * **Migration.** [`migrate_config_file`] moves every plain-text secret of
//!   `config.json` into the vault, scrubs `config.json.bak` the same way and
//!   removes stale `config.json.tmp*` files. It is idempotent: a second run
//!   finds nothing to move and writes nothing.
//! * **Single writer.** Every write of `config.json` (settings saves from the
//!   window or the tray, the device-flow token store, the migration) holds
//!   [`config_lock`] for its whole read-modify-write, so the tray/login race of
//!   D3 cannot drop a token.
//! * **Webview.** [`redact_entry`] replaces every secret with
//!   `{"hasKey": true, "masked": "••••1234"}`; [`merge_entry`] treats an
//!   empty, missing or masked secret coming back from the webview as
//!   "unchanged".
//!
//! Backends are injectable: tests hand every function a
//! [`MemoryBackend`](codexbar_providers::credential::MemoryBackend), and under
//! `cfg(test)` [`vault_for`] never returns the real Credential Manager.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use codexbar_providers::credential::{vault_ref, vault_ref_key, SecretBackend, VAULT_HINT_KEY};
use codexbar_providers::Secret;
use serde_json::{Map, Value};

/// Provider-entry fields that hold a secret of CodexBar's own.
pub const SECRET_FIELDS: &[&str] = &["apiKey", "cookieHeader", "token"];
/// The token-account list the device flow writes (see `token_store.rs`).
pub const TOKEN_ACCOUNTS_FIELD: &str = "tokenAccounts";
/// What the webview receives instead of a secret.
pub const MASK_PREFIX: &str = "••••";

static CONFIG_LOCK: Mutex<()> = Mutex::new(());

/// The process-wide lock every `config.json` writer holds for its whole
/// read-modify-write. Not re-entrant: take it once, at the outer call.
pub fn config_lock() -> MutexGuard<'static, ()> {
    CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// The vault that backs the config at `path`.
///
/// Production: Credential Manager with the DPAPI fallback — the same store
/// `PortConfig::load` resolves references through.
#[cfg(not(test))]
pub fn vault_for(_path: &Path) -> Arc<dyn SecretBackend> {
    codexbar_providers::credential::default_vault()
}

/// Tests: one in-memory vault per config path, so no test can ever read or
/// write the user's real Credential Manager entries.
#[cfg(test)]
pub fn vault_for(path: &Path) -> Arc<dyn SecretBackend> {
    use codexbar_providers::credential::MemoryBackend;
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static VAULTS: OnceLock<Mutex<HashMap<PathBuf, Arc<MemoryBackend>>>> = OnceLock::new();
    let mut vaults = VAULTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let vault = vaults.entry(path.to_path_buf()).or_default();
    Arc::clone(vault) as Arc<dyn SecretBackend>
}

/// `true` for a field name that holds a secret.
pub fn is_secret_field(name: &str) -> bool {
    SECRET_FIELDS.contains(&name)
}

fn safe_id(provider: &str) -> Option<&str> {
    let ok = !provider.is_empty()
        && provider.len() <= 64
        && provider
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then_some(provider)
}

/// Vault key for a plain secret field: `openrouter.apiKey`.
pub fn field_key(provider: &str, field: &str) -> String {
    format!("{provider}.{field}")
}

/// Vault key for token account `index`: `copilot.tokenAccounts.0`.
pub fn account_key(provider: &str, index: usize) -> String {
    format!("{provider}.{TOKEN_ACCOUNTS_FIELD}.{index}")
}

/// A non-empty plain-text secret (not a reference, not a mask).
fn plain_secret(value: &Value) -> Option<&str> {
    let text = value.as_str()?.trim();
    if text.is_empty() || text.starts_with(MASK_PREFIX) {
        return None;
    }
    Some(text)
}

/// Store `secret` under `key` and read it back before the plain copy is
/// allowed to disappear from disk.
pub fn put(vault: &dyn SecretBackend, key: &str, secret: &Secret) -> Result<Value, String> {
    vault
        .set(key, secret)
        .map_err(|e| format!("could not store {key} in {}: {e}", vault.name()))?;
    match vault.get(key) {
        Ok(Some(back)) if back.expose() == secret.expose() => Ok(vault_ref(key, secret)),
        _ => Err(format!(
            "{} did not return {key} after storing it",
            vault.name()
        )),
    }
}

// ---------------------------------------------------------------------------
// Webview redaction (D1)
// ---------------------------------------------------------------------------

/// What the webview sees for a stored secret: presence and a masked hint.
pub fn masked(value: &Value) -> Value {
    // Already a placeholder: redacting twice must not lose `hasKey`.
    if value.get("hasKey").is_some() && vault_ref_key(value).is_none() {
        let mut out = Map::new();
        out.insert(
            "hasKey".into(),
            Value::Bool(value["hasKey"].as_bool().unwrap_or(false)),
        );
        if let Some(mask) = value
            .get("masked")
            .and_then(Value::as_str)
            .filter(|m| m.starts_with(MASK_PREFIX))
        {
            out.insert("masked".into(), Value::String(mask.to_string()));
        }
        return Value::Object(out);
    }
    // Already a placeholder (redacting twice must not lose `hasKey`).
    if value.get("hasKey").is_some() && vault_ref_key(value).is_none() {
        let mut out = Map::new();
        out.insert(
            "hasKey".into(),
            Value::Bool(value["hasKey"].as_bool().unwrap_or(false)),
        );
        if let Some(mask) = value
            .get("masked")
            .and_then(Value::as_str)
            .filter(|m| m.starts_with(MASK_PREFIX))
        {
            out.insert("masked".into(), Value::String(mask.to_string()));
        }
        return Value::Object(out);
    }
    let hint = if vault_ref_key(value).is_some() {
        value
            .get(VAULT_HINT_KEY)
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        plain_secret(value).and_then(|s| {
            let secret = Secret::new(s);
            (secret.expose().chars().count() > 8).then(|| secret.suffix(4))
        })
    };
    let has_key = vault_ref_key(value).is_some() || plain_secret(value).is_some();
    let mut out = Map::new();
    out.insert("hasKey".into(), Value::Bool(has_key));
    if has_key {
        out.insert(
            "masked".into(),
            Value::String(format!("{MASK_PREFIX}{}", hint.unwrap_or_default())),
        );
    }
    Value::Object(out)
}

/// Replace every secret of one provider entry with [`masked`].
pub fn redact_entry(extra: &mut Map<String, Value>) {
    for (name, value) in extra.iter_mut() {
        if is_secret_field(name) {
            *value = masked(value);
        }
    }
    if let Some(accounts) = extra
        .get_mut(TOKEN_ACCOUNTS_FIELD)
        .and_then(|t| t.get_mut("accounts"))
        .and_then(Value::as_array_mut)
    {
        for account in accounts.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(token) = account.get_mut("token") {
                *token = masked(token);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Merge on save (D2, D3)
// ---------------------------------------------------------------------------

/// Merge one provider entry coming from the webview/tray (`incoming`) over the
/// one on disk.
///
/// * Non-secret fields: `incoming` wins; fields it does not mention are kept.
/// * Secret fields: an empty, missing, masked or `{hasKey…}` value means
///   *unchanged*; a new plain value goes to the vault and is replaced by a
///   reference; an explicit `null` clears the key (and its vault entry).
/// * `tokenAccounts` is owned by the device-flow writer: it is kept from disk
///   unless `incoming` carries new plain tokens (moved to the vault).
pub fn merge_entry(
    provider: &str,
    disk: &Map<String, Value>,
    incoming: &Map<String, Value>,
    vault: &dyn SecretBackend,
) -> Result<Map<String, Value>, String> {
    let mut out = disk.clone();
    for (name, value) in incoming {
        if is_secret_field(name) {
            if value.is_null() {
                if let Some(id) = safe_id(provider) {
                    let _ = vault.delete(&field_key(id, name));
                }
                out.remove(name);
            } else if let Some(text) = plain_secret(value) {
                let Some(id) = safe_id(provider) else {
                    continue;
                };
                out.insert(
                    name.clone(),
                    put(vault, &field_key(id, name), &Secret::new(text))?,
                );
            } else if vault_ref_key(value).is_some() && !out.contains_key(name) {
                // A reference the disk lost (e.g. an older in-memory copy).
                out.insert(name.clone(), value.clone());
            }
        } else if name == TOKEN_ACCOUNTS_FIELD {
            let mut candidate = value.clone();
            let moved = migrate_accounts(provider, &mut candidate, vault, true)?;
            let disk_has = out.contains_key(TOKEN_ACCOUNTS_FIELD);
            if moved > 0 || (!disk_has && has_account_refs(&candidate)) {
                out.insert(name.clone(), candidate);
            }
        } else {
            out.insert(name.clone(), value.clone());
        }
    }
    Ok(out)
}

fn has_account_refs(accounts: &Value) -> bool {
    accounts
        .get("accounts")
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.iter()
                .any(|a| a.get("token").and_then(vault_ref_key).is_some())
        })
}

/// Merge a whole serialised `Settings` over the raw JSON on disk.
///
/// Top-level keys and provider entries the incoming value does not know about
/// are kept from disk.
pub fn merge_root(
    disk: &Value,
    incoming: &Value,
    vault: &dyn SecretBackend,
) -> Result<Value, String> {
    let mut out = disk.as_object().cloned().unwrap_or_default();
    let empty = Vec::new();
    let disk_providers = disk
        .get("providers")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let incoming = incoming
        .as_object()
        .ok_or_else(|| "settings did not serialise to an object".to_string())?;

    for (name, value) in incoming {
        if name != "providers" {
            out.insert(name.clone(), value.clone());
        }
    }

    let mut providers = Vec::new();
    let mut seen = Vec::new();
    for entry in incoming
        .get("providers")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
    {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
        let on_disk = disk_providers
            .iter()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(id))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        providers.push(Value::Object(merge_entry(id, &on_disk, entry, vault)?));
        seen.push(id.to_string());
    }
    // Entries the running build does not know (a newer config) survive.
    for entry in disk_providers {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
        if !seen.iter().any(|s| s == id) {
            providers.push(entry.clone());
        }
    }
    out.insert("providers".into(), Value::Array(providers));
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// Migration (D4)
// ---------------------------------------------------------------------------

/// Move the plain tokens of one `tokenAccounts` object into the vault.
/// `overwrite == false` keeps a value the vault already holds (used for the
/// `.bak`, whose secrets are older than the live file's).
fn migrate_accounts(
    provider: &str,
    accounts: &mut Value,
    vault: &dyn SecretBackend,
    overwrite: bool,
) -> Result<usize, String> {
    let Some(list) = accounts.get_mut("accounts").and_then(Value::as_array_mut) else {
        return Ok(0);
    };
    let mut moved = 0;
    for (index, account) in list.iter_mut().enumerate() {
        let Some(account) = account.as_object_mut() else {
            continue;
        };
        let Some(token) = account.get("token") else {
            continue;
        };
        let Some(text) = plain_secret(token) else {
            continue;
        };
        let Some(id) = safe_id(provider) else {
            // Never leave it in plain text, even under an odd id.
            account.remove("token");
            moved += 1;
            continue;
        };
        let key = account_key(id, index);
        let secret = Secret::new(text);
        let reference = if !overwrite && matches!(vault.get(&key), Ok(Some(_))) {
            vault_ref(&key, &secret)
        } else {
            put(vault, &key, &secret)?
        };
        account.insert("token".into(), reference);
        moved += 1;
    }
    Ok(moved)
}

/// Replace every plain-text secret in a config root with a vault reference.
/// Returns how many values moved. Running it twice moves nothing the second
/// time.
pub fn migrate_value(
    root: &mut Value,
    vault: &dyn SecretBackend,
    overwrite: bool,
) -> Result<usize, String> {
    let Some(providers) = root.get_mut("providers").and_then(Value::as_array_mut) else {
        return Ok(0);
    };
    let mut moved = 0;
    for entry in providers.iter_mut() {
        let Some(entry) = entry.as_object_mut() else {
            continue;
        };
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        for field in SECRET_FIELDS {
            let Some(text) = entry.get(*field).and_then(plain_secret) else {
                continue;
            };
            let secret = Secret::new(text);
            let Some(safe) = safe_id(&id) else {
                entry.remove(*field);
                moved += 1;
                continue;
            };
            let key = field_key(safe, field);
            let reference = if !overwrite && matches!(vault.get(&key), Ok(Some(_))) {
                vault_ref(&key, &secret)
            } else {
                put(vault, &key, &secret)?
            };
            entry.insert((*field).to_string(), reference);
            moved += 1;
        }
        if let Some(accounts) = entry.get_mut(TOKEN_ACCOUNTS_FIELD) {
            moved += migrate_accounts(&id, accounts, vault, overwrite)?;
        }
    }
    Ok(moved)
}

/// Atomic write with no `.bak`: a unique temp file next to `path`, then a
/// rename over it.
pub fn write_atomic(path: &Path, value: &Value) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| format!("could not serialise {}: {e}", path.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}-{nanos}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not replace {}: {e}", path.display())
    })
}

/// `<path>.bak`
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

/// What a migration did. Counts only — never a value.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub moved: usize,
    pub backup_moved: usize,
    pub backup_removed: bool,
    pub temp_files_removed: usize,
}

impl MigrationReport {
    pub fn changed(&self) -> bool {
        self.moved + self.backup_moved + self.temp_files_removed > 0 || self.backup_removed
    }
}

/// Migrate `config.json`, its `.bak` and stray temp files. Takes
/// [`config_lock`]; see [`migrate_config_file_locked`] for callers that
/// already hold it.
pub fn migrate_config_file(
    path: &Path,
    vault: &dyn SecretBackend,
) -> Result<MigrationReport, String> {
    let _guard = config_lock();
    migrate_config_file_locked(path, vault)
}

/// [`migrate_config_file`] for a caller that already holds [`config_lock`].
///
/// A `config.json` that does not parse is left alone (and the error returned):
/// the migration never replaces a file it does not understand. A `.bak` that
/// does not parse is deleted, since it could hold plain secrets and is only a
/// convenience copy.
pub fn migrate_config_file_locked(
    path: &Path,
    vault: &dyn SecretBackend,
) -> Result<MigrationReport, String> {
    let mut report = MigrationReport::default();

    match std::fs::read(path) {
        Ok(bytes) => {
            let mut root: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
            report.moved = migrate_value(&mut root, vault, true)?;
            if report.moved > 0 {
                write_atomic(path, &root)?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    }

    let bak = backup_path(path);
    if let Ok(bytes) = std::fs::read(&bak) {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(mut root) => {
                report.backup_moved = migrate_value(&mut root, vault, false)?;
                if report.backup_moved > 0 {
                    write_atomic(&bak, &root)?;
                }
            }
            Err(_) => {
                std::fs::remove_file(&bak)
                    .map_err(|e| format!("could not remove {}: {e}", bak.display()))?;
                report.backup_removed = true;
            }
        }
    }

    // Leftover `config.json.tmp*` from an interrupted write of an older build.
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
        let prefix = format!("{}.tmp", name.to_string_lossy());
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix)
                    && std::fs::remove_file(entry.path()).is_ok()
                {
                    report.temp_files_removed += 1;
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codexbar_core::ProviderId;
    use codexbar_providers::credential::MemoryBackend;
    use codexbar_providers::{Env, PortConfig};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codexbar-secrets-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    const PLAIN: &str = r#"{"providers":[
        {"id":"openrouter","enabled":true,"apiKey":"sk-or-plain-fixture-1234","region":"eu"},
        {"id":"copilot","enabled":true,"tokenAccounts":{"activeIndex":0,
          "accounts":[{"token":"gho_plain_fixture_token","label":"lbl"}]}},
        {"id":"cursor","enabled":false,"cookieHeader":"session=cookie-fixture-value"}
      ],"refreshIntervalSecs":300}"#;

    fn assert_no_plain(text: &str) {
        for needle in ["sk-or-plain", "gho_plain", "cookie-fixture"] {
            assert!(!text.contains(needle), "{needle} left in plain text");
        }
    }

    /// (c) The migration empties config.json and .bak of plain text, stores the
    /// values in the vault, and a second run changes nothing.
    #[test]
    fn migration_is_idempotent_and_leaves_no_plain_text() {
        let path = scratch("migrate");
        std::fs::write(&path, PLAIN).unwrap();
        std::fs::write(backup_path(&path), PLAIN).unwrap();
        let stray = path.with_file_name("config.json.tmp4242");
        std::fs::write(&stray, PLAIN).unwrap();
        let vault = MemoryBackend::new();

        let first = migrate_config_file(&path, &vault).unwrap();
        assert_eq!(first.moved, 3);
        assert_eq!(first.backup_moved, 3);
        assert_eq!(first.temp_files_removed, 1);
        assert!(!stray.exists());

        let live = std::fs::read_to_string(&path).unwrap();
        let bak = std::fs::read_to_string(backup_path(&path)).unwrap();
        assert_no_plain(&live);
        assert_no_plain(&bak);
        assert!(live.contains("\"$vault\""));
        // Non-secret fields are untouched.
        let root: Value = serde_json::from_str(&live).unwrap();
        assert_eq!(root["refreshIntervalSecs"], 300);
        assert_eq!(root["providers"][0]["region"], "eu");
        assert_eq!(root["providers"][0]["apiKey"]["hint"], "1234");

        let mut keys = vault.keys();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "copilot.tokenAccounts.0",
                "cursor.cookieHeader",
                "openrouter.apiKey"
            ]
        );

        // Idempotent: nothing moves, nothing is rewritten.
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let second = migrate_config_file(&path, &vault).unwrap();
        assert!(!second.changed(), "{second:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), live);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );

        // The provider crate still resolves every secret through the vault.
        let vault = Arc::new(vault);
        let env = Env::empty().with("CODEXBAR_CONFIG", path.to_string_lossy().to_string());
        let config = PortConfig::load(&env).unwrap().unwrap().with_vault(vault);
        assert_eq!(
            config.api_key(ProviderId::OpenRouter).unwrap().expose(),
            "sk-or-plain-fixture-1234"
        );
        assert_eq!(
            config
                .active_token_account(ProviderId::Copilot)
                .unwrap()
                .expose(),
            "gho_plain_fixture_token"
        );
        assert_eq!(
            config.field(ProviderId::Cursor, "cookieHeader").as_deref(),
            Some("session=cookie-fixture-value")
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// The `.bak` never overwrites a newer secret already in the vault.
    #[test]
    fn the_backup_does_not_override_the_live_secret() {
        let path = scratch("bak-order");
        std::fs::write(&path, PLAIN).unwrap();
        std::fs::write(
            backup_path(&path),
            r#"{"providers":[{"id":"openrouter","apiKey":"sk-or-OLD-backup-value"}]}"#,
        )
        .unwrap();
        let vault = MemoryBackend::new();
        migrate_config_file(&path, &vault).unwrap();
        assert_eq!(
            vault.get("openrouter.apiKey").unwrap().unwrap().expose(),
            "sk-or-plain-fixture-1234"
        );
        let bak = std::fs::read_to_string(backup_path(&path)).unwrap();
        assert!(!bak.contains("OLD-backup"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// An unparseable `.bak` may hold plain secrets: it is removed. An
    /// unparseable `config.json` is left exactly as it was.
    #[test]
    fn unparseable_files_are_never_rewritten_but_a_bad_backup_is_removed() {
        let path = scratch("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        std::fs::write(backup_path(&path), "{ \"apiKey\": \"sk-half").unwrap();
        let vault = MemoryBackend::new();
        assert!(migrate_config_file(&path, &vault).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");

        std::fs::write(&path, "{}").unwrap();
        let report = migrate_config_file(&path, &vault).unwrap();
        assert!(report.backup_removed);
        assert!(!backup_path(&path).exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A vault that cannot store the value leaves the plain file untouched
    /// rather than losing the secret.
    #[test]
    fn a_failing_vault_never_loses_the_secret() {
        struct Down;
        impl SecretBackend for Down {
            fn name(&self) -> &'static str {
                "down"
            }
            fn get(&self, _: &str) -> Result<Option<Secret>, String> {
                Err("down".into())
            }
            fn set(&self, _: &str, _: &Secret) -> Result<(), String> {
                Err("down".into())
            }
            fn delete(&self, _: &str) -> Result<(), String> {
                Err("down".into())
            }
        }
        let path = scratch("down");
        std::fs::write(&path, PLAIN).unwrap();
        let err = migrate_config_file(&path, &Down).unwrap_err();
        assert!(!err.contains("sk-or-plain"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PLAIN);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn redaction_keeps_presence_and_hint_only() {
        let mut extra: Map<String, Value> = serde_json::from_str(
            r#"{"apiKey":{"$vault":"openrouter.apiKey","hint":"1234"},
                "cookieHeader":"session=cookie-fixture-value",
                "region":"eu",
                "tokenAccounts":{"activeIndex":0,"accounts":[{"token":"gho-keep-plain","label":"l"}]}}"#,
        )
        .unwrap();
        redact_entry(&mut extra);
        let text = serde_json::to_string(&extra).unwrap();
        assert!(!text.contains("$vault"));
        assert!(!text.contains("cookie-fixture"));
        assert!(!text.contains("gho-keep"));
        assert_eq!(extra["apiKey"]["hasKey"], true);
        assert_eq!(extra["apiKey"]["masked"], "••••1234");
        assert_eq!(extra["region"], "eu");
        assert_eq!(
            extra["tokenAccounts"]["accounts"][0]["token"]["hasKey"],
            true
        );
    }

    #[test]
    fn redacting_twice_keeps_the_placeholder() {
        let once = masked(&serde_json::json!({"$vault": "a.apiKey", "hint": "9f2c"}));
        assert_eq!(masked(&once), once);
        assert_eq!(once["masked"], "••••9f2c");
    }

    #[test]
    fn merge_treats_empty_masked_and_missing_secrets_as_unchanged() {
        let vault = MemoryBackend::new();
        let disk: Map<String, Value> = serde_json::from_str(
            r#"{"id":"openrouter","apiKey":{"$vault":"openrouter.apiKey"},"region":"eu",
                "tokenAccounts":{"activeIndex":0,"accounts":[{"token":{"$vault":"openrouter.tokenAccounts.0"}}]}}"#,
        )
        .unwrap();
        for incoming in [
            r#"{"id":"openrouter","enabled":true}"#,
            r#"{"id":"openrouter","apiKey":""}"#,
            r#"{"id":"openrouter","apiKey":"••••1234"}"#,
            r#"{"id":"openrouter","apiKey":{"hasKey":true,"masked":"••••1234"},
                "tokenAccounts":{"activeIndex":0,"accounts":[{"token":{"hasKey":true}}]}}"#,
        ] {
            let incoming: Map<String, Value> = serde_json::from_str(incoming).unwrap();
            let merged = merge_entry("openrouter", &disk, &incoming, &vault).unwrap();
            assert_eq!(merged["apiKey"], disk["apiKey"], "{incoming:?}");
            assert_eq!(merged["tokenAccounts"], disk["tokenAccounts"]);
            assert_eq!(merged["region"], "eu");
        }

        // A new plain value goes to the vault, never to the map.
        let incoming: Map<String, Value> = serde_json::from_str(
            r#"{"id":"openrouter","apiKey":"sk-or-new-value-5678","region":"us"}"#,
        )
        .unwrap();
        let merged = merge_entry("openrouter", &disk, &incoming, &vault).unwrap();
        assert!(!serde_json::to_string(&merged)
            .unwrap()
            .contains("sk-or-new"));
        assert_eq!(merged["apiKey"]["$vault"], "openrouter.apiKey");
        assert_eq!(merged["region"], "us");
        assert_eq!(
            vault.get("openrouter.apiKey").unwrap().unwrap().expose(),
            "sk-or-new-value-5678"
        );

        // An explicit null clears it.
        let incoming: Map<String, Value> =
            serde_json::from_str(r#"{"id":"openrouter","apiKey":null}"#).unwrap();
        let merged = merge_entry("openrouter", &disk, &incoming, &vault).unwrap();
        assert!(!merged.contains_key("apiKey"));
        assert!(vault.get("openrouter.apiKey").unwrap().is_none());
    }
}
